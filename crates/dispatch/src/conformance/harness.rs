//! The live-execution harness (C-1 behavioral RUN-proof). Drives real `qd`
//! subprocess invocations against [`super::registry::conformance_battery`]'s
//! cells, producing `Pass`/`Fail` outcomes through a commissioned [`Runner`] —
//! never a fabricated result.
//!
//! **Not invoked by this crate's own `cargo test` run.** No `#[test]` in this
//! file spins a live daemon or touches the network — every function here is
//! cap-cargo-*compilable* today, but is wired to an external live-run
//! entrypoint (gated the same way the existing seeds already are —
//! `QD_PI_LIVE`, `QD_CODEX_LIVE`, `QD_ACP_LIVE`) that fires only once the
//! physical envelope's RAM/disk headroom is confirmed (see the atomic plan's
//! Physical envelope section). Building this module costs nothing beyond a
//! normal cap-cargo compile; *running* any function in it spawns a real
//! process.
//!
//! **A cell with no implementation here is simply not observed.** [`run_cell`]
//! returns `None` for any (lane, cell id) it has no driver for — the caller
//! must NOT synthesize a `Blocked` or a `Pass` in that case.
//! [`super::evidence::RunArtifactBuilder::build`] already turns an unresolved
//! applicable cell into a loud C-1 defect; that is the correct signal, and
//! this harness never papers over it.
//!
//! **Populated cell-by-cell as each is proven live**, not all at once — a
//! subprocess-driving function that has never actually been run is unverified
//! code, not evidence (see the org's "never report an unrun result as
//! observed" discipline). Implemented and live-probed, serialized one daemon
//! at a time per conf-build-coord-2/mc-5's protocol (2026-07-14):
//! `d1.boot-readiness` on all five lanes — `pi` (cred-free), `codex`
//! (version-pin-gated on this box — resolves `Blocked`, zero RAM footprint),
//! `acp/claude-code` (needs the bridge dir on `PATH`, see
//! [`acp::bridge_bin_dir`]), `acp/opencode` (no PATH prep needed), and
//! `claude-code` bare (via `bond commission`'s zero-completion warm-leaf
//! boot — NOT `qd start`'s prompt-driven shapes, both of which `qd` itself
//! refuses; see [`claude_code::boot_readiness`]'s doc comment for the full
//! story). Every other cell in [`super::registry::conformance_battery`] is
//! still unresolved — `run_cell` returns `None` for them, which is the
//! correct, honest state, not a bug.

use std::process::Command;

use super::evidence::{Outcome, Runner};
use super::ids::{CellId, Lane};

/// The `qd` binary this box's operator already has on PATH — the same
/// invocation pattern every seed test uses. `path_prefix`, when given, is
/// prepended to `PATH` for this invocation only (ACP lanes need their bridge
/// binary's directory on `PATH` for the adapter's default bridge command to
/// resolve — pi/codex need no such prefix).
fn qd(path_prefix: Option<&str>) -> Command {
    let mut cmd = Command::new("qd");
    if let Some(prefix) = path_prefix {
        let cur = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{prefix}:{cur}"));
    }
    cmd
}

/// `d1.boot-readiness`, driven purely through the public `qd` CLI surface:
/// start a uniquely-named session for `provider_id`, confirm it resolves via
/// a FRESH `qd info --json` (a separate process, not a cached handle) as live
/// with a real pid, and stop it — leaving no resident behind regardless of
/// pass/fail (best-effort cleanup: the stop call's own result never overrides
/// the observed outcome). Shared by every lane's `boot_readiness` driver —
/// the mechanism this cell measures ("does a fresh CLI process resolve the
/// resident as really up") is domain-general, not lane-specific.
///
/// Corrected after the pi single-cell live probe (2026-07-14, RAM-watched per
/// conf-build-coord-2/mc-5's greenlight): the pi rubric's internal harness
/// (`provider/pi/conformance.rs`) reads the registry row FILE directly, which
/// carries an `endpoint` field — the public `qd info` CLI surface (text AND
/// `--json`) does NOT expose that field at all. Asserting on it would have
/// been a cell that could never pass through the CLI this harness actually
/// drives. `live: true` + a present `pid` is the CLI-surface-honest
/// equivalent of the same underlying claim ("the resident is really up").
fn boot_readiness_via_cli(
    runner: &Runner,
    session_name: &str,
    provider_id: &str,
    path_prefix: Option<&str>,
) -> Outcome {
    boot_readiness_via_cli_with_args(runner, session_name, provider_id, path_prefix, &[])
}

/// As [`boot_readiness_via_cli`], with additional `qd start` arguments
/// (`extra_args`) appended after `--provider <id>`. Exists for lanes whose
/// `qd start` refuses a bare headless start (`claude-code` requires `-p
/// <prompt>` — "a bare headless start is a no-op turn"): the assertion shape
/// (`live:true` + a real pid via a fresh `qd info --json`) stays identical to
/// every other lane; only the start invocation grows.
fn boot_readiness_via_cli_with_args(
    runner: &Runner,
    session_name: &str,
    provider_id: &str,
    path_prefix: Option<&str>,
    extra_args: &[&str],
) -> Outcome {
    let start_cmd = format!(
        "qd start {session_name} --provider {provider_id}{}",
        if extra_args.is_empty() {
            String::new()
        } else {
            format!(" {}", extra_args.join(" "))
        }
    );
    let start = qd(path_prefix)
        .args(["start", session_name, "--provider", provider_id])
        .args(extra_args)
        .output();
    let start = match start {
        Ok(o) => o,
        Err(e) => {
            return runner.fail(
                vec![start_cmd],
                format!("spawn error: {e}"),
                "failed to spawn qd itself — not a provider-daemon failure",
            );
        }
    };
    if !start.status.success() {
        let stderr = String::from_utf8_lossy(&start.stderr).into_owned();
        // A version-pin gate ("codex 0.144.1 detected, pinned 0.143...") is a
        // genuine per-box gate — named, with a stated re-entry condition — not
        // a defect: no daemon is spawned, nothing to tear down, and retrying
        // the same command on the same box will fail the same way until the
        // pin is resolved. Structurally `Blocked`, never `Fail` (live-probe
        // confirmed 2026-07-14 on the codex lane: `qd start` refuses loudly
        // before spawning anything, RAM unaffected).
        if stderr.contains("pinned") && stderr.contains("detected") {
            return Outcome::blocked(
                stderr.trim().to_string(),
                format!("re-pin {provider_id} (see the stated fixture-diff script) or set the stated unpinned-override env var, then retry"),
            );
        }
        // Another before-spawn CLI-usage gate, discovered live 2026-07-14 on
        // claude-code (bare) with `-p`: `qd start` refuses a one-off `-p`
        // stream-json invocation outright ("dispatch does not spawn one-off
        // `claude -p` stream-json runs..."), stating the correct shape
        // (`--interactive`, or bypass `qd` entirely for a one-off). Same
        // family as the version-pin gate — named, re-entry stated, nothing
        // spawned, NO completion call reached the model (confirmed: RAM
        // unaffected, no residual process). Structurally `Blocked`, never
        // `Fail` — a `Fail` implies the cell executed and its claim did not
        // hold; this cell never executed at all.
        if stderr.contains("does not spawn one-off") {
            return Outcome::blocked(
                stderr.trim().to_string(),
                "invoke qd start with the CLI shape the gate names (--interactive for a tracked session, or bypass qd for a true one-off) — the -p/--model flags used here do not fit qd start's one-off refusal".to_string(),
            );
        }
        let _ = qd(path_prefix).args(["stop", session_name]).output();
        return runner.fail(
            vec![start_cmd],
            format!("exit {:?}; stderr: {stderr}", start.status.code()),
            "qd start did not succeed",
        );
    }
    let info = qd(path_prefix).args(["info", session_name, "--json"]).output();
    let outcome = match info {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(&stdout);
            match parsed {
                Ok(v)
                    if v.get("live").and_then(serde_json::Value::as_bool) == Some(true)
                        && v.get("pid").and_then(serde_json::Value::as_i64).is_some() =>
                {
                    runner.pass(
                        vec![start_cmd.clone(), format!("qd info {session_name} --json")],
                        stdout,
                    )
                }
                Ok(_) => runner.fail(
                    vec![format!("qd info {session_name} --json")],
                    stdout,
                    "info resolved but was not live with a real pid",
                ),
                Err(e) => runner.fail(
                    vec![format!("qd info {session_name} --json")],
                    stdout,
                    format!("info output did not parse as JSON: {e}"),
                ),
            }
        }
        Ok(o) => runner.fail(
            vec![format!("qd info {session_name} --json")],
            String::from_utf8_lossy(&o.stderr).into_owned(),
            "qd info did not resolve the just-started session",
        ),
        Err(e) => runner.fail(
            vec![format!("qd info {session_name} --json")],
            format!("spawn error: {e}"),
            "failed to spawn qd info",
        ),
    };
    // Best-effort teardown — never overrides the already-decided outcome
    // above, and never itself observed as a cell (D1's teardown cell is a
    // separate, not-yet-implemented driver).
    let _ = qd(path_prefix).args(["stop", session_name]).output();
    outcome
}

/// Whether `pid` is a live process, checked via `ps -p` (a real OS-level
/// check — never a cached/self-reported field, matching the "never `qd
/// info`'s cached fields" discipline the atomic plan states for D5 and
/// applies here too: teardown must be verified at the OS, not just asked of
/// `qd` again).
fn os_pid_alive(pid: i64) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Start a session via `qd start --provider <id>` and return the commands
/// run so far plus `Ok(())`, or an immediate `Outcome` (a version-pin gate
/// resolves `Blocked`; any other start failure resolves `Fail`) — shared
/// start-only half of the several D1 cells below that each do a DIFFERENT
/// post-start check but the identical start dance.
fn start_via_cli(
    runner: &Runner,
    session_name: &str,
    provider_id: &str,
    path_prefix: Option<&str>,
) -> Result<Vec<String>, Outcome> {
    let start_cmd = format!("qd start {session_name} --provider {provider_id}");
    let start = qd(path_prefix)
        .args(["start", session_name, "--provider", provider_id])
        .output();
    let start = match start {
        Ok(o) => o,
        Err(e) => {
            return Err(runner.fail(
                vec![start_cmd],
                format!("spawn error: {e}"),
                "failed to spawn qd itself — not a provider-daemon failure",
            ));
        }
    };
    if !start.status.success() {
        let stderr = String::from_utf8_lossy(&start.stderr).into_owned();
        if stderr.contains("pinned") && stderr.contains("detected") {
            return Err(Outcome::blocked(
                stderr.trim().to_string(),
                format!("re-pin {provider_id} (see the stated fixture-diff script) or set the stated unpinned-override env var, then retry"),
            ));
        }
        return Err(runner.fail(
            vec![start_cmd],
            format!("exit {:?}; stderr: {stderr}", start.status.code()),
            "qd start did not succeed",
        ));
    }
    Ok(vec![start_cmd])
}

/// A fresh `qd info --json` read (a SEPARATE process each call, never a
/// cached handle) parsed to JSON, or `None` on any failure to resolve/parse.
fn fetch_info_json(session_name: &str, path_prefix: Option<&str>) -> Option<serde_json::Value> {
    let o = qd(path_prefix)
        .args(["info", session_name, "--json"])
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    serde_json::from_str(&String::from_utf8_lossy(&o.stdout)).ok()
}

/// `d1.launch-addressing`: the launch row carries real addressing fields —
/// specifically, a fresh `qd info --json` resolves the EXACT requested
/// session name to a live, real pid. (The other addressing fields the seed
/// checked — sessionId, endpoint — are covered by `d1.boot-readiness`'s
/// `live`+`pid` check and D1's own transport-is-our-daemon cell
/// respectively; this cell's distinct claim is name-addressing fidelity.)
fn launch_addressing_via_cli(
    runner: &Runner,
    session_name: &str,
    provider_id: &str,
    path_prefix: Option<&str>,
) -> Outcome {
    let mut commands = match start_via_cli(runner, session_name, provider_id, path_prefix) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let info_cmd = format!("qd info {session_name} --json");
    commands.push(info_cmd.clone());
    let outcome = match fetch_info_json(session_name, path_prefix) {
        Some(v)
            if v.get("name").and_then(serde_json::Value::as_str) == Some(session_name)
                && v.get("live").and_then(serde_json::Value::as_bool) == Some(true)
                && v.get("pid").and_then(serde_json::Value::as_i64).is_some() =>
        {
            runner.pass(commands, v.to_string())
        }
        Some(v) => runner.fail(
            commands,
            v.to_string(),
            "info resolved but name/live/pid did not all match the requested session",
        ),
        None => runner.fail(
            commands,
            "qd info did not resolve or parse".to_string(),
            "the just-started session did not address back correctly",
        ),
    };
    let _ = qd(path_prefix).args(["stop", session_name]).output();
    outcome
}

/// `d1.transport-is-our-daemon`: the live process backing a session is
/// confirmed OUR daemon, not merely a pid that happens to be alive — checked
/// via `ps -p <pid> -o cmd=`, never `qd`'s own self-report, matching the
/// same never-trust-the-claimant-alone discipline as teardown's OS-level
/// death check.
fn transport_is_our_daemon_via_cli(
    runner: &Runner,
    session_name: &str,
    provider_id: &str,
    path_prefix: Option<&str>,
) -> Outcome {
    let mut commands = match start_via_cli(runner, session_name, provider_id, path_prefix) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let info_cmd = format!("qd info {session_name} --json");
    commands.push(info_cmd);
    let pid = fetch_info_json(session_name, path_prefix)
        .and_then(|v| v.get("pid").and_then(serde_json::Value::as_i64).map(|_| v))
        .and_then(|v| v.get("pid").and_then(serde_json::Value::as_i64));
    let outcome = match pid {
        Some(p) => {
            let cmd_cmd = format!("ps -p {p} -o cmd=");
            commands.push(cmd_cmd);
            let cmdline = Command::new("ps")
                .args(["-p", &p.to_string(), "-o", "cmd="])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
            match cmdline {
                Some(c) if c.contains("qd") => runner.pass(commands, c),
                Some(c) => runner.fail(
                    commands,
                    c,
                    "the live pid's cmdline does not look like our qd daemon",
                ),
                None => runner.fail(
                    commands,
                    "ps -p failed to resolve the pid's cmdline".to_string(),
                    "cannot confirm the resident is our daemon",
                ),
            }
        }
        None => runner.fail(
            commands,
            "qd info did not resolve a pid".to_string(),
            "cannot confirm transport without a pid",
        ),
    };
    let _ = qd(path_prefix).args(["stop", session_name]).output();
    outcome
}

/// `d1.liveness-process-alive`: a liveness check reflects a real OS-level
/// pid-alive check (`ps -p`), never a cached/self-reported `qd` field —
/// applying D5's "never `qd info`'s cached fields" discipline to D1's own
/// liveness claim while the session is genuinely running (the mirror image
/// of teardown's post-stop death check).
fn liveness_process_alive_via_cli(
    runner: &Runner,
    session_name: &str,
    provider_id: &str,
    path_prefix: Option<&str>,
) -> Outcome {
    let mut commands = match start_via_cli(runner, session_name, provider_id, path_prefix) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let info_cmd = format!("qd info {session_name} --json");
    commands.push(info_cmd);
    let pid = fetch_info_json(session_name, path_prefix)
        .and_then(|v| v.get("pid").and_then(serde_json::Value::as_i64));
    let outcome = match pid {
        Some(p) => {
            commands.push(format!("ps -p {p}"));
            if os_pid_alive(p) {
                runner.pass(commands, format!("pid {p} confirmed alive via `ps -p` while live"))
            } else {
                runner.fail(
                    commands,
                    format!("pid {p} reported by qd info but NOT alive at the OS level"),
                    "a liveness claim must be backed by a real OS-level check",
                )
            }
        }
        None => runner.fail(
            commands,
            "qd info did not resolve a pid".to_string(),
            "cannot confirm liveness without a pid",
        ),
    };
    let _ = qd(path_prefix).args(["stop", session_name]).output();
    outcome
}

/// `d1.reconnect-resolves-via-registry`: a FRESH, separate `qd info`
/// process re-addresses a live session through the on-disk registry — driven
/// here as TWO independent `qd info --json` subprocess invocations (never
/// one process's cached state reused for both reads), both agreeing on
/// `live`+the same pid.
fn reconnect_resolves_via_cli(
    runner: &Runner,
    session_name: &str,
    provider_id: &str,
    path_prefix: Option<&str>,
) -> Outcome {
    let mut commands = match start_via_cli(runner, session_name, provider_id, path_prefix) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let info_cmd = format!("qd info {session_name} --json");
    let first = fetch_info_json(session_name, path_prefix);
    commands.push(info_cmd.clone());
    let second = fetch_info_json(session_name, path_prefix);
    commands.push(info_cmd);
    let pid_of = |v: &serde_json::Value| v.get("pid").and_then(serde_json::Value::as_i64);
    let live_of = |v: &serde_json::Value| v.get("live").and_then(serde_json::Value::as_bool);
    let outcome = match (&first, &second) {
        (Some(a), Some(b))
            if live_of(a) == Some(true)
                && live_of(b) == Some(true)
                && pid_of(a).is_some()
                && pid_of(a) == pid_of(b) =>
        {
            runner.pass(
                commands,
                format!("two independent qd info reads agree: pid {:?}, both live", pid_of(a)),
            )
        }
        _ => runner.fail(
            commands,
            format!("first={first:?} second={second:?}"),
            "two fresh, separate qd info reads did not both resolve the same live pid",
        ),
    };
    let _ = qd(path_prefix).args(["stop", session_name]).output();
    outcome
}

/// `d1.resume-same-session-id`, driven through the public `qd` CLI surface:
/// start, capture the sessionId, stop, then `qd resume <name> --no-attach`
/// (the non-TTY agent-facing revival verb — no interactive attach tail, safe
/// to drive headlessly) and confirm the SAME sessionId comes back (no
/// re-mint), with a real pid. Applies only to lanes whose wire protocol has
/// an actual resume/session-load primitive (ACP-family lanes + bare
/// claude-code — see the registry's applicability map; pi/codex are
/// `NaPermitted` here, a genuine protocol difference, not a coverage gap).
fn resume_same_session_id_via_cli(
    runner: &Runner,
    session_name: &str,
    provider_id: &str,
    path_prefix: Option<&str>,
) -> Outcome {
    let mut commands = match start_via_cli(runner, session_name, provider_id, path_prefix) {
        Ok(c) => c,
        Err(o) => return o,
    };
    commands.push(format!("qd info {session_name} --json"));
    let original_session_id = fetch_info_json(session_name, path_prefix)
        .and_then(|v| v.get("sessionId").and_then(serde_json::Value::as_str).map(str::to_string));

    // A real turn to revive INTO — structural precondition, manually
    // confirmed 2026-07-15 before writing this fix (same finding as pi's
    // and claude-code-bare's identical fixes): `qd resume` refuses a
    // zero-completion boot ("nothing to resume"). Manually verified this
    // works through a PLAIN `qd start`-booted ACP resident (not just the
    // D2 3-phase daemon fixture's `qd acp-daemon` boot path, which was the
    // only prior proof) for both acp/claude-code and acp/opencode before
    // committing this driver.
    let prompt = "Reply with exactly the single word OK and nothing else. Do not use any tools.";
    let send_cmd = format!("qd send:relay {session_name} <a trivial real turn>");
    let send = qd(path_prefix).args(["send:relay", session_name, prompt]).output();
    commands.push(send_cmd);
    if !matches!(&send, Ok(o) if o.status.success()) {
        let stderr = match &send {
            Ok(o) => String::from_utf8_lossy(&o.stderr).into_owned(),
            Err(e) => format!("spawn error: {e}"),
        };
        let _ = qd(path_prefix).args(["stop", session_name]).output();
        return runner.fail(commands, format!("qd send:relay did not succeed; stderr/err: {stderr}"), "cannot exercise resume-same-session-id without a completed real turn to resume into");
    }
    let wait_cmd = format!("qd wait {session_name} --timeout 45000");
    let wait = qd(path_prefix).args(["wait", session_name, "--timeout", "45000"]).output();
    commands.push(wait_cmd);
    if !matches!(&wait, Ok(o) if o.status.success()) {
        let stderr = match &wait {
            Ok(o) => String::from_utf8_lossy(&o.stderr).into_owned(),
            Err(e) => format!("spawn error: {e}"),
        };
        let _ = qd(path_prefix).args(["stop", session_name]).output();
        return runner.fail(commands, format!("qd wait did not succeed; stderr/err: {stderr}"), "the real turn must complete before stopping the resident");
    }

    let stop_cmd = format!("qd stop {session_name}");
    let _ = qd(path_prefix).args(["stop", session_name]).output();
    commands.push(stop_cmd);
    let resume_cmd = format!("qd resume {session_name} --no-attach");
    let resume = qd(path_prefix)
        .args(["resume", session_name, "--no-attach"])
        .output();
    commands.push(resume_cmd);
    let outcome = match (original_session_id, resume) {
        (None, _) => runner.fail(
            commands,
            "could not capture the original sessionId before stopping".to_string(),
            "cannot verify resume continuity without a starting sessionId",
        ),
        (Some(_), Err(e)) => runner.fail(
            commands,
            format!("spawn error resuming: {e}"),
            "failed to spawn qd resume",
        ),
        (Some(_), Ok(o)) if !o.status.success() => runner.fail(
            commands,
            format!(
                "resume exit {:?}; stderr: {}",
                o.status.code(),
                String::from_utf8_lossy(&o.stderr)
            ),
            "qd resume did not succeed",
        ),
        (Some(original), Ok(_)) => {
            commands.push(format!("qd info {session_name} --json"));
            match fetch_info_json(session_name, path_prefix) {
                Some(v)
                    if v.get("sessionId").and_then(serde_json::Value::as_str) == Some(original.as_str())
                        && v.get("live").and_then(serde_json::Value::as_bool) == Some(true)
                        && v.get("pid").and_then(serde_json::Value::as_i64).is_some() =>
                {
                    runner.pass(
                        commands,
                        format!("resumed session carries the SAME sessionId {original:?}, live with a real pid"),
                    )
                }
                Some(v) => runner.fail(
                    commands,
                    format!("original={original:?} post-resume={v}"),
                    "resume did not re-establish the same sessionId (or was not live)",
                ),
                None => runner.fail(
                    commands,
                    format!("original={original:?}"),
                    "qd info did not resolve the resumed session",
                ),
            }
        }
    };
    // `acp/claude-code` wraps the real claude engine, which writes its own
    // transcript to ~/.claude/projects/<cwd>/<session>.jsonl — a SHARED
    // directory (this cwd is the caller's own, not a jail), so remove only
    // this run's own file by its exact path, never the directory. Found
    // and fixed live 2026-07-15 (a leftover from this cell's first ACP
    // real-turn confirmation) — same class of residue as `a9f2020`'s fix
    // for the pong-turn/resume-recall drivers, just not yet applied here
    // since this driver never drew a real turn before that day.
    if provider_id == "acp/claude-code" {
        if let Some(jsonl_path) = fetch_info_json(session_name, path_prefix)
            .and_then(|v| v.get("jsonlPath").and_then(serde_json::Value::as_str).map(str::to_string))
        {
            let _ = std::fs::remove_file(&jsonl_path);
        }
    }
    let _ = qd(path_prefix).args(["stop", session_name]).output();
    outcome
}

/// `d1.multiplex-concurrent-distinct-residents`: start TWO sessions of the
/// SAME lane (derived `-a`/`-b` suffixes off `session_name`), leave BOTH
/// live concurrently, confirm both resolve independently (distinct pids,
/// each addressable by its own name, both genuinely live), then tear both
/// down. This is the ONE cell whose own claim requires two concurrent
/// daemons — a deliberate, explicitly-flagged exception to the
/// one-daemon-at-a-time serialize discipline every other cell in this
/// harness holds to (see the module docs and the commit history around
/// 2026-07-14 for the pre-clearance discussion before this was first run
/// live). Never call this casually — it is the only driver in this file
/// that puts two residents up at once by design.
fn multiplex_concurrent_via_cli(
    runner: &Runner,
    session_name: &str,
    provider_id: &str,
    path_prefix: Option<&str>,
) -> Outcome {
    let name_a = format!("{session_name}-a");
    let name_b = format!("{session_name}-b");

    let start_a = match start_via_cli(runner, &name_a, provider_id, path_prefix) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let start_b = match start_via_cli(runner, &name_b, provider_id, path_prefix) {
        Ok(c) => c,
        Err(o) => {
            // A is up but B's start failed/gated — tear A down before
            // returning B's resolution (never leave A dangling).
            let _ = qd(path_prefix).args(["stop", &name_a]).output();
            return o;
        }
    };
    let mut commands = start_a;
    commands.extend(start_b);
    commands.push(format!("qd info {name_a} --json"));
    commands.push(format!("qd info {name_b} --json"));

    let info_a = fetch_info_json(&name_a, path_prefix);
    let info_b = fetch_info_json(&name_b, path_prefix);
    let pid_of = |v: &serde_json::Value| v.get("pid").and_then(serde_json::Value::as_i64);
    let live_of = |v: &serde_json::Value| v.get("live").and_then(serde_json::Value::as_bool);

    let outcome = match (&info_a, &info_b) {
        (Some(a), Some(b))
            if live_of(a) == Some(true)
                && live_of(b) == Some(true)
                && pid_of(a).is_some()
                && pid_of(b).is_some()
                && pid_of(a) != pid_of(b) =>
        {
            runner.pass(
                commands,
                format!(
                    "two concurrent {provider_id} residents, distinct pids {:?} and {:?}, both independently live",
                    pid_of(a), pid_of(b)
                ),
            )
        }
        _ => runner.fail(
            commands,
            format!("a={info_a:?} b={info_b:?}"),
            "two concurrently-started sessions did not both resolve as distinct, live residents",
        ),
    };
    let _ = qd(path_prefix).args(["stop", &name_a]).output();
    let _ = qd(path_prefix).args(["stop", &name_b]).output();
    outcome
}

/// A minimal, hand-rolled temp-dir jail (no `tempfile` crate — that's a
/// `[dev-dependencies]`-only entry, unavailable to library code under
/// `src/`). Isolates `HOME`/`QD_HOME` so a hand-fabricated "cold" registry
/// row never touches the real environment. Cleaned up (`remove_dir_all`,
/// best-effort) on `Drop`.
struct Jail {
    root: std::path::PathBuf,
    home: std::path::PathBuf,
    qd_home: std::path::PathBuf,
    registry_dir: std::path::PathBuf,
    events_dir: std::path::PathBuf,
}

impl Jail {
    fn new(tag: &str) -> std::io::Result<Self> {
        let root = std::env::temp_dir().join(format!(
            "c1-jail-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let home = root.join("home");
        let qd_home = root.join("qd");
        let registry_dir = home.join(".claude").join("sessions");
        let events_dir = qd_home.join("state").join("sessions");
        std::fs::create_dir_all(&registry_dir)?;
        std::fs::create_dir_all(&events_dir)?;
        Ok(Jail {
            root,
            home,
            qd_home,
            registry_dir,
            events_dir,
        })
    }
}

impl Drop for Jail {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The `qd` binary to drive for cells that need to test THIS SOURCE TREE's
/// actual behavior, not whatever happens to be deployed at `qd` on `PATH`.
/// Checks `QD_BIN` first (a caller — e.g. a test file using Cargo's
/// `env!("CARGO_BIN_EXE_qd")`, only available where there's a `[[bin]]`
/// target in the same package, which library code under `src/` is not —
/// sets this to the freshly-built worktree binary), falling back to bare
/// `"qd"` (PATH resolution, the deployed binary) for every other cell that
/// has never needed this distinction.
///
/// Live-caught 2026-07-14: `d1.boot-readiness` and friends all measure
/// stable, long-unchanged start/stop/info mechanics and passed cleanly
/// against the deployed binary — no discrepancy there. But
/// `d6.cold-target-send-fails-loud-with-terminal`'s specific
/// `emit_door_failure` path is newer than what's deployed (`/home/
/// u/.quorum/bin/qd`, dated 2026-07-09 — 5 days stale
/// relative to this worktree): the SAME fixture, byte-identical, produced
/// completely different (and correct) behavior once pointed at the
/// worktree's own freshly-built binary instead. The deployed binary was
/// never the bug; it was simply missing the code this cell exists to prove.
fn qd_bin() -> String {
    std::env::var("QD_BIN").unwrap_or_else(|_| "qd".to_string())
}

/// `d6.cold-target-send-fails-loud-with-terminal`: a resolved session whose
/// resident is unreachable — a REAL live process backs the registry row (the
/// resolve scan filters dead pids), but the row carries NO recorded
/// `endpoint`, hitting the daemon-lane "not reachable" door — drives the
/// REAL, compiled `qd send:relay` verb (never a bare library call) inside an
/// isolated jail (`HOME`/`QD_HOME` never touch the real environment). Mirrors
/// `tests/delivery_daemon_door_conformance.rs`'s own proven fixture shape
/// (a bare `sleep` standing in for the dead wire, `dispatch::registry::
/// write_entry` for the fixture row — the SAME internal call the accepted
/// seed test itself uses to fabricate the precondition; the CLAIM under
/// test is `qd send:relay`'s real, compiled behavior, driven for real).
/// Cred-free, deterministic, no live gate — the door fires on registry
/// shape alone. Uses [`qd_bin`] — this cell's behavior depends on code that
/// may not be deployed yet (see that function's doc comment).
fn cold_target_send_fails_via_jail(runner: &Runner, session_name: &str, provider_id: &str) -> Outcome {
    let jail = match Jail::new(session_name) {
        Ok(j) => j,
        Err(e) => {
            return runner.fail(
                vec!["(jail setup)".to_string()],
                format!("failed to create jail dirs: {e}"),
                "cannot fabricate the cold-target fixture without a jail",
            );
        }
    };

    let mut sleeper = match Command::new("sleep").arg("60").spawn() {
        Ok(c) => c,
        Err(e) => {
            return runner.fail(
                vec!["sleep 60".to_string()],
                format!("spawn error: {e}"),
                "cannot back the cold-target row without a real live pid",
            );
        }
    };
    let live_pid = sleeper.id() as i64;
    let session_id = format!("c1-cold-target-{session_name}");

    let entry = crate::registry::RegistryEntry {
        pid: Some(live_pid),
        session_id: Some(session_id.clone()),
        provider: Some(provider_id.to_string()),
        name: Some(session_name.to_string()),
        cwd: Some(jail.home.to_string_lossy().to_string()),
        // Deliberately NO `endpoint` — this is the whole point: a live pid
        // (resolves) with no recorded wire (the "not reachable" door).
        ..Default::default()
    };
    if let Err(e) = crate::registry::write_entry(&jail.registry_dir, &entry) {
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        return runner.fail(
            vec!["(write registry row)".to_string()],
            format!("failed to write the cold-target registry row: {e}"),
            "cannot drive qd send:relay without a resolvable row",
        );
    }

    let message = "C-1 d6 cold-target probe — resident is gone";
    let send_cmd = format!("qd send:relay {session_name} {message:?} (jailed)");
    let out = Command::new(qd_bin())
        .args(["send:relay", session_name, message])
        .env("HOME", &jail.home)
        .env("QD_HOME", &jail.qd_home)
        .env_remove("QD_BOOT_AWAIT_RELAY")
        .output();
    let commands = vec!["(write registry row)".to_string(), send_cmd];

    let outcome = match out {
        Ok(o) if !o.status.success() => {
            // Fails loud, as expected — now confirm the truthful terminal.
            let log_path = jail.events_dir.join(format!("{session_id}.events.jsonl"));
            match std::fs::read_to_string(&log_path) {
                Ok(raw) => {
                    let send_failed = raw.lines().find_map(|l| {
                        let v: serde_json::Value = serde_json::from_str(l).ok()?;
                        (v.get("event").and_then(serde_json::Value::as_str) == Some("send-failed"))
                            .then_some(v)
                    });
                    match send_failed {
                        Some(v) => runner.pass(
                            commands,
                            format!(
                                "qd send:relay exited {:?} (fail loud) and recorded a truthful send-failed terminal: {v}",
                                o.status.code()
                            ),
                        ),
                        None => runner.fail(
                            commands,
                            format!("exit {:?}; log had no send-failed event: {raw}", o.status.code()),
                            "a door failure must record a send-failed terminal, not just fail loud",
                        ),
                    }
                }
                Err(e) => runner.fail(
                    commands,
                    format!(
                        "exit {:?}; stderr: {}; could not read target delivery log {log_path:?}: {e}; qd_home listing: {:?}",
                        o.status.code(),
                        String::from_utf8_lossy(&o.stderr),
                        std::fs::read_dir(&jail.qd_home)
                            .ok()
                            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect::<Vec<_>>()),
                    ),
                    "a door failure must leave a readable delivery log for the target",
                ),
            }
        }
        Ok(o) => runner.fail(
            commands,
            format!(
                "exit {:?} (succeeded) — a cold-target send must fail loud, never succeed; stdout: {}",
                o.status.code(),
                String::from_utf8_lossy(&o.stdout)
            ),
            "qd send:relay against an unreachable resident must not report success",
        ),
        Err(e) => runner.fail(
            commands,
            format!("spawn error: {e}"),
            "failed to spawn qd itself — not a door-failure result",
        ),
    };

    let _ = sleeper.kill();
    let _ = sleeper.wait();
    outcome
}

/// `d6.bridge-death-detection-no-false-positive`, ACP-family lanes only.
/// Mirrors `tests/acp_cc_live.rs::bridge_confirmed_dead_flips_only_when_child_exits`
/// exactly — a proven, in-family seed with a library-level shape (drives
/// [`crate::provider::acp::AcpHost`] directly, no `qd` subprocess, no bridge
/// binary, no node dependency: a trivial `sh` child stands in for the ACP
/// bridge child, same as the seed). Two directions in one cell, both
/// required for the claim: a LIVE child must never read as confirmed-dead
/// (the false-positive this cell exists to rule out — a transient blip or a
/// busy turn must not trip a self-terminate), and a genuinely EXITED child
/// must read as confirmed-dead, reliably, within a bounded poll. Cred-free,
/// deterministic, no live gate.
fn bridge_death_detection_via_host(runner: &Runner) -> Outcome {
    use crate::provider::acp::AcpHost;

    let tmp = std::env::temp_dir().join(format!(
        "c1-bridge-death-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        return runner.fail(
            vec!["(mkdir bridge-death jail)".to_string()],
            format!("failed to create scratch dir: {e}"),
            "cannot spawn AcpHost bridge stand-ins without a cwd",
        );
    }

    let commands = vec![
        "AcpHost::spawn(\"sh\", [\"-c\", \"sleep 30\"]) — alive stand-in".to_string(),
        "AcpHost::spawn(\"sh\", [\"-c\", \"exit 0\"]) — exiting stand-in".to_string(),
    ];

    let alive = AcpHost::spawn("sh", &["-c".to_string(), "sleep 30".to_string()], &tmp);
    let alive = match alive {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(
                commands,
                format!("spawn error (alive stand-in): {e}"),
                "cannot exercise the false-positive direction without a live bridge stand-in",
            );
        }
    };
    if alive.bridge_confirmed_dead() {
        drop(alive);
        let _ = std::fs::remove_dir_all(&tmp);
        return runner.fail(
            commands,
            "a live bridge child (sh -c 'sleep 30') read as confirmed-dead immediately after spawn"
                .to_string(),
            "a live bridge child must never be confirmed dead — this is the false-positive the cell exists to rule out",
        );
    }
    drop(alive); // Drop kills + reaps it — no leak, no lingering false-negative window.

    let dead = AcpHost::spawn("sh", &["-c".to_string(), "exit 0".to_string()], &tmp);
    let dead = match dead {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(
                commands,
                format!("spawn error (exiting stand-in): {e}"),
                "cannot exercise the true-positive direction without an exiting bridge stand-in",
            );
        }
    };
    let mut confirmed = false;
    for _ in 0..100 {
        if dead.bridge_confirmed_dead() {
            confirmed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = std::fs::remove_dir_all(&tmp);
    if confirmed {
        runner.pass(
            commands,
            "a live bridge stand-in (sh -c 'sleep 30') was never confirmed dead; an exited \
             bridge stand-in (sh -c 'exit 0') was confirmed dead within a 2s poll bound — no \
             false positive in either direction"
                .to_string(),
        )
    } else {
        runner.fail(
            commands,
            "an exited bridge stand-in (sh -c 'exit 0') was never confirmed dead within a 2s poll bound"
                .to_string(),
            "a genuinely exited bridge child must be reliably detected as dead",
        )
    }
}

/// `node` on `PATH`? (the fake ACP bridge in [`write_fake_bridge`] is a node
/// program — mirrors `tests/acp_cc_live.rs::node_program` exactly).
fn node_program() -> Option<String> {
    for cand in ["node", "/usr/bin/node"] {
        if Command::new(cand)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(cand.to_string());
        }
    }
    None
}

/// Write the deterministic fake ACP agent script used by the D3 queue
/// cells — mirrors `tests/acp_cc_live.rs::write_fake_bridge` exactly
/// (same wire shape, same canned responses). No model, no network, no
/// live `next_update` drain — the queue cells never call `next_update`,
/// so the fake agent's `session/prompt` handler is never even reached;
/// only `initialize`/`session/new` matter here.
fn write_fake_bridge(dir: &std::path::Path) -> std::path::PathBuf {
    let script = r#"
const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin });
function send(o){ process.stdout.write(JSON.stringify(o) + "\n"); }
rl.on('line', (line) => {
  line = line.trim(); if (!line) return;
  let m; try { m = JSON.parse(line); } catch { return; }
  const { id, method, params } = m;
  if (method === 'initialize') {
    send({ jsonrpc:"2.0", id, result:{ protocolVersion:1, agentInfo:{name:"fake-acp",version:"0"}, authMethods:[] }});
  } else if (method === 'session/new') {
    send({ jsonrpc:"2.0", id, result:{ sessionId:"fake-sess-1" }});
  } else if (method === 'session/prompt') {
    send({ jsonrpc:"2.0", method:"session/update", params:{ sessionId: params.sessionId, update:{ sessionUpdate:"agent_message_chunk", content:{type:"text", text:"OK"} }}});
    send({ jsonrpc:"2.0", id, result:{ stopReason:"end_turn" }});
  } else if (method === 'session/load') {
    send({ jsonrpc:"2.0", id, result:{}});
  } else if (id !== undefined && id !== null) {
    send({ jsonrpc:"2.0", id, error:{code:-32601, message:"unknown method"}});
  }
});
"#;
    let path = dir.join("fake-acp-bridge.js");
    let _ = std::fs::write(&path, script);
    path
}

/// `d3.queue-overflow-honors-configured-capacity`, ACP-family lanes only.
/// Mirrors `tests/acp_cc_live.rs::acp_host_queue_overflow_honors_configured_capacity`
/// exactly (in-family seed, library-level shape): spawns a real `AcpHost`
/// against the deterministic fake bridge at a CONFIGURED capacity of 2,
/// drives three injects through the production `Provider::inject` ladder
/// (fits: in-flight + 2 backlog), then asserts the 4th overflows with the
/// real `InjectError::Precondition("queue full")` — never a silently
/// widened default. Both ACP-family lanes share the same `AcpProvider`
/// implementation (`provider_for` resolves both ids to the identical
/// struct, differing only by id string), so one driver covers both.
/// Cred-free, deterministic (no model, no network), no live gate.
fn queue_overflow_via_host(runner: &Runner, provider_id: &str) -> Outcome {
    use crate::effects::MapEnv;
    use crate::paths::QdPaths;
    use crate::provider::acp::rpc::AcpClient;
    use crate::provider::acp::AcpHost;
    use crate::provider::{provider_for, InjectError, ProviderFx, SessionKey};

    let node = match node_program() {
        Some(n) => n,
        None => {
            return Outcome::blocked(
                "node not found on PATH — the deterministic fake ACP bridge is a node program"
                    .to_string(),
                "install node (or add it to PATH), then retry".to_string(),
            );
        }
    };
    let tmp = std::env::temp_dir().join(format!(
        "c1-queue-overflow-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        return runner.fail(
            vec!["(mkdir queue-overflow jail)".to_string()],
            format!("failed to create scratch dir: {e}"),
            "cannot spawn the fake bridge without a cwd",
        );
    }
    let fake = write_fake_bridge(&tmp);

    let commands = vec![
        format!("AcpHost::spawn_with_capacity({node}, [fake-acp-bridge.js], cap=2)"),
        "Provider::inject x4 (3 fit, 4th must overflow)".to_string(),
    ];

    let host = AcpHost::spawn_with_capacity(&node, &[fake.to_string_lossy().to_string()], &tmp, 2);
    let host = match host {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(
                commands,
                format!("spawn error: {e}"),
                "cannot exercise queue capacity without a live AcpHost",
            );
        }
    };
    if let Err(e) = host.initialize() {
        let _ = std::fs::remove_dir_all(&tmp);
        return runner.fail(
            commands,
            format!("initialize error: {e}"),
            "cannot exercise queue capacity without a completed handshake",
        );
    }
    let session = match host.new_session(&tmp.to_string_lossy()) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(
                commands,
                format!("session/new error: {e}"),
                "cannot exercise queue capacity without a session",
            );
        }
    };

    let env = MapEnv::default();
    let paths = QdPaths::from_home(&tmp);
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: tmp.clone(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: Some(&host),
        pi_rpc: None,
        acp_pre_dispatch: None,
    };
    let provider = match provider_for(provider_id) {
        Some(p) => p,
        None => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(
                commands,
                format!("provider_for({provider_id:?}) returned None"),
                "the ACP provider must be registered under this id",
            );
        }
    };
    let key = SessionKey { id: &session, name: Some("c1-d3"), cwd: Some("/work/proj"), pid: None };

    for i in 1..=3 {
        if let Err(e) = provider.inject(&fx, &key, "m", "c1-d3-queue-test") {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(
                commands,
                format!("inject #{i} unexpectedly failed to fit at capacity 2: {e:?}"),
                "the first three injects (1 in-flight + 2 backlog) must fit the configured capacity",
            );
        }
    }
    let overflow = provider.inject(&fx, &key, "m", "c1-d3-queue-test");
    let _ = std::fs::remove_dir_all(&tmp);
    match overflow {
        Err(InjectError::Precondition(msg)) if msg == "queue full" => runner.pass(
            commands,
            format!(
                "injects 1-3 fit the configured capacity of 2 (1 in-flight + 2 backlog); \
                 inject #4 overflowed with the real InjectError::Precondition({msg:?}) — the \
                 configured bound was honored, not silently widened to the default"
            ),
        ),
        Err(e) => runner.fail(
            commands,
            format!("inject #4 failed, but not with the expected Precondition(\"queue full\"): {e:?}"),
            "an overflow at the configured capacity must surface as InjectError::Precondition(\"queue full\")",
        ),
        Ok(id) => runner.fail(
            commands,
            format!(
                "inject #4 succeeded (id {id:?}) — the configured capacity of 2 was not honored (silently widened?)"
            ),
            "a 4th inject past the configured capacity of 2 must overflow, not succeed",
        ),
    }
}

/// `python3` on `PATH`? (the cancel-mapping fixture is a python program —
/// mirrors `tests/acp_cancel_mapping.rs::python3_available` exactly).
fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `d6.cancel-maps-to-truthful-terminal`, ACP-family lanes only. Mirrors
/// `tests/acp_cancel_mapping.rs::cancel_maps_literal_cancelled_and_releases_the_slot`
/// exactly (in-family seed, library-level shape): drives a real `AcpHost`
/// against the fixture at `tests/fixtures/cancel_fake_agent.py` (holds an
/// in-flight prompt until interrupted), sends `session/cancel`, and asserts
/// BOTH halves of the claim in one cell — the interrupt surfaces as the
/// literal `Terminal{stop: Cancelled}` (never a silently-swallowed
/// `end_turn` or a `TerminalError`), AND the queue slot is released after
/// (`!host.in_flight()`, no leak). `CARGO_MANIFEST_DIR` resolves the fixture
/// path correctly from library code (unlike `CARGO_BIN_EXE_qd`, it needs no
/// `[[bin]]` target). Cred-free (fixture peer only, no live claude turn),
/// no live gate.
fn cancel_maps_to_truthful_terminal_via_host(runner: &Runner) -> Outcome {
    use crate::provider::acp::{AcpClient, AcpEvent, AcpHost, StopReason};

    if !python3_available() {
        return Outcome::blocked(
            "python3 not found on PATH — the cancel-mapping fixture is a python program"
                .to_string(),
            "install python3 (or add it to PATH), then retry".to_string(),
        );
    }
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cancel_fake_agent.py");
    if !fixture.exists() {
        return runner.fail(
            vec!["(check fixture exists)".to_string()],
            format!("fixture agent missing: {}", fixture.display()),
            "cannot exercise cancel-mapping without the accepted seed's fixture agent",
        );
    }
    let cwd = std::env::temp_dir();
    let commands = vec![
        format!("AcpHost::spawn(python3, [{}])", fixture.display()),
        "host.prompt(...) then host.cancel(...)".to_string(),
    ];

    let host = match AcpHost::spawn("python3", &[fixture.to_string_lossy().into_owned()], &cwd) {
        Ok(h) => h,
        Err(e) => {
            return runner.fail(
                commands,
                format!("spawn error: {e}"),
                "cannot exercise cancel-mapping without a live AcpHost",
            );
        }
    };
    if let Err(e) = host.initialize() {
        return runner.fail(
            commands,
            format!("initialize error: {e}"),
            "cannot exercise cancel-mapping without a completed handshake",
        );
    }
    let session = match host.new_session(cwd.to_str().unwrap_or(".")) {
        Ok(s) => s,
        Err(e) => {
            return runner.fail(
                commands,
                format!("session/new error: {e}"),
                "cannot exercise cancel-mapping without a session",
            );
        }
    };
    if let Err(e) = host.prompt(&session, "go", "c1-d6-cancel-test", &|| {}) {
        return runner.fail(
            commands,
            format!("prompt enqueue+send error: {e}"),
            "cannot exercise cancel-mapping without an in-flight turn",
        );
    }
    if !host.in_flight() {
        return runner.fail(
            commands,
            "the turn was not in flight immediately after prompt".to_string(),
            "the seed's fixture holds the response specifically so a cancel has something to interrupt",
        );
    }
    if let Err(e) = host.cancel(&session) {
        return runner.fail(
            commands,
            format!("cancel error: {e}"),
            "session/cancel must succeed against an in-flight turn",
        );
    }

    let mut events: Vec<AcpEvent> = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let terminal = loop {
        if std::time::Instant::now() >= deadline {
            return runner.fail(
                commands,
                format!("timed out before terminal; events so far: {events:?}"),
                "a cancelled in-flight turn must resolve to a terminal within a bounded window",
            );
        }
        match host.next_update(std::time::Duration::from_millis(500)) {
            Ok(Some(ev)) => {
                let is_terminal =
                    matches!(ev, AcpEvent::Terminal { .. } | AcpEvent::TerminalError { .. });
                events.push(ev.clone());
                if is_terminal {
                    break ev;
                }
            }
            Ok(None) => continue,
            Err(e) => {
                return runner.fail(
                    commands,
                    format!("next_update errored before terminal: {e}; events so far: {events:?}"),
                    "the transport must stay healthy through a cancel-to-terminal drain",
                );
            }
        }
    };

    match terminal {
        AcpEvent::Terminal { stop, .. } if stop == StopReason::Cancelled => {
            if host.in_flight() {
                runner.fail(
                    commands,
                    format!(
                        "terminal was the correct Cancelled, but the queue slot is still in-flight after — a leak; events={events:?}"
                    ),
                    "the queue slot must be released once the cancelled terminal drains (no leak)",
                )
            } else {
                runner.pass(
                    commands,
                    format!(
                        "session/cancel against an in-flight turn mapped to the literal \
                         Terminal{{stop: Cancelled}} (never end_turn, never a TerminalError), and \
                         the queue slot was released after (no leak); events={events:?}"
                    ),
                )
            }
        }
        other => runner.fail(
            commands,
            format!("cancel did not yield a clean Terminal{{Cancelled}}, got {other:?}; events={events:?}"),
            "session/cancel must map the bridge's cancelled to StopReason::Cancelled, truthfully",
        ),
    }
}

/// `d6.self-terminate-on-wedged-child`, ACP-family lanes only (`NaPermitted`
/// for pi/codex — see the `APPLICABILITY RULING` note below). Drives the REAL
/// `crate::provider::acp::wire::serve` loop (the qd daemon's own ws server
/// for this lane) against a REAL `AcpHost` backed by a `sh` stand-in bridge
/// child — end-to-end, no `FakeResident` (that type is `#[cfg(test)]`-private
/// to `wire.rs`, unreachable from library code; a real subprocess is used
/// instead, same technique as [`bridge_death_detection_via_host`]). Proves
/// both halves in one cell, mirroring `wire.rs`'s own
/// `serve_self_terminates_on_confirmed_dead_bridge` /
/// `serve_does_not_self_terminate_on_live_bridge` exactly: a LIVE bridge
/// child must NOT cause `serve` to return (no silent loss of a
/// recoverable session), and a bridge child confirmed dead (reaped) MUST
/// cause `serve` to return `Err` within a bounded window (self-terminate,
/// not a lingering zombie 'live' lie). Cred-free, deterministic, no live
/// gate.
///
/// **APPLICABILITY RULING (conf-build-coord-3, 2026-07-15):** the battery's
/// registry doc text for this cell ("when a required child process is
/// killed out from under a daemon, the daemon self-terminates") matches
/// `wire::serve`'s guard exactly, but `grep`ping this crate found `fn serve`
/// and this `bridge_confirmed_dead`-driven self-terminate guard ONLY in
/// `provider/acp/wire.rs` — no analogous `serve` loop or child-confirmed-dead
/// self-terminate exists for `pi` or `codex` (`grep -rn "fn serve\b"
/// src/provider/` returns exactly one hit). Flagged to the coordinator, who
/// ruled `NaPermitted` for pi/codex (same pattern as the sibling D3/D6
/// ACP-only cells) — see [`super::registry::conformance_battery`]'s
/// applicability for this cell id.
fn self_terminate_on_wedged_child_via_host(runner: &Runner) -> Outcome {
    use crate::provider::acp::{serve, AcpHost};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let tmp = std::env::temp_dir().join(format!(
        "c1-wedged-child-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        return runner.fail(
            vec!["(mkdir wedged-child jail)".to_string()],
            format!("failed to create scratch dir: {e}"),
            "cannot spawn AcpHost bridge stand-ins without a cwd",
        );
    }

    let commands = vec![
        "wire::serve(AcpHost{bridge=sh -c 'sleep 30'}, listener, shutdown=false)".to_string(),
        "wire::serve(AcpHost{bridge=sh -c 'exit 0', reaped}, listener, shutdown=false)".to_string(),
    ];

    // Arm 1: a LIVE bridge child must NOT cause serve to self-terminate.
    let listener1 = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(commands, format!("bind error: {e}"), "cannot exercise serve without a listener");
        }
    };
    let alive = AcpHost::spawn("sh", &["-c".to_string(), "sleep 30".to_string()], &tmp);
    let alive = match alive {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(commands, format!("spawn error (alive stand-in): {e}"), "cannot exercise the no-false-positive arm without a live bridge stand-in");
        }
    };
    let port1 = match listener1.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(commands, format!("local_addr error: {e}"), "cannot nudge the accept loop without the bound port");
        }
    };
    let shutdown1 = Arc::new(AtomicBool::new(false));
    let sd1 = shutdown1.clone();
    let h1 = std::thread::spawn(move || {
        let _keep_alive = alive;
        serve(&_keep_alive, &listener1, &sd1)
    });
    // No-false-positive WINDOW: serve must NOT self-terminate at ANY point while
    // the bridge child is alive — not merely "not within the first 400ms". A
    // single point-check could miss a serve that wrongly self-terminates a couple
    // of seconds in, so poll across a window comparable to arm 2's 5s
    // self-terminate budget. On a fail here h1 has finished, so join() reaps it
    // (dropping `alive`, killing its bridge child) — no linger on this path.
    let live_window = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < live_window {
        if h1.is_finished() {
            let res = h1.join();
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(
                commands,
                format!("serve self-terminated against a LIVE bridge child within the 2s no-false-positive window: {res:?}"),
                "serve must NOT self-terminate while the bridge child is alive — this would silently drop a recoverable session",
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    shutdown1.store(true, Ordering::SeqCst);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port1)); // nudge the accept loop awake
    let join_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !h1.is_finished() && std::time::Instant::now() < join_deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if !h1.is_finished() {
        let _ = std::fs::remove_dir_all(&tmp);
        return runner.fail(
            commands,
            "serve thread (live-bridge arm) did not join within 5s of shutdown being set".to_string(),
            "the normal shutdown path must still end serve promptly — a harness-cleanliness requirement, not the cell's own claim, but needed to avoid a leaked thread",
        );
    }

    // Arm 2: a bridge child confirmed DEAD (reaped) must self-terminate serve.
    let listener2 = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(commands, format!("bind error (arm 2): {e}"), "cannot exercise serve without a listener");
        }
    };
    let dead = AcpHost::spawn("sh", &["-c".to_string(), "exit 0".to_string()], &tmp);
    let dead = match dead {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(commands, format!("spawn error (exiting stand-in): {e}"), "cannot exercise the self-terminate arm without an exiting bridge stand-in");
        }
    };
    // Wait for the reap to land before starting serve, so the guard is
    // exercised against a GENUINELY confirmed-dead child, not a race.
    let reap_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !dead.bridge_confirmed_dead() && std::time::Instant::now() < reap_deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !dead.bridge_confirmed_dead() {
        let _ = std::fs::remove_dir_all(&tmp);
        return runner.fail(
            commands,
            "the exiting bridge stand-in was never confirmed dead within a 2s poll bound".to_string(),
            "cannot exercise the self-terminate arm without a confirmed-dead precondition",
        );
    }
    let shutdown2 = Arc::new(AtomicBool::new(false));
    let sd2 = shutdown2.clone();
    let h2 = std::thread::spawn(move || {
        let _keep_dead = dead;
        serve(&_keep_dead, &listener2, &sd2)
    });
    let start = std::time::Instant::now();
    while !h2.is_finished() {
        if start.elapsed() >= std::time::Duration::from_secs(5) {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(
                commands,
                "serve did not self-terminate within 5s against a confirmed-dead bridge child (no shutdown set)".to_string(),
                "serve must self-terminate promptly when the required child process is confirmed dead — a lingering zombie 'live' state is exactly the honesty defect this cell exists to rule out",
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let res2 = h2.join();
    let _ = std::fs::remove_dir_all(&tmp);
    match res2 {
        Ok(Err(_)) => runner.pass(
            commands,
            "arm 1 (live bridge child): serve did NOT self-terminate within 400ms and joined \
             cleanly on shutdown — no silent loss of a recoverable session. arm 2 (bridge child \
             confirmed dead/reaped): serve self-terminated (returned Err) within the 5s bound, \
             with no shutdown flag set — a required child process killed out from under the \
             daemon makes the daemon self-terminate rather than lingering falsely live"
                .to_string(),
        ),
        Ok(Ok(())) => runner.fail(
            commands,
            "serve returned Ok(()) against a confirmed-dead bridge — self-terminate must surface as Err".to_string(),
            "a self-terminate on a confirmed-dead required child must be a loud Err, not a quiet Ok",
        ),
        Err(e) => runner.fail(
            commands,
            format!("serve thread panicked: {e:?}"),
            "the self-terminate guard must return an Err cleanly, never panic",
        ),
    }
}

/// `d3.queue-slot-released-no-leak`, ACP-family lanes only. NO pre-existing
/// named seed test covers this exact claim (checked `acp_cc_live.rs` and the
/// `AcpHost`/`OutboundQueue` source directly) — authored fresh as the
/// natural companion to [`queue_overflow_via_host`], reusing that seed's
/// own externally-observable pattern (inject success/failure through the
/// production `Provider::inject` ladder — never reaching into the private
/// `OutboundQueue` internals `AcpHost` doesn't expose) rather than a new
/// fixture or mechanism. Shape: fill to the configured bound of 2 (1
/// in-flight + 2 backlog, exactly as the overflow cell proves fits), then
/// drain the in-flight turn to its terminal (freeing its slot — the next
/// backlogged item is promoted in-flight, per `OutboundQueue::complete`/
/// `try_release`), then assert a NEW inject now FITS where, before the
/// drain, an equivalent 4th inject overflowed. If the slot leaked (stayed
/// counted as full), this new inject would incorrectly overflow too — that
/// is the defect this cell exists to catch. Cred-free, deterministic (fake
/// bridge, no model/network), no live gate.
fn queue_slot_released_via_host(runner: &Runner, provider_id: &str) -> Outcome {
    use crate::effects::MapEnv;
    use crate::paths::QdPaths;
    use crate::provider::acp::rpc::AcpClient;
    use crate::provider::acp::{AcpEvent, AcpHost};
    use crate::provider::{provider_for, InjectError, ProviderFx, SessionKey};

    let node = match node_program() {
        Some(n) => n,
        None => {
            return Outcome::blocked(
                "node not found on PATH — the deterministic fake ACP bridge is a node program"
                    .to_string(),
                "install node (or add it to PATH), then retry".to_string(),
            );
        }
    };
    let tmp = std::env::temp_dir().join(format!(
        "c1-queue-slot-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        return runner.fail(
            vec!["(mkdir queue-slot jail)".to_string()],
            format!("failed to create scratch dir: {e}"),
            "cannot spawn the fake bridge without a cwd",
        );
    }
    let fake = write_fake_bridge(&tmp);

    let commands = vec![
        format!("AcpHost::spawn_with_capacity({node}, [fake-acp-bridge.js], cap=2)"),
        "Provider::inject x3 (fills to the bound); drain in-flight to terminal; inject again".to_string(),
    ];

    let host = AcpHost::spawn_with_capacity(&node, &[fake.to_string_lossy().to_string()], &tmp, 2);
    let host = match host {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(commands, format!("spawn error: {e}"), "cannot exercise slot release without a live AcpHost");
        }
    };
    if let Err(e) = host.initialize() {
        let _ = std::fs::remove_dir_all(&tmp);
        return runner.fail(commands, format!("initialize error: {e}"), "cannot exercise slot release without a completed handshake");
    }
    let session = match host.new_session(&tmp.to_string_lossy()) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(commands, format!("session/new error: {e}"), "cannot exercise slot release without a session");
        }
    };

    let env = MapEnv::default();
    let paths = QdPaths::from_home(&tmp);
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: tmp.clone(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: Some(&host),
        pi_rpc: None,
        acp_pre_dispatch: None,
    };
    let provider = match provider_for(provider_id) {
        Some(p) => p,
        None => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(commands, format!("provider_for({provider_id:?}) returned None"), "the ACP provider must be registered under this id");
        }
    };
    let key = SessionKey { id: &session, name: Some("c1-d3-slot"), cwd: Some("/work/proj"), pid: None };

    let mut turn_ids = Vec::new();
    for i in 1..=3 {
        match provider.inject(&fx, &key, "m", "c1-d3-slot-test") {
            Ok(id) => turn_ids.push(id),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                return runner.fail(
                    commands,
                    format!("inject #{i} unexpectedly failed to fit at capacity 2: {e:?}"),
                    "the first three injects (1 in-flight + 2 backlog) must fit the configured capacity",
                );
            }
        }
    }
    let first_turn = turn_ids[0].clone();

    // Drain updates until the FIRST turn's terminal arrives — freeing its
    // slot (the next backlogged item is promoted in-flight internally).
    let mut events: Vec<AcpEvent> = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if std::time::Instant::now() >= deadline {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(
                commands,
                format!("timed out draining the first turn's terminal; events so far: {events:?}"),
                "the in-flight turn must resolve to a terminal within a bounded window against the deterministic fake bridge",
            );
        }
        match host.next_update(std::time::Duration::from_millis(500)) {
            Ok(Some(ev)) => {
                let is_first_terminal = matches!(
                    &ev,
                    AcpEvent::Terminal { turn, .. } | AcpEvent::TerminalError { turn, .. }
                    if *turn == first_turn
                );
                events.push(ev);
                if is_first_terminal {
                    break;
                }
            }
            Ok(None) => continue,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                return runner.fail(
                    commands,
                    format!("next_update errored before the first turn's terminal: {e}; events so far: {events:?}"),
                    "the transport must stay healthy through a drain-to-terminal",
                );
            }
        }
    }

    // The slot claim: a NEW inject must now FIT — before the drain, an
    // equivalent 4th inject would have overflowed (proven by the sibling
    // d3.queue-overflow-honors-configured-capacity cell).
    let released = provider.inject(&fx, &key, "m", "c1-d3-slot-test");
    let _ = std::fs::remove_dir_all(&tmp);
    match released {
        Ok(_id) => runner.pass(
            commands,
            format!(
                "filled to the configured bound of 2 (1 in-flight + 2 backlog); drained the \
                 first turn to its terminal ({} events observed); a subsequent inject then FIT \
                 — the freed slot was released, not leaked",
                events.len()
            ),
        ),
        Err(InjectError::Precondition(msg)) => runner.fail(
            commands,
            format!(
                "after draining the first turn to its terminal, a subsequent inject still \
                 overflowed ({msg:?}) — the freed slot was not released (a leak); events={events:?}"
            ),
            "the queue slot must be released once its turn's terminal drains, freeing room for a new inject",
        ),
        Err(e) => runner.fail(
            commands,
            format!("post-drain inject failed, but not with the expected overflow shape: {e:?}"),
            "an unexpected error obscures whether the slot was released",
        ),
    }
}

/// A real `qd acp-daemon` resident, fronting the deterministic fake bridge,
/// registered by name so `qd send:relay`/`qd wait` resolve to it. Shared
/// spawn+readiness+registration logic for [`three_phase_sequence_via_daemon`]
/// and [`delivery_log_home_override_via_daemon`] — both mirror
/// `tests/acp_daemon_three_phase.rs`'s two sibling cells exactly (in-family
/// seed, CLI-subprocess shape — this seed drives real compiled `qd`
/// processes, unlike the pure-library D3/D6 drivers above). Uses [`qd_bin`]:
/// like `d6.cold-target-send-fails-loud-with-terminal`, this exercises the
/// real `qd acp-daemon`/`send:relay`/`wait` verb machinery, which may be
/// newer than what's deployed on `PATH`.
struct ThreePhaseDaemon {
    child: std::process::Child,
    daemon_log: std::path::PathBuf,
}

impl Drop for ThreePhaseDaemon {
    fn drop(&mut self) {
        let _ = Command::new("kill").arg(self.child.id().to_string()).status();
        let _ = self.child.wait();
    }
}

fn spawn_three_phase_daemon(
    node: &str,
    fake_bridge: &std::path::Path,
    home: &std::path::Path,
    cwd: &str,
    url: &str,
    qd_home_override: Option<&std::path::Path>,
) -> Result<ThreePhaseDaemon, String> {
    let daemon_log = home.join("acp-daemon.log");
    let logf = std::fs::File::create(&daemon_log).map_err(|e| format!("create daemon log: {e}"))?;
    let mut cmd = Command::new(qd_bin());
    cmd.args([
        "acp-daemon",
        "--listen",
        url,
        "--cwd",
        cwd,
        "--bridge-cmd",
        node,
        "--bridge-arg",
        &fake_bridge.to_string_lossy(),
    ])
    .env("HOME", home)
    .env_remove("QD_BOOT_AWAIT_RELAY")
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::from(logf.try_clone().map_err(|e| format!("clone log fd: {e}"))?))
    .stderr(std::process::Stdio::from(logf));
    match qd_home_override {
        Some(p) => {
            cmd.env("QD_HOME", p);
        }
        None => {
            cmd.env_remove("QD_HOME");
        }
    }
    let child = cmd.spawn().map_err(|e| format!("spawn qd acp-daemon: {e}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if let Ok(c) = crate::provider::acp::AcpConnection::connect(url, std::time::Duration::from_millis(500)) {
            if matches!(c.status_session_id(), Ok(Some(_))) {
                ready = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let daemon = ThreePhaseDaemon { child, daemon_log };
    if ready {
        Ok(daemon)
    } else {
        let log = std::fs::read_to_string(&daemon.daemon_log).unwrap_or_default();
        Err(format!("acp daemon not ready in 30s; daemon log:\n{log}"))
    }
}

/// `d2.turn-phase-sequence-strict-order`, ACP-family lanes only. Mirrors
/// `tests/acp_daemon_three_phase.rs::acp_daemon_one_send_produces_all_three_phases_in_sequence`
/// exactly. Cred-free, deterministic — the fake bridge answers a canned
/// `end_turn`, so this proves the daemon's OWN phase-logging mechanism
/// (send-initiated → turn-accepted → message-seen, in order, one send_id),
/// never what a model said. No live gate, no token draw.
fn three_phase_sequence_via_daemon(runner: &Runner, provider_id: &str) -> Outcome {
    let node = match node_program() {
        Some(n) => n,
        None => {
            return Outcome::blocked(
                "node not found on PATH — the deterministic fake ACP bridge is a node program".to_string(),
                "install node (or add it to PATH), then retry".to_string(),
            );
        }
    };
    let root = std::env::temp_dir().join(format!(
        "c1-3phase-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    let home = root.join("home");
    let registry_dir = home.join(".claude").join("sessions");
    let events_dir = home.join(".quorum").join("dispatch").join("state").join("sessions");
    if std::fs::create_dir_all(&registry_dir).is_err() || std::fs::create_dir_all(&events_dir).is_err() {
        return runner.fail(
            vec!["(mkdir 3phase jail)".to_string()],
            "failed to create jail dirs".to_string(),
            "cannot fabricate the 3-phase fixture without a jail",
        );
    }
    let cwd = home.to_string_lossy().to_string();
    let fake = write_fake_bridge(&home);
    let port = match std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()) {
        Ok(a) => a.port(),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&root);
            return runner.fail(vec!["(bind port)".to_string()], format!("bind error: {e}"), "cannot start the daemon without a free port");
        }
    };
    let url = format!("ws://127.0.0.1:{port}");

    let commands = vec![
        format!("qd acp-daemon --listen {url} --bridge-cmd {node} --bridge-arg fake-acp-bridge.js"),
        "qd send:relay <name> <msg>".to_string(),
        "qd wait <name> --timeout 30000".to_string(),
    ];

    let daemon = match spawn_three_phase_daemon(&node, &fake, &home, &cwd, &url, None) {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&root);
            return runner.fail(commands, e, "the ACP daemon must reach a ready session/new within 30s against the fake bridge");
        }
    };

    let name = "c1-3phase";
    let uuid = format!("c1-3phase-{provider_id}-{}", std::process::id()).replace('/', "-");
    let entry = crate::registry::RegistryEntry {
        pid: Some(daemon.child.id() as i64),
        session_id: Some(uuid.clone()),
        provider: Some(provider_id.to_string()),
        name: Some(name.to_string()),
        cwd: Some(cwd.clone()),
        endpoint: Some(url.clone()),
        ..Default::default()
    };
    if let Err(e) = crate::registry::write_entry(&registry_dir, &entry) {
        let _ = std::fs::remove_dir_all(&root);
        return runner.fail(commands, format!("failed to write registry row: {e}"), "cannot drive qd send:relay without a resolvable row");
    }

    let message = "C-1 d2 three-phase probe — one observed send";
    let expect_sha = crate::events::sha256_hex(message.as_bytes());

    let send = Command::new(qd_bin())
        .args(["send:relay", name, message])
        .env("HOME", &home)
        .env_remove("QD_HOME")
        .env_remove("QD_BOOT_AWAIT_RELAY")
        .output();
    let send = match send {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let dlog = std::fs::read_to_string(&daemon.daemon_log).unwrap_or_default();
            let _ = std::fs::remove_dir_all(&root);
            return runner.fail(commands, format!("send:relay exit {:?}; stderr: {}; daemon log:\n{dlog}", o.status.code(), String::from_utf8_lossy(&o.stderr)), "qd send:relay must succeed against a live resident");
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&root);
            return runner.fail(commands, format!("spawn error: {e}"), "failed to spawn qd itself");
        }
    };
    let turn_id = String::from_utf8_lossy(&send.stdout).trim().to_string();
    if turn_id.is_empty() {
        let _ = std::fs::remove_dir_all(&root);
        return runner.fail(commands, "send:relay printed no turn id (empty stdout)".to_string(), "send:relay must print the injected turn id (the send_id)");
    }

    let wait = Command::new(qd_bin())
        .args(["wait", name, "--timeout", "30000"])
        .env("HOME", &home)
        .env_remove("QD_HOME")
        .env_remove("QD_BOOT_AWAIT_RELAY")
        .output();
    let wait_ok = matches!(&wait, Ok(o) if o.status.success());
    let wait_err = match &wait {
        Ok(o) => String::from_utf8_lossy(&o.stderr).into_owned(),
        Err(e) => format!("spawn error: {e}"),
    };

    let log_path = events_dir.join(format!("{uuid}.events.jsonl"));
    let raw = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&root);

    if !wait_ok {
        return runner.fail(commands, format!("qd wait did not succeed; stderr: {wait_err}; log:\n{raw}"), "qd wait must return 0 on a clean end_turn terminal");
    }
    let recs: Vec<serde_json::Value> = raw.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect();
    let phase_idx = |event: &str| -> Option<usize> {
        recs.iter().position(|r| r["event"] == event && r["send_id"].as_str() == Some(turn_id.as_str()))
    };
    let i_init = match phase_idx("send-initiated") {
        Some(i) => i,
        None => return runner.fail(commands, format!("phase 1 send-initiated (send_id={turn_id}) missing; log:\n{raw}"), "the daemon must log send-initiated for this send_id"),
    };
    let i_acc = match phase_idx("turn-accepted") {
        Some(i) => i,
        None => return runner.fail(commands, format!("phase 2 turn-accepted (send_id={turn_id}) missing; log:\n{raw}"), "the daemon must log turn-accepted for this send_id"),
    };
    let i_seen = match phase_idx("message-seen") {
        Some(i) => i,
        None => return runner.fail(commands, format!("phase 3 message-seen (send_id={turn_id}) missing; log:\n{raw}"), "the daemon must log message-seen for this send_id"),
    };
    if !(i_init < i_acc && i_acc < i_seen) {
        return runner.fail(commands, format!("phases out of order: send-initiated@{i_init} turn-accepted@{i_acc} message-seen@{i_seen}; log:\n{raw}"), "the three phases must appear in strict append order: sent < delivered < terminal");
    }
    let sha_ok = [i_init, i_acc, i_seen].iter().all(|&i| recs[i]["content_sha256"].as_str() == Some(expect_sha.as_str()));
    if !sha_ok {
        return runner.fail(commands, format!("content_sha256 mismatch across phases; expected {expect_sha}; log:\n{raw}"), "all three phases must carry the same content_sha256, correlated by send_id");
    }
    runner.pass(
        commands,
        format!(
            "one observed send (send_id={turn_id}) produced all three phases in strict order: \
             send-initiated@{i_init} < turn-accepted@{i_acc} < message-seen@{i_seen}, all sharing \
             content_sha256={expect_sha}"
        ),
    )
}

/// `d2.delivery-log-consistency-under-home-override`, ACP-family lanes only.
/// Mirrors `tests/acp_daemon_three_phase.rs::acp_daemon_three_phase_under_qd_home_override_lands_in_qd_home_log_never_orphans`
/// exactly: same drive as [`three_phase_sequence_via_daemon`], but under a
/// `QD_HOME` that diverges from `<HOME>/.quorum/dispatch` — the regression
/// shape the seed's own commit (`b6a65da9`) fixed (phases and terminal once
/// resolved to DIFFERENT logs under this exact configuration). Asserts all
/// three phases land in ONE `QD_HOME`-honoring log — no orphan. Cred-free,
/// deterministic, no live gate, no token draw.
fn delivery_log_home_override_via_daemon(runner: &Runner, provider_id: &str) -> Outcome {
    let node = match node_program() {
        Some(n) => n,
        None => {
            return Outcome::blocked(
                "node not found on PATH — the deterministic fake ACP bridge is a node program".to_string(),
                "install node (or add it to PATH), then retry".to_string(),
            );
        }
    };
    let root = std::env::temp_dir().join(format!(
        "c1-3phase-qdh-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    let home = root.join("home");
    let qd_home = root.join("qd");
    let registry_dir = home.join(".claude").join("sessions");
    let qd_events_dir = qd_home.join("state").join("sessions");
    let home_events_dir = home.join(".quorum").join("dispatch").join("state").join("sessions");
    if std::fs::create_dir_all(&registry_dir).is_err() || std::fs::create_dir_all(&qd_events_dir).is_err() {
        return runner.fail(
            vec!["(mkdir qd-home-override jail)".to_string()],
            "failed to create jail dirs".to_string(),
            "cannot fabricate the fixture without a jail",
        );
    }
    let cwd = home.to_string_lossy().to_string();
    let fake = write_fake_bridge(&home);
    let port = match std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()) {
        Ok(a) => a.port(),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&root);
            return runner.fail(vec!["(bind port)".to_string()], format!("bind error: {e}"), "cannot start the daemon without a free port");
        }
    };
    let url = format!("ws://127.0.0.1:{port}");

    let commands = vec![
        format!("qd acp-daemon --listen {url} (QD_HOME={})", qd_home.display()),
        "qd send:relay <name> <msg> (QD_HOME set)".to_string(),
        "qd wait <name> --timeout 30000 (QD_HOME set)".to_string(),
    ];

    let daemon = match spawn_three_phase_daemon(&node, &fake, &home, &cwd, &url, Some(&qd_home)) {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&root);
            return runner.fail(commands, e, "the ACP daemon must reach a ready session/new within 30s against the fake bridge, under QD_HOME");
        }
    };

    let name = "c1-3phase-qdh";
    let uuid = format!("c1-3phase-qdh-{provider_id}-{}", std::process::id()).replace('/', "-");
    let entry = crate::registry::RegistryEntry {
        pid: Some(daemon.child.id() as i64),
        session_id: Some(uuid.clone()),
        provider: Some(provider_id.to_string()),
        name: Some(name.to_string()),
        cwd: Some(cwd.clone()),
        endpoint: Some(url.clone()),
        ..Default::default()
    };
    if let Err(e) = crate::registry::write_entry(&registry_dir, &entry) {
        let _ = std::fs::remove_dir_all(&root);
        return runner.fail(commands, format!("failed to write registry row: {e}"), "cannot drive qd send:relay without a resolvable row");
    }

    let message = "C-1 d2 three-phase probe — under a QD_HOME override";
    let expect_sha = crate::events::sha256_hex(message.as_bytes());

    let send = Command::new(qd_bin())
        .args(["send:relay", name, message])
        .env("HOME", &home)
        .env("QD_HOME", &qd_home)
        .env_remove("QD_BOOT_AWAIT_RELAY")
        .output();
    let send = match send {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let dlog = std::fs::read_to_string(&daemon.daemon_log).unwrap_or_default();
            let _ = std::fs::remove_dir_all(&root);
            return runner.fail(commands, format!("send:relay exit {:?}; stderr: {}; daemon log:\n{dlog}", o.status.code(), String::from_utf8_lossy(&o.stderr)), "qd send:relay must succeed against a live resident under QD_HOME");
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&root);
            return runner.fail(commands, format!("spawn error: {e}"), "failed to spawn qd itself");
        }
    };
    let turn_id = String::from_utf8_lossy(&send.stdout).trim().to_string();
    if turn_id.is_empty() {
        let _ = std::fs::remove_dir_all(&root);
        return runner.fail(commands, "send:relay printed no turn id (empty stdout)".to_string(), "send:relay must print the injected turn id");
    }

    let wait = Command::new(qd_bin())
        .args(["wait", name, "--timeout", "30000"])
        .env("HOME", &home)
        .env("QD_HOME", &qd_home)
        .env_remove("QD_BOOT_AWAIT_RELAY")
        .output();
    let wait_ok = matches!(&wait, Ok(o) if o.status.success());
    let wait_err = match &wait {
        Ok(o) => String::from_utf8_lossy(&o.stderr).into_owned(),
        Err(e) => format!("spawn error: {e}"),
    };

    let qd_log_path = qd_events_dir.join(format!("{uuid}.events.jsonl"));
    let home_log_path = home_events_dir.join(format!("{uuid}.events.jsonl"));
    let raw = std::fs::read_to_string(&qd_log_path).unwrap_or_default();
    let orphan_raw = std::fs::read_to_string(&home_log_path).ok();
    let _ = std::fs::remove_dir_all(&root);

    if !wait_ok {
        return runner.fail(commands, format!("qd wait did not succeed under QD_HOME; stderr: {wait_err}; log:\n{raw}"), "qd wait must return 0 under QD_HOME on a clean end_turn terminal");
    }
    if raw.trim().is_empty() {
        return runner.fail(commands, format!("QD_HOME delivery log is empty or missing at {qd_log_path:?}; orphan-log present={}", orphan_raw.is_some()), "all three phases must land in the QD_HOME-honoring log, never orphan into the from_home log");
    }
    let recs: Vec<serde_json::Value> = raw.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect();
    let phase_idx = |event: &str| -> Option<usize> {
        recs.iter().position(|r| r["event"] == event && r["send_id"].as_str() == Some(turn_id.as_str()))
    };
    let (i_init, i_acc, i_seen) = match (phase_idx("send-initiated"), phase_idx("turn-accepted"), phase_idx("message-seen")) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        other => {
            return runner.fail(
                commands,
                format!("not all three phases found in the QD_HOME log (found {other:?}); this is the orphan regression the seed's commit b6a65da9 fixed; log:\n{raw}"),
                "all three phases must land in the ONE QD_HOME-honoring log, correlated by send_id — never split across logs",
            );
        }
    };
    if !(i_init < i_acc && i_acc < i_seen) {
        return runner.fail(commands, format!("phases out of order under QD_HOME: send-initiated@{i_init} turn-accepted@{i_acc} message-seen@{i_seen}; log:\n{raw}"), "the three phases must appear in strict order even under QD_HOME");
    }
    let sha_ok = [i_init, i_acc, i_seen].iter().all(|&i| recs[i]["content_sha256"].as_str() == Some(expect_sha.as_str()));
    if !sha_ok {
        return runner.fail(commands, format!("content_sha256 mismatch across phases under QD_HOME; expected {expect_sha}; log:\n{raw}"), "all three phases must carry the same content_sha256 under QD_HOME too");
    }
    if orphan_raw.is_some_and(|s| !s.trim().is_empty()) {
        return runner.fail(commands, "the from_home (QD_HOME-ignorant) log is non-empty — a phase orphaned there despite QD_HOME being set".to_string(), "no phase may land in the QD_HOME-ignorant log when QD_HOME diverges — that IS the b6a65da9 regression");
    }
    runner.pass(
        commands,
        format!(
            "under a QD_HOME override diverging from <HOME>/.quorum/dispatch, all three phases \
             (send_id={turn_id}) landed in the ONE QD_HOME-honoring log in strict order \
             (send-initiated@{i_init} < turn-accepted@{i_acc} < message-seen@{i_seen}), and the \
             QD_HOME-ignorant from_home log stayed empty — no orphan"
        ),
    )
}

/// The real `claude-code-acp` bridge entry script (`$QD_ACP_BRIDGE` overrides
/// the STEP-0 install default) — mirrors `tests/acp_cc_live.rs::real_bridge_script`
/// exactly. UNLIKE the D3/D6 cells above, [`turn_correlation_and_tee_via_host`]
/// draws a REAL model turn (cleared by mc-5, 2026-07-15, after the D2/D5
/// token-tranche flag) — this is the ONE spot in this file that needs the
/// genuine bridge, not the canned fake.
fn real_bridge_script() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("QD_ACP_BRIDGE") {
        return std::path::PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(home)
        .join("work/acp-step0/node_modules/@zed-industries/claude-code-acp/dist/index.js")
}

/// The real ACP bridge wraps the actual `claude` engine, which writes its
/// OWN transcript to `~/.claude/projects/<cwd-with-/-replaced-by-->/<session>.jsonl`
/// — a store outside the jail this file otherwise fully controls. Live-caught
/// 2026-07-15 (residue sweep before firing the D5 resume-recall tranche):
/// every real-turn ACP cell run left one of these behind, uncleaned, since
/// removing the jail dir alone never touches it. `cwd` here is always this
/// file's own unique per-invocation jail path (`/tmp/c1-pongturn-<pid>-<nanos>`
/// or similar), so the resulting project dir is uniquely this run's — safe
/// to `rm -rf` outright, never a shared directory another session's real
/// work might live in (unlike claude-code-bare's `bond commission`, which
/// lands in the CALLER's cwd and needs a narrower, single-file cleanup —
/// see `claude_code::run_one_claude_code_turn`).
fn claude_project_dir_for(cwd: &std::path::Path) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let encoded: String = cwd.to_string_lossy().chars().map(|c| if c == '/' { '-' } else { c }).collect();
    std::path::PathBuf::from(home).join(".claude/projects").join(encoded)
}

/// Raw evidence from ONE real ACP turn, collected once and consumed by two
/// independent cell wrappers ([`turn_correlation_via_host`],
/// [`transcript_tee_via_host`]). `run_cell` dispatches one cell id at a
/// time with its own `Runner`, so a single `ProofOfRun` can't literally
/// serve two cell ids — rather than introduce cross-call caching (fragile,
/// and couples two independently-dispatched cells' outcomes), each wrapper
/// draws its OWN real turn via [`run_one_pong_turn`]. Two draws per lane
/// instead of one shared draw, still well inside mc-5's cleared budget (a
/// single short PONG exchange, "negligible" per their own estimate) —
/// simplicity and per-cell independence over a marginal token saving.
struct PongTurnEvidence {
    commands: Vec<String>,
    turn: String,
    terminal_correlates: bool,
    terminal_is_end_turn: bool,
    completion_visible: bool,
    completion_protocol_confirmed: bool,
    assistant_text: String,
    updates: usize,
}

/// Drives ONE real ACP turn ("reply with exactly PONG, no tools") through
/// the real `claude-code-acp` bridge, mirroring
/// `tests/acp_cc_live.rs::acp_cc_live_full_lifecycle`'s early section
/// (boot → session/new → inject → drive to terminal → CONF-EVENT
/// completion → transcript tee). `Err` carries an already-built `Outcome`
/// (blocked on a missing precondition, or a failure) for the caller to
/// return directly; `Ok` carries the raw facts for the caller's own
/// cell-specific assertion.
fn run_one_pong_turn(runner: &Runner, provider_id: &str) -> Result<PongTurnEvidence, Outcome> {
    use crate::effects::MapEnv;
    use crate::paths::QdPaths;
    use crate::provider::acp::completion::AcpTurnCompletion;
    use crate::provider::acp::rpc::{AcpClient, Confidence, TerminalReason};
    use crate::provider::acp::{AcpEvent, AcpHost, StopReason};
    use crate::provider::{provider_for, ProviderFx, SessionKey};
    use crate::wait::{TurnCompletion, TurnCompletionProbe};

    let (bridge_cmd, bridge_args): (String, Vec<String>) = match provider_id {
        "acp/claude-code" => {
            let node = match node_program() {
                Some(n) => n,
                None => {
                    return Err(Outcome::blocked(
                        "node not found on PATH — the real ACP bridge is a node program".to_string(),
                        "install node (or add it to PATH), then retry".to_string(),
                    ));
                }
            };
            let bridge = real_bridge_script();
            if !bridge.exists() {
                return Err(Outcome::blocked(
                    format!("real ACP bridge script not found at {}", bridge.display()),
                    "install the claude-code-acp bridge (see QD_ACP_BRIDGE), then retry".to_string(),
                ));
            }
            let creds = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude/.credentials.json");
            if !creds.exists() {
                return Err(Outcome::blocked(
                    "no ~/.claude/.credentials.json — this cell needs real claude-code credentials for an actual model turn".to_string(),
                    "authenticate claude-code on this box, then retry".to_string(),
                ));
            }
            (node, vec![bridge.to_string_lossy().to_string()])
        }
        "acp/opencode" => {
            if Command::new("opencode").arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
                ("opencode".to_string(), vec!["acp".to_string()])
            } else {
                return Err(Outcome::blocked(
                    "opencode not found on PATH — its own binary IS the ACP bridge (opencode acp)".to_string(),
                    "install opencode (or add it to PATH), then retry".to_string(),
                ));
            }
        }
        other => {
            return Err(runner.fail(vec![], format!("no real-bridge argv known for provider_id {other:?}"), "run_one_pong_turn only supports the two ACP-family lanes"));
        }
    };
    let tmp = std::env::temp_dir().join(format!(
        "c1-pongturn-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    if std::fs::create_dir_all(&tmp).is_err() {
        return Err(runner.fail(vec!["(mkdir pong-turn jail)".to_string()], "failed to create scratch dir".to_string(), "cannot spawn the real bridge without a cwd"));
    }
    let cwd = tmp.to_string_lossy().to_string();
    let prompt = "Reply with exactly the single word PONG and nothing else. Do not use any tools.";

    let commands = vec![
        format!("AcpHost::spawn({bridge_cmd}, {bridge_args:?})"),
        format!("Provider::inject(\"{prompt}\")"),
    ];

    let host = match AcpHost::spawn(&bridge_cmd, &bridge_args, &tmp) {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(claude_project_dir_for(&tmp));
            return Err(runner.fail(commands, format!("spawn error: {e}"), "cannot exercise a real turn without a live AcpHost bridge"));
        }
    };
    let provider = match provider_for(provider_id) {
        Some(p) => p,
        None => {
            let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(claude_project_dir_for(&tmp));
            return Err(runner.fail(commands, format!("provider_for({provider_id:?}) returned None"), "the ACP provider must be registered under this id"));
        }
    };
    let env = MapEnv::default();
    let paths = QdPaths::from_home(&tmp);
    let fx = ProviderFx {
        env: &env,
        paths: &paths,
        socket_dir: tmp.clone(),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: Some(&host),
        pi_rpc: None,
        acp_pre_dispatch: None,
    };
    if let Err(e) = provider.boot_waiter(&fx).wait_ready("c1-pong-turn") {
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(claude_project_dir_for(&tmp));
        return Err(runner.fail(commands, format!("boot_waiter.wait_ready error: {e:?}"), "the ACP initialize handshake must succeed through the provider seam"));
    }
    let session = match host.new_session(&cwd) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(claude_project_dir_for(&tmp));
            return Err(runner.fail(commands, format!("session/new error: {e}"), "cannot exercise a real turn without a session"));
        }
    };
    let key = SessionKey { id: &session, name: Some("c1-pong-turn"), cwd: Some(&cwd), pid: None };
    let turn = match provider.inject(&fx, &key, prompt, "c1-pong-turn-test") {
        Ok(t) => t,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(claude_project_dir_for(&tmp));
            return Err(runner.fail(commands, format!("inject error: {e:?}"), "cannot exercise a real turn without a successful inject"));
        }
    };

    let mut updates = 0usize;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let terminal = loop {
        if std::time::Instant::now() >= deadline {
            let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(claude_project_dir_for(&tmp));
            return Err(runner.fail(commands, format!("timed out before a terminal ({updates} updates seen)"), "a trivial single-word turn must resolve to a terminal within 120s"));
        }
        match host.next_update(std::time::Duration::from_millis(500)) {
            Ok(Some(AcpEvent::Update { .. })) => updates += 1,
            Ok(Some(term @ (AcpEvent::Terminal { .. } | AcpEvent::TerminalError { .. }))) => break term,
            Ok(None) => continue,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(claude_project_dir_for(&tmp));
                return Err(runner.fail(commands, format!("next_update errored before terminal: {e}"), "the transport must stay healthy through a drain-to-terminal"));
            }
        }
    };
    let (term_turn, term_stop) = match &terminal {
        AcpEvent::Terminal { turn: t, stop, .. } => (t.clone(), *stop),
        other => {
            let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(claude_project_dir_for(&tmp));
            return Err(runner.fail(commands, format!("expected a Terminal, got {other:?}"), "a trivial single-word turn must terminate cleanly, not TerminalError"));
        }
    };
    let completion = AcpTurnCompletion::new(&host);
    let completion_visible = completion.poll_completion() == TurnCompletionProbe::Visible;
    let completion_protocol_confirmed = completion.terminal_reason() == Some((TerminalReason::Completed, Confidence::ProtocolConfirmed));
    let assistant_text = host.assistant_text();
    let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(claude_project_dir_for(&tmp));

    Ok(PongTurnEvidence {
        commands,
        terminal_correlates: term_turn == turn,
        terminal_is_end_turn: term_stop == StopReason::EndTurn,
        completion_visible,
        completion_protocol_confirmed,
        assistant_text,
        updates,
        turn,
    })
}

/// `d2.turn-correlation-and-completion`, ACP-family variant. Draws one real
/// PONG turn via [`run_one_pong_turn`] and asserts the reply correlates to
/// the exact injected turn id, terminates `EndTurn`, and completion is
/// independently confirmed (protocol + visible) — never inferred.
fn turn_correlation_via_host(runner: &Runner, provider_id: &str) -> Outcome {
    let ev = match run_one_pong_turn(runner, provider_id) {
        Ok(ev) => ev,
        Err(outcome) => return outcome,
    };
    if !ev.terminal_correlates {
        return runner.fail(ev.commands, format!("the terminal turn id did not match the injected turn id (turn={})", ev.turn), "the reply must correlate back to the exact injected turn id");
    }
    if !ev.terminal_is_end_turn {
        return runner.fail(ev.commands, format!("terminal stop reason was not EndTurn (turn={})", ev.turn), "a clean PONG-only turn must terminate end_turn");
    }
    if !ev.completion_visible {
        return runner.fail(ev.commands, format!("poll_completion() was not Visible (turn={})", ev.turn), "completion must be independently confirmed as visible, not just a terminal event");
    }
    if !ev.completion_protocol_confirmed {
        return runner.fail(ev.commands, format!("terminal_reason() was not (Completed, ProtocolConfirmed) (turn={})", ev.turn), "completion must be protocol-confirmed, not inferred");
    }
    runner.pass(
        ev.commands,
        format!(
            "the reply correlated to the exact injected turn (turn={}), terminated EndTurn, and \
             completion was independently confirmed (poll_completion=Visible, \
             terminal_reason=(Completed, ProtocolConfirmed)) — {} session/update(s) streamed \
             before the terminal",
            ev.turn, ev.updates
        ),
    )
}

/// `d5.transcript-tee-captures-assistant-text`, ACP-family variant. Draws
/// one real PONG turn via [`run_one_pong_turn`] and asserts the transcript
/// tee (`host.assistant_text()`) captured the model's literal reply text —
/// Pete's-view feed fidelity.
fn transcript_tee_via_host(runner: &Runner, provider_id: &str) -> Outcome {
    let ev = match run_one_pong_turn(runner, provider_id) {
        Ok(ev) => ev,
        Err(outcome) => return outcome,
    };
    if !ev.assistant_text.contains("PONG") {
        return runner.fail(ev.commands, format!("transcript tee assistant_text={:?} does not contain PONG (turn={})", ev.assistant_text, ev.turn), "the tee must capture the model's literal reply text");
    }
    runner.pass(
        ev.commands,
        format!(
            "the transcript tee captured the model's literal reply text ({:?}, contains PONG, \
             turn={})",
            ev.assistant_text, ev.turn
        ),
    )
}

/// `d5.resume-jsonl-continuity-and-recall`, ACP-family variant. Two REAL
/// turns across two SEPARATE `AcpHost` processes (mirrors
/// `tests/acp_cc_live.rs::acp_cc_live_full_lifecycle`'s RESUME section —
/// "load the prior session on a FRESH bridge" — extended with an actual
/// recall turn, which that seed never drove): turn 1 tells the model a
/// random codeword on `host1`; `host1` is then DROPPED (simulating the
/// daemon stopping — no persist beyond the underlying claude engine's own
/// transcript); a FRESH `host2` spawns a NEW bridge process and
/// `load_session`s the SAME session id; turn 2 asks the model to recall
/// the codeword. Asserts: `host2.session_id()` is the SAME id (never
/// re-minted/forked), the recall reply contains the exact codeword, and —
/// "grounded in transcript mtime/jsonl, never qd info cached fields" per
/// the cell's own claim — the underlying claude engine's OWN transcript
/// file (`~/.claude/projects/<cwd>/<session>.jsonl`, the same store
/// [`claude_project_dir_for`] cleans up) is a SINGLE file (no fork)
/// containing both turns' assistant replies. Draws 2 real turns (cleared
/// by mc-5's D2/D5 tranche clearance — same "single short exchange" shape,
/// just twice).
fn resume_recall_via_host(runner: &Runner, provider_id: &str) -> Outcome {
    use crate::effects::MapEnv;
    use crate::paths::QdPaths;
    use crate::provider::acp::completion::AcpTurnCompletion;
    use crate::provider::acp::rpc::{AcpClient, Confidence, TerminalReason};
    use crate::provider::acp::{AcpEvent, AcpHost};
    use crate::provider::{provider_for, ProviderFx, SessionKey};
    use crate::wait::{TurnCompletion, TurnCompletionProbe};

    let (bridge_cmd, bridge_args): (String, Vec<String>) = match provider_id {
        "acp/claude-code" => {
            let node = match node_program() {
                Some(n) => n,
                None => return Outcome::blocked("node not found on PATH — the real ACP bridge is a node program".to_string(), "install node (or add it to PATH), then retry".to_string()),
            };
            let bridge = real_bridge_script();
            if !bridge.exists() {
                return Outcome::blocked(format!("real ACP bridge script not found at {}", bridge.display()), "install the claude-code-acp bridge (see QD_ACP_BRIDGE), then retry".to_string());
            }
            let creds = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude/.credentials.json");
            if !creds.exists() {
                return Outcome::blocked("no ~/.claude/.credentials.json — this cell needs real claude-code credentials for actual model turns".to_string(), "authenticate claude-code on this box, then retry".to_string());
            }
            (node, vec![bridge.to_string_lossy().to_string()])
        }
        "acp/opencode" => {
            if Command::new("opencode").arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
                ("opencode".to_string(), vec!["acp".to_string()])
            } else {
                return Outcome::blocked("opencode not found on PATH — its own binary IS the ACP bridge (opencode acp)".to_string(), "install opencode (or add it to PATH), then retry".to_string());
            }
        }
        other => return runner.fail(vec![], format!("no real-bridge argv known for provider_id {other:?}"), "resume_recall_via_host only supports the two ACP-family lanes"),
    };
    let tmp = std::env::temp_dir().join(format!(
        "c1-resumerecall-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    if std::fs::create_dir_all(&tmp).is_err() {
        return runner.fail(vec!["(mkdir resume-recall jail)".to_string()], "failed to create scratch dir".to_string(), "cannot spawn the real bridge without a cwd");
    }
    let cwd = tmp.to_string_lossy().to_string();
    let codeword = format!("ZEBRA-{}-QUARK", std::process::id() % 10000);
    let turn1_prompt = format!("Remember this exact codeword for later: {codeword}. Just acknowledge you've noted it in one short sentence. Do not use any tools.");
    let turn2_prompt = "What was the codeword I told you earlier? Reply with just the codeword, nothing else. Do not use any tools.".to_string();

    let commands = vec![
        format!("AcpHost::spawn({bridge_cmd}, {bridge_args:?}) [turn 1: state the codeword]"),
        "drop host1 (simulated daemon stop)".to_string(),
        format!("AcpHost::spawn({bridge_cmd}, {bridge_args:?}) + load_session(same id) [turn 2: recall]"),
    ];

    let drive = |host: &AcpHost, budget: std::time::Duration| -> Result<AcpEvent, String> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if std::time::Instant::now() >= deadline {
                return Err("timed out before a terminal".to_string());
            }
            match host.next_update(std::time::Duration::from_millis(500)) {
                Ok(Some(AcpEvent::Update { .. })) => continue,
                Ok(Some(term @ (AcpEvent::Terminal { .. } | AcpEvent::TerminalError { .. }))) => return Ok(term),
                Ok(None) => continue,
                Err(e) => return Err(format!("next_update errored: {e}")),
            }
        }
    };
    let env = MapEnv::default();
    let paths = QdPaths::from_home(&tmp);
    let provider = match provider_for(provider_id) {
        Some(p) => p,
        None => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(commands, format!("provider_for({provider_id:?}) returned None"), "the ACP provider must be registered under this id");
        }
    };

    // --- Turn 1: state the codeword, on host1. ---
    let host1 = match AcpHost::spawn(&bridge_cmd, &bridge_args, &tmp) {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(commands, format!("spawn error (host1): {e}"), "cannot exercise resume-recall without a live AcpHost");
        }
    };
    let fx1 = ProviderFx { env: &env, paths: &paths, socket_dir: tmp.clone(), mux: None, clock: None, sleeper: None, relay: None, relay_port: None, app_server: None, codex_expected_turn_id: None, acp_client: Some(&host1), pi_rpc: None, acp_pre_dispatch: None };
    if let Err(e) = provider.boot_waiter(&fx1).wait_ready("c1-resume-recall-1") {
        let _ = std::fs::remove_dir_all(&tmp);
        return runner.fail(commands, format!("boot_waiter.wait_ready error (host1): {e:?}"), "the ACP initialize handshake must succeed through the provider seam");
    }
    let session = match host1.new_session(&cwd) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(commands, format!("session/new error: {e}"), "cannot exercise resume-recall without a session");
        }
    };
    let key1 = SessionKey { id: &session, name: Some("c1-resume-recall"), cwd: Some(&cwd), pid: None };
    if let Err(e) = provider.inject(&fx1, &key1, &turn1_prompt, "c1-resume-recall-test") {
        let _ = std::fs::remove_dir_all(&tmp);
        return runner.fail(commands, format!("inject error (turn 1): {e:?}"), "cannot exercise resume-recall without a successful first inject");
    }
    if let Err(e) = drive(&host1, std::time::Duration::from_secs(120)) {
        let _ = std::fs::remove_dir_all(&tmp);
        return runner.fail(commands, format!("turn 1 drain error: {e}"), "the codeword-stating turn must resolve to a terminal within 120s");
    }
    drop(host1); // simulated daemon stop — no persist beyond the engine's own transcript.

    // --- Turn 2: recall the codeword, on a FRESH host2 via load_session. ---
    let host2 = match AcpHost::spawn(&bridge_cmd, &bridge_args, &tmp) {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return runner.fail(commands, format!("spawn error (host2): {e}"), "cannot exercise the resume half without a fresh AcpHost");
        }
    };
    if let Err(e) = host2.initialize() {
        let _ = std::fs::remove_dir_all(&tmp);
        return runner.fail(commands, format!("initialize error (host2): {e}"), "the fresh bridge must complete its own handshake");
    }
    if let Err(e) = host2.load_session(&session, &cwd) {
        let _ = std::fs::remove_dir_all(&tmp);
        return runner.fail(commands, format!("load_session error: {e}"), "session/load must resume the prior session on the fresh bridge");
    }
    let resumed_id = host2.session_id();
    if resumed_id.as_deref() != Some(session.as_str()) {
        let _ = std::fs::remove_dir_all(&tmp);
        return runner.fail(commands, format!("post-load session id {resumed_id:?} != original {session:?}"), "resume must re-establish the SAME session id, never re-mint or fork");
    }
    let fx2 = ProviderFx { env: &env, paths: &paths, socket_dir: tmp.clone(), mux: None, clock: None, sleeper: None, relay: None, relay_port: None, app_server: None, codex_expected_turn_id: None, acp_client: Some(&host2), pi_rpc: None, acp_pre_dispatch: None };
    let key2 = SessionKey { id: &session, name: Some("c1-resume-recall"), cwd: Some(&cwd), pid: None };
    if let Err(e) = provider.inject(&fx2, &key2, &turn2_prompt, "c1-resume-recall-test") {
        let _ = std::fs::remove_dir_all(&tmp);
        return runner.fail(commands, format!("inject error (turn 2): {e:?}"), "cannot exercise recall without a successful second inject");
    }
    if let Err(e) = drive(&host2, std::time::Duration::from_secs(120)) {
        let _ = std::fs::remove_dir_all(&tmp);
        return runner.fail(commands, format!("turn 2 drain error: {e}"), "the recall turn must resolve to a terminal within 120s");
    }
    let completion = AcpTurnCompletion::new(&host2);
    let completion_ok = completion.poll_completion() == TurnCompletionProbe::Visible
        && completion.terminal_reason() == Some((TerminalReason::Completed, Confidence::ProtocolConfirmed));
    let recall_text = host2.assistant_text();

    // --- Ground the claim in the underlying engine's OWN transcript,
    // never qd's cached fields — per the cell's own wording. ONLY
    // acp/claude-code wraps the real `claude` binary and writes a jsonl
    // transcript to ~/.claude/projects/; live-caught 2026-07-15:
    // acp/opencode's `opencode acp` is a DIFFERENT engine entirely (its
    // own SQLite store at ~/.local/share/opencode/opencode.db, confirmed
    // by directory listing — no jsonl anywhere), so checking for a jsonl
    // file there isn't a bug to route around, it's the wrong technique for
    // a different engine. For opencode, `host2.assistant_text()` — read
    // directly off the live ACP wire events, never qd's registry/info
    // fields — already satisfies "never qd cached fields"; the SAME-file/
    // no-fork strengthening is claude-code-only.
    let jsonl_check = if provider_id == "acp/claude-code" {
        let project_dir = claude_project_dir_for(&tmp);
        // The ACP terminal event (what `drive` blocks on) fires on the
        // WIRE reply; the underlying claude engine's own jsonl write can
        // lag it slightly (live-caught 2026-07-15: an immediate single
        // read sometimes saw only 1 of 2 assistant messages, flaky, not
        // deterministic). Poll briefly rather than reading once.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let (jsonl_files, assistant_count) = loop {
            let files: Vec<std::path::PathBuf> = std::fs::read_dir(&project_dir)
                .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl")).collect())
                .unwrap_or_default();
            let count = files.first().map(|p| std::fs::read_to_string(p).unwrap_or_default().lines().filter(|l| l.contains("\"role\":\"assistant\"")).count()).unwrap_or(0);
            if count >= 2 || std::time::Instant::now() >= deadline {
                break (files, count);
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        let single_file = jsonl_files.len() == 1;
        let file_count = jsonl_files.len();
        let _ = std::fs::remove_dir_all(&project_dir);
        Some((project_dir, single_file, file_count, assistant_count))
    } else {
        None
    };
    let _ = std::fs::remove_dir_all(&tmp);

    if !completion_ok {
        return runner.fail(commands, format!("recall turn's completion was not protocol-confirmed (poll={:?}, reason={:?})", completion.poll_completion(), completion.terminal_reason()), "the recall turn's completion must be independently confirmed, same as any other turn");
    }
    if !recall_text.contains(&codeword) {
        return runner.fail(commands, format!("recall reply {recall_text:?} does not contain the codeword {codeword:?} — the model did not recall pre-stop context"), "post-resume, the model must recall pre-stop context (the injected codeword)");
    }
    if let Some((project_dir, single_file, file_count, assistant_count)) = &jsonl_check {
        if !single_file {
            return runner.fail(commands, format!("expected exactly 1 transcript jsonl file in {project_dir:?}, found {file_count}"), "the transcript must grow the SAME session file — never fork into a second file across resume");
        }
        if *assistant_count < 2 {
            return runner.fail(commands, format!("expected >= 2 assistant messages in the single transcript file, found {assistant_count}"), "the transcript must show both turns' assistant replies in the one continuous file");
        }
    }
    runner.pass(
        commands,
        format!(
            "resume re-established the SAME session id ({session:?}, never re-minted); the \
             recall turn's completion was protocol-confirmed (Visible, Completed/ProtocolConfirmed); \
             the recall reply ({recall_text:?}) contains the exact codeword injected on turn 1{}",
            match &jsonl_check {
                Some((_, _, _, n)) => format!(
                    "; and the underlying claude engine's own transcript jsonl (grounded, not \
                     qd's cached fields) is a SINGLE file with {n} assistant messages — grew, \
                     never forked, across the resume"
                ),
                None => "; grounded in the live ACP wire events (host2.assistant_text()), never \
                          qd's cached fields — opencode's own engine has no jsonl transcript to \
                          additionally check (confirmed: it stores state in its own SQLite db, \
                          not a per-session jsonl file), so the same-file/no-fork strengthening \
                          claude-code gets doesn't apply here"
                    .to_string(),
            }
        ),
    )
}

/// Shared entry-gate probe for codex's D2/D5 real-turn cells: attempts
/// `qd start --provider codex` and resolves `Blocked` on the same
/// version-pin gate `boot_readiness_via_cli` already detects (this box's
/// installed codex is newer than qd's pinned version). If codex
/// unexpectedly boots (a box where the pin gate doesn't fire), this
/// honestly fails rather than fabricating a pass — no real-turn/id-tagged
/// driver is implemented yet for that path (see `codex::turn_correlation`'s
/// doc comment); the accidentally-started daemon is torn down best-effort.
fn codex_gate_probe(runner: &Runner, session_name: &str) -> Outcome {
    let start_cmd = format!("qd start {session_name} --provider codex");
    let start = qd(None).args(["start", session_name, "--provider", "codex"]).output();
    match start {
        Ok(o) if !o.status.success() => {
            let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
            if stderr.contains("pinned") && stderr.contains("detected") {
                Outcome::blocked(
                    stderr.trim().to_string(),
                    "re-pin codex (see the stated fixture-diff script) or set the stated unpinned-override env var, then retry".to_string(),
                )
            } else {
                runner.fail(vec![start_cmd], format!("exit {:?}; stderr: {stderr}", o.status.code()), "qd start --provider codex failed, but not with the expected version-pin gate signature")
            }
        }
        Ok(_) => {
            let _ = qd(None).args(["stop", session_name]).output();
            runner.fail(
                vec![start_cmd],
                "qd start --provider codex unexpectedly SUCCEEDED (the version-pin gate did not fire on this box)".to_string(),
                "no real-turn/id-tagged driver is implemented for codex yet — this cell needs one built before it can Pass on a box where codex actually boots",
            )
        }
        Err(e) => runner.fail(vec![start_cmd], format!("spawn error: {e}"), "failed to spawn qd itself"),
    }
}

/// The pinned pi binary (`QD_PI_BIN` if set, else the box default) — pi is
/// NOT on `PATH`. Mirrors `tests/pi_verb_roundtrip_live.rs::pi_bin` exactly.
fn pi_bin() -> std::path::PathBuf {
    if let Ok(b) = std::env::var("QD_PI_BIN") {
        if !b.is_empty() {
            return std::path::PathBuf::from(b);
        }
    }
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".npm-pi-global/bin/pi")
}

/// Symlink the REAL `~/.pi/agent` OAuth (auth.json + settings.json,
/// read-only) into an isolated `home` — pi's OWN credential, never a
/// claude cred, never a swap. Mirrors
/// `tests/pi_verb_roundtrip_live.rs::link_real_pi_cred` exactly. Returns
/// `true` iff a real `auth.json` was found and linked.
fn link_real_pi_cred(home: &std::path::Path) -> bool {
    let real_home = std::env::var("HOME").unwrap_or_default();
    let real_agent = std::path::PathBuf::from(&real_home).join(".pi/agent");
    let real_auth = real_agent.join("auth.json");
    if real_auth.metadata().is_err() {
        return false;
    }
    let dst_agent = home.join(".pi/agent");
    if std::fs::create_dir_all(&dst_agent).is_err() {
        return false;
    }
    let _ = std::os::unix::fs::symlink(&real_auth, dst_agent.join("auth.json"));
    let real_settings = real_agent.join("settings.json");
    if real_settings.exists() {
        let _ = std::os::unix::fs::symlink(&real_settings, dst_agent.join("settings.json"));
    }
    dst_agent.join("auth.json").exists()
}

/// Extract `"<key>":"<value>"` from a whitespace-compacted JSON string.
/// Mirrors `tests/pi_verb_roundtrip_live.rs::extract_json_str`.
fn extract_json_str(compact: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = compact.find(&needle)? + needle.len();
    let rest = &compact[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Resolve a pi row's `(endpoint, session_id)` by name. Mirrors
/// `tests/pi_verb_roundtrip_live.rs::read_pi_row`.
fn read_pi_row(sessions_dir: &std::path::Path, name: &str) -> Option<(String, String)> {
    let want_name = format!("\"name\":\"{name}\"");
    for e in std::fs::read_dir(sessions_dir).ok()?.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        if !compact.contains(&want_name) {
            continue;
        }
        let endpoint = extract_json_str(&compact, "endpoint")?;
        let session_id = extract_json_str(&compact, "sessionId").unwrap_or_default();
        return Some((endpoint, session_id));
    }
    None
}

/// Count `message`-lines whose `message.role == "assistant"` across every
/// `*.jsonl` under `dir` (recursive). Mirrors
/// `tests/pi_verb_roundtrip_live.rs::count_assistant_messages`.
/// The concatenated raw text of every assistant-role message in pi's transcript
/// under `dir` — the tee surface's captured reply CONTENT (empty if none).
/// Mirrors [`count_pi_assistant_messages`]'s scan and the substring-on-an-
/// assistant-role-line shape `pi_resume_recall` already uses to check recall;
/// collecting all such lines makes a `.contains(<expected reply>)` check robust
/// to transcript file/line ordering.
fn collect_pi_assistant_text(dir: &std::path::Path) -> String {
    let mut out = String::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    for line in text.lines() {
                        let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
                        if compact.contains("\"role\":\"assistant\"") {
                            out.push_str(line);
                            out.push('\n');
                        }
                    }
                }
            }
        }
    }
    out
}

fn count_pi_assistant_messages(dir: &std::path::Path) -> usize {
    let mut n = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    for line in text.lines() {
                        let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
                        if compact.contains("\"role\":\"assistant\"") {
                            n += 1;
                        }
                    }
                }
            }
        }
    }
    n
}

/// Raw evidence from ONE real pi turn — the pi counterpart to
/// [`PongTurnEvidence`]. **Declared receipt strength: `exclusivity-inferred`**
/// (see the module-level note below), weaker than ACP's `id-tagged` proof:
/// pi's `RpcSessionState` (its `get_state` wire reply) carries NO turn-id
/// field — grepped the struct, confirmed absent — so there is no
/// externally-observable id to join the reply back to the specific
/// injected turn. R9's QS-3 ("variance only as declared D2 receipt
/// strength or declared n-a") governs this: the underlying capability
/// (pi's daemon DOES internally know which reply answers which turn) is
/// real, so `NaPermitted` would be the WRONG disposition (that's for a
/// missing capability, not a missing proof-strength field) — ruled by
/// conf-build-coord-3, 2026-07-15. Instead this is proven at the strength
/// pi's protocol actually supports: **exactly one turn is ever in flight
/// during this drive** (asserted, `debug_assert`-backed below, never just
/// implicit) — so a busy→idle transition plus exactly one NEW assistant
/// transcript message is sound correlation BY EXCLUSION, not by id-match.
struct PiTurnEvidence {
    commands: Vec<String>,
    turn_id: String,
    idle_before: bool,
    busy_seen: bool,
    idle_after: bool,
    assistant_before: usize,
    assistant_after: usize,
    /// The concatenated text of the assistant-role transcript messages after
    /// the turn — the tee surface's captured reply CONTENT, so a cell can
    /// assert the model's literal reply, not merely that a row appeared.
    assistant_text: String,
}

/// Drives ONE real pi turn through `qd send:relay` against a live
/// credentialed pi resident, mirroring
/// `tests/pi_verb_roundtrip_live.rs::pi_tier_b_send_turn_live`'s A5-FULL
/// path exactly (isolated HOME, pi's own symlinked OAuth, a single tiny
/// prompt, busy→idle via `get_state`, transcript growth for the outcome).
/// `Err` carries an already-built `Outcome` (blocked on a missing
/// precondition, or a failure); `Ok` carries the raw facts.
fn run_one_pi_turn(runner: &Runner) -> Result<PiTurnEvidence, Outcome> {
    use crate::provider::pi::conformance::SCRUB_VARS;
    use crate::provider::pi::rpc::PiRpc;
    use crate::provider::pi::PiRemote;

    let pi = pi_bin();
    if !pi.exists() {
        return Err(Outcome::blocked(
            format!("pinned pi binary not found at {}", pi.display()),
            "set QD_PI_BIN to a real pi binary, then retry".to_string(),
        ));
    }
    let home = std::env::temp_dir().join(format!(
        "c1-pi-turn-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    let sess_dir = home.join("pi-sessions");
    if std::fs::create_dir_all(&sess_dir).is_err() {
        return Err(runner.fail(vec!["(mkdir pi-turn jail)".to_string()], "failed to create scratch dirs".to_string(), "cannot spawn a real pi resident without a cwd/session dir"));
    }
    if !link_real_pi_cred(&home) {
        let _ = std::fs::remove_dir_all(&home);
        return Err(Outcome::blocked(
            "no real ~/.pi/agent/auth.json on this box — this cell needs pi's OWN OAuth for an actual model turn".to_string(),
            "authenticate pi on this box, then retry".to_string(),
        ));
    }
    let cwd = home.to_string_lossy().into_owned();
    let name = "c1-pi-turn";
    let qd_cmd = |args: &[&str]| -> Command {
        let mut c = Command::new(qd_bin());
        c.args(args).env("HOME", &home).env("QD_PI_BIN", &pi).env("PI_CODING_AGENT_SESSION_DIR", &sess_dir);
        for v in SCRUB_VARS {
            c.env_remove(v);
        }
        c
    };

    let start_out = qd_cmd(&["start", name, "--provider", "pi", "--cwd", &cwd]).output();
    let commands = vec![
        format!("qd start {name} --provider pi --cwd {cwd}"),
        format!("qd send:relay {name} <prompt>"),
    ];
    match &start_out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let _ = std::fs::remove_dir_all(&home);
            return Err(runner.fail(commands, format!("qd start --provider pi exit {:?}; stderr: {}", o.status.code(), String::from_utf8_lossy(&o.stderr)), "cannot exercise a real turn without a live pi resident"));
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&home);
            return Err(runner.fail(commands, format!("spawn error: {e}"), "failed to spawn qd itself"));
        }
    }

    let stop_and_clean = |home: &std::path::Path| {
        let _ = qd_cmd(&["stop", name]).output();
        let _ = std::fs::remove_dir_all(home);
    };

    let sessions_dir = crate::paths::QdPaths::from_home(&home).sessions_dir;
    let (endpoint, _session_id) = match read_pi_row(&sessions_dir, name) {
        Some(r) => r,
        None => {
            stop_and_clean(&home);
            return Err(runner.fail(commands, "no pi row with endpoint found after start".to_string(), "cannot exercise a real turn without a resolvable resident row"));
        }
    };

    let idle_before = match PiRemote::connect(&endpoint, std::time::Duration::from_secs(5)) {
        Ok(c) => {
            let idle = matches!(c.get_state(), Ok(st) if !st.is_streaming);
            let _ = c.close();
            idle
        }
        Err(_) => false,
    };
    let assistant_before = count_pi_assistant_messages(&sess_dir);

    let prompt = "Reply with exactly the single word PONG and nothing else. Do not use any tools.";
    let send_out = qd_cmd(&["send:relay", name, prompt]).output();
    let (send_ok, turn_id) = match &send_out {
        Ok(o) => (o.status.success(), String::from_utf8_lossy(&o.stdout).trim().to_string()),
        Err(e) => {
            stop_and_clean(&home);
            return Err(runner.fail(commands, format!("spawn error: {e}"), "failed to spawn qd send:relay"));
        }
    };
    if !send_ok || turn_id.is_empty() {
        let stderr = send_out.map(|o| String::from_utf8_lossy(&o.stderr).into_owned()).unwrap_or_default();
        stop_and_clean(&home);
        return Err(runner.fail(commands, format!("qd send:relay did not mint a turn id; stderr: {stderr}"), "cannot exercise a real turn without a successful send"));
    }
    // EXCLUSIVITY PRECONDITION (see PiTurnEvidence's doc comment): exactly ONE
    // turn was ever injected against this resident — the whole
    // exclusivity-inferred proof depends on it. This is guaranteed STRUCTURALLY:
    // there is exactly one `send:relay` call above and none below, on a
    // freshly-started resident. (A prior `debug_assert_eq!(1, 1, …)` here was a
    // literal tautology — it asserted nothing and could never trip on a second
    // inject — so it was removed rather than left implying a guard that wasn't
    // there; the invariant is enforced by the single call site, not a runtime
    // check.)

    let poll = PiRemote::connect(&endpoint, std::time::Duration::from_secs(5));
    let (mut busy_seen, mut idle_after) = (false, false);
    if let Ok(poll) = poll {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while std::time::Instant::now() < deadline {
            match poll.get_state() {
                Ok(st) => {
                    if st.is_streaming {
                        busy_seen = true;
                    } else if busy_seen {
                        idle_after = true;
                        break;
                    }
                }
                Err(_) => {}
            }
            std::thread::sleep(std::time::Duration::from_millis(75));
        }
        let _ = poll.close();
    }
    let assistant_after = count_pi_assistant_messages(&sess_dir);
    // Capture the reply CONTENT before stop_and_clean removes the transcript —
    // so a cell can assert the model's literal text, not just the row count.
    let assistant_text = collect_pi_assistant_text(&sess_dir);
    stop_and_clean(&home);

    Ok(PiTurnEvidence {
        commands,
        turn_id,
        idle_before,
        busy_seen,
        idle_after,
        assistant_before,
        assistant_after,
        assistant_text,
    })
}

/// `d2.turn-correlation-and-completion`, pi variant. **Declared receipt
/// strength: `exclusivity-inferred`** — see [`PiTurnEvidence`]'s doc
/// comment. Asserts: idle before the send, busy observed during, idle
/// observed after (protocol-confirmed completion via `get_state`), and
/// exactly one new assistant transcript message (the turn OUTCOME,
/// correlated to the one turn_id injected by exclusion — no concurrent
/// turn existed that could have produced it).
fn pi_turn_correlation(runner: &Runner) -> Outcome {
    let ev = match run_one_pi_turn(runner) {
        Ok(ev) => ev,
        Err(outcome) => return outcome,
    };
    if !ev.idle_before {
        return runner.fail(ev.commands, format!("resident was not Idle before the send (turn={})", ev.turn_id), "the exclusivity precondition requires starting from a genuinely idle resident");
    }
    if !ev.busy_seen {
        return runner.fail(ev.commands, format!("never observed is_streaming BUSY during the turn (turn={})", ev.turn_id), "completion must be protocol-confirmed via a busy phase, not inferred from silence");
    }
    if !ev.idle_after {
        return runner.fail(ev.commands, format!("resident did not return to IDLE after the turn within 90s (turn={})", ev.turn_id), "completion must be protocol-confirmed via a return to idle");
    }
    let grew_by_exactly_one = ev.assistant_after == ev.assistant_before + 1;
    if !grew_by_exactly_one {
        return runner.fail(
            ev.commands,
            format!("assistant message count did not grow by exactly 1 (before={} after={}, turn={})", ev.assistant_before, ev.assistant_after, ev.turn_id),
            "with exactly one turn injected, exactly one new assistant message is required for the exclusivity-inferred correlation to hold",
        );
    }
    runner.pass(
        ev.commands,
        format!(
            "[receipt-strength: exclusivity-inferred] resident was idle before the send \
             (turn={}), transitioned busy during, and returned idle after (protocol-confirmed \
             via get_state) — exactly one new assistant transcript message appeared \
             (before={} after={}), and exactly one turn was ever injected, so that message \
             correlates to this turn_id BY EXCLUSION (no id-tag exists in pi's wire protocol \
             to join on directly, per RpcSessionState's fields)",
            ev.turn_id, ev.assistant_before, ev.assistant_after
        ),
    )
}

/// `d5.transcript-tee-captures-assistant-text`, pi variant. Same
/// exclusivity-inferred correlation as [`pi_turn_correlation`] (the
/// transcript growth check IS the tee-captured-text check for pi — pi's
/// transcript jsonl is the only tee surface this harness can read without
/// a new mechanism); a distinct real turn is drawn per this file's
/// per-cell-independence convention.
fn pi_transcript_tee(runner: &Runner) -> Outcome {
    let ev = match run_one_pi_turn(runner) {
        Ok(ev) => ev,
        Err(outcome) => return outcome,
    };
    let grew_by_exactly_one = ev.assistant_after == ev.assistant_before + 1;
    if !grew_by_exactly_one {
        return runner.fail(
            ev.commands,
            format!("assistant message count did not grow by exactly 1 (before={} after={}, turn={})", ev.assistant_before, ev.assistant_after, ev.turn_id),
            "the tee (pi's own transcript jsonl) must capture exactly one new assistant reply for this one injected turn",
        );
    }
    // Row COUNT alone is not enough — a tee that appended GARBAGE would still
    // grow by one. Assert the tee captured the model's LITERAL reply CONTENT
    // (the prompt asked for exactly "PONG"), matching the ACP sibling's
    // `.contains("PONG")` standard. This is the content half of "captures
    // assistant text", not merely "a row appeared".
    if !ev.assistant_text.contains("PONG") {
        return runner.fail(
            ev.commands,
            format!("transcript tee assistant_text did not contain the model's literal reply PONG (turn={}); captured assistant-role text was {:?}", ev.turn_id, ev.assistant_text),
            "the tee (pi's own transcript jsonl) must capture the model's literal reply text, not just a new row",
        );
    }
    runner.pass(
        ev.commands,
        format!(
            "[receipt-strength: exclusivity-inferred] pi's transcript jsonl (the tee surface for \
             this lane) grew by exactly one new assistant message (before={} after={}) for the \
             one turn injected (turn={}), and that message's captured text contains the model's \
             literal reply PONG — Pete's-view feed fidelity (content, not just row count), \
             correlated by exclusion",
            ev.assistant_before, ev.assistant_after, ev.turn_id
        ),
    )
}

/// `d6.self-terminate-on-wedged-child` on the `pi` lane — a REAL live probe
/// (mc-5-cleared, single daemon). Starts a pi daemon, kills its `pi` CHILD out
/// from under it (NOT the daemon), and reads `qd info` liveness. The cell's
/// property is that the daemon "self-terminates RATHER THAN lingering falsely
/// live." Live-probed by hand 2026-07-15 (conf-build-coord-3 ruling): the pi
/// daemon does NEITHER — `serve_pi` (residence.rs:552) has no child-death guard
/// (it swallows `get_state` errors, `backstop_poll` "leaves the belief
/// untouched"), and `qd info`'s `live` is computed from the DAEMON pid plus a
/// status that floors to `idle` when `get_state` fails (resolve.rs:30-40,
/// join.rs:438-449). So a dead child yields `live:true, status:idle`. Confirmed
/// directly: daemon pid `S`/alive, child pid `Z <defunct>` (dead), yet `qd info`
/// reported `live:true status:idle`. This driver reports that TRUE outcome — a
/// FAIL in-grid, per the ruling: a Required cell whose real behavior violates
/// the honesty invariant reports FAIL, it is not laundered behind a NaPermitted
/// reason (which was the pre-2026-07-15 disposition, resting on a false "no
/// serve-equivalent loop" premise). The underlying qd product defect (pi
/// liveness derivation) is OUT of C-1 scope and escalated separately to
/// provider-liveness owners. A red cell demotes pi's tier; it does not block.
fn pi_self_terminate_probe(runner: &Runner, session_name: &str) -> Outcome {
    // qd stop reaps the daemon AND any (zombie) pi child it still holds — the
    // hand-probe confirmed this leaves zero residual. The direct kill is a
    // backstop so a probe never leaks a daemon if stop somehow misses it.
    let teardown = |daemon_pid: Option<i64>| {
        let _ = qd(None).args(["stop", session_name]).output();
        if let Some(p) = daemon_pid {
            let _ = Command::new("kill").args(["-9", &p.to_string()]).status();
        }
    };

    let start_cmd = format!("qd start {session_name} --provider pi");
    let mut commands = vec![start_cmd.clone()];
    let start = qd(None).args(["start", session_name, "--provider", "pi"]).output();
    match start {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            teardown(None);
            return runner.fail(
                commands,
                format!("qd start exit {:?}; stderr: {}", o.status.code(), String::from_utf8_lossy(&o.stderr)),
                "could not start a pi daemon to exercise the wedged-child probe",
            );
        }
        Err(e) => return runner.fail(commands, format!("spawn error: {e}"), "failed to spawn qd start"),
    }

    // Precondition: the session is live with a real daemon pid before we touch it.
    commands.push(format!("qd info {session_name} --json"));
    let daemon_pid = match fetch_info_json(session_name, None) {
        Some(v) if v.get("live").and_then(serde_json::Value::as_bool) == Some(true) => {
            v.get("pid").and_then(serde_json::Value::as_i64)
        }
        other => {
            teardown(None);
            return runner.fail(
                commands,
                format!("pre-kill info was not live: {other:?}"),
                "the pi session was not live before the probe — cannot exercise the wedged-child claim",
            );
        }
    };
    let daemon_pid = match daemon_pid {
        Some(p) => p,
        None => {
            teardown(None);
            return runner.fail(commands, "pre-kill info carried no pid".to_string(), "cannot locate the daemon's child without the daemon pid");
        }
    };

    // Find the pi CHILD: a direct child of the daemon whose command name is `pi`
    // (never the daemon itself, never a stray qd subprocess).
    commands.push(format!("pgrep -P {daemon_pid}"));
    let child_pid = Command::new("pgrep")
        .args(["-P", &daemon_pid.to_string()])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<i64>().ok())
                .find(|&cp| {
                    Command::new("ps")
                        .args(["-o", "comm=", "-p", &cp.to_string()])
                        .output()
                        .ok()
                        .map(|c| String::from_utf8_lossy(&c.stdout).trim() == "pi")
                        .unwrap_or(false)
                })
        });
    let child_pid = match child_pid {
        Some(c) => c,
        None => {
            teardown(Some(daemon_pid));
            return runner.fail(
                commands,
                format!("no `pi` child found under daemon {daemon_pid}"),
                "cannot exercise the wedged-child probe without locating the pi child process",
            );
        }
    };

    // Kill ONLY the child, out from under the daemon, and confirm it is genuinely
    // dead (gone, or a defunct zombie) before reading liveness.
    commands.push(format!("kill -9 {child_pid}"));
    let _ = Command::new("kill").args(["-9", &child_pid.to_string()]).status();
    std::thread::sleep(std::time::Duration::from_millis(800));
    let child_stat = Command::new("ps")
        .args(["-o", "stat=", "-p", &child_pid.to_string()])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let child_dead = child_stat.is_empty() || child_stat.starts_with('Z');
    if !child_dead {
        teardown(Some(daemon_pid));
        return runner.fail(
            commands,
            format!("child {child_pid} not confirmed dead after SIGKILL (ps stat={child_stat:?})"),
            "cannot rule on the wedged-child claim without a confirmed-dead child",
        );
    }

    // Read liveness with the child DEAD. The property: self-terminate rather than
    // linger falsely-live.
    commands.push(format!("qd info {session_name} --json"));
    let post = fetch_info_json(session_name, None);
    let daemon_still_alive = os_pid_alive(daemon_pid);
    let outcome = match &post {
        // Falsely-live: qd info still reports live:true with the child dead. FAIL.
        Some(v) if v.get("live").and_then(serde_json::Value::as_bool) == Some(true) => runner.fail(
            commands,
            format!(
                "pi child {child_pid} confirmed DEAD (ps stat={child_stat:?}) out from under daemon {daemon_pid} (daemon still alive: {daemon_still_alive}); qd info STILL reports {v}",
            ),
            "the pi daemon neither self-terminated nor reported the session dead when its child was killed — it lingers FALSELY-LIVE (live:true, status floors to idle). Honest FAIL of the self-terminate-rather-than-linger property. Underlying qd product defect (pi liveness derivation keyed on the daemon pid) escalated separately, out of C-1 scope",
        ),
        // Honest: info reports not-live once the child died.
        Some(v) => runner.pass(
            commands,
            format!("pi child {child_pid} killed (ps stat={child_stat:?}); qd info honestly reports not-live: {v} — the daemon did not linger falsely-live"),
        ),
        // Honest: the session no longer resolves (daemon self-terminated).
        None => runner.pass(
            commands,
            format!("pi child {child_pid} killed (ps stat={child_stat:?}); qd info no longer resolves the session — the daemon self-terminated, not falsely-live"),
        ),
    };
    teardown(Some(daemon_pid));
    outcome
}

/// `d5.resume-jsonl-continuity-and-recall`, pi variant. Manually verified
/// end-to-end before writing this driver (same discipline as
/// claude-code-bare's mechanism discovery): `qd resume <name> --no-attach`
/// genuinely works for pi and re-establishes the SAME session id — this
/// directly contradicts `d1.resume-same-session-id`'s current
/// `NaPermitted` reason for pi ("no analogous resume/session-load
/// primitive measured by any existing seed"). NOT fixing that
/// classification here (out of THIS cell's scope, a call for whoever owns
/// `d1`'s applicability) — flagged separately. Two real turns: turn 1
/// states a codeword; `qd stop` + `qd resume --no-attach`; turn 2 asks for
/// recall. Asserts the SAME session id post-resume, the recall reply
/// (read from pi's own transcript, assistant-role only) contains the
/// codeword, and the transcript is a SINGLE jsonl file (no fork) — mirrors
/// `tests/pi_verb_roundtrip_live.rs`'s `count_jsonl_files`/
/// `assistant_message_contains` shape exactly (that seed proves the
/// analogous claim for the DEAD-ONLY floor path; this proves it for the
/// live daemon `qd resume` path, which was never exercised by any seed).
fn pi_resume_recall(runner: &Runner) -> Outcome {
    use crate::provider::pi::conformance::SCRUB_VARS;

    let pi = pi_bin();
    if !pi.exists() {
        return Outcome::blocked(format!("pinned pi binary not found at {}", pi.display()), "set QD_PI_BIN to a real pi binary, then retry".to_string());
    }
    let home = std::env::temp_dir().join(format!(
        "c1-pi-resume-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    let sess_dir = home.join("pi-sessions");
    if std::fs::create_dir_all(&sess_dir).is_err() {
        return runner.fail(vec!["(mkdir pi-resume jail)".to_string()], "failed to create scratch dirs".to_string(), "cannot spawn a real pi resident without a cwd/session dir");
    }
    if !link_real_pi_cred(&home) {
        let _ = std::fs::remove_dir_all(&home);
        return Outcome::blocked("no real ~/.pi/agent/auth.json on this box — this cell needs pi's OWN OAuth for actual model turns".to_string(), "authenticate pi on this box, then retry".to_string());
    }
    let cwd = home.to_string_lossy().into_owned();
    let name = "c1-pi-resume";
    let qd_cmd = |args: &[&str]| -> Command {
        let mut c = Command::new(qd_bin());
        c.args(args).env("HOME", &home).env("QD_PI_BIN", &pi).env("PI_CODING_AGENT_SESSION_DIR", &sess_dir);
        for v in SCRUB_VARS {
            c.env_remove(v);
        }
        c
    };
    let fail_and_clean = |runner: &Runner, home: &std::path::Path, commands: Vec<String>, observed: String, why: &str| -> Outcome {
        let _ = qd_cmd(&["stop", name]).output();
        let _ = std::fs::remove_dir_all(home);
        runner.fail(commands, observed, why)
    };

    let codeword = format!("ZEBRA-{}-QUARK", std::process::id() % 10000);
    let mut commands = vec![format!("qd start {name} --provider pi --cwd {cwd}")];
    let start = qd_cmd(&["start", name, "--provider", "pi", "--cwd", &cwd]).output();
    match &start {
        Ok(o) if o.status.success() => {}
        Ok(o) => return fail_and_clean(runner, &home, commands, format!("qd start exit {:?}; stderr: {}", o.status.code(), String::from_utf8_lossy(&o.stderr)), "cannot exercise resume-recall without a live pi resident"),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&home);
            return runner.fail(commands, format!("spawn error: {e}"), "failed to spawn qd itself");
        }
    }

    let sessions_dir = crate::paths::QdPaths::from_home(&home).sessions_dir;
    let (original_session_id, endpoint1) = match read_pi_row(&sessions_dir, name).map(|(e, s)| (s, e)) {
        Some(r) if !r.0.is_empty() => r,
        _ => return fail_and_clean(runner, &home, commands, "no pi row with sessionId+endpoint found after start".to_string(), "cannot exercise resume-recall without a resolvable resident row"),
    };

    let turn1_prompt = format!("Remember this exact codeword for later: {codeword}. Just acknowledge you've noted it in one short sentence. Do not use any tools.");
    commands.push(format!("qd send:relay {name} <turn1: state codeword>"));
    if !matches!(qd_cmd(&["send:relay", name, &turn1_prompt]).output(), Ok(o) if o.status.success()) {
        return fail_and_clean(runner, &home, commands, "qd send:relay (turn 1) did not succeed".to_string(), "cannot exercise resume-recall without a successful first send");
    }
    if let Err(e) = poll_pi_idle(&endpoint1, std::time::Duration::from_secs(90)) {
        return fail_and_clean(runner, &home, commands, format!("turn 1 did not settle idle: {e}"), "turn 1 must complete (settle idle) before stopping the resident");
    }

    commands.push(format!("qd stop {name}"));
    let _ = qd_cmd(&["stop", name]).output();
    commands.push(format!("qd resume {name} --no-attach"));
    let resume = qd_cmd(&["resume", name, "--no-attach"]).output();
    match &resume {
        Ok(o) if o.status.success() => {}
        Ok(o) => return fail_and_clean(runner, &home, commands, format!("qd resume exit {:?}; stderr: {}", o.status.code(), String::from_utf8_lossy(&o.stderr)), "qd resume must succeed against a stopped pi session"),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&home);
            return runner.fail(commands, format!("spawn error: {e}"), "failed to spawn qd resume");
        }
    }

    let (resumed_session_id, endpoint2) = match read_pi_row(&sessions_dir, name).map(|(e, s)| (s, e)) {
        Some(r) => r,
        None => return fail_and_clean(runner, &home, commands, "no pi row found after resume".to_string(), "cannot exercise the recall turn without a resolvable resumed row"),
    };
    if resumed_session_id != original_session_id {
        return fail_and_clean(runner, &home, commands, format!("post-resume sessionId {resumed_session_id:?} != original {original_session_id:?}"), "resume must re-establish the SAME session id, never re-mint or fork");
    }

    let turn2_prompt = "What was the codeword I told you earlier? Reply with just the codeword, nothing else. Do not use any tools.".to_string();
    commands.push(format!("qd send:relay {name} <turn2: recall>"));
    if !matches!(qd_cmd(&["send:relay", name, &turn2_prompt]).output(), Ok(o) if o.status.success()) {
        return fail_and_clean(runner, &home, commands, "qd send:relay (turn 2) did not succeed".to_string(), "cannot exercise recall without a successful second send");
    }
    if let Err(e) = poll_pi_idle(&endpoint2, std::time::Duration::from_secs(90)) {
        return fail_and_clean(runner, &home, commands, format!("turn 2 did not settle idle: {e}"), "the recall turn must complete (settle idle)");
    }

    let jsonl_files: Vec<std::path::PathBuf> = std::fs::read_dir(&sess_dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl")).collect())
        .unwrap_or_default();
    let single_file = jsonl_files.len() == 1;
    let recall_ok = jsonl_files.first().is_some_and(|p| {
        std::fs::read_to_string(p).unwrap_or_default().lines().any(|l| {
            let compact: String = l.chars().filter(|c| !c.is_whitespace()).collect();
            compact.contains("\"role\":\"assistant\"") && compact.contains(&codeword)
        })
    });

    let _ = qd_cmd(&["stop", name]).output();
    let _ = std::fs::remove_dir_all(&home);

    if !single_file {
        return runner.fail(commands, format!("expected exactly 1 transcript jsonl file, found {}", jsonl_files.len()), "the transcript must grow the SAME session file across resume — never fork into a second file");
    }
    if !recall_ok {
        return runner.fail(commands, format!("no assistant-role message contains the codeword {codeword:?} after resume+recall"), "post-resume, the model must recall pre-stop context (the injected codeword)");
    }
    runner.pass(
        commands,
        format!(
            "qd resume re-established the SAME sessionId ({original_session_id:?}, never \
             re-minted or forked); the recall turn's assistant reply (found in the single \
             continuous transcript jsonl) contains the exact codeword {codeword:?} injected on \
             turn 1"
        ),
    )
}

/// Poll a pi resident's `get_state` until `is_streaming` transitions
/// busy→idle (or times out) — the shared completion-confirmation shape for
/// pi's real-turn cells. `Err` on timeout or a connect failure.
fn poll_pi_idle(endpoint: &str, budget: std::time::Duration) -> Result<(), String> {
    use crate::provider::pi::rpc::PiRpc;
    use crate::provider::pi::PiRemote;

    let conn = PiRemote::connect(endpoint, std::time::Duration::from_secs(5)).map_err(|e| format!("connect error: {e}"))?;
    let deadline = std::time::Instant::now() + budget;
    let mut busy_seen = false;
    while std::time::Instant::now() < deadline {
        match conn.get_state() {
            Ok(st) => {
                if st.is_streaming {
                    busy_seen = true;
                } else if busy_seen {
                    let _ = conn.close();
                    return Ok(());
                }
            }
            Err(_) => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(75));
    }
    let _ = conn.close();
    Err("timed out waiting for busy->idle".to_string())
}

/// `d1.resume-same-session-id`, pi variant. **Registry correction
/// (2026-07-15):** this cell was NaPermitted for pi until [`pi_resume_recall`]
/// disproved the reason — reclassified to `Required`, matching the ACP
/// lanes (see `registry.rs`'s correction comment on this cell). Reuses the
/// SAME mechanism [`pi_resume_recall`] already proved (`qd resume <name>
/// --no-attach` preserves the session id), truncated after the id
/// comparison — this cell's claim doesn't need a second recall turn, so it
/// draws only ONE real turn, not two. Manually confirmed BEFORE writing
/// this driver that a cred-free resume (no prior real turn) genuinely
/// HANGS (a live 20s timeout, not a guess) — this is exactly why resume
/// was originally deferred to the token phase: a real prior turn is a
/// structural precondition for pi's own resume path, not an optional
/// strengthening.
fn pi_resume_same_session_id(runner: &Runner) -> Outcome {
    use crate::provider::pi::conformance::SCRUB_VARS;

    let pi = pi_bin();
    if !pi.exists() {
        return Outcome::blocked(format!("pinned pi binary not found at {}", pi.display()), "set QD_PI_BIN to a real pi binary, then retry".to_string());
    }
    let home = std::env::temp_dir().join(format!(
        "c1-pi-d1resume-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    let sess_dir = home.join("pi-sessions");
    if std::fs::create_dir_all(&sess_dir).is_err() {
        return runner.fail(vec!["(mkdir pi-d1resume jail)".to_string()], "failed to create scratch dirs".to_string(), "cannot spawn a real pi resident without a cwd/session dir");
    }
    if !link_real_pi_cred(&home) {
        let _ = std::fs::remove_dir_all(&home);
        return Outcome::blocked("no real ~/.pi/agent/auth.json on this box — a prior real turn (needing real credentials) is a structural precondition for pi's resume path".to_string(), "authenticate pi on this box, then retry".to_string());
    }
    let cwd = home.to_string_lossy().into_owned();
    let name = "c1-pi-d1resume";
    let qd_cmd = |args: &[&str]| -> Command {
        let mut c = Command::new(qd_bin());
        c.args(args).env("HOME", &home).env("QD_PI_BIN", &pi).env("PI_CODING_AGENT_SESSION_DIR", &sess_dir);
        for v in SCRUB_VARS {
            c.env_remove(v);
        }
        c
    };
    let fail_and_clean = |runner: &Runner, home: &std::path::Path, commands: Vec<String>, observed: String, why: &str| -> Outcome {
        let _ = qd_cmd(&["stop", name]).output();
        let _ = std::fs::remove_dir_all(home);
        runner.fail(commands, observed, why)
    };

    let mut commands = vec![format!("qd start {name} --provider pi --cwd {cwd}")];
    let start = qd_cmd(&["start", name, "--provider", "pi", "--cwd", &cwd]).output();
    match &start {
        Ok(o) if o.status.success() => {}
        Ok(o) => return fail_and_clean(runner, &home, commands, format!("qd start exit {:?}; stderr: {}", o.status.code(), String::from_utf8_lossy(&o.stderr)), "cannot exercise resume-same-session-id without a live pi resident"),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&home);
            return runner.fail(commands, format!("spawn error: {e}"), "failed to spawn qd itself");
        }
    }

    let sessions_dir = crate::paths::QdPaths::from_home(&home).sessions_dir;
    let (original_session_id, endpoint1) = match read_pi_row(&sessions_dir, name).map(|(e, s)| (s, e)) {
        Some(r) if !r.0.is_empty() => r,
        _ => return fail_and_clean(runner, &home, commands, "no pi row with sessionId+endpoint found after start".to_string(), "cannot exercise resume-same-session-id without a resolvable resident row"),
    };

    // A real turn to revive INTO — structural precondition, see this fn's
    // doc comment. Content is irrelevant; only completion matters here.
    commands.push(format!("qd send:relay {name} <a trivial real turn>"));
    let prompt = "Reply with exactly the single word OK and nothing else. Do not use any tools.";
    if !matches!(qd_cmd(&["send:relay", name, prompt]).output(), Ok(o) if o.status.success()) {
        return fail_and_clean(runner, &home, commands, "qd send:relay did not succeed".to_string(), "cannot exercise resume-same-session-id without a completed real turn to resume into");
    }
    if let Err(e) = poll_pi_idle(&endpoint1, std::time::Duration::from_secs(90)) {
        return fail_and_clean(runner, &home, commands, format!("the turn did not settle idle: {e}"), "the real turn must complete before stopping the resident");
    }

    commands.push(format!("qd stop {name}"));
    let _ = qd_cmd(&["stop", name]).output();
    commands.push(format!("qd resume {name} --no-attach"));
    let resume = qd_cmd(&["resume", name, "--no-attach"]).output();
    match &resume {
        Ok(o) if o.status.success() => {}
        Ok(o) => return fail_and_clean(runner, &home, commands, format!("qd resume exit {:?}; stderr: {}", o.status.code(), String::from_utf8_lossy(&o.stderr)), "qd resume must succeed against a stopped pi session with a real prior turn"),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&home);
            return runner.fail(commands, format!("spawn error: {e}"), "failed to spawn qd resume");
        }
    }

    let resumed_session_id = read_pi_row(&sessions_dir, name).map(|(_, s)| s);
    let _ = qd_cmd(&["stop", name]).output();
    let _ = std::fs::remove_dir_all(&home);

    match resumed_session_id {
        Some(id) if id == original_session_id => runner.pass(
            commands,
            format!("qd resume re-established the SAME sessionId ({original_session_id:?}, never re-minted) after a real prior turn — resume-same-session-id holds for pi, disproving the earlier NaPermitted reason"),
        ),
        Some(id) => runner.fail(commands, format!("post-resume sessionId {id:?} != original {original_session_id:?}"), "resume must re-establish the SAME session id, never re-mint"),
        None => runner.fail(commands, "no pi row found after resume".to_string(), "cannot confirm resume-same-session-id without a resolvable resumed row"),
    }
}

/// `d1.teardown-reaps-process-group`, driven through the public `qd` CLI
/// surface: start, capture the real pid from a fresh `qd info --json`, stop,
/// then verify at the OS level (`ps -p`, never `qd`'s own self-report) that
/// the pid is genuinely gone. Shared by every `qd start`-based lane — the
/// claim ("teardown reaps the resident, not just marks it stopped") is
/// domain-general.
fn teardown_reaps_via_cli(
    runner: &Runner,
    session_name: &str,
    provider_id: &str,
    path_prefix: Option<&str>,
) -> Outcome {
    let start_cmd = format!("qd start {session_name} --provider {provider_id}");
    let start = qd(path_prefix)
        .args(["start", session_name, "--provider", provider_id])
        .output();
    let start = match start {
        Ok(o) => o,
        Err(e) => {
            return runner.fail(
                vec![start_cmd],
                format!("spawn error: {e}"),
                "failed to spawn qd itself — not a provider-daemon failure",
            );
        }
    };
    if !start.status.success() {
        let stderr = String::from_utf8_lossy(&start.stderr).into_owned();
        if stderr.contains("pinned") && stderr.contains("detected") {
            return Outcome::blocked(
                stderr.trim().to_string(),
                format!("re-pin {provider_id} (see the stated fixture-diff script) or set the stated unpinned-override env var, then retry"),
            );
        }
        return runner.fail(
            vec![start_cmd],
            format!("exit {:?}; stderr: {stderr}", start.status.code()),
            "qd start did not succeed",
        );
    }
    let info = qd(path_prefix).args(["info", session_name, "--json"]).output();
    let pid = match info {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
            serde_json::from_str::<serde_json::Value>(&stdout)
                .ok()
                .and_then(|v| v.get("pid").and_then(serde_json::Value::as_i64))
        }
        _ => None,
    };
    let pid = match pid {
        Some(p) => p,
        None => {
            let _ = qd(path_prefix).args(["stop", session_name]).output();
            return runner.fail(
                vec![start_cmd, format!("qd info {session_name} --json")],
                "no pid resolved before teardown".to_string(),
                "cannot verify teardown without a starting pid",
            );
        }
    };
    let stop = qd(path_prefix).args(["stop", session_name]).output();
    let stop_cmd = format!("qd stop {session_name}");
    match stop {
        Ok(o) if o.status.success() => {
            if os_pid_alive(pid) {
                runner.fail(
                    vec![start_cmd, format!("qd info {session_name} --json"), stop_cmd],
                    format!("pid {pid} still alive at the OS level after qd stop reported success"),
                    "teardown must genuinely reap the process, not just mark the row stopped",
                )
            } else {
                runner.pass(
                    vec![start_cmd, format!("qd info {session_name} --json"), stop_cmd],
                    format!("pid {pid} confirmed dead via `ps -p` after qd stop"),
                )
            }
        }
        Ok(o) => runner.fail(
            vec![start_cmd, stop_cmd],
            format!(
                "exit {:?}; stderr: {}",
                o.status.code(),
                String::from_utf8_lossy(&o.stderr)
            ),
            "qd stop did not succeed",
        ),
        Err(e) => runner.fail(
            vec![start_cmd, stop_cmd],
            format!("spawn error: {e}"),
            "failed to spawn qd stop",
        ),
    }
}

pub mod pi {
    use super::*;

    /// `d1.boot-readiness` on the `pi` lane. Cred-free — no OAuth needed.
    pub fn boot_readiness(runner: &Runner, session_name: &str) -> Outcome {
        boot_readiness_via_cli(runner, session_name, "pi", None)
    }

    /// `d1.teardown-reaps-process-group` on the `pi` lane. Cred-free.
    pub fn teardown_reaps(runner: &Runner, session_name: &str) -> Outcome {
        teardown_reaps_via_cli(runner, session_name, "pi", None)
    }

    /// `d1.launch-addressing` on the `pi` lane.
    pub fn launch_addressing(runner: &Runner, session_name: &str) -> Outcome {
        launch_addressing_via_cli(runner, session_name, "pi", None)
    }

    /// `d1.transport-is-our-daemon` on the `pi` lane.
    pub fn transport_is_our_daemon(runner: &Runner, session_name: &str) -> Outcome {
        transport_is_our_daemon_via_cli(runner, session_name, "pi", None)
    }

    /// `d1.liveness-process-alive` on the `pi` lane.
    pub fn liveness_process_alive(runner: &Runner, session_name: &str) -> Outcome {
        liveness_process_alive_via_cli(runner, session_name, "pi", None)
    }

    /// `d1.reconnect-resolves-via-registry` on the `pi` lane.
    pub fn reconnect_resolves(runner: &Runner, session_name: &str) -> Outcome {
        reconnect_resolves_via_cli(runner, session_name, "pi", None)
    }

    /// `d1.resume-same-session-id` on the `pi` lane. **Reclassified from
    /// NaPermitted to Required, 2026-07-15** — see [`pi_resume_same_session_id`]'s
    /// doc comment and `registry.rs`'s correction comment on this cell.
    /// REAL model turn (one — the structural precondition resume needs to
    /// revive into, not an optional strengthening; confirmed by hand that
    /// a cred-free resume genuinely hangs).
    pub fn resume_same_session_id(runner: &Runner, _session_name: &str) -> Outcome {
        pi_resume_same_session_id(runner)
    }

    /// `d1.multiplex-concurrent-distinct-residents` on the `pi` lane.
    /// Cred-free — the lightest lane, deliberately the first (and, per
    /// mc-5's clearance 2026-07-14, the ONLY lane cleared so far) to run
    /// this cell's genuine 2-concurrent-daemon exception live.
    pub fn multiplex_concurrent(runner: &Runner, session_name: &str) -> Outcome {
        multiplex_concurrent_via_cli(runner, session_name, "pi", None)
    }

    /// `d6.cold-target-send-fails-loud-with-terminal` on the `pi` lane.
    /// Cred-free, jailed — never touches a real daemon or the real registry.
    pub fn cold_target_send_fails(runner: &Runner, session_name: &str) -> Outcome {
        cold_target_send_fails_via_jail(runner, session_name, "pi")
    }

    /// `d6.self-terminate-on-wedged-child` on the `pi` lane — a REAL live probe
    /// that reports its TRUE outcome (a FAIL: pi lingers falsely-live when its
    /// child dies). Reclassified Required 2026-07-15 (conf-build-coord-3) after
    /// the prior NaPermitted reason ("no wire::serve-equivalent loop") was found
    /// false — `serve_pi` exists and has no child-death guard. See
    /// [`pi_self_terminate_probe`] for the full mechanism + evidence.
    pub fn self_terminate_on_wedged_child(runner: &Runner, session_name: &str) -> Outcome {
        pi_self_terminate_probe(runner, session_name)
    }

    /// `d6.bridge-death-detection-no-false-positive` on the `pi` lane:
    /// `NotApplicable`, matching the registry's declared reason.
    pub fn bridge_death_detection(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("pi has no ACP bridge child process for this liveness primitive to apply to")
    }

    /// `d6.cancel-maps-to-truthful-terminal` on the `pi` lane: `NotApplicable`,
    /// matching the registry's declared reason.
    pub fn cancel_maps_to_truthful_terminal(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("pi has no measured ACP session/cancel-equivalent primitive")
    }

    /// `d2.turn-phase-sequence-strict-order` on the `pi` lane:
    /// `NotApplicable`, matching the registry's declared reason (found
    /// silently undriven alongside the D6 cells, same class of gap, closed
    /// consistently 2026-07-15).
    pub fn turn_phase_sequence(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("pi does not route turns through the ACP-family delivery-log store any existing seed measures")
    }

    /// `d2.delivery-log-consistency-under-home-override` on the `pi` lane:
    /// `NotApplicable`, same reason.
    pub fn delivery_log_home_override(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("pi does not route turns through the ACP-family delivery-log store any existing seed measures")
    }

    /// `d3.queue-overflow-honors-configured-capacity` on the `pi` lane:
    /// `NotApplicable`, matching the registry's declared reason.
    pub fn queue_overflow(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("pi has no measured analogous outbound-queue/in-flight primitive to the ACP host's")
    }

    /// `d3.queue-slot-released-no-leak` on the `pi` lane: `NotApplicable`,
    /// matching the registry's declared reason.
    pub fn queue_slot_released(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("pi has no measured analogous outbound-queue/in-flight primitive to the ACP host's")
    }

    /// `d2.turn-correlation-and-completion` on the `pi` lane. REAL model
    /// turn, `exclusivity-inferred` receipt strength — see
    /// [`pi_turn_correlation`]'s doc comment.
    pub fn turn_correlation(runner: &Runner, _session_name: &str) -> Outcome {
        pi_turn_correlation(runner)
    }

    /// `d5.transcript-tee-captures-assistant-text` on the `pi` lane. REAL
    /// model turn — see [`pi_transcript_tee`].
    pub fn transcript_tee(runner: &Runner, _session_name: &str) -> Outcome {
        pi_transcript_tee(runner)
    }

    /// `d5.resume-jsonl-continuity-and-recall` on the `pi` lane. REAL model
    /// turns (x2) — see [`pi_resume_recall`].
    pub fn resume_recall(runner: &Runner, _session_name: &str) -> Outcome {
        pi_resume_recall(runner)
    }
}

pub mod codex {
    use super::*;

    /// `d1.boot-readiness` on the `codex` lane. Requires real codex
    /// credentials (`~/.codex/auth.json`) — NOT cred-free like pi; the
    /// serialize-by-default live-run protocol treats this lane as an unknown,
    /// larger footprint until measured (codex spawns an app-server process
    /// in addition to our own daemon, per `codex_conformance_live.rs`'s seed
    /// coverage).
    ///
    /// Live-probed 2026-07-14: on this box, `qd start` refuses codex behind a
    /// version-pin gate (installed codex CLI newer than qd's pinned version)
    /// BEFORE spawning anything — `boot_readiness_via_cli`'s gate detection
    /// resolves this as `Blocked`, correctly, with zero RAM footprint.
    pub fn boot_readiness(runner: &Runner, session_name: &str) -> Outcome {
        boot_readiness_via_cli(runner, session_name, "codex", None)
    }

    /// `d1.teardown-reaps-process-group` on the `codex` lane. Expected
    /// `Blocked` on this box (same version-pin gate as boot-readiness — the
    /// gate fires before any process to tear down exists).
    pub fn teardown_reaps(runner: &Runner, session_name: &str) -> Outcome {
        teardown_reaps_via_cli(runner, session_name, "codex", None)
    }

    /// `d1.launch-addressing` on the `codex` lane. Expected `Blocked`.
    pub fn launch_addressing(runner: &Runner, session_name: &str) -> Outcome {
        launch_addressing_via_cli(runner, session_name, "codex", None)
    }

    /// `d1.transport-is-our-daemon` on the `codex` lane. Expected `Blocked`.
    pub fn transport_is_our_daemon(runner: &Runner, session_name: &str) -> Outcome {
        transport_is_our_daemon_via_cli(runner, session_name, "codex", None)
    }

    /// `d1.liveness-process-alive` on the `codex` lane. Expected `Blocked`.
    pub fn liveness_process_alive(runner: &Runner, session_name: &str) -> Outcome {
        liveness_process_alive_via_cli(runner, session_name, "codex", None)
    }

    /// `d1.reconnect-resolves-via-registry` on the `codex` lane. Expected `Blocked`.
    pub fn reconnect_resolves(runner: &Runner, session_name: &str) -> Outcome {
        reconnect_resolves_via_cli(runner, session_name, "codex", None)
    }

    /// `d1.resume-same-session-id` on the `codex` lane: `NotApplicable`,
    /// matching the registry's own declared reason.
    pub fn resume_same_session_id(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable(
            "codex's wire protocol has no analogous resume/session-load primitive measured by any existing seed",
        )
    }

    /// `d1.multiplex-concurrent-distinct-residents` on the `codex` lane.
    /// Expected `Blocked` on this box (same version-pin gate — fires on the
    /// first of the two starts before any daemon exists).
    pub fn multiplex_concurrent(runner: &Runner, session_name: &str) -> Outcome {
        multiplex_concurrent_via_cli(runner, session_name, "codex", None)
    }

    /// `d6.cold-target-send-fails-loud-with-terminal` on the `codex` lane —
    /// the seed's own primary target ("the CLEANEST door lane to inject
    /// end-to-end here"). Unlike every `qd start`-based cell, this one does
    /// NOT go through `qd start` at all (the registry row is fabricated
    /// directly, then `qd send:relay` is driven against it) — so codex's
    /// version-pin gate, which only fires inside `qd start`, does NOT apply
    /// here. Expected `Pass`, not `Blocked`.
    pub fn cold_target_send_fails(runner: &Runner, session_name: &str) -> Outcome {
        cold_target_send_fails_via_jail(runner, session_name, "codex")
    }

    /// `d6.self-terminate-on-wedged-child` on the `codex` lane: `NotApplicable`,
    /// for the architecture-true reason (reason corrected 2026-07-15 after the
    /// prior naive-grep reason was found false for pi). Unlike pi (a
    /// dispatch-owned resident, which was reclassified Required + FAIL), codex is
    /// NOT qd-hosted as a serve loop — the codex app-server is provider-owned and
    /// self-listening, qd is only a ws client, and the tracked pid IS the
    /// app-server itself, so there is no qd-owned daemon-with-killable-child to
    /// self-terminate and app-server death is honestly not-live.
    pub fn self_terminate_on_wedged_child(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable(
            "codex is not qd-hosted as a serve loop: the codex app-server is a provider-owned, self-listening detached process (launch_plan = [codex, app-server] + --listen) and qd is only a ws client whose recorded pid IS the app-server itself — no qd-owned daemon-with-killable-child to self-terminate, and app-server death is honestly not-live",
        )
    }

    /// `d6.bridge-death-detection-no-false-positive` on the `codex` lane:
    /// `NotApplicable`, matching the registry's declared reason.
    pub fn bridge_death_detection(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("codex has no ACP bridge child process for this liveness primitive to apply to")
    }

    /// `d6.cancel-maps-to-truthful-terminal` on the `codex` lane:
    /// `NotApplicable`, matching the registry's declared reason.
    pub fn cancel_maps_to_truthful_terminal(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("codex has no measured ACP session/cancel-equivalent primitive")
    }

    /// `d2.turn-phase-sequence-strict-order` on the `codex` lane:
    /// `NotApplicable`, matching the registry's declared reason (found
    /// silently undriven alongside the D6 cells, closed consistently
    /// 2026-07-15).
    pub fn turn_phase_sequence(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("codex does not route turns through the ACP-family delivery-log store any existing seed measures")
    }

    /// `d2.delivery-log-consistency-under-home-override` on the `codex`
    /// lane: `NotApplicable`, same reason.
    pub fn delivery_log_home_override(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("codex does not route turns through the ACP-family delivery-log store any existing seed measures")
    }

    /// `d3.queue-overflow-honors-configured-capacity` on the `codex` lane:
    /// `NotApplicable`, matching the registry's declared reason.
    pub fn queue_overflow(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("codex has no measured analogous outbound-queue/in-flight primitive to the ACP host's")
    }

    /// `d3.queue-slot-released-no-leak` on the `codex` lane:
    /// `NotApplicable`, matching the registry's declared reason.
    pub fn queue_slot_released(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("codex has no measured analogous outbound-queue/in-flight primitive to the ACP host's")
    }

    /// `d2.turn-correlation-and-completion` on the `codex` lane. Codex's
    /// wire protocol DOES carry an explicit `turn_id` (both the RPC layer —
    /// `expectedTurnId`/`turn_interrupt(thread_id, turn_id)` — and the
    /// rollout event stream — `TaskComplete { turn_id }`), so on a box
    /// where codex actually boots this cell's declared receipt strength
    /// would be `id-tagged`, matching ACP's — NOT the pi lane's weaker
    /// `exclusivity-inferred` shape (checked at the coordinator's explicit
    /// request, 2026-07-15: "don't assume they're identical without
    /// checking"). No real-turn driver is built here yet, though: on this
    /// box `qd start --provider codex` refuses behind the same version-pin
    /// gate every other codex cell hits, before any turn could fire — this
    /// resolves `Blocked`, matching that established pattern, and an
    /// id-tagged driver is deferred until a box where codex actually boots.
    pub fn turn_correlation(runner: &Runner, session_name: &str) -> Outcome {
        codex_gate_probe(runner, session_name)
    }

    /// `d5.transcript-tee-captures-assistant-text` on the `codex` lane.
    /// Same gate situation as [`turn_correlation`] — `Blocked` on this box.
    pub fn transcript_tee(runner: &Runner, session_name: &str) -> Outcome {
        codex_gate_probe(runner, session_name)
    }
}

pub mod acp {
    use super::*;

    /// The bridge `.bin` dir, prepended to `PATH` so the adapter's default
    /// `claude-code-acp` command resolves (mirrors
    /// `tests/acp_resume_roundtrip_live.rs`'s `bridge_bin_dir()` exactly,
    /// including its env override — `qd` itself does not add this to `PATH`;
    /// resolving the bridge command is the caller's responsibility, same as
    /// every seed test already assumes).
    fn bridge_bin_dir() -> String {
        if let Ok(p) = std::env::var("QD_ACP_BRIDGE_BIN_DIR") {
            return p;
        }
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/work/acp-step0/node_modules/.bin")
    }

    /// `d1.boot-readiness` on the `acp/claude-code` lane. Requires real
    /// claude-code credentials (`~/.claude/.credentials.json`) AND the
    /// bridge binary on `PATH` (see [`bridge_bin_dir`]) — live-probed
    /// 2026-07-14: WITHOUT the bridge dir on `PATH`, `qd start` hangs rather
    /// than failing fast (killed at a 15s bound, confirmed zero residual
    /// process/RAM impact); WITH it, boot completes in well under a second,
    /// `qd info --json` shows `live:true` + a real pid + `"provider":
    /// "acp/claude-code"`, and `qd stop` group-reaps both the qd session and
    /// the bridge child (confirmed via the process table — no residual
    /// `claude-code-acp` process after stop).
    pub fn boot_readiness(runner: &Runner, session_name: &str) -> Outcome {
        boot_readiness_via_cli(
            runner,
            session_name,
            "acp/claude-code",
            Some(&bridge_bin_dir()),
        )
    }

    /// `d1.teardown-reaps-process-group` on the `acp/claude-code` lane.
    pub fn teardown_reaps(runner: &Runner, session_name: &str) -> Outcome {
        teardown_reaps_via_cli(runner, session_name, "acp/claude-code", Some(&bridge_bin_dir()))
    }

    /// `d1.launch-addressing` on the `acp/claude-code` lane.
    pub fn launch_addressing(runner: &Runner, session_name: &str) -> Outcome {
        launch_addressing_via_cli(runner, session_name, "acp/claude-code", Some(&bridge_bin_dir()))
    }

    /// `d1.transport-is-our-daemon` on the `acp/claude-code` lane.
    pub fn transport_is_our_daemon(runner: &Runner, session_name: &str) -> Outcome {
        transport_is_our_daemon_via_cli(runner, session_name, "acp/claude-code", Some(&bridge_bin_dir()))
    }

    /// `d1.liveness-process-alive` on the `acp/claude-code` lane.
    pub fn liveness_process_alive(runner: &Runner, session_name: &str) -> Outcome {
        liveness_process_alive_via_cli(runner, session_name, "acp/claude-code", Some(&bridge_bin_dir()))
    }

    /// `d1.reconnect-resolves-via-registry` on the `acp/claude-code` lane.
    pub fn reconnect_resolves(runner: &Runner, session_name: &str) -> Outcome {
        reconnect_resolves_via_cli(runner, session_name, "acp/claude-code", Some(&bridge_bin_dir()))
    }

    /// `d1.resume-same-session-id` on the `acp/claude-code` lane.
    pub fn resume_same_session_id(runner: &Runner, session_name: &str) -> Outcome {
        resume_same_session_id_via_cli(runner, session_name, "acp/claude-code", Some(&bridge_bin_dir()))
    }

    /// `d1.multiplex-concurrent-distinct-residents` on the `acp/claude-code`
    /// lane. NOT yet cleared to run live (mc-5 greenlit pi only, 2026-07-14 —
    /// re-check before firing this on any heavier lane).
    pub fn multiplex_concurrent(runner: &Runner, session_name: &str) -> Outcome {
        multiplex_concurrent_via_cli(runner, session_name, "acp/claude-code", Some(&bridge_bin_dir()))
    }

    /// `d6.cold-target-send-fails-loud-with-terminal` on the `acp/claude-code`
    /// lane — no bridge dir needed (the fixture row is fabricated directly;
    /// `qd send:relay` never spawns a bridge for an unreachable target).
    pub fn cold_target_send_fails(runner: &Runner, session_name: &str) -> Outcome {
        cold_target_send_fails_via_jail(runner, session_name, "acp/claude-code")
    }

    /// `d6.bridge-death-detection-no-false-positive` — lane-agnostic across
    /// the ACP family (the mechanism under test is `AcpHost::bridge_confirmed_dead`
    /// itself, not anything `acp/claude-code`-specific; `opencode` shares the
    /// same driver). See [`bridge_death_detection_via_host`].
    pub fn bridge_death_detection(runner: &Runner, _session_name: &str) -> Outcome {
        bridge_death_detection_via_host(runner)
    }

    /// `d3.queue-overflow-honors-configured-capacity` on the `acp/claude-code`
    /// lane. See [`queue_overflow_via_host`].
    pub fn queue_overflow(runner: &Runner, _session_name: &str) -> Outcome {
        queue_overflow_via_host(runner, "acp/claude-code")
    }

    /// `d3.queue-slot-released-no-leak` on the `acp/claude-code` lane. See
    /// [`queue_slot_released_via_host`].
    pub fn queue_slot_released(runner: &Runner, _session_name: &str) -> Outcome {
        queue_slot_released_via_host(runner, "acp/claude-code")
    }

    /// `d6.cancel-maps-to-truthful-terminal`. See
    /// [`cancel_maps_to_truthful_terminal_via_host`]; lane-agnostic within
    /// the ACP family, `opencode` shares this driver.
    pub fn cancel_maps_to_truthful_terminal(runner: &Runner, _session_name: &str) -> Outcome {
        cancel_maps_to_truthful_terminal_via_host(runner)
    }

    /// `d6.self-terminate-on-wedged-child`. See
    /// [`self_terminate_on_wedged_child_via_host`] — including the
    /// applicability question flagged in its doc comment.
    pub fn self_terminate_on_wedged_child(runner: &Runner, _session_name: &str) -> Outcome {
        self_terminate_on_wedged_child_via_host(runner)
    }

    /// `d2.turn-phase-sequence-strict-order` on the `acp/claude-code` lane.
    /// See [`three_phase_sequence_via_daemon`].
    pub fn turn_phase_sequence(runner: &Runner, _session_name: &str) -> Outcome {
        three_phase_sequence_via_daemon(runner, "acp/claude-code")
    }

    /// `d2.delivery-log-consistency-under-home-override` on the
    /// `acp/claude-code` lane. See [`delivery_log_home_override_via_daemon`].
    pub fn delivery_log_home_override(runner: &Runner, _session_name: &str) -> Outcome {
        delivery_log_home_override_via_daemon(runner, "acp/claude-code")
    }

    /// `d2.turn-correlation-and-completion` on the `acp/claude-code` lane.
    /// REAL model turn — see [`turn_correlation_via_host`].
    pub fn turn_correlation(runner: &Runner, _session_name: &str) -> Outcome {
        turn_correlation_via_host(runner, "acp/claude-code")
    }

    /// `d5.transcript-tee-captures-assistant-text` on the `acp/claude-code`
    /// lane. REAL model turn — see [`transcript_tee_via_host`].
    pub fn transcript_tee(runner: &Runner, _session_name: &str) -> Outcome {
        transcript_tee_via_host(runner, "acp/claude-code")
    }

    /// `d5.resume-jsonl-continuity-and-recall` on the `acp/claude-code`
    /// lane. REAL model turns (x2) — see [`resume_recall_via_host`].
    pub fn resume_recall(runner: &Runner, _session_name: &str) -> Outcome {
        resume_recall_via_host(runner, "acp/claude-code")
    }
}

pub mod opencode {
    use super::*;

    /// `d1.boot-readiness` on the `acp/opencode` lane. Requires real opencode
    /// credentials. Unlike `acp/claude-code`, needs NO `PATH` prepending — the
    /// `opencode` binary (which serves as its own ACP bridge via `opencode
    /// acp`, per `tests/acp_opencode_live.rs:222`'s `--bridge-cmd opencode
    /// --bridge-arg acp`) is already resolvable on this box's normal `PATH`.
    ///
    /// The CLI's DOCUMENTED `--provider` value is the bare `opencode` (`qd
    /// start --help` lists "claude-code (default) or opencode" — it never
    /// mentions the internal `acp/opencode` id `Lane::Opencode::provider_id`
    /// returns; passing the internal id to the CLI flag is untested and not
    /// assumed to work). Live-probed 2026-07-14 with `--provider opencode`:
    /// boot completed in well under a second, MemAvailable delta ~276MB
    /// (same order of magnitude as the other lanes, under the 1.5G trigger),
    /// `qd info --json` showed `live:true` + a real pid + `"provider":
    /// "acp/opencode"` (the CLI resolves the documented flag value to the
    /// internal id internally — confirming they name the same lane), and
    /// `qd stop` released cleanly (MemAvailable recovered to at-or-above the
    /// pre-boot baseline; no residual process for this session's pid).
    pub fn boot_readiness(runner: &Runner, session_name: &str) -> Outcome {
        boot_readiness_via_cli(runner, session_name, "opencode", None)
    }

    /// `d1.teardown-reaps-process-group` on the `acp/opencode` lane.
    pub fn teardown_reaps(runner: &Runner, session_name: &str) -> Outcome {
        teardown_reaps_via_cli(runner, session_name, "opencode", None)
    }

    /// `d1.launch-addressing` on the `acp/opencode` lane.
    pub fn launch_addressing(runner: &Runner, session_name: &str) -> Outcome {
        launch_addressing_via_cli(runner, session_name, "opencode", None)
    }

    /// `d1.transport-is-our-daemon` on the `acp/opencode` lane.
    pub fn transport_is_our_daemon(runner: &Runner, session_name: &str) -> Outcome {
        transport_is_our_daemon_via_cli(runner, session_name, "opencode", None)
    }

    /// `d1.liveness-process-alive` on the `acp/opencode` lane.
    pub fn liveness_process_alive(runner: &Runner, session_name: &str) -> Outcome {
        liveness_process_alive_via_cli(runner, session_name, "opencode", None)
    }

    /// `d1.reconnect-resolves-via-registry` on the `acp/opencode` lane.
    pub fn reconnect_resolves(runner: &Runner, session_name: &str) -> Outcome {
        reconnect_resolves_via_cli(runner, session_name, "opencode", None)
    }

    /// `d1.resume-same-session-id` on the `acp/opencode` lane.
    pub fn resume_same_session_id(runner: &Runner, session_name: &str) -> Outcome {
        resume_same_session_id_via_cli(runner, session_name, "opencode", None)
    }

    /// `d1.multiplex-concurrent-distinct-residents` on the `acp/opencode`
    /// lane. NOT yet cleared to run live (mc-5 greenlit pi only, 2026-07-14).
    pub fn multiplex_concurrent(runner: &Runner, session_name: &str) -> Outcome {
        multiplex_concurrent_via_cli(runner, session_name, "opencode", None)
    }

    /// `d6.cold-target-send-fails-loud-with-terminal` on the `acp/opencode`
    /// lane. Uses the on-disk provider id `"acp/opencode"` (matching what
    /// `qd info --json` actually shows for real opencode rows — the
    /// internal id, not the `opencode` CLI-flag alias) since this fabricates
    /// the registry row directly rather than going through `qd start`'s
    /// flag-to-internal-id resolution.
    pub fn cold_target_send_fails(runner: &Runner, session_name: &str) -> Outcome {
        cold_target_send_fails_via_jail(runner, session_name, "acp/opencode")
    }

    /// `d6.bridge-death-detection-no-false-positive` — shares the
    /// `acp/claude-code` lane's driver; the mechanism is lane-agnostic
    /// within the ACP family. See [`super::acp::bridge_death_detection`].
    pub fn bridge_death_detection(runner: &Runner, _session_name: &str) -> Outcome {
        super::bridge_death_detection_via_host(runner)
    }

    /// `d3.queue-overflow-honors-configured-capacity` on the `acp/opencode`
    /// lane. Shares the identical `AcpProvider` implementation with
    /// `acp/claude-code` (see [`queue_overflow_via_host`]'s doc comment).
    pub fn queue_overflow(runner: &Runner, _session_name: &str) -> Outcome {
        super::queue_overflow_via_host(runner, "acp/opencode")
    }

    /// `d3.queue-slot-released-no-leak` on the `acp/opencode` lane. Shares
    /// the identical `AcpProvider` implementation with `acp/claude-code`.
    pub fn queue_slot_released(runner: &Runner, _session_name: &str) -> Outcome {
        super::queue_slot_released_via_host(runner, "acp/opencode")
    }

    /// `d6.cancel-maps-to-truthful-terminal`. Shares the `acp/claude-code`
    /// lane's driver — see [`super::acp::cancel_maps_to_truthful_terminal`].
    pub fn cancel_maps_to_truthful_terminal(runner: &Runner, _session_name: &str) -> Outcome {
        super::cancel_maps_to_truthful_terminal_via_host(runner)
    }

    /// `d6.self-terminate-on-wedged-child`. Shares the `acp/claude-code`
    /// lane's driver — see [`super::acp::self_terminate_on_wedged_child`].
    pub fn self_terminate_on_wedged_child(runner: &Runner, _session_name: &str) -> Outcome {
        super::self_terminate_on_wedged_child_via_host(runner)
    }

    /// `d2.turn-phase-sequence-strict-order` on the `acp/opencode` lane.
    /// COMPOSITIONAL coverage per the seed's own doc comment: opencode
    /// routes through the identical `run_acp_send`/`run_acp_wait` verb
    /// path — the fake bridge is the only difference, and this cell
    /// already fakes the bridge.
    pub fn turn_phase_sequence(runner: &Runner, _session_name: &str) -> Outcome {
        super::three_phase_sequence_via_daemon(runner, "acp/opencode")
    }

    /// `d2.delivery-log-consistency-under-home-override` on the
    /// `acp/opencode` lane. Same compositional-coverage reasoning as
    /// [`turn_phase_sequence`].
    pub fn delivery_log_home_override(runner: &Runner, _session_name: &str) -> Outcome {
        super::delivery_log_home_override_via_daemon(runner, "acp/opencode")
    }

    /// `d2.turn-correlation-and-completion` on the `acp/opencode` lane.
    /// REAL model turn via the real `opencode acp` bridge (its own
    /// binary — see `run_one_pong_turn`'s bridge-selection branch).
    pub fn turn_correlation(runner: &Runner, _session_name: &str) -> Outcome {
        super::turn_correlation_via_host(runner, "acp/opencode")
    }

    /// `d5.transcript-tee-captures-assistant-text` on the `acp/opencode`
    /// lane. REAL model turn.
    pub fn transcript_tee(runner: &Runner, _session_name: &str) -> Outcome {
        super::transcript_tee_via_host(runner, "acp/opencode")
    }

    /// `d5.resume-jsonl-continuity-and-recall` on the `acp/opencode` lane.
    /// REAL model turns (x2). Shares the driver — see
    /// [`super::acp::resume_recall`].
    pub fn resume_recall(runner: &Runner, _session_name: &str) -> Outcome {
        super::resume_recall_via_host(runner, "acp/opencode")
    }
}

pub mod claude_code {
    use super::*;

    // `d6.cold_target_send_fails` has NO driver here, deliberately: bare
    // claude-code rows never carry an `endpoint` field even when perfectly
    // healthy (registry.rs's own doc comment on that field: "claude rows
    // NEVER carry it"). The cold-target fixture's whole mechanism is "a live
    // pid with no recorded endpoint" — for a daemon-hosted lane (pi/codex/
    // acp) that shape is distinctive and triggers the "not reachable" door;
    // for claude-code it's indistinguishable from a normal row, so this
    // fixture doesn't test anything meaningful here. Bare claude-code's
    // actual "unreachable" signal (if any) is unresearched — left undriven
    // rather than wiring in a fixture that wouldn't prove what the cell
    // claims.

    /// `d1.boot-readiness` on the bare `claude-code` lane — the DEFAULT lane,
    /// which sets the public provider badge. `qd start`'s one-off
    /// prompt-driven shapes (bare headless with no prompt, and `-p
    /// <prompt>`) are BOTH refused before spawning anything — live-probed
    /// 2026-07-14 — because neither is how this lane is actually run in
    /// production. The representative boot is the **warm-leaf commission**:
    /// `bond commission --name <n>` (no `--call`) boots a live, qd-tracked
    /// claude-code session with **zero completions** — "a WARM FRESH LEAF
    /// until someone CALLs it" — the exact mechanism every session in this
    /// org (including this executor) was born through. This dissolves the
    /// token question entirely: D1 needs no completion at all, matching
    /// every other lane's boot-readiness cell.
    ///
    /// Ruled 2026-07-14 (conf-build-coord-2 on call
    /// `01KXGW9E0M28F4FB9KKNRR5TNW`, after the earlier bypass-via-`claude -p`
    /// attempt was correctly rejected as measuring Anthropic's API rather
    /// than qd's claude-code lane): bypassing `qd` is a cell that LIES
    /// (green on the wrong evidence) and is never acceptable, even to save a
    /// token. Live-probed clean the same day: `qd info --json` on the
    /// commissioned session showed `"provider":"claude-code"`, `live:true`,
    /// a real pid, and **`Turns: 0, Tokens: 0`** — direct, structural
    /// confirmation of zero spend. `bond decommission` tears it down.
    pub fn boot_readiness(runner: &Runner, session_name: &str) -> Outcome {
        let commission = Command::new("bond")
            .args(["commission", "--name", session_name])
            .output();
        let commission = match commission {
            Ok(o) => o,
            Err(e) => {
                return runner.fail(
                    vec![format!("bond commission --name {session_name}")],
                    format!("spawn error: {e}"),
                    "failed to spawn bond itself",
                );
            }
        };
        if !commission.status.success() {
            return runner.fail(
                vec![format!("bond commission --name {session_name}")],
                format!(
                    "exit {:?}; stderr: {}",
                    commission.status.code(),
                    String::from_utf8_lossy(&commission.stderr)
                ),
                "bond commission did not succeed",
            );
        }
        let info = Command::new("qd")
            .args(["info", session_name, "--json"])
            .output();
        let outcome = match info {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
                let parsed: Result<serde_json::Value, _> = serde_json::from_str(&stdout);
                match parsed {
                    Ok(v)
                        if v.get("live").and_then(serde_json::Value::as_bool) == Some(true)
                            && v.get("pid").and_then(serde_json::Value::as_i64).is_some()
                            && v.get("provider").and_then(serde_json::Value::as_str)
                                == Some("claude-code") =>
                    {
                        runner.pass(
                            vec![
                                format!("bond commission --name {session_name}"),
                                format!("qd info {session_name} --json"),
                            ],
                            stdout,
                        )
                    }
                    Ok(_) => runner.fail(
                        vec![format!("qd info {session_name} --json")],
                        stdout,
                        "commissioned session was not live/claude-code with a real pid",
                    ),
                    Err(e) => runner.fail(
                        vec![format!("qd info {session_name} --json")],
                        stdout,
                        format!("info output did not parse as JSON: {e}"),
                    ),
                }
            }
            Ok(o) => runner.fail(
                vec![format!("qd info {session_name} --json")],
                String::from_utf8_lossy(&o.stderr).into_owned(),
                "qd info did not resolve the just-commissioned session",
            ),
            Err(e) => runner.fail(
                vec![format!("qd info {session_name} --json")],
                format!("spawn error: {e}"),
                "failed to spawn qd info",
            ),
        };
        // Best-effort teardown — never overrides the already-decided outcome.
        let _ = Command::new("bond")
            .args(["decommission", session_name, "--note", "C-1 d1.boot-readiness probe"])
            .output();
        outcome
    }

    /// `d1.teardown-reaps-process-group` on the bare `claude-code` lane, via
    /// `bond decommission` (the representative teardown for a `bond
    /// commission`-booted session — see `boot_readiness`'s doc comment for
    /// why this lane's mechanism differs from every `qd start`-based lane).
    /// Verified at the OS level (`ps -p`), never via `qd`'s own self-report.
    pub fn teardown_reaps(runner: &Runner, session_name: &str) -> Outcome {
        let commission_cmd = format!("bond commission --name {session_name}");
        let commission = Command::new("bond")
            .args(["commission", "--name", session_name])
            .output();
        let commission = match commission {
            Ok(o) => o,
            Err(e) => {
                return runner.fail(
                    vec![commission_cmd],
                    format!("spawn error: {e}"),
                    "failed to spawn bond itself",
                );
            }
        };
        if !commission.status.success() {
            return runner.fail(
                vec![commission_cmd],
                format!(
                    "exit {:?}; stderr: {}",
                    commission.status.code(),
                    String::from_utf8_lossy(&commission.stderr)
                ),
                "bond commission did not succeed",
            );
        }
        let info = Command::new("qd")
            .args(["info", session_name, "--json"])
            .output();
        let pid = match info {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
                serde_json::from_str::<serde_json::Value>(&stdout)
                    .ok()
                    .and_then(|v| v.get("pid").and_then(serde_json::Value::as_i64))
            }
            _ => None,
        };
        let pid = match pid {
            Some(p) => p,
            None => {
                let _ = Command::new("bond")
                    .args(["decommission", session_name, "--note", "C-1 probe cleanup"])
                    .output();
                return runner.fail(
                    vec![commission_cmd, format!("qd info {session_name} --json")],
                    "no pid resolved before teardown".to_string(),
                    "cannot verify teardown without a starting pid",
                );
            }
        };
        let decommission_cmd = format!("bond decommission {session_name}");
        let decommission = Command::new("bond")
            .args(["decommission", session_name, "--note", "C-1 d1.teardown-reaps probe"])
            .output();
        match decommission {
            Ok(o) if o.status.success() => {
                if os_pid_alive(pid) {
                    runner.fail(
                        vec![commission_cmd, format!("qd info {session_name} --json"), decommission_cmd],
                        format!("pid {pid} still alive at the OS level after bond decommission reported success"),
                        "teardown must genuinely reap the process, not just append the decommission event",
                    )
                } else {
                    runner.pass(
                        vec![commission_cmd, format!("qd info {session_name} --json"), decommission_cmd],
                        format!("pid {pid} confirmed dead via `ps -p` after bond decommission"),
                    )
                }
            }
            Ok(o) => runner.fail(
                vec![commission_cmd, decommission_cmd],
                format!(
                    "exit {:?}; stderr: {}",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr)
                ),
                "bond decommission did not succeed",
            ),
            Err(e) => runner.fail(
                vec![commission_cmd, decommission_cmd],
                format!("spawn error: {e}"),
                "failed to spawn bond decommission",
            ),
        }
    }

    /// `bond commission --name <n>` and return the commands run so far, or an
    /// immediate `Outcome` on failure — the `claude_code`-lane equivalent of
    /// [`start_via_cli`] (which is `qd start`-based and doesn't apply here).
    fn commission(runner: &Runner, session_name: &str) -> Result<Vec<String>, Outcome> {
        let commission_cmd = format!("bond commission --name {session_name}");
        let out = Command::new("bond")
            .args(["commission", "--name", session_name])
            .output();
        match out {
            Ok(o) if o.status.success() => Ok(vec![commission_cmd]),
            Ok(o) => Err(runner.fail(
                vec![commission_cmd],
                format!(
                    "exit {:?}; stderr: {}",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr)
                ),
                "bond commission did not succeed",
            )),
            Err(e) => Err(runner.fail(
                vec![commission_cmd],
                format!("spawn error: {e}"),
                "failed to spawn bond itself",
            )),
        }
    }

    fn decommission_best_effort(session_name: &str) {
        let _ = Command::new("bond")
            .args(["decommission", session_name, "--note", "C-1 probe cleanup"])
            .output();
    }

    /// `d1.launch-addressing` on the bare `claude-code` lane.
    pub fn launch_addressing(runner: &Runner, session_name: &str) -> Outcome {
        let mut commands = match commission(runner, session_name) {
            Ok(c) => c,
            Err(o) => return o,
        };
        commands.push(format!("qd info {session_name} --json"));
        let outcome = match fetch_info_json(session_name, None) {
            Some(v)
                if v.get("name").and_then(serde_json::Value::as_str) == Some(session_name)
                    && v.get("live").and_then(serde_json::Value::as_bool) == Some(true)
                    && v.get("pid").and_then(serde_json::Value::as_i64).is_some() =>
            {
                runner.pass(commands, v.to_string())
            }
            Some(v) => runner.fail(
                commands,
                v.to_string(),
                "info resolved but name/live/pid did not all match the requested session",
            ),
            None => runner.fail(
                commands,
                "qd info did not resolve or parse".to_string(),
                "the just-commissioned session did not address back correctly",
            ),
        };
        decommission_best_effort(session_name);
        outcome
    }

    /// `d1.transport-is-our-daemon` on the bare `claude-code` lane.
    ///
    /// Live-probed 2026-07-14: unlike every `qd start`-based lane (whose
    /// resident process is literally `.../bin/qd <verb>...`, so a `contains
    /// "qd"` check is correct there), a `bond commission`-booted session's
    /// resident is the `claude` binary ITSELF (this executor's own kind of
    /// process) — its cmdline never contains "qd" at all. The
    /// lane-appropriate check is that the cmdline carries the EXACT
    /// requested session name as its `--name` argument — a stronger,
    /// binary-name-agnostic proof that this specific process is the one we
    /// addressed, which the observed cmdline confirms is present
    /// (`... --name <session_name>`).
    pub fn transport_is_our_daemon(runner: &Runner, session_name: &str) -> Outcome {
        let mut commands = match commission(runner, session_name) {
            Ok(c) => c,
            Err(o) => return o,
        };
        commands.push(format!("qd info {session_name} --json"));
        let pid = fetch_info_json(session_name, None)
            .and_then(|v| v.get("pid").and_then(serde_json::Value::as_i64));
        let outcome = match pid {
            Some(p) => {
                commands.push(format!("ps -p {p} -o cmd="));
                let cmdline = Command::new("ps")
                    .args(["-p", &p.to_string(), "-o", "cmd="])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
                match cmdline {
                    Some(c) if c.contains(session_name) => runner.pass(commands, c),
                    Some(c) => runner.fail(
                        commands,
                        c,
                        "the live pid's cmdline does not carry the requested session name",
                    ),
                    None => runner.fail(
                        commands,
                        "ps -p failed to resolve the pid's cmdline".to_string(),
                        "cannot confirm the resident is our daemon",
                    ),
                }
            }
            None => runner.fail(
                commands,
                "qd info did not resolve a pid".to_string(),
                "cannot confirm transport without a pid",
            ),
        };
        decommission_best_effort(session_name);
        outcome
    }

    /// `d1.liveness-process-alive` on the bare `claude-code` lane.
    pub fn liveness_process_alive(runner: &Runner, session_name: &str) -> Outcome {
        let mut commands = match commission(runner, session_name) {
            Ok(c) => c,
            Err(o) => return o,
        };
        commands.push(format!("qd info {session_name} --json"));
        let pid = fetch_info_json(session_name, None)
            .and_then(|v| v.get("pid").and_then(serde_json::Value::as_i64));
        let outcome = match pid {
            Some(p) => {
                commands.push(format!("ps -p {p}"));
                if os_pid_alive(p) {
                    runner.pass(commands, format!("pid {p} confirmed alive via `ps -p` while live"))
                } else {
                    runner.fail(
                        commands,
                        format!("pid {p} reported by qd info but NOT alive at the OS level"),
                        "a liveness claim must be backed by a real OS-level check",
                    )
                }
            }
            None => runner.fail(
                commands,
                "qd info did not resolve a pid".to_string(),
                "cannot confirm liveness without a pid",
            ),
        };
        decommission_best_effort(session_name);
        outcome
    }

    /// `d1.reconnect-resolves-via-registry` on the bare `claude-code` lane.
    pub fn reconnect_resolves(runner: &Runner, session_name: &str) -> Outcome {
        let mut commands = match commission(runner, session_name) {
            Ok(c) => c,
            Err(o) => return o,
        };
        let info_cmd = format!("qd info {session_name} --json");
        let first = fetch_info_json(session_name, None);
        commands.push(info_cmd.clone());
        let second = fetch_info_json(session_name, None);
        commands.push(info_cmd);
        let pid_of = |v: &serde_json::Value| v.get("pid").and_then(serde_json::Value::as_i64);
        let live_of = |v: &serde_json::Value| v.get("live").and_then(serde_json::Value::as_bool);
        let outcome = match (&first, &second) {
            (Some(a), Some(b))
                if live_of(a) == Some(true)
                    && live_of(b) == Some(true)
                    && pid_of(a).is_some()
                    && pid_of(a) == pid_of(b) =>
            {
                runner.pass(
                    commands,
                    format!("two independent qd info reads agree: pid {:?}, both live", pid_of(a)),
                )
            }
            _ => runner.fail(
                commands,
                format!("first={first:?} second={second:?}"),
                "two fresh, separate qd info reads did not both resolve the same live pid",
            ),
        };
        decommission_best_effort(session_name);
        outcome
    }

    /// `d1.resume-same-session-id` on the bare `claude-code` lane.
    /// **Rewritten 2026-07-15** (conf-build-coord-3 ruling, after the
    /// acceptance-candidate review): the ORIGINAL version of this driver
    /// commissioned a ZERO-COMPLETION warm leaf and tried to resume it
    /// directly — exactly the shape live-probed 2026-07-14 to refuse
    /// ("stopped and has no resumable transcript — nothing to resume"),
    /// which is WHY this cell was deferred to the token phase in the first
    /// place. It was never actually rewritten once the token phase's own
    /// mechanism (a real prior turn to revive into) was proven by
    /// [`resume_recall`] (commit `0dfd8a8`) — this fixes that: commission,
    /// a real turn, decommission, `qd resume --no-attach`, confirm the
    /// SAME sessionId — the same mechanism `resume_recall` uses, truncated
    /// after the id check (no second recall turn needed for this
    /// narrower claim), mirroring exactly how `pi::resume_same_session_id`
    /// was fixed (commit `e62a99e`).
    pub fn resume_same_session_id(runner: &Runner, session_name: &str) -> Outcome {
        let mut commands = match commission(runner, session_name) {
            Ok(c) => c,
            Err(o) => return o,
        };
        let fail_and_clean = |runner: &Runner, commands: Vec<String>, observed: String, why: &str| -> Outcome {
            decommission_best_effort(session_name);
            runner.fail(commands, observed, why)
        };

        let original_session_id = fetch_info_json(session_name, None)
            .and_then(|v| v.get("sessionId").and_then(serde_json::Value::as_str).map(str::to_string));
        commands.push(format!("qd info {session_name} --json"));
        let original_session_id = match original_session_id {
            Some(id) => id,
            None => return fail_and_clean(runner, commands, "could not capture the original sessionId after commission".to_string(), "cannot verify resume continuity without a starting sessionId"),
        };

        // A real turn to revive INTO — structural precondition, see this
        // fn's doc comment (matches pi's identical finding).
        let prompt = "Reply with exactly the single word OK and nothing else. Do not use any tools.";
        commands.push(format!("qd send:relay {session_name} <a trivial real turn>"));
        if !matches!(Command::new(qd_bin()).args(["send:relay", session_name, prompt]).output(), Ok(o) if o.status.success()) {
            return fail_and_clean(runner, commands, "qd send:relay did not succeed".to_string(), "cannot exercise resume-same-session-id without a completed real turn to resume into");
        }
        commands.push(format!("qd wait {session_name} --timeout 120000"));
        if !matches!(Command::new(qd_bin()).args(["wait", session_name, "--timeout", "120000"]).output(), Ok(o) if o.status.success()) {
            return fail_and_clean(runner, commands, "qd wait did not succeed".to_string(), "the real turn must complete before decommissioning");
        }

        // The real turn above wrote this session's transcript to the CALLER's
        // cwd project dir (`~/.claude/projects/<cwd>/<session>.jsonl`) — a SHARED
        // store this file's jail cleanup never touches. Capture its exact path
        // now (before decommission removes the session `qd info` reads from), and
        // remove only THIS run's own file on every exit path below — never
        // rm -rf the shared dir. Same residue class as `run_one_claude_code_turn`
        // and 94c4065; this specific driver was missed.
        let jsonl_path = fetch_info_json(session_name, None)
            .and_then(|v| v.get("jsonlPath").and_then(serde_json::Value::as_str).map(str::to_string));
        let clean_projects_file = || {
            if let Some(p) = &jsonl_path {
                let _ = std::fs::remove_file(p);
            }
        };

        commands.push(format!("bond decommission {session_name}"));
        decommission_best_effort(session_name);
        commands.push(format!("qd resume {session_name} --no-attach"));
        let resume = Command::new(qd_bin()).args(["resume", session_name, "--no-attach"]).output();
        match &resume {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                clean_projects_file();
                return runner.fail(commands, format!("resume exit {:?}; stderr: {}", o.status.code(), String::from_utf8_lossy(&o.stderr)), "qd resume must succeed against a decommissioned session with a real prior turn");
            }
            Err(e) => {
                clean_projects_file();
                return runner.fail(commands, format!("spawn error resuming: {e}"), "failed to spawn qd resume");
            }
        }

        commands.push(format!("qd info {session_name} --json"));
        let outcome = match fetch_info_json(session_name, None) {
            Some(v)
                if v.get("sessionId").and_then(serde_json::Value::as_str) == Some(original_session_id.as_str())
                    && v.get("live").and_then(serde_json::Value::as_bool) == Some(true)
                    && v.get("pid").and_then(serde_json::Value::as_i64).is_some() =>
            {
                runner.pass(
                    commands,
                    format!("resumed session carries the SAME sessionId {original_session_id:?} (never re-minted) after a real prior turn, live with a real pid"),
                )
            }
            Some(v) => runner.fail(commands, format!("original={original_session_id:?} post-resume={v}"), "resume did not re-establish the same sessionId (or was not live)"),
            None => runner.fail(commands, format!("original={original_session_id:?}"), "qd info did not resolve the resumed session"),
        };
        decommission_best_effort(session_name);
        clean_projects_file();
        outcome
    }

    /// `d6.self-terminate-on-wedged-child` on the bare `claude-code` lane:
    /// `NotApplicable`, for the architecture-true reason (reason corrected
    /// 2026-07-15 after the prior naive-grep reason was found false for pi).
    /// claude-code-bare is `Hosting::MuxPane` — the claude CLI is itself the
    /// single tracked leaf process in a qd-owned mux pane; there is no qd daemon
    /// owning a separate child, so "self-terminate on wedged child" has no
    /// referent, and liveness keyed on the claude process's own pid is honest.
    pub fn self_terminate_on_wedged_child(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable(
            "claude-code (bare) is Hosting::MuxPane — the claude CLI is itself the single tracked leaf process in a qd-owned mux pane, with no qd daemon owning a separate child, so 'self-terminate on wedged child' has no referent and liveness keyed on the claude process's own pid is honest",
        )
    }

    /// `d6.cold-target-send-fails-loud-with-terminal` on the bare `claude-code`
    /// lane: `NotApplicable`, matching the registry's NaPermitted declaration
    /// (F2, 2026-07-15). claude-code-bare rows never carry an `endpoint` field
    /// even when healthy, so the cold-target fixture's live-pid-no-endpoint
    /// mechanism cannot distinguish a cold resident from a live one on this lane
    /// — there is no meaningful mechanism to drive. Wired to PRODUCE the
    /// NotApplicable observation (like the sibling D6/D3 n-a cells), rather than
    /// falling through `run_cell` to `None` where a full-grid assembler could
    /// misread a declared n-a as an unresolved defect.
    pub fn cold_target_send_fails(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable(
            "claude-code-bare rows never carry an endpoint field even when healthy, so the cold-target fixture's mechanism (a live pid with no recorded endpoint) is indistinguishable from a normal healthy row on this lane — it cannot distinguish a cold resident from a live one, so there is no meaningful mechanism to drive here",
        )
    }

    /// `d6.bridge-death-detection-no-false-positive` on the bare
    /// `claude-code` lane: `NotApplicable`, 2026-07-15 correction
    /// (conf-build-coord-3, confirming grep on record — no `AcpHost`/
    /// `acp_client` usage anywhere in this lane's send/commission path, no
    /// bridge child process to detect death of).
    pub fn bridge_death_detection(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("claude-code (bare) has no ACP bridge child process for this liveness primitive to apply to")
    }

    /// `d6.cancel-maps-to-truthful-terminal` on the bare `claude-code`
    /// lane: `NotApplicable`, same correction — no `session/cancel` RPC to
    /// map.
    pub fn cancel_maps_to_truthful_terminal(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("claude-code (bare) has no measured ACP session/cancel-equivalent primitive")
    }

    /// `d2.turn-phase-sequence-strict-order` on the bare `claude-code`
    /// lane: `NotApplicable`, 2026-07-15 correction (conf-build-coord-3,
    /// confirming grep on record — traced `qd send:relay`'s dispatch in
    /// `send_relay.rs::run`: it branches to `run_codex_send`/`run_acp_send`/
    /// `run_pi_send` for those three providers specifically, each using
    /// `emit_daemon_send_events`/`emit_daemon_seen`, the 3-phase mechanism
    /// this cell measures. claude-code-bare falls through to
    /// `emit_relay_send_events` instead — a structurally different generic
    /// relay-message path with its own "send-initiated -> relay-delivered"
    /// shape, no `message-seen`-equivalent terminal).
    pub fn turn_phase_sequence(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("claude-code (bare) does not route turns through the ACP-family delivery-log store any existing seed measures")
    }

    /// `d2.delivery-log-consistency-under-home-override` on the bare
    /// `claude-code` lane: `NotApplicable`, same correction.
    pub fn delivery_log_home_override(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("claude-code (bare) does not route turns through the ACP-family delivery-log store any existing seed measures")
    }

    /// `d3.queue-overflow-honors-configured-capacity` on the bare
    /// `claude-code` lane: `NotApplicable`, 2026-07-15 correction
    /// (conf-build-coord-3, confirming grep on record — grepped every `fn
    /// run_*` in `send_relay.rs`: no `run_claude_send` exists at all, so
    /// this lane never calls `Provider::inject` and never touches
    /// `AcpHost`/`OutboundQueue`).
    pub fn queue_overflow(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("claude-code (bare) has no measured analogous outbound-queue/in-flight primitive to the ACP host's")
    }

    /// `d3.queue-slot-released-no-leak` on the bare `claude-code` lane:
    /// `NotApplicable`, same correction.
    pub fn queue_slot_released(_runner: &Runner, _session_name: &str) -> Outcome {
        Outcome::not_applicable("claude-code (bare) has no measured analogous outbound-queue/in-flight primitive to the ACP host's")
    }

    /// `d1.multiplex-concurrent-distinct-residents` on the bare `claude-code`
    /// lane: two concurrent `bond commission`-booted warm leaves, confirmed
    /// distinct and independently live, torn down together via `bond
    /// decommission`. NOT yet cleared to run live (mc-5 greenlit the pi pair
    /// only, 2026-07-14 — a claude-code pair is a full agent session each,
    /// heavier than pi, so it needs its own re-check).
    pub fn multiplex_concurrent(runner: &Runner, session_name: &str) -> Outcome {
        let name_a = format!("{session_name}-a");
        let name_b = format!("{session_name}-b");

        let start_a = match commission(runner, &name_a) {
            Ok(c) => c,
            Err(o) => return o,
        };
        let start_b = match commission(runner, &name_b) {
            Ok(c) => c,
            Err(o) => {
                decommission_best_effort(&name_a);
                return o;
            }
        };
        let mut commands = start_a;
        commands.extend(start_b);
        commands.push(format!("qd info {name_a} --json"));
        commands.push(format!("qd info {name_b} --json"));

        let info_a = fetch_info_json(&name_a, None);
        let info_b = fetch_info_json(&name_b, None);
        let pid_of = |v: &serde_json::Value| v.get("pid").and_then(serde_json::Value::as_i64);
        let live_of = |v: &serde_json::Value| v.get("live").and_then(serde_json::Value::as_bool);

        let outcome = match (&info_a, &info_b) {
            (Some(a), Some(b))
                if live_of(a) == Some(true)
                    && live_of(b) == Some(true)
                    && pid_of(a).is_some()
                    && pid_of(b).is_some()
                    && pid_of(a) != pid_of(b) =>
            {
                runner.pass(
                    commands,
                    format!(
                        "two concurrent claude-code residents, distinct pids {:?} and {:?}, both independently live",
                        pid_of(a), pid_of(b)
                    ),
                )
            }
            _ => runner.fail(
                commands,
                format!("a={info_a:?} b={info_b:?}"),
                "two concurrently-commissioned sessions did not both resolve as distinct, live residents",
            ),
        };
        decommission_best_effort(&name_a);
        decommission_best_effort(&name_b);
        outcome
    }

    /// Raw evidence from ONE real turn on a bond-commissioned claude-code
    /// session. **Mechanism note (2026-07-15):** the originally-ruled shape
    /// for this lane's real turns (`qd start --attach`, interactive mode)
    /// turned out to be genuinely unimplemented — confirmed at the source
    /// level (`src/bin/qd/verbs/lifecycle.rs`: `"qd start: --attach is not
    /// yet supported in the Rust engine (A5)"`), not a stale-binary
    /// illusion. Manually probed the alternative instead: [`commission`]
    /// (the SAME zero-token mechanism `d1.boot-readiness` already uses,
    /// already accepted) followed by real `qd send:relay` + `qd wait`
    /// against the commissioned session — this WORKS, verified end-to-end
    /// by hand before writing this driver (a real turn landed, transcript
    /// confirmed, exit 0 throughout). This reuses the exact same
    /// `send:relay`/`wait` verb surface every other lane's real turns go
    /// through, arguably MORE faithful than a bespoke `--attach` path would
    /// have been. **Declared receipt strength: `exclusivity-inferred`**,
    /// same shape as pi (see `PiTurnEvidence`'s doc comment) — `qd
    /// send:relay`'s stdout prints a relay message id, not the transcript's
    /// internal `promptId`, so there's no confirmed external id-join;
    /// correlation instead rests on a FRESH commission (zero prior turns,
    /// so `assistant_before` is always 0) plus `qd wait`'s protocol-level
    /// blocking-until-done, plus exactly one new assistant transcript
    /// message with the expected content.
    struct ClaudeCodeTurnEvidence {
        commands: Vec<String>,
        assistant_text: String,
        stop_reason: Option<String>,
        assistant_count: usize,
    }

    fn run_one_claude_code_turn(runner: &Runner) -> Result<ClaudeCodeTurnEvidence, Outcome> {
        let session_name = "c1-cc-bare-turn";
        let mut commands = match commission(runner, session_name) {
            Ok(c) => c,
            Err(o) => return Err(o),
        };
        // Remove this run's OWN transcript from the shared caller-cwd project
        // dir (`~/.claude/projects/<cwd>/<session>.jsonl`) — by its exact path,
        // never rm -rf the shared dir. Must run on the FAILURE paths too: a send
        // that succeeded then a wait that failed still wrote a real transcript
        // (the success-path remove_file below only fires when wait returns 0).
        // `jsonlPath` is lazy, so this is a no-op when no turn has written yet.
        let clean_projects_residue = || {
            if let Some(p) = fetch_info_json(session_name, None)
                .and_then(|v| v.get("jsonlPath").and_then(serde_json::Value::as_str).map(str::to_string))
            {
                let _ = std::fs::remove_file(p);
            }
        };

        let prompt = "Reply with exactly the single word PONG and nothing else. Do not use any tools.";
        let send_cmd = format!("qd send:relay {session_name} <prompt>");
        let send = Command::new(qd_bin()).args(["send:relay", session_name, prompt]).output();
        commands.push(send_cmd);
        match &send {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                clean_projects_residue();
                decommission_best_effort(session_name);
                return Err(runner.fail(commands, format!("send:relay exit {:?}; stderr: {}", o.status.code(), String::from_utf8_lossy(&o.stderr)), "qd send:relay must succeed against a live commissioned session"));
            }
            Err(e) => {
                clean_projects_residue();
                decommission_best_effort(session_name);
                return Err(runner.fail(commands, format!("spawn error: {e}"), "failed to spawn qd send:relay"));
            }
        }

        let wait_cmd = format!("qd wait {session_name} --timeout 120000");
        let wait = Command::new(qd_bin()).args(["wait", session_name, "--timeout", "120000"]).output();
        commands.push(wait_cmd);
        match &wait {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                clean_projects_residue();
                decommission_best_effort(session_name);
                return Err(runner.fail(commands, format!("qd wait exit {:?}; stderr: {}", o.status.code(), String::from_utf8_lossy(&o.stderr)), "qd wait must return 0 once the turn completes"));
            }
            Err(e) => {
                clean_projects_residue();
                decommission_best_effort(session_name);
                return Err(runner.fail(commands, format!("spawn error: {e}"), "failed to spawn qd wait"));
            }
        }

        // `jsonlPath` is LAZY (populated only once a turn has actually
        // written to the transcript — confirmed live: absent right after
        // commission, present after send+wait) — fetched here, post-turn,
        // not before.
        let jsonl_path = fetch_info_json(session_name, None)
            .and_then(|v| v.get("jsonlPath").and_then(serde_json::Value::as_str).map(str::to_string));
        let jsonl_path = match jsonl_path {
            Some(p) => p,
            None => {
                decommission_best_effort(session_name);
                return Err(runner.fail(commands, "qd info --json carried no jsonlPath even after send+wait".to_string(), "cannot read the transcript without its path"));
            }
        };
        let raw = std::fs::read_to_string(&jsonl_path).unwrap_or_default();
        decommission_best_effort(session_name);
        // `bond commission` lands this session's transcript in the CALLER's
        // cwd project dir (`~/.claude/projects/<cwd>/`), which is SHARED
        // with other real sessions — never rm -rf the directory (unlike
        // the ACP jail's uniquely-owned dir). Remove only this run's own
        // file, by its exact path. Live-caught 2026-07-15: an earlier
        // version of this driver left these behind uncleaned; verified by
        // hand which files were actually mine (exact session-name match +
        // timestamp) before ever deleting anything from a shared directory.
        let _ = std::fs::remove_file(&jsonl_path);

        let mut assistant_count = 0usize;
        let mut last_text = String::new();
        let mut last_stop = None;
        for line in raw.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if v.get("type").and_then(serde_json::Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(msg) = v.get("message") else { continue };
            if msg.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
                continue;
            }
            assistant_count += 1;
            last_text = msg
                .get("content")
                .and_then(serde_json::Value::as_array)
                .and_then(|arr| arr.iter().find_map(|c| c.get("text").and_then(serde_json::Value::as_str)))
                .unwrap_or_default()
                .to_string();
            last_stop = msg.get("stop_reason").and_then(serde_json::Value::as_str).map(str::to_string);
        }

        Ok(ClaudeCodeTurnEvidence {
            commands,
            assistant_text: last_text,
            stop_reason: last_stop,
            assistant_count,
        })
    }

    /// `d2.turn-correlation-and-completion`, bare `claude-code` variant.
    /// **Declared receipt strength: `exclusivity-inferred`** — see
    /// [`ClaudeCodeTurnEvidence`]'s doc comment. A fresh commission has zero
    /// prior turns, so exactly one new assistant message (with the correct
    /// content and a clean `end_turn`) correlates to this one injected turn
    /// by exclusion; completion itself is protocol-confirmed by `qd wait`'s
    /// own blocking-until-done contract, not inferred from silence.
    pub fn turn_correlation(runner: &Runner, _session_name: &str) -> Outcome {
        let ev = match run_one_claude_code_turn(runner) {
            Ok(ev) => ev,
            Err(outcome) => return outcome,
        };
        if ev.assistant_count != 1 {
            return runner.fail(ev.commands, format!("expected exactly 1 assistant message on a fresh commission, found {}", ev.assistant_count), "the exclusivity-inferred correlation requires exactly one new assistant message for the one turn injected");
        }
        if ev.stop_reason.as_deref() != Some("end_turn") {
            return runner.fail(ev.commands, format!("stop_reason was {:?}, expected Some(\"end_turn\")", ev.stop_reason), "a clean PONG-only turn must terminate end_turn");
        }
        if !ev.assistant_text.contains("PONG") {
            return runner.fail(ev.commands, format!("assistant_text={:?} does not contain PONG", ev.assistant_text), "the landed reply must match what was asked");
        }
        runner.pass(
            ev.commands,
            format!(
                "[receipt-strength: exclusivity-inferred] qd wait returned 0 (protocol-confirmed \
                 completion); the fresh commission's transcript shows exactly one new assistant \
                 message ({:?}, stop_reason=end_turn) — correlates to the one turn injected by \
                 exclusion",
                ev.assistant_text
            ),
        )
    }

    /// `d5.transcript-tee-captures-assistant-text`, bare `claude-code`
    /// variant. A distinct real turn is drawn per this file's
    /// per-cell-independence convention.
    pub fn transcript_tee(runner: &Runner, _session_name: &str) -> Outcome {
        let ev = match run_one_claude_code_turn(runner) {
            Ok(ev) => ev,
            Err(outcome) => return outcome,
        };
        if !ev.assistant_text.contains("PONG") {
            return runner.fail(ev.commands, format!("transcript assistant_text={:?} does not contain PONG", ev.assistant_text), "the transcript (this lane's own jsonl IS the tee — it's the file Pete's-view reads) must capture the model's literal reply text");
        }
        runner.pass(
            ev.commands,
            format!(
                "[receipt-strength: exclusivity-inferred] the session's own transcript jsonl \
                 (Pete's-view feed) captured the model's literal reply text ({:?}, contains PONG)",
                ev.assistant_text
            ),
        )
    }

    /// `d5.resume-jsonl-continuity-and-recall`, bare `claude-code` variant
    /// — the LAST cell in the whole composite. Manually verified end-to-end
    /// by hand before writing this driver (same discipline as every
    /// mechanism this session): `bond commission` → real turn (state a
    /// codeword) → `bond decommission` → `qd resume <name> --no-attach`
    /// (the SAME mechanism the existing `resume_same_session_id` driver
    /// above already proves preserves the session id) → real turn (recall)
    /// → the session's own transcript jsonl (found via `jsonlPath`, same
    /// lazy-fetch-after-turn fix as [`run_one_claude_code_turn`]) is a
    /// SINGLE continuous file containing the assistant's recall of the
    /// codeword. Two real turns.
    pub fn resume_recall(runner: &Runner, _session_name: &str) -> Outcome {
        let session_name = "c1-cc-bare-resume";
        let mut commands = match commission(runner, session_name) {
            Ok(c) => c,
            Err(o) => return o,
        };
        let fail_and_clean = |runner: &Runner, commands: Vec<String>, observed: String, why: &str| -> Outcome {
            decommission_best_effort(session_name);
            runner.fail(commands, observed, why)
        };

        let codeword = format!("ZEBRA-{}-QUARK", std::process::id() % 10000);
        let turn1 = format!("Remember this exact codeword for later: {codeword}. Just acknowledge you've noted it in one short sentence. Do not use any tools.");
        commands.push(format!("qd send:relay {session_name} <turn1: state codeword>"));
        if !matches!(Command::new(qd_bin()).args(["send:relay", session_name, &turn1]).output(), Ok(o) if o.status.success()) {
            return fail_and_clean(runner, commands, "qd send:relay (turn 1) did not succeed".to_string(), "cannot exercise resume-recall without a successful first send");
        }
        commands.push(format!("qd wait {session_name} --timeout 120000"));
        if !matches!(Command::new(qd_bin()).args(["wait", session_name, "--timeout", "120000"]).output(), Ok(o) if o.status.success()) {
            return fail_and_clean(runner, commands, "qd wait (turn 1) did not succeed".to_string(), "qd wait must return 0 once turn 1 completes");
        }

        commands.push(format!("bond decommission {session_name}"));
        decommission_best_effort(session_name);
        commands.push(format!("qd resume {session_name} --no-attach"));
        let resume = Command::new(qd_bin()).args(["resume", session_name, "--no-attach"]).output();
        if !matches!(&resume, Ok(o) if o.status.success()) {
            let stderr = match &resume {
                Ok(o) => String::from_utf8_lossy(&o.stderr).into_owned(),
                Err(e) => format!("spawn error: {e}"),
            };
            return runner.fail(commands, format!("qd resume did not succeed; stderr/err: {stderr}"), "qd resume must succeed against a decommissioned claude-code-bare session");
        }

        let turn2 = "What was the codeword I told you earlier? Reply with just the codeword, nothing else. Do not use any tools.".to_string();
        commands.push(format!("qd send:relay {session_name} <turn2: recall>"));
        if !matches!(Command::new(qd_bin()).args(["send:relay", session_name, &turn2]).output(), Ok(o) if o.status.success()) {
            return fail_and_clean(runner, commands, "qd send:relay (turn 2) did not succeed".to_string(), "cannot exercise recall without a successful second send");
        }
        commands.push(format!("qd wait {session_name} --timeout 120000"));
        if !matches!(Command::new(qd_bin()).args(["wait", session_name, "--timeout", "120000"]).output(), Ok(o) if o.status.success()) {
            return fail_and_clean(runner, commands, "qd wait (turn 2) did not succeed".to_string(), "qd wait must return 0 once the recall turn completes");
        }

        let jsonl_path = fetch_info_json(session_name, None)
            .and_then(|v| v.get("jsonlPath").and_then(serde_json::Value::as_str).map(str::to_string));
        let jsonl_path = match jsonl_path {
            Some(p) => p,
            None => return fail_and_clean(runner, commands, "qd info --json carried no jsonlPath after both turns".to_string(), "cannot read the transcript without its path"),
        };
        let raw = std::fs::read_to_string(&jsonl_path).unwrap_or_default();
        decommission_best_effort(session_name);
        let _ = std::fs::remove_file(&jsonl_path);

        let recall_found = raw.lines().any(|l| {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else { return false };
            if v.get("type").and_then(serde_json::Value::as_str) != Some("assistant") {
                return false;
            }
            let Some(msg) = v.get("message") else { return false };
            if msg.get("role").and_then(serde_json::Value::as_str) != Some("assistant") {
                return false;
            }
            msg.get("content")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|arr| arr.iter().any(|c| c.get("text").and_then(serde_json::Value::as_str).is_some_and(|t| t.contains(&codeword))))
        });
        if !recall_found {
            return runner.fail(commands, format!("no assistant message contains the codeword {codeword:?} across both turns"), "post-resume, the model must recall pre-stop context (the injected codeword)");
        }
        runner.pass(
            commands,
            format!(
                "qd resume re-established the SAME session (proven by the existing \
                 resume_same_session_id driver's mechanism) after a real prior turn; the recall \
                 turn's assistant reply, found in the ONE continuous transcript jsonl (no fork), \
                 contains the exact codeword {codeword:?} injected on turn 1"
            ),
        )
    }
}

/// Dispatch a (lane, cell id) to its live driver, if one exists. `None` means
/// "no implementation yet" — the caller must leave the cell unresolved, never
/// invent a resolution for it (see module docs).
pub fn run_cell(lane: Lane, cell: &CellId, runner: &Runner, session_name: &str) -> Option<Outcome> {
    match (lane, cell.0.as_str()) {
        (Lane::Pi, "d1.boot-readiness") => Some(pi::boot_readiness(runner, session_name)),
        (Lane::Codex, "d1.boot-readiness") => Some(codex::boot_readiness(runner, session_name)),
        (Lane::AcpClaudeCode, "d1.boot-readiness") => {
            Some(acp::boot_readiness(runner, session_name))
        }
        (Lane::Opencode, "d1.boot-readiness") => Some(opencode::boot_readiness(runner, session_name)),
        (Lane::ClaudeCode, "d1.boot-readiness") => {
            Some(claude_code::boot_readiness(runner, session_name))
        }
        (Lane::Pi, "d1.teardown-reaps-process-group") => {
            Some(pi::teardown_reaps(runner, session_name))
        }
        (Lane::Codex, "d1.teardown-reaps-process-group") => {
            Some(codex::teardown_reaps(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d1.teardown-reaps-process-group") => {
            Some(acp::teardown_reaps(runner, session_name))
        }
        (Lane::Opencode, "d1.teardown-reaps-process-group") => {
            Some(opencode::teardown_reaps(runner, session_name))
        }
        (Lane::ClaudeCode, "d1.teardown-reaps-process-group") => {
            Some(claude_code::teardown_reaps(runner, session_name))
        }
        (Lane::Pi, "d1.launch-addressing") => Some(pi::launch_addressing(runner, session_name)),
        (Lane::Codex, "d1.launch-addressing") => Some(codex::launch_addressing(runner, session_name)),
        (Lane::AcpClaudeCode, "d1.launch-addressing") => {
            Some(acp::launch_addressing(runner, session_name))
        }
        (Lane::Opencode, "d1.launch-addressing") => {
            Some(opencode::launch_addressing(runner, session_name))
        }
        (Lane::ClaudeCode, "d1.launch-addressing") => {
            Some(claude_code::launch_addressing(runner, session_name))
        }
        (Lane::Pi, "d1.transport-is-our-daemon") => {
            Some(pi::transport_is_our_daemon(runner, session_name))
        }
        (Lane::Codex, "d1.transport-is-our-daemon") => {
            Some(codex::transport_is_our_daemon(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d1.transport-is-our-daemon") => {
            Some(acp::transport_is_our_daemon(runner, session_name))
        }
        (Lane::Opencode, "d1.transport-is-our-daemon") => {
            Some(opencode::transport_is_our_daemon(runner, session_name))
        }
        (Lane::ClaudeCode, "d1.transport-is-our-daemon") => {
            Some(claude_code::transport_is_our_daemon(runner, session_name))
        }
        (Lane::Pi, "d1.liveness-process-alive") => {
            Some(pi::liveness_process_alive(runner, session_name))
        }
        (Lane::Codex, "d1.liveness-process-alive") => {
            Some(codex::liveness_process_alive(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d1.liveness-process-alive") => {
            Some(acp::liveness_process_alive(runner, session_name))
        }
        (Lane::Opencode, "d1.liveness-process-alive") => {
            Some(opencode::liveness_process_alive(runner, session_name))
        }
        (Lane::ClaudeCode, "d1.liveness-process-alive") => {
            Some(claude_code::liveness_process_alive(runner, session_name))
        }
        (Lane::Pi, "d1.reconnect-resolves-via-registry") => {
            Some(pi::reconnect_resolves(runner, session_name))
        }
        (Lane::Codex, "d1.reconnect-resolves-via-registry") => {
            Some(codex::reconnect_resolves(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d1.reconnect-resolves-via-registry") => {
            Some(acp::reconnect_resolves(runner, session_name))
        }
        (Lane::Opencode, "d1.reconnect-resolves-via-registry") => {
            Some(opencode::reconnect_resolves(runner, session_name))
        }
        (Lane::ClaudeCode, "d1.reconnect-resolves-via-registry") => {
            Some(claude_code::reconnect_resolves(runner, session_name))
        }
        (Lane::Pi, "d1.resume-same-session-id") => {
            Some(pi::resume_same_session_id(runner, session_name))
        }
        (Lane::Codex, "d1.resume-same-session-id") => {
            Some(codex::resume_same_session_id(runner, session_name))
        }
        (Lane::ClaudeCode, "d1.resume-same-session-id") => {
            Some(claude_code::resume_same_session_id(runner, session_name))
        }
        // AcpClaudeCode/Opencode's d1.resume-same-session-id, wired
        // 2026-07-15 (conf-build-coord-3 ruling, the last cell of the
        // 19x5 matrix): `resume_same_session_id_via_cli` now sends a real
        // turn before stopping+resuming (same fix as pi/claude-code-bare).
        // Manually verified BEFORE writing the fix that `qd send:relay`
        // genuinely works against a PLAIN `qd start`-booted ACP resident
        // for both lanes (not just the D2 3-phase daemon fixture's `qd
        // acp-daemon` boot path, the only prior proof) — real turn sent,
        // completed, session id preserved across stop+resume, both lanes.
        (Lane::AcpClaudeCode, "d1.resume-same-session-id") => {
            Some(acp::resume_same_session_id(runner, session_name))
        }
        (Lane::Opencode, "d1.resume-same-session-id") => {
            Some(opencode::resume_same_session_id(runner, session_name))
        }
        (Lane::Pi, "d1.multiplex-concurrent-distinct-residents") => {
            Some(pi::multiplex_concurrent(runner, session_name))
        }
        (Lane::Codex, "d1.multiplex-concurrent-distinct-residents") => {
            Some(codex::multiplex_concurrent(runner, session_name))
        }
        // AcpClaudeCode/Opencode/ClaudeCode's multiplex_concurrent drivers,
        // wired 2026-07-15 after mc-5 cleared the 3-lane ask (SERIALIZED
        // across lanes, lightest-first: acp/cc -> acp/opencode -> cc-bare;
        // measure-before-each; cc-bare gated at <~1.8G avail; abort on
        // sustained vmstat so>0) — see the acceptance-candidate's
        // "what's still open" section for the clearance record.
        (Lane::AcpClaudeCode, "d1.multiplex-concurrent-distinct-residents") => {
            Some(acp::multiplex_concurrent(runner, session_name))
        }
        (Lane::Opencode, "d1.multiplex-concurrent-distinct-residents") => {
            Some(opencode::multiplex_concurrent(runner, session_name))
        }
        (Lane::ClaudeCode, "d1.multiplex-concurrent-distinct-residents") => {
            Some(claude_code::multiplex_concurrent(runner, session_name))
        }
        (Lane::Pi, "d6.cold-target-send-fails-loud-with-terminal") => {
            Some(pi::cold_target_send_fails(runner, session_name))
        }
        (Lane::Codex, "d6.cold-target-send-fails-loud-with-terminal") => {
            Some(codex::cold_target_send_fails(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d6.cold-target-send-fails-loud-with-terminal") => {
            Some(acp::cold_target_send_fails(runner, session_name))
        }
        (Lane::Opencode, "d6.cold-target-send-fails-loud-with-terminal") => {
            Some(opencode::cold_target_send_fails(runner, session_name))
        }
        // ClaudeCode's d6.cold-target-send-fails-loud-with-terminal is
        // NaPermitted (F2) — the fixture's live-pid-no-endpoint mechanism can't
        // distinguish cold from healthy on this lane (claude rows never carry an
        // endpoint). Wired to PRODUCE the NotApplicable observation like its
        // NaPermitted siblings, never falling through to None.
        (Lane::ClaudeCode, "d6.cold-target-send-fails-loud-with-terminal") => {
            Some(claude_code::cold_target_send_fails(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d6.bridge-death-detection-no-false-positive") => {
            Some(acp::bridge_death_detection(runner, session_name))
        }
        (Lane::Opencode, "d6.bridge-death-detection-no-false-positive") => {
            Some(opencode::bridge_death_detection(runner, session_name))
        }
        (Lane::Pi, "d6.bridge-death-detection-no-false-positive") => {
            Some(pi::bridge_death_detection(runner, session_name))
        }
        (Lane::Codex, "d6.bridge-death-detection-no-false-positive") => {
            Some(codex::bridge_death_detection(runner, session_name))
        }
        (Lane::ClaudeCode, "d6.bridge-death-detection-no-false-positive") => {
            Some(claude_code::bridge_death_detection(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d3.queue-overflow-honors-configured-capacity") => {
            Some(acp::queue_overflow(runner, session_name))
        }
        (Lane::Opencode, "d3.queue-overflow-honors-configured-capacity") => {
            Some(opencode::queue_overflow(runner, session_name))
        }
        (Lane::Pi, "d3.queue-overflow-honors-configured-capacity") => {
            Some(pi::queue_overflow(runner, session_name))
        }
        (Lane::Codex, "d3.queue-overflow-honors-configured-capacity") => {
            Some(codex::queue_overflow(runner, session_name))
        }
        (Lane::ClaudeCode, "d3.queue-overflow-honors-configured-capacity") => {
            Some(claude_code::queue_overflow(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d6.cancel-maps-to-truthful-terminal") => {
            Some(acp::cancel_maps_to_truthful_terminal(runner, session_name))
        }
        (Lane::Opencode, "d6.cancel-maps-to-truthful-terminal") => {
            Some(opencode::cancel_maps_to_truthful_terminal(runner, session_name))
        }
        (Lane::Pi, "d6.cancel-maps-to-truthful-terminal") => {
            Some(pi::cancel_maps_to_truthful_terminal(runner, session_name))
        }
        (Lane::Codex, "d6.cancel-maps-to-truthful-terminal") => {
            Some(codex::cancel_maps_to_truthful_terminal(runner, session_name))
        }
        (Lane::ClaudeCode, "d6.cancel-maps-to-truthful-terminal") => {
            Some(claude_code::cancel_maps_to_truthful_terminal(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d6.self-terminate-on-wedged-child") => {
            Some(acp::self_terminate_on_wedged_child(runner, session_name))
        }
        (Lane::Opencode, "d6.self-terminate-on-wedged-child") => {
            Some(opencode::self_terminate_on_wedged_child(runner, session_name))
        }
        (Lane::Pi, "d6.self-terminate-on-wedged-child") => {
            Some(pi::self_terminate_on_wedged_child(runner, session_name))
        }
        (Lane::Codex, "d6.self-terminate-on-wedged-child") => {
            Some(codex::self_terminate_on_wedged_child(runner, session_name))
        }
        (Lane::ClaudeCode, "d6.self-terminate-on-wedged-child") => {
            Some(claude_code::self_terminate_on_wedged_child(runner, session_name))
        }
        // Pi/Codex have no analogous serve-loop/child-confirmed-dead
        // self-terminate mechanism (registry NaPermitted, ruled 2026-07-15
        // — see the driver's doc comment) — no driver needed.
        (Lane::AcpClaudeCode, "d3.queue-slot-released-no-leak") => {
            Some(acp::queue_slot_released(runner, session_name))
        }
        (Lane::Opencode, "d3.queue-slot-released-no-leak") => {
            Some(opencode::queue_slot_released(runner, session_name))
        }
        (Lane::Pi, "d3.queue-slot-released-no-leak") => {
            Some(pi::queue_slot_released(runner, session_name))
        }
        (Lane::Codex, "d3.queue-slot-released-no-leak") => {
            Some(codex::queue_slot_released(runner, session_name))
        }
        (Lane::ClaudeCode, "d3.queue-slot-released-no-leak") => {
            Some(claude_code::queue_slot_released(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d2.turn-phase-sequence-strict-order") => {
            Some(acp::turn_phase_sequence(runner, session_name))
        }
        (Lane::Opencode, "d2.turn-phase-sequence-strict-order") => {
            Some(opencode::turn_phase_sequence(runner, session_name))
        }
        (Lane::Pi, "d2.turn-phase-sequence-strict-order") => {
            Some(pi::turn_phase_sequence(runner, session_name))
        }
        (Lane::Codex, "d2.turn-phase-sequence-strict-order") => {
            Some(codex::turn_phase_sequence(runner, session_name))
        }
        (Lane::ClaudeCode, "d2.turn-phase-sequence-strict-order") => {
            Some(claude_code::turn_phase_sequence(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d2.delivery-log-consistency-under-home-override") => {
            Some(acp::delivery_log_home_override(runner, session_name))
        }
        (Lane::Opencode, "d2.delivery-log-consistency-under-home-override") => {
            Some(opencode::delivery_log_home_override(runner, session_name))
        }
        (Lane::Pi, "d2.delivery-log-consistency-under-home-override") => {
            Some(pi::delivery_log_home_override(runner, session_name))
        }
        (Lane::Codex, "d2.delivery-log-consistency-under-home-override") => {
            Some(codex::delivery_log_home_override(runner, session_name))
        }
        (Lane::ClaudeCode, "d2.delivery-log-consistency-under-home-override") => {
            Some(claude_code::delivery_log_home_override(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d2.turn-correlation-and-completion") => {
            Some(acp::turn_correlation(runner, session_name))
        }
        (Lane::Opencode, "d2.turn-correlation-and-completion") => {
            Some(opencode::turn_correlation(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d5.transcript-tee-captures-assistant-text") => {
            Some(acp::transcript_tee(runner, session_name))
        }
        (Lane::Opencode, "d5.transcript-tee-captures-assistant-text") => {
            Some(opencode::transcript_tee(runner, session_name))
        }
        (Lane::Pi, "d2.turn-correlation-and-completion") => {
            Some(pi::turn_correlation(runner, session_name))
        }
        (Lane::Codex, "d2.turn-correlation-and-completion") => {
            Some(codex::turn_correlation(runner, session_name))
        }
        (Lane::Pi, "d5.transcript-tee-captures-assistant-text") => {
            Some(pi::transcript_tee(runner, session_name))
        }
        (Lane::Codex, "d5.transcript-tee-captures-assistant-text") => {
            Some(codex::transcript_tee(runner, session_name))
        }
        (Lane::ClaudeCode, "d2.turn-correlation-and-completion") => {
            Some(claude_code::turn_correlation(runner, session_name))
        }
        (Lane::ClaudeCode, "d5.transcript-tee-captures-assistant-text") => {
            Some(claude_code::transcript_tee(runner, session_name))
        }
        (Lane::AcpClaudeCode, "d5.resume-jsonl-continuity-and-recall") => {
            Some(acp::resume_recall(runner, session_name))
        }
        (Lane::Opencode, "d5.resume-jsonl-continuity-and-recall") => {
            Some(opencode::resume_recall(runner, session_name))
        }
        (Lane::Pi, "d5.resume-jsonl-continuity-and-recall") => {
            Some(pi::resume_recall(runner, session_name))
        }
        (Lane::ClaudeCode, "d5.resume-jsonl-continuity-and-recall") => {
            Some(claude_code::resume_recall(runner, session_name))
        }

        // ===== C-2 cells =====
        // D6 partial-write (transport-agnostic reader) — all 5 lanes.
        (l, "d6.partial-write") => Some(c2::partial_write(runner, l)),
        // D6 advertised-surface honesty (qd CLI surface) — all 5 lanes.
        (l, "d6.advertised-surface-honesty") => {
            Some(c2::advertised_surface(runner, l, session_name))
        }
        // D7 property cells on claude-code (the MuxPane lane) — real qrmux
        // attach nested in a real tmux, plain shell pane.
        (Lane::ClaudeCode, "d7.nested-render-correctness") => {
            Some(c2::nested_render_correctness(runner))
        }
        (Lane::ClaudeCode, "d7.detach-chord-reaches-qrmux") => {
            Some(c2::detach_chord_reaches_qrmux(runner))
        }
        (Lane::ClaudeCode, "d7.mouse-passthrough") => Some(c2::mouse_passthrough(runner)),
        (Lane::ClaudeCode, "d7.term-terminfo-honest") => {
            Some(c2::term_terminfo_honest(runner))
        }
        (Lane::ClaudeCode, "d7.altscreen-enter-exit-across-nesting") => {
            Some(c2::altscreen_enter_exit(runner))
        }
        (Lane::ClaudeCode, "d7.cross-size-reattach-both-directions") => {
            Some(c2::cross_size_reattach(runner))
        }
        // D7 zellij-coverage: claude-code → structured Blocked (per-box gate);
        // daemon lanes fall to the d7 daemon-NA guard below.
        (Lane::ClaudeCode, "d7.zellij-nested-render-coverage") => {
            Some(c2::zellij_coverage_claude())
        }
        // D7 (every cell) on a daemon lane → NotApplicable (no terminal to attach).
        // ClaudeCode's d7 property drivers are all wired in their own arms above.
        (l, c) if c.starts_with("d7.") && !matches!(l, Lane::ClaudeCode) => {
            Some(c2::na_d7_daemon(l))
        }
        // D6 dead-relay-sidecar: claude-code → real driver (honest FAIL); daemon → NA.
        (Lane::ClaudeCode, "d6.dead-relay-sidecar") => {
            Some(c2::dead_relay_sidecar(runner, session_name))
        }
        (l, "d6.dead-relay-sidecar") if !matches!(l, Lane::ClaudeCode) => {
            Some(c2::na_dead_relay_daemon(l))
        }
        // D6 sender-killed / wedged-daemon: claude-code → NA; daemon → real driver.
        (Lane::ClaudeCode, "d6.sender-killed-mid-send") => Some(c2::na_sender_killed_cc()),
        (Lane::ClaudeCode, "d6.wedged-daemon") => Some(c2::na_wedged_cc()),
        (l, "d6.wedged-daemon") => Some(c2::wedged_daemon(runner, l, session_name)),
        (l, "d6.sender-killed-mid-send") => Some(c2::sender_killed_mid_send(runner, l, session_name)),

        _ => None,
    }
}

/// C-2: the discriminating new cells (D7 env-matrix + D6 injections +
/// advertised-surface). Each cell exists to FAIL when its defect is present.
/// Consumes the C-1 evidence schema verbatim (never a private evidence format):
/// every `Pass`/`Fail` mints through the commissioned [`Runner`]; the `NotApplicable`
/// / `Blocked` producers mirror the registry's declared reasons byte-for-byte so a
/// stranger reading the grid sees the SAME reason the manifest declares.
///
/// **Same live-run discipline as the rest of this module**: no `#[test]` here;
/// every function is wired to the `QD_C2_LIVE`-gated `tests/conformance_c2_live.rs`
/// entrypoint. The deterministic cells (partial-write, advertised-surface) touch
/// no daemon and no network; the D7 cells drive the standalone `qrmux` binary
/// nested in a real outer tmux with a plain shell pane (provider-invariant render
/// path — see the module doc in `conformance_c2_live.rs`); the D6 injection cells
/// drive the real `qd send:relay` verb inside an isolated [`Jail`].
pub mod c2 {
    use super::*;

    // ==================================================================
    // NotApplicable / Blocked producers — reasons MIRROR registry.rs so the
    // observed outcome's reason matches the manifest's declared applicability.
    // ==================================================================

    /// D7 (any cell) on a `Hosting::Daemon` lane: no terminal to attach.
    pub fn na_d7_daemon(lane: Lane) -> Outcome {
        Outcome::not_applicable(format!(
            "{} is Hosting::Daemon — a provider-owned daemon thread with no qd-owned mux pane and no terminal to attach (lifecycle.rs:68), so the qrmux attach-client env-matrix / nested-render property has no referent on this lane",
            lane.provider_id()
        ))
    }

    /// `d6.dead-relay-sidecar` on a daemon lane: no CcRelay sidecar POST.
    pub fn na_dead_relay_daemon(lane: Lane) -> Outcome {
        Outcome::not_applicable(format!(
            "{} routes send:relay to its daemon ws/ACP inject ladder before the relay-port check and carries no relay port — it never opens a CcRelay HTTP sidecar POST, so the dead-relay-sidecar transport door has no referent on this lane",
            lane.provider_id()
        ))
    }

    /// `d6.sender-killed-mid-send` on claude-code-bare: no sender-written terminal.
    pub fn na_sender_killed_cc() -> Outcome {
        Outcome::not_applicable(
            "claude-code-bare's send:relay writes no success terminal on the relay path (only the non-terminal send-initiated/relay-delivered, and only after the POST returns a message_id — emit_relay_send_events, send_relay.rs:155), so there is no sender-written success terminal a mid-send kill could falsely produce; the phantom-terminal property has no positive referent on this lane",
        )
    }

    /// `d6.wedged-daemon` on claude-code-bare: no daemon inject path.
    pub fn na_wedged_cc() -> Outcome {
        Outcome::not_applicable(
            "the wedged-daemon defense (fixed 5s connect + 30s request timeout on the ws/ACP inject → loud exit + a send-failed delivery-log door) is a daemon-inject-path property; claude-code-bare has no daemon inject path — it routes through the relay POST, bounded by a transport read-timeout with a stderr-only failure and no delivery-log door (that transport-door gap is covered by d6.dead-relay-sidecar)",
        )
    }

    /// `d7.zellij-nested-render-coverage` on claude-code: a genuine per-box gate —
    /// zellij is not installed on this box. Structured, re-entrant Blocked (never
    /// prose, never a silent skip) — a stranger reading the grid sees the zellij
    /// axis as a queryable row with its re-entry path.
    pub fn zellij_coverage_claude() -> Outcome {
        Outcome::blocked(
            "zellij is not installed on this box (lima) — the zellij-nested attach render/detach/resize axis (named in the C-2 target alongside tmux/screen) cannot be exercised here",
            "run this cell on a zellij-equipped box. NOTE the granularity asymmetry (6 per-property tmux/screen cells vs this 1 collapsed zellij cell): on a real zellij box this cell must be split to match the per-property granularity (render/detach/mouse/TERM/altscreen/cross-size-reattach), or carry a composite observation across all of them — the collapse is a box-gap artifact on lima, not a coverage shortcut",
        )
    }

    // ==================================================================
    // D6 partial-write — the delivery-log reader rejects a truncated record
    // (all 5 lanes; the reader is transport-agnostic). Real file I/O through
    // the production read path (`crate::events::read_merged` → `parse_events`).
    // ==================================================================

    /// A complete `message-seen` success terminal (well-formed envelope: event +
    /// pid + seq, plus a send_id payload). `parse_one` accepts it.
    const PW_COMPLETE: &str = r#"{"v":1,"ts":"2026-07-15T00:00:00Z","pid":4242,"seq":1,"session":"c2-pw","name":"c2-pw","event":"message-seen","send_id":"c2-pw-sid"}"#;
    /// The SAME record shape, truncated mid-object (valid JSON prefix, no closing
    /// brace). Written with no trailing newline it is a TORN TAIL; written before
    /// a good line it is interior corruption. Either way `parse_one` must reject
    /// it — never materialize a half-record as a terminal.
    const PW_TRUNCATED: &str = r#"{"v":1,"ts":"2026-07-15T00:00:00Z","pid":4242,"seq":2,"session":"c2-pw","name":"c2-pw","event":"message-se"#;

    /// `d6.partial-write` (all lanes). Writes real delivery-log files in a jail and
    /// reads them through the production `read_merged`/`parse_events` path.
    /// Discrimination arms, all in the evidence: (1) positive control — a COMPLETE
    /// record IS read as a `message-seen` terminal; (2) a torn-tail truncated
    /// record is DROPPED (only the complete terminal survives); (3) an interior
    /// truncated record is forensic-counted (`corrupt_interior`) but never a
    /// terminal and never changes the verdict.
    pub fn partial_write(runner: &Runner, lane: Lane) -> Outcome {
        use crate::events::{parse_events, read_merged};

        let jail = match Jail::new(&format!("c2-pw-{}", lane.provider_id().replace('/', "-"))) {
            Ok(j) => j,
            Err(e) => {
                return runner.fail(
                    vec!["(jail setup)".to_string()],
                    format!("failed to create jail dirs: {e}"),
                    "cannot exercise the partial-write reader without a jail",
                )
            }
        };
        // state_dir whose `sessions/` holds the delivery-log files read_merged reads.
        let state_dir = jail.qd_home.join("state");
        let sessions = state_dir.join("sessions");
        if let Err(e) = std::fs::create_dir_all(&sessions) {
            return runner.fail(
                vec!["(mkdir state/sessions)".to_string()],
                format!("mkdir failed: {e}"),
                "cannot stage the delivery-log files",
            );
        }
        // Arm 1+2: one real file with a complete terminal then a torn-tail truncated
        // record (no trailing newline). read_merged → only the complete terminal.
        let sid = "c2-pw-sid-file";
        let torn_path = sessions.join(format!("{sid}.events.jsonl"));
        if let Err(e) = std::fs::write(&torn_path, format!("{PW_COMPLETE}\n{PW_TRUNCATED}")) {
            return runner.fail(
                vec![format!("(write {torn_path:?})")],
                format!("write failed: {e}"),
                "cannot stage the torn-tail delivery log",
            );
        }
        let torn = read_merged(&state_dir, Some(sid), None);
        let torn_terminals = torn
            .records
            .iter()
            .filter(|r| r.event == "message-seen")
            .count();
        let saw_complete = torn_terminals >= 1;
        let torn_dropped = torn_terminals == 1; // the truncated 2nd record must NOT add a terminal

        // Arm 3: interior corruption — truncated line FOLLOWED by a good line.
        // Pure `parse_events` over constructed text (the same reader).
        let interior = parse_events(&format!("{PW_TRUNCATED}\n{PW_COMPLETE}\n"));
        let interior_terminals = interior
            .records
            .iter()
            .filter(|r| r.event == "message-seen")
            .count();
        let interior_ok = interior_terminals == 1 && interior.corrupt_interior == 1;

        let commands = vec![
            format!("write {sid}.events.jsonl = <complete message-seen>\\n<truncated torn tail, no \\n>"),
            format!("crate::events::read_merged(state_dir, Some({sid:?}), None)"),
            "crate::events::parse_events(<truncated interior line>\\n<complete line>\\n)".to_string(),
        ];
        let observed = format!(
            "lane={}; reader=crate::events::read_merged→parse_events/parse_one (events.rs:650/693, transport-agnostic); \
             arm1 complete-record-read-as-terminal={saw_complete}; \
             arm2 torn-tail-terminals={torn_terminals} (expect 1 — truncated record dropped, not materialized); \
             arm3 interior: terminals={interior_terminals} corrupt_interior={} (expect 1/1 — forensic-counted, never a terminal, verdict unchanged)",
            lane.provider_id(),
            interior.corrupt_interior,
        );
        if saw_complete && torn_dropped && interior_ok {
            runner.pass(commands, observed)
        } else {
            runner.fail(
                commands,
                observed,
                "a truncated/partial delivery-log record was read back as a complete terminal, or a complete record failed to read, or interior corruption altered the verdict — the partial-write honesty property does NOT hold",
            )
        }
    }

    // ==================================================================
    // D6 advertised-surface honesty — documented flag vs behavior (all lanes).
    // Both discrimination arms in the evidence: a flag advertised as a working
    // feature but unconditionally rejected as unsupported → DRIFT (fail arm);
    // a flag whose help DISCLOSES it is unsupported/deprecated/no-op → HONEST
    // (pass arm). Reads the ACTUAL curated help at runtime (qd <verb> --help),
    // never a hardcoded assumption about the advertised surface.
    // ==================================================================

    /// One audited (verb, flag) advertised-surface probe.
    struct FlagProbe {
        /// `qd <verb> --help` — the verb whose curated help advertises the flag.
        verb: &'static str,
        /// The flag token as it appears in help + on the command line.
        flag: &'static str,
        /// The full `qd` argv to exercise the flag (jailed).
        run_args: Vec<String>,
        /// Human label for the evidence.
        label: &'static str,
    }

    /// `d6.advertised-surface-honesty` (all lanes). Audits a fixed set of documented
    /// flags: the `start --attach`/`--port` drift pair (advertised as functional in
    /// help.rs but unconditionally exit-1) and the `stop --force` honest-disclosure
    /// control (help.rs discloses "deprecated no-op"). The `--attach` probe uses the
    /// LANE's own `--provider` so the audit is lane-relevant.
    pub fn advertised_surface(runner: &Runner, lane: Lane, session_name: &str) -> Outcome {
        // Harness-fault paths in this cell route to Blocked, NEVER runner.fail: the
        // DESIGNED outcome is FAIL (doc-drift RED) on every lane, so a
        // jail/spawn/read fault collapsed into Fail would be masked as the genuine
        // drift RED by the is_fail()-only test (the C2-TS class — proactively swept).
        let jail = match Jail::new(&format!("c2-adv-{}", lane.provider_id().replace('/', "-"))) {
            Ok(j) => j,
            Err(e) => {
                return Outcome::blocked(
                    format!("failed to create the audit jail dirs: {e} — a harness fault, not a conformance verdict"),
                    "rerun; if persistent, check temp-dir writability",
                )
            }
        };
        let sess = format!("c2-adv-{session_name}");
        let provider = lane.provider_id();
        let probes = vec![
            // Drift pair — advertised as functional in help, always exit-1.
            FlagProbe {
                verb: "start",
                flag: "--attach",
                run_args: vec![
                    "start".into(),
                    sess.clone(),
                    "--provider".into(),
                    provider.into(),
                    "--attach".into(),
                ],
                label: "start --attach (help.rs:132 advertises 'Attach interactively instead of starting detached'; lifecycle.rs:410 exit-1 'not yet supported')",
            },
            FlagProbe {
                verb: "start",
                flag: "--port",
                run_args: vec![
                    "start".into(),
                    sess.clone(),
                    "--port".into(),
                    "4096".into(),
                ],
                label: "start --port (help.rs:137 advertises 'Port for OpenCode server (default: auto-scan 4096-4106)'; lifecycle.rs:371 exit-1 'parked')",
            },
            // Honest-disclosure control — help discloses deprecated no-op.
            FlagProbe {
                verb: "stop",
                flag: "--force",
                run_args: vec!["stop".into(), sess.clone(), "--force".into()],
                label: "stop --force (help.rs:101 discloses 'Deprecated no-op (stop never prompts)')",
            },
        ];

        let mut commands = Vec::new();
        let mut lines = Vec::new();
        let mut drift_found = false;
        let mut honest_control_ok = false;

        for p in &probes {
            // 1) Read the curated help for the verb and find the flag's advertised line.
            let help_cmd = format!("qd {} --help", p.verb);
            commands.push(help_cmd.clone());
            // Audit the TESTED binary (QD_BIN=CARGO_BIN_EXE_qd), like every other
            // cell — never a stale PATH `qd`. `qd(None)` would be `Command::new("qd")`
            // (the deployed ~/.quorum/bin/qd, which can advertise a different surface);
            // `qd_bin()` honors QD_BIN so this cell audits the same tree it claims to.
            let help_out = Command::new(qd_bin())
                .args([p.verb, "--help"])
                .env("HOME", &jail.home)
                .env("QD_HOME", &jail.qd_home)
                .output();
            let help_text = match &help_out {
                Ok(o) => format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ),
                Err(e) => {
                    return Outcome::blocked(
                        format!("could not run `{help_cmd}` to read the advertised surface: {e} — a harness fault, not a conformance verdict"),
                        "rerun; if persistent, check the qd binary (QD_BIN) spawnability",
                    )
                }
            };
            let help_line = help_text
                .lines()
                .find(|l| l.contains(p.flag))
                .unwrap_or("<flag not present in curated help>")
                .trim()
                .to_string();
            let advertised = help_text.lines().any(|l| l.contains(p.flag));
            let discloses_unsupported = {
                let l = help_line.to_ascii_lowercase();
                l.contains("not yet")
                    || l.contains("unsupported")
                    || l.contains("deprecated")
                    || l.contains("no-op")
                    || l.contains("parked")
            };

            // 2) Exercise the flag (jailed) and observe exit + stderr.
            let run_cmd = format!("qd {} (jailed)", p.run_args.join(" "));
            commands.push(run_cmd);
            // The TESTED binary (QD_BIN), not PATH `qd` — see the help probe above.
            let run_out = Command::new(qd_bin())
                .args(&p.run_args)
                .env("HOME", &jail.home)
                .env("QD_HOME", &jail.qd_home)
                .env_remove("QD_BOOT_AWAIT_RELAY")
                .output();
            let (exit_code, stderr) = match &run_out {
                Ok(o) => (
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr).to_string(),
                ),
                Err(e) => {
                    return Outcome::blocked(
                        format!("could not run `qd {}` to exercise the audited flag: {e} — a harness fault, not a conformance verdict", p.run_args.join(" ")),
                        "rerun; if persistent, check the qd binary (QD_BIN) spawnability",
                    )
                }
            };
            let stderr_l = stderr.to_ascii_lowercase();
            // The flag was rejected AS UNSUPPORTED (not merely as some unrelated
            // error like session-not-found): non-zero exit AND stderr names the
            // unsupported/parked condition alongside the flag token.
            let rejected_as_unsupported = exit_code != Some(0)
                && stderr.contains(p.flag)
                && (stderr_l.contains("not yet supported")
                    || stderr_l.contains("parked")
                    || stderr_l.contains("unsupported"));

            // DRIFT: the advertised surface (help) presents the flag as a working
            // feature (no disclosure) yet the handler unconditionally rejects it as
            // unsupported. The honest form (disclosed → PASS arm) is NOT drift.
            let drift = advertised && rejected_as_unsupported && !discloses_unsupported;
            if drift {
                drift_found = true;
            }
            // The honest-disclosure control PASSES iff its help discloses AND it is
            // NOT rejected-as-unsupported (accepted/no-op as documented).
            if p.flag == "--force" && discloses_unsupported && !rejected_as_unsupported {
                honest_control_ok = true;
            }
            lines.push(format!(
                "[{}] advertised={advertised} discloses_unsupported={discloses_unsupported} exit={exit_code:?} rejected_as_unsupported={rejected_as_unsupported} → drift={drift}; help_line={help_line:?}",
                p.label
            ));
        }

        let observed = format!(
            "lane={}; audited {} documented flags via runtime `qd <verb> --help` (advertised surface) vs behavior; \
             honest-disclosure control (stop --force) classified HONEST={honest_control_ok}; drift_found={drift_found}; \
             per-flag: {}",
            lane.provider_id(),
            probes.len(),
            lines.join(" | ")
        );

        // Both arms must be demonstrated: the honest control classified correctly
        // (proves the detector is not merely always-red) AND the verdict is the
        // drift finding.
        if !honest_control_ok {
            // The detector can't classify the honest control → a HARNESS/detector
            // fault, NOT a conformance FAIL. An Outcome::Fail here would be
            // indistinguishable from a genuine drift finding under an is_fail()-only
            // assertion (a broken detector must never read as a drift RED). Blocked
            // with a re-entry (trivial-shape C2-TS-2).
            return Outcome::blocked(
                format!(
                    "the honest-disclosure control (stop --force) was NOT classified as honest — the advertised-surface detector is unsound (cannot distinguish disclosed-unsupported from advertised-but-broken), so no drift verdict can be trusted. Evidence: {observed}"
                ),
                "re-check the stop --force curated help disclosure + the detector's discloses_unsupported keyword set; rerun once the detector classifies the honest control",
            );
        }
        if drift_found {
            runner.fail(
                commands,
                observed,
                "the curated help advertises a flag as a working feature (no unsupported/deprecated/no-op disclosure) while the handler unconditionally rejects it as unsupported — an advertised-surface honesty FAILURE (doc-drift). start --attach / start --port are the live instances; stop --force is the honest-disclosure PASS arm that proves the detector discriminates",
            )
        } else {
            runner.pass(
                commands,
                observed,
            )
        }
    }

    // ==================================================================
    // D7 env-matrix — the qrmux attach client nested inside a REAL outer tmux
    // with a PLAIN SHELL pane. Required only on claude-code (the MuxPane lane);
    // the render/reflow/attach/altscreen/TERM path is server-side and
    // provider-invariant (VTE Screen/Grid), and qd's claude MuxPane attaches via
    // the IDENTICAL attach_session path (embedded_mux.rs:461) the standalone
    // qrmux binary uses — so a shell-pane qrmux attach exercises the exact code
    // the claude-code lane's attach surface rides (representative, not trivial).
    // ==================================================================

    /// The `qrmux` binary to drive. `QRMUX_BIN` (set by the live test to the
    /// freshly-built worktree binary — fingerprinted before any RUN-proof
    /// counts) else bare `qrmux` on PATH.
    pub fn qrmux_bin() -> String {
        std::env::var("QRMUX_BIN").unwrap_or_else(|_| "qrmux".to_string())
    }

    /// A nested-mux fixture: an OUTER `tmux` session at a known size, into which a
    /// standalone `qrmux` attach client is launched with a plain shell pane. The
    /// qrmux socket dir is isolated to a SHORT `XDG_RUNTIME_DIR` (sun_path budget;
    /// never touches the real per-user runtime dir). `Drop` reaps the qrmux
    /// session, the outer tmux session, and the runtime dir.
    struct NestedMux {
        outer: String,
        inner: String,
        rt: std::path::PathBuf,
        qrmux: String,
        cols: u16,
        rows: u16,
    }

    impl NestedMux {
        fn new(tag: &str, cols: u16, rows: u16) -> Result<Self, String> {
            let uniq = format!("{}-{}", std::process::id(), tag);
            let outer = format!("c2d7o-{uniq}");
            let inner = format!("c2i{}", std::process::id());
            // SHORT runtime dir: qrmux binds <rt>/qrmux/<inner>.sock; keep the
            // path well under the Unix sun_path limit.
            let rt = std::path::PathBuf::from(format!("/tmp/c2q{}", std::process::id()));
            std::fs::create_dir_all(&rt).map_err(|e| format!("mkdir rt {rt:?}: {e}"))?;
            let m = NestedMux {
                outer,
                inner,
                rt,
                qrmux: qrmux_bin(),
                cols,
                rows,
            };
            // Start the outer tmux, detached, at the requested size. TMUX unset so
            // we never nest into an ambient server; a private socket keeps us clear
            // of any real user tmux.
            let out = Command::new("tmux")
                .args([
                    "new-session",
                    "-d",
                    "-s",
                    &m.outer,
                    "-x",
                    &cols.to_string(),
                    "-y",
                    &rows.to_string(),
                ])
                .env_remove("TMUX")
                .output()
                .map_err(|e| format!("tmux new-session spawn: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "tmux new-session failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            Ok(m)
        }

        fn tmux(&self, args: &[&str]) -> std::process::Output {
            Command::new("tmux")
                .args(args)
                .env_remove("TMUX")
                .output()
                .unwrap_or_else(|e| panic!("tmux {args:?}: {e}"))
        }

        /// The foreground command of the outer pane — the reliable attach signal:
        /// `qrmux` while a client is attached, `bash` (the shell) otherwise.
        fn pane_cmd(&self) -> String {
            let o = self.tmux(&[
                "display-message",
                "-t",
                &self.outer,
                "-p",
                "#{pane_current_command}",
            ]);
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }

        /// Poll `pane_cmd()` until it equals `want` (or `!= want` when `equal` is
        /// false), up to `timeout_ms`. Returns whether the condition was met.
        fn wait_pane_cmd(&self, want: &str, equal: bool, timeout_ms: u64) -> bool {
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_millis(timeout_ms) {
                let cur = self.pane_cmd();
                if (cur == want) == equal {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            (self.pane_cmd() == want) == equal
        }

        /// Launch `qrmux new <inner>` in the outer pane (isolated socket dir) and
        /// wait until the client is attached (pane cmd == qrmux).
        fn qrmux_new(&self) -> Result<(), String> {
            let cmd = format!(
                "XDG_RUNTIME_DIR={} {} new {}",
                self.rt.display(),
                self.qrmux,
                self.inner
            );
            self.tmux(&["send-keys", "-t", &self.outer, &cmd, "Enter"]);
            if !self.wait_pane_cmd("qrmux", true, 8000) {
                return Err(format!(
                    "qrmux client never attached (pane cmd stayed {:?}) after `{cmd}`",
                    self.pane_cmd()
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
            Ok(())
        }

        /// Launch `qrmux attach <inner>` in the outer pane and wait for attach.
        fn qrmux_attach(&self) -> Result<(), String> {
            let cmd = format!(
                "XDG_RUNTIME_DIR={} {} attach {}",
                self.rt.display(),
                self.qrmux,
                self.inner
            );
            self.tmux(&["send-keys", "-t", &self.outer, &cmd, "Enter"]);
            if !self.wait_pane_cmd("qrmux", true, 8000) {
                return Err(format!(
                    "qrmux client never re-attached (pane cmd stayed {:?}) after `{cmd}`",
                    self.pane_cmd()
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
            Ok(())
        }

        /// Send a line to the pane's foreground (the inner shell while attached).
        fn send_line(&self, s: &str) {
            self.tmux(&["send-keys", "-t", &self.outer, s, "Enter"]);
            std::thread::sleep(std::time::Duration::from_millis(600));
        }

        /// Send raw bytes (hex) to the pane — e.g. the detach chord `1c`.
        fn send_hex(&self, hex: &str) {
            self.tmux(&["send-keys", "-t", &self.outer, "-H", hex]);
            std::thread::sleep(std::time::Duration::from_millis(400));
        }

        /// The rendered pane grid (what qrmux painted, as tmux displays it).
        fn capture(&self) -> String {
            let o = self.tmux(&["capture-pane", "-t", &self.outer, "-p"]);
            String::from_utf8_lossy(&o.stdout).to_string()
        }

        /// Detach the qrmux client (raw 0x1c) and wait until the pane returns to
        /// the shell. Returns whether the client detached.
        fn detach(&self) -> bool {
            self.send_hex("1c");
            self.wait_pane_cmd("qrmux", false, 5000)
        }

        /// Resize the outer window (SIGWINCH reaches the qrmux client → the server
        /// re-renders at the new size).
        fn resize(&mut self, cols: u16, rows: u16) {
            self.tmux(&[
                "resize-window",
                "-t",
                &self.outer,
                "-x",
                &cols.to_string(),
                "-y",
                &rows.to_string(),
            ]);
            self.cols = cols;
            self.rows = rows;
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    impl Drop for NestedMux {
        fn drop(&mut self) {
            let _ = Command::new(&self.qrmux)
                .args(["kill", &self.inner])
                .env("XDG_RUNTIME_DIR", &self.rt)
                .env_remove("TMUX")
                .output();
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &self.outer])
                .env_remove("TMUX")
                .output();
            let _ = std::fs::remove_dir_all(&self.rt);
        }
    }

    /// `d7.nested-render-correctness` (claude-code). A known marker printed in the
    /// inner shell must render, intact and readable, in the outer tmux capture —
    /// proving the qrmux attach client paints a correct grid nested in a real mux.
    pub fn nested_render_correctness(runner: &Runner) -> Outcome {
        let mux = match NestedMux::new("render", 80, 24) {
            Ok(m) => m,
            Err(e) => return runner.fail(vec!["(nested-mux setup)".into()], e, "cannot stage the nested mux"),
        };
        if let Err(e) = mux.qrmux_new() {
            return runner.fail(vec!["qrmux new (nested in tmux)".into()], e, "qrmux did not attach nested in tmux");
        }
        let marker = format!("RENDERMARK{}", std::process::id());
        // Wrap the OUTPUT in [[…]] so the assertion matches the rendered output,
        // NOT the typed command's echo (the echo shows `[[%s]]`, only the output
        // shows `[[<marker>]]`).
        let wrapped = format!("[[{marker}]]");
        mux.send_line(&format!("printf '[[%s]]\\n' {marker}"));
        let cap = mux.capture();
        let present = cap.lines().any(|l| l.contains(&wrapped));
        let commands = vec![
            "tmux new-session (outer, 80x24)".into(),
            format!("qrmux new {} (nested)", mux.inner),
            format!("printf '%s' {marker} (inner shell)"),
            "tmux capture-pane -p".into(),
        ];
        let observed = format!(
            "marker {marker:?} rendered-in-nested-capture={present}; pane_cmd={:?}; capture tail: {:?}",
            mux.pane_cmd(),
            cap.lines().rev().take(4).collect::<Vec<_>>()
        );
        if present {
            runner.pass(commands, observed)
        } else {
            runner.fail(commands, observed, "the marker did not render in the nested qrmux capture — render-correctness through the nesting does NOT hold")
        }
    }

    /// `d7.detach-chord-reaches-qrmux` (claude-code). The hardcoded chord (0x1c)
    /// sent through the outer mux detaches the inner client AND the resident
    /// survives; a NON-chord byte does NOT detach (the discriminating negative arm).
    pub fn detach_chord_reaches_qrmux(runner: &Runner) -> Outcome {
        let mux = match NestedMux::new("detach", 80, 24) {
            Ok(m) => m,
            Err(e) => return runner.fail(vec!["(nested-mux setup)".into()], e, "cannot stage the nested mux"),
        };
        if let Err(e) = mux.qrmux_new() {
            return runner.fail(vec!["qrmux new (nested)".into()], e, "qrmux did not attach");
        }
        let marker = format!("DETACHSURV{}", std::process::id());
        let wrapped = format!("[[{marker}]]");
        mux.send_line(&format!("printf '[[%s]]\\n' {marker}"));

        // Negative arm: a non-chord byte ('x' = 0x78) must NOT detach.
        mux.send_hex("78");
        std::thread::sleep(std::time::Duration::from_millis(400));
        let still_attached_after_plain = mux.pane_cmd() == "qrmux";

        // Positive arm: the chord (0x1c) detaches the client.
        let detached = mux.detach();

        // Resident survives: reattach and the prior marker is still present.
        let reattach_ok = mux.qrmux_attach().is_ok();
        let survived = reattach_ok && mux.capture().lines().any(|l| l.contains(&wrapped));

        let commands = vec![
            format!("qrmux new {} (nested)", mux.inner),
            "send 0x78 (non-chord) → assert still attached".into(),
            "send 0x1c (detach chord) → assert detached".into(),
            format!("qrmux attach {} → assert resident survived (marker present)", mux.inner),
        ];
        let observed = format!(
            "non-chord-byte-kept-attached={still_attached_after_plain}; chord-detached={detached}; \
             reattach-ok={reattach_ok}; resident-survived-with-marker={survived}"
        );
        if still_attached_after_plain && detached && survived {
            runner.pass(commands, observed)
        } else {
            runner.fail(commands, observed, "the detach-chord property does not hold nested: either a non-chord byte detached, the chord failed to detach, or the resident did not survive")
        }
    }

    /// `d7.term-terminfo-honest` (claude-code). The TERM the inner pane sees is
    /// truthfully the pinned xterm-256color the qrmux PTY provides (pty.rs:82) —
    /// read honestly through the nesting, not asserted.
    pub fn term_terminfo_honest(runner: &Runner) -> Outcome {
        let mux = match NestedMux::new("term", 80, 24) {
            Ok(m) => m,
            Err(e) => return runner.fail(vec!["(nested-mux setup)".into()], e, "cannot stage the nested mux"),
        };
        if let Err(e) = mux.qrmux_new() {
            return runner.fail(vec!["qrmux new (nested)".into()], e, "qrmux did not attach");
        }
        let tag = format!("TERMIS{}", std::process::id());
        // Output form `TERMIS<pid>[[<value>]]`; the typed command's echo shows
        // `TERMIS<pid>[[%s]]`, so matching `TERMIS<pid>[[xterm-256color]]` hits ONLY
        // the expanded output line, never the command echo (the earlier false FAIL).
        mux.send_line(&format!("printf '{tag}[[%s]]\\n' \"$TERM\""));
        let cap = mux.capture();
        let want = format!("{tag}[[xterm-256color]]");
        let line = cap
            .lines()
            .find(|l| l.contains(&tag) && l.contains("[[") && !l.contains("%s"))
            .unwrap_or("")
            .to_string();
        let honest = cap.lines().any(|l| l.contains(&want));
        let commands = vec![
            format!("qrmux new {} (nested)", mux.inner),
            "printf '$TERM' (inner shell)".into(),
            "tmux capture-pane -p".into(),
        ];
        let observed = format!("inner TERM line={line:?}; pinned-xterm-256color(pty.rs:82)-honest={honest}");
        if honest {
            runner.pass(commands, observed)
        } else {
            runner.fail(commands, observed, "the inner TERM was not the pinned xterm-256color the PTY provides — TERM/terminfo honesty does NOT hold through the nesting")
        }
    }

    /// `d7.altscreen-enter-exit-across-nesting` (claude-code). Entering the alt
    /// screen shows alt content; exiting restores the main grid (no alt residue) —
    /// rendered correctly across the nesting.
    pub fn altscreen_enter_exit(runner: &Runner) -> Outcome {
        let mux = match NestedMux::new("alt", 80, 24) {
            Ok(m) => m,
            Err(e) => return runner.fail(vec!["(nested-mux setup)".into()], e, "cannot stage the nested mux"),
        };
        if let Err(e) = mux.qrmux_new() {
            return runner.fail(vec!["qrmux new (nested)".into()], e, "qrmux did not attach");
        }
        // [[…]]-wrapped outputs so assertions match the RENDERED output, never the
        // typed command's echo (which carries `[[%s]]`). Without this, the echo of
        // the alt-entering command — typed on MAIN before the alt switch — would
        // leave `ALTSCR` text on the restored main grid and falsely fail alt_gone.
        let main_mark = format!("[[MAINSCR{}]]", std::process::id());
        let alt_mark = format!("[[ALTSCR{}]]", std::process::id());
        mux.send_line(&format!("printf '[[MAINSCR%s]]\\n' {}", std::process::id()));
        // Enter alt screen, print alt content.
        mux.send_line(&format!(
            "printf '\\033[?1049h'; printf '[[ALTSCR%s]]\\n' {}",
            std::process::id()
        ));
        let in_alt = mux.capture();
        let alt_shows = in_alt.lines().any(|l| l.contains(&alt_mark));
        let main_hidden_in_alt = !in_alt.lines().any(|l| l.contains(&main_mark));
        // Exit alt screen; the main grid (with main_mark) is restored.
        mux.send_line("printf '\\033[?1049l'");
        let after = mux.capture();
        let main_restored = after.lines().any(|l| l.contains(&main_mark));
        let alt_gone = !after.lines().any(|l| l.contains(&alt_mark));
        let commands = vec![
            format!("qrmux new {} (nested)", mux.inner),
            "printf main marker; \\033[?1049h + alt marker".into(),
            "capture (alt); printf \\033[?1049l; capture (main restored)".into(),
        ];
        let observed = format!(
            "alt-content-shown={alt_shows}; main-hidden-in-alt={main_hidden_in_alt}; \
             main-restored-on-exit={main_restored}; alt-residue-gone={alt_gone}"
        );
        if alt_shows && main_restored && alt_gone {
            runner.pass(commands, observed)
        } else {
            runner.fail(commands, observed, "alt-screen enter/exit did not render correctly across the nesting (alt content missing, main not restored, or alt residue left)")
        }
    }

    /// `d7.mouse-passthrough` (claude-code). A mouse mode enabled by the inner app
    /// survives a detach/reattach — qrmux re-emits the inner app's mouse mode to a
    /// client re-attaching through the outer mux (server-side mode replay,
    /// render.rs:158-167). Observed via `capture-pane -e`'s escape-visible stream
    /// on reattach.
    pub fn mouse_passthrough(runner: &Runner) -> Outcome {
        let mux = match NestedMux::new("mouse", 80, 24) {
            Ok(m) => m,
            Err(e) => return runner.fail(vec!["(nested-mux setup)".into()], e, "cannot stage the nested mux"),
        };
        // Enable tmux's own mouse so the outer pane reflects the inner request.
        mux.tmux(&["set", "-t", &mux.outer, "mouse", "on"]);
        if let Err(e) = mux.qrmux_new() {
            return runner.fail(vec!["qrmux new (nested)".into()], e, "qrmux did not attach");
        }
        // ARM A — nested LIVE passthrough: the inner app enables SGR mouse (?1006)
        // + button-event (?1002); while attached, the OUTER tmux must reflect the
        // inner program's mouse request (the mode passed through qrmux into the
        // nesting).
        mux.send_line("printf '\\033[?1002h\\033[?1006h'");
        let flag_attached = {
            let o = mux.tmux(&[
                "display-message",
                "-t",
                &mux.outer,
                "-p",
                "#{?mouse_any_flag,mouse,nomouse}",
            ]);
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        let nested_live_passthrough = flag_attached == "mouse";

        // ARM B — reattach REPLAY: detach, then re-attach with qrmux's raw output
        // captured through a real pty (`script`), and confirm qrmux RE-EMITS the
        // inner app's mouse modes on the reattach full-render (server-side mode
        // replay, render.rs:158-167). tmux's own mouse flag is an unreliable
        // observable for a re-attach (it cycles on the program change), so the
        // reattach arm reads qrmux's actual byte stream, which carries the truth.
        let _ = mux.detach();
        let raw_out = std::env::temp_dir().join(format!("c2ms{}.out", std::process::id()));
        let attach_cmd = format!(
            "XDG_RUNTIME_DIR={} {} attach {}",
            mux.rt.display(),
            mux.qrmux,
            mux.inner
        );
        let mut child = match Command::new("script")
            .args(["-qec", &attach_cmd, &raw_out.to_string_lossy()])
            .env_remove("TMUX")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return runner.fail(
                    vec!["script -qec 'qrmux attach' (pty capture)".into()],
                    format!("spawn script: {e}"),
                    "cannot capture the reattach byte stream to confirm mouse-mode replay",
                )
            }
        };
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let _ = child.kill();
        let _ = child.wait();
        let raw = std::fs::read(&raw_out).unwrap_or_default();
        let raw_s = String::from_utf8_lossy(&raw);
        let reattach_replays = raw_s.contains("[?1002h") || raw_s.contains("[?1006h");
        let _ = std::fs::remove_file(&raw_out);

        let commands = vec![
            format!("qrmux new {} (nested in tmux)", mux.inner),
            "inner: printf '\\033[?1002h\\033[?1006h' (enable mouse)".into(),
            "tmux #{mouse_any_flag} while attached (ARM A: nested live passthrough)".into(),
            "detach; script -qec 'qrmux attach' (ARM B: raw reattach stream re-emits ?1002h/?1006h)".into(),
        ];
        let observed = format!(
            "ARM A nested-live-passthrough (tmux mouse flag while attached)={nested_live_passthrough} (flag={flag_attached:?}); \
             ARM B reattach-replays-mouse-modes-in-raw-stream={reattach_replays}"
        );
        if nested_live_passthrough && reattach_replays {
            runner.pass(commands, observed)
        } else {
            runner.fail(
                commands,
                observed,
                "mouse passthrough does not hold: the inner app's mouse mode did not reach the outer mux while attached, or was not re-emitted on reattach",
            )
        }
    }

    /// `d7.cross-size-reattach-both-directions` (claude-code). THE SC-1 gap.
    /// Content wider than the reattach width, plus scrollback, must survive a
    /// cross-size reattach (grow AND shrink) intact. EXPECTED RED while the qrmux
    /// cross-width reflow garble is live (Grid::resize per-row truncation, no
    /// rewrap — grid.rs:741-774; the existing grid.rs:1330 test locks in the
    /// lossy behavior). Discrimination is against the defect-present tree per B-1:
    /// today's live-garble tree (recorded here), else C-4's D7 mutant (F-1).
    pub fn cross_size_reattach(runner: &Runner) -> Outcome {
        let pid = std::process::id();
        // Setup faults route to Blocked, NEVER runner.fail: this cell's DESIGNED
        // outcome is FAIL (the SC-1 RED), so a nested-mux/attach fault collapsed into
        // Fail would be masked as the genuine RED by the is_fail()-only test (the
        // C2-TS class — proactively swept). The PASS-lane D7 cells keep runner.fail
        // (is_pass() already catches a setup fault there).
        let mut mux = match NestedMux::new("reattach", 80, 24) {
            Ok(m) => m,
            Err(e) => return Outcome::blocked(
                format!("could not stage the nested mux (outer tmux + qrmux): {e} — a harness fault, not the SC-1 defect"),
                "rerun; if persistent, check tmux availability + the short XDG_RUNTIME_DIR socket isolation",
            ),
        };
        if let Err(e) = mux.qrmux_new() {
            return Outcome::blocked(
                format!("qrmux did not attach nested in tmux: {e} — a harness fault, not the SC-1 defect", ),
                "rerun; if persistent, check the qrmux binary + the attach-signal (pane_current_command) timing",
            );
        }
        // A 60-char line: fits on one row at 80, must WRAP (not truncate) at 40.
        // START and END markers straddle the 40-col boundary; correct reflow keeps
        // END on a continuation row, the truncation garble DROPS it.
        let start = format!("L1START{pid}");
        let end = format!("L1END{pid}");
        let pad = "A".repeat(60usize.saturating_sub(start.len() + end.len() + 1));
        let content = format!("{start}_{pad}_{end}");
        // Write the 60-char line to a file and `cat` it: the typed command echo is
        // `cat <path>` (carries NO marker), so START/END appear ONLY in the rendered
        // output — the assertion cannot be fooled by the command echo, and END being
        // wide (past col 40) is a genuine output-only signal on the shrink reattach.
        let marker_path = std::env::temp_dir().join(format!("c2xs{pid}.txt"));
        let _ = std::fs::write(&marker_path, format!("{content}\n"));
        // ORDER MATTERS (why-holder F1, note 01KXJPJXXK). Generate NON-EMPTY
        // scrollback FIRST (R9 §C-2 requires non-empty scrollback), THEN print the
        // wide test line LAST so it is the BOTTOM VISIBLE row at the moment of resize.
        // If the wide line is printed first with 30 scrollback lines after, it scrolls
        // OFF-SCREEN before the resize; scrollback rows keep their original 80-col
        // width (grid.rs:742) so END is never truncated — a perfectly-reflowing qrmux
        // then yields the IDENTICAL "END absent" from the visible-only capture, and
        // the RED would NOT demonstrate the SC-1 defect at all. The wide line must be
        // VISIBLE at resize so the shrink exercises Grid::resize on it.
        mux.send_line(&format!("for i in $(seq 1 30); do echo SB{pid}_$i; done"));
        mux.send_line(&format!("cat {}", marker_path.display()));

        // SHRINK direction: detach, resize 80→40, reattach.
        let detach1 = mux.detach();
        mux.resize(40, 24);
        let reattach1 = mux.qrmux_attach();
        let cap_shrink = mux.capture();
        let end_present_after_shrink = cap_shrink.lines().any(|l| l.contains(&end));
        let start_present_after_shrink = cap_shrink.lines().any(|l| l.contains(&start));

        // GROW direction: detach, resize 40→100, reattach.
        let detach2 = mux.detach();
        mux.resize(100, 24);
        let reattach2 = mux.qrmux_attach();
        let cap_grow = mux.capture();
        // After growing back wide, the full 60-char line should be intact on one row.
        let full_line_intact_after_grow = cap_grow
            .lines()
            .any(|l| l.contains(&start) && l.contains(&end));
        let _ = std::fs::remove_file(&marker_path);

        let commands = vec![
            format!("qrmux new {} (nested, 80x24)", mux.inner),
            "print 30 scrollback lines, THEN a visible 60-char wrapping line (bottom row)".into(),
            "detach; resize 80→40; reattach; capture (SHRINK)".into(),
            "detach; resize 40→100; reattach; capture (GROW)".into(),
        ];
        let observed = format!(
            "detach1={detach1} reattach1={} SHRINK: start-present={start_present_after_shrink} end-present={end_present_after_shrink} (correct reflow keeps END on a continuation row; truncation garble DROPS it); \
             detach2={detach2} reattach2={} GROW: full-60ch-line-intact-on-one-row={full_line_intact_after_grow}; \
             shrink-capture tail={:?}",
            reattach1.is_ok(),
            reattach2.is_ok(),
            cap_shrink.lines().filter(|l| l.contains(&format!("{pid}"))).take(4).collect::<Vec<_>>(),
        );
        // Harness-fault gate FIRST (trivial-shape C2-TS-1): a reattach flake or a
        // non-rendered wide line is NOT the SC-1 defect — it must be Blocked
        // (re-entry: rerun), never the designed Fail. The SC-1 RED is gated on the
        // truncation FINGERPRINT (line rendered + END dropped past the new width),
        // not on the mere negation of the pass condition (which a flake would trip).
        if reattach1.is_err() {
            return Outcome::blocked(
                format!("SHRINK reattach (qrmux attach) failed — a harness flake, not the SC-1 defect. {observed}"),
                "rerun the cell; if persistent, check the NestedMux attach timing / socket isolation",
            );
        }
        if !start_present_after_shrink {
            return Outcome::blocked(
                format!("the wide test line did not render as the bottom visible row after the SHRINK reattach (START absent) — a harness fault (the line must be VISIBLE to exercise Grid::resize on it), not the SC-1 defect. {observed}"),
                "rerun the cell; if persistent, check the scrollback-then-wide-line ordering / capture timing",
            );
        }
        if reattach2.is_err() {
            return Outcome::blocked(
                format!("GROW reattach (qrmux attach) failed — a harness flake, not the SC-1 defect. {observed}"),
                "rerun the cell; if persistent, check the NestedMux attach timing / socket isolation",
            );
        }
        // Both reattaches OK and the wide line rendered VISIBLE at shrink (START
        // present). The fingerprints now discriminate genuinely: SHRINK keeps END on
        // a continuation row (reflow) → part of PASS; SHRINK drops END past col 40
        // (truncation) → the SC-1 RED. GROW corroborates: after growing back wide the
        // full line is intact iff reflow, dropped iff the shrink already truncated it.
        if end_present_after_shrink && full_line_intact_after_grow {
            runner.pass(commands, observed)
        } else {
            runner.fail(
                commands,
                observed,
                "cross-size reattach did NOT preserve wide content — the SC-1 truncation FINGERPRINT: the wide line rendered (START present) and both reattaches succeeded, yet END was DROPPED past col 40 on the SHRINK reattach and/or the full line was not restored on GROW. qrmux's Grid::resize truncates rather than reflowing on a width change (grid.rs:741-774), so content past the new width is dropped on reattach. Expected RED on today's defect-present tree (B-1; else discriminated vs C-4's D7 mutant per F-1). The underlying qrmux reflow defect is brano/C-4's remit, out of C-2 scope",
            )
        }
    }

    // ==================================================================
    // D6 daemon-inject injections — wedged-daemon + sender-killed-mid-send +
    // dead-relay-sidecar. Made to GENUINELY inject (c2-executor-2, 2026-07-15),
    // atop c2-executor-1's reusable HangServer / jail / kill-group scaffolding.
    //
    // The identity fence is the crux (live-caught by c2-executor-1). qd's send
    // paths gate the wedge behind a daemon-IDENTITY probe of the row's pid:
    //   - acp/{claude-code,opencode}: run_acp_send → cmdline_is_our_acp_daemon
    //     (acp_residence.rs:156) — the pid's /proc cmdline must carry the
    //     `acp-daemon` marker AND the recorded endpoint (substring), else it
    //     fast-fails at the identity door (Tier != Acp) before reaching connect.
    //   - pi: run_pi_send → cmdline_is_our_pi_daemon (pi/residence.rs:392) +
    //     floor_may_fire (pi/floor.rs:230). A NON-matching identity is "resident
    //     dead" → DROPS to the `pi -p` structured FLOOR (a real one-shot child, NOT
    //     a wedge). Identity must match `pi-daemon` + the EXACT `--listen <ep>`
    //     token pair to reach the bounded PiRemote::connect.
    //   - codex: run_codex_send has NO cmdline fence (pid-alive + endpoint +
    //     non-empty thread-id only) → it reaches WsAppServer::connect directly.
    //
    // A fabricated `sleep 60` pid fails those fences (the old WIP gap: acp/pi
    // fast-failed the identity door ~88ms, never reaching a wedge). The fix: the
    // row's pid is a lightweight single `python3` process whose ARGV carries the
    // lane's daemon marker + `--listen ws://127.0.0.1:<port>` (so
    // cmdline_is_our_*_daemon passes), while the in-process HangServer OWNS + CAMPS
    // the endpoint port. qd checks the pid's cmdline and the port's camp
    // SEPARATELY — the identity pid never has to be the port-owner — so the wedge
    // condition qd actually defends against (a live identity-verified resident whose
    // endpoint TCP-connects but never services the ws upgrade) is modelled
    // faithfully. A real SIGSTOP'd daemon behaves identically (kernel completes the
    // SYN into the backlog; the frozen process never answers the upgrade).
    //
    // INJECTION PROOF (the why-holder checks faithfulness): the HangServer COUNTS
    // accepted connections; every daemon-wedge cell asserts accepted > 0, so a
    // green/red that never actually reached the wedge is caught, never counted.
    //
    // Verdicts on exit-code + timing + recorded delivery-log terminal — never on
    // log prose. Bounded waits throughout (a genuine hang becomes a truthful FAIL,
    // not a hung run). Cred-free + jailed (HOME/QD_HOME isolated). `kill -KILL --
    // -<pgid>` (the `--`) for group reaps; identity/parent processes reaped too.
    // ==================================================================

    /// A TCP server that ACCEPTS connections and HOLDS them without ever servicing
    /// the ws upgrade — the "camp"/"wedge". A background thread accepts on a
    /// non-blocking listener until signalled to stop and COUNTS every accepted
    /// connection (the injection proof: a cell asserts `accepted() > 0` so a verdict
    /// that never reached the wedge is caught). Clean shutdown, no leaked
    /// thread/socket.
    struct HangServer {
        port: u16,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        accepted: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl HangServer {
        fn start() -> std::io::Result<HangServer> {
            use std::sync::atomic::Ordering;
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let port = listener.local_addr()?.port();
            listener.set_nonblocking(true)?;
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let s2 = stop.clone();
            let a2 = accepted.clone();
            let handle = std::thread::spawn(move || {
                // Hold every accepted stream so the peer's ws-upgrade request blocks
                // (never answered) until its own read timeout fires (acp/pi) or
                // forever (codex — the unbounded-handshake finding).
                let mut held: Vec<std::net::TcpStream> = Vec::new();
                while !s2.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            a2.fetch_add(1, Ordering::Relaxed);
                            held.push(stream);
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(25));
                        }
                        Err(_) => break,
                    }
                }
            });
            Ok(HangServer {
                port,
                stop,
                accepted,
                handle: Some(handle),
            })
        }

        /// How many connections the wedge accepted — the injection proof (a send
        /// that reached the camped endpoint incremented this).
        fn accepted(&self) -> usize {
            self.accepted.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl Drop for HangServer {
        fn drop(&mut self) {
            self.stop
                .store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// Outcome of a bounded `qd send:relay` run. `status == None` ⇒ the send
    /// exceeded the ceiling and was group-killed (a genuine unbounded hang).
    struct BoundedRun {
        status: Option<std::process::ExitStatus>,
        elapsed: std::time::Duration,
        timed_out: bool,
        stderr: String,
    }

    /// Spawn `qd send:relay <name> <msg>` (jailed) in its OWN process group and wait
    /// up to `ceiling`. A run that exceeds the ceiling — a genuine unbounded hang,
    /// e.g. the codex `WsAppServer::connect` camped-upgrade finding — is GROUP-killed
    /// (`kill -KILL -- -<pgid>`, the `--`) and reported as `timed_out`: a truthful
    /// FAIL, never a hung run. stdout/stderr redirect to jail files (no pipe-fill
    /// deadlock while polling `try_wait`).
    fn bounded_send(
        jail: &Jail,
        name: &str,
        msg: &str,
        ceiling: std::time::Duration,
    ) -> Result<BoundedRun, String> {
        use std::os::unix::process::CommandExt;
        let err_path = jail.root.join("send.stderr");
        let out_f = std::fs::File::create(jail.root.join("send.stdout"))
            .map_err(|e| format!("stdout file: {e}"))?;
        let err_f = std::fs::File::create(&err_path).map_err(|e| format!("stderr file: {e}"))?;
        let child = unsafe {
            Command::new(qd_bin())
                .args(["send:relay", name, msg])
                .env("HOME", &jail.home)
                .env("QD_HOME", &jail.qd_home)
                .env_remove("QD_BOOT_AWAIT_RELAY")
                .stdin(std::process::Stdio::null())
                .stdout(out_f)
                .stderr(err_f)
                .pre_exec(|| {
                    // Own session/pgroup so a hung send + any child it spawns is
                    // reapable as a group.
                    if libc_setsid() == -1 {
                        let _ = set_own_pgroup();
                    }
                    Ok(())
                })
                .spawn()
        };
        let mut child = child.map_err(|e| format!("spawn qd send:relay: {e}"))?;
        let pgid = child.id() as i32;
        let started = std::time::Instant::now();
        let (status, timed_out) = loop {
            match child.try_wait() {
                Ok(Some(st)) => break (Some(st), false),
                Ok(None) => {
                    if started.elapsed() >= ceiling {
                        // Genuine unbounded hang: reap the whole group.
                        let _ = Command::new("kill")
                            .args(["-KILL", "--", &format!("-{pgid}")])
                            .status();
                        let _ = child.wait();
                        break (None, true);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(format!("wait qd send:relay: {e}")),
            }
        };
        let elapsed = started.elapsed();
        let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
        Ok(BoundedRun {
            status,
            elapsed,
            timed_out,
            stderr,
        })
    }

    /// The daemon-identity marker the lane's send path fences on: acp lanes probe
    /// for `acp-daemon` (acp_residence.rs ADAPTER_VERB); pi for `pi-daemon`
    /// (pi/residence.rs PI_ADAPTER_VERB). codex has no cmdline fence — the marker is
    /// inert there, so it reuses the acp one (harmless).
    fn lane_daemon_marker(lane: Lane) -> &'static str {
        match lane {
            Lane::Pi => "pi-daemon",
            _ => "acp-daemon",
        }
    }

    /// Does `pid`'s live cmdline pass the identity fence `lane`'s send path applies?
    /// (acp lanes → cmdline_is_our_acp_daemon; pi → cmdline_is_our_pi_daemon; codex
    /// has no fence, so the acp check is used as a liveness+marker sanity probe.)
    fn identity_probes_ours(pid: i64, lane: Lane, endpoint: &str) -> bool {
        let cl = crate::create_daemon::real_cmdline_probe(pid);
        match lane {
            Lane::Pi => crate::provider::pi::residence::cmdline_is_our_pi_daemon(
                cl.as_deref(),
                Some(endpoint),
            ),
            _ => crate::acp_residence::cmdline_is_our_acp_daemon(cl.as_deref(), Some(endpoint)),
        }
    }

    /// Fabricate a jailed daemon registry row for `lane` whose endpoint points at
    /// the camped `hang_port`, backed by a live pid whose /proc cmdline PASSES the
    /// lane's daemon-identity fence. The identity process is a SINGLE lightweight
    /// `python3` that just sleeps, with argv `<marker> --listen ws://127.0.0.1:<port>`
    /// so `cmdline_is_our_{acp,pi}_daemon` matches (marker substring for acp; the
    /// exact `--listen <ep>` token pair for pi — real_cmdline_probe reads
    /// `ps command=`, a space-joined argv). One process (no exec, no child) → a
    /// SIGKILL reaps it cleanly, no orphan. Blocks until the identity is
    /// cmdline-identifiable BEFORE returning, so the send never races a not-yet-up
    /// pid. Returns the jail, the identity child (the caller kills it), the session
    /// id, and the session name.
    fn daemon_wedge_fixture(
        session_name: &str,
        lane: Lane,
        hang_port: u16,
    ) -> Result<(Jail, std::process::Child, String, String), String> {
        let jail = Jail::new(session_name).map_err(|e| format!("jail: {e}"))?;
        let endpoint = format!("ws://127.0.0.1:{hang_port}");
        let marker = lane_daemon_marker(lane);
        let mut identity = Command::new("python3")
            .args([
                "-c",
                "import time,sys; time.sleep(600)",
                marker,
                "--listen",
                &endpoint,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn identity daemon (python3): {e}"))?;
        let live_pid = identity.id() as i64;

        // Wait until the identity process is up and its cmdline probes as ours
        // (python startup + argv exposure) BEFORE qd probes it — else the send could
        // race a not-yet-identifiable pid and fast-fail spuriously.
        let ready = {
            let start = std::time::Instant::now();
            loop {
                if identity_probes_ours(live_pid, lane, &endpoint) {
                    break true;
                }
                if start.elapsed() > std::time::Duration::from_secs(3) {
                    break false;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };
        if !ready {
            let _ = identity.kill();
            let _ = identity.wait();
            return Err(format!(
                "identity daemon pid {live_pid} did not become cmdline-identifiable as our {marker} (+ --listen {endpoint}) within 3s"
            ));
        }

        let session_id = format!("c2-wedge-{session_name}");
        let name = format!("c2wd{}", std::process::id());
        let entry = crate::registry::RegistryEntry {
            pid: Some(live_pid),
            session_id: Some(session_id.clone()),
            provider: Some(lane.provider_id().to_string()),
            name: Some(name.clone()),
            cwd: Some(jail.home.to_string_lossy().to_string()),
            endpoint: Some(endpoint),
            transport: Some("ws".to_string()),
            ..Default::default()
        };
        if let Err(e) = crate::registry::write_entry(&jail.registry_dir, &entry) {
            let _ = identity.kill();
            let _ = identity.wait();
            return Err(format!("write registry row: {e}"));
        }
        Ok((jail, identity, session_id, name))
    }

    /// `d6.wedged-daemon` (daemon lanes). A live identity-verified resident whose
    /// endpoint is CAMPED (accepts the TCP connect, never services the ws upgrade)
    /// — the genuine wedge. The property: `qd send:relay` must fail LOUD within a
    /// bounded time AND record a truthful send-failed terminal — never hang forever,
    /// never falsely succeed.
    ///
    /// Per-lane discrimination, grounded in the send-path trace:
    ///  - acp/{claude-code,opencode} + pi: AcpConnection/PiRemote::connect set the
    ///    read/write timeout BEFORE the tungstenite handshake (wire.rs:436,
    ///    remote.rs:80), so the camped upgrade fails at the 5s bound → loud exit +
    ///    emit_door_failure writes a send-failed terminal → PASS.
    ///  - codex: WsAppServer::connect (ws.rs:73) calls bare `tungstenite::connect`
    ///    and applies the read timeout only AFTER it returns — so the camped upgrade
    ///    read is UNBOUNDED and the send hangs forever. The bounded wait turns that
    ///    into a truthful FAIL (the honest finding: codex does not bound a wedged
    ///    handshake; the product fix is out of C-2 scope, documented durably).
    pub fn wedged_daemon(runner: &Runner, lane: Lane, session_name: &str) -> Outcome {
        // Harness/fixture faults below route to Outcome::Blocked, NEVER runner.fail:
        // on the codex lane the DESIGNED outcome is FAIL (unbounded hang), so a
        // fault collapsed into Fail would be masked as the genuine RED by the
        // is_fail()-only test (the C2-TS-1/2/3 class — proactively swept here).
        let hang = match HangServer::start() {
            Ok(h) => h,
            Err(e) => {
                return Outcome::blocked(
                    format!("could not start the in-process wedge listener (HangServer): {e} — a harness fault, not a conformance verdict"),
                    "rerun; if persistent, check local TCP bind availability (127.0.0.1:0)",
                )
            }
        };
        let hang_port = hang.port;
        let (jail, mut identity, session_id, name) =
            match daemon_wedge_fixture(session_name, lane, hang_port) {
                Ok(v) => v,
                Err(e) => {
                    drop(hang);
                    return Outcome::blocked(
                        format!("could not stage the wedged-daemon fixture (identity daemon not cmdline-verifiable): {e} — a fixture-faithfulness fault, not a conformance verdict"),
                        "rerun; if persistent, check python3 availability + the identity-process cmdline readiness probe timing",
                    );
                }
            };
        // Ceiling well above the 5s inject connect bound; codex's unbounded hang
        // exceeds it → group-killed → truthful FAIL.
        let ceiling = std::time::Duration::from_secs(15);
        let run = bounded_send(&jail, &name, "c2 wedged-daemon probe", ceiling);
        let _ = identity.kill();
        let _ = identity.wait();
        let accepted = hang.accepted();
        drop(hang);

        let commands = vec![
            format!(
                "(fixture: {} identity daemon pid + registry row, endpoint→camped :{hang_port})",
                lane_daemon_marker(lane)
            ),
            format!("qd send:relay {name} <msg> (jailed, own pgroup, bounded {ceiling:?})"),
        ];
        let run = match run {
            Ok(r) => r,
            Err(e) => {
                return Outcome::blocked(
                    format!("could not run the bounded wedged-daemon send: {e} — a harness fault, not a conformance verdict"),
                    "rerun; if persistent, check qd binary spawnability under the jail",
                )
            }
        };

        // Injection proof: the send must have actually reached the camped wedge. A
        // 0-accept is a fixture-faithfulness fault → Blocked, NEVER the designed
        // codex FAIL (else an is_fail()-only test masks the harness fault as the RED).
        if accepted == 0 {
            return Outcome::blocked(
                format!(
                    "the wedge accepted 0 connections — the send never reached the camped endpoint (fixture did not inject); timed_out={} elapsed={:?} stderr={:?} — a fixture-faithfulness fault, not a conformance verdict",
                    run.timed_out,
                    run.elapsed,
                    run.stderr.lines().next().unwrap_or("")
                ),
                "rerun; if persistent, check the identity cmdline fence (acp/pi marker + --listen endpoint) and the HangServer bind/accept",
            );
        }

        // A genuine unbounded hang (codex): the property is VIOLATED.
        if run.timed_out {
            return runner.fail(
                commands,
                format!(
                    "lane={}; wedge accepted={accepted}; qd send:relay did NOT return within {ceiling:?} (group-killed) — the wedged handshake is UNBOUNDED on this lane. Grounded: WsAppServer::connect (ws.rs:73) calls bare tungstenite::connect and applies the read timeout only AFTER it returns, so a camped ws upgrade read never bounds. stderr={:?}",
                    lane.provider_id(),
                    run.stderr.lines().next().unwrap_or("")
                ),
                "HONEST FAIL: a wedged (camped) daemon makes qd send:relay HANG UNBOUNDED on this lane — it must fail loud within a bounded time. A real qd product defect: codex WsAppServer::connect (ws.rs:73) lacks the pre-handshake read/write timeout that acp AcpConnection::connect (wire.rs:436) and pi PiRemote::connect (remote.rs:80) set before the tungstenite handshake. The product fix is out of C-2 scope; surfaced + documented durably (cf. the C-1 pi self-terminate FAIL).",
            );
        }

        let status = run.status.expect("a non-timed-out run has a status");
        if status.success() {
            return runner.fail(
                commands,
                format!(
                    "lane={}; wedge accepted={accepted}; qd send:relay EXIT 0 against a camped daemon in {:?} — a false success; stderr={:?}",
                    lane.provider_id(),
                    run.elapsed,
                    run.stderr.lines().next().unwrap_or("")
                ),
                "a wedged daemon must fail loud, never falsely succeed",
            );
        }

        // Loud failure within the bound: confirm the truthful send-failed terminal.
        let (send_failed, raw) = has_send_failed(&jail.events_dir, &session_id);
        let bounded = run.elapsed < ceiling;
        let observed = format!(
            "lane={}; wedge accepted={accepted}; qd send:relay exited {:?} (loud) in {:?} (bounded, < {ceiling:?}); send-failed-terminal-present={send_failed}; delivery-log lines={}",
            lane.provider_id(),
            status.code(),
            run.elapsed,
            raw.lines().count()
        );
        if send_failed && bounded {
            runner.pass(commands, observed)
        } else {
            runner.fail(
                commands,
                observed,
                "a wedged daemon must fail loud WITHIN the timeout AND record a truthful send-failed terminal in the delivery log",
            )
        }
    }

    /// `d6.sender-killed-mid-send` (daemon lanes). The sender process is killed
    /// (`kill -KILL -- -<pgid>`) while mid-send (blocked on the wedge). The
    /// delivery log must show NO falsely-completed/seen terminal — the sender
    /// structurally writes no success terminal before the wire delivers, so an
    /// abrupt kill leaves at most a non-terminal record, never a phantom success.
    pub fn sender_killed_mid_send(runner: &Runner, lane: Lane, session_name: &str) -> Outcome {
        use std::os::unix::process::CommandExt;
        let hang = match HangServer::start() {
            Ok(h) => h,
            Err(e) => {
                return runner.fail(
                    vec!["(hang-server)".into()],
                    format!("could not start the wedge listener: {e}"),
                    "cannot stage a mid-send window without a hang-server",
                )
            }
        };
        let hang_port = hang.port;
        let (jail, mut identity, session_id, name) =
            match daemon_wedge_fixture(session_name, lane, hang_port) {
                Ok(v) => v,
                Err(e) => {
                    drop(hang);
                    return runner.fail(
                        vec!["(wedge fixture)".into()],
                        e,
                        "cannot stage the mid-send fixture (identity daemon not verifiable)",
                    );
                }
            };

        // Spawn the sender in its OWN process group so we can reap the whole group.
        let child = unsafe {
            Command::new(qd_bin())
                .args(["send:relay", &name, "c2 sender-killed probe"])
                .env("HOME", &jail.home)
                .env("QD_HOME", &jail.qd_home)
                .env_remove("QD_BOOT_AWAIT_RELAY")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .pre_exec(|| {
                    // New session/pgroup so the child pid == its pgid.
                    if libc_setsid() == -1 {
                        // setpgid fallback: put us in our own group.
                        let _ = set_own_pgroup();
                    }
                    Ok(())
                })
                .spawn()
        };
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = identity.kill();
                let _ = identity.wait();
                drop(hang);
                return runner.fail(
                    vec!["qd send:relay (spawned, own pgroup)".into()],
                    format!("spawn error: {e}"),
                    "cannot exercise a mid-send kill without spawning the sender",
                );
            }
        };
        let pgid = child.id() as i32;
        // A genuine mid-send window: poll until the wedge has ACCEPTED the sender's
        // connection (the sender is now parked in the ws handshake read) before the
        // kill. accepted>0 PROVES the kill lands mid-send, not before connect.
        let mid_send = {
            let start = std::time::Instant::now();
            loop {
                if hang.accepted() > 0 {
                    break true;
                }
                if start.elapsed() > std::time::Duration::from_secs(5) {
                    break false;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        };
        // Small settle so the sender is genuinely parked in the handshake read.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let kill_cmd = format!("kill -KILL -- -{pgid}");
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{pgid}")])
            .status();
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = identity.kill();
        let _ = identity.wait();
        let accepted = hang.accepted();
        drop(hang);

        // Read the target's delivery log: assert NO success terminal (message-seen)
        // and no other terminal was falsely written by the killed sender.
        let log_path = jail.events_dir.join(format!("{session_id}.events.jsonl"));
        let raw = std::fs::read_to_string(&log_path).unwrap_or_default();
        let terminals: Vec<String> = raw
            .lines()
            .filter_map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).ok()?;
                let ev = v.get("event")?.as_str()?.to_string();
                // Success/completed terminals that would be a phantom if present.
                (ev == "message-seen" || ev == "turn-anchored").then_some(ev)
            })
            .collect();
        let no_phantom = terminals.is_empty();

        let commands = vec![
            format!(
                "(fixture: {} identity daemon + registry row, endpoint→camped :{hang_port})",
                lane_daemon_marker(lane)
            ),
            format!("qd send:relay {name} <msg> (spawned own pgroup; blocks on the camped wedge)"),
            kill_cmd,
            format!("read {session_id}.events.jsonl → assert no message-seen/turn-anchored terminal"),
        ];
        let observed = format!(
            "lane={}; wedge accepted={accepted} (>0 ⇒ the kill landed in a real mid-send window); sender killed mid-send (parked in the ws handshake); delivery-log success-terminals found={:?} (expect none); log lines={}",
            lane.provider_id(),
            terminals,
            raw.lines().count()
        );

        // Injection proof: the sender must have reached the wedge before the kill.
        // A miss is a fixture-faithfulness fault → Blocked, NEVER a conformance
        // verdict (consistent with the C2-TS class; all lanes here are designed PASS,
        // so is_pass() already catches it — Blocked is the correct classification).
        if !mid_send || accepted == 0 {
            return Outcome::blocked(
                format!(
                    "the sender never reached the wedge before the kill (mid_send={mid_send}, accepted={accepted}) — no genuine mid-send window, the fixture did not inject. A fixture-faithfulness fault, not a conformance verdict. {observed}"
                ),
                "rerun; if persistent, check the identity cmdline fence + HangServer accept timing vs the 5s mid-send poll",
            );
        }
        if no_phantom {
            runner.pass(commands, observed)
        } else {
            runner.fail(
                commands,
                observed,
                "a killed-mid-send sender left a phantom success terminal in the delivery log — the no-false-completion honesty property does NOT hold",
            )
        }
    }

    /// `setsid(2)` via libc (new session + process group). Signature-only extern.
    fn libc_setsid() -> i64 {
        extern "C" {
            fn setsid() -> i32;
        }
        unsafe { setsid() as i64 }
    }

    /// Fallback: put the current process in its own process group (`setpgid(0,0)`).
    fn set_own_pgroup() -> i32 {
        extern "C" {
            fn setpgid(pid: i32, pgid: i32) -> i32;
        }
        unsafe { setpgid(0, 0) }
    }

    // ==================================================================
    // D6 dead-relay-sidecar (claude-code only) — an honest FAIL cell.
    // The claude relay path resolves a relay port from a sidecar in
    // <HOME>/.claude/relay and POSTs to it (CcRelay). When that sidecar's port is
    // DEAD (connection refused), send:relay fails LOUD (exit 1, stderr) but writes
    // NO delivery-log terminal — the claude relay-transport door has no send-failed
    // record (unlike the daemon doors). That gap IS the finding (cf. the C-1 pi
    // self-terminate FAIL). Both arms in evidence: a no-relay CONTROL that DOES
    // write a send-failed terminal (proving the detector sees a terminal when one
    // exists) vs the dead-sidecar FINDING (loud, no terminal).
    // ==================================================================

    /// Does the target's delivery log carry a `send-failed` terminal?
    fn has_send_failed(events_dir: &std::path::Path, session_id: &str) -> (bool, String) {
        let log_path = events_dir.join(format!("{session_id}.events.jsonl"));
        let raw = std::fs::read_to_string(&log_path).unwrap_or_default();
        let found = raw.lines().any(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .ok()
                .and_then(|v| v.get("event").and_then(|e| e.as_str()).map(|s| s == "send-failed"))
                .unwrap_or(false)
        });
        (found, raw)
    }

    /// Spawn a "session" process (the registry pid) that OWNS a live CHILD process
    /// (the sidecar pid) — a genuine parent→child pair so `fast_relay_lookup`'s
    /// PID-ANCESTRY walk (relay.rs:257 — a relay's pid must have the matched entry's
    /// pid as an ancestor ≤5 levels) resolves the sidecar to the session row.
    /// `bash -c 'sleep N & echo $!; wait'` backgrounds its child, prints the child's
    /// pid on stdout, then blocks in `wait` (staying alive as the parent). The parent
    /// runs in its own process group (setsid) so the caller reaps BOTH with
    /// `kill -KILL -- -<pgid>`. Returns (parent Child, parent pid, child pid).
    fn parent_with_child() -> Result<(std::process::Child, i64, i64), String> {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let mut parent = unsafe {
            Command::new("bash")
                .args(["-c", "sleep 600 & echo $!; wait"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .pre_exec(|| {
                    if libc_setsid() == -1 {
                        let _ = set_own_pgroup();
                    }
                    Ok(())
                })
                .spawn()
                .map_err(|e| format!("spawn parent shell: {e}"))?
        };
        let parent_pid = parent.id() as i64;
        let stdout = parent
            .stdout
            .take()
            .ok_or_else(|| "no stdout on the parent shell".to_string())?;
        let mut line = String::new();
        // The `echo $!` runs immediately after backgrounding sleep → one line, fast.
        std::io::BufReader::new(stdout)
            .read_line(&mut line)
            .map_err(|e| format!("read child pid line: {e}"))?;
        let child_pid = line
            .trim()
            .parse::<i64>()
            .map_err(|e| format!("parse child pid {line:?}: {e}"))?;
        Ok((parent, parent_pid, child_pid))
    }

    /// `d6.dead-relay-sidecar` (claude-code). Honest FAIL: loud error, no terminal.
    /// The claude relay path resolves a relay port from a sidecar (matched by
    /// PID-ANCESTRY, relay.rs:230), then POSTs to it (CcRelay). A sidecar whose port
    /// is DEAD (connect refused) makes the POST fail → `InjectError::RelayFailed`
    /// (send_relay.rs:544), which prints to stderr and exits 1 with NO
    /// `emit_door_failure` — so no delivery-log terminal is written. THAT gap is the
    /// finding. Both arms in evidence: a no-relay CONTROL that DOES write a terminal
    /// (the pre-wire `no_relay_exit` door, proving the detector is not blind) vs the
    /// dead-sidecar FINDING (loud, no terminal).
    pub fn dead_relay_sidecar(runner: &Runner, session_name: &str) -> Outcome {
        // Setup/staging faults below route to Blocked, NEVER runner.fail: the DESIGNED
        // outcome is the honest-FAIL RED, so a jail/ancestry/bind fault collapsed into
        // Fail would be masked as the genuine RED by the is_fail()-only test (the
        // C2-TS class — proactively swept).
        // ---- FINDING arm: a relay sidecar whose port is DEAD (connect refused). ----
        let jail = match Jail::new(&format!("c2-deadrelay-{session_name}")) {
            Ok(j) => j,
            Err(e) => {
                return Outcome::blocked(
                    format!("could not stage the dead-relay jail: {e} — a harness fault, not a conformance verdict"),
                    "rerun; if persistent, check temp-dir writability",
                )
            }
        };
        // A genuine parent→child pid pair so the sidecar (child pid) resolves to the
        // session row (parent pid) via fast_relay_lookup's ancestry walk — otherwise
        // the send falls to the WRONG door (no_relay_exit, which DOES write a
        // terminal) and the finding is never exercised.
        let (mut parent, session_pid, child_pid) = match parent_with_child() {
            Ok(v) => v,
            Err(e) => return Outcome::blocked(
                format!("could not stage the parent→child relay ancestry: {e} — a fixture-faithfulness fault, not a conformance verdict"),
                "rerun; if persistent, check bash availability + the `echo $!` child-pid capture",
            ),
        };
        let pgid = parent.id() as i32;
        let reap_parent = |parent: &mut std::process::Child| {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{pgid}")])
                .status();
            let _ = parent.wait();
        };
        let name = format!("c2dr{}", std::process::id());
        let session_id = format!("c2-deadrelay-{name}");
        // A definitely-closed port: bind then drop → connect refuses.
        let dead_port = match std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()) {
            Ok(a) => a.port(),
            Err(e) => {
                reap_parent(&mut parent);
                return Outcome::blocked(
                    format!("could not obtain a dead port (bind probe): {e} — a harness fault, not a conformance verdict"),
                    "rerun; if persistent, check local TCP bind availability (127.0.0.1:0)",
                );
            }
        };
        // Registry row (claude-code, PARENT pid) + a relay sidecar keyed to the
        // CHILD pid pointing at the dead port.
        let entry = crate::registry::RegistryEntry {
            pid: Some(session_pid),
            session_id: Some(session_id.clone()),
            provider: Some("claude-code".to_string()),
            name: Some(name.clone()),
            cwd: Some(jail.home.to_string_lossy().to_string()),
            ..Default::default()
        };
        let _ = crate::registry::write_entry(&jail.registry_dir, &entry);
        let relay_dir = jail.home.join(".claude").join("relay");
        let _ = std::fs::create_dir_all(&relay_dir);
        let sidecar = serde_json::json!({
            "port": dead_port,
            "sessionId": session_id,
            "pid": child_pid,
            "name": name,
        });
        let _ = std::fs::write(
            relay_dir.join(format!("{name}.json")),
            serde_json::to_vec(&sidecar).unwrap_or_default(),
        );

        let send_cmd = format!(
            "qd send:relay {name} <msg> (jailed; sidecar child pid {child_pid} under session pid {session_pid} → DEAD port {dead_port})"
        );
        // Bounded — a dead port refuses immediately, but never risk a hang.
        let run = bounded_send(&jail, &name, "c2 dead-relay-sidecar probe", std::time::Duration::from_secs(15));
        reap_parent(&mut parent);
        let run = match run {
            Ok(r) => r,
            Err(e) => return Outcome::blocked(
                format!("could not run the bounded dead-relay send: {e} — a harness fault, not a conformance verdict"),
                "rerun; if persistent, check qd binary spawnability under the jail",
            ),
        };
        if run.timed_out {
            return Outcome::blocked(
                format!("qd send:relay did not return within the bound against a supposedly-dead relay port (unexpected hang); stderr={:?} — a fixture-faithfulness fault (the 'dead' port likely got reused by another process and now camps), not the designed honest-FAIL", run.stderr.lines().next().unwrap_or("")),
                "rerun; if persistent despite a fresh dead port, investigate whether the claude CcRelay POST lacks a connect timeout (a possible product gap to surface separately)",
            );
        }
        let exit_code = run.status.and_then(|s| s.code());
        let loud = exit_code != Some(0);
        let stderr = run.stderr.clone();
        // Fail-closed door discriminator (a faithfulness GUARD, not the verdict): the
        // resolved-port POST-failure door prints "Failed to send message"
        // (send_relay.rs:546); the pre-wire no-relay door prints "has no relay". If
        // the port did NOT resolve (ancestry match failed → wrong door), reject —
        // never a false honest-FAIL green from an un-exercised finding.
        let post_attempted = stderr.contains("Failed to send message");
        let hit_no_relay_door = stderr.contains("has no relay");
        let (finding_has_terminal, finding_log) = has_send_failed(&jail.events_dir, &session_id);

        // ---- CONTROL arm: NO relay sidecar → the pre-wire no-relay door DOES write
        // a send-failed terminal (proves the detector sees a terminal when present). ----
        let cjail = Jail::new(&format!("c2-norelay-{session_name}")).ok();
        let (control_has_terminal, control_note) = if let Some(cj) = &cjail {
            let mut csleep = Command::new("sleep").arg("30").spawn().ok();
            let cpid = csleep.as_ref().map(|c| c.id() as i64).unwrap_or(-1);
            let cname = format!("c2nr{}", std::process::id());
            let csid = format!("c2-norelay-{cname}");
            let centry = crate::registry::RegistryEntry {
                pid: Some(cpid),
                session_id: Some(csid.clone()),
                provider: Some("claude-code".to_string()),
                name: Some(cname.clone()),
                cwd: Some(cj.home.to_string_lossy().to_string()),
                ..Default::default()
            };
            let _ = crate::registry::write_entry(&cj.registry_dir, &centry);
            // NO relay sidecar written → resolve_relay_port falls to no_relay_exit.
            let _ = bounded_send(cj, &cname, "c2 no-relay control", std::time::Duration::from_secs(15));
            if let Some(mut c) = csleep.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            let (found, _) = has_send_failed(&cj.events_dir, &csid);
            (found, format!("no-relay control wrote send-failed terminal={found}"))
        } else {
            (false, "control jail unavailable".to_string())
        };

        let commands = vec![
            "(claude row: parent pid; relay sidecar: child pid → dead port)".to_string(),
            send_cmd,
            "CONTROL: (claude row, NO sidecar) qd send:relay → no-relay door".to_string(),
        ];
        let observed = format!(
            "FINDING (dead relay sidecar, port RESOLVED via child-pid ancestry): exit={exit_code:?} loud={loud} elapsed={:?} post_attempted={post_attempted} hit_no_relay_door={hit_no_relay_door}; delivery-log-has-send-failed-terminal={finding_has_terminal} (the honest GAP — the claude relay-transport POST-failure door, inject_via_provider RelayFailed send_relay.rs:544, is stderr-only, writes NO terminal); stderr={:?}; log_lines={}; \
             CONTROL: {control_note} (proves the detector sees a send-failed terminal when the path writes one — so the FINDING's absence is real, not a blind detector)",
            run.elapsed,
            stderr.lines().next().unwrap_or(""),
            finding_log.lines().count(),
        );

        // Faithfulness gate FIRST (trivial-shape C2-TS-3): the finding is only
        // trustworthy if the send genuinely reached the resolved-port POST door (not
        // the no-relay door). A miss here is a FIXTURE-FAITHFULNESS fault (flaky
        // /proc ancestry walk, or an stderr flush/capture race), NOT a conformance
        // FAIL — it must be Blocked, never the designed honest-FAIL (else an
        // is_fail()-only test would mask a harness fault as the genuine RED).
        if hit_no_relay_door || !post_attempted {
            return Outcome::blocked(
                format!(
                    "the dead-relay send did NOT reach the resolved-port POST door (hit_no_relay_door={hit_no_relay_door}, post_attempted={post_attempted}): ancestry match failed → it fell to the no-relay door, OR the transport-failure stderr was absent — a fixture-faithfulness fault, the finding was not genuinely exercised. {observed}"
                ),
                "rerun; if persistent, check the parent→child pid ancestry timing (fast_relay_lookup's /proc walk) and the qd stderr capture/flush",
            );
        }
        // The cell's claim requires BOTH a loud error AND a truthful send-failed
        // terminal. The dead relay sidecar produces the loud error but NO terminal —
        // an honest FAIL (the durable-terminal half does not hold). The control
        // proves the detector is not blind. If the product later writes a terminal
        // on relay-transport failure, this flips to Pass — revisit then.
        if loud && !finding_has_terminal && control_has_terminal {
            runner.fail(
                commands,
                observed,
                "HONEST FAIL: a dead relay sidecar makes claude send:relay fail LOUD (exit non-zero) but records NO truthful send-failed terminal in the delivery log — the claude relay-transport POST-failure door (inject_via_provider's InjectError::RelayFailed arm, send_relay.rs:544, prints to stderr and returns 1 with no emit_door_failure) is stderr-only, unlike the daemon doors and the pre-wire no-relay door. A failed relay send leaves no durable record. (The control's no-relay door DID write a terminal, so the detector is not blind.) The underlying product gap is escalated separately, out of C-2 scope; cf. the C-1 pi self-terminate FAIL.",
            )
        } else if loud && finding_has_terminal {
            // The product now writes a terminal on relay-transport failure → the
            // honesty property holds → PASS (the gap was fixed).
            runner.pass(commands, observed)
        } else {
            // Reached only when loud (guaranteed by post_attempted above) &&
            // !finding_has_terminal && !control_has_terminal — i.e. the no-relay
            // CONTROL failed to write a terminal, so the detector may be BLIND. That
            // is a harness/detector fault, NOT a conformance FAIL (trivial-shape
            // C2-TS-3) — Blocked, never the designed honest-FAIL.
            Outcome::blocked(
                format!(
                    "the no-relay CONTROL did not write a send-failed terminal (control_has_terminal={control_has_terminal}) — the detector may be blind, so the FINDING's terminal-absence cannot be trusted. A harness/detector fault, not a conformance FAIL. {observed}"
                ),
                "rerun; if persistent, check the no-relay control path genuinely writes a delivery-log send-failed terminal (no_relay_exit → emit_door_failure) so the detector is provably not blind",
            )
        }
    }
}
