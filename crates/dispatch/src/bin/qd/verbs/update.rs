//! REAL `sb update` backend (A5 spec §4.3, fresh-design divergence). Thin
//! binding over the pure [`dispatch::update`] library: resolve the running exe path +
//! the brew/cargo channel hints, decide the channel, print the `[update]`
//! report, and exec the resolved argv (inherit stdio). Exit 0/1 ONLY (ADR 0008):
//! a real channel inherits the child's exit (clamped to 0/1); an undeterminable
//! channel is exit 1.

use std::path::{Path, PathBuf};

use dispatch::effects::{Env, RealEnv};
use dispatch::exec::{Exec, RealExec};
use dispatch::update::{decide_update_action, run_update, UpdateAction, UpdateExec};

/// The workspace repository, taken from the COMPILED manifest so the argv repo
/// never drifts from Cargo.toml's `repository` field.
const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// Real update exec: spawn argv with inherited stdio via [`RealExec`].
struct RealUpdateExec<'a>(&'a RealExec);
impl UpdateExec for RealUpdateExec<'_> {
    fn run_inherit(&self, argv: &[String]) -> i32 {
        if argv.is_empty() {
            return 1;
        }
        let (cmd, rest) = argv.split_first().unwrap();
        self.0.spawn_inherit(cmd, rest, &[], None).unwrap_or(1)
    }
}

/// `sb update` — self-update via the detected install channel (Homebrew/cargo).
pub fn run() -> i32 {
    let env = RealEnv;
    let exec = RealExec;

    // Resolve the running exe path. On failure we cannot determine the channel →
    // surface the same Unknown guidance (exit 1).
    let exe_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => {
            eprintln!(
                "sb update: cannot determine install channel (expected Homebrew or cargo); \
                 reinstall manually from {REPO_URL}."
            );
            return 1;
        }
    };

    // brew --prefix (best-effort; absent brew → None, leaves the Cellar-segment
    // heuristic as the sole Homebrew signal).
    let brew_prefix = brew_prefix(&exec);

    // ~/.cargo/bin: CARGO_HOME/bin if set, else HOME/.cargo/bin.
    let cargo_bin = cargo_bin(&env);

    let action = decide_update_action(
        &exe_path,
        brew_prefix.as_deref(),
        cargo_bin.as_deref(),
        REPO_URL,
    );

    let runner = RealUpdateExec(&exec);
    let outcome = run_update(action, &runner);

    // Print the (pre-exec) report lines that the lib produced. For a real
    // channel the child's stdout already streamed (inherit); we add the final
    // status line here.
    for line in &outcome.report {
        match &outcome.action {
            // Unknown's single line is an error → stderr.
            UpdateAction::Unknown { .. } => eprintln!("{line}"),
            _ => println!("{line}"),
        }
    }
    match &outcome.action {
        UpdateAction::Unknown { .. } => {} // message already printed above
        _ => {
            if outcome.exit_code == 0 {
                // RELAY-PATH HARDENING (re-point on update): an upgrade replaces
                // the binary and can change the path/inode it lives at (a new brew
                // Cellar version dir; a re-installed cargo bin). Claude Code spawns
                // the relay via the ABSOLUTE PATH stored in `~/.claude.json`, so a
                // path change there orphans every new session's relay until
                // something re-points it. After a SUCCESSFUL update we re-point the
                // (already-registered) relay at the running binary — idempotent,
                // best-effort, and consent-consistent: we only CORRECT an existing
                // registration, never create one the user never asked for.
                repoint_relay_after_update(&env, &exec);
                println!("[update] done.");
            } else {
                eprintln!(
                    "[update] ERROR: update command exited {} — sb was not updated.",
                    outcome.exit_code
                );
            }
        }
    }

    // Exit 0/1 only (ADR 0008): clamp any non-zero child exit to 1.
    if outcome.exit_code == 0 {
        0
    } else {
        1
    }
}

/// Repair the relay MCP registration after a successful update (relay-path
/// hardening v2). Best-effort + non-fatal: never changes the update's exit code.
/// CONSENT-CONSISTENT — it only ever touches an EXISTING registration; an
/// unregistered relay is left alone (bootstrap is where a user opts in).
///
/// Because we now register the BARE `sb` command (resolved via PATH), a normal
/// update does NOT need to re-point anything — the bare command keeps working
/// across the upgrade. This step therefore acts only as a BACKSTOP: if the box
/// still carries a broken LEGACY absolute-path entry (an absolute path naming a
/// file that no longer exists after the upgrade), it migrates that entry to the
/// bare form. A bare entry, or an absolute path that still exists, is left alone.
/// Requires `claude` on PATH (we drive `claude mcp`); absent → silent no-op.
fn repoint_relay_after_update(env: &RealEnv, exec: &RealExec) {
    use dispatch::bootstrap::real_command_exists;
    use dispatch::relay_server::register;

    // Resolve HOME → `~/.claude.json`; absent → nothing to inspect.
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        return;
    };
    let claude_json_path = Path::new(&home).join(".claude.json");
    let Ok(claude_json) = std::fs::read_to_string(&claude_json_path) else {
        return; // no config → relay was never registered here; nothing to repair.
    };

    // Only a BROKEN legacy absolute-path entry warrants a repair. A bare command —
    // or an absolute path that still exists — is valid and left untouched. (Also
    // covers the unregistered case → None → not stale → no-op, preserving consent.)
    if !register::relay_command_is_stale(&claude_json, |c| Path::new(c).exists()) {
        return;
    }

    // We drive `claude mcp`; `claude` must be on PATH.
    if !real_command_exists(exec, "claude") {
        eprintln!(
            "[update] note: relay registration points at a stale `sb` path but `claude` is not \
             on PATH — repair it with: sb relay:repoint"
        );
        return;
    }

    match register::register_relay(exec) {
        Ok(()) => println!(
            "[update] relay registration repaired — re-pointed at the bare `{}` command.",
            register::RELAY_BARE_COMMAND
        ),
        Err(e) => eprintln!("[update] note: relay re-point failed ({e}); run: sb relay:repoint"),
    }
}

/// `brew --prefix` (best-effort): the Homebrew install root, or None if brew is
/// absent / errors. Trims trailing newline.
fn brew_prefix(exec: &RealExec) -> Option<String> {
    let out = exec
        .run("brew", &["--prefix".to_string()], &[], None, Some(5000))
        .ok()?;
    if out.status != Some(0) {
        return None;
    }
    let p = out.stdout.trim().to_string();
    if p.is_empty() {
        None
    } else {
        Some(p)
    }
}

/// `~/.cargo/bin`: `CARGO_HOME/bin` if set, else `HOME/.cargo/bin` (through the
/// env seam; never the real home directly).
fn cargo_bin(env: &dyn Env) -> Option<PathBuf> {
    if let Some(cargo_home) = env.var("CARGO_HOME").filter(|s| !s.is_empty()) {
        return Some(Path::new(&cargo_home).join("bin"));
    }
    env.var("HOME")
        .filter(|s| !s.is_empty())
        .map(|h| Path::new(&h).join(".cargo").join("bin"))
}
