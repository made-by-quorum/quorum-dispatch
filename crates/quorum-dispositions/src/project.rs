//! The pure projection: envelopes ∪ disposition-events → per-id summary records
//! (format doc §3).
//!
//! Under R8/R8a/R8b, `dispositions.jsonl` is an append-only log of typed
//! witnessed EVENTS ([`DispositionEvent`]), never state records. State is a
//! VIEW, folded here: [`project_summary`] emits one [`SummaryRecord`] per
//! `correlation_id` (the `qd dispositions` DEFAULT output), carrying the coarse
//! 4-state [`SummaryState`] UNCHANGED (guard 2) plus `last_event`/`witness` and
//! the analytics fields. `qd dispositions --events` emits the raw event rows.
//!
//! The fold is what fixes the live bug the old "first terminal wins" collapse
//! caused: a `delivery-failed@t1 → delivered@t3` funnel now summarizes to
//! `delivered` (a delivered event EXISTS, [`crate::has_delivered`]), never
//! `failed` forever.
//!
//! The `last_event`/`witness` pick is MAX by `(witnessed_at, witness)`
//! lexicographic; on a FULL tie the LATER-IN-INPUT row wins (R11.2 — within one
//! witness's file, file order IS witnessed order; the ms timestamp is a lossy
//! projection of append order). PAIRED-NULL invariant (R11.1): `last_event` and
//! `witness` are null together, exactly when no events exist.
//!
//! Output order is deterministic (for golden tests): envelopes in input order,
//! then orphan-event ids (events with no envelope in scope) in input order.

use std::collections::HashMap;

use crate::record::{DispositionEvent, Envelope, EventKind, SummaryRecord, SummaryState};

/// Whether a `delivered` event exists for `correlation_id` in `events`. This is
/// the idempotence + delivered-view predicate (R8: "delivered = a delivered
/// event EXISTS, irreversible"). Pure; the store uses it in phase 2.
pub fn has_delivered(events: &[DispositionEvent], correlation_id: &str) -> bool {
    events
        .iter()
        .any(|e| e.event == EventKind::Delivered && e.correlation_id == correlation_id)
}

/// Derive the coarse published [`SummaryState`] from the folded view inputs.
///
/// RATIFIED (R10). Kept as an isolated fn so the precedence stays auditable in
/// one place. Precedence (highest first) — delivered > expired > failed >
/// pending:
///
/// 1. `Delivered` — a delivered event EXISTS: the only absorbing state
///    (irreversible).
/// 2. `Expired`   — expired > failed is the contract transported: delivery-failed
///    is not terminal under the retry model; expired = no delivered event by the
///    envelope's own `expires_at`, failure history or none.
/// 3. `Failed`    — the latest event is `delivery-failed`, awaiting retry.
/// 4. `Pending`   — otherwise (latest is accepted/attempted/queued, or none).
fn derive_state(
    has_delivered: bool,
    expires_at: Option<i64>,
    last_event: Option<EventKind>,
    now_ms: i64,
) -> SummaryState {
    if has_delivered {
        SummaryState::Delivered
    } else if expires_at.is_some_and(|exp| now_ms >= exp) {
        SummaryState::Expired
    } else if last_event == Some(EventKind::DeliveryFailed) {
        SummaryState::Failed
    } else {
        SummaryState::Pending
    }
}

/// A per-id accumulator folded from the event stream (in input order).
struct Fold<'a> {
    /// The `last_event` pick: MAX by `(witnessed_at, witness)` lexicographic;
    /// on a FULL tie (equal witnessed_at AND equal witness) the LATER-IN-INPUT
    /// row wins (R11.2 — within one witness's file, file order IS witnessed
    /// order). Across witnesses at equal witnessed_at the lexicographically
    /// greater witness wins — arbitrary but deterministic under ANY union order.
    last_event: EventKind,
    last_event_at: i64,
    /// The witness of the `last_event` pick (published as summary `witness`).
    last_witness: &'a str,
    attempts: u32,
    last_attempt_at: Option<i64>,
    first_delivered_at: Option<i64>,
    has_delivered: bool,
    /// authored_at/origin from the FIRST event (used only when no envelope is
    /// in scope — an orphan-event summary; every event carries `origin`, R11).
    first_authored_at: i64,
    first_origin: &'a str,
}

impl<'a> Fold<'a> {
    fn new(ev: &'a DispositionEvent) -> Self {
        let mut f = Fold {
            last_event: ev.event,
            last_event_at: i64::MIN,
            last_witness: "",
            attempts: 0,
            last_attempt_at: None,
            first_delivered_at: None,
            has_delivered: false,
            first_authored_at: ev.authored_at,
            first_origin: &ev.origin,
        };
        f.absorb(ev);
        f
    }

    fn absorb(&mut self, ev: &'a DispositionEvent) {
        // R11.2 pick: MAX by (witnessed_at, witness) lexicographic. Replace on
        // `>=` so a FULL tie (equal witnessed_at AND equal witness) lets the
        // LATER-IN-INPUT row win — within one witness's file, file order IS
        // witnessed order (the ms timestamp is a lossy projection of append
        // order). The old strict-`>` fold was a ruled BUG (a same-instant retry
        // kept the earlier row).
        if (ev.witnessed_at, ev.witness.as_str()) >= (self.last_event_at, self.last_witness) {
            self.last_event = ev.event;
            self.last_event_at = ev.witnessed_at;
            self.last_witness = &ev.witness;
        }
        match ev.event {
            EventKind::Attempted => {
                self.attempts += 1;
                self.last_attempt_at =
                    Some(self.last_attempt_at.map_or(ev.witnessed_at, |m| m.max(ev.witnessed_at)));
            }
            EventKind::Delivered => {
                self.has_delivered = true;
                self.first_delivered_at =
                    Some(self.first_delivered_at.map_or(ev.witnessed_at, |m| m.min(ev.witnessed_at)));
            }
            _ => {}
        }
    }
}

/// Project an envelope stream ∪ a disposition-EVENT stream into one
/// [`SummaryRecord`] per `correlation_id` at wall-clock `now_ms` (epoch-ms).
/// Pure and deterministic.
///
/// Per id: `attempts` = count of `attempted`; `last_event`/`witness` = the
/// event MAX by `(witnessed_at, witness)` (full tie → later-in-input, R11.2),
/// both `None` iff no events (R11.1); `last_attempt_at` = max `witnessed_at`
/// over `attempted` (else None); `first_delivered_at` = min `witnessed_at` over
/// `delivered` (else None). `expires_at`/`authored_at`/`origin` come from the
/// joined [`Envelope`] when in scope, else `expires_at = None` and
/// `authored_at`/`origin` from the (first) event.
///
/// Output order: envelopes in input order, then orphan-event ids in input order.
pub fn project_summary(
    envelopes: &[Envelope],
    events: &[DispositionEvent],
    now_ms: i64,
) -> Vec<SummaryRecord> {
    // Fold the event stream once, keyed by correlation_id, preserving the
    // first-appearance order of orphan ids.
    let mut folds: HashMap<&str, Fold> = HashMap::new();
    let mut event_id_order: Vec<&str> = Vec::new();
    for ev in events {
        let id = ev.correlation_id.as_str();
        folds
            .entry(id)
            .and_modify(|f| f.absorb(ev))
            .or_insert_with(|| {
                event_id_order.push(id);
                Fold::new(ev)
            });
    }

    let mut out = Vec::new();
    let mut seen_env: HashMap<&str, ()> = HashMap::new();

    // Pass 1 — envelopes in input order, dedup by correlation_id (first wins).
    for env in envelopes {
        let id = env.correlation_id.as_str();
        if seen_env.insert(id, ()).is_some() {
            continue; // duplicate envelope id already emitted
        }
        out.push(summary_for(
            id,
            folds.get(id),
            Some(env),
            now_ms,
        ));
    }

    // Pass 2 — orphan-event ids (no envelope in scope) in first-appearance order.
    for id in event_id_order {
        if seen_env.contains_key(id) {
            continue; // has an envelope → handled in pass 1
        }
        out.push(summary_for(id, folds.get(id), None, now_ms));
    }

    out
}

/// Build one summary from a fold (present unless the id came from an envelope
/// with no events) joined to its envelope (present unless orphan-event).
fn summary_for(
    id: &str,
    fold: Option<&Fold>,
    env: Option<&Envelope>,
    now_ms: i64,
) -> SummaryRecord {
    // {last_event, witness}: PAIRED-NULL (R11.1) — both from the fold's pick,
    // both None exactly when no events exist. An envelope with NO events has no
    // witnessed moment; a summary never reports one nobody witnessed (the old
    // fabricated-`accepted` default is OVERRULED — it poisoned
    // `WHERE last_event='accepted'` views).
    let last_event = fold.map(|f| f.last_event);
    let witness = fold.map(|f| f.last_witness.to_string());
    let has_delivered = fold.is_some_and(|f| f.has_delivered);
    let attempts = fold.map_or(0, |f| f.attempts);
    let last_attempt_at = fold.and_then(|f| f.last_attempt_at);
    let first_delivered_at = fold.and_then(|f| f.first_delivered_at);

    let expires_at = env.map(|e| e.expires_at);
    // authored_at/origin: prefer the envelope (origin truth); else the (first)
    // event — every event carries `origin` (R11), so there is no nullable escape.
    let (authored_at, origin) = match env {
        Some(e) => (e.authored_at, e.origin.clone()),
        None => {
            let f = fold.expect("orphan id has at least one event");
            (f.first_authored_at, f.first_origin.to_string())
        }
    };

    let state = derive_state(has_delivered, expires_at, last_event, now_ms);

    SummaryRecord {
        v: 1,
        correlation_id: id.to_string(),
        state,
        attempts,
        last_event,
        last_attempt_at,
        first_delivered_at,
        expires_at,
        authored_at,
        origin,
        witness,
    }
}

/// Point-query convenience: project, then filter to a single `correlation_id`.
/// Each id yields AT MOST one summary, so this returns `Option`.
pub fn project_one(
    envelopes: &[Envelope],
    events: &[DispositionEvent],
    now_ms: i64,
    correlation_id: &str,
) -> Option<SummaryRecord> {
    project_summary(envelopes, events, now_ms)
        .into_iter()
        .find(|r| r.correlation_id == correlation_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(id: &str, authored: i64, expires: i64) -> Envelope {
        Envelope {
            v: 1,
            correlation_id: id.to_string(),
            authored_at: authored,
            expires_at: expires,
            target: "t".to_string(),
            origin: "origin-host".to_string(),
            body: "b".to_string(),
        }
    }

    // ---- the funnel-at-projection-level test (§6 amendment, build FIRST) ----

    #[test]
    fn fail_then_retry_then_succeed_folds_to_delivered() {
        // The sequence that the OLD "first terminal wins" collapse resolved to
        // FAILED forever. Now it folds to Delivered.
        // [attempted@t1, delivery-failed@t1(wake), attempted@t2, queued@t2, delivered@t3]
        let (t1, t2, t3) = (100, 200, 300);
        let events = vec![
            DispositionEvent::attempted("a".into(), t1, "h".into(), "o".into(), 10),
            DispositionEvent::delivery_failed("a".into(), t1, "h".into(), "o".into(), 10, "wake".into()),
            DispositionEvent::attempted("a".into(), t2, "h".into(), "o".into(), 10),
            DispositionEvent::queued("a".into(), t2, "h".into(), "o".into(), 10),
            DispositionEvent::delivered("a".into(), t3, "h".into(), "o".into(), 10),
        ];
        let envs = [env("a", 10, 100_000)];
        let out = project_summary(&envs, &events, 400);
        assert_eq!(out.len(), 1);
        let s = &out[0];
        assert_eq!(s.state, SummaryState::Delivered, "delivered event exists → Delivered");
        assert_eq!(s.attempts, 2, "two attempted events");
        assert_eq!(s.last_event, Some(EventKind::Delivered), "latest by witnessed_at is delivered@t3");
        assert_eq!(s.witness.as_deref(), Some("h"), "witness of the last_event pick");
        assert_eq!(s.first_delivered_at, Some(t3));
        assert_eq!(s.last_attempt_at, Some(t2), "max witnessed over attempted");
        assert_eq!(s.expires_at, Some(100_000), "from the envelope");
        assert_eq!(s.authored_at, 10, "from the envelope");
        assert_eq!(s.origin, "origin-host", "from the envelope");
    }

    // ---- R11.2 tie-break: the ruled strict-`>` bug scenarios ----

    #[test]
    fn same_instant_retry_within_one_witness_last_row_wins() {
        // delivery-failed@t then attempted@t (SAME witness): the later-in-input
        // row wins the pick → last_event=Attempted → Pending. (The old
        // strict-`>` fold kept DeliveryFailed → Failed — the ruled bug.)
        let envs = [env("a", 10, 1_000_000)];
        let t = 500;
        let events = [
            DispositionEvent::delivery_failed("a".into(), t, "h".into(), "o".into(), 10, "wake".into()),
            DispositionEvent::attempted("a".into(), t, "h".into(), "o".into(), 10),
        ];
        let out = project_summary(&envs, &events, 600);
        assert_eq!(out[0].last_event, Some(EventKind::Attempted), "file-last row at the tie wins");
        assert_eq!(out[0].witness.as_deref(), Some("h"));
        assert_eq!(out[0].state, SummaryState::Pending, "retry in flight, not Failed");
    }

    #[test]
    fn whole_funnel_compressed_into_one_instant_folds_to_delivered() {
        // The §6 scenario with ALL five rows at the same witnessed_at, same
        // witness: file order is witnessed order → the file-last row (delivered)
        // is the pick, and state is Delivered.
        let envs = [env("a", 10, 1_000_000)];
        let t = 500;
        let events = [
            DispositionEvent::attempted("a".into(), t, "h".into(), "o".into(), 10),
            DispositionEvent::delivery_failed("a".into(), t, "h".into(), "o".into(), 10, "wake".into()),
            DispositionEvent::attempted("a".into(), t, "h".into(), "o".into(), 10),
            DispositionEvent::queued("a".into(), t, "h".into(), "o".into(), 10),
            DispositionEvent::delivered("a".into(), t, "h".into(), "o".into(), 10),
        ];
        let out = project_summary(&envs, &events, 600);
        assert_eq!(out[0].state, SummaryState::Delivered);
        assert_eq!(out[0].last_event, Some(EventKind::Delivered), "the file-last row");
        assert_eq!(out[0].witness.as_deref(), Some("h"));
        assert_eq!(out[0].attempts, 2);
        assert_eq!(out[0].first_delivered_at, Some(t));
        assert_eq!(out[0].last_attempt_at, Some(t));
    }

    #[test]
    fn cross_witness_equal_instant_greater_witness_wins_any_order() {
        // delivered@t witness "a", delivery-failed@t witness "b": (t,"b") >
        // (t,"a") lexicographic → last_event=DeliveryFailed with witness "b";
        // state=Delivered (a delivered event exists). SAME result under either
        // union order (determinism under ANY union order).
        let envs = [env("x", 10, 1_000_000)];
        let t = 500;
        let del_a = DispositionEvent::delivered("x".into(), t, "a".into(), "o".into(), 10);
        let fail_b = DispositionEvent::delivery_failed("x".into(), t, "b".into(), "o".into(), 10, "wake".into());
        for events in [
            vec![del_a.clone(), fail_b.clone()],
            vec![fail_b, del_a],
        ] {
            let out = project_summary(&envs, &events, 600);
            assert_eq!(out[0].last_event, Some(EventKind::DeliveryFailed), "b > a lexicographic");
            assert_eq!(out[0].witness.as_deref(), Some("b"));
            assert_eq!(out[0].state, SummaryState::Delivered, "delivered exists → Delivered");
        }
    }

    // ---- derive_state matrix (RATIFIED R10) ----

    #[test]
    fn derive_state_delivered_wins_over_everything() {
        // delivered-exists → Delivered even past expiry / after a later failure.
        assert_eq!(
            derive_state(true, Some(10), Some(EventKind::DeliveryFailed), 1_000_000),
            SummaryState::Delivered
        );
    }

    #[test]
    fn derive_state_expired_when_no_delivery_past_expiry() {
        assert_eq!(
            derive_state(false, Some(1000), Some(EventKind::Queued), 1000),
            SummaryState::Expired,
            "now == expires_at → expired (>=)"
        );
        assert_eq!(
            derive_state(false, Some(1000), Some(EventKind::Attempted), 1001),
            SummaryState::Expired
        );
        // Zero events + past expiry → still Expired (expired keys on absence).
        assert_eq!(derive_state(false, Some(1000), None, 1001), SummaryState::Expired);
    }

    #[test]
    fn derive_state_failed_when_last_is_delivery_failed_pre_expiry() {
        assert_eq!(
            derive_state(false, Some(1_000_000), Some(EventKind::DeliveryFailed), 5),
            SummaryState::Failed
        );
        // no expires_at, last failed → Failed
        assert_eq!(
            derive_state(false, None, Some(EventKind::DeliveryFailed), 5),
            SummaryState::Failed
        );
    }

    #[test]
    fn derive_state_pending_when_last_is_attempt_or_queued_or_none() {
        assert_eq!(
            derive_state(false, Some(1_000_000), Some(EventKind::Attempted), 5),
            SummaryState::Pending
        );
        assert_eq!(
            derive_state(false, Some(1_000_000), Some(EventKind::Queued), 5),
            SummaryState::Pending
        );
        assert_eq!(
            derive_state(false, Some(1_000_000), Some(EventKind::Accepted), 5),
            SummaryState::Pending
        );
        // No events at all → Pending (the "or none" arm).
        assert_eq!(derive_state(false, Some(1_000_000), None, 5), SummaryState::Pending);
        assert_eq!(derive_state(false, None, None, 5), SummaryState::Pending);
    }

    #[test]
    fn derive_state_orphan_never_expired() {
        // No envelope → expires_at None → never Expired, whatever now is.
        assert_eq!(
            derive_state(true, None, Some(EventKind::Delivered), i64::MAX),
            SummaryState::Delivered
        );
        assert_eq!(
            derive_state(false, None, Some(EventKind::DeliveryFailed), i64::MAX),
            SummaryState::Failed
        );
        assert_eq!(
            derive_state(false, None, Some(EventKind::Queued), i64::MAX),
            SummaryState::Pending
        );
    }

    // ---- has_delivered ----

    #[test]
    fn has_delivered_true_and_false() {
        let events = vec![
            DispositionEvent::attempted("a".into(), 1, "h".into(), "o".into(), 0),
            DispositionEvent::delivered("a".into(), 2, "h".into(), "o".into(), 0),
            DispositionEvent::delivery_failed("b".into(), 3, "h".into(), "o".into(), 0, "wake".into()),
        ];
        assert!(has_delivered(&events, "a"));
        assert!(!has_delivered(&events, "b"), "b only failed");
        assert!(!has_delivered(&events, "missing"));
    }

    // ---- projection basics ----

    #[test]
    fn envelope_only_no_events_has_paired_nulls_and_is_pending_pre_expiry() {
        let envs = [env("a", 10, 1000)];
        let out = project_summary(&envs, &[], 500);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, SummaryState::Pending);
        // R11.1 paired-null invariant: no witnessed moment → BOTH null.
        assert_eq!(out[0].last_event, None, "no fabricated accepted");
        assert_eq!(out[0].witness, None, "null together with last_event");
        assert_eq!(out[0].attempts, 0);
        assert_eq!(out[0].last_attempt_at, None);
        assert_eq!(out[0].first_delivered_at, None);
        assert_eq!(out[0].origin, "origin-host");
    }

    #[test]
    fn envelope_only_post_expiry_is_expired_with_paired_nulls() {
        let envs = [env("a", 10, 1000)];
        let out = project_summary(&envs, &[], 1000);
        assert_eq!(out[0].state, SummaryState::Expired);
        assert_eq!(out[0].last_event, None);
        assert_eq!(out[0].witness, None);
    }

    #[test]
    fn failed_only_pre_expiry_is_failed() {
        let envs = [env("a", 10, 1_000_000)];
        let events =
            [DispositionEvent::delivery_failed("a".into(), 60, "wit".into(), "o".into(), 10, "wake".into())];
        let out = project_summary(&envs, &events, 100);
        assert_eq!(out[0].state, SummaryState::Failed);
        assert_eq!(out[0].last_event, Some(EventKind::DeliveryFailed));
        assert_eq!(out[0].witness.as_deref(), Some("wit"));
    }

    #[test]
    fn orphan_event_summary_no_envelope() {
        // A delivered event whose envelope is not in scope → summary from the
        // event alone: expires_at None, never Expired; origin from the event's
        // `origin` field (NOT the witness); witness = the last event's witness.
        let events = [DispositionEvent::delivered("orphan".into(), 700, "wit".into(), "orig-o".into(), 42)];
        let out = project_summary(&[], &events, i64::MAX);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].correlation_id, "orphan");
        assert_eq!(out[0].state, SummaryState::Delivered);
        assert_eq!(out[0].expires_at, None, "no envelope → no expires_at");
        assert_eq!(out[0].authored_at, 42, "authored_at from the event");
        assert_eq!(out[0].origin, "orig-o", "origin from the event's origin, NOT the witness");
        assert_eq!(out[0].witness.as_deref(), Some("wit"), "witness of the last event");
        assert_eq!(out[0].first_delivered_at, Some(700));
    }

    #[test]
    fn orphan_failed_never_expired_even_far_future() {
        let events =
            [DispositionEvent::delivery_failed("o".into(), 5, "wit".into(), "og".into(), 1, "wake".into())];
        let out = project_summary(&[], &events, i64::MAX);
        assert_eq!(out[0].state, SummaryState::Failed, "orphan → never Expired");
        assert_eq!(out[0].origin, "og");
    }

    #[test]
    fn output_order_envelopes_then_orphan_events() {
        let envs = [env("e1", 1, 1000), env("e2", 2, 1000)];
        let events = [
            DispositionEvent::delivered("e1".into(), 10, "h".into(), "o".into(), 1),
            DispositionEvent::delivery_failed("orphanA".into(), 20, "h".into(), "o".into(), 1, "x".into()),
            DispositionEvent::delivered("orphanB".into(), 30, "h".into(), "o".into(), 1),
        ];
        let out = project_summary(&envs, &events, 5);
        let ids: Vec<&str> = out.iter().map(|r| r.correlation_id.as_str()).collect();
        assert_eq!(ids, vec!["e1", "e2", "orphanA", "orphanB"]);
        assert_eq!(out[1].state, SummaryState::Pending, "e2 no events → pending");
    }

    #[test]
    fn last_event_is_latest_by_witnessed_at_not_input_order() {
        // delivery-failed@t=800 arrives in input BEFORE delivered@t=500, but
        // last_event picks the latest witnessed_at (800) → DeliveryFailed;
        // yet has_delivered still true → state Delivered (delivered wins).
        let envs = [env("a", 10, 1_000_000)];
        let events = [
            DispositionEvent::delivery_failed("a".into(), 800, "h".into(), "o".into(), 10, "late".into()),
            DispositionEvent::delivered("a".into(), 500, "h".into(), "o".into(), 10),
        ];
        let out = project_summary(&envs, &events, 100);
        assert_eq!(out[0].last_event, Some(EventKind::DeliveryFailed), "latest witnessed_at");
        assert_eq!(out[0].state, SummaryState::Delivered, "but delivered exists → Delivered");
        assert_eq!(out[0].first_delivered_at, Some(500));
    }

    #[test]
    fn duplicate_envelope_id_first_wins() {
        let envs = [env("dup", 1, 1000), env("dup", 999, 1000)];
        let out = project_summary(&envs, &[], 5);
        assert_eq!(out.len(), 1, "duplicate envelope id deduped");
        assert_eq!(out[0].authored_at, 1, "first envelope wins");
    }

    #[test]
    fn attempts_counts_only_attempted_events() {
        let envs = [env("a", 10, 1_000_000)];
        let events = [
            DispositionEvent::accepted("a".into(), 1, "h".into(), "o".into(), 10),
            DispositionEvent::attempted("a".into(), 2, "h".into(), "o".into(), 10),
            DispositionEvent::queued("a".into(), 3, "h".into(), "o".into(), 10),
            DispositionEvent::attempted("a".into(), 4, "h".into(), "o".into(), 10),
            DispositionEvent::delivery_failed("a".into(), 5, "h".into(), "o".into(), 10, "wake".into()),
        ];
        let out = project_summary(&envs, &events, 100);
        assert_eq!(out[0].attempts, 2, "two attempted events");
        assert_eq!(out[0].last_attempt_at, Some(4));
        assert_eq!(out[0].first_delivered_at, None);
        assert_eq!(out[0].state, SummaryState::Failed, "last is delivery-failed, no delivery");
    }

    #[test]
    fn project_one_filters_to_id() {
        let envs = [env("a", 1, 1000), env("b", 2, 1000)];
        let got = project_one(&envs, &[], 5, "b").unwrap();
        assert_eq!(got.correlation_id, "b");
        assert!(project_one(&envs, &[], 5, "missing").is_none());
    }
}
