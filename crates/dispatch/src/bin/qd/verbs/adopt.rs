//! `qd wrap <name>` — self-wrap preparation or fenced external wrap.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use clap::ArgMatches;

use dispatch::adoption::{
    self, AdoptRecord, IdleHeuristic, Management, RestartRecipe, ADOPT_READINESS_TIMEOUT_MS,
    ADOPT_SIGTERM_GRACE_MS,
};
use dispatch::effects::{Clock, Env, RealClock, RealEnv};
use dispatch::exec::RealExec;
use dispatch::identity::SessionIdentity;
use dispatch::model::Session;
use dispatch::mux::Mux;
use dispatch::mux_selector::Backend;
use dispatch::paths::QdPaths;

use super::{common, whoami};

pub fn run(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    run_for_query(query, m.get_flag("force"))
}

fn run_for_query(query: &str, force: bool) -> i32 {
    let target = match common::resolve_session_uncapped(query) {
        Ok(session) => session,
        Err(code) => return code,
    };

    let env = RealEnv;
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(home) => std::path::PathBuf::from(home),
        None => {
            eprintln!("qd wrap: current-session identity indeterminate (HOME is not set).");
            return 1;
        }
    };
    let paths = QdPaths::from_home_env(&home, &env);
    let rows = dispatch::effects::process_rows(&RealExec).unwrap_or_default();
    let relay_candidates = dispatch::relay::get_relay_ports(
        &paths.relay_dir,
        &dispatch::relay_http::HttpRelayProbe::new(),
    );
    let relays = adoption::verify_live_relays(
        &relay_candidates,
        &dispatch::relay_http::CcRelay::new(),
        &dispatch::effects::is_pid_alive,
    );
    let access =
        adoption::classify_session(&target, &relays, &rows, &dispatch::effects::is_pid_alive);

    if let Err(message) = require_bare_target(access.management, target_label(&target)) {
        eprintln!("{message}");
        return 1;
    }

    let mode = match adoption_mode(whoami::resolve_current_identity(&env, &RealExec), &target) {
        Ok(mode) => mode,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };

    match mode {
        AdoptionMode::SelfAdopt => run_self_adopt(&target, access.relay_port, &home, &paths),
        AdoptionMode::External => run_external_adopt(&target, force, &home, &paths),
    }
}

fn run_self_adopt(target: &Session, relay_port: Option<u16>, home: &Path, paths: &QdPaths) -> i32 {
    let Some(relay_port) = relay_port else {
        eprintln!(
            "qd wrap: \"{}\" is bare but its relay MCP server is not running, so Claude cannot call shutdown_for_adoption. Register the relay with `qd relay:register`, restart this Claude session, then retry self-wrap.",
            target_label(target)
        );
        return 1;
    };
    let _ = relay_port; // Positive tool-availability proof; the MCP call is in-process.

    let Some(pid) = target.pid else {
        eprintln!(
            "qd wrap: current-session identity indeterminate (target pid identity is incomplete)."
        );
        return 1;
    };
    let pid = pid as i32;
    let Some(start_ms) = dispatch::effects::proc_start_ms(pid) else {
        eprintln!(
            "qd wrap: current-session identity indeterminate (could not read kernel start time for target pid {pid})."
        );
        return 1;
    };
    let incarnation = read_incarnation(home, target.name.as_deref());
    let record = AdoptRecord::prepared(
        target_label(target).to_string(),
        target.session_id.clone(),
        SessionIdentity::new(target.session_id.clone(), pid, start_ms, incarnation),
        target.cwd.clone(),
        RealClock.now_ms(),
    );
    if let Err(e) = adoption::write_prepared(&paths.state_dir, &record) {
        eprintln!("qd wrap: could not prepare self-wrap state: {e}. Session left running.");
        return 1;
    }

    println!(
        "Self-wrap prepared for \"{}\".\n\
         Claude Code: call the relay MCP tool `shutdown_for_adoption` now.\n\
         It will register pending-wrap state, print the manual qrmux restart command to this terminal, and terminate this Claude process. It will never restart automatically.",
        record.name
    );
    0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdoptionMode {
    SelfAdopt,
    External,
}

fn adoption_mode(current: whoami::Resolution, target: &Session) -> Result<AdoptionMode, String> {
    match current {
        whoami::Resolution::Full(answer) => match self_comparison(&answer, target) {
            Ok(true) => Ok(AdoptionMode::SelfAdopt),
            Ok(false) => Ok(AdoptionMode::External),
            Err(reason) => Err(format!(
                "qd wrap: current-session identity indeterminate ({reason})."
            )),
        },
        // A caller with no managed/Claude identity is the ordinary external
        // operator shell. This is positive non-self context, not a failure.
        whoami::Resolution::NotManaged => Ok(AdoptionMode::External),
        whoami::Resolution::PartialCold(_) | whoami::Resolution::Indeterminate => Err(
            "qd wrap: current-session identity indeterminate; refusing to decide whether the target is self or external."
                .to_string(),
        ),
    }
}

fn target_label(target: &Session) -> &str {
    target
        .name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&target.session_id)
}

fn require_bare_target(management: Management, label: &str) -> Result<(), String> {
    match management {
        Management::Bare => Ok(()),
        Management::Managed => Err(format!(
            "qd wrap: \"{label}\" is already managed and receivable; there is nothing to wrap."
        )),
        Management::NotApplicable => Err(format!(
            "qd wrap: \"{label}\" is not a live Claude Code session; only a live bare session can self-wrap."
        )),
    }
}

/// Compare the complete live row returned by whoami with the resolved target.
/// A stable UUID mismatch proves external. A UUID match still requires the
/// `(pid,start_ms)` incarnation pair; missing or conflicting facts are
/// indeterminate, never guessed self.
fn self_comparison(current: &whoami::Answer, target: &Session) -> Result<bool, String> {
    if current.session_id != target.session_id {
        return Ok(false);
    }
    let current_pid = current
        .pid
        .ok_or_else(|| "whoami returned no pid".to_string())?;
    let target_pid = target
        .pid
        .ok_or_else(|| "target returned no pid".to_string())?;
    let current_start = current
        .started_at_ms
        .ok_or_else(|| "whoami returned no process start".to_string())?;
    let target_start = target
        .started_at_ms
        .ok_or_else(|| "target returned no process start".to_string())?;
    if current_pid != target_pid || current_start != target_start {
        return Err(format!(
            "the UUID matches but process identities differ: whoami=({current_pid},{current_start}), target=({target_pid},{target_start})"
        ));
    }
    Ok(true)
}

fn read_incarnation(home: &std::path::Path, name: Option<&str>) -> u64 {
    let Some(name) = name else { return 0 };
    let Some(file) = dispatch::registry::claim_file_name(name) else {
        return 0;
    };
    std::fs::read(home.join(".claude").join("claims").join(file))
        .map(|bytes| dispatch::registry::claim_incarnation(&bytes))
        .unwrap_or(0)
}

const FORCE_NOTE: &str = "qd wrap: --force skipped only the best-effort idle heuristic; identity fences, SIGTERM-only grace, relaunch identity, and readiness checks remain enabled.";

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelaunchEvidence {
    observed_name: Option<String>,
    observed_session_id: Option<String>,
    dev_channels_observed: bool,
    resume_boot_success: bool,
    managed_relay_proof: bool,
    diagnostic: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RelaunchReap {
    pid: i32,
    exited: bool,
}

trait ExternalAdoptOps {
    fn idle_heuristic(&mut self, claude_pid: i32) -> Result<IdleHeuristic, String>;
    fn write_prepared(&mut self, record: &AdoptRecord) -> Result<(), String>;
    fn register_pending(&mut self, record: &AdoptRecord) -> Result<(), String>;
    fn verify_kill_fence(&mut self, record: &AdoptRecord) -> Result<(), String>;
    fn send_sigterm(&mut self, pid: i32) -> Result<(), String>;
    fn wait_for_sigterm_exit(&mut self, pid: i32) -> Result<bool, String>;
    fn install_stop_hook(&mut self, record: &AdoptRecord) -> Result<PathBuf, String>;
    fn relaunch(
        &mut self,
        record: &AdoptRecord,
        expected_name: Option<&str>,
        recipe: &RestartRecipe,
    ) -> Result<RelaunchEvidence, String>;
    fn terminate_relaunched(&mut self, name: &str) -> Result<Option<RelaunchReap>, String>;
    fn finalize(&mut self, record: &AdoptRecord) -> Result<(), String>;
    fn cleanup(&mut self, session_id: &str) -> String;
}

fn run_external_adopt(target: &Session, force: bool, home: &Path, paths: &QdPaths) -> i32 {
    let Some(pid) = target.pid else {
        eprintln!("qd wrap: external target identity is incomplete (pid is required).");
        return 1;
    };
    let pid = pid as i32;
    let Some(start_ms) = dispatch::effects::proc_start_ms(pid) else {
        eprintln!(
            "qd wrap: external target identity is incomplete (could not read kernel start time for target pid {pid})."
        );
        return 1;
    };
    let record = AdoptRecord::prepared(
        target_label(target).to_string(),
        target.session_id.clone(),
        SessionIdentity::new(
            target.session_id.clone(),
            pid,
            start_ms,
            read_incarnation(home, target.name.as_deref()),
        ),
        target.cwd.clone(),
        RealClock.now_ms(),
    );
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("qd wrap: cannot resolve the qd executable for relaunch: {error}");
            return 1;
        }
    };
    let mut ops = RealExternalOps {
        home: home.to_path_buf(),
        paths: paths.clone(),
        exe,
    };
    if force {
        eprintln!("{FORCE_NOTE}");
    }
    match execute_external_adopt(target.name.as_deref(), &record, force, &mut ops) {
        Ok(()) => {
            for line in external_success_lines(&record.name) {
                println!("{line}");
            }
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn execute_external_adopt(
    expected_name: Option<&str>,
    record: &AdoptRecord,
    force: bool,
    ops: &mut dyn ExternalAdoptOps,
) -> Result<(), String> {
    if !force {
        match ops.idle_heuristic(record.identity.pid)? {
            IdleHeuristic::ProbablyIdle => {}
            IdleHeuristic::Busy(children) => {
                let observed = children
                    .iter()
                    .map(|child| child.description())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(busy_error(&record.name, &observed));
            }
        }
    }

    if let Err(error) = ops.write_prepared(record) {
        return Err(fail_with_cleanup(
            format!(
                "qd wrap: could not write prepared wrap state: {error}. No signal was sent."
            ),
            record,
            ops,
        ));
    }
    if let Err(error) = ops.register_pending(record) {
        return Err(fail_with_cleanup(
            format!(
                "qd wrap: registration write failed before SIGTERM: {error}. No signal was sent."
            ),
            record,
            ops,
        ));
    }
    if let Err(error) = ops.verify_kill_fence(record) {
        return Err(fail_with_cleanup(
            format!("qd wrap: kill-seam identity fence mismatch: {error}. No signal was sent."),
            record,
            ops,
        ));
    }
    if let Err(error) = ops.send_sigterm(record.identity.pid) {
        return Err(fail_with_cleanup(
            format!(
                "qd wrap: could not send SIGTERM to pid {}: {error}.",
                record.identity.pid
            ),
            record,
            ops,
        ));
    }
    match ops.wait_for_sigterm_exit(record.identity.pid) {
        Ok(true) => {}
        Ok(false) => {
            return Err(fail_with_cleanup(
                format!(
                    "qd wrap: process did not exit within {}ms after SIGTERM; the session may be mid-turn or in a protected state. If you are sure it is idle, retry with `qd wrap {} --force`.",
                    ADOPT_SIGTERM_GRACE_MS, record.name
                ),
                record,
                ops,
            ));
        }
        Err(error) => {
            return Err(fail_with_cleanup(
                format!("qd wrap: could not verify process exit after SIGTERM: {error}."),
                record,
                ops,
            ));
        }
    }

    let settings = match ops.install_stop_hook(record) {
        Ok(path) => path,
        Err(error) => {
            return Err(fail_with_cleanup(
                format!("qd wrap: Stop-hook installation failed before relaunch: {error}."),
                record,
                ops,
            ));
        }
    };
    let recipe = RestartRecipe::for_adoption(
        &record.name,
        &record.session_id,
        record.cwd.as_deref(),
        settings,
    );
    let evidence = match ops.relaunch(record, expected_name, &recipe) {
        Ok(evidence) => evidence,
        Err(error) => {
            return Err(fail_with_cleanup(
                format!("qd wrap: qrmux relaunch failed: {error}. Session not marked managed."),
                record,
                ops,
            ));
        }
    };

    let any_identity_observed =
        evidence.observed_session_id.is_some() || evidence.observed_name.is_some();
    if any_identity_observed
        && (evidence.observed_session_id.as_deref() != Some(record.session_id.as_str())
            || evidence.observed_name.as_deref() != expected_name)
    {
        return Err(fail_after_relaunch_with_cleanup(
            format!(
                "qd wrap: resume identity mismatch: expected name={expected_name:?} sessionId={:?}, observed name={:?} sessionId={:?}. Session not marked managed.",
                record.session_id, evidence.observed_name, evidence.observed_session_id
            ),
            record,
            ops,
        ));
    }

    if !(evidence.resume_boot_success && evidence.managed_relay_proof) {
        let diagnostic = evidence
            .diagnostic
            .as_deref()
            .map(|detail| format!(" Detail: {detail}"))
            .unwrap_or_default();
        return Err(fail_after_relaunch_with_cleanup(
            format!(
                "qd wrap: readiness failed closed after {}ms: resume boot success {}; managed relay proof {}. Session not marked managed.{diagnostic}",
                ADOPT_READINESS_TIMEOUT_MS,
                confirmed_word(evidence.resume_boot_success),
                confirmed_word(evidence.managed_relay_proof),
            ),
            record,
            ops,
        ));
    }

    if !any_identity_observed {
        return Err(fail_after_relaunch_with_cleanup(
            format!(
                "qd wrap: resume identity mismatch: readiness completed but no live row proved name={expected_name:?} sessionId={:?}. Session not marked managed.",
                record.session_id
            ),
            record,
            ops,
        ));
    }

    if let Err(error) = ops.finalize(record) {
        return Err(fail_with_cleanup(
            format!("qd wrap: final managed-registration write failed: {error}. Session not marked managed."),
            record,
            ops,
        ));
    }
    Ok(())
}

fn confirmed_word(value: bool) -> &'static str {
    if value {
        "confirmed"
    } else {
        "not confirmed"
    }
}

fn busy_error(name: &str, observed: &str) -> String {
    format!(
        "qd wrap: \"{name}\" is busy by the best-effort idle heuristic: observed direct child process(es): {observed}. This check can detect some foreground tools, but generation with no running tool is externally invisible; it cannot prove the session is idle. If a human has confirmed the session is idle, rerun with `qd wrap {name} --force`."
    )
}

fn fail_with_cleanup(
    message: String,
    record: &AdoptRecord,
    ops: &mut dyn ExternalAdoptOps,
) -> String {
    format!("{message} {}", ops.cleanup(&record.session_id))
}

fn fail_after_relaunch_with_cleanup(
    message: String,
    record: &AdoptRecord,
    ops: &mut dyn ExternalAdoptOps,
) -> String {
    let reap = match ops.terminate_relaunched(&record.name) {
        Ok(Some(RelaunchReap { pid, exited: true })) => {
            format!("Relaunched Claude pid {pid} exited after SIGTERM.")
        }
        Ok(Some(RelaunchReap { pid, exited: false })) => format!(
            "Relaunched Claude pid {pid} did not exit within {ADOPT_SIGTERM_GRACE_MS}ms after SIGTERM; no stronger signal was sent."
        ),
        Ok(None) => "No live relaunched Claude process was found to terminate.".to_string(),
        Err(error) => {
            format!("Best-effort SIGTERM of the relaunched Claude process failed: {error}.")
        }
    };
    fail_with_cleanup(format!("{message} {reap}"), record, ops)
}

fn external_success_lines(name: &str) -> [String; 2] {
    [
        format!(
            "Wrapped \"{name}\": verified the same session identity and managed relay readiness."
        ),
        format!("qd attach {name}"),
    ]
}

fn external_relaunch_line(name: &str) -> String {
    format!(
        "Relaunch spawned for \"{name}\". If wrapping does not complete, reconnect with `qd attach {name}` once relaunch finishes; do not use `claude --resume`, because a second bare copy would fork the session."
    )
}

struct RealExternalOps {
    home: PathBuf,
    paths: QdPaths,
    exe: PathBuf,
}

impl ExternalAdoptOps for RealExternalOps {
    fn idle_heuristic(&mut self, claude_pid: i32) -> Result<IdleHeuristic, String> {
        let mut rows = dispatch::effects::process_rows(&RealExec)
            .map_err(|error| format!("could not enumerate the process tree: {error}"))?;
        if !rows.contains_key(&claude_pid) {
            return Err(format!(
                "qd wrap: target pid {claude_pid} disappeared before the idle heuristic"
            ));
        }
        let children: Vec<i32> = rows
            .iter()
            .filter_map(|(pid, row)| (row.ppid == claude_pid).then_some(*pid))
            .collect();
        dispatch::effects::enrich_process_argv(&mut rows, &children);
        Ok(adoption::external_idle_heuristic(claude_pid, &rows))
    }

    fn write_prepared(&mut self, record: &AdoptRecord) -> Result<(), String> {
        adoption::write_prepared(&self.paths.state_dir, record).map(|_| ())
    }

    fn register_pending(&mut self, record: &AdoptRecord) -> Result<(), String> {
        adoption::register_pending(&self.paths.state_dir, record).map(|_| ())
    }

    fn verify_kill_fence(&mut self, record: &AdoptRecord) -> Result<(), String> {
        adoption::verify_incarnation_fence(&self.home, record)?;
        let pid = record.identity.pid;
        if !dispatch::effects::is_pid_alive(pid) {
            return Err(format!("target pid {pid} is no longer alive"));
        }
        let observed_start = dispatch::effects::proc_start_ms(pid)
            .ok_or_else(|| format!("could not re-read start time for target pid {pid}"))?;
        if !adoption::start_time_fence_matches(record.identity.start_ms, observed_start) {
            return Err(format!(
                "pid {pid} start_ms disagrees beyond the 1000ms process-clock granularity allowance: prepared={}, observed={observed_start}",
                record.identity.start_ms
            ));
        }
        let argv = dispatch::effects::exact_process_argv(pid)
            .ok_or_else(|| format!("could not re-read exact argv for target pid {pid}"))?;
        let program = argv
            .first()
            .and_then(|arg| Path::new(arg).file_name())
            .and_then(|name| name.to_str());
        if program != Some("claude") {
            return Err(format!(
                "pid {pid} no longer carries the Claude process identity (exact argv={argv:?})"
            ));
        }
        // Close the small read window too: the incarnation claim must still
        // agree after the live pid/start/argv facts were re-read.
        adoption::verify_incarnation_fence(&self.home, record)
    }

    fn send_sigterm(&mut self, pid: i32) -> Result<(), String> {
        // This is the only target-process signal site in external adoption.
        // The signal is fixed in the implementation; callers cannot select a
        // stronger signal through a parameter or through --force.
        if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
            return Ok(());
        }
        if !dispatch::effects::is_pid_alive(pid) {
            return Ok(());
        }
        Err(std::io::Error::last_os_error().to_string())
    }

    fn wait_for_sigterm_exit(&mut self, pid: i32) -> Result<bool, String> {
        let deadline = Instant::now() + Duration::from_millis(ADOPT_SIGTERM_GRACE_MS);
        loop {
            if !dispatch::effects::is_pid_alive(pid) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn install_stop_hook(&mut self, record: &AdoptRecord) -> Result<PathBuf, String> {
        let command = adoption::stop_hook_command(&self.exe, &record.session_id);
        adoption::write_stop_hook_settings(&self.paths.state_dir, &record.session_id, &command)
    }

    fn relaunch(
        &mut self,
        record: &AdoptRecord,
        expected_name: Option<&str>,
        recipe: &RestartRecipe,
    ) -> Result<RelaunchEvidence, String> {
        let register = Command::new(&self.exe)
            .args(recipe.relay_register_args())
            .output()
            .map_err(|error| format!("could not execute relay registration: {error}"))?;
        if !register.status.success() {
            return Err(format!(
                "relay registration exited {:?}: {}",
                register.status.code(),
                output_detail(&register.stdout, &register.stderr)
            ));
        }

        let canonical = dispatch::qrmux_dir::resolve_qrmux_dir(&self.home, &RealEnv)
            .map_err(|error| format!("could not resolve qrmux socket dir: {error}"))?;
        let mux_box = common::build_mux(Backend::Embedded, &self.home, &RealEnv)
            .map_err(|code| format!("could not construct embedded qrmux adapter (exit {code})"))?;

        let mut command = Command::new(&self.exe);
        command
            .args(recipe.resume_args())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in recipe.env_pairs() {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("could not execute qd resume: {error}"))?;
        println!("{}", external_relaunch_line(&record.name));
        let deadline = Instant::now() + Duration::from_millis(ADOPT_READINESS_TIMEOUT_MS);
        let monitor = {
            let mut effects = RealResumeMonitorEffects {
                child: &mut child,
                mux: mux_box.as_ref(),
                socket_dir: &canonical,
                name: &record.name,
            };
            monitor_resume_readiness(&mut effects, deadline)?
        };

        let mut managed_relay_proof = false;
        let mut observed: Option<Session> = None;
        if monitor.resume_boot_success {
            while Instant::now() < deadline {
                // Re-resolve by session_id on every iteration. Resolving by name once at
                // boot-success is stale: the relaunched claude re-execs its pid and the
                // registry row's name is absent during the boot window → observed stays
                // None for the full 45s even as the real session goes managed in ~2s.
                let fresh = common::resolve_session_uncapped(&record.session_id).ok();
                managed_relay_proof = fresh.as_ref().is_some_and(|session| {
                    let rows = dispatch::effects::process_rows(&RealExec).unwrap_or_default();
                    let candidates = dispatch::relay::get_relay_ports(
                        &self.paths.relay_dir,
                        &dispatch::relay_http::HttpRelayProbe::new(),
                    );
                    let relays = adoption::verify_live_relays(
                        &candidates,
                        &dispatch::relay_http::CcRelay::new(),
                        &dispatch::effects::is_pid_alive,
                    );
                    adoption::classify_session(
                        session,
                        &relays,
                        &rows,
                        &dispatch::effects::is_pid_alive,
                    )
                    .management
                        == Management::Managed
                });
                if managed_relay_proof {
                    observed = fresh;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        Ok(RelaunchEvidence {
            observed_name: observed.as_ref().and_then(|session| session.name.clone()),
            observed_session_id: observed.map(|session| session.session_id),
            dev_channels_observed: monitor.dev_channels_observed,
            resume_boot_success: monitor.resume_boot_success,
            managed_relay_proof,
            diagnostic: monitor.diagnostic.or_else(|| {
                expected_name
                    .is_none()
                    .then(|| "the original session had no name".to_string())
            }),
        })
    }

    fn terminate_relaunched(&mut self, name: &str) -> Result<Option<RelaunchReap>, String> {
        let session = match common::resolve_session_uncapped(name) {
            Ok(session) => session,
            Err(_) => return Ok(None),
        };
        if session.provider != "claude-code" || !dispatch::resolve::is_live_status(session.status) {
            return Ok(None);
        }
        let Some(pid) = session.pid.filter(|pid| *pid > 0).map(|pid| pid as i32) else {
            return Ok(None);
        };
        if !dispatch::effects::is_pid_alive(pid) {
            return Ok(None);
        }

        // Rollback targets the resolved Claude pid directly. It deliberately
        // does not use Mux::kill and never escalates beyond this fixed signal.
        if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
            if !dispatch::effects::is_pid_alive(pid) {
                return Ok(Some(RelaunchReap { pid, exited: true }));
            }
            return Err(format!(
                "could not send SIGTERM to relaunched Claude pid {pid}: {}",
                std::io::Error::last_os_error()
            ));
        }
        let exited = self.wait_for_sigterm_exit(pid)?;
        Ok(Some(RelaunchReap { pid, exited }))
    }

    fn finalize(&mut self, record: &AdoptRecord) -> Result<(), String> {
        adoption::finalize_adoption(&self.paths.state_dir, record).map(|_| ())
    }

    fn cleanup(&mut self, session_id: &str) -> String {
        let mut failures = Vec::new();
        if let Err(error) = adoption::cleanup_final(&self.paths.state_dir, session_id) {
            failures.push(error);
        }
        if let Err(error) = adoption::rollback_pending(&self.paths.state_dir, session_id) {
            failures.push(error);
        }
        if let Err(error) = adoption::cleanup_prepared(&self.paths.state_dir, session_id) {
            failures.push(error);
        }
        if let Err(error) = adoption::cleanup_stop_hook(&self.paths.state_dir, session_id) {
            failures.push(error);
        }
        if failures.is_empty() {
            "Wrap state rolled back.".to_string()
        } else {
            format!("Wrap state cleanup FAILED: {}", failures.join("; "))
        }
    }
}

struct ResumeMonitor {
    dev_channels_observed: bool,
    resume_boot_success: bool,
    diagnostic: Option<String>,
}

trait ResumeMonitorEffects {
    fn now(&self) -> Instant;
    fn sleep(&mut self, duration: Duration);
    fn history(&mut self) -> std::io::Result<String>;
    fn poll_helper(&mut self) -> std::io::Result<Option<bool>>;
    fn send_helper_sigterm(&mut self);
    fn take_helper_output(&mut self) -> (String, String);
}

struct RealResumeMonitorEffects<'a> {
    child: &'a mut Child,
    mux: &'a dyn Mux,
    socket_dir: &'a Path,
    name: &'a str,
}

impl ResumeMonitorEffects for RealResumeMonitorEffects<'_> {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn history(&mut self) -> std::io::Result<String> {
        self.mux.history(self.socket_dir, self.name)
    }

    fn poll_helper(&mut self) -> std::io::Result<Option<bool>> {
        self.child
            .try_wait()
            .map(|status| status.map(|value| value.success()))
    }

    fn send_helper_sigterm(&mut self) {
        // Fixed signal: readiness timeout never escalates beyond SIGTERM.
        let _ = unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
    }

    fn take_helper_output(&mut self) -> (String, String) {
        (
            read_child_pipe(self.child.stdout.take()),
            read_child_pipe(self.child.stderr.take()),
        )
    }
}

fn monitor_resume_readiness(
    effects: &mut dyn ResumeMonitorEffects,
    deadline: Instant,
) -> Result<ResumeMonitor, String> {
    let signal_text = dispatch::boot::named_dialogs()
        .into_iter()
        .find(|dialog| dialog.key == "dev-channels")
        .map(|dialog| dialog.match_text)
        .ok_or_else(|| "the existing boot matcher has no dev-channels entry".to_string())?;
    let mut dev_channels_observed = false;
    let mut status: Option<bool> = None;

    loop {
        if !dev_channels_observed {
            if let Ok(history) = effects.history() {
                dev_channels_observed = dispatch::boot::strip_ansi(&history).contains(&signal_text);
            }
        }
        if status.is_none() {
            status = effects
                .poll_helper()
                .map_err(|error| format!("could not poll qd resume: {error}"))?;
        }
        if status == Some(false) || status == Some(true) || effects.now() >= deadline {
            break;
        }
        effects.sleep(Duration::from_millis(50));
    }

    if status.is_none() {
        // Bound the helper process too. This terminates only the waiting `qd
        // resume` subprocess; the qrmux-hosted Claude process is deliberately
        // left for inspection and is not marked managed by this adoption.
        effects.send_helper_sigterm();
        let reap_deadline = effects.now() + Duration::from_millis(ADOPT_SIGTERM_GRACE_MS);
        loop {
            status = effects
                .poll_helper()
                .map_err(|error| format!("could not reap timed-out qd resume: {error}"))?;
            if status.is_some() || effects.now() >= reap_deadline {
                break;
            }
            let remaining = reap_deadline.saturating_duration_since(effects.now());
            effects.sleep(remaining.min(Duration::from_millis(25)));
        }
    }

    if status.is_none() {
        return Ok(ResumeMonitor {
            dev_channels_observed,
            resume_boot_success: false,
            diagnostic: Some(
                "the qd resume helper did not exit after its readiness deadline".to_string(),
            ),
        });
    }
    let (stdout, stderr) = effects.take_helper_output();
    let resume_boot_success = status == Some(true);
    let detail = output_detail(stdout.as_bytes(), stderr.as_bytes());
    Ok(ResumeMonitor {
        dev_channels_observed,
        resume_boot_success,
        diagnostic: (!detail.is_empty()).then_some(detail),
    })
}

fn read_child_pipe(mut pipe: Option<impl Read>) -> String {
    let mut output = String::new();
    if let Some(pipe) = pipe.as_mut() {
        let _ = pipe.read_to_string(&mut output);
    }
    output
}

fn output_detail(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("stdout: {stdout}"),
        (true, false) => format!("stderr: {stderr}"),
        (false, false) => format!("stdout: {stdout}; stderr: {stderr}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch::adoption::ObservedChild;
    use dispatch::model::{SessionBranch, SessionStatus};

    fn target() -> Session {
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
            hosting: None,
            which_branch: SessionBranch::LiveRegistry,
        }
    }

    fn answer(uuid: &str, pid: Option<i64>, start: Option<i64>) -> whoami::Answer {
        whoami::Answer {
            name: Some("bare-one".into()),
            session_id: uuid.into(),
            pid,
            started_at_ms: start,
            qd_id: Some("ab3kx9mq".into()),
            source: "env",
        }
    }

    #[test]
    fn self_detect_requires_uuid_pid_and_start_match() {
        let t = target();
        assert_eq!(
            self_comparison(&answer("uuid-1", Some(4242), Some(1_700_000_000_000)), &t),
            Ok(true)
        );
        assert_eq!(
            self_comparison(
                &answer("uuid-other", Some(4242), Some(1_700_000_000_000)),
                &t
            ),
            Ok(false)
        );
        assert!(self_comparison(&answer("uuid-1", None, Some(1)), &t)
            .unwrap_err()
            .contains("no pid"));
        assert!(self_comparison(&answer("uuid-1", Some(999), Some(1)), &t)
            .unwrap_err()
            .contains("process identities differ"));
    }

    #[test]
    fn adopt_classification_errors_and_mode_are_explicit() {
        let t = target();
        let managed = require_bare_target(Management::Managed, "bare-one").unwrap_err();
        assert!(
            managed.contains("already managed and receivable"),
            "{managed}"
        );
        let not_live = require_bare_target(Management::NotApplicable, "bare-one").unwrap_err();
        assert!(
            not_live.contains("not a live Claude Code session"),
            "{not_live}"
        );

        assert_eq!(
            adoption_mode(
                whoami::Resolution::Full(answer("uuid-1", Some(4242), Some(1_700_000_000_000))),
                &t
            ),
            Ok(AdoptionMode::SelfAdopt)
        );
        assert_eq!(
            adoption_mode(
                whoami::Resolution::Full(answer("uuid-other", Some(4242), Some(1_700_000_000_000))),
                &t
            ),
            Ok(AdoptionMode::External)
        );
        assert_eq!(
            adoption_mode(whoami::Resolution::NotManaged, &t),
            Ok(AdoptionMode::External)
        );

        for resolution in [
            whoami::Resolution::Indeterminate,
            whoami::Resolution::PartialCold(answer("uuid-1", None, None)),
        ] {
            let indeterminate = adoption_mode(resolution, &t).unwrap_err();
            assert!(
                indeterminate.contains("current-session identity indeterminate"),
                "{indeterminate}"
            );
        }
    }

    fn record() -> AdoptRecord {
        AdoptRecord::prepared(
            "bare-one".into(),
            "uuid-1".into(),
            SessionIdentity::new("uuid-1", 4242, 1_700_000_000_000, 7),
            Some("/work".into()),
            1_800_000_000_000,
        )
    }

    struct FakeOps {
        events: Vec<&'static str>,
        idle: IdleHeuristic,
        write_error: Option<String>,
        pending_error: Option<String>,
        fence_error: Option<String>,
        signal_error: Option<String>,
        exited: bool,
        hook_error: Option<String>,
        evidence: RelaunchEvidence,
        relaunch_error: Option<String>,
        rollback_pid: Option<i32>,
        rollback_exited: bool,
        rollback_queries: Vec<String>,
        rollback_signals: Vec<i32>,
        final_error: Option<String>,
    }

    impl Default for FakeOps {
        fn default() -> Self {
            Self {
                events: Vec::new(),
                idle: IdleHeuristic::ProbablyIdle,
                write_error: None,
                pending_error: None,
                fence_error: None,
                signal_error: None,
                exited: true,
                hook_error: None,
                evidence: RelaunchEvidence {
                    observed_name: Some("bare-one".into()),
                    observed_session_id: Some("uuid-1".into()),
                    dev_channels_observed: true,
                    resume_boot_success: true,
                    managed_relay_proof: true,
                    diagnostic: None,
                },
                relaunch_error: None,
                rollback_pid: None,
                rollback_exited: true,
                rollback_queries: Vec::new(),
                rollback_signals: Vec::new(),
                final_error: None,
            }
        }
    }

    impl ExternalAdoptOps for FakeOps {
        fn idle_heuristic(&mut self, _pid: i32) -> Result<IdleHeuristic, String> {
            self.events.push("idle");
            Ok(self.idle.clone())
        }

        fn write_prepared(&mut self, _record: &AdoptRecord) -> Result<(), String> {
            self.events.push("prepared");
            self.write_error.clone().map_or(Ok(()), Err)
        }

        fn register_pending(&mut self, _record: &AdoptRecord) -> Result<(), String> {
            self.events.push("pending");
            self.pending_error.clone().map_or(Ok(()), Err)
        }

        fn verify_kill_fence(&mut self, _record: &AdoptRecord) -> Result<(), String> {
            self.events.push("fence");
            self.fence_error.clone().map_or(Ok(()), Err)
        }

        fn send_sigterm(&mut self, _pid: i32) -> Result<(), String> {
            self.events.push("term");
            self.signal_error.clone().map_or(Ok(()), Err)
        }

        fn wait_for_sigterm_exit(&mut self, _pid: i32) -> Result<bool, String> {
            self.events.push("grace");
            Ok(self.exited)
        }

        fn install_stop_hook(&mut self, _record: &AdoptRecord) -> Result<PathBuf, String> {
            self.events.push("hook");
            self.hook_error
                .clone()
                .map_or_else(|| Ok(PathBuf::from("/state/settings.json")), Err)
        }

        fn relaunch(
            &mut self,
            _record: &AdoptRecord,
            _expected_name: Option<&str>,
            _recipe: &RestartRecipe,
        ) -> Result<RelaunchEvidence, String> {
            self.events.push("relaunch");
            self.relaunch_error
                .clone()
                .map_or_else(|| Ok(self.evidence.clone()), Err)
        }

        fn terminate_relaunched(&mut self, name: &str) -> Result<Option<RelaunchReap>, String> {
            self.events.push("rollback-resolve");
            self.rollback_queries.push(name.to_string());
            let Some(pid) = self.rollback_pid else {
                return Ok(None);
            };
            self.events.push("rollback-term");
            self.rollback_signals.push(libc::SIGTERM);
            self.events.push("rollback-grace");
            if self.rollback_exited {
                self.rollback_pid = None;
            }
            Ok(Some(RelaunchReap {
                pid,
                exited: self.rollback_exited,
            }))
        }

        fn finalize(&mut self, _record: &AdoptRecord) -> Result<(), String> {
            self.events.push("final");
            self.final_error.clone().map_or(Ok(()), Err)
        }

        fn cleanup(&mut self, _session_id: &str) -> String {
            self.events.push("cleanup");
            "Wrap state rolled back.".to_string()
        }
    }

    struct FakeResumeMonitorEffects {
        base: Instant,
        elapsed: Duration,
        boot_at: Option<Duration>,
        helper_success_at: Option<Duration>,
        helper_exit_after_sigterm: Option<Duration>,
        sigterm_at: Option<Duration>,
        sigterm_calls: usize,
        history_polls: usize,
        helper_polls: usize,
        output_taken: bool,
    }

    impl FakeResumeMonitorEffects {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                elapsed: Duration::ZERO,
                boot_at: None,
                helper_success_at: None,
                helper_exit_after_sigterm: None,
                sigterm_at: None,
                sigterm_calls: 0,
                history_polls: 0,
                helper_polls: 0,
                output_taken: false,
            }
        }

        fn deadline_after(&self, duration: Duration) -> Instant {
            self.base + duration
        }
    }

    impl ResumeMonitorEffects for FakeResumeMonitorEffects {
        fn now(&self) -> Instant {
            self.base + self.elapsed
        }

        fn sleep(&mut self, duration: Duration) {
            self.elapsed += duration;
        }

        fn history(&mut self) -> std::io::Result<String> {
            self.history_polls += 1;
            if self.boot_at.is_some_and(|at| self.elapsed >= at) {
                let signal = dispatch::boot::named_dialogs()
                    .into_iter()
                    .find(|dialog| dialog.key == "dev-channels")
                    .expect("dev-channels matcher")
                    .match_text;
                Ok(format!("boot output: {signal}"))
            } else {
                Ok("Claude is starting".to_string())
            }
        }

        fn poll_helper(&mut self) -> std::io::Result<Option<bool>> {
            self.helper_polls += 1;
            if let (Some(signaled_at), Some(exit_after)) =
                (self.sigterm_at, self.helper_exit_after_sigterm)
            {
                if self.elapsed >= signaled_at + exit_after {
                    return Ok(Some(false));
                }
            }
            if self.helper_success_at.is_some_and(|at| self.elapsed >= at) {
                return Ok(Some(true));
            }
            Ok(None)
        }

        fn send_helper_sigterm(&mut self) {
            assert!(self.sigterm_at.is_none(), "helper signaled more than once");
            self.sigterm_at = Some(self.elapsed);
            self.sigterm_calls += 1;
        }

        fn take_helper_output(&mut self) -> (String, String) {
            self.output_taken = true;
            (String::new(), String::new())
        }
    }

    #[test]
    fn real_readiness_monitor_deadline_sigterms_reaps_and_rolls_back() {
        let mut effects = FakeResumeMonitorEffects::new();
        effects.helper_exit_after_sigterm = Some(Duration::from_millis(ADOPT_SIGTERM_GRACE_MS));
        let deadline = effects.deadline_after(Duration::from_millis(100));

        let monitor = monitor_resume_readiness(&mut effects, deadline).unwrap();

        assert!(!monitor.dev_channels_observed);
        assert!(!monitor.resume_boot_success);
        assert_eq!(
            effects.sigterm_calls, 1,
            "SIGTERM is the sole helper signal"
        );
        assert_eq!(effects.sigterm_at, Some(Duration::from_millis(100)));
        assert_eq!(
            effects.elapsed,
            Duration::from_millis(100 + ADOPT_SIGTERM_GRACE_MS),
            "the timed-out helper receives the full shared SIGTERM grace"
        );
        assert!(effects.helper_polls > effects.history_polls);
        assert!(effects.output_taken, "the exited helper was reaped");

        let mut ops = FakeOps::default();
        ops.evidence.dev_channels_observed = monitor.dev_channels_observed;
        ops.evidence.resume_boot_success = monitor.resume_boot_success;
        ops.evidence.managed_relay_proof = false;
        ops.evidence.diagnostic = monitor.diagnostic;
        let error =
            execute_external_adopt(Some("bare-one"), &record(), false, &mut ops).unwrap_err();
        assert!(
            error.contains("readiness failed closed after 45000ms"),
            "{error}"
        );
        assert!(
            error.contains("resume boot success not confirmed"),
            "{error}"
        );
        assert!(!ops.events.contains(&"final"));
        assert_eq!(ops.events.last(), Some(&"cleanup"));
    }

    #[test]
    fn real_readiness_monitor_succeeds_when_boot_text_arrives_before_deadline() {
        let mut effects = FakeResumeMonitorEffects::new();
        effects.boot_at = Some(Duration::from_millis(50));
        effects.helper_success_at = Some(Duration::from_millis(50));
        let deadline = effects.deadline_after(Duration::from_millis(100));

        let monitor = monitor_resume_readiness(&mut effects, deadline).unwrap();

        assert!(monitor.dev_channels_observed);
        assert!(monitor.resume_boot_success);
        assert_eq!(monitor.diagnostic, None);
        assert_eq!(effects.elapsed, Duration::from_millis(50));
        assert_eq!(effects.history_polls, 2);
        assert_eq!(effects.sigterm_calls, 0);
        assert!(effects.output_taken);
    }

    #[test]
    fn busy_foreground_child_fails_with_honest_force_guidance() {
        let mut ops = FakeOps {
            idle: IdleHeuristic::Busy(vec![ObservedChild {
                pid: 5001,
                argv: Some(vec!["/bin/bash".into(), "-lc".into(), "sleep 30".into()]),
            }]),
            ..FakeOps::default()
        };
        let error =
            execute_external_adopt(Some("bare-one"), &record(), false, &mut ops).unwrap_err();
        assert_eq!(
            error,
            "qd wrap: \"bare-one\" is busy by the best-effort idle heuristic: observed direct child process(es): pid 5001 argv=[\"/bin/bash\", \"-lc\", \"sleep 30\"]. This check can detect some foreground tools, but generation with no running tool is externally invisible; it cannot prove the session is idle. If a human has confirmed the session is idle, rerun with `qd wrap bare-one --force`."
        );
        assert_eq!(ops.events, vec!["idle"]);
    }

    #[test]
    fn force_bypasses_only_idle_check() {
        let mut ops = FakeOps {
            idle: IdleHeuristic::Busy(vec![ObservedChild {
                pid: 5001,
                argv: None,
            }]),
            ..FakeOps::default()
        };
        execute_external_adopt(Some("bare-one"), &record(), true, &mut ops).unwrap();
        assert_eq!(
            ops.events,
            vec!["prepared", "pending", "fence", "term", "grace", "hook", "relaunch", "final"]
        );
        assert_eq!(FORCE_NOTE, "qd wrap: --force skipped only the best-effort idle heuristic; identity fences, SIGTERM-only grace, relaunch identity, and readiness checks remain enabled.");
    }

    #[test]
    fn kill_fence_mismatch_never_signals_and_rolls_back() {
        let mut ops = FakeOps {
            fence_error: Some("pid reused".into()),
            ..FakeOps::default()
        };
        let error =
            execute_external_adopt(Some("bare-one"), &record(), false, &mut ops).unwrap_err();
        assert!(
            error.contains("kill-seam identity fence mismatch"),
            "{error}"
        );
        assert!(error.contains("No signal was sent"), "{error}");
        assert_eq!(
            ops.events,
            vec!["idle", "prepared", "pending", "fence", "cleanup"]
        );
    }

    #[test]
    fn sigterm_grace_timeout_rolls_back_without_relaunch() {
        let mut ops = FakeOps {
            exited: false,
            ..FakeOps::default()
        };
        let error =
            execute_external_adopt(Some("bare-one"), &record(), false, &mut ops).unwrap_err();
        assert!(
            error.contains("did not exit within 5000ms after SIGTERM"),
            "{error}"
        );
        assert!(error.contains("qd wrap bare-one --force"), "{error}");
        assert_eq!(
            ops.events,
            vec!["idle", "prepared", "pending", "fence", "term", "grace", "cleanup"]
        );
    }

    #[test]
    fn resume_identity_mismatch_is_not_finalized() {
        let mut ops = FakeOps::default();
        ops.evidence.observed_session_id = Some("wrong-uuid".into());
        let error =
            execute_external_adopt(Some("bare-one"), &record(), false, &mut ops).unwrap_err();
        assert!(error.contains("resume identity mismatch"), "{error}");
        assert!(!ops.events.contains(&"final"));
        assert_eq!(ops.events.last(), Some(&"cleanup"));
    }

    #[test]
    fn failed_readiness_sigterms_relaunched_process_reaps_before_rollback() {
        let mut ops = FakeOps {
            rollback_pid: Some(5252),
            ..FakeOps::default()
        };
        ops.evidence.managed_relay_proof = false;

        let error =
            execute_external_adopt(Some("bare-one"), &record(), false, &mut ops).unwrap_err();

        assert!(error.contains("Relaunched Claude pid 5252 exited after SIGTERM"));
        assert_eq!(ops.rollback_queries, vec!["bare-one"]);
        assert_eq!(ops.rollback_signals, vec![libc::SIGTERM]);
        let prohibited_stronger_signal = 9;
        assert!(
            ops.rollback_signals
                .iter()
                .all(|signal| *signal != prohibited_stronger_signal),
            "rollback sent the prohibited stronger signal"
        );
        assert_eq!(ops.rollback_pid, None, "the fake process was reaped");
        let grace = ops
            .events
            .iter()
            .position(|event| *event == "rollback-grace")
            .unwrap();
        let cleanup = ops
            .events
            .iter()
            .position(|event| *event == "cleanup")
            .unwrap();
        assert!(grace < cleanup, "{:?}", ops.events);
        assert_eq!(ops.events.last(), Some(&"cleanup"));
    }

    #[test]
    fn relaunched_process_sigterm_timeout_reports_no_escalation() {
        let mut ops = FakeOps {
            rollback_pid: Some(5252),
            rollback_exited: false,
            ..FakeOps::default()
        };
        ops.evidence.managed_relay_proof = false;

        let error =
            execute_external_adopt(Some("bare-one"), &record(), false, &mut ops).unwrap_err();

        assert!(
            error.contains(
                "Relaunched Claude pid 5252 did not exit within 5000ms after SIGTERM; no stronger signal was sent"
            ),
            "{error}"
        );
        assert_eq!(ops.rollback_signals, vec![libc::SIGTERM]);
        assert_eq!(ops.rollback_pid, Some(5252));
    }

    #[test]
    fn readiness_requires_signal_boot_and_managed_proof_before_registration() {
        // dev_channels_observed is no longer a required gate (--channels never
        // shows the dialog text; managed_relay_proof is the real proof of relay
        // readiness). Only boot success and managed relay proof are required.
        for missing in ["boot", "managed"] {
            let mut ops = FakeOps::default();
            match missing {
                "boot" => ops.evidence.resume_boot_success = false,
                _ => ops.evidence.managed_relay_proof = false,
            }
            let error =
                execute_external_adopt(Some("bare-one"), &record(), false, &mut ops).unwrap_err();
            assert!(
                error.contains("readiness failed closed after 45000ms"),
                "{error}"
            );
            assert!(
                !ops.events.contains(&"final"),
                "{missing}: {:?}",
                ops.events
            );
            assert_eq!(ops.events.last(), Some(&"cleanup"));
        }
    }

    #[test]
    fn registration_write_failures_are_explicit_and_cleaned() {
        let mut prepared = FakeOps {
            write_error: Some("directory fsync failed".into()),
            ..FakeOps::default()
        };
        let error =
            execute_external_adopt(Some("bare-one"), &record(), false, &mut prepared).unwrap_err();
        assert!(
            error.contains("could not write prepared wrap state"),
            "{error}"
        );
        assert_eq!(prepared.events.last(), Some(&"cleanup"));

        let mut pending = FakeOps {
            pending_error: Some("disk full".into()),
            ..FakeOps::default()
        };
        let error =
            execute_external_adopt(Some("bare-one"), &record(), false, &mut pending).unwrap_err();
        assert!(
            error.contains("registration write failed before SIGTERM"),
            "{error}"
        );
        assert!(!pending.events.contains(&"term"));
        assert_eq!(pending.events.last(), Some(&"cleanup"));

        let mut final_write = FakeOps {
            final_error: Some("read-only state dir".into()),
            ..FakeOps::default()
        };
        let error = execute_external_adopt(Some("bare-one"), &record(), false, &mut final_write)
            .unwrap_err();
        assert!(
            error.contains("final managed-registration write failed"),
            "{error}"
        );
        assert_eq!(final_write.events.last(), Some(&"cleanup"));
    }

    #[test]
    fn hook_installation_precedes_spawn_and_success_prints_attach() {
        let mut ops = FakeOps::default();
        execute_external_adopt(Some("bare-one"), &record(), false, &mut ops).unwrap();
        let hook = ops
            .events
            .iter()
            .position(|event| *event == "hook")
            .unwrap();
        let relaunch = ops
            .events
            .iter()
            .position(|event| *event == "relaunch")
            .unwrap();
        assert!(hook < relaunch, "{:?}", ops.events);
        assert_eq!(
            external_success_lines("bare-one"),
            [
                "Wrapped \"bare-one\": verified the same session identity and managed relay readiness.".to_string(),
                "qd attach bare-one".to_string(),
            ]
        );
        assert_eq!(
            external_relaunch_line("bare-one"),
            "Relaunch spawned for \"bare-one\". If wrapping does not complete, reconnect with `qd attach bare-one` once relaunch finishes; do not use `claude --resume`, because a second bare copy would fork the session."
        );
    }

    #[test]
    fn target_signal_site_is_term_only_structurally() {
        let source = include_str!("adopt.rs");
        let prohibited = ["SIG", "KILL"].concat();
        assert!(!source.contains(&prohibited));
        assert!(source.contains("libc::SIGTERM"));
        assert!(!source.contains(&["signal", "(9)"].concat()));
        assert!(!source.contains(&["kill", " -9"].concat()));
    }
}
