//! `qd messages <session> [--json|--table] [--full] [--window <spec>]
//! [--host <h> | --all] [--archive]` — the per-SESSION read verb.
//!
//! `qd dispositions` publishes the transport's rows keyed by `correlation_id`,
//! which is the right key for auditing a delivery and the wrong one for the
//! question a person actually asks: *what has this session said and heard?* That
//! question is answered by the ENVELOPE — the only record carrying both ends
//! (`target`, `sender`) and a `body` — joined to the folded disposition of each
//! id. This verb is that join, filtered to the rows with one session on EITHER
//! end, in time order.
//!
//! ## What it reports, and what it CANNOT (read this before trusting a report)
//!
//! Every row is an envelope with the session on one END of it: ADDRESSED TO it
//! (matched on `target`) or SENT BY it (matched on [`Envelope::sender`]), both
//! sides in one timeline, each row carrying which side it is. That is a
//! conversation. The remaining boundaries are structural, not omissions here:
//!
//! - **The sent side starts where `sender` does.** `log.jsonl` gained `sender`
//!   after it gained everything else, and rows written before that carry `null`
//!   — unattributed, matchable from neither end. So the SENT half of a long
//!   history is truncated at the field's introduction, and no amount of reading
//!   fixes it: attribution comes from the recorded field or not at all, never
//!   from guessing by adjacency. A short sent side means "not recorded", which is
//!   not the same claim as "did not happen".
//! - **A `sender` is an id, a `target` is whatever was typed.** The two ends
//!   match on different terms and deliberately so: `QD_SESSION_ID` is injected
//!   verbatim at session create, so the sent side is an exact id comparison,
//!   while the received side keeps the whole [`Addresses`] ladder (name, id, id
//!   prefix, `@host`) because a human typed that end. Neither ladder is applied
//!   to the other end — an id-prefix tier over `sender` would let a two-char
//!   collision claim another session's authorship.
//! - **`qd send` only.** qd is the sole writer of the transport files, so a
//!   message that never passed through the send door is not in them. The relay
//!   server's reply path (`mcp__relay__reply`, including everything a `--wait`
//!   sender gets back) appends NO envelope, so replies are invisible here.
//! - **Orphan events are dropped.** An id with events but no envelope in scope
//!   has no `target` (R14.2 normalized it away), so it cannot be attributed to
//!   any session. [`dispatch::dispositions::query_joined`] drops those rows;
//!   they stay visible in `qd dispositions`, which owes no target.
//!
//! The human view says the first of these in its footer, because a table of
//! messages that silently means "half the messages" is worse than no table.
//!
//! ## Matching a session to the addresses it was written under
//!
//! The envelope stores the address AS THE CALLER TYPED IT (R9.4 — the raw
//! string, never a resolved id), so one session's history is spread across every
//! spelling anyone used: its name, its stable id, an id prefix copied out of
//! `qd ls`, each optionally `@host`-qualified. [`Addresses`] is that set, built
//! from the SAME resolver the acting verbs use, and [`target_matches`] is the
//! pure predicate over it (with its own unit tests below).
//!
//! An UNRESOLVED query is not an error while rows exist: a stopped-and-collected
//! session's messages outlive the session, and refusing to show them would make
//! the log unreadable exactly when it is most wanted. The query is then matched
//! literally. Only "no such session AND no rows" is the familiar exit-1
//! `No session matching …`.
//!
//! ## Surfaces
//!
//! JSONL for machines (one row per line, full body, the envelope's fields plus
//! the joined disposition), the aligned table for a human at a TTY, chosen by the
//! same driver rule `qd ls` uses (`--json`/`--table` override; agent or pipe ⇒
//! JSON). `--full` replaces the table with the untruncated bodies.
//!
//! ## Exit codes
//!
//! - `0` on success, including zero rows for a session that exists.
//! - `1` on a store IO error, an unset HOME, or an unknown session with no rows.
//! - `12` (`origin_send::EXIT_REFUSED`) on a malformed `--window`, an invalid
//!   `--host`, or a `--host`+`--all` conflict THAT REACHES THE BODY (a SYNC
//!   refusal, the shared `Refusal` shape). clap rejects the flag conflict itself
//!   at parse, which is its own exit 1 — 12 is the belt-and-suspenders arm.
//! - `141` on a broken downstream pipe (`| head`), never a panic.

use std::collections::BTreeSet;

use clap::ArgMatches;

use dispatch::dispositions::{
    local_host, query_joined, Envelope, EventKind, Scope, SummaryRecord, SummaryState,
};
use dispatch::effects::{Clock, Env, RealClock, RealEnv};
use dispatch::fmt::{relative_time, truncate_id};
use dispatch::join::JoinOpts;
use dispatch::model::Session;
use dispatch::paths::QdPaths;
use dispatch::resolve::{resolve_session, Resolution};
use dispatch::setup::style::Style;

use super::common;
// The scope + window resolvers are the SIBLING verb's, reused rather than
// re-derived: both verbs take the identical `--host/--all/--archive/--window`
// flags with identical meaning (including the path-traversal guard on `--host`
// and the `parse_expires` duration grammar), and two copies of that would be two
// things to keep in step. They are unit-tested over there.
use super::dispositions::{select_scope, window_lower_bound};

/// How wide a one-line body preview may be in the human table before it is
/// elided. Fixed rather than terminal-derived: the table has to render
/// identically under a test harness that has no terminal, and `--full` is the
/// escape hatch for anyone who wants the whole message.
const BODY_PREVIEW_MAX: usize = 72;

/// The short form of a `correlation_id` in the human views (ULIDs are 26 chars;
/// the leading 8 are already time-ordered and unique in any realistic log).
const ID_SHORT_LEN: usize = 8;

// ===========================================================================
// The pure core: which raw `target` strings belong to this session
// ===========================================================================

/// Every address one session answers to, lowercased, plus the host namespaces in
/// scope. Built by [`resolve_addresses`]; consumed by [`target_matches`].
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Addresses {
    /// Exact aliases: the query as typed, and each matched session's name,
    /// stable id, and provider session id.
    exact: BTreeSet<String>,
    /// The matched stable ids, for the id-PREFIX tier — `qd ls` prints shortest-
    /// unique id prefixes, so a send addressed by the prefix a person copied off
    /// that table is logged under the prefix, not the id.
    qd_ids: Vec<String>,
    /// The host namespaces whose rows count: always this host, plus whatever the
    /// caller's scope unioned in. `None` = accept ANY host qualifier (`--all`).
    hosts: Option<BTreeSet<String>>,
}

/// Does this envelope's RAW `target` address the session [`Addresses`] describes?
///
/// The address is split on the LAST `@` exactly as the send door splits it
/// (`send_unified::parse_address`) — neither names nor stable ids contain one.
/// The host half gates first: a `name@peer` row is a message to THAT host's
/// session, a different session than the local one of the same name, so it counts
/// only when the caller unioned that host in. A malformed `name@` (empty host)
/// never matches — it is not an address, and guessing which host it meant is the
/// one thing a report must not do.
pub(crate) fn target_matches(target: &str, a: &Addresses) -> bool {
    let (name, host) = match target.rsplit_once('@') {
        Some((n, h)) => (n, Some(h)),
        None => (target, None),
    };
    if let Some(h) = host {
        let h = h.to_lowercase();
        // An EMPTY host ("name@") is malformed, and is refused BEFORE the scope
        // check — otherwise `--all`, whose whole meaning is "every namespace",
        // would be the one scope that accepted an address naming none.
        if h.is_empty() {
            return false;
        }
        match &a.hosts {
            None => {} // --all: every namespace
            Some(hosts) if hosts.contains(&h) => {}
            Some(_) => return false,
        }
    }
    let name = name.to_lowercase();
    // An empty name half ("@host", or the empty address itself) is not an
    // address — the send door refuses one, so nothing was ever logged under it.
    // Checked BEFORE the alias lookup so an empty alias, however it got into the
    // set, can never act as a wildcard.
    if name.is_empty() {
        return false;
    }
    if a.exact.contains(&name) {
        return true;
    }
    // The id-prefix tier. Bounded at 2 chars — the same floor `idstore::prefix_map`
    // uses when it mints the prefixes `qd ls` displays — so a one-character target
    // can never sweep in a whole id space.
    name.len() >= 2 && a.qd_ids.iter().any(|id| id.starts_with(&name))
}

/// Which END of an envelope the queried session is on — the one thing a
/// two-sided report must say per row, or it is just a pile of messages.
///
/// Computed at selection from the two matches, never stored: the format doc's
/// §1 rows are normalized, and "is this mine, and which way" is a fact about the
/// QUERY, not about the envelope. Serializes lowercase (`sent`/`received`/
/// `self`) — a machine reading the JSONL splits on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Direction {
    /// The session AUTHORED it: its id is the envelope's `sender`.
    Sent,
    /// The session was ADDRESSED: one of its spellings is the envelope's `target`.
    Received,
    /// BOTH ends are this session. `qd send`'s self-send fence refuses this at
    /// the door, but only when `QD_SESSION_ID` RESOLVES through the idstore — an
    /// unresolvable id sails past the fence and lands here. Reported as its own
    /// value rather than collapsed onto a side, because picking one would state
    /// something the row does not say.
    #[serde(rename = "self")]
    Loopback,
}

impl Direction {
    /// The table glyph, from the queried session's point of view: out, in, or a
    /// loop back to itself.
    fn glyph(self) -> &'static str {
        match self {
            Direction::Sent => "→",
            Direction::Received => "←",
            Direction::Loopback => "↺",
        }
    }
}

/// Is this envelope's `sender` the session [`Addresses`] describes — i.e. did
/// that session AUTHOR this message?
///
/// Exact, lowercased, against the alias set ONLY. Three deliberate absences:
///
/// - **No id-prefix tier.** `target`'s tier exists because a person copies a
///   shortened id out of `qd ls` and types it; `sender` is never typed — qd
///   writes the `QD_SESSION_ID` injected at session create, always in full. A
///   prefix tier here would buy nothing and let a two-char collision assert
///   another session's authorship, which is the one error an attribution column
///   must not make.
/// - **No `@host` split.** A sender id is unqualified by construction. Where the
///   row came from is the container's business (a local file ⇒ this host, a
///   mirror ⇒ that host), and `scope` has already bounded which containers were
///   read.
/// - **No match on absence.** `None` — a human in a shell, a cron, or any row
///   predating the field — matches NOTHING. An unattributed row is not evidence
///   that the queried session sent it.
pub(crate) fn sender_matches(sender: Option<&str>, a: &Addresses) -> bool {
    match sender {
        Some(id) if !id.is_empty() => a.exact.contains(&id.to_lowercase()),
        _ => false,
    }
}

/// Build the [`Addresses`] for `query` under `scope`.
///
/// Resolution WIDENS, it never refuses. `One` contributes that session's
/// spellings; `Many` contributes all of them (an ambiguous name is logged under
/// one string for every session sharing it — the rows are genuinely
/// indistinguishable in the log, so refusing would withhold data without making
/// anything less ambiguous; the `target` column shows what was actually
/// addressed); `None` leaves just the literal query. The returned bool is whether
/// anything resolved — the caller needs it for the unknown-session exit.
fn resolve_addresses(
    query: &str,
    scope: &Scope,
    env: &dyn Env,
    sessions: &[Session],
) -> (Addresses, bool) {
    let mut exact = BTreeSet::new();
    let mut qd_ids = Vec::new();
    // The address as TYPED is always an alias: `qd send` logs the caller's raw
    // string, so a message addressed by a prefix or an id is findable only under
    // the spelling that was used. An EMPTY query contributes nothing (clap
    // requires the positional, but `qd messages ""` satisfies it) — see
    // `target_matches`, which refuses an empty name half from the other side.
    if !query.is_empty() {
        exact.insert(query.to_lowercase());
    }

    let matched: Vec<&Session> = match resolve_session(query, sessions) {
        Resolution::One(s) => vec![s],
        Resolution::Many(v) => v,
        Resolution::None => Vec::new(),
    };
    let resolved = !matched.is_empty();
    for s in matched {
        if let Some(n) = s.name.as_ref().filter(|n| !n.is_empty()) {
            exact.insert(n.to_lowercase());
        }
        if let Some(id) = s.qd_id.as_ref().filter(|i| !i.is_empty()) {
            exact.insert(id.to_lowercase());
            qd_ids.push(id.to_lowercase());
        }
        if !s.session_id.is_empty() {
            exact.insert(s.session_id.to_lowercase());
        }
    }

    // Host namespaces: this host always, plus the one the caller unioned in.
    // `--all` unioned EVERY peer, so it accepts any qualifier rather than
    // enumerating `remote/` a second time.
    let hosts = match scope {
        Scope::All => None,
        Scope::Local => Some(BTreeSet::from([local_host(env).to_lowercase()])),
        Scope::Host(h) => Some(BTreeSet::from([
            local_host(env).to_lowercase(),
            h.to_lowercase(),
        ])),
    };
    (
        Addresses {
            exact,
            qd_ids,
            hosts,
        },
        resolved,
    )
}

/// A summary row passes `--window` on its NULLABLE `authored_at`. Every row here
/// is envelope-rooted so the timeline is in fact always present, but the summary
/// carries it as an `Option` and inventing a value to compare would be a lie —
/// an absent timeline is kept, exactly as the sibling verb keeps its orphans.
fn passes_window(authored_at: Option<i64>, lower_bound: Option<i64>) -> bool {
    match (lower_bound, authored_at) {
        (None, _) => true,
        (Some(_), None) => true,
        (Some(lb), Some(a)) => a >= lb,
    }
}

// ===========================================================================
// The row, and its two renderings
// ===========================================================================

/// One reported message: the envelope's own fields, the computed `direction`,
/// then how its delivery went. Field order IS the wire order (serde emits in
/// declaration order) — envelope first, disposition second, `body` last because
/// it is the long one and a human eyeballing raw JSONL wants the keys before the
/// prose.
///
/// `sender` rides beside `target` (both are envelope ends, and a consumer wants
/// them adjacent); `direction` follows them because it is DERIVED from exactly
/// those two against the query. Attaching a computed column at emission is the
/// format doc's own allowance — a view concern, never storage — which is why
/// `direction` appears here and in no `.jsonl` file.
#[derive(serde::Serialize)]
struct Row<'a> {
    v: u32,
    correlation_id: &'a str,
    authored_at: i64,
    expires_at: i64,
    target: &'a str,
    origin: &'a str,
    sender: Option<&'a str>,
    direction: Direction,
    state: SummaryState,
    attempts: u32,
    last_event: Option<EventKind>,
    last_attempt_at: Option<i64>,
    first_delivered_at: Option<i64>,
    body: &'a str,
}

impl<'a> Row<'a> {
    fn new(e: &'a Envelope, s: &'a SummaryRecord, direction: Direction) -> Row<'a> {
        Row {
            v: 1,
            correlation_id: &e.correlation_id,
            authored_at: e.authored_at,
            expires_at: e.expires_at,
            target: &e.target,
            origin: &e.origin,
            sender: e.sender.as_deref(),
            direction,
            state: s.state,
            attempts: s.attempts,
            last_event: s.last_event,
            last_attempt_at: s.last_attempt_at,
            first_delivered_at: s.first_delivered_at,
            body: &e.body,
        }
    }
}

/// The state word as it appears in the human views.
fn state_word(state: SummaryState) -> &'static str {
    match state {
        SummaryState::Pending => "pending",
        SummaryState::Delivered => "delivered",
        SummaryState::Failed => "failed",
        SummaryState::Expired => "expired",
    }
}

/// Collapse a body to ONE line for the table: every control character (a newline
/// most of all) becomes a space, runs collapse, and an over-long result is cut on
/// a CHAR boundary with an ellipsis. Never emits a raw control byte into a
/// terminal — a body is opaque prose qd never parsed, so it can contain anything.
fn preview(body: &str) -> String {
    // COLLAPSE FIRST, ELIDE SECOND. Deciding to elide mid-push — the obvious way
    // to write this — made a body that merely ENDED in whitespace render exactly
    // like a truncated one: `"x"*72 + "   "` and `"x"*73` both came out as 72
    // chars plus an ellipsis, so the one column a reader scans could not be
    // trusted to mean "there is more". Elision is a claim about content, and the
    // trailing whitespace has to be gone before the claim can be made.
    let mut out = String::with_capacity(body.len().min(BODY_PREVIEW_MAX + 2));
    let mut n = 0usize;
    let mut prev_space = false;
    for ch in body.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if ch == ' ' {
            if prev_space || out.is_empty() {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        out.push(ch);
        n += 1;
        // ONE char past the budget settles it — that char is either real content
        // (⇒ elide) or the whitespace the trim below removes (⇒ do not). Nothing
        // further can change either the prefix or the verdict, so stop reading.
        if n > BODY_PREVIEW_MAX + 1 {
            break;
        }
    }
    while out.ends_with(' ') {
        out.pop();
        n -= 1;
    }
    if n > BODY_PREVIEW_MAX {
        let mut cut: String = out.chars().take(BODY_PREVIEW_MAX).collect();
        cut.push('…');
        return cut;
    }
    out
}

/// The `--full` counterpart to [`preview`]: keep the body's own line structure
/// (`\n`) and its tabs, neutralize every OTHER control character to a space.
///
/// A body is prose from whoever could reach the send door, and it is printed to a
/// terminal that executes what it is handed. `preview` says so in its own doc and
/// sanitizes; `--full` used to `push_str` the body verbatim, which undid that
/// defense three functions later — a peer could plant `ESC [ … m`, an OSC title/
/// clipboard sequence, or a bare BEL, and it fired the moment an operator read
/// their messages. The escapes went out even when qd had suppressed its OWN color
/// for a pipe, which is the giveaway that the body was never being treated as the
/// untrusted data it is.
///
/// Substituting a space (rather than dropping the byte) is deliberate and matches
/// [`preview`]: the residue — `[31m` where an SGR sequence was — stays visible, so
/// a reader can see that something was removed instead of silently reading a body
/// that is missing bytes.
fn sanitize_block(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\t' => c,
            c if c.is_control() => ' ',
            c => c,
        })
        .collect()
}

/// The aligned human table: when, which way, how it went, which id, and the
/// message. Widths from visible length; the same two-space column gap `qd ls`
/// uses.
///
/// `Dir` sits second, right after `When`, because in a two-sided transcript it
/// is what turns a list into a conversation — a reader scans the arrows to
/// follow the exchange and only then reads across. It is one glyph wide, so it
/// costs the columns after it three characters and nothing else.
fn render_table(rows: &[(Envelope, SummaryRecord, Direction)], now_ms: i64, st: Style) -> String {
    let headers = ["When", "Dir", "State", "Id", "Message"];
    let cells: Vec<[String; 5]> = rows
        .iter()
        .map(|(e, s, d)| {
            [
                relative_time(e.authored_at, now_ms),
                d.glyph().to_string(),
                state_word(s.state).to_string(),
                truncate_id(&e.correlation_id, ID_SHORT_LEN),
                preview(&e.body),
            ]
        })
        .collect();

    // Column widths over the header AND every cell — the last column never pads
    // (nothing follows it).
    let mut w = [0usize; 5];
    for (i, h) in headers.iter().enumerate() {
        w[i] = h.chars().count();
    }
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }

    let mut out = String::new();
    let head: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| pad(h, w[i], i == headers.len() - 1))
        .collect();
    out.push_str(&st.dim(head.join("  ").trim_end()));
    out.push('\n');
    for (row, (_, s, _)) in cells.iter().zip(rows.iter()) {
        let when = pad(&row[0], w[0], false);
        let dir = pad(&row[1], w[1], false);
        let state = pad(&row[2], w[2], false);
        let state = match s.state {
            // The two states that mean the prose did not land are the whole
            // reason to scan this column, so they are the only colored cells.
            SummaryState::Failed | SummaryState::Expired => {
                let colored = st.bold_red(state.trim_end());
                format!(
                    "{colored}{}",
                    " ".repeat(state.len() - state.trim_end().len())
                )
            }
            _ => state,
        };
        let id = st.dim(&pad(&row[3], w[3], false));
        out.push_str(&format!("{when}  {dir}  {state}  {id}  {}\n", row[4]));
    }
    out
}

/// Pad `s` to `width` visible chars (never truncates — widths were measured from
/// these same strings). The final column pads to nothing.
fn pad(s: &str, width: usize, last: bool) -> String {
    if last {
        return s.to_string();
    }
    let n = s.chars().count();
    format!("{s}{}", " ".repeat(width.saturating_sub(n)))
}

/// The table's footer sentence: how many, split by side, with the glyph legend.
///
/// Only the sides actually PRESENT are named. A session that never sent anything
/// reads "3 messages to/from \"alpha\" — 3 received (←)", not a line with a
/// "0 sent" in it: a zero on a side is the ordinary case, and spelling it out
/// every time trains a reader to skip the one line that says what was counted.
/// `self` appears only when it happened, which is nearly never (see
/// [`Direction::Loopback`]).
fn footer_line(rows: &[(Envelope, SummaryRecord, Direction)], query: &str) -> String {
    let count = |want: Direction| rows.iter().filter(|(_, _, d)| *d == want).count();
    let parts: Vec<String> = [
        (Direction::Sent, "sent"),
        (Direction::Received, "received"),
        (Direction::Loopback, "self"),
    ]
    .iter()
    .filter_map(|(dir, word)| match count(*dir) {
        0 => None,
        n => Some(format!("{n} {word} ({})", dir.glyph())),
    })
    .collect();

    format!(
        "{} message{} to/from \"{query}\" — {}.",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        parts.join(", "),
    )
}

/// `--full`: one block per message, the body in full — its own newlines and tabs
/// kept, every other control character neutralized by [`sanitize_block`]. "Full"
/// is a claim about CONTENT, never a licence to hand a terminal escape sequences
/// a peer wrote. The header line carries what
/// the table's columns would have said plus BOTH raw ends of the envelope,
/// because at this verbosity the exact addresses a message travelled between are
/// part of the record.
///
/// The ends are rendered as `sender → target`, which states the direction
/// literally instead of leaning on the table's from-the-queried-session glyph:
/// at full verbosity the reader is looking at one message, not scanning a
/// column, and `a1b2c3d4 → alpha@brano` needs no legend. An unattributed sender
/// prints as `—` — the same honest absence the `null` in the row is.
fn render_full(rows: &[(Envelope, SummaryRecord, Direction)], now_ms: i64, st: Style) -> String {
    let mut out = String::new();
    for (e, s, _) in rows {
        // The id and BOTH ends are logged strings — the id and target from the
        // sender's argv (`--correlation-id`, the raw address), the sender from
        // its environment — so the header is sanitized on the same terms as the
        // body, not just the prose.
        let head = sanitize_block(&format!(
            "── {} · {} · {} · {} → {}",
            e.correlation_id,
            state_word(s.state),
            relative_time(e.authored_at, now_ms),
            e.sender.as_deref().unwrap_or("—"),
            e.target,
        ))
        .replace('\n', " ");
        out.push_str(&st.dim(&head));
        out.push('\n');
        let body = sanitize_block(&e.body);
        out.push_str(&body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

// ===========================================================================
// The verb
// ===========================================================================

pub fn run(m: &ArgMatches) -> i32 {
    let env = RealEnv;
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        eprintln!("qd messages: HOME is not set");
        return 1;
    };
    let paths = QdPaths::from_home_env(std::path::Path::new(&home), &env);

    let scope = match select_scope(m) {
        Ok(s) => s,
        Err(r) => return r.emit(),
    };
    let archive = m.get_flag("archive");
    let now_ms = RealClock.now_ms();
    let lower_bound = match window_lower_bound(now_ms, m) {
        Ok(lb) => lb,
        Err(r) => return r.emit(),
    };
    let query = m.get_one::<String>("session").cloned().unwrap_or_default();

    // The gather is the SAME uncapped, tombstone-inclusive view `info` resolves
    // against: a stopped session still has a history, and capping the list could
    // silently turn a known session into an unknown one.
    let sessions = match common::all_sessions(JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: false,
        limit: None,
    }) {
        Ok(s) => s,
        // The gather prints its own failure and chooses the exit code.
        Err(code) => return code,
    };
    let (addresses, resolved) = resolve_addresses(&query, &scope, &env, &sessions);

    let joined = match query_joined(&paths, &scope, archive, now_ms) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("qd messages: failed reading disposition store: {e}");
            return 1;
        }
    };
    // BOTH ends, one pass. A row is reported when the session is on either end of
    // it, and WHICH end is decided here — the only place that knows both the
    // envelope and the query. A row matching neither is dropped; a row matching
    // both is `self` (see [`Direction::Loopback`]), never silently picked a side
    // for.
    let mut rows: Vec<(Envelope, SummaryRecord, Direction)> = joined
        .into_iter()
        .filter(|(_, s)| passes_window(s.authored_at, lower_bound))
        .filter_map(|(e, s)| {
            let direction = match (
                sender_matches(e.sender.as_deref(), &addresses),
                target_matches(&e.target, &addresses),
            ) {
                (true, true) => Direction::Loopback,
                (true, false) => Direction::Sent,
                (false, true) => Direction::Received,
                (false, false) => return None,
            };
            Some((e, s, direction))
        })
        .collect();
    // Time order — the report is a conversation, not a file dump. `authored_at`
    // is the ORIGIN timeline (N10), so a peer's rows interleave with local ones
    // by when they were written rather than by which file they were read from.
    // The id breaks ties: it is a ULID, so equal-millisecond rows still land in
    // mint order rather than an arbitrary one.
    rows.sort_by(|(a, _, _), (b, _, _)| {
        a.authored_at
            .cmp(&b.authored_at)
            .then_with(|| a.correlation_id.cmp(&b.correlation_id))
    });

    // An unknown session with nothing logged is the familiar miss — the same
    // message every other verb gives, so a typo reads as a typo. An unknown
    // session WITH rows is not a miss at all: it is a session that has since been
    // stopped and collected, and its history is exactly what was asked for.
    if !resolved && rows.is_empty() {
        eprintln!("No session matching \"{query}\"");
        return 1;
    }

    let emit_json = resolve_emit_json(
        m.get_flag("json"),
        m.get_flag("table"),
        m.get_flag("full"),
        &env,
    );
    let payload = if emit_json {
        let mut buf = String::new();
        for (e, s, d) in &rows {
            buf.push_str(&serde_json::to_string(&Row::new(e, s, *d)).unwrap_or_default());
            buf.push('\n');
        }
        buf
    } else {
        let st = Style::detect(
            crate::tty::stdout_is_tty(),
            env.var("NO_COLOR").as_deref(),
            env.var("TERM").as_deref(),
        );
        if rows.is_empty() {
            format!("No messages logged for \"{query}\".\n")
        } else if m.get_flag("full") {
            render_full(&rows, now_ms, st)
        } else {
            let mut buf = render_table(&rows, now_ms, st);
            // The honest footer: the total, then the split that produced it, then
            // the legend for the glyph column. The split is the load-bearing part
            // — a two-sided table invites the reading "this is the whole
            // conversation", and the one way it can quietly not be is a sent side
            // that is short because `sender` postdates the rows (module doc), so
            // the count of each side is stated rather than left to be inferred
            // from scanning arrows.
            buf.push_str(&st.dim(&format!(
                "\n{}\n",
                footer_line(&rows, &query)
            )));
            buf
        }
    };
    emit_or_pipe_exit(&payload);
    0
}

/// JSONL vs the human table, by the rule `qd ls` established (`ls::resolve_emit_json`):
/// `--json` wins outright, `--table` forces the human surface even for an agent
/// caller, and otherwise I/O follows who drives — an agent or a pipe gets the
/// machine surface, a human at a TTY gets the table.
///
/// `--full` ALSO forces the human surface, which is where this departs from `ls`
/// and its inert `--short`. The two look like the same case and are not: `--short`
/// narrows a surface JSON also has, so an agent's bare `--short` degrades to a
/// still-useful document. `--full` has NO meaning under JSON — that surface never
/// elided a body in the first place — so honoring the auto-default would answer a
/// question nobody asked and silently drop the only flag the caller typed. A flag
/// that exists solely to change how the human view prints is a request for the
/// human view. An explicit `--json --full` still yields JSON: an explicit selector
/// always beats an implied one.
fn resolve_emit_json(json_flag: bool, table_flag: bool, full_flag: bool, env: &dyn Env) -> bool {
    use crate::driver::{resolve_driver_real, Driver, DriverOverride};
    if json_flag {
        return true;
    }
    let over = if table_flag || full_flag {
        DriverOverride::Interactive
    } else {
        DriverOverride::None
    };
    matches!(resolve_driver_real(over, env), Driver::Agent)
}

/// Emit the fully-built payload, exiting CLEANLY (141) on a broken pipe rather
/// than panicking with a partial document — the `ls.rs` / `dispositions.rs`
/// discipline (engine-hardening item 20).
fn emit_or_pipe_exit(payload: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let res = out.write_all(payload.as_bytes()).and_then(|()| out.flush());
    if let Err(e) = res {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(141);
        }
        eprintln!("qd messages: failed writing output: {e}");
        std::process::exit(141);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch::effects::MapEnv;
    use dispatch::model::{SessionBranch, SessionStatus};

    /// A minimal joined row, the `resolve.rs` test fixture's shape.
    fn base(session_id: &str, name: Option<&str>, qd_id: Option<&str>) -> Session {
        Session {
            name: name.map(str::to_string),
            user_named: None,
            session_id: session_id.to_string(),
            code: None,
            qd_id: qd_id.map(str::to_string),
            pid: None,
            status: SessionStatus::Idle,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "claude-code".to_string(),
            entrypoint: None,
            lineage: None,
            hosting: None,
            which_branch: SessionBranch::LiveRegistry,
        }
    }

    /// Addresses for a session known by `names`, on the local host only.
    fn addrs(names: &[&str], ids: &[&str]) -> Addresses {
        Addresses {
            exact: names.iter().map(|n| n.to_lowercase()).collect(),
            qd_ids: ids.iter().map(|i| i.to_lowercase()).collect(),
            hosts: Some(BTreeSet::from(["local".to_string()])),
        }
    }

    fn env_with(kv: &[(&str, &str)]) -> MapEnv {
        let mut e = MapEnv::default();
        for (k, v) in kv {
            e.vars.insert((*k).to_string(), (*v).to_string());
        }
        e
    }

    fn envelope(target: &str, authored: i64, body: &str) -> Envelope {
        Envelope {
            sender: None,
            ..envelope_from("ab3kx9mq", target, authored, body)
        }
    }

    /// An envelope with a recorded `sender` — the agent-authored case.
    fn envelope_from(sender: &str, target: &str, authored: i64, body: &str) -> Envelope {
        Envelope {
            v: 1,
            correlation_id: format!("ID{authored}"),
            authored_at: authored,
            expires_at: authored + 1000,
            target: target.to_string(),
            origin: "local".to_string(),
            sender: Some(sender.to_string()),
            body: body.to_string(),
        }
    }

    fn summary(state: SummaryState, authored: i64) -> SummaryRecord {
        SummaryRecord {
            v: 1,
            correlation_id: format!("ID{authored}"),
            state,
            attempts: 1,
            last_event: None,
            last_attempt_at: None,
            first_delivered_at: None,
            expires_at: Some(authored + 1000),
            authored_at: Some(authored),
            origin: Some("local".to_string()),
        }
    }

    // ---- target_matches: the address tiers -----------------------------------

    /// A bare target matches an exact alias, case-insensitively (the log stores
    /// the caller's raw spelling, which may differ in case from the row).
    #[test]
    fn bare_target_matches_exact_alias_any_case() {
        let a = addrs(&["alpha"], &[]);
        assert!(target_matches("alpha", &a));
        assert!(target_matches("ALPHA", &a));
        assert!(!target_matches("alphab", &a), "not a prefix tier for names");
        assert!(!target_matches("alph", &a));
    }

    /// The id-PREFIX tier: `qd ls` prints shortest-unique id prefixes, so a send
    /// addressed by the prefix a person copied is logged under the PREFIX.
    #[test]
    fn id_prefix_tier_matches_from_two_chars() {
        let a = addrs(&["alpha"], &["a1b2c3d4"]);
        assert!(target_matches("a1b2c3d4", &a), "the whole id");
        assert!(target_matches("a1b2", &a), "a copied prefix");
        assert!(target_matches("a1", &a), "the two-char floor");
        assert!(
            !target_matches("a", &a),
            "one char must never sweep in an id space"
        );
        assert!(!target_matches("b1", &a), "a prefix of nothing we own");
    }

    /// The host half gates first: `name@peer` is THAT host's session, a different
    /// session than the local one of the same name.
    #[test]
    fn host_qualifier_gates_on_scope() {
        let local_only = addrs(&["alpha"], &[]);
        assert!(target_matches("alpha@local", &local_only), "this host");
        assert!(
            !target_matches("alpha@peerbox", &local_only),
            "a peer's session is not this one"
        );

        let with_peer = Addresses {
            hosts: Some(BTreeSet::from(["local".to_string(), "peerbox".to_string()])),
            ..addrs(&["alpha"], &[])
        };
        assert!(target_matches("alpha@peerbox", &with_peer));
        assert!(!target_matches("alpha@other", &with_peer));

        let any_host = Addresses {
            hosts: None, // --all
            ..addrs(&["alpha"], &[])
        };
        assert!(target_matches("alpha@anything", &any_host));
        assert!(
            target_matches("alpha", &any_host),
            "unqualified still counts"
        );
    }

    /// A malformed or hostile address never matches by accident.
    #[test]
    fn malformed_addresses_never_match() {
        let a = addrs(&["alpha", ""], &[]);
        assert!(
            !target_matches("alpha@", &a),
            "empty host is not an address"
        );
        assert!(!target_matches("@local", &a), "empty name half");
        assert!(!target_matches("", &a), "an empty target matches nothing");
        // …and under EVERY scope, including `--all`, whose "every namespace" arm
        // used to skip the host check and so accept an address naming none.
        let any_host = Addresses {
            hosts: None,
            ..addrs(&["alpha"], &[])
        };
        assert!(
            !target_matches("alpha@", &any_host),
            "--all must not be the one scope that accepts a malformed address"
        );

        // Split is on the LAST '@' (names and ids contain none), so a name that
        // itself contains one is compared whole.
        let odd = addrs(&["we@ird"], &[]);
        assert!(target_matches("we@ird@local", &odd));
        assert!(!target_matches("we@ird@peer", &odd));
    }

    // ---- sender_matches: the authored end ------------------------------------

    /// The sent side is an EXACT id comparison, case-insensitive because the
    /// received side is and a reader should not have to hold two rules.
    #[test]
    fn sender_matches_an_exact_alias_any_case() {
        let a = addrs(&["alpha", "a1b2c3d4"], &["a1b2c3d4"]);
        assert!(sender_matches(Some("a1b2c3d4"), &a));
        assert!(sender_matches(Some("A1B2C3D4"), &a));
        assert!(!sender_matches(Some("b9b9b9b9"), &a), "another session");
    }

    /// NO id-prefix tier on the sent side — the asymmetry with `target_matches`
    /// is the point. `sender` is written by qd from the injected
    /// `QD_SESSION_ID`, never typed, so a prefix would only ever be a collision,
    /// and a collision here claims another session AUTHORED something.
    #[test]
    fn sender_has_no_prefix_tier_though_target_does() {
        let a = addrs(&["alpha"], &["a1b2c3d4"]);
        assert!(
            target_matches("a1b2", &a),
            "the received side still honors a typed prefix"
        );
        assert!(
            !sender_matches(Some("a1b2"), &a),
            "the sent side must not accept a prefix as authorship"
        );
    }

    /// An UNATTRIBUTED envelope belongs to no one's sent side: absent, empty, or
    /// predating the field, it is never evidence that this session authored it.
    #[test]
    fn sender_absence_never_matches() {
        let a = addrs(&["alpha", ""], &["a1b2c3d4"]);
        assert!(!sender_matches(None, &a), "no sender recorded");
        assert!(
            !sender_matches(Some(""), &a),
            "an empty sender must not act as a wildcard even with an empty alias in the set"
        );
    }

    /// A sender id carries no `@host` and is not split on one: an id containing
    /// an `@` is compared whole, so the received side's host grammar cannot leak
    /// across and make `a1b2c3d4@peer` match the session `a1b2c3d4`.
    #[test]
    fn sender_is_never_split_on_a_host_qualifier() {
        let a = addrs(&["alpha"], &["a1b2c3d4"]);
        assert!(!sender_matches(Some("a1b2c3d4@local"), &a));
        assert!(
            target_matches("a1b2c3d4@local", &a),
            "the received side DOES split — the two ends read differently on purpose"
        );
    }

    // ---- resolve_addresses ---------------------------------------------------

    /// An UNRESOLVED query still yields the literal alias — a stopped-and-
    /// collected session's messages outlive the session, and the raw string is
    /// how they were logged.
    #[test]
    fn unresolved_query_keeps_the_literal_alias() {
        let env = env_with(&[]);
        let (a, resolved) = resolve_addresses("Ghost", &Scope::Local, &env, &[]);
        assert!(!resolved, "nothing in the gather matched");
        assert!(a.exact.contains("ghost"), "{:?}", a.exact);
        assert!(target_matches("ghost", &a));
    }

    /// A resolved session contributes every spelling it can be addressed by.
    #[test]
    fn resolved_session_contributes_all_its_spellings() {
        let env = env_with(&[]);
        let sessions = vec![base("uuid-1111", Some("alpha"), Some("a1b2c3d4"))];
        let (a, resolved) = resolve_addresses("alpha", &Scope::Local, &env, &sessions);
        assert!(resolved);
        for spelling in ["alpha", "a1b2c3d4", "uuid-1111"] {
            assert!(
                target_matches(spelling, &a),
                "{spelling} should match {a:?}"
            );
        }
        assert!(target_matches("a1b2", &a), "and its id prefix");
    }

    /// An AMBIGUOUS name widens rather than refuses: both sessions were logged
    /// under the same string, so the rows are indistinguishable in the log and
    /// refusing would withhold data without resolving anything.
    #[test]
    fn ambiguous_query_widens_to_every_match() {
        let env = env_with(&[]);
        let sessions = vec![
            base("uuid-1", Some("dup"), Some("aaaaaaaa")),
            base("uuid-2", Some("dup"), Some("bbbbbbbb")),
        ];
        let (a, resolved) = resolve_addresses("dup", &Scope::Local, &env, &sessions);
        assert!(resolved);
        assert!(target_matches("aaaaaaaa", &a));
        assert!(target_matches("bbbbbbbb", &a));
    }

    /// The host set follows the SCOPE, and honors QD_HOST for this host's id.
    #[test]
    fn host_set_follows_scope_and_qd_host() {
        let env = env_with(&[("QD_HOST", "Brano")]);
        let (local, _) = resolve_addresses("alpha", &Scope::Local, &env, &[]);
        assert_eq!(local.hosts, Some(BTreeSet::from(["brano".to_string()])));

        let (host, _) = resolve_addresses("alpha", &Scope::Host("Peer".into()), &env, &[]);
        assert_eq!(
            host.hosts,
            Some(BTreeSet::from(["brano".to_string(), "peer".to_string()]))
        );

        let (all, _) = resolve_addresses("alpha", &Scope::All, &env, &[]);
        assert_eq!(all.hosts, None, "--all accepts every namespace");
    }

    // ---- window --------------------------------------------------------------

    #[test]
    fn window_bound_is_inclusive_and_keeps_null_timelines() {
        assert!(
            passes_window(Some(1000), Some(1000)),
            "boundary is inclusive"
        );
        assert!(passes_window(Some(1001), Some(1000)));
        assert!(!passes_window(Some(999), Some(1000)));
        assert!(passes_window(None, None), "no bound keeps everything");
        assert!(
            passes_window(None, Some(1000)),
            "an absent timeline is never excluded by a bound it cannot be measured against"
        );
    }

    // ---- preview -------------------------------------------------------------

    /// A body is opaque prose qd never parsed: newlines and control bytes must
    /// never reach the terminal, and runs collapse to one space.
    #[test]
    fn preview_flattens_control_bytes_and_collapses_runs() {
        assert_eq!(preview("hello"), "hello");
        assert_eq!(preview("two\nlines"), "two lines");
        assert_eq!(preview("a\t\tb"), "a b");
        assert_eq!(preview("  lead and trail  "), "lead and trail");
        assert_eq!(preview("\x1b[31mred\x1b[0m"), "[31mred [0m", "no raw ESC");
        assert_eq!(preview(""), "");
        assert_eq!(preview("   "), "");
    }

    /// Elision is by CHARACTER, at the budget, with the ellipsis inside it.
    #[test]
    fn preview_elides_at_the_budget_on_char_boundaries() {
        let long = "x".repeat(BODY_PREVIEW_MAX + 10);
        let out = preview(&long);
        assert_eq!(out.chars().count(), BODY_PREVIEW_MAX + 1, "{out}");
        assert!(out.ends_with('…'));

        let exact = "y".repeat(BODY_PREVIEW_MAX);
        assert_eq!(preview(&exact), exact, "exactly at the budget is untouched");

        // The regression: a body at the budget that merely ENDS in whitespace is
        // complete, and must not claim elision. It read identically to a genuinely
        // truncated body before the collapse-then-elide split.
        let padded = format!("{exact}   ");
        assert_eq!(
            preview(&padded),
            exact,
            "trailing space is not 'more content'"
        );
        assert!(!preview(&padded).ends_with('…'));
        // One real char past the budget IS elided — including past a space, which
        // an early break would have swallowed.
        let more = format!("{exact} z");
        assert!(more.len() > exact.len());
        assert!(
            preview(&more).ends_with('…'),
            "real content past the budget elides"
        );
        assert_eq!(preview(&more).chars().count(), BODY_PREVIEW_MAX + 1);

        // Multibyte must not panic and must not split a char.
        let emoji = "🌍".repeat(BODY_PREVIEW_MAX + 5);
        let out = preview(&emoji);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), BODY_PREVIEW_MAX + 1);
    }

    // ---- table + full rendering ---------------------------------------------

    /// The table is one line per message plus a header, widths measured in
    /// visible characters, and a multi-line body never breaks the row.
    #[test]
    fn table_is_one_line_per_message() {
        let rows = vec![
            (
                envelope("alpha", 1000, "first\nwith a newline"),
                summary(SummaryState::Delivered, 1000),
                Direction::Received,
            ),
            (
                envelope_from("a1b2c3d4", "beta", 2000, "second"),
                summary(SummaryState::Failed, 2000),
                Direction::Sent,
            ),
        ];
        let out = render_table(&rows, 5000, Style::PLAIN);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "header + 2 rows: {out}");
        let squashed: String = lines[0].split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(squashed, "When Dir State Id Message", "{}", lines[0]);
        assert!(lines[1].contains("first with a newline"), "{}", lines[1]);
        assert!(lines[1].contains("delivered"));
        assert!(lines[2].contains("failed"));
        // The two sides carry their own glyph — this is the column that makes
        // the table a transcript rather than a list.
        assert!(lines[1].contains('←'), "the received row: {}", lines[1]);
        assert!(lines[2].contains('→'), "the sent row: {}", lines[2]);
        // The State column is padded to the widest word, so both rows' Id column
        // starts at the same offset.
        let id_col = |l: &str| l.find("ID").expect("id cell");
        assert_eq!(id_col(lines[1]), id_col(lines[2]), "{out}");
    }

    /// PLAIN style emits no escapes at all — what a pipe and every test sees.
    #[test]
    fn plain_style_emits_no_escapes() {
        let rows = vec![(
            envelope("alpha", 1000, "body"),
            summary(SummaryState::Expired, 1000),
            Direction::Received,
        )];
        let out = render_table(&rows, 5000, Style::PLAIN);
        assert!(!out.contains('\x1b'), "{out:?}");
    }

    /// `--full` prints the body VERBATIM, newlines and all.
    #[test]
    fn full_prints_the_body_verbatim() {
        let rows = vec![(
            envelope_from("a1b2c3d4", "alpha", 1000, "line one\nline two"),
            summary(SummaryState::Delivered, 1000),
            Direction::Received,
        )];
        let out = render_full(&rows, 5000, Style::PLAIN);
        assert!(out.contains("line one\nline two\n"), "{out:?}");
        assert!(out.contains("ID1000"), "the full id, not the short one");
        assert!(out.contains("→ alpha"), "the raw target");
    }

    // ---- the JSON row --------------------------------------------------------

    /// The wire row is the envelope's fields THEN the disposition's, with `body`
    /// last. Pinned because a consumer reads these keys.
    #[test]
    fn json_row_carries_both_halves_of_the_join() {
        let e = envelope("alpha", 1000, "hi");
        let s = summary(SummaryState::Delivered, 1000);
        let line = serde_json::to_string(&Row::new(&e, &s, Direction::Received)).unwrap();
        for key in [
            "\"v\":1",
            "\"correlation_id\":\"ID1000\"",
            "\"authored_at\":1000",
            "\"expires_at\":2000",
            "\"target\":\"alpha\"",
            "\"origin\":\"local\"",
            "\"sender\":null",
            "\"direction\":\"received\"",
            "\"state\":\"delivered\"",
            "\"attempts\":1",
            "\"last_event\":null",
            "\"body\":\"hi\"",
        ] {
            assert!(line.contains(key), "{key} missing from {line}");
        }
        assert!(line.ends_with("\"body\":\"hi\"}"), "body is last: {line}");
    }

    /// `direction` is the computed column, and it serializes as a WORD, not the
    /// table's glyph: the JSONL is what a machine splits on, and `→` is a
    /// rendering choice this surface has no business exporting.
    #[test]
    fn json_row_direction_is_a_machine_word_not_a_glyph() {
        let s0 = summary(SummaryState::Delivered, 1000);
        for (dir, want) in [
            (Direction::Sent, "\"direction\":\"sent\""),
            (Direction::Received, "\"direction\":\"received\""),
            (Direction::Loopback, "\"direction\":\"self\""),
        ] {
            let e = envelope_from("a1b2c3d4", "alpha", 1000, "hi");
            let line = serde_json::to_string(&Row::new(&e, &s0, dir)).unwrap();
            assert!(line.contains(want), "{want} missing from {line}");
            assert!(!line.contains('→'), "no glyph on the wire: {line}");
            assert!(line.contains("\"sender\":\"a1b2c3d4\""), "{line}");
        }
    }

    // ---- the footer ----------------------------------------------------------

    /// The footer names only the sides that are PRESENT, and pluralizes on the
    /// total. A zero side is the ordinary case and saying "0 sent" every time
    /// trains a reader to skip the one line that reports what was counted.
    #[test]
    fn footer_names_only_the_sides_present() {
        let recv = |t: i64| {
            (
                envelope("alpha", t, "b"),
                summary(SummaryState::Delivered, t),
                Direction::Received,
            )
        };
        let sent = |t: i64| {
            (
                envelope_from("a1b2c3d4", "beta", t, "b"),
                summary(SummaryState::Delivered, t),
                Direction::Sent,
            )
        };

        assert_eq!(
            footer_line(&[recv(1), recv(2), sent(3)], "alpha"),
            "3 messages to/from \"alpha\" — 1 sent (→), 2 received (←)."
        );
        assert_eq!(
            footer_line(&[recv(1)], "alpha"),
            "1 message to/from \"alpha\" — 1 received (←).",
            "singular on the total, and no 0-sent clause"
        );
        assert_eq!(
            footer_line(&[sent(1), sent(2)], "alpha"),
            "2 messages to/from \"alpha\" — 2 sent (→).",
            "a session that only sent gets no 0-received clause"
        );
    }

    /// `self` is reported when it happens and is silent otherwise — it is nearly
    /// unreachable (the send door's fence), so a permanent "0 self" would be
    /// noise on every invocation forever.
    #[test]
    fn footer_reports_self_only_when_it_occurred() {
        let loopback = (
            envelope_from("a1b2c3d4", "alpha", 1, "b"),
            summary(SummaryState::Delivered, 1),
            Direction::Loopback,
        );
        assert_eq!(
            footer_line(std::slice::from_ref(&loopback), "alpha"),
            "1 message to/from \"alpha\" — 1 self (↺)."
        );
    }

    // ---- surface selection ---------------------------------------------------

    /// The `pub(super)` reuse of the sibling verb's `select_scope` /
    /// `window_lower_bound` is safe ONLY while both verbs register all three arg
    /// ids: clap's `get_flag`/`get_one` PANIC on an id the invoked verb never
    /// declared, so dropping or renaming `--all` on either verb would blow up the
    /// OTHER one at runtime, with nothing at compile time to say so. This test is
    /// that missing signal.
    #[test]
    fn both_verbs_keep_the_arg_ids_the_shared_resolvers_read() {
        for verb in ["messages", "dispositions"] {
            let cmd = crate::cli::build_cli();
            let sub = cmd.find_subcommand(verb).expect("the verb is registered");
            let ids: BTreeSet<String> = sub
                .get_arguments()
                .map(|a| a.get_id().to_string())
                .collect();
            for id in ["host", "all", "window"] {
                assert!(
                    ids.contains(id),
                    "`{verb}` must keep --{id} while select_scope/window_lower_bound are shared \
                     (clap panics on an unregistered id), got {ids:?}"
                );
            }
        }
    }

    /// `--full` keeps the body's OWN structure and neutralizes everything else. A
    /// body is prose a peer wrote; the terminal reading it executes escapes.
    #[test]
    fn full_never_forwards_a_terminal_escape() {
        let hostile = "before\x1b[31mred\x07\x1b]0;pwned\x07after\nsecond\tline";
        let rows = vec![(
            envelope("alpha", 1000, hostile),
            summary(SummaryState::Delivered, 1000),
            Direction::Received,
        )];
        let out = render_full(&rows, 5000, Style::PLAIN);
        assert!(!out.contains('\x1b'), "no ESC survives: {out:?}");
        assert!(!out.contains('\x07'), "no BEL survives: {out:?}");
        assert!(out.contains("second\tline"), "tabs are kept: {out:?}");
        assert!(out.contains("after\nsecond"), "newlines are kept: {out:?}");
        assert!(
            out.contains("[31mred"),
            "the residue stays visible: {out:?}"
        );

        // The HEADER is logged data too — a hostile correlation_id or target must
        // not escape either.
        let mut e = envelope("tar\x1b[2Jget", 1000, "body");
        e.correlation_id = "id\x1b[1m".to_string();
        let rows = vec![(e, summary(SummaryState::Delivered, 1000), Direction::Received)];
        let out = render_full(&rows, 5000, Style::PLAIN);
        assert!(!out.contains('\x1b'), "no ESC in the header: {out:?}");
    }

    /// The `qd ls` rule: `--json` wins, `--table` forces human even for an agent,
    /// and otherwise an agent marker means the machine surface.
    #[test]
    fn surface_follows_who_drives() {
        let agent = env_with(&[("QD_SESSION_ID", "s-1")]);
        let human = env_with(&[]);
        assert!(resolve_emit_json(true, false, false, &human), "--json wins");
        assert!(
            resolve_emit_json(false, false, false, &agent),
            "an agent gets JSON"
        );
        assert!(
            !resolve_emit_json(false, true, false, &agent),
            "--table forces the human surface"
        );
        assert!(
            !resolve_emit_json(false, false, true, &agent),
            "--full is meaningless under JSON, so it asks for the human surface"
        );
        assert!(
            resolve_emit_json(true, false, true, &human),
            "an explicit --json still beats the surface --full implies"
        );
    }
}
