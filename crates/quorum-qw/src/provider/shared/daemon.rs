//! Shared daemon-hosted-resident primitives: spawn a detached process, allocate
//! its port, probe its live cmdline for identity before signaling it, and reap
//! it. Split out of the former flat `src/create_daemon.rs`
//! (PROVIDER-REORG-SPEC.md) into `provider/shared/` because acp's, pi's AND
//! codex's daemon lanes all spawn-and-reap a resident process through these
//! SAME primitives — only the PROTOCOL each resident speaks once it is up
//! differs, and that protocol-specific half (the create pipeline, its
//! `DaemonError`/`DaemonDeps` surface, and the codex cmdline match) stayed
//! behind in [`crate::provider::codex::app_server::create`], the only harness
//! that ever drove `run_new_daemon` from this file. See that module's docs for
//! the codex-specific create sequence; this module is the vocabulary every
//! daemon lane shares to get a process up, identify it, and take it down.
//!
//! SEAMS (offline-testable by construction): spawning is behind
//! [`DaemonSpawner`] (real [`RealDaemonSpawner`] + a per-harness test fake);
//! port allocation is behind [`PortAllocator`]; cmdline identity is behind
//! [`CmdlineProbe`] (real [`real_cmdline_probe`] + a deterministic test
//! closure). Each harness's create/resume/kill choreography injects these the
//! same way the pre-split `create_daemon.rs` did.

// ===========================================================================
// A spawned daemon's handle.
// ===========================================================================

/// A spawned daemon handle — just its pid (the durable identity is the registry
/// row keyed by this pid; the daemon process is owned by the OS after detach).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnedDaemon {
    pub pid: i64,
}

// ===========================================================================
// W9 FIX M-1/Mo-2: cmdline-identity guard before treating a STORED pid as OUR
// daemon. Under exact-pid-reuse the stored daemon pid can be re-assigned by the
// OS to an UNRELATED process that happens to be a group leader; a blind
// `-pgid` SIGKILL would then reap a foreign group, and a blind alive-check
// would falsely report "running". The fix: cheaply + connectionlessly read the
// live pid's command line and require it to look like OUR daemon BEFORE
// signaling / before believing it alive. The probe is a `pid -> Option<cmdline>`
// closure ([`CmdlineProbe`]) so units feed a matching / non-matching / absent
// cmdline with no real process.
//
// This probe is the shared half only: EVERY harness's group-kill + resume-alive
// decision needs the same connectionless "read this pid's cmdline" step, but
// what counts as a MATCH is harness-specific — codex's
// [`cmdline_is_our_daemon`](crate::provider::codex::app_server::create::cmdline_is_our_daemon),
// pi's `cmdline_is_our_pi_daemon`, and acp's `cmdline_is_our_acp_daemon` each
// layer their own protocol-specific match on top of the SAME probe.
// ===========================================================================

/// A connectionless "pid → its command line" probe. Production wraps the existing
/// `ps` seam ([`real_cmdline_probe`]); tests inject a deterministic closure.
pub type CmdlineProbe<'a> = dyn Fn(i64) -> Option<String> + 'a;

/// The production cmdline probe: read one pid's command line via the existing
/// process-table `ps` seam ([`crate::effects::RealProcessTable`] over
/// [`crate::exec::RealExec`]) — the SAME single `ps` spawn point the rest of the
/// engine uses (no raw shell-out). A non-visible pid / a `ps` failure ⇒ `None`.
pub fn real_cmdline_probe(pid: i64) -> Option<String> {
    use crate::effects::{ProcessTable, RealProcessTable};
    use crate::exec::RealExec;
    if pid <= 0 || pid > i32::MAX as i64 {
        return None;
    }
    RealProcessTable::new(RealExec).cmdline(pid as i32)
}

// ===========================================================================
// Spawn + kill.
// ===========================================================================

/// The detached-spawn seam (codex-p2-spec §3.2, generalized to every daemon
/// lane). The real impl runs `std::process::Command` + `process_group(0)` with
/// stdout/stderr → a log file (the P-2-proven detach); a per-harness test fake
/// records the request + hands back a canned pid without spawning anything.
pub trait DaemonSpawner {
    /// Spawn `argv` (already including whatever transport flag the harness
    /// needs, e.g. codex's `--listen ws://…`) DETACHED, in `cwd`, with `env`
    /// overrides layered on, stdout+stderr → `log_path` (parent dirs created).
    /// Returns the spawned pid or an io error.
    fn spawn_detached(
        &self,
        argv: &[String],
        env: &[(String, String)],
        cwd: &std::path::Path,
        log_path: &std::path::Path,
    ) -> std::io::Result<SpawnedDaemon>;

    /// Kill a spawned daemon (SIGTERM → grace → SIGKILL by the recorded PGID —
    /// a launcher that exec-spawns a native child a launcher-only SIGKILL
    /// orphans, so the real impl signals the whole process group `-pgid`;
    /// instance-addressed by the pgid OUR spawn created, never a name/pattern,
    /// L10). Used by the failure-cleanup path. The fake records the pid.
    fn kill(&self, pid: i64);
}

/// The outcome of a daemon-hosted kill (always success — even the already-dead seal is
/// a success). The verb prints the success line + exits 0.
///
/// Lives HERE, with the [`DaemonSpawner::kill`] pgid ladder it reports on, because BOTH
/// daemon kill paths return it: codex's
/// [`kill_codex`](crate::provider::codex::resume::kill_codex) and ACP's
/// [`kill_acp`](crate::provider::acp::resume::kill_acp). Homing it in either resume
/// module would make one lane depend on the other's; homing it in the ladder's own
/// module makes it what it is — the shared vocabulary of that one rung.
///
/// NAMED `DaemonKillOutcome`, not `KillOutcome`, because this crate ALREADY exports a
/// [`KillOutcome`](crate::contract::KillOutcome): the `LaneOps` contract DTO, a
/// serde-carried `{reaped, tombstoned}` pair of [`Confirmation`](crate::contract::Confirmation)s
/// that crosses the qw↔qd boundary. The two are genuinely different answers — that one
/// reports what a lane's kill CONFIRMED, this one reports whether we had a live,
/// identity-matched daemon to signal in the first place — so they are not reconciled,
/// and the distinct name keeps them from shadowing each other.
#[derive(Debug, Clone, PartialEq)]
pub struct DaemonKillOutcome {
    /// Did we actually group-signal the daemon? `true` only when the recorded pid
    /// was alive AND its live command line matched OUR daemon (the W9 M-1
    /// identity guard). `false` is the already-gone / not-our-daemon edge: NO group
    /// signal was sent, but we still tombstoned (the dead-row seal).
    pub was_alive: bool,
}

// ===========================================================================
// Port allocation.
// ===========================================================================

/// A port allocator: bind `127.0.0.1:0`, read the OS port, drop the listener,
/// RE-ROLLING any port in [`RELAY_RANGE`]. Boxed so a test can inject a
/// deterministic allocator (e.g. forcing a relay-range port first to prove the
/// re-roll). The real impl is [`real_alloc_port`].
pub type PortAllocator<'a> = dyn Fn() -> std::io::Result<u16> + 'a;

/// The relay-probe port range qd scans (codex-p2-spec §3.2 + fleet lesson). A
/// daemon port landing here is RE-ROLLED so the relay scan never collides with a
/// daemon lane. Shared because [`real_alloc_port`] is the one allocator every
/// daemon-hosted lane uses to pick its listen port.
pub const RELAY_RANGE: std::ops::RangeInclusive<u16> = 8900..=9000;

/// Real port allocator (codex-p2-spec §3.2): bind `127.0.0.1:0`, read the OS
/// port, drop the listener, RE-ROLLING any port in [`RELAY_RANGE`]. A handful of
/// attempts is plenty — the OS rarely reuses a just-freed port (the codex_ws.rs
/// test-server precedent).
pub fn real_alloc_port() -> std::io::Result<u16> {
    let mut held = Vec::new();
    let port = loop {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let p = l.local_addr()?.port();
        if !RELAY_RANGE.contains(&p) {
            // Drop `l` here (end of scope) so the port is free for the daemon.
            break p;
        }
        held.push(l); // hold the bad one so the next bind differs
        if held.len() >= 64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "could not get a port outside 8900-9000 in 64 tries",
            ));
        }
    };
    drop(held);
    Ok(port)
}

// ===========================================================================
// Real seams (production).
// ===========================================================================

/// Real detached spawner (codex-p2-spec §3.2, P-2-proven): `std::process::Command`
/// + `process_group(0)`, stdin null, stdout/stderr → `log_path`, cwd set.
pub struct RealDaemonSpawner;

impl DaemonSpawner for RealDaemonSpawner {
    fn spawn_detached(
        &self,
        argv: &[String],
        env: &[(String, String)],
        cwd: &std::path::Path,
        log_path: &std::path::Path,
    ) -> std::io::Result<SpawnedDaemon> {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        if argv.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty daemon argv",
            ));
        }
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        let log_err = log.try_clone()?;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .current_dir(cwd);
        // IDENTITY INVARIANT (WS-A.2): a detached daemon NEVER inherits the
        // commissioner's session identity through the process subtree — identity is
        // explicit-injection ONLY. `std::process::Command` inherits the caller's FULL
        // env, so a daemon spawned from inside a qd session would otherwise carry the
        // commissioner's QD_SESSION_ID/CLAUDE_CODE_SESSION_ID (the leaked-identity
        // self-send / misattribution bug). Scrub them BEFORE the overlay: a provider
        // that intentionally injects its own id (ACP/codex/pi pass QD_SESSION_ID)
        // re-adds it in the loop below — Rust's env-map semantics make a later
        // `.env(k, v)` override the `env_remove`, so the injected value survives while
        // an un-injected inherited var stays gone.
        cmd.env_remove("QD_SESSION_ID");
        cmd.env_remove("CLAUDE_CODE_SESSION_ID");
        cmd.env_remove("CLAUDECODE");
        for (k, v) in env {
            cmd.env(k, v);
        }
        // The detach: own process group (setsid class) → the daemon survives qd's
        // exit + the terminal closing (P-2 GREEN).
        cmd.process_group(0);
        let child = cmd.spawn()?;
        Ok(SpawnedDaemon {
            pid: child.id() as i64,
        })
    }

    fn kill(&self, pid: i64) {
        // GROUP-scoped SIGTERM → grace → SIGKILL, addressed by the RECORDED pgid.
        //
        // W4 FINDING (revised at lead review): the homebrew/npm `codex` command is
        // a LAUNCHER that exec-spawns the native app-server as a child. AFTER a ws
        // session has touched it, the launcher IGNORES SIGTERM (>3s, verified
        // live) and only dies on SIGKILL — and SIGKILL to the LAUNCHER does NOT
        // propagate: the live-lane belt caught the NATIVE CHILD surviving as an
        // orphan after a pid-scoped kill (the implementer's run got lucky on
        // timing; the lead's re-run did not). We spawned with `process_group(0)`,
        // so the launcher pid IS the pgid of the whole daemon subtree — signaling
        // `-pgid` reaps launcher + native child together. This is still INSTANCE-
        // addressed (the pgid exists because OUR spawn created it — L10 bans
        // NAME/pattern addressing, not recorded-group addressing).
        // W7 CARRY: the kill verb for codex rows must use this same group ladder.
        if pid <= 1 || pid > i32::MAX as i64 {
            return;
        }
        let pgid = pid as i32;
        // SIGTERM the group; brief grace; SIGKILL the group. ESRCH = already gone.
        crate::safe_kill::safe_group_kill(pgid as i64, libc::SIGTERM);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        while std::time::Instant::now() < deadline {
            if !crate::effects::is_pid_alive(pgid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        crate::safe_kill::safe_group_kill(pgid as i64, libc::SIGKILL);
        // The launcher was spawned by THIS process (Command::spawn), so after a
        // SIGKILL it is a ZOMBIE until reaped. Reap it eagerly so the failure path
        // leaves no zombie (the native child re-parents to init, which reaps it).
        reap_zombie(pgid);
    }
}

/// Best-effort reap of a just-killed child pid (WNOHANG, bounded). A no-op if the
/// pid is not our child (`ECHILD`) or is already reaped. Defuses the zombie the
/// in-process `Command::spawn` + SIGKILL leaves (W4 finding).
pub fn reap_zombie(pid: i32) {
    for _ in 0..20 {
        let mut status: libc::c_int = 0;
        // SAFETY: waitpid with WNOHANG is non-blocking; pid is a specific child.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        // r == pid: reaped; r == 0: not yet exited (kill in flight) — retry; r <
        // 0: ECHILD/EINTR — not our child or gone, stop.
        if r == pid || r < 0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// WS-A.2 (pi-provider env-leak fix): prove the REAL spawner's identity scrub +
    /// override ordering in ONE live spawn. A detached daemon must NOT inherit the
    /// commissioner's QD_SESSION_ID/CLAUDE_CODE_SESSION_ID/CLAUDECODE, yet a
    /// provider's INTENTIONAL `QD_SESSION_ID` overlay must survive: `env_remove` then
    /// `.env(k, v)` ⇒ the injected value wins; `env_remove` with no re-inject ⇒ the
    /// inherited var is gone. Env mutation is serialized behind a lock and the prior
    /// values are restored (the registry.rs QD_DEBUG test idiom).
    #[test]
    fn spawn_detached_scrubs_inherited_identity_but_keeps_injected_overlay() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        let keys = ["QD_SESSION_ID", "CLAUDE_CODE_SESSION_ID", "CLAUDECODE"];
        let saved: Vec<(&str, Option<String>)> =
            keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();

        // Simulate the commissioner's identity leaking through the process subtree.
        std::env::set_var("QD_SESSION_ID", "commissioner-id");
        std::env::set_var("CLAUDE_CODE_SESSION_ID", "commissioner-cc-uuid");
        std::env::set_var("CLAUDECODE", "1");

        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("scrub.log");
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'QD=[%s]\\nCC=[%s]\\nCD=[%s]\\n' \
             \"$QD_SESSION_ID\" \"$CLAUDE_CODE_SESSION_ID\" \"$CLAUDECODE\""
                .to_string(),
        ];
        // The provider re-injects ONLY its own QD_SESSION_ID (the pi/acp/codex overlay).
        let overlay = vec![(
            "QD_SESSION_ID".to_string(),
            "injected-daemon-id".to_string(),
        )];

        RealDaemonSpawner
            .spawn_detached(&argv, &overlay, dir.path(), &log_path)
            .expect("spawn the printenv probe");

        // Detached child (own process group) → poll the log until it has all 3 lines.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let out = loop {
            let s = std::fs::read_to_string(&log_path).unwrap_or_default();
            if s.lines().count() >= 3 || std::time::Instant::now() >= deadline {
                break s;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };

        // Restore the environment BEFORE asserting (a failed assert must not leak vars).
        for (k, v) in &saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }

        // env_remove THEN env(k, v) → the injected overlay value wins.
        assert!(
            out.contains("QD=[injected-daemon-id]"),
            "the injected QD_SESSION_ID overlay survives the scrub: {out:?}"
        );
        // env_remove with NO re-inject → the inherited identity is scrubbed to absent.
        assert!(
            out.contains("CC=[]"),
            "inherited CLAUDE_CODE_SESSION_ID was scrubbed: {out:?}"
        );
        assert!(
            out.contains("CD=[]"),
            "inherited CLAUDECODE was scrubbed: {out:?}"
        );
    }

    // === Port allocator never returns 8900-9000 (mutation-evidence comment). ===
    //
    // MUTATION EVIDENCE (§13 "port allocator returns 8900-9000"): the real
    // allocator re-rolls any port in the relay range. Removing the
    // `RELAY_RANGE.contains` guard in `real_alloc_port` would let the OS hand back
    // an 8900-9000 port; this invariant (run many times) reds if that guard goes.
    #[test]
    fn real_alloc_port_never_in_relay_range() {
        for _ in 0..40 {
            let p = real_alloc_port().expect("alloc a local port");
            assert!(
                !(8900..=9000).contains(&p),
                "allocator returned a relay-range port {p}"
            );
        }
    }
}
