//! M1: zmx socket-dir resolution, TS src/utils.ts:12-135.
//!
//! Port of collapseRepeatedSegments / resolveZmxDir / legacyZmxDirs (Bug-D
//! keystone). zmx selects its socket dir from
//! `ZMX_DIR > XDG_RUNTIME_DIR > $TMPDIR/zmx-<uid>` (falling back to
//! `/tmp/zmx-<uid>` when TMPDIR is empty). sb must derive the SAME dir the running
//! zmx uses and pin BOTH the read path (getZmxSessions) and the write path
//! (startDetached / zmx kill / attach / send) to it. Hard-coding `/tmp/zmx-<uid>`
//! ignored TMPDIR and caused the keystone read/write split, where `sb new` wrote a
//! session into `$TMPDIR/zmx-<uid>` while the reader/killer looked in
//! `/tmp/zmx-<uid>` → "live process, dead/unattachable zmx, can't kill via sb."
//!
//! On the origin box `TMPDIR=/tmp/claude-501` is injected into every Claude
//! context, so the real dir is `/tmp/claude-501/zmx-501` — resolved dynamically,
//! never hard-coded. (Port of the Bug-D keystone block, utils.ts:12-23.)

use crate::effects::Env;
use std::path::{Path, PathBuf};

/// Port of collapseRepeatedSegments, utils.ts:34-55.
///
/// Collapse runs of identical consecutive path segments. This is the precise
/// signature of the TMPDIR-compounding bug: each Claude-spawns-Claude re-injects
/// TMPDIR RELATIVE to the already-set one, so TMPDIR grows into
/// `/tmp/claude-501/claude-501/claude-501/...`. Without this, every nesting depth
/// derives a DIFFERENT zmx socket dir and sessions scatter into dirs no single
/// caller can reach (the "can't attach to anything" report).
///
/// Collapsing consecutive duplicates maps every nesting depth back to ONE
/// canonical dir: `/tmp/claude-501/claude-501/.../zmx-501` → `/tmp/claude-501/zmx-501`.
/// A single non-repeating segment (e.g. `/tmp/cstest-x`) is left untouched, so a
/// genuinely distinct TMPDIR still yields a distinct dir. Idempotent.
pub fn collapse_repeated_segments(p: &str) -> String {
    let lead = if p.starts_with('/') { "/" } else { "" };
    let mut out: Vec<&str> = Vec::new();
    for s in p.split('/').filter(|s| !s.is_empty()) {
        if out.last() != Some(&s) {
            out.push(s);
        }
    }
    format!("{lead}{}", out.join("/"))
}

/// Port of resolveZmxDir, pinned utils.ts:69-82 @ 0d0fa9e (commit 4bf5a76).
///
/// Replicate zmx's own precedence: `ZMX_DIR > XDG_RUNTIME_DIR > $TMPDIR/zmx-<uid>`,
/// falling back to `/tmp/zmx-<uid>` when TMPDIR is empty. The TMPDIR fallback is
/// normalized (trailing slashes stripped, then [`collapse_repeated_segments`]) so a
/// compounded TMPDIR re-nesting pegs to ONE canonical dir instead of scattering by
/// nesting depth. ZMX_DIR is explicit and NOT rewritten — the belt (ZMX_DIR set
/// fleet-wide) still wins outright.
///
/// PIN-RECONCILIATION (TS moved, we follow): the XDG branch now returns
/// `$XDG_RUNTIME_DIR/zmx`, NOT the bare root. War-story carried from pinned
/// utils.ts:71-77 @ 0d0fa9e: zmx's real XDG behavior is `$XDG_RUNTIME_DIR/zmx`
/// (proven empirically on Lima — zmx run with only XDG set created
/// `/run/user/501/zmx`). Returning the bare root (the old behavior) made sb write
/// sessions into the root while default zmx sessions lived in `<root>/zmx` — the
/// C2-Lima send-blind/kill-claims-success family (DUR-1/2, LIFE-9). Synthesis-gate
/// sanctioned fix 2026-06-04. Note the added `.trim()`: the XDG value is now trimmed
/// before the join (old behavior returned the original untrimmed value).
///
/// TS treats empty / whitespace-only env vars as unset (`env.X && env.X.trim()`).
///
/// L2: the XDG_RUNTIME_DIR tier is exercised LIVE on Linux only — macOS has no such
/// tier, so a real macOS run never reaches this branch (the golden scenario records
/// that exclusion). The unit tests below exercise the precedence MATH (not OS
/// reality) and are valid on any platform.
pub fn resolve_zmx_dir(env: &dyn Env) -> PathBuf {
    let uid = env.uid();

    // env.ZMX_DIR && env.ZMX_DIR.trim()  — empty/whitespace-only counts as unset.
    if let Some(zmx_dir) = nonblank(env.var("ZMX_DIR")) {
        return PathBuf::from(zmx_dir); // NOT rewritten — explicit belt wins.
    }
    // join(env.XDG_RUNTIME_DIR.trim(), "zmx") — pinned utils.ts:77 @ 0d0fa9e.
    if let Some(xdg) = nonblank(env.var("XDG_RUNTIME_DIR")) {
        return PathBuf::from(xdg.trim()).join("zmx");
    }

    // rawTmp = TMPDIR (trailing slashes stripped) or "/tmp"; then collapse.
    let raw_tmp = match nonblank(env.var("TMPDIR")) {
        Some(t) => strip_trailing_slashes(&t).to_string(),
        None => "/tmp".to_string(),
    };
    let tmp = collapse_repeated_segments(&raw_tmp);
    PathBuf::from(tmp).join(format!("zmx-{uid}"))
}

/// `env.X && env.X.trim()`: Some only when non-empty after trimming. Returns the
/// ORIGINAL value (TS returns `env.X`, not the trimmed copy, for ZMX_DIR/XDG).
fn nonblank(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// `replace(/\/+$/, "")`: strip a run of trailing slashes (TS does this to TMPDIR
/// before collapsing).
fn strip_trailing_slashes(s: &str) -> &str {
    s.trim_end_matches('/')
}

const MAX_NEST: usize = 8;

/// Test-lane env var overriding the legacy `/tmp` SCAN ROOT (A14-2(c), W1 carry).
///
/// Consulted ONLY for the SURVIVING READ scan (zmx-lane legacy-dir visibility +
/// the C2 cutover/TS-compat window). When set, the legacy scan reads the override
/// dir(s) INSTEAD of literal `/tmp`, giving jails read-invisibility. When UNSET
/// the production default (literal `/tmp` READ scan) is unchanged — the A14-2
/// ruling keeps the read scan as a VISIBILITY-ONLY surface.
///
/// BINDING (A14-2 discriminator, carried so the next reader can't relax it):
/// destructive targets are NEVER sourced from `/tmp` enumeration alone; `/tmp`
/// scan output is VISIBILITY-ONLY. This override touches the READ surface only.
pub const TEST_SCAN_ROOTS_ENV: &str = "SB_TEST_SCAN_ROOTS";

/// Resolve the legacy `/tmp` SCAN ROOT, honoring the test-lane override
/// ([`TEST_SCAN_ROOTS_ENV`]). Production passes `default` (literal `/tmp`); when
/// the override is set (test lanes only) it returns the override path(s) instead,
/// colon-separated. UNSET → `default` unchanged (the production default survives,
/// A14-2(c)).
///
/// This is READ-ONLY scan-root selection: the returned roots feed
/// [`legacy_zmx_dirs`]'s readdir, never a destructive sweep.
pub fn legacy_scan_roots(env: &dyn Env, default: &Path) -> Vec<PathBuf> {
    match nonblank(env.var(TEST_SCAN_ROOTS_ENV)) {
        Some(raw) => raw
            .split(':')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect(),
        None => vec![default.to_path_buf()],
    }
}

/// A scan root paired with its (already-read) direct-child listing. The pure
/// decider takes these so all I/O lives in the gather fn — `root` is the directory,
/// `listing` is its readdir (empty when the dir is unreadable; TS wraps readdir in
/// `try {} catch {}`).
#[derive(Debug, Clone)]
pub struct ScanRoot {
    pub root: PathBuf,
    pub listing: Vec<String>,
}

/// The XDG runtime-dir family for legacy-dir discovery (pinned utils.ts:141-166 @
/// 0d0fa9e). On Linux login shells zmx may place sessions under
/// `$XDG_RUNTIME_DIR/zmx` or `/run/user/<uid>/zmx`; this carries the raw env value
/// + the `/run/user/<uid>` fallback so the gather fn can scan both.
///
/// `Some(XdgFamily)` at the gather seam = scan the family (PRODUCTION). `None` =
/// suppressed (hermetic test fixtures / isolated mode). This maps the TS
/// roots-presence convention (`if (!roots)`) to an explicit Option, decoupled from
/// the scan-root identity — the two are INDEPENDENT axes (ADD-9b red-team BLOCKER 1:
/// production passes the literal `/tmp` positionally, so "roots provided = isolated"
/// would suppress the XDG family in production and reinstate the C2-Lima bug).
#[derive(Debug, Clone)]
pub struct XdgFamily {
    /// Raw `XDG_RUNTIME_DIR` env value (untrimmed; the decider trims + blank-filters
    /// it). Kept raw so the dedupe-quirk string compare is faithful to TS.
    pub xdg_runtime_dir: Option<String>,
    /// `/run/user/<uid>` in production; injectable to a temp path for hermetic tests.
    pub run_user_dir: PathBuf,
}

impl XdgFamily {
    /// Production constructor: raw `XDG_RUNTIME_DIR` from env + `/run/user/<uid>`.
    pub fn from_env(env: &dyn Env, uid: u32) -> Self {
        XdgFamily {
            xdg_runtime_dir: env.var("XDG_RUNTIME_DIR"),
            run_user_dir: PathBuf::from(format!("/run/user/{uid}")),
        }
    }
}

/// Port of legacyZmxDirs's PURE decider (pinned utils.ts:95-166 @ 0d0fa9e, commit
/// 4bf5a76).
///
/// Other candidate socket dirs that could hold sessions created under a DIFFERENT
/// caller TMPDIR (login-shell fallback, a Claude tmpdir, or any arbitrary
/// `TMPDIR=/tmp/<x>`). Discovered dynamically rather than hardcoded (red-team #1):
/// a session created under e.g. `TMPDIR=/tmp/rttest` lands its socket in
/// `/tmp/rttest/zmx-<uid>`, which two fixed paths would miss — leaving `sb ls` /
/// `reconcile` blind and `sb kill` able to leak the zmx side. We scan:
///   - per scan root: `<root>/zmx-<uid>` (direct child) + every
///     `<root>/<x>/zmx-<uid>` (any `TMPDIR=<root>/<x>` context) + the repeated-segment
///     chain `<root>/<name>/<name>/.../zmx-<uid>` so the TMPDIR-compounding nests
///     created BEFORE the resolveZmxDir normalization landed are still discoverable.
///     Bounded depth (MAX_NEST=8, no broad walk): only follows the exact
///     repeated-segment signature of the bug.
///   - per XDG-family root (PIN-RECONCILIATION, pinned utils.ts:158-165 @ 0d0fa9e):
///     the root itself ("in case zmx uses it directly") + each direct child whose
///     name starts with `zmx` (readdir, no deep walk). This REPLACES the old bare-XDG
///     candidate. The xdg_family_roots are EMPTY in isolated/hermetic mode (caller
///     passes `None` at the gather seam) — see [`XdgFamily`].
///
/// Returns only existing dirs, deduped, excluding the canonical dir, in TS insertion
/// order. `scan_roots` carry the `/tmp` treatment; `xdg_family_roots` carry the XDG
/// treatment; `existing` reports whether a path currently exists (all I/O injected so
/// the gather fn does it). In production `scan_roots` is `[{ /tmp, readdir(/tmp) }]`.
pub fn legacy_candidates(
    uid: u32,
    canonical: &Path,
    scan_roots: &[ScanRoot],
    xdg_family_roots: &[ScanRoot],
    existing: &dyn Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    // candidates: a Set with JS insertion order. We reproduce that order with a Vec
    // + membership set (so a later duplicate insert is a no-op, like Set.add).
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut in_set: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let add = |c: PathBuf,
               candidates: &mut Vec<PathBuf>,
               in_set: &mut std::collections::HashSet<PathBuf>| {
        if in_set.insert(c.clone()) {
            candidates.push(c);
        }
    };

    let zmx_name = format!("zmx-{uid}");

    // --- scan roots: zmx-<uid> + chain walk (pinned utils.ts:101-120 @ 0d0fa9e). ---
    for ScanRoot { root, listing } in scan_roots {
        // candidates.add(join(root, `zmx-<uid>`))
        add(root.join(&zmx_name), &mut candidates, &mut in_set);
        // for name of readdirSync(root): glob + repeated-segment chain walk.
        for name in listing {
            let mut prefix = root.join(name);
            // candidates.add(join(prefix, `zmx-<uid>`)) — added unconditionally.
            add(prefix.join(&zmx_name), &mut candidates, &mut in_set);
            // Follow root/name/name/name/... while the same segment keeps repeating
            // AND the prefix dir exists. Bounded by MAX_NEST.
            for _ in 0..MAX_NEST {
                prefix = prefix.join(name);
                if !existing(&prefix) {
                    break;
                }
                add(prefix.join(&zmx_name), &mut candidates, &mut in_set);
            }
        }
    }

    // --- XDG family: root itself + zmx* direct children (pinned utils.ts:158-165). ---
    // The xdgRoots dedupe + existence gating is done by the gather fn (which builds
    // these ScanRoots); here we just add the root + its zmx-prefixed children.
    for ScanRoot { root, listing } in xdg_family_roots {
        // candidates.add(xdgRoot) — "in case zmx uses it directly".
        add(root.clone(), &mut candidates, &mut in_set);
        for name in listing {
            // if (!name.startsWith("zmx")) continue;
            if !name.starts_with("zmx") {
                continue;
            }
            add(root.join(name), &mut candidates, &mut in_set);
        }
    }

    // Final filter: d !== canonical && existsSync(d) && !out.includes(d).
    // (Insertion order preserved; in_set already deduped, so the includes-check is
    // satisfied by construction, but we keep the existence + canonical guards.)
    candidates
        .into_iter()
        .filter(|d| d.as_path() != canonical && existing(d))
        .collect()
}

/// Thin fs gather for [`legacy_candidates`] (the I/O half of pinned utils.ts:95-166
/// @ 0d0fa9e).
///
/// Reads each scan root's listing (tests pass a temp dir so nothing touches the real
/// `/tmp`) and probes existence via std fs. `canonical` is this process's resolved
/// dir (from [`resolve_zmx_dir`]); `xdg` carries the XDG family to scan, or `None` to
/// suppress it (hermetic test fixtures / isolated mode).
///
/// XDG-family root build (pinned utils.ts:143-157 @ 0d0fa9e): xdgRoots =
/// `[trimmed XDG_RUNTIME_DIR if non-blank]` then push `run_user_dir` UNLESS the
/// trimmed-xdg equals the run_user dir.
///
/// **C1l / W5 fix (ADD-15-ruled, spec carry C1l):** the comparison now NORMALIZES
/// trailing slashes before dedupe, so the old XDG trailing-slash raw-string quirk
/// dies — `/run/user/501/` == `/run/user/501` and dedupes to ONE root (it scanned
/// BOTH before this fix). ADD-15 overrules the scope-note-letter "faithful-quirk"
/// argument: a trailing slash is the same directory and double-scanning it was a
/// wart, not a feature. The witness test
/// `legacy_zmx_dirs_trailing_slash_xdg_double_scans` RETIRES-WITH-REASON below
/// (renamed + asserting the NEW deduped behavior). Each xdgRoot is then included
/// only if it exists on disk.
///
/// NOTE: the production caller uses the literal `/tmp` scan root, the uid from
/// [`resolve_zmx_dir`], and `Some(XdgFamily::from_env(..))`; tests inject temp roots
/// and pass `None` for hermetic isolation. Errors reading a dir → empty listing.
pub fn legacy_zmx_dirs(
    uid: u32,
    canonical: &Path,
    scan_roots: &[PathBuf],
    xdg: Option<&XdgFamily>,
) -> Vec<PathBuf> {
    let read_listing = |dir: &Path| -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default()
    };

    let scan: Vec<ScanRoot> = scan_roots
        .iter()
        .map(|root| ScanRoot {
            root: root.clone(),
            listing: read_listing(root),
        })
        .collect();

    // Build the XDG-family ScanRoots (suppressed when `xdg` is None).
    let mut xdg_family: Vec<ScanRoot> = Vec::new();
    if let Some(fam) = xdg {
        // xdgRoots: [trimmed XDG if non-blank], then push run_user UNLESS it equals
        // the trimmed-xdg. C1l/ADD-15: compare with trailing slashes NORMALIZED so
        // `/run/user/501/` dedupes against `/run/user/501` (the old raw-string quirk
        // double-scanned a trailing-slash XDG; that wart is now dead).
        let mut xdg_root_strs: Vec<String> = Vec::new();
        if let Some(x) = nonblank(fam.xdg_runtime_dir.clone()) {
            xdg_root_strs.push(x.trim().to_string());
        }
        let run_user_str = fam.run_user_dir.to_string_lossy().to_string();
        let already = xdg_root_strs
            .iter()
            .any(|s| strip_trailing_slashes(s) == strip_trailing_slashes(&run_user_str));
        if !already {
            xdg_root_strs.push(run_user_str);
        }
        for s in xdg_root_strs {
            let root = PathBuf::from(&s);
            // if (!existsSync(xdgRoot)) continue;
            if !root.exists() {
                continue;
            }
            let listing = read_listing(&root);
            xdg_family.push(ScanRoot { root, listing });
        }
    }

    let existing = |p: &Path| p.exists();
    legacy_candidates(uid, canonical, &scan, &xdg_family, &existing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;
    use std::collections::HashSet;

    fn env(uid: u32, pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            uid,
        }
    }

    // --- collapse_repeated_segments (utils.ts:34-55) ---

    #[test]
    fn collapse_compounded_tmpdir_pegs_to_one() {
        // The exact bug signature: nested claude-501 repeats collapse to one.
        assert_eq!(
            collapse_repeated_segments("/tmp/claude-501/claude-501/claude-501"),
            "/tmp/claude-501"
        );
    }

    #[test]
    fn collapse_single_non_repeating_untouched() {
        // A genuinely distinct TMPDIR still yields a distinct dir.
        assert_eq!(collapse_repeated_segments("/tmp/cstest-x"), "/tmp/cstest-x");
        assert_eq!(collapse_repeated_segments("/tmp/a/b/c"), "/tmp/a/b/c");
    }

    #[test]
    fn collapse_is_idempotent() {
        let once = collapse_repeated_segments("/tmp/claude-501/claude-501/x/x/y");
        let twice = collapse_repeated_segments(&once);
        assert_eq!(once, twice);
        assert_eq!(once, "/tmp/claude-501/x/y");
    }

    #[test]
    fn collapse_leading_slash_handling() {
        // Leading slash preserved; non-absolute stays non-absolute.
        assert_eq!(collapse_repeated_segments("/a/a"), "/a");
        assert_eq!(collapse_repeated_segments("a/a/b"), "a/b");
        // TS: lead="/" + "".join([]) = "/" for a bare "/"; "" stays "".
        assert_eq!(collapse_repeated_segments("/"), "/");
        assert_eq!(collapse_repeated_segments(""), "");
    }

    // --- resolve_zmx_dir tier matrix (utils.ts:57-73, L1) ---

    #[test]
    fn tier_zmx_dir_wins_outright() {
        // ZMX_DIR set → returned verbatim, NOT rewritten (even if it has repeats).
        let e = env(501, &[("ZMX_DIR", "/custom/zmx/dir")]);
        assert_eq!(resolve_zmx_dir(&e), PathBuf::from("/custom/zmx/dir"));

        // The belt is not collapsed: a repeated-segment ZMX_DIR is returned as-is.
        let e2 = env(501, &[("ZMX_DIR", "/x/x/dir")]);
        assert_eq!(resolve_zmx_dir(&e2), PathBuf::from("/x/x/dir"));
    }

    #[test]
    fn tier_xdg_runtime_dir() {
        // L2: this tier is LIVE on Linux only; on macOS it never fires in
        // production. This test exercises the precedence MATH, not OS reality, so
        // it is valid on every platform.
        //
        // PIN-RECONCILIATION (pinned utils.ts:77 @ 0d0fa9e): XDG resolves to
        // `<root>/zmx`, NOT the bare root. Re-contracted exactly as TS's own test was
        // (pinned utils.test.ts ~line 339: `/run/user/501` → `/run/user/501/zmx`).
        let e = env(501, &[("XDG_RUNTIME_DIR", "/run/user/501")]);
        assert_eq!(resolve_zmx_dir(&e), PathBuf::from("/run/user/501/zmx"));
        // The added `.trim()`: a trailing-space XDG value is trimmed, then joined
        // with `zmx`. (The stale "XDG NOT rewritten → bare root verbatim"
        // sub-assertion is DELETED — it contradicted the pin; red-team item 5.)
        let e2 = env(501, &[("XDG_RUNTIME_DIR", "  /run/user/501  ")]);
        assert_eq!(resolve_zmx_dir(&e2), PathBuf::from("/run/user/501/zmx"));
        // A trailing-slash XDG value: trim() leaves the slash, join("zmx") still
        // yields `/run/user/501/zmx` (PathBuf collapses the doubled separator).
        let e3 = env(501, &[("XDG_RUNTIME_DIR", "/run/user/501/")]);
        assert_eq!(resolve_zmx_dir(&e3), PathBuf::from("/run/user/501/zmx"));
    }

    #[test]
    fn tier_tmpdir_collapsed_and_suffixed() {
        let e = env(501, &[("TMPDIR", "/tmp/claude-501")]);
        assert_eq!(
            resolve_zmx_dir(&e),
            PathBuf::from("/tmp/claude-501/zmx-501")
        );
    }

    #[test]
    fn tier_tmpdir_fallback_to_tmp() {
        // No TMPDIR → /tmp/zmx-<uid>.
        let e = env(501, &[]);
        assert_eq!(resolve_zmx_dir(&e), PathBuf::from("/tmp/zmx-501"));
    }

    #[test]
    fn empty_and_whitespace_vars_are_unset() {
        // env.X && env.X.trim(): empty or whitespace-only → unset, fall through.
        let e = env(
            501,
            &[("ZMX_DIR", ""), ("XDG_RUNTIME_DIR", "   "), ("TMPDIR", "")],
        );
        assert_eq!(resolve_zmx_dir(&e), PathBuf::from("/tmp/zmx-501"));
    }

    #[test]
    fn tmpdir_trailing_slashes_stripped_then_collapsed() {
        // replace(/\/+$/,"") then collapse.
        let e = env(501, &[("TMPDIR", "/tmp/claude-501///")]);
        assert_eq!(
            resolve_zmx_dir(&e),
            PathBuf::from("/tmp/claude-501/zmx-501")
        );

        // Compounded + trailing slashes: pegs to ONE canonical dir.
        let e2 = env(501, &[("TMPDIR", "/tmp/claude-501/claude-501/claude-501/")]);
        assert_eq!(
            resolve_zmx_dir(&e2),
            PathBuf::from("/tmp/claude-501/zmx-501")
        );
    }

    #[test]
    fn compounded_tmpdir_canonical_pins_read_and_write() {
        // L1 doc-test: every nesting depth derives the SAME canonical dir, so the
        // read path and the write path agree (the keystone Bug-D fix).
        let depths = [
            "/tmp/claude-501",
            "/tmp/claude-501/claude-501",
            "/tmp/claude-501/claude-501/claude-501",
            "/tmp/claude-501/claude-501/claude-501/claude-501",
        ];
        let canonical = PathBuf::from("/tmp/claude-501/zmx-501");
        for d in depths {
            let e = env(501, &[("TMPDIR", d)]);
            assert_eq!(resolve_zmx_dir(&e), canonical, "depth {d}");
        }
    }

    // --- legacy_candidates pure decider (pinned utils.ts:95-166 @ 0d0fa9e) ---

    /// Build a single scan root from a literal path + listing (test convenience).
    fn sroot(root: &str, listing: &[&str]) -> ScanRoot {
        ScanRoot {
            root: PathBuf::from(root),
            listing: listing.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn legacy_includes_tmp_fallback() {
        let canonical = PathBuf::from("/tmp/claude-501/zmx-501");
        // The empty-TMPDIR fallback /tmp/zmx-501 exists.
        let exist: HashSet<PathBuf> = [PathBuf::from("/tmp/zmx-501")].into_iter().collect();
        let existing = |p: &Path| exist.contains(p);

        let out = legacy_candidates(
            501,
            &canonical,
            &[sroot("/tmp", &[])], // empty /tmp listing
            &[],                   // no XDG family (isolated)
            &existing,
        );
        assert_eq!(out, vec![PathBuf::from("/tmp/zmx-501")]);
    }

    #[test]
    fn legacy_globs_tmp_subdirs() {
        let canonical = PathBuf::from("/tmp/claude-501/zmx-501");
        // Two TMPDIR contexts: /tmp/rttest/zmx-501 and /tmp/other/zmx-501 exist.
        let exist: HashSet<PathBuf> = [
            PathBuf::from("/tmp/rttest/zmx-501"),
            PathBuf::from("/tmp/other/zmx-501"),
        ]
        .into_iter()
        .collect();
        let existing = |p: &Path| exist.contains(p);

        let out = legacy_candidates(
            501,
            &canonical,
            &[sroot("/tmp", &["rttest", "other", "noise"])],
            &[],
            &existing,
        );
        // /tmp/zmx-501 doesn't exist so it's filtered; the two real ones survive,
        // /tmp/noise/zmx-501 doesn't exist so it's filtered.
        assert_eq!(
            out,
            vec![
                PathBuf::from("/tmp/rttest/zmx-501"),
                PathBuf::from("/tmp/other/zmx-501"),
            ]
        );
    }

    #[test]
    fn legacy_walks_repeated_segment_chain() {
        let canonical = PathBuf::from("/tmp/claude-501/zmx-501");
        // The legacy nest: /tmp/claude-501/claude-501/.../zmx-501 from before the
        // collapse landed. The chain prefixes exist down to depth 3.
        let exist: HashSet<PathBuf> = [
            PathBuf::from("/tmp/claude-501/claude-501"),
            PathBuf::from("/tmp/claude-501/claude-501/claude-501"),
            // socket dirs that exist (legacy scattered sessions):
            PathBuf::from("/tmp/claude-501/claude-501/zmx-501"),
            PathBuf::from("/tmp/claude-501/claude-501/claude-501/zmx-501"),
        ]
        .into_iter()
        .collect();
        let existing = |p: &Path| exist.contains(p);

        let out = legacy_candidates(
            501,
            &canonical,
            &[sroot("/tmp", &["claude-501"])],
            &[],
            &existing,
        );
        // canonical (/tmp/claude-501/zmx-501) is excluded; the two deeper legacy
        // socket dirs are discovered.
        assert_eq!(
            out,
            vec![
                PathBuf::from("/tmp/claude-501/claude-501/zmx-501"),
                PathBuf::from("/tmp/claude-501/claude-501/claude-501/zmx-501"),
            ]
        );
    }

    #[test]
    fn legacy_chain_walk_stops_at_max_nest() {
        let canonical = PathBuf::from("/tmp/x/zmx-501");
        // An adversarial deep nest: every prefix exists. The walk must stop after
        // MAX_NEST (8) deeper levels — bounded, no runaway.
        let existing = |_: &Path| true;
        let out = legacy_candidates(501, &canonical, &[sroot("/tmp", &["x"])], &[], &existing);
        // candidates added: /tmp/zmx-501, /tmp/x/zmx-501 (base), then 8 chain levels.
        // Each is "existing" (true) so all survive except canonical (/tmp/x/zmx-501).
        // Count = 1 (tmp fallback) + 1 (base, but == canonical -> excluded) + 8 chain.
        // So 1 + 8 = 9 survivors.
        assert_eq!(out.len(), 9, "{out:?}");
        assert!(!out.contains(&canonical), "canonical excluded");
    }

    #[test]
    fn legacy_dedupe_and_exclude_canonical() {
        let canonical = PathBuf::from("/tmp/zmx-501"); // canonical IS the tmp fallback
        let exist: HashSet<PathBuf> = [PathBuf::from("/tmp/dup/zmx-501")].into_iter().collect();
        let existing = |p: &Path| exist.contains(p);
        // Listing has "dup" twice (Set.add dedupes); canonical == /tmp/zmx-501 excluded.
        let out = legacy_candidates(
            501,
            &canonical,
            &[sroot("/tmp", &["dup", "dup"])],
            &[],
            &existing,
        );
        assert_eq!(out, vec![PathBuf::from("/tmp/dup/zmx-501")]);
    }

    // --- XDG-family decider behavior (pinned utils.ts:158-165 @ 0d0fa9e) ---

    #[test]
    fn legacy_xdg_family_root_and_zmx_children() {
        // The XDG root itself + each zmx*-prefixed direct child are candidates; a
        // non-zmx child is excluded. (root-itself + zmx* children, no deep walk.)
        let canonical = PathBuf::from("/tmp/claude-501/zmx-501");
        let exist: HashSet<PathBuf> = [
            PathBuf::from("/run/user/501"),
            PathBuf::from("/run/user/501/zmx"),
            PathBuf::from("/run/user/501/zmx-cstest"),
            PathBuf::from("/run/user/501/other"), // non-zmx → excluded even if existing
        ]
        .into_iter()
        .collect();
        let existing = |p: &Path| exist.contains(p);

        let out = legacy_candidates(
            501,
            &canonical,
            &[], // no /tmp scan roots in this focused case
            &[sroot("/run/user/501", &["zmx", "zmx-cstest", "other"])],
            &existing,
        );
        assert_eq!(
            out,
            vec![
                PathBuf::from("/run/user/501"),
                PathBuf::from("/run/user/501/zmx"),
                PathBuf::from("/run/user/501/zmx-cstest"),
            ],
            "root itself + zmx* children, non-zmx excluded"
        );
    }

    #[test]
    fn legacy_c2_migration_bare_xdg_root_returned() {
        // C2 migration case (red-team item 3): canonical = <xdg>/zmx; the BARE <xdg>
        // root still exists (old-sb sessions live there) → returned as legacy, NOT
        // excluded by the canonical filter. The actual post-upgrade cleanup path.
        let canonical = PathBuf::from("/run/user/501/zmx");
        let exist: HashSet<PathBuf> = [PathBuf::from("/run/user/501")].into_iter().collect();
        let existing = |p: &Path| exist.contains(p);

        let out = legacy_candidates(
            501,
            &canonical,
            &[],
            &[sroot("/run/user/501", &[])],
            &existing,
        );
        // The bare root /run/user/501 is the legacy dir; canonical (.../zmx) excluded.
        assert_eq!(out, vec![PathBuf::from("/run/user/501")]);
    }

    // --- legacy_zmx_dirs thin fs gather (injected roots; XDG via XdgFamily) ---

    #[test]
    fn legacy_zmx_dirs_reads_injected_tmp_root() {
        use std::fs;
        let tmp_root = tempfile::tempdir().unwrap();
        let root = tmp_root.path();
        // Create /tmp_root/rttest/zmx-501 as an existing legacy socket dir.
        let legacy = root.join("rttest").join("zmx-501");
        fs::create_dir_all(&legacy).unwrap();
        // Canonical is elsewhere (not under this temp root).
        let canonical = PathBuf::from("/nonexistent/zmx-501");

        let out = legacy_zmx_dirs(501, &canonical, &[root.to_path_buf()], None);
        assert!(out.contains(&legacy), "found injected legacy dir: {out:?}");
        // Nothing here touches the real /tmp or the real home (L9a discipline).
    }

    #[test]
    fn legacy_zmx_dirs_run_user_fallback_when_xdg_unset() {
        // XDG_RUNTIME_DIR unset → the run_user_dir is the sole XDG-family root. We
        // inject run_user_dir at a temp path containing a zmx* child (hermetic).
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let scan = tmp.path().join("scan");
        let run_user = tmp.path().join("run-user-501");
        let zmx_child = run_user.join("zmx");
        fs::create_dir_all(&scan).unwrap();
        fs::create_dir_all(&zmx_child).unwrap();
        let canonical = PathBuf::from("/nonexistent/zmx-501");

        let fam = XdgFamily {
            xdg_runtime_dir: None, // XDG unset → fall back to run_user_dir
            run_user_dir: run_user.clone(),
        };
        let out = legacy_zmx_dirs(501, &canonical, &[scan], Some(&fam));
        assert!(out.contains(&run_user), "run_user root itself: {out:?}");
        assert!(out.contains(&zmx_child), "run_user zmx child: {out:?}");
    }

    #[test]
    fn legacy_zmx_dirs_xdg_trim_applied() {
        // The XDG env value is trimmed before use (matches resolveZmxDir .trim()).
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().join("xdg-runtime");
        let zmx_child = xdg.join("zmx");
        fs::create_dir_all(&zmx_child).unwrap();
        let canonical = PathBuf::from("/nonexistent/zmx-501");

        // Whitespace-padded XDG value → trimmed → existing dir found.
        let padded = format!("  {}  ", xdg.to_string_lossy());
        let fam = XdgFamily {
            xdg_runtime_dir: Some(padded),
            run_user_dir: tmp.path().join("run-user-501"), // nonexistent → skipped
        };
        let out = legacy_zmx_dirs(501, &canonical, &[], Some(&fam));
        assert!(out.contains(&xdg), "trimmed xdg root: {out:?}");
        assert!(out.contains(&zmx_child), "trimmed xdg zmx child: {out:?}");
    }

    #[test]
    fn legacy_zmx_dirs_trailing_slash_xdg_dedupes_normalized() {
        // RETIRED-WITH-REASON (C1l / ADD-15, spec carry C1l): this was
        // `legacy_zmx_dirs_trailing_slash_xdg_double_scans`, the FAITHFUL-QUIRK
        // witness asserting a trailing-slash XDG (`/run/user/501/`) failed to
        // dedupe against the slash-less run_user, so BOTH roots scanned. ADD-15
        // overrules that scope-note-letter argument: a trailing slash is the SAME
        // directory and double-scanning it was a wart. The test now asserts the
        // NEW NORMALIZED behavior — `<p>/` dedupes against `<p>` to a single root.
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        // A single on-disk dir reachable by both "<p>" and "<p>/".
        let dir = tmp.path().join("runtime");
        let zmx_child = dir.join("zmx");
        fs::create_dir_all(&zmx_child).unwrap();
        let canonical = PathBuf::from("/nonexistent/zmx-501");

        let base = dir.to_string_lossy().to_string();
        let fam = XdgFamily {
            // trimmed-xdg has a trailing slash; run_user does not — NORMALIZED
            // comparison now dedupes them (the quirk is dead).
            xdg_runtime_dir: Some(format!("{base}/")),
            run_user_dir: PathBuf::from(&base),
        };
        let out = legacy_zmx_dirs(501, &canonical, &[], Some(&fam));
        // The dir + its zmx child are present exactly once.
        assert!(out.contains(&dir), "runtime dir present: {out:?}");
        assert!(out.contains(&zmx_child), "zmx child present: {out:?}");
        let dir_count = out.iter().filter(|p| **p == dir).count();
        assert_eq!(
            dir_count, 1,
            "trailing-slash XDG deduped against run_user (C1l fix): {out:?}"
        );
    }

    #[test]
    fn legacy_zmx_dirs_isolated_mode_suppresses_xdg() {
        // Isolated-mode suppression (mirrors pinned utils.test.ts ~line 330): None
        // at the gather seam → NO XDG candidates even when an XDG dir with a zmx
        // child exists on disk. (Machine-global XDG state must not bleed into
        // hermetic/injected-root runs.)
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let scan = tmp.path().join("scan");
        let fake_xdg = tmp.path().join("xdg-runtime");
        let fake_zmx = fake_xdg.join("zmx");
        fs::create_dir_all(&scan).unwrap();
        fs::create_dir_all(&fake_zmx).unwrap();
        let canonical = PathBuf::from("/nonexistent/zmx-501");

        // None → XDG family suppressed. The fake_zmx must NOT appear.
        let out = legacy_zmx_dirs(501, &canonical, &[scan], None);
        assert!(
            !out.contains(&fake_zmx),
            "XDG suppressed in isolated mode: {out:?}"
        );
        assert!(!out.contains(&fake_xdg), "XDG root suppressed too: {out:?}");
    }

    #[test]
    fn legacy_zmx_dirs_xdg_dedupes_when_run_user_equals_xdg() {
        // When trimmed-xdg STRING == run_user STRING, run_user is NOT pushed again
        // (TS `xdgRoots.includes` true → skip). Only one XDG root scans.
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("runtime");
        let zmx_child = dir.join("zmx");
        fs::create_dir_all(&zmx_child).unwrap();
        let canonical = PathBuf::from("/nonexistent/zmx-501");

        let s = dir.to_string_lossy().to_string();
        let fam = XdgFamily {
            xdg_runtime_dir: Some(s.clone()),
            run_user_dir: PathBuf::from(&s), // identical string → deduped
        };
        let out = legacy_zmx_dirs(501, &canonical, &[], Some(&fam));
        // dir + its zmx child appear exactly once (Set dedupe regardless), and no
        // panic from the single-root path.
        assert!(out.contains(&dir));
        assert!(out.contains(&zmx_child));
        let dir_count = out.iter().filter(|p| **p == dir).count();
        assert_eq!(dir_count, 1, "deduped to a single entry: {out:?}");
    }
}
