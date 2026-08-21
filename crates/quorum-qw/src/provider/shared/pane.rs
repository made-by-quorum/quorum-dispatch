//! `provider::pane` — the effects a MUX-PANE provider lane needs, in one place.
//!
//! [`crate::provider::codex::pty::pane`] and [`crate::provider::pi::pty::pane`]
//! host their provider's TUI in a mux pane by driving the SAME create pipeline claude uses
//! ([`crate::create::run_new`]) with a different argv. They therefore need the
//! same effects, resolved the same way, and this is that set — declared once so
//! the two lanes cannot drift apart, and so the binary builds it once.
//!
//! It is [`crate::create::NewDeps`]'s shape minus the two fields a pane lane pins
//! rather than injects — the provider is definitionally its own, and the boot
//! waiter is [`crate::create::OkBootWaiter`] because readiness for a pane IS the
//! I6 attachability verify `run_new` already ran — plus the ids-store path both
//! lanes pre-mint into.
//!
//! Nothing is CONSTRUCTED from these; they are handed in. The backend, the socket
//! dirs, the mux and the ids-store path are all resolved by the caller, exactly as
//! `run_new`'s own verb resolves them.

use std::path::PathBuf;

use crate::effects::{Clock, Env};
use crate::exec::Exec;
use crate::mux::Mux;
use crate::mux_selector::Backend;
use crate::paths::QdPaths;

/// Injected effects + resolved paths for one mux-pane create or revive.
pub struct PaneDeps<'a> {
    /// Env for the launch-flag + agent-dir precedence `run_new` applies (L9a),
    /// and for the provider-specific preflights that read it.
    pub env: &'a dyn Env,
    /// The exec seam — used only by `run_new`'s preflight probe.
    pub exec: &'a dyn Exec,
    /// Clock: the discovery floor, the claim payload, the row stamps.
    pub clock: &'a dyn Clock,
    /// Home→state layout (L9a). The registry row is written under
    /// `paths.sessions_dir`, and a revive's tombstone is consumed there.
    pub paths: &'a QdPaths,
    /// The backend-selected mux. Also re-listed after `run_new` to key the row by
    /// the LIVE pane's pid.
    pub mux: &'a dyn Mux,
    /// The selected mux backend (C1 M4fix — the launch-failure error path names
    /// the ACTUAL backend).
    pub backend: Backend,
    /// The canonical socket dir the pane is created in (Bug D keystone, L1).
    pub canonical_dir: PathBuf,
    /// Legacy candidate socket dirs — the I6 verify + live-name pre-check scan
    /// canonical THEN these (canonical-wins dedupe).
    pub legacy_dirs: Vec<PathBuf>,
    /// The stable-id store. Both lanes pre-mint into it because the id must exist
    /// at env-bake time so the pane's env file can export `QD_SESSION_ID` (a `qd`
    /// run from INSIDE the session then knows which session it is).
    pub ids_path: PathBuf,
}
