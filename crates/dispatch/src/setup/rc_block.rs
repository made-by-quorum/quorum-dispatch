//! The managed PATH block in a shell rc file (R15 item 3) — port of
//! `qrm/src/verbs.rs::wire_shell_path` + `qrm/src/rewrite/bashrc.rs`.
//!
//! # What the markers buy
//!
//! `# >>> … >>>` / `# <<< … <<<` fence a region of the user's rc file that
//! setup owns. Everything outside the fence is never touched, and a re-run
//! REPLACES the fenced region instead of appending a second one — which is the
//! whole reason the markers exist: `qrm bootstrap` was re-run routinely and had
//! to not grow the file each time. That idempotence is carried over verbatim;
//! the marker TEXT is re-branded from `qrm bootstrap` to `qd setup`, since
//! `qrm` does not ship.
//!
//! # What is deliberately NOT carried over: the baked `claude()` wrapper
//!
//! `qrm`'s block also defined a `claude()` shell function that called
//! `qd start` / `qd connect`. That is a fossil generator and this repo already
//! ruled against it — see [`crate::shell_init`]'s module doc: a wrapper BAKED
//! into an rc file breaks silently the moment a verb's argument contract
//! changes (observed live, 2026-06-09), which is why `qd` moved to the
//! eval-init pattern (`eval "$(qd init zsh)"`) where the wrapper body ships
//! inside the binary. Re-introducing the baked wrapper here would undo that
//! ruling. So this block carries PATH and nothing else, and the init line stays
//! `qd bootstrap`'s consent-gated step.
//!
//! `QD_HOME`/`FRAME_ROOT` are also dropped: `~/.quorum/dispatch` is already the
//! default, and `FRAME_ROOT` belongs to `qf`, which does not ship.

/// Start marker. Matched by prefix+suffix (like `qrm`'s `rewrite::bashrc`) so a
/// re-brand of the middle never orphans a block a previous version wrote.
const START_PREFIX: &str = "# >>>";
const START_SUFFIX: &str = "setup >>>";
const END_PREFIX: &str = "# <<<";
const END_SUFFIX: &str = "setup <<<";

fn is_marker(line: &str, prefix: &str, suffix: &str) -> bool {
    let t = line.trim();
    t.starts_with(prefix) && t.ends_with(suffix)
}

/// Render the managed block for `bin_dir`. Always ends with a newline.
///
/// Note the `PATH` guard: re-sourcing an rc file (every new shell in a nested
/// session) would otherwise prepend the directory again and again, growing
/// `$PATH` without bound. `qrm`'s block had no guard; this one does.
pub fn managed_block(bin_dir: &str) -> String {
    format!(
        "# >>> qd setup >>>\n\
         # Managed by `qd setup` — do not edit between these markers.\n\
         # `qd` must be on PATH: the ~/.claude.json relay pin stores the BARE\n\
         # command `qd`, which Claude Code resolves via PATH when it launches\n\
         # the relay MCP server.\n\
         case \":$PATH:\" in\n\
         \x20 *\":{bin_dir}:\"*) ;;\n\
         \x20 *) export PATH=\"{bin_dir}:$PATH\" ;;\n\
         esac\n\
         # <<< qd setup <<<\n"
    )
}

/// Locate the managed block's inclusive `(start, end)` line indices.
/// `None` when there is no well-formed block.
fn locate_block(lines: &[&str]) -> Option<(usize, usize)> {
    let start = lines
        .iter()
        .position(|l| is_marker(l, START_PREFIX, START_SUFFIX))?;
    let end = lines
        .iter()
        .position(|l| is_marker(l, END_PREFIX, END_SUFFIX))?;
    if end > start {
        Some((start, end))
    } else {
        // A malformed fence (end before start) is left ALONE: we would rather
        // append a correct block than surgically edit a file we cannot parse.
        None
    }
}

/// Does `content` already carry a managed block that exports `bin_dir`?
/// This is the DETECTION half — it is what lets a re-run report "already
/// wired" instead of rewriting a file for no reason.
pub fn block_exports(content: &str, bin_dir: &str) -> bool {
    let lines: Vec<&str> = content.split('\n').collect();
    match locate_block(&lines) {
        Some((s, e)) => lines[s..=e].iter().any(|l| l.contains(bin_dir)),
        None => false,
    }
}

/// Upsert the managed block into `content`. IDEMPOTENT: an existing block is
/// replaced in place (preserving everything around it); otherwise a fresh block
/// is appended. Running this twice yields a byte-identical file.
pub fn upsert_block(content: &str, bin_dir: &str) -> String {
    let block = managed_block(bin_dir);
    let lines: Vec<&str> = content.split('\n').collect();
    match locate_block(&lines) {
        Some((start, end)) => {
            let mut out: Vec<String> = lines[..start].iter().map(|s| s.to_string()).collect();
            // `block` ends in a newline; splitting drops the trailing empty
            // element so the rejoin does not gain a blank line each pass.
            out.extend(block.trim_end_matches('\n').split('\n').map(|s| s.to_string()));
            out.extend(lines[end + 1..].iter().map(|s| s.to_string()));
            out.join("\n")
        }
        None => {
            let mut s = content.to_string();
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&block);
            s
        }
    }
}

/// Is `dir` present in a `PATH`-shaped, colon-separated string? Used to decide
/// whether the rc edit is needed at all — under Homebrew it usually is not.
pub fn path_contains(path_var: &str, dir: &str) -> bool {
    !dir.is_empty() && path_var.split(':').any(|p| p == dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIN: &str = "/home/u/.quorum/bin";

    #[test]
    fn appends_a_block_to_a_file_that_has_none() {
        let out = upsert_block("export EDITOR=vi\n", BIN);
        assert!(out.starts_with("export EDITOR=vi\n"), "{out}");
        assert!(out.contains("# >>> qd setup >>>"));
        assert!(out.contains("# <<< qd setup <<<"));
        assert!(out.contains(BIN));
        assert!(block_exports(&out, BIN));
    }

    #[test]
    fn seeds_an_empty_file_without_a_leading_blank_line() {
        let out = upsert_block("", BIN);
        assert!(out.starts_with("# >>> qd setup >>>"), "{out:?}");
    }

    #[test]
    fn re_running_is_byte_identical() {
        // The property the markers exist for: `qd setup --fix` is run
        // repeatedly and must never grow the rc file.
        let once = upsert_block("export EDITOR=vi\n", BIN);
        let twice = upsert_block(&once, BIN);
        assert_eq!(once, twice, "second pass changed the file");
        let thrice = upsert_block(&twice, BIN);
        assert_eq!(twice, thrice);
        assert_eq!(once.matches("# >>> qd setup >>>").count(), 1);
    }

    #[test]
    fn rewriting_preserves_everything_outside_the_fence() {
        let original = concat!(
            "before A\n",
            "# >>> qd setup >>>\n",
            "export PATH=\"/old/bin:$PATH\"\n",
            "# <<< qd setup <<<\n",
            "after B\n"
        );
        let out = upsert_block(original, BIN);
        assert!(out.contains("before A\n"), "{out}");
        assert!(out.contains("after B"), "{out}");
        assert!(!out.contains("/old/bin"), "stale block survived: {out}");
        assert!(out.contains(BIN));
        assert_eq!(out.matches("# >>> qd setup >>>").count(), 1);
    }

    #[test]
    fn a_re_pointed_bin_dir_is_detected_as_not_wired() {
        let out = upsert_block("", "/old/bin");
        assert!(block_exports(&out, "/old/bin"));
        assert!(!block_exports(&out, BIN), "must notice the dir changed");
    }

    #[test]
    fn a_malformed_fence_is_left_alone_and_a_good_block_is_appended() {
        // End marker before start: we do not attempt surgery on a file whose
        // fence we cannot trust — we append a well-formed block instead.
        let bad = "# <<< qd setup <<<\n# >>> qd setup >>>\n";
        let out = upsert_block(bad, BIN);
        assert!(out.starts_with(bad), "{out}");
        assert!(out.contains(BIN));
    }

    #[test]
    fn the_block_guards_against_duplicating_path_entries() {
        // Re-sourcing an rc file must not grow $PATH — the guard `qrm`'s block
        // did not have.
        let b = managed_block(BIN);
        assert!(b.contains("case \":$PATH:\""), "{b}");
        assert!(b.ends_with('\n'));
    }

    #[test]
    fn the_baked_claude_wrapper_is_not_carried_over() {
        // shell_init.rs ruled baked wrappers out (they fossilise); the init
        // line is `qd bootstrap`'s step, not this block's.
        let b = managed_block(BIN);
        assert!(!b.contains("claude()"), "{b}");
        assert!(!b.contains("qd start"), "{b}");
    }

    #[test]
    fn path_membership_is_exact_not_substring() {
        assert!(path_contains("/usr/bin:/home/u/.quorum/bin:/bin", BIN));
        assert!(!path_contains("/usr/bin:/bin", BIN));
        // A prefix match must not count: `~/.quorum/bin2` is a different dir.
        assert!(!path_contains("/home/u/.quorum/bin2:/bin", BIN));
        assert!(!path_contains("", BIN));
        assert!(!path_contains("/bin", ""));
    }
}
