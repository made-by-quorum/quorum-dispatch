//! Permissive rollout-JSONL reading (codex-p2-spec sections 3.3, 3.4, 6.4).
//!
//! **NEVER a contract (L8 / codex-p2-spec section 3.4 designed-degrade).** The
//! codex rollout JSONL is NOT schema-dumpable and its format can drift across 0.x
//! minors (e.g. the 0.137 cold-rollout compression); the schema fixture-diff
//! harness (W1) does NOT see it. So every reader here treats unreadable /
//! unparseable / unexpected input as a DEGRADE case — empty stats, status None
//! (caller falls back), a survived row — NEVER a panic or an error escape. The
//! degrade tests feed garbage bytes AND gzip-magic bytes (§13 ledger row).
//!
//! **The real rollout taxonomy (bound from disk, cited):** every line is
//! `{"timestamp": "<iso>", "type": "<kind>", "payload": {..}}` and the `event_msg`
//! kind carries a NESTED `payload.type` discriminator. Cited from
//! `exec/codex-p2-evidence/rss/rss-jail/codex-home/sessions/2026/06/07/
//! rollout-2026-06-07T02-09-07-019ea0b3-04d3-7400-8d95-f55d41e961e4.jsonl`
//! (a fresh 0.134.0 rollout with ONE COMPLETED TURN):
//!
//!   - line 0  `{"type":"session_meta","payload":{"id":..,"cwd":..,"timestamp":..}}`
//!   - line 1  `{"type":"event_msg","payload":{"type":"task_started","turn_id":..}}`
//!   - line 4  `{"type":"turn_context","payload":{"turn_id":..,..}}`
//!   - line 6  `{"type":"event_msg","payload":{"type":"user_message","message":..}}`
//!   - line 7  `{"type":"event_msg","payload":{"type":"agent_message","message":..}}`
//!   - line 9  `{"type":"event_msg","payload":{"type":"token_count",..}}`
//!   - line 10 `{"type":"event_msg","payload":{"type":"task_complete","turn_id":..,
//!              "last_agent_message":"RSS-PROBE",..}}`
//!   - `response_item` lines carry model/tool items we do not consume.
//!
//! The Busy-evidence rollout (task_started with NO matching task_complete) is
//! `exec/codex-spike-evidence/jail/codex-home/sessions/2026/06/06/
//! rollout-2026-06-06T19-19-21-019e9f3b-deea-7392-9861-b5d8ad376e2b.jsonl`.

use std::path::Path;

use serde_json::Value;

use crate::jsonl::JsonlStats;
use crate::model::{SessionStatus, TurnPreview};

/// A parsed rollout line, classified permissively by the top-level `type` and
/// (for `event_msg`) the nested `payload.type`. Unknown kinds are [`Other`].
#[derive(Debug, Clone, PartialEq)]
pub enum RolloutLine {
    /// `{"type":"session_meta","payload":{"id":..,"cwd":..,"timestamp":..}}`.
    SessionMeta {
        id: Option<String>,
        cwd: Option<String>,
    },
    /// `{"type":"turn_context",..}` — a per-turn context record (not consumed
    /// beyond its presence).
    TurnContext,
    /// `{"type":"response_item",..}` — a model/tool item (not consumed).
    ResponseItem,
    /// `{"type":"event_msg","payload":{"type":"task_started","turn_id":..}}`.
    TaskStarted { turn_id: Option<String> },
    /// `{"type":"event_msg","payload":{"type":"task_complete","turn_id":..,
    /// "last_agent_message":..}}`.
    TaskComplete { turn_id: Option<String> },
    /// `{"type":"event_msg","payload":{"type":"agent_message","message":..}}`.
    AgentMessage { message: Option<String> },
    /// `{"type":"event_msg","payload":{"type":"user_message","message":..}}`.
    UserMessage,
    /// `{"type":"event_msg","payload":{"type":"token_count","info":{
    /// "last_token_usage":{"total_tokens":..},..}}}`. `occupancy` = the current
    /// context fill derived from the PER-TURN `last_token_usage` (see
    /// [`parse_line`]); `None` when `info` is empty/aborted or the field is absent.
    TokenCount { occupancy: Option<u64> },
    /// Any line we do not specifically classify (still permissively kept).
    Other,
}

/// The top-level `timestamp` + the classified line.
#[derive(Debug, Clone, PartialEq)]
pub struct RolloutRecord {
    pub timestamp: Option<String>,
    pub line: RolloutLine,
}

/// Parse one rollout JSONL line into a [`RolloutRecord`]. Returns `None` if the
/// bytes are not a JSON object (a degrade case — the caller skips it).
pub fn parse_line(raw: &str) -> Option<RolloutRecord> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let obj = v.as_object()?;
    let timestamp = obj
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let kind = obj.get("type").and_then(Value::as_str);
    let payload = obj.get("payload");
    let line = match kind {
        Some("session_meta") => {
            let id = payload
                .and_then(|p| p.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let cwd = payload
                .and_then(|p| p.get("cwd"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            RolloutLine::SessionMeta { id, cwd }
        }
        Some("turn_context") => RolloutLine::TurnContext,
        Some("response_item") => RolloutLine::ResponseItem,
        Some("event_msg") => {
            let ev = payload.and_then(|p| p.get("type")).and_then(Value::as_str);
            match ev {
                Some("task_started") => RolloutLine::TaskStarted {
                    turn_id: payload
                        .and_then(|p| p.get("turn_id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
                Some("task_complete") => RolloutLine::TaskComplete {
                    turn_id: payload
                        .and_then(|p| p.get("turn_id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
                Some("agent_message") => RolloutLine::AgentMessage {
                    message: payload
                        .and_then(|p| p.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
                Some("user_message") => RolloutLine::UserMessage,
                Some("token_count") => RolloutLine::TokenCount {
                    occupancy: parse_occupancy(payload.and_then(|p| p.get("info"))),
                },
                _ => RolloutLine::Other,
            }
        }
        _ => RolloutLine::Other,
    };
    Some(RolloutRecord { timestamp, line })
}

/// Derive the CURRENT context occupancy (Pete feedback #5) from a `token_count`
/// event's `payload.info`. Codex carries two accounting blocks:
///   - `total_token_usage.total_tokens` — MONOTONIC lifetime cumulative (a fresh
///     codex session shows 1.5M+); using it would make a context-occupancy number
///     meaningless. We do NOT use it.
///   - `last_token_usage` — the MOST-RECENT turn's `TokenUsageBreakdown`. Its
///     `total_tokens` is the prompt-as-sent + the output for that turn = the
///     current context fill, the codex analog of the claude
///     `input + cache_read + cache_creation` number. This is what we use; it drops
///     after a compaction, matching the claude semantic.
///
/// Permissive (L8 / codex format drift across 0.x minors): empty `info:{}`
/// (aborted/probe rollout), a missing `last_token_usage`, or a missing
/// `total_tokens` → `None` (caller falls back to the last-known value, else 0).
/// We accept the rollout snake_case (`last_token_usage`/`total_tokens`) and the
/// app-server camelCase (`lastTokenUsage`/`totalTokens`) so a minor rename does
/// not silently zero the number.
fn parse_occupancy(info: Option<&Value>) -> Option<u64> {
    let info = info?;
    let last = info
        .get("last_token_usage")
        .or_else(|| info.get("lastTokenUsage"))?;
    last.get("total_tokens")
        .or_else(|| last.get("totalTokens"))
        .and_then(Value::as_u64)
}

/// Read + parse every line of a rollout file permissively. An unreadable file
/// (missing, gzip, garbage bytes) yields an EMPTY vec — never an error escape.
/// A non-JSON line is skipped, the good lines around it survive (L8).
pub fn read_lines(path: &Path) -> Vec<RolloutRecord> {
    let Ok(content) = std::fs::read_to_string(path) else {
        // Unreadable (missing) OR non-UTF-8 (gzip magic 0x1f 0x8b… is not valid
        // UTF-8) → degrade to empty, NEVER a panic.
        return Vec::new();
    };
    content
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .filter_map(parse_line)
        .collect()
}

/// Derive a connectionless ls/info status from a rollout's line tail
/// (codex-p2-spec section 3.3 dual-channel block):
///   - an OPEN `task_started` with no matching `task_complete` ⇒ `Busy`;
///   - balanced (every started turn completed) ⇒ `Idle`;
///   - empty / unreadable / no turn anchors ⇒ `None` (caller fallback).
///
/// "Matching" is by `turn_id`: a `task_complete` cancels the open `task_started`
/// with the same `turn_id`. A `task_started` whose `turn_id` is never completed
/// leaves the session Busy (the daemon writes task_started even for a
/// human-TUI-started turn under co-attach — truthful, codex-p2-spec section 3.3).
///
/// This is a provider-INTERNAL function (NOT a trait method — no other provider
/// has a rollout); invoked by the codex gather step, never the generic join.
pub fn derive_status(lines: &[RolloutRecord]) -> Option<SessionStatus> {
    // Share the open-turn tracker with `open_turn_id` (the two channels never
    // disagree): no anchors at all → None; any turn still open ⇒ Busy; balanced
    // ⇒ Idle.
    let saw_anchor = lines.iter().any(|rec| {
        matches!(
            rec.line,
            RolloutLine::TaskStarted { .. } | RolloutLine::TaskComplete { .. }
        )
    });
    if !saw_anchor {
        // No turn anchors at all → we cannot tell; caller falls back.
        return None;
    }
    if open_turns(lines).is_empty() {
        Some(SessionStatus::Idle)
    } else {
        Some(SessionStatus::Busy)
    }
}

/// The turn_id of the LAST still-open `task_started` (no matching `task_complete`)
/// — the steer precondition the W6 SEND ladder feeds to `turn/steer`'s
/// `expectedTurnId` (codex-p2-spec section 7.5). `None` when the tail is balanced
/// (every started turn completed), has no turn anchors, or is unreadable.
///
/// Shares the SAME open-turn tracking as [`derive_status`] (a `task_complete`
/// cancels the matching open `turn_id`, else the OLDEST open turn) so the two
/// channels never disagree: `open_turn_id(..).is_some()` iff
/// `derive_status(..) == Some(Busy)`. An open turn whose `turn_id` was absent on
/// the wire is tracked as a placeholder but yields `None` here (an empty
/// `expectedTurnId` is not a usable steer precondition — the caller falls back to
/// a fresh turn, which is the stale-fence outcome by another name).
///
/// Returns the LAST open turn id (the most recent still-running turn) — codex
/// runs one active turn at a time, so in practice there is at most one.
pub fn open_turn_id(lines: &[RolloutRecord]) -> Option<String> {
    let open = open_turns(lines);
    // The most recent still-open turn with a non-empty recorded id.
    open.into_iter().rev().find(|id| !id.is_empty())
}

/// The shared open-turn tracker (codex-p2-spec section 3.3 / 7.5): walk the tail
/// pairing `task_started`/`task_complete` by `turn_id`; a complete with no exact
/// match closes the OLDEST open turn (best-effort balance). Returns the ids of the
/// turns still open at the end (an absent-id start is a `""` placeholder so a
/// stray complete cannot falsely balance it). Both [`derive_status`] and
/// [`open_turn_id`] consume this, so the two channels can never disagree.
fn open_turns(lines: &[RolloutRecord]) -> Vec<String> {
    let mut open: Vec<String> = Vec::new();
    for rec in lines {
        match &rec.line {
            RolloutLine::TaskStarted { turn_id } => {
                open.push(turn_id.clone().unwrap_or_default());
            }
            RolloutLine::TaskComplete { turn_id } => {
                let want = turn_id.clone().unwrap_or_default();
                if let Some(pos) = open.iter().position(|o| o == &want) {
                    open.remove(pos);
                } else if !open.is_empty() {
                    open.remove(0);
                }
            }
            _ => {}
        }
    }
    open
}

/// Rollout-shaped transcript stats (codex-p2-spec section 6.4): turn counts (one
/// per `task_complete`), the last timestamp seen on any line, and — when
/// `include_preview` — a preview from the LAST `agent_message`. Permissive: an
/// unreadable file yields the zeroed default (NEVER a panic or error).
///
/// `read_stats(path, ..)` is exactly `read_stats_from_lines(&read_lines(path), ..)`
/// — the file read plus the derivation. Its behavior is unchanged by the split.
pub fn read_stats(path: &Path, include_preview: bool) -> JsonlStats {
    read_stats_from_lines(&read_lines(path), include_preview)
}

/// The stats derivation over ALREADY-PARSED rollout lines — the body of
/// [`read_stats`] minus the file read. Split out (lsview A1 F1) so the codex
/// live-row gather can derive stats AND connectionless status from a SINGLE
/// [`read_lines`] pass: the stats cache's status-aware reader calls this and
/// [`derive_status`] on ONE parse rather than reading the rollout twice. An
/// empty slice degrades to the zeroed default exactly as an unreadable file does.
pub fn read_stats_from_lines(lines: &[RolloutRecord], include_preview: bool) -> JsonlStats {
    let mut stats = JsonlStats::default();
    if lines.is_empty() {
        // Degrade: unreadable / empty / gzip → zeroed default (caller treats it
        // as a fallback row, never an error).
        return stats;
    }
    let mut previews: Vec<TurnPreview> = Vec::new();
    // Context occupancy (Pete #5): last-wins from `token_count` events that carry
    // a usable `last_token_usage.total_tokens` (current window fill, not lifetime).
    let mut last_occupancy: Option<u64> = None;
    for rec in lines {
        if let Some(ts) = &rec.timestamp {
            if !ts.is_empty() {
                stats.last_timestamp = Some(ts.clone());
            }
        }
        match &rec.line {
            // A completed turn is the turn anchor we count.
            RolloutLine::TaskComplete { .. } => {
                stats.turns += 1;
            }
            RolloutLine::TokenCount {
                occupancy: Some(occ),
            } => {
                last_occupancy = Some(*occ);
            }
            RolloutLine::SessionMeta { cwd, .. }
                if stats.cwd.is_none() && cwd.as_deref().is_some_and(|c| !c.is_empty()) =>
            {
                stats.cwd = cwd.clone();
            }
            RolloutLine::AgentMessage { message }
                if include_preview && message.as_deref().is_some_and(|m| !m.is_empty()) =>
            {
                let m = message.as_deref().unwrap_or_default();
                previews.push(TurnPreview {
                    role: "assistant",
                    text: m.chars().take(200).collect(),
                    timestamp: rec.timestamp.clone(),
                });
            }
            _ => {}
        }
    }
    if include_preview {
        // Preview = the LAST agent_message (codex-p2-spec section 6.4); keep the
        // last few for parity with the claude last-turns shape.
        let n = previews.len();
        stats.last_turns = Some(previews.split_off(n.saturating_sub(6)));
    }
    // Tokens = current context occupancy (Pete #5): the last `token_count` event's
    // per-turn fill. No usable token_count event (probe/aborted/old rollout) → 0.
    stats.tokens = last_occupancy.unwrap_or(0);
    stats
}

/// A rollout filename parsed into its ISO timestamp + uuidv7 id
/// (codex-p2-spec section 6.4). The on-disk shape is
/// `rollout-<ISO-ts>-<uuidv7>.jsonl` where the ISO ts uses `-` separators (e.g.
/// `rollout-2026-06-07T02-09-07-019ea0b3-04d3-7400-8d95-f55d41e961e4.jsonl`).
///
/// The uuidv7 is the LAST 5 dash-joined groups (8-4-4-4-12); the timestamp is
/// everything between the `rollout-` prefix and the uuid. Returns `None` if the
/// stem does not have the `rollout-` prefix or a trailing uuid shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutName {
    pub timestamp: String,
    pub id: String,
}

/// Parse a rollout filename (the file's basename, with or without `.jsonl`).
pub fn parse_filename(name: &str) -> Option<RolloutName> {
    let stem = name.strip_suffix(".jsonl").unwrap_or(name);
    let rest = stem.strip_prefix("rollout-")?;
    // The uuidv7 is the last 5 dash groups: 8-4-4-4-12 hex. Split on '-' and take
    // the trailing 5 groups as the id; the leading groups are the timestamp.
    let parts: Vec<&str> = rest.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    let split_at = parts.len() - 5;
    let id_parts = &parts[split_at..];
    // Validate the uuid group lengths (8-4-4-4-12) so a non-uuid tail does not
    // get mistaken for an id (still permissive — just returns None to degrade).
    let lens = [8usize, 4, 4, 4, 12];
    if id_parts.len() != 5 || !id_parts.iter().zip(lens).all(|(g, n)| g.len() == n) {
        return None;
    }
    if !id_parts
        .iter()
        .all(|g| g.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    let id = id_parts.join("-");
    let timestamp = parts[..split_at].join("-");
    if timestamp.is_empty() {
        return None;
    }
    Some(RolloutName { timestamp, id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_file(dir: &TempDir, name: &str, content: &[u8]) -> std::path::PathBuf {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content).unwrap();
        p
    }

    // === REAL rollout lines (cited inline from the on-disk evidence) ===
    //
    // The completed-turn rollout (rss-jail), lines transcribed verbatim. Top-level
    // shape `{"timestamp":..,"type":..,"payload":{..}}`; event_msg nests
    // payload.type.
    const EV_SESSION_META: &str = r#"{"timestamp":"2026-06-07T06:09:26.889Z","type":"session_meta","payload":{"id":"019ea0b3-04d3-7400-8d95-f55d41e961e4","timestamp":"2026-06-07T06:09:07.283Z","cwd":"/jail/work","originator":"qd-rss-probe","cli_version":"0.134.0"}}"#;
    const EV_TASK_STARTED: &str = r#"{"timestamp":"2026-06-07T06:09:26.899Z","type":"event_msg","payload":{"type":"task_started","turn_id":"019ea0b3-5157-7913-8a49-3308f6be7cb0","started_at":1780812566}}"#;
    const EV_TURN_CONTEXT: &str = r#"{"timestamp":"2026-06-07T06:09:26.903Z","type":"turn_context","payload":{"turn_id":"019ea0b3-5157-7913-8a49-3308f6be7cb0","cwd":"/jail/work","approval_policy":"never"}}"#;
    const EV_USER_MESSAGE: &str = r#"{"timestamp":"2026-06-07T06:09:26.909Z","type":"event_msg","payload":{"type":"user_message","message":"Reply with exactly the text RSS-PROBE and nothing else.","images":[]}}"#;
    const EV_AGENT_MESSAGE: &str = r#"{"timestamp":"2026-06-07T06:09:37.100Z","type":"event_msg","payload":{"type":"agent_message","message":"RSS-PROBE","phase":null,"memory_citation":null}}"#;
    const EV_TOKEN_COUNT: &str = r#"{"timestamp":"2026-06-07T06:09:37.101Z","type":"event_msg","payload":{"type":"token_count","info":{}}}"#;
    const EV_TASK_COMPLETE: &str = r#"{"timestamp":"2026-06-07T06:09:37.105Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"019ea0b3-5157-7913-8a49-3308f6be7cb0","last_agent_message":"RSS-PROBE","completed_at":1780812577,"duration_ms":10223}}"#;
    const EV_RESPONSE_ITEM: &str = r#"{"timestamp":"2026-06-07T06:09:26.901Z","type":"response_item","payload":{"type":"message","role":"user"}}"#;

    fn completed_turn_rollout() -> String {
        [
            EV_SESSION_META,
            EV_TASK_STARTED,
            EV_RESPONSE_ITEM,
            EV_TURN_CONTEXT,
            EV_USER_MESSAGE,
            EV_AGENT_MESSAGE,
            EV_TOKEN_COUNT,
            EV_TASK_COMPLETE,
        ]
        .join("\n")
            + "\n"
    }

    #[test]
    fn parses_every_real_line_kind() {
        let sm = parse_line(EV_SESSION_META).unwrap();
        assert_eq!(
            sm.line,
            RolloutLine::SessionMeta {
                id: Some("019ea0b3-04d3-7400-8d95-f55d41e961e4".into()),
                cwd: Some("/jail/work".into()),
            }
        );
        assert_eq!(sm.timestamp.as_deref(), Some("2026-06-07T06:09:26.889Z"));

        assert_eq!(
            parse_line(EV_TASK_STARTED).unwrap().line,
            RolloutLine::TaskStarted {
                turn_id: Some("019ea0b3-5157-7913-8a49-3308f6be7cb0".into())
            }
        );
        assert_eq!(
            parse_line(EV_TASK_COMPLETE).unwrap().line,
            RolloutLine::TaskComplete {
                turn_id: Some("019ea0b3-5157-7913-8a49-3308f6be7cb0".into())
            }
        );
        assert_eq!(
            parse_line(EV_AGENT_MESSAGE).unwrap().line,
            RolloutLine::AgentMessage {
                message: Some("RSS-PROBE".into())
            }
        );
        assert_eq!(
            parse_line(EV_USER_MESSAGE).unwrap().line,
            RolloutLine::UserMessage
        );
        assert_eq!(
            parse_line(EV_TOKEN_COUNT).unwrap().line,
            // empty `info:{}` → no usable occupancy.
            RolloutLine::TokenCount { occupancy: None }
        );
        assert_eq!(
            parse_line(EV_TURN_CONTEXT).unwrap().line,
            RolloutLine::TurnContext
        );
        assert_eq!(
            parse_line(EV_RESPONSE_ITEM).unwrap().line,
            RolloutLine::ResponseItem
        );
    }

    // === derive_status truth table (codex-p2-spec section 3.3 + §13 ledger) ===

    // MUTATION EVIDENCE (codex-p2-spec section 13 "rollout busy/idle anchor
    // inverted"): an OPEN task_started with no matching task_complete MUST be Busy.
    // If derive_status inverted the rule (open ⇒ Idle, balanced ⇒ Busy) this reds.
    // NAMED: open_task_started_is_busy.
    #[test]
    fn open_task_started_is_busy() {
        // Real Busy rollout shape (spike 019e9f3b: task_started, NO task_complete).
        let lines: Vec<RolloutRecord> = [EV_SESSION_META, EV_TASK_STARTED, EV_AGENT_MESSAGE]
            .iter()
            .filter_map(|l| parse_line(l))
            .collect();
        assert_eq!(
            derive_status(&lines),
            Some(SessionStatus::Busy),
            "an open task_started with no task_complete must be Busy"
        );
    }

    #[test]
    fn balanced_turns_are_idle() {
        let content = completed_turn_rollout();
        let lines: Vec<RolloutRecord> = content.lines().filter_map(parse_line).collect();
        assert_eq!(
            derive_status(&lines),
            Some(SessionStatus::Idle),
            "a started turn that completed must be Idle"
        );
    }

    #[test]
    fn no_anchors_is_none() {
        let lines: Vec<RolloutRecord> = [EV_SESSION_META, EV_USER_MESSAGE]
            .iter()
            .filter_map(|l| parse_line(l))
            .collect();
        assert_eq!(
            derive_status(&lines),
            None,
            "no turn anchors → None (caller falls back)"
        );
    }

    #[test]
    fn empty_lines_is_none() {
        assert_eq!(derive_status(&[]), None);
    }

    #[test]
    fn two_started_one_complete_is_busy() {
        // turn A completes, turn B is still open → Busy (match by turn_id).
        let a_start = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"A"}}"#;
        let a_done = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"A"}}"#;
        let b_start = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"B"}}"#;
        let lines: Vec<RolloutRecord> = [a_start, a_done, b_start]
            .iter()
            .filter_map(|l| parse_line(l))
            .collect();
        assert_eq!(derive_status(&lines), Some(SessionStatus::Busy));
    }

    // === open_turn_id truth table (codex-p2-spec section 7.5 — the steer
    // precondition; §13 ledger "rollout busy/idle anchor inverted" shares the
    // tracker, so this also pins the open-turn pairing) ===

    // The Busy/Idle channel (derive_status) and the steer-precondition channel
    // (open_turn_id) SHARE the open-turn tracker — this row pins the invariant
    // `open_turn_id(..).is_some() == (derive_status(..) == Some(Busy))` on the
    // real fixtures. A refactor that let them disagree reds here.
    fn parse_all(lines: &[&str]) -> Vec<RolloutRecord> {
        lines.iter().filter_map(|l| parse_line(l)).collect()
    }

    #[test]
    fn open_turn_id_balanced_is_none() {
        // The real completed-turn rollout: every started turn completed → None.
        let lines: Vec<RolloutRecord> = completed_turn_rollout()
            .lines()
            .filter_map(parse_line)
            .collect();
        assert_eq!(open_turn_id(&lines), None, "balanced tail → no open turn");
        // Channel agreement: balanced ⇒ Idle, and open_turn_id None.
        assert_eq!(derive_status(&lines), Some(SessionStatus::Idle));
    }

    #[test]
    fn open_turn_id_one_open_is_its_id() {
        // The real Busy rollout shape (task_started, NO task_complete) → the open
        // turn's id is returned (this is exactly what feeds turn/steer's
        // expectedTurnId in the W6 SEND ladder).
        let lines = parse_all(&[EV_SESSION_META, EV_TASK_STARTED, EV_AGENT_MESSAGE]);
        assert_eq!(
            open_turn_id(&lines).as_deref(),
            Some("019ea0b3-5157-7913-8a49-3308f6be7cb0"),
            "an open task_started returns its turn_id"
        );
        // Channel agreement: an open turn ⇒ Busy.
        assert_eq!(derive_status(&lines), Some(SessionStatus::Busy));
    }

    #[test]
    fn open_turn_id_no_anchors_is_none() {
        let lines = parse_all(&[EV_SESSION_META, EV_USER_MESSAGE]);
        assert_eq!(open_turn_id(&lines), None, "no anchors → no open turn");
    }

    #[test]
    fn open_turn_id_returns_the_latest_open_turn() {
        // turn A completes, turn B is still open → B's id is the steer target.
        let a_start = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"A"}}"#;
        let a_done = r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"A"}}"#;
        let b_start = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"B"}}"#;
        let lines = parse_all(&[a_start, a_done, b_start]);
        assert_eq!(open_turn_id(&lines).as_deref(), Some("B"));
        assert_eq!(derive_status(&lines), Some(SessionStatus::Busy));
    }

    #[test]
    fn open_turn_id_skips_an_id_less_open_turn() {
        // A started turn with NO recorded turn_id is open (so derive_status is
        // Busy), but it is not a usable steer precondition — open_turn_id returns
        // None and the SEND ladder falls back to a fresh turn.
        let no_id = r#"{"type":"event_msg","payload":{"type":"task_started"}}"#;
        let lines = parse_all(&[no_id]);
        assert_eq!(derive_status(&lines), Some(SessionStatus::Busy));
        assert_eq!(
            open_turn_id(&lines),
            None,
            "an open turn with no recorded id is not a usable expectedTurnId"
        );
    }

    #[test]
    fn open_turn_id_degrades_on_garbage() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "rollout.jsonl", b"\x00not json\xff");
        assert_eq!(open_turn_id(&read_lines(&path)), None, "garbage → None");
    }

    // === read_stats over the real completed-turn rollout ===

    #[test]
    fn read_stats_over_real_completed_turn() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "rollout.jsonl", completed_turn_rollout().as_bytes());
        let stats = read_stats(&path, true);
        assert_eq!(stats.turns, 1, "one task_complete = one turn");
        assert_eq!(
            stats.last_timestamp.as_deref(),
            Some("2026-06-07T06:09:37.105Z"),
            "last timestamp = the task_complete line"
        );
        assert_eq!(stats.cwd.as_deref(), Some("/jail/work"));
        let previews = stats.last_turns.expect("preview present");
        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0].role, "assistant");
        assert_eq!(
            previews[0].text, "RSS-PROBE",
            "preview = last agent_message"
        );
        // The RSS-PROBE rollout's token_count carries an EMPTY `info:{}` (a real
        // probe artifact) → no usable occupancy → 0 (L8 degrade, not a panic).
        assert_eq!(stats.tokens, 0, "empty token_count info → 0 occupancy");
    }

    #[test]
    fn occupancy_from_last_token_usage_not_lifetime_total() {
        // A populated token_count: occupancy = `last_token_usage.total_tokens`
        // (the per-turn current fill), NOT the monotonic `total_token_usage`.
        let tc = r#"{"timestamp":"2026-06-07T06:09:40.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":1537063},"last_token_usage":{"input_tokens":12000,"cached_input_tokens":3500,"output_tokens":300,"total_tokens":15800},"model_context_window":258400}}}"#;
        let lines = format!("{EV_SESSION_META}\n{EV_TASK_STARTED}\n{tc}\n{EV_TASK_COMPLETE}");
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "rollout.jsonl", lines.as_bytes());
        let stats = read_stats(&path, false);
        assert_eq!(
            stats.tokens, 15800,
            "last_token_usage.total_tokens, not 1537063"
        );
    }

    #[test]
    fn occupancy_last_token_count_wins() {
        // Two token_count events; the LAST usable one is the current fill.
        let tc1 = r#"{"timestamp":"2026-06-07T06:09:40.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":5000}}}}"#;
        let tc2 = r#"{"timestamp":"2026-06-07T06:09:50.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":9200}}}}"#;
        let lines = format!("{tc1}\n{tc2}");
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "rollout.jsonl", lines.as_bytes());
        let stats = read_stats(&path, false);
        assert_eq!(stats.tokens, 9200);
    }

    #[test]
    fn read_stats_no_preview_omits_turns_list() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "rollout.jsonl", completed_turn_rollout().as_bytes());
        let stats = read_stats(&path, false);
        assert_eq!(stats.turns, 1);
        assert!(stats.last_turns.is_none());
    }

    // === DEGRADE tests (codex-p2-spec section 13: garbage + gzip → empty stats /
    // None status, NEVER a panic / error escape) ===

    // MUTATION EVIDENCE (codex-p2-spec section 13 "rollout reader crashes on
    // unreadable input"): if read_lines/read_stats errored (unwrap, ?-escape) on
    // garbage or gzip bytes instead of degrading, these red. NAMED.
    #[test]
    fn garbage_bytes_degrade_to_empty_stats_and_none_status() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "rollout.jsonl", b"\x00\x01not json at all\xff\xfe");
        let stats = read_stats(&path, true);
        assert_eq!(stats, JsonlStats::default(), "garbage → zeroed stats");
        assert_eq!(
            derive_status(&read_lines(&path)),
            None,
            "garbage → None status"
        );
    }

    #[test]
    fn gzip_magic_bytes_degrade_not_panic() {
        // 0x1f 0x8b is the gzip magic (the 0.137 compressed-rollout class). Not
        // valid UTF-8 → read_to_string fails → empty, NEVER a panic.
        let tmp = TempDir::new().unwrap();
        let path = write_file(&tmp, "rollout.jsonl", &[0x1f, 0x8b, 0x08, 0x00, 0x00, 0xde]);
        let stats = read_stats(&path, true);
        assert_eq!(stats, JsonlStats::default());
        assert_eq!(read_lines(&path).len(), 0);
        assert_eq!(derive_status(&read_lines(&path)), None);
    }

    #[test]
    fn missing_file_degrades_to_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("does-not-exist.jsonl");
        assert_eq!(read_stats(&path, true), JsonlStats::default());
        assert!(read_lines(&path).is_empty());
    }

    #[test]
    fn a_bad_line_is_skipped_good_lines_survive() {
        let tmp = TempDir::new().unwrap();
        let content = format!("{EV_TASK_STARTED}\n{{ this is broken\n{EV_TASK_COMPLETE}\n");
        let path = write_file(&tmp, "rollout.jsonl", content.as_bytes());
        let lines = read_lines(&path);
        // The two good event lines survive; the broken middle line is dropped.
        assert_eq!(lines.len(), 2);
        assert_eq!(derive_status(&lines), Some(SessionStatus::Idle));
    }

    // === filename parsing (codex-p2-spec section 6.4) ===

    #[test]
    fn parses_real_rollout_filename() {
        let name = "rollout-2026-06-07T02-09-07-019ea0b3-04d3-7400-8d95-f55d41e961e4.jsonl";
        let parsed = parse_filename(name).unwrap();
        assert_eq!(parsed.id, "019ea0b3-04d3-7400-8d95-f55d41e961e4");
        assert_eq!(parsed.timestamp, "2026-06-07T02-09-07");
    }

    #[test]
    fn parses_filename_without_jsonl_suffix() {
        let name = "rollout-2026-06-06T19-19-21-019e9f3b-deea-7392-9861-b5d8ad376e2b";
        let parsed = parse_filename(name).unwrap();
        assert_eq!(parsed.id, "019e9f3b-deea-7392-9861-b5d8ad376e2b");
        assert_eq!(parsed.timestamp, "2026-06-06T19-19-21");
    }

    #[test]
    fn rejects_non_rollout_filename() {
        assert_eq!(parse_filename("notes.jsonl"), None);
        assert_eq!(parse_filename("rollout-no-uuid-here.jsonl"), None);
        // A trailing group that is not 8-4-4-4-12 hex degrades to None.
        assert_eq!(
            parse_filename("rollout-2026-06-07T02-09-07-not-a-real-uuid-here.jsonl"),
            None
        );
    }
}
