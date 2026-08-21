//! `provider/opencode/` — the opencode-specific corner of the provider seam.
//!
//! OpenCode's LIVE transport is ACP (Agent Client Protocol) — the same
//! parameterized driver [`crate::provider::shared::acp`] serves for
//! `acp/claude-code`. There is no opencode-only PROTOCOL code to hold here,
//! and that is the point: the only piece of that lane which is genuinely a
//! fact about opencode is WHICH bridge program qd spawns, and it lives beside
//! this module in [`acp`] — four lines, against a shared driver opencode did
//! not have to write.
//!
//! What DOES belong here, and is the whole of this directory today, is
//! [`store`]: a READ-ONLY reader over opencode's own on-disk `opencode.db`
//! SQLite store — the mechanism `qd ls` uses to surface a cold OpenCode
//! session without going through ACP at all. It knows nothing about the live
//! transport; it only knows opencode's storage format.
//!
//! Pure relocation: `store` was this module's own body (`opencode/mod.rs`)
//! before the harness-first reorg split "opencode's on-disk store" out from
//! "opencode's live transport" as a matter of principle, even though the
//! transport half isn't populated yet. The `pub use` lines below preserve
//! every pre-reorg path (`crate::provider::opencode::store_dir`,
//! `::sessions`, `::OpencodeSession`, `::OPENCODE_DB_FILENAME`,
//! `::PROVIDER_ID`) so existing call sites keep resolving unchanged.

pub mod acp;
pub mod store;

pub use store::{sessions, store_dir, OpencodeSession, OPENCODE_DB_FILENAME, PROVIDER_ID};
