//! Self-adoption state and honest Claude-session receivability classification.
//!
//! A live Claude registry row proves that a session exists, but it does not prove
//! that Claude loaded the development channel which consumes relay notifications.
//! Likewise, a relay sidecar alone proves only that the user-scope MCP server was
//! spawned.  A session is [`Management::Managed`] only when all of the following
//! positive facts agree in one process snapshot:
//!
//! - the registry-backed session is live and its process is `claude`;
//! - that Claude argv contains the exact relay-channel load flag;
//! - a live relay process reports the same provider session id; and
//! - the relay's ancestry reaches that Claude pid.
//!
//! Anything less is [`Management::Bare`].  This intentionally prefers false
//! negatives over the dangerous false positive "receivable".

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::effects::ProcRow;
use crate::identity::SessionIdentity;
use crate::model::{RelayHealth, Session};
use crate::relay::RelayContract;
use crate::resolve::is_live_status;

const ADOPTION_DIR: &str = "adoption";
const PREPARED_DIR: &str = "prepared";
const PENDING_DIR: &str = "pending";
const FINAL_DIR: &str = "final";
const HOOKS_DIR: &str = "hooks";
const HEALTH_TIMEOUT_MS: u64 = 500;

/// `ps -o etime=` has one-second resolution. Two reads of the same process can
/// therefore differ by one second solely because they straddle a display tick.
/// Adoption's destructive seam accepts no wider start-time disagreement.
pub const ADOPT_START_TIME_SLACK_MS: u64 = 1_000;
pub const ADOPT_SIGTERM_GRACE_MS: u64 = 5_000;
pub const ADOPT_READINESS_TIMEOUT_MS: u64 = 45_000;

const RESTART_MUX: &str = "embedded";
const RELAY_REGISTER_VERB: &str = "relay:register";
// --channels avoids the interactive first-run confirmation dialog that
// --dangerously-load-development-channels triggers on a fresh --session-id
// start (no prior approval record). Both flags load server:relay; --channels
// is the non-interactive form Claude itself recommends in the dialog text.
const RESTART_FLAGS: &[&str] = &[
    "--dangerously-skip-permissions",
    "--channels",
    "server:relay",
];

/// The public bare/managed vocabulary used by `qd ls`, `qd adopt`, and relay
/// sends. Non-Claude and stopped rows are outside this classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Management {
    Bare,
    Managed,
    NotApplicable,
}

impl Management {
    pub fn as_str(self) -> &'static str {
        match self {
            Management::Bare => "bare",
            Management::Managed => "managed",
            Management::NotApplicable => "-",
        }
    }
}

/// Result of the intentionally limited external-idle heuristic. An empty or
/// support-process-only direct-child census is named `ProbablyIdle`: it is not
/// an assertion that Claude is between turns, because text generation has the
/// same externally visible process tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleHeuristic {
    ProbablyIdle,
    Busy(Vec<ObservedChild>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedChild {
    pub pid: i32,
    pub argv: Option<Vec<String>>,
}

impl ObservedChild {
    pub fn description(&self) -> String {
        match &self.argv {
            Some(argv) => format!("pid {} argv={argv:?}", self.pid),
            None => format!("pid {} (exact argv unavailable)", self.pid),
        }
    }
}

/// Classify only the target Claude pid's direct children, using their exact OS
/// argv elements. `ProcRow::cmd` (flattened `ps` display text) is deliberately
/// ignored. Any child other than a pure caffeinate keep-alive or relay channel
/// sidecar is positive evidence of foreground work. An unreadable
/// argv is conservatively busy because it cannot be proved to be either exempt
/// support process.
pub fn external_idle_heuristic(claude_pid: i32, rows: &HashMap<i32, ProcRow>) -> IdleHeuristic {
    let mut children: Vec<ObservedChild> = rows
        .iter()
        .filter(|(_, row)| row.ppid == claude_pid)
        .map(|(pid, row)| ObservedChild {
            pid: *pid,
            argv: row.argv.clone(),
        })
        .collect();
    children.sort_by_key(|child| child.pid);

    let busy: Vec<ObservedChild> = children
        .into_iter()
        .filter(|child| {
            let Some(argv) = child.argv.as_deref() else {
                return true;
            };
            !is_caffeinate_keep_alive(argv) && !is_relay_sidecar(argv)
        })
        .collect();
    if busy.is_empty() {
        IdleHeuristic::ProbablyIdle
    } else {
        IdleHeuristic::Busy(busy)
    }
}

fn argv_program(argv: &[String]) -> Option<&str> {
    Path::new(argv.first()?)
        .file_name()
        .and_then(|name| name.to_str())
}

fn is_caffeinate_keep_alive(argv: &[String]) -> bool {
    if argv_program(argv) != Some("caffeinate") {
        return false;
    }

    // Closed macOS caffeinate(8) keep-alive grammar. Boolean assertion flags
    // may be separate or combined; -t and -w each consume one numeric value.
    // Anything else can be a wrapped foreground command, so fail closed.
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-t" | "-w" => {
                let Some(value) = args.next() else {
                    return false;
                };
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return false;
                }
            }
            flags
                if flags.len() > 1
                    && flags.starts_with('-')
                    && flags[1..]
                        .bytes()
                        .all(|byte| matches!(byte, b'd' | b'i' | b'm' | b's' | b'u')) =>
            {
                // Pure boolean keep-alive flags, including combined forms.
            }
            _ => return false,
        }
    }
    true
}

fn is_relay_sidecar(argv: &[String]) -> bool {
    if argv_program(argv) == Some("qd") && argv.get(1).is_some_and(|arg| arg == "relay:serve") {
        return true;
    }
    // Claude's development-channel launcher uses this complete package-runner
    // shape:
    //   bun run --cwd ~/.claude/channels/relay --shell=bun --silent start
    // Keep the relay-directory check at that fixed cwd slot. In particular,
    // relay-looking prompt text, eval payloads, and unrelated option values
    // must not turn an arbitrary Bun child into an exempt support process.
    if argv_program(argv) != Some("bun")
        || argv.len() != 7
        || argv.get(1).map(String::as_str) != Some("run")
        || argv.get(2).map(String::as_str) != Some("--cwd")
        || argv.get(4).map(String::as_str) != Some("--shell=bun")
        || argv.get(5).map(String::as_str) != Some("--silent")
        || argv.get(6).map(String::as_str) != Some("start")
    {
        return false;
    }
    argv.get(3)
        .is_some_and(|cwd| cwd.ends_with("/.claude/channels/relay"))
}

pub fn start_time_fence_matches(expected_ms: i64, observed_ms: i64) -> bool {
    expected_ms.abs_diff(observed_ms) <= ADOPT_START_TIME_SLACK_MS
}

/// Positive relay proof for one session. `relay_port` may be present for a bare
/// session: that means its ordinary MCP server exists, not that Claude loaded
/// the inbound development channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionAccess {
    pub management: Management,
    pub relay_port: Option<u16>,
}

/// Turn sidecar/scan candidates into positive live relay facts. Sidecars are
/// discovery hints, not receivability proof: the HTTP server must answer now
/// and agree on port, pid, provider UUID, and status. Checking pid liveness
/// first avoids waiting on the common dead-sidecar case.
pub fn verify_live_relays(
    candidates: &[RelayHealth],
    client: &dyn RelayContract,
    is_alive: &dyn Fn(i32) -> bool,
) -> Vec<RelayHealth> {
    verify_live_relays_with(candidates, is_alive, &|port| {
        client.health(port, HEALTH_TIMEOUT_MS).ok()
    })
}

fn verify_live_relays_with(
    candidates: &[RelayHealth],
    is_alive: &dyn Fn(i32) -> bool,
    health: &dyn Fn(u16) -> Option<RelayHealth>,
) -> Vec<RelayHealth> {
    candidates
        .iter()
        .filter(|candidate| candidate.pid > 0 && is_alive(candidate.pid))
        .filter_map(|candidate| {
            let live = health(candidate.port)?;
            (live.status == "ok"
                && live.port == candidate.port
                && live.pid == candidate.pid
                && live.session_id == candidate.session_id)
                .then_some(live)
        })
        .collect()
}

/// Classify one joined session against a single relay/process snapshot.
pub fn classify_session(
    session: &Session,
    relays: &[RelayHealth],
    rows: &HashMap<i32, ProcRow>,
    is_alive: &dyn Fn(i32) -> bool,
) -> SessionAccess {
    if session.provider != "claude-code" || !is_live_status(session.status) {
        return SessionAccess {
            management: Management::NotApplicable,
            relay_port: None,
        };
    }
    let Some(pid) = session.pid.filter(|p| *p > 0).map(|p| p as i32) else {
        return SessionAccess {
            management: Management::Bare,
            relay_port: None,
        };
    };

    classify_live_claude(&session.session_id, pid, relays, rows, is_alive)
}

/// The fast-resolver form of [`classify_session`], used before a full `Session`
/// exists. The caller already proved this is a live Claude registry row.
pub fn classify_live_claude(
    session_id: &str,
    claude_pid: i32,
    relays: &[RelayHealth],
    rows: &HashMap<i32, ProcRow>,
    is_alive: &dyn Fn(i32) -> bool,
) -> SessionAccess {
    let relay_port =
        relay_for_session(session_id, claude_pid, relays, rows, is_alive).map(|r| r.port);
    let channel_loaded = rows
        .get(&claude_pid)
        .and_then(|row| row.argv.as_deref())
        .is_some_and(claude_loads_relay_channel);
    SessionAccess {
        management: if relay_port.is_some() && channel_loaded {
            Management::Managed
        } else {
            Management::Bare
        },
        relay_port,
    }
}

/// Positive proof that the exact argv belongs to Claude itself and contains the
/// relay development-channel option as standalone elements. Flattened process
/// display text is never accepted because it loses argument boundaries.
pub fn claude_loads_relay_channel(argv: &[String]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    if Path::new(program).file_name().and_then(|s| s.to_str()) != Some("claude") {
        return false;
    }
    let option_end = argv
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(argv.len());
    let options = &argv[1..option_end];
    // Match both the old --dangerously-load-development-channels flag (existing
    // managed sessions) and the new --channels flag (sessions relaunched after
    // the --session-id fix). Both forms load server:relay.
    options.windows(2).any(|w| {
        (w[0] == "--dangerously-load-development-channels" || w[0] == "--channels")
            && w[1] == "server:relay"
    }) || options.iter().any(|t| {
        t == "--dangerously-load-development-channels=server:relay"
            || t == "--channels=server:relay"
    })
}

fn relay_for_session<'a>(
    session_id: &str,
    claude_pid: i32,
    relays: &'a [RelayHealth],
    rows: &HashMap<i32, ProcRow>,
    is_alive: &dyn Fn(i32) -> bool,
) -> Option<&'a RelayHealth> {
    relays.iter().find(|relay| {
        relay.session_id == session_id
            && relay.pid > 0
            && is_alive(relay.pid)
            && ancestor_reaches(relay.pid, claude_pid, rows, 5)
    })
}

fn ancestor_reaches(
    descendant: i32,
    ancestor: i32,
    rows: &HashMap<i32, ProcRow>,
    max_depth: usize,
) -> bool {
    let mut current = descendant;
    for _ in 0..max_depth {
        let Some(row) = rows.get(&current) else {
            return false;
        };
        if row.ppid == ancestor {
            return true;
        }
        if row.ppid <= 1 || row.ppid == current {
            return false;
        }
        current = row.ppid;
    }
    false
}

/// Persisted prepared/pending record. The `identity` triple fences the request
/// to the exact process incarnation which `qd adopt` resolved through whoami.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptRecord {
    pub version: u8,
    pub state: String,
    pub name: String,
    pub session_id: String,
    pub identity: SessionIdentity,
    pub cwd: Option<String>,
    pub restart_command: String,
    pub prepared_at_ms: i64,
}

impl AdoptRecord {
    pub fn prepared(
        name: String,
        session_id: String,
        identity: SessionIdentity,
        cwd: Option<String>,
        prepared_at_ms: i64,
    ) -> Self {
        let restart_command = restart_command(&name);
        Self {
            version: 1,
            state: "prepared".to_string(),
            name,
            session_id,
            identity,
            cwd,
            restart_command,
            prepared_at_ms,
        }
    }
}

/// The exact manual command shown in `/dev/tty` and returned in the MCP result.
/// `QD_MUX=embedded` selects dispatch's qrmux adapter. The comment deliberately
/// exposes the registered MCP command so the operator can audit every required
/// piece without relying on hidden defaults.
pub fn restart_command(name: &str) -> String {
    RestartRecipe::new(name).render_manual_command()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// One audited qrmux relaunch recipe shared by the self-adopt instruction and
/// external adoption's programmatic `qd adoption:relaunch`. External adoption
/// adds only a session-scoped settings path; the mux selection and required
/// Claude flags are sourced from the same constants as the printed command.
///
/// `resume_args` deliberately uses the hidden `adoption:relaunch` verb (not
/// `qd resume <name>`) so the relaunch does not depend on a live registry row.
/// When a bare Claude exits after SIGTERM — especially a zero-turn session that
/// never wrote a JSONL file — its registry row may be deleted entirely, making
/// `qd resume <name>` fail with "No session matching". The
/// `adoption:relaunch` verb bypasses registry resolution and relaunches claude
/// directly from the known session UUID + name, surviving this gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartRecipe {
    name: String,
    session_id: String,
    cwd: Option<String>,
    settings_path: Option<PathBuf>,
}

impl RestartRecipe {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            session_id: String::new(),
            cwd: None,
            settings_path: None,
        }
    }

    pub fn with_settings(name: &str, settings_path: PathBuf) -> Self {
        Self {
            name: name.to_string(),
            session_id: String::new(),
            cwd: None,
            settings_path: Some(settings_path),
        }
    }

    /// Full constructor used by external adoption: carries the session UUID and
    /// cwd so `resume_args` can route through `adoption:relaunch` without a
    /// registry lookup.
    pub fn for_adoption(
        name: &str,
        session_id: &str,
        cwd: Option<&str>,
        settings_path: PathBuf,
    ) -> Self {
        Self {
            name: name.to_string(),
            session_id: session_id.to_string(),
            cwd: cwd.map(str::to_string),
            settings_path: Some(settings_path),
        }
    }

    /// Programmatic relaunch args for the adopt flow. Uses the hidden
    /// `adoption:relaunch` verb to bypass registry resolution, so the relaunch
    /// succeeds even when the original session's registry row was deleted on
    /// exit (the zero-turn bare-session gap).
    pub fn resume_args(&self) -> Vec<String> {
        let mut args = vec![
            "adoption:relaunch".to_string(),
            "--session-id".to_string(),
            self.session_id.clone(),
            "--name".to_string(),
            self.name.clone(),
        ];
        if let Some(cwd) = &self.cwd {
            args.push("--cwd".to_string());
            args.push(cwd.clone());
        }
        args
    }

    pub fn relay_register_args(&self) -> Vec<String> {
        vec![RELAY_REGISTER_VERB.to_string()]
    }

    pub fn env_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("QD_MUX".to_string(), RESTART_MUX.to_string()),
            ("QD_CLAUDE_FLAGS".to_string(), RESTART_FLAGS.join(" ")),
        ];
        if let Some(path) = &self.settings_path {
            pairs.push((
                "QD_ADOPTION_SETTINGS".to_string(),
                path.to_string_lossy().into_owned(),
            ));
        }
        pairs
    }

    pub fn render_manual_command(&self) -> String {
        let name = shell_quote(&self.name);
        let settings = self
            .settings_path
            .as_ref()
            .map_or_else(String::new, |path| {
                format!(
                    "QD_ADOPTION_SETTINGS={} ",
                    shell_quote(&path.to_string_lossy())
                )
            });
        format!(
            "# qrmux-managed relaunch; relay MCP command: qd relay:serve\n\
             qd {RELAY_REGISTER_VERB} && QD_MUX={RESTART_MUX} \
QD_CLAUDE_FLAGS={} \
{settings}qd resume {name} && qd attach {name}",
            shell_quote(&RESTART_FLAGS.join(" ")),
        )
    }
}

pub fn prepared_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_path(state_dir, PREPARED_DIR, session_id)
}

pub fn pending_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_path(state_dir, PENDING_DIR, session_id)
}

pub fn final_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_path(state_dir, FINAL_DIR, session_id)
}

fn state_path(state_dir: &Path, phase: &str, session_id: &str) -> PathBuf {
    state_dir
        .join(ADOPTION_DIR)
        .join(phase)
        .join(format!("{}.json", session_key(session_id)))
}

/// Stage a validated self-adoption request. This is not pending-adopt state:
/// only the MCP shutdown tool promotes it via [`register_pending`].
pub fn write_prepared(state_dir: &Path, record: &AdoptRecord) -> Result<PathBuf, String> {
    let path = prepared_path(state_dir, &record.session_id);
    atomic_write_json(&path, record)?;
    Ok(path)
}

/// Load the request prepared for this relay session. The pid match prevents a
/// stale prepared record for an older incarnation from being adopted.
pub fn load_prepared(
    state_dir: &Path,
    session_id: &str,
    claude_pid: i32,
) -> Result<AdoptRecord, String> {
    let direct = prepared_path(state_dir, session_id);
    if direct.exists() {
        let record = read_record(&direct)?;
        return validate_prepared(record, claude_pid);
    }

    // The relay's historical random-id fallback may not equal the registry UUID.
    // Fall back to an exact pid scan, but refuse ambiguity rather than guess.
    let dir = state_dir.join(ADOPTION_DIR).join(PREPARED_DIR);
    let mut matches = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Ok(record) = read_record(&entry.path()) {
                if record.identity.pid == claude_pid {
                    matches.push(record);
                }
            }
        }
    }
    match matches.len() {
        1 => validate_prepared(matches.pop().expect("len 1"), claude_pid),
        0 => Err(
            "no prepared self-adoption request for this Claude session; run `qd adopt <name>` first"
                .to_string(),
        ),
        n => Err(format!(
            "{n} prepared adoption requests match this Claude pid; refusing an ambiguous shutdown"
        )),
    }
}

fn validate_prepared(record: AdoptRecord, claude_pid: i32) -> Result<AdoptRecord, String> {
    if record.state != "prepared" {
        return Err(format!(
            "adoption request is in state {:?}, not prepared",
            record.state
        ));
    }
    if record.identity.pid != claude_pid {
        return Err(format!(
            "prepared adoption targets Claude pid {}, but this tool belongs to pid {claude_pid}",
            record.identity.pid
        ));
    }
    if record.identity.session_id != record.session_id {
        return Err(
            "prepared adoption record has conflicting stable session identities".to_string(),
        );
    }
    Ok(record)
}

/// Re-read the monotonic name claim immediately before shutdown. A newer claim
/// incarnation means this prepared writer has been fenced out and must not
/// write pending state or signal any process. Bare legacy sessions commonly
/// have no claim file; that is the backward-compatible incarnation `0`.
pub fn verify_incarnation_fence(home: &Path, record: &AdoptRecord) -> Result<(), String> {
    let Some(file) = crate::registry::claim_file_name(&record.name) else {
        return Ok(());
    };
    let current = fs::read(home.join(".claude").join("claims").join(file))
        .map(|bytes| crate::registry::claim_incarnation(&bytes))
        .unwrap_or(0);
    if record.identity.fences_out(current) {
        return Err(format!(
            "prepared adoption incarnation {} was fenced by newer name-claim incarnation {current}",
            record.identity.incarnation
        ));
    }
    Ok(())
}

/// The load-bearing state transition. A write or prepared-cleanup error is
/// returned to the MCP tool, which must not print a shutdown notice or signal
/// Claude on this path.
pub fn register_pending(state_dir: &Path, record: &AdoptRecord) -> Result<PathBuf, String> {
    register_pending_with_remover(state_dir, record, &durable_remove)
}

fn register_pending_with_remover(
    state_dir: &Path,
    record: &AdoptRecord,
    remove_prepared: &dyn Fn(&Path) -> io::Result<()>,
) -> Result<PathBuf, String> {
    let mut pending = record.clone();
    pending.state = "pending".to_string();
    let path = pending_path(state_dir, &pending.session_id);
    atomic_write_json(&path, &pending)?;
    let prepared = prepared_path(state_dir, &pending.session_id);
    if let Err(e) = remove_prepared(&prepared) {
        let rollback = match rollback_pending(state_dir, &pending.session_id) {
            Ok(()) => "pending-adopt state rolled back".to_string(),
            Err(rollback_err) => format!("pending-adopt rollback FAILED: {rollback_err}"),
        };
        return Err(format!(
            "prepared-adopt cleanup failed for {}: {e}; {rollback}",
            prepared.display()
        ));
    }
    Ok(path)
}

#[cfg(test)]
pub(crate) fn register_pending_with_remover_for_test(
    state_dir: &Path,
    record: &AdoptRecord,
    remove_prepared: &dyn Fn(&Path) -> io::Result<()>,
) -> Result<PathBuf, String> {
    register_pending_with_remover(state_dir, record, remove_prepared)
}

/// Best-effort suppression cleanup used whenever Claude remains running after a
/// pending record was written. Successful removal includes a directory fsync so
/// the rollback is crash-durable too.
pub fn rollback_pending(state_dir: &Path, session_id: &str) -> Result<(), String> {
    let path = pending_path(state_dir, session_id);
    durable_remove(&path).map_err(|e| format!("could not remove {}: {e}", path.display()))
}

/// Best-effort prepared-state cleanup for suppression paths. Callers report
/// this result alongside pending rollback so a surviving prepared record is
/// never hidden behind a clean pending-only diagnostic.
pub fn cleanup_prepared(state_dir: &Path, session_id: &str) -> Result<(), String> {
    let path = prepared_path(state_dir, session_id);
    durable_remove(&path).map_err(|e| format!("could not remove {}: {e}", path.display()))
}

/// Commit a ready adoption. The pending record must exist and agree before the
/// final record is written. A pending-cleanup failure removes the just-written
/// final record, so callers never observe a successful final transition with a
/// stranded intermediate phase.
pub fn finalize_adoption(state_dir: &Path, record: &AdoptRecord) -> Result<PathBuf, String> {
    finalize_adoption_with_remover(state_dir, record, &durable_remove)
}

fn finalize_adoption_with_remover(
    state_dir: &Path,
    record: &AdoptRecord,
    remove_pending: &dyn Fn(&Path) -> io::Result<()>,
) -> Result<PathBuf, String> {
    let pending_path = pending_path(state_dir, &record.session_id);
    let pending = read_record(&pending_path).map_err(|e| {
        format!("pending-adopt state is missing or unreadable before final registration: {e}")
    })?;
    if pending.state != "pending"
        || pending.session_id != record.session_id
        || pending.identity != record.identity
    {
        return Err("pending-adopt state disagrees with the adoption being finalized".to_string());
    }

    let mut final_record = record.clone();
    final_record.state = "final".to_string();
    let final_path = final_path(state_dir, &record.session_id);
    atomic_write_json(&final_path, &final_record)?;
    if let Err(e) = remove_pending(&pending_path) {
        let rollback = durable_remove(&final_path)
            .map(|_| "final registration rolled back".to_string())
            .unwrap_or_else(|rollback_err| {
                format!("final registration rollback FAILED: {rollback_err}")
            });
        return Err(format!(
            "pending-adopt cleanup failed for {}: {e}; {rollback}",
            pending_path.display()
        ));
    }
    Ok(final_path)
}

pub fn cleanup_final(state_dir: &Path, session_id: &str) -> Result<(), String> {
    let path = final_path(state_dir, session_id);
    durable_remove(&path).map_err(|e| format!("could not remove {}: {e}", path.display()))
}

#[cfg(test)]
pub(crate) fn finalize_adoption_with_remover_for_test(
    state_dir: &Path,
    record: &AdoptRecord,
    remove_pending: &dyn Fn(&Path) -> io::Result<()>,
) -> Result<PathBuf, String> {
    finalize_adoption_with_remover(state_dir, record, remove_pending)
}

fn session_key(session_id: &str) -> String {
    format!("{:x}", Sha256::digest(session_id.as_bytes()))
}

pub fn stop_hook_dir(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join(ADOPTION_DIR)
        .join(HOOKS_DIR)
        .join(session_key(session_id))
}

/// Remove every artifact belonging to the per-session Stop hook. Rollback may
/// run before settings are written, after events have been appended, or more
/// than once, so an already-absent hook directory is a successful cleanup.
pub fn cleanup_stop_hook(state_dir: &Path, session_id: &str) -> Result<(), String> {
    let path = stop_hook_dir(state_dir, session_id);
    match fs::remove_dir_all(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not remove {}: {error}", path.display())),
    }
}

pub fn stop_hook_settings_path(state_dir: &Path, session_id: &str) -> PathBuf {
    stop_hook_dir(state_dir, session_id).join("settings.json")
}

pub fn stop_hook_events_path(state_dir: &Path, session_id: &str) -> PathBuf {
    stop_hook_dir(state_dir, session_id).join("stop-events.jsonl")
}

/// Absolute hidden-verb command installed into the per-session Claude settings
/// file. Quoting is POSIX-shell safe for both the executable and stable id.
pub fn stop_hook_command(exe: &Path, session_id: &str) -> String {
    format!(
        "{} adoption:stop {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(session_id)
    )
}

/// Write the Claude Code settings file that scopes the authoritative Stop hook
/// to this one relaunched process via `claude --settings <path>`.
pub fn write_stop_hook_settings(
    state_dir: &Path,
    session_id: &str,
    hook_command: &str,
) -> Result<PathBuf, String> {
    let path = stop_hook_settings_path(state_dir, session_id);
    let settings = serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": hook_command
                }]
            }]
        }
    });
    atomic_write_value(&path, &settings)?;
    Ok(path)
}

/// Append one Stop event. `O_APPEND` makes concurrent hook invocations
/// record-sized atomic on the local state filesystem; `sync_data` makes a
/// successful hidden verb a durable boundary observation.
pub fn record_stop_hook_event(
    state_dir: &Path,
    session_id: &str,
    observed_at_ms: i64,
    hook_pid: u32,
) -> Result<PathBuf, String> {
    let path = stop_hook_events_path(state_dir, session_id);
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid Stop-hook event path: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    let event = serde_json::json!({
        "version": 1,
        "event": "Stop",
        "sessionId": session_id,
        "observedAtMs": observed_at_ms,
        "hookPid": hook_pid,
    });
    let mut bytes =
        serde_json::to_vec(&event).map_err(|e| format!("could not encode Stop-hook event: {e}"))?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("could not open {}: {e}", path.display()))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_data())
        .map_err(|e| format!("could not append {}: {e}", path.display()))?;
    Ok(path)
}

fn atomic_write_value(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid adoption state path: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("adopt"),
        std::process::id()
    ));
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| format!("could not encode adoption state: {e}"))?;
    bytes.push(b'\n');
    atomic_write_bytes(path, &tmp, parent, &bytes)
}

fn read_record(path: &Path) -> Result<AdoptRecord, String> {
    let bytes = fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("could not parse {}: {e}", path.display()))
}

fn atomic_write_json(path: &Path, record: &AdoptRecord) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid adoption state path: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("adopt"),
        std::process::id()
    ));
    let mut bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| format!("could not encode adoption state: {e}"))?;
    bytes.push(b'\n');
    atomic_write_bytes(path, &tmp, parent, &bytes)
}

fn atomic_write_bytes(path: &Path, tmp: &Path, parent: &Path, bytes: &[u8]) -> Result<(), String> {
    let write = || -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(tmp, path)?;
        sync_directory(parent)?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(tmp);
        return Err(format!("could not write {}: {e}", path.display()));
    }
    Ok(())
}

fn durable_remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            let parent = path.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
            })?;
            sync_directory(parent)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

/// Find the actual Claude ancestor of the relay server. The exact OS argv
/// executable must be `claude`; flattened `ps command=` display text is never
/// identity proof. The immediate bun channel wrapper is deliberately skipped.
/// Ambiguity is impossible in a linear ancestry chain, and absence is a
/// fail-closed error at the MCP boundary.
pub fn find_claude_ancestor(relay_pid: i32, rows: &HashMap<i32, ProcRow>) -> Option<i32> {
    let mut current = relay_pid;
    for _ in 0..8 {
        let row = rows.get(&current)?;
        let parent = row.ppid;
        if parent <= 1 || parent == current {
            return None;
        }
        let parent_row = rows.get(&parent)?;
        if parent_row
            .argv
            .as_deref()
            .and_then(argv_program)
            .is_some_and(|program| program == "claude")
        {
            return Some(parent);
        }
        current = parent;
    }
    None
}

/// Complete human-facing shutdown notice, written to `/dev/tty` and repeated in
/// the MCP result. It never goes to the relay's protocol stdout.
pub fn shutdown_notice(record: &AdoptRecord) -> String {
    format!(
        "qd adopt: pending adoption registered for \"{}\".\n\
         Claude Code is about to be terminated. It will NOT restart automatically.\n\
         If termination is suppressed or fails, an explicit failure line will follow.\n\
         Restart it manually in this terminal with:\n{}",
        record.name, record.restart_command
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SessionBranch, SessionStatus};
    use tempfile::tempdir;

    fn session() -> Session {
        Session {
            name: Some("bare-one".into()),
            user_named: Some(true),
            session_id: "uuid-1".into(),
            code: None,
            qd_id: Some("ab3kx9mq".into()),
            pid: Some(4242),
            status: SessionStatus::Idle,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: Some(8901),
            turns: 0,
            tokens: 0,
            cwd: Some("/work".into()),
            last_active_ms: None,
            version: None,
            started_at_ms: Some(1_700_000_000_000),
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "claude-code".into(),
            entrypoint: None,
            lineage: None,
            which_branch: SessionBranch::LiveRegistry,
        }
    }

    fn rows(channel: bool) -> HashMap<i32, ProcRow> {
        HashMap::from([
            (
                5000,
                ProcRow {
                    ppid: 4999,
                    cmd: "qd relay:serve".into(),
                    argv: None,
                },
            ),
            (
                4999,
                ProcRow {
                    ppid: 4242,
                    cmd: "bun run --cwd /home/.claude/channels/relay start".into(),
                    argv: None,
                },
            ),
            (
                4242,
                ProcRow {
                    ppid: 4000,
                    cmd: if channel {
                        "claude --dangerously-load-development-channels server:relay --name bare-one"
                            .into()
                    } else {
                        "claude --name bare-one".into()
                    },
                    argv: Some(if channel {
                        vec![
                            "claude".into(),
                            "--dangerously-load-development-channels".into(),
                            "server:relay".into(),
                            "--name".into(),
                            "bare-one".into(),
                        ]
                    } else {
                        vec!["claude".into(), "--name".into(), "bare-one".into()]
                    }),
                },
            ),
            (
                4000,
                ProcRow {
                    ppid: 1,
                    cmd: "qd qrmux-server --session bare-one".into(),
                    argv: None,
                },
            ),
        ])
    }

    fn relays() -> Vec<RelayHealth> {
        vec![RelayHealth {
            session_id: "uuid-1".into(),
            port: 8901,
            pid: 5000,
            status: "ok".into(),
        }]
    }

    #[test]
    fn classification_requires_channel_and_live_matching_relay() {
        let alive = |_: i32| true;
        assert_eq!(
            classify_session(&session(), &relays(), &rows(false), &alive),
            SessionAccess {
                management: Management::Bare,
                relay_port: Some(8901)
            }
        );
        assert_eq!(
            classify_session(&session(), &relays(), &rows(true), &alive),
            SessionAccess {
                management: Management::Managed,
                relay_port: Some(8901)
            }
        );
        let dead = |_: i32| false;
        assert_eq!(
            classify_session(&session(), &relays(), &rows(true), &dead).management,
            Management::Bare
        );
    }

    #[test]
    fn adoption_relay_verification_rejects_stale_or_disagreeing_sidecars() {
        let candidate = relays().pop().unwrap();
        let alive = |_: i32| true;
        let none = verify_live_relays_with(std::slice::from_ref(&candidate), &alive, &|_| None);
        assert!(none.is_empty(), "an unanswered sidecar is not live proof");
        let wrong_uuid = verify_live_relays_with(std::slice::from_ref(&candidate), &alive, &|_| {
            Some(RelayHealth {
                session_id: "someone-else".into(),
                ..candidate.clone()
            })
        });
        assert!(wrong_uuid.is_empty(), "health must agree with the sidecar");
        let verified = verify_live_relays_with(std::slice::from_ref(&candidate), &alive, &|_| {
            Some(candidate.clone())
        });
        assert_eq!(verified, vec![candidate]);
    }

    #[test]
    fn wrapper_merely_mentioning_claude_channels_is_never_managed() {
        assert!(!claude_loads_relay_channel(&[
            "bun".into(),
            "run".into(),
            "--dangerously-load-development-channels".into(),
            "server:relay".into(),
        ]));
        assert!(!claude_loads_relay_channel(&[
            "claude".into(),
            "--name".into(),
            "x".into(),
        ]));
        assert!(claude_loads_relay_channel(&[
            "/opt/bin/claude".into(),
            "--dangerously-load-development-channels".into(),
            "server:relay".into(),
        ]));
        assert!(claude_loads_relay_channel(&[
            "claude".into(),
            "--dangerously-load-development-channels=server:relay".into(),
        ]));
        assert!(!claude_loads_relay_channel(&[
            "claude".into(),
            "--".into(),
            "--dangerously-load-development-channels=server:relay".into(),
        ]));
        assert!(!claude_loads_relay_channel(&[
            "claude".into(),
            "--".into(),
            "--dangerously-load-development-channels".into(),
            "server:relay".into(),
        ]));
        assert!(claude_loads_relay_channel(&[
            "claude".into(),
            "--dangerously-load-development-channels".into(),
            "server:relay".into(),
            "--".into(),
            "positional prompt".into(),
        ]));
        assert!(claude_loads_relay_channel(&[
            "claude".into(),
            "--dangerously-load-development-channels=server:relay".into(),
            "--".into(),
            "positional prompt".into(),
        ]));
        // --channels server:relay is accepted (non-interactive form, used by new relaunches).
        assert!(claude_loads_relay_channel(&[
            "claude".into(),
            "--channels".into(),
            "server:relay".into(),
        ]));
        assert!(claude_loads_relay_channel(&[
            "claude".into(),
            "--channels=server:relay".into(),
        ]));
        // bun wrapper mentioning --channels is still bare.
        assert!(!claude_loads_relay_channel(&[
            "bun".into(),
            "--channels".into(),
            "server:relay".into(),
        ]));
    }

    #[test]
    fn prompt_text_containing_channel_flag_words_stays_bare() {
        let mut process_rows = rows(false);
        let claude = process_rows.get_mut(&4242).unwrap();
        claude.cmd =
            "claude ordinary prompt --dangerously-load-development-channels server:relay".into();
        claude.argv = Some(vec![
            "claude".into(),
            "ordinary prompt --dangerously-load-development-channels server:relay".into(),
        ]);

        let access = classify_session(&session(), &relays(), &process_rows, &|_| true);
        assert_eq!(access.management, Management::Bare);
        assert_eq!(
            access.relay_port,
            Some(8901),
            "healthy matching relay fixture"
        );
    }

    #[test]
    fn missing_exact_argv_fact_stays_bare_even_when_ps_text_looks_managed() {
        let mut process_rows = rows(true);
        process_rows.get_mut(&4242).unwrap().argv = None;

        assert_eq!(
            classify_session(&session(), &relays(), &process_rows, &|_| true).management,
            Management::Bare
        );
    }

    #[test]
    fn restart_command_is_complete_and_uses_attach_vocabulary() {
        let cmd = restart_command("bare-one");
        eprintln!("restart command evidence:\n{cmd}");
        for required in [
            "qrmux-managed",
            "qd relay:serve",
            "QD_MUX=embedded",
            "--channels server:relay",
            "qd resume 'bare-one'",
            "qd attach 'bare-one'",
        ] {
            assert!(cmd.contains(required), "missing {required:?}: {cmd}");
        }
        let retired_vocabulary = ["qd ", "con", "nect"].concat();
        assert!(!cmd.contains(&retired_vocabulary));
    }

    #[test]
    fn external_restart_recipe_shares_required_flags_and_adds_scoped_settings() {
        // for_adoption: the constructor used by the external-adoption flow.
        // resume_args() uses adoption:relaunch (bypasses registry resolution).
        let recipe = RestartRecipe::for_adoption(
            "bare-one",
            "uuid-bare-one",
            Some("/work/project"),
            PathBuf::from("/home with space/state/adopt-settings.json"),
        );
        assert_eq!(
            recipe.resume_args(),
            vec![
                "adoption:relaunch",
                "--session-id",
                "uuid-bare-one",
                "--name",
                "bare-one",
                "--cwd",
                "/work/project",
            ]
        );
        assert_eq!(recipe.relay_register_args(), vec!["relay:register"]);
        let env = recipe.env_pairs();
        assert!(env.contains(&("QD_MUX".into(), "embedded".into())));
        assert!(env.iter().any(|(key, value)| {
            key == "QD_CLAUDE_FLAGS"
                && value == "--dangerously-skip-permissions --channels server:relay"
        }));
        assert!(env.contains(&(
            "QD_ADOPTION_SETTINGS".into(),
            "/home with space/state/adopt-settings.json".into()
        )));
        let rendered = recipe.render_manual_command();
        assert!(rendered.contains("qd relay:register"));
        assert!(rendered.contains("QD_MUX=embedded"));
        assert!(rendered.contains("qd resume 'bare-one'"));
        assert!(rendered.contains("qd attach 'bare-one'"));

        // with_settings: legacy constructor still uses resume_args() with the
        // name only (no session-id known) — resume_args() returns adoption:relaunch
        // with an empty session-id in this case (zero-field form).
        let legacy = RestartRecipe::with_settings(
            "bare-one",
            PathBuf::from("/home with space/state/adopt-settings.json"),
        );
        // The legacy path has no session_id, so resume_args produces an
        // adoption:relaunch with empty --session-id. Document the shape.
        let legacy_args = legacy.resume_args();
        assert_eq!(legacy_args[0], "adoption:relaunch");
        assert!(legacy_args.contains(&"--name".to_string()));
        assert!(legacy_args.contains(&"bare-one".to_string()));
    }

    #[test]
    fn idle_heuristic_uses_exact_argv_and_names_empty_support_only_probably_idle() {
        let support = HashMap::from([
            (
                5001,
                ProcRow {
                    ppid: 4242,
                    cmd: "misleading display text bash sleep".into(),
                    argv: Some(vec![
                        "/usr/bin/caffeinate".into(),
                        "-i".into(),
                        "-t".into(),
                        "300".into(),
                    ]),
                },
            ),
            (
                5002,
                ProcRow {
                    ppid: 4242,
                    cmd: "anything".into(),
                    argv: Some(vec![
                        "/opt/bun".into(),
                        "run".into(),
                        "--cwd".into(),
                        "/home/.claude/channels/relay".into(),
                        "--shell=bun".into(),
                        "--silent".into(),
                        "start".into(),
                    ]),
                },
            ),
        ]);
        assert_eq!(
            external_idle_heuristic(4242, &support),
            IdleHeuristic::ProbablyIdle
        );
        assert_eq!(
            external_idle_heuristic(4242, &HashMap::new()),
            IdleHeuristic::ProbablyIdle
        );

        let mut foreground = support;
        foreground.insert(
            5003,
            ProcRow {
                ppid: 4242,
                // The display text says support; exact argv says a shell.
                cmd: "/usr/bin/caffeinate -w 4242".into(),
                argv: Some(vec!["/bin/zsh".into(), "-lc".into(), "sleep 30".into()]),
            },
        );
        assert_eq!(
            external_idle_heuristic(4242, &foreground),
            IdleHeuristic::Busy(vec![ObservedChild {
                pid: 5003,
                argv: Some(vec!["/bin/zsh".into(), "-lc".into(), "sleep 30".into()]),
            }])
        );

        foreground.get_mut(&5003).unwrap().argv = None;
        assert!(matches!(
            external_idle_heuristic(4242, &foreground),
            IdleHeuristic::Busy(children) if children[0].argv.is_none()
        ));
    }

    fn assert_direct_child_is_busy(argv: &[&str]) {
        let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_string()).collect();
        let rows = HashMap::from([(
            5001,
            ProcRow {
                ppid: 4242,
                cmd: "display text is not classification evidence".into(),
                argv: Some(argv.clone()),
            },
        )]);
        assert_eq!(
            external_idle_heuristic(4242, &rows),
            IdleHeuristic::Busy(vec![ObservedChild {
                pid: 5001,
                argv: Some(argv),
            }])
        );
    }

    fn assert_direct_child_is_probably_idle(argv: &[&str]) {
        let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_string()).collect();
        let rows = HashMap::from([(
            5001,
            ProcRow {
                ppid: 4242,
                cmd: "display text is not classification evidence".into(),
                argv: Some(argv),
            },
        )]);
        assert_eq!(
            external_idle_heuristic(4242, &rows),
            IdleHeuristic::ProbablyIdle
        );
    }

    #[test]
    fn bun_relay_path_in_option_value_is_busy() {
        assert_direct_child_is_busy(&[
            "/opt/bun",
            "run",
            "--cwd",
            "/home/.claude/channels/relay",
            "start",
        ]);
    }

    #[test]
    fn bun_relay_path_lookalike_in_cwd_is_busy() {
        assert_direct_child_is_busy(&[
            "/opt/bun",
            "run",
            "--cwd",
            "/home/.claude/channels/relay-foreground-tool",
            "--shell=bun",
            "--silent",
            "start",
        ]);
    }

    #[test]
    fn bun_relay_path_with_literal_backslashes_is_busy() {
        assert_direct_child_is_busy(&[
            "/opt/bun",
            "run",
            "--cwd",
            r"/tmp/foreground\.claude\channels\relay",
            "--shell=bun",
            "--silent",
            "start",
        ]);
    }

    #[test]
    fn bun_relay_path_in_non_script_position_is_busy() {
        assert_direct_child_is_busy(&[
            "/opt/bun",
            "run",
            "foreground-tool",
            "ordinary-input",
            "/home/.claude/channels/relay",
        ]);
    }

    #[test]
    fn bun_relay_path_in_eval_payload_is_busy() {
        assert_direct_child_is_busy(&[
            "/opt/bun",
            "--eval",
            "await Bun.file('/home/.claude/channels/relay/input').text()",
        ]);
    }

    #[test]
    fn caffeinate_pure_keep_alive_grammar_is_probably_idle() {
        assert_direct_child_is_probably_idle(&["/usr/bin/caffeinate", "-i", "-t", "300"]);
        assert_direct_child_is_probably_idle(&["/usr/bin/caffeinate", "-w", "4242"]);
        assert_direct_child_is_probably_idle(&["/usr/bin/caffeinate", "-dimsu"]);
    }

    #[test]
    fn caffeinate_wrapped_or_invalid_shapes_are_busy() {
        assert_direct_child_is_busy(&["/usr/bin/caffeinate", "long-build"]);
        assert_direct_child_is_busy(&["/usr/bin/caffeinate", "-i", "-t", "300", "some-command"]);
        assert_direct_child_is_busy(&["/usr/bin/caffeinate", "-t"]);
        assert_direct_child_is_busy(&["/usr/bin/caffeinate", "-x"]);
    }

    #[test]
    fn adoption_start_fence_uses_one_second_granularity_slack_only() {
        assert!(start_time_fence_matches(10_000, 9_000));
        assert!(start_time_fence_matches(10_000, 11_000));
        assert!(!start_time_fence_matches(10_000, 8_999));
        assert!(!start_time_fence_matches(10_000, 11_001));
    }

    #[test]
    fn prepared_to_pending_is_durable_and_removes_prepared() {
        let tmp = tempdir().unwrap();
        let rec = AdoptRecord::prepared(
            "bare-one".into(),
            "uuid-1".into(),
            SessionIdentity::new("uuid-1", 4242, 1_700_000_000_000, 0),
            Some("/work".into()),
            1_800_000_000_000,
        );
        let prepared = write_prepared(tmp.path(), &rec).unwrap();
        assert!(prepared.exists());
        let loaded = load_prepared(tmp.path(), "uuid-1", 4242).unwrap();
        assert_eq!(loaded, rec);
        let pending = register_pending(tmp.path(), &loaded).unwrap();
        assert!(pending.exists());
        assert!(!prepared.exists());
        let on_disk: AdoptRecord = serde_json::from_slice(&fs::read(pending).unwrap()).unwrap();
        assert_eq!(on_disk.state, "pending");
        assert_eq!(on_disk.identity.pid, 4242);
    }

    #[test]
    fn pending_to_final_is_durable_and_removes_pending() {
        let tmp = tempdir().unwrap();
        let rec = AdoptRecord::prepared(
            "bare-one".into(),
            "uuid-1".into(),
            SessionIdentity::new("uuid-1", 4242, 1_700_000_000_000, 0),
            Some("/work".into()),
            1_800_000_000_000,
        );
        write_prepared(tmp.path(), &rec).unwrap();
        let pending = register_pending(tmp.path(), &rec).unwrap();
        let final_record = finalize_adoption(tmp.path(), &rec).unwrap();
        assert!(!pending.exists());
        assert!(final_record.exists());
        let on_disk: AdoptRecord =
            serde_json::from_slice(&fs::read(final_record).unwrap()).unwrap();
        assert_eq!(on_disk.state, "final");
    }

    #[test]
    fn final_pending_cleanup_failure_rolls_back_final() {
        let tmp = tempdir().unwrap();
        let rec = AdoptRecord::prepared(
            "bare-one".into(),
            "uuid-1".into(),
            SessionIdentity::new("uuid-1", 4242, 1, 0),
            None,
            2,
        );
        write_prepared(tmp.path(), &rec).unwrap();
        register_pending(tmp.path(), &rec).unwrap();
        let error = finalize_adoption_with_remover_for_test(tmp.path(), &rec, &|_| {
            Err(io::Error::other("injected pending cleanup failure"))
        })
        .unwrap_err();
        assert!(error.contains("pending-adopt cleanup failed"), "{error}");
        assert!(error.contains("final registration rolled back"), "{error}");
        assert!(!final_path(tmp.path(), "uuid-1").exists());
    }

    #[test]
    fn stop_hook_settings_are_session_scoped_and_events_append() {
        let tmp = tempdir().unwrap();
        let command = stop_hook_command(Path::new("/opt/qd binary"), "uuid-'one");
        let settings = write_stop_hook_settings(tmp.path(), "uuid-'one", &command).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(value["hooks"]["Stop"][0]["hooks"][0]["type"], "command");
        assert_eq!(value["hooks"]["Stop"][0]["hooks"][0]["command"], command);
        assert!(command.contains("adoption:stop"));
        assert!(settings.starts_with(tmp.path().join("adoption").join("hooks")));

        let events = record_stop_hook_event(tmp.path(), "uuid-'one", 1234, 99).unwrap();
        record_stop_hook_event(tmp.path(), "uuid-'one", 5678, 100).unwrap();
        let event_text = fs::read_to_string(events).unwrap();
        let lines: Vec<&str> = event_text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event"], "Stop");
        assert_eq!(first["sessionId"], "uuid-'one");
    }

    #[test]
    fn stop_hook_cleanup_removes_settings_and_events_and_is_idempotent() {
        let tmp = tempdir().unwrap();
        let session_id = "uuid-cleanup";
        let settings =
            write_stop_hook_settings(tmp.path(), session_id, "/opt/qd adoption:stop").unwrap();
        let events = record_stop_hook_event(tmp.path(), session_id, 1234, 99).unwrap();

        assert!(settings.exists());
        assert!(events.exists());
        cleanup_stop_hook(tmp.path(), session_id).unwrap();
        assert!(!stop_hook_dir(tmp.path(), session_id).exists());
        cleanup_stop_hook(tmp.path(), session_id).unwrap();
    }

    #[test]
    fn pending_write_failure_is_explicit() {
        let tmp = tempdir().unwrap();
        let state_file = tmp.path().join("not-a-directory");
        fs::write(&state_file, b"x").unwrap();
        let rec = AdoptRecord::prepared(
            "bare-one".into(),
            "uuid-1".into(),
            SessionIdentity::new("uuid-1", 4242, 1, 0),
            None,
            2,
        );
        let err = register_pending(&state_file, &rec).unwrap_err();
        assert!(err.contains("could not create"), "{err}");
    }

    #[test]
    fn adoption_incarnation_fence_refuses_a_newer_claim() {
        let tmp = tempdir().unwrap();
        let claims = tmp.path().join(".claude").join("claims");
        fs::create_dir_all(&claims).unwrap();
        let record = AdoptRecord::prepared(
            "bare-one".into(),
            "uuid-1".into(),
            SessionIdentity::new("uuid-1", 4242, 1, 2),
            None,
            2,
        );
        let file = crate::registry::claim_file_name("bare-one").unwrap();
        fs::write(
            claims.join(file),
            crate::registry::claim_payload_with_incarnation(999, Some(3), 4, "bare-one", 3),
        )
        .unwrap();
        let err = verify_incarnation_fence(tmp.path(), &record).unwrap_err();
        assert!(err.contains("fenced by newer"), "{err}");
    }

    #[test]
    fn finds_claude_grandparent_not_bun_parent() {
        assert_eq!(find_claude_ancestor(5000, &rows(true)), Some(4242));

        let mut misleading = rows(true);
        let claude = misleading.get_mut(&4242).unwrap();
        claude.cmd = "claude --name bare-one".into();
        claude.argv = Some(vec!["/bin/sleep".into(), "30".into()]);
        assert_eq!(find_claude_ancestor(5000, &misleading), None);
    }
}
