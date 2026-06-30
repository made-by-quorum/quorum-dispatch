//! C-CONF — cred-free C0 conformance for the codex adapter, RUN-not-read, driving
//! the real `qd` BINARY by verb name against a LIVE codex daemon and asserting each
//! verb's OBSERVED EFFECT AT SOURCE (registry row / rollout file / process tree / ws
//! endpoint) — NEVER a return string. Gated on `QD_CODEX_LIVE=1`; a no-op otherwise
//! so the default suite never spawns a real codex daemon.
//!
//! WHY BINARY-DRIVEN (vs the library-driven codex_*_live.rs): each `qd` invocation
//! is a SEPARATE process that must re-discover the daemon through the on-disk
//! registry + the `cmdline_is_our_daemon` re-check. That makes reconnect/addressing
//! (item #2, LB#12) a GENUINE cross-invocation property, not a single-process
//! simulation. The verbs are addressed BY NAME exactly as a human/agent drives them.
//!
//! TIER-A SCOPE (cred-free, NO model turn, NO OPENAI_API_KEY / NO OPENROUTER key):
//! the 8 structural rubric items #1,2,3,8,9,10,11,12. send:relay + resume require a
//! model turn (codex writes the rollout file LAZILY on the first turn — W4 finding,
//! see codex_resume_kill_live.rs), so they are tier-b (C-LIVE), excluded here.
//!
//! The jail + binary-driver + at-source readers + reaper + evidence-bundle helpers
//! live in `tests/common/live.rs` (shared with the C-RED / C-CHAOS targets). HOME is
//! load-bearing (L9a/ADD-4) — never the real HOME; a short XDG_RUNTIME_DIR keeps the
//! qrmux socket under the 104-byte sun_path budget; reap is instance-addressed
//! (never a name-addressed pkill) with a no-survivor belt.

mod common;
use common::live::*;

use std::sync::Arc;

// ===========================================================================
// Test 1 — start → addressing/boot/transport/auth/pin → liveness → teardown.
// Covers items #1, #2, #3, #8, #9, #11, #12.
// ===========================================================================
#[test]
fn cconf_start_addressing_boot_transport_teardown_live() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping the codex conformance harness (test 1)");
        return;
    }
    let jail = make_jail("t1");
    let codex_home = jail.join("codex-home");
    let bundle = evidence_dir("t1-start-addressing-boot-teardown");
    let pids = Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
    let _belt = ReapAll(pids.clone());

    // --- VERB: qd start --provider codex (drives #1,#2,#3,#9,#12 at create) ----
    let start = run_qd(
        &jail,
        &["start", "cdx-conf", "--provider", "codex", "--cwd",
          jail.join("work").to_string_lossy().as_ref()],
    );
    assert!(
        start.status.success(),
        "qd start --provider codex against 0.143.0-alpha.14 SUCCEEDS (item #12: the C-PIN \
         prerelease-tolerant parse unblocks create — no VersionUnknown). stderr: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    // AT SOURCE: exactly one live codex row, fully formed.
    let rows = codex_rows(&jail);
    assert_eq!(rows.len(), 1, "exactly one codex daemon row written");
    let row = &rows[0];
    let pid = row.pid.expect("row carries a real pid");
    pids.lock().unwrap().push(pid);

    // #2 launch & addressing — the row carries pid + endpoint + sessionId(thread uuid).
    assert!(pid > 0, "#2 real daemon pid");
    assert_eq!(row.name.as_deref(), Some("cdx-conf"), "#2 row name");
    assert_eq!(row.provider.as_deref(), Some("codex"), "#9 provider codex");
    assert_eq!(row.status.as_deref(), Some("idle"), "#11 fresh = idle");
    let endpoint = row.endpoint.clone().expect("#1 row carries a ws endpoint");
    let thread_id = row.session_id.clone().expect("#2 row carries a thread id");

    // #1 transport — endpoint is ws on loopback, port OUTSIDE the relay range.
    assert!(
        endpoint.starts_with("ws://127.0.0.1:"),
        "#1 ws endpoint: {endpoint}"
    );
    let port = endpoint_port(&endpoint);
    assert!(
        !(8900..=9000).contains(&port),
        "#1 endpoint port {port} OUTSIDE the relay range"
    );
    // #2 birth-id — thread id is a real uuid AND is bound in ids.jsonl.
    assert!(
        thread_id.contains('-') && thread_id.len() >= 32,
        "#2 thread id looks like a real uuid: {thread_id}"
    );
    let ids = state_dir(&jail).join("ids.jsonl");
    let ids_body = std::fs::read_to_string(&ids).unwrap_or_default();
    assert!(
        ids_body.contains(&thread_id),
        "#2 birth-id bound in ids.jsonl (thread {thread_id}): {ids_body}"
    );

    // #3 boot/readiness + #9 auth/config — the daemon is alive and its log shows it
    // bound the endpoint after initialize (create returns only past the gate).
    assert!(
        dispatch::effects::is_pid_alive(pid as i32),
        "#3 daemon alive post-create"
    );
    let log = std::fs::read_to_string(log_dir(&jail).join("codex-cdx-conf.log")).unwrap_or_default();
    assert!(
        log.contains("listening on") || log.contains(&endpoint),
        "#3 daemon log shows it bound the endpoint: {log}"
    );
    // #9 — the live daemon process carries THIS jail's CODEX_HOME (auth/config jail).
    assert!(
        jail_codex_daemon_alive(&codex_home),
        "#9 a codex app-server for the jail CODEX_HOME is alive"
    );

    // #1 transport (live) — a real ws initialize handshake against the endpoint.
    assert!(
        ws_initialize_ok(&endpoint),
        "#1 the app-server/ws transport accepts a live initialize at {endpoint}"
    );

    // --- EVIDENCE (alive snapshot, BEFORE teardown) ---------------------------
    ev_copy(&bundle, &sessions_dir(&jail).join(format!("{pid}.json")), "row.json");
    ev_copy(&bundle, &log_dir(&jail).join("codex-cdx-conf.log"), "daemon.log");
    ev_copy(&bundle, &state_dir(&jail).join("ids.jsonl"), "ids.jsonl");
    ev_text(&bundle, "endpoint.txt", &format!("{endpoint}\nport={port}\nthread_id={thread_id}\n"));
    ev_proctree(&bundle, "proctree-alive.txt", &codex_home);

    // --- VERB: qd info <s> (FRESH proc → #2 reconnect/addressing across invocations)
    let info = run_qd(&jail, &["info", "cdx-conf", "--json"]);
    assert!(
        info.status.success(),
        "#2 a fresh qd info process re-addresses the live session. stderr: {}",
        String::from_utf8_lossy(&info.stderr)
    );

    // --- VERB: qd ls --live (#11 liveness; connectionless status read, no socket) -
    // NOTE: the standalone `qd live` verb is the INTERACTIVE status TUI — in a
    // non-TTY subprocess it (correctly) exits 1 "requires an interactive terminal",
    // so it carries no headless conformance signal. The programmatic liveness probe
    // is `qd ls --live --json` + the at-source pid classifier below.
    let ls_live = run_qd(&jail, &["ls", "--live", "--json"]);
    assert!(
        ls_live.status.success(),
        "#11 qd ls --live runs. stderr: {}",
        String::from_utf8_lossy(&ls_live.stderr)
    );
    // AT SOURCE: while up, the classifier truth is pid-alive + the row present.
    assert!(
        dispatch::effects::is_pid_alive(pid as i32),
        "#11 the daemon is live while ls --live reports"
    );
    // The live ls --json surfaces the codex session id (the row is in the live set).
    let ls_live_out = String::from_utf8_lossy(&ls_live.stdout);
    assert!(
        ls_live_out.contains(&thread_id),
        "#11 qd ls --live surfaces the live codex session {thread_id}: {ls_live_out}"
    );

    // --- VERB: qd stop <s> (#8 teardown) → assert at source: dead + tombstone + clean
    let stop = run_qd(&jail, &["stop", "cdx-conf"]);
    assert!(
        stop.status.success(),
        "#8 qd stop runs. stderr: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    wait_dead(pid);
    assert!(
        !dispatch::effects::is_pid_alive(pid as i32),
        "#8 daemon reaped by qd stop (pid {pid})"
    );
    let tomb = sessions_dir(&jail).join(format!("{pid}.json.tombstoned"));
    assert!(tomb.exists(), "#8 the killed row is tombstoned");
    assert!(
        !sessions_dir(&jail).join(format!("{pid}.json")).exists(),
        "#8 the live row file was consumed by the tombstone rename"
    );
    let tomb_body = std::fs::read_to_string(&tomb).unwrap_or_default();
    assert!(
        tomb_body.contains("\"provider\": \"codex\"") && tomb_body.contains(&endpoint),
        "#8 tombstone carries provider + endpoint: {tomb_body}"
    );
    assert!(
        !jail_codex_daemon_alive(&codex_home),
        "#8 no codex app-server survives the stop (proc tree clean)"
    );

    // --- EVIDENCE (teardown snapshot: tombstone + clean tree + dead pid) -------
    ev_copy(&bundle, &tomb, "tombstone.json");
    ev_proctree(&bundle, "proctree-after-stop.txt", &codex_home);
    ev_text(
        &bundle,
        "teardown.txt",
        &format!("pid {pid} is_pid_alive={} (expect false)\n", dispatch::effects::is_pid_alive(pid as i32)),
    );

    *pids.lock().unwrap() = Vec::new(); // stopped cleanly; the belt is a no-op now
    let _ = std::fs::remove_dir_all(&jail); // jail reclaimed; evidence persisted in `bundle`
}

// ===========================================================================
// Test 2 — multiplex: two codex daemons, one per session (item #10).
// ===========================================================================
#[test]
fn cconf_multiplex_two_daemons_live() {
    if !live() {
        eprintln!("QD_CODEX_LIVE != 1 — skipping the codex conformance harness (test 2)");
        return;
    }
    let jail = make_jail("t2");
    let codex_home = jail.join("codex-home");
    let bundle = evidence_dir("t2-multiplex");
    let pids = Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
    let _belt = ReapAll(pids.clone());

    for name in ["cdx-a", "cdx-b"] {
        let out = run_qd(
            &jail,
            &["start", name, "--provider", "codex", "--cwd",
              jail.join("work").to_string_lossy().as_ref()],
        );
        assert!(
            out.status.success(),
            "#10 qd start {name} succeeds. stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // AT SOURCE: two distinct codex rows — distinct pids, endpoints, ports, threads.
    let rows = codex_rows(&jail);
    assert_eq!(rows.len(), 2, "#10 two distinct codex daemon rows");
    let pid_a = rows[0].pid.unwrap();
    let pid_b = rows[1].pid.unwrap();
    for r in &rows {
        pids.lock().unwrap().push(r.pid.unwrap());
    }
    assert_ne!(pid_a, pid_b, "#10 distinct daemon pids");
    let ep_a = rows[0].endpoint.clone().unwrap();
    let ep_b = rows[1].endpoint.clone().unwrap();
    assert_ne!(ep_a, ep_b, "#10 distinct endpoints");
    assert_ne!(
        endpoint_port(&ep_a),
        endpoint_port(&ep_b),
        "#10 distinct ports (one-daemon-per-session)"
    );
    assert_ne!(
        rows[0].session_id, rows[1].session_id,
        "#10 distinct thread ids"
    );
    assert!(
        dispatch::effects::is_pid_alive(pid_a as i32) && dispatch::effects::is_pid_alive(pid_b as i32),
        "#10 both daemons alive concurrently"
    );

    // --- EVIDENCE (two distinct daemons alive) --------------------------------
    ev_copy(&bundle, &sessions_dir(&jail).join(format!("{pid_a}.json")), "row-a.json");
    ev_copy(&bundle, &sessions_dir(&jail).join(format!("{pid_b}.json")), "row-b.json");
    ev_text(
        &bundle,
        "multiplex.txt",
        &format!(
            "pid_a={pid_a} ep_a={ep_a} port_a={}\npid_b={pid_b} ep_b={ep_b} port_b={}\n",
            endpoint_port(&ep_a),
            endpoint_port(&ep_b)
        ),
    );
    ev_proctree(&bundle, "proctree-alive.txt", &codex_home);

    // Teardown both via the verb (#10/#8); assert each stop verb succeeds (loud, not
    // silent — a failed stop that leaks a daemon must surface, not pass quietly).
    for name in ["cdx-a", "cdx-b"] {
        let stop = run_qd(&jail, &["stop", name]);
        assert!(
            stop.status.success(),
            "#10 qd stop {name} succeeds. stderr: {}",
            String::from_utf8_lossy(&stop.stderr)
        );
    }
    wait_dead(pid_a);
    wait_dead(pid_b);
    assert!(
        !dispatch::effects::is_pid_alive(pid_a as i32) && !dispatch::effects::is_pid_alive(pid_b as i32),
        "#10 both daemons reaped"
    );
    assert!(
        !jail_codex_daemon_alive(&codex_home),
        "#10 no codex app-server survives for the jail CODEX_HOME"
    );

    ev_proctree(&bundle, "proctree-after-stop.txt", &codex_home);

    *pids.lock().unwrap() = Vec::new();
    let _ = std::fs::remove_dir_all(&jail); // jail reclaimed; evidence persisted in `bundle`
}
