//! `qd resume` decision core (spec §5.3; TS `commands/lifecycle.ts:408-530` +
//! `utils.ts:470-565`).
//!
//! Resume relaunches a COLD session in zmx (by default). The PURE pieces here are
//! the preflight deciders the bin verb drives BEFORE any spawn:
//!   - F3 cwd reality-check ([`resolve_resume_cwd`], `utils.ts:480-505`),
//!   - S2 session-name validation ([`validate_session_name`], `utils.ts:553-565`),
//!   - the zmx-name derivation ([`derive_zmx_name`], `lifecycle.ts:475-477`).
//!
//! The claude relaunch flag is `--resume <session-id>` exactly as the TS
//! `buildClaudeCmd(["--resume", session.sessionId])` does at the resume call-site
//! (`lifecycle.ts:474`). The Rust [`crate::launch::build_claude_cmd`] takes
//! `extra` args, so the bin verb passes `["--resume", session_id]` through
//! `build_new_extra_args`-free (resume does not add `--name`; it reuses the
//! existing session id) — see the bin verb for the exact assembly.

/// Result of [`resolve_resume_cwd`] — a resolved cwd, or an actionable error.
#[derive(Debug, Clone, PartialEq)]
pub enum ResumeCwd {
    Cwd(String),
    Error(String),
}

/// PURE: resolve the working directory for resume with a REALITY CHECK on the
/// recorded cwd (F3 / DUR-3+DUR-5, `resolveResumeCwd`, utils.ts:480-505).
///
/// A renamed/deleted project dir previously reached the spawn and crashed with a
/// raw ENOENT; this turns that into a clean, actionable error suggesting `--cwd`.
/// Order: explicit `--cwd` override (must exist) → recorded session cwd (must
/// exist, else error) → caller's cwd fallback. `exists` is injected (tests forge
/// the filesystem; no real fs touched).
pub fn resolve_resume_cwd(
    recorded_cwd: Option<&str>,
    override_cwd: Option<&str>,
    exists: &dyn Fn(&str) -> bool,
    fallback: &str,
) -> ResumeCwd {
    if let Some(over) = override_cwd {
        if !exists(over) {
            return ResumeCwd::Error(format!("--cwd directory does not exist: {over}"));
        }
        return ResumeCwd::Cwd(over.to_string());
    }
    if let Some(rec) = recorded_cwd {
        if !exists(rec) {
            return ResumeCwd::Error(format!(
                "Session's recorded directory no longer exists: {rec}\n  \
                 The project may have been moved, renamed, or deleted.\n  \
                 Resume it elsewhere with: qd resume <session> --cwd <dir>"
            ));
        }
        return ResumeCwd::Cwd(rec.to_string());
    }
    ResumeCwd::Cwd(fallback.to_string())
}

/// PURE: session-name safety guard (S2, `validateSessionName`, utils.ts:553-565).
///
/// Reject names that could cause path traversal via the env-file path or inject
/// shell metacharacters into the single-quoted `bash -lc` prefix (a `'` in the
/// name breaks the surrounding single-quote context). Allowed: alphanumeric,
/// hyphen, underscore, dot. Returns `None` if valid, `Some(message)` if invalid.
pub fn validate_session_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("Session name must not be empty.".to_string());
    }
    let ok = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-');
    if !ok {
        return Some(format!(
            "Session name \"{name}\" contains unsafe characters. \
             Names may only contain letters, digits, hyphens, underscores, and dots."
        ));
    }
    None
}

/// PURE: sanitize an auto-derived session name into a mux/zmx-safe name.
///
/// An auto-named session's `name` comes from the claude transcript TITLE (join.rs:
/// `stats.name`), which routinely contains spaces and punctuation — e.g.
/// "gdb debug helper". Feeding that straight into the zmx name (which also becomes
/// an env-file path component and a `bash -lc` single-quoted token) is what
/// `validate_session_name` rejects, so a `resume`/`attach` of an auto-named cold
/// session used to die with "Session name … contains unsafe characters". Pete's
/// ruling: attach/resume must "just work" — we must not punish the user for a
/// title claude generated. So we MAP unsafe bytes to `-` (then collapse runs and
/// trim edge dashes for readability) instead of refusing.
///
/// Allowed (mirrors `validate_session_name`): ASCII alphanumeric, `_`, `.`, `-`.
/// Returns "" when nothing survives (e.g. an all-emoji title) so the caller can
/// fall back to `claude-<id>`. The result always passes `validate_session_name`.
pub fn sanitize_zmx_name(name: &str) -> String {
    let mapped = name.chars().map(|c| {
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
            c
        } else {
            '-'
        }
    });
    // Collapse runs of '-' (so "a  b" → "a-b", not "a--b").
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for c in mapped {
        if c == '-' {
            if !prev_dash {
                out.push(c);
            }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}

/// PURE: derive the zmx session name for a resume (`lifecycle.ts:475-477`):
/// `opts.zmxName || session.name || "claude-" + sessionId[0..8]`.
///
/// An explicit `--zmx-name` is returned VERBATIM (the user typed it; the caller's
/// `validate_session_name` guards it loudly). An auto-derived `session.name` is run
/// through [`sanitize_zmx_name`] so a spacey/auto title produces a valid name
/// instead of a hard error; if nothing survives sanitization we fall through to the
/// `claude-<id>` fallback.
pub fn derive_zmx_name(
    zmx_name_opt: Option<&str>,
    session_name: Option<&str>,
    session_id: &str,
) -> String {
    if let Some(z) = zmx_name_opt.filter(|s| !s.is_empty()) {
        return z.to_string();
    }
    if let Some(n) = session_name.filter(|s| !s.is_empty()) {
        let safe = sanitize_zmx_name(n);
        if !safe.is_empty() {
            return safe;
        }
    }
    let slice: String = session_id.chars().take(8).collect();
    format!("claude-{slice}")
}

// ===========================================================================
// Same-name preflights + the destructive stale-pane sub-step
// ===========================================================================
//
// Moved here from the binary's `verbs::common` with `revive_claude` (the only
// caller of both). They were already deciders — one pure, one destructive —
// wearing a printing wrapper; the wrapper is what stayed behind.

/// P0 wave-2 (spec-w2-env D4) — the D4 same-name guard: is a LIVE registry
/// session OTHER than the resume target currently holding `zmx_name`?
///
/// The hazard (spike §4): resume kills a same-name zmx pane before relaunching;
/// if cold session B (named "wk") is resumed by id while a DIFFERENT live
/// session A also derives zmx name "wk", B's resume would kill A's pane. The
/// scan walks non-tombstoned registry rows with an ALIVE pid whose DERIVED zmx
/// name (`derive_zmx_name(None, row.name, row.session_id)` — exactly how a
/// session's pane is named) equals `zmx_name`, excluding the target's own rows
/// (`session_id == target_session_id` — the legitimate own-stale-pane case).
/// Returns the holder's display id via the shared fallback chain
/// ([`crate::idstore::holder_display_id`]).
///
/// CASE-FOLDED match (red-team r5 F1, lead-adjudicated): names are
/// CASE-INSENSITIVE for uniqueness (the r4 ruling) and this guard is the
/// resume-side sibling of the create gate — byte-exact comparison here let
/// `resume` revive cold `worker` beside live `WORKER` (the exact end-state
/// the create-side fix prevents), and `--zmx-name` gave a shortcut route.
pub fn live_zmx_name_holder(
    sessions_dir: &std::path::Path,
    ids_path: &std::path::Path,
    zmx_name: &str,
    target_session_id: &str,
) -> Option<String> {
    let holder = crate::registry::read_entries(sessions_dir, false)
        .into_iter()
        .filter(|s| !s.tombstoned)
        .find(|s| {
            s.entry.session_id.as_deref() != Some(target_session_id)
                && s.entry
                    .pid
                    .is_some_and(|p| p != 0 && crate::effects::is_pid_alive(p as i32))
                && derive_zmx_name(
                    None,
                    s.entry.name.as_deref(),
                    s.entry.session_id.as_deref().unwrap_or(""),
                )
                .eq_ignore_ascii_case(zmx_name)
        })?;
    Some(crate::idstore::holder_display_id(
        ids_path,
        holder.entry.session_id.as_deref(),
        holder.entry.pid,
    ))
}

/// Why [`clear_stale_panes`] refused. BOTH arms mean: nothing was killed in the
/// dir that tripped, and the caller must not launch.
///
/// The `qd <verb>:` prefix is the CALLER's — see [`line`](Self::line).
#[derive(Debug, Clone, PartialEq)]
pub enum StalePaneRefusal {
    /// r7 M1: the pane reports a non-positive pid, which is UNKNOWN, not dead
    /// (the zmx parser maps a garbled pid to 0). We cannot PROVE it is dead, so
    /// we refuse on missing evidence rather than kill.
    UnreadablePid { pane: String },
    /// The name-matching pane's process is ALIVE, so it is necessarily someone
    /// else's (the resume target was already proven non-live by the must-be-cold
    /// + id-collision preflights). Refuse loudly rather than kill a live process.
    LivePane { pane: String, pid: i32 },
}

impl StalePaneRefusal {
    /// The complete stderr line, with the CALLER's verb stamped in. The verb
    /// names the command the user typed (`resume` / `attach` / `send`), which is
    /// CLI knowledge, so it arrives as an argument rather than living in the
    /// variant.
    pub fn line(&self, verb: &str) -> String {
        format!("qd {verb}: {}", self.body())
    }

    /// The line WITHOUT its attribution — everything after `qd <verb>: `.
    ///
    /// Exists for the caller that has the message but not the verb:
    /// [`crate::contract::LaneOps::wake`] is reached from `resume`, `attach` and
    /// `send`, and a lane is not a verb, so it carries the body out and lets the
    /// verb stamp it. Inventing a verb here (the lane once passed the literal
    /// `"wake"`, a command no user can type) is the failure this replaces.
    pub fn body(&self) -> String {
        match self {
            StalePaneRefusal::UnreadablePid { pane } => format!(
                "pane \"{pane}\" reports no readable pid — cannot prove it is \
                 dead; refusing to clear it. Inspect with: zmx ls"
            ),
            StalePaneRefusal::LivePane { pane, pid } => format!(
                "a RUNNING pane named \"{pane}\" (pid {pid}) holds this name — \
                 refusing to kill a live process. Stop it first (qd stop {pane}) or rename."
            ),
        }
    }

    /// Both refusals are VERB-attributed — neither carries its own `qd <verb>:`
    /// prefix, so a caller printing [`body`](Self::body) must stamp one. Spelled
    /// out rather than left implicit so it stays an answer when a variant is
    /// added.
    pub fn is_self_attributed(&self) -> bool {
        false
    }
}

/// Red-team r6 F1 (lead-adjudicated): clear stale same-name panes SAFELY.
///
/// "Stale" means DEAD. The D4 registry guard is blind to live sessions whose
/// registry row carries no name (a revived claude relaunches with `--resume
/// <id>` only — its pane keeps the title-derived name but its new row has
/// `name=None`), so the pane-side check must not trust the registry at all:
/// for every (case-folded, r4 ruling) name-matching pane across the scanned
/// dirs, if the pane's PROCESS IS ALIVE → refuse loudly (the resume target
/// itself was already proven non-live by the must-be-cold + id-collision
/// preflights, so an alive matching pane is necessarily someone else's);
/// only a DEAD pane is killed — by its FOUND name (panes are case-preserving),
/// and ALL dead matches are cleared, not just the first (r6 M2).
///
/// Returns `Err(refusal)` on a live-pane or unknown-pid refusal (r7 M1: pid <= 0
/// is unprovable, not dead), `Ok(())` after clearing zero-or-more dead panes.
pub fn clear_stale_panes(
    mux: &dyn crate::mux::Mux,
    dirs: &[std::path::PathBuf],
    zmx_name: &str,
) -> Result<(), StalePaneRefusal> {
    for dir in dirs {
        for pane in mux
            .list(dir)
            .unwrap_or_default()
            .into_iter()
            .filter(|z| z.name.eq_ignore_ascii_case(zmx_name))
        {
            // r7 M1: a non-positive pid is UNKNOWN, not dead (the zmx parser
            // maps a garbled pid to 0) — we cannot prove the pane is dead, so
            // refuse on missing evidence rather than kill.
            if pane.pid <= 0 {
                return Err(StalePaneRefusal::UnreadablePid { pane: pane.name });
            }
            if crate::effects::is_pid_alive(pane.pid) {
                return Err(StalePaneRefusal::LivePane {
                    pane: pane.name,
                    pid: pane.pid,
                });
            }
            let _ = mux.kill(dir, &pane.name);
        }
    }
    Ok(())
}


// ===========================================================================
// Live-id-collision preflight (Pete feedback #6)
// ===========================================================================
//
// Moved here from `dispatch::resolve` + the binary's `verbs::common` with the
// acp resume choreography, which has to run these two raw-registry preflights in
// a LOAD-BEARING position (after the already-alive gate, before the resume claim)
// and could not reach back into the qd crate to do it. `dispatch::resolve`
// re-exports the pure decider so its existing importers are untouched; the
// binary keeps a printing wrapper over the refusal for its other three verbs.

// The join collapses two same-id LIVE rows to one (keep-newest, join.rs §dedupe) —
// CORRECT for the legitimate stale-old-pid + new-live-pid case (e.g. codex resume
// leaves the old row), but it HIDES a genuine collision: two distinct ALIVE
// processes sharing an id. `resolve_or_die`'s loud `Many` path never fires for an
// id-collision because the dedup already starved it. This pure decider runs over
// the RAW registry's ALIVE rows (the caller filters by `is_pid_alive`) so resume /
// attach can refuse loudly instead of silently picking a survivor.

/// A LIVE registry row reduced to the fields the collision preflight needs. The
/// caller has ALREADY confirmed `pid` is alive (`is_pid_alive`) before building
/// this — the decider is pure and trusts that filtering.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveRow {
    pub session_id: String,
    pub name: Option<String>,
    pub pid: i64,
}

/// Verdict of the live-id-collision preflight. Pure over the ALIVE rows.
#[derive(Debug, PartialEq)]
pub enum LiveIdCollision {
    /// No ALIVE row carries the target id — genuinely resumable (truly cold, or
    /// only stale dead-pid rows remain). Proceed.
    Resumable,
    /// Exactly one ALIVE row carries the target id: the session is actually live
    /// (the deduped join may report it Cold — the SEAM-3 misread). A resume would
    /// spawn a SECOND process on the same id → caller refuses ("already alive, use
    /// attach"). An attach-style verb may instead attach to this pid.
    AlreadyAlive { pid: i64 },
    /// ≥2 ALIVE rows carry the target id: a genuine duplicate-id collision. The
    /// caller MUST refuse loudly and surface all of them — never silently pick.
    Collision(Vec<LiveRow>),
}

/// Classify the target id against the ALIVE registry rows (caller pre-filters by
/// `is_pid_alive`). An empty target id never matches (the empty-id case is handled
/// by the resume verb's own "no session ID" guard, and ZmxOnly rows carry "").
pub fn detect_live_id_collision(target_id: &str, alive: &[LiveRow]) -> LiveIdCollision {
    if target_id.is_empty() {
        return LiveIdCollision::Resumable;
    }
    let matches: Vec<LiveRow> = alive
        .iter()
        .filter(|r| r.session_id == target_id)
        .cloned()
        .collect();
    match matches.len() {
        0 => LiveIdCollision::Resumable,
        1 => LiveIdCollision::AlreadyAlive {
            pid: matches[0].pid,
        },
        _ => LiveIdCollision::Collision(matches),
    }
}


/// The ALIVE registry rows carrying `target_id`, read from the RAW (pre-dedup)
/// registry. The deduped join collapses two same-id live rows to one, which is
/// exactly what hides the collision, so this reads the unmerged truth and filters
/// by `is_pid_alive` itself. An empty target id never matches (ZmxOnly rows carry
/// `""`).
pub fn alive_rows_with_id(sessions_dir: &std::path::Path, target_id: &str) -> Vec<LiveRow> {
    if target_id.is_empty() {
        return Vec::new();
    }
    crate::registry::read_entries(sessions_dir, false)
        .into_iter()
        .filter(|s| !s.tombstoned)
        .filter_map(|s| {
            let pid = s.entry.pid?;
            if s.entry.session_id.as_deref() == Some(target_id)
                && crate::effects::is_pid_alive(pid as i32)
            {
                Some(LiveRow {
                    session_id: target_id.to_string(),
                    name: s.entry.name.clone(),
                    pid,
                })
            } else {
                None
            }
        })
        .collect()
}

/// A ≥2-alive-rows-share-one-id refusal, with the rows the caller must list.
///
/// The message is MULTI-LINE — a header plus one line per colliding row — which
/// is why this is a `lines(verb)` returning a `Vec<String>` rather than a single
/// `Display`. The `qd <verb>:` prefix is the CALLER's (resume / attach / send all
/// hit this), so the verb arrives as an argument.
#[derive(Debug, Clone, PartialEq)]
pub struct IdCollisionRefusal {
    pub target_id: String,
    pub rows: Vec<LiveRow>,
}

impl IdCollisionRefusal {
    /// The complete stderr block, header first, one indented line per row.
    pub fn lines(&self, verb: &str) -> Vec<String> {
        let mut out = vec![format!(
            "qd {verb}: id collision — {} live sessions share id {} — refusing to act \
             (cannot disambiguate). Kill the duplicate(s) first:",
            self.rows.len(),
            crate::fmt::truncate_id_default(&self.target_id)
        )];
        for r in &self.rows {
            let name = r.name.as_deref().unwrap_or("(unnamed)");
            out.push(format!("  {name}\tPID {}", r.pid));
        }
        out
    }

    /// Process exit code for the refusal (the verb precedent).
    pub fn exit_code(&self) -> i32 {
        1
    }
}

/// Live-id-collision preflight SHARED by resume / attach / send (Pete feedback
/// #6). When ≥2 ALIVE rows share `target_id` the verb CANNOT disambiguate —
/// `Some(refusal)` says so and carries the rows to list; `None` means proceed.
///
/// An attach-style verb must call this too: attaching to one of two same-id
/// sessions would silently pick a survivor — exactly the bug. The single-alive
/// ("already alive") case is verb-SPECIFIC (resume refuses → attach; attach may
/// attach directly) and is NOT decided here — see [`alive_pid_for_id`].
pub fn id_collision_refusal(
    sessions_dir: &std::path::Path,
    target_id: &str,
) -> Option<IdCollisionRefusal> {
    let alive = alive_rows_with_id(sessions_dir, target_id);
    match detect_live_id_collision(target_id, &alive) {
        LiveIdCollision::Collision(rows) => Some(IdCollisionRefusal {
            target_id: target_id.to_string(),
            rows,
        }),
        _ => None,
    }
}

/// The single live pid carrying `target_id` IFF the session is actually alive
/// (exactly one alive row). `None` when truly cold OR when ≥2 collide (the caller
/// runs [`id_collision_refusal`] first for the ≥2 case). Resume uses this to
/// harden the must-be-cold gate against the deduped-status misread (SEAM 3): the
/// join may report a session Cold (dedup of a stale row) while a live pid still
/// carries its id — resuming would spawn a SECOND process on the same id.
pub fn alive_pid_for_id(sessions_dir: &std::path::Path, target_id: &str) -> Option<i64> {
    let alive = alive_rows_with_id(sessions_dir, target_id);
    match detect_live_id_collision(target_id, &alive) {
        LiveIdCollision::AlreadyAlive { pid } => Some(pid),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exists_set(set: &[&'static str]) -> impl Fn(&str) -> bool {
        let owned: Vec<String> = set.iter().map(|s| s.to_string()).collect();
        move |p: &str| owned.iter().any(|x| x == p)
    }

    // --- resolveResumeCwd (utils.test.ts:270-289) ---

    #[test]
    fn recorded_cwd_exists_used() {
        let f = exists_set(&["/proj/x"]);
        assert_eq!(
            resolve_resume_cwd(Some("/proj/x"), None, &f, "/caller"),
            ResumeCwd::Cwd("/proj/x".to_string())
        );
    }

    #[test]
    fn recorded_cwd_missing_clean_error_not_enoent() {
        let f = exists_set(&[]);
        match resolve_resume_cwd(Some("/proj/x"), None, &f, "/caller") {
            ResumeCwd::Error(e) => {
                assert!(e.contains("no longer exists: /proj/x"));
                assert!(e.contains("--cwd"));
            }
            other => panic!("expected clean error, got {other:?}"),
        }
    }

    #[test]
    fn override_wins_when_it_exists() {
        let f = exists_set(&["/proj/y"]);
        assert_eq!(
            resolve_resume_cwd(Some("/gone"), Some("/proj/y"), &f, "/c"),
            ResumeCwd::Cwd("/proj/y".to_string())
        );
    }

    #[test]
    fn override_missing_is_error() {
        let f = exists_set(&[]);
        match resolve_resume_cwd(Some("/gone"), Some("/also-gone"), &f, "/c") {
            ResumeCwd::Error(e) => assert!(e.contains("--cwd directory does not exist")),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn no_recorded_no_override_falls_back_to_caller() {
        let f = exists_set(&[]);
        assert_eq!(
            resolve_resume_cwd(None, None, &f, "/caller"),
            ResumeCwd::Cwd("/caller".to_string())
        );
    }

    // --- validateSessionName (S2) ---

    #[test]
    fn valid_names_accepted() {
        assert_eq!(validate_session_name("my-sess_1.2"), None);
        assert_eq!(validate_session_name("qdrg-abc-kill1"), None);
    }

    #[test]
    fn empty_name_rejected() {
        assert!(validate_session_name("").is_some());
    }

    #[test]
    fn traversal_and_injection_rejected() {
        // Path traversal.
        assert!(validate_session_name("../etc/passwd").is_some());
        // Shell-injection via single quote (would break the bash -lc prefix).
        assert!(validate_session_name("a'b").is_some());
        // Spaces and slashes.
        assert!(validate_session_name("a b").is_some());
        assert!(validate_session_name("a/b").is_some());
        // A semicolon (command separator).
        assert!(validate_session_name("a;rm -rf").is_some());
    }

    #[test]
    fn at_sign_rejected_guards_the_name_at_host_addressing_grammar() {
        // LOAD-BEARING ADDRESSING INVARIANT (named per the R8 lesson — guarded by a
        // test, not merely implied by the whitelist). `qd send` parses `name@host` on
        // the LAST `@` (send_unified.rs `parse_address`, `rsplit_once('@')`), and the
        // apply-driver routing filter parses the target host the SAME way (last-@).
        // BOTH rely on session names — and stable_ids (qdId base32, sessionId UUID) —
        // NEVER containing `@`; otherwise a bare local send to a session named `a@b`
        // would misparse as name=`a`, host=`b` (a phantom remote send). That invariant
        // is ENFORCED here: `validate_session_name` (run FIRST at `qd start`, create.rs
        // `run_new` step 0a) refuses `@` — it is not in the `[A-Za-z0-9._-]` whitelist.
        assert!(
            validate_session_name("a@b").is_some(),
            "'@' must be refused in a session name"
        );
        assert!(validate_session_name("cut-els@els").is_some());
        assert!(validate_session_name("@host").is_some());
        // Auto-derived names cannot smuggle the delimiter either: `sanitize_zmx_name`
        // maps `@` (a non-whitelist byte) to `-`.
        assert!(!sanitize_zmx_name("a@b").contains('@'));
    }

    // --- derive_zmx_name (lifecycle.ts:475-477) ---

    #[test]
    fn zmx_name_opt_wins() {
        assert_eq!(
            derive_zmx_name(Some("custom"), Some("name"), "abcdefgh12345"),
            "custom"
        );
    }

    #[test]
    fn session_name_when_no_opt() {
        assert_eq!(derive_zmx_name(None, Some("name"), "abcdefgh12345"), "name");
    }

    #[test]
    fn claude_prefix_fallback_takes_first_eight() {
        assert_eq!(
            derive_zmx_name(None, None, "abcdefgh12345"),
            "claude-abcdefgh"
        );
        // Short id: takes what's there.
        assert_eq!(derive_zmx_name(None, None, "abc"), "claude-abc");
    }

    #[test]
    fn empty_opt_and_name_fall_through() {
        assert_eq!(
            derive_zmx_name(Some(""), Some(""), "abcdefgh"),
            "claude-abcdefgh"
        );
    }

    // --- sanitize_zmx_name + auto-named derivation (the attach/resume "spaces in
    //     the name" fix) ---

    #[test]
    fn sanitize_spaces_to_hyphens() {
        assert_eq!(sanitize_zmx_name("gdb debug helper"), "gdb-debug-helper");
    }

    #[test]
    fn sanitize_collapses_runs_and_trims_edges() {
        // Multiple/edge unsafe chars don't leak stray or doubled hyphens.
        assert_eq!(sanitize_zmx_name("  a   b  "), "a-b");
        assert_eq!(sanitize_zmx_name("a/b;c"), "a-b-c");
        assert_eq!(
            sanitize_zmx_name("-leading.and.trailing-"),
            "leading.and.trailing"
        );
    }

    #[test]
    fn sanitize_keeps_already_valid_names_unchanged() {
        assert_eq!(sanitize_zmx_name("my-sess_1.2"), "my-sess_1.2");
    }

    #[test]
    fn sanitize_all_unsafe_yields_empty() {
        // Nothing survives → caller falls back to claude-<id>.
        assert_eq!(sanitize_zmx_name("   "), "");
        assert_eq!(sanitize_zmx_name("🙂🙂"), "");
    }

    #[test]
    fn auto_named_session_derives_a_valid_zmx_name() {
        // The end-to-end bug: an auto-named cold session's title has spaces. The
        // derived name must be sanitized AND pass validate_session_name (no longer
        // the "contains unsafe characters" hard error).
        let derived = derive_zmx_name(None, Some("gdb debug helper"), "abcdefgh12345");
        assert_eq!(derived, "gdb-debug-helper");
        assert_eq!(validate_session_name(&derived), None);
    }

    #[test]
    fn all_unsafe_session_name_falls_back_to_claude_id() {
        assert_eq!(
            derive_zmx_name(None, Some("🙂 🙂"), "abcdefgh12345"),
            "claude-abcdefgh"
        );
    }

    #[test]
    fn explicit_zmx_name_opt_is_not_sanitized() {
        // An explicit --zmx-name is the user's typed choice: returned verbatim so the
        // caller's validate_session_name can reject it LOUDLY rather than silently
        // rewriting their input.
        assert_eq!(derive_zmx_name(Some("a b"), Some("name"), "id"), "a b");
    }

    // === Moved with `clear_stale_panes` + `live_zmx_name_holder` from the
    // binary's `verbs::common` tests. The functions came here with
    // `revive_claude`, their only caller; these pins came with them so the
    // relocation did not quietly drop coverage. `RegistryEntry` + the DEAD_PID
    // constant are re-declared locally — the qd test module keeps its own copies
    // for the preflights that stayed behind.

    use crate::registry::RegistryEntry;

    // A pid that is reliably DEAD: max-ish, never a running process. is_pid_alive
    // → false (ESRCH), so a row keyed by it is the "stale dead-pid row" case.
    const DEAD_PID: i64 = 2_147_483_646;

    /// Minimal fake Mux for the clear_stale_panes pins: a fixed pane list and
    /// a recorded kill log. Everything else unreachable in these tests.
    struct PaneMux {
        panes: Vec<crate::mux::MuxSession>,
        kills: std::cell::RefCell<Vec<String>>,
    }
    impl crate::mux::Mux for PaneMux {
        fn list(&self, _d: &std::path::Path) -> std::io::Result<Vec<crate::mux::MuxSession>> {
            Ok(self.panes.clone())
        }
        fn list_raw(&self, _d: &std::path::Path) -> std::io::Result<Vec<crate::mux::MuxSession>> {
            Ok(self.panes.clone())
        }
        fn run_detached(
            &self,
            _d: &std::path::Path,
            _n: &str,
            _c: &str,
            _w: &std::path::Path,
        ) -> std::io::Result<crate::exec::ExecResult> {
            unreachable!("not exercised")
        }
        fn kill(&self, _d: &std::path::Path, name: &str) -> std::io::Result<i32> {
            self.kills.borrow_mut().push(name.to_string());
            Ok(0)
        }
        fn attach(&self, _d: &std::path::Path, _n: &str) -> std::io::Result<i32> {
            unreachable!("not exercised")
        }
        fn send(
            &self,
            _d: &std::path::Path,
            _n: &str,
            _t: &str,
        ) -> std::io::Result<crate::exec::ExecResult> {
            unreachable!("not exercised")
        }
        fn history(&self, _d: &std::path::Path, _n: &str) -> std::io::Result<String> {
            unreachable!("not exercised")
        }
        fn wait(&self, _d: &std::path::Path, _n: &[String]) -> std::io::Result<i32> {
            unreachable!("not exercised")
        }
    }

    fn pane(name: &str, pid: i32) -> crate::mux::MuxSession {
        crate::mux::MuxSession {
            name: name.to_string(),
            pid,
            clients: 0,
            created: 0,
            start_dir: String::new(),
            cmd: String::new(),
            current: false,
            socket_dir: None,
            ended: None,
            exit_code: None,
            zmx_status: None,
            err: None,
        }
    }

    /// Red-team r6 F1: an ALIVE name-matching pane (any case) is REFUSED —
    /// never killed (the registry guard is blind to revived no-name rows;
    /// "stale" means DEAD). Uses this test process's own pid as the live one.
    #[test]
    fn clear_stale_panes_refuses_alive_pane_any_case() {
        let me = std::process::id() as i32;
        let mux = PaneMux {
            panes: vec![pane("Fix-bug", me)],
            kills: std::cell::RefCell::new(vec![]),
        };
        let dirs = vec![std::path::PathBuf::from("/tmp/x")];
        let got = clear_stale_panes(&mux, &dirs, "fix-bug");
        assert!(
            matches!(got, Err(StalePaneRefusal::LivePane { .. })),
            "alive case-variant pane must refuse: {got:?}"
        );
        assert!(mux.kills.borrow().is_empty(), "NEVER kills a live pane");
    }

    /// Dead panes ARE cleared — ALL case-variants (r6 M2: not just the first),
    /// each killed by its FOUND name (panes are case-preserving).
    #[test]
    fn clear_stale_panes_kills_all_dead_variants_by_found_name() {
        let mux = PaneMux {
            panes: vec![
                pane("wk", DEAD_PID as i32),
                pane("WK", DEAD_PID as i32 - 1),
                pane("other", DEAD_PID as i32 - 2),
            ],
            kills: std::cell::RefCell::new(vec![]),
        };
        let dirs = vec![std::path::PathBuf::from("/tmp/x")];
        let got = clear_stale_panes(&mux, &dirs, "wk");
        assert_eq!(got, Ok(()));
        assert_eq!(
            *mux.kills.borrow(),
            vec!["wk".to_string(), "WK".to_string()],
            "both dead variants cleared by FOUND name; 'other' untouched"
        );
    }

    /// r7 M1: pid <= 0 is UNKNOWN (zmx parser maps a garbled pid to 0), not
    /// dead — refuse, never kill on missing evidence.
    #[test]
    fn clear_stale_panes_refuses_unreadable_pid() {
        let mux = PaneMux {
            panes: vec![pane("wk", 0)],
            kills: std::cell::RefCell::new(vec![]),
        };
        let dirs = vec![std::path::PathBuf::from("/tmp/x")];
        let got = clear_stale_panes(&mux, &dirs, "wk");
        assert!(
            matches!(got, Err(StalePaneRefusal::UnreadablePid { .. })),
            "unprovable pid must refuse: {got:?}"
        );
        assert!(mux.kills.borrow().is_empty(), "nothing killed");
    }

    /// A minimal live/dead registry row, keyed by pid (rows are `<pid>.json`, so
    /// distinct rows need distinct pids, exactly as real sessions have).
    fn row(dir: &std::path::Path, pid: i64, id: &str, name: &str) {
        let e = RegistryEntry {
            pid: Some(pid),
            session_id: Some(id.to_string()),
            name: Some(name.to_string()),
            status: Some("idle".to_string()),
            ..Default::default()
        };
        crate::registry::write_entry(dir, &e).unwrap();
    }

    // === P0 wave-2 (spec-w2-env D4): the resume same-name guard ===

    /// The spike hazard, pinned: cold session B (uuid-b, named "wk") resumed
    /// while a DIFFERENT live session A (uuid-a) also derives zmx name "wk" →
    /// the guard names A (by stable id when mapped) and blocks. The legitimate
    /// own-stale-pane case (only B's OWN rows hold the name) passes.
    #[test]
    fn resume_guard_blocks_live_other_holder_allows_own_stale() {
        let dir = tempfile::tempdir().unwrap();
        let ids_dir = tempfile::tempdir().unwrap();
        let ids_path = ids_dir.path().join("ids.jsonl");
        let me = std::process::id() as i64;

        // Own stale row (the resume TARGET's uuid) — alive or not, it is
        // excluded by session_id, so no holder is reported.
        row(dir.path(), me, "uuid-b", "wk");
        assert_eq!(
            live_zmx_name_holder(dir.path(), &ids_path, "wk", "uuid-b"),
            None,
            "the target's own row is never a blocking holder"
        );

        // A DIFFERENT live session holding the same derived name → blocked,
        // named by its STABLE id (seeded in the idstore).
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id() as i64;
        row(dir.path(), child_pid, "uuid-a", "wk");
        let mut g = || "ab3kx9mq".to_string();
        crate::idstore::mint_or_get_with(
            &ids_path,
            "uuid-a",
            Some("wk"),
            &crate::effects::FixedClock(0),
            &mut g,
        )
        .unwrap();
        let holder = live_zmx_name_holder(dir.path(), &ids_path, "wk", "uuid-b");
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(holder.as_deref(), Some("ab3kx9mq"));
    }

    /// Red-team r5 F1 (lead-adjudicated): the guard is CASE-FOLDED — a live
    /// session named `WORKER` blocks resuming a cold case-variant `worker`
    /// (and the `--zmx-name` shortcut), exactly as the create gate refuses a
    /// case-variant start (r4 ruling: names case-insensitive for uniqueness).
    /// Pre-fix the byte-exact compare revived `worker` beside live `WORKER`,
    /// recreating the r4 end-state through the resume path.
    #[test]
    fn resume_guard_blocks_case_variant_live_holder() {
        let dir = tempfile::tempdir().unwrap();
        let ids_path = dir.path().join("ids.jsonl");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id() as i64;
        row(dir.path(), child_pid, "uuid-a", "WORKER");
        // Resume target uuid-b derives zmx name "worker" — a case-variant of
        // the live holder's "WORKER" → blocked (holder reported).
        let holder = live_zmx_name_holder(dir.path(), &ids_path, "worker", "uuid-b");
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            holder.is_some(),
            "case-variant live holder must block the resume"
        );
    }

    #[test]
    fn resume_guard_skips_dead_tombstoned_and_other_names() {
        let dir = tempfile::tempdir().unwrap();
        let ids_path = dir.path().join("ids.jsonl");

        // Dead pid holding the name → not a live holder.
        row(dir.path(), DEAD_PID, "uuid-a", "wk");
        // Alive pid holding a DIFFERENT name → no match.
        row(dir.path(), std::process::id() as i64, "uuid-c", "other");
        assert_eq!(
            live_zmx_name_holder(dir.path(), &ids_path, "wk", "uuid-b"),
            None
        );

        // Tombstoned alive row holding the name → not a live holder.
        let e = RegistryEntry {
            pid: Some(std::process::id() as i64),
            session_id: Some("uuid-t".to_string()),
            name: Some("wk".to_string()),
            status: Some("idle".to_string()),
            ..Default::default()
        };
        crate::registry::write_entry(dir.path(), &e).unwrap();
        crate::registry::ensure_tombstone(dir.path(), std::process::id() as i64, Some(&e));
        assert_eq!(
            live_zmx_name_holder(dir.path(), &ids_path, "wk", "uuid-b"),
            None,
            "tombstoned rows are not live holders"
        );
    }

    /// An unmapped holder falls back to the truncated provider UUID, and the
    /// derived-name matching covers the unnamed case (`claude-<id8>`).
    #[test]
    fn resume_guard_uuid_fallback_and_derived_name_match() {
        let dir = tempfile::tempdir().unwrap();
        let ids_path = dir.path().join("ids.jsonl");
        let me = std::process::id() as i64;
        // No idstore line → display falls back to the truncated uuid.
        row(dir.path(), me, "uuid-live-holder", "wk");
        assert_eq!(
            live_zmx_name_holder(dir.path(), &ids_path, "wk", "uuid-b"),
            Some(crate::fmt::truncate_id_default("uuid-live-holder"))
        );
        // An UNNAMED live row derives `claude-<first8>` — the guard matches the
        // DERIVED pane name, exactly what the stale kill would target.
        let dir2 = tempfile::tempdir().unwrap();
        let e = RegistryEntry {
            pid: Some(me),
            session_id: Some("abcdef1234567890".to_string()),
            name: None,
            status: Some("idle".to_string()),
            ..Default::default()
        };
        crate::registry::write_entry(dir2.path(), &e).unwrap();
        assert!(
            live_zmx_name_holder(dir2.path(), &ids_path, "claude-abcdef12", "uuid-b").is_some(),
            "derived-name holders are matched"
        );
    }


    // --- live-id-collision preflight (Pete feedback #6) ---

    /// Renamed from `row` on the move out of `dispatch::resolve`: this module's
    /// other fixture of that name WRITES a registry file, and these two are not
    /// the same thing.
    fn live_row(id: &str, name: &str, pid: i64) -> LiveRow {
        LiveRow {
            session_id: id.to_string(),
            name: Some(name.to_string()),
            pid,
        }
    }

    #[test]
    fn collision_two_alive_rows_same_id_is_surfaced() {
        // THE BUG: two distinct ALIVE processes share one id. The deduped join
        // hides this (collapses to one row); the preflight must surface BOTH so
        // resume refuses loudly instead of silently picking a survivor.
        let alive = vec![
            live_row("dup-uuid", "qd-rust-dup", 100),
            live_row("dup-uuid", "qd-rust-dup", 200),
        ];
        match detect_live_id_collision("dup-uuid", &alive) {
            LiveIdCollision::Collision(rows) => {
                assert_eq!(rows.len(), 2, "both colliding rows surfaced");
                let pids: Vec<i64> = rows.iter().map(|r| r.pid).collect();
                assert!(pids.contains(&100) && pids.contains(&200));
            }
            other => panic!("expected Collision, got {other:?}"),
        }
    }

    #[test]
    fn one_alive_row_same_id_is_already_alive() {
        // The SEAM-3 misread: the join may report this session Cold (dedup of a
        // stale row), but a live pid carries its id → it is actually alive.
        let alive = vec![live_row("live-uuid", "worker", 777)];
        assert_eq!(
            detect_live_id_collision("live-uuid", &alive),
            LiveIdCollision::AlreadyAlive { pid: 777 }
        );
    }

    #[test]
    fn no_alive_row_with_id_is_resumable() {
        // Truly cold / only stale dead-pid rows (the caller filtered them out, so
        // they never appear here) → genuinely resumable. The legitimate
        // codex-resume-leaves-old-row case lands here (old pid dead → not alive).
        let alive = vec![live_row("other-uuid", "elsewhere", 5)];
        assert_eq!(
            detect_live_id_collision("cold-uuid", &alive),
            LiveIdCollision::Resumable
        );
    }

    #[test]
    fn empty_target_id_never_matches() {
        // ZmxOnly rows carry session_id ""; an empty target must not collide with
        // them (the resume verb's own empty-id guard handles the empty case).
        let alive = vec![live_row("", "zmx-only", 9), live_row("", "zmx-only-2", 10)];
        assert_eq!(
            detect_live_id_collision("", &alive),
            LiveIdCollision::Resumable
        );
    }


    // === Moved with `alive_rows_with_id` / `id_collision_refusal` /
    // `alive_pid_for_id` from the binary's `verbs::common` tests. These drive
    // REAL registry files and REAL live pids (the test process + a spawned
    // `sleep`), which is the whole point — the decider is pure and separately
    // pinned above; these pin the raw-registry read that feeds it.

    #[test]
    fn gather_keeps_only_alive_rows_matching_the_id() {
        // Rows are keyed `<pid>.json`, so distinct rows need distinct pids (as real
        // sessions have). `me` is the alive+matching row; a spawned child is an
        // alive row with the WRONG id (must be filtered out by id, not liveness).
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id() as i64;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id() as i64;
        row(dir.path(), me, "wanted", "live-match"); // alive + matching id → kept
        row(dir.path(), DEAD_PID, "wanted", "dead-match"); // matching id but DEAD → dropped
        row(dir.path(), child_pid, "other", "live-other"); // alive but WRONG id → dropped

        let got = alive_rows_with_id(dir.path(), "wanted");
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(got.len(), 1, "only the alive+matching row: {got:?}");
        assert_eq!(got[0].pid, me);
        assert_eq!(got[0].name.as_deref(), Some("live-match"));
    }

    #[test]
    fn single_alive_row_is_already_alive_not_a_collision() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id() as i64;
        row(dir.path(), me, "solo", "only");
        row(dir.path(), DEAD_PID, "solo", "stale"); // stale dead row sharing the id
                                                    // refuse_id_collision proceeds (≥2-alive only); alive_pid_for_id flags it
                                                    // alive (the SEAM-3 Cold-misread hardening input).
        assert_eq!(id_collision_refusal(dir.path(), "solo"), None);
        assert_eq!(alive_pid_for_id(dir.path(), "solo"), Some(me));
    }

    #[test]
    fn empty_or_absent_id_is_resumable() {
        let dir = tempfile::tempdir().unwrap();
        row(dir.path(), std::process::id() as i64, "", "zmx-only");
        assert!(
            alive_rows_with_id(dir.path(), "").is_empty(),
            "empty never matches"
        );
        assert_eq!(id_collision_refusal(dir.path(), ""), None);
        assert_eq!(alive_pid_for_id(dir.path(), "missing"), None);
    }

    #[test]
    fn two_alive_rows_same_id_collide_end_to_end() {
        // The real bug, end-to-end with TWO genuinely-live pids: the test process +
        // a spawned child. refuse_id_collision must refuse (exit 1); it is NOT
        // reported as merely "already alive".
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id() as i64;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id() as i64;
        row(dir.path(), me, "dup", "first");
        row(dir.path(), child_pid, "dup", "second");

        let verdict = id_collision_refusal(dir.path(), "dup");
        // Cleanup BEFORE asserting so a failed assert never leaks the child.
        let _ = child.kill();
        let _ = child.wait();

        let verdict = verdict.expect("two live pids on one id must refuse");
        assert_eq!(verdict.exit_code(), 1);
        assert_eq!(verdict.rows.len(), 2, "both colliding rows surfaced: {verdict:?}");
        // And it is a Collision, not AlreadyAlive (≥2, so alive_pid_for_id is None).
    }}
