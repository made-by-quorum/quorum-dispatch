//! ACP-CHAOS — the CONCURRENT fault fleet hardening the ALREADY-LANDED shared
//! crate-backed ACP driver (`provider/acp/client.rs` on `agent-client-protocol
//! =1.0.1`) + the residence layer (`acp_residence.rs` / `create_daemon.rs`) under
//! induced crashes / wedges / PID-reuse, driven against LIVE `opencode` (free-tier,
//! zero-spend) as a MULTI-DAEMON fleet in ONE isolated throwaway `QD_HOME`.
//!
//! Posture (P-final hardening atomic, acp-adoption child-3): inject a fault, then
//! prove the driver+residence SURVIVE — NO stale-endpoint latch, NO wedge (fail-FAST
//! within a preregistered ceiling), NO orphaned process — with a per-class
//! FIRE-WITNESS proving the fault ACTUALLY FIRED (else a "clean" survival is VACUOUS
//! and does not count). Every assertion is AT SOURCE (jail-scoped `ps`/`pgrep`, the
//! ws endpoint, the production `cmdline_is_our_acp_daemon` predicate) — NEVER a return
//! string.
//!
//! Separate from `tests/faultinj.rs` BY CONSTRUCTION (faultinj is characterized
//! WANDERING-FLAKY under load on this box; this fleet is heavier load — it does NOT
//! extend faultinj and inherit the wander). It REUSES only the PATTERN of the disabled
//! `tests/codex_chaos.rs` (jail-scoped instance-addressed probes, the panic-safe group
//! reaper, per-class evidence bundle, the setsid-escape CHARACTERIZATION) — retargeted
//! to opencode via `qd acp-daemon --bridge-cmd opencode`.
//!
//! ## The five fault classes (each: OBSERVED-EFFECT-AT-SOURCE + FIRE-WITNESS + the
//! ## CROSS-CONTAMINATION assertion — one daemon's fault never latches/orphans/wedges
//! ## a SIBLING)
//!   F1  crash → no stale-endpoint latch + rebind.
//!   F2  PID-reuse → impostor rejected (the recorded `--listen` endpoint keys identity,
//!       not pid-liveness).
//!   F3a wedge-AT-connect (5s connect guard; ceiling 10s) + paired UNGUARDED-CONTROL arm.
//!   F3b wedge-AFTER-connect / request-hang (30s request guard; ceiling 60s) + paired
//!       UNGUARDED-CONTROL arm; run at {1 hung + 2 live siblings} (mh-coord-14 ruled).
//!   F4  pgid-teardown → 0 orphans in the pgid-reachable set + escape-boundary
//!       CHARACTERIZATION.
//!
//! ## Convergence
//! Each class converges at N=3 CONSECUTIVE non-vacuous clean rounds (subtle findings
//! cluster in the tail). Rounds are the cargo invocation's job (evidence dir per round
//! via `QD_ACP_CHAOS_EVIDENCE` / `QD_ACP_CHAOS_ROUND`); a single `#[test]` IS one round.
//!
//! ## HARD LIMITS (binding)
//!   * NO LIVE CLAUDE TURN ever — the live-agent fault dimension is opencode ONLY.
//!   * ISOLATED throwaway `QD_HOME` (`mktemp`-style under CARGO_TARGET_TMPDIR); the
//!     session-id env vars are SCRUBBED; NEVER the live `~/.quorum` / `~/.claude`.
//!   * Evidence OUTSIDE cargo `target/`, under `ws/quorum/underway/multi-harness/
//!     evidence/hardening/`.
//!   * All tests are `#[ignore]` — deliberate-only: `cargo test -p quorum-dispatch
//!     --test acp_chaos -- --ignored` with `QD_ACP_CHAOS_LIVE=1` (they spawn real
//!     `opencode acp`). This hard-gate keeps the destructive group-reaping path off any
//!     acceptance / zero-NEW run.
//!
//! ## AT-SOURCE ENVIRONMENT FINDING (baked here, surfaced to the oracle)
//! On THIS box `opencode` (1.17.10) is a NATIVE bun-compiled ELF binary
//! (`opencode.exe`), NOT the node-wrapper the migration-spec §7 anticipates. So the
//! "node wrapper setsids an internal HTTP-server grandchild OUT of the group" escape
//! vector may be ABSENT in this build. F4b therefore CHARACTERIZES the real process
//! tree empirically (does an escapee exist? its lifecycle?) rather than assuming the
//! node model.

#![allow(clippy::needless_range_loop)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dispatch::acp_residence::cmdline_is_our_acp_daemon;
use dispatch::create_daemon::{real_cmdline_probe, reap_zombie, DaemonSpawner, RealDaemonSpawner};
use dispatch::effects::is_pid_alive;
use dispatch::provider::acp::{AcpClient, AcpConnection};

// ===========================================================================
// Preregistered ceilings — each a single NAMED CONSTANT (one-line change if a
// number moves). Derived AT SOURCE from the production guards.
// ===========================================================================

/// F3a wedge-at-connect ceiling. Governed by the 5s connect timeout
/// (`send_relay.rs:596` / `wait.rs:481`; `AcpConnection::connect` bounds TCP-connect +
/// ws-handshake I/O by that `timeout`, `wire.rs:425-436`). 2× connect_timeout + fleet
/// scheduling margin.
const F3A_CONNECT_CEILING: Duration = Duration::from_secs(10);

/// F3b wedge-after-connect / request-hang ceiling. Governed by the post-connect
/// `DEFAULT_REQUEST_TIMEOUT = 30s` (`wire.rs:63`, applied in `read_response` via the
/// per-connection `request_timeout` Cell, `wire.rs:442,507`). 2× request_timeout.
const F3B_REQUEST_CEILING: Duration = Duration::from_secs(60);

/// The production connect timeout the driving verbs pass (`Duration::from_secs(5)` at
/// `send_relay.rs:596` / `wait.rs:481`). The GUARDED arm uses exactly this.
const PROD_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Readiness budget for a fresh opencode daemon (init + session/new establish). The
/// opencode live smoke uses 60s (`acp_opencode_live.rs:236`).
const READINESS_BUDGET: Duration = Duration::from_secs(60);

/// F3b daemon-count (mh-coord-14 RULED): 1 genuine post-connect hang + 2 live siblings.
/// The rider-1 structural finding (impossible-by-construction resource-exhaustion)
/// licenses the {1 hung + 1 sibling} minimum; 2 siblings is a robust cross-contamination
/// signal at zero extra hang-time (siblings are healthy, respond fast).
const F3B_SIBLINGS: usize = 2;

/// The session-id env vars scrubbed from every jail (charge HARD LIMIT): a fleet drive
/// must never inherit the enclosing session's identity.
const SCRUB_ENV: &[&str] = &[
    "QD_SESSION_ID",
    "SB_SESSION_ID",
    "QD_BOOT_AWAIT_RELAY",
    "CLAUDE_CODE_SESSION_ID",
];

// ===========================================================================
// Gating — deliberate-only, live-only. NO SKIP-as-ok: a gated early-return is NOT
// a pass; when we run for convergence we set the gate and confirm no SKIP line.
// ===========================================================================

/// The live gate: `QD_ACP_CHAOS_LIVE=1` (spawns real `opencode acp`).
fn live() -> bool {
    std::env::var("QD_ACP_CHAOS_LIVE").as_deref() == Ok("1")
}

/// `opencode` on PATH (the bridge the fleet drives).
fn opencode_on_path() -> bool {
    Command::new("opencode")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The `qd` binary under test (Cargo sets this for the freshly-built dev binary —
/// package `quorum-dispatch`, bin `qd`; the same handle `acp_opencode_live.rs:200`
/// uses).
fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

// ===========================================================================
// Evidence bundle (OUTSIDE cargo target/) — the reconstructable primary evidence
// the acceptance oracle rules on. Root: `QD_ACP_CHAOS_EVIDENCE` or
// `$HOME/work/ws/quorum/underway/multi-harness/evidence/hardening`; per-class +
// per-round subdir.
// ===========================================================================

fn evidence_root() -> PathBuf {
    if let Ok(p) = std::env::var("QD_ACP_CHAOS_EVIDENCE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join("work/ws/quorum/underway/multi-harness/evidence/hardening")
}

/// Per-class, per-round bundle dir. Round from `QD_ACP_CHAOS_ROUND` (the cargo sets it,
/// e.g. `round1`); default `round-adhoc`.
fn bundle(class: &str) -> PathBuf {
    let round = std::env::var("QD_ACP_CHAOS_ROUND").unwrap_or_else(|_| "round-adhoc".to_string());
    let dir = evidence_root().join(class).join(round);
    std::fs::create_dir_all(&dir).unwrap();
    eprintln!("ACP-CHAOS EVIDENCE [{class}]: {}", dir.display());
    dir
}

fn ev_text(b: &Path, name: &str, body: &str) {
    let _ = std::fs::write(b.join(name), body);
}

/// Append a line to a running log in the bundle AND to stderr (RUN-not-read).
fn ev_line(b: &Path, name: &str, line: &str) {
    eprintln!("{line}");
    let path = b.join(name);
    let mut prev = std::fs::read_to_string(&path).unwrap_or_default();
    prev.push_str(line);
    prev.push('\n');
    let _ = std::fs::write(&path, prev);
}

// ===========================================================================
// At-source process / pgid probes (codex_chaos PATTERN; `-x`/env-scoped, never a
// global name-kill). Integration tests cannot use `libc` (a normal-not-dev dep), so
// signals shell out to `kill`; the PRODUCTION teardown ladder is invoked via the lib
// `RealDaemonSpawner.kill` (which owns libc internally).
// ===========================================================================

/// The process-group id of a pid (`ps -o pgid=`). `None` if the pid is gone — the F1
/// crash fire-witness (daemon genuinely dead, not a self-exit).
fn pgid_of(pid: i64) -> Option<i64> {
    let out = Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse::<i64>().ok()
}

/// The full argv of a pid (`ps -o args=`). Empty when the pid is gone.
fn proc_args(pid: i64) -> String {
    Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The scheduler state of a pid (`ps -o stat=`; the leading char: `R`/`S`/`T`/`Z`…).
/// `T` = stopped — the F3a/F3b wedge fire-witness that a SIGSTOP genuinely landed.
fn proc_state(pid: i64) -> String {
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// ALL pids whose `/proc/<pid>/environ` (via `ps eww`) carries THIS jail's
/// `QD_HOME=<qd_home>` — instance-addressed (never a global kill). Captures the daemon
/// AND its opencode bridge child (which inherits QD_HOME: `run_connection` only
/// `env_remove`s BRIDGE_ENV_STRIP, `client.rs:200`). `-A` then env-filter, exactly the
/// codex_chaos `CODEX_HOME=` needle discipline.
fn jail_pids(qd_home: &Path) -> Vec<i64> {
    let needle = format!("QD_HOME={}", qd_home.display());
    let Ok(out) = Command::new("ps").args(["-A", "-o", "pid="]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse::<i64>().ok())
        .filter(|&pid| {
            Command::new("ps")
                .args(["eww", "-p", &pid.to_string()])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains(&needle))
                .unwrap_or(false)
        })
        .collect()
}

/// Shell a signal by NAME to a single pid (`kill -<SIG> <pid>`). Returns whether the
/// `kill` reported success (the SIGKILL-delivery fire-witness for F1).
fn signal(pid: i64, sig: &str) -> bool {
    Command::new("kill")
        .args([&format!("-{sig}"), &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Wait (bounded) for a pid to go dead (kernel settle after a kill).
fn wait_dead(pid: i64) {
    for _ in 0..40 {
        if !is_pid_alive(pid as i32) {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// SIGKILL a daemon WE spawned (`Command::spawn`), then REAP its zombie so the pid
/// TRULY disappears. A crashed child is a ZOMBIE (defunct) until its parent waits: `ps`
/// still lists it (with a pgid) and `kill(pid,0)` still succeeds, so a bare `pgid_of ==
/// None` / `is_pid_alive == false` fire-witness would falsely fail even though the
/// SIGKILL landed. We loop the production `reap_zombie` (WNOHANG waitpid) until the pid
/// is genuinely gone — so the fire-witness reads a reaped-and-gone pid, not a defunct
/// entry. Returns whether the SIGKILL was delivered. (F1/F2 only; F3a/F3b use STOP/CONT,
/// F4 teardown reaps via `RealDaemonSpawner.kill`.)
fn crash_and_reap(pid: i64) -> bool {
    let killed = signal(pid, "KILL");
    for _ in 0..40 {
        reap_zombie(pid as i32);
        if !is_pid_alive(pid as i32) && pgid_of(pid).is_none() {
            return killed;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    killed
}

/// Panic-safe group reaper: SIGKILL each recorded pid AND its process group on drop,
/// so a mid-test panic never leaks a detached opencode daemon / bridge child. `--`
/// forces `-{pid}` to be a process-GROUP operand (never an option-misparse into
/// `kill(-1)` — the audit-proven 2026-06-30 outage); pid>1 guards pgid 0 / -1.
struct GroupReaper(Arc<Mutex<Vec<i64>>>);
impl Drop for GroupReaper {
    fn drop(&mut self) {
        for &pid in self.0.lock().unwrap().iter() {
            if pid > 1 {
                let _ = Command::new("kill")
                    .args(["-9", "--", &format!("-{pid}")])
                    .status();
                let _ = Command::new("kill")
                    .args(["-9", "--", &pid.to_string()])
                    .status();
            }
        }
    }
}

// ===========================================================================
// The fleet: isolated jail + multi-daemon opencode spawn (modelled on
// `acp_opencode_live.rs::opencode_live_daemon_smoke_cross_process`, the ACCEPTED
// cross-process residence recipe). Each daemon is its OWN process group
// (`process_group(0)`) so the F4 `-pgid` teardown reaps its subtree — faithfully
// mirroring the production detached spawn (S2: a reuse of `RealDaemonSpawner`,
// `acp_residence.rs`).
// ===========================================================================

/// An isolated throwaway jail under CARGO_TARGET_TMPDIR (workspace tree, never /tmp,
/// never the live `~/.quorum`). Holds ONE `QD_HOME` shared by the whole fleet.
struct Jail {
    root: PathBuf,
    qd_home: PathBuf,
    cwd: PathBuf,
}

impl Jail {
    fn make(tag: &str) -> Jail {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("acpchaos-{tag}-{}-{nanos}", std::process::id()));
        // A monotonic counter suffix so two calls in one process never collide.
        let root = uniquify(root);
        let qd_home = root.join("qd-home");
        let cwd = root.join("work");
        std::fs::create_dir_all(&qd_home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        Jail { root, qd_home, cwd }
    }

    /// Prove the sandbox AT SOURCE: the jail is under CARGO_TARGET_TMPDIR and is NOT
    /// the real `~/.quorum` / `~/.claude`. Panics if a jail ever resolves the live
    /// fabric (the `assert_not_real_home` discipline).
    fn assert_sandbox(&self, b: &Path) {
        let home = std::env::var("HOME").unwrap_or_default();
        let real_quorum = PathBuf::from(&home).join(".quorum");
        let real_claude = PathBuf::from(&home).join(".claude");
        assert_ne!(self.qd_home, real_quorum, "jail QD_HOME must NOT be the live ~/.quorum");
        assert!(
            !self.qd_home.starts_with(&real_quorum) && !self.qd_home.starts_with(&real_claude),
            "jail QD_HOME {:?} must not sit under the live fabric",
            self.qd_home
        );
        let tmp_root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        assert!(
            self.root.starts_with(&tmp_root),
            "jail must live under CARGO_TARGET_TMPDIR, got {:?}",
            self.root
        );
        ev_text(
            b,
            "sandbox-at-source.txt",
            &format!(
                "QD_HOME={}\ncwd={}\nunder CARGO_TARGET_TMPDIR={}\nNOT ~/.quorum={} NOT ~/.claude={}\n\
                 scrubbed env vars={:?}\n",
                self.qd_home.display(),
                self.cwd.display(),
                tmp_root.display(),
                real_quorum.display(),
                real_claude.display(),
                SCRUB_ENV,
            ),
        );
    }
}

/// A monotonic within-process disambiguator so two `Jail::make` in one test never
/// collide (Instant::elapsed can repeat at coarse clocks).
fn uniquify(mut p: PathBuf) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let name = format!("{}-{n}", p.file_name().unwrap().to_string_lossy());
    p.set_file_name(name);
    p
}

/// A spawned fleet member: the resident `qd acp-daemon` (opencode bridge), its pid, its
/// own pgid, and the ws endpoint we launched it with (`--listen`).
struct Daemon {
    name: String,
    pid: i64,
    pgid: i64,
    url: String,
}

impl Daemon {
    /// Is this daemon's recorded endpoint LIVE + OURS (the production reconnect
    /// re-check): connect-success is liveness, the cmdline `--listen <endpoint>` +
    /// pid-liveness is IDENTITY (`send_relay.rs` gate). Returns (connectable, ours).
    fn endpoint_ours(&self) -> (bool, bool) {
        let connectable = AcpConnection::connect(&self.url, PROD_CONNECT_TIMEOUT)
            .ok()
            .and_then(|c| c.status_session_id().ok())
            .flatten()
            .is_some();
        let cmdline = real_cmdline_probe(self.pid);
        let ours = is_pid_alive(self.pid as i32)
            && cmdline_is_our_acp_daemon(cmdline.as_deref(), Some(self.url.as_str()));
        (connectable, ours)
    }
}

/// Allocate a free loopback port OUTSIDE the relay-probe range 8900-9000 (the fleet
/// lesson `create_daemon.rs::real_alloc_port` guards). Bind :0, take the port, drop.
fn alloc_port() -> u16 {
    for _ in 0..64 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        if !(8900..=9000).contains(&p) {
            return p;
        }
    }
    panic!("could not allocate a port outside 8900-9000");
}

/// Spawn one resident `qd acp-daemon` fronting real `opencode acp`, in its OWN process
/// group, under the jail's throwaway QD_HOME with the session-id env vars SCRUBBED.
/// Polls to readiness (a `status` session id) — proving the opencode session
/// established (the F3b "connection genuinely established" precondition). Registers the
/// pid+pgid with the reaper belt.
fn spawn_daemon(jail: &Jail, name: &str, reaper: &Arc<Mutex<Vec<i64>>>, b: &Path) -> Daemon {
    let port = alloc_port();
    let url = format!("ws://127.0.0.1:{port}");
    let log = jail.root.join(format!("acp-daemon-{name}.log"));
    let out = std::fs::File::create(&log).unwrap();

    // Bind the cwd as an owned String so every array element is `&String` (the proven
    // `acp_opencode_live.rs:220` arg shape — LUB-coerces to `&str`, no `&Cow` in the mix).
    let cwd_s = jail.cwd.to_string_lossy().to_string();
    let mut cmd = Command::new(qd_bin());
    cmd.args([
        "acp-daemon",
        "--listen",
        &url,
        "--cwd",
        &cwd_s,
        "--bridge-cmd",
        "opencode",
        "--bridge-arg",
        "acp",
    ])
    .env("QD_HOME", &jail.qd_home)
    .stdin(Stdio::null())
    .stdout(Stdio::from(out.try_clone().unwrap()))
    .stderr(Stdio::from(out))
    .current_dir(&jail.cwd);
    for v in SCRUB_ENV {
        cmd.env_remove(v);
    }
    // Own process group (S2 detached-spawn discipline) → the `-pgid` teardown reaps the
    // daemon + its opencode bridge child together (F4).
    cmd.process_group(0);
    let child = cmd.spawn().expect("spawn qd acp-daemon");
    let pid = child.id() as i64;
    reaper.lock().unwrap().push(pid);
    // We do NOT hold the Child (the daemon is detached + owned by the OS after
    // process_group(0)); leak the handle so Drop does not reap it out from under us.
    std::mem::forget(child);

    // Readiness poll — connect + status until the opencode session establishes.
    let deadline = Instant::now() + READINESS_BUDGET;
    let mut ready = false;
    while Instant::now() < deadline {
        if let Ok(c) = AcpConnection::connect(&url, Duration::from_millis(500)) {
            if let Ok(Some(_sid)) = c.status_session_id() {
                ready = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let pgid = pgid_of(pid).unwrap_or(pid);
    ev_line(
        b,
        "fleet.txt",
        &format!("daemon {name}: pid={pid} pgid={pgid} url={url} ready={ready} log={}", log.display()),
    );
    assert!(
        ready,
        "daemon {name} (pid {pid}) not ready in {READINESS_BUDGET:?} — see {}",
        log.display()
    );
    Daemon { name: name.to_string(), pid, pgid, url }
}

/// Measure how long a RAW blocking socket read against `addr` stays blocked, capped at
/// `cap` (the UNGUARDED-CONTROL arm: absent the production timeout, the same camp/hang
/// blocks past the ceiling). We open a raw TCP connection (succeeds into the kernel
/// accept backlog even when the daemon is stopped), write a probe, then a blocking
/// `read()` with NO deadline in a helper thread. If it has not returned within `cap`,
/// the read is still blocked → we return `cap` (proving "≥ cap", chosen > ceiling) and
/// detach the thread (its socket drops at teardown). A prompt EOF/reset instead returns
/// the (short) elapsed — which would FAIL the "would hang past ceiling" assertion,
/// correctly (the camp was not genuine).
fn measure_unguarded_block(addr: &str, cap: Duration) -> Duration {
    let addr = addr.to_string();
    let (tx, rx) = std::sync::mpsc::channel::<Duration>();
    std::thread::spawn(move || {
        let sockaddr = addr.trim_start_matches("ws://");
        let start = Instant::now();
        // Raw connect with NO handshake deadline (the revert of the production bound).
        match TcpStream::connect(sockaddr) {
            Ok(mut s) => {
                // A minimal ws-upgrade request; the camped/stopped serve loop never
                // reads/answers it, so the response read blocks.
                let _ = s.write_all(
                    b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                      Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
                );
                let mut buf = [0u8; 64];
                let _ = s.read(&mut buf); // NO read timeout → blocks while camped
                let _ = tx.send(start.elapsed());
            }
            Err(_) => {
                let _ = tx.send(start.elapsed());
            }
        }
    });
    match rx.recv_timeout(cap) {
        Ok(elapsed) => elapsed,           // the read returned before the cap
        Err(_) => cap,                    // still blocked at the cap → "≥ cap > ceiling"
    }
}

// ===========================================================================
// CLASS F1 — crash → no stale-endpoint latch + rebind.
// ===========================================================================

#[test]
#[ignore = "destructive live chaos — deliberate only: cargo test -p quorum-dispatch --test acp_chaos -- --ignored (+ QD_ACP_CHAOS_LIVE=1)"]
fn f1_crash_no_stale_latch_and_rebind() {
    if !live() {
        eprintln!("QD_ACP_CHAOS_LIVE != 1 — F1 not run (NOT a pass: no fault captured)");
        return;
    }
    assert!(opencode_on_path(), "opencode must be on PATH for the live fleet");
    let b = bundle("f1-crash-rebind");
    let reaper = Arc::new(Mutex::new(Vec::<i64>::new()));
    let _belt = GroupReaper(reaper.clone());

    let jail = Jail::make("f1");
    jail.assert_sandbox(&b);

    // FLEET: victim A + sibling B in ONE QD_HOME (concurrent core).
    let a = spawn_daemon(&jail, "A-victim", &reaper, &b);
    let sib = spawn_daemon(&jail, "B-sibling", &reaper, &b);

    // Pre-crash: A's endpoint answers + is OURS.
    let (a_conn0, a_ours0) = a.endpoint_ours();
    assert!(a_conn0 && a_ours0, "A endpoint live + ours before crash");

    // FIRE-WITNESS: SIGKILL A (a TRUE crash — pure SIGKILL, no qd stop / SIGTERM grace),
    // delivery captured; reap the zombie so the pid truly disappears; then prove A is
    // GENUINELY dead (pgid gone), not a self-exit.
    let killed = crash_and_reap(a.pid);
    let a_pgid_gone = pgid_of(a.pid).is_none();
    assert!(killed, "SIGKILL delivery to A captured (fire-witness a)");
    assert!(a_pgid_gone, "A pgid gone post-crash — daemon genuinely dead, not a self-exit (fire-witness b)");
    assert!(!is_pid_alive(a.pid as i32), "A pid dead");

    // NO STALE-ENDPOINT LATCH: the recorded endpoint is NOT re-served, and the
    // production reconnect re-check REJECTS the dead daemon (cmdline probe None →
    // cmdline_is_our_acp_daemon false → would not latch).
    let (a_conn1, a_ours1) = a.endpoint_ours();
    assert!(!a_conn1, "crashed A endpoint no longer served (no stale endpoint answering)");
    assert!(!a_ours1, "reconnect re-check REJECTS crashed A → no stale-endpoint latch");

    // REBIND: a fresh daemon re-addresses cleanly on a new endpoint.
    let a2 = spawn_daemon(&jail, "A2-rebind", &reaper, &b);
    let (a2_conn, a2_ours) = a2.endpoint_ours();
    assert!(a2_conn && a2_ours, "rebound daemon answers + reconnect re-check ACCEPTS it");
    assert_ne!(a2.pid, a.pid, "rebind is a NEW daemon (distinct pid)");

    // CROSS-CONTAMINATION: the SIBLING is untouched by A's crash + rebind.
    let (sib_conn, sib_ours) = sib.endpoint_ours();
    assert!(sib_conn && sib_ours, "sibling B endpoint unaffected by A's crash (no cross-contamination)");

    ev_text(
        &b,
        "f1-at-source.txt",
        &format!(
            "A: pid={} pgid={} url={}\n\
             pre-crash: A live+ours = {a_conn0}/{a_ours0}\n\
             FIRE-WITNESS: SIGKILL delivered = {killed}; A pgid gone (genuinely dead) = {a_pgid_gone}\n\
             no-stale-latch: crashed A served = {a_conn1}; reconnect re-check ours = {a_ours1} (both false)\n\
             REBIND: A2 pid={} url={} answers+ours = {a2_conn}/{a2_ours}\n\
             CROSS-CONTAMINATION: sibling B live+ours = {sib_conn}/{sib_ours} (unaffected)\n",
            a.pid, a.pgid, a.url, a2.pid, a2.url,
        ),
    );
}

// ===========================================================================
// CLASS F2 — PID-reuse → impostor rejected. The production identity fence keys on the
// recorded `--listen <endpoint>` in the live cmdline, NOT pid-liveness: a live process
// standing where a crashed daemon's (pid,endpoint) record points is REJECTED.
//
// HONEST CONSTRUCTION (disclosed, not a SKIP): forcing the OS to re-hand the EXACT
// numeric pid is infeasible under Linux pid monotonicity (it would require wrapping
// pid_max ≈ millions of spawns). The guard defends the threat regardless — it
// discriminates on cmdline+endpoint, so a REAL live foreign process occupying the
// record's pid role is the faithful adversary. We prove the guard rejects it AT SOURCE.
// ===========================================================================

#[test]
#[ignore = "destructive live chaos — deliberate only: cargo test -p quorum-dispatch --test acp_chaos -- --ignored (+ QD_ACP_CHAOS_LIVE=1)"]
fn f2_pid_reuse_impostor_rejected() {
    if !live() {
        eprintln!("QD_ACP_CHAOS_LIVE != 1 — F2 not run (NOT a pass)");
        return;
    }
    assert!(opencode_on_path(), "opencode must be on PATH");
    let b = bundle("f2-pid-reuse");
    let reaper = Arc::new(Mutex::new(Vec::<i64>::new()));
    let _belt = GroupReaper(reaper.clone());

    let jail = Jail::make("f2");
    jail.assert_sandbox(&b);

    // FLEET: victim A + sibling B.
    let a = spawn_daemon(&jail, "A-victim", &reaper, &b);
    let sib = spawn_daemon(&jail, "B-sibling", &reaper, &b);
    let recorded_endpoint = a.url.clone();

    // Crash A → free its pid (the pre-reuse condition). Reap the zombie so the pid is
    // genuinely gone (a Command::spawn child is defunct until waited).
    crash_and_reap(a.pid);
    assert!(!is_pid_alive(a.pid as i32), "A crashed (pid freed)");

    // Spawn a live FOREIGN process (the impostor) — a `sleep` that is provably NOT our
    // acp-daemon (distinct comm/cmdline). It occupies the (pid,endpoint) RECORD's role:
    // the record now points at a live pid running something else.
    let mut impostor = Command::new("sleep")
        .arg("3600")
        .spawn()
        .expect("spawn impostor");
    let impostor_pid = impostor.id() as i64;
    reaper.lock().unwrap().push(impostor_pid);
    let impostor_cmdline = proc_args(impostor_pid);

    // FIRE-WITNESS: the impostor is a provably DIFFERENT process — alive, distinct
    // cmdline, does NOT carry the acp-daemon marker or the recorded endpoint.
    let impostor_alive = is_pid_alive(impostor_pid as i32);
    let distinct = impostor_cmdline.contains("sleep")
        && !impostor_cmdline.contains("acp-daemon")
        && !impostor_cmdline.contains(&recorded_endpoint);
    assert!(impostor_alive, "impostor alive (a live foreign pid)");
    assert!(distinct, "impostor is provably a DIFFERENT process (distinct comm/cmdline): {impostor_cmdline:?}");

    // THE GUARD REJECTS IT: the production identity check on the impostor's live cmdline
    // against the recorded endpoint returns FALSE → no stale latch onto a reused pid.
    let probe = real_cmdline_probe(impostor_pid);
    let latched = is_pid_alive(impostor_pid as i32)
        && cmdline_is_our_acp_daemon(probe.as_deref(), Some(recorded_endpoint.as_str()));
    assert!(
        !latched,
        "PID-reuse defense: a live foreign pid at the recorded (pid,endpoint) role is REJECTED (no latch)"
    );

    // CROSS-CONTAMINATION: sibling B unaffected.
    let (sib_conn, sib_ours) = sib.endpoint_ours();
    assert!(sib_conn && sib_ours, "sibling B unaffected by A's crash + the impostor");

    let _ = impostor.kill();
    let _ = impostor.wait();

    ev_text(
        &b,
        "f2-at-source.txt",
        &format!(
            "recorded (crashed A): pid={} endpoint={recorded_endpoint}\n\
             impostor: pid={impostor_pid} alive={impostor_alive} cmdline={impostor_cmdline:?}\n\
             FIRE-WITNESS distinct-process = {distinct}\n\
             GUARD cmdline_is_our_acp_daemon(impostor cmdline, recorded endpoint) latched = {latched} (false = rejected)\n\
             CROSS-CONTAMINATION: sibling B live+ours = {sib_conn}/{sib_ours}\n\
             HONESTY NOTE: exact numeric pid-reuse of {} is infeasible to force under Linux pid \
             monotonicity; the guard keys on cmdline+endpoint, so a real live foreign process is the \
             faithful adversary — rejection proven at source.\n",
            a.pid, a.pid,
        ),
    );
}

// ===========================================================================
// CLASS F3a — wedge-AT-connect (5s connect guard; ceiling 10s) + UNGUARDED-CONTROL arm.
// A wedged daemon that won't service NEW connections (SIGSTOP its serve loop); a
// concurrent verb (the production 5s connect) fails FAST within the ceiling.
// ===========================================================================

#[test]
#[ignore = "destructive live chaos — deliberate only: cargo test -p quorum-dispatch --test acp_chaos -- --ignored (+ QD_ACP_CHAOS_LIVE=1)"]
fn f3a_wedge_at_connect_fails_fast() {
    if !live() {
        eprintln!("QD_ACP_CHAOS_LIVE != 1 — F3a not run (NOT a pass)");
        return;
    }
    assert!(opencode_on_path(), "opencode must be on PATH");
    let b = bundle("f3a-wedge-at-connect");
    let reaper = Arc::new(Mutex::new(Vec::<i64>::new()));
    let _belt = GroupReaper(reaper.clone());

    let jail = Jail::make("f3a");
    jail.assert_sandbox(&b);

    // FLEET: victim A + 2 siblings (wider fleet for F1/F2/F3a/F4).
    let a = spawn_daemon(&jail, "A-victim", &reaper, &b);
    let sib1 = spawn_daemon(&jail, "B-sibling", &reaper, &b);
    let sib2 = spawn_daemon(&jail, "C-sibling", &reaper, &b);

    // WEDGE: SIGSTOP A's serve loop → it accepts TCP into the backlog but never performs
    // the ws handshake (the camped-serve model wire.rs's own bound test uses).
    assert!(signal(a.pid, "STOP"), "SIGSTOP delivered to A");
    // FIRE-WITNESS (a): the camp is GENUINELY blocked — A is in state 'T' (stopped).
    let state = proc_state(a.pid);
    let stopped = state.starts_with('T');
    assert!(stopped, "A genuinely stopped (state {state:?}) — camp is real, not a no-op");

    // GUARDED: the production 5s connect fails FAST, within the 10s ceiling.
    let t0 = Instant::now();
    let guarded = AcpConnection::connect(&a.url, PROD_CONNECT_TIMEOUT);
    let guarded_elapsed = t0.elapsed();
    assert!(guarded.is_err(), "connect to the camped daemon must NOT yield a connection");
    assert!(
        guarded_elapsed < F3A_CONNECT_CEILING,
        "GUARDED connect fails FAST within {F3A_CONNECT_CEILING:?}, took {guarded_elapsed:?}"
    );

    // FIRE-WITNESS (b) — UNGUARDED CONTROL ARM: absent the 5s bound, the SAME camp
    // blocks PAST the ceiling. Measure a raw handshake read (no deadline), capped just
    // above the ceiling.
    let control_cap = F3A_CONNECT_CEILING + Duration::from_secs(2);
    let unguarded = measure_unguarded_block(&a.url, control_cap);
    assert!(
        unguarded >= F3A_CONNECT_CEILING,
        "UNGUARDED control: the same camp blocks past the {F3A_CONNECT_CEILING:?} ceiling, measured {unguarded:?} \
         (proves the guard is load-bearing, not that the camp was benign)"
    );

    // CROSS-CONTAMINATION: both siblings answer FAST while A is camped.
    let (s1c, s1o) = sib1.endpoint_ours();
    let (s2c, s2o) = sib2.endpoint_ours();
    assert!(s1c && s1o, "sibling B unaffected by A's connect-wedge");
    assert!(s2c && s2o, "sibling C unaffected by A's connect-wedge");

    // Unstick A (slot released — the guard returned; the caller was never wedged).
    signal(a.pid, "CONT");

    ev_text(
        &b,
        "f3a-at-source.txt",
        &format!(
            "A: pid={} url={}\n\
             WEDGE: SIGSTOP → state={state:?} stopped={stopped} (fire-witness a: camp genuine)\n\
             GUARDED connect (prod 5s): err={} elapsed={guarded_elapsed:?} ceiling={F3A_CONNECT_CEILING:?} (fail-fast)\n\
             UNGUARDED control: blocked {unguarded:?} >= ceiling {F3A_CONNECT_CEILING:?} (fire-witness b: would-hang)\n\
             CROSS-CONTAMINATION: sib B={s1c}/{s1o} sib C={s2c}/{s2o} (fast, unaffected)\n",
            a.pid, a.url, guarded.is_err(),
        ),
    );
}

// ===========================================================================
// CLASS F3b — wedge-AFTER-connect / request-hang (30s request guard; ceiling 60s) +
// UNGUARDED-CONTROL arm. Connection ESTABLISHES, then the request hangs (SIGSTOP the
// daemon AFTER the session is up + the driving connection is open) → the per-connection
// 30s DEFAULT_REQUEST_TIMEOUT fails the request FAST within the 60s ceiling. Run at
// {1 hung + 2 live siblings} (mh-coord-14 ruled; the rider-1 structural finding —
// impossible-by-construction resource exhaustion — licenses reduced concurrency).
// ===========================================================================

// NOTE: named `f5_f3b_…` (not `f3b_…`) so libtest's ALPHABETICAL run order under
// `--test-threads=1` places this class LAST — f1, f2, f3a, f4, then f5_f3b. F3b is the
// ~95s class (30s request guard + ~63s unguarded-control cap); running it after the
// cheap classes means a cheaper-class machinery bug surfaces before F3b's wall-clock is
// spent (hardening-lead's convergence sequencing). It is still fault-class F3b.
#[test]
#[ignore = "destructive live chaos — deliberate only: cargo test -p quorum-dispatch --test acp_chaos -- --ignored (+ QD_ACP_CHAOS_LIVE=1)"]
fn f5_f3b_wedge_after_connect_request_hang_fails_fast() {
    if !live() {
        eprintln!("QD_ACP_CHAOS_LIVE != 1 — F3b not run (NOT a pass)");
        return;
    }
    assert!(opencode_on_path(), "opencode must be on PATH");
    let b = bundle("f3b-request-hang");
    let reaper = Arc::new(Mutex::new(Vec::<i64>::new()));
    let _belt = GroupReaper(reaper.clone());

    // Bake the rider-1 structural finding into evidence (impossible-by-construction).
    ev_text(
        &b,
        "f3b-resource-finding.txt",
        "RIDER-1 STRUCTURAL FINDING (established at source; mh-coord-14 accepted; fresh oracle re-derives):\n\
         Concurrent post-connect request-hangs contend NO shared bounded dispatch resource —\n\
         * PROCESS-ISOLATED: each daemon is its own `qd acp-daemon` process (own AcpHost, own\n\
           `acp-crate-conn` thread [client.rs spawn_with_capacity], own bridge child, own serve loop).\n\
         * PER-INVOCATION client path: each qd verb constructs its OWN AcpConnection (send_relay.rs:596 /\n\
           wait.rs:481); no shared client pool.\n\
         * PER-CONNECTION 30s timeout: DEFAULT_REQUEST_TIMEOUT is a per-AcpConnection Cell<Duration>\n\
           (wire.rs:63,442,507), not global.\n\
         * NO lock held across the in-flight request (no Mutex/flock/registry-lock in the drive path);\n\
           run_adapter (acp_residence.rs) writes NO registry row; fd ceiling is per-process.\n\
         => resource-exhaustion-under-simultaneous-request-hang is impossible-BY-CONSTRUCTION, so\n\
         {1 genuine hung + 2 live siblings} fully characterizes F3b.\n",
    );

    let jail = Jail::make("f3b");
    jail.assert_sandbox(&b);

    // FLEET: victim A (to be hung) + 2 live siblings.
    let a = spawn_daemon(&jail, "A-victim", &reaper, &b);
    let mut sibs = Vec::new();
    for i in 0..F3B_SIBLINGS {
        sibs.push(spawn_daemon(&jail, &format!("sibling-{i}"), &reaper, &b));
    }

    // ESTABLISH: a driving connection open + a session id (proves connection genuinely
    // established BEFORE the wedge). Held OPEN across the stop (a post-connect wedge).
    let conn = AcpConnection::connect(&a.url, PROD_CONNECT_TIMEOUT).expect("connect A");
    let session = conn
        .status_session_id()
        .ok()
        .flatten()
        .expect("A session established (connection genuinely established, fire-witness a-i)");

    // WEDGE AFTER connect: SIGSTOP the resident so the established connection's next
    // request gets NO response frame (the turn hangs mid-flight).
    assert!(signal(a.pid, "STOP"), "SIGSTOP delivered to A (post-connect)");
    let state = proc_state(a.pid);
    let stopped = state.starts_with('T');
    assert!(stopped, "A genuinely stopped post-connect (state {state:?}) — the hang is real");

    // GUARDED: a request on the ALREADY-OPEN connection hangs, bounded by the 30s
    // per-connection request timeout → fails FAST within the 60s ceiling. `prompt`
    // awaits the daemon's response frame, which never comes (daemon frozen) → the
    // `read_response` deadline (DEFAULT_REQUEST_TIMEOUT) fires.
    let t0 = Instant::now();
    let hung = conn.prompt(&session, "Reply with the single word PONG.", "acp-chaos-f3b");
    let guarded_elapsed = t0.elapsed();
    assert!(hung.is_err(), "the post-connect request to a wedged daemon must fail (no response)");
    assert!(
        guarded_elapsed < F3B_REQUEST_CEILING,
        "GUARDED request fails FAST within {F3B_REQUEST_CEILING:?}, took {guarded_elapsed:?} (fire-witness a-ii: request genuinely hung, then bounded)"
    );

    // FIRE-WITNESS (b) — UNGUARDED CONTROL ARM: absent the 30s request bound, a raw read
    // against the wedged daemon blocks PAST the 60s ceiling.
    let control_cap = F3B_REQUEST_CEILING + Duration::from_secs(3);
    let unguarded = measure_unguarded_block(&a.url, control_cap);
    assert!(
        unguarded >= F3B_REQUEST_CEILING,
        "UNGUARDED control: absent the 30s bound, the hang blocks past the {F3B_REQUEST_CEILING:?} ceiling, measured {unguarded:?}"
    );

    // CROSS-CONTAMINATION: every live sibling answers FAST while A is hung (the concurrent
    // core — a hung daemon never wedges/latches a healthy sibling).
    let mut sib_states = Vec::new();
    for s in &sibs {
        let (c, o) = s.endpoint_ours();
        assert!(c && o, "sibling {} unaffected by A's request-hang (no cross-contamination)", s.name);
        sib_states.push(format!("{}={c}/{o}", s.name));
    }

    signal(a.pid, "CONT");

    ev_text(
        &b,
        "f3b-at-source.txt",
        &format!(
            "A: pid={} url={} session={session}\n\
             ESTABLISHED before wedge (fire-witness a-i): true\n\
             WEDGE: SIGSTOP post-connect → state={state:?} stopped={stopped}\n\
             GUARDED request (30s guard): err={} elapsed={guarded_elapsed:?} ceiling={F3B_REQUEST_CEILING:?} (fail-fast)\n\
             UNGUARDED control: blocked {unguarded:?} >= ceiling {F3B_REQUEST_CEILING:?} (would-hang)\n\
             CROSS-CONTAMINATION siblings ({}) : {}\n",
            a.pid, a.url, hung.is_err(), F3B_SIBLINGS, sib_states.join(" "),
        ),
    );
}

// ===========================================================================
// CLASS F4 — pgid-teardown → 0 orphans in the pgid-reachable set + escape-boundary
// CHARACTERIZATION. The production `-pgid` ladder (RealDaemonSpawner.kill) reaps the
// adapter + its in-group opencode bridge child; any descendant that setsids OUT of the
// group is characterized at source (this native-bun opencode may have NONE — surfaced).
// ===========================================================================

#[test]
#[ignore = "destructive live chaos — deliberate only: cargo test -p quorum-dispatch --test acp_chaos -- --ignored (+ QD_ACP_CHAOS_LIVE=1)"]
fn f4_pgid_teardown_zero_orphans_and_escape_characterization() {
    if !live() {
        eprintln!("QD_ACP_CHAOS_LIVE != 1 — F4 not run (NOT a pass)");
        return;
    }
    assert!(opencode_on_path(), "opencode must be on PATH");
    let b = bundle("f4-pgid-teardown");
    let reaper = Arc::new(Mutex::new(Vec::<i64>::new()));
    let _belt = GroupReaper(reaper.clone());

    let jail = Jail::make("f4");
    jail.assert_sandbox(&b);

    // FLEET: victim A + sibling B (both under the ONE QD_HOME).
    let a = spawn_daemon(&jail, "A-victim", &reaper, &b);
    let sib = spawn_daemon(&jail, "B-sibling", &reaper, &b);

    // FIRE-WITNESS (a): the in-group opencode bridge child provably EXISTS + is RUNNING
    // pre-teardown, sharing A's pgid (the load-bearing `-pgid`: we reap a REAL live
    // child, not zero children). jail_pids ∩ pgid==A.pgid, minus the daemon itself.
    let before = jail_pids(&jail.qd_home);
    let a_pgid = a.pgid;
    let in_group_children: Vec<i64> = before
        .iter()
        .copied()
        .filter(|&p| p != a.pid && pgid_of(p) == Some(a_pgid))
        .collect();
    ev_text(
        &b,
        "proctree-before.txt",
        &format!(
            "jail pids (QD_HOME={}): {before:?}\nA pid={} pgid={a_pgid}\nin-group children (opencode bridge): {in_group_children:?}\n",
            jail.qd_home.display(), a.pid,
        ),
    );
    assert!(
        !in_group_children.is_empty() && in_group_children.iter().all(|&p| is_pid_alive(p as i32)),
        "the in-group opencode bridge child is live pre-teardown (fire-witness a): {in_group_children:?}"
    );

    // Any descendant that ESCAPED A's group (setsid → different pgid) but is still under
    // this jail's QD_HOME — the F4b escape candidate set (characterized, not gated).
    let escapees_before: Vec<i64> = before
        .iter()
        .copied()
        .filter(|&p| p != a.pid && pgid_of(p) != Some(a_pgid) && pgid_of(sib.pid) != pgid_of(p))
        .collect();

    // TEARDOWN: the PRODUCTION `-pgid` ladder (SIGTERM -pgid → grace → SIGKILL -pgid →
    // reap) — RealDaemonSpawner.kill, the exact ladder acp_residence S2 reuses.
    RealDaemonSpawner.kill(a.pid);
    wait_dead(a.pid);

    // 0 ORPHANS IN THE PGID-REACHABLE SET: the daemon + every in-group child is reaped.
    assert!(!is_pid_alive(a.pid as i32), "A daemon reaped");
    // Settle (bounded): the in-group bridge child is SIGKILLed by the `-pgid` ladder;
    // as `a.pid`'s child it reparents to init, which reaps it. It is NOT our direct
    // child (post-reparent) so we cannot `waitpid` it — poll until the pgid-reachable
    // set clears, so we never read a transient zombie mid-reparenting as a survivor.
    let mut survivors_in_group: Vec<i64> = Vec::new();
    for _ in 0..40 {
        survivors_in_group = in_group_children
            .iter()
            .copied()
            .filter(|&p| is_pid_alive(p as i32))
            .collect();
        if survivors_in_group.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        survivors_in_group.is_empty(),
        "the -pgid teardown reaped the in-group bridge child — ZERO orphans, got survivors {survivors_in_group:?}"
    );

    // F4b ESCAPE-BOUNDARY CHARACTERIZATION (native-bun opencode; NOT a vacuous "zero
    // survivors"). Any process still alive under this jail's QD_HOME that is NOT the
    // sibling's subtree is an escaped descendant. Characterize its lifecycle + whether
    // it LEAKS (holds A's port / wedges the sibling). A benign self-terminating escapee
    // = disclosed boundary; a genuine LEAK = a REAL finding to ESCALATE.
    let after = jail_pids(&jail.qd_home);
    let sib_pgid = pgid_of(sib.pid);
    let escapees_after: Vec<i64> = after
        .iter()
        .copied()
        .filter(|&p| p != sib.pid && pgid_of(p) != sib_pgid)
        .collect();
    // Does any escapee still hold A's ws port? (leak test: the crashed endpoint re-served)
    let port_held = AcpConnection::connect(&a.url, PROD_CONNECT_TIMEOUT)
        .ok()
        .and_then(|c| c.status_session_id().ok())
        .flatten()
        .is_some();

    // CROSS-CONTAMINATION: the sibling is untouched by A's teardown.
    let (sib_conn, sib_ours) = sib.endpoint_ours();
    assert!(sib_conn && sib_ours, "sibling B unaffected by A's pgid teardown");

    // The HARD gate: 0 orphans in the pgid-reachable set (asserted above). The escapee
    // determination is surfaced, NOT folded into pass/fail — UNLESS it genuinely leaks.
    let leak = port_held; // holds A's endpoint port after teardown = operational harm
    assert!(
        !leak,
        "F4b: an escaped descendant is HOLDING A's endpoint port after teardown = a genuine LEAK (REAL finding — escalate)"
    );

    ev_text(
        &b,
        "f4-at-source.txt",
        &format!(
            "A: pid={} pgid={a_pgid} url={}\n\
             FIRE-WITNESS (a): in-group opencode bridge child(ren) live pre-teardown = {in_group_children:?}\n\
             escape candidates pre-teardown (different pgid, same jail) = {escapees_before:?}\n\
             TEARDOWN: production -pgid ladder (RealDaemonSpawner.kill)\n\
             0 ORPHANS in pgid-reachable set: in-group survivors = {survivors_in_group:?} (empty = pass)\n\
             F4b CHARACTERIZATION: escapees after = {escapees_after:?}; A port re-served by an escapee = {port_held}\n\
             LEAK (holds port / accumulates / wedges sibling) = {leak} (false = benign disclosed boundary)\n\
             CROSS-CONTAMINATION: sibling B live+ours = {sib_conn}/{sib_ours}\n\
             ENV NOTE: opencode 1.17.10 is a native bun ELF (opencode.exe), NOT a node wrapper — the §7 \
             node-setsid grandchild escape vector may be ABSENT in this build; characterized empirically above.\n",
            a.pid, a.url,
        ),
    );
    // Snapshot the after-tree for reconstructability.
    ev_text(
        &b,
        "proctree-after.txt",
        &format!("jail pids after teardown (QD_HOME={}): {after:?}\n", jail.qd_home.display()),
    );
}
