//! `provider::pi::daemon` — the pi-daemon RESIDENT lane (item 1 + item 3): the
//! machinery that makes a pi session reachable across separate `qd` CLI
//! invocations, since pi itself is stdio-only and has no self-listening server.
//!
//! Four files, two sides of one process boundary:
//!
//!   - **[`residence`] — the HOST side.** `run_pi_adapter`, the `qd pi-daemon`
//!     resident entry: spawns + owns the `pi --mode rpc` child (over
//!     [`crate::provider::pi::stdio::PiStdio`]) and serves a loopback ws front
//!     ([`residence::serve_pi`]) for the life of the session. This is the
//!     process a `create`/`resume` spawns DETACHED and that outlives the `qd`
//!     invocation that spawned it — that outliving IS residence.
//!   - **[`remote`] — the CLIENT side.** [`remote::PiRemote`] is a short-lived
//!     `qd` invocation's [`crate::provider::pi::stdio::rpc::PiRpc`] — it is a
//!     **ws/`TcpStream` client that reaches across to the resident's loopback
//!     front, NOT a stdio transport.** Do not confuse it with
//!     [`crate::provider::pi::stdio::PiStdio`] (the daemon-INTERNAL stdio driver
//!     that actually owns the pi child): `PiRemote` never touches pi's
//!     stdin/stdout, only the resident's front-door socket.
//!   - **[`create`]** (`pi/daemon.rs` pre-reorg) — the create choreography:
//!     claim → port-alloc → spawn the resident detached → readiness → registry
//!     row, plus the item-3 pgid teardown. The codex `create_daemon` mirror.
//!   - **[`resume`]** — the resume DECISION around the same choreography: the
//!     resumability gate, the already-alive no-op, split out of the `qd resume`
//!     verb body.
//!
//! `create` is re-exported at this module's root (`pub use self::create::*`) so
//! `provider::pi::daemon::{create_pi_session, PiCreateDeps, PiCreateParams,
//! PiCreateError, teardown_pi_daemon, ...}` keeps resolving exactly as it did
//! pre-reorg, when `daemon.rs` WAS this module — the file just moved sideways
//! into `create.rs` to make room for this directory taking the `daemon` name.

pub mod create;
pub mod remote;
pub mod residence;
pub mod resume;

// Compatibility re-export — `daemon.rs` (the create/resume choreography + pgid
// teardown) became `daemon/create.rs` when this directory claimed the `daemon`
// name; re-exporting its items here keeps every pre-reorg
// `pi::daemon::{create_pi_session, PiCreateDeps, PiCreateParams, PiCreateError,
// PiCreateOutcome, pi_daemon_is_alive, teardown_pi_daemon}` call site resolving
// unchanged.
pub use self::create::*;
