//! Structured `pi -p --mode json` floor: pi's DEAD-ONLY 2nd transport.
//!
//! Two halves, both here:
//!   1. A PURE NDJSON lifecycle parser ([`parse_floor_stdout`]): the turn
//!      outcome (final assistant text + `stopReason` + `usage`) is read from the
//!      `turn_end` event; `agent_end` is the terminal marker. NO screen-scraping.
//!      Torn / non-JSON stdout lines are skipped, never wedged (the
//!      [`super::stdio`] framing discipline).
//!   2. A ONE-SHOT delivery driver ([`run_floor_turn`]): spawn
//!      `pi -p --mode json -c --session-dir <dir> <prompt>` (stdin null, stdout
//!      piped, stderr INHERITED — pi diagnostics never contaminate the stream),
//!      read stdout to EOF, reap. The `-p` child is single + short-lived; a
//!      wedged child is bounded by an EXACT-child kill (never a group kill — the
//!      `safe_group_kill` binding; the `-p` child spawns no resident/grandchild).
//!
//! Continuity (multi-turn state) = `-c` (--continue) + ONE dedicated
//! `--session-dir` per qd-session ([`floor_session_dir`]): the first floor turn
//! seeds the dir, every turn after passes `-c` so pi resumes the most-recent
//! session there and APPENDS to the single session file (proven at A6.0). NO
//! `--provider` is ever passed — pi's own settings cred (openai-codex/gpt-5.5,
//! `~/.pi/agent`) is inherited; the floor NEVER swaps the credential.

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// The completed assistant turn captured from a `turn_end` event.
#[derive(Clone, Debug, PartialEq)]
pub struct FloorTurnOutcome {
    pub assistant_text: String,
    pub stop_reason: Option<String>,
    pub usage: Option<Value>,
}

/// Why a structurally valid terminal stream did not yield an outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FloorParseError {
    MissingAgentEnd,
    MissingTurnEnd,
}

/// Parse raw stdout from `pi -p --mode json` into the final turn outcome.
///
/// The authoritative assistant text, stop reason, and usage are read from the
/// final `turn_end.message` snapshot. `agent_end` is the terminal marker; lines
/// after it are ignored.
pub fn parse_floor_stdout(stdout: &str) -> Result<FloorTurnOutcome, FloorParseError> {
    let mut outcome = None;
    let mut saw_agent_end = false;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(obj) = value.as_object() else {
            continue;
        };
        match obj.get("type").and_then(Value::as_str) {
            Some("turn_end") => {
                outcome = parse_turn_end(&value);
            }
            Some("agent_end") => {
                saw_agent_end = true;
                break;
            }
            _ => {}
        }
    }

    if !saw_agent_end {
        return Err(FloorParseError::MissingAgentEnd);
    }
    outcome.ok_or(FloorParseError::MissingTurnEnd)
}

fn parse_turn_end(value: &Value) -> Option<FloorTurnOutcome> {
    let message = value.get("message")?;
    Some(FloorTurnOutcome {
        assistant_text: content_text(message.get("content")?),
        stop_reason: message
            .get("stopReason")
            .and_then(Value::as_str)
            .map(str::to_string),
        usage: message.get("usage").cloned(),
    })
}

fn content_text(content: &Value) -> String {
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            out.push_str(text);
        }
    }
    out
}

/// C5/C3 (D2 obligation (c)) — the content-keyed LANDING test, shared by the pi
/// RESIDENT observer (`run_pi_wait`) and the DEAD-ONLY structured floor
/// (`run_pi_floor_send`): is a send's content PRESENT as a USER-turn record in a
/// pi session jsonl, matched on its `content_sha256` (the SAME key relay
/// message-seen matches on)? A user prompt / a steer's text lands as a
/// `{"type":"message","message":{"role":"user","content":[{"type":"text",
/// "text":…}]}}` record (pi `session-manager.js appendMessage`). An ASSISTANT
/// record is NEVER a landing — this guards against the assistant echoing the
/// prompt (the false-positive). NEVER terminal-keyed. READ-ONLY.
///
/// ── WHY A USER RECORD IS MATCHED TWICE (punch R10) ──────────────────────────
/// The key is sha256 of the RAW message — the one qd's intent record,
/// `send-initiated`, and `message-seen` all carry (`content_sha256` is over the
/// inner body; the relay lane ruled the same way). But a delegated send reaches
/// pi WRAPPED in the sender envelope
/// ([`crate::provider::shared::attribution`]), so the user record holds the WIRE
/// text and hashing it alone would never match — every attributed send would
/// report PENDING forever and no terminal would land. Each user record is
/// therefore hashed as-is AND, when it parses as one of our envelopes, un-wrapped
/// and hashed again. One key, both seams (the SEND-seam `confirm_landing` and the
/// WAIT-seam `observe_landed_sends`), both pi sub-lanes.
pub fn rollout_landed(session_jsonl: &str, content_sha256: &str) -> bool {
    for line in session_jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(msg) = v.get("message") else {
            continue;
        };
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(content) = msg.get("content") else {
            continue;
        };
        let text = content_text(content);
        if text.is_empty() {
            continue;
        }
        if crate::events::sha256_hex(text.as_bytes()) == content_sha256 {
            return true;
        }
        // The attributed form: strip OUR envelope and key the sender's own bytes.
        if let Some(body) = crate::provider::shared::attribution::inner_body(&text) {
            if crate::events::sha256_hex(body.as_bytes()) == content_sha256 {
                return true;
            }
        }
    }
    false
}

// ===========================================================================
// One-shot delivery driver (the floor lane). The pure argv/path builders are
// unit-tested offline; the live spawn is RUN-not-read by the gated
// `QD_PI_FLOOR_LIVE` continuity tier.
// ===========================================================================

/// Default delivery deadline for one floor turn. The `-p` child is single-shot
/// and exits on its own; this bounds a wedged child so the fallback lane
/// DEGRADES (best-effort) rather than hanging the send verb. On expiry the child
/// is killed EXACT-child (never a group kill — the `safe_group_kill` binding).
pub const DEFAULT_FLOOR_TIMEOUT: Duration = Duration::from_secs(180);

/// One floor turn's inputs.
pub struct FloorTurn<'a> {
    /// The pi binary (`QD_PI_BIN` override at the verb, else `"pi"`).
    pub pi_bin: &'a str,
    /// The dedicated per-qd-session floor dir ([`floor_session_dir`]).
    pub session_dir: &'a Path,
    /// The working directory for the turn (the qd session's cwd).
    pub cwd: &'a Path,
    /// The prompt (relay message body), passed as the trailing positional.
    pub prompt: &'a str,
    /// Pass `-c` (continue the most-recent session in `session_dir`). The caller
    /// sets this true iff `session_dir` already holds a prior floor turn
    /// ([`dir_has_session`]); a fresh dir seeds without `-c`.
    pub continue_session: bool,
}

/// Why a floor turn did not deliver a captured outcome.
#[derive(Debug)]
pub enum FloorError {
    /// The child could not be spawned (bad bin / OS error).
    Spawn(String),
    /// The child ran to exit but the stream yielded no terminal outcome.
    Parse(FloorParseError),
    /// The child exceeded [`DEFAULT_FLOOR_TIMEOUT`] and was killed (exact-child).
    Timeout,
    /// An IO/wait failure owning or reaping the child.
    Io(String),
}

impl std::fmt::Display for FloorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FloorError::Spawn(s) => write!(f, "floor spawn failed: {s}"),
            FloorError::Parse(e) => write!(f, "floor stream had no terminal outcome ({e:?})"),
            FloorError::Timeout => write!(f, "floor turn exceeded the delivery deadline"),
            FloorError::Io(s) => write!(f, "floor io: {s}"),
        }
    }
}

/// Build the floor argv (PURE, unit-testable): `-p --mode json --session-dir
/// <dir> [-c] <prompt>`. NO `--provider`/`--model` — pi's own settings cred is
/// inherited (never overridden/swapped here). The prompt is the trailing
/// positional (pi's `[messages...]`).
fn build_floor_argv(session_dir: &Path, continue_session: bool, prompt: &str) -> Vec<String> {
    let mut argv = vec![
        "-p".to_string(),
        "--mode".to_string(),
        "json".to_string(),
        "--session-dir".to_string(),
        session_dir.display().to_string(),
    ];
    if continue_session {
        argv.push("-c".to_string());
    }
    argv.push(prompt.to_string());
    argv
}

/// The floor's DEAD-ONLY gate (super-22 acceptance condition 8). Given the A5/S6
/// identity+liveness facts, decide whether the structured floor may fire. It
/// fires ONLY when there is NO identity-verified live resident: pid absent, OR
/// pid dead, OR the live `/proc` cmdline is not our `pi-daemon`, OR no endpoint
/// is recorded (the "not reachable" fence branch). When a live identity-verified
/// resident EXISTS the floor NEVER fires — even if a subsequent ws connect fails
/// or times out (a live-but-wedged resident is the rpc retry/steer path, and a
/// one-shot `-c --session-dir` concurrent with the resident's open session JSONL
/// would RACE/CORRUPT it — single-writer safety). dead ⇒ floor; alive ⇒ never.
pub fn floor_may_fire(
    pid_present: bool,
    pid_alive: bool,
    cmdline_is_ours: bool,
    endpoint_present: bool,
) -> bool {
    let resident_alive = pid_present && pid_alive && cmdline_is_ours && endpoint_present;
    !resident_alive
}

/// The dedicated floor session-dir for a qd session: ONE stable dir per qd
/// session, isolated under `pi_sessions_root/qd-floor/<session_id>` so it never
/// collides with the rpc resident's own sessions and `-c`'s most-recent pick is
/// unambiguous (only this qd-session's floor turns live here). Stable across
/// invocations (keyed by session id) so turn-2 resumes turn-1.
pub fn floor_session_dir(pi_sessions_root: &Path, session_id: &str) -> PathBuf {
    pi_sessions_root.join("qd-floor").join(session_id)
}

/// Whether `session_dir` already holds a prior floor session (⇒ pass `-c`). A
/// dir with no `.jsonl` yet ⇒ seed (no `-c`). Checks the dir and up to two
/// nested levels (pi may write flat under `--session-dir` or nest under a
/// `--<enc-cwd>--` bucket).
pub fn dir_has_session(session_dir: &Path) -> bool {
    fn has_jsonl(dir: &Path, depth: usize) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                return true;
            }
            if depth > 0 && path.is_dir() && has_jsonl(&path, depth - 1) {
                return true;
            }
        }
        false
    }
    has_jsonl(session_dir, 2)
}

/// The operator-visible drop-log line (parity with the ACP ladder's
/// `drop_log_line`: a degradation is NEVER silently swallowed). The verb writes
/// this to stderr when it drops a PROVABLY-DEAD pi resident to the floor.
pub fn floor_drop_log_line(session: &str, reason: &str) -> String {
    format!(
        "pi: session \"{session}\" dropped to the structured `-p --mode json` floor \
         (native rpc resident provably dead): {reason}"
    )
}

/// Drive ONE floor turn: spawn the `-p --mode json` child, drain its stdout to
/// EOF on a dedicated reader thread (so a full pipe never deadlocks the reap),
/// reap within `timeout`, and parse the terminal outcome from the stream.
///
/// stderr is INHERITED (pi diagnostics → our stderr, never the protocol stream —
/// the [`super::stdio`] discipline). On a timeout the child is killed EXACT-child
/// (`Child::kill`, never a process-group signal — the `safe_group_kill` binding;
/// a `-p` child spawns no resident/grandchild at A6.0). The exit code is not
/// consulted: the parser is the authority on turn completion (`agent_end`).
pub fn run_floor_turn(turn: &FloorTurn, timeout: Duration) -> Result<FloorTurnOutcome, FloorError> {
    let argv = build_floor_argv(turn.session_dir, turn.continue_session, turn.prompt);
    let mut cmd = Command::new(turn.pi_bin);
    cmd.args(&argv)
        .current_dir(turn.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd
        .spawn()
        .map_err(|e| FloorError::Spawn(format!("spawn {}: {e}", turn.pi_bin)))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| FloorError::Io("floor child stdout not piped".to_string()))?;
    // Sole reader: drain to EOF on its own thread. On a timeout kill the child
    // closes stdout, so this read returns and the join cannot hang.
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = BufReader::new(stdout).read_to_string(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let outcome_kind = loop {
        match child.try_wait() {
            Ok(Some(_status)) => break Ok(()),
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Exact-child kill — NEVER a process-group signal.
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(FloorError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(FloorError::Io(format!("wait: {e}")));
            }
        }
    };
    let stdout_buf = reader.join().unwrap_or_default();
    match outcome_kind {
        Ok(()) => parse_floor_stdout(&stdout_buf).map_err(FloorError::Parse),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PONG_FIXTURE: &str = r#"{"type":"session","version":3,"id":"a6probe1"}
{"type":"agent_start"}
{"type":"turn_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"PONG"}}
{"type":"turn_end","message":{"role":"assistant","content":[{"type":"text","text":"PONG"}],"usage":{"input":1109,"output":6,"cacheRead":0,"cacheWrite":0,"totalTokens":1115},"stopReason":"stop"},"toolResults":[]}
{"type":"agent_end","messages":[],"willRetry":false}
"#;

    #[test]
    fn parses_turn_end_outcome() {
        let parsed = parse_floor_stdout(PONG_FIXTURE).unwrap();
        assert_eq!(parsed.assistant_text, "PONG");
        assert_eq!(parsed.stop_reason.as_deref(), Some("stop"));
        assert_eq!(
            parsed.usage,
            Some(json!({
                "input": 1109,
                "output": 6,
                "cacheRead": 0,
                "cacheWrite": 0,
                "totalTokens": 1115
            }))
        );
    }

    #[test]
    fn skips_torn_lines_without_failing() {
        let fixture = PONG_FIXTURE.replace(
            "{\"type\":\"turn_start\"}",
            "{\"type\":\"turn_start\"}\nthis is not json\n{\"type\":\"message_update\"",
        );

        let parsed = parse_floor_stdout(&fixture).unwrap();
        assert_eq!(parsed.assistant_text, "PONG");
        assert_eq!(parsed.stop_reason.as_deref(), Some("stop"));
    }

    // A real recorded continue-turn `turn_end` (openai-codex/gpt-5.5 shape, with
    // the extra `textSignature`/`api`/`provider`/`responseId` fields) still
    // yields the recalled codeword — the parser reads ONLY `content[].text`.
    #[test]
    fn parses_real_continue_turn_end_shape() {
        let real = r#"{"type":"session","id":"019f2062"}
{"type":"agent_start"}
{"type":"turn_start"}
{"type":"turn_end","message":{"role":"assistant","content":[{"type":"text","text":"ZEPHYR","textSignature":"{\"v\":1}"}],"api":"openai-codex-responses","provider":"openai-codex","model":"gpt-5.5","usage":{"input":117,"output":8,"totalTokens":1149},"stopReason":"stop","responseId":"resp_x"},"toolResults":[]}
{"type":"agent_end","messages":[],"willRetry":false}
"#;
        let parsed = parse_floor_stdout(real).unwrap();
        assert_eq!(parsed.assistant_text, "ZEPHYR");
        assert_eq!(parsed.stop_reason.as_deref(), Some("stop"));
    }

    // A stream that never terminates (`agent_end` missing) is a degrade, not a
    // wedge — the outcome is unavailable rather than a hang/panic.
    #[test]
    fn missing_agent_end_is_a_clean_error() {
        let truncated = "{\"type\":\"turn_start\"}\n{\"type\":\"message_update\"}\n";
        assert_eq!(
            parse_floor_stdout(truncated),
            Err(FloorParseError::MissingAgentEnd)
        );
    }

    // The argv is the proven A6.0 form: `-p --mode json --session-dir <dir>
    // <prompt>` with NO `--provider`/`--model` (pi's own cred is inherited).
    #[test]
    fn build_floor_argv_seeds_without_continue() {
        let argv = build_floor_argv(Path::new("/s/dir"), false, "hello");
        assert_eq!(
            argv,
            vec!["-p", "--mode", "json", "--session-dir", "/s/dir", "hello"]
        );
        assert!(!argv.iter().any(|a| a == "-c"));
        // Cred is NEVER overridden on the floor argv (pi-cred-only discipline).
        assert!(!argv.iter().any(|a| a == "--provider" || a == "--model"));
    }

    // Once a prior floor turn seeded the dir, `-c` is passed BEFORE the prompt so
    // pi resumes the most-recent session (continuity), prompt stays trailing.
    #[test]
    fn build_floor_argv_continues_with_dash_c() {
        let argv = build_floor_argv(Path::new("/s/dir"), true, "recall the codeword");
        assert_eq!(
            argv,
            vec![
                "-p",
                "--mode",
                "json",
                "--session-dir",
                "/s/dir",
                "-c",
                "recall the codeword"
            ]
        );
    }

    // The floor dir is dedicated + stable per qd-session (isolated from the rpc
    // resident's sessions under a `qd-floor/<id>` bucket).
    #[test]
    fn floor_session_dir_is_dedicated_and_stable() {
        let root = Path::new("/home/u/.pi/agent/sessions");
        let d1 = floor_session_dir(root, "sess-abc");
        let d2 = floor_session_dir(root, "sess-abc");
        assert_eq!(d1, d2, "stable across turns for the same qd-session");
        assert!(d1.ends_with("qd-floor/sess-abc"));
        assert_ne!(
            d1,
            floor_session_dir(root, "sess-xyz"),
            "distinct per session"
        );
    }

    // Seed-vs-continue detection: empty dir ⇒ seed (no `-c`); a `.jsonl` present
    // (flat or nested under a `--<enc-cwd>--` bucket) ⇒ continue.
    #[test]
    fn dir_has_session_detects_prior_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("qd-floor").join("sess-1");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!dir_has_session(&dir), "fresh dir ⇒ seed");

        std::fs::write(dir.join("2026_abc.jsonl"), b"{}\n").unwrap();
        assert!(dir_has_session(&dir), "flat .jsonl ⇒ continue");

        let nested = tmp.path().join("qd-floor").join("sess-2").join("--cwd--");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("2026_def.jsonl"), b"{}\n").unwrap();
        assert!(
            dir_has_session(&tmp.path().join("qd-floor").join("sess-2")),
            "nested .jsonl ⇒ continue"
        );
    }

    // DEAD-ONLY gate (super-22 cond 8): the floor fires ONLY when NO live
    // identity-verified resident exists. Every "alive" combination must NOT floor.
    #[test]
    fn floor_may_fire_only_when_no_live_resident() {
        // The one live+identity-ok case — NEVER floor (single-writer safety).
        assert!(
            !floor_may_fire(true, true, true, true),
            "alive+ok ⇒ NEVER floor"
        );
        // Every provably-dead/gone branch ⇒ floor is safe.
        assert!(floor_may_fire(false, false, false, false), "no pid");
        assert!(floor_may_fire(true, false, true, true), "pid dead");
        assert!(floor_may_fire(true, true, false, true), "identity mismatch");
        assert!(floor_may_fire(true, true, true, false), "missing endpoint");
    }

    // Exhaustive: the ONLY non-floor verdict across all 16 fence-input combos is
    // the fully-live one (a live resident is never floored, whatever else holds).
    #[test]
    fn floor_may_fire_alive_is_the_sole_no_floor_verdict() {
        let mut no_floor = 0;
        for bits in 0..16u8 {
            let fire = floor_may_fire(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
            if !fire {
                no_floor += 1;
                assert_eq!(bits, 0b1111, "only the all-live case suppresses the floor");
            }
        }
        assert_eq!(no_floor, 1);
    }

    // The drop is observable, not silent (ACP-ladder parity).
    #[test]
    fn floor_drop_log_line_is_observable() {
        let line = floor_drop_log_line("sess-1", "!is_pid_alive(pid)");
        assert!(line.contains("sess-1"));
        assert!(line.contains("floor"));
        assert!(line.contains("provably dead"));
    }

    #[test]
    fn rollout_landed_content_keys_user_records_only() {
        let prompt = "steer: land this exact floor text";
        let sha = crate::events::sha256_hex(prompt.as_bytes());
        let user_rec = serde_json::to_string(&json!({
            "type": "message",
            "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}
        }))
        .unwrap();
        let asst_echo = serde_json::to_string(&json!({
            "type": "turn_end",
            "message": {"role": "assistant", "content": [{"type": "text", "text": prompt}], "stopReason": "stop"}
        }))
        .unwrap();

        // A USER record carrying the sent bytes → landed.
        let with_user = format!("{{\"type\":\"session\",\"id\":\"s\"}}\n{user_rec}\n{asst_echo}\n{{\"type\":\"agent_end\"}}\n");
        assert!(
            rollout_landed(&with_user, &sha),
            "user record with the sent bytes → landed"
        );
        // ONLY an assistant ECHO of the prompt (no user record) → NOT landed
        // (the false-positive guard — a steer's landing is a USER record).
        assert!(
            !rollout_landed(&asst_echo, &sha),
            "assistant echo is never a landing"
        );
        // Absent content / torn lines → not landed, never fatal.
        assert!(!rollout_landed(
            "{\"type\":\"turn_start\"}\nnot json\n",
            &sha
        ));
    }

    /// punch R10 — an ATTRIBUTED send lands as the WIRE text (the sender envelope
    /// plus the body), while every ledger record keys sha256 of the RAW message.
    /// The matcher un-wraps, so the one key still joins: without this, an
    /// attributed pi send would report PENDING forever and no terminal would ever
    /// land for it.
    #[test]
    fn rollout_landed_sees_through_the_sender_envelope() {
        let prompt = "review the diff on ftue/punch-r10 and report back";
        let sha = crate::events::sha256_hex(prompt.as_bytes());
        let wire = crate::provider::shared::attribution::attribute(prompt, "sess-A").into_owned();
        assert_ne!(
            crate::events::sha256_hex(wire.as_bytes()),
            sha,
            "non-vacuity: the wire text hashes DIFFERENTLY from the raw message"
        );
        let user_rec = serde_json::to_string(&json!({
            "type": "message",
            "message": {"role": "user", "content": [{"type": "text", "text": wire}]}
        }))
        .unwrap();
        assert!(
            rollout_landed(&format!("{user_rec}\n"), &sha),
            "the enveloped user record still keys the RAW message"
        );
        // The un-wrap is not a loosening: an unrelated body inside a well-formed
        // envelope is still not this send.
        let other = crate::provider::shared::attribution::attribute("a different task", "sess-A")
            .into_owned();
        let other_rec = serde_json::to_string(&json!({
            "type": "message",
            "message": {"role": "user", "content": [{"type": "text", "text": other}]}
        }))
        .unwrap();
        assert!(!rollout_landed(&format!("{other_rec}\n"), &sha));
        // …and an ASSISTANT echo of the WIRE text is still never a landing.
        let asst = serde_json::to_string(&json!({
            "type": "message",
            "message": {"role": "assistant", "content": [{"type": "text", "text": wire}]}
        }))
        .unwrap();
        assert!(!rollout_landed(&format!("{asst}\n"), &sha));
    }
}
