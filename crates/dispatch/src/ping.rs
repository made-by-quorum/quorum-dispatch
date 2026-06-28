//! `qd ping` — generic session-liveness classifier (Phase G), ported from
//! `0d0fa9e:src/commands/ping.ts`. Absorbs the legacy `monitor.sh` health-checker
//! + its multi-session sweep into the engine.
//!
//! DESIGN (mirrors the TS): the verdict is a PURE classifier [`classify_health`]
//! — plain numbers in, {classification, exit code, lines} out, NO I/O. The
//! command wrapper ([`run_health_single`] / [`run_health_prefix`]) does the
//! engine-native resolution + sweep over an injected `&[Session]`, so the
//! classification and the resolution are independently unit-testable with no
//! subprocess.
//!
//! EXIT-CODE CONTRACT (FROZEN — drop-in for monitor.sh; callers depend on these,
//! ping.ts:16-20). Ping owns exit band 0–4 (ADR 0008; monitor.sh contract is
//! FROZEN — byte-level care):
//!   0 = idle/done   — session completed or not running (healthy)
//!   1 = stuck       — shell hung past threshold (safe to kill)
//!   2 = active      — working normally (keep waiting)
//!   3 = error       — ambiguous name (multiple matches) or lookup error
//!   4 = ambiguous   — may be done or stuck; check deliverables before killing

use crate::model::{Session, SessionStatus};
use crate::resolve::{resolve_session, Resolution};

// --- thresholds (seconds) — identical to monitor.sh (ping.ts:31-33). ---

/// Shell with no status change > 5 min → stuck.
const SHELL_STUCK_S: i64 = 300;
/// idle/cold with 0 turns after > 5 min → ambiguous.
const COLD_ZERO_TURN_S: i64 = 300;
/// busy with 0 turns after > 10 min → ambiguous.
const BUSY_ZERO_TURN_S: i64 = 600;

/// The classification (`ping.ts Classification`, :38-43).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    Done,
    Stuck,
    Active,
    Error,
    Ambiguous,
}

impl Classification {
    /// The FROZEN exit code (`ping.ts EXIT`, :45-51).
    pub fn exit_code(self) -> i32 {
        match self {
            Classification::Done => 0,
            Classification::Stuck => 1,
            Classification::Active => 2,
            Classification::Error => 3,
            Classification::Ambiguous => 4,
        }
    }

    /// The lowercase tag used on the `--json` surface (`classification` field).
    pub fn as_str(self) -> &'static str {
        match self {
            Classification::Done => "done",
            Classification::Stuck => "stuck",
            Classification::Active => "active",
            Classification::Error => "error",
            Classification::Ambiguous => "ambiguous",
        }
    }
}

/// Live session status for the classifier (`ping.ts HealthInput.status`, :54-55).
/// Adds the synthetic `Missing` (no live session — not found / dead) on top of
/// the registry's [`SessionStatus`]; the classifier treats it as done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Idle,
    Busy,
    Shell,
    Cold,
    Killed,
    Missing,
}

impl HealthStatus {
    fn as_str(self) -> &'static str {
        match self {
            HealthStatus::Idle => "idle",
            HealthStatus::Busy => "busy",
            HealthStatus::Shell => "shell",
            HealthStatus::Cold => "cold",
            HealthStatus::Killed => "killed",
            HealthStatus::Missing => "missing",
        }
    }

    fn from_session_status(s: SessionStatus) -> Self {
        match s {
            SessionStatus::Idle => HealthStatus::Idle,
            SessionStatus::Busy => HealthStatus::Busy,
            SessionStatus::Shell => HealthStatus::Shell,
            SessionStatus::Cold => HealthStatus::Cold,
            SessionStatus::Killed => HealthStatus::Killed,
        }
    }
}

/// The classifier inputs (`ping.ts HealthInput`, :53-62).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthInput {
    pub status: HealthStatus,
    /// Completed turns (jsonl).
    pub turns: i64,
    /// Seconds since last activity (now − lastActive).
    pub age_seconds: i64,
    /// Seconds since session start (now − startedAt).
    pub uptime_seconds: i64,
}

/// The verdict (`ping.ts HealthVerdict`, :64-69).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthVerdict {
    pub classification: Classification,
    pub exit_code: i32,
    /// Human annotation lines (the status line is added by the caller).
    pub lines: Vec<String>,
}

fn verdict(classification: Classification, lines: Vec<String>) -> HealthVerdict {
    HealthVerdict {
        classification,
        exit_code: classification.exit_code(),
        lines,
    }
}

/// The PURE classifier (`ping.ts classifyHealth`, :71-117). Reproduces
/// monitor.sh's pre-checks + python block branch order EXACTLY (FIRST MATCH
/// WINS). The contract above is FROZEN — do not reorder.
pub fn classify_health(input: HealthInput) -> HealthVerdict {
    let HealthInput {
        status,
        turns,
        age_seconds,
        uptime_seconds,
    } = input;

    // monitor.sh: no live session / no PID → cold/done.
    if status == HealthStatus::Missing {
        return verdict(Classification::Done, vec![]);
    }

    // monitor.sh python: status in (idle, cold) → done, UNLESS 0 turns + old.
    if status == HealthStatus::Idle || status == HealthStatus::Cold {
        if turns == 0 && uptime_seconds > COLD_ZERO_TURN_S {
            return verdict(
                Classification::Ambiguous,
                vec![
                    "AMBIGUOUS: session cold with 0 completed turns — may have finished in one turn or never started".to_string(),
                    "ACTION: check for deliverable files before killing".to_string(),
                ],
            );
        }
        return verdict(Classification::Done, vec![]);
    }

    // killed → treat as done (not running; nothing to wait for).
    if status == HealthStatus::Killed {
        return verdict(Classification::Done, vec![]);
    }

    // monitor.sh python: status == shell && age > 300 → stuck.
    if status == HealthStatus::Shell && age_seconds > SHELL_STUCK_S {
        return verdict(
            Classification::Stuck,
            vec!["STUCK: shell for >5 min with no status change".to_string()],
        );
    }

    // monitor.sh python: turns == 0 && status == busy && uptime > 600 → ambiguous.
    if status == HealthStatus::Busy && turns == 0 && uptime_seconds > BUSY_ZERO_TURN_S {
        return verdict(
            Classification::Ambiguous,
            vec![
                "AMBIGUOUS: busy with 0 completed turns after >10 min — may be doing work in one long turn or hung".to_string(),
                "ACTION: check token growth between ticks; check for deliverable files".to_string(),
            ],
        );
    }

    // monitor.sh python: otherwise → active, keep waiting.
    verdict(Classification::Active, vec![])
}

/// Derive a [`HealthInput`] from a resolved [`Session`] (`ping.ts
/// inputFromSession`, :144-154). `now_ms` is the clock read (injected). Ages
/// clamp at 0 (a future timestamp never yields a negative age).
pub fn input_from_session(s: &Session, now_ms: i64) -> HealthInput {
    let last_ms = s.last_active_ms.unwrap_or(now_ms);
    let start_ms = s.started_at_ms.unwrap_or(now_ms);
    HealthInput {
        status: HealthStatus::from_session_status(s.status),
        turns: s.turns as i64,
        age_seconds: ((now_ms - last_ms) / 1000).max(0),
        uptime_seconds: ((now_ms - start_ms) / 1000).max(0),
    }
}

fn status_line(name: &str, input: &HealthInput) -> String {
    format!(
        "{name}: status={}  age={}s  turns={}  uptime={}s",
        input.status.as_str(),
        input.age_seconds,
        input.turns,
        input.uptime_seconds
    )
}

/// The display name TS uses: `name || sessionId` (`ping.ts`, repeated).
fn display_name(s: &Session) -> &str {
    match s.name.as_deref() {
        Some(n) if !n.is_empty() => n,
        _ => s.session_id.as_str(),
    }
}

/// One result of a ping run (`ping.ts HealthRunResult`, :233-237).
#[derive(Debug, PartialEq, Eq)]
pub struct HealthRunResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

// --- JSON shaping (ping.ts jsonEntry, :171-185 + the ambiguous-name shape). ---

fn json_entry(name: &str, input: &HealthInput, v: &HealthVerdict) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "status": input.status.as_str(),
        "ageSeconds": input.age_seconds,
        "turns": input.turns,
        "uptimeSeconds": input.uptime_seconds,
        "classification": v.classification.as_str(),
        "exitCode": v.exit_code,
    })
}

/// Single-session run (`ping.ts runHealthSingle`, :245-292). Resolve the query
/// against the session list, classify the hit:
///   None  (no match)      → done (exit 0), monitor.sh "cold (session not found)".
///   Many  (>1 live match) → error (exit 3), ambiguous NAME (distinct from
///                           ambiguous HEALTH = 4).
///   One                   → classify it.
pub fn run_health_single(
    query: &str,
    sessions: &[Session],
    now_ms: i64,
    json: bool,
) -> HealthRunResult {
    match resolve_session(query, sessions) {
        Resolution::None => {
            let input = HealthInput {
                status: HealthStatus::Missing,
                turns: 0,
                age_seconds: 0,
                uptime_seconds: 0,
            };
            let v = classify_health(input);
            if json {
                return HealthRunResult {
                    exit_code: v.exit_code,
                    stdout: json_entry(query, &input, &v).to_string() + "\n",
                    stderr: String::new(),
                };
            }
            HealthRunResult {
                exit_code: v.exit_code,
                stdout: format!("{query}: status=cold  (session not found or dead)\n"),
                stderr: String::new(),
            }
        }
        Resolution::Many(matches) => {
            // Ambiguous NAME → exit 3. monitor.sh emitted this as status=error.
            let names: Vec<String> = matches
                .iter()
                .map(|s| display_name(s).to_string())
                .collect();
            let exit = Classification::Error.exit_code();
            if json {
                return HealthRunResult {
                    exit_code: exit,
                    stdout: serde_json::json!({
                        "name": query,
                        "classification": "error",
                        "exitCode": exit,
                        "matches": names,
                    })
                    .to_string()
                        + "\n",
                    stderr: String::new(),
                };
            }
            HealthRunResult {
                exit_code: exit,
                stdout: format!(
                    "{query}: status=error  (ambiguous name — {} sessions match: {})\n",
                    matches.len(),
                    names.join(", ")
                ),
                stderr: String::new(),
            }
        }
        Resolution::One(s) => {
            let input = input_from_session(s, now_ms);
            let v = classify_health(input);
            let name = display_name(s);
            if json {
                return HealthRunResult {
                    exit_code: v.exit_code,
                    stdout: json_entry(name, &input, &v).to_string() + "\n",
                    stderr: String::new(),
                };
            }
            let mut out_lines = vec![status_line(name, &input)];
            out_lines.extend(v.lines.iter().cloned());
            HealthRunResult {
                exit_code: v.exit_code,
                stdout: out_lines.join("\n") + "\n",
                stderr: String::new(),
            }
        }
    }
}

/// Prefix sweep (`ping.ts runHealthPrefix`, :297-355). Classify EVERY session
/// whose name starts with the prefix; one line per session. Aggregate exit: 1 if
/// any stuck; else 4 if any ambiguous; else 0 (all healthy: done/active). NO
/// matches is NOT an error — it is a healthy empty sweep, exit 0.
pub fn run_health_prefix(
    prefix: &str,
    sessions: &[Session],
    now_ms: i64,
    json: bool,
) -> HealthRunResult {
    let matched: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.name.as_deref().is_some_and(|n| n.starts_with(prefix)))
        .collect();

    if matched.is_empty() {
        if json {
            return HealthRunResult {
                exit_code: Classification::Done.exit_code(),
                stdout: "[]\n".to_string(),
                stderr: String::new(),
            };
        }
        return HealthRunResult {
            exit_code: Classification::Done.exit_code(),
            stdout: format!("No sessions matching '{prefix}'\n"),
            stderr: String::new(),
        };
    }

    struct Entry {
        name: String,
        input: HealthInput,
        v: HealthVerdict,
    }
    let entries: Vec<Entry> = matched
        .iter()
        .map(|s| {
            let input = input_from_session(s, now_ms);
            let v = classify_health(input);
            Entry {
                name: display_name(s).to_string(),
                input,
                v,
            }
        })
        .collect();

    // Aggregate worst classification → exit. stuck (1) > ambiguous (4) > rest.
    let any_stuck = entries
        .iter()
        .any(|e| e.v.classification == Classification::Stuck);
    let any_ambiguous = entries
        .iter()
        .any(|e| e.v.classification == Classification::Ambiguous);
    let agg_exit = if any_stuck {
        Classification::Stuck.exit_code()
    } else if any_ambiguous {
        Classification::Ambiguous.exit_code()
    } else {
        Classification::Done.exit_code()
    };

    if json {
        let arr: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| json_entry(&e.name, &e.input, &e.v))
            .collect();
        return HealthRunResult {
            exit_code: agg_exit,
            stdout: serde_json::Value::Array(arr).to_string() + "\n",
            stderr: String::new(),
        };
    }

    let mut lines: Vec<String> = Vec::new();
    for e in &entries {
        lines.push(format!("  {}", status_line(&e.name, &e.input)));
        for l in &e.v.lines {
            lines.push(format!("    {l}"));
        }
    }
    if any_stuck || any_ambiguous {
        lines.push(String::new());
        lines.push("--- ALERTS ---".to_string());
        for e in &entries {
            if e.v.classification == Classification::Stuck {
                lines.push(format!(
                    "ALERT: {} stuck ({}s)",
                    e.name, e.input.age_seconds
                ));
            }
            if e.v.classification == Classification::Ambiguous {
                lines.push(format!("ALERT: {} ambiguous — check deliverables", e.name));
            }
        }
    } else {
        lines.push(String::new());
        lines.push("All sessions healthy.".to_string());
    }
    HealthRunResult {
        exit_code: agg_exit,
        stdout: lines.join("\n") + "\n",
        stderr: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SessionBranch;

    fn input(status: HealthStatus, turns: i64, age: i64, uptime: i64) -> HealthInput {
        HealthInput {
            status,
            turns,
            age_seconds: age,
            uptime_seconds: uptime,
        }
    }

    // ============================================================
    // classify_health: every branch (G-P1 unit matrix).
    // ============================================================

    #[test]
    fn missing_is_done() {
        let v = classify_health(input(HealthStatus::Missing, 0, 0, 99999));
        assert_eq!(v.classification, Classification::Done);
        assert_eq!(v.exit_code, 0);
    }

    #[test]
    fn idle_with_turns_is_done() {
        let v = classify_health(input(HealthStatus::Idle, 3, 0, 99999));
        assert_eq!(v.classification, Classification::Done);
    }

    #[test]
    fn cold_with_turns_is_done() {
        let v = classify_health(input(HealthStatus::Cold, 1, 0, 99999));
        assert_eq!(v.classification, Classification::Done);
    }

    #[test]
    fn idle_zero_turns_old_is_ambiguous() {
        let v = classify_health(input(HealthStatus::Idle, 0, 0, COLD_ZERO_TURN_S + 1));
        assert_eq!(v.classification, Classification::Ambiguous);
        assert_eq!(v.exit_code, 4);
        assert!(v.lines[0].contains("cold with 0 completed turns"));
    }

    #[test]
    fn cold_zero_turns_old_is_ambiguous() {
        let v = classify_health(input(HealthStatus::Cold, 0, 0, COLD_ZERO_TURN_S + 1));
        assert_eq!(v.classification, Classification::Ambiguous);
    }

    #[test]
    fn killed_is_done() {
        // killed reaches the killed branch (not idle/cold), → done.
        let v = classify_health(input(HealthStatus::Killed, 0, 99999, 99999));
        assert_eq!(v.classification, Classification::Done);
    }

    #[test]
    fn shell_old_is_stuck() {
        let v = classify_health(input(HealthStatus::Shell, 0, SHELL_STUCK_S + 1, 0));
        assert_eq!(v.classification, Classification::Stuck);
        assert_eq!(v.exit_code, 1);
        assert!(v.lines[0].contains("STUCK"));
    }

    #[test]
    fn busy_zero_turns_old_is_ambiguous() {
        let v = classify_health(input(HealthStatus::Busy, 0, 0, BUSY_ZERO_TURN_S + 1));
        assert_eq!(v.classification, Classification::Ambiguous);
        assert!(v.lines[0].contains("busy with 0 completed turns"));
    }

    #[test]
    fn busy_with_turns_is_active() {
        let v = classify_health(input(HealthStatus::Busy, 5, 0, 99999));
        assert_eq!(v.classification, Classification::Active);
        assert_eq!(v.exit_code, 2);
    }

    #[test]
    fn shell_young_is_active() {
        let v = classify_health(input(HealthStatus::Shell, 0, SHELL_STUCK_S, 0));
        assert_eq!(v.classification, Classification::Active);
    }

    // ============================================================
    // Threshold boundaries (off-by-one negative controls, G-N1 credit).
    // The contract uses STRICT `>` — at the threshold EXACTLY it is NOT yet
    // ambiguous/stuck.
    // ============================================================

    #[test]
    fn cold_zero_turns_at_threshold_is_done_not_ambiguous() {
        // uptime == COLD_ZERO_TURN_S (300) is NOT > 300 → done (boundary).
        let v = classify_health(input(HealthStatus::Cold, 0, 0, COLD_ZERO_TURN_S));
        assert_eq!(
            v.classification,
            Classification::Done,
            "at the 300s boundary it must still be done (strict >)"
        );
    }

    #[test]
    fn cold_zero_turns_one_past_threshold_is_ambiguous() {
        let v = classify_health(input(HealthStatus::Cold, 0, 0, COLD_ZERO_TURN_S + 1));
        assert_eq!(v.classification, Classification::Ambiguous);
    }

    #[test]
    fn shell_at_threshold_is_active_not_stuck() {
        // age == SHELL_STUCK_S (300) is NOT > 300 → active (boundary).
        let v = classify_health(input(HealthStatus::Shell, 0, SHELL_STUCK_S, 0));
        assert_eq!(v.classification, Classification::Active);
    }

    #[test]
    fn shell_one_past_threshold_is_stuck() {
        let v = classify_health(input(HealthStatus::Shell, 0, SHELL_STUCK_S + 1, 0));
        assert_eq!(v.classification, Classification::Stuck);
    }

    #[test]
    fn busy_at_threshold_is_active_not_ambiguous() {
        // uptime == BUSY_ZERO_TURN_S (600) is NOT > 600 → active (boundary).
        let v = classify_health(input(HealthStatus::Busy, 0, 0, BUSY_ZERO_TURN_S));
        assert_eq!(v.classification, Classification::Active);
    }

    #[test]
    fn busy_one_past_threshold_is_ambiguous() {
        let v = classify_health(input(HealthStatus::Busy, 0, 0, BUSY_ZERO_TURN_S + 1));
        assert_eq!(v.classification, Classification::Ambiguous);
    }

    // ============================================================
    // input_from_session: clock math + clamping.
    // ============================================================

    fn sess(name: &str, status: SessionStatus) -> Session {
        Session {
            name: Some(name.to_string()),
            user_named: None,
            session_id: format!("sid-{name}"),
            code: None,
            qd_id: None,
            pid: None,
            status,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "claude-code".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: SessionBranch::LiveRegistry,
        }
    }

    #[test]
    fn input_from_session_computes_age_and_uptime() {
        let mut s = sess("w", SessionStatus::Busy);
        s.turns = 4;
        let now = 1_000_000;
        s.last_active_ms = Some(now - 30_000); // 30s ago
        s.started_at_ms = Some(now - 600_000); // 600s ago
        let i = input_from_session(&s, now);
        assert_eq!(i.status, HealthStatus::Busy);
        assert_eq!(i.turns, 4);
        assert_eq!(i.age_seconds, 30);
        assert_eq!(i.uptime_seconds, 600);
    }

    #[test]
    fn input_from_session_clamps_future_timestamps_to_zero() {
        let mut s = sess("w", SessionStatus::Idle);
        let now = 1_000;
        s.last_active_ms = Some(now + 50_000); // in the future
        s.started_at_ms = Some(now + 50_000);
        let i = input_from_session(&s, now);
        assert_eq!(i.age_seconds, 0);
        assert_eq!(i.uptime_seconds, 0);
    }

    #[test]
    fn input_from_session_missing_timestamps_default_to_now() {
        let s = sess("w", SessionStatus::Idle);
        let now = 9_999_999;
        let i = input_from_session(&s, now);
        // last/start default to now → age/uptime 0.
        assert_eq!(i.age_seconds, 0);
        assert_eq!(i.uptime_seconds, 0);
    }

    // ============================================================
    // run_health_single: resolution → classification.
    // ============================================================

    #[test]
    fn single_not_found_is_done_cold_line() {
        let r = run_health_single("ghost", &[], 0, false);
        assert_eq!(r.exit_code, 0);
        assert!(r
            .stdout
            .contains("ghost: status=cold  (session not found or dead)"));
    }

    #[test]
    fn single_not_found_json() {
        let r = run_health_single("ghost", &[], 0, true);
        assert_eq!(r.exit_code, 0);
        let v: serde_json::Value = serde_json::from_str(r.stdout.trim()).unwrap();
        assert_eq!(v["name"], "ghost");
        assert_eq!(v["status"], "missing");
        assert_eq!(v["classification"], "done");
    }

    #[test]
    fn single_one_match_classifies() {
        let mut s = sess("worker", SessionStatus::Busy);
        s.turns = 2;
        let now = 1_000_000;
        s.started_at_ms = Some(now - 5_000);
        s.last_active_ms = Some(now - 1_000);
        let r = run_health_single("worker", std::slice::from_ref(&s), now, false);
        assert_eq!(r.exit_code, 2); // active
        assert!(r.stdout.contains("worker: status=busy"));
    }

    #[test]
    fn single_ambiguous_name_is_error_exit_3() {
        let mut a = sess("dup", SessionStatus::Idle);
        a.session_id = "sid-a".to_string();
        let mut b = sess("dup", SessionStatus::Busy);
        b.session_id = "sid-b".to_string();
        let sessions = vec![a, b];
        let r = run_health_single("dup", &sessions, 0, false);
        assert_eq!(r.exit_code, 3);
        assert!(r
            .stdout
            .contains("status=error  (ambiguous name — 2 sessions match"));
    }

    #[test]
    fn single_ambiguous_name_json_lists_matches() {
        let mut a = sess("dup", SessionStatus::Idle);
        a.session_id = "sid-a".to_string();
        let mut b = sess("dup", SessionStatus::Busy);
        b.session_id = "sid-b".to_string();
        let r = run_health_single("dup", &[a, b], 0, true);
        let v: serde_json::Value = serde_json::from_str(r.stdout.trim()).unwrap();
        assert_eq!(v["classification"], "error");
        assert_eq!(v["exitCode"], 3);
        assert_eq!(v["matches"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn single_classified_json_shape() {
        let mut s = sess("w", SessionStatus::Idle);
        s.turns = 0;
        let now = 1_000_000;
        s.started_at_ms = Some(now - 400_000); // 400s → cold-zero-turn ambiguous
        s.last_active_ms = Some(now);
        let r = run_health_single("w", std::slice::from_ref(&s), now, true);
        let v: serde_json::Value = serde_json::from_str(r.stdout.trim()).unwrap();
        assert_eq!(v["name"], "w");
        assert_eq!(v["classification"], "ambiguous");
        assert_eq!(v["exitCode"], 4);
        assert_eq!(v["turns"], 0);
        assert_eq!(v["uptimeSeconds"], 400);
    }

    // ============================================================
    // run_health_prefix: sweep aggregation (G-P1).
    // ============================================================

    fn aged(
        name: &str,
        status: SessionStatus,
        turns: u64,
        age_s: i64,
        uptime_s: i64,
        now: i64,
    ) -> Session {
        let mut s = sess(name, status);
        s.turns = turns;
        s.last_active_ms = Some(now - age_s * 1000);
        s.started_at_ms = Some(now - uptime_s * 1000);
        s
    }

    #[test]
    fn prefix_empty_sweep_is_done_exit_0() {
        let r = run_health_prefix("none", &[], 0, false);
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("No sessions matching 'none'"));
    }

    #[test]
    fn prefix_empty_sweep_json_is_empty_array() {
        let r = run_health_prefix("none", &[], 0, true);
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.stdout, "[]\n");
    }

    #[test]
    fn prefix_all_healthy_exit_0() {
        let now = 1_000_000;
        let s1 = aged("wk-a", SessionStatus::Busy, 3, 1, 10, now); // active
        let s2 = aged("wk-b", SessionStatus::Idle, 1, 1, 10, now); // done
        let r = run_health_prefix("wk-", &[s1, s2], now, false);
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("All sessions healthy."));
        assert!(!r.stdout.contains("--- ALERTS ---"));
    }

    #[test]
    fn prefix_any_ambiguous_exit_4() {
        let now = 1_000_000;
        let s1 = aged("wk-a", SessionStatus::Busy, 3, 1, 10, now); // active
        let s2 = aged("wk-b", SessionStatus::Cold, 0, 1, 400, now); // ambiguous
        let r = run_health_prefix("wk-", &[s1, s2], now, false);
        assert_eq!(r.exit_code, 4);
        assert!(r.stdout.contains("--- ALERTS ---"));
        assert!(r.stdout.contains("ALERT: wk-b ambiguous"));
    }

    #[test]
    fn prefix_any_stuck_wins_over_ambiguous_exit_1() {
        let now = 1_000_000;
        let stuck = aged("wk-a", SessionStatus::Shell, 0, 400, 400, now); // stuck
        let amb = aged("wk-b", SessionStatus::Cold, 0, 1, 400, now); // ambiguous
        let r = run_health_prefix("wk-", &[stuck, amb], now, false);
        // stuck (1) dominates ambiguous (4).
        assert_eq!(r.exit_code, 1);
        assert!(r.stdout.contains("ALERT: wk-a stuck"));
        assert!(r.stdout.contains("ALERT: wk-b ambiguous"));
    }

    #[test]
    fn prefix_json_array_shape() {
        let now = 1_000_000;
        let s1 = aged("wk-a", SessionStatus::Busy, 3, 1, 10, now);
        let r = run_health_prefix("wk-", std::slice::from_ref(&s1), now, true);
        let v: serde_json::Value = serde_json::from_str(r.stdout.trim()).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "wk-a");
        assert_eq!(arr[0]["classification"], "active");
    }

    #[test]
    fn prefix_filters_by_name_prefix_only() {
        let now = 1_000_000;
        let inc = aged("wk-a", SessionStatus::Busy, 3, 1, 10, now);
        let exc = aged("other", SessionStatus::Busy, 3, 1, 10, now);
        let r = run_health_prefix("wk-", &[inc, exc], now, true);
        let v: serde_json::Value = serde_json::from_str(r.stdout.trim()).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }
}
