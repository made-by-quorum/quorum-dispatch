//! C1 M4 item 7: EmbeddedMux adapter against a REAL jailed qrmux daemon.
//!
//! These are HERMETIC (jail rule 9 + ADD-4): each test resolves its OWN jailed
//! qrmux dir (XDG_RUNTIME_DIR under a short per-run jail root), spawns a fresh
//! qrmux daemon there, exercises the adapter's 8-verb mapping over the M3 client
//! ops, then tears the jail down — kills the daemon by pid + removes the jail dir,
//! verifying no orphan daemon survives.
//!
//! ## Why a pre-spawned daemon (cross-crate binary constraint)
//!
//! The adapter's auto-launch (`ensure_server_running_with`) re-execs the embedder
//! launch spec — in THIS test crate `current_exe()` is the test harness, which has
//! no daemon subcommand. So we pre-spawn the REAL `qrmux` binary with
//! `server --socket-dir <jaildir>`. The adapter ops then SHORT-CIRCUIT the
//! auto-launch (connect to the already-live socket and return early), so EVERY
//! verb — including `run_detached` — runs against the real daemon. This keeps the
//! test a genuine end-to-end adapter→protocol→daemon exercise, not a mock.
//!
//! **COLD-START coverage (C1 M4fix):** because these crate-level tests pre-spawn,
//! they do NOT exercise the PRODUCTION cold-start path where QD ITSELF must launch
//! the daemon (the Lima a6 bug: `current_exe() server` failed for the `qd` binary).
//! That path is covered at GATE level by `tests/c1_gate.rs::g_coldstart`, which
//! drives the real `qd new` with NO pre-spawned daemon and asserts QD stood the
//! daemon up via its hidden `qd qrmux-server` entry (+ a severed-launch mutation
//! control). Do not "fix" this file to cold-start — the cross-crate binary
//! constraint is real; the gate arm owns cold-start.
//!
//! ## sun_path budget
//!
//! The jail root sits under literal `/tmp/qd-embedded-runs/` (TEST infra, NOT
//! engine code — ADD-14 governs ENGINE writes; the daemon socket at
//! `<jail>/xdg/qrmux/qrmux.sock` must fit macOS's 104-byte sun_path, so the base
//! must be short). The engine-resolved dir is asserted to NOT create qrmux-named
//! paths under literal `/tmp` (the ADD-14 belt + its negative control).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dispatch::effects::MapEnv;
use dispatch::embedded_mux::{embedded_socket_dir, EmbeddedMux};
use dispatch::mux::Mux;
use dispatch::mux_selector::EmbeddedEnv;
use dispatch::qrmux_dir::resolve_qrmux_dir;

// ---------------------------------------------------------------------------
// Jail + daemon helpers (test infra)
// ---------------------------------------------------------------------------

/// A per-run hermetic jail: short root under /tmp, jailed HOME + XDG_RUNTIME_DIR.
struct Jail {
    root: PathBuf,
    home: PathBuf,
    xdg_runtime: PathBuf,
}

impl Jail {
    fn establish(tag: &str) -> Jail {
        // Short base for the sun_path budget. A nanosecond suffix avoids collisions.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from("/tmp/qd-embedded-runs").join(format!("{tag}-{nanos}"));
        let home = root.join("h");
        let xdg_runtime = root.join("x");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&xdg_runtime).unwrap();
        // 0700 on the runtime dir (qrmux's socket-dir belt expects per-user perms).
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&xdg_runtime, std::fs::Permissions::from_mode(0o700)).ok();
        Jail {
            root,
            home,
            xdg_runtime,
        }
    }

    /// The EmbeddedEnv snapshot the adapter consumes (jailed XDG → tier 1).
    fn embedded_env(&self) -> EmbeddedEnv {
        EmbeddedEnv {
            xdg_runtime_dir: Some(self.xdg_runtime.to_string_lossy().into_owned()),
            qd_home: None,
            uid: 501,
        }
    }

    /// The dir the engine resolves for this jail (== `$XDG_RUNTIME_DIR/qrmux`).
    fn resolved_dir(&self) -> PathBuf {
        resolve_qrmux_dir(&self.home, &self.embedded_env()).unwrap()
    }

    fn teardown(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Locate the built `qrmux` binary from the test exe's target dir. Panics with a
/// build hint if absent (never a silent skip — that would make the test vacuous).
fn qrmux_binary() -> PathBuf {
    // current_exe = <target>/debug/deps/<test-hash>; the bin is <target>/debug/qrmux.
    let exe = std::env::current_exe().expect("current_exe");
    let mut dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/debug")
        .to_path_buf();
    dir.push("qrmux");
    assert!(
        dir.exists(),
        "qrmux binary not found at {dir:?} — build it first: \
         scripts/build-lock.sh cargo build -p qrmux --bin qrmux"
    );
    dir
}

/// The ADD-14 belt predicate (R-F), SCOPED to qrmux*/qd-shaped names at the /tmp
/// ROOT — the production default an un-de-/tmp'd resolver would emit
/// (`/tmp/qrmux`, `/tmp/qrmux-<uid>`, `/tmp/qd-<uid>`, `/tmp/qd/...`). It must NOT
/// match a deeper jail path that merely nests under /tmp for the sun_path budget.
fn is_tmp_root_qrmux_path(dir: &Path) -> bool {
    let s = dir.to_string_lossy();
    let Some(rest) = s.strip_prefix("/tmp/") else {
        return false;
    };
    // The FIRST /tmp segment must be one of the un-de-/tmp'd PRODUCTION DEFAULTS:
    // `qrmux`, `qrmux-<uid>`, `qd`, or `qd-<uid-digits>`. (Deliberately NOT a broad
    // `qd-` prefix — the test's own jail base `qd-embedded-runs` must not match;
    // `qd-<digits>` is the uid-suffixed default shape only.)
    let first = rest.split('/').next().unwrap_or("");
    first == "qrmux"
        || first == "qd"
        || first
            .strip_prefix("qrmux-")
            .is_some_and(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
        || first
            .strip_prefix("qd-")
            .is_some_and(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
}

/// A spawned jailed daemon; kills AND REAPS the process on drop (no orphan, and
/// no lingering zombie — the daemon is a direct child of this test process, so we
/// must waitpid it after the kill or `kill -0` would see the unreaped zombie).
struct DaemonGuard {
    pid: u32,
    child: Option<std::process::Child>,
}

impl DaemonGuard {
    /// Kill (TERM→KILL) and reap. Idempotent-ish: safe to call once.
    fn kill_and_reap(&mut self) {
        let _ = Command::new("/bin/kill")
            .arg("-TERM")
            .arg(self.pid.to_string())
            .stderr(Stdio::null())
            .status();
        std::thread::sleep(Duration::from_millis(150));
        let _ = Command::new("/bin/kill")
            .arg("-9")
            .arg(self.pid.to_string())
            .stderr(Stdio::null())
            .status();
        if let Some(mut c) = self.child.take() {
            let _ = c.wait(); // reap the zombie so kill -0 reflects reality.
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

/// Is a process alive AND not a zombie? `kill -0` returns success for a zombie
/// (it still "exists"), so we additionally reject the Z/zombie state via ps.
fn pid_alive(pid: u32) -> bool {
    let exists = Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !exists {
        return false;
    }
    // Reject zombies: a Z state means the process is dead-but-unreaped.
    let out = Command::new("/bin/ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output();
    match out {
        Ok(o) => {
            let st = String::from_utf8_lossy(&o.stdout);
            let st = st.trim();
            // Empty (gone) or Z* (zombie) ⇒ not alive.
            !st.is_empty() && !st.starts_with('Z')
        }
        Err(_) => exists,
    }
}

/// WS-C M3b: spawn a PER-SESSION daemon `qrmux server --socket-dir <dir>
/// --session <name>` in the jail and wait for the `<dir>/<name>.sock` leaf. The
/// legacy shared-daemon mode (no `--session`, bound `qrmux.sock`) is RETIRED.
/// Returns the guard + the actual bound socket path the daemon created.
///
/// A long `QRMUX_CLAIM_TIMEOUT_MS` keeps the pre-spawned daemon alive through the
/// adapter chain (the daemon starts EMPTY and would otherwise reap itself on the
/// claim timeout before `run_detached` claims it). The adapter's own
/// `ensure_session_server_running` then probes this live `<name>.sock`, finds it
/// Up, and short-circuits — so every verb runs against this real daemon (the
/// cross-crate cold-start constraint the file header documents is unchanged).
fn start_daemon(jail: &Jail, dir: &Path, name: &str) -> (DaemonGuard, PathBuf) {
    let bin = qrmux_binary();
    let child = Command::new(&bin)
        .arg("server")
        .arg("--socket-dir")
        .arg(dir)
        .arg("--session")
        .arg(name)
        .env_clear()
        .env("HOME", &jail.home)
        .env("XDG_RUNTIME_DIR", &jail.xdg_runtime)
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("QRMUX_CLAIM_TIMEOUT_MS", "60000")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn qrmux server --session");
    let pid = child.id();
    // Keep the Child so the guard can waitpid (reap) after the kill — the daemon is
    // a direct child of this test process (we spawn it WITHOUT setsid, unlike the
    // production launcher), so an unreaped exit lingers as a zombie.
    let mut guard = DaemonGuard {
        pid,
        child: Some(child),
    };

    let socket = dir.join(format!("{name}.sock"));
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if socket.exists() {
            return (guard, socket);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    guard.kill_and_reap();
    panic!("daemon socket not created within 5s at {socket:?}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Probe a per-session socket's ServerHello.session (the §4.4 process-level
/// identity leg of the keystone). Connects, writes the v3 preamble + Hello, and
/// returns the `session` field of the daemon's ServerHello. NO canonicalize()
/// anywhere — the path is used verbatim (§4.4 invariant).
fn server_hello_session(socket: &Path) -> String {
    use qrmux::protocol::{self, codec::FrameReader, write_preamble, ClientMsg, ServerMsg};
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    let socket = socket.to_path_buf();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            // ECONNREFUSED-retry at connect (punch item 16, launcher-lane
            // parallel): the daemon's socket file can exist before its accept loop
            // is scheduled under load — a transient refusal, not a dead daemon.
            let connect_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match UnixStream::connect(&socket).await {
                    Ok(s) => break s,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::ConnectionRefused
                            && tokio::time::Instant::now() < connect_deadline =>
                    {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    Err(e) => panic!("connect: {e}"),
                }
            };
            write_preamble(&mut stream).await.expect("preamble");
            let hello = protocol::encode(&ClientMsg::Hello { caps: vec![] }).unwrap();
            stream.write_all(&hello).await.expect("write hello");
            let mut frames = FrameReader::new();
            loop {
                if let Some(msg) = frames.decode_next::<ServerMsg>().expect("decode") {
                    match msg {
                        ServerMsg::Hello { session, .. } => return session,
                        other => panic!("expected ServerHello, got {other:?}"),
                    }
                }
                if !frames.fill_from(&mut stream).await.expect("fill") {
                    panic!("server closed before ServerHello");
                }
            }
        })
    })
    .join()
    .expect("hello probe thread")
}

/// KEYSTONE (G-CRUD / Bug-D, item 7; generalized per WS-C §4.4): for the live
/// session row — socket.parent() == engine-resolved dir AND the leaf ==
/// `<name>.sock` AND ServerHello.session == name. NON-VACUOUS: asserts the
/// daemon's ACTUAL bound socket path + its process-level identity, and the
/// adapter agrees on the same dir. NO canonicalize() anywhere (§4.4 invariant) —
/// resolution-fn output compared against resolution-fn output.
#[test]
fn keystone_engine_dir_equals_daemon_bound_dir() {
    let jail = Jail::establish("keystone");
    let dir = jail.resolved_dir();
    let name = "keystone-sess";
    let (_guard, bound_socket) = start_daemon(&jail, &dir, name);

    // (1) The daemon bound its socket UNDER the engine-resolved dir.
    assert_eq!(
        bound_socket.parent().unwrap(),
        dir.as_path(),
        "daemon-bound socket dir must equal the engine-resolved dir"
    );
    assert!(bound_socket.exists(), "the bound socket file must exist");

    // (2) leaf ↔ identity: the leaf is exactly `<name>.sock`.
    assert_eq!(
        bound_socket.file_name().unwrap(),
        std::ffi::OsStr::new(&format!("{name}.sock")),
        "per-session leaf must be <name>.sock"
    );

    // (3) process-level identity: ServerHello.session == name (not just pathname).
    assert_eq!(
        server_hello_session(&bound_socket),
        name,
        "ServerHello.session must equal the session name (identity belt)"
    );

    // The adapter, given the same home+env, resolves the SAME dir.
    let mux = EmbeddedMux::new(jail.home.clone(), jail.embedded_env());
    assert_eq!(
        mux.resolved_dir().unwrap(),
        dir,
        "adapter agrees on the dir"
    );

    jail.teardown();
}

/// Full verb mapping against the REAL daemon: create→list→send→history→kill.
#[test]
fn adapter_verb_mapping_against_real_daemon() {
    let jail = Jail::establish("verbs");
    let dir = jail.resolved_dir();
    let name = "qdtest-verbs";
    // WS-C M3b: pre-spawn the PER-SESSION daemon for THIS session name. The
    // adapter's run_detached then short-circuits its own cold-start onto this live
    // `<name>.sock` (cross-crate binary constraint — file header).
    let (guard, _socket) = start_daemon(&jail, &dir, name);
    let mux = EmbeddedMux::new(jail.home.clone(), jail.embedded_env());

    // run_detached: daemon runs `bash -lc <cmd>` in the EXPLICIT cwd. Use a cmd
    // that writes a sentinel then keeps the shell alive (read) so the session
    // stays listed for the rest of the chain.
    let cmd = "echo SENTINEL_ABC123; exec sleep 30";
    let res = mux
        .run_detached(&dir, name, cmd, &jail.home)
        .expect("run_detached");
    assert_eq!(res.status, Some(0), "run_detached acked");

    // list / list_raw: the session is present (synthesized row), attachable.
    let listed = mux.list(&dir).expect("list");
    assert!(
        listed.iter().any(|s| s.name == name),
        "created session is listed: {:?}",
        listed.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    let row = listed.iter().find(|s| s.name == name).unwrap();
    // Synthesis: clients 0, ended None, zmx_status attachable, socket_dir tagged.
    assert_eq!(row.clients, 0);
    assert_eq!(row.ended, None);
    assert_eq!(row.zmx_status.as_deref(), Some("attachable"));
    assert_eq!(
        row.socket_dir.as_deref(),
        Some(dir.to_string_lossy().as_ref())
    );
    assert!(row.pid > 0, "real child pid surfaced");
    // list_raw returns the same rows (D-LISTRAW: no ended surfacing).
    let raw = mux.list_raw(&dir).expect("list_raw");
    assert!(raw.iter().any(|s| s.name == name));

    // history: the sentinel the shell echoed is in scrollback. The daemon renders
    // the PTY output asynchronously, so poll briefly for it to appear.
    let mut hist = String::new();
    for _ in 0..60 {
        hist = mux.history(&dir, name).expect("history");
        if hist.contains("SENTINEL_ABC123") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        hist.contains("SENTINEL_ABC123"),
        "history must contain the echoed sentinel; got: {hist:?}"
    );

    // send: a fire-and-forget write acks a byte count (ExecResult status 0).
    let sent = mux.send(&dir, name, "echo hi\n").expect("send");
    assert_eq!(sent.status, Some(0), "send acked");

    // kill: maps to exit 0 on a clean kill.
    let code = mux.kill(&dir, name).expect("kill");
    assert_eq!(code, 0, "kill of a live session → exit 0");

    // After kill the session is gone (qrmux sessions vanish on end — D-LISTRAW).
    // Poll briefly for the daemon to drop it.
    let mut gone = false;
    for _ in 0..40 {
        if !mux.list(&dir).unwrap().iter().any(|s| s.name == name) {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(gone, "session vanished after kill");

    // kill of an absent session → nonzero (mapped exit 1), never a panic.
    let code = mux.kill(&dir, "no-such-session").expect("kill absent");
    assert_eq!(code, 1, "kill of an absent session → exit 1");

    let pid = guard.pid;
    drop(guard); // kills the daemon by pid.
                 // No orphan: the daemon pid is dead after the guard drop.
    let mut dead = false;
    for _ in 0..40 {
        if !pid_alive(pid) {
            dead = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        dead,
        "daemon pid {pid} must be dead after teardown (no orphan)"
    );

    jail.teardown();
}

/// ADD-14 belt (item 7, spec A14-1 rider R-F): zero NEW literal-/tmp paths with
/// qrmux*/qd-shaped names created by the engine resolution + adapter — SCOPED to
/// those name-classes so shared-host /tmp churn can't flake it.
#[test]
fn add14_no_new_literal_tmp_qrmux_paths() {
    let jail = Jail::establish("add14");
    let env = jail.embedded_env();

    // The engine-resolved dir for a JAILED run (XDG set) lands under the jail's
    // runtime dir, NEVER at the UN-jailed literal-/tmp default. The belt's scope is
    // the qrmux/qd name-classes at the /tmp ROOT (e.g. `/tmp/qrmux`, `/tmp/qd-uid`)
    // — the production default an un-de-/tmp'd resolver would have produced — NOT
    // the test's own jail base (which legitimately nests under /tmp for sun_path).
    let dir = resolve_qrmux_dir(&jail.home, &env).unwrap();
    assert!(
        !is_tmp_root_qrmux_path(&dir),
        "engine qrmux dir must not be a literal-/tmp-ROOT qrmux/qd path: {dir:?}"
    );
    // And it sits under the jailed XDG runtime dir (tier 1) — the agreement that
    // makes the belt above non-trivial (it resolved to the jail, not /tmp).
    assert!(
        dir.starts_with(&jail.xdg_runtime),
        "jailed run resolves under XDG runtime dir: {dir:?}"
    );

    // Tier 2 (no XDG) lands under the jailed HOME (<home>/.quorum/dispatch/mux), never at a
    // /tmp-ROOT qrmux/qd default. (The jail HOME itself nests under /tmp for the
    // sun_path budget — that's test infra, not an engine /tmp write.)
    let env2 = EmbeddedEnv {
        xdg_runtime_dir: None,
        qd_home: None,
        uid: 501,
    };
    let dir2 = resolve_qrmux_dir(&jail.home, &env2).unwrap();
    assert!(
        !is_tmp_root_qrmux_path(&dir2),
        "tier-2 dir must not be a /tmp-root qrmux/qd path: {dir2:?}"
    );
    assert_eq!(
        dir2,
        jail.home.join(".quorum").join("dispatch").join("mux"),
        "tier-2 lands at <home>/.quorum/dispatch/mux"
    );

    jail.teardown();
}

/// NEGATIVE CONTROL for the ADD-14 belt (R-F): point resolution at literal /tmp
/// artificially (XDG_RUNTIME_DIR=/tmp) → the resolved dir IS under /tmp/qrmux, so
/// the belt's check MUST trip. If it didn't, the belt above would be vacuous.
#[test]
fn add14_negative_control_belt_trips_when_pointed_at_tmp() {
    // Artificially point XDG at literal /tmp — tier 1 → /tmp/qrmux.
    let env = EmbeddedEnv {
        xdg_runtime_dir: Some("/tmp".to_string()),
        qd_home: None,
        uid: 501,
    };
    let dir = resolve_qrmux_dir(Path::new("/jail/home"), &env).unwrap();
    assert_eq!(dir, PathBuf::from("/tmp/qrmux"));
    // The SAME predicate the positive belt uses must TRIP here (return true). If it
    // didn't, the positive belt would be vacuous.
    assert!(
        is_tmp_root_qrmux_path(&dir),
        "negative control: pointing XDG at /tmp MUST trip the belt predicate \
         (so the positive belt is non-vacuous), got: {dir:?}"
    );
}

/// The selector picks the EmbeddedMux for unset/"embedded" and surfaces a loud
/// named error for a bogus value — exercised here against the real types end to
/// end (the unit matrix lives in mux_selector.rs).
#[test]
fn selector_builds_embedded_and_errors_on_bogus() {
    use dispatch::mux_selector::{parse_backend, select_mux, Backend};

    let mut vars = std::collections::HashMap::new();
    vars.insert("XDG_RUNTIME_DIR".to_string(), "/run/user/501".to_string());
    let env = MapEnv {
        vars: vars.clone(),
        uid: 501,
    };
    assert_eq!(parse_backend(&env).unwrap(), Backend::Embedded);
    let _mux = select_mux(Backend::Embedded, Path::new("/jail/home"), &env).unwrap();

    let mut bogus_vars = vars.clone();
    bogus_vars.insert("QD_MUX".to_string(), "nonsense".to_string());
    let bogus = MapEnv {
        vars: bogus_vars,
        uid: 501,
    };
    let err = parse_backend(&bogus).unwrap_err();
    assert_ne!(err.exit_code, 1, "distinct exit code for bogus QD_MUX");
    assert!(err.message.contains("nonsense") && err.message.contains("zmx"));
}

/// Sanity: the free fn and a constructed adapter resolve identical dirs (the
/// single-source-of-truth contract the call sites rely on).
#[test]
fn embedded_socket_dir_matches_adapter() {
    let jail = Jail::establish("sot");
    let env = jail.embedded_env();
    let free = embedded_socket_dir(&jail.home, &env).unwrap();
    let mux = EmbeddedMux::new(jail.home.clone(), env);
    assert_eq!(free, mux.resolved_dir().unwrap());
    jail.teardown();
}
