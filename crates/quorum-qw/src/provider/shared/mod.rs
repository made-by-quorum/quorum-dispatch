//! `provider/shared/` — transport machinery that more than one HARNESS runs.
//!
//! # The rule this directory exists to enforce
//!
//! The provider tree is organised HARNESS-FIRST: `claude/`, `codex/`, `pi/` and
//! `opencode/` each own the code that knows about exactly one agent program, and
//! group it by the TRANSPORT that carries messages to it (`pty/`, `relay/`,
//! `app_server/`, `stdio/`, `daemon/`). That rule answers "where does this file
//! live?" for every file that knows one harness.
//!
//! It does not answer it for a transport that several harnesses run the SAME way,
//! and this crate has three of those. Putting them under a harness would be a
//! lie; duplicating them under each harness would be worse. They live here.
//!
//! # What is here, and why each one is genuinely shared
//!
//! - [`daemon`] — the daemon-hosted-resident primitives: [`daemon::DaemonSpawner`]
//!   (+ its real [`daemon::RealDaemonSpawner`]), port allocation
//!   ([`daemon::PortAllocator`]/[`daemon::real_alloc_port`]), the cmdline-identity
//!   probe ([`daemon::CmdlineProbe`]/[`daemon::real_cmdline_probe`]), and zombie
//!   reaping. `provider::acp`'s daemon lane, `provider::pi::daemon`, and
//!   `provider::codex::app_server::create` all spawn-and-reap a resident process
//!   through these SAME primitives; what differs per harness is the PROTOCOL
//!   spoken to the resident once it is up (codex's ws app-server RPC, pi's own
//!   stdio/daemon dialect, acp's JSON-RPC), which is why each harness keeps its
//!   own `run_new_daemon`-shaped create pipeline and its own cmdline MATCH
//!   (`cmdline_is_our_daemon` / `cmdline_is_our_pi_daemon` /
//!   `cmdline_is_our_acp_daemon`) layered on top of this shared probe.
//! - [`acp`] — the Agent Client Protocol spine. **The load-bearing case.** One
//!   parameterized [`acp::AcpProvider`] serves `acp/claude-code` AND
//!   `acp/opencode` today; the two registered instances differ by exactly three
//!   fields (`id`, `bridge_cmd`, `bridge_args`), and, as [`acp`]'s own docs put
//!   it, "only the bridge SPAWN differs; the protocol, queue, correlation, and
//!   verb dispatch are identical". Those per-harness bridge specs DO live under
//!   their harness ([`crate::provider::claude::acp`],
//!   [`crate::provider::opencode::acp`]) — what stays here is the protocol
//!   implementation none of them may fork. `doc/tbd/pi-acp-exploration/` sizes pi
//!   as a THIRD instance of the same driver, and `doc/tbd/acp-everywhere-report.md`
//!   floats collapsing all four native transports into it; both get cheaper the
//!   more strictly this stays one implementation.
//! - [`pane`] — [`pane::PaneDeps`], the effects bag every mux-pane lane needs.
//!   codex's and pi's TUI lanes drive the SAME [`crate::create::run_new`]
//!   pipeline with a different argv, so they need the same effects resolved the
//!   same way. Declared once so the two lanes cannot drift.
//! - [`attribution`] — the SENDER ENVELOPE the text-only lanes put in front of a
//!   delegated message (punch R10). codex's app-server turn and pi's prompt are
//!   both PLAIN STRINGS with no header field to carry a sender, so both render
//!   the identity `Provider::inject` is handed the SAME way — one rendering, one
//!   reply-path line, one inverse parser the content-keyed matchers un-wrap
//!   with. Two harnesses run it; a second copy is exactly the drift this
//!   directory exists to prevent.
//! - [`viewer`] — the human VIEWER pane on a daemon-hosted session
//!   ([`viewer::pane_name`] + [`viewer::reap_pane`]). `codex/app-server` and
//!   `acp/opencode` both have a residence that is a SERVER, so both answer "a
//!   daemon has no terminal" by opening a second CLIENT of it in a pane; the
//!   pane's name and its reap are identical for both, and neither function
//!   contains a line about a harness. What DOES know a harness — which argv the
//!   viewer runs, which refusals its row must clear — stays in the lane.
//! - [`fixture`] — [`fixture::FixtureDaemonProvider`], the compiled-in
//!   daemon-SHAPED conformance fixture. Harness-neutral by construction: it is
//!   the standing proof that the [`crate::provider::Provider`] trait is not
//!   claude-shaped, so it belongs to no harness at all.
//!
//! # The test for adding something here
//!
//! Two or more harnesses must run the code, not merely resemble each other.
//! codex's `app_server/` and pi's `stdio/` are both "spawn a resident, speak a
//! JSON-RPC dialect to it, reap it on stop" — they are ANALOGOUS, and their docs
//! cross-reference each other as such, but they share no code and no wire format.
//! Analogy is not sharing. They stay under their harness.

pub mod acp;
pub mod attribution;
pub mod daemon;
pub mod fixture;
pub mod pane;
pub mod viewer;
