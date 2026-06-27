//! B3 R8 — seam-hole probe (EVIDENCE-ONLY, env-gated, NOT a gate row).
//!
//! Spec: `exec/b3-spec.md` R8 (STRETCH). Goal: attempt LOCAL reproduction of
//! the quarantined G3 defect (deterministic final-marker loss on a slow 2-core
//! macOS CI runner: a marker line missing from BOTH history AND the settling
//! screen of a fresh POST-stream capture, while the ATTACHED stream received
//! it) by injecting artificial delay at the suspected replay-write-path race
//! seam, via the env-gated `QRMUX_TEST_SEAM_DELAY_MS` hook (`session.rs`
//! `SeamDelay`).
//!
//! This is a PROBE, not a gate row. The whole-matrix entrypoint
//! `r8_seam_probe_matrix` is `#[ignore]`d so a normal `cargo test` /
//! `cargo test --workspace` gate run never executes it (it must not pollute
//! M4). Run explicitly:
//!
//!   cargo test -p qrmux --test b3_r8_probe -- --ignored --nocapture
//!
//! It drives the SAME real jailed daemon as the gate suite (jail.rs self-jails;
//! no production state is touched — ADD-10; DaemonGuard reaps on drop) and
//! writes ALL raw evidence under `target/test-evidence/r8-probe/`.
//!
//! NO FIX is attempted here. Fix ownership of the quarantined row stays with
//! B2 pass (b) (GATE-B2.md quarantine ledger). Outcome is REPRODUCED (capture
//! evidence) or NOT-REPRODUCED (record the matrix + analysis).

#[path = "lib/mod.rs"]
#[allow(dead_code, unused_imports)] // shared harness; this binary uses a subset
mod libmod;
use libmod::client::{record_attach, strip_ansi, Recorded};
use libmod::*;

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Probe parameters (the timebox: 3 sites × 2 delays = 6 cells, one process).
// ---------------------------------------------------------------------------

/// Sites injected by `SeamDelay` (see session.rs docs):
///   a = send_initial_state, after snapshot / before frame write
///   b = after replay ScreenUpdate / before screen_notify arms the live relay
///   c = PTY reader, after read / before Screen::process
const SITES: [&str; 3] = ["a", "b", "c"];
const DELAYS_MS: [u64; 2] = [20, 100];

/// Lines streamed per cell. G3 used 8000; the probe widens the seam with delay,
/// so a smaller stream keeps each cell fast while still producing a long live
/// stream that overlaps the mid-stream fresh attach. The FINAL marker is the
/// one the CI defect lost.
const N_LINES: usize = 400;
const PREFIX: &str = "r8-line-";
const WIDTH: usize = 5;

fn final_marker() -> String {
    format!("{}{:0w$}", PREFIX, N_LINES, w = WIDTH)
}
fn marker_n(n: usize) -> String {
    format!("{}{:0w$}", PREFIX, n, w = WIDTH)
}

/// Evidence root for the probe: target/test-evidence/r8-probe/<cell>/.
fn evidence_dir(cell: &str) -> PathBuf {
    let dir = PathBuf::from("target/test-evidence/r8-probe").join(cell);
    fs::create_dir_all(&dir).expect("create evidence dir");
    dir
}

/// Count how many of r8-line-1..=N appear in `text`.
fn count_numbered_lines(text: &str, lo: usize, hi: usize) -> usize {
    (lo..=hi).filter(|i| text.contains(&marker_n(*i))).count()
}

/// Outcome of one matrix cell.
struct CellResult {
    site: String,
    delay_ms: u64,
    /// The attached stream saw the final marker (must be true, like the CI
    /// defect: stream received it). If false the cell is inconclusive (the
    /// generator/child didn't complete) — NOT a reproduction.
    stream_saw_final: bool,
    /// Final marker present in the fresh post-stream capture's HISTORY frames.
    final_in_history: bool,
    /// Final marker present in the fresh post-stream capture's settling SCREEN.
    final_in_screen: bool,
    /// Lines present / N in the fresh post-stream capture (history+screen+live).
    lines_present: usize,
    /// REPRODUCED iff the stream saw the final marker but the fresh capture lost
    /// it from BOTH history AND screen (the exact CI defect shape).
    reproduced: bool,
    note: String,
}

/// Run ONE matrix cell against a fresh jailed daemon carrying the seam delay.
fn run_cell(site: &str, delay_ms: u64) -> Result<CellResult, Box<dyn Error>> {
    let cell = format!("{}-{}ms", site, delay_ms);
    let ev = evidence_dir(&cell);
    let jail = setup_jail(&format!("b3_r8_{}", cell.replace('-', "_")))?;

    // Inject the seam delay into the DAEMON env (start_daemon_in_jail env-clears
    // then applies these). Inert in every other process.
    let mut env = jail_env(&jail);
    env.push((
        "QRMUX_TEST_SEAM_DELAY_MS".to_string(),
        format!("{}:{}", site, delay_ms),
    ));

    let (_daemon, socket) = start_daemon_in_jail(&jail, &env, "r8")?;
    let session = "r8";
    create_session(&socket, session)?;

    let final_m = final_marker();

    // 1) Attach a streaming observer, then stream N numbered lines via tee.
    //    The tee file is the oracle: it proves the CHILD produced every line
    //    (so a short fresh capture is a MUX loss, not a child-death).
    let observer = AttachedClient::attach(&socket, session)?;
    let oracle = jail.tmpdir.join("r8_oracle.txt");
    send_to_session(
        &socket,
        &env,
        session,
        &format!(
            "seq -f '{}%0{}.0f' 1 {} | tee {}\n",
            PREFIX,
            WIDTH,
            N_LINES,
            oracle.display()
        ),
    )?;

    // 2) Wait until the stream is genuinely flowing (some mid lines landed on
    //    the observer), THEN do the fresh attach DURING the stream — the
    //    reattach path that exercises send_initial_state concurrently with live
    //    production. This is where sites a/b/c bite.
    let mid = marker_n(N_LINES / 4);
    let mid_deadline = Instant::now() + Duration::from_secs(30);
    let mut mid_seen = false;
    while Instant::now() < mid_deadline {
        if observer.captured_text().contains(&mid) {
            mid_seen = true;
            break;
        }
        if let Some(e) = observer.error() {
            return Err(format!("[{}] observer errored before mid marker: {}", cell, e).into());
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    if !mid_seen {
        // Slow start; proceed anyway — the fresh attach still overlaps the tail.
        fs::write(
            ev.join("NOTE.txt"),
            "mid marker not seen before fresh attach\n",
        )?;
    }

    // The fresh attach DURING the stream. AttachedClient::attach evicts the
    // observer (one-client mux). This live client is the "attached stream" the
    // CI defect says DID receive the final marker.
    let live = AttachedClient::attach(&socket, session)?;
    drop(observer); // evicted; release its worker.

    // 3) Wait for the FINAL marker on the live (re)attached stream.
    let stream_deadline = Instant::now() + Duration::from_secs(60);
    let mut stream_saw_final = false;
    while Instant::now() < stream_deadline {
        if live.captured_text().contains(&final_m) {
            stream_saw_final = true;
            break;
        }
        if let Some(e) = live.error() {
            return Err(format!("[{}] live stream errored: {}", cell, e).into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    fs::write(ev.join("live_stream_tail.txt"), {
        let t = live.captured_text();
        let start = t.len().saturating_sub(2000);
        t[start..].to_string()
    })?;
    live.close();

    let oracle_lines = fs::read_to_string(&oracle)
        .map(|s| s.lines().count())
        .unwrap_or(0);

    // 4) Settle, then the AUTHORITATIVE fresh POST-stream capture. Use the
    //    NON-COLLAPSING recorder so history frames and the settling screen are
    //    inspectable SEPARATELY (the CI defect: missing from BOTH).
    let settled = settle_then_record(&socket, session, &final_m, Duration::from_secs(30))?;
    let hist_lines = settled.history_lines();
    let hist_text: String = hist_lines
        .iter()
        .map(|l| strip_ansi(&String::from_utf8_lossy(l)))
        .collect::<Vec<_>>()
        .join("\n");
    let screen_text = settled
        .screen_update()
        .map(|b| strip_ansi(&String::from_utf8_lossy(b)))
        .unwrap_or_default();

    let final_in_history = hist_text.contains(&final_m);
    let final_in_screen = screen_text.contains(&final_m);
    let combined = format!("{}\n{}", hist_text, screen_text);
    let lines_present = count_numbered_lines(&combined, 1, N_LINES);

    // Raw evidence for the cell.
    fs::write(ev.join("post_stream_history.txt"), &hist_text)?;
    fs::write(ev.join("post_stream_screen.txt"), &screen_text)?;
    fs::write(
        ev.join("frame_structure.txt"),
        format!(
            "{:#?}\n",
            settled.frames.iter().map(frame_summary).collect::<Vec<_>>()
        ),
    )?;

    let reproduced = stream_saw_final && !final_in_history && !final_in_screen;
    let note = if reproduced {
        format!(
            "REPRODUCED: stream saw {final_m}, oracle file has {oracle_lines}/{N_LINES} lines, \
             but fresh capture LOST it from BOTH history and screen ({lines_present}/{N_LINES} present)"
        )
    } else if !stream_saw_final {
        format!(
            "INCONCLUSIVE: live stream did NOT reach {final_m} (oracle file {oracle_lines}/{N_LINES}) — \
             generator/child did not complete; not a mux-loss repro"
        )
    } else {
        format!(
            "NOT REPRODUCED: stream saw {final_m}; fresh capture has it (history={final_in_history}, \
             screen={final_in_screen}); {lines_present}/{N_LINES} lines present, oracle {oracle_lines}/{N_LINES}"
        )
    };
    fs::write(ev.join("RESULT.txt"), format!("cell {}\n{}\n", cell, note))?;

    teardown_jail(&jail)?;

    Ok(CellResult {
        site: site.to_string(),
        delay_ms,
        stream_saw_final,
        final_in_history,
        final_in_screen,
        lines_present,
        reproduced,
        note,
    })
}

fn frame_summary(f: &libmod::client::RecordedFrame) -> String {
    use libmod::client::RecordedFrame::*;
    match f {
        Connected { name, new_session } => format!("Connected{{{name},new={new_session}}}"),
        History(lines) => format!("History[{} lines]", lines.len()),
        ScreenUpdate(b) => format!("ScreenUpdate[{} bytes]", b.len()),
        Passthrough(b) => format!("Passthrough[{} bytes]", b.len()),
    }
}

/// Wait for the marker to appear in a fresh collapsing capture (production
/// landed), then take the authoritative NON-COLLAPSING recording. Polls a
/// throwaway collapsing capture first so we don't record mid-flight.
fn settle_then_record(
    socket: &Path,
    session: &str,
    marker: &str,
    timeout: Duration,
) -> Result<Recorded, Box<dyn Error>> {
    let start = Instant::now();
    loop {
        let cap = capture_session(socket, session, 200)?;
        if cap.text().contains(marker) {
            break;
        }
        if start.elapsed() > timeout {
            // Production never quiesced WITH the marker on the collapsing view —
            // record anyway so the loss (if any) is captured as evidence.
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // Brief settle, then the authoritative fresh attach (non-collapsing).
    std::thread::sleep(Duration::from_millis(300));
    record_attach(socket, session)
}

// ---------------------------------------------------------------------------
// Matrix entrypoint (ignored — probe, not gate).
// ---------------------------------------------------------------------------

#[test]
#[ignore = "B3 R8 probe: run explicitly with --ignored; not a gate row"]
fn r8_seam_probe_matrix() -> Result<(), Box<dyn Error>> {
    let mut results: Vec<CellResult> = Vec::new();
    for site in SITES {
        for delay in DELAYS_MS {
            eprintln!("=== R8 cell: site {} delay {}ms ===", site, delay);
            match run_cell(site, delay) {
                Ok(r) => {
                    eprintln!("    {}", r.note);
                    results.push(r);
                }
                Err(e) => {
                    eprintln!("    CELL ERROR: {}", e);
                    results.push(CellResult {
                        site: site.to_string(),
                        delay_ms: delay,
                        stream_saw_final: false,
                        final_in_history: false,
                        final_in_screen: false,
                        lines_present: 0,
                        reproduced: false,
                        note: format!("CELL ERROR: {}", e),
                    });
                }
            }
        }
    }

    // Matrix table.
    let mut table = String::new();
    table.push_str(&format!(
        "B3 R8 seam-hole probe matrix ({} lines/cell, final marker {})\n",
        N_LINES,
        final_marker()
    ));
    table.push_str(
        "site | delay | stream_saw_final | final_in_history | final_in_screen | lines | verdict\n",
    );
    table.push_str(
        "-----|-------|------------------|------------------|-----------------|-------|--------\n",
    );
    for r in &results {
        let verdict = if r.reproduced {
            "REPRODUCED"
        } else if !r.stream_saw_final {
            "INCONCLUSIVE"
        } else {
            "not-repro"
        };
        table.push_str(&format!(
            "{:>4} | {:>4}ms | {:>16} | {:>16} | {:>15} | {:>3}/{} | {}\n",
            r.site,
            r.delay_ms,
            r.stream_saw_final,
            r.final_in_history,
            r.final_in_screen,
            r.lines_present,
            N_LINES,
            verdict,
        ));
    }
    for r in &results {
        table.push_str(&format!("\n[{}:{}ms] {}\n", r.site, r.delay_ms, r.note));
    }

    let any_repro = results.iter().any(|r| r.reproduced);
    table.push_str(&format!(
        "\nOVERALL: {}\n",
        if any_repro {
            "REPRODUCED (>=1 cell lost the final marker from BOTH history and screen)"
        } else {
            "NOT REPRODUCED (no cell lost the final marker from both surfaces)"
        }
    ));

    let root = PathBuf::from("target/test-evidence/r8-probe");
    fs::create_dir_all(&root)?;
    fs::write(root.join("MATRIX.txt"), &table)?;
    eprintln!("\n{}", table);
    eprintln!("evidence: {}/", root.display());

    // The probe ALWAYS records evidence and does not fail on a reproduction
    // (evidence-only; no fix here). It only fails if a cell could not run at all
    // — already surfaced as a CELL ERROR row above; we keep the test green so
    // the matrix is always written. Reproduction is read from MATRIX.txt.
    Ok(())
}
