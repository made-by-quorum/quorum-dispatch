//! End-to-end validation of the INTERACTIVE codex lane (`qd start --provider
//! codex --interactive`) against the real `qd` binary, a real mux pane, and a real
//! registry — with no human in the loop.
//!
//! WHY THIS CAN BE A TEST AT ALL. The lane's first design coupled `qd start` to
//! codex disclosing a thread id, and codex does not disclose one until a human
//! types into the TUI — so validating it meant a person at a terminal. The
//! decoupled design removed both obstacles: `qd start` now returns as soon as the
//! pane is attachable (a plain non-interactive command with an exit code and a row
//! to assert on), and identity binds when a rollout FILE APPEARS, which a test can
//! simply create.
//!
//! WHAT STANDS IN FOR CODEX. `QD_CODEX_BIN` points the launch argv at any binary,
//! so the pane runs a real long-lived process under the real mux without needing
//! codex installed, authenticated, or interactive. The rollout is a fixture copied
//! from a REAL one's `session_meta` shape (codex-cli 0.146.1) — including the
//! detail that trips naive implementations: the record's top-level timestamp is
//! LATER than the payload's, because codex stamps it when it finally flushes, not
//! when the session began.
//!
//! WHAT THIS DELIBERATELY DOES NOT PROVE: that a future codex still writes that
//! shape. That is a live-evidence question (see `provider::codex::tui`'s module
//! doc for the measurements); this pins OUR half of the contract.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use common::p0bins::{establish_jail, qd_bin, JailScaffold};

/// Install the stand-in for the codex TUI: a script that RECORDS ITS ARGV and then
/// blocks.
///
/// Recording the argv is what makes the resume assertion real. A stand-in that
/// merely stays alive would let the test pass even if the revive launched a bare
/// `codex` — the pane would be up either way, and "up" is all the row proves. The
/// log turns "did the thread id reach the process?" into a direct observation.
///
/// Blocking regardless of arguments matters too: `/bin/cat resume <uuid>` would
/// treat those as filenames, exit instantly, and leave the liveness assertions
/// racing a dying pane.
fn install_fake_codex(dir: &Path) -> std::path::PathBuf {
    let bin = dir.join("fake-codex");
    std::fs::write(
        &bin,
        "#!/bin/sh\nprintf 'LAUNCH %s\\n' \"$*\" >> \"$(dirname \"$0\")/argv.log\"\nexec sleep 600\n",
    )
    .expect("write fake codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    bin
}

/// Block until the stand-in has recorded `n` launches, then return those lines.
///
/// Polling is REQUIRED, not defensive: `qd start`/`qd resume` return once the pane
/// is registered and attachable, which is strictly earlier than the process inside
/// it getting scheduled. Reading the log immediately races that gap — and the race
/// is silent, because "not written yet" and "launched with no arguments" are the
/// same empty read. Each launch writes exactly one `LAUNCH …` line, so waiting on
/// the COUNT makes the two distinguishable.
fn launches(dir: &Path, n: usize) -> Vec<String> {
    let path = dir.join("argv.log");
    for _ in 0..100 {
        let lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("LAUNCH"))
            .map(|l| l.trim().to_string())
            .collect();
        if lines.len() >= n {
            return lines;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("stand-in never recorded {n} launch(es); log: {:?}", std::fs::read_to_string(&path));
}

/// Drive the real `qd` in the jail, with the codex root pointed inside it too so
/// nothing reads or writes the developer's real `~/.codex`.
fn qd(
    j: &JailScaffold,
    codex_home: &Path,
    cwd: &Path,
    pane_bin: &Path,
    args: &[&str],
) -> (i32, String, String) {
    let out = Command::new(qd_bin())
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("HOME", &j.home)
        .env("QD_HOME", &j.qd_home)
        .env("XDG_RUNTIME_DIR", &j.xdg)
        .env("TMPDIR", j.root.join("tmp"))
        .env("CODEX_HOME", codex_home)
        .env("QD_CODEX_BIN", pane_bin)
        .env("QD_BOOT_AWAIT_RELAY", "0")
        .env("QD_TEST_NO_BARE_PROCS", "1")
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .output()
        .expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The single registry row in the jail, as a field map. Panics if there is not
/// exactly one — every assertion here is about one session.
fn row(j: &JailScaffold) -> BTreeMap<String, serde_json::Value> {
    let dir = j.home.join(".claude").join("sessions");
    let mut rows: Vec<_> = std::fs::read_dir(&dir)
        .expect("sessions dir")
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .collect();
    assert_eq!(rows.len(), 1, "expected exactly one registry row in {dir:?}");
    let bytes = std::fs::read(rows.remove(0).path()).expect("read row");
    serde_json::from_slice(&bytes).expect("row is json")
}

fn field(j: &JailScaffold, k: &str) -> Option<String> {
    row(j).get(k).map(|v| match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// Write a rollout with the REAL `session_meta` shape. `started` is the thread's
/// own start stamp (the payload's); `written` is when codex flushed the record —
/// deliberately later, as observed live.
fn write_rollout(codex_home: &Path, uuid: &str, cwd: &str, started: &str, written: &str) {
    let day = codex_home.join("sessions").join("2026").join("08").join("06");
    std::fs::create_dir_all(&day).expect("rollout dir");
    let body = format!(
        "{{\"timestamp\":\"{written}\",\"type\":\"session_meta\",\"payload\":{{\
         \"session_id\":\"{uuid}\",\"id\":\"{uuid}\",\"timestamp\":\"{started}\",\
         \"cwd\":\"{cwd}\",\"originator\":\"codex-tui\",\"cli_version\":\"0.146.1\"}}}}\n"
    );
    std::fs::write(day.join(format!("rollout-2026-08-06T00-00-00-{uuid}.jsonl")), body)
        .expect("write rollout");
}

fn remove_rollout(codex_home: &Path, uuid: &str) {
    let day = codex_home.join("sessions").join("2026").join("08").join("06");
    let _ = std::fs::remove_file(day.join(format!("rollout-2026-08-06T00-00-00-{uuid}.jsonl")));
}

/// Epoch ms → the ISO-8601 UTC spelling codex uses.
fn iso(ms: i64) -> String {
    dispatch::render::epoch_ms_to_iso(ms)
}

fn pid_alive(pid: i64) -> bool {
    pid > 0 && unsafe { libc::kill(pid as i32, 0) == 0 }
}

const OTHER_CWD: &str = "019f0000-0000-7000-8000-0000000000a1";
const TOO_OLD: &str = "019f0000-0000-7000-8000-0000000000b2";
const OURS: &str = "019f0000-0000-7000-8000-0000000000c3";
const RIVAL: &str = "019f0000-0000-7000-8000-0000000000d4";

/// The whole lane, in the order a user would exercise it.
///
/// Kept as ONE test rather than several: each step depends on the live pane the
/// previous one created, and spawning a real mux session per assertion would be
/// both slow and racy. The step banners keep a failure's location obvious.
#[test]
fn interactive_codex_starts_unidentified_then_binds_its_thread() {
    let j = establish_jail(Path::new("/tmp/qd-cxint"), "cxint");
    let codex_home = j.root.join("codex_home");
    std::fs::create_dir_all(codex_home.join("sessions")).expect("codex home");
    // The session's cwd, CANONICAL: `getcwd` resolves symlinks, so this is the
    // spelling the row will carry (on macOS /tmp is a symlink to /private/tmp).
    let work = std::fs::canonicalize(&j.root).expect("canonical jail root");
    let fake = install_fake_codex(&work);

    // --- 1. start returns without waiting for identity -----------------------
    let (rc, stdout, stderr) = qd(
        &j,
        &codex_home,
        &work,
        &fake,
        &["start", "cxauto", "--provider", "codex", "--interactive"],
    );
    assert_eq!(rc, 0, "start failed: {stdout}{stderr}");
    assert!(
        stdout.contains("attach with"),
        "start should point at attach, said: {stdout}"
    );
    assert_eq!(
        launches(&work, 1)[0],
        "LAUNCH",
        "a FRESH start must launch a bare `codex` — no resume fragment"
    );

    // --- 2. the row exists and is honestly UNIDENTIFIED ----------------------
    assert_eq!(field(&j, "provider").as_deref(), Some("codex"));
    assert_eq!(field(&j, "hosting").as_deref(), Some("mux-pane"));
    assert_eq!(
        field(&j, "sessionId"),
        None,
        "codex discloses no thread until the session is used — the row must say so"
    );
    assert_eq!(
        field(&j, "endpoint"),
        None,
        "a pane-hosted session has no ws endpoint"
    );
    assert_eq!(field(&j, "cwd").as_deref(), Some(work.to_str().unwrap()));
    let pid: i64 = field(&j, "pid").unwrap().parse().expect("pid");
    let started: i64 = field(&j, "startedAt").unwrap().parse().expect("startedAt");
    assert!(pid_alive(pid), "the pane process should be alive");

    // --- 3. it lists, and send refuses for the RIGHT reason ------------------
    let (_, ls, _) = qd(&j, &codex_home, &work, &fake, &["ls", "--json"]);
    assert!(ls.contains("cxauto"), "ls should list it: {ls}");
    let (_, _, send_err) = qd(&j, &codex_home, &work, &fake, &["send", "cxauto", "hi"]);
    assert!(
        send_err.contains("has not been used yet"),
        "send should name the real cause, said: {send_err}"
    );

    // --- 4. non-qualifying rollouts must NOT bind ----------------------------
    let after = iso(started + 5_000);
    let before = iso(started - 600_000);
    let cwd_s = work.to_str().unwrap();

    write_rollout(&codex_home, OTHER_CWD, "/somewhere/else", &after, &after);
    qd(&j, &codex_home, &work, &fake, &["ls"]);
    assert_eq!(field(&j, "sessionId"), None, "a thread in another cwd bound");

    // The user's other codex, open in THIS repo since before we started. Its
    // rollout is actively written, so only its recorded START rules it out.
    write_rollout(&codex_home, TOO_OLD, cwd_s, &before, &after);
    qd(&j, &codex_home, &work, &fake, &["ls"]);
    assert_eq!(
        field(&j, "sessionId"),
        None,
        "a thread that predates the session bound"
    );

    // --- 5. two qualifying threads ⇒ bind NEITHER ----------------------------
    write_rollout(&codex_home, OURS, cwd_s, &after, &after);
    write_rollout(&codex_home, RIVAL, cwd_s, &after, &after);
    qd(&j, &codex_home, &work, &fake, &["ls"]);
    assert_eq!(
        field(&j, "sessionId"),
        None,
        "ambiguity must not resolve to a guess"
    );

    // --- 6. exactly one ⇒ binds, and PERSISTS --------------------------------
    remove_rollout(&codex_home, RIVAL);
    qd(&j, &codex_home, &work, &fake, &["ls"]);
    assert_eq!(
        field(&j, "sessionId").as_deref(),
        Some(OURS),
        "the surviving qualifying thread should be bound"
    );
    let (_, ls2, _) = qd(&j, &codex_home, &work, &fake, &["ls", "--json"]);
    assert!(ls2.contains(OURS), "ls should render the bound id: {ls2}");

    // --- 7. resume reopens the SAME thread (the human round trip) ------------
    //
    // This is what makes use case 1 whole: a human starts a session, works in it,
    // stops it, and comes back later to the same conversation. The revived pane
    // must carry the thread id forward — a new session under the old name would
    // silently lose the history the user came back for.
    qd(&j, &codex_home, &work, &fake, &["stop", "cxauto"]);
    let (rc, out, err) = qd(&j, &codex_home, &work, &fake, &["resume", "cxauto"]);
    assert_eq!(rc, 0, "resume failed: {out}{err}");
    assert_eq!(
        field(&j, "sessionId").as_deref(),
        Some(OURS),
        "the revived row must carry the SAME thread, not a fresh one"
    );
    assert_eq!(field(&j, "hosting").as_deref(), Some("mux-pane"));
    let revived_pid: i64 = field(&j, "pid").unwrap().parse().expect("pid");
    assert!(pid_alive(revived_pid), "the revived pane should be alive");
    assert_ne!(revived_pid, pid, "revive is a NEW process, not the old one");
    // THE assertion this whole stand-in exists for: the thread id actually reached
    // the launched process as `resume <uuid>`. Without it the test would pass on a
    // revive that started a bare `codex` and lost the conversation.
    let second = launches(&work, 2)[1].clone();
    assert_eq!(
        second,
        format!("LAUNCH resume {OURS}"),
        "revive must launch `codex resume <thread-id>`, carrying the conversation forward"
    );
    // Exactly one row survives: the old tombstone was consumed, so the session
    // does not haunt `ls --all` as a second entry.
    let tombs = std::fs::read_dir(j.home.join(".claude").join("sessions"))
        .expect("sessions dir")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tombstoned"))
        .count();
    assert_eq!(tombs, 0, "the prior tombstone should have been consumed");

    // --- 8. a never-used session cannot be resumed, and says why -------------
    //
    // No thread was ever opened, so there is no conversation to reopen. Launching
    // a bare `codex` here would hand back a DIFFERENT session under the same name.
    let (rc2, _, err2) = qd(
        &j,
        &codex_home,
        &work,
        &fake,
        &["start", "cxfresh", "--provider", "codex", "--interactive"],
    );
    assert_eq!(rc2, 0, "second start failed: {err2}");
    qd(&j, &codex_home, &work, &fake, &["stop", "cxfresh"]);
    let (rc3, _, err3) = qd(&j, &codex_home, &work, &fake, &["resume", "cxfresh"]);
    assert_ne!(rc3, 0, "resuming a never-used session must fail");
    assert!(
        err3.contains("never used"),
        "should explain there is no thread to resume, said: {err3}"
    );
    let pid = revived_pid;

    // --- 9. stop reaps the pane (the leak the hosting field prevents) --------
    qd(&j, &codex_home, &work, &fake, &["stop", "cxauto"]);
    let mut gone = false;
    for _ in 0..40 {
        if !pid_alive(pid) {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        gone,
        "stop must reap the pane; pid {pid} survived (the app-server group-kill \
         path would tombstone the row and leave this running)"
    );
}


/// codex-interactive, USE CASE 2: `qd attach` on a LIVE daemon-hosted session
/// opens a human viewer bound to that session's own app server — it does not
/// stop, convert, or redirect.
///
/// What this pins is the ARGV, because that is the whole mechanism: the viewer
/// must be `codex --remote <the row's endpoint> resume <the row's thread id>`.
/// Get the endpoint wrong and the TUI bootstraps its own app server and shows an
/// unrelated blank session; get the thread wrong and it shows the wrong
/// conversation. Both fail silently — they still render a plausible TUI.
///
/// The final terminal handover is NOT asserted (it needs a real TTY, and the test
/// harness has none); the pane and its argv are the observable part, and the
/// handover was verified live.
#[test]
fn attach_on_a_live_daemon_session_opens_a_remote_bound_viewer() {
    let j = establish_jail(Path::new("/tmp/qd-cxview"), "cxview");
    let codex_home = j.root.join("codex_home");
    std::fs::create_dir_all(codex_home.join("sessions")).expect("codex home");
    let work = std::fs::canonicalize(&j.root).expect("canonical jail root");
    let fake = install_fake_codex(&work);

    // A daemon-hosted row as `run_new_daemon` writes one: a thread id from
    // thread/start and a ws endpoint, no `hosting` field (daemon is codex's
    // structural default), keyed by a pid that is genuinely alive.
    const THREAD: &str = "019fd941-6045-70a0-b977-be27f19985bf";
    const ENDPOINT: &str = "ws://127.0.0.1:56167";
    let pid = std::process::id() as i64;
    let row = serde_json::json!({
        "pid": pid,
        "sessionId": THREAD,
        "cwd": work.to_str().unwrap(),
        "startedAt": 1_786_000_000_000i64,
        "updatedAt": 1_786_000_000_000i64,
        "status": "idle",
        "name": "agentsess",
        "provider": "codex",
        "endpoint": ENDPOINT,
    });
    std::fs::write(
        j.home.join(".claude").join("sessions").join(format!("{pid}.json")),
        serde_json::to_vec_pretty(&row).unwrap(),
    )
    .expect("write daemon row");

    // Attach. Its terminal handover fails in a test harness (no TTY), which is
    // fine — the viewer is spawned before that point.
    let (_rc, out, err) = qd(&j, &codex_home, &work, &fake, &["attach", "agentsess"]);
    assert!(
        out.contains("Opened a viewer") || err.contains("Opened a viewer") || out.contains("attaching"),
        "attach should open a viewer, not redirect. stdout={out:?} stderr={err:?}"
    );
    assert!(
        !err.contains("daemon-hosted (no terminal to attach)"),
        "a LIVE codex daemon session must no longer get the blanket redirect: {err}"
    );

    // THE assertion: the viewer is bound to THIS session's app server and THIS
    // session's thread.
    assert_eq!(
        launches(&work, 1)[0],
        format!("LAUNCH --remote {ENDPOINT} resume {THREAD}"),
        "the viewer must attach to the session's own app server and thread"
    );
}
