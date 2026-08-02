//! ACK-2 M5 — GATE-ASSEMBLY INTEGRATION ROWS (ack2-spec §11): the jail-level
//! event-sequence teeth G3, the three-arm boot-readiness gate G7, the
//! transcript-race G8, the §9 exit/stdout stability assertions, and the §G10
//! privacy grep. These drive the REAL `qd` binary (`CARGO_BIN_EXE_qd`) through
//! per-run hermetic fakerepl-backed jails against the embedded qrmux daemon —
//! the SAME engine path `c1_gate.rs` exercises, with `fakerepl` substituted for
//! claude via the `CLAUDE_BIN` override (launch.rs:23-27).
//!
//! ## How the chain works (validated, M5 probe)
//!
//! `qd start` boots `CLAUDE_BIN`; the embedded EventBootWaiter polls
//! `<HOME>/.claude/sessions/<pid>.json` for the named row. `fakerepl` writes that
//! row (status idle), reads its PTY stdin per the burst model, and on submit
//! appends a claude-shaped user record to `QD_FAKEREPL_CONVO_JSONL`. With
//! `QD_FAKEREPL_SESSION_ID` carried on the row, `qd`'s registry→sessionId→
//! find_jsonl_path chain resolves the transcript, so the W8 verify + the --wait
//! anchor loop land real `turn-anchored` events. Every event the wiring emits
//! lands in `<QD_HOME>/state/sessions/<key>.events.jsonl` (sessionId-keyed when
//! resolvable; `byname-<name>` on a failed boot).
//!
//! ## Jail invariants (rule 9 + ADD-4 + ADD-12 + ADD-14)
//!
//! Each row builds its own jail tempdir laid out EXACTLY as fakerepl's belt
//! requires: HOME=`<base>/qdrg-runs/<id>/home`, QD_HOME=`root/qd_home`,
//! ZMX_DIR=`root/zmx`, TMPDIR=`root/tmp`, plus an own XDG_RUNTIME_DIR (0700) so
//! the embedded qrmux socket dir is per-run. The base lives under a SHORT
//! literal-/tmp prefix so the qrmux `sun_path` fits macOS's 104-byte budget —
//! TEST infra only (ADD-14 governs ENGINE writes; the engine's own paths are
//! QD_HOME-honoring and asserted clean by C1's belt rows). Every payload carries
//! a distinctive KEY-SHAPED `sk-ack3canary<tag>…` marker (≥24-char body) built by
//! [`key_canary`]; [`assert_no_canary_in_events`] greps the whole events file for
//! it after each jail row (§G10). ADD-20 (ack3-spec §6.4): the key-shaped canary
//! MUST be redacted out of the `content_preview` (and absent everywhere) — that
//! is the load-bearing privacy grep under the redacted-preview regime. The G10
//! row adds the second lane: a PLAIN-text phrase from the same message is PRESENT
//! in the send-initiated `content_preview` (the flip is not vacuous).
//!
//! ## Skip mechanism (matches c1_gate)
//!
//! The rows need the `qrmux` binary (embedded backend) + the `fakerepl` binary.
//! [`require_bins`] PANICS with a build hint if either is absent — never a silent
//! vacuous pass (the c1_gate `qrmux_bin` contract). `fakerepl` carries the
//! fakerepl_gate staleness guard.

#![allow(clippy::too_many_arguments)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use dispatch::events::{parse_events, EventRecord, ReadResult};

// ===========================================================================
// Binary locators (c1_gate + fakerepl_gate patterns)
// ===========================================================================

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// `<target>/<profile>` from the running test exe (`.../deps/<testbin>`).
fn profile_dir() -> PathBuf {
    std::env::current_exe()
        .expect("current_exe")
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>")
        .to_path_buf()
}

/// The built `qrmux` binary (embedded backend). PANICS with a build hint if
/// absent — never a silent skip (c1_gate `qrmux_bin` contract).
fn qrmux_bin() -> PathBuf {
    let bin = profile_dir().join("qrmux");
    assert!(
        bin.exists(),
        "qrmux binary not found at {bin:?} — build it first: \
         scripts/build-lock.sh cargo build -p qrmux --bin qrmux"
    );
    bin
}

/// The built `fakerepl` binary, with the fakerepl_gate STALENESS GUARD (a binary
/// older than the newest fakerepl source is a stale oracle → fail loud). Missing
/// → build-once (a nested `cargo build` cannot deadlock the parent target lock
/// for a DIFFERENT package). NEVER a silent skip.
fn fakerepl_bin() -> PathBuf {
    let bin = profile_dir().join("fakerepl");
    if bin.exists() {
        if let Some(bin_mtime) = mtime(&bin) {
            let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../fakerepl/src")
                .canonicalize()
                .expect("fakerepl src dir");
            let newest_src = std::fs::read_dir(&src_dir)
                .expect("read fakerepl src")
                .flatten()
                .filter_map(|e| mtime(&e.path()))
                .max();
            if let Some(newest) = newest_src {
                assert!(
                    bin_mtime >= newest,
                    "STALE fakerepl binary at {bin:?} (older than {src_dir:?}) — \
                     the gate would test an outdated oracle. Run: cargo build -p fakerepl"
                );
            }
        }
        return bin;
    }
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "fakerepl"])
        .status()
        .expect("spawn cargo build -p fakerepl");
    assert!(status.success(), "cargo build -p fakerepl failed");
    assert!(
        bin.exists(),
        "fakerepl binary missing at {bin:?} after build"
    );
    bin
}

fn mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Assert both backend binaries exist (call at the top of every live row). The
/// asserts inside the locators fail LOUD — a missing binary is a build error, not
/// a skip.
fn require_bins() {
    let _ = qrmux_bin();
    let _ = fakerepl_bin();
}

// ===========================================================================
// Jail (fakerepl-belt shaped; c1_gate jail discipline)
// ===========================================================================

/// A per-run hermetic jail shaped EXACTLY as the fakerepl belt requires
/// (HOME=`*/qdrg-runs/<id>/home`, QD_HOME=`root/qd_home`, ZMX_DIR=`root/zmx`,
/// TMPDIR=`root/tmp`) plus an own XDG_RUNTIME_DIR for the embedded qrmux socket.
struct Jail {
    root: PathBuf,
    home: PathBuf,
    xdg: PathBuf,
    qd_home: PathBuf,
    /// state-tier sessions dir (`<QD_HOME>/state/sessions`) where events files land.
    ev_dir: PathBuf,
    /// pre-resolved convo JSONL path (a project dir under HOME); fakerepl writes here.
    convo: PathBuf,
    /// the sessionId fakerepl stamps on its registry row.
    uuid: String,
    /// session names created via `qd start` in this jail (reaped at teardown so the
    /// embedded-daemon-hosted fakerepl children never orphan + wedge the build lock).
    created: std::cell::RefCell<Vec<String>>,
}

impl Jail {
    fn establish(tag: &str) -> Jail {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // SHORT literal-/tmp base so the embedded qrmux sun_path fits (c1_gate
        // note); the qdrg-runs/<id> segment satisfies fakerepl's HOME belt.
        let base = PathBuf::from("/tmp/qd-ack2gate");
        let root = base.join("qdrg-runs").join(format!("{tag}-{nanos}"));
        let home = root.join("home");
        let xdg = base.join(format!("x-{tag}-{nanos}"));
        let qd_home = root.join("qd_home");
        let ev_dir = qd_home.join("state").join("sessions");
        let sessions = home.join(".claude").join("sessions");
        let projects = home.join(".claude").join("projects").join("proj");
        for d in [
            &sessions,
            &projects,
            &xdg,
            &qd_home,
            &root.join("tmp"),
            &root.join("zmx"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o700)).ok();
        common::assert_not_real_home(&home);
        // Each jail is fully hermetic (own HOME/QD_HOME), so a fixed uuid-shaped id
        // is fine — fakerepl stamps it on the registry row + writes the convo file
        // at the projects-dir path it resolves to. (uuid alphabet ⇒ never collides
        // with the `byname-` key prefix, §4.1.)
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

    /// Base env for `fakerepl` knobs shared by most rows.
    fn fakerepl_env<'a>(&'a self, name: &'a str) -> Vec<(&'a str, String)> {
        vec![
            ("QD_FAKEREPL_NAME", name.to_string()),
            ("QD_FAKEREPL_SESSION_ID", self.uuid.clone()),
            (
                "QD_FAKEREPL_CONVO_JSONL",
                self.convo.to_string_lossy().into_owned(),
            ),
        ]
    }

    /// Read the sessionId-keyed events file text (empty if absent).
    fn events_text(&self) -> String {
        std::fs::read_to_string(self.ev_dir.join(format!("{}.events.jsonl", self.uuid)))
            .unwrap_or_default()
    }

    /// Read the byname-keyed events file text (empty if absent).
    fn byname_events_text(&self, name: &str) -> String {
        std::fs::read_to_string(self.ev_dir.join(format!("byname-{name}.events.jsonl")))
            .unwrap_or_default()
    }

    /// All events-file text under the jail (for the privacy grep — covers both keys).
    fn all_events_text(&self) -> String {
        let mut out = String::new();
        let _ = walk(&self.qd_home, &mut |p| {
            if p.to_string_lossy().ends_with(".events.jsonl") {
                out.push_str(&std::fs::read_to_string(p).unwrap_or_default());
                out.push('\n');
            }
        });
        out
    }

    fn teardown(&self) {
        // Reap the embedded-daemon-hosted fakerepl children FIRST (per-target
        // `qd stop --force`, NEVER a destructive sweep — c1_gate discipline), so a
        // lingering fakerepl holding the PTY never orphans + wedges the build lock.
        // THEN remove the jail tree. `qd stop` runs under this jail's env so it
        // talks to this jail's embedded daemon only.
        let names: Vec<String> = self.created.borrow().clone();
        for name in names {
            let _ = run_qd(self, &["stop", "--force", &name], &[]);
        }
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.xdg);
    }
}

/// Recursive file walk (test infra).
fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) -> std::io::Result<()> {
    if dir.is_dir() {
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            if p.is_dir() {
                walk(&p, f)?;
            } else {
                f(&p);
            }
        }
    }
    Ok(())
}

// ===========================================================================
// qd driver
// ===========================================================================

/// Run `qd <args>` under the jail env with `fakerepl` as CLAUDE_BIN and the given
/// extra env (fakerepl knobs etc.). Returns (exit, stdout, stderr, elapsed).
fn run_qd(jail: &Jail, args: &[&str], extra: &[(&str, String)]) -> (i32, String, String, Duration) {
    // WP-B-CS-1 (D2): `qd start` now auto-detects the driver, and this harness pipes
    // stdio (`cmd.output()`), so a bare start would be a non-TTY caller → the HEADLESS
    // surface. These gate tests exercise the INTERACTIVE -p delivery + §9/ack2 event
    // emission (an interactive-path feature), so force the interactive surface with
    // the `--interactive` override (inserted right after the `start` subcommand).
    // Without it, start routes headless and emits NONE of the interactive-path events
    // asserted below. (Behavior delta — non-TTY `qd start -p` is headless by design
    // now — is flagged in the WP-B-CS-1 response for the red-team/integration.)
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
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", fr.parent().unwrap().display()),
        )
        .env("TERM", "xterm-256color")
        .env("CLAUDE_BIN", fr.to_string_lossy().into_owned());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let start = Instant::now();
    let out = cmd.output().expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        start.elapsed(),
    )
}

/// Override CLAUDE_BIN (e.g. /bin/sleep for the never-ready readiness arm).
fn run_qd_claude(
    jail: &Jail,
    args: &[&str],
    claude_bin: &str,
    extra: &[(&str, String)],
) -> (i32, String, String, Duration) {
    // WP-B-CS-1 (D2): force the INTERACTIVE surface for `start` (this harness pipes
    // stdio → a bare start would auto-detect the headless surface). Same reason +
    // delta as `run_qd`'s injection above.
    let injected: Vec<String>;
    let arg_refs: Vec<&str>;
    let args: &[&str] = if args.first() == Some(&"start") {
        injected = std::iter::once("start".to_string())
            .chain(std::iter::once("--interactive".to_string()))
            .chain(args[1..].iter().map(|s| s.to_string()))
            .collect();
        arg_refs = injected.iter().map(String::as_str).collect();
        &arg_refs
    } else {
        args
    };
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
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("CLAUDE_BIN", claude_bin);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let start = Instant::now();
    let out = cmd.output().expect("spawn qd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        start.elapsed(),
    )
}

// ===========================================================================
// Event-file helpers
// ===========================================================================

/// Parse the sessionId-keyed events file into records.
fn events_of(jail: &Jail) -> ReadResult {
    parse_events(&jail.events_text())
}

/// The event-name sequence for `send_id`, in file order.
fn seq_for(recs: &[EventRecord], send_id: &str) -> Vec<String> {
    recs.iter()
        .filter(|r| r.send_id().as_deref() == Some(send_id))
        .map(|r| r.event.clone())
        .collect()
}

/// The LAST `send-initiated` record's send_id for `verb` (send:pty / new-p). The
/// queue/idle rows seed a prior `new -p`, so we pick the row-under-test by verb +
/// send_path where needed.
fn send_id_for(recs: &[EventRecord], verb: &str, send_path: Option<&str>) -> Option<String> {
    recs.iter()
        .rfind(|r| {
            r.event == "send-initiated"
                && r.str_field("verb").as_deref() == Some(verb)
                && send_path
                    .map(|sp| r.str_field("send_path").as_deref() == Some(sp))
                    .unwrap_or(true)
        })
        .and_then(|r| r.send_id())
}

/// ADD-20 (ack3-spec §6.4): a KEY-SHAPED privacy canary for `tag`. `sk-` prefix +
/// a ≥24-char body (so BOTH the prefix rule AND the generic run belt would scrub
/// it). Embedded in each row's payload; [`assert_no_canary_in_events`] asserts it
/// is redacted out of EVERY events field (content_preview included). The body is
/// high-entropy-shaped (alnum) and unique per tag so a leak is unambiguous.
fn key_canary(tag: &str) -> String {
    // `sk-ack3canary-<tag>` then pad the body past 24 chars with a fixed alnum run.
    format!("sk-ack3canary{tag}0123456789ABCDEFGHIJ")
}

/// ADD-20 (§6.4) lane 2: a distinctive PLAIN-text phrase (no key prefix, every
/// run < 24 so it SURVIVES redaction) that the G10 row asserts is PRESENT in the
/// send-initiated `content_preview` — proving the preview ships and the flip is
/// not vacuous. Spaces keep every token short.
const PLAIN_PREVIEW_PHRASE: &str = "ack3 plain preview phrase ok";

/// §G10 PRIVACY: assert the KEY-SHAPED `canary` marker is ABSENT from EVERY events
/// file in the jail (the redacted-preview regime — ADD-20 §6.4). Called by every
/// jail row. The canary is a `sk-`-prefixed ≥24-body token, so a correct preview
/// redacts it to `[REDACTED:sk-…]`; a match here would be unambiguous raw-secret
/// leakage. Mutation: emitting raw text (un-redacted) in any event field REDs this.
fn assert_no_canary_in_events(jail: &Jail, canary: &str) {
    let text = jail.all_events_text();
    assert!(
        !text.contains(canary),
        "§G10 PRIVACY VIOLATION: raw payload canary {canary:?} found in an events \
         file (events must carry content_sha256 + content_len ONLY):\n{text}"
    );
    // Positive control: the events file is NON-empty (a vacuous-empty file would
    // pass the grep trivially). Each jail row writes at least a send-initiated /
    // priming record, so this holds on every caller.
    assert!(
        !text.trim().is_empty(),
        "§G10 positive control: events file is empty — the privacy grep would be \
         vacuous; the row must have emitted at least one record"
    );
}

/// Build a chunked (>1024B) payload carrying a privacy canary. The canary is at
/// the FRONT so it is in the first chunk; the filler makes the payload chunk and
/// drives the verify→anchored path.
fn chunked_payload(canary: &str) -> String {
    format!("{canary} {}", "X".repeat(1100))
}

// ===========================================================================
// G3 — per-path event SEQUENCES (jail, real qd binary + fakerepl + embedded mux)
// ===========================================================================

/// G3 idle-chunked: `qd start -p <chunked>` → send-initiated(new-p,idle) →
/// chunks-delivered → turn-anchored. The send-initiated carries chunks>1 +
/// matching chunk_sha256s + content fields. (The new -p verify read-back IS the
/// anchor; W8 Verified.)
#[test]
fn g3_seq_new_p_idle_chunked_anchors() {
    require_bins();
    let jail = Jail::establish("g3nc");
    let name = "g3nc";
    let canary = key_canary("1");
    let payload = chunked_payload(&canary);
    let env = jail.fakerepl_env(name);
    let mut env = env;
    env.push(("QD_FAKEREPL_BUSY_MS", "1500".to_string()));

    let (code, out, _err, _d) = run_qd(&jail, &["start", name, "-p", &payload], &env);
    assert_eq!(code, 0, "new -p chunked Accepted exit 0");
    // §9 stdout literal (G9 piggyback).
    assert!(
        out.contains(&format!("Prompt delivered to \"{name}\"")),
        "new -p stdout wording unchanged: {out:?}"
    );

    let recs = events_of(&jail).records;
    let sid = send_id_for(&recs, "new-p", Some("idle")).expect("send-initiated(new-p) present");
    let seq = seq_for(&recs, &sid);
    assert_eq!(
        seq,
        vec!["send-initiated", "chunks-delivered", "turn-anchored"],
        "G3 idle-chunked exact sequence"
    );

    // The send-initiated carries chunks>1 + matching per-chunk shas + content fields.
    let si = recs
        .iter()
        .find(|r| r.event == "send-initiated" && r.send_id().as_deref() == Some(&sid))
        .unwrap();
    let chunks = si.u64_field("chunks").unwrap();
    assert!(chunks > 1, "chunked payload → chunks>1 (got {chunks})");
    let shas = si
        .obj
        .get("chunk_sha256s")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(
        shas.len() as u64,
        chunks,
        "chunk_sha256s length == chunks (under the cap)"
    );
    // The shas MATCH the production splitter over the canonical payload.
    let expected: Vec<String> =
        dispatch::submit::chunk_text(&payload, dispatch::events::CHUNK_BYTES)
            .iter()
            .map(|c| dispatch::events::sha256_hex(c.as_bytes()))
            .collect();
    let got: Vec<String> = shas
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(got, expected, "chunk_sha256s match the production splitter");
    assert_eq!(
        si.u64_field("content_len"),
        Some(payload.len() as u64),
        "content_len = UTF-8 byte length"
    );
    assert_eq!(
        si.str_field("content_sha256"),
        Some(dispatch::events::sha256_hex(payload.as_bytes())),
        "content_sha256 = sha of the canonical text"
    );

    // The turn-anchored carries line_index >= 0 (u64, always ≥ 0).
    let anchor = recs
        .iter()
        .find(|r| r.event == "turn-anchored" && r.send_id().as_deref() == Some(&sid))
        .unwrap();
    assert!(
        anchor
            .obj
            .get("anchor")
            .and_then(|a| a.get("line_index"))
            .and_then(|v| v.as_u64())
            .is_some(),
        "turn-anchored carries anchor.line_index"
    );

    assert_no_canary_in_events(&jail, &canary);
    jail.teardown();
}

/// G3 idle-single: `qd start -p <single-chunk>` → send-initiated(idle) →
/// chunks-delivered; NO terminal (single-chunk never runs verify → stays dangling
/// by design, §9 "written" row).
#[test]
fn g3_seq_new_p_idle_single_no_terminal() {
    require_bins();
    let jail = Jail::establish("g3ns");
    let name = "g3ns";
    let canary = key_canary("2");
    // Single-chunk payload (< CHUNK_BYTES) carrying the canary.
    let payload = format!("{canary}-hello");
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_FAKEREPL_BUSY_MS", "1200".to_string()));

    let (code, out, _err, _d) = run_qd(&jail, &["start", name, "-p", &payload], &env);
    assert_eq!(code, 0, "new -p single Accepted exit 0");
    assert!(out.contains(&format!("Prompt delivered to \"{name}\"")));

    let recs = events_of(&jail).records;
    let sid = send_id_for(&recs, "new-p", Some("idle")).expect("send-initiated present");
    let seq = seq_for(&recs, &sid);
    assert_eq!(
        seq,
        vec!["send-initiated", "chunks-delivered"],
        "G3 idle-single: send-initiated + chunks-delivered, NO terminal (dangling)"
    );
    // Single-chunk: chunks == 1.
    let si = recs
        .iter()
        .find(|r| r.event == "send-initiated" && r.send_id().as_deref() == Some(&sid))
        .unwrap();
    assert_eq!(si.u64_field("chunks"), Some(1), "single-chunk → chunks==1");

    assert_no_canary_in_events(&jail, &canary);
    jail.teardown();
}

/// G3 queue path: hold the session busy (long BUSY_MS) via a prior `new -p`, then
/// `qd send:pty` (no --wait) → send-initiated(busy-queued) → chunks-delivered; NO
/// terminal. stdout "Message queued in ...".
#[test]
fn g3_seq_sendpty_queue_busy_queued() {
    require_bins();
    let jail = Jail::establish("g3q");
    let name = "g3q";
    let canary = key_canary("3");
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_FAKEREPL_BUSY_MS", "8000".to_string())); // long busy hold

    // Prior send: new -p makes the session busy for ~8s (seeds the convo too).
    let (c1, _o1, _e1, _) = run_qd(&jail, &["start", name, "-p", "seed-prior"], &env);
    assert_eq!(c1, 0, "prior new -p Accepted");

    // While busy → send:pty queues. Payload carries the canary.
    let qmsg = format!("{canary}-queued");
    let (c2, out2, _e2, _) = run_qd(&jail, &["send:pty", name, &qmsg], &env);
    assert_eq!(c2, 0, "queue send exit 0 (Message queued)");
    assert!(
        out2.contains(&format!("Message queued in \"{name}\" (session busy)")),
        "queue stdout wording unchanged: {out2:?}"
    );

    let recs = events_of(&jail).records;
    let sid = send_id_for(&recs, "send:pty", Some("busy-queued"))
        .expect("send-initiated(send:pty,busy-queued) present");
    let seq = seq_for(&recs, &sid);
    assert_eq!(
        seq,
        vec!["send-initiated", "chunks-delivered"],
        "G3 queue: send-initiated(busy-queued) + chunks-delivered, NO terminal"
    );
    let si = recs
        .iter()
        .find(|r| r.event == "send-initiated" && r.send_id().as_deref() == Some(&sid))
        .unwrap();
    assert_eq!(
        si.str_field("send_path"),
        Some("busy-queued".to_string()),
        "queue path → send_path busy-queued"
    );

    assert_no_canary_in_events(&jail, &canary);
    jail.teardown();
}

/// G3 --wait complete: seed convo, then `qd send:pty --wait <chunked>` →
/// ... → turn-anchored with anchor.line_index ≥ 0 AND status-transition records
/// present (discharges the status-transition jail coverage too).
#[test]
fn g3_seq_sendpty_wait_complete_anchored_with_status_transitions() {
    require_bins();
    let jail = Jail::establish("g3w");
    let name = "g3w";
    let canary = key_canary("4");
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_FAKEREPL_BUSY_MS", "1200".to_string()));

    // Seed the session + convo via new -p (so send:pty --wait resolves the jsonl).
    let (c1, _o1, _e1, _) = run_qd(&jail, &["start", name, "-p", "seed"], &env);
    assert_eq!(c1, 0);
    std::thread::sleep(Duration::from_millis(1500)); // settle back to idle

    let payload = chunked_payload(&canary);
    let (c2, _o2, _e2, _) = run_qd(
        &jail,
        &["send:pty", name, &payload, "--wait", "--timeout", "8"],
        &env,
    );
    assert_eq!(c2, 0, "--wait Complete exit 0");

    let recs = events_of(&jail).records;
    let sid =
        send_id_for(&recs, "send:pty", Some("idle")).expect("send-initiated(send:pty) present");
    let seq = seq_for(&recs, &sid);
    // m-1 (merge ruling, fixed in-window): EXACT sequence — the W8 verify emits
    // THE turn-anchored; the --wait Complete arm is suppressed when the verify
    // already anchored (one landed signal per send_id). Exact (not contains) so a
    // spurious extra terminal can never hide.
    assert_eq!(
        seq,
        vec!["send-initiated", "chunks-delivered", "turn-anchored"],
        "G3 --wait complete EXACT sequence (no duplicate turn-anchored)"
    );

    // status-transition records present (the §9 R4 seam; busy then idle observed).
    let statuses: Vec<String> = recs
        .iter()
        .filter(|r| r.event == "status-transition")
        .filter_map(|r| r.str_field("status"))
        .collect();
    assert!(
        statuses.contains(&"idle".to_string()),
        "status-transition records present (observed flips): {statuses:?}"
    );

    // The turn-anchored anchor carries line_index >= 0.
    let anchor = recs
        .iter()
        .find(|r| r.event == "turn-anchored" && r.send_id().as_deref() == Some(&sid))
        .unwrap();
    let li = anchor
        .obj
        .get("anchor")
        .and_then(|a| a.get("line_index"))
        .and_then(|v| v.as_u64());
    assert!(li.is_some(), "anchor.line_index present (≥0): {anchor:?}");

    assert_no_canary_in_events(&jail, &canary);
    jail.teardown();
}

/// G3 --wait timeout (THE WAIT-LOOP non-foreclosure): fakerepl under ABSORB never
/// submits our send → `qd send:pty --wait --timeout 3` times out → the `WaitOutcome::
/// TimedOut` arm emits NO terminal (amend rider 3, red-team finding G: a timed-out
/// --wait watch is in-band-UNDETERMINABLE — `anchored:true` is provably-landed, the
/// un-anchored path is still-queued-behind-the-turn — so foreclosing it with
/// anchor-timeout would FALSE-FAIL a possibly-landed send and foreclose recovery).
/// The send stays dead-dangling-recoverable; exit 1 + the timeout stderr are
/// unchanged. NO turn-anchored AND NO anchor-timeout for this send.
///
/// MUTATION CONTROL (INVERTED post amend rider 3): the EXACT-sequence assert now
/// forbids ANY terminal — RE-ADDING a foreclosing anchor-timeout at the
/// `WaitOutcome::TimedOut` arm makes this row RED (a spurious terminal can never hide
/// in an exact-sequence assert). Paired with delivery_recover_verb.rs, which proves
/// the compiled verb closes such a dead-dangling send from the transcript.
#[test]
fn g3_seq_sendpty_wait_timeout_no_foreclosing_terminal() {
    require_bins();
    let jail = Jail::establish("g3wt");
    let name = "g3wt";
    let canary = key_canary("5");
    // Pre-seed the convo file so --wait resolves the jsonl (fakerepl under ABSORB
    // never writes a NEW user record → the send never anchors).
    std::fs::write(
        &jail.convo,
        "{\"type\":\"user\",\"message\":{\"content\":\"seed\"}}\n",
    )
    .unwrap();
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_FAKEREPL_BUSY_MS", "1000".to_string()));
    env.push(("QD_FAKEREPL_ABSORB_ALL_CRS", "1".to_string()));

    // new WITHOUT -p (avoid the slow stalled-deliver path); fakerepl boots + writes
    // its pid file regardless of ABSORB.
    let (c1, _o1, _e1, _) = run_qd(&jail, &["start", name], &env);
    assert_eq!(c1, 0, "new (no -p) boots under ABSORB");
    std::thread::sleep(Duration::from_millis(800));

    let timeout_s = 3u64;
    let qmsg = format!("{canary}-neveranchors");
    let (c2, _o2, err2, _) = run_qd(
        &jail,
        &[
            "send:pty",
            name,
            &qmsg,
            "--wait",
            "--timeout",
            &timeout_s.to_string(),
        ],
        &env,
    );
    assert_eq!(c2, 1, "--wait TimedOut exit 1 (unchanged)");
    assert!(
        err2.contains("timeout") || err2.contains("Timed out"),
        "timeout stderr: {err2:?}"
    );

    let recs = events_of(&jail).records;
    let sid = send_id_for(&recs, "send:pty", Some("idle")).expect("send-initiated present");
    let seq = seq_for(&recs, &sid);
    // m-1 tightening + amend rider 3: EXACT sequence (chunks still mux-acked under
    // ABSORB — the absorption is CR-level; the never-going-busy path skips the W8
    // verify via verify_eligible=false, then the wait loop times out). Post finding
    // G the TimedOut arm mints NO terminal, so the exact sequence is the two-event
    // prefix — a spurious terminal (a re-added anchor-timeout, or any other) can
    // never hide in an exact assert.
    assert_eq!(
        seq,
        vec!["send-initiated", "chunks-delivered"],
        "G3 --wait timeout EXACT sequence — the TimedOut arm forecloses nothing (amend rider 3, finding G)"
    );
    // No foreclosing terminal of ANY kind — the send is dead-dangling-recoverable,
    // and `qd delivery:recover` (proven in delivery_recover_verb.rs) is its closer.
    assert!(
        !seq.iter()
            .any(|e| e == "anchor-timeout" || e == "pending-abandoned" || e == "turn-anchored"),
        "the TimedOut arm must mint NO terminal (recovery, not the door, closes it): {seq:?}"
    );

    assert_no_canary_in_events(&jail, &canary);
    jail.teardown();
}

// ===========================================================================
// G7 — boot-readiness, three arms (jail + fakerepl)
// ===========================================================================

/// The N for G7(a). Spec: N≥20. Overridable via ACK2_G7_N for quick local probes.
fn g7_n() -> usize {
    std::env::var("ACK2_G7_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20)
}

/// G7(a) PIPELINE ARM: N (≥20) `qd start -p <chunked>` primed sends each reach
/// turn-anchored in the events file. Proves the measurement pipeline + the
/// existing serialization+remediation hold under N. Records the NN/NN number.
#[test]
fn g7a_pipeline_arm_nn_anchored() {
    require_bins();
    let n = g7_n();
    let mut anchored = 0usize;
    let start = Instant::now();
    let mut failures: Vec<String> = Vec::new();
    for i in 0..n {
        let jail = Jail::establish(&format!("g7a{i}"));
        let name = format!("g7a{i}");
        let canary = key_canary(&format!("G7A{i}"));
        let payload = chunked_payload(&canary);
        let mut env = jail.fakerepl_env(&name);
        env.push(("QD_FAKEREPL_BUSY_MS", "1200".to_string()));

        let (code, _out, err, _d) = run_qd(&jail, &["start", &name, "-p", &payload], &env);
        let recs = events_of(&jail).records;
        let got_anchor = recs.iter().any(|r| {
            r.event == "turn-anchored"
                && r.str_field("content_sha256")
                    == Some(dispatch::events::sha256_hex(payload.as_bytes()))
        });
        if code == 0 && got_anchor {
            anchored += 1;
        } else {
            failures.push(format!(
                "iter {i}: exit={code} anchored={got_anchor} stderr={}",
                err.trim()
            ));
        }
        // Privacy holds per iteration too.
        assert_no_canary_in_events(&jail, &canary);
        jail.teardown();
    }
    eprintln!(
        "[G7a] {anchored}/{n} primed sends anchored in {:.1}s",
        start.elapsed().as_secs_f64()
    );
    assert_eq!(
        anchored, n,
        "G7(a) pipeline arm: {anchored}/{n} anchored — failures: {failures:?}"
    );
}

/// G7(b) MEASUREMENT ARM (the non-vacuous tooth): fakerepl ABSORB_ALL_CRS (the
/// remediation CRs are absorbed too → the swallow is INDUCED + unrecoverable) →
/// `qd start -p <chunked>` → exit 10 (Stalled, unchanged contract) AND the events
/// file shows send-initiated + chunks-delivered and NO terminal — the
/// written-never-anchored signature COUNTED from the file.
///
/// Post amend rider 3 (finding G): the lifecycle Stalled arm mints NO terminal. The
/// deliver bytes were written + `\r` submitted, so the turn may yet commit — an
/// `anchor-timeout` here would false-fail a possibly-landed prime and foreclose
/// recovery. So the swallow signature is now the two-event prefix with NO
/// turn-anchored AND NO anchor-timeout; exit 10 + the loud WARNING stderr are
/// unchanged (map_deliver_outcome), and `qd delivery:recover` is the closer
/// (delivery_recover_verb.rs). RE-ADDING a foreclosing terminal here REDs this row.
///
/// SLOW (~50s): the stalled deliver_prompt exhausts its full DELIVER_TIMEOUT_S
/// remediation budget against a never-accepting fakerepl. Kept in-suite (under the
/// 60s single-row bound); see the gate report runtime note.
#[test]
fn g7b_measurement_arm_induced_swallow_counted() {
    require_bins();
    let jail = Jail::establish("g7b");
    let name = "g7b";
    let canary = key_canary("G7B");
    let payload = chunked_payload(&canary);
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_FAKEREPL_BUSY_MS", "1500".to_string()));
    env.push(("QD_FAKEREPL_ABSORB_ALL_CRS", "1".to_string()));

    let (code, _out, _err, d) = run_qd(&jail, &["start", name, "-p", &payload], &env);
    eprintln!("[G7b] induced-swallow new -p took {:.1}s", d.as_secs_f64());
    assert_eq!(code, 10, "Stalled exit 10 (unchanged contract)");

    let recs = events_of(&jail).records;
    let sid = send_id_for(&recs, "new-p", Some("idle")).expect("send-initiated present");
    let seq = seq_for(&recs, &sid);
    // The COUNTED written-never-anchored signature — m-1 tightening + amend rider 3:
    // EXACT sequence (written = send-initiated + chunks-delivered; never-anchored =
    // NO terminal, the Stalled arm forecloses nothing; no spurious terminal can
    // hide). The signature is the two-event prefix with no turn-anchored terminal.
    assert_eq!(
        seq,
        vec!["send-initiated", "chunks-delivered"],
        "G7(b) written-never-anchored EXACT sequence — Stalled forecloses nothing (finding G): {seq:?}"
    );
    // No foreclosing terminal of ANY kind — dead-dangling-recoverable.
    assert!(
        !seq.iter()
            .any(|e| e == "anchor-timeout" || e == "pending-abandoned" || e == "turn-anchored"),
        "the lifecycle Stalled arm must mint NO terminal (recovery closes it): {seq:?}"
    );

    assert_no_canary_in_events(&jail, &canary);
    jail.teardown();
}

/// G7(c) READINESS ARM: CLAUDE_BIN=/bin/sleep boots a pane that never writes a
/// pid file → `qd start -p` REFUSES HONESTLY (nonzero exit, NO prompt ever
/// delivered). The guarded property is NO BLIND WRITE: regardless of WHICH
/// honest refusal fires, the prompt bytes are never sent.
///
/// TWO HONEST-REFUSAL PATHS (b3 watch realignment — orc-ruled). The test used
/// to demand the pid-file path specifically; under heavy concurrent load the
/// item-17 I6 attachability verify can HONESTLY return NotAttachable (the
/// Bug-D embedded-daemon registration race) BEFORE the boot waiter ever runs.
/// Both are honest refusals and the no-blind-write property holds in BOTH;
/// demanding only the first was a test-expectation conflation (the g7c flake
/// class). This test now fails ONLY on false-success / blind-write /
/// wrong-error — never on either honest refusal:
///   - PID-FILE path — stderr names the PID file. This covers BOTH the full
///     40s pid-file timeout AND the punch-6 pane-death fail-FAST (the
///     `/bin/sleep <name> ...` pane dies at startup on bad args → BootTimeout
///     whose detail names "before Claude Code wrote its PID file"). Either
///     way the boot waiter ran and emitted EXACTLY one
///     priming-readiness-timeout{phase:"pid-file"} (today's assertion).
///   - NotAttachable / Bug-D path — stderr names the not-attachable /
///     registration-failed shape → assert ZERO readiness events (the boot
///     waiter never ran, so no priming-readiness-timeout is CORRECT there).
///   - neither honest shape → FAIL as a wrong-error.
///
/// The UNCONDITIONAL guards (code != 0, convo empty, no send events, no
/// canary) hold on every path — that is what g7c exists to assert.
///
/// RUNTIME: up to ~40s on the full pid-file-timeout path (BootTimeouts
/// default, not env-overridable in production); the pane-death fail-fast and
/// NotAttachable paths return in seconds. Kept in-suite; see report runtime.
#[test]
fn g7c_readiness_arm_priming_timeout_no_blind_write() {
    require_bins();
    let jail = Jail::establish("g7c");
    let name = "g7c";
    let canary = key_canary("G7C");
    let payload = format!("{canary}-prompt");

    let start = Instant::now();
    // /bin/sleep never writes the named pid file → EventBootWaiter refuses,
    // OR (under load) the I6 verify refuses first with NotAttachable.
    let (code, _out, err, _d) =
        run_qd_claude(&jail, &["start", name, "-p", &payload], "/bin/sleep", &[]);
    eprintln!(
        "[G7c] readiness /bin/sleep took {:.1}s",
        start.elapsed().as_secs_f64()
    );

    // --- UNCONDITIONAL guards (the no-blind-write property — every path) -----
    assert_ne!(code, 0, "boot failure → nonzero exit (existing contract)");

    // NO-BLIND-WRITE: ZERO prompt bytes delivered — the convo JSONL is
    // absent/empty (the prompt code sits AFTER the early return on BOTH honest
    // paths; no mux.send of the payload).
    let convo = std::fs::read_to_string(&jail.convo).unwrap_or_default();
    assert!(
        convo.trim().is_empty(),
        "no-blind-write: ZERO prompt bytes delivered (convo absent/empty): {convo:?}"
    );
    // §G10 + NEGATIVE CONTROL: no send events anywhere, and the canary is absent.
    assert!(
        !jail.byname_events_text(name).contains("send-initiated"),
        "readiness failure emits NO send events (the prompt was never delivered)"
    );
    assert_no_canary_in_events(&jail, &canary);

    // --- CONDITIONAL on which honest-refusal path fired ----------------------
    let recs = parse_events(&jail.byname_events_text(name)).records;
    // The boot waiter ran (pid-file timeout OR the punch-6 pane-death
    // fail-fast); its BootTimeout detail names the PID file in both cases.
    let is_pidfile_path = err.contains("PID file");
    // The I6 NotAttachable / Bug-D registration-race shape (create.rs Display).
    let is_not_attachable = err.contains("not attachable in the zmx socket dir")
        || err.contains("registration failed (Bug D)");
    if is_pidfile_path {
        // EXACTLY one priming-readiness-timeout{phase:"pid-file"}.
        assert_eq!(
            recs.len(),
            1,
            "pid-file path: byname events file has exactly one record: {recs:?}"
        );
        assert_eq!(recs[0].event, "priming-readiness-timeout");
        assert_eq!(
            recs[0].str_field("phase"),
            Some("pid-file".to_string()),
            "phase pid-file"
        );
    } else if is_not_attachable {
        // Honest Bug-D refusal BEFORE the boot waiter ran → ZERO readiness
        // events is correct (no priming-readiness-timeout to emit).
        assert!(
            !jail
                .byname_events_text(name)
                .contains("priming-readiness-timeout"),
            "NotAttachable path: the boot waiter never ran, so NO \
             priming-readiness-timeout event is expected: {recs:?}"
        );
    } else {
        // Neither honest shape → a wrong-error regression.
        panic!(
            "g7c: stderr is neither the pid-file nor the NotAttachable/Bug-D \
             honest-refusal shape (wrong-error): {err:?}"
        );
    }
    jail.teardown();
}

/// G7 NEGATIVE CONTROL: a NORMAL boot (fakerepl, succeeds) emits NO
/// priming-readiness-timeout (the readiness event fires only on a failed boot).
/// This is arm (a)'s control, asserted explicitly per spec.
#[test]
fn g7_negative_control_normal_boot_no_readiness_event() {
    require_bins();
    let jail = Jail::establish("g7neg");
    let name = "g7neg";
    let payload = chunked_payload(&key_canary("G7NEG"));
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_FAKEREPL_BUSY_MS", "1000".to_string()));

    let (code, _out, _err, _d) = run_qd(&jail, &["start", name, "-p", &payload], &env);
    assert_eq!(code, 0, "normal boot succeeds");

    // No byname readiness file (sessionId resolved → sessionId-keyed file used),
    // and no priming-readiness-timeout anywhere.
    let all = jail.all_events_text();
    assert!(
        !all.contains("priming-readiness-timeout"),
        "normal boot emits NO priming-readiness-timeout: {all}"
    );
    jail.teardown();
}

// ===========================================================================
// G8 — transcript-race / C1-ppid class (events-reader level; no jail)
// ===========================================================================

/// G8 (C1-ppid race): a torn trailing line that COMPLETES between two reads flips
/// from skipped→parsed. We parse the SAME growing text twice (the reader's own
/// `parse_events`); the torn-tail record is invisible on the first read (silent
/// torn-tail skip) and visible once its line completes.
///
/// MUTATION: making the reader treat a torn tail as corruption (counting it /
/// surfacing it as a record) would break the "completes between polls flips
/// correctly" contract — the first read would mis-parse or mis-count.
#[test]
fn g8_c1_ppid_torn_tail_completes_between_polls_flips_verdict() {
    // A complete record + a TORN trailing record (no '\n', truncated mid-JSON).
    let complete = r#"{"v":1,"ts":"2026-06-06T00:00:00.000Z","pid":1,"seq":0,"session":"s","event":"send-initiated","send_id":"sid"}"#;
    let torn_partial = r#"{"v":1,"ts":"2026-06-06T00:00:01.000Z","pid":1,"seq":1,"session":"s","event":"turn-anch"#;

    // POLL 1: the file's tail is mid-append — the torn record is NOT yet parseable.
    let read1 = format!("{complete}\n{torn_partial}");
    let r1 = parse_events(&read1);
    assert_eq!(
        r1.records.len(),
        1,
        "torn tail skipped on poll 1 (only the complete record)"
    );
    assert_eq!(
        r1.corrupt_interior, 0,
        "the torn TRAILING line is NOT counted as corruption (it is in-flight)"
    );
    assert!(
        !r1.records.iter().any(|r| r.event == "turn-anchored"),
        "the incomplete terminal is invisible on poll 1"
    );

    // POLL 2: between the polls the writer FINISHED the line (+ its '\n'). The
    // record now parses → the verdict flips.
    let completed_terminal = r#"{"v":1,"ts":"2026-06-06T00:00:01.000Z","pid":1,"seq":1,"session":"s","event":"turn-anchored","send_id":"sid","content_sha256":"ab","anchor":{"transcript":"t","start_offset":0,"line_index":0}}"#;
    let read2 = format!("{complete}\n{completed_terminal}\n");
    let r2 = parse_events(&read2);
    assert_eq!(
        r2.records.len(),
        2,
        "the completed terminal is now visible on poll 2"
    );
    assert!(
        r2.records.iter().any(|r| r.event == "turn-anchored"),
        "the C1-ppid-race record completed between polls and flips the verdict"
    );
    // The terminal verdict for the send is now present (first_terminal_for finds it).
    assert!(
        dispatch::events::first_terminal_for(&r2.records, "sid").is_some(),
        "await_received-style verdict resolves once the terminal completes mid-poll"
    );
    assert!(
        dispatch::events::first_terminal_for(&r1.records, "sid").is_none(),
        "control: no verdict while the terminal line was still torn"
    );
}

/// G8b (C1-ppid race, await-poll form): an await_received-style poll over a file
/// that GAINS its terminal mid-poll returns the verdict. We use the events library
/// directly: parse before (dangling) and after (terminal landed) the mid-poll
/// completion. NEGATIVE: a foreign send_id stays unresolved.
#[test]
fn g8_await_poll_gains_terminal_mid_poll_returns_verdict() {
    let si = r#"{"v":1,"ts":"2026-06-06T00:00:00.000Z","pid":1,"seq":0,"session":"s","event":"send-initiated","send_id":"sidX"}"#;
    // Before: only the send-initiated → dangling, no terminal.
    let before = parse_events(&format!("{si}\n"));
    assert!(
        dispatch::events::first_terminal_for(&before.records, "sidX").is_none(),
        "dangling before the terminal lands"
    );
    // Mid-poll the terminal completes.
    let term = r#"{"v":1,"ts":"2026-06-06T00:00:02.000Z","pid":1,"seq":1,"session":"s","event":"anchor-timeout","send_id":"sidX","waited_ms":3000}"#;
    let after = parse_events(&format!("{si}\n{term}\n"));
    let verdict = dispatch::events::first_terminal_for(&after.records, "sidX");
    assert!(
        verdict.is_some(),
        "the poll returns the verdict once the terminal lands"
    );
    assert_eq!(verdict.unwrap().event, "anchor-timeout");
    // NEGATIVE: a different send_id is NOT resolved by this terminal.
    assert!(
        dispatch::events::first_terminal_for(&after.records, "other-sid").is_none(),
        "control: a foreign send_id stays dangling"
    );
}

// ===========================================================================
// G9 — zero --json/exit-code diffs on existing surfaces (§9 stability)
// ===========================================================================

/// G9: the §9-table exit codes + stdout wordings for the live jail paths match the
/// pre-ACK-2 wordings (the literals are in the cherry-picked source). The G3/G7
/// rows above already piggyback these assertions inline; this row consolidates the
/// EXPLICIT literal checks in one place so the stability claim is self-contained.
///
/// SCOPE (R5/L7, stated honestly): this pins EXIT CODES + STDOUT only. The
/// existing golden/parity suites (run UNCHANGED in CI) pin the broader surface;
/// additive stderr WARNs follow the A6 telemetry best-effort precedent and are
/// NOT byte-pinned here (the golden pins stderr PRESENCE, not bytes).
#[test]
fn g9_exit_codes_and_stdout_match_pre_ack2() {
    require_bins();

    // Accepted new -p chunked → exit 0, "Prompt delivered to ...".
    {
        let jail = Jail::establish("g9a");
        let name = "g9a";
        let payload = chunked_payload(&key_canary("G9A"));
        let mut env = jail.fakerepl_env(name);
        env.push(("QD_FAKEREPL_BUSY_MS", "1000".to_string()));
        let (code, out, _e, _) = run_qd(&jail, &["start", name, "-p", &payload], &env);
        assert_eq!(code, 0, "Accepted exit 0");
        assert_eq!(
            out.trim_end().lines().last(),
            Some(format!("Prompt delivered to \"{name}\"").as_str()),
            "new -p Accepted stdout literal: {out:?}"
        );
        jail.teardown();
    }

    // Queue send → exit 0, "Message queued in ... (session busy)".
    {
        let jail = Jail::establish("g9q");
        let name = "g9q";
        let mut env = jail.fakerepl_env(name);
        env.push(("QD_FAKEREPL_BUSY_MS", "8000".to_string()));
        let (c1, _o, _e, _) = run_qd(&jail, &["start", name, "-p", "seed"], &env);
        assert_eq!(c1, 0);
        let (c2, out2, _e2, _) = run_qd(&jail, &["send:pty", name, "qmsg"], &env);
        assert_eq!(c2, 0, "queue exit 0");
        assert!(
            out2.contains(&format!("Message queued in \"{name}\" (session busy)")),
            "queue stdout literal: {out2:?}"
        );
        jail.teardown();
    }

    // Idle send (no --wait) → exit 0, "Message sent to <name>".
    {
        let jail = Jail::establish("g9i");
        let name = "g9i";
        let mut env = jail.fakerepl_env(name);
        env.push(("QD_FAKEREPL_BUSY_MS", "1000".to_string()));
        // seed so the session exists + is idle.
        let (c1, _o, _e, _) = run_qd(&jail, &["start", name, "-p", "seed"], &env);
        assert_eq!(c1, 0);
        std::thread::sleep(Duration::from_millis(1500));
        // single-chunk idle send (no --wait): "Message sent to <name>".
        let (c2, out2, _e2, _) = run_qd(&jail, &["send:pty", name, "hello"], &env);
        assert_eq!(c2, 0, "idle send exit 0");
        assert!(
            out2.contains(&format!("Message sent to {name}")),
            "idle send stdout literal: {out2:?}"
        );
        jail.teardown();
    }

    // Stalled new -p → exit 10 (ABSORB). The byname/sessionId events show the
    // written-never-anchored signature; here we only assert the EXIT contract.
    // (Covered with event assertions in g7b; this is the lean exit-code pin.)
    // NOTE: omitted from g9 to keep g9 fast — the exit-10 path is asserted in g7b.

    // boot-failure → nonzero exit. (Covered in g7c; not repeated here to avoid a
    // second 40s boot timeout.)
}

// ===========================================================================
// G10 — privacy (consolidated, ADD-20 §6.4 TWO-LANE): assert_no_canary_in_events
// (the KEY-SHAPED absence lane) is called by EVERY jail row above. This row adds
// the second lane: under the redacted-preview regime the key-shaped canary MUST
// be redacted out (absent everywhere) WHILE a plain-text phrase from the SAME
// message is PRESENT in the send-initiated content_preview (the flip is not
// vacuous) and ABSENT from every other field. The positive detection control is
// retained (the grep would RED on a real leak).
// ===========================================================================

/// G10 two-lane privacy (ADD-20 §6.4):
///   LANE 1 (key-shaped absence): the `sk-`-prefixed ≥24-body canary is REDACTED
///     out of EVERY events field (content_preview included) — the load-bearing
///     privacy grep survives the flip.
///   LANE 2 (plain-text presence): a distinctive plain phrase from the SAME
///     message is PRESENT in the send-initiated content_preview (proving the
///     preview ships) and ABSENT from every OTHER record/field (sha-only there).
///   Positive control retained: a synthetic leaky record is still caught.
#[test]
fn g10_privacy_grep_is_load_bearing() {
    require_bins();
    let jail = Jail::establish("g10");
    let name = "g10";
    let canary = key_canary("G10");
    // The message carries the plain phrase FIRST (so it lands in the ≤256B
    // content_preview), then the key-shaped canary, then filler to force chunking.
    let payload = format!("{PLAIN_PREVIEW_PHRASE} {canary} {}", "X".repeat(1100));
    let mut env = jail.fakerepl_env(name);
    env.push(("QD_FAKEREPL_BUSY_MS", "1000".to_string()));

    let (code, _out, _e, _) = run_qd(&jail, &["start", name, "-p", &payload], &env);
    assert_eq!(code, 0);

    let events = jail.all_events_text();
    // LANE 1 — the key-shaped canary is ABSENT from EVERY events field (redacted).
    assert!(
        !events.contains(&canary),
        "LANE 1: key-shaped canary {canary:?} absent (redacted) from events: {events}"
    );
    // The payload SHA is PRESENT — the events DID describe the payload (the row is
    // not vacuously passing on an empty/missing file).
    let sha = dispatch::events::sha256_hex(payload.as_bytes());
    assert!(
        events.contains(&sha),
        "the payload content_sha256 IS present (events describe the payload by sha): {events}"
    );

    // LANE 2 — the plain phrase is PRESENT in a send-initiated content_preview AND
    // ABSENT from every other field. Parse the records and pull the preview value.
    let recs = events_of(&jail).records;
    let previews: Vec<String> = recs
        .iter()
        .filter(|r| r.event == "send-initiated")
        .filter_map(|r| r.str_field("content_preview"))
        .collect();
    assert!(
        previews.iter().any(|p| p.contains(PLAIN_PREVIEW_PHRASE)),
        "LANE 2: the plain phrase {PLAIN_PREVIEW_PHRASE:?} IS present in a \
         send-initiated content_preview (the preview ships): previews={previews:?}"
    );
    // ABSENT from every OTHER field: remove the content_preview VALUES from the
    // whole-events text; the plain phrase must not appear in what remains.
    let mut residue = events.clone();
    for p in &previews {
        residue = residue.replace(p.as_str(), "");
    }
    assert!(
        !residue.contains(PLAIN_PREVIEW_PHRASE),
        "LANE 2: the plain phrase appears ONLY in content_preview, nowhere else: {residue}"
    );

    // POSITIVE CONTROL: a synthetic events line carrying the raw key canary IS
    // caught by the same substring grep → the grep would RED on a real leak.
    let leaky = format!("{{\"event\":\"send-initiated\",\"content_preview\":\"{canary}\"}}");
    assert!(
        leaky.contains(&canary),
        "the grep detects the key canary in a leaky record (mutation control)"
    );

    // And the standard whole-jail key-shaped privacy assertion.
    assert_no_canary_in_events(&jail, &canary);
    jail.teardown();
}
