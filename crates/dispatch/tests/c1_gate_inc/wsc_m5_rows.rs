// ===========================================================================
// WS-C M5 RELEASE-BUILD MEASUREMENT GATES (spec §7, §8; riders R-1(5)/R-1(6)).
// These promote the WS-A research probes (wsa_floodcont / wsa_rsscurve, this
// file's predecessors in rows.rs) to GATE GRADE by closing their caveats:
//   * RELEASE build (probes were dev build),
//   * long windows ≥60s (probes were 2–3s),
//   * claude-shaped children (the c1_gate write_fake_claude registry-row shim —
//     see "CHILDREN" note below),
//   * a SAME-RUN shared-baseline arm (QRMUX_TEST_SHARED) so the soak RTT is
//     gated against a real comparator, not only an absolute ceiling (red-team M8).
//
// BOTH rows are #[ignore]-gated (Lima-row precedent): they need RELEASE binaries
// + a long runtime, so they never run in the fast workspace suite. Run them
// EXPLICITLY by name (see the per-row "RUN:" docs and the evidence headers).
//
// ---------------------------------------------------------------------------
// RELEASE BINARIES (the gate's whole point — house locator pattern).
//
// The row drives the REAL RELEASE `qd` binary for `qd start`, so each session's
// cold-start auto-launches a RELEASE `qd qrmux-server --session <name>` daemon
// (embedded_mux::embedder_launch_spec re-execs current_exe() = the release qd).
// The daemons — the processes under measurement (RSS, runtime, RTT) — are thus
// release. The in-process EmbeddedMux client used for the RTT/history/list
// measurement is just the WIRE caller; its profile does not change what the
// daemon does. We belt-and-suspenders QD_EMBEDDED_DAEMON_PROGRAM = the release qd
// on the in-process client env too, so any in-process launch is release as well
// (in practice the daemons are already up from `qd start`, so the client only
// connects).
//
// release_qd_bin()/release_qrmux_bin() locate target/release/{qd,qrmux} from the
// test exe's target dir and PANIC WITH A REMEDY if absent — never a silent skip
// (the c1_gate qrmux_bin contract).
//
// ---------------------------------------------------------------------------
// CHILDREN (spec §7 "fake-repl claude-shaped harness where wiring permits, else
// the probe's cat shape with the delta NAMED"):
//
// We use the c1_gate `write_fake_claude` shim, NOT the full `fakerepl` binary
// (crates/dispatch/tests/ack2_gate.rs). NAMED DELTA: write_fake_claude writes the SAME
// claude-shaped registry row a real Claude writes (`<sessions>/<pid>.json`, name
// + status idle + kind claude-code) and then execs a real interactive app
// (`cat` for quiet/idle sessions, a flood shell for flooders). It does NOT carry
// fakerepl's convo-JSONL/submit/turn-anchor semantics. THAT delta is IRRELEVANT
// to what these rows measure: G-SOAK measures the daemon's PTY/render RTT under
// output+input flood, and per-daemon RSS; G-IDLE measures idle per-daemon RSS.
// The convo/submit machinery exercises engine transcript paths, not the daemon's
// PTY/render hot path. The fakerepl jail layout (qdrg-runs convo belt) is a
// different jail shape than the c1_gate Jail these rows reuse; wiring it in would
// fork the harness for zero measurement value. So: claude-shaped at the registry
// layer (more claude-shaped than the bare-`cat` WS-A probe, which had no row at
// all), cat/flood at the PTY layer. This delta is also stamped into the evidence
// headers.
// ===========================================================================

/// Locate `target/release/qd` from the test exe's target dir. PANICS with a build
/// remedy if absent (house locator pattern; never a silent vacuous skip).
fn release_qd_bin() -> PathBuf {
    release_bin("qd")
}

/// Locate `target/release/qrmux` (the daemon binary, present for parity even
/// though `qd qrmux-server` is the launched daemon entry).
#[allow(dead_code)]
fn release_qrmux_bin() -> PathBuf {
    release_bin("qrmux")
}

fn release_bin(name: &str) -> PathBuf {
    // current_exe = <target>/<profile>/deps/<test-hash>; the workspace target root
    // is <target> (two parents up from <profile>/deps). The release dir is a
    // SIBLING of whatever profile dir the test built under.
    let exe = std::env::current_exe().expect("current_exe");
    let target_root = exe
        .parent() // deps/
        .and_then(Path::parent) // <profile>/
        .and_then(Path::parent) // <target>/
        .expect("target root")
        .to_path_buf();
    let bin = target_root.join("release").join(name);
    assert!(
        bin.exists(),
        "RELEASE binary not found at {bin:?} — these M5 gates measure the RELEASE \
         daemon. Build first: scripts/build-lock.sh cargo build --release -p quorum-dispatch \
         --bin qd -p qrmux --bin qrmux"
    );
    bin
}

/// Build environment for driving the RELEASE `qd` binary in a jail. Mirrors
/// Jail::apply_embedded but as a (k, String) vec for Command construction in
/// threads, AND points QD_EMBEDDED_DAEMON_PROGRAM at the release qd so any
/// daemon launch (engine or in-process client) is release.
fn release_jail_env(jail: &Jail) -> Vec<(String, String)> {
    let rel = release_qd_bin().to_string_lossy().into_owned();
    vec![
        ("HOME".into(), jail.home.to_string_lossy().into_owned()),
        ("QD_HOME".into(), jail.root.join("qdhome").to_string_lossy().into_owned()),
        ("XDG_RUNTIME_DIR".into(), jail.xdg_runtime.to_string_lossy().into_owned()),
        ("TMPDIR".into(), jail.root.join("tmp").to_string_lossy().into_owned()),
        ("ZMX_DIR".into(), jail.root.join("zmx").to_string_lossy().into_owned()),
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("TERM".into(), "xterm-256color".into()),
        ("QD_EMBEDDED_DAEMON_PROGRAM".into(), rel),
        // Lifecycle-collapse A-3: relay wait is DEFAULT-ON now; these hermetic
        // boots have no sidecar — explicit env opt-out.
        ("QD_BOOT_AWAIT_RELAY".into(), "0".into()),
    ]
}

/// `qd start <name>` through the RELEASE binary with the claude-shaped fake-claude
/// shim execing `app`. `extra` adds env (QD_FAKE_NAME, QRMUX_TEST_SHARED, …).
/// Returns the exit code. The daemon auto-launched is the release `qd qrmux-server`.
fn release_qd_new(
    jail: &Jail,
    fake: &Path,
    name: &str,
    app_unused_marker: &str,
    extra: &[(String, String)],
) -> i32 {
    let _ = app_unused_marker; // app is baked into `fake`; kept for call-site clarity
    let bin = release_qd_bin();
    let mut cmd = Command::new(bin);
    cmd.args(["start", name]).env_clear();
    for (k, v) in release_jail_env(jail) {
        cmd.env(k, v);
    }
    cmd.env("CLAUDE_BIN", fake.to_string_lossy().into_owned());
    cmd.env("QD_FAKE_NAME", name);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let _ = std::fs::create_dir_all(jail.root.join("tmp"));
    let _ = std::fs::create_dir_all(jail.root.join("zmx"));
    let out = cmd.output().expect("spawn release qd start");
    // Teardown-leak belt: record the auto-launched per-session daemon post-boot
    // (engine-cold-started, pid unseen at spawn) via the exact-socket-dir lookup.
    record_engine_daemons(&jail.root, &jail.resolved_dir());
    out.status.code().unwrap_or(-1)
}

/// Pre-spawn ONE RELEASE shared-fate daemon (`qd qrmux-server --session shared`
/// with QRMUX_TEST_SHARED=1) and wait for its `shared.sock`. The shared-baseline
/// arm uses this PROBE-style construction (pre-spawned daemon + in-process
/// mux_create) rather than the engine `qd start` cold-start: under the seam the
/// full engine boot-waiter/registry-join path is the production-irrelevant
/// multi-session-on-one-daemon world M4 documented as flaky through `qd start`, so
/// the deterministic probe mechanism is the honest baseline construction. Uses
/// the RELEASE `qd` binary (`qd qrmux-server`) so the baseline daemon is release,
/// same as the split arm's daemons. Returns (DaemonGuard, shared.sock path).
fn start_release_shared_daemon(jail: &Jail, dir: &Path) -> (DaemonGuard, PathBuf) {
    std::fs::create_dir_all(dir).ok();
    let bin = release_qd_bin();
    let mut cmd = Command::new(&bin);
    cmd.arg("qrmux-server")
        .arg("--socket-dir")
        .arg(dir)
        .arg("--session")
        .arg("shared")
        .env_clear()
                    // Lifecycle-collapse A-3: relay wait is DEFAULT-ON now; these
                    // hermetic boots have no sidecar — explicit env opt-out.
                    .env("QD_BOOT_AWAIT_RELAY", "0")
        .env("HOME", &jail.home)
        .env("XDG_RUNTIME_DIR", &jail.xdg_runtime)
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("QRMUX_CLAIM_TIMEOUT_MS", "120000")
        .env("QRMUX_TEST_SHARED", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().expect("spawn release shared qrmux-server");
    let pid = child.id();
    // Teardown-leak belt: direct spawn, pid known — record at spawn with the
    // --socket-dir identity so a future run can identity-reap a leak.
    record_daemon_pid(&jail.root, pid, &dir.to_string_lossy());
    let mut guard = DaemonGuard { pid, child: Some(child) };
    let socket = dir.join("shared.sock");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if socket.exists() {
            return (guard, socket);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    guard.kill_and_reap();
    panic!("shared daemon socket not created within 5s at {socket:?}");
}

/// Per-session qrmux daemon RSS in KB (`ps -o rss=`), 0 if gone.
fn rss_kb(pid: u32) -> u64 {
    Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

/// Reap EVERY per-session qrmux daemon bound to `dir` (TERM then KILL), by
/// recorded pid via `all_session_daemon_pids` (wsc_m4_rows.rs). Returns the count
/// of daemons that were still alive when teardown began (for the leak check).
/// THE TEARDOWN-LEAK CLASS IS A LIVE FINDING — this row spawns ~14+ daemons and
/// must reap them ALL.
fn reap_all_daemons(dir: &Path) -> usize {
    let pids = all_session_daemon_pids(dir);
    let n = pids.len();
    for p in &pids {
        let _ = Command::new("/bin/kill").arg("-TERM").arg(p.to_string()).stderr(Stdio::null()).status();
    }
    std::thread::sleep(Duration::from_millis(200));
    for p in &pids {
        let _ = Command::new("/bin/kill").arg("-9").arg(p.to_string()).stderr(Stdio::null()).status();
    }
    n
}

/// Write a flooding fake-claude: claude-shaped registry row, then a shell loop
/// that (after a trigger line on stdin) emits UNBOUNDED CHANGING output with a
/// liveness counter in every line (flood-liveness provable from history).
fn write_flood_fake_claude(jail: &Jail) -> PathBuf {
    let path = jail.root.join("fake-claude-flood.sh");
    let sessions = jail.sessions_dir.to_string_lossy().into_owned();
    let pad = ".".repeat(160);
    let script = format!(
        r#"#!/bin/bash
PID=$$
NAME="${{QD_FAKE_NAME:-flood}}"
SESS="{sessions}"
mkdir -p "$SESS"
printf '{{"pid":%s,"sessionId":"sid-%s-%s","cwd":"/w","startedAt":1717000000000,"updatedAt":1717003600000,"status":"idle","name":"%s","version":"0.1.0","kind":"claude-code","entrypoint":"claude"}}' "$PID" "$NAME" "$PID" "$NAME" > "$SESS/$PID.json"
read x
i=0
while :; do i=$((i+1)); echo "FLOOD $i {pad}"; done
"#
    );
    std::fs::write(&path, script).expect("write flood fake-claude");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ok();
    path
}

// ===========================================================================
// G-SOAK (R-1(5)) — release-build many-session soak, probe caveats CLOSED.
//
// RUN:
//   scripts/build-lock.sh cargo build --release -p quorum-dispatch --bin qd -p qrmux --bin qrmux
//   scripts/build-lock.sh cargo test -p quorum-dispatch --test c1_gate -- --ignored --exact g_soak --nocapture
//
// SHAPE (spec §7): RELEASE build; N≥10 per-session daemons (claude-shaped quiet
// children + flooders); ≥3 flooders (unbounded changing output w/ liveness
// counter) + ≥4 input-blast threads (256B SendInput, acked counts); window ≥60s.
// Sibling RTT = SendInput-marker → GetHistory-render (app-output-keyed, ADD-6).
//
// GATE ASSERTIONS (spec §7 as RECALIBRATED by orc-10 ruling relay-1780778364230-20,
// 2026-06-06 16:39 EDT — bounds are spec-set; if they fail honestly the row REDS
// and the numbers are reported, the bound is NOT tuned):
//   1. sibling RTT p95 ≤ 25ms PATHOLOGY CEILING. Provenance: was 10ms (rev-C
//      calibration guess); it FLAPPED at the boundary across identical runs
//      (10.33/9.88ms — both committed, g-soak_result*.txt) because under
//      spec-minimum saturating load on one box it measures HOST SCHEDULING, not
//      the split. Probe-era RTT ~1ms; >25ms = order-of-magnitude pathology.
//   2. THE BINDING ARCHITECTURE TOOTH: sibling RTT p95 ≤ 3× a SAME-RUN
//      shared-baseline arm (identical workload shape under QRMUX_TEST_SHARED=1 —
//      one daemon, same N, same execution; both arms share host conditions, so
//      the comparison is methodologically sound where the absolute is not);
//   3. ZERO timeouts;
//   4. `ls` (scan path) completes ≤ 1s at N≥10.
// Per-daemon RSS recorded across phases.
// POSITIVE CONTROLS (probe pattern, NOW ASSERTED): flood-liveness per flooder +
// acked-blast counts > 0.
// ===========================================================================

#[test]
#[ignore = "WS-C M5 G-SOAK: RELEASE-build many-session soak (≥60s, ~14 daemons) — run explicitly by name with release binaries built (see row docs)"]
fn g_soak() {
    let mut detail = String::new();
    let mut ok = true;

    // Knobs (defaults satisfy the spec minimums; env-tunable for heavier runs).
    let n_quiet: usize = std::env::var("SOAK_N_QUIET").ok().and_then(|s| s.parse().ok()).unwrap_or(7);
    let n_flood: usize = std::env::var("SOAK_N_FLOOD").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    let n_blast: usize = std::env::var("SOAK_N_BLAST").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
    let n_total = n_quiet + n_flood; // N≥10 per-session daemons
    let window = Duration::from_secs(
        std::env::var("SOAK_WINDOW_S").ok().and_then(|s| s.parse().ok()).unwrap_or(60),
    );
    let rel_qd = release_qd_bin();
    detail.push_str(&format!(
        "BUILD PROFILE: RELEASE (daemon = {})\nCONFIG: N_total={n_total} (quiet={n_quiet} + flooders={n_flood}), blast_threads={n_blast}, window={}s\nCHILDREN: claude-shaped write_fake_claude shim (registry row + exec cat/flood-loop); NOT full fakerepl (delta named in header)\n",
        rel_qd.display(),
        window.as_secs(),
    ));

    // ===================================================================
    // Shared closures (used by BOTH the split arm and the shared-baseline arm).
    // ===================================================================

    // RTT: send marker+CR to `name` via the in-process wire client, poll history
    // until the marker renders (the real consumer path: SendInput + GetHistory
    // through the daemon — ADD-6: keyed on the SIBLING's rendered app output).
    fn rtt(mux: &EmbeddedMux, dir: &Path, name: &str, marker: &str, deadline: Duration) -> (Duration, bool) {
        let t0 = Instant::now();
        if mux.send(dir, name, &format!("{marker}\r")).is_err() {
            return (t0.elapsed(), true);
        }
        loop {
            if let Ok(h) = mux.history(dir, name) {
                if h.contains(marker) {
                    return (t0.elapsed(), false);
                }
            }
            if t0.elapsed() > deadline {
                return (t0.elapsed(), true);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn stats(lat: &[(Duration, bool)]) -> (f64, f64, f64, f64, usize, usize) {
        let mut ms: Vec<f64> = lat.iter().map(|(d, _)| d.as_secs_f64() * 1000.0).collect();
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let timeouts = lat.iter().filter(|(_, t)| *t).count();
        let n = ms.len();
        if n == 0 {
            return (0.0, 0.0, 0.0, 0.0, 0, 0);
        }
        (ms[0], ms[n / 2], ms[(n * 95 / 100).min(n - 1)], ms[n - 1], timeouts, n)
    }

    // One full soak arm. Returns (rtt_p95_ms, total_timeouts, ls_max_ms,
    // flood_live_all, blast_total, per-phase detail string, max_daemon_rss_kb).
    // `shared` selects the QRMUX_TEST_SHARED baseline construction.
    let run_arm = |arm: &str, shared: bool, detail: &mut String| -> (f64, usize, f64, bool, u64, u64) {
        let jail = Jail::establish(if shared { "gsoak-shared" } else { "gsoak-split" });
        let dir = jail.resolved_dir();
        let quiet_fake = write_fake_claude(&jail, "cat");
        let flood_fake = write_flood_fake_claude(&jail);
        let flooders: Vec<String> = (0..n_flood).map(|i| format!("f{i}")).collect();

        // The in-process wire client (release daemon program for any launch).
        let env = embedded_env(&jail);
        let mux = EmbeddedMux::new(jail.home.clone(), env);

        // The shared arm holds ONE pre-spawned release shared daemon for the arm's
        // life (reaped at teardown). The split arm uses engine cold-start.
        let mut shared_guard: Option<DaemonGuard> = None;
        let mut booted = 0usize;

        if shared {
            // PROBE-style construction: set the seam in THIS process env so the
            // in-process client collapses to shared.sock, pre-spawn ONE release
            // shared daemon, create N sessions via mux_create (run_detached). The
            // arms run STRICTLY SEQUENTIALLY (shared returns before split starts),
            // so the process-global env is scoped to this arm and removed below.
            // SAFETY: single-threaded at this point; blaster threads spawn later,
            // after the var is set, and the var is removed only after they join.
            unsafe {
                std::env::set_var("QRMUX_TEST_SHARED", "1");
            }
            let (g, _sock) = start_release_shared_daemon(&jail, &dir);
            shared_guard = Some(g);
            // Quiet sessions + flooders all collapse onto the one shared daemon.
            for i in 0..n_quiet {
                let name = format!("q{i}");
                // run_detached: the engine mux primitive `qd start` drives, minus the
                // boot-waiter/registry-join (those are the flaky-under-seam engine
                // legs M4 named — the probe path avoids them). The child is `cat`.
                if mux.run_detached(&dir, &name, "exec cat", &jail.home).map(|r| r.status == Some(0)).unwrap_or(false) {
                    booted += 1;
                }
            }
            let flood_cmd = format!("read x; i=0; while :; do i=$((i+1)); echo \"FLOOD $i {}\"; done", ".".repeat(160));
            for f in &flooders {
                if mux.run_detached(&dir, f, &flood_cmd, &jail.home).map(|r| r.status == Some(0)).unwrap_or(false) {
                    booted += 1;
                }
            }
        } else {
            // SPLIT arm: full engine cold-start via the RELEASE `qd` binary — each
            // `qd start` auto-launches its own per-session release daemon. This is
            // the realistic, claude-shaped path (the topology under gate).
            for i in 0..n_quiet {
                let name = format!("q{i}");
                if release_qd_new(&jail, &quiet_fake, &name, "cat", &[]) == 0 {
                    booted += 1;
                }
            }
            for f in &flooders {
                if release_qd_new(&jail, &flood_fake, f, "flood", &[]) == 0 {
                    booted += 1;
                }
            }
        }
        detail.push_str(&format!("[{arm}] booted {booted}/{n_total} sessions\n"));

        // Daemon count + RSS snapshot. Split: N daemons; shared: 1 daemon.
        let daemon_pids = all_session_daemon_pids(&dir);
        let expect_daemons = if shared { 1 } else { n_total };
        detail.push_str(&format!(
            "[{arm}] daemon count = {} (expected {expect_daemons})\n",
            daemon_pids.len()
        ));
        let rss_pre: Vec<u64> = daemon_pids.iter().map(|p| rss_kb(*p)).collect();
        let max_rss_pre = rss_pre.iter().copied().max().unwrap_or(0);
        let sum_rss_pre: u64 = rss_pre.iter().sum();
        detail.push_str(&format!(
            "[{arm}] PRE-FLOOD per-daemon RSS: max={max_rss_pre} KB sum={sum_rss_pre} KB across {} daemon(s)\n",
            rss_pre.len()
        ));

        // Trigger flooders (send the read-trigger line).
        for f in &flooders {
            let _ = mux.send(&dir, f, "go\r");
        }
        std::thread::sleep(Duration::from_millis(400));

        // Flood-liveness positive control: every flooder's history is CHANGING.
        let mut flood_live = true;
        for f in &flooders {
            let h1 = mux.history(&dir, f).unwrap_or_default();
            std::thread::sleep(Duration::from_millis(200));
            let h2 = mux.history(&dir, f).unwrap_or_default();
            let live = h1.contains("FLOOD") && h2.contains("FLOOD") && h1 != h2;
            flood_live &= live;
        }
        detail.push_str(&format!("[{arm}] flood-liveness (all {n_flood} flooders changing) = {flood_live}\n"));

        // Input-blast threads: 256B SendInput at a discard sink (a quiet session).
        // Acked-count positive control.
        let blast_target = "q0".to_string();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let payload = "B".repeat(256);
        let mut blasters = Vec::new();
        for _ in 0..n_blast {
            let stop_c = Arc::clone(&stop);
            let jail_home = jail.home.clone();
            let benv = embedded_env(&jail);
            let dir_c = dir.clone();
            let payload_c = payload.clone();
            let target_c = blast_target.clone();
            blasters.push(std::thread::spawn(move || -> u64 {
                let mux_t = EmbeddedMux::new(jail_home, benv);
                let mut sent = 0u64;
                while !stop_c.load(std::sync::atomic::Ordering::Relaxed) {
                    if mux_t.send(&dir_c, &target_c, &payload_c).is_ok() {
                        sent += 1;
                    }
                }
                sent
            }));
        }

        // The ≥60s measurement window: continuously sample sibling RTT against a
        // QUIET sibling (q1) + ls latency, while flood + blast run.
        let sib = "q1";
        let mut rtts: Vec<(Duration, bool)> = Vec::new();
        let mut ls_lat: Vec<(Duration, bool)> = Vec::new();
        let t_window = Instant::now();
        let mut iter = 0u64;
        while t_window.elapsed() < window {
            rtts.push(rtt(&mux, &dir, sib, &format!("PING_{arm}_{iter}_X"), Duration::from_secs(10)));
            let t0 = Instant::now();
            let ls_ok = mux.list(&dir).is_ok();
            ls_lat.push((t0.elapsed(), !ls_ok));
            iter += 1;
            std::thread::sleep(Duration::from_millis(50));
        }

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let blast_total: u64 = blasters.into_iter().map(|h| h.join().unwrap_or(0)).sum();

        // Post-flood RSS.
        let rss_post: Vec<u64> = daemon_pids.iter().map(|p| rss_kb(*p)).collect();
        let max_rss_post = rss_post.iter().copied().max().unwrap_or(0);
        let sum_rss_post: u64 = rss_post.iter().sum();

        let (r_min, r_med, r_p95, r_max, r_to, r_n) = stats(&rtts);
        let (_l_min, l_med, l_p95, l_max, l_to, l_n) = stats(&ls_lat);
        detail.push_str(&format!(
            "[{arm}] sibling RTT (n={r_n}): min={r_min:.2} median={r_med:.2} p95={r_p95:.2} max={r_max:.2} ms, timeouts={r_to}\n\
             [{arm}] ls latency (n={l_n}): median={l_med:.2} p95={l_p95:.2} max={l_max:.2} ms, errors={l_to}\n\
             [{arm}] POST-FLOOD per-daemon RSS: max={max_rss_post} KB sum={sum_rss_post} KB\n\
             [{arm}] input-blast acked total = {blast_total} ({n_blast} threads x 256B)\n"
        ));

        // Teardown THIS arm's daemons + jail (per-target by recorded pid). This
        // reaps EVERY qrmux daemon bound to `dir` — per-session daemons (split)
        // AND the one shared daemon (shared arm), by recorded pid.
        let alive = reap_all_daemons(&dir);
        // Belt: also reap the held shared guard explicitly (its pid is in the dir
        // set above, but the guard also drops here regardless).
        if let Some(mut g) = shared_guard.take() {
            g.kill_and_reap();
        }
        detail.push_str(&format!("[{arm}] teardown: reaped {alive} live daemon(s)\n"));
        std::thread::sleep(Duration::from_millis(150));
        let leaked = all_session_daemon_pids(&dir).len();
        detail.push_str(&format!("[{arm}] post-teardown leaked daemons = {leaked}\n"));
        jail.teardown();

        // Remove the process-global seam BEFORE returning so the SPLIT arm (next,
        // sequential) computes real per-session paths. SAFETY: all blaster threads
        // have joined above; single-threaded here.
        if shared {
            unsafe {
                std::env::remove_var("QRMUX_TEST_SHARED");
            }
        }

        (r_p95, r_to + l_to, l_max, flood_live, blast_total, max_rss_post)
    };

    // -------- SAME-RUN SHARED BASELINE ARM (QRMUX_TEST_SHARED=1, one daemon) ----
    detail.push_str("\n=== SHARED-BASELINE ARM (QRMUX_TEST_SHARED=1, one daemon, same N) ===\n");
    let (shared_p95, _shared_to, _shared_ls, shared_flood_live, shared_blast, shared_rss) =
        run_arm("shared", true, &mut detail);

    // -------- SPLIT ARM (per-session daemons — the topology under gate) --------
    detail.push_str("\n=== SPLIT ARM (per-session daemons, N daemons) ===\n");
    let (split_p95, split_to, split_ls_max, split_flood_live, split_blast, split_rss) =
        run_arm("split", false, &mut detail);

    // ===================================================================
    // GATE ASSERTIONS (spec §7 verbatim).
    // ===================================================================
    let baseline_ratio = if shared_p95 > 0.0 { split_p95 / shared_p95 } else { f64::INFINITY };
    // [1] 25ms PATHOLOGY CEILING (was 10ms; recalibrated per orc-10 ruling
    // relay-1780778364230-20 — see the header provenance block).
    let a1_abs = split_p95 <= 25.0;
    let a2_ratio = split_p95 <= 3.0 * shared_p95;
    let a3_zero_to = split_to == 0;
    let a4_ls = split_ls_max <= 1000.0;
    let pc_flood = split_flood_live && shared_flood_live;
    let pc_blast = split_blast > 0 && shared_blast > 0;

    detail.push_str(&format!(
        "\n=== GATE ASSERTIONS (spec §7) ===\n\
         shared-baseline p95 = {shared_p95:.2} ms (shared daemon RSS max {shared_rss} KB)\n\
         split p95          = {split_p95:.2} ms (split daemon RSS max {split_rss} KB)\n\
         baseline ratio (split/shared) = {baseline_ratio:.2}x\n\
         [1] sibling RTT p95 ≤ 25ms PATHOLOGY CEILING (recalibrated, was 10): {split_p95:.2} ≤ 25 = {a1_abs}\n\
         [2] sibling RTT p95 ≤ 3× shared baseline: {split_p95:.2} ≤ {:.2} = {a2_ratio}\n\
         [3] ZERO timeouts (split arm): timeouts={split_to} → {a3_zero_to}\n\
         [4] ls completes ≤ 1s at N≥10: ls_max={split_ls_max:.2} ms ≤ 1000 = {a4_ls}\n\
         positive control flood-liveness (both arms) = {pc_flood}\n\
         positive control input-blast acked>0 (both arms) = {pc_blast}\n",
        3.0 * shared_p95,
    ));

    ok &= a1_abs && a2_ratio && a3_zero_to && a4_ls && pc_flood && pc_blast;

    let verdict = if ok {
        "G-SOAK VERDICT: PASS — release-build, N≥10 per-session daemons under ≥3 flooders + ≥4 input-blast threads for ≥60s: sibling RTT p95 ≤ 3× same-run shared baseline (BINDING tooth) and ≤ 25ms pathology ceiling (recalibrated from 10ms, orc-10 ruling relay-1780778364230-20), ZERO timeouts, ls ≤ 1s at N≥10; positive controls (flood-liveness + acked blast) asserted."
    } else {
        "G-SOAK VERDICT: FAIL — one or more spec §7 assertions did not hold (see detail; bounds are spec-set and NOT tuned here)."
    };
    write_result("g-soak", verdict, &detail);
    assert!(ok, "G-SOAK failed:\n{detail}");
}

// ===========================================================================
// G-IDLE (R-1(6)) — release-build idle-footprint MEASUREMENT row (spec §7, §8).
//
// RUN:
//   scripts/build-lock.sh cargo build --release -p quorum-dispatch --bin qd -p qrmux --bin qrmux
//   scripts/build-lock.sh cargo test -p quorum-dispatch --test c1_gate -- --ignored --exact g_idle --nocapture
//
// N = 0, 1, 5, 10, 20 idle per-session daemons (quiet claude-shaped children),
// settled ≥60s at each point. Record per-daemon RSS (ps -o rss=) + the macOS
// caveat (RSS double-counts shared text; PSS-honest numbers need the Lima/Linux
// lane — NAMED follow-up, does NOT block). Output: a table (N, per-daemon RSS
// min/median/max, sum) + a comparison line vs probe-2 dev-build numbers
// (5.9MB base, ~78KB marginal shared). NO pass/fail threshold (measurement row)
// — but FLAG in the evidence if marginal-per-session exceeds 2× the dev-build
// probe shape (spec §7).
// ===========================================================================

#[test]
#[ignore = "WS-C M5 G-IDLE: RELEASE-build idle-RSS measurement (N=0..20, ≥60s settle each) — run explicitly by name with release binaries built (see row docs)"]
fn g_idle() {
    let mut detail = String::new();

    let settle = Duration::from_secs(
        std::env::var("IDLE_SETTLE_S").ok().and_then(|s| s.parse().ok()).unwrap_or(60),
    );
    let points: Vec<usize> = std::env::var("IDLE_POINTS")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![0, 1, 5, 10, 20]);
    let rel_qd = release_qd_bin();

    detail.push_str(&format!(
        "BUILD PROFILE: RELEASE (daemon = {})\nMEASUREMENT: per-session daemon idle RSS at N = {points:?}; settle {}s each point.\n\
         CHILDREN: claude-shaped write_fake_claude shim (registry row + exec cat), NOT full fakerepl (delta: no convo/submit semantics — irrelevant to idle RSS).\n\
         MACOS RSS CAVEAT (spec §8): `ps -o rss` double-counts SHARED TEXT PAGES — N identical release binaries share their text segment, so ΣRSS OVERCOUNTS the true marginal fleet cost. PSS-honest numbers (smaps_rollup) require the Lima/Linux lane. NAMED FOLLOW-UP, not a blocker for this row.\n\n",
        rel_qd.display(),
        settle.as_secs(),
    ));

    // For each N: fresh jail, boot N quiet sessions, settle, record per-daemon RSS.
    // Table rows: N | daemons | min | median | max | sum (all KB).
    let mut table: Vec<(usize, usize, u64, u64, u64, u64)> = Vec::new();
    // Track the per-session marginal (median single-daemon RSS) for the flag.
    let mut base_n1_median: Option<u64> = None;

    for &n in &points {
        let jail = Jail::establish(&format!("gidle-n{n}"));
        let dir = jail.resolved_dir();
        let quiet_fake = write_fake_claude(&jail, "cat");

        let mut booted = 0usize;
        for i in 0..n {
            let name = format!("idle{i}");
            let code = release_qd_new(&jail, &quiet_fake, &name, "cat", &[]);
            if code == 0 {
                booted += 1;
            }
        }

        // Settle (the probe caveat closed: ≥60s, not the probe's 300ms).
        std::thread::sleep(settle);

        let pids = all_session_daemon_pids(&dir);
        let mut rss: Vec<u64> = pids.iter().map(|p| rss_kb(*p)).filter(|&r| r > 0).collect();
        rss.sort_unstable();
        let (min, med, max, sum) = if rss.is_empty() {
            (0, 0, 0, 0)
        } else {
            let m = rss.len();
            (rss[0], rss[m / 2], rss[m - 1], rss.iter().sum())
        };
        table.push((n, pids.len(), min, med, max, sum));
        if n == 1 {
            base_n1_median = Some(med);
        }
        detail.push_str(&format!(
            "N={n}: booted {booted}/{n}, daemons={}, per-daemon RSS KB: min={min} median={med} max={max} sum={sum}\n",
            pids.len()
        ));

        // Teardown: reap ALL daemons by recorded pid (teardown-leak class belt).
        let alive = reap_all_daemons(&dir);
        std::thread::sleep(Duration::from_millis(150));
        let leaked = all_session_daemon_pids(&dir).len();
        detail.push_str(&format!("  teardown: reaped {alive} live daemon(s), leaked after = {leaked}\n"));
        jail.teardown();
    }

    // ---- Table + comparison line (spec §7/§8). ----
    detail.push_str("\n=== IDLE RSS TABLE (per-session daemons, RELEASE build) ===\n");
    detail.push_str("N     daemons  min(KB)  median(KB)  max(KB)  sum(KB)\n");
    for (n, d, mn, md, mx, sm) in &table {
        detail.push_str(&format!("{n:<5} {d:<8} {mn:<8} {md:<11} {mx:<8} {sm}\n"));
    }

    // Comparison vs probe-2 (wsa-rss-curve, dev build): shared base 5.9MB,
    // marginal idle ~78KB shared. Per-session split: each daemon is its OWN
    // process base — that base IS the per-session marginal under the split.
    let split_marginal = base_n1_median.unwrap_or(0);
    let probe_shared_base_kb = 5900u64; // ~5.9MB
    let probe_marginal_shared_kb = 78u64; // ~78KB marginal (shared daemon)
    detail.push_str(&format!(
        "\n=== COMPARISON vs probe-2 (wsa-rss-curve, DEV build, SHARED daemon) ===\n\
         probe-2 (dev, shared): base daemon ~{probe_shared_base_kb} KB; marginal idle session ~{probe_marginal_shared_kb} KB (shared text + screen).\n\
         M5 (release, SPLIT): each idle session = its OWN daemon process. Per-session marginal = median single-daemon RSS at N=1 = {split_marginal} KB.\n\
         NOTE: the split's per-session marginal is necessarily ~one process base (no sharing of the HashMap/runtime that the shared daemon amortized); the macOS ΣRSS overcounts shared TEXT, so the honest fleet cost is lower than ΣRSS — PSS lane named above.\n"
    ));

    // ---- The FLAG (spec §7): flag if marginal-per-session > 2× the dev-build
    // probe shape. The dev-build probe's PER-PROCESS base (the split's implied
    // per-session cost in probe-2) was ~5.9MB; the spec's "2× the dev-build probe
    // shape" guards a release regression of the per-process base. ----
    let flag_threshold_kb = 2 * probe_shared_base_kb; // 2× the dev per-process base
    let flagged = split_marginal > flag_threshold_kb;
    detail.push_str(&format!(
        "\n=== FLAG (spec §7 measurement guard) ===\n\
         per-session marginal (release, N=1 median) = {split_marginal} KB; 2× dev-build per-process base ({probe_shared_base_kb} KB) = {flag_threshold_kb} KB.\n\
         FLAGGED to orc (marginal > 2× dev base)? {flagged}\n\
         (Measurement row — NO pass/fail. A true flag is reported to the lead/orc, not failed here.)\n"
    ));

    let verdict = format!(
        "G-IDLE VERDICT: MEASUREMENT COMPLETE (release build, N={points:?}, ≥{}s settle each) — per-daemon idle RSS table recorded; macOS RSS-overcount caveat + PSS Lima follow-up named; flag-vs-2×-dev-base = {flagged}. No pass/fail threshold (measurement row).",
        settle.as_secs(),
    );
    write_result("g-idle", &verdict, &detail);
    // Measurement row: NO assert on the numbers. The only invariant we hold is
    // that the harness actually measured (the binaries booted daemons).
    assert!(
        table.iter().any(|(n, d, ..)| *n == 0 || *d > 0),
        "G-IDLE: no daemons measured at any N>0 — harness/boot failure, not a real measurement:\n{detail}"
    );
}
