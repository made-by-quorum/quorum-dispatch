//! **The WIRE, named.** `qd send --carrier <pty|relay>`, `qd send --wait`, and
//! the one stderr line the three `send:<carrier>` verbs now print before they do
//! exactly what they did yesterday.
//!
//! ── WHY THIS MODULE EXISTS ──────────────────────────────────────────────────
//! `send:pty` / `send:relay` were never lane SELECTORS. Bare `qd send` already
//! picks the lane, and for a claude pane row it picks the WIRE too —
//! [`quorum_qw::lanes::claude_carrier`] resolves `(Some(port), _) => Relay`, so a
//! claude session that has a relay port gets a relay send from bare `qd send`
//! while `send:pty` typed into the pane. Those two are not two spellings of one
//! delivery:
//!
//!   - the PANE wire arrives as **typed user input** — a leading `/` is a slash
//!     command and the session RUNS it;
//!   - the RELAY wire arrives as a **channel notification**, which is never a
//!     command (`frame/src/delivery.rs:107` records this as a spike finding).
//!
//! So the carriers could not simply be deleted in favour of bare `send`: bare
//! `send` had no way to say "the pane wire". `--carrier` is that way, and it is
//! the whole reason the three verbs can be deprecated rather than kept.
//!
//! ── WHAT IS SHARED AND WHAT IS DELIBERATELY NOT ─────────────────────────────
//! The two wires' DELIVERY bodies are shared: `--carrier pty` runs
//! [`super::send::run_send_pty_resolved`] and `--carrier relay` runs
//! [`super::send_relay::run_with_client`] — the same bodies the deprecated verbs
//! run, not copies of them. What is NOT shared is the FRONT DOOR, and that is
//! the point of the split below:
//!
//!   - the deprecated verbs keep their own front door byte-for-byte (uncapped
//!     resolve, `No session matching "x"` with a capital N, exit **1**, and a
//!     tombstone refusal that says "resume it first"). That contract is what
//!     existing callers grep — `dispatch/test/golden/scenarios/a3_state_assertions.sh`
//!     and `tests/resolve_beyond_cap.rs` both read that exact string — and
//!     deprecating rather than deleting means keeping it;
//!   - `qd send --carrier` comes through the UNIFIED front door (address
//!     desugaring, `resolve_target`'s `refused{unknown}`/`refused{ambiguous}` at
//!     exit **12** in lowercase, the self-send fence, the id-collision refusal),
//!     because it is `qd send`, and a flag must not move the verb's door.
//!
//! ── AND THE COLD TARGET IS REFUSED, NOT WOKEN ───────────────────────────────
//! Bare `qd send` deliberately WAKES a stopped target (`send_unified.rs`,
//! `wake_if_cold: true`). A PINNED wire cannot: a pane to type into and a relay
//! port to POST at are both properties of a RUNNING session, and reviving one in
//! order to honour a `--carrier` flag would make the flag do something nobody
//! asked it to. [`refuse_not_live`] names that, and it is also what keeps the
//! deprecated verbs' "never revive" posture from being quietly inverted by the
//! new flags.

use clap::ArgMatches;

use dispatch::model::Session;
use dispatch::origin_send::Refusal;
use dispatch::relay_http::CcRelay;
use quorum_qw::lane::Lane;

/// Which WIRE a send goes over, when the caller names one.
///
/// Two, not three. `send:http` has no variant here and that is a decision, not
/// an omission — see [`deprecated_http`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carrier {
    /// Type the message into the session's mux pane, as if a human typed it.
    /// Arrives as USER INPUT: a leading `/` executes.
    Pty,
    /// Hand the message to the session's message-passing wire — the claude relay
    /// server's HTTP endpoint, or a resident's own protocol (codex turn, ACP
    /// prompt, pi resident turn). Arrives as a NOTIFICATION: never a command.
    Relay,
}

impl Carrier {
    /// The `--carrier <value>` token. Also the value clap accepts, so the two
    /// cannot drift.
    fn token(self) -> &'static str {
        match self {
            Carrier::Pty => "pty",
            Carrier::Relay => "relay",
        }
    }

    fn parse(raw: &str) -> Option<Carrier> {
        match raw {
            "pty" => Some(Carrier::Pty),
            "relay" => Some(Carrier::Relay),
            _ => None,
        }
    }
}

/// The carrier-shaped half of a `qd send` invocation.
///
/// Built ONLY when the caller actually asked for one — see [`from_send_matches`],
/// whose `None` is what keeps the default `qd send` path byte-identical.
pub struct Request {
    pub carrier: Option<Carrier>,
    pub wait: bool,
    pub raw: bool,
    pub full: bool,
    /// The raw `--timeout` string. Kept as typed because the two wires parse it
    /// differently and both parsers are pinned: the pane wire runs
    /// `pty::timeout_ms` (and raises it to its own 75s delivery-watch floor), the
    /// relay wire runs `send_relay::parse_timeout`'s leading-integer read.
    pub timeout: String,
}

/// The `--timeout` default both deprecated verbs already carry (`"120"`), spelled
/// once so `qd send`'s new flag cannot drift from them.
const DEFAULT_TIMEOUT: &str = "120";

/// Did this `qd send` ask for a wire?
///
/// `None` — no `--carrier`, no `--wait` — is the load-bearing answer: it is what
/// `run_send_unified` tests to decide whether ANY of this module runs, so the
/// default send path keeps `LaneOps::deliver`, its write-then-deliver envelope,
/// its claim lock and its disposition stamps exactly as they are today. These
/// flags ADD a path; they do not re-tune the one that exists.
///
/// `--raw`/`--full`/`--timeout` alone do NOT arm it. They are modifiers of a wait
/// that is not happening, so on their own they must stay the no-op they are on
/// `send:pty` rather than silently divert delivery.
pub fn from_send_matches(m: &ArgMatches) -> Option<Request> {
    let carrier = m
        .get_one::<String>("carrier")
        .and_then(|raw| Carrier::parse(raw));
    let wait = m.get_flag("wait");
    if carrier.is_none() && !wait {
        return None;
    }
    Some(Request {
        carrier,
        wait,
        raw: m.get_flag("raw"),
        full: m.get_flag("full"),
        timeout: m
            .get_one::<String>("timeout")
            .cloned()
            .unwrap_or_else(|| DEFAULT_TIMEOUT.to_string()),
    })
}

/// Whether any carrier-shaped flag is present at all — including the modifiers
/// [`from_send_matches`] deliberately ignores.
///
/// Used by the INBOUND-mode door, which must refuse every origin-only flag rather
/// than accept-and-drop it. An envelope carries its own body and its own
/// correlation id; it carries no wire choice and no wait, so naming one alongside
/// `--inbound-envelope` is a contradiction the door says out loud — the same
/// posture `--expires`, `--host` and `--correlation-id` already take there.
pub fn inbound_conflict(m: &ArgMatches) -> Option<&'static str> {
    if m.get_one::<String>("carrier").is_some() {
        return Some("--carrier");
    }
    if m.get_flag("wait") {
        return Some("--wait");
    }
    if m.get_flag("raw") {
        return Some("--raw");
    }
    if m.get_flag("full") {
        return Some("--full");
    }
    if m.get_one::<String>("timeout").is_some() {
        return Some("--timeout");
    }
    None
}

// ===========================================================================
// `qd send --carrier` / `qd send --wait` — the unified front door's carrier arm
// ===========================================================================

/// Run a `qd send` that named a wire.
///
/// Reached from `run_send_unified` AFTER its whole front door has run — address
/// desugaring, `resolve_target`, the self-send fence, the id-collision refusal
/// and the by-id refresh — and after the row's [`Lane`] is in hand. Everything
/// this function refuses is refused BEFORE a byte is delivered, because a
/// `--wait` the lane cannot honour must not become a send that silently returns
/// nothing.
///
/// `lane` is `None` for a provider no lane can address; that case never reaches a
/// wire, so it is refused here rather than handed to a carrier that would have to
/// re-derive the same verdict.
pub fn run_from_unified(
    session: &Session,
    lane: Option<Lane>,
    live: bool,
    query: &str,
    message: &str,
    req: &Request,
) -> i32 {
    let label = session.name.as_deref().unwrap_or(query);

    let Some(lane) = lane else {
        return Refusal::refused(
            "carrier",
            format!(
                "\"{label}\" has provider \"{}\", which no lane can address — there is no wire to pin",
                session.provider
            ),
        )
        .emit();
    };

    // A pinned wire needs a RUNNING session; see this module's header for why
    // `--carrier` must not inherit bare `send`'s wake.
    if !live {
        return refuse_not_live(label);
    }

    // THE `--wait` GATE, and it is the LANE that answers it. Never a provider
    // list here: `Lane::captures_reply` is the property, and its doc carries the
    // reasoning for the one lane that has it.
    if req.wait && !lane.captures_reply() {
        return refuse_wait_unsupported(label, lane);
    }

    let carrier = match req.carrier {
        Some(c) => c,
        // AUTOMATIC, and it must stay today's automatic. A recorded relay port
        // selects the relay wire before the pane is considered — the same
        // precedence `quorum_qw::lanes::claude_carrier` applies inside
        // `LaneOps::deliver`, which is what an un-flagged `qd send` reaches. A
        // `--wait` with no `--carrier` therefore blocks on the same wire the
        // message would have gone over anyway.
        None => {
            if session.relay_port.is_some() {
                Carrier::Relay
            } else {
                Carrier::Pty
            }
        }
    };

    if carrier == Carrier::Pty && !lane.is_pane() {
        return Refusal::refused(
            "carrier",
            format!(
                "\"{label}\" is on lane {lane} and has no pane to type into — --carrier {} needs \
                 a session hosted in a mux pane. Use --carrier {}, or omit --carrier and let qd \
                 select the wire.",
                Carrier::Pty.token(),
                Carrier::Relay.token()
            ),
        )
        .emit();
    }
    if carrier == Carrier::Relay && (req.raw || req.full) {
        return Refusal::refused(
            "carrier",
            "--raw / --full are pane-wire extraction modes (they select which transcript blocks \
             to print); the relay wire returns the reply text only. Drop them, or use --carrier pty."
                .to_string(),
        )
        .emit();
    }

    run_carrier(session, query, message, carrier, req)
}

/// Deliver over `carrier` — THE shared body, and the only place either wire is
/// reached from more than one verb.
///
/// Both arms are the deprecated verbs' own bodies rather than new ones. That is
/// requirement-driven and not merely tidy: the reply capture on each wire is
/// several hundred lines of five-armed outcome rendering apiece
/// (`send.rs`'s anchor loop and embedded-terminal await; `send_relay.rs`'s
/// long-poll with its 3-retry connection-drop budget), and a second copy of
/// either would be a second contract to keep in step.
fn run_carrier(
    session: &Session,
    query: &str,
    message: &str,
    carrier: Carrier,
    req: &Request,
) -> i32 {
    match carrier {
        Carrier::Pty => super::send::run_send_pty_resolved(
            session,
            message,
            req.wait,
            req.raw,
            req.full,
            &req.timeout,
        ),
        Carrier::Relay => super::send_relay::run_with_client(
            query,
            message,
            req.wait,
            super::send_relay::parse_timeout_or_default(&req.timeout),
            &CcRelay::new(),
        ),
    }
}

/// `refused{carrier}` for a target that is not running.
fn refuse_not_live(label: &str) -> i32 {
    Refusal::refused(
        "carrier",
        format!(
            "\"{label}\" is not live — a pinned wire (--carrier / --wait) delivers into a running \
             session's pane or relay, and reviving one to honour a flag is not what the flag says. \
             Send without --carrier/--wait to wake it, or run `qd resume {label}` first."
        ),
    )
    .emit()
}

/// `refused{wait-unsupported}` — the teaching refusal for a lane with no reply
/// channel.
///
/// It names the LANE (not the provider — a `claude-code` row is `claude-code/acp`
/// or `claude-code/mux-pane` and only one of them has the channel), says what the
/// lane DOES give back, and points at the two things that actually work. The
/// `qd wait` line is deliberately explicit that it reports the transition and not
/// the text: offering it without that sentence is how a caller ends up believing
/// a busy→idle exit 0 was a reply.
fn refuse_wait_unsupported(label: &str, lane: Lane) -> i32 {
    Refusal::refused(
        "wait-unsupported",
        format!(
            "\"{label}\" is on lane {lane}, which has no reply channel — a send there reports \
             acceptance and a turn id, never the reply body. Send it without --wait; then \
             `qd wait {label}` blocks until the session goes busy→idle (it reports the \
             TRANSITION, it does not print the reply), and `qd attach {label}` shows what it said."
        ),
    )
    .emit()
}

// ===========================================================================
// The deprecation notices
// ===========================================================================

/// ONE line, on stderr, naming `qd send` as the replacement — then the verb runs
/// unchanged.
///
/// **stderr, and only stderr, on purpose.** Every one of these verbs has callers
/// that read its stdout: `qd send:relay` prints a bare `message_id` that scripts
/// capture, `qd send:pty --wait` prints the reply body and is `qf call`'s
/// delivery primitive (`frame/src/delivery.rs`), and `qd send:http`'s exit 1 is
/// used by three cases in `tests/resolve_beyond_cap.rs` as a deliberately
/// non-destructive resolution oracle. A deprecation that moved a byte of stdout
/// or changed an exit code would break the callers it exists to give time to.
fn notice(verb: &str, replacement: &str) {
    eprintln!("qd {verb}: DEPRECATED — use `{replacement}`. This verb still works, unchanged.");
}

/// `send:pty` ⇒ the pane wire.
pub fn deprecated_pty() {
    notice(
        "send:pty",
        "qd send <session> <message> --carrier pty [--wait] [--raw|--full] [--timeout <secs>]",
    );
}

/// `send:relay` ⇒ the relay/daemon wire.
pub fn deprecated_relay() {
    notice(
        "send:relay",
        "qd send <session> <message> --carrier relay [--wait] [--timeout <secs>]",
    );
}

/// `send:http` ⇒ `qd send`, and NOTHING ELSE CHANGES — the reason being the whole
/// content of this function.
///
/// This verb has never delivered a message. Engine sessions are never
/// `provider = opencode`, so every invocation since it was ported takes the
/// "not an OpenCode session" branch and exits 1 with the success path parked. It
/// is therefore not a carrier that needs an alias; it is a refusal with a
/// resolution step in front of it, and three cases in
/// `tests/resolve_beyond_cap.rs` use exactly that shape as a resolution oracle
/// they can point at a real session without delivering anything.
///
/// Giving it a `--carrier` delegation would convert "always refuses" into
/// "actually delivers", which is a NEW destructive behaviour arriving under the
/// banner of a deprecation. So it gets the notice and keeps its body. There is no
/// [`Carrier`] variant for it, and the `--carrier` flag does not accept `http`.
pub fn deprecated_http() {
    notice("send:http", "qd send <session> <message>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use quorum_qw::lane::{Harness, Mode};

    /// The flag's accepted values and the enum are ONE list. `cli.rs` hands clap
    /// `["pty", "relay"]` and this parses them; a third value added to either
    /// without the other is the drift this pins.
    #[test]
    fn every_carrier_round_trips_through_its_flag_token() {
        for c in [Carrier::Pty, Carrier::Relay] {
            assert_eq!(Carrier::parse(c.token()), Some(c), "{c:?}");
        }
        // `http` is deliberately not a carrier — the verb never delivered
        // anything, so there is no wire to pin. See `deprecated_http`.
        assert_eq!(Carrier::parse("http"), None);
        assert_eq!(Carrier::parse(""), None);
    }

    /// The one lane with a reply channel, asked as a LANE and not as a provider
    /// name. `claude-code` alone is not the answer: the ACP lane's rows carry
    /// provider `claude-code` too, and they have no reply channel — which is the
    /// exact case a provider-string gate would have got wrong.
    #[test]
    fn only_the_claude_pane_lane_captures_a_reply() {
        assert!(quorum_qw::lane::CLAUDE_PANE.captures_reply());
        for lane in Lane::ALL {
            if lane == quorum_qw::lane::CLAUDE_PANE {
                continue;
            }
            assert!(
                !lane.captures_reply(),
                "{lane} must refuse --wait rather than make it a silent no-op"
            );
        }
        let acp = Lane::new(Harness::ClaudeCode, Mode::Acp).expect("claude-code/acp is a lane");
        assert!(
            !acp.captures_reply(),
            "a claude-code row on the ACP lane has no reply channel"
        );
    }
}
