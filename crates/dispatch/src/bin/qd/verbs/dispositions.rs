//! `qd dispositions [<correlation_id>] [--window <spec>] [--host <h> | --all]
//! [--archive]` — the qd–qf transition W5 READ verb.
//!
//! This is a thin CLI over the W2 store's projection query
//! ([`dispatch::dispositions::query`]): it resolves the read [`Scope`] from the
//! flags, reads `now_ms` from the injected [`RealClock`], calls `query`, applies
//! the optional caller-supplied `--window` lower bound, and emits the result as
//! **JSONL on stdout** — one [`EmittedRecord::to_jsonl_line`] per line (format
//! doc §3, the one shape frame projects over via DuckDB). No envelope, no pretty
//! mode, no human surface: JSONL is the contract.
//!
//! ## `--window` is STATELESS + caller-windowed (N2)
//!
//! qd stores NO read-state and NO cursor, ever. `--window <dur>` is purely a
//! lower bound the CALLER brings each invocation: keep only records whose
//! `authored_at >= now_ms - dur_ms`. Absent ⇒ no window (every record in scope).
//! The duration grammar is shared with `qd send --expires`
//! ([`dispatch::origin_send::parse_expires`]): a bare integer = seconds, else
//! `<int>{s|m|h|d}`. A bad form is a SYNC arg refusal (exit
//! [`origin_send::EXIT_REFUSED`] = 12), rendered through the shared [`Refusal`]
//! for consistency with the send door.
//!
//! ## Scope
//!
//! `--host <h>` ⇒ [`Scope::Host`] (local UNION `remote/<h>/`); `--all` ⇒
//! [`Scope::All`] (local UNION every peer); neither ⇒ [`Scope::Local`]. `--host`
//! and `--all` are mutually exclusive (clap `conflicts_with`, with a checked
//! belt-and-suspenders refusal). `--archive` additionally unions the local
//! archive tier.
//!
//! ## Exit codes
//!
//! - `0` on success — even zero records (empty output is success, NOT an error).
//! - `12` ([`origin_send::EXIT_REFUSED`]) on a malformed `--window` or a
//!   `--host`+`--all` conflict that reaches the body (a SYNC refusal).
//! - `1` on a store IO error (a clear stderr message).
//! - `141` on a broken downstream pipe (`| head` / `| jq` closes early) — the
//!   `ls.rs` `emit_or_pipe_exit` idiom, never a panic.

use clap::ArgMatches;

use dispatch::dispositions::{query, Scope};
use dispatch::effects::{Clock, Env, RealClock, RealEnv};
use dispatch::origin_send::Refusal;
use dispatch::paths::QdPaths;

/// Emit a fully-built payload to stdout, exiting CLEANLY (141) on a broken pipe
/// instead of letting the write PANIC — the same hardening as `ls.rs`
/// `emit_or_pipe_exit` (engine-hardening item 20). `qd dispositions | head` /
/// `| jq` closing the pipe early must not crash with a partial document + a Rust
/// backtrace; 141 (128 + SIGPIPE) is the conventional "downstream went away"
/// code, and a consumer can distinguish a COMPLETE stream (exit 0) from a
/// truncated one (exit ≠ 0).
fn emit_or_pipe_exit(payload: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let res = out.write_all(payload.as_bytes()).and_then(|()| out.flush());
    if let Err(e) = res {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(141);
        }
        // Any other write failure is still a hard error — never swallowed into a
        // success exit (the partial-as-success trap).
        eprintln!("qd dispositions: failed writing output: {e}");
        std::process::exit(141);
    }
}

/// Resolve the read [`Scope`] from the `--host`/`--all` flags. Neither ⇒
/// [`Scope::Local`]; `--host <h>` ⇒ [`Scope::Host`]; `--all` ⇒ [`Scope::All`].
///
/// `--host` and `--all` are mutually exclusive. clap already rejects the
/// combination at parse (`conflicts_with`), so this belt-and-suspenders refusal
/// is normally unreachable — but keeping it means the scope resolver is total
/// and independently testable, and a future caller invoking the matches without
/// the clap conflict still refuses cleanly rather than silently preferring one.
fn select_scope(m: &ArgMatches) -> Result<Scope, Refusal> {
    let host = m.get_one::<String>("host").cloned();
    let all = m.get_flag("all");
    match (host, all) {
        (Some(_), true) => Err(Refusal::refused(
            "scope",
            "--host and --all are mutually exclusive",
        )),
        (Some(h), false) => Ok(Scope::Host(h)),
        (None, true) => Ok(Scope::All),
        (None, false) => Ok(Scope::Local),
    }
}

/// Resolve the optional `--window` lower bound on `authored_at`, at `now_ms`.
///
/// Absent ⇒ `None` (no window — every record in scope survives). Present ⇒
/// `Some(now_ms - dur_ms)`: a record is kept iff `authored_at >= this bound`
/// ([`passes_window`]). The bound is saturating (a `--window` larger than
/// `now_ms` clamps to `i64::MIN` rather than wrapping positive, so it never
/// silently excludes everything). A malformed duration is a SYNC [`Refusal`].
fn window_lower_bound(now_ms: i64, m: &ArgMatches) -> Result<Option<i64>, Refusal> {
    match m.get_one::<String>("window") {
        None => Ok(None),
        Some(raw) => {
            let dur_ms = dispatch::origin_send::parse_expires(raw)
                .map_err(|msg| Refusal::refused("window", msg))?;
            Ok(Some(now_ms.saturating_sub(dur_ms)))
        }
    }
}

/// A record passes the `--window` filter iff its `authored_at` is at/after the
/// resolved lower bound. `None` bound ⇒ always kept (no window).
fn passes_window(authored_at: i64, lower_bound: Option<i64>) -> bool {
    match lower_bound {
        None => true,
        Some(lb) => authored_at >= lb,
    }
}

pub fn run(m: &ArgMatches) -> i32 {
    let env = RealEnv;
    // HOME is required to locate the qd data root (same posture as whoami/the
    // store). QD_HOME (honored via from_home_env) overrides the data root — this
    // MUST match the store + the send writer, so the verb reads exactly the
    // files `qd send` wrote.
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        eprintln!("qd dispositions: HOME is not set");
        return 1;
    };
    let paths = QdPaths::from_home_env(std::path::Path::new(&home), &env);

    let scope = match select_scope(m) {
        Ok(s) => s,
        Err(r) => return emit_refusal(&r),
    };
    let archive = m.get_flag("archive");
    let only = m.get_one::<String>("correlation_id").cloned();

    let now_ms = RealClock.now_ms();
    let lower_bound = match window_lower_bound(now_ms, m) {
        Ok(lb) => lb,
        Err(r) => return emit_refusal(&r),
    };

    let records = match query(&paths, &scope, archive, now_ms, only.as_deref()) {
        Ok(rs) => rs,
        Err(e) => {
            eprintln!("qd dispositions: failed reading disposition store: {e}");
            return 1;
        }
    };

    // Build the whole JSONL payload in memory first (so qd is never the source of
    // a mid-document truncation on a broken pipe — same discipline as ls.rs), one
    // record per line, filtered by the caller's window.
    let mut buf = String::new();
    for rec in &records {
        if passes_window(rec.authored_at, lower_bound) {
            buf.push_str(&rec.to_jsonl_line());
            buf.push('\n');
        }
    }
    emit_or_pipe_exit(&buf);
    0
}

/// Render a [`Refusal`] to stderr and return its exit code. The shared Refusal
/// stderr line reads `qd send: refused{…}: …`; that `qd send:` prefix is the
/// pinned machine-stable token of the failure family (contract §6), reused here
/// verbatim so a consumer keys on the SAME `{class,reason}` shape across doors.
fn emit_refusal(r: &Refusal) -> i32 {
    r.emit()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a `dispositions` argv into its subcommand matches through the REAL
    /// clap tree (so `conflicts_with` + arg wiring are exercised, not a hand-rolled
    /// Command that could drift from cli.rs).
    fn parse(args: &[&str]) -> Result<clap::ArgMatches, clap::Error> {
        let mut argv = vec!["qd", "dispositions"];
        argv.extend_from_slice(args);
        let top = crate::cli::build_cli().try_get_matches_from(argv)?;
        // Unwrap the subcommand matches (the layer run() receives).
        let (_, sub) = top.subcommand().expect("dispositions subcommand");
        Ok(sub.clone())
    }

    // ---- select_scope -------------------------------------------------------

    #[test]
    fn scope_defaults_to_local() {
        let m = parse(&[]).unwrap();
        assert_eq!(select_scope(&m).unwrap(), Scope::Local);
    }

    #[test]
    fn scope_host_flag() {
        let m = parse(&["--host", "peerbox"]).unwrap();
        assert_eq!(select_scope(&m).unwrap(), Scope::Host("peerbox".into()));
    }

    #[test]
    fn scope_all_flag() {
        let m = parse(&["--all"]).unwrap();
        assert_eq!(select_scope(&m).unwrap(), Scope::All);
    }

    #[test]
    fn scope_host_and_all_conflict_at_parse() {
        // clap's conflicts_with rejects the combination BEFORE run() sees it.
        let err = parse(&["--host", "h", "--all"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn scope_host_and_all_conflict_refusal_when_bypassing_clap() {
        // Belt-and-suspenders: if both somehow reach select_scope, it refuses
        // rather than silently preferring one. Build the matches for this verb
        // directly WITHOUT the top-level conflict wiring by re-declaring a minimal
        // Command mirroring the two args (no conflicts_with) so both can be set.
        use clap::{Arg, ArgAction, Command};
        let m = Command::new("dispositions")
            .arg(Arg::new("host").long("host").action(ArgAction::Set))
            .arg(Arg::new("all").long("all").action(ArgAction::SetTrue))
            .try_get_matches_from(["dispositions", "--host", "h", "--all"])
            .unwrap();
        let r = select_scope(&m).unwrap_err();
        assert_eq!(r.class, "scope");
    }

    // ---- window_lower_bound + passes_window ---------------------------------

    #[test]
    fn window_absent_is_no_bound() {
        let m = parse(&[]).unwrap();
        assert_eq!(window_lower_bound(1_000_000, &m).unwrap(), None);
    }

    #[test]
    fn window_present_subtracts_from_now() {
        // "30m" = 1_800_000 ms; bound = now - 1_800_000.
        let m = parse(&["--window", "30m"]).unwrap();
        assert_eq!(
            window_lower_bound(5_000_000, &m).unwrap(),
            Some(5_000_000 - 1_800_000)
        );
        // bare integer = seconds.
        let m = parse(&["--window", "90"]).unwrap();
        assert_eq!(window_lower_bound(5_000_000, &m).unwrap(), Some(5_000_000 - 90_000));
    }

    #[test]
    fn window_bad_form_is_refusal_class_window() {
        let m = parse(&["--window", "1.5h"]).unwrap();
        let r = window_lower_bound(1_000, &m).unwrap_err();
        assert_eq!(r.class, "window");
        assert!(r.stderr_line().contains("refused{window}"));
    }

    #[test]
    fn window_larger_than_now_saturates_not_wraps() {
        // A window far past now must clamp low (keep everything), never wrap to a
        // huge positive bound that would exclude all records.
        let m = parse(&["--window", "1d"]).unwrap();
        let lb = window_lower_bound(i64::MIN + 10, &m).unwrap().unwrap();
        assert_eq!(lb, i64::MIN, "saturating_sub clamps at the floor");
    }

    #[test]
    fn passes_window_boundary_is_inclusive() {
        // authored_at == bound is KEPT (>=).
        assert!(passes_window(1000, Some(1000)));
        assert!(passes_window(1001, Some(1000)));
        assert!(!passes_window(999, Some(1000)));
        // No bound keeps everything.
        assert!(passes_window(i64::MIN, None));
    }
}
