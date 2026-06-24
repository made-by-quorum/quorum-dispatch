//! `sb connect <session>` (ADD-26, W1 phase 1) — the human "get me into this
//! session" verb. Dispatches on the row's PROVIDER HOSTING first, then liveness:
//!
//!   - `Hosting::Daemon` (codex) → LOUD redirect (no terminal to attach), exit 1.
//!   - `Hosting::MuxPane` (claude) → live → reuse the shared attach mechanic;
//!     cold → AUTO-REVIVE (W1 phase 2) then attach the live pane. Revive FAILS →
//!     the SHARED cold-error pointing at `sb connect`, exit 1.
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

use dispatch::join::JoinOpts;

use super::common;
use super::common::resolve_or_die;
use super::lifecycle;
use super::lifecycle::AttachOutcome;
use super::resume;

/// `sb connect <session>` — resolve the row, then hand to the shared attach
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
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: false,
        include_preview: false,
        limit: Some(50),
    };
    let sessions = match common::all_sessions(opts) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let session = match resolve_or_die(query, &sessions) {
        Ok(s) => s,
        Err(code) => return code,
    };

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
                        eprintln!("sb connect: {e}");
                        1
                    }
                }
            }
            // revive_claude already printed its own loud, `sb connect:`-prefixed
            // error explaining the failure. Return that code as-is — do NOT append
            // the cold-error pointer (it says "revive with `sb connect`", i.e. re-run
            // the command that just failed — circular/confusing on the human verb).
            Err(code) => code,
        },
    }
}
