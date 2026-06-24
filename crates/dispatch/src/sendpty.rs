//! send:pty PURE deciders (a4-spec §2.3).
//!
//! Port of `qa/hardening@3dd9f1e:src/utils.ts:289-423`: the queue-to-busy send
//! decision, the JSONL-keyed `--wait` attribution (user-record anchor + the loop
//! move), and the content-verified-CR stuck predicate. All pure (data in,
//! decision out) so the branching is unit-testable without touching real zmx/fs.
//! The `commands/` path prefix on cited comments is preserved per a prior red-team.
//!
//! ANSI stripping REUSES [`crate::boot::strip_ansi`] (boot.rs:159-247) — the W6
//! differential test below proves it agrees with the TS `stripAnsi` regex
//! semantics (utils.ts:373-383) before we adopt it as the composer stripper.

use crate::boot::strip_ansi;
use serde::Deserialize;

// --- send:pty busy/idle send decision (queue-to-busy directive) ------------
//
// VERBATIM (qa/hardening@3dd9f1e:src/utils.ts:283-301):
//  - IDLE: send + acceptance-keyed verify-then-CR (commands/submit.ts). Unchanged.
//  - BUSY: send (same zmx send + "\r"), but SKIP verify-then-CR entirely — that
//    discipline's own contract is "never CR a busy session", and its acceptance
//    check keys on idle→busy, which can't fire when we START busy. Report
//    "queued", not "sent"; no "did not go busy" warning (it IS busy).
//
// --wait used to refuse a busy session because the next idle couldn't be
// attributed to our message. That reasoning is SUPERSEDED: --wait now anchors on
// the JSONL — it waits for our message to appear as a user record (the queue
// draining) and reads the assistant response that FOLLOWS it (see findUserAnchor
// / decideWait below + commands/send.ts). Attribution is keyed on the record, not
// on busy/idle cycles, so --wait works on busy sessions too.

/// What `send:pty` does given the session status
/// (qa/hardening@3dd9f1e:src/utils.ts:302 `SendPtyAction`). NO refusal case — the
/// ruling: sending is always safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendPtyAction {
    /// idle path: send + verify-then-CR.
    SendVerify,
    /// busy path: send (no CR), report "queued".
    SendQueue,
}

/// PURE: busy → queue, anything else (idle/shell) → verify. Port of
/// `decideSendPty` (qa/hardening@3dd9f1e:src/utils.ts:303-305).
pub fn decide_send_pty(status: &str) -> SendPtyAction {
    if status == "busy" {
        SendPtyAction::SendQueue
    } else {
        SendPtyAction::SendVerify
    }
}

// --- send:pty --wait JSONL anchor ------------------------------------------

/// A JSONL conversation record. Only the fields we key on are typed; the live
/// records carry much more. Shape matches what session.ts already parses
/// (`type: "user"` with `message.content` either a string or a content-block
/// array containing a `{ type: "text", text }` block). Port of `JsonlRecord`
/// (qa/hardening@3dd9f1e:src/utils.ts:312-315). Permissive (L8): unknown fields
/// are ignored, missing fields default to None.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct JsonlRecord {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub message: Option<JsonlMessage>,
}

/// The `message` sub-object of a [`JsonlRecord`]. `content` is untyped (string OR
/// content-block array) — [`user_record_text`] inspects it permissively.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct JsonlMessage {
    #[serde(default)]
    pub content: Option<serde_json::Value>,
}

/// Extract the user-typed text of a record, or `None` if it is not a user record
/// carrying text. Port of `userRecordText`
/// (qa/hardening@3dd9f1e:src/utils.ts:317-339).
///
/// ---------------------------------------------------------------------------
/// VERBATIM (utils.ts:307-316 doc-comment):
/// Mirrors session.ts's user-preview extraction: content is either a bare string
/// or a content-block array whose first text block holds the prompt. Returns
/// undefined for non-user records — so an assistant turn that happens to echo the
/// message text can NEVER false-anchor (we only match the user record claude
/// writes when it takes the message up).
/// ---------------------------------------------------------------------------
pub fn user_record_text(rec: &JsonlRecord) -> Option<String> {
    if rec.r#type.as_deref() != Some("user") {
        return None;
    }
    let content = rec.message.as_ref()?.content.as_ref()?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        // First text block whose `text` is a string (TS `.find(b => b &&
        // b.type === "text" && typeof b.text === "string")`).
        for b in arr {
            let is_text = b.get("type").and_then(|t| t.as_str()) == Some("text");
            if is_text {
                if let Some(text) = b.get("text").and_then(|t| t.as_str()) {
                    return Some(text.to_string());
                }
            }
        }
    }
    None
}

/// Find the index of OUR message in a list of parsed records: the first user
/// record (in file order) whose text equals the sent message. Port of
/// `findUserAnchor` (qa/hardening@3dd9f1e:src/utils.ts:341-352).
///
/// ---------------------------------------------------------------------------
/// VERBATIM (utils.ts:341-347 doc-comment):
/// "First past the start offset" is the caller's responsibility (it only parses
/// records written after startOffset), so identical text sent twice anchors on
/// the correct occurrence by construction. Returns -1 if our message has not
/// appeared yet — for a busy session that means the queue has not drained to us.
/// ---------------------------------------------------------------------------
///
/// Rust returns `Option<usize>` (`None` for the TS `-1`).
pub fn find_user_anchor(records: &[JsonlRecord], message: &str) -> Option<usize> {
    records
        .iter()
        .position(|r| user_record_text(r).as_deref() == Some(message))
}

/// The `--wait` loop's next move. Port of `WaitDecision`
/// (qa/hardening@3dd9f1e:src/utils.ts:354).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitDecision {
    /// Our user record has not appeared yet (queue not drained to us).
    Waiting,
    /// Our record appeared but the session is still busy responding.
    Collecting,
    /// Our record appeared AND the session is back to idle — response finished.
    Complete,
}

/// PURE decision from `(anchor_found × status)`. Port of `decideWait`
/// (qa/hardening@3dd9f1e:src/utils.ts:354-365).
///
/// ---------------------------------------------------------------------------
/// VERBATIM (utils.ts:354-360 doc-comment):
/// Decide the --wait loop's next move from the current observations. PURE so the
/// (anchor-found × status) matrix is unit-testable without real fs/pid polling.
///  - "waiting": our user record has not appeared yet (queue not drained to us).
///  - "collecting": our record appeared but the session is still busy responding.
///  - "complete": our record appeared AND the session is back to idle — the
///    response that follows the anchor is finished. Idle is only a COMPLETION
///    signal here; attribution is already anchored by the record, never by idle.
/// ---------------------------------------------------------------------------
///
/// `status` is `Option` (TS `string | undefined`): an unreadable status is NEVER
/// read as completion (utils.ts:363-364 `status === "idle"` — undefined ≠ idle).
pub fn decide_wait(anchor_found: bool, status: Option<&str>) -> WaitDecision {
    if !anchor_found {
        return WaitDecision::Waiting;
    }
    if status == Some("idle") {
        WaitDecision::Complete
    } else {
        WaitDecision::Collecting
    }
}

// --- send:pty queued-send stuck detection (content-verified CR) ------------
//
// VERBATIM (qa/hardening@3dd9f1e:src/utils.ts:367-378):
// A busy-session queued send delivers as two writes (text, then "\r" alone) to
// mimic a human keystroke. If even that leaves the message unsubmitted in the
// composer (paste-burst still absorbing the \r), we remediate — but ONLY when we
// can SEE our text sitting unsubmitted. This is the predicate for that decision:
// does the session's screen still hold OUR message in the composer?
//
// Why this is safe under the revised contract (see commands/send.ts): we never
// BLIND-CR a busy session — every remediation CR is conditional on this returning
// true, i.e. on our exact text being visibly present and unsubmitted, so the CR
// can only submit OUR message. If it already queued, the composer is empty, this
// returns false, and no CR is emitted.

/// The live composer prompt glyph (`❯`) — the region after the LAST one is the
/// composer (scrollback quotes appear before it).
const PROMPT_GLYPH: char = '\u{276f}'; // ❯

/// Collapse ALL whitespace runs to single spaces and trim — so a composer that
/// WRAPS a long message across lines still matches the one-line needle. Port of
/// `normalizeWs` (qa/hardening@3dd9f1e:src/utils.ts:387-389,
/// `s.replace(/\s+/g, " ").trim()`).
pub fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            in_ws = true;
        } else {
            if in_ws && !out.is_empty() {
                out.push(' ');
            }
            in_ws = false;
            out.push(ch);
        }
    }
    out
}

/// Does the session's screen still hold OUR message unsubmitted in the composer?
/// Port of `composerHoldsMessage` (qa/hardening@3dd9f1e:src/utils.ts:391-423).
///
/// ---------------------------------------------------------------------------
/// VERBATIM (utils.ts:391-405 doc-comment):
/// Anchored to the region AFTER the LAST prompt glyph (❯) in the dump: that glyph
/// marks the live composer prompt, so a processed user turn quoted in scrollback
/// (which appears BEFORE the last ❯) can never false-positive as "still unsent".
/// If no glyph is present we fall back to scanning the whole dump (conservative:
/// a false "stuck" only costs one extra content-verified CR, never a blind one).
///
/// Both haystack and needle are ANSI-stripped and whitespace-normalized, so a
/// message the composer wrapped across several lines still matches.
/// ---------------------------------------------------------------------------
pub fn composer_holds_message(screen_text: &str, message: &str) -> bool {
    let needle = normalize_ws(&strip_ansi(message));
    if needle.is_empty() {
        return false;
    }
    let clean = strip_ansi(screen_text);
    // Region after the LAST ❯ glyph; no glyph → whole dump (TS
    // `lastIndexOf("❯")` / `slice(lastPrompt)` else whole `clean`).
    let region = match clean.rfind(PROMPT_GLYPH) {
        Some(idx) => &clean[idx..],
        None => &clean,
    };
    normalize_ws(region).contains(&needle)
}

// ===========================================================================
// send:pty --wait JSONL anchor LOOP + response extraction (a4-spec §3.1 steps
// 7-8). Pure over injected deps so the decide_wait-driven loop is unit-testable
// without real 500ms sleeps / real fs (spec §3.1 test note: "seam the sleep").
// The bin layer (verbs/send.rs) wires the real fs/clock/stderr.
// ===========================================================================

/// A single JSONL line parsed ONCE: the raw text (for `--raw`) paired with its
/// `serde_json::Value` (for extraction) — the N10 index-alignment unit. A line
/// that fails to parse carries `Value::Null` (the TS `{} as JsonlRecord` empty
/// record), so anchor + extraction stay index-aligned by construction.
#[derive(Debug, Clone)]
pub struct ParsedLine {
    pub raw: String,
    pub value: serde_json::Value,
}

/// Parse a JSONL slice (the bytes AFTER `start_offset`) into index-aligned
/// [`ParsedLine`]s: `split('\n')` → drop blank lines → parse-or-empty. This is
/// the SINGLE pipeline both the anchor and the extraction read from (N10): TS
/// relies on identical `slice → split → filter` filtering for `anchor_idx + 1`
/// to line up (send.ts:331-336); doing it once here is structurally immune.
pub fn parse_jsonl_slice(slice: &str) -> Vec<ParsedLine> {
    slice
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(|l| ParsedLine {
            raw: l.to_string(),
            // TS `try { JSON.parse(l) } catch { return {} }` — a bad line is an
            // empty record (permissive, L8). We use Null; the JsonlRecord view
            // deserializes Null → default (non-anchoring), and the extractor
            // skips a non-object value.
            value: serde_json::from_str(l).unwrap_or(serde_json::Value::Null),
        })
        .collect()
}

/// The [`JsonlRecord`] view of a [`ParsedLine`] (for the anchor). A non-object /
/// unparsed line yields `JsonlRecord::default()` (TS empty record) — never
/// anchors.
fn record_view(p: &ParsedLine) -> JsonlRecord {
    serde_json::from_value(p.value.clone()).unwrap_or_default()
}

/// Find OUR message's anchor index among parsed lines (the [`find_user_anchor`]
/// contract, applied to the record-view of each line).
pub fn find_anchor(lines: &[ParsedLine], message: &str) -> Option<usize> {
    lines
        .iter()
        .position(|p| user_record_text(&record_view(p)).as_deref() == Some(message))
}

/// Effects the `--wait` loop needs, injected so the decide_wait matrix + the
/// died/timeout branches are unit-testable without real fs/clock/sleep.
pub trait WaitDeps {
    /// Read the JSONL slice past `start_offset` and parse it (the bin wires the
    /// real `read(jsonl)[start_offset..]` → [`parse_jsonl_slice`]).
    ///
    /// `Err(reason)` = the source lost its integrity (ADD-8 W5): the file is now
    /// SHORTER than `start_offset` (truncated/rotated mid-wait) or the offset no
    /// longer falls on a char boundary. The loop fails LOUD on it — silently
    /// re-scanning from byte 0 could anchor on an EARLIER identical message,
    /// defeating the first-past-offset guarantee (`find_user_anchor` doc).
    fn read_lines(&self) -> Result<Vec<ParsedLine>, String>;
    /// Read the session status from the pid file. `None` → the file could not be
    /// read/parsed (TS `catch` → "session died", send.ts:236-241).
    fn read_status(&self) -> Option<String>;
    /// Sleep `ms` (the 500ms poll; seamed so tests run instantly).
    fn sleep(&self, ms: u64);
    /// Monotonic-ish clock in ms (the timeout deadline).
    fn now_ms(&self) -> i64;
    /// Emit a progress glyph to stderr (':' waiting, '.' collecting; TS
    /// send.ts:243). Seamed so tests can assert the glyph sequence if wanted.
    fn progress(&self, glyph: char);
}

/// The `--wait` loop's terminal outcome (a4-spec §3.1 step 7). The bin maps each
/// to its stderr/exit + (for `Complete`) runs extraction over `lines`.
#[derive(Debug)]
pub enum WaitOutcome {
    /// `decide_wait` reached `Complete`: the response that follows our anchor is
    /// finished. Carries the final parsed lines + the anchor index for extraction.
    Complete {
        lines: Vec<ParsedLine>,
        anchor: Option<usize>,
    },
    /// The pid status became unreadable mid-wait → "session died" / "Session
    /// exited while waiting for response." exit 1 (send.ts:236-241).
    Died,
    /// The timeout elapsed. `anchored` distinguishes the two TS messages
    /// (send.ts:251-264): anchored → "Timed out waiting for response."; un-
    /// anchored → the still-queued wording. Both exit 1.
    TimedOut { anchored: bool },
    /// The JSONL source lost its integrity mid-wait (shrank/rotated past the
    /// start offset — ADD-8 W5). NEVER silently re-anchored; the bin prints the
    /// reason + exits 1.
    SourceError(String),
}

/// The send:pty `--wait` JSONL-anchor loop (a4-spec §3.1 step 7), PURE over
/// [`WaitDeps`]. Port of the loop in send.ts:217-265.
///
/// Each iteration: re-read+parse the slice, (re)find the anchor, read pid status
/// (unreadable → [`WaitOutcome::Died`]), then `decide_wait(anchored, status)`:
/// `complete` → break with the final lines; `collecting` → '.', `waiting` → ':';
/// sleep `poll_ms`. On deadline → [`WaitOutcome::TimedOut`] keyed on whether we
/// ever anchored. The anchor index, once found, is STICKY (TS `if (anchorIdx <
/// 0) anchorIdx = ...`) so a later re-parse can't lose it.
///
/// ## B2 item 3 — the snapshot POSTDATES the completion observation
/// The transcript and the status file have INDEPENDENT writers; nothing orders
/// "final assistant rows flushed" before "status reads idle". The pre-fix loop
/// returned the lines snapshot taken BEFORE the status read that observed
/// idle, so a reply landing in that window (or a row still mid-flush) yielded
/// an EMPTY capture while the reply sat in the file — the diagnosed item-3
/// field symptom (punch_b2_item3_repro.rs). On `Complete` the loop now
/// RE-READS the slice until QUIESCENT (see [`settle_snapshot`]), so the
/// returned snapshot postdates the idle observation — the binding invariant —
/// and a still-flushing transcript gets a bounded window to finish. The anchor
/// index stays valid (the slice is append-only; a shrink fails loud in
/// read_lines).
pub fn run_wait_loop(
    deps: &dyn WaitDeps,
    message: &str,
    timeout_ms: i64,
    poll_ms: u64,
) -> WaitOutcome {
    let start = deps.now_ms();
    let mut anchor: Option<usize> = None;

    while deps.now_ms() - start < timeout_ms {
        let lines = match deps.read_lines() {
            Ok(l) => l,
            // W5: a shrunk/rotated source fails LOUD — re-scanning from byte 0
            // could anchor on an earlier identical message (silent wrong-anchor).
            Err(reason) => return WaitOutcome::SourceError(reason),
        };
        if anchor.is_none() {
            anchor = find_anchor(&lines, message);
        }

        let status = match deps.read_status() {
            Some(s) => s,
            None => return WaitOutcome::Died,
        };

        match decide_wait(anchor.is_some(), Some(status.as_str())) {
            // B2 item 3: postdate + settle the snapshot (doc block above).
            WaitDecision::Complete => {
                return match settle_snapshot(deps, lines, poll_ms) {
                    Ok(snap) => WaitOutcome::Complete {
                        lines: snap,
                        anchor,
                    },
                    Err(reason) => WaitOutcome::SourceError(reason),
                };
            }
            WaitDecision::Collecting => deps.progress('.'),
            WaitDecision::Waiting => deps.progress(':'),
        }
        deps.sleep(poll_ms);
    }

    WaitOutcome::TimedOut {
        anchored: anchor.is_some(),
    }
}

/// Bound on the post-completion settle re-reads (B2 item 3). Each attempt is
/// one settle-delay + one slice read; the common case exits on the FIRST
/// attempt (the file was already quiescent). 5 attempts × poll_ms/4 (125ms at
/// the production 500ms poll) ≈ 625ms worst-case added latency — paid only
/// while the transcript keeps changing under us.
const MAX_SETTLE_READS: u32 = 5;

/// B2 item 3: re-read the slice until QUIESCENT — two consecutive reads, one
/// settle-delay apart, with identical content — starting from the
/// pre-observation `lines`. Always sleeps at least once, so the returned
/// snapshot strictly POSTDATES the completion observation AND survived one
/// settle-delay unchanged (a row mid-flush at the idle flip gets a bounded
/// window to land). Exhausting the bound returns the LAST read — the
/// loud-empty-capture belt downstream ([`capture_or_defect`]) names anything
/// still missing rather than ever reporting empty-as-success.
fn settle_snapshot(
    deps: &dyn WaitDeps,
    lines: Vec<ParsedLine>,
    poll_ms: u64,
) -> Result<Vec<ParsedLine>, String> {
    let settle_ms = (poll_ms / 4).max(1);
    let mut prev = lines;
    for _ in 0..MAX_SETTLE_READS {
        deps.sleep(settle_ms);
        let fresh = deps.read_lines()?;
        let quiescent =
            fresh.len() == prev.len() && fresh.iter().zip(&prev).all(|(a, b)| a.raw == b.raw);
        prev = fresh;
        if quiescent {
            break;
        }
    }
    Ok(prev)
}

/// The Default-mode empty-extraction sentinel (send.ts:329 wording). ONE const
/// shared by the producer ([`extract_response`]) and the consumer
/// ([`capture_or_defect`], whose string-compare is LOAD-BEARING for the B2
/// item-3 never-empty-as-success invariant) — a drift between two copies of
/// the literal would silently regress item 3 back to empty-as-success.
pub const NO_TEXT_RESPONSE: &str = "(no text response)";

/// What to extract from the post-anchor lines (a4-spec §3.1 step 8 / send.ts:
/// 280-330). The bin chooses the mode from `--raw`/`--full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractMode {
    /// `--raw`: dump the raw JSONL lines verbatim, one per line.
    Raw,
    /// default: assistant text blocks where `stop_reason == "end_turn"`, joined
    /// "\n\n"; empty → [`NO_TEXT_RESPONSE`].
    Default,
    /// `--full`: ALL assistant text + `[thinking] …` + `[tool: name] <input≤200>`,
    /// joined "\n\n".
    Full,
}

/// Extract the response from the parsed lines, taking only those AFTER the anchor
/// (`anchor + 1 ..`; no anchor → all lines, TS `anchorIdx >= 0 ? slice(anchorIdx
/// + 1) : allLines`). Port of send.ts:280-330. Returns the string the bin prints.
pub fn extract_response(lines: &[ParsedLine], anchor: Option<usize>, mode: ExtractMode) -> String {
    let new_lines: &[ParsedLine] = match anchor {
        Some(i) => lines.get(i + 1..).unwrap_or(&[]),
        None => lines,
    };

    if mode == ExtractMode::Raw {
        // Each raw line, newline-joined (the bin `for line { println!(line) }`).
        return new_lines
            .iter()
            .map(|p| p.raw.as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }

    let mut text_blocks: Vec<String> = Vec::new();
    let mut all_blocks: Vec<String> = Vec::new();

    for p in new_lines {
        let obj = &p.value;
        if obj.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = obj.get("message").and_then(|m| m.get("content")) else {
            continue;
        };
        // TS: Array.isArray(content) ? content : [content].
        let blocks: Vec<&serde_json::Value> = match content.as_array() {
            Some(arr) => arr.iter().collect(),
            None => vec![content],
        };
        let stop_reason = obj
            .get("message")
            .and_then(|m| m.get("stop_reason"))
            .and_then(|v| v.as_str());
        for b in blocks {
            let btype = b.get("type").and_then(|v| v.as_str());
            if btype == Some("text") {
                if let Some(text) = b.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        if stop_reason == Some("end_turn") {
                            text_blocks.push(text.to_string());
                        }
                        all_blocks.push(text.to_string());
                    }
                }
            }
            if mode == ExtractMode::Full && btype == Some("thinking") {
                if let Some(t) = b.get("thinking").and_then(|v| v.as_str()) {
                    all_blocks.push(format!("[thinking] {t}"));
                }
            }
            if mode == ExtractMode::Full && btype == Some("tool_use") {
                let tool_name = b.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                let input = b.get("input").cloned().unwrap_or(serde_json::Value::Null);
                let input_str = serde_json::to_string(&input).unwrap_or_default();
                let truncated: String = input_str.chars().take(200).collect();
                all_blocks.push(format!("[tool: {tool_name}] {truncated}"));
            }
        }
    }

    if mode == ExtractMode::Full {
        all_blocks.join("\n\n")
    } else {
        let joined = text_blocks.join("\n\n");
        if joined.is_empty() {
            NO_TEXT_RESPONSE.to_string()
        } else {
            joined
        }
    }
}

/// B2 item 3 — the binding `--wait` capture invariant: NEVER return an empty
/// capture as success. `Ok(body)` carries real captured content; `Err(observed)`
/// names exactly what was observed (observations, not inferences) so the bin
/// can fail LOUD (non-zero exit) instead of printing emptiness with exit 0.
///
/// "Empty" per mode:
/// - `Default`: the [`NO_TEXT_RESPONSE`] sentinel (no end_turn assistant
///   text after the anchor);
/// - `Full` / `Raw`: the empty string (nothing extracted at all — the old bin
///   printed it, or printed nothing, and exited 0).
///
/// SANCTIONED DIVERGENCE from the TS surface (phase-2 ruling): TS printed
/// "(no text response)" / nothing and exited 0; an anchored Complete with an
/// empty extraction is a DEFECT SIGNAL — the transcript may have still been
/// flushing, or the turn genuinely produced no text — and the caller must be
/// told loudly either way, never handed empty-as-success.
///
/// ## ACCEPTED RESIDUAL (red-team round 1, W4 — adjudicated DOCUMENT branch)
/// `Full`/`Raw` with a NON-empty extraction can still return PARTIAL turn
/// content as success when the turn's tail rows stall past the settle window
/// (the captured content is real, just possibly incomplete). The proposed
/// belt — "no end_turn row after the anchor = defect" — was checked against
/// real transcripts and REJECTED on the evidence: 256 of 3059 complete turns
/// (~8.4%, 40-transcript corpus, 2026-06-11) legitimately carry NO end_turn
/// assistant row (tool_use-only tails, `stop_sequence` terminals, one
/// `refusal`), so the belt would loudly fail genuine captures. The settle
/// re-read ([`run_wait_loop`]'s quiescence loop) remains the mitigation;
/// anything it misses is bounded staleness, never an EMPTY-as-success.
pub fn capture_or_defect(
    lines: &[ParsedLine],
    anchor: Option<usize>,
    mode: ExtractMode,
) -> Result<String, String> {
    let body = extract_response(lines, anchor, mode);
    let empty = match mode {
        ExtractMode::Default => body == NO_TEXT_RESPONSE,
        ExtractMode::Full | ExtractMode::Raw => body.is_empty(),
    };
    if !empty {
        return Ok(body);
    }
    // Observations only: what we read, where the anchor sat, what was missing.
    let after_anchor = match anchor {
        Some(i) => lines.len().saturating_sub(i + 1),
        None => lines.len(),
    };
    Err(format!(
        "the response turn completed (anchored, session idle) but extraction found no \
         response content: {after_anchor} transcript line(s) after the anchor, none \
         yielding {what}",
        what = match mode {
            ExtractMode::Default => "assistant end_turn text",
            ExtractMode::Full => "assistant text/thinking/tool blocks",
            ExtractMode::Raw => "raw lines",
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(v: serde_json::Value) -> JsonlRecord {
        serde_json::from_value(v).unwrap()
    }
    fn user_str(text: &str) -> JsonlRecord {
        rec(json!({"type": "user", "message": {"content": text}}))
    }
    fn user_blocks(text: &str) -> JsonlRecord {
        rec(json!({"type": "user", "message": {"content": [{"type": "text", "text": text}]}}))
    }
    fn assistant(text: &str) -> JsonlRecord {
        rec(json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}))
    }

    // --- decide_send_pty (ported + full matrix) ---------------------------

    #[test]
    fn decide_send_pty_idle_is_verify() {
        assert_eq!(decide_send_pty("idle"), SendPtyAction::SendVerify);
    }

    #[test]
    fn decide_send_pty_busy_is_queue() {
        assert_eq!(decide_send_pty("busy"), SendPtyAction::SendQueue);
    }

    #[test]
    fn decide_send_pty_full_matrix_only_busy_queues() {
        // a4-spec D: decide_send_pty × all statuses. ONLY "busy" queues; every
        // other status (incl. "shell", unknown) takes the verify path — NO refusal
        // case (the ruling).
        for s in ["idle", "shell", "starting", "dead", "", "BUSY", "weird"] {
            assert_eq!(
                decide_send_pty(s),
                SendPtyAction::SendVerify,
                "status {s:?} must NOT queue (only exact \"busy\" does)"
            );
        }
        assert_eq!(decide_send_pty("busy"), SendPtyAction::SendQueue);
    }

    // --- user_record_text / find_user_anchor (ported + additions) ---------

    #[test]
    fn anchors_on_user_string_content() {
        let recs = [
            assistant("earlier output"),
            user_str("ping"),
            assistant("pong"),
        ];
        assert_eq!(find_user_anchor(&recs, "ping"), Some(1));
    }

    #[test]
    fn anchors_on_content_block_user_record() {
        let recs = [user_blocks("hello there")];
        assert_eq!(find_user_anchor(&recs, "hello there"), Some(0));
    }

    #[test]
    fn assistant_echo_never_false_anchors() {
        // An assistant turn quoting our text must not be mistaken for the user
        // record. user_record_text returns None for non-user records.
        let recs = [assistant("you said: ping"), user_str("ping")];
        assert_eq!(find_user_anchor(&recs, "ping"), Some(1));
        // Direct: the assistant record extracts to None.
        assert_eq!(user_record_text(&assistant("ping")), None);
    }

    #[test]
    fn identical_text_twice_first_past_offset_wins() {
        // The caller only parses records past startOffset, so "first in this
        // slice" is the correct occurrence by construction.
        let recs = [user_str("dup"), assistant("a"), user_str("dup")];
        assert_eq!(find_user_anchor(&recs, "dup"), Some(0));
    }

    #[test]
    fn not_yet_present_is_none() {
        let recs = [assistant("busy with prior task")];
        assert_eq!(find_user_anchor(&recs, "ping"), None);
    }

    #[test]
    fn bad_json_lines_skipped_permissively() {
        // a4-spec D: bad-JSON lines skipped permissively (L8). The caller parses
        // line-by-line; a malformed line yields a default (empty) record that
        // extracts to None and never anchors. Model that here: a default record
        // interleaved among good ones is simply non-matching.
        let recs = [
            JsonlRecord::default(), // a bad line parsed to empty
            user_str("real message"),
        ];
        assert_eq!(user_record_text(&JsonlRecord::default()), None);
        assert_eq!(find_user_anchor(&recs, "real message"), Some(1));
    }

    #[test]
    fn user_record_with_non_string_content_is_none() {
        // content present but neither a string nor a text-block array → None.
        let r = rec(json!({"type": "user", "message": {"content": 42}}));
        assert_eq!(user_record_text(&r), None);
        // content-block array with only a non-text block → None.
        let r2 = rec(json!({"type": "user", "message": {"content": [{"type": "image"}]}}));
        assert_eq!(user_record_text(&r2), None);
        // user record with no message → None.
        let r3 = rec(json!({"type": "user"}));
        assert_eq!(user_record_text(&r3), None);
    }

    // --- decide_wait (ported + full matrix) -------------------------------

    #[test]
    fn decide_wait_no_anchor_is_waiting_regardless_of_status() {
        assert_eq!(decide_wait(false, Some("busy")), WaitDecision::Waiting);
        assert_eq!(decide_wait(false, Some("idle")), WaitDecision::Waiting);
        assert_eq!(decide_wait(false, None), WaitDecision::Waiting);
    }

    #[test]
    fn decide_wait_anchored_busy_is_collecting() {
        assert_eq!(decide_wait(true, Some("busy")), WaitDecision::Collecting);
    }

    #[test]
    fn decide_wait_anchored_idle_is_complete() {
        assert_eq!(decide_wait(true, Some("idle")), WaitDecision::Complete);
    }

    #[test]
    fn decide_wait_anchored_unreadable_is_collecting_not_complete() {
        // A missing/unreadable status must NEVER be read as completion.
        assert_eq!(decide_wait(true, None), WaitDecision::Collecting);
    }

    #[test]
    fn decide_wait_full_matrix() {
        // a4-spec D: decide_wait full matrix. anchor ∈ {false,true} × status ∈
        // {idle, busy, shell, None}. Idle is completion ONLY when anchored.
        use WaitDecision::*;
        let cases: &[(bool, Option<&str>, WaitDecision)] = &[
            (false, Some("idle"), Waiting),
            (false, Some("busy"), Waiting),
            (false, Some("shell"), Waiting),
            (false, None, Waiting),
            (true, Some("idle"), Complete),
            (true, Some("busy"), Collecting),
            (true, Some("shell"), Collecting),
            (true, None, Collecting),
        ];
        for (anchor, status, want) in cases {
            assert_eq!(
                decide_wait(*anchor, *status),
                *want,
                "anchor={anchor} status={status:?}"
            );
        }
    }

    // --- normalize_ws -----------------------------------------------------

    #[test]
    fn normalize_ws_collapses_and_trims() {
        assert_eq!(normalize_ws("  a \n\t b   c \n"), "a b c");
        assert_eq!(normalize_ws(""), "");
        assert_eq!(normalize_ws("   "), "");
        assert_eq!(normalize_ws("solid"), "solid");
    }

    // --- composer_holds_message (ported + additions) ----------------------

    const ESC: char = '\u{1b}';

    /// A screen dump: scrollback, then the live composer prompt line (TS test
    /// helper `screen`).
    fn screen(composer: &str, scrollback: &str) -> String {
        format!("{scrollback}\n\u{276f} {composer}\n")
    }

    #[test]
    fn composer_exact_match() {
        assert!(composer_holds_message(
            &screen("SMOKE4: hello there", ""),
            "SMOKE4: hello there"
        ));
    }

    #[test]
    fn composer_wrapped_across_lines_matches() {
        // The composer wraps a long message; whitespace-normalization collapses
        // the injected newlines so the one-line needle still matches.
        let wrapped = "this is a very long\nmessage that the composer\nwrapped across lines";
        assert!(composer_holds_message(
            &screen(wrapped, ""),
            "this is a very long message that the composer wrapped across lines"
        ));
    }

    #[test]
    fn composer_ansi_decorated_matches_after_strip() {
        let decorated = format!("{ESC}[2m{ESC}[36mqueued text{ESC}[0m");
        assert!(composer_holds_message(
            &screen(&decorated, ""),
            "queued text"
        ));
    }

    #[test]
    fn composer_absent_empty_composer_is_false() {
        assert!(!composer_holds_message(
            &screen("", ""),
            "SMOKE4: hello there"
        ));
    }

    #[test]
    fn composer_processed_turn_in_scrollback_does_not_false_positive() {
        // The same text appears as an ALREADY-PROCESSED turn (before the last ❯),
        // and the composer is now empty. Anchoring to the region after the LAST ❯
        // excludes scrollback, so this is correctly NOT stuck.
        let scrollback = "\u{276f} SMOKE4: hello there\n  (assistant replied...)";
        assert!(!composer_holds_message(
            &screen("", scrollback),
            "SMOKE4: hello there"
        ));
    }

    #[test]
    fn composer_empty_needle_is_false() {
        assert!(!composer_holds_message(&screen("", ""), ""));
        // A needle that is only whitespace also normalizes to empty → false.
        assert!(!composer_holds_message(&screen("anything", ""), "   "));
    }

    #[test]
    fn composer_no_glyph_falls_back_to_whole_dump() {
        // No ❯ anywhere → scan the whole dump (conservative). Our text present →
        // true.
        assert!(composer_holds_message(
            "plain dump with queued text in it",
            "queued text"
        ));
        assert!(!composer_holds_message(
            "plain dump without it",
            "queued text"
        ));
    }

    #[test]
    fn composer_unicode_message_matches() {
        // a4-spec D: Unicode. A multibyte message survives strip+normalize and
        // matches in the composer region.
        let msg = "café ☕ 日本語 — naïve";
        assert!(composer_holds_message(&screen(msg, ""), msg));
        // And it does NOT match when only present in scrollback (empty composer).
        let scrollback = format!("\u{276f} {msg}\n (replied)");
        assert!(!composer_holds_message(&screen("", &scrollback), msg));
    }

    // --- W6 differential: boot::strip_ansi vs TS stripAnsi semantics ------
    //
    // a4-spec §2.3 W6 (MANDATORY): before adopting boot::strip_ansi as the
    // composer stripper, run it against the TS stripAnsi regex semantics
    // (utils.ts:373-383) on the input classes the regex targets — OSC title
    // sequences (ESC ] 0;… BEL and ESC ] …ST), CSI color/cursor runs, and lone
    // ESC. `ts_strip_ansi_reference` is a faithful port of the THREE TS regexes,
    // used as the oracle.
    //
    // RESULT (flagged to the lead — do NOT "resolve" this silently): boot and TS
    // AGREE on every CSI and OSC class (the only escapes claude's TUI emits in a
    // composer dump — verified by the boot-corpus tests + the cases below). They
    // DIVERGE on the bare lone-ESC class: TS's `\x1b[@-_]?` consumes the next byte
    // ONLY when it is in 0x40..=0x5f, whereas boot's catch-all 2-byte rule
    // (boot.rs:215-219) consumes ESC + ANY following byte. So
    // `strip_ansi("plain\x1b=more")` is boot→"plainmore" vs TS→"plain=more"
    // (`=` is 0x3d), and `"\x1b xtext"`-style sequences differ likewise.
    //
    // We do NOT extend boot::strip_ansi to the narrower TS rule: boot's existing
    // `strip_ansi_charset_keypad_and_lone_esc` test (boot.rs:693-697) ASSERTS the
    // wider behavior (it strips `\x1b=` whole — keypad-mode is real claude TUI
    // output), so narrowing would regress a sanctioned boot invariant. The two
    // strippers serve different corpora with a deliberate, now-DOCUMENTED edge
    // difference that is inert for the composer (a composer dump carries no bare
    // `ESC <0x20-0x3f/0x60-0x7e>` runs — those bytes only appear inside the CSI/OSC
    // payloads both strip identically). The `w6_*` tests below pin BOTH the
    // agreement (CSI/OSC) and the documented divergence (lone-ESC) so a future
    // change to either stripper is caught. ESCALATION: this is a flagged semantic
    // edge for the lead, not a silent adoption.

    /// Reference port of the TS `stripAnsi` regexes (utils.ts:373-383), applied in
    /// the same order:
    ///   1. CSI:  \x1b\[[0-9;?]*[ -/]*[@-~]
    ///   2. OSC:  \x1b\][^\x07\x1b]*(?:\x07|\x1b\\)
    ///   3. lone: \x1b[@-_]?
    ///
    /// Hand-rolled (no regex dep) but faithful to the regex semantics.
    fn ts_strip_ansi_reference(s: &str) -> String {
        let b = s.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == 0x1b {
                // Try CSI: ESC [ [0-9;?]* [ -/]* [@-~]
                if b.get(i + 1) == Some(&b'[') {
                    let mut j = i + 2;
                    while j < b.len() && matches!(b[j], b'0'..=b'9' | b';' | b'?') {
                        j += 1;
                    }
                    while j < b.len() && (0x20..=0x2f).contains(&b[j]) {
                        j += 1;
                    }
                    if j < b.len() && (0x40..=0x7e).contains(&b[j]) {
                        i = j + 1; // consumed CSI incl. final byte
                        continue;
                    }
                    // No final byte → falls through to the lone-ESC rule.
                }
                // Try OSC: ESC ] [^BEL,ESC]* (BEL | ESC \)
                if b.get(i + 1) == Some(&b']') {
                    let mut j = i + 2;
                    while j < b.len() && b[j] != 0x07 && b[j] != 0x1b {
                        j += 1;
                    }
                    if j < b.len() && b[j] == 0x07 {
                        i = j + 1; // BEL-terminated
                        continue;
                    }
                    if j + 1 < b.len() && b[j] == 0x1b && b[j + 1] == b'\\' {
                        i = j + 2; // ST-terminated
                        continue;
                    }
                    // Unterminated OSC → the regex doesn't match; fall to lone-ESC.
                }
                // lone ESC: \x1b[@-_]?  (ESC + optionally ONE byte in 0x40..=0x5f)
                if b.get(i + 1).is_some_and(|&n| (0x40..=0x5f).contains(&n)) {
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn w6_strip_ansi_agrees_on_csi_and_osc_classes() {
        // The CSI + OSC classes the TS regex targets (utils.ts:373-383) — the ONLY
        // escapes a claude composer dump carries. boot::strip_ansi and the TS
        // reference must produce identical output on each; a regression in either
        // stripper trips this.
        let cases: &[&str] = &[
            // OSC title set, BEL-terminated.
            "\u{1b}]0;window title\u{07}keep",
            // OSC hyperlink, ST-terminated (ESC \).
            "before\u{1b}]8;;https://x\u{1b}\\link\u{1b}]8;;\u{1b}\\after",
            // CSI colour run + cursor moves around text.
            "\u{1b}[31mred\u{1b}[0m \u{1b}[2J\u{1b}[1;1Hhome",
            // CSI with ? private param + intermediate bytes.
            "\u{1b}[?25ltext\u{1b}[0 q",
            // Lone ESC followed by a [@-_] byte (BOTH consume ESC + the one byte).
            "a\u{1b}Mb",
            // Lone trailing ESC at end-of-input (BOTH drop it).
            "trail\u{1b}",
            // Plain text, no escapes (identity).
            "just plain text 123",
            // Mixed CSI + OSC + plain.
            "\u{1b}[1mbold\u{1b}[0m\u{1b}]0;t\u{07}done",
            // Unicode payload between escapes (composer Unicode survives both).
            "\u{1b}[2mcafé ☕ 日本語\u{1b}[0m",
        ];
        for c in cases {
            assert_eq!(
                strip_ansi(c),
                ts_strip_ansi_reference(c),
                "boot::strip_ansi diverges from TS stripAnsi on CSI/OSC input {c:?}"
            );
        }
    }

    #[test]
    fn w6_documented_lone_esc_divergence() {
        // The DOCUMENTED, FLAGGED divergence (see the section comment above): on a
        // BARE lone ESC followed by a byte OUTSIDE 0x40..=0x5f, boot consumes the
        // byte (its wide catch-all rule, sanctioned by boot.rs's keypad test)
        // whereas TS's `\x1b[@-_]?` keeps it. This is inert for the composer
        // (composer dumps carry no such bare runs — those bytes only appear inside
        // CSI/OSC payloads, which both strip identically). Pinned so any change to
        // EITHER stripper surfaces here for the lead's review rather than silently.
        // ESC = (keypad, 0x3d): boot strips the `=`, TS keeps it.
        assert_eq!(strip_ansi("plain\u{1b}=more"), "plainmore");
        assert_eq!(ts_strip_ansi_reference("plain\u{1b}=more"), "plain=more");
        // ESC x (0x78): boot strips the `x`, TS keeps it.
        assert_eq!(strip_ansi("\u{1b}xtext"), "text");
        assert_eq!(ts_strip_ansi_reference("\u{1b}xtext"), "xtext");
    }

    // --- parse_jsonl_slice + find_anchor (N10 index-alignment) ------------

    #[test]
    fn parse_slice_drops_blanks_and_keeps_index_alignment() {
        let slice = "\n{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n\n\
                     {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"yo\"}]}}\n";
        let lines = parse_jsonl_slice(slice);
        assert_eq!(lines.len(), 2, "blank lines dropped");
        assert_eq!(find_anchor(&lines, "hi"), Some(0));
        // The raw line at the anchor index is the user record (alignment).
        assert!(lines[0].raw.contains("\"content\":\"hi\""));
    }

    #[test]
    fn parse_slice_bad_line_is_empty_record_non_anchoring() {
        // A malformed line parses to Null → record-view default → never anchors,
        // but STILL occupies its index slot (alignment preserved, L8/N10).
        let slice = "{ not json\n{\"type\":\"user\",\"message\":{\"content\":\"real\"}}";
        let lines = parse_jsonl_slice(slice);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].value.is_null(), "bad line → Null value");
        assert_eq!(find_anchor(&lines, "real"), Some(1));
    }

    // --- run_wait_loop (decide_wait-driven, seamed sleep) -----------------

    /// A scripted [`WaitDeps`]: a timeline of (lines, status) keyed by poll
    /// count, an advancing virtual clock, and a recorded glyph string. No real
    /// sleeps. `status_at(poll)` returns None to model a dead session.
    struct FakeWait {
        polls: std::cell::Cell<u32>,
        t: std::cell::Cell<i64>,
        glyphs: std::cell::RefCell<String>,
        lines_at: Box<dyn Fn(u32) -> Vec<ParsedLine>>,
        status_at: Box<dyn Fn(u32) -> Option<&'static str>>,
    }
    impl FakeWait {
        fn new(
            lines_at: impl Fn(u32) -> Vec<ParsedLine> + 'static,
            status_at: impl Fn(u32) -> Option<&'static str> + 'static,
        ) -> Self {
            Self {
                polls: std::cell::Cell::new(0),
                t: std::cell::Cell::new(0),
                glyphs: std::cell::RefCell::new(String::new()),
                lines_at: Box::new(lines_at),
                status_at: Box::new(status_at),
            }
        }
    }
    impl WaitDeps for FakeWait {
        fn read_lines(&self) -> Result<Vec<ParsedLine>, String> {
            Ok((self.lines_at)(self.polls.get()))
        }
        fn read_status(&self) -> Option<String> {
            (self.status_at)(self.polls.get()).map(str::to_string)
        }
        fn sleep(&self, ms: u64) {
            // Each sleep ends one poll: advance the clock + the poll counter.
            self.t.set(self.t.get() + ms as i64);
            self.polls.set(self.polls.get() + 1);
        }
        fn now_ms(&self) -> i64 {
            self.t.get()
        }
        fn progress(&self, glyph: char) {
            self.glyphs.borrow_mut().push(glyph);
        }
    }

    fn assistant_line(text: &str, stop: &str) -> ParsedLine {
        let raw = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"stop_reason\":\"{stop}\",\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}"
        );
        ParsedLine {
            value: serde_json::from_str(&raw).unwrap(),
            raw,
        }
    }
    fn user_line(text: &str) -> ParsedLine {
        let raw = format!("{{\"type\":\"user\",\"message\":{{\"content\":\"{text}\"}}}}");
        ParsedLine {
            value: serde_json::from_str(&raw).unwrap(),
            raw,
        }
    }

    #[test]
    fn wait_loop_waits_then_collects_then_completes() {
        // poll 0: anchor not present, busy → Waiting (':'); poll 1: anchor
        // present, busy → Collecting ('.'); poll 2: anchor present, idle →
        // Complete. The glyph order ':.' proves the decide_wait transitions.
        let lines_at = |poll: u32| -> Vec<ParsedLine> {
            if poll == 0 {
                vec![]
            } else {
                vec![user_line("hi"), assistant_line("done", "end_turn")]
            }
        };
        let status_at = |poll: u32| -> Option<&'static str> {
            match poll {
                0 => Some("busy"),
                1 => Some("busy"),
                _ => Some("idle"),
            }
        };
        let f = FakeWait::new(lines_at, status_at);
        let out = run_wait_loop(&f, "hi", 60_000, 500);
        match out {
            WaitOutcome::Complete { lines, anchor } => {
                assert_eq!(anchor, Some(0));
                assert_eq!(lines.len(), 2);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
        assert_eq!(
            *f.glyphs.borrow(),
            ":.",
            "waiting(':') then collecting('.')"
        );
    }

    #[test]
    fn wait_loop_unreadable_status_is_died() {
        let f = FakeWait::new(|_| vec![user_line("hi")], |_| None);
        assert!(matches!(
            run_wait_loop(&f, "hi", 60_000, 500),
            WaitOutcome::Died
        ));
    }

    #[test]
    fn wait_loop_timeout_anchored_vs_unanchored() {
        // Anchored but never idle → TimedOut { anchored: true }.
        let f = FakeWait::new(|_| vec![user_line("hi")], |_| Some("busy"));
        match run_wait_loop(&f, "hi", 1500, 500) {
            WaitOutcome::TimedOut { anchored } => assert!(anchored),
            other => panic!("expected TimedOut anchored, got {other:?}"),
        }
        // Never anchored (our message never surfaces) → anchored: false.
        let f2 = FakeWait::new(
            |_| vec![assistant_line("other", "end_turn")],
            |_| Some("busy"),
        );
        match run_wait_loop(&f2, "hi", 1500, 500) {
            WaitOutcome::TimedOut { anchored } => assert!(!anchored),
            other => panic!("expected TimedOut un-anchored, got {other:?}"),
        }
    }

    #[test]
    fn wait_loop_unreadable_after_anchor_still_died() {
        // decide_wait(true, None) is Collecting, but the loop treats an unreadable
        // STATUS as death FIRST (the pid-file read failed) — matches TS, which
        // exits on the status read catch before decide_wait.
        let f = FakeWait::new(|_| vec![user_line("hi")], |_| None);
        assert!(matches!(
            run_wait_loop(&f, "hi", 60_000, 500),
            WaitOutcome::Died
        ));
    }

    /// ADD-8 W5: a JSONL source that SHRINKS mid-wait must fail LOUD
    /// (SourceError), never silently re-scan from byte 0 (which could anchor on
    /// an earlier identical message). The fake errs from poll 2 onward,
    /// modelling a rotation after the wait started.
    #[test]
    fn wait_loop_shrunk_source_is_source_error_never_silent_rescan() {
        struct ShrinkWait {
            polls: std::cell::Cell<u32>,
        }
        impl WaitDeps for ShrinkWait {
            fn read_lines(&self) -> Result<Vec<ParsedLine>, String> {
                if self.polls.get() >= 2 {
                    Err(
                        "conversation JSONL shrank below the start offset (rotated/truncated)"
                            .to_string(),
                    )
                } else {
                    Ok(vec![]) // not yet anchored, still growing normally
                }
            }
            fn read_status(&self) -> Option<String> {
                Some("busy".to_string())
            }
            fn sleep(&self, _ms: u64) {
                self.polls.set(self.polls.get() + 1);
            }
            fn now_ms(&self) -> i64 {
                self.polls.get() as i64 * 500
            }
            fn progress(&self, _glyph: char) {}
        }
        let f = ShrinkWait {
            polls: std::cell::Cell::new(0),
        };
        match run_wait_loop(&f, "dup", 60_000, 500) {
            WaitOutcome::SourceError(reason) => {
                assert!(
                    reason.contains("shrank"),
                    "reason names the shrink: {reason}"
                );
            }
            other => panic!("expected SourceError, got {other:?}"),
        }
    }

    // --- extract_response (raw / default / full) --------------------------

    #[test]
    fn extract_default_only_end_turn_text_after_anchor() {
        let lines = vec![
            user_line("hi"),
            assistant_line("interim", "tool_use"), // not end_turn → excluded from default
            assistant_line("final answer", "end_turn"),
        ];
        let out = extract_response(&lines, Some(0), ExtractMode::Default);
        assert_eq!(out, "final answer");
    }

    #[test]
    fn extract_default_no_text_fallback() {
        let lines = vec![user_line("hi")];
        assert_eq!(
            extract_response(&lines, Some(0), ExtractMode::Default),
            NO_TEXT_RESPONSE
        );
    }

    #[test]
    fn extract_raw_dumps_lines_after_anchor() {
        let lines = vec![user_line("hi"), assistant_line("a", "end_turn")];
        let out = extract_response(&lines, Some(0), ExtractMode::Raw);
        assert_eq!(out, lines[1].raw);
    }

    #[test]
    fn extract_full_includes_thinking_and_tool_use() {
        let raw_think = "{\"type\":\"assistant\",\"message\":{\"content\":[\
            {\"type\":\"thinking\",\"thinking\":\"hmm\"},\
            {\"type\":\"text\",\"text\":\"body\"},\
            {\"type\":\"tool_use\",\"name\":\"grep\",\"input\":{\"q\":\"x\"}}]}}";
        let line = ParsedLine {
            value: serde_json::from_str(raw_think).unwrap(),
            raw: raw_think.to_string(),
        };
        let lines = vec![user_line("hi"), line];
        let out = extract_response(&lines, Some(0), ExtractMode::Full);
        assert!(out.contains("[thinking] hmm"));
        assert!(out.contains("body"));
        assert!(out.contains("[tool: grep] {\"q\":\"x\"}"));
    }

    #[test]
    fn extract_full_truncates_tool_input_to_200_chars() {
        let big = "y".repeat(500);
        let raw = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"t\",\"input\":{{\"v\":\"{big}\"}}}}]}}}}"
        );
        let line = ParsedLine {
            value: serde_json::from_str(&raw).unwrap(),
            raw,
        };
        let out = extract_response(&[user_line("hi"), line], Some(0), ExtractMode::Full);
        let marker = "[tool: t] ";
        let after = out.split(marker).nth(1).unwrap();
        assert_eq!(
            after.chars().count(),
            200,
            "tool input truncated to 200 chars"
        );
    }

    #[test]
    fn extract_no_anchor_uses_all_lines() {
        // anchor None → all lines considered (TS `allLines`).
        let lines = vec![assistant_line("z", "end_turn")];
        assert_eq!(extract_response(&lines, None, ExtractMode::Default), "z");
    }
}
