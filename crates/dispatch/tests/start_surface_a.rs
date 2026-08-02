//! Lifecycle-collapse workstream A — the `qd start` surface (spec §3).
//!
//! Integration rows for A-1 (`--json` identity output), A-2 (exit 0 ⇒ id
//! bound; the ruled bind-failure arms — the arm LOGIC is unit-tested in
//! `dispatch::bindphase`, this file exercises the end-to-end fixtures the
//! acceptance names), A-3 (`--no-await-relay` beats the env alias), and A-5
//! (name validation at the start boundary — the guard that used to live in
//! `frame commission`).
//!
//! Jail / driver helpers MIRROR ack2_gate.rs (duplicated — test binaries
//! cannot import each other; the ack3-spec §2 duplication sanction).
//!
//! The `#[ignore]` rows are the acceptance-evidence runs (the seeded
//! NoneBindable fixture takes the full boot-phase budget; the cold-start
//! smoke loops 50 boots): run them explicitly with `-- --ignored` and keep
//! the output as evidence.

use std::path::{Path, PathBuf};
use std::process::Command;

// ===========================================================================
// Binary locators (duplicated from ack2_gate)
// ===========================================================================

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

fn profile_dir() -> PathBuf {
    std::env::current_exe()
        .expect("current_exe")
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>")
        .to_path_buf()
}

fn qrmux_bin() -> PathBuf {
    let bin = profile_dir().join("qrmux");
    assert!(
        bin.exists(),
        "qrmux binary not found at {bin:?} — build it first: \
         scripts/build-lock.sh cargo build -p qrmux --bin qrmux"
    );
    bin
}

fn fakerepl_bin() -> PathBuf {
    let bin = profile_dir().join("fakerepl");
    assert!(
        bin.exists(),
        "fakerepl binary not found at {bin:?} — build it first: \
         cargo build -p fakerepl"
    );
    bin
}

fn require_bins() {
    let _ = qrmux_bin();
    let _ = fakerepl_bin();
}

// ===========================================================================
// Jail (duplicated from ack2_gate, trimmed)
// ===========================================================================

struct Jail {
    root: PathBuf,
    home: PathBuf,
    xdg: PathBuf,
    qd_home: PathBuf,
    uuid: String,
    created: std::cell::RefCell<Vec<String>>,
}

impl Jail {
    fn establish(tag: &str) -> Jail {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = PathBuf::from("/tmp/qd-startA");
        let root = base.join("qdrg-runs").join(format!("{tag}-{nanos}"));
        let home = root.join("home");
        let xdg = base.join(format!("x-{tag}-{nanos}"));
        let qd_home = root.join("qd_home");
        let sessions = home.join(".claude").join("sessions");
        let projects = home.join(".claude").join("projects").join("proj");
        for d in [
            &sessions,
            &projects,
            &xdg,
            &qd_home,
            &root.join("tmp"),
            &root.join("zmx"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o700)).ok();
        let uuid = "11111111-2222-3333-4444-666666666666".to_string();
        Jail {
            root,
            home,
            xdg,
            qd_home,
            uuid,
            created: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn fakerepl_env<'a>(&'a self, name: &'a str) -> Vec<(&'a str, String)> {
        vec![
            ("QD_FAKEREPL_NAME", name.to_string()),
            ("QD_FAKEREPL_SESSION_ID", self.uuid.clone()),
        ]
    }

    fn sessions_dir(&self) -> PathBuf {
        self.home.join(".claude").join("sessions")
    }

    fn teardown(&self) {
        let names: Vec<String> = self.created.borrow().clone();
        for name in names {
            let _ = run_qd(self, &["stop", "--force", &name], &[]);
        }
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.xdg);
    }
}

fn build_cmd(jail: &Jail, args: &[&str], extra: &[(&str, String)]) -> Command {
    let fr = fakerepl_bin();
    let mut cmd = Command::new(qd_bin());
    cmd.args(args);
    cmd.env_clear()
        // Default jail posture: relay wait OFF via the transition alias (these
        // hermetic boots never write a relay sidecar). Individual rows that
        // test the flag/env precedence override this via `extra`.
        .env("QD_BOOT_AWAIT_RELAY", "0")
        .env("HOME", &jail.home)
        .env("QD_HOME", &jail.qd_home)
        .env("XDG_RUNTIME_DIR", &jail.xdg)
        .env("TMPDIR", jail.root.join("tmp"))
        .env("ZMX_DIR", jail.root.join("zmx"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", fr.parent().unwrap().display()),
        )
        .env("TERM", "xterm-256color")
        .env("CLAUDE_BIN", fr.to_string_lossy().into_owned());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd
}

fn run_qd(jail: &Jail, args: &[&str], extra: &[(&str, String)]) -> (i32, String, String) {
    if args.first() == Some(&"start") {
        if let Some(name) = args.iter().find(|a| !a.starts_with('-') && **a != "start") {
            jail.created.borrow_mut().push((*name).to_string());
        }
    }
    let out = build_cmd(jail, args, extra).output().expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Parse the single JSON object a `--json` invocation leaves on stdout.
fn parse_json_stdout(out: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(out.trim())
        .unwrap_or_else(|e| panic!("stdout is not one JSON object ({e}): {out:?}"))
}

// ===========================================================================
// A-5 — name validation at the start boundary (moved from frame commission)
// ===========================================================================

/// Empty name: rejected before any FS/env op, exit 1. (S2's message shape.)
#[test]
fn a5_empty_name_rejected() {
    require_bins();
    let jail = Jail::establish("a5e");
    let (code, _out, err) = run_qd(&jail, &["start", "--interactive", ""], &[]);
    assert_eq!(code, 1, "empty name exits 1");
    assert!(
        err.contains("must not be empty"),
        "teaches the empty-name rule: {err:?}"
    );
    assert!(
        std::fs::read_dir(jail.sessions_dir()).unwrap().next().is_none(),
        "nothing created"
    );
    jail.teardown();
}

/// Colon-bearing name (the frame ref grammar `<name>:<id>` splits on `:`):
/// rejected at the same boundary, exit 1 — the guard `frame commission`
/// carried at commission.rs:49-59 lives HERE now (spec A-5; S2 enforces it).
#[test]
fn a5_colon_name_rejected() {
    require_bins();
    let jail = Jail::establish("a5c");
    let (code, _out, err) = run_qd(&jail, &["start", "--interactive", "a:b"], &[]);
    assert_eq!(code, 1, "colon name exits 1");
    assert!(
        err.contains("unsafe characters"),
        "S2 whitelist message: {err:?}"
    );
    assert!(
        std::fs::read_dir(jail.sessions_dir()).unwrap().next().is_none(),
        "nothing created"
    );
    jail.teardown();
}

/// A-5 + A-1 compose: a pre-create rejection under --json still leaves one
/// machine object on stdout (catch-all class "start-failed").
#[test]
fn a5_rejection_under_json_emits_error_object() {
    require_bins();
    let jail = Jail::establish("a5j");
    let (code, out, _err) = run_qd(&jail, &["start", "--interactive", "--json", "a:b"], &[]);
    assert_eq!(code, 1);
    let v = parse_json_stdout(&out);
    assert_eq!(v["error"]["class"], "start-failed");
    assert_eq!(v["error"]["session"]["name"], "a:b");
    jail.teardown();
}

// ===========================================================================
// A-1 + A-2 — --json identity, exit 0 ⇒ bound
// ===========================================================================

/// The happy path: `start --json` exits 0 and stdout is EXACTLY one JSON
/// object {name, qdId, sessionId, status, live} whose sessionId equals the
/// registry row's (the independent read), with the human line on stderr.
#[test]
fn a1_json_identity_bound_on_success() {
    require_bins();
    let jail = Jail::establish("a1j");
    let name = "a1j-s";
    let env = jail.fakerepl_env(name);
    let (code, out, err) = run_qd(&jail, &["start", "--interactive", "--json", name], &env);
    assert_eq!(code, 0, "start --json exits 0 (stderr: {err:?})");
    let v = parse_json_stdout(&out);
    assert_eq!(v["name"], name);
    assert_eq!(v["sessionId"], jail.uuid.as_str(), "bound to the row's uuid");
    let qd_id = v["qdId"].as_str().expect("qdId string");
    assert_eq!(qd_id.len(), 8, "stable id is 8 chars");
    assert_eq!(v["live"], true);
    // Human line moved to stderr under --json; stdout carries ONLY the object.
    assert!(
        err.contains("Started detached session"),
        "human line on stderr: {err:?}"
    );
    assert_eq!(
        out.trim().lines().count(),
        1,
        "stdout is exactly one JSON line: {out:?}"
    );
    // Independent registry read: the row this jail booted carries the SAME
    // sessionId the JSON printed (exit 0 ⇒ bound was checked in-process by
    // the bind phase; this is the outside witness).
    let rows = dispatch::registry::read_entries(&jail.sessions_dir(), false);
    let row = rows
        .iter()
        .find(|s| s.entry.name.as_deref() == Some(name))
        .expect("registry row exists");
    assert_eq!(row.entry.session_id.as_deref(), Some(jail.uuid.as_str()));
    jail.teardown();
}

/// Human output without --json is byte-shaped as before (stdout line).
#[test]
fn a1_human_output_unchanged_without_json() {
    require_bins();
    let jail = Jail::establish("a1h");
    let name = "a1h-s";
    let env = jail.fakerepl_env(name);
    let (code, out, _err) = run_qd(&jail, &["start", "--interactive", name], &env);
    assert_eq!(code, 0);
    assert!(
        out.contains(&format!("Started detached session \"{name}\"")),
        "human stdout line: {out:?}"
    );
    jail.teardown();
}

// ===========================================================================
// A-3 — relay-wait precedence: flag > env alias > default-on
// ===========================================================================

/// `--no-await-relay` beats an explicit env opt-IN: with QD_BOOT_AWAIT_RELAY=1
/// (which would hang this sidecar-less boot to the 60s budget) the flag still
/// yields a fast exit 0.
#[test]
fn a3_no_await_relay_flag_beats_env_optin() {
    require_bins();
    let jail = Jail::establish("a3f");
    let name = "a3f-s";
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_BOOT_AWAIT_RELAY", "1".to_string()));
    let start = std::time::Instant::now();
    let (code, _out, err) = run_qd(
        &jail,
        &["start", "--interactive", "--no-await-relay", name],
        &env,
    );
    let elapsed = start.elapsed();
    assert_eq!(code, 0, "flag wins over env opt-in (stderr: {err:?})");
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "no relay-budget burn: {elapsed:?}"
    );
    jail.teardown();
}

// ===========================================================================
// A-2 acceptance evidence rows (#[ignore] — run explicitly, keep the output)
// ===========================================================================

/// The seeded NoneBindable fixture (spec §3 acceptance, N1): fakerepl told to
/// NEVER write a sessionId → the bind phase retries to the boot-phase budget
/// (~40s) then fails: exit 1, class "unbound" on stdout, session left RUNNING
/// (named on stderr per the I6 posture).
#[test]
#[ignore = "acceptance evidence: burns the full ~40s bind budget by design"]
fn a2_seeded_nonebindable_exits_1_class_unbound() {
    require_bins();
    let jail = Jail::establish("a2u");
    let name = "a2u-s";
    let env = vec![
        ("QD_FAKEREPL_NAME", name.to_string()),
        ("QD_FAKEREPL_OMIT_SESSION_ID", "1".to_string()),
    ];
    let start = std::time::Instant::now();
    let (code, out, err) = run_qd(&jail, &["start", "--interactive", "--json", name], &env);
    let elapsed = start.elapsed();
    assert_eq!(code, 1, "unbound exits 1 (out: {out:?} err: {err:?})");
    let v = parse_json_stdout(&out);
    assert_eq!(v["error"]["class"], "unbound");
    assert_eq!(v["error"]["session"]["name"], name);
    assert_eq!(v["error"]["ids"]["env"].as_str().map(str::len), Some(8));
    assert!(
        err.contains("IS RUNNING"),
        "I6 posture: says exactly what was left: {err:?}"
    );
    // Bounded-but-nonzero latency, as the acceptance notes: the budget is the
    // boot-phase knob (40s default), not the overall 60s.
    assert!(
        elapsed >= std::time::Duration::from_secs(35),
        "ran the budget: {elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(70),
        "bounded: {elapsed:?}"
    );
    // The session was left running — the row is present and alive.
    let rows = dispatch::registry::read_entries(&jail.sessions_dir(), false);
    assert!(
        rows.iter().any(|s| s.entry.name.as_deref() == Some(name)),
        "session left in place"
    );
    jail.teardown();
}

/// The 50-cold-start smoke (spec §3 acceptance, N1): N sequential
/// start --json boots in one jail, every one exit 0 with a parseable, BOUND,
/// UNIQUE identity. N via QD_SMOKE_STARTS (default 50).
#[test]
#[ignore = "acceptance evidence: ~50 sequential boots, minutes of wall-clock"]
fn a2_cold_start_smoke_50() {
    require_bins();
    let n: usize = std::env::var("QD_SMOKE_STARTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let jail = Jail::establish("a2s");
    let mut seen_ids = std::collections::HashSet::new();
    for i in 0..n {
        let name = format!("smoke-{i}");
        let uuid = format!("11111111-2222-3333-4444-{:012}", i);
        let env = vec![
            ("QD_FAKEREPL_NAME", name.clone()),
            ("QD_FAKEREPL_SESSION_ID", uuid.clone()),
        ];
        let (code, out, err) =
            run_qd(&jail, &["start", "--interactive", "--json", &name], &env);
        assert_eq!(code, 0, "boot {i} exit 0 (err: {err:?})");
        let v = parse_json_stdout(&out);
        assert_eq!(v["sessionId"], uuid.as_str(), "boot {i} bound");
        let qd_id = v["qdId"].as_str().expect("qdId").to_string();
        assert!(seen_ids.insert(qd_id), "boot {i} minted a unique stable id");
        // Stop it so the jail never holds 50 live fakerepls.
        let (stop_code, _o, _e) = run_qd(&jail, &["stop", "--force", &name], &[]);
        assert_eq!(stop_code, 0, "boot {i} stops clean");
    }
    assert_eq!(seen_ids.len(), n, "zero resolution failures in {n} starts");
    jail.teardown();
}
