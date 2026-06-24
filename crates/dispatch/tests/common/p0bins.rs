//! Shared P0 jail-test scaffolding (spec-w9-simplify S3): the built-binary
//! locators + the fakerepl jail-belt dir scaffold + the jailed `sb` runner
//! shared by `p0_id_matrix.rs` and `p0_qafix.rs` ONLY. The pre-existing gated
//! suites (ack2_gate / ack3_* / c1_gate / …) keep their sanctioned inline
//! copies — do NOT point them here.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// The REAL `sb` binary under test.
pub fn sb_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dispatch")
}

/// `<target>/<profile>` from the running test exe (`.../deps/<testbin>`).
pub fn profile_dir() -> PathBuf {
    std::env::current_exe()
        .expect("current_exe")
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>")
        .to_path_buf()
}

/// The built `sbmux` binary (embedded backend). PANICS with a build hint if
/// absent — never a silent skip (c1_gate contract).
pub fn sbmux_bin() -> PathBuf {
    let bin = profile_dir().join("sbmux");
    assert!(
        bin.exists(),
        "sbmux binary not found at {bin:?} — build it first: \
         scripts/build-lock.sh cargo build -p sbmux --bin sbmux"
    );
    bin
}

/// The built `fakerepl` binary with the fakerepl_gate STALENESS GUARD; missing
/// → build-once. NEVER a silent skip.
pub fn fakerepl_bin() -> PathBuf {
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
                    "STALE fakerepl binary at {bin:?} — run: cargo build -p fakerepl"
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

/// The shared jail dir layout (fakerepl's jail belt, a4-spec §5): HOME at
/// `<base>/sbrg-runs/<tag>-<nanos>/home` with `sb_home`/`tmp`/`zmx` as
/// root-siblings, plus a 0700 XDG dir beside the runs. `base` must be a SHORT
/// literal-/tmp path so the embedded sbmux sun_path fits (the 104-byte macOS
/// budget; c1_gate note / L21).
pub struct JailScaffold {
    pub root: PathBuf,
    pub home: PathBuf,
    pub xdg: PathBuf,
    pub sb_home: PathBuf,
}

/// Create the shared jail scaffold under `base` for `tag` (nanos-unique).
/// Creates `home/.claude/{sessions,projects/proj}`, the XDG dir (0700),
/// `sb_home`, `root/tmp`, `root/zmx`; asserts the L9a not-real-home guard.
pub fn establish_jail(base: &Path, tag: &str) -> JailScaffold {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = base.join("sbrg-runs").join(format!("{tag}-{nanos}"));
    let home = root.join("home");
    let xdg = base.join(format!("x-{tag}-{nanos}"));
    let sb_home = root.join("sb_home");
    for d in [
        &home.join(".claude").join("sessions"),
        &home.join(".claude").join("projects").join("proj"),
        &xdg,
        &sb_home,
        &root.join("tmp"),
        &root.join("zmx"),
    ] {
        std::fs::create_dir_all(d).unwrap();
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o700)).ok();
    super::assert_not_real_home(&home);
    JailScaffold {
        root,
        home,
        xdg,
        sb_home,
    }
}

/// Run `sb <args>` fully jailed (`env_clear` + the jail env contract):
/// HOME/SB_HOME/XDG_RUNTIME_DIR/TMPDIR/ZMX_DIR off the scaffold, PATH =
/// fakerepl's dir + `/usr/bin:/bin`, TERM, and `claude_bin` as CLAUDE_BIN.
/// `extra` pairs land LAST (per-launch fakerepl identity knobs).
pub fn run_sb_jailed(
    j: &JailScaffold,
    claude_bin: &Path,
    args: &[&str],
    extra: &[(&str, String)],
) -> (i32, String, String) {
    let fr = fakerepl_bin();
    let mut cmd = Command::new(sb_bin());
    cmd.args(args);
    cmd.env_clear()
        .env("HOME", &j.home)
        .env("SB_HOME", &j.sb_home)
        .env("XDG_RUNTIME_DIR", &j.xdg)
        .env("TMPDIR", j.root.join("tmp"))
        .env("ZMX_DIR", j.root.join("zmx"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", fr.parent().unwrap().display()),
        )
        .env("TERM", "xterm-256color")
        .env("CLAUDE_BIN", claude_bin.as_os_str());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn sb");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}
