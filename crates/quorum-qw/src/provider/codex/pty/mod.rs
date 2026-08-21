//! `provider::codex::pty` — the codex TUI-hosted-in-a-mux-pane lane (`qd start
//! --provider codex --interactive`, and the cold arms of `qd resume` / `qd
//! attach` / `qd send`): create/revive choreography for a pane the human drives
//! directly, plus the identity discovery that lane uniquely needs.
//!
//! Contrast with [`super::app_server`] (the ws JSON-RPC daemon lane, which has a
//! live rpc handle and gets its identity handed to it by `thread/start`): a
//! pty-hosted codex session speaks NO protocol to qd at all — it is just a
//! process attached to a pane — so its thread id can only be OBSERVED, later,
//! from the rollout it eventually opens (see [`tui`]'s module doc for the
//! measured "session exists" vs "identity exists" gap).
//!
//! Submodules:
//!   - [`pane`] — the mux-pane create + revive choreography (name claim, zmx
//!     preflight, attachability verify, viewer-pane reap on kill) — the pane twin
//!     of [`super::app_server::resume`], which is the daemon lane.
//!   - [`tui`]  — identity discovery: `pick_thread` binds a thread id from the
//!     rollout it opens once one appears (never sooner — a codex TUI does not
//!     open its rollout until the first interaction), plus the misattribution
//!     guard that makes deferred binding safe.
//!
//! Physically relocated out of the former flat `provider::codex::{pane,tui}`
//! during the harness-first reorg (PROVIDER-REORG-SPEC.md); the flat paths are
//! preserved as re-exports on `provider::codex` for existing callers.

pub mod pane;
pub mod tui;
