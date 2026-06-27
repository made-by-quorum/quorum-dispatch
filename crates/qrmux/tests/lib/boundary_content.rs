// Single source of truth for `check_boundary_content` (C1 M5 / carry C1b F2).
//
// This file is `include!`-spliced into TWO module scopes that cannot share a
// `use`-able symbol because of the crate-visibility boundary:
//   1. `tests/lib/assertions.rs` (the integration-test library), and
//   2. `src/screen/grid.rs`'s `#[cfg(test)] mod tests` (a src-side unit test
//      that runs the checker against a live grid read-back).
//
// Before C1 the two carried byte-level copies (B3 carry C-F2) that could drift
// silently. Unification here eliminates the drift class entirely — there is now
// one definition. `include!` is used (not `#[path] mod`) because BOTH include
// sites are modules whose on-disk dir is virtual (`grid.rs` inline `mod tests`;
// `assertions.rs` itself `#[path]`-mounted via `lib/mod.rs`), and a `#[path] mod`
// there makes rustc try to traverse a phantom dir on disk. `include!` splices
// tokens relative to the INCLUDER FILE's dir, sidestepping that.
//
// CONSEQUENCE OF `include!`: these contents land mid-module, so this header must
// be regular `//` comments (inner `//!` doc comments are illegal after items) and
// the file must contain ONLY top-level items, no inner attributes. Keep it
// dependency-free (std-only) so both sites compile it unchanged. Unification was
// preferred over a normalize-and-compare guard: the two old copies differed by
// nesting indentation + the `pub` keyword, so a literal byte-compare guard could
// not have been used directly (it would have needed brittle normalization).

/// Assert that a FIFO-evicted scrollback retained EXACTLY the most-recent window.
///
/// `retained` is the encoded line numbers still present in scrollback, IN
/// scrollback order (oldest → newest); `cap` is the scrollback limit; `written`
/// is the total number of lines scrolled out. After FIFO eviction the retained
/// set MUST be EXACTLY the most-recent window `(written - cap + 1)..=written`, in
/// order. The single window-equality below subsumes four properties at once:
///   - length == cap,
///   - the correct (recent) window is kept,
///   - it is in ascending order,
///   - the evicted prefix is absent AND no out-of-window line survives.
///
/// Teeth (R7(b)) feed it (i) an off-by-one set (cap+1 indices) and (ii) a
/// wrong-WINDOW set of the correct length (e.g. 5..=14 when 6..=15 is expected);
/// each MUST Err — which a len-only check could not catch for case (ii).
pub fn check_boundary_content(retained: &[usize], cap: usize, written: usize) -> Result<(), String> {
    if retained.len() != cap {
        return Err(format!(
            "boundary-content FAIL: retained {} lines, expected exactly cap={} (off-by-one eviction?)",
            retained.len(),
            cap
        ));
    }
    let expected: Vec<usize> = ((written.saturating_sub(cap) + 1)..=written).collect();
    if retained != expected.as_slice() {
        return Err(format!(
            "boundary-content FAIL: retained window {:?} != expected most-recent window {:?} \
             (cap={}, written={}) — wrong window, wrong order, or evicted prefix survived",
            retained, expected, cap, written
        ));
    }
    Ok(())
}
