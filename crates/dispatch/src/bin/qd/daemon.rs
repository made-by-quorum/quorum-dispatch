//! C1 M4fix: the HIDDEN embedded-qrmux-daemon entry for the `qd` binary.
//!
//! The qd binary IS the embedded qrmux daemon — it links the `qrmux` crate. The
//! client launcher (`ensure_server_running_with`) re-execs THIS subcommand
//! (`qd qrmux-server [--socket-dir DIR]`) instead of `current_exe() server`,
//! because `qd` has no bare `server` verb (that assumption broke embedded
//! cold-start in production — Lima a6-embedded-backend-DELTA.txt).
//!
//! Dispatched PRE-CLAP (main.rs) so it never enters the user-facing surface: the
//! a3 help/exit-byte contract stays byte-unchanged. Args are hand-parsed (mirrors
//! qrmux's own `Command::Server { socket_dir, session }`): the accepted options are
//! `--socket-dir <DIR>` (C1 D1/R26 socket-dir propagation) and `--session <NAME>`
//! (WS-C M2 single-session mode, spec §4.1).

use std::path::PathBuf;

/// Parsed argv for the embedded daemon: the socket-dir override and the optional
/// WS-C single-session identity.
#[derive(Debug, Default, PartialEq)]
struct ServerArgs {
    socket_dir: Option<PathBuf>,
    session: Option<String>,
}

/// Run the embedded qrmux daemon. `args` is the argv tail AFTER `qrmux-server`
/// (e.g. `["--socket-dir", "<dir>"]`, `["--socket-dir", "<dir>", "--session",
/// "<name>"]`, or empty). Returns the process exit code.
///
/// Mirrors `qrmux::main`'s
/// `Command::Server { socket_dir, session } => run_server(socket_dir, session)`:
/// builds a tokio runtime and blocks on
/// `qrmux::server::run_server(Option<PathBuf>, String)`. WS-C M3b: `--session` is
/// REQUIRED (legacy shared-daemon mode retired); a missing value exits 2.
pub fn run_qrmux_server(args: &[String]) -> i32 {
    let parsed = match parse_server_args(args) {
        Ok(d) => d,
        Err(msg) => {
            eprintln!("qd qrmux-server: {msg}");
            return 2;
        }
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("qd qrmux-server: failed to build runtime: {e}");
            return 1;
        }
    };

    // WS-C M3b: `--session` is REQUIRED — the legacy shared-daemon mode is retired
    // (spec §1, §9). A missing session is a launcher wiring bug worth surfacing
    // loudly (exit 2), never a silent fall-through to a removed legacy bind.
    let session = match parsed.session {
        Some(s) => s,
        None => {
            eprintln!(
                "qd qrmux-server: --session <NAME> is required (legacy shared-daemon mode retired)"
            );
            return 2;
        }
    };

    // WP-B2b-2b: the embedded daemon injects the headless launch factory (the
    // qd-side `RegistryStatusSink` + the daemon-resolved launch posture — design
    // §D-2b) so a `LaunchHeadless` can spawn a `claude -p` turn and write the
    // registry status row in real time. Resolved best-effort from the daemon's
    // env; if HOME is unset the factory is omitted (headless then refused) rather
    // than failing the daemon bind. The PRODUCTION trigger (`qd start` →
    // LaunchHeadless) lands in WP-B-CS — this populates the seam the design names.
    use dispatch::effects::Env as _;
    let headless: Option<std::sync::Arc<dyn qrmux::headless_session::HeadlessFactory>> =
        dispatch::effects::RealEnv
            .var("HOME")
            .filter(|s| !s.is_empty())
            .map(|home| {
                let paths = dispatch::paths::SbPaths::from_home_env(
                    std::path::Path::new(&home),
                    &dispatch::effects::RealEnv,
                );
                std::sync::Arc::new(
                    dispatch::daemon_headless::DaemonHeadlessFactory::from_daemon_env(
                        &dispatch::effects::RealEnv,
                        &paths,
                    ),
                ) as std::sync::Arc<dyn qrmux::headless_session::HeadlessFactory>
            });

    // R3c-Step-1: resolve the per-session control socket path the daemon binds.
    // It MUST rendezvous with the relay server's `send_control` target, which keys
    // on `CLAUDE_CODE_SESSION_ID` + `SbPaths::state_dir` (relay_server/mod.rs). The
    // daemon derives the identical path from the SAME env, so both sides agree
    // without a new CLI arg. Only THIS entry can name `dispatch::control_sock`
    // (qrmux cannot — it is the lower crate); bare qrmux (`qrmux/src/main.rs`) passes
    // None and runs with no ctrl servicer. Absent env (no session id / HOME) → None.
    //
    // R3c item-1: resolve the (session_id, state_dir) pair ONCE — both the control
    // socket AND the daemon-lifetime liveness lock key on it, so they rendezvous
    // with the relay server / the reconcile `probe_dead` fast-path from the SAME
    // env without a new CLI arg.
    let session_state: Option<(String, PathBuf)> = dispatch::effects::RealEnv
        .var("CLAUDE_CODE_SESSION_ID")
        .filter(|s| !s.is_empty())
        .and_then(|sid| {
            dispatch::effects::RealEnv
                .var("HOME")
                .filter(|h| !h.is_empty())
                .map(|home| {
                    let paths = dispatch::paths::SbPaths::from_home_env(
                        std::path::Path::new(&home),
                        &dispatch::effects::RealEnv,
                    );
                    (sid, paths.state_dir)
                })
        });

    let ctrl_sock: Option<PathBuf> = session_state
        .as_ref()
        .map(|(sid, state_dir)| dispatch::control_sock::control_sock_path(state_dir, sid));

    // R3c item-1 (HIGH-stakes): acquire the per-session liveness flock at
    // DAEMON-LIFETIME. THIS process — the embedded qrmux daemon — is the long-lived
    // per-session daemon (single-session mode, §4.1); it holds the lock for its
    // whole life and the kernel releases it on daemon death (flock last-close), so
    // `LivenessLock::probe_dead` / `reconcile`'s flock fast-path see the session as
    // live exactly while this daemon is alive (R2 §R3a-Step-1; makes the
    // flock+incarnation scaffolding load-bearing on the live path). NOT the launcher
    // (`create_daemon::run_new_daemon` is the LAUNCHER and returns — placing the lock
    // there would free it immediately, the rev0 fleet-wide false-alive). The fd
    // defaults to `FD_CLOEXEC` SET (livelock.rs), so the PTY tool subprocesses this
    // daemon spawns do NOT inherit it across `exec` (the P4 scoped-CLOEXEC fix: a
    // surviving tool child can no longer keep a dead session looking alive). We
    // deliberately do NOT call `allow_inherit` — this process IS the intended holder,
    // not a launcher handing the lock to a managed wrapper. Held in `_liveness_lock`
    // across `block_on`; dropped (and kernel-released) only when the daemon exits.
    let _liveness_lock = session_state.as_ref().and_then(|(sid, state_dir)| {
        match dispatch::livelock::LivenessLock::acquire(state_dir, sid) {
            Ok(Some(lock)) => {
                eprintln!("qd qrmux-server: liveness lock acquired for session {sid}");
                Some(lock)
            }
            Ok(None) => {
                // The `.sock` bind above already proved no live duplicate daemon;
                // a held lock here is a stale-holder anomaly. Log and run degraded
                // — the `/proc start_ms` confirmer remains the tombstone authority.
                eprintln!(
                    "qd qrmux-server: liveness lock already held for session {sid} (continuing)"
                );
                None
            }
            Err(e) => {
                eprintln!(
                    "qd qrmux-server: liveness lock acquire failed for session {sid}: {e} \
                     (continuing degraded — /proc remains the tombstone authority)"
                );
                None
            }
        }
    });

    match rt.block_on(qrmux::server::run_server_ctrl(
        parsed.socket_dir,
        session,
        headless,
        ctrl_sock,
    )) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("qd qrmux-server: {e}");
            1
        }
    }
}

/// Hand-parse `--socket-dir <DIR>` and the optional `--session <NAME>` (WS-C M2).
/// Both `--flag value` and `--flag=value` forms are accepted. Unknown args or a
/// missing value are loud errors (exit 2) — the launcher only ever passes these
/// flags, so anything else is a wiring bug worth surfacing, not silently ignoring.
fn parse_server_args(args: &[String]) -> Result<ServerArgs, String> {
    let mut out = ServerArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket-dir" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| "--socket-dir requires a directory argument".to_string())?;
                out.socket_dir = Some(PathBuf::from(val));
                i += 2;
            }
            other if other.starts_with("--socket-dir=") => {
                let val = &other["--socket-dir=".len()..];
                if val.is_empty() {
                    return Err("--socket-dir requires a directory argument".to_string());
                }
                out.socket_dir = Some(PathBuf::from(val));
                i += 1;
            }
            "--session" => {
                let val = args
                    .get(i + 1)
                    .ok_or_else(|| "--session requires a name argument".to_string())?;
                out.session = Some(val.clone());
                i += 2;
            }
            other if other.starts_with("--session=") => {
                let val = &other["--session=".len()..];
                if val.is_empty() {
                    return Err("--session requires a name argument".to_string());
                }
                out.session = Some(val.to_string());
                i += 1;
            }
            other => {
                return Err(format!("unexpected argument '{other}'"));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_socket_dir_split_form() {
        let args = vec![
            "--socket-dir".to_string(),
            "/run/user/501/qrmux".to_string(),
        ];
        assert_eq!(
            parse_server_args(&args).unwrap().socket_dir,
            Some(PathBuf::from("/run/user/501/qrmux"))
        );
    }

    #[test]
    fn parse_socket_dir_eq_form() {
        let args = vec!["--socket-dir=/x/y".to_string()];
        assert_eq!(
            parse_server_args(&args).unwrap().socket_dir,
            Some(PathBuf::from("/x/y"))
        );
    }

    #[test]
    fn parse_no_args_is_none() {
        assert_eq!(parse_server_args(&[]).unwrap(), ServerArgs::default());
    }

    #[test]
    fn parse_missing_value_errors() {
        let args = vec!["--socket-dir".to_string()];
        assert!(parse_server_args(&args).is_err());
    }

    #[test]
    fn parse_unknown_arg_errors() {
        let args = vec!["--bogus".to_string()];
        assert!(parse_server_args(&args).is_err());
    }

    /// WS-C M2: `--session` (both forms) threads through; combined with
    /// `--socket-dir` the launcher's full argv parses.
    #[test]
    fn parse_session_split_and_eq_forms() {
        let split = vec!["--session".to_string(), "alpha".to_string()];
        assert_eq!(
            parse_server_args(&split).unwrap().session,
            Some("alpha".into())
        );

        let eq = vec!["--session=beta".to_string()];
        assert_eq!(parse_server_args(&eq).unwrap().session, Some("beta".into()));

        let both = vec![
            "--socket-dir".to_string(),
            "/run/user/501/qrmux".to_string(),
            "--session".to_string(),
            "gamma".to_string(),
        ];
        let parsed = parse_server_args(&both).unwrap();
        assert_eq!(
            parsed.socket_dir,
            Some(PathBuf::from("/run/user/501/qrmux"))
        );
        assert_eq!(parsed.session, Some("gamma".into()));
    }

    #[test]
    fn parse_session_missing_value_errors() {
        let args = vec!["--session".to_string()];
        assert!(parse_server_args(&args).is_err());
    }
}
