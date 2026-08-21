//! **qw — session management.**
//!
//! The package `qd` delegates to for everything about running an agent session:
//! lifecycle (start / wake / kill), enumeration and health, delivering a message
//! into the session, and attaching a terminal to it.
//!
//! # The boundary
//!
//! `qd` depends on `qw`, never the reverse. Everything qw needs to describe an
//! answer, qw owns — including [`contract::SessionStatus`], which is deliberately
//! qw's own type rather than a borrow of `dispatch::model::SessionStatus`. A
//! package boundary owns the types it reports; borrowing one back across the
//! boundary would invert the dependency.
//!
//! [`lane::Lane`] is the dispatch key and does **not** cross the boundary. qd
//! never holds one: the DTOs carry an opaque `provider` label for display, not a
//! matchable identity. Handing qd a `Lane` would make
//! `match summary.lane.harness { … }` the ergonomic thing to write, which is the
//! coupling this boundary exists to remove.
//!
//! # What is still outside
//!
//! `provider/*`, the per-provider gather functions, `qrmux` (session hosting) and
//! the session-lifecycle verb bodies all belong here and have not moved yet —
//! each is blocked on its own dependency untangling. The seven `LaneOps`
//! implementations likewise still live in the `qd` binary, because they delegate
//! to verb functions that a separate crate cannot reach. They move in when their
//! delegation targets do. Tracked in
//! `doc/tbd/provider-architecture/07-lane-gaps.md`.

// The same re-export dispatch carries, for the same reason: the modules moving
// into this crate resolve `crate::effects`, `crate::model`, `crate::idstore` and
// friends. Re-exporting quorum-core here means those paths keep resolving and NOT
// ONE import inside the moved files has to change — the move stays a relocation
// rather than a rewrite.
pub use quorum_core::{effects, exec, fmt, idstore, model, paths, timefmt, zmx_dir};

// The session-management modules, moved out of `dispatch` (qd/qw split step 2).
pub mod boot;
// The per-session control socket, moved out of `dispatch` alongside the
// `qrmux-server` daemon entry that derives its path (ruling D6). It brought no
// crate edges with it — the module is std-only — and it lands here rather than in
// the `quorum-core` leaf because its peers are qw's: the servicer that BINDS it is
// qw's embedded qrmux daemon, and [`livelock`], the sibling that entry resolves
// beside from the same (session_id, state_dir) pair, is already qw's. `dispatch`
// re-exports it, so `relay_server`'s `crate::control_sock::…` keeps resolving.
pub mod control_sock;
pub mod create;

/// Compatibility re-export of the two homes `create_daemon` split into.
///
/// The shared daemon primitives (spawn/kill, port allocation, the cmdline-
/// identity probe) went to [`provider::shared::daemon`] because acp, pi and
/// codex all spawn-and-reap a resident daemon through them — only the
/// PROTOCOL each resident speaks once it is up differs. The `run_new_daemon`
/// pipeline and its `DaemonError`/`DaemonDeps` surface went to
/// [`provider::codex::app_server::create`] because only codex ever drove it:
/// [`provider::codex::app_server::create::cmdline_is_our_daemon`] matching the
/// literal `"codex"`/`"app-server"` argv tokens is the proof — pi and acp have
/// their own cmdline matchers built on the same shared probe. This module
/// keeps the ~65 existing call sites across `quorum-qw` and `dispatch`
/// resolving unchanged.
pub mod create_daemon {
    pub use crate::provider::codex::app_server::create::*;
    pub use crate::provider::shared::daemon::*;
}

/// The five delivery carriers, and the ledger emitters they share.
///
/// Phase 3B of `doc/tbd/provider-architecture/11-stage3-plan.md`: the bodies the
/// deleted `lanes::Carriers` trait used to call back UP into the `qd` binary for.
/// See the module docs for the argument.
pub mod delivery;
pub mod embedded_mux;
pub mod events;
/// The gather half of `qd ls`: every effectful read the session join consumes.
/// qw gathers, qd merges — see the module doc.
pub mod gather;
pub mod identity;
// `LaneOps::await_idle` — the four per-harness idle watchers `qd wait` used to
// hold, and the two delivery observers that emit from them. Ruling D2.
pub mod idle;
pub mod jsonl;
pub mod kill;
pub mod lanes;
pub mod launch;
pub mod livelock;
pub mod liveness;
pub mod mux;
pub mod mux_selector;
// The observe/dashboard PURE FOLD over the daemon's `Republish*` control facts.
// Moved here with `wait_channel`, which holds a `DashboardState` — a qw crate may
// not reach back into `dispatch`, and this module's only dependency is
// `qrmux::protocol`, so it travels rather than being duplicated.
pub mod observe;
pub mod preflight;
pub mod provider;
pub mod provider_gather;
pub mod qrmux_dir;
// The `qrmux-server` daemon entry, hosted by BOTH binaries (ruling D6). It was
// `bin/qd/daemon.rs` until `current_exe()` — what the launcher in `embedded_mux`
// re-execs — started resolving to `qw`.
pub mod qrmux_server;
pub mod registry;
pub use provider::claude::relay;
pub mod resume;
pub mod safe_kill;
pub mod sendpty;
pub mod stats_cache;
pub mod submit;
/// A6 telemetry — the `marks.jsonl` stream, its two engine line kinds, and the
/// pure snapshot fold.
///
/// qw's, per `doc/tbd/provider-architecture/07-lane-gaps.md:549-553`: its only
/// non-core dependency is [`registry`], which is qw's, and its old `crate::render`
/// edge was cut for exactly this move. `dispatch` re-exports it, so every
/// `dispatch::telemetry::…` / `crate::telemetry::…` path keeps resolving.
pub mod telemetry;
pub mod tombstone;
pub mod wait;
// The live `qd wait` republish subscriber. It belongs beside the wait loop it
// feeds: `LaneOps::await_idle`'s claude arm builds it, and after the qd/qw split
// that arm runs inside `qw`.
pub mod wait_channel;
pub mod zmx_list;
pub mod zmx_mux;

pub mod conformance;
pub mod contract;
// The qd/qw WIRE (stage 4): `LaneOps` serialized as line-delimited JSON over a
// child's stdio. `wire::client::WireLane` is the qd-side `LaneOps` that is a
// subprocess; `wire::server::serve` is what the `qw` binary runs. Lives here
// rather than in `dispatch` because the qw binary must not link `dispatch` —
// see `src/bin/qw.rs`.
pub mod wire;
pub mod fixture;
pub mod lane;
pub mod lane_read;

pub use contract::{
    Confirmation, Degradation, DeliverPolicy, Health, HealthSource, KillOutcome, LaneError, LaneOps,
    LedgerAddress, Listing, Message, MessageId, Receipt, SessionHandle, SessionId, SessionStatus,
    SessionSummary, StartRequest, Terminal, TerminalExpectation,
};
pub use lane::{lane_for, Harness, Lane, Mode};
// `lane_ops_with_carriers` and `Carriers` were here until phase 3B. `lane_ops` is
// now the ONLY constructor, because `deliver`'s bodies are `delivery` functions
// rather than callbacks into the `qd` binary.
pub use lanes::{lane_ops, CarrierOutcome, LaneImpl, ReviveHandle};
