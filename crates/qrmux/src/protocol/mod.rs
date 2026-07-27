//! Length-prefixed binary protocol for client-server communication over Unix sockets.
//! A fixed 5-byte version preamble (see [`handshake`]) precedes all frames;
//! messages are then serialized with bincode and framed with a 4-byte
//! big-endian length prefix. See `crates/qrmux/PROTOCOL.md` for the wire spec.

pub mod codec;
pub mod handshake;
pub mod messages;

pub use codec::{encode, read_one_message, FrameReader};
pub use handshake::{
    read_preamble, validate_hello_caps, write_preamble, PreambleCheck, ERR_EXPECTED_HELLO,
    HISTORY_LOGICAL_STREAM_V1_CAP, HISTORY_LOGICAL_V1_CAP, INITIAL_SIZE_CONFIRM_V1_CAP,
    PROTOCOL_VERSION,
};
pub use messages::{ClientMsg, ConnectMode, ServerMsg, SessionInfo};

#[cfg(test)]
mod tests_history_protocol;
