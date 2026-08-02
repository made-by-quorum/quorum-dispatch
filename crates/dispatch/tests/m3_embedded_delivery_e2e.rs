//! M3 GREEN full-chain embedded delivery e2e (F1 acceptance proof).
//!
//! Proves the send:pty single-writer handoff DELIVERS end-to-end against a harness
//! that presents a real claude-shaped composer (the `❯` prompt + Ctrl-U line-discard
//! + echo, via `fakerepl`'s opt-in `QD_FAKEREPL_COMPOSER_MODE=1`): qd `PendingDelivery`
//! → mux countdown+fire (plain-composer verify passes on `❯`) → inject → the
//! LandingProbe scans the transcript → a REAL `message-seen` for THIS send_id in the
//! authoritative ledger `<QD_HOME>/state/sessions/<sessionId>.events.jsonl` → `--wait`
//! reads it (exit 0) / no-`--wait` resolves it after the sender is gone. This is the
//! FULL chain, not a `❯`-presence check.
//!
//! Root-cause note (rt1 r1 F1): the deterministic ack2/ack3/c1 gates use a fakerepl
//! that NEVER modelled the `❯` composer a real claude presents (and the pre-M3
//! content-verified CR keyed on), so the mux's fire correctly verify-BLOCKS there —
//! a HARNESS artifact, not an M3 bug (M3's consumption is honest). This test closes
//! F1 for the `❯`-composer (claude-shaped) path. codex(`›`)/pi(no-glyph) delivery is
//! M4-fact-gated (a documented M3 deferral), NOT exercised here.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use dispatch::events::{parse_events, sha256_hex, EventRecord};

// --- binary locators (ack2_gate/ack3_matrix patterns) ----------------------

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}
fn profile_dir() -> PathBuf {
    std::env::current_exe()
        .expect("current_exe")
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>")
        .to_path_buf()
}
fn mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}
fn qrmux_bin() -> PathBuf {
    let bin = profile_dir().join("qrmux");
    assert!(
        bin.exists(),
        "qrmux binary not found at {bin:?} — build it: cargo build -p qrmux --bin qrmux"
    );
    bin
}
fn fakerepl_bin() -> PathBuf {
    let bin = profile_dir().join("fakerepl");
    assert!(
        bin.exists(),
        "fakerepl binary not found at {bin:?} — build it: cargo build -p fakerepl"
    );
    // Staleness guard: a fakerepl older than its source is a stale oracle.
    if let Some(bm) = mtime(&bin) {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fakerepl/src");
        if let Ok(rd) = std::fs::read_dir(&src) {
            if let Some(newest) = rd.flatten().filter_map(|e| mtime(&e.path())).max() {
                assert!(bm >= newest, "STALE fakerepl at {bin:?}: run cargo build -p fakerepl");
            }
        }
    }
    bin
}

// --- jail (minimal replica of the ack3_matrix embedded-fakerepl jail) -------

struct Jail {
    root: PathBuf,
    home: PathBuf,
    xdg: PathBuf,
    qd_home: PathBuf,
    ev_dir: PathBuf,
    convo: PathBuf,
    uuid: String,
    created: std::cell::RefCell<Vec<String>>,
}

impl Jail {
    fn establish(tag: &str) -> Jail {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = PathBuf::from("/tmp/qd-m3e2e");
        // NOTE: the `qdrg-runs` segment is LOAD-BEARING — fakerepl's jail belt
        // (`jail::assert_jailed_env`) REFUSES a HOME not matching `*/qdrg-runs/*/home`.
        let root = base.join("qdrg-runs").join(format!("{tag}-{nanos}"));
        let home = root.join("home");
        let xdg = base.join(format!("x-{tag}-{nanos}"));
        let qd_home = root.join("qd_home");
        let ev_dir = qd_home.join("state").join("sessions");
        let sessions = home.join(".claude").join("sessions");
        let projects = home.join(".claude").join("projects").join("proj");
        for d in [&sessions, &projects, &xdg, &qd_home, &root.join("tmp"), &root.join("zmx")] {
            std::fs::create_dir_all(d).unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o700)).ok();
        let uuid = "11111111-2222-3333-4444-555555555555".to_string();
        let convo = projects.join(format!("{uuid}.jsonl"));
        Jail {
            root,
            home,
            xdg,
            qd_home,
            ev_dir,
            convo,
            uuid,
            created: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// fakerepl env + the opt-in COMPOSER MODE (the whole point: a `❯` composer the
    /// mux fire's plain-composer verify accepts).
    fn fakerepl_env<'a>(&'a self, name: &'a str) -> Vec<(&'a str, String)> {
        vec![
            ("QD_FAKEREPL_NAME", name.to_string()),
            ("QD_FAKEREPL_SESSION_ID", self.uuid.clone()),
            ("QD_FAKEREPL_CONVO_JSONL", self.convo.to_string_lossy().into_owned()),
            ("QD_FAKEREPL_COMPOSER_MODE", "1".to_string()),
            // A short busy window so the turn completes + the reply lands quickly.
            ("QD_FAKEREPL_BUSY_MS", "300".to_string()),
        ]
    }

    fn events_text(&self) -> String {
        std::fs::read_to_string(self.ev_dir.join(format!("{}.events.jsonl", self.uuid)))
            .unwrap_or_default()
    }
    fn engine_records(&self) -> Vec<EventRecord> {
        parse_events(&self.events_text()).records
    }
    fn convo_text(&self) -> String {
        std::fs::read_to_string(&self.convo).unwrap_or_default()
    }
    fn teardown(&self) {
        let names: Vec<String> = self.created.borrow().clone();
        for name in names {
            let _ = run_qd(self, &["stop", "--force", &name], &[]);
        }
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.xdg);
    }
}

fn run_qd(jail: &Jail, args: &[&str], extra: &[(&str, String)]) -> (i32, String, String) {
    let injected: Vec<String>;
    let arg_refs: Vec<&str>;
    let args: &[&str] = if args.first() == Some(&"start") {
        if let Some(name) = args.get(1) {
            jail.created.borrow_mut().push((*name).to_string());
        }
        injected = std::iter::once("start".to_string())
            .chain(std::iter::once("--interactive".to_string()))
            .chain(args[1..].iter().map(|s| s.to_string()))
            .collect();
        arg_refs = injected.iter().map(String::as_str).collect();
        &arg_refs
    } else {
        args
    };
    let fr = fakerepl_bin();
    let _ = qrmux_bin(); // fail loud if the embedded backend binary is missing.
    let mut cmd = Command::new(qd_bin());
    cmd.args(args);
    cmd.env_clear()
        // Lifecycle-collapse A-3: relay readiness is DEFAULT-ON for `qd start`
        // now; these hermetic boots never write a relay sidecar, so opt out via
        // the transition alias (env "0" = explicit off; flag > env > default).
        .env("QD_BOOT_AWAIT_RELAY", "0")
        .env("HOME", &jail.home)
        .env("QD_HOME", &jail.qd_home)
        .env("XDG_RUNTIME_DIR", &jail.xdg)
        .env("TMPDIR", jail.root.join("tmp"))
        .env("ZMX_DIR", jail.root.join("zmx"))
        .env("PATH", format!("{}:/usr/bin:/bin", fr.parent().unwrap().display()))
        .env("TERM", "xterm-256color")
        .env("CLAUDE_BIN", fr.to_string_lossy().into_owned());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn send_id_for(recs: &[EventRecord], send_path: Option<&str>) -> Option<String> {
    recs.iter()
        .filter(|r| r.event == "send-initiated" && r.send_id().is_some())
        .filter(|r| send_path.map_or(true, |sp| r.str_field("send_path").as_deref() == Some(sp)))
        .next_back()
        .and_then(|r| r.send_id())
}

/// The FIRST terminal (7-set) event for `send_id`, polling the ledger up to
/// `budget` (the mux writes it ASYNChronously — the whole single-writer point). The
/// deterministic analog of the production `dispatch::sendpty::watch_terminal`.
fn poll_terminal(jail: &Jail, send_id: &str, budget: Duration) -> Option<EventRecord> {
    let deadline = Instant::now() + budget;
    loop {
        let recs = jail.engine_records();
        if let Some(t) = dispatch::events::first_terminal_for(&recs, send_id) {
            return Some(t);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// --wait FULL CHAIN: qd send:pty --wait → the mux fires (verify `❯` → inject →
/// LandingProbe transcript scan → message-seen) → watch_terminal reads it → exit 0;
/// and the ledger carries a REAL message-seen for the send_id with the payload sha.
#[test]
fn embedded_send_pty_wait_delivers_message_seen() {
    let jail = Jail::establish("wait");
    let name = "w";
    let env = jail.fakerepl_env(name);
    let marker = "M3E2E_WAIT_MARKER";

    let (cb, _o, _e) = run_qd(&jail, &["start", name, "-p", "seed"], &env);
    assert_eq!(cb, 0, "start booted");
    std::thread::sleep(Duration::from_millis(1000)); // seed turn done → idle `❯`.

    let (code, _out, err) = run_qd(
        &jail,
        &["send:pty", name, marker, "--wait", "--timeout", "30"],
        &env,
    );

    let recs = jail.engine_records();
    let sid = send_id_for(&recs, Some("idle")).unwrap_or_else(|| {
        panic!("send-initiated(idle) present; ledger=\n{}", jail.events_text())
    });
    let term = poll_terminal(&jail, &sid, Duration::from_secs(20)).unwrap_or_else(|| {
        panic!(
            "NO terminal for send_id={sid} within 20s (F1 would reproduce here without \
             composer-mode). stderr={err}\nledger=\n{}",
            jail.events_text()
        )
    });
    assert_eq!(
        term.event, "message-seen",
        "the mux resolves a delivered send to message-seen; got {} — ledger:\n{}",
        term.event,
        jail.events_text()
    );
    assert_eq!(
        term.str_field("content_sha256").as_deref(),
        Some(sha256_hex(marker.as_bytes()).as_str()),
        "message-seen.content_sha256 == sha256(the sent marker)"
    );
    // The transcript user record is the CLEAN marker (Ctrl-U clear-chord handled —
    // never `\x15`-polluted, which would have failed the LandingProbe as a mismatch).
    assert!(
        jail.convo_text().contains(marker),
        "the clean marker landed as a user record in the transcript:\n{}",
        jail.convo_text()
    );
    assert_eq!(code, 0, "send:pty --wait exits 0 on a delivered+replied send; stderr={err}");
    jail.teardown();
}

/// no-`--wait` FULL CHAIN: qd send:pty (no --wait) → exit 0 "queued" immediately;
/// the mux still resolves the send to exactly ONE terminal (message-seen) AFTER the
/// sender exits — drop-immune, no reader-presence dependency. We POLL the ledger for
/// it (the terminal is written asynchronously by the mux, the single-writer point).
#[test]
fn embedded_send_pty_no_wait_resolves_message_seen_after_sender_gone() {
    let jail = Jail::establish("nowait");
    let name = "n";
    let env = jail.fakerepl_env(name);
    let marker = "M3E2E_NOWAIT_MARKER";

    let (cb, _o, _e) = run_qd(&jail, &["start", name, "-p", "seed"], &env);
    assert_eq!(cb, 0, "start booted");
    std::thread::sleep(Duration::from_millis(1000));

    let (code, out, err) = run_qd(&jail, &["send:pty", name, marker], &env);
    assert_eq!(
        code, 0,
        "no-wait send:pty exits 0 (handed off / DeliveryQueued); stderr={err}"
    );
    assert!(
        out.contains("queued"),
        "no-wait stdout is the honest queued receipt (never a false 'landed'); got {out:?}"
    );

    // The sender has EXITED. The mux still resolves to exactly one terminal.
    let recs = jail.engine_records();
    let sid = send_id_for(&recs, Some("idle"))
        .unwrap_or_else(|| panic!("send-initiated present; ledger=\n{}", jail.events_text()));
    let term = poll_terminal(&jail, &sid, Duration::from_secs(20)).unwrap_or_else(|| {
        panic!(
            "NO terminal for send_id={sid} within 20s after the sender exited \
             (the mux must resolve a mux-held send to exactly one terminal). ledger=\n{}",
            jail.events_text()
        )
    });
    assert_eq!(
        term.event, "message-seen",
        "the mux-written terminal is message-seen; got {} — ledger:\n{}",
        term.event,
        jail.events_text()
    );
    // Exactly ONE terminal for this send_id (single-writer: the mux owns the one).
    // Re-read FRESH: the mux wrote the terminal asynchronously, AFTER the `recs`
    // snapshot above (which poll_terminal saw land in a later read).
    let recs = jail.engine_records();
    let terminals: Vec<&EventRecord> = recs
        .iter()
        .filter(|r| {
            r.send_id().as_deref() == Some(sid.as_str()) && dispatch::events::is_terminal(&r.event)
        })
        .collect();
    assert_eq!(terminals.len(), 1, "exactly one terminal per send_id (single-writer)");
    assert!(jail.convo_text().contains(marker), "the clean marker landed in the transcript");
    jail.teardown();
}

/// F2 (QS-7 W8 truncation preserved on --wait): a chunked send whose payload lands
/// TRUNCATED (fakerepl records a shorter shared-prefix user record) is detected by
/// the mux's LandingProbe as `LandingScan::Mismatch` → `turn-anchored-mismatch`
/// terminal → `--wait` maps it to an HONEST failure (exit 1), NEVER a false
/// message-seen. This is where the synchronous no-wait W8 exit-1 relocated: on the
/// mux delivery surface, surfaced through `--wait`. (The no-`--wait` path is
/// honestly `queued` with NO synchronous truncation verdict — an explicit
/// async-deferral contract, resolved by --wait here or M5(c) reconcile; documented
/// at the no-wait return in send.rs.)
#[test]
fn embedded_send_pty_wait_detects_truncation_as_mismatch() {
    let jail = Jail::establish("trunc");
    let name = "t";
    let mut env = jail.fakerepl_env(name);
    // fakerepl records only the first 8 bytes of the submitted composer → a
    // shorter, shared-prefix user record: the truncation signature.
    env.push(("QD_FAKEREPL_TRUNCATE_USER_RECORD_BYTES", "8".to_string()));
    let marker = "TRUNC_MARKER_LONG_ENOUGH_TO_CUT";

    let (cb, _o, _e) = run_qd(&jail, &["start", name, "-p", "seed"], &env);
    assert_eq!(cb, 0, "start booted");
    std::thread::sleep(Duration::from_millis(1000));

    let (code, _out, err) = run_qd(
        &jail,
        &["send:pty", name, marker, "--wait", "--timeout", "30"],
        &env,
    );

    let recs = jail.engine_records();
    let sid = send_id_for(&recs, Some("idle"))
        .unwrap_or_else(|| panic!("send-initiated present; ledger=\n{}", jail.events_text()));
    let term = poll_terminal(&jail, &sid, Duration::from_secs(20)).unwrap_or_else(|| {
        panic!("NO terminal for send_id={sid} within 20s; ledger=\n{}", jail.events_text())
    });
    assert_eq!(
        term.event, "turn-anchored-mismatch",
        "a truncated landing is detected as turn-anchored-mismatch (W8 preserved on --wait), \
         NOT message-seen; got {} — ledger:\n{}",
        term.event,
        jail.events_text()
    );
    assert_ne!(code, 0, "--wait maps a mismatch terminal to an HONEST failure exit; stderr={err}");
    jail.teardown();
}
