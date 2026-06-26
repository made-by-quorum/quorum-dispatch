//! `provider/acp/ladder.rs` — the §4 capability-detection / fallback ladder for the
//! CC slice (A2 §4 + §5, the `inject` single-call-site Mode-A switch).
//!
//! The 3-tier ladder is **ACP > native > PTY floor**. In the CC-only scope the live
//! tiers are **ACP (Tier 1, Mode-A)** and the **PTY/Mode-B floor (Tier 3)** — the
//! `native` middle tier is codex/pi/opencode and is DEFERRED. The tier is a DERIVED
//! value recomputed per verb from `(provider, transport-field, endpoint-alive)`
//! (A2 §4, the resolution of blocker F1.1: there was no `tier` storage, so tier is
//! derived, with ONE persisted bit for degradation). The ONE persisted bit is
//! [`crate::registry::RegistryEntry::transport`]: absent = the derived structured
//! default; `"pty"` = a one-way latch recording that the row was DEGRADED to the
//! floor (the wedge-revival rule and the "no auto-PTY after a structured send" rule
//! read it).
//!
//! This module is the DECISION authority (pure, fully unit-tested against every A2
//! §3 LADDER negative control). The actual PTY/Mode-B re-delivery reuses the EXISTING
//! claude floor UNCHANGED (no-regression) — the verb consults [`derive_tier`] to pick
//! the lane and [`on_inject_error`] to react to a send-time transport loss.

/// The persisted degradation-latch value written to
/// [`crate::registry::RegistryEntry::transport`] when a row drops to the floor.
pub const PTY_TRANSPORT: &str = "pty";

/// The delivery lane for a verb against an ACP-capable session (A2 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Tier 1 — the structured ACP transport (Mode-A): `inject` enqueues →
    /// `session/prompt`.
    Acp,
    /// Tier 3 — the PTY / Mode-B floor: the universal fallback (the EXISTING claude
    /// drive path, unchanged). Reached when ACP is unavailable or the row is latched.
    PtyFloor,
}

/// Derive the tier for a verb (A2 §4 — recomputed each verb, cheap). The persisted
/// `transport` latch wins (one-way): once `"pty"`, the row stays on the floor for its
/// life (no silent re-upgrade). Otherwise an `acp/*` provider with a LIVE endpoint
/// gets the ACP tier; anything else (a dead ACP endpoint — the "ACP-claimed-but-dead"
/// red-team false-positive — or a non-acp provider) falls to the PTY floor.
pub fn derive_tier(provider_id: &str, transport_field: Option<&str>, endpoint_alive: bool) -> Tier {
    // The one-way degradation latch reads first: a row degraded to PTY stays PTY.
    if transport_field == Some(PTY_TRANSPORT) {
        return Tier::PtyFloor;
    }
    // Only `acp/*` providers have an ACP tier (CC-only scope: codex/pi/opencode
    // native tiers are deferred). A dead endpoint → the floor (never hang on a
    // claimed-but-dead structured transport — the negative control).
    if provider_id.starts_with("acp/") && endpoint_alive {
        return Tier::Acp;
    }
    Tier::PtyFloor
}

/// What the verb must do when an `inject` could not be delivered over the structured
/// (ACP) tier (A2 §3 LADDER + §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LadderOutcome {
    /// ACP was unavailable and NO structured send was issued for this turn (the
    /// `session/prompt` never reached the wire) → it is SAFE to drop to the PTY/Mode-B
    /// floor and DELIVER this turn (the "must fall to PTY, not fail/hang" negative
    /// control). The verb latches `transport=pty` ([`degrade_to_pty`]) + logs the drop.
    DropToPtyFloor,
    /// A structured send WAS already issued for this turn (or a prior turn on this
    /// session) → auto-PTY would risk DUPLICATE delivery, so the floor is
    /// HUMAN-RECOVERY-ONLY here. The verb MUST refuse an automatic PTY retry (A2 §3
    /// "Structured-delivery-attempted → no silent PTY"; the wedge-revival constraint).
    HumanRecoveryOnly,
    /// Not a transport-availability failure (SC-1a bounded-overflow backpressure /
    /// a structured precondition) → the ladder does NOT degrade the tier; surface the
    /// error as-is (backpressure ≠ a tier-down; SC-1a ≠ SC-3).
    Surface,
}

/// Decide the ladder reaction to an [`crate::provider::InjectError`] from a structured
/// (ACP-tier) `inject`. `structured_send_issued` is the turn-record bit A2 §3 names:
/// `true` once a `session/prompt` has been ISSUED for this turn OR a prior structured
/// turn landed on this session (so an auto-PTY now risks duplicate delivery).
pub fn on_inject_error(
    err: &crate::provider::InjectError,
    structured_send_issued: bool,
) -> LadderOutcome {
    use crate::provider::InjectError;
    match err {
        // No transport at all (ACP host absent / connection closed): the structured
        // send did NOT reach the wire for THIS turn. Safe to floor it — UNLESS a
        // structured turn was already issued on this session (no auto-second-turn PTY).
        InjectError::NoTransport(_) => {
            if structured_send_issued {
                LadderOutcome::HumanRecoveryOnly
            } else {
                LadderOutcome::DropToPtyFloor
            }
        }
        // SC-1a backpressure (`Precondition("queue full")`) and any other structured
        // precondition are NOT transport losses — surface them (do not tier-down).
        InjectError::Precondition(_) | InjectError::RelayFailed(_) => LadderOutcome::Surface,
    }
}

/// Apply the one-way PTY degradation latch to a row's `transport` field (idempotent):
/// records that the row dropped to the floor so the derived tier stays `PtyFloor` for
/// the row's life (A2 §4 — no silent re-upgrade).
pub fn degrade_to_pty(transport: &mut Option<String>) {
    *transport = Some(PTY_TRANSPORT.to_string());
}

/// The operator-visible drop log line (A1 §4.3 Trigger 2: a degradation is NEVER
/// silently swallowed — it is observable). The verb writes this to stderr when it
/// degrades a row to the floor.
pub fn drop_log_line(session: &str, reason: &str) -> String {
    format!("acp: session \"{session}\" dropped to the PTY/Mode-B floor (transport=pty): {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::InjectError;

    // A2 §4: a structured acp row with a LIVE endpoint derives Tier 1 (ACP). The
    // tier-2-preferred invariant — a mutation forcing PtyFloor here would red.
    #[test]
    fn healthy_acp_row_derives_acp_tier() {
        assert_eq!(derive_tier("acp/claude-code", None, true), Tier::Acp);
    }

    // The red-team false-positive control (A2 §3): ACP-CLAIMED-BUT-DEAD (endpoint not
    // alive) must fall to the floor, NEVER hang on the dead structured transport.
    #[test]
    fn acp_claimed_but_dead_endpoint_falls_to_floor() {
        assert_eq!(derive_tier("acp/claude-code", None, false), Tier::PtyFloor);
    }

    // The one-way latch (A2 §4): a row degraded to `pty` derives the floor even if the
    // endpoint would now probe alive (no silent re-upgrade).
    #[test]
    fn pty_latched_row_stays_on_floor_even_if_endpoint_alive() {
        assert_eq!(
            derive_tier("acp/claude-code", Some("pty"), true),
            Tier::PtyFloor
        );
    }

    // A non-acp provider has no ACP tier in this scope → the floor.
    #[test]
    fn non_acp_provider_derives_floor() {
        assert_eq!(derive_tier("claude-code", None, true), Tier::PtyFloor);
    }

    // The "must fall to PTY, not fail/hang" negative control (charge §3): ACP
    // unavailable BEFORE any structured send → drop to the floor and DELIVER.
    #[test]
    fn no_transport_before_structured_send_drops_to_floor() {
        let err = InjectError::NoTransport("sess-1".to_string());
        assert_eq!(on_inject_error(&err, false), LadderOutcome::DropToPtyFloor);
    }

    // The dup-delivery guard (A2 §3, panel gpt-5.5 2.5/F): once a structured send is
    // issued, auto-PTY is REFUSED — human recovery only.
    #[test]
    fn no_transport_after_structured_send_is_human_recovery_only() {
        let err = InjectError::NoTransport("sess-1".to_string());
        assert_eq!(
            on_inject_error(&err, true),
            LadderOutcome::HumanRecoveryOnly,
            "no auto-PTY after a structured send (duplicate-delivery risk)"
        );
    }

    // SC-1a backpressure (`queue full`) is NOT a transport loss → surface, do not
    // tier-down (backpressure ≠ SC-3 ban, ≠ a degradation).
    #[test]
    fn queue_full_precondition_surfaces_not_tier_down() {
        let err = InjectError::Precondition("queue full".to_string());
        assert_eq!(on_inject_error(&err, false), LadderOutcome::Surface);
    }

    // The latch is one-way + idempotent.
    #[test]
    fn degrade_to_pty_is_one_way_idempotent() {
        let mut t: Option<String> = None;
        degrade_to_pty(&mut t);
        assert_eq!(t.as_deref(), Some("pty"));
        degrade_to_pty(&mut t);
        assert_eq!(t.as_deref(), Some("pty"), "idempotent");
        // And a row latched to pty derives the floor — closing the loop.
        assert_eq!(derive_tier("acp/claude-code", t.as_deref(), true), Tier::PtyFloor);
    }

    // The drop is observable, not silent (A1 §4.3 Trigger 2).
    #[test]
    fn drop_log_line_is_observable() {
        let line = drop_log_line("sess-1", "acp_client absent (InjectError::NoTransport)");
        assert!(line.contains("sess-1"));
        assert!(line.contains("transport=pty"));
        assert!(line.contains("PTY/Mode-B floor"));
    }
}
