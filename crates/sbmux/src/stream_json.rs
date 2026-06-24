//! Pure stream-json parser core (WP-B1) — the per-session daemon's real-time
//! channel reader, with NO process spawn and NO socket (those are WP-B2).
//!
//! `claude -p --output-format stream-json --verbose` emits one `\n`-terminated
//! NDJSON event per `write()` (trackA §1, spike-1 Q5). Over a pipe a reader
//! gets arbitrary chunks: a partial trailing line, multiple events in one read,
//! or an event larger than one read/pipe chunk (≥23 KB observed; design for
//! hundreds of KB). This module is the **line-reassembly state machine** plus
//! the **event taxonomy + extraction** that realizes the §2.3/§2.4 + Q5
//! contract.
//!
//! Design (purity, so WP-B2 can wire it to the real reader + socket):
//! - [`StreamParser::push`] is fed `&[u8]` chunks (whatever a `read()` returned)
//!   and returns the events whose terminating `\n` has now arrived.
//! - [`StreamParser::finish`] is called at EOF: it **discards any unterminated
//!   tail** (a valid prefix on kill — spike-1 Q4 — never a torn record) and
//!   reports whether the in-flight turn ended without a `result`
//!   (turn-aborted).
//! - The reassembly buffer is **BOUNDED** by a configurable hard cap; a single
//!   line that exceeds the cap is a pathological/never-terminating event — the
//!   parser surfaces a [`StreamEvent::CircuitBreaker`] signal ("kill the child")
//!   rather than growing unbounded. The actual child-kill is WP-B2; here we
//!   expose the signal cleanly and the buffer never exceeds the cap.
//!
//! Framing rule (load-bearing, verified against real captures — see the
//! `embedded_newline_*` tests): an event is ready only when a literal `\n`
//! terminates it. A `\n` *inside* a JSON string value is escaped as `\\n` in
//! valid JSON, so a literal newline only ever occurs *between* records; we
//! still handle a hypothetical raw embedded newline safely (it would merely
//! frame early and the malformed half surfaces as [`StreamEvent::Other`] with a
//! parse error, never a panic).

use serde::Deserialize;

/// Default hard cap on the reassembly buffer (the largest a single un-terminated
/// line may grow to before the circuit-breaker fires). 16 MiB matches the
/// `protocol::codec::MAX_FRAME_SIZE` posture: well above the largest plausible
/// real event (tens to a few hundred KB) yet bounded so a malformed
/// never-terminating line cannot OOM the daemon (§H.5).
pub const DEFAULT_MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// One parsed item off the stdout stream. Forward-compatible: an unknown or
/// future `type` (or a malformed-but-framed line) never crashes — it surfaces
/// as [`StreamEvent::Other`] (evolution rule (b): readers skip what they don't
/// know, here as a visible "other" rather than a silent drop, so the consumer
/// can log/forward it).
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// `system` event, subtype `init` — the FIRST event of a turn (t≈0). Carries
    /// READY + the identity binding: `session_id` == `<session_id>.jsonl`
    /// (spike-1 Q6), known before turn completion.
    SystemInit { session_id: String },
    /// `assistant` event — carries the whole message. The assistant **content**
    /// is taken from THIS stdout event, never the transcript (bug#3: `result`
    /// precedes the unfsynced `.jsonl` append). A single turn may emit multiple
    /// `assistant` events (intermediate text / `tool_use` / `thinking` blocks —
    /// q5frame capture).
    Assistant {
        session_id: String,
        /// The concatenated text of all `text` content blocks in the message
        /// (the in-band assistant content). `tool_use`/`thinking` blocks carry
        /// no `text` and contribute nothing here.
        content: String,
    },
    /// `result` event, subtype `success`/`error` — always last in a turn; the
    /// **turn-end signal**.
    Result(ResultEvent),
    /// `rate_limit_event` — parsed but **ignore-for-control** (spike-1 §11
    /// flag 5: non-deterministic presence; do not gate any turn logic on it).
    RateLimit { session_id: String },
    /// A framed line that is valid JSON but a `type` we do not specially handle
    /// (e.g. `user` from `--replay-user-messages`, `system/status`, or a future
    /// type), OR a framed line that failed JSON parse. Never crashes; carries
    /// the raw line so a consumer can log/forward it.
    Other {
        /// The `type` field if the line parsed as a JSON object with a string
        /// `type`; `None` if the line was not parseable as such.
        kind: Option<String>,
        /// The raw line bytes (without the terminating `\n`), lossily decoded.
        raw: String,
        /// Set when the line was not valid JSON (a malformed-but-framed record).
        parse_error: bool,
    },
    /// **Circuit-breaker** (§H.5): a single un-terminated line exceeded the hard
    /// cap. The reassembly buffer is dropped (never exceeds the cap) and the
    /// consumer must abort/kill the child (WP-B2 owns the actual kill). After
    /// this the parser is in a poisoned state and emits no further events.
    CircuitBreaker {
        cap_bytes: usize,
        observed_bytes: usize,
    },
}

/// The extracted `result`-event facts the consumer needs for turn-end / cost /
/// liveness republish (§2.4).
#[derive(Debug, Clone, PartialEq)]
pub struct ResultEvent {
    pub session_id: String,
    /// `false` for subtype `success`, `true` for subtype `error`.
    pub is_error: bool,
    /// e.g. `end_turn`. `None` if absent.
    pub stop_reason: Option<String>,
    /// Token accounting from `usage` (§2.4 / §H.3 cache-tier telemetry hook).
    pub usage: Usage,
    /// `total_cost_usd`. NOTE (spike-3 / memo §2.2): under keep-alive this is
    /// CUMULATIVE per session; under re-spawn it is per-process. The parser
    /// reports the raw field — the consumer (WP-B2) applies the delta policy.
    pub total_cost_usd: Option<f64>,
}

/// Token usage extracted from a `result` event's `usage` object. Missing fields
/// default to 0 (forward-compat; never errors).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    /// `usage.cache_creation.ephemeral_5m_input_tokens` — the 5-minute tier
    /// (§H.3: a regression to 5-min TTL flips this positive; the telemetry hook
    /// alerts on the flip).
    pub ephemeral_5m_input_tokens: u64,
    /// `usage.cache_creation.ephemeral_1h_input_tokens` — the 1-hour tier the
    /// re-spawn cost claim leans on (§D2).
    pub ephemeral_1h_input_tokens: u64,
}

/// Outcome reported by [`StreamParser::finish`] for the in-flight turn at EOF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    /// A `result` event was observed for the turn before EOF — clean turn-end.
    Completed,
    /// EOF reached without a `result` for the in-flight turn (a killed turn —
    /// spike-1 Q4: session resumable via `--resume`). Distinct from a clean
    /// completion.
    Aborted,
    /// No events at all were ever parsed (empty stream / immediate EOF).
    NoTurn,
}

/// The pure line-reassembly + extraction state machine. Feed it bytes with
/// [`push`](Self::push); call [`finish`](Self::finish) at EOF. No I/O, no
/// process, no socket — WP-B2 wires it to the real reader.
pub struct StreamParser {
    /// The reassembly buffer: bytes seen since the last `\n` (a partial line).
    buf: Vec<u8>,
    /// Hard cap on `buf` length; exceeding it fires the circuit-breaker.
    max_line_bytes: usize,
    /// True once a `result` event has been observed for the current turn.
    saw_result: bool,
    /// True once any event at all has been parsed.
    saw_any_event: bool,
    /// True once the circuit-breaker fired — the parser is poisoned and emits
    /// nothing further.
    tripped: bool,
}

impl StreamParser {
    /// New parser with the default hard cap ([`DEFAULT_MAX_LINE_BYTES`]).
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_MAX_LINE_BYTES)
    }

    /// New parser with an explicit hard cap on a single line's length.
    pub fn with_cap(max_line_bytes: usize) -> Self {
        Self {
            buf: Vec::new(),
            max_line_bytes,
            saw_result: false,
            saw_any_event: false,
            tripped: false,
        }
    }

    /// The current reassembly buffer length (for the §H.5 "never exceeds the
    /// cap" proof).
    pub fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    /// True once the circuit-breaker has tripped (the parser is poisoned).
    pub fn is_tripped(&self) -> bool {
        self.tripped
    }

    /// Feed one chunk of bytes (whatever a `read()` returned). Returns every
    /// event whose terminating `\n` has now arrived, in order. A partial
    /// trailing line stays buffered until its `\n` arrives.
    ///
    /// If a single un-terminated line would exceed the hard cap, the parser
    /// drops the buffer, emits a [`StreamEvent::CircuitBreaker`], and trips
    /// (all subsequent calls return empty). The buffer never exceeds the cap.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.tripped {
            return out;
        }
        let mut rest = chunk;
        loop {
            match memchr(b'\n', rest) {
                Some(nl) => {
                    // A complete line is everything buffered + up to (not incl.)
                    // the newline. Enforce the cap on the *completed* line too —
                    // a line at-or-under cap is fine; over cap trips.
                    let completed_len = self.buf.len() + nl;
                    if completed_len > self.max_line_bytes {
                        out.push(self.trip(completed_len));
                        return out;
                    }
                    let line: &[u8] = if self.buf.is_empty() {
                        &rest[..nl]
                    } else {
                        self.buf.extend_from_slice(&rest[..nl]);
                        &self.buf
                    };
                    let ev = parse_line(line);
                    if let StreamEvent::Result(_) = ev {
                        self.saw_result = true;
                    }
                    self.saw_any_event = true;
                    out.push(ev);
                    self.buf.clear();
                    rest = &rest[nl + 1..];
                }
                None => {
                    // No newline in the remainder: it is a partial trailing
                    // line. Buffer it, enforcing the cap BEFORE growing past it.
                    let would_be = self.buf.len() + rest.len();
                    if would_be > self.max_line_bytes {
                        out.push(self.trip(would_be));
                        return out;
                    }
                    self.buf.extend_from_slice(rest);
                    break;
                }
            }
        }
        out
    }

    /// Signal EOF. **Discards any unterminated tail** (a valid prefix on kill —
    /// never emit a partial line as an event) and reports the in-flight turn's
    /// outcome. Returns [`TurnOutcome::Aborted`] when events were seen but no
    /// `result` arrived (a killed turn), [`TurnOutcome::Completed`] when a
    /// `result` was seen, [`TurnOutcome::NoTurn`] when the stream was empty.
    ///
    /// The buffer is cleared (the tail is discarded, never parsed).
    pub fn finish(&mut self) -> TurnOutcome {
        // Discard the unterminated tail — never parse a no-newline remainder.
        self.buf.clear();
        if !self.saw_any_event {
            return TurnOutcome::NoTurn;
        }
        // A clean turn-end requires a `result`; a tripped stream or a kill
        // before `result` is Aborted.
        if self.saw_result && !self.tripped {
            TurnOutcome::Completed
        } else {
            TurnOutcome::Aborted
        }
    }

    /// Trip the circuit-breaker: poison the parser, drop the buffer, return the
    /// signal event.
    fn trip(&mut self, observed: usize) -> StreamEvent {
        self.tripped = true;
        self.buf = Vec::new();
        StreamEvent::CircuitBreaker {
            cap_bytes: self.max_line_bytes,
            observed_bytes: observed,
        }
    }
}

impl Default for StreamParser {
    fn default() -> Self {
        Self::new()
    }
}

/// The minimal JSON shapes the parser extracts from. `#[serde(default)]` on
/// every optional field + NO `deny_unknown_fields` = forward-compatible
/// (evolution rule (a)/(e)): a future field never breaks parsing.
#[derive(Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    type_: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    message: Option<RawMessage>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<RawUsage>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
}

#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    content: Vec<RawContentBlock>,
}

#[derive(Deserialize)]
struct RawContentBlock {
    #[serde(rename = "type")]
    type_: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_creation: Option<RawCacheCreation>,
}

#[derive(Deserialize, Default)]
struct RawCacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

/// Parse one complete (newline-stripped) line into a typed [`StreamEvent`].
/// Never errors / never panics: a malformed line or an unknown `type` surfaces
/// as [`StreamEvent::Other`].
fn parse_line(line: &[u8]) -> StreamEvent {
    let ev: RawEvent = match serde_json::from_slice(line) {
        Ok(ev) => ev,
        Err(_) => {
            return StreamEvent::Other {
                kind: None,
                raw: String::from_utf8_lossy(line).into_owned(),
                parse_error: true,
            };
        }
    };
    let session_id = ev.session_id.clone().unwrap_or_default();
    match ev.type_.as_deref() {
        Some("system") if ev.subtype.as_deref() == Some("init") => {
            StreamEvent::SystemInit { session_id }
        }
        Some("assistant") => {
            let content = ev
                .message
                .map(|m| {
                    m.content
                        .into_iter()
                        .filter(|b| b.type_.as_deref() == Some("text"))
                        .filter_map(|b| b.text)
                        .collect::<String>()
                })
                .unwrap_or_default();
            StreamEvent::Assistant {
                session_id,
                content,
            }
        }
        Some("result") => StreamEvent::Result(ResultEvent {
            session_id,
            // subtype "error" OR is_error == true ⇒ error.
            is_error: ev.is_error.unwrap_or(false) || ev.subtype.as_deref() == Some("error"),
            stop_reason: ev.stop_reason,
            usage: ev.usage.map(Usage::from).unwrap_or_default(),
            total_cost_usd: ev.total_cost_usd,
        }),
        Some("rate_limit_event") => StreamEvent::RateLimit { session_id },
        other => StreamEvent::Other {
            kind: other.map(|s| s.to_string()),
            raw: String::from_utf8_lossy(line).into_owned(),
            parse_error: false,
        },
    }
}

impl From<RawUsage> for Usage {
    fn from(u: RawUsage) -> Self {
        let cc = u.cache_creation.unwrap_or_default();
        Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            cache_creation_input_tokens: u.cache_creation_input_tokens,
            ephemeral_5m_input_tokens: cc.ephemeral_5m_input_tokens,
            ephemeral_1h_input_tokens: cc.ephemeral_1h_input_tokens,
        }
    }
}

/// Find the first occurrence of `needle` in `hay`. Dependency-free helper (the
/// `memchr` crate is only a transitive dep; this keeps the module's direct deps
/// to serde/serde_json and the hot path simple).
fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

#[cfg(test)]
mod tests;
