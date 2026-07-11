//! ACK-3 R-REC recovery-read e2e rows (ack3-spec §4) — kill the sending engine
//! mid-wait (SIGKILL, so WatchGuard::Drop does NOT run — the §7 dead-writer gap),
//! then a LATER invocation resolves anchored / truncated / abandoned over the
//! REAL killed-engine artifacts (real send-initiated record incl. offset, real
//! convo JSONL from fakerepl, real pid liveness). The ONLY non-real input is the
//! Clock: `await_received` takes a PUBLIC Clock parameter, injected at
//! `real_now + 31s` so the §7 30s dead-dangling age gate is satisfied via the
//! API's own seam (named honestly — not waited out). A PURE `is_dead_dangling`
//! control proves the gate itself BEFORE the resolution call (no emission).
//!
//! Jail / run_qd helpers MIRROR ack2_gate.rs (duplicated — test binaries cannot
//! import each other; ack3-spec §2 sanctions duplication).

#![allow(clippy::too_many_arguments)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use dispatch::effects::FixedClock;
use dispatch::events::{
    await_received, is_dead_dangling, parse_events, AwaitBudget, AwaitDeps, EventRecord, ReaderCtx,
    Received, RecoveryDeps,
};

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
                    "STALE fakerepl binary at {bin:?} — run: cargo build -p fakerepl"
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
// Jail (duplicated)
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
        let base = PathBuf::from("/tmp/qd-ack3rec");
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

    /// The sessionId-keyed engine events file path.
    fn events_file(&self) -> PathBuf {
        self.ev_dir.join(format!("{}.events.jsonl", self.uuid))
    }

    fn engine_records(&self) -> Vec<EventRecord> {
        parse_events(&std::fs::read_to_string(self.events_file()).unwrap_or_default()).records
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

// ===========================================================================
// qd driver: blocking (run_qd) + child spawn (spawn_qd)
// ===========================================================================

fn build_cmd(jail: &Jail, args: &[&str], extra: &[(&str, String)]) -> Command {
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
    cmd
}

fn run_qd(jail: &Jail, args: &[&str], extra: &[(&str, String)]) -> (i32, String, String) {
    // WP-B-CS-1 (D2): force the INTERACTIVE surface for `start` — this harness pipes
    // stdio (`.output()`), so a bare start auto-detects the HEADLESS surface. These
    // recovery tests exercise the interactive create + -p delivery. Behavior delta
    // (non-TTY `qd start -p` is headless by design now) flagged in the response.
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
    let out = build_cmd(jail, args, extra).output().expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Spawn `qd` as a detached CHILD (handle kept so the test can SIGKILL it).
fn spawn_qd(jail: &Jail, args: &[&str], extra: &[(&str, String)]) -> Child {
    build_cmd(jail, args, extra)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn qd child")
}

/// SIGKILL a child by pid (WatchGuard::Drop cannot run — the §7 dead-writer gap).
fn sigkill(child: &Child) {
    // SAFETY: SIGKILL to a known child pid.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
}

// ===========================================================================
// Recovery deps over the REAL jail files (mirrors PlantedDeps but points at the
// killed engine's actual convo JSONL). now_ms = real_now + 31s (the §7 age gate
// is satisfied via the API seam, not waited out — named honestly).
// ===========================================================================

struct RealJailRecoveryDeps {
    convo: PathBuf,
    now_ms: i64,
}

impl RecoveryDeps for RealJailRecoveryDeps {
    fn read_transcript(&self, _path: &str) -> Option<String> {
        std::fs::read_to_string(&self.convo).ok()
    }
    fn resolve_transcript(&self, _s: Option<&str>, _n: Option<&str>) -> Option<String> {
        // Offset-absent path re-resolves NOW; we hand back the real convo path.
        self.convo.to_str().map(str::to_string)
    }
    fn now_ms(&self) -> i64 {
        self.now_ms
    }
}

struct RealAwaitDeps(RealJailRecoveryDeps);
impl RecoveryDeps for RealAwaitDeps {
    fn read_transcript(&self, p: &str) -> Option<String> {
        self.0.read_transcript(p)
    }
    fn resolve_transcript(&self, s: Option<&str>, n: Option<&str>) -> Option<String> {
        self.0.resolve_transcript(s, n)
    }
    fn now_ms(&self) -> i64 {
        self.0.now_ms()
    }
}
impl AwaitDeps for RealAwaitDeps {
    fn sleep(&self, _ms: u64) {}
}

fn real_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ===========================================================================
// The kill recipe (deterministic, file-keyed — no sleeps-as-logic).
// ===========================================================================

/// Drive one R-REC row: boot, spawn the --wait send as a child, poll the engine
/// file until chunks-delivered appears for the send, SIGKILL the child, return
/// the send-initiated record (the dangle subject). `extra` carries the per-state
/// fakerepl seam env; `busy_ms` widens the kill window for the anchored race.
///
/// Returns `None` if the wait loop ANCHORED before the kill (the fast-turn race);
/// the caller retries once with a longer busy_ms, else FAILS LOUD.
fn drive_and_kill(
    jail: &Jail,
    name: &str,
    msg: &str,
    extra: &[(&str, String)],
) -> Option<EventRecord> {
    let mut env = jail.fakerepl_env(name);
    for (k, v) in extra {
        env.push((k, v.clone()));
    }

    // Pre-create the convo so `send:pty --wait`'s jsonl precondition resolves (the
    // offset snapshot is taken at this pre-existing size; the injected record, if
    // any, lands PAST it). For the anchored/truncated rows fakerepl appends the
    // real user record on submit; for the abandoned row (EAT_INPUT) it never does.
    std::fs::write(
        &jail.convo,
        "{\"type\":\"user\",\"message\":{\"content\":\"seed-prior\"}}\n",
    )
    .unwrap();

    // Boot (idle session), settle.
    let (cb, _o, _e) = run_qd(jail, &["start", name], &env);
    assert_eq!(cb, 0, "R-REC boot succeeds");
    std::thread::sleep(Duration::from_millis(800));

    // Spawn the --wait send as a CHILD; keep the handle.
    let child = spawn_qd(
        jail,
        &["send:pty", name, msg, "--wait", "--timeout", "30"],
        &env,
    );

    // Poll the engine file (500ms, bounded ~20s) until chunks-delivered appears
    // for the send. We key on the LAST send-initiated for send:pty (the boot has
    // no send-initiated; this send is the only one).
    let start = Instant::now();
    let mut killed = false;
    let mut si: Option<EventRecord> = None;
    while start.elapsed() < Duration::from_secs(20) {
        let recs = jail.engine_records();
        // Identify our send's send_id (the last send:pty send-initiated).
        let sid = recs
            .iter()
            .rfind(|r| {
                r.event == "send-initiated" && r.str_field("verb").as_deref() == Some("send:pty")
            })
            .and_then(|r| r.send_id());
        if let Some(sid) = sid {
            // If a TERMINAL already landed for this send → the fast-turn race.
            let terminal = recs.iter().any(|r| {
                r.send_id().as_deref() == Some(&sid)
                    && matches!(
                        r.event.as_str(),
                        "turn-anchored"
                            | "turn-anchored-mismatch"
                            | "anchor-timeout"
                            | "pending-abandoned"
                    )
            });
            if terminal {
                // Anchored/terminal before we could kill — race. Reap + signal retry.
                sigkill(&child);
                let _ = wait_child(child);
                return None;
            }
            let chunks = recs
                .iter()
                .any(|r| r.event == "chunks-delivered" && r.send_id().as_deref() == Some(&sid));
            if chunks {
                // The write is past — kill NOW (Drop must not run).
                sigkill(&child);
                si = recs
                    .into_iter()
                    .find(|r| r.event == "send-initiated" && r.send_id().as_deref() == Some(&sid));
                killed = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = wait_child(child);
    assert!(killed, "R-REC: chunks-delivered never appeared within 20s");
    si
}

/// Reap a child (so it never orphans).
fn wait_child(mut child: Child) -> Option<i32> {
    let _ = child.kill(); // belt: already SIGKILLed, harmless if reaped
    child.wait().ok().and_then(|s| s.code())
}

/// The shared post-kill resolution: assert the dangle, run the PURE
/// is_dead_dangling control (young=false, aged=true), then resolve via
/// await_received with a FixedClock at real_now+31s. Returns the Received verdict
/// + the records AFTER the late event landed.
fn resolve_after_kill(
    jail: &Jail,
    si: &EventRecord,
    send_id: &str,
) -> (Received, Vec<EventRecord>) {
    // Re-read to confirm the dangle: no terminal for this send_id.
    let recs = jail.engine_records();
    assert!(
        dispatch::events::first_terminal_for(&recs, send_id).is_none(),
        "R-REC dangle: no terminal for the SIGKILLed send (Drop did not run) — seq {:?}",
        recs.iter()
            .filter(|r| r.send_id().as_deref() == Some(send_id))
            .map(|r| r.event.clone())
            .collect::<Vec<_>>()
    );

    // REQUIRED pure-predicate control (§4.1): the age gate itself. real_now holds
    // the young dangle (false); real_now+31s trips it (true). No emission (an
    // await-with-real-clock control would emit anchor-timeout and poison the file).
    let real_now = real_now_ms();
    assert!(
        !is_dead_dangling(&recs, si, real_now),
        "control: a fresh dangle is NOT dead-dangling at real_now (age gate held)"
    );
    assert!(
        is_dead_dangling(&recs, si, real_now + 31_000),
        "control: the dangle IS dead-dangling at real_now+31s (age gate trips)"
    );

    // Resolve via the PUBLIC library API: clock fixed at real_now+31s; deps read
    // the REAL convo file; writer/ctx point at the REAL jail event file.
    let clock = FixedClock(real_now + 31_000);
    let deps = RealAwaitDeps(RealJailRecoveryDeps {
        convo: jail.convo.clone(),
        now_ms: real_now + 31_000,
    });
    // events_path joins state_dir + "sessions" + "<key>.events.jsonl"; jail.ev_dir
    // is <QD_HOME>/state/sessions, so the writer/ctx state_dir is <QD_HOME>/state.
    let state_dir = jail.qd_home.join("state");
    let writer = dispatch::events::EventWriter::for_key(
        &state_dir,
        &jail.uuid,
        Some(jail.uuid.clone()),
        None,
    );
    let ctx = ReaderCtx {
        state_dir: &state_dir,
        session_id: Some(&jail.uuid),
        name: None,
    };
    let budget = AwaitBudget {
        poll_ms: 1,
        max_polls: 2,
    };
    let got = await_received(&deps, &clock, &writer, ctx, send_id, budget);
    (got, jail.engine_records())
}

// ===========================================================================
// R-REC-anchored: no seams; the kill lands between chunks-delivered and anchor.
// Verdict: turn-anchored{recovered:true, attribution:"offset"}.
// ===========================================================================

#[test]
fn r_rec_anchored_recovers_via_offset() {
    require_bins();
    let jail = Jail::establish("rra");
    let name = "rra";
    let msg = format!(
        "ACK3-RREC-anchored-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    // Attempt with BUSY_MS=10000; retry ONCE with 20000 on a fast-turn race.
    let si = {
        let extra = vec![("QD_FAKEREPL_BUSY_MS", "10000".to_string())];
        match drive_and_kill(&jail, name, &msg, &extra) {
            Some(si) => si,
            None => {
                // Race: anchored before kill. Retry once with a wider window.
                let jail2 = Jail::establish("rra2");
                let extra2 = vec![("QD_FAKEREPL_BUSY_MS", "20000".to_string())];
                let si = drive_and_kill(&jail2, name, &msg, &extra2).expect(
                    "R-REC-anchored: anchored before kill even at BUSY_MS=20000 — FAIL LOUD",
                );
                let sid = si.send_id().unwrap();
                let (got, recs) = resolve_after_kill(&jail2, &si, &sid);
                assert_anchored_recovery(&got, &recs, &sid);
                jail2.teardown();
                return;
            }
        }
    };
    let sid = si.send_id().unwrap();
    let (got, recs) = resolve_after_kill(&jail, &si, &sid);
    assert_anchored_recovery(&got, &recs, &sid);
    jail.teardown();
}

fn assert_anchored_recovery(got: &Received, recs: &[EventRecord], sid: &str) {
    assert_eq!(
        *got,
        Received::Anchored,
        "R-REC-anchored verdict is Anchored"
    );
    let ta = recs
        .iter()
        .find(|r| r.event == "turn-anchored" && r.send_id().as_deref() == Some(sid))
        .expect("late turn-anchored appended to the real file");
    assert_eq!(
        ta.obj.get("recovered").and_then(|v| v.as_bool()),
        Some(true),
        "recovered:true on the late event"
    );
    assert_eq!(
        ta.str_field("attribution").as_deref(),
        Some("offset"),
        "attribution offset (the send-initiated carried the transcript offset)"
    );
}

// ===========================================================================
// R-REC-truncated: TRUNCATE=1500 + the M5 2100B/3-chunk geometry.
// Verdict: turn-anchored-mismatch{recovered:true, actual_len:1500}.
// ===========================================================================

#[test]
fn r_rec_truncated_recovers_mismatch() {
    require_bins();
    let jail = Jail::establish("rrt");
    let name = "rrt";
    // 2100-byte ASCII message (3 chunks → chunk-prefix match keeps 1 full chunk).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let head = format!("ACK3-RREC-trunc-{nanos} ");
    let msg = format!("{head}{}", "X".repeat(2100 - head.len()));
    assert_eq!(msg.len(), 2100);

    let extra = vec![
        ("QD_FAKEREPL_TRUNCATE_USER_RECORD_BYTES", "1500".to_string()),
        ("QD_FAKEREPL_BUSY_MS", "10000".to_string()),
    ];
    let si = drive_and_kill(&jail, name, &msg, &extra)
        .expect("R-REC-truncated: kill landed after chunks-delivered (no fast-turn race expected with BUSY_MS=10000)");
    let sid = si.send_id().unwrap();
    let (got, recs) = resolve_after_kill(&jail, &si, &sid);

    assert_eq!(
        got,
        Received::AnchoredMismatch,
        "R-REC-truncated verdict is AnchoredMismatch"
    );
    let mm = recs
        .iter()
        .find(|r| r.event == "turn-anchored-mismatch" && r.send_id().as_deref() == Some(&sid))
        .expect("late turn-anchored-mismatch appended");
    assert_eq!(
        mm.obj.get("recovered").and_then(|v| v.as_bool()),
        Some(true),
        "recovered:true"
    );
    assert_eq!(
        mm.u64_field("actual_len"),
        Some(1500),
        "actual_len 1500 (the truncated prefix)"
    );
    jail.teardown();
}

// ===========================================================================
// R-REC-empty-window (R6 (b), seam ruling 01KX8MDPDX): EAT_INPUT=1 → the recipient
// wrote NO user record past the send's offset → an EMPTY window. An empty window is
// UNDETERMINED (still growable — the recipient hasn't demonstrably progressed past
// the send), so recovery mints NO terminal: it must NOT false-abandon (the pre-R6
// behavior this test formerly asserted, `recovery-no-candidate`, was exactly the
// systematic false-abandon R6 removes). Driven through the bounded await, the empty
// window never resolves → the await's §8 budget exhausts to a POSITIVE anchor-timeout
// (the await's own timeout, C6-deferred — NOT a recovery foreclosure).
// ===========================================================================

#[test]
fn r_rec_empty_window_stays_recoverable() {
    require_bins();
    let jail = Jail::establish("rrab");
    let name = "rrab";
    let msg = format!(
        "ACK3-RREC-empty-window-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    let extra = vec![
        ("QD_FAKEREPL_EAT_INPUT", "1".to_string()),
        ("QD_FAKEREPL_BUSY_MS", "10000".to_string()),
    ];
    let si = drive_and_kill(&jail, name, &msg, &extra)
        .expect("R-REC-empty-window: kill landed after chunks-delivered");
    let sid = si.send_id().unwrap();
    let (got, recs) = resolve_after_kill(&jail, &si, &sid);

    // R6 (b): an empty window is recoverable, not abandoned. Recovery emits nothing;
    // the bounded await exhausts its budget to a POSITIVE anchor-timeout.
    assert_eq!(
        got,
        Received::AnchorTimeout,
        "R6 (b): an empty window stays recoverable → the bounded await times out positively, NOT Abandoned"
    );
    // Recovery must NOT have minted a pending-abandoned (no false-abandon of an empty,
    // still-growable window).
    assert!(
        !recs
            .iter()
            .any(|r| r.event == "pending-abandoned" && r.send_id().as_deref() == Some(&sid)),
        "empty window must NOT foreclose with pending-abandoned (R6 (b)); recs: {:?}",
        recs.iter().map(|r| r.event.clone()).collect::<Vec<_>>()
    );
    // The only terminal for the send is the await's positive §8 anchor-timeout.
    assert!(
        recs.iter()
            .any(|r| r.event == "anchor-timeout" && r.send_id().as_deref() == Some(&sid)),
        "the bounded await emits a positive anchor-timeout on budget exhaustion"
    );
    jail.teardown();
}
