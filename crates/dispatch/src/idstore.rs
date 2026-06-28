//! P0 wave-1 A — stable session ids (spec-cli.md §3/§11).
//!
//! Engine-minted 8-char stable ids, keyed by the PROVIDER session UUID
//! (`Session.session_id` — the only identity that survives stop/resume; the
//! registry row is Claude-Code-authored and dies with the process). The store is
//! an append-only JSONL at `<state_dir>/ids.jsonl` (the marks.jsonl pattern):
//! one `mint` event per id, ever.
//!
//! **Never reused / never reassigned falls out of append-only**: mint lines are
//! never deleted or rewritten, the fold is first-mint-wins per session_id, and
//! the collision check at mint time runs against ALL historical ids (every
//! parseable line, including any that the fold's first-wins rule shadows). An id
//! therefore names one provider session forever.
//!
//! Concurrency: [`mint_or_get`] holds an exclusive `flock` on the store file
//! across read-fold + append, so concurrent minters (e.g. two `qd ls` backfills)
//! agree on one id per session and never interleave lines. Readers (the join)
//! use [`fold`] WITHOUT the lock — torn trailing lines are tolerated (skipped).
//!
//! ## Mint timing at `qd start` (P0 wave-2, spec-w2-env Design note option (a))
//!
//! At `qd start <name>` the provider UUID does not exist until Claude Code boots
//! and writes its registry row — but the stable id must already be in the
//! child's env at launch (env-bake time). The flow is **mint-at-start,
//! bind-at-boot-confirm**:
//!
//!   1. [`mint_unbound`] BEFORE launch: appends a `mint` event whose
//!      `session_id` is `null` (carries the `name` for legibility). The id is
//!      injected as `QD_SESSION_ID` in the launch env.
//!   2. After the boot waiter confirms the registry row (boot.rs finds the row
//!      by name; the row carries the provider UUID), [`bind`] appends a
//!      `{"event":"bind","id":…,"session_id":…}` line under the same flock.
//!
//! How the fold resolves binds (first-wins, unbound-garbage ids stay reserved)
//! is specified at [`fold_str`] and [`bind`]. `resume` paths know the UUID up
//! front and use [`mint_or_get`] directly — the id rides through unchanged
//! across resumes.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::effects::Clock;
use crate::model::Session;

/// The id alphabet: base36 minus the ruled-out ambiguous symbols `{0, 1, o, l}`.
/// 32 symbols, lowercase canonical; resolution folds input to lowercase.
pub const ALPHABET: &str = "23456789abcdefghijkmnpqrstuvwxyz";

/// Stable ids are exactly 8 chars from [`ALPHABET`] (~1.1e12 space).
pub const ID_LEN: usize = 8;

/// Bounded regeneration attempts when a freshly-generated id collides with a
/// historical one (each collision is a ~1/32^8 event; 16 in a row means the RNG
/// is broken — error loudly rather than loop).
const MAX_MINT_RETRIES: usize = 16;

/// The store path under a resolved qd state dir: `<state_dir>/ids.jsonl`.
pub fn ids_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ids.jsonl")
}

/// Lowercase-fold a candidate id (ids are case-insensitive at resolution,
/// lowercase canonical on disk).
pub fn normalize(s: &str) -> String {
    s.to_lowercase()
}

/// True iff `s` (after lowercase fold) is a well-formed full id: exactly
/// [`ID_LEN`] chars, every one in [`ALPHABET`].
pub fn is_valid_id(s: &str) -> bool {
    let folded = normalize(s);
    folded.chars().count() == ID_LEN && folded.chars().all(|c| ALPHABET.contains(c))
}

/// True iff every char of `s` (after lowercase fold) is in [`ALPHABET`] — the
/// shape gate for the resolution prefix tier (≥2 chars is the caller's check).
pub fn is_alphabet_prefix(s: &str) -> bool {
    !s.is_empty() && normalize(s).chars().all(|c| ALPHABET.contains(c))
}

/// The folded store: both directions of the id ↔ provider-session-uuid map.
#[derive(Debug, Clone, Default)]
pub struct IdMap {
    /// id → session_id. Carries EVERY historical id (first session per id), so
    /// the mint-time collision check sees ids even on lines the by_session
    /// first-wins rule shadows (defense in depth). `None` = an UNBOUND mint
    /// (minted pre-boot with `session_id: null`, no bind folded yet) — the id
    /// is still reserved forever (collision check is `contains_key`).
    pub by_id: HashMap<String, Option<String>>,
    /// session_id → id. First mint (or bind) for a session_id wins; later lines
    /// for the same session_id are ignored (the lock should prevent them existing).
    pub by_session: HashMap<String, String>,
    /// WP-B5-iii (obl-4 lineage): a FORK's provider session_id → its PARENT
    /// instance's stable qdId. Recorded by a `lineage` event at fork time
    /// (verb-side, keyed by the fork's pre-minted uuid — works for both the
    /// interactive and headless launch paths with no daemon/protocol change).
    /// First-wins. STRICTLY the parent pointer — never the fork's own id (which
    /// stays `by_session[fork_uuid]`).
    pub by_parent: HashMap<String, String>,
}

impl IdMap {
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty() && self.by_session.is_empty() && self.by_parent.is_empty()
    }
}

/// Pure fold over the store's text. Unparseable lines (torn tail, foreign
/// shapes) are skipped. `mint` events with a non-empty string `session_id`
/// map both directions; `mint` events with a `null`/absent `session_id` are
/// UNBOUND (reserve the id only); `bind` events resolve onto their mint
/// (id must exist, unbound; session must not already have an id). First-wins
/// in both directions, in file order.
pub fn fold_str(text: &str) -> IdMap {
    let mut map = IdMap::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue; // torn / foreign line — tolerated
        };
        let event = v.get("event").and_then(|e| e.as_str());
        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
        let session_id = v.get("session_id").and_then(|x| x.as_str()).unwrap_or("");
        match event {
            Some("mint") => {
                if id.is_empty() {
                    continue;
                }
                if session_id.is_empty() {
                    // UNBOUND mint (pre-boot): the id is reserved, no session yet.
                    map.by_id.entry(id.to_string()).or_insert(None);
                    continue;
                }
                map.by_id
                    .entry(id.to_string())
                    .or_insert_with(|| Some(session_id.to_string()));
                map.by_session
                    .entry(session_id.to_string())
                    .or_insert_with(|| id.to_string());
            }
            Some("bind") => {
                if id.is_empty() || session_id.is_empty() {
                    continue;
                }
                // A bind resolves ONLY onto an existing UNBOUND mint, and only
                // when the session doesn't already have an id (first-wins).
                if map.by_session.contains_key(session_id) {
                    continue;
                }
                if let Some(slot) = map.by_id.get_mut(id) {
                    if slot.is_none() {
                        *slot = Some(session_id.to_string());
                        map.by_session
                            .insert(session_id.to_string(), id.to_string());
                    }
                }
                // unknown id / already-bound id — ignored
            }
            Some("lineage") => {
                // WP-B5-iii obl-4: a fork's session_id → parent qdId. First-wins.
                let parent = v.get("parent").and_then(|x| x.as_str()).unwrap_or("");
                if session_id.is_empty() || parent.is_empty() {
                    continue;
                }
                map.by_parent
                    .entry(session_id.to_string())
                    .or_insert_with(|| parent.to_string());
            }
            _ => continue,
        }
    }
    map
}

/// Read-only fold of the store file. Missing file → empty map. NO lock taken —
/// safe for the join/read path (torn trailing lines are skipped by `fold_str`).
pub fn fold(path: &Path) -> IdMap {
    match std::fs::read_to_string(path) {
        Ok(text) => fold_str(&text),
        Err(_) => IdMap::default(),
    }
}

/// Resolve a RAW stable-id candidate to its bound provider session uuid:
/// lowercase-fold ([`normalize`]) → shape gate ([`is_valid_id`]) → `by_id`
/// lookup, with an UNBOUND mint (`None` slot) resolving to `None`.
///
/// THE one chain for "what engine identity resolves?" — `qd whoami` and the
/// send:relay `from_session` derivation (B2 item 5) both call this, so
/// attribution and whoami answer identically BY CONSTRUCTION (the
/// name-consistency lesson: two hand-rolled copies of a derivation drift).
/// `None` = malformed shape, unknown id, or unbound mint — the caller falls
/// through to its own fallback chain; this function never invents an identity.
pub fn resolve_to_uuid(map: &IdMap, raw: &str) -> Option<String> {
    let id = normalize(raw);
    if !is_valid_id(&id) {
        return None;
    }
    map.by_id.get(&id).cloned().flatten()
}

/// The display id for a name-holding session in a refusal message (shared by
/// the create-path registry scan and the resume-path D4 zmx-name scan — the
/// two SCANS stay distinct, only this fallback chain is shared): the holder's
/// stable id when the store maps its UUID (READ-ONLY fold; an error path never
/// mints), else the truncated provider UUID, else `PID <pid>`.
pub fn holder_display_id(ids_path: &Path, session_id: Option<&str>, pid: Option<i64>) -> String {
    let uuid = session_id.unwrap_or_default();
    if !uuid.is_empty() {
        let ids = fold(ids_path);
        if let Some(id) = ids.by_session.get(uuid) {
            return id.clone();
        }
        return crate::fmt::truncate_id_default(uuid);
    }
    format!("PID {}", pid.unwrap_or(0))
}

/// Fill `Session.qd_id` from a fold (the join-layer pass; read-only, no lock).
pub fn fill_qd_ids(sessions: &mut [Session], map: &IdMap) {
    for s in sessions {
        if s.qd_id.is_none() && !s.session_id.is_empty() {
            s.qd_id = map.by_session.get(&s.session_id).cloned();
        }
    }
}

/// WP-B5-iii obl-4: fill `Session.lineage` (the fork→PARENT-qdId pointer) from a
/// fold — the SAME read-only join pass `qd ls` runs (`common::all_sessions`), so
/// a fork's parent is discoverable through the live resolver path, not a raw row
/// assert. Parity-safe: `lineage` is NOT serialized on the `ls --json` surface
/// (a lineage output field is a B7-owned golden-delta, deferred). Strictly the
/// parent pointer — never the fork's own id.
pub fn fill_lineage(sessions: &mut [Session], map: &IdMap) {
    for s in sessions {
        if s.lineage.is_none() && !s.session_id.is_empty() {
            s.lineage = map.by_parent.get(&s.session_id).cloned();
        }
    }
}

/// Mint a stable id for `session_id`, or return the already-minted one.
///
/// Holds an exclusive `flock` on the store file across read-fold + append:
/// open/create → LOCK_EX → fold → decide → ONE `O_APPEND` line → unlock (on
/// drop). If `session_id` is already mapped the existing id is returned and
/// nothing is appended. A generated id colliding with ANY historical id is
/// regenerated (bounded; then a loud error). Empty `session_id` is an error —
/// ids are never minted for "" (ZmxOnly rows carry "").
pub fn mint_or_get(
    path: &Path,
    session_id: &str,
    name: Option<&str>,
    clock: &dyn Clock,
) -> Result<String, String> {
    mint_or_get_with(path, session_id, name, clock, &mut random_id)
}

/// [`mint_or_get`] with an injected id generator (the testable seam for the
/// collision-regeneration path; production passes [`random_id`]).
pub fn mint_or_get_with(
    path: &Path,
    session_id: &str,
    name: Option<&str>,
    clock: &dyn Clock,
    gen_id: &mut dyn FnMut() -> String,
) -> Result<String, String> {
    if session_id.is_empty() {
        return Err("idstore: refusing to mint for an empty session_id".to_string());
    }
    let (mut file, map) = open_locked(path)?;

    if let Some(existing) = map.by_session.get(session_id) {
        return Ok(existing.clone());
    }

    let id = generate_fresh_id(&map, gen_id)?;
    let line = serde_json::json!({
        "v": 1,
        "ts": crate::render::epoch_ms_to_iso(clock.now_ms()),
        "event": "mint",
        "id": id,
        "session_id": session_id,
        "name": name,
    })
    .to_string();
    file.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("idstore: append failed on {}: {e}", path.display()))?;
    Ok(id)
}

/// Mint an UNBOUND id (P0 wave-2 mint-at-start flow): the provider UUID does
/// not exist yet at `qd start` env-bake time, so the mint line carries
/// `session_id: null` (+ `name` for legibility). The id is injected into the
/// child's env; [`bind`] attaches the UUID after the boot waiter confirms the
/// registry row. Same flock + collision discipline as [`mint_or_get`].
pub fn mint_unbound(path: &Path, name: Option<&str>, clock: &dyn Clock) -> Result<String, String> {
    mint_unbound_with(path, name, clock, &mut random_id)
}

/// [`mint_unbound`] with an injected id generator (the testable seam).
pub fn mint_unbound_with(
    path: &Path,
    name: Option<&str>,
    clock: &dyn Clock,
    gen_id: &mut dyn FnMut() -> String,
) -> Result<String, String> {
    let (mut file, map) = open_locked(path)?;
    let id = generate_fresh_id(&map, gen_id)?;
    let line = serde_json::json!({
        "v": 1,
        "ts": crate::render::epoch_ms_to_iso(clock.now_ms()),
        "event": "mint",
        "id": id,
        "session_id": serde_json::Value::Null,
        "name": name,
    })
    .to_string();
    file.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("idstore: append failed on {}: {e}", path.display()))?;
    Ok(id)
}

/// What [`bind`] actually did — the caller-visible truth (P0 redfix F1: the old
/// `Ok(winning_id)` shape let a caller's divergence warning silently never fire
/// when the session was already mapped to a DIFFERENT id).
///
/// FOLD RULES ARE UNCHANGED — this is API surface only: every variant returns
/// without altering what the store would have contained under the old API.
#[derive(Debug, Clone, PartialEq)]
pub enum BindOutcome {
    /// The `bind` event was appended; `session_id` now maps to `id`.
    Bound,
    /// `session_id` already maps to this same `id` — idempotent re-bind (e.g. a
    /// retried verb). Nothing appended; silent by contract.
    AlreadyBoundSameId,
    /// `session_id` already maps to a DIFFERENT id (an earlier mint/bind won —
    /// first-wins per session; the unbound mint stays reserved garbage). The
    /// caller decides whether this is the loud divergence case.
    SessionHasDifferentId { existing: String },
}

/// Bind a previously [`mint_unbound`]-minted id to its provider session UUID
/// (the boot waiter confirmed the registry row). Under the flock:
///
///   - `session_id` already mapped to a DIFFERENT id (an earlier mint/bind won)
///     → [`BindOutcome::SessionHasDifferentId`] naming the winner; appends
///     nothing — first-wins per session, the unbound mint becomes ignorable
///     garbage (its id stays reserved forever).
///   - `id` is an unbound mint → appends the `bind` event →
///     [`BindOutcome::Bound`].
///   - `id` already bound to this same `session_id` → idempotent
///     [`BindOutcome::AlreadyBoundSameId`].
///   - `id` unknown or bound to a DIFFERENT session → error (caller bug).
pub fn bind(
    path: &Path,
    id: &str,
    session_id: &str,
    clock: &dyn Clock,
) -> Result<BindOutcome, String> {
    if session_id.is_empty() {
        return Err("idstore: refusing to bind an empty session_id".to_string());
    }
    let id = normalize(id);
    let (mut file, map) = open_locked(path)?;

    if let Some(existing) = map.by_session.get(session_id) {
        if *existing == id {
            return Ok(BindOutcome::AlreadyBoundSameId);
        }
        return Ok(BindOutcome::SessionHasDifferentId {
            existing: existing.clone(),
        });
    }
    match map.by_id.get(&id) {
        None => Err(format!("idstore: bind for unknown id {id:?}")),
        Some(Some(other)) => Err(format!(
            "idstore: bind for id {id:?} already bound to a different session ({other})"
        )),
        Some(None) => {
            let line = serde_json::json!({
                "v": 1,
                "ts": crate::render::epoch_ms_to_iso(clock.now_ms()),
                "event": "bind",
                "id": id,
                "session_id": session_id,
            })
            .to_string();
            file.write_all(format!("{line}\n").as_bytes())
                .map_err(|e| format!("idstore: append failed on {}: {e}", path.display()))?;
            Ok(BindOutcome::Bound)
        }
    }
}

/// WP-B5-iii obl-4: record a FORK's lineage pointer — `fork_session_id` (the
/// fork's pre-minted provider uuid) → `parent_qd_id` (the parent INSTANCE's
/// stable qdId). Append-only `lineage` event under the store lock; first-wins
/// (idempotent on a retried fork launch). Called verb-side at fork time for BOTH
/// the interactive and headless paths (keyed by the uuid both paths know), so no
/// daemon/protocol plumbing is needed. STRICTLY the parent pointer — the fork's
/// OWN qdId is minted separately via [`mint_or_get`] and is never touched here.
pub fn record_lineage(
    path: &Path,
    fork_session_id: &str,
    parent_qd_id: &str,
    clock: &dyn Clock,
) -> Result<(), String> {
    if fork_session_id.is_empty() || parent_qd_id.is_empty() {
        return Err("idstore: refusing to record lineage with an empty id".to_string());
    }
    let (mut file, map) = open_locked(path)?;
    if map.by_parent.contains_key(fork_session_id) {
        return Ok(()); // first-wins, idempotent
    }
    let line = serde_json::json!({
        "v": 1,
        "ts": crate::render::epoch_ms_to_iso(clock.now_ms()),
        "event": "lineage",
        "session_id": fork_session_id,
        "parent": parent_qd_id,
    })
    .to_string();
    file.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("idstore: append failed on {}: {e}", path.display()))?;
    Ok(())
}

/// Open the store with an exclusive `flock` held for the file handle's life and
/// fold its current contents. The lock spans read-fold + the caller's single
/// append; it releases when the returned `File` drops (every early return).
fn open_locked(path: &Path) -> Result<(std::fs::File, IdMap), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("idstore: could not create state dir: {e}"))?;
    }

    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("idstore: could not open {}: {e}", path.display()))?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(format!(
            "idstore: flock failed on {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }

    // Fold under the lock (read via a fresh handle so the append-mode offset is
    // untouched; the lock guarantees a quiescent file).
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("idstore: could not read {}: {e}", path.display()))?;
    Ok((file, fold_str(&text)))
}

/// Generate an id absent from ALL history (bounded regeneration; validity
/// checked). Shared by the bound + unbound mint paths.
fn generate_fresh_id(map: &IdMap, gen_id: &mut dyn FnMut() -> String) -> Result<String, String> {
    let mut id = gen_id();
    let mut tries = 0;
    while map.by_id.contains_key(&id) {
        tries += 1;
        if tries >= MAX_MINT_RETRIES {
            return Err(format!(
                "idstore: {MAX_MINT_RETRIES} id collisions in a row — id generation is broken"
            ));
        }
        id = gen_id();
    }
    if !is_valid_id(&id) {
        return Err(format!("idstore: generated id {id:?} is not a valid id"));
    }
    Ok(id)
}

/// 8 chars drawn uniformly from [`ALPHABET`] via `/dev/urandom` (the
/// relay_server `read_urandom` pattern). The alphabet has 32 symbols, so one
/// random byte masked to 5 bits is an unbiased draw. Degenerate pid+nanos
/// fallback if `/dev/urandom` is unreadable (essentially impossible) — never
/// fails to produce an id.
pub fn random_id() -> String {
    let alphabet: Vec<char> = ALPHABET.chars().collect();
    let mut bytes = [0u8; ID_LEN];
    if read_urandom(&mut bytes).is_err() {
        // Degenerate fallback: pid + nanos. Not cryptographic; the urandom path
        // is the norm (mirrors relay_server::random_uuid_v4).
        let seed = (std::process::id() as u128) << 64
            | (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
                & 0xFFFF_FFFF_FFFF_FFFF);
        let seed_bytes = seed.to_le_bytes();
        bytes.copy_from_slice(&seed_bytes[..ID_LEN]);
    }
    bytes
        .iter()
        .map(|b| alphabet[(b & 0x1F) as usize])
        .collect()
}

/// Fill `buf` from `/dev/urandom` (relay_server/mod.rs pattern, replicated here
/// — that copy is private to the relay server module).
fn read_urandom(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

/// Per id, the shortest prefix (minimum length 2) unique within `ids`; the full
/// id when no shorter prefix disambiguates (including the degenerate duplicate
/// case, which the by_session fold precludes upstream).
pub fn shortest_unique_prefixes(ids: &[String]) -> Vec<String> {
    ids.iter()
        .map(|id| {
            let chars: Vec<char> = id.chars().collect();
            for len in 2..chars.len() {
                let prefix: String = chars[..len].iter().collect();
                let n = ids.iter().filter(|o| o.starts_with(&prefix)).count();
                if n == 1 {
                    return prefix;
                }
            }
            id.clone()
        })
        .collect()
}

/// Convenience: the `id → shortest-unique-prefix` map for a session list (the
/// `ls` display set). Only sessions WITH an id participate.
pub fn prefix_map(sessions: &[Session]) -> HashMap<String, String> {
    let ids: Vec<String> = {
        let mut seen = HashSet::new();
        sessions
            .iter()
            .filter_map(|s| s.qd_id.clone())
            .filter(|id| seen.insert(id.clone()))
            .collect()
    };
    let prefixes = shortest_unique_prefixes(&ids);
    ids.into_iter().zip(prefixes).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::FixedClock;

    const TS: i64 = 1_717_500_300_000;

    fn store(dir: &tempfile::TempDir) -> PathBuf {
        ids_path(&dir.path().join("state"))
    }

    // --- alphabet pin (spec D1) ---

    #[test]
    fn alphabet_pinned_verbatim() {
        assert_eq!(ALPHABET, "23456789abcdefghijkmnpqrstuvwxyz");
        assert_eq!(ALPHABET.chars().count(), 32);
        for banned in ['0', '1', 'o', 'l', 'O', 'L'] {
            assert!(
                !ALPHABET.contains(banned.to_ascii_lowercase()),
                "alphabet must not contain {banned:?}"
            );
        }
        // No duplicates.
        let set: HashSet<char> = ALPHABET.chars().collect();
        assert_eq!(set.len(), 32);
    }

    #[test]
    fn validity_and_normalize() {
        assert!(is_valid_id("ab3kx9mq"));
        assert!(is_valid_id("AB3KX9MQ"), "case-insensitive after fold");
        assert!(!is_valid_id("ab3kx9m"), "7 chars");
        assert!(!is_valid_id("ab3kx9mqz"), "9 chars");
        assert!(!is_valid_id("ab3kx9m0"), "contains 0");
        assert!(!is_valid_id("ab3kx9ml"), "contains l");
        assert!(!is_valid_id("ab3kx9m1"), "contains 1");
        assert!(!is_valid_id("ab3kx9mo"), "contains o");
        assert_eq!(normalize("AB3KX9MQ"), "ab3kx9mq");
    }

    #[test]
    fn random_id_is_well_formed() {
        for _ in 0..100 {
            let id = random_id();
            assert!(is_valid_id(&id), "random id {id:?} must be valid");
        }
    }

    // --- mint / fold / dedupe ---

    #[test]
    fn mint_then_get_returns_same_id_and_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let clock = FixedClock(TS);
        let a = mint_or_get(&path, "uuid-a", Some("alpha"), &clock).unwrap();
        assert!(is_valid_id(&a));
        let again = mint_or_get(&path, "uuid-a", Some("alpha-renamed"), &clock).unwrap();
        assert_eq!(a, again, "second call returns the existing id");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1, "no second line appended");
        // The mint line carries the v1 shape.
        let v: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["event"], "mint");
        assert_eq!(v["id"], serde_json::json!(a));
        assert_eq!(v["session_id"], "uuid-a");
        assert_eq!(v["name"], "alpha");
        assert_eq!(v["ts"], "2024-06-04T11:25:00.000Z");
    }

    /// (N-ids, Item 3) VERIFY (not invent): the shared `mint_or_get` backfill is
    /// PROVIDER-AGNOSTIC — it keys purely on `session_id`, so an acp row (whose
    /// `session_id` is the ACP sessionId) mints/reuses a stable id by the SAME path as
    /// codex/claude, with NO acp-specific binding. And because `run_acp_resume` PRESERVES
    /// the sessionId, the id is STABLE across a stop→resume (same sessionId → same id,
    /// append-only, never reassigned). This is the proof the ls backfill already covers
    /// acp rows (the plan's verify-existing disposition).
    #[test]
    fn acp_row_mints_and_reuses_stable_id_across_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let clock = FixedClock(TS);
        let acp_sid = "ff474896-c848-46f0-9ed6-48d470a7dcbb"; // an ACP sessionId (UUID)
        let first = mint_or_get(&path, acp_sid, Some("acp-wk"), &clock).unwrap();
        assert!(is_valid_id(&first), "an acp row mints a valid stable id");
        // A later `ls` backfill (rename is harmless) → SAME id, no new line.
        let again = mint_or_get(&path, acp_sid, Some("acp-wk-renamed"), &clock).unwrap();
        assert_eq!(first, again, "the same acp sessionId reuses its id (never reassigned)");
        // RESUME preserves the sessionId → the resumed row's backfill reuses the SAME id.
        let after_resume = mint_or_get(&path, acp_sid, Some("acp-wk"), &clock).unwrap();
        assert_eq!(first, after_resume, "the id is STABLE across stop→resume (same sessionId)");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            1,
            "append-only: exactly one mint line, no reassignment"
        );
    }

    #[test]
    fn mint_with_no_name_writes_null() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        mint_or_get(&path, "uuid-n", None, &FixedClock(TS)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert!(v["name"].is_null());
    }

    #[test]
    fn empty_session_id_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = mint_or_get(&store(&dir), "", None, &FixedClock(TS)).unwrap_err();
        assert!(err.contains("empty session_id"), "{err}");
    }

    #[test]
    fn fold_is_first_mint_wins_and_bidirectional() {
        let text = concat!(
            r#"{"v":1,"ts":"t","event":"mint","id":"aaaaaaaa","session_id":"s1","name":null}"#,
            "\n",
            r#"{"v":1,"ts":"t","event":"mint","id":"bbbbbbbb","session_id":"s1","name":null}"#,
            "\n",
            r#"{"v":1,"ts":"t","event":"mint","id":"cccccccc","session_id":"s2","name":"x"}"#,
            "\n",
        );
        let map = fold_str(text);
        assert_eq!(map.by_session["s1"], "aaaaaaaa", "first mint wins");
        assert_eq!(map.by_session["s2"], "cccccccc");
        assert_eq!(map.by_id["aaaaaaaa"], Some("s1".to_string()));
        // The shadowed line's id is STILL a historical id (collision defense).
        assert_eq!(map.by_id["bbbbbbbb"], Some("s1".to_string()));
        assert_eq!(map.by_id["cccccccc"], Some("s2".to_string()));
    }

    #[test]
    fn fold_tolerates_torn_tail_and_foreign_lines() {
        let text = concat!(
            r#"{"v":1,"ts":"t","event":"mint","id":"aaaaaaaa","session_id":"s1","name":null}"#,
            "\n",
            r#"{"v":1,"event":"other-event","id":"ignored0","session_id":"s9"}"#,
            "\n",
            "not json at all\n",
            r#"{"v":1,"ts":"t","event":"mint","id":"bbbb"#, // torn tail
        );
        let map = fold_str(text);
        assert_eq!(map.by_session.len(), 1);
        assert_eq!(map.by_session["s1"], "aaaaaaaa");
        assert_eq!(map.by_id.len(), 1);
    }

    #[test]
    fn fold_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let map = fold(&store(&dir));
        assert!(map.is_empty());
    }

    // --- resolve_to_uuid (S4 — the ONE whoami/attribution resolution chain) ---

    #[test]
    fn resolve_to_uuid_chain() {
        let map = fold_str(concat!(
            "{\"event\":\"mint\",\"id\":\"ab3kx9mq\",\"session_id\":\"uuid-1\"}\n",
            "{\"event\":\"mint\",\"id\":\"cd47qrst\",\"session_id\":null}\n",
        ));
        // Bound mint resolves; case-folded input resolves identically.
        assert_eq!(resolve_to_uuid(&map, "ab3kx9mq"), Some("uuid-1".into()));
        assert_eq!(resolve_to_uuid(&map, "AB3KX9MQ"), Some("uuid-1".into()));
        // Unknown id, malformed shape, unbound mint → None (never invents).
        assert_eq!(resolve_to_uuid(&map, "zzzzzzzz"), None);
        assert_eq!(resolve_to_uuid(&map, "not-an-id!"), None);
        assert_eq!(resolve_to_uuid(&map, "cd47qrst"), None, "unbound mint");
        assert_eq!(resolve_to_uuid(&map, ""), None);
    }

    // --- unbound mint + bind (P0 wave-2, mint-at-start / bind-at-boot-confirm) ---

    #[test]
    fn mint_unbound_writes_null_session_and_reserves_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let id = mint_unbound(&path, Some("wk"), &FixedClock(TS)).unwrap();
        assert!(is_valid_id(&id));
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(v["event"], "mint");
        assert!(v["session_id"].is_null(), "unbound mint: {v}");
        assert_eq!(v["name"], "wk");
        // The fold reserves the id (collision defense) but maps no session.
        let map = fold(&path);
        assert_eq!(map.by_id[&id], None);
        assert!(map.by_session.is_empty());
        // A later mint colliding with the UNBOUND id must regenerate.
        let mut calls = 0;
        let id2 = {
            let id = id.clone();
            let mut generator = move || {
                calls += 1;
                if calls == 1 {
                    id.clone()
                } else {
                    "ffffffff".to_string()
                }
            };
            mint_or_get_with(&path, "s-new", None, &FixedClock(TS), &mut generator).unwrap()
        };
        assert_eq!(id2, "ffffffff", "unbound ids are reserved forever");
    }

    #[test]
    fn bind_resolves_onto_its_unbound_mint() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let clock = FixedClock(TS);
        let id = mint_unbound(&path, Some("wk"), &clock).unwrap();
        let bound = bind(&path, &id, "uuid-wk", &clock).unwrap();
        assert_eq!(bound, BindOutcome::Bound);
        let map = fold(&path);
        assert_eq!(map.by_session["uuid-wk"], id);
        assert_eq!(map.by_id[&id], Some("uuid-wk".to_string()));
        // Idempotent: binding the same pair again appends nothing new.
        let again = bind(&path, &id, "uuid-wk", &clock).unwrap();
        assert_eq!(again, BindOutcome::AlreadyBoundSameId);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "one mint + one bind: {text}");
        // mint_or_get after the bind returns the SAME id (the invariant: the id
        // in the child's env equals the id the registry surfaces, forever).
        let same = mint_or_get(&path, "uuid-wk", Some("wk"), &clock).unwrap();
        assert_eq!(same, id);
    }

    #[test]
    fn bind_first_wins_per_session_returns_existing_id() {
        // The bind-vs-existing-binding race: the session UUID already has an
        // id (earlier mint); a later bind for a fresh unbound mint must NOT
        // reassign — it returns the existing id and appends nothing.
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let clock = FixedClock(TS);
        let mut g1 = || "aaaaaaaa".to_string();
        let existing = mint_or_get_with(&path, "uuid-x", None, &clock, &mut g1).unwrap();
        let mut g2 = || "bbbbbbbb".to_string();
        let unbound = mint_unbound_with(&path, Some("wk"), &clock, &mut g2).unwrap();
        let got = bind(&path, &unbound, "uuid-x", &clock).unwrap();
        assert_eq!(
            got,
            BindOutcome::SessionHasDifferentId {
                existing: existing.clone()
            },
            "first mint wins; bind NAMES the winner (the caller's divergence case)"
        );
        let map = fold(&path);
        assert_eq!(map.by_session["uuid-x"], existing);
        assert_eq!(map.by_id["bbbbbbbb"], None, "garbage mint stays unbound");
    }

    #[test]
    fn bind_unknown_or_foreign_bound_id_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let clock = FixedClock(TS);
        let err = bind(&path, "zzzzzzzz", "uuid-a", &clock).unwrap_err();
        assert!(err.contains("unknown id"), "{err}");
        // An id bound to session A cannot be re-bound to session B.
        let mut g = || "aaaaaaaa".to_string();
        mint_or_get_with(&path, "uuid-a", None, &clock, &mut g).unwrap();
        let err = bind(&path, "aaaaaaaa", "uuid-b", &clock).unwrap_err();
        assert!(err.contains("different session"), "{err}");
        assert!(
            bind(&path, "aaaaaaaa", "", &clock).is_err(),
            "empty session_id refused"
        );
    }

    #[test]
    fn fold_ignores_bind_for_unknown_or_bound_id_and_second_bind() {
        let text = concat!(
            // bind BEFORE any mint of that id → ignored.
            r#"{"v":1,"ts":"t","event":"bind","id":"eeeeeeee","session_id":"s0"}"#,
            "\n",
            r#"{"v":1,"ts":"t","event":"mint","id":"aaaaaaaa","session_id":null,"name":"wk"}"#,
            "\n",
            r#"{"v":1,"ts":"t","event":"bind","id":"aaaaaaaa","session_id":"s1"}"#,
            "\n",
            // second bind for the same id → ignored (first-wins).
            r#"{"v":1,"ts":"t","event":"bind","id":"aaaaaaaa","session_id":"s2"}"#,
            "\n",
        );
        let map = fold_str(text);
        assert_eq!(map.by_session.len(), 1);
        assert_eq!(map.by_session["s1"], "aaaaaaaa");
        assert_eq!(map.by_id["aaaaaaaa"], Some("s1".to_string()));
        assert!(!map.by_id.contains_key("eeeeeeee"));
    }

    // --- collision regeneration (injected generator) ---

    #[test]
    fn collision_with_historical_id_regenerates() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let clock = FixedClock(TS);
        // Seed: s1 owns "aaaaaaaa" — note 'a' is in the alphabet.
        let mut first = || "aaaaaaaa".to_string();
        let a = mint_or_get_with(&path, "s1", None, &clock, &mut first).unwrap();
        assert_eq!(a, "aaaaaaaa");
        // s2's generator collides once, then yields a fresh id.
        let mut calls = 0;
        let mut colliding = || {
            calls += 1;
            if calls == 1 {
                "aaaaaaaa".to_string()
            } else {
                "bbbbbbbb".to_string()
            }
        };
        let b = mint_or_get_with(&path, "s2", None, &clock, &mut colliding).unwrap();
        assert_eq!(b, "bbbbbbbb", "collision regenerated");
        let map = fold(&path);
        assert_eq!(map.by_session["s1"], "aaaaaaaa");
        assert_eq!(map.by_session["s2"], "bbbbbbbb");
    }

    #[test]
    fn collision_against_shadowed_historical_id_still_regenerates() {
        // An id that appears ONLY on a fold-shadowed line (same session minted
        // twice — should be impossible under the lock, defense in depth) must
        // STILL be unavailable to new mints: the check is against ALL history.
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            concat!(
                r#"{"v":1,"ts":"t","event":"mint","id":"aaaaaaaa","session_id":"s1","name":null}"#,
                "\n",
                r#"{"v":1,"ts":"t","event":"mint","id":"shadowed","session_id":"s1","name":null}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut calls = 0;
        let mut generator = || {
            calls += 1;
            if calls == 1 {
                "shadowed".to_string()
            } else {
                "ffffffff".to_string()
            }
        };
        let id = mint_or_get_with(&path, "s2", None, &FixedClock(TS), &mut generator).unwrap();
        assert_eq!(id, "ffffffff");
    }

    #[test]
    fn exhausted_retries_error_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let clock = FixedClock(TS);
        let mut fixed = || "aaaaaaaa".to_string();
        mint_or_get_with(&path, "s1", None, &clock, &mut fixed).unwrap();
        // A generator that ALWAYS collides must error, not loop forever.
        let err = mint_or_get_with(&path, "s2", None, &clock, &mut fixed).unwrap_err();
        assert!(err.contains("collisions"), "{err}");
    }

    // --- concurrency (spec D1: required) ---

    #[test]
    fn concurrent_same_session_id_yields_exactly_one_id_and_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        const N: usize = 16;
        let ids: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..N)
                .map(|_| {
                    let path = path.clone();
                    scope.spawn(move || {
                        mint_or_get(&path, "shared-uuid", Some("w"), &FixedClock(TS)).unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let first = &ids[0];
        assert!(
            ids.iter().all(|i| i == first),
            "all callers got ONE id: {ids:?}"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let mint_lines = text
            .lines()
            .filter(|l| l.contains("\"shared-uuid\""))
            .count();
        assert_eq!(mint_lines, 1, "exactly one mint line: {text}");
        assert_eq!(text.lines().count(), 1);
    }

    #[test]
    fn concurrent_distinct_session_ids_no_collisions_no_torn_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        const N: usize = 24;
        let ids: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..N)
                .map(|i| {
                    let path = path.clone();
                    scope.spawn(move || {
                        mint_or_get(&path, &format!("uuid-{i}"), None, &FixedClock(TS)).unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let set: HashSet<&String> = ids.iter().collect();
        assert_eq!(set.len(), N, "no id collisions: {ids:?}");
        // Every line parses (no interleaved/torn lines) and the fold sees all N.
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), N);
        for line in text.lines() {
            let v: serde_json::Value =
                serde_json::from_str(line).expect("every line parses cleanly");
            assert_eq!(v["event"], "mint");
        }
        let map = fold(&path);
        assert_eq!(map.by_session.len(), N);
        assert_eq!(map.by_id.len(), N);
    }

    // --- prefixes ---

    #[test]
    fn shortest_unique_prefixes_min_two_chars() {
        let ids = vec!["ab3kx9mq".to_string(), "cd47qrst".to_string()];
        assert_eq!(shortest_unique_prefixes(&ids), vec!["ab", "cd"]);
    }

    #[test]
    fn shortest_unique_prefixes_extends_past_shared_prefix() {
        let ids = vec![
            "ab3kx9mq".to_string(),
            "ab47qrst".to_string(),
            "ab4mnpqr".to_string(),
        ];
        assert_eq!(shortest_unique_prefixes(&ids), vec!["ab3", "ab47", "ab4m"]);
    }

    #[test]
    fn shortest_unique_prefixes_full_id_when_needed() {
        // Two ids differing only in the LAST char → full ids.
        let ids = vec!["ab3kx9mq".to_string(), "ab3kx9mr".to_string()];
        assert_eq!(shortest_unique_prefixes(&ids), vec!["ab3kx9mq", "ab3kx9mr"]);
        // Degenerate duplicates → full id (never a panic).
        let dup = vec!["ab3kx9mq".to_string(), "ab3kx9mq".to_string()];
        assert_eq!(shortest_unique_prefixes(&dup), vec!["ab3kx9mq", "ab3kx9mq"]);
    }

    #[test]
    fn single_id_gets_two_char_prefix() {
        let ids = vec!["ab3kx9mq".to_string()];
        assert_eq!(shortest_unique_prefixes(&ids), vec!["ab"]);
    }

    /// P0 QA (spec-w4-qa B): prefix DISPLAY DETERMINISM with many ids forced to
    /// share 2-char prefixes (minted through the REAL path via the injected
    /// generator seam, not a hand-built vec). The `id → prefix` mapping must be
    /// a pure function of the id SET — invariant under list order — and every
    /// rendered prefix must stay unique within the set (rendered handles feed
    /// back through the resolution prefix tier; an ambiguous handle would
    /// refuse on the very surface that displayed it).
    #[test]
    fn prefix_map_is_order_invariant_and_unique_under_crowding() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let clock = FixedClock(TS);
        // 12 ids across 3 crowded 2-char families (ab/cd/ef) + one loner.
        let forced = [
            "ab222222", "ab233333", "ab244444", "ab255555", "cd222222", "cd333333", "cd344444",
            "ef222222", "ef333333", "ef444444", "ef455555", "zz222222",
        ];
        let mut ids: Vec<String> = Vec::new();
        for (i, want) in forced.iter().enumerate() {
            let mut gen = || want.to_string();
            let id = mint_or_get_with(&path, &format!("uuid-{i}"), None, &clock, &mut gen).unwrap();
            assert_eq!(&id, want, "generator-injected id minted verbatim");
            ids.push(id);
        }

        let baseline: HashMap<String, String> = ids
            .iter()
            .cloned()
            .zip(shortest_unique_prefixes(&ids))
            .collect();
        let mut seen = HashSet::new();
        for (id, p) in &baseline {
            assert!(p.len() >= 2, "min-2 violated: {id}→{p}");
            assert!(id.starts_with(p.as_str()), "{p} is not a prefix of {id}");
            assert!(seen.insert(p.clone()), "duplicate display prefix {p}");
            assert_eq!(
                ids.iter().filter(|o| o.starts_with(p.as_str())).count(),
                1,
                "prefix {p} is ambiguous within the displayed set"
            );
        }
        // Order invariance: reversed / rotated / sorted permutations all yield
        // the SAME id→prefix mapping (ls render order must not change handles).
        let mut reversed = ids.clone();
        reversed.reverse();
        let mut rotated = ids.clone();
        rotated.rotate_left(5);
        let mut sorted = ids.clone();
        sorted.sort();
        for perm in [reversed, rotated, sorted] {
            let m: HashMap<String, String> = perm
                .iter()
                .cloned()
                .zip(shortest_unique_prefixes(&perm))
                .collect();
            assert_eq!(m, baseline, "prefix map must be order-invariant");
        }
    }

    /// Minimal cold `Session` row for the lineage fill test.
    fn sess(session_id: &str) -> Session {
        Session {
            name: None,
            user_named: None,
            session_id: session_id.to_string(),
            code: None,
            qd_id: None,
            pid: None,
            status: crate::model::SessionStatus::Cold,
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
            which_branch: crate::model::SessionBranch::ColdJsonl,
        }
    }

    #[test]
    fn lineage_round_trips_and_fill_surfaces_parent_qdid_not_own() {
        // WP-B5-iii obl-4: record a fork→parent lineage pointer, fold it back, and
        // surface it onto the fork's Session via the SAME `fill_lineage` the live
        // `qd ls` join (`common::all_sessions`) runs — discovery through the
        // resolver path, not a raw-row assert.
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let clock = FixedClock(TS);
        let fork_uuid = "fork-uuid-1111";
        // The fork has its OWN qdId (minted from its own uuid)…
        let fork_own = mint_or_get(&path, fork_uuid, Some("forked"), &clock).unwrap();
        // …and a lineage pointer to the PARENT instance's qdId.
        let parent_qd = "parentqd";
        record_lineage(&path, fork_uuid, parent_qd, &clock).unwrap();

        let map = fold(&path);
        assert_eq!(
            map.by_parent.get(fork_uuid),
            Some(&parent_qd.to_string()),
            "lineage event folds into by_parent"
        );
        assert_eq!(
            map.by_session.get(fork_uuid),
            Some(&fork_own),
            "fork keeps its OWN qdId"
        );

        // fill_lineage sets lineage = PARENT qdId, NOT the fork's own id (no leak).
        let mut rows = vec![sess(fork_uuid)];
        fill_qd_ids(&mut rows, &map);
        fill_lineage(&mut rows, &map);
        assert_eq!(
            rows[0].qd_id.as_deref(),
            Some(fork_own.as_str()),
            "fork's OWN qdId"
        );
        assert_eq!(
            rows[0].lineage.as_deref(),
            Some(parent_qd),
            "lineage = PARENT qdId"
        );
        assert_ne!(
            rows[0].lineage, rows[0].qd_id,
            "GUARDRAIL: lineage is the PARENT pointer, never the fork's own id"
        );

        // Idempotent: a second record appends nothing (first-wins).
        record_lineage(&path, fork_uuid, parent_qd, &clock).unwrap();
        assert_eq!(
            fold(&path).by_parent.len(),
            1,
            "lineage record is idempotent"
        );

        // RED-BEFORE oracle: with NO lineage event, fill_lineage leaves None — so
        // the fill is load-bearing, not vacuously always-Some.
        let dir2 = tempfile::tempdir().unwrap();
        let path2 = store(&dir2);
        mint_or_get(&path2, fork_uuid, Some("forked"), &FixedClock(TS)).unwrap();
        let mut rows2 = vec![sess(fork_uuid)];
        fill_lineage(&mut rows2, &fold(&path2));
        assert_eq!(
            rows2[0].lineage, None,
            "no lineage event ⇒ Session.lineage stays None"
        );
    }
}
