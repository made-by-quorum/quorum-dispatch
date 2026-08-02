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

pub mod acp_residence;
pub mod adoption;
pub mod archive;
pub mod backends;
pub mod bindphase;
pub mod boot;
pub mod bootstrap;
pub mod codes;
pub mod conformance;
pub mod control_sock;
pub mod create;
pub mod create_daemon;
pub mod effects;
pub mod embedded_mux;
pub mod events;
pub mod exec;
pub mod extensions;
pub mod fmt;
pub mod fork_seed;
pub mod gc;
pub mod health;
pub mod identity;
pub mod idstore;
pub mod inbox_gc;
pub mod join;
pub mod jsonl;
pub mod kill;
pub mod launch;
pub mod livelock;
pub mod liveness;
pub mod model;
pub mod mux;
pub mod mux_selector;
pub mod observe;
pub mod paths;
pub mod ping;
pub mod preflight;
pub mod presence;
pub mod progress;
pub mod provider;
pub mod qrmux_dir;
pub mod reconcile;
pub mod recovery;
pub mod redact;
pub mod registry;
pub mod relay;
pub mod relay_http;
pub mod relay_presence;
pub mod relay_server;
pub mod render;
pub mod resolve;
pub mod resume;
pub mod resume_daemon;
pub mod safe_kill;
pub mod secrets;
pub mod sendpty;
pub mod shell_init;
pub mod stats_cache;
pub mod status_recency;
pub mod stray;
pub mod submit;
pub mod survey;
pub mod telemetry;
pub mod tombstone;
pub mod update;
pub mod wait;
pub mod wait_channel;
pub mod zmx_dir;
pub mod zmx_list;
pub mod zmx_mux;

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
