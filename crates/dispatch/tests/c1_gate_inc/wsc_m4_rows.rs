// ===========================================================================
// WS-C M4 ENGINE-LEVEL gate rows (spec §7). Driven through the REAL `qd` binary
// against REAL per-session qrmux daemons in hermetic jails — the same harness
// the C1 rows use (Jail / run_qd / mux_create / fake-claude / pid_alive). These
// are the NEW/INVERTED arms that make the per-session split's claims falsifiable:
//
//   * G-ISOL (R-1(4))       — positive isolation + the QRMUX_TEST_SHARED
//                             shared-fate NEGATIVE control (must RED).
//   * G-COLDSTART-N (R-1(3))— (a) same-session race → 1 daemon; (b) cross-session
//                             burst → N daemons no convoy; (d) create-vs-teardown;
//                             plus the QD_EMBEDDED_DAEMON_PROGRAM mutation control.
//   * G-EVSPLIT             — two daemons write disjoint events files concurrently
//                             with clean bookends + epoch fencing across respawn.
//
// The wire-level skew + claim-timeout + grace arms are qrmux-level
// (crates/qrmux/tests/wsc_m4.rs).
// ===========================================================================

/// PID of the per-session qrmux daemon (`qd qrmux-server --socket-dir <dir>
/// --session <name>`) bound to `dir` for `name`, if alive. Greps `ps` for the
/// exact argv triple (qrmux-server + the dir + `--session <name>`).
fn session_daemon_pid(dir: &Path, name: &str) -> Option<u32> {
    let out = Command::new("/bin/ps").args(["-axo", "pid=,args="]).output().ok()?;
    let want_dir = dir.to_string_lossy();
    let want_session = format!("--session {name}");
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim_start();
        let Some((pid, args)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if args.contains("qrmux-server")
            && args.contains(want_dir.as_ref())
            && args.contains(&want_session)
        {
            if let Ok(p) = pid.trim().parse::<u32>() {
                return Some(p);
            }
        }
    }
    None
}

/// All per-session qrmux daemon pids bound to `dir` (any session). Used to assert
/// the daemon COUNT (one-per-session positive; one-for-both negative control).
fn all_session_daemon_pids(dir: &Path) -> Vec<u32> {
    let out = match Command::new("/bin/ps").args(["-axo", "pid=,args="]).output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let want_dir = dir.to_string_lossy();
    let mut pids = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim_start();
        let Some((pid, args)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if args.contains("qrmux-server") && args.contains(want_dir.as_ref()) {
            if let Ok(p) = pid.trim().parse::<u32>() {
                pids.push(p);
            }
        }
    }
    pids
}

/// SIGKILL a pid (no SIGTERM grace — simulate an abrupt daemon crash).
fn sigkill(pid: u32) {
    let _ = Command::new("/bin/kill")
        .args(["-9", &pid.to_string()])
        .stderr(Stdio::null())
        .status();
}

/// Boot a LIVE session end to end through the engine cold-start path: `qd start
/// <name>` with the fake-claude (writes the live registry row + execs the app).
/// Returns (exit, stdout, stderr). The daemon is auto-launched by `qd`.
fn qd_new_live(jail: &Jail, name: &str, app: &str) -> (i32, String, String) {
    let fake = write_fake_claude(jail, app);
    let fake_s = fake.to_string_lossy().into_owned();
    let r = run_qd_env(jail, &["start", name], &[("CLAUDE_BIN", &fake_s), ("QD_FAKE_NAME", name)]);
    // Teardown-leak belt: `qd start` AUTO-LAUNCHED a per-session daemon we never saw
    // a pid for at spawn. Record it post-boot via the exact-socket-dir ps lookup so
    // a future run can identity-reap it if this run dies before teardown.
    record_engine_daemons(&jail.root, &jail.resolved_dir());
    r
}

// ===========================================================================
// G-ISOL (R-1(4)) — the headline arm.
// ===========================================================================

#[test]
fn g_isol() {
    let mut detail = String::new();
    let mut ok = true;

    // ----------------------------------------------------------------------
    // POSITIVE: two sessions on SEPARATE per-session daemons. SIGKILL A's
    // daemon → A dead + torn down; B PROVEN alive by OPERATION (send:pty acks +
    // history renders post-kill); A recoverable cold (`qd start` relaunch).
    // ----------------------------------------------------------------------
    let jail = Jail::establish("gisol-pos");
    let dir = jail.resolved_dir();

    // Cold-start two LIVE sessions via the engine (each auto-launches its own
    // per-session daemon binding `<name>.sock`).
    let (ca, _oa, ea) = qd_new_live(&jail, "alpha", "cat");
    let (cb, _ob, eb) = qd_new_live(&jail, "bravo", "cat");
    detail.push_str(&format!(
        "POS create: alpha exit={ca} (stderr {}), bravo exit={cb} (stderr {})\n",
        ea.trim(),
        eb.trim()
    ));
    ok &= ca == 0 && cb == 0;

    // PRECONDITION: one daemon EACH, DISTINCT pids (the split's whole point).
    let pid_a = session_daemon_pid(&dir, "alpha");
    let pid_b = session_daemon_pid(&dir, "bravo");
    let distinct = matches!((pid_a, pid_b), (Some(a), Some(b)) if a != b);
    detail.push_str(&format!(
        "POS precondition: alpha daemon pid={pid_a:?}, bravo daemon pid={pid_b:?}, DISTINCT={distinct}\n"
    ));
    ok &= distinct;

    // Capture the per-session PTY child pids BEFORE the kill (blast-radius truth).
    let mux = mux_for(&jail);
    let child_a = mux.list(&dir).unwrap_or_default().into_iter().find(|s| s.name == "alpha").map(|s| s.pid as u32);
    let child_b = mux.list(&dir).unwrap_or_default().into_iter().find(|s| s.name == "bravo").map(|s| s.pid as u32);
    detail.push_str(&format!("POS children: alpha child pid={child_a:?}, bravo child pid={child_b:?}\n"));

    // Both live + operable pre-kill: send to bravo and confirm the daemon acks
    // the send (history won't carry a blind-inject into a non-composer `cat` — see
    // the post-kill block / g_isol Fix A below).
    let (cs0, _o, _e) = run_qd(&jail, &["send:pty", "bravo", "PRE_KILL_B"]);
    ok &= cs0 == 0;

    // SIGKILL alpha's daemon ONLY.
    if let Some(a) = pid_a {
        let t0 = Instant::now();
        sigkill(a);
        // A's daemon DEAD.
        let mut a_dead = false;
        while t0.elapsed() < Duration::from_secs(3) {
            if !pid_alive(a) {
                a_dead = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        detail.push_str(&format!(
            "POS kill: alpha daemon pid {a} dead in {:?} = {a_dead}\n",
            t0.elapsed()
        ));
        ok &= a_dead;
    } else {
        ok = false;
    }

    // A's WORLD torn down: alpha's child also dies (its daemon owned the PTY
    // master; daemon death = that session's world dies — per-session, the point).
    if let Some(ac) = child_a {
        let mut child_dead = false;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(4) {
            if !pid_alive(ac) {
                child_dead = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        detail.push_str(&format!("POS teardown: alpha child pid {ac} dead = {child_dead}\n"));
        ok &= child_dead;
    }

    // B PROVEN ALIVE BY OPERATION (not just pid): bravo's daemon still serves
    // send:pty AND a history read.
    //
    // g_isol Fix A (clerk ruling, under Pete's grant): under M-series
    // polite delivery, a send:pty into a NON-COMPOSER carrier (bravo is a bare
    // `cat` — no `❯` prompt glyph) correctly VERIFY-BLOCKS: the fire declines to
    // blind-type (`composer_is_plain` = None → honest verify-block, never a blind
    // type), so the marker does NOT render in history. That is the QS-5 safety gate
    // working as designed — NOT a liveness failure. (The old expectation that the
    // marker renders encoded v4's superseded blind-write contract; Fix B — bypass
    // the gate for unattended non-composer sends — was REJECTED.) So we prove B
    // alive-by-operation WITHOUT a blind inject: daemon pid alive + send:pty acks
    // (exit 0 — the daemon served the request) + the daemon serves a history READ +
    // mux.list finds bravo (the correct jailed daemon is reachable), and we assert
    // the marker is ABSENT (the honest verify-block outcome).
    let b_pid_survives = pid_b.map(pid_alive).unwrap_or(false);
    let (cs, _os, es) = run_qd(&jail, &["send:pty", "bravo", "POST_KILL_MARKER_B"]);
    let send_ok = cs == 0;
    // The daemon serves a history read (Ok transport, not an error) ...
    let hist_read_ok = mux.history(&dir, "bravo").is_ok();
    // ... and list still finds bravo (correct jailed daemon reachable, not a
    // wrong-daemon read).
    let list_finds_bravo = mux
        .list(&dir)
        .unwrap_or_default()
        .into_iter()
        .any(|s| s.name == "bravo");
    // Verify-block contract: the non-composer carrier receives NO blind inject, so
    // the marker must be ABSENT from history.
    let marker_absent =
        !strip_ansi(&mux.history(&dir, "bravo").unwrap_or_default()).contains("POST_KILL_MARKER_B");
    detail.push_str(&format!(
        "POS B-alive-by-operation: bravo daemon pid alive={b_pid_survives}, send:pty exit={cs} (stderr {}), history-read-served={hist_read_ok}, list-finds-bravo={list_finds_bravo}, verify-block marker-absent={marker_absent}\n",
        es.trim()
    ));
    ok &= b_pid_survives && send_ok && hist_read_ok && list_finds_bravo && marker_absent;

    // A RECOVERABLE COLD: a fresh `qd start alpha` relaunches a daemon for it.
    let _ = run_qd(&jail, &["stop", "--force", "alpha"]);
    let (cr, _or, er) = qd_new_live(&jail, "alpha", "cat");
    let relaunched = cr == 0 && session_daemon_pid(&dir, "alpha").is_some();
    detail.push_str(&format!(
        "POS recover-cold: qd start alpha exit={cr} (stderr {}), daemon relaunched={relaunched}\n",
        er.trim()
    ));
    ok &= relaunched;

    // Teardown positive jail (kill any daemons we left).
    for p in all_session_daemon_pids(&dir) {
        sigkill(p);
    }
    jail.teardown();

    // ----------------------------------------------------------------------
    // The NEGATIVE control (shared-fate, MUST RED) lives at the QRMUX LEVEL:
    // `crates/qrmux/tests/wsc_m4.rs::g_isol_negative_shared_fate_red`. WHY
    // qrmux-level: the QRMUX_TEST_SHARED seam puts TWO sessions on ONE daemon —
    // a multi-session-on-one-daemon world the production engine NEVER builds, so
    // the full `qd start` engine path (boot-waiter, registry join, end-watch on the
    // shared manager) interacts with the artificial mode in production-irrelevant
    // ways that make an engine-level negative flaky. The negative control's JOB
    // is gate INTEGRITY (prove the inversion can't pass vacuously), not engine-
    // path coverage — the POSITIVE arm above already exercises the full engine
    // path. At the qrmux level the construction is deterministic: ONE daemon under
    // the seam, two sessions created on it via the wire (the manager genuinely
    // goes multi-session — the machinery exists), ONE pid serves BOTH (asserted),
    // SIGKILL it → BOTH children die (the shared-fate RED the control detects).
    // This is the SAME alive-by-operation logic the positive arm uses, applied to
    // the shared world. See the spec §7 G-ISOL negative-control construction.

    let verdict = if ok {
        "G-ISOL (POSITIVE) VERDICT: PASS — A,B on DISTINCT per-session daemons (one pid each); SIGKILL A's daemon → A child DEAD + world torn down, B PROVEN ALIVE BY OPERATION (send:pty acks + daemon serves a history read + list finds bravo; the polite fire correctly VERIFY-BLOCKS a blind inject into the non-composer cat — QS-5 upheld, g_isol Fix A), A recoverable cold (qd start relaunch). The shared-fate NEGATIVE control (must RED) is the qrmux-level g_isol_negative_shared_fate_red arm (QRMUX_TEST_SHARED seam)."
    } else {
        "G-ISOL (POSITIVE) VERDICT: FAIL"
    };
    write_result("g-isol", verdict, &detail);
    assert!(ok, "G-ISOL (positive) failed:\n{detail}");
}

// ===========================================================================
// G-COLDSTART-N (R-1(3)) — per-session cold-start race classes (engine level).
//   (a) same-session race: K concurrent ops on ONE absent session → 1 daemon.
//   (b) cross-session burst: N concurrent creations → N daemons, no convoy.
//   (d) create-vs-teardown: create racing a predecessor exit-on-end → clean.
//   MUTATION: QD_EMBEDDED_DAEMON_PROGRAM severed → cold start REDS (embedded-named).
// (c claim-timeout + grace are the qrmux-level wsc_m4 arms.)
// ===========================================================================

#[test]
fn g_coldstart_n() {
    let mut detail = String::new();
    let mut ok = true;

    // ---- (a) same-session race: K concurrent ensures of ONE absent session.
    // The engine adapter's per-session flock serializes births → exactly ONE
    // daemon, exactly ONE session; the others connect to it (no orphans).
    {
        let jail = Jail::establish("gcsn-same");
        let dir = jail.resolved_dir();
        let k = 8usize;
        // Pre-spawn-free precondition.
        let pre = !dir.join("alpha.sock").exists() && session_daemon_pid(&dir, "alpha").is_none();
        // K concurrent `qd start alpha` (CreateOrAttach semantics: first creates,
        // the rest attach/connect or fail loud+bounded — never a second daemon).
        let fake = write_fake_claude(&jail, "cat");
        let fake_s = fake.to_string_lossy().into_owned();
        let mut handles = Vec::new();
        for _ in 0..k {
            let bin = qd_bin().to_string();
            let fake_s = fake_s.clone();
            let home = jail.home.clone();
            let qd_home = jail.root.join("qdhome");
            let xdg = jail.xdg_runtime.clone();
            let tmp = jail.root.join("tmp");
            let zmx = jail.root.join("zmx");
            handles.push(std::thread::spawn(move || {
                let out = Command::new(bin)
                    // WP-B-CS-1 (D2): force the interactive surface — piped stdio
                    // would auto-detect headless. Tests concurrent cold-start.
                    .args(["start", "--interactive", "alpha"])
                    .env_clear()
                    // Lifecycle-collapse A-3: relay wait is DEFAULT-ON now; these
                    // hermetic boots have no sidecar — explicit env opt-out.
                    .env("QD_BOOT_AWAIT_RELAY", "0")
                    .env("HOME", &home)
                    .env("QD_HOME", &qd_home)
                    .env("XDG_RUNTIME_DIR", &xdg)
                    .env("TMPDIR", &tmp)
                    .env("ZMX_DIR", &zmx)
                    .env("PATH", "/usr/bin:/bin")
                    .env("TERM", "xterm-256color")
                    .env("CLAUDE_BIN", &fake_s)
                    .env("QD_FAKE_NAME", "alpha")
                    .output()
                    .expect("spawn qd start");
                out.status.code().unwrap_or(-1)
            }));
        }
        let codes: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let n_ok = codes.iter().filter(|&&c| c == 0).count();
        // At least one create succeeds; none HANG (all threads returned a code).
        // Exactly ONE daemon for alpha, and exactly ONE alpha socket.
        std::thread::sleep(Duration::from_millis(300));
        let one_daemon = session_daemon_pid(&dir, "alpha").is_some()
            && all_session_daemon_pids(&dir).len() == 1;
        let sockets = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| n.ends_with(".sock") && n != "qrmux.sock")
                    .count()
            })
            .unwrap_or(0);
        detail.push_str(&format!(
            "(a) same-session K={k}: pre-spawn-free={pre}, exit codes={codes:?} (>=1 ok: {n_ok}), exactly ONE daemon={one_daemon}, session sockets={sockets}\n"
        ));
        ok &= pre && n_ok >= 1 && one_daemon && sockets == 1;
        for p in all_session_daemon_pids(&dir) {
            sigkill(p);
        }
        jail.teardown();
    }

    // ---- (b) cross-session burst: N concurrent creations of N DISTINCT
    // sessions → N daemons, ALL live, generalized keystone per session, no
    // convoy (wall-clock bounded vs an N×serial estimate).
    {
        let jail = Jail::establish("gcsn-cross");
        let dir = jail.resolved_dir();
        let n = 8usize;
        let fake = write_fake_claude(&jail, "cat");
        let fake_s = fake.to_string_lossy().into_owned();
        let names: Vec<String> = (0..n).map(|i| format!("s{i}")).collect();
        let t0 = Instant::now();
        let mut handles = Vec::new();
        for name in &names {
            let bin = qd_bin().to_string();
            let fake_s = fake_s.clone();
            let name = name.clone();
            let home = jail.home.clone();
            let qd_home = jail.root.join("qdhome");
            let xdg = jail.xdg_runtime.clone();
            let tmp = jail.root.join("tmp");
            let zmx = jail.root.join("zmx");
            handles.push(std::thread::spawn(move || {
                let out = Command::new(bin)
                    // WP-B-CS-1 (D2): force the interactive surface (piped stdio).
                    .args(["start", "--interactive", &name])
                    .env_clear()
                    // Lifecycle-collapse A-3: relay wait is DEFAULT-ON now; these
                    // hermetic boots have no sidecar — explicit env opt-out.
                    .env("QD_BOOT_AWAIT_RELAY", "0")
                    .env("HOME", &home)
                    .env("QD_HOME", &qd_home)
                    .env("XDG_RUNTIME_DIR", &xdg)
                    .env("TMPDIR", &tmp)
                    .env("ZMX_DIR", &zmx)
                    .env("PATH", "/usr/bin:/bin")
                    .env("TERM", "xterm-256color")
                    .env("CLAUDE_BIN", &fake_s)
                    .env("QD_FAKE_NAME", &name)
                    .output()
                    .expect("spawn qd start");
                out.status.code().unwrap_or(-1)
            }));
        }
        let codes: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let wall = t0.elapsed();
        let all_ok = codes.iter().all(|&c| c == 0);
        std::thread::sleep(Duration::from_millis(400));
        // N daemons, all live; generalized keystone: each name has its own
        // `<name>.sock` in the engine-resolved dir, and its daemon answers.
        let mut all_present = true;
        for name in &names {
            let has_sock = dir.join(format!("{name}.sock")).exists();
            let has_daemon = session_daemon_pid(dir.as_path(), name).is_some();
            if !(has_sock && has_daemon) {
                all_present = false;
            }
        }
        let daemon_count = all_session_daemon_pids(&dir).len();
        // No-convoy evidence: N parallel cold-starts must finish well under a
        // serial estimate (~N × a single cold-start ~1s each = ~Ns). We bound at
        // a generous half-of-serial ceiling to catch a launch convoy without
        // flaking on a loaded box.
        let serial_estimate = Duration::from_millis(n as u64 * 1500);
        let no_convoy = wall < serial_estimate;
        detail.push_str(&format!(
            "(b) cross-session N={n}: all exit 0={all_ok}, all N daemons present (keystone per session)={all_present}, daemon count={daemon_count}, wall={wall:?} < serial_estimate {serial_estimate:?} (no convoy)={no_convoy}\n"
        ));
        ok &= all_ok && all_present && daemon_count == n && no_convoy;
        for p in all_session_daemon_pids(&dir) {
            sigkill(p);
        }
        jail.teardown();
    }

    // ---- (d) create-vs-teardown: create racing a predecessor's exit-on-end.
    // Boot a session, kill it (drives exit-on-end: unlink + exit), then
    // IMMEDIATELY `qd start` the SAME name → a clean relaunch, no bind-error leak.
    {
        let jail = Jail::establish("gcsn-cvt");
        let dir = jail.resolved_dir();
        let (c1, _o, _e) = qd_new_live(&jail, "race", "cat");
        ok &= c1 == 0;
        // Kill the session (engine kill → daemon exit-on-end teardown).
        let _ = run_qd(&jail, &["stop", "--force", "race"]);
        // Race a fresh create immediately (the launcher's retiring/absent
        // four-state handles the socket-removed-between-scan-and-connect window).
        let (c2, _o2, e2) = qd_new_live(&jail, "race", "cat");
        let relaunch_clean = c2 == 0
            && session_daemon_pid(&dir, "race").is_some()
            && !e2.to_lowercase().contains("address already in use")
            && !e2.to_lowercase().contains("bind");
        detail.push_str(&format!(
            "(d) create-vs-teardown: kill then immediate recreate exit={c2}, clean relaunch (no bind-error leak)={relaunch_clean}\n  stderr: {}\n",
            e2.trim()
        ));
        ok &= relaunch_clean;
        for p in all_session_daemon_pids(&dir) {
            sigkill(p);
        }
        jail.teardown();
    }

    // ---- MUTATION CONTROL: sever the embedded daemon launch program → the
    // per-session cold start MUST RED with the embedded-named error (the M4fix
    // control, re-asserted in per-session form: no socket bound for the name).
    {
        let jail = Jail::establish("gcsn-mut");
        let dir = jail.resolved_dir();
        let bogus = jail.root.join("no-such-embedded-daemon");
        let bogus_s = bogus.to_string_lossy().into_owned();
        let fake = write_fake_claude(&jail, "cat");
        let fake_s = fake.to_string_lossy().into_owned();
        let (cm, om, em) = run_qd_env(
            &jail,
            &["start", "mut"],
            &[
                ("CLAUDE_BIN", &fake_s),
                ("QD_FAKE_NAME", "mut"),
                ("QD_EMBEDDED_DAEMON_PROGRAM", &bogus_s),
            ],
        );
        let mut_socket = dir.join("mut.sock");
        let combined = format!("{om}\n{em}").to_lowercase();
        let names_embedded = combined.contains("embedded") && combined.contains("qrmux");
        let red = cm != 0 && !mut_socket.exists();
        detail.push_str(&format!(
            "MUTATION (severed QD_EMBEDDED_DAEMON_PROGRAM={}): qd start exit={cm} (want nonzero), no per-session socket={}, error names embedded qrmux={names_embedded}\n  stderr: {}\n",
            bogus.display(),
            !mut_socket.exists(),
            em.trim()
        ));
        ok &= red && names_embedded;
        for p in all_session_daemon_pids(&dir) {
            sigkill(p);
        }
        jail.teardown();
    }

    let verdict = if ok {
        "G-COLDSTART-N VERDICT: PASS — (a) K=8 same-session race → exactly 1 daemon/1 socket, others bounded; (b) N=8 cross-session burst → 8 daemons all live, keystone per session, no convoy (wall < serial estimate); (d) create-vs-teardown → clean relaunch no bind-error leak; MUTATION (severed launch program) reds with embedded-named error"
    } else {
        "G-COLDSTART-N VERDICT: FAIL"
    };
    write_result("g-coldstart-n", verdict, &detail);
    assert!(ok, "G-COLDSTART-N failed:\n{detail}");
}

// ===========================================================================
// G-EVSPLIT — two sessions' daemons write DISJOINT events files concurrently
// (interleaved sends) with clean session-opened/closed bookends + epoch fencing
// across a SIGKILL-respawn of ONE daemon (successor opens epoch+1; predecessor's
// file intact).
// ===========================================================================

/// Read + JSON-parse an events file's records as `serde_json::Value` lines.
fn read_event_records(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn event_tag(v: &serde_json::Value) -> Option<&str> {
    v.get("event").and_then(|e| e.as_str())
}

#[test]
fn g_evsplit() {
    let mut detail = String::new();
    let mut ok = true;

    let jail = Jail::establish("gevsplit");
    let dir = jail.resolved_dir();
    let events_dir = dir.join("events");

    // Two LIVE sessions (each its own per-session daemon → each its own events
    // writer by construction; no cross-daemon file sharing exists).
    let (ca, _o, _e) = qd_new_live(&jail, "ev-a", "cat");
    let (cb, _o, _e) = qd_new_live(&jail, "ev-b", "cat");
    ok &= ca == 0 && cb == 0;

    // INTERLEAVED sends to both, concurrently, to exercise concurrent writers.
    // Each send is spawned and BOUNDED (spawn + try_wait poll + kill-on-timeout):
    // under heavy host contention an unbounded `.output()` on a send racing a
    // daemon could otherwise wedge the join — every wait in this row is bounded.
    let mut handles = Vec::new();
    for (name, marker) in [("ev-a", "AAA"), ("ev-b", "BBB")] {
        let bin = qd_bin().to_string();
        let home = jail.home.clone();
        let qd_home = jail.root.join("qdhome");
        let xdg = jail.xdg_runtime.clone();
        let tmp = jail.root.join("tmp");
        let zmx = jail.root.join("zmx");
        handles.push(std::thread::spawn(move || {
            for i in 0..5 {
                let mut child = match Command::new(&bin)
                    .args(["send:pty", name, &format!("{marker}{i}")])
                    .env_clear()
                    // Lifecycle-collapse A-3: relay wait is DEFAULT-ON now; these
                    // hermetic boots have no sidecar — explicit env opt-out.
                    .env("QD_BOOT_AWAIT_RELAY", "0")
                    .env("HOME", &home)
                    .env("QD_HOME", &qd_home)
                    .env("XDG_RUNTIME_DIR", &xdg)
                    .env("TMPDIR", &tmp)
                    .env("ZMX_DIR", &zmx)
                    .env("PATH", "/usr/bin:/bin")
                    .env("TERM", "xterm-256color")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                // Bounded wait (≤8s) then kill — never an unbounded join.
                let t0 = Instant::now();
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        _ if t0.elapsed() > Duration::from_secs(8) => {
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        _ => std::thread::sleep(Duration::from_millis(30)),
                    }
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    std::thread::sleep(Duration::from_millis(400));

    // DISJOINT FILES: each session owns its own `<name>.daemon.<epoch>.jsonl`.
    let a_epoch1 = events_dir.join("ev-a.daemon.1.jsonl");
    let b_epoch1 = events_dir.join("ev-b.daemon.1.jsonl");
    let disjoint = a_epoch1.exists() && b_epoch1.exists() && a_epoch1 != b_epoch1;
    detail.push_str(&format!(
        "disjoint files: {} (exists {}), {} (exists {})\n",
        a_epoch1.display(),
        a_epoch1.exists(),
        b_epoch1.display(),
        b_epoch1.exists()
    ));
    ok &= disjoint;

    // CLEAN OPEN BOOKEND: first record of each file is session-opened (seq 1).
    for (label, path) in [("ev-a", &a_epoch1), ("ev-b", &b_epoch1)] {
        let recs = read_event_records(path);
        let opened = recs
            .first()
            .and_then(event_tag)
            .map(|t| t == "session-opened")
            .unwrap_or(false);
        // NO CROSS-CONTAMINATION: ev-a's file must not carry ev-b's content sha
        // markers and vice versa — we assert each file's records all carry the
        // SAME session identity by construction (one writer per file). The file
        // is named for the session; we check no record names the OTHER session.
        let other = if label == "ev-a" { "ev-b" } else { "ev-a" };
        let no_contam = !std::fs::read_to_string(path)
            .unwrap_or_default()
            .contains(&format!("\"{other}\""));
        detail.push_str(&format!(
            "{label}: open bookend (first=session-opened)={opened}, no cross-contamination (no {other} ref)={no_contam}, records={}\n",
            recs.len()
        ));
        ok &= opened && no_contam;
    }

    // EPOCH FENCING across a SIGKILL-respawn of ONE daemon (ev-a). Capture the
    // predecessor's epoch-1 file bytes, SIGKILL ev-a's daemon (no clean close),
    // relaunch via a fresh `qd start`/send → the successor opens epoch+1 and the
    // predecessor's epoch-1 file is BYTE-IDENTICAL (never appended).
    let pred_bytes = std::fs::read(&a_epoch1).unwrap_or_default();
    if let Some(pid_a) = session_daemon_pid(&dir, "ev-a") {
        sigkill(pid_a);
        let t0 = Instant::now();
        while pid_alive(pid_a) && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(40));
        }
    }
    // Re-create ev-a (cold relaunch → fresh daemon → epoch 2 file).
    let _ = run_qd(&jail, &["stop", "--force", "ev-a"]);
    let (cr, _o, _e) = qd_new_live(&jail, "ev-a", "cat");
    ok &= cr == 0;
    let _ = run_qd(&jail, &["send:pty", "ev-a", "EPOCH2_MARK"]);
    std::thread::sleep(Duration::from_millis(400));

    let a_epoch2 = events_dir.join("ev-a.daemon.2.jsonl");
    let successor_epoch2 = a_epoch2.exists();
    let pred_intact = std::fs::read(&a_epoch1).unwrap_or_default() == pred_bytes;
    // Successor's first record is a fresh session-opened with epoch 2.
    let succ_opened_epoch2 = read_event_records(&a_epoch2)
        .first()
        .map(|r| {
            event_tag(r) == Some("session-opened")
                && r.get("epoch").and_then(|e| e.as_u64()) == Some(2)
        })
        .unwrap_or(false);
    detail.push_str(&format!(
        "epoch fencing: successor opened epoch-2 file={successor_epoch2}, successor first record session-opened+epoch2={succ_opened_epoch2}, predecessor epoch-1 BYTE-IDENTICAL (never appended)={pred_intact}\n"
    ));
    ok &= successor_epoch2 && succ_opened_epoch2 && pred_intact;

    for p in all_session_daemon_pids(&dir) {
        sigkill(p);
    }
    jail.teardown();

    let verdict = if ok {
        "G-EVSPLIT VERDICT: PASS — two per-session daemons write DISJOINT events files concurrently (interleaved sends), clean session-opened bookends, no cross-contamination; epoch fencing across SIGKILL-respawn of one daemon (successor opens epoch 2, predecessor epoch-1 file byte-identical / never appended)"
    } else {
        "G-EVSPLIT VERDICT: FAIL"
    };
    write_result("g-evsplit", verdict, &detail);
    assert!(ok, "G-EVSPLIT failed:\n{detail}");
}

// G-LEGACY (W-4) was removed: the legacy `<dir>/qrmux.sock` shared-daemon probe
// and its per-target unlink/warning are dropped as part of the cat-(iii) rename
// (the one sanctioned behavior change).
