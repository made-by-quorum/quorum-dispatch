//! M4: getAllSessions join decider + apply_list_cap, TS src/session.ts:609-645,830-1083.
//!
//! `join_sessions` is the pure, mechanical port of `getAllSessions`
//! (session.ts:830-1048): zmx-by-pid + 3-level ancestor matching, sessionId
//! dedupe keep-most-recent, live-from-registry → cold-from-JSONL (mtime-sorted)
//! → unmatched-zmx → (opencode: EMPTY in A1) → tombstoned-if-requested → sort by
//! lastActive desc → `apply_list_cap`.
//!
//! ## DESIGN NOTE — pre-gathered I/O (faithful pure port)
//!
//! TS interleaves I/O inside the join: `findJsonlPath` + `getJsonlStats` per live
//! entry (session.ts:905-908), `scanAllJsonlFiles` + per-file `getJsonlStats`
//! (session.ts:946-951), `findJsonlPath`+`getJsonlStats` per tombstone
//! (session.ts:1016-1019), and a `ps` ancestry walk. To keep the decider PURE we
//! pre-gather all of it into [`JoinInputs`]:
//!
//! - `transcripts` — `scanAllJsonlFiles` result (mtime in ms).
//! - `stats_for` — `getJsonlStats(path)` for EVERY path the join will consult
//!   (live, cold, tombstone), keyed by path.
//! - `jsonl_path_for` — `findJsonlPath(sessionId, cwd)` result per sessionId
//!   (live + tombstone rows look up by id), keyed by id.
//! - `ppid_map` — the `ps -eo pid=,ppid=` map for the ancestor walk.
//! - `relays`/`registry`/`tombstoned` — the three reads done up-front in TS.
//!
//! [`gather`] does exactly these reads against injected roots + the Mux seam.
//!
//! ## STRAY WIRING (spec §7)
//!
//! [`join_with_strays`] runs the TS-faithful join, then calls [`stray::classify`]
//! and returns the strays ALONGSIDE the sessions: `(Vec<Session>, Vec<Stray>)`.
//! The TS-faithful rows are byte-identical to the TS output; strays are an
//! ADDITIONAL surface that [`render::ls_json`](crate::render::ls_json) appends as
//! `status: "unmanaged"` rows. This keeps `model.rs` untouched (no `Unmanaged`
//! status variant) and the non-stray ls --json byte-identical to TS. The whole
//! stray shape is pass-(b) regenerate-friendly + fixture-frozen.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::effects::{Clock, Env, ProcessTable, RelayProbe};
use crate::jsonl::{self, JsonlStats, TranscriptMeta};
use crate::model::{Session, SessionBranch, SessionStatus};
use crate::mux::{merge_canonical_wins, Mux, MuxSession};
use crate::paths::QdPaths;
use crate::registry::{self, ScannedEntry, TombstonedEntry};
use crate::relay::{self, match_by_ancestry};
use crate::stray::{self, Stray};
use crate::{codes, resolve, zmx_dir};

/// Backend-selected socket-dir set the gather scans (C1 D2 item 2).
///
/// The dir computation is the ONLY backend-divergent part of gather. zmx keeps the
/// canonical+legacy cross-dir scan (Bug-D, byte-identical to pre-C1); embedded
/// uses a SINGLE [`crate::qrmux_dir::resolve_qrmux_dir`] dir with NO legacy list
/// (the embedded daemon binds one dir; there is no TMPDIR-scatter to recover from).
///
/// The backend is parsed ONCE (mux_selector) and feeds BOTH this resolver AND the
/// runtime mux selection — no divergent double-read of QD_MUX (spec item 4).
#[derive(Debug, Clone)]
pub enum MuxDirs {
    /// canonical first, then legacy (the cross-dir Bug-D scan order).
    Zmx {
        canonical: PathBuf,
        legacy: Vec<PathBuf>,
    },
    /// the single embedded qrmux dir (legacy list EMPTY by construction).
    Embedded { dir: PathBuf },
}

impl MuxDirs {
    /// Build the zmx-lane dir set: `resolve_zmx_dir` + `legacy_zmx_dirs` against
    /// `tmp_root` (the literal `/tmp` in production; a temp dir in tests) and the
    /// XDG family. Byte-identical to the pre-C1 inline computation in `gather`.
    pub fn zmx(env: &dyn Env, tmp_root: &Path, xdg: Option<&zmx_dir::XdgFamily>) -> Self {
        Self::zmx_roots(env, &[tmp_root.to_path_buf()], xdg)
    }

    /// As [`zmx`](Self::zmx) but over an explicit list of legacy SCAN ROOTS — the
    /// production caller passes the result of [`zmx_dir::legacy_scan_roots`] (the
    /// A14-2(c) test-lane override applied to the surviving READ scan). Single root
    /// `[/tmp]` is the production default; the test lane substitutes a jail dir.
    pub fn zmx_roots(
        env: &dyn Env,
        scan_roots: &[PathBuf],
        xdg: Option<&zmx_dir::XdgFamily>,
    ) -> Self {
        let canonical = zmx_dir::resolve_zmx_dir(env);
        let legacy = zmx_dir::legacy_zmx_dirs(env.uid(), &canonical, scan_roots, xdg);
        MuxDirs::Zmx { canonical, legacy }
    }

    /// Build the embedded-lane dir set: a single resolved qrmux dir.
    pub fn embedded(dir: PathBuf) -> Self {
        MuxDirs::Embedded { dir }
    }

    /// The dirs to scan, canonical FIRST (precedence order for `merge_canonical_wins`).
    pub fn ordered(&self) -> Vec<PathBuf> {
        match self {
            MuxDirs::Zmx { canonical, legacy } => {
                let mut dirs = vec![canonical.clone()];
                dirs.extend(legacy.iter().cloned());
                dirs
            }
            MuxDirs::Embedded { dir } => vec![dir.clone()],
        }
    }
}

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
}

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

/// 3-level ancestor walk (TS `getAncestorPids`, session.ts:609-645). PURE.
///
/// For each pid, walk UP `levels` times collecting ancestors; stop when the
/// parent is missing, equals the current pid (self-cycle), or is `<= 1`.
///
/// zmx-tracks-the-shell comment (session.ts:885-886): the chain can be
/// `zmx → bash → bash → claude`, so the registered claude pid's GRANDPARENT (or
/// great-grandparent) may be the zmx-tracked shell — hence walking up to 3.
fn ancestor_pids(
    pids: &[i32],
    ppid_map: &HashMap<i32, i32>,
    levels: usize,
) -> HashMap<i32, Vec<i32>> {
    let mut out = HashMap::new();
    for &pid in pids {
        let mut ancestors = Vec::new();
        let mut current = pid;
        for _ in 0..levels {
            let Some(&parent) = ppid_map.get(&current) else {
                break;
            };
            if parent == current || parent <= 1 {
                break;
            }
            ancestors.push(parent);
            current = parent;
        }
        out.insert(pid, ancestors);
    }
    out
}

/// The pure join decider — mechanical port of `getAllSessions`
/// (session.ts:830-1048). No I/O; everything comes from [`JoinInputs`].
pub fn join_sessions(inputs: &JoinInputs, opts: JoinOpts) -> Vec<Session> {
    join_sessions_counted(inputs, opts).0
}

/// [`join_sessions`] plus the cap-drop count (punch B5 item 2-D): the second
/// element is how many ELIGIBLE rows the active cap truncated (0 when the view
/// is uncapped). The `ls` verb's loud truncation trailer needs
/// "total-eligible − shown" without re-deriving the default view's eligibility
/// filter outside this module.
pub fn join_sessions_counted(inputs: &JoinInputs, opts: JoinOpts) -> (Vec<Session>, usize) {
    // zmxByPid map (session.ts:842-843).
    let mut zmx_by_pid: HashMap<i32, &MuxSession> = HashMap::new();
    for z in &inputs.zmx_sessions {
        zmx_by_pid.insert(z.pid, z);
    }

    // zmxByName index (C1 redfix) — the EMBEDDED-lane by-name fallback for live
    // rows whose pid/ancestor walk transiently misses the mux session (cold-start
    // ppid-snapshot race). Built only when `match_live_by_name`; empty otherwise
    // so the zmx lane is untouched. Last-occurrence-per-name wins (mirrors the
    // unmatched-zmx Map.set rule), which is moot for embedded (names are unique).
    let mut zmx_by_name: HashMap<String, &MuxSession> = HashMap::new();
    if inputs.match_live_by_name {
        for z in &inputs.zmx_sessions {
            zmx_by_name.insert(z.name.clone(), z);
        }
    }

    // Relay: match by PID parentage (session.ts:845-873).
    let relay_by_claude_pid = match_by_ancestry(&inputs.relays, &inputs.ppid_map);

    // Deduplicate PID entries by sessionId — keep most-recent updatedAt
    // (session.ts:875-883). TS keys by `p.sessionId`; an entry with no sessionId
    // keys under "" (the empty string) and the same keep-newest rule applies.
    //
    // R5-2 (red-team round 5, Child B / opencode D1): the key carries the row's
    // PROVIDER CLASS (`acp/*` vs everything else) alongside the sessionId. The
    // collapse exists for one-session-many-process-generations (a codex resume's
    // stale old row, a claude re-incarnation) — always SAME-class rows. But an
    // `acp/*` row and a plain `claude-code` row can legitimately share ONE
    // sessionId (Child B's PTY floor ran such a companion by design; Child D
    // retired that floor, but the pair still arises from a leftover Child-B-era
    // dev companion row or a human manually running `claude --resume
    // <session_id>`). Keying by sessionId alone let the plain twin's liveness
    // heartbeat permanently shadow the ACP row out of every join-derived
    // surface (ls, send, wait, resume, attach, info) — hiding the session's
    // canonical identity (and its refusal surface). Same-class rows keep the
    // exact old keep-newest collapse; only the cross-class pair coexists.
    let mut pid_by_session_id: HashMap<(String, bool), &registry::RegistryEntry> = HashMap::new();
    // Preserve first-seen order of sessionIds so the live-row emission order is
    // deterministic (TS iterates a Map whose insertion order is first-seen).
    let mut order: Vec<(String, bool)> = Vec::new();
    let provider_class_is_acp = |e: &registry::RegistryEntry| -> bool {
        e.provider.as_deref().is_some_and(|p| p.starts_with("acp/"))
    };
    for scanned in &inputs.registry {
        let e = &scanned.entry;
        let sid = e.session_id.clone().unwrap_or_default();
        let key = (sid, provider_class_is_acp(e));
        match pid_by_session_id.get(&key) {
            Some(existing) => {
                // p.updatedAt > existing.updatedAt (missing → treated as 0).
                let new_ua = e.updated_at.unwrap_or(0);
                let old_ua = existing.updated_at.unwrap_or(0);
                if new_ua > old_ua {
                    pid_by_session_id.insert(key, e);
                }
            }
            None => {
                order.push(key.clone());
                pid_by_session_id.insert(key, e);
            }
        }
    }
    let deduped: Vec<&registry::RegistryEntry> =
        order.iter().map(|key| pid_by_session_id[key]).collect();

    // Ancestor walk for zmx matching (session.ts:887, getAncestorPids 3 levels).
    let live_pids: Vec<i32> = deduped
        .iter()
        .filter_map(|p| p.pid.map(|x| x as i32))
        .collect();
    let ancestor_map = ancestor_pids(&live_pids, &inputs.ppid_map, 3);

    let mut sessions: Vec<Session> = Vec::new();
    let mut seen_session_ids: HashSet<String> = HashSet::new();
    let mut used_zmx: HashSet<String> = HashSet::new();

    // --- Live sessions from PID registry (session.ts:893-934). ---
    for p in &deduped {
        let sid = p.session_id.clone().unwrap_or_default();
        seen_session_ids.insert(sid.clone());

        // Check PID itself + ancestors against zmx.
        let pid_i32 = p.pid.map(|x| x as i32);
        let mut zmx_match: Option<&MuxSession> =
            pid_i32.and_then(|pid| zmx_by_pid.get(&pid).copied());
        if zmx_match.is_none() {
            if let Some(pid) = pid_i32 {
                if let Some(ancestors) = ancestor_map.get(&pid) {
                    for a in ancestors {
                        if let Some(z) = zmx_by_pid.get(a).copied() {
                            zmx_match = Some(z);
                            break;
                        }
                    }
                }
            }
        }

        // EMBEDDED-lane BY-NAME fallback (C1 redfix). The pid/ancestor walk depends
        // on the engine's single `ps` snapshot carrying the registry pid's ppid edge
        // up to the mux-tracked pid. On a fresh `qd new` cold-start that edge can be
        // invisible (the child is forked microseconds before the snapshot), so the
        // walk misses even though the mux session is listed — leaving a live session
        // wrongly unlinked. The embedded daemon keys sessions by NAME and the
        // registry row carries that same name, so match by name when the pid path
        // came up empty. Deterministic: no dependence on `ps` propagation. Skip a
        // mux session already claimed by an earlier row (`used_zmx`). zmx lane:
        // `zmx_by_name` is empty → no-op (byte-stable).
        if zmx_match.is_none() {
            if let Some(n) = nonempty(p.name.clone()) {
                if let Some(z) = zmx_by_name.get(&n).copied() {
                    if !used_zmx.contains(&z.name) {
                        zmx_match = Some(z);
                    }
                }
            }
        }

        // findJsonlPath(sessionId, cwd) + getJsonlStats (pre-gathered).
        let jsonl_path = inputs.jsonl_path_for.get(&sid).cloned();
        let stats = jsonl_path
            .as_ref()
            .and_then(|p| inputs.stats_for.get(p).cloned())
            .unwrap_or_default();

        if let Some(z) = zmx_match {
            used_zmx.insert(z.name.clone());
        }

        // codex P1 W6 (codex-p1-spec section 7.2): resolve the provider value
        // ONCE here — it keys BOTH the status derivation (below) and the
        // `provider` field of the constructed row (the W1 read-back: absent on
        // disk = "claude-code" at the read-back boundary). Resolve-once shape:
        // the value is computed exactly once per row and reused, never
        // re-derived.
        let provider_id = p
            .provider
            .clone()
            .unwrap_or_else(|| "claude-code".to_string());

        // codex P1 W6 (codex-p1-spec section 7.2): status derivation moves BEHIND
        // the provider seam. The claude raw signal IS the registry status STRING
        // (join.rs §1.8) — feed it to `provider.parse_status` as a JSON String
        // (one small allocation per live row, acceptable on a CLI path). The
        // fallback (Idle) is the JOIN's choice, not the provider's: the trait doc
        // says `parse_status` returns None on unknown/missing and the CALLER
        // picks the fallback.
        //
        // War story carried from the pre-seam site (join.rs:307-317): TS trusts
        // the string (`status: pid.status`). An unknown/missing string can't be
        // one of the five typed variants; we fall back to Idle — fixtures never
        // exercise it (a live registry entry is by definition live; idle is the
        // safe live default; UNREACHABLE for valid fixtures).
        //
        // UNKNOWN-provider rows (`provider_for` → None): we STILL derive status
        // via the claude rules here (render-survival, L8 — `ls` must show the
        // row; ACTING verbs already refuse via the W1 arming). This is a
        // render-survival derivation, NOT a dispatch endorsement of the unknown
        // provider.
        //
        // codex P2 W5 (codex-p2-spec sections 3.3, 7.4): a codex LIVE row does NOT
        // use the string-based `parse_status` path — its status derives
        // CONNECTIONLESS from the rollout tail, pre-gathered into
        // `codex_status_for` (the gather step did the rollout I/O; the join stays
        // pure). A codex row absent from the map (no rollout / no turn anchors yet)
        // is Idle — a just-created codex session is idle. NO socket is ever opened.
        // This is the SAME claude-parity dead-row posture: the join never gates on
        // pid-aliveness (claude rows keep their registry status when the pid dies;
        // reconcile/gc tombstones a dead row OUT OF BAND), so a dead-daemon codex
        // row keeps its last rollout-derived status here, never inventing a new
        // join-time Cold/Killed transition.
        //
        // MUTATION EVIDENCE (codex-p2-spec section 13 "rollout busy/idle anchor
        // inverted"): an open-turn rollout pre-derives Busy into the map; if the
        // codex branch fell back to parse_status (which returns None for a codex
        // row → Idle) the busy assertion in the W5 join tests + the ls-codex golden
        // would red.
        let status = if provider_id == "codex" {
            inputs
                .codex_status_for
                .get(&sid)
                .copied()
                .unwrap_or(SessionStatus::Idle)
        } else if provider_id == "pi" {
            // B1: a live pi row's status is the resident `is_streaming` point-read
            // (gather step). Absent from the map = unreachable/dead resident → Idle
            // (parity with codex's absent-row posture; a cold/just-created pi row
            // reads idle). The string-based `parse_status` is NEVER used for pi — its
            // native signal is an isStreaming object, not the claude status STRING
            // (which pi's parse_status rejects → the always-Idle bug this replaces).
            inputs
                .pi_status_for
                .get(&sid)
                .copied()
                .unwrap_or(SessionStatus::Idle)
        } else {
            let status_provider = crate::provider::provider_for(&provider_id)
                .unwrap_or_else(|| crate::provider::provider_for("claude-code").unwrap());
            status_provider
                .parse_status(&serde_json::Value::String(
                    p.status.clone().unwrap_or_default(),
                ))
                .unwrap_or(SessionStatus::Idle)
        };

        // B1: a live pi row's turn count is the resident's drop-immune busy→idle-edge
        // count (gather step), overriding the generic transcript-derived value; any
        // other provider (and a pi row absent from the map) keeps the transcript
        // `turns`. Precomputed here because `sid` is moved into the Session below.
        let row_turns = if provider_id == "pi" {
            inputs
                .pi_turns_for
                .get(&sid)
                .copied()
                .unwrap_or(stats.turns)
        } else {
            stats.turns
        };

        // name: pid.name || jsonlStats.name (TS ||: empty string is falsy too).
        let name = nonempty(p.name.clone()).or_else(|| stats.name.clone());
        // userNamed: !!pid.name || !!jsonlStats.userNamed.
        let user_named = nonempty(p.name.clone()).is_some() || stats.user_named;

        sessions.push(Session {
            name,
            user_named: Some(user_named),
            session_id: sid,
            code: None,
            qd_id: None,
            pid: p.pid,
            status,
            zmx_name: zmx_match.map(|z| z.name.clone()),
            zmx_clients: zmx_match.map(|z| z.clients),
            socket_dir: zmx_match.and_then(|z| z.socket_dir.clone()),
            relay_port: pid_i32.and_then(|pid| relay_by_claude_pid.get(&pid).copied()),
            turns: row_turns,
            tokens: stats.tokens,
            cwd: p.cwd.clone(),
            last_active_ms: p.updated_at,
            version: p.version.clone(),
            started_at_ms: p.started_at,
            git_branch: stats.git_branch.clone(),
            jsonl_path: jsonl_path.map(path_to_string),
            last_turns: stats.last_turns.clone(),
            // codex P1, R1 (codex-p1-spec section 3.2): the persisted field, read
            // back once above as `provider_id` (absent = claude-code at the
            // read-back boundary; the default is NEVER written to disk — flipping
            // it reds the existing goldens). codex P1 W6 reuses that SAME
            // resolved value here (resolve-once: it also keyed the status
            // derivation above — never re-derived).
            provider: provider_id,
            // WP-B5-ii-a (ii): carry the registry `entrypoint` onto the row (same
            // read-back shape as `provider`/`cwd`/`version`) so the `qd ls`
            // daemon-down render gate can scope itself to headless rows. Only the
            // LiveRegistry branch has a registry row to source it; all other
            // branches below carry `None`.
            entrypoint: p.entrypoint.clone(),
            lineage: None,
            which_branch: SessionBranch::LiveRegistry,
        });
    }

    // zmx sessions not matched to a Claude PID — index by name (session.ts:937-942).
    // TS `Map.set` keeps the LAST occurrence per name; we replicate that while
    // preserving first-seen iteration order for the eventual emission.
    let mut unmatched_order: Vec<String> = Vec::new();
    let mut unmatched_zmx: HashMap<String, &MuxSession> = HashMap::new();
    for z in &inputs.zmx_sessions {
        if !used_zmx.contains(&z.name) {
            if !unmatched_zmx.contains_key(&z.name) {
                unmatched_order.push(z.name.clone());
            }
            unmatched_zmx.insert(z.name.clone(), z);
        }
    }

    // --- Cold sessions from JSONL, mtime-sorted desc (session.ts:944-978). ---
    let mut all_jsonl: Vec<&TranscriptMeta> = inputs.transcripts.iter().collect();
    // b.mtime - a.mtime (descending). Stable sort matches TS Array.sort stability.
    all_jsonl.sort_by_key(|t| std::cmp::Reverse(t.mtime_ms));

    // Item 3 (red-team round-2): an acp/* session is DAEMON-hosted, but its transcript IS
    // the bridge's CC JSONL under ~/.claude/projects — so the claude ColdJsonl scan would
    // otherwise SHADOW a STOPPED acp row's tombstone (claiming its sessionId FIRST, before
    // the Tombstoned branch), surfacing it as a claude ColdJsonl row (provider "claude-code"
    // + a JSONL-derived name) instead of the Tombstoned acp row (provider "acp/claude-code"
    // + the FRIENDLY name). That breaks `qd resume <name>` post-stop (the friendly name is
    // lost) AND misroutes resume to the claude path (not `run_acp_resume`'s faithful
    // `session/load`). When tombstones are requested, let an ACP tombstone WIN over its own
    // cold-JSONL shadow — skip the ColdJsonl row here so the Tombstoned branch surfaces the
    // proper acp row. SCOPED to acp/* tombstones; claude/codex keep the cold-wins collapse
    // (byte-stable — the `tombstone_seen_guard_skips_already_cold` regression is preserved).
    let acp_tombstone_sids: HashSet<&str> = if opts.include_tombstoned {
        inputs
            .tombstoned
            .iter()
            .filter(|t| {
                t.data
                    .provider
                    .as_deref()
                    .map(|p| p.starts_with("acp/"))
                    .unwrap_or(false)
            })
            .filter_map(|t| t.data.session_id.as_deref())
            .collect()
    } else {
        HashSet::new()
    };

    for jf in all_jsonl {
        if seen_session_ids.contains(&jf.session_id) {
            continue;
        }
        // An acp tombstone owns this sessionId → let the Tombstoned branch surface it
        // (acp provider + friendly name), NOT a shadowing claude ColdJsonl row.
        if acp_tombstone_sids.contains(jf.session_id.as_str()) {
            continue;
        }
        let stats = inputs.stats_for.get(&jf.path).cloned().unwrap_or_default();
        // lastActive = new Date(lastTimestamp) if present, else jf.mtime.
        let last_active_ms = stats
            .last_timestamp
            .as_deref()
            .and_then(parse_iso_ms)
            .unwrap_or(jf.mtime_ms);

        // name-merge: consume an unmatched zmx with the same name (session.ts:957-958).
        let name = stats.name.clone();
        let zmx_match: Option<&MuxSession> =
            name.as_deref().and_then(|n| unmatched_zmx.get(n).copied());
        if let Some(n) = name.as_deref() {
            if zmx_match.is_some() {
                unmatched_zmx.remove(n);
                // unmatched_order keeps the name; ZmxOnly emission below skips it.
            }
        }

        sessions.push(Session {
            name,
            user_named: Some(stats.user_named),
            session_id: jf.session_id.clone(),
            code: None,
            qd_id: None,
            pid: zmx_match.map(|z| z.pid as i64),
            // TS: `zmxMatch ? "cold" : "cold"` — cold either way (ported honestly).
            status: SessionStatus::Cold,
            zmx_name: zmx_match.map(|z| z.name.clone()),
            zmx_clients: zmx_match.map(|z| z.clients),
            socket_dir: zmx_match.and_then(|z| z.socket_dir.clone()),
            relay_port: None,
            turns: stats.turns,
            tokens: stats.tokens,
            // cwd: jsonlStats.cwd || jf.projectDir.
            cwd: nonempty(stats.cwd.clone()).or_else(|| Some(jf.project_dir.clone())),
            last_active_ms: Some(last_active_ms),
            version: None,
            started_at_ms: None,
            git_branch: stats.git_branch.clone(),
            jsonl_path: Some(path_to_string(jf.path.clone())),
            last_turns: stats.last_turns.clone(),
            // codex P1, R1 (codex-p1-spec section 3.2): STRUCTURAL literal, not a
            // default — a ColdJsonl row derives from the claude transcript scan
            // (jsonl.rs is claude's transcript surface), so it is claude's lane by
            // construction; there is no persisted provider field to read here.
            provider: "claude-code".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: SessionBranch::ColdJsonl,
        });
        // Mark this id seen so a later branch (tombstoned / codex-cold) keyed on
        // the SAME session_id does not emit a second row. Without this, a cold
        // JSONL row plus a tombstoned registry entry for the same session both
        // surface — the "Ambiguous — matches 2 sessions … same id, one dead PID,
        // one PID -" collision in resume/include_tombstoned callers.
        seen_session_ids.insert(jf.session_id.clone());
    }

    // --- Remaining unmatched zmx (no JSONL match), session.ts:980-996. ---
    for name in &unmatched_order {
        let Some(z) = unmatched_zmx.get(name).copied() else {
            continue; // was consumed by a cold name-merge above.
        };
        sessions.push(Session {
            name: Some(z.name.clone()),
            user_named: None, // ABSENT in the ZmxOnly branch (TS literal omits it).
            session_id: String::new(), // TS "".
            code: None,
            qd_id: None,
            pid: Some(z.pid as i64),
            status: SessionStatus::Cold,
            zmx_name: Some(z.name.clone()),
            zmx_clients: Some(z.clients),
            socket_dir: z.socket_dir.clone(),
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: nonempty(Some(z.start_dir.clone())),
            // lastActive = new Date(z.created * 1000): seconds → ms.
            last_active_ms: Some(z.created * 1000),
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            // codex P1, R1 (codex-p1-spec section 3.2): STRUCTURAL literal, not a
            // default — a ZmxOnly row is a mux-pane-only row from the mux-pane lane,
            // which is claude's surface; no persisted provider field exists here.
            provider: "claude-code".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: SessionBranch::ZmxOnly,
        });
    }

    // --- OpenCode: NONE in A1 (named exclusion, spec §2). The TS appends
    //     getOpenCodeSessions() here (session.ts:998-1007); A1 emits nothing —
    //     keeping provider/opencode fields on the model so the join is additive
    //     later. No silent skip: this is the recorded exclusion point. ---

    // --- Tombstoned sessions if requested (session.ts:1009-1040). ---
    if opts.include_tombstoned {
        for t in &inputs.tombstoned {
            let sid = t.data.session_id.clone().unwrap_or_default();
            if seen_session_ids.contains(&sid) {
                continue;
            }
            seen_session_ids.insert(sid.clone());

            let jsonl_path = inputs.jsonl_path_for.get(&sid).cloned();
            let stats = jsonl_path
                .as_ref()
                .and_then(|p| inputs.stats_for.get(p).cloned())
                .unwrap_or_default();

            let name = nonempty(t.data.name.clone()).or_else(|| stats.name.clone());
            let user_named = nonempty(t.data.name.clone()).is_some() || stats.user_named;

            sessions.push(Session {
                name,
                user_named: Some(user_named),
                session_id: sid,
                code: None,
                qd_id: None,
                pid: t.data.pid,
                status: SessionStatus::Killed,
                zmx_name: None,
                zmx_clients: None,
                socket_dir: None,
                relay_port: None,
                turns: stats.turns,
                tokens: stats.tokens,
                cwd: t.data.cwd.clone(),
                last_active_ms: t.data.updated_at,
                version: t.data.version.clone(),
                started_at_ms: t.data.started_at,
                git_branch: stats.git_branch.clone(),
                jsonl_path: jsonl_path.map(path_to_string),
                last_turns: stats.last_turns.clone(),
                // codex P1, R1 (codex-p1-spec section 3.2): READ the persisted
                // field from the captured tombstone data; absent = claude-code at
                // the read-back boundary (default never written to disk).
                provider: t
                    .data
                    .provider
                    .clone()
                    .unwrap_or_else(|| "claude-code".to_string()),
                entrypoint: None,
                lineage: None,
                which_branch: SessionBranch::Tombstoned,
            });
        }
    }

    // --- COLD codex rows (codex-p2-spec section 7.4 — the ColdJsonl analog for
    //     the codex provider). An ADDITIVE step: foreign/dead codex threads
    //     discovered under the codex root by the gather step (sqlite index primary,
    //     rollout scan fallback), already joined-against-live (live wins) THERE, so
    //     here we only drop any id already seen as live/cold/tombstoned (the same
    //     seen-guard the tombstone branch uses). Emitted as `Cold` rows in the
    //     ColdJsonl render shape (provider key, no pid/zmx/relay/version) carrying
    //     provider "codex". EMPTY for the claude lane → byte-stable. Cut-ladder
    //     rung C1: if this interaction got gnarly the gather step ships empty and
    //     only live codex rows surface. ---
    for cold in &inputs.codex_cold {
        if seen_session_ids.contains(&cold.id) {
            continue;
        }
        seen_session_ids.insert(cold.id.clone());
        let name = nonempty(cold.name.clone());
        sessions.push(Session {
            // userNamed: a sqlite title is a derived label (codex makes one from
            // the first user message), not a human-chosen name — treat a present
            // title as named (it surfaces in the default view), absent → false.
            user_named: Some(name.is_some()),
            name,
            session_id: cold.id.clone(),
            code: None,
            qd_id: None,
            pid: None,
            status: SessionStatus::Cold,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: nonempty(cold.cwd.clone()),
            last_active_ms: cold.last_active_ms,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: cold.jsonl_path.clone(),
            last_turns: None,
            // codex P2 W5 (codex-p2-spec section 9.1): the codex provider value —
            // the rule-8 contract surface. A cold codex row is the codex lane by
            // construction (discovered under the codex root).
            provider: "codex".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: SessionBranch::ColdJsonl,
        });
    }

    // --- COLD pi rows (lsview A2 — the ColdJsonl analog for the pi provider, the
    //     way the codex-cold branch above is for codex). An ADDITIVE step: on-disk
    //     pi transcripts discovered under the pi sessions root by the gather step's
    //     per-provider scan union (`inputs.pi_cold`), joined-against-live by the
    //     SAME seen-guard the tombstone/codex branches use (any id already emitted
    //     as live/cold/tombstoned wins). Emitted as `Cold` ColdJsonl-shaped rows
    //     carrying provider "pi", with stats pulled from the shared cache
    //     (`stats_for` by transcript path) exactly like the claude ColdJsonl branch
    //     — but with NO zmx name-merge (pi is daemon-hosted, not a mux pane) and NO
    //     `project_dir` cwd fallback (pi's `project_dir` slot is the ENCODED bucket
    //     dir NAME, not a real cwd — cwd comes from the parsed stats only). EMPTY
    //     when the pi store is empty or its root is absent → a clean zero,
    //     byte-stable no-op for every non-pi fleet. ---
    for meta in &inputs.pi_cold {
        if seen_session_ids.contains(&meta.session_id) {
            continue;
        }
        seen_session_ids.insert(meta.session_id.clone());
        let stats = inputs.stats_for.get(&meta.path).cloned().unwrap_or_default();
        // lastActive = new Date(lastTimestamp) if present, else the file mtime
        // (the same derivation the claude ColdJsonl branch uses).
        let last_active_ms = stats
            .last_timestamp
            .as_deref()
            .and_then(parse_iso_ms)
            .unwrap_or(meta.mtime_ms);
        let name = stats.name.clone();
        sessions.push(Session {
            user_named: Some(stats.user_named),
            name,
            session_id: meta.session_id.clone(),
            code: None,
            qd_id: None,
            pid: None,
            status: SessionStatus::Cold,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: stats.turns,
            tokens: stats.tokens,
            // cwd from the parsed stats ONLY — pi's `project_dir` is the encoded
            // bucket dir name, not a real path, so it is NOT a cwd fallback.
            cwd: nonempty(stats.cwd.clone()),
            last_active_ms: Some(last_active_ms),
            version: None,
            started_at_ms: None,
            git_branch: stats.git_branch.clone(),
            jsonl_path: Some(path_to_string(meta.path.clone())),
            last_turns: stats.last_turns.clone(),
            // lsview A2: the pi provider value — a cold pi row is the pi lane by
            // construction (discovered under the pi sessions root). ACTING verbs
            // route it through the pi Hosting::Daemon redirect, NOT the
            // unknown-provider refusal (unchanged — carried constraint #2).
            provider: "pi".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: SessionBranch::ColdJsonl,
        });
    }

    // --- Sort by lastActive descending (session.ts:1043-1045). null → 0. ---
    // TS Array.sort is stable; Rust sort_by_key is stable — equal keys keep order.
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_active_ms.unwrap_or(0)));

    apply_list_cap_counted(sessions, opts.include_all, opts.limit)
}

/// Apply the list cap + default-view filter (TS `applyListCap`, session.ts:1050-1083).
/// PURE.
///
/// Cap policy (war story, session.ts:1050-1064):
///   - An explicit `limit` (qd ls -n N) ALWAYS wins, even with --all.
///   - Otherwise the full view (`include_all`, i.e. `qd ls -a`) is UNCAPPED — the
///     complete authoritative list across every discovered socket dir + all
///     tombstones. (Previously `?? 20` silently capped -a at 20, hiding dead
///     sessions beyond the 20 most-recent — the "qd ls -a is not authoritative"
///     bug.)
///   - The default view (no --all) caps at 20 and shows only named, non-killed.
///
/// A malformed `limit` (0, negative, NaN — e.g. from `qd ls -n abc` →
/// `parseInt`→NaN) is treated as UNSET, never as "show zero": it must not be able
/// to silently empty the authoritative view. Only a positive integer caps. The
/// NaN case arrives here as `None` from the CLI layer (TS
/// `opts.limit ? parseInt(opts.limit,10) : undefined`); `Some(n)` with `n <= 0`
/// is the negative/zero case and is also treated as unset.
pub fn apply_list_cap(
    sessions: Vec<Session>,
    include_all: bool,
    limit: Option<i64>,
) -> Vec<Session> {
    apply_list_cap_counted(sessions, include_all, limit).0
}

/// [`apply_list_cap`] plus the cap-drop count (punch B5 item 2-D): identical
/// filter + cap; the second element is how many ELIGIBLE rows (post-filter) the
/// active cap truncated — 0 when uncapped. The cap may be the default 20 OR an
/// explicit `-n`; the CALLER decides which case warrants a trailer (the ls verb
/// only announces the default-cap case).
pub fn apply_list_cap_counted(
    sessions: Vec<Session>,
    include_all: bool,
    limit: Option<i64>,
) -> (Vec<Session>, usize) {
    // typeof number && isInteger && > 0 ? limit : undefined. `i64` is integral by
    // construction; the validity test is just `> 0`.
    let valid_limit: Option<usize> = match limit {
        Some(n) if n > 0 => Some(n as usize),
        _ => None,
    };
    // validLimit ?? (includeAll ? Infinity : 20).
    let cap: Option<usize> = match valid_limit {
        Some(n) => Some(n),
        None if include_all => None, // Infinity → uncapped.
        None => Some(20),
    };

    if !include_all {
        // Default: only named, non-killed sessions (but include cold ones).
        let filtered: Vec<Session> = sessions
            .into_iter()
            .filter(|s| s.user_named == Some(true) && s.status != SessionStatus::Killed)
            .collect();
        return take_cap(filtered, cap);
    }
    take_cap(sessions, cap)
}

fn take_cap(mut v: Vec<Session>, cap: Option<usize>) -> (Vec<Session>, usize) {
    let dropped = cap.map_or(0, |n| v.len().saturating_sub(n));
    if let Some(n) = cap {
        v.truncate(n);
    }
    (v, dropped)
}

/// Run the TS-faithful join, then attach strays (spec §7).
///
/// Returns `(sessions, strays)`. `sessions` are byte-faithful TS rows;
/// `strays` are the additional live-unmanaged surface that
/// [`render::ls_json`](crate::render::ls_json) appends. Strays are classified
/// against the UNION of live + tombstoned registry session ids and the set of
/// live registry pids (so a managed proc is never stray evidence).
pub fn join_with_strays(inputs: &JoinInputs, opts: JoinOpts) -> (Vec<Session>, Vec<Stray>) {
    let sessions = join_sessions(inputs, opts);

    // registry_session_ids = UNION(live, tombstoned) — a transcript whose id is
    // managed (live OR tombstoned) is never a stray.
    let mut registry_ids: HashSet<String> = HashSet::new();
    for s in &inputs.registry {
        if let Some(id) = &s.entry.session_id {
            registry_ids.insert(id.clone());
        }
    }
    for t in &inputs.tombstoned {
        if let Some(id) = &t.data.session_id {
            registry_ids.insert(id.clone());
        }
    }
    // registry_pids_alive = live registry pids (a proc already accounted for).
    let registry_pids: HashSet<i32> = inputs
        .registry
        .iter()
        .filter_map(|s| s.entry.pid.map(|p| p as i32))
        .collect();

    let strays = stray::classify(
        &inputs.transcripts,
        &registry_ids,
        &registry_pids,
        &inputs.claude_procs,
        inputs.now_ms,
    );
    (sessions, strays)
}

/// Convenience: assign short codes to the joined rows (TS `assignShortCodes`
/// runs post-join, index.ts:55). Strays are coded separately in render.
pub fn assign_codes(sessions: &mut [Session]) {
    codes::assign_short_codes(sessions);
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
    let mut scans: Vec<(PathBuf, Vec<MuxSession>)> = Vec::new();
    for dir in mux_dirs.ordered() {
        let list = mux.list(&dir).unwrap_or_default();
        scans.push((dir, list));
    }
    let zmx_sessions = merge_canonical_wins(scans);

    // --- registry reads (live + tombstoned). ---
    let registry = registry::read_entries(&paths.sessions_dir, false);
    let tombstoned = if opts.include_tombstoned {
        registry::get_tombstoned_entries(&paths.sessions_dir)
    } else {
        Vec::new()
    };

    // --- relay (sidecars, else probe). ---
    let relays = relay::get_relay_ports(&paths.relay_dir, probe);

    // --- ppid map + claude procs. ---
    let ppid_map = pt.ppid_map().unwrap_or_default();
    let claude_procs = pt.claude_procs().unwrap_or_default();

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
    //     missing root), never an error — the brano-today live case.
    //   - codex: its cold store is a sqlite index + rollout tree (NOT a
    //     `scan_transcripts` surface), so its cold discovery stays in
    //     `gather_codex` (result-identical, per the A2 discretion).
    let claude = crate::provider::provider_for("claude-code")
        .expect("claude-code provider is always registered");
    let pi_provider =
        crate::provider::provider_for("pi").expect("pi provider is always registered");
    // A MINIMAL fx for pi's root resolution: pi reads ONLY `fx.env` (HOME /
    // PI_CODING_AGENT_SESSION_DIR), never `fx.paths` — the placeholder home is
    // unused by it (the same minimal-fx shape `gather_codex` builds for codex).
    let pi_fx_home = QdPaths::from_home(Path::new("/nonexistent-pi-fx-home"));
    let pi_fx = crate::provider::ProviderFx {
        env,
        paths: &pi_fx_home,
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
    // claude's scan → the existing ColdJsonl channel (byte-identical). pi's scan →
    // the additive `pi_cold` channel (join emits those as `provider: "pi"` rows).
    let transcripts = claude.scan_transcripts(&paths.projects_dir);
    let pi_cold = pi_provider.scan_transcripts(&pi_provider.transcript_root(&pi_fx));

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
    for scanned in &registry {
        if let Some(sid) = &scanned.entry.session_id {
            let provider_id = scanned.entry.provider.as_deref().unwrap_or("claude-code");
            let prov = crate::provider::provider_for(provider_id)
                .unwrap_or_else(|| crate::provider::provider_for("claude-code").unwrap());
            let key = crate::provider::SessionKey {
                id: sid,
                name: scanned.entry.name.as_deref(),
                cwd: scanned.entry.cwd.as_deref(),
                pid: scanned.entry.pid,
            };
            if let Some(p) = prov.transcript_path(&paths.projects_dir, &key) {
                stats_for.entry(p.clone()).or_insert_with(|| {
                    stats_cache
                        .get_or_read(&p, opts.include_preview, |pp| jsonl::read_stats(pp, true))
                });
                jsonl_path_for.insert(sid.clone(), p);
            }
        }
    }
    // Tombstoned entries (only matters when include_tombstoned).
    // codex P1 W7 (codex-p1-spec section 7.2): same per-row provider dispatch as
    // the live loop above — claude rows route through `transcript_path`, unknown
    // degrades to claude derivation for render-survival.
    for t in &tombstoned {
        if let Some(sid) = &t.data.session_id {
            if !jsonl_path_for.contains_key(sid) {
                let provider_id = t.data.provider.as_deref().unwrap_or("claude-code");
                let prov = crate::provider::provider_for(provider_id)
                    .unwrap_or_else(|| crate::provider::provider_for("claude-code").unwrap());
                let key = crate::provider::SessionKey {
                    id: sid,
                    name: t.data.name.as_deref(),
                    cwd: t.data.cwd.as_deref(),
                    pid: t.data.pid,
                };
                if let Some(p) = prov.transcript_path(&paths.projects_dir, &key) {
                    stats_for.entry(p.clone()).or_insert_with(|| {
                        stats_cache
                            .get_or_read(&p, opts.include_preview, |pp| jsonl::read_stats(pp, true))
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
                stats_cache.get_or_read(&t.path, opts.include_preview, |pp| {
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
        gather_codex(env, &registry, &mut stats_cache);
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
    let (pi_status_for, pi_turns_for) = gather_pi(&registry);

    JoinInputs {
        zmx_sessions,
        registry,
        tombstoned,
        relays,
        ppid_map,
        transcripts,
        stats_for,
        jsonl_path_for,
        claude_procs,
        now_ms: clock.now_ms(),
        // EMBEDDED lane → by-name live-row fallback (C1 redfix race fix). The zmx
        // lane stays false to preserve TS-byte-identical output.
        match_live_by_name: matches!(mux_dirs, MuxDirs::Embedded { .. }),
        codex_status_for,
        codex_cold,
        pi_status_for,
        pi_turns_for,
        pi_cold,
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
        let endpoint = scanned
            .entry
            .endpoint
            .clone()
            .filter(|s| !s.is_empty());
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
    registry: &[ScannedEntry],
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

// --- small helpers ---

/// JS `||` truthiness for an `Option<String>`: `Some("")` is falsy → None.
fn nonempty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.is_empty())
}

fn path_to_string(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

/// Parse `"YYYY-MM-DDTHH:MM:SS[.fff][Z]"` to epoch ms (UTC), for the cold-row
/// `lastActive = new Date(jsonlStats.lastTimestamp)` (session.ts:952-954). Local
/// copy of the same days-from-civil math `jsonl` uses for preview ordering —
/// duplicated here rather than widening the `jsonl` API surface. Anything
/// unrecognized → None (caller falls back to the file mtime). Unit-tested below.
fn parse_iso_ms(ts: &str) -> Option<i64> {
    let bytes = ts.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let num = |a: usize, b: usize| -> Option<i64> { ts.get(a..b)?.parse::<i64>().ok() };
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let min = num(14, 16)?;
    let sec = num(17, 19)?;
    let mut millis = 0i64;
    let rest = &ts[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let mut ms_str = digits;
            ms_str.truncate(3);
            while ms_str.len() < 3 {
                ms_str.push('0');
            }
            millis = ms_str.parse().ok()?;
        }
    }
    let days = days_from_civil(year, month, day)?;
    let secs = days * 86_400 + hour * 3600 + min * 60 + sec;
    Some(secs * 1000 + millis)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm). Same as `jsonl`'s copy.
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

// Re-export the resolve tier used by `qd info` callers (kept here so the join
// module is the single entry-point the CLI consumes).
pub use resolve::{resolve_session, Resolution};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::MuxSession;
    use crate::registry::RegistryEntry;

    fn mux(name: &str, pid: i32, clients: u32, socket: &str, created: i64) -> MuxSession {
        MuxSession {
            name: name.to_string(),
            pid,
            clients,
            created,
            start_dir: "/work".to_string(),
            cmd: "claude".to_string(),
            current: false,
            socket_dir: Some(socket.to_string()),
            ended: None,
            exit_code: None,
            zmx_status: None,
            err: None,
        }
    }

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
            },
            tombstoned: false,
            degraded: Vec::new(),
        }
    }

    fn meta(sid: &str, dir: &str, mtime: i64) -> TranscriptMeta {
        TranscriptMeta {
            session_id: sid.to_string(),
            path: PathBuf::from(format!("/projects/{dir}/{sid}.jsonl")),
            mtime_ms: mtime,
            project_dir: dir.to_string(),
        }
    }

    fn base_inputs() -> JoinInputs {
        JoinInputs::default()
    }

    fn find<'a>(sessions: &'a [Session], sid: &str) -> &'a Session {
        sessions
            .iter()
            .find(|s| s.session_id == sid)
            .unwrap_or_else(|| panic!("no session {sid}"))
    }

    // --- parse_iso_ms ---

    #[test]
    fn iso_ms_parses_and_orders() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        let a = parse_iso_ms("2026-06-04T10:00:00.000Z").unwrap();
        let b = parse_iso_ms("2026-06-04T10:00:01.500Z").unwrap();
        assert_eq!(b - a, 1500);
        // leap day.
        assert!(parse_iso_ms("2024-02-29T12:30:45.678Z").is_some());
        assert_eq!(parse_iso_ms("garbage"), None);
    }

    // --- zmx ancestor matching ---

    #[test]
    fn zmx_matches_via_grandparent_ancestor() {
        // claude pid 400; zmx tracks the SHELL at pid 200 (grandparent).
        // ppid: 400→300→200. zmx_by_pid has 200.
        let mut inputs = base_inputs();
        inputs.registry = vec![live(400, "sid-a", Some("worker"), 5_000)];
        inputs.zmx_sessions = vec![mux("worker-zmx", 200, 1, "/tmp/zmx-501", 1000)];
        inputs.ppid_map = [(400, 300), (300, 200), (200, 100)].into_iter().collect();

        let out = join_sessions(&inputs, JoinOpts::default());
        let s = find(&out, "sid-a");
        assert_eq!(
            s.zmx_name.as_deref(),
            Some("worker-zmx"),
            "matched zmx via grandparent ancestor walk"
        );
        assert_eq!(s.zmx_clients, Some(1));
        assert_eq!(s.socket_dir.as_deref(), Some("/tmp/zmx-501"));
    }

    #[test]
    fn zmx_direct_pid_match_preferred() {
        let mut inputs = base_inputs();
        inputs.registry = vec![live(400, "sid-a", Some("w"), 5_000)];
        inputs.zmx_sessions = vec![mux("direct", 400, 0, "/tmp/zmx-501", 1000)];
        inputs.ppid_map = [(400, 300)].into_iter().collect();
        let out = join_sessions(&inputs, JoinOpts::default());
        assert_eq!(find(&out, "sid-a").zmx_name.as_deref(), Some("direct"));
    }

    // --- C1 redfix: EMBEDDED by-name fallback when the ppid edge is INVISIBLE ---
    //
    // This is the DETERMINISTIC arm for the g_coldstart race. It STRUCTURALLY forces
    // the adverse ordering the live race only sometimes produces: a live registry
    // row whose pid neither equals the mux pid NOR has ANY ppid edge reaching it
    // (empty `ppid_map` = the `ps` snapshot missed the just-forked child). The
    // pid/ancestor walk MUST therefore miss. With `match_live_by_name` (embedded
    // lane), the row falls back to the by-NAME mux match → `zmx_name = Some`. This
    // is the EXACT failure shape captured in the C1 redfix evidence (mux pid 7176,
    // reg pid 7180, engine ppid-chain = [7180] — no edge to 7176).

    #[test]
    fn embedded_by_name_links_live_row_when_ppid_edge_invisible() {
        let mut inputs = base_inputs();
        // Registry row pid 7180; mux session pid 7176 (the parent shell). NO ppid
        // entry for 7180 → the ancestor walk yields nothing (the cold-start race).
        inputs.registry = vec![live(7180, "sid-cold", Some("cold-sess"), 5_000)];
        inputs.zmx_sessions = vec![mux("cold-sess", 7176, 0, "/run/user/501/qrmux", 1000)];
        inputs.ppid_map = HashMap::new();
        inputs.match_live_by_name = true; // EMBEDDED lane.

        let out = join_sessions(&inputs, JoinOpts::default());
        let s = find(&out, "sid-cold");
        assert_eq!(
            s.zmx_name.as_deref(),
            Some("cold-sess"),
            "embedded by-name fallback links the live row despite the missing ppid edge"
        );
        assert_eq!(s.socket_dir.as_deref(), Some("/run/user/501/qrmux"));
        // status stays LIVE (idle/busy from the registry), NOT cold — so send:pty
        // accepts it (the cold guard is what the legacy bug never reached).
        assert_eq!(s.status, SessionStatus::Busy);
    }

    #[test]
    fn zmx_lane_does_not_name_match_live_rows_byte_stable() {
        // The SAME shape under the ZMX lane (match_live_by_name = false): the row
        // must NOT name-match. The mux session stays a SEPARATE unmatched-zmx cold
        // row, and the live row has no zmx_name — preserving TS-byte-identical
        // output. This is the negative control proving the fix is embedded-only.
        let mut inputs = base_inputs();
        inputs.registry = vec![live(7180, "sid-cold", Some("cold-sess"), 5_000)];
        inputs.zmx_sessions = vec![mux("cold-sess", 7176, 0, "/tmp/zmx-501", 1000)];
        inputs.ppid_map = HashMap::new();
        inputs.match_live_by_name = false; // ZMX lane (default).

        // include_all so the unmatched-zmx (userNamed=None) ZmxOnly row is visible
        // — the default view filters it out, which would mask the negative control.
        let opts = JoinOpts {
            include_all: true,
            ..Default::default()
        };
        let out = join_sessions(&inputs, opts);
        assert_eq!(
            find(&out, "sid-cold").zmx_name,
            None,
            "zmx lane never by-name-matches a live row (byte-stable with TS)"
        );
        // The unmatched zmx session surfaces as its OWN separate ZmxOnly row
        // (unchanged TS-faithful behavior — NOT merged into the live row).
        assert!(
            out.iter().any(|s| s.session_id.is_empty()
                && s.zmx_name.as_deref() == Some("cold-sess")
                && s.which_branch == SessionBranch::ZmxOnly),
            "the unmatched zmx session remains a separate ZmxOnly row; got: {:?}",
            out.iter()
                .map(|s| (s.session_id.clone(), s.zmx_name.clone(), s.which_branch))
                .collect::<Vec<_>>()
        );
    }

    // --- sessionId dedupe keep-newest ---

    #[test]
    fn dedupe_keeps_most_recent_updated_at() {
        let mut inputs = base_inputs();
        // Same sessionId, different pids/updatedAt — newest (8000) wins.
        inputs.registry = vec![
            live(100, "dup", Some("old"), 3_000),
            live(200, "dup", Some("new"), 8_000),
        ];
        let out = join_sessions(&inputs, JoinOpts::default());
        let dups: Vec<&Session> = out.iter().filter(|s| s.session_id == "dup").collect();
        assert_eq!(dups.len(), 1, "deduped to one row");
        assert_eq!(dups[0].pid, Some(200));
        assert_eq!(dups[0].name.as_deref(), Some("new"));
    }

    /// [`live`] with an explicit provider (+ optional transport latch) — the R5-2
    /// dedup tests need to distinguish an `acp/*` row from its plain-claude floor
    /// companion sharing the same sessionId.
    fn live_with_provider(
        pid: i64,
        sid: &str,
        name: Option<&str>,
        updated: i64,
        provider: &str,
        transport: Option<&str>,
    ) -> ScannedEntry {
        let mut scanned = live(pid, sid, name, updated);
        scanned.entry.provider = Some(provider.to_string());
        scanned.entry.transport = transport.map(str::to_string);
        scanned
    }

    // R5-2 (red-team round 5, Child B / opencode D1): an `acp/claude-code` row
    // and a plain `claude-code` twin resuming the SAME sessionId (Child B's
    // retired floor companion; post-Child-D, a leftover dev row or a manual
    // `claude --resume`). The twin's liveness heartbeat keeps bumping its
    // updatedAt while the dead ACP row's stamp is frozen — with a
    // sessionId-only dedup key the twin permanently shadowed the ACP row out
    // of every join surface (ls/send/wait/resume/attach/info), hiding the
    // session's canonical identity. MUTATION EVIDENCE: reverting the dedup key
    // to sessionId-only (dropping the provider-class component) reds this test.
    #[test]
    fn acp_row_coexists_with_a_plain_twin_sharing_the_session_id() {
        let mut inputs = base_inputs();
        inputs.registry = vec![
            // The ACP original: a Child-B-era latched row shape, stale updatedAt.
            live_with_provider(100, "shared-sid", Some("base"), 3_000, "acp/claude-code", Some("pty")),
            // The live plain twin (the Child-B-era companion shape): plain
            // claude-code, heartbeat keeps it newest.
            live_with_provider(200, "shared-sid", Some("base-floor"), 9_000, "claude-code", None),
        ];
        let out = join_sessions(&inputs, JoinOpts::default());
        let rows: Vec<&Session> = out.iter().filter(|s| s.session_id == "shared-sid").collect();
        assert_eq!(
            rows.len(),
            2,
            "the acp original and its plain twin must BOTH survive the dedup \
             (cross-provider-class rows are two legitimate sessions, not one stale pair): {:?}",
            out.iter().map(|s| (&s.name, &s.provider)).collect::<Vec<_>>()
        );
        let acp = rows.iter().find(|s| s.provider == "acp/claude-code").expect("acp row emitted");
        let floor = rows.iter().find(|s| s.provider == "claude-code").expect("companion emitted");
        assert_eq!(acp.name.as_deref(), Some("base"));
        assert_eq!(floor.name.as_deref(), Some("base-floor"));
    }

    // R5-2 companion case: SAME-class rows keep the exact old keep-newest collapse —
    // the legitimate one-session-many-process-generations shape (an acp resume's
    // stale old row beside the fresh one) must still dedupe to the newest row.
    #[test]
    fn same_class_acp_rows_still_collapse_keep_newest() {
        let mut inputs = base_inputs();
        inputs.registry = vec![
            live_with_provider(100, "dup-acp", Some("old"), 3_000, "acp/claude-code", None),
            live_with_provider(200, "dup-acp", Some("new"), 8_000, "acp/claude-code", None),
        ];
        let out = join_sessions(&inputs, JoinOpts::default());
        let dups: Vec<&Session> = out.iter().filter(|s| s.session_id == "dup-acp").collect();
        assert_eq!(dups.len(), 1, "same-class rows still dedupe to one");
        assert_eq!(dups[0].pid, Some(200), "keep-newest still wins within the class");
    }

    // --- cold row zmx name-merge ---

    #[test]
    fn cold_row_consumes_unmatched_zmx_by_name() {
        let mut inputs = base_inputs();
        // No registry; a transcript whose stats.name == an unmatched zmx name.
        inputs.transcripts = vec![meta("cold-sid", "-work-proj", 5_000)];
        let stats = JsonlStats {
            name: Some("ghost".to_string()),
            user_named: true,
            turns: 3,
            ..Default::default()
        };
        inputs.stats_for = [(PathBuf::from("/projects/-work-proj/cold-sid.jsonl"), stats)]
            .into_iter()
            .collect();
        inputs.zmx_sessions = vec![mux("ghost", 777, 2, "/tmp/zmx-501", 1000)];

        let out = join_sessions(&inputs, JoinOpts::default());
        // The cold row merged the zmx "ghost"; no separate ZmxOnly row appears.
        let cold = find(&out, "cold-sid");
        assert_eq!(cold.status, SessionStatus::Cold);
        assert_eq!(cold.zmx_name.as_deref(), Some("ghost"));
        assert_eq!(cold.pid, Some(777));
        assert_eq!(cold.turns, 3);
        // Only one row total (no leftover ZmxOnly for "ghost").
        assert_eq!(out.len(), 1, "zmx consumed by name-merge, no ZmxOnly row");
    }

    // --- lsview A2: pi cold rows (the per-provider scan union's pi channel) ---

    /// An on-disk pi transcript (discovered by the gather union's pi scan)
    /// surfaces as a `Cold` ColdJsonl row tagged provider "pi", with stats pulled
    /// from the shared cache by transcript path — no registry row, no zmx, no pid.
    #[test]
    fn pi_cold_rows_emit_as_cold_provider_pi() {
        let mut inputs = base_inputs();
        inputs.pi_cold = vec![meta("pi-sid-1", "--work-pi--", 9_000)];
        let stats = JsonlStats {
            name: Some("pi-session".to_string()),
            user_named: true,
            turns: 4,
            tokens: 1_200,
            cwd: Some("/work/pi".to_string()),
            ..Default::default()
        };
        inputs.stats_for = [(
            PathBuf::from("/projects/--work-pi--/pi-sid-1.jsonl"),
            stats,
        )]
        .into_iter()
        .collect();

        let out = join_sessions(&inputs, JoinOpts::default());
        let row = find(&out, "pi-sid-1");
        assert_eq!(row.provider, "pi", "a cold pi row carries provider pi");
        assert_eq!(row.status, SessionStatus::Cold);
        assert_eq!(row.which_branch, SessionBranch::ColdJsonl);
        assert_eq!(row.turns, 4, "stats flow from the shared cache by path");
        assert_eq!(row.tokens, 1_200);
        assert_eq!(row.cwd.as_deref(), Some("/work/pi"));
        assert_eq!(row.pid, None, "a cold pi row has no pid");
        assert_eq!(row.zmx_name, None, "pi is daemon-hosted — no zmx name-merge");
        assert_eq!(out.len(), 1);
    }

    /// An empty pi store (empty/absent root → `scan_transcripts` returns empty →
    /// `pi_cold` empty) contributes ZERO rows and never disturbs existing rows —
    /// the brano-today clean-zero case, additive no-op.
    #[test]
    fn empty_pi_cold_adds_nothing_to_existing_rows() {
        let mut inputs = base_inputs();
        // one ordinary claude cold row; pi_cold left empty (the default).
        inputs.transcripts = vec![meta("cc-sid", "-proj", 5_000)];
        inputs.stats_for = [(
            PathBuf::from("/projects/-proj/cc-sid.jsonl"),
            JsonlStats { name: Some("cc".into()), user_named: true, ..Default::default() },
        )]
        .into_iter()
        .collect();
        assert!(inputs.pi_cold.is_empty());

        let out = join_sessions(&inputs, JoinOpts::default());
        assert_eq!(out.len(), 1, "empty pi_cold adds no rows");
        assert_eq!(out[0].provider, "claude-code", "the existing row is untouched");
    }

    /// trap #4 (registry-row vs cold-row collision): a session id already emitted
    /// (here a live registry row) is NOT re-emitted as a cold pi row — the shared
    /// seen-guard the tombstone/codex branches use, live-wins.
    #[test]
    fn pi_cold_row_deduped_against_live_same_id_live_wins() {
        let mut inputs = base_inputs();
        inputs.registry = vec![live(4242, "dup-sid", Some("live-row"), 10_000)];
        inputs.pi_cold = vec![meta("dup-sid", "--work-pi--", 9_000)];

        let out = join_sessions(
            &inputs,
            JoinOpts { include_all: true, ..Default::default() },
        );
        let rows: Vec<&Session> = out.iter().filter(|s| s.session_id == "dup-sid").collect();
        assert_eq!(rows.len(), 1, "one row for the shared id (live wins, no cold dup)");
        assert_eq!(rows[0].which_branch, SessionBranch::LiveRegistry);
        assert_ne!(rows[0].provider, "pi", "the live row is not overwritten by a cold pi row");
    }

    // --- zmx-only row epoch math ---

    #[test]
    fn zmx_only_row_created_seconds_to_ms() {
        let mut inputs = base_inputs();
        inputs.zmx_sessions = vec![mux("lonely", 555, 0, "/tmp/zmx-501", 1_700_000_000)];
        // ZmxOnly rows have no userNamed → filtered out of the default view; use
        // include_all to see them (matches TS: a zmx-only row is unnamed).
        let opts = JoinOpts {
            include_all: true,
            ..Default::default()
        };
        let out = join_sessions(&inputs, opts);
        assert_eq!(out.len(), 1);
        let s = &out[0];
        assert_eq!(s.which_branch, SessionBranch::ZmxOnly);
        assert_eq!(s.session_id, "");
        assert_eq!(s.pid, Some(555));
        assert_eq!(s.name.as_deref(), Some("lonely"));
        assert_eq!(s.user_named, None, "ZmxOnly omits userNamed");
        assert_eq!(
            s.last_active_ms,
            Some(1_700_000_000 * 1000),
            "created seconds → ms"
        );
        assert_eq!(s.cwd.as_deref(), Some("/work"));
    }

    // --- tombstone rows only with flag + seen-guard ---

    #[test]
    fn tombstone_only_with_flag() {
        let mut inputs = base_inputs();
        inputs.tombstoned = vec![TombstonedEntry {
            path: PathBuf::from("/sessions/9.json.tombstoned"),
            pid: 9,
            data: RegistryEntry {
                pid: Some(9),
                session_id: Some("dead".to_string()),
                cwd: Some("/work".to_string()),
                started_at: Some(1000),
                updated_at: Some(2000),
                status: Some("idle".to_string()),
                name: Some("gonesoon".to_string()),
                version: Some("1.0".to_string()),
                ..Default::default()
            },
            mtime_ms: 2000,
            degraded: Vec::new(),
        }];
        // Without the flag: no tombstone row.
        let out = join_sessions(&inputs, JoinOpts::default());
        assert!(out.is_empty());
        // With include_tombstoned + include_all: the killed row appears.
        let opts = JoinOpts {
            include_all: true,
            include_tombstoned: true,
            ..Default::default()
        };
        let out = join_sessions(&inputs, opts);
        let s = find(&out, "dead");
        assert_eq!(s.status, SessionStatus::Killed);
        assert_eq!(s.which_branch, SessionBranch::Tombstoned);
        assert_eq!(s.name.as_deref(), Some("gonesoon"));
    }

    #[test]
    fn tombstone_seen_guard_skips_already_live() {
        let mut inputs = base_inputs();
        inputs.registry = vec![live(5, "shared", Some("live-one"), 9_000)];
        inputs.tombstoned = vec![TombstonedEntry {
            path: PathBuf::from("/sessions/6.json.tombstoned"),
            pid: 6,
            data: RegistryEntry {
                pid: Some(6),
                session_id: Some("shared".to_string()),
                updated_at: Some(2000),
                status: Some("idle".to_string()),
                name: Some("dead-one".to_string()),
                ..Default::default()
            },
            mtime_ms: 2000,
            degraded: Vec::new(),
        }];
        let opts = JoinOpts {
            include_all: true,
            include_tombstoned: true,
            ..Default::default()
        };
        let out = join_sessions(&inputs, opts);
        // sessionId "shared" was seen as live → tombstone skipped.
        let rows: Vec<&Session> = out.iter().filter(|s| s.session_id == "shared").collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, SessionStatus::Busy);
        assert_eq!(rows[0].name.as_deref(), Some("live-one"));
    }

    #[test]
    fn tombstone_seen_guard_skips_already_cold() {
        // A cold JSONL row and a tombstoned registry entry for the SAME session_id
        // must collapse to ONE row (the cold one wins, emitted first). Regression
        // for the resume "Ambiguous — matches 2 sessions" collision where one row
        // carried the dead tombstone PID and the other (cold) had PID -.
        let mut inputs = base_inputs();
        inputs.transcripts = vec![meta("shared", "-work-proj", 5_000)];
        inputs.stats_for = [(
            PathBuf::from("/projects/-work-proj/shared.jsonl"),
            JsonlStats {
                name: Some("cold-one".to_string()),
                user_named: true,
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        inputs.tombstoned = vec![TombstonedEntry {
            path: PathBuf::from("/sessions/6.json.tombstoned"),
            pid: 6,
            data: RegistryEntry {
                pid: Some(6),
                session_id: Some("shared".to_string()),
                updated_at: Some(2000),
                status: Some("idle".to_string()),
                name: Some("dead-one".to_string()),
                ..Default::default()
            },
            mtime_ms: 2000,
            degraded: Vec::new(),
        }];
        let opts = JoinOpts {
            include_all: true,
            include_tombstoned: true,
            ..Default::default()
        };
        let out = join_sessions(&inputs, opts);
        // sessionId "shared" was seen as cold → tombstone skipped; no dup, no dead pid.
        let rows: Vec<&Session> = out.iter().filter(|s| s.session_id == "shared").collect();
        assert_eq!(
            rows.len(),
            1,
            "cold + tombstone for same id must collapse to one"
        );
        assert_eq!(rows[0].status, SessionStatus::Cold);
        assert_eq!(rows[0].name.as_deref(), Some("cold-one"));
        assert_eq!(rows[0].pid, None);
    }

    /// Item 3 (red-team round-2 root-cause): a STOPPED dispatch acp row has BOTH a cold CC
    /// JSONL (the bridge's transcript under ~/.claude/projects) AND an acp tombstone for the
    /// SAME sessionId. The acp TOMBSTONE must WIN over the claude ColdJsonl shadow — so the
    /// row carries `provider="acp/claude-code"` (→ resume routes to `run_acp_resume`, NOT
    /// the claude path) and the FRIENDLY name (→ `qd resume <name>` resolves post-stop).
    /// REVERT CONTROL: remove the acp-tombstone skip in the ColdJsonl loop → the row
    /// surfaces as ColdJsonl (`provider="claude-code"`, name "jsonl-title") → these asserts
    /// RED (the exact red-team failure: `claude-<sid>` route + name not resolvable).
    #[test]
    fn acp_tombstone_wins_over_its_cold_jsonl_shadow() {
        let mut inputs = base_inputs();
        // The bridge's CC JSONL exists for the acp sessionId (the shadow source).
        inputs.transcripts = vec![meta("acp-S", "-w-projX", 5_000)];
        inputs.stats_for = [(
            PathBuf::from("/projects/-w-projX/acp-S.jsonl"),
            JsonlStats {
                name: Some("jsonl-title".to_string()),
                user_named: true,
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        inputs.jsonl_path_for =
            [("acp-S".to_string(), PathBuf::from("/projects/-w-projX/acp-S.jsonl"))]
                .into_iter()
                .collect();
        // The acp tombstone (kill_acp preserves provider + friendly name).
        inputs.tombstoned = vec![TombstonedEntry {
            path: PathBuf::from("/sessions/7.json.tombstoned"),
            pid: 7,
            data: RegistryEntry {
                pid: Some(7),
                session_id: Some("acp-S".to_string()),
                name: Some("myacp".to_string()),
                provider: Some("acp/claude-code".to_string()),
                cwd: Some("/w/projX".to_string()),
                status: Some("idle".to_string()),
                ..Default::default()
            },
            mtime_ms: 2000,
            degraded: Vec::new(),
        }];
        let opts = JoinOpts {
            include_all: true,
            include_tombstoned: true,
            ..Default::default()
        };
        let out = join_sessions(&inputs, opts);
        let rows: Vec<&Session> = out.iter().filter(|s| s.session_id == "acp-S").collect();
        assert_eq!(rows.len(), 1, "exactly one row for the acp sessionId (no dup)");
        let r = rows[0];
        assert_eq!(
            r.which_branch,
            SessionBranch::Tombstoned,
            "the acp TOMBSTONE wins, not the claude ColdJsonl shadow"
        );
        assert_eq!(
            r.provider, "acp/claude-code",
            "provider preserved → resume routes to run_acp_resume (not the claude path)"
        );
        assert_eq!(
            r.name.as_deref(),
            Some("myacp"),
            "the FRIENDLY name is surfaced → qd resume <name> resolves post-stop"
        );
        // resolve-by-name finds the stopped acp row (the user path).
        match crate::resolve::resolve_session("myacp", &out) {
            crate::resolve::Resolution::One(s) => assert_eq!(s.session_id, "acp-S"),
            other => panic!("resolve-by-name must find the stopped acp row, got {other:?}"),
        }
    }

    /// Negative control / isolation: a CLAUDE (non-acp) tombstone + its cold JSONL still
    /// COLLAPSES cold-wins (unchanged) — the acp fix does not perturb the claude/codex
    /// path. (Mirrors `tombstone_seen_guard_skips_already_cold`, asserting the fix is
    /// acp-scoped.)
    #[test]
    fn non_acp_tombstone_still_cold_wins_after_acp_fix() {
        let mut inputs = base_inputs();
        inputs.transcripts = vec![meta("cld-S", "-w-projY", 5_000)];
        inputs.stats_for = [(
            PathBuf::from("/projects/-w-projY/cld-S.jsonl"),
            JsonlStats { name: Some("cold-name".into()), user_named: true, ..Default::default() },
        )]
        .into_iter()
        .collect();
        inputs.tombstoned = vec![TombstonedEntry {
            path: PathBuf::from("/sessions/8.json.tombstoned"),
            pid: 8,
            data: RegistryEntry {
                pid: Some(8),
                session_id: Some("cld-S".to_string()),
                name: Some("dead-name".to_string()),
                provider: Some("claude-code".to_string()), // NON-acp → cold-wins preserved
                ..Default::default()
            },
            mtime_ms: 2000,
            degraded: Vec::new(),
        }];
        let opts = JoinOpts { include_all: true, include_tombstoned: true, ..Default::default() };
        let out = join_sessions(&inputs, opts);
        let r = out.iter().find(|s| s.session_id == "cld-S").unwrap();
        assert_eq!(r.which_branch, SessionBranch::ColdJsonl, "claude cold still wins (unchanged)");
        assert_eq!(r.name.as_deref(), Some("cold-name"));
    }

    // --- sort order ---

    #[test]
    fn sorts_by_last_active_desc() {
        let mut inputs = base_inputs();
        inputs.registry = vec![
            live(1, "a", Some("a"), 1_000),
            live(2, "b", Some("b"), 9_000),
            live(3, "c", Some("c"), 5_000),
        ];
        let out = join_sessions(&inputs, JoinOpts::default());
        let ids: Vec<&str> = out.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["b", "c", "a"], "lastActive descending");
    }

    // --- apply_list_cap matrix ---

    fn named_session(sid: &str, named: bool, status: SessionStatus, last: i64) -> Session {
        Session {
            name: Some(sid.to_string()),
            user_named: Some(named),
            session_id: sid.to_string(),
            code: None,
            qd_id: None,
            pid: Some(1),
            status,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: Some(last),
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

    #[test]
    fn cap_default_filters_named_non_killed_and_caps_20() {
        let mut v: Vec<Session> = (0..25)
            .map(|i| named_session(&format!("s{i}"), true, SessionStatus::Idle, i))
            .collect();
        // Add an unnamed + a killed; both must be filtered out in the default view.
        v.push(named_session("unnamed", false, SessionStatus::Idle, 100));
        v.push(named_session("killed", true, SessionStatus::Killed, 100));
        let out = apply_list_cap(v, false, None);
        assert_eq!(out.len(), 20, "default caps at 20 after filtering");
        assert!(out.iter().all(|s| s.user_named == Some(true)));
        assert!(out.iter().all(|s| s.status != SessionStatus::Killed));
    }

    #[test]
    fn cap_all_uncapped() {
        let v: Vec<Session> = (0..50)
            .map(|i| named_session(&format!("s{i}"), true, SessionStatus::Idle, i))
            .collect();
        let out = apply_list_cap(v, true, None);
        assert_eq!(out.len(), 50, "--all is uncapped");
    }

    #[test]
    fn cap_explicit_limit_wins_even_with_all() {
        let v: Vec<Session> = (0..50)
            .map(|i| named_session(&format!("s{i}"), true, SessionStatus::Idle, i))
            .collect();
        let out = apply_list_cap(v, true, Some(5));
        assert_eq!(out.len(), 5, "explicit -n caps even with --all");
    }

    #[test]
    fn cap_zero_and_negative_treated_unset() {
        // -n 0 and -n -3 must NOT empty the view (treated as unset).
        let v: Vec<Session> = (0..30)
            .map(|i| named_session(&format!("s{i}"), true, SessionStatus::Idle, i))
            .collect();
        let out_zero = apply_list_cap(v.clone(), false, Some(0));
        assert_eq!(out_zero.len(), 20, "limit 0 → unset → default cap 20");
        let out_neg = apply_list_cap(v, true, Some(-3));
        assert_eq!(out_neg.len(), 30, "negative → unset → --all uncapped");
    }

    #[test]
    fn cap_all_does_not_filter_unnamed_or_killed() {
        let v = vec![
            named_session("unnamed", false, SessionStatus::Idle, 1),
            named_session("killed", true, SessionStatus::Killed, 2),
        ];
        let out = apply_list_cap(v, true, None);
        assert_eq!(out.len(), 2, "--all shows unnamed + killed");
    }

    /// B5 item 2-D: the counted variant reports exactly how many ELIGIBLE rows
    /// the active cap dropped — total-eligible − shown, AFTER the default
    /// view's named/non-killed filter (filtered-out rows are not "more").
    #[test]
    fn cap_counted_reports_dropped_eligible_rows() {
        let mut v: Vec<Session> = (0..25)
            .map(|i| named_session(&format!("s{i}"), true, SessionStatus::Idle, i))
            .collect();
        // Ineligible rows (unnamed / killed) must NOT count toward "dropped".
        v.push(named_session("unnamed", false, SessionStatus::Idle, 100));
        v.push(named_session("killed", true, SessionStatus::Killed, 100));
        let (out, dropped) = apply_list_cap_counted(v, false, None);
        assert_eq!(out.len(), 20);
        assert_eq!(dropped, 5, "25 eligible − 20 shown");
    }

    /// B5 item 2-D: uncapped views (--all, and at/under-cap defaults) report 0;
    /// an explicit limit reports its own drop count (the VERB decides the
    /// trailer only fires for the default cap).
    #[test]
    fn cap_counted_zero_when_uncapped_or_under_cap() {
        let v: Vec<Session> = (0..50)
            .map(|i| named_session(&format!("s{i}"), true, SessionStatus::Idle, i))
            .collect();
        let (out, dropped) = apply_list_cap_counted(v.clone(), true, None);
        assert_eq!((out.len(), dropped), (50, 0), "--all uncapped → 0 dropped");
        let (out, dropped) = apply_list_cap_counted(v[..20].to_vec(), false, None);
        assert_eq!(
            (out.len(), dropped),
            (20, 0),
            "exactly-at-cap default → 0 dropped (no trailer at the boundary)"
        );
        let (out, dropped) = apply_list_cap_counted(v, false, Some(5));
        assert_eq!(
            (out.len(), dropped),
            (5, 45),
            "explicit -n reports its drop; the verb gates the trailer"
        );
    }

    // --- name truthiness (empty pid.name falls through to jsonl name) ---

    #[test]
    fn empty_registry_name_falls_through_to_jsonl_name() {
        let mut inputs = base_inputs();
        let mut e = live(7, "sid-x", Some(""), 5_000); // empty name
        e.entry.cwd = Some("/work/proj".to_string());
        inputs.registry = vec![e];
        let stats = JsonlStats {
            name: Some("from-jsonl".to_string()),
            user_named: true,
            ..Default::default()
        };
        let p = PathBuf::from("/projects/-work-proj/sid-x.jsonl");
        inputs.jsonl_path_for = [("sid-x".to_string(), p.clone())].into_iter().collect();
        inputs.stats_for = [(p, stats)].into_iter().collect();
        let out = join_sessions(&inputs, JoinOpts::default());
        let s = find(&out, "sid-x");
        assert_eq!(s.name.as_deref(), Some("from-jsonl"), "empty name is falsy");
        assert_eq!(s.user_named, Some(true), "jsonl userNamed carries");
    }

    // --- codex P1 W6: status derivation through the provider seam ---
    //
    // The LiveRegistry status derivation moved BEHIND `provider.parse_status`
    // (codex-p1-spec section 7.2), keyed per-row on the read-back provider value
    // (resolved ONCE, shared with the `provider` field). These pin ZERO behavior
    // change: every status string maps to the SAME variant it did pre-seam, for
    // each of: an explicit "claude-code" provider, an ABSENT provider field, AND
    // an UNKNOWN provider (render-survival — derives via the claude rules anyway).
    //
    // MUTATION EVIDENCE (CR-3): swapping the status parser (e.g. routing through
    // the FixtureDaemonProvider, which returns None for a registry STRING) or
    // losing the Idle fallback would red `unknown_status_string_falls_back_to_idle`
    // and the busy/shell assertions below.

    /// A live row with an explicit status string AND provider value.
    fn live_with(sid: &str, status: &str, provider: Option<&str>) -> ScannedEntry {
        let mut e = live(1234, sid, Some("w"), 5_000);
        e.entry.status = Some(status.to_string());
        e.entry.provider = provider.map(str::to_string);
        e
    }

    fn status_of(status_str: &str, provider: Option<&str>) -> SessionStatus {
        let mut inputs = base_inputs();
        inputs.registry = vec![live_with("sid-w6", status_str, provider)];
        let out = join_sessions(&inputs, JoinOpts::default());
        find(&out, "sid-w6").status
    }

    #[test]
    fn w6_explicit_and_absent_provider_derive_identical_status() {
        // For each registry status string, an explicit "claude-code" provider and
        // an ABSENT provider field derive the SAME variant — and the SAME variant
        // pre-seam join produced (idle/busy/shell are the three valid live states).
        for (raw, want) in [
            ("idle", SessionStatus::Idle),
            ("busy", SessionStatus::Busy),
            ("shell", SessionStatus::Shell),
        ] {
            assert_eq!(
                status_of(raw, Some("claude-code")),
                want,
                "explicit claude-code provider derives {raw} → {want:?}"
            );
            assert_eq!(
                status_of(raw, None),
                want,
                "absent provider (read-back default) derives {raw} → {want:?}"
            );
            assert_eq!(
                status_of(raw, Some("claude-code")),
                status_of(raw, None),
                "explicit and absent provider derive IDENTICAL status for {raw}"
            );
        }
    }

    #[test]
    fn w6_unknown_status_string_falls_back_to_idle() {
        // An unknown/empty status string can't be a typed variant → the JOIN's
        // Idle fallback fires (parse_status returns None; the caller picks Idle).
        // This is the war-story-carried fallback (UNREACHABLE for valid fixtures).
        assert_eq!(
            status_of("not-a-real-status", Some("claude-code")),
            SessionStatus::Idle
        );
        assert_eq!(status_of("", None), SessionStatus::Idle);
    }

    // --- codex P2 W5: live codex status derives from the pre-gathered rollout
    //     map, NOT the registry string parse_status path ---
    //
    // The codex live-row branch (codex-p2-spec sections 3.3, 7.4) reads
    // `codex_status_for` (the gather step pre-derived it off the rollout tail,
    // connectionless). A codex row absent from the map is Idle (fresh thread).
    //
    // MUTATION EVIDENCE (codex-p2-spec section 13 "rollout busy/idle anchor
    // inverted" / status path inversion): if the codex branch fell back to
    // `parse_status` (which returns None for a codex registry STRING → Idle), the
    // Busy assertion below would red. NAMED.

    fn codex_live(sid: &str, name: &str, updated: i64) -> ScannedEntry {
        let mut e = live(5000, sid, Some(name), updated);
        e.entry.provider = Some("codex".to_string());
        e.entry.endpoint = Some("ws://127.0.0.1:18951".to_string());
        // The registry status string is IGNORED for codex rows (no parse_status).
        e.entry.status = Some("idle".to_string());
        e
    }

    #[test]
    fn codex_live_row_status_comes_from_pregathered_rollout_map() {
        let mut inputs = base_inputs();
        inputs.registry = vec![codex_live("cdx-busy", "codex-a", 9_000)];
        // The gather step derived Busy from an open-turn rollout tail.
        inputs.codex_status_for = [("cdx-busy".to_string(), SessionStatus::Busy)]
            .into_iter()
            .collect();
        let out = join_sessions(&inputs, JoinOpts::default());
        let s = find(&out, "cdx-busy");
        assert_eq!(
            s.status,
            SessionStatus::Busy,
            "codex live status = the rollout-derived map value, NOT parse_status(\"idle\")"
        );
        assert_eq!(s.provider, "codex");
    }

    #[test]
    fn codex_live_row_absent_from_map_is_idle() {
        // A fresh codex thread (no rollout / no anchors) is absent from the map →
        // Idle (a just-created codex session is idle). NO socket opened.
        let mut inputs = base_inputs();
        inputs.registry = vec![codex_live("cdx-fresh", "codex-b", 8_000)];
        // codex_status_for is EMPTY (no rollout file yet).
        let out = join_sessions(&inputs, JoinOpts::default());
        assert_eq!(find(&out, "cdx-fresh").status, SessionStatus::Idle);
    }

    // --- B1: live pi status derives from the pre-gathered resident point-read
    //     map, NOT the registry string parse_status path (which pi rejects). ---
    //
    // MUTATION EVIDENCE: a pi registry status STRING fed to `parse_status` returns
    // None → Idle (pi's native signal is an isStreaming OBJECT, not a claude
    // string). So WITHOUT the `pi_status_for` branch a running pi turn would read
    // Idle in `ls`. These deterministic (no-socket) tests guard the join wiring;
    // the gather's live connect is RUN-not-read at the live-dogfood phase.

    fn pi_live(sid: &str, name: &str, updated: i64) -> ScannedEntry {
        let mut e = live(6000, sid, Some(name), updated);
        e.entry.provider = Some("pi".to_string());
        e.entry.endpoint = Some("ws://127.0.0.1:19100".to_string());
        // The registry status string is IGNORED for pi rows (no parse_status).
        e.entry.status = Some("idle".to_string());
        e
    }

    #[test]
    fn pi_live_row_status_comes_from_pregathered_is_streaming_map() {
        let mut inputs = base_inputs();
        inputs.registry = vec![pi_live("pi-busy", "pi-a", 9_000)];
        // The gather step read is_streaming:true off the resident → Busy.
        inputs.pi_status_for = [("pi-busy".to_string(), SessionStatus::Busy)]
            .into_iter()
            .collect();
        inputs.pi_turns_for = [("pi-busy".to_string(), 3u64)].into_iter().collect();
        let out = join_sessions(&inputs, JoinOpts::default());
        let s = find(&out, "pi-busy");
        assert_eq!(
            s.status,
            SessionStatus::Busy,
            "pi live status = the resident is_streaming point-read, NOT parse_status(\"idle\")"
        );
        assert_eq!(s.turns, 3, "pi turns = the resident's busy→idle-edge count");
        assert_eq!(s.provider, "pi");
    }

    #[test]
    fn pi_live_row_absent_from_map_is_idle() {
        // An unreachable / dead / mis-identified pi resident is absent from the map
        // → Idle (a cold/just-created pi session is idle). Turns fall back to the
        // transcript value (0 with no transcript here).
        let mut inputs = base_inputs();
        inputs.registry = vec![pi_live("pi-cold", "pi-b", 8_000)];
        // pi_status_for / pi_turns_for EMPTY (resident unreachable).
        let out = join_sessions(&inputs, JoinOpts::default());
        let s = find(&out, "pi-cold");
        assert_eq!(s.status, SessionStatus::Idle);
        assert_eq!(s.turns, 0);
    }

    #[test]
    fn codex_cold_rows_emit_as_cold_with_provider_codex() {
        // A discovered cold codex thread (no live row) emits a Cold row carrying
        // provider "codex", in the ColdJsonl render shape.
        let mut inputs = base_inputs();
        inputs.codex_cold = vec![CodexColdRow {
            id: "cold-thread-uuid".to_string(),
            name: Some("a codex title".to_string()),
            cwd: Some("/work/codexproj".to_string()),
            jsonl_path: Some("/codex/sessions/2026/06/07/rollout-x.jsonl".to_string()),
            last_active_ms: Some(7_000),
        }];
        let opts = JoinOpts {
            include_all: true,
            ..Default::default()
        };
        let out = join_sessions(&inputs, opts);
        let s = find(&out, "cold-thread-uuid");
        assert_eq!(s.status, SessionStatus::Cold);
        assert_eq!(s.provider, "codex");
        assert_eq!(s.which_branch, SessionBranch::ColdJsonl);
        assert_eq!(s.name.as_deref(), Some("a codex title"));
        assert_eq!(s.cwd.as_deref(), Some("/work/codexproj"));
        assert_eq!(s.user_named, Some(true), "a present title is named");
        assert_eq!(s.pid, None);
        assert_eq!(s.zmx_name, None);
    }

    #[test]
    fn codex_cold_row_skipped_when_id_seen_live() {
        // A cold codex row whose id ALSO has a live codex registry row is dropped
        // by the seen-guard (live wins — the gather joins live-wins, the join's
        // seen_session_ids is the belt). Only the live row survives.
        let mut inputs = base_inputs();
        inputs.registry = vec![codex_live("shared-uuid", "live-codex", 9_000)];
        inputs.codex_status_for = [("shared-uuid".to_string(), SessionStatus::Busy)]
            .into_iter()
            .collect();
        inputs.codex_cold = vec![CodexColdRow {
            id: "shared-uuid".to_string(),
            name: Some("stale title".to_string()),
            cwd: None,
            jsonl_path: None,
            last_active_ms: Some(1_000),
        }];
        let opts = JoinOpts {
            include_all: true,
            ..Default::default()
        };
        let out = join_sessions(&inputs, opts);
        let rows: Vec<&Session> = out
            .iter()
            .filter(|s| s.session_id == "shared-uuid")
            .collect();
        assert_eq!(rows.len(), 1, "live wins; cold duplicate dropped");
        assert_eq!(rows[0].status, SessionStatus::Busy, "the LIVE row survives");
    }

    #[test]
    fn codex_cold_nameless_row_is_unnamed() {
        // A rollout-only cold codex row (no sqlite title) has no name → userNamed
        // false → filtered from the default view (visible only with --all).
        let mut inputs = base_inputs();
        inputs.codex_cold = vec![CodexColdRow {
            id: "nameless-uuid".to_string(),
            name: None,
            cwd: Some("/w".to_string()),
            jsonl_path: Some("/r/x.jsonl".to_string()),
            last_active_ms: Some(5_000),
        }];
        let opts = JoinOpts {
            include_all: true,
            ..Default::default()
        };
        let out = join_sessions(&inputs, opts);
        let s = find(&out, "nameless-uuid");
        assert_eq!(s.name, None);
        assert_eq!(s.user_named, Some(false));
    }

    #[test]
    fn w6_unknown_provider_row_still_derives_status_via_claude_rules() {
        // An UNKNOWN-provider row (provider_for → None) STILL derives status via
        // the claude rules — render-survival (L8): `ls` must show the row; ACTING
        // verbs refuse via the W1 arming. This is a render-survival derivation,
        // NOT a dispatch endorsement of "weird-prov".
        assert_eq!(status_of("busy", Some("weird-prov")), SessionStatus::Busy);
        assert_eq!(status_of("idle", Some("weird-prov")), SessionStatus::Idle);
        assert_eq!(status_of("shell", Some("weird-prov")), SessionStatus::Shell);
        // And the row SURVIVES carrying the unknown provider value verbatim.
        let mut inputs = base_inputs();
        inputs.registry = vec![live_with("sid-weird", "busy", Some("weird-prov"))];
        let out = join_sessions(&inputs, JoinOpts::default());
        assert_eq!(find(&out, "sid-weird").provider, "weird-prov");
    }
}
