//! `acp_residence.rs` — the cross-process ACP **residence layer** (STEP0-RESIDENCE-SCOPE
//! §2c seams S1/S2/S6/S8). This is the dispatch-side machinery that makes the resident
//! `AcpHost` reachable across SEPARATE CLI invocations — the thing codex got for free
//! (its app-server natively listens) but ACP must build (its bridge is stdio-only).
//!
//! - **S1 [`run_adapter`]** — the resident `qd acp-daemon` entry (NET-NEW; no codex
//!   analog). Owns the bridge subprocess (via [`AcpHost`]), establishes the ACP session,
//!   and serves the ws front ([`super::provider::acp::serve`]) until SIGTERM. It outlives
//!   the create verb that spawned it — that IS residence.
//! - **S2 detached spawn** — [`build_adapter_argv`] + a *reuse* of the codex
//!   [`RealDaemonSpawner`](crate::create_daemon::RealDaemonSpawner) (`process_group(0)`,
//!   stdin null, stdout/stderr→log). Spawning the adapter in its own process group means a
//!   group-scoped kill reaps adapter + bridge child TOGETHER (S8 two-level teardown, the
//!   codex group-kill discipline).
//! - **readiness** — [`connect_ready`] polls connect+`status` until the resident session
//!   is established (the codex `connect_with_retry` analog).
//! - **S6 identity** — [`cmdline_is_our_acp_daemon`] (the `cmdline_is_our_daemon` analog).
//!
//! Faithfulness (rider-3): the adapter relays the REAL bridge stream through `serve`; it
//! NEVER synthesizes events (see `provider/acp/wire.rs`). If the bridge cannot
//! `initialize`/`new_session` faithfully, the adapter exits nonzero — it does not fake.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::provider::acp::{AcpConnection, AcpHost, BRIDGE_BIN};

/// The hidden subcommand marker the adapter process is launched under (pre-clap in
/// `bin/qd/main.rs`). Also the discriminator in the S6 cmdline-identity check.
pub const ADAPTER_VERB: &str = "acp-daemon";

/// Set by the SIGTERM/SIGINT handler so [`run_adapter`]'s `serve` loop returns and the
/// bridge is drained (graceful half of the S8 two-level teardown).
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_sig: libc::c_int) {
    // async-signal-safe: a single relaxed atomic store.
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Parsed `qd acp-daemon` options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterOpts {
    /// `--listen ws://127.0.0.1:<port>` — the ws endpoint the adapter binds + serves.
    pub listen: String,
    /// `--cwd <dir>` — the working dir the bridge (and ACP session) runs in.
    pub cwd: PathBuf,
    /// `--bridge-cmd <cmd>` — the bridge program (default [`BRIDGE_BIN`]).
    pub bridge_cmd: String,
    /// `--bridge-arg <arg>` (repeatable) — extra bridge args (e.g. a local node entry).
    pub bridge_args: Vec<String>,
}

/// Parse `--listen`/`--cwd`/`--bridge-cmd`/`--bridge-arg` (both `--flag value` and
/// `--flag=value`). `--listen` and `--cwd` are required.
pub fn parse_adapter_args(args: &[String]) -> Result<AdapterOpts, String> {
    let mut listen: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut bridge_cmd: Option<String> = None;
    let mut bridge_args: Vec<String> = Vec::new();
    let mut i = 0;
    // Take the value of `--flag`: from `=` if present, else the next token.
    let take = |i: &mut usize, args: &[String], flag: &str| -> Result<String, String> {
        let cur = &args[*i];
        if let Some(eq) = cur.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
            Ok(eq.to_string())
        } else {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("acp-daemon: {flag} requires a value"))
        }
    };
    while i < args.len() {
        let a = &args[i];
        if a == "--listen" || a.starts_with("--listen=") {
            listen = Some(take(&mut i, args, "--listen")?);
        } else if a == "--cwd" || a.starts_with("--cwd=") {
            cwd = Some(take(&mut i, args, "--cwd")?);
        } else if a == "--bridge-cmd" || a.starts_with("--bridge-cmd=") {
            bridge_cmd = Some(take(&mut i, args, "--bridge-cmd")?);
        } else if a == "--bridge-arg" || a.starts_with("--bridge-arg=") {
            bridge_args.push(take(&mut i, args, "--bridge-arg")?);
        } else {
            return Err(format!("acp-daemon: unexpected arg {a:?}"));
        }
        i += 1;
    }
    Ok(AdapterOpts {
        listen: listen.ok_or("acp-daemon: --listen is required")?,
        cwd: PathBuf::from(cwd.ok_or("acp-daemon: --cwd is required")?),
        bridge_cmd: bridge_cmd.unwrap_or_else(|| BRIDGE_BIN.to_string()),
        bridge_args,
    })
}

/// Extract the TCP port from a `ws://127.0.0.1:<port>` endpoint.
pub fn endpoint_port(endpoint: &str) -> Option<u16> {
    let rest = endpoint.strip_prefix("ws://")?;
    let host_port = rest.split('/').next()?;
    host_port.rsplit(':').next()?.parse().ok()
}

/// Build the detached-adapter argv: `<exe> acp-daemon --listen <ep> --cwd <cwd>
/// [--bridge-cmd <cmd>] [--bridge-arg <a>]…`. The `--listen <ep>` pair is also what the
/// S6 identity check ([`cmdline_is_our_acp_daemon`]) matches on the live `/proc` cmdline.
pub fn build_adapter_argv(
    exe: &Path,
    endpoint: &str,
    cwd: &Path,
    bridge_cmd: Option<&str>,
    bridge_args: &[String],
) -> Vec<String> {
    let mut argv = vec![
        exe.to_string_lossy().into_owned(),
        ADAPTER_VERB.to_string(),
        "--listen".to_string(),
        endpoint.to_string(),
        "--cwd".to_string(),
        cwd.to_string_lossy().into_owned(),
    ];
    if let Some(cmd) = bridge_cmd {
        argv.push("--bridge-cmd".to_string());
        argv.push(cmd.to_string());
    }
    for a in bridge_args {
        argv.push("--bridge-arg".to_string());
        argv.push(a.clone());
    }
    argv
}

/// S6 — does this live `/proc` cmdline look like OUR acp-daemon for `endpoint`? The
/// `cmdline_is_our_daemon` analog: it must carry the [`ADAPTER_VERB`] marker AND, when an
/// endpoint is given, the `--listen <endpoint>` we spawned it with (defeats PID reuse — a
/// connect-success is liveness, identity is the cmdline + ProcKey fence). `cmdline` is the
/// `/proc/<pid>/cmdline` with NUL-joined argv flattened to spaces.
pub fn cmdline_is_our_acp_daemon(cmdline: Option<&str>, endpoint: Option<&str>) -> bool {
    let cmd = match cmdline {
        Some(c) => c,
        None => return false,
    };
    if !cmd.contains(ADAPTER_VERB) {
        return false;
    }
    match endpoint {
        // No endpoint to discriminate on → the marker alone (a row that lost its ep).
        None | Some("") => true,
        // The recorded endpoint must appear on the live cmdline (we spawned it with
        // `--listen <endpoint>`); match the substring so a reformat still matches.
        Some(ep) => cmd.contains(ep),
    }
}

/// readiness — poll connect+`status` until the resident session is established or
/// `timeout` elapses (the codex `connect_with_retry` analog). Returns the live
/// connection on success so the caller can drive it immediately.
pub fn connect_ready(endpoint: &str, timeout: Duration) -> Result<AcpConnection, String> {
    let deadline = Instant::now() + timeout;
    let mut last_err = String::from("never connected");
    while Instant::now() < deadline {
        match AcpConnection::connect(endpoint, Duration::from_millis(500)) {
            Ok(conn) => match conn.status_session_id() {
                Ok(Some(_sid)) => return Ok(conn),
                Ok(None) => {
                    last_err = "adapter up but session not yet established".into();
                }
                Err(e) => last_err = format!("status: {e}"),
            },
            Err(e) => last_err = format!("connect: {e}"),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "acp adapter at {endpoint} not ready within {timeout:?}: {last_err}"
    ))
}

/// S1 — the resident `qd acp-daemon` entry. Spawns + OWNS the bridge, establishes the ACP
/// session, and serves the ws front until SIGTERM. Returns the process exit code. This is
/// the process that OUTLIVES the create verb (residence). It NEVER fakes: a bridge that
/// cannot handshake/establish a session exits nonzero.
pub fn run_adapter(args: &[String]) -> i32 {
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
            eprintln!("acp-daemon: bad --listen endpoint {:?}", opts.listen);
            return 2;
        }
    };
    let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("acp-daemon: bind 127.0.0.1:{port}: {e}");
            return 1;
        }
    };
    // Own the bridge (S1): spawn it, handshake, establish the resident session.
    let host = match AcpHost::spawn(&opts.bridge_cmd, &opts.bridge_args, &opts.cwd) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("acp-daemon: spawn bridge {:?}: {e}", opts.bridge_cmd);
            return 1;
        }
    };
    if let Err(e) = crate::provider::acp::AcpClient::initialize(&host) {
        eprintln!("acp-daemon: bridge initialize failed: {e}");
        return 1;
    }
    let cwd_str = opts.cwd.to_string_lossy().into_owned();
    let session = match crate::provider::acp::AcpClient::new_session(&host, &cwd_str) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("acp-daemon: session/new failed: {e}");
            return 1;
        }
    };
    // Graceful teardown: SIGTERM/SIGINT → SHUTDOWN → serve returns → drain the bridge.
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
    }
    eprintln!("acp-daemon: ready — listening {}, session {session}", opts.listen);
    let serve_res = crate::provider::acp::serve(&host, &listener, &SHUTDOWN);
    // S8 two-level teardown (graceful half): reap the bridge child + join the reader.
    host.shutdown();
    match serve_res {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("acp-daemon: serve error: {e}");
            1
        }
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
            "--bridge-cmd".into(),
            "node".into(),
            "--bridge-arg".into(),
            "/p/index.js".into(),
        ])
        .unwrap();
        assert_eq!(o.listen, "ws://127.0.0.1:9123");
        assert_eq!(o.cwd, PathBuf::from("/tmp/x"));
        assert_eq!(o.bridge_cmd, "node");
        assert_eq!(o.bridge_args, vec!["/p/index.js".to_string()]);
        // default bridge when unspecified
        let d = parse_adapter_args(&["--listen=ws://127.0.0.1:1".into(), "--cwd".into(), ".".into()])
            .unwrap();
        assert_eq!(d.bridge_cmd, BRIDGE_BIN);
        // missing required
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
    fn build_adapter_argv_assembles_self_exec() {
        let argv = build_adapter_argv(
            Path::new("/usr/bin/qd"),
            "ws://127.0.0.1:9000",
            Path::new("/work"),
            Some("node"),
            &["/p/i.js".to_string()],
        );
        assert_eq!(
            argv,
            vec![
                "/usr/bin/qd",
                "acp-daemon",
                "--listen",
                "ws://127.0.0.1:9000",
                "--cwd",
                "/work",
                "--bridge-cmd",
                "node",
                "--bridge-arg",
                "/p/i.js",
            ]
        );
    }

    #[test]
    fn cmdline_identity_requires_marker_and_endpoint() {
        let ep = "ws://127.0.0.1:9000";
        // marker + endpoint present → ours
        assert!(cmdline_is_our_acp_daemon(
            Some("/usr/bin/qd acp-daemon --listen ws://127.0.0.1:9000 --cwd /w"),
            Some(ep)
        ));
        // marker present but a DIFFERENT endpoint → not ours (PID reuse / wrong instance)
        assert!(!cmdline_is_our_acp_daemon(
            Some("/usr/bin/qd acp-daemon --listen ws://127.0.0.1:9999 --cwd /w"),
            Some(ep)
        ));
        // no marker → not ours (a reused pid running something else)
        assert!(!cmdline_is_our_acp_daemon(Some("/usr/bin/some-other --listen ws://127.0.0.1:9000"), Some(ep)));
        // no cmdline → not ours
        assert!(!cmdline_is_our_acp_daemon(None, Some(ep)));
        // no endpoint to discriminate → marker alone suffices
        assert!(cmdline_is_our_acp_daemon(Some("qd acp-daemon --listen x"), None));
    }
}
