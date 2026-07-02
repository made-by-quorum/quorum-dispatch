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
