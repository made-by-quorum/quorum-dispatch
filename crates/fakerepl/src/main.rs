//! `fakerepl` — a deterministic fake claude REPL for the A4 submit-discipline
//! gate (a4-spec §5). A test harness, NEVER shipped (`publish = false`).
//!
//! It emulates claude's externally-observable boot/submit contract closely
//! enough to exercise the M1 submit discipline (`verify_accepted_then_cr` /
//! `deliver_prompt`) over a REAL PTY with REAL timing — but with ZERO RNG and no
//! wall-clock-keyed decisions beyond the single 50ms burst-gap constant. Load
//! variation comes from the HARNESS varying env knobs per iteration, never from
//! anything random inside this binary.
//!
//! # Contract surface (the SUT reads these)
//!
//! - **Registry row.** On start (after the jail belt passes) it writes
//!   `$HOME/.claude/sessions/<pid>.json` with the shape `boot::read_pid_status`
//!   parses: `{"pid": <pid>, "status": "idle", "name": "<name>"}`. Status
//!   transitions rewrite the file atomically (tmp + rename). The row is removed
//!   on clean exit (SIGTERM / stdin EOF).
//! - **Application output.** On submit it prints, to its PTY stdout,
//!   `[turn <n>] accepted bytes=<composer_len> composer_crs=<k>`; after the busy
//!   hold it prints `[turn <n>] done`. These are the gate's ONLY turn-count
//!   oracle (ADD-6: never echo — the harness parses these app-output lines, not
//!   any echoed input).
//!
//! # Burst model (a4-spec §5, deterministic)
//!
//! stdin chunks separated by < `GAP_MS` (50ms) belong to ONE burst. A burst is a
//! PASTE iff its total length ≥ `QD_FAKEREPL_PASTE_THRESHOLD` (default 8). A CR
//! INSIDE a paste burst is absorbed as a literal newline into the composer
//! (claude's paste-burst behavior); a CR arriving as its OWN non-paste burst
//! SUBMITS the composer. `QD_FAKEREPL_ABSORB_ALL_CRS=1` makes EVERY CR absorbed
//! (powers the stalled exit-contract row + the W8 control). A CR arriving while
//! BUSY is recorded (`cr_while_busy`), composer-buffered, and does NOT start a
//! turn (matches claude: queued input).
//!
//! # Jail refusal (a4-spec §5, redesigned per spec-red-team R3)
//!
//! Refuse (stderr naming the failed check + exit 13) unless ALL hold: (a) `HOME`
//! matches `*/qdrg-runs/*/home`; (b) with `root := dirname(HOME)`,
//! `QD_HOME == root/qd_home`, `ZMX_DIR == root/zmx`, `TMPDIR == root/tmp`. NO
//! dependence on `JAIL_ROOT`/`JAIL_RUNID`/`JAIL_PREFIX` — those are shell-local in
//! jail.sh (NO export), so a child across the zmx boundary never sees them. The
//! belt is derived purely from the EXPORTED isolation set (jail.sh:139-146).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::{Duration, Instant};

mod jail;
mod report;

use report::Reporter;

/// The registry-row path as a NUL-terminated C string, published for the SIGTERM
/// handler to `unlink(2)` in an async-signal-safe way. Null until the row is
/// written; leaked on purpose (lives for the whole process).
static PID_FILE_C: AtomicPtr<libc::c_char> = AtomicPtr::new(std::ptr::null_mut());

/// SIGTERM handler: unlink the registry row (async-signal-safe via raw
/// `unlink(2)`), then `_exit(0)`. `qd kill` / harness teardown SIGTERMs the
/// child; without this the row would linger and a later scan would see a phantom
/// session. We do NOT touch the report sink here (not async-signal-safe); the
/// harness reads the report after the child is gone and tolerates a missing
/// trailing flush (each line is already flushed on write).
extern "C" fn on_sigterm(_sig: libc::c_int) {
    let p = PID_FILE_C.load(Ordering::SeqCst);
    if !p.is_null() {
        // SAFETY: p is a valid NUL-terminated C string for the process lifetime
        // (leaked); unlink is async-signal-safe.
        unsafe {
            libc::unlink(p);
        }
    }
    // SAFETY: _exit is async-signal-safe.
    unsafe { libc::_exit(0) };
}

/// The burst-gap constant: stdin chunks arriving < this apart coalesce into one
/// burst. MEASURED (not assumed) against portable-pty — see crates/fakerepl/
/// README.md and the `coalescing_note` integration test.
const GAP_MS: u64 = 50;

/// Default paste threshold: a burst ≥ this many bytes is a PASTE (its CRs are
/// absorbed, not submits). Overridable via `QD_FAKEREPL_PASTE_THRESHOLD`.
const DEFAULT_PASTE_THRESHOLD: usize = 8;

/// Default busy-hold after a submit, ms. Overridable via `QD_FAKEREPL_BUSY_MS`.
const DEFAULT_BUSY_MS: u64 = 500;

fn main() {
    // ---- Jail refusal FIRST (before any fs writes / registry row). ----------
    if let Err(reason) = jail::assert_jailed_env() {
        eprintln!("fakerepl: REFUSED — {reason}");
        std::process::exit(13);
    }

    // Put stdin (the slave PTY) into RAW mode so reads return per-burst instead
    // of blocking for a line terminator, and so no ICRNL/ECHO line discipline
    // mangles the byte stream. This mirrors what claude's TUI does — and is
    // LOAD-BEARING for the burst model: in the default canonical mode a read()
    // blocks until a newline, so a paste with no trailing \n would never arrive.
    set_stdin_raw();

    let cfg = Config::from_env();
    // ACK-1: EAT_INPUT and TRUNCATE compose with nothing — both set is a
    // harness error, refused loudly (ack1-spec §4.2; no silent precedence).
    if cfg.eat_input && cfg.truncate_user_record_bytes.is_some() {
        eprintln!(
            "fakerepl: REFUSED — QD_FAKEREPL_EAT_INPUT and \
             QD_FAKEREPL_TRUNCATE_USER_RECORD_BYTES are mutually exclusive"
        );
        std::process::exit(13);
    }
    let mut repl = match Repl::start(cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fakerepl: startup failed: {e}");
            std::process::exit(1);
        }
    };
    repl.run();
    repl.shutdown();
}

/// Knobs read once at start; all from env (no flags except `--name`).
struct Config {
    name: String,
    paste_threshold: usize,
    busy_ms: u64,
    absorb_all_crs: bool,
    /// tty-queue OVERFLOW model (ADR 0009 mode (a), A4 follow-up). When set, a
    /// SINGLE burst whose length EXCEEDS this many bytes is DROPPED WHOLESALE — no
    /// byte reaches the composer, a `drop` report event is recorded, and no turn
    /// starts. This MODELS THE CLASS (the live ~4096B canonical-tty-queue overflow:
    /// test/golden/dryrun/a4-r6-probe-evidence.md), NOT a specific machine boundary
    /// — the live drop boundary (8KB clean / ≥12KB dropped on brano; TS ~4KB) is
    /// MACHINE/LOAD-DEPENDENT. 4096 is a representative model default for the
    /// negative-control pairing; the INVARIANT the gate proves is the ≤1024B chunk
    /// size, which passes UNDER any realistic queue bound. `None` (unset) → no drop.
    drop_over_bytes: Option<usize>,
    /// W8 reader-stall / saturation seam (models the D16 reader-stall window).
    /// Once cumulative INPUT bytes first reach `stall_after_bytes`, the reader
    /// "pauses" for `stall_ms`; while paused it admits at most `stall_queue_cap`
    /// bytes to the composer (counted from the stall trigger) and DROPS the rest
    /// (a `stall_drop` report event). All three must be set to arm the seam.
    stall_after_bytes: Option<usize>,
    stall_ms: u64,
    stall_queue_cap: usize,
    /// W8 conversation-JSONL emulation: when set, every submit appends a
    /// claude-shaped user record and every turn-done appends a stub assistant
    /// end_turn record (giving the SUT's verify step a transcript to read).
    convo_jsonl: Option<PathBuf>,
    /// W8 end-to-end leg: when set, the registry row carries this `sessionId`
    /// so the SUT's registry→sessionId→find_jsonl_path resolution chain works
    /// against the fakerepl (the scenario places the convo JSONL at the
    /// projects-dir path this id resolves to). Unset → row unchanged.
    session_id: Option<String>,
    /// ACK-1 injection-4 seam (ack1-spec §4.2): consume ALL stdin bytes —
    /// they leave the PTY queue and are COUNTED (an `eaten` report event per
    /// burst) — but NOTHING reaches the composer: no burst classification,
    /// no submit, no turn line, no user record. "Bytes demonstrably consumed,
    /// no anchor." Wins over the burst model entirely. Conflicts with
    /// `truncate_user_record_bytes` (refused at startup, exit 13).
    eat_input: bool,
    /// ACK-1 injection-5 seam (ack1-spec §4.2): on submit, the convo-JSONL
    /// user record carries a PREFIX of the composer's RAW BYTES — cut at n,
    /// rounded DOWN to the nearest UTF-8 boundary (never panics, never
    /// injects U+FFFD into the kept prefix). A
    /// `truncated_user_record {requested, actual}` report event records the
    /// real cut. The turn proceeds normally otherwise (app-output line and
    /// `bytes=` count are the FULL composer).
    truncate_user_record_bytes: Option<usize>,
    sessions_dir: PathBuf,
    report_path: Option<PathBuf>,
    /// M3 e2e delivery proof (opt-in, DEFAULT-OFF; `QD_FAKEREPL_COMPOSER_MODE=1`).
    /// Renders a faithful-er claude composer so the attended mux fire's discipline
    /// is exercised end-to-end: (1) prints the `❯` prompt glyph on idle (so the
    /// mux's `SafeDefaultFacts::composer_is_plain` verify passes — a plain composer
    /// is present); (2) treats the clear-chord `Ctrl-U` (0x15) as line-discard (so
    /// the injected turn is the clean payload, not `\x15`-polluted — the LandingProbe
    /// then matches Landed, not Mismatch); (3) echoes composer bytes to the PTY (so
    /// the marker renders in history). Existing gates do NOT set it → their behavior
    /// is byte-identical.
    composer_mode: bool,
}

impl Config {
    fn from_env() -> Self {
        // --name <n>  (the only flag); falls back to QD_FAKEREPL_NAME, then a
        // deterministic default. This is the registry row's `name` the SUT's
        // find_pid_file keys on.
        let mut name: Option<String> = None;
        let mut resume_id: Option<String> = None;
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            if a == "--name" {
                name = args.next();
            } else if a == "--resume" {
                // WP-B5-iii: faithful claude emulation — `--resume <id>` continues
                // session <id>, so the registry row's sessionId is <id> unless a
                // test pins QD_FAKEREPL_SESSION_ID explicitly. This lets a
                // Mechanism-S fork (whose uuid qd mints PRE-spawn and passes via
                // `--resume <fork_uuid>`) register that uuid without the test
                // pre-knowing it.
                resume_id = args.next();
            }
        }
        let name = name
            .or_else(|| std::env::var("QD_FAKEREPL_NAME").ok())
            .unwrap_or_else(|| "fakerepl".to_string());

        let paste_threshold = std::env::var("QD_FAKEREPL_PASTE_THRESHOLD")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(DEFAULT_PASTE_THRESHOLD);

        let busy_ms = std::env::var("QD_FAKEREPL_BUSY_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_BUSY_MS);

        let absorb_all_crs = std::env::var("QD_FAKEREPL_ABSORB_ALL_CRS")
            .map(|v| v == "1")
            .unwrap_or(false);

        // tty-queue overflow model: a single burst LONGER than this many bytes is
        // dropped wholesale (models the live ~4096B canonical-tty-queue overflow,
        // ADR 0009 mode (a)). Unset → no drop.
        let drop_over_bytes = std::env::var("QD_FAKEREPL_DROP_OVER_BYTES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok());

        // W8 reader-stall seam. Armed only when STALL_AFTER_BYTES is set; the
        // other two carry sensible defaults so a partial config still models a
        // stall (cap 0 = total mid-loss, ms 0 = no-op pause).
        let stall_after_bytes = std::env::var("QD_FAKEREPL_STALL_AFTER_BYTES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok());
        let stall_ms = std::env::var("QD_FAKEREPL_STALL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let stall_queue_cap = std::env::var("QD_FAKEREPL_STALL_QUEUE_CAP")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);

        // W8 conversation-JSONL emulation path (None = unset → no transcript).
        let convo_jsonl = std::env::var_os("QD_FAKEREPL_CONVO_JSONL").map(PathBuf::from);

        // W8 end-to-end leg: optional sessionId for the registry row.
        // QD_FAKEREPL_SESSION_ID pins it for the env-driven tests; absent, fall
        // back to the `--resume <id>` argv (WP-B5-iii — real claude continues that
        // session id) so a Mechanism-S fork adopts its seeded uuid.
        let session_id = std::env::var("QD_FAKEREPL_SESSION_ID").ok().or(resume_id);

        // ACK-1 seams (ack1-spec §4.2). The both-set conflict is refused in
        // main() (exit 13 — fail-loud, no silent precedence).
        let eat_input = std::env::var("QD_FAKEREPL_EAT_INPUT")
            .map(|v| v == "1")
            .unwrap_or(false);
        let truncate_user_record_bytes = std::env::var("QD_FAKEREPL_TRUNCATE_USER_RECORD_BYTES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok());

        // Registry dir = $HOME/.claude/sessions (the jail HOME). HOME is
        // guaranteed jail-shaped by the belt that already ran.
        let home = std::env::var("HOME").expect("HOME present (belt passed)");
        let sessions_dir = Path::new(&home).join(".claude").join("sessions");

        let report_path = std::env::var_os("QD_FAKEREPL_REPORT").map(PathBuf::from);

        // M3 e2e composer emulation (opt-in, default-off).
        let composer_mode = std::env::var("QD_FAKEREPL_COMPOSER_MODE")
            .map(|v| v == "1")
            .unwrap_or(false);

        Self {
            name,
            paste_threshold,
            busy_ms,
            absorb_all_crs,
            drop_over_bytes,
            stall_after_bytes,
            stall_ms,
            stall_queue_cap,
            convo_jsonl,
            session_id,
            eat_input,
            truncate_user_record_bytes,
            sessions_dir,
            report_path,
            composer_mode,
        }
    }
}

/// Session status as it appears in the registry row (`read_pid_status` reads
/// exactly this string).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Idle,
    Busy,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Busy => "busy",
        }
    }
}

struct Repl {
    cfg: Config,
    pid: i64,
    pid_file: PathBuf,
    /// Boot instant, epoch ms — stamped once and written as the row's
    /// `startedAt` (fidelity: real Claude stamps its boot; the engine's stop
    /// verb (r8) uses (pid, start-time) identity to tell the session's
    /// process from a reused pid, so a row WITHOUT `startedAt` reads as
    /// unidentifiable when the cmdline is exec'd away).
    started_at_ms: i64,
    status: Status,
    /// Accumulated, not-yet-submitted composer bytes (length is the oracle's
    /// `bytes=` field). Absorbed CRs become literal '\n' here.
    composer: Vec<u8>,
    /// CRs absorbed into the CURRENT composer buffer (the `composer_crs=` field).
    composer_crs: u32,
    turn: u32,
    /// When the current busy window ends (None when idle). The run loop polls
    /// stdin until this instant so CRs arriving while busy are recorded.
    busy_until: Option<Instant>,
    /// W8 stall seam state. Cumulative INPUT bytes seen (the stall trigger keys on
    /// this); whether the (one-shot) stall has triggered; when the stall pause ends
    /// (None until armed); and how many bytes have been admitted to the composer
    /// SINCE the stall trigger (capped at `stall_queue_cap`).
    input_bytes_total: usize,
    stall_triggered: bool,
    stall_until: Option<Instant>,
    stall_admitted: usize,
    reporter: Reporter,
}

impl Repl {
    fn start(cfg: Config) -> std::io::Result<Self> {
        let pid = std::process::id() as i64;
        std::fs::create_dir_all(&cfg.sessions_dir)?;
        let pid_file = cfg.sessions_dir.join(format!("{pid}.json"));
        let reporter = Reporter::open(cfg.report_path.as_deref());
        let started_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut repl = Self {
            cfg,
            pid,
            pid_file,
            started_at_ms,
            status: Status::Idle,
            composer: Vec::new(),
            composer_crs: 0,
            turn: 0,
            busy_until: None,
            input_bytes_total: 0,
            stall_triggered: false,
            stall_until: None,
            stall_admitted: 0,
            reporter,
        };
        repl.write_registry_row()?;
        repl.install_sigterm_handler();
        // M3 composer-mode: show the `❯` prompt on the initial idle screen so the
        // mux fire's plain-composer verify (SafeDefaultFacts, `❯`) passes.
        if repl.cfg.composer_mode {
            repl.render_prompt();
        }
        Ok(repl)
    }

    /// M3 composer-mode: emit the `❯ ` prompt glyph to the PTY (idle re-prompt), so
    /// the rendered screen carries a composer anchor. No-op unless `composer_mode`.
    fn render_prompt(&self) {
        if !self.cfg.composer_mode {
            return;
        }
        let mut out = std::io::stdout().lock();
        // Leading \r so the prompt starts a fresh line under a raw PTY.
        let _ = out.write_all("\r\u{276f} ".as_bytes());
        let _ = out.flush();
    }

    /// Publish the registry-row path for the SIGTERM handler and install it.
    /// After this, a SIGTERM unlinks the row and exits cleanly.
    fn install_sigterm_handler(&self) {
        // Leak a CString so the pointer is valid for the whole process (the
        // handler may fire at any time).
        if let Ok(c) = std::ffi::CString::new(self.pid_file.to_string_lossy().as_bytes()) {
            let raw = c.into_raw();
            PID_FILE_C.store(raw, Ordering::SeqCst);
        }
        // SAFETY: on_sigterm is async-signal-safe (only unlink + _exit).
        unsafe {
            libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t);
        }
    }

    /// Atomically (tmp + rename) write the registry row with the current status.
    fn write_registry_row(&mut self) -> std::io::Result<()> {
        let mut row = serde_json::json!({
            "pid": self.pid,
            "status": self.status.as_str(),
            "name": self.cfg.name,
            "startedAt": self.started_at_ms,
        });
        // W8 end-to-end leg: optional sessionId so the SUT's registry→sessionId→
        // find_jsonl_path verify resolution works against the fakerepl.
        if let Some(sid) = &self.cfg.session_id {
            row["sessionId"] = serde_json::Value::String(sid.clone());
        }
        let body = serde_json::to_vec_pretty(&row)?;
        // tmp + rename for atomicity (the SUT polls this file concurrently).
        let tmp = self
            .cfg
            .sessions_dir
            .join(format!(".{}.json.tmp", self.pid));
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.pid_file)?;
        Ok(())
    }

    fn set_status(&mut self, s: Status) {
        if self.status != s {
            self.status = s;
            // Best-effort: a transient write error must not crash the harness'
            // child mid-soak (it would mask the discipline being tested).
            let _ = self.write_registry_row();
            self.reporter.transition(s.as_str(), self.turn);
        }
    }

    /// The main stdin loop. Reads one burst at a time, classifies it, and drives
    /// the composer / submit state machine. Returns on stdin EOF.
    ///
    /// CRITICAL (a4-spec §5 "a CR arriving while busy is RECORDED, no turn"): the
    /// busy hold is a TIMED STATE, not a blocking sleep. The loop keeps READING
    /// during the busy window so a CR that arrives while busy is classified as
    /// `cr_while_busy` (queued input, no turn) — claude's real behavior, and the
    /// thing negative-control A exercises. The busy→idle transition fires when the
    /// window elapses, whether or not input arrives.
    fn run(&mut self) {
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut read_buf = [0u8; 65536];

        loop {
            // End any elapsed busy window before deciding how to block.
            self.maybe_end_busy();

            // Choose the wait budget: while busy, wake by the end of the busy
            // window (so the idle transition is on time); while idle, block.
            let wait_ms = match self.busy_until {
                Some(until) => {
                    let rem = until.saturating_duration_since(Instant::now());
                    Some(rem.as_millis() as u64)
                }
                None => None, // block
            };

            let got = match wait_ms {
                // Idle: block for the first byte of a burst (EOF ends the session).
                None => match handle.read(&mut read_buf) {
                    Ok(0) => break,
                    Ok(n) => Some(n),
                    Err(_) => break,
                },
                // Busy: wait up to the remaining window. No data → loop to end busy.
                Some(ms) => {
                    if poll_stdin_ready(ms) {
                        match handle.read(&mut read_buf) {
                            Ok(0) => break,
                            Ok(n) => Some(n),
                            Err(_) => break,
                        }
                    } else {
                        None // window elapsed with no input
                    }
                }
            };

            let Some(n) = got else {
                continue; // busy window elapsed; top of loop ends it
            };

            let mut burst: Vec<u8> = read_buf[..n].to_vec();
            // Coalesce: keep reading while more bytes arrive within GAP_MS.
            loop {
                match read_within_gap(&mut handle, &mut read_buf) {
                    GapRead::Data(m) => burst.extend_from_slice(&read_buf[..m]),
                    GapRead::Gap => break,
                    GapRead::Eof => {
                        self.process_burst(&burst);
                        self.shutdown();
                        std::process::exit(0);
                    }
                }
            }
            self.process_burst(&burst);
        }
    }

    /// If a busy window has elapsed, transition to idle and emit the done line.
    fn maybe_end_busy(&mut self) {
        if let Some(until) = self.busy_until {
            if Instant::now() >= until {
                self.busy_until = None;
                let n = self.turn;
                self.set_status(Status::Idle);
                self.emit(&format!("[turn {n}] done"));
                // W8: a stub assistant end_turn record on turn-done (gives the
                // transcript an assistant record after the user record).
                self.append_convo_assistant(n);
                // M3 composer-mode: re-show the idle `❯` prompt so a subsequent
                // send's fire verify still finds a plain composer.
                self.render_prompt();
            }
        }
    }

    /// Classify + apply one burst to the composer / submit machine.
    fn process_burst(&mut self, burst: &[u8]) {
        // ACK-1 EAT_INPUT (injection 4, ack1-spec §4.2): the bytes were READ
        // (they left the PTY queue — that is the "demonstrably consumed"
        // assert, recorded per burst) but nothing else happens: no burst
        // classification, no composer, no submit, no turn, no user record.
        if self.cfg.eat_input {
            self.reporter.eaten(burst.len());
            return;
        }

        let is_paste = burst.len() >= self.cfg.paste_threshold;
        self.reporter.burst(burst.len(), is_paste);

        // tty-queue OVERFLOW (ADR 0009 mode (a)): a single burst exceeding the model
        // bound is DROPPED WHOLESALE — no byte reaches the composer, no CR is even
        // examined, no turn starts (mirrors the live EMPTY-DROPPED mode: composer
        // empty, delta 0, did-not-go-busy). The SUT's chunking keeps every write
        // ≤1024B (< any realistic bound), so a chunked payload NEVER trips this; an
        // UNCHUNKED single 16KB write does → the negative-control RED.
        if let Some(limit) = self.cfg.drop_over_bytes {
            if burst.len() > limit {
                self.reporter.drop(burst.len(), limit);
                return;
            }
        }

        for &b in burst {
            // W8 reader-stall / saturation seam: keyed on CUMULATIVE input bytes.
            self.input_bytes_total += 1;
            self.maybe_arm_stall();

            // M3 composer-mode: the mux fire's clear-chord `Ctrl-U` (0x15) is
            // line-discard — empty the composer + re-prompt so the injected turn is
            // the CLEAN payload (never 0x15-prefixed, which would fail the mux's
            // LandingProbe as a mismatch). Mirrors a real readline composer; only
            // active under the opt-in flag, so existing gates are unaffected.
            if self.cfg.composer_mode && b == 0x15 {
                self.composer.clear();
                self.composer_crs = 0;
                self.render_prompt();
                continue;
            }
            if b == b'\r' {
                self.handle_cr(is_paste);
            } else if self.admit_content_byte() {
                // Ordinary content byte → composer. (We don't model editing;
                // every non-CR byte just accumulates, like claude's composer
                // collecting pasted text.) Under an active stall the byte may be
                // DROPPED (saturation) — admit_content_byte decides.
                self.composer.push(b);
                // M3 composer-mode: echo the admitted byte so the marker renders in
                // history (a real composer echoes typed/pasted input). Opt-in only.
                if self.cfg.composer_mode {
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(&[b]);
                    let _ = out.flush();
                }
            }
        }
        // A stall window that expired during this burst is reported once it ends
        // (see admit_content_byte / end_stall_if_elapsed). Flush the drop tally.
        self.end_stall_if_elapsed();
    }

    /// Arm the one-shot reader stall the first time cumulative input crosses
    /// `stall_after_bytes` (W8 D16 model). Idempotent after the first trigger.
    fn maybe_arm_stall(&mut self) {
        if self.stall_triggered {
            return;
        }
        let Some(after) = self.cfg.stall_after_bytes else {
            return;
        };
        if self.input_bytes_total >= after {
            self.stall_triggered = true;
            self.stall_admitted = 0;
            self.stall_until = Some(Instant::now() + Duration::from_millis(self.cfg.stall_ms));
        }
    }

    /// Decide whether the current content byte reaches the composer under the W8
    /// stall seam. Returns true to ADMIT, false to DROP.
    ///
    /// Model (the spec's sanctioned model-boundary simplification): while the
    /// stall pause is active (`now < stall_until`), the reader admits at most
    /// `stall_queue_cap` bytes counted FROM the stall trigger; bytes beyond the cap
    /// arriving during the pause are DROPPED (saturation). Once the pause elapses
    /// (`now >= stall_until`) the reader resumes and every byte is admitted again —
    /// so a payload whose middle arrived during the pause lands as
    /// leading + cap + trailing, i.e. SHORTER than sent but sharing its leading
    /// bytes (the truncation signature). When the seam is unarmed every byte is
    /// admitted (byte-identical to the pre-W8 behavior).
    fn admit_content_byte(&mut self) -> bool {
        let Some(until) = self.stall_until else {
            return true; // no active stall window
        };
        if Instant::now() >= until {
            // Pause elapsed — reading resumes; close the window (reports the tally)
            // and admit this (post-pause) byte.
            self.end_stall_if_elapsed();
            return true;
        }
        // Paused: admit up to the queue cap, drop the rest (saturation).
        if self.stall_admitted < self.cfg.stall_queue_cap {
            self.stall_admitted += 1;
            true
        } else {
            false
        }
    }

    /// If the stall pause has elapsed, close the window and emit a `stall_drop`
    /// report event with how many bytes the saturation dropped. Idempotent.
    fn end_stall_if_elapsed(&mut self) {
        if let Some(until) = self.stall_until {
            if Instant::now() >= until {
                self.stall_until = None;
                // The bytes that arrived during the pause beyond the cap were the
                // dropped ones; report the cap as the admitted count so the harness
                // can cross-check the model boundary. (The exact dropped count is
                // governed by the SUT's fixed chunk pacing during the T-ms pause.)
                self.reporter
                    .stall_drop(self.cfg.stall_queue_cap, self.cfg.stall_ms);
            }
        }
    }

    /// Decide what a single CR does, given the burst's paste-ness and current
    /// status. The CR dispositions (a4-spec §5 + the overflow follow-up):
    /// - busy → cr_while_busy: absorbed, NO turn start.
    /// - absorb_all_crs → absorbed as literal newline (never submits).
    /// - in a paste burst → absorbed as literal newline.
    /// - empty composer → empty_noop: submits nothing (no turn). claude does not
    ///   start a turn for an empty prompt (load-bearing for the tty-queue overflow
    ///   model: a dropped write leaves the composer empty, so the trailing "\r" must
    ///   NOT fake a turn).
    /// - own non-paste keystroke on a NON-empty composer → SUBMIT.
    fn handle_cr(&mut self, is_paste: bool) {
        if self.status == Status::Busy {
            // Queued-while-busy input: recorded, buffered, no turn.
            self.composer.push(b'\n');
            self.composer_crs += 1;
            self.reporter.cr("while_busy");
            return;
        }
        if self.cfg.absorb_all_crs {
            self.composer.push(b'\n');
            self.composer_crs += 1;
            self.reporter.cr("in_paste"); // absorbed-as-newline class
            return;
        }
        if is_paste {
            self.composer.push(b'\n');
            self.composer_crs += 1;
            self.reporter.cr("in_paste");
            return;
        }
        // A lone non-paste CR keystroke on an EMPTY composer submits NOTHING (claude
        // does not start a turn for an empty prompt). This is load-bearing for the
        // tty-queue OVERFLOW model: when a giant write is DROPPED wholesale the
        // composer is left empty, so the following separate "\r" must NOT manufacture
        // a (zero-byte) turn — it has to land as the live EMPTY-DROPPED mode (no turn,
        // did-not-go-busy). In every NON-drop row a content-gated CR only ever arrives
        // with the composer holding text, so this guard never suppresses a real submit.
        if self.composer.is_empty() {
            self.reporter.cr("empty_noop");
            return;
        }
        // A lone non-paste CR keystroke → SUBMIT the composer.
        self.reporter.cr("keystroke");
        self.submit();
    }

    /// Submit the current composer as one turn: status→busy, emit the accepted
    /// app-output line, and ARM the busy window (`busy_until`). The busy→idle
    /// transition + done line fire later from the run loop (`maybe_end_busy`) so
    /// the loop keeps reading during the hold and a CR arriving while busy is
    /// recorded as `cr_while_busy` (a4-spec §5). busy_ms is a harness knob (no
    /// RNG); the SUT's discipline polls the registry status during the window.
    fn submit(&mut self) {
        self.turn += 1;
        let n = self.turn;
        let bytes = self.composer.len();
        let crs = self.composer_crs;

        // W8: the conversation user record carries the ACTUAL submitted composer
        // content (post-stall-truncation) — that is what the SUT's verify step
        // reads back. Capture it BEFORE the composer is cleared.
        //
        // ACK-1 TRUNCATE (injection 5, ack1-spec §4.2): the cut is applied to
        // the RAW BYTES before any UTF-8 conversion, rounded DOWN to the
        // nearest UTF-8 boundary (never panics, never injects U+FFFD into the
        // kept prefix — red-team F6). The chunk-prefix sha join of rev C §2.4
        // needs byte-exact prefixes; the report's requested/actual pair lets
        // the harness assert the cut landed where it asked (trivially equal
        // for ASCII contents).
        let record_bytes: &[u8] = if let Some(requested) = self.cfg.truncate_user_record_bytes {
            let mut k = requested.min(self.composer.len());
            // A position is a UTF-8 boundary unless it lands on a
            // continuation byte (0b10xxxxxx). k == len is always a
            // boundary (no byte to inspect).
            while k > 0 && k < self.composer.len() && (self.composer[k] & 0xC0) == 0x80 {
                k -= 1;
            }
            self.reporter.truncated_user_record(requested, k);
            &self.composer[..k]
        } else {
            &self.composer[..]
        };
        let composer_text = String::from_utf8_lossy(record_bytes).into_owned();

        // Reset the composer for the next turn (the submitted content is "sent").
        self.composer.clear();
        self.composer_crs = 0;
        self.append_convo_user(&composer_text);

        self.set_status(Status::Busy);
        self.emit(&format!(
            "[turn {n}] accepted bytes={bytes} composer_crs={crs}"
        ));
        self.reporter.turn(n, bytes, crs);

        self.busy_until = Some(Instant::now() + Duration::from_millis(self.cfg.busy_ms));
    }

    /// Write one application-output line to stdout (the PTY), flushed.
    fn emit(&self, line: &str) {
        let mut out = std::io::stdout().lock();
        // Use \r\n: under a raw PTY the terminal does not translate \n→\r\n, so a
        // bare \n would stair-step. The harness parses on "[turn" prefixes, so the
        // exact line ending is not load-bearing — but \r\n keeps captured logs
        // readable.
        let _ = out.write_all(line.as_bytes());
        let _ = out.write_all(b"\r\n");
        let _ = out.flush();
    }

    /// W8: append a claude-shaped USER record to the conversation JSONL (if
    /// `QD_FAKEREPL_CONVO_JSONL` is set). serde_json does the escaping. Flushed per
    /// line (the SUT polls this file). Best-effort: a write error must not crash
    /// the harness child (the convo file is a test fixture, not load-bearing).
    fn append_convo_user(&mut self, text: &str) {
        let Some(path) = self.cfg.convo_jsonl.clone() else {
            return;
        };
        let rec = serde_json::json!({
            "type": "user",
            "message": { "content": text },
        });
        self.append_convo_line(&path, &rec);
    }

    /// W8: append a stub assistant `end_turn` record to the conversation JSONL.
    fn append_convo_assistant(&mut self, turn: u32) {
        let Some(path) = self.cfg.convo_jsonl.clone() else {
            return;
        };
        let rec = serde_json::json!({
            "type": "assistant",
            "message": {
                "stop_reason": "end_turn",
                "content": [{ "type": "text", "text": format!("fakerepl reply {turn}") }],
            },
        });
        self.append_convo_line(&path, &rec);
    }

    /// Append one JSON record as a line to the convo JSONL, flushed.
    fn append_convo_line(&self, path: &Path, rec: &serde_json::Value) {
        if let Ok(line) = serde_json::to_string(rec) {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = f.write_all(line.as_bytes());
                let _ = f.write_all(b"\n");
                let _ = f.flush();
            }
        }
    }

    /// Remove the registry row (clean exit). Idempotent.
    fn shutdown(&mut self) {
        self.reporter.flush();
        let _ = std::fs::remove_file(&self.pid_file);
    }
}

/// Put fd 0 (the slave PTY) into raw mode: no canonical line buffering, no echo,
/// no CR/NL input translation. After this a `read()` returns as soon as ANY
/// bytes are available (VMIN=1, VTIME=0), which is what the burst model needs.
/// A no-op if fd 0 is not a tty (e.g. a plain pipe in a manual smoke test).
fn set_stdin_raw() {
    // SAFETY: fd 0 is a valid descriptor; termios calls are standard libc.
    unsafe {
        if libc::isatty(0) != 1 {
            return;
        }
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return;
        }
        libc::cfmakeraw(&mut t);
        // cfmakeraw already sets VMIN=1, VTIME=0 and clears ICANON/ECHO/ICRNL.
        let _ = libc::tcsetattr(0, libc::TCSANOW, &t);
    }
}

/// Result of a within-gap poll+read.
enum GapRead {
    Data(usize),
    Gap,
    Eof,
}

/// Poll stdin (fd 0) for up to `GAP_MS`; if data is ready, read it. A timeout
/// (`Gap`) closes the current burst; EOF ends the session.
fn read_within_gap<R: Read>(handle: &mut R, buf: &mut [u8]) -> GapRead {
    if !poll_stdin_ready(GAP_MS) {
        return GapRead::Gap;
    }
    match handle.read(buf) {
        Ok(0) => GapRead::Eof,
        Ok(m) => GapRead::Data(m),
        Err(_) => GapRead::Eof,
    }
}

/// Block up to `timeout_ms` for fd 0 to be readable. Returns true iff readable
/// before the timeout. Uses `poll(2)` (portable across macOS + Linux).
fn poll_stdin_ready(timeout_ms: u64) -> bool {
    // We must not over-wait: poll can return early on signal (EINTR); loop the
    // remaining budget so the 50ms gap stays a tight, deterministic bound.
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let ms = remaining.as_millis() as libc::c_int;
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd, count 1, finite non-negative timeout.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        if rc > 0 {
            return true;
        }
        if rc == 0 {
            return false; // timed out
        }
        // rc < 0: EINTR → retry with the shrunken budget; other errno → give up
        // (treat as gap so the burst closes rather than spinning).
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return false;
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}
