//! REAL lifecycle backends: attach / new / info / live (spec §3).

use std::path::PathBuf;

use clap::ArgMatches;

use std::cell::Cell;

use dispatch::boot::{RealSleeper, Sleeper};
use dispatch::create::{run_new as create_run_new, NewDeps, NewError, NewParams};
use dispatch::effects::{Clock, Env, RealClock, RealEnv};
use dispatch::events::{self, Anchor, EventWriter, Payload, WatchGuard};
use dispatch::exec::RealExec;
use dispatch::join::JoinOpts;
use dispatch::launch::capture_backend_env;
use dispatch::model::{Session, SessionStatus};
use dispatch::mux::Mux;
use dispatch::zmx_dir::{legacy_zmx_dirs, resolve_zmx_dir, XdgFamily};

use super::common;
use super::common::resolve_or_die;

// --- attach mechanic (commands/lifecycle.ts:355-395) ---
// P0 start-surface rework (STATE 22): the `attach` VERB is a retired erroring
// stub (verbs/stubs.rs; was ADD-26 demoted/hidden). The shared attach MECHANIC
// below stays — `connect` is its one caller now.

/// The cold-vs-done outcome of [`attach_resolved`] (W1 phase 2). A `MuxPane`
/// (claude) session with no live pane is `Cold` — the SHARED mechanic returns it to
/// the CALLER instead of deciding: `connect` maps it to auto-revive-then-attach.
/// Every other path (opencode parked, daemon redirect, unknown-provider refusal,
/// collision refusal, live attach) is a terminal `Done(code)`.
pub enum AttachOutcome {
    Done(i32),
    Cold,
}

/// The shared attach mechanic (ADD-26): provider dispatch + the zmx live-handoff,
/// called by `connect::run` (its one caller since the attach-verb retirement,
/// STATE 22). `verb` names the caller for the opencode/unknown-provider wording.
///
/// Dispatch order: collision refusal → opencode (parked) → daemon redirect (codex)
/// → unknown-provider refusal → cold-vs-live. Cold (`MuxPane`, no live pane) is
/// returned to the CALLER as [`AttachOutcome::Cold`] (W1 phase 2): `connect`
/// maps it to auto-revive-then-attach. All other outcomes are terminal
/// [`AttachOutcome::Done`].
pub fn attach_resolved(verb: &str, session: &Session) -> AttachOutcome {
    // ADD-8 residual fix (W1 phase 2) — live-id-collision PREFLIGHT over the RAW
    // registry, SHARED with resume (Pete feedback #6). The deduped join collapses
    // two same-id LIVE rows to one, so a bare `sb attach`/`sb connect` would
    // silently attach to the deduped survivor. We refuse a genuine ≥2-alive
    // collision LOUDLY here (before provider dispatch) so BOTH connect and demoted
    // attach inherit the guard. A 1-alive session is NOT refused (refuse_id_collision
    // returns None for the single-alive case → normal attach proceeds; we do NOT use
    // alive_pid_for_id, which would block the legitimate single-live attach).
    {
        let env = RealEnv;
        if let Ok(paths) = common::paths_from_home(&env) {
            if let Some(code) =
                common::refuse_id_collision(verb, &session.session_id, &paths.sessions_dir)
            {
                return AttachOutcome::Done(code);
            }
        }
    }
    // OpenCode attach (commands/lifecycle.ts:362-376) is parked (OpenCode ruling).
    if session.provider == "opencode" {
        eprintln!("sb {verb}: OpenCode attach is not supported in the Rust engine (parked).");
        return AttachOutcome::Done(1);
    }
    // codex (Hosting::Daemon) IS supported but has no terminal to attach — the
    // shared LOUD redirect, NOT the wrong "unknown provider" refusal (latent fix).
    if let Some(p) = dispatch::provider::provider_for(&session.provider) {
        if p.hosting() == dispatch::provider::Hosting::Daemon {
            let name = session.name.as_deref().unwrap_or(&session.session_id);
            return AttachOutcome::Done(common::daemon_redirect(name));
        }
    }
    // Unknown provider (not claude/opencode/codex) → refuse LOUDLY.
    if let Some(code) = common::refuse_unknown_provider(verb, session) {
        return AttachOutcome::Done(code);
    }

    // WP-B5-i — the LIVE connect resolver (the deferred B-CS-2 wiring). A headless
    // (agent-driven) claude session has NO mux pane, so the cold check below would
    // misread a LIVE one as Cold and try to revive it. Resolve the row's mode from
    // its `entrypoint` discriminant + liveness FIRST: a live headless target →
    // `ConnectAction::Observe` (read-only dashboard). Interactive/cold targets fall
    // through to today's pane-attach / cold-revive unchanged.
    {
        let env = RealEnv;
        if let Ok(paths) = common::paths_from_home(&env) {
            let entrypoint = session
                .pid
                .and_then(|p| dispatch::registry::read_entry(&paths.sessions_dir, p))
                .and_then(|e| e.entrypoint);
            let pid_alive = session
                .pid
                .is_some_and(|p| p != 0 && dispatch::effects::is_pid_alive(p as i32));
            let has_live_pane = session.zmx_name.is_some();
            let mode = dispatch::observe::resolve_target_mode(
                entrypoint.as_deref(),
                pid_alive,
                has_live_pane,
            );
            if dispatch::observe::connect_dispatch(mode)
                == dispatch::observe::ConnectAction::Observe
            {
                return AttachOutcome::Done(run_headless_observe(session));
            }
        }
    }

    // MuxPane (claude) cold → return Cold to the CALLER (W1 phase 2): no business
    // branch on the `verb` string here. connect → auto-revive-then-attach.
    let Some(zmx_name) = session.zmx_name.as_deref() else {
        return AttachOutcome::Cold;
    };

    // Target the session's recorded socket dir (Bug D), falling back to canonical
    // when it has none (commands/lifecycle.ts:387-391: `session.socketDir ?? canonicalZmxDir()`).
    let env = RealEnv;
    let canonical = resolve_zmx_dir(&env);
    let dir = session
        .socket_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or(canonical);

    // Backend-selected mux (C1 D3). An attachable session carries its socket_dir
    // (tagged by the backend's list), so the embedded lane targets the sbmux dir.
    let mux = match common::real_mux() {
        Ok(m) => m,
        Err(code) => return AttachOutcome::Done(code),
    };
    AttachOutcome::Done(match mux.attach(&dir, zmx_name) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sb {verb}: {e}");
            1
        }
    })
}

/// WP-B-CS-2-LIVE — the live OBSERVE run-loop `sb connect` lands on for a live
/// headless target (replacing B5-i's one-shot snapshot). The full interactive
/// surface, wired to the banked `observe.rs` primitives:
///   1. LIVE RE-RENDER of the read-only dashboard — CONTROL FACTS ONLY (§2a): each
///      `Republish*` control fact off the SAME socket-republish stream `sb wait`
///      reads (the observe-armed [`ChannelSubscriber`]) folds into
///      [`DashboardState`] and re-renders. No content path exists, so §2a holds by
///      construction.
///   2. THE 1/2/3 MENU — relay (async, non-disruptive) / cut-over-to-drive (with an
///      optional buffered input) / just-watch.
///   3. TURN-BOUNDARY CUTOVER EXECUTION — choice 2 arms the [`CutoverGate`]; the
///      run-loop drives `gate.observe` per frame (via [`observe_tick`]) and fires
///      EXACTLY at a turn boundary (never mid-turn), then runs the ordered
///      execution: tear down the headless session → `revive_claude` the SAME claude
///      session into a native TUI → deliver the buffered input FIRST → attach.
fn run_headless_observe(session: &Session) -> i32 {
    use dispatch::observe::{
        execute_cutover, menu_action, menu_text, observe_tick, parse_menu_choice, CutoverDecision,
        CutoverGate, MenuChoice, ObserveAction,
    };
    use std::io::BufRead;
    use std::sync::mpsc::TryRecvError;
    use std::time::Duration;

    let env = RealEnv;
    let name = session.name.as_deref().unwrap_or(&session.session_id);
    let paths = match common::paths_from_home(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let dir = match dispatch::sbmux_dir::resolve_sbmux_dir(&paths.home, &env) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("sb connect: {e}");
            return 1;
        }
    };
    let socket_path = match sbmux::server::session_socket_path_for(Some(&dir), name) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sb connect: {e}");
            return 1;
        }
    };
    println!("Observing headless session \"{name}\" (read-only — CONTROL FACTS ONLY).");
    println!("{}", menu_text());

    // Subscribe with the observe-armed TAP so the run-loop can drive the cutover
    // gate from the live `Republish*` stream (the SAME path `sb wait` reads).
    let sub =
        dispatch::wait_channel::ChannelSubscriber::connect_observing(socket_path, name.to_string());
    // Settle the channel (subscribe + first frame) before the first render.
    let _ = sub.await_status(Duration::from_secs(2));

    // SEAM (b) — seed the in-flight busy baseline from the AUTHORITATIVE
    // daemon-written ROW STATUS (a control fact). A subscriber that connects DURING
    // a busy turn receives NO replayed in-flight frame (the republish hub forwards
    // only NEW frames; the turn-start `Ready` already fired before we subscribed),
    // so `dashboard_snapshot().in_turn` reads FALSE mid-turn — the busy render would
    // be missed and a choice-2 cutover would mis-fire immediately instead of
    // deferring. The row's `status` is the SAME control fact the resolver already
    // read to route here; we fold it as a synthetic `RepublishStatus{busy}` through
    // the control-facts-only path (§2a-safe by construction — busy/idle is never
    // assistant text), seeding BOTH the rendered dashboard AND the cutover gate.
    // Live frames off the tap then keep both authoritative (the real `TurnEnd` flips
    // in-turn off and IS the boundary the deferred cutover fires at).
    let row_busy = session
        .pid
        .and_then(|p| dispatch::registry::read_entry(&paths.sessions_dir, p))
        .and_then(|e| e.status)
        .is_some_and(|s| s.eq_ignore_ascii_case("busy"));

    let mut dash = dispatch::observe::DashboardState::default();
    let mut gate = CutoverGate::default();
    if row_busy {
        let busy = sbmux::protocol::ServerMsg::RepublishStatus {
            status: "busy".to_string(),
        };
        dash.apply(&busy);
        gate.observe(&busy);
    }
    let mut last_render = String::new();
    let render_if_changed = |dash: &dispatch::observe::DashboardState, last: &mut String| {
        let out = dash.render(None);
        if out != *last {
            println!("{out}");
            *last = out;
        }
    };
    render_if_changed(&dash, &mut last_render);

    // Operator stdin on a background thread, polled non-blocking from the loop so a
    // blocking read never stalls the live re-render / cutover firing.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // EOF: dropping `tx` disconnects the channel (the loop sees Disconnected).
    });

    // Two-line stdin protocol: a menu digit, then (after "2") the optional buffer.
    let mut awaiting_buffer = false;
    let mut stdin_open = true;
    let mut exec = RealCutoverExec { session };

    loop {
        // (1) Drain new control facts → re-render + drive the cutover gate. The
        //     cutover fires EXACTLY at a turn boundary (observe_tick IS the gate).
        for frame in sub.drain_control_frames() {
            // Fold each live control fact into BOTH the rendered dashboard and the
            // cutover gate (§2a: the tap carries control facts only).
            dash.apply(&frame);
            if let Some(fire) = observe_tick(&mut gate, &frame) {
                // Render the boundary state (the turn going idle) BEFORE handing the
                // wheel over, so the operator sees the turn complete at the moment of
                // cutover (the boundary frame both flips in-turn off AND fires).
                render_if_changed(&dash, &mut last_render);
                println!("Turn boundary reached — cutting over to drive...");
                return into_code(execute_cutover(fire, &mut exec));
            }
        }
        render_if_changed(&dash, &mut last_render);

        // (2) Operator input (non-blocking).
        match rx.try_recv() {
            Ok(line) if awaiting_buffer => {
                awaiting_buffer = false;
                let buffered = {
                    let t = line.trim();
                    (!t.is_empty()).then(|| t.to_string())
                };
                match menu_action(MenuChoice::CutOverToDrive, &mut gate, buffered) {
                    ObserveAction::CutOver(CutoverDecision::FireNow) => {
                        let fire = gate.poll_fire().expect("FireNow yields a fire");
                        println!("At a boundary — cutting over to drive now...");
                        return into_code(execute_cutover(fire, &mut exec));
                    }
                    ObserveAction::CutOver(CutoverDecision::DeferredToBoundary) => {
                        println!(
                            "Cutover queued — it fires at the next turn boundary (never mid-turn)."
                        );
                    }
                    _ => {}
                }
            }
            Ok(line) => match parse_menu_choice(&line) {
                Some(MenuChoice::RelayMessage) => {
                    println!(
                        "To message this agent WITHOUT disrupting it (async; it keeps working), \
                         run:\n  sb send:relay {name} \"<your message>\""
                    );
                }
                Some(MenuChoice::CutOverToDrive) => {
                    awaiting_buffer = true;
                    println!(
                        "Enter input to BUFFER for the cutover (delivered first when you take \
                         the wheel), or press Enter to cut over with none:"
                    );
                }
                Some(MenuChoice::JustWatch) => {
                    println!("Watching (read-only). Press 2 to cut over, 1 to relay.");
                }
                None => {
                    println!("Unrecognized choice.");
                    println!("{}", menu_text());
                }
            },
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => stdin_open = false,
        }

        // (3) Termination: the stream ended or the operator detached, and no cutover
        //     is outstanding (a pending cutover fires above at the boundary / End).
        if (dash.ended.is_some() || !stdin_open) && !gate.is_pending() {
            if let Some(outcome) = &dash.ended {
                println!("Session stream ended: {outcome}.");
            }
            return 0;
        }

        std::thread::sleep(Duration::from_millis(80));
    }
}

/// Collapse [`execute_cutover`]'s `Result<i32, i32>` (Ok=attach exit code, Err=the
/// loud-printed failure code) to the verb's process exit code.
fn into_code(r: Result<i32, i32>) -> i32 {
    match r {
        Ok(code) => code,
        Err(code) => code,
    }
}

/// The PRODUCTION [`dispatch::observe::CutoverExec`] — the real teardown + revive +
/// buffered-input delivery + attach a fired cutover performs. The ORDERING and the
/// never-mid-turn gate are proven deterministically against a fake exec in
/// `observe.rs`; THIS wires the steps to real effects (the seam the real-claude
/// seed drives end-to-end).
struct RealCutoverExec<'a> {
    session: &'a Session,
}

impl dispatch::observe::CutoverExec for RealCutoverExec<'_> {
    type Handle = super::resume::ReviveHandle;
    type Err = i32;

    fn teardown_headless(&mut self) -> Result<(), i32> {
        // Reap the live headless claude child (and its descendant subtree) so the
        // SAME claude session can be revived natively without two producers racing
        // the one transcript. The B5-i row is CHILD-PID-keyed, so `session.pid` IS
        // the live claude child (proven by `headless_cli_addressability`).
        let pid = self.session.pid.unwrap_or(0);
        if pid > 0 {
            reap_process_subtree(pid as i32);
        }
        Ok(())
    }

    fn revive_to_drive(&mut self) -> Result<Self::Handle, i32> {
        // Resume the SAME claude session id into a detached, drivable native pane
        // (the shared cold→drivable revive; resumes by the row's recorded id).
        super::resume::revive_claude(self.session, None, dispatch::launch::RenderMode::Inline)
    }

    fn deliver_buffered_input(
        &mut self,
        handle: &Self::Handle,
        input: Option<&str>,
    ) -> Result<(), i32> {
        // Buffered-input-FIRST: deliver the queued text to the revived pane BEFORE
        // the human takes the wheel. A `None` buffer is a no-op (the slot still runs
        // ahead of attach — the ordering the `execute_cutover` test pins).
        let Some(text) = input else { return Ok(()) };
        let env = RealEnv;
        let paths = common::paths_from_home(&env)?;
        let mux = common::real_mux()?;
        let clock = RealClock;
        let sleeper = RealSleeper;
        let deps = dispatch::submit::RealDeliverDeps {
            mux: mux.as_ref(),
            clock: &clock,
            sleeper: &sleeper,
            zmx_name: handle.zmx_name.clone(),
            session_name: handle.zmx_name.clone(),
            sessions_dir: paths.sessions_dir.clone(),
            dir: handle.socket_dir.clone(),
        };
        let _ = dispatch::submit::deliver_prompt(&deps, text, dispatch::submit::DELIVER_TIMEOUT_S);
        Ok(())
    }

    fn attach(&mut self, handle: Self::Handle) -> Result<i32, i32> {
        // Hand the human the wheel: attach the now-live native pane (a plain
        // mux.attach — the session is already up). This is the step whose physical
        // native-TUI/terminal attach the real-claude seed probes for the env
        // boundary (Q2b) — the LOGIC above is all proven on-box.
        let mux = common::real_mux()?;
        match mux.attach(&handle.socket_dir, &handle.zmx_name) {
            Ok(code) => Ok(code),
            Err(e) => {
                eprintln!("sb connect: {e}");
                Err(1)
            }
        }
    }
}

/// Best-effort reap of a live headless session's process subtree: the recorded
/// claude child `pid` and its descendants, each `(pid, start-time)`-verified by
/// [`dispatch::effects::kill_pid_tree`]. Scoped mirror of `sb stop`'s descendant sweep —
/// the child MUST be dead before `revive_claude` resumes the SAME claude session,
/// or two claude processes race the one transcript. Self is excluded
/// (`descendant_kill_list` never sweeps toward the caller).
fn reap_process_subtree(pid: i32) {
    use dispatch::exec::Exec;
    let exec = RealExec;
    let rows = exec
        .run(
            "ps",
            &["-eo".to_string(), "pid=,ppid=,command=".to_string()],
            &[],
            None,
            None,
        )
        .ok()
        .map(|r| dispatch::effects::parse_ps_rows(&r.stdout));
    let self_pid = std::process::id() as i32;
    let victims: Vec<(i32, i64)> = rows
        .as_ref()
        .map(|rows| {
            dispatch::kill::descendant_kill_list(pid, self_pid, rows)
                .into_iter()
                .filter_map(|p| dispatch::effects::proc_start_ms(p).map(|s| (p, s)))
                .collect()
        })
        .unwrap_or_default();
    // Descendants first, then the child root (SIGTERM→SIGKILL with a short grace).
    dispatch::effects::kill_pid_tree(&victims, 1500);
    dispatch::effects::kill_pid(pid, 1500);
}

// --- new (A2 run_new path, commands/lifecycle.ts:707-809) ---

/// `sb start <name> [claudeArgs...]` (P0 W1: today's `new` renamed, sbx
/// spec-cli §11; the retired `new` verb errors in verbs/stubs.rs and never
/// reaches this backend) — A2's detached create (run_new), now WITH
/// A4 `-p/--prompt` + `--model` DELIVERY (spec §3.4) and the went-busy EXIT
/// CONTRACT (§3.5: 0=accepted, 10=stalled, 1=infra/other). `--attach`
/// DEFERRED→A5; `--provider opencode`/`--port` DEFERRED (parked); A6 makes
/// `--via <name>` LIVE — F1 caller-capture + backends.json profile composition
/// thread the backend env into create (spec §2.2 + §3.2).
/// punch item 11: the fork-target transcript preflight. Returns `Some(error)`
/// when the target has NO transcript on disk (claude: `transcript_path` over
/// the projects-dir for that sid), `None` when the fork is legal. Status-blind
/// by design: a tombstoned (stopped) session WHOSE TRANSCRIPT EXISTS is a
/// legal fork source (the fork reads the transcript, not the process) —
/// pinned by unit test.
///
/// S4: the root is `paths.projects_dir` DIRECTLY — by this point a codex fork
/// is refused upstream and same-provider has run, so only claude reaches here;
/// passing the claude root matches the three existing claude-site call sites
/// (wait.rs / send.rs / join.rs). The provider-generic `transcript_root(fx)`
/// seam returns if codex forks ever become legal.
fn fork_transcript_missing_error(
    provider_impl: &dyn dispatch::provider::Provider,
    paths: &dispatch::paths::SbPaths,
    target: &Session,
) -> Option<String> {
    let root = &paths.projects_dir;
    // F4 (red-team r1): an EXISTING-but-unenumerable projects root (permission
    // failure) must not produce a FALSE "no transcript exists" claim —
    // find_jsonl_path maps a read_dir error to not-found. The preflight is an
    // improvement layer, never a gate that lies: SKIP it (fail-open to the
    // pre-B1 behavior — the fork proceeds and any genuine absence surfaces
    // downstream). An ABSENT root keeps refusing: "no transcript exists" is
    // then a true statement (claude never wrote one).
    if root.exists() && std::fs::read_dir(root).is_err() {
        return None;
    }
    let key = dispatch::provider::SessionKey {
        id: &target.session_id,
        name: target.name.as_deref(),
        cwd: target.cwd.as_deref(),
        pid: target.pid,
    };
    if provider_impl.transcript_path(root, &key).is_some() {
        return None;
    }
    let display = target.name.as_deref().unwrap_or(&target.session_id);
    Some(format!(
        "sb start: cannot fork \"{display}\" — no transcript exists for session id \
         {} (looked under {}). The session never produced a transcript (it may have \
         been created but never used). Pick another --fork source, or start fresh \
         without --fork.",
        target.session_id,
        root.display()
    ))
}

/// WP-B5-iii Mechanism S (`FORK-IDENTITY-SPEC.md` §3/§4/§5a): seed a forked
/// transcript for `target` at a FRESH fork UUID under `cwd`'s projects slug, and
/// return `(fork_uuid, optional staleness notice)`. The fork then launches with
/// `--resume <fork_uuid>` in `cwd` — claude resolves `--resume` purely by
/// `<cwd-slug>/<uuid>.jsonl` (no session index; `WP-B5iii-TRANSCRIPT-TRACE.md`
/// exp F-SEED), so the seed must live under the FORK's launch-cwd slug. The
/// parent transcript is read O_RDONLY only (never mutated). Identity rides the
/// pre-minted `fork_uuid`: the caller `mint_or_get(fork_uuid)`s the fork's OWN
/// sbId (option A — never the parent's). Mechanism S is uniform (default + the
/// `--turn` rewind) per the lead's Q4-FOLLOWUP ruling; it touches ZERO banked
/// daemon/sbmux/protocol/argv surface (the fidelity gate proves it equivalent to
/// native `--fork-session`).
fn seed_fork_transcript(
    provider_impl: &dyn dispatch::provider::Provider,
    paths: &dispatch::paths::SbPaths,
    cwd: &std::path::Path,
    target: &Session,
    turn: Option<usize>,
    env: &dyn Env,
) -> Result<(String, Option<String>), i32> {
    let display = target
        .name
        .as_deref()
        .unwrap_or(&target.session_id)
        .to_string();
    // Locate the parent's DURABLE transcript via the provider seam (the same
    // resolver the transcript-existence preflight used).
    let key = dispatch::provider::SessionKey {
        id: &target.session_id,
        name: target.name.as_deref(),
        cwd: target.cwd.as_deref(),
        pid: target.pid,
    };
    let Some(parent_path) = provider_impl.transcript_path(&paths.projects_dir, &key) else {
        eprintln!("sb start: cannot fork \"{display}\" — its transcript is unreadable.");
        return Err(1);
    };
    let text = match std::fs::read_to_string(&parent_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "sb start: cannot read the source transcript at {}: {e}.",
                parent_path.display()
            );
            return Err(1);
        }
    };
    let records = dispatch::fork_seed::parse_records(&text);
    let point = match turn {
        Some(n) => dispatch::fork_seed::ForkPoint::Turn(n),
        None => dispatch::fork_seed::ForkPoint::Latest,
    };
    let resolved = match dispatch::fork_seed::resolve(&records, point) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("sb start: cannot fork \"{display}\" — {e}");
            return Err(1);
        }
    };
    // Option A: the fork's session-id is known PRE-spawn (we mint it), so the
    // seed is named by it AND the fork's own sbId is minted from it.
    let fork_uuid = dispatch::fork_seed::new_fork_uuid();
    let seed = dispatch::fork_seed::rekey_truncate(&records, resolved.boundary_idx, &fork_uuid);
    // Write under the FORK's launch-cwd slug (claude resolves --resume by it).
    let slug = dispatch::jsonl::cwd_to_project_path(&cwd.to_string_lossy());
    let dir = paths.projects_dir.join(slug);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "sb start: cannot create projects dir {}: {e}",
            dir.display()
        );
        return Err(1);
    }
    let seed_path = dir.join(format!("{fork_uuid}.jsonl"));
    if let Err(e) = std::fs::write(&seed_path, dispatch::fork_seed::to_jsonl(&seed)) {
        eprintln!(
            "sb start: cannot write the forked transcript {}: {e}",
            seed_path.display()
        );
        return Err(1);
    }
    // WP-B5-iii obl-4: record the fork→parent lineage pointer (the PARENT's sbId,
    // keyed by the fork's uuid — works for BOTH the interactive and headless
    // launch paths, no daemon/protocol change). Best-effort + additive: a lineage
    // hiccup never fails the launch; only recorded when the parent has a stable
    // sbId. STRICTLY the parent pointer — the fork's OWN sbId is minted elsewhere.
    if let Some(parent_sb) = target.sb_id.as_deref() {
        if let Ok(ids_path) = common::ids_store_path(env) {
            if let Err(e) =
                dispatch::idstore::record_lineage(&ids_path, &fork_uuid, parent_sb, &RealClock)
            {
                eprintln!("sb start: WARNING — fork lineage not recorded: {e}");
            }
        }
    }
    Ok((fork_uuid, resolved.staleness_report()))
}

pub fn run_new(m: &ArgMatches) -> i32 {
    let env = RealEnv;
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("sb start: HOME is not set — cannot resolve the session state dir.");
            return 1;
        }
    };

    let name = m
        .get_one::<String>("name")
        .expect("required by clap")
        .clone();
    let cwd_opt = m.get_one::<String>("cwd").map(PathBuf::from);
    // P0 start-surface rework (STATE 21 ruling): `--resume` is GONE from start
    // (the resume verb owns same-participant wake); `--fork <session>` is the
    // one transcript-seeded start — a NEW participant forked from an existing
    // session's transcript. The value is resolved below via the standard
    // target pipeline (name | full bond id | unambiguous prefix).
    let fork_target = m.get_one::<String>("fork").cloned();
    // WP-B5-iii (FORK-IDENTITY-SPEC §4): `--turn N` = REWIND-ONLY (1-based), only
    // meaningful with `--fork`. Omitted ⇒ latest safe boundary. A non-positive or
    // non-numeric value, or `--turn` without `--fork`, is an error-that-teaches.
    let fork_turn: Option<usize> = match m.get_one::<String>("turn") {
        None => None,
        Some(s) => match s.parse::<usize>() {
            Ok(n) if n >= 1 => Some(n),
            _ => {
                eprintln!(
                    "sb start: --turn must be a positive integer (the conversational-turn \
                     ordinal to rewind the fork to)."
                );
                return 1;
            }
        },
    };
    if fork_turn.is_some() && fork_target.is_none() {
        eprintln!(
            "sb start: --turn is only valid together with --fork (it rewinds the fork to a \
             past conversational-turn boundary)."
        );
        return 1;
    }
    let attach = m.get_flag("attach");
    // WP-B-CS-1 (D2): the driver-mode override flags (auto-detect escape hatch).
    let headless_flag = m.get_flag("headless");
    let interactive_flag = m.get_flag("interactive");
    let agent = m.get_one::<String>("agent").cloned();
    // `sb start --agent <name>` is RETIRED. The old static-agent path resolved
    // `~/.sb/plugins/core/agents/<name>.md` and fail-closed booted that role;
    // role/agent CONTENT now lives in the work-model plugin and spawning is
    // `bond commission <role>.md`, never the engine's native flag. Refuse here —
    // BEFORE preflight/claim/launch, so nothing is created — with the teaching
    // error (the new/kill/attach retirement pattern). NOTHING uses --agent.
    if agent.is_some() {
        return super::stubs::run_start_agent_retired();
    }
    let prompt = m.get_one::<String>("prompt").cloned();
    let model = m.get_one::<String>("model").cloned();
    let provider = m.get_one::<String>("provider").cloned();
    let port = m.get_one::<String>("port").cloned();
    let claude_args: Vec<String> = m
        .get_many::<String>("claudeArgs")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    // --via <name> (spec §3.2): now LIVE (no longer a no-op). Resolved against
    // backends.json below; threads the composed backend env into create.
    let via = m.get_one::<String>("via").cloned();
    // punch item 7: per-session render mode (flag > render-default config >
    // inline). Resolved here, injected as a birth property by create.rs via the
    // shared launch_env_pairs assembly. Meaningless for the codex daemon path
    // (no mux pane to render) — silently unused there.
    let render = common::resolve_render_mode(m, &env);

    // Deferred surfaces: ACCEPT at parse (above), then honest error AFTER parse
    // (spec §3 row 5 — this changes A2's reject-at-parse to accept-then-honest-error).
    if provider.as_deref() == Some("opencode") || port.is_some() {
        eprintln!(
            "sb start: --provider opencode / --port are not yet supported in the Rust engine \
             (parked OpenCode)."
        );
        return 1;
    }
    // codex P1, R1 (codex-p1-spec section 3.3): FAIL-CLOSED on an unknown
    // --provider value (orc-14-ENDORSED change; ADD-13(4); Hardening-#1 pattern).
    // Today's silent fall-through boots claude on garbage input; this exits 1
    // BEFORE preflight/claim (no state). None / "claude-code" / "codex" proceed
    // (the opencode/--port honest-error above stays FIRST + byte-identical). codex
    // P2 W4: codex is now a supported value (GATE-R RULED (A) daemon-thread).
    if let Some(p) = provider.as_deref() {
        if p != "claude-code" && p != "codex" {
            eprintln!(
                "sb start: unknown provider \"{p}\" — this engine supports: claude-code, codex \
                 (--provider opencode is parked)."
            );
            return 1;
        }
    }
    // codex P1 W3 (codex-p1-spec section 7.1 step 2): resolve the provider ONCE
    // from the validated value (None ⇒ "claude-code"). The W1 fail-closed check
    // above already guarantees the value is "claude-code" here, so `provider_for`
    // resolves; the defensive None arm re-prints the SAME loud unknown-provider
    // error rather than panicking (it is structurally unreachable given the check,
    // but a fail-closed exit is the only honest posture if the two ever drift).
    let provider_id = provider.as_deref().unwrap_or("claude-code");
    let Some(provider_impl) = dispatch::provider::provider_for(provider_id) else {
        eprintln!(
            "sb start: unknown provider \"{provider_id}\" — this engine supports: claude-code, codex \
             (--provider opencode is parked)."
        );
        return 1;
    };
    if attach {
        eprintln!("sb start: --attach is not yet supported in the Rust engine (A5).");
        return 1;
    }
    // P0 qafix R2 (orc ruling 2026-06-10), kept across the start-surface rework:
    // codex start has NO transcript seed — `run_new_codex_daemon` never receives
    // --fork, so it used to be DROPPED silently pre-validation. Errors-that-teach:
    // refuse loudly, naming what codex doesn't support and the working revive
    // path. (The R2 --resume refusal arm died with the flag itself — `start
    // --resume` is now an unknown option at parse.)
    if provider_id == "codex" && fork_target.is_some() {
        eprintln!(
            "sb start: --fork is not supported with --provider codex — codex start \
             always begins a new thread (no transcript branching). To revive a stopped \
             codex session, use \"sb resume <name>\"."
        );
        return 1;
    }
    // -p/--prompt + --model delivery now LANDS (spec §3.4) — no more honest error.

    let cwd =
        cwd_opt.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let paths = dispatch::paths::SbPaths::from_home(&home);

    // --- P0 start-surface rework: resolve the `--fork <session>` target -------
    // The standard target pipeline (common::all_sessions + resolve_or_die — the
    // same loud, house-shaped ambiguity/not-found errors resume/connect print).
    // JoinOpts mirror resume's (include_all so an auto-named/cold target
    // resolves; include_tombstoned so a stopped session's transcript is
    // forkable). The resolved session's provider UUID seeds the forked launch
    // (claude: `--resume <uuid> --fork-session`); the forked boot mints a NEW
    // provider UUID, so identity is always mint_unbound + bind-at-boot-confirm
    // below (the qafix R1 live-collision preflight is gone for start: forking a
    // LIVE session is legal and safe — a new UUID, a new participant).
    let (fork_uuid, fork_staleness): (Option<String>, Option<String>) = match fork_target.as_deref()
    {
        None => (None, None),
        Some(query) => {
            let opts = JoinOpts {
                include_all: true,
                include_tombstoned: true,
                include_preview: false,
                limit: Some(50),
            };
            let sessions = match common::all_sessions(opts) {
                Ok(s) => s,
                Err(code) => return code,
            };
            let target = match resolve_or_die(query, &sessions) {
                Ok(s) => s,
                Err(code) => return code,
            };
            // A ZmxOnly row (pane with no registry/transcript identity) carries
            // an EMPTY session_id — there is no transcript to fork.
            if target.session_id.is_empty() {
                eprintln!(
                    "sb start: session \"{}\" has no provider session id — nothing to fork.",
                    target.name.as_deref().unwrap_or(query)
                );
                return 1;
            }
            // P0 red-team r2 (lead-adjudicated, errors-that-teach): the fork
            // TARGET must be same-provider. Without this, a codex/opencode
            // target's UUID is handed to `claude --resume <uuid> --fork-session`
            // and fails downstream as a confusing empty boot — the symmetric
            // twin of the codex-START --fork refusal above.
            if target.provider != provider_id {
                eprintln!(
                    "sb start: cannot fork \"{}\" — it is a {} session and the new \
                     session's provider is {} (transcripts don't fork across providers).",
                    target.name.as_deref().unwrap_or(query),
                    target.provider,
                    provider_id
                );
                return 1;
            }
            // punch item 11: transcript-existence preflight. A transcript-less
            // target used to pass preflight and die downstream as an
            // undiagnostic BootTimeout (the forked claude exits when --resume
            // finds nothing). Resolve the target's transcript through the
            // provider seam (claude: the projects-dir JSONL for that sid) and
            // refuse LOUDLY before any mint/claim/launch. Tombstoned targets
            // WITH a transcript stay legal (the fork reads the transcript) —
            // this checks transcript existence, never liveness/status.
            if let Some(msg) = fork_transcript_missing_error(provider_impl, &paths, target) {
                eprintln!("{msg}");
                return 1;
            }
            // WP-B5-iii Mechanism S: seed a NEW forked transcript at a fresh fork
            // uuid and resume THAT (not the parent) — the parent stays untouched
            // (O_RDONLY). The fork's identity rides the pre-minted uuid (option A:
            // `mint_or_get(fork_uuid)` below mints the fork's OWN sbId, never the
            // parent's). Uniform for the default (latest safe) + `--turn` rewind.
            match seed_fork_transcript(provider_impl, &paths, &cwd, target, fork_turn, &env) {
                Ok((uuid, stale)) => (Some(uuid), stale),
                Err(code) => return code,
            }
        }
    };
    // §5a: never silently fork stale — surface the gap when the chosen safe
    // boundary lags the live head (e.g. the source is mid-flight on a tool).
    if let Some(notice) = &fork_staleness {
        eprintln!("sb start: {notice}");
    }

    // codex P2 W4 (codex-p2-spec §7.1): the FIRST PRODUCTION `hosting()` consult,
    // sanctioned by GATE-R RULED (A) daemon-thread FINAL + the per-session
    // topology (orc ruling 02:18 06-07). MuxPane → the existing claude
    // choreography below, UNTOUCHED (blast-radius rule). Daemon → a NEW sibling
    // create path (no daemon logic threads through create.rs). The daemon path
    // does NOT use the zmx/sbmux backend selection, mux, or boot waiter — its
    // readiness is the app-server initialize handshake, not a pid-file/went-busy.
    if provider_impl.hosting() == dispatch::provider::Hosting::Daemon {
        return run_new_codex_daemon(
            provider_impl,
            &env,
            &home,
            &paths,
            &name,
            &cwd,
            agent.clone(),
            prompt.clone(),
        );
    }

    // --- WP-B-CS-1 (D2): driver auto-detect routing (claude lane only) ---------
    // I/O mode follows who DRIVES (S-B-COMMAND-SURFACE-RULINGS). A HUMAN caller →
    // today's interactive native-TUI create path BELOW, byte-unchanged. An AGENT
    // caller → the headless stream-json launch (the LaunchHeadless client helper).
    // `--headless`/`--interactive` override the auto-detect. Codex (daemon-hosted)
    // already returned above, so this governs only the claude/mux lane.
    match crate::driver::start_route(
        crate::driver::resolve_driver_real(
            crate::driver::DriverOverride::from_flags(headless_flag, interactive_flag),
            &env,
        ),
        prompt.is_some(),
    ) {
        // Human → fall through to the interactive create path below (unchanged).
        crate::driver::StartRoute::Interactive => {}
        // Fork B: a bare agent/headless start with no `-p` is a usage error (a
        // headless `claude -p ""` is a degenerate no-op turn). Teach the working
        // re-entry verbs.
        crate::driver::StartRoute::RefuseNoPrompt => {
            eprintln!(
                "sb start: agent/headless start requires -p <prompt> (a bare headless \
                 start is a no-op turn). To re-enter an existing session use \
                 \"sb resume <name>\" or \"sb connect <name>\"."
            );
            return 1;
        }
        // Agent + prompt → headless launch via the per-session sbmux daemon.
        crate::driver::StartRoute::Headless => {
            // WP-B5-iii (Q4 ruling): headless `--fork` is REQUIRED (use cases 1&2
            // are agent-self-fork — no TTY → headless only). The earlier
            // interactive-only refusal is LIFTED. Mechanism S already seeded the
            // forked transcript at `fork_uuid` above; the headless launch resumes
            // THAT (additive on the proven headless spine — zero daemon/sbmux/
            // protocol/argv change). The daemon mints the fork's OWN sbId via
            // `mint_or_get(resume_session_id=fork_uuid)` (B5-ii-b parity, never the
            // parent's) + the daemon-minted child-pid row (`provider=None`). A
            // headless fork still requires `-p` (the agent's first subtask;
            // RefuseNoPrompt above) — the engine stays goal-agnostic (INERT: no
            // goal/Stop-hook injected, so no auto-continue), and Mechanism S
            // seeds a SAFE `end_turn` boundary so no in-flight tool auto-executes.
            let prompt = prompt
                .as_deref()
                .expect("start_route Headless implies prompt.is_some()");
            // WP-B5-i: the headless launch MINTS a child-pid-keyed registry row
            // (addressable) AND threads the per-session `cwd` + `claudeArgs`
            // end-to-end. WP-B5-iii: `resume_session_id` = the seeded `fork_uuid`
            // for a fork (`None` for a fresh start — byte-identical to today).
            return match dispatch::embedded_mux::launch_headless_embedded(
                &home,
                &env,
                &name,
                prompt,
                fork_uuid.as_deref(),
                cwd.to_str(),
                &claude_args,
            ) {
                Ok(()) => {
                    println!("Launched headless session \"{name}\".");
                    0
                }
                Err(e) => {
                    eprintln!("sb start: {e}");
                    1
                }
            };
        }
    }

    // Backend-selected create dirs (C1 D2/D3). ONE SB_MUX parse drives the canonical
    // dir, the legacy list, AND the mux below — the embedded lane creates the
    // session in its single sbmux dir (legacy EMPTY); the zmx lane keeps the
    // canonical + cross-dir legacy scan (Bug-D). A bogus SB_MUX exits loudly here.
    let backend = match common::select_backend(&env) {
        Ok(b) => b,
        Err(code) => return code,
    };
    let (canonical, legacy) = match backend {
        dispatch::mux_selector::Backend::Zmx => {
            let canonical = resolve_zmx_dir(&env);
            // PRODUCTION: scan `/tmp` AND the env-derived XDG family (independent
            // axes; ADD-9b red-team BLOCKER 1). A14-2(c): the surviving READ scan
            // honors SB_TEST_SCAN_ROOTS (test lanes only; production = literal /tmp).
            let scan_roots =
                dispatch::zmx_dir::legacy_scan_roots(&env, std::path::Path::new("/tmp"));
            let xdg = XdgFamily::from_env(&env, env.uid());
            let legacy = legacy_zmx_dirs(env.uid(), &canonical, &scan_roots, Some(&xdg));
            (canonical, legacy)
        }
        dispatch::mux_selector::Backend::Embedded => {
            let canonical = match dispatch::sbmux_dir::resolve_sbmux_dir(&home, &env) {
                Ok(d) => d,
                Err(msg) => {
                    eprintln!("sb start: {msg}");
                    return 1;
                }
            };
            (canonical, Vec::new()) // embedded: single dir, legacy EMPTY.
        }
    };
    // --- F1 capture + --via composition (spec §2.2 + §3.2) -------------------
    // Capture the caller's backend env (lifecycle.ts:874) via the injected Env
    // seam (L9a — never raw std::env). When --via is given, overlay the resolved
    // backends.json profile (profile-wins, §3.2.3). The result is the env-key set
    // create.rs writes to the 0600 self-deleting file. EMPTY ⇒ byte-zero change.
    let (backend_env, backend_env_unset) = match compose_backend_env(&env, &home, via.as_deref()) {
        Ok(set) => set,
        Err(code) => return code, // the helper already printed the loud error.
    };

    let exec = RealExec;
    // Backend-selected mux (C1 D3): the create path drives whichever backend
    // SB_MUX names. NewDeps.mux + EventBootWaiter.mux are `&dyn Mux`, so we pass
    // the boxed mux by reference.
    let mux = match common::build_mux(backend, &home, &env) {
        Ok(m) => m,
        Err(code) => return code,
    };
    let clock = RealClock;

    // --- P0 wave-2 (spec-w2-env D1): pre-mint the stable id BEFORE launch -----
    // The id must exist at env-bake time. EVERY start (fresh or forked) boots a
    // session whose provider UUID does not exist yet — a fork mints a NEW UUID
    // at boot — so the identity flow is always mint UNBOUND now, `bind` after
    // the boot waiter confirms the registry row (idstore module doc, "mint
    // timing at sb start"; the wave-2 pre_bound mint_or_get arm died with
    // `--resume`). A mint failure is fail-closed: never boot a session whose
    // env would silently miss its identity (the EnvFileWriteFailed posture).
    let ids_path = match common::ids_store_path(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };
    // WP-B5-iii option A (interactive fork): a fork's claude session-id is the
    // pre-minted `fork_uuid` (the seeded transcript) — known PRE-spawn — so mint
    // the fork's OWN sbId from it via `mint_or_get` (RESUME-path parity, row↔env
    // match at spawn, NEVER the parent's). A non-fork start keeps the unbound
    // pre-mint (bound at boot-confirm). (The headless fork path mints identically
    // inside the daemon via `mint_or_get(resume_session_id=fork_uuid)`.)
    let minted = match &fork_uuid {
        Some(uuid) => dispatch::idstore::mint_or_get(&ids_path, uuid, Some(&name), &clock),
        None => dispatch::idstore::mint_unbound(&ids_path, Some(&name), &clock),
    };
    let sb_session_id = match minted {
        Ok(id) => id,
        Err(e) => {
            eprintln!("sb start: could not mint a stable session id: {e}. No session was created.");
            return 1;
        }
    };

    let sleeper = RealSleeper;
    // codex P1 W3 (codex-p1-spec section 7.1 step 4): obtain the boot waiter
    // THROUGH the provider seam. `provider.boot_waiter(fx)` borrows mux/clock/
    // sleeper/socket_dir out of `fx` and returns the SAME EventBootWaiter the bin
    // verb used to construct inline (claude's impl is a 1:1 delegate to
    // `EventBootWaiter::new` with the same args) — so the boot wait routes through
    // the seam while create.rs keeps driving the injected `BootWaiter` trait. `fx`
    // must outlive the box + the create_run_new call, so it is bound here. The
    // launch-only members (relay/relay_port) are None; boot consumes mux/clock/
    // sleeper/socket_dir/paths.sessions_dir.
    let boot_fx = dispatch::provider::ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: canonical.clone(),
        mux: Some(mux.as_ref()),
        clock: Some(&clock),
        sleeper: Some(&sleeper),
        relay: None,
        relay_port: None,
        // codex-only transport; the claude boot path has no app-server.
        app_server: None,
        codex_expected_turn_id: None,
    };
    let boot_waiter = provider_impl.boot_waiter(&boot_fx);

    let deps = NewDeps {
        mux: mux.as_ref(),
        exec: &exec,
        env: &env,
        clock: &clock,
        paths: &paths,
        canonical_dir: canonical.clone(),
        legacy_dirs: legacy,
        boot_waiter: boot_waiter.as_ref(),
        provider: provider_impl,
        backend,
    };
    let params = NewParams {
        name: name.clone(),
        agent,
        // WP-B5-iii Mechanism S: a fork resumes the sb-SEEDED transcript by its
        // pre-minted `fork_uuid` with a PLAIN `--resume` (NO `--fork-session` — sb
        // already did the faithful copy/rekey/truncate; the fidelity gate proves
        // it equivalent to native). `fork=false` ⇒ launch_plan emits `--resume
        // <fork_uuid>` only. Non-fork start: resume=None (byte-identical to today).
        fork: false,
        resume: fork_uuid,
        claude_args,
        // warranty #2: --model is now a LAUNCH FLAG (birth property), not a
        // post-boot /model slash command. Carried into the claude argv via
        // build_new_extra_args; the post-boot delivery below is GONE.
        model: model.clone(),
        cwd,
        backend_env,
        backend_env_unset,
        sb_session_id: Some(sb_session_id.clone()),
        render,
    };

    let out = match create_run_new(&deps, &params) {
        Ok(out) => out,
        Err(e) => {
            // P0 wave-2: a BootTimeout leaves the session in place (not reaped) —
            // its row may still appear. Best-effort bind of the pre-minted id so
            // a late-booting session's env id matches what `ls` will surface.
            // Silent: the loud boot error below stays byte-stable.
            if let NewError::BootTimeout { .. } = &e {
                bind_minted_id_best_effort(&ids_path, &sb_session_id, &paths, &name, &clock);
            }
            // §5.1 / D6: a BootTimeout on the -p flow emits a positive
            // priming-readiness-timeout to the BYNAME file (no sessionId exists on
            // a failed boot) BEFORE the existing loud exit. ONLY when -p was
            // requested (a bare `sb new` boot timeout keeps today's behavior
            // exactly). The existing stderr/exit are UNCHANGED.
            if prompt.is_some() {
                if let NewError::BootTimeout { phase, .. } = &e {
                    emit_priming_timeout(&home, &env, &clock, &name, *phase);
                }
            }
            eprintln!("{e}");
            return e.exit_code();
        }
    };
    println!("Started detached session \"{}\"", out.name);

    // --- P0 wave-2 (spec-w2-env D1, design option (a)): bind at boot-confirm --
    // The boot waiter confirmed the registry row exists (run_pid_phase found it
    // by name), so the provider UUID is readable NOW — for a fork this is the
    // NEW provider UUID the forked boot minted. Append the `bind` event
    // folding the pre-minted unbound id onto it — from here the id in the
    // child's env equals the id the registry/ls surface, forever. A bind
    // failure warns loudly (the session is up; killing it would not help) —
    // the id stays unbound and `ls` would lazily mint a DIFFERENT id, which
    // the warning names so the divergence is visible, never silent.
    // P0 redfix F1: row selection is liveness-filtered (pick_live_named_row) so
    // a crash-leftover same-name DEAD row can never win by read-dir order, and
    // bind's typed outcome surfaces the already-mapped-to-a-different-id case
    // (the divergence the old Ok(winner) shape swallowed).
    {
        let rows = dispatch::registry::read_entries(&paths.sessions_dir, false);
        let alive = |pid: i64| dispatch::effects::is_pid_alive(pid as i32);
        match dispatch::registry::pick_live_named_row(&rows, &name, &alive) {
            dispatch::registry::LiveNamePick::One { session_id: sid } => {
                match dispatch::idstore::bind(&ids_path, &sb_session_id, &sid, &clock) {
                    // Bound, or an idempotent re-bind of the same pair: silent.
                    Ok(dispatch::idstore::BindOutcome::Bound)
                    | Ok(dispatch::idstore::BindOutcome::AlreadyBoundSameId) => {}
                    // The divergence case: the registry session already maps to
                    // a DIFFERENT stable id — name BOTH ids, never silent.
                    Ok(dispatch::idstore::BindOutcome::SessionHasDifferentId { existing }) => {
                        eprintln!(
                            "WARNING: stable-id divergence for session \"{name}\": env \
                             carries {sb_session_id}, registry session {sid} already \
                             maps to {existing} — sessions disagree; `sb ls` will \
                             surface {existing}, not the session's SB_SESSION_ID."
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "WARNING: could not bind stable id {sb_session_id} to session \
                             {sid}: {e} — `sb ls` may surface a different id than the \
                             session's SB_SESSION_ID."
                        );
                    }
                }
            }
            dispatch::registry::LiveNamePick::NoneBindable => {
                eprintln!(
                    "WARNING: session \"{name}\" booted but its registry row carries no \
                     sessionId yet — stable id {sb_session_id} is unbound; `sb ls` may \
                     surface a different id than the session's SB_SESSION_ID."
                );
            }
            dispatch::registry::LiveNamePick::Ambiguous { count } => {
                eprintln!(
                    "WARNING: {count} RUNNING sessions claim the name \"{name}\" — \
                     refusing to bind stable id {sb_session_id} to either (two live \
                     sessions sharing a name is an anomaly; resolve it, e.g. `sb ls` + \
                     `sb kill`, before trusting name addressing)."
                );
            }
        }
    }

    // --- Warranty belt (2026-06-11): warn at BIRTH if the session is born
    // transport-less. The new engine's relay is a user-scope MCP Claude Code
    // loads from `~/.claude.json` (registered by `sb relay:register`); if that
    // step was skipped (the P0-cutover failure class), every session boots with
    // no relay and the gap stays SILENT until the first `send:relay`. Catch it
    // here. CHEAP (a bounded file read + parse, NO `claude` subprocess),
    // NON-FATAL, never blocks or fails start: an unreadable/unparseable config or
    // a present registration ⇒ say nothing (no false nag); only a PERSISTENTLY
    // absent relay warns. Best-effort by the same contract as the binds above.
    //
    // WP-B (#2): the read is a BOUNDED STABLE-READ behind the `RelayPresence`
    // interceptor — `~/.claude.json` is the user-global file concurrent claude
    // sessions atomically rewrite ~10×/turn with no lock, so a lone read can land
    // in a lost-update window and see the relay transiently absent. The
    // stable-read re-reads across a bounded window and nags ONLY on a stable
    // (persistent) absence; a transient absent / instability ⇒ say nothing. This
    // hardens the EXISTING config-presence read (no new disk-as-status read, §6.0).
    {
        use dispatch::relay_presence::{RelayPresence, StableRelayPresence};
        let claude_json = home.join(".claude.json");
        if StableRelayPresence::for_claude_json(&claude_json)
            .check()
            .should_warn()
        {
            eprintln!(
                "WARNING: session \"{name}\" is up but no user-scope relay MCP is \
                 registered with Claude Code — `sb send:relay` to it will fail. \
                 Register once with `sb relay:register` (see doc/DEPLOY.md), then \
                 new sessions pick it up."
            );
        }
    }

    // --- A6 §4.2: telemetry create stamp (lead integration step) -------------
    // Placed AFTER successful boot-verify (the session is REAL — a failed boot
    // never stamps) and BEFORE the -p delivery branch (red-team F4: the -p path
    // returns early via map_deliver_outcome; a stamp below it would never run on
    // that path, and a Stalled exit-10 session DOES get its events — it booted).
    // Both appends are best-effort by contract (spec §4.1): a failure warns to
    // stderr and NEVER changes this verb's exit code.
    {
        // spawned_by: resolve the CREATING session via the shared ppid walk
        // (spec §4.2; absent — not empty — when the caller is a human shell).
        let creator = dispatch::telemetry::find_caller_session(&paths, &exec);
        // sessionId: SINGLE non-blocking read attempt of the new session's
        // registry entry — claude-code may not have written it yet (it owns
        // <pid>.json); absent is fine, the fold name-keys until then (§4.3).
        let session_id = dispatch::registry::read_entries(&paths.sessions_dir, false)
            .into_iter()
            .find(|s| s.entry.name.as_deref() == Some(name.as_str()))
            .and_then(|s| s.entry.session_id);
        let ev = dispatch::telemetry::CreateEvent {
            name: name.clone(),
            session_id: session_id.clone(),
            spawned_by: creator.as_ref().and_then(|c| c.name.clone()),
            spawned_by_session_id: creator.as_ref().and_then(|c| c.session_id.clone()),
            backend: via.clone(),
        };
        if let Err(e) = dispatch::telemetry::append_create_event(&env, &clock, &ev) {
            eprintln!("WARNING: telemetry create-event append failed (non-fatal): {e}");
        }
        // The uniform create-usage line (spec §4.1 — all FOUR verbs, F9).
        if let Err(e) = dispatch::telemetry::append_usage(
            &env,
            &clock,
            "create",
            session_id.as_deref(),
            Some(name.as_str()),
        ) {
            eprintln!("WARNING: telemetry usage append failed (non-fatal): {e}");
        }
    }

    // --- §3.4 delivery: -p (post boot-ready) ---------------------------------
    // The session is created + idle here. `-p` runs the load-robust
    // deliver_prompt and PARTICIPATES in the went-busy exit contract (§3.5).
    //
    // warranty #2 (2026-06-11): the post-boot `/model <m>` delivery that used to
    // run HERE is REMOVED. `--model` is now a LAUNCH FLAG (build_new_extra_args,
    // a birth property of the session) — current Claude Code's `/model` slash
    // command PERSISTS as the shared global default ("saved as your default for
    // new sessions"), so the old delivery polluted the default a later plain
    // session would inherit, AND combined with `-p` it dropped the prompt (the
    // /model submit + settle left the composer such that the -p body never
    // landed → exit 10). As a launch flag the model is set before boot, touches
    // no shared state, and the -p path runs unencumbered. (Supersedes ADR 0009's
    // /model carve-out — see ADR 0009 superseding note.)

    // -p → deliver_prompt with DELIVER_TIMEOUT_S (15 — NOT send:pty's 120, N9).
    if let Some(p) = &prompt {
        // --- ACK-2 §9 (M3): engine event emission for the -p send (best-effort) -
        // events key: sessionId if resolvable NOW (the existing non-blocking
        // registry read), else byname(name) — the key choice is STICKY for ALL of
        // this send's events (§4.1). state_dir honors SB_HOME (§4.1 / ADD-14).
        let ev_state = dispatch::paths::SbPaths::from_home_env(&home, &env).state_dir;
        let ev_session_id = dispatch::registry::read_entries(&paths.sessions_dir, false)
            .into_iter()
            .find(|s| s.entry.name.as_deref() == Some(name.as_str()))
            .and_then(|s| s.entry.session_id);
        let ev_key = ev_session_id
            .clone()
            .unwrap_or_else(|| events::byname_key(&name));
        let writer = EventWriter::for_key(
            &ev_state,
            &ev_key,
            ev_session_id.clone(),
            Some(name.clone()),
        );
        let send_id = events::mint_send_id(&clock);

        // D10 (R6/R7): the transcript+offset snapshot is UNCONDITIONAL-when-
        // resolvable (not only when payload_needs_verify). The verify step still
        // uses the SAME offset; a single-chunk send simply doesn't run verify.
        let ev_transcript = resolve_new_p_transcript(&paths, &name);
        let snapshot_offset: u64 = ev_transcript
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok().map(|m| m.len()))
            .unwrap_or(0);
        // The verify window offset = the snapshot when verify runs, else unused.
        let verify_offset: u64 = if dispatch::submit::payload_needs_verify(p) {
            snapshot_offset
        } else {
            0
        };

        // §2.3.1 send-initiated (verb:"new-p", send_path:"idle"): minted BEFORE
        // the first chunk write. Per-chunk + content shas from the production
        // splitter; transcript/offset present when resolvable (D10).
        let chunks_vec = dispatch::submit::chunk_text(p, dispatch::events::CHUNK_BYTES);
        let chunk_sha256s: Vec<String> = chunks_vec
            .iter()
            .map(|c| events::sha256_hex(c.as_bytes()))
            .collect();
        let ev_transcript_str = ev_transcript.as_ref().map(|p| p.display().to_string());
        let ev_transcript_offset = ev_transcript.as_ref().map(|_| snapshot_offset);
        events::warn_emit(
            &writer,
            &clock,
            &Payload::SendInitiated {
                send_id: send_id.clone(),
                verb: events::verb_str(true).to_string(),
                send_path: "idle".to_string(),
                content_sha256: events::sha256_hex(p.as_bytes()),
                content_len: p.len() as u64,
                chunks: chunks_vec.len() as u32,
                chunk_sha256s,
                chunk_sha256s_capped: false,
                transcript: ev_transcript_str,
                transcript_offset: ev_transcript_offset,
                // ADD-20 (§6.2): redacted ≤256B preview of the -p prompt text.
                content_preview: Some(dispatch::redact::redact_for_preview(
                    p,
                    dispatch::events::PREVIEW_CAP_BYTES,
                )),
            },
        );

        // §9: the deliver runs through a RECORDING DeliverDeps (Real* binding +
        // per-chunk ack capture; the DeliverDeps trait + pure deliver_prompt core
        // are UNTOUCHED). chunks-delivered emits when EVERY text chunk acked.
        let deliver = RecordingDeliverDeps {
            inner: dispatch::submit::RealDeliverDeps {
                mux: mux.as_ref(),
                clock: &clock,
                sleeper: &sleeper,
                zmx_name: name.clone(),
                session_name: name.clone(),
                sessions_dir: paths.sessions_dir.clone(),
                dir: out.socket_dir.clone(),
            },
            mux: mux.as_ref(),
            dir: out.socket_dir.clone(),
            zmx_name: name.clone(),
            sleeper: &sleeper,
            total: Cell::new(0),
            acked: Cell::new(0),
        };

        // rev C row 24 WatchGuard: armed across the deliver-acceptance watch +
        // verify; an early return / panic without a terminal Drops it →
        // pending-abandoned{watch-interrupted}. Disarmed on every terminal below.
        let guard = WatchGuard::arm(&writer, &clock, &send_id);

        let outcome =
            dispatch::submit::deliver_prompt(&deliver, p, dispatch::submit::DELIVER_TIMEOUT_S);

        // §2.3.2 chunks-delivered: all text chunks acked → emit.
        let acks_total = deliver.total.get();
        let acks_acked = deliver.acked.get();
        if acks_total > 0 && acks_acked == acks_total {
            events::warn_emit(
                &writer,
                &clock,
                &Payload::ChunksDelivered {
                    send_id: send_id.clone(),
                    chunks_acked: acks_acked,
                    // -p delivery is the embedded/zmx backend per the create lane;
                    // name the channel honestly via the shared label.
                    ack_source: new_p_ack_source().to_string(),
                },
            );
        }

        // W8 verify-after-submit: CHUNKED deliveries that went busy get a bounded
        // payload read-back (M11 sanctioned; ADR-0012). Loud exit-1 ONLY on
        // POSITIVE truncation evidence; resolution failure / no record / foreign
        // records DEGRADE to one warn (this path's transcript may simply not be
        // resolvable yet — false-fails are the design enemy, red-team R1).
        // Single-chunk prompts: behavior byte-for-byte unchanged (scope guard).
        if outcome == dispatch::submit::DeliverOutcome::Accepted
            && dispatch::submit::payload_needs_verify(p)
        {
            let deps = NewPVerifyDeps {
                paths: &paths,
                name: &name,
                offset: verify_offset,
                clock: &clock,
                sleeper: &sleeper,
            };
            match dispatch::submit::verify_chunked_payload(
                &deps,
                p,
                dispatch::submit::VERIFY_TIMEOUT_S,
                dispatch::submit::VERIFY_POLL_MS,
            ) {
                dispatch::submit::PayloadVerifyOutcome::Verified => {
                    // §9 anchored: a Verified read-back IS the landed signal.
                    // recovered false; attribution absent; the anchor uses the
                    // verify-window offset, line_index 0 (verify returns texts, not
                    // indices — documented unknown).
                    emit_new_p_anchored(
                        &writer,
                        &clock,
                        &send_id,
                        p,
                        ev_transcript.as_deref(),
                        verify_offset,
                    );
                    guard.disarm();
                    return map_deliver_outcome(outcome, &name);
                }
                dispatch::submit::PayloadVerifyOutcome::Truncated { expected, recorded } => {
                    // §2.3.4 anchored-mismatch (terminal): the lengths come from the
                    // outcome; actual_sha is re-derived HERE (re-read past the offset
                    // + longest truncation signature). Emitted BEFORE the unchanged
                    // exit-1.
                    emit_new_p_mismatch(
                        &writer,
                        &clock,
                        &send_id,
                        p,
                        ev_transcript.as_deref(),
                        verify_offset,
                        expected,
                        recorded,
                    );
                    guard.disarm();
                    // The turn STARTED (went busy) — this fires AFTER acceptance
                    // as a distinct named error in the EXISTING exit-1 failure
                    // class (ADR-0008 codes untouched; ADR-0012). NO auto-retry:
                    // the truncated turn already reached the model (M11 §2).
                    eprintln!(
                        "ERROR: payload truncated in delivery to \"{name}\": expected {expected} bytes, \
                         recorded {recorded}.\n  The turn started (went busy) — do NOT blindly resend \
                         (double-submit risk).\n  Attach: sb connect {name}"
                    );
                    return 1;
                }
                dispatch::submit::PayloadVerifyOutcome::Unattributable => {
                    // No terminal (spec §9: NoRecord/Unattributable/SourceUnavailable
                    // → stays dangling). The send remains outstanding by design.
                    eprintln!(
                        "WARNING: could not attribute the delivered payload in \"{name}\"'s \
                         transcript — check: sb connect {name}"
                    );
                }
                dispatch::submit::PayloadVerifyOutcome::NoRecord
                | dispatch::submit::PayloadVerifyOutcome::SourceUnavailable(_) => {
                    eprintln!(
                        "WARNING: could not verify payload delivery to \"{name}\" \
                         (transcript not yet resolvable) — check: sb connect {name}"
                    );
                }
            }
        }

        // §9: deliver outcome → terminal events (Accepted single-chunk emits NO
        // terminal — written+accepted, anchor comes from verify only; it stays
        // dangling by design). Stalled → anchor-timeout (the deliver budget);
        // PidFileMissing → pending-abandoned{session-died}. The exit codes /
        // stdout / stderr from map_deliver_outcome are UNCHANGED.
        match outcome {
            dispatch::submit::DeliverOutcome::Stalled => {
                events::warn_emit(
                    &writer,
                    &clock,
                    &Payload::AnchorTimeout {
                        send_id: send_id.clone(),
                        // The deliver budget (DELIVER_TIMEOUT_S) is the watch bound.
                        waited_ms: dispatch::submit::DELIVER_TIMEOUT_S * 1000,
                    },
                );
            }
            dispatch::submit::DeliverOutcome::PidFileMissing => {
                events::warn_emit(
                    &writer,
                    &clock,
                    &Payload::PendingAbandoned {
                        send_id: send_id.clone(),
                        reason: "session-died".to_string(),
                    },
                );
            }
            // Accepted (single-chunk, or chunked-with-degraded-verify): NO terminal
            // — the send remains dangling by design (§9 "written" row).
            dispatch::submit::DeliverOutcome::Accepted => {}
        }
        guard.disarm();
        return map_deliver_outcome(outcome, &name);
    }

    0
}

/// P0 wave-2: best-effort bind of a pre-minted unbound id after a BootTimeout
/// (the session is NOT reaped — its row may already exist or appear late). One
/// non-blocking registry read by name; every failure is silent (the loud boot
/// error owns stderr, byte-stable). P0 redfix F1: the row pick is the SAME
/// liveness-filtered helper as the boot-confirm site (pick_live_named_row) —
/// no-row and ambiguous both mean "don't bind", silently here by this path's
/// contract.
fn bind_minted_id_best_effort(
    ids_path: &std::path::Path,
    sb_session_id: &str,
    paths: &dispatch::paths::SbPaths,
    name: &str,
    clock: &RealClock,
) {
    let rows = dispatch::registry::read_entries(&paths.sessions_dir, false);
    let alive = |pid: i64| dispatch::effects::is_pid_alive(pid as i32);
    if let dispatch::registry::LiveNamePick::One { session_id: sid } =
        dispatch::registry::pick_live_named_row(&rows, name, &alive)
    {
        let _ = dispatch::idstore::bind(ids_path, sb_session_id, &sid, clock);
    }
}

/// codex P2 W4 (codex-p2-spec §7.2): the daemon-hosted `sb new` arm. Assembles
/// the REAL [`dispatch::create_daemon::DaemonDeps`] seams (the daemon analog of how the
/// claude arm assembles `NewDeps`) and drives the lib-side
/// [`dispatch::create_daemon::run_new_daemon`], then maps the outcome/error to the
/// verb's stdout/stderr + exit code. The daemon path is self-contained: no zmx/
/// sbmux backend, no `EventBootWaiter`, no F1 env-file — its readiness IS the
/// app-server initialize handshake, and the row is written by the lib itself.
#[allow(clippy::too_many_arguments)]
fn run_new_codex_daemon(
    provider_impl: &'static dyn dispatch::provider::Provider,
    env: &RealEnv,
    home: &std::path::Path,
    paths: &dispatch::paths::SbPaths,
    name: &str,
    cwd: &std::path::Path,
    agent: Option<String>,
    prompt: Option<String>,
) -> i32 {
    use dispatch::create_daemon::{real_alloc_port, DaemonDeps, DaemonParams, RealDaemonSpawner};
    use dispatch::provider::codex::{AppServerRpc, RpcError, WsAppServer};

    let exec = RealExec;
    let clock = RealClock;
    let spawner = RealDaemonSpawner;
    // The connector: open a real ws client to the recorded endpoint. Boxed as the
    // injected `RpcConnector` so the lib drives it without holding a transport
    // type (the contract-stays-the-contract seam — ADD-5 pattern). The connect
    // timeout floor matches the lib's connect-retry granularity.
    let connect = |url: &str| -> Result<Box<dyn AppServerRpc>, RpcError> {
        WsAppServer::connect(url, std::time::Duration::from_secs(5)).map(|c| {
            let boxed: Box<dyn AppServerRpc> = Box::new(c);
            boxed
        })
    };
    let alloc = real_alloc_port;

    // W9 FIX M-2: the claims dir for the atomic name-claim — `<.claude>/claims`,
    // alongside `sessions/` (the claude create-path layout). Derived from the
    // sessions dir's parent so the claim shares the registry's state root.
    let claims_dir = paths
        .sessions_dir
        .parent()
        .map(|p| p.join("claims"))
        .unwrap_or_else(|| home.join(".claude").join("claims"));

    // P0 wave-2: the shared ids store (semantics: ids_store_path's doc).
    let ids_path = match common::ids_store_path(env) {
        Ok(p) => p,
        Err(code) => return code,
    };

    let deps = DaemonDeps {
        provider: provider_impl,
        env,
        exec: &exec,
        clock: &clock,
        sessions_dir: paths.sessions_dir.clone(),
        claims_dir,
        // The daemon's stdout/stderr log root: `<sb_home>/.sb/log` (codex-p2-spec
        // §3.2). Resolved off the injected home so a jailed HOME points the log
        // into the jail (L9a). The file is `codex-<name>.log`.
        log_dir: home.join(".quorum").join("dispatch").join("log"),
        spawner: &spawner,
        connect: &connect,
        alloc_port: &alloc,
        ids_path,
    };
    let params = DaemonParams {
        name: name.to_string(),
        cwd: cwd.to_path_buf(),
        agent,
        passthrough: vec![],
        prompt,
    };

    match dispatch::create_daemon::run_new_daemon(&deps, &params) {
        Ok(out) => {
            println!("Started detached codex session \"{}\"", out.name);
            0
        }
        Err(e) => {
            eprintln!("{e}");
            e.exit_code()
        }
    }
}

/// §2.3.2 `chunks-delivered.ack_source` for the `new -p` create path. The create
/// lane drives whichever backend SB_MUX names; the embedded daemon blocks on its
/// per-write InputSent ack ("input-sent"), zmx observes only `zmx send` exit 0
/// ("cli-exit"). Reuse the send-verb label so both verbs name the channel the same.
fn new_p_ack_source() -> &'static str {
    match common::send_backend_label() {
        common::SendBackend::Embedded => "input-sent",
        common::SendBackend::Zmx => "cli-exit",
    }
}

/// §5.1 / D6: emit `priming-readiness-timeout` to the BYNAME file on a -p boot
/// timeout (no sessionId exists on a failed boot). `phase` is the TYPED boot
/// phase carried from the source on `NewError::BootTimeout` (m-4, ack3-spec §8):
/// `Idle` → "idle", `PidFile` → "pid-file" — no longer string-matched out of the
/// detail wording. `waited_ms` is best-effort (the configured phase deadline).
/// The existing stderr/exit are UNCHANGED — this is purely additive.
fn emit_priming_timeout(
    home: &std::path::Path,
    env: &RealEnv,
    clock: &RealClock,
    name: &str,
    phase: dispatch::boot::BootPhase,
) {
    let ev_state = dispatch::paths::SbPaths::from_home_env(home, env).state_dir;
    let writer = EventWriter::for_key(
        &ev_state,
        &events::byname_key(name),
        None,
        Some(name.to_string()),
    );
    let defaults = dispatch::boot::BootTimeouts::default();
    // m-4 (ack3-spec §8): phase is read TYPED from the BootFailure carried up the
    // create seam — the old `detail.contains("did not reach idle")` string-match
    // (the named COUPLING) is gone; a reworded boot error can no longer misfile it.
    let (phase, waited_ms) = match phase {
        dispatch::boot::BootPhase::Idle => ("idle", defaults.overall_ms.max(0) as u64),
        dispatch::boot::BootPhase::PidFile => ("pid-file", defaults.pid_phase_ms.max(0) as u64),
        // Fix-A (RESPEC-DELTA §4): the relay-sidecar phase shares the overall
        // deadline (it runs after idle, bounded by the same boot deadline).
        dispatch::boot::BootPhase::Relay => ("relay", defaults.overall_ms.max(0) as u64),
    };
    events::warn_emit(
        &writer,
        clock,
        &Payload::PrimingReadinessTimeout {
            waited_ms,
            phase: phase.to_string(),
        },
    );
}

/// §9 anchored emission for the `new -p` Verified path (mirrors send:pty's W8
/// anchored): recovered false, attribution absent, line_index 0 (verify returns
/// texts, not indices). `transcript` may be unresolved (None → empty string —
/// the anchor still carries the offset).
fn emit_new_p_anchored(
    writer: &EventWriter,
    clock: &dyn Clock,
    send_id: &str,
    message: &str,
    transcript: Option<&std::path::Path>,
    offset: u64,
) {
    events::warn_emit(
        writer,
        clock,
        &Payload::TurnAnchored {
            send_id: send_id.to_string(),
            content_sha256: events::sha256_hex(message.as_bytes()),
            anchor: Anchor {
                transcript: transcript
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                start_offset: offset,
                line_index: 0,
            },
            recovered: false,
            attribution: None,
        },
    );
}

/// §2.3.4 anchored-mismatch for the `new -p` Truncated path. The
/// PayloadVerifyOutcome carries lengths only; `actual_sha` is re-derived HERE by
/// re-reading the user texts past the offset + sha-ing the longest truncation-
/// signature record (honest value; "" if the re-read fails / transcript absent).
#[allow(clippy::too_many_arguments)]
fn emit_new_p_mismatch(
    writer: &EventWriter,
    clock: &dyn Clock,
    send_id: &str,
    message: &str,
    transcript: Option<&std::path::Path>,
    offset: u64,
    expected: usize,
    recorded: usize,
) {
    let actual_sha = transcript
        .and_then(|t| dispatch::submit::read_user_texts_past_offset(t, offset).ok())
        .and_then(|texts| {
            dispatch::submit::longest_truncation_signature(&texts, message)
                .map(|r| events::sha256_hex(r.as_bytes()))
        })
        .unwrap_or_default();
    events::warn_emit(
        writer,
        clock,
        &Payload::TurnAnchoredMismatch {
            send_id: send_id.to_string(),
            expected_sha: events::sha256_hex(message.as_bytes()),
            actual_sha,
            expected_len: expected as u64,
            actual_len: recorded as u64,
            recovered: false,
            attribution: None,
        },
    );
}

/// A bin-local [`DeliverDeps`] that wraps [`RealDeliverDeps`] and RECORDS each
/// `send_message` text-chunk ack (ACK-2 §9 recorder; red-team R9). `send_message`
/// re-implements the Real impl's CHUNKED two-write delivery but captures each
/// chunk's `mux.send(...).is_ok()`; the CR write is NOT counted (only text chunks
/// are chunks-delivered evidence). All OTHER trait methods delegate to `inner`, so
/// the bounded-retry core + the DeliverDeps trait are UNTOUCHED.
struct RecordingDeliverDeps<'a> {
    inner: dispatch::submit::RealDeliverDeps<'a>,
    mux: &'a dyn Mux,
    dir: PathBuf,
    zmx_name: String,
    sleeper: &'a RealSleeper,
    total: Cell<u32>,
    acked: Cell<u32>,
}

impl dispatch::submit::DeliverDeps for RecordingDeliverDeps<'_> {
    fn send_message(&self, message: &str) {
        // CHUNKED two-write delivery (mirrors RealDeliverDeps::send_message,
        // submit.rs), recording each text chunk's ack. The settle + separate "\r"
        // are byte-identical to the Real impl; only the per-chunk result, which the
        // Real impl discards via `let _ =`, is captured here.
        dispatch::submit::send_text_chunked(
            &mut |chunk| {
                self.total.set(self.total.get() + 1);
                if self.mux.send(&self.dir, &self.zmx_name, chunk).is_ok() {
                    self.acked.set(self.acked.get() + 1);
                }
            },
            &mut |ms| self.sleeper.sleep_ms(ms),
            message,
            dispatch::submit::ChunkSendOptions::default(),
        );
        self.sleeper.sleep_ms(dispatch::submit::TWO_WRITE_SETTLE_MS);
        let _ = self.mux.send(&self.dir, &self.zmx_name, "\r");
    }
    fn read_screen(&self) -> String {
        self.inner.read_screen()
    }
    fn find_pid_file(&self) -> Option<PathBuf> {
        self.inner.find_pid_file()
    }
    fn submit_deps(
        &self,
        pid_file: PathBuf,
        message: &str,
    ) -> Box<dyn dispatch::submit::SubmitDeps + '_> {
        self.inner.submit_deps(pid_file, message)
    }
}

/// W8: resolve the just-created session's transcript path (best-effort, single
/// pass): registry row by NAME → sessionId → `find_jsonl_path`. `None` at any
/// step (claude hasn't written its row / id / transcript yet — normal for a
/// fresh session).
fn resolve_new_p_transcript(
    paths: &dispatch::paths::SbPaths,
    name: &str,
) -> Option<std::path::PathBuf> {
    let entry = dispatch::registry::read_entries(&paths.sessions_dir, false)
        .into_iter()
        .find(|s| s.entry.name.as_deref() == Some(name))?;
    let sid = entry.entry.session_id?;
    dispatch::jsonl::find_jsonl_path(&paths.projects_dir, &sid, entry.entry.cwd.as_deref())
}

/// W8 [`dispatch::submit::VerifyDeps`] for the `sb new -p` path: RE-resolves the
/// transcript each poll (registry → sessionId → path; the fresh session's
/// row/transcript may land mid-budget) and reads user texts past the
/// pre-delivery offset. Every resolution failure is a re-polled `Err` —
/// [`dispatch::submit::verify_chunked_payload`] degrades to `SourceUnavailable` only
/// when NO read ever succeeds.
struct NewPVerifyDeps<'a> {
    paths: &'a dispatch::paths::SbPaths,
    name: &'a str,
    offset: u64,
    clock: &'a RealClock,
    sleeper: &'a RealSleeper,
}

impl dispatch::submit::VerifyDeps for NewPVerifyDeps<'_> {
    fn read_user_texts(&self) -> Result<Vec<String>, String> {
        let path = resolve_new_p_transcript(self.paths, self.name)
            .ok_or_else(|| "session transcript not yet resolvable".to_string())?;
        dispatch::submit::read_user_texts_past_offset(&path, self.offset)
    }
    fn sleep(&self, ms: u64) {
        use dispatch::boot::Sleeper;
        self.sleeper.sleep_ms(ms);
    }
    fn now_ms(&self) -> i64 {
        use dispatch::effects::Clock;
        self.clock.now_ms()
    }
}

/// §3.5 WENT-BUSY EXIT CONTRACT (HARDENING #3, ADR 0008): map the three-way
/// [`DeliverOutcome`] of an `sb new -p` delivery to the sanctioned exit codes.
///
///   - [`DeliverOutcome::Accepted`] → **0**, stdout `Prompt delivered to "<n>"`.
///   - [`DeliverOutcome::Stalled`]  → **10**, stderr the WARNING block (the
///     session EXISTS; the turn-start is unconfirmed) — the deliberate divergence
///     from TS's always-0 (lifecycle.ts:921-931) so an external `bond spawn` can
///     tell "made, not running" apart from "made + running".
///   - [`DeliverOutcome::PidFileMissing`] → **1** (R1: a vanished PID file is an
///     INFRA failure, NOT a stall; routing it to 10 would lie to `bond spawn`).
///
/// The TS WARNING wording (lifecycle.ts:923-930) is ported verbatim for the
/// Stalled branch; PidFileMissing gets its own infra-distinguishing stderr.
fn map_deliver_outcome(outcome: dispatch::submit::DeliverOutcome, name: &str) -> i32 {
    use dispatch::submit::DeliverOutcome;
    match outcome {
        DeliverOutcome::Accepted => {
            println!("Prompt delivered to \"{name}\"");
            0
        }
        DeliverOutcome::Stalled => {
            // VERBATIM TS WARNING block (qa/hardening@3dd9f1e:src/commands/
            // lifecycle.ts:923-930) — the session exists, the prompt may sit
            // unsubmitted in the composer. Exit 10 is the Rust-only contract.
            eprintln!(
                "WARNING: Prompt sent to \"{name}\" but session did not go busy.\n\
                 The prompt may be in the composer but not submitted.\n  \
                 Attach: sb connect {name}"
            );
            10
        }
        DeliverOutcome::PidFileMissing => {
            // R1: NOT a stall — the registry row vanished post-boot (infra). Exit
            // 1, with stderr that distinguishes it from the Stalled (10) case.
            eprintln!(
                "ERROR: Prompt sent to \"{name}\" but the session's PID file vanished \
                 after boot — the registry row disappeared (infra failure, not a stall).\n  \
                 Check: sb ls\n  Attach: sb connect {name}"
            );
            1
        }
    }
}

/// F1 capture + `--via` composition for `sb new` (spec §2.2 + §3.2). Returns the
/// composed backend-env key set create.rs writes to the 0600 self-deleting file,
/// or an exit code on a loud failure (the loud message is already printed).
///
/// Without `--via`: just the caller capture (lifecycle.ts:874). With `--via`:
///   1. the name passes an UNCONDITIONAL S2-style charset check (fresh surface —
///      never ruling-dependent; the name reaches the unknown-name error string),
///   2. read+parse `<sbHome>/state/backends.json` (O_NOFOLLOW, perm WARN, §3.1),
///   3. resolve the profile (loud unknown-name listing known names only, §3.2.2),
///   4. compose profile-wins + credential-slot exclusivity (§3.2.3), resolving a
///      `secret` credential via the name-agnostic `get_secret` (keychain→file,
///      NO env-override tier — §3.2.3 / red-team F3/F10).
///
/// The credential VALUE flows ONLY through the returned vec (then the 0600 file);
/// it never touches argv, never any error/log string here.
/// A7 F12: (composed backend-env pairs, whitelist keys to `unset -v` in the
/// env file). The unset list is empty for non-`--via` sessions (F1 parity).
type BackendEnvComposition = (Vec<(String, String)>, Vec<String>);

fn compose_backend_env(
    env: &RealEnv,
    home: &std::path::Path,
    via: Option<&str>,
) -> Result<BackendEnvComposition, i32> {
    // F1: capture the caller's whitelisted backend env (injected Env seam, L9a).
    let captured = capture_backend_env(env);

    let Some(via_name) = via else {
        // no --via → plain capture (byte-zero when empty), NO unsets: the F1
        // surface stays TS-parity (utils.ts:668-680 sources only, never unsets).
        return Ok((captured, Vec::new()));
    };

    // The --via name must pass charset validation BEFORE any use (spec §3.2;
    // unconditional fresh surface). Reuse the ported S2 guard so the rule and
    // message match the session-name S2 exactly.
    if let Some(msg) = dispatch::resume::validate_session_name(via_name) {
        eprintln!("sb start --via: invalid backend name: {msg}");
        return Err(1);
    }

    // Resolve <sbHome>/state via SB_HOME-honoring paths (same as marks.jsonl);
    // independent of the create path's own `paths` so nothing else changes.
    let state_paths = dispatch::paths::SbPaths::from_home_env(home, env);
    let file_path = dispatch::backends::backends_file_path(&state_paths.state_dir);

    let (file, perm_warning) = match dispatch::backends::read_backends_file(&file_path) {
        Ok(ok) => ok,
        Err(e) => {
            eprintln!("{e}");
            return Err(1);
        }
    };
    if let Some(w) = perm_warning {
        eprintln!("{}", w.message());
    }

    let backend = match file.resolve(via_name) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            return Err(1);
        }
    };

    // The secret resolver: the name-agnostic get_secret (keychain→file, ADR-0010
    // locked-fallback intact). NO env-override tier (profile-wins determinism).
    let composed = with_real_secret_deps(env, |deps| {
        dispatch::backends::compose_via_env(via_name, &captured, &backend, &|key| {
            dispatch::secrets::get_secret(key, deps)
        })
    });
    match composed {
        Ok(set) => {
            // A7 F12 fix: for a --via session the child's whitelisted env must be
            // EXACTLY the composed set. Whitelist keys NOT in the set get explicit
            // `unset -v` lines in the env file — removal from the composed pairs
            // alone leaves the caller-inherited/profile-re-exported value riding
            // (observed live on Lima 2026-06-05: real caller ANTHROPIC_API_KEY
            // reached a profile-secret child; macOS scenario leg was vacuously
            // green because no caller key was exported).
            let unset: Vec<String> = dispatch::launch::BACKEND_ENV_KEYS
                .iter()
                .filter(|k| !set.iter().any(|(sk, _)| sk == *k))
                .map(|k| k.to_string())
                .collect();
            Ok((set, unset))
        }
        Err(e) => {
            // e.g. SecretMissing — names the key + `sb config set` hint, no value.
            eprintln!("{e}");
            Err(1)
        }
    }
}

/// Build a real [`dispatch::secrets::SecretDeps`] over the production seams and run
/// `f`. Mirrors `survey.rs`/`config.rs`'s real-fs closures (chmod-600 file
/// backend, per-process notice guards). The `--via` path only ever READS, but
/// the locked-keychain fallback may file-read, so the standard real closures are
/// used. The secret value returned by `f` is held only in memory.
fn with_real_secret_deps<R>(
    env: &RealEnv,
    f: impl FnOnce(&dispatch::secrets::SecretDeps) -> R,
) -> R {
    use std::fs;
    use std::sync::atomic::AtomicBool;
    let exec = RealExec;
    let notice = AtomicBool::new(false);
    let locked_diag = AtomicBool::new(false);

    let read_file = |p: &str| fs::read_to_string(p).ok();
    let write_file = |p: &str, text: &str| {
        if let Some(parent) = std::path::Path::new(p).parent() {
            let _ = fs::create_dir_all(parent);
        }
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut fh) = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(p)
        {
            use std::io::Write;
            let _ = fh.write_all(text.as_bytes());
        }
    };
    let chmod = |p: &str, mode: u32| {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(p, fs::Permissions::from_mode(mode));
    };
    let file_exists = |p: &str| std::path::Path::new(p).exists();
    let keychain_available = || dispatch::secrets::real_keychain_available(&exec);
    let deps = dispatch::secrets::SecretDeps {
        platform: std::env::consts::OS,
        env,
        exec: &exec,
        keychain_available: &keychain_available,
        read_file: &read_file,
        write_file: &write_file,
        chmod: &chmod,
        file_exists: &file_exists,
        fallback_notice_emitted: &notice,
        locked_diag_emitted: &locked_diag,
    };
    f(&deps)
}

// --- info (commands/status.ts:560-662) ---

/// `sb info <session>` — A1 render/jsonl per commands/status.ts:561-660 field order. The
/// OpenCode lastTurns fetch branch is skipped (parked); unknown session →
/// resolveOrDie error + exit 1.
///
/// P0 spec-w8: `--json` prints ONE json object (render::info_json — the
/// point-resolution surface bond joins against) instead of the human text.
/// Resolution is the standard pipeline UNCHANGED — the not-found/ambiguity
/// error paths above the branch stay loud on stderr + exit 1 and emit NO json.
pub fn run_info(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    let json = m.get_flag("json");

    // info uses includeAll + includePreview (commands/status.ts:564-567).
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: true,
        limit: None,
    };
    let sessions = match common::all_sessions(opts) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let session = match resolve_or_die(query, &sessions) {
        Ok(s) => s,
        Err(code) => return code,
    };

    if json {
        // sbIdPrefix: shortest-unique among the resolved session LIST, the
        // same computation ls uses (idstore::prefix_map).
        let prefixes = dispatch::idstore::prefix_map(&sessions);
        // live: status-live AND pid-alive-where-a-pid-exists, through the real
        // is_pid_alive effects seam (semantics + pins: resolve::is_live_with_pid).
        let alive = |p: i32| dispatch::effects::is_pid_alive(p);
        let live = dispatch::resolve::is_live_with_pid(session.status, session.pid, &alive);
        println!(
            "{}",
            dispatch::render::to_pretty(&dispatch::render::info_json(session, &prefixes, live))
        );
        return 0;
    }

    let now = RealClock.now_ms();
    // A6 §4.4: load the telemetry fold BEST-EFFORT (any read error → empty map)
    // and pass it. Empty fold ≡ today's bytes exactly (additive-only, G-A1).
    let fold = dispatch::telemetry::fold_from_env(&RealEnv);
    let fold_ref = if fold.is_empty() { None } else { Some(&fold) };
    print!(
        "{}",
        dispatch::render::info_text_with_fold(session, now, fold_ref)
    );
    0
}

// --- live (commands/status.ts:394-556) ---

/// `sb live` — the 2s-refresh TUI + 3-char-code keystroke→attach (commands/status.ts:395-
/// 560). REAL interactive path for a TTY; for non-TTY, TS crashes with a Bun
/// ReferenceError (corpus 33-*). We do NOT replicate the stack trace: print the
/// header then a clean one-line error to stderr and exit 1.
//
// parity exclusion pending lead ruling — TS crashes non-TTY (corpus 33-*)
pub fn run_live(m: &ArgMatches) -> i32 {
    let all = m.get_flag("all");

    let is_tty = unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
        && unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };

    if !is_tty {
        // TS header to stdout (the clear + title, commands/status.ts:433-434), then a clean
        // error to stderr (NOT the Bun stack trace) + exit 1.
        print!("\x1b[2J\x1b[H");
        println!("sb live  type code to connect · q to quit\n");
        eprintln!("sb live: requires an interactive terminal (TTY).");
        return 1;
    }

    // Interactive: render loop (2s) + raw-mode keystroke capture. Implemented with
    // std + termios via libc (no new crate dep). 3-char [a-z0-9] code → attach;
    // q / Ctrl-C → quit (commands/status.ts:525-555).
    run_live_interactive(all)
}

fn run_live_interactive(all: bool) -> i32 {
    use std::io::Read;
    use std::time::{Duration, Instant};

    // Enter raw mode (port of process.stdin.setRawMode(true), commands/status.ts:413).
    let mut termios = match RawMode::enter() {
        Some(t) => t,
        None => {
            eprintln!("sb live: could not enter raw terminal mode.");
            return 1;
        }
    };

    let opts = JoinOpts {
        include_all: all,
        include_tombstoned: all,
        include_preview: false,
        limit: Some(if all { 50 } else { 20 }),
    };

    let mut last_refresh = Instant::now()
        .checked_sub(Duration::from_secs(3))
        .unwrap_or_else(Instant::now);
    let mut code_buf = String::new();
    let mut current: Vec<Session> = Vec::new();
    let mut stdin = std::io::stdin();

    loop {
        if last_refresh.elapsed() >= Duration::from_secs(2) {
            current = common::all_sessions(opts).unwrap_or_default();
            print!("\x1b[2J\x1b[H");
            println!("sb live  type code to connect · q to quit\n");
            if current.is_empty() {
                println!("No sessions found.");
            } else {
                print!("{}", super::ls::render_table_for_live(&current));
            }
            use std::io::Write;
            let _ = std::io::stdout().flush();
            last_refresh = Instant::now();
        }

        // Non-blocking-ish read with a short timeout via poll on stdin fd.
        let mut buf = [0u8; 1];
        if poll_stdin(250) {
            if stdin.read(&mut buf).unwrap_or(0) == 0 {
                continue;
            }
            let key = buf[0];
            // q or Ctrl-C → quit (commands/status.ts:528).
            if key == b'q' || key == 0x03 {
                termios.restore();
                print!("\x1b[2J\x1b[H");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                return 0;
            }
            // Collect [a-z0-9] for short-code selection (commands/status.ts:542-554).
            let ch = (key as char).to_ascii_lowercase();
            if ch.is_ascii_alphanumeric() {
                code_buf.push(ch);
                if code_buf.len() >= 3 {
                    let sel = code_buf.clone();
                    code_buf.clear();
                    if let Some(match_) = current.iter().find(|s| s.code.as_deref() == Some(&sel)) {
                        let m = match_.clone();
                        termios.restore();
                        print!("\x1b[2J\x1b[H");
                        let label = m.name.clone().unwrap_or_else(|| m.session_id.clone());
                        if m.status == SessionStatus::Cold {
                            // W2: resume IS first-class now (no longer "not supported").
                            // The live picker runs in raw termios mid-loop; rather than
                            // re-enter the full revive choreography from here, point the
                            // user at the dedicated verbs — `sb connect` for the human
                            // revive+attach, `sb resume` for the agent cold-revive.
                            print!("\x1b[2J\x1b[H");
                            eprintln!(
                                "Session \"{label}\" is cold (not running). Revive it with:\n  \
                                 sb connect {label}   (human: revive and attach)\n  \
                                 sb resume {label}    (agent: revive to drivable)"
                            );
                            return 1;
                        }
                        println!("Attaching to \"{label}\"...");
                        return attach_session(&m);
                    }
                }
            }
        }
    }
}

/// Attach to a resolved live session from the live picker (connectToSession for
/// the claude-code path).
fn attach_session(s: &Session) -> i32 {
    let Some(zmx_name) = s.zmx_name.as_deref() else {
        eprintln!(
            "Session \"{}\" is not in zmx.",
            s.name.as_deref().unwrap_or(&s.session_id)
        );
        return 1;
    };
    let env = RealEnv;
    let canonical = resolve_zmx_dir(&env);
    let dir = s
        .socket_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or(canonical);
    // Backend-selected mux (C1 D3). An attachable session carries its socket_dir
    // (tagged by the backend's list), so the embedded lane targets the sbmux dir.
    let mux = match common::real_mux() {
        Ok(m) => m,
        Err(code) => return code,
    };
    match mux.attach(&dir, zmx_name) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sb live: {e}");
            1
        }
    }
}

// --- raw-mode terminal handling (termios via libc; no new crate dep) ---

struct RawMode {
    orig: libc::termios,
    active: bool,
}

impl RawMode {
    fn enter() -> Option<RawMode> {
        unsafe {
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut orig) != 0 {
                return None;
            }
            let mut raw = orig;
            // cfmakeraw-lite: disable canonical mode + echo (status.ts setRawMode).
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(RawMode { orig, active: true })
        }
    }

    fn restore(&mut self) {
        if self.active {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.orig);
            }
            self.active = false;
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Poll stdin for readable data with a timeout (ms). Returns true if readable.
fn poll_stdin(timeout_ms: i32) -> bool {
    unsafe {
        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        libc::poll(&mut pfd, 1, timeout_ms) > 0 && (pfd.revents & libc::POLLIN) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::map_deliver_outcome;
    use dispatch::submit::DeliverOutcome;

    // -------------------------------------------------------------------------
    // punch item 11: fork-target transcript preflight.
    // -------------------------------------------------------------------------
    use super::fork_transcript_missing_error;
    use dispatch::model::{Session, SessionBranch, SessionStatus};

    /// A minimal fork-target Session fixture (the ls.rs pattern).
    fn fork_target(sid: &str, cwd: Option<&str>, status: SessionStatus) -> Session {
        Session {
            name: Some("src".to_string()),
            user_named: Some(true),
            session_id: sid.to_string(),
            code: None,
            sb_id: None,
            pid: None,
            status,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: cwd.map(String::from),
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "claude-code".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: SessionBranch::ColdJsonl,
        }
    }

    /// Tombstoned/stopped WITH a transcript on disk = a LEGAL fork (the fork
    /// reads the transcript, not the process). Status-blind by design.
    #[test]
    fn fork_preflight_tombstoned_with_transcript_is_legal() {
        let home = tempfile::tempdir().unwrap();
        let paths = dispatch::paths::SbPaths::from_home(home.path());
        // Plant the transcript where the claude provider seam resolves it
        // (projects_dir scan tier — any project subdir holding <sid>.jsonl).
        let proj = paths.projects_dir.join("-work-proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("uuid-tomb.jsonl"), "{}\n").unwrap();
        let target = fork_target("uuid-tomb", Some("/work/proj"), SessionStatus::Killed);
        let err =
            fork_transcript_missing_error(&dispatch::provider::ClaudeProvider, &paths, &target);
        assert_eq!(err, None, "tombstoned-with-transcript must stay forkable");
    }

    /// Transcript-less target → immediate teaching error naming the target, the
    /// sid, the searched root, and what to do. (The call site returns exit 1
    /// BEFORE any mint/claim/launch — no boot wait can ever start.)
    /// F4 (red-team r1): an EXISTING-but-unreadable projects root must NOT
    /// produce the false "no transcript exists" claim — the preflight SKIPS
    /// (fail-open, pre-B1 behavior) rather than refuse with a lie. Unix-only
    /// (chmod-000 semantics; root bypasses DAC so skip under uid 0).
    #[cfg(unix)]
    #[test]
    fn fork_preflight_fails_open_on_unreadable_projects_root() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores mode bits — the staging is meaningless.
        }
        let home = tempfile::tempdir().unwrap();
        let paths = dispatch::paths::SbPaths::from_home(home.path());
        std::fs::create_dir_all(&paths.projects_dir).unwrap();
        std::fs::set_permissions(&paths.projects_dir, std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let target = fork_target("uuid-hidden", Some("/work/proj"), SessionStatus::Cold);
        let err =
            fork_transcript_missing_error(&dispatch::provider::ClaudeProvider, &paths, &target);
        // Restore perms FIRST so the tempdir can clean up even if we assert-fail.
        std::fs::set_permissions(&paths.projects_dir, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(
            err, None,
            "an unreadable root must fail OPEN (skip), never claim 'no transcript exists'"
        );
    }

    #[test]
    fn fork_preflight_transcript_less_is_teaching_error() {
        let home = tempfile::tempdir().unwrap();
        let paths = dispatch::paths::SbPaths::from_home(home.path());
        let target = fork_target("uuid-ghost", Some("/work/proj"), SessionStatus::Cold);
        let msg =
            fork_transcript_missing_error(&dispatch::provider::ClaudeProvider, &paths, &target)
                .expect("transcript-less fork must be refused");
        // Error-text pins: target display name, sid, searched root, guidance.
        assert!(msg.contains("sb start: cannot fork \"src\""), "{msg}");
        assert!(
            msg.contains("no transcript exists for session id uuid-ghost"),
            "{msg}"
        );
        assert!(
            msg.contains(&paths.projects_dir.display().to_string()),
            "names the searched root: {msg}"
        );
        assert!(msg.contains("Pick another --fork source"), "{msg}");
        assert!(msg.contains("start fresh without --fork"), "{msg}");
    }

    // §3.5 exit contract: the three-way DeliverOutcome → exit mapping, all three
    // ways (ADR 0008). Pure return-code assertions; the stdout/stderr wording is
    // covered by the golden new_went_busy_exit.sh scenario (M4 Level 2).
    #[test]
    fn deliver_outcome_accepted_is_0() {
        assert_eq!(map_deliver_outcome(DeliverOutcome::Accepted, "wk"), 0);
    }

    #[test]
    fn deliver_outcome_stalled_is_10() {
        assert_eq!(map_deliver_outcome(DeliverOutcome::Stalled, "wk"), 10);
    }

    #[test]
    fn deliver_outcome_pidfile_missing_is_1_not_10() {
        // R1: a vanished PID file is infra (exit 1), explicitly NOT the stalled
        // code 10 — routing it to 10 would lie to an external bond spawn.
        assert_eq!(map_deliver_outcome(DeliverOutcome::PidFileMissing, "wk"), 1);
    }

    // -------------------------------------------------------------------------
    // §5.1 / G3 — priming-readiness-timeout emission (M3). The emission is the
    // reachable unit seam (the full create-boot path is jail-only, M5/G7c). The
    // mutation control: deleting the warn_emit in emit_priming_timeout REDs these.
    // -------------------------------------------------------------------------
    use super::emit_priming_timeout;
    use dispatch::boot::BootPhase;
    use dispatch::create::NewError;
    use dispatch::effects::{RealClock, RealEnv};
    use dispatch::events::{byname_key, parse_events};

    /// The byname events file emit_priming_timeout writes to, resolved the SAME
    /// way the function does (SB_HOME-honoring) so the test is hermetic.
    fn byname_events_file(home: &std::path::Path, name: &str) -> std::path::PathBuf {
        let state = dispatch::paths::SbPaths::from_home_env(home, &RealEnv).state_dir;
        dispatch::events::events_path(&state, &byname_key(name))
    }

    #[test]
    fn priming_timeout_pid_file_phase_emits_to_byname() {
        let home = tempfile::tempdir().unwrap();
        // m-4 (ack3-spec §8): keyed on the TYPED phase, not a detail string.
        emit_priming_timeout(home.path(), &RealEnv, &RealClock, "wk", BootPhase::PidFile);
        let file = byname_events_file(home.path(), "wk");
        let text = std::fs::read_to_string(&file).expect("byname events file written");
        let recs = parse_events(&text).records;
        assert_eq!(recs.len(), 1, "exactly one record");
        assert_eq!(recs[0].event, "priming-readiness-timeout");
        assert_eq!(recs[0].str_field("phase").as_deref(), Some("pid-file"));
        // waited_ms = the pid-phase default (40s); best-effort.
        assert_eq!(recs[0].u64_field("waited_ms"), Some(40_000));
        // No sessionId on a failed boot → keyed by name only.
        assert_eq!(recs[0].name.as_deref(), Some("wk"));
        assert!(recs[0].session.is_none());
    }

    #[test]
    fn priming_timeout_idle_phase_typed() {
        let home = tempfile::tempdir().unwrap();
        // m-4 (ack3-spec §8): the Idle phase is read TYPED, not parsed from wording.
        emit_priming_timeout(home.path(), &RealEnv, &RealClock, "wk", BootPhase::Idle);
        let file = byname_events_file(home.path(), "wk");
        let text = std::fs::read_to_string(&file).unwrap();
        let recs = parse_events(&text).records;
        assert_eq!(recs[0].str_field("phase").as_deref(), Some("idle"));
        assert_eq!(recs[0].u64_field("waited_ms"), Some(60_000));
    }

    /// m-4 REGRESSION TOOTH (ack3-spec §8): a BootTimeout whose detail string is
    /// REWORDED — it does NOT contain "did not reach idle" — but whose TYPED phase
    /// is `Idle` still files the event as "idle". The deleted string-match would
    /// have misfiled this as "pid-file" (the exact brittleness m-4 removes). We
    /// drive through the create-path destructure the real consumer uses, so the
    /// phase flows the same way production threads it.
    #[test]
    fn priming_timeout_reworded_idle_detail_still_files_idle() {
        let err = NewError::BootTimeout {
            name: "wk".to_string(),
            phase: BootPhase::Idle,
            // Deliberately REWORDED: no "did not reach idle" substring.
            detail: "session never settled to idle".to_string(),
        };
        let NewError::BootTimeout { phase, detail, .. } = &err else {
            panic!("constructed a BootTimeout");
        };
        // Guard the premise: the old string-match key is genuinely absent.
        assert!(!detail.contains("did not reach idle"));

        let home = tempfile::tempdir().unwrap();
        emit_priming_timeout(home.path(), &RealEnv, &RealClock, "wk", *phase);
        let file = byname_events_file(home.path(), "wk");
        let text = std::fs::read_to_string(&file).unwrap();
        let recs = parse_events(&text).records;
        // Typed phase wins: filed as "idle" despite the reworded detail.
        assert_eq!(recs[0].str_field("phase").as_deref(), Some("idle"));
        assert_eq!(recs[0].u64_field("waited_ms"), Some(60_000));
    }

    /// m-4 ZERO-SURFACE ASSERT (orc rider, ack3-spec §8): adding the typed `phase`
    /// to `NewError::BootTimeout` must NOT change the human surface. The Display
    /// output is byte-identical for BOTH phases (phase is not printed), and the
    /// create-path exit code stays 1.
    #[test]
    fn boot_timeout_display_byte_identical_both_phases_exit_unchanged() {
        let expected = "ERROR: Session \"wk\" did not reach idle state within timeout.\n\
                        The zmx session exists but Claude Code may not have booted.\n  \
                        Check: sb ls\n  Attach: sb connect wk\n  (boot detail here)";
        for phase in [BootPhase::PidFile, BootPhase::Idle] {
            let err = NewError::BootTimeout {
                name: "wk".to_string(),
                phase,
                detail: "boot detail here".to_string(),
            };
            assert_eq!(format!("{err}"), expected, "Display must not vary by phase");
            assert_eq!(err.exit_code(), 1, "BootTimeout stays on the exit-1 lane");
        }
    }
}
