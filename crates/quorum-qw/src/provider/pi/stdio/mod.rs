//! `provider::pi::stdio` — the stdio JSONL-RPC transport to a `pi` child process.
//!
//! Grouped here because all three files answer the SAME question — "how does
//! this process talk to `pi` over its stdin/stdout" — but for TWO structurally
//! different transports that must not be confused:
//!
//!   - **The persistent daemon-internal driver** ([`stdio`] the file, holding
//!     [`PiStdio`]) — owns a long-lived `pi --mode rpc` child for the life of a
//!     resident session, correlating request/response pairs and demuxing
//!     interleaved events off one reader thread. This is the ONLY code that
//!     actually drives pi; everything else reaches it across process boundaries
//!     (see [`crate::provider::pi::daemon`]).
//!   - **[`floor`] — the one-shot spawn-and-drain path**: `pi -p --mode json`,
//!     run to completion and its NDJSON output collected, with no persistent
//!     child, no daemon, no correlation state. Unrelated to the resident lane
//!     except that it happens to also be pi-over-stdio.
//!
//! [`rpc`] is the wire CONTRACT both transports speak against: the frame/event
//! envelope types and the [`rpc::PiRpc`] trait ([`stdio::stdio`]'s [`PiStdio`]
//! and [`crate::provider::pi::daemon::remote::PiRemote`] are its two real
//! implementations — one in-daemon, one a ws client to the daemon). Keeping the
//! contract in this directory rather than beside its daemon-side caller is
//! deliberate: [`floor`] needs the same envelope types and has nothing to do
//! with the daemon lane at all.
//!
//! Pre-reorg callers spelled this transport's types directly under
//! `provider::pi::{rpc,stdio,floor}`; see the compatibility re-exports in
//! `provider::pi::mod` for why those paths still resolve unchanged.

pub mod floor;
pub mod rpc;
pub mod stdio;

// Compatibility re-export — preserves the pre-reorg `pi::stdio::PiStdio` path
// (this directory used to be the single file `pi/stdio.rs`). `CHANNEL_CAPACITY`
// needs no such line: it is consumed only inside `stdio.rs` and its own tests.
pub use self::stdio::PiStdio;
