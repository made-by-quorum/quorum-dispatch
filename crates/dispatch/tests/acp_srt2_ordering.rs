//! SRT-2 / MF2 — terminal-ordering RECONSTRUCTABLE test (the crate-backed driver's SC-2b proof).
//!
//! The migration-spec's SRT-2 risk: the crate delivers `session/update`s via
//! `on_receive_notification` callbacks AND the `session/prompt` result via the awaited request —
//! two sources. If the terminal could overtake a trailing `agent_message_chunk`, Pete's view
//! truncates. This test drives the REAL crate connection against a FIXTURE agent
//! (`tests/fixtures/srt2_fake_agent.py`) — deliberately NOT opencode — that emits the final chunk
//! notification AND the prompt response in ONE write (the tightest legal interleaving), FORCING
//! the adverse timing rather than leaning on opencode's incidental notifications-before-response
//! ordering (which would make a naive driver pass by luck; see evidence/opencode-acp/WIRE-FINDINGS.md).
//!
//! It asserts: (1) the terminal is the LAST event, (2) the final `agent_message_chunk` (a sentinel
//! token) is surfaced strictly BEFORE the terminal, (3) the full assistant text is intact. The
//! driver passes because Updates are pushed INLINE in the crate's dispatch loop (which blocks on
//! the callback before reading the response frame) and the terminal is funnelled onto the SAME
//! single ordered bus — so a terminal can never overtake a chunk that preceded it on the wire.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use dispatch::provider::acp::{AcpClient, AcpEvent, AcpHost, StopReason};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/srt2_fake_agent.py")
}

/// python3 present? The fixture agent is a python script; a box without python3 SKIPS (a
/// documented honest partial) rather than redding the suite. The RUN box (where the conformance
/// instance exercises this) has python3.
fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn srt2_terminal_never_overtakes_last_chunk() {
    if !python3_available() {
        eprintln!("SKIP srt2_terminal_never_overtakes_last_chunk: python3 not available");
        return;
    }
    let fixture = fixture_path();
    assert!(fixture.exists(), "fixture agent missing: {}", fixture.display());
    let cwd = std::env::temp_dir();

    let host = AcpHost::spawn(
        "python3",
        &[fixture.to_string_lossy().into_owned()],
        &cwd,
    )
    .expect("spawn fixture agent");

    host.initialize().expect("initialize handshake");
    let session = host
        .new_session(cwd.to_str().unwrap())
        .expect("session/new");
    let _turn = host.prompt(&session, "go", "test").expect("prompt enqueue+send");

    // Drain events until the terminal, recording arrival order.
    let mut events: Vec<AcpEvent> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out before terminal; events so far: {events:?}"
        );
        match host.next_update(Duration::from_millis(500)) {
            Ok(Some(ev)) => {
                let terminal = matches!(
                    ev,
                    AcpEvent::Terminal { .. } | AcpEvent::TerminalError { .. }
                );
                events.push(ev);
                if terminal {
                    break;
                }
            }
            Ok(None) => continue,
            Err(e) => panic!("next_update errored before terminal: {e}"),
        }
    }

    // (1) The terminal is the LAST event — nothing surfaced after it (no post-terminal chunk).
    let terminal_idx = events.len() - 1;
    assert!(
        matches!(
            events[terminal_idx],
            AcpEvent::Terminal {
                stop: StopReason::EndTurn,
                ..
            }
        ),
        "last event must be the end_turn terminal, got {:?}",
        events[terminal_idx]
    );

    // (2) The final agent_message_chunk (sentinel) is surfaced strictly BEFORE the terminal.
    let sentinel_idx = events
        .iter()
        .position(|e| matches!(e, AcpEvent::Update { payload, .. }
            if payload.get("content").and_then(|c| c.get("text")).and_then(|t| t.as_str())
                == Some("LASTWORD_SENTINEL")))
        .expect("the LASTWORD_SENTINEL chunk must be surfaced, never truncated");
    assert!(
        sentinel_idx < terminal_idx,
        "the last agent_message_chunk must precede the terminal (no truncation): \
         sentinel@{sentinel_idx} terminal@{terminal_idx}; events={events:?}"
    );

    // (3) The full assistant text is intact, ending with the sentinel.
    let text = host.assistant_text();
    assert!(
        text.ends_with("LASTWORD_SENTINEL"),
        "assistant_text truncated the last chunk: {text:?}"
    );
    assert!(
        text.contains("ordering") && text.contains("boundary"),
        "assistant_text lost earlier chunks: {text:?}"
    );

    host.shutdown();
}
