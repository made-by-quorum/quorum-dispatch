//! C1 M4fix: the HIDDEN embedded-sbmux-daemon entry for the `sb` binary.
//!
//! The sb binary IS the embedded sbmux daemon — it links the `sbmux` crate. The
//! client launcher (`ensure_server_running_with`) re-execs THIS subcommand
//! (`sb sbmux-server [--socket-dir DIR]`) instead of `current_exe() server`,
//! because `sb` has no bare `server` verb (that assumption broke embedded
//! cold-start in production — Lima a6-embedded-backend-DELTA.txt).
//!
//! Dispatched PRE-CLAP (main.rs) so it never enters the user-facing surface: the
//! a3 help/exit-byte contract stays byte-unchanged. Args are hand-parsed (mirrors
//! sbmux's own `Command::Server { socket_dir, session }`): the accepted options are
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

/// Run the embedded sbmux daemon. `args` is the argv tail AFTER `sbmux-server`
/// (e.g. `["--socket-dir", "<dir>"]`, `["--socket-dir", "<dir>", "--session",
/// "<name>"]`, or empty). Returns the process exit code.
///
/// Mirrors `sbmux::main`'s
/// `Command::Server { socket_dir, session } => run_server(socket_dir, session)`:
/// builds a tokio runtime and blocks on
/// `sbmux::server::run_server(Option<PathBuf>, String)`. WS-C M3b: `--session` is
/// REQUIRED (legacy shared-daemon mode retired); a missing value exits 2.
pub fn run_sbmux_server(args: &[String]) -> i32 {
    let parsed = match parse_server_args(args) {
        Ok(d) => d,
        Err(msg) => {
            eprintln!("sb sbmux-server: {msg}");
            return 2;
        }
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("sb sbmux-server: failed to build runtime: {e}");
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
                "sb sbmux-server: --session <NAME> is required (legacy shared-daemon mode retired)"
            );
            return 2;
        }
    };

    // WP-B2b-2b: the embedded daemon injects the headless launch factory (the
    // sb-side `RegistryStatusSink` + the daemon-resolved launch posture — design
    // §D-2b) so a `LaunchHeadless` can spawn a `claude -p` turn and write the
    // registry status row in real time. Resolved best-effort from the daemon's
    // env; if HOME is unset the factory is omitted (headless then refused) rather
    // than failing the daemon bind. The PRODUCTION trigger (`sb start` →
    // LaunchHeadless) lands in WP-B-CS — this populates the seam the design names.
    use dispatch::effects::Env as _;
    let headless: Option<std::sync::Arc<dyn sbmux::headless_session::HeadlessFactory>> =
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
                ) as std::sync::Arc<dyn sbmux::headless_session::HeadlessFactory>
            });

    match rt.block_on(sbmux::server::run_server(
        parsed.socket_dir,
        session,
        headless,
    )) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("sb sbmux-server: {e}");
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
            "/run/user/501/sbmux".to_string(),
        ];
        assert_eq!(
            parse_server_args(&args).unwrap().socket_dir,
            Some(PathBuf::from("/run/user/501/sbmux"))
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
            "/run/user/501/sbmux".to_string(),
            "--session".to_string(),
            "gamma".to_string(),
        ];
        let parsed = parse_server_args(&both).unwrap();
        assert_eq!(
            parsed.socket_dir,
            Some(PathBuf::from("/run/user/501/sbmux"))
        );
        assert_eq!(parsed.session, Some("gamma".into()));
    }

    #[test]
    fn parse_session_missing_value_errors() {
        let args = vec!["--session".to_string()];
        assert!(parse_server_args(&args).is_err());
    }
}
