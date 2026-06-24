//! M3: `is_live_status` + `resolve_session` tiers, ported from TS
//! src/session.ts:1165-1239.
//!
//! Pure deciders over `&[Session]` — no I/O, no clock.

use crate::model::{Session, SessionStatus};

/// `isLiveStatus` (src/session.ts:1165-1173).
///
/// A session is "live" iff it is not a tombstone. Cold (JSONL-only history) and
/// killed (tombstoned) records are dead; idle/busy/shell are live. Single source
/// of truth for the live/dead split used by name-resolution (NEW-1) and by
/// the retired `spawn` verb's V5/V5b live-collision checks (historical).
pub fn is_live_status(status: SessionStatus) -> bool {
    status != SessionStatus::Cold && status != SessionStatus::Killed
}

/// P0 `info --json` `live` semantics (spec-w8): [`is_live_status`] AND
/// pid-alive-where-a-pid-exists. This is the field bond joins against for
/// liveness annotation.
///
/// A pid of `None` or `0` means "no pid recorded" (e.g. ZmxOnly rows) — fall
/// back to the status-only view there, the SAME convention as the verb layer's
/// `resolve_or_die` liveness refinement (we never call a legitimately-live
/// pid-less row dead). `pid_alive` is the injected `effects::is_pid_alive`
/// seam (production passes it directly; tests inject).
///
/// Pinned arms: stale-idle-dead-pid → `false`; cold → `false` (regardless of
/// pid); idle + alive pid → `true`; live status + no pid → `true`.
pub fn is_live_with_pid(
    status: SessionStatus,
    pid: Option<i64>,
    pid_alive: &dyn Fn(i32) -> bool,
) -> bool {
    is_live_status(status)
        && match pid {
            Some(p) if p != 0 => pid_alive(p as i32),
            _ => true,
        }
}

/// Result of resolving a query against the session list (TS returns
/// `Session | Session[] | null`).
#[derive(Debug)]
pub enum Resolution<'a> {
    /// Exactly one match.
    One(&'a Session),
    /// Ambiguous — multiple matches at the resolving tier.
    Many(Vec<&'a Session>),
    /// No match at any tier.
    None,
}

/// `resolveSession` (src/session.ts:1175-1239, reworked for stable ids — P0
/// wave-1 / spec-cli.md §3). Tiers, in order:
///   a) full stable-id exact (valid 8-char idstore id, case-insensitive) —
///      REPLACES the retired 3-char-code tier
///   b) PID (TS `parseInt` — leading-integer prefix, so `"123abc"` → 123; `>0`)
///   c) exact name (case-insensitive) with the live-over-tombstone refinement
///   d) name prefix
///   e) stable-id prefix (≥2 chars, valid-alphabet chars only; >1 → loud Many)
///   f) provider session-UUID prefix (back-compat addressing)
///   g) zmx name exact-or-prefix
///
/// Names deliberately outrank id-prefixes (spec §3: name | full id |
/// unambiguous prefix; ambiguity is an error, never a guess) — so an id prefix
/// can never shadow a name, and a full 8-char id is unambiguous by construction
/// at tier (a).
///
/// PID-over-id-prefix shadowing (P0 redfix F3, deliberate): an all-digit
/// 2-7 char query is tried as a PID at tier (b) BEFORE the stable-id-prefix
/// tier (e) — by design; if it matches a live PID, the id prefix never gets a
/// look-in. In fact tier (b) parses the query's LEADING DIGIT RUN (JS
/// `parseInt` semantics), so even "23a" hits PID 23 first. To address a
/// session whose id starts with digits, use more DIGITS if that changes the
/// parsed number, or the full 8-char id — which resolves at tier (a), ABOVE
/// the PID tier, and is always unambiguous.
///
/// Each tier: 1 match → `One`, >1 → `Many`, else fall to the next tier; if all
/// tiers miss, `None`.
pub fn resolve_session<'a>(query: &str, sessions: &'a [Session]) -> Resolution<'a> {
    // Default (pure) liveness: the on-disk status ENUM only — exactly the behavior
    // before the pid-aware refinement landed. Pure callers (ping / mark / send:relay)
    // keep this status-only view; acting verbs call
    // [`resolve_session_with_liveness`] with a real `is_pid_alive`-backed predicate.
    resolve_session_with_liveness(query, sessions, |s| is_live_status(s.status))
}

/// Liveness-aware [`resolve_session`]. `is_alive(&Session) -> bool` is the TRUE
/// liveness oracle the caller supplies — acting verbs (resume/connect/attach) pass
/// a predicate that gates on PID-aliveness (`is_pid_alive`), so a stale dead-pid row
/// whose on-disk status still says `idle`/`busy` does NOT count as live. This is the
/// fix for the "Ambiguous — matches 2 sessions" false collision where one row is a
/// genuinely-alive process and the other a dead leftover sharing the name/code.
///
/// The live-refinement runs on BOTH the full-stable-id tier and the NAME tier:
/// when a tier finds >1 match but exactly ONE is genuinely alive, it resolves to
/// that one instead of erroring. ≥2 genuinely-alive matches still return `Many`
/// (the loud ambiguity that `refuse_id_collision` and the verbs rely on).
pub fn resolve_session_with_liveness<'a>(
    query: &str,
    sessions: &'a [Session],
    is_alive: impl Fn(&Session) -> bool,
) -> Resolution<'a> {
    let q = query.to_lowercase();

    // Refine a multi-match down to the unique genuinely-alive row, if there is
    // exactly one. Returns `Some(resolution)` when the tier is decided (One or
    // Many), `None` to fall through to the next tier (0 alive among the matches).
    let refine = |matched: Vec<&'a Session>| -> Option<Resolution<'a>> {
        let live: Vec<&Session> = matched.iter().copied().filter(|s| is_alive(s)).collect();
        match live.len() {
            1 => Some(Resolution::One(live[0])),
            n if n > 1 => Some(Resolution::Many(live)),
            _ => None,
        }
    };

    // --- a) Full stable-id exact match (P0 wave-1; replaces the code tier). ---
    // Shape-gated: the (lowercased) query is a well-formed 8-char idstore id.
    // Ids are unique by construction (append-only mint, collision-checked), so
    // >1 match means duplicate joined rows — keep the code tier's pid-aware
    // refinement as defense in depth rather than silently picking.
    //
    // CROSS-NAMESPACE AMBIGUITY (red-team r3, lead-adjudicated MATERIAL): a
    // query can simultaneously be session-X's full id AND session-Y's exact
    // name (an id-shaped name). Id-space uniqueness does NOT make the QUERY
    // unambiguous across namespaces — spec §3 is normative ("ambiguity is an
    // error, never a guess"), so a coexisting DISTINCT exact-name match makes
    // this tier return the loud `Many` instead of silently preferring the id
    // owner (which made the name-owner unaddressable and sent `stop` to the
    // wrong session). Liveness does NOT refine this case — the collision is
    // between namespaces, not stale rows; the operator must disambiguate
    // (rename or address by the other token).
    if crate::idstore::is_valid_id(&q) {
        let id_match: Vec<&Session> = sessions
            .iter()
            .filter(|s| s.sb_id.as_deref() == Some(q.as_str()))
            .collect();
        if !id_match.is_empty() {
            let name_shadow: Vec<&Session> = sessions
                .iter()
                .filter(|s| {
                    s.name.as_deref().map(str::to_lowercase) == Some(q.clone())
                        && !id_match
                            .iter()
                            .any(|m| std::ptr::eq(*m as *const Session, *s as *const Session))
                })
                .collect();
            if !name_shadow.is_empty() {
                let mut all = id_match.clone();
                all.extend(name_shadow);
                return Resolution::Many(all);
            }
        }
        if id_match.len() == 1 {
            return Resolution::One(id_match[0]);
        }
        if id_match.len() > 1 {
            return refine(id_match.clone()).unwrap_or(Resolution::Many(id_match));
        }
    }

    // --- b) PID match (any number). ---
    // TS `parseInt(query)`: leading-integer prefix parse, so "123abc" → 123 and
    // a non-numeric prefix → NaN. We replicate that prefix parse (NOT Rust's
    // strict `str::parse`, which would reject "123abc").
    if let Some(num) = parse_int_prefix(query) {
        if num > 0 {
            let pid_match: Vec<&Session> = sessions.iter().filter(|s| s.pid == Some(num)).collect();
            if pid_match.len() == 1 {
                return Resolution::One(pid_match[0]);
            }
            if pid_match.len() > 1 {
                return Resolution::Many(pid_match);
            }
        }
    }

    // --- c) Exact name match — prefer the unique LIVE exact-match over cold
    // tombstones (NEW-1; contract A header / §5). sb does not enforce name
    // uniqueness and keeps cold tombstones indefinitely, so a name shared by one
    // live session and ≥1 cold record must resolve to the LIVE one for all
    // addressing (info / send:pty / wait / kill / the spawned principal's /goal).
    // Without this, a freshly-spawned principal whose name has cold namesakes
    // would be un-addressable ("Ambiguous — matches N"). Genuinely ambiguous ONLY
    // when >1 LIVE session shares the exact name.
    let name_exact: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.name.as_deref().map(str::to_lowercase) == Some(q.clone()))
        .collect();
    if name_exact.len() == 1 {
        return Resolution::One(name_exact[0]);
    }
    if name_exact.len() > 1 {
        // Pid-aware refinement (the NAME-tier fix): `is_alive` now gates on real
        // liveness (PID for acting verbs; status-only for pure callers), so a
        // dead-pid stale "idle" row no longer counts as live and the unique
        // genuinely-alive same-name row resolves. 0 alive (all cold/killed
        // tombstones, OR all stale dead-pid rows) falls through to the prefix/id/zmx
        // tiers so a purely-dead name still resolves for resume/info exactly as
        // before. >1 genuinely-alive same-name → ambiguous (loud `Many`).
        if let Some(res) = refine(name_exact) {
            return res;
        }
    }

    // --- d) Name prefix match. ---
    let name_prefix: Vec<&Session> = sessions
        .iter()
        .filter(|s| {
            s.name
                .as_deref()
                .is_some_and(|n| n.to_lowercase().starts_with(&q))
        })
        .collect();
    if name_prefix.len() == 1 {
        return Resolution::One(name_prefix[0]);
    }
    if name_prefix.len() > 1 {
        return Resolution::Many(name_prefix);
    }

    // --- e) Stable-id prefix match (≥2 chars, valid-alphabet chars only). ---
    // Sits BEFORE the provider-UUID-prefix tier. Unambiguous → resolve; >1 →
    // loud Many (spec: ambiguity is an error, never a guess). A wrong-alphabet
    // query (e.g. carrying 0/1/o/l or a hyphen) skips this tier entirely and
    // falls through to the UUID tier below.
    if q.chars().count() >= 2 && crate::idstore::is_alphabet_prefix(&q) {
        let sbid_prefix: Vec<&Session> = sessions
            .iter()
            .filter(|s| s.sb_id.as_deref().is_some_and(|id| id.starts_with(&q)))
            .collect();
        if sbid_prefix.len() == 1 {
            return Resolution::One(sbid_prefix[0]);
        }
        if sbid_prefix.len() > 1 {
            return Resolution::Many(sbid_prefix);
        }
    }

    // --- f) Provider session-UUID prefix match (back-compat addressing). ---
    let id_prefix: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.session_id.to_lowercase().starts_with(&q))
        .collect();
    if id_prefix.len() == 1 {
        return Resolution::One(id_prefix[0]);
    }
    if id_prefix.len() > 1 {
        return Resolution::Many(id_prefix);
    }

    // --- g) zmx name match (exact OR prefix). ---
    let zmx_match: Vec<&Session> = sessions
        .iter()
        .filter(|s| {
            s.zmx_name.as_deref().is_some_and(|z| {
                let z = z.to_lowercase();
                z == q || z.starts_with(&q)
            })
        })
        .collect();
    if zmx_match.len() == 1 {
        return Resolution::One(zmx_match[0]);
    }
    if zmx_match.len() > 1 {
        return Resolution::Many(zmx_match);
    }

    Resolution::None
}

/// Replicate JS `parseInt(s)` (radix 10): skip leading whitespace, take an
/// optional sign then the leading run of digits; "123abc" → 123, "abc" → None,
/// "" → None. Returns None where JS yields NaN.
fn parse_int_prefix(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let mut chars = t.chars().peekable();
    let mut out = String::new();
    if let Some(&c) = chars.peek() {
        if c == '+' || c == '-' {
            out.push(c);
            chars.next();
        }
    }
    let mut saw_digit = false;
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            out.push(c);
            saw_digit = true;
            chars.next();
        } else {
            break;
        }
    }
    if !saw_digit {
        return None; // NaN
    }
    out.parse::<i64>().ok()
}

// --- Live-id-collision preflight (Pete feedback #6: duplicate session ids) -----
//
// sb does NOT mint its own session id — `Session.session_id` IS the provider's id
// (claude's session UUID / codex's thread_id). Uniqueness therefore rides on (a)
// the provider minting fresh on fork (claude: `--fork-session`) and (b) the engine
// never re-registering a LIVE id under a second row.
//
// The join collapses two same-id LIVE rows to one (keep-newest, join.rs §dedupe) —
// CORRECT for the legitimate stale-old-pid + new-live-pid case (e.g. codex resume
// leaves the old row), but it HIDES a genuine collision: two distinct ALIVE
// processes sharing an id. `resolve_or_die`'s loud `Many` path never fires for an
// id-collision because the dedup already starved it. This pure decider runs over
// the RAW registry's ALIVE rows (the caller filters by `is_pid_alive`) so resume /
// connect can refuse loudly instead of silently picking a survivor.

/// A LIVE registry row reduced to the fields the collision preflight needs. The
/// caller has ALREADY confirmed `pid` is alive (`is_pid_alive`) before building
/// this — the decider is pure and trusts that filtering.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveRow {
    pub session_id: String,
    pub name: Option<String>,
    pub pid: i64,
}

/// Verdict of the live-id-collision preflight. Pure over the ALIVE rows.
#[derive(Debug, PartialEq)]
pub enum LiveIdCollision {
    /// No ALIVE row carries the target id — genuinely resumable (truly cold, or
    /// only stale dead-pid rows remain). Proceed.
    Resumable,
    /// Exactly one ALIVE row carries the target id: the session is actually live
    /// (the deduped join may report it Cold — the SEAM-3 misread). A resume would
    /// spawn a SECOND process on the same id → caller refuses ("already alive, use
    /// attach"). A connect-style verb may instead attach to this pid.
    AlreadyAlive { pid: i64 },
    /// ≥2 ALIVE rows carry the target id: a genuine duplicate-id collision. The
    /// caller MUST refuse loudly and surface all of them — never silently pick.
    Collision(Vec<LiveRow>),
}

/// Classify the target id against the ALIVE registry rows (caller pre-filters by
/// `is_pid_alive`). An empty target id never matches (the empty-id case is handled
/// by the resume verb's own "no session ID" guard, and ZmxOnly rows carry "").
pub fn detect_live_id_collision(target_id: &str, alive: &[LiveRow]) -> LiveIdCollision {
    if target_id.is_empty() {
        return LiveIdCollision::Resumable;
    }
    let matches: Vec<LiveRow> = alive
        .iter()
        .filter(|r| r.session_id == target_id)
        .cloned()
        .collect();
    match matches.len() {
        0 => LiveIdCollision::Resumable,
        1 => LiveIdCollision::AlreadyAlive {
            pid: matches[0].pid,
        },
        _ => LiveIdCollision::Collision(matches),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SessionBranch, SessionStatus};

    fn base(session_id: &str, status: SessionStatus) -> Session {
        Session {
            name: None,
            user_named: None,
            session_id: session_id.to_string(),
            code: None,
            sb_id: None,
            pid: None,
            status,
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
            which_branch: SessionBranch::LiveRegistry,
        }
    }

    fn one_id<'a>(r: &Resolution<'a>) -> &'a str {
        match r {
            Resolution::One(s) => s.session_id.as_str(),
            _ => panic!("expected One, got {r:?}"),
        }
    }

    // --- is_live_with_pid (P0 info --json spec-w8) — both arms pinned ---

    #[test]
    fn live_with_pid_stale_idle_dead_pid_is_false() {
        // On-disk status still says idle but the process is GONE → live:false.
        let dead = |_: i32| false;
        assert!(!is_live_with_pid(SessionStatus::Idle, Some(4242), &dead));
    }

    #[test]
    fn live_with_pid_cold_is_false_regardless_of_pid() {
        let alive = |_: i32| true;
        assert!(!is_live_with_pid(SessionStatus::Cold, None, &alive));
        assert!(!is_live_with_pid(SessionStatus::Cold, Some(4242), &alive));
        assert!(!is_live_with_pid(SessionStatus::Killed, Some(4242), &alive));
    }

    #[test]
    fn live_with_pid_idle_alive_is_true() {
        let alive = |_: i32| true;
        assert!(is_live_with_pid(SessionStatus::Idle, Some(4242), &alive));
        assert!(is_live_with_pid(SessionStatus::Busy, Some(4242), &alive));
    }

    #[test]
    fn live_with_pid_no_pid_falls_back_to_status_only() {
        // None / 0 = "no pid recorded" (ZmxOnly rows): status-only view, the
        // pid_alive seam must NOT be consulted.
        let must_not_call = |_: i32| panic!("pid_alive consulted for a pid-less row");
        assert!(is_live_with_pid(SessionStatus::Idle, None, &must_not_call));
        assert!(is_live_with_pid(
            SessionStatus::Shell,
            Some(0),
            &must_not_call
        ));
    }

    // --- tier (a): full stable-id exact ---

    #[test]
    fn stable_id_full_exact_resolves() {
        let mut a = base("aaa", SessionStatus::Idle);
        a.sb_id = Some("ab3kx9mq".into());
        let b = base("bbb", SessionStatus::Idle);
        let sessions = vec![a, b];
        assert_eq!(one_id(&resolve_session("ab3kx9mq", &sessions)), "aaa");
    }

    #[test]
    fn stable_id_full_exact_is_case_insensitive() {
        let mut a = base("aaa", SessionStatus::Idle);
        a.sb_id = Some("ab3kx9mq".into());
        let sessions = vec![a];
        assert_eq!(one_id(&resolve_session("AB3KX9MQ", &sessions)), "aaa");
    }

    #[test]
    fn stable_id_full_miss_falls_through_to_later_tiers() {
        // A valid-shaped 8-char id that matches NO sb_id must keep falling: here
        // it lands on the session-UUID prefix tier (the uuid starts with it).
        let a = base("ab3kx9mq-uuid-rest", SessionStatus::Cold);
        let sessions = vec![a];
        assert_eq!(
            one_id(&resolve_session("ab3kx9mq", &sessions)),
            "ab3kx9mq-uuid-rest"
        );
    }

    #[test]
    fn stable_id_full_duplicate_rows_are_many() {
        // Two joined rows carrying the SAME sb_id (impossible by construction;
        // defense in depth) → loud Many under the status-only view.
        let mut a = base("aaa", SessionStatus::Idle);
        a.sb_id = Some("ab3kx9mq".into());
        let mut b = base("bbb", SessionStatus::Idle);
        b.sb_id = Some("ab3kx9mq".into());
        let sessions = vec![a, b];
        match resolve_session("ab3kx9mq", &sessions) {
            Resolution::Many(v) => assert_eq!(v.len(), 2),
            other => panic!("expected Many, got {other:?}"),
        }
    }

    #[test]
    fn three_char_code_no_longer_resolves() {
        // The retired code tier: a query equal to a row's 3-char code finds
        // nothing (codes are dead for resolution).
        let mut a = base("aaa", SessionStatus::Idle);
        a.code = Some("ab1".into());
        let sessions = vec![a];
        assert!(matches!(
            resolve_session("ab1", &sessions),
            Resolution::None
        ));
    }

    // --- tier (e): stable-id prefix ---

    #[test]
    fn stable_id_prefix_unambiguous_resolves() {
        let mut a = base("aaa", SessionStatus::Idle);
        a.sb_id = Some("ab3kx9mq".into());
        let mut b = base("bbb", SessionStatus::Idle);
        b.sb_id = Some("cd47qrst".into());
        let sessions = vec![a, b];
        assert_eq!(one_id(&resolve_session("ab", &sessions)), "aaa");
        assert_eq!(one_id(&resolve_session("AB3", &sessions)), "aaa");
        assert_eq!(one_id(&resolve_session("cd47qrs", &sessions)), "bbb");
    }

    /// P0 redfix F3 pin: an all-digit 2-7 char query hits the PID tier (b)
    /// BEFORE the stable-id-prefix tier (e) — deliberate shadowing. The full
    /// 8-char id still resolves at tier (a), above PID.
    #[test]
    fn all_digit_short_query_resolves_as_pid_before_id_prefix() {
        let mut a = base("aaa", SessionStatus::Idle);
        a.pid = Some(23);
        let mut b = base("bbb", SessionStatus::Idle);
        b.sb_id = Some("23abcdef".into());
        let sessions = vec![a, b];
        // "23" is both a live PID and an id prefix → the PID wins, by design.
        assert_eq!(one_id(&resolve_session("23", &sessions)), "aaa");
        // Tier (b) parses the LEADING DIGIT RUN (JS parseInt): "23a" → 23, so
        // the PID still wins — pinned, not assumed.
        assert_eq!(one_id(&resolve_session("23a", &sessions)), "aaa");
        // The guaranteed escape hatch: the full 8-char id (tier (a), above PID).
        assert_eq!(one_id(&resolve_session("23abcdef", &sessions)), "bbb");
    }

    #[test]
    fn stable_id_prefix_ambiguous_is_loud_many() {
        let mut a = base("aaa", SessionStatus::Idle);
        a.sb_id = Some("ab3kx9mq".into());
        let mut b = base("bbb", SessionStatus::Idle);
        b.sb_id = Some("ab47qrst".into());
        let sessions = vec![a, b];
        match resolve_session("ab", &sessions) {
            Resolution::Many(v) => assert_eq!(v.len(), 2),
            other => panic!("expected Many, got {other:?}"),
        }
        // One more char disambiguates.
        assert_eq!(one_id(&resolve_session("ab3", &sessions)), "aaa");
    }

    #[test]
    fn stable_id_prefix_miss_falls_through_to_uuid_tier() {
        // No sb_id starts with "de"; the provider UUID does → UUID tier resolves.
        let mut a = base("deadbeef-1234", SessionStatus::Cold);
        a.sb_id = Some("ab3kx9mq".into());
        let sessions = vec![a];
        assert_eq!(one_id(&resolve_session("de", &sessions)), "deadbeef-1234");
    }

    #[test]
    fn wrong_alphabet_prefix_skips_id_tier_and_hits_uuid_tier() {
        // "10ab" carries 0/1 (not in the id alphabet) → skips the stable-id
        // prefix tier entirely and falls through to the UUID-prefix tier.
        let mut a = base("10abcdef-9999", SessionStatus::Cold);
        a.sb_id = Some("ab3kx9mq".into());
        let sessions = vec![a];
        assert_eq!(one_id(&resolve_session("10ab", &sessions)), "10abcdef-9999");
    }

    #[test]
    fn name_outranks_stable_id_prefix() {
        // Spec §3 tier order: a NAME that happens to look like an id prefix
        // resolves as the name, never the id.
        let mut named = base("named-uuid", SessionStatus::Idle);
        named.name = Some("ab".into());
        let mut by_id = base("id-uuid", SessionStatus::Idle);
        by_id.sb_id = Some("ab3kx9mq".into());
        let sessions = vec![named, by_id];
        assert_eq!(one_id(&resolve_session("ab", &sessions)), "named-uuid");
    }

    // --- red-team r3 (lead-adjudicated MATERIAL): full-id vs exact-name -----

    #[test]
    fn full_id_vs_distinct_exact_name_is_ambiguous() {
        // Spec §3 "ambiguity is an error, never a guess": a query that is
        // simultaneously X's FULL stable id and Y's EXACT name is a
        // cross-namespace double-match → loud Many, never silently the id
        // owner (the pre-fix behavior made Y unaddressable by its own unique
        // name and sent `stop <query>` to X). Case-insensitive on both sides.
        let mut id_owner = base("uuid-x", SessionStatus::Idle);
        id_owner.sb_id = Some("fz2p89tv".into());
        id_owner.name = Some("first".into());
        let mut name_owner = base("uuid-y", SessionStatus::Idle);
        name_owner.name = Some("FZ2P89TV".into()); // exact name, case-folded
        let sessions = vec![id_owner, name_owner];
        match resolve_session("fz2p89tv", &sessions) {
            Resolution::Many(v) => {
                assert_eq!(v.len(), 2, "both namespace owners surfaced");
                let ids: Vec<&str> = v.iter().map(|s| s.session_id.as_str()).collect();
                assert!(ids.contains(&"uuid-x") && ids.contains(&"uuid-y"));
            }
            other => panic!("expected Many, got {other:?}"),
        }
    }

    #[test]
    fn full_id_owner_also_named_that_string_resolves() {
        // The SAME session carrying the query as both its id and its name is
        // not ambiguous — one referent, One.
        let mut s = base("uuid-x", SessionStatus::Idle);
        s.sb_id = Some("fz2p89tv".into());
        s.name = Some("fz2p89tv".into());
        let sessions = vec![s];
        assert_eq!(one_id(&resolve_session("fz2p89tv", &sessions)), "uuid-x");
    }

    #[test]
    fn dead_id_owner_vs_live_name_owner_still_ambiguous() {
        // Liveness does NOT refine the cross-namespace collision: even a COLD
        // id-owner shadowing a LIVE name-owner stays a loud Many (the operator
        // must disambiguate — rename, or use the other token). This pins that
        // the fix did not bolt the pid-aware refinement onto this case.
        let mut id_owner = base("uuid-x", SessionStatus::Cold);
        id_owner.sb_id = Some("fz2p89tv".into());
        let mut name_owner = base("uuid-y", SessionStatus::Busy);
        name_owner.name = Some("fz2p89tv".into());
        let sessions = vec![id_owner, name_owner];
        match resolve_session_with_liveness("fz2p89tv", &sessions, |s| s.session_id == "uuid-y") {
            Resolution::Many(v) => assert_eq!(v.len(), 2),
            other => panic!("expected Many, got {other:?}"),
        }
    }

    #[test]
    fn pid_tier_and_parseint_prefix() {
        let mut a = base("aaa", SessionStatus::Idle);
        a.pid = Some(123);
        let sessions = vec![a];
        // exact numeric
        assert_eq!(one_id(&resolve_session("123", &sessions)), "aaa");
        // "123abc" → parseInt 123 → matches pid 123
        assert_eq!(one_id(&resolve_session("123abc", &sessions)), "aaa");
    }

    #[test]
    fn pid_tier_ambiguous() {
        let mut a = base("aaa", SessionStatus::Idle);
        a.pid = Some(7);
        let mut b = base("bbb", SessionStatus::Idle);
        b.pid = Some(7);
        let sessions = vec![a, b];
        match resolve_session("7", &sessions) {
            Resolution::Many(v) => assert_eq!(v.len(), 2),
            other => panic!("expected Many, got {other:?}"),
        }
    }

    #[test]
    fn exact_name_unique() {
        let mut a = base("aaa", SessionStatus::Idle);
        a.name = Some("Alpha".into());
        let sessions = vec![a];
        assert_eq!(one_id(&resolve_session("alpha", &sessions)), "aaa");
    }

    #[test]
    fn live_over_tombstone_one_live_two_cold() {
        // 1 live + 2 cold, same name → resolves to the live one.
        let mut live = base("live", SessionStatus::Busy);
        live.name = Some("Worker".into());
        let mut c1 = base("c1", SessionStatus::Cold);
        c1.name = Some("Worker".into());
        let mut c2 = base("c2", SessionStatus::Killed);
        c2.name = Some("Worker".into());
        let sessions = vec![c1, live, c2];
        assert_eq!(one_id(&resolve_session("worker", &sessions)), "live");
    }

    #[test]
    fn pid_aware_refinement_drops_dead_pid_stale_row_by_name() {
        // THE DUP-SESSION BUG: two same-name (and same-code) rows, both status Idle,
        // but one pid is genuinely ALIVE and the other is a stale leftover whose pid
        // is DEAD. The status-only view counts BOTH live → false "Ambiguous". The
        // pid-aware predicate drops the dead-pid row so the name resolves to the one
        // truly-alive session.
        let mut alive = base("alive-uuid", SessionStatus::Idle);
        alive.name = Some("orc-13".into());
        alive.sb_id = Some("ab3kx9mq".into());
        alive.pid = Some(100);
        let mut stale = base("stale-uuid", SessionStatus::Idle);
        stale.name = Some("orc-13".into());
        stale.sb_id = Some("ab3kx9mq".into()); // duplicate row, same stable id
        stale.pid = Some(200); // status still Idle, but its process is DEAD
        let sessions = vec![stale, alive];

        // Predicate: only pid 100 is genuinely alive.
        let live = |s: &Session| s.pid == Some(100);

        // NAME tier resolves to the live one (not Many).
        assert_eq!(
            one_id(&resolve_session_with_liveness("orc-13", &sessions, live)),
            "alive-uuid"
        );
        // Full STABLE-ID tier resolves to the live one too (not Many).
        assert_eq!(
            one_id(&resolve_session_with_liveness("ab3kx9mq", &sessions, live)),
            "alive-uuid"
        );

        // CONTROL: the status-only default still sees BOTH as live → Many (proving
        // the test bites only because of the pid-aware predicate).
        match resolve_session("orc-13", &sessions) {
            Resolution::Many(v) => assert_eq!(v.len(), 2),
            other => panic!("status-only must stay Many, got {other:?}"),
        }
    }

    #[test]
    fn pid_aware_two_genuinely_alive_same_name_still_ambiguous() {
        // Do NOT regress the loud collision: ≥2 GENUINELY-alive same-name rows must
        // still return Many even under the pid-aware predicate.
        let mut a = base("a-uuid", SessionStatus::Idle);
        a.name = Some("dup".into());
        a.pid = Some(100);
        let mut b = base("b-uuid", SessionStatus::Busy);
        b.name = Some("dup".into());
        b.pid = Some(200);
        let sessions = vec![a, b];
        let both_live = |_: &Session| true;
        match resolve_session_with_liveness("dup", &sessions, both_live) {
            Resolution::Many(v) => assert_eq!(v.len(), 2),
            other => panic!("expected Many, got {other:?}"),
        }
    }

    #[test]
    fn two_live_same_name_is_ambiguous() {
        let mut l1 = base("l1", SessionStatus::Idle);
        l1.name = Some("Dup".into());
        let mut l2 = base("l2", SessionStatus::Busy);
        l2.name = Some("Dup".into());
        let sessions = vec![l1, l2];
        match resolve_session("dup", &sessions) {
            Resolution::Many(v) => assert_eq!(v.len(), 2),
            other => panic!("expected Many, got {other:?}"),
        }
    }

    #[test]
    fn zero_live_exact_falls_through_to_prefix() {
        // 2 cold exact "node" + a distinct longer cold name that shares the
        // prefix. The exact tier finds 2, 0 live → falls through. The prefix
        // tier then matches all three names that start with "node" → Many.
        let mut c1 = base("c1", SessionStatus::Cold);
        c1.name = Some("node".into());
        let mut c2 = base("c2", SessionStatus::Killed);
        c2.name = Some("node".into());
        let mut c3 = base("c3", SessionStatus::Cold);
        c3.name = Some("nodejs".into());
        let sessions = vec![c1, c2, c3];
        // exact "node" matches c1,c2 (2, 0 live) → fall through; prefix matches
        // c1,c2,c3 → Many of 3.
        match resolve_session("node", &sessions) {
            Resolution::Many(v) => assert_eq!(v.len(), 3),
            other => panic!("expected Many of 3, got {other:?}"),
        }
    }

    #[test]
    fn zero_live_exact_single_prefix_resolves() {
        // 0 live exact → fall through; if the prefix tier then has exactly one
        // distinct match it resolves to One. Here both cold names are identical
        // "node", so prefix still finds 2 → Many. Use a single cold record to
        // show the One path through the prefix tier.
        let mut c1 = base("only", SessionStatus::Cold);
        c1.name = Some("solo".into());
        let sessions = vec![c1];
        // exact "solo" is a single match → One directly (len==1 branch).
        assert_eq!(one_id(&resolve_session("solo", &sessions)), "only");
    }

    #[test]
    fn name_prefix_tier() {
        let mut a = base("aaa", SessionStatus::Idle);
        a.name = Some("frontend".into());
        let sessions = vec![a];
        assert_eq!(one_id(&resolve_session("front", &sessions)), "aaa");
    }

    #[test]
    fn session_id_prefix_tier() {
        let a = base("deadbeef-1234", SessionStatus::Cold);
        let sessions = vec![a];
        assert_eq!(
            one_id(&resolve_session("deadbe", &sessions)),
            "deadbeef-1234"
        );
    }

    #[test]
    fn zmx_name_exact_or_prefix_tier() {
        let mut a = base("aaa", SessionStatus::Idle);
        a.zmx_name = Some("zmx-sess-42".into());
        let sessions = vec![a];
        // exact
        assert_eq!(one_id(&resolve_session("zmx-sess-42", &sessions)), "aaa");
        // prefix
        assert_eq!(one_id(&resolve_session("zmx-sess", &sessions)), "aaa");
    }

    #[test]
    fn no_match_is_none() {
        let a = base("aaa", SessionStatus::Idle);
        let sessions = vec![a];
        assert!(matches!(
            resolve_session("nothing-here", &sessions),
            Resolution::None
        ));
    }

    #[test]
    fn parse_int_prefix_semantics() {
        assert_eq!(parse_int_prefix("123abc"), Some(123));
        assert_eq!(parse_int_prefix("  42"), Some(42));
        assert_eq!(parse_int_prefix("abc"), None);
        assert_eq!(parse_int_prefix(""), None);
        assert_eq!(parse_int_prefix("-5x"), Some(-5));
    }

    // --- live-id-collision preflight (Pete feedback #6) ---

    fn row(id: &str, name: &str, pid: i64) -> LiveRow {
        LiveRow {
            session_id: id.to_string(),
            name: Some(name.to_string()),
            pid,
        }
    }

    #[test]
    fn collision_two_alive_rows_same_id_is_surfaced() {
        // THE BUG: two distinct ALIVE processes share one id. The deduped join
        // hides this (collapses to one row); the preflight must surface BOTH so
        // resume refuses loudly instead of silently picking a survivor.
        let alive = vec![
            row("orc-13-uuid", "sb-rust-orc-13", 100),
            row("orc-13-uuid", "sb-rust-orc-13", 200),
        ];
        match detect_live_id_collision("orc-13-uuid", &alive) {
            LiveIdCollision::Collision(rows) => {
                assert_eq!(rows.len(), 2, "both colliding rows surfaced");
                let pids: Vec<i64> = rows.iter().map(|r| r.pid).collect();
                assert!(pids.contains(&100) && pids.contains(&200));
            }
            other => panic!("expected Collision, got {other:?}"),
        }
    }

    #[test]
    fn one_alive_row_same_id_is_already_alive() {
        // The SEAM-3 misread: the join may report this session Cold (dedup of a
        // stale row), but a live pid carries its id → it is actually alive.
        let alive = vec![row("live-uuid", "worker", 777)];
        assert_eq!(
            detect_live_id_collision("live-uuid", &alive),
            LiveIdCollision::AlreadyAlive { pid: 777 }
        );
    }

    #[test]
    fn no_alive_row_with_id_is_resumable() {
        // Truly cold / only stale dead-pid rows (the caller filtered them out, so
        // they never appear here) → genuinely resumable. The legitimate
        // codex-resume-leaves-old-row case lands here (old pid dead → not alive).
        let alive = vec![row("other-uuid", "elsewhere", 5)];
        assert_eq!(
            detect_live_id_collision("cold-uuid", &alive),
            LiveIdCollision::Resumable
        );
    }

    #[test]
    fn empty_target_id_never_matches() {
        // ZmxOnly rows carry session_id ""; an empty target must not collide with
        // them (the resume verb's own empty-id guard handles the empty case).
        let alive = vec![row("", "zmx-only", 9), row("", "zmx-only-2", 10)];
        assert_eq!(
            detect_live_id_collision("", &alive),
            LiveIdCollision::Resumable
        );
    }
}
