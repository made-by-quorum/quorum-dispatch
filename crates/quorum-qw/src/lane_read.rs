//! The lane READ paths — `LaneOps::list` and `LaneOps::health`, per lane.
//!
//! Stage-2 phase 1 (`doc/tbd/provider-architecture/06-stage2-plan.md`). Kept out
//! of `lanes.rs` because that file is the seven-arm dispatcher over the WRITE
//! paths (start/wake/kill/deliver/attach); the read paths need a different set of
//! delegation targets — the per-provider cold scans and the per-provider status
//! sources — and they are the only two operations that can be cross-checked
//! against the value `qd ls` ships today.
//!
//! # What a lane's `list` answers, and what it does NOT
//!
//! **`list` is the lane's COLD store, not everything with its provider label.**
//! That is the ruled split (`07-lane-gaps.md` §A, `08-session-merge-policy.md`
//! §Relationship to the qd/qw split): of the seven sources `qd ls` merges, four
//! are per-provider on-disk stores and become one `list()` each; the other three
//! — the live registry loop, orphaned mux panes, and tombstones — stay with qd,
//! because none of them is any harness's session. A live registry row is a file
//! **qd wrote about a process qd started**; a tombstone is a gravestone qd
//! carved. Neither is an artifact the harness would recognise.
//!
//! So the four implementations are:
//!
//! | Lane | Store | Delegates to |
//! |---|---|---|
//! | `claude-code/mux-pane` | `~/.claude/projects` transcript tree | [`crate::provider_gather::claude_cold_scan`] |
//! | `codex/daemon` | `$CODEX_HOME` sqlite index + rollout tree | [`crate::provider_gather::codex_cold_scan`] |
//! | `pi/daemon` | `$PI_CODING_AGENT_SESSION_DIR` transcripts | [`crate::provider_gather::pi_cold_scan`] |
//! | `acp/opencode/daemon` | the monolithic `opencode.db` | [`crate::provider_gather::opencode_cold_scan`] |
//!
//! # Why three lanes list nothing, and why that is an answer
//!
//! **A cold store is per-HARNESS, not per-lane.** A transcript on disk records no
//! hosting: nothing in a codex rollout says whether the thread was driven from a
//! mux pane or from a resident. So a harness with two lanes cannot partition its
//! cold store between them, and if both lanes returned it the union would
//! double-count every row.
//!
//! The tie-break is not arbitrary — it is the one the rest of the tree already
//! makes. A cold row is emitted with `hosting: None`, and
//! `provider::row_hosting(provider, None)` falls through to the harness's
//! STRUCTURAL hosting, so every acting verb already treats a cold codex row as
//! `codex/daemon` and a cold pi row as `pi/daemon`. Assigning the store to
//! [`Harness::cold_store_owner_mode`] is therefore a restatement of existing
//! behaviour, not a new policy. `codex/mux-pane` and `pi/mux-pane` list nothing.
//!
//! [`Harness::cold_store_owner_mode`] is deliberately NOT
//! [`Harness::create_default_mode`], and the day those two disagree is the day
//! this distinction earns its keep: the lane `qd start` creates may move, but a
//! transcript that is already on disk cannot follow it. The owner tracks the ROW
//! default, because a cold row is emitted with `hosting: None` and read back
//! through exactly that derivation.
//!
//! `acp/claude-code/daemon` lists nothing for a different and sharper reason: the
//! ACP-CC bridge has **no store of its own**. It writes claude-shaped JSONL into
//! `~/.claude/projects`, which is claude's root, so its cold rows are found by
//! claude's scan wearing claude's label. That is rule 4's one real cross-provider
//! collision, and resolving it is qd's `acp_tombstone_sids` — a CROSS-lane rule
//! no single lane can evaluate. A lane that invented a second scan of claude's
//! root here would produce the duplicate the merge exists to prevent.
//!
//! # What a lane's `health` answers
//!
//! **`health` is the status `qd ls` renders for a LIVE row**, which is why the
//! render-path gates fold in here (see [`health_for`]). An id with no live
//! registry row is [`LaneError::NotFound`]: a cold row's `Cold` is a STRUCTURAL
//! fact (there is no process) rather than an observation, and `list` already
//! carries it. Making `health` re-scan a cold store to re-derive a constant would
//! cost a full tree walk per call and answer nothing new.
//!
//! # The `DiscoveryHealth` gap — the repair, and what it does NOT reach
//!
//! `list` used to return `Result<Vec<SessionSummary>, LaneError>`, which had no
//! channel for "I looked and could not read the store". That cost nothing in
//! practice — `DiscoveryHealth` records only the mux/`ps`/registry reads, all of
//! which are qd's own, and no provider gather has ever written to it (see
//! [`crate::provider_gather`]'s module docs) — but it was lossy for the signal
//! that SHOULD exist: every scan below is L8-permissive, so an unreadable
//! `opencode.db` and an absent one both yield zero rows.
//!
//! Promoting that to `Err(LaneError::Transport)` would have been worse, not
//! better: it discards the rows the lane DID read and turns a partial answer into
//! no answer, which is the "never convert EPERM into not-found" rule inverted. So
//! the repair was a FIELD — [`crate::contract::Listing`], made in stage-2 phase
//! 2's trait revision pass.
//!
//! **What this file can honestly report through it is one thing: the store ROOT
//! was present and could not be opened** ([`root_degradation`]). That is the case
//! the L8 flattening actually hides, and it is reportable without touching any
//! scan. It is deliberately ALL that is wired:
//!
//! - a store root that is absent yields no degradation, because absent IS the
//!   answer for a user who does not run that harness;
//! - a refused `opencode.db` inside a READABLE store dir still reads as absent,
//!   because `opencode::sessions` degrades its own failures to empty and returns
//!   a `Vec` with no error channel;
//! - likewise an unreadable individual transcript, a malformed row, or a failed
//!   sqlite query — `scan_transcripts` and `gather_codex` swallow all three by
//!   design.
//!
//! Widening those is not this pass; the capacity to report them now exists, which
//! is the part that needed a contract change.

use quorum_core::effects::Env;
use quorum_core::paths::QdPaths;

use crate::contract::{
    Degradation, Health, HealthSource, LaneError, Listing, SessionId, SessionStatus, SessionSummary,
};
use crate::jsonl::{JsonlStats, TranscriptMeta};
use crate::lane::{Harness, Lane, Mode};
use crate::model::SessionStatus as ModelStatus;
use crate::provider_gather as gather;
use crate::registry::ScannedEntry;
use crate::stats_cache::StatsCache;

// ===========================================================================
// list
// ===========================================================================

/// Every session this lane's own on-disk store knows about, plus what it could
/// not read.
///
/// Permissive throughout: a store that is absent, empty or unreadable yields zero
/// rows and never an error — an `ls` must not fail because a provider the user
/// does not run is not installed. What changed in the phase-2 trait revision is
/// that the unreadable case no longer reads the same as the absent one: it is
/// reported in [`Listing::degraded`] alongside whatever rows WERE read. See the
/// module docs for the reads that still cannot tell the two apart.
pub fn list_for(lane: Lane, paths: &QdPaths, env: &dyn Env) -> Result<Listing, LaneError> {
    // The stats cache is LOADED but never persisted here. Persistence belongs to
    // the one gather that owns an `ls` pass (`gather_providers` writes it once at
    // the end); a lane list is a reader riding the same warm entries, and two
    // writers racing over one snapshot file would be a regression in a read path.
    let mut cache = StatsCache::load(&paths.state_dir);

    let rows = match (lane.harness, lane.mode) {
        (Harness::ClaudeCode, _) => transcript_rows(
            gather::claude_cold_scan(paths),
            "claude-code",
            ClaudeRoot::ProjectDirFallback,
            &mut cache,
        ),
        (Harness::Pi, Mode::Daemon) => transcript_rows(
            gather::pi_cold_scan(paths, env),
            "pi",
            // pi's `project_dir` slot is the ENCODED bucket dir NAME, not a real
            // path, so it is NOT a cwd fallback (carried verbatim from the join's
            // pi cold block).
            ClaudeRoot::StatsCwdOnly,
            &mut cache,
        ),
        (Harness::Codex, Mode::Daemon) => gather::codex_cold_scan(paths, env, &mut cache)
            .into_iter()
            .map(|c| SessionSummary {
                id: SessionId(c.id),
                provider: "codex".to_string(),
                name: gather::nonempty(c.name),
                cwd: gather::nonempty(c.cwd),
                status: SessionStatus::Cold,
                // The join's codex cold block emits literal zeros: the sqlite
                // index carries no turn/token columns and the rollout is not read
                // for a cold row. Carried verbatim.
                turns: 0,
                tokens: 0,
                last_active_ms: c.last_active_ms,
                git_branch: None,
            })
            .collect(),
        (Harness::Opencode, _) => gather::opencode_cold_scan(env)
            .into_iter()
            .map(|c| SessionSummary {
                id: SessionId(c.id),
                // The DISPLAY label, which is deliberately NOT the lane's provider
                // id: a cold OpenCode row is read from the OpenCode store, not
                // through the ACP bridge, and `qd ls` badges it `opencode`. The
                // lane is `acp/opencode` because that is how the session is
                // DRIVEN; the label says where the row was FOUND.
                provider: "opencode".to_string(),
                name: gather::nonempty(c.name),
                cwd: gather::nonempty(c.cwd),
                status: SessionStatus::Cold,
                // PRE-DERIVED columns from the one enumerate query — these never
                // route through the transcript stats cache.
                turns: c.turns,
                tokens: c.tokens,
                last_active_ms: c.last_active_ms,
                git_branch: None,
            })
            .collect(),

        // The lanes with no cold store of their own. `Ok(vec![])` is an ANSWER
        // here, not a stub — see the module docs. Spelled out rather than caught
        // by a wildcard so that adding a harness forces a decision.
        //
        // A cold store is per-HARNESS, not per-lane: a session file on disk
        // records no hosting, so it cannot say which of its harness's lanes
        // created it. The store therefore belongs to the STRUCTURAL-DEFAULT lane
        // (`Harness::cold_store_owner_mode`, which is where
        // `row_hosting(provider, None)` already sends a hosting-less cold row),
        // and every sibling lane
        // enumerates ZERO. A second claimant would DOUBLE-COUNT every cold row of
        // that harness — which is what `each_cold_store_is_enumerated_by_exactly
        // _one_lane` exists to catch.
        //
        // So `pi/extension` and `pi/mux-pane` list nothing (pi's store is
        // `pi/daemon`'s), and `codex/app-server` and `codex/mux-pane` list nothing
        // (codex's is `codex/daemon`'s). `codex/app-server`'s sessions are LIVE
        // rows, and live rows have never been any lane's to enumerate.
        //
        // claude/app-server and opencode/app-server are absent because the
        // `(Harness::ClaudeCode, _)` and `(Harness::Opencode, _)` arms above
        // already absorb them. Neither lane can be constructed, so which arm
        // swallows the impossible combination decides nothing.
        (Harness::Codex, Mode::Pane | Mode::Extension | Mode::AppServer)
        | (Harness::Pi, Mode::Pane | Mode::Extension | Mode::AppServer)
        | (Harness::AcpClaudeCode, _) => Vec::new(),
    };
    Ok(Listing {
        sessions: rows,
        degraded: store_degradations(lane, paths, env),
    })
}

/// The one degradation this layer can observe: the lane's store ROOT is there and
/// cannot be opened.
///
/// Deliberately a SEPARATE probe rather than a return value threaded back out of
/// the scans. Threading it would mean widening `Provider::scan_transcripts`,
/// `gather_codex` and `opencode::sessions` — three permissive readers whose
/// permissiveness is load-bearing elsewhere — for a signal none of them has a
/// caller for yet. One `read_dir` on a root the scan is about to walk anyway is
/// the cheap honest half, and it catches the case the L8 flattening actually
/// hides (a sandbox or a mode-0 dir), at the cost of one extra syscall per lane
/// list.
///
/// The three lanes with no cold store of their own report nothing, because there
/// is no root to fail to read.
fn store_degradations(lane: Lane, paths: &QdPaths, env: &dyn Env) -> Vec<Degradation> {
    let fx = gather::root_fx(env, paths);
    let root = |id: &str| crate::provider::provider_for(id).map(|p| p.transcript_root(&fx));
    let probed = match (lane.harness, lane.mode) {
        // claude's store IS the projects dir — the same root `claude_cold_scan`
        // hands to `scan_transcripts`, not a re-derivation.
        (Harness::ClaudeCode, _) => Some(paths.projects_dir.clone()),
        (Harness::Pi, Mode::Daemon) => root("pi"),
        // `$CODEX_HOME/sessions` — the rollout tree. Its PARENT holds the sqlite
        // index, which `gather_codex` reads through a reader that reports nothing,
        // so an index refused inside a readable tree stays invisible here.
        (Harness::Codex, Mode::Daemon) => root("codex"),
        (Harness::Opencode, _) => crate::provider::opencode::store_dir(env),
        // No store of its own ⇒ no root ⇒ nothing that can be unreadable. The
        // same set as the `list_for` arm above, for the same per-harness-store
        // reason.
        (Harness::Codex, Mode::Pane | Mode::Extension | Mode::AppServer)
        | (Harness::Pi, Mode::Pane | Mode::Extension | Mode::AppServer)
        | (Harness::AcpClaudeCode, _) => None,
    };
    probed
        .and_then(|path| root_degradation(&path))
        .into_iter()
        .collect()
}

/// `Some` only when the root EXISTS and cannot be read.
///
/// `NotFound` is not a degradation and must never be reported as one: a user who
/// does not run codex has no codex root, and "there are no codex sessions" is the
/// correct answer rather than a failure to look. Every OTHER error — permission
/// denied above all — means the answer is UNKNOWN, and the whole point of
/// [`Degradation`] is that unknown may not be spelled as empty.
///
/// `source` is the resolved PATH rather than a hand-written label, because that
/// is the concrete thing in the user's terms here: the reader's next move is to
/// go look at it, and a jailed or `CODEX_HOME`-relocated root would be
/// unfindable from a prose name.
fn root_degradation(root: &std::path::Path) -> Option<Degradation> {
    match std::fs::read_dir(root) {
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(Degradation {
            source: root.display().to_string(),
            detail: e.to_string(),
        }),
    }
}

/// Whether a transcript-backed cold row may fall back to the scan's `project_dir`
/// for its cwd. claude's is the real project slug; pi's is an encoded bucket name.
enum ClaudeRoot {
    ProjectDirFallback,
    StatsCwdOnly,
}

/// The shared projection for the two transcript-tree stores (claude and pi).
///
/// Carried FIELD BY FIELD from the join's cold blocks, including the derivations
/// that are easy to get subtly wrong: `lastActive` is the parsed
/// `lastTimestamp` when present and the file mtime otherwise, and `cwd` follows
/// JS `||` truthiness (an empty string is falsy).
fn transcript_rows(
    metas: Vec<TranscriptMeta>,
    provider: &str,
    root: ClaudeRoot,
    cache: &mut StatsCache,
) -> Vec<SessionSummary> {
    // mtime-descending. That is the order the join's claude cold block imposes
    // before emitting; the pi block does not sort at all, so its rows come out in
    // readdir order — nondeterministic. Sorting both here costs nothing (qd
    // re-sorts the merged list by lastActive regardless) and buys a lane list
    // that is reproducible run to run, which a caller comparing two lists needs.
    // Stable, so equal mtimes keep scan order.
    let mut metas = metas;
    metas.sort_by_key(|t| std::cmp::Reverse(t.mtime_ms));

    metas
        .into_iter()
        .map(|t| {
            let stats = stats_for(&t.path, provider, cache);
            let last_active_ms = stats
                .last_timestamp
                .as_deref()
                .and_then(crate::jsonl::parse_iso8601_utc_ms)
                .unwrap_or(t.mtime_ms);
            let cwd = match root {
                ClaudeRoot::ProjectDirFallback => {
                    gather::nonempty(stats.cwd.clone()).or(Some(t.project_dir.clone()))
                }
                ClaudeRoot::StatsCwdOnly => gather::nonempty(stats.cwd.clone()),
            };
            SessionSummary {
                id: SessionId(t.session_id),
                provider: provider.to_string(),
                name: stats.name.clone(),
                cwd,
                status: SessionStatus::Cold,
                turns: stats.turns,
                tokens: stats.tokens,
                last_active_ms: Some(last_active_ms),
                git_branch: stats.git_branch.clone(),
            }
        })
        .collect()
}

/// Read one transcript's stats through the SHARED cache with THAT provider's
/// reader — the same store-full/serve-subset call the gather makes.
///
/// `include_preview = false`: [`SessionSummary`] has no preview slot, and the
/// cache always STORES full, so the served subset is byte-identical on every
/// field this projection reads.
fn stats_for(path: &std::path::Path, provider: &str, cache: &mut StatsCache) -> JsonlStats {
    let p = crate::provider::provider_for(provider)
        .expect("list_for only names providers that are always registered");
    cache.get_or_read(path, false, |pp| p.transcript_stats(pp, true))
}

// ===========================================================================
// health
// ===========================================================================

/// The status this lane observes for one LIVE session, and WHERE that answer
/// came from.
///
/// # This is the `qd ls` answer, gates included
///
/// The join derives a live row's status three ways (registry string / codex
/// rollout tail / pi resident), and then `qd ls` applies two RENDER-path
/// corrections on top: the liveness gate (`verbs/ls.rs`, which downgrades a row
/// whose pid is dead or reused to `cold`) and the ACP override
/// (`acp_human_status`, the ONLY production ACP status source there is). Both
/// fold in here, because a caller asking a lane "how is this session" wants the
/// answer the user is shown, not an intermediate the renderer is about to
/// correct.
///
/// # Per-lane sources — BUG 1 is closed by the pi/mux-pane arm
///
/// | Lane | Source | [`HealthSource`] |
/// |---|---|---|
/// | `claude-code/mux-pane` | the registry row's status string, then the liveness gate | `RegistryStatus`, or `ProcessLiveness` when the gate corrected it |
/// | `codex/*` | the rollout tail (`codex::derive_status`) — connectionless | `TranscriptTail` |
/// | `pi/mux-pane` | the transcript tail (`pi::session::derive_status`) | `TranscriptTail` |
/// | `pi/daemon` | the resident `is_streaming` point-read, transcript tail on a miss | `LiveRpc`, else `TranscriptTail` |
/// | `acp/*` | adapter pid liveness + `--listen` identity (`acp_status_classify`) | `ProcessLiveness` |
///
/// **BUG 1 was pi/mux-pane returning `Idle` with no source at all**: the resident
/// point-read gates on `endpoint.is_some()`, a pi TUI row has no endpoint by
/// construction, so every such row fell through to the join's `unwrap_or(Idle)`
/// having consulted nothing. The endpoint-free derivation
/// [`crate::provider::pi::session::derive_status`] is that missing source, and
/// keying on the lane is what makes it reachable. The same reasoning extends
/// pi/daemon: an unreachable resident now falls back to the transcript rather
/// than to a sourceless `Idle`.
///
/// A lane never reports a status it did not observe. Where a source is consulted
/// and has nothing to say — a codex thread with no turn anchors, a pi session in
/// its lazy-write window — the answer is `Idle` with that source named, which is
/// the join's own documented posture ("a just-created session is idle") and is
/// categorically different from never looking.
pub fn health_for(
    lane: Lane,
    paths: &QdPaths,
    env: &dyn Env,
    id: &SessionId,
) -> Result<Health, LaneError> {
    let scanned = live_row(paths, id).ok_or_else(|| LaneError::NotFound { id: id.clone() })?;
    let e = &scanned.entry;
    let now = crate::effects::Clock::now_ms(&crate::effects::RealClock);

    let (status, source) = match (lane.harness, lane.mode) {
        // --- ACP: the adapter's pid + its recorded `--listen` identity. The ONLY
        //     production ACP status source there is; `AcpProvider::parse_status`
        //     synthesizes a shape nothing calls. Never reads the stored status
        //     string — that is the stale-data failure this classification exists
        //     to avoid.
        (Harness::AcpClaudeCode | Harness::Opencode, _) => (acp_status(e), HealthSource::ProcessLiveness),

        // --- codex: the rollout tail, both lanes. Connectionless by design, which
        //     is why `qd ls` exempts codex rows from the pid liveness gate — a
        //     codex row's liveness IS this file read.
        (Harness::Codex, _) => (codex_status(paths, env, e), HealthSource::TranscriptTail),

        // --- pi has no app-server residence; unreachable through `Lane::new`.
        //     Answered rather than left to a wildcard so the arm below keeps
        //     meaning "pi/daemon" exactly.
        (Harness::Pi, Mode::AppServer) => (SessionStatus::Cold, HealthSource::ProcessLiveness),

        // --- pi/daemon: the resident point-read first (it is the drop-immune
        //     signal `qd wait` gates on), the transcript tail when the resident is
        //     unreachable or the identity gate refuses.
        (Harness::Pi, Mode::Daemon) => match gather::pi_live_status(&scanned) {
            Some((s, _turns)) => (gate(from_model(s), e), HealthSource::LiveRpc),
            None => (gate(pi_transcript_status(paths, env, e), e), HealthSource::TranscriptTail),
        },

        // --- pi/mux-pane: BUG 1. No endpoint exists, so the resident read can
        //     never apply; the transcript tail is the whole answer.
        (Harness::Pi, Mode::Pane) => (
            gate(pi_transcript_status(paths, env, e), e),
            HealthSource::TranscriptTail,
        ),

        // --- pi/extension: ASK the session. This is the lane's whole point.
        //
        //     The pane sibling above can only read the transcript, and the
        //     transcript cannot answer "is pi working right now" at all — it is
        //     a record of what finished, and for a fresh session it does not
        //     exist yet (pi writes nothing until its first assistant reply). So
        //     `pi/mux-pane` reports `Busy` never, and `Idle` for a session that
        //     may be mid-turn.
        //
        //     Here `isIdle()` is read out of the running process. `LiveRpc` is
        //     the honest source name and it is the same one `pi/daemon` uses for
        //     its resident point-read — the transport differs, the epistemic
        //     status does not.
        //
        //     Falling back to the transcript when the channel does not answer is
        //     the `pi/daemon` posture exactly: an unreachable front is a reason
        //     to consult the slower source, not a reason to fail a read-only
        //     verb. A session whose pi died has no channel AND a stale
        //     transcript, and `gate` is what keeps that from reading as live.
        (Harness::Pi, Mode::Extension) => {
            match extension_live_status(env, e) {
                Some(s) => (gate(s, e), HealthSource::LiveRpc),
                None => (
                    gate(pi_transcript_status(paths, env, e), e),
                    HealthSource::TranscriptTail,
                ),
            }
        }


        // --- claude: the registry status string, as the join reads it. The string
        //     has no writer inside this repo (`07-lane-gaps.md` §C), so
        //     `RegistryStatus` must NOT be read as "observed just now" — it is
        //     whatever is on disk, however stale. The gate is what makes it
        //     honest about liveness.
        (Harness::ClaudeCode, _) => {
            let raw = claude_status(e);
            let gated = gate(raw, e);
            let source = if gated == raw {
                HealthSource::RegistryStatus
            } else {
                // The gate did not read the status string — it read the process
                // table. Reporting `RegistryStatus` for an answer the registry
                // did not give would be the lane misattributing its own source.
                HealthSource::ProcessLiveness
            };
            (gated, source)
        }
    };

    Ok(Health {
        status,
        source,
        observed_at_ms: Some(now),
    })
}

/// The live registry row for an id — the same exact, id-keyed read
/// [`crate::lanes::row_for_id`] does, minus the mux join (`health` needs no pane
/// coordinates).
fn live_row(paths: &QdPaths, id: &SessionId) -> Option<ScannedEntry> {
    crate::registry::read_entries(&paths.sessions_dir, false)
        .into_iter()
        .find(|s| s.entry.session_id.as_deref() == Some(id.as_str()))
}

/// `pi/extension`'s live signal: one `health` frame off the control channel.
///
/// `None` means "the channel did not answer", which the caller turns into a
/// transcript read — never into a status. The distinction is the point: this
/// function reports what it OBSERVED or nothing at all, and a lane that cannot
/// reach its session must not invent `Idle` for it.
///
/// # Cost, and why a read-only verb may pay it
///
/// A loopback `connect(2)`, one line written, one line read, on a socket whose
/// server is in the same process tree. It is bounded by
/// [`crate::provider::pi::extension::client::CALL_TIMEOUT`] and by the row
/// having an endpoint at all — a row with no recorded socket short-circuits to
/// the derived path, and a missing file fails the connect immediately rather
/// than waiting. `pi/daemon` already pays the same shape of cost for its
/// resident point-read (`gather::pi_live_status`), and for the same reason: it
/// is the only signal that can distinguish busy from idle.
fn extension_live_status(
    env: &dyn Env,
    e: &crate::registry::RegistryEntry,
) -> Option<SessionStatus> {
    // A dead pid cannot be serving a socket, and checking first turns the common
    // cold case into a process-table read instead of a connect that has to fail.
    if !e.pid.is_some_and(|p| crate::effects::is_pid_alive(p as i32)) {
        return None;
    }
    let sock = crate::provider::pi::extension::socket_for(
        env,
        e.endpoint.as_deref(),
        e.session_id.as_deref().unwrap_or_default(),
    );
    let health = crate::provider::pi::extension::Client::connect(&sock)
        .and_then(|mut c| c.health())
        .ok()?;
    Some(if health.busy {
        SessionStatus::Busy
    } else {
        SessionStatus::Idle
    })
}

/// claude's raw signal IS the registry status STRING — fed to `parse_status` as a
/// JSON String, exactly as the join does. The `Idle` fallback is the CALLER's
/// choice, not the provider's: `parse_status` returns `None` on an
/// unknown/missing token and the caller picks.
fn claude_status(e: &crate::registry::RegistryEntry) -> SessionStatus {
    let p = crate::provider::provider_for("claude-code").expect("claude-code is always registered");
    p.parse_status(&serde_json::Value::String(
        e.status.clone().unwrap_or_default(),
    ))
    .map(from_model)
    .unwrap_or(SessionStatus::Idle)
}

/// codex's connectionless rollout-tail derivation. Absent (no rollout resolved,
/// or no turn anchors in it) ⇒ `Idle` — a fresh thread with no anchors is idle,
/// and NO socket is ever opened.
fn codex_status(
    paths: &QdPaths,
    env: &dyn Env,
    e: &crate::registry::RegistryEntry,
) -> SessionStatus {
    use crate::provider::codex::{self, CodexProvider};
    use crate::provider::{Provider, SessionKey};

    let Some(sid) = e.session_id.as_deref() else {
        return SessionStatus::Idle;
    };
    let fx = gather::root_fx(env, paths);
    let root = CodexProvider.transcript_root(&fx);
    let key = SessionKey {
        id: sid,
        name: e.name.as_deref(),
        cwd: e.cwd.as_deref(),
        pid: e.pid,
    };
    CodexProvider
        .transcript_path(&root, &key)
        .and_then(|p| codex::derive_status(&codex::rollout::read_lines(&p)))
        .map(from_model)
        .unwrap_or(SessionStatus::Idle)
}

/// pi's endpoint-free derivation — the source added for BUG 1, mirroring codex's
/// rollout-tail read. `None` means "no on-disk signal" (the lazy-write window)
/// and the caller picks `Idle`, exactly as the join already does for codex;
/// `derive_status`'s own docs forbid coercing it any closer to the read.
fn pi_transcript_status(
    paths: &QdPaths,
    env: &dyn Env,
    e: &crate::registry::RegistryEntry,
) -> SessionStatus {
    let Some(sid) = e.session_id.as_deref() else {
        return SessionStatus::Idle;
    };
    let p = crate::provider::provider_for("pi").expect("pi is always registered");
    let fx = gather::root_fx(env, paths);
    let root = p.transcript_root(&fx);
    crate::provider::pi::session::derive_status_for(&root, sid, e.cwd.as_deref())
        .map(from_model)
        .unwrap_or(SessionStatus::Idle)
}

/// The pure 3-way ACP classification, moved verbatim from `verbs/ls.rs`.
///
/// Derived from the LIVE primary probe, never the stale stored status:
///   - pid not alive                       → "stopped"       (Killed)
///   - pid alive ∧ our adapter for the ep  → "live"          (Idle) — never Busy
///   - pid alive but identity-fail (reuse) → "dead-endpoint"  (Cold)
///
/// Identity is `cmdline_is_our_acp_daemon` (the live cmdline carries the recorded
/// `--listen <endpoint>`), the SAME liveness the resume gate uses — NO
/// reachability connect (it would misread a busy-but-alive adapter as dead AND
/// can never crash `ls` on a dead endpoint).
///
/// **There is no way to know whether an ACP session is busy** (`07-lane-gaps.md`
/// §B). `Idle` here means "alive and ours", not "not working". Ruling B stands:
/// do not invent a busy source, and do not narrow the contract to hide the gap.
pub fn acp_status_classify(pid_alive: bool, identity_ok: bool) -> SessionStatus {
    if !pid_alive {
        SessionStatus::Killed
    } else if identity_ok {
        SessionStatus::Idle
    } else {
        SessionStatus::Cold
    }
}

fn acp_status(e: &crate::registry::RegistryEntry) -> SessionStatus {
    let pid = e.pid.filter(|&p| p != 0);
    let pid_alive = pid
        .map(|p| crate::effects::is_pid_alive(p as i32))
        .unwrap_or(false);
    // The endpoint rides the registry row the lane already has in hand — the ls
    // seam re-reads it by pid because a `Session` does not carry it; here it is a
    // field away.
    let endpoint = e.endpoint.clone().filter(|s| !s.is_empty());
    let identity_ok = match (pid, endpoint.as_deref()) {
        (Some(p), Some(ep)) => crate::provider::acp::residence::cmdline_is_our_acp_daemon(
            crate::create_daemon::real_cmdline_probe(p).as_deref(),
            Some(ep),
        ),
        _ => false,
    };
    acp_status_classify(pid_alive, identity_ok)
}

/// The `qd ls` render-path liveness gate, applied to the lanes `ls` applies it
/// to.
///
/// Carried verbatim from `verbs/ls.rs`: a row shown live whose pid is classified
/// NOT-alive (zombie/gone/exited, or a reused pid) is downgraded to `cold`, and a
/// HEADLESS row whose owning per-session daemon is down is downgraded too — the
/// daemon can die while the orphaned claude child it launched stays alive, so the
/// pid classifier alone would keep the row's stale `busy`.
///
/// **codex and `acp/*` are exempt**, and that exemption is the interesting part:
/// both are daemon-hosted with their OWN liveness model (a rollout tail; an
/// adapter pid plus its `--listen` identity), so a claude `(pid, starttime)` gate
/// would mis-classify them. In this module that exemption is structural — the
/// codex and ACP arms simply never call this — rather than the string test
/// `s.provider != "codex" && !s.provider.starts_with("acp/")` it replaces.
fn gate(status: SessionStatus, e: &crate::registry::RegistryEntry) -> SessionStatus {
    let src = crate::liveness::OsLiveness::new();
    let env = crate::effects::RealEnv;
    let qrmux_dir = crate::effects::Env::var(&env, "HOME")
        .filter(|h| !h.is_empty())
        .and_then(|h| crate::qrmux_dir::resolve_qrmux_dir(std::path::Path::new(&h), &env).ok());
    let daemon_src = crate::liveness::SocketDaemonLiveness::new(qrmux_dir);
    from_model(crate::liveness::gated_ls_status_headless(
        to_model(status),
        e.entrypoint.as_deref(),
        e.name.as_deref(),
        e.pid,
        e.started_at,
        &daemon_src,
        &src,
    ))
}

// --- the two status vocabularies -------------------------------------------
//
// `contract::SessionStatus` is qw's WIRE type and `model::SessionStatus` is the
// one every internal source speaks. The mapping is total and lossless in both
// directions (the tokens are identical by construction), so these are spelled
// out rather than hidden behind a `From` impl — a `From` between two enums with
// the same variant names is exactly the kind of conversion that silently absorbs
// a newly-added variant.

fn from_model(s: ModelStatus) -> SessionStatus {
    match s {
        ModelStatus::Idle => SessionStatus::Idle,
        ModelStatus::Busy => SessionStatus::Busy,
        ModelStatus::Shell => SessionStatus::Shell,
        ModelStatus::Cold => SessionStatus::Cold,
        ModelStatus::Killed => SessionStatus::Killed,
    }
}

fn to_model(s: SessionStatus) -> ModelStatus {
    match s {
        SessionStatus::Idle => ModelStatus::Idle,
        SessionStatus::Busy => ModelStatus::Busy,
        SessionStatus::Shell => ModelStatus::Shell,
        SessionStatus::Cold => ModelStatus::Cold,
        SessionStatus::Killed => ModelStatus::Killed,
    }
}

// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The absent/unreadable distinction, which is the ONLY thing
    /// [`Listing::degraded`] can carry today — so it had better be drawn on the
    /// right side.
    ///
    /// A missing root is silence: a user who does not run codex has no codex
    /// root, and reporting that as a degradation would cry wolf on every host.
    /// Anything else is UNKNOWN and must be said out loud — here through a
    /// corrupt store (a FILE where the root should be a directory), which is
    /// deterministic on every platform and needs no mode bits, unlike the
    /// permission case it stands in for.
    #[test]
    fn an_absent_store_is_silence_and_an_unreadable_one_is_not() {
        let tmp = tempfile::tempdir().unwrap();

        let absent = tmp.path().join("no-such-root");
        assert_eq!(root_degradation(&absent), None, "absent IS the answer");

        let corrupt = tmp.path().join("root-is-a-file");
        std::fs::write(&corrupt, b"not a directory").unwrap();
        let d = root_degradation(&corrupt).expect("an unreadable root must be reported");
        assert_eq!(d.source, corrupt.display().to_string());
        assert!(!d.detail.is_empty(), "the OS error must be preserved verbatim");

        assert_eq!(root_degradation(tmp.path()), None, "a readable root is clean");
    }

    /// The store-ownership table above, asserted. A harness's cold store is owned
    /// by exactly ONE of its lanes, and it is the one
    /// [`Harness::cold_store_owner_mode`] names — the same lane
    /// `provider::row_hosting(provider, None)` sends a cold row to.
    ///
    /// **Asserted against `cold_store_owner_mode`, NOT `create_default_mode`.**
    /// The two answer the same today and they are not the same question: the
    /// create default is about sessions that do not exist yet and is free to
    /// move, while a transcript already on disk records no hosting and so is
    /// stuck with the lane a hosting-less row re-derives to. If the create
    /// default moves and this test still passes, that is the split working.
    #[test]
    fn exactly_one_lane_per_harness_owns_the_cold_store() {
        for h in Harness::ALL {
            let owners: Vec<Lane> = Lane::ALL
                .into_iter()
                .filter(|l| l.harness == h && lane_owns_a_cold_store(*l))
                .collect();
            assert!(
                owners.len() <= 1,
                "{h:?}: {} lanes claim the cold store; a store on disk records no \
                 hosting, so two claimants means every row is counted twice",
                owners.len()
            );
            if let Some(l) = owners.first() {
                assert_eq!(
                    l.mode,
                    h.cold_store_owner_mode(),
                    "{h:?}: the cold store must belong to the lane cold_store_owner_mode \
                     names — that is where row_hosting(provider, None) already sends a \
                     hosting-less cold row, and a transcript on disk carries no hosting \
                     of its own to say otherwise"
                );
            }
        }
    }

    /// Mirrors the `list_for` match arms. Kept as a separate predicate so the
    /// test above cannot drift from the implementation without one of the two
    /// failing to compile.
    fn lane_owns_a_cold_store(l: Lane) -> bool {
        !matches!(
            (l.harness, l.mode),
            (Harness::Codex, Mode::Pane | Mode::Extension | Mode::AppServer)
                | (Harness::Pi, Mode::Pane | Mode::Extension | Mode::AppServer)
                | (Harness::AcpClaudeCode, _)
        )
    }

    /// The ACP truth table (`07-lane-gaps.md` §B), including the part that is a
    /// LIMITATION rather than a behaviour: alive-and-ours is `Idle`, never `Busy`,
    /// because no ACP busy source exists.
    #[test]
    fn acp_classification_is_liveness_only_never_busy() {
        assert_eq!(acp_status_classify(false, false), SessionStatus::Killed);
        assert_eq!(acp_status_classify(false, true), SessionStatus::Killed);
        assert_eq!(acp_status_classify(true, true), SessionStatus::Idle);
        assert_eq!(acp_status_classify(true, false), SessionStatus::Cold);
    }

    #[test]
    fn the_status_vocabularies_round_trip() {
        for s in [
            SessionStatus::Idle,
            SessionStatus::Busy,
            SessionStatus::Shell,
            SessionStatus::Cold,
            SessionStatus::Killed,
        ] {
            assert_eq!(from_model(to_model(s)), s);
        }
    }
}
