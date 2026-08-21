//! `provider/claude/pty/` — the claude PTY/mux-pane lane.
//!
//! Groups the claude-only code that drives a native-TUI `claude` process inside
//! a `zmx`/mux pane, mirroring the `pty/` groupings the codex and pi harnesses
//! each carry for the same shape of concern (a detached, ready-gated relaunch
//! into a pane the caller can `mux.attach` to). Today that is just
//! [`revive`] — the cold→drivable relaunch shared by `qd resume`, `qd attach`'s
//! cold arm, `qd send`'s wake path, the adoption relaunch, and the lane `wake`
//! path. A claude `pane.rs` (the fused `zmx attach` driver) would join it here
//! if/when it moves out of the shared pane machinery.
//!
//! Pure relocation: `revive` was `provider::claude::revive` (itself moved out
//! of the flat `dispatch` crate before that). The parent `claude/mod.rs`
//! carries `pub use self::pty::revive;`, re-exporting it back at
//! `crate::provider::claude::revive` so none of its five external callers
//! needed to change.

pub mod revive;
