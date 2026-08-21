//! `provider::pi::extension` — the `pi/extension` lane: a real pi TUI in a mux
//! pane that `qw` can also drive, over a control channel served from inside pi's
//! own process.
//!
//! # What this lane is for
//!
//! `pi/mux-pane` already puts a pi TUI in a pane a human can attach to, and it
//! already delivers into it. It does so by TYPING: the carrier writes the
//! message into the pane's PTY as keystrokes and then watches the transcript on
//! disk to decide whether it landed
//! (`qrmux::attended::fire::AcceptanceSignal::Landing`). That works, and it is
//! inference at every step. It cannot ask whether pi is busy, cannot address a
//! turn, cannot distinguish a landed message from a coincidentally-appearing
//! one, and is blind for the whole of pi's lazy-write window — a fresh session
//! writes NOTHING to disk until its first assistant reply
//! (`provider::pi::pty::tui`).
//!
//! This lane replaces the inference with a question. pi's extension API exposes
//! `sendUserMessage`, `isIdle`, `abort` and the `agent_start` / `agent_settled`
//! lifecycle; an extension loaded into the session serves them over a unix
//! socket, and `qw` asks. Delivery is acknowledged, status is reported, and
//! turns are counted by the process that is having them.
//!
//! Crucially it gives all that up NOTHING: the TUI is real, the pane is real,
//! and the human attaches and types into the same composer. Two clients, one
//! session — the pi analogue of `codex/app-server`, arrived at from the
//! opposite direction. codex had a daemon that a TUI could join; pi has a TUI
//! that can be taught to answer.
//!
//! # The three pieces
//!
//!   - [`install`] — where the extension source lives, how it gets onto disk,
//!     and the socket path math.
//!   - [`client`] — `qw`'s half of the wire: connect, `deliver`, `health`,
//!     `await_idle`, `interrupt`.
//!   - [`create`] — the launch, which is the pi-TUI pane create plus a socket
//!     flag and two row fields.
//!
//! The server half is not Rust and does not live under `src/`: it is
//! `assets/pi-extension/quorum-lane.ts`, baked into the binary by
//! [`install::SOURCE`]. Read it alongside [`client`] — they are one protocol in
//! two languages, and neither file is complete on its own.
//!
//! # The flag, and why it is the whole safety story
//!
//! The extension installs to pi's GLOBAL discovery directory, so it loads into
//! every pi session on the machine. It does nothing at all unless
//! `--quorum-sock <path>` (or `$QUORUM_PI_SOCK`) names a socket to serve. That
//! gate is why a global install is acceptable: a session `qw` did not launch
//! gets one registered flag and no behaviour.

pub mod client;
pub mod create;
pub mod install;

pub use client::{AwaitOutcome, Client, ClientError, Health};
pub use create::{
    create_extension_session, endpoint_for, plan_extension_launch, revive_extension_session,
    socket_for, socket_from_endpoint, ExtensionLaunch,
};

/// The CLI flag the extension registers, and the launch passes.
///
/// One constant because it is spelled in two places that must agree and cannot
/// check each other: pi's `launch_plan` emits it, and the TypeScript reads it
/// via `pi.getFlag`. A drift between them is a silently channel-less session.
pub const SOCK_FLAG: &str = "--quorum-sock";
