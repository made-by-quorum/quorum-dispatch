//! Jail refusal belt (a4-spec §5, redesigned per spec-red-team R3 + the finding
//! the red-team's own fix missed).
//!
//! `JAIL_ROOT` / `JAIL_RUNID` / `JAIL_PREFIX` are shell-LOCAL in jail.sh
//! (jail.sh:86-98, NO `export`) — a child across the zmx boundary NEVER sees
//! them. So neither v1's QD_HOME glob nor the red-team's JAIL_ROOT-keyed fix can
//! work. This belt is derived entirely from the EXPORTED isolation set
//! (jail.sh:139-146):
//!
//!   export HOME="$JAIL_ROOT/home"
//!   export QD_HOME="$JAIL_ROOT/qd_home"
//!   export ZMX_DIR="$JAIL_ROOT/zmx"
//!   export TMPDIR="$JAIL_ROOT/tmp"
//!
//! Refuse (Err with the failed-check reason) unless ALL hold:
//!   (a) HOME matches `*/qdrg-runs/*/home`  (the positive jail-layout marker);
//!   (b) with `root := dirname(HOME)`:  QD_HOME == root/qd_home
//!                                       ZMX_DIR == root/zmx
//!                                       TMPDIR == root/tmp
//!
//! (b) is the COHERENCE check: the full exported isolation set must agree on ONE
//! jail root, so a partial spoof (HOME jail-shaped but QD_HOME pointing
//! elsewhere) is refused. Mirrors `jail_assert_established`'s positive-detection
//! philosophy (jail.sh:202-258) without inventing a parallel convention OR
//! depending on un-exported vars.

use std::path::Path;

/// Returns `Ok(())` if the current process env is a coherent qdrg jail, else
/// `Err(reason)` naming the first failed check (so the exit-13 stderr is
/// diagnostic, per a4-spec §5's negative-control rows).
pub fn assert_jailed_env() -> Result<(), String> {
    let home = require_var("HOME")?;
    let home_n = normalize(&home);

    // (a) HOME positive marker: `*/qdrg-runs/*/home`.
    if !home_matches_jail_layout(&home_n) {
        return Err(format!(
            "HOME does not match the jail layout `*/qdrg-runs/*/home` (got {home})"
        ));
    }

    // root := dirname(HOME). After (a), HOME ends in `/home`, so the parent is
    // the per-run jail root.
    let root = Path::new(&home_n)
        .parent()
        .ok_or_else(|| format!("cannot derive jail root from HOME ({home})"))?
        .to_string_lossy()
        .to_string();

    // (b) coherence: every exported isolation var must sit under the SAME root.
    check_under_root("QD_HOME", "qd_home", &root)?;
    check_under_root("ZMX_DIR", "zmx", &root)?;
    check_under_root("TMPDIR", "tmp", &root)?;

    Ok(())
}

/// Read an env var, erroring with a named reason if absent/empty.
fn require_var(key: &str) -> Result<String, String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(format!("{key} is not set")),
    }
}

/// Assert `<KEY>` is set and equals `<root>/<leaf>` (after trailing-slash
/// normalization). Names the failed check on mismatch.
fn check_under_root(key: &str, leaf: &str, root: &str) -> Result<(), String> {
    let val = require_var(key)?;
    let want = format!("{root}/{leaf}");
    if normalize(&val) != want {
        return Err(format!(
            "{key} is not coherent with the jail root: expected {want}, got {val}"
        ));
    }
    Ok(())
}

/// HOME matches `*/qdrg-runs/*/home`: the final component is `home`, the
/// grandparent component is some run id, and the great-grandparent is
/// `qdrg-runs`. Component-based (not substring) so a path like
/// `/x/qdrg-runs-evil/home` cannot spoof it.
fn home_matches_jail_layout(home_n: &str) -> bool {
    let p = Path::new(home_n);
    let mut comps: Vec<&str> = p
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    // Need at least: qdrg-runs / <runid> / home  (3 trailing components).
    let home = comps.pop();
    let _runid = comps.pop();
    let qdrg = comps.pop();
    home == Some("home") && qdrg == Some("qdrg-runs")
}

/// Strip a single trailing slash (jail.sh exports slash-free paths, but a caller
/// env could carry one). Does not touch interior structure.
fn normalize(s: &str) -> String {
    let t = s.strip_suffix('/').unwrap_or(s);
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // We can't safely mutate process env in parallel unit tests, so these test
    // the pure predicates directly.

    #[test]
    fn home_layout_accepts_valid_jail() {
        assert!(home_matches_jail_layout(
            "/var/folders/xx/T/qdrg-runs/ABC123/home"
        ));
        assert!(home_matches_jail_layout("/tmp/qdrg-runs/r/home"));
    }

    #[test]
    fn home_layout_rejects_clean_and_spoofs() {
        assert!(!home_matches_jail_layout("/home/u"));
        assert!(!home_matches_jail_layout("/home/someone"));
        // Substring spoof: `qdrg-runs-evil` is NOT the `qdrg-runs` component.
        assert!(!home_matches_jail_layout("/x/qdrg-runs-evil/r/home"));
        // Missing the trailing `home` component.
        assert!(!home_matches_jail_layout("/tmp/qdrg-runs/r"));
        // `home` present but not under qdrg-runs.
        assert!(!home_matches_jail_layout("/tmp/other/r/home"));
    }

    #[test]
    fn check_under_root_coherence() {
        let root = "/tmp/qdrg-runs/r";
        // env-free variant of check: just the path equality the function does.
        assert_eq!(
            normalize("/tmp/qdrg-runs/r/qd_home/"),
            format!("{root}/qd_home")
        );
        assert_ne!(normalize("/elsewhere/qd_home"), format!("{root}/qd_home"));
    }
}
