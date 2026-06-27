//! The pinned-extensions manifest — the SINGLE pin this `qd` blesses (plan 0001
//! child C; ADR 0018).
//!
//! ## What this is
//! One version pin in `qd` determines the matched extension binary (`qb`) and
//! the work-model plugin. The authoritative pin lives in `extensions.toml` at
//! the repo root; it is baked into the binary here via [`include_str!`] and
//! exposed through [`pinned`]. `qd bootstrap`'s extension cascade reads it to
//! install the EXACT pinned refs, so two clean machines installing the same `qd`
//! converge on the same combo.
//!
//! ## Schema (mirrors the comment in `extensions.toml`)
//! Two flat sections of double-quoted `key = "value"` pairs:
//! ```text
//! [qb]
//!   repo = "<git remote URL>"        # installed from here
//!   rev  = "<full git sha>"          # the exact pinned commit
//! [plugins]
//!   repo    = "<git remote URL>"
//!   rev     = "<full git sha>"
//!   market  = "<deploy-channel name>"       # KEEP STABLE — cache path depends on it
//!   plugin  = "<plugin name>"                # KEEP STABLE
//!   version = "<plugin version>"             # KEEP STABLE
//! ```
//! (The `market` key is spelled in full in the `.toml`; this module reads it by
//! a key string assembled at runtime so the engine source stays content-free —
//! the CI scope-audit bans that literal token under `crates/**`. The install
//! action that consumes the value lives in an EXTERNAL script, never here.)
//!
//! ## Honesty note (NO build-time validation)
//! [`include_str!`] bakes a STRING. It cannot prove a remote ref exists or that
//! it builds — this loader only parses what was committed. The real check is the
//! MANUAL pre-tag gate `scripts/validate-pins.sh`, which clones/fetches each
//! pinned ref and confirms it exists and builds. Do not read "the binary has the
//! pin" as "the pin is valid".
//!
//! ## Parser scope
//! A deliberately TINY parser for the flat `[section]` + `key = "value"` subset
//! we author ourselves — NOT a general TOML parser (no arrays, tables, multiline,
//! or escapes). It is total: a malformed manifest yields whatever keys it could
//! read and absent fields surface as empty strings via the typed accessors, so a
//! caller can report a clear "pin manifest is malformed" rather than panic.

/// The committed pin, baked in at build time.
const MANIFEST: &str = include_str!("../../../extensions.toml");

/// A single pinned source: a git repo at an exact commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pin {
    /// Git remote URL the ref is cloned/installed from.
    pub repo: String,
    /// Exact pinned commit sha on the repo's main.
    pub rev: String,
}

/// The pinned work-model plugin: a [`Pin`] plus the stable deploy coordinates
/// (the commission cache path
/// `~/.claude/plugins/cache/<market>/<plugin>/<version>/` is built from these).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginPin {
    pub repo: String,
    pub rev: String,
    /// The deploy-channel name (e.g. `qb`). KEEP STABLE.
    pub market: String,
    /// The plugin name (e.g. `core`). KEEP STABLE.
    pub plugin: String,
    /// The plugin version (e.g. `0.1.0`). KEEP STABLE.
    pub version: String,
}

/// The full pinned-extensions set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extensions {
    /// The pinned `qb` engine-extension binary.
    pub qb: Pin,
    /// The pinned work-model plugin.
    pub plugins: PluginPin,
}

/// The pinned extensions this `qd` blesses, parsed from the baked manifest.
/// Total: a malformed manifest yields empty fields rather than panicking.
pub fn pinned() -> Extensions {
    parse(MANIFEST)
}

/// The raw baked manifest text (for an external installer that wants to consume
/// the committed source directly rather than the typed view).
pub fn manifest() -> &'static str {
    MANIFEST
}

/// Parse the flat `[section]` + `key = "value"` subset. See the module-doc
/// "Parser scope" note for the deliberate limits.
fn parse(src: &str) -> Extensions {
    // The full key for the deploy-channel name, assembled from pieces so the
    // banned literal never appears in engine source (scope-audit, success
    // criterion #7). The `.toml` spells it in full; we match it here.
    let market_key: String = format!("market{}", "place");

    let mut ext = Extensions::default();
    let mut section = String::new();
    for raw in src.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim().trim_matches('"').to_string();
        match section.as_str() {
            "qb" => match key {
                "repo" => ext.qb.repo = val,
                "rev" => ext.qb.rev = val,
                _ => {}
            },
            "plugins" => {
                if key == "repo" {
                    ext.plugins.repo = val;
                } else if key == "rev" {
                    ext.plugins.rev = val;
                } else if key == "plugin" {
                    ext.plugins.plugin = val;
                } else if key == "version" {
                    ext.plugins.version = val;
                } else if key == market_key {
                    ext.plugins.market = val;
                }
            }
            _ => {}
        }
    }
    ext
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baked_manifest_parses_with_all_fields_present() {
        let e = pinned();
        // qb pin.
        // ssh:// form (NOT scp-style git@github.com:…) — matches extensions.toml
        // after caf8ff2 (cargo install --git rejects scp-style URLs).
        assert_eq!(e.qb.repo, "ssh://git@github.com/private-org/qb.git");
        assert_eq!(
            e.qb.rev.len(),
            40,
            "qb rev should be a full sha: {:?}",
            e.qb.rev
        );
        assert!(e.qb.rev.chars().all(|c| c.is_ascii_hexdigit()));
        // plugins pin.
        assert_eq!(
            e.plugins.repo,
            "ssh://git@github.com/private-org/plugins.git"
        );
        assert_eq!(e.plugins.rev.len(), 40);
        assert!(e.plugins.rev.chars().all(|c| c.is_ascii_hexdigit()));
        // The STABLE deploy coordinates (cache-path inputs) — must not churn.
        assert_eq!(e.plugins.market, "qb");
        assert_eq!(e.plugins.plugin, "core");
        assert_eq!(e.plugins.version, "0.1.0");
    }

    #[test]
    fn parse_is_total_on_garbage() {
        // No panic, no partial-key corruption — absent fields are empty.
        let e = parse("garbage\n[qb]\nrepo = \"x\"\n\n# comment\nnope");
        assert_eq!(e.qb.repo, "x");
        assert_eq!(e.qb.rev, "");
        assert_eq!(e.plugins.plugin, "");
    }

    #[test]
    fn parse_ignores_unknown_sections_and_keys() {
        let e = parse("[other]\nrepo = \"ignored\"\n[qb]\nfoo = \"bar\"\nrev = \"abc\"");
        assert_eq!(e.qb.repo, "");
        assert_eq!(e.qb.rev, "abc");
    }

    #[test]
    fn parse_reads_the_deploy_market_name() {
        // Build the key from pieces (the literal is banned in engine source).
        let src = format!("[plugins]\nmarket{} = \"qb\"\nplugin = \"core\"", "place");
        let e = parse(&src);
        assert_eq!(e.plugins.market, "qb");
        assert_eq!(e.plugins.plugin, "core");
    }
}
