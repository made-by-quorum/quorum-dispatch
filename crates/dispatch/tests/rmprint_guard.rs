//! Effort A / Reading B — the "no dispatch-spawned `claude --print`" door-can't-
//! reopen matrix (atomic2-impl-plan.md §5/§6/§7).
//!
//! Closes the 4 forbidden flags F = { -p, --print, --output-format, --input-format }
//! (and their value/attached/cluster forms) on the dispatch-spawn surfaces via the
//! ONE shared cluster-aware matcher in `launch.rs`:
//!   - E2 (`lifecycle.rs run_start`)   — REFUSE, loud teaching error, nothing spawned.
//!   - CF (`launch.rs claude_flags`)    — STRIP+warn (unit-tested there; argv golden here).
//!
//! H9 (`daemon_headless.rs resolve`) RETIRED with the P4DB drive-burn — its file is
//! deleted because the `LaunchHeadless` wire verb it backstopped is burned (A2 §4/§8).
//! E2 + CF cover every surviving spawn surface (interactive create + revive).
//!
//! This file holds: the E2 behavioral surface matrix (driving the REAL `qd` bin
//! against a jailed HOME — refusal is PRE-spawn so no real claude is needed), the CF
//! argv golden through the real `ClaudeProvider` seam, the §5 structural source-canary,
//! and the §6 live-claude grammar re-confirm (`#[ignore]`, skips if claude is absent).
//!
//! Jailed-test note: refusal fires before any mint/claim/spawn, so the assertions are
//! over the jailed session-dir (must stay empty) + this process's own exit code +
//! stderr — never the host process table.

use std::path::{Path, PathBuf};
use std::process::Command;

fn qd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_qd")
}

/// A minimal jail: a tempdir HOME with a `.claude/sessions` dir and a no-op
/// CLAUDE_BIN, so a benign launch never execs the real claude and an E2 refusal
/// can be proven to mint NOTHING.
struct Jail {
    _root: tempfile::TempDir,
    home: PathBuf,
    xdg: PathBuf,
    fake_claude: PathBuf,
}

fn jail() -> Jail {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let xdg = root.path().join("x");
    std::fs::create_dir_all(home.join(".claude").join("sessions")).unwrap();
    std::fs::create_dir_all(home.join(".claude").join("projects")).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    if let Ok(real) = std::env::var("HOME") {
        assert_ne!(home, PathBuf::from(real), "test home must never equal real HOME");
    }
    let fake_claude = root.path().join("fake_claude.sh");
    std::fs::write(&fake_claude, "#!/bin/bash\nexit 0\n").unwrap();
    std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    Jail {
        _root: root,
        home,
        xdg,
        fake_claude,
    }
}

impl Jail {
    fn sessions_dir(&self) -> PathBuf {
        self.home.join(".claude").join("sessions")
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(qd_bin())
            .args(args)
            .current_dir(&self.home)
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", &self.xdg)
            .env("CLAUDE_BIN", &self.fake_claude)
            .env_remove("QD_HOME")
            .env_remove("QD_MUX")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .output()
            .expect("spawn qd")
    }

    /// No `<pid>.json` registry row was minted (E2 refuses before mint/claim/spawn).
    fn minted_nothing(&self) -> bool {
        let dir = self.sessions_dir();
        std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten().all(|e| {
                    e.path().extension().and_then(|s| s.to_str()) != Some("json")
                })
            })
            .unwrap_or(true)
    }
}

/// The E2 teaching-error fingerprint (atomic2-impl-plan.md §2.1).
const REFUSE_FINGERPRINT: &str = "cannot forward";

/// E2 REFUSE — agent/headless lane. The trailing `-- <F>` routes into claudeArgs;
/// qd's own `-p hi` (the prompt) is untouched (the two-`-p` trap boundary).
#[test]
fn e2_agent_headless_lane_refuses_forbidden() {
    for fvar in ["--print", "--output-format", "--input-format", "-p", "-cp", "--output-format=json"] {
        let j = jail();
        let out = j.run(&["start", "wk", "--headless", "-p", "hi", "--", fvar]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(1),
            "agent-lane {fvar}: E2 must exit 1; stderr={err}"
        );
        assert!(
            err.contains(REFUSE_FINGERPRINT),
            "agent-lane {fvar}: teaching error expected; stderr={err}"
        );
        assert!(j.minted_nothing(), "agent-lane {fvar}: nothing must be minted");
    }
}

/// E2 REFUSE — human/interactive lane (no `-p`). E2 fires before the driver
/// auto-detect / RefuseNoPrompt, so it refuses regardless of TTY.
#[test]
fn e2_human_lane_refuses_forbidden() {
    for fvar in ["--print", "-p", "-vp"] {
        let j = jail();
        let out = j.run(&["start", "wk", "--", fvar]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "human-lane {fvar}; stderr={err}");
        assert!(
            err.contains(REFUSE_FINGERPRINT),
            "human-lane {fvar}: teaching error expected; stderr={err}"
        );
        assert!(j.minted_nothing(), "human-lane {fvar}: nothing minted");
    }
}

/// E2 REFUSE — codex / acp providers, UNIFORM. E2 sits before the codex/acp early
/// returns and before provider validation, so a forbidden flag refuses identically.
#[test]
fn e2_codex_and_acp_refuse_uniformly() {
    for provider in ["codex", "acp/claude-code"] {
        let j = jail();
        let out = j.run(&["start", "wk", "--provider", provider, "-p", "hi", "--", "--print"]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{provider}: E2 must refuse uniformly; stderr={err}"
        );
        assert!(
            err.contains(REFUSE_FINGERPRINT),
            "{provider}: teaching error expected; stderr={err}"
        );
        assert!(j.minted_nothing(), "{provider}: nothing minted");
    }
}

/// E2 teaching-error TEXT — the exact §2.1 wording (the two-`-p` disambiguation +
/// the two working re-entry recipes).
#[test]
fn e2_teaching_error_text() {
    let j = jail();
    let out = j.run(&["start", "wk", "--headless", "-p", "hi", "--", "--print"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot forward \"--print\""), "names the flag: {err}");
    assert!(err.contains("dispatch only creates tracked, attachable sessions"), "{err}");
    assert!(
        err.contains("-p/--print/--output-format/--input-format would make a one-off headless run"),
        "{err}"
    );
    assert!(err.contains("For a one-off print run: use `claude -p"), "{err}");
    assert!(
        err.contains("For a tracked agent turn: `qd start <name> -p <prompt>`"),
        "{err}"
    );
    assert!(err.contains("that -p is qd's prompt flag, not claude's"), "{err}");
}

/// E2 NEGATIVE — qd's own `-p <prompt>` is NOT refused (the two-`-p` trap): a clean
/// `qd start wk -p hi` (no trailing forbidden) must NOT print the teaching error.
#[test]
fn e2_qd_prompt_flag_not_refused() {
    let j = jail();
    // A benign trailing arg keeps the claudeArgs vector clean of F.
    let out = j.run(&["start", "wk", "--headless", "-p", "hi", "--", "--add-dir", "/x"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains(REFUSE_FINGERPRINT),
        "qd's own -p prompt + benign passthrough must NOT trip E2; stderr={err}"
    );
}

/// E2 NEGATIVE — a session-name payload carrying a lowercase `p` (`-nProp`) is NOT
/// refused (the `-n` required value swallows the rest).
#[test]
fn e2_name_payload_not_refused() {
    for name_tok in ["-nProp", "-nmyProp"] {
        let j = jail();
        let out = j.run(&["start", "wk", "--headless", "-p", "hi", "--", name_tok]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            !err.contains(REFUSE_FINGERPRINT),
            "{name_tok}: a name with a 'p' must NOT trip E2; stderr={err}"
        );
    }
}

/// E2 OVER-REFUSAL (Minor #1, documented SAFE): a VALUE token that itself starts
/// `-p`/`--print` (a `-profile.json` path value) trips E2 — the matcher scans
/// tokens, not flag-vs-value position. Over-refusal in the safe direction; pinned.
#[test]
fn e2_value_position_over_refuses_documented() {
    let j = jail();
    let out = j.run(&["start", "wk", "--headless", "-p", "hi", "--", "--mcp-config", "-profile.json"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "value-pos over-refuse; stderr={err}");
    assert!(err.contains(REFUSE_FINGERPRINT), "value-pos over-refuse teaches; stderr={err}");
}

// --- CF argv golden (the operator base-flags channel) ---------------------------

/// CF STRIP at the argv level through the REAL `ClaudeProvider` seam (the interactive
/// + revive lane consumer of `claude_flags`). With a forbidden flag in
/// `QD_CLAUDE_FLAGS`, the resolved launch argv must NOT carry F — yet the session
/// still launches (a non-empty argv is produced). The DEFAULT posture flags survive.
#[test]
fn cf_argv_golden_strips_forbidden_from_env() {
    use dispatch::effects::MapEnv;
    use dispatch::paths::QdPaths;
    use dispatch::provider::{ClaudeProvider, LaunchRequest, Provider, ProviderFx};

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let paths = QdPaths::from_home(&home);
    let env = MapEnv {
        vars: [(
            "QD_CLAUDE_FLAGS".to_string(),
            "--dangerously-skip-permissions --output-format json --print --add-dir /x".to_string(),
        )]
        .into_iter()
        .collect(),
        uid: 501,
    };
    let fx = ProviderFx {
        await_relay: None,
        env: &env,
        paths: &paths,
        socket_dir: home.join("zmx-501"),
        mux: None,
        clock: None,
        sleeper: None,
        relay: None,
        relay_port: None,
        app_server: None,
        codex_expected_turn_id: None,
        acp_client: None,
        pi_rpc: None,
            acp_pre_dispatch: None,
    };
    let req = LaunchRequest {
        name: "wk".to_string(),
        cwd: Some("/work".to_string()),
        resume: None,
        fork: false,
        agent: None,
        model: None,
        passthrough: vec![],
        interactive: false,
        control_socket: None,
    };
    let plan = ClaudeProvider.launch_plan(&fx, &req);

    // F never reaches argv (the unconditional CF security property).
    assert!(
        !plan.argv.iter().any(|a| a == "--print" || a == "--output-format"),
        "CF must strip F from the operator base flags; argv={:?}",
        plan.argv
    );
    // The benign operator flag survives, and the session still launches.
    assert!(
        plan.argv.iter().any(|a| a == "--dangerously-skip-permissions"),
        "benign operator flag survives; argv={:?}",
        plan.argv
    );
    assert!(
        plan.argv.iter().any(|a| a == "--add-dir"),
        "benign operator flag survives; argv={:?}",
        plan.argv
    );
    assert!(!plan.argv.is_empty(), "session still launches (non-empty argv)");
}

// --- §5 structural source-canary (regression-pin on the 3 forward/source sites) -

fn crate_src(rel: &str) -> String {
    // qd/qw split: some canaried sources now live in the sibling `quorum-qw`
    // crate, so `rel` may reach across with `../quorum-qw/…`. The guard still
    // anchors on THIS crate's manifest dir; only the relative path changed.
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {p:?}: {e}"))
}

/// §5 layer-2: each SURVIVING forward/source site references the SHARED predicate,
/// positioned at the gate. After the P4DB drive-burn the two surviving sites are E2
/// (`lifecycle.rs run_start`) and CF (`launch.rs claude_flags`); H9
/// (`daemon_headless.rs resolve`) retired with its deleted file (A2 §4/§8). A
/// refactor that moves/drops a surviving guard REDs here (structural) AND the
/// behavioral matrix above REDs (belt-and-suspenders).
#[test]
fn structural_canary_surviving_sites_reference_shared_predicate() {
    // E2: lifecycle.rs run_start — `forbidden_in(` between the claudeArgs parse and
    // the provider split.
    let life = crate_src("src/bin/qd/verbs/lifecycle.rs");
    let parse_at = life
        .find(r#".get_many::<String>("claudeArgs")"#)
        .expect("the claudeArgs parse site");
    let split_at = life
        .find(r#"provider_impl.id().starts_with("acp/")"#)
        .expect("the provider/lane split (acp early-return)");
    let guard_at = life
        .find("forbidden_in(")
        .expect("lifecycle.rs run_start must reference the shared forbidden_in predicate");
    assert!(
        parse_at < guard_at && guard_at < split_at,
        "E2 guard must sit between the claudeArgs parse and the provider split"
    );

    // H9 (daemon_headless.rs resolve) is RETIRED with the P4DB drive-burn: the
    // `DaemonHeadlessFactory` file is deleted because the `LaunchHeadless` wire verb
    // it backstopped is itself burned (no `LaunchHeadless { claude_args }` frame can
    // reach a `resolve()` anymore). E2 + CF still cover every SURVIVING spawn surface
    // (interactive create + revive), so there is nothing left for H9 to guard — and
    // no `src/daemon_headless.rs` source to canary. See A2 §4 (H9 retires with its
    // file) / §8 (the H9 framed-error test moves to the delete class).

    // CF: launch.rs claude_flags — `strip_forbidden(` before the return, inside the
    // claude_flags body (above the split-out resolver).
    let lf = crate_src("../quorum-qw/src/launch.rs");
    let cf_at = lf
        .find("pub fn claude_flags(")
        .expect("the claude_flags resolver");
    let resolver_at = lf
        .find("fn resolve_claude_flags(")
        .expect("the split-out base resolver marks the end of claude_flags' body");
    assert!(cf_at < resolver_at, "claude_flags precedes its split-out resolver");
    // Scope to the claude_flags BODY (between its `fn` and the split-out resolver) so
    // the `strip_forbidden` DEFINITION earlier in the file isn't what we match.
    let cf_body = &lf[cf_at..resolver_at];
    assert!(
        cf_body.contains("strip_forbidden("),
        "CF: claude_flags' body must call strip_forbidden before returning"
    );
}

/// §5 fourth bullet: the interactive PTY appender (`build_new_extra_args`) needs no
/// OWN guard — its passthrough producer is E2 (same process, no wire bypass) and its
/// base flags come from the CF-guarded `claude_flags`. Pin that invariant: the
/// appender does NOT itself reference the predicate (it relies on the upstream
/// guards), and its sole production caller (`provider.rs launch_plan`) resolves the
/// base `flags` via `claude_flags` (the CF strip). A refactor that fed the appender
/// from an unguarded base reds here.
#[test]
fn structural_canary_interactive_appender_base_flags_are_cf_guarded() {
    let lf = crate_src("../quorum-qw/src/launch.rs");
    // The appender body (between its `fn` and the next `pub fn`) carries NO own
    // matcher reference — it is guarded transitively by E2 (passthrough) + CF (base).
    let app_at = lf
        .find("pub fn build_new_extra_args(")
        .expect("the interactive appender");
    let after = &lf[app_at..];
    let body_end = after[1..]
        .find("\npub fn ")
        .map(|i| i + 1)
        .unwrap_or(after.len());
    let app_body = &after[..body_end];
    assert!(
        !app_body.contains("forbidden_in(") && !app_body.contains("strip_forbidden("),
        "the interactive appender must rely on upstream E2/CF, not carry its own guard"
    );
    // Its production caller resolves base flags through the CF-guarded resolver.
    let prov = crate_src("../quorum-qw/src/provider.rs");
    let plan_at = prov.find("fn launch_plan(").expect("ClaudeProvider launch_plan");
    let plan_tail = &prov[plan_at..];
    let claude_flags_at = plan_tail
        .find("claude_flags(")
        .expect("launch_plan resolves base flags via the CF-guarded claude_flags");
    let appender_at = plan_tail
        .find("build_new_extra_args(")
        .expect("launch_plan feeds the appender");
    assert!(
        claude_flags_at < appender_at,
        "the CF-guarded base flags must be resolved before the appender consumes them"
    );
}

/// §5 layer-1: there is exactly ONE home for the forbidden set (`FORBIDDEN_LONG` in
/// launch.rs). A second, drifting copy anywhere in the crate is the smell the guard
/// forbids.
#[test]
fn structural_canary_single_home_for_forbidden_set() {
    let lf = crate_src("../quorum-qw/src/launch.rs");
    let defs = lf.matches("const FORBIDDEN_LONG").count();
    assert_eq!(defs, 1, "exactly one FORBIDDEN_LONG definition (single source of truth)");
}

// --- §6 LIVE-CLAUDE grammar re-confirm (guards the §3.2 matcher drift risk) ------

fn real_claude_bin() -> Option<PathBuf> {
    if let Ok(b) = std::env::var("CLAUDE_BIN") {
        let p = PathBuf::from(&b);
        if p.is_file() {
            return Some(p);
        }
    }
    let default = PathBuf::from("/home/u/.local/bin/claude");
    if default.is_file() {
        return Some(default);
    }
    // PATH lookup.
    if let Ok(out) = Command::new("which").arg("claude").output() {
        if out.status.success() {
            let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// LIVE re-confirm (gated like the other live-claude tests: `#[ignore]`, and a
/// graceful skip when no claude binary is reachable): the matcher's load-bearing
/// grammar facts still hold on the build's claude — `-n` CONSUMES the rest of its
/// token (so `-nph` is parsed as `--name "ph"`, NOT `--name + --print`), and a bare
/// `-p` cluster reaches `--print`. If claude's grammar drifts, this REDs before the
/// matcher silently goes fail-open.
#[test]
#[ignore = "needs a live claude binary; run with --ignored --nocapture"]
fn live_claude_grammar_reconfirm() {
    let Some(claude) = real_claude_bin() else {
        eprintln!("live_claude_grammar_reconfirm: no claude binary reachable — SKIPPED");
        return;
    };

    // `-n` is a REQUIRED-value flag that consumes the rest of the cluster as the
    // NAME. So `-nph` => name = "ph"; claude must NOT treat the `p` as `--print`.
    // We probe with `-nph --help`: claude consumes "ph" as the name and then shows
    // help (no print-mode error/behavior). The load-bearing fact is that `-n`
    // swallows the trailing chars; if claude made `-n` boolean, `-nph` would reach
    // `--print` and the matcher (stopping at `n`) would MISS it.
    let help = Command::new(&claude).arg("--help").output().expect("claude --help");
    let help_txt = format!(
        "{}{}",
        String::from_utf8_lossy(&help.stdout),
        String::from_utf8_lossy(&help.stderr)
    );
    // The grammar facts the matcher pins: --name takes a value, --print exists.
    assert!(
        help_txt.contains("--name") && help_txt.contains("--print"),
        "claude --help must still document --name (value) and --print; got:\n{help_txt}"
    );
    // --name must still advertise a required value (a `<...>` placeholder after -n,
    // --name). If it ever becomes a bare boolean, the `-n stops the walk` rule in
    // the matcher would let `-np` slip — this is the drift tripwire.
    assert!(
        help_txt.contains("--name <") || help_txt.contains("--name <name>") || help_txt.contains("-n, --name"),
        "claude --name must still take a value (the matcher's -n-consumes rule); got:\n{help_txt}"
    );
}
