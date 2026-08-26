//! `provider::pi::residence` — the pi cross-process **residence layer** (item 1):
//! the dispatch-side machinery that makes a resident pi session reachable across
//! SEPARATE `qd` CLI invocations. pi, like ACP and UNLIKE codex, is stdio-only —
//! it has no self-listening server — so quorum WRITES the resident host. This is
//! the structural mirror of `acp_residence` (dispatch-side) (AFFIRMED by pi-lead as the
//! design intent): codex `create_daemon`/`resume_daemon` supply the *choreography*
//! + the pgid teardown (item 3); `acp_residence` supplies the *host-process*
//! pattern.
//!
//! - **[`run_pi_adapter`]** — the resident `qd pi-daemon` entry (NET-NEW; the
//!   `acp_residence::run_adapter` analog). Spawns + OWNS the `pi --mode rpc` child
//!   via [`crate::provider::pi::stdio::PiStdio`], binds the birth-id with a boot `get_state`,
//!   and serves the loopback ws front ([`serve_pi`]) until SIGTERM. Outlives the
//!   create verb that spawned it — that IS residence. **Moved base, OPTION B
//!   (P4DB `d44e869`):** the resident no longer constructs a `RegistryStatusSink`
//!   and no longer PUSHES pi events into one — that sink was deleted and status
//!   is now derived ON-READ (pi R3: pid is never read). The serve loop only
//!   drains pi's event buffer (bounded, discarded); the [`crate::provider::pi::status`] mapper
//!   → [`crate::provider::pi::republish::Republish`] contract is retained for A2's status poll.
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

use std::cell::RefCell;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::{Message, WebSocket};

use crate::provider::pi::stdio::rpc::{PiEvent, PiRpc};
use crate::provider::pi::stdio::PiStdio;

/// The hidden subcommand marker the resident is launched under (pre-clap in
/// `bin/qd/main.rs`, the `acp-daemon` precedent at main.rs:67). Also the
/// discriminator in the S6 cmdline-identity check.
///
/// WIRE-IN (gated, flagged for review): `bin/qd/main.rs` needs
/// `Some("pi-daemon") => return dispatch::provider::pi::daemon::residence::run_pi_adapter(&rest[1..])`
/// added pre-clap — a merge-seam site beyond the ProviderFx 10 + provider_for arm
/// + verb guard.
pub const PI_ADAPTER_VERB: &str = "pi-daemon";

/// The boot `get_state` readiness timeout (PA1: max observed 788ms; live sample
/// 595ms — ~2s is ample margin, per the handoff note + impl-plan §4.1).
const BOOT_TIMEOUT: Duration = Duration::from_secs(2);

/// Front-loop poll granularity — how often the accept/serve loop wakes to
/// re-check SHUTDOWN and to drain buffered pi events (bounded, discarded).
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Max reader-thread frames consumed by one resident pump pass.
///
/// This keeps each pass finite even if pi is writing continuously; sustained
/// boundedness comes from calling the pump on every idle and connected loop
/// iteration. With the current `POLL_INTERVAL`, steady-state consumption is
/// capped at `EVENT_DRAIN_MAX_FRAMES / POLL_INTERVAL` (~6.8k events/s);
/// boundedness over time therefore rests on pi's real sustained emission rate
/// staying under that pass-frequency ceiling, not on drain-to-empty behavior.
const EVENT_DRAIN_MAX_FRAMES: usize = 1024;

// ===========================================================================
// B1 — completed-turn counting as busy→idle EDGES (drop-immune BY CONSTRUCTION).
// ===========================================================================

/// Counts COMPLETED pi turns by reconciling TWO un-turn-id'd signal streams —
/// the drained event boundaries (`agent_start`/`agent_end`, precise but able to
/// LAG behind real time by a serve pass or be DROPPED on channel overflow) and
/// real-time `is_streaming` point-reads (the backstop). pi events carry no
/// turn-id, so attribution rests entirely on open/close bracketing + pi's FIFO
/// event ordering.
///
/// ## The guarantee — the BINDING NEVER-OVERCOUNT clause (fix-cycle 3)
/// `turns` is a SECONDARY accounting surface behind the primary gate (the exact,
/// drop-immune `is_streaming` point-read). A false-HIGH count is strictly WORSE
/// than a fail-safe low one (a high count masks lack of progress → a coordinator
/// moves on prematurely). So `turns` adopts a **never-overcount bias**:
/// * **Primary gate (`is_streaming` point-read): EXACT + drop-immune.** Unchanged,
///   live-verified on tool-turns. `turns` never affects it.
/// * **`turns` NEVER OVERCOUNTS** under no-drop operation OR any single dropped
///   event OR any event↔point-read interleaving. Proven across the full matrix
///   (H / A / B / C / E1 / E2 / G / races / 12-turn accumulation).
/// * **No-drop operation: exactly correct.** Every real turn counts once.
/// * **`turns` may UNDERCOUNT by one PER ambiguous corner.** The corner is a
///   `agent_start` observed while a turn is ALREADY believed open with NO
///   intervening idle observation — the surviving events `[start, start, end]`.
///   That is fundamentally ambiguous: the middle event could have been a real
///   terminal (→ 2 turns) OR a `willRetry:true` boundary (→ 1 retrying turn), and
///   the disambiguating bit is physically gone. Never-overcount ⇒ we (re)open
///   WITHOUT counting the prior, so a genuine dropped-real-terminal back-to-back
///   turn undercounts by one (a chain of K such undercounts by K). This is the
///   ACCEPTED, DOCUMENTED fundamental residual — it fixes Finding H (a dropped
///   `willRetry:true` boundary no longer overcounts) at the cost of the symmetric
///   dropped-real-terminal undercount, the direction the clause prizes.
/// * **The point-read backstop still counts genuine idle-gapped turns.** The
///   undercount above bites ONLY when there is NO observable inter-turn idle. If pi
///   really went idle between two back-to-back turns, a point-read of that idle
///   counts the first (via `observe(false)`), so genuine separated turns are NOT
///   lost — the undercount is confined to the truly-ambiguous no-idle corner.
/// * **Compound losses (E1/E2) + willRetry-with-dropped-terminal (C):** additional
///   documented UNDERcount residuals (never overcounts). Same fail-safe direction.
///
/// ## What was REMOVED in fix-cycle 3 (Finding H)
/// The round-1 "Finding A: count-the-prior on a start-while-open" rule is GONE —
/// it was the sole overcount path (a dropped `willRetry:true` boundary made the
/// retry-resume start count a spurious prior completion). A start-while-open is
/// fundamentally ambiguous (dropped-terminal new turn vs dropped-boundary retry
/// resume), so under the never-overcount clause it just (re)opens the turn.
/// Consequently `opened_by_event` (which only gated Finding A + the now-harmless
/// start-race) is also gone.
///
/// ## `pending_delayed_terminals` — the round-2 (Finding G/E1) reconciliation
/// A point-read that counts a close increments this; the delivered `agent_end`
/// for that same close (which by FIFO arrives BEFORE the next turn's `agent_start`)
/// consumes one without recounting — a COUNTER (not a bool) that SURVIVES a
/// point-read re-open (fixing G) and is invalidated by the next `agent_start` event
/// (FIFO: a pr-closed turn's real terminal precedes the next start).
#[derive(Debug, Default)]
struct TurnTracker {
    /// A turn is currently believed OPEN (started, not yet counted).
    streaming: bool,
    /// A `willRetry:true` boundary is open: the next `agent_start` is a RESUME, and
    /// point-read edges are suppressed until the retry's real terminal (Finding C).
    retry_pending: bool,
    /// FIFO-scoped terminal-dedup counter: point-read closes awaiting the delivered
    /// terminal that (by FIFO) will confirm them. Survives a point-read re-open
    /// (Finding G); invalidated by the next `agent_start` event (FIFO).
    pending_delayed_terminals: u32,
    /// Completed turns counted so far.
    completed: u64,
}

impl TurnTracker {
    /// Fold in ONE `is_streaming` point-read. Returns `true` iff it counted a
    /// completion (a real busy→idle edge of an open turn). This is the ONLY event
    /// path that legitimately counts a dropped-terminal turn — a dropped end WITH an
    /// OBSERVED idle (not the ambiguous no-idle corner) still counts here.
    fn observe(&mut self, streaming_now: bool) -> bool {
        if streaming_now {
            // pi reports busy. Open if not already open. Do NOT touch
            // `pending_delayed_terminals` (a re-open must not drop a pending FIFO
            // dedup — Finding G). Do NOT clear `retry_pending` (a busy reading during
            // a retry is the retry running).
            self.streaming = true;
            false
        } else if self.streaming && !self.retry_pending {
            // A believed-open turn is now idle → count the close. Record a pending
            // dedup so the delivered terminal for THIS close (FIFO-next) is consumed
            // rather than recounted.
            self.streaming = false;
            self.pending_delayed_terminals += 1;
            self.completed += 1;
            true
        } else {
            // Already idle, OR a `willRetry` gap (Finding C: the point-read is
            // willRetry-blind, so suppress its edge — the retry's real terminal
            // counts it). Status stays truthful; no count.
            self.streaming = false;
            false
        }
    }

    /// Fold a drained pi EVENT — the precise turn boundary. Returns `true` iff it
    /// counted a completion.
    fn observe_event(&mut self, event: &PiEvent) -> bool {
        match event {
            PiEvent::AgentStart => {
                // FIFO invalidation: reaching the next turn's start means any pending
                // pr-close's delivered terminal (which by FIFO precedes this start)
                // has already arrived-and-deduped OR was dropped — either way it can
                // no longer consume THIS or a later turn's terminal. Clear the batch.
                self.pending_delayed_terminals = 0;
                // A retry RESUME clears the boundary; a start-while-open is otherwise
                // fundamentally ambiguous (dropped-terminal new turn vs dropped
                // willRetry-boundary resume). NEVER-OVERCOUNT (fix-cycle 3, Finding H):
                // we (re)open WITHOUT counting a prior completion in BOTH cases. The
                // removed "Finding A count-the-prior" was the sole overcount path.
                self.retry_pending = false;
                self.streaming = true;
                false
            }
            // A `willRetry:true` boundary is NOT terminal — stay busy; the next
            // `agent_start` is a resume (`retry_pending`), not a prior completion.
            PiEvent::AgentEnd { will_retry: true } => {
                self.retry_pending = true;
                self.streaming = true;
                false
            }
            // A delivered TERMINAL ends any retry sequence.
            PiEvent::AgentEnd { will_retry: false } => {
                self.retry_pending = false;
                if self.pending_delayed_terminals > 0 {
                    // The FIFO-next delivered terminal CONFIRMS a prior point-read
                    // close → consume one, do NOT recount, and do NOT touch
                    // `streaming` (this terminal belonged to an already-counted PRIOR
                    // turn, not the current open one). Surviving a point-read re-open
                    // is what fixes Finding G.
                    self.pending_delayed_terminals -= 1;
                    false
                } else if self.streaming {
                    // Normal close.
                    self.streaming = false;
                    self.completed += 1;
                    true
                } else {
                    // Finding B: a terminal while idle with no pending dedup ⇒ a turn
                    // whose `agent_start` we never observed (dropped start) → count.
                    // Cannot overcount under a drop: a drop REMOVES events, and pi
                    // emits exactly one terminal per run (a duplicate — Finding F — is
                    // unreachable, and is not a drop scenario).
                    self.completed += 1;
                    true
                }
            }
            // Every other event is status-irrelevant to the counter.
            _ => false,
        }
    }
}

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
        // Finding D fix: match the endpoint as the EXACT `--listen <ep>` argument
        // token, NOT a substring — a recorded port ":900" must not string-nest into
        // a live daemon's ":9001". Accept both `--listen <ep>` and `--listen=<ep>`
        // (the two argv spellings [`build_pi_adapter_argv`] / clap accept).
        Some(ep) => {
            let mut toks = cmd.split_whitespace();
            while let Some(t) = toks.next() {
                if t == "--listen" {
                    if toks.next() == Some(ep) {
                        return true;
                    }
                } else if t.strip_prefix("--listen=") == Some(ep) {
                    return true;
                }
            }
            false
        }
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

    // B1: one turn-tracker for the life of the resident. Its count survives repeated
    // turns and is served through the front `get_state` reply (the ls/info surface).
    let tracker = RefCell::new(TurnTracker::default());
    let code = serve_pi(&pi, &listener, &SHUTDOWN, &tracker);

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
fn serve_pi(
    pi: &PiStdio,
    listener: &TcpListener,
    shutdown: &AtomicBool,
    tracker: &RefCell<TurnTracker>,
) -> i32 {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return 0;
        }
        // Idle drain: consume buffered pi events (bounded), folding each into the
        // turn tracker's streaming belief; then a backstop is_streaming point-read
        // reconciles a dropped `agent_end` (B1). NO client is required for either.
        pump_events(pi, tracker);
        backstop_poll(pi, tracker);

        listener.set_nonblocking(true).ok();
        match listener.accept() {
            Ok((stream, _peer)) => {
                listener.set_nonblocking(false).ok();
                if let Ok(ws) = tungstenite::accept(stream) {
                    handle_connection(pi, ws, shutdown, tracker);
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
/// **OPTION B:** status is still derived on-read (the resident pushes nothing to
/// any sink). **B1:** each drained event is ALSO folded into the [`TurnTracker`]'s
/// streaming belief ([`TurnTracker::observe_event`]) — the drop-immune completed-
/// turn count's common (non-dropped) path. This is a pure in-process fold, no I/O:
/// it never issues a `get_state`, so it cannot block on the pi child (the backstop
/// point-read lives in [`backstop_poll`], kept OUT of the drain for that reason).
fn pump_events(pi: &PiStdio, tracker: &RefCell<TurnTracker>) {
    // A tight bound so the loop stays responsive to accept()/shutdown; events that
    // arrive later are caught on the next pass (the reader thread buffers them).
    let _ = pi.drain_events(EVENT_DRAIN_MAX_FRAMES, |event| {
        tracker.borrow_mut().observe_event(&event);
    });
    log_frame_counts(pi);
}

/// The drop-immune BACKSTOP (B1): an `is_streaming` point-read reconciles the turn
/// tracker when the event stream could not. Kept SEPARATE from [`pump_events`]
/// because it issues a `get_state` round-trip (which the pure drain must never do).
///
/// Cadence, bounded on purpose: poll EVERY pass while a turn is believed open —
/// that is the window where a dropped `agent_end` would otherwise leave the count
/// short and the belief stuck — AND sample every 8th otherwise-idle pass (~1.2s at
/// [`POLL_INTERVAL`]) so a turn whose `agent_start` was itself dropped is still
/// discovered. An idle resident therefore wakes the pi child at most ~once/1.2s.
/// A `get_state` error (pi gone) leaves the belief untouched; the next reader's
/// own point-read (and the status derivation) still see the real state.
fn backstop_poll(pi: &PiStdio, tracker: &RefCell<TurnTracker>) {
    use std::sync::atomic::AtomicU64;
    static POLL_PASS: AtomicU64 = AtomicU64::new(0);
    let n = POLL_PASS.fetch_add(1, Ordering::Relaxed);
    let believed_streaming = tracker.borrow().streaming;
    if believed_streaming || n % 8 == 0 {
        if let Ok(state) = pi.get_state() {
            tracker.borrow_mut().observe(state.is_streaming);
        }
    }
}

/// B2 live-dogfood diagnostic: emit the reader thread's received/dropped
/// frame counters to the resident's own stderr (captured to `pi-<name>.log`)
/// roughly every 50 poll passes (~7.5s at `POLL_INTERVAL`). Throttled so a
/// long-lived idle resident doesn't spam its log; not on any control path.
fn log_frame_counts(pi: &PiStdio) {
    use std::sync::atomic::AtomicU64;
    static PASS: AtomicU64 = AtomicU64::new(0);
    let n = PASS.fetch_add(1, Ordering::Relaxed);
    if n % 50 == 0 {
        let (received, dropped) = pi.frame_counts();
        eprintln!("pi-daemon: frame_counts received={received} dropped={dropped}");
    }
}

/// Serve one ws connection: read front requests `{id, m, …}`, drive `PiStdio`,
/// write `{id, ok}` / `{id, e}`. Returns on Close/EOF/transport (the resident is
/// untouched; the next connection re-attaches). Mirror of
/// `acp::wire::handle_connection`.
fn handle_connection(
    pi: &PiStdio,
    mut ws: WebSocket<TcpStream>,
    shutdown: &AtomicBool,
    tracker: &RefCell<TurnTracker>,
) {
    let _ = ws.get_ref().set_read_timeout(Some(POLL_INTERVAL));
    loop {
        if shutdown.load(Ordering::SeqCst) {
            let _ = ws.close(None);
            return;
        }
        // Drain + reconcile even while a client is connected but quiet (B1): the
        // same turn-tracking the idle serve loop does, so a completion is counted
        // whether or not a client is attached.
        pump_events(pi, tracker);
        backstop_poll(pi, tracker);

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
        let frame = match dispatch_front(pi, &req, tracker) {
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
fn dispatch_front(
    pi: &PiStdio,
    req: &Value,
    tracker: &RefCell<TurnTracker>,
) -> Result<Value, String> {
    let m = req
        .get("m")
        .and_then(Value::as_str)
        .ok_or_else(|| "request missing method 'm'".to_string())?;
    let arg_str = |k: &str| {
        req.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match m {
        // Readiness/health probe (the connect_ready poll reads session_id).
        "status" => Ok(json!({
            "session_id": pi.get_state().ok().map(|s| s.session_id),
        })),
        // The B1 observability point-read: the live streaming flag AND the
        // drop-immune completed-turn count, from ONE round-trip. Every reader
        // `get_state` (the `qd wait` poll, the ls/info gather) also RECONCILES the
        // tracker against this fresh `is_streaming` — so any reader's observation
        // is itself a backstop that heals a dropped `agent_end`.
        "get_state" => pi
            .get_state()
            .map(|s| {
                tracker.borrow_mut().observe(s.is_streaming);
                json!({
                    "session_id": s.session_id,
                    "is_streaming": s.is_streaming,
                    "turns": tracker.borrow().completed,
                })
            })
            .map_err(|e| e.to_string()),
        "prompt" => {
            // Default to steer (option-(i): start-if-idle / steer-if-busy).
            let behavior = Some(crate::provider::pi::stdio::rpc::StreamingBehavior::Steer);
            pi.prompt(&arg_str("message"), behavior)
                .map(|turn_id| json!({ "turn_id": turn_id }))
                .map_err(|e| e.to_string())
        }
        "steer" => pi
            .steer(&arg_str("message"))
            .map(|_| json!({}))
            .map_err(|e| e.to_string()),
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
        assert!(cmdline_is_our_pi_daemon(
            Some("qd pi-daemon --listen x"),
            None
        ));
    }

    /// The acp twin's D6 check: `pi-daemon` is on the `qw` binary too now, so the
    /// probe must latch a resident spawned as `qw pi-daemon` (every new one) AND one
    /// spawned as `qd pi-daemon` (every one already running when it landed). It keys
    /// on [`PI_ADAPTER_VERB`], never on the program name.
    #[test]
    fn cmdline_identity_is_program_agnostic_across_the_qd_qw_move() {
        let ep = "ws://127.0.0.1:9000";
        for exe in ["/usr/local/bin/qd", "/usr/local/bin/qw"] {
            let cmd = format!("{exe} pi-daemon --listen {ep} --cwd /w");
            assert!(
                cmdline_is_our_pi_daemon(Some(&cmd), Some(ep)),
                "the {exe} spelling must latch: {cmd}"
            );
            assert!(cmdline_is_our_pi_daemon(Some(&cmd), None));
        }
    }

    // =======================================================================
    // B1 — the completed-turn count as a busy→idle EDGE (drop-immune). These are
    // DETERMINISTIC: they drive the [`TurnTracker`] directly, so a dropped
    // `agent_end` is FORCED (simply never fed as an event) rather than raced.
    // =======================================================================

    /// Baseline: a clean turn (agent_start → agent_end) counts exactly once, and
    /// repeated turns accumulate — the count survives across turns (clause 4).
    #[test]
    fn clean_turns_count_on_the_busy_to_idle_edge() {
        let mut t = TurnTracker::default();
        assert_eq!(t.completed, 0);
        // Turn 1 via the event path.
        assert!(!t.observe_event(&PiEvent::AgentStart));
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(t.completed, 1);
        assert!(!t.streaming, "idle after a clean turn");
        // Turn 2 — accumulates.
        t.observe_event(&PiEvent::AgentStart);
        t.observe_event(&PiEvent::AgentEnd { will_retry: false });
        assert_eq!(t.completed, 2);
    }

    /// THE FORCED-DROP PROOF (clause 3 / the inviolable constraint). A turn's
    /// `agent_start` is delivered but its terminal `agent_end` is DROPPED (never
    /// fed as an event — the deterministic drop). The subsequent `is_streaming`
    /// point-read (the backstop, or any reader's `get_state`) observes idle and
    /// RECONCILES: the session is (a) NOT stuck busy and (b) the turn is counted
    /// exactly once. A raw-event counter would have missed it entirely.
    #[test]
    fn dropped_agent_end_is_reconciled_by_the_is_streaming_edge() {
        let mut t = TurnTracker::default();
        // agent_start delivered → the turn is believed open.
        t.observe_event(&PiEvent::AgentStart);
        assert!(t.streaming);
        assert_eq!(t.completed, 0);

        // ---- agent_end DROPPED: no observe_event for the terminal. ----
        // The event path alone is now stuck-busy at count 0 (the defect a raw
        // agent_end counter would exhibit) — assert that pre-reconciliation state
        // to show the point-read is load-bearing, not decorative.
        assert!(
            t.streaming,
            "event path alone: still (wrongly) believed busy"
        );
        assert_eq!(
            t.completed, 0,
            "event path alone: the completion is uncounted"
        );

        // The drop-immune backstop: an is_streaming point-read reads FALSE.
        let counted = t.observe(false);
        assert!(counted, "the busy→idle edge fires on the point-read");
        assert!(!t.streaming, "reconciled: NOT stuck busy");
        assert_eq!(
            t.completed, 1,
            "reconciled: counted exactly once, not missed"
        );
    }

    /// Idempotence across signals: when BOTH the event path and a redundant
    /// point-read observe the same close, the turn is counted ONCE — the event
    /// path and the poll path never double-count (clause 3's "must NOT miscount").
    #[test]
    fn event_and_poll_observing_the_same_close_do_not_double_count() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart);
        // The common path: agent_end delivered → counted once.
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(t.completed, 1);
        // A reader's point-read now ALSO reads idle — no second edge (already idle).
        assert!(!t.observe(false));
        assert_eq!(t.completed, 1, "the same close counts once, not twice");
    }

    /// A `willRetry:true` boundary is an auto-retry, not a terminal (parity with
    /// `PiStatusMapper`): it does not count and the turn stays busy until the real
    /// terminal. Also confirms a point-read during the retry window stays busy.
    #[test]
    fn will_retry_boundary_is_not_a_completion() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart);
        assert!(!t.observe_event(&PiEvent::AgentEnd { will_retry: true }));
        assert_eq!(t.completed, 0);
        assert!(t.streaming, "still busy across a retry boundary");
        // A point-read mid-retry still reads streaming (pi has not gone idle).
        assert!(!t.observe(true));
        assert_eq!(t.completed, 0);
        // The real terminal finally counts it.
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(t.completed, 1);
    }

    /// A short turn the ~150ms point-read would SKIP (start+end both between two
    /// polls) is still counted, because both `agent_start` and `agent_end` are
    /// delivered as events — the complementary half of the reconciliation.
    #[test]
    fn short_turn_between_polls_is_caught_by_events() {
        let mut t = TurnTracker::default();
        // No point-read ever fires; only the event pair.
        t.observe_event(&PiEvent::AgentStart);
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(t.completed, 1);
    }

    // =======================================================================
    // ROUND-1 RED-TEAM REGRESSION GUARDS — the red-team's exact defect
    // sequences, now asserting the FIXED behavior (a PASS here = the defect is
    // gone). Ported from b1-redteam-round1-findings.md (findings A, B, C-interaction,
    // D). Deterministic at the tracker/function seam.
    // =======================================================================

    /// FINDING A under the NEVER-OVERCOUNT clause (fix-cycle 3): a dropped `agent_end`
    /// + the next turn's `agent_start` with NO intervening idle is the ambiguous
    /// `[start, start, end]` corner (dropped real terminal → 2, OR dropped willRetry
    /// boundary → 1 — indistinguishable). Never-overcount ⇒ the start-while-open
    /// re-opens WITHOUT counting the prior, so this UNDERCOUNTS to 1 (the ACCEPTED
    /// residual). It never overcounts. (Round-1 counted 2 here — that same rule
    /// overcounted Finding H, so it was removed.)
    #[test]
    fn rt_a_dropped_end_back_to_back_undercounts_never_overcounts() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart); // turn 1 opens
                                               // turn 1's agent_end DROPPED — no event.
        assert!(
            !t.observe_event(&PiEvent::AgentStart),
            "start-while-open re-opens WITHOUT counting the prior (never-overcount)"
        );
        assert_eq!(t.completed, 0, "the ambiguous prior is NOT counted");
        assert!(t.streaming, "turn 2 is open");
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false })); // turn 2 closes
        assert_eq!(
            t.completed, 1,
            "ACCEPTED residual: 2 turns undercount to 1 (never 2+ overcount)"
        );
    }

    /// The undercount above bites ONLY the truly-ambiguous NO-IDLE corner: a genuine
    /// back-to-back turn whose inter-turn idle IS observed by a point-read is still
    /// counted (the backstop `observe(false)` counts turn 1 before turn 2 re-opens).
    #[test]
    fn rt_a_dropped_end_with_observed_idle_still_counts_both() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart); // turn 1 opens
                                               // turn 1's agent_end DROPPED, BUT pi really went idle and a point-read saw it:
        assert!(
            t.observe(false),
            "the backstop point-read counts turn 1's real close"
        );
        assert_eq!(t.completed, 1);
        t.observe_event(&PiEvent::AgentStart); // turn 2 (invalidates the pending dedup)
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(
            t.completed, 2,
            "genuine idle-gapped turns are NOT lost — only the no-idle corner is"
        );
    }

    /// FINDING B (was: count 0). A dropped `agent_start` with a delivered terminal
    /// now counts the turn (terminal-while-idle ⇒ a turn whose start we missed).
    #[test]
    fn rt_b_dropped_start_then_delivered_end_now_counts() {
        let mut t = TurnTracker::default();
        // agent_start DROPPED — belief never rises.
        assert!(
            t.observe_event(&PiEvent::AgentEnd { will_retry: false }),
            "a delivered terminal while idle counts the dropped-start turn"
        );
        assert_eq!(
            t.completed, 1,
            "FIXED: dropped-start + delivered end counts 1 (was 0)"
        );
        assert!(!t.streaming, "and not stuck busy");
    }

    /// FINDING B via the POINT-READ path: a dropped-start turn caught busy by a
    /// point-read (an active reader / an on-turn backstop sample) also counts.
    #[test]
    fn rt_b_dropped_start_caught_by_pointread_counts() {
        let mut t = TurnTracker::default();
        t.observe(true); // point-read samples the turn busy (start was dropped)
        assert_eq!(t.completed, 0);
        assert!(t.observe(false)); // and idle → count
        assert_eq!(t.completed, 1);
    }

    /// A clean retry-turn counts EXACTLY once (start, willRetry boundary, resume,
    /// terminal → 1), and a following dropped-end back-to-back turn undercounts under
    /// the never-overcount clause (never overcounts). Confirms the retry path and the
    /// residual coexist without corruption.
    #[test]
    fn rt_willretry_turn_counts_once_then_dropped_end_undercounts() {
        let mut t = TurnTracker::default();
        // A completed retry-turn first (start, retry boundary, resume, terminal).
        t.observe_event(&PiEvent::AgentStart);
        t.observe_event(&PiEvent::AgentEnd { will_retry: true });
        t.observe_event(&PiEvent::AgentStart); // retry resume (not counted)
        t.observe_event(&PiEvent::AgentEnd { will_retry: false });
        assert_eq!(t.completed, 1, "the retry-turn counts once, not twice");
        // A new turn whose end is DROPPED, then the next turn's start (ambiguous corner).
        t.observe_event(&PiEvent::AgentStart);
        assert!(
            !t.observe_event(&PiEvent::AgentStart),
            "never-overcount: the dropped-end prior is NOT counted at the next start"
        );
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(
            t.completed, 2,
            "3 real turns (retry + dropped-end + clean) → counted 2 (the dropped-end undercounts); never overcounts"
        );
    }

    /// willRetry↔A INTERACTION, case 2 (the overcount trap): the retry-RESUME
    /// `agent_start` must NOT be counted as a prior completion.
    #[test]
    fn rt_willretry_resume_start_does_not_count() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart);
        assert!(!t.observe_event(&PiEvent::AgentEnd { will_retry: true }));
        assert_eq!(t.completed, 0);
        // The retry resumes with another agent_start — NOT a prior completion.
        assert!(
            !t.observe_event(&PiEvent::AgentStart),
            "retry-resume start is not a completion"
        );
        assert_eq!(t.completed, 0, "no overcount at the retry resume");
        // The real terminal counts the ONE logical turn exactly once.
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(t.completed, 1);
    }

    /// FINDING C hardening: a point-read observing idle DURING a retry gap is
    /// SUPPRESSED (the point-read path is willRetry-aware), so no spurious
    /// completion, and the real terminal counts the turn exactly once (no overcount).
    #[test]
    fn rt_c_willretry_pointread_idle_blip_is_suppressed_no_overcount() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart);
        t.observe_event(&PiEvent::AgentEnd { will_retry: true }); // retry boundary
        assert!(
            !t.observe(false),
            "point-read idle mid-retry is suppressed (no spurious edge)"
        );
        assert_eq!(t.completed, 0, "no overcount from the retry-gap point-read");
        t.observe_event(&PiEvent::AgentStart); // retry resume
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(
            t.completed, 1,
            "one logical turn counted once (was 2 = overcount)"
        );
    }

    /// EVENT↔POINT-READ RACE (start side): a point-read provisionally opens a turn
    /// whose real `agent_start` then arrives (delayed across serve passes). The
    /// delayed start just re-affirms the open — it never counts (Finding A, the only
    /// path that would have spuriously counted here, is removed under never-overcount).
    #[test]
    fn rt_race_pointread_open_then_delayed_start_no_overcount() {
        let mut t = TurnTracker::default();
        t.observe(true); // point-read sees busy first (provisional open)
        assert!(
            !t.observe_event(&PiEvent::AgentStart),
            "the delayed real start re-affirms the open, never a spurious count"
        );
        assert_eq!(t.completed, 0, "no spurious count on start-while-open");
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(t.completed, 1, "the turn counts exactly once");
    }

    /// EVENT↔POINT-READ RACE (terminal side): a point-read closes a turn (counts),
    /// then the delivered terminal for that SAME close arrives (delayed). It must be
    /// consumed WITHOUT recounting. Guarded by `pending_delayed_terminals`.
    #[test]
    fn rt_race_pointread_close_then_delayed_terminal_no_double_count() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart);
        assert!(t.observe(false)); // point-read races ahead of the terminal → counts
        assert_eq!(t.completed, 1);
        assert!(
            !t.observe_event(&PiEvent::AgentEnd { will_retry: false }),
            "the delivered terminal for an already-counted close is deduped"
        );
        assert_eq!(t.completed, 1, "no double count");
        // A subsequent genuine turn still counts (the dedup consumed exactly one).
        t.observe_event(&PiEvent::AgentStart);
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(t.completed, 2);
    }

    /// CHARACTERIZATION (documented residual, beyond the single-drop clause): a turn
    /// with BOTH its start AND end unobserved by events, sub-poll-gap, no concurrent
    /// reader — modeled as the shipped backstop cadence `believed || n%8==0` over a
    /// real is_streaming timeline — is uncounted. Status stays truthful (not stuck).
    /// This is a DOUBLE-drop (two lost signals), not the single-drop teeth clause.
    #[test]
    fn residual_double_unobserved_subgap_turn_is_uncounted_but_status_truthful() {
        let real = [false, false, true, true, true, true, false, false, false];
        let mut t = TurnTracker::default();
        for (n, &s) in real.iter().enumerate() {
            if t.streaming || n % 8 == 0 {
                t.observe(s);
            }
        }
        assert_eq!(
            t.completed, 0,
            "documented residual: double-unobserved sub-gap turn"
        );
        assert!(!t.streaming, "but status is truthful — never stuck busy");
    }

    /// FINDING D: the identity guard now matches the EXACT `--listen <ep>` token —
    /// a recorded port ":900" no longer string-nests into a live ":9001".
    #[test]
    fn rt_d_identity_guard_exact_token_not_substring() {
        let live = "/usr/bin/qd pi-daemon --listen ws://127.0.0.1:9001 --cwd /w";
        // Prefix-nesting endpoint must NO LONGER match (was a false positive).
        assert!(!cmdline_is_our_pi_daemon(
            Some(live),
            Some("ws://127.0.0.1:900")
        ));
        // Exact-port still matches (the common case is preserved).
        assert!(cmdline_is_our_pi_daemon(
            Some(live),
            Some("ws://127.0.0.1:9001")
        ));
        // The `--listen=<ep>` spelling also matches exactly.
        assert!(cmdline_is_our_pi_daemon(
            Some("qd pi-daemon --listen=ws://127.0.0.1:9001 --cwd /w"),
            Some("ws://127.0.0.1:9001")
        ));
        assert!(!cmdline_is_our_pi_daemon(
            Some("qd pi-daemon --listen=ws://127.0.0.1:9001 --cwd /w"),
            Some("ws://127.0.0.1:900")
        ));
    }

    // =======================================================================
    // ROUND-2 RED-TEAM REGRESSION GUARDS — G FIXED (fixed-behavior asserts);
    // E1/E2 documented compound residuals (residual-behavior asserts). Ported
    // from b1-redteam-round2-findings.md. Deterministic at the tracker seam.
    // =======================================================================

    /// FINDING G (was: 2 turns counted as 3 — an OVERCOUNT with NO drops). The
    /// FIFO-scoped `pending_delayed_terminals` COUNTER survives a point-read re-open
    /// (the round-1 bool did not), so turn1's DELAYED terminal is deduped even after
    /// a point-read opens turn2 — no premature second count. 2 turns → 2. FIXED.
    #[test]
    fn rt2_g_reopen_does_not_defeat_terminal_dedup_now_counts_two() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart); // drain delivers turn1's start
        assert!(t.observe(false)); // point-read counts turn1's close
        assert_eq!(t.completed, 1);
        assert_eq!(t.pending_delayed_terminals, 1);
        // A point-read samples turn2 busy (provisional re-open) — must NOT drop the
        // pending dedup.
        assert!(!t.observe(true));
        assert_eq!(
            t.pending_delayed_terminals, 1,
            "the re-open must NOT clear the pending dedup"
        );
        // turn1's DELAYED terminal drains — deduped (not misapplied to turn2).
        assert!(
            !t.observe_event(&PiEvent::AgentEnd { will_retry: false }),
            "turn1's delayed terminal is deduped, not counted as turn2's close"
        );
        assert_eq!(
            t.completed, 1,
            "no premature second count (was 2 = the G overcount)"
        );
        // turn2's real start + terminal.
        t.observe_event(&PiEvent::AgentStart);
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(t.completed, 2, "FIXED: 2 turns counted as 2 (was 3)");
    }

    /// The G-fix must NOT regress the single-drop case it is entangled with: turn1's
    /// end DROPPED (not delayed), a point-read closes turn1 AND opens turn2, then
    /// turn2 is fully DELIVERED. The next `agent_start` INVALIDATES the pending dedup
    /// (FIFO: turn1's terminal would have preceded it), so turn2's terminal counts.
    /// 2 turns → 2 (NOT undercounted).
    #[test]
    fn rt2_g_dropped_end_then_pointread_reopen_then_delivered_turn2_counts_two() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart); // turn1 start
                                               // turn1 end DROPPED.
        assert!(t.observe(false)); // point-read closes turn1
        assert_eq!(t.completed, 1);
        assert!(!t.observe(true)); // point-read opens turn2 (provisional)
                                   // turn2's real start invalidates the (now-stale, terminal-dropped) pending:
        assert!(!t.observe_event(&PiEvent::AgentStart));
        assert_eq!(
            t.pending_delayed_terminals, 0,
            "a start invalidates a stale pending dedup"
        );
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(
            t.completed, 2,
            "turn2's terminal counts — the dropped-end path is NOT regressed"
        );
    }

    /// FINDING E1 — DOCUMENTED COMPOUND RESIDUAL (undercount 2→1). A point-read
    /// closes turn1 (its end dropped), then turn2's start is ALSO dropped and turn2
    /// is never sampled busy → turn2's delivered terminal is deduped against turn1's
    /// pending close. TWO adjacent single-drops with no disambiguating point-read;
    /// un-ID'd events cannot tell turn2's terminal from turn1's delayed one. Biasing
    /// to count here would overcount the common Finding-G delayed-terminal case
    /// (worse). Status stays truthful.
    #[test]
    fn rt2_e1_documented_residual_leaked_dedup_undercounts() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart); // turn1
                                               // turn1 end DROPPED.
        assert!(t.observe(false)); // point-read closes turn1 (pending=1)
        assert_eq!(t.completed, 1);
        // turn2 start DROPPED, turn2 not sampled busy → its terminal is deduped:
        assert!(!t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(
            t.completed, 1,
            "DOCUMENTED RESIDUAL: compound adjacent drops undercount 2→1"
        );
        assert!(
            !t.streaming,
            "status truthful (idle) — the GATE is not wedged"
        );
    }

    /// FINDING E2 — DOCUMENTED COMPOUND RESIDUAL (undercount 2→1). Adjacent dropped
    /// end(turn1) + dropped start(turn2), NO point-read between → the only surviving
    /// events [start(t1), end(t2)] are indistinguishable from ONE clean turn. Same
    /// fundamental un-ID'd compound class as E1 and the double-drop residual.
    #[test]
    fn rt2_e2_documented_residual_erased_boundary_undercounts() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart); // turn1
                                               // turn1 end DROPPED; turn2 start DROPPED; no point-read.
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(
            t.completed, 1,
            "DOCUMENTED RESIDUAL: erased boundary undercounts 2→1"
        );
        assert!(!t.streaming, "status truthful (idle)");
    }

    /// A CHAIN of back-to-back dropped ends is the ambiguous corner repeated — each
    /// start-while-open could be a dropped-terminal new turn OR a dropped-boundary
    /// retry resume. Never-overcount ⇒ NONE of the start-while-opens count, so a chain
    /// of K dropped-end turns UNDERCOUNTS by K (only the final terminal's turn counts).
    /// This makes the documented undercount potentially > 1 (a whole chain), the
    /// precise breadth of the residual. It NEVER overcounts.
    /// start,(drop),start,(drop),start,(drop),start,end → 1 (was 4 with the removed A rule).
    #[test]
    fn rt2_a_chain_of_dropped_ends_undercounts_never_overcounts() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart);
        assert!(!t.observe_event(&PiEvent::AgentStart)); // re-open, no count
        assert!(!t.observe_event(&PiEvent::AgentStart)); // re-open, no count
        assert!(!t.observe_event(&PiEvent::AgentStart)); // re-open, no count
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false })); // final close
        assert_eq!(
            t.completed, 1,
            "4 real turns → counted 1 (chain undercount, breadth of the residual); NEVER overcounts"
        );
    }

    /// FINDING F — REFUTED as a fixable "cheap safe guard" (documented ruling). The
    /// F repro [start, end, end] (a DUPLICATE terminal) is the IDENTICAL surviving-
    /// event sequence, with IDENTICAL tracker state, to a REACHABLE single-drop case:
    /// a normal turn followed by a turn whose `agent_start` was dropped
    /// [start(t1), end(t1), <start(t2) dropped>, end(t2)]. No guard can distinguish
    /// them without turn-ids. F is UNREACHABLE (pi wire law: one `agent_end` per run,
    /// FIFO single-reader — no duplicate terminal); the dropped-start-after-a-turn
    /// case IS reachable (a single drop). So the tracker COUNTS the third event —
    /// correct for the reachable case — and a suppress-guard would REGRESS it. This
    /// test pins both readings of the same sequence to lock the trade-off.
    #[test]
    fn rt2_f_terminal_after_terminal_counts_favoring_reachable_dropped_start() {
        // Reading 1 (REACHABLE, single dropped start of t2) — MUST count 2:
        let mut reachable = TurnTracker::default();
        reachable.observe_event(&PiEvent::AgentStart); // t1
        assert!(reachable.observe_event(&PiEvent::AgentEnd { will_retry: false })); // t1 close → 1
                                                                                    // t2 start DROPPED
        assert!(
            reachable.observe_event(&PiEvent::AgentEnd { will_retry: false }),
            "Finding B: t2's terminal (start dropped) counts — a REACHABLE single drop"
        );
        assert_eq!(
            reachable.completed, 2,
            "reachable dropped-start-after-a-turn must count 2"
        );
        // Reading 2 (UNREACHABLE duplicate terminal) is the SAME sequence + state, so
        // it necessarily counts the same way — an unreachable overcount we accept
        // rather than regress the reachable case above. No safe guard distinguishes them.
    }

    /// The gate stays truthful under EVERY round-2 counter defect: in G/E1/E2 the
    /// `streaming` belief ends FALSE matching reality (a wrong COUNT never wedges the
    /// STATUS). Asserted inline in the E1/E2/G tests too; consolidated here.
    #[test]
    fn rt2_held_status_truthful_under_all_counter_defects() {
        // E1 end-state.
        let mut a = TurnTracker::default();
        a.observe_event(&PiEvent::AgentStart);
        a.observe(false);
        a.observe_event(&PiEvent::AgentEnd { will_retry: false });
        assert!(!a.streaming, "E1: idle belief matches reality");
        // E2 end-state.
        let mut b = TurnTracker::default();
        b.observe_event(&PiEvent::AgentStart);
        b.observe_event(&PiEvent::AgentEnd { will_retry: false });
        assert!(!b.streaming, "E2: idle belief matches reality");
    }

    // =======================================================================
    // ROUND-4 (Finding H) + the BINDING NEVER-OVERCOUNT clause (fix-cycle 3).
    // H is FIXED (now counts 1, was the overcount 2). Its controls + a
    // never-overcount sweep across the whole matrix are the acceptance surface.
    // =======================================================================

    /// FINDING H (was: a SINGLE dropped `agent_end{willRetry:true}` boundary → 2, an
    /// OVERCOUNT). The boundary drop means `retry_pending` never arms, so the resume
    /// start used to hit the removed Finding-A rule. Now start-while-open never counts
    /// → the ONE retrying turn counts exactly 1. NEVER-OVERCOUNT.
    #[test]
    fn rt4_h_dropped_retry_boundary_no_longer_overcounts() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart); // the retrying turn opens
                                               // agent_end{willRetry:true} DROPPED — the ONLY dropped event; retry_pending never arms.
        assert!(
            !t.observe_event(&PiEvent::AgentStart), // the retry RESUME start
            "start-while-open does not count (was the H overcount)"
        );
        assert_eq!(t.completed, 0);
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false })); // the real terminal
        assert_eq!(
            t.completed, 1,
            "FIXED: one retrying turn counts 1 (was 2 = the H overcount)"
        );
    }

    /// H must not overcount even with FAITHFUL point-reads (is_streaming stays TRUE
    /// throughout a retry, per the live probe — so point-reads see busy and add nothing).
    #[test]
    fn rt4_h_no_overcount_with_faithful_pointreads() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart);
        assert!(!t.observe(true)); // point-read: busy (retry running)
                                   // boundary DROPPED; resume start:
        assert!(!t.observe_event(&PiEvent::AgentStart));
        assert!(!t.observe(true)); // point-read: still busy
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(
            t.completed, 1,
            "faithful point-reads do not disambiguate → still 1, no overcount"
        );
    }

    /// H control: dropping BOTH the boundary AND the resume start → the two omissions
    /// cancel and the single turn counts 1 (H is specific to a boundary-ONLY drop).
    #[test]
    fn rt4_h_control_dropped_boundary_and_resume_counts_one() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart);
        // boundary DROPPED, resume start DROPPED.
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(t.completed, 1);
    }

    /// H control: dropping only the resume start (boundary delivered) → 1.
    #[test]
    fn rt4_h_control_dropped_resume_only_counts_one() {
        let mut t = TurnTracker::default();
        t.observe_event(&PiEvent::AgentStart);
        t.observe_event(&PiEvent::AgentEnd { will_retry: true }); // boundary delivered → retry_pending
                                                                  // resume start DROPPED.
        assert!(t.observe_event(&PiEvent::AgentEnd { will_retry: false }));
        assert_eq!(
            t.completed, 1,
            "boundary armed retry_pending; the terminal counts the one turn once"
        );
    }

    /// THE NEVER-OVERCOUNT SWEEP — the binding property, asserted directly across the
    /// full matrix: for each scenario `completed <= real_turns` (never over), and the
    /// no-drop scenarios are EXACT. Documents the exact under/exact outcome per case.
    #[test]
    fn never_overcount_across_the_full_matrix() {
        // (name, real_turns, expected_completed, driver)
        type Drv = fn(&mut TurnTracker);
        let cases: &[(&str, u64, u64, Drv)] = &[
            ("clean 1", 1, 1, |t| {
                t.observe_event(&PiEvent::AgentStart);
                t.observe_event(&PiEvent::AgentEnd { will_retry: false });
            }),
            ("clean 3", 3, 3, |t| {
                for _ in 0..3 {
                    t.observe_event(&PiEvent::AgentStart);
                    t.observe_event(&PiEvent::AgentEnd { will_retry: false });
                }
            }),
            ("H dropped-boundary", 1, 1, |t| {
                t.observe_event(&PiEvent::AgentStart);
                t.observe_event(&PiEvent::AgentStart);
                t.observe_event(&PiEvent::AgentEnd { will_retry: false });
            }),
            ("A dropped-end back-to-back", 2, 1, |t| {
                t.observe_event(&PiEvent::AgentStart);
                t.observe_event(&PiEvent::AgentStart);
                t.observe_event(&PiEvent::AgentEnd { will_retry: false });
            }),
            ("A dropped-end WITH idle", 2, 2, |t| {
                t.observe_event(&PiEvent::AgentStart);
                t.observe(false);
                t.observe_event(&PiEvent::AgentStart);
                t.observe_event(&PiEvent::AgentEnd { will_retry: false });
            }),
            ("B dropped-start", 1, 1, |t| {
                t.observe_event(&PiEvent::AgentEnd { will_retry: false });
            }),
            ("C retry clean", 1, 1, |t| {
                t.observe_event(&PiEvent::AgentStart);
                t.observe_event(&PiEvent::AgentEnd { will_retry: true });
                t.observe(false);
                t.observe_event(&PiEvent::AgentStart);
                t.observe_event(&PiEvent::AgentEnd { will_retry: false });
            }),
            ("E1 compound", 2, 1, |t| {
                t.observe_event(&PiEvent::AgentStart);
                t.observe(false);
                t.observe_event(&PiEvent::AgentEnd { will_retry: false });
            }),
            ("E2 compound", 2, 1, |t| {
                t.observe_event(&PiEvent::AgentStart);
                t.observe_event(&PiEvent::AgentEnd { will_retry: false });
            }),
            ("G delayed-terminal+reopen", 2, 2, |t| {
                t.observe_event(&PiEvent::AgentStart);
                t.observe(false);
                t.observe(true);
                t.observe_event(&PiEvent::AgentEnd { will_retry: false });
                t.observe_event(&PiEvent::AgentStart);
                t.observe_event(&PiEvent::AgentEnd { will_retry: false });
            }),
        ];
        for (name, real, want, drv) in cases {
            let mut t = TurnTracker::default();
            drv(&mut t);
            assert!(
                t.completed <= *real,
                "{name}: OVERCOUNT {} > {real} real turns",
                t.completed
            );
            assert_eq!(
                t.completed, *want,
                "{name}: expected {want}, got {}",
                t.completed
            );
        }
    }

    /// Long mixed accumulation (12 turns): clean, dropped-start(B), dropped-end-with-idle,
    /// pr-close+delayed-terminal(G), retry-turn — accumulates without overcount and never
    /// exceeds the real count. (Each block adds exactly one COUNTED turn here.)
    #[test]
    fn twelve_turn_mixed_accumulation_never_overcounts() {
        let mut t = TurnTracker::default();
        let start = |t: &mut TurnTracker| {
            t.observe_event(&PiEvent::AgentStart);
        };
        let end = |t: &mut TurnTracker| {
            t.observe_event(&PiEvent::AgentEnd { will_retry: false });
        };
        for i in 0..12u64 {
            match i % 4 {
                0 => {
                    start(&mut t);
                    end(&mut t);
                } // clean
                1 => {
                    end(&mut t);
                } // B: dropped start
                2 => {
                    start(&mut t);
                    t.observe(false);
                } // pr-close (dropped end, idle observed)
                _ => {
                    start(&mut t);
                    t.observe_event(&PiEvent::AgentEnd { will_retry: true });
                    start(&mut t);
                    end(&mut t);
                } // retry-turn
            }
            assert!(
                t.completed <= i + 1,
                "step {i}: overcount {} > {}",
                t.completed,
                i + 1
            );
        }
        assert_eq!(
            t.completed, 12,
            "12 counted turns accumulate exactly, no overcount"
        );
    }
}

// ===========================================================================
// B2 — sustained-load conformance test (Component 2 of the quality-loop).
//
// TARGET under test: the pi resident's reader-thread event buffer stays bounded
// for the life of the process — it does not grow without limit while a turn is
// producing sustained output, whether or not a client is connected.
//
// WHAT IS EXERCISED. The buffer-bound mechanism is the resident drain:
// `pump_events` (this file) → `PiStdio::drain_events(EVENT_DRAIN_MAX_FRAMES, …)`
// (stdio.rs), a NON-BLOCKING, per-pass-bounded consume of the reader-thread mpsc
// channel + the `pending` VecDeque. These tests drive event *production* through
// a synthetic pi-shaped `/bin/sh` child (the `spawn_shell_fixture` pattern from
// stdio.rs — cred-free, no real pi binary, no `QD_PI_LIVE` gate) and assert the
// OBSERVED buffer state at source (frames actually consumed / stranded), never a
// return string, in the house style of `conformance.rs` / `chaos.rs`.
//
// DEFECT DISCRIMINATION (the stop-condition's teeth). Every clause is proven
// twice: once with the SHIPPED drain, once with a faithful in-process
// reproduction of a defect, so each named assertion is shown *capable of catching
// the defect* rather than passing vacuously:
//   * `under_drain_pass` reproduces the PRE-FIX bug exactly — the old
//     `pump_events` body `while let Ok(Some(_)) = pi.next_event(Duration::ZERO) {}`.
//     `next_event(ZERO)` pops only `pending` then returns `Ok(None)` before any
//     channel `recv` is attempted, so it consumes NOTHING from the channel. This
//     is the buffer-grows-for-the-whole-turn defect.
//   * `unbounded_drain_pass` reproduces the NAIVE-FIX risk the plan names — a
//     drain-to-empty pass with no per-pass cap, which does work proportional to
//     the whole backlog and would starve accept()/shutdown.
// The shipped drain (`pump_events` / `drain_events(EVENT_DRAIN_MAX_FRAMES, …)`)
// beats both.
// ===========================================================================
#[cfg(test)]
mod sustained_load {
    // The buffer-bound mechanism, imported as the ACTUAL shipped symbols/values
    // (not re-declared): `pump_events` is the resident pump this test drives, and
    // the two consts are the exact bounds the resident ships with.
    use super::{pump_events, TurnTracker, EVENT_DRAIN_MAX_FRAMES, POLL_INTERVAL};
    use crate::provider::pi::stdio::rpc::PiRpc; // brings `next_event` into scope on &PiStdio
    // `CHANNEL_CAPACITY` is crate-visible but not re-exported at `pi::stdio` —
    // it is consumed only here and inside `stdio.rs` itself, so it keeps the
    // deep path rather than earning a compatibility line in `stdio/mod.rs`.
    use crate::provider::pi::stdio::stdio::CHANNEL_CAPACITY;
    use crate::provider::pi::stdio::PiStdio;
    use std::cell::RefCell;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// An inert turn-tracker for the B2 buffer-bound assertions: `pump_events`
    /// now folds each drained event into a tracker (B1), but these tests observe
    /// only the CHANNEL drain (residual / consumed / peak-per-pass), which the
    /// fold cannot change. A throwaway tracker keeps the drive on the REAL shipped
    /// `pump_events` symbol without coupling to B1's counting semantics.
    fn inert_tracker() -> RefCell<TurnTracker> {
        RefCell::new(TurnTracker::default())
    }

    /// The per-pass frame cap the resident ships with (1024), aliased for report
    /// readability. This is the SHIPPED value, not a copy.
    const CAP: usize = EVENT_DRAIN_MAX_FRAMES;

    /// Spawn a synthetic pi-shaped emitter: `/bin/sh -c <script>` writing pi-wire
    /// JSONL to stdout, exactly the `spawn_shell_fixture` pattern from stdio.rs.
    /// Scripts MUST end with `cat >/dev/null` so the child stays alive (stdout
    /// stays open ⇒ no EOF/`Closed` frame ⇒ `drain_events` returns `Ok(count)`),
    /// modelling a resident whose pi child is mid-turn. Dropped/closed at test end.
    fn spawn_emitter(script: &str) -> PiStdio {
        PiStdio::spawn(
            "/bin/sh",
            &["-c".to_string(), script.to_string()],
            Path::new("/tmp"),
            &[],
        )
        .expect("spawn synthetic pi-shaped emitter")
    }

    /// A shell fragment emitting `n` bare `agent_start` events (→ `PiEvent::AgentStart`)
    /// as fast as the shell can, no inter-event pause.
    fn emit_burst(n: usize) -> String {
        format!(
            "i=0\nwhile [ \"$i\" -lt {n} ]; do printf '{{\"type\":\"agent_start\"}}\\n'; i=$((i+1)); done\n"
        )
    }

    /// A script emitting `windows` bursts of `chunk` `agent_start` events each,
    /// separated by `sleep <secs>` — CONTINUOUS, sustained production at roughly
    /// `chunk`/window over a full simulated turn (not a one-shot burst). Ends with
    /// `cat >/dev/null` so the child stays alive. Total emitted = chunk*windows.
    fn emit_sustained_script(chunk: usize, windows: usize, sleep_secs: &str) -> String {
        let inner = format!(
            "j=0\nwhile [ \"$j\" -lt {chunk} ]; do printf '{{\"type\":\"agent_start\"}}\\n'; j=$((j+1)); done\n"
        );
        format!("r=0\nwhile [ \"$r\" -lt {windows} ]; do {inner}sleep {sleep_secs}; r=$((r+1)); done\ncat >/dev/null\n")
    }

    /// The shipped steady-state consumption ceiling, events/second:
    /// EVENT_DRAIN_MAX_FRAMES per POLL_INTERVAL. This is the rate above which a
    /// bounded-per-pass drain called once per POLL_INTERVAL cannot keep up.
    fn ceiling_per_sec() -> f64 {
        CAP as f64 / POLL_INTERVAL.as_secs_f64()
    }

    /// THE SHIPPED DRAIN, instrumented to count. Byte-for-byte the call
    /// `pump_events` makes — `pi.drain_events(EVENT_DRAIN_MAX_FRAMES, …)` — the
    /// only difference being the retained callback counts instead of discarding
    /// (the callback IS the resident's status/republish seam; counting is an
    /// observation, not a behavior change). Returns frames consumed this pass.
    fn fixed_pass(pi: &PiStdio) -> usize {
        let mut n = 0usize;
        let _ = pi.drain_events(EVENT_DRAIN_MAX_FRAMES, |_ev| n += 1);
        n
    }

    /// PRE-FIX BUG reproduction: the old `pump_events` body. `next_event(ZERO)`
    /// returns `Ok(None)` before any channel recv, so this consumes 0 channel
    /// frames — the buffer-grows-unbounded defect. Returns frames consumed.
    fn under_drain_pass(pi: &PiStdio) -> usize {
        let mut n = 0usize;
        while let Ok(Some(_ev)) = pi.next_event(Duration::ZERO) {
            n += 1;
        }
        n
    }

    /// NAIVE-FIX risk reproduction: a per-pass-UNBOUNDED consuming drain (cap =
    /// usize::MAX). Consumes the entire backlog in one pass — work scales with the
    /// buffer, the starvation the shipped per-pass cap prevents. Returns frames.
    fn unbounded_drain_pass(pi: &PiStdio) -> usize {
        let mut n = 0usize;
        let _ = pi.drain_events(usize::MAX, |_ev| n += 1);
        n
    }

    /// Repeatedly run `fixed_pass` (no wait) until a pass consumes 0, recording the
    /// per-pass counts. The whole static backlog, drained by the shipped bounded
    /// pump. Returns (per_pass_counts, total).
    fn drain_to_zero_recording(pi: &PiStdio) -> (Vec<usize>, usize) {
        let mut passes = Vec::new();
        let mut total = 0usize;
        loop {
            let d = fixed_pass(pi);
            if d == 0 {
                break;
            }
            total += d;
            passes.push(d);
            if passes.len() > 100_000 {
                break; // runaway backstop; never hit in practice
            }
        }
        (passes, total)
    }

    /// Generous settle so the reader thread has delivered every emitted line into
    /// the channel before a deterministic measurement. `n` lines at the reader's
    /// tight read_line+serde rate (~10^5–10^6 lines/s) take << this; the margin
    /// absorbs a loaded box.
    fn settle_for(n: usize) {
        let ms = 400 + (n as u64) / 40; // 400ms base + ~1ms per 40 lines
        std::thread::sleep(Duration::from_millis(ms));
    }

    /// One named-assertion outcome for the evidence report.
    struct Clause {
        name: &'static str,
        clause: &'static str,
        pass: bool,
        detail: String,
    }

    /// THE FULL MATRIX. One clean run exercises all four standard clauses; each is
    /// backed by a named assertion demonstrated to FAIL under a reproduced defect
    /// and PASS under the shipped drain. Run with:
    ///   ~/cap-cargo.sh test -p quorum-dispatch --lib \
    ///     provider::pi::daemon::residence::sustained_load -- --nocapture --test-threads=1
    #[test]
    fn sustained_load_buffer_bound_conformance() {
        let mut report: Vec<Clause> = Vec::new();
        let mut log = String::new();
        log.push_str(&format!(
            "B2 SUSTAINED-LOAD CONFORMANCE — shipped bounds: EVENT_DRAIN_MAX_FRAMES(CAP)={CAP}, POLL_INTERVAL={POLL_INTERVAL:?}\n\n"
        ));

        // -------------------------------------------------------------------
        // A0 (preamble) — the SHIPPED `pump_events` symbol actually consumes the
        // channel; the pre-fix drain strands it. Defect-catching sanity that ties
        // every downstream `fixed_pass` proxy to the real resident pump.
        // -------------------------------------------------------------------
        {
            let k = 300usize; // < CAP: one pass clears it entirely
            let pi = spawn_emitter(&format!("{}cat >/dev/null\n", emit_burst(k)));
            settle_for(k);
            // REAL shipped pump (drains + folds into an inert tracker), then
            // observe residual at source.
            pump_events(&pi, &inert_tracker());
            let residual = fixed_pass(&pi);
            let _ = pi.close();

            let pi_bug = spawn_emitter(&format!("{}cat >/dev/null\n", emit_burst(k)));
            settle_for(k);
            let under = under_drain_pass(&pi_bug); // pre-fix: consumes 0
            let recovered = drain_to_zero_recording(&pi_bug).1; // stranded events, recovered
            let _ = pi_bug.close();

            let pass = residual == 0 && under == 0 && recovered == k;
            let detail = format!(
                "shipped pump_events() drained {k} channel events → residual={residual} (want 0). \
                 PRE-FIX under_drain consumed under={under} (want 0) and stranded recovered={recovered} (want {k}).",
            );
            log.push_str(&format!(
                "[A0 shipped-pump-consumes] {}\n  {detail}\n\n",
                if pass { "PASS" } else { "FAIL" }
            ));
            report.push(Clause {
                name: "A0_shipped_pump_consumes_channel",
                clause: "(preamble)",
                pass,
                detail,
            });
        }

        // -------------------------------------------------------------------
        // A1 — SUSTAINED OUTPUT, at the REAL shipped cadence and WITHIN the
        // consumption envelope. A full simulated turn (≥10 pump passes at the real
        // POLL_INTERVAL=150ms) of CONTINUOUS production held BELOW the steady-state
        // ceiling (CAP/POLL_INTERVAL ≈ 6827/s). The shipped bounded pump keeps pace
        // over time: the buffer converges to 0 and never exceeds one pass's cap.
        // (Revised per B2 red-team R1: the old A1 pumped every 50ms — a cadence that
        // does not exist in the shipped code and out-drains real production. A1 now
        // pumps at the true POLL_INTERVAL. The ABOVE-ceiling case — where this same
        // real cadence does NOT keep pace — is characterized separately in the
        // `sustained_above_ceiling_growth` test below.)
        // -------------------------------------------------------------------
        {
            // ~0.5× ceiling: chunk per window well under CAP, so the 150ms pump
            // clears each window with headroom. 24 windows ⇒ a full multi-pass turn.
            let chunk = 256usize;
            let windows = 24usize;
            let n_total = chunk * windows; // 6144
            let pi = spawn_emitter(&emit_sustained_script(chunk, windows, "0.06"));

            // Faithful idle-serve drain at the REAL POLL_INTERVAL.
            let mut total = 0usize;
            let mut peak = 0usize;
            let mut passes = 0usize;
            let mut zero_streak = 0usize;
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(6) {
                std::thread::sleep(POLL_INTERVAL); // the REAL shipped cadence
                let d = fixed_pass(&pi);
                total += d;
                peak = peak.max(d);
                passes += 1;
                if d == 0 {
                    zero_streak += 1;
                } else {
                    zero_streak = 0;
                }
                // Production ends ~1.7s in; require a settled tail before stopping.
                if start.elapsed() > Duration::from_millis(2000) && zero_streak >= 2 {
                    break;
                }
            }
            let residual = drain_to_zero_recording(&pi).1;
            let _ = pi.close();

            // Bug contrast (deterministic settle): the pump consumes nothing, the
            // whole turn's output is stranded in the channel.
            let pi_bug = spawn_emitter(&format!("{}cat >/dev/null\n", emit_burst(n_total)));
            settle_for(n_total);
            let under = under_drain_pass(&pi_bug);
            let stranded = drain_to_zero_recording(&pi_bug).1;
            let _ = pi_bug.close();

            let expected_stranded = n_total.min(CHANNEL_CAPACITY);
            let real_cadence_turn = passes >= 10; // a genuine multi-pass turn, not a burst
            let pass = total == n_total
                && residual == 0
                && peak <= CAP // never more than one pass's worth of work per pass
                && real_cadence_turn
                && under == 0
                && stranded == expected_stranded;
            let detail = format!(
                "SUB-ceiling turn ({chunk}×{windows}={n_total} events, ~{:.0}/s < ceiling≈{:.0}/s) drained at the REAL \
                 POLL_INTERVAL={POLL_INTERVAL:?} over {passes} passes (≥10 ⇒ a real turn): consumed total={total} (want {n_total}), \
                 residual_after={residual} (want 0), peak_per_pass={peak} (≤CAP={CAP} — pump kept pace with headroom). \
                 PRE-FIX: pump consumed under={under} (want 0); stranded={stranded} \
                 (want min(total, CHANNEL_CAPACITY)={expected_stranded} after raw-channel drop-on-full).",
                n_total as f64 / start.elapsed().as_secs_f64(),
                ceiling_per_sec(),
            );
            log.push_str(&format!(
                "[A1 sustained-output] {}\n  {detail}\n\n",
                if pass { "PASS" } else { "FAIL" }
            ));
            report.push(Clause {
                name: "A1_sustained_output_stays_bounded",
                clause: "Sustained output",
                pass,
                detail,
            });
        }

        // -------------------------------------------------------------------
        // A2 — NO CLIENT CONNECTED (idle-serve path). Mirrors serve_pi's loop:
        //   loop { if shutdown break; pump_events(pi); accept()->WouldBlock -> sleep(POLL) }
        // No ws client is attached; the ONLY buffer-relevant call is pump_events,
        // invoked once per POLL_INTERVAL, exactly as in serve_pi (residence.rs).
        // -------------------------------------------------------------------
        {
            let m = 3 * CAP + 137; // > CAP: needs several passes
            let pi = spawn_emitter(&format!("{}cat >/dev/null\n", emit_burst(m)));
            // Idle-serve mirror: pump on the POLL_INTERVAL cadence (the accept()
            // WouldBlock sleep). Non-buffer work (accept) cannot touch the buffer.
            let mut total = 0usize;
            let mut peak = 0usize;
            let mut zero_streak = 0usize;
            for _ in 0..64 {
                let d = fixed_pass(&pi); // == pump_events body (serve_pi idle drain)
                total += d;
                peak = peak.max(d);
                if d == 0 {
                    zero_streak += 1;
                } else {
                    zero_streak = 0;
                }
                if zero_streak >= 2 && total >= m {
                    break;
                }
                std::thread::sleep(POLL_INTERVAL); // accept() WouldBlock -> sleep(POLL_INTERVAL)
            }
            let residual = drain_to_zero_recording(&pi).1;
            let _ = pi.close();

            let pi_bug = spawn_emitter(&format!("{}cat >/dev/null\n", emit_burst(m)));
            settle_for(m);
            let under = under_drain_pass(&pi_bug);
            let stranded = drain_to_zero_recording(&pi_bug).1;
            let _ = pi_bug.close();

            let pass = total == m && residual == 0 && peak <= CAP && under == 0 && stranded == m;
            let detail = format!(
                "idle-serve cadence over {m} events: consumed total={total} (want {m}), residual={residual} (want 0), \
                 peak_per_pass={peak} (≤CAP={CAP}). PRE-FIX: under={under} (want 0), stranded={stranded} (want {m}).",
            );
            log.push_str(&format!(
                "[A2 no-client idle-serve] {}\n  {detail}\n\n",
                if pass { "PASS" } else { "FAIL" }
            ));
            report.push(Clause {
                name: "A2_bound_holds_idle_serve_no_client",
                clause: "No client connected",
                pass,
                detail,
            });
        }

        // -------------------------------------------------------------------
        // A3 — CLIENT CONNECTED BUT QUIET (connection-loop path). Mirrors
        // handle_connection's loop:
        //   loop { if shutdown break; pump_events(pi); ws.read()->TimedOut(POLL) -> continue }
        // A client is attached but issues NO front requests, so ws.read() times
        // out every POLL_INTERVAL and the only buffer-relevant call is the SAME
        // pump_events. (A quiet client issues no request(), so there is no RefCell
        // borrow to overlap — the single-borrow discipline is trivially kept.)
        // -------------------------------------------------------------------
        {
            let m = 3 * CAP + 251;
            let pi = spawn_emitter(&format!("{}cat >/dev/null\n", emit_burst(m)));
            settle_for(m);
            // Connection-loop mirror driven by the REAL shipped `pump_events`
            // symbol: pump once per iteration; ws.read() timing out after
            // POLL_INTERVAL is the per-iteration wait; a quiet client issues no
            // front request. Enough iterations to exceed ceil(m/CAP) passes.
            let iters = m / CAP + 4;
            for _ in 0..iters {
                pump_events(&pi, &inert_tracker()); // ← the actual connection-loop drain (residence.rs)
                std::thread::sleep(Duration::from_millis(1)); // ws.read() TimedOut -> continue
            }
            // The bound: the real pump_events fully consumed the channel iff a probe
            // now finds nothing (consumed == m - residual).
            let residual = drain_to_zero_recording(&pi).1;
            let _ = pi.close();

            let pi_bug = spawn_emitter(&format!("{}cat >/dev/null\n", emit_burst(m)));
            settle_for(m);
            let under = under_drain_pass(&pi_bug);
            let stranded = drain_to_zero_recording(&pi_bug).1;
            let _ = pi_bug.close();

            let pass = residual == 0 && under == 0 && stranded == m;
            let detail = format!(
                "connection-loop cadence over {m} events, driven by the REAL pump_events() per iteration \
                 ({iters} iterations): buffer fully consumed → residual={residual} (want 0 ⇒ consumed all {m}). \
                 PRE-FIX under_drain: consumed under={under} (want 0), stranded={stranded} (want {m}).",
            );
            log.push_str(&format!(
                "[A3 client-quiet conn-loop] {}\n  {detail}\n\n",
                if pass { "PASS" } else { "FAIL" }
            ));
            report.push(Clause {
                name: "A3_bound_holds_client_connected_quiet",
                clause: "Client connected but quiet",
                pass,
                detail,
            });
        }

        // -------------------------------------------------------------------
        // A4 — RESPONSIVENESS PRESERVED. Under a large channel backlog (flood),
        // each shipped pump pass (a) consumes AT MOST CAP frames and (b) completes
        // in << POLL_INTERVAL wall-time, so the serve loop returns to its
        // accept()/shutdown check within poll granularity. The NAIVE unbounded
        // drain instead consumes the ENTIRE backlog in one pass (≫ CAP), scaling
        // pass cost with the buffer — the starvation the per-pass cap prevents.
        // -------------------------------------------------------------------
        {
            let m = 25 * CAP; // 25600: a large flood; raw channel keeps backlog bounded.
            let pi = spawn_emitter(&format!("{}cat >/dev/null\n", emit_burst(m)));
            settle_for(m);

            // Time each shipped bounded pass while the backlog is present.
            let mut max_pass = Duration::ZERO;
            let mut worst_frames = 0usize;
            let mut consumed = 0usize;
            loop {
                let t = Instant::now();
                let d = fixed_pass(&pi);
                let dt = t.elapsed();
                if d == 0 {
                    break;
                }
                max_pass = max_pass.max(dt);
                worst_frames = worst_frames.max(d);
                consumed += d;
            }
            let _ = pi.close();

            // Contrast (deterministic): the naive unbounded drain does the whole
            // backlog in ONE pass.
            let pi2 = spawn_emitter(&format!("{}cat >/dev/null\n", emit_burst(m)));
            settle_for(m);
            let one_shot = unbounded_drain_pass(&pi2);
            let _ = pi2.close();

            // With a fully-settled backlog of 25×CAP, the first passes each consume
            // EXACTLY CAP — direct evidence that ≫CAP events were queued at one
            // instant (the "faster than any single drain pass would clear" case) and
            // that the pass is nonetheless capped at CAP.
            let per_pass_capped_exactly = worst_frames == CAP;
            let within_poll = max_pass < POLL_INTERVAL;
            let cap_is_loadbearing = one_shot > CAP; // naive drain unbounded per pass
            let pass = per_pass_capped_exactly
                && within_poll
                && cap_is_loadbearing
                && consumed == CHANNEL_CAPACITY;
            let detail = format!(
                "flood {m} emitted (=25×CAP): raw reader channel bounded backlog at CHANNEL_CAPACITY={CHANNEL_CAPACITY}; \
                 shipped bounded pump — worst_frames_per_pass={worst_frames} (want ==CAP={CAP}), \
                 slowest_pass={max_pass:?} (< POLL_INTERVAL={POLL_INTERVAL:?}), total_consumed={consumed} (want CHANNEL_CAPACITY). \
                 NAIVE unbounded drain consumed {one_shot} in a SINGLE pass (≫CAP → would starve accept/shutdown if the channel cap were larger).",
            );
            log.push_str(&format!(
                "[A4 responsiveness] {}\n  {detail}\n\n",
                if pass { "PASS" } else { "FAIL" }
            ));
            report.push(Clause {
                name: "A4_responsiveness_preserved_under_load",
                clause: "Responsiveness preserved",
                pass,
                detail,
            });
        }

        // ---- Evidence report (primary output the oracle reads) --------------
        log.push_str("==== CLAUSE → NAMED ASSERTION MAP ====\n");
        for c in &report {
            log.push_str(&format!(
                "  [{}] {:<42} :: {}\n        {}\n",
                if c.pass { "PASS" } else { "FAIL" },
                c.name,
                c.clause,
                c.detail,
            ));
        }
        let all_pass = report.iter().all(|c| c.pass);
        let matrix_covered = {
            let clauses: std::collections::HashSet<&str> =
                report.iter().map(|c| c.clause).collect();
            clauses.contains("Sustained output")
                && clauses.contains("No client connected")
                && clauses.contains("Client connected but quiet")
                && clauses.contains("Responsiveness preserved")
        };
        log.push_str(&format!(
            "\nMATRIX COVERED (all 4 clauses present): {matrix_covered}\nALL ASSERTIONS PASS: {all_pass}\n"
        ));
        // Primary evidence to stdout (visible under --nocapture) — the oracle rules
        // on THIS output, not on a claim of success.
        println!("\n{log}");

        assert!(
            matrix_covered,
            "not all four standard clauses were exercised"
        );
        assert!(
            all_pass,
            "a named clause assertion failed — see the evidence report above"
        );
    }

    // =======================================================================
    // A5 — SUSTAINED ABOVE-CEILING production at the REAL cadence. The
    // loop-adequacy strengthening B2 red-team R1 asked for.
    //
    // The shipped drain consumes at most CAP frames per POLL_INTERVAL pass →
    // a steady-state ceiling of CAP/POLL_INTERVAL ≈ 6827 events/s. This test
    // drives production CONTINUOUSLY ABOVE that ceiling for a full turn (many
    // real-cadence passes, not a one-shot burst) and MEASURES what happens to the
    // raw reader channel. The required defense-in-depth cap means the residual
    // should stay within CHANNEL_CAPACITY instead of growing with total excess
    // production; overflow frames are intentionally dropped by the reader thread.
    // =======================================================================
    #[test]
    fn sustained_above_ceiling_stays_within_channel_cap() {
        let ceiling = ceiling_per_sec();
        // ~2× ceiling per window: 2×CAP events emitted, brief sleep, repeated for a
        // full turn. On any box whose shell printf exceeds ~7k/s (all of them), the
        // delivered rate stays comfortably above the ceiling.
        let chunk = 2 * CAP; // 2048 per window
        let windows = 18usize;
        let n_total = chunk * windows; // 36864
        let pi = spawn_emitter(&emit_sustained_script(chunk, windows, "0.05"));

        // PHASE 1 — faithful serve loop: the shipped bounded pump at the REAL
        // POLL_INTERVAL. Run through a sustained saturated window, then measure
        // the residual before extra pump passes can drain the capped backlog away.
        let mut per_pass: Vec<usize> = Vec::new();
        let mut consumed_p1 = 0usize;
        let start = Instant::now();
        let phase1 = Duration::from_millis(1500);
        while start.elapsed() < phase1 {
            std::thread::sleep(POLL_INTERVAL); // the REAL shipped cadence — NOT accelerated
            let d = fixed_pass(&pi); // the REAL shipped bounded-pump body
            consumed_p1 += d;
            per_pass.push(d);
        }
        let elapsed_p1 = start.elapsed();

        // PHASE 2 — drain what the bounded pump left behind. With a bounded raw
        // channel this residual is the queued backlog after drop-on-full shedding,
        // not the total excess production.
        let (drain_passes, residual) = drain_to_zero_recording(&pi);
        let _ = pi.close();

        let total_delivered = consumed_p1 + residual;
        // Passes pinned at exactly CAP == the pump was throughput-limited (saturated)
        // that pass: ≥ CAP was always waiting ⇒ the buffer never emptied.
        let saturated_passes = per_pass.iter().filter(|&&c| c == CAP).count();
        let consumption_rate = consumed_p1 as f64 / elapsed_p1.as_secs_f64();
        let delivered_rate = total_delivered as f64 / elapsed_p1.as_secs_f64();
        let attempted_rate = n_total as f64 / elapsed_p1.as_secs_f64();

        // METHOD VALIDITY (asserted — else the characterization is inconclusive, and
        // an inconclusive run must fail honestly, never pass silently):
        //   (1) production genuinely exceeded the ceiling, and
        //   (2) it was sustained across a real multi-pass turn (not a burst).
        let above_ceiling = attempted_rate > ceiling;
        // ≥8 real-cadence passes each pinned at CAP = ~1.2s+ of continuous
        // above-ceiling load — unambiguously sustained, not a 2–3 pass burst.
        let sustained_turn = saturated_passes >= 8;

        // MEASURED OUTCOME: the raw channel must be fixed and bounded even when
        // production is sustained above the resident drain's throughput ceiling.
        let buffer_bounded = residual <= CHANNEL_CAPACITY;
        let verdict = if buffer_bounded {
            "BOUNDED (residual stayed within CHANNEL_CAPACITY under sustained above-ceiling load; overflow was dropped)"
        } else {
            "GREW PAST CHANNEL_CAPACITY (raw channel is not enforcing the required bound)"
        };

        let mut log = String::new();
        log.push_str(
            "A5 — SUSTAINED ABOVE-CEILING RAW-CHANNEL CAP CHARACTERIZATION (real cadence)\n",
        );
        log.push_str(&format!(
            "  shipped ceiling = CAP/POLL_INTERVAL = {CAP}/{POLL_INTERVAL:?} ≈ {ceiling:.0} events/s\n"
        ));
        log.push_str(&format!(
            "  production      = {chunk}×{windows} = {n_total} events, sustained above ceiling\n"
        ));
        log.push_str(&format!(
            "  pump cadence    = REAL POLL_INTERVAL = {POLL_INTERVAL:?} (NOT accelerated)\n"
        ));
        log.push_str(&format!(
            "  raw channel cap = CHANNEL_CAPACITY = {CHANNEL_CAPACITY} frames (drop-on-full)\n"
        ));
        log.push_str(&format!(
            "  phase-1 passes  = {} (saturated@CAP = {saturated_passes})  elapsed = {elapsed_p1:?}\n",
            per_pass.len()
        ));
        log.push_str(&format!("  per-pass drained (phase 1) = {per_pass:?}\n"));
        log.push_str(&format!(
            "  attempted_rate  = {attempted_rate:.0}/s   accepted_rate = {delivered_rate:.0}/s   consumption_rate = {consumption_rate:.0}/s\n"
        ));
        log.push_str(&format!(
            "  residual after above-ceiling turn, drained in {} passes = {residual} frames (= {:.1}× CHANNEL_CAPACITY)\n",
            drain_passes.len(),
            residual as f64 / CHANNEL_CAPACITY as f64,
        ));
        log.push_str(&format!("  total delivered = {total_delivered}\n"));
        log.push_str(&format!(
            "\n  METHOD VALID: above_ceiling={above_ceiling} (attempted {attempted_rate:.0}/s > ceiling {ceiling:.0}/s), \
             sustained_turn={sustained_turn} ({saturated_passes} saturated passes ≥ 8)\n"
        ));
        log.push_str(&format!("  MEASURED OUTCOME: {verdict}\n"));
        log.push_str(
            "\n  DEFENSE-IN-DEPTH: the resident drain still has a finite steady-state\n\
             \x20 consumption ceiling, but the reader-thread mpsc is now itself bounded.\n\
             \x20 Under sustained above-ceiling production, frames beyond CHANNEL_CAPACITY\n\
             \x20 are shed with try_send rather than accumulating without limit.\n",
        );
        println!("\n{log}");

        // Assert method validity and the new raw-channel bound.
        assert!(
            above_ceiling,
            "inconclusive: could not sustain above-ceiling production on this box \
             (attempted {attempted_rate:.0}/s ≤ ceiling {ceiling:.0}/s) — see report"
        );
        assert!(
            sustained_turn,
            "inconclusive: above-ceiling load was not sustained across a real multi-pass turn \
             (only {saturated_passes} saturated passes) — see report"
        );
        assert!(
            buffer_bounded,
            "raw channel residual exceeded CHANNEL_CAPACITY={CHANNEL_CAPACITY}: residual={residual}"
        );
    }

    // =======================================================================
    // B1 FORCED-DROP end-to-end (clause 3), through the REAL bounded channel.
    //
    // Not "realistic load" (which drops ~0%): a deterministic full-channel
    // OVERFLOW forces the trailing `agent_end` into the dropped tail, so the
    // resident drain NEVER sees the turn close. This ties the drop MECHANISM
    // (B2's `try_send` drop-on-full) to B1's reconciliation:
    //   (a) after draining every DELIVERED frame the event path alone is stuck
    //       "busy" with the completion uncounted — i.e. the drop really happened
    //       (frame_counts reports dropped>0), proving the point-read is
    //       load-bearing, not decorative; and
    //   (b) a single `is_streaming` point-read reading FALSE (the backstop, or
    //       ANY reader `get_state`) reconciles: NOT stuck-busy, counted once.
    // =======================================================================
    #[test]
    fn forced_channel_drop_of_agent_end_is_reconciled_by_a_point_read() {
        // A realistic single turn: one opening `agent_start` (delivered), a flood of
        // benign `message_update` DELTAS (the assistant streaming — status-irrelevant
        // to the counter, so they fill the channel without being mis-read as turns),
        // then the terminal `agent_end` — which free-falls into the dropped tail.
        // (The filler is deltas, NOT `agent_start`: deltas are what pi really streams
        // mid-turn — status-irrelevant, so they can't skew the count either way.)
        // `cat >/dev/null` keeps stdout open.
        let flood = CHANNEL_CAPACITY + 2000;
        let script = format!(
            "printf '{{\"type\":\"agent_start\"}}\\n'\n\
             i=0\n while [ \"$i\" -lt {flood} ]; do printf '{{\"type\":\"message_update\",\"assistantMessageEvent\":{{\"type\":\"text_delta\",\"delta\":\"x\"}}}}\\n'; i=$((i+1)); done\n\
             printf '{{\"type\":\"agent_end\",\"willRetry\":false}}\\n'\n\
             cat >/dev/null\n"
        );
        let pi = spawn_emitter(&script);
        // Let the whole burst land (fill + drops) before draining.
        std::thread::sleep(Duration::from_millis(600));

        let tracker = RefCell::new(TurnTracker::default());
        // Drain every DELIVERED frame into the tracker (several bounded passes).
        for _ in 0..((CHANNEL_CAPACITY / CAP) + 4) {
            pump_events(&pi, &tracker);
        }

        let (_received, dropped) = pi.frame_counts();
        assert!(
            dropped > 0,
            "the flood must have dropped frames (including the terminal agent_end)"
        );
        {
            let t = tracker.borrow();
            assert!(
                t.streaming,
                "event path alone: the dropped agent_end left the belief (wrongly) busy"
            );
            assert_eq!(
                t.completed, 0,
                "event path alone: the completion is uncounted"
            );
        }

        // The drop-immune backstop: an is_streaming point-read reads FALSE.
        let counted = tracker.borrow_mut().observe(false);
        assert!(counted, "the busy→idle edge fires on the point-read");
        let t = tracker.borrow();
        assert!(
            !t.streaming,
            "reconciled: NOT stuck-busy after a dropped agent_end"
        );
        assert_eq!(
            t.completed, 1,
            "reconciled: counted exactly once, not missed"
        );

        let _ = pi.close();
    }
}
