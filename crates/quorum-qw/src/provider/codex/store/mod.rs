//! `provider::codex::store` — read-only codex on-disk state readers: rollout
//! JSONL and the best-effort `state_5.sqlite` `threads` cache. NEVER a contract
//! (codex-p2-spec section 3.4 designed-degrade) — every read here is permissive,
//! and every failure degrades to a rescan rather than an error.
//!
//! Contrast with [`super::app_server`] (the live ws JSON-RPC transport): nothing
//! in this module group ever opens a socket or speaks a protocol to a running
//! `codex` process. It only reads what codex has already written to disk, and it
//! is the ONLY place in the codex tree permitted to be wrong in the permissive
//! direction — a torn/compressed/garbage rollout or a stale sqlite row degrades
//! silently to "no data" or a rescan, never a hard error.
//!
//! Submodules:
//!   - [`rollout`] — permissive rollout-JSONL reading (taxonomy, `derive_status`,
//!     `read_stats`, filename parse) — NEVER a contract (codex-p2-spec section
//!     3.4 designed-degrade).
//!   - [`index`]   — READ-ONLY best-effort `state_5.sqlite` `threads` cache
//!     (codex-p2-spec section 6.3); every failure degrades to the rollout scan
//!     [`rollout`] performs directly.
//!
//! Physically relocated out of the former flat `provider::codex::{rollout,index}`
//! during the harness-first reorg (PROVIDER-REORG-SPEC.md); the flat paths are
//! preserved as re-exports on `provider::codex` for existing callers.

pub mod index;
pub mod rollout;
