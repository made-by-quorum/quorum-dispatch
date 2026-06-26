//! Headless-session container (WP-B2b-2b, design §A) — the parallel-map entry
//! that lets a daemon run a `claude -p` stream-json turn ALONGSIDE the deeply
//! PTY-shaped [`crate::session::Session`] WITHOUT threading a kind-enum through
//! every `Session` method (max blast radius). A [`HeadlessSession`] owns the
//! running [`HeadlessReader`] (which owns the child + reader/pump threads, kills
//! the child on the circuit-breaker, and joins both threads on drop) plus the
//! [`SubscriberHub`] the socket-republish relay subscribes to.
//!
//! Liveness: [`HeadlessSession::is_alive`] is true while the turn is in flight
//! (the reader thread is still draining). Once the reader runs to EOF/breaker the
//! session is reapable — the daemon's `SessionManager::take_dead_headless` drops
//! it (Drop joins the pump, delivering the terminal `RepublishEnd` to subscribers
//! first). This is the fact the LOAD-BEARING `len()`/`is_empty()`/
//! `all_sessions_ended()` edits read so `run_server` never exits mid-headless-turn.

use crate::headless::{Fanout, HeadlessLaunch, HeadlessReader, Sink, DEFAULT_QUEUE_CAP};
use crate::republish_hub::SocketFanoutSink;
use crate::republish_hub::SubscriberHub;
use crate::stream_json::DEFAULT_MAX_LINE_BYTES;
use std::sync::Arc;

/// Daemon-side resolver for a headless launch (design §D-2b, the injected
/// "DaemonCtx factory"). The daemon process resolves the launch posture
/// (`claude_bin`/`flags`/`env`/`cwd` — seam #1) AND the registry-status sink (the
/// sb-side `RegistryStatusSink`, which sbmux cannot name) for one
/// `LaunchHeadless`. The socket fan-out half of the composite is added by
/// [`SessionManager::create_headless`]; this resolver supplies only the
/// registry-write half so neither crate references the other's sink type.
///
/// `None` in [`crate::server::client_handler::DaemonCtx`] (bare sbmux / the test
/// fixture) means the daemon has no headless launch support → `LaunchHeadless`
/// is refused with a framed error.
pub trait HeadlessFactory: Send + Sync {
    /// Resolve a launch for session `name`. Returns the spawn spec plus the
    /// registry-status [`Sink`] (the `a` side of the composite). `Err` → the
    /// dispatch arm returns a framed `ServerMsg::Error` (no session created).
    ///
    /// WP-B5-i: `cwd` is the per-session working directory the child spawns in
    /// (`None` = the factory's own default / the daemon cwd); `claude_args` are
    /// extra passthrough flags the resolver appends AFTER its resolved posture
    /// flags (flags-first). Both ride per-request from `LaunchHeadless` so the
    /// daemon no longer resolves cwd/posture solely from its own environment.
    fn resolve(
        &self,
        name: &str,
        prompt: &str,
        resume_session_id: Option<&str>,
        cwd: Option<&str>,
        claude_args: &[String],
    ) -> anyhow::Result<HeadlessLaunchPlan>;
}

/// Builds the registry-status [`Sink`] once the claude CHILD pid is known. WP-B5-i
/// (identity option B): the sink must be keyed on the claude child pid, but the pid
/// only exists AFTER [`HeadlessLaunch::spawn`] — so the daemon-side factory cannot
/// build the sink at `resolve()` time. Instead it hands back THIS deferred builder;
/// [`HeadlessSession::start`] spawns the child, reads its pid, and calls this to
/// mint the child-pid-keyed sink. `FnOnce` (consumed exactly once per launch) +
/// `Send` (it crosses into `start`, which may run on any task).
pub type StatusSinkFactory = Box<dyn FnOnce(i64) -> Box<dyn Sink> + Send>;

/// The resolved plan for one headless launch: the spawn spec + the deferred
/// registry-status sink builder. `status_sink_factory` of `None` means "socket
/// fan-out only" (no registry write — e.g. a bare-sbmux launcher that has no `sb`
/// registry); `Some` is invoked with the claude child pid post-spawn and composed
/// into a [`Fanout`] with the socket sink so ONE reader/pump drives both.
pub struct HeadlessLaunchPlan {
    pub launch: HeadlessLaunch,
    pub status_sink_factory: Option<StatusSinkFactory>,
}

/// One running headless session: the reader/pump draining a `claude -p` child's
/// stdout into the composite sink, plus the hub the socket relay subscribes to.
pub struct HeadlessSession {
    name: String,
    /// The running reader (owns child + reader/pump threads; joins on drop, kills
    /// the child on breaker). `is_running()` is the liveness fact.
    reader: HeadlessReader,
    /// The socket subscribers' hub — the `SubscribeRepublish` relay registers here.
    hub: Arc<SubscriberHub>,
    /// Spawn time as Unix epoch seconds (`None` if the clock could not be read).
    created: Option<u64>,
}

impl HeadlessSession {
    /// Spawn the launch's child, compose `Fanout { a: status_sink, b:
    /// SocketFanoutSink(hub) }` (or just the socket sink when `status_sink` is
    /// `None`), and start the [`HeadlessReader`]. Returns the live session.
    pub fn start(name: String, plan: HeadlessLaunchPlan) -> anyhow::Result<Self> {
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs());

        let (child, stdout) = plan.launch.spawn()?;
        // WP-B5-i (identity option B): the claude child pid exists ONLY now (post-
        // spawn). Read it and hand it to the deferred status-sink factory so the
        // registry sink is keyed on the claude CHILD pid — NOT the daemon's own pid
        // (the rejected option A) — uniform with the interactive `<claude_pid>.json`
        // model and durable across daemon death (the §H.1/B5-ii property).
        let child_pid = child.pid();

        let hub = Arc::new(SubscriberHub::new());
        let socket_sink = SocketFanoutSink::new(hub.clone());

        // Compose the fan-out: ONE reader/pump drives BOTH the registry-status
        // write (a) AND the socket republish (b). Order is fixed (a then b) by
        // `Fanout`. When there is no registry sink (bare sbmux), the socket sink
        // stands alone.
        let sink: Box<dyn Sink> = match plan.status_sink_factory {
            Some(make_status) => Box::new(Fanout {
                a: make_status(child_pid),
                b: socket_sink,
            }),
            None => Box::new(socket_sink),
        };

        let reader = HeadlessReader::start(
            stdout,
            sink,
            child,
            DEFAULT_QUEUE_CAP,
            DEFAULT_MAX_LINE_BYTES,
        );

        Ok(Self {
            name,
            reader,
            hub,
            created,
        })
    }

    /// True while the turn is in flight (the reader thread is still draining).
    /// The daemon's lifecycle/reaper read this through the parallel `headless`
    /// map so `run_server` never exits mid-turn (the LOAD-BEARING fact).
    pub fn is_alive(&self) -> bool {
        self.reader.is_running()
    }

    /// Register a new socket subscriber, returning the receiver half for its relay
    /// task (`republish_hub::relay_subscriber`).
    pub fn subscribe(&self) -> tokio::sync::mpsc::Receiver<crate::protocol::messages::ServerMsg> {
        self.hub.subscribe()
    }

    /// A cloneable wake handle for the daemon's control-socket arm (R3c-Step-1).
    /// The arm clones this under the `manager` lock, releases the lock, then calls
    /// [`crate::headless::HeadlessWake::wake`] — so the wake never holds the lock
    /// (the no-starvation keystone, R3c-Step-0 point 2).
    pub fn wake_handle(&self) -> crate::headless::HeadlessWake {
        self.reader.wake_handle()
    }

    /// The session name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Spawn time as Unix epoch seconds (`None` if the clock could not be read).
    pub fn created(&self) -> Option<u64> {
        self.created
    }

    /// True once the circuit-breaker fired (the child was killed) — diagnostics.
    pub fn breaker_fired(&self) -> bool {
        self.reader.breaker_fired()
    }
}
