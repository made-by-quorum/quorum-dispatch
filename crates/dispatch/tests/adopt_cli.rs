//! Real-binary error surface for `qd adopt`.

use std::process::{Command, Stdio};
use std::time::Duration;

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.stdin.take();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn adopt_name_not_found_is_clear_and_fail_closed() {
    let jail = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_qd"))
        .args(["adopt", "missing-session"])
        .env("HOME", jail.path())
        .env_remove("QD_HOME")
        .env_remove("QD_SESSION_ID")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No session matching \"missing-session\""),
        "{stderr}"
    );
    assert!(!jail.path().join(".quorum/dispatch/state/adoption").exists());
}

#[test]
fn adopt_ambiguous_prefix_is_clear_and_fail_closed() {
    let home = std::path::PathBuf::from(format!(
        "/tmp/qd-a-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(home.join(".claude/sessions")).unwrap();
    let first = Command::new("sleep").arg("30").spawn().unwrap();
    let second = Command::new("sleep").arg("30").spawn().unwrap();
    let first = ChildGuard(first);
    let second = ChildGuard(second);
    for (child, name, uuid) in [
        (&first, "ambiguous-one", "ambiguous-uuid-1"),
        (&second, "ambiguous-two", "ambiguous-uuid-2"),
    ] {
        let pid = child.0.id();
        std::fs::write(
            home.join(format!(".claude/sessions/{pid}.json")),
            serde_json::json!({
                "pid": pid,
                "sessionId": uuid,
                "startedAt": 1,
                "status": "idle",
                "name": name,
                "provider": "claude-code"
            })
            .to_string(),
        )
        .unwrap();
    }

    let output = Command::new(env!("CARGO_BIN_EXE_qd"))
        .args(["adopt", "ambiguous"])
        .env("HOME", &home)
        .env_remove("QD_HOME")
        .env_remove("QD_SESSION_ID")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Ambiguous"), "{stderr}");
    assert!(stderr.contains("matches 2 sessions"), "{stderr}");
    assert!(!home.join(".quorum/dispatch/state/adoption").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn adopt_same_name_ambiguity_prints_live_candidate_context() {
    let home = std::path::PathBuf::from(format!(
        "/tmp/qd-a-context-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(home.join(".claude/sessions")).unwrap();
    let first = ChildGuard(Command::new("sleep").arg("30").spawn().unwrap());
    let second = ChildGuard(Command::new("sleep").arg("30").spawn().unwrap());
    for (child, uuid) in [(&first, "same-name-uuid-1"), (&second, "same-name-uuid-2")] {
        let pid = child.0.id();
        std::fs::write(
            home.join(format!(".claude/sessions/{pid}.json")),
            serde_json::json!({
                "pid": pid,
                "sessionId": uuid,
                "startedAt": 1,
                "status": "idle",
                "name": "same-name",
                "provider": "claude-code"
            })
            .to_string(),
        )
        .unwrap();
    }

    let output = Command::new(env!("CARGO_BIN_EXE_qd"))
        .args(["adopt", "same-name"])
        .env("HOME", &home)
        .env_remove("QD_HOME")
        .env_remove("QD_SESSION_ID")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Ambiguous"), "{stderr}");
    assert!(stderr.contains("matches 2 sessions"), "{stderr}");
    for child in [&first, &second] {
        let pid_field = format!("PID {}", child.0.id());
        let candidate = stderr
            .lines()
            .find(|line| line.contains(&pid_field))
            .unwrap_or_else(|| panic!("missing candidate {pid_field} in:\n{stderr}"));
        assert!(candidate.contains("\tbare\talive\tstarted "), "{candidate}");
        assert!(candidate.contains(" ago"), "{candidate}");
    }
    assert!(!home.join(".quorum/dispatch/state/adoption").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn adopt_bare_send_real_binary_refuses_before_queueing() {
    let jail = tempfile::tempdir().unwrap();
    let home = jail.path();
    let uuid = format!("adopt-bare-{}", std::process::id());
    let qd = env!("CARGO_BIN_EXE_qd");

    // The shell pid becomes a process whose argv[0] is exactly `claude`; its
    // relay child inherits fd 3 as a live MCP stdin pipe. This gives the real
    // resolver a truthful live bare topology without launching Claude Code.
    let child = Command::new("bash")
        .arg("-c")
        .arg("exec 3<&0; \"$QD_TEST_BIN\" relay:serve <&3 & exec -a claude sleep 30")
        .env("QD_TEST_BIN", qd)
        .env("HOME", home)
        .env("CLAUDE_CODE_SESSION_ID", &uuid)
        .env_remove("QD_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let claude_pid = child.id();
    let mut claude = ChildGuard(child);

    let fixture_argv = dispatch::effects::process_rows(&dispatch::exec::RealExec)
        .unwrap()
        .get(&(claude_pid as i32))
        .and_then(|row| row.argv.clone())
        .expect("fake Claude real argv");
    eprintln!("fake Claude argv evidence: {fixture_argv:?}");
    assert_eq!(fixture_argv.first().map(String::as_str), Some("claude"));
    assert!(
        !fixture_argv
            .iter()
            .any(|arg| arg.contains("dangerously-load-development-channels")),
        "bare fixture must not carry the channel option: {fixture_argv:?}"
    );

    let relay_dir = home.join(".claude/relay");
    let sidecar = (0..100).find_map(|_| {
        let found = std::fs::read_dir(&relay_dir).ok().and_then(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .find(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        });
        if found.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        found
    });
    assert!(sidecar.is_some(), "relay sidecar did not appear");

    let started_at = (0..100)
        .find_map(|_| {
            let started = dispatch::effects::proc_start_ms(claude_pid as i32);
            if started.is_none() {
                std::thread::sleep(Duration::from_millis(20));
            }
            started
        })
        .expect("fake Claude process start time");

    let sessions = home.join(".claude/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join(format!("{claude_pid}.json")),
        serde_json::json!({
            "pid": claude_pid,
            "sessionId": uuid,
            "startedAt": started_at,
            "updatedAt": started_at,
            "status": "idle",
            "name": "bare-one",
            "provider": "claude-code"
        })
        .to_string(),
    )
    .unwrap();

    let listing = Command::new(qd)
        .args(["ls", "--live", "--json"])
        .env("HOME", home)
        .env_remove("QD_HOME")
        .output()
        .unwrap();
    assert!(listing.status.success(), "{}", String::from_utf8_lossy(&listing.stderr));
    let rows: serde_json::Value = serde_json::from_slice(&listing.stdout).unwrap();
    let bare = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "bare-one")
        .expect("bare row in qd ls --live --json");
    eprintln!("qd ls bare evidence: {bare}");
    assert_eq!(bare["management"], "bare");

    let output = Command::new(qd)
        .args(["send:relay", "bare-one", "must-not-queue"])
        .env("HOME", home)
        .env_remove("QD_HOME")
        .env_remove("QD_SESSION_ID")
        .output()
        .unwrap();

    // Close the MCP stdin before assertions so the relay exits even on failure.
    let _ = claude.0.stdin.take();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("bare-send stderr evidence: {}", stderr.trim());
    let expected = "Destination \"bare-one\" is non-receivable (bare); no message was queued. Ask the human to have that Claude Code session run `qd adopt bare-one`. Adoption requires a manual qrmux restart with `qd relay:serve` and `--dangerously-load-development-channels server:relay`.";
    assert!(stderr.contains(expected), "{stderr}");
}

#[test]
fn adopt_external_uses_kernel_start_when_registry_started_at_lags() {
    let jail = tempfile::tempdir().unwrap();
    let home = jail.path();
    let uuid = format!("adopt-lagged-start-{}", std::process::id());
    let qd = env!("CARGO_BIN_EXE_qd");

    // Match the real bare-Claude topology used above: the shell becomes a
    // process whose argv[0] is `claude`, with a live relay MCP child.
    let child = Command::new("bash")
        .arg("-c")
        .arg("exec 3<&0; \"$QD_TEST_BIN\" relay:serve <&3 & exec -a claude sleep 30")
        .env("QD_TEST_BIN", qd)
        .env("HOME", home)
        .env("CLAUDE_CODE_SESSION_ID", &uuid)
        .env_remove("QD_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let claude_pid = child.id();
    let mut claude = ChildGuard(child);

    let fixture_argv = dispatch::effects::process_rows(&dispatch::exec::RealExec)
        .unwrap()
        .get(&(claude_pid as i32))
        .and_then(|row| row.argv.clone())
        .expect("fake Claude real argv");
    assert_eq!(fixture_argv.first().map(String::as_str), Some("claude"));

    let relay_dir = home.join(".claude/relay");
    let sidecar = (0..100).find_map(|_| {
        let found = std::fs::read_dir(&relay_dir).ok().and_then(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        });
        if found.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        found
    });
    assert!(sidecar.is_some(), "relay sidecar did not appear");

    let real_start_ms = (0..100)
        .find_map(|_| {
            let started = dispatch::effects::proc_start_ms(claude_pid as i32);
            if started.is_none() {
                std::thread::sleep(Duration::from_millis(20));
            }
            started
        })
        .expect("fake Claude process start time");
    // Stay beyond the 1000ms fence even if the two real `ps`-based reads land
    // on opposite display ticks; the old registry-backed preparation must fail.
    let lagged_registration_ms = real_start_ms + 2_100;

    let sessions = home.join(".claude/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join(format!("{claude_pid}.json")),
        serde_json::json!({
            "pid": claude_pid,
            "sessionId": uuid,
            "startedAt": lagged_registration_ms,
            "updatedAt": lagged_registration_ms,
            "status": "idle",
            "name": "bare-lagged-start",
            "provider": "claude-code"
        })
        .to_string(),
    )
    .unwrap();

    let output = Command::new(qd)
        .args(["adopt", "bare-lagged-start", "--force"])
        .env("HOME", home)
        .env_remove("QD_HOME")
        .env_remove("QD_SESSION_ID")
        .output()
        .unwrap();

    let target_status = claude.0.try_wait().unwrap();
    let _ = claude.0.stdin.take();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("start_ms disagrees"),
        "registry lag incorrectly tripped the process-start fence: {stderr}"
    );
    assert!(
        !stderr.contains("kill-seam identity fence mismatch"),
        "adoption did not pass the kill fence: {stderr}"
    );
    assert!(
        target_status.is_some(),
        "target was not SIGTERM'd after passing the kill fence; status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
}
