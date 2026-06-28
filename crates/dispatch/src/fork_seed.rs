//! WP-B5-iii — Mechanism S: fork-transcript seeding (claude-only).
//!
//! qd-side realization of `FORK-IDENTITY-SPEC.md` §3/§4/§5/§5a. To fork a session
//! at a PAST or the latest-safe boundary, qd places a forked copy of the parent
//! transcript at `<projects>/<cwd-slug>/<fork_uuid>.jsonl`; then
//! `claude --resume <fork_uuid>` (launched in the cwd whose slug holds the file)
//! resumes the fork. This is the same faithful copy/re-key claude's native
//! `--fork-session` performs (research S4.1: COPY into a NEW id-keyed file,
//! re-stamp every record's `sessionId` to the child id, PRESERVE the
//! `uuid`/`parentUuid` DAG), done by qd so it can ALSO truncate at a chosen safe
//! boundary (`--turn`) and report staleness (§5a). The seeding MECHANISM is the
//! build-time decision the spec §3 left open; this module is that mechanism.
//!
//! Characterized first-hand (`WP-B5iii-TRANSCRIPT-TRACE.md`, claude 2.1.177):
//! claude resolves `--resume <uuid>` PURELY by `<cwd-slug>/<uuid>.jsonl` — there
//! is no session index — so an qd-placed, re-keyed, truncated transcript resumes
//! cleanly (exp F-SEED) and the parent file is never touched.
//!
//! PURE: operates over the transcript text / parsed records only — no process, no
//! socket, no real claude, no filesystem (the verb layer reads the durable parent
//! file, calls these, and writes the result). Mirrors `jsonl.rs`'s permissive
//! parse (L8): a bad/torn line is skipped, never fatal.

use serde_json::Value;

/// The user-facing fork point (`FORK-IDENTITY-SPEC.md` §4). The resolved fork
/// point is ALWAYS a safe record offset (an `assistant`/`end_turn` boundary);
/// inference-step indices are never exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPoint {
    /// `--turn` omitted ⇒ fork at the LATEST safe boundary ("clone me as I am
    /// now"). Resolved against the DURABLE on-disk record, not the un-fsync'd
    /// tail (§5a).
    Latest,
    /// `--turn N` (1-based) ⇒ REWIND to the Nth conversational-turn boundary.
    Turn(usize),
}

/// Why a fork point could not be resolved — errors that teach (the verb layer
/// prints these, never an opaque downstream `claude` failure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The parent transcript has no completed conversational turn
    /// (`assistant`/`end_turn`) at all — nothing safe to fork.
    NoSafeBoundary,
    /// `--turn N` named a turn past what the transcript holds.
    TurnOutOfRange { requested: usize, available: usize },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NoSafeBoundary => write!(
                f,
                "the source transcript has no completed turn boundary to fork at \
                 (no `end_turn` record yet — let the session complete a turn first)."
            ),
            ResolveError::TurnOutOfRange {
                requested,
                available,
            } => write!(
                f,
                "--turn {requested} is out of range: the source transcript has \
                 {available} completed turn(s)."
            ),
        }
    }
}

/// A trailing in-flight tool (`FORK-IDENTITY-SPEC.md` §5 end-state 3): the
/// transcript continues PAST the latest safe boundary with an `assistant`
/// `tool_use` and no matching `tool_result` — the lone unsafe state. Captured so
/// the verb layer can REPORT staleness (§5a) rather than silently forking stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    /// The pending tool's name, if recoverable from the `tool_use` block.
    pub tool_name: Option<String>,
    /// How many records sit AFTER the chosen safe boundary (the staleness gap).
    pub records_after: usize,
}

/// A resolved fork point: where to truncate + the turn accounting + any trailing
/// in-flight tool the fork is being placed BEFORE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Inclusive truncate offset: the seed keeps records `[0..=boundary_idx]`.
    pub boundary_idx: usize,
    /// 1-based ordinal of the chosen boundary among the transcript's completed
    /// turns.
    pub turn_ordinal: usize,
    /// Total completed conversational turns in the source.
    pub total_turns: usize,
    /// Set when the source continues past the chosen boundary into an in-flight
    /// tool (§5a staleness). `None` ⇒ the boundary IS the live tail (fresh).
    pub trailing_in_flight: Option<InFlight>,
}

impl Resolved {
    /// The §5a staleness line — `Some` when the chosen safe boundary lags the
    /// live head because a tool is in flight; `None` when forking at the fresh
    /// tail. The verb layer surfaces this so a stale fork is never silent.
    pub fn staleness_report(&self) -> Option<String> {
        self.trailing_in_flight.as_ref().map(|inf| {
            let tool = inf.tool_name.as_deref().unwrap_or("a tool");
            format!(
                "forking at the latest SAFE boundary (turn {}), {} record(s) behind \
                 the live head — the source is mid-flight on {tool}; the in-flight \
                 turn is NOT included in the fork.",
                self.turn_ordinal, inf.records_after
            )
        })
    }
}

/// Permissive parse of a transcript's text into ordered records (`jsonl.rs`
/// discipline, L8): split on `\n`, skip empty + unparseable lines. In a real
/// claude transcript only the final line can be torn (claude never fsyncs,
/// §5a) — and the fork boundary is an earlier `end_turn`, so a dropped torn tail
/// never affects the seed.
pub fn parse_records(text: &str) -> Vec<Value> {
    text.split('\n')
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

fn record_type(rec: &Value) -> Option<&str> {
    rec.get("type").and_then(|v| v.as_str())
}

fn assistant_stop_reason(rec: &Value) -> Option<&str> {
    if record_type(rec) != Some("assistant") {
        return None;
    }
    rec.get("message")
        .and_then(|m| m.get("stop_reason"))
        .and_then(|v| v.as_str())
}

/// Indices of completed conversational-turn boundaries: `assistant` records
/// whose `message.stop_reason == "end_turn"` (`FORK-IDENTITY-SPEC.md` §5
/// end-state 1: a fork at `end_turn` idles → SAFE). A COMPLETED `tool_result`
/// tail is also safe (§5 end-state 2) but is not a turn boundary; the lone
/// UNSAFE state is a trailing `tool_use` with no result (end-state 3), captured
/// separately by [`trailing_in_flight`].
pub fn end_turn_indices(records: &[Value]) -> Vec<usize> {
    records
        .iter()
        .enumerate()
        .filter(|(_, r)| assistant_stop_reason(r) == Some("end_turn"))
        .map(|(i, _)| i)
        .collect()
}

/// First `tool_use` block name found in an `assistant` record's content array.
fn tool_use_name(rec: &Value) -> Option<String> {
    let content = rec.get("message").and_then(|m| m.get("content"))?;
    let arr = content.as_array()?;
    for block in arr {
        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
            return Some(
                block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("a tool")
                    .to_string(),
            );
        }
    }
    None
}

/// Does the transcript END mid-in-flight-tool (`FORK-IDENTITY-SPEC.md` §5
/// end-state 3 — the lone UNSAFE state)? Tracks the latest UNRESOLVED assistant
/// `tool_use` across the whole record list: a later `end_turn` (state 1) or a
/// `tool_result` (state 2) RESOLVES it (both safe); a `tool_use` with nothing
/// resolving it afterward means the source is mid-flight on that tool. Returns
/// the pending tool's name when the file ends unsafely, else `None` (at rest).
fn ends_in_flight(records: &[Value]) -> Option<String> {
    let mut pending: Option<String> = None;
    for rec in records {
        match assistant_stop_reason(rec) {
            Some("tool_use") => {
                pending = Some(tool_use_name(rec).unwrap_or_else(|| "a tool".to_string()))
            }
            Some("end_turn") => pending = None,
            _ => {
                if has_tool_result(rec) {
                    pending = None;
                }
            }
        }
    }
    pending
}

fn has_tool_result(rec: &Value) -> bool {
    let Some(content) = rec.get("message").and_then(|m| m.get("content")) else {
        return false;
    };
    content
        .as_array()
        .map(|arr| {
            arr.iter()
                .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
        })
        .unwrap_or(false)
}

/// Resolve a [`ForkPoint`] to a concrete safe record offset (`FORK-IDENTITY-SPEC.md`
/// §4/§5/§5a).
///
/// - **`Latest` (default), file at rest** ⇒ clone the WHOLE transcript
///   (`boundary_idx` = last record). This is faithful to native
///   `--resume <parent> --fork-session` (research S4.1: a full re-keyed copy —
///   NO dropped records, incl. post-`end_turn` trailers); the binding fidelity
///   gate pins this equivalence.
/// - **`Latest`, file ends mid-in-flight-tool** (§5 end-state 3) ⇒ back up to the
///   latest completed turn (last `end_turn`) and REPORT staleness (§5a) — never
///   fork the unsafe tail. Needs a prior `end_turn` to retreat to.
/// - **`Turn(n)`** ⇒ REWIND to the Nth conversational-turn boundary (1-based).
///   Deliberate, so not flagged as staleness.
pub fn resolve(records: &[Value], point: ForkPoint) -> Result<Resolved, ResolveError> {
    let turns = end_turn_indices(records);
    let total = turns.len();
    match point {
        ForkPoint::Turn(n) => {
            if n == 0 || n > total {
                return Err(ResolveError::TurnOutOfRange {
                    requested: n,
                    available: total,
                });
            }
            Ok(Resolved {
                boundary_idx: turns[n - 1],
                turn_ordinal: n,
                total_turns: total,
                trailing_in_flight: None,
            })
        }
        ForkPoint::Latest => match ends_in_flight(records) {
            // At rest ⇒ full faithful copy (the fidelity-gate case).
            None => {
                if records.is_empty() {
                    return Err(ResolveError::NoSafeBoundary);
                }
                Ok(Resolved {
                    boundary_idx: records.len() - 1,
                    turn_ordinal: total,
                    total_turns: total,
                    trailing_in_flight: None,
                })
            }
            // Mid-in-flight-tool ⇒ retreat to the last completed turn + report §5a.
            Some(tool) => {
                let Some(&b) = turns.last() else {
                    return Err(ResolveError::NoSafeBoundary);
                };
                Ok(Resolved {
                    boundary_idx: b,
                    turn_ordinal: total,
                    total_turns: total,
                    trailing_in_flight: Some(InFlight {
                        tool_name: Some(tool),
                        records_after: records.len() - 1 - b,
                    }),
                })
            }
        },
    }
}

/// Truncate to `[0..=boundary_idx]` and re-key: every record that carries a
/// `sessionId` is re-stamped to `fork_uuid`; `uuid`/`parentUuid` edges are
/// PRESERVED (research S4.1 A3/A4 — the same re-key claude's `--fork-session`
/// does). Returns the seed records in order.
pub fn rekey_truncate(records: &[Value], boundary_idx: usize, fork_uuid: &str) -> Vec<Value> {
    records
        .iter()
        .take(boundary_idx + 1)
        .map(|rec| {
            let mut rec = rec.clone();
            if let Some(obj) = rec.as_object_mut() {
                if obj.contains_key("sessionId") {
                    obj.insert(
                        "sessionId".to_string(),
                        Value::String(fork_uuid.to_string()),
                    );
                }
            }
            rec
        })
        .collect()
}

/// Generate a fresh RFC-4122 v4 UUID for the fork's claude session-id (option A:
/// the id is known PRE-spawn, so the verb names the seed `<uuid>.jsonl` AND mints
/// the fork's own qdId via `idstore::mint_or_get(<uuid>)` — never the parent's).
/// Self-contained `/dev/urandom` draw with a pid+nanos fallback, deliberately
/// mirroring `relay_server::random_uuid_v4` / `idstore::random_id` (the codebase
/// keeps these per-module rather than sharing). claude's `--resume` accepts a
/// canonical UUID and writes `<uuid>.jsonl` keyed exactly by it (exp F-A/F-SEED).
pub fn new_fork_uuid() -> String {
    let mut bytes = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_ok();
    if !ok {
        // Degenerate fallback (urandom unreadable — essentially never): pid+nanos.
        let seed = (std::process::id() as u128) << 64
            | (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
                & 0xFFFF_FFFF_FFFF_FFFF);
        bytes.copy_from_slice(&seed.to_le_bytes());
    }
    // RFC 4122: version 4 in the high nibble of byte 6; variant 10xx in byte 8.
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11],
        bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// Serialize seed records back to transcript text (one compact JSON object per
/// line, trailing newline) — the bytes the verb layer writes to
/// `<cwd-slug>/<fork_uuid>.jsonl`.
pub fn to_jsonl(records: &[Value]) -> String {
    let mut out = String::new();
    for rec in records {
        out.push_str(&rec.to_string());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal but faithful transcript: trailer, user, assistant/end_turn (turn
    // 1), user, assistant/end_turn (turn 2), then a trailer. sessionId = "PARENT".
    fn parent_two_turns() -> String {
        [
            r#"{"type":"queue-operation","sessionId":"PARENT"}"#,
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"PARENT"}"#,
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"PARENT","message":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"user","uuid":"u2","parentUuid":"a1","sessionId":"PARENT"}"#,
            r#"{"type":"assistant","uuid":"a2","parentUuid":"u2","sessionId":"PARENT","message":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"ai-title","sessionId":"PARENT","aiTitle":"x"}"#,
        ]
        .join("\n")
    }

    #[test]
    fn parse_skips_blank_and_torn_tail() {
        // A torn (unterminated, unparseable) final line is dropped (L8); good
        // records survive.
        let text = format!("{}\n{{\"type\":\"assist", parent_two_turns());
        let recs = parse_records(&text);
        assert_eq!(recs.len(), 6, "torn tail dropped, 6 good records kept");
    }

    #[test]
    fn end_turn_indices_finds_both_turns() {
        let recs = parse_records(&parent_two_turns());
        assert_eq!(end_turn_indices(&recs), vec![2, 4]);
    }

    #[test]
    fn resolve_latest_clones_full_transcript_when_at_rest() {
        // Parent ends safely (end_turn + ai-title trailer, no in-flight tool) →
        // the default clones the WHOLE transcript (last record idx = 5), faithful
        // to native --fork-session (no dropped trailers). NO staleness.
        let recs = parse_records(&parent_two_turns());
        let r = resolve(&recs, ForkPoint::Latest).unwrap();
        assert_eq!(
            r.boundary_idx, 5,
            "full copy: last record, not truncated at end_turn"
        );
        assert_eq!(r.turn_ordinal, 2);
        assert_eq!(r.total_turns, 2);
        assert_eq!(r.trailing_in_flight, None, "at rest ⇒ no staleness");
    }

    #[test]
    fn resolve_turn_rewinds_to_past_boundary() {
        let recs = parse_records(&parent_two_turns());
        let r = resolve(&recs, ForkPoint::Turn(1)).unwrap();
        assert_eq!(r.boundary_idx, 2, "turn 1 boundary");
        assert_eq!(r.turn_ordinal, 1);
    }

    #[test]
    fn resolve_turn_out_of_range_errors() {
        let recs = parse_records(&parent_two_turns());
        assert_eq!(
            resolve(&recs, ForkPoint::Turn(3)),
            Err(ResolveError::TurnOutOfRange {
                requested: 3,
                available: 2
            })
        );
        assert_eq!(
            resolve(&recs, ForkPoint::Turn(0)),
            Err(ResolveError::TurnOutOfRange {
                requested: 0,
                available: 2
            })
        );
    }

    #[test]
    fn resolve_no_end_turn_is_no_safe_boundary() {
        // A transcript that only ever opened a turn (user + an in-flight tool_use,
        // no end_turn) has no safe boundary.
        let text = [
            r#"{"type":"user","uuid":"u1","sessionId":"P"}"#,
            r#"{"type":"assistant","uuid":"a1","sessionId":"P","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash"}]}}"#,
        ]
        .join("\n");
        let recs = parse_records(&text);
        assert_eq!(
            resolve(&recs, ForkPoint::Latest),
            Err(ResolveError::NoSafeBoundary)
        );
    }

    #[test]
    fn rekey_truncate_restamps_session_and_preserves_dag() {
        let recs = parse_records(&parent_two_turns());
        let seed = rekey_truncate(&recs, 2, "FORK");
        assert_eq!(seed.len(), 3, "truncated to [0..=2]");
        // Every record's sessionId re-stamped to the fork id (A4).
        for rec in &seed {
            if let Some(sid) = rec.get("sessionId") {
                assert_eq!(sid.as_str(), Some("FORK"));
            }
        }
        // uuid / parentUuid DAG byte-preserved (A3).
        assert_eq!(seed[1].get("uuid").unwrap().as_str(), Some("u1"));
        assert_eq!(seed[2].get("parentUuid").unwrap().as_str(), Some("u1"));
    }

    #[test]
    fn in_flight_tool_is_detected_and_reported() {
        // Parent ends on an in-flight tool_use (no tool_result) AFTER turn 1.
        let text = [
            r#"{"type":"user","uuid":"u1","sessionId":"P"}"#,
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"P","message":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"user","uuid":"u2","parentUuid":"a1","sessionId":"P"}"#,
            r#"{"type":"assistant","uuid":"a2","parentUuid":"u2","sessionId":"P","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash"}]}}"#,
        ]
        .join("\n");
        let recs = parse_records(&text);
        let r = resolve(&recs, ForkPoint::Latest).unwrap();
        // Latest SAFE boundary is turn 1 (idx 1), NOT the in-flight tool_use tail.
        assert_eq!(r.boundary_idx, 1);
        let inf = r.trailing_in_flight.as_ref().expect("in-flight tail");
        assert_eq!(inf.tool_name.as_deref(), Some("Bash"));
        assert_eq!(inf.records_after, 2);
        // §5a staleness line surfaces the in-flight tool — never silently stale.
        let report = r.staleness_report().expect("staleness reported");
        assert!(
            report.contains("Bash"),
            "names the in-flight tool: {report}"
        );
        assert!(report.contains("turn 1"));
    }

    #[test]
    fn at_rest_transcript_is_not_in_flight() {
        // A transcript ending on end_turn (+ trailers) is at rest — never flagged
        // in-flight, so the default clones it whole with no staleness.
        let recs = parse_records(&parent_two_turns());
        assert_eq!(ends_in_flight(&recs), None);
    }

    #[test]
    fn completed_tool_result_tail_is_safe() {
        // §5 end-state 2: a tool_use FOLLOWED by a tool_result is RESOLVED → safe
        // (not in-flight), even with no trailing end_turn.
        let text = [
            r#"{"type":"user","uuid":"u1","sessionId":"P"}"#,
            r#"{"type":"assistant","uuid":"a1","sessionId":"P","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"user","uuid":"u2","sessionId":"P","message":{"content":[{"type":"tool_result"}]}}"#,
        ]
        .join("\n");
        let recs = parse_records(&text);
        assert_eq!(
            ends_in_flight(&recs),
            None,
            "completed tool_result resolves the tool_use"
        );
    }

    // --- Fix-shaped oracle mutations (prove the contract is load-bearing) -------

    #[test]
    fn oracle_mutation_latest_off_by_one_would_pick_unsafe_tail() {
        // If `resolve(Latest)` were mutated to pick `boundary+1` (the in-flight
        // tail) instead of the latest end_turn, the seed would include an
        // unsafe in-flight tool_use. Demonstrate the boundary is the END_TURN,
        // and that picking the next record would land on a tool_use (unsafe).
        let text = [
            r#"{"type":"user","uuid":"u1","sessionId":"P"}"#,
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"P","message":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"assistant","uuid":"a2","parentUuid":"a1","sessionId":"P","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash"}]}}"#,
        ]
        .join("\n");
        let recs = parse_records(&text);
        let r = resolve(&recs, ForkPoint::Latest).unwrap();
        assert_eq!(
            assistant_stop_reason(&recs[r.boundary_idx]),
            Some("end_turn")
        );
        // The mutation target: boundary+1 is the unsafe tool_use.
        assert_eq!(
            assistant_stop_reason(&recs[r.boundary_idx + 1]),
            Some("tool_use"),
            "off-by-one would seed an in-flight tool (the defect this guards)"
        );
    }

    #[test]
    fn oracle_mutation_missing_rekey_would_leak_parent_id() {
        // If rekey were skipped, seed records keep the PARENT sessionId — the
        // collision/identity-leak this guards. Prove rekey actually changes them.
        let recs = parse_records(&parent_two_turns());
        let before = recs[2]
            .get("sessionId")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let seed = rekey_truncate(&recs, 2, "FORK");
        let after = seed[2]
            .get("sessionId")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(before, "PARENT");
        assert_eq!(
            after, "FORK",
            "rekey must re-stamp; a no-op would leak PARENT"
        );
        assert_ne!(before, after);
    }

    #[test]
    fn new_fork_uuid_canonical_shape_and_unique() {
        let a = new_fork_uuid();
        let b = new_fork_uuid();
        assert_ne!(a, b, "two draws differ");
        // 8-4-4-4-12 hex, version 4, variant 8/9/a/b.
        let segs: Vec<&str> = a.split('-').collect();
        assert_eq!(
            segs.iter().map(|s| s.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_eq!(&segs[2][0..1], "4", "version nibble");
        assert!(
            matches!(&segs[3][0..1], "8" | "9" | "a" | "b"),
            "variant nibble"
        );
    }

    #[test]
    fn roundtrip_seed_is_valid_jsonl() {
        let recs = parse_records(&parent_two_turns());
        let seed = rekey_truncate(&recs, 4, "FORK");
        let text = to_jsonl(&seed);
        let reparsed = parse_records(&text);
        assert_eq!(reparsed.len(), seed.len());
        assert_eq!(reparsed, seed, "serialize∘parse round-trips");
    }
}
