//! sbmux — terminal multiplexer with native scrollback passthrough.
//!
//! This crate provides a multiplexed terminal emulator and daemon infrastructure
//! for managing sessions with native scrollback handling.

pub mod cli;
pub mod client;
pub mod events;
pub mod headless;
pub mod headless_session;
pub mod procid;
pub mod protocol;
pub mod pty;
pub mod republish_hub;
pub mod screen;
pub mod server;
pub mod session;
pub mod stream_json;
