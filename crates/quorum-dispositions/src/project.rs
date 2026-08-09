//! The pure projection: envelopes ∪ disposition-events → per-id summary records
//! (format doc §3).
//!
//! Under R8/R8a/R8b + R14, `dispositions.jsonl` is an append-only log of typed
//! EVENTS ([`DispositionEvent`]), never state records. State is a VIEW, folded
//! here: [`project_summary`] emits one [`SummaryRecord`] per `correlation_id`
//! (the `qd dispositions` DEFAULT output), carrying the coarse 4-state
//! [`SummaryState`] UNCHANGED (guard 2) plus `last_event` and the analytics
//! fields. `qd dispositions --events` emits the raw event rows.
//!
//! The fold is what fixes the live bug the old "first terminal wins" collapse
//! caused: a `delivery-failed@t1 → delivered@t3` funnel now summarizes to
//! `delivered` (a delivered event EXISTS, [`crate::has_delivered`]), never
//! `failed` forever.
//!
//! # Source-agnostic fold (R14.2 / R14a pin 2)
//!
//! This leaf takes a FLAT events slice and knows nothing of `source`/host —
//! event rows no longer carry `witness` (normalized away, R14.2). The
//! `last_event` pick is MAX by `created_at`; on a FULL tie the LATER-IN-INPUT
//! row wins (within one file, file order IS recorded order; the ms timestamp is
//! a lossy projection of append order). CROSS-SOURCE determinism — the
//! `(created_at, source)` comparator that made a multi-host union order-invariant
//! — is now the UNION READER's responsibility one layer up (it concatenates
//! per-source rows in sorted-host order before calling this fn), and is tested
//! in the dispatch layer, NOT here. Origin/authored_at come ONLY from the joined
//! envelope now (R14.2), so an orphan-event summary is honestly null across
//! origin/authored_at/expires_at.

use std::collections::HashMap;

use crate::record::{DispositionEvent, Envelope, EventKind, SummaryRecord, SummaryState};

/// Whether a `delivered` event exists for `correlation_id` in `events`. This is
/// the idempotence + delivered-view predicate (R8: "delivered = a delivered
/// event EXISTS, irreversible"). Pure; the store uses it in phase 2.
pub fn has_delivered(events: &[DispositionEvent], correlation_id: &str) -> bool {
    events
        .iter()
        .any(|e| e.kind() == EventKind::Delivered && e.correlation_id() == correlation_id)
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
/// 4. `Pending`   — otherwise (latest is attempted/queued/**refused**, or none).
///    `refused` is PENDING-class: refused = never left ≠ failed (R14.3).
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
struct Fold {
    /// The `last_event` pick: MAX by `created_at`; on a FULL tie (equal
    /// `created_at`) the LATER-IN-INPUT row wins (R14.2 — within one file, file
    /// order IS recorded order; the ms timestamp is a lossy projection of append
    /// order). Cross-source order-invariance is the union reader's job (see the
    /// module doc); this leaf folds a flat, already-ordered slice.
    last_event: EventKind,
    last_event_at: i64,
    attempts: u32,
    last_attempt_at: Option<i64>,
    first_delivered_at: Option<i64>,
    has_delivered: bool,
}

impl Fold {
    fn new(ev: &DispositionEvent) -> Self {
        let mut f = Fold {
            last_event: ev.kind(),
            last_event_at: i64::MIN,
            attempts: 0,
            last_attempt_at: None,
            first_delivered_at: None,
            has_delivered: false,
        };
        f.absorb(ev);
        f
    }

    fn absorb(&mut self, ev: &DispositionEvent) {
        // R14.2 pick: MAX by created_at. Replace on `>=` so a FULL tie (equal
        // created_at) lets the LATER-IN-INPUT row win — within one file, file
        // order IS recorded order (the ms timestamp is a lossy projection of
        // append order). A same-instant retry (delivery-failed@t then
        // attempted@t) folds to last_event=Attempted → pending, not Failed.
        let created_at = ev.created_at();
        if created_at >= self.last_event_at {
            self.last_event = ev.kind();
            self.last_event_at = created_at;
        }
        match ev.kind() {
            EventKind::Attempted => {
                self.attempts += 1;
                self.last_attempt_at =
                    Some(self.last_attempt_at.map_or(created_at, |m| m.max(created_at)));
            }
            EventKind::Delivered => {
                self.has_delivered = true;
                self.first_delivered_at =
                    Some(self.first_delivered_at.map_or(created_at, |m| m.min(created_at)));
            }
            _ => {}
        }
    }
}

/// Project an envelope stream ∪ a disposition-EVENT stream into one
/// [`SummaryRecord`] per `correlation_id` at wall-clock `now_ms` (epoch-ms).
/// Pure and deterministic.
///
/// Per id: `attempts` = count of `attempted`; `last_event` = the event MAX by
/// `created_at` (full tie → later-in-input, R14.2), `None` iff no events (R11.1);
/// `last_attempt_at` = max `created_at` over `attempted` (else None);
/// `first_delivered_at` = min `created_at` over `delivered` (else None).
/// `expires_at`/`authored_at`/`origin` come from the joined [`Envelope`] when in
/// scope, else ALL null (R14.2 honest null — events no longer carry them).
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
        let id = ev.correlation_id();
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
        out.push(summary_for(id, folds.get(id), Some(env), now_ms));
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
    // `last_event`: None exactly when no events exist (R11.1) — a summary never
    // reports a moment nobody recorded (the old fabricated-`accepted` default is
    // overruled).
    let last_event = fold.map(|f| f.last_event);
    let has_delivered = fold.is_some_and(|f| f.has_delivered);
    let attempts = fold.map_or(0, |f| f.attempts);
    let last_attempt_at = fold.and_then(|f| f.last_attempt_at);
    let first_delivered_at = fold.and_then(|f| f.first_delivered_at);

    // R14.2 honest null: origin/authored_at/expires_at come ONLY from the joined
    // envelope. An orphan-event summary (no envelope in scope) is null across all
    // three — events no longer carry origin/authored_at (the copy was
    // denormalization), so there is nothing to fall back to.
    let expires_at = env.map(|e| e.expires_at);
    let authored_at = env.map(|e| e.authored_at);
    let origin = env.map(|e| e.origin.clone());

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
            DispositionEvent::attempted("a".into(), t1),
            DispositionEvent::delivery_failed("a".into(), t1, "wake".into()),
            DispositionEvent::attempted("a".into(), t2),
            DispositionEvent::queued("a".into(), t2),
            DispositionEvent::delivered("a".into(), t3, "d".into()),
        ];
        let envs = [env("a", 10, 100_000)];
        let out = project_summary(&envs, &events, 400);
        assert_eq!(out.len(), 1);
        let s = &out[0];
        assert_eq!(s.state, SummaryState::Delivered, "delivered event exists → Delivered");
        assert_eq!(s.attempts, 2, "two attempted events");
        assert_eq!(s.last_event, Some(EventKind::Delivered), "latest by created_at is delivered@t3");
        assert_eq!(s.first_delivered_at, Some(t3));
        assert_eq!(s.last_attempt_at, Some(t2), "max created_at over attempted");
        assert_eq!(s.expires_at, Some(100_000), "from the envelope");
        assert_eq!(s.authored_at, Some(10), "from the envelope");
        assert_eq!(s.origin, Some("origin-host".to_string()), "from the envelope");
    }

    // ---- R14.2 tie-break: the same-instant scenarios (within one file) ----

    #[test]
    fn same_instant_retry_last_row_wins() {
        // delivery-failed@t then attempted@t: the later-in-input row wins the
        // pick → last_event=Attempted → Pending. (A strict-`>` fold would keep
        // DeliveryFailed → Failed — the ruled bug, now under created_at.)
        let envs = [env("a", 10, 1_000_000)];
        let t = 500;
        let events = [
            DispositionEvent::delivery_failed("a".into(), t, "wake".into()),
            DispositionEvent::attempted("a".into(), t),
        ];
        let out = project_summary(&envs, &events, 600);
        assert_eq!(out[0].last_event, Some(EventKind::Attempted), "file-last row at the tie wins");
        assert_eq!(out[0].state, SummaryState::Pending, "retry in flight, not Failed");
    }

    #[test]
    fn whole_funnel_compressed_into_one_instant_folds_to_delivered() {
        // The §6 scenario with ALL five rows at the same created_at: file order
        // is recorded order → the file-last row (delivered) is the pick, and
        // state is Delivered.
        let envs = [env("a", 10, 1_000_000)];
        let t = 500;
        let events = [
            DispositionEvent::attempted("a".into(), t),
            DispositionEvent::delivery_failed("a".into(), t, "wake".into()),
            DispositionEvent::attempted("a".into(), t),
            DispositionEvent::queued("a".into(), t),
            DispositionEvent::delivered("a".into(), t, "d".into()),
        ];
        let out = project_summary(&envs, &events, 600);
        assert_eq!(out[0].state, SummaryState::Delivered);
        assert_eq!(out[0].last_event, Some(EventKind::Delivered), "the file-last row");
        assert_eq!(out[0].attempts, 2);
        assert_eq!(out[0].first_delivered_at, Some(t));
        assert_eq!(out[0].last_attempt_at, Some(t));
    }

    // ---- derive_state matrix (RATIFIED R10; refused = pending-class R14.3) ----

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
    fn derive_state_pending_when_last_is_attempt_or_queued_or_refused_or_none() {
        assert_eq!(
            derive_state(false, Some(1_000_000), Some(EventKind::Attempted), 5),
            SummaryState::Pending
        );
        assert_eq!(
            derive_state(false, Some(1_000_000), Some(EventKind::Queued), 5),
            SummaryState::Pending
        );
        // refused is PENDING-class (R14.3) — refused = never left ≠ failed.
        assert_eq!(
            derive_state(false, Some(1_000_000), Some(EventKind::Refused), 5),
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
            DispositionEvent::attempted("a".into(), 1),
            DispositionEvent::delivered("a".into(), 2, "d".into()),
            DispositionEvent::delivery_failed("b".into(), 3, "wake".into()),
        ];
        assert!(has_delivered(&events, "a"));
        assert!(!has_delivered(&events, "b"), "b only failed");
        assert!(!has_delivered(&events, "missing"));
    }

    // ---- projection basics ----

    #[test]
    fn envelope_only_no_events_null_last_event_and_is_pending_pre_expiry() {
        let envs = [env("a", 10, 1000)];
        let out = project_summary(&envs, &[], 500);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, SummaryState::Pending);
        // no events → last_event null (no fabricated accepted).
        assert_eq!(out[0].last_event, None, "no fabricated accepted");
        assert_eq!(out[0].attempts, 0);
        assert_eq!(out[0].last_attempt_at, None);
        assert_eq!(out[0].first_delivered_at, None);
        // origin/authored_at/expires_at from the envelope (in scope).
        assert_eq!(out[0].origin, Some("origin-host".to_string()));
        assert_eq!(out[0].authored_at, Some(10));
        assert_eq!(out[0].expires_at, Some(1000));
    }

    #[test]
    fn envelope_only_post_expiry_is_expired() {
        let envs = [env("a", 10, 1000)];
        let out = project_summary(&envs, &[], 1000);
        assert_eq!(out[0].state, SummaryState::Expired);
        assert_eq!(out[0].last_event, None);
    }

    #[test]
    fn failed_only_pre_expiry_is_failed() {
        let envs = [env("a", 10, 1_000_000)];
        let events = [DispositionEvent::delivery_failed("a".into(), 60, "wake".into())];
        let out = project_summary(&envs, &events, 100);
        assert_eq!(out[0].state, SummaryState::Failed);
        assert_eq!(out[0].last_event, Some(EventKind::DeliveryFailed));
    }

    #[test]
    fn refused_only_summary_is_pending() {
        // A refused-only summary → state pending (refused is pending-class,
        // R14.3), last_event = refused.
        let envs = [env("a", 10, 1_000_000)];
        let events = [DispositionEvent::refused("a".into(), 60, "no-live-receive-path".into())];
        let out = project_summary(&envs, &events, 100);
        assert_eq!(out[0].state, SummaryState::Pending, "refused ≠ failed, pending-class");
        assert_eq!(out[0].last_event, Some(EventKind::Refused));
        assert_eq!(out[0].attempts, 0, "refused is not an attempt");
    }

    #[test]
    fn orphan_event_summary_no_envelope_is_triple_null() {
        // A delivered event whose envelope is not in scope → summary from the
        // event alone: origin, authored_at, AND expires_at ALL null (R14.2 honest
        // null); state derivable (delivered), but never Expired.
        let events = [DispositionEvent::delivered("orphan".into(), 700, "d".into())];
        let out = project_summary(&[], &events, i64::MAX);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].correlation_id, "orphan");
        assert_eq!(out[0].state, SummaryState::Delivered);
        assert_eq!(out[0].expires_at, None, "no envelope → no expires_at");
        assert_eq!(out[0].authored_at, None, "no envelope → authored_at null (R14.2)");
        assert_eq!(out[0].origin, None, "no envelope → origin null (R14.2)");
        assert_eq!(out[0].first_delivered_at, Some(700));
    }

    #[test]
    fn orphan_failed_never_expired_even_far_future() {
        let events = [DispositionEvent::delivery_failed("o".into(), 5, "wake".into())];
        let out = project_summary(&[], &events, i64::MAX);
        assert_eq!(out[0].state, SummaryState::Failed, "orphan → never Expired");
        assert_eq!(out[0].origin, None, "orphan → origin null");
        assert_eq!(out[0].authored_at, None, "orphan → authored_at null");
    }

    #[test]
    fn output_order_envelopes_then_orphan_events() {
        let envs = [env("e1", 1, 1000), env("e2", 2, 1000)];
        let events = [
            DispositionEvent::delivered("e1".into(), 10, "d".into()),
            DispositionEvent::delivery_failed("orphanA".into(), 20, "x".into()),
            DispositionEvent::delivered("orphanB".into(), 30, "d".into()),
        ];
        let out = project_summary(&envs, &events, 5);
        let ids: Vec<&str> = out.iter().map(|r| r.correlation_id.as_str()).collect();
        assert_eq!(ids, vec!["e1", "e2", "orphanA", "orphanB"]);
        assert_eq!(out[1].state, SummaryState::Pending, "e2 no events → pending");
    }

    #[test]
    fn last_event_is_latest_by_created_at_not_input_order() {
        // delivery-failed@t=800 arrives in input BEFORE delivered@t=500, but
        // last_event picks the latest created_at (800) → DeliveryFailed;
        // yet has_delivered still true → state Delivered (delivered wins).
        let envs = [env("a", 10, 1_000_000)];
        let events = [
            DispositionEvent::delivery_failed("a".into(), 800, "late".into()),
            DispositionEvent::delivered("a".into(), 500, "d".into()),
        ];
        let out = project_summary(&envs, &events, 100);
        assert_eq!(out[0].last_event, Some(EventKind::DeliveryFailed), "latest created_at");
        assert_eq!(out[0].state, SummaryState::Delivered, "but delivered exists → Delivered");
        assert_eq!(out[0].first_delivered_at, Some(500));
    }

    #[test]
    fn duplicate_envelope_id_first_wins() {
        let envs = [env("dup", 1, 1000), env("dup", 999, 1000)];
        let out = project_summary(&envs, &[], 5);
        assert_eq!(out.len(), 1, "duplicate envelope id deduped");
        assert_eq!(out[0].authored_at, Some(1), "first envelope wins");
    }

    #[test]
    fn attempts_counts_only_attempted_events() {
        let envs = [env("a", 10, 1_000_000)];
        let events = [
            DispositionEvent::refused("a".into(), 1, "ambiguous".into()),
            DispositionEvent::attempted("a".into(), 2),
            DispositionEvent::queued("a".into(), 3),
            DispositionEvent::attempted("a".into(), 4),
            DispositionEvent::delivery_failed("a".into(), 5, "wake".into()),
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
