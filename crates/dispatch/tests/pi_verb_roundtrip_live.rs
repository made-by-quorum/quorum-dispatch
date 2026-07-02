//! Item 7 — pi tier-(a) LIVE conformance: drive the REAL `qd` verbs BY NAME against
//! a live `pi --mode rpc` for the 8 credential-free rubric items (#1,2,3,8,9,10,11,12),
//! RUN-not-read. The driving + assertions live in
//! [`dispatch::provider::pi::conformance`]; this wrapper supplies the live wiring,
//! runs the sweep, and EMITS the evidence artifact (the LIVE-RUN-EVIDENCE guard:
//! a green is proven by the inspectable report, never inferred from a pass count).
//!
//! CRED-FREE: tier-a touches no model turn, so this needs only the pinned pi 0.80.2
//! (via `QD_PI_BIN`) — NO OAuth. Gated `QD_PI_LIVE=1` (spawns real residents + pi
//! children). Run:
//!   QD_PI_LIVE=1 QD_PI_BIN=~/.npm-pi-global/bin/pi \
//!     env -u QD_HOME -u QD_SESSION_ID -u SB_SESSION_ID -u QD_BOOT_AWAIT_RELAY \
//!         -u CLAUDE_CODE_SESSION_ID \
//!     cargo test -p quorum-dispatch --test pi_verb_roundtrip_live -- --nocapture
//!
//! The real-on-disk-dir `encode_cwd_dir` confirm (needs an actual pi-created dir =
//! a turn = assistant-gated lazy-write) is TIER-B; here the CRED-FREE regex+PA5
//! shape assertion runs (deferred real-dir confirm noted in the report).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use dispatch::paths::QdPaths;
use dispatch::provider::pi::conformance::{run_tier_a, QdRunner, SCRUB_VARS};
use dispatch::provider::pi::{PiRemote, PiRpc};

fn live() -> bool {
    std::env::var("QD_PI_LIVE").as_deref() == Ok("1")
}

/// The pinned pi binary: `QD_PI_BIN` if set, else the quorum-box default
/// (`~/.npm-pi-global/bin/pi`). pi is NOT on PATH.
fn pi_bin() -> PathBuf {
    if let Ok(b) = std::env::var("QD_PI_BIN") {
        if !b.is_empty() {
            return PathBuf::from(b);
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".npm-pi-global/bin/pi")
}

/// The installed `rpc-types.d.ts` for the #12 shape/hash pin: `QD_PI_DTS` if set,
/// else the npm-global install path.
fn d_ts_path() -> PathBuf {
    if let Ok(p) = std::env::var("QD_PI_DTS") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(
        ".npm-pi-global/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/rpc/rpc-types.d.ts",
    )
}

#[test]
fn pi_tier_a_conformance_live() {
    if !live() {
        eprintln!("pi_tier_a_conformance_live: SKIPPED (set QD_PI_LIVE=1 to run the live sweep)");
        return;
    }
    let pi = pi_bin();
    assert!(
        pi.exists(),
        "pinned pi binary not found at {} (set QD_PI_BIN)",
        pi.display()
    );
    let dts = d_ts_path();
    let qd = PathBuf::from(env!("CARGO_BIN_EXE_qd"));

    // Isolated HOME (a tempdir) → the registry + pi sessions live entirely inside it;
    // QdRunner also truly-unsets the 5 session vars per spawn (the preregistered scrub).
    let home = tempfile::tempdir().expect("tempdir HOME");
    let runner = QdRunner::new(qd, pi, home.path().to_path_buf());

    let report = run_tier_a(&runner, Some(&dts));

    // LIVE-RUN-EVIDENCE guard: write the inspectable artifact + print its path, so a
    // green is RECONSTRUCTABLE from observed state, never a bare pass count.
    let evidence_path = std::env::var("QD_PI_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.path().join("pi-tier-a-evidence.json"));
    let json = report.to_evidence_json();
    let _ = std::fs::write(&evidence_path, &json);
    eprintln!("=== pi tier-(a) conformance evidence: {} ===", evidence_path.display());
    eprintln!("{json}");

    // Rule on the report.
    if !report.all_green() {
        let fails: Vec<String> = report
            .failures()
            .iter()
            .map(|r| format!("  #{} FAIL: {} | observed: {}", r.item, r.detail, r.observed))
            .collect();
        panic!(
            "pi tier-(a) conformance NOT green ({} item-results failed):\n{}",
            report.failures().len(),
            fails.join("\n")
        );
    }
    eprintln!(
        "pi tier-(a) conformance GREEN: {} item-results all passed (8 rubric items).",
        report.items.len()
    );
}

// ===========================================================================
// Item 7 (tier-b) — pi LIVE SEND TURN: drive the WIRED `qd send:relay` arm against a
// live CREDENTIALED pi resident, RUN-not-read. Proves the A5 send wiring end-to-end:
//   * WIRING FLOOR (always asserted when gated): the arm RESOLVES the resident +
//     CONNECTS + `PiProvider::inject` MINTS a turn id (`qd send:relay` exits 0 and
//     prints a non-empty turn id) — the creds-free proof the wiring is exercised.
//   * FULL (cred present): the real turn COMPLETES. DEC-1 signal-B reads is_streaming
//     BUSY DURING then IDLE AFTER via a ws `get_state` poll — the A2 mechanism, since
//     main derives the row status ON-READ (the resident pushes no status stream). The
//     turn OUTCOME (a NEW assistant message) lands in the pi TRANSCRIPT — NOT the send
//     client (`PiRemote::next_event` is `Ok(None)` by design; events flow to pi's own
//     session jsonl). Outcome is read from the transcript, per the charge.
//
// TIER-B needs a real model turn ⇒ a CREDENTIALED pi (its OWN `~/.pi/agent` OAuth —
// NEVER claude cred, NEVER a cred-swap). DISTINCT gate `QD_PI_LIVE_TIERB=1` (tier-a's
// `QD_PI_LIVE` is cred-free). The resident inherits OAuth from a `~/.pi/agent`
// assembled in the isolated tempdir HOME by SYMLINKING the real `auth.json`/
// `settings.json` (pi's own cred, read-only). qd's registry + pi's session transcript
// stay in the tempdir (`PI_CODING_AGENT_SESSION_DIR`). Bounded spend: one tiny turn.
//   QD_PI_LIVE_TIERB=1 QD_PI_BIN=~/.npm-pi-global/bin/pi \
//     cargo test -p quorum-dispatch --test pi_verb_roundtrip_live \
//       pi_tier_b_send_turn_live -- --nocapture --test-threads=1

fn tierb_live() -> bool {
    std::env::var("QD_PI_LIVE_TIERB").as_deref() == Ok("1")
}

/// Assemble a `~/.pi/agent` under `home` carrying the REAL pi OAuth by symlinking the
/// live `auth.json` (+ `settings.json`) from the invoking user's `$HOME/.pi/agent`.
/// Returns `true` when a real `auth.json` was found and linked (cred present). This is
/// pi's OWN credential — no claude cred, no swap; the link is read-only.
fn link_real_pi_cred(home: &Path) -> bool {
    let real_home = std::env::var("HOME").unwrap_or_default();
    let real_agent = PathBuf::from(&real_home).join(".pi/agent");
    let real_auth = real_agent.join("auth.json");
    if real_auth.metadata().is_err() {
        return false; // cred absent → PARTIAL floor
    }
    let dst_agent = home.join(".pi/agent");
    if std::fs::create_dir_all(&dst_agent).is_err() {
        return false;
    }
    // Symlink auth.json (required) + settings.json (best-effort, for provider/model).
    let _ = std::os::unix::fs::symlink(&real_auth, dst_agent.join("auth.json"));
    let real_settings = real_agent.join("settings.json");
    if real_settings.exists() {
        let _ = std::os::unix::fs::symlink(&real_settings, dst_agent.join("settings.json"));
    }
    dst_agent.join("auth.json").exists()
}

/// Count `message`-lines whose `message.role == "assistant"` across every `*.jsonl`
/// under `dir` (recursive). Whitespace-normalized substring match — no serde needed;
/// robust to pi's jsonl layout. The COUNT growth (before→after) is the transcript
/// proof a real assistant reply landed (the turn OUTCOME, read from the transcript).
fn count_assistant_messages(dir: &Path) -> usize {
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
                        let compact: String =
                            line.chars().filter(|c| !c.is_whitespace()).collect();
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

#[test]
fn pi_tier_b_send_turn_live() {
    if !tierb_live() {
        eprintln!(
            "pi_tier_b_send_turn_live: SKIPPED (set QD_PI_LIVE_TIERB=1 to run the live send turn)"
        );
        return;
    }
    let pi = pi_bin();
    assert!(pi.exists(), "pinned pi binary not found at {} (set QD_PI_BIN)", pi.display());
    let qd = PathBuf::from(env!("CARGO_BIN_EXE_qd"));

    // Isolated HOME (tempdir): qd registry lives here; pi transcript lives in `sess_dir`.
    let home = tempfile::tempdir().expect("tempdir HOME");
    let home_path = home.path().to_path_buf();
    let sess_dir = home_path.join("pi-sessions");
    std::fs::create_dir_all(&sess_dir).expect("mk sess dir");

    // Wire pi's OWN OAuth into the isolated HOME (symlinked, read-only). cred_present
    // distinguishes A5-FULL (real turn) from the A5-PARTIAL wiring floor.
    let cred_present = link_real_pi_cred(&home_path);

    let cwd = home_path.to_string_lossy().into_owned();
    let name = "pi-tierb";

    // A scrubbed `qd` command under the isolated HOME + the pinned pi + the isolated pi
    // session dir (so the transcript is findable). The 5 hermeticity vars are unset.
    let qd_cmd = |args: &[&str]| -> Command {
        let mut c = Command::new(&qd);
        c.args(args)
            .env("HOME", &home_path)
            .env("QD_PI_BIN", &pi)
            .env("PI_CODING_AGENT_SESSION_DIR", &sess_dir);
        for v in SCRUB_VARS {
            c.env_remove(v);
        }
        c
    };

    // --- start the pi resident -------------------------------------------------
    let start_out = qd_cmd(&["start", name, "--provider", "pi", "--cwd", &cwd])
        .output()
        .expect("spawn qd start");
    assert!(
        start_out.status.success(),
        "qd start --provider pi failed: exit={:?}\nstderr={}",
        start_out.status.code(),
        String::from_utf8_lossy(&start_out.stderr)
    );

    // Resolve the row (RUN-not-read: the endpoint is observed at the registry source).
    let sessions_dir = QdPaths::from_home(&home_path).sessions_dir;
    let (endpoint, session_id) = read_pi_row(&sessions_dir, name)
        .expect("pi row with endpoint after start");
    assert!(endpoint.starts_with("ws://"), "row endpoint not a ws url: {endpoint}");

    // Ensure we always tear the resident down (no leaked pi child), even on panic.
    // NB `qd kill` is RETIRED ("use qd stop") — teardown MUST use `qd stop` (pid-based).
    struct Killer<'a> {
        run: &'a dyn Fn(&[&str]) -> Command,
        name: &'a str,
    }
    impl Drop for Killer<'_> {
        fn drop(&mut self) {
            let mut c = (self.run)(&["stop", self.name]);
            let _ = c.output();
        }
    }
    let _killer = Killer { run: &qd_cmd, name };

    // --- pre-send state: Idle (is_streaming == false) via a ws get_state poll -----
    // The resident fronts ONE ws client at a time, so this probe CONNECTS + pokes +
    // CLOSES before the send — never held across the send's own connect.
    let idle_before = match PiRemote::connect(&endpoint, Duration::from_secs(5)) {
        Ok(c) => {
            let idle = matches!(c.get_state(), Ok(st) if !st.is_streaming);
            let _ = c.close();
            idle
        }
        Err(_) => false,
    };

    let assistant_before = count_assistant_messages(&sess_dir);

    // --- drive the WIRED send arm (the A5 delta) -------------------------------
    let prompt = "In 2-3 short sentences, explain why an idempotent operation is safe \
                  to retry. Answer directly with no tool use.";
    let send_out = qd_cmd(&["send:relay", name, prompt]).output().expect("spawn qd send");
    let send_exit = send_out.status.code();
    let turn_id = String::from_utf8_lossy(&send_out.stdout).trim().to_string();
    let send_stderr = String::from_utf8_lossy(&send_out.stderr).trim().to_string();

    // WIRING FLOOR (A5-PARTIAL): the arm resolved the resident, connected, and inject
    // minted a turn id. Always asserted when gated.
    assert_eq!(
        send_exit,
        Some(0),
        "qd send:relay (pi arm) did not exit 0: exit={send_exit:?}\nstderr={send_stderr}"
    );
    assert!(
        !turn_id.is_empty(),
        "qd send:relay (pi arm) printed no turn id (wiring not exercised)\nstderr={send_stderr}"
    );

    // --- DEC-1 signal-B: BUSY during, IDLE after (ws get_state poll) ------------
    // The send client has closed its connection, so the single-client front is now free
    // for this poll connection (opened AFTER the send, held through the turn).
    let poll = PiRemote::connect(&endpoint, Duration::from_secs(5))
        .expect("connect post-send poll client to resident");
    let mut busy_seen = false;
    let mut idle_after = false;
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        match poll.get_state() {
            Ok(st) => {
                if st.is_streaming {
                    busy_seen = true;
                } else if busy_seen {
                    idle_after = true;
                    break;
                }
            }
            Err(_) => {} // transient during a streaming turn; keep polling
        }
        std::thread::sleep(Duration::from_millis(75));
    }
    let _ = poll.close();

    // --- turn OUTCOME from the TRANSCRIPT (not the send client) -----------------
    let assistant_after = count_assistant_messages(&sess_dir);
    let transcript_grew = assistant_after > assistant_before;

    // --- evidence artifact (LIVE-RUN-EVIDENCE: green is reconstructable) --------
    let tier = if cred_present && busy_seen && idle_after && transcript_grew {
        "A5-FULL"
    } else {
        "A5-PARTIAL(wiring-floor)"
    };
    let evidence = format!(
        "{{\n  \"tier\": \"{tier}\",\n  \"cred_present\": {cred_present},\n  \
         \"session_id\": \"{session_id}\",\n  \"endpoint\": \"{endpoint}\",\n  \
         \"send_exit\": {send_exit:?},\n  \"turn_id\": \"{turn_id}\",\n  \
         \"idle_before\": {idle_before},\n  \"busy_during\": {busy_seen},\n  \
         \"idle_after\": {idle_after},\n  \"assistant_before\": {assistant_before},\n  \
         \"assistant_after\": {assistant_after},\n  \"transcript_grew\": {transcript_grew}\n}}"
    );
    let evidence_path = std::env::var("QD_PI_TIERB_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_path.join("pi-tier-b-evidence.json"));
    let _ = std::fs::write(&evidence_path, &evidence);
    eprintln!("=== pi tier-(b) send-turn evidence: {} ===", evidence_path.display());
    eprintln!("{evidence}");

    // --- rulings ---------------------------------------------------------------
    assert!(idle_before, "resident was not Idle before the send (is_streaming true)");
    if cred_present {
        // A5-FULL: a real credentialed turn must complete end-to-end.
        assert!(busy_seen, "signal-B: never observed is_streaming BUSY during the turn");
        assert!(idle_after, "signal-B: resident did not return to IDLE after the turn (timeout)");
        assert!(
            transcript_grew,
            "turn OUTCOME: no NEW assistant message in the transcript \
             (before={assistant_before} after={assistant_after})"
        );
        eprintln!("pi tier-(b) A5-FULL GREEN: send arm wired, real turn completed, signal-B proven.");
    } else {
        eprintln!(
            "pi tier-(b) A5-PARTIAL: wiring floor proven (send minted turn id {turn_id}); \
             cred absent → real-turn completion deferred (durable dogfood cred-gap)."
        );
    }
}

// ===========================================================================
// A6.1 — pi STRUCTURED FLOOR (Shape S) integrated continuity + DEAD-ONLY trigger,
// RUN-not-read. Drives the WIRED `qd send:relay` pi arm end-to-end and proves:
//   * DEAD-ONLY trigger (super-22 cond 8): an ALIVE identity-verified resident is
//     driven through the rpc path (NEVER floors); a PROVABLY-DEAD resident (killed
//     pid, row not tombstoned) DROPS to the structured `-p --mode json` floor.
//   * INTEGRATED CONTINUITY (acceptance 1, BLOCKING): ≥2 turns THROUGH the floor
//     lane (the run_pi_send → floor::run_floor_turn path, NOT a standalone `pi -p`);
//     turn-2 RECALLS turn-1's codeword; the floor session persists as a SINGLE
//     appended file (no fork) under a dedicated per-qd-session dir.
//   * SCRAPE-FREE capture: the outcome is parsed from the `turn_end`/`agent_end`
//     ndjson by floor::parse_floor_stdout (the driver the arm calls).
//   * NON-VACUITY: the floor-drop stderr line is asserted (the run actually took the
//     floor branch, never a skip-mode no-op).
// TIER: needs pi's OWN `~/.pi/agent` OAuth for a real recall turn (A6-FULL); cred
// absent ⇒ A6-PARTIAL (trigger + drop observed; recall deferred). NEVER claude/swap.
// DISTINCT gate `QD_PI_FLOOR_LIVE=1`. Bounded spend: ~3 tiny turns.
//   QD_PI_FLOOR_LIVE=1 QD_PI_BIN=~/.npm-pi-global/bin/pi \
//     cargo test -p quorum-dispatch --test pi_verb_roundtrip_live \
//       pi_floor_continuity_live -- --nocapture --test-threads=1

fn floor_live() -> bool {
    std::env::var("QD_PI_FLOOR_LIVE").as_deref() == Ok("1")
}

/// Count every `*.jsonl` under `dir` (recursive). A single file across ≥2 floor
/// turns is the "single appended session, no fork" continuity proof.
fn count_jsonl_files(dir: &Path) -> usize {
    let mut n = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                n += 1;
            }
        }
    }
    n
}

/// True iff some `assistant`-role message across the `*.jsonl` under `dir` contains
/// `needle`. This proves RECALL (turn-2's assistant reply carries the codeword) —
/// distinct from turn-1's USER message which also contains it, so we require the
/// `assistant` role on the SAME line.
fn assistant_message_contains(dir: &Path, needle: &str) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p) {
                for line in text.lines() {
                    let compact: String =
                        line.chars().filter(|c| !c.is_whitespace()).collect();
                    if compact.contains("\"role\":\"assistant\"") && compact.contains(needle) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Read `(endpoint, sessionId, pid)` for a pi row by name. RUN-not-read.
fn read_pi_row_with_pid(sessions_dir: &Path, name: &str) -> Option<(String, String, i64)> {
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
        let pid = extract_json_num(&compact, "pid").unwrap_or(0);
        return Some((endpoint, session_id, pid));
    }
    None
}

/// Pull a numeric `"<key>":<number>` out of a whitespace-compacted JSON string.
fn extract_json_num(compact: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{key}\":");
    let start = compact.find(&needle)? + needle.len();
    let rest = &compact[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[test]
fn pi_floor_continuity_live() {
    if !floor_live() {
        eprintln!(
            "pi_floor_continuity_live: SKIPPED (set QD_PI_FLOOR_LIVE=1 to run the integrated floor turn)"
        );
        return;
    }
    let pi = pi_bin();
    assert!(pi.exists(), "pinned pi binary not found at {} (set QD_PI_BIN)", pi.display());
    let qd = PathBuf::from(env!("CARGO_BIN_EXE_qd"));

    let home = tempfile::tempdir().expect("tempdir HOME");
    let home_path = home.path().to_path_buf();
    let sess_dir = home_path.join("pi-sessions");
    std::fs::create_dir_all(&sess_dir).expect("mk sess dir");
    let cred_present = link_real_pi_cred(&home_path);
    let cwd = home_path.to_string_lossy().into_owned();
    let name = "pi-floor";

    let qd_cmd = |args: &[&str]| -> Command {
        let mut c = Command::new(&qd);
        c.args(args)
            .env("HOME", &home_path)
            .env("QD_PI_BIN", &pi)
            .env("PI_CODING_AGENT_SESSION_DIR", &sess_dir);
        for v in SCRUB_VARS {
            c.env_remove(v);
        }
        c
    };

    // --- start a real pi resident (ALIVE) --------------------------------------
    let start_out = qd_cmd(&["start", name, "--provider", "pi", "--cwd", &cwd])
        .output()
        .expect("spawn qd start");
    assert!(
        start_out.status.success(),
        "qd start --provider pi failed: exit={:?}\nstderr={}",
        start_out.status.code(),
        String::from_utf8_lossy(&start_out.stderr)
    );
    let sessions_dir = QdPaths::from_home(&home_path).sessions_dir;
    let (endpoint, session_id, pid) =
        read_pi_row_with_pid(&sessions_dir, name).expect("pi row after start");
    assert!(endpoint.starts_with("ws://"), "row endpoint not ws: {endpoint}");
    assert!(pid > 0, "row pid not recorded (cannot prove dead-vs-alive)");

    // Teardown guard (belt-and-suspenders; the group kill below already reaps).
    struct Killer<'a> {
        run: &'a dyn Fn(&[&str]) -> Command,
        name: &'a str,
    }
    impl Drop for Killer<'_> {
        fn drop(&mut self) {
            let _ = (self.run)(&["stop", self.name]).output();
        }
    }
    let _killer = Killer { run: &qd_cmd, name };

    // --- CO-FIRE ISOLATION (super-22 cond 8, ALIVE side): a send against the LIVE
    //     identity-verified resident is driven through the rpc path and NEVER floors.
    let alive_send = qd_cmd(&["send:relay", name, "Reply with only: OK. No tool use."])
        .output()
        .expect("spawn alive send");
    let alive_stderr = String::from_utf8_lossy(&alive_send.stderr).to_string();
    assert_eq!(
        alive_send.status.code(),
        Some(0),
        "alive send (rpc path) did not exit 0: stderr={alive_stderr}"
    );
    assert!(
        !alive_stderr.contains("dropped to the structured"),
        "ALIVE resident FLOORED — dead-only trigger violated (single-writer-safety break)\nstderr={alive_stderr}"
    );

    // --- kill the resident's process GROUP → PROVABLY DEAD (row not tombstoned) --
    // The neg-pgid goes via `kill -9 -- -<pgid>` (the external-kill neg-pgid misparse
    // guard); the resident is its own pgid leader, so `-pid` is the group (reaps the
    // resident + its pi rpc child, no orphan leak).
    let _ = Command::new("kill")
        .args(["-9", "--", &format!("-{pid}")])
        .output();
    let mut dead = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !dispatch::effects::is_pid_alive(pid as i32) {
            dead = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(dead, "resident pid {pid} still alive after group kill — cannot prove DEAD-ONLY trigger");

    // The floor's dedicated per-qd-session dir (isolated from the resident's sessions
    // under sess_dir root). Count/recall across the whole qd-floor tree so the proof
    // does not depend on the exact session-id derivation.
    let floor_root = sess_dir.join("qd-floor");
    let codeword = "ZEPHYR";

    // --- TURN 1 THROUGH THE FLOOR: seed the codeword ----------------------------
    let t1_prompt =
        format!("Remember this codeword for later: {codeword}. Reply with only: OK. No tool use.");
    let t1 = qd_cmd(&["send:relay", name, &t1_prompt]).output().expect("spawn floor t1");
    let t1_stderr = String::from_utf8_lossy(&t1.stderr).to_string();
    // NON-VACUITY + DEAD-ONLY (dead side): the send took the floor branch.
    assert!(
        t1_stderr.contains("dropped to the structured"),
        "turn-1 did NOT drop to the floor against a dead resident (trigger not exercised)\nstderr={t1_stderr}"
    );

    // --- TURN 2 THROUGH THE FLOOR: recall (continuity via -c + same dir) ---------
    let t2_prompt = "What was the codeword I asked you to remember? Reply with only that word. No tool use.";
    let t2 = qd_cmd(&["send:relay", name, t2_prompt]).output().expect("spawn floor t2");
    let t2_stderr = String::from_utf8_lossy(&t2.stderr).to_string();
    assert!(
        t2_stderr.contains("dropped to the structured"),
        "turn-2 did NOT drop to the floor\nstderr={t2_stderr}"
    );

    // --- continuity: SINGLE appended session file (no fork) + assistant RECALL ---
    let session_files = count_jsonl_files(&floor_root);
    let recalled = assistant_message_contains(&floor_root, codeword);

    let tier = if cred_present && session_files == 1 && recalled {
        "A6-FULL"
    } else {
        "A6-PARTIAL(trigger+delivery)"
    };
    let evidence = format!(
        "{{\n  \"tier\": \"{tier}\",\n  \"cred_present\": {cred_present},\n  \
         \"session_id\": \"{session_id}\",\n  \"resident_pid\": {pid},\n  \
         \"resident_killed_dead\": {dead},\n  \"alive_send_floored\": {},\n  \
         \"t1_exit\": {:?},\n  \"t2_exit\": {:?},\n  \"t1_dropped_to_floor\": {},\n  \
         \"t2_dropped_to_floor\": {},\n  \"floor_session_files\": {session_files},\n  \
         \"assistant_recalled_codeword\": {recalled}\n}}",
        alive_stderr.contains("dropped to the structured"),
        t1.status.code(),
        t2.status.code(),
        t1_stderr.contains("dropped to the structured"),
        t2_stderr.contains("dropped to the structured"),
    );
    let evidence_path = std::env::var("QD_PI_FLOOR_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_path.join("pi-a6-floor-evidence.json"));
    let _ = std::fs::write(&evidence_path, &evidence);
    eprintln!("=== pi A6.1 floor continuity evidence: {} ===", evidence_path.display());
    eprintln!("{evidence}");

    // --- rulings ---------------------------------------------------------------
    if cred_present {
        assert_eq!(t1.status.code(), Some(0), "turn-1 floor delivery did not exit 0\nstderr={t1_stderr}");
        assert_eq!(t2.status.code(), Some(0), "turn-2 floor delivery did not exit 0\nstderr={t2_stderr}");
        assert_eq!(
            session_files, 1,
            "continuity: floor forked into {session_files} session files (want 1 single appended file)"
        );
        assert!(
            recalled,
            "continuity: turn-2 did NOT recall codeword {codeword} through the integrated floor lane"
        );
        eprintln!(
            "pi A6.1 floor A6-FULL GREEN: alive⇒rpc (no floor); dead⇒floor; 2 turns through the floor, single-file continuity + recall proven."
        );
    } else {
        eprintln!(
            "pi A6.1 floor A6-PARTIAL: DEAD-ONLY trigger proven (alive⇒no-floor, dead⇒floor drop-log observed x2); \
             cred absent → recall/single-file completion deferred (durable cred-gap)."
        );
    }
}

/// Read a pi registry row by `name` from `sessions_dir`, returning `(endpoint, sessionId)`.
/// RUN-not-read: the row is the observed effect. Minimal string parse (no serde in tests).
fn read_pi_row(sessions_dir: &Path, name: &str) -> Option<(String, String)> {
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

/// Pull `"<key>":"<value>"` out of a whitespace-compacted JSON string.
fn extract_json_str(compact: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = compact.find(&needle)? + needle.len();
    let rest = &compact[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}
