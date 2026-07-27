//! attended/driver.rs — the live per-session integration layer.
//!
//! This is the THIN glue the module head (`attended/mod.rs`) describes: it drives
//! the already-tested pure state machine ([`evaluate`], [`fire`]) with real time
//! and a real PTY. It owns three things:
//!
//! - [`AttendedState`] — the `Arc` handle bundle the server holds on each
//!   `Session` (journal, lock, spool) plus the channels into the timer task.
//! - the **per-session timer task** — a pure [`Scheduler`] (which held send fires,
//!   and when) driven by a minimal async runner (`select!` on the command channel
//!   and the earliest countdown). Spawned at session creation, **aborted in
//!   `Session::drop`** (no leaked task; crash-safe).
//! - [`SessionFireEffects`] — the real [`FireEffects`] binding over the session's
//!   PTY writer + screen model + child status source.
//!
//! # What is tested where
//! The correctness-critical DECISIONS are pure and unit-tested: [`evaluate`]
//! (timer), [`fire`] (fire sequence over injected seams), [`reconcile`]
//! (restart), and [`Scheduler::next_action`] (which send, and when) below. The
//! async runner + the real bindings are thin glue over those; the live
//! fire-over-a-real-PTY is M5's adversarial-battery territory.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::banner::{BannerSnapshot, CountdownView, ToastKind, ToastView};
use super::emitter::MuxEmitter;
use super::fire::{
    fire, FireConfig, FireEffects, FireOutcome, HarnessFacts, LandingProbe, SafeDefaultFacts,
    TranscriptLandingProbe,
};
use super::spool::{PendingRecord, Spool};
use super::{AttendedConfig, Clock, FireDecision, FirePhase, InputLock, Journal, SystemClock};

// ===========================================================================
// Path resolution (QD_HOME-honoring, matching dispatch's QdPaths + events_path).
// ===========================================================================

/// The authoritative delivery-ledger state dir: `QD_HOME/state`, else
/// `<home>/.quorum/dispatch/state` (bootstrap.ts:88-96; dispatch `QdPaths`). The
/// mux resolves this itself because it cannot import dispatch.
pub fn resolve_state_dir() -> Option<PathBuf> {
    if let Some(qd_home) = std::env::var_os("QD_HOME") {
        return Some(PathBuf::from(qd_home).join("state"));
    }
    dirs::home_dir().map(|h| h.join(".quorum").join("dispatch").join("state"))
}

/// The authoritative ledger file for a mux-held send:
/// `<state_dir>/sessions/<key>.events.jsonl`, `key = sessionId` when known, else
/// `byname-<name>` (dispatch `events_path`/`byname_key`, mirrored — the reader
/// merges the two).
pub fn ledger_path(state_dir: &std::path::Path, session: Option<&str>, name: &str) -> PathBuf {
    let key = match session {
        Some(sid) if !sid.is_empty() => sid.to_string(),
        _ => format!("byname-{name}"),
    };
    state_dir.join("sessions").join(format!("{key}.events.jsonl"))
}

// ===========================================================================
// StatusSource — the harness status seam (M4 fills; claude-shaped default).
// ===========================================================================

/// Where to read the hosted child's `busy`/`idle` status. The submit discipline
/// keys acceptance on `busy`. The status SOURCE is per-harness (M4's
/// `read_status_source()`), so M1 ships a claude-shaped default (scan
/// `<sessions_dir>/*.json` for the row whose `name` matches, read its `status`) —
/// mirrors dispatch's `find_pid_file`/`read_pid_status`.
///
/// `confirmable_acceptance` (M4 F1) is the STANDING capability "this harness has a
/// status source that can confirm turn acceptance at all" — a property of the
/// harness, NOT of a single `read()` (a confirmable source can still return `None`
/// transiently). The fire is gated OFF (before any clear/inject) when this is
/// false: firing without a confirmable idle/busy signal would inject a real turn
/// and then be unable to observe acceptance — reporting a real delivery as a
/// failure (the F1 defect) — and `composer_is_plain → Some(true)` alone cannot
/// rule out a mid-turn (busy) composer (the Q7 busy-state residual).
#[derive(Debug, Clone)]
pub struct StatusSource {
    pub sessions_dir: Option<PathBuf>,
    pub name: String,
    /// Whether this harness has ANY confirmable acceptance signal (M4 F1). See the
    /// struct doc. `claude_default` ⇒ true; `none_source` (codex/pi, Q7 residual)
    /// ⇒ false.
    pub confirmable_acceptance: bool,
}

impl StatusSource {
    /// The claude-shaped default: `<home>/.claude/sessions`. M4 supplies the real
    /// per-harness source (codex, pi) behind this seam. Acceptance IS confirmable
    /// (claude writes its `<pid>.json` status), so the fire runs normally — a
    /// transient `None` read never gates it off.
    pub fn claude_default(name: &str) -> Self {
        Self {
            sessions_dir: dirs::home_dir().map(|h| h.join(".claude").join("sessions")),
            name: name.to_string(),
            confirmable_acceptance: true,
        }
    }

    /// A harness with NO landed busy/idle status source (codex + pi — the Q7
    /// busy-state residual; not established by the Q7 spike, so not forced).
    /// Acceptance is NOT confirmable ⇒ the fire is gated OFF **before any
    /// clear/inject** (F1): the send resolves to an honest non-delivery terminal
    /// (`send-failed{acceptance-unconfirmable}`) with the composer UNTOUCHED — no
    /// clear, no inject, no CR, no delivery lie, no double-submit.
    ///
    /// Un-gating codex/pi delivery needs BOTH (either alone is a no-op):
    /// - **(a) reachability — LANDED (M5/T5).** A spawned codex/pi pane now resolves
    ///   to [`Harness::Codex`]/[`Harness::Pi`] via [`Harness::from_command`] (it
    ///   parses the `bash -lc command '<bin>' …` login-shell launch), so THIS
    ///   `none_source` + the CodexFacts/PiFacts/CodexLandingProbe + the F1 gate are
    ///   actually SELECTED. Before T5, argv0 was always `bash` ⇒ everything fell to
    ///   `Harness::Default` and this code was dead.
    /// - **(b) a genuinely-confirmable status source — STILL DEFERRED (M5/T6).** To
    ///   flip `confirmable_acceptance: true` HONESTLY the source must confirm
    ///   acceptance from primary source (a real busy transition after CR, or a
    ///   LandingProbe hit) BEFORE any success is reported (M4's F1). M5 observation
    ///   (codex 0.144.1 / pi 0.80.2, live): pi shows a reliable whole-turn
    ///   `Working…` status row (acceptance-confirmable) but its session transcript
    ///   is append-on-exit, so a LIVE landing is not LandingProbe-confirmable (and
    ///   the session dies with the mux); codex's `esc to interrupt` busy line is
    ///   REPLACED by streamed text mid-turn, so `wait_for_busy` can miss it → a
    ///   remediation-CR double-submit + false not-accepted (F1). Neither writes a
    ///   pollable busy/idle file. So both stay honestly verify-blocked. Re-entry:
    ///   pi needs a live-readable transcript source (or landing-as-acceptance in the
    ///   fire); codex needs a streaming-covering busy signal (or landing-as-
    ///   acceptance). When one lands, wire it here (`confirmable_acceptance: true` +
    ///   the real source) and the fire un-gates for that harness.
    pub fn none_source(name: &str) -> Self {
        Self {
            sessions_dir: None,
            name: name.to_string(),
            confirmable_acceptance: false,
        }
    }

    /// The STANDING capability "acceptance is confirmable for this harness" (M4 F1)
    /// — keys the fire gate. NOT a momentary `read()` value.
    pub fn is_acceptance_confirmable(&self) -> bool {
        self.confirmable_acceptance
    }

    /// Read the hosted child's status, or `None` if unresolvable (row absent /
    /// unreadable / no source). Best-effort; never panics.
    pub fn read(&self) -> Option<String> {
        let dir = self.sessions_dir.as_ref()?;
        let rd = std::fs::read_dir(dir).ok()?;
        for dent in rd.flatten() {
            let path = dent.path();
            if path.extension().map(|e| e != "json").unwrap_or(true) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            if v.get("name").and_then(|n| n.as_str()) == Some(self.name.as_str()) {
                return v
                    .get("status")
                    .and_then(|s| s.as_str())
                    .map(str::to_string);
            }
        }
        None
    }
}

// ===========================================================================
// Harness — the per-harness identity that selects the M4 facts/probe/status.
// ===========================================================================

/// Which PTY-carried harness a session hosts, derived from the spawned command's
/// program (argv0 basename). Only codex and pi have LANDED Q7 facts (the doctrine's
/// attended carriers); every other carrier stays on the safe claude-shaped default
/// (honest verify-blocked / not-accepted — no enablement without landed facts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Codex,
    Pi,
    /// claude (the incumbent default) or an unknown/shell carrier — both ride
    /// [`SafeDefaultFacts`] + [`TranscriptLandingProbe`] + the claude status source.
    Default,
}

impl Harness {
    /// Classify from the spawned command's argv0 (its basename). `None`/unknown ⇒
    /// [`Harness::Default`]. Matched tightly on the exact binary basename so a
    /// look-alike (e.g. `pip`, `pipx`) never misclassifies as `pi`.
    pub fn from_argv0(argv0: Option<&str>) -> Harness {
        let base = argv0
            .and_then(|p| p.rsplit(['/', '\\']).next())
            .unwrap_or("");
        // Strip a trailing platform suffix if any (defensive; lima has none).
        let base = base.strip_suffix(".exe").unwrap_or(base);
        match base {
            "codex" => Harness::Codex,
            "pi" => Harness::Pi,
            b if b.contains("claude") => Harness::Default,
            _ => Harness::Default,
        }
    }

    /// M5/T5 (reachability). Classify from a spawned [`crate::pty::CommandSpec`]'s
    /// argv. A DIRECT program (argv0 = the harness binary) classifies by basename
    /// via [`Harness::from_argv0`]. A LOGIN-SHELL launch — `["bash","-lc",<cmd>]`
    /// (or `sh`), the shape `CreateDetached` uses for EVERY provider — hides the
    /// real program inside `<cmd>`, so argv0 is always `bash` and
    /// [`Harness::from_argv0`] alone always yields [`Harness::Default`] (the M4 F1
    /// "reachability" gap: CodexFacts/PiFacts/the F1 gate were never selected). This
    /// parses the launched binary out of dispatch's `command '<bin>' …` assembly
    /// ([`crate::pty::CommandSpec::login_shell_c`] over
    /// `dispatch::launch::build_claude_cmd_from_argv`) and classifies THAT.
    ///
    /// SAFE for the accepted claude/default path (INVIOLABLE, byte-for-byte): the
    /// parse only ever PROMOTES an exact `codex`/`pi` bin basename to its harness.
    /// claude's bin basenames to [`Harness::Default`] either way (the `contains
    /// ("claude")`/`_` arms), and any cmd this cannot parse falls back to
    /// [`Harness::from_argv0`] on argv0 (`bash` → Default) — so the classification
    /// RESULT for claude and unknown carriers is unchanged. A cross-crate integration
    /// test pins the parser to the real launch assembly (r3), so an assembly change
    /// fails loudly rather than silently dropping codex/pi back to verify-blocked.
    pub fn from_command(argv: &[String]) -> Harness {
        let prog0 = argv.first().map(String::as_str);
        let is_login_shell = matches!(
            prog0.and_then(|p| p.rsplit(['/', '\\']).next()),
            Some("bash") | Some("sh")
        );
        if is_login_shell {
            if let Some(bin) = login_shell_launched_bin(argv) {
                return Harness::from_argv0(Some(&bin));
            }
        }
        Harness::from_argv0(prog0)
    }

    /// The M4 per-harness facts (codex/pi) or the safe default.
    fn facts(self) -> Arc<dyn HarnessFacts> {
        match self {
            Harness::Codex => Arc::new(super::fire::CodexFacts),
            Harness::Pi => Arc::new(super::fire::PiFacts),
            Harness::Default => Arc::new(SafeDefaultFacts),
        }
    }

    /// The M4 per-harness landing probe. codex rollouts need their own shape; pi
    /// records land via the (broadened) default probe alongside claude.
    fn probe(self) -> Arc<dyn LandingProbe> {
        match self {
            Harness::Codex => Arc::new(super::fire::CodexLandingProbe),
            Harness::Pi | Harness::Default => Arc::new(TranscriptLandingProbe),
        }
    }

    /// The status source. codex/pi have no landed busy/idle source (Q7 residual) ⇒
    /// a `none_source` whose acceptance is UNCONFIRMABLE ⇒ the fire is gated OFF
    /// before any clear/inject (F1: honest non-delivery, composer untouched, no
    /// delivery lie); claude/default rides the claude-shaped registry source and
    /// fires normally.
    fn status_source(self, name: &str) -> StatusSource {
        match self {
            Harness::Codex | Harness::Pi => StatusSource::none_source(name),
            Harness::Default => StatusSource::claude_default(name),
        }
    }
}

/// Extract the launched binary from a `["<shell>","-lc",<cmd>]` argv where `<cmd>`
/// is dispatch's `command '<bin>' '<flag>' …` assembly (optionally behind a
/// self-deleting dot-source env prefix `. '<f>'; rm -f '<f>'; …`). Returns the FIRST
/// `command '<bin>'` segment's bin — the harness binary — or `None` if the shape is
/// not recognized (caller falls back to a safe [`Harness::Default`]).
///
/// Robust to `;` inside a later single-quoted flag value: it takes the FIRST
/// `;`-segment that (trimmed) begins `command '` and reads only up to the next `'`
/// (the bin), so a `;` in a subsequent flag never corrupts the bin. The dot-source
/// prefix segments begin with `.`/`rm`, never `command '`.
fn login_shell_launched_bin(argv: &[String]) -> Option<String> {
    if argv.len() != 3 || argv.get(1).map(String::as_str) != Some("-lc") {
        return None;
    }
    let cmd = argv.get(2)?;
    for seg in cmd.split(';') {
        if let Some(rest) = seg.trim_start().strip_prefix("command '") {
            let bin = rest.split('\'').next()?;
            if !bin.is_empty() {
                return Some(bin.to_string());
            }
        }
    }
    None
}

// ===========================================================================
// SessionFireEffects — the real FireEffects binding.
// ===========================================================================

/// The live [`FireEffects`] for a session: raw PTY writes to the session's writer,
/// screen render via the qrmux screen model, child status via [`StatusSource`].
/// Blocking by construction (the discipline is synchronous); the runner drives it
/// in `spawn_blocking`.
pub struct SessionFireEffects {
    pty_writer: crate::pty::SharedPtyWriter,
    screen: crate::session::SharedScreen,
    status: StatusSource,
    clock: SystemClock,
}

impl SessionFireEffects {
    pub fn new(
        pty_writer: crate::pty::SharedPtyWriter,
        screen: crate::session::SharedScreen,
        status: StatusSource,
    ) -> Self {
        Self {
            pty_writer,
            screen,
            status,
            clock: SystemClock,
        }
    }

    fn write_pty(&self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut w = self
            .pty_writer
            .lock()
            .map_err(|_| std::io::Error::other("pty_writer mutex poisoned"))?;
        w.write_all(bytes)?;
        w.flush()
    }
}

impl FireEffects for SessionFireEffects {
    fn send_text(&self, text: &str) {
        let _ = self.write_pty(text.as_bytes());
    }
    fn send_cr(&self) {
        let _ = self.write_pty(b"\r");
    }
    fn write_raw(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.write_pty(bytes)
    }
    fn read_screen(&self) -> String {
        match self.screen.lock() {
            Ok(s) => crate::screen::screen_text(&s),
            Err(p) => crate::screen::screen_text(&p.into_inner()),
        }
    }
    fn read_status(&self) -> Option<String> {
        self.status.read()
    }
    fn acceptance_confirmable(&self) -> bool {
        // M4 F1: a standing property of the harness's status source, not a read.
        self.status.is_acceptance_confirmable()
    }
    fn sleep(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

// ===========================================================================
// Scheduler — the PURE "which send fires, and when" decision (unit-tested).
// ===========================================================================

/// A send the timer is tracking.
struct InFlight {
    record: PendingRecord,
    message: String,
    /// Deliver-now forced this send (fire immediately, ignore the countdown).
    forced: bool,
}

/// What the runner should do next, computed purely from the in-flight set + the
/// journal + the clock.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum SchedAction {
    /// Fire this send now.
    Fire(String),
    /// No send is eligible yet; sleep this many ms then re-evaluate (the earliest
    /// countdown deadline).
    Sleep(u64),
    /// Nothing in flight — block for the next command.
    Idle,
}

/// The in-flight set + the pure decision. Sends are kept in ACCEPTANCE order.
#[derive(Default)]
struct Scheduler {
    sends: Vec<InFlight>,
}

impl Scheduler {
    fn add(&mut self, record: PendingRecord, message: String) {
        self.sends.push(InFlight {
            record,
            message,
            forced: false,
        });
    }

    /// Take (remove) the named send out of the set (about to fire / resolved).
    fn take(&mut self, send_id: &str) -> Option<InFlight> {
        let idx = self.sends.iter().position(|s| s.record.send_id == send_id)?;
        Some(self.sends.remove(idx))
    }

    /// Mark a send forced (deliver-now). `None` forces the EARLIEST-accepted send.
    fn force(&mut self, send_id: Option<&str>) {
        match send_id {
            Some(id) => {
                if let Some(s) = self.sends.iter_mut().find(|s| s.record.send_id == id) {
                    s.forced = true;
                }
            }
            None => {
                if let Some(s) = self.sends.first_mut() {
                    s.forced = true;
                }
            }
        }
    }

    /// The next action. A forced (deliver-now) send fires first; else the earliest
    /// send whose [`evaluate`] is `Immediate`; else sleep to the earliest countdown
    /// deadline; else idle. Pure — no clock/fs.
    fn next_action(&self, journal: &Journal, cfg: &AttendedConfig, now_ms: i64) -> SchedAction {
        if let Some(s) = self.sends.iter().find(|s| s.forced) {
            return SchedAction::Fire(s.record.send_id.clone());
        }
        let mut min_deadline: Option<i64> = None;
        for s in &self.sends {
            match super::evaluate(journal, s.record.accepted_at_ms, now_ms, cfg, s.record.priority)
            {
                FireDecision::Immediate => return SchedAction::Fire(s.record.send_id.clone()),
                FireDecision::Countdown { deadline_ms } => {
                    min_deadline =
                        Some(min_deadline.map_or(deadline_ms, |m: i64| m.min(deadline_ms)));
                }
            }
        }
        match min_deadline {
            Some(d) => SchedAction::Sleep((d - now_ms).max(0) as u64),
            None => SchedAction::Idle,
        }
    }

    /// PURE read for the M2 banner: the earliest held send's countdown deadline + whether it is a
    /// priority send, or `None` when nothing is being held (empty set / everything Immediate /
    /// a forced deliver-now about to fire). Mirrors `next_action`'s Countdown branch WITHOUT any
    /// state change — the banner reads the same decision the timer acts on, so the display can never
    /// disagree with the live timer. (The read-only exposure ruled in `01KXAK5FPK`.)
    ///
    /// N1 (red-team r1): this shows the earliest-**deadline** (the soonest-to-fire — what the human
    /// wants from "how long until something fires"), whereas `deliver_now(None)` fires the
    /// earliest-**accepted** (`force` → `sends.first()`, M1's FIFO). In the single-held-send norm
    /// they coincide; only >1 concurrent held send on one session can differ (rare) — accepted as
    /// known behavior (reconciling would change M1's `force` scope or show a not-soonest deadline).
    fn banner_countdown(
        &self,
        journal: &Journal,
        cfg: &AttendedConfig,
        now_ms: i64,
    ) -> Option<CountdownView> {
        // A forced (deliver-now) send fires immediately → no countdown to show.
        if self.sends.iter().any(|s| s.forced) {
            return None;
        }
        let mut best: Option<CountdownView> = None;
        for s in &self.sends {
            if let FireDecision::Countdown { deadline_ms } =
                super::evaluate(journal, s.record.accepted_at_ms, now_ms, cfg, s.record.priority)
            {
                let is_earlier = match best {
                    None => true,
                    Some(b) => deadline_ms < b.deadline_ms,
                };
                if is_earlier {
                    best = Some(CountdownView {
                        deadline_ms,
                        priority: s.record.priority,
                    });
                }
            }
        }
        best
    }

    /// M5/T1 (MV4 countdown-start durable write, M1 carried deviation ruling
    /// `01KXA3V6QX`). The durability posture names THREE machinery-owned write
    /// points: acceptance / **countdown-start** / fire-start-before-clear. M1 wired
    /// acceptance (write-ahead) and fire-start (in [`super::fire::fire`]) but NOT
    /// countdown-start: the spool SUPPORTS it and it is unit-tested
    /// (`phase_transitions_persist_via_overwrite`) yet the live driver never called
    /// it, so a mid-countdown crash carried an EMPTY draft in the durable record.
    ///
    /// This is the missing live wiring: for each held send whose durable record is
    /// still at `Accepted` (never snapshotted) and which is NOW in a countdown,
    /// mark it `Countdown` and snapshot the current journal draft into the record.
    /// Returns the records the runner must durably spool-write (the IO stays in the
    /// runner). IDEMPOTENT — a record already past `Accepted` is skipped, so this
    /// writes at most ONCE per send, at the `Accepted→Countdown` transition; the
    /// draft is refreshed again at fire-start (per-EVENT discipline, never
    /// per-keystroke — a post-snapshot in-window keystroke delta is baseline-
    /// comparable, BUILD-DIRECTIVES §2c).
    ///
    /// Under the deployed PTY-dies-with-mux fact this is inert for a live resurface
    /// (a mid-countdown draft has no live composer to restore into), but the durable
    /// record is what the M5 crash-battery observes — so the WIRING must be live —
    /// and it is correct-by-construction for any session-survives-the-mux variant,
    /// where the countdown-start snapshot is exactly what re-surfaces.
    fn countdown_start_snapshots(
        &mut self,
        journal: &Journal,
        cfg: &AttendedConfig,
        now_ms: i64,
    ) -> Vec<PendingRecord> {
        // A forced deliver-now fires immediately — no countdown, nothing to snapshot.
        if self.sends.iter().any(|s| s.forced) {
            return Vec::new();
        }
        let mut out = Vec::new();
        for s in &mut self.sends {
            if s.record.phase != FirePhase::Accepted {
                continue; // already snapshotted (or firing) — idempotent skip
            }
            if let FireDecision::Countdown { .. } =
                super::evaluate(journal, s.record.accepted_at_ms, now_ms, cfg, s.record.priority)
            {
                s.record.phase = FirePhase::Countdown;
                s.record.draft = journal.draft().to_vec();
                out.push(s.record.clone());
            }
        }
        out
    }
}

// ===========================================================================
// AttendedState + the timer task.
// ===========================================================================

/// A command to the per-session timer task.
enum Cmd {
    /// A new pending send has been write-ahead spooled — track + schedule it.
    Accept { record: PendingRecord, message: String },
    /// Deliver-now: fire the target (or the earliest) held send immediately.
    DeliverNow(Option<String>),
    /// A human keystroke landed — wake to re-evaluate (the deadline moved).
    Keystroke,
}

/// The `Arc` bundle the server holds on each `Session`. Journal + lock + spool are
/// the shared state; the channels reach the timer task.
pub struct AttendedState {
    /// The authoritative human draft (P1). Fed ONLY by attach-scoped human input.
    pub journal: Arc<Mutex<Journal>>,
    /// The fires-while-typing lock (P3, QS-1).
    pub lock: Arc<Mutex<InputLock>>,
    /// The durable pending-delivery store (RT-R1).
    pub spool: Arc<Spool>,
    config: AttendedConfig,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
    /// Outcome watch for `--wait` (M3): `(send_id, terminal_kind)` per resolved
    /// send. The mux still WRITES the terminal to the ledger; this is the watch.
    outcomes: tokio::sync::broadcast::Sender<(String, String)>,
    /// M2 banner state. The timer task is the SOLE WRITER (publishes its
    /// already-computed phase/deadline here on every change); the mux status-line
    /// surface subscribes via [`AttendedState::banner_rx`] and reads it lock-free /
    /// bounded-staleness. A `watch` carries only the latest value — perfect for a
    /// display that repaints on transitions. Read-only exposure ruled `01KXAK5FPK`.
    banner_tx: tokio::sync::watch::Sender<BannerSnapshot>,
    /// The timer task handle — aborted in [`AttendedState::shutdown`]
    /// (`Session::drop`). `None` when no runtime was present at construction
    /// (sync tests): the pure state still works; only the live driver is absent.
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl AttendedState {
    /// Build the state and spawn the timer task (iff a tokio runtime is present).
    /// `spool_dir` is `<socket_dir>/pending/<name>/`; the emitter's ledger path is
    /// resolved lazily per send from the handoff's sessionId.
    pub fn new(
        name: String,
        spool_dir: PathBuf,
        pty_writer: crate::pty::SharedPtyWriter,
        screen: crate::session::SharedScreen,
        harness: Harness,
    ) -> std::io::Result<Arc<Self>> {
        let spool = Arc::new(Spool::open(spool_dir)?);
        let journal = Arc::new(Mutex::new(Journal::new()));
        let lock = Arc::new(Mutex::new(InputLock::new()));
        let config = AttendedConfig::default();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (outcomes, _) = tokio::sync::broadcast::channel(64);
        let (banner_tx, _banner_rx0) = tokio::sync::watch::channel(BannerSnapshot::default());

        let runner = Runner {
            name,
            harness,
            journal: journal.clone(),
            lock: lock.clone(),
            spool: spool.clone(),
            config,
            fire_cfg: FireConfig::default(),
            // M4: per-harness facts/probe. No landed facts for a harness ⇒
            // Harness::Default ⇒ SafeDefaultFacts (honest verify-blocked /
            // not-accepted, recorded deferred-flagged).
            facts: harness.facts(),
            probe: harness.probe(),
            pty_writer,
            screen,
            outcomes: outcomes.clone(),
            state_dir: resolve_state_dir(),
            emitter: None,
            scheduler: Scheduler::default(),
            banner_tx: banner_tx.clone(),
            toast: None,
        };
        // Spawn the driver ONLY when a runtime exists (server path). Sync unit
        // tests construct sessions with no runtime — they exercise the pure state.
        let task = if tokio::runtime::Handle::try_current().is_ok() {
            Some(tokio::spawn(run_driver(runner, cmd_rx)))
        } else {
            None
        };

        Ok(Arc::new(Self {
            journal,
            lock,
            spool,
            config,
            cmd_tx,
            outcomes,
            banner_tx,
            task: Mutex::new(task),
        }))
    }

    /// Accept a pending send (client_handler `PendingDelivery`): write-ahead spool
    /// (the durable ACCEPTANCE write point — before the queued receipt) then hand
    /// it to the timer task. Returns the durable-write result; the caller returns
    /// `DeliveryQueued` only on `Ok`.
    pub fn accept(
        &self,
        send_id: String,
        data: Vec<u8>,
        content_sha256: String,
        content_len: u64,
        transcript: Option<String>,
        transcript_offset: Option<u64>,
        session: Option<String>,
        name: String,
        verb: &str,
        priority: bool,
        now_ms: i64,
    ) -> std::io::Result<()> {
        let mut record = PendingRecord::accepted(
            send_id,
            content_sha256,
            content_len,
            session,
            Some(name),
            verb,
            priority,
            now_ms,
        );
        record.transcript = transcript;
        record.transcript_offset = transcript_offset;
        // Write-ahead: the send exists durably BEFORE the sender's queued receipt.
        self.spool.write(&record)?;
        let message = String::from_utf8_lossy(&data).into_owned();
        // Best-effort hand-off to the task (absent in sync tests).
        let _ = self.cmd_tx.send(Cmd::Accept { record, message });
        Ok(())
    }

    /// Feed one attach-scoped human input burst (the `client_to_pty` Input arm):
    /// journal it, perform the passthrough PTY write ATOMICALLY w.r.t. fire-start,
    /// then wake the timer (keystroke-reset).
    ///
    /// The passthrough write happens **inside the input-lock critical section**
    /// (see [`journal_admit_passthrough`]): `fire`'s `lock_and_snapshot` must take
    /// that SAME input lock to arm + snapshot, so it cannot arm+clear until this
    /// write has landed — the write is provably pre-clear. This replaces the earlier
    /// design that returned an [`super::Admit`] for the relay to write LATER on a
    /// separate deferred `spawn_blocking` (adv-r1 F1 / QS-1: that deferred write was
    /// unsequenced against fire-start and could land post-clear, duplicating the
    /// keystroke / entering the PTY mid-fire). A `Buffered` admit writes nothing (a
    /// fire is in progress; the bytes flush in order on unlock).
    ///
    /// **BLOCKING** (it does the PTY write): the caller MUST run this on a blocking
    /// thread (`tokio::task::spawn_blocking`). A write error is returned to the
    /// caller (the relay propagates it, ending the relay exactly as the pre-fix
    /// `.await??` did).
    pub fn on_human_input_passthrough(
        &self,
        bytes: &[u8],
        now_ms: i64,
        pty_writer: &crate::pty::SharedPtyWriter,
    ) -> std::io::Result<()> {
        journal_admit_passthrough(
            &self.lock,
            &self.journal,
            bytes,
            now_ms,
            self.config.paste_threshold,
            |b| {
                use std::io::Write;
                let mut w = pty_writer
                    .lock()
                    .map_err(|_| std::io::Error::other("pty_writer mutex poisoned"))?;
                w.write_all(b)?;
                w.flush()
            },
        )?;
        // Wake the timer OUTSIDE the critical section (a non-blocking channel send;
        // the timer re-reads the freshly-journaled draft on wake). Sent only AFTER
        // the passthrough write has landed and the input lock released, so a fire
        // this wake drives (evaluate → Immediate → fire, at the countdown ceiling or
        // on deliver-now) arms + clears strictly after the write — never racing it.
        let _ = self.cmd_tx.send(Cmd::Keystroke);
        Ok(())
    }

    /// Deliver-now (the `DeliverNow` control): fire the target (or earliest) held
    /// send immediately, WITHOUT a countdown reset and WITHOUT a journal entry.
    pub fn deliver_now(&self, send_id: Option<String>) {
        let _ = self.cmd_tx.send(Cmd::DeliverNow(send_id));
    }

    /// Subscribe to delivery outcomes (`--wait`, M3).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<(String, String)> {
        self.outcomes.subscribe()
    }

    /// Subscribe to the M2 banner state (read-only; the mux status-line surface).
    /// The receiver sees the latest [`BannerSnapshot`] the timer task published and
    /// wakes on `changed()` when it moves. Read-only exposure ruled `01KXAK5FPK`.
    pub fn banner_rx(&self) -> tokio::sync::watch::Receiver<BannerSnapshot> {
        self.banner_tx.subscribe()
    }

    /// Abort the timer task (called from `Session::drop`). No leaked task.
    pub fn shutdown(&self) {
        if let Ok(mut g) = self.task.lock() {
            if let Some(h) = g.take() {
                h.abort();
            }
        }
    }
}

impl Drop for AttendedState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The owned state the timer task runs over.
struct Runner {
    name: String,
    /// The hosted harness (M4) — selects the status source at fire time (facts and
    /// probe are resolved once at construction).
    harness: Harness,
    journal: Arc<Mutex<Journal>>,
    lock: Arc<Mutex<InputLock>>,
    spool: Arc<Spool>,
    config: AttendedConfig,
    fire_cfg: FireConfig,
    facts: Arc<dyn HarnessFacts>,
    probe: Arc<dyn LandingProbe>,
    pty_writer: crate::pty::SharedPtyWriter,
    screen: crate::session::SharedScreen,
    outcomes: tokio::sync::broadcast::Sender<(String, String)>,
    state_dir: Option<PathBuf>,
    /// Lazily built from the first send's sessionId, then reused so `seq` stays
    /// monotonic for this writer (pid).
    emitter: Option<Arc<MuxEmitter>>,
    scheduler: Scheduler,
    /// M2 banner publisher (this task is the SOLE WRITER). Presentation only.
    banner_tx: tokio::sync::watch::Sender<BannerSnapshot>,
    /// The last terminal outcome, held so its toast persists across loop turns
    /// until its window elapses (or, for a failure/recovery toast, a keystroke).
    toast: Option<ToastView>,
}

impl Runner {
    /// Publish the current banner state (SOLE WRITER). Expires a stale toast and
    /// pairs the given countdown with whatever toast is still in-window. A pure
    /// `watch::send` — it never blocks the timer and never touches the
    /// journal/lock/spool/emitter or any fire ordering.
    fn publish_banner(&mut self, countdown: Option<CountdownView>, now_ms: i64) {
        // Drop an expired toast so it does not linger in the published snapshot.
        if let Some(t) = &self.toast {
            let expired = now_ms - t.shown_at_ms >= toast_window_ms(&t.kind);
            if expired {
                self.toast = None;
            }
        }
        let snap = BannerSnapshot {
            countdown,
            toast: self.toast.clone(),
        };
        // Notify the relay ONLY when the snapshot actually changed — a `send` every
        // driver turn would wake the relay for a no-op render each loop (red-team r1
        // N4). The 1s tick handles the per-second countdown decrement independently,
        // so suppressing unchanged publishes costs no display freshness.
        self.banner_tx.send_if_modified(move |cur| {
            if *cur == snap {
                false
            } else {
                *cur = snap;
                true
            }
        });
    }

    /// The banner countdown at `now_ms` (a brief journal read + a pure scheduler read).
    fn countdown_now(&self, now_ms: i64) -> Option<CountdownView> {
        let j = lock_journal(&self.journal);
        self.scheduler.banner_countdown(&j, &self.config, now_ms)
    }
}

/// The display window for a toast kind (ms). Mirrors [`super::banner`]'s per-kind windows.
fn toast_window_ms(kind: &ToastKind) -> i64 {
    match kind {
        ToastKind::Delivered => super::banner::DELIVERED_TOAST_MS,
        ToastKind::Failed { .. } => super::banner::FAILED_TOAST_MS,
    }
}

impl Runner {
    /// The emitter for this session's authoritative ledger, built once from the
    /// first send's handoff metadata (sessionId + name).
    fn emitter_for(&mut self, rec: &PendingRecord) -> Option<Arc<MuxEmitter>> {
        if let Some(e) = &self.emitter {
            return Some(e.clone());
        }
        let state_dir = self.state_dir.as_ref()?;
        let path = ledger_path(state_dir, rec.session.as_deref(), &self.name);
        let start_ms = crate::procid::pid_start_ms(std::process::id()).map(|v| v as i64);
        let em = Arc::new(MuxEmitter::new(
            path,
            rec.session.clone(),
            rec.name.clone(),
            start_ms,
        ));
        self.emitter = Some(em.clone());
        Some(em)
    }
}

/// The per-session timer task. Drives [`Scheduler::next_action`]: on `Fire` it
/// runs the (blocking) [`fire`] sequence in `spawn_blocking`, emits the terminal,
/// notifies `--wait`, and clears the spool; on `Sleep` it waits (interruptible by
/// a command); on `Idle` it blocks for the next command.
async fn run_driver(mut r: Runner, mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>) {
    loop {
        let now = SystemClock.now_ms();
        let (action, countdown) = {
            let j = lock_journal(&r.journal);
            (
                r.scheduler.next_action(&j, &r.config, now),
                r.scheduler.banner_countdown(&j, &r.config, now),
            )
        };
        // Publish the banner state each turn (SOLE WRITER): reflects the very
        // decision the timer is about to act on, so the display cannot disagree.
        r.publish_banner(countdown, now);
        // M5/T1 (MV4): durably snapshot the draft at COUNTDOWN-START — the second of
        // the three machinery-owned write points. Idempotent (at most once per send
        // at the Accepted→Countdown transition); the runner owns the durable IO. A
        // persist failure is logged, never fatal (best-effort, matching the on-boot
        // sweep's posture) — the fire-start snapshot is the durable backstop.
        {
            let snaps = {
                let j = lock_journal(&r.journal);
                r.scheduler.countdown_start_snapshots(&j, &r.config, now)
            };
            for rec in &snaps {
                if let Err(e) = r.spool.write(rec) {
                    tracing::warn!(
                        session = %r.name, send_id = %rec.send_id, error = %e,
                        "attended: countdown-start draft snapshot failed to persist"
                    );
                }
            }
        }
        match action {
            SchedAction::Fire(send_id) => {
                let Some(inflight) = r.scheduler.take(&send_id) else {
                    continue;
                };
                fire_one(&mut r, inflight).await;
            }
            SchedAction::Idle => match cmd_rx.recv().await {
                Some(cmd) => handle_cmd(&mut r, cmd),
                None => break, // all senders dropped → session gone
            },
            SchedAction::Sleep(ms) => {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(ms.max(1))) => {}
                    cmd = cmd_rx.recv() => match cmd {
                        Some(cmd) => handle_cmd(&mut r, cmd),
                        None => break,
                    },
                }
            }
        }
    }
}

fn handle_cmd(r: &mut Runner, cmd: Cmd) {
    match cmd {
        Cmd::Accept { record, message } => r.scheduler.add(record, message),
        Cmd::DeliverNow(send_id) => r.scheduler.force(send_id.as_deref()),
        Cmd::Keystroke => {
            // M2: a keystroke means the human is now engaging (e.g. editing the
            // restored draft) — clear a lingering recovery notice so it doesn't
            // shadow their fresh typing. The loop-top publish reflects it. The
            // delivered toast is left to expire on its own (a keystroke mid-fast-
            // send shouldn't blink it away). Presentation only; no journal touch.
            if matches!(
                &r.toast,
                Some(ToastView {
                    kind: ToastKind::Failed { .. },
                    ..
                })
            ) {
                r.toast = None;
            }
        }
    }
}

/// Classify a terminal event name into a banner toast: the success terminal is a
/// delivered toast; every other terminal kind is the recovery notice. A
/// non-terminal name (defensive) raises nothing. Consumes the shared vocabulary's
/// `is_terminal` AND `is_success_terminal` — a new terminal kind is classified for
/// free, and "which terminal means delivered" is the leaf crate's ONE definition
/// (F5/M2 de-dup: never a locally-minted `"message-seen"` success literal).
fn toast_kind_for(event_name: &str) -> Option<ToastKind> {
    if !quorum_delivery_events::is_terminal(event_name) {
        return None;
    }
    if quorum_delivery_events::is_success_terminal(event_name) {
        Some(ToastKind::Delivered)
    } else {
        Some(ToastKind::Failed {
            reason: event_name.to_string(),
        })
    }
}

/// Fire one send: run the blocking fire in `spawn_blocking`, then emit + notify +
/// clear the spool on a terminal (leave it spooled on `Pending`).
async fn fire_one(r: &mut Runner, inflight: InFlight) {
    let effects = SessionFireEffects::new(
        r.pty_writer.clone(),
        r.screen.clone(),
        r.harness.status_source(&r.name),
    );
    let facts = r.facts.clone();
    let probe = r.probe.clone();
    let lock = r.lock.clone();
    let journal = r.journal.clone();
    let spool = r.spool.clone();
    let fire_cfg = r.fire_cfg;
    let record = inflight.record.clone();
    let message = inflight.message.clone();

    let outcome = tokio::task::spawn_blocking(move || {
        fire(
            &effects, &*facts, &*probe, &lock, &journal, &spool, record, &message, &fire_cfg,
        )
    })
    .await;

    let outcome = match outcome {
        Ok(o) => o,
        Err(_) => return, // fire task panicked/cancelled — leave spooled for reconcile
    };

    match outcome {
        FireOutcome::Terminal(payload) => {
            let kind = payload.event_name().to_string();
            // M2: raise a transient toast for the outcome (delivered vs the
            // recovery notice). Classified from the shared-vocabulary event name,
            // never a locally-minted string. Presentation only.
            let toast_kind = toast_kind_for(&kind);
            if let Some(em) = r.emitter_for(&inflight.record) {
                if let Err(e) = em.emit(&SystemClock, &payload) {
                    tracing::warn!(session = %r.name, error = %e, "mux terminal emit failed");
                }
            }
            let _ = r
                .outcomes
                .send((inflight.record.send_id.clone(), kind));
            let _ = r.spool.remove(&inflight.record.send_id);
            if let Some(kind) = toast_kind {
                let now = SystemClock.now_ms();
                r.toast = Some(ToastView {
                    kind,
                    shown_at_ms: now,
                });
                // Publish immediately so the toast shows without waiting for the
                // next scheduler turn.
                let cd = r.countdown_now(now);
                r.publish_banner(cd, now);
            }
        }
        FireOutcome::Pending => {
            // Injected + accepted, landing not yet confirmable: no terminal now
            // (never a false landed). The record stays spooled at FireCompleted;
            // restart reconciliation / a `--wait` await resolves it later.
            tracing::debug!(
                session = %r.name,
                send_id = %inflight.record.send_id,
                "send fired, landing unconfirmed within window — left pending for reconcile"
            );
        }
    }
}

fn lock_journal(j: &Mutex<Journal>) -> std::sync::MutexGuard<'_, Journal> {
    j.lock().unwrap_or_else(|p| p.into_inner())
}

/// Journal + admit + (on `Passthrough`) the PTY write, ALL under the input lock —
/// the atomic core of [`AttendedState::on_human_input_passthrough`].
///
/// Acquires the INPUT LOCK first, journals the burst under it, admits, and — when
/// admitted [`Passthrough`](super::Admit::Passthrough) — performs the PTY `write`
/// **WHILE STILL HOLDING the input-lock guard**. That hold is the mechanism that
/// closes adv-r1 F1 (QS-1): `fire`'s `lock_and_snapshot` must acquire this SAME
/// input lock to arm the fire and take the draft snapshot (see [`super::fire`]'s
/// `lock_and_snapshot`), so it cannot arm+clear until this passthrough write has
/// landed. The passthrough byte is therefore *guaranteed* to happen-before the
/// fire's clear-chord — restoring M1's "the passthrough byte is pre-clear, wiped by
/// the clear-chord and re-shown by the replay" as a real MUTUAL-EXCLUSION guarantee,
/// not the timing assumption that a deferred `spawn_blocking` write wins the race
/// (it need not). A racing keystroke thus lands in the snapshot (admitted
/// `Passthrough` before the arm → its passthrough byte is cleared, then replayed
/// once) XOR the buffer (admitted `Buffered` after the arm → flushed once on unlock)
/// — written to the PTY EXACTLY ONCE either way, never both (no duplication), never
/// neither (no loss). The burst is still ALWAYS journaled (buffered or not), so the
/// journal draft stays consistent with the composer across fires.
///
/// **Lock order — no inversion / deadlock:** INPUT LOCK is taken first, held across
/// the (brief) JOURNAL lock, then across the passthrough `write` — which acquires
/// ONLY the pty_writer lock. `fire` takes INPUT→JOURNAL (`lock_and_snapshot`,
/// `unlock_input`) and the pty_writer lock ALONE per raw write, never
/// pty_writer→INPUT; so the global acquisition order is always
/// INPUT-before-PTY_WRITER — no cycle. **No latency stall:** `write` blocks at most
/// on one of the fire's own brief per-write pty_writer holds (or the kernel PTY
/// write itself, exactly as the pre-fix deferred write did), never on the fire
/// completing. Poisoned input lock is recovered (the flag/buffer is the only state);
/// a poisoned journal skips the append (matches the pre-existing tolerance). A write
/// error propagates (the relay handles it as before).
fn journal_admit_passthrough<W: FnOnce(&[u8]) -> std::io::Result<()>>(
    lock: &Mutex<InputLock>,
    journal: &Mutex<Journal>,
    bytes: &[u8],
    now_ms: i64,
    paste_threshold: usize,
    write: W,
) -> std::io::Result<super::Admit> {
    let mut l = lock.lock().unwrap_or_else(|p| p.into_inner());
    if let Ok(mut j) = journal.lock() {
        j.on_human_input(bytes, now_ms, paste_threshold);
    }
    let admit = l.admit(bytes);
    if let super::Admit::Passthrough(b) = &admit {
        // Written WHILE `l` (the input-lock guard) is still held — the happens-before
        // that makes the passthrough byte pre-clear w.r.t. any fire. `l` drops at the
        // end of this function, AFTER the write has landed (or errored out).
        write(b)?;
    }
    Ok(admit)
}

/// The pure (journal-append, admit) core — NO PTY write. Retained as a test helper
/// for the in-memory draft-reconstruction test (`racing_keystroke_is_never_…`),
/// which models `snapshot ++ flushed` and owns the write ordering itself. Production
/// input goes through [`journal_admit_passthrough`], which writes under the lock.
/// Same lock order (INPUT LOCK before JOURNAL) and poison tolerance.
#[cfg(test)]
fn journal_and_admit(
    lock: &Mutex<InputLock>,
    journal: &Mutex<Journal>,
    bytes: &[u8],
    now_ms: i64,
    paste_threshold: usize,
) -> super::Admit {
    let mut l = lock.lock().unwrap_or_else(|p| p.into_inner());
    if let Ok(mut j) = journal.lock() {
        j.on_human_input(bytes, now_ms, paste_threshold);
    }
    l.admit(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: usize = 8;

    fn cfg() -> AttendedConfig {
        AttendedConfig {
            countdown_ceiling_ms: 60_000,
            priority_ceiling_ms: 10_000,
            quiet_window_ms: 3_000,
            paste_threshold: T,
        }
    }

    fn rec(send_id: &str, accepted_at: i64, priority: bool) -> PendingRecord {
        PendingRecord::accepted(
            send_id,
            "sha",
            5,
            Some("sid".into()),
            Some("alpha".into()),
            "send:pty",
            priority,
            accepted_at,
        )
    }

    // ---- Scheduler.next_action (the pure decision) --------------------------

    #[test]
    fn empty_scheduler_is_idle() {
        let s = Scheduler::default();
        let j = Journal::new();
        assert_eq!(s.next_action(&j, &cfg(), 1000), SchedAction::Idle);
    }

    #[test]
    fn empty_journal_quiet_send_fires_immediately() {
        let mut s = Scheduler::default();
        s.add(rec("a", 0, false), "hi".into());
        let j = Journal::new(); // no keystroke → quiet immediately
        assert_eq!(s.next_action(&j, &cfg(), 0), SchedAction::Fire("a".into()));
    }

    #[test]
    fn nonempty_draft_holds_then_sleeps_to_the_quiet_deadline() {
        let mut s = Scheduler::default();
        s.add(rec("a", 1_000, false), "hi".into());
        let mut j = Journal::new();
        j.on_human_input(b"typing", 1_000, T); // last_keystroke = 1000
        // At t=1001, hold: deadline = 1000+3000 = 4000 → sleep 2999ms.
        assert_eq!(
            s.next_action(&j, &cfg(), 1_001),
            SchedAction::Sleep(2_999)
        );
        // At/after the quiet deadline → fire.
        assert_eq!(
            s.next_action(&j, &cfg(), 4_000),
            SchedAction::Fire("a".into())
        );
    }

    // ---- M5/T1: countdown-start durable draft snapshot ---------------------

    #[test]
    fn countdown_start_snapshots_draft_once_and_round_trips_durably() {
        // At the Accepted→Countdown transition the live driver snapshots the journal
        // draft into the durable record (phase=Countdown), exactly once, and it
        // survives a spool round-trip byte-exact.
        let mut s = Scheduler::default();
        s.add(rec("a", 1_000, false), "hi".into());
        let mut j = Journal::new();
        j.on_human_input(b"half-typed thought", 1_000, T); // recent keystroke → held
        // t=1_001: within the quiet window ⇒ Countdown ⇒ snapshot fires.
        let snaps = s.countdown_start_snapshots(&j, &cfg(), 1_001);
        assert_eq!(snaps.len(), 1, "one held send snapshotted at countdown-start");
        assert_eq!(snaps[0].phase, FirePhase::Countdown);
        assert_eq!(snaps[0].draft, b"half-typed thought", "draft captured byte-exact");
        // Durable round-trip: phase + draft survive a real spool write/load.
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::open(dir.path()).unwrap();
        spool.write(&snaps[0]).unwrap();
        let got = spool.load("a").unwrap().unwrap();
        assert_eq!(got.phase, FirePhase::Countdown);
        assert_eq!(got.draft, b"half-typed thought");
        // IDEMPOTENT: a later evaluation re-writes NOTHING (record past Accepted).
        assert!(
            s.countdown_start_snapshots(&j, &cfg(), 1_002).is_empty(),
            "no re-snapshot after the Accepted→Countdown transition"
        );
    }

    #[test]
    fn countdown_start_snapshots_skips_immediate_and_forced_sends() {
        // An IMMEDIATE send (empty/quiet journal) is not held ⇒ no countdown-start
        // snapshot (fire-start snapshots it instead — a countdown never armed).
        let mut s = Scheduler::default();
        s.add(rec("imm", 0, false), "hi".into());
        let j = Journal::new();
        assert!(s.countdown_start_snapshots(&j, &cfg(), 0).is_empty());
        // A FORCED (deliver-now) send fires immediately ⇒ no countdown to snapshot.
        let mut s2 = Scheduler::default();
        s2.add(rec("f", 1_000, false), "hi".into());
        let mut j2 = Journal::new();
        j2.on_human_input(b"typing", 1_000, T);
        s2.force(None);
        assert!(s2.countdown_start_snapshots(&j2, &cfg(), 1_100).is_empty());
    }

    #[test]
    fn deliver_now_forces_the_send_regardless_of_countdown() {
        let mut s = Scheduler::default();
        s.add(rec("a", 1_000, false), "hi".into());
        let mut j = Journal::new();
        j.on_human_input(b"still typing", 1_000, T);
        // Without forcing, it would hold (recent keystroke).
        assert!(matches!(
            s.next_action(&j, &cfg(), 1_100),
            SchedAction::Sleep(_)
        ));
        // Deliver-now forces it to fire immediately.
        s.force(None);
        assert_eq!(
            s.next_action(&j, &cfg(), 1_100),
            SchedAction::Fire("a".into())
        );
    }

    #[test]
    fn earliest_accepted_immediate_send_fires_first() {
        let mut s = Scheduler::default();
        // Two sends, both quiet-eligible immediately; the earliest-accepted fires.
        s.add(rec("first", 0, false), "1".into());
        s.add(rec("second", 5, false), "2".into());
        let j = Journal::new();
        assert_eq!(
            s.next_action(&j, &cfg(), 100),
            SchedAction::Fire("first".into())
        );
    }

    // ---- banner_countdown (the M2 read the banner renders) ------------------

    #[test]
    fn banner_countdown_is_none_when_idle_or_immediate() {
        let s = Scheduler::default();
        let j = Journal::new();
        assert_eq!(s.banner_countdown(&j, &cfg(), 0), None); // empty set
        // A send with an empty/quiet journal is Immediate → nothing to display.
        let mut s2 = Scheduler::default();
        s2.add(rec("a", 0, false), "hi".into());
        assert_eq!(s2.banner_countdown(&j, &cfg(), 0), None);
    }

    #[test]
    fn banner_countdown_reports_the_held_deadline_and_priority() {
        let mut s = Scheduler::default();
        s.add(rec("a", 1_000, true), "hi".into());
        let mut j = Journal::new();
        j.on_human_input(b"typing", 1_000, T);
        // Priority ceiling 10s; quiet deadline 1000+3000=4000 is earlier → deadline 4000.
        let cd = s.banner_countdown(&j, &cfg(), 1_001).expect("counting down");
        assert_eq!(cd.deadline_ms, 4_000);
        assert!(cd.priority, "priority flag surfaces to the banner");
    }

    #[test]
    fn banner_countdown_moves_with_a_keystroke_reset() {
        // "Countdown never stale": a fresh keystroke pushes the deadline out, and the banner read
        // reflects the NEW deadline — not the old one.
        let mut s = Scheduler::default();
        s.add(rec("a", 1_000, false), "hi".into());
        let mut j = Journal::new();
        j.on_human_input(b"a", 1_000, T);
        assert_eq!(s.banner_countdown(&j, &cfg(), 1_001).unwrap().deadline_ms, 4_000);
        // Keystroke at t=2000 → deadline moves to 2000+3000=5000.
        j.on_human_input(b"b", 2_000, T);
        assert_eq!(s.banner_countdown(&j, &cfg(), 2_001).unwrap().deadline_ms, 5_000);
    }

    #[test]
    fn banner_countdown_is_none_when_a_deliver_now_is_forced() {
        let mut s = Scheduler::default();
        s.add(rec("a", 1_000, false), "hi".into());
        let mut j = Journal::new();
        j.on_human_input(b"typing", 1_000, T);
        assert!(s.banner_countdown(&j, &cfg(), 1_100).is_some(), "held before force");
        s.force(None);
        assert_eq!(
            s.banner_countdown(&j, &cfg(), 1_100),
            None,
            "a forced deliver-now is about to fire → no countdown to display"
        );
    }

    #[test]
    fn take_removes_the_send_so_it_is_not_refired() {
        let mut s = Scheduler::default();
        s.add(rec("a", 0, false), "hi".into());
        assert!(s.take("a").is_some());
        let j = Journal::new();
        assert_eq!(s.next_action(&j, &cfg(), 100), SchedAction::Idle);
        assert!(s.take("a").is_none());
    }

    #[test]
    fn min_deadline_across_multiple_held_sends_is_the_sleep() {
        let mut s = Scheduler::default();
        // priority send has the shorter ceiling → its deadline is earlier.
        s.add(rec("normal", 0, false), "n".into());
        s.add(rec("prio", 0, true), "p".into());
        let mut j = Journal::new();
        // Continuous typing keeps both held; the sleep is to the EARLIEST deadline.
        j.on_human_input(b"typing a lot here", 0, T);
        // At t just after acceptance, the quiet deadline (0+3000) is earlier than
        // either ceiling, so both want ~3000; sleep to the min.
        match s.next_action(&j, &cfg(), 100) {
            SchedAction::Sleep(ms) => assert!(ms <= 2_900, "sleeps to the earliest deadline: {ms}"),
            other => panic!("expected sleep, got {other:?}"),
        }
    }

    // ---- path resolution ----------------------------------------------------

    #[test]
    fn ledger_path_uses_sessionid_then_byname_fallback() {
        let base = std::path::Path::new("/state");
        assert_eq!(
            ledger_path(base, Some("uuid-123"), "alpha"),
            std::path::Path::new("/state/sessions/uuid-123.events.jsonl")
        );
        // No sessionId → byname- key (cannot collide with a uuid).
        assert_eq!(
            ledger_path(base, None, "alpha"),
            std::path::Path::new("/state/sessions/byname-alpha.events.jsonl")
        );
        assert_eq!(
            ledger_path(base, Some(""), "alpha"),
            std::path::Path::new("/state/sessions/byname-alpha.events.jsonl")
        );
    }

    #[test]
    fn status_source_unknown_harness_reads_none() {
        // No sessions_dir (unknown harness) ⇒ None ⇒ the fire honestly fails
        // not-accepted, never blind-types.
        let ss = StatusSource {
            sessions_dir: None,
            name: "x".into(),
            confirmable_acceptance: false,
        };
        assert_eq!(ss.read(), None);
    }

    // ---- M4: per-harness selection ---------------------------------------

    #[test]
    fn harness_from_argv0_classifies_tightly() {
        assert_eq!(Harness::from_argv0(Some("/home/u/.local/bin/codex")), Harness::Codex);
        assert_eq!(Harness::from_argv0(Some("codex")), Harness::Codex);
        assert_eq!(Harness::from_argv0(Some("/home/u/.local/bin/pi")), Harness::Pi);
        assert_eq!(Harness::from_argv0(Some("pi")), Harness::Pi);
        // Tight match: a pi look-alike is NOT pi.
        assert_eq!(Harness::from_argv0(Some("pip")), Harness::Default);
        assert_eq!(Harness::from_argv0(Some("pipx")), Harness::Default);
        // claude + shell/unknown + None ⇒ Default (safe, no enablement).
        assert_eq!(Harness::from_argv0(Some("/usr/bin/claude")), Harness::Default);
        assert_eq!(Harness::from_argv0(Some("/bin/bash")), Harness::Default);
        assert_eq!(Harness::from_argv0(None), Harness::Default);
    }

    #[test]
    fn from_command_classifies_login_shell_launches() {
        // M5/T5: the CreateDetached login-shell shape — argv0 is always `bash`, the
        // real binary hides in the `command '<bin>' …` cmd. from_command parses it.
        let ls = |cmd: &str| {
            vec!["bash".to_string(), "-lc".to_string(), cmd.to_string()]
        };
        // codex / pi launches classify (this is the reachability un-gate).
        assert_eq!(
            Harness::from_command(&ls("command '/home/u/.local/bin/codex' '--flag' 'v'")),
            Harness::Codex
        );
        assert_eq!(Harness::from_command(&ls("command 'codex'")), Harness::Codex);
        assert_eq!(
            Harness::from_command(&ls("command '/home/u/.local/bin/pi' '-m' 'x'")),
            Harness::Pi
        );
        assert_eq!(Harness::from_command(&ls("command 'pi'")), Harness::Pi);
        // A self-deleting dot-source env prefix before the launch is stripped.
        assert_eq!(
            Harness::from_command(&ls(". '/x/qd-session-env-1'; rm -f '/x/qd-session-env-1'; command 'pi' '-m'")),
            Harness::Pi
        );
        // A `;` inside a LATER single-quoted flag value never corrupts the bin.
        assert_eq!(
            Harness::from_command(&ls("command 'codex' '--sys' 'a; rm -rf /'")),
            Harness::Codex
        );
        // claude / unknown / unparseable ⇒ Default (byte-for-byte accepted path).
        assert_eq!(Harness::from_command(&ls("command '/usr/bin/claude' '--foo'")), Harness::Default);
        assert_eq!(Harness::from_command(&ls("command 'fakerepl'")), Harness::Default);
        assert_eq!(Harness::from_command(&ls("echo not-a-launch")), Harness::Default);
        assert_eq!(Harness::from_command(&ls("command 'pip'")), Harness::Default); // pi look-alike
        // A DIRECT (non-login-shell) program still classifies by argv0.
        assert_eq!(Harness::from_command(&["codex".to_string()]), Harness::Codex);
        assert_eq!(Harness::from_command(&["/bin/bash".to_string()]), Harness::Default);
        // Attach-to-shell (bash with no -lc launch) ⇒ Default.
        assert_eq!(Harness::from_command(&["bash".to_string()]), Harness::Default);
    }

    #[test]
    fn codex_pi_have_no_landed_status_source_default_is_claude() {
        // codex/pi: no landed busy/idle source (Q7 residual) ⇒ none_source ⇒
        // read() None ⇒ honest not-accepted (deferred). NOT the claude registry.
        assert_eq!(Harness::Codex.status_source("x").sessions_dir, None);
        assert_eq!(Harness::Pi.status_source("x").sessions_dir, None);
        // Default rides the claude-shaped registry source.
        assert!(Harness::Default.status_source("x").sessions_dir.is_some());
    }

    #[test]
    fn toast_kind_for_routes_through_success_terminal() {
        // The success terminal ⇒ Delivered; every other terminal ⇒ the recovery
        // notice; a non-terminal ⇒ nothing (defensive). No local "message-seen".
        assert!(matches!(toast_kind_for("message-seen"), Some(ToastKind::Delivered)));
        for t in ["send-failed", "seen-failed", "pending-abandoned", "turn-anchored-mismatch"] {
            assert!(
                matches!(toast_kind_for(t), Some(ToastKind::Failed { .. })),
                "`{t}` is a recovery-notice terminal"
            );
        }
        assert_eq!(toast_kind_for("chunks-delivered"), None, "non-terminal raises nothing");
    }

    #[test]
    fn status_source_reads_matching_row_status() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("100.json"),
            br#"{"pid":100,"name":"alpha","status":"busy"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("200.json"),
            br#"{"pid":200,"name":"other","status":"idle"}"#,
        )
        .unwrap();
        let ss = StatusSource {
            sessions_dir: Some(dir.path().to_path_buf()),
            name: "alpha".into(),
            confirmable_acceptance: true,
        };
        assert_eq!(ss.read().as_deref(), Some("busy"));
    }

    // ---- F2 (QS-1): keystroke-duplication race — journal snapshot ↔ input lock --

    #[test]
    fn racing_keystroke_is_never_duplicated_in_the_restored_draft() {
        // Models the reported cross-mutex interleave (journal-append ↔ lock-arm ↔
        // admit ↔ snapshot): a fire has ARMED the input lock and is about to snapshot
        // the draft; simultaneously a human keystroke "X" arrives. The restored draft
        // after the fire (replayed snapshot ++ flushed buffer) must contain "X"
        // EXACTLY ONCE — never duplicated (the F2 defect), never lost.
        //
        // Deterministic, no sleep-as-sync: the fire OWNS the atomic section by holding
        // the input-lock guard + arming (exactly what `lock_and_snapshot` does before
        // it snapshots). The racing keystroke runs the REAL `journal_and_admit` on
        // another thread. The fix makes that path take the input lock FIRST, so it
        // provably CANNOT journal "X" while the section is held (the poll stays "ab"
        // until timeout, then the fire snapshots "ab" and "X" is buffered → flushed
        // once). WITHOUT the fix, `journal_and_admit` journals "X" with no input lock,
        // the poll sees "abX", "X" leaks into the snapshot AND is buffered → the drain
        // duplicates it. FAILS on the pre-fix code, PASSES after.
        let lock = Arc::new(Mutex::new(InputLock::new()));
        let journal = Arc::new(Mutex::new(Journal::new()));
        // Pre-fire draft "ab" (typed + admitted passthrough before the fire).
        assert_eq!(
            journal_and_admit(&lock, &journal, b"ab", 100, T),
            crate::attended::Admit::Passthrough(b"ab".to_vec())
        );

        // FIRE enters its atomic arm+snapshot section.
        let mut fire_guard = lock.lock().unwrap();
        fire_guard.lock(); // arm

        // The racing keystroke "X" via the REAL driver path, on another thread.
        let (lk, jr) = (lock.clone(), journal.clone());
        let driver = std::thread::spawn(move || journal_and_admit(&lk, &jr, b"X", 200, T));

        // Poll the draft while holding the section. FIXED: stays "ab" (the keystroke
        // cannot journal until we release). BUGGY: becomes "abX". Bounded so the
        // fixed path can't hang.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        while journal.lock().unwrap().draft() == b"ab" && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }

        // The fire snapshots the draft (still inside the section).
        let snapshot = journal.lock().unwrap().snapshot();

        // Leave the section: release the input-lock guard. The arm flag persists —
        // mirroring the real fire holding the lock armed through inject.
        drop(fire_guard);

        // The keystroke now admits "X" (armed ⇒ Buffered), never lost.
        let admit = driver.join().unwrap();
        assert_eq!(
            admit,
            crate::attended::Admit::Buffered,
            "racing keystroke buffers during the fire (never enters the PTY mid-fire)"
        );

        // The fire unlocks + drains the buffer in order (fire.rs step 7).
        let flushed = match lock.lock() {
            Ok(mut l) => l.unlock_and_drain(),
            Err(p) => p.into_inner().unlock_and_drain(),
        };

        // The restored draft the human sees = replayed snapshot ++ flushed buffer.
        let mut restored = snapshot.clone();
        restored.extend_from_slice(&flushed);

        let x_count = restored.iter().filter(|&&b| b == b'X').count();
        assert_eq!(
            x_count, 1,
            "the racing keystroke must survive EXACTLY once (no F2 duplication, no \
             loss): snapshot={snapshot:?} flushed={flushed:?} restored={restored:?}"
        );
        assert_eq!(restored, b"abX", "restoration is byte-exact and in order");
    }

    // ==== M5 ADV-R1 F1 (QS-1) REGRESSION — the PTY WRITE STREAM (the blind spot) ===
    //
    // The existing test above models the IN-MEMORY reconstruction (`snapshot ++
    // flushed`); it never models the raw PTY WRITE STREAM where the deferred
    // Passthrough write lands. adv-r1 F1 lived exactly there: the relay wrote the
    // Passthrough bytes on a separate, unsequenced `spawn_blocking` AFTER
    // `on_human_input` returned (input lock released), so under load that write could
    // land AFTER the fire's clear-chord — duplicating the keystroke / entering the PTY
    // mid-fire. These tests assert M1's own acceptance metric (fix1-executor-
    // handoff.md:59-61) DIRECTLY on the raw writes: the human keystroke appears
    // EXACTLY ONCE after the last clear-chord, across BOTH orderings, with the fix's
    // mechanism (the passthrough write runs UNDER the input lock) exercised.

    /// A [`FireEffects`] that records the raw PTY WRITE STREAM in order. Injected
    /// discipline text (`send_text`/`send_cr`) is a submitted turn, NOT raw human
    /// bytes, so it is logged to `order` only and EXCLUDED from `raws` — exactly M1's
    /// metric (count the human byte in the raw writes after the last clear). `Arc`-
    /// shared so a concurrently-admitted passthrough write records into the SAME stream.
    struct StreamFx {
        screen: String,
        raws: Arc<Mutex<Vec<Vec<u8>>>>,
        order: Arc<Mutex<Vec<String>>>,
        clock: std::sync::atomic::AtomicI64,
    }
    impl FireEffects for StreamFx {
        fn send_text(&self, t: &str) {
            self.order.lock().unwrap().push(format!("inject-text:{t}"));
        }
        fn send_cr(&self) {
            self.order.lock().unwrap().push("inject-cr".into());
        }
        fn write_raw(&self, b: &[u8]) -> std::io::Result<()> {
            self.raws.lock().unwrap().push(b.to_vec());
            self.order
                .lock()
                .unwrap()
                .push(format!("raw:{}", String::from_utf8_lossy(b)));
            Ok(())
        }
        fn read_screen(&self) -> String {
            self.screen.clone()
        }
        fn read_status(&self) -> Option<String> {
            Some("busy".into())
        }
        fn acceptance_confirmable(&self) -> bool {
            true
        }
        fn sleep(&self, _ms: u64) {}
        fn now_ms(&self) -> i64 {
            self.clock.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        }
    }
    struct AlwaysLanded;
    impl LandingProbe for AlwaysLanded {
        fn scan(
            &self,
            _t: Option<&str>,
            _o: Option<u64>,
            _m: &str,
        ) -> crate::attended::fire::LandingScan {
            crate::attended::fire::LandingScan::Landed
        }
    }

    /// Count `byte` in the raw writes AFTER the last clear-chord (`0x15`) — the
    /// restored composer (M1's metric). Panics if no clear-chord is present.
    fn count_raw_after_last_clear(raws: &[Vec<u8>], byte: u8) -> usize {
        let last_clear = raws
            .iter()
            .rposition(|w| w.as_slice() == [0x15u8])
            .expect("a clear-chord must be present in the raw stream");
        raws[last_clear + 1..]
            .concat()
            .iter()
            .filter(|&&b| b == byte)
            .count()
    }

    #[test]
    fn qs1_write_stream_metric_flags_a_post_clear_duplicate_control() {
        // NEGATIVE CONTROL: the metric is NOT vacuous — it flags the pre-fix defect
        // shape (the deferred Passthrough write landing post-clear AND the snapshot
        // replay → 'K' twice). This is the exact 2× the red-team probe produced on
        // 9b02353 (`k_count` left:2, right:1). The regression guards below then prove
        // the fix drives it to 1.
        let raws = vec![b"\x15".to_vec(), b"K".to_vec(), b"draftK".to_vec()];
        assert_eq!(
            count_raw_after_last_clear(&raws, b'K'),
            2,
            "the metric detects a post-clear duplicate (the pre-fix defect shape)"
        );
    }

    #[test]
    fn qs1_passthrough_write_is_serialized_under_the_input_lock_and_lands_pre_clear() {
        // adv-r1 F1 (QS-1), admit-before-arm (Passthrough) ordering AT THE WORST TIME:
        // a human keystroke 'K' is admitted Passthrough (input lock UNARMED) and a
        // fire tries to arm+clear CONCURRENTLY. The FIX performs the passthrough write
        // inside `journal_admit_passthrough` WHILE HOLDING the input lock, so the
        // fire's `lock_and_snapshot` (which takes the SAME lock) CANNOT arm+clear until
        // the write has landed — the write is provably pre-clear, so after the fire's
        // clear the keystroke appears in the PTY stream EXACTLY once (the replay).
        //
        // Pre-fix, the write ran on a separate spawn_blocking with the input lock
        // ALREADY released (nothing serialized it against the fire) — it could land
        // post-clear, giving 'K' twice (the control above / the probe on 9b02353).
        let lock = Mutex::new(InputLock::new());
        let journal = Mutex::new(Journal::new());
        journal.lock().unwrap().on_human_input(b"draft", 0, T); // pre-typed draft

        let raws = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let saw_lock_held = std::sync::atomic::AtomicBool::new(false);

        // The racing keystroke 'K' via the REAL fixed input path. The write closure
        // runs UNDER the input-lock guard held by `journal_admit_passthrough`. A
        // re-entrant `try_lock` from THIS thread fails iff that guard is held — which
        // is the whole mechanism: `fire`'s `lock_and_snapshot` acquires the SAME lock
        // to arm+snapshot, so it PROVABLY cannot arm+clear while this write runs. The
        // post-clear landing (adv-r1 F1) is therefore UNREACHABLE, not merely unlikely.
        // Pre-fix, the relay wrote 'K' on a separate spawn_blocking with the lock
        // already released (nothing held it) — `try_lock` there would succeed.
        {
            let raws_w = raws.clone();
            let order_w = order.clone();
            let admit = journal_admit_passthrough(&lock, &journal, b"K", 1, T, |b| {
                if lock.try_lock().is_err() {
                    saw_lock_held.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                raws_w.lock().unwrap().push(b.to_vec());
                order_w
                    .lock()
                    .unwrap()
                    .push("raw-passthrough(under-lock):K".into());
                Ok(())
            })
            .unwrap();
            assert_eq!(
                admit,
                crate::attended::Admit::Passthrough(b"K".to_vec()),
                "unarmed ⇒ Passthrough (the direct write 'K' the input path owes)"
            );
        }
        assert!(
            saw_lock_held.load(std::sync::atomic::Ordering::SeqCst),
            "the passthrough write MUST run while the input lock is held (the mechanism \
             that serializes it before any fire arm+clear); pre-fix it ran lock-free"
        );
        // The journal now holds "draftK" — the fire's snapshot WILL include 'K' and
        // replay it; the pre-clear passthrough 'K' will be wiped by the clear-chord.
        assert_eq!(journal.lock().unwrap().draft(), b"draftK");

        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::open(dir.path().join("pending")).unwrap();
        let r = PendingRecord::accepted(
            "s1", "sha", 3, Some("sid".into()), Some("alpha".into()), "send:pty", false, 1,
        );
        let glyph = quorum_submit_discipline::PROMPT_GLYPH;
        let fx = StreamFx {
            screen: format!("{glyph} msg"),
            raws: raws.clone(),
            order: order.clone(),
            clock: std::sync::atomic::AtomicI64::new(10),
        };
        let out = fire(
            &fx,
            &SafeDefaultFacts,
            &AlwaysLanded,
            &lock,
            &journal,
            &spool,
            r,
            "msg",
            &FireConfig::default(),
        );
        assert!(
            matches!(out, FireOutcome::Terminal(_)),
            "fire resolved to a terminal: {out:?}"
        );

        let raws = raws.lock().unwrap().clone();
        let last_clear = raws
            .iter()
            .rposition(|w| w.as_slice() == [0x15u8])
            .expect("clear-chord present");
        let k_idx = raws
            .iter()
            .position(|w| w.as_slice() == b"K")
            .expect("the passthrough 'K' write is present");
        assert!(
            k_idx < last_clear,
            "the passthrough 'K' landed PRE-clear (wiped by the clear, replayed once): \
             order={:?}",
            order.lock().unwrap()
        );
        assert_eq!(
            count_raw_after_last_clear(&raws, b'K'),
            1,
            "QS-1: the human keystroke 'K' reaches the PTY EXACTLY once after the fire's \
             clear (never duplicated / mid-fire): order={:?}",
            order.lock().unwrap()
        );
    }

    #[test]
    fn qs1_buffered_keystroke_during_clear_flushes_exactly_once_in_pty_stream() {
        // adv-r1 F1 (QS-1), arm-before-admit (Buffered) ordering AT THE WORST TIME: a
        // keystroke 'K' arrives exactly as the fire writes its clear-chord (the fire
        // has already armed + snapshotted). It is admitted Buffered ⇒ NO passthrough
        // write now; it is buffered and flushed in order on unlock. Asserted on the
        // raw PTY WRITE STREAM: 'K' reaches the PTY EXACTLY once (the post-unlock
        // flush), never mid-fire, never duplicated — the fix does not regress M1's
        // fires-while-typing guarantee.
        let lock = Arc::new(Mutex::new(InputLock::new()));
        let journal = Arc::new(Mutex::new(Journal::new()));
        journal.lock().unwrap().on_human_input(b"draft", 0, T);

        let raws = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let order = Arc::new(Mutex::new(Vec::<String>::new()));

        /// A [`FireEffects`] that, on the clear-chord (its first `write_raw`), drives a
        /// racing keystroke 'K' through the REAL input path — modeling a keystroke at
        /// the exact worst moment. The fire has ALREADY armed, so
        /// `journal_admit_passthrough` admits it Buffered and its write closure (which
        /// would be the mid-fire BUG) is NEVER called.
        struct ClearRaceFx {
            screen: String,
            raws: Arc<Mutex<Vec<Vec<u8>>>>,
            order: Arc<Mutex<Vec<String>>>,
            clock: std::sync::atomic::AtomicI64,
            fired: std::sync::atomic::AtomicBool,
            lock: Arc<Mutex<InputLock>>,
            journal: Arc<Mutex<Journal>>,
            admit_seen: Mutex<Option<crate::attended::Admit>>,
        }
        impl FireEffects for ClearRaceFx {
            fn send_text(&self, t: &str) {
                self.order.lock().unwrap().push(format!("inject-text:{t}"));
            }
            fn send_cr(&self) {
                self.order.lock().unwrap().push("inject-cr".into());
            }
            fn write_raw(&self, b: &[u8]) -> std::io::Result<()> {
                self.raws.lock().unwrap().push(b.to_vec());
                self.order
                    .lock()
                    .unwrap()
                    .push(format!("raw:{}", String::from_utf8_lossy(b)));
                if !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    // The keystroke arrives DURING the clear (the fire is armed).
                    let raws2 = self.raws.clone();
                    let order2 = self.order.clone();
                    let admit =
                        journal_admit_passthrough(&self.lock, &self.journal, b"K", 5, T, |bytes| {
                            // UNREACHABLE if the fix holds: a Buffered admit writes nothing.
                            raws2.lock().unwrap().push(bytes.to_vec());
                            order2
                                .lock()
                                .unwrap()
                                .push("raw-PASSTHROUGH(mid-fire-BUG):K".into());
                            Ok(())
                        })
                        .unwrap();
                    *self.admit_seen.lock().unwrap() = Some(admit);
                }
                Ok(())
            }
            fn read_screen(&self) -> String {
                self.screen.clone()
            }
            fn read_status(&self) -> Option<String> {
                Some("busy".into())
            }
            fn acceptance_confirmable(&self) -> bool {
                true
            }
            fn sleep(&self, _ms: u64) {}
            fn now_ms(&self) -> i64 {
                self.clock.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::open(dir.path().join("pending")).unwrap();
        let r = PendingRecord::accepted(
            "s1", "sha", 3, Some("sid".into()), Some("alpha".into()), "send:pty", false, 1,
        );
        let glyph = quorum_submit_discipline::PROMPT_GLYPH;
        let fx = ClearRaceFx {
            screen: format!("{glyph} msg"),
            raws: raws.clone(),
            order: order.clone(),
            clock: std::sync::atomic::AtomicI64::new(10),
            fired: std::sync::atomic::AtomicBool::new(false),
            lock: lock.clone(),
            journal: journal.clone(),
            admit_seen: Mutex::new(None),
        };
        let out = fire(
            &fx,
            &SafeDefaultFacts,
            &AlwaysLanded,
            &lock,
            &journal,
            &spool,
            r,
            "msg",
            &FireConfig::default(),
        );
        assert!(
            matches!(out, FireOutcome::Terminal(_)),
            "fire resolved to a terminal: {out:?}"
        );
        assert_eq!(
            *fx.admit_seen.lock().unwrap(),
            Some(crate::attended::Admit::Buffered),
            "a keystroke arriving mid-fire (armed) is admitted Buffered — written NOTHING now"
        );
        let raws = raws.lock().unwrap().clone();
        assert!(
            !order
                .lock()
                .unwrap()
                .iter()
                .any(|o| o.contains("mid-fire-BUG")),
            "no passthrough write happened mid-fire (the buffered path wrote nothing): {:?}",
            order.lock().unwrap()
        );
        assert_eq!(
            count_raw_after_last_clear(&raws, b'K'),
            1,
            "QS-1: the buffered keystroke 'K' reaches the PTY EXACTLY once (the \
             post-unlock flush), never mid-fire, never duplicated: order={:?}",
            order.lock().unwrap()
        );
    }
}
