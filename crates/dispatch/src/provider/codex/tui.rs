//! `provider::codex::tui` — identity discovery for a codex session running as a
//! TUI in a mux pane (`qd start <name> --provider codex --interactive`).
//!
//! THE PROBLEM THIS SOLVES. Every other create path in the engine is handed its
//! session identity: claude WRITES its own registry row (the create path just
//! waits for it, `boot::EventBootWaiter`), and the codex/acp/pi daemon lanes ASK
//! for it over RPC (`thread/start` returns the thread id). The codex TUI does
//! neither — it knows nothing about qd, speaks no protocol to it, and its pane is
//! just a process.
//!
//! WHEN IDENTITY ACTUALLY EXISTS (measured, codex-cli 0.146.1, 2026-08-06). A
//! codex TUI does NOT open its rollout at launch. It opens it at the FIRST
//! INTERACTION, and not a moment sooner:
//!
//! | session start (from the filename) | rollout file created | gap |
//! |---|---|---|
//! | 16:52:44 | 16:52:47 |  3s |
//! | 17:10:16 | 17:10:26 | 10s |
//! | 17:09:59 | 17:10:47 | 48s |
//!
//! Those gaps are how long it took a human to type. A session launched and left
//! sitting at its composer was observed running **164 seconds with no rollout on
//! disk at all**. The `state_5.sqlite` `threads` index is no better — it lagged by
//! days and carries a `has_user_event` column. So there is NO identity source at
//! codex TUI startup, and waiting for one would mean `qd start` blocking until a
//! human typed.
//!
//! THE CONSEQUENCE, and the shape of this module. Session EXISTENCE and thread
//! IDENTITY are separate events for this lane, so the create path does not wait:
//! it writes a row the moment the pane is attachable, with no `sessionId`, and the
//! id is BOUND LATER by [`pick_thread`] from the gather step (join.rs) once the
//! rollout appears. `qd attach` needs only the pane and works immediately; the
//! verbs that genuinely need a thread id refuse honestly until it binds.
//!
//! THE MISATTRIBUTION HAZARD, and why binding is unique-or-nothing. Deferred
//! binding widens the window in which someone else's codex — quite plausibly one
//! the user has open in the SAME repo — could be mistaken for ours, and adopting a
//! stranger's thread would point this session's transcript, status and turn
//! history at another conversation, and `qd stop` at the wrong one. So a candidate
//! must clear cwd, ownership AND its own recorded start time, and if two survive,
//! [`pick_thread`] binds NEITHER. An unidentified session is a small, honest
//! inconvenience; a misidentified one is silent corruption.
//!
//! L8/L9a: every read is permissive (a corrupt/unreadable rollout is skipped,
//! never fatal) and the sessions root is INJECTED — nothing here resolves a home.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One rollout observed on disk, reduced to the three facts attribution needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutCandidate {
    /// The thread id — read from the `session_meta` payload, NOT parsed out of
    /// the filename. Both carry it, but the payload is the value the app-server
    /// and the sqlite index also key on, so it is the one that must match.
    pub id: String,
    /// The `session_meta` payload's `cwd`. `None` when the line was absent or
    /// unreadable — which [`pick_thread`] treats as DISQUALIFYING, never as a
    /// wildcard (see its doc).
    pub cwd: Option<String>,
    /// When the THREAD started, in epoch ms — parsed from the `session_meta`
    /// payload's own ISO-8601 timestamp.
    ///
    /// This, not the file's mtime, is the age test. A rollout's mtime advances
    /// with every line the conversation writes, so a codex the user has had open
    /// in this repo since this morning presents a *recent* mtime forever and would
    /// pass any mtime-based floor. Its recorded start time would not. `None` when
    /// unparseable → disqualifying, same as an unreadable cwd.
    pub started_at_ms: Option<i64>,
}

/// PURE: which thread belongs to a session we started, if we can tell UNAMBIGUOUSLY?
///
/// A candidate must clear ALL of:
///   - **`started_at_ms >= since_ms`** — the thread started at or after we
///     launched the pane. `since_ms` is the clock sample the create path took
///     immediately BEFORE spawning (persisted as the row's `startedAt`), so a
///     thread that predates the session cannot qualify.
///   - **`cwd == Some(our cwd)`** — codex records the cwd it was launched in, and
///     the pane runs in ours. A candidate whose cwd we could not read is REJECTED
///     rather than accepted: an unreadable line is nearly always a rollout
///     mid-write, and "unknown" must never be treated as "matches" when a wrong
///     match means adopting a stranger's conversation. Rejecting costs one poll.
///   - **`id` not in `owned_ids`** — a thread another registry row already claims
///     is by definition not this session's.
///
/// And then the rule that matters most: **exactly one survivor, or nothing.**
/// Two plausible threads means we cannot tell which is ours, and there is no
/// tie-break worth having — "newest" would be a guess, and the cost of guessing
/// wrong is silent and permanent. Returning `None` leaves the session
/// unidentified, which is visible, honest, and self-correcting on the next scan.
pub fn pick_thread(
    candidates: &[RolloutCandidate],
    cwd: &str,
    since_ms: i64,
    owned_ids: &HashSet<String>,
) -> Option<String> {
    let mut hits = candidates
        .iter()
        .filter(|c| c.started_at_ms.is_some_and(|t| t >= since_ms))
        .filter(|c| c.cwd.as_deref() == Some(cwd))
        .filter(|c| !c.id.is_empty() && !owned_ids.contains(&c.id));
    let first = hits.next()?;
    // A second survivor ⇒ ambiguous ⇒ bind nothing.
    if hits.next().is_some() {
        return None;
    }
    Some(first.id.clone())
}

/// Gather rollout candidates under `sessions_root` (codex's `sessions/` dir),
/// cheaply: walk `YYYY/MM/DD`, take each file's mtime from its dirent metadata,
/// and READ only those at/after `mtime_floor_ms`.
///
/// The mtime floor is purely a read-reducer — it decides which files are worth
/// opening, never which thread is ours (that is [`pick_thread`]'s recorded-start
/// test). Pass the session's `startedAt`: our own rollout is created after that,
/// while an older conversation still being written stays cheap to skip only if it
/// is genuinely idle. Correctness does not depend on it.
///
/// Permissive at every level (L8): a missing root, an unwalkable date dir, a
/// non-rollout filename, an unreadable file, or a first line that is not a
/// `session_meta` all contribute nothing and never error.
pub fn scan_candidates(sessions_root: &Path, mtime_floor_ms: i64) -> Vec<RolloutCandidate> {
    let mut out = Vec::new();
    let Ok(years) = std::fs::read_dir(sessions_root) else {
        return out;
    };
    for y in years.flatten() {
        let Ok(months) = std::fs::read_dir(y.path()) else {
            continue;
        };
        for mo in months.flatten() {
            let Ok(days) = std::fs::read_dir(mo.path()) else {
                continue;
            };
            for d in days.flatten() {
                let Ok(files) = std::fs::read_dir(d.path()) else {
                    continue;
                };
                for f in files.flatten() {
                    let fname = f.file_name().to_string_lossy().into_owned();
                    // Cheap gates first: the rollout filename shape, then mtime.
                    // Only what survives both is opened.
                    if super::rollout::parse_filename(&fname).is_none() {
                        continue;
                    }
                    let mtime_ms = f
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|dd| dd.as_millis() as i64)
                        .unwrap_or(0);
                    if mtime_ms < mtime_floor_ms {
                        continue;
                    }
                    if let Some(c) = read_session_meta(&f.path()) {
                        out.push(c);
                    }
                }
            }
        }
    }
    out
}

/// Read a rollout's `session_meta` (its FIRST line) into a candidate.
///
/// Only the first line is parsed: `session_meta` is the rollout's opening record,
/// and a rollout mid-turn can be megabytes. Note the line itself is large — codex
/// embeds its full base instructions in the payload, so this is tens of KB, not a
/// few hundred bytes. Returns `None` when the file is unreadable, empty, or its
/// first line is not a `session_meta` carrying an id — all of which are ordinary
/// "codex has not finished opening it" states.
fn read_session_meta(path: &Path) -> Option<RolloutCandidate> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    // `read_line` on a rollout being written can return a partial line; a partial
    // JSON object fails to parse and we simply try again on the next scan.
    BufReader::new(f).read_line(&mut first).ok()?;
    let trimmed = first.trim();
    let rec = super::rollout::parse_line(trimmed)?;
    let super::rollout::RolloutLine::SessionMeta { id, cwd } = rec.line else {
        return None;
    };
    // The thread's own start time lives in the PAYLOAD's timestamp, not the
    // record's top-level one: the record is stamped when it is finally written
    // (at first interaction), while the payload carries when the session began.
    // Confusing them would compare the wrong instant against the floor.
    let started_at_ms = serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|v| {
            v.get("payload")
                .and_then(|p| p.get("timestamp"))
                .and_then(|t| t.as_str())
                .and_then(parse_iso8601_ms)
        });
    Some(RolloutCandidate {
        id: id?,
        cwd,
        started_at_ms,
    })
}

/// Parse codex's `session_meta` timestamp (`2026-08-06T21:09:59.634Z`) to epoch ms.
///
/// Delegates to [`crate::events::iso_to_epoch_ms`], which already parses exactly
/// this shape for the engine's own timestamp comparisons — the crate has one
/// ISO-8601 reader, not one per provider. Best-effort by contract: `None` on
/// malformed input, which DISQUALIFIES the candidate in [`pick_thread`] rather
/// than admitting it with a wrong instant (the safe direction).
pub fn parse_iso8601_ms(s: &str) -> Option<i64> {
    crate::events::iso_to_epoch_ms(s)
}

/// The BACKFILL entry point the gather step calls: scan `sessions_root` and
/// return this session's thread id, if it can be identified unambiguously.
///
/// `since_ms` is the session row's `startedAt` (the pre-launch clock sample);
/// `cwd` the dir the pane runs in; `owned_ids` every thread id another row
/// already claims. `None` means "not yet" — the caller leaves the row
/// unidentified and tries again on the next scan.
pub fn backfill_thread_id(
    sessions_root: &Path,
    cwd: &str,
    since_ms: i64,
    owned_ids: &HashSet<String>,
) -> Option<String> {
    // NORMALIZE BOTH SIDES OF THE CWD COMPARISON before handing them to the pure
    // decider. The two sides record the same directory from different vantage
    // points and can spell it differently: the registry row carries whatever the
    // create path was given (`--cwd /tmp/foo` is stored verbatim), while codex
    // records what its own process resolves (`/private/tmp/foo` on macOS, where
    // /tmp is a symlink). An exact string compare then never matches and the
    // session stays unidentified forever — silently, since "not yet" and "never"
    // look identical from outside. Found by the end-to-end validation, which
    // tripped over exactly this /tmp vs /private/tmp split.
    let cwd = normalize_dir(cwd);
    let candidates: Vec<RolloutCandidate> = scan_candidates(sessions_root, since_ms)
        .into_iter()
        .map(|c| RolloutCandidate {
            cwd: c.cwd.as_deref().map(normalize_dir),
            ..c
        })
        .collect();
    pick_thread(&candidates, &cwd, since_ms, owned_ids)
}

/// Resolve a directory to its canonical form for comparison, falling back to the
/// input unchanged when it cannot be resolved (a dir that has since been removed,
/// or a permission failure). The fallback is why this is safe to apply to both
/// sides: two unresolvable paths still compare as the plain strings they were.
fn normalize_dir(dir: &str) -> String {
    std::fs::canonicalize(dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| dir.to_string())
}

/// The rollout root for a codex home — `<codex_home>/sessions`. Exposed so the
/// gather step and the create path name the same dir without duplicating the
/// join.
pub fn sessions_root(codex_home: &Path) -> PathBuf {
    codex_home.join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn cand(id: &str, cwd: Option<&str>, started: Option<i64>) -> RolloutCandidate {
        RolloutCandidate {
            id: id.to_string(),
            cwd: cwd.map(str::to_string),
            started_at_ms: started,
        }
    }

    fn owned(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    // --- pick_thread: the attribution matrix ---------------------------------
    //
    // MUTATION EVIDENCE: dropping any one filter reds a test here. Drop the
    // start-time filter → `ignores_threads_that_predate_the_session`. Drop the cwd
    // filter → `ignores_other_cwds`. Drop the owned filter →
    // `ignores_threads_another_row_already_owns`. Replace unique-or-nothing with a
    // newest-wins tie-break → `refuses_to_choose_between_two_plausible_threads`.

    #[test]
    fn binds_the_single_qualifying_thread() {
        let c = [cand("019f-a", Some("/work"), Some(2_000))];
        assert_eq!(
            pick_thread(&c, "/work", 1_000, &owned(&[])),
            Some("019f-a".to_string())
        );
    }

    #[test]
    fn ignores_threads_that_predate_the_session() {
        // The codex the user has had open in this repo since this morning. Its
        // rollout is being written constantly, so any mtime test would admit it —
        // its RECORDED START is what rules it out.
        let c = [cand("019f-old", Some("/work"), Some(900))];
        assert_eq!(pick_thread(&c, "/work", 1_000, &owned(&[])), None);
    }

    #[test]
    fn start_exactly_at_the_floor_qualifies() {
        let c = [cand("019f-a", Some("/work"), Some(1_000))];
        assert_eq!(
            pick_thread(&c, "/work", 1_000, &owned(&[])),
            Some("019f-a".to_string())
        );
    }

    #[test]
    fn ignores_other_cwds() {
        let c = [cand("019f-b", Some("/elsewhere"), Some(2_000))];
        assert_eq!(pick_thread(&c, "/work", 1_000, &owned(&[])), None);
    }

    #[test]
    fn unknown_cwd_or_unknown_start_is_rejected_not_a_wildcard() {
        // A rollout mid-write: accepting it would risk adopting a stranger's
        // thread; rejecting costs one more scan.
        assert_eq!(
            pick_thread(&[cand("019f-c", None, Some(2_000))], "/work", 1_000, &owned(&[])),
            None
        );
        assert_eq!(
            pick_thread(&[cand("019f-d", Some("/work"), None)], "/work", 1_000, &owned(&[])),
            None
        );
    }

    #[test]
    fn ignores_threads_another_row_already_owns() {
        let c = [cand("019f-taken", Some("/work"), Some(2_000))];
        assert_eq!(
            pick_thread(&c, "/work", 1_000, &owned(&["019f-taken"])),
            None
        );
    }

    #[test]
    fn refuses_to_choose_between_two_plausible_threads() {
        // THE rule that matters: two sessions started in this cwd after we
        // launched, neither owned. There is no honest way to tell which is ours,
        // so bind NEITHER and stay visibly unidentified.
        let c = [
            cand("019f-a", Some("/work"), Some(2_000)),
            cand("019f-b", Some("/work"), Some(5_000)),
        ];
        assert_eq!(pick_thread(&c, "/work", 1_000, &owned(&[])), None);
    }

    #[test]
    fn ambiguity_resolves_once_the_other_thread_is_owned() {
        // ...and it self-corrects: as soon as the competing thread belongs to some
        // row, ours is unambiguous again.
        let c = [
            cand("019f-a", Some("/work"), Some(2_000)),
            cand("019f-b", Some("/work"), Some(5_000)),
        ];
        assert_eq!(
            pick_thread(&c, "/work", 1_000, &owned(&["019f-b"])),
            Some("019f-a".to_string())
        );
    }

    #[test]
    fn empty_id_never_qualifies() {
        let c = [cand("", Some("/work"), Some(2_000))];
        assert_eq!(pick_thread(&c, "/work", 1_000, &owned(&[])), None);
    }

    #[test]
    fn no_candidates_is_none_not_a_panic() {
        assert_eq!(pick_thread(&[], "/work", 1_000, &owned(&[])), None);
    }

    // --- parse_iso8601_ms ----------------------------------------------------

    #[test]
    fn parses_the_real_codex_session_meta_timestamp() {
        // Verbatim from a live rollout (codex-cli 0.146.1).
        let ms = parse_iso8601_ms("2026-08-06T21:09:59.634Z").expect("parses");
        // Round-trips against the same instant with no millis.
        let base = parse_iso8601_ms("2026-08-06T21:09:59Z").expect("parses");
        assert_eq!(ms - base, 634);
    }

    #[test]
    fn epoch_and_known_instants_are_exact() {
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601_ms("1970-01-02T00:00:00Z"), Some(86_400_000));
        // 2000-03-01 — just past a leap day in a 400-year-rule leap year.
        assert_eq!(parse_iso8601_ms("2000-03-01T00:00:00Z"), Some(951_868_800_000));
        // 2026-08-06T21:09:59Z — the live rollout's own start stamp, cross-checked
        // against python `datetime.timestamp()`.
        assert_eq!(
            parse_iso8601_ms("2026-08-06T21:09:59Z"),
            Some(1_786_050_599_000)
        );
    }

    #[test]
    fn ordering_is_preserved_across_a_month_boundary() {
        let a = parse_iso8601_ms("2026-07-31T23:59:59Z").unwrap();
        let b = parse_iso8601_ms("2026-08-01T00:00:00Z").unwrap();
        assert_eq!(b - a, 1_000);
    }

    #[test]
    fn unparseable_timestamps_are_none() {
        // Structurally malformed input yields None, which disqualifies the
        // candidate. (The shared reader is deliberately lenient about RANGES — it
        // is best-effort for age math, not a validator — so this pins the shapes
        // it genuinely cannot read, not out-of-range components.)
        for bad in ["", "not-a-date", "2026-08-06", "2026-08-06 21:09:59Z"] {
            assert_eq!(parse_iso8601_ms(bad), None, "input: {bad:?}");
        }
    }

    // --- scan_candidates: the gathering half ---------------------------------

    fn write_rollout(root: &Path, day: &str, name: &str, body: &str) -> PathBuf {
        let dir = root.join(day);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    /// A session_meta line shaped like the real thing: the record's top-level
    /// timestamp is LATER than the payload's (codex writes it at first
    /// interaction), which is exactly the trap `read_session_meta` must not fall
    /// into.
    fn meta_line(id: &str, cwd: &str, started: &str, written: &str) -> String {
        format!(
            "{{\"timestamp\":\"{written}\",\"type\":\"session_meta\",\"payload\":{{\
             \"session_id\":\"{id}\",\"id\":\"{id}\",\"timestamp\":\"{started}\",\
             \"cwd\":\"{cwd}\",\"originator\":\"codex-tui\"}}}}\n"
        )
    }

    const UUID_A: &str = "019fc8bf-e3fa-7420-8152-66a1411442bb";
    const UUID_B: &str = "019fc8b9-5af4-7df1-8bb8-2fdb5a18cabf";

    #[test]
    fn scan_reads_id_cwd_and_the_payload_start_time() {
        let tmp = TempDir::new().unwrap();
        write_rollout(
            tmp.path(),
            "2026/08/06",
            &format!("rollout-2026-08-06T00-00-00-{UUID_A}.jsonl"),
            &meta_line(
                UUID_A,
                "/work",
                "2026-08-06T21:09:59.634Z",
                "2026-08-06T21:10:47.648Z",
            ),
        );
        let got = scan_candidates(tmp.path(), 0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, UUID_A);
        assert_eq!(got[0].cwd.as_deref(), Some("/work"));
        // The PAYLOAD's 21:09:59.634, NOT the record's 21:10:47.648.
        assert_eq!(
            got[0].started_at_ms,
            parse_iso8601_ms("2026-08-06T21:09:59.634Z")
        );
    }

    #[test]
    fn scan_takes_the_id_from_the_payload_not_the_filename() {
        let tmp = TempDir::new().unwrap();
        write_rollout(
            tmp.path(),
            "2026/08/06",
            &format!("rollout-2026-08-06T00-00-00-{UUID_A}.jsonl"),
            &meta_line(UUID_B, "/work", "2026-08-06T21:00:00Z", "2026-08-06T21:00:00Z"),
        );
        assert_eq!(scan_candidates(tmp.path(), 0)[0].id, UUID_B);
    }

    #[test]
    fn scan_skips_non_rollouts_garbage_and_non_meta_first_lines() {
        let tmp = TempDir::new().unwrap();
        write_rollout(tmp.path(), "2026/08/06", "notes.txt", "hello");
        write_rollout(
            tmp.path(),
            "2026/08/06",
            &format!("rollout-2026-08-06T00-00-00-{UUID_A}.jsonl"),
            "not json at all\n",
        );
        write_rollout(
            tmp.path(),
            "2026/08/06",
            &format!("rollout-2026-08-06T00-00-00-{UUID_B}.jsonl"),
            "{\"type\":\"response_item\"}\n",
        );
        assert!(scan_candidates(tmp.path(), 0).is_empty());
    }

    #[test]
    fn scan_of_a_missing_root_is_empty_not_an_error() {
        let tmp = TempDir::new().unwrap();
        assert!(scan_candidates(&tmp.path().join("nope"), 0).is_empty());
    }

    // --- backfill_thread_id: the end-to-end shape ----------------------------

    #[test]
    fn backfill_binds_our_thread_and_ignores_the_users_other_codex() {
        // The realistic scene: a codex already open in this repo (started before
        // us, still being written), plus the one our pane just made.
        let tmp = TempDir::new().unwrap();
        let ours_started = "2026-08-06T21:09:59.000Z";
        let theirs_started = "2026-08-06T18:00:00.000Z";
        write_rollout(
            tmp.path(),
            "2026/08/06",
            &format!("rollout-2026-08-06T18-00-00-{UUID_B}.jsonl"),
            &meta_line(UUID_B, "/work", theirs_started, "2026-08-06T21:30:00Z"),
        );
        write_rollout(
            tmp.path(),
            "2026/08/06",
            &format!("rollout-2026-08-06T21-09-59-{UUID_A}.jsonl"),
            &meta_line(UUID_A, "/work", ours_started, "2026-08-06T21:10:47Z"),
        );

        // The row's startedAt: sampled just before we launched the pane.
        let since = parse_iso8601_ms("2026-08-06T21:09:58.000Z").unwrap();
        assert_eq!(
            backfill_thread_id(tmp.path(), "/work", since, &HashSet::new()),
            Some(UUID_A.to_string()),
            "binds ours; the older still-active conversation is excluded by its start time"
        );
    }

    #[test]
    fn backfill_matches_a_cwd_spelled_through_a_symlink() {
        // THE defect the end-to-end validation caught: the row's cwd and the
        // rollout's cwd name the same directory by different paths (a symlinked
        // parent), so an exact string compare never binds. Both sides are
        // canonicalized before comparison.
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real-work");
        fs::create_dir_all(&real).unwrap();
        let link = tmp.path().join("linked-work");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let root = tmp.path().join("sessions");
        write_rollout(
            &root,
            "2026/08/06",
            &format!("rollout-2026-08-06T21-09-59-{UUID_A}.jsonl"),
            // codex records the RESOLVED path...
            &meta_line(
                UUID_A,
                &real.to_string_lossy(),
                "2026-08-06T21:09:59Z",
                "2026-08-06T21:10:47Z",
            ),
        );

        let since = parse_iso8601_ms("2026-08-06T21:00:00Z").unwrap();
        // ...while the row carries the path the caller typed.
        assert_eq!(
            backfill_thread_id(&root, &link.to_string_lossy(), since, &HashSet::new()),
            Some(UUID_A.to_string()),
            "the same directory spelled two ways must still bind"
        );
    }

    #[test]
    fn backfill_still_rejects_a_genuinely_different_cwd() {
        // Normalization must not become a wildcard: a real mismatch stays a
        // mismatch (the negative control for the test above).
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("proj-a");
        let b = tmp.path().join("proj-b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let root = tmp.path().join("sessions");
        write_rollout(
            &root,
            "2026/08/06",
            &format!("rollout-2026-08-06T21-09-59-{UUID_A}.jsonl"),
            &meta_line(
                UUID_A,
                &b.to_string_lossy(),
                "2026-08-06T21:09:59Z",
                "2026-08-06T21:10:47Z",
            ),
        );
        let since = parse_iso8601_ms("2026-08-06T21:00:00Z").unwrap();
        assert_eq!(
            backfill_thread_id(&root, &a.to_string_lossy(), since, &HashSet::new()),
            None
        );
    }

    #[test]
    fn backfill_is_none_while_no_rollout_exists_yet() {
        // The ordinary state of a freshly-started session: the pane is up, the
        // human has not typed, codex has written nothing.
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            backfill_thread_id(tmp.path(), "/work", 1_000, &HashSet::new()),
            None
        );
    }
}
