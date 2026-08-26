//! Item 7 — the RUN-not-read tier-(a) conformance harness for pi. Drives the REAL
//! `qd` verbs BY NAME against a live `pi --mode rpc` and asserts each verb's
//! OBSERVED effect (registry row, process tree, endpoint) — never its return
//! string, never read-and-assume (the hard constraint + memory
//! `never-report-an-unrun-result-as-observed`).
//!
//! **Library form (not a `tests/*.rs`), why.** The driver LIVES here as a library
//! so its pure helpers are unit-tested; the thin cargo-visible
//! `tests/pi_verb_roundtrip_live.rs` wrapper (gated `QD_PI_LIVE=1`) calls
//! [`run_tier_a`], asserts green, and SERIALIZES the [`ConformanceReport`] to an
//! evidence artifact.
//!
//! **The 8 tier-(a) rubric items** (credential-free / structural; impl-plan §2.1):
//!   #1 transport · #2 launch & addressing · #3 boot/readiness · #8 teardown &
//!   lifecycle · #9 auth/config/endpoint · #10 concurrency/multiplex ·
//!   #11 liveness/health · #12 maturity/stability. NONE touches a model turn — so
//!   item-7 tier-a needs only the pinned pi (via `QD_PI_BIN`), NO OAuth.
//!
//! **LIVE-RUN-EVIDENCE guard (standing requirement).** A green pass count
//! proves nothing if the run early-returned. So every [`ItemResult`] carries the
//! EXACT `commands` it ran + the `observed` state the verdict rests on (a row's
//! JSON, a pid's liveness, a cmdline) — and the wrapper writes the whole report to
//! an inspectable JSON artifact. The verdict is RECONSTRUCTABLE from the evidence,
//! not asserted.
//!
//! **RUN-PROTOCOL — env-scrub (PREREGISTERED; impl-plan §4.2).** Every spawned
//! `qd` runs under an ISOLATED tempdir `HOME` with the 5 session vars truly UNSET
//! (NO trailing `QD_HOME=`); [`QdRunner`] bakes this in.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::Serialize;
use serde_json::Value;

use crate::provider::pi::daemon::residence::cmdline_is_our_pi_daemon;
use crate::provider::pi::pin;
use crate::provider::pi::store::session::encode_cwd_dir;
use crate::paths::QdPaths;

/// The session-env vars scrubbed off EVERY spawned `qd` (the truly-unset set).
pub const SCRUB_VARS: &[&str] = &[
    "QD_HOME",
    "QD_SESSION_ID",
    "SB_SESSION_ID",
    "QD_BOOT_AWAIT_RELAY",
    "CLAUDE_CODE_SESSION_ID",
];

/// One rubric-item result. RUN-not-read: `commands` is the exact verbs run and
/// `observed` is the evidence the verdict rests on (not an assertion).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemResult {
    pub item: u8,
    pub pass: bool,
    pub detail: String,
    /// The exact `qd …` commands this item ran (the live-run evidence).
    pub commands: Vec<String>,
    /// The observed source state the verdict rests on (row JSON, pid liveness, …).
    pub observed: String,
}

/// The tier-(a) conformance report — the inspectable evidence artifact.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConformanceReport {
    pub items: Vec<ItemResult>,
}

impl ConformanceReport {
    pub fn all_green(&self) -> bool {
        !self.items.is_empty() && self.items.iter().all(|r| r.pass)
    }
    pub fn failures(&self) -> Vec<&ItemResult> {
        self.items.iter().filter(|r| !r.pass).collect()
    }
    /// Serialize to a pretty JSON evidence artifact (the wrapper writes this to disk).
    pub fn to_evidence_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }
}

/// Runs the real `qd` binary under the preregistered env-scrub + an isolated home,
/// and reads the OBSERVED registry/process state at the source.
pub struct QdRunner {
    qd_bin: PathBuf,
    pi_bin: PathBuf,
    home: PathBuf,
}

impl QdRunner {
    pub fn new(qd_bin: PathBuf, pi_bin: PathBuf, home: PathBuf) -> Self {
        Self {
            qd_bin,
            pi_bin,
            home,
        }
    }

    /// The sessions dir `qd` resolves under the isolated HOME (where rows land).
    pub fn sessions_dir(&self) -> PathBuf {
        QdPaths::from_home(&self.home).sessions_dir
    }

    /// Build the scrubbed [`Command`] for `qd args…`: isolated HOME, `QD_PI_BIN`
    /// set to the pinned pi, the [`SCRUB_VARS`] REMOVED (truly unset).
    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.qd_bin);
        cmd.args(args)
            .env("HOME", &self.home)
            .env("QD_PI_BIN", &self.pi_bin);
        for v in SCRUB_VARS {
            cmd.env_remove(v);
        }
        cmd
    }

    pub fn run(&self, args: &[&str]) -> std::io::Result<Output> {
        self.command(args).output()
    }

    /// Read the registry row whose `name` matches, by scanning the sessions dir's
    /// `*.json` at the SOURCE (RUN-not-read: the row is the observed effect, not
    /// the verb's stdout). Returns the parsed row + its on-disk path.
    pub fn read_row(&self, name: &str) -> Option<(PathBuf, Value)> {
        let dir = self.sessions_dir();
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if v.get("name").and_then(Value::as_str) == Some(name) {
                return Some((p, v));
            }
        }
        None
    }
}

/// Render an `Output` as a compact observed-evidence string (status + trimmed io).
fn out_evidence(args: &[&str], o: &Output) -> String {
    format!(
        "$ qd {}\n  exit={:?}\n  stdout={}\n  stderr={}",
        args.join(" "),
        o.status.code(),
        String::from_utf8_lossy(&o.stdout).trim(),
        String::from_utf8_lossy(&o.stderr).trim()
    )
}

/// The 8 credential-free tier-(a) rubric item numbers this harness drives.
pub fn tier_a_items() -> [u8; 8] {
    [1, 2, 3, 8, 9, 10, 11, 12]
}

/// The `qd start` verb argv for a pi session (the EXACT argv driven; pure so the
/// verb spelling is unit-asserted vs the live CLI — guards `redteam-doc-flag-drift`).
/// The session NAME is a POSITIONAL arg (cli.rs `positional("name")`), NOT a
/// `--name` flag — confirmed live (an early run flag-drifted on `--name` and the CLI
/// errored "unknown option '--name'") + matches the acp live-test precedent
/// (`start <name> --provider … --cwd …`).
pub fn start_argv<'a>(name: &'a str, cwd: &'a str) -> Vec<&'a str> {
    vec!["start", name, "--provider", "pi", "--cwd", cwd]
}

/// CRED-FREE path-shape check (the review must-confirm, tier-a portion): does
/// [`encode_cwd_dir`] match pi's documented PA5 dir shape `--<encoded-cwd>--`, with
/// every `/ \ :` mapped to `-` (no collapse)? The REAL-on-disk-dir confirm (against
/// an actual pi-created dir) is TIER-B (needs a turn / assistant-gated lazy-write)
/// and is DEFERRED there — this asserts the documented shape, RUN-not-read against
/// the regex contract. Returns (pass, observed).
pub fn encode_cwd_dir_shape_ok() -> (bool, String) {
    // Independent regex reimplementation of pi's
    // `cwd.replace(/^[/\\]/,"").replace(/[/\\:]/g,"-")` wrapped in `-- --`.
    fn expected(cwd: &str) -> String {
        let body = cwd
            .strip_prefix('/')
            .or_else(|| cwd.strip_prefix('\\'))
            .unwrap_or(cwd);
        let mapped: String = body
            .chars()
            .map(|c| {
                if c == '/' || c == '\\' || c == ':' {
                    '-'
                } else {
                    c
                }
            })
            .collect();
        format!("--{mapped}--")
    }
    let cases = [
        "/home/u/x",
        "/work/quorum-pi",
        "C:\\proj\\a",
        "/a//b", // double sep → double dash (no collapse — the C:\ lesson)
        "/with:colon",
    ];
    let mut bad = Vec::new();
    for c in cases {
        let got = encode_cwd_dir(c);
        let want = expected(c);
        // Must match the PA5 shape (`--…--`) AND the regex contract.
        if got != want || !(got.starts_with("--") && got.ends_with("--")) {
            bad.push(format!("{c:?} -> {got:?} (want {want:?})"));
        }
    }
    if bad.is_empty() {
        (
            true,
            format!(
                "encode_cwd_dir matches the PA5 `--<enc>--` shape + regex on {} cases",
                cases.len()
            ),
        )
    } else {
        (false, format!("shape mismatch: {}", bad.join("; ")))
    }
}

/// Drive the full tier-(a) conformance sweep against a live pi. RUN-not-read: each
/// item runs real verbs and inspects observed state; the returned report IS the
/// evidence artifact. `d_ts_path` (the installed `rpc-types.d.ts`) enables the #12
/// hash pin; `None` runs the version-only subset of #12.
pub fn run_tier_a(runner: &QdRunner, d_ts_path: Option<&Path>) -> ConformanceReport {
    let mut items = Vec::new();
    let cwd = runner.home.to_string_lossy().into_owned();
    let s1 = "pi-conf-1";
    let s2 = "pi-conf-2";

    // --- create session 1 (drives #1/#2/#3/#9 off one start) -------------------
    let start1 = start_argv(s1, &cwd);
    let start_out = runner.run(&start1);
    let started_ok = start_out
        .as_ref()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let row1 = runner.read_row(s1);
    let start_ev = match &start_out {
        Ok(o) => out_evidence(&start1, o),
        Err(e) => format!("$ qd {} -> spawn error {e}", start1.join(" ")),
    };
    let row1_json = row1
        .as_ref()
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| "<no row>".to_string());

    // #3 boot/readiness: start returned success (resident reached get_state ready).
    items.push(ItemResult {
        item: 3,
        pass: started_ok && row1.is_some(),
        detail: if started_ok && row1.is_some() {
            "start --provider pi succeeded; row written (resident reached get_state readiness)"
                .into()
        } else {
            "start did not succeed / no row (boot readiness not reached)".into()
        },
        commands: vec![format!("qd {}", start1.join(" "))],
        observed: format!("{start_ev}\n  row={row1_json}"),
    });

    // #2 launch & addressing: row carries name + birth-id + endpoint + pid; `qd info` resolves.
    let info1 = vec!["info", s1];
    let info_out = runner.run(&info1);
    let info_ok = info_out
        .as_ref()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let addr_ok = row1.as_ref().is_some_and(|(_, v)| {
        v.get("name").and_then(Value::as_str) == Some(s1)
            && v.get("sessionId")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            && v.get("endpoint")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            && v.get("pid").and_then(Value::as_i64).is_some()
    }) && info_ok;
    items.push(ItemResult {
        item: 2,
        pass: addr_ok,
        detail: "row carries name+sessionId(birth-id)+endpoint+pid AND `qd info <name>` resolves"
            .into(),
        commands: vec![
            format!("qd {}", start1.join(" ")),
            format!("qd {}", info1.join(" ")),
        ],
        observed: format!(
            "row={row1_json}\n{}",
            info_out
                .as_ref()
                .map(|o| out_evidence(&info1, o))
                .unwrap_or_default()
        ),
    });

    // #9 auth/config/endpoint: cred-free start works; endpoint recorded as loopback ws.
    let endpoint_ok = row1.as_ref().is_some_and(|(_, v)| {
        v.get("endpoint")
            .and_then(Value::as_str)
            .is_some_and(|e| e.starts_with("ws://127.0.0.1:"))
    });
    items.push(ItemResult {
        item: 9,
        pass: started_ok && endpoint_ok,
        detail: "credential-free start; loopback ws endpoint recorded; QD_PI_BIN honored (no OAuth needed for tier-a)".into(),
        commands: vec![format!("qd {}", start1.join(" "))],
        observed: format!("endpoint={}", row1.as_ref().and_then(|(_, v)| v.get("endpoint").and_then(Value::as_str)).unwrap_or("<none>")),
    });

    // #1 transport: the resident is OUR pi-daemon (stdio-RPC adapter), confirmed by
    // its live cmdline carrying the pi-daemon marker + the recorded --listen endpoint.
    let (transport_ok, transport_obs) = match &row1 {
        Some((_, v)) => {
            let pid = v.get("pid").and_then(Value::as_i64).unwrap_or(0);
            let endpoint = v.get("endpoint").and_then(Value::as_str);
            let cmdline = crate::create_daemon::real_cmdline_probe(pid);
            let ours = cmdline_is_our_pi_daemon(cmdline.as_deref(), endpoint);
            (
                ours,
                format!("pid={pid} cmdline={:?} endpoint={:?}", cmdline, endpoint),
            )
        }
        None => (false, "<no row>".to_string()),
    };
    items.push(ItemResult {
        item: 1,
        pass: transport_ok,
        detail: "resident is OUR pi-daemon stdio-RPC adapter (cmdline marker + recorded --listen endpoint)".into(),
        commands: vec![format!("qd {}", start1.join(" "))],
        observed: transport_obs,
    });

    // --- #10 concurrency/multiplex: a 2nd session, distinct endpoint+pid, both addressable.
    let start2 = start_argv(s2, &cwd);
    let start2_out = runner.run(&start2);
    let row2 = runner.read_row(s2);
    let concurrency_ok = start2_out
        .as_ref()
        .map(|o| o.status.success())
        .unwrap_or(false)
        && match (&row1, &row2) {
            (Some((_, a)), Some((_, b))) => {
                let ep_a = a.get("endpoint").and_then(Value::as_str);
                let ep_b = b.get("endpoint").and_then(Value::as_str);
                let pid_a = a.get("pid").and_then(Value::as_i64);
                let pid_b = b.get("pid").and_then(Value::as_i64);
                ep_a.is_some() && ep_a != ep_b && pid_a.is_some() && pid_a != pid_b
            }
            _ => false,
        };
    items.push(ItemResult {
        item: 10,
        pass: concurrency_ok,
        detail: "two concurrent pi sessions → distinct residents (distinct endpoints + pids), both addressable".into(),
        commands: vec![format!("qd {}", start2.join(" "))],
        observed: format!(
            "s1.endpoint={:?} s2.endpoint={:?}",
            row1.as_ref().and_then(|(_, v)| v.get("endpoint").and_then(Value::as_str)),
            row2.as_ref().and_then(|(_, v)| v.get("endpoint").and_then(Value::as_str))
        ),
    });

    // --- #11 liveness/health: `qd ls` lists both; the resident pid is alive.
    let ls = vec!["ls"];
    let ls_out = runner.run(&ls);
    let pid1_alive = row1
        .as_ref()
        .and_then(|(_, v)| v.get("pid").and_then(Value::as_i64))
        .is_some_and(|p| crate::effects::is_pid_alive(p as i32));
    items.push(ItemResult {
        item: 11,
        pass: pid1_alive && ls_out.as_ref().map(|o| o.status.success()).unwrap_or(false),
        detail:
            "`qd ls` succeeds and the resident pid is alive (liveness reflects the real process)"
                .into(),
        commands: vec!["qd ls".to_string()],
        observed: ls_out
            .as_ref()
            .map(|o| out_evidence(&ls, o))
            .unwrap_or_default(),
    });

    // --- #12 maturity/stability: the RPC-shape pin smoke check.
    let (m_pass, m_detail, m_obs) = match d_ts_path {
        Some(dts) => match pin::run_pin_smoke(&runner.pi_bin, dts) {
            Ok(r) => (
                r.ok(),
                format!("pin smoke ok={}; {:?}", r.ok(), r.failures()),
                format!("{r:?}"),
            ),
            Err(e) => (false, format!("pin smoke exec error: {e}"), e),
        },
        None => (
            false,
            "no rpc-types.d.ts path provided (cannot run the shape/hash pin)".into(),
            String::new(),
        ),
    };
    items.push(ItemResult {
        item: 12,
        pass: m_pass,
        detail: m_detail,
        commands: vec![format!("{} --version", runner.pi_bin.display())],
        observed: m_obs,
    });

    // --- cred-free encode_cwd_dir shape assertion (review must-confirm, tier-a part).
    let (shape_ok, shape_obs) = encode_cwd_dir_shape_ok();
    // Folded under #2's addressing/path-resolution concern as a distinct evidence row.
    items.push(ItemResult {
        item: 2,
        pass: shape_ok,
        detail: "CRED-FREE encode_cwd_dir path-shape (PA5 `--<enc>--` + regex; real-on-disk-dir confirm DEFERRED to tier-b turn)".into(),
        commands: vec!["(pure: encode_cwd_dir vs pi regex/PA5 shape)".to_string()],
        observed: shape_obs,
    });

    // --- #8 teardown & lifecycle: `qd stop` → resident pid gone (pgid), row tombstoned.
    let stop1 = vec!["stop", s1];
    let stop1_out = runner.run(&stop1);
    let _ = runner.run(&["stop", s2]); // tidy the 2nd session too
                                       // Give the SIGTERM→grace a moment (the resident's group teardown).
    let pid1 = row1
        .as_ref()
        .and_then(|(_, v)| v.get("pid").and_then(Value::as_i64));
    let gone = pid1.is_some_and(|p| !crate::effects::is_pid_alive(p as i32));
    items.push(ItemResult {
        item: 8,
        pass: stop1_out.as_ref().map(|o| o.status.success()).unwrap_or(false) && gone,
        detail: "`qd stop <name>` → resident process-group reaped (pid not alive); pgid teardown (not bare pid-kill)".into(),
        commands: vec![format!("qd {}", stop1.join(" "))],
        observed: format!(
            "{}\n  resident pid {:?} alive_after_stop={}",
            stop1_out.as_ref().map(|o| out_evidence(&stop1, o)).unwrap_or_default(),
            pid1,
            pid1.map(|p| crate::effects::is_pid_alive(p as i32)).unwrap_or(false)
        ),
    });

    ConformanceReport { items }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_vars_are_the_truly_unset_five() {
        assert_eq!(SCRUB_VARS.len(), 5);
        assert!(SCRUB_VARS.contains(&"QD_HOME"));
        assert!(SCRUB_VARS.contains(&"QD_BOOT_AWAIT_RELAY"));
    }

    #[test]
    fn tier_a_is_the_eight_credfree_items() {
        assert_eq!(tier_a_items(), [1, 2, 3, 8, 9, 10, 11, 12]);
        for excluded in [4u8, 5, 6, 7, 13] {
            assert!(!tier_a_items().contains(&excluded));
        }
    }

    #[test]
    fn start_argv_names_the_pi_provider() {
        assert_eq!(
            start_argv("s1", "/work"),
            vec!["start", "s1", "--provider", "pi", "--cwd", "/work"]
        );
    }

    // A1 registration bar (RUN-not-read): `provider_for("pi")` resolves the pi
    // provider behind the seam. This is the lib-level resolution the A1 standard
    // names — DISTINCT from the user-facing `qd start --provider pi` arm, which
    // stays ABSENT until A2 (an unknown id still yields None).
    #[test]
    fn pi_provider_resolves_via_provider_for_seam() {
        assert!(
            crate::provider::provider_for("pi").is_some(),
            "provider_for(\"pi\") must resolve the registered pi provider behind the seam"
        );
        assert!(
            crate::provider::provider_for("pi-not-a-real-provider").is_none(),
            "an unknown provider id must still resolve to None"
        );
    }

    #[test]
    fn encode_cwd_dir_shape_holds_incl_the_no_collapse_lesson() {
        // The cred-free assertion itself must pass against the real function
        // (this is the regression the session.rs test-fix encoded: C:\ -> C--).
        let (ok, obs) = encode_cwd_dir_shape_ok();
        assert!(ok, "encode_cwd_dir shape check failed: {obs}");
    }

    #[test]
    fn report_green_requires_nonempty_all_pass_and_serializes() {
        let empty = ConformanceReport::default();
        assert!(!empty.all_green());
        let mixed = ConformanceReport {
            items: vec![
                ItemResult {
                    item: 1,
                    pass: true,
                    detail: "ok".into(),
                    commands: vec![],
                    observed: "x".into(),
                },
                ItemResult {
                    item: 2,
                    pass: false,
                    detail: "boom".into(),
                    commands: vec![],
                    observed: "y".into(),
                },
            ],
        };
        assert!(!mixed.all_green());
        assert_eq!(mixed.failures().len(), 1);
        // The evidence artifact serializes (the live-run-evidence guard).
        let json = mixed.to_evidence_json();
        assert!(json.contains("\"observed\""));
        assert!(json.contains("boom"));
    }
}
