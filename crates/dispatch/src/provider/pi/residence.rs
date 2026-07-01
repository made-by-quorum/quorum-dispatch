//! `provider::pi::residence` — the pi cross-process **residence layer** (item 1):
//! the dispatch-side machinery that makes a resident pi session reachable across
//! SEPARATE `qd` CLI invocations. pi, like ACP and UNLIKE codex, is stdio-only —
//! it has no self-listening server — so quorum WRITES the resident host. This is
//! the structural mirror of [`crate::acp_residence`] (AFFIRMED by pi-lead as the
//! design intent): codex `create_daemon`/`resume_daemon` supply the *choreography*
//! + the pgid teardown (item 3); `acp_residence` supplies the *host-process*
//! pattern.
//!
//! - **[`run_pi_adapter`]** — the resident `qd pi-daemon` entry (NET-NEW; the
//!   `acp_residence::run_adapter` analog). Spawns + OWNS the `pi --mode rpc` child
//!   via [`super::stdio::PiStdio`], binds the birth-id with a boot `get_state`,
//!   and serves the loopback ws front ([`serve_pi`]) until SIGTERM. Outlives the
//!   create verb that spawned it — that IS residence. **Moved base, OPTION B
//!   (P4DB `d44e869`):** the resident no longer constructs a `RegistryStatusSink`
//!   and no longer PUSHES pi events into one — that sink was deleted and status
//!   is now derived ON-READ (pi R3: pid is never read). The serve loop only
//!   drains pi's event buffer (bounded, discarded); the [`super::status`] mapper
//!   → [`super::republish::Republish`] contract is retained for A2's status poll.
//! - **detached spawn / pgid teardown (item 3)** — [`build_pi_adapter_argv`] +
//!   a *reuse* of [`crate::create_daemon::RealDaemonSpawner`] (`process_group(0)`,
//!   group-scoped SIGTERM→grace→SIGKILL). The resident is the group leader; the
//!   pi child (and pi's `&`-detached grandchildren, PA11) inherit the group, so a
//!   `-pgid` kill reaps the whole subtree (the codex/acp two-level teardown). A
//!   bare pid-kill orphans the grandchildren — the group is the only correct scope.
//! - **[`cmdline_is_our_pi_daemon`]** — the S6 identity check (the
//!   `cmdline_is_our_daemon`/`cmdline_is_our_acp_daemon` analog), defeats PID reuse.
//! - **[`connect_ready`]** — the readiness poll (the `connect_with_retry` analog).
//!
//! Faithfulness: the resident never synthesizes status — on-read derivation reads
//! pi's REAL session state. A pi child that cannot `get_state` at boot makes the
//! adapter exit nonzero — it does not fake readiness.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::{Message, WebSocket};

use super::stdio::PiStdio;
use super::rpc::PiRpc;

/// The hidden subcommand marker the resident is launched under (pre-clap in
/// `bin/qd/main.rs`, the `acp-daemon` precedent at main.rs:67). Also the
/// discriminator in the S6 cmdline-identity check.
///
/// WIRE-IN (gated, flagged to mh-coord-3): `bin/qd/main.rs` needs
/// `Some("pi-daemon") => return dispatch::provider::pi::residence::run_pi_adapter(&rest[1..])`
/// added pre-clap — a merge-seam site beyond the ProviderFx 10 + provider_for arm
/// + verb guard.
pub const PI_ADAPTER_VERB: &str = "pi-daemon";

/// The boot `get_state` readiness timeout (PA1: max observed 788ms; live sample
/// 595ms — ~2s is ample margin, per the handoff note + impl-plan §4.1).
const BOOT_TIMEOUT: Duration = Duration::from_secs(2);

/// Front-loop poll granularity — how often the accept/serve loop wakes to
/// re-check SHUTDOWN and to drain buffered pi events (bounded, discarded).
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Set by the SIGTERM/SIGINT handler so [`run_pi_adapter`]'s serve loop returns
/// and the pi child is torn down (graceful half of the two-level teardown).
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_sig: libc::c_int) {
    // async-signal-safe: a single atomic store.
    SHUTDOWN.store(true, Ordering::SeqCst);
}

// ===========================================================================
// Options + argv (pure; mirrors acp_residence — fully fleshed, unit-tested).
// ===========================================================================

/// Parsed `qd pi-daemon` options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiAdapterOpts {
    /// `--listen ws://127.0.0.1:<port>` — the loopback ws front the resident serves.
    pub listen: String,
    /// `--cwd <dir>` — the working dir the pi child runs in.
    pub cwd: PathBuf,
    /// `--pi-bin <path>` — the pinned pi binary (NOT on PATH; `~/.npm-pi-global/
    /// bin/pi`). Absent ⇒ `"pi"` (a real run always sets it via `QD_PI_BIN`).
    pub pi_bin: Option<String>,
    /// `--session-dir <dir>` — `PI_CODING_AGENT_SESSION_DIR` (the CODEX_HOME
    /// analog) passed into the pi child so it reads/writes sessions where qd reads.
    pub session_dir: Option<String>,
    /// `--load-session <id>` (RESUME) — when present the resident boots in LOAD
    /// mode: it re-establishes THIS prior session (`--session <id>` on the pi argv
    /// + a `switch_session`), instead of a fresh session. `None` is the create path.
    pub load_session: Option<String>,
    /// `--registry-dir <dir>` — the qd sessions dir holding the registry ROW.
    /// DISTINCT from `--session-dir` (pi's OWN session storage). **OPTION B:** the
    /// resident no longer writes status here (the `RegistryStatusSink` was burned
    /// by P4DB; status is derived ON-READ). Parsed + carried on the argv but
    /// currently unread by the resident — RESERVED for A2's connect/status poll.
    pub registry_dir: Option<PathBuf>,
    /// `--started-at <ms>` — THIS incarnation's started_at, matching the registry
    /// row the create path wrote. **OPTION B:** was the sink's incarnation-CAS
    /// stamp; retained on the argv for A2's poll (the create path still stamps the
    /// row itself). Currently unread by the resident.
    pub started_at: Option<i64>,
}

/// Parse `--listen`/`--cwd`/`--pi-bin`/`--session-dir`/`--load-session` (both
/// `--flag value` and `--flag=value`). `--listen` and `--cwd` are required.
pub fn parse_adapter_args(args: &[String]) -> Result<PiAdapterOpts, String> {
    let mut listen: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut pi_bin: Option<String> = None;
    let mut session_dir: Option<String> = None;
    let mut load_session: Option<String> = None;
    let mut registry_dir: Option<String> = None;
    let mut started_at: Option<i64> = None;
    let take = |i: &mut usize, args: &[String], flag: &str| -> Result<String, String> {
        let cur = &args[*i];
        if let Some(eq) = cur.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
            Ok(eq.to_string())
        } else {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("pi-daemon: {flag} requires a value"))
        }
    };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--listen" || a.starts_with("--listen=") {
            listen = Some(take(&mut i, args, "--listen")?);
        } else if a == "--cwd" || a.starts_with("--cwd=") {
            cwd = Some(take(&mut i, args, "--cwd")?);
        } else if a == "--pi-bin" || a.starts_with("--pi-bin=") {
            pi_bin = Some(take(&mut i, args, "--pi-bin")?);
        } else if a == "--session-dir" || a.starts_with("--session-dir=") {
            session_dir = Some(take(&mut i, args, "--session-dir")?);
        } else if a == "--load-session" || a.starts_with("--load-session=") {
            load_session = Some(take(&mut i, args, "--load-session")?);
        } else if a == "--registry-dir" || a.starts_with("--registry-dir=") {
            registry_dir = Some(take(&mut i, args, "--registry-dir")?);
        } else if a == "--started-at" || a.starts_with("--started-at=") {
            let raw = take(&mut i, args, "--started-at")?;
            started_at = Some(
                raw.parse::<i64>()
                    .map_err(|e| format!("pi-daemon: --started-at not an integer: {e}"))?,
            );
        } else {
            return Err(format!("pi-daemon: unexpected arg {a:?}"));
        }
        i += 1;
    }
    Ok(PiAdapterOpts {
        listen: listen.ok_or("pi-daemon: --listen is required")?,
        cwd: PathBuf::from(cwd.ok_or("pi-daemon: --cwd is required")?),
        pi_bin,
        session_dir,
        load_session,
        registry_dir: registry_dir.map(PathBuf::from),
        started_at,
    })
}

/// Extract the TCP port from a `ws://127.0.0.1:<port>` endpoint.
pub fn endpoint_port(endpoint: &str) -> Option<u16> {
    let rest = endpoint.strip_prefix("ws://")?;
    let host_port = rest.split('/').next()?;
    host_port.rsplit(':').next()?.parse().ok()
}

/// Build the detached-resident argv: `<exe> pi-daemon --listen <ep> --cwd <cwd>
/// [--pi-bin <bin>] [--session-dir <dir>] [--load-session <id>]`. The `--listen
/// <ep>` pair is what [`cmdline_is_our_pi_daemon`] matches on the live `/proc`
/// cmdline (the per-instance discriminator).
#[allow(clippy::too_many_arguments)]
pub fn build_pi_adapter_argv(
    exe: &Path,
    endpoint: &str,
    cwd: &Path,
    pi_bin: Option<&str>,
    session_dir: Option<&str>,
    registry_dir: Option<&Path>,
    started_at: Option<i64>,
    load_session: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        exe.to_string_lossy().into_owned(),
        PI_ADAPTER_VERB.to_string(),
        "--listen".to_string(),
        endpoint.to_string(),
        "--cwd".to_string(),
        cwd.to_string_lossy().into_owned(),
    ];
    if let Some(bin) = pi_bin {
        argv.push("--pi-bin".to_string());
        argv.push(bin.to_string());
    }
    if let Some(dir) = session_dir {
        argv.push("--session-dir".to_string());
        argv.push(dir.to_string());
    }
    if let Some(dir) = registry_dir {
        argv.push("--registry-dir".to_string());
        argv.push(dir.to_string_lossy().into_owned());
    }
    if let Some(ts) = started_at {
        argv.push("--started-at".to_string());
        argv.push(ts.to_string());
    }
    if let Some(sid) = load_session {
        argv.push("--load-session".to_string());
        argv.push(sid.to_string());
    }
    argv
}

/// S6 — does this live `/proc` cmdline look like OUR pi-daemon for `endpoint`?
/// Requires the [`PI_ADAPTER_VERB`] marker AND, when an endpoint is given, the
/// `--listen <endpoint>` we spawned it with (defeats PID reuse: a connect-success
/// is liveness, identity is the cmdline + the recorded endpoint). A `None`
/// cmdline (pid not visible) ⇒ NOT ours.
pub fn cmdline_is_our_pi_daemon(cmdline: Option<&str>, endpoint: Option<&str>) -> bool {
    let cmd = match cmdline {
        Some(c) => c,
        None => return false,
    };
    if !cmd.contains(PI_ADAPTER_VERB) {
        return false;
    }
    match endpoint {
        None | Some("") => true,
        Some(ep) => cmd.contains(ep),
    }
}

// ===========================================================================
// Readiness (the qd-side connect-poll; reuses the PiRemote client).
// ===========================================================================

/// Poll connect+`status` until the resident's pi session is established (birth-id
/// bound) or `timeout` elapses — the codex `connect_with_retry` / acp
/// `connect_ready` analog. Returns the live remote on success so the create verb
/// can drive it immediately.
///
/// TODO(first-compile): `PiRemote` lands in `provider/pi/remote.rs` (the
/// `AcpConnection` analog over the same `{id,m}`→`{id,ok|e}` front protocol
/// [`serve_pi`] speaks). Wired here once that file compiles.
pub fn connect_ready(endpoint: &str, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut last_err = String::from("never connected");
    while Instant::now() < deadline {
        match super::remote::PiRemote::connect(endpoint, Duration::from_millis(500)) {
            Ok(remote) => match remote.status_session_id() {
                Ok(Some(_sid)) => return Ok(()),
                Ok(None) => last_err = "resident up but session not yet established".into(),
                Err(e) => last_err = format!("status: {e}"),
            },
            Err(e) => last_err = format!("connect: {e}"),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "pi resident at {endpoint} not ready within {timeout:?}: {last_err}"
    ))
}

// ===========================================================================
// The resident entry (item 1 host body) + the loopback front.
// ===========================================================================

/// S1 — the resident `qd pi-daemon` entry. Spawns + OWNS the pi child, binds the
/// birth-id, and serves the loopback front until SIGTERM (OPTION B: no status
/// sink — status is derived on-read). Returns the process exit code. NEVER fakes:
/// a pi child that cannot `get_state` at boot exits nonzero.
pub fn run_pi_adapter(args: &[String]) -> i32 {
    let opts = match parse_adapter_args(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let port = match endpoint_port(&opts.listen) {
        Some(p) => p,
        None => {
            eprintln!("pi-daemon: bad --listen endpoint {:?}", opts.listen);
            return 2;
        }
    };
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("pi-daemon: bind 127.0.0.1:{port}: {e}");
            return 1;
        }
    };

    // Own the pi child (item 1): spawn `pi --mode rpc` [+ --session <id> on load].
    let bin = opts.pi_bin.clone().unwrap_or_else(|| "pi".to_string());
    let mut pi_args = vec!["--mode".to_string(), "rpc".to_string()];
    if let Some(sid) = &opts.load_session {
        // RESUME: re-attach the prior session on the pi argv (the `--session <id>`
        // resume_args path); a `switch_session` after boot is the daemon-path analog.
        pi_args.push("--session".to_string());
        pi_args.push(sid.clone());
    }
    let env: Vec<(String, String)> = match &opts.session_dir {
        Some(d) => vec![("PI_CODING_AGENT_SESSION_DIR".to_string(), d.clone())],
        None => vec![],
    };
    let pi = match PiStdio::spawn(&bin, &pi_args, &opts.cwd, &env) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("pi-daemon: spawn pi {bin:?}: {e}");
            return 1;
        }
    };

    // Boot readiness: a `get_state` round-trip binds the birth-id (item 1 boot).
    pi.set_timeout(BOOT_TIMEOUT);
    let birth_id = match pi.get_state() {
        Ok(state) => state.session_id,
        Err(e) => {
            eprintln!("pi-daemon: boot get_state failed: {e}");
            pi.shutdown();
            return 1;
        }
    };
    // Restore a generous per-command deadline now that boot is done (long turns).
    pi.set_timeout(Duration::from_secs(120));

    // OPTION B (moved base, P4DB `d44e869`): the resident constructs NO
    // `RegistryStatusSink` and pushes NO status. That sink was deleted; session
    // status is derived ON-READ (pi R3: pid is never read). `opts.registry_dir`/
    // `opts.started_at` are parsed but unread here (reserved for A2's poll). The
    // serve loop below only drains pi's event buffer so it stays bounded.

    // Graceful teardown signals (the resident is the pgid leader; the create path's
    // RealDaemonSpawner group-kill is the authoritative item-3 teardown).
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
    }
    eprintln!(
        "pi-daemon: ready — listening {}, session {birth_id}",
        opts.listen
    );

    let code = serve_pi(&pi, &listener, &SHUTDOWN);

    // Teardown (graceful half): close the pi child + reader. The create-path pgid
    // kill reaps any pi grandchildren.
    pi.shutdown();
    code
}

/// The resident serve loop: front one ws client at a time (the ratified
/// single-keystone disposition — `PiStdio` is `!Sync`/one-in-flight, so no
/// `Arc<Mutex>`; mirror of `acp::wire::serve`). BETWEEN/within front requests it
/// DRAINS buffered pi events (bounded, discarded) so the reader thread's buffer
/// stays bounded even with no client connected. **OPTION B:** no status is pushed
/// — session status is derived ON-READ. Returns a process exit code.
fn serve_pi(pi: &PiStdio, listener: &TcpListener, shutdown: &AtomicBool) -> i32 {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return 0;
        }
        // Idle drain: consume any buffered pi events (non-camping, discarded).
        pump_events(pi);

        listener.set_nonblocking(true).ok();
        match listener.accept() {
            Ok((stream, _peer)) => {
                listener.set_nonblocking(false).ok();
                if let Ok(ws) = tungstenite::accept(stream) {
                    handle_connection(pi, ws, shutdown);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                eprintln!("pi-daemon: accept error: {e}");
                return 1;
            }
        }
    }
}

/// Drain currently-buffered pi events (a short, non-camping poll) so the reader
/// thread's buffer stays bounded. Single-borrow: never overlaps a front `request`.
/// **OPTION B:** events are DISCARDED — status is derived on-read, so the resident
/// pushes nothing. Draining is buffer hygiene only.
fn pump_events(pi: &PiStdio) {
    // A tight bound so the loop stays responsive to accept()/shutdown; events that
    // arrive later are caught on the next pass (the reader thread buffers them).
    while let Ok(Some(_event)) = pi.next_event(Duration::from_millis(0)) {}
}

/// Serve one ws connection: read front requests `{id, m, …}`, drive `PiStdio`,
/// write `{id, ok}` / `{id, e}`. Returns on Close/EOF/transport (the resident is
/// untouched; the next connection re-attaches). Mirror of
/// `acp::wire::handle_connection`.
fn handle_connection(pi: &PiStdio, mut ws: WebSocket<TcpStream>, shutdown: &AtomicBool) {
    let _ = ws.get_ref().set_read_timeout(Some(POLL_INTERVAL));
    loop {
        if shutdown.load(Ordering::SeqCst) {
            let _ = ws.close(None);
            return;
        }
        // Drain the pi event buffer even while a client is connected but quiet.
        pump_events(pi);

        let msg = match ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(io_err))
                if io_err.kind() == std::io::ErrorKind::WouldBlock
                    || io_err.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue; // poll granularity; re-check shutdown + pump
            }
            Err(_) => return, // Close / EOF / transport gone
        };
        let text = match msg {
            Message::Text(t) => t.as_str().to_owned(),
            Message::Close(_) => return,
            _ => continue,
        };
        let req: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let frame = match dispatch_front(pi, &req) {
            Ok(result) => json!({ "id": id, "ok": result }),
            Err(e) => json!({ "id": id, "e": e }),
        };
        if let Ok(text) = serde_json::to_string(&frame) {
            if ws.send(Message::Text(text.into())).is_err() {
                return; // peer gone
            }
        }
    }
}

/// Dispatch one decoded front request to the owned `PiStdio`. The front protocol
/// is a thin RPC over the pi trait: `m` names the method; the reply carries the
/// method's result (a `status` probe returns the birth-id + in-flight flag for
/// the readiness poll). NEVER synthesizes pi output — it relays the real driver.
fn dispatch_front(pi: &PiStdio, req: &Value) -> Result<Value, String> {
    let m = req
        .get("m")
        .and_then(Value::as_str)
        .ok_or_else(|| "request missing method 'm'".to_string())?;
    let arg_str = |k: &str| req.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    match m {
        // Readiness/health probe (the connect_ready poll reads session_id).
        "status" => Ok(json!({
            "session_id": pi.get_state().ok().map(|s| s.session_id),
        })),
        "get_state" => pi
            .get_state()
            .map(|s| json!({ "session_id": s.session_id, "is_streaming": s.is_streaming }))
            .map_err(|e| e.to_string()),
        "prompt" => {
            // Default to steer (option-(i): start-if-idle / steer-if-busy).
            let behavior = Some(super::rpc::StreamingBehavior::Steer);
            pi.prompt(&arg_str("message"), behavior)
                .map(|turn_id| json!({ "turn_id": turn_id }))
                .map_err(|e| e.to_string())
        }
        "steer" => pi.steer(&arg_str("message")).map(|_| json!({})).map_err(|e| e.to_string()),
        "follow_up" => pi
            .follow_up(&arg_str("message"))
            .map(|_| json!({}))
            .map_err(|e| e.to_string()),
        "abort" => pi.abort().map(|_| json!({})).map_err(|e| e.to_string()),
        "switch_session" => pi
            .switch_session(&arg_str("sessionPath"))
            .map(|_| json!({}))
            .map_err(|e| e.to_string()),
        other => Err(format!("unknown front method {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_adapter_args_required_and_forms() {
        let o = parse_adapter_args(&[
            "--listen".into(),
            "ws://127.0.0.1:9123".into(),
            "--cwd=/tmp/x".into(),
            "--pi-bin".into(),
            "/n/bin/pi".into(),
        ])
        .unwrap();
        assert_eq!(o.listen, "ws://127.0.0.1:9123");
        assert_eq!(o.cwd, PathBuf::from("/tmp/x"));
        assert_eq!(o.pi_bin.as_deref(), Some("/n/bin/pi"));
        assert_eq!(o.load_session, None);
        // --load-session (both forms) → load mode carries the id.
        let l = parse_adapter_args(&[
            "--listen=ws://127.0.0.1:1".into(),
            "--cwd".into(),
            ".".into(),
            "--load-session".into(),
            "sess-abc".into(),
        ])
        .unwrap();
        assert_eq!(l.load_session.as_deref(), Some("sess-abc"));
        // missing required.
        assert!(parse_adapter_args(&["--cwd".into(), ".".into()]).is_err());
        assert!(parse_adapter_args(&["--listen".into(), "x".into()]).is_err());
    }

    #[test]
    fn endpoint_port_parses_ws_url() {
        assert_eq!(endpoint_port("ws://127.0.0.1:18951"), Some(18951));
        assert_eq!(endpoint_port("ws://127.0.0.1:0"), Some(0));
        assert_eq!(endpoint_port("http://x:1"), None);
        assert_eq!(endpoint_port("ws://127.0.0.1"), None);
    }

    #[test]
    fn build_argv_round_trips_through_parse() {
        let argv = build_pi_adapter_argv(
            Path::new("/usr/bin/qd"),
            "ws://127.0.0.1:9001",
            Path::new("/work"),
            Some("/n/bin/pi"),
            Some("/sess"),
            Some(Path::new("/reg")),
            Some(1_700_000_000_000),
            Some("sess-XYZ"),
        );
        // argv minus exe+verb parses back to the same opts.
        let parsed = parse_adapter_args(&argv[2..]).unwrap();
        assert_eq!(parsed.listen, "ws://127.0.0.1:9001");
        assert_eq!(parsed.cwd, PathBuf::from("/work"));
        assert_eq!(parsed.pi_bin.as_deref(), Some("/n/bin/pi"));
        assert_eq!(parsed.session_dir.as_deref(), Some("/sess"));
        assert_eq!(parsed.registry_dir, Some(PathBuf::from("/reg")));
        assert_eq!(parsed.started_at, Some(1_700_000_000_000));
        assert_eq!(parsed.load_session.as_deref(), Some("sess-XYZ"));
    }

    #[test]
    fn cmdline_identity_requires_marker_and_endpoint() {
        let ep = "ws://127.0.0.1:9000";
        assert!(cmdline_is_our_pi_daemon(
            Some("/usr/bin/qd pi-daemon --listen ws://127.0.0.1:9000 --cwd /w"),
            Some(ep)
        ));
        // marker but a DIFFERENT endpoint → not ours (PID reuse / wrong instance).
        assert!(!cmdline_is_our_pi_daemon(
            Some("/usr/bin/qd pi-daemon --listen ws://127.0.0.1:9999 --cwd /w"),
            Some(ep)
        ));
        // no marker → not ours.
        assert!(!cmdline_is_our_pi_daemon(
            Some("/usr/bin/some-other --listen ws://127.0.0.1:9000"),
            Some(ep)
        ));
        // no cmdline → not ours.
        assert!(!cmdline_is_our_pi_daemon(None, Some(ep)));
        // no endpoint to discriminate → marker alone suffices.
        assert!(cmdline_is_our_pi_daemon(Some("qd pi-daemon --listen x"), None));
    }
}
