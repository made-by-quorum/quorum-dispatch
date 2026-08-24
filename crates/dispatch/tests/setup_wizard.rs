//! End-to-end `qd setup` against a THROWAWAY HOME (R15 + C2).
//!
//! Every test here builds a complete fake home under a `tempfile::TempDir` and
//! runs the real `qd` binary with `HOME` pointed at it. NOTHING in this file
//! may read or write the real `~` — that is the property the `qrm` tests being
//! ported from had, and it is what makes a first-run wizard safe to test at
//! all. Concretely, each spawn:
//!
//! - sets `HOME` to the temp dir,
//! - sets `QD_HOME` under it too, so the engine data dir cannot escape,
//! - sets `PATH` to a directory we control, so harness detection cannot see
//!   whatever the developer happens to have installed (the tests would
//!   otherwise pass or fail depending on whose laptop they run on),
//! - sets `QRM_RELAY_DISABLE_SCAN=1`, because `qd bootstrap`'s relay discovery
//!   port-scans localhost, which a temp HOME cannot sandbox.
//!
//! Stdin is `/dev/null` (a closed, non-TTY stdin), which is also the assertion
//! that a non-interactive run never hangs waiting for a prompt.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use dispatch::setup::harness::HarnessId;
use tempfile::TempDir;

fn qd_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_qd"))
}

/// A hermetic home: the temp dir, plus a `fakebin` we hand to `PATH` so no real
/// harness on the developer's machine is ever probed.
struct Jail {
    dir: TempDir,
}

impl Jail {
    fn new() -> Jail {
        let dir = tempfile::tempdir().expect("tempdir");
        let fakebin = dir.path().join("fakebin");
        std::fs::create_dir_all(&fakebin).unwrap();
        // `PATH` is the fakebin and NOTHING else, so no harness on the
        // developer's machine can be detected. But setup's PATH probe is
        // `sh -c 'command -v …'`, so `sh` itself has to be reachable — link it
        // in rather than widen PATH to /usr/bin, which would let a real
        // `codex`/`opencode` leak into the test.
        std::os::unix::fs::symlink("/bin/sh", fakebin.join("sh")).unwrap();
        Jail { dir }
    }

    fn home(&self) -> &Path {
        self.dir.path()
    }

    fn fakebin(&self) -> PathBuf {
        self.dir.path().join("fakebin")
    }

    /// Put an executable stub on the jail's PATH (a fake `codex`, `pi`, …).
    fn stub(&self, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = self.fakebin().join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&p, perm).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(qd_bin())
            .args(args)
            .env("HOME", self.home())
            .env("QD_HOME", self.home().join(".quorum/dispatch"))
            .env("PATH", self.fakebin())
            .env("SHELL", "/bin/zsh")
            .env("QRM_RELAY_DISABLE_SCAN", "1")
            .env_remove("QD_PI_BIN")
            .env_remove("NPM_CONFIG_PREFIX")
            .env_remove("ZDOTDIR")
            .stdin(Stdio::null())
            .output()
            .expect("spawn qd setup")
    }

    /// [`run`](Self::run) with NO arguments — the default action.
    ///
    /// Separate from `run` only so the bare-`qd` tests below read as what they
    /// are, and so `QD_TEST_NO_BARE_PROCS` can keep a real claude or codex on
    /// the developer's own machine from ever reaching a session gather. Nothing
    /// on this path performs one today — bare `qd` is pure text — but the guard
    /// costs nothing and these assertions must not become host-dependent if that
    /// changes.
    fn run_bare(&self) -> Output {
        self.run_bare_with(&[])
    }

    /// [`run_bare`](Self::run_bare) with named env vars put BACK.
    ///
    /// The jail scrubs `QD_PI_BIN` and `NPM_CONFIG_PREFIX` on every spawn, and
    /// has to: they are the two variables that decide where an off-`PATH` pi is
    /// looked for, so a developer who has either of them exported would
    /// otherwise get different answers from this suite than CI does. But they
    /// are also the exact inputs the C5 cases are ABOUT, so a few tests need to
    /// set them deliberately.
    ///
    /// Additive by construction — the scrub still runs, and these are applied
    /// after it. `run_bare(&[])` is byte-for-byte the environment every other
    /// test has always had, which is what keeps
    /// `a_pi_installed_off_path_is_found_and_reports_the_exact_qd_pi_bin_export`
    /// (whose whole premise is `QD_PI_BIN` being UNSET) honest.
    fn run_bare_with(&self, extra: &[(&str, &Path)]) -> Output {
        let mut cmd = Command::new(qd_bin());
        cmd.env("HOME", self.home())
            .env("QD_HOME", self.home().join(".quorum/dispatch"))
            .env("PATH", self.fakebin())
            .env("SHELL", "/bin/zsh")
            .env("QRM_RELAY_DISABLE_SCAN", "1")
            .env("QD_TEST_NO_BARE_PROCS", "1")
            .env_remove("QD_PI_BIN")
            .env_remove("NPM_CONFIG_PREFIX")
            .env_remove("ZDOTDIR")
            .stdin(Stdio::null());
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.output().expect("spawn bare qd")
    }

    /// Drop an executable `pi` stub at `rel` under the jail's HOME, and hand
    /// back its absolute path. Nothing here touches `PATH` — every caller is
    /// testing a pi that is deliberately NOT on it.
    fn pi_at(&self, rel: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = self.home().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "#!/bin/sh\necho 0.80.2\n").unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.home().join(rel)).ok()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

// ---------------------------------------------------------------------------
// detect-only
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_home_reports_what_is_missing_and_exits_non_zero() {
    let j = Jail::new();
    let out = j.run(&["setup"]);
    let text = stdout(&out);

    assert_eq!(out.status.code(), Some(1), "a fresh machine is not set up:\n{text}");
    assert!(text.contains("[setup]"), "{text}");
    assert!(text.contains("layout"), "{text}");
    assert!(text.contains("relay-pin"), "{text}");
    assert!(text.contains("INCOMPLETE"), "{text}");

    // DETECT-ONLY: a plain `qd setup` on a non-TTY changed nothing.
    assert!(!j.home().join(".quorum").exists(), "detect-only run created dirs");
    assert!(!j.home().join(".claude.json").exists(), "detect-only run wrote the pin");
    assert!(j.read(".zshrc").is_none(), "detect-only run touched the rc file");
    assert!(text.contains("non-interactive"), "must say why nothing happened:\n{text}");
}

#[test]
fn a_non_tty_run_never_hangs_waiting_for_a_prompt() {
    // The whole test suite would time out rather than fail if this regressed;
    // the assertion is that the process exited at all, promptly.
    let j = Jail::new();
    let out = j.run(&["setup"]);
    assert!(out.status.code().is_some(), "qd setup was killed by a signal");
}

// ---------------------------------------------------------------------------
// --json
// ---------------------------------------------------------------------------

#[test]
fn json_is_parseable_report_only_and_carries_the_detected_state() {
    let j = Jail::new();
    let out = j.run(&["setup", "--json"]);
    let text = stdout(&out);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!("`qd setup --json` did not emit valid JSON ({e}):\n{text}");
    });

    assert_eq!(v["ok"], serde_json::json!(false));
    assert_eq!(v["exit_code"], serde_json::json!(1));
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(v["relay_pin"]["state"], serde_json::json!("absent"));
    assert_eq!(
        v["layout"]["root"],
        serde_json::json!(j.home().join(".quorum").display().to_string())
    );

    // All four harnesses are reported, present or not (C2's question is "which
    // do you have", so an absent one still needs a row).
    let ids: Vec<String> = v["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["claude", "codex", "pi", "opencode"]);

    // Every check has a stable id + a status.
    for c in v["checks"].as_array().unwrap() {
        assert!(c["id"].is_string(), "{c}");
        assert!(c["status"].is_string(), "{c}");
    }

    // --json writes NOTHING.
    assert!(!j.home().join(".quorum").exists());
    assert!(!j.home().join(".claude.json").exists());
}

// ---------------------------------------------------------------------------
// --fix
// ---------------------------------------------------------------------------

#[test]
fn fix_creates_the_layout_pins_the_relay_and_wires_path() {
    let j = Jail::new();
    let out = j.run(&["setup", "--fix"]);
    let text = stdout(&out);

    // R15 item 1 — the directory structure.
    for d in [".quorum", ".quorum/bin", ".quorum/state"] {
        assert!(j.home().join(d).exists(), "missing {d}\n{text}");
    }
    // …and the engine dir, via `qd bootstrap` (never reimplemented here).
    assert!(j.home().join(".quorum/dispatch/state").exists(), "{text}");
    assert!(text.contains("[bootstrap]"), "setup must call into qd bootstrap:\n{text}");

    // R15 item 4 — the relay pin.
    let pin: serde_json::Value =
        serde_json::from_str(&j.read(".claude.json").expect("pin file")).unwrap();
    assert_eq!(pin["mcpServers"]["relay"]["command"], serde_json::json!("qd"));
    assert_eq!(pin["mcpServers"]["relay"]["args"], serde_json::json!(["relay:serve"]));
    assert_eq!(pin["mcpServers"]["relay"]["type"], serde_json::json!("stdio"));

    // R15 item 3 — the managed rc block.
    let rc = j.read(".zshrc").expect("rc file");
    assert!(rc.contains("# >>> qd setup >>>"), "{rc}");
    assert!(rc.contains("# <<< qd setup <<<"), "{rc}");
    assert!(rc.contains(&j.home().join(".quorum/bin").display().to_string()), "{rc}");
    // The retired baked `claude()` wrapper is NOT reintroduced (shell_init.rs).
    assert!(!rc.contains("claude()"), "{rc}");
}

#[test]
fn fix_is_idempotent_and_the_second_run_is_byte_identical() {
    let j = Jail::new();
    j.run(&["setup", "--fix"]);
    let rc_1 = j.read(".zshrc").unwrap();
    let pin_1 = j.read(".claude.json").unwrap();

    let out = j.run(&["setup", "--fix"]);
    assert_eq!(j.read(".zshrc").unwrap(), rc_1, "rc file changed on re-run");
    assert_eq!(j.read(".claude.json").unwrap(), pin_1, "pin changed on re-run");
    assert_eq!(
        rc_1.matches("# >>> qd setup >>>").count(),
        1,
        "managed block duplicated:\n{rc_1}"
    );
    // A second `--fix` on an already-wired home finds nothing left to fail.
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
}

#[test]
fn fix_leaves_a_wired_home_exiting_zero() {
    let j = Jail::new();
    let out = j.run(&["setup", "--fix"]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "everything fixable was fixed:\n{text}");
    assert!(text.contains("after fixes"), "{text}");
    assert!(text.contains("setup: OK"), "{text}");
}

#[test]
fn yes_applies_the_same_fixes_as_fix() {
    let j = Jail::new();
    let out = j.run(&["setup", "-y"]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(j.home().join(".claude.json").exists());
}

// ---------------------------------------------------------------------------
// The `~/.claude.json` guarantee: other keys and their ORDER survive.
// ---------------------------------------------------------------------------

#[test]
fn an_existing_claude_json_keeps_every_other_key_in_its_original_order() {
    let j = Jail::new();
    // A file shaped like the real one: several keys around mcpServers, with an
    // mcpServers entry that is not ours, and no relay entry at all (case 2).
    let before = r#"{
  "numStartups": 137,
  "installMethod": "brew",
  "autoUpdates": true,
  "mcpServers": {
    "othertool": {
      "type": "stdio",
      "command": "othertool",
      "args": ["serve"]
    }
  },
  "tipsHistory": {
    "new-user-warmup": 3
  },
  "projects": {
    "/work/a": { "allowedTools": [], "hasTrustDialogAccepted": true },
    "/work/b": { "allowedTools": ["Bash"] }
  },
  "lastReleaseNotesSeen": "2.1.0"
}
"#;
    std::fs::write(j.home().join(".claude.json"), before).unwrap();

    let out = j.run(&["setup", "--fix"]);
    let after = j.read(".claude.json").expect("pin file");

    // 1. Every top-level key survives, with its value.
    let a: serde_json::Value = serde_json::from_str(&after).unwrap();
    let b: serde_json::Value = serde_json::from_str(before).unwrap();
    for (k, v) in b.as_object().unwrap() {
        if k == "mcpServers" {
            continue; // the one object we edit, checked below
        }
        assert_eq!(a.get(k), Some(v), "key {k} was changed or dropped:\n{after}");
    }

    // 2. Top-level ORDER is unchanged, and `relay` was appended to mcpServers
    //    rather than reordering it.
    let order = |s: &str| -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(s).unwrap();
        v.as_object().unwrap().keys().cloned().collect()
    };
    assert_eq!(order(before), order(&after), "top-level key order changed:\n{after}");
    let mcp_keys: Vec<&String> = a["mcpServers"].as_object().unwrap().keys().collect();
    assert_eq!(
        mcp_keys,
        vec!["othertool", "relay"],
        "the unrelated server moved:\n{after}"
    );

    // 3. The unrelated server is untouched, and ours is correct.
    assert_eq!(a["mcpServers"]["othertool"], b["mcpServers"]["othertool"]);
    assert_eq!(a["mcpServers"]["relay"]["command"], serde_json::json!("qd"));
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
}

#[test]
fn an_unparsable_claude_json_is_reported_and_never_clobbered() {
    let j = Jail::new();
    let garbage = "{ this is not json at all\n";
    std::fs::write(j.home().join(".claude.json"), garbage).unwrap();

    let out = j.run(&["setup", "--fix"]);
    let text = stdout(&out);

    assert_eq!(
        j.read(".claude.json").as_deref(),
        Some(garbage),
        "setup rewrote a file it could not parse"
    );
    assert!(text.contains("not valid JSON"), "{text}");
    assert_eq!(out.status.code(), Some(1), "an unfixable required piece must exit 1");
}

#[test]
fn an_existing_relay_entry_is_repointed_without_disturbing_its_other_fields() {
    let j = Jail::new();
    std::fs::write(
        j.home().join(".claude.json"),
        r#"{"mcpServers":{"relay":{"type":"stdio","command":"/gone/bin/dispatch","args":["x"],"env":{"K":"V"}}},"numStartups":9}"#,
    )
    .unwrap();

    j.run(&["setup", "--fix"]);
    let a: serde_json::Value = serde_json::from_str(&j.read(".claude.json").unwrap()).unwrap();
    assert_eq!(a["mcpServers"]["relay"]["command"], serde_json::json!("qd"));
    assert_eq!(a["mcpServers"]["relay"]["args"], serde_json::json!(["relay:serve"]));
    assert_eq!(a["mcpServers"]["relay"]["env"], serde_json::json!({"K": "V"}));
    assert_eq!(a["numStartups"], serde_json::json!(9));
}

// ---------------------------------------------------------------------------
// Harness detection (C2 / C4) + the C5 finding.
// ---------------------------------------------------------------------------

#[test]
fn an_absent_harness_is_an_fyi_that_names_what_it_would_give_you() {
    let j = Jail::new(); // empty fakebin — nothing is installed
    let out = j.run(&["setup", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    for h in v["harnesses"].as_array().unwrap() {
        assert_eq!(h["found"], serde_json::json!(false), "{h}");
    }

    let text = stdout(&j.run(&["setup"]));
    // DERIVED, not copied: the display column and the prose are both cased by
    // `HarnessId::label`/`offers` now, and a hard-coded lowercase spelling here
    // would not just fail — the line FILTER below would silently match nothing
    // and pass vacuously.
    let label = HarnessId::Opencode.label();
    assert!(text.contains(HarnessId::Opencode.offers()), "{text}");
    // Not having a harness must never be what makes the run fail — check the
    // harness lines are FYI, not FAIL.
    let harness_lines: Vec<&str> = text.lines().filter(|l| l.contains(label)).collect();
    assert!(!harness_lines.is_empty(), "no {label} line at all in:\n{text}");
    for line in harness_lines {
        assert!(line.contains("fyi"), "{line}");
    }
}

#[test]
fn a_present_harness_is_detected_with_its_version() {
    let j = Jail::new();
    j.stub("opencode", "echo 'opencode 0.4.2'");
    j.stub("codex", "echo 'codex-cli 0.146.1'");

    let out = j.run(&["setup", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let by_id = |id: &str| {
        v["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == serde_json::json!(id))
            .unwrap()
            .clone()
    };

    let oc = by_id("opencode");
    assert_eq!(oc["found"], serde_json::json!(true));
    assert_eq!(oc["on_path"], serde_json::json!(true));
    assert!(oc["version"].as_str().unwrap().contains("0.4.2"), "{oc}");

    // codex reuses its OWN pin/drift logic — this stub reports the pinned
    // version, so the verdict is a match.
    let cx = by_id("codex");
    assert_eq!(cx["found"], serde_json::json!(true));
    assert_eq!(cx["pin_ok"], serde_json::json!(true), "{cx}");
}

#[test]
fn a_drifted_codex_warns_and_names_the_pin_without_failing_the_run() {
    let j = Jail::new();
    j.stub("codex", "echo 'codex-cli 0.999.0'");
    let out = j.run(&["setup", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let cx = v["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == serde_json::json!("codex"))
        .unwrap();
    assert_eq!(cx["pin_ok"], serde_json::json!(false), "{cx}");
    assert!(cx["pin_note"].as_str().unwrap().contains("BREAKING"), "{cx}");

    // Drift is a WARN: it must not be what makes the exit code non-zero.
    let checks = v["checks"].as_array().unwrap();
    let codex_check = checks
        .iter()
        .find(|c| c["id"] == serde_json::json!("harness.codex"))
        .unwrap();
    assert_eq!(codex_check["status"], serde_json::json!("warn"), "{codex_check}");
}

#[test]
fn a_pi_installed_off_path_is_found_and_reports_the_exact_qd_pi_bin_export() {
    // C5: `QD_PI_BIN` is never set and a bare `pi` misses the npm-global
    // install location. Setup finds it and prints the export.
    use std::os::unix::fs::PermissionsExt;
    let j = Jail::new();
    let npm_bin = j.home().join(".npm-pi-global/bin");
    std::fs::create_dir_all(&npm_bin).unwrap();
    let pi = npm_bin.join("pi");
    std::fs::write(&pi, "#!/bin/sh\necho 0.80.2\n").unwrap();
    let mut perm = std::fs::metadata(&pi).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&pi, perm).unwrap();

    let text = stdout(&j.run(&["setup"]));
    let expected_export = format!(r#"export QD_PI_BIN="{}""#, pi.display());
    assert!(text.contains(&expected_export), "missing the C5 export line:\n{text}");

    let v: serde_json::Value = serde_json::from_str(&stdout(&j.run(&["setup", "--json"]))).unwrap();
    let p = v["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == serde_json::json!("pi"))
        .unwrap();
    assert_eq!(p["found"], serde_json::json!(true), "{p}");
    assert_eq!(p["on_path"], serde_json::json!(false), "an off-PATH pi is not on PATH");
    assert_eq!(p["path"], serde_json::json!(pi.display().to_string()));
    // It reached the binary to version it, and it is the pinned one.
    assert_eq!(p["pin_ok"], serde_json::json!(true), "{p}");
}

// ---------------------------------------------------------------------------
// C17 — the qc plugin is detected, never installed.
// ---------------------------------------------------------------------------

#[test]
fn the_qc_plugin_is_only_ever_reported() {
    let j = Jail::new();
    let text = stdout(&j.run(&["setup", "--fix"]));
    assert!(text.contains("qc-plugin"), "{text}");
    assert!(text.contains("C17"), "the stop-point must be named:\n{text}");
    // Setup never creates the plugin registry or a plugin cache.
    assert!(
        !j.home().join(".claude/plugins").exists(),
        "setup installed a plugin it must not ship"
    );

    // With a registry present, it reads it instead of guessing.
    std::fs::create_dir_all(j.home().join(".claude/plugins")).unwrap();
    std::fs::write(
        j.home().join(".claude/plugins/installed_plugins.json"),
        r#"{"version":2,"plugins":{"charter@quorum":{"version":"0.1.0"}}}"#,
    )
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout(&j.run(&["setup", "--json"]))).unwrap();
    assert_eq!(v["qc_plugin_registered"], serde_json::json!(true));
}

// ---------------------------------------------------------------------------
// Bare `qd` — the default action.
//
// It prints the help, on every machine, set up or not. It ran `ls` historically,
// and briefly (R24) redirected into `qd setup` when the machine looked fresh —
// which meant bare `qd` did two different things depending on state the person
// could not see, and helped only someone whose machine was pristine. The brew
// formula points a new installer at bare `qd`, so what it prints has to be the
// same thing every time, and has to name `setup`.
// ---------------------------------------------------------------------------

/// A fresh HOME gets the help — not `ls`, and not an auto-run of setup.
#[test]
fn bare_qd_on_a_fresh_machine_prints_the_help() {
    let j = Jail::new();
    let out = j.run_bare();
    let so = stdout(&out);

    assert!(
        so.starts_with("Usage: qd [options] [command]"),
        "bare `qd` is the help: {so}"
    );
    assert!(
        !so.contains("[setup]"),
        "it must not RUN setup — arriving here is still install time, and C19 \
         ruled that writes need consent: {so}"
    );
    assert!(
        !so.contains("No sessions found"),
        "…and it is not the old `ls` default either: {so}"
    );
}

/// The help names `setup` and says what running it will do — the two facts that
/// decide whether a person is willing to run it on a machine they care about:
/// it writes nothing without `--fix`, and it is safe to re-run.
#[test]
fn the_help_says_what_setup_will_do() {
    let j = Jail::new();
    let so = stdout(&j.run_bare());

    assert!(so.contains("setup"), "the verb is listed: {so}");
    assert!(
        so.contains("Report-only by default"),
        "the help states the posture: {so}"
    );
    assert!(
        so.contains("qd setup --fix"),
        "…and names the form that actually applies the fixes: {so}"
    );
    assert!(
        so.contains("Safe to re-run"),
        "…and that re-running is safe: {so}"
    );
}

/// A machine WITH sessions gets the same help. The default no longer varies with
/// anything — no `~/.quorum` probe, no session count, nothing a person cannot
/// see from the command they typed.
#[test]
fn bare_qd_is_the_help_on_a_machine_with_sessions_too() {
    let j = Jail::new();
    let uuid = "c01d0001-aaaa-4aaa-8aaa-000000000001";
    let proj = j.home().join(".claude").join("projects").join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join(format!("{uuid}.jsonl")),
        format!(
            "{{\"type\":\"agent-name\",\"agentName\":\"keepme\"}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":\"hello\"}},\
             \"cwd\":\"/w\",\"sessionId\":\"{uuid}\"}}\n"
        ),
    )
    .unwrap();

    let so = stdout(&j.run_bare());
    assert!(
        so.starts_with("Usage: qd [options] [command]"),
        "same help, sessions or not: {so}"
    );
    assert!(
        !so.contains("keepme"),
        "bare `qd` does not list sessions any more — `qd ls` does: {so}"
    );
}

// ---------------------------------------------------------------------------
// FTUE punch R28 — the harness roster.
//
// Every surface that prints the top-level help (`qd --help`, `qd --help-all`,
// bare `qd`, and the tail of a COMPLETED `qd setup`) now ends with a block
// saying which harnesses are on THIS machine and whether qd can reach them.
// The tests below are e2e on purpose: `help_rows` is unit-tested pure in
// `setup::harness`, and what those unit tests cannot see is whether the roster
// a real process prints was probed from the real machine — which is the whole
// claim R28 makes. A static four-line block would pass every pure test in the
// tree and be a lie on three machines out of four.
//
// The heading is spelled out here rather than imported because it lives in a
// private const inside the `qd` BINARY (`help::HARNESS_HEADING`). That is the
// right level of coupling for an e2e test: it pins the bytes a human reads.
// ---------------------------------------------------------------------------

/// `help::HARNESS_HEADING`, as stdout.
const HARNESS_HEADING: &str = "Harnesses on this machine:";

/// The roster block's lines, RAW — padding and leading indent intact.
///
/// Separate from [`roster`] because the alignment IS a property on one of the
/// surfaces below: `--help-all` widens the verb table with long hidden-verb
/// signatures, and the roster must not move with it. A parsed `(label, state)`
/// pair has thrown away the only bytes that could show that.
fn roster_lines(text: &str) -> Vec<&str> {
    text.lines()
        .skip_while(|l| !l.contains(HARNESS_HEADING))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .collect()
}

/// The roster block, parsed back into `(label, state)` pairs.
///
/// Takes the lines after the heading up to the blank line that closes the
/// section, and splits each on the run of padding `push_section` inserts. Two
/// spaces is the minimum gap (the widest label gets exactly that), and no label
/// contains a double space, so the first `"  "` is always the column break.
///
/// Returns empty when there is no heading at all — which is how the
/// "no roster here" assertions below distinguish an absent block from an empty
/// one without matching on a harness name. Harness names appear in `qd setup`'s
/// own per-check rows too, and keying on one of those would make these tests
/// agree with each other by accident.
fn roster(text: &str) -> Vec<(String, String)> {
    roster_lines(text)
        .into_iter()
        .map(|l| {
            let (label, state) = l
                .trim()
                .split_once("  ")
                .unwrap_or_else(|| panic!("roster row has no column break: {l:?}"));
            (label.trim().to_string(), state.trim().to_string())
        })
        .collect()
}

/// Every harness gets a row, and on a machine with none of them installed every
/// row says so AND says what that harness would have given you.
///
/// The second half is the part worth pinning. "opencode — not installed" is a
/// fact about the reader's laptop that they already knew; the reason it is
/// worth four lines of a help screen is that it answers the question underneath
/// it — what am I not able to do — which is the question someone reading a help
/// screen for the first time actually has. A roster that degrades to a bare
/// absence list has lost the thing that justified printing it.
///
/// Derived from `HarnessId::ALL`, never hand-listed: a fifth harness added
/// there must appear here without anyone editing this test, and this test is
/// what proves the help walks the same list the rest of qd does.
#[test]
fn bare_qd_lists_every_harness_and_says_what_a_missing_one_would_give_you() {

    let j = Jail::new(); // empty fakebin — nothing is installed
    let so = stdout(&j.run_bare());

    assert!(so.contains(HARNESS_HEADING), "bare `qd` prints the roster:\n{so}");

    let rows = roster(&so);
    let labels: Vec<&str> = rows.iter().map(|(l, _)| l.as_str()).collect();
    let expected: Vec<&str> = HarnessId::ALL.iter().map(|id| id.label()).collect();
    assert_eq!(labels, expected, "a row per harness, in report order:\n{so}");

    for (id, (_, state)) in HarnessId::ALL.iter().zip(&rows) {
        assert!(
            state.starts_with("not installed"),
            "nothing is on this jail's PATH, so nothing may claim to be: {state}"
        );
        assert!(
            state.contains(id.offers()),
            "an absent harness's row has to say what you are missing: {state}"
        );
    }

    // The roster is a pointer to `qd setup`, so it carries the two facts that
    // decide whether a person is willing to run it (`SETUP_POSTURE`).
    assert!(so.contains("Report-only by default"), "{so}");
}

/// A harness that IS on this machine reports as configured — the assertion that
/// the roster is a probe and not a printed constant.
///
/// This is the only test in the file that can tell the difference. Every other
/// R28 assertion here passes just as well against a hard-coded four-line block,
/// because a jail's PATH is empty and "not installed" is what a constant would
/// say anyway. Putting one real executable on that PATH and requiring exactly
/// one row to change is what pins the block to the machine.
///
/// `opencode` is the harness stubbed because it needs no wiring at all (qd
/// spawns `opencode acp` per session), so "installed" and "configured" are the
/// same state for it and the assertion stays about detection rather than about
/// the wiring rules, which `readiness` unit-tests own.
#[test]
fn a_stubbed_harness_shows_up_in_the_roster_as_installed() {
    let j = Jail::new();
    j.stub("opencode", "echo 'opencode 0.4.2'");

    let so = stdout(&j.run_bare());
    let rows = roster(&so);
    let state = |label: &str| {
        rows.iter()
            .find(|(l, _)| l == label)
            .unwrap_or_else(|| panic!("no {label} row in:\n{so}"))
            .1
            .clone()
    };

    assert_eq!(
        state(HarnessId::Opencode.label()),
        "configured",
        "the stub is on PATH and opencode needs no wiring:\n{so}"
    );
    // …and ONLY that row moved. A block that said "configured" for everything
    // would pass the assertion above and be worse than no roster at all.
    assert!(
        state(HarnessId::Codex.label()).starts_with("not installed"),
        "an unstubbed harness must not be dragged along:\n{so}"
    );
}

/// A COMPLETED `qd setup` ends in the roster — after its own verdict, not
/// instead of it.
///
/// R23 made a finished setup end in the verb table, because the alternative was
/// dropping a human at a shell prompt having said nothing about what to type
/// there. R28 is the same argument one step further: the next thing that person
/// types is `qd start`, and the question that decides whether it works is which
/// harness they have. The rows scrolled past minutes ago under `--fix`'s
/// bootstrap output; this four-line block is what survives the scroll, so the
/// ORDER is load-bearing and asserted — a roster printed above the verdict
/// would be one more thing the verdict pushed off screen.
#[test]
fn a_completed_setup_ends_in_the_roster() {

    let j = Jail::new();
    let out = j.run(&["setup", "--fix"]);
    let so = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "this run has to be the clean one:\n{so}");

    let complete = so.find("Setup is complete").unwrap_or_else(|| {
        panic!("the R23 tail is what the roster hangs off:\n{so}");
    });
    let heading = so
        .find(HARNESS_HEADING)
        .unwrap_or_else(|| panic!("a finished setup names the harnesses:\n{so}"));
    assert!(
        heading > complete,
        "the roster follows the verdict, it does not push it up the scrollback:\n{so}"
    );

    let rows = roster(&so);
    let labels: Vec<&str> = rows.iter().map(|(l, _)| l.as_str()).collect();
    let expected: Vec<&str> = HarnessId::ALL.iter().map(|id| id.label()).collect();
    assert_eq!(labels, expected, "the tail carries the WHOLE roster:\n{so}");
}

/// `qd setup --json` prints the document and nothing else — no verb table, no
/// roster, no heading.
///
/// `json_is_parseable_report_only_and_carries_the_detected_state` already
/// parses this stdout, and a stray roster would red it as a parse failure. This
/// test exists anyway because that failure would arrive as "expected value at
/// line 1 column 1", which says nothing about the rule that was broken. R28
/// added a fourth print site for help text, and the rule it has to respect is
/// the one `--json` has always had: this stdout is a document, and prose in it
/// is corruption.
#[test]
fn json_setup_prints_no_roster_and_stays_one_document() {
    let j = Jail::new();
    j.stub("opencode", "echo 'opencode 0.4.2'"); // something to be tempted to list
    let so = stdout(&j.run(&["setup", "--json"]));

    assert!(
        !so.contains(HARNESS_HEADING),
        "the human roster has no place in a document:\n{so}"
    );
    assert!(!so.contains("Setup is complete"), "nor the R23 tail:\n{so}");
    assert!(!so.contains("Report-only by default"), "nor the posture line:\n{so}");

    // Exactly ONE document: `from_str` refuses trailing content, so this is the
    // assertion that nothing was appended after the JSON either.
    let v: serde_json::Value = serde_json::from_str(&so)
        .unwrap_or_else(|e| panic!("stdout is not a single JSON document ({e}):\n{so}"));
    // …and it is the real report, not an empty object that happens to parse.
    assert_eq!(v["harnesses"].as_array().map(Vec::len), Some(4), "{v}");
}

/// A FAILING `qd setup` keeps the last word. No roster.
///
/// The check that is still red and the remedy under it are the only two things
/// that reader needs, and every line printed after them is a line the terminal
/// scrolls them behind. `help_tail_follows` is unit-tested in the binary; this
/// pins that the process actually behaves that way, which is the part a unit
/// test of a two-clause predicate cannot reach.
///
/// Keyed on the HEADING, deliberately. `qd setup` always prints a per-check row
/// per harness — that is its job — so a harness NAME appears in this stdout on
/// every run and asserting on one would pass whether the tail printed or not.
/// The heading is the block.
#[test]
fn a_failing_setup_does_not_print_the_roster_tail() {
    let j = Jail::new();
    let out = j.run(&["setup"]);
    let so = stdout(&out);

    assert_eq!(out.status.code(), Some(1), "a fresh jail is not set up:\n{so}");
    assert!(
        !so.contains(HARNESS_HEADING),
        "a failing run's last words are its own:\n{so}"
    );
    assert!(!so.contains("Setup is complete"), "{so}");
    assert!(roster(&so).is_empty(), "{so}");
    // What it DOES end with: the verdict.
    assert!(so.contains("INCOMPLETE"), "{so}");
}

/// `qd --help` — the surface a person actually TYPES — prints the roster, and
/// prints it from this machine.
///
/// The other three R28 print sites are reached by accident or by ceremony: bare
/// `qd` is what the brew formula points a new installer at, and the setup tail
/// arrives once per machine. `--help` is the one someone types on purpose, on
/// the day the thing they expected to work did not, and it is the site whose
/// roster has to be true rather than decorative. The stub is what makes that
/// assertion — one harness on PATH, one row that changed, three that did not.
#[test]
fn help_prints_a_roster_probed_from_this_machine() {

    let j = Jail::new();
    j.stub("opencode", "echo 'opencode 0.4.2'");

    let out = j.run(&["--help"]);
    let so = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "`qd --help` is not a failure:\n{so}");
    assert!(so.contains(HARNESS_HEADING), "`qd --help` prints the roster:\n{so}");

    let rows = roster(&so);
    let labels: Vec<&str> = rows.iter().map(|(l, _)| l.as_str()).collect();
    let expected: Vec<&str> = HarnessId::ALL.iter().map(|id| id.label()).collect();
    assert_eq!(labels, expected, "a row per harness, in report order:\n{so}");

    let state = |label: &str| {
        rows.iter()
            .find(|(l, _)| l == label)
            .unwrap_or_else(|| panic!("no {label} row in:\n{so}"))
            .1
            .clone()
    };
    assert_eq!(
        state(HarnessId::Opencode.label()),
        "configured",
        "the stub is on PATH:\n{so}"
    );
    assert!(
        state(HarnessId::Codex.label()).starts_with("not installed"),
        "…and nothing else was dragged along with it:\n{so}"
    );
}

/// `qd --help-all` carries the roster AND its hidden-verb section, and the
/// roster is laid out identically to plain `qd --help`'s.
///
/// Two claims, and the second is the one with teeth. `--help-all` adds a
/// section of hidden verbs whose signatures are far longer than any human
/// verb's, which widens the ONE column `render_top` aligns the verb table to.
/// The roster deliberately computes its own width from the harness labels, so a
/// harness row must render byte-for-byte the same on both surfaces. Sharing the
/// table's width instead would be invisible on `--help` and would pad every
/// harness label halfway across the terminal here — a coupling where a verb
/// rename moves the harness column, which is exactly the kind of thing that
/// only shows up on the surface nobody screenshots.
///
/// The width check on `-V, --version` is what keeps the byte-equality from
/// being vacuous: it proves the two surfaces really do align their tables
/// differently, so "the roster did not move" is a fact about the roster rather
/// than about two identical renders.
#[test]
fn help_all_adds_the_hidden_verbs_without_moving_the_roster() {
    let j = Jail::new();
    j.stub("opencode", "echo 'opencode 0.4.2'");

    let help = stdout(&j.run(&["--help"]));
    let out = j.run(&["--help-all"]);
    let all = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{all}");

    // 1. The roster COMPOSES with `include_hidden` — it did not replace the
    //    section `--help-all` exists to print.
    assert!(all.contains(HARNESS_HEADING), "the roster is here too:\n{all}");
    assert!(
        all.contains("Hidden from `qd --help`"),
        "…and so is the surface --help-all is for:\n{all}"
    );

    // 2. The verb table really is wider here: the description column of a row
    //    that exists on BOTH surfaces has moved right.
    let desc_col = |text: &str| {
        let row = text
            .lines()
            .find(|l| l.trim_start().starts_with("-V, --version"))
            .unwrap_or_else(|| panic!("no version row in:\n{text}"));
        row.find("output the version number")
            .unwrap_or_else(|| panic!("no description in: {row:?}"))
    };
    assert!(
        desc_col(&all) > desc_col(&help),
        "the hidden verbs must widen the table, or claim 3 proves nothing \
         (--help {} vs --help-all {})",
        desc_col(&help),
        desc_col(&all)
    );

    // 3. …and the roster did NOT move with it. Same jail, same PATH, so every
    //    state string is the same; any difference here is alignment.
    assert_eq!(
        roster_lines(&help),
        roster_lines(&all),
        "the roster aligns to its own labels, not to the verb table:\n{all}"
    );
}

// ---------------------------------------------------------------------------
// The C5 pin, end to end — a verdict a person can act their way out of.
//
// The roster's whole claim is that it describes THIS machine. The way that
// claim fails in practice is not by being wrong about a harness nobody has; it
// is by being wrong about a harness someone has just finished installing, in
// the state `qd setup` itself told them to put it in. `readiness` used to check
// `OffPath` before `wired`, and `harness_presence`'s off-PATH sweep never
// consulted `QD_PI_BIN` at all, so a pi pinned exactly as instructed read
// "installed, not configured" — or, if the pin pointed anywhere the candidate
// list does not guess, "not installed" — on every `qd --help`, forever, with
// nothing the human could do about it.
//
// These three tests are the e2e floor under that fix. They are deliberately
// bare-`qd` tests rather than `qd setup --json` ones: the JSON carries
// `on_path`/`path`/`wired` as separate fields and a reader can reassemble the
// truth from them, but the roster has ONE word, and the bug was in the fold.
// ---------------------------------------------------------------------------

/// A pi that is off `PATH` and PINNED with `QD_PI_BIN` reads as configured —
/// wherever the pin points.
///
/// The stub goes somewhere `pi_candidates` will never guess (`somewhere-odd/`
/// is not an npm prefix, not `~/.npm-pi-global`, not `~/.local/bin`), which is
/// the point of having an override at all. That location is what makes this one
/// test cover BOTH halves of the bug: under the old code the sweep never read
/// `QD_PI_BIN`, so this row said "not installed" — and even with the sweep
/// fixed, `readiness` checking `OffPath` first would have made it "installed,
/// not configured". Only both fixes together produce "configured".
///
/// `configured` is asserted as an EXACT string, not a `contains`: "installed,
/// not configured — run `qd setup`" contains the word too, and a `contains`
/// here would have passed against the bug it exists to pin.
#[test]
fn an_off_path_pi_that_is_pinned_reads_as_configured() {
    let j = Jail::new();
    let pi = j.pi_at("somewhere-odd/pi");

    let so = stdout(&j.run_bare_with(&[("QD_PI_BIN", &pi)]));
    let rows = roster(&so);
    let state = rows
        .iter()
        .find(|(l, _)| l == HarnessId::Pi.label())
        .unwrap_or_else(|| panic!("no Pi row in:\n{so}"))
        .1
        .clone();

    assert_eq!(
        state, "configured",
        "a pi pinned exactly as `qd setup` instructs is CONFIGURED — the pin is \
         where the launch path looks first:\n{so}"
    );
}

/// A pi that is off `PATH` with NOTHING pointing at it is still "installed, not
/// configured" — the fix did not just make every row say configured.
///
/// This is the state the C5 export line exists for: qd found a pi, qd will not
/// run that pi (a bare `pi` misses it), and the human has to act. Discovered
/// here through `NPM_CONFIG_PREFIX`, which is the ordinary npm-global shape
/// rather than quorum's own provisioning location, so the case is the one a
/// real user arrives in.
#[test]
fn an_off_path_pi_with_nothing_pointing_at_it_is_unconfigured() {
    let j = Jail::new();
    let prefix = j.home().join("npm-prefix");
    j.pi_at("npm-prefix/bin/pi");

    let so = stdout(&j.run_bare_with(&[("NPM_CONFIG_PREFIX", &prefix)]));
    let rows = roster(&so);
    let state = rows
        .iter()
        .find(|(l, _)| l == HarnessId::Pi.label())
        .unwrap_or_else(|| panic!("no Pi row in:\n{so}"))
        .1
        .clone();

    assert_eq!(
        state, "installed, not configured — run `qd setup`",
        "found, but qd will not reach it until the human acts:\n{so}"
    );
}

/// The two states above are DIFFERENT rows.
///
/// That difference is the entire bug. Before the fix both spellings of "pi
/// lives off PATH" collapsed to one string, so the roster could not tell a
/// person who had finished the job from a person who had not — and told the
/// first of them, wrongly, that they still had work to do.
///
/// Asserted as its own test rather than left implicit in the two above because
/// this is the property that survives a rewording: change what either state
/// says and this still holds, delete the distinction and only this reds.
#[test]
fn a_pinned_pi_and_a_stranded_pi_do_not_read_the_same() {
    let pi_row = |so: &str| {
        roster(so)
            .into_iter()
            .find(|(l, _)| l == HarnessId::Pi.label())
            .unwrap_or_else(|| panic!("no Pi row in:\n{so}"))
            .1
    };

    let jp = Jail::new();
    let pinned_bin = jp.pi_at("somewhere-odd/pi");
    let pinned = pi_row(&stdout(&jp.run_bare_with(&[("QD_PI_BIN", &pinned_bin)])));

    let js = Jail::new();
    let prefix = js.home().join("npm-prefix");
    js.pi_at("npm-prefix/bin/pi");
    let stranded = pi_row(&stdout(&js.run_bare_with(&[("NPM_CONFIG_PREFIX", &prefix)])));

    assert_ne!(
        pinned, stranded,
        "a pi the human has already wired must not read the same as one they \
         have not — that is the verdict nobody could act their way out of"
    );
}
