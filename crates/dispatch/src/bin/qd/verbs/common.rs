//! Shared real-deps wiring for the REAL backends (spec §3). Centralizes the
//! backend-selected mux (C1 D3: QD_MUX → embedded/zmx via [`select_backend`] +
//! [`build_mux`]/[`build_mux_dirs`]), `RealEnv`/`RealClock`/`QdPaths`
//! construction, and the A1 `gather`+`join`+`assign_codes` pipeline `ls`/`info`/
//! `attach` all consume.

use std::path::Path;

use dispatch::discovery::DiscoveryHealth;
use dispatch::effects::{is_pid_alive, Clock, Env, RealClock, RealEnv, RealProcessTable};
use dispatch::exec::RealExec;
use dispatch::join::{self, JoinOpts, MuxDirs};
use dispatch::model::{Session, SessionStatus};
use dispatch::mux::Mux;
use dispatch::mux_selector::{self, Backend};
use dispatch::paths::QdPaths;
use dispatch::relay_http::HttpRelayProbe;
use dispatch::resolve::{is_live_status, resolve_session_with_liveness, Resolution};

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
/// start / resume / attach so every launch site resolves identically.
/// Read the raw `render-default` value from the config file (the SAME
/// QD_HOME-honoring path `qd config` writes), or `None` when the file or key is
/// absent. Permissive (L8): a read failure never hard-fails a launch.
///
/// Lives HERE rather than in `dispatch::launch` (qd/qw split): it reads a plain,
/// non-secret preference, and having `launch` reach for it coupled the module
/// `provider.rs` depends on to the whole config/credential surface. Every caller
/// is CLI code, so the read belongs on this side of the boundary.
pub fn render_default_from_config(env: &dyn dispatch::effects::Env) -> Option<String> {
    let path = dispatch::secrets::resolve_config_path(env);
    let text = std::fs::read_to_string(path).ok()?;
    dispatch::secrets::get_plain_config_key(&text, dispatch::launch::RENDER_DEFAULT_KEY)
}

pub fn resolve_render_mode(m: &clap::ArgMatches, env: &dyn Env) -> dispatch::launch::RenderMode {
    let flag = if m.get_flag("alt-screen") {
        Some(dispatch::launch::RenderMode::AltScreen)
    } else if m.get_flag("inline") {
        Some(dispatch::launch::RenderMode::Inline)
    } else {
        None
    };
    dispatch::launch::resolve_render_mode(flag, render_default_from_config(env).as_deref())
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
///
/// The resolution itself is `mux_selector::resolve_mux_dirs` — it moved to qw with
/// [`MuxDirs`] because the lane layer needs the same answer. What stays HERE is
/// what has always been qd's half and only qd's half: printing the failure and
/// choosing the exit code, exactly as [`build_mux`] does over `select_mux`.
pub fn build_mux_dirs(
    backend: Backend,
    home: &std::path::Path,
    env: &dyn Env,
) -> Result<MuxDirs, i32> {
    mux_selector::resolve_mux_dirs(backend, home, env).map_err(|msg| {
        eprintln!("qd: {msg}");
        1
    })
}

/// lsview A4 fix (CF-F1): the process table the `ls` bare-proc gather reads.
/// TEST LANES ONLY — `QD_TEST_NO_BARE_PROCS` (any non-empty value) selects the
/// fixture table (empty bare gather) so a jailed integration test is hermetic
/// against real bare harnesses on the host; the test process table cannot be
/// jailed the way `HOME`/`ZMX_DIR` jail the registry, so this is the reachable-
/// across-the-process-boundary suppression the spawned `qd` binary needs (same
/// discipline as `QD_TEST_SCAN_ROOTS`, above). Unset ⇒ the real `ps`/`lsof`
/// table (the production default) — bare rows render exactly as today.
pub fn ls_bare_process_table() -> Box<dyn dispatch::effects::ProcessTable> {
    if RealEnv
        .var("QD_TEST_NO_BARE_PROCS")
        .filter(|v| !v.is_empty())
        .is_some()
    {
        Box::new(dispatch::effects::FixtureProcessTable::default())
    } else {
        Box::new(RealProcessTable::new(RealExec))
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
    let (sessions, capped_out, _health) = all_sessions_full(opts)?;
    Ok((sessions, capped_out))
}

/// [`all_sessions_counted`] plus the gather's [`DiscoveryHealth`] — which
/// effectful discovery reads FAILED, as opposed to legitimately finding nothing.
///
/// The join drops [`join::JoinInputs`] on the floor, so without this the fact
/// that `ps` was refused never reaches a verb: a `relay_port: None` produced by
/// a denied read is indistinguishable from a session that genuinely has no
/// relay. Callers that REPORT an absence (`send`'s refusal, `info`'s field
/// rendering) take this door so they can say "undetermined" honestly. Callers
/// that merely list rows keep the narrower signatures.
pub fn all_sessions_full(opts: JoinOpts) -> Result<(Vec<Session>, usize, DiscoveryHealth), i32> {
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

    // ITEM 1 — loud at the source. A degraded gather is reported ONCE per
    // process, on stderr, the moment it is observed. This is the cheapest and
    // largest part of the fix: an agent that sees `ps` was refused does not
    // need to reverse-engineer a downstream "no live receive path" at all.
    warn_if_degraded(&inputs.discovery);

    Ok((sessions, capped_out, inputs.discovery))
}

/// Print the degradation warning at most once per process. Stderr only — no
/// stdout surface (including `--json`) changes shape, so scripted consumers are
/// byte-unaffected.
fn warn_if_degraded(health: &DiscoveryHealth) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    if !health.is_degraded() {
        return;
    }
    WARNED.call_once(|| {
        eprintln!("qd: warning: session discovery is degraded — {}", health.evidence());
        eprintln!(
            "qd: warning: relay and mux facts may be missing; absent values below are UNKNOWN, not confirmed absent."
        );
        if let Some(hint) = health.hint() {
            eprintln!("qd: warning: {hint}");
        }
    });
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

// `SendBackend` + `send_backend_label` USED to live here: "which mux backend is
// active", for the backend-keyed `send:pty` wording (C1 redfix) and for `new -p`'s
// `chunks-delivered.ack_source` label. Both consumers are in `quorum-qw` now — the
// pane carrier's own wording, and `delivery::pty::ack_source_label`, which
// `delivery::priming` reuses — and the decider went with the second of them as
// `delivery::pty::is_zmx_backend`, fallback preserved: a bogus `QD_MUX` answers
// EMBEDDED, the C1 default, rather than the stale zmx wording. One decider, not a
// qd copy that could drift from the qw one.

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
// shared cold pointer) was removed when the human verb gained auto-revive; attach's
// cold path auto-revives (and revive_claude prints its own loud error on
// failure), so no caller remained.

/// SHARED codex (daemon-hosted) redirect (W1 ADD-26): a `Hosting::Daemon`
/// session has NO terminal to attach. Emitted by `qd attach` (codex IS
/// supported, just not attachable — the verb-agnostic wording survived the
/// attach-verb retirement, STATE 22). eprintln!s + returns exit 1.
pub fn daemon_redirect(name: &str) -> i32 {
    eprintln!(
        "codex sessions are daemon-hosted (no terminal to attach). Drive it with: qd send:relay {name} <text>   ·   revive it with: qd resume {name}"
    );
    1
}

/// `resolveOrDie` (utils.ts:482-502): One → the session; None → `No session
/// matching "<q>"` exit 1; Many → ambiguous list exit 1.
///
/// PRIVATE by design (resolver-cap fix, Option B): the slice-taking form is the
/// footgun that let a DISPLAY cap leak into RESOLUTION — a caller could hand it a
/// capped slice. It is no longer callable from any verb. The ONLY entry points are
/// the sealed [`resolve_session_uncapped`] / [`resolve_session_uncapped_in_list`]
/// below, which own their uncapped gather and make a capped resolution
/// UNEXPRESSIBLE by construction.
fn resolve_or_die<'a>(query: &str, sessions: &'a [Session]) -> Result<&'a Session, i32> {
    // PID-AWARE liveness for the live-refinement (dup-session fix): a row counts as
    // "live" only when its status is live AND its process is genuinely alive. A
    // stale leftover whose on-disk status still says idle/busy but whose pid is DEAD
    // no longer collides with the real ALIVE row, so `qd resume/attach <code
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
            let env = RealEnv;
            let now_ms = RealClock.now_ms();
            let rows = dispatch::effects::process_rows(&RealExec).unwrap_or_default();
            let relays = home_path(&env)
                .map(|home| QdPaths::from_home_env(&home, &env))
                .map(|paths| {
                    let candidates = dispatch::relay::get_relay_ports(
                        &paths.relay_dir,
                        &dispatch::relay_http::HttpRelayProbe::new(),
                    );
                    dispatch::adoption::verify_live_relays(
                        &candidates,
                        &dispatch::relay_http::CcRelay::new(),
                        &is_pid_alive,
                    )
                })
                .unwrap_or_default();
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
                let access = dispatch::adoption::classify_session(s, &relays, &rows, &is_pid_alive);
                let mgmt = access.management.as_str();
                let liveness = if is_alive(s) { "alive" } else { "dead" };
                let age = s
                    .started_at_ms
                    .map(|ms| dispatch::fmt::relative_time(ms, now_ms))
                    .unwrap_or_else(|| "-".to_string());
                eprintln!("  [{qd_id}] {name}\t{id}\tPID {pid}\t{mgmt}\t{liveness}\tstarted {age}");
            }
            Err(1)
        }
    }
}

/// THE sealed, uncapped resolution entry point for the ACTION verbs (attach /
/// resume / fork / stop / send:pty / send:http / send:relay / wait). Resolves a
/// `<session>` handle (name | full UUID | qdId | prefix) against the FULL session
/// universe — resolution is NEVER subject to the `ls`/`live` DISPLAY cap.
///
/// Tombstones are always eligible, so a stopped session is FOUND, then rejected
/// post-resolve by the caller via [`reject_if_tombstoned`] with a clear "it is
/// stopped — resume it first" message instead of a misleading phantom miss (D-2).
/// The accept-set verbs (stop / resume / fork) skip that rejection and act on the
/// tombstone directly.
///
/// Returns an OWNED `Session`: the matcher borrows the gathered slice, which is
/// dropped here. This is the SEALED door — `include_all` / `include_tombstoned` /
/// `limit` are hardcoded (preview off), so a capped resolution is UNEXPRESSIBLE by
/// construction and the cap-leaks-into-resolution class cannot return through it.
pub fn resolve_session_uncapped(query: &str) -> Result<Session, i32> {
    Ok(resolve_session_uncapped_in_list(query, false)?.0)
}

/// [`resolve_session_uncapped`] that ALSO returns the [`DiscoveryHealth`] of the
/// gather the resolution ran against. Same sealed cap axes — this adds a
/// diagnostic output, never a resolution knob.
///
/// `send` is the caller: it must distinguish a session that HAS no receive path
/// from one whose receive path could not be READ, and that distinction lives in
/// the gather, not in the resolved row.
pub fn resolve_session_uncapped_with_health(
    query: &str,
) -> Result<(Session, DiscoveryHealth), i32> {
    let (session, _sessions, health) = resolve_session_uncapped_in_list_with_health(query, false)?;
    Ok((session, health))
}

/// [`resolve_session_uncapped`] that ALSO returns the full gathered list the
/// resolution ran against, plus a caller-chosen `include_preview`. `info` is the
/// only caller: it needs both the resolved row AND the list, to compute the `qdId`
/// shortest-unique prefix (`idstore::prefix_map`) for `--json` exactly as `ls`
/// does, and it passes `include_preview = true` to render the preview turns. Cap
/// axes hardcoded here too — `include_preview` is the SOLE knob; the list is
/// uncapped and a cap stays unexpressible.
pub fn resolve_session_uncapped_in_list(
    query: &str,
    include_preview: bool,
) -> Result<(Session, Vec<Session>), i32> {
    let (session, sessions, _health) =
        resolve_session_uncapped_in_list_with_health(query, include_preview)?;
    Ok((session, sessions))
}

/// [`resolve_session_uncapped_in_list`] plus the gather's [`DiscoveryHealth`].
/// `info` takes this door so it can render an unread field as `unknown (…)`
/// rather than as `-`, which would assert an absence it never observed.
pub fn resolve_session_uncapped_in_list_with_health(
    query: &str,
    include_preview: bool,
) -> Result<(Session, Vec<Session>, DiscoveryHealth), i32> {
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview,
        limit: None,
    };
    let (sessions, _capped, health) = all_sessions_full(opts)?;
    let session = resolve_or_die(query, &sessions)?.clone();
    Ok((session, sessions, health))
}

/// D-2 post-resolve tombstone rejection. A resolved session whose status is
/// `Killed` (a tombstone) is FOUND but not actionable for the reject-set verbs
/// (attach / send:pty / send:http / send:relay / wait): print the clear
/// "found it, but it is stopped — resume it first" message and return `Err(1)`,
/// never the misleading `No session matching`. The accept-set verbs (stop /
/// resume / fork) do NOT call this — a tombstone is a legitimate target for them.
pub fn reject_if_tombstoned(query: &str, session: &Session) -> Result<(), i32> {
    if session.status == SessionStatus::Killed {
        let id = session
            .qd_id
            .clone()
            .unwrap_or_else(|| dispatch::fmt::truncate_id_default(&session.session_id));
        let label = session.name.as_deref().unwrap_or(query);
        eprintln!("found \"{label}\" ({id}) but it is stopped — resume it first");
        return Err(1);
    }
    Ok(())
}

/// Live-id-collision refusal SHARED by resume / attach / send (Pete feedback #6).
/// Returns `Some(1)` (already printed) to refuse, `None` to proceed.
///
/// The DECISION and the wording moved to [`dispatch::resume::id_collision_refusal`]
/// with the acp resume choreography (which runs the same preflight in a
/// load-bearing position and could not reach back into this crate). This is the
/// printing wrapper the three other verbs still call — the refusal is MULTI-LINE,
/// so it prints the block the core hands back, one line at a time.
pub fn refuse_id_collision(verb: &str, target_id: &str, sessions_dir: &Path) -> Option<i32> {
    let refusal = dispatch::resume::id_collision_refusal(sessions_dir, target_id)?;
    for line in refusal.lines(verb) {
        eprintln!("{line}");
    }
    Some(refusal.exit_code())
}

/// The single live pid carrying `target_id` IFF the session is actually alive.
/// Re-exported from [`dispatch::resume`], which owns it now — see
/// [`refuse_id_collision`] on why it moved.
pub use dispatch::resume::alive_pid_for_id;

#[cfg(test)]
mod tests {

    /// Moved here with `render_default_from_config` when it left
    /// `dispatch::launch` (qd/qw split). Kept so the relocation did not quietly
    /// drop coverage: the read must still honour QD_HOME and stay permissive.
    #[test]
    fn render_default_from_config_reads_qd_home_config() {
        struct E(std::collections::HashMap<String, String>);
        impl dispatch::effects::Env for E {
            fn var(&self, k: &str) -> Option<String> {
                self.0.get(k).cloned()
            }
            fn uid(&self) -> u32 {
                0
            }
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let qd_home = tmp.path().join("qdhome");
        std::fs::create_dir_all(&qd_home).unwrap();
        std::fs::write(
            qd_home.join("config.toml"),
            "render-default = \"alt-screen\"\n",
        )
        .unwrap();

        let mut m = std::collections::HashMap::new();
        m.insert(
            "QD_HOME".to_string(),
            qd_home.to_string_lossy().into_owned(),
        );
        assert_eq!(
            render_default_from_config(&E(m)).as_deref(),
            Some("alt-screen")
        );

        // Absent file -> None, never an error (L8).
        let empty = tmp.path().join("nothing");
        std::fs::create_dir_all(&empty).unwrap();
        let mut m2 = std::collections::HashMap::new();
        m2.insert("QD_HOME".to_string(), empty.to_string_lossy().into_owned());
        assert_eq!(render_default_from_config(&E(m2)), None);
    }
    use super::*;
    use dispatch::registry::RegistryEntry;

    // A pid that is reliably DEAD: max-ish, never a running process. is_pid_alive
    // → false (ESRCH), so a row keyed by it is the "stale dead-pid row" case.
    const DEAD_PID: i64 = 2_147_483_646;

    fn row(dir: &Path, pid: i64, id: &str, name: &str) {
        let e = RegistryEntry {
            pid: Some(pid),
            session_id: Some(id.to_string()),
            name: Some(name.to_string()),
            status: Some("idle".to_string()),
            ..Default::default()
        };
        dispatch::registry::write_entry(dir, &e).unwrap();
    }


}
