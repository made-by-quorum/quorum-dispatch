//! `provider/claude/` — the claude-specific corner of the provider seam.
//!
//! LIVES IN qw (qd/qw split): ALL provider-specific code belongs in `quorum-qw`, and
//! these modules are claude-only by their own declarations. They arrived here as pure
//! relocations out of `dispatch` — `dispatch` re-exports each one under its old flat
//! name so every `dispatch::fork_seed::…` / `dispatch::backends::…` call site keeps
//! resolving.
//!
//! [`ClaudeProvider`](super::ClaudeProvider) itself still lives in `provider.rs`
//! alongside the trait it implements; moving it is a separate step. This directory
//! holds only the claude-only machinery that already stood alone.
//!
//! ## Harness-first reorg (transport grouped below the harness)
//!
//! Two sub-directories group claude's transport code by KIND, matching the
//! `codex/` and `pi/` shape:
//!
//! - [`relay`] — the cc-relay HTTP channel: the [`relay::RelayContract`]
//!   engine surface (this module was the crate-root `src/relay.rs`) plus its
//!   [`relay::http`] HTTP transport (was `claude/relay_http.rs`).
//! - [`pty`] — the native-TUI mux-pane lane: today just [`pty::revive`] (was
//!   `claude/revive.rs`), the cold→drivable relaunch shared by `qd resume`,
//!   `qd attach`, `qd send`'s wake path, and the adoption relaunch.
//!
//! The remaining three modules stay flat here because none of them knows about
//! a transport at all — each is transport-NEUTRAL claude knowledge:
//! [`adoption`] classifies an already-running process + its sidecar state,
//! [`backends`] reads `backends.json` launcher env, and [`fork_seed`] is a
//! pure text transform. There is no second harness to share any of the five
//! with, and no transport split within them to make.
//!
//! `pub use` lines below preserve every pre-reorg path
//! (`crate::provider::claude::relay_http::…`,
//! `crate::provider::claude::revive::…`) so the ~200 external call sites this
//! reorg must not disturb keep resolving unchanged.

pub mod acp;
pub mod adoption;
pub mod backends;
pub mod fork_seed;
pub mod pty;
pub mod relay;

// Preserve the pre-reorg paths: `relay_http` was a top-level module here, and
// `revive` was a top-level module here, before this reorg nested them under
// `relay/` and `pty/` respectively.
pub use self::pty::revive;
pub use self::relay::http as relay_http;
