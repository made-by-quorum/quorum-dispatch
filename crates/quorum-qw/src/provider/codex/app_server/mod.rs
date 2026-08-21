//! `provider::codex::app_server` — the codex `app-server` ws JSON-RPC transport:
//! the wire CONTRACT, the tungstenite driver, version pin/sniff, and the
//! DAEMON-lane resume/kill choreography that speaks to it.
//!
//! Everything in this group assumes a live `codex app-server` process reachable
//! over a ws connection (`ws://127.0.0.1:<port>`) — the SAME process qd spawns as
//! a daemon and drives via [`rpc::AppServerRpc`]. Contrast with
//! [`super::pty`] (a codex TUI hosted in a mux pane, which speaks NO protocol to
//! qd at all) and [`super::store`] (read-only rollout/sqlite readers that never
//! open a socket).
//!
//! Submodules:
//!   - [`rpc`]     — [`rpc::AppServerRpc`] contract + serde envelope types +
//!     typed [`rpc::RpcError`] (codex-p2-spec section 6.2; ADD-5 pattern).
//!   - [`ws`]      — [`ws::WsAppServer`], the tungstenite blocking driver
//!     (codex-p2-spec sections 3.1, 6.3).
//!   - [`version`] — the single-source version pin ([`version::PINNED`]),
//!     `codex --version` sniff, and the pure drift [`version::verdict`]
//!     (codex-p2-spec section 3.4).
//!   - [`resume`]  — `qd resume` / `qd kill` for the DAEMON lane: reconnect (or
//!     respawn) against the on-disk rollout truth, group-addressed kill, and the
//!     revive-to-DRIVABLE `thread/resume` choreography (codex-p2-spec section
//!     7.6).
//!   - [`create`]  — [`create::run_new_daemon`], the `qd new --provider codex`
//!     create pipeline (codex-p2-spec section 7.2), built on the shared
//!     spawn/port/cmdline-probe primitives in
//!     [`crate::provider::shared::daemon`]. Physically the former flat
//!     `src/create_daemon.rs` (PROVIDER-REORG-SPEC.md); the shared half of that
//!     file lives in `provider::shared::daemon` instead, since acp and pi drive
//!     it too.
//!
//! Physically relocated out of the former flat `provider::codex::{rpc,ws,
//! version,resume}` during the harness-first reorg (PROVIDER-REORG-SPEC.md); the
//! flat paths are preserved as re-exports on `provider::codex` for the ~200
//! existing call sites in `dispatch`, so nothing outside this module tree had to
//! change.

pub mod create;
pub mod resume;
pub mod rpc;
pub mod version;
pub mod ws;
