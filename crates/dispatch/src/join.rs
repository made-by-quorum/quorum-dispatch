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
use std::path::PathBuf;

use crate::jsonl::TranscriptMeta;
use crate::model::{Session, SessionBranch, SessionStatus};
use crate::mux::MuxSession;
use crate::registry;
use crate::relay::match_by_ancestry;
use crate::stray::{self, Stray};
use crate::{codes, resolve};

/// The backend-selected socket-dir set the gather scans (C1 D2 item 2).
///
/// **Defined in `quorum_qw::mux_selector` now**, beside the [`Backend`] it is keyed
/// by, because the lane layer needs the same answer:
/// `quorum_qw::lanes::row_for_id` must join the mux over the dirs the SELECTED
/// backend uses, and qw may not reach into qd to ask. Re-exported here so every
/// existing `join::MuxDirs` path keeps resolving — a relocation, not an API change
/// (the same move the eight modules above it took).
pub use crate::mux_selector::MuxDirs;

/// The gather half of this module now lives in [`quorum_qw::gather`] — every
/// effectful read `qd ls` does, plus the [`JoinInputs`] carrier it fills and the
/// [`JoinOpts`] that is in the gather's signature. What stays here is the MERGE:
/// `join_sessions_counted` and the six cold-row blocks, whose precedence rule is
/// qd's ruling (`08-session-merge-policy.md`).
///
/// RE-EXPORTED so every existing `join::gather` / `join::JoinInputs` /
/// `join::JoinOpts` path keeps resolving — a relocation, not an API change (the
/// same move `MuxDirs` and the cold-row carriers above took).
pub use quorum_qw::gather::{gather, gather_with_dirs, JoinInputs, JoinOpts};

// The two COLD-row carriers the codex/OpenCode gathers build now live with the
// gathers themselves, in `quorum_qw::provider_gather`. RE-EXPORTED here because
// they are part of [`JoinInputs`]'s shape, which is qd's own surface: every
// `join::CodexColdRow` path — in this module's tests and in any consumer — keeps
// resolving.
pub use crate::provider_gather::{CodexColdRow, OpencodeColdRow};

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
    // === THE MERGE POLICY — read this before reordering anything below ===
    //
    // Seven sources feed one list. When two of them claim the same session id the
    // winner is decided by `seen_session_ids` plus THE ORDER THESE BLOCKS RUN IN:
    // first writer wins. The blocks are visually independent `for` loops appending
    // to one `Vec`, which reads as commutative and is NOT. The ruled policy
    // (doc/tbd/provider-architecture/08-session-merge-policy.md, owner: qd):
    //
    //   1. PRECEDENCE: live > tombstoned > cold. A tombstone is a deliberate
    //      record that qd killed this session; a cold transcript is merely a file
    //      on disk. See the DEVIATION below — the code does not do this uniformly.
    //   2. WHOLE ROW, no field-level merge. The winning source supplies every
    //      field; a losing row contributes nothing. (A branch may still compose
    //      its row from its OWN pre-gathered inputs — `jsonl_path_for`/`stats_for`
    //      — which is not a cross-source merge.)
    //   3. An EMPTY session id never participates in id-keyed dedup: the ZmxOnly
    //      branch neither reads nor writes `seen_session_ids`, keying on pane name
    //      instead. (Known hole: the live and tombstone branches DO key an id-less
    //      row under `""` — see the doc's rule-3 section.)
    //   4. Cold-source ordering among providers is a non-question — claude/codex/pi
    //      mint their own uuids under disjoint roots and opencode ids are `ses_*`,
    //      so two cold sources cannot normally claim one id. The ONE exception is
    //      the ACP-CC bridge, which writes CLAUDE-shaped JSONL; `acp_tombstone_sids`
    //      below is the entire cross-provider collision handler.
    //
    // DEVIATION (deliberate, pinned, not to be "fixed" in passing): for claude and
    // codex the cold-JSONL block runs BEFORE the tombstone block and keeps the id,
    // so those two are cold > tombstoned — the reverse of rule 1. Only `acp/*` gets
    // rule 1. Flipping it is a user-visible `qd ls` change that reds
    // `tombstone_seen_guard_skips_already_cold`.
    //
    // Guards: `tests/session_merge_policy.rs` (named per-rule assertions + the
    // `ls-merge-policy.json` golden) and the unit tests at the bottom of this file.
    //
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
    // ASKED OF THE LANE. This was `p.starts_with("acp/")`, which stopped being a
    // test for anything the moment ACP became a lane: a new ACP row's provider is
    // `claude-code` — the same string its plain twin carries — so the prefix
    // answers `false` for exactly the rows this key exists to keep apart, and the
    // ACP row would be collapsed into its twin and shadowed out of every
    // join-derived surface. Silently, and only for sessions created after the
    // remodel.
    let provider_class_is_acp = |e: &registry::RegistryEntry| -> bool {
        e.provider.as_deref().is_some_and(|p| {
            quorum_qw::row_is_acp(p, e.hosting.as_deref())
        })
    };
    // codex-interactive: an UNIDENTIFIED row (no sessionId yet) must key on its
    // PID, not on the empty string.
    //
    // The collapse above is for one-session-many-process-generations, keyed on the
    // identity they share. Rows with NO identity share nothing — but they would all
    // key under `""` and keep-newest would collapse them into one, so a second
    // interactive codex session waiting for its first input would silently
    // disappear from `qd ls` (and from every join-derived surface) until it bound
    // an id. Keying an id-less row by pid keeps each one distinct while leaving the
    // identified path byte-for-byte unchanged.
    //
    // INERT for every pre-existing row: claude writes its own sessionId and the
    // daemon lanes get one from `thread/start`, so an id-less registry row does not
    // arise outside this lane. (A row with neither sessionId nor pid is degenerate;
    // it falls back to `""` exactly as before.)
    let dedupe_id = |e: &registry::RegistryEntry| -> String {
        match e.session_id.as_deref().filter(|s| !s.is_empty()) {
            Some(sid) => sid.to_string(),
            None => match e.pid {
                Some(pid) => format!("\u{0}unidentified-pid:{pid}"),
                None => String::new(),
            },
        }
    };
    for scanned in &inputs.registry {
        let e = &scanned.entry;
        let key = (dedupe_id(e), provider_class_is_acp(e));
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
            // codex-interactive: carry the registry `hosting` onto the row (the
            // same read-back shape as `provider`/`entrypoint`) so attach/kill/
            // send/resume can tell a pane-hosted codex session from a
            // daemon-hosted one. Only the LiveRegistry + Tombstoned branches have
            // a registry row to source it; the rest carry `None`, which
            // `provider::row_hosting` reads as "the provider's structural
            // hosting" — the pre-codex-interactive answer for every row.
            hosting: p.hosting.clone(),
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
                t.data.provider.as_deref().is_some_and(|p| {
                    quorum_qw::row_is_acp(p, t.data.hosting.as_deref())
                })
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
            hosting: None,
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
            hosting: None,
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
                // codex-interactive: a STOPPED session keeps its topology — this
                // is the branch that makes the field worth persisting at all.
                // `qd resume` on a tombstoned codex row must revive it into the
                // lane it was born in, and by then every live proxy (pane, pid,
                // socket dir) is long gone; only the captured tombstone data
                // still knows.
                hosting: t.data.hosting.clone(),
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
            hosting: None,
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
        let stats = inputs
            .stats_for
            .get(&meta.path)
            .cloned()
            .unwrap_or_default();
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
            hosting: None,
            which_branch: SessionBranch::ColdJsonl,
        });
    }

    // --- COLD OpenCode rows (lsview A3 — the ColdJsonl analog for the OpenCode
    //     provider, the way the codex/pi cold branches above are for theirs). An
    //     ADDITIVE step: sessions read from the monolithic `opencode.db`
    //     (`gather_opencode`), already ordered; here we only drop any id already
    //     seen as live/cold/tombstoned (the SAME seen-guard the other cold branches
    //     use). Unlike pi, the stats are PRE-DERIVED on the row (R1's ONE enumerate
    //     query — tokens are aggregated columns, turns a `message` count), so
    //     nothing is read via `stats_for` and NO A1 transcript-read counter is
    //     touched (an OpenCode SQL read is not a transcript read). Emitted as `Cold`
    //     ColdJsonl-shaped rows carrying provider "opencode", with NO zmx name-merge
    //     (OpenCode is daemon-/process-hosted, not a mux pane) and NO per-session
    //     jsonl file (the db is not a per-session transcript → jsonlPath None).
    //     EMPTY for every non-opencode fleet → byte-stable. ---
    for cold in &inputs.opencode_cold {
        if seen_session_ids.contains(&cold.id) {
            continue;
        }
        seen_session_ids.insert(cold.id.clone());
        let name = nonempty(cold.name.clone());
        sessions.push(Session {
            // A present title/slug is a derived label (OpenCode always writes one),
            // not necessarily human-chosen — treat present as named so it surfaces
            // in the default view (the codex cold-row posture).
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
            turns: cold.turns,
            tokens: cold.tokens,
            cwd: nonempty(cold.cwd.clone()),
            last_active_ms: cold.last_active_ms,
            // Cold rows do not serialize `version`/`startedAt` in the ColdJsonl
            // JSON (render.rs:151-170, TS session.ts:960-977), and codex/pi cold
            // rows carry both None — OpenCode matches for cross-provider cold-row
            // uniformity (neither is a rendered surface for a cold row here).
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            // lsview A3: the OpenCode provider value — a cold opencode row is the
            // opencode lane by construction (read from the opencode store). ACTING
            // verbs route it through the opencode provider dispatch, NOT the
            // unknown-provider refusal (the pi/codex carried-constraint posture).
            provider: "opencode".to_string(),
            entrypoint: None,
            lineage: None,
            hosting: None,
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
    use crate::jsonl::JsonlStats;
    use crate::mux::MuxSession;
    use crate::registry::{RegistryEntry, ScannedEntry, TombstonedEntry};

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
                hosting: None,
                harness_endpoint: None,
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

    // === codex-interactive: UNIDENTIFIED rows (no sessionId yet) ===
    //
    // An `--interactive` codex row exists before its thread does — codex discloses
    // no id until the user types. These pin that such a row survives the join
    // intact, and that two of them stay DISTINCT (the dedupe keys on sessionId,
    // and id-less rows would otherwise all collapse under "").

    /// A pane-hosted codex row that has not bound a thread id yet.
    fn unidentified_codex(pid: i64, name: &str, updated: i64) -> ScannedEntry {
        let mut e = live(pid, "", Some(name), updated);
        e.entry.session_id = None;
        e.entry.provider = Some("codex".to_string());
        e.entry.hosting = Some("mux-pane".to_string());
        e
    }

    #[test]
    fn an_unidentified_row_survives_the_join() {
        let mut inputs = base_inputs();
        inputs.registry = vec![unidentified_codex(100, "cx1", 5_000)];
        let out = join_sessions(&inputs, JoinOpts::default());
        assert_eq!(
            out.len(),
            1,
            "the row must not be dropped for lacking an id"
        );
        assert_eq!(out[0].name.as_deref(), Some("cx1"));
        assert_eq!(
            out[0].session_id, "",
            "no id yet — and that is the honest value"
        );
        assert_eq!(out[0].provider, "codex");
        assert_eq!(out[0].hosting.as_deref(), Some("mux-pane"));
    }

    /// MUTATION EVIDENCE: revert the dedupe key to the bare sessionId and this
    /// reds — both rows key under "" and keep-newest silently drops one, so a
    /// second interactive codex session vanishes from every join-derived surface
    /// until it happens to bind an id.
    #[test]
    fn two_unidentified_rows_stay_distinct() {
        let mut inputs = base_inputs();
        inputs.registry = vec![
            unidentified_codex(100, "cx1", 5_000),
            unidentified_codex(200, "cx2", 9_000),
        ];
        let out = join_sessions(&inputs, JoinOpts::default());
        assert_eq!(out.len(), 2, "two id-less sessions are two sessions");
        let mut names: Vec<&str> = out.iter().filter_map(|s| s.name.as_deref()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["cx1", "cx2"]);
    }

    /// The identified path is untouched: two rows that genuinely SHARE an id still
    /// collapse to the newest, exactly as before.
    #[test]
    fn rows_sharing_a_real_id_still_collapse_to_the_newest() {
        let mut inputs = base_inputs();
        inputs.registry = vec![
            live(100, "same-uuid", Some("old"), 1_000),
            live(200, "same-uuid", Some("new"), 9_000),
        ];
        let out = join_sessions(&inputs, JoinOpts::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name.as_deref(), Some("new"));
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
            live_with_provider(
                100,
                "shared-sid",
                Some("base"),
                3_000,
                "acp/claude-code",
                Some("pty"),
            ),
            // The live plain twin (the Child-B-era companion shape): plain
            // claude-code, heartbeat keeps it newest.
            live_with_provider(
                200,
                "shared-sid",
                Some("base-floor"),
                9_000,
                "claude-code",
                None,
            ),
        ];
        let out = join_sessions(&inputs, JoinOpts::default());
        let rows: Vec<&Session> = out
            .iter()
            .filter(|s| s.session_id == "shared-sid")
            .collect();
        assert_eq!(
            rows.len(),
            2,
            "the acp original and its plain twin must BOTH survive the dedup \
             (cross-provider-class rows are two legitimate sessions, not one stale pair): {:?}",
            out.iter()
                .map(|s| (&s.name, &s.provider))
                .collect::<Vec<_>>()
        );
        let acp = rows
            .iter()
            .find(|s| s.provider == "acp/claude-code")
            .expect("acp row emitted");
        let floor = rows
            .iter()
            .find(|s| s.provider == "claude-code")
            .expect("companion emitted");
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
        assert_eq!(
            dups[0].pid,
            Some(200),
            "keep-newest still wins within the class"
        );
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
        inputs.stats_for = [(PathBuf::from("/projects/--work-pi--/pi-sid-1.jsonl"), stats)]
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
        assert_eq!(
            row.zmx_name, None,
            "pi is daemon-hosted — no zmx name-merge"
        );
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
            JsonlStats {
                name: Some("cc".into()),
                user_named: true,
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();
        assert!(inputs.pi_cold.is_empty());

        let out = join_sessions(&inputs, JoinOpts::default());
        assert_eq!(out.len(), 1, "empty pi_cold adds no rows");
        assert_eq!(
            out[0].provider, "claude-code",
            "the existing row is untouched"
        );
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
            JoinOpts {
                include_all: true,
                ..Default::default()
            },
        );
        let rows: Vec<&Session> = out.iter().filter(|s| s.session_id == "dup-sid").collect();
        assert_eq!(
            rows.len(),
            1,
            "one row for the shared id (live wins, no cold dup)"
        );
        assert_eq!(rows[0].which_branch, SessionBranch::LiveRegistry);
        assert_ne!(
            rows[0].provider, "pi",
            "the live row is not overwritten by a cold pi row"
        );
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
        inputs.jsonl_path_for = [(
            "acp-S".to_string(),
            PathBuf::from("/projects/-w-projX/acp-S.jsonl"),
        )]
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
        assert_eq!(
            rows.len(),
            1,
            "exactly one row for the acp sessionId (no dup)"
        );
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
            JsonlStats {
                name: Some("cold-name".into()),
                user_named: true,
                ..Default::default()
            },
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
        let opts = JoinOpts {
            include_all: true,
            include_tombstoned: true,
            ..Default::default()
        };
        let out = join_sessions(&inputs, opts);
        let r = out.iter().find(|s| s.session_id == "cld-S").unwrap();
        assert_eq!(
            r.which_branch,
            SessionBranch::ColdJsonl,
            "claude cold still wins (unchanged)"
        );
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
            hosting: None,
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
