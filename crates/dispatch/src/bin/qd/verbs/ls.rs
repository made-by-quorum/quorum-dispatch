//! `qd ls` (alias `list`) + the default action — REAL (A1 registry+codes+render+
//! join). Ported from index.ts:36-172.
//!
//! Options: `-a/--all`, `--live`, `--json`, `--short`, `--prefix <prefix>`,
//! `-n/--limit <count>`. Explicit-limit pass-through (index.ts:46-48): ONLY an
//! explicit `-n` passes a limit; the default cap (20) + `--all` uncapping live
//! in the data layer (`apply_list_cap`). Prefix filter runs AFTER short-code
//! assignment (index.ts:57-59).
//!
//! Punch B5 item 2 (orc-ruled C1+D, additive — the human default is UNCHANGED):
//! - `--live` — live sessions ONLY (the resolver's [`dispatch::resolve::is_live_status`]
//!   class: idle/busy/shell; cold + killed are tombstones), UNCAPPED. The
//!   scripting surface: cold history can neither displace live rows behind the
//!   default cap nor flood an uncapped dump. Composes with `--json`/`--short`/
//!   `--prefix`; an explicit `-n` caps the LIVE set. CONFLICTS with `--all`
//!   at parse (a session list is one liveness class per query — the B1
//!   render-flag precedent). UNLIKE the default view, `--live` does NOT apply
//!   the named-only filter — UNNAMED live sessions appear (a scripting
//!   consumer wanting live sessions shouldn't have unnamed ones hidden).
//! - Truncation trailer — when the capped DEFAULT view (no `--all`, no `--live`,
//!   no valid `-n`) drops eligible rows, print `… N more (qd ls --all)` to
//!   STDERR in text modes. `--json` NEVER carries it (machine surface stays
//!   clean); stderr keeps piped text-mode stdout clean too.

use clap::ArgMatches;

use dispatch::fmt::{format_tokens, relative_time, shorten_path, truncate_id_default};
use dispatch::join::JoinOpts;
use dispatch::model::{Session, SessionStatus};
use dispatch::render;

use super::common;

/// Emit a fully-built `qd ls` stdout payload, exiting CLEANLY on a broken pipe
/// instead of letting `println!`/`print!` PANIC (engine-hardening item 20).
///
/// `qd ls --json` is a machine surface the supervisor's watcher parses. The
/// payload is rendered WHOLE in memory first (render → to_pretty), so qd is
/// never the source of mid-document corruption. The one remaining hazard is the
/// WRITE: when the downstream consumer closes the pipe early — a `| head`, or a
/// monitor that times out under load and stops reading — the underlying write
/// returns `BrokenPipe`, and the `println!` macro PANICS: exit 101 plus a Rust
/// backtrace note on stderr, with a PARTIAL JSON document already on stdout. To
/// a consumer that does not check the exit code, that truncated document reads
/// as truth — corrupt is worse than wrong.
///
/// We instead exit 141 (128 + SIGPIPE), the conventional "downstream went away"
/// code every shell pipeline already understands (`head`/`jq`/`cat` all surface
/// it), with NO panic and NO backtrace. A consumer can now distinguish a
/// COMPLETE document (exit 0) from a TRUNCATED one (exit ≠ 0):
/// stream-complete-or-error, never a silent partial.
///
/// SCOPE (deliberate): this does NOT reset the process-wide SIGPIPE disposition.
/// The embedded qrmux daemon and relay server run inside THIS binary and rely on
/// EPIPE arriving as an `io::Result` they handle gracefully; a global
/// `SIG_DFL` reset would turn those graceful disconnects into process death.
/// The fix lives at the verb's own stdout write.
fn emit_or_pipe_exit(payload: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let res = out.write_all(payload.as_bytes()).and_then(|()| out.flush());
    if let Err(e) = res {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(141);
        }
        // Any other write failure on this surface is still a hard error — never
        // swallow it into a success exit (the partial-as-success trap).
        eprintln!("qd ls: failed writing output: {e}");
        std::process::exit(141);
    }
}

/// Default action: bare `qd` runs `ls` with no flags (index.ts:202-204). The
/// render surface is the WP-B7 PIECE 1 auto-detect (agent/pipe ⇒ JSON, human/TTY
/// ⇒ table) — a bare `qd | cat` from an agent now yields the machine surface.
pub fn run_default() -> i32 {
    run_inner(
        false,
        false,
        resolve_emit_json(false, false),
        false,
        None,
        None,
    )
}

pub fn run(m: &ArgMatches) -> i32 {
    let all = m.get_flag("all");
    let live = m.get_flag("live");
    let json = m.get_flag("json");
    let table = m.get_flag("table");
    let short = m.get_flag("short");
    let prefix = m.get_one::<String>("prefix").cloned();
    // index.ts:48 — `opts.limit ? parseInt(opts.limit, 10) : undefined`.
    let limit = common::parse_limit(m.get_one::<String>("limit"));
    run_inner(
        all,
        live,
        resolve_emit_json(json, table),
        short,
        prefix,
        limit,
    )
}

/// WP-B7 PIECE 1 — resolve whether `qd ls` emits the JSON machine surface vs. the
/// human table, from the explicit surface selectors (`--json`/`--table`) and the
/// auto-detected driver. `--json` ⇒ JSON; `--table` ⇒ table (forces the human
/// surface even for an agent caller, via `DriverOverride::Interactive`); neither ⇒
/// the I/O-follows-who-drives default (agent/pipe ⇒ JSON, human/TTY ⇒ table). This
/// is the thin production wiring over the unit-tested pure [`crate::driver::
/// ls_render_mode`] — the same seam as [`crate::driver::resolve_driver_real`].
///
/// `--short` deliberately does NOT reach here: it is a CONTENT modifier the text
/// branch applies AFTER this surface decision, so `--table --short` yields a short
/// TABLE (the agent escape hatch to short text), while an agent's bare `--short`
/// auto-flips to JSON and `--short` stays inert under JSON exactly as `--json
/// --short` does today (qd-supervisor-11-ratified, charge amend rider).
fn resolve_emit_json(json_flag: bool, table_flag: bool) -> bool {
    use crate::driver::{ls_render_mode, resolve_driver_real, DriverOverride, LsRender};
    let over = if table_flag {
        DriverOverride::Interactive
    } else {
        DriverOverride::None
    };
    let driver = resolve_driver_real(over, &dispatch::effects::RealEnv);
    matches!(ls_render_mode(json_flag, driver), LsRender::Json)
}

fn run_inner(
    all: bool,
    live: bool,
    emit_json: bool,
    short: bool,
    prefix: Option<String>,
    limit: Option<i64>,
) -> i32 {
    // The "valid -n" rule (a positive integer; `<= 0`/absent = unset, the
    // data layer's apply_list_cap convention) is spelled ONCE here and reused
    // at both consumers below (the live truncate + the trailer gate).
    let valid_limit = limit.filter(|&n| n > 0);

    // --live (B5 item 2): join the FULL view uncapped, then keep only the
    // resolver's live class. The data-layer cap must not run first — it would
    // cap the live+cold MIXTURE before the filter (the exact trap the flag
    // closes). An explicit `-n` is applied AFTER the filter (a limit on the
    // LIVE set), mirroring the default path's limit-before-prefix ordering.
    let opts = if live {
        JoinOpts {
            include_all: true,
            include_tombstoned: false,
            include_preview: false,
            limit: None,
        }
    } else {
        JoinOpts {
            include_all: all,
            include_tombstoned: all,
            include_preview: false,
            limit,
        }
    };
    let (mut sessions, capped_out) = match common::all_sessions_counted(opts) {
        Ok(s) => s,
        Err(code) => return code,
    };

    // WP-D part (a) — LIVENESS-GATE the `qd ls` render: a row shown live whose pid
    // is classified NOT-alive by the WP-A starttime classifier (zombie/gone/exited,
    // or a reused pid) is downgraded to `cold` (stop showing a dead/zombie/gone pid
    // as live; stop counting zombies alive). RENDER-path ONLY — acting verbs keep
    // the raw registry status (gating their resolve would wrongly refuse a session
    // whose registered pid is momentarily unverifiable). Codex rows are skipped
    // (daemon-hosted; their liveness is the rollout-tail model, not a claude
    // `(pid,starttime)`). One `/proc`+`ps` classify per live row; `ls` is not a hot
    // loop. Applied BEFORE the `--live` filter so a gated row drops out of `--live`.
    //
    // WP-B5-ii-a guarantee (ii) — additionally DAEMON-DOWN-GATE headless rows: the
    // owning per-session daemon can die while the orphaned claude child it launched
    // stays alive, so the `(pid,starttime)` classifier above would KEEP the row's
    // stale `busy`. `gated_ls_status_headless` composes a per-session
    // `<dir>/<name>.sock` connect probe BEFORE the claude-pid gate (lead RULING):
    // for a headless row whose daemon is DOWN, render `cold` (Fork A — reuse cold,
    // no new token) regardless of the orphan pid. Non-headless rows are unaffected
    // (the probe is skipped). RENDER-path only. The qrmux dir is resolved
    // best-effort; if it cannot be (HOME unset / non-embedded layout) the probe
    // returns Up for every row and the gate degrades to claude-pid-only (pre-WP
    // behavior) — headless rows only exist under the embedded backend anyway.
    {
        use dispatch::effects::Env;
        let src = dispatch::liveness::OsLiveness::new();
        let env = dispatch::effects::RealEnv;
        let qrmux_dir = env.var("HOME").filter(|h| !h.is_empty()).and_then(|h| {
            dispatch::qrmux_dir::resolve_qrmux_dir(std::path::Path::new(&h), &env).ok()
        });
        let daemon_src = dispatch::liveness::SocketDaemonLiveness::new(qrmux_dir);
        for s in &mut sessions {
            // codex rows: daemon-hosted (rollout-tail liveness), skipped here. acp/*
            // rows (Item 3): ALSO daemon-hosted (resident adapter + bridge) — their
            // liveness is the adapter pid + `--listen` endpoint, NOT a claude
            // `(pid,starttime)` or qrmux socket, so the headless gate would mis-classify
            // them. They get a primary-sourced acp Status override at render time
            // (`acp_human_status`) instead.
            if s.provider != "codex" && !s.provider.starts_with("acp/") {
                s.status = dispatch::liveness::gated_ls_status_headless(
                    s.status,
                    s.entrypoint.as_deref(),
                    s.name.as_deref(),
                    s.pid,
                    s.started_at_ms,
                    &daemon_src,
                    &src,
                );
            }
        }
    }

    if live {
        // ONE liveness definition: resolve::is_live_status (idle/busy/shell;
        // NOT cold, NOT killed) — the same class name-resolution uses. Status
        // classification only; pid-aliveness is `info --json`'s `live` field.
        // --live via include_all:true intentionally lifts the default view's
        // named-only filter, so UNNAMED live rows surface here that the default
        // view hides — the right call for a scripting consumer wanting live
        // sessions (declare-don't-hide; pinned in punch_b5_ls_live.rs).
        sessions.retain(|s| dispatch::resolve::is_live_status(s.status));
        if let Some(n) = valid_limit {
            sessions.truncate(n as usize);
        }
    }
    // D trailer (B5 item 2): fires ONLY on the capped DEFAULT view — no --all,
    // no --live, no VALID -n (an invalid -n falls back to the default cap
    // everywhere, so it can truncate and warrants the same loud trailer) —
    // and only when the cap actually dropped eligible rows. Emitted on stderr
    // in TEXT modes after the listing; never on --json. NOTE: capped_out is
    // counted PRE-prefix, so `qd ls --prefix wk` over cap reports the TOTAL
    // dropped, not the wk-scoped count — internally consistent (the --all
    // remedy is also prefix-less).
    let trailer = (!all && !live && valid_limit.is_none() && capped_out > 0)
        .then(|| format!("… {capped_out} more (qd ls --all)"));

    // P0 wave-1 LAZY MINT (the backfill path for pre-existing sessions): any
    // session with a provider session_id but no mapped stable id gets one
    // minted now, under the store's exclusive lock (concurrent `ls` is safe —
    // dedupe-by-session guarantees one id). Engine `ls` writing engine state is
    // ruled acceptable; a mint failure degrades to a warned id-less row.
    if let Ok(ids_path) = common::ids_store_path(&dispatch::effects::RealEnv) {
        for s in &mut sessions {
            if s.qd_id.is_none() && !s.session_id.is_empty() {
                match dispatch::idstore::mint_or_get(
                    &ids_path,
                    &s.session_id,
                    s.name.as_deref(),
                    &dispatch::effects::RealClock,
                ) {
                    Ok(id) => s.qd_id = Some(id),
                    Err(e) => eprintln!("qd ls: {e}"),
                }
            }
        }
    }

    // Prefix filter AFTER codes (index.ts:57-59): `s.name?.startsWith(prefix)`.
    if let Some(p) = &prefix {
        sessions.retain(|s| s.name.as_deref().is_some_and(|n| n.starts_with(p.as_str())));
    }

    // (L) Item 3 — PRIMARY-SOURCED acp Status, computed ONCE over the FINAL session list
    // and fed to BOTH the JSON surface (truthful `status`) and the default human table
    // (truthful Status column). Some only for acp/* rows → non-acp JSON/golden bytes are
    // untouched. (live pid + identity + endpoint reachability; never the stale field.)
    let acp_status_overrides: Vec<Option<(String, SessionStatus)>> = {
        use dispatch::effects::Env as _;
        let sessions_dir = dispatch::effects::RealEnv
            .var("HOME")
            .filter(|h| !h.is_empty())
            .map(|h| dispatch::paths::QdPaths::from_home(std::path::Path::new(&h)).sessions_dir);
        sessions
            .iter()
            .map(|s| sessions_dir.as_deref().and_then(|sd| acp_human_status(s, sd)))
            .collect()
    };

    if emit_json {
        // ls JSON surface (render::ls_json) — selected by `--json` OR the agent/pipe
        // auto-default (WP-B7 PIECE 1). The byte-faithful TS surface. Strays are an
        // additional surface appended by join_with_strays; the M2 ls path emits
        // the TS-faithful rows only (no strays) to keep --json parity exact.
        //
        // A6 §4.4: load the telemetry fold BEST-EFFORT (any read error → empty
        // map) and pass it. Empty fold ≡ today's bytes exactly (additive-only).
        //
        // --json early-returns trailer-FREE: the machine surface never carries a
        // non-JSON line (the trailer below is the single TEXT-mode emission).
        let fold = dispatch::telemetry::fold_from_env(&dispatch::effects::RealEnv);
        let fold_ref = if fold.is_empty() { None } else { Some(&fold) };
        // WP-B-CS-2 ADDITIVE: the D3 readiness facet (ready/silent/stuck), aligned
        // to the FINAL (post-filter) session list. Classified off the SAME OS
        // liveness layer the status gate uses (a per-row `ps`/`proc` read; `ls` is
        // not a hot loop, and this is the agent/JSON surface only). The OS layer
        // yields `silent`/none today; the (B) StreamLiveness overlay (B5) folds
        // `ready`/`stuck` in without touching this call site. Codex rows are
        // skipped (daemon-hosted; rollout-tail liveness, no (pid,start) facet).
        use dispatch::liveness::LivenessSource as _;
        let facet_src = dispatch::liveness::OsLiveness::new();
        let facets: Vec<Option<&'static str>> = sessions
            .iter()
            .map(|s| {
                if s.provider == "codex" {
                    return None;
                }
                match (s.pid, s.started_at_ms) {
                    (Some(pid), Some(start)) => dispatch::liveness::readiness_facet(
                        facet_src.classify(dispatch::liveness::ProcKey::new(pid as i32, start)),
                    )
                    .map(|r| r.as_str()),
                    _ => None,
                }
            })
            .collect();
        // (L) Item 3: the truthful acp `status` texts, aligned to `sessions`.
        let acp_status_texts: Vec<Option<String>> = acp_status_overrides
            .iter()
            .map(|o| o.as_ref().map(|(t, _)| t.clone()))
            .collect();
        let value =
            render::ls_json_full_acp(&sessions, &[], fold_ref, Some(&facets), &acp_status_texts);
        // `{}\n` reproduces `println!` byte-for-byte; emit_or_pipe_exit replaces
        // the panic-on-broken-pipe with a clean exit-141 (item 20).
        emit_or_pipe_exit(&format!("{}\n", render::to_pretty(&value)));
        return 0;
    }

    if short {
        // index.ts:66-70 shape — `<cyan handle>  <name||sessionId>`. P0 wave-1:
        // the handle is the stable-id shortest-unique prefix (codes retired
        // from display); id-less rows show "---" as codes did.
        let prefixes = dispatch::idstore::prefix_map(&sessions);
        // Build the whole listing, then emit once — same bytes as the per-row
        // `println!` loop, but routed through the broken-pipe-clean writer
        // (item 20: --short is a scripting surface too).
        let mut buf = String::new();
        for s in &sessions {
            let handle = s
                .qd_id
                .as_ref()
                .and_then(|id| prefixes.get(id))
                .map(String::as_str)
                .unwrap_or("---");
            let label = nonempty_name(s).unwrap_or_else(|| s.session_id.clone());
            buf.push_str(&format!("{}  {label}\n", cyan(handle)));
        }
        emit_or_pipe_exit(&buf);
    } else if sessions.is_empty() {
        emit_or_pipe_exit("No sessions found.\n");
    } else {
        // (L) Item 3: the acp rows ride the PRIMARY-SOURCED Status override (computed
        // once above); non-acp rows → None → the byte-identical golden path.
        emit_or_pipe_exit(&render_table_human_with(&sessions, &acp_status_overrides));
    }

    // ONE trailer emission, covering every TEXT mode (short / empty / table) —
    // a future text path can't forget it. --json already returned above.
    if let Some(t) = &trailer {
        eprintln!("{t}");
    }
    0
}

/// Shared table render for `qd live`'s refresh loop (status.ts renderTable).
/// `qd live` KEEPS the full wide (12-col) table — only the default `qd ls` /
/// bare `qd` human path was narrowed to 4 columns (W3). Do NOT route live
/// through `render_table_human`.
pub fn render_table_for_live(sessions: &[Session]) -> String {
    render_table(sessions)
}

/// The DEFAULT human table for `qd ls` and bare `qd` (W3, reworked P0 wave-1):
/// 5 columns, `Name | Id | Status | Last active | Tokens`, narrow enough for
/// mobile. Agents use `--json`; `qd live` keeps the wide `render_table`.
/// The `Id` column shows the STABLE id's shortest-unique prefix (min 2 chars)
/// among the listed sessions (idstore.rs — the addressable handle, replacing
/// the retired rotating 3-char code); id-less sessions show `---` as codes
/// did. Tokens reuse `fmt::format_tokens` (the SAME formatter `qd info` + the
/// wide table use: `0` → "0", `1500` → "1.5k") and are RIGHT-aligned (it's a
/// number); a zero-token session renders the natural "0" (no new sentinel).
/// ANSI-aware widths/padding (widths from VISIBLE/plain length), matching the
/// wide table.
/// (L) Item 3 — the DEFAULT human table with PRIMARY-SOURCED acp Status overrides aligned
/// to `sessions` (Some for acp rows, None for the rest → the byte-identical stored path).
fn render_table_human_with(
    sessions: &[Session],
    overrides: &[Option<(String, SessionStatus)>],
) -> String {
    let now = {
        use dispatch::effects::Clock;
        dispatch::effects::RealClock.now_ms()
    };
    render_table_human_at_with(sessions, now, overrides)
}

/// (L) Item 3 — the pure 3-way acp Status classification (THE distinct (L) revert seam),
/// derived from the LIVE primary probe (never the stale stored status):
///   - pid not alive                       → "stopped"      (red, like killed)
///   - pid alive ∧ our adapter for the ep  → "live"         (cyan, like idle)
///   - pid alive but identity-fail (reuse) → "dead-endpoint" (dim, like cold)
/// Identity is `cmdline_is_our_acp_daemon` (the live cmdline carries the recorded
/// `--listen <endpoint>`), the SAME liveness the resume gate uses — NO reachability
/// connect (it would misread a busy-but-alive adapter as dead AND can never crash ls on
/// a dead endpoint). REVERTING the wiring to read `s.status` (the stored field) instead
/// shows a stale "idle"/"busy" on a dead/reused acp pid — the FM-L1 wrong-data the
/// oracle reverts this seam to catch.
fn acp_status_classify(pid_alive: bool, identity_ok: bool) -> (&'static str, SessionStatus) {
    if !pid_alive {
        ("stopped", SessionStatus::Killed)
    } else if identity_ok {
        ("live", SessionStatus::Idle)
    } else {
        ("dead-endpoint", SessionStatus::Cold)
    }
}

/// (L) Item 3 — the acp Status override for ONE row (None for a non-acp row). Reads the
/// LIVE primary truth: adapter pid alive (`is_pid_alive`) + `cmdline_is_our_acp_daemon`
/// identity over the re-read `--listen <endpoint>`. Pure `/proc` reads — degrades cleanly
/// (a dead/stopped acp row reads "stopped"/"dead-endpoint"; ls NEVER touches the endpoint
/// so it cannot crash on a dead one).
fn acp_human_status(s: &Session, sessions_dir: &std::path::Path) -> Option<(String, SessionStatus)> {
    if !s.provider.starts_with("acp/") {
        return None;
    }
    let pid = s.pid.filter(|&p| p != 0);
    let pid_alive = pid.map(|p| dispatch::effects::is_pid_alive(p as i32)).unwrap_or(false);
    // Endpoint is NOT on the Session surface — re-read it off the registry row by pid.
    let endpoint = pid
        .and_then(|p| dispatch::registry::read_entry(sessions_dir, p))
        .and_then(|e| e.endpoint)
        .filter(|e| !e.is_empty());
    let identity_ok = match (pid, endpoint.as_deref()) {
        (Some(p), Some(ep)) => dispatch::acp_residence::cmdline_is_our_acp_daemon(
            dispatch::create_daemon::real_cmdline_probe(p).as_deref(),
            Some(ep),
        ),
        _ => false,
    };
    let (text, variant) = acp_status_classify(pid_alive, identity_ok);
    Some((text.to_string(), variant))
}

/// Maximum visible characters for the Name column in the human (`qd ls`) table.
/// Longer names are truncated to `NAME_COL_MAX - 1` chars + `…` (one character
/// ellipsis), so the displayed cell is always ≤ NAME_COL_MAX visible chars.
/// Does NOT affect the wide table used by `qd live`.
const NAME_COL_MAX: usize = 16;

/// `render_table_human` with the clock seam (`now`) injected — the pure core,
/// so tests/goldens can freeze `now` (mirrors `relative_time`, which takes
/// `now_ms` as a parameter rather than reading the clock inline).
/// Test-only convenience: the human table with NO acp overrides (the stored-status path
/// the goldens pin). Production always routes through `render_table_human_with`.
#[cfg(test)]
fn render_table_human_at(sessions: &[Session], now: i64) -> String {
    render_table_human_at_with(sessions, now, &[])
}

/// `render_table_human_at` with per-row acp Status overrides. `overrides[i]` Some →
/// that row's Status cell shows the primary-sourced acp text/color (riding the EXISTING
/// Status column); None (incl. an empty slice) → the unchanged stored-status path, so a
/// non-acp listing is BYTE-IDENTICAL to the pre-Item-3 output (the golden).
fn render_table_human_at_with(
    sessions: &[Session],
    now: i64,
    overrides: &[Option<(String, SessionStatus)>],
) -> String {
    let headers = ["Name", "Id", "Status", "Last active", "Tokens"];
    let max_name = NAME_COL_MAX;
    // Shortest-unique prefixes among the LISTED sessions' stable ids (min 2
    // chars) — pure read-time computation, the same pattern codes used.
    let prefixes = dispatch::idstore::prefix_map(sessions);

    let rows: Vec<Vec<Cell>> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let killed = s.status == SessionStatus::Killed;

            // Name: dim when killed, else plain; `s.name || "-"`; truncate@30.
            let mut name = nonempty_name(s).unwrap_or_else(|| "-".to_string());
            if name.chars().count() > max_name {
                name = format!("{}…", name.chars().take(max_name - 1).collect::<String>());
            }
            let name_cell = if killed {
                Cell::new(name.clone(), dim(&name))
            } else {
                Cell::plain_only(name)
            };

            // Id: cyan(stable-id prefix) if mapped else dim("---") — the same
            // cyan/dim treatment the code cell had.
            let id_cell = match s.qd_id.as_ref().and_then(|id| prefixes.get(id)) {
                Some(prefix) => Cell::new(prefix.clone(), cyan(prefix)),
                None => Cell::new("---", dim("---")),
            };

            // Status: colorStatus (idle=cyan busy=yellow shell=green cold=dim killed=red).
            // (L) Item 3: an acp row carries a PRIMARY-SOURCED override (live/stopped/
            // dead-endpoint), colored via the matching variant; non-acp rows keep the
            // exact stored-status text+color → the non-acp golden is byte-identical.
            let status_cell = match overrides.get(i).and_then(|o| o.as_ref()) {
                Some((text, variant)) => Cell::new(text.clone(), color_status(*variant, text)),
                None => {
                    let status = s.status.as_str().to_string();
                    Cell::new(status.clone(), color_status(s.status, &status))
                }
            };

            // Last active: relative_time || "-".
            let last = s
                .last_active_ms
                .map(|ms| relative_time(ms, now))
                .unwrap_or_else(|| "-".to_string());

            // Tokens: the SAME existing occupancy number `qd info` / `--json`
            // surface, via the SAME formatter (`fmt::format_tokens`); zero renders
            // the natural "0". Right-aligned (it's a number).
            let tokens_cell = Cell::plain_right(format_tokens(s.tokens));

            vec![
                name_cell,
                id_cell,
                status_cell,
                Cell::plain_only(last),
                tokens_cell,
            ]
        })
        .collect();

    render_cells(&headers, &rows)
}

/// Render header + separator + body for a table of `Cell` rows. Column widths =
/// max(header.length, ...rows.map(visLen)); header is plain padEnd; body is
/// padAnsi (pad by VISIBLE length). Shared by the wide + human tables.
fn render_cells(headers: &[&str], rows: &[Vec<Cell>]) -> String {
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let mut w = h.chars().count();
            for r in rows {
                w = w.max(r[i].plain.chars().count());
            }
            w
        })
        .collect();

    let mut out = String::new();
    let header_line = headers
        .iter()
        .enumerate()
        .map(|(i, h)| pad_plain(h, widths[i]))
        .collect::<Vec<_>>()
        .join("  ");
    out.push_str(&header_line);
    out.push('\n');
    let sep: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
    out.push_str(&sep.join("  "));
    out.push('\n');
    for r in rows {
        let line = r
            .iter()
            .enumerate()
            .map(|(i, c)| pad_ansi(c, widths[i]))
            .collect::<Vec<_>>()
            .join("  ");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// One rendered cell: the PLAIN text (used for width computation, visible length)
/// and the COLORED text (used for emission). TS computes widths from `visLen`
/// (ANSI-stripped) and pads with `padAnsi` (visible-length padding), so we carry
/// both halves rather than re-stripping codes.
struct Cell {
    plain: String,
    colored: String,
    /// When true the cell pads on the LEFT (right-aligned) — for numeric columns
    /// like Tokens. Default false = padEnd (left-aligned), matching the TS table.
    right: bool,
}

impl Cell {
    fn new(plain: impl Into<String>, colored: impl Into<String>) -> Cell {
        Cell {
            plain: plain.into(),
            colored: colored.into(),
            right: false,
        }
    }
    /// A cell whose colored form equals its plain form (no SGR).
    fn plain_only(plain: impl Into<String>) -> Cell {
        let p = plain.into();
        Cell {
            colored: p.clone(),
            plain: p,
            right: false,
        }
    }
    /// A no-SGR cell that pads on the LEFT (right-aligned) — for numeric columns.
    fn plain_right(plain: impl Into<String>) -> Cell {
        let p = plain.into();
        Cell {
            colored: p.clone(),
            plain: p,
            right: true,
        }
    }
}

/// Render the full (non-compact) table, index.ts:116-171, WITH ANSI color
/// matching TS (utils.ts colorStatus/colorZmx/colorRelay/c.cyan/c.dim) emitted
/// UNCONDITIONALLY — even piped (the corpus captures prove TS colors the piped
/// output). Widths from visible (plain) length; padding by visible length.
fn render_table(sessions: &[Session]) -> String {
    let headers = [
        "Id",
        "Prov",
        "Name",
        "ID",
        "PID",
        "Status",
        "zmx",
        "Relay",
        "Turns",
        "Tokens",
        "CWD",
        "Last active",
    ];
    let max_name = 30usize;
    let home = home_for_paths();
    let now = {
        use dispatch::effects::Clock;
        dispatch::effects::RealClock.now_ms()
    };
    // P0 wave-2 addendum: the wide table's handle column is the STABLE id's
    // shortest-unique prefix (the same treatment `qd ls` got) — the rotating
    // 3-char code is retired from display.
    let prefixes = dispatch::idstore::prefix_map(sessions);

    let rows: Vec<Vec<Cell>> = sessions
        .iter()
        .map(|s| {
            let killed = s.status == SessionStatus::Killed;

            // Id: cyan(stable-id prefix) if mapped else dim("---") — the same
            // cyan/dim treatment the code cell had (index.ts:129).
            let code_cell = match s.qd_id.as_ref().and_then(|id| prefixes.get(id)) {
                Some(prefix) => Cell::new(prefix.clone(), cyan(prefix)),
                None => Cell::new("---", dim("---")),
            };

            // Prov: keyed off the row's provider id. codex P1, R1
            // (codex-p1-spec section 3.2): the claude-code arm keeps today's EXACT
            // rendering ("CC" cyan); the parked opencode arm keeps "OC" magenta;
            // any OTHER provider value renders verbatim with DEFAULT styling
            // (plain, no SGR) — L8 permissive: an unknown provider never panics
            // and never kills the row (only ACTING verbs refuse it).
            let prov_cell = match s.provider.as_str() {
                "claude-code" => Cell::new("CC", cyan("CC")),
                "opencode" => Cell::new("OC", magenta("OC")),
                other => Cell::plain_only(other.to_string()),
            };

            // Name: dim when killed, else plain (index.ts:133). `s.name || "-"`
            // with truncation at maxName (slice(0,maxName-1)+"…").
            let mut name = nonempty_name(s).unwrap_or_else(|| "-".to_string());
            if name.chars().count() > max_name {
                name = format!("{}…", name.chars().take(max_name - 1).collect::<String>());
            }
            let name_cell = if killed {
                Cell::new(name.clone(), dim(&name))
            } else {
                Cell::plain_only(name)
            };

            // ID: truncateId || "-" (plain, index.ts:134).
            let id = {
                let t = truncate_id_default(&s.session_id);
                if t.is_empty() {
                    "-".to_string()
                } else {
                    t
                }
            };

            // PID: `s.pid ? String(pid) : "-"` (plain).
            let pid = s
                .pid
                .filter(|&p| p != 0)
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string());

            // Status: colorStatus (idle=cyan busy=yellow shell=green cold=dim killed=red).
            let status = s.status.as_str().to_string();
            let status_cell = Cell::new(status.clone(), color_status(s.status, &status));

            // zmx: colorZmx (attached=cyan detached=yellow else dim).
            let zmx_str = match &s.zmx_name {
                Some(_) => {
                    if s.zmx_clients.unwrap_or(0) > 0 {
                        "attached".to_string()
                    } else {
                        "detached".to_string()
                    }
                }
                None => "-".to_string(),
            };
            let zmx_cell = Cell::new(zmx_str.clone(), color_zmx(&zmx_str));

            // Relay: colorRelay (- → dim, else magenta).
            let relay_str = s
                .relay_port
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string());
            let relay_cell = Cell::new(relay_str.clone(), color_relay(&relay_str));

            let cwd = s
                .cwd
                .as_deref()
                .map(|c| shorten_path(c, &home))
                .unwrap_or_else(|| "-".to_string());
            let last = s
                .last_active_ms
                .map(|ms| relative_time(ms, now))
                .unwrap_or_else(|| "-".to_string());

            vec![
                code_cell,
                prov_cell,
                name_cell,
                Cell::plain_only(id),
                Cell::plain_only(pid),
                status_cell,
                zmx_cell,
                relay_cell,
                Cell::plain_only(s.turns.to_string()),
                Cell::plain_only(format_tokens(s.tokens)),
                Cell::plain_only(cwd),
                Cell::plain_only(last),
            ]
        })
        .collect();

    render_cells(&headers, &rows)
}

/// `padEnd` on a plain string (header cells).
fn pad_plain(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - len))
    }
}

/// `padAnsi` (utils.ts:176-179): emit the COLORED text, padding by the difference
/// between the target width and the VISIBLE (plain) length. Left-aligned by
/// default (padEnd); `cell.right` pads on the LEFT instead (numeric columns).
fn pad_ansi(cell: &Cell, width: usize) -> String {
    let vis = cell.plain.chars().count();
    let pad = width.saturating_sub(vis);
    let spaces = " ".repeat(pad);
    if cell.right {
        format!("{spaces}{}", cell.colored)
    } else {
        format!("{}{spaces}", cell.colored)
    }
}

// --- ANSI color helpers (utils.ts:138-169), emitted unconditionally ---

fn cyan(s: &str) -> String {
    format!("\x1b[36m{s}\x1b[0m")
}
fn dim(s: &str) -> String {
    format!("\x1b[2m{s}\x1b[0m")
}
fn yellow(s: &str) -> String {
    format!("\x1b[33m{s}\x1b[0m")
}
fn green(s: &str) -> String {
    format!("\x1b[32m{s}\x1b[0m")
}
fn red(s: &str) -> String {
    format!("\x1b[31m{s}\x1b[0m")
}
fn magenta(s: &str) -> String {
    format!("\x1b[35m{s}\x1b[0m")
}

/// colorStatus (utils.ts:148-157): idle=cyan busy=yellow shell=green cold=dim
/// killed=red.
fn color_status(st: SessionStatus, text: &str) -> String {
    match st {
        SessionStatus::Idle => cyan(text),
        SessionStatus::Busy => yellow(text),
        SessionStatus::Shell => green(text),
        SessionStatus::Cold => dim(text),
        SessionStatus::Killed => red(text),
    }
}

/// colorZmx (utils.ts:159-165): attached=cyan detached=yellow else dim.
fn color_zmx(z: &str) -> String {
    match z {
        "attached" => cyan(z),
        "detached" => yellow(z),
        _ => dim(z),
    }
}

/// colorRelay (utils.ts:167-169): "-" → dim, else magenta.
fn color_relay(r: &str) -> String {
    if r == "-" {
        dim(r)
    } else {
        magenta(r)
    }
}

/// `s.name || ...` truthiness: an empty name is falsy.
fn nonempty_name(s: &Session) -> Option<String> {
    s.name
        .as_deref()
        .filter(|n| !n.is_empty())
        .map(String::from)
}

/// Best-effort home for shorten_path (L9a: read the env, never bare homedir).
fn home_for_paths() -> String {
    std::env::var("HOME").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    //! W3 — the DEFAULT 4-column human table (`Name | Code | Status | Last
    //! active`). Determinism mirrors the codex_ls.rs golden harness: the clock
    //! seam (`now`) is frozen via `render_table_human_at`, so the relative-time
    //! cell is byte-stable. The 4-col table carries NO paths, so no `<HOME>`
    //! normalization is needed. The named golden `golden/ls-human.txt` is minted
    //! through the SAME assert_golden discipline (QD_REGEN_GOLDEN=1 to regen,
    //! byte-equality otherwise).
    use super::*;
    use dispatch::model::{SessionBranch, SessionStatus};
    use std::path::PathBuf;

    const NOW_MS: i64 = 1_717_500_300_000;

    fn golden_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
    }

    /// Byte-equality golden assert; QD_REGEN_GOLDEN=1 writes `actual`. Mirrors
    /// the codex_ls.rs `assert_golden` exactly.
    fn assert_golden(name: &str, actual: &str) {
        let path = golden_dir().join(name);
        if std::env::var("QD_REGEN_GOLDEN").is_ok() {
            std::fs::create_dir_all(golden_dir()).unwrap();
            std::fs::write(&path, actual).unwrap();
            eprintln!("regenerated golden {name}");
            return;
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing golden {path:?}: {e} (run QD_REGEN_GOLDEN=1)"));
        assert_eq!(actual, expected, "golden mismatch for {name}");
    }

    /// A minimal Session fixture. Defaults are inert; tests set the load-bearing
    /// fields (name, qd_id, status, last_active_ms, tokens).
    fn session(
        name: Option<&str>,
        qd_id: Option<&str>,
        status: SessionStatus,
        last_active_ms: Option<i64>,
        tokens: u64,
    ) -> Session {
        Session {
            name: name.map(String::from),
            user_named: None,
            session_id: "sid".to_string(),
            code: None,
            qd_id: qd_id.map(String::from),
            pid: None,
            status,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens,
            cwd: None,
            last_active_ms,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "claude-code".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: SessionBranch::LiveRegistry,
        }
    }

    // ================= (L) Item 3 — acp Status override tests =================

    /// The pure (L) seam: the 3-way classification from the LIVE probe.
    #[test]
    fn acp_status_classify_is_three_way() {
        // pid not alive → stopped (red/killed), regardless of identity.
        assert_eq!(super::acp_status_classify(false, true), ("stopped", SessionStatus::Killed));
        assert_eq!(super::acp_status_classify(false, false), ("stopped", SessionStatus::Killed));
        // alive + our adapter for the recorded endpoint → live (cyan/idle).
        assert_eq!(super::acp_status_classify(true, true), ("live", SessionStatus::Idle));
        // alive but identity-fail (reused pid) → dead-endpoint (dim/cold).
        assert_eq!(super::acp_status_classify(true, false), ("dead-endpoint", SessionStatus::Cold));
    }

    /// `acp_human_status`: a non-acp row → None (byte-identical stored path). An acp row
    /// with a STORED "busy"/"idle" status but a DEAD pid → "stopped" (the live probe wins
    /// over the stale field — the FM-L1 guard). REVERTING the wiring to read `s.status`
    /// would return "busy" here.
    #[test]
    fn acp_human_status_dead_pid_is_stopped_not_stale() {
        let tmp = tempfile::tempdir().unwrap();
        // non-acp → None.
        let mut claude = session(Some("wk"), Some("a01"), SessionStatus::Busy, None, 0);
        claude.pid = Some(2_000_000_001); // dead, irrelevant for non-acp
        assert_eq!(acp_human_status(&claude, tmp.path()), None);
        // acp row, stored Busy, DEAD pid → "stopped" (NOT the stale "busy").
        let mut acp = session(Some("acp-wk"), Some("a02"), SessionStatus::Busy, None, 0);
        acp.provider = "acp/claude-code".to_string();
        acp.pid = Some(2_000_000_001); // a very large unlikely-alive pid
        let got = acp_human_status(&acp, tmp.path());
        assert_eq!(
            got,
            Some(("stopped".to_string(), SessionStatus::Killed)),
            "a dead acp pid reads 'stopped' from the live probe, never the stored 'busy'"
        );
    }

    /// The override rides the EXISTING Status column (no new column); a non-acp row with
    /// no override is BYTE-IDENTICAL to the stored-status render (the golden contract).
    #[test]
    fn acp_status_override_rides_status_column_nonacp_byte_identical() {
        let mut acp = session(Some("acp-wk"), Some("a01"), SessionStatus::Idle, None, 0);
        acp.provider = "acp/claude-code".to_string();
        let claude = session(Some("wk"), Some("a02"), SessionStatus::Busy, None, 0);
        let rows = vec![acp, claude];

        // override the acp row's Status to a primary-sourced "live"; non-acp → None.
        let overrides = vec![Some(("live".to_string(), SessionStatus::Idle)), None];
        let out = render_table_human_at_with(&rows, NOW_MS, &overrides);
        let plain: String = out.lines().map(strip_ansi).collect::<Vec<_>>().join("\n");
        // still 5 columns (no NEW column), and the acp Status shows "live".
        assert_eq!(out.lines().next().unwrap().split_whitespace().collect::<Vec<_>>(), vec!["Name", "Id", "Status", "Last", "active", "Tokens"]);
        assert!(plain.contains("live"), "acp Status override rides the Status column: {plain:?}");
        assert!(plain.contains("busy"), "the non-acp row keeps its stored status: {plain:?}");

        // The NON-ACP row's rendered line is byte-identical whether or not overrides are
        // present (all-None == the stored path == the golden).
        let none_overrides: Vec<Option<(String, SessionStatus)>> = vec![None, None];
        let with_none = render_table_human_at_with(&rows, NOW_MS, &none_overrides);
        let plain_path = render_table_human_at(&rows, NOW_MS);
        assert_eq!(with_none, plain_path, "all-None overrides == the stored-status render");
    }

    /// A representative set: a normal claude row, a codex-provider row, a killed
    /// row, a cold row, and a no-id row. `last_active_ms` deltas chosen to
    /// exercise the s/m/h tiers of relative_time against the frozen NOW_MS.
    /// The first two stable ids share a 2-char prefix ("ab") so the Id column
    /// exercises the prefix-extension path (→ "ab3" / "ab4"); the others get
    /// the 2-char minimum.
    fn fixtures() -> Vec<Session> {
        let mut codex = session(
            Some("codex-worker"),
            Some("ab47qrst"),
            SessionStatus::Busy,
            Some(NOW_MS - 90_000), // 1m ago
            1_234_567,             // → "1.2M" (exercises the M tier + widest cell)
        );
        codex.provider = "codex".to_string();
        vec![
            session(
                Some("claude-worker"),
                Some("ab3kx9mq"),
                SessionStatus::Idle,
                Some(NOW_MS - 30_000), // 30s ago
                45_200,                // → "45.2k"
            ),
            codex,
            session(
                Some("dead-one"),
                Some("ef56wxyz"),
                SessionStatus::Killed,
                Some(NOW_MS - 7_200_000), // 2h ago
                1_500,                    // → "1.5k"
            ),
            session(
                Some("cold-row"),
                Some("gh78mnpq"),
                SessionStatus::Cold,
                Some(NOW_MS - 5_400_000), // 1h ago
                999,                      // → "999" (sub-1k plain integer)
            ),
            // no-id row: qd_id None → dim "---"; zero tokens → natural "0".
            session(Some("no-code-row"), None, SessionStatus::Idle, None, 0),
        ]
    }

    #[test]
    fn human_table_is_five_columns() {
        let out = render_table_human_at(&fixtures(), NOW_MS);
        // The header carries EXACTLY the five columns in order.
        let header = out.lines().next().unwrap();
        let plain: String = strip_ansi(header);
        let cols: Vec<&str> = plain.split_whitespace().collect();
        // "Last active" is two words → header has 6 whitespace-split tokens.
        assert_eq!(
            cols,
            vec!["Name", "Id", "Status", "Last", "active", "Tokens"]
        );
        for row in out.lines().skip(2) {
            let plain = strip_ansi(row);
            // Sanity-check the row is non-empty; exact layout is pinned by the
            // golden (human_table_golden).
            assert!(!plain.trim().is_empty());
        }
    }

    #[test]
    fn human_table_id_column_shows_shortest_unique_prefixes() {
        // The Id column carries the shortest-unique prefix (min 2 chars) among
        // the LISTED ids: "ab3kx9mq"/"ab47qrst" share "ab" → extend to "ab3"/
        // "ab4"; the unshared ids get the 2-char minimum; the id-less row shows
        // "---" exactly as code-less rows did.
        let out = render_table_human_at(&fixtures(), NOW_MS);
        let row_for = |needle: &str| -> String {
            out.lines()
                .map(strip_ansi)
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("row {needle} present"))
        };
        assert!(row_for("claude-worker").contains(" ab3 "), "{out}");
        assert!(row_for("codex-worker").contains(" ab4 "), "{out}");
        assert!(row_for("dead-one").contains(" ef "), "{out}");
        assert!(row_for("cold-row").contains(" gh "), "{out}");
        assert!(row_for("no-code-row").contains(" --- "), "{out}");
        // Never the full 8-char id in the narrow column.
        assert!(!strip_ansi(&out).contains("ab3kx9mq"), "{out}");
    }

    #[test]
    fn human_table_renders_token_counts_and_zero() {
        // Tokens use fmt::format_tokens (the SAME formatter `qd info` uses) and a
        // zero-token session renders the natural "0" — no new sentinel.
        let out = render_table_human_at(&fixtures(), NOW_MS);
        let plain: String = out.lines().map(strip_ansi).collect::<Vec<_>>().join("\n");
        // The non-zero counts surface in their abbreviated forms.
        assert!(plain.contains("45.2k"), "claude-worker tokens: {plain}");
        assert!(plain.contains("1.2M"), "codex-worker tokens: {plain}");
        assert!(plain.contains("1.5k"), "dead-one tokens: {plain}");
        assert!(plain.contains("999"), "cold-row tokens: {plain}");
        // The zero-token (no-code) row renders a bare "0" in its Tokens cell.
        let zero_row = out
            .lines()
            .map(strip_ansi)
            .find(|l| l.contains("no-code-row"))
            .expect("no-code-row present");
        assert!(
            zero_row.trim_end().ends_with('0'),
            "zero tokens → trailing \"0\": {zero_row:?}"
        );
    }

    #[test]
    fn human_table_tokens_right_aligned() {
        // Right-alignment: in the Tokens column the shorter "0" cell is padded on
        // the LEFT, so its trailing edge lines up with the wider "45.2k"/"1.2M".
        let out = render_table_human_at(&fixtures(), NOW_MS);
        let lines: Vec<String> = out.lines().map(strip_ansi).collect();
        let tokens_end = |needle: &str| -> usize {
            let row = lines.iter().find(|l| l.contains(needle)).unwrap();
            row.trim_end().chars().count()
        };
        // Every body row's trailing edge (the Tokens cell is last) ends at the
        // SAME visible column → right-aligned values share a right margin.
        assert_eq!(
            tokens_end("claude-worker"),
            tokens_end("no-code-row"),
            "right-aligned Tokens share a right edge"
        );
        assert_eq!(tokens_end("codex-worker"), tokens_end("cold-row"));
    }

    /// Name column cap: names longer than NAME_COL_MAX chars are truncated to
    /// NAME_COL_MAX-1 chars + `…`; names ≤ NAME_COL_MAX display in full; the
    /// column width never exceeds NAME_COL_MAX.
    #[test]
    fn human_table_name_truncation_at_16() {
        // 20-char name — well over the cap.
        let long20 = "session-worker-x1234";
        // 17 chars: one over NAME_COL_MAX, must be truncated.
        let at_cap = "exactly-16-chars!";
        // 16 chars exactly: NOT truncated (condition is > NAME_COL_MAX, not >=).
        let exact_16 = "exactly16charsXX";
        // 5 chars: well under cap, displayed verbatim.
        let short = "short";

        let rows = vec![
            session(Some(long20), Some("a01"), SessionStatus::Idle, None, 0),
            session(Some(at_cap), Some("a02"), SessionStatus::Idle, None, 0),
            session(Some(exact_16), Some("a03"), SessionStatus::Idle, None, 0),
            session(Some(short), Some("a04"), SessionStatus::Idle, None, 0),
        ];
        let out = render_table_human_at(&rows, NOW_MS);
        let plain: String = out.lines().map(strip_ansi).collect::<Vec<_>>().join("\n");

        // long20 (20 chars) → "session-worker-…" (15 chars + ellipsis = 16 visible)
        // "session-worker-x1234" first 15 chars = "session-worker-"
        assert!(
            plain.contains("session-worker-\u{2026}"),
            "20-char name truncated to 15+ellipsis: {plain:?}"
        );
        assert!(
            !plain.contains(long20),
            "20-char name must not appear verbatim: {plain:?}"
        );

        // at_cap (17 chars) → "exactly-16-chars\u{2026}" (16 chars - 1 + ellipsis)
        // The first 15 chars of at_cap are "exactly-16-char".
        assert!(
            plain.contains("exactly-16-char\u{2026}"),
            "17-char name truncated to 15+ellipsis: {plain:?}"
        );
        assert!(
            !plain.contains(at_cap),
            "17-char name must not appear verbatim: {plain:?}"
        );

        // exact_16 (16 chars) — NOT truncated (16 is not > 16)
        assert!(
            plain.contains(exact_16),
            "16-char name must appear verbatim (not truncated): {plain:?}"
        );

        // short (5 chars) — not truncated
        assert!(
            plain.contains(short),
            "short name must appear verbatim: {plain:?}"
        );

        // The Name column separator width ≤ NAME_COL_MAX.
        let sep_line = plain.lines().nth(1).unwrap();
        let name_sep_width = sep_line.chars().take_while(|c| *c == '─').count();
        assert!(
            name_sep_width <= NAME_COL_MAX,
            "Name column width {name_sep_width} exceeds cap {NAME_COL_MAX}: {sep_line:?}"
        );
    }

    #[test]
    fn human_table_golden() {
        let out = render_table_human_at(&fixtures(), NOW_MS);
        assert_golden("ls-human.txt", &out);
    }

    /// P0 wave-2 addendum: the WIDE table (`qd live`) shows the stable Id
    /// column (shortest-unique prefix), not the retired Code column.
    #[test]
    fn wide_table_shows_stable_id_not_code() {
        let out = render_table(&fixtures());
        let header = strip_ansi(out.lines().next().unwrap());
        assert!(header.starts_with("Id "), "Id header first: {header:?}");
        assert!(!header.contains("Code"), "Code is retired: {header:?}");
        let plain: String = out.lines().map(strip_ansi).collect::<Vec<_>>().join("\n");
        // The shared-prefix pair extends ("ab3"/"ab4"); the id-less row dims to ---.
        let row_for = |needle: &str| -> String {
            plain
                .lines()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("row {needle} present"))
                .to_string()
        };
        assert!(row_for("claude-worker").starts_with("ab3 "), "{plain}");
        assert!(row_for("codex-worker").starts_with("ab4 "), "{plain}");
        assert!(row_for("no-code-row").starts_with("---"), "{plain}");
    }

    #[test]
    fn human_table_empty_set_matches_caller_empty_behavior() {
        // The human PATH prints "No sessions found." for an empty set BEFORE it
        // ever calls render_table_human (run_inner short-circuits). So the table
        // fn itself, given an empty slice, renders just the header + separator
        // (no body rows) — this documents that the empty-set USER-VISIBLE string
        // is owned by run_inner, not the renderer.
        let out = render_table_human_at(&[], NOW_MS);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "header + separator only: {out:?}");
        assert_eq!(
            strip_ansi(lines[0]).trim_end(),
            "Name  Id  Status  Last active  Tokens"
        );
    }

    /// Strip SGR sequences for plain-text assertions.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // consume until 'm'
                for n in chars.by_ref() {
                    if n == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
