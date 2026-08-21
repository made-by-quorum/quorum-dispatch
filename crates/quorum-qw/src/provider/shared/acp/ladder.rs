//! `provider/acp/ladder.rs` — tier derivation + the transport-loss disposition
//! for the `acp/*` verbs.
//!
//! **The shipped contract (Child D, opencode D1 — clerk-4's Arm-B ratification,
//! bond note 01KX01BY7G), exactly:**
//!
//! `acp/claude-code` is a **NAMED DIVERGENCE** from D1's auto-deliver
//! graceful-degrade goal, joining `codex` and `acp/opencode`:
//!
//! 1. **Transport loss REFUSES and surfaces** — pre-send, post-send, or
//!    mid-wait, `qd send`/`qd wait` refuse with the human-recovery surface
//!    ("… not reachable (try qd resume …)"), exit 1. There is NO auto-deliver
//!    fallback: the Child-B PTY-floor drive (latch + companion pane +
//!    floor delivery) was REMOVED, not gated — a red-team hunting a reachable
//!    auto-deliver path should find the machinery absent, not disabled
//!    (`bin/qd/verbs/send_relay.rs::run_acp_send`, `wait.rs::run_acp_wait`).
//!    Why: the floor drive's only revival seam is `claude --resume
//!    <session_id>`, which is structurally impossible for the only
//!    degrade-eligible population (a zero-structured-turn session has no
//!    conversation to resume — Child C, live-proven), and the Arm-A drive
//!    redesign was ruled a speculative redesign and DECLINED.
//! 2. **Identity is PRESERVED across the loss.** Before refusing, the verb
//!    records the session's identity (session_id, name, pid, cwd, provider,
//!    endpoint, transcript path, loss reason, timestamps) in the qd-owned
//!    tombstone store — `tombstone` (dispatch-side),
//!    `<QD_HOME || ~/.quorum/dispatch>/state/tombstones/<session_id>.json` —
//!    because the session's own registry row (`~/.claude/sessions/<pid>.json`)
//!    is claude-CLI-owned and janitor-reaped ~1s after daemon death (Child C,
//!    live-proven): anything persisted there is structurally unpersistable for
//!    a dead-pid session. Scope: `acp/claude-code` only; `codex`/`acp/opencode`
//!    refusals stay byte-identical and write nothing
//!    (`bin/qd/verbs/acp_loss.rs`).
//! 3. **A connect failure re-probes, then splits wedge from loss** (red-team
//!    round 1, TOCTOU finding). Reaching the connect step required `derive_tier`
//!    to read `endpoint_alive == true` (pid alive, identity-verified cmdline,
//!    endpoint present) — but that reading is a point-in-time probe taken
//!    BEFORE connect, and a daemon can die across that boundary. So on connect
//!    failure the verb re-probes liveness+identity NOW and classifies
//!    ([`classify_connect_failure`]):
//!    - **still alive + identity-verified** ⇒ a genuine WEDGE: refuse with NO
//!      tombstone. The registry row is live-pid, which the claude CLI's
//!      dead-pid janitor never reaps, so no identity is at risk; `qd resume`
//!      (kill + restart) is the recovery act.
//!    - **now dead (or identity no longer verified)** ⇒ the daemon died across
//!      the boundary: a transport LOSS — identity preserved + refuse, exactly
//!      like the entry lane.
//!    **The residual (accepted, stated):** a daemon observed as a genuine wedge
//!    that dies LATER, with no further qd interaction ever, loses its registry
//!    row to the janitor with no tombstone written. This is acceptable because
//!    (a) ANY subsequent qd send/wait on the session after the death hits the
//!    entry lane (dead pid / empty cmdline ⇒ `Tier::Unavailable`) and writes the
//!    tombstone then — the only uncovered case is a session nobody ever asks qd
//!    about again; and (b) even in that case the recovery-critical identity
//!    survives in qd-owned stores independent of this module: the stable-id ↔
//!    session_id ↔ name mapping in `<state_dir>/ids.jsonl` (`crate::idstore` —
//!    append-only, minted at start/backfill, never reaped) and the CC transcript
//!    under `~/.claude/projects/…/<session_id>.jsonl` (the janitor reaps only
//!    `sessions/<pid>.json`). The tombstone adds cwd/endpoint/loss-reason
//!    convenience on top of that durable pair; it is not the sole carrier.
//!
//! **Relation to pi's floor** (`provider/pi/floor.rs`): pi is D1's auto-deliver
//! realization — its latch-free, auto-returning floor regime is ratified live
//! behavior, deliberately SEPARATE and untouched by this module (disjoint
//! `provider_id`s, disjoint registry rows). Changing pi's floor is a
//! preregistered escalation, never build scope here.
//!
//! **What remains of the Child-B ladder:** [`derive_tier`] — the per-verb lane
//! pick (structured ACP vs unavailable), recomputed from `(provider,
//! transport-field, endpoint-alive)` with no persisted tier. The
//! [`PTY_TRANSPORT`] latch value is retained READ-side only: nothing writes
//! `transport="pty"` anymore, but a row bearing one (written by a never-deployed
//! Child-B dev binary) derives [`Tier::Unavailable`] and refuses — the
//! conservative interpretation of a row that once dropped to a floor that no
//! longer exists. `QD_ACP_PTY_FLOOR_DISABLE`, which gated only the retired
//! drive, is now a NO-OP (refusal is unconditional).
//!
//! **Exactly-once history, kept:** the wire dispatch for `session/prompt`
//! (`provider/acp/wire.rs`) still durably marks
//! [`crate::registry::RegistryEntry::structured_send_issued`] the moment the
//! request's bytes are confirmed on the wire (via `ProviderFx::acp_pre_dispatch`,
//! installed unconditionally — F3). The DISPOSITION no longer branches on it
//! (pre-send and post-send loss both refuse), but the marker stays truth about
//! the session's wire history and is consumed elsewhere (e.g. the resume seam,
//! `bin/qd/verbs/lifecycle.rs`).

/// The historical degradation-latch value on
/// [`crate::registry::RegistryEntry::transport`]. WRITE-side retired (Child D):
/// nothing latches anymore; retained so [`derive_tier`] interprets a
/// previously-latched row conservatively (unavailable ⇒ refuse), never as a
/// healthy structured tier.
pub const PTY_TRANSPORT: &str = "pty";

/// The delivery lane for a verb against an ACP-capable session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The structured ACP transport (Mode-A): `inject` enqueues →
    /// `session/prompt`.
    Acp,
    /// The structured transport is not available for this row — dead endpoint,
    /// a historical `"pty"` latch, or a non-acp provider. Child D disposition:
    /// the verb REFUSES and surfaces (identity preserved for `acp/claude-code`
    /// via `tombstone` (dispatch-side)); there is no auto-deliver tier.
    Unavailable,
}

/// What a connect failure on a previously-live-probed endpoint means, decided
/// from a FRESH liveness+identity re-probe taken AFTER the failure (the module
/// contract's clause 3; closes the red-team round-1 TOCTOU: the pre-connect
/// probe cannot "confirm" liveness across the connect boundary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectFailure {
    /// The daemon is alive and identity-verified RIGHT NOW: a genuine wedge.
    /// Refuse without a tombstone — the live-pid registry row is not
    /// janitor-reapable, so no identity is at risk.
    WedgeStillAlive,
    /// The daemon is now dead (or its pid no longer carries our cmdline): it
    /// died across the probe→connect boundary. A transport LOSS — the verb
    /// preserves identity before refusing, exactly like the entry lane.
    TransportLost,
}

/// Classify a connect failure from the post-failure re-probe readings:
/// `pid_alive_now` = `is_pid_alive(pid)` taken after the failure;
/// `cmdline_is_ours_now` = the identity fence (`cmdline_is_our_acp_daemon`)
/// re-read at the same moment. Both must hold for the wedge classification —
/// a dead pid OR a pid that no longer carries our daemon's cmdline (exit +
/// pid reuse inside the window) is a loss.
pub fn classify_connect_failure(pid_alive_now: bool, cmdline_is_ours_now: bool) -> ConnectFailure {
    if pid_alive_now && cmdline_is_ours_now {
        ConnectFailure::WedgeStillAlive
    } else {
        ConnectFailure::TransportLost
    }
}

/// Derive the tier for a verb (recomputed each verb, cheap — no persisted
/// tier). The historical `transport="pty"` latch reads first: such a row is
/// never treated as structured (see [`PTY_TRANSPORT`]). Otherwise an `acp/*`
/// provider with a LIVE endpoint gets the ACP tier; anything else (a dead ACP
/// endpoint — the "ACP-claimed-but-dead" false-positive — or a non-acp
/// provider) is unavailable, which the verbs surface as a refusal.
pub fn derive_tier(provider_id: &str, transport_field: Option<&str>, endpoint_alive: bool) -> Tier {
    // A historically-latched row is never structured (conservative read).
    if transport_field == Some(PTY_TRANSPORT) {
        return Tier::Unavailable;
    }
    // Only `acp/*` providers have an ACP tier. A dead endpoint → unavailable
    // (never hang on a claimed-but-dead structured transport — the negative
    // control).
    if provider_id.starts_with("acp/") && endpoint_alive {
        return Tier::Acp;
    }
    Tier::Unavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    // A structured acp row with a LIVE endpoint derives the ACP tier — the
    // structured-preferred invariant; a mutation forcing Unavailable here reds.
    #[test]
    fn healthy_acp_row_derives_acp_tier() {
        assert_eq!(derive_tier("acp/claude-code", None, true), Tier::Acp);
    }

    // The false-positive control: ACP-CLAIMED-BUT-DEAD (endpoint not alive)
    // must derive Unavailable — the verbs refuse; nothing may hang on (or
    // auto-deliver over) a dead structured transport.
    #[test]
    fn acp_claimed_but_dead_endpoint_is_unavailable() {
        assert_eq!(
            derive_tier("acp/claude-code", None, false),
            Tier::Unavailable
        );
    }

    // The historical latch reads conservatively: a row a Child-B dev binary
    // once degraded to `pty` is never treated as structured — even if the
    // endpoint would now probe alive — so it lands in the refusal lane.
    #[test]
    fn historically_latched_row_is_unavailable_even_if_endpoint_alive() {
        assert_eq!(
            derive_tier("acp/claude-code", Some("pty"), true),
            Tier::Unavailable
        );
    }

    // A non-acp provider has no ACP tier.
    #[test]
    fn non_acp_provider_is_unavailable() {
        assert_eq!(derive_tier("claude-code", None, true), Tier::Unavailable);
    }

    // Red-team round 1 (TOCTOU): the connect-failure classification keys on
    // the POST-failure re-probe, never the stale pre-connect reading. Only
    // alive-AND-identity-verified is a wedge; everything else is a loss that
    // must preserve identity before refusing.
    #[test]
    fn connect_failure_alive_and_verified_is_a_wedge() {
        assert_eq!(
            classify_connect_failure(true, true),
            ConnectFailure::WedgeStillAlive
        );
    }

    #[test]
    fn connect_failure_dead_pid_is_a_transport_loss() {
        assert_eq!(
            classify_connect_failure(false, false),
            ConnectFailure::TransportLost,
            "died across the probe→connect boundary — the S1/S2 TOCTOU window"
        );
        assert_eq!(
            classify_connect_failure(false, true),
            ConnectFailure::TransportLost,
            "a dead pid is a loss even if a stale cmdline reading said ours"
        );
    }

    #[test]
    fn connect_failure_pid_reuse_inside_the_window_is_a_transport_loss() {
        assert_eq!(
            classify_connect_failure(true, false),
            ConnectFailure::TransportLost,
            "an alive pid that no longer carries our daemon's cmdline is exit+reuse, not a wedge"
        );
    }
}
