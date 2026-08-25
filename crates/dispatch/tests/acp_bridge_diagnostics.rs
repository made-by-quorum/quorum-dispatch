//! `qd acp-daemon` bridge-failure DIAGNOSTICS — the end-to-end proof that the two ways an
//! ACP bridge can be broken no longer produce the same sentence.
//!
//! The bug: `AcpHost::spawn` starts the bridge on a background connection thread and returns
//! `Ok` before the spawn has happened, so the thread's perfectly good `spawn <program>: No
//! such file or directory` was dropped on the floor and the boot reported
//! `bridge initialize failed: acp connection closed`. A MISSING bridge and a PRESENT-but-wrong
//! one were byte-identical:
//!
//! ```text
//! --bridge-cmd definitely-not-a-real-binary-xyz  → "bridge initialize failed: acp connection closed"
//! --bridge-cmd /bin/echo                         → "bridge initialize failed: acp connection closed"
//! ```
//!
//! Two repairs are pinned here, at the layer the operator actually meets (the daemon's own
//! stderr, which is what lands in `~/.quorum/dispatch/log/acp-<name>.log`):
//!
//! 1. a missing bridge is refused BEFORE any spawn, by name, with the package that provides
//!    it (`acp_residence::resolve_bridge_program`);
//! 2. a bridge that exists but does not speak ACP is reported as what it is — a child that
//!    ran and exited — because the connection thread's terminal cause is now recorded on the
//!    host and consulted by `initialize`.
//!
//! Hermetic: no bridge is installed, nothing is contacted, no live agent is driven. The two
//! "bridges" are a name that cannot exist and `/bin/echo`.

use std::process::Command;

/// The `qd` binary under test (the same handle `acp_chaos.rs` uses).
fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// Boot `qd acp-daemon` against `bridge_cmd` and return its (exit code, stderr). `--listen`
/// takes port 0 — the OS picks a free one — so concurrent test runs cannot collide, and both
/// failures land long before anything is served on it.
fn boot_with_bridge(bridge_cmd: &str) -> (i32, String) {
    let out = Command::new(qd_bin())
        .args([
            "acp-daemon",
            "--listen",
            "ws://127.0.0.1:0",
            "--cwd",
            "/tmp",
            "--bridge-cmd",
            bridge_cmd,
        ])
        .output()
        .expect("qd acp-daemon runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_missing_bridge_is_refused_by_name_before_anything_is_spawned() {
    let (code, err) = boot_with_bridge("definitely-not-a-real-binary-xyz");
    assert_eq!(code, 1, "a bridge that cannot run is a boot failure: {err}");
    assert!(
        err.contains("definitely-not-a-real-binary-xyz"),
        "the refusal must NAME the program that was not found: {err}"
    );
    assert!(
        err.contains("not on PATH"),
        "…and say where it was looked for: {err}"
    );
    assert!(
        !err.contains("acp connection closed"),
        "the generic closed-connection message is exactly what this replaced: {err}"
    );
}

#[test]
fn a_program_that_is_not_a_bridge_says_the_child_exited() {
    let (code, err) = boot_with_bridge("/bin/echo");
    assert_eq!(code, 1, "a mute bridge is a boot failure: {err}");
    assert!(
        err.contains("bridge child exited"),
        "a bridge that ran and quit must be reported as such: {err}"
    );
    assert!(
        err.contains("/bin/echo"),
        "…naming the program that did it: {err}"
    );
    assert!(
        !err.contains("acp connection closed"),
        "the generic closed-connection message is exactly what this replaced: {err}"
    );
}

/// THE regression: these two are different failures and must read differently. This is the
/// assertion the original diagnosis turned on — the two messages were byte-identical.
#[test]
fn the_missing_and_the_mute_bridge_are_not_the_same_message() {
    let (_, missing) = boot_with_bridge("definitely-not-a-real-binary-xyz");
    let (_, mute) = boot_with_bridge("/bin/echo");
    assert_ne!(
        missing.trim(),
        mute.trim(),
        "a missing bridge and a present-but-wrong one are different diagnoses"
    );
}
