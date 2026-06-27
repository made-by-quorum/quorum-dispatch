//! B3 M3b checkers — reusable, falsifiable comparators for R3(c) and R6(b).
//!
//! Owned by implementer M3b. These are the SEMANTIC-CLASS integrity checkers
//! the spec (REV 3, rows R3/R6) requires, plus the negative-control teeth in
//! R7(f)(g). Each returns `Result<(), String>` so tests can assert both the
//! pass arm (real data) and the fail arm (mutated/synthetic input).
//!
//! Why these and not byte-chasing (red-team #16): the render path always emits
//! valid UTF-8 and always terminates SGR with `m` (render.rs:62 contract:
//! `write_sgr_with_reset_to` => `\x1b[ ... m`; trailing `\x1b[0m` iff the line
//! ended non-default; width-0 continuation cells are SKIPPED so rendered bytes
//! never carry a lone continuation). So the honest falsifiable surface is:
//!   - R3(c): SGR well-formedness + UTF-8-whole CJK on the recorded History
//!     line bytes (the wire form the client replays).
//!   - R6(b): wide-char continuation invariants at the CELL level (a Screen
//!     snapshot of `Cell { width }`), which the rendered-byte path can never
//!     expose.

use qrmux::screen::Cell;

// ============================================================================
// R3(c) — styled-replay integrity on a recorded History line (raw bytes)
// ============================================================================

/// SGR well-formedness for ONE history line's raw bytes (render.rs:62 contract):
///   (1) every CSI introducer `\x1b[` is terminated by a final `m` WITHIN the
///       line (no SGR straddles a line boundary; no truncated SGR);
///   (2) trailing reset `\x1b[0m` is present IFF the line is styled (carries at
///       least one SGR sequence) — render_line appends a closing reset whenever
///       it emitted a non-default style, and emits nothing for an unstyled line.
///
/// `desc` tags the error for the gate artifact.
pub fn check_sgr_well_formed(line: &[u8], desc: &str) -> Result<(), String> {
    // Walk CSI sequences. We only model SGR (`...m`); render_line emits no other
    // CSI per line, so any non-`m` CSI final on a history line is itself a defect
    // for this contract. We accept the standard CSI param/intermediate bytes and
    // require the final byte to be `m`.
    let mut i = 0;
    let mut sgr_count = 0usize;
    let mut last_was_reset = false;
    let mut saw_any_sgr_at = None; // byte offset of first SGR, for trailing check
    while i < line.len() {
        if line[i] == 0x1b {
            // Need a `[` next for a CSI.
            if i + 1 >= line.len() || line[i + 1] != b'[' {
                return Err(format!(
                    "[{}] SGR FAIL: lone ESC (0x1b) at byte {} not followed by '[' (truncated/malformed escape)",
                    desc, i
                ));
            }
            // Scan to the final byte (0x40..=0x7e). CSI params/intermediates are
            // 0x30..=0x3f and 0x20..=0x2f.
            let mut j = i + 2;
            let mut terminated = false;
            let mut final_byte = 0u8;
            while j < line.len() {
                let b = line[j];
                if (0x40..=0x7e).contains(&b) {
                    terminated = true;
                    final_byte = b;
                    break;
                }
                // Only param/intermediate bytes are legal before the final.
                if !((0x20..=0x3f).contains(&b)) {
                    return Err(format!(
                        "[{}] SGR FAIL: illegal byte 0x{:02x} inside CSI starting at byte {}",
                        desc, b, i
                    ));
                }
                j += 1;
            }
            if !terminated {
                return Err(format!(
                    "[{}] SGR FAIL: CSI starting at byte {} not terminated before end of line (truncated SGR)",
                    desc, i
                ));
            }
            if final_byte != b'm' {
                return Err(format!(
                    "[{}] SGR FAIL: CSI starting at byte {} terminated by '{}' (0x{:02x}), expected 'm' (render_line emits only SGR per line)",
                    desc, i, final_byte as char, final_byte
                ));
            }
            sgr_count += 1;
            if saw_any_sgr_at.is_none() {
                saw_any_sgr_at = Some(i);
            }
            // Is this the canonical reset `\x1b[0m`? (params == "0")
            last_was_reset = &line[i + 2..j] == b"0";
            i = j + 1;
        } else {
            i += 1;
        }
    }

    // Trailing-reset-iff-styled. A styled line is one that carries SGR. Per the
    // render contract the LAST SGR on a styled line must be the closing reset.
    if sgr_count > 0 && !last_was_reset {
        return Err(format!(
            "[{}] SGR FAIL: styled line ({} SGR seqs) does not end with trailing reset \\x1b[0m",
            desc, sgr_count
        ));
    }
    Ok(())
}

/// CJK integrity for ONE history line's raw bytes: after the SGR codes are
/// stripped, the remaining text must be WHOLE valid UTF-8 — no truncated
/// multibyte sequence (which a split wide char would leave as a lone
/// continuation byte). Render_line skips width-0 continuation cells, so a clean
/// recording can never carry a half-width byte sequence; a checker that catches
/// the corruption is the falsifiable tooth (R7(f)).
///
/// We strip CSI sequences at the BYTE level (not via lossy UTF-8 decode, which
/// would mask the very corruption we hunt) and then require the residue to be
/// valid UTF-8.
pub fn check_cjk_integrity(line: &[u8], desc: &str) -> Result<(), String> {
    let stripped = strip_csi_bytes(line);
    match std::str::from_utf8(&stripped) {
        Ok(_) => Ok(()),
        Err(e) => Err(format!(
            "[{}] CJK FAIL: post-strip bytes are not whole UTF-8 (lone continuation / split wide char): {}",
            desc, e
        )),
    }
}

/// Strip CSI escape sequences (`\x1b[ ... <final 0x40..=0x7e>`) at the byte
/// level, leaving text bytes untouched so a truncated multibyte sequence stays
/// detectable. A lone ESC not starting a CSI is dropped (one byte) — that case
/// is already caught by `check_sgr_well_formed`.
fn strip_csi_bytes(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        if line[i] == 0x1b {
            if i + 1 < line.len() && line[i + 1] == b'[' {
                let mut j = i + 2;
                while j < line.len() && !(0x40..=0x7e).contains(&line[j]) {
                    j += 1;
                }
                // Skip through the final byte (if present).
                i = if j < line.len() { j + 1 } else { j };
            } else {
                i += 1; // lone ESC
            }
        } else {
            out.push(line[i]);
            i += 1;
        }
    }
    out
}

// ============================================================================
// R6(b) — cell-level wide-char continuation integrity
// ============================================================================

/// Cell-level orphan check over a visible-grid snapshot (one `Vec<Cell>` per
/// row). Asserts the wide-char continuation invariant:
///   - a continuation cell (`width == 0`) MUST be immediately preceded, in the
///     same row, by a wide base cell (`width == 2`) — otherwise it is a LONE
///     continuation (orphan);
///   - a wide base cell (`width == 2`) MUST be followed by a continuation cell
///     (`width == 0`) and MUST NOT be the last cell in the row — otherwise its
///     other half was lost (orphan wide base).
///
/// `desc` tags the error for the gate artifact.
pub fn check_no_orphan_wide_cell(rows: &[Vec<Cell>], desc: &str) -> Result<(), String> {
    for (y, row) in rows.iter().enumerate() {
        for (x, cell) in row.iter().enumerate() {
            match cell.width {
                0 => {
                    // Lone continuation: prev cell must be a wide base.
                    let prev_is_wide_base = x > 0 && row[x - 1].width == 2;
                    if !prev_is_wide_base {
                        return Err(format!(
                            "[{}] orphan FAIL: lone wide-continuation cell at (row {}, col {}) c={:?}; preceding cell {}",
                            desc,
                            y,
                            x,
                            cell.c,
                            if x == 0 {
                                "is row start".to_string()
                            } else {
                                format!("has width {}", row[x - 1].width)
                            }
                        ));
                    }
                }
                2 => {
                    // Wide base must have a continuation after it within the row.
                    let next_is_cont = x + 1 < row.len() && row[x + 1].width == 0;
                    if !next_is_cont {
                        return Err(format!(
                            "[{}] orphan FAIL: wide base cell at (row {}, col {}) c={:?} has no continuation ({})",
                            desc,
                            y,
                            x,
                            cell.c,
                            if x + 1 >= row.len() {
                                "at row end".to_string()
                            } else {
                                format!("next cell width {}", row[x + 1].width)
                            }
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}
