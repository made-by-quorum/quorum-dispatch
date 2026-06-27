//! Relay server shared state + PURE delivery logic (M1 — no sockets, no threads).
//!
//! This module is the DATA LAYER the relay server will share across threads in
//! M2+. Everything here is pure and synchronous so it composes cleanly under a
//! `Mutex` later (spec §2 `Arc<(Mutex<Inner>, Condvar)>`).  The lock wrapper,
//! `Condvar`, and thread-park logic land in M4.
//!
//! KEY DESIGN RULE (P-G6): methods NEVER perform IO, stdout writes, or any
//! blocking syscall. They accept and return plain data only. The HTTP and MCP
//! layers gather the return values, release the lock, THEN do IO. This
//! is load-bearing: a stuck stdout consumer must never stall relay state.
//!
//! ## Parity contract anchors
//! - `mint_message_id` → P-A2, server.ts:258
//! - `record_origin` / FIFO eviction → P-E7, server.ts:44-52
//! - `buffer_reply` → P-E1, server.ts:200-201
//! - `peek_resolved` / TTL check (non-destructive read) → P-A3 + P-G4, server.ts:303-306
//! - `decide_delivery` / loop belt → P-E4, server.ts:142-150
//! - Origin guards → P-E5, server.ts:152-155
//! - `origin_for` → in-memory half of server.ts:113-117
//!
//! The inbox-file FALLBACK in `originFor` (server.ts:116-124) is IO; it is
//! deferred to a later phase.  M1 = the in-memory map half only.

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Constants (server.ts line anchors)
// ---------------------------------------------------------------------------

/// Bounded FIFO cap on the origin map (server.ts:44).
/// Oldest entry evicted when the map is at capacity. P-E7.
pub const MAX_TRACKED_ORIGINS: usize = 2000;

/// How long a buffered reply survives before TTL eviction. P-E1/P-G4.
/// server.ts:200 `setTimeout(..., 5 * 60_000)`.
pub const RESOLVED_TTL: Duration = Duration::from_secs(5 * 60);

/// Prose marker prepended to a pushed-back reply text and used for
/// loop-prevention detection (server.ts:59 `const REPLY_PREFIX`).
pub const REPLY_PREFIX: &str = "[REPLY to ";

/// Returns `true` iff `text` is a pushed-back relay reply (starts with
/// `REPLY_PREFIX`). Used for the loop-prevention belt (P-E4) and for
/// re-deriving the `is_reply` flag from a persisted inbox message
/// (server.ts:121 `data.text.startsWith(REPLY_PREFIX)`).
///
/// server.ts:121, 265.
pub fn is_reply_text(text: &str) -> bool {
    text.starts_with(REPLY_PREFIX)
}

/// On-disk INBOX FALLBACK for origin resolution (server.ts:116-124) — the half
/// of `originFor` deferred from M1. When the in-memory origin map MISSES (e.g. a
/// reply arrives after the receiving sidecar restarted, so the in-mem record was
/// lost), read the persisted inbox file `<inbox_dir>/<message_id>.json` and
/// reconstruct the [`OriginRec`] from its `from_session` + (re-derived) `is_reply`.
///
/// The `is_reply` flag is RE-DERIVED from the persisted `text` via
/// [`is_reply_text`] (server.ts:121 `data.text.startsWith(REPLY_PREFIX)`) so the
/// loop-prevention belt (P-E4) survives a sidecar restart too — a persisted
/// `[REPLY to ...]` message stays loop-prevented even after the in-mem flag is gone.
///
/// LOCK DISCIPLINE (cond 5 / P-G6): this is FILE IO. It MUST be called with the
/// state lock RELEASED. It is a free function (no `&RelayState`) precisely so it
/// cannot be invoked while holding the lock by accident — `deliver_reply` calls
/// it only after dropping the guard.
///
/// Returns `None` when the file is absent / unreadable / unparseable, or carries
/// no string `from_session` (server.ts: the `catch {}` + the `typeof` guard).
pub fn origin_from_inbox(inbox_dir: &Path, message_id: &str) -> Option<OriginRec> {
    let path = inbox_dir.join(format!("{message_id}.json"));
    let bytes = std::fs::read(path).ok()?;
    let data: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    // server.ts:118 `typeof data.from_session === 'string'` — a non-string (or
    // absent) from_session means the file does not pin an origin → None.
    let from = data
        .get("from_session")
        .and_then(|v| v.as_str())?
        .to_string();
    // Re-derive is_reply from the persisted text (server.ts:121). An absent /
    // non-string text is treated as not-a-reply (starts_with on "" is false).
    let text = data.get("text").and_then(|v| v.as_str()).unwrap_or("");
    Some(OriginRec {
        from,
        is_reply: is_reply_text(text),
    })
}

// ---------------------------------------------------------------------------
// Auxiliary types
// ---------------------------------------------------------------------------

/// An entry in the origin FIFO (server.ts:45 `{ from: string; isReply: boolean }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginRec {
    /// The `from_session` field from the inbound `POST /message` body.
    pub from: String,
    /// `true` iff the inbound message text started with `REPLY_PREFIX` — feeds
    /// the loop-prevention belt (P-E4). server.ts:265.
    pub is_reply: bool,
}

/// A buffered reply: text + the `Instant` at which it expires (TTL = RESOLVED_TTL).
/// Stored in `resolved` map. P-E1 / P-G4.
#[derive(Debug)]
pub struct ResolvedEntry {
    pub text: String,
    /// Absolute deadline past which the entry is stale and must be evicted.
    pub deadline: Instant,
}

/// Decision returned by [`decide_delivery`] — pure, never performs IO.
///
/// The HTTP / MCP layers act on the variant:
/// - `ResolveWaiter` → signal the parked Condvar (M4).
/// - `LoopPrevented` / `NotAddressable` / `NoOrigin` → build the NOT-DELIVERED
///   response text (P-E6).
/// - `PushBack { origin }` → look up origin sidecars and POST `/message` (M4).
///
/// Check order MUST match server.ts reply handler:
/// 1. Waiter present? → `ResolveWaiter` (server.ts:205-209).
/// 2. Origin lookup (in-memory; inbox fallback is IO, M4+).
/// 3. No origin record → `NoOrigin` (server.ts:135-141).
/// 4. `is_reply` → `LoopPrevented` (server.ts:142-150). P-E4.
/// 5. `unknown` / `cli` / self → `NotAddressable` (server.ts:152-155). P-E5.
/// 6. Real addressable session → `PushBack` (server.ts:151-185).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryDecision {
    /// A `/replies/<id>` long-poll is parked: resolve it (server.ts:205-209). P-E2.
    ResolveWaiter,
    /// Inbound message was itself a pushed-back reply; refusing auto-post to
    /// prevent ping-pong (server.ts:142-150). P-E4.
    LoopPrevented,
    /// Origin session is `unknown`, `cli`, or this session's own id — not
    /// addressable (server.ts:152-155). P-E5. `reason` carries the guard detail.
    NotAddressable { reason: String },
    /// Origin is a real, addressable, non-self session. `origin` is the
    /// session id to push-back to (server.ts:172-185). P-E3.
    PushBack { origin: String },
    /// No origin record exists for `message_id` (server.ts:135-141).
    NoOrigin,
}

// ---------------------------------------------------------------------------
// The shared relay state (inner; M1 = plain struct with &mut self methods)
// ---------------------------------------------------------------------------

/// In-memory relay state shared across threads (in M2+ this lives inside
/// `Arc<(Mutex<RelayState>, Condvar)>`).
///
/// In M1 the struct is used directly with `&mut self` methods so the logic
/// is fully unit-testable without any threading machinery.
pub struct RelayState {
    /// Pending waiters: `message_id` → a marker that a `/replies/<id>` long-poll
    /// is parked. In M1 this is a `bool`-equivalent; in M4 the value becomes a
    /// channel or Condvar token.
    ///
    /// server.ts:36 `pendingReplies`.
    pending: std::collections::HashMap<String, ()>,

    /// Buffered replies: `message_id` → (text, expiry). P-E1 / P-G4.
    ///
    /// server.ts:37 `resolvedReplies`.
    resolved: std::collections::HashMap<String, ResolvedEntry>,

    /// Bounded FIFO origin map: `message_id` → OriginRec. MAX_TRACKED_ORIGINS cap.
    ///
    /// Implemented as a `HashMap` for O(1) lookup + a `VecDeque` of insertion-order
    /// keys for O(1) oldest-eviction. This matches the TS `Map` FIFO property
    /// (Maps iterate in insertion order; deletion of the first key = oldest).
    ///
    /// server.ts:45 `originByMessageId`.
    origins: std::collections::HashMap<String, OriginRec>,
    /// Insertion-order key FIFO for eviction.
    origins_order: VecDeque<String>,

    /// B2 item 4 (resolve-and-verify): message_ids whose `deliver_reply` is
    /// currently parked in its bounded-ack window, waiting for the resolved
    /// long-poll's WRITE OUTCOME. The parked `/replies` thread (and the
    /// hangup-detection tick) report an ack ONLY while the marker is present,
    /// so the `acks` map can never accumulate orphans.
    verify_pending: std::collections::HashSet<String>,

    /// B2 item 4: delivery-ack outcomes, `message_id` → `true` (the resolved
    /// long-poll response WAS written to a live socket) / `false` (the waiter
    /// was dead: socket EOF before the write, or the write failed). Entries
    /// exist only under a `verify_pending` marker and are TAKEN by
    /// `deliver_reply`'s verify window.
    acks: std::collections::HashMap<String, bool>,

    /// Monotonic sequence counter for message_id minting. P-A2.
    /// server.ts:237 `let msgSeq = 0` (incremented pre-use, so first id ends in `-1`).
    seq: u64,
}

impl RelayState {
    /// Construct a fresh, empty state with the seq counter at 0 (first minted id
    /// ends in `-1`). Used by tests and the M1 unit tests where seq-seeding is
    /// irrelevant. Production boot uses [`RelayState::with_seq_seed`] (M5a iv).
    pub fn new() -> Self {
        Self::with_seq_seed(0)
    }

    /// Construct a fresh state with the per-process seq counter SEEDED to `seed`
    /// (M5a item iv). The first minted id then ends in `-{seed+1}`.
    ///
    /// WHY (the incident fix's sibling): `mint_message_id` = `relay-<epoch_ms>-<seq>`
    /// with seq per-process. Two FRESH servers booting in the same millisecond, both
    /// starting at seq 0, mint the IDENTICAL first id (`relay-<ms>-1`). The core
    /// delivery path is unaffected (each id is minted+consumed within ONE process'
    /// state maps), but the SHARED inbox dir `<inbox>/<id>.json` collides on the
    /// filename — corrupting the origin inbox-FALLBACK after a restart. Seeding the
    /// seq with a pid/random-derived base makes two fresh servers start at DIFFERENT
    /// seq values, so their first-same-ms ids diverge.
    ///
    /// ★ FORMAT INVARIANT (orc ruling iv): this seeds ONLY the SEQ (position 3) of
    /// `relay-<epoch_ms>-<seq>`. The EPOCH (position 2) is UNTOUCHED — it stays the
    /// real `now_ms`, parseable by any fleet consumer that extracts the mint epoch.
    /// `seed` is a `u64`, so the seq segment stays all-ASCII-digits (the well-formed
    /// `relay-<digits>-<digits>` shape that `is_well_formed_message_id` and any
    /// `split('-')` consumer expects is preserved).
    pub fn with_seq_seed(seed: u64) -> Self {
        RelayState {
            pending: std::collections::HashMap::new(),
            resolved: std::collections::HashMap::new(),
            origins: std::collections::HashMap::new(),
            origins_order: VecDeque::new(),
            verify_pending: std::collections::HashSet::new(),
            acks: std::collections::HashMap::new(),
            seq: seed,
        }
    }

    // -----------------------------------------------------------------------
    // Message-id minting (P-A2)
    // -----------------------------------------------------------------------

    /// Mint a fresh `relay-<now_ms>-<seq>` message id. P-A2.
    ///
    /// `seq` is per-process monotonic; incremented PRE-use (matching TS
    /// `++msgSeq` — first id ends in `-1`, never `-0`).
    ///
    /// server.ts:258 `` `relay-${Date.now()}-${++msgSeq}` ``.
    pub fn mint_message_id(&mut self, now_ms: u64) -> String {
        self.seq += 1;
        format!("relay-{now_ms}-{}", self.seq)
    }

    // -----------------------------------------------------------------------
    // Origin FIFO (P-E7)
    // -----------------------------------------------------------------------

    /// Record the origin of an inbound message. FIFO-capped at
    /// `MAX_TRACKED_ORIGINS`: when full, the OLDEST entry is evicted first.
    ///
    /// Re-insertion of an existing `message_id` is a no-op (update the value
    /// in place without touching `origins_order`), matching the TS `Map.set`
    /// behaviour where overwriting a key doesn't change its FIFO position or
    /// inflate the map size. In practice message_ids are globally unique
    /// (minted by `mint_message_id`), but the guard keeps the invariant sound.
    ///
    /// server.ts:46-52 `recordOrigin`.  P-E7.
    pub fn record_origin(&mut self, message_id: String, from_session: String, is_reply: bool) {
        let rec = OriginRec {
            from: from_session,
            is_reply,
        };
        if let Some(existing) = self.origins.get_mut(&message_id) {
            // Re-insert: update value in-place, don't touch origins_order.
            // Matches TS Map.set — overwriting a key doesn't change FIFO position
            // or inflate the map size. P-E7.
            *existing = rec;
            return;
        }
        // New key: evict oldest when at capacity (server.ts:47-50), then insert.
        if self.origins.len() >= MAX_TRACKED_ORIGINS {
            if let Some(oldest) = self.origins_order.pop_front() {
                self.origins.remove(&oldest);
            }
        }
        self.origins.insert(message_id.clone(), rec);
        self.origins_order.push_back(message_id);
    }

    /// Look up the origin record for a message. Returns `None` if not present
    /// (the caller may then fall back to the inbox file — IO deferred to M4+).
    ///
    /// In-memory map half of server.ts:113-117 `originFor`.
    pub fn origin_for(&self, message_id: &str) -> Option<OriginRec> {
        self.origins.get(message_id).cloned()
    }

    /// §X.3.5 (3-phase delivery) — the message_ids THIS relay server received as
    /// genuine incoming messages (replies excluded), bounded by
    /// `MAX_TRACKED_ORIGINS`. Used at the session-close bookend to scope
    /// `seen-failed` to messages THIS recipient actually received — never a
    /// co-homed session's: the inbox dir is per-HOME and shared, so an
    /// inbox-dir-wide scan would emit `seen-failed` for another session's
    /// still-pending message, which first-terminal-wins would turn into a
    /// wrong-fire. Scoping by this session's own origin record prevents that.
    pub fn tracked_message_ids(&self) -> Vec<String> {
        self.origins
            .iter()
            .filter(|(_, rec)| !rec.is_reply)
            .map(|(id, _)| id.clone())
            .collect()
    }

    // -----------------------------------------------------------------------
    // Reply buffer: buffer_reply + peek_resolved (P-E1, P-A3, P-G4)
    // -----------------------------------------------------------------------

    /// Buffer a reply unconditionally. Called by the `reply` MCP tool BEFORE
    /// choosing a delivery path (P-E1). The `deadline` is `now + RESOLVED_TTL`.
    ///
    /// server.ts:200-201.
    pub fn buffer_reply(&mut self, message_id: String, text: String, deadline: Instant) {
        self.resolved
            .insert(message_id, ResolvedEntry { text, deadline });
    }

    /// PEEK a buffered reply: return a CLONE of the text if present AND not yet
    /// past its deadline, LEAVING the entry in place. If the entry exists but is
    /// expired, EVICT it and return `None` (lazy TTL eviction — P-G4). If absent,
    /// return `None`.
    ///
    /// PEEK, NOT consume — this is load-bearing. server.ts:303-306 is
    /// `resolvedReplies.get(messageId)`, a non-destructive read; the entry is
    /// only deleted by the 5-min `setTimeout` (server.ts:200). buffer_reply
    /// writes the buffer FIRST unconditionally (server.ts:198-201 "retrieve from
    /// here regardless") precisely so a `qd send:relay --wait` client whose
    /// long-poll connection drops can RE-GET `/replies/<id>` IDEMPOTENTLY within
    /// the 5-min window. A consuming read would make the cached branch
    /// single-shot: if the first `/replies` HTTP response is lost on the wire,
    /// the client's retry gets `None` → the reply is permanently lost — a fresh
    /// instance of the 61-loss class this server exists to kill.
    ///
    /// LAZY EVICTION CAVEAT (M2 carry): because peek does NOT consume, the TTL
    /// is only enforced when an expired id is actually read. A buffered-but-
    /// never-retried reply LEAKS until something reads it. A proactive TTL
    /// sweeper (matching server.ts:200 `setTimeout`) is REQUIRED in M2 to bound
    /// `resolved`-map growth (P-F4). M1 only implements the lazy half.
    ///
    /// This implements the "cached branch" of `/replies/<id>` (server.ts:303-306)
    /// and the 5-min TTL (server.ts:200 `setTimeout`). P-A3 + P-G4.
    pub fn peek_resolved(&mut self, message_id: &str, now: Instant) -> Option<String> {
        match self.resolved.get(message_id) {
            None => None,
            Some(entry) if entry.deadline <= now => {
                // Expired — lazy-evict and return None. P-G4.
                self.resolved.remove(message_id);
                None
            }
            // Within TTL — CLONE and LEAVE the entry (idempotent re-read). P-A3.
            Some(entry) => Some(entry.text.clone()),
        }
    }

    // -----------------------------------------------------------------------
    // Waiter registration (used by M4 long-poll; exposed as pure bool in M1)
    // -----------------------------------------------------------------------

    /// Register that a `/replies/<id>` long-poll is parked for `message_id`.
    /// In M4 this becomes the Condvar slot; in M1 it is a boolean marker.
    pub fn register_waiter(&mut self, message_id: String) {
        self.pending.insert(message_id, ());
    }

    /// Remove the waiter registration (called when the long-poll is resolved
    /// or times out).
    pub fn remove_waiter(&mut self, message_id: &str) {
        self.pending.remove(message_id);
    }

    /// Returns `true` iff a waiter is currently registered for `message_id`.
    pub fn has_waiter(&self, message_id: &str) -> bool {
        self.pending.contains_key(message_id)
    }

    // -----------------------------------------------------------------------
    // B2 item 4 — resolve-and-verify ack handshake (bounded; one lock, P-G3)
    // -----------------------------------------------------------------------
    //
    // THE STALE-WAITER FIX, state half. `deliver_reply` no longer trusts a
    // registered waiter as proof of delivery: it MARKS the id verify-pending,
    // notifies, and parks (bounded) for the resolved long-poll's WRITE OUTCOME.
    // The `/replies` thread's RESOLVED-WRITE path OFFERS the outcome (written
    // true/false — the ONLY live nack channel); the hangup tick DEREGISTERS
    // the waiter instead (its belt offer is provably dropped today — see the
    // http.rs hangup-branch comment). An offer with no marker present is
    // dropped, so no orphan entries survive a verify window that already
    // gave up.

    /// Mark `message_id` as verify-pending (deliver_reply entered its bounded
    /// ack window). The outcome is taken via [`RelayState::take_ack`]; the
    /// window is closed by [`RelayState::unmark_verify`].
    pub fn mark_verify(&mut self, message_id: &str) {
        self.verify_pending.insert(message_id.to_string());
    }

    /// Returns `true` iff a verify window is currently OPEN for `message_id`
    /// (red-team W1): a second `deliver_reply` for the same id observes this
    /// and PARKS until the window closes — `verify_pending` is a per-id SET
    /// and `acks` a per-id MAP, so two concurrent same-id verify windows would
    /// cross-wire the single ack (and `buffer_reply` would overwrite the first
    /// window's text mid-resolve). The serialization itself lives in
    /// `deliver_reply` (it needs the Condvar).
    pub fn verify_open(&self, message_id: &str) -> bool {
        self.verify_pending.contains(message_id)
    }

    /// Report the resolved long-poll's write outcome for `message_id`
    /// (`written` = the response reached a live socket). DROPPED unless the id
    /// is verify-pending — an outcome nobody is waiting for must not leak.
    pub fn offer_ack(&mut self, message_id: &str, written: bool) {
        if self.verify_pending.contains(message_id) {
            self.acks.insert(message_id.to_string(), written);
        }
    }

    /// Take the ack outcome for `message_id` if one was offered (leaving the
    /// verify marker in place for the caller's loop). `None` → keep waiting.
    pub fn take_ack(&mut self, message_id: &str) -> Option<bool> {
        self.acks.remove(message_id)
    }

    /// End the verify window: clear the marker AND drain any late ack so the
    /// maps return to empty regardless of how the window ended.
    pub fn unmark_verify(&mut self, message_id: &str) {
        self.verify_pending.remove(message_id);
        self.acks.remove(message_id);
    }

    // -----------------------------------------------------------------------
    // Proactive TTL sweep (P-F4) — the eager half of TTL eviction
    // -----------------------------------------------------------------------

    /// Evict EVERY `resolved` entry whose deadline is at-or-before `now`,
    /// returning the count evicted. PURE: touches only the `resolved` map, no IO.
    ///
    /// This is the PROACTIVE counterpart to [`peek_resolved`]'s LAZY eviction.
    /// `peek_resolved` does NOT consume (P-A3 idempotent re-read), so a buffered
    /// reply that is NEVER re-GET would otherwise leak forever — `resolved`
    /// growing unbounded. server.ts gets this for free via a per-entry 5-min
    /// `setTimeout` (server.ts:200) that deletes the entry whether or not it was
    /// ever read; here the M2 sweeper thread calls this method on an interval to
    /// reproduce that bound (P-F4). Without it the lazy half alone cannot cap the
    /// map.
    ///
    /// Same `now: Instant` deadline semantics as `peek_resolved` (`deadline <= now`
    /// is expired) so the lazy + proactive paths agree exactly.
    pub fn sweep_expired(&mut self, now: Instant) -> usize {
        let before = self.resolved.len();
        self.resolved.retain(|_id, entry| entry.deadline > now);
        before - self.resolved.len()
    }
}

impl Default for RelayState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Pure delivery decision (the algorithm heart — no IO)
// ---------------------------------------------------------------------------

/// Pure delivery-routing decision for a `reply` tool call.
///
/// Takes only plain data (no `&mut RelayState` — the caller queries the state
/// and passes results in, so the decision logic is cleanly testable without
/// constructing a full `RelayState`).
///
/// Check order is BINDING and must match server.ts:
/// 1. Waiter present → `ResolveWaiter` (server.ts:205-209).
/// 2. `origin` is `None` → `NoOrigin` (server.ts:135-141).
/// 3. `origin.is_reply` → `LoopPrevented` (server.ts:142-150). P-E4.
/// 4. `origin.from` is `"unknown"` or `"cli"` → `NotAddressable`. P-E5.
/// 5. `origin.from == own_session_id` → `NotAddressable`. P-E5.
/// 6. Otherwise → `PushBack`. P-E3.
///
/// The buffer-first write (`buffer_reply`) MUST happen before this call
/// (caller responsibility — keeps the decision pure). P-E1 / P-G2.
pub fn decide_delivery(
    _message_id: &str,
    waiter_present: bool,
    origin: Option<&OriginRec>,
    own_session_id: &str,
) -> DeliveryDecision {
    // Step 1: waiter wins regardless of origin. server.ts:205.
    if waiter_present {
        return DeliveryDecision::ResolveWaiter;
    }

    // Steps 2-6: push-back path requires an origin record.
    let rec = match origin {
        None => return DeliveryDecision::NoOrigin, // server.ts:135-141
        Some(r) => r,
    };

    // Step 3: loop-prevention belt. server.ts:142-150. P-E4.
    if rec.is_reply {
        return DeliveryDecision::LoopPrevented;
    }

    // Step 4: non-addressable origin values. server.ts:152-153. P-E5.
    if rec.from == "unknown" || rec.from == "cli" {
        return DeliveryDecision::NotAddressable {
            reason: format!("origin \"{}\" is not an addressable session", rec.from),
        };
    }

    // Step 5: origin is this session itself. server.ts:155. P-E5.
    if rec.from == own_session_id {
        return DeliveryDecision::NotAddressable {
            reason: "origin is this session itself".to_string(),
        };
    }

    // Step 6: real, addressable, non-self session. server.ts:151-185. P-E3.
    DeliveryDecision::PushBack {
        origin: rec.from.clone(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // --- is_reply_text ---

    #[test]
    fn is_reply_text_true_for_reply_prefix() {
        assert!(is_reply_text("[REPLY to relay-1234-1] hello"));
    }

    #[test]
    fn is_reply_text_false_for_plain_text() {
        assert!(!is_reply_text("hello world"));
    }

    #[test]
    fn is_reply_text_false_for_empty() {
        assert!(!is_reply_text(""));
    }

    #[test]
    fn is_reply_text_false_for_partial_prefix() {
        // "[REPLY" alone is not the full prefix
        assert!(!is_reply_text("[REPLY hello"));
    }

    #[test]
    fn is_reply_text_true_for_exactly_the_prefix() {
        // "[REPLY to " is the prefix itself — starts_with returns true.
        // Documents the boundary: the check is prefix-only, not full syntax.
        assert!(is_reply_text(REPLY_PREFIX));
    }

    // --- mint_message_id ---

    #[test]
    fn mint_format_relay_ms_seq() {
        let mut state = RelayState::new();
        let id = state.mint_message_id(1_700_000_000_000);
        assert_eq!(id, "relay-1700000000000-1");
    }

    #[test]
    fn mint_seq_increments_monotonically() {
        let mut state = RelayState::new();
        let id1 = state.mint_message_id(100);
        let id2 = state.mint_message_id(100);
        let id3 = state.mint_message_id(200);
        assert_eq!(id1, "relay-100-1");
        assert_eq!(id2, "relay-100-2");
        assert_eq!(id3, "relay-200-3");
    }

    #[test]
    fn mint_seq_starts_at_one_not_zero() {
        // TS `++msgSeq` means first id ends in -1, never -0.
        let mut state = RelayState::new();
        let id = state.mint_message_id(0);
        assert!(id.ends_with("-1"), "first id must end in -1, got: {id}");
    }

    #[test]
    fn mint_seq_monotonic_across_many_calls() {
        let mut state = RelayState::new();
        let ids: Vec<String> = (0..10).map(|i| state.mint_message_id(i)).collect();
        for (i, id) in ids.iter().enumerate() {
            let expected_seq = i + 1;
            assert!(
                id.ends_with(&format!("-{expected_seq}")),
                "id[{i}] = {id}, expected seq {expected_seq}"
            );
        }
    }

    // --- M5a item iv: seq-seed (preserve the relay-<digits>-<digits> invariant) ---

    #[test]
    fn with_seq_seed_first_id_uses_seed_plus_one() {
        // Seeding seq=N means the first minted id ends in -(N+1).
        let mut state = RelayState::with_seq_seed(500);
        let id = state.mint_message_id(1_700_000_000_000);
        assert_eq!(id, "relay-1700000000000-501");
    }

    #[test]
    fn two_fresh_seeds_mint_distinct_ids_at_same_ms() {
        // The CORE item-iv property: two FRESH servers (different seq seeds) minting
        // in the SAME ms produce DIFFERENT ids — killing the shared-inbox filename
        // collision that two `relay-<ms>-1` ids would cause.
        let mut a = RelayState::with_seq_seed(1000);
        let mut b = RelayState::with_seq_seed(2000);
        let same_ms = 1_700_000_000_000u64;
        let id_a = a.mint_message_id(same_ms);
        let id_b = b.mint_message_id(same_ms);
        assert_ne!(
            id_a, id_b,
            "two fresh servers with different seeds must mint distinct ids at the same ms"
        );
        // Both still carry the SAME (untouched) epoch in position 2 (ruling iv).
        assert!(id_a.starts_with("relay-1700000000000-"));
        assert!(id_b.starts_with("relay-1700000000000-"));
    }

    #[test]
    fn seeded_id_preserves_relay_digits_digits_shape() {
        // ★ Consumer-audit guard: the well-formed shape `relay-<digits>-<digits>`
        // (exactly 3 segments, positions 2 and 3 all-ASCII-digits) MUST survive
        // seeding — this is what `is_well_formed_message_id` (relay_server_mcp_delivery
        // test) and any `split('-')` epoch-extractor relies on.
        let mut state = RelayState::with_seq_seed(0xFFFF_FFFF); // max seed
        let id = state.mint_message_id(1_700_000_000_000);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 3, "exactly three '-' segments: {id}");
        assert_eq!(parts[0], "relay");
        // Position 2 = the UNTOUCHED epoch — all digits, non-empty.
        assert!(!parts[1].is_empty() && parts[1].bytes().all(|b| b.is_ascii_digit()));
        // Position 3 = the SEEDED seq — still all digits, non-empty.
        assert!(!parts[2].is_empty() && parts[2].bytes().all(|b| b.is_ascii_digit()));
        // The epoch is exactly what we passed (not perturbed by the seed).
        assert_eq!(parts[1], "1700000000000");
    }

    // --- origin FIFO ---

    #[test]
    fn origin_record_and_lookup() {
        let mut state = RelayState::new();
        state.record_origin("msg-1".into(), "session-A".into(), false);
        let rec = state.origin_for("msg-1").expect("should be present");
        assert_eq!(rec.from, "session-A");
        assert!(!rec.is_reply);
    }

    #[test]
    fn origin_absent_returns_none() {
        let state = RelayState::new();
        assert!(state.origin_for("no-such-id").is_none());
    }

    #[test]
    fn origin_is_reply_flag_stored_correctly() {
        let mut state = RelayState::new();
        state.record_origin("msg-r".into(), "session-B".into(), true);
        let rec = state.origin_for("msg-r").expect("present");
        assert!(rec.is_reply);
    }

    #[test]
    fn origin_fifo_evicts_oldest_at_capacity() {
        let mut state = RelayState::new();
        // Fill to capacity.
        for i in 0..MAX_TRACKED_ORIGINS {
            state.record_origin(format!("msg-{i}"), format!("s-{i}"), false);
        }
        // All entries present.
        assert!(state.origin_for("msg-0").is_some());
        assert!(state
            .origin_for(&format!("msg-{}", MAX_TRACKED_ORIGINS - 1))
            .is_some());

        // Insert one more — should evict "msg-0" (oldest).
        state.record_origin("msg-new".into(), "s-new".into(), false);

        assert!(
            state.origin_for("msg-0").is_none(),
            "oldest entry must be evicted"
        );
        assert!(
            state.origin_for("msg-new").is_some(),
            "newest entry must survive"
        );
    }

    #[test]
    fn origin_fifo_recent_entries_survive_eviction() {
        let mut state = RelayState::new();
        // Fill to capacity.
        for i in 0..MAX_TRACKED_ORIGINS {
            state.record_origin(format!("old-{i}"), "s".into(), false);
        }
        // Insert 50 more — evicts first 50.
        for i in 0..50 {
            state.record_origin(format!("new-{i}"), "s".into(), false);
        }
        // First 50 evicted.
        for i in 0..50 {
            assert!(
                state.origin_for(&format!("old-{i}")).is_none(),
                "old-{i} should be evicted"
            );
        }
        // Entries 50..MAX still present.
        assert!(state.origin_for("old-50").is_some());
        // New entries present.
        for i in 0..50 {
            assert!(state.origin_for(&format!("new-{i}")).is_some());
        }
    }

    #[test]
    fn origin_reinsertion_does_not_duplicate_fifo_order() {
        // Re-inserting the same message_id must NOT add a second entry to
        // origins_order — otherwise eviction logic would break the size invariant.
        // This matches TS Map.set semantics (overwrite without position change).
        let mut state = RelayState::new();
        state.record_origin("msg-dup".into(), "session-A".into(), false);
        // Overwrite with a new value.
        state.record_origin("msg-dup".into(), "session-B".into(), true);

        // Value updated.
        let rec = state.origin_for("msg-dup").expect("should be present");
        assert_eq!(rec.from, "session-B");
        assert!(rec.is_reply);

        // origins_order must still have exactly ONE entry for msg-dup.
        // We can verify indirectly: fill to capacity, then insert one new entry;
        // only ONE eviction should happen.
        state = RelayState::new();
        // Insert msg-dup once.
        state.record_origin("msg-dup".into(), "A".into(), false);
        // Fill remaining capacity with unique ids.
        for i in 0..(MAX_TRACKED_ORIGINS - 1) {
            state.record_origin(format!("fill-{i}"), "s".into(), false);
        }
        assert_eq!(state.origins.len(), MAX_TRACKED_ORIGINS);

        // Re-insert msg-dup — must be a no-op on size (doesn't push to order).
        state.record_origin("msg-dup".into(), "B".into(), true);
        // Still at MAX capacity (no eviction needed — no new entry was added).
        assert_eq!(state.origins.len(), MAX_TRACKED_ORIGINS);
        // msg-dup is still present (updated value).
        let rec2 = state.origin_for("msg-dup").expect("msg-dup must survive");
        assert_eq!(rec2.from, "B");
    }

    // --- buffer_reply + peek_resolved ---

    #[test]
    fn buffer_and_peek_within_ttl() {
        let mut state = RelayState::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        state.buffer_reply("msg-1".into(), "hello".into(), deadline);
        // peek with now() well before deadline
        let result = state.peek_resolved("msg-1", Instant::now());
        assert_eq!(result, Some("hello".to_string()));
    }

    #[test]
    fn peek_resolved_past_deadline_returns_none_and_evicts() {
        let mut state = RelayState::new();
        // Deadline already in the past.
        let deadline = Instant::now() - Duration::from_secs(1);
        state.buffer_reply("msg-expired".into(), "stale".into(), deadline);

        // Should return None and evict.
        let result = state.peek_resolved("msg-expired", Instant::now());
        assert_eq!(result, None, "expired entry must return None");

        // Entry must be gone (lazily evicted on the expired read).
        let result2 = state.peek_resolved("msg-expired", Instant::now());
        assert_eq!(result2, None, "evicted entry must stay None");
    }

    #[test]
    fn peek_resolved_absent_id_returns_none() {
        let mut state = RelayState::new();
        assert_eq!(state.peek_resolved("no-such-id", Instant::now()), None);
    }

    #[test]
    fn peek_resolved_is_idempotent_within_ttl() {
        // PEEK must NOT consume: a --wait client whose long-poll HTTP response
        // is lost on the wire RE-GETs /replies/<id> and must get the SAME text.
        // server.ts:303-306 is a non-destructive `.get`. Consume = 61-loss class.
        let mut state = RelayState::new();
        let deadline = Instant::now() + Duration::from_secs(300);
        state.buffer_reply("msg-1".into(), "idempotent".into(), deadline);

        // First peek succeeds.
        assert_eq!(
            state.peek_resolved("msg-1", Instant::now()),
            Some("idempotent".to_string())
        );
        // Second peek within TTL returns the SAME text (entry NOT consumed).
        assert_eq!(
            state.peek_resolved("msg-1", Instant::now()),
            Some("idempotent".to_string()),
            "peek must be idempotent within TTL — entry must survive the first read"
        );
        // Third read for good measure.
        assert_eq!(
            state.peek_resolved("msg-1", Instant::now()),
            Some("idempotent".to_string())
        );
    }

    // --- decide_delivery ---

    #[test]
    fn decide_waiter_present_wins_regardless_of_origin() {
        // Waiter wins even if origin would be LoopPrevented.
        let origin = OriginRec {
            from: "session-X".into(),
            is_reply: true,
        };
        let decision = decide_delivery("msg-1", true, Some(&origin), "self-session");
        assert_eq!(decision, DeliveryDecision::ResolveWaiter);
    }

    #[test]
    fn decide_waiter_present_wins_with_no_origin() {
        let decision = decide_delivery("msg-1", true, None, "self-session");
        assert_eq!(decision, DeliveryDecision::ResolveWaiter);
    }

    #[test]
    fn decide_no_origin_record() {
        let decision = decide_delivery("msg-1", false, None, "self-session");
        assert_eq!(decision, DeliveryDecision::NoOrigin);
    }

    #[test]
    fn decide_loop_prevented_is_reply_origin() {
        let origin = OriginRec {
            from: "session-X".into(),
            is_reply: true,
        };
        let decision = decide_delivery("msg-1", false, Some(&origin), "self-session");
        assert_eq!(decision, DeliveryDecision::LoopPrevented);
    }

    #[test]
    fn decide_not_addressable_unknown() {
        let origin = OriginRec {
            from: "unknown".into(),
            is_reply: false,
        };
        let decision = decide_delivery("msg-1", false, Some(&origin), "self-session");
        assert!(
            matches!(decision, DeliveryDecision::NotAddressable { .. }),
            "unexpected: {decision:?}"
        );
    }

    #[test]
    fn decide_not_addressable_cli() {
        let origin = OriginRec {
            from: "cli".into(),
            is_reply: false,
        };
        let decision = decide_delivery("msg-1", false, Some(&origin), "self-session");
        assert!(
            matches!(decision, DeliveryDecision::NotAddressable { .. }),
            "unexpected: {decision:?}"
        );
    }

    #[test]
    fn decide_not_addressable_self() {
        let origin = OriginRec {
            from: "self-session".into(),
            is_reply: false,
        };
        let decision = decide_delivery("msg-1", false, Some(&origin), "self-session");
        assert!(
            matches!(decision, DeliveryDecision::NotAddressable { .. }),
            "unexpected: {decision:?}"
        );
    }

    #[test]
    fn decide_push_back_real_origin() {
        let origin = OriginRec {
            from: "session-other".into(),
            is_reply: false,
        };
        let decision = decide_delivery("msg-1", false, Some(&origin), "self-session");
        assert_eq!(
            decision,
            DeliveryDecision::PushBack {
                origin: "session-other".into()
            }
        );
    }

    /// Assert the exact check ORDER: waiter-present beats LoopPrevented.
    #[test]
    fn decide_order_waiter_beats_loop_prevented() {
        let origin = OriginRec {
            from: "some-session".into(),
            is_reply: true,
        };
        // Waiter present AND is_reply origin — waiter must win.
        let decision = decide_delivery("msg-1", true, Some(&origin), "self");
        assert_eq!(
            decision,
            DeliveryDecision::ResolveWaiter,
            "waiter must beat loop-prevented in check order"
        );
    }

    /// Assert the exact check ORDER: LoopPrevented beats NotAddressable.
    /// (is_reply + origin == "unknown" — loop check is first.)
    #[test]
    fn decide_order_loop_prevented_beats_not_addressable() {
        let origin = OriginRec {
            from: "unknown".into(),
            is_reply: true,
        };
        let decision = decide_delivery("msg-1", false, Some(&origin), "self");
        assert_eq!(
            decision,
            DeliveryDecision::LoopPrevented,
            "loop-prevented must beat not-addressable in check order"
        );
    }

    /// Assert the exact check ORDER: NoOrigin is second (after waiter check).
    #[test]
    fn decide_order_no_origin_second() {
        // No waiter, no origin → NoOrigin (not some other variant).
        let decision = decide_delivery("msg-1", false, None, "self");
        assert_eq!(decision, DeliveryDecision::NoOrigin);
    }

    // --- sweep_expired (P-F4) ---

    #[test]
    fn sweep_expired_evicts_expired_keeps_live_returns_count() {
        let mut state = RelayState::new();
        let now = Instant::now();
        // Two already-expired (deadline in the past) + two live (future deadline).
        state.buffer_reply("dead-1".into(), "a".into(), now - Duration::from_secs(1));
        state.buffer_reply("dead-2".into(), "b".into(), now - Duration::from_secs(10));
        state.buffer_reply("live-1".into(), "c".into(), now + Duration::from_secs(60));
        state.buffer_reply("live-2".into(), "d".into(), now + Duration::from_secs(300));

        let evicted = state.sweep_expired(now);
        assert_eq!(evicted, 2, "exactly the two expired entries are swept");

        // Live entries survive and remain readable (peek non-destructive).
        assert_eq!(state.peek_resolved("live-1", now), Some("c".to_string()));
        assert_eq!(state.peek_resolved("live-2", now), Some("d".to_string()));
        // Expired entries are gone.
        assert_eq!(state.peek_resolved("dead-1", now), None);
        assert_eq!(state.peek_resolved("dead-2", now), None);
    }

    #[test]
    fn sweep_expired_empty_state_returns_zero() {
        let mut state = RelayState::new();
        assert_eq!(state.sweep_expired(Instant::now()), 0);
    }

    #[test]
    fn sweep_expired_nothing_expired_returns_zero() {
        let mut state = RelayState::new();
        let now = Instant::now();
        state.buffer_reply("live".into(), "x".into(), now + Duration::from_secs(60));
        assert_eq!(state.sweep_expired(now), 0, "no live entry is swept");
        assert_eq!(state.peek_resolved("live", now), Some("x".to_string()));
    }

    #[test]
    fn sweep_expired_deadline_exactly_now_is_evicted() {
        // Boundary parity with peek_resolved: `deadline <= now` is expired.
        let mut state = RelayState::new();
        let now = Instant::now();
        state.buffer_reply("boundary".into(), "x".into(), now);
        assert_eq!(state.sweep_expired(now), 1, "deadline == now is expired");
    }

    // --- B2 item 4: resolve-and-verify ack handshake -----------------------

    #[test]
    fn ack_offered_under_marker_is_taken_once() {
        let mut state = RelayState::new();
        state.mark_verify("m1");
        state.offer_ack("m1", true);
        assert_eq!(state.take_ack("m1"), Some(true), "the offered ack is taken");
        assert_eq!(state.take_ack("m1"), None, "taken exactly once");
        state.unmark_verify("m1");
    }

    #[test]
    fn ack_without_marker_is_dropped_no_orphans() {
        // The orphan invariant: an outcome nobody is waiting for never lands.
        let mut state = RelayState::new();
        state.offer_ack("m2", true);
        assert_eq!(
            state.take_ack("m2"),
            None,
            "an offer with no verify window open is dropped"
        );
    }

    #[test]
    fn unmark_drains_late_ack() {
        // A late ack landing before unmark is drained WITH the marker, so the
        // maps return to empty however the verify window ends.
        let mut state = RelayState::new();
        state.mark_verify("m3");
        state.offer_ack("m3", false);
        state.unmark_verify("m3");
        assert_eq!(state.take_ack("m3"), None, "unmark drains the pending ack");
        // And post-unmark offers are dropped (marker gone).
        state.offer_ack("m3", true);
        assert_eq!(state.take_ack("m3"), None);
    }

    #[test]
    fn nack_overwrites_nothing_and_reports_false() {
        let mut state = RelayState::new();
        state.mark_verify("m4");
        state.offer_ack("m4", false);
        assert_eq!(
            state.take_ack("m4"),
            Some(false),
            "a nack (dead socket / failed write) reports false"
        );
        state.unmark_verify("m4");
    }
}
