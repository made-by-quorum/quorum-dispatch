//! Shared real-deps wiring for the REAL backends (spec §3). Centralizes the
//! backend-selected mux (C1 D3: QD_MUX → embedded/zmx via [`select_backend`] +
//! [`build_mux`]/[`build_mux_dirs`]), `RealEnv`/`RealClock`/`QdPaths`
//! construction, and the A1 `gather`+`join`+`assign_codes` pipeline `ls`/`info`/
//! `attach` all consume.

use std::path::Path;

use dispatch::effects::{is_pid_alive, Env, RealClock, RealEnv, RealProcessTable};
use dispatch::exec::RealExec;
use dispatch::join::{self, JoinOpts, MuxDirs};
use dispatch::model::Session;
use dispatch::mux::Mux;
use dispatch::mux_selector::{self, Backend};
use dispatch::paths::QdPaths;
use dispatch::registry;
use dispatch::relay_http::HttpRelayProbe;
use dispatch::resolve::{
    detect_live_id_collision, is_live_status, resolve_session_with_liveness, LiveIdCollision,
    LiveRow, Resolution,
};

/// Resolve HOME → `QdPaths` (L9a: never the real home directly — read the env).
/// Returns `Err(exit)` with a printed message if HOME is unset.
pub fn paths_from_home(env: &dyn Env) -> Result<QdPaths, i32> {
    match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => Ok(QdPaths::from_home(std::path::Path::new(&h))),
        None => {
            eprintln!("qd: HOME is not set — cannot resolve the session state dir.");
            Err(1)
        }
    }
}

/// The stable-id store path: `<qdHome>/state/ids.jsonl` where `qdHome =
/// QD_HOME || <home>/.quorum/dispatch` (P0 wave-1; the QD_HOME-honoring `from_home_env`
/// resolution, same as marks.jsonl). Returns `Err(exit)` if HOME is unset.
pub fn ids_store_path(env: &dyn Env) -> Result<std::path::PathBuf, i32> {
    let home = home_path(env)?;
    let paths = QdPaths::from_home_env(&home, env);
    Ok(dispatch::idstore::ids_path(&paths.state_dir))
}

/// Resolve HOME as a `PathBuf` (the embedded mux + qrmux dir resolution need the
/// raw home, not the `.claude`-layout `QdPaths`). Same L9a discipline.
fn home_path(env: &dyn Env) -> Result<std::path::PathBuf, i32> {
    match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => Ok(std::path::PathBuf::from(h)),
        None => {
            eprintln!("qd: HOME is not set — cannot resolve the session state dir.");
            Err(1)
        }
    }
}

/// Parse the QD_MUX backend ONCE (spec item 4). A bogus value prints the loud
/// named error + returns its DISTINCT exit code (G-SEL/G-NEG); valid values map
/// to [`Backend`]. The same parsed backend feeds BOTH the mux selection and the
/// gather/`MuxDirs` lane below — no divergent double-read of QD_MUX.
pub fn select_backend(env: &dyn Env) -> Result<Backend, i32> {
    mux_selector::parse_backend(env).map_err(|e| {
        eprintln!("{}", e.message);
        e.exit_code
    })
}

/// punch item 7: resolve the per-session render mode from the verb's
/// `--alt-screen` / `--inline` flags (clap marks them conflicting) + the
/// `render-default` config key. Precedence: flag > config > inline. SHARED by
/// start / resume / connect so every launch site resolves identically.
pub fn resolve_render_mode(m: &clap::ArgMatches, env: &dyn Env) -> dispatch::launch::RenderMode {
    let flag = if m.get_flag("alt-screen") {
        Some(dispatch::launch::RenderMode::AltScreen)
    } else if m.get_flag("inline") {
        Some(dispatch::launch::RenderMode::Inline)
    } else {
        None
    };
    dispatch::launch::resolve_render_mode(
        flag,
        dispatch::launch::render_default_from_config(env).as_deref(),
    )
}

/// Build the backend-selected mux as a `Box<dyn Mux>` (the loud-error path is
/// surfaced by [`select_backend`]; this assumes an already-validated backend).
pub fn build_mux(
    backend: Backend,
    home: &std::path::Path,
    env: &dyn Env,
) -> Result<Box<dyn Mux>, i32> {
    mux_selector::select_mux(backend, home, env).map_err(|e| {
        eprintln!("{}", e.message);
        e.exit_code
    })
}

/// Build the backend-selected [`MuxDirs`] the gather scans. zmx → canonical +
/// legacy (with the A14-2(c) test-lane scan-root override applied to the surviving
/// READ scan; production default = literal `/tmp`). embedded → the single resolved
/// qrmux dir (legacy EMPTY). Keyed off the SAME parsed backend as the mux.
pub fn build_mux_dirs(
    backend: Backend,
    home: &std::path::Path,
    env: &dyn Env,
) -> Result<MuxDirs, i32> {
    match backend {
        Backend::Zmx => {
            // A14-2(c): the surviving legacy READ scan honors QD_TEST_SCAN_ROOTS
            // (test lanes only); production stays literal /tmp. Visibility-only.
            let scan_roots =
                dispatch::zmx_dir::legacy_scan_roots(env, std::path::Path::new("/tmp"));
            let xdg = dispatch::zmx_dir::XdgFamily::from_env(env, env.uid());
            Ok(MuxDirs::zmx_roots(env, &scan_roots, Some(&xdg)))
        }
        Backend::Embedded => {
            let dir = dispatch::qrmux_dir::resolve_qrmux_dir(home, env).map_err(|msg| {
                eprintln!("qd: {msg}");
                1
            })?;
            Ok(MuxDirs::embedded(dir))
        }
    }
}

/// Run the A1 gather → join_sessions → assign_codes pipeline with the REAL deps,
/// returning the joined+coded session list. This is the shared `getAllSessions`
/// (session.ts:830-1048) + `assignShortCodes` (index.ts:55) path that ls / info /
/// attach all call.
///
/// `opts` controls includeAll / includeTombstoned / includePreview / limit, the
/// same knobs each verb passes (e.g. info uses include_all + include_preview;
/// attach uses defaults; ls passes the parsed flags).
pub fn all_sessions(opts: JoinOpts) -> Result<Vec<Session>, i32> {
    Ok(all_sessions_counted(opts)?.0)
}

/// [`all_sessions`] plus the cap-drop count (punch B5 item 2-D): the second
/// element is how many eligible rows the active cap truncated (0 when the view
/// is uncapped) — see [`join::join_sessions_counted`]. Only the `ls` verb's
/// truncation trailer consumes it.
pub fn all_sessions_counted(opts: JoinOpts) -> Result<(Vec<Session>, usize), i32> {
    let env = RealEnv;
    let paths = paths_from_home(&env)?;
    let home = home_path(&env)?;
    let exec = RealExec;

    // ONE QD_MUX parse drives BOTH the mux selection AND the MuxDirs lane (item 4).
    let backend = select_backend(&env)?;
    let mux = build_mux(backend, &home, &env)?;
    let mux_dirs = build_mux_dirs(backend, &home, &env)?;

    let pt = RealProcessTable::new(exec);
    // Relay: read sidecar files from disk first (get_relay_ports does that); if
    // NO sidecars exist, fall back to the real HTTP `/health` port-scan probe
    // (A4 / ADD-5 — the production scan, scanRelayPorts session.ts:185-212).
    let probe = HttpRelayProbe::new();
    let clock = RealClock;

    let inputs = join::gather_with_dirs(
        &paths,
        mux.as_ref(),
        &mux_dirs,
        &pt,
        &probe,
        &clock,
        &env,
        opts,
    );
    let (mut sessions, capped_out) = join::join_sessions_counted(&inputs, opts);
    join::assign_codes(&mut sessions);

    // P0 wave-1: fill stable ids from a READ-ONLY fold of the idstore — no lock
    // taken (torn trailing lines are tolerated by the fold). Reading never
    // writes; the lazy-mint backfill is `qd ls`'s alone (ls.rs).
    let ids = dispatch::idstore::fold(&ids_store_path(&env)?);
    dispatch::idstore::fill_qd_ids(&mut sessions, &ids);
    // WP-B5-iii obl-4: fill a fork's lineage (parent qdId) from the SAME fold —
    // so a fork's parent is discoverable through this live resolver path.
    dispatch::idstore::fill_lineage(&mut sessions, &ids);
    Ok((sessions, capped_out))
}

/// `parseInt(opts.limit, 10)` semantics for `-n` (index.ts:48): an explicit value
/// passes through; only that explicit value caps. A non-integer / absent value →
/// None (apply_list_cap treats non-positive/None as unset).
pub fn parse_limit(raw: Option<&String>) -> Option<i64> {
    raw.and_then(|s| qd_parse_int_prefix(s))
}

/// JS `parseInt` leading-integer-prefix parse ("12abc" → 12, "abc" → None). The
/// data layer's `apply_list_cap` then treats `<= 0` as unset.
fn qd_parse_int_prefix(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let mut chars = t.chars().peekable();
    let mut out = String::new();
    if let Some(&c) = chars.peek() {
        if c == '+' || c == '-' {
            out.push(c);
            chars.next();
        }
    }
    let mut saw = false;
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            out.push(c);
            saw = true;
            chars.next();
        } else {
            break;
        }
    }
    if !saw {
        return None;
    }
    out.parse::<i64>().ok()
}

/// Which mux backend is active, for backend-keyed user-facing wording (C1 redfix).
/// Distinct from [`Backend`] so the message layer never depends on the selector's
/// internal type and a future backend addition is a compile error here.
pub enum SendBackend {
    Embedded,
    Zmx,
}

/// Resolve the active backend for a USER-FACING message (the "no live mux session"
/// wording in `send:pty`). Parses QD_MUX with the SAME rule as the rest of the
/// engine; a bogus value (already rejected upstream on every path that calls this)
/// falls back to Embedded — the C1 DEFAULT — so the worst case still names the
/// default backend rather than the stale zmx wording.
pub fn send_backend_label() -> SendBackend {
    match mux_selector::parse_backend(&RealEnv) {
        Ok(Backend::Zmx) => SendBackend::Zmx,
        Ok(Backend::Embedded) | Err(_) => SendBackend::Embedded,
    }
}

/// The real, backend-selected mux as a `Box<dyn Mux>` (used by attach/live for the
/// live handoff). Parses QD_MUX once; a bogus value prints the loud named error +
/// returns its distinct exit code. The embedded backend needs HOME to resolve its
/// socket dir, so HOME-unset is also surfaced here.
pub fn real_mux() -> Result<Box<dyn Mux>, i32> {
    let env = RealEnv;
    let home = home_path(&env)?;
    let backend = select_backend(&env)?;
    build_mux(backend, &home, &env)
}

/// codex P1, R1 (codex-p1-spec section 2.3) — acting-verb refusal arming. ONE
/// source of truth for the unknown-provider refusal message across attach / kill /
/// resume / send:relay / send:pty: a resolved session whose provider is neither
/// "claude-code" nor "opencode" (opencode keeps each verb's existing parked
/// branch, FIRST + byte-identical) gets a LOUD refusal on stderr + exit 1.
///
/// Returns `Some(exit_code)` when the verb must refuse (already printed), `None`
/// when the verb may proceed. Each verb calls this right after session resolution.
///
/// STRUCTURALLY UNREACHABLE TODAY: the join defaults an absent provider to
/// claude-code and writes no provider field for claude rows, so no constructible
/// row carries an unknown provider — this is ARMED for P2, mirroring kill.rs's
/// parked-OC posture. MUTATION EVIDENCE: removing a call site OR this helper reds
/// the provider_field refusal tests.
pub fn refuse_unknown_provider(verb: &str, s: &Session) -> Option<i32> {
    match s.provider.as_str() {
        "claude-code" | "opencode" => None,
        other => {
            eprintln!(
                "qd {verb}: unknown provider \"{other}\" — this engine supports: claude-code."
            );
            Some(1)
        }
    }
}

// P0 start-surface rework (STATE 22): `cold_session_error` (the W1 ADD-26
// shared cold pointer) was REMOVED with the attach-verb retirement — connect's
// cold path auto-revives (and revive_claude prints its own loud error on
// failure), so no caller remained.

/// SHARED codex (daemon-hosted) redirect (W1 ADD-26): a `Hosting::Daemon`
/// session has NO terminal to attach. Emitted by `qd connect` (codex IS
/// supported, just not attachable — the verb-agnostic wording survived the
/// attach-verb retirement, STATE 22). eprintln!s + returns exit 1.
pub fn daemon_redirect(name: &str) -> i32 {
    eprintln!(
        "codex sessions are daemon-hosted (no terminal to attach). Drive it with: qd send:relay {name} <text>   ·   revive it with: qd resume {name}"
    );
    1
}

/// `resolveOrDie` (utils.ts:482-502): One → the session; None → `No session
/// matching "<q>"` exit 1; Many → ambiguous list exit 1. Shared by every verb
/// that resolves a `<session>` query (attach/info/live already; send:pty/
/// send:http/wait in A4).
pub fn resolve_or_die<'a>(query: &str, sessions: &'a [Session]) -> Result<&'a Session, i32> {
    // PID-AWARE liveness for the live-refinement (dup-session fix): a row counts as
    // "live" only when its status is live AND its process is genuinely alive. A
    // stale leftover whose on-disk status still says idle/busy but whose pid is DEAD
    // no longer collides with the real ALIVE row, so `qd resume/connect/attach <code
    // or name>` resolves to the one true session instead of dying with
    // "Ambiguous — matches 2 sessions". A pid of `None` or `0` means "no pid
    // recorded" (e.g. ZmxOnly rows) — fall back to the status-only view there so we
    // never drop a legitimately-live pid-less row.
    let is_alive = |s: &Session| {
        is_live_status(s.status)
            && match s.pid {
                Some(p) if p != 0 => is_pid_alive(p as i32),
                _ => true,
            }
    };
    match resolve_session_with_liveness(query, sessions, is_alive) {
        Resolution::One(s) => Ok(s),
        Resolution::None => {
            eprintln!("No session matching \"{query}\"");
            Err(1)
        }
        Resolution::Many(v) => {
            eprintln!("Ambiguous — \"{query}\" matches {} sessions:", v.len());
            for s in v {
                // P0 wave-2 addendum: the handle is the STABLE id (codes are
                // retired from display); id-less rows show "---" as before.
                let qd_id = s.qd_id.as_deref().unwrap_or("---");
                let name = s.name.as_deref().unwrap_or("(unnamed)");
                let id = dispatch::fmt::truncate_id_default(&s.session_id);
                let pid = s
                    .pid
                    .filter(|&p| p != 0)
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".to_string());
                eprintln!("  [{qd_id}] {name}\t{id}\tPID {pid}");
            }
            Err(1)
        }
    }
}

/// Gather the ALIVE registry rows (RAW, pre-dedup) whose `session_id == target_id`
/// (Pete feedback #6: duplicate session ids). Reads `<pid>.json` rows directly and
/// keeps only those whose pid is alive (`is_pid_alive`).
///
/// The RAW scan is the whole point: the join collapses two same-id LIVE rows to one
/// (keep-newest — CORRECT for the legitimate stale-old-pid + new-live-pid case, e.g.
/// codex resume leaving the old row), which HIDES a genuine collision (two distinct
/// alive processes sharing an id) from `resolve_or_die`. This helper sees the
/// unmerged truth so resume/connect can refuse loudly instead of silently picking.
///
/// An empty `target_id` returns empty (ZmxOnly rows carry "" and must not match;
/// the resume verb's own "no session ID" guard handles the empty case).
pub fn alive_rows_with_id(sessions_dir: &Path, target_id: &str) -> Vec<LiveRow> {
    if target_id.is_empty() {
        return Vec::new();
    }
    registry::read_entries(sessions_dir, false)
        .into_iter()
        .filter(|s| !s.tombstoned)
        .filter_map(|s| {
            let pid = s.entry.pid?;
            if s.entry.session_id.as_deref() == Some(target_id) && is_pid_alive(pid as i32) {
                Some(LiveRow {
                    session_id: target_id.to_string(),
                    name: s.entry.name.clone(),
                    pid,
                })
            } else {
                None
            }
        })
        .collect()
}

/// Live-id-collision refusal SHARED by resume / connect (Pete feedback #6). When
/// ≥2 ALIVE rows share `target_id` the verb CANNOT disambiguate — refuse LOUDLY and
/// list them. Returns `Some(1)` (already printed) to refuse, `None` to proceed.
///
/// A connect-style verb must call this too: connecting to drive one of two same-id
/// sessions would silently pick a survivor — exactly the bug. The single-alive
/// ("already alive") case is verb-SPECIFIC (resume refuses → attach; connect may
/// attach directly) and is NOT decided here — see [`alive_pid_for_id`].
pub fn refuse_id_collision(verb: &str, target_id: &str, sessions_dir: &Path) -> Option<i32> {
    let alive = alive_rows_with_id(sessions_dir, target_id);
    if let LiveIdCollision::Collision(rows) = detect_live_id_collision(target_id, &alive) {
        eprintln!(
            "qd {verb}: id collision — {} live sessions share id {} — refusing to act \
             (cannot disambiguate). Kill the duplicate(s) first:",
            rows.len(),
            dispatch::fmt::truncate_id_default(target_id)
        );
        for r in &rows {
            let name = r.name.as_deref().unwrap_or("(unnamed)");
            eprintln!("  {name}\tPID {}", r.pid);
        }
        return Some(1);
    }
    None
}

/// The single live pid carrying `target_id` IFF the session is actually alive
/// (exactly one alive row). `None` when truly cold OR when ≥2 collide (the caller
/// runs [`refuse_id_collision`] first for the ≥2 case). Resume uses this to harden
/// the must-be-cold gate against the deduped-status misread (SEAM 3): the join may
/// report a session Cold (dedup of a stale row) while a live pid still carries its
/// id — resuming would spawn a SECOND process on the same id.
pub fn alive_pid_for_id(sessions_dir: &Path, target_id: &str) -> Option<i64> {
    let alive = alive_rows_with_id(sessions_dir, target_id);
    match detect_live_id_collision(target_id, &alive) {
        LiveIdCollision::AlreadyAlive { pid } => Some(pid),
        _ => None,
    }
}

/// P0 wave-2 (spec-w2-env D4) — the resume same-name guard's SCAN: is a LIVE
/// registry session OTHER than the resume target currently holding `zmx_name`?
///
/// The hazard (spike §4): resume kills a same-name zmx pane before relaunching;
/// if cold session B (named "wk") is resumed by id while a DIFFERENT live
/// session A also derives zmx name "wk", B's resume would kill A's pane. The
/// scan walks non-tombstoned registry rows with an ALIVE pid whose DERIVED zmx
/// name (`derive_zmx_name(None, row.name, row.session_id)` — exactly how a
/// session's pane is named) equals `zmx_name`, excluding the target's own rows
/// (`session_id == target_session_id` — the legitimate own-stale-pane case).
/// Returns the holder's display id via the shared fallback chain
/// ([`dispatch::idstore::holder_display_id`]).
///
/// CASE-FOLDED match (red-team r5 F1, lead-adjudicated): names are
/// CASE-INSENSITIVE for uniqueness (the r4 ruling) and this guard is the
/// resume-side sibling of the create gate — byte-exact comparison here let
/// `resume` revive cold `worker` beside live `WORKER` (the exact end-state
/// the create-side fix prevents), and `--zmx-name` gave a shortcut route.
pub fn live_zmx_name_holder(
    sessions_dir: &Path,
    ids_path: &Path,
    zmx_name: &str,
    target_session_id: &str,
) -> Option<String> {
    let holder = registry::read_entries(sessions_dir, false)
        .into_iter()
        .filter(|s| !s.tombstoned)
        .find(|s| {
            s.entry.session_id.as_deref() != Some(target_session_id)
                && s.entry
                    .pid
                    .is_some_and(|p| p != 0 && is_pid_alive(p as i32))
                && dispatch::resume::derive_zmx_name(
                    None,
                    s.entry.name.as_deref(),
                    s.entry.session_id.as_deref().unwrap_or(""),
                )
                .eq_ignore_ascii_case(zmx_name)
        })?;
    Some(dispatch::idstore::holder_display_id(
        ids_path,
        holder.entry.session_id.as_deref(),
        holder.entry.pid,
    ))
}

/// The D4 guard's EXACT error line (factored so a unit pins the wording).
pub fn held_name_error_line(verb: &str, zmx_name: &str, holder: &str) -> String {
    format!(
        "qd {verb}: name \"{zmx_name}\" is held by running session {holder}; \
         rename or stop it first"
    )
}

/// P0 wave-2 (spec-w2-env D4): refuse a resume/revive whose stale same-name
/// kill would destroy a DIFFERENT live session's pane. `Some(1)` (already
/// printed) to refuse; `None` when no live other-holder exists — the target's
/// OWN stale pane keeps the existing kill-then-relaunch flow.
pub fn refuse_held_zmx_name(
    verb: &str,
    sessions_dir: &Path,
    ids_path: &Path,
    zmx_name: &str,
    target_session_id: &str,
) -> Option<i32> {
    let holder = live_zmx_name_holder(sessions_dir, ids_path, zmx_name, target_session_id)?;
    eprintln!("{}", held_name_error_line(verb, zmx_name, &holder));
    Some(1)
}

/// The live-pane refusal's EXACT error line (factored so a unit pins it).
pub fn live_pane_error_line(verb: &str, pane: &str, pid: i32) -> String {
    format!(
        "qd {verb}: a RUNNING pane named \"{pane}\" (pid {pid}) holds this name — \
         refusing to kill a live process. Stop it first (qd stop {pane}) or rename."
    )
}

/// Red-team r6 F1 (lead-adjudicated): clear stale same-name panes SAFELY.
///
/// "Stale" means DEAD. The D4 registry guard is blind to live sessions whose
/// registry row carries no name (a revived claude relaunches with `--resume
/// <id>` only — its pane keeps the title-derived name but its new row has
/// `name=None`), so the pane-side check must not trust the registry at all:
/// for every (case-folded, r4 ruling) name-matching pane across the scanned
/// dirs, if the pane's PROCESS IS ALIVE → refuse loudly (the resume target
/// itself was already proven non-live by the must-be-cold + id-collision
/// preflights, so an alive matching pane is necessarily someone else's);
/// only a DEAD pane is killed — by its FOUND name (panes are case-preserving),
/// and ALL dead matches are cleared, not just the first (r6 M2).
///
/// Returns `Err(1)` (already printed) on a live-pane or unknown-pid refusal
/// (r7 M1: pid <= 0 is unprovable, not dead), `Ok(())` after clearing
/// zero-or-more dead panes.
pub fn clear_stale_panes(
    verb: &str,
    mux: &dyn dispatch::mux::Mux,
    dirs: &[std::path::PathBuf],
    zmx_name: &str,
) -> Result<(), i32> {
    for dir in dirs {
        for pane in mux
            .list(dir)
            .unwrap_or_default()
            .into_iter()
            .filter(|z| z.name.eq_ignore_ascii_case(zmx_name))
        {
            // r7 M1: a non-positive pid is UNKNOWN, not dead (the zmx parser
            // maps a garbled pid to 0) — we cannot prove the pane is dead, so
            // refuse on missing evidence rather than kill.
            if pane.pid <= 0 {
                eprintln!(
                    "qd {verb}: pane \"{}\" reports no readable pid — cannot prove it is \
                     dead; refusing to clear it. Inspect with: zmx ls",
                    pane.name
                );
                return Err(1);
            }
            if is_pid_alive(pane.pid) {
                eprintln!("{}", live_pane_error_line(verb, &pane.name, pane.pid));
                return Err(1);
            }
            let _ = mux.kill(dir, &pane.name);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch::registry::RegistryEntry;

    // A pid that is reliably DEAD: max-ish, never a running process. is_pid_alive
    // → false (ESRCH), so a row keyed by it is the "stale dead-pid row" case.
    const DEAD_PID: i64 = 2_147_483_646;

    /// Minimal fake Mux for the clear_stale_panes pins: a fixed pane list and
    /// a recorded kill log. Everything else unreachable in these tests.
    struct PaneMux {
        panes: Vec<dispatch::mux::MuxSession>,
        kills: std::cell::RefCell<Vec<String>>,
    }
    impl dispatch::mux::Mux for PaneMux {
        fn list(&self, _d: &Path) -> std::io::Result<Vec<dispatch::mux::MuxSession>> {
            Ok(self.panes.clone())
        }
        fn list_raw(&self, _d: &Path) -> std::io::Result<Vec<dispatch::mux::MuxSession>> {
            Ok(self.panes.clone())
        }
        fn run_detached(
            &self,
            _d: &Path,
            _n: &str,
            _c: &str,
            _w: &Path,
        ) -> std::io::Result<dispatch::exec::ExecResult> {
            unreachable!("not exercised")
        }
        fn kill(&self, _d: &Path, name: &str) -> std::io::Result<i32> {
            self.kills.borrow_mut().push(name.to_string());
            Ok(0)
        }
        fn attach(&self, _d: &Path, _n: &str) -> std::io::Result<i32> {
            unreachable!("not exercised")
        }
        fn send(
            &self,
            _d: &Path,
            _n: &str,
            _t: &str,
        ) -> std::io::Result<dispatch::exec::ExecResult> {
            unreachable!("not exercised")
        }
        fn history(&self, _d: &Path, _n: &str) -> std::io::Result<String> {
            unreachable!("not exercised")
        }
        fn wait(&self, _d: &Path, _n: &[String]) -> std::io::Result<i32> {
            unreachable!("not exercised")
        }
    }

    fn pane(name: &str, pid: i32) -> dispatch::mux::MuxSession {
        dispatch::mux::MuxSession {
            name: name.to_string(),
            pid,
            clients: 0,
            created: 0,
            start_dir: String::new(),
            cmd: String::new(),
            current: false,
            socket_dir: None,
            ended: None,
            exit_code: None,
            zmx_status: None,
            err: None,
        }
    }

    /// Red-team r6 F1: an ALIVE name-matching pane (any case) is REFUSED —
    /// never killed (the registry guard is blind to revived no-name rows;
    /// "stale" means DEAD). Uses this test process's own pid as the live one.
    #[test]
    fn clear_stale_panes_refuses_alive_pane_any_case() {
        let me = std::process::id() as i32;
        let mux = PaneMux {
            panes: vec![pane("Fix-bug", me)],
            kills: std::cell::RefCell::new(vec![]),
        };
        let dirs = vec![std::path::PathBuf::from("/tmp/x")];
        let got = clear_stale_panes("resume", &mux, &dirs, "fix-bug");
        assert_eq!(got, Err(1), "alive case-variant pane must refuse");
        assert!(mux.kills.borrow().is_empty(), "NEVER kills a live pane");
    }

    /// Dead panes ARE cleared — ALL case-variants (r6 M2: not just the first),
    /// each killed by its FOUND name (panes are case-preserving).
    #[test]
    fn clear_stale_panes_kills_all_dead_variants_by_found_name() {
        let mux = PaneMux {
            panes: vec![
                pane("wk", DEAD_PID as i32),
                pane("WK", DEAD_PID as i32 - 1),
                pane("other", DEAD_PID as i32 - 2),
            ],
            kills: std::cell::RefCell::new(vec![]),
        };
        let dirs = vec![std::path::PathBuf::from("/tmp/x")];
        let got = clear_stale_panes("resume", &mux, &dirs, "wk");
        assert_eq!(got, Ok(()));
        assert_eq!(
            *mux.kills.borrow(),
            vec!["wk".to_string(), "WK".to_string()],
            "both dead variants cleared by FOUND name; 'other' untouched"
        );
    }

    /// r7 M1: pid <= 0 is UNKNOWN (zmx parser maps a garbled pid to 0), not
    /// dead — refuse, never kill on missing evidence.
    #[test]
    fn clear_stale_panes_refuses_unreadable_pid() {
        let mux = PaneMux {
            panes: vec![pane("wk", 0)],
            kills: std::cell::RefCell::new(vec![]),
        };
        let dirs = vec![std::path::PathBuf::from("/tmp/x")];
        let got = clear_stale_panes("resume", &mux, &dirs, "wk");
        assert_eq!(got, Err(1), "unprovable pid must refuse");
        assert!(mux.kills.borrow().is_empty(), "nothing killed");
    }

    fn row(dir: &Path, pid: i64, id: &str, name: &str) {
        let e = RegistryEntry {
            pid: Some(pid),
            session_id: Some(id.to_string()),
            name: Some(name.to_string()),
            status: Some("idle".to_string()),
            ..Default::default()
        };
        registry::write_entry(dir, &e).unwrap();
    }

    #[test]
    fn gather_keeps_only_alive_rows_matching_the_id() {
        // Rows are keyed `<pid>.json`, so distinct rows need distinct pids (as real
        // sessions have). `me` is the alive+matching row; a spawned child is an
        // alive row with the WRONG id (must be filtered out by id, not liveness).
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id() as i64;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id() as i64;
        row(dir.path(), me, "wanted", "live-match"); // alive + matching id → kept
        row(dir.path(), DEAD_PID, "wanted", "dead-match"); // matching id but DEAD → dropped
        row(dir.path(), child_pid, "other", "live-other"); // alive but WRONG id → dropped

        let got = alive_rows_with_id(dir.path(), "wanted");
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(got.len(), 1, "only the alive+matching row: {got:?}");
        assert_eq!(got[0].pid, me);
        assert_eq!(got[0].name.as_deref(), Some("live-match"));
    }

    #[test]
    fn single_alive_row_is_already_alive_not_a_collision() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id() as i64;
        row(dir.path(), me, "solo", "only");
        row(dir.path(), DEAD_PID, "solo", "stale"); // stale dead row sharing the id
                                                    // refuse_id_collision proceeds (≥2-alive only); alive_pid_for_id flags it
                                                    // alive (the SEAM-3 Cold-misread hardening input).
        assert_eq!(refuse_id_collision("resume", "solo", dir.path()), None);
        assert_eq!(alive_pid_for_id(dir.path(), "solo"), Some(me));
    }

    #[test]
    fn empty_or_absent_id_is_resumable() {
        let dir = tempfile::tempdir().unwrap();
        row(dir.path(), std::process::id() as i64, "", "zmx-only");
        assert!(
            alive_rows_with_id(dir.path(), "").is_empty(),
            "empty never matches"
        );
        assert_eq!(refuse_id_collision("resume", "", dir.path()), None);
        assert_eq!(alive_pid_for_id(dir.path(), "missing"), None);
    }

    // === P0 wave-2 (spec-w2-env D4): the resume same-name guard ===

    /// The spike hazard, pinned: cold session B (uuid-b, named "wk") resumed
    /// while a DIFFERENT live session A (uuid-a) also derives zmx name "wk" →
    /// the guard names A (by stable id when mapped) and blocks. The legitimate
    /// own-stale-pane case (only B's OWN rows hold the name) passes.
    #[test]
    fn resume_guard_blocks_live_other_holder_allows_own_stale() {
        let dir = tempfile::tempdir().unwrap();
        let ids_dir = tempfile::tempdir().unwrap();
        let ids_path = ids_dir.path().join("ids.jsonl");
        let me = std::process::id() as i64;

        // Own stale row (the resume TARGET's uuid) — alive or not, it is
        // excluded by session_id, so no holder is reported.
        row(dir.path(), me, "uuid-b", "wk");
        assert_eq!(
            live_zmx_name_holder(dir.path(), &ids_path, "wk", "uuid-b"),
            None,
            "the target's own row is never a blocking holder"
        );

        // A DIFFERENT live session holding the same derived name → blocked,
        // named by its STABLE id (seeded in the idstore).
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id() as i64;
        row(dir.path(), child_pid, "uuid-a", "wk");
        let mut g = || "ab3kx9mq".to_string();
        dispatch::idstore::mint_or_get_with(
            &ids_path,
            "uuid-a",
            Some("wk"),
            &dispatch::effects::FixedClock(0),
            &mut g,
        )
        .unwrap();
        let holder = live_zmx_name_holder(dir.path(), &ids_path, "wk", "uuid-b");
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(holder.as_deref(), Some("ab3kx9mq"));
    }

    /// Red-team r5 F1 (lead-adjudicated): the guard is CASE-FOLDED — a live
    /// session named `WORKER` blocks resuming a cold case-variant `worker`
    /// (and the `--zmx-name` shortcut), exactly as the create gate refuses a
    /// case-variant start (r4 ruling: names case-insensitive for uniqueness).
    /// Pre-fix the byte-exact compare revived `worker` beside live `WORKER`,
    /// recreating the r4 end-state through the resume path.
    #[test]
    fn resume_guard_blocks_case_variant_live_holder() {
        let dir = tempfile::tempdir().unwrap();
        let ids_path = dir.path().join("ids.jsonl");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id() as i64;
        row(dir.path(), child_pid, "uuid-a", "WORKER");
        // Resume target uuid-b derives zmx name "worker" — a case-variant of
        // the live holder's "WORKER" → blocked (holder reported).
        let holder = live_zmx_name_holder(dir.path(), &ids_path, "worker", "uuid-b");
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            holder.is_some(),
            "case-variant live holder must block the resume"
        );
    }

    #[test]
    fn resume_guard_skips_dead_tombstoned_and_other_names() {
        let dir = tempfile::tempdir().unwrap();
        let ids_path = dir.path().join("ids.jsonl");

        // Dead pid holding the name → not a live holder.
        row(dir.path(), DEAD_PID, "uuid-a", "wk");
        // Alive pid holding a DIFFERENT name → no match.
        row(dir.path(), std::process::id() as i64, "uuid-c", "other");
        assert_eq!(
            live_zmx_name_holder(dir.path(), &ids_path, "wk", "uuid-b"),
            None
        );

        // Tombstoned alive row holding the name → not a live holder.
        let e = RegistryEntry {
            pid: Some(std::process::id() as i64),
            session_id: Some("uuid-t".to_string()),
            name: Some("wk".to_string()),
            status: Some("idle".to_string()),
            ..Default::default()
        };
        registry::write_entry(dir.path(), &e).unwrap();
        registry::ensure_tombstone(dir.path(), std::process::id() as i64, Some(&e));
        assert_eq!(
            live_zmx_name_holder(dir.path(), &ids_path, "wk", "uuid-b"),
            None,
            "tombstoned rows are not live holders"
        );
    }

    /// An unmapped holder falls back to the truncated provider UUID, and the
    /// derived-name matching covers the unnamed case (`claude-<id8>`).
    #[test]
    fn resume_guard_uuid_fallback_and_derived_name_match() {
        let dir = tempfile::tempdir().unwrap();
        let ids_path = dir.path().join("ids.jsonl");
        let me = std::process::id() as i64;
        // No idstore line → display falls back to the truncated uuid.
        row(dir.path(), me, "uuid-live-holder", "wk");
        assert_eq!(
            live_zmx_name_holder(dir.path(), &ids_path, "wk", "uuid-b"),
            Some(dispatch::fmt::truncate_id_default("uuid-live-holder"))
        );
        // An UNNAMED live row derives `claude-<first8>` — the guard matches the
        // DERIVED pane name, exactly what the stale kill would target.
        let dir2 = tempfile::tempdir().unwrap();
        let e = RegistryEntry {
            pid: Some(me),
            session_id: Some("abcdef1234567890".to_string()),
            name: None,
            status: Some("idle".to_string()),
            ..Default::default()
        };
        registry::write_entry(dir2.path(), &e).unwrap();
        assert!(
            live_zmx_name_holder(dir2.path(), &ids_path, "claude-abcdef12", "uuid-b").is_some(),
            "derived-name holders are matched"
        );
    }

    /// D4's exact error wording, pinned (spec-w2-env: "name \"wk\" is held by
    /// running session <id>; rename or stop it first").
    #[test]
    fn held_name_error_line_is_pinned() {
        assert_eq!(
            held_name_error_line("resume", "wk", "ab3kx9mq"),
            "qd resume: name \"wk\" is held by running session ab3kx9mq; \
             rename or stop it first"
        );
        assert_eq!(
            held_name_error_line("connect", "wk", "ab3kx9mq"),
            "qd connect: name \"wk\" is held by running session ab3kx9mq; \
             rename or stop it first"
        );
    }

    #[test]
    fn two_alive_rows_same_id_collide_end_to_end() {
        // The real bug, end-to-end with TWO genuinely-live pids: the test process +
        // a spawned child. refuse_id_collision must refuse (exit 1); it is NOT
        // reported as merely "already alive".
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id() as i64;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id() as i64;
        row(dir.path(), me, "dup", "first");
        row(dir.path(), child_pid, "dup", "second");

        let verdict = refuse_id_collision("resume", "dup", dir.path());
        // Cleanup BEFORE asserting so a failed assert never leaks the child.
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(verdict, Some(1), "two live pids on one id must refuse");
        // And it is a Collision, not AlreadyAlive (≥2, so alive_pid_for_id is None).
    }
}
