//! `provider::pi::pty` — the pi TUI hosted in a mux pane (`qd start --provider
//! pi --interactive`, and the interactive arms of `qd resume` / `qd attach`).
//!
//! This is pi's OTHER lane, structurally unrelated to [`crate::provider::pi::daemon`]:
//! no resident, no stdio-RPC transport at all — a human attaches a mux pane
//! directly to the real `pi` TUI binary, and qd's job is only to launch it with
//! the right argv/identity and keep the registry row honest.
//!
//! Two files, split the same way as the codex pane lane:
//!   - [`pane`] — the create + revive CHOREOGRAPHY: atomic name claim, argv,
//!     the mux-pane pipeline, the registry row write, the anti-adoption guard.
//!     Nothing here prints and nothing here exits (see the module docs on why
//!     that discipline matters — the CLI verb owns attribution, not this).
//!   - [`tui`] — identity + on-disk FACTS about a pi session running as a TUI:
//!     the `--session-id` capability preflight, whether an id is already taken,
//!     and why pi does not need codex's after-the-fact attribution apparatus
//!     (pi's `--session-id` names the session before it is spawned).

pub mod pane;
pub mod tui;
