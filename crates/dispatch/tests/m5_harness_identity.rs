//! M5 / T5 — the r3 recommended integration assertion (reachability).
//!
//! A spawned pane's [`qrmux::pty::CommandSpec`], built through the REAL dispatch
//! launch assembly (`dispatch::launch::build_claude_cmd_from_argv`, the exact
//! producer of the `bash -lc` shell_cmd `CreateDetached` carries), classifies to the
//! right [`qrmux::attended::driver::Harness`]. This PINS `Harness::from_command` to
//! the actual assembly across the crate boundary: if the launch assembly ever
//! changes shape, THIS fails loudly rather than silently dropping codex/pi back to
//! `Harness::Default` (verify-blocked) — the reachability regression M4's r3 warned
//! about. The claude/default arm proves the accepted path is byte-for-byte unchanged.

use dispatch::launch::build_claude_cmd_from_argv;
use qrmux::attended::driver::Harness;
use qrmux::pty::CommandSpec;

/// Classify a pane spawned for `bin` + `flags` EXACTLY as create does: provider
/// argv → `build_claude_cmd_from_argv` → `login_shell_c` → the spawned argv.
fn spawned_harness(bin: &str, flags: &[&str]) -> Harness {
    let mut argv = vec![bin.to_string()];
    argv.extend(flags.iter().map(|s| s.to_string()));
    let shell_cmd = build_claude_cmd_from_argv(&argv);
    let spec = CommandSpec::login_shell_c(&shell_cmd);
    Harness::from_command(&spec.argv)
}

/// A pane whose launch is preceded by the self-deleting dot-source env prefix
/// (the F1/--via backend-env block create prepends before the `command` launch).
fn spawned_harness_with_env_prefix(bin: &str) -> Harness {
    let shell_cmd = build_claude_cmd_from_argv(&[bin.to_string(), "--x".to_string()]);
    let prefixed = format!(". '/run/x/qd-session-env-abc'; rm -f '/run/x/qd-session-env-abc'; {shell_cmd}");
    let spec = CommandSpec::login_shell_c(&prefixed);
    Harness::from_command(&spec.argv)
}

#[test]
fn spawned_codex_pane_classifies_codex() {
    assert_eq!(spawned_harness("/home/u/.local/bin/codex", &["--foo", "bar"]), Harness::Codex);
    assert_eq!(spawned_harness("codex", &[]), Harness::Codex);
    assert_eq!(spawned_harness_with_env_prefix("codex"), Harness::Codex);
}

#[test]
fn spawned_pi_pane_classifies_pi() {
    assert_eq!(spawned_harness("/home/u/.local/bin/pi", &["-m", "x"]), Harness::Pi);
    assert_eq!(spawned_harness("pi", &[]), Harness::Pi);
    assert_eq!(spawned_harness_with_env_prefix("pi"), Harness::Pi);
}

#[test]
fn spawned_claude_and_unknown_panes_stay_default_byte_for_byte() {
    // The accepted claude/default path classifies Default through the SAME assembly
    // — the parse never touches it (only exact codex/pi bins are promoted).
    assert_eq!(spawned_harness("/usr/bin/claude", &["--dangerously-x"]), Harness::Default);
    assert_eq!(spawned_harness("claude", &[]), Harness::Default);
    assert_eq!(spawned_harness("fakerepl", &[]), Harness::Default);
    assert_eq!(spawned_harness_with_env_prefix("claude"), Harness::Default);
}
