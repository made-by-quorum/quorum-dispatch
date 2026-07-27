//! Injected-effects seam (spec §3).
//!
//! Deciders never touch the environment, the clock, or the process table
//! directly — they take plain data. These traits are how gather functions get
//! that data; tests substitute fixtures. Filesystem access is NOT a trait:
//! registry/jsonl modules take injected **root paths** (see [`crate::paths`])
//! and use std fs against them, which keeps tests hermetic via temp dirs
//! (lesson L9a: HOME is load-bearing; nothing here resolves the real home).

use crate::exec::Exec;
use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

/// Environment-variable + uid access (TS reads `process.env` / `process.getuid()`,
/// src/utils.ts:25-32).
pub trait Env {
    fn var(&self, key: &str) -> Option<String>;
    fn uid(&self) -> u32;
}

/// Blanket impl so a `&dyn Env` (or any `&T: Env`) itself satisfies `impl Env`.
/// The A2 create path holds the env as `&dyn Env` but calls launch.rs helpers
/// that take `&impl Env` (M1-frozen signatures) — this bridge lets the dyn ref
/// be passed straight through with no shim.
impl<T: Env + ?Sized> Env for &T {
    fn var(&self, key: &str) -> Option<String> {
        (**self).var(key)
    }
    fn uid(&self) -> u32 {
        (**self).uid()
    }
}

/// The real process environment.
pub struct RealEnv;

impl Env for RealEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
    fn uid(&self) -> u32 {
        // SAFETY: getuid is always safe to call.
        unsafe { libc::getuid() }
    }
}

/// Fixture env for tests: a plain map + a fixed uid.
#[derive(Default)]
pub struct MapEnv {
    pub vars: HashMap<String, String>,
    pub uid: u32,
}

impl Env for MapEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }
    fn uid(&self) -> u32 {
        self.uid
    }
}

/// Clock seam — TS uses `Date.now()` for relativeTime and stray badges.
pub trait Clock {
    fn now_ms(&self) -> i64;
}

pub struct RealClock;

impl Clock for RealClock {
    fn now_ms(&self) -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// Fixed clock for tests.
pub struct FixedClock(pub i64);

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

/// One process row, for stray attribution (spec §7).
#[derive(Debug, Clone, PartialEq)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
    /// Full command line as `ps` reports it.
    pub cmd: String,
    /// Process cwd when discoverable (best-effort; None on failure).
    pub cwd: Option<String>,
    /// Process start time, epoch ms, when discoverable.
    pub started_ms: Option<i64>,
}

/// Process-table seam.
///
/// TS shells out to `ps -eo pid=,ppid=` for ancestry walks
/// (src/session.ts:609-645, 845-873) and `kill -0` for liveness
/// (src/utils.ts:380-388). The decider side consumes the returned maps; only
/// gather functions call this.
pub trait ProcessTable {
    /// pid → ppid for every visible process.
    fn ppid_map(&self) -> io::Result<HashMap<i32, i32>>;
    /// Liveness check (`kill(pid, 0)` probe): `== 0` is the ALIVE signal; ANY
    /// nonzero result is treated as dead. That is correct for ESRCH (no such
    /// process) but CONFLATES EPERM — the pid EXISTS but belongs to another
    /// user (alive-but-unsignalable) — with death. Known TS-parity limitation
    /// (utils.ts:380-388 has the same shape), kept deliberately: qd sessions
    /// are same-uid by construction, so EPERM needs a foreign-uid pid reuse to
    /// arise (punch B5 item 13 doc). An errno-aware probe exists where the
    /// distinction is load-bearing: relay_server's `pid_alive`
    /// (relay_server/mod.rs — only ESRCH counts as dead).
    fn is_alive(&self, pid: i32) -> bool;
    /// Best-effort list of running claude processes (stray discovery, spec §7).
    fn claude_procs(&self) -> io::Result<Vec<ProcInfo>>;
    /// The full command line for a single pid (the same `command=` field
    /// `claude_procs`/the wrapper-ancestry walk read), or `None` if the pid is not
    /// visible / the read failed. The codex W9 identity guard (create_daemon.rs)
    /// uses this to VERIFY a live pid is OUR codex daemon before a group-kill — so
    /// a reused pid that is now an unrelated group leader is never signaled. Served
    /// by the SAME single `ps` parse as the rest of this trait (no extra spawn
    /// point; tests substitute a `FixtureProcessTable`).
    fn cmdline(&self, pid: i32) -> Option<String>;
}

/// Real process table. Both `ppid_map` and `claude_procs` are served by ONE
/// `ps -eo pid=,ppid=,command=` parse routed through the [`Exec`] seam (so the
/// single spawn point stays in one place and tests substitute a [`ScriptedExec`]
/// — the only process spawns in the crate are in `exec.rs` + the `is_alive`
/// `kill(pid,0)` here). The `command=` form (not bare `pid=,ppid=`) is the same
/// `ps` usage TS uses for the wrapper-ancestry walk (utils.ts:454).
pub struct RealProcessTable<E: Exec> {
    exec: E,
}

impl<E: Exec> RealProcessTable<E> {
    pub fn new(exec: E) -> Self {
        Self { exec }
    }

    /// One `ps -eo pid=,ppid=,command=` parse → the full pid→{ppid,cmd} map. The
    /// shared source feeding `ppid_map`/`claude_procs`/the wrapper-ancestry walk.
    fn ps_rows(&self) -> io::Result<HashMap<i32, ProcRow>> {
        let r = self.exec.run("ps", &ps_args(), &[], None, None)?;
        Ok(parse_ps_rows(&r.stdout))
    }
}

/// One real `ps` snapshot for callers that need command and ancestry facts to
/// agree (not two separately-timed trait calls). Routed through the same Exec
/// seam and parser as [`RealProcessTable`]. Candidate Claude rows are enriched
/// with the process's real argv, preserving element boundaries which `ps`
/// display text loses. An unreadable argv remains `None` so receivability
/// classification can fail closed.
pub fn process_rows(exec: &dyn Exec) -> io::Result<HashMap<i32, ProcRow>> {
    let r = exec.run("ps", &ps_args(), &[], None, None)?;
    let mut rows = parse_ps_rows(&r.stdout);
    for (pid, row) in &mut rows {
        if command_program_is_claude(&row.cmd) {
            row.argv = process_argv(*pid);
        }
    }
    Ok(rows)
}

/// Enrich selected rows with exact OS argv elements. This is the bounded form
/// used by external adoption: one `ps` snapshot identifies the target's direct
/// children, then only those pids pay the Darwin `KERN_PROCARGS2` (or Linux
/// `/proc/<pid>/cmdline`) read. Flattened `ps command=` text is never promoted
/// into argv.
pub fn enrich_process_argv(rows: &mut HashMap<i32, ProcRow>, pids: &[i32]) {
    for pid in pids {
        if let Some(row) = rows.get_mut(pid) {
            row.argv = process_argv(*pid);
        }
    }
}

/// Read one live process's exact argv. Destructive identity fences use this
/// directly at the signal seam so they do not depend on an earlier `ps`
/// display-text classification.
pub fn exact_process_argv(pid: i32) -> Option<Vec<String>> {
    process_argv(pid)
}

impl<E: Exec> ProcessTable for RealProcessTable<E> {
    fn ppid_map(&self) -> io::Result<HashMap<i32, i32>> {
        // Port of the `ps -eo` read (src/session.ts:617-630); same single parse.
        Ok(self
            .ps_rows()?
            .into_iter()
            .map(|(pid, row)| (pid, row.ppid))
            .collect())
    }

    fn is_alive(&self, pid: i32) -> bool {
        if pid <= 0 {
            return false;
        }
        // kill -0: probe without signaling (src/utils.ts:380-388). 0 → alive;
        // nonzero → dead (EPERM conflated, per the trait doc).
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn claude_procs(&self) -> io::Result<Vec<ProcInfo>> {
        // Best-effort/cosmetic (brief deliverable 5): pid, ppid, command only;
        // cwd/start-time enrichment is platform-specific and lands with the
        // live wiring in A2. Fixture-backed tests define the decider contract.
        let mut procs: Vec<ProcInfo> = self
            .ps_rows()?
            .into_iter()
            .filter(|(_, row)| row.cmd.contains("claude"))
            .map(|(pid, row)| ProcInfo {
                pid,
                ppid: row.ppid,
                cmd: row.cmd,
                cwd: None,
                started_ms: None,
            })
            .collect();
        // HashMap iteration order is nondeterministic; sort for a stable result.
        procs.sort_by_key(|p| p.pid);
        Ok(procs)
    }

    fn cmdline(&self, pid: i32) -> Option<String> {
        if pid <= 0 {
            return None;
        }
        // Reuse the single `ps` parse (no extra spawn point); pick this pid's row.
        self.ps_rows().ok()?.get(&pid).map(|row| row.cmd.clone())
    }
}

/// One row of `ps -eo pid=,ppid=,command=` output.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcRow {
    pub ppid: i32,
    pub cmd: String,
    /// Exact argv elements read from the OS, or `None` when unavailable. This
    /// is deliberately separate from `cmd`: `ps command=` is flattened display
    /// text and cannot distinguish an option from words inside one argument.
    pub argv: Option<Vec<String>>,
}

fn command_program_is_claude(cmdline: &str) -> bool {
    let Some(program) = cmdline
        .split_whitespace()
        .next()
        .map(|s| s.trim_matches(['\'', '"']))
    else {
        return false;
    };
    std::path::Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        == Some("claude")
}

/// Read a live process's real argv with element boundaries intact.
#[cfg(target_os = "linux")]
fn process_argv(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }
    parse_linux_cmdline(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?)
}

#[cfg(target_os = "macos")]
fn process_argv(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = 0usize;
    // SAFETY: `mib` and `size` point to initialized storage; the first call
    // asks the kernel for the required buffer size and writes no payload.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size < std::mem::size_of::<libc::c_int>()
    {
        return None;
    }

    let mut buf = vec![0u8; size];
    // SAFETY: `buf` owns `size` writable bytes and `mib` remains valid for the
    // call. KERN_PROCARGS2 is read-only here (`newp = null`, `newlen = 0`).
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buf.truncate(size);
    parse_kern_procargs2(&buf)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_argv(_pid: i32) -> Option<Vec<String>> {
    None
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_linux_cmdline(bytes: &[u8]) -> Option<Vec<String>> {
    if bytes.is_empty() {
        return None;
    }
    let mut parts: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    if parts.last().is_some_and(|part| part.is_empty()) {
        parts.pop();
    }
    let argv = parts
        .into_iter()
        .map(|part| std::str::from_utf8(part).map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    (!argv.is_empty()).then_some(argv)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_kern_procargs2(bytes: &[u8]) -> Option<Vec<String>> {
    let int_len = std::mem::size_of::<i32>();
    let argc = i32::from_ne_bytes(bytes.get(..int_len)?.try_into().ok()?);
    if argc <= 0 {
        return None;
    }

    // KERN_PROCARGS2 layout: argc, executable path, NUL padding, then exactly
    // argc NUL-terminated argv elements, followed by environment strings.
    let payload = bytes.get(int_len..)?;
    let executable_end = payload.iter().position(|b| *b == 0)?;
    let mut offset = executable_end + 1;
    while payload.get(offset) == Some(&0) {
        offset += 1;
    }

    let argc = argc as usize;
    if argc > payload.len().saturating_sub(offset) {
        return None;
    }
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        let tail = payload.get(offset..)?;
        let end = tail.iter().position(|b| *b == 0)?;
        argv.push(std::str::from_utf8(&tail[..end]).ok()?.to_string());
        offset = offset.checked_add(end + 1)?;
    }
    Some(argv)
}

/// The `ps` argv the process-table read uses. `command=` (with `=` to suppress
/// the header) matches the TS usage (utils.ts:454: `ps -eo pid=,ppid=,command=`).
fn ps_args() -> Vec<String> {
    vec!["-eo".to_string(), "pid=,ppid=,command=".to_string()]
}

/// PURE: parse `ps -eo pid=,ppid=,command=` output into pid → {ppid, cmd}.
///
/// Ported from utils.ts:460-463 (`findZmxWrapperForPid`'s ps parse): each line is
/// `^\s*(\d+)\s+(\d+)\s+(.*)$`. Lines that don't match (headers, blanks) are
/// silently skipped — permissive (L8): never panic on junk from an external tool.
///
/// DF-3 (2026-06-06, SEV-HIGH dogfood finding): the field split MUST COALESCE
/// consecutive whitespace, matching the TS regex's `\s+`. `ps` pads its numeric
/// columns for alignment (Linux pads both pid and ppid on every row; macOS pads
/// any pid/ppid shorter than the column width — `    1     0 /sbin/launchd` on
/// the dev host), so a non-coalescing split (`splitn(3, char::is_whitespace)`,
/// the original port) read the pad as an empty ppid field and DROPPED the row —
/// on Linux effectively every row, killing all relay discovery / ancestry
/// surfaces. Only the two NUMERIC fields coalesce; the command remainder is
/// kept whole (internal runs of spaces inside a command line are content, not
/// padding).
pub fn parse_ps_rows(text: &str) -> HashMap<i32, ProcRow> {
    /// Split one whitespace-delimited field off the front: the field, plus the
    /// remainder with its LEADING whitespace run consumed (the `\s+`
    /// coalescing). `None` when no whitespace follows the field (the TS regex
    /// requires `\s+` between fields — a two-field line has no command and is
    /// skipped, exactly as `^\s*(\d+)\s+(\d+)\s+(.*)$` would skip it).
    fn split_field(s: &str) -> Option<(&str, &str)> {
        let end = s.find(char::is_whitespace)?;
        let (field, rest) = s.split_at(end);
        Some((field, rest.trim_start()))
    }
    let mut rows = HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some((pid, rest)) = split_field(trimmed) else {
            continue;
        };
        let Some((ppid, cmd)) = split_field(rest) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<i32>(), ppid.parse::<i32>()) else {
            continue;
        };
        rows.insert(
            pid,
            ProcRow {
                ppid,
                cmd: cmd.trim().to_string(),
                argv: None,
            },
        );
    }
    rows
}

/// Result of [`find_zmx_wrapper_for_pid`].
#[derive(Debug, Clone, PartialEq)]
pub struct ZmxWrapper {
    pub wrapper_pid: i32,
    pub zmx_name: String,
}

/// PURE decider over a pid→{ppid,cmd} ancestry map (port of `findZmxWrapperForPid`,
/// utils.ts:447-480).
///
/// Walk up from a claude PID through its process ancestry and find the owning
/// `zmx run <name>` / `zmx attach <name>` wrapper, returning the wrapper's PID and
/// the zmx session name parsed from its command line. Used as the last-resort
/// fallback in kill when the session's zmx socket dir isn't in any scanned dir
/// (red-team #1): the wrapper process is always a real ancestor regardless of dir.
///
/// Depth is capped at 6 (utils.ts:469) and the starting PID itself is skipped
/// (`cur !== pid`, utils.ts:473) so a claude invoked AS `zmx ...` doesn't match
/// itself. The walk stops at pid<=1 or a self-referential ppid (utils.ts:476).
pub fn find_zmx_wrapper_for_pid(pid: i32, rows: &HashMap<i32, ProcRow>) -> Option<ZmxWrapper> {
    if pid <= 0 {
        return None;
    }
    let mut cur = pid;
    for _ in 0..6 {
        let node = rows.get(&cur)?;
        if let Some(name) = match_zmx_wrapper_name(&node.cmd) {
            if cur != pid {
                return Some(ZmxWrapper {
                    wrapper_pid: cur,
                    zmx_name: name,
                });
            }
        }
        if node.ppid <= 1 || node.ppid == cur {
            break;
        }
        cur = node.ppid;
    }
    None
}

/// PURE: parse a `ps -o etime=` value — `[[dd-]hh:]mm:ss` — into elapsed
/// milliseconds. Both macOS and Linux `ps` emit this shape; locale-free
/// (unlike `lstart`). `None` on anything malformed.
pub fn parse_etime_ms(text: &str) -> Option<i64> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let (days, rest) = match t.split_once('-') {
        Some((d, r)) => (d.parse::<i64>().ok()?, r),
        None => (0, t),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let (h, m, s) = match parts.as_slice() {
        [h, m, s] => (
            h.parse::<i64>().ok()?,
            m.parse::<i64>().ok()?,
            s.parse::<i64>().ok()?,
        ),
        [m, s] => (0, m.parse::<i64>().ok()?, s.parse::<i64>().ok()?),
        _ => return None,
    };
    // CHECKED (WP-E hardening): an out-of-range etime (huge days/hours) must
    // overflow to `None`, never wrap to a garbage elapsed. A wrapped value would
    // flow into `proc_start_ms` and produce a nonsensical start (the sub-second
    // misparse the WP-D follow-up flagged), which the liveness classifier then
    // reads as a reused pid (`NotOurs`).
    days.checked_mul(24)?
        .checked_add(h)?
        .checked_mul(60)?
        .checked_add(m)?
        .checked_mul(60)?
        .checked_add(s)?
        .checked_mul(1000)
}

/// EFFECT (r8, lead-2): a live process's START TIME, epoch ms — the
/// program-agnostic half of pid identity. `(pid, start_time)` identifies a
/// process across `exec` (cmdline changes; start time does not): a process
/// holding pid P now whose start predates the registry row's `startedAt`
/// must BE the row's writer (two live processes never share a pid).
/// Implemented as `now − etime` via `ps -p <pid> -o etime=` (portable,
/// locale-free; second resolution — callers must carry generous slack).
/// `None` when the pid is not visible / `ps` failed.
///
/// SPAWN RETRY (WP-E hardening): the `ps` spawn itself can transiently fail under
/// load — `fork`/`posix_spawn` returns `EAGAIN`/`ENOMEM` when the host is momentarily
/// out of process/thread headroom (peak full-suite parallelism reproduces this). A
/// spawn failure is NOT evidence the pid is gone, so surrendering the `(pid, start)`
/// reuse-guard to it would be wrong (a transient miss would let a recycled pid read
/// as our-alive). We RETRY the spawn a bounded number of times with a short backoff.
/// A `ps` that RAN and simply did not find the pid (non-success / empty output) is a
/// real negative and is NOT retried — only the spawn error is. Zero overhead on the
/// success path.
pub fn proc_start_ms(pid: i32) -> Option<i64> {
    if pid <= 0 {
        return None;
    }
    let mut attempt = 0;
    let out = loop {
        match std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "etime="])
            .output()
        {
            Ok(out) => break out,
            // Transient spawn failure (EAGAIN/ENOMEM): retry, don't give up the guard.
            Err(_) if attempt < PS_SPAWN_RETRIES => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(PS_SPAWN_RETRY_BACKOFF_MS));
            }
            Err(_) => return None,
        }
    };
    if !out.status.success() {
        return None;
    }
    start_from_etime(RealClock.now_ms(), &String::from_utf8_lossy(&out.stdout))
}

/// Bounded retries for a transient `ps` SPAWN failure in [`proc_start_ms`] (a
/// fork/spawn `EAGAIN`/`ENOMEM` under load — NOT a pid-absent negative).
const PS_SPAWN_RETRIES: u32 = 4;
/// Backoff between [`proc_start_ms`] spawn retries (ms): short, so the worst-case
/// added latency on a transient failure (~`RETRIES * BACKOFF`) stays well inside
/// the boot/`ls` probe budgets; the success path pays nothing.
const PS_SPAWN_RETRY_BACKOFF_MS: u64 = 25;

/// PURE (WP-E hardening): fold a `ps -o etime=` reading taken at a known `now`
/// into a TRUSTWORTHY process start (epoch ms), or `None` when the reading is
/// out of range.
///
/// `ps -o etime=` of a sub-second-old process can misparse into a garbage
/// elapsed and thus a nonsensical start — the WP-D follow-up measured a start of
/// ~`-3.8e16`. WP-A's classifier compares this start against the registry's
/// recorded start; a garbage value lands far outside [`crate::kill::START_TIME_SLACK_MS`]
/// and is read as a reused pid ([`crate::liveness::LifecycleState::NotOurs`]) —
/// misclassifying a NEWBORN session as a stranger, which flashes it `cold` in
/// `qd ls`. An out-of-range start is therefore treated as UNKNOWN (`None`), so the
/// classifier takes its EXISTING fail-closed path ("start unreadable while present
/// ⇒ assume ours ⇒ ALIVE") — the correct, conservative reading. It is NEVER
/// trusted as a real start.
///
/// In range = a positive epoch ms not in the future: `elapsed ≥ 0` forces
/// `start ≤ now`, and a real start is `> 0`. Anything else ⇒ `None`.
pub fn start_from_etime(now_ms: i64, etime: &str) -> Option<i64> {
    let elapsed = parse_etime_ms(etime)?;
    if elapsed < 0 {
        return None;
    }
    let start = now_ms.checked_sub(elapsed)?;
    if start <= 0 {
        return None;
    }
    Some(start)
}

/// PURE (r7 F1, lead-2): the pid's ancestor chain — `[pid, parent, …]`, capped
/// at depth 10, stopping at pid ≤ 1 or a self-referential ppid. The kill
/// resolver's ownership clause asks "is this pane's pid one of the addressed
/// process's ancestors (or the process itself)?" — true for a zmx `run`
/// wrapper (ancestor), an embedded-mux direct child (equal), and an
/// embedded-mux shell parent (ancestor). A pid absent from `rows` (dead, or
/// `ps` failed) yields just `[pid]`.
pub fn ancestor_chain(pid: i32, rows: &HashMap<i32, ProcRow>) -> Vec<i32> {
    let mut chain = Vec::new();
    if pid <= 0 {
        return chain;
    }
    let mut cur = pid;
    for _ in 0..10 {
        chain.push(cur);
        let Some(node) = rows.get(&cur) else {
            break;
        };
        if node.ppid <= 1 || node.ppid == cur {
            break;
        }
        cur = node.ppid;
    }
    chain
}

/// PURE: parse a `zmx run|attach <name>` wrapper command line, returning the
/// session name. Ported from the regex at utils.ts:472:
/// `\bzmx\s+(?:run|r|attach|a)\s+(?:-\S+\s+)*([^\s]+)` — the `(?:-\S+\s+)*` skips
/// any flags (e.g. `-d`) before the name. No regex crate dependency: this is a
/// small hand-rolled scan matching that exact shape.
///
/// `pub` since r7 (OPEN-Q1): `kill::wrapper_kill_allowed` re-parses a recorded
/// wrapper pid's CURRENT cmdline with this exact shape before signaling it.
pub fn match_zmx_wrapper_name(cmd: &str) -> Option<String> {
    // Find a `zmx` token at a word boundary, then `run|r|attach|a`, then skip
    // leading `-flag` tokens, then take the next non-space token as the name.
    let bytes = cmd.as_bytes();
    let mut search_from = 0;
    while let Some(idx) = cmd[search_from..].find("zmx") {
        let pos = search_from + idx;
        // `\bzmx`: the char before must not be a word char.
        let boundary_ok = pos == 0 || !is_word_byte(bytes[pos - 1]);
        let after = pos + 3;
        if boundary_ok {
            if let Some(name) = parse_subcmd_and_name(&cmd[after..]) {
                return Some(name);
            }
        }
        search_from = pos + 3;
    }
    None
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// After a `zmx` token: require `\s+(run|r|attach|a)\s+`, then `(?:-\S+\s+)*`,
/// then capture the next `[^\s]+` token.
fn parse_subcmd_and_name(rest: &str) -> Option<String> {
    let rest = rest.strip_prefix(|c: char| c.is_whitespace())?;
    let rest = rest.trim_start();
    // The subcommand token, followed by whitespace.
    let mut it = rest.splitn(2, char::is_whitespace);
    let sub = it.next()?;
    if !matches!(sub, "run" | "r" | "attach" | "a") {
        return None;
    }
    let mut tail = it.next()?.trim_start();
    // Skip leading -flag tokens (utils.ts: `(?:-\S+\s+)*`).
    loop {
        let tok = tail.split_whitespace().next()?;
        if tok.starts_with('-') {
            // Drop this flag token and its trailing whitespace.
            tail = tail[tok.len()..].trim_start();
        } else {
            return Some(tok.to_string());
        }
    }
}

/// Liveness probe (port of `isPidAlive`, utils.ts:380-388): `kill(pid, 0)`. A
/// non-positive pid is never alive.
///
/// True semantics (punch B5 item 13 doc): `kill(pid, 0) == 0` → alive; ANY
/// nonzero return is treated as dead. Correct for ESRCH (no such process),
/// but EPERM — the pid exists under ANOTHER uid (alive, just unsignalable) —
/// is conflated with death. Deliberate TS-parity (the TS original has the
/// identical shape); reaching the conflation requires a recorded same-uid pid
/// to be reused by a foreign-uid process. Where the distinction matters, use
/// an errno-aware probe (see relay_server's `pid_alive`, which treats only
/// ESRCH as provably dead).
pub fn is_pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // process.kill(pid, 0): probe-only signal. 0 → alive; nonzero (ESRCH, and
    // EPERM by the documented conflation above) → treated dead.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Kill a single PID: SIGTERM, wait, then SIGKILL (port of `killPid`,
/// utils.ts:390-425). Returns true if the PID is dead afterward (or was never
/// alive).
///
/// Targets the PID ONLY — never the process group, so a session killing itself
/// doesn't take qd down with it (utils.ts:391-392). This PID-only discipline is
/// L10 groundwork: kill/gc/reconcile act on specific PIDs, never patterns/groups.
///
/// `grace_ms` (default 3000) is the SIGTERM→SIGKILL grace window. It is machine-
/// tuned but FAIL-SAFE: if a loaded host needs longer to shut down gracefully,
/// the worst case is an earlier SIGKILL, never a silent leak — the caller still
/// verifies the PID is dead and reports loudly on failure (utils.ts:395-399).
pub fn kill_pid(pid: i32, grace_ms: u64) -> bool {
    if pid <= 0 {
        return true;
    }
    if !is_pid_alive(pid) {
        return true;
    }
    // SIGTERM. ESRCH → already gone.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return !is_pid_alive(pid);
    }
    let deadline = Instant::now() + Duration::from_millis(grace_ms);
    while Instant::now() < deadline {
        if !is_pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    // SIGKILL.
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
        return !is_pid_alive(pid);
    }
    // Brief wait for SIGKILL to take effect (utils.ts:419-423).
    for _ in 0..10 {
        if !is_pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !is_pid_alive(pid)
}

/// Default SIGTERM→SIGKILL grace window (utils.ts:400, `graceMs = 3000`).
pub const KILL_GRACE_MS: u64 = 3000;

/// punch item 8: slack for the per-victim `(pid, start-time)` identity
/// recheck in [`kill_pid_tree`]. Both the snapshot stamp and the recheck are
/// `now − etime` reads of the SAME true start instant, so a matching process
/// differs only by rounding (`etime` has second resolution + two clock
/// reads): ±2.5s is generous for the match while staying far tighter than
/// the registry-row slack (`kill::START_TIME_SLACK_MS`, 120s — that one
/// compares against a row stamped by a different writer). Documented
/// residual: a pid freed and reused within the same ~2.5s of the snapshot
/// would pass — macOS allocates pids upward (reuse needs a wrap), so
/// sub-three-second reuse is not a practical shape.
pub const TREE_KILL_START_SLACK_MS: i64 = 2_500;

/// punch item 8 (b3-kill-spec): reap a SNAPSHOT of descendant victims, each
/// individually identity-gated. `victims` is `(pid, start_ms)` stamped by the
/// caller while the process tree was intact (BEFORE the pane/root kill —
/// teardown reparents survivors to init and erases the ppid evidence).
///
/// DESTRUCTION REQUIRES POSITIVE IDENTITY EVIDENCE: before EVERY signal
/// (the SIGTERM and again the SIGKILL) the victim's current start time must
/// still match its snapshot stamp within [`TREE_KILL_START_SLACK_MS`] — a
/// reused pid (start time moved) or a vanished pid is NEVER signaled. The
/// grace wait is collective (one `grace_ms` window for the whole set, the
/// `kill_pid` ladder shape), so the sweep's wall cost is bounded regardless
/// of victim count.
///
/// Returns the pids that still hold their snapshot identity AND are alive
/// after the ladder — loud-leak material for the caller.
pub fn kill_pid_tree(victims: &[(i32, i64)], grace_ms: u64) -> Vec<i32> {
    let identity_holds = |pid: i32, snap_start: i64| -> bool {
        match proc_start_ms(pid) {
            Some(now_start) => (now_start - snap_start).abs() <= TREE_KILL_START_SLACK_MS,
            // Invisible to ps: dead (nothing to signal) or unreadable
            // (no evidence — never signal without it).
            None => false,
        }
    };
    // `kill(pid, 0)` counts a ZOMBIE as alive, but a zombie is DEAD for this
    // sweep's purposes: it cannot execute, signals are no-ops, and reaping it
    // is its PARENT's job (which may itself be a victim dying in this same
    // pass). Without this, a victim whose parent is slow to `wait` would burn
    // the grace window and surface as a FALSE loud leak.
    let dead_or_zombie = |pid: i32| -> bool { !is_pid_alive(pid) || proc_is_zombie(pid) };
    // Phase 1: SIGTERM every victim that is alive AND still the snapshot
    // process. Bottom-up order is the caller's (descendant_kill_list).
    let mut pending: Vec<(i32, i64)> = Vec::new();
    for &(pid, snap) in victims {
        if pid <= 0 || dead_or_zombie(pid) || !identity_holds(pid, snap) {
            continue;
        }
        unsafe { libc::kill(pid, libc::SIGTERM) };
        pending.push((pid, snap));
    }
    // Collective grace window.
    let deadline = Instant::now() + Duration::from_millis(grace_ms);
    while !pending.is_empty() && Instant::now() < deadline {
        pending.retain(|&(pid, _)| !dead_or_zombie(pid));
        if pending.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    // Phase 2: SIGKILL survivors — identity re-verified AGAIN (the victim may
    // have died and the pid moved on during the grace window).
    pending.retain(|&(pid, snap)| !dead_or_zombie(pid) && identity_holds(pid, snap));
    for &(pid, _) in &pending {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    // Brief settle (the kill_pid SIGKILL-wait shape), then report leftovers.
    for _ in 0..10 {
        pending.retain(|&(pid, snap)| !dead_or_zombie(pid) && identity_holds(pid, snap));
        if pending.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    pending.into_iter().map(|(pid, _)| pid).collect()
}

/// The raw OS scheduler reading for one pid, folded into the categories the
/// lifecycle classifier ([`crate::liveness`], WP-A bugs #4/#1) needs. The key
/// distinction this carries that a bare `kill(pid,0)` cannot: **POSITIVE
/// absence** ([`ProcLiveness::Gone`] — the pid is provably reaped) is separated
/// from an **AMBIGUOUS** probe failure ([`ProcLiveness::Unknown`] — the read
/// itself could not answer). #4's fail-closed rule keys on exactly that split:
/// death is inferred only from positive absence (or a zombie), NEVER from a
/// probe that did not actually witness an exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcLiveness {
    /// Scheduler state `R`/`D` — on-CPU or in uninterruptible I/O (working).
    RunningOrDisk,
    /// Scheduler state `S`/`T`/`t`/`I`/… — sleeping/idle/stopped but PRESENT
    /// and alive (the silent-window shape: trackB measured claude here, never Z).
    Sleeping,
    /// Scheduler state `Z` (or `X`/`x`) — exited, awaiting the parent's reap
    /// (still visible in the process table; the exit STATUS is unknowable to a
    /// non-parent observer — memo Step-2 `waitpid ⇒ ECHILD`).
    Zombie,
    /// Provably ABSENT: `/proc/<pid>` is ENOENT (or `ps` reports no such pid) —
    /// reaped and gone. The ONLY reading that alone proves death-by-gone.
    Gone,
    /// The probe could not answer (a read error that is NOT a positive absence).
    /// AMBIGUOUS — never death evidence; the classifier fails closed to alive.
    Unknown,
}

/// EFFECT (WP-A #4): the program-agnostic OS liveness reading for `pid`,
/// keyed for the lifecycle classifier. On Linux reads `/proc/<pid>/stat`
/// (the memo's mandated cross-process observer source — `qd` is NOT claude's
/// parent, so `waitpid` is unavailable); elsewhere falls back to `ps -o stat=`.
/// A non-positive pid is [`ProcLiveness::Gone`].
///
/// The Linux read is ENOENT-aware so absence is POSITIVE (`Gone`), and any
/// other I/O error is `Unknown` (ambiguous, fail-closed alive) — the split #4
/// requires. The state char is taken AFTER the last `)` (the `comm` field can
/// itself contain spaces and parens), per `proc(5)`.
pub fn proc_liveness(pid: i32) -> ProcLiveness {
    if pid <= 0 {
        return ProcLiveness::Gone;
    }
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => fold_proc_state(state_char_after_comm(&stat)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => ProcLiveness::Gone,
            Err(_) => ProcLiveness::Unknown,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        match std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
        {
            Ok(out) if out.status.success() => {
                let s = String::from_utf8_lossy(&out.stdout);
                match s.trim_start().chars().next() {
                    Some(c) => fold_proc_state(Some(c)),
                    // `ps` succeeded but printed nothing — the pid is gone.
                    None => ProcLiveness::Gone,
                }
            }
            // `ps -p` exits non-zero when the pid does not exist: positive absence.
            Ok(_) => ProcLiveness::Gone,
            // Could not run `ps` at all: ambiguous, never death evidence.
            Err(_) => ProcLiveness::Unknown,
        }
    }
}

/// Fold a `/proc`/`ps` state char into [`ProcLiveness`]. `None` (no char where
/// one was expected) is `Unknown` — a malformed read is ambiguous, not death.
fn fold_proc_state(state: Option<char>) -> ProcLiveness {
    match state {
        Some('R') | Some('D') => ProcLiveness::RunningOrDisk,
        Some('Z') | Some('X') | Some('x') => ProcLiveness::Zombie,
        Some(_) => ProcLiveness::Sleeping, // S/T/t/I/W/P/K — present and alive
        None => ProcLiveness::Unknown,
    }
}

/// The `/proc/<pid>/stat` state char: the first non-space char after the LAST
/// `)`. `comm` (field 2) is wrapped in parens and may contain spaces/parens, so
/// anchoring on the last `)` is the proc(5)-correct way to find field 3.
#[cfg(target_os = "linux")]
fn state_char_after_comm(stat: &str) -> Option<char> {
    let close = stat.rfind(')')?;
    stat[close + 1..].trim_start().chars().next()
}

/// EFFECT (punch item 8): is the pid a ZOMBIE (dead, awaiting its parent's
/// `wait`)? Read via `ps -p <pid> -o stat=` — the state column starts with
/// `Z` on both macOS and Linux. `false` on any read failure (unknown is not
/// evidence of zombiehood; callers treat it as plain alive).
pub fn proc_is_zombie(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let Ok(out) = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
    else {
        return false;
    };
    out.status.success()
        && String::from_utf8_lossy(&out.stdout)
            .trim_start()
            .starts_with('Z')
}

/// Fixture process table for tests.
#[derive(Default)]
pub struct FixtureProcessTable {
    pub ppids: HashMap<i32, i32>,
    pub alive: std::collections::HashSet<i32>,
    pub claude: Vec<ProcInfo>,
}

impl ProcessTable for FixtureProcessTable {
    fn ppid_map(&self) -> io::Result<HashMap<i32, i32>> {
        Ok(self.ppids.clone())
    }
    fn is_alive(&self, pid: i32) -> bool {
        self.alive.contains(&pid)
    }
    fn claude_procs(&self) -> io::Result<Vec<ProcInfo>> {
        Ok(self.claude.clone())
    }
    fn cmdline(&self, pid: i32) -> Option<String> {
        // Fixture: serve the cmd from the `claude` proc list if present, else the
        // ppid map carries no cmd — tests that exercise `cmdline` populate `claude`.
        self.claude
            .iter()
            .find(|p| p.pid == pid)
            .map(|p| p.cmd.clone())
    }
}

/// Relay port-scan seam. The sidecar-file read is plain fs (relay.rs); this
/// trait covers ONLY the HTTP `/health` port-scan fallback
/// (src/session.ts:185-212), which is live-network and therefore A4's to
/// implement for real (ADD-5: client-of-contract). A1 is fixture-backed.
pub trait RelayProbe {
    fn scan(&self) -> Vec<crate::model::RelayHealth>;
}

/// Fixture probe: canned scan results (usually empty — the sidecar path is
/// the contract surface per ADD-5).
#[derive(Default)]
pub struct FixtureRelayProbe(pub Vec<crate::model::RelayHealth>);

impl RelayProbe for FixtureRelayProbe {
    fn scan(&self) -> Vec<crate::model::RelayHealth> {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::ScriptedExec;

    #[test]
    fn adopt_linux_cmdline_decoder_preserves_nul_delimited_argv() {
        assert_eq!(
            parse_linux_cmdline(b"claude\0--name\0bare-one\0"),
            Some(vec!["claude".into(), "--name".into(), "bare-one".into()])
        );
        assert_eq!(
            parse_linux_cmdline(b"claude\0prompt with spaces"),
            Some(vec!["claude".into(), "prompt with spaces".into()])
        );
        assert_eq!(parse_linux_cmdline(b""), None);
        assert_eq!(parse_linux_cmdline(b"claude\0\xff\0"), None);
    }

    fn kern_procargs2_buffer(argc: i32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = argc.to_ne_bytes().to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn adopt_darwin_procargs2_decoder_honors_argc_and_excludes_environment() {
        let bytes = kern_procargs2_buffer(
            3,
            b"/opt/Claude/claude\0\0\0claude\0--name\0bare-one\0HOME=/tmp\0IGNORED=1\0",
        );
        assert_eq!(
            parse_kern_procargs2(&bytes),
            Some(vec!["claude".into(), "--name".into(), "bare-one".into()])
        );
        assert_eq!(
            parse_kern_procargs2(&kern_procargs2_buffer(0, b"x\0")),
            None
        );
        assert_eq!(
            parse_kern_procargs2(&kern_procargs2_buffer(-1, b"x\0")),
            None
        );
    }

    #[test]
    fn adopt_darwin_procargs2_decoder_rejects_malformed_or_truncated_buffers() {
        assert_eq!(parse_kern_procargs2(&[1, 2, 3]), None);
        assert_eq!(
            parse_kern_procargs2(&kern_procargs2_buffer(1, b"/usr/bin/claude")),
            None
        );
        assert_eq!(
            parse_kern_procargs2(&kern_procargs2_buffer(2, b"/usr/bin/claude\0\0claude\0")),
            None
        );
        assert_eq!(
            parse_kern_procargs2(&kern_procargs2_buffer(1, b"/usr/bin/claude\0\0\xff\0")),
            None
        );
    }

    #[test]
    fn parse_ps_rows_splits_three_fields() {
        // Single-space legacy shape — kept, but on its own it is VACUOUS for the
        // DF-3 padded class (LESSONS L25); the padded-fixture rows below carry
        // the class teeth.
        let text = "  100 1 /sbin/launchd\n  200 100 bash -lc command 'claude'\nheaderjunk\n";
        let rows = parse_ps_rows(text);
        assert_eq!(rows[&100].ppid, 1);
        assert_eq!(rows[&100].cmd, "/sbin/launchd");
        assert_eq!(rows[&200].ppid, 100);
        assert_eq!(rows[&200].cmd, "bash -lc command 'claude'");
        // "headerjunk" has no ppid/cmd → skipped permissively.
        assert_eq!(rows.len(), 2);
    }

    /// DF-3 regression, Linux padded class: `ps -eo pid=,ppid=,command=` pads
    /// BOTH numeric columns on Linux, so EVERY row is multi-space-separated.
    /// FIXTURE PROVENANCE: derived from the real values in the DF-3 dogfood
    /// capture (exec/log/2026-06-06-dogfood.md 14:18 entry — pids 294020/294008,
    /// the node relay stub, the wrapper chain; the raw cat -A capture lived in
    /// the dogfood VM and was not committed, so the rows are reconstructed with
    /// the documented right-pad shape, not hand-invented values). The pre-DF-3
    /// parser returns ZERO rows for this input (every ppid reads as "") — this
    /// test is the committed proof the fix binds to the padded CLASS, which the
    /// single-space row above cannot provide.
    #[test]
    fn parse_ps_rows_df3_linux_padded_class() {
        let text = include_str!("../tests/fixtures/df3/ps-linux-padded.txt");
        let rows = parse_ps_rows(text);
        // ALL four rows parse (the pre-fix parser dropped all four).
        assert_eq!(rows.len(), 4, "every padded row must parse: {rows:?}");
        // The exact dogfood ancestry chain that DF-3 killed:
        assert_eq!(rows[&294020].ppid, 294008);
        assert!(rows[&294020].cmd.starts_with("node "));
        assert_eq!(rows[&294008].ppid, 1290);
        assert_eq!(rows[&1290].ppid, 1);
        assert!(rows[&1290].cmd.starts_with("zmx run dogfood-m3b"));
        // Kernel-thread-shaped row (heavily padded short pid) parses too.
        assert_eq!(rows[&7].ppid, 2);
        assert_eq!(rows[&7].cmd, "[kworker/0:1-events]");
    }

    /// DF-3 regression, macOS width-pad class + command-intactness.
    /// FIXTURE PROVENANCE: rows 1-3 are a REAL `ps -eww -o pid=,ppid=,command=`
    /// capture from the dev host (2026-06-06; `    1     0 /sbin/launchd` shows
    /// the width-pad live on macOS — the pre-fix parser dropped launchd's row on
    /// the host the engine ships on; -eww only to defeat capture-time column
    /// truncation, the pid/ppid padding is identical without it). Row 4 is
    /// SYNTHETIC (labeled): no real process with embedded double-space argv
    /// existed at capture time; it pins that runs of spaces INSIDE the command
    /// field are preserved verbatim (content, not padding — the red-team's
    /// command-intactness vector).
    #[test]
    fn parse_ps_rows_df3_macos_width_pad_and_command_intact() {
        let text = include_str!("../tests/fixtures/df3/ps-macos-real.txt");
        let rows = parse_ps_rows(text);
        assert_eq!(rows.len(), 4, "every captured row must parse: {rows:?}");
        // The real launchd row the pre-fix parser dropped.
        assert_eq!(rows[&1].ppid, 0);
        assert_eq!(rows[&1].cmd, "/sbin/launchd");
        // Real long-command rows survive with full command text.
        assert!(rows[&116].cmd.contains("qrmux-server --socket-dir"));
        // Command-intactness: internal double spaces preserved verbatim.
        assert_eq!(
            rows[&500].cmd,
            "bash -c sleep 45; true df3  embedded  spaces"
        );
    }

    /// DF-3 edge pins (TS `^\s*(\d+)\s+(\d+)\s+(.*)$` parity): tabs coalesce
    /// like spaces; leading-whitespace pids parse; a two-field line (no
    /// whitespace after ppid) is skipped exactly as the regex would skip it;
    /// a ppid followed by trailing whitespace yields an EMPTY command row.
    #[test]
    fn parse_ps_rows_df3_tab_and_boundary_edges() {
        let rows = parse_ps_rows("\t42\t\t41\tnice  cmd\n");
        assert_eq!(rows[&42].ppid, 41);
        assert_eq!(rows[&42].cmd, "nice  cmd");

        // Two fields, no trailing separator → no cmd capture → skipped.
        assert!(parse_ps_rows("123 456").is_empty());
        // Two fields + trailing space → cmd = "" (the regex's `(.*)$` matches empty).
        let rows = parse_ps_rows("123 456 \n");
        assert_eq!(rows[&123].ppid, 456);
        assert_eq!(rows[&123].cmd, "");
        // Negative-looking / non-numeric fields stay skipped.
        assert!(parse_ps_rows("abc def cmd\n").is_empty());
    }

    #[test]
    fn real_process_table_routes_ppid_map_and_claude_procs_through_exec() {
        let ps_out = "\
  1 0 /sbin/launchd
  555 1 zmx run mysession -d bash -lc command 'claude' '--name' 'mysession'
  556 555 bash -lc command claude --name mysession
  557 556 node /usr/local/bin/claude --name mysession
  900 1 /usr/bin/ssh somewhere
";
        let exec = ScriptedExec::new().on("ps", &["-eo"], Some(0), ps_out, "");
        let pt = RealProcessTable::new(exec);

        let map = pt.ppid_map().unwrap();
        assert_eq!(map[&557], 556);
        assert_eq!(map[&556], 555);

        // claude_procs: only rows whose cmd contains "claude", pid-sorted.
        let claude = pt.claude_procs().unwrap();
        let pids: Vec<i32> = claude.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![555, 556, 557]);
    }

    #[test]
    fn real_process_table_cmdline_reads_one_pid() {
        // W9 FIX M-1: the single-pid cmdline read (the codex identity guard's probe)
        // routes through the SAME `ps` parse — a visible pid yields its command line,
        // an unknown / non-positive pid yields None.
        let ps_out = "\
  1 0 /sbin/launchd
  4242 1 codex app-server --listen ws://127.0.0.1:18962
";
        let exec = ScriptedExec::new().on("ps", &["-eo"], Some(0), ps_out, "");
        let pt = RealProcessTable::new(exec);
        assert_eq!(
            pt.cmdline(4242).as_deref(),
            Some("codex app-server --listen ws://127.0.0.1:18962")
        );
        assert_eq!(pt.cmdline(999999), None, "unknown pid → None");
        assert_eq!(pt.cmdline(0), None, "non-positive pid → None");
        assert_eq!(pt.cmdline(-1), None);
    }

    #[test]
    fn fixture_process_table_cmdline_serves_from_claude_list() {
        let pt = FixtureProcessTable {
            claude: vec![ProcInfo {
                pid: 4242,
                ppid: 1,
                cmd: "codex app-server --listen ws://127.0.0.1:18962".to_string(),
                cwd: None,
                started_ms: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            pt.cmdline(4242).as_deref(),
            Some("codex app-server --listen ws://127.0.0.1:18962")
        );
        assert_eq!(pt.cmdline(7), None);
    }

    #[test]
    fn find_zmx_wrapper_walks_ancestry_and_parses_name() {
        // claude (557) → bash (556) → `zmx run mysession -d ...` (555).
        let mut rows = HashMap::new();
        rows.insert(
            1,
            ProcRow {
                ppid: 0,
                cmd: "/sbin/launchd".into(),
                argv: None,
            },
        );
        rows.insert(
            555,
            ProcRow {
                ppid: 1,
                cmd: "zmx run mysession -d bash -lc command 'claude'".into(),
                argv: None,
            },
        );
        rows.insert(
            556,
            ProcRow {
                ppid: 555,
                cmd: "bash -lc command claude".into(),
                argv: None,
            },
        );
        rows.insert(
            557,
            ProcRow {
                ppid: 556,
                cmd: "node claude".into(),
                argv: None,
            },
        );

        let w = find_zmx_wrapper_for_pid(557, &rows).unwrap();
        assert_eq!(w.wrapper_pid, 555);
        assert_eq!(w.zmx_name, "mysession");

        // r8: the etime parse feeding the (pid, start-time) identity arm.
        assert_eq!(parse_etime_ms("05:03"), Some((5 * 60 + 3) * 1000));
        assert_eq!(
            parse_etime_ms(" 01:02:03 "),
            Some(((60 + 2) * 60 + 3) * 1000)
        );
        assert_eq!(
            parse_etime_ms("2-03:04:05"),
            Some((((2 * 24 + 3) * 60 + 4) * 60 + 5) * 1000)
        );
        assert_eq!(parse_etime_ms(""), None);
        assert_eq!(parse_etime_ms("garbage"), None);

        // r7: the ownership chain over the same tree — [self, parent, …],
        // stopping at ppid <= 1.
        assert_eq!(ancestor_chain(557, &rows), vec![557, 556, 555]);
        // A pid with no ps row (dead / ps failed) yields just [pid].
        assert_eq!(ancestor_chain(4242, &rows), vec![4242]);
        assert!(ancestor_chain(0, &rows).is_empty());
    }

    /// WP-E hardening (red-without/green-with): an OUT-OF-RANGE etime — one whose
    /// elapsed overflows i64 — must parse to `None`, never wrap to a garbage
    /// elapsed. Without the `checked_*` guard this overflows: it panics in debug
    /// and silently wraps to a wrong `Some(..)` in release — either way this
    /// assertion fails. The garbage that would otherwise escape is exactly what
    /// produces the sub-second `proc_start_ms` misparse (~-3.8e16) the WP-D
    /// follow-up flagged.
    #[test]
    fn out_of_range_etime_parses_to_none() {
        // Days so large the seconds product overflows i64.
        assert_eq!(parse_etime_ms("999999999999999999-00:00:00"), None);
        // Hours field overflow on the no-days branch.
        assert_eq!(parse_etime_ms("999999999999999999:00:00"), None);
        // The in-range neighbours still parse (the guard rejects only overflow).
        assert_eq!(parse_etime_ms("00:00"), Some(0));
        assert_eq!(parse_etime_ms("1-00:00:00"), Some(86_400_000));
    }

    /// WP-E hardening: `start_from_etime` folds a `ps -o etime=` reading at a
    /// known `now` into a trustworthy start, or `None` when out of range. A
    /// sub-second child (`00:00`) yields `now` itself — a real, in-range start
    /// (NOT garbage). An elapsed larger than `now` (a process "started" before
    /// the epoch) is impossible ⇒ `None`, routing the classifier to its
    /// fail-closed ALIVE path instead of trusting a negative start as real.
    #[test]
    fn start_from_etime_ranges() {
        let now = 1_750_000_000_000; // ~2025, epoch ms
                                     // Sub-second-old process: 00:00 ⇒ start == now (in range, trusted).
        assert_eq!(start_from_etime(now, "      00:00"), Some(now));
        // A normal few-seconds-old process.
        assert_eq!(start_from_etime(now, "00:05"), Some(now - 5_000));
        // Elapsed exceeding `now` ⇒ start ≤ 0 ⇒ out of range ⇒ None.
        assert_eq!(start_from_etime(now, "999999999-00:00:00"), None);
        // An unparseable / overflowing etime ⇒ None (never a garbage start).
        assert_eq!(start_from_etime(now, "garbage"), None);
        assert_eq!(start_from_etime(now, "999999999999999999-00:00:00"), None);
    }

    #[test]
    fn find_zmx_wrapper_attach_alias_and_flag_skip() {
        // `zmx a -foo bar sess` → subcommand alias `a`, skip `-foo` then `bar`?
        // No: regex is `(?:-\S+\s+)*([^\s]+)`, so only LEADING `-flag` tokens are
        // skipped; the first non-flag token is the name. Here `-foo` skipped, name=bar.
        let mut rows = HashMap::new();
        rows.insert(
            10,
            ProcRow {
                ppid: 5,
                cmd: "zmx a -d bar".into(),
                argv: None,
            },
        );
        rows.insert(
            20,
            ProcRow {
                ppid: 10,
                cmd: "claude".into(),
                argv: None,
            },
        );
        let w = find_zmx_wrapper_for_pid(20, &rows).unwrap();
        assert_eq!(w.wrapper_pid, 10);
        assert_eq!(w.zmx_name, "bar");
    }

    #[test]
    fn find_zmx_wrapper_skips_self_and_caps_depth() {
        // The starting pid IS a zmx wrapper → must NOT match itself (cur != pid).
        let mut rows = HashMap::new();
        rows.insert(
            10,
            ProcRow {
                ppid: 1,
                cmd: "zmx run self".into(),
                argv: None,
            },
        );
        assert_eq!(find_zmx_wrapper_for_pid(10, &rows), None);

        // No zmx ancestor anywhere → None, no panic on a deep chain.
        let mut deep = HashMap::new();
        for i in 2..20 {
            deep.insert(
                i,
                ProcRow {
                    ppid: i - 1,
                    cmd: "bash".into(),
                    argv: None,
                },
            );
        }
        deep.insert(
            1,
            ProcRow {
                ppid: 0,
                cmd: "init".into(),
                argv: None,
            },
        );
        assert_eq!(find_zmx_wrapper_for_pid(19, &deep), None);
    }

    #[test]
    fn kill_pid_nonpositive_is_noop_true() {
        assert!(kill_pid(0, 10));
        assert!(kill_pid(-1, 10));
        assert!(!is_pid_alive(0));
    }

    #[test]
    fn kill_pid_dead_pid_returns_true() {
        // A wildly-high PID that is not alive → kill_pid short-circuits true.
        assert!(!is_pid_alive(2_000_000_000));
        assert!(kill_pid(2_000_000_000, 10));
    }
}
