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

/// The env var overriding the opencode binary on a launch argv (else
/// `"opencode"`). The `QD_CODEX_BIN` precedent, for the same reason: a box with
/// a non-PATH or versioned install needs one place to say so.
const OPENCODE_BIN_ENV: &str = "QD_OPENCODE_BIN";

/// The opencode binary to launch: the `QD_OPENCODE_BIN` override, else
/// `"opencode"` off PATH.
///
/// Shared so the ACP bridge spawn and the human viewer (`opencode attach …`) can
/// never disagree about which binary is "opencode" on this box — a viewer built
/// from a different install than the bridge would be a second opencode talking
/// to the first one's server, which is exactly the confusion this resolves once.
///
/// NOTE the asymmetry with the bridge, which is spawned as the literal
/// `AcpProvider::bridge_cmd` (`"opencode"`) rather than through here. That is a
/// real gap, not a design: the bridge argv is a `&'static str` in a `const fn`
/// spec, so it cannot consult the env. Both resolve to `"opencode"` unless the
/// override is set, so today they agree; wiring the spec to the env is the fix
/// if that ever stops being true.
pub fn opencode_bin(env: &dyn crate::effects::Env) -> String {
    env.var(OPENCODE_BIN_ENV)
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "opencode".to_string())
}
