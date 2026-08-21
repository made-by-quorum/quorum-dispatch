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

/// A BARE (outside-qd) non-claude harness process detected in the process table
/// (lsview A4). `provider` is the harness id (`"codex"` | `"opencode"` | `"pi"`),
/// `pid` its process id, `cwd` its working directory when the `lsof` enrichment
/// succeeded (best-effort — `None` on failure, the same posture `claude_procs`
/// takes today; a detectable-but-unidentifiable process still renders as a bare
/// row of its provider). Visibility only: a bare proc is NEVER a session and
/// ACTING verbs never resolve one.
#[derive(Debug, Clone, PartialEq)]
pub struct BareProc {
    pub provider: String,
    pub pid: i32,
    pub cwd: Option<String>,
}

/// One `ps`-row candidate for a bare non-claude harness session: the detected
/// `provider` and the row's `pid`, AFTER R2's representative-pick (only the
/// canonical harness-executable row survives — see [`classify_bare_nonclaude`]).
#[derive(Debug, Clone, PartialEq)]
pub struct BareCandidate {
    pub provider: &'static str,
    pub pid: i32,
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
    /// Liveness check (`kill(pid, 0)` probe), ERRNO-AWARE: success is the ALIVE
    /// signal, and on failure ONLY `ESRCH` (no such process) means dead. `EPERM`
    /// means the pid EXISTS but we are not permitted to signal it
    /// (alive-but-unsignalable) and therefore counts as ALIVE.
    ///
    /// This previously conflated `EPERM` with death as a TS-parity limitation
    /// (utils.ts:380-388 has the same shape), justified by "qd sessions are
    /// same-uid by construction, so EPERM needs a foreign-uid pid reuse to
    /// arise" (punch B5 item 13 doc). A SANDBOXED caller falsifies that premise:
    /// a seatbelt/container policy denies signalling our OWN same-uid pids, so
    /// EPERM arrives on the ordinary path and every live session reads as dead.
    /// Never convert EPERM into "not found" — see [`kill0_alive`].
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

    /// Best-effort list of BARE (outside-qd) non-claude harness processes —
    /// codex / opencode / pi — detected in the process table (lsview A4). The
    /// non-claude analog of [`ProcessTable::claude_procs`]: the per-harness `ps`
    /// predicate, the representative-pick that collapses a session's multiple
    /// matching rows to one, and the `lsof` cwd enrichment are R2's census
    /// contract (`findings/R2-bareproc-census.md`). Default: NONE — the fixture
    /// table exposes no bare procs, so every existing gather/join test is
    /// unchanged; [`RealProcessTable`] overrides with the live `ps`+`lsof`
    /// detection. The CLAUDE path is untouched (this is purely additive).
    fn bare_nonclaude_procs(&self) -> io::Result<Vec<BareProc>> {
        Ok(Vec::new())
    }
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

    /// A single process's cwd via `lsof -a -p <pid> -d cwd -Fn`, routed through
    /// the SAME [`Exec`] seam as `ps` (so tests inject a synthetic `lsof` through
    /// a [`ScriptedExec`]). Best-effort: `None` when the pid is non-positive, the
    /// spawn/read failed, or no `n<path>` line was produced (lsview A4 — the R2
    /// cwd-enrichment recipe; the same posture `claude_procs` takes on failure).
    fn cwd_via_lsof(&self, pid: i32) -> Option<String> {
        if pid <= 0 {
            return None;
        }
        let r = self
            .exec
            .run("lsof", &bare_cwd_lsof_args(pid), &[], None, None)
            .ok()?;
        parse_lsof_cwd(&r.stdout)
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
        // kill -0: probe without signaling (src/utils.ts:380-388). Errno-aware:
        // only ESRCH is death; EPERM is alive-but-unsignalable (trait doc).
        kill0_alive(pid)
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

    fn bare_nonclaude_procs(&self) -> io::Result<Vec<BareProc>> {
        // ONE `ps` parse (the shared source), then R2's detection + representative
        // pick (pure), then a per-candidate `lsof` cwd read, then collapse a
        // session's residual multi-rows to one per (provider, cwd). The claude
        // path is not touched.
        let rows = self.ps_rows()?;
        let mut procs: Vec<BareProc> = classify_bare_nonclaude(&rows)
            .into_iter()
            .map(|c| BareProc {
                provider: c.provider.to_string(),
                pid: c.pid,
                cwd: self.cwd_via_lsof(c.pid),
            })
            .collect();
        // Dedup: one row per (provider, cwd) when the cwd is known (the canonical
        // pick already yields one row per session, so this is a backstop). A
        // cwd-unknown row is kept as-is — distinct by pid, and visibility is the
        // bar. `classify_bare_nonclaude` pre-sorts by (provider, pid), so retain
        // keeps the lowest pid per (provider, cwd) deterministically.
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        procs.retain(|p| match &p.cwd {
            Some(cwd) => seen.insert((p.provider.clone(), cwd.clone())),
            None => true,
        });
        Ok(procs)
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

/// lsview A4 — the `command=` basename of a row's argv[0]: the first whitespace
/// token, quotes stripped, reduced to its path file-name (`node
/// /opt/homebrew/bin/codex` → `node`; `/…/vendor/…/bin/codex` → `codex`; `pi` →
/// `pi`). Mirrors the argv[0] extraction in [`command_program_is_claude`].
fn argv0_basename(cmd: &str) -> Option<&str> {
    let program = cmd
        .split_whitespace()
        .next()
        .map(|s| s.trim_matches(['\'', '"']))?;
    std::path::Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
}

/// lsview A4 — R2's DETECTION predicate: which non-claude harness (if any) a
/// `ps` `command=` row matches. The tokens + shapes are the census contract
/// (`findings/R2-bareproc-census.md` §4):
///   - **pi**: ANCHORED exact-match — the whole trimmed command is `pi` (pi
///     renames its process title to `pi`, masking argv+env). NEVER
///     `contains("pi")` — that matched 21/714 ambient decoys
///     (`pid`/`ppid`/`pidfile`/`spindump`/…).
///   - **codex** / **opencode**: `contains(...)` — 0/714 ambient false positives
///     each; the substring covers both bare and path-launched sessions (the
///     representative-pick below drops the wrapper rows a path launch also
///     matches).
/// pi is tested first (its anchored form cannot overlap the `contains` tokens).
fn bare_provider_for(cmd: &str) -> Option<&'static str> {
    if cmd.trim() == "pi" {
        return Some("pi");
    }
    if cmd.contains("codex") {
        return Some("codex");
    }
    if cmd.contains("opencode") {
        return Some("opencode");
    }
    None
}

/// lsview A4 — classify the process table into bare non-claude harness session
/// candidates (PURE; the `RealProcessTable` method layers `lsof` cwd on top).
///
/// Two-stage per R2 §4: (1) DETECTION — [`bare_provider_for`] tags a row's
/// harness; (2) REPRESENTATIVE-PICK — keep ONLY the canonical session row, the
/// one whose argv[0] basename IS the harness executable (`codex` / `opencode` /
/// `pi`). That single test drops every non-session match a `contains` predicate
/// also catches: codex's `node` shim and `codex-mcp` child (argv[0] `node`), and
/// opencode's `script`/`bash`/`tmux`/`env`/`nohup` path-launch wrappers (argv[0]
/// the wrapper, not `opencode`). Because the pick is a SUBSET of the detection
/// matches, it can only ever be as clean or cleaner than R2's 0/714 predicate —
/// it never widens the match. Result is sorted by (provider, pid) for a
/// deterministic downstream dedup.
pub fn classify_bare_nonclaude(rows: &HashMap<i32, ProcRow>) -> Vec<BareCandidate> {
    let mut out: Vec<BareCandidate> = rows
        .iter()
        .filter_map(|(pid, row)| {
            let provider = bare_provider_for(&row.cmd)?;
            // Representative-pick: only the canonical harness-executable row is a
            // session (drops shims, MCP children, and launch wrappers).
            (argv0_basename(&row.cmd) == Some(provider)).then_some(BareCandidate {
                provider,
                pid: *pid,
            })
        })
        .collect();
    out.sort_by(|a, b| a.provider.cmp(b.provider).then(a.pid.cmp(&b.pid)));
    out
}

/// The `lsof` argv for a single pid's cwd read (R2 §4 recipe): `lsof -a -p <pid>
/// -d cwd -Fn`. `-a` ANDs the `-p`/`-d` filters; `-Fn` emits the name field so
/// the cwd path arrives on an `n`-prefixed line.
fn bare_cwd_lsof_args(pid: i32) -> Vec<String> {
    vec![
        "-a".to_string(),
        "-p".to_string(),
        pid.to_string(),
        "-d".to_string(),
        "cwd".to_string(),
        "-Fn".to_string(),
    ]
}

/// PURE: parse `lsof -Fn` output → the cwd path (the first `n`-prefixed line's
/// remainder). `None` when no non-empty `n<path>` line is present (best-effort;
/// permissive — never panics on unexpected `lsof` output, the L8 posture).
pub fn parse_lsof_cwd(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix('n'))
        .map(str::to_string)
        .filter(|s| !s.is_empty())
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
    // process.kill(pid, 0): probe-only signal, errno-aware — only ESRCH is
    // death. See [`kill0_alive`].
    kill0_alive(pid)
}

/// THE `kill(pid, 0)` liveness probe, errno-aware. The single place the
/// alive/dead decision is made, so the EPERM rule cannot drift between the
/// [`ProcessTable::is_alive`] seam and the free [`is_pid_alive`] helper.
///
/// - success        → ALIVE (we may signal it, so it exists)
/// - `ESRCH`        → DEAD (no such process — the ONLY death signal)
/// - anything else  → ALIVE (`EPERM`: the pid exists but policy forbids
///   signalling it — a foreign-uid pid, or ANY pid when the caller runs under a
///   sandbox that denies process access)
///
/// Failing toward ALIVE is the safe direction: a session wrongly reported dead
/// is silently dropped from every carrier/liveness decision, whereas a session
/// wrongly reported alive fails loudly at its next real operation.
pub fn kill0_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
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

/// A process table whose reads are REFUSED — the sandboxed `ps` a real host
/// produces. Kept as its OWN type rather than a flag on
/// [`FixtureProcessTable`] so no existing fixture construction changes: a
/// refused read is a different KIND of table, not a variation of a working one.
///
/// `is_alive` answers `true`, matching the errno-aware [`kill0_alive`]: under a
/// sandbox, `kill(pid, 0)` fails with `EPERM`, which means alive-but-
/// unsignalable, never dead.
#[derive(Debug, Clone, Copy)]
pub struct DeniedProcessTable {
    pub errno: i32,
}

impl Default for DeniedProcessTable {
    fn default() -> Self {
        Self { errno: libc::EPERM }
    }
}

impl DeniedProcessTable {
    fn err<T>(&self) -> io::Result<T> {
        Err(io::Error::from_raw_os_error(self.errno))
    }
}

impl ProcessTable for DeniedProcessTable {
    fn ppid_map(&self) -> io::Result<HashMap<i32, i32>> {
        self.err()
    }
    fn is_alive(&self, pid: i32) -> bool {
        pid > 0
    }
    fn claude_procs(&self) -> io::Result<Vec<ProcInfo>> {
        self.err()
    }
    fn cmdline(&self, _pid: i32) -> Option<String> {
        None
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

    // --- kill0_alive: errno-aware liveness -------------------------------

    /// Our own pid is trivially alive.
    #[test]
    fn kill0_alive_says_alive_for_a_signalable_process() {
        assert!(kill0_alive(std::process::id() as i32));
    }

    /// Non-positive pids are not processes. `kill(0, …)` addresses the whole
    /// process GROUP and `kill(-1, …)` every process we may signal, so these
    /// must never reach the syscall.
    #[test]
    fn kill0_alive_rejects_non_positive_pids_without_signalling() {
        assert!(!kill0_alive(0));
        assert!(!kill0_alive(-1));
        assert!(!kill0_alive(i32::MIN));
    }

    /// THE rule: only `ESRCH` is death. `pid 1` (launchd/init) is owned by root,
    /// so an unprivileged `kill(1, 0)` fails with `EPERM` — and EPERM means the
    /// process EXISTS but we may not signal it. Reporting it dead is exactly the
    /// bug: under a sandbox that same EPERM arrives for our OWN sessions, and
    /// every one of them reads as dead.
    ///
    /// Skipped when running as root, where the call succeeds outright and the
    /// EPERM branch is unreachable.
    #[test]
    fn kill0_alive_never_converts_eperm_into_death() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        assert!(
            kill0_alive(1),
            "pid 1 exists; EPERM must never be read as 'no such process'"
        );
    }

    /// A pid that genuinely does not exist IS dead — the fix must not make
    /// everything unconditionally alive. `ESRCH` remains the one death signal.
    #[test]
    fn kill0_alive_still_reports_a_genuinely_absent_pid_as_dead() {
        // Walk down from the max pid for an unused one. On any real host the
        // high pid space is sparse, so this terminates immediately in practice.
        let absent = (1..2000)
            .map(|i| i32::MAX - i)
            .find(|&pid| unsafe { libc::kill(pid, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH));
        let Some(absent) = absent else {
            return; // no ESRCH pid available; nothing to assert
        };
        assert!(
            !kill0_alive(absent),
            "ESRCH is the death signal and must still report dead"
        );
    }

    /// Both liveness seams route through the same probe, so the EPERM rule
    /// cannot drift between them.
    #[test]
    fn both_liveness_seams_agree() {
        let table = RealProcessTable::new(crate::exec::RealExec);
        for pid in [std::process::id() as i32, 1, 0, -5] {
            assert_eq!(
                table.is_alive(pid),
                is_pid_alive(pid),
                "the trait seam and the free helper must agree for pid {pid}"
            );
        }
    }

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

    // ===================== lsview A4: bare non-claude detection =====================

    /// R2's DETECTION predicate, per harness. codex/opencode are `contains`; pi is
    /// the ANCHORED exact-match (`cmd.trim() == "pi"`), NEVER `contains("pi")`.
    #[test]
    fn bare_provider_for_matches_r2_predicates() {
        // codex — bare, node shim, native path, and codex-mcp child all contain it.
        assert_eq!(bare_provider_for("codex"), Some("codex"));
        assert_eq!(
            bare_provider_for("node /opt/homebrew/bin/codex"),
            Some("codex")
        );
        assert_eq!(
            bare_provider_for("/opt/homebrew/lib/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex"),
            Some("codex")
        );
        // opencode — bare + path.
        assert_eq!(bare_provider_for("opencode"), Some("opencode"));
        assert_eq!(
            bare_provider_for("/opt/homebrew/bin/opencode /tmp/proj"),
            Some("opencode")
        );
        // pi — ANCHORED: only the exact trimmed "pi" (title-masked). Padding OK.
        assert_eq!(bare_provider_for("pi"), Some("pi"));
        assert_eq!(bare_provider_for("  pi  "), Some("pi"));
    }

    /// The pi predicate must be ANCHORED — none of R2's 21/714 `contains("pi")`
    /// ambient decoys (`pid`/`ppid`/`pidfile`/`spindump`/`--capture-python…`/a
    /// running `ps` matching itself/…) may match ANY provider.
    #[test]
    fn bare_provider_for_rejects_the_contains_pi_decoys() {
        for decoy in [
            "/usr/sbin/spindump",
            "limactl hostagent --pidfile /run/lima/ha.pid",
            "/Applications/Dropbox.app/Contents/MacOS/Dropbox --capture-python-tracebacks",
            "ps -eo pid=,ppid=,command=",
            "/opt/homebrew/bin/pidof something",
            "python /some/pipeline.py",
            "Claude Helper (Renderer)",
        ] {
            assert_eq!(
                bare_provider_for(decoy),
                None,
                "decoy must not match: {decoy:?}"
            );
        }
        // And a claude row is never a bare NON-claude candidate.
        assert_eq!(
            bare_provider_for("claude --dangerously-skip-permissions --name wk"),
            None
        );
    }

    /// The REPRESENTATIVE-PICK collapses a codex session's multi-row tree (tmux
    /// launcher + node shim + native binary + codex-mcp child) to the ONE canonical
    /// native-binary row — the only one whose argv[0] basename is `codex`.
    #[test]
    fn classify_bare_nonclaude_codex_tree_picks_native_only() {
        let ps = "\
47493 1 tmux new-session -d -s r2codex -c /work/cwd-codex codex
47500 47493 node /opt/homebrew/bin/codex
47553 47500 /opt/homebrew/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex
47988 47553 /opt/homebrew/bin/node /home/u/.local/share/codex-mcp/macos-desktop.js
";
        let cands = classify_bare_nonclaude(&parse_ps_rows(ps));
        assert_eq!(
            cands,
            vec![BareCandidate {
                provider: "codex",
                pid: 47553
            }]
        );
    }

    /// The REPRESENTATIVE-PICK drops opencode PATH-LAUNCH wrappers (`bash`, `tmux`,
    /// `script`) — one session presents as four `contains("opencode")` rows, only the
    /// real `/…/opencode` process (argv[0] basename `opencode`) is a session (R2 §2b).
    #[test]
    fn classify_bare_nonclaude_opencode_pathlaunch_picks_real_only() {
        let ps = "\
37723 47865 /bin/bash -c eval 'script -q /tmp/t.txt /opt/homebrew/bin/opencode /tmp/proj'
37734 1 tmux new-session -d script -q /tmp/t.txt /opt/homebrew/bin/opencode /tmp/proj
37735 37734 script -q /tmp/t.txt /opt/homebrew/bin/opencode /tmp/proj
37737 37735 /opt/homebrew/bin/opencode /tmp/proj
";
        let cands = classify_bare_nonclaude(&parse_ps_rows(ps));
        assert_eq!(
            cands,
            vec![BareCandidate {
                provider: "opencode",
                pid: 37737
            }]
        );
    }

    /// Full-snapshot classify: codex native + bare opencode + pi survive; the pi
    /// `contains` decoys, the claude rows, and the wrapper/shim rows do NOT. Sorted
    /// (provider, pid).
    #[test]
    fn classify_bare_nonclaude_full_snapshot_zero_false_positives() {
        let ps = "\
1 0 /sbin/launchd
6400 6398 pi
92222 92221 opencode
47500 1 node /opt/homebrew/bin/codex
47553 47500 /opt/homebrew/lib/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex
71884 71806 claude --dangerously-skip-permissions --name wk
555 1 /usr/sbin/spindump
556 1 limactl hostagent --pidfile /run/lima/ha.pid
557 1 ps -eo pid=,ppid=,command=
558 1 node /some/pipeline.js
";
        let cands = classify_bare_nonclaude(&parse_ps_rows(ps));
        assert_eq!(
            cands,
            vec![
                BareCandidate {
                    provider: "codex",
                    pid: 47553
                },
                BareCandidate {
                    provider: "opencode",
                    pid: 92222
                },
                BareCandidate {
                    provider: "pi",
                    pid: 6400
                },
            ]
        );
    }

    /// The `lsof -Fn` cwd parse: the first `n<path>` line's remainder; permissive on
    /// junk / empty / missing-name output → `None`.
    #[test]
    fn parse_lsof_cwd_reads_the_n_line() {
        assert_eq!(
            parse_lsof_cwd("p47553\nfcwd\nn/work/cwd-codex\n").as_deref(),
            Some("/work/cwd-codex")
        );
        assert_eq!(
            parse_lsof_cwd("n/only/a/path").as_deref(),
            Some("/only/a/path")
        );
        // no n-line / empty / junk → None (best-effort).
        assert_eq!(parse_lsof_cwd("p123\nfcwd\n"), None);
        assert_eq!(parse_lsof_cwd(""), None);
        assert_eq!(parse_lsof_cwd("n\n"), None, "empty path → None");
    }

    /// END-TO-END through the SAME `Exec` seam production uses: a scripted `ps`
    /// snapshot + per-pid scripted `lsof` drives the whole `bare_nonclaude_procs`
    /// path (classify → representative-pick → lsof cwd → dedup) with NO real
    /// process launched. codex+opencode get cwds; pi's `lsof` is unscripted →
    /// best-effort `None` (still rendered). Sorted (provider, pid).
    #[test]
    fn bare_nonclaude_procs_end_to_end_via_scripted_exec() {
        let ps_out = "\
1 0 /sbin/launchd
6400 6398 pi
92222 92221 opencode
47500 1 node /opt/homebrew/bin/codex
47553 47500 /opt/homebrew/lib/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex
71884 71806 claude --dangerously-skip-permissions --name wk
555 1 limactl hostagent --pidfile /run/lima/ha.pid
";
        let exec = ScriptedExec::new()
            .on("ps", &["-eo"], Some(0), ps_out, "")
            .on(
                "lsof",
                &["-a", "-p", "47553"],
                Some(0),
                "p47553\nfcwd\nn/work/cwd-codex\n",
                "",
            )
            .on(
                "lsof",
                &["-a", "-p", "92222"],
                Some(0),
                "p92222\nfcwd\nn/work/cwd-opencode\n",
                "",
            );
        // pid 6400 (pi) has NO scripted lsof → ScriptedExec's benign empty success
        // → parse yields None → cwd best-effort None (but the row is still emitted).
        let pt = RealProcessTable::new(exec);
        let bare = pt.bare_nonclaude_procs().unwrap();
        assert_eq!(
            bare,
            vec![
                BareProc {
                    provider: "codex".into(),
                    pid: 47553,
                    cwd: Some("/work/cwd-codex".into())
                },
                BareProc {
                    provider: "opencode".into(),
                    pid: 92222,
                    cwd: Some("/work/cwd-opencode".into())
                },
                BareProc {
                    provider: "pi".into(),
                    pid: 6400,
                    cwd: None
                },
            ]
        );

        // CLAUDE BYTE-IDENTITY: the claude path is untouched — claude_procs still
        // returns exactly the claude rows, and the bare detector never claimed one.
        let claude = pt.claude_procs().unwrap();
        assert_eq!(
            claude.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![71884]
        );
        assert!(
            !bare.iter().any(|b| b.pid == 71884),
            "claude row is never a bare non-claude proc"
        );
    }

    /// Dedup backstop: two canonical native-codex rows resolving to the SAME cwd
    /// collapse to ONE (lowest pid, deterministic); a cwd-unknown row is kept
    /// distinct (visibility is the bar).
    #[test]
    fn bare_nonclaude_procs_dedups_by_provider_cwd_keeps_unknown() {
        let ps_out = "\
47553 1 /a/vendor/bin/codex
47554 1 /b/vendor/bin/codex
47600 1 /c/vendor/bin/codex
";
        let exec = ScriptedExec::new()
            .on("ps", &["-eo"], Some(0), ps_out, "")
            // 47553 and 47554 share cwd /work/shared → dedup to one (47553).
            .on(
                "lsof",
                &["-a", "-p", "47553"],
                Some(0),
                "n/work/shared\n",
                "",
            )
            .on(
                "lsof",
                &["-a", "-p", "47554"],
                Some(0),
                "n/work/shared\n",
                "",
            );
        // 47600 has no scripted lsof → cwd None → kept as a distinct row.
        let bare = RealProcessTable::new(exec).bare_nonclaude_procs().unwrap();
        assert_eq!(
            bare,
            vec![
                BareProc {
                    provider: "codex".into(),
                    pid: 47553,
                    cwd: Some("/work/shared".into())
                },
                BareProc {
                    provider: "codex".into(),
                    pid: 47600,
                    cwd: None
                },
            ]
        );
    }

    /// The DEFAULT trait impl (fixture table) exposes NO bare procs, so every
    /// existing gather/join fixture test sees an unchanged process table.
    #[test]
    fn fixture_process_table_has_no_bare_nonclaude_procs() {
        let pt = FixtureProcessTable::default();
        assert!(pt.bare_nonclaude_procs().unwrap().is_empty());
    }
}
