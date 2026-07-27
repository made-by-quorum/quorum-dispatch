//! qrmux — terminal multiplexer with native scrollback passthrough.
//!
//! This crate provides a multiplexed terminal emulator and daemon infrastructure
//! for managing sessions with native scrollback handling.

pub mod attended;
pub mod cli;
pub mod client;
pub mod events;
pub mod procid;
pub mod protocol;
pub mod pty;
pub mod screen;
pub mod server;
pub mod session;
pub mod stream_json;
