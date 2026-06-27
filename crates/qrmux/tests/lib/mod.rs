//! Test utilities: jail setup/teardown, assertions, and shared infrastructure.
//!
//! This module provides:
//! - `jail`: Hermetic per-test environment (own HOME, XDG_*, etc.)
//! - `assertions`: Reusable comparators (backlog-completeness, scroll-intact, etc.)
//! - `client`: Daemon interaction helpers (socket connection, command invocation)

pub mod assertions;
pub mod b3_checkers;
pub mod client;
pub mod daemon_reaper;
pub mod jail;

pub use assertions::{assert_altscreen_replay, assert_scroll_intact};
pub use client::{
    capture_session, create_session, list_sessions, pid_alive, qrmux_binary, send_to_session,
    send_to_session_stdin, sha256, start_daemon_in_jail, AttachedClient,
};
// B3 (M3a) additions — NOT re-exported via the glob above (kept off the glob so
// test binaries that don't use them get no `unused_imports` warning under
// `-D warnings`). The non-collapsing recorder and the ordered-backlog comparator
// are reached via their module paths: `libmod::client::{record_attach, Recorded,
// RecordedFrame}` and `libmod::assertions::assert_backlog_ordered`.
pub use jail::{jail_env, setup_jail, teardown_jail};
