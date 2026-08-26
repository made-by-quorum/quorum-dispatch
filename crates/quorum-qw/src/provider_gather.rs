//! The provider-facing half of the session gather — every effectful read the
//! `qd` join needs, and nothing that decides anything.
//!
//! # Why the cut is here
//!
//! `join.rs` (in `qd`) has always been two things bolted together: a PURE
//! decider — the mechanical port of TS `getAllSessions`, which takes
//! pre-gathered inputs and emits rows — and the I/O that fills those inputs.
//! The decider is a merge over data qd owns (mux panes, the registry, relay
//! ports, the process table, the list cap, the tombstone policy). The I/O half
//! is almost entirely PROVIDER knowledge: where codex keeps its rollouts, that
//! pi's only live turn signal is a resident socket round-trip, that OpenCode is
//! one sqlite table and not a transcript tree at all. That half belongs to qw,
//! and it is what lives here.
//!
//! The seam is the DATA the decider consumes, not a step in its control flow:
//! [`gather_providers`] returns [`ProviderGather`], a flat carrier that qd
//! splats into its `JoinInputs`. So the boundary crossing is one call and one
//! struct, the pure join keeps its existing field-by-field shape, and no
//! provider name appears in qd's half.
//!
//! # What deliberately did NOT come along
//!
//! - **`DiscoveryHealth`** is not this module's to record. It says which of the
//!   gather's reads FAILED as opposed to finding nothing, and only the
//!   mux/`ps`/registry reads write into it — so this module returns no health,
//!   and no provider gather touches it. (It used to live in qd for that reason.
//!   The `join.rs` split moved the mux/`ps`/registry reads themselves into
//!   [`crate::gather`], which made "qd's own" false, so the type settled in
//!   `quorum_core::discovery` — the leaf both sides already depend on: qw's
//!   gather fills it, qd's `render` and `send` refusal read it.)
//! - **`JoinOpts`** is still not this module's business: it is the CLI's option
//!   surface (`--all`, `--tombstoned`, `-n`), and this gather reads exactly ONE
//!   flag out of it. That flag arrives as a plain `include_preview: bool` —
//!   taking the struct would leak qd's command line down here for a single
//!   boolean. (The struct itself now lives in [`crate::gather`], because it is
//!   in the SIGNATURE of the entry point above this one; it is re-exported from
//!   `dispatch::join` and remains qd's surface in every way but declaration.)
//!
//! # Permissive throughout (L8)
//!
//! Every provider step degrades a missing/unreadable store to EMPTY, never to an
//! error: no codex root, no pi resident, no `opencode.db` are all the ordinary
//! case on most hosts. An `ls` never fails because a provider a user does not
//! run is not installed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::effects::Env;
use crate::jsonl::{self, JsonlStats, TranscriptMeta};
use crate::model::SessionStatus;
use crate::paths::QdPaths;
use crate::registry::{self, ScannedEntry, TombstonedEntry};

/// A discovered COLD codex row (codex-p2-spec section 7.4) — a foreign or dead
/// codex thread found under the codex root that has NO matching live registry
/// row. Built permissively by the gather step (sqlite index primary, rollout
/// scan fallback); the join emits it as a `Cold` `LiveRegistry`-shaped row (the
/// codex live-row branch, with no pid/zmx/endpoint). Kept as a small data
/// carrier so the pure join stays I/O-free.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CodexColdRow {
    /// The thread uuid (= sessionId).
    pub id: String,
    /// The thread title (sqlite) when present, else None (the join names it).
    pub name: Option<String>,
    pub cwd: Option<String>,
    /// The resolved rollout path, when materialized (None for a thread with no
    /// rollout file yet).
    pub jsonl_path: Option<String>,
    /// Last-active epoch ms (sqlite `updated_at` seconds → ms, or the rollout
    /// mtime), for the lastActive sort. None → 0 in the sort.
    pub last_active_ms: Option<i64>,
}

/// A discovered COLD OpenCode row (lsview A3) — an OpenCode session read from the
/// monolithic `opencode.db` `session` table (via `provider::opencode`). The
/// ColdJsonl analog for the OpenCode provider, the way [`CodexColdRow`] is for
/// codex: a small pre-derived carrier so the pure join stays I/O-free. Unlike the
/// pi cold path (per-session transcript files whose stats ride the shared
/// `stats_for` cache), OpenCode's stats are pre-aggregated COLUMNS read in ONE
/// query — so they arrive already-derived on this struct and NEVER route through
/// the A1 transcript-read cache/counter (an OpenCode SQL read is not a transcript
/// read). Built by [`gather_opencode`]; emitted as a `Cold` row carrying provider
/// `opencode`. EMPTY when no OpenCode store exists → byte-stable no-op.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OpencodeColdRow {
    /// `session.id` (`ses_…`) — the session id / dedup key.
    pub id: String,
    /// `session.title` (fallback `session.slug`), nonempty → None otherwise.
    pub name: Option<String>,
    /// `session.directory` — the absolute cwd, nonempty → None otherwise.
    pub cwd: Option<String>,
    /// turns = `count(*) FROM message WHERE session_id = id`.
    pub turns: u64,
    /// Cumulative input-side tokens (`tokens_input + tokens_cache_read +
    /// tokens_cache_write`). OpenCode has NO on-disk live context-window
    /// occupancy gauge (R1 §9), so this is the cumulative input-side total — the
    /// closest available analog of claude's occupancy formula.
    pub tokens: u64,
    /// `session.time_updated` (epoch ms) — the lastActive signal (the only
    /// timestamp a cold row renders).
    pub last_active_ms: Option<i64>,
}

/// Everything [`gather_providers`] read, in the flat shape `qd`'s `JoinInputs`
/// already expects — one field per pre-gathered channel, splatted straight in.
///
/// Deliberately NOT a nested per-provider tree: the pure join reads these as
/// independent maps (a claude row consults `stats_for`, a codex row consults
/// `codex_status_for`), and re-shaping them into `by_provider.codex.…` would
/// force the decider to learn provider names — the exact coupling this split
/// exists to remove.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProviderGather {
    /// claude's `scanAllJsonlFiles` result (mtime ms) — the ColdJsonl channel.
    pub claude_transcripts: Vec<TranscriptMeta>,
    /// `getJsonlStats(path)` for every consulted transcript path, keyed by path.
    /// SHARED across providers: claude/pi transcripts and codex rollouts all land
    /// here, keyed by path (per-provider roots are disjoint path spaces).
    pub stats_for: HashMap<PathBuf, JsonlStats>,
    /// The resolved transcript path per sessionId (live + tombstone rows look up
    /// by id), each resolved off ITS provider's root.
    pub jsonl_path_for: HashMap<String, PathBuf>,
    /// Connectionless rollout-tail status per LIVE codex row, keyed by sessionId.
    pub codex_status_for: HashMap<String, SessionStatus>,
    /// Foreign/dead codex threads under the codex root (live rows already joined
    /// out, live-wins).
    pub codex_cold: Vec<CodexColdRow>,
    /// Resident-derived status per LIVE pi row, keyed by sessionId.
    pub pi_status_for: HashMap<String, SessionStatus>,
    /// The resident's drop-immune completed-turn count per live pi row.
    pub pi_turns_for: HashMap<String, u64>,
    /// COLD pi rows scanned off the pi sessions root; their stats ride
    /// `stats_for`.
    pub pi_cold: Vec<TranscriptMeta>,
    /// COLD OpenCode rows read from `opencode.db`; their stats arrive
    /// PRE-DERIVED on the row, never through `stats_for`.
    pub opencode_cold: Vec<OpencodeColdRow>,
}

/// Do every provider-side read the pure join depends on, against injected roots
/// and the `Env` seam.
///
/// `registry` is `&mut` for ONE reason: the codex step also BINDS identity. An
/// interactive codex row is created without a sessionId (codex discloses none
/// until the user types) and gets one here — in memory AND on disk — so the same
/// `ls` that discovers the thread also renders it identified. Everything else
/// only reads.
///
/// `include_preview` is the sole flag the reads need: it selects how much of a
/// cached transcript the stats cache SERVES. The cache always STORES full (A1's
/// store-full/serve-subset design), so this never changes what gets read.
pub(crate) fn gather_providers(
    paths: &QdPaths,
    env: &dyn Env,
    registry: &mut [ScannedEntry],
    tombstoned: &[TombstonedEntry],
    include_preview: bool,
) -> ProviderGather {
    // --- transcripts scan + per-path stats: the PER-PROVIDER SCAN UNION (P2). ---
    // lsview A2: the P2 note below anticipated this — a multi-provider gather that
    // UNIONS per-provider scans, each provider's own store under its own root.
    // Every provider whose cold store is a `scan_transcripts` surface contributes
    // its scan through the SAME shape; the shared stats-acquisition loop (further
    // down, after the cache loads) reads every scanned transcript through the ONE
    // A1 cache with that provider's reader.
    //
    //   - claude: the n=1 case (codex P1 W7) — its store IS the projects dir
    //     (`jsonl.rs` is claude's transcript surface), so the root is
    //     `paths.projects_dir` and the scan is byte-identical to the pre-union
    //     single scan. It still feeds the existing ColdJsonl channel unchanged.
    //   - pi: its own sessions root (`$PI_CODING_AGENT_SESSION_DIR` else
    //     `$HOME/.pi/agent/sessions`, resolved off `fx.env` ONLY). An absent or
    //     empty store yields ZERO rows (`scan_transcripts` returns empty on a
    //     missing root), never an error — the devbox-today live case.
    //   - codex: its cold store is a sqlite index + rollout tree (NOT a
    //     `scan_transcripts` surface), so its cold discovery stays in
    //     `gather_codex` (result-identical, per the A2 discretion).
    let claude = crate::provider::provider_for("claude-code")
        .expect("claude-code provider is always registered");
    let pi_provider =
        crate::provider::provider_for("pi").expect("pi provider is always registered");
    // A MINIMAL fx for provider root resolution off `fx.env` (pi reads HOME /
    // PI_CODING_AGENT_SESSION_DIR; claude reads `fx.paths.projects_dir`), the same
    // minimal-fx shape `gather_codex` builds for codex. `paths` is the REAL paths
    // so `ClaudeProvider::transcript_root` keeps answering `projects_dir`.
    let root_fx_paths = paths.clone();
    let root_fx = root_fx(env, &root_fx_paths);
    // claude's scan → the existing ColdJsonl channel (byte-identical). pi's scan →
    // the additive `pi_cold` channel (join emits those as `provider: "pi"` rows).
    // Both go through the per-lane scan functions so `LaneOps::list` and this
    // gather share ONE definition of "what is this harness's cold store" — the
    // delegation rule, applied to the read paths.
    let transcripts = claude_cold_scan(paths);
    let pi_cold = pi_cold_scan(paths, env);

    // lsview A1: the persistent per-transcript stats cache. ONE cache, shared by
    // EVERY provider's stats acquisition below (claude here, codex in
    // `gather_codex`) — the cache is provider-agnostic; each site injects its own
    // reader. Built from the state dir (the `marks.jsonl` sibling), consulted at
    // every `read_stats` site, persisted once after the gather. A warm `ls` over
    // an unchanged transcript fleet re-reads NO transcript content. The reader is
    // ALWAYS invoked with `include_preview = true` (store full, serve subsets), so
    // a preview request is always servable from a warm hit.
    let mut stats_cache = crate::stats_cache::StatsCache::load(&paths.state_dir);

    let mut stats_for: HashMap<PathBuf, JsonlStats> = HashMap::new();
    let mut jsonl_path_for: HashMap<String, PathBuf> = HashMap::new();

    // Live entries: findJsonlPath(sessionId, cwd) + getJsonlStats.
    // codex P1 W7 (codex-p1-spec section 7.2): the per-row `find_jsonl_path`
    // dispatches by THIS row's provider value. claude rows → `transcript_path`
    // (delegates to `jsonl::find_jsonl_path`, byte-identical). An UNKNOWN-provider
    // row degrades to the claude derivation for render-survival (same L8 posture
    // as W6: `ls` must show the row; ACTING verbs already refuse). The SessionKey
    // is built from the row (id = sessionId, cwd from the row).
    //
    // pi-interactive: the ROOT is now resolved through the provider too
    // (`transcript_root`), not hard-coded to the claude `projects_dir`. This loop
    // was written when claude was the only provider with rows in it, so the claude
    // root was the only root there was; the provider-routed `transcript_path`
    // arrived above it and inherited the hard-coded argument. claude is unmoved
    // (`ClaudeProvider::transcript_root` IS `paths.projects_dir`), but a pi row was
    // being asked to find its session file under claude's tree, where it can never
    // be — so `jsonlPath` was silently always absent for pi, and "no transcript
    // resolved" is indistinguishable from pi's legitimate pre-first-reply state.
    for scanned in registry.iter() {
        if let Some(sid) = &scanned.entry.session_id {
            let provider_id = scanned.entry.provider.as_deref().unwrap_or("claude-code");
            // codex is deliberately EXCLUDED: `gather_codex` already resolves each
            // codex row's rollout off the codex root and inserts it below, deriving
            // the row's status from the SAME read. Resolving it here as well would
            // pay for the sqlite lookup / date-walk twice on every `ls`.
            if provider_id == "codex" {
                continue;
            }
            let prov = crate::provider::provider_for(provider_id)
                .unwrap_or_else(|| crate::provider::provider_for("claude-code").unwrap());
            let key = crate::provider::SessionKey {
                id: sid,
                name: scanned.entry.name.as_deref(),
                cwd: scanned.entry.cwd.as_deref(),
                pid: scanned.entry.pid,
            };
            if let Some(p) = prov.transcript_path(&prov.transcript_root(&root_fx), &key) {
                stats_for.entry(p.clone()).or_insert_with(|| {
                    stats_cache.get_or_read(&p, include_preview, |pp| jsonl::read_stats(pp, true))
                });
                jsonl_path_for.insert(sid.clone(), p);
            }
        }
    }
    // Tombstoned entries (only matters when include_tombstoned).
    // codex P1 W7 (codex-p1-spec section 7.2): same per-row provider dispatch as
    // the live loop above — claude rows route through `transcript_path`, unknown
    // degrades to claude derivation for render-survival.
    for t in tombstoned {
        if let Some(sid) = &t.data.session_id {
            if !jsonl_path_for.contains_key(sid) {
                let provider_id = t.data.provider.as_deref().unwrap_or("claude-code");
                if provider_id == "codex" {
                    continue; // see the live loop: gather_codex owns codex paths.
                }
                let prov = crate::provider::provider_for(provider_id)
                    .unwrap_or_else(|| crate::provider::provider_for("claude-code").unwrap());
                let key = crate::provider::SessionKey {
                    id: sid,
                    name: t.data.name.as_deref(),
                    cwd: t.data.cwd.as_deref(),
                    pid: t.data.pid,
                };
                if let Some(p) = prov.transcript_path(&prov.transcript_root(&root_fx), &key) {
                    stats_for.entry(p.clone()).or_insert_with(|| {
                        stats_cache
                            .get_or_read(&p, include_preview, |pp| jsonl::read_stats(pp, true))
                    });
                    jsonl_path_for.insert(sid.clone(), p);
                }
            }
        }
    }
    // Cold transcripts (the PER-PROVIDER SCAN UNION): getJsonlStats per scanned
    // file (the join reads stats_for by transcript path). Pre-gather every scanned
    // transcript's stats through the ONE cache, each with ITS provider's reader
    // (claude: `jsonl::read_stats`; pi: same today, via `PiProvider`). The reader
    // ALWAYS reads full (`true`); the cache serves the `include_preview` subset
    // (A1's store-full/serve-subset design — byte-identical to the pre-union
    // claude call). First-writer-wins (`or_insert_with`) preserves the live/
    // tombstone pre-gather above; per-provider roots are disjoint path spaces so
    // no cross-provider key collides. An empty `pi_cold` (empty/absent pi store)
    // adds nothing → byte-stable.
    for (provider, metas) in [(claude, &transcripts), (pi_provider, &pi_cold)] {
        for t in metas {
            stats_for.entry(t.path.clone()).or_insert_with(|| {
                stats_cache.get_or_read(&t.path, include_preview, |pp| {
                    provider.transcript_stats(pp, true)
                })
            });
        }
    }

    // --- codex gather step (codex-p2-spec section 7.4) — ADDITIVE. Resolves the
    //     CONNECTIONLESS status of live codex rows (rollout-tail derivation) +
    //     discovers cold codex rows. A cheap no-op when no codex root exists (most
    //     hosts) or there are no codex rows + no on-disk threads. The claude branches
    //     above are byte-untouched: this reads the codex root only. ---
    let (codex_status_for, codex_jsonl_for, codex_cold, codex_stats_for) =
        gather_codex(env, registry, &paths.sessions_dir, &mut stats_cache);
    // The codex rollout path is resolved off the codex root (NOT paths.projects_dir);
    // surface it into jsonl_path_for so the live codex row carries jsonlPath = the
    // rollout path (the per-row gather loop above resolved claude rows under
    // projects_dir, which is the WRONG root for codex — the transcript_root switch
    // is confined to the codex step, claude call sites unchanged; see W5 report).
    for (sid, path) in codex_jsonl_for {
        jsonl_path_for.insert(sid, path);
    }
    // Merge the live codex rollout stats (occupancy, Pete #5) keyed by rollout
    // path; the claude pre-gather never reaches the codex root, so these paths are
    // new keys (no claude/codex collision — different roots).
    for (path, stats) in codex_stats_for {
        stats_for.entry(path).or_insert(stats);
    }

    // lsview A1: every stats-acquisition site above (claude + codex) has now run
    // through `stats_cache`. Persist the snapshot atomically (tmp + rename;
    // best-effort — a write failure never fails the `ls`). An all-hit run is not
    // dirty and skips the write entirely. `QD_CACHE_STATS=1` emits a one-line
    // hit/miss/rebuild summary to stderr (a cheap real-fleet observability hook).
    let _ = stats_cache.persist();
    if env
        .var("QD_CACHE_STATS")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    {
        eprintln!("{}", stats_cache.debug_line());
    }

    // --- pi gather step (B1) — ADDITIVE, ONLY touches pi rows. Resolves each live
    //     pi row's turn state from its resident via an `is_streaming` point-read
    //     (the drop-immune status source). A cheap no-op when there are no pi rows.
    //     The claude/codex branches are byte-untouched. ---
    let (pi_status_for, pi_turns_for) = gather_pi(registry);

    // --- OpenCode gather step (lsview A3) — ADDITIVE, reads the OpenCode store
    //     ONLY. ONE read-only query over `opencode.db`; a cheap no-op when no
    //     OpenCode store exists. The claude/codex/pi branches are byte-untouched,
    //     and this never touches the A1 transcript-read cache/counter. ---
    let opencode_cold = gather_opencode(env);
    ProviderGather {
        claude_transcripts: transcripts,
        stats_for,
        jsonl_path_for,
        codex_status_for,
        codex_cold,
        pi_status_for,
        pi_turns_for,
        pi_cold,
        opencode_cold,
    }
}

/// codex-interactive: bind a thread id into every interactive codex row still
/// waiting for one (a no-op in the overwhelmingly common case).
///
/// A row qualifies only if it is codex, MUX-PANE hosted, has no sessionId yet, is
/// ALIVE, and knows its cwd and start time. The liveness gate is not an
/// optimization — it is a correctness guard. A dead unidentified row (a session
/// stopped before anyone ever typed into it) has no thread and never will, but its
/// cwd and start time would go on matching forever; without this gate it would sit
/// there ready to adopt the id of some unrelated codex the user starts in that
/// directory next week.
///
/// Binding writes the row back to disk AND updates it in memory, so the `ls` that
/// discovers the thread also renders it. `owned` accumulates as we go, so two rows
/// binding in the same pass cannot claim the same thread.
fn backfill_codex_thread_ids(
    registry: &mut [ScannedEntry],
    sessions_dir: &Path,
    sessions_root: &Path,
) {
    use crate::provider::{codex::tui, Hosting};

    let wants_id = |e: &registry::RegistryEntry| -> bool {
        e.provider.as_deref() == Some("codex")
            && e.session_id.as_deref().is_none_or(str::is_empty)
            && crate::provider::row_hosting("codex", e.hosting.as_deref()) == Some(Hosting::MuxPane)
            && e.pid
                .is_some_and(|p| p != 0 && crate::effects::is_pid_alive(p as i32))
            && e.cwd.is_some()
            && e.started_at.is_some()
    };

    // The hot-path exit: nothing to bind ⇒ no extra scan, no disk touch.
    if !registry.iter().any(|s| wants_id(&s.entry)) {
        return;
    }

    // Every id ANY row already claims — live AND tombstoned. Tombstones count: a
    // stopped session still owns its thread's history, and letting a live row
    // adopt it would silently graft one conversation onto another. This re-scan is
    // paid only in the bind window, never on an ordinary `ls`.
    let mut owned: HashSet<String> = registry::read_entries(sessions_dir, true)
        .into_iter()
        .filter_map(|s| s.entry.session_id)
        .filter(|s| !s.is_empty())
        .collect();

    for scanned in registry.iter_mut() {
        if !wants_id(&scanned.entry) {
            continue;
        }
        let (Some(cwd), Some(started_at)) = (scanned.entry.cwd.clone(), scanned.entry.started_at)
        else {
            continue;
        };
        let Some(id) = tui::backfill_thread_id(sessions_root, &cwd, started_at, &owned) else {
            // Not yet, or ambiguous. Either way the row stays honestly
            // unidentified and the next `ls` tries again.
            continue;
        };
        scanned.entry.session_id = Some(id.clone());
        // Best-effort persist (L8): a write failure leaves the row unidentified on
        // disk and we simply rediscover next time — never fatal to an `ls`.
        let _ = registry::write_entry(sessions_dir, &scanned.entry);
        owned.insert(id);
    }
}

/// The pi gather step (B1) — the pi analog of [`gather_codex`], kept in ONE place
/// so the claude/codex gather stays untouched. For each LIVE pi registry row it
/// opens ONE short-lived resident front connection and does a single
/// [`crate::provider::pi::PiRemote::observe`] round-trip → `(is_streaming, turns)`.
///
/// Returns `(status_by_id, turns_by_id)`:
///   - `status_by_id`: `Busy` while `is_streaming`, else `Idle`, per live pi row.
///     Absent ⇒ the join falls back to Idle (the codex absent-row posture).
///   - `turns_by_id`: the resident's drop-immune busy→idle-edge count per row.
///
/// WHY A SOCKET (unlike codex's connectionless rollout read): pi has NO on-disk
/// turn-state file — OPTION B (P4DB) burned the status sink and mandated on-read
/// derivation, and pi's lazy-written session JSONL does not encode live turn
/// state. The resident `get_state` is the ONLY faithful live signal, and it is the
/// SAME drop-immune point-read `qd wait` gates on. PERMISSIVE / fail-fast (L8): a
/// dead / unreachable / mis-identified resident contributes NOTHING (a 500ms
/// connect timeout + the identity gate) — never a hang, never an error, the row
/// just renders Idle with its transcript turns. EMPTY when there are no pi rows.
///
/// LIMITATION (documented, out of B1 scope to change): the resident serves ONE
/// front connection at a time (`PiStdio` is `!Sync`), so while a `qd wait` is
/// camped on a pi session this gather's connect for THAT row times out and the row
/// renders Idle in `ls` until the wait releases. Every other pi row, and the
/// common no-active-wait case, reads live status fine.
fn gather_pi(registry: &[ScannedEntry]) -> (HashMap<String, SessionStatus>, HashMap<String, u64>) {
    use crate::provider::pi::{residence::cmdline_is_our_pi_daemon, PiRemote};
    use std::time::Duration;

    let mut status_for: HashMap<String, SessionStatus> = HashMap::new();
    let mut turns_for: HashMap<String, u64> = HashMap::new();

    for scanned in registry {
        if scanned.entry.provider.as_deref() != Some("pi") {
            continue;
        }
        let Some(sid) = scanned.entry.session_id.clone() else {
            continue;
        };
        let endpoint = scanned.entry.endpoint.clone().filter(|s| !s.is_empty());
        let Some(pid) = scanned.entry.pid.filter(|&p| p != 0) else {
            continue;
        };
        // Identity + liveness gate (the wait/send posture): defeats pid reuse — a
        // connect-success alone is not identity. A miss → skip (the row renders Idle).
        let cmdline = crate::create_daemon::real_cmdline_probe(pid);
        let alive = endpoint.is_some()
            && crate::effects::is_pid_alive(pid as i32)
            && cmdline_is_our_pi_daemon(cmdline.as_deref(), endpoint.as_deref());
        if !alive {
            continue;
        }
        let endpoint = endpoint.expect("alive implies a live endpoint");

        // Fail-fast connect (500ms) — a live-but-camped or dead resident times out
        // and the row degrades to Idle rather than stalling `ls`.
        if let Ok(remote) = PiRemote::connect(&endpoint, Duration::from_millis(500)) {
            if let Ok(obs) = remote.observe() {
                status_for.insert(
                    sid.clone(),
                    if obs.is_streaming {
                        SessionStatus::Busy
                    } else {
                        SessionStatus::Idle
                    },
                );
                turns_for.insert(sid, obs.turns);
            }
        }
    }

    (status_for, turns_for)
}

/// The codex gather step (codex-p2-spec sections 3.3, 7.4) — ALL the codex
/// connectionless I/O, kept in ONE place so the claude gather above stays
/// byte-identical and the pure join stays I/O-free.
///
/// Returns `(status_by_id, rollout_path_by_id, cold_rows)`:
///   - `status_by_id`: the rollout-tail-derived status for each LIVE codex
///     registry row (sessionId = thread uuid). Absent ⇒ the join falls back to
///     Idle (a fresh thread with no rollout/anchors is idle). NO SOCKET opened.
///   - `rollout_path_by_id`: the resolved rollout path per live codex row (the
///     `jsonlPath` field), resolved off the CODEX root (the transcript_root
///     switch, confined here — claude call sites keep `paths.projects_dir`).
///   - `cold_rows`: foreign/dead codex threads under the codex root, JOINED
///     against live rows by id (live wins), de-duplicated.
///
/// PERMISSIVE (L8 / codex-p2-spec section 3.4): a missing codex root, an
/// unreadable sqlite db, or a garbage rollout contributes NOTHING and never
/// errors. When the codex provider cannot resolve a root (no HOME/CODEX_HOME) or
/// the root does not exist, every map/vec is empty (the no-op host case).
// The 4-tuple is local plumbing back to the single caller (status / jsonl-path /
// cold-rows / live-stats maps); a named struct would not earn its keep for one
// in-module call site (same posture as the `#[allow(too_many_arguments)]` gather
// helpers above).
#[allow(clippy::type_complexity)]
fn gather_codex(
    env: &dyn Env,
    // codex-interactive: MUTABLE because this step also BINDS identity — an
    // interactive codex row is created without a sessionId (codex discloses none
    // until the user types) and gets one here, in memory AND on disk, so the same
    // `ls` that discovers it also renders it identified.
    registry: &mut [ScannedEntry],
    // Where to persist a freshly-bound row. Only written when a bind actually
    // happens — once per session lifetime, never on an ordinary `ls`.
    sessions_dir: &Path,
    // lsview A1: the SHARED stats cache (provider-agnostic). The codex rollout
    // reads below route through it exactly like the claude sites — the cache holds
    // no codex-specific code; this site just injects the codex reader.
    stats_cache: &mut crate::stats_cache::StatsCache,
) -> (
    HashMap<String, SessionStatus>,
    HashMap<String, PathBuf>,
    Vec<CodexColdRow>,
    // Per-rollout-path stats for LIVE codex rows, merged into the join's
    // `stats_for` so a live codex row's token count = its rollout occupancy
    // (Pete #5). Keyed by the rollout path (the same key the live-row loop reads
    // via `jsonl_path_for`). The CLAUDE `stats_for` pre-gather never sees the
    // codex root, so without this a live codex row defaults to tokens 0.
    HashMap<PathBuf, JsonlStats>,
) {
    use crate::provider::codex::{self, CodexProvider};
    use crate::provider::{Provider, ProviderFx, SessionKey};

    let mut status_for: HashMap<String, SessionStatus> = HashMap::new();
    let mut jsonl_for: HashMap<String, PathBuf> = HashMap::new();
    // Live codex rollout stats (occupancy etc.), keyed by rollout path.
    let mut stats_for: HashMap<PathBuf, JsonlStats> = HashMap::new();

    // The codex provider resolves its OWN root off `fx.env` ONLY (L9a). A MINIMAL
    // fx: env + a placeholder QdPaths (codex's transcript_root reads ONLY env; the
    // paths member is unused by it). Built once and reused per row.
    let placeholder = QdPaths::from_home(Path::new("/nonexistent-codex-fx-home"));
    let fx = ProviderFx {
        await_relay: None,
        env,
        paths: &placeholder,
        socket_dir: PathBuf::new(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,
        acp_pre_dispatch: None,
    };
    let provider = CodexProvider;
    // `$CODEX_HOME/sessions` (the rollout tree root). Its PARENT is `$CODEX_HOME`,
    // which holds `state_5.sqlite`.
    let sessions_root = provider.transcript_root(&fx);

    // --- codex-interactive: BIND the thread id of any interactive codex row that
    //     does not have one yet, BEFORE the live-row loop reads ids below. ---
    //
    // An `--interactive` codex row is born without a sessionId: codex opens its
    // rollout at the user's FIRST INTERACTION, not at launch (see
    // `provider::codex::tui`), so at create time there is nothing to record. This
    // is where that debt is paid — the same pass that already reads codex rollouts
    // for status notices the new rollout and writes its id into the row.
    //
    // COST DISCIPLINE (this is the hot `qd ls` path): the whole block is behind an
    // `any()` over rows already in hand. With no unidentified interactive row —
    // i.e. always, except in the short window between starting such a session and
    // typing into it — this costs one predicate per codex row and touches no disk.
    backfill_codex_thread_ids(registry, sessions_dir, &sessions_root);

    // The set of live codex thread ids (so cold discovery joins live-wins).
    let mut live_codex_ids: HashSet<String> = HashSet::new();

    // --- LIVE codex rows: rollout-tail status + rollout path (connectionless). ---
    for scanned in registry {
        if scanned.entry.provider.as_deref() != Some("codex") {
            continue;
        }
        let Some(sid) = scanned.entry.session_id.clone() else {
            continue;
        };
        live_codex_ids.insert(sid.clone());
        let key = SessionKey {
            id: &sid,
            name: scanned.entry.name.as_deref(),
            cwd: scanned.entry.cwd.as_deref(),
            pid: scanned.entry.pid,
        };
        // Resolve the rollout file off the CODEX root (sqlite rollout_path tier 1,
        // else date-walk tier 2). None ⇒ a fresh thread with no rollout yet (lazy
        // rollout — W4 fact): no jsonlPath, Idle status (the absent-from-map case).
        if let Some(path) = provider.transcript_path(&sessions_root, &key) {
            // lsview A1 (F1): the rollout-tail STATUS and the occupancy STATS from
            // ONE content read, BOTH served through the shared cache. Pre-fix, the
            // status read (`derive_status` over `read_lines`) ran here
            // UNCONDITIONALLY on every `ls` — uncached and invisible to the counter
            // seam — so a warm `ls` still re-read every live codex rollout in full.
            // Now the status-aware seam memoizes the derived status ALONGSIDE the
            // stats in the same `(path, mtime, size)` entry: a warm hit re-reads
            // NOTHING (status AND stats) and the counter observes the read. The
            // injected reader does a SINGLE `read_lines` pass, deriving both stats
            // and the connectionless status (open turn ⇒ Busy, balanced ⇒ Idle, no
            // anchors/unreadable ⇒ None → the join falls back to Idle). No socket.
            //
            // `include_preview=false` is preserved (codex live previews are not a
            // rendered surface here); the reader still reads FULL so the store is
            // preview-complete — the served stats are byte-identical to
            // `read_stats(path, false)`, and turns still derive from the rollout's
            // task_complete anchors (Pete #5 occupancy from the token_count tail).
            let (stats, status) = stats_cache.get_or_read_with_status(&path, false, |pp| {
                let lines = codex::rollout::read_lines(pp);
                let stats = codex::rollout::read_stats_from_lines(&lines, true);
                let status = codex::derive_status(&lines);
                (stats, status)
            });
            if let Some(status) = status {
                status_for.insert(sid.clone(), status);
            }
            stats_for.entry(path.clone()).or_insert(stats);
            jsonl_for.insert(sid, path);
        }
    }

    // --- COLD codex discovery (codex-p2-spec section 7.4) — sqlite threads index
    //     PRIMARY, rollout-dir scan FALLBACK/ENRICHMENT, joined live-wins. ---
    let codex_home = sessions_root.parent().map(Path::to_path_buf);
    let mut cold_by_id: HashMap<String, CodexColdRow> = HashMap::new();

    // (1) sqlite primary: title/cwd/rollout_path/updated_at per thread.
    if let Some(home) = &codex_home {
        for row in codex::index::threads(home) {
            if live_codex_ids.contains(&row.id) {
                continue; // live wins.
            }
            let rollout = PathBuf::from(&row.rollout_path);
            cold_by_id.insert(
                row.id.clone(),
                CodexColdRow {
                    id: row.id,
                    name: nonempty(Some(row.title)),
                    cwd: nonempty(Some(row.cwd)),
                    jsonl_path: rollout.exists().then(|| path_to_string(rollout)),
                    // sqlite updated_at is epoch SECONDS → ms.
                    last_active_ms: Some(row.updated_at * 1000),
                },
            );
        }
    }

    // (2) rollout-scan fallback/enrichment: any rollout under the tree NOT already
    //     covered by sqlite + NOT live. cwd/last_active enriched from the rollout
    //     itself (session_meta cwd; file mtime). A thread already in the sqlite map
    //     keeps the richer sqlite row (sqlite primary).
    for meta in provider.scan_transcripts(&sessions_root) {
        if live_codex_ids.contains(&meta.session_id) || cold_by_id.contains_key(&meta.session_id) {
            continue;
        }
        // Permissive enrichment: read the rollout for a cwd (session_meta). A
        // garbage/gzip rollout yields nothing → the row still surfaces (id-only).
        let stats =
            stats_cache.get_or_read(&meta.path, false, |pp| codex::rollout::read_stats(pp, true));
        cold_by_id.insert(
            meta.session_id.clone(),
            CodexColdRow {
                id: meta.session_id,
                // No sqlite title → the join names it (house default for a
                // nameless row).
                name: None,
                cwd: nonempty(stats.cwd),
                jsonl_path: Some(path_to_string(meta.path)),
                last_active_ms: Some(meta.mtime_ms),
            },
        );
    }

    // Deterministic order (a HashMap iterates nondeterministically): sort by
    // lastActive desc then id, so the gather output — and the eventual golden — is
    // stable. The join's final sort is by lastActive only (stable), so a secondary
    // id tiebreak here keeps equal-lastActive cold rows in a fixed order.
    let mut cold: Vec<CodexColdRow> = cold_by_id.into_values().collect();
    cold.sort_by(|a, b| {
        b.last_active_ms
            .unwrap_or(0)
            .cmp(&a.last_active_ms.unwrap_or(0))
            .then_with(|| a.id.cmp(&b.id))
    });

    (status_for, jsonl_for, cold, stats_for)
}

/// The OpenCode gather step (lsview A3) — the OpenCode analog of [`gather_codex`],
/// kept in ONE place so the claude/codex/pi gather stays untouched. Reads the
/// monolithic `opencode.db` `session` table in ONE read-only query (via
/// `provider::opencode`) and returns the discovered sessions as cold rows.
///
/// PERMISSIVE (L8 / R1 §5, §7): an unresolvable store root, a missing/garbage db,
/// or a malformed row contributes NOTHING and never errors — `opencode::sessions`
/// degrades every failure to empty. A cheap no-op when no OpenCode store exists.
///
/// The stats (turns, tokens) are PRE-AGGREGATED columns read here in the single
/// enumerate query — they do NOT flow through the shared `stats_for` cache and do
/// NOT touch the A1 transcript-read counter (an OpenCode SQL read is not a
/// transcript read; the coordinator's cache-key ruling). Live-wins dedup against
/// live/tombstoned OpenCode rows is handled by the pure join's `seen_session_ids`
/// guard (OpenCode `ses_…` ids share no id-space with claude/codex).
fn gather_opencode(env: &dyn Env) -> Vec<OpencodeColdRow> {
    use crate::provider::opencode;
    let Some(store_dir) = opencode::store_dir(env) else {
        return Vec::new();
    };
    opencode::sessions(&store_dir)
        .into_iter()
        .map(|s| {
            // name = title || slug (R1 §5 fallback). tokens = cumulative input-side
            // (input + cache_read + cache_write), saturating so a malformed negative
            // column never underflows u64. turns/timestamps carried as-is (ms).
            let name = nonempty(Some(s.title)).or_else(|| nonempty(Some(s.slug)));
            let tokens = s
                .tokens_input
                .saturating_add(s.tokens_cache_read)
                .saturating_add(s.tokens_cache_write)
                .max(0) as u64;
            OpencodeColdRow {
                id: s.id,
                name,
                cwd: nonempty(Some(s.directory)),
                turns: s.turns.max(0) as u64,
                tokens,
                last_active_ms: Some(s.time_updated_ms),
            }
        })
        .collect()
}

// ===========================================================================
// The per-lane cold scans
// ===========================================================================
//
// One definition each, shared by [`gather_providers`] (which unions them for the
// `qd` join) and by [`crate::lane_read::list_for`] (which answers for ONE lane).
// They are deliberately the SMALLEST thing that can be called per-lane: a root
// resolution plus the provider's own scan. Nothing decides anything here — the
// synthesis into a row, and the merge against the other sources, both stay with
// their owners.

/// The MINIMAL [`crate::provider::ProviderFx`] a root resolution needs.
///
/// `transcript_root` reads `fx.env` (pi: `PI_CODING_AGENT_SESSION_DIR`/`HOME`;
/// codex: `CODEX_HOME`) and `fx.paths` (claude: `projects_dir`) and nothing else,
/// so every other field is `None`. Spelled once here rather than inline at each
/// site that needs it.
pub(crate) fn root_fx<'a>(env: &'a dyn Env, paths: &'a QdPaths) -> crate::provider::ProviderFx<'a> {
    crate::provider::ProviderFx {
        await_relay: None,
        env,
        paths,
        socket_dir: PathBuf::new(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,
        acp_pre_dispatch: None,
    }
}

/// claude's cold store: every transcript under `paths.projects_dir`.
///
/// The n=1 case of the per-provider scan union — claude's store IS the projects
/// dir, so the root needs no `fx` at all.
pub(crate) fn claude_cold_scan(paths: &QdPaths) -> Vec<TranscriptMeta> {
    let claude = crate::provider::provider_for("claude-code")
        .expect("claude-code provider is always registered");
    claude.scan_transcripts(&paths.projects_dir)
}

/// pi's cold store: every transcript under pi's OWN sessions root
/// (`$PI_CODING_AGENT_SESSION_DIR` else `$HOME/.pi/agent/sessions`), resolved off
/// the injected env only (L9a). An absent/empty root yields ZERO rows, never an
/// error.
pub(crate) fn pi_cold_scan(paths: &QdPaths, env: &dyn Env) -> Vec<TranscriptMeta> {
    let pi = crate::provider::provider_for("pi").expect("pi provider is always registered");
    let fx = root_fx(env, paths);
    pi.scan_transcripts(&pi.transcript_root(&fx))
}

/// codex's cold store, with NO live rows to join against.
///
/// Delegates to [`gather_codex`] — the same function the join calls — with an
/// EMPTY registry slice. That is the whole per-lane difference: with no live
/// codex rows in hand the live-wins filter is inert, so the lane reports every
/// thread under the codex root and qd's merge (rule 1) drops the ones a live
/// registry row already claims. The identity backfill is likewise a no-op on an
/// empty slice, so a lane list NEVER writes to the registry.
pub(crate) fn codex_cold_scan(
    paths: &QdPaths,
    env: &dyn Env,
    stats_cache: &mut crate::stats_cache::StatsCache,
) -> Vec<CodexColdRow> {
    let (_status, _jsonl, cold, _stats) =
        gather_codex(env, &mut [], &paths.sessions_dir, stats_cache);
    cold
}

/// OpenCode's cold store: the one `opencode.db` query. Already per-lane — named
/// alongside the other three so all four read the same way.
pub(crate) fn opencode_cold_scan(env: &dyn Env) -> Vec<OpencodeColdRow> {
    gather_opencode(env)
}

/// pi's LIVE turn state for ONE row: the resident `is_streaming` point-read.
///
/// Delegates to [`gather_pi`] with a one-row slice, so `LaneOps::health` opens
/// the same short-lived front connection, under the same identity+liveness gate
/// and the same 500ms fail-fast budget, that `qd ls` opens today. `None` ⇒ no
/// reachable resident (the gate refused, or the connect/observe failed) — the
/// caller decides the fallback, exactly as the join does.
pub(crate) fn pi_live_status(scanned: &ScannedEntry) -> Option<(SessionStatus, u64)> {
    let one = std::slice::from_ref(scanned);
    let (status_for, turns_for) = gather_pi(one);
    let sid = scanned.entry.session_id.as_deref()?;
    Some((
        *status_for.get(sid)?,
        turns_for.get(sid).copied().unwrap_or(0),
    ))
}

// --- small helpers ---

/// JS `||` truthiness for an `Option<String>`: `Some("")` is falsy → None.
pub(crate) fn nonempty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.is_empty())
}

fn path_to_string(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::RegistryEntry;

    // The two row builders below are DUPLICATED from `join.rs`'s test module
    // rather than moved: the pure-join tests that stayed behind still use them.
    // A shared test fixture would have to become public API of this crate to
    // cross the qd/qw boundary, which is a worse trade than ten lines twice.

    fn live(pid: i64, sid: &str, name: Option<&str>, updated: i64) -> ScannedEntry {
        ScannedEntry {
            entry: RegistryEntry {
                pid: Some(pid),
                session_id: Some(sid.to_string()),
                cwd: Some("/work/proj".to_string()),
                started_at: Some(updated - 1000),
                updated_at: Some(updated),
                status: Some("busy".to_string()),
                name: name.map(str::to_string),
                version: Some("1.0.0".to_string()),
                kind: None,
                entrypoint: None,
                backend: None,
                spawned_by: None,
                provider: None,
                endpoint: None,
                transport: None,
                structured_send_issued: None,
                hosting: None,
                harness_endpoint: None,
            },
            tombstoned: false,
            degraded: Vec::new(),
        }
    }

    /// A pane-hosted codex row that has not bound a thread id yet.
    fn unidentified_codex(pid: i64, name: &str, updated: i64) -> ScannedEntry {
        let mut e = live(pid, "", Some(name), updated);
        e.entry.session_id = None;
        e.entry.provider = Some("codex".to_string());
        e.entry.hosting = Some("mux-pane".to_string());
        e
    }

    // === codex-interactive: the backfill's guards ===

    /// A dead unidentified row must NEVER bind. Its cwd and start time go on
    /// matching forever, so without the liveness gate it would sit there waiting to
    /// adopt the id of an unrelated codex the user starts in that directory later.
    ///
    /// MUTATION EVIDENCE: drop the `is_pid_alive` clause from `wants_id` and this
    /// reds — the dead row binds the thread.
    #[test]
    fn backfill_skips_a_dead_unidentified_row() {
        use tempfile::TempDir;
        let sessions_dir = TempDir::new().unwrap();
        let codex_root = TempDir::new().unwrap();

        // A qualifying rollout is sitting right there, ready to be adopted.
        let uuid = "019fc8bf-e3fa-7420-8152-66a1411442bb";
        let day = codex_root.path().join("2026/08/06");
        std::fs::create_dir_all(&day).unwrap();
        let body = format!(
            concat!(
                "{{\"timestamp\":\"2026-08-06T21:10:00Z\",\"type\":\"session_meta\",",
                "\"payload\":{{\"id\":\"{u}\",\"cwd\":\"/work\",",
                "\"timestamp\":\"2026-08-06T21:09:59Z\"}}}}\n"
            ),
            u = uuid
        );
        std::fs::write(
            day.join(format!("rollout-2026-08-06T00-00-00-{uuid}.jsonl")),
            body,
        )
        .unwrap();

        // pid 0 is never alive (and the guard rejects 0 explicitly).
        let mut row = unidentified_codex(0, "cx-dead", 5_000);
        row.entry.cwd = Some("/work".to_string());
        row.entry.started_at = Some(0);
        let mut registry = vec![row];

        backfill_codex_thread_ids(&mut registry, sessions_dir.path(), codex_root.path());
        assert_eq!(
            registry[0].entry.session_id, None,
            "a dead row must not adopt a live thread"
        );
    }

    /// POSITIVE CONTROL for the test above: the SAME fixture with an ALIVE pid
    /// does bind. Without this, `backfill_skips_a_dead_unidentified_row` could be
    /// passing because the rollout never qualified at all, and the liveness gate it
    /// claims to pin would be untested.
    #[test]
    fn backfill_binds_for_a_live_row_with_the_same_fixture() {
        use tempfile::TempDir;
        let sessions_dir = TempDir::new().unwrap();
        let codex_root = TempDir::new().unwrap();
        let uuid = "019fc8bf-e3fa-7420-8152-66a1411442bb";
        let day = codex_root.path().join("2026/08/06");
        std::fs::create_dir_all(&day).unwrap();
        let body = format!(
            concat!(
                "{{\"timestamp\":\"2026-08-06T21:10:00Z\",\"type\":\"session_meta\",",
                "\"payload\":{{\"id\":\"{u}\",\"cwd\":\"/work\",",
                "\"timestamp\":\"2026-08-06T21:09:59Z\"}}}}\n"
            ),
            u = uuid
        );
        std::fs::write(
            day.join(format!("rollout-2026-08-06T00-00-00-{uuid}.jsonl")),
            body,
        )
        .unwrap();

        // OUR pid — unquestionably alive for the duration of this test.
        let mut row = unidentified_codex(std::process::id() as i64, "cx-live", 5_000);
        row.entry.cwd = Some("/work".to_string());
        row.entry.started_at = Some(0);
        let mut registry = vec![row];

        backfill_codex_thread_ids(&mut registry, sessions_dir.path(), codex_root.path());
        assert_eq!(
            registry[0].entry.session_id.as_deref(),
            Some(uuid),
            "a live unidentified row binds the qualifying thread"
        );
        // And it PERSISTED: the next process reads the bound row, not a fresh scan.
        let on_disk = crate::registry::read_entry(sessions_dir.path(), std::process::id() as i64)
            .expect("the bound row was written back");
        assert_eq!(on_disk.session_id.as_deref(), Some(uuid));
    }

    /// The hot-path guarantee: with nothing to bind the backfill touches no disk.
    /// Nonexistent dirs would surface any read/write attempt; returning cleanly
    /// with the row untouched proves the early exit held.
    #[test]
    fn backfill_is_a_no_op_when_every_row_is_identified() {
        let mut registry = vec![live(100, "real-uuid", Some("cx"), 5_000)];
        registry[0].entry.provider = Some("codex".to_string());
        registry[0].entry.hosting = Some("mux-pane".to_string());
        backfill_codex_thread_ids(
            &mut registry,
            Path::new("/nonexistent-sessions-dir"),
            Path::new("/nonexistent-codex-root"),
        );
        assert_eq!(
            registry[0].entry.session_id.as_deref(),
            Some("real-uuid"),
            "an identified row is left exactly as it was"
        );
    }

    /// A DAEMON-hosted codex row is not a backfill target: it got its id from
    /// `thread/start` at create, and an id-less one is a broken row, not a pending
    /// one. Binding a rollout to it would invent identity for the wrong topology.
    #[test]
    fn backfill_ignores_daemon_hosted_codex_rows() {
        let mut registry = vec![unidentified_codex(100, "cx-daemon", 5_000)];
        registry[0].entry.hosting = Some("daemon".to_string());
        registry[0].entry.cwd = Some("/work".to_string());
        backfill_codex_thread_ids(
            &mut registry,
            Path::new("/nonexistent-sessions-dir"),
            Path::new("/nonexistent-codex-root"),
        );
        assert_eq!(registry[0].entry.session_id, None);
    }
}
