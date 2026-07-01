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

use dispatch::model::SessionStatus;

use super::common;
use super::lifecycle;
use super::lifecycle::AttachOutcome;
use super::resume;

/// `qd connect <session>` — resolve the row, then hand to the shared attach
/// mechanic (provider dispatch + cold-vs-live). No `--json` (interactive verb).
pub fn run(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    // --no-attach (headless revive): revive a COLD session into a PERSISTENT,
    // relay-serving daemon and return 0 WITHOUT attaching a TTY — the entry a
    // systemd ExecStart calls. UNSET ⇒ the block below is skipped entirely and the
    // interactive attach path stays byte-identical to today.
    let no_attach = m.get_flag("no-attach");
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

    // --no-attach: revive-to-persistent-daemon (or report already-live) and return
    // 0 WITHOUT ever attaching a TTY. We branch BEFORE `attach_resolved` because
    // that shared mechanic attaches a live pane INTERNALLY before returning — so
    // honoring "do not attach" means never entering it for a live row. A COLD row
    // (status Cold; Killed rows were rejected above) routes to the SAME
    // `revive_claude` seam connect uses today, then we SKIP `mux.attach`.
    if no_attach {
        // PROVIDER GUARD: the --no-attach revive/report logic is claude-code-SHAPED
        // — `revive_claude` builds a `claude … server:relay --resume <sid>` argv, and
        // the already-live branch below assumes a mux pane serving the relay. A
        // codex / acp/* / opencode / daemon-hosted (non-claude-code) row would
        // MIS-ROUTE: a Cold row fed into `revive_claude` (wrong argv shape), a live
        // one falsely reported "already live". Every claude-code row carries the
        // literal provider "claude-code" (model.rs: the join defaults absent-on-disk
        // to it); codex ("codex"), acp ("acp/*"), opencode, and unknown are all a
        // DIFFERENT provider/hosting (see lifecycle::attach_resolved / kill.rs
        // dispatch). Refuse LOUDLY and return non-zero BEFORE the revive/report
        // logic — never proceed for a non-claude-code session.
        if session.provider != "claude-code" {
            let name = session.name.as_deref().unwrap_or(&session.session_id);
            eprintln!(
                "qd connect --no-attach: \"{name}\" is a {} session; --no-attach supports only claude-code sessions.",
                session.provider
            );
            return 1;
        }
        // Already-live claude pane already serves the relay — report and stop; do
        // NOT attach (idle/busy/shell are the live states; cold falls through).
        if matches!(
            session.status,
            SessionStatus::Idle | SessionStatus::Busy | SessionStatus::Shell
        ) {
            let name = session.name.as_deref().unwrap_or(&session.session_id);
            println!("\"{name}\" is already live (persistent); not attaching.");
            return 0;
        }
        return match resume::revive_claude(session, None, render) {
            Ok(handle) => {
                println!("Revived \"{}\" (persistent, no attach)", handle.zmx_name);
                0
            }
            // revive_claude already printed its own loud `qd connect:`-prefixed error.
            Err(code) => code,
        };
    }

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
