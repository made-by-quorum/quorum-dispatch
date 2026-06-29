//! `qd connect <session>` (ADD-26, W1 phase 1) — the human "get me into this
//! session" verb. Dispatches on the row's PROVIDER HOSTING first, then liveness:
//!
//!   - `Hosting::Daemon` (codex) → LOUD redirect (no terminal to attach), exit 1.
//!   - `Hosting::MuxPane` (claude) → live → reuse the shared attach mechanic;
//!     cold → AUTO-REVIVE (W1 phase 2) then attach the live pane. Revive FAILS →
//!     the SHARED cold-error pointing at `qd connect`, exit 1.
//!   - opencode → parked message (mirrors lifecycle.rs run_attach).
//!   - unknown provider → `refuse_unknown_provider`.
//!
//! W1 phase 2: the cold→auto-revive path is LIVE. `attach_resolved` returns the
//! cold case to this caller as [`lifecycle::AttachOutcome::Cold`] (the shared fn no
//! longer branches on the `verb` string); connect maps Cold to
//! [`super::resume::revive_claude`] (detached revive-to-drivable) THEN a plain
//! `mux.attach` of the now-live pane — the human "just works" path. Demoted
//! `attach` maps the SAME Cold outcome to the cold-error instead.

use std::path::PathBuf;

use clap::ArgMatches;

use super::common;
use super::lifecycle;
use super::lifecycle::AttachOutcome;
use super::resume;

/// `qd connect <session>` — resolve the row, then hand to the shared attach
/// mechanic (provider dispatch + cold-vs-live). No `--json` (interactive verb).
pub fn run(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    // punch item 7: per-session render mode for the auto-revive path (flag >
    // render-default config > inline). A LIVE attach is unaffected — the
    // property is launch-time only.
    let render = common::resolve_render_mode(m, &dispatch::effects::RealEnv);

    // connect is the human "attach OR resume" verb, so it must be able to RESOLVE
    // anything resume can — including a COLD, AUTO-named session (user_named=false).
    // `JoinOpts::default()` (include_all=false) runs the list cap's named-only
    // filter (join.rs apply_list_cap), which drops auto-named rows → connect would
    // die "No session matching <q>" on exactly the sessions it is supposed to
    // revive. include_all=true lifts that filter. Tombstoned rows stay EXCLUDED
    // (connect's pre-existing posture — Pete: don't widen connect to killed
    // sessions; resume's include_tombstoned is resume's own call).
    // D-2: resolve against the FULL universe through the sealed uncapped entry, so
    // connect targets anything resume can — incl. a COLD / auto-named session far
    // outside the `ls` display cap. Tombstones resolve too; connect then REJECTS a
    // stopped session post-resolve with the clear "resume it first" message. (This
    // REVERSES connect's pre-existing "tombstones excluded" posture — D-2 makes it
    // resolve-then-reject so the error teaches, never a phantom `No session matching`.)
    let session = match common::resolve_session_uncapped(query) {
        Ok(s) => s,
        Err(code) => return code,
    };
    if let Err(code) = common::reject_if_tombstoned(query, &session) {
        return code;
    }
    // The sealed entry returns an OWNED Session; the attach/revive pipeline below is
    // borrow-shaped (it took `&Session` from the old slice-borrowing resolver), so
    // re-borrow here — the mechanical owned→ref tweak the plan flagged.
    let session = &session;

    match lifecycle::attach_resolved("connect", session) {
        AttachOutcome::Done(code) => code,
        // W1 phase 2: a COLD claude session auto-revives, then we attach the live pane.
        AttachOutcome::Cold => match resume::revive_claude(session, None, render) {
            Ok(handle) => {
                // Attach the now-live pane with a plain mux.attach (NO fused
                // `zmx attach … bash -lc` — the session is already up).
                println!("Revived \"{}\"; attaching...", handle.zmx_name);
                let mux = match common::real_mux() {
                    Ok(m) => m,
                    Err(code) => return code,
                };
                let dir: PathBuf = handle.socket_dir;
                match mux.attach(&dir, &handle.zmx_name) {
                    Ok(code) => code,
                    Err(e) => {
                        eprintln!("qd connect: {e}");
                        1
                    }
                }
            }
            // revive_claude already printed its own loud, `qd connect:`-prefixed
            // error explaining the failure. Return that code as-is — do NOT append
            // the cold-error pointer (it says "revive with `qd connect`", i.e. re-run
            // the command that just failed — circular/confusing on the human verb).
            Err(code) => code,
        },
    }
}
