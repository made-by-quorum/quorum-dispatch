//! M4: ls --json / info parity surface, TS src/index.ts:61-63 + src/commands/status.ts:621-660.
//!
//! The THINNEST layer (spec §8): a fix-wave may change `ls --json`; rework lands
//! HERE, not in the deciders. Two responsibilities:
//!
//!   1. [`ls_json`] — replicate `JSON.stringify(sessions, null, 2)` EXACTLY:
//!      per-branch key SET + ORDER (the TS object literals differ per
//!      construction branch), `undefined`-valued keys OMITTED, `code` appended
//!      LAST (assignShortCodes mutates it post-construction so it lands last in
//!      JS key order), `Date` → ISO-8601 ms UTC (`Date.toJSON`).
//!   2. [`info_text`] — port status.ts:621-660 (field lines, conditional lines,
//!      Recent-conversation block). The `toLocaleString()` lines are
//!      NORMALIZATION-CLASS in the 0b comparator (byte-exact only post-
//!      normalization), since the real TS output is locale/timezone dependent.
//!
//! serde_json is built with `preserve_order` (Cargo.toml), so a `Map`'s
//! insertion order IS the emitted key order — we insert in TS-literal order.

use serde_json::{json, Map, Value};

use crate::discovery::DiscoveryHealth;
use crate::model::{Session, SessionBranch, TurnPreview};
use crate::stray::Stray;
use crate::telemetry::SnapshotMap;

// --- ls --json ---

/// Build the `ls --json` value: a JSON array of session objects in TS key order,
/// with stray rows appended (spec §7).
///
/// Pretty-printing: caller serializes with [`to_pretty`] (or
/// `serde_json::to_string_pretty`), which matches `JSON.stringify(x, null, 2)`
/// (2-space indent, inline `[]`/`{}` for empties — verified vs bun).
pub fn ls_json(sessions: &[Session], strays: &[Stray]) -> Value {
    ls_json_with_fold(sessions, strays, None)
}

/// As [`ls_json`], but with an OPTIONAL A6 telemetry fold (spec §4.4). When
/// `fold` is `None` (or yields nothing for a session) the output is
/// BYTE-IDENTICAL to [`ls_json`] — the additive `"backend"`/`"spawnedBy"` row
/// fields appear ONLY when the fold has values for that session. The render fn
/// stays pure (the verbs layer loads the fold best-effort and passes it).
pub fn ls_json_with_fold(
    sessions: &[Session],
    strays: &[Stray],
    fold: Option<&SnapshotMap>,
) -> Value {
    ls_json_full(sessions, strays, fold, None)
}

/// As [`ls_json_with_fold`], plus the WP-B-CS-2 ADDITIVE readiness facet
/// (S-B rulings D3: `ready`/`silent`/`stuck`). `readiness` is a slice ALIGNED to
/// `sessions` (index `i` ↔ `sessions[i]`); `Some(word)` emits a `"readiness"` key
/// on that row, `None` (or a `None` to `ls_json_with_fold`) emits no key — so a
/// caller that passes no facet renders BYTE-IDENTICAL to before (the facet is
/// never part of the byte-faithful TS surface; the `status` field is untouched).
/// The verbs layer computes the facet from the per-row liveness classification.
pub fn ls_json_full(
    sessions: &[Session],
    strays: &[Stray],
    fold: Option<&SnapshotMap>,
    readiness: Option<&[Option<&str>]>,
) -> Value {
    ls_json_full_acp(sessions, strays, fold, readiness, &[])
}

/// As [`ls_json_full`], plus (L) Item 3 the PRIMARY-SOURCED acp `status` override aligned
/// to `sessions`: `acp_status[i]` Some → that row's `"status"` reflects the live acp
/// probe (live/stopped/dead-endpoint), not the stale stored field. None (incl. an empty
/// slice, the [`ls_json_full`] default) → the unchanged stored status → a non-acp listing
/// is BYTE-IDENTICAL to the pre-Item-3 JSON.
pub fn ls_json_full_acp(
    sessions: &[Session],
    strays: &[Stray],
    fold: Option<&SnapshotMap>,
    readiness: Option<&[Option<&str>]>,
    acp_status: &[Option<String>],
) -> Value {
    // P0 wave-1: shortest-unique prefixes (min 2 chars) computed among the
    // LISTED sessions' stable ids — pure read-time computation, the same way
    // codes were computed. Sessions with no qd_id contribute nothing and gain
    // no keys (additive: a fixture with no ids renders today's exact bytes).
    let prefixes = crate::idstore::prefix_map(sessions);
    let mut arr: Vec<Value> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let facet = readiness.and_then(|r| r.get(i).copied().flatten());
            let acp = acp_status.get(i).and_then(|o| o.as_deref());
            session_to_value(s, fold, &prefixes, facet, acp)
        })
        .collect();
    arr.extend(strays.iter().map(stray_to_value));
    Value::Array(arr)
}

/// Serialize a `Value` exactly as `JSON.stringify(value, null, 2)` does: 2-space
/// indentation, empty array/object inline. serde_json's `to_string_pretty`
/// matches bun's output (verified: `[]`, `{}`, and arrays-of-objects identical).
pub fn to_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("Value serializes")
}

/// One TS-faithful session object. Key SET + ORDER replicate the per-branch
/// object literal; `None` fields are omitted (TS drops `undefined`); `code` is
/// appended LAST (assignShortCodes runs post-construction, index.ts:55). `code`
/// is always present once codes are assigned (TS `s.code || "---"`); a `None`
/// here means codes were not assigned — we still emit it last when `Some`.
fn session_to_value(
    s: &Session,
    fold: Option<&SnapshotMap>,
    id_prefixes: &std::collections::HashMap<String, String>,
    readiness: Option<&str>,
    acp_status: Option<&str>,
) -> Value {
    let mut m = Map::new();

    // (L) Item 3: an acp row's `status` is the PRIMARY-SOURCED override when present;
    // otherwise the stored field (non-acp rows → byte-identical to the TS surface).
    let status_str = acp_status.unwrap_or_else(|| s.status.as_str());

    // Helpers that honor TS undefined-omission.
    let str_opt = |m: &mut Map<String, Value>, k: &str, v: &Option<String>| {
        if let Some(v) = v {
            m.insert(k.to_string(), Value::String(v.clone()));
        }
    };

    match s.which_branch {
        SessionBranch::LiveRegistry => {
            // session.ts:913-933.
            str_opt(&mut m, "name", &s.name);
            m.insert("userNamed".into(), json!(s.user_named.unwrap_or(false)));
            m.insert("sessionId".into(), json!(s.session_id));
            opt_pid(&mut m, s.pid);
            m.insert("status".into(), json!(status_str));
            str_opt(&mut m, "zmxName", &s.zmx_name);
            opt_u32(&mut m, "zmxClients", s.zmx_clients);
            str_opt(&mut m, "socketDir", &s.socket_dir);
            opt_u16(&mut m, "relayPort", s.relay_port);
            m.insert("turns".into(), json!(s.turns));
            m.insert("tokens".into(), json!(s.tokens));
            str_opt(&mut m, "cwd", &s.cwd);
            opt_date(&mut m, "lastActive", s.last_active_ms);
            str_opt(&mut m, "version", &s.version);
            opt_date(&mut m, "startedAt", s.started_at_ms);
            str_opt(&mut m, "gitBranch", &s.git_branch);
            str_opt(&mut m, "jsonlPath", &s.jsonl_path);
            opt_turns(&mut m, &s.last_turns);
            m.insert("provider".into(), json!(s.provider));
        }
        SessionBranch::ColdJsonl => {
            // session.ts:960-977 — NOTE jsonlPath BEFORE gitBranch (swapped vs
            // live); no relayPort / version / startedAt.
            str_opt(&mut m, "name", &s.name);
            m.insert("userNamed".into(), json!(s.user_named.unwrap_or(false)));
            m.insert("sessionId".into(), json!(s.session_id));
            opt_pid(&mut m, s.pid);
            m.insert("status".into(), json!(status_str));
            str_opt(&mut m, "zmxName", &s.zmx_name);
            opt_u32(&mut m, "zmxClients", s.zmx_clients);
            str_opt(&mut m, "socketDir", &s.socket_dir);
            m.insert("turns".into(), json!(s.turns));
            m.insert("tokens".into(), json!(s.tokens));
            str_opt(&mut m, "cwd", &s.cwd);
            opt_date(&mut m, "lastActive", s.last_active_ms);
            str_opt(&mut m, "jsonlPath", &s.jsonl_path);
            str_opt(&mut m, "gitBranch", &s.git_branch);
            opt_turns(&mut m, &s.last_turns);
            m.insert("provider".into(), json!(s.provider));
        }
        SessionBranch::ZmxOnly => {
            // session.ts:981-995 — NO userNamed; sessionId is "".
            str_opt(&mut m, "name", &s.name);
            m.insert("sessionId".into(), json!(s.session_id));
            opt_pid(&mut m, s.pid);
            m.insert("status".into(), json!(status_str));
            str_opt(&mut m, "zmxName", &s.zmx_name);
            opt_u32(&mut m, "zmxClients", s.zmx_clients);
            str_opt(&mut m, "socketDir", &s.socket_dir);
            m.insert("turns".into(), json!(s.turns));
            m.insert("tokens".into(), json!(s.tokens));
            str_opt(&mut m, "cwd", &s.cwd);
            opt_date(&mut m, "lastActive", s.last_active_ms);
            m.insert("provider".into(), json!(s.provider));
        }
        SessionBranch::Tombstoned => {
            // session.ts:1022-1038 — no zmx*/socketDir/relayPort.
            str_opt(&mut m, "name", &s.name);
            m.insert("userNamed".into(), json!(s.user_named.unwrap_or(false)));
            m.insert("sessionId".into(), json!(s.session_id));
            opt_pid(&mut m, s.pid);
            m.insert("status".into(), json!(status_str));
            m.insert("turns".into(), json!(s.turns));
            m.insert("tokens".into(), json!(s.tokens));
            str_opt(&mut m, "cwd", &s.cwd);
            opt_date(&mut m, "lastActive", s.last_active_ms);
            str_opt(&mut m, "version", &s.version);
            opt_date(&mut m, "startedAt", s.started_at_ms);
            str_opt(&mut m, "gitBranch", &s.git_branch);
            str_opt(&mut m, "jsonlPath", &s.jsonl_path);
            opt_turns(&mut m, &s.last_turns);
            m.insert("provider".into(), json!(s.provider));
        }
    }

    // D1 / X3 (`doc/tbd/provider-architecture/16-default-lane-switch.md`) ADDITIVE:
    // the row's LANE — the stable `<provider-id>/<hosting-token>` wire id
    // ([`quorum_qw::Lane::id`]) — derived through `lane_for(provider, hosting)`, the
    // SAME one-line derivation every acting verb uses. Deriving it here rather than
    // echoing the stored `hosting` string is the point: a consumer reading this key
    // and `qd kill`/`qd send` routing the same row cannot disagree, because both ask
    // the same function (which owns the absent⇒harness-default rule).
    //
    // WHY it exists at all: until now `qd ls` emitted `provider` and nothing about
    // hosting, which was legible only while every session of a harness sat in one
    // lane by default. Once codex defaults to `codex/app-server` (with `codex/daemon`
    // still reachable via `--daemon`) a user has two codex lanes on one listing and
    // no way to tell them apart. This key is what makes a mixed-lane world legible.
    //
    // WHY `lane` and not a bare `hosting` token (outstanding call O5, §5): the lane
    // id is ALREADY the stable identifier — it is what `qw`'s wire takes on
    // `{"m":"start","lane":…}` and what `Lane::from_id` round-trips — and it reads
    // correctly for `acp/*`, whose provider id itself contains a slash. A bare token
    // would force every consumer to re-join it against `provider` and re-derive the
    // absent-means-default rule for itself.
    //
    // OMITTED — never null, never guessed — when `lane_for` answers `None`, which
    // happens for exactly one input: a provider id qd cannot place. Same
    // absent-not-null contract as `qdId`/`lineage`, and the same answer every acting
    // verb gives an unplaceable row (it refuses rather than inventing a topology).
    // Inserted straight after the branch keys so it reads next to `provider`, and
    // before `code`, which stays LAST.
    if let Some(lane) = quorum_qw::lane_for(&s.provider, s.hosting.as_deref()) {
        m.insert("lane".into(), Value::String(lane.id()));
    }

    // A6 ADDITIVE (spec §4.4): backend / spawnedBy from the telemetry fold, ONLY
    // when the fold yields a value for THIS session. Absent fold or absent values
    // → these keys are not emitted → byte-identical to the base ls --json. Placed
    // before `code` so `code` stays LAST (assignShortCodes ordering). These are
    // FRESH A6 fields, NOT part of the byte-faithful TS surface (additive-only).
    if let Some(folded) = fold.and_then(|f| f.lookup(&s.session_id, s.name.as_deref())) {
        if let Some(backend) = &folded.backend {
            m.insert("backend".into(), Value::String(backend.clone()));
        }
        if let Some(spawned_by) = &folded.spawned_by {
            m.insert("spawnedBy".into(), Value::String(spawned_by.clone()));
        }
    }

    // P0 wave-1 ADDITIVE: the stable id (full 8-char) + its shortest-unique
    // prefix among the listed rows, ONLY when the session has a mapped id —
    // sessions with no id (no session_id / not yet minted) gain no keys, so
    // id-less fixtures render today's exact bytes. Placed before `code` so
    // `code` stays LAST (back-compat: external consumers parse `code`).
    if let Some(qd_id) = &s.qd_id {
        m.insert("qdId".into(), Value::String(qd_id.clone()));
        if let Some(prefix) = id_prefixes.get(qd_id) {
            m.insert("qdIdPrefix".into(), Value::String(prefix.clone()));
        }
    }

    // WP-B7 PIECE 2 (B5-iii obl-4 OUTPUT field) ADDITIVE: a FORK's lineage pointer
    // — STRICTLY the PARENT instance's qdId — emitted ONLY when this row is a fork
    // (`lineage` is `Some`). A non-fork row gains no key, so the no-lineage case is
    // BYTE-IDENTICAL to before (the same additive-when-present precedent as
    // `qdId`/`backend`/`spawnedBy`). The fork's OWN id stays `qdId` (no parent-id
    // leak into the fork's own identity). Placed before `code` so `code` stays LAST.
    if let Some(lineage) = &s.lineage {
        m.insert("lineage".into(), Value::String(lineage.clone()));
    }

    // WP-B-CS-2 ADDITIVE: the readiness facet (D3 ready/silent/stuck), ONLY when
    // the verbs layer supplied one for this row (a live, classifiable row). A
    // fresh field, NOT part of the byte-faithful TS surface — absent ⇒ no key ⇒
    // byte-identical to before, and the `status` field above is untouched. Placed
    // before `code` so `code` stays LAST.
    if let Some(word) = readiness {
        m.insert("readiness".into(), Value::String(word.to_string()));
    }

    // code LAST (assignShortCodes mutates s.code AFTER construction → a NEW
    // property lands last in JS key order). Omitted only if codes weren't
    // assigned (None); in the ls --json path codes are always assigned.
    if let Some(code) = &s.code {
        m.insert("code".into(), Value::String(code.clone()));
    }

    Value::Object(m)
}

/// A stray row (spec §7). PROVISIONAL / pass-(b)-regenerate-friendly:
/// this entire shape is frozen by the fixture and EXPECTED to be reworked at
/// pass (b). Strays render as `status: "unmanaged"` objects appended AFTER the
/// TS-faithful rows; they are NOT part of the byte-faithful TS surface.
///
/// Frozen key order: sessionId, pid, status, turns, tokens, cwd?, lastActive,
/// jsonlPath, provider, activeRecent, code (LAST). `name` is intentionally
/// ABSENT (a stray is unmanaged → unnamed). `cwd` is omitted when unknown.
fn stray_to_value(s: &Stray) -> Value {
    let mut m = Map::new();
    m.insert("sessionId".into(), json!(s.session_id));
    if let Some(pid) = s.pid {
        m.insert("pid".into(), json!(pid));
    }
    m.insert("status".into(), Value::String("unmanaged".into()));
    m.insert("turns".into(), json!(0));
    m.insert("tokens".into(), json!(0));
    // cwd: derived from the project dir slug is lossy; we expose the jsonlPath
    // instead and omit cwd (kept simple + frozen).
    m.insert(
        "lastActive".into(),
        Value::String(epoch_ms_to_iso(s.mtime_ms)),
    );
    m.insert(
        "jsonlPath".into(),
        Value::String(s.jsonl_path.to_string_lossy().into_owned()),
    );
    // codex P1, R1 (codex-p1-spec section 3.2): KEEP the literal — strays are
    // claude-transcript strays; no provider source exists on a stray.
    m.insert("provider".into(), Value::String("claude-code".into()));
    m.insert("activeRecent".into(), json!(s.active_recent));
    // code LAST (per-stray, for addressing parity with sessions). PROVISIONAL:
    // computed locally (the offset-0 short code of the session id) rather than
    // through the cross-session collision-avoidance of `codes::assign_short_codes`
    // — strays are appended after coding the TS rows, so a clean cross-pass would
    // need model.rs changes we deliberately avoid. Pass-(b) may unify this.
    m.insert(
        "code".into(),
        Value::String(stray_short_code(&s.session_id)),
    );
    Value::Object(m)
}

/// lsview A4 — a BARE (outside-qd) non-claude harness process row, appended
/// AFTER the session rows in the DEFAULT `qd ls --json` view (visibility, not
/// adoption). Carries the R2-established best-effort identity: `provider`, `pid`,
/// and `cwd` when the `lsof` enrichment succeeded (omitted otherwise — a
/// detectable-but-unidentifiable proc still renders). `"bare": true` marks it as
/// a process-detected row distinct from a session or a stray; `status` reuses the
/// stray "unmanaged" vocabulary. ACTING verbs never see this surface — it is
/// never folded into the session list, so refusal semantics are unchanged.
pub fn bare_proc_to_value(b: &crate::effects::BareProc) -> Value {
    let mut m = Map::new();
    m.insert("provider".into(), Value::String(b.provider.clone()));
    m.insert("pid".into(), json!(b.pid));
    m.insert("status".into(), Value::String("unmanaged".into()));
    if let Some(cwd) = &b.cwd {
        m.insert("cwd".into(), Value::String(cwd.clone()));
    }
    m.insert("bare".into(), json!(true));
    Value::Object(m)
}

/// Offset-0 short code for a stray's session id (sha256 → big-endian u32 →
/// base36, first 3 chars right-padded with '0'). Mirrors `codes::short_code_at`
/// at offset 0; kept local so render does not widen the `codes` API or touch the
/// shared M1 file. PROVISIONAL / pass-(b)-regenerate-friendly.
fn stray_short_code(session_id: &str) -> String {
    use sha2::{Digest, Sha256};
    if session_id.is_empty() {
        return "---".to_string();
    }
    let hash = Sha256::digest(session_id.as_bytes());
    let num = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]);
    // base36 lowercase, no leading zeros, "0" for zero.
    let mut base36 = if num == 0 {
        "0".to_string()
    } else {
        const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut n = num;
        let mut buf = Vec::new();
        while n > 0 {
            buf.push(DIGITS[(n % 36) as usize]);
            n /= 36;
        }
        buf.reverse();
        String::from_utf8(buf).expect("base36 digits are ASCII")
    };
    base36.truncate(3);
    while base36.len() < 3 {
        base36.push('0');
    }
    base36
}

// --- info --json (P0 spec-w8) ---

/// P0 `qd info <target> --json`: ONE json object for the RESOLVED session — the
/// point-resolution surface an outside consumer joins against (liveness/names/ids), promised to
/// P1 as this EXACT field list. Shape follows the `ls --json` conventions:
/// camelCase keys; `qdId`/`qdIdPrefix` ABSENT (not null) when the session has
/// no mapped stable id. That absence is a LIFECYCLE state, not an error
/// (B5 item 12 doc note, accepted r2): ids are minted at `qd start` and bound
/// at boot-confirm, and `qd ls` lazily backfills pre-existing sessions — so a
/// session can legitimately surface here BEFORE its id exists
/// (absent-until-minted; resolution stays engine-side per spec §3). Consumers
/// must treat a missing `qdId` as "not yet minted". Unlike the ls rows, `name`
/// and `pid` are EXPLICIT `null` when absent (the spec'd contract:
/// `"name": "wk" | null`, `"pid": 123 | null`).
///
/// - `prefixes` is the shortest-unique prefix map computed among the resolved
///   session LIST (`idstore::prefix_map`, the same computation ls uses) —
///   `qdIdPrefix` is emitted only when `qdId` is and the map has it.
/// - `live` is computed by the caller via `resolve::is_live_with_pid` (the pid
///   check needs the `is_pid_alive` effects seam; this render fn stays pure).
/// - A pid of `0` means "no pid recorded" (the engine-wide convention, see
///   `is_live_with_pid`) and renders as `null`, matching what `live` consulted.
///
/// Key order: name, sessionId, qdId?, qdIdPrefix?, status, live, pid,
/// provider, lane?, jsonlPath?. `jsonlPath` follows the same absent-not-null
/// convention as `qdId` (omitted, never `null`, when the session has no
/// resolved transcript path) — mirrors `ls --json`'s `session_to_value`,
/// which reads the same `Session.jsonl_path` field (persist-relocation:
/// frame's engine adapter reads this to locate the transcript to copy,
/// without dispatch ever re-deriving the path a second time).
pub fn info_json(
    s: &Session,
    prefixes: &std::collections::HashMap<String, String>,
    live: bool,
) -> Value {
    let mut m = Map::new();
    m.insert(
        "name".into(),
        s.name
            .as_ref()
            .map(|n| Value::String(n.clone()))
            .unwrap_or(Value::Null),
    );
    m.insert("sessionId".into(), json!(s.session_id));
    if let Some(qd_id) = &s.qd_id {
        m.insert("qdId".into(), Value::String(qd_id.clone()));
        if let Some(prefix) = prefixes.get(qd_id) {
            m.insert("qdIdPrefix".into(), Value::String(prefix.clone()));
        }
    }
    m.insert("status".into(), json!(s.status.as_str()));
    m.insert("live".into(), json!(live));
    m.insert(
        "pid".into(),
        match s.pid {
            Some(p) if p != 0 => json!(p),
            _ => Value::Null,
        },
    );
    m.insert("provider".into(), json!(s.provider));
    // The row's LANE, on the same terms `ls --json` emits it (see
    // `session_to_value`): derived through `lane_for`, absent-never-null when the
    // provider cannot be placed, and sitting directly after `provider`.
    //
    // It was missing here while `provider` alone identified a session's carrier.
    // It no longer does — `claude-code` names two lanes, one delivered over a PTY
    // composer and one over an ACP bridge — so a consumer choosing a transport
    // from `provider` is choosing from a field that cannot answer. `qf` is that
    // consumer (`frame/src/delivery.rs`), it reads THIS object, and this key is
    // what lets it route on the lane rather than guess from the harness name.
    if let Some(lane) = quorum_qw::lane_for(&s.provider, s.hosting.as_deref()) {
        m.insert("lane".into(), Value::String(lane.id()));
    }
    if let Some(jsonl_path) = &s.jsonl_path {
        m.insert("jsonlPath".into(), Value::String(jsonl_path.clone()));
    }
    Value::Object(m)
}

fn opt_pid(m: &mut Map<String, Value>, pid: Option<i64>) {
    if let Some(pid) = pid {
        m.insert("pid".into(), json!(pid));
    }
}

fn opt_u32(m: &mut Map<String, Value>, k: &str, v: Option<u32>) {
    if let Some(v) = v {
        m.insert(k.into(), json!(v));
    }
}

fn opt_u16(m: &mut Map<String, Value>, k: &str, v: Option<u16>) {
    if let Some(v) = v {
        m.insert(k.into(), json!(v));
    }
}

fn opt_date(m: &mut Map<String, Value>, k: &str, ms: Option<i64>) {
    if let Some(ms) = ms {
        m.insert(k.into(), Value::String(epoch_ms_to_iso(ms)));
    }
}

fn opt_turns(m: &mut Map<String, Value>, turns: &Option<Vec<TurnPreview>>) {
    if let Some(turns) = turns {
        let arr: Vec<Value> = turns.iter().map(turn_to_value).collect();
        m.insert("lastTurns".into(), Value::Array(arr));
    }
}

/// TurnPreview → {role, text, timestamp?} — key order role,text,timestamp
/// (session.ts:524-528/536-541). `timestamp` omitted when None (TS undefined).
fn turn_to_value(t: &TurnPreview) -> Value {
    let mut m = Map::new();
    m.insert("role".into(), Value::String(t.role.to_string()));
    m.insert("text".into(), Value::String(t.text.clone()));
    if let Some(ts) = &t.timestamp {
        m.insert("timestamp".into(), Value::String(ts.clone()));
    }
    Value::Object(m)
}

// --- Date formatting ---
//
// MOVED to `quorum_core::timefmt` (qd/qw split): `epoch_ms_to_iso`,
// `epoch_ms_to_amz_date`, `epoch_ms_to_en_us_locale` and the civil-calendar math
// under them. They were never presentation — their consumers stamp WIRE fields:
// `events` (the ledger Envelope.ts), `idstore` (the ids.jsonl mint log),
// `telemetry` (marks.jsonl), `relay_server`, and `archive` (the SigV4
// x-amz-date header). Only `info_text` here was ever user-facing.
//
// The telemetry consumer is why this mattered rather than being tidy-up:
// `telemetry` belongs to qw, so `use crate::render::epoch_ms_to_iso` would have
// become a qw -> qd edge the moment it moved — a violation of the one-way rule,
// latent only because both modules happened to share a crate.
//
// Re-exported below so this module's own callers are unchanged.
pub use quorum_core::timefmt::{epoch_ms_to_amz_date, epoch_ms_to_en_us_locale, epoch_ms_to_iso};

// --- info ---

/// Port of the `qd info` text body (status.ts:621-660). Literal column alignment
/// copied from the TS source; conditional lines emitted only when present.
///
/// `now_ms` is the injected clock (for the `relativeTime` suffix on Last active).
pub fn info_text(session: &Session, now_ms: i64) -> String {
    info_text_with_fold(session, now_ms, None)
}

/// As [`info_text`], but with an OPTIONAL A6 telemetry fold (spec §4.4). When
/// `fold` is `None` (or has no values for this session) the output is
/// BYTE-IDENTICAL to [`info_text`] — the additive `Backend:` / `Spawned by:`
/// lines appear ONLY when the fold yields values. The render fn stays pure.
pub fn info_text_with_fold(session: &Session, now_ms: i64, fold: Option<&SnapshotMap>) -> String {
    info_text_full(session, now_ms, fold, &DiscoveryHealth::default())
}

/// As [`info_text_with_fold`], but told WHICH discovery reads failed during the
/// gather that produced `session`.
///
/// `qd info` renders a missing `Pane`/`Relay` as `-`, which reads as "this
/// session has none". When the read that would have found one was refused, that
/// is a claim `qd` never established — `Pane: -` and `Relay: -` are exactly the
/// ambiguity that makes a denied `ps` look like a session with no carrier. With
/// a degraded health those two lines render `unknown (ps unavailable)` instead.
///
/// A clean [`DiscoveryHealth`] (the default, and every non-sandboxed run) is
/// BYTE-IDENTICAL to [`info_text_with_fold`] — the parity goldens are untouched.
pub fn info_text_full(
    session: &Session,
    now_ms: i64,
    fold: Option<&SnapshotMap>,
    health: &DiscoveryHealth,
) -> String {
    let mut out = String::new();
    let push = |out: &mut String, line: String| {
        out.push_str(&line);
        out.push('\n');
    };

    // Name / Session ID / PID / Status (status.ts:621-624).
    push(
        &mut out,
        format!(
            "Name:        {}",
            session.name.as_deref().unwrap_or("(unnamed)")
        ),
    );
    push(
        &mut out,
        format!(
            "Session ID:  {}",
            if session.session_id.is_empty() {
                "-"
            } else {
                session.session_id.as_str()
            }
        ),
    );
    // P0 wave-1 ADDITIVE: the engine-minted stable id, ONLY when mapped —
    // id-less sessions render today's exact bytes (the parity goldens carry no
    // ids, so they are untouched).
    if let Some(qd_id) = &session.qd_id {
        push(&mut out, format!("Stable ID:   {qd_id}"));
    }
    push(
        &mut out,
        format!(
            "PID:         {}",
            // TS `session.pid || "-"`: 0 is falsy → "-".
            match session.pid {
                Some(p) if p != 0 => p.to_string(),
                _ => "-".to_string(),
            }
        ),
    );
    push(
        &mut out,
        format!("Status:      {}", session.status.as_str()),
    );

    // Provider (status.ts:625-628). codex P1, R1 (codex-p1-spec section 3.2):
    // render from `s.provider` — byte-identical for every constructible row
    // today (all rows resolve to claude-code; the render.rs info_text test pins
    // it), but a future non-claude row now renders its real value.
    push(&mut out, format!("Provider:    {}", session.provider));

    // Pane line (status.ts:629-631, whose label was `zmx`). FTUE punch R1
    // (zmx retirement) follow-through: the VALUE is the mux pane's name and its
    // attach state, and this line prints under the embedded qrmux default as
    // well as the `QD_MUX=zmx` hatch — so the label names the pane, not the
    // backend. A NAMED divergence from the TS corpus (ADR-0011); the
    // `info-alpha.txt` golden was re-minted for it.
    let pane_line = match &session.zmx_name {
        Some(name) => {
            let attached = session.zmx_clients.unwrap_or(0) > 0;
            format!(
                "{} ({})",
                name,
                if attached { "attached" } else { "detached" }
            )
        }
        // A refused mux list did not observe an absence — do not render one.
        None => DiscoveryHealth::unknown_label(&health.mux_list).unwrap_or_else(|| "-".to_string()),
    };
    push(&mut out, format!("Pane:        {pane_line}"));

    // Pane dir — the mux SOCKET dir, only when socketDir present
    // (status.ts:632-634, whose label was `zmx dir`).
    if let Some(dir) = &session.socket_dir {
        push(&mut out, format!("Pane dir:    {dir}"));
    }

    // Relay (status.ts:635).
    let relay_line = match session.relay_port {
        Some(port) => format!("localhost:{port}"),
        // `relay_port` is resolved by walking the `ps` ancestry from each relay
        // sidecar. With no process table there is no walk, hence no answer —
        // which is not the same answer as "no relay".
        None => {
            DiscoveryHealth::unknown_label(&health.process_table).unwrap_or_else(|| "-".to_string())
        }
    };
    push(&mut out, format!("Relay:       {relay_line}"));

    // Turns / Tokens / CWD (status.ts:636-638).
    push(&mut out, format!("Turns:       {}", session.turns));
    push(
        &mut out,
        format!("Tokens:      {}", crate::fmt::format_tokens(session.tokens)),
    );
    push(
        &mut out,
        format!("CWD:         {}", session.cwd.as_deref().unwrap_or("-")),
    );

    // Git branch / Version — conditional (status.ts:639-644).
    if let Some(branch) = &session.git_branch {
        push(&mut out, format!("Git branch:  {branch}"));
    }
    if let Some(version) = &session.version {
        push(&mut out, format!("Version:     {version}"));
    }

    // Started / Last active — conditional; toLocaleString lines are
    // NORMALIZATION-CLASS (status.ts:645-652).
    if let Some(started) = session.started_at_ms {
        push(
            &mut out,
            format!("Started:     {}", epoch_ms_to_en_us_locale(started)),
        );
    }
    if let Some(last) = session.last_active_ms {
        push(
            &mut out,
            format!(
                "Last active: {} ({})",
                epoch_ms_to_en_us_locale(last),
                crate::fmt::relative_time(last, now_ms)
            ),
        );
    }

    // A6 ADDITIVE (spec §4.4): Backend / Spawned by from the telemetry fold, ONLY
    // when the fold yields a value for THIS session. Absent fold or absent values
    // → no lines emitted → byte-identical to base info (G-A1 negative control).
    // FRESH A6 surface, NOT part of the byte-faithful TS info body.
    if let Some(folded) = fold.and_then(|f| f.lookup(&session.session_id, session.name.as_deref()))
    {
        if let Some(backend) = &folded.backend {
            push(&mut out, format!("Backend:     {backend}"));
        }
        if let Some(spawned_by) = &folded.spawned_by {
            push(&mut out, format!("Spawned by:  {spawned_by}"));
        }
    }

    // Recent conversation block (status.ts:654-661).
    if let Some(turns) = &session.last_turns {
        if !turns.is_empty() {
            out.push('\n');
            push(&mut out, "── Recent conversation ──".to_string());
            for t in turns {
                let prefix = if t.role == "user" { "You:" } else { "Claude:" };
                // newlines → spaces, slice 120, "…" suffix when the ORIGINAL was
                // longer than 120 (TS `t.text.length > 120`).
                let replaced: String = t.text.replace('\n', " ");
                let sliced: String = replaced.chars().take(120).collect();
                let suffix = if t.text.chars().count() > 120 {
                    "…"
                } else {
                    ""
                };
                push(&mut out, format!("  {prefix} {sliced}{suffix}"));
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SessionStatus, TurnPreview};

    fn base(branch: SessionBranch) -> Session {
        Session {
            name: None,
            user_named: None,
            session_id: String::new(),
            code: Some("abc".into()),
            qd_id: None,
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
            which_branch: branch,
        }
    }

    // --- WP-B-CS-2 readiness facet (additive; status byte-unchanged) ---

    /// §6 DoD — the readiness facet renders ADDITIVELY (a new `readiness` key from
    /// the D3 triad), the existing `status` field is BYTE-UNCHANGED, and a `None`
    /// facet for a row emits no key. Asserts the facet lands LAST-but-one (before
    /// `code`) and never touches `status`.
    #[test]
    fn readiness_facet_is_additive_and_leaves_status_unchanged() {
        let mut s = base(SessionBranch::LiveRegistry);
        s.status = SessionStatus::Busy;
        s.session_id = "sess-x".into();

        // No facet → byte-identical to the base ls --json (status present, no
        // readiness key).
        let without = to_pretty(&ls_json_full(std::slice::from_ref(&s), &[], None, None));
        let with = to_pretty(&ls_json_full(
            std::slice::from_ref(&s),
            &[],
            None,
            Some(&[Some("ready")]),
        ));

        // The `status` field is byte-identical in both renderings.
        let status_line = "\"status\": \"busy\"";
        assert!(
            without.contains(status_line),
            "base must carry status: {without}"
        );
        assert!(
            with.contains(status_line),
            "facet must NOT alter status: {with}"
        );
        // The facet appears ONLY in the with-facet rendering.
        assert!(
            !without.contains("readiness"),
            "no facet ⇒ no key: {without}"
        );
        assert!(
            with.contains("\"readiness\": \"ready\""),
            "facet renders: {with}"
        );
        // And it precedes `code` (LAST-key discipline).
        let r = with.find("\"readiness\"").unwrap();
        let c = with.find("\"code\"").unwrap();
        assert!(r < c, "readiness precedes code: {with}");

        // A None facet in the slice (a not-alive/ungated row) emits no key.
        let with_none = to_pretty(&ls_json_full(
            std::slice::from_ref(&s),
            &[],
            None,
            Some(&[None]),
        ));
        assert_eq!(
            with_none, without,
            "a None facet ⇒ byte-identical to no facet"
        );
    }

    // --- date formatters (verified vs bun) ---

    #[test]
    fn iso_matches_bun() {
        // bun: new Date(1717495200000).toJSON() → 2024-06-04T10:00:00.000Z
        assert_eq!(
            epoch_ms_to_iso(1_717_495_200_000),
            "2024-06-04T10:00:00.000Z"
        );
        // epoch 0.
        assert_eq!(epoch_ms_to_iso(0), "1970-01-01T00:00:00.000Z");
        // ms precision.
        assert_eq!(
            epoch_ms_to_iso(1_717_495_200_123),
            "2024-06-04T10:00:00.123Z"
        );
        // leap day 2024-02-29 12:30:45.678.
        assert_eq!(
            epoch_ms_to_iso(1_709_209_845_678),
            "2024-02-29T12:30:45.678Z"
        );
    }

    /// N2 (M1 residual → M5/T4): the qrmux mux emitter carries a HAND-PORTED copy
    /// of `epoch_ms_to_iso` (`qrmux::attended::emitter::epoch_ms_to_iso`) — the mux
    /// is leaf-crate-free and cannot import `dispatch::render`, so the two are
    /// character-identical BY MAINTENANCE, not by a shared definition. This
    /// differential test pins them byte-equal across a wide input sweep (epoch,
    /// pre-epoch/negative, ms sub-second boundaries, leap days, century/era edges,
    /// far future) so a future edit to either copy that drifts the emitted `ts`
    /// bytes — a byte-identity break in the delivery ledger — fails HERE loudly.
    #[test]
    fn epoch_ms_to_iso_is_byte_equal_to_the_qrmux_mux_emitter_copy() {
        // Fixed, representative instants (not random — deterministic gate).
        let fixed: &[i64] = &[
            0,                   // epoch
            -1,                  // one ms pre-epoch (div_euclid/rem_euclid arm)
            1,                   // one ms post-epoch
            999,                 // sub-second boundary
            1000,                // exact second
            -1000,               // exact second pre-epoch
            -86_400_000,         // one day pre-epoch
            1_709_209_845_678,   // leap day 2024-02-29T12:30:45.678Z
            1_717_495_200_123,   // 2024-06-04T10:00:00.123Z
            951_782_400_000,     // 2000-02-29 (leap century)
            4_102_444_800_000,   // 2100-01-01 (non-leap century)
            -2_208_988_800_000,  // 1900-01-01
            253_402_300_799_999, // 9999-12-31T23:59:59.999Z (far future)
        ];
        for &ms in fixed {
            assert_eq!(
                epoch_ms_to_iso(ms),
                qrmux::attended::emitter::epoch_ms_to_iso(ms),
                "epoch_ms_to_iso drift at ms={ms}: dispatch::render vs qrmux mux emitter"
            );
        }
        // A dense stride sweep (every ~7h over ~40 years, straddling many
        // month/leap/DST-irrelevant-UTC boundaries) to catch a civil-math drift the
        // fixed points might miss.
        let mut ms: i64 = -1_000_000_000_000;
        let end: i64 = 1_500_000_000_000;
        let step: i64 = 25_000_000; // ~6.9h
        while ms < end {
            assert_eq!(
                epoch_ms_to_iso(ms),
                qrmux::attended::emitter::epoch_ms_to_iso(ms),
                "epoch_ms_to_iso drift at ms={ms} (sweep)"
            );
            ms += step;
        }
    }

    #[test]
    fn amz_date_strips_separators_and_millis() {
        // Same instants as `iso_matches_bun` — the amz-date form is the ISO
        // form with dashes/colons/millis stripped.
        assert_eq!(epoch_ms_to_amz_date(1_717_495_200_000), "20240604T100000Z");
        assert_eq!(epoch_ms_to_amz_date(0), "19700101T000000Z");
        // ms precision is dropped, not rounded.
        assert_eq!(epoch_ms_to_amz_date(1_717_495_200_123), "20240604T100000Z");
        assert_eq!(epoch_ms_to_amz_date(1_709_209_845_678), "20240229T123045Z");
    }

    #[test]
    fn locale_matches_bun_en_us_utc() {
        // bun en-US/UTC: 1717530000000 → 6/4/2024, 7:40:00 PM
        assert_eq!(
            epoch_ms_to_en_us_locale(1_717_530_000_000),
            "6/4/2024, 7:40:00 PM"
        );
        // midnight → 12 AM (bun: 6/4/2024, 12:00:00 AM at Date.UTC(2024,5,4,0,0,0)).
        assert_eq!(
            epoch_ms_to_en_us_locale(1_717_459_200_000),
            "6/4/2024, 12:00:00 AM"
        );
        // 9:07:03 AM single-digit hour, padded min/sec (bun: 1/5/2024, 9:07:03 AM).
        assert_eq!(
            epoch_ms_to_en_us_locale(1_704_445_623_000),
            "1/5/2024, 9:07:03 AM"
        );
        // 11:59 PM (bun: 12/25/2024, 11:59:00 PM).
        assert_eq!(
            epoch_ms_to_en_us_locale(1_735_171_140_000),
            "12/25/2024, 11:59:00 PM"
        );
    }

    // --- ls_json key order + omission ---

    #[test]
    fn acp_status_override_truthful_in_json_nonacp_byte_identical() {
        // (L) Item 3: an acp row's JSON `status` shows the PRIMARY-SOURCED override
        // (here "stopped") instead of the stale stored "busy"; a None entry / a non-acp
        // row is BYTE-IDENTICAL to the no-override JSON (the parity contract).
        let mut acp = base(SessionBranch::LiveRegistry);
        acp.name = Some("acp-wk".into());
        acp.session_id = "sess-acp".into();
        acp.status = SessionStatus::Busy; // stale stored
        acp.provider = "acp/claude-code".into();

        // With the override → status reads "stopped" (the truthful probe verdict).
        let overridden = ls_json_full_acp(
            std::slice::from_ref(&acp),
            &[],
            None,
            None,
            &[Some("stopped".to_string())],
        );
        assert_eq!(
            overridden.as_array().unwrap()[0]["status"],
            json!("stopped")
        );

        // No override (empty slice) → byte-identical to plain ls_json (stored "busy").
        let plain = ls_json(std::slice::from_ref(&acp), &[]);
        let none_override = ls_json_full_acp(std::slice::from_ref(&acp), &[], None, None, &[None]);
        assert_eq!(
            to_pretty(&none_override),
            to_pretty(&plain),
            "None override == stored bytes"
        );
        assert_eq!(plain.as_array().unwrap()[0]["status"], json!("busy"));
    }

    #[test]
    fn live_branch_key_order_and_omission() {
        let mut s = base(SessionBranch::LiveRegistry);
        s.name = Some("worker".into());
        s.user_named = Some(true);
        s.session_id = "sess-1".into();
        s.pid = Some(4242);
        s.status = SessionStatus::Busy;
        s.turns = 5;
        s.tokens = 100;
        s.last_active_ms = Some(1_717_495_200_000);
        s.started_at_ms = Some(1_717_495_100_000);
        // zmxName/socketDir/cwd/version/gitBranch/jsonlPath/lastTurns left None →
        // omitted.
        let v = ls_json(&[s], &[]);
        let obj = v.as_array().unwrap()[0].as_object().unwrap();
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "name",
                "userNamed",
                "sessionId",
                "pid",
                "status",
                "turns",
                "tokens",
                "lastActive",
                "startedAt",
                "provider",
                // D1 / X3: the derived lane, next to the provider it refines. A
                // claude-code row with no recorded hosting still carries one —
                // `lane_for` resolves the absent token to the harness default —
                // because "the lane is data" is exactly what this key is for.
                "lane",
                "code", // LAST
            ]
        );
        assert_eq!(obj["lane"], json!("claude-code/mux-pane"));
        assert_eq!(obj["status"], json!("busy"));
        assert_eq!(obj["lastActive"], json!("2024-06-04T10:00:00.000Z"));
        assert_eq!(obj["code"], json!("abc"));
    }

    #[test]
    fn cold_branch_jsonl_before_gitbranch() {
        let mut s = base(SessionBranch::ColdJsonl);
        s.name = Some("c".into());
        s.user_named = Some(false);
        s.session_id = "cold-1".into();
        s.status = SessionStatus::Cold;
        s.jsonl_path = Some("/p/cold-1.jsonl".into());
        s.git_branch = Some("main".into());
        s.last_active_ms = Some(0);
        let v = ls_json(&[s], &[]);
        let obj = v.as_array().unwrap()[0].as_object().unwrap();
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        // jsonlPath must come BEFORE gitBranch in the cold branch.
        let ji = keys.iter().position(|k| *k == "jsonlPath").unwrap();
        let gi = keys.iter().position(|k| *k == "gitBranch").unwrap();
        assert!(ji < gi, "cold: jsonlPath before gitBranch");
    }

    #[test]
    fn zmx_only_omits_user_named_and_emits_empty_session_id() {
        let mut s = base(SessionBranch::ZmxOnly);
        s.name = Some("z".into());
        s.session_id = String::new();
        s.pid = Some(99);
        s.status = SessionStatus::Cold;
        s.zmx_name = Some("z".into());
        s.zmx_clients = Some(0);
        s.last_active_ms = Some(0);
        let v = ls_json(&[s], &[]);
        let obj = v.as_array().unwrap()[0].as_object().unwrap();
        assert!(!obj.contains_key("userNamed"), "ZmxOnly omits userNamed");
        assert_eq!(obj["sessionId"], json!(""));
    }

    #[test]
    fn empty_sessions_render_as_empty_array() {
        let v = ls_json(&[], &[]);
        assert_eq!(to_pretty(&v), "[]");
    }

    #[test]
    fn pretty_matches_bun_shape() {
        // bun: JSON.stringify([{a:1,b:[1,2]}],null,2) (verified in the task).
        let v = json!([{"a": 1, "b": [1, 2]}]);
        let expected = "[\n  {\n    \"a\": 1,\n    \"b\": [\n      1,\n      2\n    ]\n  }\n]";
        assert_eq!(to_pretty(&v), expected);
    }

    #[test]
    fn last_turns_key_order() {
        let mut s = base(SessionBranch::LiveRegistry);
        s.session_id = "s".into();
        s.last_turns = Some(vec![
            TurnPreview {
                role: "user",
                text: "hi".into(),
                timestamp: Some("2026-06-04T10:00:00.000Z".into()),
            },
            TurnPreview {
                role: "assistant",
                text: "yo".into(),
                timestamp: None,
            },
        ]);
        let v = ls_json(&[s], &[]);
        let turns = v.as_array().unwrap()[0]["lastTurns"].as_array().unwrap();
        let t0 = turns[0].as_object().unwrap();
        assert_eq!(
            t0.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["role", "text", "timestamp"]
        );
        let t1 = turns[1].as_object().unwrap();
        assert!(!t1.contains_key("timestamp"), "None timestamp omitted");
    }

    // --- stray render ---

    #[test]
    fn stray_renders_unmanaged_status_appended() {
        let s = base(SessionBranch::LiveRegistry);
        let stray = Stray {
            session_id: "stray-1".into(),
            jsonl_path: std::path::PathBuf::from("/p/stray-1.jsonl"),
            project_dir: "-p".into(),
            pid: Some(8888),
            mtime_ms: 0,
            active_recent: true,
        };
        let v = ls_json(&[s], &[stray]);
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let st = arr[1].as_object().unwrap();
        assert_eq!(st["status"], json!("unmanaged"));
        assert_eq!(st["pid"], json!(8888));
        assert_eq!(st["activeRecent"], json!(true));
        assert!(!st.contains_key("name"), "stray has no name");
        // code is LAST.
        assert_eq!(st.keys().next_back().map(String::as_str), Some("code"));
    }

    // --- info_text ---

    #[test]
    fn info_text_minimal() {
        let mut s = base(SessionBranch::LiveRegistry);
        s.name = Some("worker".into());
        s.session_id = "sess-1".into();
        s.pid = Some(4242);
        s.status = SessionStatus::Busy;
        s.turns = 3;
        s.tokens = 1500;
        let text = info_text(&s, 0);
        assert!(text.contains("Name:        worker\n"));
        assert!(text.contains("Session ID:  sess-1\n"));
        assert!(text.contains("PID:         4242\n"));
        assert!(text.contains("Status:      busy\n"));
        assert!(text.contains("Provider:    claude-code\n"));
        assert!(text.contains("Pane:        -\n"));
        assert!(text.contains("Relay:       -\n"));
        assert!(text.contains("Turns:       3\n"));
        assert!(text.contains("Tokens:      1.5k\n"));
        assert!(text.contains("CWD:         -\n"));
        // No socketDir → no "Pane dir" line.
        assert!(!text.contains("Pane dir:"));
    }

    // --- info_text degraded-discovery rendering ---

    fn denied_health() -> DiscoveryHealth {
        DiscoveryHealth {
            process_table: Some(crate::discovery::AcquireFailure::new(
                "ps",
                &std::io::Error::from_raw_os_error(libc::EPERM),
            )),
            ..Default::default()
        }
    }

    /// A clean health is the existing behavior, byte for byte — the parity
    /// goldens and every non-sandboxed run are untouched.
    #[test]
    fn info_text_full_with_clean_health_is_byte_identical() {
        let mut s = base(SessionBranch::LiveRegistry);
        s.name = Some("worker".into());
        s.session_id = "sess-1".into();
        s.relay_port = None;
        s.zmx_name = None;
        assert_eq!(
            info_text_full(&s, 0, None, &DiscoveryHealth::default()),
            info_text(&s, 0),
            "clean health must not alter a single byte"
        );
    }

    /// `Relay: -` asserts "this session has no relay". When the `ps` walk that
    /// resolves the port was REFUSED, that claim was never established — so the
    /// line must report the ambiguity instead of resolving it the wrong way.
    #[test]
    fn a_refused_process_table_renders_relay_as_unknown_not_absent() {
        let mut s = base(SessionBranch::LiveRegistry);
        s.name = Some("worker".into());
        s.relay_port = None;
        let text = info_text_full(&s, 0, None, &denied_health());
        assert!(
            text.contains("Relay:       unknown (ps unavailable)\n"),
            "{text}"
        );
        assert!(
            !text.contains("Relay:       -\n"),
            "an unread field must never render as a confirmed absence: {text}"
        );
    }

    /// The same rule for the mux read and the `zmx` line.
    #[test]
    fn a_refused_mux_list_renders_zmx_as_unknown_not_absent() {
        let mut s = base(SessionBranch::LiveRegistry);
        s.name = Some("worker".into());
        s.zmx_name = None;
        let health = DiscoveryHealth {
            mux_list: Some(crate::discovery::AcquireFailure::new(
                "mux list",
                &std::io::Error::from_raw_os_error(libc::EPERM),
            )),
            ..Default::default()
        };
        let text = info_text_full(&s, 0, None, &health);
        assert!(
            text.contains("Pane:        unknown (mux list unavailable)\n"),
            "{text}"
        );
    }

    /// Degradation only rewrites the fields whose OWN read failed. A relay port
    /// that WAS resolved still renders its value, and an unaffected field keeps
    /// its ordinary absent rendering.
    #[test]
    fn degradation_only_rewrites_the_fields_whose_read_failed() {
        let mut s = base(SessionBranch::LiveRegistry);
        s.name = Some("worker".into());
        s.relay_port = Some(4312);
        s.zmx_name = None;
        // Only the process table was refused; the mux read succeeded.
        let text = info_text_full(&s, 0, None, &denied_health());
        assert!(text.contains("Relay:       localhost:4312\n"), "{text}");
        assert!(
            text.contains("Pane:        -\n"),
            "a successful read that found nothing still renders as absent: {text}"
        );
    }

    #[test]
    fn info_text_conditional_lines_and_recent() {
        let mut s = base(SessionBranch::LiveRegistry);
        s.name = Some("w".into());
        s.session_id = "s".into();
        s.pid = Some(1);
        s.socket_dir = Some("/tmp/zmx-501".into());
        s.zmx_name = Some("zw".into());
        s.zmx_clients = Some(2);
        s.relay_port = Some(8901);
        s.git_branch = Some("feature".into());
        s.version = Some("1.2.3".into());
        s.last_active_ms = Some(1_717_530_000_000);
        s.last_turns = Some(vec![TurnPreview {
            role: "user",
            text: "line one\nline two".into(),
            timestamp: None,
        }]);
        let text = info_text(&s, 1_717_530_001_000);
        assert!(text.contains("Pane:        zw (attached)\n"));
        assert!(text.contains("Pane dir:    /tmp/zmx-501\n"));
        assert!(text.contains("Relay:       localhost:8901\n"));
        assert!(text.contains("Git branch:  feature\n"));
        assert!(text.contains("Version:     1.2.3\n"));
        assert!(text.contains("── Recent conversation ──\n"));
        // newline replaced by space in the preview.
        assert!(text.contains("  You: line one line two\n"));
        // Last active locale + relativeTime suffix.
        assert!(text.contains("Last active: 6/4/2024, 7:40:00 PM (1s ago)\n"));
    }

    #[test]
    fn info_recent_truncates_at_120_with_ellipsis() {
        let mut s = base(SessionBranch::LiveRegistry);
        s.session_id = "s".into();
        let long = "x".repeat(150);
        s.last_turns = Some(vec![TurnPreview {
            role: "assistant",
            text: long,
            timestamp: None,
        }]);
        let text = info_text(&s, 0);
        let line = text.lines().find(|l| l.starts_with("  Claude:")).unwrap();
        // "  Claude: " + 120 chars + "…".
        assert!(line.ends_with('…'));
        let body = line.strip_prefix("  Claude: ").unwrap();
        assert_eq!(body.chars().count(), 121, "120 chars + ellipsis");
    }

    // --- A6 §4.4 surfacing: byte-identity negative controls + positive rows ---

    use crate::telemetry::{fold_marks, SnapshotMap};

    /// A LiveRegistry session fixture with a known sessionId.
    fn a6_session() -> Session {
        let mut s = base(SessionBranch::LiveRegistry);
        s.name = Some("worker".into());
        s.user_named = Some(true);
        s.session_id = "sess-a6".into();
        s.pid = Some(4242);
        s.status = SessionStatus::Busy;
        s.turns = 5;
        s.tokens = 100;
        s.last_active_ms = Some(1_717_495_200_000);
        s.started_at_ms = Some(1_717_495_100_000);
        s
    }

    #[test]
    fn ls_json_none_fold_byte_identical_to_base() {
        // G-A1 negative control: ls_json (no fold) == ls_json_with_fold(None).
        let s = a6_session();
        let base_out = to_pretty(&ls_json(std::slice::from_ref(&s), &[]));
        let none_out = to_pretty(&ls_json_with_fold(std::slice::from_ref(&s), &[], None));
        assert_eq!(
            base_out, none_out,
            "None fold must be byte-identical to base"
        );
    }

    #[test]
    fn ls_json_empty_fold_byte_identical_to_base() {
        // An EMPTY fold (e.g. missing marks.jsonl) also yields today's bytes —
        // the verbs layer passes None when the fold is_empty, but even a present
        // empty SnapshotMap with no matching session must add no keys.
        let s = a6_session();
        let base_out = to_pretty(&ls_json(std::slice::from_ref(&s), &[]));
        let empty = SnapshotMap::default();
        let folded_out = to_pretty(&ls_json_with_fold(
            std::slice::from_ref(&s),
            &[],
            Some(&empty),
        ));
        assert_eq!(base_out, folded_out);
    }

    #[test]
    fn info_text_none_and_empty_fold_byte_identical_to_base() {
        let s = a6_session();
        let base_out = info_text(&s, 0);
        assert_eq!(info_text_with_fold(&s, 0, None), base_out);
        let empty = SnapshotMap::default();
        assert_eq!(info_text_with_fold(&s, 0, Some(&empty)), base_out);
    }

    #[test]
    fn ls_json_emits_backend_and_spawned_by_only_when_folded() {
        // Build a fold from a create event for this session.
        let create = crate::telemetry::build_create_line(
            "t",
            &crate::telemetry::CreateEvent {
                name: "worker".into(),
                session_id: Some("sess-a6".into()),
                backend: Some("ccr-3456".into()),
                spawned_by: Some("orc".into()),
                ..Default::default()
            },
        );
        let fold = fold_marks(&format!("{create}\n"));
        let s = a6_session();
        let v = ls_json_with_fold(&[s], &[], Some(&fold));
        let obj = v.as_array().unwrap()[0].as_object().unwrap();
        assert_eq!(obj["backend"], json!("ccr-3456"));
        assert_eq!(obj["spawnedBy"], json!("orc"));
        // backend/spawnedBy land BEFORE code (code stays last).
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        let bi = keys.iter().position(|k| *k == "backend").unwrap();
        let ci = keys.iter().position(|k| *k == "code").unwrap();
        assert!(bi < ci, "backend before code");
    }

    #[test]
    fn info_text_emits_backend_and_spawned_by_lines_only_when_folded() {
        let create = crate::telemetry::build_create_line(
            "t",
            &crate::telemetry::CreateEvent {
                name: "worker".into(),
                session_id: Some("sess-a6".into()),
                backend: Some("ccr-3456".into()),
                spawned_by: Some("orc".into()),
                ..Default::default()
            },
        );
        let fold = fold_marks(&format!("{create}\n"));
        let s = a6_session();
        let text = info_text_with_fold(&s, 0, Some(&fold));
        assert!(text.contains("Backend:     ccr-3456\n"));
        assert!(text.contains("Spawned by:  orc\n"));
    }

    // --- P0 wave-1: qdId / qdIdPrefix additive surfacing ---

    #[test]
    fn ls_json_emits_qd_id_and_prefix_only_when_mapped() {
        // Two rows share a 2-char id prefix → "ab3"/"ab4"; a third row has no
        // qd_id → NO qdId/qdIdPrefix keys (additive, today's exact bytes).
        let mut a = a6_session();
        a.session_id = "uuid-a".into();
        a.qd_id = Some("ab3kx9mq".into());
        let mut b = a6_session();
        b.session_id = "uuid-b".into();
        b.qd_id = Some("ab47qrst".into());
        let bare = a6_session();
        let v = ls_json(&[a, b, bare], &[]);
        let arr = v.as_array().unwrap();
        assert_eq!(arr[0]["qdId"], json!("ab3kx9mq"));
        assert_eq!(arr[0]["qdIdPrefix"], json!("ab3"));
        assert_eq!(arr[1]["qdId"], json!("ab47qrst"));
        assert_eq!(arr[1]["qdIdPrefix"], json!("ab4"));
        let bare_obj = arr[2].as_object().unwrap();
        assert!(!bare_obj.contains_key("qdId"), "no id → no qdId key");
        assert!(!bare_obj.contains_key("qdIdPrefix"));
        // Existing fields kept (additive) and code stays LAST.
        let keys: Vec<&str> = arr[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(keys.contains(&"sessionId"));
        assert!(keys.contains(&"code"));
        assert_eq!(keys.last(), Some(&"code"), "code stays last");
        let si = keys.iter().position(|k| *k == "qdId").unwrap();
        let ci = keys.iter().position(|k| *k == "code").unwrap();
        assert!(si < ci, "qdId lands before code");
    }

    #[test]
    fn ls_json_without_qd_ids_is_byte_identical_to_before() {
        // The parity-golden protection: id-less sessions render NO new keys.
        let s = a6_session();
        let out = to_pretty(&ls_json(std::slice::from_ref(&s), &[]));
        assert!(!out.contains("qdId"), "no qdId without a mapped id: {out}");
    }

    // --- WP-B7 PIECE 2: lineage (B5-iii obl-4 OUTPUT field) additive surfacing ---

    #[test]
    fn ls_json_emits_lineage_only_when_fork() {
        // A fork row (lineage Some = the PARENT's qdId) emits the additive
        // `lineage` key = that parent pointer; a non-fork row (lineage None) emits
        // NO key (additive both-ways, today's exact bytes). The fork's OWN id stays
        // `qdId` — lineage is STRICTLY the parent, never the fork's own identity.
        let mut fork = a6_session();
        fork.session_id = "fork-uuid".into();
        fork.qd_id = Some("forkid01".into());
        fork.lineage = Some("parentqd".into());
        let nonfork = a6_session(); // lineage None, no qd_id
        let v = ls_json(&[fork, nonfork], &[]);
        let arr = v.as_array().unwrap();

        // false-negative guard: the fork DOES emit lineage = the parent pointer.
        assert_eq!(arr[0]["lineage"], json!("parentqd"));
        // GUARDRAIL: lineage is the parent, NOT the fork's own id (no leak).
        assert_eq!(arr[0]["qdId"], json!("forkid01"));
        assert_ne!(arr[0]["lineage"], arr[0]["qdId"], "lineage ≠ own qdId");

        // false-positive guard: the non-fork row gains NO lineage key.
        let nf = arr[1].as_object().unwrap();
        assert!(
            !nf.contains_key("lineage"),
            "non-fork row emits no lineage key: {nf:?}"
        );

        // code stays LAST; lineage lands before it (additive-before-code, like qdId).
        let keys: Vec<&str> = arr[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.last(), Some(&"code"), "code stays last");
        let li = keys.iter().position(|k| *k == "lineage").unwrap();
        let ci = keys.iter().position(|k| *k == "code").unwrap();
        assert!(li < ci, "lineage lands before code");
    }

    #[test]
    fn ls_json_without_lineage_is_byte_identical_to_before() {
        // The parity-golden protection: a non-fork session renders NO lineage key,
        // so the no-fork case is byte-identical to the pre-B7 surface.
        let s = a6_session();
        let out = to_pretty(&ls_json(std::slice::from_ref(&s), &[]));
        assert!(
            !out.contains("lineage"),
            "no lineage key without a fork pointer: {out}"
        );
    }

    #[test]
    fn info_text_stable_id_line_only_when_mapped() {
        let mut s = a6_session();
        s.qd_id = Some("ab3kx9mq".into());
        let text = info_text(&s, 0);
        assert!(
            text.contains("Stable ID:   ab3kx9mq\n"),
            "stable id line present: {text}"
        );
        // And absent (byte-identical) when unmapped.
        let bare = a6_session();
        assert!(!info_text(&bare, 0).contains("Stable ID:"));
    }

    // --- info_json (P0 spec-w8) — the promised P1 field list ---

    #[test]
    fn info_json_mapped_emits_all_fields_in_order() {
        let mut s = a6_session();
        s.qd_id = Some("ab3kx9mq".into());
        s.jsonl_path = Some("/p/sess-a6.jsonl".into());
        let prefixes: std::collections::HashMap<String, String> =
            [("ab3kx9mq".to_string(), "ab3".to_string())].into();
        let v = info_json(&s, &prefixes, true);
        let obj = v.as_object().unwrap();
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "name",
                "sessionId",
                "qdId",
                "qdIdPrefix",
                "status",
                "live",
                "pid",
                "provider",
                "lane",
                "jsonlPath"
            ],
            "the EXACT promised field list, in order"
        );
        assert_eq!(obj["name"], json!("worker"));
        assert_eq!(obj["sessionId"], json!("sess-a6"));
        assert_eq!(obj["qdId"], json!("ab3kx9mq"));
        assert_eq!(obj["qdIdPrefix"], json!("ab3"));
        assert_eq!(obj["status"], json!("busy"));
        assert_eq!(obj["live"], json!(true));
        assert_eq!(obj["pid"], json!(4242));
        assert_eq!(obj["provider"], json!("claude-code"));
        // Derived, not echoed — the same `lane_for` every acting verb asks.
        assert_eq!(obj["lane"], json!("claude-code/mux-pane"));
        assert_eq!(obj["jsonlPath"], json!("/p/sess-a6.jsonl"));
    }

    #[test]
    fn info_json_unmapped_omits_qd_id_keys_not_null() {
        // The ls --json convention: absent-not-null for unmapped qdId.
        let s = a6_session();
        let v = info_json(&s, &std::collections::HashMap::new(), true);
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("qdId"), "unmapped → qdId ABSENT");
        assert!(!obj.contains_key("qdIdPrefix"), "qdIdPrefix ABSENT too");
        assert!(
            !obj.contains_key("jsonlPath"),
            "no transcript resolved → jsonlPath ABSENT, not null"
        );
    }

    #[test]
    fn info_json_name_and_pid_are_explicit_null_when_absent() {
        let mut s = a6_session();
        s.name = None;
        s.pid = None;
        let v = info_json(&s, &std::collections::HashMap::new(), false);
        let obj = v.as_object().unwrap();
        assert_eq!(obj["name"], Value::Null, "name: null (NOT absent)");
        assert_eq!(obj["pid"], Value::Null, "pid: null (NOT absent)");
        assert_eq!(obj["live"], json!(false));
        // pid 0 = "no pid recorded" → null as well.
        let mut z = a6_session();
        z.pid = Some(0);
        let vz = info_json(&z, &std::collections::HashMap::new(), true);
        assert_eq!(vz.as_object().unwrap()["pid"], Value::Null);
    }

    #[test]
    fn info_json_prefix_omitted_when_map_lacks_the_id() {
        // qdId mapped but no prefix computed (defensive: prefix_map always has
        // it in production since it is built from the same list) → qdId only.
        let mut s = a6_session();
        s.qd_id = Some("ab3kx9mq".into());
        let v = info_json(&s, &std::collections::HashMap::new(), true);
        let obj = v.as_object().unwrap();
        assert_eq!(obj["qdId"], json!("ab3kx9mq"));
        assert!(!obj.contains_key("qdIdPrefix"));
    }

    // --- D1 / X3: the `lane` key ---

    /// The two codex lanes are TELLABLE APART in `ls --json`, which is the whole
    /// point of the key. `provider` says "codex" for both; only `lane` separates a
    /// row hosted by the daemon from one hosted by the app server — and once
    /// `codex/app-server` is the default with `codex/daemon` still reachable via
    /// `--daemon`, both will appear in one listing routinely.
    ///
    /// MUTATION EVIDENCE: emit `s.hosting` verbatim instead of deriving through
    /// `lane_for`, and the unstamped row below reds — it would carry no lane at all
    /// where the whole codebase (and `qd kill`, and `qd send`) reads it as the
    /// harness default.
    #[test]
    fn the_lane_key_tells_two_lanes_of_one_provider_apart() {
        let lane_of = |provider: &str, hosting: Option<&str>| -> Option<String> {
            let mut s = base(SessionBranch::LiveRegistry);
            s.session_id = "sess-lane".into();
            s.provider = provider.to_string();
            s.hosting = hosting.map(str::to_string);
            let v = ls_json(std::slice::from_ref(&s), &[]);
            v.as_array().unwrap()[0]
                .as_object()
                .unwrap()
                .get("lane")
                .map(|l| l.as_str().unwrap().to_string())
        };

        assert_eq!(
            lane_of("codex", Some("daemon")).as_deref(),
            Some("codex/daemon")
        );
        assert_eq!(
            lane_of("codex", Some("app-server")).as_deref(),
            Some("codex/app-server")
        );
        // An UNSTAMPED row re-derives through the harness default — the same answer
        // `lane_for` gives every acting verb, so the listing and the router agree.
        assert_eq!(lane_of("codex", None).as_deref(), Some("codex/daemon"));
        // The two claude lanes, which is the case this key exists for: they share
        // a provider id and differ only in topology, so a consumer reading
        // `provider` alone cannot tell a PTY composer from an ACP bridge.
        assert_eq!(
            lane_of("claude-code", Some("acp")).as_deref(),
            Some("claude-code/acp")
        );
        // …and the legacy spelling of the same lane, which rows written before
        // the remodel still carry. It pins its lane off the provider id, so the
        // `hosting: "daemon"` those rows also carry cannot move it.
        assert_eq!(
            lane_of("acp/claude-code", Some("daemon")).as_deref(),
            Some("claude-code/acp")
        );
        // A row whose hosting token names a combination the harness cannot support
        // falls back to the harness default rather than inventing a lane.
        assert_eq!(
            lane_of("claude-code", Some("daemon")).as_deref(),
            Some("claude-code/mux-pane")
        );
    }

    /// An UNPLACEABLE provider gets NO `lane` key — absent, never null and never a
    /// guess. `lane_for` answers `None` for a provider id qd cannot place, and the
    /// only honest rendering of "qd does not know what this row is" is silence; a
    /// fabricated `"unknown/daemon"` would be read by a consumer as a routable lane.
    /// Same absent-not-null contract as `qdId`/`lineage`.
    #[test]
    fn an_unplaceable_provider_carries_no_lane_key() {
        let mut s = base(SessionBranch::LiveRegistry);
        s.session_id = "sess-alien".into();
        s.provider = "not-a-harness".to_string();
        let v = ls_json(std::slice::from_ref(&s), &[]);
        let obj = v.as_array().unwrap()[0].as_object().unwrap();
        assert!(
            !obj.contains_key("lane"),
            "unplaceable provider ⇒ lane ABSENT, not null: {obj:?}"
        );
        assert_eq!(obj["provider"], json!("not-a-harness"), "the row still renders");
    }

    /// `lane` lands next to `provider` and BEFORE `code`, on every construction
    /// branch. `code` stays LAST (assignShortCodes ordering) — the same discipline
    /// `backend`/`qdId`/`readiness` follow.
    #[test]
    fn the_lane_key_follows_provider_and_precedes_code_on_every_branch() {
        for branch in [
            SessionBranch::LiveRegistry,
            SessionBranch::ColdJsonl,
            SessionBranch::ZmxOnly,
            SessionBranch::Tombstoned,
        ] {
            let mut s = base(branch);
            s.session_id = "sess-order".into();
            let out = to_pretty(&ls_json(std::slice::from_ref(&s), &[]));
            let p = out.find("\"provider\"").expect("provider present");
            let l = out.find("\"lane\"").unwrap_or_else(|| panic!("lane present: {out}"));
            let c = out.find("\"code\"").expect("code present");
            assert!(p < l && l < c, "provider < lane < code on {branch:?}: {out}");
        }
    }

    /// A STRAY carries no `lane` key. A stray is an unmanaged claude transcript on
    /// disk with no registry row behind it — its `provider` is a hardcoded literal,
    /// not a recorded fact, so deriving a lane from it would dress a guess up as
    /// routing information about a session qd does not manage.
    #[test]
    fn strays_carry_no_lane() {
        let stray = Stray {
            session_id: "stray-1".into(),
            jsonl_path: std::path::PathBuf::from("/tmp/stray.jsonl"),
            project_dir: "-tmp".into(),
            pid: None,
            mtime_ms: 0,
            active_recent: false,
        };
        let v = ls_json(&[], &[stray]);
        let obj = v.as_array().unwrap()[0].as_object().unwrap();
        assert!(!obj.contains_key("lane"), "stray ⇒ no lane: {obj:?}");
    }

    #[test]
    fn ls_json_backend_only_when_spawned_by_absent() {
        // A create event with backend but no spawnedBy → only the backend field.
        let create = crate::telemetry::build_create_line(
            "t",
            &crate::telemetry::CreateEvent {
                name: "worker".into(),
                session_id: Some("sess-a6".into()),
                backend: Some("ccr-3456".into()),
                ..Default::default()
            },
        );
        let fold = fold_marks(&format!("{create}\n"));
        let s = a6_session();
        let v = ls_json_with_fold(&[s], &[], Some(&fold));
        let obj = v.as_array().unwrap()[0].as_object().unwrap();
        assert_eq!(obj["backend"], json!("ccr-3456"));
        assert!(
            !obj.contains_key("spawnedBy"),
            "spawnedBy absent when unset"
        );
    }
}
