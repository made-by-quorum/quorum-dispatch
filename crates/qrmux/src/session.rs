use crate::pty::Pty;
use crate::screen::{Screen, TerminalEmulator, TerminalSize};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Default terminal dimensions when actual size is unavailable.
pub const DEFAULT_COLS: u16 = 80;
pub const DEFAULT_ROWS: u16 = 24;

/// RAII guard that clears `has_client` flag when dropped, unless evicted.
///
/// When a new client evicts the current one, it sends `false` on the eviction
/// channel. The guard checks this on Drop: if evicted, it skips clearing
/// has_client (the new client already set it to true).
pub struct ClientGuard {
    has_client: Arc<AtomicBool>,
    evict_rx: tokio::sync::watch::Receiver<bool>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        // evict_rx initial value is `true` (not evicted).
        // Eviction sends `false`.
        if *self.evict_rx.borrow() {
            self.has_client.store(false, Ordering::Release);
        }
    }
}

/// Session names are displayed in CLI output and stored as filesystem-friendly identifiers.
/// 128 bytes is generous for human-readable names while preventing abuse.
const MAX_SESSION_NAME_LEN: usize = 128;

/// PTY read buffer size. 4 KiB matches the typical pipe buffer granularity on
/// Linux/macOS and balances syscall overhead against latency.
const PTY_READ_BUF_SIZE: usize = 4096;

/// Maximum queued DA/DSR responses when pty_writer is contended. 64 is far
/// beyond normal usage (typically 1-2 responses in flight). Overflow means a
/// deadlock-like condition where the client holds pty_writer indefinitely.
const MAX_DEFERRED: usize = 64;

/// Shared screen handle.
pub type SharedScreen = Arc<Mutex<Screen>>;

/// Shared handles for the client relay tasks.
/// Created by `Session::connect()`.
#[derive(Clone)]
pub struct SessionHandles {
    pub screen: SharedScreen,
    pub pty_writer: crate::pty::SharedPtyWriter,
    pub master: crate::pty::SharedMasterPty,
    pub dims: Arc<Mutex<TerminalSize>>,
    pub screen_notify: Arc<tokio::sync::Notify>,
    pub reader_alive: Arc<AtomicBool>,
    pub name: String,
    /// attended-UX M1: the polite-delivery state, so the `client_to_pty` relay's
    /// Input arm can journal + lock human keystrokes. `None` ⇒ today's behavior
    /// (write straight to the PTY).
    pub attended: Option<Arc<crate::attended::driver::AttendedState>>,
}

/// A single terminal session backed by a PTY and a virtual screen.
pub struct Session {
    pub(crate) name: String,
    pub(crate) pty: Pty,
    pub(crate) screen: SharedScreen,
    pub(crate) dims: Arc<Mutex<TerminalSize>>,
    /// When a client is attached, holds the sender side of a watch channel.
    /// Sending `false` evicts the active client. Replaced on each new attach.
    evict_tx: Option<tokio::sync::watch::Sender<bool>>,
    /// Wakes the client relay when new PTY data has been processed.
    screen_notify: Arc<tokio::sync::Notify>,
    /// Whether a client is currently connected (used by reader to decide draining).
    has_client: Arc<AtomicBool>,
    /// Set to false when the persistent reader thread detects PTY EOF.
    reader_alive: Arc<AtomicBool>,
    /// Handle for the persistent PTY reader thread (joined on Drop).
    reader_handle: Option<std::thread::JoinHandle<()>>,
    /// Spawn time as Unix epoch seconds; `None` if the clock could not be read.
    /// The daemon knows this (it spawned the session), so it is the single
    /// source of truth for `SessionInfo::created` (C1 D1; protocol v2).
    created: Option<u64>,
    /// Per-session daemon event writer (ACK-1 advisory write-log). `None` when
    /// events are disabled (kill switch) or the writer failed to open (the
    /// session then runs unlogged — emission must never fail session ops).
    events: Option<Arc<crate::events::EventWriter>>,
    #[cfg(test)]
    logical_history_override: Option<crate::screen::LogicalTransportEmission>,
    /// attended-UX M1: the polite-delivery state (journal / lock / spool) + the
    /// per-session timer task. `None` when there is no runtime dir (events
    /// disabled) — the session then behaves exactly as today (no journal/lock/
    /// countdown). The timer task inside is **aborted in `Session::drop`**.
    attended: Option<Arc<crate::attended::driver::AttendedState>>,
}

impl Session {
    /// Create a new session, spawning a process in a PTY of the given size.
    ///
    /// `command`/`cwd` of `None`/`None` preserves today's exact behavior (the
    /// default login shell inheriting the daemon's cwd) — the `Connect`/attach
    /// path. `Some` is used by the v2 `CreateDetached` path. See [`Pty::spawn`].
    pub fn new(
        name: String,
        cols: u16,
        rows: u16,
        history: usize,
        command: Option<crate::pty::CommandSpec>,
        cwd: Option<std::path::PathBuf>,
        events_ctx: Option<&crate::events::EventsCtx>,
    ) -> anyhow::Result<Self> {
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs());
        // M4/M5(T5): capture the harness identity from the spawned command BEFORE
        // `command` is moved into the PTY spawn. `None` (attach-to-shell / Connect)
        // ⇒ Harness::Default. `Some` uses `from_command`, which parses the launched
        // binary out of the `["bash","-lc", command '<bin>' …]` login-shell shape
        // CreateDetached uses for every provider (argv0 is always `bash`, so
        // `from_argv0` alone could never reach Codex/Pi — the M4 reachability gap).
        let harness = command
            .as_ref()
            .map(|c| crate::attended::driver::Harness::from_command(&c.argv))
            .unwrap_or(crate::attended::driver::Harness::Default);
        let pty = Pty::spawn(cols, rows, command, cwd)?;

        // ACK-1 event writer: pid captured ONCE here, right after spawn —
        // the child lock is uncontended at this point (the reader thread
        // doesn't exist yet), so this is never the try_lock-at-emit flake
        // path. Writer failure → session runs unlogged (warn), never an
        // error: the advisory stream must not fail session creation.
        let child_pid = pty
            .child_arc()
            .lock()
            .ok()
            .and_then(|c| c.process_id())
            .unwrap_or(0);
        // W2 / SPEC-v2 §5.A: the agent-pid identity token. The load-bearing
        // invariant (N6c): the token is computed from the SAME `child_pid` that
        // is recorded on `session-opened` (the agent/child — §1/M5), NEVER
        // `std::process::id()` (the daemon). Fail-safe `None` on read failure;
        // `pid==0` (no child pid) yields `None` silently.
        let pid_start_ms = crate::procid::pid_start_ms(child_pid);
        let boot_id = crate::procid::boot_id();
        let events = events_ctx.and_then(|ctx| {
            match crate::events::EventWriter::open(ctx, &name, child_pid, pid_start_ms, boot_id) {
                Ok(w) => Some(Arc::new(w)),
                Err(e) => {
                    tracing::warn!(session = %name, error = %e,
                        "events writer unavailable — session runs unlogged");
                    None
                }
            }
        });
        let screen = Arc::new(Mutex::new(Screen::new(cols, rows, history)));
        let dims = Arc::new(Mutex::new(TerminalSize { cols, rows }));
        let screen_notify = Arc::new(tokio::sync::Notify::new());
        let has_client = Arc::new(AtomicBool::new(false));
        let reader_alive = Arc::new(AtomicBool::new(true));

        // Spawn the persistent PTY reader thread.
        let pty_reader = pty.clone_reader()?;
        let pty_writer = pty.writer_arc();
        let reader_handle = {
            let screen = screen.clone();
            let notify = screen_notify.clone();
            let has_client = has_client.clone();
            let reader_alive = reader_alive.clone();
            let thread_name = format!("pty-reader-{}", name);
            std::thread::Builder::new()
                .name(thread_name)
                .spawn(move || {
                    persistent_reader_loop(
                        pty_reader,
                        screen,
                        pty_writer,
                        notify,
                        has_client,
                        reader_alive,
                    );
                })?
        };

        // attended-UX M1: create the polite-delivery state + spawn the per-session
        // timer task. The spool lives under the runtime dir (`<socket_dir>/pending/
        // <name>/`); the socket_dir is `events_ctx.dir`'s parent (dir =
        // `<socket_dir>/events`). No events_ctx (events disabled / tests) ⇒ no
        // attended state ⇒ today's exact behavior. The timer task only spawns when
        // a tokio runtime is present (the server path), so sync tests are unaffected.
        let attended = events_ctx
            .and_then(|ctx| ctx.dir.parent().map(|sd| sd.to_path_buf()))
            .and_then(|socket_dir| {
                let spool_dir = socket_dir.join("pending").join(&name);
                match crate::attended::driver::AttendedState::new(
                    name.clone(),
                    spool_dir,
                    pty.writer_arc(),
                    screen.clone(),
                    harness,
                ) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        tracing::warn!(session = %name, error = %e,
                            "attended state unavailable — session runs without polite delivery");
                        None
                    }
                }
            });

        Ok(Self {
            name,
            pty,
            screen,
            dims,
            evict_tx: None,
            screen_notify,
            has_client,
            reader_alive,
            reader_handle: Some(reader_handle),
            created,
            events,
            #[cfg(test)]
            logical_history_override: None,
            attended,
        })
    }

    /// The session's event writer handle (ACK-1), if events are enabled.
    /// Cloned alongside `pty_writer_arc` at the SendInput site and used by
    /// the lifecycle close sites.
    pub fn events_writer(&self) -> Option<Arc<crate::events::EventWriter>> {
        self.events.clone()
    }

    /// The attended-UX polite-delivery state (M1), if present. Cloned out under
    /// the manager lock then operated on after release (like `pty_writer_arc`):
    /// the client_handler `PendingDelivery`/`DeliverNow` arms drive it.
    pub fn attended_arc(&self) -> Option<Arc<crate::attended::driver::AttendedState>> {
        self.attended.clone()
    }

    /// Spawn time as Unix epoch seconds (`None` if the clock could not be read).
    pub fn created(&self) -> Option<u64> {
        self.created
    }

    /// Check if the session is still alive.
    /// A session is dead if the reader thread exited (PTY EOF or panic)
    /// or if the child process has terminated.
    pub fn is_alive(&self) -> bool {
        self.reader_alive.load(Ordering::Acquire) && self.pty.is_child_alive()
    }

    /// Get the child process PID (if available).
    /// Uses `try_lock()` to avoid blocking; returns `None` on contention or poison.
    pub fn child_pid(&self) -> Option<u32> {
        self.pty
            .child_arc()
            .try_lock()
            .ok()
            .and_then(|c| c.process_id())
    }

    /// Shared PTY writer handle, for out-of-band `send` (one-shot input)
    /// without going through the attach/connect path.
    pub fn pty_writer_arc(&self) -> crate::pty::SharedPtyWriter {
        self.pty.writer_arc()
    }

    /// Snapshot the session's content for the v2 one-shot `GetHistory` path (no
    /// attach, no eviction, no screen subscription): scrollback lines FOLLOWED
    /// BY the current visible-screen rows, in order, trailing-blank visible rows
    /// trimmed. This is a CONTENT-INSPECTION view — the boot answerer
    /// content-matches dialog text that lives on the VISIBLE screen and usually
    /// never scrolls out, so visible rows must be included.
    ///
    /// Unlike attach-replay, this DOES include the visible rows even when an
    /// alt-screen app (vim/htop/a fullscreen dialog) is up — so the answerer can
    /// still see the dialog. See [`crate::screen::Screen::get_content_history`]
    /// and PROTOCOL.md "GetHistory composition".
    pub fn history_lines(&self) -> anyhow::Result<Vec<Vec<u8>>> {
        let screen = self
            .screen
            .lock()
            .map_err(|e| anyhow::anyhow!("screen mutex poisoned: {}", e))?;
        Ok(screen.get_content_history())
    }

    /// Snapshot cell-exact logical history for a capability-negotiated wire
    /// consumer. This is read-only and uses the same screen lock as the legacy
    /// history snapshot.
    pub fn logical_history(
        &self,
        surface: crate::screen::LogicalEmissionSurface,
    ) -> anyhow::Result<crate::screen::LogicalTransportEmission> {
        #[cfg(test)]
        if let Some(emission) = &self.logical_history_override {
            return Ok(emission.clone());
        }
        let screen = self
            .screen
            .lock()
            .map_err(|e| anyhow::anyhow!("screen mutex poisoned: {}", e))?;
        Ok(screen.logical_emission(surface))
    }

    /// Test-only transport fixture used to exercise protocol/client boundaries
    /// whose byte scale would be prohibitively slow to synthesize through the
    /// terminal parser. Production builds have no override field or branch.
    #[cfg(test)]
    pub(crate) fn set_logical_history_override(
        &mut self,
        emission: crate::screen::LogicalTransportEmission,
    ) {
        self.logical_history_override = Some(emission);
    }

    /// Mark a client as connected, evicting any previous client.
    ///
    /// Returns a `ClientGuard` (clears `has_client` on drop), shared handles
    /// for the relay tasks, and an eviction watch receiver.
    ///
    /// **Must be called under the SessionManager lock** to prevent races.
    pub fn connect(
        &mut self,
    ) -> (
        ClientGuard,
        SessionHandles,
        tokio::sync::watch::Receiver<bool>,
    ) {
        // Set has_client BEFORE evicting old client, so the persistent reader
        // doesn't discard data intended for the new client.
        self.has_client.store(true, Ordering::Release);

        // Evict previous client if any
        if let Some(old_tx) = self.evict_tx.take() {
            tracing::debug!(session = %self.name, "evicting previous client");
            if old_tx.send(false).is_err() {
                tracing::debug!(session = %self.name, "evict channel: previous client already disconnected");
            }
        }

        // Create new eviction channel for this client
        let (evict_tx, evict_rx) = tokio::sync::watch::channel(true);
        self.evict_tx = Some(evict_tx);

        let guard = ClientGuard {
            has_client: self.has_client.clone(),
            evict_rx: evict_rx.clone(),
        };

        let handles = SessionHandles {
            screen: self.screen.clone(),
            pty_writer: self.pty.writer_arc(),
            master: self.pty.master_arc(),
            dims: self.dims.clone(),
            screen_notify: self.screen_notify.clone(),
            reader_alive: self.reader_alive.clone(),
            name: self.name.clone(),
            attended: self.attended.clone(),
        };

        (guard, handles, evict_rx)
    }

    /// Disconnect the current client (used by KillSession).
    /// Drops evict_tx so the connected client sees RecvError.
    pub fn disconnect(&mut self) {
        drop(self.evict_tx.take());
    }

    /// Test accessor: whether a client is connected.
    #[cfg(test)]
    pub(crate) fn has_client(&self) -> bool {
        self.has_client.load(Ordering::Acquire)
    }

    /// Test accessor: whether the reader thread is alive.
    #[cfg(test)]
    pub(crate) fn reader_alive(&self) -> bool {
        self.reader_alive.load(Ordering::Acquire)
    }
}

/// B1 negative-control hook (gate teeth). When the server process env has
/// `RETACH_B1_BREAK=drop1000`, every 1000th PTY output byte is dropped BEFORE it
/// reaches the screen/scrollback model, using a deterministic per-session counter.
/// When the env is unset, `NegControl::from_env` returns `None` and the hook is
/// entirely inert (the `Option` is checked once per read in the loop). This is
/// spike-grade fork code; the hook is obviously dead when the env is unset.
struct NegControl {
    /// Drop every Nth byte (1-based running count, per session).
    every: u64,
    count: u64,
}

impl NegControl {
    fn from_env() -> Option<Self> {
        match std::env::var("RETACH_B1_BREAK").ok().as_deref() {
            Some("drop1000") => Some(NegControl {
                every: 1000,
                count: 0,
            }),
            _ => None,
        }
    }

    /// Filter `buf[..n]` in place, dropping every `every`th byte; returns the new
    /// length. Deterministic across calls via the running per-session counter.
    fn filter(&mut self, buf: &mut [u8], n: usize) -> usize {
        let mut w = 0;
        for r in 0..n {
            self.count += 1;
            if self.count.is_multiple_of(self.every) {
                continue; // drop this byte
            }
            buf[w] = buf[r];
            w += 1;
        }
        w
    }
}

/// Env-gated retention hook for the G5 leak-guard NEGATIVE CONTROL (orc
/// rider R1 on the 10MB-floor recalibration, 2026-06-04): when the server env
/// has `QRMUX_TEST_LEAK=retain`, every PTY chunk is ALSO copied into a buffer
/// that is never freed — genuine per-byte retention. The recalibrated leak
/// guard (10% relative AND >10MB absolute) must FIRE against this; if it
/// passes, the guard has no teeth and its recalibration doesn't count.
/// Inert (None) when the env is unset, same pattern as `NegControl`.
struct LeakControl {
    /// Flat retained buffer (never freed, never shrunk).
    retained: Vec<u8>,
    /// xorshift64 state for incompressible fill.
    rng: u64,
    /// Calls since the last keep-resident sweep.
    calls: u64,
    /// Fold of the sweep reads (keeps the sweep from being optimized out).
    checksum: u64,
}

impl LeakControl {
    fn from_env() -> Option<Self> {
        match std::env::var("QRMUX_TEST_LEAK").ok().as_deref() {
            Some("retain") => Some(LeakControl {
                retained: Vec::new(),
                rng: 0x9e37_79b9_7f4a_7c15,
                calls: 0,
                checksum: 0,
            }),
            _ => None,
        }
    }

    /// Retain `buf.len()` bytes of RESIDENT, INCOMPRESSIBLE growth.
    ///
    /// Two macOS pitfalls shaped this (negctl bring-up evidence):
    /// 1. Retaining the literal chunk fails — test payloads are repetitive,
    ///    the memory compressor swallows them, and RSS visibly DROPS mid-leak
    ///    (observed 36.8→20.6 MB). Fill is xorshift bytes: incompressible.
    /// 2. IDLE retained pages get paged out under suite-wide memory pressure
    ///    (observed negative Q4−Q2 with 61MB retained) — `ps rss` is the
    ///    spec's oracle and only counts RESIDENT pages, so the injected leak
    ///    must stay hot: periodic full XOR sweep re-touches every page.
    fn retain(&mut self, buf: &[u8]) {
        for _ in 0..buf.len().div_ceil(8) {
            // xorshift64
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            self.retained.extend_from_slice(&self.rng.to_le_bytes());
        }
        self.calls += 1;
        if self.calls.is_multiple_of(64) {
            // Keep-resident sweep: read one u64 per 512B across the buffer.
            let mut acc = 0u64;
            let mut i = 0;
            while i + 8 <= self.retained.len() {
                acc ^= u64::from_le_bytes(self.retained[i..i + 8].try_into().unwrap());
                i += 512;
            }
            self.checksum ^= acc;
        }
    }
}

/// B1 G4 ingest instrumentation (spike-grade, env-gated). When the server env
/// var `RETACH_B1_COUNT` names a file path, the reader loop tallies bytes read
/// from the PTY master vs. bytes fed to `Screen::process` and rewrites the file
/// after each read (and on loop exit), so an external repro can read the totals
/// even if the session is killed rather than reaching PTY EOF. Inert when unset.
struct ByteCount {
    path: String,
    read_total: u64,
    process_total: u64,
    reads: u64,
    /// Newlines (\n) and carriage returns (\r) in the bytes fed to process — a
    /// proxy for how many line-feeds the screen model saw vs. expected.
    lf: u64,
    cr: u64,
}

impl ByteCount {
    fn from_env() -> Option<Self> {
        std::env::var("RETACH_B1_COUNT")
            .ok()
            .filter(|p| !p.is_empty())
            .map(|path| ByteCount {
                path,
                read_total: 0,
                process_total: 0,
                reads: 0,
                lf: 0,
                cr: 0,
            })
    }

    fn count_lines(&mut self, buf: &[u8]) {
        for &b in buf {
            if b == b'\n' {
                self.lf += 1;
            } else if b == b'\r' {
                self.cr += 1;
            }
        }
    }

    fn dump(&self) {
        let line = format!(
            "read_total={} process_total={} reads={} lf={} cr={}\n",
            self.read_total, self.process_total, self.reads, self.lf, self.cr
        );
        let _ = std::fs::write(&self.path, line);
    }
}

/// B3 R8 seam-hole probe hook (evidence-only, env-gated). When the server
/// process env has `QRMUX_TEST_SEAM_DELAY_MS=<site>:<millis>`, a sleep of
/// `<millis>` ms is injected at exactly ONE of the three suspected race seams
/// in the replay write path (named in `exec/b3-spec.md` R8):
///
/// Line cites are symbol-anchored (not bare line numbers) so they cannot drift
/// again as code moves (C1 M5 / carry C1b F1):
///   - site `a`: in `session_bridge::send_initial_state`, AFTER the
///     screen/history snapshot is taken (lock released) and BEFORE the
///     History/ScreenUpdate frames are written (the `SeamDelay::maybe_sleep_async
///     (.., b'a')` call). Widens the window in which the attach's view of the
///     screen can go stale vs. the live reader.
///   - site `b`: in `session_bridge::handle_session`, AFTER the ScreenUpdate
///     write and BEFORE the `setup.handles.screen_notify.notify_one()` +
///     `screen_to_client` spawn (the `maybe_sleep_async(.., b'b')` call just
///     above the `notify_one()`). Widens the window between the replay snapshot
///     and the first live render arming.
///   - site `c`: in `persistent_reader_loop` (below in this file), AFTER
///     `reader.read()` and BEFORE `Screen::process` (the synchronous
///     `SeamDelay::maybe_sleep(.., b'c')` call). Slows ingest so output produced
///     during a concurrent attach lands later relative to the replay snapshot.
///     This site runs on a dedicated OS thread (not the async runtime), so it
///     keeps the SYNC `maybe_sleep`; sites a/b are async and use
///     `maybe_sleep_async` (C1 M5 / carry C1b F4).
///
/// `<site>` is a single char in {`a`,`b`,`c`}; `<millis>` is a `u64`.
/// Unset env, an empty value, an unknown site, or an unparseable delay all
/// yield `None` from `from_env` — the hook is then entirely inert (every call
/// site checks the `Option` and the matching site before sleeping). This is the
/// same env-gated pattern as `NegControl`/`LeakControl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SeamDelay {
    pub(crate) site: u8,
    pub(crate) millis: u64,
}

impl SeamDelay {
    /// Read the hook config from `QRMUX_TEST_SEAM_DELAY_MS`. `None` (inert)
    /// unless the env names a valid `<a|b|c>:<u64>`.
    pub(crate) fn from_env() -> Option<Self> {
        Self::parse(std::env::var("QRMUX_TEST_SEAM_DELAY_MS").ok().as_deref()?)
    }

    /// Parse `<site>:<millis>` where site ∈ {a,b,c}. Pure (testable) — returns
    /// `None` for any malformed or out-of-domain spec so the hook stays inert.
    pub(crate) fn parse(spec: &str) -> Option<Self> {
        let (site_str, millis_str) = spec.split_once(':')?;
        let site = match site_str {
            "a" => b'a',
            "b" => b'b',
            "c" => b'c',
            _ => return None,
        };
        let millis: u64 = millis_str.parse().ok()?;
        Some(SeamDelay { site, millis })
    }

    /// Sleep `millis` ms iff this hook targets `site`. No-op otherwise.
    /// Call unconditionally at a seam; passing the seam's own letter selects it.
    ///
    /// SYNC variant — for the site `c` seam in `persistent_reader_loop`, which
    /// runs on a dedicated OS thread (`std::thread::Builder::spawn`), NOT the
    /// async runtime. Blocking that thread is fine; use `maybe_sleep_async` at
    /// the async seams (sites a/b) instead (C1 M5 / carry C1b F4).
    pub(crate) fn maybe_sleep(this: Option<Self>, site: u8) {
        if let Some(d) = this {
            if d.site == site {
                std::thread::sleep(std::time::Duration::from_millis(d.millis));
            }
        }
    }

    /// Async variant of `maybe_sleep` — for the site `a`/`b` seams in
    /// `session_bridge`, which fire inside async fns on the tokio runtime.
    /// Uses `tokio::time::sleep` so the delay yields the runtime worker instead
    /// of blocking it (a blocking `std::thread::sleep` there would stall other
    /// tasks on the same worker). The parser/reader stays sync (site c keeps
    /// `maybe_sleep`); only these async-site sleeps changed (C1 M5 / carry C1b
    /// F4). Inert exactly like `maybe_sleep` when `this` is `None` or the site
    /// does not match.
    pub(crate) async fn maybe_sleep_async(this: Option<Self>, site: u8) {
        if let Some(d) = this {
            if d.site == site {
                tokio::time::sleep(std::time::Duration::from_millis(d.millis)).await;
            }
        }
    }
}

/// Persistent PTY reader loop, runs for the entire session lifetime.
/// Reads PTY output, feeds it through the screen's VTE parser, and notifies
/// any connected client of new data.
fn persistent_reader_loop(
    mut reader: Box<dyn Read + Send>,
    screen: Arc<Mutex<Screen>>,
    pty_writer: Arc<Mutex<Box<dyn Write + Send>>>,
    notify: Arc<tokio::sync::Notify>,
    has_client: Arc<AtomicBool>,
    reader_alive: Arc<AtomicBool>,
) {
    let mut buf = [0u8; PTY_READ_BUF_SIZE];
    let mut deferred_responses: VecDeque<Vec<u8>> = VecDeque::new();
    // B1 negative control: None (inert) unless RETACH_B1_BREAK is set.
    let mut neg_control = NegControl::from_env();
    // Leak-guard negative control: None (inert) unless QRMUX_TEST_LEAK is set.
    let mut leak_control = LeakControl::from_env();
    // B1 G4 ingest instrumentation: when RETACH_B1_COUNT names a file, tally the
    // bytes read from the PTY master vs. bytes fed to Screen::process, and write
    // the totals on EOF. Inert (None) when the env is unset.
    let mut byte_count = ByteCount::from_env();
    // B3 R8 seam-hole probe: None (inert) unless QRMUX_TEST_SEAM_DELAY_MS is set.
    let seam_delay = SeamDelay::from_env();
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                tracing::debug!("persistent pty reader: EOF");
                break;
            }
            Ok(mut n) => {
                if let Some(bc) = byte_count.as_mut() {
                    bc.read_total += n as u64;
                    bc.reads += 1;
                }
                // Negative-control hook: drop bytes before the screen model sees
                // them. No-op when the env gate is unset (neg_control is None).
                if let Some(nc) = neg_control.as_mut() {
                    n = nc.filter(&mut buf, n);
                }
                // Leak-guard negative control: retain a copy of every chunk
                // forever. No-op when QRMUX_TEST_LEAK is unset.
                if let Some(lc) = leak_control.as_mut() {
                    lc.retain(&buf[..n]);
                }
                if let Some(bc) = byte_count.as_mut() {
                    bc.process_total += n as u64;
                    bc.count_lines(&buf[..n]);
                }
                // B3 R8 site (c): delay between read and process. Inert unless
                // QRMUX_TEST_SEAM_DELAY_MS=c:<ms>.
                SeamDelay::maybe_sleep(seam_delay, b'c');
                let mut responses = {
                    let mut scr = match screen.lock() {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(error = %e, "screen mutex poisoned in reader loop, terminating");
                            break;
                        }
                    };
                    scr.process(&buf[..n]);
                    let responses = scr.take_responses();
                    // When no client is connected, drain pending data to prevent
                    // unbounded growth. The data is already in the main scrollback.
                    if !has_client.load(Ordering::Acquire) {
                        let _ = scr.take_pending_scrollback();
                        let _ = scr.take_passthrough();
                    }
                    responses
                };

                // Prepend any deferred responses from previous iterations.
                if !deferred_responses.is_empty() {
                    let mut all: Vec<Vec<u8>> = deferred_responses.drain(..).collect();
                    all.append(&mut responses);
                    responses = all;
                }

                // Write PTY responses (DA, DSR replies) outside the screen lock.
                // Use try_lock to avoid deadlock: client_to_pty may hold
                // pty_writer during a blocking write_all while waiting for the
                // child to read — if the child is waiting for this DA response,
                // a blocking lock() here would deadlock.
                if !responses.is_empty() {
                    match pty_writer.try_lock() {
                        Ok(mut w) => {
                            for response in &responses {
                                if let Err(e) = w.write_all(response) {
                                    tracing::warn!(error = %e, "failed to write response to PTY in reader loop");
                                    break;
                                }
                            }
                            if let Err(e) = w.flush() {
                                tracing::warn!(error = %e, "failed to flush PTY writer in reader loop");
                            }
                        }
                        Err(_) => {
                            tracing::debug!(
                                "pty_writer contended, deferring {} DA/DSR response(s)",
                                responses.len()
                            );
                            for resp in responses {
                                if deferred_responses.len() >= MAX_DEFERRED {
                                    tracing::warn!(queue_len = MAX_DEFERRED, "deferred DA/DSR response queue full, dropping oldest response");
                                    deferred_responses.pop_front();
                                }
                                deferred_responses.push_back(resp);
                            }
                        }
                    }
                }

                if let Some(bc) = byte_count.as_ref() {
                    bc.dump();
                }
                notify.notify_one();
            }
            Err(e) => {
                tracing::debug!(error = %e, "persistent pty reader: read error");
                break;
            }
        }
    }
    if let Some(bc) = byte_count.as_ref() {
        bc.dump();
    }
    reader_alive.store(false, Ordering::Release);
    notify.notify_one(); // wake client to detect reader death
}

impl Drop for Session {
    fn drop(&mut self) {
        // attended-UX M1: abort the per-session timer task FIRST (crash-safe, no
        // leaked task). Explicit — a `SessionHandles` clone held by a relay task
        // keeps the `Arc<AttendedState>` alive, so we cannot rely on the Arc's own
        // Drop; `shutdown` takes the JoinHandle so it is idempotent.
        if let Some(att) = &self.attended {
            att.shutdown();
        }
        // ACK-1 close-bookend BELT (spec §2 C4): if no instrumented path
        // (kill/reap/shutdown) emitted session-closed, record reason
        // "dropped". No-op when the shared once-flag is already set — the
        // explicit sites emit first, so a "dropped" record in a file is
        // itself a finding (an uninstrumented removal path).
        if let Some(ev) = &self.events {
            ev.emit_closed(crate::events::CloseReason::Dropped);
        }
        // Use try_lock() to avoid blocking the async runtime if the child lock
        // is contended. In practice it's never contended during drop because
        // the persistent reader has already exited (or will exit after kill).
        match self.pty.child_arc().try_lock() {
            Ok(mut child) => {
                if let Err(e) = child.kill() {
                    tracing::debug!(error = %e, session = %self.name, "child already exited before kill");
                }
                if let Err(e) = child.wait() {
                    tracing::debug!(error = %e, session = %self.name, "child already reaped before wait");
                }
            }
            Err(_) => {
                tracing::warn!(session = %self.name, "child mutex contended during drop, skipping kill/wait — detaching reader thread");
                // Can't kill the child, so the PTY reader thread will block on read
                // forever. Detach it (take + drop the JoinHandle) instead of joining,
                // and skip eviction since the session is being abandoned.
                self.reader_handle.take();
                return;
            }
        }
        // Evict any connected client
        if let Some(tx) = self.evict_tx.take() {
            if tx.send(false).is_err() {
                tracing::debug!(session = %self.name, "evict channel: client already disconnected during drop");
            }
        }
        // Wait for the reader thread to exit (it will see EOF after child kill).
        if let Some(handle) = self.reader_handle.take() {
            if handle.join().is_err() {
                tracing::warn!(session = %self.name, "PTY reader thread panicked during join");
            }
        }
    }
}

/// Validate a session name: max 128 bytes, only `[a-zA-Z0-9_-.]`.
pub fn validate_session_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("session name cannot be empty");
    }
    if name.len() > MAX_SESSION_NAME_LEN {
        anyhow::bail!("session name too long (max {} bytes)", MAX_SESSION_NAME_LEN);
    }
    if let Some(ch) = name
        .chars()
        .find(|c| !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.'))
    {
        anyhow::bail!(
            "invalid character '{}' in session name (allowed: a-zA-Z0-9_-.)",
            ch
        );
    }
    Ok(())
}

/// Registry of named sessions with create, lookup, and cleanup operations.
pub struct SessionManager {
    sessions: HashMap<String, Session>,
    /// ACK-1 events context (dir + cap), threaded from `run_server`. `None`
    /// when events are disabled — every Session is then created unlogged.
    events_ctx: Option<crate::events::EventsCtx>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    /// Create an empty session manager (no event stream — tests/back-compat).
    pub fn new() -> Self {
        Self::with_events(None)
    }

    /// Create an empty session manager with the ACK-1 events context
    /// (`run_server` passes `EventsCtx::from_env(&socket_dir)`).
    pub fn with_events(events_ctx: Option<crate::events::EventsCtx>) -> Self {
        Self {
            sessions: HashMap::new(),
            events_ctx,
        }
    }

    /// Event-writer handles of all live sessions (heartbeat task).
    pub fn event_writers(&self) -> Vec<Arc<crate::events::EventWriter>> {
        self.sessions
            .values()
            .filter_map(|s| s.events_writer())
            .collect()
    }

    /// Create a new session with the given name, failing if it already exists.
    /// Zero dimensions are clamped to defaults.
    pub fn create(
        &mut self,
        name: String,
        cols: u16,
        rows: u16,
        history: usize,
    ) -> anyhow::Result<()> {
        validate_session_name(&name)?;
        if self.sessions.contains_key(&name) {
            anyhow::bail!("session '{}' already exists", name);
        }
        let c = if cols > 0 { cols } else { DEFAULT_COLS };
        let r = if rows > 0 { rows } else { DEFAULT_ROWS };
        // None/None: default shell, inherit daemon cwd (the Connect path).
        let session = Session::new(
            name.clone(),
            c,
            r,
            history,
            None,
            None,
            self.events_ctx.as_ref(),
        )?;
        self.sessions.insert(name, session);
        Ok(())
    }

    /// Create a detached session running `command` in `cwd`, failing if the
    /// name already exists. Powers the v2 `CreateDetached` message: the daemon
    /// spawns the session and acks WITHOUT attaching a client (C1 D1/R27).
    pub fn create_detached(
        &mut self,
        name: String,
        cols: u16,
        rows: u16,
        history: usize,
        command: crate::pty::CommandSpec,
        cwd: std::path::PathBuf,
    ) -> anyhow::Result<()> {
        validate_session_name(&name)?;
        if self.sessions.contains_key(&name) {
            anyhow::bail!("session '{}' already exists", name);
        }
        let c = if cols > 0 { cols } else { DEFAULT_COLS };
        let r = if rows > 0 { rows } else { DEFAULT_ROWS };
        let session = Session::new(
            name.clone(),
            c,
            r,
            history,
            Some(command),
            Some(cwd),
            self.events_ctx.as_ref(),
        )?;
        self.sessions.insert(name, session);
        Ok(())
    }

    /// Get existing session or create a new one.
    /// Returns (session, is_new).
    pub fn get_or_create(
        &mut self,
        name: &str,
        cols: u16,
        rows: u16,
        history: usize,
    ) -> anyhow::Result<(&mut Session, bool)> {
        validate_session_name(name)?;
        use std::collections::hash_map::Entry;
        match self.sessions.entry(name.to_string()) {
            Entry::Occupied(e) => {
                tracing::debug!(session = %name, "reattaching to existing session");
                Ok((e.into_mut(), false))
            }
            Entry::Vacant(e) => {
                let c = if cols > 0 { cols } else { DEFAULT_COLS };
                let r = if rows > 0 { rows } else { DEFAULT_ROWS };
                tracing::debug!(session = %name, cols = c, rows = r, "creating new session");
                // None/None: default shell, inherit daemon cwd (the Connect path).
                let session = Session::new(
                    name.to_string(),
                    c,
                    r,
                    history,
                    None,
                    None,
                    self.events_ctx.as_ref(),
                )?;
                Ok((e.insert(session), true))
            }
        }
    }

    /// Get an existing session by name (read-only).
    pub fn get(&self, name: &str) -> Option<&Session> {
        self.sessions.get(name)
    }

    /// Get an existing session by name (mutable).
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Session> {
        self.sessions.get_mut(name)
    }

    /// Remove and return a session by name, or `None` if not found.
    pub fn remove(&mut self, name: &str) -> Option<Session> {
        self.sessions.remove(name)
    }

    /// Return metadata for all active sessions.
    pub fn list(&self) -> Vec<crate::protocol::SessionInfo> {
        self.sessions.values().map(|s| {
            let dims = match s.dims.lock() {
                Ok(d) => *d,
                Err(e) => {
                    tracing::warn!(session = %s.name, error = %e, "dims mutex poisoned in list");
                    TerminalSize { cols: DEFAULT_COLS, rows: DEFAULT_ROWS }
                }
            };
            crate::protocol::SessionInfo {
                name: s.name.clone(),
                pid: s.child_pid().unwrap_or(0),
                cols: dims.cols,
                rows: dims.rows,
                created: s.created,
            }
        }).collect()
    }

    /// Remove and return all sessions (for graceful shutdown).
    pub fn drain_all(&mut self) -> Vec<Session> {
        self.sessions.drain().map(|(_, s)| s).collect()
    }

    /// Number of sessions currently in the map (live or not-yet-reaped).
    /// WS-C M2 (§4.1): the single-session lifecycle uses this to detect a CLAIM
    /// (0 → ≥1).
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// True when the manager holds no sessions.
    ///
    /// The single-session lifecycle's claim/end-watch both key on this.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// WS-C M2 (§4.1): true when every session in the map has ENDED — i.e. the
    /// map is empty (killed/reaped) OR all remaining sessions are no longer alive
    /// (child exited / reader hit EOF; the 30s cleanup sweep has not run yet).
    /// The single-session lifecycle treats this as "session ended" so exit-on-end
    /// does NOT wait on the slow reaper. The drain/events bookend ordering is
    /// preserved: the reader drains to EOF before `is_alive()` flips false, and
    /// the events close-bookend is emitted by the cleanup/kill paths — this check
    /// only TRIGGERS the daemon exit, it does not itself emit or drop anything.
    pub fn all_sessions_ended(&self) -> bool {
        self.sessions.values().all(|s| !s.is_alive())
    }

    /// Remove dead sessions and return them for cleanup outside the lock.
    pub fn take_dead_sessions(&mut self) -> Vec<Session> {
        let dead: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| !s.is_alive())
            .map(|(name, s)| {
                // Use try_lock to avoid blocking a Tokio worker thread
                // (this is called from an async cleanup task).
                let status = s
                    .pty
                    .child_arc()
                    .try_lock()
                    .ok()
                    .and_then(|mut c| c.try_wait().ok().flatten());
                tracing::info!(
                    session = %name,
                    exit_status = ?status,
                    "cleaning up dead session"
                );
                name.clone()
            })
            .collect();
        dead.into_iter()
            .filter_map(|name| self.sessions.remove(&name))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::Ordering;

    /// Helper: collect visible grid rows as trimmed strings.
    fn screen_lines(screen: &crate::screen::Screen) -> Vec<String> {
        use crate::screen::TerminalEmulator;
        screen
            .visible_rows()
            .map(|row| {
                let s: String = row.iter().map(|c| c.c).collect();
                s.trim_end().to_string()
            })
            .collect()
    }

    /// Helper: collect scrollback history as plain text (ANSI stripped).
    fn history_texts(screen: &crate::screen::Screen) -> Vec<String> {
        screen
            .get_history()
            .iter()
            .map(|b| {
                let s = String::from_utf8_lossy(b);
                let mut out = String::new();
                let mut in_esc = false;
                for ch in s.chars() {
                    if in_esc {
                        if ch.is_ascii_alphabetic() || ch == 'm' {
                            in_esc = false;
                        }
                        continue;
                    }
                    if ch == '\x1b' {
                        in_esc = true;
                        continue;
                    }
                    if ch >= ' ' {
                        out.push(ch);
                    }
                }
                out.trim_end().to_string()
            })
            .collect()
    }

    /// Poll the screen until a predicate is satisfied or timeout expires.
    fn wait_for_screen(
        screen: &Arc<Mutex<crate::screen::Screen>>,
        timeout: std::time::Duration,
        pred: impl Fn(&crate::screen::Screen) -> bool,
    ) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if let Ok(scr) = screen.lock() {
                if pred(&scr) {
                    return true;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn neg_control_drops_every_nth_byte() {
        // Use a small period to make the test cheap and deterministic.
        let mut nc = super::NegControl { every: 4, count: 0 };
        let mut buf: Vec<u8> = (0..10u8).collect();
        let n = nc.filter(&mut buf, 10);
        // 1-based counts 1..=10; drops at counts 4 and 8 -> bytes index 3 and 7.
        assert_eq!(n, 8);
        assert_eq!(&buf[..n], &[0, 1, 2, 4, 5, 6, 8, 9]);
    }

    #[test]
    fn neg_control_counter_persists_across_calls() {
        let mut nc = super::NegControl { every: 3, count: 0 };
        let mut a = vec![10, 11]; // counts 1,2 -> none dropped
        assert_eq!(nc.filter(&mut a, 2), 2);
        let mut b = vec![12, 13]; // counts 3,4 -> drop count 3 (byte 12)
        assert_eq!(nc.filter(&mut b, 2), 1);
        assert_eq!(b[0], 13);
    }

    #[test]
    fn byte_count_line_counter_accumulates() {
        let mut bc = super::ByteCount {
            path: String::new(),
            read_total: 0,
            process_total: 0,
            reads: 0,
            lf: 0,
            cr: 0,
        };
        bc.count_lines(b"ab\r\ncd\n");
        bc.count_lines(b"\r\re");
        assert_eq!(bc.lf, 2);
        assert_eq!(bc.cr, 3);
    }

    // ---- B3 R8 SeamDelay (env-gated seam-hole probe) ----

    #[test]
    fn seam_delay_parses_each_valid_site() {
        for (s, c) in [("a:50", b'a'), ("b:100", b'b'), ("c:20", b'c')] {
            let d = super::SeamDelay::parse(s).expect("valid spec parses");
            assert_eq!(d.site, c);
        }
        assert_eq!(super::SeamDelay::parse("a:0").unwrap().millis, 0);
        assert_eq!(super::SeamDelay::parse("b:100").unwrap().millis, 100);
        assert_eq!(super::SeamDelay::parse("c:20").unwrap().millis, 20);
    }

    #[test]
    fn seam_delay_rejects_malformed_specs() {
        // No separator, unknown site, multi-char site, non-numeric or negative
        // millis, empty halves, extra fields — all must yield None (inert).
        for bad in [
            "", "a", "50", "a50", "x:50", "ab:50", "A:50", "a:", ":50", "a:b", "a:-5", "a:5.0",
            "a:5:6", " a:50", "a: 50",
        ] {
            assert!(
                super::SeamDelay::parse(bad).is_none(),
                "spec {:?} must be rejected (hook stays inert)",
                bad
            );
        }
    }

    #[test]
    fn seam_delay_maybe_sleep_inert_when_none() {
        // The Option-is-None path is the unset-env path: it must NOT sleep.
        let t = std::time::Instant::now();
        super::SeamDelay::maybe_sleep(None, b'a');
        super::SeamDelay::maybe_sleep(None, b'b');
        super::SeamDelay::maybe_sleep(None, b'c');
        assert!(
            t.elapsed() < std::time::Duration::from_millis(20),
            "maybe_sleep(None, _) must be a no-op"
        );
    }

    #[test]
    fn seam_delay_maybe_sleep_only_fires_for_matching_site() {
        // A hook for site 'b' must NOT sleep when called at sites 'a'/'c'.
        let hook = Some(super::SeamDelay {
            site: b'b',
            millis: 200,
        });
        let t = std::time::Instant::now();
        super::SeamDelay::maybe_sleep(hook, b'a');
        super::SeamDelay::maybe_sleep(hook, b'c');
        assert!(
            t.elapsed() < std::time::Duration::from_millis(50),
            "non-matching sites must not sleep"
        );
        // And it MUST sleep at its own site.
        let t2 = std::time::Instant::now();
        super::SeamDelay::maybe_sleep(hook, b'b');
        assert!(
            t2.elapsed() >= std::time::Duration::from_millis(150),
            "matching site must sleep ~millis"
        );
    }

    /// Persistent reader processes PTY output while no client is connected.
    ///
    /// Simulates: client opens session running `sleep 2 && echo MARKER`,
    /// disconnects immediately, reconnects after the command completes,
    /// and finds MARKER in the screen or scrollback.
    #[test]
    fn persistent_reader_captures_output_without_client() {
        let session =
            Session::new("test-persistent".into(), 80, 24, 1000, None, None, None).unwrap();

        // No client connected — persistent reader is running with has_client=false.
        assert!(!session.has_client());
        assert!(session.reader_alive());

        // Write a command that produces output after a short delay.
        // Use a unique marker so we can find it unambiguously.
        {
            let writer = session.pty.writer_arc();
            let mut w = writer.lock().unwrap();
            w.write_all(b"sleep 1 && echo PERSISTENT_READER_OK\n")
                .unwrap();
            w.flush().unwrap();
        }

        // Wait for the marker to appear in the screen (up to 5s).
        let found = wait_for_screen(&session.screen, std::time::Duration::from_secs(5), |scr| {
            let lines = screen_lines(scr);
            let hist = history_texts(scr);
            lines
                .iter()
                .chain(hist.iter())
                .any(|l| l.contains("PERSISTENT_READER_OK"))
        });

        assert!(
            found,
            "persistent reader should capture PTY output even with no client connected"
        );

        // Reader should still be alive (shell is still running).
        assert!(session.reader_alive());
    }

    /// After the child process exits, a reconnecting client sees the final
    /// output and reader_alive is false.
    #[test]
    fn persistent_reader_detects_child_exit() {
        let session = Session::new("test-exit".into(), 80, 24, 1000, None, None, None).unwrap();

        // Tell the shell to print a marker and exit.
        {
            let writer = session.pty.writer_arc();
            let mut w = writer.lock().unwrap();
            w.write_all(b"echo GOODBYE && exit\n").unwrap();
            w.flush().unwrap();
        }

        // Wait for reader_alive to become false (child exited, PTY EOF).
        let exited = wait_for_screen(&session.screen, std::time::Duration::from_secs(5), |_| {
            !session.reader_alive()
        });
        assert!(exited, "reader_alive should become false after child exits");

        // The marker should be visible in the screen or scrollback.
        let scr = session.screen.lock().unwrap();
        let lines = screen_lines(&scr);
        let hist = history_texts(&scr);
        let found = lines
            .iter()
            .chain(hist.iter())
            .any(|l| l.contains("GOODBYE"));
        assert!(found, "final output should be captured before reader exits");
    }

    #[test]
    #[allow(clippy::assertions_on_constants)] // documents the bound contract
    fn deferred_responses_bounded() {
        assert!(MAX_DEFERRED > 0 && MAX_DEFERRED <= 128);
    }

    #[test]
    fn session_manager_create_and_list() {
        let mut mgr = SessionManager::new();
        mgr.create("test1".into(), 80, 24, 1000).unwrap();
        let list = mgr.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "test1");
    }

    #[test]
    fn session_manager_duplicate_create_fails() {
        let mut mgr = SessionManager::new();
        mgr.create("test".into(), 80, 24, 1000).unwrap();
        assert!(mgr.create("test".into(), 80, 24, 1000).is_err());
    }

    #[test]
    fn session_manager_get_or_create() {
        let mut mgr = SessionManager::new();
        let (session, is_new) = mgr.get_or_create("test", 80, 24, 1000).unwrap();
        assert_eq!(session.name, "test");
        assert!(is_new);
        // Should return existing session
        let (session, is_new) = mgr.get_or_create("test", 80, 24, 1000).unwrap();
        assert_eq!(session.name, "test");
        assert!(!is_new);
        assert_eq!(mgr.list().len(), 1);
    }

    #[test]
    fn session_manager_remove() {
        let mut mgr = SessionManager::new();
        mgr.create("test".into(), 80, 24, 1000).unwrap();
        assert!(mgr.remove("test").is_some());
        assert!(mgr.remove("test").is_none());
        assert_eq!(mgr.list().len(), 0);
    }

    #[test]
    fn session_manager_get_or_create_zero_dimensions() {
        let mut mgr = SessionManager::new();
        let (session, is_new) = mgr.get_or_create("test", 0, 0, 1000).unwrap();
        // Should clamp to 80x24 defaults
        let dims = *session.dims.lock().unwrap();
        assert_eq!(dims.cols, 80);
        assert_eq!(dims.rows, 24);
        assert!(is_new);
    }

    #[test]
    fn validate_session_name_valid() {
        assert!(validate_session_name("my-session.1_OK").is_ok());
        assert!(validate_session_name("a").is_ok());
        assert!(validate_session_name(&"x".repeat(128)).is_ok());
    }

    #[test]
    fn validate_session_name_empty() {
        let err = validate_session_name("").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn validate_session_name_too_long() {
        let err = validate_session_name(&"x".repeat(129)).unwrap_err();
        assert!(err.to_string().contains("too long"));
    }

    #[test]
    fn validate_session_name_invalid_chars() {
        assert!(validate_session_name("foo/bar").is_err());
        assert!(validate_session_name("foo bar").is_err());
        assert!(validate_session_name("foo\0bar").is_err());
        assert!(validate_session_name("../escape").is_err());
    }

    #[test]
    fn session_manager_rejects_invalid_names() {
        let mut mgr = SessionManager::new();
        assert!(mgr.create("bad/name".into(), 80, 24, 1000).is_err());
        assert!(mgr.get_or_create("bad name", 80, 24, 1000).is_err());
    }

    #[test]
    fn take_dead_sessions_returns_dead() {
        let mut mgr = SessionManager::new();
        mgr.create("alive".into(), 80, 24, 100).unwrap();
        mgr.create("doomed".into(), 80, 24, 100).unwrap();

        // Kill the doomed session's child process
        {
            let session = mgr.get_mut("doomed").unwrap();
            let child_arc = session.pty.child_arc();
            let mut child = child_arc.lock().unwrap();
            child.kill().ok();
            child.wait().ok();
        }

        // Give the reader thread a moment to detect EOF
        std::thread::sleep(std::time::Duration::from_millis(200));

        let dead = mgr.take_dead_sessions();
        let dead_names: Vec<&str> = dead.iter().map(|s| s.name.as_str()).collect();
        assert!(
            dead_names.contains(&"doomed"),
            "dead list should contain 'doomed': {:?}",
            dead_names
        );
        assert!(
            !dead_names.contains(&"alive"),
            "dead list should not contain 'alive': {:?}",
            dead_names
        );

        // Only 'alive' should remain
        assert_eq!(mgr.list().len(), 1);
        assert_eq!(mgr.list()[0].name, "alive");
    }

    #[test]
    fn take_dead_sessions_empty_when_all_alive() {
        let mut mgr = SessionManager::new();
        mgr.create("s1".into(), 80, 24, 100).unwrap();
        mgr.create("s2".into(), 80, 24, 100).unwrap();

        let dead = mgr.take_dead_sessions();
        assert!(
            dead.is_empty(),
            "no sessions should be dead: {:?}",
            dead.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert_eq!(mgr.list().len(), 2);
    }

    #[test]
    fn client_guard_clears_has_client_on_drop() {
        let has_client = Arc::new(AtomicBool::new(true));
        let (_evict_tx, evict_rx) = tokio::sync::watch::channel(true);
        {
            let _guard = ClientGuard {
                has_client: has_client.clone(),
                evict_rx,
            };
            assert!(has_client.load(Ordering::Acquire));
        }
        assert!(!has_client.load(Ordering::Acquire));
    }

    #[test]
    fn client_guard_skips_clear_when_evicted() {
        let has_client = Arc::new(AtomicBool::new(true));
        let (evict_tx, evict_rx) = tokio::sync::watch::channel(true);
        {
            let _guard = ClientGuard {
                has_client: has_client.clone(),
                evict_rx,
            };
            let _ = evict_tx.send(false);
        }
        assert!(has_client.load(Ordering::Acquire));
    }
}
