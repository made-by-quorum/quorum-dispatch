//! qrmux — terminal multiplexer with native scrollback passthrough.
//!
//! Instead of intercepting scrollback (like tmux/screen), qrmux sends completed
//! lines directly to stdout, letting the client terminal handle scrolling natively.
//!
//! This binary wraps the qrmux library for daemon/client operations.

use clap::Parser;
use cli::{Cli, Command};
use qrmux::{cli, client, protocol, server};
use std::io::Read;
use tracing_subscriber::EnvFilter;

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("qrmux=info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let rt = tokio::runtime::Runtime::new()?;

    // WS-C M3b: the standalone CLI is the SAME per-session world as the engine
    // (spec §3, D-SOCKDIR mirror). Open/New/Attach go through the per-session
    // attach (`<dir>/<name>.sock`, cold-start via the standalone `server` entry);
    // List scans the dir; Kill/Send address the named session's daemon. The legacy
    // shared-daemon verbs are retired.
    let result = match cli.command {
        Command::Open { name, history } => rt.block_on(client::session_client::attach_session(
            None,
            None,
            &name,
            history,
            protocol::ConnectMode::CreateOrAttach,
        )),
        Command::New { name, history } => {
            let name = name.unwrap_or_else(generate_name);
            rt.block_on(client::session_client::attach_session(
                None,
                None,
                &name,
                history,
                protocol::ConnectMode::CreateOnly,
            ))
        }
        Command::Attach { name } => rt.block_on(client::session_client::attach_session(
            None,
            None,
            &name,
            0,
            protocol::ConnectMode::AttachOnly,
        )),
        Command::List => rt.block_on(client::list_sessions()),
        Command::Kill { name } => rt.block_on(client::kill_session(&name)),
        Command::Send {
            name,
            data,
            cr,
            stdin,
        } => match build_send_payload(data, cr, stdin) {
            Ok(payload) => rt.block_on(client::send_input(&name, payload)),
            Err(e) => Err(e),
        },
        Command::Server {
            socket_dir,
            session,
        } => rt.block_on(server::run_server(socket_dir, session)),
    };

    // Shut down runtime with timeout to avoid hanging on blocked stdin thread
    rt.shutdown_timeout(std::time::Duration::from_millis(100));

    result
}

/// Build the byte payload for `send`. With `--stdin`, read raw bytes from stdin
/// (binary-safe). Otherwise interpret backslash escapes in the data argument.
/// `--cr` appends a trailing carriage return in both cases.
fn build_send_payload(data: Option<String>, cr: bool, stdin: bool) -> anyhow::Result<Vec<u8>> {
    let mut payload = if stdin {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        buf
    } else {
        let s =
            data.ok_or_else(|| anyhow::anyhow!("send: provide a data argument or use --stdin"))?;
        interpret_escapes(&s)
    };
    if cr {
        payload.push(b'\r');
    }
    Ok(payload)
}

/// Interpret a minimal, predictable set of backslash escapes in `s`, returning
/// the resulting bytes. Supports \r \n \t \0 and \\ (literal backslash). An
/// unrecognized escape is left verbatim (backslash + the following char) so the
/// behavior is never surprising. A trailing lone backslash is kept literally.
fn interpret_escapes(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'r' => out.push(b'\r'),
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'0' => out.push(0),
                b'\\' => out.push(b'\\'),
                other => {
                    out.push(b'\\');
                    out.push(other);
                }
            }
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn generate_name() -> String {
    use std::hash::{BuildHasher, Hasher};
    // RandomState::new() seeds from OS randomness, giving real uniqueness
    // without adding external dependencies.
    let hash = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("s-{:016x}", hash)
}

#[cfg(test)]
mod tests {
    use super::interpret_escapes;

    #[test]
    fn escapes_common() {
        assert_eq!(interpret_escapes("a\\rb"), b"a\rb");
        assert_eq!(interpret_escapes("x\\ny"), b"x\ny");
        assert_eq!(interpret_escapes("t\\tt"), b"t\tt");
        assert_eq!(interpret_escapes("z\\0z"), vec![b'z', 0, b'z']);
        assert_eq!(interpret_escapes("a\\\\b"), b"a\\b");
    }

    #[test]
    fn escapes_unknown_left_verbatim() {
        assert_eq!(interpret_escapes("a\\qb"), b"a\\qb");
    }

    #[test]
    fn escapes_trailing_backslash_literal() {
        assert_eq!(interpret_escapes("end\\"), b"end\\");
    }

    #[test]
    fn escapes_plain_passthrough() {
        assert_eq!(interpret_escapes("echo B1SENTINEL"), b"echo B1SENTINEL");
    }
}
