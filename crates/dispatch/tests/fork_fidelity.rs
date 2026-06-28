//! WP-B5-iii — BINDING FIDELITY GATE (obl charge rider 2 / §Q4-FOLLOWUP).
//!
//! Proves qd's Mechanism-S seed (`fork_seed` copy/rekey) is EQUIVALENT to claude's
//! NATIVE `claude --resume <parent> --fork-session` output for the SAME parent,
//! normalized ONLY for the new session id + the appended turn. The oracle is a
//! REAL native fork (not a synthetic/hand-written expected file). NON-VACUOUS +
//! FULL (every parent MESSAGE record present, in order, re-keyed — no dropped /
//! reordered / truncated records) with TEETH (a deliberately-broken rekey or a
//! dropped record MUST red the equivalence).
//!
//! Real-claude, isolated HOME (the `engine-research/harness/lib.sh` `env -i`
//! contract — live `~/.claude*` NEVER written). `#[ignore]` — run explicitly:
//!   cargo test -p dispatch --test fork_fidelity -- --ignored --nocapture
//! Skips (not fails) if the claude binary or credentials are absent.

use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const CLAUDE_BIN: &str = "/home/u/.local/bin/claude";
const LIVE_CREDS: &str = "/home/u/.claude/.credentials.json";

/// One isolated sandbox: synthetic HOME + CLAUDE_CONFIG_DIR, OAuth credential
/// copied in, a trusted work cwd. Mirrors `lib.sh` qb_create. Destroyed on drop.
struct Sandbox {
    root: PathBuf,
    config: PathBuf,
    work: PathBuf,
}

impl Sandbox {
    fn create() -> Option<Self> {
        if !Path::new(CLAUDE_BIN).exists() || !Path::new(LIVE_CREDS).exists() {
            return None;
        }
        let root = std::env::temp_dir().join(format!("qd-forkfid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let config = root.join("config");
        let work = root.join("work");
        std::fs::create_dir_all(&config).ok()?;
        std::fs::create_dir_all(&work).ok()?;
        std::fs::create_dir_all(root.join("home")).ok()?;
        std::fs::copy(LIVE_CREDS, config.join(".credentials.json")).ok()?;
        // Trusted-cwd + onboarding seed so headless -p never blocks on a dialog.
        let cfg = serde_json::json!({
            "hasCompletedOnboarding": true,
            "bypassPermissionsModeAccepted": true,
            "mcpServers": {},
            "enableAllProjectMcpServers": false,
            "projects": { work.to_string_lossy(): {
                "hasTrustDialogAccepted": true,
                "projectOnboardingSeenCount": 1,
                "hasClaudeMdExternalIncludesApproved": false,
            }},
        });
        std::fs::write(config.join(".claude.json"), cfg.to_string()).ok()?;
        Some(Sandbox { root, config, work })
    }

    /// `claude` under the `env -i` isolation contract (no live HOME inherited).
    fn claude(&self, args: &[&str]) -> String {
        let out = self.claude_raw(args);
        assert!(
            out.status.success(),
            "claude {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Like [`claude`] but tolerates a non-zero exit (the in-flight resume probe).
    fn claude_raw(&self, args: &[&str]) -> std::process::Output {
        Command::new(CLAUDE_BIN)
            .args(args)
            .current_dir(&self.work)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("CLAUDE_CONFIG_DIR", &self.config)
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "dumb")
            .output()
            .expect("spawn claude")
    }

    /// Write a transcript file at `<slug>/<uuid>.jsonl` (the cwd-slug claude
    /// resolves `--resume` by) and return that uuid.
    fn place_transcript(&self, uuid: &str, records: &[Value]) {
        let dir = self.projects_slug_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{uuid}.jsonl")),
            dispatch::fork_seed::to_jsonl(records),
        )
        .unwrap();
    }

    fn projects_slug_dir(&self) -> PathBuf {
        // claude writes transcripts under <config>/projects/<cwd-slug>/.
        let slug = self.work.to_string_lossy().replace('/', "-");
        self.config.join("projects").join(slug)
    }

    // --- obl-2 real-in-flight-tail harness (race a live blocking tool + SIGKILL) ---

    fn marker_path(&self) -> PathBuf {
        self.work.join("INFLIGHT_MARKER")
    }
    fn fifo_path(&self) -> PathBuf {
        self.work.join("blockfifo")
    }

    /// Reset the in-flight-probe state: remove the marker, and (re)create the
    /// blocking FIFO with a FRESH inode (so a re-executed `cat <fifo>` blocks on a
    /// writer-less pipe again, and any prior reader is detached).
    fn reset_block_state(&self) {
        let _ = std::fs::remove_file(self.marker_path());
        let fifo = self.fifo_path();
        let _ = std::fs::remove_file(&fifo);
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(
            rc,
            0,
            "mkfifo {fifo:?} (errno {})",
            std::io::Error::last_os_error()
        );
    }

    /// `claude` spawned in its OWN process group (`process_group(0)` ⇒ pgid == the
    /// child's pid), stdio redirected to `<root>/<label>.{out,err}`, stdin from
    /// /dev/null. Returns the live child + its pgid; the caller SIGKILLs the whole
    /// group via [`killpg`] so the spawned blocking tool (`cat <fifo>`) dies WITH
    /// claude and never orphans (never the parent test — a distinct group).
    fn spawn_grouped(&self, label: &str, args: &[&str]) -> (std::process::Child, i32) {
        let out = std::fs::File::create(self.root.join(format!("{label}.out"))).unwrap();
        let err = std::fs::File::create(self.root.join(format!("{label}.err"))).unwrap();
        let child = Command::new(CLAUDE_BIN)
            .args(args)
            .current_dir(&self.work)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("CLAUDE_CONFIG_DIR", &self.config)
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "dumb")
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .expect("spawn claude");
        let pgid = child.id() as i32;
        (child, pgid)
    }

    /// WP-B7 carry-in (obl-2 hygiene): unblock a `cat <fifo>` grandchild that
    /// ESCAPED the process-group SIGKILL. `spawn_grouped` puts claude in its own
    /// pgid and `killpg` reaps the group, but claude's Bash tool can place the
    /// spawned command in a DIFFERENT process group — then the `cat` survives,
    /// FIFO-blocked forever on the (now-unlinked) inode. Opening the FIFO write-end
    /// NON-BLOCKING and closing it delivers EOF to any blocked reader so it exits;
    /// `O_NONBLOCK` makes the open return `ENXIO` (never hang) when there is no
    /// reader. MUST be called while THIS inode is still at `fifo_path()` — i.e.
    /// right after the `killpg`, before the next `place_blocking_fifo` re-mints it.
    fn unblock_fifo(&self) {
        let c = std::ffi::CString::new(self.fifo_path().to_string_lossy().as_bytes()).unwrap();
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
        if fd >= 0 {
            unsafe {
                libc::close(fd);
            }
        }
    }

    /// Capture a REAL claude-authored in-flight tail. Turn 1 is one COMPLETED turn
    /// (`end_turn`) — the safe boundary the guard retreats to. Turn 2 resumes it and
    /// elicits a genuine BLOCKING Bash `tool_use` (`printf RAN > marker && cat <fifo>`
    /// — a FIFO read blocks WITHOUT tripping claude's foreground-`sleep` guard); we
    /// poll the DURABLE transcript until it ends on an unanswered `tool_use`, then
    /// SIGKILL the whole group while the `tool_result` is NOT yet written. Returns
    /// the captured records.
    fn capture_in_flight(&self, u1: &str) -> Vec<Value> {
        // Turn 1 — a completed turn.
        let _ = self.claude(&[
            "--strict-mcp-config",
            "-p",
            "Reply with exactly: PARENT_OK",
            "--session-id",
            u1,
            "--output-format",
            "json",
        ]);
        // Turn 2 — resume + a real blocking tool_use, raced + SIGKILL'd mid-flight.
        self.reset_block_state();
        let cmd = format!(
            "printf RAN > {} && cat {}",
            self.marker_path().display(),
            self.fifo_path().display()
        );
        let prompt =
            format!("Use the Bash tool to run this exact command verbatim and nothing else: {cmd}");
        let (mut child, pgid) = self.spawn_grouped(
            "capture",
            &[
                "--strict-mcp-config",
                "--dangerously-skip-permissions",
                "--resume",
                u1,
                "--output-format",
                "stream-json",
                "--verbose",
                "-p",
                &prompt,
            ],
        );
        let xs = self.projects_slug_dir().join(format!("{u1}.jsonl"));
        let mut captured: Option<Vec<Value>> = None;
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(&xs) {
                let recs = dispatch::fork_seed::parse_records(&text);
                if ends_in_flight_tool_id(&recs).is_some() {
                    captured = Some(recs);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
        let _ = child.wait();
        // Reap any `cat <fifo>` grandchild that escaped the group SIGKILL, while
        // this inode is still linked at fifo_path() (obl-2 hygiene).
        self.unblock_fifo();
        captured.expect("captured a REAL in-flight tool_use tail (race+SIGKILL) before timeout")
    }

    /// Write `records` re-keyed to a fresh fork uuid into the cwd-slug dir (each
    /// resume probe gets its OWN source file so probes never mutate each other).
    fn place_rekeyed(&self, records: &[Value], new_uuid: &str) {
        let rekeyed: Vec<Value> = records
            .iter()
            .map(|r| {
                let mut r = r.clone();
                if let Some(o) = r.as_object_mut() {
                    if o.contains_key("sessionId") {
                        o.insert("sessionId".into(), Value::String(new_uuid.to_string()));
                    }
                }
                r
            })
            .collect();
        self.place_transcript(new_uuid, &rekeyed);
    }

    /// Resume `uuid` with `extra_args` and watch for auto-execution of the pending
    /// blocking tool. Polls (≤ `max_polls` × 300ms) for the marker OR child exit,
    /// then SIGKILLs the group. Returns `(marker_appeared, records_after)`.
    fn resume_probe(
        &self,
        uuid: &str,
        label: &str,
        extra_args: &[&str],
        max_polls: u32,
    ) -> (bool, usize) {
        self.reset_block_state();
        let mut args: Vec<&str> = vec![
            "--strict-mcp-config",
            "--dangerously-skip-permissions",
            "--resume",
            uuid,
        ];
        args.extend_from_slice(extra_args);
        let (mut child, pgid) = self.spawn_grouped(label, &args);
        let marker = self.marker_path();
        let mut appeared = false;
        for _ in 0..max_polls {
            if marker.exists() {
                appeared = true;
                break;
            }
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        appeared = appeared || marker.exists();
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
        let _ = child.wait();
        // Reap any `cat <fifo>` grandchild that escaped the group SIGKILL, while
        // this inode is still linked at fifo_path() (obl-2 hygiene).
        self.unblock_fifo();
        let after = std::fs::read_to_string(self.projects_slug_dir().join(format!("{uuid}.jsonl")))
            .map(|t| dispatch::fork_seed::parse_records(&t).len())
            .unwrap_or(0);
        (appeared, after)
    }
}

/// The id of the transcript's last `tool_use` block IF it is UNANSWERED (no later
/// `tool_result` references it) — i.e. the transcript ends mid-in-flight-tool.
/// `None` when the latest tool_use is resolved (or there is none): the source is
/// at rest. (Mirrors `dispatch::fork_seed`'s in-flight predicate, keyed on the id so the
/// test can assert the SAME id is dropped by the guard.)
fn ends_in_flight_tool_id(records: &[Value]) -> Option<String> {
    let mut last_tu: Option<String> = None;
    let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in records {
        if let Some(arr) = r
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            for x in arr {
                match x.get("type").and_then(|t| t.as_str()) {
                    Some("tool_use") => {
                        if let Some(id) = x.get("id").and_then(|i| i.as_str()) {
                            last_tu = Some(id.to_string());
                        }
                    }
                    Some("tool_result") => {
                        if let Some(id) = x.get("tool_use_id").and_then(|i| i.as_str()) {
                            answered.insert(id.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    last_tu.filter(|id| !answered.contains(id))
}

/// The `command` input of the `tool_use` block carrying `id`, for asserting the
/// captured tail is the REAL blocking Bash we elicited (not a synthetic stub).
fn tool_use_command(records: &[Value], id: &str) -> Option<String> {
    for r in records {
        let Some(arr) = r
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for x in arr {
            if x.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                && x.get("id").and_then(|i| i.as_str()) == Some(id)
            {
                return x
                    .get("input")
                    .and_then(|i| i.get("command"))
                    .and_then(|c| c.as_str())
                    .map(str::to_string);
            }
        }
    }
    None
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn session_id_of(result_json: &str) -> String {
    serde_json::from_str::<Value>(result_json)
        .ok()
        .and_then(|v| {
            v.get("session_id")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        })
        .expect("result JSON carries session_id")
}

fn is_message(r: &Value) -> bool {
    matches!(
        r.get("type").and_then(|t| t.as_str()),
        Some("user") | Some("assistant") | Some("attachment")
    )
}

/// The MESSAGE-DAG records (user/assistant/attachment — the load-bearing
/// conversation, per research S4.1 which keys equivalence on message records,
/// not the reorderable trailers), with `sessionId` REMOVED so the STRUCTURAL
/// comparison is independent of the (differing) new session id. Each record
/// keeps `type`/`uuid`/`parentUuid`/`message`, so a dropped / reordered /
/// content-mutated record IS caught — while a missed rekey is NOT (that is
/// checked separately by [`all_msg_session_ids_eq`], so the two teeth hit
/// distinct, non-overlapping predicates).
fn msg_dag_structure(records: &[Value]) -> Vec<Value> {
    records
        .iter()
        .filter(|r| is_message(r))
        .map(|r| {
            let mut r = r.clone();
            if let Some(o) = r.as_object_mut() {
                o.remove("sessionId");
            }
            r
        })
        .collect()
}

/// REKEY completeness: every message record carrying a `sessionId` has it equal
/// to `id`. A missed rekey (a record left on the parent id) makes this false.
fn all_msg_session_ids_eq(records: &[Value], id: &str) -> bool {
    records.iter().filter(|r| is_message(r)).all(|r| {
        r.get("sessionId")
            .and_then(|s| s.as_str())
            .map_or(true, |s| s == id)
    })
}

#[test]
#[ignore = "real-claude; run explicitly with --ignored"]
fn fidelity_gate_qd_seed_equals_native_fork_session() {
    let Some(qb) = Sandbox::create() else {
        eprintln!("SKIP fidelity_gate: claude binary or credentials absent");
        return;
    };
    let u1 = "11111111-1111-4111-8111-111111111111";

    // 1. Parent: one completed turn under a known session id.
    let _ = qb.claude(&[
        "--strict-mcp-config",
        "-p",
        "The secret word is BANANA. Reply with exactly: PARENT_OK",
        "--session-id",
        u1,
        "--output-format",
        "json",
    ]);
    let parent_path = qb.projects_slug_dir().join(format!("{u1}.jsonl"));
    let parent_text = std::fs::read_to_string(&parent_path).expect("parent transcript");
    let parent_md5_before = parent_text.clone();

    // 2. ORACLE — REAL native `--resume <u1> --fork-session` of the SAME parent.
    let native_result = qb.claude(&[
        "--strict-mcp-config",
        "-p",
        "Reply with exactly: FORK_OK",
        "--resume",
        u1,
        "--fork-session",
        "--output-format",
        "json",
    ]);
    let native_uuid = session_id_of(&native_result);
    assert_ne!(native_uuid, u1, "native fork mints a distinct id");
    let native_text =
        std::fs::read_to_string(qb.projects_slug_dir().join(format!("{native_uuid}.jsonl")))
            .expect("native fork transcript");

    // Parent left byte-untouched by the native fork (sanity on the oracle).
    assert_eq!(
        std::fs::read_to_string(&parent_path).unwrap(),
        parent_md5_before,
        "native fork must not mutate the parent"
    );

    // 3. qd's Mechanism-S seed of the SAME parent (the artifact under test).
    let parent_recs = dispatch::fork_seed::parse_records(&parent_text);
    let resolved =
        dispatch::fork_seed::resolve(&parent_recs, dispatch::fork_seed::ForkPoint::Latest)
            .expect("parent has a safe boundary");
    let seed_recs =
        dispatch::fork_seed::rekey_truncate(&parent_recs, resolved.boundary_idx, "FORK");

    // 4. STRUCTURAL EQUIVALENCE (sessionId removed): the qd seed's message DAG ==
    //    the native fork's COPIED message DAG. native = copy(parent) + the appended
    //    FORK_OK turn, so we take only as many message records as the parent had
    //    (the appended turn is excluded). FULL — no dropped/reordered/mutated record.
    let native_recs = dispatch::fork_seed::parse_records(&native_text);
    let seed_struct = msg_dag_structure(&seed_recs);
    let native_struct = msg_dag_structure(&native_recs);
    let parent_struct = msg_dag_structure(&parent_recs);
    assert!(
        !parent_struct.is_empty(),
        "vacuous guard: parent has no message records"
    );
    assert!(
        native_struct.len() >= parent_struct.len(),
        "native fork copied fewer message records than the parent: {} < {}",
        native_struct.len(),
        parent_struct.len()
    );
    let native_prefix = &native_struct[..parent_struct.len()];
    assert_eq!(
        seed_struct, *native_prefix,
        "FIDELITY: qd seed message DAG must equal native --fork-session's copied DAG"
    );
    assert_eq!(
        seed_struct, parent_struct,
        "qd seed is a faithful copy of the parent DAG (no drop/reorder/mutate)"
    );

    // 4b. REKEY completeness: every qd seed message record is re-keyed to the fork
    //     id; native re-keyed its copied DAG to ITS new id (the same faithful re-key).
    assert!(
        all_msg_session_ids_eq(&seed_recs, "FORK"),
        "every qd seed message record re-keyed to the fork id (no parent-id leak)"
    );
    let native_prefix_recs: Vec<Value> = native_recs
        .iter()
        .filter(|r| is_message(r))
        .take(parent_struct.len())
        .cloned()
        .collect();
    assert!(
        all_msg_session_ids_eq(&native_prefix_recs, &native_uuid),
        "native --fork-session re-keyed its copied DAG to its own new id"
    );

    // 5. TEETH — each hits a DISTINCT, non-overlapping predicate (proving the gate
    //    is non-vacuous, both ways):
    //  (a) missed rekey — one record left on the PARENT id → REKEY-completeness reds.
    let mut leaked = seed_recs.clone();
    leaked
        .iter_mut()
        .find(|r| is_message(r) && r.get("sessionId").is_some())
        .expect("a sessionId-bearing MESSAGE record")
        .as_object_mut()
        .unwrap()
        .insert("sessionId".into(), Value::String(u1.to_string()));
    assert!(
        !all_msg_session_ids_eq(&leaked, "FORK"),
        "TEETH: a missed rekey (parent-id leak) MUST red rekey-completeness"
    );
    //  (b) dropped record → STRUCTURAL equivalence reds.
    let dropped_struct: Vec<Value> = msg_dag_structure(&seed_recs).into_iter().skip(1).collect();
    assert_ne!(
        dropped_struct, *native_prefix,
        "TEETH: a dropped message record MUST red structural equivalence"
    );

    eprintln!(
        "FIDELITY GATE PASS: qd seed ({} msg records) == native --fork-session copied DAG \
         (full, no drop/reorder); both faithfully re-keyed; parent untouched; teeth (leak reds \
         rekey, drop reds structure) both fire.",
        seed_struct.len()
    );
}

#[test]
#[ignore = "real-claude; run explicitly with --ignored"]
fn obl2_in_flight_tool_guard_drops_unsafe_tail() {
    // WP-B5-iii obl-2: POSITIVELY reproduce that resuming a transcript ending on
    // an in-flight `tool_use` (no `tool_result`) auto-executes the pending tool
    // (the DANGER, FORK-IDENTITY-SPEC §5 end-state 3) — and prove the engine guard
    // (Mechanism-S `resolve(Latest)` retreats to the last completed turn) drops it,
    // so a fork comes up quiescent. RED (raw) → tool runs; GREEN (qd seed) → it does
    // not. Real claude, isolated HOME.
    let Some(qb) = Sandbox::create() else {
        eprintln!("SKIP obl2: claude binary or credentials absent");
        return;
    };
    let u1 = "21111111-1111-4111-8111-111111111111";
    let _ = qb.claude(&[
        "--strict-mcp-config",
        "-p",
        "Reply with exactly: PARENT_OK",
        "--session-id",
        u1,
        "--output-format",
        "json",
    ]);
    let parent_path = qb.projects_slug_dir().join(format!("{u1}.jsonl"));
    let parent_recs =
        dispatch::fork_seed::parse_records(&std::fs::read_to_string(&parent_path).unwrap());
    let last_uuid = parent_recs
        .iter()
        .rev()
        .find_map(|r| r.get("uuid").and_then(|u| u.as_str()).map(str::to_string))
        .expect("a parent record uuid");

    let marker = qb.work.join("INFLIGHT_MARKER");
    let cmd = format!("printf RAN > {}", marker.display());

    // Build an in-flight transcript: the parent (re-keyed to U_INF) + an UNANSWERED
    // assistant tool_use (Bash) as the last record.
    let u_inf = "2af1f1f1-1111-4111-8111-111111111111";
    let mut inflight =
        dispatch::fork_seed::rekey_truncate(&parent_recs, parent_recs.len() - 1, u_inf);
    inflight.push(serde_json::json!({
        "parentUuid": last_uuid,
        "isSidechain": false,
        "type": "assistant",
        "uuid": "aaaa1111-1111-4111-8111-1111111111ff",
        "sessionId": u_inf,
        "message": {
            "role": "assistant",
            "model": "claude-opus-4-8",
            "content": [{"type":"tool_use","id":"toolu_inflight1","name":"Bash","input":{"command": cmd}}],
            "stop_reason": "tool_use"
        }
    }));
    qb.place_transcript(u_inf, &inflight);

    // GUARD (deterministic): Mechanism-S resolve(Latest) on the in-flight transcript
    // RETREATS to the last completed turn (drops the tool_use tail) + reports §5a.
    let resolved =
        dispatch::fork_seed::resolve(&inflight, dispatch::fork_seed::ForkPoint::Latest).unwrap();
    assert!(
        resolved.boundary_idx < inflight.len() - 1,
        "guard: boundary must be BEFORE the in-flight tool_use tail"
    );
    let stale = resolved
        .staleness_report()
        .expect("staleness reported for in-flight tail");
    assert!(
        stale.contains("Bash"),
        "staleness names the in-flight tool: {stale}"
    );
    let u_seed = "2bf1f1f1-1111-4111-8111-111111111111";
    let seed = dispatch::fork_seed::rekey_truncate(&inflight, resolved.boundary_idx, u_seed);
    assert!(
        !dispatch::fork_seed::to_jsonl(&seed).contains("toolu_inflight1"),
        "guard: the qd seed must NOT contain the in-flight tool_use"
    );
    qb.place_transcript(u_seed, &seed);

    // GUARD (real claude, NON-VACUOUS): resuming the qd SEED runs the requested
    // turn (proves it genuinely resumed — SEED_OK in the result) and does NOT
    // auto-execute any pending tool (the in-flight tail was dropped → no marker).
    let _ = std::fs::remove_file(&marker);
    let seed_result = qb.claude(&[
        "--strict-mcp-config",
        "--dangerously-skip-permissions",
        "-p",
        "Reply with exactly: SEED_OK",
        "--resume",
        u_seed,
        "--output-format",
        "json",
    ]);
    assert!(
        seed_result.contains("SEED_OK"),
        "non-vacuous: the qd seed actually resumed (ran the turn): {seed_result}"
    );
    assert!(
        !marker.exists(),
        "GUARD: the qd seed (in-flight tail dropped) must NOT auto-execute the pending tool"
    );

    eprintln!(
        "obl-2 GUARD PASS: Mechanism-S retreated to the last completed turn (boundary {} < tail {}), \
         §5a staleness named the in-flight tool, the seed contains no tool_use, and the seed resumed \
         clean with NO auto-exec (marker absent, SEED_OK ran). The positive auto-exec repro of the RAW \
         in-flight tail (a synthetic tool_use under headless -p) did not trigger on claude 2.1.178 — \
         see obl-2 escalation; the guard makes the danger moot regardless.",
        resolved.boundary_idx, inflight.len() - 1
    );
}

#[test]
#[ignore = "real-claude; run explicitly with --ignored"]
fn obl2_real_in_flight_tail_no_silent_autoexec_guard_drops_it() {
    // WP-B5-iii obl-2 §7.2 — the POSITIVE-repro residual, done with a REAL
    // claude-authored in-flight tail (NOT a synthetic injected tool_use) and the
    // ACTUAL resume/auto-continue modes.
    //
    // FIRST-HAND FINDING (claude 2.1.178), which REFINES FORK-IDENTITY-SPEC §5
    // end-state 3 ("ends on a tool_use with no result → fork AUTO-executes the
    // pending tool → DANGER", a research-S4 / refusal-CONTRACT reading): claude
    // does NOT resume INTO a pending tool_use. On the GENUINE auto-continue modes
    // (stream-json EOF with no message; interactive PTY) it stays QUIESCENT and the
    // pending tool_use stays UNANSWERED — NO silent auto-exec. The pending tool runs
    // only as a RE-DECISION on an explicit new `-p` turn, and even then claude often
    // inspects + refuses a dangerous pending command (observed: it `ls`/`file`'d the
    // blocking FIFO and declined). So the danger surface is NARROWER than the spec
    // claim, and the engine's Mechanism-S guard is defense-in-depth.
    //
    // HARD-asserted (deterministic): the capture is a real unanswered tool_use; the
    // genuine auto-continue mode does NOT auto-exec; the engine GUARD on the SAME
    // source retreats + drops the tail + resumes clean. EXERCISED + logged (NOT
    // hard-asserted, since claude re-decides non-deterministically): the `-p
    // continue` re-decision path. See WP-B5iii-OBL2-FINDINGS.md.
    let Some(qb) = Sandbox::create() else {
        eprintln!("SKIP obl2-real: claude binary or credentials absent");
        return;
    };
    let u1 = "41111111-1111-4111-8111-111111111111";

    // 1. CAPTURE a REAL in-flight tail (race a live blocking Bash tool_use + SIGKILL).
    let captured = qb.capture_in_flight(u1);
    let tool_id = ends_in_flight_tool_id(&captured)
        .expect("the captured transcript ends on an UNANSWERED tool_use (real in-flight tail)");
    let cmd = tool_use_command(&captured, &tool_id)
        .expect("the in-flight tool_use carries a Bash command");
    assert!(
        cmd.contains("INFLIGHT_MARKER") && cmd.contains("cat "),
        "the captured tail is the REAL blocking Bash we elicited (not synthetic): {cmd}"
    );
    let end_turns = dispatch::fork_seed::end_turn_indices(&captured);
    assert!(
        !end_turns.is_empty() && *end_turns.last().unwrap() < captured.len() - 1,
        "capture has a PRIOR completed turn and the in-flight tail is AFTER it (idxs {end_turns:?} of {})",
        captured.len()
    );

    // 2. GENUINE AUTO-CONTINUE (stream-json EOF, no message) → NO silent auto-exec.
    //    The §7.2-(ii) safety result: the real provider resume of the real in-flight
    //    tail does NOT execute the pending tool.
    let u_eof = "42111111-1111-4111-8111-111111111111";
    qb.place_rekeyed(&captured, u_eof);
    let (eof_marker, eof_after) = qb.resume_probe(
        u_eof,
        "eof",
        &[
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
        ],
        40,
    );
    assert!(
        !eof_marker,
        "SAFETY (§7.2-ii): genuine auto-continue (stream-json EOF) must NOT auto-execute the pending tool"
    );
    // NON-VACUOUS: the session WAS loaded (records grew) yet the pending tool_use is
    // STILL unanswered — claude did not synthesize a tool_result / resume into it.
    assert!(
        eof_after >= captured.len(),
        "stream-json EOF resume genuinely loaded the session (non-vacuous): {eof_after} < {}",
        captured.len()
    );
    let eof_recs = dispatch::fork_seed::parse_records(
        &std::fs::read_to_string(qb.projects_slug_dir().join(format!("{u_eof}.jsonl"))).unwrap(),
    );
    assert!(
        ends_in_flight_tool_id(&eof_recs).is_some(),
        "the pending tool_use is STILL unanswered after the EOF resume (no auto-continue into it)"
    );

    // 3. RE-DECISION path (`-p continue` — the engine's `-p` launch family) —
    //    EXERCISED + reported, NOT hard-asserted: claude re-decides on a new turn
    //    (it may re-run the command OR inspect+refuse it). Documents that the danger
    //    is REACHABLE as a re-decision (it WAS reproduced in spikes), just not a
    //    silent auto-continue and not deterministic.
    let u_cont = "43111111-1111-4111-8111-111111111111";
    qb.place_rekeyed(&captured, u_cont);
    let (cont_marker, _) = qb.resume_probe(
        u_cont,
        "cont",
        &["-p", "continue", "--output-format", "json"],
        120,
    );
    eprintln!(
        "obl-2 RE-DECISION probe (`-p continue` on the REAL in-flight tail): the pending command {} \
         — a NEW-turn re-decision, NOT a silent auto-continue (non-deterministic across runs).",
        if cont_marker {
            "RAN (marker present → claude chose to re-execute)"
        } else {
            "did NOT run (marker absent → claude inspected/refused or ignored it)"
        }
    );

    // 4. ENGINE GUARD (Mechanism-S) on the SAME real source → retreats + drops the tail.
    let resolved = dispatch::fork_seed::resolve(&captured, dispatch::fork_seed::ForkPoint::Latest)
        .expect("guard resolves the real in-flight source to its prior safe boundary");
    assert!(
        resolved.boundary_idx < captured.len() - 1,
        "guard: the boundary is BEFORE the in-flight tail (idx {} of {})",
        resolved.boundary_idx,
        captured.len()
    );
    let stale = resolved
        .staleness_report()
        .expect("§5a staleness reported for the in-flight tail");
    assert!(
        stale.contains("Bash"),
        "§5a staleness names the in-flight tool: {stale}"
    );
    let u_seed = "44111111-1111-4111-8111-111111111111";
    let seed = dispatch::fork_seed::rekey_truncate(&captured, resolved.boundary_idx, u_seed);
    let seed_jsonl = dispatch::fork_seed::to_jsonl(&seed);
    assert!(
        !seed_jsonl.contains(&tool_id),
        "guard: the qd seed must NOT contain the in-flight tool_use id {tool_id}"
    );
    assert!(
        !seed_jsonl.contains("\"tool_use\""),
        "guard: the qd seed carries no tool_use block at all"
    );
    qb.place_transcript(u_seed, &seed);

    // 5. The GUARDED fork resumes CLEAN (non-vacuous, SEED_OK) with NO auto-exec —
    //    the pending tool never reached the fork. Uses the engine's `-p` launch path.
    let (seed_marker, _) = qb.resume_probe(
        u_seed,
        "seed",
        &[
            "-p",
            "Reply with exactly: SEED_OK",
            "--output-format",
            "json",
        ],
        40,
    );
    let seed_out = std::fs::read_to_string(qb.root.join("seed.out")).unwrap_or_default();
    assert!(
        seed_out.contains("SEED_OK"),
        "non-vacuous: the guarded seed actually resumed (ran the turn): {seed_out}"
    );
    assert!(
        !seed_marker,
        "GUARD: the guarded fork (in-flight tail dropped) does NOT auto-execute the pending tool"
    );

    eprintln!(
        "obl-2 PASS (real claude 2.1.178): captured a REAL in-flight tail (tool {tool_id}, cmd {cmd:?}); \
         genuine auto-continue (stream-json EOF) did NOT auto-exec and the pending tool stayed unanswered; \
         Mechanism-S retreated to boundary {} (< tail {}), §5a named Bash, the seed dropped the tool_use; \
         the guarded fork resumed clean (SEED_OK) with NO auto-exec. SAFETY FINDING: claude does not resume \
         INTO a pending tool_use; the guard is defense-in-depth (re-check on a claude version bump).",
        resolved.boundary_idx,
        captured.len() - 1
    );
}
