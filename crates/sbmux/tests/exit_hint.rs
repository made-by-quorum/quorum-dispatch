//! Exit-hint UX harness — drive the real `sbmux` client in a PTY and assert the
//! post-teardown screen state.
//!
//! WHY THIS EXISTS (Pete: "why can't you run the dogfood instead of me?"):
//! Interactive/UX changes to the attach client (here: the "[press enter]" hint
//! added to `cleanup_terminal`) used to require a human to attach, detach, and
//! eyeball the screen. This harness does it mechanically: it allocates a pty,
//! runs an `sbmux open` → detach flow that exercises the client's teardown path
//! (`session_client::attach_session` line ~361 → `cleanup_terminal`), captures
//! every byte the client wrote to its terminal, and ASSERTS the post-teardown
//! screen state:
//!   1. the screen-clear sequence `\x1b[2J` is present (we KEEP the blank), AND
//!   2. the `[press enter]` hint is present (so the blank isn't a confusing void).
//!
//! It BITES: the hint assertion keys on the exact literal `[press enter]` that
//! `cleanup_terminal` prints. Delete that string from the cleanup sequence and
//! `exit_hint_present_after_clear` goes red (the clear-sequence assertion stays
//! green, proving the two are independent — a regression that drops only the hint
//! is caught). See the module-level note at the bottom on extending this to
//! `sb new` / full-attach flows.

// The shared `lib/` support module is `#[path]`-included into every integration
// binary. This binary uses a narrow subset (jail + per-session daemon spawn);
// allow the resulting dead-code/unused-import noise for THIS binary only.
#![allow(dead_code, unused_imports)]

#[path = "lib/mod.rs"]
mod libmod;

use libmod::client::start_daemon_in_jail;
use libmod::jail_env;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Detach key the client filters on (Ctrl+\, 0x1c). Writing this to the client's
/// pty makes `run_stdin_to_socket` send Detach and return, which lands the
/// attach flow at the `cleanup_terminal()` teardown (the path under test).
const DETACH_KEY: u8 = 0x1c;

/// The literal hint `cleanup_terminal` prints after clearing the screen.
const HINT: &str = "[press enter]";

/// The clear-screen CSI the cleanup sequence emits (ED 2 — erase entire screen).
const CLEAR_SCREEN: &[u8] = b"\x1b[2J";

/// A reader thread that accumulates everything the client writes to the pty.
struct PtyCapture {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl PtyCapture {
    fn snapshot(&self) -> Vec<u8> {
        self.buf.lock().unwrap().clone()
    }

    /// Block up to `timeout` for the captured bytes to satisfy `pred`.
    fn wait_for(&self, timeout: Duration, pred: impl Fn(&[u8]) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if pred(&self.snapshot()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// Drive `sbmux open <session>` in a pty against an already-running per-session
/// daemon, attach, send the detach key, and return everything the client wrote
/// to the terminal (including the teardown `cleanup_terminal` sequence).
///
/// This is the seed of a general "drive sb in a pty + assert screen" utility:
/// it isolates (a) starting a daemon in a jail, (b) running a client binary in a
/// real pty with the jail env, (c) capturing terminal output, and (d) waiting on
/// observable substrings — the four primitives any interactive-flow assertion needs.
fn capture_attach_then_detach() -> Vec<u8> {
    let jail = setup_jail("exit-hint").expect("jail setup");
    // Session name MUST carry the jail's session prefix so teardown's
    // prefix-guarded sweep can reap it.
    let session = format!("{}exit", jail.session_prefix);

    // Start the per-session daemon (binds <socket_dir>/<session>.sock). The
    // client resolves the SAME dir via XDG_RUNTIME_DIR (jail env), so it attaches
    // to this daemon rather than cold-starting its own.
    let (_daemon, _socket) =
        start_daemon_in_jail(&jail.jail, &jail.env, &session).expect("start daemon");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(libmod::client::sbmux_binary());
    cmd.env_clear();
    for (k, v) in &jail.env {
        cmd.env(k, v);
    }
    // The jail env omits PATH/TERM by default; the client doesn't strictly need
    // them, but the daemon's shell does — start_daemon_in_jail already added them
    // to its own copy. Give the client a sane TERM so its output isn't degraded.
    cmd.env("TERM", "xterm-256color");
    cmd.arg("open");
    cmd.arg(&session);
    cmd.cwd(&jail.jail.tmpdir);

    let mut child = pair.slave.spawn_command(cmd).expect("spawn sbmux open");
    // Drop the slave so the client sees EOF once we close the master.
    drop(pair.slave);

    let buf = Arc::new(Mutex::new(Vec::new()));
    let cap = PtyCapture { buf: buf.clone() };
    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let reader_buf = buf.clone();
    let reader_handle = std::thread::spawn(move || {
        let mut tmp = [0u8; 4096];
        loop {
            match reader.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => reader_buf.lock().unwrap().extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
    });

    let mut writer = pair.master.take_writer().expect("take writer");

    // Wait until the client confirms attach. The client prints a `[retach: ...]`
    // marker to stderr (which the pty merges into the master stream) once the
    // daemon's Connected frame lands — that is our "attached" signal.
    let attached = cap.wait_for(Duration::from_secs(10), |b| {
        String::from_utf8_lossy(b).contains("[retach:")
    });
    assert!(
        attached,
        "client never reported attach within 10s; captured so far:\n{}",
        String::from_utf8_lossy(&cap.snapshot())
    );

    // Record how far the stream had advanced at attach, so the teardown
    // assertions look only at bytes written AFTER we requested detach (the
    // attach replay can itself contain a clear; we want the cleanup one).
    let pre_detach_len = cap.snapshot().len();

    // Send the detach key → client sends Detach, stdin task returns, attach flow
    // reaches `cleanup_terminal()` (the teardown path under test).
    writer.write_all(&[DETACH_KEY]).expect("write detach key");
    writer.flush().expect("flush detach key");

    // Wait for the teardown to PRODUCE OUTPUT. We key on the clear-screen
    // sequence (ED 2) — the teardown-complete signal common to both assertions,
    // and one the hint can't fake. Deliberately NOT keyed on the hint, so the
    // helper stays neutral: a build that drops only the hint still completes the
    // wait here, letting `clear_screen_present_after_teardown` pass while
    // `exit_hint_present_after_clear` fails (orthogonality).
    let saw_teardown = cap.wait_for(Duration::from_secs(10), |b| {
        b.len() > pre_detach_len
            && b[pre_detach_len..]
                .windows(CLEAR_SCREEN.len())
                .any(|w| w == CLEAR_SCREEN)
    });

    // Close the master to drop the client, then reap the reader thread.
    drop(writer);
    drop(pair.master);
    // Belt: ensure the client process can't outlive the test (it should have
    // exited on detach, but a hung client must never leak past the jail).
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader_handle.join();

    let full = cap.snapshot();

    // Clean up the jail (daemon guard drops with `jail` scope end too).
    let _ = teardown_jail(&jail.jail);

    assert!(
        saw_teardown,
        "teardown never produced the clear-screen sequence within 10s; full capture:\n{}",
        String::from_utf8_lossy(&full)
    );

    full
}

/// A jail plus its derived env, kept together so the daemon guard and teardown
/// both see a consistent view.
struct JailCtx {
    jail: libmod::jail::Jail,
    env: Vec<(String, String)>,
    session_prefix: String,
}

fn setup_jail(runid: &str) -> Result<JailCtx, Box<dyn std::error::Error>> {
    let jail = libmod::setup_jail(runid)?;
    let env = jail_env(&jail);
    let session_prefix = jail.session_prefix.clone();
    Ok(JailCtx {
        jail,
        env,
        session_prefix,
    })
}

fn teardown_jail(jail: &libmod::jail::Jail) -> Result<(), Box<dyn std::error::Error>> {
    libmod::teardown_jail(jail)
}

#[test]
fn exit_hint_present_after_clear() {
    let out = capture_attach_then_detach();
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains(HINT),
        "expected the exit hint {:?} in the post-teardown terminal output, but it was absent.\n\
         This is the regression the harness guards: dropping the hint from \
         cleanup_terminal leaves the user staring at a blank screen.\nFull capture:\n{}",
        HINT,
        text
    );
}

#[test]
fn clear_screen_present_after_teardown() {
    // We KEEP the screen-clear (Pete rejected the no-clear approach). Assert the
    // clear sequence is still emitted on teardown — independent of the hint, so a
    // change that drops ONLY the hint still reds `exit_hint_present_after_clear`
    // while this one stays green (proving the two assertions are orthogonal).
    let out = capture_attach_then_detach();
    let contains_clear = out.windows(CLEAR_SCREEN.len()).any(|w| w == CLEAR_SCREEN);
    assert!(
        contains_clear,
        "expected the clear-screen sequence {:?} in teardown output; it was absent \
         (did the no-clear branch get adopted?).\nFull capture:\n{}",
        CLEAR_SCREEN,
        String::from_utf8_lossy(&out)
    );
}

// ── Full-repaint (resize) anti-flicker regression ─────────────────────────────
//
// A RESIZE forces the server to render a FULL repaint (client SIGWINCH →
// Resize + RefreshScreen → server render(full=true)). The full repaint used to
// emit a screen-wide clear `\x1b[2J` and THEN redraw every row, wrapped in a
// DEC-2026 synchronized-output bracket. That is only atomic if the terminal
// honors DEC mode 2026; over ssh to a terminal that ignores 2026 (Blink/iOS,
// hterm/xterm.js) the `\x1b[2J` blanks the screen immediately and the multi-KB
// redraw paints in progressively behind ssh latency → a visible whole-screen
// blank flash.
//
// The fix: the full repaint no longer emits a global `\x1b[2J`. It redraws every
// row in place using the same per-row erase-to-EOL (`\x1b[K`) the incremental
// path uses, so there is never a global blank state on screen, on ANY terminal.
// This test drives a real client in a pty, resizes the pty (raising SIGWINCH in
// the client), and asserts the post-resize byte-stream: NO bare `\x1b[2J`, and
// the per-row redraw IS present (CUP + `\x1b[K`).

/// Drive `sbmux open <session>` in a pty, attach, then resize the pty master to
/// a different geometry (raising SIGWINCH in the client → full repaint), and
/// return ONLY the bytes the client wrote to the terminal AFTER the resize.
fn capture_bytes_after_resize() -> Vec<u8> {
    let jail = setup_jail("resize-repaint").expect("jail setup");
    let session = format!("{}resize", jail.session_prefix);

    let (_daemon, _socket) =
        start_daemon_in_jail(&jail.jail, &jail.env, &session).expect("start daemon");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(libmod::client::sbmux_binary());
    cmd.env_clear();
    for (k, v) in &jail.env {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.arg("open");
    cmd.arg(&session);
    cmd.cwd(&jail.jail.tmpdir);

    let mut child = pair.slave.spawn_command(cmd).expect("spawn sbmux open");
    drop(pair.slave);

    let buf = Arc::new(Mutex::new(Vec::new()));
    let cap = PtyCapture { buf: buf.clone() };
    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let reader_buf = buf.clone();
    let reader_handle = std::thread::spawn(move || {
        let mut tmp = [0u8; 4096];
        loop {
            match reader.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => reader_buf.lock().unwrap().extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
    });

    let attached = cap.wait_for(Duration::from_secs(10), |b| {
        String::from_utf8_lossy(b).contains("[retach:")
    });
    assert!(
        attached,
        "client never reported attach within 10s; captured so far:\n{}",
        String::from_utf8_lossy(&cap.snapshot())
    );

    // Let the initial attach repaint settle so its bytes don't bleed into the
    // post-resize slice. We wait for the stream to go quiet for a short window.
    let mut prev_len = cap.snapshot().len();
    let settle_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let len = cap.snapshot().len();
        if len == prev_len || Instant::now() >= settle_deadline {
            break;
        }
        prev_len = len;
    }

    // Everything captured so far is the attach replay; the resize repaint lands
    // after this mark.
    let pre_resize_len = cap.snapshot().len();

    // Resize the pty master to a DIFFERENT geometry. This raises SIGWINCH in the
    // client (production path: server/session_setup.rs uses master.resize too),
    // which sends Resize + RefreshScreen → server full repaint.
    pair.master
        .resize(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize pty master");

    // Wait for the full repaint to produce output after the resize. The repaint
    // is wrapped in a sync block, so its begin marker is our "repaint arrived"
    // signal — independent of whether a clear is present (so a regression that
    // re-introduces \x1b[2J still completes this wait and fails the assertion).
    let saw_repaint = cap.wait_for(Duration::from_secs(10), |b| {
        b.len() > pre_resize_len
            && b[pre_resize_len..]
                .windows(SYNC_BEGIN.len())
                .any(|w| w == SYNC_BEGIN)
    });

    drop(pair.master);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader_handle.join();

    let full = cap.snapshot();
    let _ = teardown_jail(&jail.jail);

    assert!(
        saw_repaint,
        "resize never produced a repaint (sync block) within 10s; full capture:\n{}",
        String::from_utf8_lossy(&full)
    );

    full[pre_resize_len..].to_vec()
}

/// DEC-2026 synchronized-output begin — wraps the full repaint.
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
/// Per-row erase-to-end-of-line (EL 0) the full repaint now uses instead of a
/// global clear.
const ERASE_EOL: &[u8] = b"\x1b[K";

#[test]
fn resize_full_repaint_does_not_global_clear() {
    let after = capture_bytes_after_resize();

    // (1) The full repaint must NOT emit a global screen clear. `\x1b[2J` blanks
    // the whole screen at once; on terminals that ignore DEC 2026 the redraw
    // then paints in behind ssh latency, flashing the screen blank. This is the
    // regression the fix removes — re-introducing `\x1b[2J` in the full path
    // reds this assertion.
    let has_global_clear = after.windows(CLEAR_SCREEN.len()).any(|w| w == CLEAR_SCREEN);
    assert!(
        !has_global_clear,
        "resize full repaint emitted a global screen clear (\\x1b[2J); this \
         flashes the screen blank on terminals that ignore DEC 2026.\n\
         Post-resize bytes:\n{}",
        String::from_utf8_lossy(&after)
    );

    // (2) The full repaint must instead redraw rows in place: at least one
    // per-row erase-to-EOL (\x1b[K) must be present after the resize. This is
    // what overwrites the old (pre-resize) content without a global blank state,
    // and is the positive evidence the per-row redraw path actually ran.
    let has_per_row_erase = after.windows(ERASE_EOL.len()).any(|w| w == ERASE_EOL);
    assert!(
        has_per_row_erase,
        "resize full repaint did not emit any per-row erase-to-EOL (\\x1b[K); \
         the per-row redraw path did not run.\nPost-resize bytes:\n{}",
        String::from_utf8_lossy(&after)
    );
}

// ── Extending this harness ───────────────────────────────────────────────────
//
// `capture_attach_then_detach` is deliberately factored into reusable steps:
//   • start_daemon_in_jail(...)      — any per-session daemon flow
//   • CommandBuilder + openpty       — run ANY sbmux/sb client binary in a pty
//   • PtyCapture::wait_for(...)      — block on an observable terminal substring
//   • write_all(&[...])              — inject keystrokes (detach, Enter, text)
//
// To validate `sb new` / full-attach flows: swap the binary (sb instead of
// sbmux), drive the create→attach path, and key `wait_for` on the session's
// app-output sentinels instead of the `[retach:` marker. The capture + assert
// machinery is identical — only the spawn command and the "ready"/"done"
// substrings change.
