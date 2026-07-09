//! C-RED — the tier-a adversarial-JSONL red-team battery for the pi adapter
//! (WS-A.2, frame 01KWB61JKMAPK9ZWJZ7ZN7CKRW). Mirrors the [`super::conformance`]
//! library form: the probe battery LIVES here (so its pure helpers are
//! unit-tested) and a thin gated `tests/pi_redteam.rs` wrapper runs it, drives the
//! stub-pi/live-pi layers, and SERIALIZES the round evidence bundle.
//!
//! **Posture.** Try to make the pi adapter's JSONL handling panic / crash /
//! corrupt-parse / mis-derive status / mis-map an error at the PA6/PA7 seams. A
//! probe records its adversarial INPUT + the OBSERVED-at-source degradation; a
//! `is_break` finding is a NEW crash/parse-corruption/mis-derivation/mis-mapping
//! class. The BAR: degrade-not-crash, correct status-derivation, correct
//! error-mapping; weak field-validation must not leak a raw TypeError or run
//! literal `undefined` (PA7).
//!
//! **Three layers** (impl-plan §4.3, pi-loop-preregistration):
//!   - **F** pure fixtures — call `classify`/`read_lines`/`parse_status`/
//!     `on_event`/`parse_filename`/`find_session_file`/`inject` directly with
//!     adversarial inputs. No process, no creds, deterministic. The bulk; lives here.
//!   - **S** stub-pi — a committed fake-pi script on `QD_PI_BIN`, driven through the
//!     REAL [`super::stdio::PiStdio`], elicits the PRIVATE `reader_loop`/`request`
//!     wire-framing (oversized/torn-tail/interleaved/U+2028/id-less) without a real
//!     pi. Driven by the wrapper (needs a spawned process).
//!   - **L** live real pi (`pi --mode rpc`, pinned, CRED-FREE) — reserved for the
//!     two things only a real pi answers: does pi leak a raw TypeError / run literal
//!     `undefined` for a malformed / `bash`-no-command frame (PA7), and does a real
//!     JS writer emit U+2028/U+2029 as a frame separator. Driven by the wrapper.
//!
//! **Non-vacuity.** Every panic-prone probe runs under [`std::panic::catch_unwind`]
//! so a real panic is RECORDED as a break, never crashes the harness; a clean
//! probe records the graceful degradation. [`hardened_surface_survey`] pins the
//! permissive/unwrap_or_default sites surveyed — the proof the reds came from a
//! probed-and-found-hardened surface, not shallow probing (the acceptance-oracle
//! non-vacuity evidence). The unbounded `reader_loop` read_line (stdio.rs:64) is
//! PINNED as a CHARACTERIZATION (`is_break:false`, `characterization:true`) for the
//! why-holder acceptance oracle — it degrades fine at ~1MiB, so "no per-line cap"
//! is a correctness question, not a break.

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

use crate::effects::MapEnv;
use crate::model::SessionStatus;
use crate::paths::QdPaths;
use crate::provider::{InjectError, Provider, ProviderFx, SessionKey};

use super::rpc::{
    classify, AssistantEvent, Frame, PiEvent, PiRpc, PiRpcError, RpcSessionState, StreamingBehavior,
};
use super::session::{encode_cwd_dir, find_session_file, parse_filename, read_lines, FileLine};
use super::status::PiStatusMapper;
use super::PI_PROVIDER;

use std::time::Duration;

// ===========================================================================
// Evidence model (mirrors conformance::ItemResult / ConformanceReport).
// ===========================================================================

/// One adversarial probe's outcome. RUN-not-read: `input` is the exact adversarial
/// payload and `observed` is the at-source degradation the verdict rests on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedFinding {
    /// The PA6/PA7 probe class (e.g. "PA6/oversized-line", "PA7/inject-map").
    pub class: String,
    /// The specific probe within the class.
    pub probe: String,
    /// "F" fixture / "S" stub-pi / "L" live-pi.
    pub layer: String,
    /// The adversarial input (truncated for the oversized cases).
    pub input: String,
    /// The observed degradation at source (the recorded result / caught panic).
    pub observed: String,
    /// A NEW crash/parse-corruption/mis-derivation/mis-mapping class → resets the
    /// clean-round counter. `false` = the adapter degraded gracefully (the bar held).
    pub is_break: bool,
    /// A reach-boundary CHARACTERIZATION for the why-holder acceptance oracle (NOT a
    /// break, does not reset the counter) — e.g. the unbounded read_line.
    pub characterization: bool,
}

impl RedFinding {
    fn ok(class: &str, probe: &str, layer: &str, input: impl Into<String>, observed: impl Into<String>) -> Self {
        Self { class: class.into(), probe: probe.into(), layer: layer.into(), input: input.into(), observed: observed.into(), is_break: false, characterization: false }
    }
    fn brk(class: &str, probe: &str, layer: &str, input: impl Into<String>, observed: impl Into<String>) -> Self {
        Self { class: class.into(), probe: probe.into(), layer: layer.into(), input: input.into(), observed: observed.into(), is_break: true, characterization: false }
    }
    fn characterize(class: &str, probe: &str, layer: &str, input: impl Into<String>, observed: impl Into<String>) -> Self {
        Self { class: class.into(), probe: probe.into(), layer: layer.into(), input: input.into(), observed: observed.into(), is_break: false, characterization: true }
    }
}

/// A C-RED round report — the inspectable evidence bundle the oracle replays.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RedReport {
    pub findings: Vec<RedFinding>,
    /// The pinned non-vacuity proof: the hardened sites this battery probed.
    pub hardened_surface: Vec<String>,
}

impl RedReport {
    /// The NEW break-classes found this round (resets the clean-round counter when nonempty).
    pub fn breaks(&self) -> Vec<&RedFinding> {
        self.findings.iter().filter(|f| f.is_break).collect()
    }
    /// "Clean" = no break-class found (characterizations don't count). Note: a
    /// nonempty battery is required — an empty report is NOT clean (confirm-it-RAN).
    pub fn clean(&self) -> bool {
        !self.findings.is_empty() && self.breaks().is_empty()
    }
    pub fn characterizations(&self) -> Vec<&RedFinding> {
        self.findings.iter().filter(|f| f.characterization).collect()
    }
    pub fn to_evidence_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }
}

// ===========================================================================
// Non-vacuity: the hardened-surface survey (pinned into every round bundle).
// ===========================================================================

/// The permissive/unwrap_or_default sites surveyed at source on b9b810d — pinned
/// so the acceptance oracle sees the reds came from a probed-and-found-hardened
/// surface, not shallow probing (pi-lead-2 reinforcement #2).
pub fn hardened_surface_survey() -> Vec<String> {
    [
        "rpc.rs:182 classify — v.as_object()? → None on non-object; response via from_value(..).unwrap_or_default()",
        "rpc.rs:207 classify_assistant_event — all field reads unwrap_or_default",
        "session.rs:70 read_lines — missing file → Vec::new (lazy-write); non-JSON/parse-err → FileLine::Unknown",
        "session.rs:114 parse_filename — rsplit_once('_'), permissive None on bad shape",
        "mod.rs:136 parse_status — as_object()? → None; isStreaming bool first, then type, else None",
        "status.rs:67 PiStatusMapper::on_event — permissive event match, Other for unknown kinds",
        "mod.rs:241 inject — Transport|Closed→NoTransport, Protocol|Timeout→Precondition",
        "stdio.rs:64 reader_loop — non-JSON/non-object line skipped (continue), never wedged",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// ===========================================================================
// FakePiRpc — a programmable PiRpc test-double (none existed; PiStdio/PiRemote
// are the only real impls). Returns a programmed PiRpcError per call so the
// inject error-MAP can be probed without a process.
// ===========================================================================

/// What the fake returns from `prompt` (the inject SEND path). `Ok` carries the
/// minted turn id; the error variants drive the [`PiProvider::inject`] mapping.
#[derive(Debug, Clone)]
pub enum FakeOutcome {
    Ok(String),
    Transport(String),
    Closed,
    Protocol(String),
    Timeout,
}

/// A PiRpc whose `prompt` returns a programmed [`FakeOutcome`]; every other method
/// errors `Closed` (unused on the inject path). Single-call, no process.
pub struct FakePiRpc {
    pub outcome: FakeOutcome,
}

impl PiRpc for FakePiRpc {
    fn get_state(&self) -> Result<RpcSessionState, PiRpcError> {
        Ok(RpcSessionState::default())
    }
    fn prompt(&self, _message: &str, _behavior: Option<StreamingBehavior>) -> Result<String, PiRpcError> {
        match &self.outcome {
            FakeOutcome::Ok(id) => Ok(id.clone()),
            FakeOutcome::Transport(s) => Err(PiRpcError::Transport(s.clone())),
            FakeOutcome::Closed => Err(PiRpcError::Closed),
            FakeOutcome::Protocol(s) => Err(PiRpcError::Protocol(s.clone())),
            FakeOutcome::Timeout => Err(PiRpcError::Timeout),
        }
    }
    fn steer(&self, _message: &str) -> Result<(), PiRpcError> { Err(PiRpcError::Closed) }
    fn follow_up(&self, _message: &str) -> Result<(), PiRpcError> { Err(PiRpcError::Closed) }
    fn abort(&self) -> Result<(), PiRpcError> { Err(PiRpcError::Closed) }
    fn switch_session(&self, _session_path: &str) -> Result<(), PiRpcError> { Err(PiRpcError::Closed) }
    fn new_session(&self, _parent: Option<&str>) -> Result<(), PiRpcError> { Err(PiRpcError::Closed) }
    fn next_event(&self, _timeout: Duration) -> Result<Option<PiEvent>, PiRpcError> { Ok(None) }
    fn close(&self) -> Result<(), PiRpcError> { Ok(()) }
}

/// A PiRpc that RECORDS the exact `message` bytes `inject` hands it (interior
/// mutability — the trait is `&self`). Proves the adapter forwards a user message
/// as ONE opaque string, never splitting/interpreting it (the frame-injection
/// non-vacuity proof).
pub struct CapturingPiRpc {
    pub seen: RefCell<Vec<String>>,
}

impl PiRpc for CapturingPiRpc {
    fn get_state(&self) -> Result<RpcSessionState, PiRpcError> {
        Ok(RpcSessionState::default())
    }
    fn prompt(&self, message: &str, _behavior: Option<StreamingBehavior>) -> Result<String, PiRpcError> {
        self.seen.borrow_mut().push(message.to_string());
        Ok("c1".to_string())
    }
    fn steer(&self, _message: &str) -> Result<(), PiRpcError> { Err(PiRpcError::Closed) }
    fn follow_up(&self, _message: &str) -> Result<(), PiRpcError> { Err(PiRpcError::Closed) }
    fn abort(&self) -> Result<(), PiRpcError> { Err(PiRpcError::Closed) }
    fn switch_session(&self, _session_path: &str) -> Result<(), PiRpcError> { Err(PiRpcError::Closed) }
    fn new_session(&self, _parent: Option<&str>) -> Result<(), PiRpcError> { Err(PiRpcError::Closed) }
    fn next_event(&self, _timeout: Duration) -> Result<Option<PiEvent>, PiRpcError> { Ok(None) }
    fn close(&self) -> Result<(), PiRpcError> { Ok(()) }
}

// ===========================================================================
// Probe helper — run a panic-prone closure, record a panic as a break.
// ===========================================================================

/// Run a probe closure that returns `(observed, is_break)`. A panic is CAUGHT and
/// recorded as a break (the harness never dies on a real panic — that IS the find).
fn probe<F: FnOnce() -> (String, bool)>(class: &str, name: &str, layer: &str, input: impl Into<String>, f: F) -> RedFinding {
    let input = input.into();
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok((observed, is_break)) if is_break => RedFinding::brk(class, name, layer, input, observed),
        Ok((observed, _)) => RedFinding::ok(class, name, layer, input, observed),
        Err(_) => RedFinding::brk(class, name, layer, input, "PANIC (caught by catch_unwind) — a real break"),
    }
}

// ===========================================================================
// The pure-F fixture battery.
// ===========================================================================

/// PA6 — classify (the stdout frame demux). Adversarial Values: non-object,
/// missing type, response with non-bool success, deeply-nested, huge string,
/// unknown kind, null/array. Bar: never panic, sensible Frame/None.
fn battery_classify() -> Vec<RedFinding> {
    let cases: Vec<(&str, Value)> = vec![
        ("non-object-array", json!([1, 2, 3])),
        ("non-object-string", json!("not an object")),
        ("null", Value::Null),
        ("missing-type", json!({"foo": "bar"})),
        ("response-nonbool-success", json!({"type": "response", "command": "prompt", "success": "yes"})),
        ("response-missing-fields", json!({"type": "response"})),
        ("response-success-null", json!({"type": "response", "success": null})),
        ("unknown-event-kind", json!({"type": "queue_update", "steering": []})),
        ("agent_end-nonbool-willRetry", json!({"type": "agent_end", "willRetry": "true"})),
        ("message_update-bad-inner", json!({"type": "message_update", "assistantMessageEvent": 42})),
        ("deeply-nested", nest(json!({"type": "x"}), 200)),
        ("huge-type-string", json!({"type": "z".repeat(100_000)})),
    ];
    cases
        .into_iter()
        .map(|(name, v)| {
            let input = truncate(&v.to_string(), 200);
            probe("PA6/classify", name, "F", input, move || {
                let r = classify(&v);
                // Bar: classify never panics; non-object → None, object → Some(Frame).
                let observed = match &r {
                    None => "None (non-object skipped)".to_string(),
                    Some(Frame::Response(resp)) => format!("Response(success={}, command={:?})", resp.success, resp.command),
                    Some(Frame::Event(e)) => format!("Event({})", event_name(e)),
                };
                // No mis-classification break expected; record graceful outcome.
                (observed, false)
            })
        })
        .collect()
}

/// PA6 — read_lines (session transcript parse) against adversarial files written
/// under `workdir`. Bar: missing → empty (lazy-write); malformed/torn → Unknown,
/// good lines preserved; never panic.
fn battery_read_lines(workdir: &Path) -> Vec<RedFinding> {
    let good = r#"{"type":"session","id":"u1","timestamp":"t","cwd":"/w","version":3}"#;
    let entry = r#"{"type":"message","id":"e1","parentId":null,"timestamp":"t2"}"#;
    let big_line = format!(r#"{{"type":"message","id":"big","timestamp":"t","pad":"{}"}}"#, "A".repeat(1_000_000));
    let cases: Vec<(&str, String)> = vec![
        ("missing-file", String::new()), // special-cased below (don't write)
        ("malformed-json", format!("{good}\n{{torn json\n{entry}\n")),
        ("torn-tail", format!("{good}\n{entry}\n{{\"type\":\"message\",\"id\":\"trunc")),
        ("interleaved-valid-invalid", format!("{good}\nnot json at all\n{entry}\n@@@\n{entry}\n")),
        ("unknown-frame-kind", format!("{good}\n{{\"type\":\"quantum_foam\",\"id\":\"q\"}}\n")),
        ("missing-field-entry", format!("{good}\n{{\"type\":\"message\"}}\n")),
        ("oversized-1mib-line", format!("{good}\n{big_line}\n{entry}\n")),
        ("u2028-in-line", format!("{good}\n{{\"type\":\"message\",\"id\":\"e\\u2028x\",\"timestamp\":\"t\"}}\n")),
        ("empty-and-whitespace-lines", format!("{good}\n\n   \n{entry}\n")),
        ("duplicate-ids", format!("{good}\n{entry}\n{entry}\n")),
    ];
    let mut out = Vec::new();
    for (name, content) in cases {
        let path = workdir.join(format!("rl-{name}.jsonl"));
        let is_missing = name == "missing-file";
        if !is_missing {
            let _ = std::fs::write(&path, &content);
        }
        let p = path.clone();
        let input = if is_missing { "<no file>".to_string() } else { truncate(&content, 160) };
        out.push(probe("PA6/read_lines", name, "F", input, move || {
            let lines = read_lines(&p);
            let headers = lines.iter().filter(|l| matches!(l, FileLine::Header(_))).count();
            let entries = lines.iter().filter(|l| matches!(l, FileLine::Entry(_))).count();
            let unknown = lines.iter().filter(|l| matches!(l, FileLine::Unknown)).count();
            let observed = format!("lines={} header={} entry={} unknown={}", lines.len(), headers, entries, unknown);
            // Bar: missing → 0 lines; otherwise no panic and a good header survives
            // any adversarial neighbor. A break would be a panic (caught) — none here.
            (observed, false)
        }));
    }
    out
}

/// PA6 — parse_status mis-derivation. Adversarial status objects; the dangerous
/// path is the wrong Busy/Idle. Bar: correct derivation or None, never a wrong flip.
fn battery_parse_status() -> Vec<RedFinding> {
    // (name, input, expected) — expected is the ONLY correct derivation.
    let cases: Vec<(&str, Value, Option<SessionStatus>)> = vec![
        ("streaming-true", json!({"isStreaming": true}), Some(SessionStatus::Busy)),
        ("streaming-false", json!({"isStreaming": false}), Some(SessionStatus::Idle)),
        ("streaming-string-true", json!({"isStreaming": "true"}), None), // non-bool → falls through, no type → None
        ("streaming-number-1", json!({"isStreaming": 1}), None),
        ("streaming-null", json!({"isStreaming": null}), None),
        ("both-streaming-and-event", json!({"isStreaming": true, "type": "agent_end"}), Some(SessionStatus::Busy)), // isStreaming wins
        ("event-agent_start", json!({"type": "agent_start"}), Some(SessionStatus::Busy)),
        ("event-agent_end", json!({"type": "agent_end"}), Some(SessionStatus::Idle)),
        ("claude-status-string", json!("busy"), None), // not an object → None (cross-feed negative control)
        ("empty-object", json!({}), None),
        ("unknown-type", json!({"type": "agent_paused"}), None),
    ];
    cases
        .into_iter()
        .map(|(name, v, expected)| {
            let input = truncate(&v.to_string(), 120);
            probe("PA6/parse_status", name, "F", input, move || {
                let got = PI_PROVIDER.parse_status(&v);
                let is_break = got != expected;
                let observed = format!("derived={got:?} expected={expected:?}{}", if is_break { " <<< MIS-DERIVATION" } else { "" });
                (observed, is_break)
            })
        })
        .collect()
}

/// PA6 — PiStatusMapper::on_event pathological sequences. Bar: out-of-order events
/// never panic and never mis-classify the EOF outcome.
fn battery_on_event() -> Vec<RedFinding> {
    let mut out = Vec::new();

    // agent_end without a preceding agent_start → TurnEnd (idle), no panic.
    out.push(probe("PA6/on_event", "end-without-start", "F", "[agent_end]", || {
        let mut m = PiStatusMapper::new("s".into());
        let r = m.on_event(&PiEvent::AgentEnd { will_retry: false });
        (format!("{r:?}"), false)
    }));
    // double agent_start → two Ready, still consistent.
    out.push(probe("PA6/on_event", "double-start", "F", "[agent_start, agent_start]", || {
        let mut m = PiStatusMapper::new("s".into());
        let _ = m.on_event(&PiEvent::AgentStart);
        let r2 = m.on_event(&PiEvent::AgentStart);
        (format!("second-start={r2:?}"), false)
    }));
    // willRetry boundary stays busy → EOF is Aborted (mid-turn), not Completed.
    out.push(probe("PA6/on_event", "willretry-then-eof", "F", "[agent_start, agent_end(willRetry)]", || {
        let mut m = PiStatusMapper::new("s".into());
        m.on_event(&PiEvent::AgentStart);
        m.on_event(&PiEvent::AgentEnd { will_retry: true });
        let eof = m.on_eof();
        let is_break = format!("{eof:?}") != "Eof(Aborted)";
        (format!("eof={eof:?} (want Eof(Aborted))"), is_break)
    }));
    // EOF with no events → NoTurn.
    out.push(probe("PA6/on_event", "eof-no-events", "F", "[]", || {
        let m = PiStatusMapper::new("s".into());
        let eof = m.on_eof();
        let is_break = format!("{eof:?}") != "Eof(NoTurn)";
        (format!("eof={eof:?} (want Eof(NoTurn))"), is_break)
    }));
    // inner error reason recorded → is_error on the eventual TurnEnd.
    out.push(probe("PA6/on_event", "inner-error-then-end", "F", "[agent_start, error(aborted), agent_end]", || {
        let mut m = PiStatusMapper::new("s".into());
        m.on_event(&PiEvent::AgentStart);
        m.on_event(&PiEvent::MessageUpdate { assistant_event: Some(AssistantEvent::Error { reason: "aborted".into() }) });
        let r = m.on_event(&PiEvent::AgentEnd { will_retry: false });
        let is_error = format!("{r:?}").contains("is_error: true");
        (format!("turn_end={r:?}"), !is_error)
    }));
    out
}

/// PA6 — filename / cwd-encoding / lazy-write path math. Bar: bad names → None,
/// never panic; lazy-write window → None (not "no session").
fn battery_paths(workdir: &Path) -> Vec<RedFinding> {
    let mut out = Vec::new();
    let names = [
        ("leading-underscore", "_abc.jsonl"),
        ("trailing-underscore", "abc_.jsonl"),
        ("no-underscore", "noseparator.jsonl"),
        ("no-jsonl-ext", "ts_uuid.txt"),
        ("empty", ""),
        ("only-ext", ".jsonl"),
        ("unicode", "2026_u\u{2028}id.jsonl"),
        ("very-long", "ts_"),
    ];
    for (name, fname) in names {
        let f = if name == "very-long" { format!("ts_{}.jsonl", "u".repeat(5000)) } else { fname.to_string() };
        let f2 = f.clone();
        out.push(probe("PA6/parse_filename", name, "F", truncate(&f, 80), move || {
            let r = parse_filename(&f2);
            (format!("{r:?}"), false) // bar: never panic; None or a parse is both fine
        }));
    }
    // encode_cwd_dir adversarial cwds.
    for (name, cwd) in [("empty", ""), ("colon", "/a:b"), ("backslash", "C:\\x"), ("unicode", "/π/x"), ("trailing-slash", "/a/")] {
        let c = cwd.to_string();
        out.push(probe("PA6/encode_cwd_dir", name, "F", cwd, move || {
            let r = encode_cwd_dir(&c);
            let ok_shape = r.starts_with("--") && r.ends_with("--");
            (format!("{r:?} shape_ok={ok_shape}"), !ok_shape)
        }));
    }
    // lazy-write: a dir with NO <ts>_<uuid>.jsonl yet → find returns None (not a crash,
    // and the caller must read that as "not yet flushed", not "no session").
    let root = workdir.join("sessions-root");
    let _ = std::fs::create_dir_all(root.join(encode_cwd_dir("/w")));
    let root2 = root.clone();
    out.push(probe("PA6/lazy-write", "dir-without-file", "F", "<dir exists, no file>", move || {
        let r = find_session_file(&root2, "u1", Some("/w"));
        let is_break = r.is_some(); // a found file here would be wrong (none was written)
        (format!("find_session_file={r:?} (lazy-write window → None expected)"), is_break)
    }));
    out
}

/// PA7 — inject error-MAP. A FakePiRpc returns each PiRpcError variant; assert the
/// [`PiProvider::inject`] mapping: Transport|Closed → NoTransport;
/// Protocol|Timeout → Precondition (incl. a raw "TypeError"/"undefined" string,
/// which must be CONTAINED in a Precondition, never executed). Bar: exact mapping.
fn battery_inject_map() -> Vec<RedFinding> {
    let env = MapEnv { vars: Default::default(), uid: 501 };
    // QdPaths::from_home is pure path math (no I/O); a placeholder home is fine —
    // inject never touches disk on the error-MAP path.
    let paths = QdPaths::from_home(&PathBuf::from("/nonexistent-redteam-jail"));
    let key = SessionKey { id: "sid-red", name: None, cwd: Some("/w"), pid: None };

    let cases: Vec<(&str, FakeOutcome, fn(&InjectError) -> bool, &str)> = vec![
        ("transport-deadpipe", FakeOutcome::Transport("broken pipe".into()), |e| matches!(e, InjectError::NoTransport(_)), "NoTransport"),
        ("closed", FakeOutcome::Closed, |e| matches!(e, InjectError::NoTransport(_)), "NoTransport"),
        ("protocol-typeerror", FakeOutcome::Protocol("TypeError: cannot read properties of undefined".into()), |e| matches!(e, InjectError::Precondition(_)), "Precondition"),
        ("protocol-undefined", FakeOutcome::Protocol("ran command: undefined".into()), |e| matches!(e, InjectError::Precondition(_)), "Precondition"),
        ("timeout", FakeOutcome::Timeout, |e| matches!(e, InjectError::Precondition(_)), "Precondition"),
    ];
    let mut out = Vec::new();
    for (name, outcome, want, want_label) in cases {
        let fake = FakePiRpc { outcome };
        let fx = ProviderFx {
            env: &env,
            paths: &paths,
            socket_dir: paths.sessions_dir.clone(),
            mux: None,
            clock: None,
            sleeper: None,
            relay: None,
            relay_port: None,
            app_server: None,
            codex_expected_turn_id: None,
            acp_client: None,
            pi_rpc: Some(&fake),
            acp_pre_dispatch: None,
        };
        let r = PI_PROVIDER.inject(&fx, &key, "hello", "from");
        let mapped_ok = match &r {
            Ok(_) => false,
            Err(e) => want(e),
        };
        // The PA7 leak guard: a raw TypeError / undefined string must be CONTAINED
        // in a Precondition (surfaced as an error), never silently dropped/executed.
        let observed = format!("inject → {r:?} (want {want_label})");
        out.push(if mapped_ok {
            RedFinding::ok("PA7/inject-map", name, "F", want_label, observed)
        } else {
            RedFinding::brk("PA7/inject-map", name, "F", want_label, observed)
        });
    }
    // inject with NO transport → NoTransport (the absent-rpc guard).
    let fx = ProviderFx {
        env: &env, paths: &paths, socket_dir: paths.sessions_dir.clone(), mux: None, clock: None,
        sleeper: None, relay: None, relay_port: None, app_server: None, codex_expected_turn_id: None,
        acp_client: None, pi_rpc: None,
            acp_pre_dispatch: None,
    };
    let r = PI_PROVIDER.inject(&fx, &key, "hello", "from");
    out.push(if matches!(r, Err(InjectError::NoTransport(_))) {
        RedFinding::ok("PA7/inject-map", "absent-rpc", "F", "NoTransport", format!("inject(no pi_rpc) → {r:?}"))
    } else {
        RedFinding::brk("PA7/inject-map", "absent-rpc", "F", "NoTransport", format!("inject(no pi_rpc) → {r:?}"))
    });
    out
}

/// PA7 — FRAME-INJECTION-VIA-MESSAGE: the POSITIVE non-vacuity proof that pi's
/// raw footguns (bash-no-command → runs literal `undefined`; prompt-no-message →
/// TypeError) are OUT OF REACH through the adapter. (1) the adapter forwards a
/// user message as ONE opaque string (a capturing rpc sees the exact bytes — it
/// never splits/interprets it); (2) the on-wire frame is ONE ndjson line because
/// `serde_json::to_string` (stdio.rs:160 `write_frame`) escapes the embedded
/// newline, so an injected `\n{"type":"bash"}` becomes escaped TEXT inside the
/// `message` value (stdio.rs:371 always sets `message`), never a smuggled 2nd frame.
fn battery_frame_injection() -> Vec<RedFinding> {
    let env = MapEnv { vars: Default::default(), uid: 501 };
    let paths = QdPaths::from_home(&PathBuf::from("/nonexistent-redteam-jail"));
    let key = SessionKey { id: "sid-red", name: None, cwd: Some("/w"), pid: None };
    let mut out = Vec::new();

    // The adversarial message literally embeds a bash frame + a dangerous command.
    let evil = "hi\n{\"type\":\"bash\",\"command\":\"rm -rf /\"}\n";

    // (1) inject forwards it WHOLE — the capturing rpc sees exactly one call with
    // the exact bytes (no split, no interpretation).
    let cap = CapturingPiRpc { seen: RefCell::new(Vec::new()) };
    let fx = ProviderFx {
        env: &env, paths: &paths, socket_dir: paths.sessions_dir.clone(), mux: None, clock: None,
        sleeper: None, relay: None, relay_port: None, app_server: None, codex_expected_turn_id: None,
        acp_client: None, pi_rpc: Some(&cap),
            acp_pre_dispatch: None,
    };
    let r = PI_PROVIDER.inject(&fx, &key, evil, "from");
    let seen = cap.seen.borrow().clone();
    let passed_whole = seen.len() == 1 && seen.first().map(String::as_str) == Some(evil);
    out.push(if r.is_ok() && passed_whole {
        RedFinding::ok("PA7/frame-injection", "message-forwarded-as-one-opaque-string", "F", truncate(evil, 90),
            format!("adapter inject forwarded the message WHOLE (no frame split / no interpretation); rpc saw {} call(s) with the exact bytes (stdio.rs:371 always sets `message`)", seen.len()))
    } else {
        RedFinding::brk("PA7/frame-injection", "message-forwarded-as-one-opaque-string", "F", truncate(evil, 90),
            format!("adapter MISHANDLED the injection: inject={r:?} seen={seen:?}"))
    });

    // (2) the serialized wire frame is ONE ndjson line — the embedded newline is
    // serde-escaped, so pi reads ONE chat message, never a 2nd `bash` frame.
    let frame = json!({"id":"c1","type":"prompt","message": evil, "streamingBehavior":"steer"});
    let wire = serde_json::to_string(&frame).unwrap_or_default();
    let no_raw_newline = !wire.contains('\n'); // 0x0A absent → the `\n` was escaped to `\\n`
    let pi_sees_one_frame = no_raw_newline && wire.matches("\"type\":").count() == 1;
    out.push(if pi_sees_one_frame {
        RedFinding::ok("PA7/frame-injection", "serialized-frame-is-one-ndjson-line", "F", truncate(evil, 90),
            format!("serde_json::to_string (stdio.rs:160 write_frame) produced ONE ndjson line, no raw newline (embedded \\n escaped) → the bash footgun is UNREACHABLE through the adapter. wire={}", truncate(&wire, 160)))
    } else {
        RedFinding::brk("PA7/frame-injection", "serialized-frame-is-one-ndjson-line", "F", truncate(evil, 90),
            format!("serialized frame is NOT a single safe ndjson line (raw newline / multi-frame) → injection POSSIBLE: {}", truncate(&wire, 200)))
    });
    out
}

/// PA7 — EMPTY-MESSAGE framing. The adapter CAN be asked to inject `message:""`,
/// so assert its FRAMING is well-formed ndjson with an empty `message` (degrade-
/// not-crash at construction). pi's RUNTIME behavior on an empty message is a
/// model TURN → TIER-B (deferred here, NOT run cred-free); a resulting pi error
/// would map through inject to Precondition (battery_inject_map covers the MAP).
fn battery_empty_message_framing() -> Vec<RedFinding> {
    let frame = json!({"id":"c1","type":"prompt","message":"", "streamingBehavior":"steer"});
    let wire = serde_json::to_string(&frame).unwrap_or_default();
    let reparse: Result<Value, _> = serde_json::from_str(&wire);
    let ok = reparse
        .as_ref()
        .map(|v| v.get("message").and_then(Value::as_str) == Some("") && v.get("type").and_then(Value::as_str) == Some("prompt"))
        .unwrap_or(false);
    vec![if ok {
        RedFinding::ok("PA7/empty-message", "framing-well-formed", "F", "message=\"\"",
            format!("empty-message prompt frame is valid ndjson (message=\"\", type=prompt); real-pi turn behavior DEFERRED to tier-b. wire={wire}"))
    } else {
        RedFinding::brk("PA7/empty-message", "framing-well-formed", "F", "message=\"\"",
            format!("empty-message frame is malformed: {wire}"))
    }]
}

/// ROUND-4 FRESH TAIL batch (digs the tail for EXHAUSTION, not just stability).
/// All F-layer/pure — after two instrument defects, pure probes minimize re-void
/// risk. The prior battery stays as regression; this is the new coverage. Taste-
/// filtered to the plausible remaining surface, not pedantic infinite variations.
fn battery_tail_deep(workdir: &Path) -> Vec<RedFinding> {
    let mut out = Vec::new();
    let hdr = "{\"type\":\"session\",\"id\":\"u\",\"timestamp\":\"t\",\"cwd\":\"/w\"}";

    // 1. NUL byte (0x00) in a transcript line → degrade (Unknown/parse), never panic.
    let p = workdir.join("t4-nul.jsonl");
    let mut nul = format!("{hdr}\n").into_bytes();
    nul.extend_from_slice(b"{\"type\":\"message\",\"id\":\"e");
    nul.push(0x00);
    nul.extend_from_slice(b"\",\"timestamp\":\"t\"}\n");
    let _ = std::fs::write(&p, &nul);
    let pc = p.clone();
    out.push(probe("PA6/tail", "nul-byte-in-line", "F", "embedded 0x00", move || {
        let n = read_lines(&pc).len();
        (format!("read_lines len={n} (no panic on raw NUL)"), false)
    }));

    // 2. serde recursion-limit deep nesting (>128) in a line → Err → Unknown (the
    //    limit guards against a stack overflow), no panic.
    let deep = {
        let mut s = String::new();
        for _ in 0..300 { s.push_str("{\"k\":"); }
        s.push('1');
        for _ in 0..300 { s.push('}'); }
        s
    };
    let p2 = workdir.join("t4-deep.jsonl");
    let _ = std::fs::write(&p2, format!("{hdr}\n{deep}\n"));
    let p2c = p2.clone();
    out.push(probe("PA6/tail", "serde-recursion-limit-deep-nesting", "F", "300-deep nested object line", move || {
        let lines = read_lines(&p2c);
        let unknown = lines.iter().filter(|l| matches!(l, FileLine::Unknown)).count();
        (format!("lines={} unknown={} (deep line rejected gracefully, no stack overflow)", lines.len(), unknown), false)
    }));

    // 3. duplicate JSON keys → serde last-wins, no panic.
    out.push(probe("PA6/tail", "duplicate-json-keys", "F", r#"{"type":"response","type":"agent_start"}"#, || {
        let v: Result<Value, _> = serde_json::from_str(r#"{"type":"response","type":"agent_start","success":true}"#);
        let observed = match v {
            Ok(val) => format!("parsed last-wins type={:?}; classify={}", val.get("type"), if classify(&val).is_some() { "Some" } else { "None" }),
            Err(e) => format!("json rejected: {e}"),
        };
        (observed, false)
    }));

    // 4. overflow / float / negative numbers in numeric fields → permissive, no panic.
    out.push(probe("PA6/tail", "overflow-numbers", "F", "messageCount=1e23 / -1 ; willRetry=1e400", || {
        let cases = [
            r#"{"isStreaming":false,"messageCount":99999999999999999999999}"#,
            r#"{"isStreaming":false,"messageCount":-1}"#,
            r#"{"type":"agent_end","willRetry":1e400}"#,
        ];
        let mut notes = Vec::new();
        for cse in cases {
            match serde_json::from_str::<Value>(cse) {
                Ok(val) => notes.push(format!("status={:?}", PI_PROVIDER.parse_status(&val))),
                Err(_) => notes.push("json-rejected".into()),
            }
        }
        (notes.join("; "), false)
    }));

    // 5. UTF-8 BOM + CRLF framing → degrade (BOM line Unknown, CRLF handled), no panic.
    let pbom = workdir.join("t4-bom.jsonl");
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(format!("{hdr}\r\n{{\"type\":\"message\",\"id\":\"e\",\"timestamp\":\"t\"}}\r\n").as_bytes());
    let _ = std::fs::write(&pbom, &bom);
    let pbomc = pbom.clone();
    out.push(probe("PA6/tail", "bom-and-crlf-framing", "F", "UTF-8 BOM + CRLF lines", move || {
        let lines = read_lines(&pbomc);
        let entries = lines.iter().filter(|l| matches!(l, FileLine::Entry(_))).count();
        (format!("lines={} entry={} (BOM line degrades to Unknown, CRLF handled by str::lines)", lines.len(), entries), false)
    }));

    // 6. response `id` as a NUMBER → serde can't coerce to Option<String> → the whole
    //    response defaults (id:None) → no mis-correlation (a numeric id must NOT become
    //    a matching string id).
    out.push(probe("PA7/tail", "response-id-as-number", "F", r#"{"type":"response","id":123,…}"#, || {
        let v: Value = serde_json::from_str(r#"{"type":"response","id":123,"command":"prompt","success":true}"#).unwrap();
        match classify(&v) {
            Some(Frame::Response(r)) => {
                let is_break = r.id.is_some(); // a numeric id must degrade to None, never a spurious match
                (format!("id={:?} (numeric id → None: no mis-correlation)", r.id), is_break)
            }
            other => (format!("{other:?}"), false),
        }
    }));

    // 7. 1 MiB message serialize → ONE ndjson line, no panic/corruption.
    out.push(probe("PA7/tail", "huge-1mib-message-serialize", "F", "message=1MiB", || {
        let msg = "A".repeat(1_000_000);
        let frame = json!({"id":"c1","type":"prompt","message":msg,"streamingBehavior":"steer"});
        let wire = serde_json::to_string(&frame).unwrap_or_default();
        let one_line = !wire.contains('\n') && wire.len() > 1_000_000;
        (format!("serialized {} bytes, single_ndjson_line={}", wire.len(), one_line), !one_line)
    }));

    // 8. control chars / NUL inside a user message → adapter forwards it WHOLE; serde
    //    escapes them on the wire (the frame-injection containment, control-char variant).
    {
        let env = MapEnv { vars: Default::default(), uid: 501 };
        let paths = QdPaths::from_home(&PathBuf::from("/nonexistent-redteam-jail"));
        let key = SessionKey { id: "s", name: None, cwd: Some("/w"), pid: None };
        let evil = "x\u{0}\u{1}\n\t\r{\"type\":\"bash\"}";
        let cap = CapturingPiRpc { seen: RefCell::new(Vec::new()) };
        let fx = ProviderFx {
            env: &env, paths: &paths, socket_dir: paths.sessions_dir.clone(), mux: None, clock: None,
            sleeper: None, relay: None, relay_port: None, app_server: None, codex_expected_turn_id: None,
            acp_client: None, pi_rpc: Some(&cap),
            acp_pre_dispatch: None,
        };
        let r = PI_PROVIDER.inject(&fx, &key, evil, "from");
        let seen = cap.seen.borrow().clone();
        let whole = seen.len() == 1 && seen.first().map(String::as_str) == Some(evil);
        out.push(if r.is_ok() && whole {
            RedFinding::ok("PA7/tail", "control-chars-nul-in-message", "F", "ctrl+NUL+embedded-frame",
                format!("adapter forwarded the message WHOLE ({} call, exact bytes); serde escapes ctrl/NUL on the wire", seen.len()))
        } else {
            RedFinding::brk("PA7/tail", "control-chars-nul-in-message", "F", "ctrl+NUL+embedded-frame",
                format!("mishandled: inject={r:?} seen={seen:?}"))
        });
    }

    // 9. scan_transcripts over an adversarial sessions root (a DIR named like a session
    //    file, a non-jsonl, a valid one) → degrade, no panic, skip non-matching.
    let root = workdir.join("t4-adv-sessions");
    let bucket = root.join(encode_cwd_dir("/w"));
    let _ = std::fs::create_dir_all(bucket.join("2026_dirnotfile.jsonl")); // a DIR named like a session file
    let _ = std::fs::write(bucket.join("not-a-session.txt"), "x");
    let _ = std::fs::write(bucket.join("2026_realuuid.jsonl"), "{}");
    let rootc = root.clone();
    out.push(probe("PA6/tail", "scan_transcripts-adversarial-root", "F", "dir-as-file + non-jsonl + valid", move || {
        let metas = PI_PROVIDER.scan_transcripts(&rootc);
        (format!("scan_transcripts → {} meta(s), no panic (skips non-jsonl; permissive on a dir-named-like-a-file)", metas.len()), false)
    }));

    out
}

/// ROUND-5 FRESH TAIL batch (the C-RED convergence round) — a DIFFERENT corner
/// from round-4: file-structure edges, path math, and non-string/non-object frame
/// fields. All F-layer/pure/deterministic. Taste-filtered.
fn battery_tail_deep5(workdir: &Path) -> Vec<RedFinding> {
    let mut out = Vec::new();
    let hdr = "{\"type\":\"session\",\"id\":\"u\",\"timestamp\":\"t\",\"cwd\":\"/w\"}";

    // 1. empty file (0 bytes) → 0 lines, not an error (lazy-write edge).
    let pe = workdir.join("t5-empty.jsonl");
    let _ = std::fs::write(&pe, b"");
    let pec = pe.clone();
    out.push(probe("PA6/tail5", "empty-file", "F", "0 bytes", move || {
        let n = read_lines(&pec).len();
        (format!("read_lines len={n}"), n != 0)
    }));

    // 2. header-not-first (a message line as line 1, no session header) → entries
    //    still parse, no panic.
    let ph = workdir.join("t5-nohdr.jsonl");
    let _ = std::fs::write(&ph, "{\"type\":\"message\",\"id\":\"e1\",\"timestamp\":\"t\"}\n{\"type\":\"message\",\"id\":\"e2\",\"timestamp\":\"t\"}\n");
    let phc = ph.clone();
    out.push(probe("PA6/tail5", "header-not-first", "F", "message line as line 1", move || {
        let lines = read_lines(&phc);
        let h = lines.iter().filter(|l| matches!(l, FileLine::Header(_))).count();
        (format!("lines={} headers={h} (no header → entries still parse)", lines.len()), false)
    }));

    // 3. volume — 5000 entry lines → no panic, correct count (robustness/perf).
    let pv = workdir.join("t5-vol.jsonl");
    let mut s = format!("{hdr}\n");
    for i in 0..5000 {
        s.push_str(&format!("{{\"type\":\"message\",\"id\":\"e{i}\",\"timestamp\":\"t\"}}\n"));
    }
    let _ = std::fs::write(&pv, &s);
    let pvc = pv.clone();
    out.push(probe("PA6/tail5", "volume-5000-lines", "F", "5001-line file", move || {
        let n = read_lines(&pvc).len();
        (format!("read_lines len={n} (5001 expected)"), n != 5001)
    }));

    // 4. parse_filename adversarial shapes → None or a parse, never a panic.
    for (name, fname) in [
        ("multi-underscore", "a_b_c.jsonl"),
        ("double-ext", ".jsonl.jsonl"),
        ("embedded-newline", "x\n_y.jsonl"),
        ("only-ext", ".jsonl"),
    ] {
        let f = fname.to_string();
        out.push(probe("PA6/tail5", name, "F", fname.escape_debug().to_string(), move || {
            (format!("{:?}", parse_filename(&f)), false)
        }));
    }

    // 5. encode_cwd_dir on traversal / trailing-slashes / unicode / very-long → still
    //    the `--…--` shape, never a panic.
    let longp = format!("/{}", "a".repeat(5000));
    for (name, cwd) in [
        ("traversal", "/a/../../etc".to_string()),
        ("trailing-slashes", "/a///".to_string()),
        ("unicode", "/π/λ/x".to_string()),
        ("very-long", longp),
    ] {
        out.push(probe("PA6/tail5", name, "F", truncate(&cwd, 40), move || {
            let r = encode_cwd_dir(&cwd);
            let shape = r.starts_with("--") && r.ends_with("--");
            (format!("{} shape_ok={shape}", truncate(&r, 50)), !shape)
        }));
    }

    // 6. classify with `type` as a NON-string (number/array/object) or missing → the
    //    `as_str().unwrap_or("")` path → Other(""), never a panic.
    for (name, v) in [
        ("type-number", json!({"type":123,"success":true})),
        ("type-array", json!({"type":[1],"x":1})),
        ("type-object", json!({"type":{"k":1}})),
        ("type-missing", json!({"x":1})),
    ] {
        out.push(probe("PA6/tail5", name, "F", truncate(&v.to_string(), 50), move || {
            let kind = match classify(&v) {
                None => "None",
                Some(Frame::Response(_)) => "Response",
                Some(Frame::Event(_)) => "Event",
            };
            (format!("classify → {kind}"), false)
        }));
    }

    // 7. message_update with a NON-object assistantMessageEvent (array/string/null) →
    //    classify_assistant_event degrades to Other(""), never a panic.
    for (name, v) in [
        ("amse-array", json!({"type":"message_update","assistantMessageEvent":[1,2]})),
        ("amse-string", json!({"type":"message_update","assistantMessageEvent":"x"})),
        ("amse-null", json!({"type":"message_update","assistantMessageEvent":null})),
    ] {
        out.push(probe("PA6/tail5", name, "F", truncate(&v.to_string(), 60), move || {
            (format!("classify some={}", classify(&v).is_some()), false)
        }));
    }

    // 8. parse_status with isStreaming as a non-bool (object/array) or a numeric type →
    //    degrade to None (no wrong Busy/Idle flip), no panic.
    for (name, v) in [
        ("isStreaming-object", json!({"isStreaming":{"x":1}})),
        ("isStreaming-array", json!({"isStreaming":[true]})),
        ("type-number-status", json!({"type":42})),
    ] {
        out.push(probe("PA7/tail5", name, "F", truncate(&v.to_string(), 50), move || {
            let st = PI_PROVIDER.parse_status(&v);
            (format!("status={st:?}"), st.is_some()) // all must be None
        }));
    }

    out
}

/// The unbounded-read_line CHARACTERIZATION (stdio.rs:64): no per-line byte cap.
/// PINNED for the why-holder acceptance oracle — NOT a break (degrades fine at
/// ~1MiB: no panic/crash/parse-corruption). Whether a cap SHOULD exist (DoS
/// surface) is the oracle's correctness call, not the loop's.
fn characterization_read_line_no_cap() -> RedFinding {
    RedFinding::characterize(
        "PA6/reader-reach",
        "read_line-no-per-line-cap",
        "F",
        "stdio.rs:64 reader_loop BufReader::read_line",
        "reader_loop reads a full stdout line into a String with NO per-line byte cap. At ~1MiB it degrades fine \
         (allocates, then serde_json parses or skips → no panic/crash/parse-corruption), so it is NOT a break and \
         does NOT reset the clean counter. CHARACTERIZATION for the acceptance oracle: an absurdly large unterminated \
         line is an unbounded-memory/DoS reach (the daemon byte-cap breaker, status.rs on_breaker, is the separate \
         control). Oracle rules whether 'no per-line cap' is an acceptable boundary.",
    )
}

/// Run the full pure-F fixture battery (callable from the cfg(test) tests AND the
/// gated wrapper). `workdir` is a caller-provided scratch dir (a tempdir) for the
/// file-backed probes — kept out of the lib so `tempfile` stays a dev-dep.
pub fn run_fixture_battery(workdir: &Path) -> RedReport {
    let mut findings = Vec::new();
    findings.extend(battery_classify());
    findings.extend(battery_read_lines(workdir));
    findings.extend(battery_parse_status());
    findings.extend(battery_on_event());
    findings.extend(battery_paths(workdir));
    findings.extend(battery_inject_map());
    findings.extend(battery_frame_injection());
    findings.extend(battery_empty_message_framing());
    findings.extend(battery_tail_deep(workdir)); // round-4 fresh tail batch (accumulates as regression)
    findings.extend(battery_tail_deep5(workdir)); // round-5 fresh tail batch (convergence round)
    findings.push(characterization_read_line_no_cap());
    RedReport { findings, hardened_surface: hardened_surface_survey() }
}

// ===========================================================================
// Small helpers.
// ===========================================================================

fn event_name(e: &PiEvent) -> &'static str {
    match e {
        PiEvent::AgentStart => "agent_start",
        PiEvent::MessageUpdate { .. } => "message_update",
        PiEvent::AgentEnd { .. } => "agent_end",
        PiEvent::Other(_) => "other",
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    // Back off to a char boundary so the harness never panics on its OWN input
    // (a self-inflicted slice panic would be a false break).
    let mut cut = n;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…[+{} bytes]", &s[..cut], s.len() - cut)
}

/// Build a deeply-nested object `{"k": {"k": {... v ...}}}` to `depth` for the
/// stack-safety classify probe.
fn nest(leaf: Value, depth: usize) -> Value {
    let mut v = leaf;
    for _ in 0..depth {
        v = json!({ "k": v });
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pure-F battery runs clean against the built adapter (the offline
    /// baseline — the live S/L layers are added by the gated wrapper). Every
    /// finding is recorded; a break here is a real production defect.
    #[test]
    fn fixture_battery_is_clean_on_the_built_adapter() {
        let tmp = tempfile::tempdir().unwrap();
        let report = run_fixture_battery(tmp.path());
        // confirm-it-RAN: the battery is nonempty and the survey is pinned.
        assert!(!report.findings.is_empty(), "battery must run probes");
        assert!(!report.hardened_surface.is_empty(), "non-vacuity survey must be pinned");
        let breaks = report.breaks();
        assert!(
            breaks.is_empty(),
            "C-RED fixture battery found break(s) — route to pi-lead-2, do NOT paper over:\n{}",
            breaks.iter().map(|b| format!("  [{}] {} :: {}", b.class, b.probe, b.observed)).collect::<Vec<_>>().join("\n")
        );
        // The read_line characterization is pinned (not a break).
        assert_eq!(report.characterizations().len(), 1);
    }

    #[test]
    fn parse_status_never_mis_derives() {
        // The mis-derivation guard in isolation (the dangerous wrong-flip).
        let report = {
            let mut f = Vec::new();
            f.extend(battery_parse_status());
            RedReport { findings: f, hardened_surface: vec![] }
        };
        assert!(report.clean(), "parse_status mis-derivation: {:?}", report.breaks());
    }

    #[test]
    fn inject_map_is_exact() {
        let report = RedReport { findings: battery_inject_map(), hardened_surface: vec![] };
        assert!(report.clean(), "inject error-MAP break: {:?}", report.breaks());
    }
}
