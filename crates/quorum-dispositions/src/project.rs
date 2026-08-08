//! The pure left-join projection: envelopes ⟕ dispositions → emitted records
//! (format doc §3).
//!
//! This is the exact join `qd dispositions` publishes and frame projects over,
//! computed here as a pure function so it is golden-testable and reusable on
//! either side. It is a LEFT join keyed on `correlation_id`:
//!
//! - envelope has a `delivered`/`failed` terminal → that terminal (witnessed_at
//!   from the disposition, authored_at from the envelope, authority + reason
//!   from the disposition);
//! - envelope, no terminal, `now < expires_at`     → `pending` (witnessed_at null);
//! - envelope, no terminal, `now >= expires_at`    → `expired` (witnessed_at null);
//! - a terminal whose envelope is NOT in scope     → the terminal emitted from
//!   the disposition ALONE (self-contained: it carries authored_at + authority);
//!   `pending`/`expired` are never emitted here (only the envelope knows
//!   `expires_at`).
//!
//! Output order is deterministic (for golden tests): envelopes in input order,
//! then orphan dispositions in input order. Frame re-sorts in SQL; callers here
//! rely only on determinism.

use std::collections::HashMap;

use crate::record::{Disposition, EmittedRecord, Envelope, RecordState, StoredState};

/// Select, per `correlation_id`, the ONE terminal disposition that wins: the
/// earliest `witnessed_at`, ties broken stably by input order (first wins).
/// Returns a map id → index-into-`dispositions` of the winner.
fn winning_terminal_index(dispositions: &[Disposition]) -> HashMap<&str, usize> {
    let mut winner: HashMap<&str, usize> = HashMap::new();
    for (i, d) in dispositions.iter().enumerate() {
        winner
            .entry(d.correlation_id.as_str())
            .and_modify(|cur| {
                // Keep the earliest witnessed_at. On a tie, keep the incumbent
                // (earlier input index) — stable "first wins".
                if dispositions[i].witnessed_at < dispositions[*cur].witnessed_at {
                    *cur = i;
                }
            })
            .or_insert(i);
    }
    winner
}

fn stored_to_record_state(s: StoredState) -> RecordState {
    match s {
        StoredState::Delivered => RecordState::Delivered,
        StoredState::Failed => RecordState::Failed,
    }
}

/// The terminal emitted record for a disposition joined to an envelope's
/// `authored_at` (self-contained fields otherwise come from the disposition).
fn terminal_from(disp: &Disposition, authored_at: i64) -> EmittedRecord {
    EmittedRecord {
        v: 1,
        correlation_id: disp.correlation_id.clone(),
        state: stored_to_record_state(disp.state),
        authored_at,
        witnessed_at: Some(disp.witnessed_at),
        authority: disp.authority.clone(),
        reason: disp.reason.clone(),
    }
}

/// Project an envelope stream ⟕ a disposition stream into the emitted 4-state
/// record stream at wall-clock `now_ms` (epoch-ms). Pure and deterministic.
///
/// See the module doc for the join rules and the output-order guarantee.
pub fn project(
    envelopes: &[Envelope],
    dispositions: &[Disposition],
    now_ms: i64,
) -> Vec<EmittedRecord> {
    let winner = winning_terminal_index(dispositions);

    let mut out = Vec::new();
    // Track which correlation_ids the envelope pass consumed, so a matched
    // disposition is NOT re-emitted as an orphan below.
    let mut seen_env: HashMap<&str, ()> = HashMap::new();

    // Pass 1 — envelopes in input order, dedup by correlation_id (first wins).
    for env in envelopes {
        let id = env.correlation_id.as_str();
        if seen_env.insert(id, ()).is_some() {
            continue; // duplicate envelope id already emitted
        }
        let rec = match winner.get(id) {
            Some(&di) => terminal_from(&dispositions[di], env.authored_at),
            None => {
                let state = if now_ms < env.expires_at {
                    RecordState::Pending
                } else {
                    RecordState::Expired
                };
                EmittedRecord {
                    v: 1,
                    correlation_id: env.correlation_id.clone(),
                    state,
                    authored_at: env.authored_at,
                    witnessed_at: None,
                    authority: env.authority.clone(),
                    reason: None,
                }
            }
        };
        out.push(rec);
    }

    // Pass 2 — orphan dispositions (correlation_id NOT among envelopes) in input
    // order, deduped by correlation_id via the winning-terminal representative.
    // Iterating `dispositions` in order and emitting each id at its FIRST
    // appearance gives "input order"; the emitted row uses the winner index.
    let mut emitted_orphan: HashMap<&str, ()> = HashMap::new();
    for d in dispositions {
        let id = d.correlation_id.as_str();
        if seen_env.contains_key(id) {
            continue; // has an envelope → already handled in pass 1
        }
        if emitted_orphan.insert(id, ()).is_some() {
            continue; // this orphan id already emitted
        }
        let di = winner[id]; // present by construction
        let disp = &dispositions[di];
        // Self-contained: authored_at comes from the disposition itself.
        out.push(terminal_from(disp, disp.authored_at));
    }

    out
}

/// Point-query convenience: project, then filter to a single `correlation_id`.
///
/// A `correlation_id` yields AT MOST one emitted record (envelopes dedup first,
/// orphans dedup by id), so this returns `Option`. Callers wanting the full set
/// filter [`project`]'s `Vec` directly.
pub fn project_one(
    envelopes: &[Envelope],
    dispositions: &[Disposition],
    now_ms: i64,
    correlation_id: &str,
) -> Option<EmittedRecord> {
    project(envelopes, dispositions, now_ms)
        .into_iter()
        .find(|r| r.correlation_id == correlation_id)
}

#[cfg(test)]
mod tests {
    // `Envelope` comes in via `super::*` (module-level import); no explicit
    // re-import (that would be a redundant-import warning).
    use super::*;

    fn env(id: &str, authored: i64, expires: i64) -> Envelope {
        Envelope {
            v: 1,
            correlation_id: id.to_string(),
            authored_at: authored,
            expires_at: expires,
            target: "t".to_string(),
            authority: "origin-host".to_string(),
            body: "b".to_string(),
        }
    }

    fn disp(id: &str, state: StoredState, witnessed: i64, reason: Option<&str>) -> Disposition {
        Disposition {
            v: 1,
            correlation_id: id.to_string(),
            state,
            authored_at: 100,
            witnessed_at: witnessed,
            authority: "witness-host".to_string(),
            reason: reason.map(str::to_string),
        }
    }

    #[test]
    fn envelope_plus_delivered() {
        let envs = [env("a", 10, 1000)];
        let disps = [disp("a", StoredState::Delivered, 500, None)];
        let out = project(&envs, &disps, 42);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, RecordState::Delivered);
        assert_eq!(out[0].witnessed_at, Some(500));
        assert_eq!(out[0].authored_at, 10, "authored_at from the envelope");
        assert_eq!(out[0].authority, "witness-host", "authority from the disposition");
        assert_eq!(out[0].reason, None);
    }

    #[test]
    fn envelope_plus_failed_with_reason() {
        let envs = [env("a", 10, 1000)];
        let disps = [disp("a", StoredState::Failed, 600, Some("wake"))];
        let out = project(&envs, &disps, 42);
        assert_eq!(out[0].state, RecordState::Failed);
        assert_eq!(out[0].witnessed_at, Some(600));
        assert_eq!(out[0].reason.as_deref(), Some("wake"));
    }

    #[test]
    fn envelope_no_disp_pre_expiry_is_pending() {
        let envs = [env("a", 10, 1000)];
        let out = project(&envs, &[], 500); // now < expires
        assert_eq!(out[0].state, RecordState::Pending);
        assert_eq!(out[0].witnessed_at, None, "pending → witnessed_at null");
        assert_eq!(out[0].authority, "origin-host", "origin authority for pending");
        assert_eq!(out[0].reason, None);
    }

    #[test]
    fn envelope_no_disp_post_expiry_is_expired() {
        let envs = [env("a", 10, 1000)];
        let out = project(&envs, &[], 1000); // now == expires → expired (>=)
        assert_eq!(out[0].state, RecordState::Expired);
        assert_eq!(out[0].witnessed_at, None, "expired → witnessed_at null");
        assert_eq!(out[0].authority, "origin-host");
    }

    #[test]
    fn expiry_boundary_is_inclusive() {
        // now == expires_at → expired (rule is `now >= expires_at`).
        let envs = [env("a", 10, 1000)];
        assert_eq!(project(&envs, &[], 999)[0].state, RecordState::Pending);
        assert_eq!(project(&envs, &[], 1000)[0].state, RecordState::Expired);
        assert_eq!(project(&envs, &[], 1001)[0].state, RecordState::Expired);
    }

    #[test]
    fn orphan_disposition_emits_terminal_self_contained() {
        // Disposition with NO envelope in scope → terminal from the disposition alone.
        let disps = [disp("orphan", StoredState::Delivered, 700, None)];
        let out = project(&[], &disps, 42);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, RecordState::Delivered);
        assert_eq!(out[0].correlation_id, "orphan");
        assert_eq!(out[0].witnessed_at, Some(700));
        assert_eq!(out[0].authored_at, 100, "authored_at from the disposition itself");
        assert_eq!(out[0].authority, "witness-host");
    }

    #[test]
    fn output_order_envelopes_then_orphans() {
        let envs = [env("e1", 1, 1000), env("e2", 2, 1000)];
        let disps = [
            disp("e1", StoredState::Delivered, 10, None),
            disp("orphanA", StoredState::Failed, 20, Some("x")),
            disp("orphanB", StoredState::Delivered, 30, None),
        ];
        let out = project(&envs, &disps, 5);
        let ids: Vec<&str> = out.iter().map(|r| r.correlation_id.as_str()).collect();
        // envelopes (input order) first, then orphans (input order).
        assert_eq!(ids, vec!["e1", "e2", "orphanA", "orphanB"]);
        assert_eq!(out[1].state, RecordState::Pending, "e2 has no disp → pending");
    }

    #[test]
    fn duplicate_envelope_id_first_wins() {
        let envs = [env("dup", 1, 1000), env("dup", 999, 1000)];
        let out = project(&envs, &[], 5);
        assert_eq!(out.len(), 1, "duplicate envelope id deduped");
        assert_eq!(out[0].authored_at, 1, "first envelope wins");
    }

    #[test]
    fn duplicate_terminal_earliest_witnessed_wins() {
        let envs = [env("a", 10, 1000)];
        // Two terminals for the same id; the earlier witnessed_at (500) wins,
        // even though it appears SECOND in input order.
        let disps = [
            disp("a", StoredState::Failed, 800, Some("late")),
            disp("a", StoredState::Delivered, 500, None),
        ];
        let out = project(&envs, &disps, 42);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, RecordState::Delivered, "earliest witnessed wins");
        assert_eq!(out[0].witnessed_at, Some(500));
    }

    #[test]
    fn duplicate_terminal_tie_breaks_by_input_order() {
        let envs = [env("a", 10, 1000)];
        // Equal witnessed_at → the FIRST in input order wins (stable).
        let disps = [
            disp("a", StoredState::Failed, 500, Some("first")),
            disp("a", StoredState::Delivered, 500, None),
        ];
        let out = project(&envs, &disps, 42);
        assert_eq!(out[0].state, RecordState::Failed, "tie → first input wins");
        assert_eq!(out[0].reason.as_deref(), Some("first"));
    }

    #[test]
    fn orphan_duplicate_deduped_by_winner() {
        // Two orphan terminals for the same id: emit once, using the winner.
        let disps = [
            disp("o", StoredState::Failed, 900, Some("late")),
            disp("o", StoredState::Delivered, 400, None),
        ];
        let out = project(&[], &disps, 42);
        assert_eq!(out.len(), 1, "orphan id emitted once");
        assert_eq!(out[0].witnessed_at, Some(400), "winner = earliest witnessed");
        assert_eq!(out[0].state, RecordState::Delivered);
    }

    #[test]
    fn project_one_filters_to_id() {
        let envs = [env("a", 1, 1000), env("b", 2, 1000)];
        let got = project_one(&envs, &[], 5, "b").unwrap();
        assert_eq!(got.correlation_id, "b");
        assert!(project_one(&envs, &[], 5, "missing").is_none());
    }
}
