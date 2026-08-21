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
    assert!(text.contains("opencode sessions over ACP"), "{text}");
    // Not having a harness must never be what makes the run fail — check the
    // harness lines are FYI, not FAIL.
    for line in text.lines().filter(|l| l.contains("opencode")) {
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
