//! sbmux gate pass (a) integration suite — B1 stress cases as REAL tests.
//!
//! Every test here carries HARD assertions keyed on application output
//! (ADD-6: never on PTY echo). The 2026-06-04 audit found the previous
//! suite passed vacuously (placeholder capture returned empty buffers);
//! this rewrite is the corrective. Standing rule: a scenario with no real
//! assertion must be `#[ignore]`d with a FAIL-CLOSED note, never green.
//!
//! Spec: `exec/b2-spec.md` (gate pass (a), deliverable #4).
//! Named divergences: #1 altscreen absorb-and-REPLAY (ADR-0004, REVERSED
//! 2026-06-10: performer absorbs; renderer re-emits 1049 per client — see
//! doc/inbox/2026-06-10-sbmux-phone-scroll-regression.md),
//! #2 kernel echo loss under flood (ADD-6, macOS-specific).
//! B1 baseline: fork lineage 6533d77 (results re-proven here, not imported).

#[path = "lib/mod.rs"]
// Shared test harness is `#[path]`-included into every integration test binary
// (integration_tests, b3_replay, b3_resize); each uses a DIFFERENT subset, so a
// binary leaves some helpers/re-exports unused — which `-D warnings` rejects.
// This allow (matching b3_replay/b3_resize) keeps the crate-wide clippy gate
// green as new test binaries consume only part of the harness.
#[allow(dead_code, unused_imports)]
mod libmod;
use libmod::*;

use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ============================================================================
// Shared helpers
// ============================================================================

/// Evidence directory for a scenario: crates/sbmux/target/test-evidence/<runid>/<scenario>.
fn evidence_dir(scenario: &str) -> PathBuf {
    let runid = std::env::var("SBMUX_GATE_RUNID").unwrap_or_else(|_| "dev".to_string());
    let dir = PathBuf::from("target/test-evidence")
        .join(runid)
        .join(scenario);
    fs::create_dir_all(&dir).expect("create evidence dir");
    dir
}

/// Poll a fresh-attach capture of `session` until `pred(text)` holds or `timeout`.
/// Returns the final captured text (predicate satisfied) or Err on timeout.
fn wait_for_text(
    socket: &std::path::Path,
    session: &str,
    timeout: Duration,
    poll: Duration,
    pred: impl Fn(&str) -> bool,
) -> Result<String, Box<dyn Error>> {
    let start = Instant::now();
    // Progress series: capture length per poll. On timeout the error carries
    // it so the artifact alone discriminates "runner slow but advancing" from
    // "pipeline frozen at N chars" (orc-2 directive, G3 macOS-CI disposition).
    let mut progress: Vec<(u64, usize)> = Vec::new();
    loop {
        let cap = capture_session(socket, session, 150)?;
        let text = cap.text();
        if pred(&text) {
            return Ok(text);
        }
        progress.push((start.elapsed().as_secs(), text.len()));
        if start.elapsed() > timeout {
            let tail: String = text.chars().skip(text.len().saturating_sub(300)).collect();
            let series: Vec<String> = progress
                .iter()
                .map(|(t, l)| format!("{}s:{}", t, l))
                .collect();
            return Err(format!(
                "timed out after {:?} waiting for predicate (last capture: {} chars;                  progress [{}]; tail: {:?})",
                timeout,
                text.len(),
                series.join(" "),
                tail
            )
            .into());
        }
        std::thread::sleep(poll);
    }
}

/// Wait for `marker` to appear, then take a FRESH capture once the session has
/// quiesced (two consecutive captures of equal length) and return it.
///
/// WHY (war story, found during G6 bring-up): a capture that attaches
/// mid-production can satisfy a tail-line predicate through its live-drain
/// stream while its history snapshot predates most of the production — a
/// partial snapshot+stream mix, NOT the full replay. Asserting completeness on
/// the predicate-satisfying capture undercounts (23/500 observed). The engine
/// ring is complete once production quiesces (probe: 1486/1486 rows); assert
/// on a settled re-capture.
fn wait_then_settled_capture(
    socket: &std::path::Path,
    session: &str,
    marker: &str,
    timeout: Duration,
) -> Result<String, Box<dyn Error>> {
    wait_for_text(socket, session, timeout, Duration::from_millis(300), |t| {
        t.contains(marker)
    })?;
    let mut last = capture_session(socket, session, 150)?.text();
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(300));
        let next = capture_session(socket, session, 150)?.text();
        if next.len() == last.len() && next.contains(marker) {
            return Ok(next);
        }
        last = next;
    }
    Err("capture never settled (production still running after 9s?)".into())
}

/// Responsive check: round-trip a unique sentinel through the session.
/// The sentinel is quote-split on the input side so the assertion can only be
/// satisfied by EXECUTED output, not by the echoed command line (ADD-6).
fn assert_session_responsive(
    socket: &std::path::Path,
    env: &[(String, String)],
    session: &str,
    tag: &str,
) -> Result<(), Box<dyn Error>> {
    let cmd = format!("echo RESP''ONSIVE-{}\n", tag);
    let needle = format!("RESPONSIVE-{}", tag);
    send_to_session(socket, env, session, &cmd)?;
    wait_for_text(
        socket,
        session,
        Duration::from_secs(30),
        Duration::from_millis(300),
        |t| t.contains(&needle),
    )?;
    Ok(())
}

/// Count how many of the lines `prefix<lo>..prefix<hi>` (zero-padded width)
/// appear in `text`. Zero-drop comparator for generated numbered lines.
fn count_numbered_lines(text: &str, prefix: &str, lo: usize, hi: usize, width: usize) -> usize {
    (lo..=hi)
        .filter(|i| text.contains(&format!("{}{:0w$}", prefix, i, w = width)))
        .count()
}

// ============================================================================
// G1 — send verb (one-shot, keyed on app output)
// ============================================================================

/// G1 — `send` one-shot: bytes land at the PTY with NO client attached;
/// acceptance keyed on EXECUTED output captured via a fresh attach.
///
/// The sentinel is quote-split (`B1SEN''TINEL_G1`) so the echoed command line
/// can never satisfy the assertion — only the executed `echo` output contains
/// the joined form. War story: ADD-6 (kernel echo loss; key on app output).
#[test]
fn g1_send_verb() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("g1_send_verb")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "g1")?;
    let session = "g1";
    create_session(&socket, session)?;

    // One-shot send with zero clients attached (create_session dropped its attach).
    send_to_session(&socket, &env, session, "echo B1SEN''TINEL_G1\n")?;

    // Fresh attach capture; assert EXECUTED sentinel present.
    let text = wait_for_text(
        &socket,
        session,
        Duration::from_secs(30),
        Duration::from_millis(300),
        |t| t.contains("B1SENTINEL_G1"),
    )?;

    let ev = evidence_dir("g1");
    fs::write(ev.join("g1_capture.txt"), &text)?;
    fs::write(
        ev.join("g1_result.txt"),
        format!(
            "G1 PASS\nsentinel=B1SENTINEL_G1 found in fresh-attach capture\ncapture_sha256={}\ncapture_bytes={}\n",
            sha256(text.as_bytes()),
            text.len()
        ),
    )?;

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// G2 — altscreen stress (less; ADR-0004 invariants)
// ============================================================================

/// G2 — Altscreen: `less` under the screen-model mux. Divergence #1
/// (absorb-and-REPLAY): the vte performer consumes DEC 1049/47/1047
/// server-side — clients never see the inner app's raw mode bytes — and the
/// RENDER layer replays the absorbed alt-screen state per client (`?1049h`
/// on an attach into a fullscreen app; gate reversal approved 2026-06-10,
/// see doc/inbox/2026-06-10-sbmux-phone-scroll-regression.md). Asserts
/// ADR-0004 intent invariants, not byte forwarding:
///   (a) render: app content visible to a fresh attach while less runs
///   (b) restore-equivalence: pre-app sentinel back on screen after exit
///   (c) scroll-intact: pre-app scrollback present in replay after exit
///   (d) altscreen-replay: a fresh attach DURING the app carries exactly one
///       ?1049h (and no exit); a fresh attach AFTER exit carries zero 1049
///       sequences; legacy ?47/?1047 never appear
#[test]
fn g2_altscreen_stress() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("g2_altscreen")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "g2")?;
    let session = "g2";
    create_session(&socket, session)?;
    let ev = evidence_dir("g2");

    // Pre-app content: scrollback lines + a primary-screen sentinel.
    send_to_session(&socket, &env, session, "seq -f 'g2-pre-%03.0f' 1 40\n")?;
    send_to_session(&socket, &env, session, "echo G2-PRE''APP-SENTINEL\n")?;
    wait_for_text(
        &socket,
        session,
        Duration::from_secs(30),
        Duration::from_millis(300),
        |t| t.contains("G2-PREAPP-SENTINEL"),
    )?;

    // File for less, with a marker that exists NOWHERE else.
    let file = jail.tmpdir.join("g2_file.txt");
    let content: String = (1..=100)
        .map(|i| format!("G2-FILE-{:03}-xyzzymarker\n", i))
        .collect();
    fs::write(&file, &content)?;

    // Run less (altscreen app).
    send_to_session(
        &socket,
        &env,
        session,
        &format!("less {}\n", file.display()),
    )?;

    // (a) render: fresh attach sees the app's screen content.
    let during = wait_for_text(
        &socket,
        session,
        Duration::from_secs(30),
        Duration::from_millis(300),
        |t| t.contains("xyzzymarker"),
    )?;
    fs::write(ev.join("g2_during_less.txt"), &during)?;

    // (d) altscreen-replay DURING the app: a fresh attach lands inside the
    // alt screen, so its raw render must carry exactly one ?1049h (the
    // renderer's per-client replay of the absorbed state) and no exit.
    let cap_during = capture_session(&socket, session, 150)?;
    assert_altscreen_replay(&cap_during.raw_render(), 1, 0, "g2-during")
        .map_err(|e| -> Box<dyn Error> { e.into() })?;

    // Quit less.
    send_to_session(&socket, &env, session, "q")?;

    // (b) restore-equivalence: pre-app sentinel visible again post-exit,
    // and the app's content gone from the primary screen.
    let after = wait_for_text(
        &socket,
        session,
        Duration::from_secs(30),
        Duration::from_millis(300),
        |t| t.contains("G2-PREAPP-SENTINEL") && !t.contains("xyzzymarker"),
    )
    .map_err(|e| format!("restore-equivalence failed: {}", e))?;
    fs::write(ev.join("g2_after_less.txt"), &after)?;

    // (c) scroll-intact: pre-app scrollback lines present in the replay.
    let n = count_numbered_lines(&after, "g2-pre-", 1, 40, 3);
    assert_eq!(
        n, 40,
        "scroll-intact: expected all 40 pre-app lines, got {}",
        n
    );

    // (d) altscreen-replay after exit: the session is back on the main
    // screen, so a fresh attach must carry ZERO 1049 sequences (main-screen
    // captures are byte-preserved from the pre-replay behavior).
    let cap_after = capture_session(&socket, session, 150)?;
    assert_altscreen_replay(&cap_after.raw_render(), 0, 0, "g2-after")
        .map_err(|e| -> Box<dyn Error> { e.into() })?;

    fs::write(
        ev.join("g2_result.txt"),
        "G2 PASS\n(a) render: xyzzymarker visible during less\n\
         (b) restore-equivalence: G2-PREAPP-SENTINEL restored, marker gone\n\
         (c) scroll-intact: 40/40 pre-app lines in replay\n\
         (d) altscreen-replay: exactly one ?1049h during, zero 1049 after\n\
         Divergence #1 (ADR-0004, reversed 2026-06-10): app mode bytes absorbed \
         server-side; renderer replays 1049 per client\n",
    )?;

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// G3 — SIGWINCH: propagation + storm during streaming
// ============================================================================

/// G3 — (a) single resize → child's WINCH trap fires (signal really delivered);
/// (b) ≥60 interleaved resizes WHILE a generator streams 8000 lines → zero-drop,
/// final size correct, responsive after. Resizes go through a persistent
/// attached client (`ClientMsg::Resize` → daemon TIOCSWINSZ → child SIGWINCH) —
/// the real protocol path, not a simulation.
#[test]
fn g3_winch_storm() -> Result<(), Box<dyn Error>> {
    // QUARANTINE (macOS GitHub CI ONLY; ledger: GATE-B2.md "Quarantine"):
    // deterministic final-marker loss on the GH macOS runner — capture frozen
    // at full volume (~112,678 chars, progress series flat 0s→120s) with
    // g3-line-08000 absent from BOTH history and screen, while the ATTACHED
    // stream saw it and stty/barrier passed. Does not reproduce on brano
    // (incl. with the runner's 73-char prompt width simulated — probe
    // 8000/8000) or on ubuntu-latest. Gated on the workflow-set env var AND
    // target_os so it can never silently skip elsewhere: macOS coverage
    // continues locally; Linux coverage continues in CI. Open finding for
    // pass (b).
    if cfg!(target_os = "macos")
        && std::env::var("SBMUX_CI_QUARANTINE")
            .map(|v| v.contains("g3-macos"))
            .unwrap_or(false)
    {
        eprintln!(
            "G3 QUARANTINED on macOS CI (SBMUX_CI_QUARANTINE=g3-macos):              see crates/sbmux/GATE-B2.md 'Quarantine' for the evidence chain.              NOT a pass on this platform-lane; macOS proof is the local lane."
        );
        return Ok(());
    }
    let jail = setup_jail("g3_winch")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "g3")?;
    let session = "g3";
    create_session(&socket, session)?;
    let ev = evidence_dir("g3");

    // Persistent attached client — the resize source AND the observer.
    // NOTE: no capture_session/wait_for_text while attached — a fresh attach
    // EVICTS the attached client (one-client mux semantics).
    let attached = AttachedClient::attach(&socket, session)?;

    // Poll the attached client's own live stream for a marker.
    let wait_attached = |needle: &str, secs: u64| -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if attached.captured_text().contains(needle) {
                return Ok(());
            }
            if let Some(e) = attached.error() {
                return Err(format!("attached client errored: {}", e));
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "timed out waiting for {:?} on attached stream",
                    needle
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };

    // (a) Single resize: trap WINCH, resize once, trap must fire.
    send_to_session(&socket, &env, session, "trap 'echo WINCH-FIRED' WINCH\n")?;
    std::thread::sleep(Duration::from_millis(300));
    attached.resize(132, 43)?;
    wait_attached("WINCH-FIRED", 30)
        .map_err(|e| format!("G3(a): WINCH trap never fired after resize: {}", e))?;
    fs::write(ev.join("g3_single_resize.txt"), attached.captured_text())?;

    // (b) Storm during streaming: clear the trap (so trap output can't
    // interleave mid-line with the generator), stream 8000 numbered lines in
    // the foreground, and fire 60 randomized resizes while it runs.
    send_to_session(&socket, &env, session, "trap - WINCH\n")?;
    // tee oracle: discriminates "child died early under resize storm" (file
    // truncated too) from "mux dropped output" (file complete, screen short).
    let g3_file = jail.tmpdir.join("g3_oracle.txt");
    send_to_session(
        &socket,
        &env,
        session,
        &format!(
            "seq -f 'g3-line-%05.0f' 1 8000 | tee {}\n",
            g3_file.display()
        ),
    )?;

    let sizes: Vec<(u16, u16)> = (0..60)
        .map(|i| (60 + ((i * 7) % 80) as u16, 20 + ((i * 3) % 20) as u16))
        .collect();
    for &(cols, rows) in &sizes {
        attached.resize(cols, rows)?;
        std::thread::sleep(Duration::from_millis(10));
    }
    // Final, known size — asserted via stty after the stream completes.
    attached.resize(100, 30)?;

    // Wait for the stream to finish on the ATTACHED client's view.
    if let Err(e) = wait_attached("g3-line-08000", 120) {
        // Diagnose: tee oracle separates child-death from mux-drop.
        let file_lines = fs::read_to_string(&g3_file)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        let tail: String = {
            let t = attached.captured_text();
            t.chars().skip(t.len().saturating_sub(300)).collect()
        };
        return Err(format!(
            "G3(b): generator did not complete under storm: {}; tee oracle file has {}/8000 \
             lines ({}); attached tail: {:?}",
            e,
            file_lines,
            if file_lines >= 8000 {
                "MUX DROPPED OUTPUT — real bug"
            } else {
                "child died early under storm"
            },
            tail
        )
        .into());
    }

    // Final size correct: stty reports through the still-attached client's PTY.
    // (`stty size` prints "rows cols".)
    // Barrier: confirm the daemon's dims snapshot shows the final size before
    // asking the child (resizes ride the attached connection; stty rides a
    // separate SendInput connection and could otherwise overtake them).
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let dims = list_sessions(&socket)?
                .iter()
                .find(|s| s.name == session)
                .map(|s| (s.cols, s.rows));
            if dims == Some((100, 30)) {
                break;
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "G3(b): daemon dims never reached 100x30; last snapshot: {:?}",
                    dims
                )
                .into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    send_to_session(&socket, &env, session, "stty size\n")?;
    wait_attached("30 100", 30).map_err(|e| {
        let t = attached.captured_text();
        let tail: String = t.chars().skip(t.len().saturating_sub(400)).collect();
        format!(
            "G3(b): final size 30x100 not reported by stty: {}; attached tail: {:?}",
            e, tail
        )
    })?;
    if let Some(e) = attached.error() {
        return Err(format!("G3: attached client errored during storm: {}", e).into());
    }
    attached.close();

    // Zero-drop on a settled FRESH capture (post-close; ring is complete once
    // production quiesces — see wait_then_settled_capture war story).
    // 120s: GitHub macOS runners under parallel suite load need well over 30s
    // for 8000 lines through the pipeline (observed CI timeout at 30s with
    // ~112KB already captured). A hang still fails — just later.
    let text_b =
        wait_then_settled_capture(&socket, session, "g3-line-08000", Duration::from_secs(120))?;
    let n = count_numbered_lines(&text_b, "g3-line-", 1, 8000, 5);
    fs::write(ev.join("g3_post_storm_capture.txt"), &text_b)?;
    assert_eq!(
        n,
        8000,
        "G3(b) zero-drop FAIL: {}/8000 lines intact after {} interleaved resizes",
        n,
        sizes.len() + 1
    );

    // Responsive after.
    assert_session_responsive(&socket, &env, session, "G3")?;

    fs::write(
        ev.join("g3_result.txt"),
        format!(
            "G3 PASS\n(a) single resize: WINCH-FIRED observed\n\
             (b) storm: {} resizes interleaved with 8000-line stream; zero-drop 8000/8000\n\
             final size: 30 rows x 100 cols confirmed via stty\nresponsive after: yes\n",
            sizes.len() + 1
        ),
    )?;

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// G4 — paste-burst (>64KB single write; byte-exact + scrollback survival)
// ============================================================================

/// G4 — One ~72KB burst (2000 checksummed lines) in a SINGLE SendInput write.
/// PRIMARY: byte-exact recovery at the application (tee output file ==
/// burst input, SHA256-verified). SECONDARY: app-output scrollback zero-drop
/// (tee's stdout → screen model → history). Echo is disabled in-session;
/// per ADD-6 nothing here keys on echo bytes — macOS kernel echo loss under
/// flood is an OS divergence the mux cannot compensate.
#[test]
fn g4_paste_burst() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("g4_burst")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "g4")?;
    let session = "g4";
    create_session(&socket, session)?;
    let ev = evidence_dir("g4");

    // Burst: 2000 numbered lines, ~36B each (~72KB > 64KB requirement).
    let burst: String = (1..=2000)
        .map(|i| format!("line{:04} {}\n", i, "X".repeat(28)))
        .collect();
    let burst_bytes = burst.as_bytes();
    let burst_sha = sha256(burst_bytes);
    assert!(burst_bytes.len() > 64 * 1024, "burst must exceed 64KB");

    // Consumer: tee → file (byte-exact oracle) + stdout (scrollback oracle).
    // Echo off so the input flood never depends on kernel echo (ADD-6).
    let out_file = jail.tmpdir.join("g4_burst_out.bin");
    send_to_session(
        &socket,
        &env,
        session,
        &format!("stty -echo; tee {}\n", out_file.display()),
    )?;
    std::thread::sleep(Duration::from_millis(500));

    // The burst: ONE protocol write.
    send_to_session_stdin(&socket, &env, session, burst_bytes)?;

    // Wait for the file to reach full size, then EOF tee (Ctrl-D).
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let size = fs::metadata(&out_file).map(|m| m.len()).unwrap_or(0);
        if size >= burst_bytes.len() as u64 {
            break;
        }
        if Instant::now() > deadline {
            return Err(format!(
                "G4 PRIMARY: consumer file stalled at {}/{} bytes",
                size,
                burst_bytes.len()
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    send_to_session_stdin(&socket, &env, session, &[0x04])?;

    // PRIMARY: byte-exact.
    let recovered = fs::read(&out_file)?;
    let recovered_sha = sha256(&recovered);
    fs::write(ev.join("g4_burst_input.bin"), burst_bytes)?;
    fs::write(ev.join("g4_burst_output.bin"), &recovered)?;
    assert_eq!(
        recovered.len(),
        burst_bytes.len(),
        "G4 PRIMARY: length mismatch {} != {}",
        recovered.len(),
        burst_bytes.len()
    );
    assert_eq!(
        recovered_sha, burst_sha,
        "G4 PRIMARY: SHA256 mismatch — burst not byte-exact"
    );

    // SECONDARY: scrollback survival of tee's app output (zero-drop, all 2000
    // lines in the 10k history window), keyed on app output per ADD-6.
    // Settled capture: a mid-stream snapshot undercounts (see helper war story).
    let text = wait_then_settled_capture(&socket, session, "line2000", Duration::from_secs(90))?;
    let n = count_numbered_lines(&text, "line", 1, 2000, 4);
    fs::write(ev.join("g4_scrollback.txt"), &text)?;
    assert_eq!(
        n, 2000,
        "G4 SECONDARY: scrollback zero-drop FAIL — {}/2000 app-output lines intact",
        n
    );

    fs::write(
        ev.join("g4_result.txt"),
        format!(
            "G4 PASS\nPRIMARY byte-exact: {} bytes, sha256 input {} == output {}\n\
             SECONDARY scrollback: 2000/2000 app-output lines intact\n\
             ADD-6: all assertions app-output-keyed; echo disabled in-session\n",
            recovered.len(),
            burst_sha,
            recovered_sha
        ),
    )?;

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// G5 — 1M-line soak (RSS budget + plateau + responsive + recent window)
// ============================================================================

/// G5 — 1,000,000 lines (~80B each) through a detached session (no client
/// attached during the soak). Daemon memory footprint (resident+compressed,
/// see `process_footprint_kb`) sampled each second.
/// PASS: (a) peak footprint ≤ 300MB (macOS) / 250MB (Linux); (b) plateau:
/// mean footprint of final quarter ≤ mean of second quarter + 10% (with the
/// 10MB absolute floor); (c) responsive after; (d) most-recent history-window
/// lines intact (zero-drop on last 1000).
///
/// `SBMUX_SOAK_LINES` overrides the line count for DEV iteration only — the
/// gate runs the default 1,000,000. Evidence records the actual count; a
/// reduced-count run is NOT a gate pass.
#[test]
fn g5_soak() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("g5_soak")?;
    let env = jail_env(&jail);
    let (daemon, socket) = start_daemon_in_jail(&jail, &env, "g5")?;
    let session = "g5";
    create_session(&socket, session)?;
    let ev = evidence_dir("g5");

    let lines: usize = std::env::var("SBMUX_SOAK_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);

    // Generator: ~80B/line numbered output, then a DONE sentinel (quote-split).
    let payload = "Y".repeat(56); // "soak-0001234567-" + 56 + NL ≈ 80B
    send_to_session(
        &socket,
        &env,
        session,
        &format!(
            "seq -f 'soak-%010.0f-{}' 1 {}; echo SOAK-DO''NE-G5\n",
            payload, lines
        ),
    )?;

    // Footprint sampler until the soak completes. Spec cadence is 5s, but the
    // daemonized soak ingests 1M lines in ~30s — 5s would give ~5 samples,
    // too thin for a quarter-based plateau trend. 1s sampling is a deviation
    // in the MORE-evidence direction (~30 samples); evidence records both
    // cadence and count.
    //
    // Flake-rootcause pass: metric switched from `ps rss` to phys_footprint
    // (resident+compressed, process_footprint_kb) — rss stops counting
    // retained-but-idle pages once the compressor takes them, so a REAL leak
    // could hide from G5(b) under ambient memory pressure (the 3865fe9
    // ratchet red demonstrated exactly that residency loss on the negative
    // control). Also a syscall, not a /bin/ps fork — no spawn stalls under
    // load. The leak_guard_negative_control_fires /
    // leak_guard_no_retention_control_quiet pair proves the amended guard's
    // teeth from both sides on this same metric.
    let daemon_pid = daemon.pid;
    let samples = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let samples = samples.clone();
        let done = done.clone();
        std::thread::spawn(move || {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(kb) = process_footprint_kb(daemon_pid) {
                    samples.lock().unwrap().push(kb);
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
    };

    // Wait for completion — generous ceiling, poll every 10s with cheap captures.
    let res = wait_for_text(
        &socket,
        session,
        Duration::from_secs(30 * 60),
        Duration::from_secs(30),
        |t| t.contains("SOAK-DONE-G5"),
    );
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = sampler.join();
    res.map_err(|e| format!("G5: soak did not complete: {}", e))?;
    // Settled re-capture for completeness assertions (mid-stream snapshots
    // undercount; see wait_then_settled_capture war story).
    let text_done =
        wait_then_settled_capture(&socket, session, "SOAK-DONE-G5", Duration::from_secs(60))?;

    // RSS assertions.
    let samples = samples.lock().unwrap().clone();
    let sample_log: String = samples
        .iter()
        .enumerate()
        .map(|(i, kb)| format!("{}\t{}\n", i, kb))
        .collect();
    fs::write(ev.join("g5_rss_samples.tsv"), &sample_log)?;
    assert!(
        samples.len() >= 4,
        "G5: too few RSS samples ({}) for a plateau check — soak too short?",
        samples.len()
    );
    let peak_kb = *samples.iter().max().unwrap();
    let budget_kb: u64 = if cfg!(target_os = "macos") {
        300 * 1024
    } else {
        250 * 1024
    };
    assert!(
        peak_kb <= budget_kb,
        "G5(a): peak footprint {} KB exceeds budget {} KB",
        peak_kb,
        budget_kb
    );

    let q = samples.len() / 4;
    let mean = |s: &[u64]| s.iter().sum::<u64>() as f64 / s.len().max(1) as f64;
    let q2_mean = mean(&samples[q..2 * q]);
    let q4_mean = mean(&samples[3 * q..]);
    // Leak trend = relative growth AND a data-volume-scale absolute delta.
    // WHY THE FLOOR (war story, QA fresh-run catch + discriminating probes):
    // at this daemon's ~13 MB base, single allocator arena steps of 1.5-3.3 MB
    // trip a pure 10% check. Probes: 1M lines → Δ3.2 MB; 2M lines (double the
    // volume) → Δ1.5 MB — anti-correlated with data, non-monotonic mid-run
    // (16.8→14.7 MB), i.e. allocator noise, not retention. A REAL leak retains
    // O(bytes-per-line × lines): even 13% retention of the 80 MB soak is
    // >10 MB. Floor = 10 MB: 3× the observed noise band, far below any real
    // leak signature at gate volume. Peak budget (300 MB) still applies above.
    const LEAK_ABS_FLOOR_KB: f64 = 10.0 * 1024.0;
    assert!(
        q4_mean <= q2_mean * 1.10 || (q4_mean - q2_mean) <= LEAK_ABS_FLOOR_KB,
        "G5(b): leak trend — final-quarter mean footprint {:.0} KB > second-quarter mean {:.0} KB + 10%          AND delta {:.0} KB exceeds the {:.0} KB allocator-noise floor",
        q4_mean,
        q2_mean,
        q4_mean - q2_mean,
        LEAK_ABS_FLOOR_KB
    );

    // (c) responsive after.
    assert_session_responsive(&socket, &env, session, "G5")?;

    // (d) most-recent window intact: last 1000 lines all present.
    let n = count_numbered_lines(&text_done, "soak-", lines - 999, lines, 10);
    assert_eq!(
        n, 1000,
        "G5(d): recent-window zero-drop FAIL — {}/1000 most-recent lines intact",
        n
    );

    fs::write(
        ev.join("g5_result.txt"),
        format!(
            "G5 PASS\nlines={} (gate value 1000000)\nsamples={} (1s cadence; denser than spec's 5s — more evidence)\n\
             peak_rss_kb={} budget_kb={}\nq2_mean_kb={:.0} q4_mean_kb={:.0} (plateau OK)\n\
             responsive=yes\nrecent_window=1000/1000 intact\n",
            lines,
            samples.len(),
            peak_kb,
            budget_kb,
            q2_mean,
            q4_mean
        ),
    )?;

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// G6 — reattach-replay (backlog-completeness, scroll-intact, altscreen-replay)
// ============================================================================

/// G6 — Produce content, detach (zero clients), produce MORE content while
/// detached, cold-reattach. ADR-0004 invariants on the replay:
/// backlog-completeness (detached-production present, most-recent window
/// complete), scroll-intact (pre-detach sentinel present), altscreen-replay
/// (a MAIN-SCREEN session's raw replay carries zero 1049 sequences — the
/// renderer only replays alt state when the inner app is actually in it).
#[test]
fn g6_reattach_replay() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("g6_reattach")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "g6")?;
    let session = "g6";
    create_session(&socket, session)?;
    let ev = evidence_dir("g6");

    // Pre-detach: 1000 numbered lines + sentinel, with an attached client.
    // Wait on the ATTACHED client's own stream — a capture poll would evict it.
    let attached = AttachedClient::attach(&socket, session)?;
    send_to_session(&socket, &env, session, "seq -f 'g6-pre-%04.0f' 1 1000\n")?;
    send_to_session(&socket, &env, session, "echo G6-PRE''DETACH-SENTINEL\n")?;
    {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !attached.captured_text().contains("G6-PREDETACH-SENTINEL") {
            if let Some(e) = attached.error() {
                return Err(format!("G6 pre-phase: attached client errored: {}", e).into());
            }
            if Instant::now() > deadline {
                return Err("G6 pre-phase: sentinel never reached attached client".into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    // Detach (client drops; session lives by construction).
    attached.close();
    std::thread::sleep(Duration::from_millis(300));

    // Detached production: 500 more lines with ZERO clients attached.
    send_to_session(&socket, &env, session, "seq -f 'g6-post-%04.0f' 1 500\n")?;

    // Cold reattach: settled capture of the replay (a mid-production capture
    // satisfies the tail predicate via live-drain but undercounts — observed
    // 23/500; see wait_then_settled_capture war story).
    let cap = wait_then_settled_capture(&socket, session, "g6-post-0500", Duration::from_secs(90))?;
    let raw = capture_session(&socket, session, 150)?;
    fs::write(ev.join("g6_replay.txt"), &cap)?;

    // backlog-completeness: ALL detached-production lines in the replay.
    let n_post = count_numbered_lines(&cap, "g6-post-", 1, 500, 4);
    assert_eq!(
        n_post, 500,
        "G6 backlog-completeness FAIL: {}/500 detached-production lines in replay",
        n_post
    );
    // ... and the pre-detach window too (1000 + 500 + chrome < 10k ring).
    let n_pre = count_numbered_lines(&cap, "g6-pre-", 1, 1000, 4);
    assert_eq!(n_pre, 1000, "G6: {}/1000 pre-detach lines in replay", n_pre);

    // scroll-intact: pre-detach sentinel present.
    let lines: Vec<String> = cap.lines().map(|s| s.to_string()).collect();
    assert_scroll_intact(&lines, "G6-PREDETACH-SENTINEL", "g6")
        .map_err(|e| -> Box<dyn Error> { e.into() })?;

    // altscreen-replay on the raw replay bytes: main-screen session → zero
    // 1049 sequences (pre-replay byte behavior preserved).
    assert_altscreen_replay(&raw.raw_render(), 0, 0, "g6")
        .map_err(|e| -> Box<dyn Error> { e.into() })?;

    fs::write(
        ev.join("g6_result.txt"),
        format!(
            "G6 PASS\nbacklog-completeness: 500/500 detached + 1000/1000 pre-detach\n\
             scroll-intact: G6-PREDETACH-SENTINEL present\naltscreen-replay: raw clean (main screen)\n\
             replay_sha256={}\n",
            sha256(cap.as_bytes())
        ),
    )?;

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// Negative control — drop1000 breaker MUST make the zero-drop comparator fail
// ============================================================================

/// Harness teeth (B1's drop1000 breaker, carried in src/session.rs): with
/// `RETACH_B1_BREAK=drop1000` in the DAEMON env, every 1000th PTY output byte
/// is dropped before the screen model. The same zero-drop comparator used by
/// G4-secondary/G6 must PASS on a healthy daemon and FAIL on the broken one.
/// If the broken daemon passes, the harness has no teeth and NO gate result
/// counts (spec pass (a) item 1).
///
/// Note: the breaker is on the PTY OUTPUT path, so it bites the
/// scrollback/backlog comparators (G4-secondary, G6) — G4's PRIMARY
/// (input-side byte-exact via tee file) is by design untouched by an
/// output-path breaker. Same mapping as B1.
#[test]
fn negative_control_breaker_bites() -> Result<(), Box<dyn Error>> {
    let ev = evidence_dir("negctl");
    let lines = 3000usize; // ~66KB of output → ~66 dropped bytes under breaker

    // Control arm: healthy daemon — comparator must pass.
    let healthy_count = {
        let jail = setup_jail("negctl_healthy")?;
        let env = jail_env(&jail);
        let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "nc")?;
        create_session(&socket, "nc")?;
        send_to_session(
            &socket,
            &env,
            "nc",
            &format!("seq -f 'nc-%05.0f-MARK' 1 {}\n", lines),
        )?;
        let text = wait_for_text(
            &socket,
            "nc",
            Duration::from_secs(120),
            Duration::from_millis(500),
            |t| t.contains(&format!("nc-{:05}-MARK", lines)),
        )?;
        let n = count_numbered_lines(&text, "nc-", 1, lines, 5);
        teardown_jail(&jail)?;
        n
    };
    assert_eq!(
        healthy_count, lines,
        "negctl CONTROL ARM failed: healthy daemon dropped lines ({}/{}) — comparator or mux broken",
        healthy_count, lines
    );

    // Breaker arm: RETACH_B1_BREAK=drop1000 in the daemon env — comparator MUST fail.
    let broken_count = {
        let jail = setup_jail("negctl_broken")?;
        let mut env = jail_env(&jail);
        env.push(("RETACH_B1_BREAK".to_string(), "drop1000".to_string()));
        let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "nc")?;
        create_session(&socket, "nc")?;
        send_to_session(
            &socket,
            &env,
            "nc",
            &format!("seq -f 'nc-%05.0f-MARK' 1 {}\n", lines),
        )?;
        // The final line itself may be corrupted by the breaker; wait for the
        // capture to stabilize instead of waiting for an exact marker.
        std::thread::sleep(Duration::from_secs(5));
        let mut last = String::new();
        for _ in 0..20 {
            let t = capture_session(&socket, "nc", 200)?.text();
            if !t.is_empty() && t.len() == last.len() {
                break;
            }
            last = t;
            std::thread::sleep(Duration::from_secs(1));
        }
        let n = count_numbered_lines(&last, "nc-", 1, lines, 5);
        teardown_jail(&jail)?;
        n
    };

    fs::write(
        ev.join("negctl_result.txt"),
        format!(
            "NEGATIVE CONTROL\nhealthy arm: {}/{} lines intact (comparator passes)\n\
             breaker arm (RETACH_B1_BREAK=drop1000): {}/{} lines intact\n\
             TEETH REQUIREMENT: breaker arm MUST be < {} — {}\n",
            healthy_count,
            lines,
            broken_count,
            lines,
            lines,
            if broken_count < lines {
                "BITES (PASS)"
            } else {
                "NO TEETH (FAIL)"
            }
        ),
    )?;

    // THE TEETH ASSERTION: the breaker must visibly corrupt the output stream.
    assert!(
        broken_count < lines,
        "NEGATIVE CONTROL FAILED: broken daemon passed the zero-drop comparator \
         ({}/{} lines intact) — the harness has NO TEETH and no gate result counts",
        broken_count,
        lines
    );

    Ok(())
}

// ============================================================================
// Leak-guard negative control (orc-2 rider R1 on the 10MB-floor recalibration)
// ============================================================================

/// Process memory footprint in KB — COMPRESSION-IMMUNE (flake-rootcause pass).
///
/// WHY NOT `ps rss` (the old sampler): resident-set size stops counting
/// retained-but-idle pages the moment the macOS compressor (or Linux swap)
/// takes them. In the 3865fe9 ratchet red, a genuine ~61MB retention measured
/// ~20MB short at completion (q2 42619 → q4 51629 KB, delta 9011 < the 10240
/// floor) — the pages were RETAINED but no longer RESIDENT under ambient
/// memory pressure, so the rss-based quarter-delta guard went blind. A real
/// G5 leak could hide the same way.
///
/// macOS: `proc_pid_rusage` `ri_phys_footprint` (kernel ledger: resident dirty
/// + compressed). Linux: `VmRSS + VmSwap` from /proc/<pid>/status.
fn process_footprint_kb(pid: u32) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::proc_pid_rusage(
                pid as libc::c_int,
                libc::RUSAGE_INFO_V2,
                &mut info as *mut libc::rusage_info_v2 as *mut libc::rusage_info_t,
            )
        };
        if ret == 0 {
            Some(info.ri_phys_footprint / 1024)
        } else {
            None
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let field = |name: &str| -> u64 {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
                .unwrap_or(0)
        };
        Some(field("VmRSS:") + field("VmSwap:"))
    }
}

/// Drive the ~61MB retention ramp in LOCKSTEP and return one footprint sample
/// per confirmed chunk (flake-rootcause pass, member 4).
///
/// WHY LOCKSTEP (vs the old free-running paced shell loop + wall-clock 1s
/// sampler): wall-clock sampling is only coupled to ramp progress by scheduler
/// luck — under load the sampler/test thread starves while the shell+daemon
/// run free, so the sampled window lands on a fraction of the ramp and the
/// quarter-based guard goes blind (3865fe9 ratchet; the in-test comment's
/// earlier q2 33.8 → q4 32.5 observation). Here the TEST drives each chunk:
/// send chunk k (50k lines) → wait for its echo-split marker (rendered marker
/// = daemon read+retained through chunk k; marker bytes follow chunk bytes in
/// PTY stream order) → take ONE sample. The shell is idle between sends, so
/// sample k reflects exactly post-chunk-k retention — quarters index ramp
/// PROGRESS by construction, under ANY load. 16 samples: q2 = post-chunks 5-8
/// (~+25MB), q4 = post-chunks 13-16 (~+55MB), delta ~30MB >> the 10MB floor.
fn run_leak_ramp_lockstep(
    socket: &std::path::Path,
    env: &[(String, String)],
    session: &str,
    daemon_pid: u32,
) -> Result<Vec<u64>, Box<dyn Error>> {
    const CHUNKS: usize = 16;
    const LINES_PER_CHUNK: usize = 50_000;
    let payload = "Y".repeat(56);
    let mut samples = Vec::with_capacity(CHUNKS);
    for k in 1..=CHUNKS {
        let lo = (k - 1) * LINES_PER_CHUNK + 1;
        let hi = k * LINES_PER_CHUNK;
        // Marker is quote-split in the command (LEAK-CHUNK-k-DO''NE) so the
        // PTY echo of the command line itself can never satisfy the wait.
        send_to_session(
            socket,
            env,
            session,
            &format!(
                "seq -f 'leak-%010.0f-{}' {} {}; echo LEAK-CHUNK-{}-DO''NE\n",
                payload, lo, hi, k
            ),
        )?;
        let marker = format!("LEAK-CHUNK-{}-DONE", k);
        wait_for_text(
            socket,
            session,
            Duration::from_secs(120),
            Duration::from_millis(500),
            |t| t.contains(&marker),
        )
        .map_err(|e| format!("leak ramp: chunk {} marker not seen: {}", k, e))?;
        if let Some(kb) = process_footprint_kb(daemon_pid) {
            samples.push(kb);
        }
    }
    Ok(samples)
}

/// Shared quarter-math + evidence writer for the two leak-guard controls.
/// Guard math is EXACTLY G5(b): q4_mean > q2_mean * 1.10 AND delta > 10240KB.
fn leak_guard_eval(
    samples: &[u64],
    ev: &std::path::Path,
    tag: &str,
    injected: bool,
) -> Result<(f64, f64, bool), Box<dyn Error>> {
    let sample_log: String = samples
        .iter()
        .enumerate()
        .map(|(i, kb)| format!("{}\t{}\n", i, kb))
        .collect();
    fs::write(ev.join("leakctl_rss_samples.tsv"), &sample_log)?;
    assert!(
        samples.len() >= 8,
        "{tag}: too few footprint samples ({})",
        samples.len()
    );
    let q = samples.len() / 4;
    let mean = |s: &[u64]| s.iter().sum::<u64>() as f64 / s.len().max(1) as f64;
    let q2_mean = mean(&samples[q..2 * q]);
    let q4_mean = mean(&samples[3 * q..]);
    const LEAK_ABS_FLOOR_KB: f64 = 10.0 * 1024.0;
    let guard_fires = q4_mean > q2_mean * 1.10 && (q4_mean - q2_mean) > LEAK_ABS_FLOOR_KB;
    fs::write(
        ev.join("leakctl_result.txt"),
        format!(
            "LEAK-GUARD CONTROL ({tag})\n\
             retention injected: {} (800000 lines ~61MB through the PTY)\n\
             metric: phys_footprint (resident+compressed; macOS proc_pid_rusage / Linux VmRSS+VmSwap)\n\
             sampling: generation-keyed lockstep, one sample per confirmed 50k-line chunk\n\
             q2_mean_kb={:.0} q4_mean_kb={:.0} delta_kb={:.0}\n\
             guard (10% relative AND >10240KB absolute): {}\n",
            if injected { "SBMUX_TEST_LEAK=retain" } else { "NO (control-of-the-control)" },
            q2_mean,
            q4_mean,
            q4_mean - q2_mean,
            if guard_fires { "FIRES" } else { "did not fire" }
        ),
    )?;
    Ok((q2_mean, q4_mean, guard_fires))
}

/// The G5 leak guard was recalibrated (10% relative AND >10MB absolute floor)
/// after allocator-noise false positives — an amendment is only valid if the
/// amended guard still has teeth. This test injects GENUINE retention
/// (`SBMUX_TEST_LEAK=retain`: the daemon keeps a copy of every PTY chunk,
/// never freed) and asserts the recalibrated guard condition FIRES on the
/// lockstep-sampled footprint series. 800k lines × ~80B ≈ 61MB retained — far
/// past the 10MB floor and the 10% relative bound. If this guard does NOT
/// fire here, the G5 criterion has no teeth and no G5 result counts.
#[test]
fn leak_guard_negative_control_fires() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("leakctl")?;
    let mut env = jail_env(&jail);
    env.push(("SBMUX_TEST_LEAK".to_string(), "retain".to_string()));
    let (daemon, socket) = start_daemon_in_jail(&jail, &env, "lk")?;
    let session = "lk";
    create_session(&socket, session)?;
    let ev = evidence_dir("leakctl");

    let samples = run_leak_ramp_lockstep(&socket, &env, session, daemon.pid)?;
    let (q2_mean, q4_mean, guard_fires) = leak_guard_eval(&samples, &ev, "leakctl", true)?;

    assert!(
        guard_fires,
        "LEAK-GUARD NEGATIVE CONTROL FAILED: genuine ~61MB retention did not trip the \
         recalibrated guard (q2 {:.0} KB → q4 {:.0} KB, delta {:.0} KB) — the G5 leak \
         criterion has NO TEETH and no G5 result counts",
        q2_mean,
        q4_mean,
        q4_mean - q2_mean
    );

    teardown_jail(&jail)?;
    Ok(())
}

/// Control-of-the-control (flake-rootcause pass): the SAME lockstep harness
/// with NO retention injected must NOT fire the guard. Proves the firing
/// control's signal comes from the injected retention — not from ambient
/// daemon growth at identical volume (screen + bounded scrollback stay ~flat
/// across 800k lines) and not from footprint-metric noise. Together the pair
/// pins the guard from both sides: genuine retention → fires; same workload
/// without retention → quiet.
#[test]
fn leak_guard_no_retention_control_quiet() -> Result<(), Box<dyn Error>> {
    let jail = setup_jail("leakctl_quiet")?;
    let env = jail_env(&jail); // NO SBMUX_TEST_LEAK
    let (daemon, socket) = start_daemon_in_jail(&jail, &env, "lkq")?;
    let session = "lkq";
    create_session(&socket, session)?;
    let ev = evidence_dir("leakctl_quiet");

    let samples = run_leak_ramp_lockstep(&socket, &env, session, daemon.pid)?;
    let (q2_mean, q4_mean, guard_fires) = leak_guard_eval(&samples, &ev, "leakctl_quiet", false)?;

    assert!(
        !guard_fires,
        "NO-RETENTION CONTROL FAILED: the guard fired without injected retention \
         (q2 {:.0} KB → q4 {:.0} KB, delta {:.0} KB) — the negative control's pass \
         would be meaningless (ambient growth alone trips it)",
        q2_mean,
        q4_mean,
        q4_mean - q2_mean
    );

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// Protocol version negotiation — clean refusal both directions
// ============================================================================

/// Old-version client → modern daemon: the daemon must reply with a FRAMED
/// error naming both versions, promptly — not hang, not silently drop.
/// (Unit-level coverage in server::client_handler tests; this is the
/// integration proof against a real jailed daemon over a real socket.)
#[tokio::test]
async fn version_negotiation_old_client_refused() -> Result<(), Box<dyn Error>> {
    use sbmux::protocol::codec::FrameReader;
    use sbmux::protocol::handshake::PREAMBLE_MAGIC;
    use sbmux::protocol::ServerMsg;
    use tokio::io::AsyncWriteExt;

    let jail = setup_jail("vers_old_client")?;
    let env = jail_env(&jail);
    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "vers")?;
    let ev = evidence_dir("version-negotiation");

    let refusal = tokio::time::timeout(Duration::from_secs(15), async {
        // ECONNREFUSED-retry at connect (punch item 16, launcher-lane parallel).
        // `start_daemon_in_jail` returns on socket-FILE existence, not accept-
        // readiness; under full-suite load the freshly-spawned daemon can have
        // its socket bound yet momentarily refuse a connect before its accept
        // loop is scheduled — a transient ECONNREFUSED, not a dead daemon (this
        // row's raw os-111 victim). One refusal is not death: retry with backoff
        // until it accepts. A genuinely dead socket refuses every retry and the
        // bounded deadline below (then the outer 15s) still fails honestly.
        let connect_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match tokio::net::UnixStream::connect(&socket).await {
                Ok(s) => break s,
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionRefused
                        && tokio::time::Instant::now() < connect_deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err::<ServerMsg, Box<dyn Error>>(e.into()),
            }
        };
        // Old-version preamble: correct magic, version 0.
        let mut preamble = [0u8; 5];
        preamble[..4].copy_from_slice(&PREAMBLE_MAGIC);
        preamble[4] = 0;
        stream.write_all(&preamble).await?;

        let mut frames = FrameReader::new();
        loop {
            if let Some(msg) = frames.decode_next::<ServerMsg>()? {
                return Ok::<ServerMsg, Box<dyn Error>>(msg);
            }
            if !frames.fill_from(&mut stream).await? {
                return Err("server closed without a framed refusal".into());
            }
        }
    })
    .await
    .map_err(|_| "daemon HUNG instead of refusing old-version client")??;

    match refusal {
        ServerMsg::Error(e) => {
            assert!(
                e.contains("version mismatch") && e.contains("client v0"),
                "refusal must name the mismatch, got: {}",
                e
            );
            fs::write(
                ev.join("old_client_refusal.txt"),
                format!("framed refusal received within bound (15s): {}\n", e),
            )?;
        }
        other => return Err(format!("expected framed Error, got {:?}", other).into()),
    }

    teardown_jail(&jail)?;
    Ok(())
}

/// Modern client → old daemon (symmetric direction): simulated by a mock
/// daemon that refuses v1 with a framed error (exactly what a v0 daemon
/// built from this lineage does — the Error variant index is frozen, see
/// PROTOCOL.md). The client must surface the refusal as a clean error,
/// promptly — not hang.
#[tokio::test]
async fn version_negotiation_modern_client_clean_error() -> Result<(), Box<dyn Error>> {
    use sbmux::protocol::{self, ServerMsg};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let tmp = tempfile::tempdir()?;
    let sock = tmp.path().join("oldd.sock");
    let listener = tokio::net::UnixListener::bind(&sock)?;
    let ev = evidence_dir("version-negotiation");

    // Mock old daemon: read 5-byte preamble, refuse with a framed Error, then
    // HOLD the connection open until the client closes — dropping immediately
    // races the client's in-flight Connect write into an EPIPE before it can
    // read the refusal (observed on macOS CI).
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut preamble = [0u8; 5];
        stream.read_exact(&mut preamble).await.unwrap();
        let resp = protocol::encode(&ServerMsg::Error(
            "protocol version mismatch: client v1, server v0 — refusing connection".into(),
        ))
        .unwrap();
        stream.write_all(&resp).await.unwrap();
        // Drain until client EOF (bounded by the test's own 5s timeout).
        let mut sink = [0u8; 1024];
        while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
    });

    // Modern client connects and attempts a session connect; must get the
    // refusal as a clean Err within the timeout (not a hang/panic).
    // (Error stringified inside the closure: Box<dyn Error> is not Send.)
    let result: Result<(), String> = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::spawn_blocking({
            let sock = sock.clone();
            move || {
                capture_session(&sock, "any", 0)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
        }),
    )
    .await
    .map_err(|_| "client HUNG on old-daemon refusal")??;

    let err = result.expect_err("client must error on version refusal, not succeed");
    assert!(
        err.contains("version mismatch"),
        "client must surface the framed refusal, got: {}",
        err
    );
    fs::write(
        ev.join("modern_client_refusal.txt"),
        format!("client surfaced clean error within bound (15s): {}\n", err),
    )?;

    server.await?;
    Ok(())
}

// ============================================================================
// Detach-by-construction — kill -9 a real attached client process
// ============================================================================

/// Spawn a REAL `sbmux attach` client process on its own PTY, kill -9 it,
/// and prove: daemon survives, session child survives, reattach is clean with
/// state preserved (pre-kill sentinel in replay), session still responsive.
#[test]
fn detach_by_construction_kill9() -> Result<(), Box<dyn Error>> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let jail = setup_jail("detach_kill9")?;
    let env = jail_env(&jail);
    let (daemon, socket) = start_daemon_in_jail(&jail, &env, "dk9")?;
    let session = "dk9";
    create_session(&socket, session)?;
    let ev = evidence_dir("detach-by-construction");

    // State to preserve across the kill.
    send_to_session(&socket, &env, session, "echo DK9-PRE''KILL-SENTINEL\n")?;
    wait_for_text(
        &socket,
        session,
        Duration::from_secs(30),
        Duration::from_millis(300),
        |t| t.contains("DK9-PREKILL-SENTINEL"),
    )?;

    // Session child pid (the shell) — must survive the client kill.
    let sessions = list_sessions(&socket)?;
    let child_pid = sessions
        .iter()
        .find(|s| s.name == session)
        .ok_or("session not in list")?
        .pid;

    // Real attach client on its own PTY (raw mode needs a tty).
    let pty = native_pty_system().openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut cmd = CommandBuilder::new(sbmux_binary());
    cmd.args(["attach", session]);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("PATH", "/usr/bin:/bin");
    let mut client_child = pty.slave.spawn_command(cmd)?;
    let client_pid = client_child.process_id().ok_or("no client pid")?;

    // Let it attach (give it time to complete the preamble + Connect).
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        pid_alive(client_pid),
        "attach client died before kill -9 (attach failed?)"
    );

    // kill -9 the client, then DROP THE PTY PAIR before reaping. War story
    // (two failed attempts): (1) blocking wait() hung while the master stayed
    // open; (2) after SIGKILL the client sat in exit state `?Es` ("trying to
    // exit") and try_wait never reaped — macOS wedges a PTY-attached process's
    // exit until the master side closes. kill -0 can't be the oracle either
    // (zombies still pass it). Order matters: kill, close PTY, then reap.
    std::process::Command::new("/bin/kill")
        .args(["-9", &client_pid.to_string()])
        .status()?;
    drop(pty); // close master+slave so the killed client's exit can complete
    let kill_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if client_child.try_wait()?.is_some() {
            break; // reaped: kill -9 confirmed
        }
        if Instant::now() > kill_deadline {
            let ps = std::process::Command::new("/bin/ps")
                .args(["-o", "pid,stat,command", "-p", &client_pid.to_string()])
                .output()?;
            return Err(format!(
                "client not reapable 10s after kill -9 + PTY close; ps says: {}",
                String::from_utf8_lossy(&ps.stdout).trim()
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));

    // Daemon + child survive.
    assert!(pid_alive(daemon.pid), "DAEMON DIED after client kill -9");
    assert!(
        pid_alive(child_pid),
        "SESSION CHILD DIED after client kill -9"
    );

    // Reattach clean: state preserved.
    let text = wait_for_text(
        &socket,
        session,
        Duration::from_secs(30),
        Duration::from_millis(300),
        |t| t.contains("DK9-PREKILL-SENTINEL"),
    )
    .map_err(|e| format!("reattach after kill -9 lost pre-kill state: {}", e))?;
    fs::write(ev.join("dk9_reattach.txt"), &text)?;

    // Still responsive.
    assert_session_responsive(&socket, &env, session, "DK9")?;

    fs::write(
        ev.join("dk9_result.txt"),
        format!(
            "DETACH-BY-CONSTRUCTION PASS\nclient pid {} kill -9'd\n\
             daemon pid {} survived\nsession child pid {} survived\n\
             reattach: DK9-PREKILL-SENTINEL preserved\nresponsive after: yes\n",
            client_pid, daemon.pid, child_pid
        ),
    )?;

    teardown_jail(&jail)?;
    Ok(())
}

// ============================================================================
// Jail safety self-test
// ============================================================================

// ============================================================================
// C1 D1 — socket-dir override CROSSES the process boundary (R26 keystone)
// + protocol v2 library surface (GetHistory / CreateDetached / list `created`)
// ============================================================================

/// Bug-D keystone (foreshadowing G-CRUD): launch the daemon with an EXPLICIT
/// `--socket-dir` pointing somewhere the env tiers would NEVER resolve to, and
/// assert the daemon binds THERE (engine-resolved dir == daemon-bound dir).
/// Then drive the v2 library surface — CreateDetached, GetHistory, and list
/// (with the daemon-populated `created`) — against that override per-call.
#[test]
fn d1_socket_dir_override_crosses_process_boundary() -> Result<(), Box<dyn Error>> {
    use libmod::client::{sbmux_binary, sweep_orphan_daemons, DaemonGuard};
    use std::process::{Command, Stdio};

    let jail = setup_jail("d1_socketdir_override")?;
    let env = jail_env(&jail);

    // An EXPLICIT socket dir that is NOT $XDG_RUNTIME_DIR/sbmux (the env tier).
    // If the daemon ignored --socket-dir and re-resolved from env, the socket
    // would land at the env default and this assert would fail. WS-C M3b: the
    // daemon is PER-SESSION now (binds `<dir>/<name>.sock`, not `sbmux.sock`).
    // SHORT session name: the per-session leaf `<name>.sock` now eats the
    // sun_path budget, and the explicit dir nests deep under the jail root — a
    // full session_prefix-length name would overflow the 104-byte budget. Each
    // jail is dir-isolated already, so a short literal is sufficient here.
    let name = "d1det".to_string();
    let explicit_dir = jail.jail_root.join("explicit_mux");
    let explicit_sock = explicit_dir.join(format!("{name}.sock"));
    let env_default_sock = jail.socket_dir.join(format!("{name}.sock"));

    sweep_orphan_daemons();
    let _ = fs::remove_file(&explicit_sock);

    let mut cmd_env: Vec<(String, String)> = env.clone();
    if !cmd_env.iter().any(|(k, _)| k == "PATH") {
        cmd_env.push(("PATH".into(), "/usr/bin:/bin".into()));
    }
    // Long claim timeout: the pre-spawned daemon starts EMPTY and would otherwise
    // reap itself before create_detached_session claims it.
    cmd_env.push(("SBMUX_CLAIM_TIMEOUT_MS".into(), "60000".into()));
    let daemon = Command::new(sbmux_binary())
        .arg("server")
        .arg("--socket-dir")
        .arg(&explicit_dir)
        .arg("--session")
        .arg(&name)
        .env_clear()
        .envs(cmd_env.iter().cloned())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let daemon_pid = daemon.id();
    std::mem::forget(daemon); // let it daemonize; reaped via guard below
    let _guard = DaemonGuard { pid: daemon_pid };

    // Wait for the socket at the EXPLICIT dir.
    let start = Instant::now();
    while !explicit_sock.exists() {
        if start.elapsed() > Duration::from_secs(5) {
            return Err(format!(
                "daemon did not bind --socket-dir target {} within 5s",
                explicit_sock.display()
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // KEYSTONE: bound exactly at the override (per-session leaf), NOT at the env tier.
    assert!(
        explicit_sock.exists(),
        "daemon must bind the --socket-dir override (per-session leaf)"
    );
    assert!(
        !env_default_sock.exists(),
        "daemon must NOT fall back to the env-tier socket dir when --socket-dir is given"
    );

    // Drive the PER-SESSION library surface against the override per-call. The
    // daemon is already up, so create_detached_session's
    // ensure_session_server_running is a no-op (it handshakes the live socket)
    // and never tries to cold-start via the test binary.
    let rt = tokio::runtime::Runtime::new()?;
    let dir = Some(explicit_dir.as_path());
    let marker_dir = jail.jail_root.join("det_cwd");
    fs::create_dir_all(&marker_dir)?;
    let marker_cwd = std::fs::canonicalize(&marker_dir)?;

    rt.block_on(async {
        // CreateDetached: spawn a session that writes its cwd and some output.
        // Emit a SCROLLED marker (pushed into scrollback by >1 screenful of
        // filler) AND a FRESH marker that stays on the visible bottom rows — the
        // v2 GetHistory composition (scrollback + visible) must surface BOTH.
        let acked = sbmux::client::session_client::create_detached_session(
            dir,
            None,
            &name,
            "pwd -P > cwd.out; echo D1_BACKLOG_LINE; for i in $(seq 1 60); do echo filler_$i; done; echo D1_VISIBLE_LINE; sleep 30",
            marker_cwd.clone(),
            1000,
        )
        .await
        .expect("create_detached_session");
        assert_eq!(acked, name);
    });

    // The detached command ran in the EXPLICIT cwd.
    let marker = marker_cwd.join("cwd.out");
    let start = Instant::now();
    while !marker.exists() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(marker.exists(), "detached command did not run in cwd");
    assert_eq!(
        fs::read_to_string(&marker)?.trim(),
        marker_cwd.to_string_lossy()
    );

    // scan_sessions: the session is listed with a daemon-populated `created`.
    let sessions = rt.block_on(sbmux::client::discovery::scan_sessions(dir))?;
    let info = sessions
        .iter()
        .find(|s| s.name == name)
        .expect("detached session should be listed");
    assert!(
        info.created.is_some(),
        "daemon must populate SessionInfo.created"
    );

    // GetHistory one-shot (no attach): composition = scrollback + visible. Both
    // the SCROLLED marker and the FRESH visible marker must be present.
    let history = rt.block_on(async {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let h = sbmux::client::session_client::get_history_session(dir, &name)
                .await
                .expect("get_history_session");
            let joined = h
                .iter()
                .map(|l| String::from_utf8_lossy(l).to_string())
                .collect::<Vec<_>>()
                .join("\n");
            // Wait until the visible marker (the last thing echoed) lands.
            if joined.contains("D1_VISIBLE_LINE") || Instant::now() > deadline {
                break joined;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    });
    assert!(
        history.contains("D1_BACKLOG_LINE"),
        "GetHistory should return the scrolled-back line, got: {history:?}"
    );
    assert!(
        history.contains("D1_VISIBLE_LINE"),
        "GetHistory must return the FRESH visible line (boot-answerer case), got: {history:?}"
    );

    // GetHistory altscreen arm: a marker drawn by a fullscreen (alt-screen) app
    // must still appear — content inspection, not replay. Cheap: send raw alt
    // + marker bytes to the same session via the one-shot send path.
    rt.block_on(async {
        sbmux::client::session_client::send_input_session(
            dir,
            None,
            &name,
            // Suspend the foreground `sleep`, enter alt screen, draw a marker.
            b"\x1a\x1b[?1049h\x1b[2J\x1b[HD1_ALT_DIALOG".to_vec(),
        )
        .await
        .expect("send_input_session");
    });
    let alt_history = rt.block_on(async {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let h = sbmux::client::session_client::get_history_session(dir, &name)
                .await
                .expect("get_history_session");
            let joined = h
                .iter()
                .map(|l| String::from_utf8_lossy(l).to_string())
                .collect::<Vec<_>>()
                .join("\n");
            if joined.contains("D1_ALT_DIALOG") || Instant::now() > deadline {
                break joined;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    });
    assert!(
        alt_history.contains("D1_ALT_DIALOG"),
        "GetHistory must surface an alt-screen dialog marker, got: {alt_history:?}"
    );

    // Teardown: kill the session via the data API (per-target, not a sweep),
    // then teardown the jail (sweeps the override socket dir too).
    rt.block_on(sbmux::client::session_client::kill_session_session(
        dir, &name,
    ))
    .ok();
    drop(_guard); // SIGTERM/SIGKILL the daemon before removing the jail tree
    teardown_jail(&jail)?;
    Ok(())
}

#[test]
fn test_jail_refusal() {
    // Fail-closed: jail setup refuses production paths; HOME is jailed.
    let jail = setup_jail("refusal_test").expect("jail setup");
    assert!(
        jail.assert_established().is_ok(),
        "jail should be established"
    );
    assert!(
        !jail
            .home
            .to_string_lossy()
            .contains(&jail.real_home.to_string_lossy().to_string()),
        "jailed HOME should differ from REAL_HOME"
    );
    teardown_jail(&jail).expect("teardown");
}
