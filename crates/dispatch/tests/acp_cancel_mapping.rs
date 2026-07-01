//! claude-migration — literal-`cancelled` cancel-mapping RUN test (isolation-safe; NO live claude).
//!
//! The retained `rpc.rs` seam maps the wire `cancelled` ↔ `StopReason::Cancelled` and
//! `Cancelled → TerminalReason::Cancelled` (pinned by `stop_reason_wire_roundtrips_all_variants` /
//! `r8_terminal_reason_mapping_is_explicit`). What those pure-function tests do NOT cover is the
//! DRIVER path end-to-end, because the only bridge exercised so far (opencode) returns `end_turn`
//! on cancel. This test drives the REAL crate-backed [`AcpHost`] — same `client.rs` the claude
//! bridge runs on — against a fixture that resolves the in-flight prompt with `cancelled`, and
//! asserts the interrupt surfaces faithfully as `Terminal{ stop: Cancelled }` with the queue slot
//! released (no leak). A claude break may emit exactly this literal stop-reason, and it matters
//! more on Pete's path — so it is verified here without a live claude turn (fixture peer only).

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use dispatch::provider::acp::{AcpClient, AcpEvent, AcpHost, StopReason};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cancel_fake_agent.py")
}

fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn cancel_maps_literal_cancelled_and_releases_the_slot() {
    if !python3_available() {
        eprintln!("SKIP cancel_maps_literal_cancelled_and_releases_the_slot: python3 not available");
        return;
    }
    let fixture = fixture_path();
    assert!(fixture.exists(), "fixture agent missing: {}", fixture.display());
    let cwd = std::env::temp_dir();

    let host = AcpHost::spawn("python3", &[fixture.to_string_lossy().into_owned()], &cwd)
        .expect("spawn fixture agent");
    host.initialize().expect("initialize handshake");
    let session = host.new_session(cwd.to_str().unwrap()).expect("session/new");

    // Send a turn (the fixture streams a chunk then HOLDS the response) → it is in flight.
    let _turn = host.prompt(&session, "go", "test").expect("prompt enqueue+send");
    assert!(host.in_flight(), "the turn must be in flight before we cancel it");

    // The interrupt: cancel → session/cancel → the fixture resolves the held prompt `cancelled`.
    host.cancel(&session).expect("cancel");

    // Drain to the terminal, recording arrival order.
    let mut events: Vec<AcpEvent> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let terminal = loop {
        assert!(
            Instant::now() < deadline,
            "timed out before terminal; events so far: {events:?}"
        );
        match host.next_update(Duration::from_millis(500)) {
            Ok(Some(ev)) => {
                let is_terminal =
                    matches!(ev, AcpEvent::Terminal { .. } | AcpEvent::TerminalError { .. });
                events.push(ev.clone());
                if is_terminal {
                    break ev;
                }
            }
            Ok(None) => continue,
            Err(e) => panic!("next_update errored before terminal: {e}"),
        }
    };

    // The interrupt surfaces as the literal `Cancelled` terminal — NOT end_turn, NOT a TerminalError.
    match terminal {
        AcpEvent::Terminal { stop, .. } => assert_eq!(
            stop,
            StopReason::Cancelled,
            "session/cancel must map the bridge's `cancelled` to StopReason::Cancelled; events={events:?}"
        ),
        other => panic!("cancel must yield a clean Terminal{{Cancelled}}, got {other:?}"),
    }

    // No queue leak: the slot is released once the cancelled terminal drains.
    assert!(
        !host.in_flight(),
        "the queue slot must be released after the cancelled terminal (no leak)"
    );

    host.shutdown();
}
