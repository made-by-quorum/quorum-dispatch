//! `qd` — the engine crate of the qd→Rust rewrite.
//!
//! Phase A1: the data layer behind `qd ls --json` / `qd info`, ported from the
//! TypeScript qd (`~/work/switchboard`, read-only source of truth) with its
//! war-story comments carried forward (LESSONS.md rule 1 — comments-carry).
//!
//! Architecture (spec: ws/switchboard/rust/exec/a1-spec.md):
//! - **Deciders are pure.** All I/O lives behind the seam traits in [`effects`]
//!   and the [`mux::Mux`] trait; gather functions collect inputs, deciders join.
//! - **Permissive schema parsing** (CONVENTIONS.md, lesson L8): every struct
//!   deserialized from persisted/external data uses `#[serde(default)]` +
//!   `Option<T>`, never `deny_unknown_fields`. Corrupt blobs fail CLEANLY.
//! - **The registry is a disposable snapshot of the event stream** (ADD-3):
//!   see [`registry`] module docs. Lineage = `spawned_by` ONLY (ADD-3a/3b).

pub mod archive;
pub mod bindphase;
pub mod bootstrap;
pub mod codes;
pub mod conformance;
// Moved to the `quorum-core` LEAF crate (qd/qw split): infrastructure both qd and
// qw need, owned by neither. RE-EXPORTED here so every existing
// `crate::effects::…` / `dispatch::model::…` path keeps resolving — the move is a
// relocation, not an API change.
pub use quorum_core::{discovery, effects, exec, fmt, idstore, model, paths, redact, zmx_dir};

// Session management now lives in the `quorum-qw` crate. RE-EXPORTED so every
// existing `dispatch::provider::…` / `crate::registry::…` path keeps resolving —
// including nested ones, since re-exporting a module carries its whole submodule
// tree. 58 of the 94 integration tests reach in this way.
//
// Two of these are session STATE rather than session management, and each is here
// for its own reason. `tombstone` is provider-specific by CONTRACT even though its
// code is not — read its Scope note: it is written only for `acp/claude-code`, by
// the ACP transport-loss paths, and it sits beside the janitored `registry` row it
// exists to outlive. `identity` was headed for the `quorum-core` leaf — it knows
// nothing about any provider — and stopped here instead because `proc_key()` calls
// `liveness::ProcKey::new`, and `liveness` is qw; core is a strict leaf and may not
// depend on qw.
pub use quorum_qw::{
    boot, control_sock, create, create_daemon, embedded_mux, events, identity, jsonl, kill, launch,
    livelock, liveness, mux, mux_selector, observe, preflight, provider, provider_gather,
    qrmux_dir, registry, relay, resume, safe_kill, sendpty, stats_cache, submit, telemetry,
    tombstone, wait, wait_channel, zmx_list, zmx_mux,
};

// Provider-specific code lives in `quorum-qw` (the enforced rule). These modules moved
// there but keep their old FLAT dispatch names via `as` re-exports, so every existing
// `dispatch::acp_residence::…` / `crate::backends::…` path keeps resolving — again a
// relocation, not an API change.
pub use quorum_qw::provider::acp::residence as acp_residence;
pub use quorum_qw::provider::claude::{adoption, backends, fork_seed, relay_http};

pub mod dispositions;
pub mod extensions;
pub mod gc;
pub mod health;
pub mod inbox_gc;
pub mod join;
pub mod lane;
pub mod origin_send;
pub mod ping;
pub mod presence;
pub mod progress;
// The stage-2 gate (doc/tbd/provider-architecture/06-stage2-plan.md phase 6): a
// source scan pinning where provider strings may appear in the verb layer, and
// driving ROUTING on them to zero. Test-only — it reads this crate's own `src/`
// and has no production caller.
#[cfg(test)]
mod provider_gate;
// The stage-3 gate: a source scan pinning which qd files may name qw's DELIVERY
// log at all, so `09-ledger-split.md`'s "qd does not read qw's" is measured
// rather than remembered. Test-only, same shape as `provider_gate`.
#[cfg(test)]
mod ledger_gate;
pub mod reconcile;
pub mod recovery;
pub mod relay_presence;
pub mod relay_server;
pub mod render;
pub mod resolve;
// `resume_daemon` was two modules wearing one name: its doc claimed "DAEMON-hosted
// (codex)" while a third of its body was the ACP adapter's kill / alive-decision /
// resume-claim / bridge-verify machinery, which never touches the codex app-server. It
// split along that seam and both halves moved to `quorum-qw`
// (`provider::codex::resume`, `provider::acp::resume`).
//
// A FACADE, not a re-export of one module: `bin/qd/verbs/{kill,resume,wait}.rs` and
// three live integration tests reach in through the single flat `dispatch::resume_daemon`
// path, so both halves have to arrive under it. The globs are unambiguous because the
// one genuinely shared item — the kill result — is defined ONCE, in
// `quorum_qw::create_daemon::DaemonKillOutcome`, next to the pgid ladder it reports on.
// (It is named off `KillOutcome` deliberately: `quorum-qw` already exports a DIFFERENT
// `KillOutcome`, the `LaneOps` contract DTO carrying `reaped`/`tombstoned`
// `Confirmation`s. Same word, different question — one asks "was there a live daemon to
// signal", the other "did the kill land".)
pub mod resume_daemon {
    pub use quorum_qw::create_daemon::DaemonKillOutcome;
    pub use quorum_qw::provider::acp::resume::*;
    pub use quorum_qw::provider::codex::resume::*;
}
pub mod secrets;
pub mod setup;
pub mod shell_init;
pub mod status_recency;
pub mod stray;
pub mod survey;
pub mod update;

/// Returns the crate's marker name. Anchor kept from the 0a scaffold (the 0a
/// gate's smoke test references it).
pub fn crate_marker() -> &'static str {
    "qd"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_marker_is_qd() {
        assert_eq!(crate_marker(), "qd");
    }
}
