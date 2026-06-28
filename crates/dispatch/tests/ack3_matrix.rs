//! ACK-3 e2e INJECTION MATRIX (ack3-spec §2-§4) — five fault-injection rows
//! (M1-M5) + negative twins driving the REAL `qd` binary over the embedded
//! `qrmux` daemon with `fakerepl` as Claude, asserting BOTH event streams (the
//! engine file at `<QD_HOME>/state/sessions/<key>.events.jsonl` and the daemon
//! file at `<XDG_RUNTIME_DIR>/qrmux/events/<session>.daemon.<epoch>.jsonl`),
//! joined by content sha (unique-by-construction contents). Plus the R-REC
//! recovery-read rows (§4) and the coverage assertion (§3.3).
//!
//! The jail / run_qd / event-reader helpers MIRROR ack2_gate.rs (duplicated, not
//! factored — integration test binaries cannot import each other; ack3-spec §2
//! sanctions duplication, and another agent owns ack2_gate.rs this phase).
//!
//! ## Fault arming (the matrix keystone, spec §1)
//!
//! `ensure_server_running_with` spawns the daemon WITHOUT `env_clear`, so
//! `QRMUX_FAULT_*` passed as run_qd extras reaches the daemon process; the fault
//! layer reads its env ONCE at daemon start. The arming `qd` call IS the
//! session-creating one, and `qd start`'s boot waiter writes an Enter ("\r")
//! through the SAME session — so every daemon fault carries
//! `QRMUX_FAULT_MATCH_SHA256=<content sha>` (the AND-filter lets the boot "\r"
//! pass). M1/M2/M3 use SINGLE-CHUNK contents so the FRAME sha the daemon matches
//! equals the engine's content sha.
//!
//! ## EXPECTED-RED set (pending ADD-18, a parallel agent)
//!
//! M1/M2's exit-11 + stderr asserts are written to the ADD-18 contract but RED on
//! this base (the verb still exits 0). They are isolated in
//! `*_exit11_expected_red_pending_add18` tests so the event-stream asserts
//! (which MUST pass now) live in separate green tests. See the gate report.

#![allow(clippy::too_many_arguments)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use dispatch::events::{parse_events, sha256_hex, EventRecord};
use qrmux::events::{parse_line, DaemonEvent};

// ===========================================================================
// Binary locators (ack2_gate patterns, duplicated)
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
    if bin.exists() {
        if let Some(bin_mtime) = mtime(&bin) {
            let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../fakerepl/src")
                .canonicalize()
                .expect("fakerepl src dir");
            let newest_src = std::fs::read_dir(&src_dir)
                .expect("read fakerepl src")
                .flatten()
                .filter_map(|e| mtime(&e.path()))
                .max();
            if let Some(newest) = newest_src {
                assert!(
                    bin_mtime >= newest,
                    "STALE fakerepl binary at {bin:?} (older than {src_dir:?}) — \
                     the gate would test an outdated oracle. Run: cargo build -p fakerepl"
                );
            }
        }
        return bin;
    }
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "fakerepl"])
        .status()
        .expect("spawn cargo build -p fakerepl");
    assert!(status.success(), "cargo build -p fakerepl failed");
    assert!(
        bin.exists(),
        "fakerepl binary missing at {bin:?} after build"
    );
    bin
}

fn mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn require_bins() {
    let _ = qrmux_bin();
    let _ = fakerepl_bin();
}

// ===========================================================================
// Jail (fakerepl-belt shaped; ack2_gate jail discipline)
// ===========================================================================

struct Jail {
    root: PathBuf,
    home: PathBuf,
    xdg: PathBuf,
    qd_home: PathBuf,
    ev_dir: PathBuf,
    convo: PathBuf,
    uuid: String,
    created: std::cell::RefCell<Vec<String>>,
}

impl Jail {
    fn establish(tag: &str) -> Jail {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = PathBuf::from("/tmp/qd-ack3mat");
        let root = base.join("qdrg-runs").join(format!("{tag}-{nanos}"));
        let home = root.join("home");
        let xdg = base.join(format!("x-{tag}-{nanos}"));
        let qd_home = root.join("qd_home");
        let ev_dir = qd_home.join("state").join("sessions");
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
        let uuid = "11111111-2222-3333-4444-555555555555".to_string();
        let convo = projects.join(format!("{uuid}.jsonl"));
        Jail {
            root,
            home,
            xdg,
            qd_home,
            ev_dir,
            convo,
            uuid,
            created: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Base fakerepl env knobs.
    fn fakerepl_env<'a>(&'a self, name: &'a str) -> Vec<(&'a str, String)> {
        vec![
            ("QD_FAKEREPL_NAME", name.to_string()),
            ("QD_FAKEREPL_SESSION_ID", self.uuid.clone()),
            (
                "QD_FAKEREPL_CONVO_JSONL",
                self.convo.to_string_lossy().into_owned(),
            ),
        ]
    }

    /// The engine events file text (sessionId-keyed).
    fn events_text(&self) -> String {
        std::fs::read_to_string(self.ev_dir.join(format!("{}.events.jsonl", self.uuid)))
            .unwrap_or_default()
    }

    /// The full set of engine event records (sessionId + byname merged).
    fn engine_records(&self) -> Vec<EventRecord> {
        let mut out = parse_events(&self.events_text()).records;
        // Merge any byname file too (failed-boot fallback key).
        let _ = walk(&self.ev_dir, &mut |p| {
            let s = p.to_string_lossy();
            if s.contains("byname-") && s.ends_with(".events.jsonl") {
                out.extend(parse_events(&std::fs::read_to_string(p).unwrap_or_default()).records);
            }
        });
        out
    }

    /// All engine events text under the jail (for the privacy grep).
    fn all_engine_text(&self) -> String {
        let mut out = String::new();
        let _ = walk(&self.qd_home, &mut |p| {
            if p.to_string_lossy().ends_with(".events.jsonl") {
                out.push_str(&std::fs::read_to_string(p).unwrap_or_default());
                out.push('\n');
            }
        });
        out
    }

    /// The daemon events dir (`<xdg>/qrmux/events`).
    fn daemon_events_dir(&self) -> PathBuf {
        self.xdg.join("qrmux").join("events")
    }

    /// All daemon event records for this jail (concat every `.daemon.<epoch>.jsonl`
    /// in filename order), parsed via the qrmux reader.
    fn daemon_records(&self) -> Vec<DaemonEvent> {
        let mut files: Vec<PathBuf> = Vec::new();
        let _ = walk(&self.daemon_events_dir(), &mut |p| {
            if p.to_string_lossy().ends_with(".jsonl") {
                files.push(p.to_path_buf());
            }
        });
        files.sort();
        let mut out = Vec::new();
        for f in files {
            for line in std::fs::read_to_string(&f).unwrap_or_default().lines() {
                if let Some(ev) = parse_line(line) {
                    out.push(ev);
                }
            }
        }
        out
    }

    /// All daemon events text under the jail (for the privacy grep).
    fn all_daemon_text(&self) -> String {
        let mut out = String::new();
        let _ = walk(&self.daemon_events_dir(), &mut |p| {
            if p.to_string_lossy().ends_with(".jsonl") {
                out.push_str(&std::fs::read_to_string(p).unwrap_or_default());
                out.push('\n');
            }
        });
        out
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

fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) -> std::io::Result<()> {
    if dir.is_dir() {
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            if p.is_dir() {
                walk(&p, f)?;
            } else {
                f(&p);
            }
        }
    }
    Ok(())
}

// ===========================================================================
// qd driver (ack2_gate run_qd, duplicated)
// ===========================================================================

fn run_qd(jail: &Jail, args: &[&str], extra: &[(&str, String)]) -> (i32, String, String, Duration) {
    // WP-B-CS-1 (D2): `qd start` now auto-detects the driver, and this harness pipes
    // stdio (`cmd.output()`), so a bare start would be a non-TTY caller → the HEADLESS
    // surface (and a no-`-p` start would even hit Fork B's refuse-no-prompt). These
    // recovery-matrix tests exercise the INTERACTIVE create + -p delivery, so force
    // the interactive surface with the `--interactive` override (inserted after the
    // `start` subcommand). Behavior delta flagged in the WP-B-CS-1 response.
    let injected: Vec<String>;
    let arg_refs: Vec<&str>;
    let args: &[&str] = if args.first() == Some(&"start") {
        if let Some(name) = args.get(1) {
            jail.created.borrow_mut().push((*name).to_string());
        }
        injected = std::iter::once("start".to_string())
            .chain(std::iter::once("--interactive".to_string()))
            .chain(args[1..].iter().map(|s| s.to_string()))
            .collect();
        arg_refs = injected.iter().map(String::as_str).collect();
        &arg_refs
    } else {
        args
    };
    let fr = fakerepl_bin();
    let mut cmd = Command::new(qd_bin());
    cmd.args(args);
    cmd.env_clear()
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
    let start = Instant::now();
    let out = cmd.output().expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        start.elapsed(),
    )
}

// ===========================================================================
// Event-stream helpers (engine + daemon)
// ===========================================================================

/// The engine event-name sequence for `send_id`, in file order.
fn engine_seq(recs: &[EventRecord], send_id: &str) -> Vec<String> {
    recs.iter()
        .filter(|r| r.send_id().as_deref() == Some(send_id))
        .map(|r| r.event.clone())
        .collect()
}

/// The send_id of the LAST `send-initiated` for `verb` (+ optional send_path).
fn send_id_for(recs: &[EventRecord], verb: &str, send_path: Option<&str>) -> Option<String> {
    recs.iter()
        .rfind(|r| {
            r.event == "send-initiated"
                && r.str_field("verb").as_deref() == Some(verb)
                && send_path
                    .map(|sp| r.str_field("send_path").as_deref() == Some(sp))
                    .unwrap_or(true)
        })
        .and_then(|r| r.send_id())
}

/// The `content_sha256` of a daemon event, if it carries one.
fn daemon_sha(ev: &DaemonEvent) -> Option<&str> {
    match ev {
        DaemonEvent::PtyBytesWritten { content_sha256, .. }
        | DaemonEvent::PtyWriteFailed { content_sha256, .. } => Some(content_sha256),
        _ => None,
    }
}

/// The daemon event NAME (kebab), for coverage + scanning.
fn daemon_event_name(ev: &DaemonEvent) -> &'static str {
    match ev {
        DaemonEvent::SessionOpened { .. } => "session-opened",
        DaemonEvent::PtyBytesWritten { .. } => "pty-bytes-written",
        DaemonEvent::PtyWriteFailed { .. } => "pty-write-failed",
        DaemonEvent::SessionClosed { .. } => "session-closed",
        DaemonEvent::EventsTruncated { .. } => "events-truncated",
        DaemonEvent::Heartbeat { .. } => "heartbeat",
    }
}

// ===========================================================================
// SHARED ROW PREDICATES (pure; the mutation-evidence file DUPLICATES these by
// shape — see ack3_mutation_evidence.rs — because test binaries cannot import
// each other; ack3-spec §3.1 house pattern).
// ===========================================================================

/// M1 daemon-side predicate: NO daemon record carries `sha` (the frame was
/// dropped). The M1-vs-M3 discriminator: TRUE for M1, FALSE for M3.
fn pred_daemon_sha_absent(daemon: &[DaemonEvent], sha: &str) -> bool {
    !daemon.iter().any(|e| daemon_sha(e) == Some(sha))
}

/// M2 daemon-side predicate: a `pty-write-failed{errno:5}` for `sha` is present,
/// and NO `pty-bytes-written` for `sha`.
fn pred_daemon_write_failed(daemon: &[DaemonEvent], sha: &str) -> bool {
    let failed = daemon.iter().any(|e| {
        matches!(e, DaemonEvent::PtyWriteFailed { content_sha256, errno: Some(5), .. } if content_sha256 == sha)
    });
    let written = daemon
        .iter()
        .any(|e| matches!(e, DaemonEvent::PtyBytesWritten { content_sha256, .. } if content_sha256 == sha));
    failed && !written
}

/// M3 daemon-side predicate: `pty-bytes-written` for `sha` is PRESENT (the
/// deception). The M1-vs-M3 discriminator: TRUE for M3, FALSE for M1.
fn pred_daemon_bytes_written(daemon: &[DaemonEvent], sha: &str) -> bool {
    daemon
        .iter()
        .any(|e| matches!(e, DaemonEvent::PtyBytesWritten { content_sha256, .. } if content_sha256 == sha))
}

/// Engine "written-never-delivered" prefix (M1/M2): send-initiated PRESENT,
/// chunks-delivered ABSENT for `send_id`.
fn pred_engine_initiated_no_chunks(recs: &[EventRecord], send_id: &str) -> bool {
    let seq = engine_seq(recs, send_id);
    seq.contains(&"send-initiated".to_string()) && !seq.contains(&"chunks-delivered".to_string())
}

/// M3/M4 engine exact sequence: [send-initiated, chunks-delivered, anchor-timeout].
fn pred_engine_anchor_timeout_seq(recs: &[EventRecord], send_id: &str) -> bool {
    engine_seq(recs, send_id) == ["send-initiated", "chunks-delivered", "anchor-timeout"]
}

/// M5 engine exact sequence: [send-initiated, chunks-delivered,
/// turn-anchored-mismatch] (EXACTLY — a spurious extra terminal must fail it).
fn pred_engine_mismatch_seq(recs: &[EventRecord], send_id: &str) -> bool {
    engine_seq(recs, send_id)
        == [
            "send-initiated",
            "chunks-delivered",
            "turn-anchored-mismatch",
        ]
}

/// ADD-18 exit-contract predicate (§5.2): status == 11 AND stderr carries the
/// pinned write-failure line. (EXPECTED RED until ADD-18 lands.)
const ADD18_STDERR: &str = "ERROR: PTY write failed";
fn pred_add18_exit_contract(status: i32, stderr: &str) -> bool {
    status == 11 && stderr.contains(ADD18_STDERR)
}

/// M4 child-side predicate: the fakerepl report carries an `eaten{bytes==len}`
/// record (M4 cannot collapse into M3). `report` is the report JSONL text.
fn pred_report_eaten(report: &str, content_len: usize) -> bool {
    report
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|v| {
            v.get("event").and_then(|e| e.as_str()) == Some("eaten")
                && v.get("bytes").and_then(|b| b.as_u64()) == Some(content_len as u64)
        })
}

// ===========================================================================
// Privacy (key-shaped canary, spec §2 / §6.4 pre-ADD-20 form)
// ===========================================================================

/// Build a row message: `ACK3-INJ<n>-<nanos>` + a ≥24-char key-shaped canary +
/// optional filler to reach `pad_to` bytes (ASCII). Returns (message, canary).
fn row_message(n: u32, pad_to: usize) -> (String, String) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // ≥24-char [A-Za-z0-9_-] run, unique by construction.
    let canary = format!("ACK3canaryKEY_{n}_{nanos}");
    assert!(canary.len() >= 24, "canary must be ≥24 chars: {canary}");
    let head = format!("ACK3-INJ{n}-{nanos} {canary} ");
    let msg = if head.len() >= pad_to {
        head
    } else {
        format!("{head}{}", "X".repeat(pad_to - head.len()))
    };
    (msg, canary)
}

/// The key-shaped canary MUST be absent from EVERY record in BOTH streams (sha
/// only). Pre-ADD-20 this is the whole privacy assert; post-ADD-20 the parallel
/// agent owns the plain-text-preview lane (we assert the KEY canary stays out).
fn assert_canary_absent(jail: &Jail, canary: &str) {
    let engine = jail.all_engine_text();
    let daemon = jail.all_daemon_text();
    assert!(
        !engine.contains(canary),
        "PRIVACY: key-shaped canary {canary:?} leaked into an ENGINE events file:\n{engine}"
    );
    assert!(
        !daemon.contains(canary),
        "PRIVACY: key-shaped canary {canary:?} leaked into a DAEMON events file:\n{daemon}"
    );
    // Positive control: at least one stream is non-empty (the grep is non-vacuous).
    assert!(
        !engine.trim().is_empty() || !daemon.trim().is_empty(),
        "PRIVACY positive control: both streams empty — the grep would be vacuous"
    );
}

// ===========================================================================
// M1 — inj-1 frame drop
// ===========================================================================

/// Arm + drive M1's fault, returning (jail, send_id, content_sha, exit, stderr).
/// Single-chunk message so frame sha == content sha. ~15s (one OP_TIMEOUT).
fn drive_m1(jail: &Jail, name: &str) -> (String, String, i32, String, String) {
    let (msg, canary) = row_message(1, 0);
    let sha = sha256_hex(msg.as_bytes());
    let mut env = jail.fakerepl_env(name);
    // Arm the daemon BEFORE the first qd call (env read once at daemon start).
    env.push(("QRMUX_FAULT_DROP_FRAMES", "send-input".to_string()));
    env.push(("QRMUX_FAULT_SESSION", name.to_string()));
    env.push(("QRMUX_FAULT_MATCH_SHA256", sha.clone()));
    env.push(("QD_FAKEREPL_BUSY_MS", "800".to_string()));

    // Boot (the arming call). The boot "\r" passes the sha AND-filter.
    let (cb, _o, _e, _) = run_qd(jail, &["start", name], &env);
    assert_eq!(cb, 0, "M1 boot succeeds (boot Enter passes the sha filter)");
    std::thread::sleep(Duration::from_millis(600));

    let (code, _out, err, d) = run_qd(jail, &["send:pty", name, &msg], &env);
    eprintln!(
        "[M1] send:pty (frame drop) took {:.1}s exit={code}",
        d.as_secs_f64()
    );
    let sid =
        send_id_for(&jail.engine_records(), "send:pty", None).expect("M1 send-initiated present");
    (sid, sha, code, err, canary)
}

#[test]
fn m1_frame_drop_event_streams() {
    require_bins();
    let jail = Jail::establish("m1");
    let name = "m1";
    let (sid, sha, _code, _err, canary) = drive_m1(&jail, name);

    // Daemon: NO record for the sha (the frame was dropped).
    let daemon = jail.daemon_records();
    assert!(
        pred_daemon_sha_absent(&daemon, &sha),
        "M1 daemon: NO record for the dropped frame's sha — got {daemon:?}"
    );

    // Engine: send-initiated PRESENT, chunks-delivered ABSENT.
    let recs = jail.engine_records();
    assert!(
        pred_engine_initiated_no_chunks(&recs, &sid),
        "M1 engine: send-initiated present, chunks-delivered absent — seq {:?}",
        engine_seq(&recs, &sid)
    );

    assert_canary_absent(&jail, &canary);
    jail.teardown();
}

/// M1 ADD-18 contract (§5): the verb blocks ~15s then exits 11 + the write-failure
/// stderr line. EXPECTED RED on this base (the verb exits 0 pre-ADD-18); written
/// to the contract so it becomes integration evidence.
#[test]
fn m1_frame_drop_exit11_expected_red_pending_add18() {
    require_bins();
    let jail = Jail::establish("m1x");
    let name = "m1x";
    let (_sid, _sha, code, err, _canary) = drive_m1(&jail, name);
    assert!(
        pred_add18_exit_contract(code, &err),
        "M1 ADD-18 contract: expected exit 11 + {ADD18_STDERR:?}, got exit={code} stderr={err:?}"
    );
    jail.teardown();
}

/// M1 N-twin: seam UNSET → the send delivers cleanly (chunks-delivered present).
#[test]
fn m1_negative_twin_clean() {
    require_bins();
    let jail = Jail::establish("m1n");
    let name = "m1n";
    let (msg, canary) = row_message(1, 0);
    let sha = sha256_hex(msg.as_bytes());
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_FAKEREPL_BUSY_MS", "800".to_string()));

    let (cb, _o, _e, _) = run_qd(&jail, &["start", name], &env);
    assert_eq!(cb, 0);
    std::thread::sleep(Duration::from_millis(600));
    let (code, _out, _err, _) = run_qd(&jail, &["send:pty", name, &msg], &env);
    assert_eq!(code, 0, "N-twin clean idle send exits 0");

    let recs = jail.engine_records();
    let sid = send_id_for(&recs, "send:pty", None).expect("N-twin send-initiated");
    let seq = engine_seq(&recs, &sid);
    assert!(
        seq.contains(&"chunks-delivered".to_string()),
        "N-twin: chunks-delivered present (clean delivery) — seq {seq:?}"
    );
    // Daemon: bytes-written present for the sha (the bytes reached the PTY).
    let daemon = jail.daemon_records();
    assert!(
        pred_daemon_bytes_written(&daemon, &sha),
        "N-twin daemon: pty-bytes-written present for the sha"
    );
    // Whole-matrix sanity: NO fault leakage.
    assert_no_fault_leakage(&daemon);
    assert_canary_absent(&jail, &canary);
    jail.teardown();
}

// ===========================================================================
// M2 — inj-2 PTY write error
// ===========================================================================

fn drive_m2(jail: &Jail, name: &str) -> (String, String, i32, String, String) {
    let (msg, canary) = row_message(2, 0);
    let sha = sha256_hex(msg.as_bytes());
    let mut env = jail.fakerepl_env(name);
    env.push(("QRMUX_FAULT_PTY_WRITE", "error".to_string()));
    env.push(("QRMUX_FAULT_SESSION", name.to_string()));
    env.push(("QRMUX_FAULT_MATCH_SHA256", sha.clone()));
    env.push(("QD_FAKEREPL_BUSY_MS", "800".to_string()));

    let (cb, _o, _e, _) = run_qd(jail, &["start", name], &env);
    assert_eq!(cb, 0, "M2 boot succeeds (boot Enter passes the sha filter)");
    std::thread::sleep(Duration::from_millis(600));

    let (code, _out, err, d) = run_qd(jail, &["send:pty", name, &msg], &env);
    eprintln!(
        "[M2] send:pty (write error) took {:.1}s exit={code}",
        d.as_secs_f64()
    );
    let sid =
        send_id_for(&jail.engine_records(), "send:pty", None).expect("M2 send-initiated present");
    (sid, sha, code, err, canary)
}

#[test]
fn m2_pty_write_error_event_streams() {
    require_bins();
    let jail = Jail::establish("m2");
    let name = "m2";
    let (sid, sha, _code, _err, canary) = drive_m2(&jail, name);

    let daemon = jail.daemon_records();
    assert!(
        pred_daemon_write_failed(&daemon, &sha),
        "M2 daemon: pty-write-failed{{errno:5}} for the sha + NO pty-bytes-written — got {daemon:?}"
    );

    let recs = jail.engine_records();
    assert!(
        pred_engine_initiated_no_chunks(&recs, &sid),
        "M2 engine: send-initiated present, chunks-delivered absent — seq {:?}",
        engine_seq(&recs, &sid)
    );

    assert_canary_absent(&jail, &canary);
    jail.teardown();
}

/// M2 ADD-18 contract (§5.2): exit 11 + stderr line. EXPECTED RED pending ADD-18.
#[test]
fn m2_pty_write_error_exit11_expected_red_pending_add18() {
    require_bins();
    let jail = Jail::establish("m2x");
    let name = "m2x";
    let (_sid, _sha, code, err, _canary) = drive_m2(&jail, name);
    assert!(
        pred_add18_exit_contract(code, &err),
        "M2 ADD-18 contract: expected exit 11 + {ADD18_STDERR:?}, got exit={code} stderr={err:?}"
    );
    jail.teardown();
}

/// M2 N-twin: seam UNSET → clean delivery.
#[test]
fn m2_negative_twin_clean() {
    require_bins();
    let jail = Jail::establish("m2n");
    let name = "m2n";
    let (msg, canary) = row_message(2, 0);
    let sha = sha256_hex(msg.as_bytes());
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_FAKEREPL_BUSY_MS", "800".to_string()));

    let (cb, _o, _e, _) = run_qd(&jail, &["start", name], &env);
    assert_eq!(cb, 0);
    std::thread::sleep(Duration::from_millis(600));
    let (code, _out, _err, _) = run_qd(&jail, &["send:pty", name, &msg], &env);
    assert_eq!(code, 0);
    let recs = jail.engine_records();
    let sid = send_id_for(&recs, "send:pty", None).expect("N-twin send-initiated");
    assert!(engine_seq(&recs, &sid).contains(&"chunks-delivered".to_string()));
    let daemon = jail.daemon_records();
    assert!(pred_daemon_bytes_written(&daemon, &sha));
    assert_no_fault_leakage(&daemon);
    assert_canary_absent(&jail, &canary);
    jail.teardown();
}

/// M2 filter-precision negative (spec §2): fault ARMED (error + session +
/// sha filters), but the only session's NAME does not match the session
/// filter → the send flows clean e2e (the AND-filter excludes it).
///
/// REDESIGNED at lead integration (was: two fakerepls w/ distinct identities
/// in ONE daemon — IMPOSSIBLE, discovered here): the engine create path
/// captures ONLY the ANTHROPIC_* backend keys per session (launch.rs
/// BACKEND_ENV_KEYS / write_session_env_file_with_unsets); every other var —
/// including QD_FAKEREPL_SESSION_ID / _CONVO_JSONL — is inherited from the
/// DAEMON's spawn env. So a second session in the same daemon silently got
/// the FIRST session's fakerepl identity, the two registry rows collided on
/// sessionId, and name-resolution became scan-order-dependent (a ~1-in-3
/// "No session matching" flake). Same-daemon filter precision is carried by
/// ack1's in-row negatives at the daemon level, split by leg (merge-ruling
/// C-1 cite correction): SESSION leg = R-F1's control (non-matching session,
/// same armed daemon — ack1_events.rs r_f1_fault_error); SHA leg = R-F2's
/// control (non-matching content, same armed session — r_f2_fault_swallow).
/// THIS row keeps the e2e leg: an ARMED daemon + a non-matching session
/// name → clean flow.
#[test]
fn m2_filter_precision_other_session_clean() {
    require_bins();
    let jail = Jail::establish("m2fp");
    let armed_for = "m2fpa"; // the session filter names this — never created
    let other = "m2fpb"; // the only session; name does NOT match the filter
    let (msg, canary) = row_message(2, 0);
    let sha = sha256_hex(msg.as_bytes());

    // Arm error+session+sha on the daemon at its (session-creating) boot. The
    // boot Enter and the test send both go to `other`, which the session
    // filter excludes — nothing is faulted despite the sha matching.
    let mut env = jail.fakerepl_env(other);
    env.push(("QRMUX_FAULT_PTY_WRITE", "error".to_string()));
    env.push(("QRMUX_FAULT_SESSION", armed_for.to_string()));
    env.push(("QRMUX_FAULT_MATCH_SHA256", sha.clone()));
    env.push(("QD_FAKEREPL_BUSY_MS", "800".to_string()));
    let (c1, _o, _e, _) = run_qd(&jail, &["start", other], &env);
    assert_eq!(c1, 0, "non-matching session boots in the ARMED daemon");

    // Send the FILTERED-SHA content to the non-matching session → flows clean
    // (plain idle send, no --wait — the clean path returns exit 0).
    let (code, out, err, _) = run_qd(&jail, &["send:pty", other, &msg], &env);
    assert_eq!(
        code, 0,
        "send to the non-armed session flows clean (exit 0); stdout={out:?} stderr={err:?}"
    );

    // Daemon: a pty-bytes-written for the sha exists (the unfiltered session's
    // frame reached the PTY) and there is NO pty-write-failed for it.
    let daemon = jail.daemon_records();
    assert!(
        pred_daemon_bytes_written(&daemon, &sha),
        "filter precision: the non-armed session's bytes reached the PTY — got {daemon:?}"
    );
    assert!(
        !daemon.iter().any(|e| matches!(
            e,
            DaemonEvent::PtyWriteFailed { content_sha256, .. } if content_sha256 == &sha
        )),
        "filter precision: NO pty-write-failed for the sha (the session filter excluded the other session)"
    );
    assert_canary_absent(&jail, &canary);
    jail.teardown();
}

// ===========================================================================
// M3 — inj-3 silent post-ack drop (swallow)
// ===========================================================================

#[test]
fn m3_silent_swallow_event_streams() {
    require_bins();
    let jail = Jail::establish("m3");
    let name = "m3";
    let (msg, canary) = row_message(3, 0);
    let sha = sha256_hex(msg.as_bytes());
    let report = jail.root.join("m3-report.jsonl");
    let mut env = jail.fakerepl_env(name);
    env.push(("QRMUX_FAULT_PTY_WRITE", "swallow".to_string()));
    env.push(("QRMUX_FAULT_MATCH_SHA256", sha.clone()));
    env.push(("QD_FAKEREPL_BUSY_MS", "800".to_string()));
    env.push(("QD_FAKEREPL_REPORT", report.to_string_lossy().into_owned()));

    // Seed the convo (so --wait resolves the jsonl) + settle to idle. The seed's
    // sha differs from the injected content sha → it passes the AND-filter clean.
    let (cb, _o, _e, _) = run_qd(&jail, &["start", name, "-p", "seed"], &env);
    assert_eq!(cb, 0);
    std::thread::sleep(Duration::from_millis(1200));

    let (code, _out, _err, d) = run_qd(
        &jail,
        &["send:pty", name, &msg, "--wait", "--timeout", "3"],
        &env,
    );
    eprintln!("[M3] swallow --wait took {:.1}s", d.as_secs_f64());
    assert_eq!(
        code, 1,
        "M3 --wait TimedOut exit 1 (the write succeeded daemon-side)"
    );

    // Daemon: pty-bytes-written PRESENT for the sha (the deception — bytes ack'd
    // but never reached the PTY).
    let daemon = jail.daemon_records();
    assert!(
        pred_daemon_bytes_written(&daemon, &sha),
        "M3 daemon: pty-bytes-written present (swallow reports success) — got {daemon:?}"
    );

    // Engine EXACT sequence: send-initiated → chunks-delivered → anchor-timeout
    // with waited_ms == 3000.
    let recs = jail.engine_records();
    let sid = send_id_for(&recs, "send:pty", None).expect("M3 send-initiated");
    assert!(
        pred_engine_anchor_timeout_seq(&recs, &sid),
        "M3 engine exact sequence — got {:?}",
        engine_seq(&recs, &sid)
    );
    let at = recs
        .iter()
        .find(|r| r.event == "anchor-timeout" && r.send_id().as_deref() == Some(&sid))
        .unwrap();
    assert_eq!(
        at.u64_field("waited_ms"),
        Some(3000),
        "M3 waited_ms == 3000"
    );

    // Child corroboration: the fakerepl report shows NO burst/turn carrying the
    // INJECTED content (the bytes were swallowed daemon-side, so fakerepl never
    // received them). The seed (`new -p seed`) legitimately produced its own
    // 4-byte turn; the injected message (much longer) must NOT appear as a turn of
    // its own byte length. (Discriminator is the daemon log; this is corroboration
    // only, spec §2.)
    let report_text = std::fs::read_to_string(&report).unwrap_or_default();
    let injected_turn = report_text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|v| {
            v.get("event").and_then(|e| e.as_str()) == Some("turn")
                && v.get("bytes").and_then(|b| b.as_u64()) == Some(msg.len() as u64)
        });
    assert!(
        !injected_turn,
        "M3 corroboration: fakerepl saw NO turn carrying the injected content (bytes \
         swallowed daemon-side) — report:\n{report_text}"
    );

    assert_canary_absent(&jail, &canary);
    jail.teardown();
}

/// M3 N-twin: seam UNSET → the send anchors cleanly under --wait.
#[test]
fn m3_negative_twin_clean() {
    require_bins();
    let jail = Jail::establish("m3n");
    let name = "m3n";
    let (msg, canary) = row_message(3, 0);
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_FAKEREPL_BUSY_MS", "800".to_string()));

    // Seed convo so --wait resolves the jsonl, settle to idle.
    let (cb, _o, _e, _) = run_qd(&jail, &["start", name, "-p", "seed"], &env);
    assert_eq!(cb, 0);
    std::thread::sleep(Duration::from_millis(1200));

    let (code, _out, _err, _) = run_qd(
        &jail,
        &["send:pty", name, &msg, "--wait", "--timeout", "8"],
        &env,
    );
    assert_eq!(code, 0, "M3 N-twin --wait Complete exit 0");
    let recs = jail.engine_records();
    let sid = send_id_for(&recs, "send:pty", Some("idle")).expect("N-twin send-initiated");
    assert_eq!(
        engine_seq(&recs, &sid),
        ["send-initiated", "chunks-delivered", "turn-anchored"],
        "M3 N-twin exact sequence (clean anchor)"
    );
    let daemon = jail.daemon_records();
    assert_no_fault_leakage(&daemon);
    assert_canary_absent(&jail, &canary);
    jail.teardown();
}

// ===========================================================================
// M4 — inj-4 no-anchor with consumption (EAT_INPUT)
// ===========================================================================

#[test]
fn m4_eat_input_event_streams() {
    require_bins();
    let jail = Jail::establish("m4");
    let name = "m4";
    let (msg, canary) = row_message(4, 0);
    let sha = sha256_hex(msg.as_bytes());
    let report = jail.root.join("m4-report.jsonl");
    let mut env = jail.fakerepl_env(name);
    // NO daemon fault. fakerepl eats the input.
    env.push(("QD_FAKEREPL_EAT_INPUT", "1".to_string()));
    env.push(("QD_FAKEREPL_REPORT", report.to_string_lossy().into_owned()));
    env.push(("QD_FAKEREPL_BUSY_MS", "800".to_string()));

    // NOTE: EAT_INPUT eats EVERY burst, so `new -p seed` cannot anchor and the
    // convo file is never written by a seed. We therefore PRE-CREATE the convo
    // file (so --wait resolves the jsonl) with a benign prior record; the eaten
    // injected send adds no NEW user record (the M4 assert).
    std::fs::write(
        &jail.convo,
        "{\"type\":\"user\",\"message\":{\"content\":\"seed-prior\"}}\n",
    )
    .unwrap();
    let (cb, _o, _e, _) = run_qd(&jail, &["start", name], &env);
    assert_eq!(cb, 0);
    std::thread::sleep(Duration::from_millis(600));

    let (code, _out, _err, d) = run_qd(
        &jail,
        &["send:pty", name, &msg, "--wait", "--timeout", "3"],
        &env,
    );
    eprintln!("[M4] eat-input --wait took {:.1}s", d.as_secs_f64());
    assert_eq!(code, 1, "M4 --wait TimedOut exit 1");

    // Daemon: pty-bytes-written PRESENT (the bytes DID reach the PTY; the loss is
    // child-side, not daemon-side — this is the M3-vs-M4 discriminator on the
    // daemon stream: M4 wrote, M3 swallowed).
    let daemon = jail.daemon_records();
    assert!(
        pred_daemon_bytes_written(&daemon, &sha),
        "M4 daemon: pty-bytes-written present (bytes reached the PTY) — got {daemon:?}"
    );

    // Engine EXACT sequence (same as M3).
    let recs = jail.engine_records();
    let sid = send_id_for(&recs, "send:pty", None).expect("M4 send-initiated");
    assert!(
        pred_engine_anchor_timeout_seq(&recs, &sid),
        "M4 engine exact sequence — got {:?}",
        engine_seq(&recs, &sid)
    );

    // Child consumption assert: report has eaten{bytes==content_len}; the convo
    // JSONL has NO user record (the bytes were eaten before the composer).
    let report_text = std::fs::read_to_string(&report).unwrap_or_default();
    assert!(
        pred_report_eaten(&report_text, msg.len()),
        "M4 child: report has eaten{{bytes=={}}} — report:\n{report_text}",
        msg.len()
    );
    // The injected content produced NO new user record (eaten before the
    // composer). The pre-seeded "seed-prior" record is the only user record; the
    // injected message text must not appear in the convo.
    let convo = std::fs::read_to_string(&jail.convo).unwrap_or_default();
    let user_records = convo
        .lines()
        .filter(|l| l.contains("\"type\":\"user\""))
        .count();
    assert_eq!(
        user_records, 1,
        "M4 child: exactly the pre-seed user record (the eaten input added none) — convo:\n{convo}"
    );
    assert!(
        !convo.contains(&msg),
        "M4 child: the injected content never reached a user record (eaten) — convo:\n{convo}"
    );

    assert_canary_absent(&jail, &canary);
    jail.teardown();
}

/// M4 N-twin: EAT_INPUT UNSET → the send anchors cleanly (the user record lands).
#[test]
fn m4_negative_twin_clean() {
    require_bins();
    let jail = Jail::establish("m4n");
    let name = "m4n";
    let (msg, canary) = row_message(4, 0);
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_FAKEREPL_BUSY_MS", "800".to_string()));

    let (cb, _o, _e, _) = run_qd(&jail, &["start", name, "-p", "seed"], &env);
    assert_eq!(cb, 0);
    std::thread::sleep(Duration::from_millis(1200));
    let (code, _out, _err, _) = run_qd(
        &jail,
        &["send:pty", name, &msg, "--wait", "--timeout", "8"],
        &env,
    );
    assert_eq!(code, 0, "M4 N-twin --wait Complete exit 0");
    let recs = jail.engine_records();
    let sid = send_id_for(&recs, "send:pty", Some("idle")).expect("N-twin send-initiated");
    assert_eq!(
        engine_seq(&recs, &sid),
        ["send-initiated", "chunks-delivered", "turn-anchored"],
        "M4 N-twin exact sequence (clean anchor)"
    );
    // The convo DID get a user record (the input was NOT eaten).
    let convo = std::fs::read_to_string(&jail.convo).unwrap_or_default();
    assert!(
        convo.contains("\"type\":\"user\""),
        "M4 N-twin: convo has a user record (input not eaten)"
    );
    assert_no_fault_leakage(&jail.daemon_records());
    assert_canary_absent(&jail, &canary);
    jail.teardown();
}

// ===========================================================================
// M5 — inj-5 truncation (3-chunk idle path, W8 verify mismatch)
// ===========================================================================

#[test]
fn m5_truncation_event_streams() {
    require_bins();
    let jail = Jail::establish("m5");
    let name = "m5";
    // 2100-byte ASCII message → 3 chunks (1024-byte splitter).
    let (msg, canary) = row_message(5, 2100);
    assert_eq!(msg.len(), 2100, "M5 message is exactly 2100 bytes");
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_FAKEREPL_TRUNCATE_USER_RECORD_BYTES", "1500".to_string()));
    env.push(("QD_FAKEREPL_BUSY_MS", "800".to_string()));

    // Seed convo + settle to idle so the idle chunked path (W8 verify) engages.
    let (cb, _o, _e, _) = run_qd(&jail, &["start", name, "-p", "seed"], &env);
    assert_eq!(cb, 0);
    std::thread::sleep(Duration::from_millis(1200));

    let (code, _out, err, d) = run_qd(&jail, &["send:pty", name, &msg], &env);
    eprintln!(
        "[M5] truncation idle send took {:.1}s exit={code}",
        d.as_secs_f64()
    );
    assert_eq!(code, 1, "M5 verify Truncated exit 1 (existing surface)");
    assert!(
        err.contains("payload truncated"),
        "M5 stderr names the truncation: {err:?}"
    );

    // Daemon: pty-bytes-written ×3 (per-frame full bytes — every chunk reached the
    // PTY; the truncation is child-side at the user-record write).
    let daemon = jail.daemon_records();
    let written: Vec<u64> = daemon
        .iter()
        .filter_map(|e| match e {
            DaemonEvent::PtyBytesWritten { bytes, .. } => Some(*bytes),
            _ => None,
        })
        .collect();
    // 3 text chunks + a separate "\r" frame. Assert AT LEAST the 3 chunk writes
    // landed (the chunked text frames); the "\r" is a 4th 1-byte write.
    assert!(
        written.iter().filter(|&&b| b > 1).count() >= 3,
        "M5 daemon: ≥3 chunked text frames written full-byte — got bytes {written:?}"
    );

    // Engine EXACT sequence ending in turn-anchored-mismatch (exactly ONE terminal).
    let recs = jail.engine_records();
    let sid = send_id_for(&recs, "send:pty", Some("idle")).expect("M5 send-initiated");
    assert!(
        pred_engine_mismatch_seq(&recs, &sid),
        "M5 engine exact sequence ending in turn-anchored-mismatch — got {:?}",
        engine_seq(&recs, &sid)
    );
    let mm = recs
        .iter()
        .find(|r| r.event == "turn-anchored-mismatch" && r.send_id().as_deref() == Some(&sid))
        .unwrap();
    assert_eq!(
        mm.u64_field("expected_len"),
        Some(2100),
        "M5 expected_len 2100"
    );
    assert_eq!(mm.u64_field("actual_len"), Some(1500), "M5 actual_len 1500");

    assert_canary_absent(&jail, &canary);
    jail.teardown();
}

/// M5 N-twin: TRUNCATE UNSET → the 3-chunk idle NO-WAIT send is seen cleanly. Post
/// the §X.5 (3-phase delivery) remap, the async no-wait W8 success emits `message-seen`
/// (the on-received terminal), NOT `turn-anchored` (which is retained only on the
/// `--wait`/new-p/recovery paths). The clean-anchor invariant holds — exactly ONE
/// terminal per send_id — it is just `message-seen` now.
#[test]
fn m5_negative_twin_clean() {
    require_bins();
    let jail = Jail::establish("m5n");
    let name = "m5n";
    let (msg, canary) = row_message(5, 2100);
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_FAKEREPL_BUSY_MS", "800".to_string()));

    let (cb, _o, _e, _) = run_qd(&jail, &["start", name, "-p", "seed"], &env);
    assert_eq!(cb, 0);
    std::thread::sleep(Duration::from_millis(1200));
    let (code, _out, _err, _) = run_qd(&jail, &["send:pty", name, &msg], &env);
    assert_eq!(
        code, 0,
        "M5 N-twin clean idle send exit 0 (verify Verified)"
    );
    let recs = jail.engine_records();
    let sid = send_id_for(&recs, "send:pty", Some("idle")).expect("N-twin send-initiated");
    assert_eq!(
        engine_seq(&recs, &sid),
        ["send-initiated", "chunks-delivered", "message-seen"],
        "M5 N-twin exact sequence (clean on-received: async no-wait W8 → message-seen, §X.5)"
    );
    assert_no_fault_leakage(&jail.daemon_records());
    assert_canary_absent(&jail, &canary);
    jail.teardown();
}

/// I2 (async pty, the UNGATE) — a ≤1-chunk SHORT async no-wait send. Pre-ungate a
/// single-chunk send was OUT of verify scope (`payload_needs_verify` false) so it
/// emitted NO on-received terminal. Group C's ungate (`if verify_eligible &&
/// (needs_verify || !wait)`, send.rs:567) makes the no-wait path verify even a
/// short send and emit `message-seen` — and the short send STILL exits 0 (the
/// degrade-warn arms keep it from newly failing-loud). NO `turn-anchored` on this
/// path (it is the on-received terminal). Real local-built dispatch; tailed log.
#[test]
fn i2_async_pty_short_send_ungated_to_message_seen_exit0() {
    require_bins();
    let jail = Jail::establish("i2");
    let name = "i2";
    // A SHORT message: well under CHUNK_BYTES=1024 → chunks==1 → needs_verify false.
    // (Use the canary builder for the privacy assert; pad_to small ⇒ one chunk.)
    let (msg, canary) = row_message(2, 64);
    assert!(
        msg.len() <= 1024,
        "I2 must be a ≤1-chunk (≤1024B) send to prove the ungate; got {} bytes",
        msg.len()
    );
    let mut env = jail.fakerepl_env(name);
    // A busy window so the idle delivery goes busy → verify_eligible stays true
    // (the ungate then fires on the !wait path).
    env.push(("QD_FAKEREPL_BUSY_MS", "800".to_string()));

    let (cb, _o, _e, _) = run_qd(&jail, &["start", name, "-p", "seed"], &env);
    assert_eq!(cb, 0);
    std::thread::sleep(Duration::from_millis(1200));
    // NO --wait: the async no-wait path is the one Group C ungates.
    let (code, _out, _err, _) = run_qd(&jail, &["send:pty", name, &msg], &env);
    assert_eq!(
        code, 0,
        "I2: a short async no-wait send STILL exits 0 (ungate degrades, never fails-loud)"
    );

    let recs = jail.engine_records();
    let sid = send_id_for(&recs, "send:pty", Some("idle")).expect("I2 send-initiated");
    let seq = engine_seq(&recs, &sid);
    assert_eq!(
        seq,
        ["send-initiated", "chunks-delivered", "message-seen"],
        "I2: short no-wait send emits message-seen (the ungate), exactly one terminal; got {seq:?}"
    );
    assert!(
        !seq.iter().any(|e| e == "turn-anchored"),
        "I2: NO turn-anchored on the async no-wait path (§X.5)"
    );
    // content_sha256 = sha256(message) (the robust pty key, §X.3.4).
    let seen = recs
        .iter()
        .find(|r| r.event == "message-seen" && r.send_id().as_deref() == Some(sid.as_str()))
        .expect("the message-seen record");
    assert_eq!(
        seen.str_field("content_sha256"),
        Some(dispatch::events::sha256_hex(msg.as_bytes())),
        "I2: message-seen content_sha256 == sha256(message)"
    );
    assert_eq!(
        seen.send_id().as_deref(),
        Some(sid.as_str()),
        "send_id present + consistent"
    );

    if let Ok(dir) = std::env::var("DISPATCH_PROOF_DIR") {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            std::path::Path::new(&dir).join("I2-async-pty.jsonl"),
            jail.events_text(),
        );
    }
    assert_no_fault_leakage(&jail.daemon_records());
    assert_canary_absent(&jail, &canary);
    jail.teardown();
}

// ===========================================================================
// Whole-matrix sanity (§3.2): zero pty-write-failed + zero anchor-timeout in an
// N-row's captures (no fault leakage between jails).
// ===========================================================================

/// A clean (N-row) daemon capture must carry NO pty-write-failed record.
fn assert_no_fault_leakage(daemon: &[DaemonEvent]) {
    assert!(
        !daemon
            .iter()
            .any(|e| matches!(e, DaemonEvent::PtyWriteFailed { .. })),
        "N-row daemon capture must have ZERO pty-write-failed (no fault leakage) — got {daemon:?}"
    );
}

// ===========================================================================
// §3.3 — coverage assertion (exhaustive match over BOTH payload enums, no
// wildcard arm; a new event kind breaks the BUILD until assigned a row).
// ===========================================================================

/// A delegation entry: an event kind not produced by THIS matrix, mapped to the
/// in-repo test file + fn that exercises it (verified by the scan below).
struct Delegation {
    file: &'static str,
    test_fn: &'static str,
}

/// EXHAUSTIVE classification of every ENGINE payload kind (events.rs `Payload`):
/// either OWNED (this matrix exercises it) or DELEGATED (named file + fn). The
/// match has NO wildcard arm — adding a Payload variant breaks the build.
fn engine_kind_disposition(p: &dispatch::events::Payload) -> Option<Delegation> {
    use dispatch::events::Payload::*;
    match p {
        // OWNED by this matrix:
        SendInitiated { .. } => None,        // every M-row
        ChunksDelivered { .. } => None,      // M3/M4/N-rows
        TurnAnchored { .. } => None,         // N-twins + R-REC-anchored
        TurnAnchoredMismatch { .. } => None, // M5 + R-REC-truncated
        AnchorTimeout { .. } => None,        // M3/M4
        PendingAbandoned { .. } => None,     // R-REC-abandoned
        // DELEGATED (not produced here; named in-repo carriers):
        ComposerCleared { .. } => Some(Delegation {
            file: "crates/dispatch/src/events.rs",
            test_fn: "g1_representative_of_each_kind_roundtrips",
        }),
        PrimingReadinessTimeout { .. } => Some(Delegation {
            file: "crates/dispatch/tests/ack2_gate.rs",
            test_fn: "g7c_readiness_arm_priming_timeout_no_blind_write",
        }),
        StatusTransition { .. } => Some(Delegation {
            file: "crates/dispatch/tests/ack2_gate.rs",
            test_fn: "g3_seq_sendpty_wait_complete_anchored_with_status_transitions",
        }),
        EventsTruncated => Some(Delegation {
            file: "crates/dispatch/src/events.rs",
            test_fn: "rotation_reserve_band_takes_terminal_only",
        }),
        // §X (3-phase delivery) — not produced by this matrix; DELEGATED to the
        // events.rs unit tests (U1 shape + U4 terminal-class). Emission of these
        // kinds is exercised by the Tier-2 seam integration (relay/pty on-received).
        RelayDelivered { .. } => Some(Delegation {
            file: "crates/dispatch/src/events.rs",
            test_fn: "x3_relay_delivered_key_order_and_nonterminal",
        }),
        MessageSeen { .. } => Some(Delegation {
            file: "crates/dispatch/src/events.rs",
            test_fn: "x3_message_seen_key_order_and_terminal",
        }),
        SeenFailed { .. } => Some(Delegation {
            file: "crates/dispatch/src/events.rs",
            test_fn: "x3_seen_failed_key_order_and_terminal",
        }),
        // R3d recovery-ladder forensics — not produced by this matrix; DELEGATED to
        // the events.rs replay tests (emit the kinds, read the file back, replay).
        RungEntered { .. } | RungSucceeded { .. } | RungTimeout { .. } => Some(Delegation {
            file: "crates/dispatch/src/events.rs",
            test_fn: "r3d_recovery_episode_reconstructs_from_log_alone",
        }),
        RecoveryCrit { .. } => Some(Delegation {
            file: "crates/dispatch/src/events.rs",
            test_fn: "r3d_recovery_crit_episode_reconstructs_from_log",
        }),
    }
}

/// EXHAUSTIVE classification of every DAEMON event kind (qrmux events.rs
/// `DaemonEvent`): OWNED or DELEGATED. No wildcard arm.
fn daemon_kind_disposition(e: &DaemonEvent) -> Option<Delegation> {
    match e {
        // OWNED by this matrix:
        DaemonEvent::PtyBytesWritten { .. } => None, // M3/M4/M5/N-rows
        DaemonEvent::PtyWriteFailed { .. } => None,  // M2
        // DELEGATED:
        DaemonEvent::SessionOpened { .. } => Some(Delegation {
            file: "crates/qrmux/src/events.rs",
            test_fn: "r_epoch_excl_retry",
        }),
        DaemonEvent::SessionClosed { .. } => Some(Delegation {
            file: "crates/qrmux/src/events.rs",
            test_fn: "close_bookend_idempotent",
        }),
        DaemonEvent::EventsTruncated { .. } => Some(Delegation {
            file: "crates/qrmux/src/events.rs",
            test_fn: "r_seq_monotonic_and_suppression_gap",
        }),
        DaemonEvent::Heartbeat { .. } => Some(Delegation {
            file: "crates/qrmux/src/events.rs",
            test_fn: "r_ser_golden_lines_byte_exact",
        }),
    }
}

/// Resolve a source file relative to the crate manifest dir (crates/qd).
fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

/// A delegation is honest iff the named fn exists in the named file AND there is
/// no `#[ignore]` in its attribute window (the ≤6 lines before `fn <name>`).
/// Named honestly (spec §3.3 / red-team R6): weaker teeth than the exhaustive
/// match — a hollowed-out body would pass — it only keeps delegation names from
/// rotting. The match half carries the structural tooth.
fn delegation_is_live(d: &Delegation) -> bool {
    let text = std::fs::read_to_string(repo_path(d.file)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let needle = format!("fn {}", d.test_fn);
    for (i, line) in lines.iter().enumerate() {
        if line.contains(&needle) {
            // Scan the attribute window above for #[ignore].
            let start = i.saturating_sub(6);
            let ignored = lines[start..i].iter().any(|l| l.contains("#[ignore"));
            return !ignored;
        }
    }
    false
}

#[test]
fn coverage_inventory_every_event_kind_exercised() {
    // ENGINE: instantiate one of EVERY Payload variant so the exhaustive match
    // visits each (the structural tooth: a new variant fails to compile here).
    let anchor = dispatch::events::Anchor {
        transcript: "/t".into(),
        start_offset: 0,
        line_index: 0,
    };
    let engine_all: Vec<dispatch::events::Payload> = vec![
        dispatch::events::Payload::SendInitiated {
            send_id: "s".into(),
            verb: "send:pty".into(),
            send_path: "idle".into(),
            content_sha256: sha256_hex(b"x"),
            content_len: 1,
            chunks: 1,
            chunk_sha256s: vec![sha256_hex(b"x")],
            chunk_sha256s_capped: false,
            transcript: None,
            transcript_offset: None,
            // ADD-20 (W2, landed after W3's base): additive field — the coverage
            // instantiation carries it None; the exhaustive-match tooth is the
            // variant list, not field values.
            content_preview: None,
        },
        dispatch::events::Payload::ChunksDelivered {
            send_id: "s".into(),
            chunks_acked: 1,
            ack_source: "input-sent".into(),
        },
        dispatch::events::Payload::TurnAnchored {
            send_id: "s".into(),
            content_sha256: sha256_hex(b"x"),
            anchor: anchor.clone(),
            recovered: false,
            attribution: None,
        },
        dispatch::events::Payload::TurnAnchoredMismatch {
            send_id: "s".into(),
            expected_sha: sha256_hex(b"x"),
            actual_sha: sha256_hex(b"y"),
            expected_len: 2,
            actual_len: 1,
            recovered: false,
            attribution: None,
        },
        dispatch::events::Payload::AnchorTimeout {
            send_id: "s".into(),
            waited_ms: 3000,
        },
        dispatch::events::Payload::PendingAbandoned {
            send_id: "s".into(),
            reason: "recovery-no-candidate".into(),
        },
        dispatch::events::Payload::ComposerCleared {
            send_id: "s".into(),
        },
        dispatch::events::Payload::PrimingReadinessTimeout {
            waited_ms: 40000,
            phase: "pid-file".into(),
        },
        dispatch::events::Payload::StatusTransition {
            status: "idle".into(),
            source: "status-file-poll".into(),
        },
        dispatch::events::Payload::EventsTruncated,
        dispatch::events::Payload::RelayDelivered {
            send_id: "s".into(),
            content_sha256: sha256_hex(b"x"),
        },
        dispatch::events::Payload::MessageSeen {
            send_id: "s".into(),
            content_sha256: sha256_hex(b"x"),
        },
        dispatch::events::Payload::SeenFailed {
            send_id: "s".into(),
            reason: "recipient-gone".into(),
        },
        dispatch::events::Payload::RungEntered {
            session_id: "s".into(),
            rung: "respawn".into(),
        },
        dispatch::events::Payload::RungSucceeded {
            session_id: "s".into(),
            rung: "respawn".into(),
        },
        dispatch::events::Payload::RungTimeout {
            session_id: "s".into(),
            rung: "respawn".into(),
            waited_ms: 1,
        },
        dispatch::events::Payload::RecoveryCrit {
            session_id: "s".into(),
            consecutive_failures: 3,
        },
    ];
    for p in &engine_all {
        if let Some(d) = engine_kind_disposition(p) {
            assert!(
                delegation_is_live(&d),
                "ENGINE kind {} delegated to {}::{} — fn missing or #[ignore]d",
                p.event_name(),
                d.file,
                d.test_fn
            );
        }
    }

    // DAEMON: one of every DaemonEvent variant (the structural tooth on the qrmux
    // enum — a new daemon variant fails to compile here).
    use qrmux::events::{CloseReason, EventMeta};
    let meta = EventMeta {
        session: "s".into(),
        epoch: 1,
        seq: 1,
        ts_ms: 0,
    };
    let daemon_all: Vec<DaemonEvent> = vec![
        DaemonEvent::SessionOpened {
            meta: meta.clone(),
            pid: 1,
            schema_version: 1,
            pid_start_ms: None,
            boot_id: None,
        },
        DaemonEvent::PtyBytesWritten {
            meta: meta.clone(),
            bytes: 1,
            content_sha256: sha256_hex(b"x"),
            content_len: 1,
        },
        DaemonEvent::PtyWriteFailed {
            meta: meta.clone(),
            errno: Some(5),
            error: "io".into(),
            content_sha256: sha256_hex(b"x"),
            content_len: 1,
        },
        DaemonEvent::SessionClosed {
            meta: meta.clone(),
            reason: CloseReason::Killed,
        },
        DaemonEvent::EventsTruncated {
            meta: meta.clone(),
            cap_bytes: 4096,
        },
        DaemonEvent::Heartbeat { meta: meta.clone() },
    ];
    for e in &daemon_all {
        if let Some(d) = daemon_kind_disposition(e) {
            assert!(
                delegation_is_live(&d),
                "DAEMON kind {} delegated to {}::{} — fn missing or #[ignore]d",
                daemon_event_name(e),
                d.file,
                d.test_fn
            );
        }
    }
}

// R-REC recovery rows live in ack3_recovery.rs (sibling test binary). This file
// keeps the injection matrix + the coverage assertion.
