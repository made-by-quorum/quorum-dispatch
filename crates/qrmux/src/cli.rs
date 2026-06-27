use clap::{Parser, Subcommand};

const DEFAULT_HISTORY: usize = 10000;
const MAX_HISTORY: usize = 1_000_000;

fn parse_history(s: &str) -> Result<usize, String> {
    let val: usize = s.parse().map_err(|e| format!("{e}"))?;
    if val > MAX_HISTORY {
        return Err(format!("history size must be at most {MAX_HISTORY}"));
    }
    Ok(val)
}

#[derive(Parser)]
#[command(
    name = "retach",
    version,
    about = "Terminal multiplexer with native scrollback"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Open a session: attach if it exists, create if not
    Open {
        /// Session name
        name: String,
        /// Scrollback history size, 0 to disable (used when creating)
        #[arg(long, default_value_t = DEFAULT_HISTORY, value_parser = parse_history)]
        history: usize,
    },
    /// Create a new session
    New {
        /// Session name (auto-generated if omitted)
        name: Option<String>,
        /// Scrollback history size (0 to disable)
        #[arg(long, default_value_t = DEFAULT_HISTORY, value_parser = parse_history)]
        history: usize,
    },
    /// Attach to an existing session
    Attach {
        /// Session name
        name: String,
    },
    /// List active sessions
    List,
    /// Kill a session
    Kill {
        /// Session name
        name: String,
    },
    /// Send one-shot input to a session's PTY without attaching (out-of-band).
    ///
    /// The data argument interprets common backslash escapes (\r \n \t \0 \\).
    /// Use --cr to append a carriage return, or --stdin to read raw bytes from
    /// stdin (binary-safe; the data argument is ignored when --stdin is set).
    Send {
        /// Session name
        name: String,
        /// Data to send (backslash escapes interpreted). Optional with --stdin.
        data: Option<String>,
        /// Append a trailing carriage return (\r) to the data.
        #[arg(long)]
        cr: bool,
        /// Read raw bytes from stdin instead of the data argument (binary-safe).
        #[arg(long)]
        stdin: bool,
    },
    /// Start the server (internal)
    #[command(hide = true)]
    Server {
        /// Bind the daemon socket under this directory instead of the
        /// env-resolved default. Set by the client launcher so the override
        /// crosses the process boundary (C1 D1/R26). Standalone use omits it.
        #[arg(long)]
        socket_dir: Option<std::path::PathBuf>,
        /// WS-C M2/M3b (§4.1): the session this daemon serves — REQUIRED. The
        /// daemon binds `<dir>/<name>.sock` and runs SINGLE-SESSION (capacity-1,
        /// claim-timeout, exit-on-end). The legacy shared-daemon mode (no
        /// `--session`, bound `<dir>/qrmux.sock`) is RETIRED (spec §1, §9).
        #[arg(long)]
        session: String,
    },
}
