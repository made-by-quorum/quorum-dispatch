//! Boot-readiness MECHANICAL ports (spec §6 boot-wait seam; M2 slice).
//!
//! This module lands ONLY the two mechanical PID-file primitives M3's real
//! dialog-free boot waiter (spec §8) builds on:
//!   - [`find_pid_file`]  — port of `findPidFile`  (lifecycle.ts:127-153)
//!   - [`read_pid_status`] — port of `readPidStatus` (lifecycle.ts:159-167)
//!
//! Explicitly NOT here (M3 owns them, spec §8): the went-busy / idle wait loop,
//! ANY `zmx send` Enter-keystroke logic, and the delegated-consent dialog
//! answerer. The blind-Enter loop (lifecycle.ts:213-227) is DELETED by design
//! (L5 sanctioned divergence) — nothing in this file sends a keystroke.
//!
//! L9a: the sessions dir is INJECTED (from `SbPaths::sessions_dir`), never the
//! real `homedir()` the TS hardcodes (lifecycle.ts:131) — tests pass a temp dir.
//! L8: registry rows are parsed PERMISSIVELY — a corrupt `<x>.json` row is
//! SKIPPED, never fatal to the scan (TS per-file `catch {}`, lifecycle.ts:146).

use std::path::{Path, PathBuf};

/// Which boot phase a [`BootFailure`] occurred in (m-4, ack3-spec §8). Carried
/// TYPED from the failing phase helper instead of string-matched out of the
/// detail wording downstream (the old `emit_priming_timeout` coupling). PidFile
/// = phase 1 (`run_pid_phase`: PID file never appeared, or a dialog failure);
/// Idle = phase 2 (`run_idle_phase`: PID file present but never reached idle).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BootPhase {
    PidFile,
    Idle,
    /// Fix-A (RESPEC-DELTA §4): the child's relay SIDECAR never appeared within
    /// the boot deadline — up-live now requires pid + idle + relay-sidecar present
    /// so the relay (default) priming transport is sound. A loud BootTimeout, never
    /// a silent hang.
    Relay,
}

/// A boot-readiness failure: the TYPED phase + the human-facing detail string.
/// The detail stays byte-identical to the pre-m-4 wording (the user surface is
/// unchanged); `phase` is the machine-readable fact that used to be re-derived
/// by string-matching the detail (ack3-spec §8).
#[derive(Debug, Clone, PartialEq)]
pub struct BootFailure {
    pub phase: BootPhase,
    pub detail: String,
}

/// A sleep/poll seam so tests never actually sleep. `find_pid_file` calls
/// [`Sleeper::sleep_ms`] between directory polls; production wires
/// [`RealSleeper`], tests wire a fixture that records (and returns instantly).
pub trait Sleeper {
    fn sleep_ms(&self, ms: u64);
}

/// Real sleep via `std::thread::sleep`.
pub struct RealSleeper;

impl Sleeper for RealSleeper {
    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// A monotonic-ish clock seam for the poll deadline. We reuse the crate's
/// [`crate::effects::Clock`] (epoch-ms) rather than `Instant` so `find_pid_file`
/// is fully driveable from a fixture (a [`crate::effects::FixedClock`] won't
/// advance, so a test injects a small clock-advancing fixture if it wants the
/// loop to terminate by timeout — see tests).
use crate::effects::Clock;

/// Poll `<sessions_dir>` for a registry row whose `name` field equals
/// `session_name`, returning its file path. Port of `findPidFile`
/// (lifecycle.ts:127-153).
///
/// Loop: while `clock.now_ms() < deadline`, readdir the sessions dir, parse each
/// `*.json` PERMISSIVELY (a corrupt/locked row is SKIPPED — TS per-file
/// `catch {}`, lifecycle.ts:146; a missing dir is also tolerated — TS outer
/// `catch {}`, lifecycle.ts:148), and return the first whose `name` matches.
/// Between polls, `sleep_ms(poll_ms)` (TS `Bun.sleep(1000)`, lifecycle.ts:150).
/// Returns `None` if nothing matched before the timeout (TS returns `undefined`).
///
/// Divergence (intentional, comment-carried): TS writes a progress `.` to stderr
/// each poll (lifecycle.ts:149) — that is UX the CLI layer owns, not this pure-ish
/// scan, so it is omitted here.
pub fn find_pid_file(
    sessions_dir: &Path,
    session_name: &str,
    timeout_ms: i64,
    poll_ms: u64,
    clock: &dyn Clock,
    sleeper: &dyn Sleeper,
) -> Option<PathBuf> {
    let deadline = clock.now_ms() + timeout_ms;
    loop {
        if let Some(hit) = scan_for_name(sessions_dir, session_name) {
            return Some(hit);
        }
        // Re-check the deadline AFTER a miss so a zero/elapsed timeout still does
        // exactly one scan (matches TS: the readdir runs before the first sleep,
        // and `Date.now() < deadline` gates the NEXT iteration).
        if clock.now_ms() >= deadline {
            return None;
        }
        sleeper.sleep_ms(poll_ms);
    }
}

/// One readdir pass: first `*.json` whose parsed `name` equals `session_name`.
/// A missing dir (TS outer `catch`) or a corrupt row (TS inner `catch`) yields
/// no match rather than an error.
fn scan_for_name(sessions_dir: &Path, session_name: &str) -> Option<PathBuf> {
    let rd = std::fs::read_dir(sessions_dir).ok()?;
    for dent in rd.flatten() {
        let fname = dent.file_name();
        let fname = fname.to_string_lossy();
        if !fname.ends_with(".json") {
            continue;
        }
        let path = dent.path();
        // Permissive: read+parse failures (corrupt/locked/partial write) are
        // skipped, the scan continues (TS inner `catch {}`).
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if value.get("name").and_then(|n| n.as_str()) == Some(session_name) {
            return Some(path);
        }
    }
    None
}

/// Read the `status` field from a registry file. Port of `readPidStatus`
/// (lifecycle.ts:159-167): returns the status string, or `None` if the file
/// can't be read or parsed (TS `catch { return undefined }`).
pub fn read_pid_status(pid_file: &Path) -> Option<String> {
    let bytes = std::fs::read(pid_file).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("status")
        .and_then(|s| s.as_str())
        .map(str::to_string)
}

/// Read the child's `sessionId` from its registry row (Fix-A, RESPEC-DELTA §4):
/// the claude session UUID, which keys the child's relay sidecar
/// (`<relay_dir>/<pid>.json`'s `sessionId`). `None` if the file is unreadable, not
/// JSON, or the field is absent/non-string (the registry row populates it during
/// boot; the relay phase re-reads each poll until it lands). Permissive, mirroring
/// [`read_pid_status`].
pub fn read_pid_session_id(pid_file: &Path) -> Option<String> {
    let bytes = std::fs::read(pid_file).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("sessionId")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ===========================================================================
// M3: dialog-free boot waiter + delegated-consent answerer (spec §8, ADR 0005).
//
// This is the REAL [`crate::create::BootWaiter`], REWRITTEN from
// `waitForSessionReady` (lifecycle.ts:184-248) as a SANCTIONED divergence
// (ADR 0005). The TS structure is preserved — a PID-file phase (cap
// min(40s, deadline)) then an idle-status phase (overall 60s) — but its
// blind-Enter loop (lifecycle.ts:213-227) is DELETED. In its place:
//
//   - ZERO keystrokes on the stock path (no dialog → PID file appears → idle).
//   - The delegated-consent answerer: when the PID file is still absent, the
//     ANSI-stripped history tail is content-matched against a NAMED-dialog list
//     (A2 ships exactly one entry). A matched dialog gets `\r` ONCE, then ≤1
//     retry, then FAIL LOUD. An UNMATCHED dialog gets ZERO keystrokes and an
//     immediate loud failure (ADR 0005 §2: an unmatched dialog is NEVER
//     answered — consent integrity, e.g. the folder-trust dialog).
//
// ADD-6: every screen decision keys on APPLICATION OUTPUT (the dialog text in
// `zmx history`), never on echoed input — the answerer reads history, the only
// thing it ever writes is the single `\r`.
// ===========================================================================

use crate::mux::Mux;

/// Strip ANSI / terminal control sequences from a screen capture, returning the
/// plain text the dialog matcher reasons over. A small state machine — no regex
/// crate, no new deps (spec §8: the matcher is "ANSI-stripped history tail").
///
/// Handles the escape classes claude's TUI emits (verified against the captured
/// dev-channels dialog, 2026-06-04 journal):
///
/// - **CSI**: `ESC [` … final byte in `0x40..=0x7e` (colours, cursor moves).
/// - **OSC**: `ESC ]` … terminated by BEL (`0x07`) or ST (`ESC \`)
///   (title/hyperlink sequences).
/// - **Two/three-byte escapes**: `ESC (` `B` (charset), `ESC =`/`ESC >`
///   (keypad), and a lone `ESC` followed by a single non-`[`/`]` byte.
/// - A trailing/lone `ESC` at end-of-input is dropped.
///
/// Other control bytes (CR, BEL outside OSC) are dropped; `\n` and `\t` are kept
/// so the tail keeps its line structure for the "Enter to confirm" marker scan.
pub fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            // ESC. Look at the next byte to classify the sequence.
            let Some(&next) = bytes.get(i + 1) else {
                // Lone trailing ESC — drop it.
                break;
            };
            // Compute the byte just past the escape sequence, then SNAP to a
            // char boundary: `zmx history` is external/possibly-truncated, and a
            // multibyte char immediately after a (malformed) escape would
            // otherwise leave `i` mid-char and panic the slice below (L8: never
            // panic on junk from an external tool — found via adversarial probe
            // `strip_ansi("\x1b(中")`).
            let skip_to = match next {
                b'[' => {
                    // CSI: ESC [ <params/intermediates> <final 0x40..=0x7e>.
                    let mut j = i + 2;
                    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    // Skip the final byte too (j points at it, if present).
                    if j < bytes.len() {
                        j + 1
                    } else {
                        j
                    }
                }
                b']' => {
                    // OSC: ESC ] ... terminated by BEL (0x07) or ST (ESC \).
                    let mut j = i + 2;
                    loop {
                        if j >= bytes.len() {
                            break;
                        }
                        if bytes[j] == 0x07 {
                            j += 1;
                            break;
                        }
                        if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                            j += 2;
                            break;
                        }
                        j += 1;
                    }
                    j
                }
                b'(' | b')' | b'*' | b'+' => {
                    // Charset designation: ESC ( B  etc. — ESC + intermediate +
                    // one final byte. Skip all three (or two if truncated).
                    i + 3
                }
                _ => {
                    // Any other two-byte escape (ESC = / ESC > / ESC M …): skip
                    // ESC + the one following byte.
                    i + 2
                }
            };
            i = floor_to_char_boundary(input, skip_to.min(bytes.len()));
            continue;
        }
        // Drop bare control bytes (CR, BEL, …) but keep newline + tab so the
        // tail's line structure survives for the marker scan.
        if b == b'\n' || b == b'\t' {
            out.push(b as char);
            i += 1;
            continue;
        }
        if b < 0x20 || b == 0x7f {
            i += 1;
            continue;
        }
        // Copy ONE whole UTF-8 char starting at the (boundary-aligned) `i`. Using
        // `chars().next()` instead of a hand-computed length keeps the slice
        // char-boundary-safe even if `i` somehow points at a continuation byte.
        match input[i..].chars().next() {
            Some(ch) => {
                out.push(ch);
                i += ch.len_utf8();
            }
            None => i += 1,
        }
    }
    out
}

/// Advance `idx` forward to the nearest UTF-8 char boundary at or after it
/// (`str::ceil_char_boundary` is unstable, so this is the hand-rolled form). A
/// computed escape-skip index that lands inside a multibyte char is moved to the
/// start of the NEXT char so the subsequent slice never panics.
fn floor_to_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// A consent dialog the boot answerer is OPTED IN to auto-confirm (spec §8,
/// brief 5e). `key` is a stable identifier (used in failure messages + the
/// answered-once bookkeeping); `match_text` is a substring searched in the
/// ANSI-stripped history tail to recognize the dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedDialog {
    pub key: String,
    pub match_text: String,
}

/// The named-dialog registry: the boot dialogs the answerer is OPTED IN to
/// auto-confirm. Each `match_text` is the dialog's stable TITLE line (captured
/// ANSI-stripped); the per-line options are NOT matched on (only the title), so
/// a re-rendered option list still matches. Recognition is two-factor — a
/// dialog is answered only when the shared `Enter to confirm` marker AND a
/// named `match_text` BOTH appear in the tail (see [`detect_dialog`]).
///
/// Entries:
/// - **dev-channels** (spec §8; 5d RESOLVED 2026-06-04) — title
///   `WARNING: Loading development channels`.
/// - **folder-trust** (eng-lane item 2, Pete-ruled 2026-06-12; board STATE 132)
///   — title `Quick safety check`, the 2.1.x folder-trust dialog. Registration
///   happens only after this dialog is answered, so on a fresh dir it BLOCKS
///   boot; default-Yes (Enter selects "1. Yes, I trust this folder") is the
///   fleet's intent — the engine spawns into operator-controlled dirs. This
///   REVISES the original ADR 0005 §2 exclusion of the trust dialog (see ADR);
///   the "never auto-answer an UNLISTED dialog" rule is unchanged — folder-trust
///   is now a VETTED list entry, not a blanket loosening.
///
/// Returned as DATA (a `Vec`) so a later phase can feed the list from config
/// (spec §8: "config list"); growth happens via config or a vetted addition
/// here, NEVER by widening [`detect_dialog`] to answer unmatched dialogs.
pub fn named_dialogs() -> Vec<NamedDialog> {
    vec![
        NamedDialog {
            key: "dev-channels".to_string(),
            match_text: "WARNING: Loading development channels".to_string(),
        },
        NamedDialog {
            key: "folder-trust".to_string(),
            // The trust dialog's stable header. Two-factor with the marker
            // makes a wrong-victim send (answering a non-trust dialog) require
            // BOTH this distinctive header AND `Enter to confirm` in the same
            // boot-window tail — see the wrong-victim tests below.
            match_text: "Quick safety check".to_string(),
        },
    ]
}

/// Marker line shared by claude's confirmation dialogs (the dev-channels AND the
/// folder-trust dialogs both render it): a line containing "Enter to confirm".
/// Its presence in the ANSI-stripped tail means SOME dialog is blocking the boot.
const DIALOG_MARKER: &str = "Enter to confirm";

/// What the screen-tail content-match concluded (spec §8).
#[derive(Debug, Clone, PartialEq)]
pub enum DialogState {
    /// No dialog marker present — boot is proceeding normally (the stock path).
    NoDialog,
    /// A NAMED dialog (carries its `key`) is on screen — the answerer may send
    /// exactly one `\r` (then ≤1 retry).
    Matched(String),
    /// A dialog marker is present but NO named dialog matched — NEVER answered;
    /// the caller fails loudly with ZERO keystrokes (ADR 0005 §2).
    Unmatched,
}

/// PURE: classify a screen tail. ANSI-stripped; if the dialog marker
/// ("Enter to confirm") is absent → [`DialogState::NoDialog`]. If present and a
/// named dialog's `match_text` also appears → [`DialogState::Matched`]; else
/// [`DialogState::Unmatched`]. The registry is injected (`dialogs`) so a test —
/// and a later config-fed list — can vary it.
///
/// ADD-6: the input is APPLICATION OUTPUT (the dialog text claude rendered),
/// never echoed input — the caller passes `zmx history` tail here.
pub fn detect_dialog(screen_tail: &str, dialogs: &[NamedDialog]) -> DialogState {
    let stripped = strip_ansi(screen_tail);
    if !stripped.contains(DIALOG_MARKER) {
        return DialogState::NoDialog;
    }
    for d in dialogs {
        if stripped.contains(&d.match_text) {
            return DialogState::Matched(d.key.clone());
        }
    }
    DialogState::Unmatched
}

/// Take the last `n` lines of a capture (the "tail" the matcher reasons over).
/// Pure helper so the history read in [`EventBootWaiter`] stays small.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Tunable timeouts for [`EventBootWaiter`], defaulting to the TS values
/// (lifecycle.ts:187 `timeoutSec = 60`; :205 `Date.now() + 40000` PID cap).
#[derive(Debug, Clone)]
pub struct BootTimeouts {
    /// Overall deadline (TS `timeoutSec * 1000`, lifecycle.ts:189).
    pub overall_ms: i64,
    /// PID-file phase cap, intersected with the overall deadline
    /// (TS `Math.min(deadline, Date.now() + 40000)`, lifecycle.ts:205).
    pub pid_phase_ms: i64,
    /// Poll interval for both phases (TS `Bun.sleep(1000)` / `Bun.sleep(2000)`;
    /// we poll the PID file every ~1s, lifecycle.ts:150/243).
    pub poll_ms: u64,
    /// Settle wait after sending `\r` before re-reading history to check the
    /// dialog dismissed (the answerer's "re-read history after a short settle").
    pub settle_ms: u64,
}

/// Default steady-state boot poll interval (ms). INTENTIONAL divergence from the
/// TS `Bun.sleep(1000)` pin (lifecycle.ts:150): under ADD-13 ("works-well over
/// parity") the TS source is an intent reference, not an obligation. A session is
/// typically ready ~1.7s in but a 1s poll only OBSERVES it on the next tick,
/// burning up to ~1s of dead-wait per `qd new`. A finer poll cannot overshoot the
/// upper-bound timeouts (`overall_ms`/`pid_phase_ms`) — the loops check-before-
/// sleep and re-check the deadline — so those upper bounds stay TS-faithful.
const DEFAULT_BOOT_POLL_MS: u64 = 125;
/// Floor for `poll_ms` (env-supplied values are clamped up to this) to avoid a
/// pathological busy-spin from a tiny/zero override.
const MIN_BOOT_POLL_MS: u64 = 10;

/// Resolve the boot poll interval: `SB_BOOT_POLL_MS` if set to a valid integer,
/// else [`DEFAULT_BOOT_POLL_MS`]; clamped to a [`MIN_BOOT_POLL_MS`] floor.
fn resolve_boot_poll_ms() -> u64 {
    std::env::var("SB_BOOT_POLL_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_BOOT_POLL_MS)
        .max(MIN_BOOT_POLL_MS)
}

impl Default for BootTimeouts {
    fn default() -> Self {
        Self {
            overall_ms: 60_000,   // TS timeoutSec = 60 (lifecycle.ts:187)
            pid_phase_ms: 40_000, // TS Date.now() + 40000 (lifecycle.ts:205)
            // INTENTIONAL divergence from TS Bun.sleep(1000) (lifecycle.ts:150);
            // see DEFAULT_BOOT_POLL_MS. Env-overridable via SB_BOOT_POLL_MS.
            poll_ms: resolve_boot_poll_ms(),
            settle_ms: 1_000,
        }
    }
}

/// The REAL boot-readiness waiter (spec §8) — a [`crate::create::BootWaiter`].
///
/// Drives the named session's boot to readiness over the injected [`Mux`]
/// (history reads + the single answerer `\r`), [`Clock`] (deadlines), and
/// [`Sleeper`] (polls — tests advance a fake clock and never truly sleep). It
/// scans the injected `sessions_dir` for the PID file (L9a: never the real home)
/// and the `socket_dir` is the canonical dir the session was created in.
///
/// Send-count bound (self-review keystone, ADR 0005 §2): for a single named
/// dialog the answerer sends `\r` AT MOST TWICE — once on first match, once on
/// the single retry — then FAILS. An unmatched dialog sends ZERO. The stock
/// path sends ZERO. These bounds are enforced by `answered` (answered-at-most-
/// once bookkeeping) + `retried` (the ≤1 retry guard); see `run_pid_phase`.
pub struct EventBootWaiter<'a> {
    pub mux: &'a dyn Mux,
    /// The canonical socket dir the session lives in (history + send target).
    pub socket_dir: PathBuf,
    /// Injected registry dir (L9a) the PID file appears under.
    pub sessions_dir: PathBuf,
    /// Fix-A (RESPEC-DELTA §4): the global relay sidecar dir
    /// (`<home>/.claude/relay`). `Some` ⇒ `wait_ready` runs a third readiness
    /// phase that blocks on the child's relay sidecar (matched by `sessionId`)
    /// before returning. `None` ⇒ the relay phase is SKIPPED (resume / tests /
    /// any boot that does not drive a fresh child's relay-default priming) — the
    /// pre-Fix-A pid+idle readiness, byte-for-byte. Set via [`Self::with_relay_dir`].
    pub relay_dir: Option<PathBuf>,
    pub clock: &'a dyn Clock,
    pub sleeper: &'a dyn Sleeper,
    pub timeouts: BootTimeouts,
    /// The named-dialog registry (A2 default = [`named_dialogs`]; injectable so
    /// a later phase feeds it from config and tests can vary it).
    pub dialogs: Vec<NamedDialog>,
    /// WP-A (#4 + #1): the shared liveness source. Pane-death conviction routes
    /// through this classifier (#4: dead ONLY on a confirmed Exited*/Gone
    /// verdict, never from a silent/absent round), and the registration-row
    /// (PID-file) absence the phase polls for is never itself read as death (#1
    /// — an alive pane keeps the boot waiting, not failing). Default = the OS
    /// classifier; tests inject a fixture via [`EventBootWaiter::with_liveness`].
    pub liveness: Box<dyn crate::liveness::LivenessSource + 'a>,
}

/// R1 (observations-not-inferences): the last ACTUAL pane observation in
/// [`EventBootWaiter::run_pid_phase`] — updated only on an `Ok(list_raw)`, so
/// the timeout wording states what was seen, never an unchecked inference.
/// S2: `Dead` carries the death-evidence count, so the fail-fast verdict
/// (count >= 2) and the timeout wording share ONE state variable with no
/// implicit sync invariant between them.
///
/// F1 (red-team r1): death evidence is TWO-TIER. Both production muxes ERASE
/// list failures into Ok — ZmxMux maps any zmx failure to Ok(vec![]) and the
/// embedded mux silently OMITS a row whose probe timed out — so an ABSENT row
/// is indistinguishable from a degraded list. Absence may therefore count as
/// death evidence ONLY after the pane was sighted Alive earlier in this run
/// (absence-after-presence); an ended row is IDENTITY-POSITIVE evidence (the
/// pane's own row says it ended) and counts regardless. Absence with no prior
/// Alive sighting is UNKNOWN: it never counts, never convicts, and falls
/// through to the honest timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneObservation {
    /// No successful `list_raw` read yet (every read erred).
    Never,
    /// Reads succeeded but the pane has NOT yet been sighted alive — only
    /// absent rows so far (F1: the production degraded-list shape; also a pane
    /// that died before registering). Cannot convict; words the timeout.
    NeverSeenAlive,
    /// The last successful read showed the pane listed and not ended.
    Alive,
    /// Death evidence: `n` rounds of ended-row sightings (identity-positive)
    /// or absence-after-presence (the verdict fires at 2).
    Dead(u32),
}

impl<'a> EventBootWaiter<'a> {
    /// Construct with the A2 default timeouts + named-dialog registry.
    pub fn new(
        mux: &'a dyn Mux,
        socket_dir: PathBuf,
        sessions_dir: PathBuf,
        clock: &'a dyn Clock,
        sleeper: &'a dyn Sleeper,
    ) -> Self {
        Self {
            mux,
            socket_dir,
            sessions_dir,
            relay_dir: None,
            clock,
            sleeper,
            timeouts: BootTimeouts::default(),
            dialogs: named_dialogs(),
            liveness: Box::new(crate::liveness::OsLiveness::new()),
        }
    }

    /// WP-A: swap the liveness source (tests inject a scripted classifier; the
    /// (B) track later swaps the real impl). Builder so the production call
    /// sites keep the 5-arg [`EventBootWaiter::new`].
    pub fn with_liveness(
        mut self,
        liveness: Box<dyn crate::liveness::LivenessSource + 'a>,
    ) -> Self {
        self.liveness = liveness;
        self
    }

    /// Fix-A (RESPEC-DELTA §4): arm the relay-sidecar readiness phase. The create
    /// path passes the global `<home>/.claude/relay` dir so up-live blocks on the
    /// child's own relay sidecar (§4.3 — sidecar-presence soundly implies the
    /// relay `message-seen` will fire). Builder so the 5-arg [`Self::new`] callers
    /// that never drive a fresh child's relay priming stay unchanged.
    pub fn with_relay_dir(mut self, relay_dir: PathBuf) -> Self {
        self.relay_dir = Some(relay_dir);
        self
    }

    /// Phase 1 (lifecycle.ts:204-233, blind-Enter loop DELETED): poll for the
    /// PID file every `poll_ms`. On each round where it is STILL ABSENT, read the
    /// history tail and run [`detect_dialog`]:
    ///
    /// - `NoDialog`  → keep polling (the stock path: zero keystrokes).
    /// - `Matched`   → answer with ONE `\r`, settle, re-read; if the SAME dialog
    ///   still shows, ≤1 retry total, then FAIL.
    /// - `Unmatched` → FAIL IMMEDIATELY, zero keystrokes (ADR 0005 §2).
    ///
    /// Returns the PID file path on success, or `Err(detail)` (loud, naming
    /// `qd connect <name>`) on any dialog failure or PID-phase timeout.
    fn run_pid_phase(&self, name: &str, deadline: i64) -> Result<PathBuf, String> {
        // PID phase cap = min(overall deadline, now + pid_phase_ms)
        // (lifecycle.ts:205).
        let pid_deadline = deadline.min(self.clock.now_ms() + self.timeouts.pid_phase_ms);

        // Answered-at-most-once bookkeeping (send-count bound): a key in
        // `answered` has had its single first `\r` sent; `retried` records the
        // one allowed retry. Together they cap a single dialog at 2 sends total.
        let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut retried: std::collections::HashSet<String> = std::collections::HashSet::new();

        // punch item 6: pane-death fail-FAST. A `zmx run` that exits 0 while
        // the launched command dies instantly (bad claude binary, broken flags,
        // a failing env-file source) used to degrade into this phase's FULL
        // timeout — the dead pane polled silently for 40s. The principal zmx
        // shape for that class is an ENDED row with an exit code (identity-
        // positive evidence); two evidence rounds convict with a death-specific
        // detail.
        //
        // F1 (red-team r1): a MISSING row is NOT identity-positive — both
        // production muxes erase list failures into Ok (zmx → Ok(vec![]),
        // embedded → row silently omitted), so absence convicts ONLY after the
        // pane was sighted Alive earlier in THIS run (absence-after-presence).
        // Absence with no prior sighting is UNKNOWN — never counted, never a
        // verdict (this also covers the zmx registration window: not yet
        // sighted = cannot convict). A genuine Err(list) is equally unknown
        // (forward contract — production currently never produces it).
        //
        // R1 (observations-not-inferences): this is also the LAST actual pane
        // observation, updated ONLY on an Ok(list_raw). The timeout message
        // below words the pane's state by what was OBSERVED — never an
        // unchecked inference. S2: ONE state variable — the evidence count
        // rides Dead(n), so the fail-fast verdict and the timeout wording
        // cannot drift out of sync.
        let mut last_pane_obs = PaneObservation::Never;
        // F1: has the pane been sighted alive at least once in THIS run?
        // (Arms the absence-after-presence tier; survives later Dead states.)
        let mut seen_alive = false;
        // WP-A (#4): the pane identity `(pid, starttime)` captured at the last
        // ALIVE sighting — the key the classifier re-probes before any death
        // conviction. Captured with the start-time so the re-probe is PID-reuse
        // robust (a recycled pid classifies NotOurs, not our-death).
        let mut last_alive_key: Option<crate::liveness::ProcKey> = None;

        loop {
            // Look for the PID file (one scan; the answerer drives the polling,
            // not find_pid_file's own loop — we want a history check each round).
            if let Some(hit) = scan_for_name(&self.sessions_dir, name) {
                return Ok(hit);
            }

            // punch item 6: is the pane still alive? (Checked BEFORE the dialog
            // read — a dead pane has no dialog to answer, and its history read
            // would silently come back empty → NoDialog → the old slow timeout.)
            // `list_raw` (not the filtered `list`, which DROPS ended tasks) so
            // an ended row is still visible and carries its exit code.
            if let Ok(rows) = self.mux.list_raw(&self.socket_dir) {
                let row = rows.iter().find(|r| r.name == name);
                match row {
                    // Listed and not ended: the one alive shape. Capture the
                    // pane identity (#4) so any later death conviction re-probes
                    // THIS process, reuse-robustly.
                    Some(r) if r.ended.is_none() => {
                        seen_alive = true;
                        last_pane_obs = PaneObservation::Alive;
                        last_alive_key = Some(self.pane_key(r.pid));
                    }
                    // ENDED row: the pane's OWN row says it ended (zmx keeps
                    // ended rows in list_raw, with the exit code) — identity-
                    // positive evidence. Still GATED through the classifier (#4:
                    // dead ONLY on a confirmed Exited*/Gone verdict — a row that
                    // says "ended" for a pid the OS shows alive is a reused-pid /
                    // mux glitch, not our death).
                    Some(r) => {
                        let n = match last_pane_obs {
                            PaneObservation::Dead(n) => n + 1,
                            _ => 1,
                        };
                        if n >= 2 {
                            if self.confirm_pane_dead(self.pane_key(r.pid)) {
                                return Err(self.pane_died_detail(name, r.exit_code));
                            }
                            // Classifier says alive → fail-closed, keep waiting.
                            last_pane_obs = PaneObservation::Alive;
                        } else {
                            last_pane_obs = PaneObservation::Dead(n);
                        }
                    }
                    // ABSENCE-AFTER-PRESENCE: the pane was sighted alive in this
                    // run and the mux row is now gone. This is the #4 "silent/
                    // absent ⇒ dead" inference — NEVER trusted on its own. The
                    // conviction is GATED through the classifier (≥3× re-probe on
                    // the captured identity); a mux row can vanish transiently
                    // while the OS process lives (the #1 sibling: row-absence is
                    // not liveness), so an alive classifier verdict keeps the
                    // boot WAITING, never failing.
                    None if seen_alive => {
                        let n = match last_pane_obs {
                            PaneObservation::Dead(n) => n + 1,
                            _ => 1,
                        };
                        if n >= 2 {
                            let confirmed = last_alive_key
                                .map(|k| self.confirm_pane_dead(k))
                                .unwrap_or(false);
                            if confirmed {
                                return Err(self.pane_died_detail(name, None));
                            }
                            // Classifier says the pane is alive (or could not
                            // prove death) → authoritative positive sighting;
                            // reset the death tally and keep waiting (#1/#4).
                            last_pane_obs = PaneObservation::Alive;
                        } else {
                            last_pane_obs = PaneObservation::Dead(n);
                        }
                    }
                    // ABSENCE with NO prior sighting = UNKNOWN (F1: the
                    // production degraded-list shape — Ok(vec![]) from a
                    // failing zmx, an omitted row from a timed-out embedded
                    // probe). Never counts, never convicts; it only upgrades
                    // the wording state (and never ERASES identity-positive
                    // Dead evidence already gathered).
                    None => {
                        if last_pane_obs == PaneObservation::Never {
                            last_pane_obs = PaneObservation::NeverSeenAlive;
                        }
                    }
                }
            }

            // PID file absent → inspect the screen for a blocking dialog. ADD-6:
            // history is APPLICATION OUTPUT, the only signal we key on.
            let history = self.mux.history(&self.socket_dir, name).unwrap_or_default();
            let tail = tail_lines(&history, 30);
            match detect_dialog(&tail, &self.dialogs) {
                DialogState::NoDialog => {
                    // Stock path / dialog already cleared — keep waiting. ZERO
                    // keystrokes (the gate-asserted contract).
                }
                DialogState::Unmatched => {
                    // ADR 0005 §2: NEVER answer a dialog not in the named list.
                    // Fail loudly with ZERO keystrokes, carrying the tail.
                    return Err(format!(
                        "unanswered dialog — qd connect {name} to answer it manually \
                         (no keystroke was sent). Screen tail:\n{}",
                        strip_ansi(&tail).trim()
                    ));
                }
                DialogState::Matched(key) => {
                    // Send-count bound (ADR 0005 §2): a named dialog gets the
                    // first `\r` (records `answered`), then AT MOST one retry
                    // `\r` (records `retried`). A dialog still matched after both
                    // is a hard FAIL — never a 3rd send.
                    if !answered.contains(&key) {
                        // First answer: ONE `\r`, settle, re-read.
                        answered.insert(key.clone());
                        self.send_enter(name)?;
                        self.sleeper.sleep_ms(self.timeouts.settle_ms);
                        // Dismissed? loop back (PID file may be appearing).
                        // Still showing handled by the `retried` branch below on
                        // the next iteration (or right here next round).
                    } else if !retried.contains(&key) {
                        // Answered once, still showing → the single allowed retry.
                        retried.insert(key.clone());
                        self.send_enter(name)?;
                        self.sleeper.sleep_ms(self.timeouts.settle_ms);
                    } else {
                        // Answered AND retried, still showing → give up loudly.
                        // No 3rd send EVER (the bound).
                        return Err(format!(
                            "dialog '{key}' did not dismiss after retry — \
                             qd connect {name} to answer it manually"
                        ));
                    }
                    // Loop back to re-scan (PID file + dialog state) immediately;
                    // do NOT fall through to the poll sleep — the settle already
                    // waited and the dialog may have just cleared.
                    continue;
                }
            }

            // Deadline gate AFTER a miss so a just-elapsed timeout still did one
            // scan+detect (mirrors find_pid_file's structure).
            if self.clock.now_ms() >= pid_deadline {
                // punch item 6 + R1 (observations-not-inferences): word the
                // pane's state by the LAST actual observation, never an
                // unchecked inference. Last-seen-ALIVE is the only state that
                // earns "the pane is still up"; a single dead sighting (one
                // short of the death verdict above) or zero successful list
                // reads state exactly that and nothing more.
                let pane_note = match last_pane_obs {
                    PaneObservation::Alive => "the pane is still up, so the boot is slow or stuck",
                    PaneObservation::Dead(_) => {
                        "the pane was not seen alive on the last check — it may have just died"
                    }
                    // F1: only-absent-rows is stated as exactly that — the pane
                    // was never seen alive (a degraded list, a launch that died
                    // before registering, or one that never created the pane).
                    PaneObservation::NeverSeenAlive => "the pane was never seen alive",
                    PaneObservation::Never => {
                        "the pane's state could not be confirmed (mux list kept failing)"
                    }
                };
                return Err(format!(
                    "PID file for \"{name}\" did not appear within {}ms — {pane_note}; \
                     qd connect {name} to inspect",
                    self.timeouts.pid_phase_ms
                ));
            }
            self.sleeper.sleep_ms(self.timeouts.poll_ms);
        }
    }

    /// punch item 6: the pane-died-during-boot detail. Names the death (with
    /// zmx's recorded exit code when the ended row still carries one), points
    /// the victim at the states to check, and carries the pane's last screen
    /// tail (best-effort — zmx history often survives the task briefly) so the
    /// launch failure's own stderr is not lost.
    ///
    /// F2 (red-team r1, accepted+documented): these diagnostics are zmx-modeled.
    /// The EMBEDDED backend has no ended rows (a dead qrmux session simply
    /// vanishes from the list — D-LISTRAW) and no surviving history, so on that
    /// lane a death verdict is reachable only via absence-after-presence, with
    /// `exit_code = None` and usually an empty tail — thinner but still honest.
    fn pane_died_detail(&self, name: &str, exit_code: Option<i32>) -> String {
        let died = match exit_code {
            Some(code) => format!("exited (status {code})"),
            None => "is gone".to_string(),
        };
        let tail = self
            .mux
            .history(&self.socket_dir, name)
            .map(|h| strip_ansi(&tail_lines(&h, 15)).trim().to_string())
            .unwrap_or_default();
        let tail_note = if tail.is_empty() {
            String::new()
        } else {
            format!(" Last screen output:\n{tail}")
        };
        format!(
            "session pane \"{name}\" {died} before Claude Code wrote its PID file — \
             the launch command died at startup (not a slow boot). Check the launch \
             command and `qd ls` / the mux pane list for leftovers.{tail_note}"
        )
    }

    /// WP-A: build the reuse-robust pane identity for the classifier — the pane
    /// pid plus its start-time read NOW (the instant it was sighted alive). A
    /// pid whose start-time is unreadable stamps `0`, which a later re-read can
    /// only match within slack if the pid genuinely vanished (its re-read also
    /// fails) — fail-closed-safe (the classifier treats an unreadable identity
    /// as "cannot disprove ours", never as death).
    fn pane_key(&self, pid: i32) -> crate::liveness::ProcKey {
        crate::liveness::ProcKey::new(pid, crate::effects::proc_start_ms(pid).unwrap_or(0))
    }

    /// WP-A (#4): the claude-pid-leg death confirmation — extend the existing
    /// ≥3× re-probe (`server_launcher::probe_liveness_confirmed`, punch item 16)
    /// to the boot waiter's pane. Re-classify `key` up to [`DEATH_CONFIRM_PROBES`]
    /// times with [`DEATH_CONFIRM_BACKOFF_MS`] backoff (driven by the injected
    /// sleeper, so tests never truly sleep), short-circuiting the moment any
    /// reading is NOT dead. Returns true ONLY when every probe is a positive
    /// Exited*/Gone verdict ([`crate::liveness::confirmed_dead`]) — a silent /
    /// ambiguous / NotOurs reading is fail-closed to NOT-dead.
    fn confirm_pane_dead(&self, key: crate::liveness::ProcKey) -> bool {
        use crate::liveness::{confirmed_dead, DEATH_CONFIRM_BACKOFF_MS};
        let mut seq = Vec::with_capacity(crate::liveness::DEATH_CONFIRM_PROBES);
        seq.push(self.liveness.classify(key));
        for &backoff in DEATH_CONFIRM_BACKOFF_MS.iter() {
            // Any non-dead reading settles it (not dead) — stop re-probing.
            if !seq.last().is_some_and(|s| s.is_dead()) {
                break;
            }
            self.sleeper.sleep_ms(backoff);
            seq.push(self.liveness.classify(key));
        }
        confirmed_dead(&seq)
    }

    /// Send EXACTLY ONE carriage return through the Mux (so the ScriptedExec /
    /// FixtureMux send-log captures it — the keystroke audit, spec §8). The only
    /// place this waiter ever writes to the session.
    fn send_enter(&self, name: &str) -> Result<(), String> {
        self.mux
            .send(&self.socket_dir, name, "\r")
            .map(|_| ())
            .map_err(|e| format!("failed to send Enter to \"{name}\": {e}"))
    }

    /// Phase 2 (lifecycle.ts:235-247): poll `read_pid_status` until "idle" within
    /// the overall deadline. Returns `Err` if idle is never reached in time.
    fn run_idle_phase(&self, name: &str, pid_file: &Path, deadline: i64) -> Result<(), String> {
        loop {
            if read_pid_status(pid_file).as_deref() == Some("idle") {
                return Ok(());
            }
            if self.clock.now_ms() >= deadline {
                return Err(format!(
                    "session \"{name}\" did not reach idle status within timeout"
                ));
            }
            self.sleeper.sleep_ms(self.timeouts.poll_ms);
        }
    }

    /// Fix-A — phase 3 (RESPEC-DELTA §4.2): poll the global `relay_dir` until the
    /// child's OWN relay sidecar is present, matched by the `sessionId` from its
    /// registry row (`pid_file`). The relay server is MCP-spawned by Claude Code,
    /// async to and decoupled from dispatch's idle gate (§4.1), so `qd start` can
    /// return idle BEFORE the relay is up — the bind-race this phase closes. Once
    /// the sidecar exists, a priming relay POST after up-live lands on a live relay
    /// server and its `message-seen` fires (§4.3). Bounded by the same boot
    /// `deadline`; on timeout → loud `Err` → BootTimeout (never a silent hang).
    /// The child's `sessionId` is re-read each poll (the registry row may populate
    /// it just after idle).
    fn run_relay_phase(
        &self,
        name: &str,
        pid_file: &Path,
        relay_dir: &Path,
        deadline: i64,
    ) -> Result<(), String> {
        // The wall-clock `deadline` is the PRIMARY bound (production: RealClock).
        // The poll-count cap is a SAFETY backstop so the phase can NEVER spin
        // forever under a non-advancing clock (e.g. a fixed test clock that drives
        // boot offline) — a degenerate clock that never crosses the deadline still
        // terminates in `max_polls` iterations. Both yield the same loud Err
        // (BootTimeout), never a silent hang.
        let poll = self.timeouts.poll_ms.max(1);
        let max_polls = (deadline - self.clock.now_ms()).max(0) as u64 / poll + 2;
        let mut polls: u64 = 0;
        loop {
            if let Some(sid) = read_pid_session_id(pid_file) {
                if crate::relay::read_sidecars(relay_dir)
                    .iter()
                    .any(|h| h.session_id == sid)
                {
                    return Ok(());
                }
            }
            polls += 1;
            if self.clock.now_ms() >= deadline || polls >= max_polls {
                return Err(format!(
                    "session \"{name}\" relay sidecar did not appear within timeout"
                ));
            }
            self.sleeper.sleep_ms(poll);
        }
    }
}

impl crate::create::BootWaiter for EventBootWaiter<'_> {
    /// Port of `waitForSessionReady` (lifecycle.ts:184-248), blind-Enter loop
    /// DELETED (ADR 0005). Phase 1 (PID file + dialog answerer) then Phase 2
    /// (idle status), both inside the overall deadline.
    fn wait_ready(&self, name: &str) -> Result<(), BootFailure> {
        let deadline = self.clock.now_ms() + self.timeouts.overall_ms;
        // Phase is the TRUTH SITE: a `run_pid_phase` error (PID-file timeout or a
        // dialog failure) is the PidFile phase; a `run_idle_phase` error is the
        // Idle phase. Each helper owns exactly one phase, so wrapping its String
        // here assigns the typed phase without re-deriving it from the wording.
        let pid_file = self
            .run_pid_phase(name, deadline)
            .map_err(|detail| BootFailure {
                phase: BootPhase::PidFile,
                detail,
            })?;
        self.run_idle_phase(name, &pid_file, deadline)
            .map_err(|detail| BootFailure {
                phase: BootPhase::Idle,
                detail,
            })?;
        // Fix-A (RESPEC-DELTA §4): a THIRD phase after pid + idle — the child's
        // relay sidecar must be present so the relay-default priming transport is
        // sound (§4.3). Only when armed (the create path sets `relay_dir`); resume
        // and the unit tests leave it `None` and keep the pre-Fix-A readiness.
        if let Some(relay_dir) = self.relay_dir.clone() {
            self.run_relay_phase(name, &pid_file, &relay_dir, deadline)
                .map_err(|detail| BootFailure {
                    phase: BootPhase::Relay,
                    detail,
                })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::Clock;
    use std::cell::{Cell, RefCell};
    use std::fs;
    use tempfile::tempdir;

    /// Poll-interval resolution: the new fast default, the SB_BOOT_POLL_MS
    /// override, invalid-value fallback, and the floor clamp. Pure value
    /// resolution — no real sleeps. All env cases live in ONE test because the
    /// process environment is global and Rust runs tests in parallel threads;
    /// mutating it here and restoring at the end keeps it deterministic.
    #[test]
    fn boot_poll_ms_resolution() {
        // Snapshot + clear so the default branch is observed cleanly.
        let prior = std::env::var("SB_BOOT_POLL_MS").ok();
        std::env::remove_var("SB_BOOT_POLL_MS");

        // (a) Unset → fast default (NOT the old TS 1000ms pin).
        assert_eq!(resolve_boot_poll_ms(), 125);
        assert_eq!(DEFAULT_BOOT_POLL_MS, 125);
        assert_eq!(BootTimeouts::default().poll_ms, 125);
        // Upper-bound timeouts stay TS-faithful.
        assert_eq!(BootTimeouts::default().overall_ms, 60_000);
        assert_eq!(BootTimeouts::default().pid_phase_ms, 40_000);

        // (b) Valid override is honored.
        std::env::set_var("SB_BOOT_POLL_MS", "50");
        assert_eq!(resolve_boot_poll_ms(), 50);
        std::env::set_var("SB_BOOT_POLL_MS", " 200 "); // trimmed
        assert_eq!(resolve_boot_poll_ms(), 200);

        // (c) Invalid value → falls back to the default.
        std::env::set_var("SB_BOOT_POLL_MS", "not-a-number");
        assert_eq!(resolve_boot_poll_ms(), 125);
        std::env::set_var("SB_BOOT_POLL_MS", "");
        assert_eq!(resolve_boot_poll_ms(), 125);

        // (d) Floor clamp: a too-small / zero override is raised to the floor.
        std::env::set_var("SB_BOOT_POLL_MS", "0");
        assert_eq!(resolve_boot_poll_ms(), MIN_BOOT_POLL_MS);
        std::env::set_var("SB_BOOT_POLL_MS", "3");
        assert_eq!(resolve_boot_poll_ms(), MIN_BOOT_POLL_MS);

        // Restore prior environment.
        match prior {
            Some(v) => std::env::set_var("SB_BOOT_POLL_MS", v),
            None => std::env::remove_var("SB_BOOT_POLL_MS"),
        }
    }

    /// A clock that advances by a fixed step each read, so the find loop is
    /// guaranteed to reach its deadline without a real sleep.
    struct SteppingClock {
        now: Cell<i64>,
        step: i64,
    }
    impl SteppingClock {
        fn new(step: i64) -> Self {
            Self {
                now: Cell::new(0),
                step,
            }
        }
    }
    impl Clock for SteppingClock {
        fn now_ms(&self) -> i64 {
            let v = self.now.get();
            self.now.set(v + self.step);
            v
        }
    }

    /// Records sleeps; never actually sleeps.
    #[derive(Default)]
    struct RecordingSleeper {
        calls: Cell<u32>,
    }
    impl Sleeper for RecordingSleeper {
        fn sleep_ms(&self, _ms: u64) {
            self.calls.set(self.calls.get() + 1);
        }
    }

    fn write_row(dir: &Path, pid: i64, name: &str) {
        fs::write(
            dir.join(format!("{pid}.json")),
            format!(r#"{{"pid":{pid},"name":"{name}","status":"idle"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn find_pid_file_returns_matching_row_first_pass() {
        let dir = tempdir().unwrap();
        write_row(dir.path(), 10, "other");
        write_row(dir.path(), 20, "wanted");
        let clock = SteppingClock::new(1);
        let sleeper = RecordingSleeper::default();
        let hit = find_pid_file(dir.path(), "wanted", 5000, 1000, &clock, &sleeper).unwrap();
        assert!(hit.ends_with("20.json"));
        // Found on the first scan → no sleep.
        assert_eq!(sleeper.calls.get(), 0);
    }

    #[test]
    fn find_pid_file_skips_corrupt_rows() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("1.json"), b"{not json").unwrap();
        // A non-json file is ignored entirely.
        fs::write(dir.path().join("notes.txt"), b"hi").unwrap();
        write_row(dir.path(), 2, "wanted");
        let clock = SteppingClock::new(1);
        let sleeper = RecordingSleeper::default();
        let hit = find_pid_file(dir.path(), "wanted", 5000, 1000, &clock, &sleeper).unwrap();
        assert!(hit.ends_with("2.json"));
    }

    #[test]
    fn find_pid_file_times_out_to_none() {
        let dir = tempdir().unwrap();
        write_row(dir.path(), 1, "present-but-wrong");
        // Step 100ms/read, 250ms timeout → loop exits by deadline, no real sleep.
        let clock = SteppingClock::new(100);
        let sleeper = RecordingSleeper::default();
        assert_eq!(
            find_pid_file(dir.path(), "absent", 250, 1000, &clock, &sleeper),
            None
        );
        // At least one sleep happened between polls before the deadline tripped.
        assert!(sleeper.calls.get() >= 1);
    }

    #[test]
    fn find_pid_file_missing_dir_is_tolerated() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope");
        let clock = SteppingClock::new(100);
        let sleeper = RecordingSleeper::default();
        assert_eq!(
            find_pid_file(&missing, "x", 200, 1000, &clock, &sleeper),
            None
        );
    }

    #[test]
    fn read_pid_status_reads_or_none() {
        let dir = tempdir().unwrap();
        write_row(dir.path(), 7, "s");
        assert_eq!(
            read_pid_status(&dir.path().join("7.json")),
            Some("idle".to_string())
        );
        // Missing file → None.
        assert_eq!(read_pid_status(&dir.path().join("404.json")), None);
        // Corrupt file → None.
        fs::write(dir.path().join("8.json"), b"{bad").unwrap();
        assert_eq!(read_pid_status(&dir.path().join("8.json")), None);
        // Row with no status → None.
        fs::write(dir.path().join("9.json"), br#"{"pid":9,"name":"s"}"#).unwrap();
        assert_eq!(read_pid_status(&dir.path().join("9.json")), None);
    }

    // =======================================================================
    // M3: strip_ansi / detect_dialog / EventBootWaiter (spec §8, ADR 0005).
    // =======================================================================

    use crate::create::BootWaiter;
    use crate::exec::ExecResult;
    use crate::mux::{Mux, MuxSession};
    use std::collections::VecDeque;
    use std::io;

    // --- strip_ansi --------------------------------------------------------

    #[test]
    fn strip_ansi_removes_csi_color_and_cursor() {
        // SGR colour + cursor-move CSI sequences around plain text.
        let s = "\x1b[31mred\x1b[0m \x1b[2J\x1b[1;1Hhome";
        assert_eq!(strip_ansi(s), "red home");
    }

    #[test]
    fn strip_ansi_removes_osc_bel_and_st_terminated() {
        // OSC title set terminated by BEL, and a hyperlink terminated by ST.
        let bel = "\x1b]0;window title\x07keep";
        assert_eq!(strip_ansi(bel), "keep");
        let st = "\x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\end";
        assert_eq!(strip_ansi(st), "linkend");
    }

    #[test]
    fn strip_ansi_charset_keypad_and_lone_esc() {
        // Charset designation ESC ( B, keypad ESC =, and a trailing lone ESC.
        let s = "\x1b(Bplain\x1b=more\x1b";
        assert_eq!(strip_ansi(s), "plainmore");
    }

    #[test]
    fn strip_ansi_keeps_newlines_tabs_drops_cr() {
        let s = "line1\r\n\tline2";
        assert_eq!(strip_ansi(s), "line1\n\tline2");
    }

    #[test]
    fn strip_ansi_multibyte_after_escape_never_panics() {
        // Adversarial / truncated history (L8: external input must never panic).
        // A multibyte char immediately after a charset/2-byte escape used to land
        // `i` mid-UTF-8 and panic the slice — these must all return cleanly.
        assert_eq!(strip_ansi("\u{1b}(B中文text"), "中文text");
        // ESC ( followed directly by a multibyte char (intermediate is the lead
        // byte of 中) — the boundary snap saves us; output is whatever remains.
        let _ = strip_ansi("\u{1b}(中"); // must not panic
        let _ = strip_ansi("\u{1b}]8;;\u{276f}"); // unterminated OSC + multibyte
        let _ = strip_ansi("plain中\u{1b}[0m");
        // The common case still strips cleanly.
        assert_eq!(strip_ansi("plain中\u{1b}[0m"), "plain中");
    }

    #[test]
    fn strip_ansi_on_captured_dev_channels_dialog() {
        // The real captured dialog, wrapped in representative escape codes (the
        // box-drawing + colour the TUI emits). After stripping, the WARNING
        // title line and the marker survive for detect_dialog.
        let raw = "\x1b[2m\x1b[38;5;240m╭─\x1b[0m\n\
            \x1b[1mWARNING: Loading development channels\x1b[0m\n\
            \x1b[2m--dangerously-load-development-channels is for local channel development only.\x1b[0m\n\
            \x1b[36m❯ 1. I am using this for local development\x1b[0m\n\
            \x1b[2m  2. Exit\x1b[0m\n\
            \x1b[2mEnter to confirm · Esc to cancel\x1b[0m\n";
        let out = strip_ansi(raw);
        assert!(out.contains("WARNING: Loading development channels"));
        assert!(out.contains("Enter to confirm"));
        assert!(out.contains("1. I am using this for local development"));
        // No ESC bytes survive.
        assert!(!out.contains('\x1b'));
    }

    // --- detect_dialog -----------------------------------------------------

    const DEV_CHANNELS_TAIL: &str = "\
WARNING: Loading development channels
--dangerously-load-development-channels is for local channel development only. Do not use this option to run channels you have downloaded off the internet.
Please use --channels to run a list of approved channels.
Channels: server:relay
\u{276f} 1. I am using this for local development
  2. Exit
Enter to confirm \u{b7} Esc to cancel";

    // The REAL 2.1.175 folder-trust dialog, captured VERBATIM from `zmx history`
    // of a live `claude` booted in a fresh (untrusted) dir — exactly the bytes
    // the production boot waiter's `ZmxMux::history` (plain `zmx history <name>`,
    // no `--vt`) hands `detect_dialog`. Provenance (L25 — external-tool fixtures
    // start from a real capture, never a hand-typed minimal shape): captured
    // 2026-06-12, eng-lane item 2, claude 2.1.175 on devhost; board STATE
    // 132. The render carries the title `Quick safety check` and the shared
    // `Enter to confirm` marker with LITERAL spaces (the VT-rendered scrollback,
    // not cursor-positioned glyphs). This is the differential's ground truth.
    const FOLDER_TRUST_TAIL: &str = include_str!("../tests/fixtures/boot/trust-dialog-2.1.175.txt");

    // A hypothetical FUTURE dialog: the shared marker is present but NO named
    // entry matches — the exemplar for the "never answer an UNLISTED dialog"
    // guarantee (ADR 0005 §2). Synthetic BY DESIGN (additive, labeled per L25):
    // it must be a shape no real capture exhibits, so it can never accidentally
    // equal a listed dialog.
    const UNKNOWN_DIALOG_TAIL: &str = "\
Some brand-new confirmation we have never seen
\u{276f} 1. Proceed
  2. Cancel
Enter to confirm \u{b7} Esc to cancel";

    #[test]
    fn detect_dialog_no_marker_is_nodialog() {
        let st = detect_dialog("just some normal boot output\n> ready", &named_dialogs());
        assert_eq!(st, DialogState::NoDialog);
    }

    #[test]
    fn detect_dialog_matched_dev_channels() {
        let st = detect_dialog(DEV_CHANNELS_TAIL, &named_dialogs());
        assert_eq!(st, DialogState::Matched("dev-channels".to_string()));
    }

    #[test]
    fn detect_dialog_matched_folder_trust_real_capture() {
        // eng-lane item 2: the REAL 2.1.175 trust dialog is now MATCHED (was
        // Unmatched → never dismissed → boot timed out, board STATE 132).
        let st = detect_dialog(FOLDER_TRUST_TAIL, &named_dialogs());
        assert_eq!(st, DialogState::Matched("folder-trust".to_string()));
    }

    #[test]
    fn folder_trust_real_capture_differential_old_vs_new_registry() {
        // THE repro (failing-then-green on a REAL capture): the only thing that
        // changed is the registry. Against the OLD registry (dev-channels only)
        // the real trust dialog is Unmatched — the pre-fix bug, where the marker
        // was present but no named entry matched, so the answerer refused it and
        // boot stalled. Against the NEW registry it is Matched("folder-trust").
        let old_registry = vec![NamedDialog {
            key: "dev-channels".to_string(),
            match_text: "WARNING: Loading development channels".to_string(),
        }];
        assert_eq!(
            detect_dialog(FOLDER_TRUST_TAIL, &old_registry),
            DialogState::Unmatched,
            "pre-fix: marker present, no named match → Unmatched (boot stalls)"
        );
        assert_eq!(
            detect_dialog(FOLDER_TRUST_TAIL, &named_dialogs()),
            DialogState::Matched("folder-trust".to_string()),
            "post-fix: the vetted folder-trust entry matches"
        );
    }

    #[test]
    fn detect_dialog_unmatched_unknown_dialog_with_marker() {
        // A marker-bearing dialog with NO named match is STILL Unmatched — the
        // fix added ONE vetted entry, it did not loosen the unmatched guarantee.
        let st = detect_dialog(UNKNOWN_DIALOG_TAIL, &named_dialogs());
        assert_eq!(st, DialogState::Unmatched);
    }

    #[test]
    fn detect_dialog_trust_title_without_marker_is_nodialog() {
        // WRONG-VICTIM guard: the trust TITLE appearing in prose (e.g. scrollback
        // text, or this very phrase "Quick safety check") WITHOUT the shared
        // `Enter to confirm` marker is NOT a dialog → NoDialog, ZERO keystrokes.
        // Recognition is two-factor; the title alone never triggers a send.
        let prose = "earlier output mentioning a Quick safety check in passing\n> ready";
        assert_eq!(
            detect_dialog(prose, &named_dialogs()),
            DialogState::NoDialog
        );
    }

    #[test]
    fn named_dialogs_lists_dev_channels_then_folder_trust() {
        let d = named_dialogs();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].key, "dev-channels");
        assert_eq!(d[0].match_text, "WARNING: Loading development channels");
        assert_eq!(d[1].key, "folder-trust");
        assert_eq!(d[1].match_text, "Quick safety check");
    }

    // --- EventBootWaiter test harness --------------------------------------

    /// A test mux that:
    ///
    /// - returns history from a staged queue (each `history` call pops the next
    ///   screen; the LAST value sticks once the queue drains),
    /// - records every `send` (the keystroke audit),
    /// - reports the pane's state to `list_raw` from a staged queue (same
    ///   pop-and-stick shape — S1: state TRANSITIONS are stageable),
    /// - optionally writes the PID file once `send` has been called
    ///   `pid_after_sends` times (modeling "the \r dismissed the dialog and
    ///   claude then wrote its registry file") or once `list_raw` has been
    ///   polled `pid_after_polls` times (modeling a slow boot that completes).
    ///
    /// Only `history` + `send` + `list_raw` are exercised by the waiter; the
    /// other verbs (including the filtered `list`, which the waiter must never
    /// use — it drops ended rows) are `unreachable!()` so a stray call is caught.
    struct BootMux {
        history_script: RefCell<VecDeque<String>>,
        last_history: RefCell<String>,
        send_log: RefCell<Vec<String>>,
        sessions_dir: PathBuf,
        pid_name: String,
        /// Write the PID file after this many sends (None = never via send).
        pid_after_sends: Option<usize>,
        /// punch item 6 / S1: per-`list_raw`-call pane states (pop front, the
        /// LAST popped value sticks once the queue drains). Default = Alive.
        pane_script: RefCell<VecDeque<PaneState>>,
        last_pane: Cell<PaneState>,
        /// Write the PID file (idle) once `list_raw` has been called this many
        /// times (None = never via polling).
        pid_after_polls: Option<usize>,
        list_raw_calls: Cell<usize>,
    }
    /// FIXTURE FIDELITY IS BIDIRECTIONAL — a fixture must match production's
    /// honesty EXACTLY, in both directions. One direction: a fixture must not
    /// be more DISHONEST than production (the P0-r9 boot-liar-rows precedent).
    /// Other direction: a fixture must not be more HONEST than production —
    /// the B1 F1 lesson: this fixture's `ListErr` arm gave the waiter an Err
    /// channel the REAL muxes never produce (ZmxMux maps every zmx failure to
    /// Ok(vec![]); the embedded mux silently OMITS a row whose probe failed),
    /// so the review exercised an Err-tolerance that was dead code in
    /// production while the real degraded shape — Ok-ERASURE — went untested
    /// and falsely convicted healthy panes.
    #[derive(Clone, Copy)]
    enum PaneState {
        Alive,
        /// THE production degraded-list shape (F1): Ok with the row erased —
        /// what a failing `zmx list` (Ok(vec![])) or a timed-out embedded
        /// probe (row omitted) actually looks like. Also a genuinely-reaped
        /// pane. The waiter cannot tell these apart; pins stage THIS arm for
        /// degraded-list behavior.
        Gone,
        /// zmx's identity-positive death shape: the ended row, exit code kept.
        Ended(Option<i32>),
        /// The list_raw read ERRORS. FORWARD-CONTRACT arm, not a production
        /// shape: production muxes currently never return Err (see above) —
        /// this pins the waiter's Err-tolerance semantics in case a future
        /// mux grows an honest error channel.
        ListErr,
    }
    impl BootMux {
        fn new(sessions_dir: PathBuf, name: &str) -> Self {
            Self {
                history_script: RefCell::new(VecDeque::new()),
                last_history: RefCell::new(String::new()),
                send_log: RefCell::new(Vec::new()),
                sessions_dir,
                pid_name: name.to_string(),
                pid_after_sends: None,
                pane_script: RefCell::new(VecDeque::new()),
                last_pane: Cell::new(PaneState::Alive),
                pid_after_polls: None,
                list_raw_calls: Cell::new(0),
            }
        }
        /// Fixed pane state for every round (a one-element script that sticks).
        fn with_pane(self, pane: PaneState) -> Self {
            self.with_pane_script(&[pane])
        }
        /// S1: per-round pane states; the last one sticks (the history shape).
        fn with_pane_script(mut self, states: &[PaneState]) -> Self {
            self.pane_script = RefCell::new(states.iter().copied().collect());
            self
        }
        fn with_history(mut self, screens: &[&str]) -> Self {
            self.history_script = RefCell::new(screens.iter().map(|s| s.to_string()).collect());
            self
        }
        fn pid_after_sends(mut self, n: usize) -> Self {
            self.pid_after_sends = Some(n);
            self
        }
        fn pid_after_polls(mut self, n: usize) -> Self {
            self.pid_after_polls = Some(n);
            self
        }
        /// The pane state for THIS list_raw call (pop front; last sticks).
        fn next_pane_state(&self) -> PaneState {
            let mut q = self.pane_script.borrow_mut();
            if let Some(front) = q.pop_front() {
                self.last_pane.set(front);
                front
            } else {
                self.last_pane.get()
            }
        }
        fn send_count(&self) -> usize {
            self.send_log.borrow().len()
        }
        fn write_pid(&self, status: &str) {
            let _ = std::fs::create_dir_all(&self.sessions_dir);
            std::fs::write(
                self.sessions_dir.join("100.json"),
                format!(
                    r#"{{"pid":100,"name":"{}","status":"{status}"}}"#,
                    self.pid_name
                ),
            )
            .unwrap();
        }
        /// The named pane's list row. `ended` = Some(exit_code) for an ended row.
        fn pane_row(&self, ended: Option<Option<i32>>) -> MuxSession {
            MuxSession {
                name: self.pid_name.clone(),
                pid: 111,
                clients: 0,
                created: 0,
                start_dir: "/w".into(),
                cmd: "claude".into(),
                current: false,
                socket_dir: None,
                ended: ended.map(|_| 1_700_000_000),
                exit_code: ended.flatten(),
                zmx_status: None,
                err: None,
            }
        }
    }
    impl Mux for BootMux {
        fn history(&self, _d: &Path, _n: &str) -> io::Result<String> {
            let mut q = self.history_script.borrow_mut();
            if let Some(front) = q.pop_front() {
                *self.last_history.borrow_mut() = front.clone();
                Ok(front)
            } else {
                Ok(self.last_history.borrow().clone())
            }
        }
        fn send(&self, _d: &Path, _n: &str, text: &str) -> io::Result<ExecResult> {
            self.send_log.borrow_mut().push(text.to_string());
            if let Some(threshold) = self.pid_after_sends {
                if self.send_log.borrow().len() >= threshold {
                    self.write_pid("idle");
                }
            }
            Ok(ExecResult {
                status: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            })
        }
        fn list(&self, _d: &Path) -> io::Result<Vec<MuxSession>> {
            // S3: the waiter must NEVER use the filtered list (it drops ended
            // rows — a dead pane would look identical to a missing one).
            unreachable!("boot waiter must use list_raw, never the filtered list")
        }
        fn list_raw(&self, _d: &Path) -> io::Result<Vec<MuxSession>> {
            self.list_raw_calls.set(self.list_raw_calls.get() + 1);
            if let Some(threshold) = self.pid_after_polls {
                if self.list_raw_calls.get() >= threshold {
                    self.write_pid("idle");
                }
            }
            match self.next_pane_state() {
                PaneState::Alive => Ok(vec![self.pane_row(None)]),
                PaneState::Gone => Ok(vec![]),
                PaneState::Ended(code) => Ok(vec![self.pane_row(Some(code))]),
                PaneState::ListErr => Err(io::Error::other("zmx list failed")),
            }
        }
        fn run_detached(&self, _d: &Path, _n: &str, _c: &str, _w: &Path) -> io::Result<ExecResult> {
            unreachable!("boot waiter never runs a session")
        }
        fn kill(&self, _d: &Path, _n: &str) -> io::Result<i32> {
            unreachable!("boot waiter never kills")
        }
        fn wait(&self, _d: &Path, _n: &[String]) -> io::Result<i32> {
            unreachable!("boot waiter never waits on tasks")
        }
        fn attach(&self, _d: &Path, _n: &str) -> io::Result<i32> {
            unreachable!("boot waiter never attaches")
        }
    }

    /// Build an `EventBootWaiter` with a fast-stepping clock + recording sleeper
    /// (no real sleep) and tight timeouts so the tests terminate quickly.
    fn waiter<'a>(
        mux: &'a dyn Mux,
        socket_dir: PathBuf,
        sessions_dir: PathBuf,
        clock: &'a dyn Clock,
        sleeper: &'a dyn Sleeper,
    ) -> EventBootWaiter<'a> {
        EventBootWaiter {
            mux,
            socket_dir,
            sessions_dir,
            relay_dir: None,
            clock,
            sleeper,
            // Small budgets so a timeout test exits in a few stepped reads.
            timeouts: BootTimeouts {
                overall_ms: 1_000,
                pid_phase_ms: 500,
                poll_ms: 1,
                settle_ms: 1,
            },
            dialogs: named_dialogs(),
            // Default: the pane really died — so the existing death-conviction
            // tests confirm Gone hermetically (no real /proc). Tests exercising
            // the silent-but-alive guard override via `.with_liveness(..)`.
            liveness: Box::new(FixtureLiveness::fixed(
                crate::liveness::LifecycleState::Gone,
            )),
        }
    }

    /// A scripted [`crate::liveness::LivenessSource`] for the boot tests: returns
    /// the queued verdicts in order, then repeats the last forever. `fixed(s)`
    /// returns `s` on every probe; `scripted([..])` drives the ≥3× re-probe.
    struct FixtureLiveness {
        seq: std::cell::RefCell<std::collections::VecDeque<crate::liveness::LifecycleState>>,
        last: crate::liveness::LifecycleState,
    }

    impl FixtureLiveness {
        fn fixed(s: crate::liveness::LifecycleState) -> Self {
            Self {
                seq: std::cell::RefCell::new(std::collections::VecDeque::new()),
                last: s,
            }
        }
        fn scripted(states: &[crate::liveness::LifecycleState]) -> Self {
            Self {
                seq: states
                    .iter()
                    .copied()
                    .collect::<std::collections::VecDeque<_>>()
                    .into(),
                last: *states.last().expect("non-empty script"),
            }
        }
    }

    impl crate::liveness::LivenessSource for FixtureLiveness {
        fn classify(&self, _key: crate::liveness::ProcKey) -> crate::liveness::LifecycleState {
            self.seq.borrow_mut().pop_front().unwrap_or(self.last)
        }
    }

    #[test]
    fn stock_boot_zero_keystrokes() {
        // PID file already present + idle → no dialog, ZERO sends.
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess");
        mux.write_pid("idle");
        let clock = SteppingClock::new(1);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        assert!(w.wait_ready("sess").is_ok());
        // THE GATE: zero keystrokes (dialog-free stock boot, ADR 0005 §1).
        assert_eq!(mux.send_count(), 0, "stock boot must send ZERO keystrokes");
    }

    #[test]
    fn mutation_evidence_injected_enter_fails_zero_keystroke_assert() {
        // MUTATION EVIDENCE (gate row 10): a deliberately-wrong waiter that sends
        // one Enter on the stock path MUST fail the zero-keystroke assert — this
        // proves the assert in `stock_boot_zero_keystrokes` has TEETH. We simulate
        // the buggy waiter by issuing the blind Enter ourselves against the same
        // mux, then asserting the gate would catch it.
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess");
        mux.write_pid("idle");
        // The MUTATION: a blind Enter (what the deleted TS loop, lifecycle.ts:215,
        // would have sent). The real EventBootWaiter NEVER does this on the stock
        // path — that is the whole point of ADR 0005.
        mux.send(&socket, "sess", "\r").unwrap();
        // The zero-keystroke gate now catches the mutant: send_count != 0.
        assert_ne!(
            mux.send_count(),
            0,
            "mutation must trip the gate — proving the assert is not vacuous"
        );
    }

    #[test]
    fn dev_channels_dialog_answered_once_then_boots() {
        // Round 1: dialog showing (PID absent). After ONE \r the mux writes the
        // PID file (idle) and the next history read shows the dialog cleared.
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess")
            .with_history(&[DEV_CHANNELS_TAIL, "booted\n> ready"])
            .pid_after_sends(1);
        let clock = SteppingClock::new(1);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        assert!(w.wait_ready("sess").is_ok());
        // EXACTLY one \r sent.
        assert_eq!(mux.send_count(), 1);
        assert_eq!(mux.send_log.borrow()[0], "\r");
    }

    #[test]
    fn dev_channels_dialog_persists_two_sends_then_fail() {
        // The dialog NEVER clears: every history read shows it, the PID file never
        // appears. Answerer: \r, retry \r, then FAIL — send-log length == 2, never 3.
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        // History always returns the dialog (queue drains → last sticks).
        let mux = BootMux::new(sessions.clone(), "sess").with_history(&[DEV_CHANNELS_TAIL]);
        let clock = SteppingClock::new(1);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        let err = w.wait_ready("sess").unwrap_err();
        assert!(err.detail.contains("qd connect"), "loud failure: {err:?}");
        assert!(err.detail.contains("dev-channels"));
        // HARD BOUND: exactly 2 sends, never 3.
        assert_eq!(mux.send_count(), 2, "persistent dialog: \\r + 1 retry only");
    }

    #[test]
    fn unmatched_dialog_fails_immediately_zero_keystrokes() {
        // An UNKNOWN dialog (marker present, NOT named) → FAIL with ZERO sends,
        // error carries the tail (ADR 0005 §2). The exemplar is a synthetic
        // never-seen dialog now that the real folder-trust dialog is a vetted
        // MATCH (see folder_trust_dialog_answered_with_single_enter).
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess").with_history(&[UNKNOWN_DIALOG_TAIL]);
        let clock = SteppingClock::new(1);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        let err = w.wait_ready("sess").unwrap_err();
        assert!(err.detail.contains("unanswered dialog"));
        assert!(err.detail.contains("qd connect"));
        // The error includes the (stripped) tail for diagnosis.
        assert!(err.detail.contains("Some brand-new confirmation"));
        // CRITICAL: ZERO keystrokes to an unmatched dialog.
        assert_eq!(
            mux.send_count(),
            0,
            "an unmatched dialog must NEVER receive a keystroke"
        );
    }

    #[test]
    fn folder_trust_dialog_answered_with_single_enter() {
        // eng-lane item 2: the REAL 2.1.175 trust dialog is now DISMISSED by the
        // single answerer `\r` (default = "1. Yes, I trust this folder"), then
        // the PID file appears and boot proceeds. EXACTLY ONE keystroke — the
        // fix that closes board STATE 132's stall.
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        // Round 1: trust dialog showing (PID absent). After ONE \r the mux
        // writes the PID file (idle) and the next history read shows it cleared.
        let mux = BootMux::new(sessions.clone(), "sess")
            .with_history(&[FOLDER_TRUST_TAIL, "booted\n> ready"])
            .pid_after_sends(1);
        let clock = SteppingClock::new(1);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        w.wait_ready("sess")
            .expect("trust dialog dismissed, boot ready");
        assert_eq!(
            mux.send_count(),
            1,
            "the trust dialog is dismissed with EXACTLY ONE \\r"
        );
    }

    #[test]
    fn folder_trust_dialog_persists_two_sends_then_fail() {
        // The send-count BOUND holds for the new entry exactly as for
        // dev-channels: a trust dialog that never clears gets `\r` + one retry,
        // then a loud FAIL — never a 3rd send (ADR 0005 §2).
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess").with_history(&[FOLDER_TRUST_TAIL]);
        let clock = SteppingClock::new(1);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        let err = w.wait_ready("sess").unwrap_err();
        assert!(
            err.detail.contains("folder-trust"),
            "names the dialog: {err:?}"
        );
        assert!(err.detail.contains("qd connect"));
        assert_eq!(mux.send_count(), 2, "persistent dialog: \\r + 1 retry only");
    }

    #[test]
    fn phase2_timeout_pid_present_never_idle() {
        // PID file exists but status is "busy" forever → phase-2 timeout (TS
        // lifecycle.ts:236-247 timeout path). Zero sends (no dialog involved).
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess");
        mux.write_pid("busy"); // present but never idle
                               // Step the clock fast enough to blow the overall deadline.
        let clock = SteppingClock::new(200);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        let err = w.wait_ready("sess").unwrap_err();
        assert_eq!(err.phase, BootPhase::Idle);
        assert!(
            err.detail.contains("did not reach idle"),
            "phase-2 timeout: {err:?}"
        );
        assert_eq!(mux.send_count(), 0);
    }

    #[test]
    fn pid_phase_timeout_no_dialog_no_pid() {
        // No dialog, no PID file ever (pane stays ALIVE) → phase-1 timeout.
        // Zero sends (NoDialog path keys on app output and never blind-sends).
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess").with_history(&["booting...\n"]);
        let clock = SteppingClock::new(200);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        let err = w.wait_ready("sess").unwrap_err();
        assert_eq!(err.phase, BootPhase::PidFile);
        assert!(err.detail.contains("did not appear") || err.detail.contains("qd connect"));
        // punch item 6 (diagnostics): the timeout names the pane as still up —
        // distinguishable at the surface from the pane-death fail-fast below.
        assert!(
            err.detail.contains("the pane is still up"),
            "timeout must name the live pane: {}",
            err.detail
        );
        assert_eq!(
            mux.send_count(),
            0,
            "no dialog → zero keystrokes even on timeout"
        );
    }

    // --- punch item 6: pane-death fail-FAST (never a slow timeout) ----------

    /// F1 RE-PIN (red-team r1 — this REPLACES the pre-F1 pin that convicted on
    /// Gone-only): absent rows with NO prior alive sighting are the production
    /// degraded-list shape (Ok-erasure — a failing `zmx list` returns
    /// Ok(vec![]), a timed-out embedded probe omits the row) and MUST NOT
    /// convict. The waiter runs to the honest timeout, worded by the
    /// observation: "the pane was never seen alive".
    #[test]
    fn gone_without_prior_sighting_never_convicts_honest_timeout() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess")
            .with_pane(PaneState::Gone)
            .with_history(&["booting...\n"]);
        // Fast clock: this test EXPECTS the deadline (no conviction possible).
        let clock = SteppingClock::new(200);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        let err = w.wait_ready("sess").unwrap_err();
        assert_eq!(err.phase, BootPhase::PidFile);
        assert!(
            err.detail.contains("did not appear"),
            "honest timeout, not a verdict: {}",
            err.detail
        );
        assert!(
            !err.detail.contains("before Claude Code wrote its PID file"),
            "absence without prior sighting must NEVER convict: {}",
            err.detail
        );
        assert!(
            err.detail.contains("the pane was never seen alive"),
            "worded by the observation: {}",
            err.detail
        );
        assert_eq!(mux.send_count(), 0);
    }

    /// F1: ABSENCE-AFTER-PRESENCE still convicts fail-fast — the pane was
    /// sighted alive in this run, then gone two consecutive rounds (zmx reaped
    /// a dead task's row). Death-specific detail, long before the deadline.
    #[test]
    fn absence_after_presence_convicts_fail_fast() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess")
            .with_pane_script(&[PaneState::Alive, PaneState::Gone, PaneState::Gone])
            .with_history(&["bash: claude: command not found\n"]);
        // A SLOW clock (1ms/read): the deadline is unreachable in three rounds,
        // so an error here is PROOF of fail-fast, not a disguised timeout.
        let clock = SteppingClock::new(1);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        let err = w.wait_ready("sess").unwrap_err();
        assert_eq!(err.phase, BootPhase::PidFile);
        assert!(
            err.detail
                .contains("is gone before Claude Code wrote its PID file"),
            "death-specific detail: {}",
            err.detail
        );
        assert!(
            !err.detail.contains("did not appear within"),
            "must NOT be the timeout wording: {}",
            err.detail
        );
        // The launch failure's own output rides the error (best-effort tail).
        assert!(
            err.detail.contains("command not found"),
            "screen tail carried: {}",
            err.detail
        );
        assert_eq!(mux.send_count(), 0, "a dead pane gets zero keystrokes");
        // Fail-FAST: two poll sleeps (alive round + tolerated miss) + the two
        // WP-A death-confirm backoff sleeps (the ≥3× claude-pid-leg re-probe).
        // Still bounded — nowhere near the 40s timeout crawl this guards against.
        assert!(
            sleeper.calls.get() <= 4,
            "fail-fast, not a timeout crawl: {} sleeps",
            sleeper.calls.get()
        );
    }

    /// An ENDED row (zmx still lists the dead task) fails fast — IDENTITY-
    /// POSITIVE evidence (the pane's own row says ended), so it convicts even
    /// with no prior alive sighting, naming zmx's recorded exit code. This is
    /// the principal item-6 shape: an instantly-dead command shows as an ended
    /// row with its exit code.
    #[test]
    fn pane_ended_fails_fast_with_exit_code() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess").with_pane(PaneState::Ended(Some(127)));
        let clock = SteppingClock::new(1);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        let err = w.wait_ready("sess").unwrap_err();
        assert_eq!(err.phase, BootPhase::PidFile);
        assert!(
            err.detail
                .contains("exited (status 127) before Claude Code wrote its PID file"),
            "ended detail names the exit code: {}",
            err.detail
        );
        assert_eq!(mux.send_count(), 0);
    }

    /// R1 (observations-not-inferences): a timeout with list_raw ERRORING every
    /// round had ZERO pane observations — the message must NOT claim "the pane
    /// is still up" (an unchecked inference); it states only the observation
    /// ("could not be confirmed") + the same guidance. A list error is never
    /// death evidence either (no fail-fast — this runs to the deadline).
    /// FORWARD-CONTRACT pin (F1): production muxes currently never return Err
    /// (they erase failures into Ok — see PaneState); this pins the waiter's
    /// Err-tolerance for a future mux with an honest error channel.
    #[test]
    fn pid_phase_timeout_with_erroring_list_states_unconfirmed_not_alive() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess")
            .with_pane(PaneState::ListErr)
            .with_history(&["booting...\n"]);
        let clock = SteppingClock::new(200);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        let err = w.wait_ready("sess").unwrap_err();
        assert_eq!(err.phase, BootPhase::PidFile);
        // It IS the timeout (a list error never fail-fasts as death)…
        assert!(err.detail.contains("did not appear"), "{}", err.detail);
        assert!(
            !err.detail.contains("before Claude Code wrote its PID file"),
            "list errors are not death evidence: {}",
            err.detail
        );
        // …and the pane claim is an OBSERVATION, not an inference.
        assert!(
            !err.detail.contains("the pane is still up"),
            "zero observations must not claim a live pane: {}",
            err.detail
        );
        assert!(
            err.detail.contains("could not be confirmed"),
            "states the unconfirmed observation: {}",
            err.detail
        );
        assert!(err.detail.contains("qd connect"), "{}", err.detail);
        assert_eq!(mux.send_count(), 0);
    }

    /// NEGATIVE CONTROL (S1 restaged — the old form pre-wrote the PID file and
    /// never reached the pane check): a LATE PID file with one dead-looking
    /// round still boots. Under F1 the round-1 absence is UNKNOWN (no prior
    /// sighting — the degraded-list / registration-window shape), so it is
    /// structurally unconvictable; the pane check provably executed.
    #[test]
    fn live_pane_with_late_pid_file_still_boots() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        // S1 restage (the old form pre-wrote the PID file, so the round-1 scan
        // returned before the pane check ever ran — vacuous). Now: the PID
        // file appears only after the THIRD list_raw poll, and the pane is
        // ABSENT on round 1 then ALIVE from round 2 — the zmx registration
        // window / degraded-list shape, which F1 makes structurally
        // unconvictable (no prior sighting), NOT fail-fast as death.
        let mux = BootMux::new(sessions.clone(), "sess")
            .with_pane_script(&[PaneState::Gone, PaneState::Alive])
            .pid_after_polls(3)
            .with_history(&["booting...\n"]);
        let clock = SteppingClock::new(1); // deadline far — an Err here is a bug
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        assert!(
            w.wait_ready("sess").is_ok(),
            "one transient dead round must be tolerated, then the late PID file boots"
        );
        // The pane check genuinely ran (≥3 polls — the restage's teeth).
        assert!(
            mux.list_raw_calls.get() >= 3,
            "the pane check must have executed: {} polls",
            mux.list_raw_calls.get()
        );
        assert_eq!(mux.send_count(), 0);
    }

    /// S1 (companion): the deadline fires on the round where the pane was seen
    /// dead ONCE (one short of the death verdict) — the R1 Dead-seen-once
    /// timeout wording arm. Staging: pane script Alive, Alive, Gone with clock
    /// step 200 / pid cap 500 → pid_deadline = 700; the deadline-gate reads
    /// are 400 (round 1), 600 (round 2), 800 (round 3) — so the timeout trips
    /// on round 3, the exact round whose list_raw observed the pane dead for
    /// the FIRST time.
    #[test]
    fn pid_phase_timeout_after_one_dead_round_states_not_seen_alive() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess")
            .with_pane_script(&[PaneState::Alive, PaneState::Alive, PaneState::Gone])
            .with_history(&["booting...\n"]);
        let clock = SteppingClock::new(200);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper);
        let err = w.wait_ready("sess").unwrap_err();
        assert_eq!(err.phase, BootPhase::PidFile);
        // It IS the timeout (one dead sighting is below the death verdict)…
        assert!(err.detail.contains("did not appear"), "{}", err.detail);
        assert!(
            !err.detail.contains("before Claude Code wrote its PID file"),
            "one dead round is not the death verdict: {}",
            err.detail
        );
        // …and the wording states the OBSERVATION, not "still up".
        assert!(
            !err.detail.contains("the pane is still up"),
            "dead-seen-once must not claim a live pane: {}",
            err.detail
        );
        assert!(
            err.detail.contains("not seen alive on the last check"),
            "states the dead observation: {}",
            err.detail
        );
        assert_eq!(mux.send_count(), 0);
    }

    // ================= WP-A (#4 + #1): classifier-gated death =================
    // Each of these uses the SAME mux input as an existing convict test but a
    // different LIVENESS verdict — the OUTCOME FLIP (convict ↔ keep-waiting)
    // proves the classifier is the load-bearing oracle, not the absent/ended row.

    /// #4 + #1 (false-POSITIVE-death guard): SAME staging as
    /// `absence_after_presence_convicts_fail_fast` (pane Alive then the mux row
    /// GONE two rounds) — but the classifier confirms the pane ALIVE (the
    /// silent-window / suppressed-row shape). Absence is NOT death: the boot
    /// keeps waiting and times out reporting the pane ALIVE ("still up"), NEVER
    /// "is gone". FIX-SHAPED MUTATION: deleting the `confirm_pane_dead` gate (the
    /// un-guarded classifier) convicts here → this flips RED.
    #[test]
    fn absence_with_alive_classifier_reports_alive_not_dead() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess")
            .with_pane_script(&[PaneState::Alive, PaneState::Gone, PaneState::Gone])
            .with_history(&["booting...\n"]);
        // Same staging as the dead-seen-once sibling: timeout trips on round 3,
        // the round whose absence-after-presence reaches the death tally.
        let clock = SteppingClock::new(200);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper).with_liveness(Box::new(
            FixtureLiveness::fixed(crate::liveness::LifecycleState::AliveSilentValid),
        ));
        let err = w.wait_ready("sess").unwrap_err();
        assert_eq!(err.phase, BootPhase::PidFile);
        // NOT death — the un-gated bug would convict "is gone".
        assert!(
            !err.detail.contains("is gone before"),
            "a live pane must NOT be convicted from absence: {}",
            err.detail
        );
        // It IS the honest timeout, and it REPORTS THE PANE ALIVE (#1: the
        // absent registration row's liveness came from the classifier).
        assert!(err.detail.contains("did not appear"), "{}", err.detail);
        assert!(
            err.detail.contains("the pane is still up"),
            "classifier-alive ⇒ reported ALIVE, not dead: {}",
            err.detail
        );
        assert_eq!(mux.send_count(), 0);
    }

    /// FIX-SHAPED MUTATION (drop the re-read): the ≥3× death-confirmation must
    /// require EVERY probe dead. A scripted classifier that reads dead, then
    /// ALIVE, then dead must NOT convict — a single-shot probe (the dropped
    /// re-read) would convict on the first `Gone`. SAME mux input as the convict
    /// sibling; the transient-alive middle probe spares the pane.
    #[test]
    fn death_confirm_requires_all_reprobes_dead() {
        use crate::liveness::LifecycleState::*;
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess")
            .with_pane_script(&[PaneState::Alive, PaneState::Gone, PaneState::Gone])
            .with_history(&["booting...\n"]);
        let clock = SteppingClock::new(200);
        let sleeper = RecordingSleeper::default();
        // Re-probe sequence: dead, ALIVE, dead — the middle reading spares it.
        let w = waiter(&mux, socket, sessions, &clock, &sleeper).with_liveness(Box::new(
            FixtureLiveness::scripted(&[Gone, AliveSilentValid, Gone]),
        ));
        let err = w.wait_ready("sess").unwrap_err();
        assert!(
            !err.detail.contains("is gone before"),
            "a transient-alive re-probe must spare the pane (the re-read is load-bearing): {}",
            err.detail
        );
        assert!(
            err.detail.contains("did not appear"),
            "honest timeout: {}",
            err.detail
        );
        // The re-probe backoff sleeps were spent (the gate actually ran).
        assert!(
            sleeper.calls.get() >= 1,
            "the death-confirm re-probe must have run"
        );
    }

    /// #4 (reused-pid / mux-glitch guard): even an IDENTITY-POSITIVE ended row is
    /// gated through the classifier. SAME staging as
    /// `pane_ended_fails_fast_with_exit_code`, but the classifier says the pid is
    /// ALIVE (a recycled pid the mux still lists as ended) — so the ended row
    /// does NOT convict. The verdict FLIP vs the Gone-classifier sibling proves
    /// "dead ONLY on a confirmed Exited*/Gone verdict" (#4), never on the row alone.
    #[test]
    fn ended_row_with_alive_classifier_is_not_death() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        let mux = BootMux::new(sessions.clone(), "sess").with_pane(PaneState::Ended(Some(127)));
        let clock = SteppingClock::new(200);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper).with_liveness(Box::new(
            FixtureLiveness::fixed(crate::liveness::LifecycleState::AliveSilentValid),
        ));
        let err = w.wait_ready("sess").unwrap_err();
        assert!(
            !err.detail.contains("exited (status 127)"),
            "an ended row over an ALIVE pid must not convict: {}",
            err.detail
        );
        assert!(
            err.detail.contains("did not appear"),
            "honest timeout: {}",
            err.detail
        );
        assert_eq!(mux.send_count(), 0);
    }

    // =======================================================================
    // Fix-A — the relay-sidecar readiness phase (RESPEC-DELTA §4).
    // =======================================================================

    /// Write a registry row carrying name + idle status + the child's sessionId.
    fn write_pid_with_session(sessions_dir: &Path, name: &str, session_id: &str) {
        fs::create_dir_all(sessions_dir).unwrap();
        fs::write(
            sessions_dir.join("100.json"),
            format!(r#"{{"pid":100,"name":"{name}","status":"idle","sessionId":"{session_id}"}}"#),
        )
        .unwrap();
    }

    /// Write a relay sidecar `<relay_dir>/<pid>.json` (the shape `write_sidecar`
    /// emits: port + pid + sessionId + startedAt).
    fn write_sidecar(relay_dir: &Path, pid: i64, session_id: &str) {
        fs::create_dir_all(relay_dir).unwrap();
        fs::write(
            relay_dir.join(format!("{pid}.json")),
            format!(r#"{{"port":4321,"pid":{pid},"sessionId":"{session_id}","startedAt":"1"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn read_pid_session_id_parses_and_degrades() {
        let dir = tempdir().unwrap();
        write_pid_with_session(dir.path(), "sess", "sess-uuid-1");
        assert_eq!(
            read_pid_session_id(&dir.path().join("100.json")).as_deref(),
            Some("sess-uuid-1")
        );
        // A row with no sessionId (the early-boot shape) → None (keep polling).
        fs::write(
            dir.path().join("100.json"),
            r#"{"pid":100,"name":"sess","status":"idle"}"#,
        )
        .unwrap();
        assert_eq!(read_pid_session_id(&dir.path().join("100.json")), None);
    }

    #[test]
    fn fix_a_wait_ready_passes_once_relay_sidecar_present() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let relay = dir.path().join("relay");
        let socket = dir.path().join("zmx-501");
        write_pid_with_session(&sessions, "sess", "sess-uuid-1");
        // The child's OWN relay sidecar (matching sessionId) is present.
        write_sidecar(&relay, 200, "sess-uuid-1");

        let mux = BootMux::new(sessions.clone(), "sess");
        let clock = SteppingClock::new(1);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper).with_relay_dir(relay);
        // pid + idle + relay-sidecar present ⇒ up-live.
        assert!(w.wait_ready("sess").is_ok());
    }

    #[test]
    fn fix_a_wait_ready_times_out_to_relay_phase_when_sidecar_absent() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let relay = dir.path().join("relay");
        let socket = dir.path().join("zmx-501");
        write_pid_with_session(&sessions, "sess", "sess-uuid-1");
        fs::create_dir_all(&relay).unwrap(); // empty — no sidecar ever appears.

        let mux = BootMux::new(sessions.clone(), "sess");
        // Step the clock fast so the bounded relay loop reaches the deadline.
        let clock = SteppingClock::new(200);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper).with_relay_dir(relay);
        let err = w.wait_ready("sess").unwrap_err();
        // Loud BootTimeout, typed to the Relay phase (never a silent hang).
        assert_eq!(err.phase, BootPhase::Relay);
        assert!(
            err.detail.contains("relay sidecar did not appear"),
            "relay-phase timeout wording: {}",
            err.detail
        );
    }

    #[test]
    fn fix_a_ignores_foreign_sidecars_matches_by_session_id() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let relay = dir.path().join("relay");
        let socket = dir.path().join("zmx-501");
        write_pid_with_session(&sessions, "sess", "sess-uuid-1");
        // A sidecar exists, but for a DIFFERENT session → must NOT satisfy the gate.
        write_sidecar(&relay, 200, "some-other-session");

        let mux = BootMux::new(sessions.clone(), "sess");
        let clock = SteppingClock::new(200);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper).with_relay_dir(relay);
        let err = w.wait_ready("sess").unwrap_err();
        assert_eq!(
            err.phase,
            BootPhase::Relay,
            "a foreign sidecar must not match"
        );
    }

    #[test]
    fn no_relay_dir_keeps_pre_fix_a_readiness() {
        // When the relay phase is NOT armed (resume / tests / non-priming boots),
        // up-live stays pid + idle — no sidecar required (byte-for-byte pre-Fix-A).
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let socket = dir.path().join("zmx-501");
        write_pid_with_session(&sessions, "sess", "sess-uuid-1");
        // NOTE: no relay dir, no sidecar.
        let mux = BootMux::new(sessions.clone(), "sess");
        let clock = SteppingClock::new(1);
        let sleeper = RecordingSleeper::default();
        let w = waiter(&mux, socket, sessions, &clock, &sleeper); // relay_dir = None
        assert!(
            w.wait_ready("sess").is_ok(),
            "without a relay_dir the readiness gate must not require a sidecar"
        );
    }
}
