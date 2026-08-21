//! The one wording that tells an agent session how to reach the other agent
//! sessions — and the two places that say it.
//!
//! ── WHY THIS IS A MODULE AND NOT TWO STRING LITERALS ────────────────────────
//! Punch items R9 and R21 are the same sentence pair arriving by two different
//! doors, and the punch list says so ("Shares its wording with R9"):
//!
//! - **R9** appends it to the relay MCP `INSTRUCTIONS`
//!   ([`dispatch::relay_server::mcp::INSTRUCTIONS`]), which a claude session
//!   reads at MCP handshake time. Before R9 that string explained only how to
//!   ANSWER — `reply`, the delivery guarantee, the ping-pong rule — and a cold
//!   session was never told that peers exist, how to find them, or how to
//!   originate.
//! - **R21** types it into a never-messaged claude pane as its opening turn
//!   ([`crate::provider::claude::revive`]). That send is what MINTS the provider
//!   session id the row was missing, so the same text that onboards the agent is
//!   also what makes the row addressable. Two jobs, one message.
//!
//! Two copies of one wording is a drift bug waiting for its first divergent
//! edit — the same argument [`crate::lanes::ReviveHandle`] records for its own
//! single definition. So it is defined ONCE, here.
//!
//! ── WHY HERE, AND WHY A MACRO ───────────────────────────────────────────────
//! **Here** because the dependency direction is load-bearing and one-way: `qd`
//! (the `dispatch` crate, which owns `relay_server`) depends on `qw`, NEVER the
//! reverse. A constant the relay server and the claude revive both reach must
//! therefore live at or below `qw`, and this is qw's own subject matter — what a
//! session is told about talking to other sessions.
//!
//! **A macro** because [`dispatch::relay_server::mcp::INSTRUCTIONS`] is a
//! `pub const &str` that is asserted byte-for-byte against a frozen MCP fixture
//! and against the bun reference server, and `&str` values cannot be
//! concatenated at const-evaluation time without hand-rolled byte-array
//! gymnastics. `concat!` CAN compose them — but only from literals, and it
//! expands macro arguments before it does. So the shared wording is a macro that
//! expands to a string literal: [`HOW_TO_REACH_PEERS`] is that literal standing
//! alone (what R21 sends), and `concat!("…", how_to_reach_peers!())` is that same
//! literal welded onto the end of the instructions (what R9 reads). One
//! definition, two shapes, no copy.
//!
//! ── WHAT THE WORDING HAS TO CARRY, AND WHAT IT MUST NOT ─────────────────────
//! Exactly two sentences: peers are discovered with `qd ls`, and
//! `qd send:relay <session> "…"` originates a message (`--wait` blocks for the
//! reply). R9 says two sentences and means it — the instructions string is read
//! by every claude session at handshake and every word in it competes with the
//! session's actual work. Voice matches the sentences already there: second
//! person, imperative, commands in single quotes.

/// The shared wording, as a string literal (see the module docs for why a macro).
///
/// Prefer [`HOW_TO_REACH_PEERS`] wherever a `&str` will do; reach for the macro
/// only where a LITERAL is required — i.e. inside a `concat!` that builds another
/// `const`.
#[macro_export]
macro_rules! how_to_reach_peers {
    () => {
        "Peers are the other sessions in this fleet: list them with 'qd ls' and address one by name. You originate a message with 'qd send:relay <session> \"...\"', adding --wait to block until the reply comes back."
    };
}

/// The shared wording as a constant — R21's opening message, and the tail of
/// R9's MCP instructions.
pub const HOW_TO_REACH_PEERS: &str = how_to_reach_peers!();

#[cfg(test)]
mod tests {
    use super::HOW_TO_REACH_PEERS;

    /// The two sentences R9 specifies, both present and both naming their verb.
    /// A future edit is free to reword; it is NOT free to drop either half, which
    /// is the whole content of the item.
    #[test]
    fn carries_both_halves_discovery_and_origination() {
        assert!(
            HOW_TO_REACH_PEERS.contains("qd ls"),
            "peers are discovered with `qd ls` — that half is R9's first sentence"
        );
        assert!(
            HOW_TO_REACH_PEERS.contains("qd send:relay <session>"),
            "`qd send:relay <session>` originates — that half is R9's second sentence"
        );
        assert!(
            HOW_TO_REACH_PEERS.contains("--wait"),
            "`--wait` is how a caller blocks for the reply; without it the second \
             sentence only half-explains the verb"
        );
    }

    /// Two sentences, not three. The instructions string is read by every claude
    /// session at handshake, so the cap is part of the item ("Do not bloat it").
    #[test]
    fn is_exactly_two_sentences() {
        let sentences = HOW_TO_REACH_PEERS.matches(". ").count() + 1;
        assert_eq!(
            sentences, 2,
            "R9 asks for exactly two sentences: {HOW_TO_REACH_PEERS}"
        );
    }
}
