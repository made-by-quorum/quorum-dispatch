//! End-to-end validation of the INTERACTIVE pi lane (`qd start --provider pi
//! --interactive`) against the real `qd` binary, a real mux pane, and a real
//! registry — with no human in the loop.
//!
//! WHAT THIS PROVES THAT THE CODEX TWIN CANNOT. The codex lane's whole difficulty
//! is that identity arrives LATE and has to be attributed
//! (`codex_interactive_lane.rs` spends most of its length on which rollouts must
//! NOT bind). pi's `--session-id` lets the launcher NAME the session, so the
//! interesting assertions here are the opposite ones: the row is identified from
//! its first instant, the SAME id reaches the launched process on both a fresh
//! start and a revive, and the transcript pi eventually writes is found under the
//! id we chose.
//!
//! WHAT STANDS IN FOR PI. `QD_PI_BIN` points the launch argv at any binary, so the
//! pane runs a real long-lived process under the real mux without needing pi
//! installed, authenticated, or interactive. The stand-in records its argv, which
//! is what turns "did our id reach the process?" into a direct observation rather
//! than an inference from a row we wrote ourselves.
//!
//! WHAT THIS DELIBERATELY DOES NOT PROVE: that a future pi still honors
//! `--session-id` with these semantics, or still persists on the schedule
//! `provider::pi::tui` records. Those are live-evidence questions against a pinned
//! binary (see that module's doc for where each fact was read); this pins OUR half
//! of the contract.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use common::p0bins::{establish_jail, qd_bin, JailScaffold};

/// Install the stand-in for the pi TUI: a script that ANSWERS `--help`, RECORDS
/// ITS ARGV, and otherwise blocks.
///
/// Recording the argv is the point. A stand-in that merely stayed alive would let
/// every assertion below pass even if the launch dropped `--session-id` entirely
/// — the pane would be up either way, and "up" is all the row proves on its own.
///
/// Blocking regardless of arguments matters too: `/bin/cat --session-id <uuid>`
/// would treat those as filenames, exit instantly, and leave the liveness
/// assertions racing a dying pane.
///
/// ANSWERING `--help` is not scaffolding, it is part of the contract now. The
/// create path probes the binary for `--session-id` support before it claims a
/// name (pi 0.74.2 has no such flag and dies inside the pane, which is a miserable
/// failure to diagnose), so a stand-in that could not answer would be refused —
/// and, since the old one `exec sleep`ed on every argv, it would hang the probe
/// rather than fail it. That is exactly the wedge the probe's timeout now bounds;
/// this makes the stand-in behave like the thing it stands in for instead.
///
/// The file is named `pi`, and MUST be: qrmux classifies the harness by the
/// launched binary's basename, so a stand-in called `fake-pi` silently rides
/// claude's composer facts instead of `PiFacts`.
fn install_fake_pi(dir: &Path) -> std::path::PathBuf {
    let bin_dir = dir.join("pi-bin");
    std::fs::create_dir_all(&bin_dir).expect("stand-in dir");
    let bin = bin_dir.join("pi");
    std::fs::write(
        &bin,
        "#!/bin/sh\n         case \"$*\" in *--help*) echo '  --session-id <id>  Use exact project session ID'; exit 0;; esac\n         printf 'LAUNCH %s\\n' \"$*\" >> \"$(dirname \"$0\")/../argv.log\"\nexec sleep 600\n",
    )
    .expect("write fake pi");
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
    for _ in 0..200 {
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
    panic!(
        "stand-in never recorded {n} launch(es); log: {:?}",
        std::fs::read_to_string(&path)
    );
}

/// Drive the real `qd` in the jail, with pi's session store pointed inside it too
/// so nothing reads or writes the developer's real `~/.pi`.
fn qd(
    j: &JailScaffold,
    pi_sessions: &Path,
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
        .env("PI_CODING_AGENT_SESSION_DIR", pi_sessions)
        .env("QD_PI_BIN", pane_bin)
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

/// The name pi gives this session's file — `<ts>_<id>.jsonl`, FLAT in the root.
///
/// FLAT, not `--<enc-cwd>--/<ts>_<id>.jsonl`, and the difference is the whole
/// point of this helper existing rather than the path being inlined. pi picks its
/// layout from WHO chose the session dir: left to itself it buckets by cwd under
/// `~/.pi/agent/sessions`, but handed a directory explicitly — which is what
/// `PI_CODING_AGENT_SESSION_DIR` does, and what the `qd` helper above sets — it
/// joins the filename straight onto that directory with no bucket at all.
/// Verified live against pi 0.80.2 in both configurations.
///
/// A fixture written in the OTHER layout would still pass a reader that looked
/// only in the bucket, which is exactly how the bucket-only search survived: the
/// test would be pinning qd's assumption instead of pi's behaviour.
fn session_file_path(root: &Path, id: &str) -> std::path::PathBuf {
    root.join(format!("2026-08-07T00-00-00-000Z_{id}.jsonl"))
}

/// Write the session file pi WOULD write for `id`: header line first, then a user
/// message and the assistant reply whose arrival is what makes pi flush at all.
fn write_session_file(root: &Path, cwd: &str, id: &str, user_text: &str) {
    std::fs::create_dir_all(root).expect("session dir");
    let body = format!(
        "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\
         \"timestamp\":\"2026-08-07T00:00:00.000Z\",\"cwd\":\"{cwd}\"}}\n\
         {{\"type\":\"message\",\"id\":\"e1\",\"parentId\":null,\
         \"timestamp\":\"2026-08-07T00:00:01.000Z\",\"message\":{{\"role\":\"user\",\
         \"content\":[{{\"type\":\"text\",\"text\":\"{user_text}\"}}]}}}}\n\
         {{\"type\":\"message\",\"id\":\"e2\",\"parentId\":\"e1\",\
         \"timestamp\":\"2026-08-07T00:00:02.000Z\",\"message\":{{\"role\":\"assistant\",\
         \"content\":[{{\"type\":\"text\",\"text\":\"ok\"}}]}}}}\n"
    );
    std::fs::write(session_file_path(root, id), body).expect("write session file");
}

fn pid_alive(pid: i64) -> bool {
    pid > 0 && unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Is `id` shaped like the v4 UUID the create path mints?
fn looks_like_uuid_v4(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    groups.iter().map(|g| g.len()).collect::<Vec<_>>() == vec![8, 4, 4, 4, 12]
        && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        && groups[2].starts_with('4')
}

/// The whole lane, in the order a user would exercise it.
///
/// KEPT AS ONE TEST, for the reason `codex_interactive_lane.rs` states and this
/// file learned the hard way: each step depends on the live pane the previous one
/// created, and spawning a real mux session per assertion is both slow and racy.
/// Written first as three tests, this flaked ~40% of runs — three jails creating
/// embedded-mux panes concurrently, with a revived pane reliably registering and
/// then sitting alive without ever executing its command. Serialized, or run
/// alone, every one of them passed. The step banners keep a failure's location
/// obvious, which is what the separate tests were bought for.
#[test]
fn interactive_pi_is_identified_from_birth_and_carries_its_session_across_revives() {
    let j = establish_jail(Path::new("/tmp/qd-piint"), "piint");
    let pi_sessions = j.root.join("pi_sessions");
    std::fs::create_dir_all(&pi_sessions).expect("pi sessions root");
    // The session's cwd, CANONICAL: `getcwd` resolves symlinks, so this is the
    // spelling the row must carry AND the spelling pi encodes into its bucket
    // name (on macOS /tmp is a symlink to /private/tmp).
    let work = std::fs::canonicalize(&j.root).expect("canonical jail root");
    let fake = install_fake_pi(&work);

    // --- 1. `--interactive` is still refused for acp/*, and no longer for pi ---
    //
    // FIRST, because it is the narrowing this lane required: pi was removed from a
    // refusal acp/* still needs. Asserting only the pi half would pass if the
    // refusal had been deleted outright, silently promising acp/* an attachable
    // session and delivering a daemon. Neither arm spawns a pane.
    let (rc_acp, _, err_acp) = qd(
        &j,
        &pi_sessions,
        &work,
        &fake,
        &["start", "acpsess", "--provider", "acp/opencode", "--interactive"],
    );
    assert_ne!(
        rc_acp, 0,
        "acp/* has no terminal to attach; --interactive must still refuse"
    );
    assert!(
        err_acp.contains("--interactive is not supported"),
        "the refusal should name the flag: {err_acp}"
    );
    assert!(
        err_acp.contains("claude-code, codex and pi"),
        "and should name the lanes that DO exist, now including pi: {err_acp}"
    );

    // --- 2. start, and the row is IDENTIFIED IMMEDIATELY ---------------------
    //
    // THE headline difference from codex. There is no unidentified window to wait
    // out, because we chose the id rather than discovering it.
    let (rc, stdout, stderr) = qd(
        &j,
        &pi_sessions,
        &work,
        &fake,
        &["start", "piauto", "--provider", "pi", "--interactive"],
    );
    assert_eq!(rc, 0, "start failed: {stdout}{stderr}");
    assert!(
        stdout.contains("attach with"),
        "start should point at attach, said: {stdout}"
    );

    assert_eq!(field(&j, "provider").as_deref(), Some("pi"));
    assert_eq!(
        field(&j, "hosting").as_deref(),
        Some("mux-pane"),
        "the load-bearing field: it routes attach/stop/send away from the resident lane"
    );
    assert_eq!(
        field(&j, "endpoint"),
        None,
        "a pane-hosted session has no resident front"
    );
    assert_eq!(
        field(&j, "cwd").as_deref(),
        Some(work.to_str().unwrap()),
        "the row must store the RESOLVED cwd — pi encodes that spelling into its \
         session directory name, so an unresolved one sends every transcript \
         lookup to a directory that cannot exist"
    );
    let sid = field(&j, "sessionId").expect(
        "a pi row is identified from birth — --session-id names the session at launch",
    );
    assert!(
        looks_like_uuid_v4(&sid),
        "the minted id should be a v4 UUID (collision-free, so --session-id can \
         never silently OPEN a stranger's session): {sid}"
    );

    // THE assertion the stand-in exists for: our id actually reached the process.
    assert_eq!(
        launches(&work, 1)[0],
        format!("LAUNCH --session-id {sid}"),
        "a fresh start must launch `pi --session-id <minted>` — not a bare `pi`, \
         which would let pi choose an id we could not address"
    );

    let pid: i64 = field(&j, "pid").unwrap().parse().expect("pid");
    assert!(pid_alive(pid), "the pane process should be alive");

    // --- 3. it lists, keyed by the id we chose -------------------------------
    let (_, ls, _) = qd(&j, &pi_sessions, &work, &fake, &["ls", "--json"]);
    assert!(ls.contains("piauto"), "ls should list it: {ls}");
    assert!(ls.contains(&sid), "ls should render the bound id: {ls}");

    // --- 4. the transcript pi eventually writes is FOUND ---------------------
    //
    // The end-to-end proof that the id and the cwd encoding agree: pi writes
    // nothing until its first assistant reply, and when it does, the file lands in
    // a bucket named after the cwd ITS process resolved. If the row stored the
    // caller's spelling instead, this lookup would fail forever — and fail
    // silently, since "no file yet" is also the legitimate pre-first-reply state.
    write_session_file(&pi_sessions, work.to_str().unwrap(), &sid, "hello pi");
    let (_, ls2, _) = qd(&j, &pi_sessions, &work, &fake, &["ls", "--json"]);
    assert!(
        ls2.contains(&format!("_{sid}.jsonl")),
        "ls must resolve the session transcript under the id we dictated: {ls2}"
    );

    // --- 5. resume reopens the SAME session (the human round trip) -----------
    //
    // This is what makes the lane whole: a human starts a session, works in it,
    // stops it, and comes back to the same conversation. A new session under the
    // old name would silently lose the history they came back for.
    qd(&j, &pi_sessions, &work, &fake, &["stop", "piauto"]);
    let (rc, out, err) = qd(&j, &pi_sessions, &work, &fake, &["resume", "piauto"]);
    assert_eq!(rc, 0, "resume failed: {out}{err}");
    assert!(
        out.contains("Revived pi session"),
        "resume must take the pane-revive arm, said: stdout={out:?} stderr={err:?}"
    );
    assert_eq!(
        field(&j, "sessionId").as_deref(),
        Some(sid.as_str()),
        "the revived row must carry the SAME session, not a fresh one"
    );
    assert_eq!(field(&j, "hosting").as_deref(), Some("mux-pane"));
    let revived_pid: i64 = field(&j, "pid").unwrap().parse().expect("pid");
    assert!(pid_alive(revived_pid), "the revived pane should be alive");
    assert_ne!(revived_pid, pid, "revive is a NEW process, not the old one");
    assert_eq!(
        launches(&work, 2)[1],
        format!("LAUNCH --session-id {sid}"),
        "revive must relaunch with the SAME id — `--session-id` reopens the \
         existing session, which is the whole conversation-carrying mechanism"
    );

    // Exactly one row survives: the old tombstone was consumed, so the session
    // does not haunt `ls --all` as a second entry.
    let tombs = std::fs::read_dir(j.home.join(".claude").join("sessions"))
        .expect("sessions dir")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tombstoned"))
        .count();
    assert_eq!(tombs, 0, "the prior tombstone should have been consumed");

    // --- 6. stop reaps the pane (the leak the hosting field prevents) --------
    qd(&j, &pi_sessions, &work, &fake, &["stop", "piauto"]);
    let mut gone = false;
    for _ in 0..40 {
        if !pid_alive(revived_pid) {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        gone,
        "stop must reap the pane; pid {revived_pid} survived (the resident \
         group-kill path would tombstone the row and leave this running)"
    );

    // --- 7. a NEVER-USED session still revives, and this diverges from codex --
    //
    // `revive_codex_tui` must refuse the equivalent step: codex mints no thread id
    // until someone types, so an unused codex row has no id at all, and launching a
    // bare `codex` would hand back a different conversation under the same name. A
    // pi row has had its id since birth, so reviving an unused one is well defined
    // — pi recreates a session under that same id (its file having never been
    // written) and the row keeps addressing exactly what it always did.
    //
    // Pinned rather than left implicit, because the codex twin's matching step
    // asserts the OPPOSITE outcome and a reader comparing the lanes should find
    // the difference stated.
    let (rc_f, out_f, err_f) = qd(
        &j,
        &pi_sessions,
        &work,
        &fake,
        &["start", "pifresh", "--provider", "pi", "--interactive"],
    );
    assert_eq!(rc_f, 0, "second start failed: {out_f}{err_f}");
    let fresh_sid = field(&j, "sessionId").expect("identified from birth");
    assert_ne!(fresh_sid, sid, "a second start mints its own id");

    // Stopped without ever being used: pi wrote no session file for it (its persist
    // defers until the first assistant reply), so there is no transcript to carry.
    qd(&j, &pi_sessions, &work, &fake, &["stop", "pifresh"]);
    assert!(
        !session_file_path(&pi_sessions, &fresh_sid).exists(),
        "precondition: an unused session leaves no transcript behind"
    );

    let (rc2, out2, err2) = qd(&j, &pi_sessions, &work, &fake, &["resume", "pifresh"]);
    assert_eq!(
        rc2, 0,
        "an unused pi session revives (contrast codex, which must refuse): {out2}{err2}"
    );
    assert_eq!(
        field(&j, "sessionId").as_deref(),
        Some(fresh_sid.as_str()),
        "the id is carried, not re-minted — the row keeps addressing the same session"
    );
    assert_eq!(
        launches(&work, 4)[3],
        format!("LAUNCH --session-id {fresh_sid}"),
        "the revive launches the same id, which pi creates-if-missing"
    );

    qd(&j, &pi_sessions, &work, &fake, &["stop", "pifresh"]);
}
