//! `qd kill` decision core (spec §5.1; TS `commands/lifecycle.ts:532-789`).
//!
//! The Claude/zmx path is a **dual-reap**: never bail "nothing to kill" while a
//! known PID is alive. The decider here is split into PURE pieces that take
//! plain data (the resolved zmx list, raw cross-dir list, a process-ancestry
//! map) and decide the reap *plan*; the bin verb (`bin/dispatch/verbs/kill.rs`) drives
//! the live effects (zmx kill, kill_pid, ensure_tombstone, env-file cleanup,
//! post-verify scan) around that plan.
//!
//! # The war stories this carries (comments-carry, L8/L10)
//!
//! - **Bug A / I4** (`lifecycle.ts:584-589`): zmx kill ALONE is not enough — a
//!   self-killing session or a socket-dir split can leave `zmxName` unresolved,
//!   so we ALSO reap the claude PID directly and only bail if we have neither.
//! - **Red-team #1** (`lifecycle.ts:600-617`): the session's zmx socket may live
//!   in an UNSCANNED TMPDIR dir, so `byName` won't resolve it — but the owning
//!   `zmx run <name>` wrapper is always a real ANCESTOR of the claude PID. Walk
//!   ancestry to find it (`find_zmx_wrapper_for_pid`, effects.rs) so the zmx side
//!   is reaped instead of leaked. We mark the socket dir UNCONFIRMED so success
//!   reporting stays honest (we cannot `zmx kill` a dir we don't know).
//! - **Red-team #2 / ended-task** (`lifecycle.ts:619-641`): when the claude PID
//!   is already dead (out-of-band `kill -9`/OOM, no graceful self-removal), the
//!   zmx task is `ended` — filtered out of the live list, and ancestry from the
//!   dead PID finds nothing. But the ended task's own `pid` field IS the live
//!   `zmx run` wrapper PID, which keeps running and leaks. Look it up by name in
//!   the RAW cross-dir list (includes ended tasks) and reap that wrapper.
//! - **F2/C2 post-verify** (`lifecycle.ts:751-784`): after a no-failures reap, do
//!   ONE presence scan for the session name across all dirs. A survivor means the
//!   "success" would be a silent lie (e.g. a stale zmxName targeted the wrong
//!   name) — exit 1 with an advisory. The hint uses `zmx ls`, NEVER `zmx kill`,
//!   so we never direct the operator to kill an innocent same-named session.

use crate::mux::MuxSession;

/// The resolved zmx-reap target the gather phase hands the bin verb.
///
/// Mirrors the TS locals (`zmxName`, `zmxDir`, `wrapperPid`, `zmxDirUnconfirmed`)
/// after the three-fallback chain at `lifecycle.ts:592-641`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ZmxReapTarget {
    /// The resolved zmx session name, if any (`haveZmx` is `name.is_some()`).
    pub zmx_name: Option<String>,
    /// The socket dir to pin `zmx kill` to (when confirmed).
    pub zmx_dir: Option<String>,
    /// A wrapper PID to reap directly (ancestry / ended-task fallback). 0 = none.
    pub wrapper_pid: i32,
    /// The socket dir is not among the scanned dirs (red-team #1): we cannot pin
    /// `zmx kill` to it, so we reap the wrapper PID and rely on zmx's dead-PTY
    /// cleanup, and success reporting additionally requires the wrapper to die.
    pub zmx_dir_unconfirmed: bool,
}

/// PURE: resolve the zmx-reap target from the three-fallback chain
/// (`lifecycle.ts:592-641`), given:
///
/// - `resolved_zmx_name`: the `session.zmxName` the join produced (may be `None`).
/// - `session_name`: the session's display name (the by-name lookup key).
/// - `pid`: the resolved claude PID (`0` when unknown).
/// - `canonical_dir`: the default socket dir.
/// - `live_list`: the FILTERED cross-dir zmx list (no ended/unreachable).
/// - `raw_list`: the RAW cross-dir list (INCLUDES ended/unreachable tasks).
/// - `wrapper`: the ancestry-walk result for `pid` (`find_zmx_wrapper_for_pid`),
///   pre-computed by the caller (it owns the `ProcessTable`).
/// - `owner_chain`: the addressed pid's ancestor chain (`effects::ancestor_chain`
///   — `[pid, parent, …]`), the process-truth ownership witness. Empty when
///   pid is 0 or `ps` failed.
/// - `is_alive`: pid-liveness probe, injected as data so the decider stays pure
///   (the `reconcile::plan` pattern).
///
/// # Red-team r7 F1 (lead-2-adjudicated): kill only CONFIRMED targets
///
/// A name match alone is NOT identification: a stale row's name byte-matches a
/// recreated LIVE session's pane (registry row name and pane name are different
/// sources unless the addressed session owns the pane). The invariant, extended
/// from r6's never-kill-an-alive-pane: **never destroy a pane that isn't
/// positively identified as the addressed session's own (process truth) or
/// dead.** Concretely, fallback 1 accepts a name-matched pane only when:
///
/// - **(a)** `pane.pid == pid` — the addressed row's pid IS the pane process
///   (the join's cold-row adoption sets `row.pid` from the pane, and the
///   embedded mux reports the pty child directly; pid-pinned, so a pane
///   replaced since the join fails the check), or
/// - **(b)** `owner_chain.contains(pane.pid)` — the pane's process is an
///   ANCESTOR of the addressed pid: the zmx `run` wrapper, or the embedded
///   pty child / shell the session program runs under (NOTE for red-team:
///   clause (b) depends on the `ps`-derived chain — a `ps` failure degrades to
///   refusal + fallback 2/3, never to a name-trusting kill), or
/// - **(c)** the pane's tracked process is DEAD (`pid > 0` and not alive — the
///   stale leftover this verb exists to clean; `pid <= 0` is *unknown*, not
///   dead, and is never accepted).
///
/// When NOTHING is confirmed the target carries `zmx_name: None` — the verb
/// then performs NO pane kill at all. (Previously a join-resolved `zmx_name`
/// with no scanned-dir match fired `mux.kill` blindly at the canonical dir —
/// the same victim class as fallback 1, one step later.)
///
/// The chain, in order:
/// 1. by-name in the live list (`lifecycle.ts:597-606`), ownership-or-dead
///    gated as above → confirms name + dir.
/// 2. ancestry wrapper, whenever no pane is CONFIRMED yet AND pid>0 (red-team
///    #1, `:608-617`) → sets `wrapper_pid`, marks the dir UNCONFIRMED. (r7: the
///    gate widened from "name unresolved" to "nothing confirmed", so a refused
///    fallback-1 match still reaps the session's REAL wrapper.)
/// 3. dead-task by-name in the RAW list, only when no wrapper yet (red-team #2,
///    `:619-641`) → the dead task's tracked pid IS the live wrapper. r7/r8: only
///    NON-ATTACHABLE rows qualify (ended/unreachable/err — the exact live-filter
///    negation) — the raw list also contains LIVE tasks, which are the same
///    victim class as fallback 1. The recorded pid is mint-time data with a
///    real reuse window; the caller must cmdline-guard it
///    (`wrapper_kill_allowed`) before signaling.
///
/// r8 PRECONDITION (identity-first): every pid-derived input here — `pid` for
/// clause (a), `wrapper`, `owner_chain` — must come through
/// [`gate_pid_witnesses`]: a Registry-sourced pid that is alive but not the
/// session's program is FOREIGN and contributes NO witnesses (a reused pid's
/// ancestry/equality otherwise confirms a victim's pane).
#[allow(clippy::too_many_arguments)]
pub fn resolve_zmx_target(
    resolved_zmx_name: Option<&str>,
    session_name: Option<&str>,
    pid: i64,
    canonical_dir: &str,
    live_list: &[MuxSession],
    raw_list: &[MuxSession],
    wrapper: Option<(i32, String)>,
    owner_chain: &[i32],
    is_alive: &dyn Fn(i32) -> bool,
) -> ZmxReapTarget {
    let lookup_key = resolved_zmx_name.or(session_name).unwrap_or("");

    // 1. Name-based candidates in the live list (across all socket dirs),
    //    accepted only under the ownership-or-dead gate (r7 F1). Candidate
    //    match is CASE-FOLDED (r8 m3): names are case-insensitive for
    //    addressing (r4) and the ownership/dead gate now does the
    //    discrimination the old byte-exact rationale relied on — byte-exact
    //    here only leaked legacy case-variant own panes.
    if !lookup_key.is_empty() {
        for pane in live_list
            .iter()
            .filter(|z| z.name.eq_ignore_ascii_case(lookup_key))
        {
            let owned_by_row_pid = pid > 0 && i64::from(pane.pid) == pid;
            let owned_by_ancestry = pane.pid > 0 && owner_chain.contains(&pane.pid);
            let pane_dead = pane.pid > 0 && !is_alive(pane.pid);
            if owned_by_row_pid || owned_by_ancestry || pane_dead {
                return ZmxReapTarget {
                    zmx_name: Some(pane.name.clone()),
                    zmx_dir: Some(
                        pane.socket_dir
                            .clone()
                            .unwrap_or_else(|| canonical_dir.to_string()),
                    ),
                    wrapper_pid: 0,
                    zmx_dir_unconfirmed: false,
                };
            }
        }
    }

    // 2. Process-tree fallback (red-team #1): the wrapper is a real ancestor of
    //    the claude PID even when its socket lives in an unscanned dir.
    if pid > 0 {
        if let Some((wpid, wname)) = wrapper {
            return ZmxReapTarget {
                zmx_name: Some(wname),
                zmx_dir: Some(canonical_dir.to_string()),
                wrapper_pid: wpid,
                zmx_dir_unconfirmed: true, // dir not among scanned dirs
            };
        }
    }

    // 3. Dead-task fallback (red-team #2): the ended task's `pid` is the live
    //    `zmx run` wrapper. NON-ATTACHABLE rows only (r7 narrowed to ended;
    //    r8 m1 widened to the exact live-filter negation — ended OR
    //    unreachable OR err — because an unreachable row's leaked wrapper was
    //    otherwise never reaped): a live task is the same victim class as
    //    fallback 1 and must never be selected. The recorded pid is mint-time
    //    data with a real reuse window; the caller must cmdline-guard it
    //    (`wrapper_kill_allowed`) before signaling — THAT guard, not the
    //    candidate filter, is the kill-side identity.
    if !lookup_key.is_empty() {
        if let Some(ended) = raw_list.iter().find(|z| {
            z.name.eq_ignore_ascii_case(lookup_key) && z.pid > 0 && !crate::mux::is_attachable(z)
        }) {
            return ZmxReapTarget {
                zmx_name: Some(ended.name.clone()),
                zmx_dir: Some(
                    ended
                        .socket_dir
                        .clone()
                        .unwrap_or_else(|| canonical_dir.to_string()),
                ),
                wrapper_pid: ended.pid,
                zmx_dir_unconfirmed: false,
            };
        }
    }

    // Nothing confirmed: NO pane target — even when the join resolved a name.
    // The verb's pid-side reap (and tombstone) still proceeds.
    ZmxReapTarget {
        zmx_name: None,
        zmx_dir: Some(canonical_dir.to_string()),
        wrapper_pid: 0,
        zmx_dir_unconfirmed: false,
    }
}

/// PURE (red-team r8 M#1, lead-2-adjudicated): does this cmdline look like an
/// INVOCATION of the session's program? TOKEN-BASENAME matching, never a raw
/// substring — r8 falsified the substring form (`cmd.contains("claude")`): any
/// process under a `.claude` PATH (`node ~/.claude/plugins/x.js`,
/// `vim claude.md`) contained the bytes and was mis-identified as the session.
/// A token matches when the whole token equals `session_bin`, or its path
/// basename equals `basename(session_bin)` (`CLAUDE_BIN` overrides the
/// program in test/alt installs), or its basename contains `"claude"` AND
/// carries no file extension — programs are extensionless (`claude`,
/// `fake-claude`, `/usr/local/bin/claude`) while the r8-falsified strangers
/// are all content FILES (`claude.md`, `.claude/plugins/helper.js`,
/// `~/.claude/logs/x.log`). The contains-not-equals form matters because
/// `CLAUDE_BIN` is ambient env: a session launched under an override is
/// legitimately stopped from a shell WITHOUT it (live g_coldstart_n leg (d)),
/// and judging that live session foreign would strand its name and tombstone
/// it alive. This is the structural-shape lesson from `wrapper_kill_allowed`
/// (parse, don't substring).
///
/// Documented residual (accepted, punch-listed): a stranger whose cmdline has
/// an extensionless TOKEN whose basename contains `claude` (`man claude`, a
/// hypothetical `claude-monitor`) still matches — far narrower than the
/// substring class; a start-time-vs-row-`started_at` belt is the future
/// tightener.
pub fn cmdline_is_session_program(cmdline: &str, session_bin: &str) -> bool {
    let bin_base = session_bin.rsplit('/').next().unwrap_or("");
    cmdline.split_whitespace().any(|tok| {
        if !session_bin.is_empty() && tok == session_bin {
            return true;
        }
        let tok_base = tok.rsplit('/').next().unwrap_or("");
        if !bin_base.is_empty() && tok_base == bin_base {
            return true;
        }
        tok_base.contains("claude") && !tok_base.contains('.')
    })
}

/// PURE (red-team r7 OPEN-Q1 + r8 M#1, lead-2-adjudicated): a recorded claude
/// pid that is ALIVE but whose current cmdline is visibly NOT an invocation of
/// the session's program is a REUSED pid — `qd stop` must never signal it,
/// and (r8 M#2) must never derive an ownership witness from it; the caller
/// zeroes `owner_chain`/`wrapper`/the clause-(a) pid BEFORE resolution.
/// Applies to REGISTRY-derived pids only — a pane-derived pid (cold-row
/// adoption, ZmxOnly) is same-invocation pane truth and exempt (caller's
/// provenance check). `kill_codex`'s cmdline guard is the codex-lane
/// equivalent.
/// Clock slack for the start-time arm: the row writer stamps `startedAt`
/// within moments of its own process start (same machine, same clock); the
/// `etime`-derived probe has second resolution. A reused pid's occupant
/// started only after the ORIGINAL died, so a 2-minute window is generous
/// for the legit case and still requires death + reuse within 2 minutes of
/// boot to evade — documented residual.
pub const START_TIME_SLACK_MS: i64 = 120_000;

/// The predicate is TWO-ARMED — the pid is OURS when either holds:
/// - **cmdline arm** ([`cmdline_is_session_program`]): the cmdline looks
///   like an invocation of the session program. UNSOUND AGAINST `exec` (the
///   fake-claude harness `exec cat`s away; any program may) — kept as the
///   belt for rows without a usable `startedAt`.
/// - **start-time arm** (the sound one): the process holding the pid STARTED
///   no later than the row's `startedAt` (+[`START_TIME_SLACK_MS`]). Two
///   live processes never share a pid, so a process that held this pid when
///   the row was written IS the row's writer — identity that survives
///   `exec`, is program-agnostic, and reads portably (`ps -o etime=`).
///   A reused pid's occupant necessarily started AFTER the original died,
///   hence after `startedAt`.
///
/// NOTE (r8, investigated and rejected): an `QD_SESSION_ID` env arm — the
/// spec-§3 identity — would be exact, but macOS exposes another process's
/// environment only within the caller's own subtree (`ps -E` returns just
/// the command for arbitrary same-uid pids), so it is unreadable exactly
/// where the engine runs. The spec's trust model is env-ASSERTED by
/// cooperating writers, not env-PROBED at kill time.
///
/// A `None` cmdline (pid invisible to `ps`) is NOT foreign — the pid most
/// likely died in the probe window, and `kill_pid` on a dead pid is a no-op
/// `true`; refusing on `None` would break legit kills on a `ps` hiccup (and
/// the witnesses are naturally empty when `ps` failed).
pub fn pid_is_foreign(
    alive: bool,
    cmdline: Option<&str>,
    session_bin: &str,
    proc_start_ms: Option<i64>,
    row_started_at_ms: Option<i64>,
) -> bool {
    let started_ours = matches!(
        (proc_start_ms, row_started_at_ms),
        (Some(p), Some(r)) if p <= r + START_TIME_SLACK_MS
    );
    alive
        && !started_ours
        && matches!(cmdline, Some(cmd) if !cmdline_is_session_program(cmd, session_bin))
}

/// PURE (punch item 8, b3 adversarial concern 1): may the DESCENDANT SWEEP
/// root on this pid? This is STRICTER than the lone-pid kill gate and is used
/// ONLY for the sweep-root decision — the single-pid kill semantics
/// ([`pid_is_foreign`], the r7/r8 adjudication) are UNCHANGED.
///
/// THE GAP THIS CLOSES: `pid_is_foreign` returns NOT-foreign when the cmdline
/// is `None` (the adjudicated "pid probably died in the probe window" cell —
/// correct for a lone kill, which is a no-op on a dead pid). But a reused
/// registry pid now held by a STRANGER, whose cmdline read happened to fail,
/// would root a tree sweep over the STRANGER's children — and every per-victim
/// `(pid, start-time)` recheck passes because those victims genuinely ARE the
/// stranger's descendants. A single-pid residual becomes a SUBTREE residual.
///
/// So the sweep roots only on a POSITIVE witness that the root is ours:
/// - **cmdline matched-ours**: cmdline is `Some` AND parses as the session
///   program ([`cmdline_is_session_program`]); OR
/// - **confirmed started-ours**: both start times are readable AND the
///   process started no later than the row's `started_at` (+slack).
///
/// And a CONFIRMED start MISMATCH is a hard veto: when `proc_start_ms`
/// succeeds and is BEYOND the slack from `row_started_at_ms`, that is
/// positive evidence of NON-identity — no sweep, even if the cmdline read
/// failed (absent cmdline must not override a confirmed mismatch). When the
/// ONLY evidence is "alive but unidentifiable" (cmdline `None` AND start time
/// unreadable/unconfirmable) the sweep does NOT root — the lone root-kill
/// still runs, grandchildren leak (the accepted leak direction).
pub fn sweep_root_allowed(
    cmdline: Option<&str>,
    session_bin: &str,
    proc_start_ms: Option<i64>,
    row_started_at_ms: Option<i64>,
) -> bool {
    // Hard veto: a confirmed start-time mismatch proves non-identity.
    if let (Some(p), Some(r)) = (proc_start_ms, row_started_at_ms) {
        if p > r + START_TIME_SLACK_MS {
            return false;
        }
    }
    let cmdline_ours = matches!(cmdline, Some(cmd) if cmdline_is_session_program(cmd, session_bin));
    let started_ours = matches!(
        (proc_start_ms, row_started_at_ms),
        (Some(p), Some(r)) if p <= r + START_TIME_SLACK_MS
    );
    cmdline_ours || started_ours
}

/// Where a session row's recorded pid CAME FROM — the provenance that decides
/// whether the pid needs identity-proof before use (r8, lead-2-adjudicated).
///
/// - `Registry`: the pid was READ BACK from a persisted registry record
///   (`LiveRegistry` / `Tombstoned` join branches). It is a historical claim
///   with a real reuse window — it must pass the cmdline identity check
///   before ANY witness (ancestor chain, zmx wrapper, clause-(a) equality) is
///   derived from it.
/// - `PaneDerived`: the join took the pid from THIS invocation's pane scan
///   (`ColdJsonl` adoption / `ZmxOnly`). Same-scan pane truth — the pane IS
///   the source, so pid-vs-pane consistency is tautological and the foreign
///   gate does not apply (gating it would regress adopted-row stop; the Q2
///   disposition).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidProvenance {
    Registry,
    PaneDerived,
}

/// PURE (r8 M#2 + the fb2 sibling, lead-2-adjudicated): identity-first
/// witness gating. r8 proved a reused registry pid poisons the DERIVED
/// witnesses: its ancestor chain can reach a live victim's pane (clause
/// (b)) and its zmx-wrapper walk can land on the victim's wrapper (fallback
/// 2). So: a Registry-sourced pid that is ALIVE but fails the
/// session-program cmdline check is FOREIGN — the derived witnesses (chain,
/// wrapper) are zeroed BEFORE resolution.
///
/// The pid itself is NOT zeroed: clause (a) — `pane.pid == row.pid` — is
/// MUX-CORROBORATED current truth (the live mux reports that pane's child
/// pid right now; two independent sources agreeing), not a derivation from
/// the recorded pid. It survives the gate even when the cmdline cannot
/// identify the process — the session app's cmdline is unconstrained by the
/// registry contract (the fake-claude harness `exec cat`s; a custom
/// CLAUDE_BIN may be stopped from a shell without the override). Documented
/// residual: pid reuse landing EXACTLY on the pty child of a same-name pane
/// — a single-pid coincidence, vs. the dozens-wide descendant window the
/// chain gate closes.
///
/// Returns `(ownership_pid, wrapper, owner_chain, pid_foreign)` — the first
/// three are what `resolve_zmx_target` may consume; `pid_foreign` drives the
/// verb's signal-skip + tombstone-proceeds honest edge (re-check liveness at
/// signal time: a pane kill may already have reaped the process).
#[allow(clippy::too_many_arguments)]
pub fn gate_pid_witnesses(
    provenance: PidProvenance,
    pid: i64,
    alive: bool,
    cmdline: Option<&str>,
    session_bin: &str,
    proc_start_ms: Option<i64>,
    row_started_at_ms: Option<i64>,
    wrapper: Option<(i32, String)>,
    owner_chain: Vec<i32>,
) -> (i64, Option<(i32, String)>, Vec<i32>, bool) {
    let foreign = provenance == PidProvenance::Registry
        && pid > 0
        && pid_is_foreign(
            alive,
            cmdline,
            session_bin,
            proc_start_ms,
            row_started_at_ms,
        );
    if foreign {
        (pid, None, Vec::new(), true)
    } else {
        (pid, wrapper, owner_chain, false)
    }
}

/// PURE (r7 OPEN-Q1 sibling, fallback-3 belt): the ended-task `pid` was
/// recorded at task mint and may have been REUSED since. Only signal it when
/// its CURRENT cmdline parses as a zmx wrapper for this session name
/// (case-folded — names are case-insensitive for addressing, r4 rule).
/// `None` (invisible pid) → not allowed: the wrapper is already gone and there
/// is nothing to signal; skipping is the safe no-op.
pub fn wrapper_kill_allowed(cmdline: Option<&str>, zmx_name: &str) -> bool {
    match cmdline {
        None => false,
        Some(cmd) => crate::effects::match_zmx_wrapper_name(cmd)
            .is_some_and(|n| n.eq_ignore_ascii_case(zmx_name)),
    }
}

/// PURE: do we have anything to kill at all? (`lifecycle.ts:643-649`). If neither
/// a zmx target nor a live PID, the kill is a no-op error ("Nothing to kill").
pub fn have_kill_target(target: &ZmxReapTarget, pid: i64) -> bool {
    target.zmx_name.is_some() || pid > 0
}

/// PURE (punch item 8, b3-kill-spec): the descendant subtree of `root` in
/// BOTTOM-UP kill order (deepest first; pid-ascending within a depth for
/// determinism). `root` itself is NOT in the list — the verb's existing
/// per-pid ladder owns the root.
///
/// This is the DESCENDANT-TREE leg of the grandchild-survival fix (the
/// declared alternative to a launch-side process group): every victim is a
/// ppid-descendant of the already-identity-verified claude pid in a snapshot
/// taken while the tree was intact, and the caller additionally stamps each
/// with `(pid, start-time)` at snapshot and re-verifies before any signal
/// ([`crate::effects::kill_pid_tree`]) — per-victim positive identity, the
/// exec-proof pattern, rather than killpg's unenumerated group membership.
///
/// SELF-STOP GUARD (the L10 lesson carried from `kill_pid`'s PID-only
/// discipline): when `self_pid` — the running `qd stop` process — is inside
/// the subtree (a session stopping ITSELF), the sweep returns EMPTY. Sweeping
/// would SIGTERM qd and its shell mid-flight, breaking the stop it is
/// executing; self-stop keeps today's exact behavior (pane + claude pid reap
/// only). The grandchild gap stays open for that one shape — documented.
///
/// Termination is the visited set's alone (every pid is enqueued at most
/// once). The depth cap of 32 is a separate BLAST-RADIUS / cost bound, NOT a
/// termination need — and it is KEPT deliberately: removing it would widen
/// the kill list (a behavior change on a destructive path). Its one tension
/// with the loud-leak posture: a victim deeper than 32 levels is SILENTLY
/// dropped from the sweep (a real but pathological process tree; 32 levels
/// of nesting under one session is far outside any observed shape).
pub fn descendant_kill_list(
    root: i32,
    self_pid: i32,
    rows: &std::collections::HashMap<i32, crate::effects::ProcRow>,
) -> Vec<i32> {
    use std::collections::{HashMap, HashSet};
    if root <= 0 {
        return Vec::new();
    }
    // Child map from the ppid rows (self-parented / non-positive rows dropped).
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for (pid, row) in rows {
        if *pid > 0 && row.ppid > 0 && row.ppid != *pid {
            children.entry(row.ppid).or_default().push(*pid);
        }
    }
    let mut seen: HashSet<i32> = HashSet::new();
    seen.insert(root);
    let mut frontier = vec![root];
    let mut ordered: Vec<(u32, i32)> = Vec::new(); // (depth, pid)
    let mut depth: u32 = 0;
    while !frontier.is_empty() && depth < 32 {
        depth += 1;
        let mut next = Vec::new();
        for p in frontier {
            if let Some(kids) = children.get(&p) {
                for &c in kids {
                    if seen.insert(c) {
                        ordered.push((depth, c));
                        next.push(c);
                    }
                }
            }
        }
        frontier = next;
    }
    if root == self_pid || ordered.iter().any(|&(_, p)| p == self_pid) {
        return Vec::new(); // self-stop: never sweep toward the caller
    }
    ordered.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    ordered.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(name: &str, pid: i32, dir: &str) -> MuxSession {
        MuxSession {
            name: name.to_string(),
            pid,
            clients: 0,
            created: 0,
            start_dir: "/h".to_string(),
            cmd: "claude".to_string(),
            current: false,
            socket_dir: Some(dir.to_string()),
            ended: None,
            exit_code: None,
            zmx_status: None,
            err: None,
        }
    }

    fn z_ended(name: &str, pid: i32, dir: &str) -> MuxSession {
        let mut s = z(name, pid, dir);
        s.ended = Some(1_700_000_000);
        s
    }

    const CANON: &str = "/tmp/zmx-501";

    /// Everything dead — the pre-r7 tests ran against stale leftovers, which
    /// the ownership-or-dead gate accepts via clause (c).
    const ALL_DEAD: &dyn Fn(i32) -> bool = &|_| false;

    #[test]
    fn fallback1_by_name_in_live_list_resolves_dir() {
        // zmxName unresolved by the join, but a DEAD task exists by the session
        // name in a non-canonical dir → name + dir resolved from the list
        // (clause (c): dead pane).
        let live = vec![z("sess", 111, "/tmp/legacy/zmx-501")];
        let t = resolve_zmx_target(
            None,
            Some("sess"),
            222,
            CANON,
            &live,
            &[],
            None,
            &[],
            ALL_DEAD,
        );
        assert_eq!(t.zmx_name.as_deref(), Some("sess"));
        assert_eq!(t.zmx_dir.as_deref(), Some("/tmp/legacy/zmx-501"));
        assert_eq!(t.wrapper_pid, 0);
        assert!(!t.zmx_dir_unconfirmed);
    }

    #[test]
    fn fallback1_prefers_resolved_zmx_name_over_session_name() {
        let live = vec![z("zmx-actual", 111, CANON)];
        let t = resolve_zmx_target(
            Some("zmx-actual"),
            Some("display"),
            222,
            CANON,
            &live,
            &[],
            None,
            &[],
            ALL_DEAD,
        );
        assert_eq!(t.zmx_name.as_deref(), Some("zmx-actual"));
    }

    #[test]
    fn fallback2_ancestry_wrapper_marks_dir_unconfirmed() {
        // No name anywhere; pid>0 with an ancestry wrapper → wrapper_pid set,
        // dir UNCONFIRMED (red-team #1).
        let t = resolve_zmx_target(
            None,
            None,
            999,
            CANON,
            &[],
            &[],
            Some((555, "wrappedsess".to_string())),
            &[],
            ALL_DEAD,
        );
        assert_eq!(t.zmx_name.as_deref(), Some("wrappedsess"));
        assert_eq!(t.wrapper_pid, 555);
        assert!(
            t.zmx_dir_unconfirmed,
            "ancestry dir is unscanned → unconfirmed"
        );
    }

    #[test]
    fn fallback2_skipped_when_dead_pane_already_confirmed() {
        // A DEAD same-name pane is confirmed by fallback 1 (clause (c)) — the
        // ancestry witness is never consulted (it stays confirmed).
        let live = vec![z("sess", 111, CANON)];
        let t = resolve_zmx_target(
            Some("sess"),
            Some("sess"),
            999,
            CANON,
            &live,
            &[],
            Some((555, "wrappedsess".to_string())),
            &[],
            ALL_DEAD,
        );
        assert_eq!(t.zmx_name.as_deref(), Some("sess"));
        assert_eq!(t.wrapper_pid, 0);
        assert!(!t.zmx_dir_unconfirmed);
    }

    #[test]
    fn fallback3_ended_task_in_raw_list_reaps_tracked_wrapper_pid() {
        // claude PID dead, task ended (filtered from live), no ancestry. The
        // ended task's tracked pid IS the live wrapper → reap it directly.
        let raw = vec![z_ended("sess", 777, "/tmp/legacy/zmx-501")];
        let t = resolve_zmx_target(None, Some("sess"), 0, CANON, &[], &raw, None, &[], ALL_DEAD);
        assert_eq!(t.zmx_name.as_deref(), Some("sess"));
        assert_eq!(t.wrapper_pid, 777);
        assert_eq!(t.zmx_dir.as_deref(), Some("/tmp/legacy/zmx-501"));
        // NOT marked unconfirmed — the raw list gave us the real dir.
        assert!(!t.zmx_dir_unconfirmed);
    }

    #[test]
    fn fallback3_skipped_when_wrapper_already_found() {
        // An ancestry wrapper was found (fallback 2) → the ended-task lookup is
        // skipped (fallback 2 returns first).
        let raw = vec![z_ended("wrappedsess", 777, CANON)];
        let t = resolve_zmx_target(
            None,
            None,
            999,
            CANON,
            &[],
            &raw,
            Some((555, "wrappedsess".to_string())),
            &[],
            ALL_DEAD,
        );
        assert_eq!(
            t.wrapper_pid, 555,
            "ancestry wrapper kept, not the ended pid"
        );
    }

    #[test]
    fn no_target_when_no_name_and_no_pid() {
        let t = resolve_zmx_target(None, None, 0, CANON, &[], &[], None, &[], ALL_DEAD);
        assert_eq!(t.zmx_name, None);
        assert!(!have_kill_target(&t, 0));
    }

    #[test]
    fn have_target_with_pid_only() {
        let t = resolve_zmx_target(None, None, 0, CANON, &[], &[], None, &[], ALL_DEAD);
        // No zmx, but a live PID → still a target (dual-reap; Bug A).
        assert!(have_kill_target(&t, 4242));
    }

    #[test]
    fn ended_task_with_zero_pid_is_not_reapable() {
        // An ended task whose tracked pid is 0 (no wrapper) → no wrapper reaped.
        let raw = vec![z_ended("sess", 0, CANON)];
        let t = resolve_zmx_target(None, Some("sess"), 0, CANON, &[], &raw, None, &[], ALL_DEAD);
        assert_eq!(t.wrapper_pid, 0);
        // zmx_name also stays None — the `pid > 0` filter excluded the row.
        assert_eq!(t.zmx_name, None);
    }

    // ===== red-team r7 F1 pins (lead-2): kill only CONFIRMED targets =====

    #[test]
    fn r7_f1_stale_row_never_captures_live_victim_pane() {
        // THE r7 repro: stop a STALE row (dead pid 87654) whose name was
        // recreated — the live list holds the VICTIM's pane (pid 95689, ALIVE).
        // No ownership clause holds → NO pane target at all; the pid-side
        // tombstone path still proceeds (have_kill_target via pid).
        let live = vec![z("wk", 95689, CANON)];
        let alive = |p: i32| p == 95689;
        let t = resolve_zmx_target(
            None,
            Some("wk"),
            87654,
            CANON,
            &live,
            &[],
            None,
            // A genuinely-dead pid has no ps row, so the chain is bare [pid].
            // When the dead pid has been REUSED the chain is NOT bare — that
            // case is the r8 M#2 class, handled by gate_pid_witnesses (see
            // r8_m2_foreign_registry_pid_contributes_no_witnesses + the
            // resolution truth table).
            &[87654],
            &alive,
        );
        assert_eq!(t.zmx_name, None, "victim pane must NOT become the target");
        assert_eq!(t.wrapper_pid, 0);
        assert!(have_kill_target(&t, 87654), "pid-side reap still proceeds");
    }

    #[test]
    fn fallback1_clause_a_row_pid_is_pane_wrapper() {
        // Cold-row adoption shape (join sets row.pid = pane.pid): the addressed
        // row's pid IS the live pane wrapper → ownership established, confirmed.
        let live = vec![z("wk", 4242, CANON)];
        let alive = |p: i32| p == 4242;
        let t = resolve_zmx_target(
            Some("wk"),
            Some("wk"),
            4242,
            CANON,
            &live,
            &[],
            None,
            &[4242],
            &alive,
        );
        assert_eq!(t.zmx_name.as_deref(), Some("wk"));
        assert_eq!(t.wrapper_pid, 0);
        assert!(!t.zmx_dir_unconfirmed);
    }

    #[test]
    fn fallback1_clause_b_owner_chain_owns_pane() {
        // Legit live stop: the pane's pid is an ANCESTOR of the addressed
        // claude pid (zmx wrapper / embedded shell) → ownership established,
        // confirmed. The chain, not the name, is the identification.
        let live = vec![z("wk", 555, CANON)];
        let alive = |p: i32| p == 555 || p == 999;
        let t = resolve_zmx_target(
            Some("wk"),
            Some("wk"),
            999,
            CANON,
            &live,
            &[],
            Some((555, "wk".to_string())),
            &[999, 555], // claude 999 ← wrapper 555
            &alive,
        );
        assert_eq!(t.zmx_name.as_deref(), Some("wk"));
        assert_eq!(
            t.wrapper_pid, 0,
            "confirmed pane is mux-killed, not pid-killed"
        );
        assert!(!t.zmx_dir_unconfirmed);
    }

    #[test]
    fn fallback1_embedded_shell_parent_in_chain_owns_pane() {
        // Embedded-mux shape (g_coldstart_n regression): the pane pid is the
        // pty CHILD (qrmux SessionInfo.pid) — here a `bash -lc` shell whose
        // child is the session program (row pid 800). The shell 700 is in the
        // row pid's ancestor chain → owned, confirmed; the zmx wrapper witness
        // does not exist on this backend and must not be required.
        let live = vec![z("race", 700, CANON)];
        let alive = |p: i32| p == 700 || p == 800;
        let t = resolve_zmx_target(
            Some("race"),
            Some("race"),
            800,
            CANON,
            &live,
            &[],
            None, // no zmx wrapper on the embedded backend
            &[800, 700],
            &alive,
        );
        assert_eq!(t.zmx_name.as_deref(), Some("race"));
        assert!(!t.zmx_dir_unconfirmed);
    }

    #[test]
    fn refused_fallback1_still_reaps_real_wrapper_via_ancestry() {
        // Scope extension: our pane lives in an UNSCANNED dir; a same-name
        // VICTIM sits in the canonical dir. The victim fails every ownership
        // clause; the ancestry witness then reaps OUR real wrapper instead
        // (dir unconfirmed) — never the victim, never a blind canonical kill.
        let live = vec![z("wk", 95689, CANON)];
        let alive = |p: i32| p == 95689 || p == 999 || p == 555;
        let t = resolve_zmx_target(
            Some("wk"),
            Some("wk"),
            999,
            CANON,
            &live,
            &[],
            Some((555, "wk".to_string())),
            &[999, 555], // our wrapper 555 — the victim 95689 is NOT in the chain
            &alive,
        );
        assert_eq!(t.zmx_name.as_deref(), Some("wk"));
        assert_eq!(t.wrapper_pid, 555, "the ancestry wrapper, not the victim");
        assert!(t.zmx_dir_unconfirmed);
    }

    #[test]
    fn fallback3_ignores_live_tasks_in_raw_list() {
        // The raw list contains LIVE tasks too — a live same-name task is the
        // same victim class as fallback 1 and must never be selected.
        let raw = vec![z("wk", 95689, CANON)]; // not ended
        let alive = |p: i32| p == 95689;
        let t = resolve_zmx_target(
            None,
            Some("wk"),
            87654,
            CANON,
            &[],
            &raw,
            None,
            &[87654],
            &alive,
        );
        assert_eq!(t.zmx_name, None);
        assert_eq!(t.wrapper_pid, 0);
    }

    #[test]
    fn no_blind_canonical_kill_when_nothing_confirmed() {
        // Join-resolved zmx_name with NO scanned-dir match, no witness, no
        // ended task: the old code kept zmx_name and mux-killed blindly at the
        // canonical dir. Now: no confirmation → no pane target.
        let t = resolve_zmx_target(
            Some("wk"),
            Some("wk"),
            999,
            CANON,
            &[],
            &[],
            None,
            &[999],
            &|p| p == 999,
        );
        assert_eq!(t.zmx_name, None, "no confirmed pane → no pane kill at all");
        assert_eq!(t.wrapper_pid, 0);
    }

    // ===== r7 OPEN-Q1 pins: pid-reuse guards =====

    #[test]
    fn pid_is_foreign_flags_alive_non_session_cmdline() {
        assert!(pid_is_foreign(
            true,
            Some("/bin/bash -l"),
            "claude",
            None,
            None
        ));
        assert!(!pid_is_foreign(
            true,
            Some("node /usr/local/bin/claude --name wk"),
            "claude",
            None,
            None
        ));
        // CLAUDE_BIN override (test SUTs / alt installs): the configured
        // program's cmdline is the SESSION's, not foreign.
        assert!(!pid_is_foreign(
            true,
            Some("/tmp/jail/fakerepl --port 1"),
            "/tmp/jail/fakerepl",
            None,
            None
        ));
        assert!(pid_is_foreign(
            true,
            Some("/bin/bash -l"),
            "/tmp/jail/fakerepl",
            None,
            None
        ));
        // Invisible pid: most likely died in the probe window — NOT foreign
        // (kill_pid on a dead pid is a no-op true; don't break legit kills).
        assert!(!pid_is_foreign(true, None, "claude", None, None));
        // A dead pid is never foreign — there is nothing to mis-signal.
        assert!(!pid_is_foreign(
            false,
            Some("/bin/bash -l"),
            "claude",
            None,
            None
        ));
    }

    /// The START-TIME arm (r8, the sound half): (pid, start-time) identity
    /// survives `exec` — the registry contract does not constrain the
    /// session app's cmdline (`exec cat` in the fake-claude harness).
    #[test]
    fn pid_is_foreign_start_time_arm_survives_exec() {
        const BOOT: i64 = 1_750_000_000_000;
        // Process started AT the row's startedAt (claude writes the row
        // moments after its own start): OURS, regardless of cmdline.
        assert!(!pid_is_foreign(
            true,
            Some("cat"),
            "claude",
            Some(BOOT - 1_000),
            Some(BOOT)
        ));
        // Within the slack window (etime is second-resolution): OURS.
        assert!(!pid_is_foreign(
            true,
            Some("cat"),
            "claude",
            Some(BOOT + START_TIME_SLACK_MS - 1),
            Some(BOOT)
        ));
        // Started WELL AFTER the row's boot: a reused pid (the occupant of a
        // reused pid necessarily started after the original died): FOREIGN.
        assert!(pid_is_foreign(
            true,
            Some("cat"),
            "claude",
            Some(BOOT + 3_600_000),
            Some(BOOT)
        ));
        // Start-time unreadable + cmdline unidentifiable: FOREIGN (the
        // cmdline arm is the only evidence and it is negative).
        assert!(pid_is_foreign(
            true,
            Some("cat"),
            "claude",
            None,
            Some(BOOT)
        ));
        // Row has no startedAt: the cmdline arm decides alone.
        assert!(pid_is_foreign(
            true,
            Some("python3 -m http.server"),
            "claude",
            Some(BOOT),
            None
        ));
    }

    #[test]
    fn wrapper_kill_allowed_requires_matching_zmx_wrapper_cmdline() {
        assert!(wrapper_kill_allowed(Some("zmx run -d wk claude"), "wk"));
        // Case-folded name match (r4 rule: names case-insensitive).
        assert!(wrapper_kill_allowed(Some("zmx run WK claude"), "wk"));
        // A reused pid running something else: never signal.
        assert!(!wrapper_kill_allowed(Some("vim notes.txt"), "wk"));
        // A zmx wrapper for a DIFFERENT session: never signal.
        assert!(!wrapper_kill_allowed(Some("zmx run other claude"), "wk"));
        // Already gone (invisible): nothing to signal.
        assert!(!wrapper_kill_allowed(None, "wk"));
    }

    // ===== red-team r8 pins (lead-2): identity-first =====

    #[test]
    fn r8_m1_session_program_is_token_basename_not_substring() {
        // The falsified substring form: anything under a .claude PATH (or a
        // claude.md argument) contained the bytes "claude" and was
        // mis-identified as the session — then SIGTERMed on a reused pid.
        assert!(!cmdline_is_session_program(
            "node /home/u/.claude/plugins/helper.js",
            "claude"
        ));
        assert!(!cmdline_is_session_program("vim claude.md", "claude"));
        assert!(!cmdline_is_session_program(
            "tail -f /Users/u/.claude/logs/x.log",
            "claude"
        ));
        // Real invocations match by token basename.
        assert!(cmdline_is_session_program("claude --name wk", "claude"));
        assert!(cmdline_is_session_program(
            "node /usr/local/bin/claude --resume abc",
            "claude"
        ));
        // CLAUDE_BIN override: full-token or basename of the override.
        assert!(cmdline_is_session_program(
            "/tmp/jail/fakerepl --port 1",
            "/tmp/jail/fakerepl"
        ));
        assert!(!cmdline_is_session_program(
            "/bin/bash -l",
            "/tmp/jail/fakerepl"
        ));
        // Extensionless basename CONTAINING "claude": a session program shape
        // (`fake-claude` launched under a CLAUDE_BIN that is absent from the
        // stopping shell's env — ambient-env reality, g_coldstart_n leg (d)).
        assert!(cmdline_is_session_program(
            "/jail/fake-claude --boot",
            "claude"
        ));
        // …but a dotted basename is a content FILE, never the program.
        assert!(!cmdline_is_session_program("less notes.claude", "claude"));
        // Documented residual (accepted): an extensionless token whose
        // basename contains "claude" still matches (`man claude`);
        // punch-listed start-time belt.
        assert!(cmdline_is_session_program("man claude", "claude"));
    }

    #[test]
    fn r8_m2_foreign_registry_pid_contributes_no_witnesses() {
        // THE r8 M#2 shape: the stale registry pid was REUSED by a process
        // inside a live same-name victim's tree — the raw ancestor chain
        // reaches the victim's pane pid. The gate must zero EVERYTHING.
        let poisoned_chain = vec![19932, 19914]; // reused pid → victim pane
        let (own_pid, wrapper, chain, foreign) = gate_pid_witnesses(
            PidProvenance::Registry,
            19932,
            true,                           // reused pid is alive
            Some("python3 -m http.server"), // ...as a stranger
            "claude",
            None, // start time unreadable
            Some(1_750_000_000_000),
            Some((19914, "wk".to_string())), // fb2 sibling: poisoned wrapper walk
            poisoned_chain,
        );
        assert_eq!(
            own_pid, 19932,
            "the pid itself survives — clause (a) is mux-corroborated, not derived"
        );
        assert_eq!(wrapper, None, "fallback-2 witness suppressed");
        assert!(chain.is_empty(), "clause (b) chain suppressed");
        assert!(foreign, "signal skipped; tombstone proceeds");

        // End-to-end through the resolver: the victim pane survives.
        let live = vec![z("wk", 19914, CANON)];
        let alive = |p: i32| p == 19914 || p == 19932;
        let t = resolve_zmx_target(
            None,
            Some("wk"),
            own_pid,
            CANON,
            &live,
            &[],
            wrapper,
            &chain,
            &alive,
        );
        assert_eq!(t.zmx_name, None, "poisoned witnesses must confirm NOTHING");
        assert_eq!(t.wrapper_pid, 0);
    }

    #[test]
    fn r8_pane_derived_pid_is_exempt_from_the_foreign_gate() {
        // Cold-row adoption / ZmxOnly: the pid came from THIS invocation's
        // pane scan — same-scan truth. A bash remnant's cmdline is not
        // "claude", and gating it would regress adopted-row stop (Q2).
        let (own_pid, _, chain, foreign) = gate_pid_witnesses(
            PidProvenance::PaneDerived,
            700,
            true,
            Some("bash -lc something"),
            "claude",
            None,
            None,
            None,
            vec![700],
        );
        assert_eq!(own_pid, 700);
        assert!(!foreign);
        assert_eq!(chain, vec![700]);
    }

    #[test]
    fn r8_m3_candidate_filters_are_case_folded() {
        // Names are case-insensitive for addressing (r4); the ownership/dead
        // gate now does the discrimination, so the candidate match folds. A
        // legacy case-variant OWN pane (dead) is reaped by its FOUND name…
        let live = vec![z("WK", 111, CANON)];
        let t = resolve_zmx_target(
            None,
            Some("wk"),
            222,
            CANON,
            &live,
            &[],
            None,
            &[],
            ALL_DEAD,
        );
        assert_eq!(t.zmx_name.as_deref(), Some("WK"), "killed by FOUND name");
        // …while a live case-variant VICTIM is still refused by the gate.
        let alive = |p: i32| p == 111;
        let t = resolve_zmx_target(None, Some("wk"), 222, CANON, &live, &[], None, &[], &alive);
        assert_eq!(t.zmx_name, None);
    }

    #[test]
    fn r8_m1_fallback3_takes_unreachable_rows_too() {
        // r7 narrowed fallback 3 to ended-only; an UNREACHABLE row's leaked
        // wrapper was then never reaped. Widened to the exact live-filter
        // negation (!is_attachable); the verb's wrapper_kill_allowed cmdline
        // guard remains the kill-side identity.
        let mut unreachable = z("sess", 777, CANON);
        unreachable.zmx_status = Some("unreachable".to_string());
        let raw = vec![unreachable];
        let t = resolve_zmx_target(None, Some("sess"), 0, CANON, &[], &raw, None, &[], ALL_DEAD);
        assert_eq!(t.zmx_name.as_deref(), Some("sess"));
        assert_eq!(t.wrapper_pid, 777);
    }

    // ===== THE RESOLUTION TRUTH TABLE (orc directive, r8) =====
    //
    // Wholesale closure of the seam r6/r7/r8 walked instance-by-instance:
    // every (row-source × recorded-pid state × pane state) cell pinned, not
    // just the cells findings visited. Row-sources collapse pairwise into
    // PidProvenance at this layer — the verb's mapping is itself pinned by
    // construction (LiveRegistry|Tombstoned → Registry; ColdJsonl|ZmxOnly →
    // PaneDerived), so 2×4×3 = 24 cells here cover the 4×4×3 enumeration.
    //
    // Pid states: AliveOurs (cmdline = session program), AliveForeign
    // (stranger cmdline), Dead, ProbeNone (alive, ps-invisible). Pane states:
    // OWNED (name-matched pane linked by pid truth: in-chain for Registry,
    // pid-equal for PaneDerived/ProbeNone), OTHER (name-matched, alive,
    // unlinked — the victim class), NONE (no name match).
    //
    // Expected columns: the confirmed pane target (None = no pane is killed),
    // and pid_foreign (signal skipped + tombstone proceeds when true).
    #[test]
    fn resolution_truth_table() {
        use PidProvenance::*;

        const PID: i64 = 900; // the addressed row's recorded pid
        const LINK: i32 = 901; // a pane pid linked to PID by process truth
        const VICTIM: i32 = 902; // an alive same-name pane, NOT linked

        #[derive(Clone, Copy, PartialEq)]
        enum PidState {
            AliveOurs,
            AliveForeign,
            Dead,
            ProbeNone,
        }
        use PidState::*;
        #[derive(Clone, Copy, PartialEq)]
        enum PaneState {
            Owned,
            Other,
            NoPane,
        }
        use PaneState::*;

        struct Cell {
            prov: PidProvenance,
            pid_state: PidState,
            pane: PaneState,
            expect_pane_killed: bool,
            expect_foreign: bool,
        }
        // EVERY cell, expectations written by hand (not re-derived):
        #[rustfmt::skip]
        let table = [
            // Registry-sourced pid (LiveRegistry / Tombstoned rows).
            Cell { prov: Registry, pid_state: AliveOurs,    pane: Owned,  expect_pane_killed: true,  expect_foreign: false },
            Cell { prov: Registry, pid_state: AliveOurs,    pane: Other,  expect_pane_killed: false, expect_foreign: false },
            Cell { prov: Registry, pid_state: AliveOurs,    pane: NoPane, expect_pane_killed: false, expect_foreign: false },
            // Foreign (reused) pid: NOTHING confirms, signal skipped (r8 M#2).
            Cell { prov: Registry, pid_state: AliveForeign, pane: Owned,  expect_pane_killed: false, expect_foreign: true  },
            Cell { prov: Registry, pid_state: AliveForeign, pane: Other,  expect_pane_killed: false, expect_foreign: true  },
            Cell { prov: Registry, pid_state: AliveForeign, pane: NoPane, expect_pane_killed: false, expect_foreign: true  },
            // Dead pid (the stale row): only a DEAD pane is cleaned (r7 F1 —
            // an Owned link cannot exist for a dead pid, so Owned here means
            // "the stale row's own dead pane", accepted via clause (c)).
            Cell { prov: Registry, pid_state: Dead,         pane: Owned,  expect_pane_killed: true,  expect_foreign: false },
            Cell { prov: Registry, pid_state: Dead,         pane: Other,  expect_pane_killed: false, expect_foreign: false },
            Cell { prov: Registry, pid_state: Dead,         pane: NoPane, expect_pane_killed: false, expect_foreign: false },
            // ProbeNone (alive, ps-invisible): not foreign (r7 ruling), but
            // the chain is bare [pid] — only a pid-equal pane confirms.
            Cell { prov: Registry, pid_state: ProbeNone,    pane: Owned,  expect_pane_killed: true,  expect_foreign: false },
            Cell { prov: Registry, pid_state: ProbeNone,    pane: Other,  expect_pane_killed: false, expect_foreign: false },
            Cell { prov: Registry, pid_state: ProbeNone,    pane: NoPane, expect_pane_killed: false, expect_foreign: false },
            // Pane-derived pid (ColdJsonl adoption / ZmxOnly rows): the
            // foreign gate NEVER applies (same-scan pane truth, Q2) — incl.
            // the AliveForeign-cmdline cell (a bash remnant is still the
            // adopted pane's own process).
            Cell { prov: PaneDerived, pid_state: AliveOurs,    pane: Owned,  expect_pane_killed: true,  expect_foreign: false },
            Cell { prov: PaneDerived, pid_state: AliveOurs,    pane: Other,  expect_pane_killed: false, expect_foreign: false },
            Cell { prov: PaneDerived, pid_state: AliveOurs,    pane: NoPane, expect_pane_killed: false, expect_foreign: false },
            Cell { prov: PaneDerived, pid_state: AliveForeign, pane: Owned,  expect_pane_killed: true,  expect_foreign: false },
            Cell { prov: PaneDerived, pid_state: AliveForeign, pane: Other,  expect_pane_killed: false, expect_foreign: false },
            Cell { prov: PaneDerived, pid_state: AliveForeign, pane: NoPane, expect_pane_killed: false, expect_foreign: false },
            Cell { prov: PaneDerived, pid_state: Dead,         pane: Owned,  expect_pane_killed: true,  expect_foreign: false },
            Cell { prov: PaneDerived, pid_state: Dead,         pane: Other,  expect_pane_killed: false, expect_foreign: false },
            Cell { prov: PaneDerived, pid_state: Dead,         pane: NoPane, expect_pane_killed: false, expect_foreign: false },
            Cell { prov: PaneDerived, pid_state: ProbeNone,    pane: Owned,  expect_pane_killed: true,  expect_foreign: false },
            Cell { prov: PaneDerived, pid_state: ProbeNone,    pane: Other,  expect_pane_killed: false, expect_foreign: false },
            Cell { prov: PaneDerived, pid_state: ProbeNone,    pane: NoPane, expect_pane_killed: false, expect_foreign: false },
        ];
        assert_eq!(table.len(), 24, "full enumeration, no cell skipped");

        for (i, cell) in table.iter().enumerate() {
            // --- instantiate the cell exactly as the verb would ---
            let alive_pid = matches!(cell.pid_state, AliveOurs | AliveForeign | ProbeNone);
            let cmdline = match cell.pid_state {
                AliveOurs => Some("claude --name wk"),
                AliveForeign => Some("python3 -m http.server"),
                Dead => None,
                ProbeNone => None,
            };
            // The pane's pid: Owned panes are linked by process truth. For
            // Registry rows the link is the ancestor chain (or, for a Dead
            // pid's own stale pane, the pane is simply DEAD); for PaneDerived
            // / ProbeNone rows the link is pid equality (clause (a)).
            let owned_pane_pid: i32 = match (cell.prov, cell.pid_state) {
                (Registry, AliveOurs | AliveForeign) => LINK,
                (Registry, Dead) => LINK, // dead stale pane (clause (c))
                (Registry, ProbeNone) => PID as i32,
                (PaneDerived, _) => PID as i32, // join took pid FROM the pane
            };
            let panes = match cell.pane {
                Owned => vec![z("wk", owned_pane_pid, CANON)],
                Other => vec![z("wk", VICTIM, CANON)],
                NoPane => vec![],
            };
            // Raw witnesses as the verb builds them (before the gate). The
            // AliveForeign chain is POISONED (reaches LINK — the M#2 shape:
            // for the Owned cell LINK is the pane, proving the gate is what
            // protects it, not luck).
            let raw_chain: Vec<i32> = match cell.pid_state {
                AliveOurs | AliveForeign => vec![PID as i32, LINK],
                Dead | ProbeNone => vec![PID as i32],
            };
            let is_alive = move |p: i32| -> bool {
                if p == PID as i32 {
                    return alive_pid;
                }
                if p == LINK {
                    // The linked pane process: alive except in the Dead row
                    // state (the stale row's own pane died with it).
                    return !matches!(cell.pid_state, Dead);
                }
                if p == VICTIM {
                    return true; // the victim is always alive
                }
                false
            };

            // --- the unit under test: gate, then resolve ---
            let (own_pid, wrapper, chain, foreign) = gate_pid_witnesses(
                cell.prov, PID, alive_pid, cmdline, "claude",
                None, // start-time arm: exercised by its own pins
                None, None, // wrapper witness: exercised by its own pins; chain here
                raw_chain,
            );
            let t = resolve_zmx_target(
                None,
                Some("wk"),
                own_pid,
                CANON,
                &panes,
                &[],
                wrapper,
                &chain,
                &is_alive,
            );

            let pane_killed = t.zmx_name.is_some();
            assert_eq!(
                pane_killed, cell.expect_pane_killed,
                "cell {i}: pane-killed mismatch"
            );
            assert_eq!(foreign, cell.expect_foreign, "cell {i}: foreign mismatch");
            // Universal invariant: an OTHER pane (the alive, unlinked victim)
            // is NEVER the target, in any cell.
            if cell.pane == Other {
                assert_eq!(t.zmx_name, None, "cell {i}: victim pane targeted");
                assert_eq!(t.wrapper_pid, 0, "cell {i}: victim wrapper targeted");
            }
        }
    }
}

#[cfg(test)]
mod descendant_tests {
    use super::descendant_kill_list;
    use crate::effects::ProcRow;
    use std::collections::HashMap;

    fn rows(edges: &[(i32, i32)]) -> HashMap<i32, ProcRow> {
        // (pid, ppid) pairs.
        edges
            .iter()
            .map(|&(pid, ppid)| {
                (
                    pid,
                    ProcRow {
                        ppid,
                        cmd: format!("proc-{pid}"),
                    },
                )
            })
            .collect()
    }

    /// punch item 8 PIN: bottom-up order — deepest victims first (a parent is
    /// never signaled before its children), pid-ascending within a depth.
    #[test]
    fn bottom_up_deepest_first() {
        // 100 -> {201, 202}; 201 -> {301}; 301 -> {401}
        let r = rows(&[(100, 1), (201, 100), (202, 100), (301, 201), (401, 301)]);
        let list = descendant_kill_list(100, 9_999_999, &r);
        assert_eq!(list, vec![401, 301, 201, 202]);
    }

    /// Root excluded; unrelated processes (the NEIGHBOR class) excluded.
    #[test]
    fn neighbors_and_root_never_listed() {
        // 100 -> 201; neighbor 555 (same depth, different parent 50).
        let r = rows(&[(100, 1), (201, 100), (50, 1), (555, 50)]);
        let list = descendant_kill_list(100, 9_999_999, &r);
        // The exact-equality is the whole assertion: root (100) is the verb
        // ladder's job not the sweep's, and the neighbor (555, under a
        // different parent 50) is outside the tree — both are absent by
        // construction of `vec![201]`. (Separate `!contains` asserts here
        // would be unfalsifiable after this line — S1.)
        assert_eq!(list, vec![201]);
    }

    /// punch item 8 SELF-STOP GUARD: qd stopping the session it runs INSIDE
    /// (self_pid is a descendant of root) sweeps NOTHING.
    #[test]
    fn self_stop_yields_empty_sweep() {
        // claude(100) -> shell(200) -> qd(300); plus a sibling subtree 210->310.
        let r = rows(&[(100, 1), (200, 100), (300, 200), (210, 100), (310, 210)]);
        assert_eq!(descendant_kill_list(100, 300, &r), Vec::<i32>::new());
        // And root == self likewise.
        assert_eq!(descendant_kill_list(100, 100, &r), Vec::<i32>::new());
        // Control: with self OUTSIDE the tree the sweep is real.
        let list = descendant_kill_list(100, 7_777, &r);
        assert_eq!(list, vec![300, 310, 200, 210]);
    }

    /// Cycle/garbage safety: self-parented rows and ppid cycles terminate.
    #[test]
    fn cycles_and_garbage_terminate() {
        // 100 -> 201; 201 -> 100 (cycle); 666 self-parented; 0/negative noise.
        let r = rows(&[(100, 201), (201, 100), (666, 666), (-5, 1)]);
        let list = descendant_kill_list(100, 9_999_999, &r);
        assert_eq!(list, vec![201]); // visits 201 once, never loops
        assert!(descendant_kill_list(0, 1, &r).is_empty());
        assert!(descendant_kill_list(-1, 1, &r).is_empty());
    }
}

#[cfg(test)]
mod sweep_root_tests {
    use super::{sweep_root_allowed, START_TIME_SLACK_MS};

    const BIN: &str = "claude";

    /// b3 adversarial concern 1 PIN: NO positive witness (cmdline None AND
    /// start time unconfirmable) → the sweep does NOT root. This is the
    /// reused-stranger-with-failed-cmdline-probe shape the lone-kill gate
    /// (pid_is_foreign) lets through as not-foreign.
    #[test]
    fn no_witness_does_not_root_sweep() {
        // cmdline unreadable, and no row started_at to confirm against.
        assert!(!sweep_root_allowed(None, BIN, Some(1_000), None));
        // cmdline unreadable, and proc start unreadable too.
        assert!(!sweep_root_allowed(None, BIN, None, Some(1_000)));
        assert!(!sweep_root_allowed(None, BIN, None, None));
    }

    /// A confirmed start-time MISMATCH (beyond slack) is a hard veto even when
    /// the cmdline probe failed — absent cmdline must not override it.
    #[test]
    fn confirmed_start_mismatch_vetoes_even_with_absent_cmdline() {
        let row = 1_000_000;
        let way_later = row + START_TIME_SLACK_MS + 60_000;
        assert!(!sweep_root_allowed(None, BIN, Some(way_later), Some(row)));
        // And even if the cmdline LOOKS ours, a confirmed mismatch still vetoes
        // (a reused pid running an unrelated `claude`-named program).
        assert!(!sweep_root_allowed(
            Some("claude"),
            BIN,
            Some(way_later),
            Some(row)
        ));
    }

    /// Positive witnesses root the sweep, matching today's intent.
    #[test]
    fn positive_witness_roots_sweep() {
        let row = 1_000_000;
        // cmdline matched-ours, start time unknown → roots.
        assert!(sweep_root_allowed(
            Some("/usr/bin/claude --foo"),
            BIN,
            None,
            Some(row)
        ));
        // confirmed started-ours (within slack), cmdline unreadable → roots.
        assert!(sweep_root_allowed(None, BIN, Some(row + 1_000), Some(row)));
        // started slightly before the row stamp (the common legit case) → roots.
        assert!(sweep_root_allowed(None, BIN, Some(row - 5_000), Some(row)));
    }
}
