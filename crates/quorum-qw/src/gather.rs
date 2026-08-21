//! The gather half of `qd ls` — every effectful read the session join consumes,
//! and the two carriers it fills.
//!
//! # Why this is qw's
//!
//! `dispatch::join` was one file doing two jobs: it READ the world (mux dirs,
//! the registry, relay sidecars, the process table, every provider's store) and
//! then it MERGED what it read into one list of sessions. The reads are session
//! management — qw's, by the same rule that moved `registry`, `mux`, `relay` and
//! `provider_gather` here. The merge is qd's: which source wins for a duplicated
//! session id is a presentation ruling, owned and written down in
//! `08-session-merge-policy.md`, and it stays in `dispatch::join`.
//!
//! So the cut is: **qw gathers, qd merges.** [`gather`] / [`gather_with_dirs`]
//! produce a [`JoinInputs`]; `dispatch::join::join_sessions_counted` consumes one.
//! The boundary is the struct, not a step in anyone's control flow.
//!
//! # The two types that had to come along
//!
//! [`JoinInputs`] is the boundary value itself — produced here, consumed there —
//! so it belongs on the producing side. [`JoinOpts`] came with it only because it
//! is in the gather's SIGNATURE: the gather reads exactly two flags out of it
//! (`include_tombstoned`, `include_preview`). Splitting it into a separate
//! gather-side options struct would have changed every call site, and this was a
//! file split rather than a rewrite — so it moved whole and `dispatch::join`
//! re-exports it, keeping it qd's option surface in every way except which crate
//! declares it.
//!
//! Both are RE-EXPORTED from `dispatch::join`, so every existing
//! `join::JoinInputs` / `join::gather` path resolves unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use quorum_core::discovery::{AcquireFailure, DiscoveryHealth};

use crate::effects::{Clock, Env, ProcessTable, RelayProbe};
use crate::jsonl::{JsonlStats, TranscriptMeta};
use crate::model::SessionStatus;
use crate::mux::{merge_canonical_wins, Mux, MuxSession};
use crate::mux_selector::MuxDirs;
use crate::paths::QdPaths;
use crate::registry::{self, ScannedEntry, TombstonedEntry};
use crate::relay;
use crate::zmx_dir;

/// Pre-gathered inputs for the pure join (see the DESIGN NOTE above).
#[derive(Debug, Clone, Default)]
pub struct JoinInputs {
    /// Cross-dir merged zmx sessions (canonical-wins already applied).
    pub zmx_sessions: Vec<MuxSession>,
    /// LIVE registry entries (non-tombstoned).
    pub registry: Vec<ScannedEntry>,
    /// Tombstoned registry entries (separate, used only with `include_tombstoned`).
    pub tombstoned: Vec<TombstonedEntry>,
    /// Relay sidecar/probe results.
    pub relays: Vec<crate::model::RelayHealth>,
    /// pid → ppid for the ancestor walk (TS `ps -eo pid=,ppid=`).
    pub ppid_map: HashMap<i32, i32>,
    /// All scanned transcripts (mtime ms), `scanAllJsonlFiles`.
    pub transcripts: Vec<TranscriptMeta>,
    /// `getJsonlStats(path)` for every consulted path, keyed by path.
    pub stats_for: HashMap<PathBuf, JsonlStats>,
    /// `findJsonlPath(sessionId, cwd)` result per sessionId (live + tombstone).
    pub jsonl_path_for: HashMap<String, PathBuf>,
    /// Live claude processes (stray discovery, spec §7).
    pub claude_procs: Vec<crate::effects::ProcInfo>,
    /// Injected clock for the stray activity badge.
    pub now_ms: i64,
    /// Backend-keyed: when true (EMBEDDED lane only), a live registry row whose
    /// PID/ancestor walk fails to find its mux session falls back to a BY-NAME
    /// match against an unused mux session. The embedded qrmux daemon tracks each
    /// session by NAME and the registry row carries that same name, so the link is
    /// deterministic and needs no `ps` ancestry. This closes the cold-start race
    /// (C1 redfix): on a fresh `qd new`, the claude registry row lands before the
    /// child's ppid edge is visible in the engine's single `ps` snapshot, so the
    /// pid→mux-pid ancestry walk transiently misses — leaving `zmx_name = None`
    /// and `send:pty` falsely reporting "not mux-live". The zmx lane keeps this
    /// FALSE (byte-stable): zmx tracks the shell, where by-name merging an
    /// unmatched zmx session into a live row would change the TS-faithful output.
    pub match_live_by_name: bool,
    /// codex P2 W5 (codex-p2-spec sections 3.3, 7.4): the CONNECTIONLESS status a
    /// codex LIVE registry row derives, keyed by sessionId (= thread uuid). Built
    /// in the gather step (which does the rollout-tail I/O) and consumed by the
    /// pure live-row loop, exactly as `stats_for`/`jsonl_path_for` pre-gather the
    /// claude transcript I/O. A codex row whose id is in this map takes that status
    /// INSTEAD of the string-based `parse_status` dispatch; a codex row absent from
    /// it (no rollout/no anchors) falls back to Idle (a just-created codex session
    /// is idle). NO SOCKET is ever opened (the durable rollout file is the source).
    /// EMPTY for the claude lane (no codex rows) → byte-stable.
    pub codex_status_for: HashMap<String, SessionStatus>,
    /// codex P2 W5 (codex-p2-spec section 7.4): COLD codex rows (foreign/dead
    /// threads discovered under the codex root, joined by id against live rows —
    /// live wins). The ColdJsonl analog for the codex provider. EMPTY when no codex
    /// root exists (most hosts) → a cheap no-op. Appended by the codex gather step
    /// AFTER the four claude-shaped branches; the join emits them as `Cold` rows.
    pub codex_cold: Vec<CodexColdRow>,
    /// B1: the LIVE status a pi registry row derives from its resident's
    /// `is_streaming` POINT-READ, keyed by sessionId. Unlike codex (a connectionless
    /// rollout-tail read), pi has no on-disk turn-state file, so the ONLY faithful
    /// live signal is the resident get_state — the gather step opens ONE short-lived
    /// front connection per live pi row. A row absent from this map (unreachable /
    /// dead / mis-identified resident) falls back to Idle in the join (the codex
    /// absent-row posture). EMPTY when there are no pi rows → byte-stable no-op.
    pub pi_status_for: HashMap<String, SessionStatus>,
    /// B1: the resident's drop-immune completed-turn count per live pi row (the
    /// busy→idle-edge count, NOT a raw `agent_end` tally), from the SAME get_state
    /// round-trip as `pi_status_for`. A row present here OVERRIDES the generic
    /// transcript-derived `turns` for that pi row; absent → the transcript value.
    pub pi_turns_for: HashMap<String, u64>,
    /// lsview A2: COLD pi rows — on-disk pi transcripts discovered under the pi
    /// sessions root by the gather step's per-provider scan union (the ColdJsonl
    /// analog for the pi provider, the way `codex_cold` is for codex). Their stats
    /// ride the shared `stats_for` map (keyed by transcript path). EMPTY when the
    /// pi store is empty OR its root is absent (`scan_transcripts` returns empty on
    /// a missing root) → a clean zero, byte-stable no-op for every non-pi fleet.
    pub pi_cold: Vec<TranscriptMeta>,
    /// lsview A3: COLD OpenCode rows — sessions read from the monolithic
    /// `opencode.db` `session` table (via `quorum_qw::provider_gather`'s OpenCode
    /// gather step). The ColdJsonl analog for the OpenCode provider (the way
    /// `codex_cold` is for codex). Their
    /// stats arrive PRE-DERIVED on the row (R1's ONE enumerate query), NOT via the
    /// shared `stats_for` cache — an OpenCode SQL read is not a transcript read, so
    /// it never touches the A1 read-counter. EMPTY when the OpenCode store is empty
    /// OR its root is absent → a clean zero, byte-stable no-op for every non-opencode
    /// fleet.
    pub opencode_cold: Vec<OpencodeColdRow>,
    /// Which of the gather's effectful reads FAILED (as opposed to finding
    /// nothing). The join itself never consults this — it is carried so the
    /// VERB layer can tell "no receive path" from "could not determine a
    /// receive path". Default (= everything succeeded) keeps every existing
    /// fixture's meaning exactly. See [`quorum_core::discovery`].
    pub discovery: DiscoveryHealth,
}

// The two COLD-row carriers the codex/OpenCode gathers build now live with the
// gathers themselves, in `quorum_qw::provider_gather`. RE-EXPORTED here because
// they are part of [`JoinInputs`]'s shape, which is qd's own surface: every
// `join::CodexColdRow` path — in this module's tests and in any consumer — keeps
// resolving.
pub use crate::provider_gather::{CodexColdRow, OpencodeColdRow};

/// Join options (TS `getAllSessions` opts + `applyListCap` limit).
#[derive(Debug, Clone, Copy, Default)]
pub struct JoinOpts {
    pub include_all: bool,
    pub include_tombstoned: bool,
    pub include_preview: bool,
    /// Explicit `-n N`. The CLI layer parses `parseInt` → on NaN it passes
    /// `None` (TS `opts.limit ? parseInt(opts.limit,10) : undefined`); a
    /// non-positive / non-integer value is treated as unset by `apply_list_cap`.
    pub limit: Option<i64>,
}

// --- I/O half: gather (session.ts: the reads done up-front in getAllSessions). ---

/// Collect every input the pure join needs, doing ALL I/O against injected roots
/// and the Mux/effects seams. Mirror of the up-front reads in `getAllSessions`
/// (session.ts:836-840) plus the per-entry `findJsonlPath`/`getJsonlStats` and
/// the ancestor `ps` read — all pre-gathered here so the join stays pure.
///
/// `tmp_root` is the dir TS hard-codes as `/tmp` for legacy zmx-dir discovery
/// (parameterized so tests inject a temp dir AS `/tmp`; production passes the
/// literal `/tmp`).
///
/// `xdg` is the XDG runtime-dir family to scan for legacy dirs, or `None` to
/// suppress it (hermetic test fixtures / isolated mode). PRODUCTION callers pass
/// `Some(XdgFamily::from_env(env, uid))`; the scan-root identity and XDG-family
/// Option are INDEPENDENT axes (ADD-9b red-team BLOCKER 1: a temp `tmp_root` must
/// NOT suppress the XDG family, and the literal `/tmp` must NOT force it on).
#[allow(clippy::too_many_arguments)]
pub fn gather(
    paths: &QdPaths,
    mux: &dyn Mux,
    env: &dyn Env,
    pt: &dyn ProcessTable,
    probe: &dyn RelayProbe,
    clock: &dyn Clock,
    tmp_root: &Path,
    xdg: Option<&zmx_dir::XdgFamily>,
    opts: JoinOpts,
) -> JoinInputs {
    // The ZMX-LANE dir computation, unchanged (byte-identical to pre-C1): canonical
    // + legacy. The C1 backend-aware path goes through [`gather_with_dirs`] with a
    // pre-built [`MuxDirs`]; this signature stays stable so every existing zmx-path
    // test compiles + passes unchanged (spec item 2).
    let dirs = MuxDirs::zmx(env, tmp_root, xdg);
    gather_with_dirs(paths, mux, &dirs, pt, probe, clock, env, opts)
}

/// The backend-aware gather (C1 D2 item 2): the dir computation is lifted out of
/// [`gather`] into a [`MuxDirs`] selected by the backend. The zmx lane builds the
/// canonical+legacy list (byte-identical to the old inline code); the embedded
/// lane builds a SINGLE [`crate::qrmux_dir::resolve_qrmux_dir`] dir with an EMPTY
/// legacy list. Everything else (registry/relay/ppid/transcripts) is unchanged.
#[allow(clippy::too_many_arguments)]
pub fn gather_with_dirs(
    paths: &QdPaths,
    mux: &dyn Mux,
    mux_dirs: &MuxDirs,
    pt: &dyn ProcessTable,
    probe: &dyn RelayProbe,
    clock: &dyn Clock,
    // codex P2 W5 (codex-p2-spec section 7.4): the env seam the codex gather step
    // resolves `$CODEX_HOME` off (via `CodexProvider::transcript_root`). The claude
    // gather below ignores it (its root is `paths.projects_dir`), so the claude
    // path is byte-identical; the codex step is a cheap no-op when no codex root
    // exists. Threaded here (not derived inside) for the L9a injected-env discipline.
    env: &dyn Env,
    opts: JoinOpts,
) -> JoinInputs {
    // --- mux: ordered dirs (canonical first), per-dir filtered list, canonical-wins. ---
    let mut discovery = DiscoveryHealth::default();
    let mut scans: Vec<(PathBuf, Vec<MuxSession>)> = Vec::new();
    for dir in mux_dirs.ordered() {
        // A failed list is NOT an empty list: record it, so a missing
        // `zmx_name`/`socket_dir` downstream reads as undetermined rather than
        // as a confirmed absence of the PTY carrier. First failure wins (they
        // share one cause); the empty-result path is untouched.
        let list = match mux.list(&dir) {
            Ok(list) => list,
            Err(e) => {
                discovery
                    .mux_list
                    .get_or_insert_with(|| AcquireFailure::new("mux list", &e));
                Vec::new()
            }
        };
        scans.push((dir, list));
    }
    let zmx_sessions = merge_canonical_wins(scans);

    // --- registry reads (live + tombstoned). ---
    let mut registry = registry::read_entries(&paths.sessions_dir, false);
    let tombstoned = if opts.include_tombstoned {
        registry::get_tombstoned_entries(&paths.sessions_dir)
    } else {
        Vec::new()
    };

    // --- relay (sidecars, else probe). ---
    let relays = relay::get_relay_ports(&paths.relay_dir, probe);

    // --- ppid map + claude procs. ---
    // An unreadable process table is the load-bearing failure: `match_by_ancestry`
    // over an EMPTY map matches no relay, so EVERY claude row silently loses its
    // `relay_port`. Recording the error is what lets the send refusal say
    // "could not determine" instead of asserting an absence it never observed.
    let ppid_map = match pt.ppid_map() {
        Ok(map) => map,
        Err(e) => {
            discovery.process_table = Some(AcquireFailure::new("ps", &e));
            HashMap::new()
        }
    };
    let claude_procs = match pt.claude_procs() {
        Ok(procs) => procs,
        Err(e) => {
            discovery.claude_procs = Some(AcquireFailure::new("ps", &e));
            Vec::new()
        }
    };

    // --- provider gather (qd/qw split): every provider-side read the join needs
    //     — claude/pi transcript scans + the shared stats cache, codex's
    //     connectionless rollout derivation + cold discovery, pi's resident
    //     point-read, OpenCode's one sqlite query. ALL of it lives in
    //     `quorum_qw::provider_gather`, which is where provider knowledge
    //     belongs; what stays here is the merge that consumes it. The registry is
    //     passed MUTABLY because the codex step also binds a freshly-discovered
    //     thread id into an interactive row (in memory and on disk), so the same
    //     `ls` that discovers it also renders it identified. ---
    let gathered = crate::provider_gather::gather_providers(
        paths,
        env,
        &mut registry,
        &tombstoned,
        opts.include_preview,
    );

    JoinInputs {
        zmx_sessions,
        registry,
        tombstoned,
        relays,
        ppid_map,
        // Splatted field-by-field rather than nested: the pure join reads these
        // as independent pre-gathered channels, and keeping them flat is what
        // lets the decider — and every fixture that builds a `JoinInputs`
        // literal — stay untouched by the move.
        transcripts: gathered.claude_transcripts,
        stats_for: gathered.stats_for,
        jsonl_path_for: gathered.jsonl_path_for,
        claude_procs,
        now_ms: clock.now_ms(),
        // EMBEDDED lane → by-name live-row fallback (C1 redfix race fix). The zmx
        // lane stays false to preserve TS-byte-identical output.
        match_live_by_name: matches!(mux_dirs, MuxDirs::Embedded { .. }),
        codex_status_for: gathered.codex_status_for,
        codex_cold: gathered.codex_cold,
        pi_status_for: gathered.pi_status_for,
        pi_turns_for: gathered.pi_turns_for,
        pi_cold: gathered.pi_cold,
        opencode_cold: gathered.opencode_cold,
        discovery,
    }
}
