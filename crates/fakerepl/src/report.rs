//! Report channel: one JSON object per event, newline-delimited, to
//! `$QD_FAKEREPL_REPORT` (a4-spec §5). The harness CROSS-CHECKS this against the
//! application output; the gate's turn-count oracle keys on app-output, NEVER on
//! this report (ADD-6 — the report records echo-INDEPENDENT facts only).
//!
//! Event shapes (the `event` discriminator + details):
//!   {"event":"burst","size":<n>,"paste":<bool>}
//!   {"event":"drop","size":<n>,"limit":<n>}   (tty-queue overflow, ADR 0009 mode (a))
//!   {"event":"stall_drop","admitted":<n>,"stall_ms":<t>}   (W8 reader-stall saturation)
//!   {"event":"cr","cr_kind":"in_paste"|"keystroke"|"while_busy"|"empty_noop"}
//!   {"event":"transition","status":"busy"|"idle","turn":<n>}
//!   {"event":"turn","turn":<n>,"bytes":<n>,"composer_crs":<k>}
//!   {"event":"eaten","bytes":<n>}   (ACK-1 EAT_INPUT — injection 4 consumption assert)
//!   {"event":"truncated_user_record","requested":<n>,"actual":<k>}   (ACK-1 TRUNCATE — injection 5)

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

/// Append-only JSONL sink. A no-op if `$QD_FAKEREPL_REPORT` is unset or the file
/// can't be opened (the report is a diagnostic cross-check, never load-bearing —
/// its absence must not crash the harness child).
pub struct Reporter {
    sink: Option<BufWriter<File>>,
}

impl Reporter {
    pub fn open(path: Option<&Path>) -> Self {
        let sink = path.and_then(|p| File::create(p).ok()).map(BufWriter::new);
        Self { sink }
    }

    fn emit(&mut self, value: serde_json::Value) {
        if let Some(w) = self.sink.as_mut() {
            // Flush each line so an external reader sees events promptly and a
            // crash loses at most the in-flight line.
            if serde_json::to_writer(&mut *w, &value).is_ok() {
                let _ = w.write_all(b"\n");
                let _ = w.flush();
            }
        }
    }

    pub fn burst(&mut self, size: usize, paste: bool) {
        self.emit(serde_json::json!({
            "event": "burst", "size": size, "paste": paste,
        }));
    }

    /// tty-queue OVERFLOW: a single burst of `size` bytes exceeded the model bound
    /// `limit` and was dropped wholesale (no composer content, no turn) — ADR 0009
    /// mode (a). The negative-control pairing cross-checks this against zero turns.
    pub fn drop(&mut self, size: usize, limit: usize) {
        self.emit(serde_json::json!({
            "event": "drop", "size": size, "limit": limit,
        }));
    }

    /// W8 reader-stall saturation: the stall window (`stall_ms`) elapsed having
    /// admitted at most `admitted` bytes; everything past the cap that arrived
    /// during the pause was DROPPED at the model boundary (the silent mid-loss the
    /// verify step must catch). A cross-check event; the gate keys on the composer
    /// length, not this.
    pub fn stall_drop(&mut self, admitted: usize, stall_ms: u64) {
        self.emit(serde_json::json!({
            "event": "stall_drop", "admitted": admitted, "stall_ms": stall_ms,
        }));
    }

    pub fn cr(&mut self, cr_kind: &str) {
        self.emit(serde_json::json!({
            "event": "cr", "cr_kind": cr_kind,
        }));
    }

    pub fn transition(&mut self, status: &str, turn: u32) {
        self.emit(serde_json::json!({
            "event": "transition", "status": status, "turn": turn,
        }));
    }

    pub fn turn(&mut self, turn: u32, bytes: usize, composer_crs: u32) {
        self.emit(serde_json::json!({
            "event": "turn", "turn": turn, "bytes": bytes, "composer_crs": composer_crs,
        }));
    }

    /// ACK-1 EAT_INPUT (injection 4): this burst's bytes were consumed off the
    /// PTY and discarded before the composer — the child-side consumption
    /// assert ("bytes demonstrably consumed, no anchor").
    pub fn eaten(&mut self, bytes: usize) {
        self.emit(serde_json::json!({
            "event": "eaten", "bytes": bytes,
        }));
    }

    /// ACK-1 TRUNCATE (injection 5): the user record was cut to `actual` raw
    /// bytes (requested `requested`; rounded down to a UTF-8 boundary).
    pub fn truncated_user_record(&mut self, requested: usize, actual: usize) {
        self.emit(serde_json::json!({
            "event": "truncated_user_record", "requested": requested, "actual": actual,
        }));
    }

    pub fn flush(&mut self) {
        if let Some(w) = self.sink.as_mut() {
            let _ = w.flush();
        }
    }
}
