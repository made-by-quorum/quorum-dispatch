//! P0 QA — the id-identity matrix + coverage-gap rows (spec-w4-qa, Pete-ruled
//! STOP CONDITIONS A1-A4 + section B).
//!
//! Drives the REAL `qd` binary through per-run hermetic fakerepl-backed jails
//! (the ack2_gate.rs harness shape) and pins, for the claude provider:
//!
//!   A1. resume → SAME qb id (env AND ls agree)
//!   A2. fork (`start <name> --fork <session>`, the STATE-21 valued surface) →
//!       NEW qb id (engine side; the provider-level "fork mints a new UUID"
//!       fact is pinned by the `#[ignore]`d real-claude probe below)
//!   A3. RETIRED BY RULING (STATE 21, spec-w7-start-surface): branch-via-start
//!       (`--resume` WITHOUT `--fork`) was removed from the CLI — see the
//!       retirement note at the A3 section below for the recorded history.
//!   A4. stop→resume round-trip ×3 — (UUID, name, qb id) preserved each cycle.
//!
//! The codex half of the matrix: resume-revive same-id is unit-pinned in
//! resume_daemon.rs (`revived_daemon_env_carries_the_existing_stable_id`) and
//! live-pinned in codex_resume_kill_live.rs (ids-fold assertions, QD_CODEX_LIVE
//! gated); fork ABSENCE is pinned at the provider seam (provider_seam.rs: codex
//! `resume_args` ignores `fork` — no `--fork-session` shape exists) and at the
//! verb surface below (`codex_start_refuses_fork_loudly`).
//!
//! ## How a launch's ENV is observed (the "env AND ls agree" oracle)
//!
//! CLAUDE_BIN points at a generated WRAPPER script that appends one line per
//! launch — `LAUNCH <tag> argv=<argv> QD_SESSION_ID=<env value>` — to a jail
//! log, exports the fakerepl identity knobs (per-launch, NOT daemon-inherited:
//! the ack3_matrix lesson — a second session in one daemon silently inherits
//! the FIRST session's fakerepl env), then execs fakerepl. The wrapper sees the
//! POST-dot-source environment, i.e. exactly what a real claude process sees.
//!
//! ## Jail fidelity notes (learned empirically, see the QA report)
//!
//! - A stopped session is resumable only once a TRANSCRIPT exists: the cold
//!   join row derives from the jsonl scan, and its NAME comes from the
//!   transcript's `agent-name` record (jsonl.rs name precedence) — the
//!   tombstone row is shadowed by the ColdJsonl row (join.rs seen_session_ids).
//!   `seed_transcript` therefore writes the agent-name + a user record with a
//!   real cwd, mirroring what claude-code persists for every real session.
//! - fakerepl reads its registry-row name from `--name` argv (present at
//!   start) or `QD_FAKEREPL_NAME` (the resume launch carries no `--name`;
//!   claude re-derives the name from its transcript — the wrapper export is
//!   the fixture's stand-in for that).

mod common;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;

// The shared P0 jail scaffolding (spec-w9-simplify S3): the binary locators,
// the jail-belt dir scaffold, and the jailed runner live in
// tests/common/p0bins.rs (shared with p0_qafix.rs ONLY).
use common::p0bins::{
    establish_jail, fakerepl_bin, run_qd_jailed, qd_bin, qrmux_bin, JailScaffold,
};

fn require_bins() {
    let _ = qrmux_bin();
    let _ = fakerepl_bin();
}

// ===========================================================================
// Jail (ack2_gate shape: fakerepl-belt HOME + own QD_HOME/ZMX_DIR/TMPDIR/XDG)
// ===========================================================================

struct Jail {
    /// The shared jail-belt scaffold (root/home/xdg/qd_home) — p0bins.
    dirs: JailScaffold,
    /// A REAL dir used as the seeded transcripts' cwd (the resume cwd
    /// reality-check refuses a vanished dir).
    work: PathBuf,
    /// (name) created via `qd start` — reaped on Drop so a failed assert never
    /// leaks a fakerepl child holding the embedded daemon open.
    created: RefCell<Vec<String>>,
}

impl Jail {
    fn establish(tag: &str) -> Jail {
        // SHORT literal-/tmp base so the embedded qrmux sun_path fits (the
        // 104-byte macOS budget; c1_gate note).
        let dirs = establish_jail(Path::new("/tmp/qd-p0idm"), tag);
        let work = dirs.root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        Jail {
            dirs,
            work,
            created: RefCell::new(Vec::new()),
        }
    }

    /// Generate a per-launch CLAUDE_BIN wrapper: logs `LAUNCH <tag> argv=…
    /// QD_SESSION_ID=…`, exports the fakerepl identity for THIS launch, execs
    /// fakerepl. `uuid: None` ⇒ the booted row carries NO sessionId (the
    /// bind-residual arm).
    fn wrapper(&self, tag: &str, name: &str, uuid: Option<&str>) -> PathBuf {
        let path = self.dirs.root.join(format!("wrap-{tag}.sh"));
        let log = self.launches_log();
        let fr = fakerepl_bin();
        let identity = match uuid {
            Some(u) => format!(
                "export QD_FAKEREPL_SESSION_ID='{u}'\nexport QD_FAKEREPL_CONVO_JSONL='{}'\n",
                self.convo_path(u).display()
            ),
            None => String::new(),
        };
        let body = format!(
            "#!/bin/sh\n\
             echo \"LAUNCH {tag} argv=$* QD_SESSION_ID=$QD_SESSION_ID\" >> '{}'\n\
             {identity}export QD_FAKEREPL_NAME='{name}'\n\
             exec '{}' \"$@\"\n",
            log.display(),
            fr.display()
        );
        std::fs::write(&path, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn launches_log(&self) -> PathBuf {
        self.dirs.root.join("launches.log")
    }

    /// One log line per launch, in order.
    fn launches(&self) -> Vec<String> {
        std::fs::read_to_string(self.launches_log())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn convo_path(&self, uuid: &str) -> PathBuf {
        self.dirs
            .home
            .join(".claude")
            .join("projects")
            .join("proj")
            .join(format!("{uuid}.jsonl"))
    }

    /// Seed the transcript a REAL claude session would have left behind: the
    /// `agent-name` record (cold-row name derivation, jsonl.rs) + one user
    /// record carrying a cwd that exists (the resume reality-check).
    fn seed_transcript(&self, uuid: &str, name: &str) {
        // WP-B5-iii: include a completed turn (assistant/end_turn) so a
        // Mechanism-S fork has a SAFE boundary to clone at (resume tests ignore it).
        let body = format!(
            "{{\"type\":\"agent-name\",\"agentName\":\"{name}\"}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":\"hello\"}},\
             \"cwd\":\"{}\",\"sessionId\":\"{uuid}\"}}\n\
             {{\"type\":\"assistant\",\"sessionId\":\"{uuid}\",\
             \"message\":{{\"stop_reason\":\"end_turn\"}}}}\n",
            self.work.display()
        );
        std::fs::write(self.convo_path(uuid), body).unwrap();
    }

    fn ids_path(&self) -> PathBuf {
        self.dirs.qd_home.join("state").join("ids.jsonl")
    }

    fn ids_fold(&self) -> dispatch::idstore::IdMap {
        dispatch::idstore::fold(&self.ids_path())
    }
}

impl Drop for Jail {
    fn drop(&mut self) {
        // Reap created sessions FIRST (per-target stop, never a sweep) so a
        // lingering fakerepl never orphans + wedges the build lock — and run it
        // on PANIC too (this is why teardown lives in Drop here).
        let names: Vec<String> = self.created.borrow().clone();
        for name in names {
            let _ = run_qd_inner(self, None, &["stop", "--force", &name]);
        }
        let _ = std::fs::remove_dir_all(&self.dirs.root);
        let _ = std::fs::remove_dir_all(&self.dirs.xdg);
    }
}

/// Run `qd <args>` under the jail env. `claude_bin: None` ⇒ fakerepl directly
/// (fine for non-launching verbs).
fn run_qd_inner(jail: &Jail, claude_bin: Option<&Path>, args: &[&str]) -> (i32, String, String) {
    let cb = claude_bin
        .map(Path::to_path_buf)
        .unwrap_or_else(fakerepl_bin);
    run_qd_jailed(&jail.dirs, &cb, args, &[])
}

/// Run a verb that may LAUNCH a session (start/resume) under `wrapper`.
fn run_qd(jail: &Jail, wrapper: &Path, args: &[&str]) -> (i32, String, String) {
    // WP-B-CS-1 (D2): force the INTERACTIVE surface for `qd start` — this harness's
    // wrapper is a fake-claude CLAUDE_BIN script, NOT a PTY for `qd`, so a bare start
    // would auto-detect the HEADLESS surface (and the mint/bind identity these tests
    // assert is the interactive create-path flow). `resume` is intentionally NOT
    // forced: resume is ALWAYS headless now (D3), and the resume-identity tests that
    // depend on the OLD behavior are #[ignore]'d pending B5. Delta flagged in the
    // WP-B-CS-1 response.
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
    run_qd_inner(jail, Some(wrapper), args)
}

/// `qd ls --all --json` rows as (name, qdId, sessionId, status) tuples.
fn ls_rows(jail: &Jail) -> Vec<(Option<String>, Option<String>, String, String)> {
    let (code, out, err) = run_qd_inner(jail, None, &["ls", "--all", "--json"]);
    assert_eq!(code, 0, "ls --all --json exits 0; stderr: {err}");
    let rows: serde_json::Value = serde_json::from_str(&out).expect("ls --json parses");
    rows.as_array()
        .expect("array")
        .iter()
        .map(|r| {
            (
                r.get("name").and_then(|v| v.as_str()).map(str::to_string),
                r.get("qdId").and_then(|v| v.as_str()).map(str::to_string),
                r.get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                r.get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

/// The qdId `ls` surfaces for the row carrying `uuid` (must exist, must be one).
fn ls_qd_id_for(jail: &Jail, uuid: &str) -> Option<String> {
    let rows = ls_rows(jail);
    let matched: Vec<_> = rows.iter().filter(|r| r.2 == uuid).collect();
    assert_eq!(
        matched.len(),
        1,
        "exactly one ls row for uuid {uuid}: {rows:?}"
    );
    matched[0].1.clone()
}

/// WP-B7 PIECE 2: the `lineage` value `qd ls --json` surfaces on the row carrying
/// `uuid` — `None` when the row emits NO `lineage` key at all (the additive
/// non-fork case; the field is present ONLY for forks). Read straight off the
/// machine surface, so this exercises the render OUTPUT path end-to-end.
fn ls_lineage_for(jail: &Jail, uuid: &str) -> Option<String> {
    let (code, out, err) = run_qd_inner(jail, None, &["ls", "--all", "--json"]);
    assert_eq!(code, 0, "ls --all --json exits 0; stderr: {err}");
    let rows: serde_json::Value = serde_json::from_str(&out).expect("ls --json parses");
    let matched: Vec<_> = rows
        .as_array()
        .expect("array")
        .iter()
        .filter(|r| r.get("sessionId").and_then(|v| v.as_str()) == Some(uuid))
        .collect();
    assert_eq!(matched.len(), 1, "exactly one ls row for uuid {uuid}");
    matched[0]
        .get("lineage")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Extract the `--resume <uuid>` value from a launch log line (WP-B5-iii: a
/// Mechanism-S fork resumes its qd-minted seed uuid via plain `--resume`).
fn resume_arg_of(line: &str) -> Option<String> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    toks.iter()
        .position(|t| *t == "--resume")
        .and_then(|i| toks.get(i + 1).map(|s| s.to_string()))
}

/// Parse `QD_SESSION_ID=<v>` off a launch log line.
fn env_id_of(line: &str) -> String {
    line.rsplit("QD_SESSION_ID=")
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

const U1: &str = "11111111-2222-3333-4444-555555555555";

// ===========================================================================
// A1 — resume → SAME qb id (env AND ls agree)
// ===========================================================================

/// MUTATION EVIDENCE: keying the resume path's `mint_or_get` by anything other
/// than the provider UUID (or minting fresh on resume) reds the same-id
/// asserts; dropping QD_SESSION_ID from the resume env file reds the env-line
/// assert (the wrapper logs the POST-dot-source environment).
// WP-B5-ii-b (PROOF 3) RE-ENABLED: the B-CS-1 D3 deferral is closed. B5-i landed
// the child-pid identity row, and the supervisor-10 ruling wired QD_SESSION_ID into
// the headless resume launch env (daemon_headless.rs, via launch_env_pairs) — the
// PARITY fix the interactive path always had. Resume now carries the SAME recorded
// qdId in the child env AND on the ls surface (the row↔env consistency invariant),
// so this asserts the REAL D3 behaviour (not re-oracled).
#[test]
fn a1_claude_resume_same_qb_id_env_and_ls_agree() {
    require_bins();
    let jail = Jail::establish("a1");
    let wrap = jail.wrapper("wk", "wk", Some(U1));

    // START: mint-unbound → env carries the id → bind at boot-confirm.
    let (code, _out, err) = run_qd(&jail, &wrap, &["start", "wk"]);
    assert_eq!(code, 0, "start: {err}");
    let id_at_start = env_id_of(&jail.launches()[0]);
    assert!(
        dispatch::idstore::is_valid_id(&id_at_start),
        "launch env carried a well-formed qb id, got {id_at_start:?}"
    );
    // env AND ls agree at start.
    assert_eq!(ls_qd_id_for(&jail, U1).as_deref(), Some(&*id_at_start));
    // The id is BOUND to the provider UUID in the store.
    assert_eq!(
        jail.ids_fold().by_session.get(U1),
        Some(&id_at_start),
        "start bound the minted id to the booted UUID"
    );
    // RAW store shape (panel-review hardening: don't rely on the fold as the
    // only oracle — the writer and the folder could drift together): exactly
    // one mint event (unbound at write, carrying the name) + one bind event.
    let raw = std::fs::read_to_string(jail.ids_path()).unwrap();
    let lines: Vec<serde_json::Value> = raw
        .lines()
        .map(|l| serde_json::from_str(l).expect("well-formed store line"))
        .collect();
    assert_eq!(lines.len(), 2, "mint + bind: {raw}");
    assert_eq!(lines[0]["event"], "mint");
    assert_eq!(lines[0]["id"], id_at_start.as_str());
    assert_eq!(lines[0]["session_id"], serde_json::Value::Null);
    assert_eq!(lines[0]["name"], "wk");
    assert_eq!(lines[1]["event"], "bind");
    assert_eq!(lines[1]["id"], id_at_start.as_str());
    assert_eq!(lines[1]["session_id"], U1);

    // STOP → seed the transcript a real session would have → RESUME.
    jail.seed_transcript(U1, "wk");
    let (code, _o, err) = run_qd(&jail, &wrap, &["stop", "wk"]);
    assert_eq!(code, 0, "stop: {err}");
    let (code, _o, err) = run_qd(&jail, &wrap, &["resume", "wk", "--no-attach"]);
    assert_eq!(code, 0, "resume: {err}");

    let launches = jail.launches();
    assert_eq!(launches.len(), 2, "two launches: {launches:?}");
    // The resume launch passed the EXACT branch fragment: --resume <uuid>,
    // never --fork-session.
    assert!(
        launches[1].contains(&format!("--resume {U1}")),
        "resume argv: {}",
        launches[1]
    );
    assert!(
        !launches[1].contains("--fork-session"),
        "resume must not fork: {}",
        launches[1]
    );
    // A1: SAME qb id in the resumed process's ENV…
    assert_eq!(
        env_id_of(&launches[1]),
        id_at_start,
        "resume env carries the SAME qb id"
    );
    // …AND on the ls surface, with no extra mint in the store. Append-only:
    // the resume added NO lines (mint_or_get found the binding and returned).
    assert_eq!(ls_qd_id_for(&jail, U1).as_deref(), Some(&*id_at_start));
    assert_eq!(jail.ids_fold().by_id.len(), 1, "no second id minted");
    let raw_after = std::fs::read_to_string(jail.ids_path()).unwrap();
    assert_eq!(
        raw_after, raw,
        "the store is append-only and resume appended nothing"
    );
}

// ===========================================================================
// A4 — stop→resume round-trip ×3 (orc Q4): identity preserved every cycle
// ===========================================================================

/// Resumability after `stop` is the CONTRACT (the tombstone is the discovery
/// mechanism, not a terminal state). Three consecutive cycles; after each, the
/// (UUID, name, qb id) triple is unchanged on BOTH the env and ls surfaces.
// WP-B5-ii-b (PROOF 3) RE-ENABLED: see a1. The headless resume launch now injects
// the child's OWN recorded qdId as QD_SESSION_ID (daemon_headless.rs), so the
// (UUID, name, qdId) triple is preserved across every stop/resume cycle on BOTH the
// env and ls surfaces — the real D3 behaviour, not re-oracled.
#[test]
fn a4_claude_stop_resume_three_cycles_identity_stable() {
    require_bins();
    let jail = Jail::establish("a4");
    let wrap = jail.wrapper("wk", "wk", Some(U1));

    let (code, _o, err) = run_qd(&jail, &wrap, &["start", "wk"]);
    assert_eq!(code, 0, "start: {err}");
    let id0 = env_id_of(&jail.launches()[0]);
    jail.seed_transcript(U1, "wk");

    for cycle in 1..=3 {
        // Resolver breadth (panel-review hardening): the stop/resume TARGETS
        // rotate across name / full stable id / unique stable-id prefix — the
        // ACT verbs resolve stable-id queries, not just `info`.
        let prefix = &id0[..2];
        let (stop_q, resume_q) = match cycle {
            1 => ("wk".to_string(), id0.clone()),
            2 => (id0.clone(), prefix.to_string()),
            _ => (prefix.to_string(), "wk".to_string()),
        };
        let (code, _o, err) = run_qd(&jail, &wrap, &["stop", &stop_q]);
        assert_eq!(code, 0, "cycle {cycle} stop {stop_q}: {err}");
        // Between stop and resume the session is COLD on the read surface —
        // exactly one row for the UUID, never a live leftover.
        let rows = ls_rows(&jail);
        let u1_rows: Vec<_> = rows.iter().filter(|r| r.2 == U1).collect();
        assert_eq!(
            u1_rows.len(),
            1,
            "cycle {cycle}: one row when stopped: {rows:?}"
        );
        assert_eq!(
            u1_rows[0].3, "cold",
            "cycle {cycle}: stopped session reads cold: {rows:?}"
        );
        let (code, _o, err) = run_qd(&jail, &wrap, &["resume", &resume_q, "--no-attach"]);
        assert_eq!(code, 0, "cycle {cycle} resume {resume_q}: {err}");

        let launches = jail.launches();
        let last = launches.last().unwrap();
        assert!(
            last.contains(&format!("--resume {U1}")),
            "cycle {cycle}: resume preserves the UUID: {last}"
        );
        assert_eq!(
            env_id_of(last),
            id0,
            "cycle {cycle}: env qb id stable across the round trip"
        );
        let rows = ls_rows(&jail);
        let row = rows.iter().find(|r| r.2 == U1).expect("the U1 row");
        assert_eq!(
            row.0.as_deref(),
            Some("wk"),
            "cycle {cycle}: name preserved: {rows:?}"
        );
        assert_eq!(
            row.1.as_deref(),
            Some(&*id0),
            "cycle {cycle}: ls qb id stable"
        );
    }
    // Three cycles, one mint line's worth of ids: the store never grew.
    assert_eq!(jail.ids_fold().by_id.len(), 1, "no ids minted by resumes");
}

// ===========================================================================
// A2 — fork → NEW qb id (STATE-21 surface: `start <name> --fork <session>`)
// ===========================================================================

/// `start <new-name> --fork <session>` resolves the target session (name /
/// full qb id / unambiguous prefix — the standard pipeline). WP-B5-iii
/// Mechanism S: qd pre-mints the fork's OWN uuid, seeds `<fork_uuid>.jsonl`
/// from the target's transcript (copy/rekey/truncate at a SAFE boundary), and
/// launches a PLAIN `--resume <fork_uuid>` (NO `--fork-session`). Identity is
/// option A: `mint_or_get(fork_uuid)` mints the fork's OWN qb id bound to its
/// qd-minted uuid — NEVER the parent's. (Real-claude provider semantics for the
/// underlying resume pinned by `a3_real_claude_provider_semantics`.) Inheriting
/// the original qb id, or resuming the PARENT's uuid, would be THE bug.
///
/// Two fork arms: by NAME (over the stopped original), then by qb-id PREFIX
/// (over the live first fork) — both resolution tiers drive the same path.
///
/// MUTATION EVIDENCE: keying identity off the PARENT's uuid (`mint_or_get(parent)`
/// — the old --fork-session shape) reds the ids-differ + own-uuid-binding
/// asserts; emitting `--fork-session` (native, not Mechanism S) reds the
/// no-fork-session assert; breaking prefix resolution reds the wk3 arm.
#[test]
fn a2_claude_fork_mints_new_qb_id() {
    require_bins();
    let jail = Jail::establish("a2");
    let wrap1 = jail.wrapper("wk", "wk", Some(U1));

    let (code, _o, err) = run_qd(&jail, &wrap1, &["start", "wk"]);
    assert_eq!(code, 0, "start wk: {err}");
    let id1 = env_id_of(&jail.launches()[0]);
    jail.seed_transcript(U1, "wk");
    let (code, _o, err) = run_qd(&jail, &wrap1, &["stop", "wk"]);
    assert_eq!(code, 0, "stop wk: {err}");

    // FORK by NAME off the stopped session, under a new name. WP-B5-iii
    // Mechanism S: qd mints the fork's OWN uuid PRE-spawn, seeds
    // <fork_uuid>.jsonl from wk's transcript (rekeyed), and launches `--resume
    // <fork_uuid>` (NO --fork-session — qd did the copy). The wrapper does NOT
    // pin a session id (uuid:None) so fakerepl adopts the qd-minted uuid via
    // --resume (faithful real-claude behavior).
    let wrap2 = jail.wrapper("wk2", "wk2", None);
    let (code, _o, err) = run_qd(&jail, &wrap2, &["start", "wk2", "--fork", "wk"]);
    assert_eq!(code, 0, "start wk2 --fork wk: {err}");

    let launches = jail.launches();
    let fork_launch = &launches[1];
    let fork_uuid = resume_arg_of(fork_launch).expect("fork argv carries --resume <fork_uuid>");
    assert_ne!(
        fork_uuid, U1,
        "Mechanism S resumes the qd-minted SEED uuid, NOT the parent's: {fork_launch}"
    );
    assert!(
        !fork_launch.contains("--fork-session"),
        "Mechanism S uses a plain --resume of the seed, never native --fork-session: {fork_launch}"
    );
    let id2 = env_id_of(fork_launch);
    assert!(dispatch::idstore::is_valid_id(&id2), "fork env id: {id2:?}");
    // THE pin: a forked session is a NEW identity (never the original's).
    assert_ne!(id2, id1, "fork must NOT inherit the original qb id");

    let ids = jail.ids_fold();
    assert_eq!(
        ids.by_session.get(U1),
        Some(&id1),
        "original binding intact"
    );
    assert_eq!(
        ids.by_session.get(&fork_uuid),
        Some(&id2),
        "fork's own qb id bound to its qd-minted uuid (option A: mint_or_get(fork_uuid))"
    );
    assert_eq!(ls_qd_id_for(&jail, &fork_uuid).as_deref(), Some(&*id2));
    assert_eq!(ls_qd_id_for(&jail, U1).as_deref(), Some(&*id1));

    // WP-B5-iii obl-4: the real fork launch recorded a lineage pointer to the
    // PARENT's qdId (id1 = wk), keyed by the fork's uuid — and STRICTLY the
    // parent, never the fork's OWN id (id2). (Surfacing onto Session.lineage via
    // fill_lineage in the live join is unit-proven in idstore.rs.)
    let lineage = jail.ids_fold();
    assert_eq!(
        lineage.by_parent.get(&fork_uuid),
        Some(&id1),
        "fork lineage → PARENT qdId recorded at launch"
    );
    assert_ne!(
        lineage.by_parent.get(&fork_uuid),
        Some(&id2),
        "GUARDRAIL: lineage is the parent, not the fork's own id"
    );

    // WP-B7 PIECE 2 — the lineage pointer now surfaces on the `qd ls --json` OUTPUT
    // (additive, parent-pointer-only). RED before the render.rs lineage emission:
    // with no `lineage` key on the row, the fork's `ls_lineage_for` is `None` and
    // the `Some(id1)` assert fails. GREEN after. The non-fork PARENT row (U1) emits
    // NO `lineage` key at all — additive both-ways, the same precedent as `qdId`.
    assert_eq!(
        ls_lineage_for(&jail, &fork_uuid).as_deref(),
        Some(&*id1),
        "fork's `ls --json` row emits lineage = PARENT qdId (id1)"
    );
    assert_ne!(
        ls_lineage_for(&jail, &fork_uuid).as_deref(),
        Some(&*id2),
        "GUARDRAIL: emitted lineage is the parent (id1), NEVER the fork's own id (id2)"
    );
    assert_eq!(
        ls_lineage_for(&jail, U1),
        None,
        "non-fork PARENT row emits NO lineage key (additive — present only for forks)"
    );

    // FORK by qb-id PREFIX off the LIVE first fork (live targets are legal — a
    // fork is a new participant). Mechanism S already seeded wk2's transcript
    // (<fork_uuid>.jsonl, with an end_turn carried from wk), so it is itself
    // forkable. The query is id2's shortest prefix NOT shared with id1, floored
    // at 2 chars (the resolver stable-id-prefix tier minimum, resolve.rs e).
    let split = id1
        .bytes()
        .zip(id2.bytes())
        .position(|(a, b)| a != b)
        .expect("distinct ids");
    let prefix = &id2[..(split + 1).max(2)];
    let wrap3 = jail.wrapper("wk3", "wk3", None);
    let (code, _o, err) = run_qd(&jail, &wrap3, &["start", "wk3", "--fork", prefix]);
    assert_eq!(code, 0, "start wk3 --fork {prefix}: {err}");
    let launches = jail.launches();
    let fork3 = &launches[2];
    let fork3_uuid = resume_arg_of(fork3).expect("prefix-fork argv carries --resume");
    assert_ne!(fork3_uuid, fork_uuid, "second fork seeds its OWN uuid");
    assert!(
        !fork3.contains("--fork-session"),
        "plain --resume of the seed, no native fork: {fork3}"
    );
    let id3 = env_id_of(fork3);
    assert!(
        dispatch::idstore::is_valid_id(&id3),
        "fork3 env id: {id3:?}"
    );
    assert_ne!(id3, id2, "second fork is a third identity");
    assert_eq!(
        jail.ids_fold().by_session.get(&fork3_uuid),
        Some(&id3),
        "second fork bound to ITS qd-minted uuid"
    );
}

// ===========================================================================
// A3 — RETIRED BY RULING (STATE 21, spec-w7-start-surface, 2026-06-10)
//
// `a3_claude_branch_without_fork_shares_the_qb_id` pinned the BRANCH-VIA-START
// surface: `start <new-name> --resume <uuid>` (no fork) → the provider KEEPS
// the UUID ⇒ the engine pre-bound via `mint_or_get(uuid)` → the SAME qb id on
// a second process. The STATE-21 ruling REMOVED `--resume` from start entirely
// (TS-parity residue, redundant with the resume verb — and the source of the
// two-live-participants-one-id hazard this suite had escalated), so the engine
// arm is gone: `start --resume` is an unknown option (pinned in cli.rs +
// p0_qafix.rs) and the wave-2 `pre_bound` mint arm was removed with it.
//
// The PROVIDER-LEVEL fact stays pinned: `--resume <uuid>` WITHOUT
// `--fork-session` keeps the UUID (resume ≡ branch at the provider level) —
// see the `#[ignore]`d `a3_real_claude_provider_semantics` probe below, which
// drives the real `claude` binary directly and is UNTOUCHED by the CLI ruling
// (last run 2026-06-10, claude 2.1.170: PASS).
// ===========================================================================

// ===========================================================================
// A2/A3 provider half — the REAL claude binary (ignored: needs network+auth)
// ===========================================================================

/// The provider-level empirical pin behind a2/a3: real `claude` headless.
///
///   1. fresh `-p` run            → mints UUID X
///   2. `-p --resume X`           → session_id == X  (branch KEEPS the UUID)
///   3. `-p --resume X --fork-session` → session_id != X (fork mints NEW)
///
/// `#[ignore]`: drives the REAL binary against the REAL HOME (auth/keychain +
/// network + ~30s and a few cents of spend). Scripted runner:
///
///   cargo test -p quorum-dispatch --test p0_id_matrix -- --ignored a3_real_claude
///
/// Last run 2026-06-10 (claude 2.1.170): PASS — resume kept
/// 863cbef7-…, fork minted 27ea36f0-… (recorded in the P0 QA report).
#[test]
#[ignore = "drives the real `claude` binary: needs auth + network + model spend"]
fn a3_real_claude_provider_semantics() {
    let probe_dir = std::env::temp_dir().join(format!("p0qa-claude-probe-{}", std::process::id()));
    std::fs::create_dir_all(&probe_dir).unwrap();
    let run = |extra: &[&str], prompt: &str| -> String {
        let mut args = vec!["-p"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&[prompt, "--output-format", "json", "--model", "haiku"]);
        let out = Command::new("claude")
            .args(&args)
            .current_dir(&probe_dir)
            .output()
            .expect("spawn real claude");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "claude {args:?} failed: {stdout} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: serde_json::Value = serde_json::from_str(&stdout).expect("result json");
        v["session_id"].as_str().expect("session_id").to_string()
    };

    let u_fresh = run(&[], "Reply with exactly: ok1");
    let u_branch = run(&["--resume", &u_fresh], "Reply with exactly: ok2");
    assert_eq!(
        u_branch, u_fresh,
        "PROVIDER PIN: --resume WITHOUT --fork-session KEEPS the session id"
    );
    let u_fork = run(
        &["--resume", &u_fresh, "--fork-session"],
        "Reply with exactly: ok3",
    );
    assert_ne!(
        u_fork, u_fresh,
        "PROVIDER PIN: --fork-session mints a NEW session id"
    );
}

// ===========================================================================
// A2 codex — asserted ABSENCE at the verb surface (loud refusal, orc-ruled)
// ===========================================================================

/// Codex has NO fork operation: the provider seam ignores `fork`
/// (provider_seam.rs pins `resume_args(key, true) == ["resume", id]` — no
/// `--fork-session` shape exists for codex). QA originally captured the verb
/// arm silently DROPPING `--resume`/`--fork` and surfaced the question; the
/// orchestrator RULED (qafix R2, 2026-06-10) that the codex arm must REFUSE
/// each flag loudly, teaching `qd resume <name>` as the revive path. STATE 21
/// then removed `--resume` from start entirely (its refusal arm died with the
/// flag), so this row pins the surviving `--fork <session>` refusal SHAPE:
/// exit 1, refusal-not-silent-drop, zero state. (Exact wording is byte-pinned
/// in p0_qafix.rs.)
#[test]
fn codex_start_refuses_fork_loudly() {
    let jail = Jail::establish("cdx");
    // No wrapper: the R2 refusal fires before any codex-on-PATH probe (and
    // before fork-target resolution — "T-1" needn't exist).
    let (code, _out, err) = run_qd_inner(
        &jail,
        None,
        &["start", "cx", "--provider", "codex", "--fork", "T-1"],
    );
    assert_eq!(code, 1, "codex + fork → loud refusal; stderr: {err}");
    assert!(
        err.contains("not supported with --provider codex") && err.contains("qd resume"),
        "the failure is the R2 refusal teaching the revive path: {err}"
    );
    // Nothing was minted and no row was written before the refusal exit.
    assert!(!jail.ids_path().exists(), "no id minted: {err}");
    let sessions = jail.dirs.home.join(".claude").join("sessions");
    assert_eq!(
        std::fs::read_dir(&sessions).unwrap().count(),
        0,
        "no registry rows written"
    );
}

// ===========================================================================
// B — bind-residual WARNING (wave-2 open-q 3): row present, sessionId missing
// ===========================================================================

/// The booted row carries NO sessionId at the verb's post-boot read (real-world
/// shape: claude registered its row but hasn't stamped sessionId yet, and never
/// does before the read). The start still SUCCEEDS; the warning is LOUD and
/// names the unbound id; the mint stays reserved-but-unbound (never surfaces on
/// any session; `ls` shows the id-less row as `---`).
#[test]
fn b_bind_residual_unbound_mint_warns_loud() {
    require_bins();
    let jail = Jail::establish("bres");
    let wrap = jail.wrapper("wk", "wk", None); // no QD_FAKEREPL_SESSION_ID

    let (code, out, err) = run_qd(&jail, &wrap, &["start", "wk"]);
    assert_eq!(code, 0, "the session is up — exit 0; stderr: {err}");
    assert!(out.contains("Started detached session \"wk\""), "{out}");

    let minted = env_id_of(&jail.launches()[0]);
    assert!(dispatch::idstore::is_valid_id(&minted));
    // The WARNING is loud + names the unbound id (pinning the wording's
    // load-bearing fragments, not the full sentence).
    assert!(
        err.contains("WARNING")
            && err.contains("carries no sessionId yet")
            && err.contains(&minted)
            && err.contains("unbound"),
        "loud bind-residual warning naming {minted}: {err}"
    );
    // The mint is reserved (collision-checked forever) but UNBOUND.
    let ids = jail.ids_fold();
    assert_eq!(
        ids.by_id.get(&minted),
        Some(&None),
        "the mint stays unbound: {ids:?}"
    );
    assert!(ids.by_session.is_empty(), "no UUID binding exists");
    // The id never surfaces on any session: the row is id-less on every surface.
    let rows = ls_rows(&jail);
    let wk = rows.iter().find(|r| r.0.as_deref() == Some("wk")).unwrap();
    assert_eq!(wk.1, None, "row carries no qdId: {rows:?}");
    // WP-B7 PIECE 1 adapt: this row asserts the id-less placeholder on the SHORT
    // TEXT surface. Under the table→JSON auto-flip an agent caller's bare `--short`
    // auto-flips to JSON, so we inject `--table` (the surface selector) to keep the
    // short surface — `--table --short` is the ratified agent short-text escape
    // hatch (surface=Table + content=short ⇒ short table). Coverage preserved
    // (still pins `---` + name on the short surface), not masked.
    let (code, out, _e) = run_qd_inner(&jail, None, &["ls", "--table", "--short"]);
    assert_eq!(code, 0);
    assert!(
        out.lines().any(|l| l.contains("---") && l.contains("wk")),
        "--short shows the id-less placeholder: {out}"
    );

    // CONTINUATION (panel-review hardening): the row later gains a sessionId
    // (claude stamps it after the verb's read). The warning's promised
    // divergence is REAL and pinned: `ls` lazily mints a DIFFERENT id for the
    // UUID; the launch-env id stays reserved-but-unbound FOREVER (never
    // surfaces on any session, still collision-checked).
    let sessions = jail.dirs.home.join(".claude").join("sessions");
    let row_path = std::fs::read_dir(&sessions)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "json"))
        .expect("the wk row");
    let row: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&row_path).unwrap()).unwrap();
    let mut stamped = row.clone();
    stamped["sessionId"] = serde_json::Value::String("late-uuid-0001".into());
    std::fs::write(&row_path, serde_json::to_string(&stamped).unwrap()).unwrap();

    let (code, _out, _err) = run_qd_inner(&jail, None, &["ls"]);
    assert_eq!(code, 0);
    let ids = jail.ids_fold();
    let late_id = ids
        .by_session
        .get("late-uuid-0001")
        .cloned()
        .expect("ls lazy-minted for the late-stamped UUID");
    assert_ne!(
        late_id, minted,
        "the divergence the warning names: the lazy mint is a DIFFERENT id"
    );
    assert_eq!(
        ids.by_id.get(&minted),
        Some(&None),
        "the launch-env id stays unbound forever"
    );
}

// ===========================================================================
// B — ls lazy-mint FAILURE: store unwritable → warned, ---, exit 0
// ===========================================================================

/// A pre-existing session (row with a sessionId, no mapped id) meets an
/// UNWRITABLE id store at `qd ls`: the backfill mint fails, the row degrades to
/// a warned id-less row (`---`), and ls still exits 0 (a read surface never
/// hard-fails on engine-state writes).
///
/// MUTATION EVIDENCE: propagating the mint error as a nonzero ls exit (or
/// dropping the row) reds this; silently swallowing the error (no stderr) reds
/// the warning assert.
#[test]
fn b_ls_lazy_mint_failure_degrades_warned_exit_0() {
    let jail = Jail::establish("lmf");
    // A live row (the test runner's own pid is alive) with a sessionId.
    let pid = std::process::id() as i64;
    std::fs::write(
        jail.dirs
            .home
            .join(".claude")
            .join("sessions")
            .join(format!("{pid}.json")),
        format!(
            r#"{{"pid":{pid},"sessionId":"lazy-uuid-0001","cwd":"/w","status":"idle","name":"lz"}}"#
        ),
    )
    .unwrap();
    // Make the store unwritable: ids.jsonl as a DIRECTORY (open-for-append fails).
    std::fs::create_dir_all(jail.ids_path()).unwrap();

    // WP-B7 PIECE 1 adapt: this asserts the HUMAN TABLE degrades the id-less row
    // to `---`. Under the auto-flip a bare agent `ls` yields JSON, so we inject
    // `--table` (the explicit human-table surface) to keep the table assertion.
    // The stderr-warning assert below is surface-independent; the `--json`
    // degradation is separately pinned at the end of this test.
    let (code, out, err) = run_qd_inner(&jail, None, &["ls", "--table"]);
    assert_eq!(
        code, 0,
        "ls exits 0 despite the mint failure; stderr: {err}"
    );
    assert!(
        err.contains("qd ls: idstore:"),
        "the warning names the verb AND the failing subsystem (not some \
         unrelated stderr line): {err:?}"
    );
    assert!(
        out.lines().any(|l| l.contains("lz") && l.contains("---")),
        "the row shows the id-less placeholder: {out}"
    );
    // The --json surface degrades the same way: row present, no qdId key.
    let (code, out, _e) = run_qd_inner(&jail, None, &["ls", "--json"]);
    assert_eq!(code, 0);
    let rows: serde_json::Value = serde_json::from_str(&out).unwrap();
    let row = &rows.as_array().unwrap()[0];
    assert_eq!(row["name"], "lz");
    assert!(row.get("qdId").is_none(), "no qdId on a failed mint: {row}");
}

// ===========================================================================
// B — whoami: env id pointing at a TOMBSTONED row's UUID
// ===========================================================================

/// QD_SESSION_ID resolves through the idstore to a UUID whose only registry
/// presence is a TOMBSTONE. PINNED ANSWER: the env path still answers (exit 0,
/// identitySource "env", sessionId + qdId known) but the tombstoned row
/// contributes NOTHING — name/pid are absent (whoami's row lookup excludes
/// tombstones), so the human surface prints the UUID, not the dead row's name.
#[test]
fn b_whoami_env_id_of_tombstoned_row_answers_without_the_dead_name() {
    let jail = Jail::establish("whot");
    let state = jail.dirs.qd_home.join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("ids.jsonl"),
        r#"{"v":1,"ts":"t","event":"mint","id":"ab3kx9mq","session_id":"tomb-uuid-0001","name":"wk"}"#,
    )
    .unwrap();
    std::fs::write(
        jail.dirs
            .home
            .join(".claude")
            .join("sessions")
            .join("90001.json.tombstoned"),
        r#"{"pid":90001,"sessionId":"tomb-uuid-0001","status":"idle","name":"wk"}"#,
    )
    .unwrap();

    let run_whoami = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(qd_bin())
            .args(args)
            .env_clear()
            .env("HOME", &jail.dirs.home)
            .env("QD_HOME", &jail.dirs.qd_home)
            .env("QD_SESSION_ID", "ab3kx9mq")
            .env("PATH", "/usr/bin:/bin")
            .output()
            .expect("spawn qd");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    let (code, out, err) = run_whoami(&["whoami", "--json"]);
    assert_eq!(code, 0, "env identity answers: {err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["identitySource"], "env");
    assert_eq!(v["sessionId"], "tomb-uuid-0001");
    assert_eq!(v["qdId"], "ab3kx9mq");
    assert_eq!(
        v["name"],
        serde_json::Value::Null,
        "dead name NOT surfaced: {v}"
    );
    assert_eq!(
        v["pid"],
        serde_json::Value::Null,
        "dead pid NOT surfaced: {v}"
    );

    // Human surface: name||sessionId → the UUID.
    let (code, out, _e) = run_whoami(&["whoami"]);
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "tomb-uuid-0001");
}

// ===========================================================================
// B — retired stubs: --help renders help; the stub does NOT fire
// ===========================================================================

#[test]
fn b_retired_stub_help_renders_without_firing_the_stub() {
    let jail = Jail::establish("help");
    for (verb, pointer, stub_line) in [
        (
            "new",
            "(retired — use qd start)",
            "is retired; use `qd start`",
        ),
        (
            "kill",
            "(retired — use qd stop)",
            "is retired; use `qd stop`",
        ),
    ] {
        for flag in ["--help", "-h"] {
            let (code, out, err) = run_qd_inner(&jail, None, &[verb, flag]);
            assert_eq!(code, 0, "qd {verb} {flag} renders help, exit 0: {err}");
            assert!(
                out.contains(pointer),
                "qd {verb} {flag} help points at the live verb: {out}"
            );
            assert!(
                !err.contains(stub_line),
                "the stub must NOT fire on {flag}: {err}"
            );
        }
    }
}

// ===========================================================================
// B — registry queryability + stable-id prefix resolution (bin level)
// ===========================================================================

/// Two live rows, ids sharing a 2-char prefix, pre-seeded store. Pins:
/// ls --json qdId/qdIdPrefix; ls --short handles; info "Stable ID:"; full-id +
/// unique-prefix resolution; ambiguous-prefix LOUD refusal; whoami env path
/// joining the live row.
#[test]
fn b_queryability_surfaces_and_stable_id_resolution() {
    let jail = Jail::establish("query");
    let state = jail.dirs.qd_home.join("state");
    std::fs::create_dir_all(&state).unwrap();
    // ab3kx9mq / ab47qrst share the 2-char prefix "ab" → display prefixes ab3/ab4.
    std::fs::write(
        state.join("ids.jsonl"),
        concat!(
            r#"{"v":1,"ts":"t","event":"mint","id":"ab3kx9mq","session_id":"qa-uuid-aaaa-0001","name":"wka"}"#,
            "\n",
            r#"{"v":1,"ts":"t","event":"mint","id":"ab47qrst","session_id":"qa-uuid-bbbb-0002","name":"wkb"}"#,
            "\n"
        ),
    )
    .unwrap();
    // Two live rows (the runner's pid + pid 1 are both alive; status strings keep
    // the rows live on the read surfaces).
    let sessions = jail.dirs.home.join(".claude").join("sessions");
    let pid = std::process::id() as i64;
    std::fs::write(
        sessions.join(format!("{pid}.json")),
        format!(
            r#"{{"pid":{pid},"sessionId":"qa-uuid-aaaa-0001","cwd":"/w","status":"idle","name":"wka"}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        sessions.join("1.json"),
        r#"{"pid":1,"sessionId":"qa-uuid-bbbb-0002","cwd":"/w","status":"idle","name":"wkb"}"#,
    )
    .unwrap();

    // ls --json: additive qdId + qdIdPrefix per row.
    let (code, out, err) = run_qd_inner(&jail, None, &["ls", "--json"]);
    assert_eq!(code, 0, "{err}");
    let rows: serde_json::Value = serde_json::from_str(&out).unwrap();
    let field = |name: &str, key: &str| -> String {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("row {name}: {rows}"))[key]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(field("wka", "qdId"), "ab3kx9mq");
    assert_eq!(field("wkb", "qdId"), "ab47qrst");
    assert_eq!(
        field("wka", "qdIdPrefix"),
        "ab3",
        "shared 2-char prefix extends"
    );
    assert_eq!(field("wkb", "qdIdPrefix"), "ab4");

    // ls --short: the prefix IS the handle. WP-B7 PIECE 1 adapt: inject `--table`
    // so this exercises the SHORT TEXT surface (the test's intent) rather than
    // silently passing on JSON post-flip — JSON happens to also contain "ab3"/"wka"
    // (qdIdPrefix/name), so a bare `--short` would VACUOUSLY pass under the agent
    // auto-JSON and erode the --short coverage. `--table --short` = short table.
    let (_c, out, _e) = run_qd_inner(&jail, None, &["ls", "--table", "--short"]);
    assert!(out.contains("ab3") && out.contains("wka"), "{out}");
    assert!(out.contains("ab4") && out.contains("wkb"), "{out}");

    // info: by full stable id AND by unique 3-char prefix; the Stable ID line.
    for query in ["ab3kx9mq", "ab3"] {
        let (code, out, err) = run_qd_inner(&jail, None, &["info", query]);
        assert_eq!(code, 0, "info {query}: {err}");
        assert!(out.contains("Stable ID:   ab3kx9mq"), "info {query}: {out}");
        assert!(out.contains("qa-uuid-aaaa-0001"), "info {query}: {out}");
    }
    // Ambiguous 2-char prefix → LOUD refusal, never a guess.
    let (code, _out, err) = run_qd_inner(&jail, None, &["info", "ab"]);
    assert_eq!(code, 1, "ambiguous prefix refuses: {err}");
    assert!(err.contains("Ambiguous"), "loud Many: {err}");

    // whoami --json on the env path joins the LIVE row (name + pid + source).
    let out = Command::new(qd_bin())
        .args(["whoami", "--json"])
        .env_clear()
        .env("HOME", &jail.dirs.home)
        .env("QD_HOME", &jail.dirs.qd_home)
        .env("QD_SESSION_ID", "AB3KX9MQ") // case-insensitive resolution
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("spawn qd");
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(v["identitySource"], "env");
    assert_eq!(v["name"], "wka");
    assert_eq!(v["qdId"], "ab3kx9mq");

    // whoami WITHOUT the env var: the ppid-walk fallback answers at the bin
    // level (the forged wka row is keyed by THIS test process's pid — a real
    // ancestor of the spawned qd), with the qdId joined READ-ONLY from the
    // fold and identitySource "ppid".
    let out = Command::new(qd_bin())
        .args(["whoami", "--json"])
        .env_clear()
        .env("HOME", &jail.dirs.home)
        .env("QD_HOME", &jail.dirs.qd_home)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("spawn qd");
    assert_eq!(out.status.code(), Some(0), "ppid fallback answers");
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(v["identitySource"], "ppid");
    assert_eq!(v["name"], "wka");
    assert_eq!(
        v["qdId"], "ab3kx9mq",
        "qdId joined read-only on the walk path"
    );
}
