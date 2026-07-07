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
/// Key order: name, sessionId, qdId?, qdIdPrefix?, status, live, pid, provider.
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

/// Epoch ms → `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC), replicating JS `Date.toJSON`
/// (`toISOString`), which is ALWAYS ms-precision UTC. No chrono — civil-date
/// math (Howard Hinnant). Verified vs bun (`new Date(ms).toJSON()`).
pub fn epoch_ms_to_iso(ms: i64) -> String {
    let (y, mo, d, h, mi, s, milli) = civil_from_epoch_ms(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

/// Epoch ms → en-US `toLocaleString()` form `M/D/YYYY, H:MM:SS AM/PM` in UTC.
///
/// NORMALIZATION-CLASS (spec §8): the real TS output is locale + timezone
/// dependent (`Date.toLocaleString()` with no args). The 0b comparator normalizes
/// these lines; we emit a DETERMINISTIC en-US/UTC form so the Rust output is
/// stable and byte-exact only POST-normalization. Verified vs bun:
///   `bun -e 'console.log(new Date(1717530000000).toLocaleString("en-US",{timeZone:"UTC"}))'`
///     → 6/4/2024, 3:40:00 PM
/// Rules: no leading zero on month/day/hour; zero-padded minute/second; 12-hour
/// with AM/PM; midnight → 12 AM, noon → 12 PM.
pub fn epoch_ms_to_en_us_locale(ms: i64) -> String {
    let (y, mo, d, h24, mi, s, _milli) = civil_from_epoch_ms(ms);
    let (h12, ampm) = match h24 {
        0 => (12, "AM"),
        1..=11 => (h24, "AM"),
        12 => (12, "PM"),
        _ => (h24 - 12, "PM"),
    };
    format!("{mo}/{d}/{y}, {h12}:{mi:02}:{s:02} {ampm}")
}

/// Decompose epoch ms (UTC) into (year, month, day, hour, min, sec, milli).
fn civil_from_epoch_ms(ms: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let total_secs = ms.div_euclid(1000);
    let milli = ms.rem_euclid(1000) as u32;
    let days = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400);
    let hour = (secs_of_day / 3600) as u32;
    let min = ((secs_of_day % 3600) / 60) as u32;
    let sec = (secs_of_day % 60) as u32;
    let (y, mo, d) = civil_from_days(days);
    (y, mo, d, hour, min, sec, milli)
}

/// Inverse of days-from-civil (Howard Hinnant). days since 1970-01-01 → (y,m,d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

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

    // zmx line (status.ts:629-631).
    let zmx_line = match &session.zmx_name {
        Some(name) => {
            let attached = session.zmx_clients.unwrap_or(0) > 0;
            format!(
                "{} ({})",
                name,
                if attached { "attached" } else { "detached" }
            )
        }
        None => "-".to_string(),
    };
    push(&mut out, format!("zmx:         {zmx_line}"));

    // zmx dir — only when socketDir present (status.ts:632-634).
    if let Some(dir) = &session.socket_dir {
        push(&mut out, format!("zmx dir:     {dir}"));
    }

    // Relay (status.ts:635).
    let relay_line = match session.relay_port {
        Some(port) => format!("localhost:{port}"),
        None => "-".to_string(),
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
        assert_eq!(overridden.as_array().unwrap()[0]["status"], json!("stopped"));

        // No override (empty slice) → byte-identical to plain ls_json (stored "busy").
        let plain = ls_json(std::slice::from_ref(&acp), &[]);
        let none_override = ls_json_full_acp(std::slice::from_ref(&acp), &[], None, None, &[None]);
        assert_eq!(to_pretty(&none_override), to_pretty(&plain), "None override == stored bytes");
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
                "code", // LAST
            ]
        );
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
        assert!(text.contains("zmx:         -\n"));
        assert!(text.contains("Relay:       -\n"));
        assert!(text.contains("Turns:       3\n"));
        assert!(text.contains("Tokens:      1.5k\n"));
        assert!(text.contains("CWD:         -\n"));
        // No socketDir → no "zmx dir" line.
        assert!(!text.contains("zmx dir:"));
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
        assert!(text.contains("zmx:         zw (attached)\n"));
        assert!(text.contains("zmx dir:     /tmp/zmx-501\n"));
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
                "provider"
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
    }

    #[test]
    fn info_json_unmapped_omits_qd_id_keys_not_null() {
        // The ls --json convention: absent-not-null for unmapped qdId.
        let s = a6_session();
        let v = info_json(&s, &std::collections::HashMap::new(), true);
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("qdId"), "unmapped → qdId ABSENT");
        assert!(!obj.contains_key("qdIdPrefix"), "qdIdPrefix ABSENT too");
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
