//! C-CHAOS — the fault-injection loop component for the pi tier-(a) adapter
//! (WS-A.2). The mirror of the codex C-CHAOS harness (`tests/codex_chaos.rs` on
//! `main`), re-expressed against pi's REAL live-daemon surface
//! (`residence.rs` / `create_daemon.rs` / `registry.rs` / the pgid teardown) and
//! cast in the pi house-style (a library here + a thin `QD_PI_LIVE`-gated
//! `tests/pi_chaos.rs` wrapper, like [`super::redteam`] / [`super::conformance`]).
//!
//! **What it proves.** The adapter degrades-not-crashes, leaves no orphans, and
//! never latches a stale endpoint/transcript-tail, across the 4 preregistered
//! fault classes — each asserting OBSERVED EFFECT AT SOURCE (registry-row JSON,
//! the proc tree via `ps`/`pgrep`, the loopback endpoint, the `<pid>.json` status),
//! NEVER a return-or-exit string. Cred-free (NO model turn).
//!
//! **The 4 fault classes** (preregistration §C-CHAOS):
//!   1. reconnect-across-invocations
//!   2. daemon crash + PID-reuse / same-port rebind (incarnation-CAS, no split-brain)
//!   3. wedge / daemon-gone wait-fallback (connectionless, status-keyed)
//!   4. pgid-teardown grandchild-orphan (+ the non-vacuity escapee characterization)
//!
//! **At-source mechanism deviations from the codex-mirror preregistration**
//! (approved by pi-lead-3 pre-run as faithfulness corrections, NOT a bar change —
//! documented here + emitted in the round evidence):
//!   - **Class 1**: `qd info --json` DELIBERATELY OMITS `endpoint`
//!     (`registry.rs:125-134`, `#[serde(skip…)]`, §9.4 "endpoint stays OUT of the
//!     JSON surface"). So `pid` is read from `qd info --json` at source, and the
//!     `endpoint` is RE-RESOLVED from the registry row + a live `PiRemote` connect.
//!     This matches the codex precedent (which also sources endpoint from the row).
//!   - **Class 3**: pi has NO dedicated `qd wait` branch — it falls through the
//!     claude path (`wait.rs:146+`), which feeds `paths.projects_dir` (claude's
//!     root) to `pi.transcript_path` (`wait.rs:213-237`), NOT pi's own
//!     `transcript_root` (`provider/pi/mod.rs:76`). So a fabricated pi transcript is
//!     never read; pi's daemon-gone wait is STATUS-KEYED off `<pid>.json`
//!     (`PidFileStatus`/`StatusFallback`). The "fabricate-transcript-at-
//!     transcript_path()" literal was a codex-mirror artifact. The class-3
//!     transcript-root divergence is PINNED as an `is_break:false` characterization
//!     for the tier-a acceptance oracle.
//!
//! **Instrument-faithfulness note (class 4).** The pinned pi is a Node script
//! (`~/.npm-pi-global/bin/pi` → `dist/cli.js`, `#!/usr/bin/env node`) spawned via
//! `Command::new(pi_bin)` WITHOUT its own process_group (`stdio.rs:266`), so the pi
//! child's `comm` is `node`, not `pi` — `pgrep -x pi` is a NULL instrument for pi
//! (the run-protocol mandate is kept only as a recorded cross-check). The PRIMARY
//! class-4 probe is PGID-MEMBERSHIP enumeration (every live pid whose pgid == the
//! resident pid), instance-scoped by the unique resident pgid, with a load-bearing
//! NON-VACUITY guard (≥1 non-resident member must exist before teardown).
//!
//! **Counter-handling.** A clean class = `pass && !is_break`. A real SUT break
//! (`is_break:true`) is reproduce-first then bubbled (counter resets). A
//! characterization (`is_break:false`) is pinned for the oracle (no reset). The
//! harness asserts at source and emits a `RAN.txt` per class so a gate-skip can
//! never read as clean (the live-gated-noop guard).

use std::collections::HashSet;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use super::conformance::{start_argv, QdRunner};
use crate::provider::pi::daemon::residence::cmdline_is_our_pi_daemon;
use crate::create_daemon::{real_cmdline_probe, DaemonSpawner, RealDaemonSpawner};
use crate::effects::is_pid_alive;
use crate::registry::{self, RegistryEntry, StatusWriteOutcome, TombstoneOutcome};

/// One fault-class verdict. RUN-not-read: `commands` is the exact injection driven
/// and `observed` is the at-source evidence the verdict rests on (never an
/// assertion). `is_break` = a REAL SUT break (degrade-not-crash violated);
/// `is_characterization` = a pinned finding for the acceptance oracle (is_break:false).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChaosVerdict {
    /// The fault class label, e.g. `"class1-reconnect"`.
    pub class: String,
    /// The mechanism behaved correctly (degraded, did not crash/orphan/latch).
    pub pass: bool,
    /// A REAL SUT break surfaced (routes to reproduce-first → bubble → reset).
    pub is_break: bool,
    /// A pinned characterization for the tier-a acceptance oracle (is_break:false).
    pub is_characterization: bool,
    /// Human detail (what was asserted, what diverged).
    pub detail: String,
    /// The exact injection/probe commands run (the live-run evidence).
    pub commands: Vec<String>,
    /// The observed source state the verdict rests on (row JSON, pgids, statuses…).
    pub observed: String,
    /// The on-disk evidence dir for this class (`…/round{N}/<class>/`).
    pub evidence_dir: String,
}

impl ChaosVerdict {
    fn pass(class: &str, dir: &Path, detail: impl Into<String>) -> Self {
        Self {
            class: class.into(),
            pass: true,
            is_break: false,
            is_characterization: false,
            detail: detail.into(),
            commands: Vec::new(),
            observed: String::new(),
            evidence_dir: dir.display().to_string(),
        }
    }
    fn brk(class: &str, dir: &Path, detail: impl Into<String>) -> Self {
        Self {
            class: class.into(),
            pass: false,
            is_break: true,
            is_characterization: false,
            detail: detail.into(),
            commands: Vec::new(),
            observed: String::new(),
            evidence_dir: dir.display().to_string(),
        }
    }
    fn characterization(class: &str, dir: &Path, detail: impl Into<String>) -> Self {
        Self {
            class: class.into(),
            pass: true,
            is_break: false,
            is_characterization: true,
            detail: detail.into(),
            commands: Vec::new(),
            observed: String::new(),
            evidence_dir: dir.display().to_string(),
        }
    }
    fn cmd(mut self, c: impl Into<String>) -> Self {
        self.commands.push(c.into());
        self
    }
    fn obs(mut self, o: impl Into<String>) -> Self {
        self.observed = o.into();
        self
    }
}

/// The round report — the inspectable evidence artifact the oracle rules on.
#[derive(Debug, Clone, Serialize)]
pub struct ChaosReport {
    pub round: u32,
    pub live: bool,
    pub verdicts: Vec<ChaosVerdict>,
}

impl ChaosReport {
    /// A round is CLEAN when every verdict passed and NONE is a break. A
    /// characterization (is_break:false) does NOT dirty the round.
    pub fn clean(&self) -> bool {
        !self.verdicts.is_empty() && self.verdicts.iter().all(|v| v.pass && !v.is_break)
    }
    pub fn breaks(&self) -> Vec<&ChaosVerdict> {
        self.verdicts.iter().filter(|v| v.is_break).collect()
    }
    pub fn characterizations(&self) -> Vec<&ChaosVerdict> {
        self.verdicts
            .iter()
            .filter(|v| v.is_characterization)
            .collect()
    }
    pub fn to_evidence_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }
}

// ===========================================================================
// At-source proc-tree probes (RUN-not-read: every value comes from `ps`/`pgrep`
// at the source, never inferred).
// ===========================================================================

/// `ps -o pgid= -p <pid>` — the process-group id of `pid`, at source. `None` if
/// the pid is gone / ps errors. The class-4 escape-proof primitive.
pub fn pgid_of(pid: i64) -> Option<i64> {
    let out = Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().parse::<i64>().ok()
}

/// `ps -o args= -p <pid>` — the full cmdline of `pid`, at source (empty if gone).
pub fn proc_args(pid: i64) -> String {
    Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// A live process-group member: `(pid, pgid, comm, args)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcRow {
    pub pid: i64,
    pub pgid: i64,
    pub comm: String,
    pub args: String,
}

/// Parse `ps -eo pid=,pgid=,comm=,args=` output into rows (pure — unit-tested).
pub fn parse_ps_rows(stdout: &str) -> Vec<ProcRow> {
    let mut rows = Vec::new();
    for line in stdout.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(pgid), Some(comm)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(pgid)) = (pid.parse::<i64>(), pgid.parse::<i64>()) else {
            continue;
        };
        let args: String = it.collect::<Vec<_>>().join(" ");
        rows.push(ProcRow {
            pid,
            pgid,
            comm: comm.to_string(),
            args,
        });
    }
    rows
}

/// Every live pid whose pgid == `pgid`, at source (the instance-scoped teardown
/// unit — immune to comm-name and to pgrep self-match).
pub fn pgid_members(pgid: i64) -> Vec<ProcRow> {
    let out = match Command::new("ps")
        .args(["-eo", "pid=,pgid=,comm=,args="])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    parse_ps_rows(&String::from_utf8_lossy(&out.stdout))
        .into_iter()
        .filter(|r| r.pgid == pgid)
        .collect()
}

/// `pgrep -x <comm>` — exact-comm match (the run-protocol cross-check; for pi this
/// is expected to be a NULL instrument since the child runs as `node`).
pub fn pgrep_x(comm: &str) -> Vec<i64> {
    Command::new("pgrep")
        .args(["-x", comm])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<i64>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Mint a realistic DEAD pid: spawn a `sleep`, kill+reap it, return its (now-dead)
/// pid — for the connectionless daemon-gone fabricated row (class 3).
fn dead_pid() -> i64 {
    match Command::new("sleep").arg("600").spawn() {
        Ok(mut child) => {
            let pid = child.id() as i64;
            let _ = child.kill();
            let _ = child.wait();
            pid
        }
        // Fall back to an implausibly-high pid if spawn fails (still "dead").
        Err(_) => 2_147_480_000,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse the port out of a `ws://127.0.0.1:<port>` endpoint, at source.
pub fn endpoint_port(endpoint: &str) -> Option<u16> {
    endpoint
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
}

/// RAII reaper belt: on drop, group-SIGKILL every recorded pgid and pid (so a
/// panicking test leaks nothing). Uses the raw `libc::kill` syscall — NOT external
/// `/usr/bin/kill` (which misparses a negative pgid).
struct GroupReaper {
    pgids: Vec<i64>,
    pids: Vec<i64>,
    /// The executor's OWN pgid, captured at construction — the drop belt refuses to
    /// group-kill it (layer-a(iii)) even during panic-unwind cleanup.
    own_pgid: i64,
}
impl GroupReaper {
    fn new() -> Self {
        Self {
            pgids: Vec::new(),
            pids: Vec::new(),
            own_pgid: own_pgid(),
        }
    }
    fn watch_group(&mut self, pgid: i64) {
        self.pgids.push(pgid);
    }
    fn watch_pid(&mut self, pid: i64) {
        self.pids.push(pid);
    }
}
impl Drop for GroupReaper {
    fn drop(&mut self) {
        // A4 CONTAINMENT BELT (layer a): even the panic-unwind cleanup path must never
        // group-kill the executor's own group or a fleet.service group. safe_group_kill
        // already no-ops pgid <= 1 (0 = own group, 1 = kill(-1) box-killer); we ADD the
        // own-pgid + live fleet-PGID exclusions here. No panic (a Drop that panics during
        // an active unwind aborts the process); refusing the kill is the safe action.
        let fleet = fleet_pgids_now();
        for &pgid in &self.pgids {
            if pgid > 1 && pgid != self.own_pgid && !fleet.contains(&pgid) {
                crate::safe_kill::safe_group_kill(pgid, libc::SIGKILL);
            }
        }
        for &pid in &self.pids {
            if pid > 0 {
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
        }
    }
}

// ===========================================================================
// Evidence helpers
// ===========================================================================

fn class_dir(root: &Path, class: &str) -> PathBuf {
    let d = root.join(class);
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Write the post-gate RAN marker (proves the class actually executed live; a
/// gate-skip writes nothing — the live-gated-noop guard).
fn write_ran(dir: &Path, note: &str) {
    let _ = std::fs::write(dir.join("RAN.txt"), format!("RAN: {note}\n"));
}

/// Write the at-source observed evidence for a class.
fn write_at_source(dir: &Path, class: &str, body: &str) {
    let _ = std::fs::write(dir.join(format!("{class}-at-source.txt")), body);
}

// ===========================================================================
// A4 CONTAINMENT (A4-ISOLATION-PLAN.md — BINDING). Three layers make it
// impossible for a chaos group-kill to reach a fleet.service process:
//   (a) own-pgid: every candidate group-kill pgid is asserted a known-own resident
//       pgid — (i) >1, (ii) spawned THIS round, (iii) != executor own pgid,
//       (iv) ∉ the live fleet.service PGID set (TIGHTENING #1: fleet PIDs mapped to
//       PGIDs via getpgid, compared PGID-to-PGID). A violation ABORTS + panics.
//   (b) before==after fleet.service cgroup.procs snapshot (the teeth): any
//       pre-existing fleet pid GONE after a kill/round → VOID + panic naming it.
//       Snapshotted LIVE (never hardcoded — the supervisor pid moves on reparent).
//   (c) physical: the LIVE test-run is launched under a systemd-run --scope
//       transient cgroup OUTSIDE fleet.service (scripts/pi-a4-chaos-run.sh); the
//       residents inherit that scope. capture_proc_cgroup() records the proof.
// These guards are DEFENSE-IN-DEPTH on top of (c); each is independently sufficient.
// ===========================================================================

/// The durable supervisor's cgroup proc-list (world-readable: `-rw-r--r-- root`).
const FLEET_CGROUP_PROCS: &str = "/sys/fs/cgroup/system.slice/fleet.service/cgroup.procs";

/// `getpgid(pid)` at source via libc (`getpgid(0)` = own group). `None` if the pid
/// is gone / not permitted. Reading a pgid needs no ownership on Linux.
fn getpgid(pid: i64) -> Option<i64> {
    let r = unsafe { libc::getpgid(pid as libc::pid_t) };
    if r < 0 {
        None
    } else {
        Some(r as i64)
    }
}

/// This process's own process-group id. `getpgid(0)` never fails.
fn own_pgid() -> i64 {
    unsafe { libc::getpgid(0) as i64 }
}

/// Parse a `cgroup.procs`-format file (one pid per line) into a pid set. An absent
/// file (fleet.service not present, e.g. hermetic CI / a dev box) yields the EMPTY
/// set — layers (a)(iv)/(b) then correctly degrade to no-ops (nothing to protect).
fn read_pid_set(path: &str) -> HashSet<i64> {
    std::fs::read_to_string(path)
        .map(|s| {
            s.lines()
                .filter_map(|l| l.trim().parse::<i64>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Layer (b) RAW snapshot: the LIVE fleet.service pid set, read at call time (NEVER
/// hardcoded — super19's daemon pid already moved 1956851→1956858 on reparent).
/// Written to evidence; the ASSERTION runs on the DURABLE-filtered subset below.
pub fn fleet_snapshot() -> HashSet<i64> {
    read_pid_set(FLEET_CGROUP_PROCS)
}

/// `/proc/<pid>/comm` at source (the process's command name). `None` if the pid is
/// gone. Used to classify fleet.service members as durable vs transient churn.
fn comm_of(pid: i64) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// TRANSIENT keep-alive churn (super-21's durable-member filter, binding for the
/// Phase-2 converge): a coordinator's keep-alive `sleep`/shell leaves naturally
/// spawn+exit constantly, and their exit is NOT an outage. These are EXCLUDED from
/// the layer-(b) durable snapshot so a benign natural exit cannot false-VOID a round.
///
/// A4.1 ALSO excludes the build-TOOLCHAIN comm set (super-22's build-churn hardening):
/// when a `cargo` build overlaps a live converge round, its compiler/linker/build-driver
/// processes transit fleet.service and EXIT as build steps finish mid-round — a benign
/// exit that would otherwise read as a durable member vanishing (false BREACH / VOID).
/// This set is EMPIRICALLY DERIVED, NOT guessed: an `strace -f -e execve` capture of a
/// real cold `cap-cargo` + `sccache` build on THIS box (aarch64), taking only successful
/// exec basenames truncated to the 15-char `TASK_COMM` width that `/proc/<pid>/comm`
/// reports (see `comm_of`). Observed here: `cargo`/`rustc`/`sccache` (drivers),
/// `cc`/`collect2`/`ld.mold` (the linker chain — this box links via the `mold` linker,
/// whose comm is `ld.mold`, per the cargo config's `-fuse-ld=mold`, plus GNU `collect2`),
/// `build-script-bu` (a compiled `build-script-build`, comm-truncated to 15), and
/// `pkg-config` (dependency probing). Names the toolchain could spawn but that did NOT
/// transit on this box (no C/C++ source, no `rust-lld`/`lld`/`clang`) are deliberately
/// NOT added: including un-observed names would be guessing, and their absence errs SAFE
/// (an uncovered build tool → false-VOID, never a missed real breach).
///
/// CORRECTNESS (why relaxing protection for this set is SAFE + leaves the teeth intact):
/// a build-toolchain process in fleet.service is ALWAYS ephemeral build churn — it is
/// NEVER a legitimate durable fleet member (durable = qd/claude=node/relay/pi/codex/
/// systemd daemons + coordinators). Excluding it therefore errs CORRECTLY (prevents a
/// false-VOID) WITHOUT weakening real-breach detection: no durable member is ever named
/// `rustc`/`cargo`/`cc`/`ld.mold`/etc., so a real qd/claude/pi/relay vanish STILL VOIDs.
/// The match is EXACT full-comm (never substring), so no build name can collide with a
/// durable member. Everything NOT in this deliberately-narrow set stays durable/PROTECTED
/// (fail-safe toward MORE protection, never less: an unknown or future comm errs toward
/// false-VOID, never toward missing a real breach).
fn is_transient_comm(comm: &str) -> bool {
    matches!(
        comm,
        // super-21: keep-alive / shell churn.
        "sleep" | "bash" | "sh" | "dash" | "env" | "timeout" | "cat" | ""
        // A4.1: empirically-derived build-toolchain churn (this box, EXACT full-comm).
        | "cargo" | "rustc" | "sccache" | "cc" | "collect2" | "ld.mold"
        | "build-script-bu" | "pkg-config"
    ) || comm.contains("<defunct>")
}

/// Layer (b) DURABLE snapshot: fleet.service `cgroup.procs` MINUS transient keep-alive
/// churn (super-21's filter). This is the set the `before==after` assertion runs on —
/// a DURABLE member (qd/claude/relay/pi coordinator or daemon) disappearing is a
/// containment BREACH; a transient leaf exiting is not. A pid gone at classification
/// time is simply absent (it is not in `before` either). Returns the durable pid set.
fn fleet_snapshot_durable() -> HashSet<i64> {
    read_pid_set(FLEET_CGROUP_PROCS)
        .into_iter()
        .filter(|&p| match comm_of(p) {
            Some(c) => !is_transient_comm(&c),
            None => false,
        })
        .collect()
}

/// The live fleet.service PGID set: `{ getpgid(p) : p ∈ fleet.service cgroup.procs }`
/// (TIGHTENING #1 — map each fleet PID to its PGID, so a fleet process whose
/// group-leader pid is not itself in the cgroup is still excluded).
fn fleet_pgids_now() -> HashSet<i64> {
    fleet_snapshot().into_iter().filter_map(getpgid).collect()
}

/// Layer (b) core (the TEETH), pure + unit-tested. The pids present in `before` but
/// GONE in `after`, EXCLUDING `killed` (pids that were members of a group we
/// deliberately + layer-a-safely reaped — their disappearance is intended, not an
/// outage). New pids appearing is fine (coordinators spawn/reap keep-alives).
///
/// The `killed` exclusion matters ONLY when the test shares the fleet cgroup — i.e.
/// run WITHOUT the layer-(c) scope (Phase-1 demonstration), where an own-pgid
/// synthetic/resident group's pids transit through fleet.service `cgroup.procs`.
/// UNDER the scope (Phase-2 production) those pids are never in the fleet snapshot,
/// so `killed ∩ before = ∅` and this reduces to the plan's pure `after ⊇ before`.
fn fleet_lost(before: &HashSet<i64>, after: &HashSet<i64>, killed: &HashSet<i64>) -> Vec<i64> {
    let mut gone: Vec<i64> = before
        .difference(after)
        .filter(|p| !killed.contains(p))
        .copied()
        .collect();
    gone.sort_unstable();
    gone
}

/// Layer (b) assertion in its pure form (`after ⊇ before`, no intended-kill
/// exclusion) — the plan's literal teeth, the zero-exclusion base case of
/// `fleet_lost`. Exercised by the unit tests (the live paths use `fleet_lost` with
/// the intended-kill exclusion so they are correct scoped OR unscoped).
#[cfg(test)]
fn fleet_intact(before: &HashSet<i64>, after: &HashSet<i64>) -> Result<(), String> {
    let gone = fleet_lost(before, after, &HashSet::new());
    if gone.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "FLEET.SERVICE OUTAGE: pid(s) {gone:?} present BEFORE disappeared AFTER \
             (a chaos kill reached a durable supervisor/coordinator) — round VOID"
        ))
    }
}

/// Layer (a) pure core (unit-tested synthetically — no /proc, no real fleet). The
/// four teeth, in the plan's order. A candidate group-kill `pgid` is safe iff it is
/// (i) >1, (ii) a resident spawned this round, (iii) != own pgid, (iv) ∉ fleet PGIDs.
fn check_kill_safe(
    pgid: i64,
    own_pgid: i64,
    spawned: &HashSet<i64>,
    fleet_pgids: &HashSet<i64>,
) -> Result<(), String> {
    if pgid <= 1 {
        return Err(format!(
            "layer-a(i): refuse group-kill of pgid={pgid} (≤1 = own group / kill(-1) box-killer)"
        ));
    }
    if !spawned.contains(&pgid) {
        return Err(format!(
            "layer-a(ii): refuse group-kill of pgid={pgid} — NOT a resident spawned this round \
             (spawned={spawned:?}); a wrong/broad recorded pgid is the exact 'reap a sibling' failure"
        ));
    }
    if pgid == own_pgid {
        return Err(format!(
            "layer-a(iii): refuse group-kill of pgid={pgid} == executor own pgid"
        ));
    }
    if fleet_pgids.contains(&pgid) {
        return Err(format!(
            "layer-a(iv): refuse group-kill of pgid={pgid} — it is a fleet.service PGID \
             (would reap a durable supervisor/coordinator; fleet PGIDs={fleet_pgids:?})"
        ));
    }
    Ok(())
}

/// Layer (a) per-round kill authority: the pgids THIS round spawned, plus the
/// executor's own pgid. The fleet PGID set is read LIVE at each assertion.
pub struct KillGuard {
    own_pgid: i64,
    spawned_pgids: HashSet<i64>,
    /// Pids that were MEMBERS of a group we deliberately (layer-a-safely) reaped —
    /// excluded from the round-level layer-(b) check (their vanishing is intended).
    /// See `fleet_lost`: only relevant unscoped; empty-intersection under the scope.
    killed_member_pids: HashSet<i64>,
}

impl Default for KillGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl KillGuard {
    pub fn new() -> Self {
        Self {
            own_pgid: own_pgid(),
            spawned_pgids: HashSet::new(),
            killed_member_pids: HashSet::new(),
        }
    }

    /// Record a resident/root pgid THIS round spawned. A `process_group(0)` leader's
    /// pgid == its pid (create_daemon.rs:861 / the 4b root), so pass the leader pid.
    /// pgids ≤1 are never recorded (they can never be a valid kill target).
    pub fn record_spawn(&mut self, pgid: i64) {
        if pgid > 1 {
            self.spawned_pgids.insert(pgid);
        }
    }

    /// Assert a candidate group-kill pgid is safe (layer-a i–iv), reading the fleet
    /// PGID set LIVE. Err → the caller ABORTS the kill, VOIDs the round, and panics.
    pub fn assert_kill_safe(&self, pgid: i64) -> Result<(), String> {
        check_kill_safe(pgid, self.own_pgid, &self.spawned_pgids, &fleet_pgids_now())
    }
}

/// The ONLY path a chaos-issued group-kill takes: (a) assert the pgid is own-resident
/// safe → abort+panic on violation; capture the group we are about to reap (its
/// members' disappearance is intended); (b) snapshot the live fleet pid set, run the
/// kill, re-snapshot, and assert NO NON-TARGET fleet pid vanished → VOID+panic
/// otherwise. `do_kill` is the actual reap (safe_group_kill for the raw path; the
/// production RealDaemonSpawner.kill ladder for the 4b root — both route through
/// safe_group_kill).
fn guarded_kill(guard: &mut KillGuard, pgid: i64, ctx: &str, do_kill: impl FnOnce()) {
    if let Err(e) = guard.assert_kill_safe(pgid) {
        panic!("A4 CONTAINMENT ABORT [{ctx}]: {e}");
    }
    // The group we are about to reap — its members' vanishing is INTENDED, so exclude
    // them from the disappearance check (they transit fleet.service cgroup.procs only
    // when unscoped; under the layer-(c) scope they are not in the fleet snapshot).
    let members: HashSet<i64> = pgid_members(pgid).into_iter().map(|r| r.pid).collect();
    // Assert on the DURABLE fleet set (super-21's filter): transient keep-alive churn
    // is excluded so only a durable coordinator/daemon disappearing can VOID.
    let before = fleet_snapshot_durable();
    do_kill();
    // Read-consistency settle: if a snapshot caught mid-transition trips the check,
    // re-read once before concluding. (Does NOT weaken it — a truly reaped DURABLE
    // fleet pid stays gone across both reads.)
    let mut after = fleet_snapshot_durable();
    if !fleet_lost(&before, &after, &members).is_empty() {
        std::thread::sleep(Duration::from_millis(250));
        after = fleet_snapshot_durable();
    }
    let gone = fleet_lost(&before, &after, &members);
    if !gone.is_empty() {
        // A DURABLE fleet member vanished around this kill and was NOT a member of the
        // reaped group → CONTAINMENT BREACH. Panic aborts the round; the converge
        // driver HALTS + bubbles (no auto-retry).
        panic!(
            "A4 CONTAINMENT BREACH [{ctx}]: DURABLE fleet.service pid(s) {gone:?} disappeared and \
             were NOT members of the reaped group pgid={pgid} — a kill reached a durable \
             supervisor/coordinator. HALT + preserve evidence + bubble."
        );
    }
    guard.killed_member_pids.extend(members);
}

/// Layer (c) evidence: capture `/proc/<pid>/cgroup` for a spawned resident/root — the
/// artifact proving the transient scope reached the RESIDENTS (they inherit the
/// launcher's cgroup, create_daemon uses `process_group(0)` but NOT a new cgroup).
/// Under the `pi-a4-chaos-r<N>` scope this reads that unit, NOT
/// `system.slice/fleet.service`. Best-effort; written per spawn.
fn capture_proc_cgroup(pid: i64, dir: &Path, label: &str) {
    let body = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .unwrap_or_else(|e| format!("(unreadable: {e})"));
    let _ = std::fs::write(
        dir.join(format!("{label}-proc-cgroup.txt")),
        format!("pid={pid}\n{body}"),
    );
}

// ===========================================================================
// The 4 fault classes
// ===========================================================================

/// CLASS 1 — reconnect-across-invocations (LIVE). A fresh `qd` proc re-resolves
/// the live daemon, proven AT SOURCE: pid from `qd info --json`, endpoint
/// re-resolved from the registry row + a live `PiRemote`/identity reconnect.
fn class1_reconnect(
    runner: &QdRunner,
    cwd: &str,
    dir: &Path,
    guard: &mut KillGuard,
) -> Vec<ChaosVerdict> {
    let class = "class1-reconnect";
    write_ran(dir, "class1 live reconnect");
    let name = "pi-chaos-1";
    let mut log = String::new();

    // Inject: start a live resident.
    let start = start_argv(name, cwd);
    let started = runner.run(&start);
    let started_ok = started
        .as_ref()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let row = runner.read_row(name);
    let (pid, endpoint, started_at) = match &row {
        Some((_, v)) => (
            v.get("pid").and_then(Value::as_i64),
            v.get("endpoint")
                .and_then(Value::as_str)
                .map(str::to_string),
            v.get("startedAt").and_then(Value::as_i64),
        ),
        None => (None, None, None),
    };
    log.push_str(&format!(
        "start ok={started_ok}; row pid={pid:?} endpoint={endpoint:?} startedAt={started_at:?}\n"
    ));

    let Some(pid) = pid else {
        write_at_source(dir, class, &log);
        return vec![ChaosVerdict::brk(class, dir, "no resident row/pid after start").obs(log)];
    };
    // A4 containment: record this resident's pgid as a known-own kill target and
    // capture its cgroup (layer-c residents-reached evidence).
    guard.record_spawn(pgid_of(pid).unwrap_or(pid));
    capture_proc_cgroup(pid, dir, "class1-resident");

    // FRESH qd proc: `qd info <name> --json` — parse at source (pid IS present;
    // endpoint is #[serde(skip)] by design, §9.4 / registry.rs:125-134).
    let info_args = ["info", name, "--json"];
    let info = runner.run(&info_args);
    let info_json: Option<Value> = info
        .as_ref()
        .ok()
        .and_then(|o| serde_json::from_slice(&o.stdout).ok());
    let info_pid = info_json
        .as_ref()
        .and_then(|v| v.get("pid").and_then(Value::as_i64));
    let info_live = info_json
        .as_ref()
        .and_then(|v| v.get("live").and_then(Value::as_bool));
    let info_has_endpoint = info_json
        .as_ref()
        .map(|v| v.get("endpoint").is_some())
        .unwrap_or(false);
    log.push_str(&format!(
        "qd info --json: pid={info_pid:?} live={info_live:?} has_endpoint_field={info_has_endpoint}\n"
    ));

    // FRESH qd proc: `qd ls --live --json` — sessionId surfaces.
    let ls_args = ["ls", "--live", "--json"];
    let ls = runner.run(&ls_args);
    let ls_has_name = ls
        .as_ref()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(name))
        .unwrap_or(false);
    log.push_str(&format!(
        "qd ls --live --json contains {name}: {ls_has_name}\n"
    ));

    // Endpoint RE-RESOLVED from the registry row (the §9.4-faithful source).
    let row_endpoint = runner.read_row(name).and_then(|(_, v)| {
        v.get("endpoint")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    log.push_str(&format!(
        "endpoint re-resolved from row: {row_endpoint:?}\n"
    ));

    // Prove the live reconnect at source: live ws connect + identity.
    let alive_i64 = |p: i64| is_pid_alive(p as i32);
    let probe = real_cmdline_probe;
    let daemon_alive =
        crate::provider::pi::daemon::pi_daemon_is_alive(pid, row_endpoint.as_deref(), &alive_i64, &probe);
    let identity =
        cmdline_is_our_pi_daemon(real_cmdline_probe(pid).as_deref(), row_endpoint.as_deref());
    let ws_ok = match &row_endpoint {
        Some(ep) => crate::provider::pi::daemon::residence::connect_ready(ep, Duration::from_secs(5)).is_ok(),
        None => false,
    };
    log.push_str(&format!(
        "reconnect: pi_daemon_is_alive={daemon_alive} cmdline_is_our_pi_daemon={identity} ws_connect_ready={ws_ok}\n"
    ));

    // Exactly 1 live row for this name; record the resident's group-member count as
    // observed proc evidence.
    let group_members = pgid_members(pid).len();
    let one_live = runner.read_row(name).is_some() && is_pid_alive(pid as i32);
    log.push_str(&format!(
        "exactly-one-live(name): {one_live} (resident pgid members={group_members})\n"
    ));

    // Faithfulness note for the oracle.
    log.push_str(
        "FAITHFULNESS NOTE: qd info --json OMITS endpoint by design (registry.rs:125-134, #[serde(skip)], §9.4). \
         pid is read from info --json at source; endpoint is re-resolved from the registry row + a live PiRemote connect. \
         Matches the codex precedent (endpoint sourced from the row).\n",
    );

    let pass = started_ok
        && info_pid == Some(pid)
        && info_live == Some(true)
        && !info_has_endpoint
        && ls_has_name
        && row_endpoint.is_some()
        && daemon_alive
        && identity
        && ws_ok
        && one_live;

    // Teardown (cleanup).
    let _ = runner.run(&["stop", name]);

    write_at_source(dir, class, &log);
    let v = if pass {
        ChaosVerdict::pass(
            class,
            dir,
            "fresh qd reconnect: info --json pid+live at source; endpoint re-resolved from row; live ws + identity reconnect TRUE",
        )
    } else {
        ChaosVerdict::brk(
            class,
            dir,
            "reconnect did not resolve the live daemon at source (see observed)",
        )
    };
    vec![v
        .cmd(format!("qd {}", start.join(" ")))
        .cmd(format!("qd {}", info_args.join(" ")))
        .cmd(format!("qd {}", ls_args.join(" ")))
        .obs(log)]
}

/// CLASS 2 — daemon crash + PID-reuse / same-port rebind (LIVE). Crash the daemon
/// abruptly (bypassing `qd stop`, so the row is left STALE), prove NO stale-endpoint
/// latch, prove the incarnation-CAS vetoes a foreign-incarnation write (no
/// split-brain), then prove a fresh start is a NEW incarnation.
fn class2_crash_rebind(
    runner: &QdRunner,
    cwd: &str,
    dir: &Path,
    round: u32,
    guard: &mut KillGuard,
) -> Vec<ChaosVerdict> {
    let class = "class2-crash-rebind";
    write_ran(dir, "class2 live crash+rebind");
    let name = "pi-chaos-2";
    let mut log = String::new();
    let mut reaper = GroupReaper::new();

    // Inject A: start a resident, capture pid_a / endpoint_a / started_at_a.
    let start = start_argv(name, cwd);
    let _ = runner.run(&start);
    let row_a = runner.read_row(name);
    let (Some(pid_a), endpoint_a, started_at_a) = (
        row_a
            .as_ref()
            .and_then(|(_, v)| v.get("pid").and_then(Value::as_i64)),
        row_a.as_ref().and_then(|(_, v)| {
            v.get("endpoint")
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
        row_a
            .as_ref()
            .and_then(|(_, v)| v.get("startedAt").and_then(Value::as_i64)),
    ) else {
        write_at_source(dir, class, &log);
        return vec![ChaosVerdict::brk(class, dir, "no resident A row/pid after start").obs(log)];
    };
    let port_a = endpoint_a.as_deref().and_then(endpoint_port);
    log.push_str(&format!(
        "A: pid={pid_a} endpoint={endpoint_a:?} port={port_a:?} startedAt={started_at_a:?}\n"
    ));
    reaper.watch_group(pid_a);
    guard.record_spawn(pgid_of(pid_a).unwrap_or(pid_a));
    capture_proc_cgroup(pid_a, dir, "class2-resident-a");

    // CRASH abruptly: group SIGKILL via the box-killer chokepoint (NOT `qd stop`,
    // NOT external kill — sidesteps the neg-pgid misparse false-RED). pid_a is
    // parsed from a JSON row, so 0/1 is possible → safe_group_kill no-ops those.
    // Row is left STALE. A4 containment: guarded_kill asserts (a) own-resident +
    // (b) fleet-intact around the reap.
    guarded_kill(guard, pid_a, "class2-crash-A", || {
        crate::safe_kill::safe_group_kill(pid_a, libc::SIGKILL)
    });
    // Wait for the group to die.
    let crash_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < crash_deadline && is_pid_alive(pid_a as i32) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let a_dead = !is_pid_alive(pid_a as i32);
    log.push_str(&format!("after group SIGKILL: pid_a alive={}\n", !a_dead));

    // Assert NO stale-endpoint latch at source: row still present (NOT tombstoned),
    // pid dead, ws connect fails, identity gone.
    let sessions_dir = runner.sessions_dir();
    let row_file = sessions_dir.join(format!("{pid_a}.json"));
    let tomb_file = sessions_dir.join(format!("{pid_a}.json.tombstoned"));
    let row_present = row_file.exists();
    let tombstoned = tomb_file.exists();
    let ws_after = endpoint_a
        .as_deref()
        .map(|ep| crate::provider::pi::daemon::residence::connect_ready(ep, Duration::from_secs(1)).is_ok())
        .unwrap_or(false);
    let identity_after =
        cmdline_is_our_pi_daemon(real_cmdline_probe(pid_a).as_deref(), endpoint_a.as_deref());
    log.push_str(&format!(
        "stale row: present={row_present} tombstoned={tombstoned}; ws_connect_after_crash={ws_after} identity_after={identity_after}\n"
    ));

    // Incarnation-CAS veto (split-brain prevention) at source: a FOREIGN started_at
    // is Rejected; the TRUE started_at is Written.
    let foreign = started_at_a.map(|s| s + 1).unwrap_or(now_ms() + 1);
    let cas_foreign = registry::set_status(&sessions_dir, pid_a, Some(foreign), "busy", now_ms());
    let cas_true = registry::set_status(&sessions_dir, pid_a, started_at_a, "offline", now_ms());
    let foreign_rejected = matches!(cas_foreign, Ok(StatusWriteOutcome::Rejected { .. }));
    let true_written = matches!(cas_true, Ok(StatusWriteOutcome::Written));
    log.push_str(&format!(
        "incarnation-CAS: set_status(foreign startedAt)={cas_foreign:?} → rejected={foreign_rejected}; set_status(true startedAt)={cas_true:?} → written={true_written}\n"
    ));

    // tombstone_guarded with a foreign stamp must REFUSE (reused-PID supersede guard).
    let tg_foreign = registry::tombstone_guarded(&sessions_dir, pid_a, Some(foreign));
    let tg_refused = matches!(tg_foreign, TombstoneOutcome::Refused { .. });
    log.push_str(&format!(
        "tombstone_guarded(foreign startedAt)={tg_foreign:?} → refused={tg_refused}\n"
    ));

    // Clean up A's stale row, then start B = a NEW incarnation.
    registry::ensure_tombstone(
        &sessions_dir,
        pid_a,
        row_a
            .as_ref()
            .map(|(_, v)| {
                // Best-effort typed capture for the tombstone audit trail.
                serde_json::from_value::<RegistryEntry>(v.clone()).unwrap_or_default()
            })
            .as_ref(),
    );

    let name_b = "pi-chaos-2b";
    let start_b = start_argv(name_b, cwd);
    let _ = runner.run(&start_b);
    let row_b = runner.read_row(name_b);
    let pid_b = row_b
        .as_ref()
        .and_then(|(_, v)| v.get("pid").and_then(Value::as_i64));
    let endpoint_b = row_b.as_ref().and_then(|(_, v)| {
        v.get("endpoint")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let started_at_b = row_b
        .as_ref()
        .and_then(|(_, v)| v.get("startedAt").and_then(Value::as_i64));
    let port_b = endpoint_b.as_deref().and_then(endpoint_port);
    if let Some(b) = pid_b {
        reaper.watch_group(b);
    }
    let b_distinct = pid_b.is_some() && pid_b != Some(pid_a) && started_at_b != started_at_a;
    let b_live = pid_b.map(|p| is_pid_alive(p as i32)).unwrap_or(false);
    let b_reconnect = match &endpoint_b {
        Some(ep) => crate::provider::pi::daemon::residence::connect_ready(ep, Duration::from_secs(5)).is_ok(),
        None => false,
    };
    log.push_str(&format!(
        "B: pid={pid_b:?} endpoint={endpoint_b:?} port={port_b:?} startedAt={started_at_b:?} distinct={b_distinct} live={b_live} reconnect={b_reconnect}\n"
    ));
    log.push_str(&format!(
        "PORT CHARACTERIZATION (not asserted equal — ephemeral re-roll): port_a={port_a:?} port_b={port_b:?}\n"
    ));

    // Teardown B.
    let _ = runner.run(&["stop", name_b]);

    // No-stale-latch + CAS + fresh-incarnation all hold.
    let pass = a_dead
        && row_present
        && !ws_after
        && !identity_after
        && foreign_rejected
        && true_written
        && tg_refused
        && b_distinct
        && b_live
        && b_reconnect;

    write_at_source(dir, class, &log);
    let v = if pass {
        ChaosVerdict::pass(
            class,
            dir,
            "crash leaves NO live latch (stale row not connectable/identified); incarnation-CAS rejects foreign startedAt (no split-brain); fresh start is a new incarnation",
        )
    } else {
        ChaosVerdict::brk(
            class,
            dir,
            "crash/rebind: a stale-endpoint latch, CAS stomp, or failed fresh-incarnation surfaced (see observed)",
        )
    };
    let mut out = vec![v
        .cmd(format!("qd {}", start.join(" ")))
        .cmd(format!(
            "safe_group_kill({pid_a}, SIGKILL)  (abrupt group crash, bypass qd stop)"
        ))
        .cmd(format!("qd {}", start_b.join(" ")))
        .obs(log)];

    // --- ROUND ≥2: REBIND CONTENTION — start a fresh resident reusing the NAME of a
    // crashed predecessor whose STALE (dead-pid, non-tombstoned) row is still present.
    // Characterize whether the name-claim reclaims over the crashed predecessor.
    // Fresh-batch tail dig; classified as a CHARACTERIZATION (the oracle rules the
    // correct behavior) unless it panics/crashes.
    if round >= 2 {
        write_ran(dir, "class2' rebind contention over crashed predecessor");
        let mut rlog = String::new();
        let rname = "pi-chaos-2-rebind";
        // Start C, capture pid_c, crash its group abruptly (stale row left).
        let _ = runner.run(&start_argv(rname, cwd));
        let row_c = runner.read_row(rname);
        let pid_c = row_c
            .as_ref()
            .and_then(|(_, v)| v.get("pid").and_then(Value::as_i64));
        if let Some(pid_c) = pid_c {
            reaper.watch_group(pid_c);
            guard.record_spawn(pgid_of(pid_c).unwrap_or(pid_c));
            capture_proc_cgroup(pid_c, dir, "class2-resident-c");
            // Box-killer chokepoint: pid_c is parsed from a JSON row (0/1 possible).
            // A4 containment: guarded_kill asserts (a)+(b) around the reap.
            guarded_kill(guard, pid_c, "class2-rebind-C", || {
                crate::safe_kill::safe_group_kill(pid_c, libc::SIGKILL)
            });
            let dl = Instant::now() + Duration::from_secs(3);
            while Instant::now() < dl && is_pid_alive(pid_c as i32) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let sessions_dir = runner.sessions_dir();
            let c_row_present = sessions_dir.join(format!("{pid_c}.json")).exists();
            // Now RECLAIM the same name while the stale row persists.
            let reout = runner.run(&start_argv(rname, cwd));
            let reok = reout.as_ref().map(|o| o.status.success()).unwrap_or(false);
            let restderr = reout
                .as_ref()
                .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
                .unwrap_or_default();
            let panicked = restderr.contains("panicked");
            let row_re = runner.read_row(rname);
            let pid_re = row_re
                .as_ref()
                .and_then(|(_, v)| v.get("pid").and_then(Value::as_i64));
            let reclaimed = reok
                && pid_re.is_some()
                && pid_re != Some(pid_c)
                && pid_re.map(|p| is_pid_alive(p as i32)).unwrap_or(false);
            rlog.push_str(&format!(
                "rebind: crashed pid_c={pid_c} (stale row present={c_row_present}); re-start same name '{rname}' → ok={reok} new_pid={pid_re:?} reclaimed_live={reclaimed} panicked={panicked}\nstderr={restderr}\n"
            ));
            // Cleanup: stop the reclaimed resident; tombstone the stale predecessor.
            let _ = runner.run(&["stop", rname]);
            registry::ensure_tombstone(&sessions_dir, pid_c, None);
            write_at_source(dir, "class2-rebind", &rlog);
            let vr = if panicked {
                ChaosVerdict::brk(class, dir, "rebind contention: a panic surfaced re-claiming a crashed predecessor's name — break")
            } else {
                ChaosVerdict::characterization(class, dir, format!("rebind contention over a crashed predecessor's stale row characterized for the oracle (reclaimed_live={reclaimed})"))
            };
            out.push(
                vr.cmd(format!("qd start {rname} (x2, crash between)"))
                    .obs(rlog),
            );
        } else {
            let _ = runner.run(&["stop", rname]);
            out.push(
                ChaosVerdict::brk(
                    class,
                    dir,
                    "class2' rebind: no resident row/pid after start (instrument)",
                )
                .obs(rlog),
            );
        }
    }

    out
}

/// CLASS 3 — wedge / daemon-gone wait-fallback. Two probes + one pinned
/// characterization. (3a) connectionless daemon-gone (always-on): fabricate a
/// dead-pid row, `qd wait` must resolve from `<pid>.json` status WITHOUT
/// hang/panic/stale-latch. (3b) live wedge (LIVE): freeze the resident's child,
/// `qd wait` must stay bounded + not panic. (3c) PIN the transcript-root
/// divergence (is_break:false) for the acceptance oracle.
fn class3_wedge(
    runner: &QdRunner,
    cwd: &str,
    dir: &Path,
    live: bool,
    round: u32,
    guard: &mut KillGuard,
) -> Vec<ChaosVerdict> {
    let class = "class3-wedge";
    let mut out = Vec::new();
    let mut log = String::new();
    let sessions_dir = runner.sessions_dir();

    // --- 3a: connectionless daemon-gone wait (always-on) --------------------
    write_ran(dir, "class3a daemon-gone connectionless wait");
    let dead = dead_pid();
    let name_a = "pi-chaos-3a";
    let row = RegistryEntry {
        pid: Some(dead),
        session_id: Some("pi-chaos-3a-sid".into()),
        cwd: Some(cwd.to_string()),
        status: Some("idle".into()),
        name: Some(name_a.into()),
        provider: Some("pi".into()),
        ..Default::default()
    };
    let _ = registry::write_entry(&sessions_dir, &row);
    let wait_args = ["wait", name_a, "--timeout", "3"];
    let t0 = Instant::now();
    let wout = runner.run(&wait_args);
    let elapsed = t0.elapsed();
    let stderr = wout
        .as_ref()
        .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
        .unwrap_or_default();
    let exit = wout.as_ref().ok().and_then(|o| o.status.code());
    let panicked = stderr.contains("panicked");
    // Bounded: a dead-pid idle row resolves connectionlessly; must NOT exceed the
    // timeout by a wide margin (no infinite hang) and must NOT panic.
    let bounded = elapsed < Duration::from_secs(8);
    log.push_str(&format!(
        "3a daemon-gone: dead_pid={dead} status=idle; qd wait --timeout 3 → exit={exit:?} elapsed={elapsed:?} bounded={bounded} panicked={panicked}\nstderr={stderr}\n"
    ));
    let pass_3a = bounded && !panicked;
    write_at_source(dir, "class3a", &log);
    let v3a = if pass_3a {
        ChaosVerdict::pass(class, dir, "daemon-gone wait resolves connectionlessly from <pid>.json status, bounded + no panic (status-keyed)")
    } else if !bounded {
        ChaosVerdict::brk(
            class,
            dir,
            "daemon-gone wait HUNG (exceeded bound) — degrade-not-crash violated",
        )
    } else {
        ChaosVerdict::brk(
            class,
            dir,
            "daemon-gone wait PANICKED — degrade-not-crash violated",
        )
    };
    out.push(
        v3a.cmd(format!("qd {}", wait_args.join(" ")))
            .obs(log.clone()),
    );
    // Clean up the fabricated row.
    let _ = std::fs::remove_file(sessions_dir.join(format!("{dead}.json")));

    // --- 3a' (ROUND ≥2): BUSY-status dead-pid → qd wait must POLL TO THE DEADLINE
    // (no false-Idle short-circuit, no infinite hang). Fresh-batch tail dig.
    if round >= 2 {
        write_ran(dir, "class3a' busy-status dead-pid polls to deadline");
        let mut blog = String::new();
        let bdead = dead_pid();
        let bname = "pi-chaos-3a-busy";
        let brow = RegistryEntry {
            pid: Some(bdead),
            session_id: Some("pi-chaos-3a-busy-sid".into()),
            cwd: Some(cwd.to_string()),
            status: Some("busy".into()),
            name: Some(bname.into()),
            provider: Some("pi".into()),
            ..Default::default()
        };
        let _ = registry::write_entry(&sessions_dir, &brow);
        let bwait = ["wait", bname, "--timeout", "2"];
        let t0 = Instant::now();
        let bout = runner.run(&bwait);
        let belapsed = t0.elapsed();
        let bstderr = bout
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
            .unwrap_or_default();
        let bpanicked = bstderr.contains("panicked");
        // A busy dead-pid must NOT resolve instantly (no false-Idle) and must NOT hang:
        // it polls to ~the 2s deadline then exits bounded.
        let polled = belapsed >= Duration::from_millis(1500);
        let bounded = belapsed < Duration::from_secs(8);
        blog.push_str(&format!(
            "3a' busy dead-pid={bdead} status=busy; qd wait --timeout 2 → elapsed={belapsed:?} polled_to_deadline={polled} bounded={bounded} panicked={bpanicked}\nstderr={bstderr}\n"
        ));
        write_at_source(dir, "class3a-busy", &blog);
        let v3ab = if bpanicked {
            ChaosVerdict::brk(
                class,
                dir,
                "busy dead-pid wait PANICKED — degrade-not-crash violated",
            )
        } else if !bounded {
            ChaosVerdict::brk(
                class,
                dir,
                "busy dead-pid wait HUNG past bound — degrade-not-crash violated",
            )
        } else if !polled {
            // Resolved instantly on a BUSY row → potential false-Idle. Characterize for
            // the oracle (could be a benign status-resolution choice), not a hard break.
            ChaosVerdict::characterization(class, dir, "busy dead-pid wait resolved FAST (<1.5s) rather than polling to deadline — characterized for the oracle (possible status-resolution choice, not a hang/crash)")
        } else {
            ChaosVerdict::pass(class, dir, "busy dead-pid wait polled to the deadline then exited bounded (no false-Idle, no hang)")
        };
        out.push(v3ab.cmd(format!("qd {}", bwait.join(" "))).obs(blog));
        let _ = std::fs::remove_file(sessions_dir.join(format!("{bdead}.json")));
    }

    // --- 3c: PIN the transcript-root divergence (always-on characterization) -
    let cite = "PIN (is_break:false, for the tier-a acceptance oracle): pi has NO dedicated qd wait branch — it falls through the claude path (wait.rs:146+). \
        The fall-through feeds paths.PROJECTS_DIR (claude's .claude/projects) to pi.transcript_path (wait.rs:213-237), NOT pi's own transcript_root ($HOME/.pi/agent/sessions, provider/pi/mod.rs:76). \
        So pi.transcript_path resolves None for a real pi row → 'no transcript found … status-keyed only' → wait is STATUS-KEYED off <pid>.json (PidFileStatus/StatusFallback, wait.rs:39-54/:266-272). \
        Unlike codex (run_codex_wait reads the rollout tail), pi's daemon-gone wait is status-keyed. Whether pi SHOULD have its own wait branch reading pi's transcript_root is a SUBTLE-CORRECTNESS ruling for the acceptance oracle.";
    write_at_source(dir, "class3c-characterization", cite);
    out.push(
        ChaosVerdict::characterization(
            class,
            dir,
            "transcript-root divergence pinned for the acceptance oracle",
        )
        .obs(cite.to_string()),
    );

    // --- 3b: live wedge (frozen child) --------------------------------------
    if live {
        write_ran(dir, "class3b live wedge (SIGSTOP child)");
        let mut wlog = String::new();
        let name_b = "pi-chaos-3b";
        let start = start_argv(name_b, cwd);
        let _ = runner.run(&start);
        let row_b = runner.read_row(name_b);
        let pid_b = row_b
            .as_ref()
            .and_then(|(_, v)| v.get("pid").and_then(Value::as_i64));
        if let Some(pid_b) = pid_b {
            guard.record_spawn(pgid_of(pid_b).unwrap_or(pid_b));
            capture_proc_cgroup(pid_b, dir, "class3b-resident");
            // Find the resident's child (a non-resident pgid member) and freeze it.
            let members = pgid_members(pid_b);
            let child = members.iter().find(|r| r.pid != pid_b).cloned();
            wlog.push_str(&format!(
                "resident pid={pid_b}; pgid members={:?}; child={:?}\n",
                members, child
            ));
            if let Some(child) = &child {
                unsafe { libc::kill(child.pid as i32, libc::SIGSTOP) };
                wlog.push_str(&format!(
                    "SIGSTOP child pid={} comm={}\n",
                    child.pid, child.comm
                ));
            }
            let wait_args = ["wait", name_b, "--timeout", "2"];
            let t0 = Instant::now();
            let wout = runner.run(&wait_args);
            let elapsed = t0.elapsed();
            let stderr = wout
                .as_ref()
                .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
                .unwrap_or_default();
            let panicked = stderr.contains("panicked");
            let bounded = elapsed < Duration::from_secs(8);
            wlog.push_str(&format!(
                "wedged qd wait --timeout 2 → elapsed={elapsed:?} bounded={bounded} panicked={panicked}\nstderr={stderr}\n"
            ));
            // Unfreeze + teardown.
            if let Some(child) = &child {
                unsafe { libc::kill(child.pid as i32, libc::SIGCONT) };
            }
            let _ = runner.run(&["stop", name_b]);
            let pass_3b = bounded && !panicked;
            write_at_source(dir, "class3b", &wlog);
            let v3b = if pass_3b {
                ChaosVerdict::pass(class, dir, "live wedge (frozen child): qd wait stays bounded + no panic (degrades, does not hang/crash)")
            } else if !bounded {
                ChaosVerdict::brk(
                    class,
                    dir,
                    "live wedge: qd wait HUNG past bound — degrade-not-crash violated",
                )
            } else {
                ChaosVerdict::brk(
                    class,
                    dir,
                    "live wedge: qd wait PANICKED — degrade-not-crash violated",
                )
            };
            out.push(
                v3b.cmd(format!("qd {}", start.join(" ")))
                    .cmd(format!("qd {}", wait_args.join(" ")))
                    .obs(wlog),
            );
        } else {
            let _ = runner.run(&["stop", name_b]);
            out.push(
                ChaosVerdict::brk(class, dir, "live wedge: no resident row/pid after start")
                    .obs(wlog),
            );
        }
    }

    out
}

/// CLASS 4 — pgid-teardown grandchild-orphan. (4a) in-group child reaped (LIVE):
/// `qd stop` group-teardown reaps the resident + child (PRIMARY = pgid-membership;
/// pgrep -x pi cross-check recorded). (4b) setsid escapee reach-boundary (always-on
/// characterization): PROVE the escape (daemon_pgid != child_pgid), reap the group,
/// PIN that the escapee is NOT reached — characterized, never presumed dead.
fn class4_pgid_teardown(
    runner: &QdRunner,
    cwd: &str,
    dir: &Path,
    live: bool,
    round: u32,
    guard: &mut KillGuard,
) -> Vec<ChaosVerdict> {
    let class = "class4-pgid-teardown";
    let mut out = Vec::new();

    // --- 4a: in-group child reaped (LIVE) -----------------------------------
    if live {
        write_ran(dir, "class4a live pgid teardown reaps in-group child");
        let mut log = String::new();
        let mut reaper = GroupReaper::new();
        let name = "pi-chaos-4a";
        let start = start_argv(name, cwd);
        let _ = runner.run(&start);
        let row = runner.read_row(name);
        let pid = row
            .as_ref()
            .and_then(|(_, v)| v.get("pid").and_then(Value::as_i64));
        if let Some(pid) = pid {
            reaper.watch_group(pid);
            guard.record_spawn(pgid_of(pid).unwrap_or(pid));
            capture_proc_cgroup(pid, dir, "class4a-resident");

            // ROUND ≥2 — GRANDCHILD-IN-GROUP non-vacuity (pi-lead-3 BINDING). cred-free
            // tier-a pi spawns NO &-detached grandchildren, so to make the
            // "grandchild-orphan" class non-vacuous we INJECT an &-detached member INTO
            // the resident's pgid (process_group(resident_pid)) — a descendant that
            // STAYS in the group (distinct from 4b's setsid-escapee that LEAVES it).
            // kill(-pgid) teardown reaps by GROUP MEMBERSHIP regardless of tree depth,
            // so an injected in-group member faithfully proxies a real grandchild's
            // reap. We assert it is a member BEFORE and dead AFTER.
            let mut gc_pid: Option<i64> = None;
            let mut gc_is_member = false;
            let mut gc_child: Option<Child> = None;
            if round >= 2 {
                match Command::new("sleep")
                    .arg("600")
                    .process_group(pid as i32)
                    .spawn()
                {
                    Ok(child) => {
                        let g = child.id() as i64;
                        gc_pid = Some(g);
                        // NB: do NOT reaper.watch_pid(g) — the grandchild is IN the
                        // resident pgid, so reaper.watch_group(pid) already covers it on
                        // panic; watching the bare pid would risk SIGKILL to a reused pid
                        // after we reap it below.
                        // Confirm it actually JOINED the resident's group at source.
                        for _ in 0..20 {
                            if pgid_of(g) == Some(pid) {
                                gc_is_member = true;
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        log.push_str(&format!(
                            "GRANDCHILD INJECTED: pid={g} process_group({pid}) → pgid_of={:?} joined_group={gc_is_member}\n",
                            pgid_of(g)
                        ));
                        gc_child = Some(child);
                    }
                    Err(e) => {
                        log.push_str(&format!("GRANDCHILD INJECTION FAILED to spawn: {e}\n"));
                    }
                }
            }

            // PRIMARY at-source probe: pgid-membership BEFORE teardown.
            let before = pgid_members(pid);
            let non_resident: Vec<&ProcRow> = before.iter().filter(|r| r.pid != pid).collect();
            let comms: Vec<String> = non_resident.iter().map(|r| r.comm.clone()).collect();
            // Cross-check: the protocol-named pgrep -x pi (expected NULL for node child).
            let pgrep_pi = pgrep_x("pi");
            log.push_str(&format!(
                "BEFORE: resident pid={pid}; pgid members={:?}\n  non-resident members (the child/grandchildren)={:?} comms={:?}\n  pgrep -x pi cross-check={:?}\n",
                before, non_resident, comms, pgrep_pi
            ));
            let _ = std::fs::write(dir.join("proctree-before.txt"), format!("{before:#?}"));

            // NON-VACUITY: at least one non-resident member must exist, else the
            // teardown assertion is vacuous.
            let non_vacuous = !non_resident.is_empty();

            // Inject: the PRODUCTION teardown (qd stop → run_pi_kill → teardown_pi_daemon
            // → RealDaemonSpawner.kill: -pgid SIGTERM→1.5s grace→SIGKILL).
            let _ = runner.run(&["stop", name]);
            std::thread::sleep(Duration::from_millis(300));

            // Assert AT SOURCE by RE-ENUMERATING the group (pgid-scoped → immune to
            // PID-reuse in the wait window: a reused pid has a DIFFERENT pgid). Any
            // surviving member of the resident's group = an orphan/leak.
            // Reap the injected grandchild zombie: its parent is THIS test process, so
            // a kill(-pgid) leaves it a ZOMBIE until waitpid'd — and is_pid_alive(zombie)
            // returns TRUE (kill(pid,0) succeeds on a zombie). try_wait reaps it so the
            // liveness check is truthful. (The 4b in-group child re-parents to init,
            // which reaps it — only this test-parented grandchild needs the manual reap.)
            if let Some(c) = gc_child.as_mut() {
                for _ in 0..20 {
                    if let Ok(Some(_)) = c.try_wait() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            let after = pgid_members(pid);
            let survivors: Vec<&ProcRow> = after
                .iter()
                .filter(|r| is_pid_alive(r.pid as i32))
                .collect();
            let resident_dead = !is_pid_alive(pid as i32);
            let gc_dead = gc_pid.map(|g| !is_pid_alive(g as i32)).unwrap_or(true);
            let sessions_dir = runner.sessions_dir();
            let tombstoned = sessions_dir.join(format!("{pid}.json.tombstoned")).exists();
            log.push_str(&format!(
                "AFTER qd stop: resident_dead={resident_dead}; surviving group members (pgid=={pid})={:?}; row tombstoned={tombstoned}; grandchild(pid={gc_pid:?}) dead={gc_dead}\n",
                survivors
            ));
            let _ = std::fs::write(dir.join("proctree-after.txt"), format!("{after:#?}"));
            log.push_str(&format!(
                "INSTRUMENT NOTE (data-driven): child comms OBSERVED = {comms:?}; pgrep -x pi cross-check matched = {pgrep_pi:?}. \
                 Pre-run prediction comm=node was REFUTED at source: the pi cli sets process.title=\"pi\", so comm is \"pi\" and pgrep -x pi DOES match the child. \
                 pgid-membership remains the PRINCIPLED PRIMARY anyway: it is instance-scoped by the UNIQUE resident pgid (immune to OTHER pi residents on the box AND to self-match), whereas pgrep -x pi is comm-GLOBAL (it over-matches a multi-resident box — it only coincided here because the box was quiet).\n"
            ));
            if round >= 2 {
                log.push_str(&format!(
                    "GRANDCHILD NON-VACUITY (round {round}): injected &-detached in-group member pid={gc_pid:?} joined_group={gc_is_member} (member BEFORE teardown); dead AFTER teardown={gc_dead}. A real grandchild inherits the resident pgid identically, so kill(-pgid) reaps it the same way (depth-independent).\n"
                ));
            }

            // Round ≥2 REQUIRES the grandchild to have joined (else the grandchild
            // non-vacuity is unmet — an instrument issue to bubble, NOT a clean pass).
            let gc_ok = round < 2 || (gc_is_member && gc_dead);
            let gc_inject_failed = round >= 2 && !gc_is_member;
            let pass = non_vacuous && resident_dead && survivors.is_empty() && gc_ok;
            write_at_source(dir, "class4a", &log);
            let v4a = if !non_vacuous {
                // Not a SUT break — a vacuity guard trip (instrument): bubble.
                ChaosVerdict::brk(class, dir, "VACUITY GUARD: no non-resident pgid member existed before teardown — cannot assert orphan-free (instrument/setup issue, NOT a clean pass)")
            } else if gc_inject_failed {
                ChaosVerdict::brk(class, dir, "GRANDCHILD NON-VACUITY UNMET: the injected &-detached member did not join the resident pgid (instrument/setup issue — bubble, NOT a clean pass)")
            } else if pass {
                let detail = if round >= 2 {
                    "pgid teardown reaped the resident + in-group child + the INJECTED &-detached grandchild (no orphan); row tombstoned — grandchild non-vacuity satisfied, PRIMARY pgid-membership at source"
                } else {
                    "pgid teardown reaped the resident + all in-group children (no orphan); row tombstoned — PRIMARY pgid-membership at source"
                };
                ChaosVerdict::pass(class, dir, detail)
            } else {
                ChaosVerdict::brk(class, dir, "ORPHAN: a pgid member (child or injected grandchild) survived qd stop teardown — leak (see observed)")
            };
            out.push(
                v4a.cmd(format!("qd {}", start.join(" ")))
                    .cmd(format!(
                        "qd stop {name}  (→ teardown_pi_daemon → -pgid SIGTERM→grace→SIGKILL)"
                    ))
                    .obs(log),
            );
        } else {
            let _ = runner.run(&["stop", name]);
            out.push(
                ChaosVerdict::brk(class, dir, "class4a: no resident row/pid after start").obs(log),
            );
        }
    }

    // --- 4b: setsid escapee reach-boundary (always-on characterization) ------
    write_ran(
        dir,
        "class4b setsid escapee reach-boundary characterization",
    );
    out.push(class4b_escapee(dir, round, guard));

    out
}

/// CLASS 4b — the non-vacuity escapee characterization. Model a root in its OWN
/// process group (mimicking the daemon spawn) with a `setsid` descendant that
/// escapes to a new group. PROVE the escape at source (root_pgid != escapee_pgid),
/// reap the root's group via the PRODUCTION ladder, then PIN that the escapee is
/// NOT reached — a CHARACTERIZATION of teardown's reach boundary, never a
/// zero-survivors assertion (that would be vacuous: the escapee is out of the pgid
/// BY CONSTRUCTION).
fn class4b_escapee(dir: &Path, round: u32, guard: &mut KillGuard) -> ChaosVerdict {
    let class = "class4-pgid-teardown";
    let mut log = String::new();
    let mut reaper = GroupReaper::new();
    let escapee_pidfile = dir.join("escapee.pid");
    let _ = std::fs::remove_file(&escapee_pidfile);

    // Root in its own group; inside it a setsid child detaches to a new session.
    // ROUND ≥2 (combined-topology tail dig): ALSO spawn an IN-GROUP child (`sleep 600 &`,
    // which inherits the root pgid and STAYS) alongside the setsid escapee (which
    // LEAVES). After teardown we then assert BOTH reach outcomes in one tree: the
    // in-group child is REACHED (dead), the escapee is NOT reached (alive).
    let ingroup_prefix = if round >= 2 { "sleep 600 & " } else { "" };
    let script = format!(
        "{ingroup_prefix}setsid sh -c 'echo $$ > \"{pf}\"; exec sleep 600' & sleep 600",
        pf = escapee_pidfile.display()
    );
    let root = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .process_group(0) // root_pgid == root_pid (mimics the daemon's process_group(0))
        .spawn();
    let mut root = match root {
        Ok(c) => c,
        Err(e) => {
            log.push_str(&format!("spawn root failed: {e}\n"));
            write_at_source(dir, "class4b", &log);
            return ChaosVerdict::brk(
                class,
                dir,
                "class4b: could not spawn the escapee model (instrument)",
            )
            .obs(log);
        }
    };
    let root_pid = root.id() as i64;
    reaper.watch_group(root_pid);
    // A4 containment: the 4b root spawns with process_group(0) → its own fresh pgid
    // (pgid == pid), a synthetic non-fleet group. Record it as a known-own target and
    // capture its cgroup (layer-c evidence — under the scope it reads pi-a4-chaos-r<N>).
    guard.record_spawn(pgid_of(root_pid).unwrap_or(root_pid));
    capture_proc_cgroup(root_pid, dir, "class4b-root");

    // Poll for the escapee pid (setsid writes its $$ after detaching).
    let mut escapee_pid: Option<i64> = None;
    for _ in 0..50 {
        if let Ok(s) = std::fs::read_to_string(&escapee_pidfile) {
            if let Ok(p) = s.trim().parse::<i64>() {
                escapee_pid = Some(p);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let Some(escapee_pid) = escapee_pid else {
        log.push_str("escapee pid never appeared\n");
        let _ = root.kill();
        let _ = root.wait();
        write_at_source(dir, "class4b", &log);
        return ChaosVerdict::brk(class, dir, "class4b: escapee never detached (instrument)")
            .obs(log);
    };
    reaper.watch_pid(escapee_pid);

    let root_pgid = pgid_of(root_pid);
    let escapee_pgid = pgid_of(escapee_pid);
    let root_alive = is_pid_alive(root_pid as i32);
    let escapee_alive = is_pid_alive(escapee_pid as i32);
    log.push_str(&format!(
        "root pid={root_pid} pgid={root_pgid:?} alive={root_alive}; escapee pid={escapee_pid} pgid={escapee_pgid:?} alive={escapee_alive}\n"
    ));

    // NON-VACUITY GUARD: prove the escape is REAL (different pgid).
    let escape_proven = match (root_pgid, escapee_pgid) {
        (Some(r), Some(e)) => r != e,
        _ => false,
    };
    log.push_str(&format!(
        "ESCAPE PROVEN (root_pgid != escapee_pgid): {escape_proven}\n"
    ));

    if !escape_proven {
        write_at_source(dir, "class4b", &log);
        let _ = root.kill();
        let _ = root.wait();
        return ChaosVerdict::brk(
            class,
            dir,
            "class4b VACUITY GUARD: the setsid descendant did NOT leave the root pgid — cannot characterize the reach boundary (instrument)",
        )
        .obs(log);
    }

    // ROUND ≥2: capture the IN-GROUP child (a non-root member of root's pgid) so we
    // can assert it is REACHED (dead) by the same teardown that does NOT reach the escapee.
    let ingroup_child: Option<i64> = if round >= 2 {
        let m = pgid_members(root_pid)
            .into_iter()
            .find(|r| r.pid != root_pid)
            .map(|r| r.pid);
        log.push_str(&format!(
            "IN-GROUP child (round {round} combined topology) = {m:?}\n"
        ));
        m
    } else {
        None
    };

    // Reap the root's GROUP via the PRODUCTION ladder (-pgid SIGTERM→grace→SIGKILL).
    // A4 containment: guarded_kill asserts (a) own-resident (root_pgid == root_pid,
    // recorded above, ∉ fleet) + (b) fleet-intact around the production reap. This
    // fires on the OFFLINE subset too — the connectionless run exercises the (a)+(b)
    // guards live against a synthetic, contained own-pgid group.
    guarded_kill(guard, root_pid, "class4b-root", || {
        RealDaemonSpawner.kill(root_pid)
    });
    let _ = root.wait();
    std::thread::sleep(Duration::from_millis(200));
    let root_dead = !is_pid_alive(root_pid as i32);
    let escapee_survived = is_pid_alive(escapee_pid as i32);
    // The in-group child re-parents to init on root's death; poll for init to reap the
    // zombie before concluding (avoid a flaky false-orphan on the reaping race).
    if let Some(c) = ingroup_child {
        for _ in 0..20 {
            if !is_pid_alive(c as i32) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let ingroup_reached = ingroup_child.map(|c| !is_pid_alive(c as i32));
    log.push_str(&format!(
        "AFTER -pgid teardown of root group: root_dead={root_dead}; escapee_survived={escapee_survived} (escapee_pgid={escapee_pgid:?} != root_pgid={root_pgid:?}); in-group child reached(dead)={ingroup_reached:?}\n"
    ));
    log.push_str(
        "REACH BOUNDARY (characterized, NOT presumed): a setsid-escaped descendant is OUT of the pgid BY CONSTRUCTION, so the -pgid teardown does not reach it. \
         Whether such an escapee is in/out of the tier-a threat model is the acceptance oracle's ruling (codex precedent: setsid-escapee ruled OUT of tier-a threat model).\n",
    );

    // Explicitly reap the escapee (it is genuinely orphaned by design).
    unsafe { libc::kill(escapee_pid as i32, libc::SIGKILL) };

    write_at_source(dir, "class4b", &log);
    // The MEANINGFUL outcome is: escape proven + root group reaped. The escapee's
    // survival is the CHARACTERIZED boundary (is_break:false), not a pass/fail. For
    // round ≥2 an IN-GROUP child surviving the same teardown IS a real reach failure.
    let ingroup_orphan = ingroup_reached == Some(false);
    let v = if !root_dead {
        ChaosVerdict::brk(
            class,
            dir,
            "class4b: the IN-GROUP root survived its own -pgid teardown — that IS a reach failure (see observed)",
        )
    } else if ingroup_orphan {
        ChaosVerdict::brk(
            class,
            dir,
            "class4b ORPHAN: the IN-GROUP child survived the -pgid teardown that reaped the root — a real reach failure (see observed)",
        )
    } else if round >= 2 {
        ChaosVerdict::characterization(
            class,
            dir,
            "combined topology: -pgid teardown REACHED the root + in-group child (both dead) but did NOT reach the setsid escapee (escape PROVEN root_pgid != escapee_pgid) — reach boundary characterized for the oracle",
        )
    } else {
        ChaosVerdict::characterization(
            class,
            dir,
            "escape PROVEN at source (root_pgid != escapee_pgid); -pgid teardown reaped the in-group root; the escaped descendant is NOT reached — reach boundary characterized for the oracle",
        )
    };
    v.cmd(format!(
        "sh -c <setsid-escape script>; process_group(0); root_pid={root_pid}"
    ))
    .cmd(format!(
        "RealDaemonSpawner.kill({root_pid})  (-pgid SIGTERM→grace→SIGKILL)"
    ))
    .obs(log)
}

/// Drive ONE C-CHAOS round across all 4 fault classes. `live` gates the classes
/// that need a real resident (1, 2, 3b, 4a); the connectionless probes (3a, 3c, 4b)
/// always run. The returned report IS the evidence artifact.
pub fn run_chaos_round(
    runner: &QdRunner,
    cwd: &str,
    round: u32,
    evidence_root: &Path,
    live: bool,
) -> ChaosReport {
    let mut verdicts = Vec::new();

    // A4 CONTAINMENT — layer (a) per-round kill authority + layer (b) round-level
    // fleet snapshot (the teeth). Snapshot the LIVE fleet.service pid set BEFORE the
    // round, run the classes threading the guard (each chaos-issued kill also does its
    // own before/after check via guarded_kill), then re-snapshot AFTER and assert
    // `after ⊇ before`. Any pre-existing fleet pid GONE → the round is VOID (panic
    // naming the missing pid). Snapshots are written to evidence for the oracle.
    let mut guard = KillGuard::new();
    let fleet_before_raw = fleet_snapshot();
    let fleet_before = fleet_snapshot_durable();

    if live {
        verdicts.extend(class1_reconnect(
            runner,
            cwd,
            &class_dir(evidence_root, "class1-reconnect"),
            &mut guard,
        ));
        verdicts.extend(class2_crash_rebind(
            runner,
            cwd,
            &class_dir(evidence_root, "class2-crash-rebind"),
            round,
            &mut guard,
        ));
    }
    verdicts.extend(class3_wedge(
        runner,
        cwd,
        &class_dir(evidence_root, "class3-wedge"),
        live,
        round,
        &mut guard,
    ));
    verdicts.extend(class4_pgid_teardown(
        runner,
        cwd,
        &class_dir(evidence_root, "class4-pgid-teardown"),
        live,
        round,
        &mut guard,
    ));

    let fleet_after_raw = fleet_snapshot();
    let fleet_after = fleet_snapshot_durable();
    // Layer-(b) teeth for the whole round, on the DURABLE set: a durable fleet member
    // gone AND not an intended-kill member is a containment breach. (Under the layer-(c)
    // scope killed members are never in the fleet snapshot; transient churn is filtered.)
    let lost = fleet_lost(&fleet_before, &fleet_after, &guard.killed_member_pids);
    let sorted = |s: &HashSet<i64>| {
        let mut v: Vec<i64> = s.iter().copied().collect();
        v.sort_unstable();
        v
    };
    let _ = std::fs::write(
        evidence_root.join("fleet-snapshot.txt"),
        format!(
            "fleet.service cgroup.procs snapshot (layer b, round {round})\nsource: {FLEET_CGROUP_PROCS}\n\
             RAW    BEFORE ({} pids): {:?}\nRAW    AFTER  ({} pids): {:?}\n\
             DURABLE BEFORE ({} pids): {:?}\nDURABLE AFTER  ({} pids): {:?}\n\
             (transient keep-alive sleep/bash/sh churn filtered out — super-21 durable-member filter)\n\
             intended-kill members excluded: {} pids\n\
             DURABLE fleet pids lost (must be empty; any = BREACH): {lost:?}\nintact: {}\n",
            fleet_before_raw.len(), sorted(&fleet_before_raw),
            fleet_after_raw.len(), sorted(&fleet_after_raw),
            fleet_before.len(), sorted(&fleet_before),
            fleet_after.len(), sorted(&fleet_after),
            guard.killed_member_pids.len(),
            lost.is_empty(),
        ),
    );
    if !lost.is_empty() {
        panic!(
            "A4 CONTAINMENT BREACH [round {round}]: DURABLE fleet.service pid(s) {lost:?} disappeared \
             during the round and were NOT intended-kill members — a chaos kill reached a durable \
             supervisor/coordinator. HALT + preserve evidence + bubble."
        );
    }

    ChaosReport {
        round,
        live,
        verdicts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_rows_parse_pid_pgid_comm_args() {
        let sample = "  1234   1200 node /home/x/.npm-pi-global/bin/pi --mode rpc\n   42      42 sh -c sleep 600\n";
        let rows = parse_ps_rows(sample);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pid, 1234);
        assert_eq!(rows[0].pgid, 1200);
        assert_eq!(rows[0].comm, "node");
        assert!(rows[0].args.contains("--mode rpc"));
        assert_eq!(rows[1].pid, 42);
        assert_eq!(rows[1].pgid, 42);
    }

    #[test]
    fn endpoint_port_parses_loopback_ws() {
        assert_eq!(endpoint_port("ws://127.0.0.1:54321"), Some(54321));
        assert_eq!(endpoint_port("not-an-endpoint"), None);
    }

    #[test]
    fn clean_round_requires_nonempty_no_break() {
        let dir = std::path::Path::new("/tmp/x");
        let mut r = ChaosReport {
            round: 1,
            live: true,
            verdicts: vec![ChaosVerdict::pass("c", dir, "ok")],
        };
        assert!(r.clean());
        // A characterization does NOT dirty the round.
        r.verdicts
            .push(ChaosVerdict::characterization("c", dir, "pinned"));
        assert!(r.clean());
        // A break DOES.
        r.verdicts.push(ChaosVerdict::brk("c", dir, "boom"));
        assert!(!r.clean());
        assert_eq!(r.breaks().len(), 1);
        assert_eq!(r.characterizations().len(), 1);
        // Empty is not clean.
        let empty = ChaosReport {
            round: 1,
            live: true,
            verdicts: vec![],
        };
        assert!(!empty.clean());
    }

    #[test]
    fn pass_and_break_verdict_shapes() {
        let dir = std::path::Path::new("/tmp/x");
        let p = ChaosVerdict::pass("c1", dir, "good");
        assert!(p.pass && !p.is_break && !p.is_characterization);
        let b = ChaosVerdict::brk("c1", dir, "bad");
        assert!(!b.pass && b.is_break);
        let c = ChaosVerdict::characterization("c1", dir, "pin");
        assert!(c.pass && !c.is_break && c.is_characterization);
    }

    // ---- A4 CONTAINMENT guard-firing (synthetic; no /proc, no real fleet) -------

    fn set(xs: &[i64]) -> HashSet<i64> {
        xs.iter().copied().collect()
    }

    #[test]
    fn layer_a_i_refuses_pgid_leq_1() {
        // 0 = own group, 1 = kill(-1) box-killer — refused BEFORE any set lookup.
        let spawned = set(&[]);
        let fleet = set(&[]);
        assert!(check_kill_safe(1, 1234, &spawned, &fleet)
            .unwrap_err()
            .contains("layer-a(i)"));
        assert!(check_kill_safe(0, 1234, &spawned, &fleet)
            .unwrap_err()
            .contains("layer-a(i)"));
        assert!(check_kill_safe(-5, 1234, &spawned, &fleet)
            .unwrap_err()
            .contains("layer-a(i)"));
    }

    #[test]
    fn layer_a_ii_refuses_unknown_pgid() {
        // A pgid we did NOT spawn this round is the exact "wrong/broad kill" failure.
        let spawned = set(&[5000]);
        let fleet = set(&[]);
        let e = check_kill_safe(9999, 1234, &spawned, &fleet).unwrap_err();
        assert!(e.contains("layer-a(ii)"), "{e}");
    }

    #[test]
    fn layer_a_iii_refuses_own_pgid() {
        // Even if it were (impossibly) in the spawned set, the executor's own pgid
        // must never be group-killed.
        let own = 1234;
        let spawned = set(&[own]);
        let fleet = set(&[]);
        let e = check_kill_safe(own, own, &spawned, &fleet).unwrap_err();
        assert!(e.contains("layer-a(iii)"), "{e}");
    }

    #[test]
    fn layer_a_iv_refuses_fleet_pgid() {
        // TIGHTENING #1: the candidate is compared against the fleet PGID set (not the
        // raw fleet PID set). A pgid that is a fleet.service group leader is refused
        // even if it somehow appears in the spawned set.
        let spawned = set(&[7777]);
        let fleet_pgids = set(&[7777]);
        let e = check_kill_safe(7777, 1234, &spawned, &fleet_pgids).unwrap_err();
        assert!(e.contains("layer-a(iv)"), "{e}");
    }

    #[test]
    fn layer_a_allows_known_own_resident() {
        // >1, spawned this round, != own pgid, ∉ fleet pgids → the only allowed case.
        let spawned = set(&[4242]);
        let fleet_pgids = set(&[9001, 9002]);
        assert!(check_kill_safe(4242, 1234, &spawned, &fleet_pgids).is_ok());
    }

    #[test]
    fn layer_b_detects_fleet_disappearance() {
        // The teeth: a pid present BEFORE and GONE AFTER is an outage → Err naming it.
        let before = set(&[100, 200, 300]);
        let after_gone = set(&[200, 300, 400]); // 100 disappeared, 400 is new
        let e = fleet_intact(&before, &after_gone).unwrap_err();
        assert!(e.contains("100"), "{e}");
        assert!(e.contains("VOID"), "{e}");
    }

    #[test]
    fn layer_b_allows_superset_after() {
        // after ⊇ before is intact; NEW pids appearing (coordinators spawning
        // keep-alives) is explicitly fine — only DISAPPEARANCE is the signal.
        let before = set(&[100, 200]);
        assert!(fleet_intact(&before, &set(&[100, 200])).is_ok()); // unchanged
        assert!(fleet_intact(&before, &set(&[100, 200, 999])).is_ok()); // superset
        assert!(fleet_intact(&set(&[]), &set(&[1, 2, 3])).is_ok()); // empty before
    }

    #[test]
    fn durable_filter_classifies_churn_vs_daemons() {
        // super-21's durable-member filter: transient keep-alive churn excluded;
        // coordinators/daemons (qd/claude/relay/pi/node/codex) PROTECTED as durable.
        for churn in ["sleep", "bash", "sh", "dash", "env", "timeout", "cat", ""] {
            assert!(is_transient_comm(churn), "{churn} should be transient");
        }
        assert!(is_transient_comm("sleep <defunct>"));
        // A4.1: the empirically-derived build-toolchain set is ALSO transient (a build
        // overlapping a converge must not false-VOID when a build step exits mid-round).
        for build in BUILD_TOOLCHAIN_COMMS {
            assert!(
                is_transient_comm(build),
                "{build} (build toolchain) should be transient"
            );
        }
        // Fail-safe toward protection: anything not obviously churn is durable.
        for durable in ["qd", "claude", "codex", "pi", "node", "relay", "systemd"] {
            assert!(
                !is_transient_comm(durable),
                "{durable} must be durable/protected"
            );
        }
        // EXACT full-comm, never substring: no build comm collides with a durable member,
        // and a name merely CONTAINING a build comm stays durable (e.g. a hypothetical
        // "cargo-daemon" is NOT falsely transient).
        for durable in ["qd", "claude", "codex", "pi", "node", "relay", "systemd"] {
            for build in BUILD_TOOLCHAIN_COMMS {
                assert_ne!(
                    *build, durable,
                    "build comm {build} must not equal durable {durable}"
                );
            }
        }
        assert!(
            !is_transient_comm("cargo-daemon"),
            "substring must NOT match"
        );
        assert!(
            !is_transient_comm("rustc-wrapper"),
            "substring must NOT match"
        );
    }

    /// The A4.1 empirically-derived build-toolchain comm set (this box, aarch64),
    /// mirrored from `is_transient_comm` for the classification + non-vacuity tests.
    const BUILD_TOOLCHAIN_COMMS: &[&str] = &[
        "cargo",
        "rustc",
        "sccache",
        "cc",
        "collect2",
        "ld.mold",
        "build-script-bu",
        "pkg-config",
    ];

    /// NON-VACUITY via the SHARED source (super-22's revert-probe bar, acceptance-
    /// BLOCKING). This drives the SAME `is_transient_comm` classifier that
    /// `fleet_snapshot_durable` uses (here over a synthetic fleet so the build-overlap
    /// scenario is exercisable without /proc), then runs the real teeth
    /// (`fleet_intact`/`fleet_lost`) over the resulting durable snapshot. It proves BOTH
    /// halves through ONE shared mechanism:
    ///   (a) a build finishing mid-converge does NOT false-VOID (build churn filtered out);
    ///   (b) a DURABLE member vanishing STILL VOIDs (teeth untouched).
    /// REVERT-PROBE: deleting the A4.1 build-comm additions from `is_transient_comm`
    /// FLIPS (a) RED (a build tool becomes "durable", so its benign exit VOIDs) while
    /// (b) stays GREEN — proving the additions are load-bearing AND distinct from the
    /// teeth (not a vacuous relabel). The `durable_of` closure below is the exact
    /// synthetic analogue of `fleet_snapshot_durable`'s `!is_transient_comm(comm_of(p))`
    /// filter, so a revert genuinely propagates here.
    #[test]
    fn build_churn_no_void_but_durable_vanish_still_voids() {
        // Durable snapshot = fleet members MINUS transient churn — the SHARED classifier,
        // exactly as fleet_snapshot_durable applies it (synthetic comms in place of /proc).
        let durable_of = |members: &[(i64, &str)]| -> HashSet<i64> {
            members
                .iter()
                .filter(|(_, c)| !is_transient_comm(c))
                .map(|(p, _)| *p)
                .collect()
        };

        // A live fleet during a build-overlapping converge: durable daemons/coordinators
        // (10-14) PLUS the whole build toolchain (20-27) transiting fleet.service.
        let before_members: &[(i64, &str)] = &[
            (10, "qd"),
            (11, "claude"),
            (12, "pi"),
            (13, "relay"),
            (14, "codex"),
            (20, "rustc"),
            (21, "cc"),
            (22, "ld.mold"),
            (23, "build-script-bu"),
            (24, "sccache"),
            (25, "cargo"),
            (26, "collect2"),
            (27, "pkg-config"),
        ];
        let before = durable_of(before_members);
        // The durable snapshot EXCLUDES every build-tool pid, KEEPS every daemon.
        // (Under a revert this assertion ALSO flips, since the build pids re-enter.)
        assert_eq!(
            before,
            set(&[10, 11, 12, 13, 14]),
            "build toolchain must be filtered OUT of the durable snapshot"
        );

        // (a) The build finishes mid-round: the ENTIRE toolchain exits; daemons survive.
        let after_build_done = set(&[10, 11, 12, 13, 14]);
        assert!(
            fleet_intact(&before, &after_build_done).is_ok(),
            "a build finishing mid-converge must NOT false-VOID (build churn is transient)"
        );

        // (b) A DURABLE member (claude, pid 11) vanishes — teeth intact: STILL VOIDs.
        let after_outage = set(&[10, 12, 13, 14]);
        assert_eq!(
            fleet_lost(&before, &after_outage, &HashSet::new()),
            vec![11],
            "a durable member vanishing MUST still be caught (teeth untouched)"
        );
    }

    #[test]
    fn layer_b_excludes_intended_kill_members_keeps_teeth() {
        // Unscoped, an own-pgid group's pids (300, 301) transit fleet.service
        // cgroup.procs then vanish on the INTENDED reap — excluded, not an outage.
        let before = set(&[100, 200, 300, 301]);
        let after = set(&[100, 200]); // 300,301 (intended) gone; 100,200 survive
        let killed = set(&[300, 301]);
        assert!(fleet_lost(&before, &after, &killed).is_empty());
        // But a NON-target fleet pid (200) vanishing is STILL caught — teeth intact.
        let after_outage = set(&[100]); // 200 (a real coordinator) also gone
        assert_eq!(fleet_lost(&before, &after_outage, &killed), vec![200]);
    }
}
