use std::path::{Path, PathBuf};
use tokio::net::UnixStream;

/// How to spawn the daemon process (C1 M4fix).
///
/// The standalone qrmux CLI re-execs `current_exe()` with `["server"]` (its own
/// binary HAS a `server` subcommand). But an EMBEDDING binary (the `sb` engine)
/// is the daemon too — it links qrmux — yet its daemon entry is a DIFFERENT
/// subcommand (`sb qrmux-server`), and `current_exe()` for it is the `sb` binary,
/// which has NO bare `server` verb. Re-execing `sb server …` failed in production
/// ("too many arguments") so the embedded daemon never started (Lima delta
/// a6-embedded-backend-DELTA.txt). The launch spec lets the embedder say exactly
/// which program + arg-prefix re-creates ITS daemon, decoupling the launcher from
/// the assumption "my binary has a bare `server` verb".
///
/// The spawned argv is `program` + `args_prefix` + `["--socket-dir", <dir>]`
/// (the `--socket-dir` pair only when a dir override is given).
#[derive(Debug, Clone)]
pub struct ServerLaunchSpec {
    /// The daemon program to exec (e.g. the `sb` binary's `current_exe()`).
    pub program: PathBuf,
    /// Args BEFORE `--socket-dir` (e.g. `["qrmux-server"]` for the sb embedder,
    /// or `["server"]` to mirror the standalone default explicitly).
    pub args_prefix: Vec<String>,
}

impl ServerLaunchSpec {
    /// The standalone-CLI default: `current_exe() server` (the qrmux binary's own
    /// `server` subcommand). Used when no explicit spec is supplied. The launcher
    /// appends `--socket-dir <dir> --session <name>` so it cold-starts a
    /// per-session daemon (WS-C M3b — the legacy no-`--session` bind is retired).
    fn default_standalone() -> anyhow::Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            args_prefix: vec!["server".to_string()],
        })
    }
}

// WS-C M3a/M3b: per-session launcher (spec §4.2). The ONLY launcher now — the
// legacy shared-daemon `ensure_server_running[_with]` (which bound `qrmux.sock`)
// is RETIRED (M3b, spec §1/§9). Liveness is the §4.2 FOUR-state model keyed on a
// preamble+Hello handshake probe (NOT socket-file existence, pb1 Q3a):
//   - Up        — handshake completes and the daemon answers a session-addressed
//                 probe verb (live OR claim-window). The launcher is done.
//   - Retiring  — handshake completes but a session-addressed verb returns the
//                 named ERR_SESSION_ENDED (§4.1 step 4): present, mid-teardown.
//                 Brief backoff → re-probe; after the daemon's unlink-before-exit
//                 the path yields Absent → relaunch. NEVER unlink an answering
//                 daemon's socket (unlink eligibility is ECONNREFUSED-only).
//   - Crashed   — connect ECONNREFUSED against an EXISTING socket: dead daemon,
//                 stale socket. Unlink-under-the-flock + respawn (today's
//                 launcher would burn the full poll budget here instead).
//   - Absent    — connect ENOENT (no socket): clean cold-start path.
// ===========================================================================

use crate::protocol::{self, ClientMsg, FrameReader, ServerMsg};
use crate::server::client_handler::ERR_SESSION_ENDED;
use crate::server::socket::{session_lock_path_for, session_socket_path_for};
use tokio::io::AsyncWriteExt;

/// Base readiness/poll budget per daemon (~5s), env-overridable via
/// `QRMUX_LAUNCH_BUDGET_MS` because N-way concurrent cold-start on a loaded box
/// makes a fixed 50×100ms tight (the G3 slow-runner lesson class, §4.2).
const LAUNCH_BUDGET_BASE: std::time::Duration = std::time::Duration::from_millis(5_000);

fn launch_budget_from_env() -> std::time::Duration {
    std::env::var("QRMUX_LAUNCH_BUDGET_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(LAUNCH_BUDGET_BASE)
}

/// Item-16 death-confirmation backoffs for the LAUNCHER probe: 3 probes total
/// (1 + 2 retries) over ~350ms before a refusing socket is believed dead.
/// Mirrors `discovery::DEAD_CONFIRM_BACKOFF_MS` (kept module-local so the two
/// probe layers stay decoupled) — a live daemon under a full accept backlog
/// answers again once its queue drains (well under these backoffs); a genuinely
/// dead socket refuses all three. Cost of a TRUE stale socket: ~350ms slower
/// cold-start — read-path latency, never a correctness risk.
const CRASH_CONFIRM_BACKOFF_MS: [u64; 2] = [100, 250];

/// Jittered poll interval. Base 100ms ± up to 50ms so racing same-session
/// waiters (lock-blocked) and readiness pollers don't thunder in lockstep
/// (pb1 Q3b). Cheap, allocation-free PRNG seeded from the clock + a per-call
/// nonce — this is jitter, not crypto.
fn jittered_interval() -> std::time::Duration {
    use std::time::{SystemTime, UNIX_EPOCH};
    static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // xorshift-ish mix; we only need the low bits for a 0..=50ms jitter.
    let mut x = now ^ (n.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    let jitter_ms = x % 51; // 0..=50
    std::time::Duration::from_millis(100 + jitter_ms)
}

/// The §4.2 four-state liveness classification of a per-session socket.
#[derive(Debug, PartialEq)]
enum Liveness {
    /// Handshake completed and the daemon answered a session-addressed probe
    /// (live session OR claim-window). Ready to accept create/connect.
    Up,
    /// Handshake completed but a session-addressed verb returned the named
    /// session-ended refusal: present and mid-teardown (§4.1 step 4).
    Retiring,
    /// connect() ECONNREFUSED against an existing socket: dead daemon, stale
    /// socket. Eligible for unlink-under-the-flock.
    Crashed,
    /// connect() ENOENT: no socket. Clean cold-start path.
    Absent,
}

/// Probe `<dir>/<name>.sock`: connect + preamble + Hello + a read-only
/// session-addressed verb, classifying into the four §4.2 states.
///
/// The session-addressed probe is `GetHistory{name}` (read-only — no attach, no
/// eviction, no side effects): on a LIVE session it returns `History`; on a
/// CLAIM-WINDOW daemon (no session yet) it returns a plain "not found" Error
/// (still [`Liveness::Up`] — the daemon is present and the launcher's create
/// will claim it); on a RETIRING daemon it returns the named
/// [`ERR_SESSION_ENDED`]. A ServerHello identity mismatch is treated as Up-ish
/// (a daemon IS answering on this leaf) but is surfaced as an error so a
/// swapped/mis-bound socket fails loudly rather than silently relaunching.
async fn probe_liveness(path: &std::path::Path, name: &str) -> anyhow::Result<Liveness> {
    let mut stream = match UnixStream::connect(path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            return Ok(Liveness::Crashed)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Liveness::Absent),
        Err(e) => return Err(e.into()),
    };
    protocol::write_preamble(&mut stream).await?;
    // Hello-first, then read the ServerHello (the daemon's FIRST reply frame).
    let hello = protocol::encode(&ClientMsg::Hello { caps: vec![] })?;
    stream.write_all(&hello).await?;
    let mut frames = FrameReader::new();
    loop {
        if !frames.fill_from(&mut stream).await? {
            // Closed before ServerHello: treat as not-up. A bound-but-broken
            // daemon is rare; classify Crashed so the flock path re-probes and
            // unlinks if the socket is genuinely stale.
            return Ok(Liveness::Crashed);
        }
        match frames.decode_next::<ServerMsg>()? {
            Some(ServerMsg::Hello { session, .. }) => {
                // G-ISOL negative-control seam (spec §7): under `QRMUX_TEST_SHARED=1`
                // every name probes the ONE `shared.sock` daemon (launched as some
                // first-mover name), so the launcher identity belt is relaxed — the
                // launcher must read the shared daemon as Up for any name. Inert in
                // production (env unset).
                if session != name && !crate::server::socket::shared_fate_test_mode() {
                    anyhow::bail!(
                        "qrmux daemon at {} identifies as session '{}', expected '{}'",
                        path.display(),
                        session,
                        name
                    );
                }
                break;
            }
            Some(ServerMsg::Error(e)) => anyhow::bail!("{}", e),
            Some(other) => anyhow::bail!(
                "expected server Hello, got {:?}",
                std::mem::discriminant(&other)
            ),
            None => continue,
        }
    }
    // Session-addressed probe to distinguish Up from Retiring (the ServerHello
    // alone succeeds even mid-teardown — the session-ended refusal only fires on
    // a session-addressed verb, §4.1 step 4).
    let probe = protocol::encode(&ClientMsg::GetHistory {
        name: name.to_string(),
    })?;
    stream.write_all(&probe).await?;
    loop {
        if !frames.fill_from(&mut stream).await? {
            return Ok(Liveness::Crashed);
        }
        match frames.decode_next::<ServerMsg>()? {
            Some(ServerMsg::History(_)) => return Ok(Liveness::Up),
            Some(ServerMsg::Error(e)) if e == ERR_SESSION_ENDED => return Ok(Liveness::Retiring),
            // Any other Error (e.g. "session not found" in the claim window) =
            // present and ready to be claimed.
            Some(ServerMsg::Error(_)) => return Ok(Liveness::Up),
            Some(other) => anyhow::bail!(
                "unexpected probe response: {:?}",
                std::mem::discriminant(&other)
            ),
            None => continue,
        }
    }
}

/// Item-16 death-confirmation for the LAUNCHER path — the wrong-victim guard.
///
/// [`probe_liveness`] classifies a SINGLE `connect()` `ECONNREFUSED` as
/// [`Liveness::Crashed`]. But one refusal is NOT death: a LIVE per-session
/// daemon whose listen backlog is momentarily full (N-way concurrent cold-start
/// on a loaded box — the ECONNREFUSED-under-load flake) refuses a connect, and
/// the launcher's response to `Crashed` is DESTRUCTIVE — under the per-session
/// flock it UNLINKS the socket ([`ensure_session_server_running`] step 3) and
/// spawns a capacity-1 replacement that then cannot bind, exhausting the poll
/// budget (`c1_gate.rs:356` "failed to start daemon"; the `version_negotiation`
/// raw-os-111 victim is the same mechanism one layer down at the bare connect).
///
/// So a `Crashed` reading is re-probed with backoff and escalates as `Crashed`
/// ONLY when EVERY probe refuses — positive death evidence, mirroring
/// [`crate::client::discovery`]'s `probe_with_death_confirmation` (punch item
/// 16). Any probe that completes the handshake ([`Liveness::Up`] / `Retiring`)
/// or finds the socket gone ([`Liveness::Absent`]) returns immediately: the
/// daemon was never dead.
///
/// This NEVER fabricates an alive reading from a dead daemon. `Up`/`Retiring`
/// require a real handshake a dead daemon cannot answer; `Absent` requires the
/// socket file to be gone. A genuine death still refuses all three probes and is
/// honestly classified `Crashed` (cold-start proceeds, ~350ms later) — honest
/// failure is delayed, never masked into a false-alive.
async fn probe_liveness_confirmed(path: &std::path::Path, name: &str) -> anyhow::Result<Liveness> {
    let mut outcome = probe_liveness(path, name).await?;
    for backoff_ms in CRASH_CONFIRM_BACKOFF_MS {
        if !matches!(outcome, Liveness::Crashed) {
            return Ok(outcome);
        }
        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
        outcome = probe_liveness(path, name).await?;
    }
    Ok(outcome)
}

/// Ensure a per-session daemon is running for `name` (spec §4.2). Idempotent:
/// a live daemon is a fast-path no-op; a crashed/absent daemon is (re)launched
/// under the per-session flock; a retiring daemon is awaited then relaunched.
///
/// `socket_dir` of `Some(dir)` derives `<dir>/<name>.sock` / `.lock` / `.log`
/// and propagates `--socket-dir <dir> --session <name>` to the spawned daemon
/// (the R26 crossing). `launch` selects the daemon program (the embedder's
/// `sb qrmux-server`); `None` is the standalone default.
pub async fn ensure_session_server_running(
    socket_dir: Option<&Path>,
    name: &str,
    launch: Option<&ServerLaunchSpec>,
) -> anyhow::Result<()> {
    crate::server::socket::validate_session_identity(name)?;
    let path = session_socket_path_for(socket_dir, name)?;
    let budget = launch_budget_from_env();

    // (1) Fast path: handshake probe (death-confirmed — a transient ECONNREFUSED
    // of a backlog-saturated LIVE daemon must read Up here, not fall through to a
    // needless lock+teardown).
    match probe_liveness_confirmed(&path, name).await? {
        Liveness::Up => return Ok(()),
        Liveness::Retiring => {
            // Present and mid-teardown: brief backoff, then re-probe. After the
            // daemon's unlink-before-exit this resolves to Absent → relaunch.
            // NEVER unlink here (unlink eligibility is ECONNREFUSED-only).
            wait_through_retiring(&path, name, budget).await?;
            // Fall through to the lock+spawn path below (the daemon is gone).
        }
        Liveness::Crashed | Liveness::Absent => {}
    }

    // (2) Acquire the per-session flock (LOCK_EX | LOCK_NB).
    let lock_path = session_lock_path_for(socket_dir, name)?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    let _lock_guard =
        match nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
            Ok(guard) => guard,
            Err((_, nix::errno::Errno::EWOULDBLOCK)) => {
                // Another invocation is launching THIS session — poll for it to
                // come up with jitter/backoff, bounded by the per-daemon budget.
                let deadline = tokio::time::Instant::now() + budget;
                while tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(jittered_interval()).await;
                    if matches!(probe_liveness(&path, name).await, Ok(Liveness::Up)) {
                        return Ok(());
                    }
                }
                anyhow::bail!(
                    "timed out waiting for another launcher to start session '{}'",
                    name
                );
            }
            Err((_, e)) => anyhow::bail!("failed to acquire session launch lock: {}", e),
        };

    // (3) Lock held: re-probe (lost-race double-check + crashed-state detect).
    // DEATH-CONFIRMED: this is the destructive site — a `Crashed` reading here
    // UNLINKS the socket below, so it must be CONSISTENT refusal, never a single
    // transient one against a live-but-backlogged daemon (wrong-victim unlink).
    match probe_liveness_confirmed(&path, name).await? {
        Liveness::Up => return Ok(()), // another launcher won the race
        Liveness::Retiring => {
            // Raced into teardown after we took the lock; await ENOENT, then spawn.
            wait_through_retiring(&path, name, budget).await?;
        }
        Liveness::Crashed => {
            // Dead daemon, stale socket: unlink UNDER THE LOCK (unlink ownership
            // (ii), §4.2), then spawn.
            if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    return Err(anyhow::anyhow!(
                        "failed to unlink stale socket {:?}: {}",
                        path,
                        e
                    ));
                }
            }
        }
        Liveness::Absent => {}
    }

    // Spawn `<program> <args_prefix...> --socket-dir <dir> --session <name>`
    // with the existing setsid/FD-hygiene/reap mechanics; stdio → per-session
    // `<name>.log` (spec §2).
    let spec = match launch {
        Some(s) => s.clone(),
        None => ServerLaunchSpec::default_standalone()?,
    };
    let log_path = path.with_extension("log");
    let log_file_stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    use std::os::unix::process::CommandExt;
    unsafe {
        let mut command = std::process::Command::new(&spec.program);
        command.args(&spec.args_prefix);
        if let Some(dir) = socket_dir {
            command.arg("--socket-dir").arg(dir);
        }
        // Per-session: tell the daemon WHICH session it serves (it binds
        // `<dir>/<name>.sock` and enforces capacity-1, §4.1).
        command.arg("--session").arg(name);
        let mut child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(log_file_stderr))
            .pre_exec(|| {
                if nix::libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let max_fd = match nix::libc::sysconf(nix::libc::_SC_OPEN_MAX) {
                    n if n > 0 => (n as i32).min(4096),
                    _ => 1024,
                };
                for fd in 3..max_fd {
                    nix::libc::fcntl(fd, nix::libc::F_SETFD, nix::libc::FD_CLOEXEC);
                }
                Ok(())
            })
            .spawn()?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }

    // (3 cont.) Readiness poll = connect+handshake with jitter/backoff, bounded
    // by the per-daemon budget. The flock is held ACROSS this spawn+poll so
    // same-session creators serialize (different sessions parallelize — different
    // lock files).
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(jittered_interval()).await;
        if matches!(probe_liveness(&path, name).await, Ok(Liveness::Up)) {
            // _lock_guard drops here, releasing the flock.
            return Ok(());
        }
    }

    // _lock_guard drops here, releasing the flock.
    anyhow::bail!("failed to start daemon for session '{}'", name);
}

/// Wait through the "retiring" state (§4.2): the daemon answered a handshake but
/// is mid-teardown. Poll with jitter/backoff until the probe yields `Absent`
/// (the daemon unlinked its socket and exited) or `Crashed` (it died leaving a
/// stale socket). Returns when the path is no longer a live/retiring daemon.
/// NEVER unlinks — unlink eligibility is ECONNREFUSED-only.
async fn wait_through_retiring(
    path: &std::path::Path,
    name: &str,
    budget: std::time::Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(jittered_interval()).await;
        // DEATH-CONFIRMED: a single transient refusal of a still-live retiring
        // daemon must not end the wait early — the caller's under-lock-Retiring
        // branch spawns WITHOUT unlinking, so a false `Crashed` here would bind-
        // race a live socket (capacity-1 bail). Only confirmed death exits.
        match probe_liveness_confirmed(path, name).await? {
            Liveness::Absent | Liveness::Crashed => return Ok(()),
            // Still retiring, or an Up reading (a fresh daemon already raced in —
            // let the caller's re-probe under the lock settle it): keep polling
            // until Absent/Crashed or the caller re-checks Up under the lock.
            Liveness::Retiring => continue,
            Liveness::Up => return Ok(()),
        }
    }
    anyhow::bail!(
        "timed out waiting for retiring daemon of session '{}' to exit",
        name
    );
}
