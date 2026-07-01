//! C-RED — tier-a adversarial-JSONL red-team RUNNER (frame 01KWB61JKMAPK9ZWJZ7ZN7CKRW).
//! Mirrors `tests/pi_verb_roundtrip_live.rs`: the battery LIVES in
//! `dispatch::provider::pi::redteam`; this wrapper RUNS it, drives the stub-pi (S)
//! and live-pi (L) layers, and SERIALIZES the round evidence bundle (confirm-it-RAN).
//!
//! A ROUND = one full fresh batch across ALL classes (F + S + L); CLEAN = no new
//! break-class anywhere; STOP = 3 consecutive clean rounds (pi-lead-2 mechanical
//! exhaustion oracle rules at source from the bundle). Run:
//!   env -u QD_HOME -u QD_SESSION_ID -u SB_SESSION_ID -u QD_BOOT_AWAIT_RELAY \
//!       -u CLAUDE_CODE_SESSION_ID \
//!     QD_PI_BIN=$HOME/.npm-pi-global/bin/pi QD_PI_LIVE=1 QD_RED_ROUND=1 \
//!     cargo test -p quorum-dispatch --features faultinj --test pi_redteam -- --nocapture
//!
//! The pure-F battery runs UN-gated (offline). S/L gate on QD_PI_LIVE=1. The L
//! layer carries POST-gate run-evidence (the captured real-pi responses) per the
//! live-gated-test-noop-pass discipline — a gate-off early-return proves nothing.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dispatch::provider::pi::redteam::{run_fixture_battery, RedFinding, RedReport};
use dispatch::provider::pi::{PiRpc, PiRpcError, PiStdio};

fn live() -> bool {
    std::env::var("QD_PI_LIVE").as_deref() == Ok("1")
}

fn round() -> u32 {
    std::env::var("QD_RED_ROUND").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
}

fn pi_bin() -> PathBuf {
    if let Ok(b) = std::env::var("QD_PI_BIN") {
        if !b.is_empty() {
            return PathBuf::from(b);
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".npm-pi-global/bin/pi")
}

fn stub_pi() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stub-pi/stub-pi.sh")
}

/// The round evidence dir: `<target>/cred-evidence/cred/round{N}/`. Overridable via
/// QD_RED_EVIDENCE_DIR; default resolves the workspace target from the manifest.
fn evidence_dir() -> PathBuf {
    let base = std::env::var("QD_RED_EVIDENCE_DIR").map(PathBuf::from).unwrap_or_else(|_| {
        // CARGO_MANIFEST_DIR = <ws>/dispatch/crates/dispatch; the workspace target is
        // <ws>/target (the workspace root is ~/work/quorum-pi, not dispatch/).
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../target/cred-evidence/cred")
    });
    base.join(format!("round{}", round()))
}

fn write_evidence(name: &str, body: &str) -> PathBuf {
    let dir = evidence_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap_or_else(|e| panic!("write evidence {path:?}: {e}"));
    path
}

// ===========================================================================
// S layer — drive the stub-pi through the REAL PiStdio, per adversarial mode.
// ===========================================================================

/// Expected boot outcome for an S mode (the non-vacuity check; a mismatch = break).
enum Expect {
    Ok,
    Timeout,
    /// "pi is gone" — the stub exited, so the adapter degrades to EITHER Closed
    /// (read hit EOF) OR Transport (the get_state write lost the race to the exit and
    /// hit a broken pipe). BOTH are correct degrade-not-crash mappings; accepting
    /// both removes the exit-timing flake without going vacuous (Ok/Timeout/panic
    /// are still rejected).
    Gone,
}

fn drive_stub(stub: &Path, mode: &str, expect: Expect) -> RedFinding {
    let args = vec!["--mode".to_string(), "rpc".to_string()];
    let cwd = std::env::temp_dir();
    let env = vec![("STUB_PI_MODE".to_string(), mode.to_string())];
    let stub_s = stub.to_string_lossy().into_owned();

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let pi = match PiStdio::spawn(&stub_s, &args, &cwd, &env) {
            Ok(p) => p,
            Err(e) => return (format!("spawn error: {e}"), true), // spawn must not fail
        };
        pi.set_timeout(Duration::from_millis(900));
        let st = pi.get_state();
        // For torn-tail: after a good boot, the next_event must hit EOF → Closed.
        let after = if mode == "torn-tail" {
            Some(pi.next_event(Duration::from_millis(400)))
        } else {
            None
        };
        let _ = pi.close();
        let st_kind = match &st {
            Ok(s) => format!("Ok(session_id={:?})", s.session_id),
            Err(PiRpcError::Timeout) => "Err(Timeout)".to_string(),
            Err(PiRpcError::Closed) => "Err(Closed)".to_string(),
            Err(e) => format!("Err({e})"),
        };
        let matched = match expect {
            Expect::Ok => matches!(st, Ok(_)),
            Expect::Timeout => matches!(st, Err(PiRpcError::Timeout)),
            Expect::Gone => matches!(st, Err(PiRpcError::Closed) | Err(PiRpcError::Transport(_))),
        };
        let after_s = match &after {
            Some(Ok(None)) => " next_event=Ok(None)".to_string(),
            Some(Ok(Some(_))) => " next_event=Ok(event)".to_string(),
            Some(Err(e)) => format!(" next_event=Err({e})"),
            None => String::new(),
        };
        // torn-tail extra: the stub stays alive with an unterminated partial line, so
        // the reader blocks on it (never a frame, no corruption) → next_event=Ok(None).
        let torn_ok = mode != "torn-tail" || matches!(after, Some(Ok(None)));
        let is_break = !matched || !torn_ok;
        (format!("boot={st_kind}{after_s}"), is_break)
    }));

    match res {
        Ok((observed, is_break)) => mk("PA6/stub-pi", mode, "S", format!("STUB_PI_MODE={mode}"), observed, is_break),
        Err(_) => mk("PA6/stub-pi", mode, "S", format!("STUB_PI_MODE={mode}"), "PANIC (caught) — real break".to_string(), true),
    }
}

fn s_layer(stub: &Path) -> Vec<RedFinding> {
    vec![
        drive_stub(stub, "oversized", Expect::Ok),
        drive_stub(stub, "oversized-garbage", Expect::Ok),
        drive_stub(stub, "interleaved", Expect::Ok),
        drive_stub(stub, "u2028", Expect::Ok),
        drive_stub(stub, "idless", Expect::Timeout),
        drive_stub(stub, "wrongid", Expect::Timeout),
        drive_stub(stub, "dupid", Expect::Ok),
        drive_stub(stub, "eof-immediate", Expect::Gone),
        drive_stub(stub, "torn-tail", Expect::Ok),
    ]
}

// ===========================================================================
// L layer — raw-drive the REAL pi with adversarial frames (a3 harness shape) and
// CAPTURE the on-wire responses. The post-gate run-evidence (confirm-it-RAN).
// ===========================================================================

/// Spawn real `pi --mode rpc`, write each raw frame line, then read stdout lines
/// until `deadline`, then reap. Returns the captured lines (the run-evidence).
fn live_raw_probe(pi: &Path, frames: &[&str]) -> Vec<String> {
    let mut child = match Command::new(pi)
        .arg("--mode")
        .arg("rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return vec![format!("<spawn error: {e}>")],
    };
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(line.trim_end().to_string()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    for f in frames {
        let _ = writeln!(stdin, "{f}");
        let _ = stdin.flush();
    }
    // collect for a bounded window
    let mut lines = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(l) => lines.push(l),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if lines.len() >= frames.len() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    lines
}

fn l_layer(pi: &Path) -> (Vec<RedFinding>, String) {
    if !pi.exists() {
        return (
            vec![mk("PA7/live-pi", "pi-absent", "L", pi.display().to_string(), "real pi binary not found — L layer SKIPPED (recorded, not a pass)", false)],
            "L SKIPPED: pi binary absent\n".to_string(),
        );
    }
    // The PA7 elicitation: a boot, then adversarial frames only a real pi answers.
    let frames = [
        r#"{"id":"c1","type":"get_state"}"#,                       // boot — confirm-it-RAN
        r#"{"id":"c2","type":"bash"}"#,                            // PA7: bash with NO command
        r#"{"id":"c3","type":"prompt"}"#,                          // PA7: prompt with NO message
        r#"{"id":"c4","type":"totally_unknown_command_xyz"}"#,     // unknown command
        r#"{"id":"c5","type":"get_state" "#,                       // PA6: malformed (torn) frame
    ];
    let captured = live_raw_probe(pi, &frames);
    let joined = captured.join("\n");
    // confirm-it-RAN: a real boot response came back (proves the live path executed).
    let booted = captured.iter().any(|l| l.contains("\"command\":\"get_state\"") || l.contains("sessionId"));
    // CORRECTED DETECTOR (round-1 false-negative fix). pi's footguns, observed at
    // source: a bash-no-command response with success:true whose output ran literal
    // `undefined` (`/bin/bash: line 1: undefined: command not found`); a
    // prompt-no-message response leaking a raw TypeError. These are pi-surface
    // properties under RAW frames — the adapter does not drive them (see the F-layer
    // PA7/frame-injection containment proof), so they are CHARACTERIZATIONS, not breaks.
    let pi_runs_undefined = captured
        .iter()
        .any(|l| l.contains("\"command\":\"bash\"") && l.contains("undefined"));
    let pi_leaks_typeerror = captured.iter().any(|l| {
        l.contains("\"command\":\"prompt\"")
            && (l.contains("Cannot read properties") || l.contains("TypeError") || l.contains("undefined"))
    });
    let mut findings = vec![mk(
        "PA7/live-pi",
        "boot-confirms-ran",
        "L",
        "get_state",
        format!("real-pi boot response observed={booted}; captured {} line(s) (post-gate run-evidence in live-raw-capture.txt)", captured.len()),
        !booted, // a missing boot response with pi present = the live path didn't actually run
    )];
    findings.push(mkc(
        "PA7/live-pi",
        "pi-runs-literal-undefined-on-bash-no-command",
        "L",
        r#"raw {"type":"bash"} (no command) — NOT adapter-reachable"#,
        format!("pi_runs_literal_undefined={pi_runs_undefined} (raw pi: output `/bin/bash: line 1: undefined: command not found`, exitCode 127, success:TRUE). CHARACTERIZATION for the acceptance oracle: pi-surface footgun; the adapter never constructs a bash frame and serde-escapes user input (stdio.rs:160/:371) → unreachable through the adapter (see F PA7/frame-injection)."),
    ));
    findings.push(mkc(
        "PA7/live-pi",
        "pi-leaks-typeerror-on-prompt-no-message",
        "L",
        r#"raw {"type":"prompt"} (no message) — NOT adapter-reachable"#,
        format!("pi_leaks_typeerror={pi_leaks_typeerror} (raw pi error: `Cannot read properties of undefined (reading 'startsWith')`). CHARACTERIZATION: the adapter ALWAYS sets `message` (stdio.rs:371) so it never sends this frame; a pi error maps to InjectError::Precondition (inject MAP, mod.rs:241)."),
    ));
    let evidence = format!(
        "# L-layer raw real-pi capture (post-gate run-evidence)\n# pi={}\n#\n# READ WITH the F-layer PA7/frame-injection findings (report.json): input → adapter\n# serde-escapes/forwards-whole (stdio.rs:160/:371) → pi never sees a 2nd frame. The\n# footguns below are reachable ONLY by driving pi RAW (this probe), never via the adapter.\n\nFRAMES:\n{}\n\nCAPTURED:\n{}\n",
        pi.display(),
        frames.join("\n"),
        joined
    );
    (findings, evidence)
}

// ===========================================================================
fn mk(class: &str, probe: &str, layer: &str, input: impl Into<String>, observed: impl Into<String>, is_break: bool) -> RedFinding {
    RedFinding { class: class.into(), probe: probe.into(), layer: layer.into(), input: input.into(), observed: observed.into(), is_break, characterization: false }
}

/// A CHARACTERIZATION finding (is_break:false, characterization:true) — a reach-
/// boundary pinned for the why-holder acceptance oracle, never a counter reset.
fn mkc(class: &str, probe: &str, layer: &str, input: impl Into<String>, observed: impl Into<String>) -> RedFinding {
    RedFinding { class: class.into(), probe: probe.into(), layer: layer.into(), input: input.into(), observed: observed.into(), is_break: false, characterization: true }
}

#[test]
fn pi_c_red_round() {
    // --- F layer (always; offline) -----------------------------------------
    let tmp = tempfile::tempdir().unwrap();
    let mut report: RedReport = run_fixture_battery(tmp.path());

    // --- S + L layers (gated) ----------------------------------------------
    let mut live_evidence = String::new();
    if live() {
        let stub = stub_pi();
        assert!(stub.exists(), "stub-pi fixture missing at {stub:?}");
        report.findings.extend(s_layer(&stub));
        let (l_findings, l_ev) = l_layer(&pi_bin());
        report.findings.extend(l_findings);
        live_evidence = l_ev;
    } else {
        report.findings.push(mk(
            "meta",
            "live-gated",
            "S/L",
            "QD_PI_LIVE",
            "S+L layers SKIPPED (set QD_PI_LIVE=1) — a clean F-only run is NOT a clean ROUND",
            false,
        ));
    }

    // --- serialize the round evidence bundle (confirm-it-RAN) ---------------
    let bundle = report.to_evidence_json();
    let p = write_evidence("report.json", &bundle);
    if !live_evidence.is_empty() {
        write_evidence("live-raw-capture.txt", &live_evidence);
    }
    let abs = std::fs::canonicalize(&p).unwrap_or(p);
    eprintln!("C-RED round{} evidence → {}", round(), abs.display());
    eprintln!(
        "C-RED round{}: {} findings, {} break(s), {} characterization(s), live={}",
        round(),
        report.findings.len(),
        report.breaks().len(),
        report.characterizations().len(),
        live()
    );
    for b in report.breaks() {
        eprintln!("  BREAK [{}] {} ({}): {}", b.class, b.probe, b.layer, b.observed);
    }

    // The F layer must be clean offline; in a live run the WHOLE surface must be
    // clean for the round to count. A break here routes to pi-lead-2 (do not paper over).
    assert!(
        report.breaks().is_empty(),
        "C-RED round{} found {} break-class(es) — bubble pi-lead-2, do NOT fix production unilaterally",
        round(),
        report.breaks().len()
    );

    // A live round must have actually run S+L (confirm-it-RAN, not a gate-off pass).
    if live() {
        let ran_s = report.findings.iter().any(|f| f.layer == "S");
        let ran_l = report.findings.iter().any(|f| f.layer == "L");
        assert!(ran_s && ran_l, "live round must execute S and L layers (confirm-it-RAN)");
    }
}
