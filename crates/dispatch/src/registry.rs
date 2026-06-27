//! PID registry: read/write/tombstones + the atomic name-claim primitive.
//! Ported from TS `src/session.ts:325-429` (getPidEntries, tombstoneSession,
//! readRegistryEntry, ensureTombstone, getTombstonedEntries).
//!
//! # The registry is a DISPOSABLE SNAPSHOT of the event stream
//!
//! Per ADD-3/3a/3b (Pete-ruled): the durable thing is the append-only event
//! stream `marks.jsonl` (owned by the engine from A3); the on-disk
//! `<pid>.json` registry is a rebuildable snapshot of that stream. The
//! rebuild-from-marks path (A6) is literally "iterate marks → [`write_entry`]"
//! with NO schema change here. This module implements NEITHER marks NOR
//! rollups — only the snapshot read/write/tombstone surface and the
//! atomic-claim primitive.
//!
//! # Lineage: `spawned_by` ONLY
//!
//! [`RegistryEntry`] carries exactly one lineage field, `spawned_by`
//! (`spawnedBy` on disk). There is deliberately NO `on_behalf_of` and NO
//! enumerated org-vocabulary event type anywhere in this module (ADD-3a/3b):
//! all org vocabulary is qb-side content flowing through the dumb `qd mark`
//! verb (A3); `marks.jsonl` payloads are OPAQUE to the engine.
//!
//! # Permissive parsing (L8 / CONVENTIONS.md)
//!
//! Read side is `#[serde(default)]` + all-`Option`, no `deny_unknown_fields`:
//! legacy / missing-field / unknown-field registry blobs parse; a genuinely
//! corrupt blob fails CLEANLY (`Err`/`None`), never panics, never aborts a
//! directory scan. Write side skips `None` fields so written files stay
//! minimal and byte-compatible with what TS reads permissively.
//!
//! # Per-field-permissive read (A4 pass-(b) F3, owner A1 pass-(b))
//!
//! `#[serde(default)]` covers MISSING fields, not WRONG-TYPED ones: with a plain
//! whole-struct `serde_json::from_slice`, a single wrong-typed field (e.g. a
//! string `"12345"` where `startedAt` is declared `i64`) makes the WHOLE row fail
//! to deserialize — Rust would drop the row entirely and the session goes silently
//! invisible to `ls`/`resolve`. That is STRICTER than the TS dynamic read, which
//! renders the row (with the raw value in place), and against the repo
//! permissive-parse rule in spirit (A4 pass-(b) F3, orc-3-ruled, owner A1).
//!
//! The fix is [`RegistryEntry::from_value`]: parse each field independently from a
//! `serde_json::Value` object; a wrong-typed field DEGRADES to its default
//! (`None`) and THE ROW SURVIVES, with the degraded field names carried out for
//! observability. All read paths ([`read_entries`], [`read_entry`],
//! [`get_tombstoned_entries`]) funnel through it via [`parse_file`]. Genuinely
//! corrupt input (not valid JSON, or a non-object top-level value) keeps the old
//! behavior: a clean skip / `None`, never a panic (L8).
//!
//! ## Observability (design call — degraded rows must be DETECTABLE)
//!
//! A degraded row must not vanish silently. Two surfaces carry the signal:
//!   1. the degraded field-name list rides on the read-path return types
//!      ([`ScannedEntry::degraded`], [`TombstonedEntry::degraded`]) so a future
//!      `qd doctor` verb (later phase) can report it as the user-facing surface;
//!   2. one stderr warning line per degraded (or genuinely-skipped) file, gated
//!      behind `SB_DEBUG=1` and SILENT by default — stdout byte-parity surfaces
//!      (`ls --json`, etc.) MUST NOT change for well-typed fixtures, and the
//!      default-silent gate guarantees that. See [`debug_warn`].
//!
//! ## NAMED residual: Rust renders the row defaulted; TS renders the raw value
//!
//! Coercion policy is `serde_json::from_value` PER FIELD — there is deliberately
//! NO string→number coercion. A string `startedAt` degrades to `None`; the row
//! survives with that field defaulted. This matches "default + survive" but NOT
//! TS's "render the raw string in place". So for a wrong-typed row, byte-parity is
//! NOT claimed: TS shows the raw value, Rust shows the field defaulted. The
//! contract A1 pass-(b) delivers is the row's PRESENCE (the session is visible
//! again), not byte-identity of a corrupt field. This is the sanctioned shape of
//! survival.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A PID registry entry. Port of TS `PidEntry` (src/session.ts:64-75) PLUS the
/// two new fields `backend` and `spawned_by`.
///
/// On-disk field names are TS camelCase (`sessionId`, `startedAt`, `updatedAt`,
/// `spawnedBy`, `backend`) so the write path round-trips byte-compatibly with
/// what the TS reads. Every field is `Option` and the read side is permissive
/// (L8): missing/legacy/extra fields never fail a parse. The write side skips
/// `None` so files stay minimal.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RegistryEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    /// NEW (brief deliverable 3): the mux/runtime backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// NEW: the ONLY lineage field (ADD-3a SUPERSEDES ADD-3). NO `on_behalf_of`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawned_by: Option<String>,
    /// NEW (codex P1, R1): the session's provider id. Absent on disk =
    /// claude-code AT THE READ-BACK BOUNDARY (the join supplies the default —
    /// never materialized on disk, so pre-existing rows, tombstones and goldens
    /// stay byte-stable; codex-p1-spec section 3.1). On-disk key is `provider`
    /// (a single lowercase word — `rename_all = "camelCase"` is a no-op on it;
    /// pinned by a round-trip test, not assumed). Appended LAST so the
    /// serialized key order of every existing field is untouched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// codex P2 W4 (codex-p2-spec sections 7.2, 9.2): the daemon-hosted session's
    /// recorded ws endpoint (`ws://127.0.0.1:<port>`). Internal surface — qd
    /// reconnects to it per-verb (claude rows NEVER carry it). skip-None +
    /// appended LAST keeps every existing row / tombstone / golden byte-stable
    /// (the same R1 / `provider` pattern), pinned by the absent-stays-absent
    /// round-trip test. On-disk key is `endpoint` (a single lowercase word —
    /// `rename_all = "camelCase"` is a no-op on it; pinned, not assumed). NOT in
    /// `--json` (endpoint stays OUT of the human/agent JSON surface, §9.4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// scoped-ACP-CC (A2 §A3-ACP, the F1.1 tier-degradation field): the ONE
    /// persisted degradation latch for the §4 fallback ladder. Absent = the
    /// DERIVED tier applies (a healthy session needs NO field — the tier is
    /// derived per verb from `(provider, transport-field, endpoint-alive)`);
    /// written ONLY on degradation (e.g. `"pty"` when ACP dropped to the Mode-B
    /// floor — `InjectError::NoTransport`). skip-None + appended LAST keeps every
    /// existing row / tombstone / golden byte-stable (the same R1 / `provider` /
    /// `endpoint` pattern), pinned by the absent-stays-absent round-trip test. On-disk
    /// key is `transport` (a single lowercase word — `rename_all = "camelCase"` is a
    /// no-op on it; pinned, not assumed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

impl RegistryEntry {
    /// Per-field-permissive read (A4 pass-(b) F3, owner A1 pass-(b)).
    ///
    /// `#[serde(default)]` covers MISSING fields, not WRONG-TYPED ones. A plain
    /// whole-struct deserialize drops the WHOLE row when one field is wrong-typed
    /// (e.g. string `"12345"` where `startedAt` is `i64`) — stricter than the TS
    /// dynamic read (which renders the row with the raw value) and against the
    /// repo permissive-parse rule in spirit.
    ///
    /// This reads each field independently from a JSON object: a wrong-typed field
    /// DEGRADES to its default (`None`) and the row SURVIVES; the degraded field
    /// names are returned for observability. Coercion policy is
    /// `serde_json::from_value` per field — NO string→number coercion: a string
    /// `startedAt` becomes `None`, NOT TS's raw-string render. That is the NAMED,
    /// sanctioned residual (module doc): the row's PRESENCE is the contract, not
    /// byte-parity of a wrong-typed field.
    ///
    /// Returns `None` when `v` is not a JSON object (a genuinely-corrupt /
    /// non-object value is a clean skip, matching the old whole-struct `Err`).
    /// Unknown fields are ignored (no `deny_unknown_fields`), exactly as the
    /// derive-based read did.
    pub fn from_value(v: Value) -> Option<(RegistryEntry, Vec<&'static str>)> {
        let mut obj = match v {
            Value::Object(map) => map,
            // Non-object top-level value (number, string, array, ...) is not a
            // registry row — skip cleanly, as the whole-struct read would Err.
            _ => return None,
        };
        let mut degraded: Vec<&'static str> = Vec::new();
        let mut entry = RegistryEntry::default();

        // Helper: pull a field by its on-disk (camelCase) key, parse it as T; on a
        // type error degrade to default and record the (snake_case) field name. A
        // genuinely-absent key is a no-op (stays default — the MISSING case).
        macro_rules! field {
            ($target:expr, $ty:ty, $disk_key:literal, $field_name:literal) => {
                if let Some(raw) = obj.remove($disk_key) {
                    match serde_json::from_value::<$ty>(raw) {
                        Ok(val) => $target = val,
                        Err(_) => degraded.push($field_name),
                    }
                }
            };
        }

        field!(entry.pid, Option<i64>, "pid", "pid");
        field!(entry.session_id, Option<String>, "sessionId", "sessionId");
        field!(entry.cwd, Option<String>, "cwd", "cwd");
        field!(entry.started_at, Option<i64>, "startedAt", "startedAt");
        field!(entry.updated_at, Option<i64>, "updatedAt", "updatedAt");
        field!(entry.status, Option<String>, "status", "status");
        field!(entry.name, Option<String>, "name", "name");
        field!(entry.version, Option<String>, "version", "version");
        field!(entry.kind, Option<String>, "kind", "kind");
        field!(entry.entrypoint, Option<String>, "entrypoint", "entrypoint");
        field!(entry.backend, Option<String>, "backend", "backend");
        field!(entry.spawned_by, Option<String>, "spawnedBy", "spawnedBy");
        // codex P1, R1 (codex-p1-spec section 3.1): per-field-permissive provider
        // read. Dropping THIS row would silently lose the provider field on a
        // wrong-typed row (and a whole-struct parse would drop the whole row) —
        // the `wrong_typed_provider_number_degrades` test below is the mutation
        // evidence: removing this line reds it (the row would carry no provider /
        // would vanish).
        field!(entry.provider, Option<String>, "provider", "provider");
        // codex P2 W4 (codex-p2-spec section 7.2): per-field-permissive endpoint
        // read. A wrong-typed `"endpoint": 7` DEGRADES (row survives, "endpoint"
        // named) instead of dropping the whole row. The
        // `wrong_typed_endpoint_number_degrades` test below is the mutation
        // evidence: removing this line reds it (the endpoint would silently vanish
        // / a whole-struct parse would drop the row).
        field!(entry.endpoint, Option<String>, "endpoint", "endpoint");
        // scoped-ACP-CC (A2 §A3-ACP): per-field-permissive transport read. A
        // wrong-typed `"transport": 1` DEGRADES (row survives, "transport" named)
        // instead of dropping the whole row — same R1 discipline as provider/endpoint.
        // The `wrong_typed_transport_number_degrades` test is the mutation evidence.
        field!(entry.transport, Option<String>, "transport", "transport");
        // Any remaining keys are unknown fields — ignored (permissive).

        Some((entry, degraded))
    }
}

/// A read entry plus whether it came from a `.tombstoned` file.
///
/// Divergence-from-literal-port: TS marks tombstoned entries by mutating a
/// runtime `_tombstoned` flag onto the parsed object (session.ts:338). We keep
/// the wire struct clean and carry the flag alongside instead, so M4's join can
/// distinguish live vs tombstoned without a sentinel field leaking into the
/// schema (and so a `_tombstoned` key never round-trips back to disk).
#[derive(Debug, Clone, PartialEq)]
pub struct ScannedEntry {
    pub entry: RegistryEntry,
    pub tombstoned: bool,
    /// Field names that were WRONG-TYPED on disk and degraded to default (A4
    /// pass-(b) F3). Empty for a clean row. Carried so a future `qd doctor` verb
    /// can surface degraded rows; the row itself still appears in `ls`.
    pub degraded: Vec<&'static str>,
}

/// A tombstoned registry file with its on-disk metadata. Port of the return
/// shape of `getTombstonedEntries` (session.ts:413).
#[derive(Debug, Clone, PartialEq)]
pub struct TombstonedEntry {
    pub path: PathBuf,
    /// PID parsed from the filename (`<pid>.json.tombstoned`).
    pub pid: i64,
    pub data: RegistryEntry,
    /// File mtime in ms since the Unix epoch (TS exposes a `Date`; we expose ms
    /// so the decider layer never touches `SystemTime`).
    pub mtime_ms: i64,
    /// Wrong-typed field names degraded to default on this tombstone (A4
    /// pass-(b) F3). Empty for a clean row.
    pub degraded: Vec<&'static str>,
}

// --- Read paths (session.ts:327-429) ---

/// Scan the sessions dir for registry entries. Port of `getPidEntries`
/// (session.ts:327-355).
///
/// Permissive (L8 / TS `catch {}`): an unparseable `<x>.json` file is SKIPPED
/// silently — never fails the scan. A missing dir yields an empty vec (TS outer
/// `catch { return [] }`). Tombstoned (`.tombstoned`) files are included only
/// when `include_tombstoned`.
pub fn read_entries(sessions_dir: &Path, include_tombstoned: bool) -> Vec<ScannedEntry> {
    let rd = match fs::read_dir(sessions_dir) {
        Ok(rd) => rd,
        // Missing dir / unreadable → empty (TS outer catch).
        Err(_) => return Vec::new(),
    };
    let mut entries = Vec::new();
    for dent in rd.flatten() {
        let name = dent.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".tombstoned") {
            if !include_tombstoned {
                continue;
            }
            // Per-file catch {}: a corrupt tombstone is skipped, scan continues.
            if let Some((entry, degraded)) = parse_file(&dent.path()) {
                entries.push(ScannedEntry {
                    entry,
                    tombstoned: true,
                    degraded,
                });
            }
            continue;
        }
        if !name.ends_with(".json") {
            continue;
        }
        if let Some((entry, degraded)) = parse_file(&dent.path()) {
            entries.push(ScannedEntry {
                entry,
                tombstoned: false,
                degraded,
            });
        }
    }
    entries
}

/// Outcome of [`pick_live_named_row`] — which scanned row (if any) the
/// bind-at-boot-confirm path may bind to (P0 redfix F1).
#[derive(Debug, Clone, PartialEq)]
pub enum LiveNamePick {
    /// Exactly one ALIVE row holds the name and carries a non-empty sessionId —
    /// the normal path: bind to it.
    One { session_id: String },
    /// No bindable row: zero ALIVE rows hold the name, OR the unique alive row
    /// carries no (or an empty) sessionId yet. The caller's existing
    /// "no sessionId yet" warning path.
    NoneBindable,
    /// More than one ALIVE row claims the name — do NOT bind; that two running
    /// sessions claim one name is itself an anomaly the user must see.
    Ambiguous { count: usize },
}

/// Pick the registry row the bind-at-boot-confirm path should bind a pre-minted
/// stable id to (P0 redfix F1): among NON-tombstoned rows whose `name` matches,
/// only rows whose `pid` is ALIVE (per the injected `is_alive` predicate — the
/// `effects::is_pid_alive` seam in production) are candidates. A row without a
/// pid is never alive.
///
/// WHY the liveness filter: a crash-leftover same-name dead row (non-tombstoned)
/// could otherwise win by read-dir order and the caller would bind the fresh
/// session's id to a STALE row's UUID, silently.
pub fn pick_live_named_row(
    rows: &[ScannedEntry],
    name: &str,
    is_alive: &dyn Fn(i64) -> bool,
) -> LiveNamePick {
    let alive: Vec<&ScannedEntry> = rows
        .iter()
        .filter(|s| {
            !s.tombstoned
                && s.entry.name.as_deref() == Some(name)
                && s.entry.pid.is_some_and(is_alive)
        })
        .collect();
    match alive.as_slice() {
        [] => LiveNamePick::NoneBindable,
        [only] => match only.entry.session_id.as_deref() {
            Some(sid) if !sid.is_empty() => LiveNamePick::One {
                session_id: sid.to_string(),
            },
            _ => LiveNamePick::NoneBindable,
        },
        many => LiveNamePick::Ambiguous { count: many.len() },
    }
}

/// Read one live registry entry by PID. Port of `readRegistryEntry`
/// (session.ts:378-384): returns the parsed live `<pid>.json`, else `None`
/// (missing OR corrupt — TS `catch { return undefined }`).
pub fn read_entry(sessions_dir: &Path, pid: i64) -> Option<RegistryEntry> {
    parse_file(&sessions_dir.join(format!("{pid}.json"))).map(|(entry, _degraded)| entry)
}

/// Get all tombstoned registry files with metadata. Port of
/// `getTombstonedEntries` (session.ts:413-429). PID is parsed from the filename
/// (`<pid>.json.tombstoned`); files whose name doesn't yield a PID, or whose
/// body is corrupt, or whose mtime is unreadable, are skipped (TS per-file
/// `catch {}`).
pub fn get_tombstoned_entries(sessions_dir: &Path) -> Vec<TombstonedEntry> {
    let rd = match fs::read_dir(sessions_dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut results = Vec::new();
    for dent in rd.flatten() {
        let name = dent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".tombstoned") {
            continue;
        }
        let path = dent.path();
        // PID from filename: strip ".json.tombstoned" (TS replace).
        let pid = match name
            .strip_suffix(".json.tombstoned")
            .and_then(|s| s.parse::<i64>().ok())
        {
            Some(pid) => pid,
            None => continue,
        };
        let (data, degraded) = match parse_file(&path) {
            Some(d) => d,
            None => continue,
        };
        let mtime_ms = match file_mtime_ms(&path) {
            Some(ms) => ms,
            None => continue,
        };
        results.push(TombstonedEntry {
            path,
            pid,
            data,
            mtime_ms,
            degraded,
        });
    }
    results
}

/// Read+parse a registry file per-field-permissively (A4 pass-(b) F3). Returns
/// `Some((entry, degraded))` for any valid JSON object — a wrong-typed field
/// degrades to default and the row SURVIVES, its name pushed into `degraded`.
/// Returns `None` if the file is missing, is not valid JSON at all, or is a
/// valid-but-non-object JSON value (genuinely corrupt → clean skip, the TS
/// per-file `catch {}` / `catch { undefined }`). NEVER panics (L8).
///
/// Observability: a degraded OR genuinely-skipped file emits ONE stderr line
/// gated behind `SB_DEBUG=1` (silent by default — see [`debug_warn`]).
fn parse_file(path: &Path) -> Option<(RegistryEntry, Vec<&'static str>)> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        // Missing/unreadable file: silent (not a corruption signal).
        Err(_) => return None,
    };
    // Parse to a generic Value first so per-field parsing can survive wrong types.
    // A genuine JSON syntax error (not valid JSON at all) is a clean skip.
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            debug_warn(&format!(
                "skipped corrupt registry file (invalid JSON): {}",
                path.display()
            ));
            return None;
        }
    };
    match RegistryEntry::from_value(value) {
        Some((entry, degraded)) => {
            if !degraded.is_empty() {
                debug_warn(&format!(
                    "registry file has wrong-typed field(s) {:?}, degraded to default \
                     (row survives): {}",
                    degraded,
                    path.display()
                ));
            }
            Some((entry, degraded))
        }
        None => {
            // Valid JSON but not an object (number/string/array/...): not a row.
            debug_warn(&format!(
                "skipped registry file (valid JSON but not an object): {}",
                path.display()
            ));
            None
        }
    }
}

/// Emit ONE diagnostic line to stderr, gated behind `SB_DEBUG=1`. SILENT by
/// default so stdout byte-parity surfaces (`ls --json`, ...) never change. WHY a
/// decision function: a future `qd doctor` verb (later phase) is the user-facing
/// surface for degraded/skipped rows; until then this debug gate is the only
/// signal, and factoring it lets tests assert the message shape directly without
/// capturing process stderr. Returns the formatted line (for testability) and
/// writes it only when the gate is on.
fn debug_warn(msg: &str) -> Option<String> {
    if std::env::var_os("SB_DEBUG").is_some_and(|v| v == "1") {
        let line = format!("qd[registry]: {msg}");
        eprintln!("{line}");
        Some(line)
    } else {
        None
    }
}

/// File mtime in ms since the Unix epoch.
fn file_mtime_ms(path: &Path) -> Option<i64> {
    let mtime = fs::metadata(path).ok()?.modified().ok()?;
    let dur = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_millis() as i64)
}

// --- Write paths ---

/// Write a registry entry to `<sessions_dir>/<pid>.json`, pretty-printed with
/// 2-space indentation and NO trailing newline — matching TS
/// `JSON.stringify(data, null, 2)` written via `fs.writeFile` (which adds no
/// trailing newline; verified against src/session.ts:405). Creates the dir
/// recursively.
///
/// The entry's own `pid` field names the file (the TS registry keys files by
/// PID). Factored as `write_entry(dir, &RegistryEntry)` so a future
/// rebuild-from-marks (A6) is "iterate marks → write_entry" with no schema
/// change (ADD-3).
///
/// # Atomicity (§H.6 "atomic rename")
///
/// The row is written to a tmp file in the SAME directory
/// (`.<pid>.json.tmp.<nonce>`) then `fs::rename`d into place. On POSIX a rename
/// within a directory is atomic, so a concurrent reader NEVER observes a torn /
/// partial row — it sees the OLD complete row or the NEW complete row, never a
/// half-written one. `<nonce>` is a process-unique value (`pid` of THIS process
/// combined with a monotonic `static AtomicU64`) so two in-process writers can
/// never collide on the tmp name; no randomness is used (deterministic, testable).
/// On any error the tmp is best-effort removed so a crash mid-write leaves no
/// litter. Byte-format parity (`to_string_pretty`, no trailing newline — the TS
/// `JSON.stringify(.., null, 2)` shape) is preserved exactly.
///
/// # CAS-ENFORCEMENT GATE (R3d / R1 §7 inv 1 — code-review invariant)
///
/// `write_entry` is the row-CREATION / full-row-rebuild primitive (boot owns
/// creation: `create_daemon` / `resume_daemon` / the `daemon_status` mint
/// fallback). It performs NO incarnation CAS. A **status TRANSITION** of an
/// EXISTING row MUST go through [`set_status`] (the starttime-CAS-guarded path),
/// NEVER through a raw `write_entry` that flips the `status` field — a bare
/// `write_entry` would let a stale incarnation stomp the current one (a LOST
/// UPDATE under concurrency; proven RED by the R3d CONCURRENCY negative control
/// `class_r3d_*` in `tests/faultinj.rs`). Reviewers: a new `write_entry` call that
/// mutates `status` on an already-persisted row is a defect — route it through
/// `set_status`. The current production `write_entry` callers
/// (`create_daemon.rs`, `resume_daemon.rs`, `daemon_status.rs:268`) all CREATE the
/// row; none transition status on an existing one.
pub fn write_entry(sessions_dir: &Path, entry: &RegistryEntry) -> io::Result<()> {
    let pid = entry.pid.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "write_entry: RegistryEntry.pid is required to name the file",
        )
    })?;
    let json = serde_json::to_string_pretty(entry)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let path = sessions_dir.join(format!("{pid}.json"));
    atomic_write(sessions_dir, &path, pid, json.as_bytes())
}

/// Monotonic per-process counter feeding the tmp-file nonce — combined with the
/// writer process's pid it makes every tmp name unique WITHOUT randomness
/// (deterministic + testable; §H.6).
static TMP_NONCE: AtomicU64 = AtomicU64::new(0);

/// Atomically write `bytes` to `final_path` (which lives in `sessions_dir`) via
/// tmp-write + `fs::rename`. `pid` names the final file and seeds the tmp nonce.
/// Models the atomic-rename idiom on [`tombstone`]. Best-effort removes the tmp
/// on any failure.
fn atomic_write(sessions_dir: &Path, final_path: &Path, pid: i64, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(sessions_dir)?;
    let nonce = TMP_NONCE.fetch_add(1, Ordering::Relaxed);
    // Process-unique: this writer's pid + a monotonic counter. Same dir as the
    // final file so the rename is a same-filesystem (atomic) rename.
    let tmp_path = sessions_dir.join(format!(".{pid}.json.tmp.{}.{nonce}", std::process::id()));
    // Scope the file handle so it is closed before the rename.
    let write_res = (|| -> io::Result<()> {
        let mut f = File::create(&tmp_path)?;
        f.write_all(bytes)?;
        Ok(())
    })();
    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp_path, final_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(())
}

/// Outcome of a CAS-guarded [`set_status`] call.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusWriteOutcome {
    /// The status was written (adopted the incarnation) and the row persisted.
    Written,
    /// A DIFFERENT incarnation owns this pid's row (`started_at` on disk is
    /// `Some(d)` with `d != expected_started_at`). The on-disk row was left
    /// UNTOUCHED — no split-brain stomp.
    Rejected { on_disk_started_at: Option<i64> },
    /// No `<pid>.json` exists. Status updates NEVER create a row (boot owns
    /// creation) — nothing was written.
    NoRow,
}

/// Atomically set the `status` field of an EXISTING registry row, guarded by a
/// starttime CAS (§H.6 gate condition D6: no split-brain on a daemon restart or
/// botched resume).
///
/// - Reads `<pid>.json`; if absent → [`StatusWriteOutcome::NoRow`] (status
///   updates never CREATE a row — boot owns creation).
/// - CAS: if the on-disk `started_at` is `Some(d)` and `d != expected_started_at`
///   → [`StatusWriteOutcome::Rejected`] (a DIFFERENT incarnation owns this pid's
///   row; refuse to stomp it). The on-disk row is left UNTOUCHED. The stamp is an
///   EXACT epoch-ms recorded at boot, so this is an exact match (NOT the slack
///   comparison `claim_name` uses for live-proc start times).
/// - If on-disk `started_at` is `None`, OR `== expected_started_at` → ADOPT: set
///   `status` + `updated_at = now_ms`, PRESERVE every other field, write
///   atomically (via [`write_entry`]'s tmp+rename). → [`StatusWriteOutcome::Written`].
///
/// Idempotent: calling twice with the same `(status, expected_started_at)`
/// converges — status stays stable, the row stays valid, a single file persists;
/// only `updated_at` refreshes (an intended liveness heartbeat, not a violation).
///
/// `now_ms` is injected (no `SystemTime` inside) — matching the codebase's
/// testable-purity convention (cf. [`claim_name`] injecting `is_alive`/`proc_start`).
pub fn set_status(
    sessions_dir: &Path,
    pid: i64,
    expected_started_at: Option<i64>,
    status: &str,
    now_ms: i64,
) -> io::Result<StatusWriteOutcome> {
    // Read the live row. Absent / unparseable → NoRow (never create on a status
    // update). `read_entry` already returns None for both missing and corrupt.
    let mut entry = match read_entry(sessions_dir, pid) {
        Some(e) => e,
        None => return Ok(StatusWriteOutcome::NoRow),
    };
    // CAS on the incarnation stamp. A populated on-disk stamp that disagrees with
    // the writer's expected incarnation = a foreign incarnation owns this row:
    // refuse (no split-brain). A `None` on-disk stamp, or an exact match, adopts.
    // A `None` expected stamp adopts unconditionally (the writer asserts no
    // specific incarnation).
    if let (Some(disk), Some(expected)) = (entry.started_at, expected_started_at) {
        if disk != expected {
            return Ok(StatusWriteOutcome::Rejected {
                on_disk_started_at: Some(disk),
            });
        }
    }
    // Adopt: flip status, refresh the heartbeat, preserve all other fields, write
    // atomically.
    entry.status = Some(status.to_string());
    entry.updated_at = Some(now_ms);
    write_entry(sessions_dir, &entry)?;
    Ok(StatusWriteOutcome::Written)
}

/// Tombstone a live registry file. Port of `tombstoneSession`
/// (session.ts:362-372): rename `<pid>.json` → `<pid>.json.tombstoned`. Returns
/// `true` if a rename happened, `false` if the live `<pid>.json` was absent (TS
/// `catch { return false }`).
pub fn tombstone(sessions_dir: &Path, pid: i64) -> bool {
    let json_path = sessions_dir.join(format!("{pid}.json"));
    let tomb_path = sessions_dir.join(format!("{pid}.json.tombstoned"));
    fs::rename(&json_path, &tomb_path).is_ok()
}

/// Ensure a tombstone artifact exists for a killed PID. Port of
/// `ensureTombstone` (session.ts:392-408).
///
/// Idempotent: if a tombstone already exists, no-op. If the live `<pid>.json`
/// is still present, rename it. Otherwise — Claude Code already removed its own
/// registry file on graceful shutdown (SIGTERM) — write a tombstone from
/// `captured` data (if any) so an audit trail remains.
pub fn ensure_tombstone(sessions_dir: &Path, pid: i64, captured: Option<&RegistryEntry>) {
    let tomb_path = sessions_dir.join(format!("{pid}.json.tombstoned"));
    // Already tombstoned? done.
    if tomb_path.exists() {
        return;
    }
    // Live file present → rename it.
    if tombstone(sessions_dir, pid) {
        return;
    }
    // Otherwise synthesize a tombstone from captured data, if we have it.
    if let Some(captured) = captured {
        // Best-effort (TS catch {}): a failed mkdir/write leaves no tombstone
        // rather than aborting the kill path.
        if fs::create_dir_all(sessions_dir).is_ok() {
            if let Ok(json) = serde_json::to_string_pretty(captured) {
                let _ = fs::write(&tomb_path, json);
            }
        }
    }
}

/// Outcome of a CAS-guarded tombstone (R3c-Step-2 P5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TombstoneOutcome {
    /// Renamed `<pid>.json` → `.tombstoned` (the row matched the expected
    /// incarnation, OR there was a live row and no incarnation was asserted).
    Tombstoned,
    /// The live `<pid>.json` was absent — nothing to rename (and, for the
    /// `ensure_*` variant, a synthesized tombstone was written if `captured`).
    Absent,
    /// REFUSED: the on-disk `started_at` changed since `expected_started_at` was
    /// captured (a new incarnation reused the PID during a reconcile window).
    /// Tombstoning it would kill a live successor — the row is left UNTOUCHED.
    Refused { on_disk_started_at: Option<i64> },
}

/// CAS-guarded tombstone (R3c-Step-2 P5.2, R1 RF-5). Re-reads the row IMMEDIATELY
/// before the rename and REFUSES if `started_at` changed since `expected_started_at`
/// was captured — closing the reconcile-window race where a reused PID's NEW
/// incarnation row would otherwise be tombstoned by a stale recovery decision.
///
/// `expected_started_at == None` means "no incarnation asserted" → behaves like the
/// unconditional [`tombstone`] (the behavior-preservation contract for callers that
/// already verified identity). The CAS window is read..rename; `fs::rename` is
/// atomic and every writer goes through [`atomic_write`]'s tmp+rename, so the
/// re-read never sees a torn row.
pub fn tombstone_guarded(
    sessions_dir: &Path,
    pid: i64,
    expected_started_at: Option<i64>,
) -> TombstoneOutcome {
    if let (Some(expected), Some(entry)) =
        (expected_started_at, read_entry(sessions_dir, pid))
    {
        if let Some(disk) = entry.started_at {
            if disk != expected {
                return TombstoneOutcome::Refused {
                    on_disk_started_at: Some(disk),
                };
            }
        }
    }
    let json_path = sessions_dir.join(format!("{pid}.json"));
    let tomb_path = sessions_dir.join(format!("{pid}.json.tombstoned"));
    if fs::rename(&json_path, &tomb_path).is_ok() {
        TombstoneOutcome::Tombstoned
    } else {
        TombstoneOutcome::Absent
    }
}

/// As [`ensure_tombstone`], but CAS-guarded on `expected_started_at` (R3c-Step-2
/// P5.2). Recovery Rung 4 calls THIS so a reused-PID live row is never tombstoned.
/// With `expected_started_at == None` it is behavior-identical to [`ensure_tombstone`]
/// (the behavior-preservation contract). A [`TombstoneOutcome::Refused`] aborts the
/// destructive step — the caller must re-verify identity, never blind-retry.
pub fn ensure_tombstone_guarded(
    sessions_dir: &Path,
    pid: i64,
    captured: Option<&RegistryEntry>,
    expected_started_at: Option<i64>,
) -> TombstoneOutcome {
    let tomb_path = sessions_dir.join(format!("{pid}.json.tombstoned"));
    // Already tombstoned? done (idempotent — mirrors ensure_tombstone).
    if tomb_path.exists() {
        return TombstoneOutcome::Tombstoned;
    }
    match tombstone_guarded(sessions_dir, pid, expected_started_at) {
        TombstoneOutcome::Tombstoned => TombstoneOutcome::Tombstoned,
        TombstoneOutcome::Refused { on_disk_started_at } => {
            TombstoneOutcome::Refused { on_disk_started_at }
        }
        // No live row → synthesize a tombstone from captured data (best-effort,
        // exactly as ensure_tombstone does on the absent-live-file branch).
        TombstoneOutcome::Absent => {
            if let Some(captured) = captured {
                if fs::create_dir_all(sessions_dir).is_ok() {
                    if let Ok(json) = serde_json::to_string_pretty(captured) {
                        let _ = fs::write(&tomb_path, json);
                    }
                }
            }
            TombstoneOutcome::Absent
        }
    }
}

// --- Atomic name-claim primitive (HARDENING #2 foundation, spec §5) ---

/// A held name claim. Drop does NOT auto-release — call [`NameClaim::release`]
/// explicitly (A2 owns enforcement/lifecycle; A1 builds the primitive).
#[derive(Debug)]
pub struct NameClaim {
    path: PathBuf,
}

impl NameClaim {
    /// The claim file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Release the claim by deleting its file.
    pub fn release(self) -> io::Result<()> {
        fs::remove_file(&self.path)
    }
}

/// Why a [`claim_name`] failed.
#[derive(Debug)]
pub enum ClaimError {
    /// The name was already claimed. Carries the existing claim's payload,
    /// read best-effort for diagnostics (empty if the read failed).
    AlreadyClaimed { existing_payload: Vec<u8> },
    /// An I/O error other than "already exists".
    Io(io::Error),
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimError::AlreadyClaimed { .. } => write!(f, "name already claimed"),
            ClaimError::Io(e) => write!(f, "claim I/O error: {e}"),
        }
    }
}

impl std::error::Error for ClaimError {}

/// Encode a raw name into a path-safe, COLLISION-FREE claim-file stem.
///
/// ADD-8 fix (redteam-retro finding #4): the previous strip-and-collapse
/// sanitizer was LOSSY — distinct raw names could map to one stem (e.g.
/// `../../etc/passwd` and `etcpasswd`), so one racer would spuriously lose the
/// claim with `NameClaimed` (failed safe, but wrong). The claim file is OUR
/// internal hardening-#2 mechanism (TS has none), so we fix it internally with
/// zero CLI-surface change: lossless percent-encoding. Bytes in
/// `[A-Za-z0-9._-]` except `%` pass through; every other byte (incl. `/`, `\`,
/// NUL, and `%` itself) becomes `%XX`. Escaping `%` makes the map INJECTIVE —
/// distinct raw names always yield distinct stems — and escaping the
/// separators means a crafted name can never escape `claims_dir`. A stem of
/// `..` is impossible: the suffix `.claim` is always appended, and `..` only
/// traverses as a COMPLETE path component. Only the genuinely empty name is
/// rejected (`None`).
///
/// CASE-FOLDED (red-team r4 F1, lead-adjudicated: names are CASE-INSENSITIVE
/// for uniqueness, matching the resolver's name tiers): `WORKER` and `worker`
/// encode to the SAME stem, so case-variant racers serialize at the same
/// O_EXCL claim file. ASCII fold only — names are ASCII-whitelisted at create
/// (`validate_session_name`), so this is total. The map stays injective over
/// the case-folded name space (percent-escapes are uppercase hex, emitted for
/// non-alphanumerics only, so they cannot collide with folded letters).
fn encode_claim_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                out.push(b.to_ascii_lowercase() as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    Some(out)
}

/// The ACTUAL claim FILE NAME for `name` (B4 S3): `<encoded-stem>.claim`, the
/// exact basename on disk. The operator-recovery hints in the `NameClaimed`
/// errors MUST print THIS (not the raw name): the stem is case-folded +
/// percent-escaped, so a valid uppercase name like `MyAgent` lives at
/// `myagent.claim` — a hint naming `MyAgent.claim` would fail `rm` on a
/// case-sensitive fs. `None` only for the empty name (never a real session).
pub fn claim_file_name(name: &str) -> Option<String> {
    encode_claim_name(name).map(|stem| format!("{stem}.claim"))
}

/// Build the JSON claim payload (B4 S4: the WRITER lives next to
/// [`claim_name`]'s parser so the 2-shape protocol cannot drift). The claim
/// file carries `{pid, [start,] timestamp, name}`:
///   - `start` (the claimant's own `proc_start_ms`, epoch ms) is the exec-proof
///     half of pid identity — present when the probe succeeded, so a contender
///     can reap a recycled-pid claim (`claim_name`'s start arm);
///   - `start` is ABSENT (the 2nd shape) when the probe failed (a `ps` hiccup)
///     OR for a claim written by a pre-B4 binary — readers fall back to
///     is-alive-only (backward-compatible, pinned).
///
/// Hand-built (no serde struct for four scalar fields) — stable key order for a
/// readable holder string. Key order matches the parser's field reads.
pub fn claim_payload(pid: u32, start: Option<i64>, ts: i64, name: &str) -> String {
    match start {
        Some(start) => {
            format!(r#"{{"pid":{pid},"start":{start},"timestamp":{ts},"name":{name:?}}}"#)
        }
        None => format!(r#"{{"pid":{pid},"timestamp":{ts},"name":{name:?}}}"#),
    }
}

/// WS-R R3a-Step-2 — build the claim payload WITH an explicit `incarnation`
/// fence (R1 §3; resolves Open-Q #10). The `incarnation` is a monotonic counter
/// per session-NAME (never decremented, R1 §3 inv 2): a writer holding
/// incarnation N must not stomp a row owned by N+1.
///
/// BACKWARD COMPAT (the load-bearing property): incarnation `0` emits the EXACT
/// legacy [`claim_payload`] bytes (no `"incarnation"` field). Only a non-zero
/// incarnation appends `,"incarnation":N`. So:
///   - an OLD claim file (no field) reads back as incarnation 0 via
///     [`claim_incarnation`] (`serde(default)` semantics);
///   - a fresh claim at incarnation 0 is byte-identical to the legacy writer
///     (existing pin test + on-disk files unchanged);
///   - the field is purely additive at the end → the existing parser (which
///     reads `pid`/`start`/`timestamp`/`name` by key) is unaffected.
pub fn claim_payload_with_incarnation(
    pid: u32,
    start: Option<i64>,
    ts: i64,
    name: &str,
    incarnation: u64,
) -> String {
    let base = claim_payload(pid, start, ts, name);
    if incarnation == 0 {
        // Legacy-identical: no field for the default incarnation.
        return base;
    }
    // Additive: insert before the closing brace. `base` always ends in `}`.
    let trimmed = base.strip_suffix('}').expect("claim_payload ends with }");
    format!(r#"{trimmed},"incarnation":{incarnation}}}"#)
}

/// WS-R R3a-Step-2 — read the `incarnation` fence from a claim payload, with
/// `serde(default)` semantics: an ABSENT field (a legacy/pre-R3a claim file, or
/// an unparseable blob) reads as incarnation `0`. This is the backward-compat
/// reader the dead-holder reap + the recovery (re-)claim consult.
pub fn claim_incarnation(payload: &[u8]) -> u64 {
    serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| v.get("incarnation").and_then(serde_json::Value::as_u64))
        .unwrap_or(0)
}

/// WS-R R3a-Step-2 — the monotonic NEXT incarnation for a (re-)claim of `name`.
/// Reads the prior claim file's incarnation (absent/corrupt = 0, via
/// [`claim_incarnation`]) and returns `prev + 1`. This is the increment site
/// (R1 §3 inv 2): every successful (re-)claim — including a dead-holder reap and
/// a recovery Rung-4 respawn — bumps the fence so a stale writer at N cannot
/// stomp the new incarnation at N+1. Never decremented. A missing claim file is
/// a first claim → incarnation 1.
pub fn next_claim_incarnation(claims_dir: &Path, name: &str) -> u64 {
    let Some(file) = claim_file_name(name) else {
        return 1;
    };
    let path = claims_dir.join(file);
    let prev = match fs::read(&path) {
        Ok(bytes) => claim_incarnation(&bytes),
        Err(_) => 0, // no prior claim (or unreadable) → first incarnation.
    };
    prev + 1
}

/// Atomically claim `name` by creating `<claims_dir>/<encoded>.claim` with
/// `O_EXCL` (`create_new(true)`) and writing `payload` into it.
///
/// SINGLE atomic op for the claim: there is NO read-then-check-then-write
/// window anywhere. WHY: a read-then-write race lets two concurrent `qd new`
/// both observe "name free" and both claim ONE name — the `create_new` /
/// `O_EXCL` open is the single point where exactly one racer wins. (A2 wires
/// enforcement into the create path; A1 builds + concurrency-tests the
/// primitive.) `claims_dir` is created recursively first — mkdir is not the
/// atomic point; the `O_EXCL` open of the claim file is.
///
/// On `EEXIST`, the existing claim's payload is read and its `"pid"` field
/// checked against the injected `is_alive` predicate (the `effects::is_pid_alive`
/// seam in production — injected so units don't need real processes). A claim
/// whose holder pid is NOT alive is STALE (P0 redfix F2: `ClaimGuard` cannot run
/// on SIGKILL, so a kill mid-boot would otherwise brick the name forever): the
/// stale file is removed and the `O_EXCL` create retried EXACTLY ONCE — a second
/// `EEXIST` means a live racer recreated it and we lose as today. An unparseable
/// payload or an alive holder → [`ClaimError::AlreadyClaimed`] with the existing
/// payload, unchanged. An unsanitizable name yields an `InvalidInput` I/O error.
///
/// EXEC-PROOF HOLDER IDENTITY (qb punch B4 item 10, the P0 (pid,start-time)
/// kill-path lesson): `is_alive(pid)` alone cannot tell the CLAIMANT from a
/// stranger that recycled its pid — a dead claimant whose pid was reused made
/// the claim look live forever. So a claim payload that carries the claimant's
/// `"start"` (its own start time, epoch ms) is additionally verified through
/// the injected `proc_start` probe (the `effects::proc_start_ms` seam): a LIVE
/// pid whose current occupant started LATER than the claimed start (beyond
/// [`crate::kill::START_TIME_SLACK_MS`], the kill-path slack — second-resolution
/// `etime` probes need generous slack) is a recycled pid, hence STALE, and is
/// reaped like a dead holder. BACKWARD-COMPAT (pinned): an OLD claim with no
/// `"start"` field still parses and falls back to the is-alive-only check (no
/// regression for a claim written by a pre-B4 binary mid-cutover) — the probe
/// is NOT consulted. An unverifiable probe (`proc_start` = `None` on a live
/// pid: a `ps` hiccup) honors the claim — "cannot verify" is not "stale".
///
/// OPERATOR RECOVERY (documented): if a name is wedged by a claim that this
/// reap logic refuses to clear (e.g. an alive-pid holder that is not really a
/// create — pid recycled within the slack window), the recovery is to delete
/// the claim file by hand: `<qd-home>/.claude/claims/<name>.claim` (the path a
/// loser's `NameClaimed` error prints). The claim file only closes the create
/// window; deleting it never corrupts a booted session (the durable record is
/// the registry row).
pub fn claim_name(
    claims_dir: &Path,
    name: &str,
    payload: &[u8],
    is_alive: &dyn Fn(i64) -> bool,
    proc_start: &dyn Fn(i64) -> Option<i64>,
) -> Result<NameClaim, ClaimError> {
    let safe = encode_claim_name(name).ok_or_else(|| {
        ClaimError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "claim_name: name is empty",
        ))
    })?;
    // mkdir then O_EXCL is fine — the claim file itself is the atomic point.
    fs::create_dir_all(claims_dir).map_err(ClaimError::Io)?;
    let path = claims_dir.join(format!("{safe}.claim"));
    match try_create_claim(&path, payload) {
        Err(ClaimError::AlreadyClaimed { existing_payload }) => {
            // Dead-holder reap (F2) + recycled-pid reap (B4 item 10): only a
            // payload that PARSES and names a pid is reapable. Anything else →
            // refused, as before.
            let stale = claim_is_stale(&existing_payload, is_alive, proc_start);
            if stale {
                // Best-effort remove (NotFound = someone else reaped first —
                // fine, the retry create is the arbiter). Then ONE retry:
                // a second EEXIST = a live racer won in the window.
                let _ = fs::remove_file(&path);
                try_create_claim(&path, payload)
            } else {
                Err(ClaimError::AlreadyClaimed { existing_payload })
            }
        }
        other => other,
    }
}

/// Whether an existing claim's holder is DEAD or its pid was RECYCLED — the
/// reapable conditions, shared by [`claim_name`] and [`claim_name_with_incarnation`]
/// so both paths apply IDENTICAL reap semantics. Dead holder (F2): the pid is not
/// alive. Recycled pid (B4 item 10): the pid is alive but its CURRENT occupant
/// started after the claimed `"start"` (beyond [`crate::kill::START_TIME_SLACK_MS`]).
/// An unparseable payload, no pid, an old `"start"`-less format, or an unverifiable
/// probe (`None` on a live pid) → NOT stale (honor the claim — "cannot verify" is
/// not "stale").
fn claim_is_stale(
    existing_payload: &[u8],
    is_alive: &dyn Fn(i64) -> bool,
    proc_start: &dyn Fn(i64) -> Option<i64>,
) -> bool {
    let parsed = serde_json::from_slice::<serde_json::Value>(existing_payload).ok();
    let holder_pid = parsed
        .as_ref()
        .and_then(|v| v.get("pid").and_then(serde_json::Value::as_i64));
    match holder_pid {
        // Dead holder (F2). The probe is never consulted.
        Some(pid) if !is_alive(pid) => true,
        // Alive pid: exec-proof identity (item 10) — recycled iff the current
        // occupant started after the claimed start (beyond the kill-path slack).
        Some(pid) => match parsed
            .as_ref()
            .and_then(|v| v.get("start").and_then(serde_json::Value::as_i64))
        {
            Some(claimed_start) => matches!(
                proc_start(pid),
                Some(cur) if cur > claimed_start + crate::kill::START_TIME_SLACK_MS
            ),
            None => false,
        },
        None => false,
    }
}

/// WS-R R3c item-1 — atomically claim `name` AND stamp a monotonic `incarnation`
/// fence read INSIDE the `O_EXCL` critical section (TOCTOU-safe; R1 §3, clause 7).
///
/// The incarnation is **never** pre-read before the claim (which would let a
/// concurrent (re-)claim bump it between the read and the create — the TOCTOU the
/// fence exists to prevent). Instead it is derived AT the atomic point:
/// - **fresh win** (the `O_EXCL` `create_new` succeeds with no prior file): there
///   was no prior holder at the atomic instant → incarnation `1` (the monotonic
///   base; [`next_claim_incarnation`] on the just-won empty file is `1`).
/// - **reap-and-reclaim** (`EEXIST` → the holder is dead/recycled per
///   [`claim_is_stale`]): the prior incarnation is read HERE, inside the section,
///   from the file we are about to reap, via [`next_claim_incarnation`] (prev+1),
///   BEFORE the reap rename. So a stale writer at incarnation N cannot stomp the
///   new claim at N+1 (the name-level fence; the row-level fence is `started_at`).
///
/// `make_payload(incarnation)` builds the claim bytes for the assigned incarnation
/// (callers use [`claim_payload_with_incarnation`]). Returns the claim guard AND
/// the assigned incarnation (the respawn path records it as the successor's fence).
/// Reap/recycle semantics are IDENTICAL to [`claim_name`]; only the payload is
/// incarnation-stamped and computed inside the section.
pub fn claim_name_with_incarnation(
    claims_dir: &Path,
    name: &str,
    is_alive: &dyn Fn(i64) -> bool,
    proc_start: &dyn Fn(i64) -> Option<i64>,
    make_payload: &dyn Fn(u64) -> Vec<u8>,
) -> Result<(NameClaim, u64), ClaimError> {
    let safe = encode_claim_name(name).ok_or_else(|| {
        ClaimError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "claim_name_with_incarnation: name is empty",
        ))
    })?;
    fs::create_dir_all(claims_dir).map_err(ClaimError::Io)?;
    let path = claims_dir.join(format!("{safe}.claim"));

    // First attempt: a FRESH O_EXCL create. Winning it means no prior holder
    // existed at the atomic instant → incarnation 1. The incarnation is read INSIDE
    // the critical section (we hold the just-won exclusive file), never before it.
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut f) => {
            // next_claim_incarnation on the empty just-won file is 1 (the base).
            let inc = next_claim_incarnation(claims_dir, name).max(1);
            f.write_all(&make_payload(inc)).map_err(ClaimError::Io)?;
            return Ok((NameClaim { path }, inc));
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => { /* reap below */ }
        Err(e) => return Err(ClaimError::Io(e)),
    }

    // EEXIST: a prior claim is present. Reap iff the holder is dead/recycled (SAME
    // logic as claim_name). The prior incarnation is read here, INSIDE the section,
    // from the still-present file we are about to reap.
    let existing_payload = read_best_effort(&path);
    if !claim_is_stale(&existing_payload, is_alive, proc_start) {
        return Err(ClaimError::AlreadyClaimed { existing_payload });
    }
    // Bump the fence off the reaped holder's incarnation (prev+1), read INSIDE the
    // section BEFORE the reap rename — the TOCTOU-safe ordering (clause 7).
    let inc = next_claim_incarnation(claims_dir, name);
    let _ = fs::remove_file(&path); // best-effort; the retry create is the arbiter
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut f) => {
            f.write_all(&make_payload(inc)).map_err(ClaimError::Io)?;
            Ok((NameClaim { path }, inc))
        }
        // A live racer recreated it in the reap window — we lose, as today.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(ClaimError::AlreadyClaimed {
            existing_payload: read_best_effort(&path),
        }),
        Err(e) => Err(ClaimError::Io(e)),
    }
}

/// One `O_EXCL` create attempt of the claim file (the single atomic point).
/// `EEXIST` → [`ClaimError::AlreadyClaimed`] carrying the existing payload,
/// read best-effort for diagnostics.
fn try_create_claim(path: &Path, payload: &[u8]) -> Result<NameClaim, ClaimError> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            f.write_all(payload).map_err(ClaimError::Io)?;
            Ok(NameClaim {
                path: path.to_path_buf(),
            })
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let existing_payload = read_best_effort(path);
            Err(ClaimError::AlreadyClaimed { existing_payload })
        }
        Err(e) => Err(ClaimError::Io(e)),
    }
}

/// Read a file's bytes best-effort; empty vec on any error (diagnostics-only).
fn read_best_effort(path: &Path) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Ok(mut f) = File::open(path) {
        let _ = f.read_to_end(&mut buf);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use tempfile::tempdir;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test/golden/fixtures/layer2/dirty-state")
    }

    fn full_entry() -> RegistryEntry {
        RegistryEntry {
            pid: Some(4242),
            session_id: Some("sess-abc".into()),
            cwd: Some("/work/proj".into()),
            started_at: Some(1_700_000_000_000),
            updated_at: Some(1_700_000_100_000),
            status: Some("busy".into()),
            name: Some("work".into()),
            version: Some("1.2.3".into()),
            kind: Some("claude".into()),
            entrypoint: Some("cli".into()),
            backend: Some("zmx".into()),
            spawned_by: Some("orchestrator-1".into()),
            // codex P1, R1: a populated provider exercises the write+read paths
            // (round_trip_full_entry + the camelCase/omit-None pins below).
            provider: Some("claude-code".into()),
            // codex P2 W4: claude rows never carry endpoint — `full_entry` models a
            // claude-shaped row, so endpoint is None here (byte-stability of the
            // existing camelCase/omit-None pins). The codex-shaped tests below
            // populate it directly.
            endpoint: None,
            transport: None,
        }
    }

    /// A codex-shaped registry entry — the daemon-create row W4 writes (codex-p2
    /// -spec section 7.2): provider "codex" + a recorded ws endpoint, sessionId =
    /// the thread uuid, pid = the daemon pid. Used by the endpoint round-trip +
    /// the codex-tombstone test.
    fn codex_entry() -> RegistryEntry {
        RegistryEntry {
            pid: Some(90909),
            session_id: Some("019e9f4b-adb9-7ec1-b4ed-08247847426a".into()),
            cwd: Some("/work/proj".into()),
            started_at: Some(1_700_000_000_000),
            updated_at: Some(1_700_000_000_000),
            status: Some("idle".into()),
            name: Some("cdx".into()),
            version: None,
            kind: None,
            entrypoint: None,
            backend: None,
            spawned_by: None,
            provider: Some("codex".into()),
            endpoint: Some("ws://127.0.0.1:18951".into()),
            transport: None,
        }
    }

    // --- Round-trip ---

    #[test]
    fn round_trip_full_entry() {
        let dir = tempdir().unwrap();
        let entry = full_entry();
        write_entry(dir.path(), &entry).unwrap();
        let back = read_entry(dir.path(), 4242).unwrap();
        assert_eq!(back, entry);
    }

    // --- R3c-Step-2 P5.2: CAS-guarded tombstone -----------------------------

    /// Behavior preservation: with `expected_started_at == None`,
    /// `tombstone_guarded` renames a live row EXACTLY like the unconditional
    /// `tombstone` — existing identity-verified callers are unaffected.
    #[test]
    fn tombstone_guarded_none_expected_matches_unconditional() {
        let dir = tempdir().unwrap();
        write_entry(dir.path(), &full_entry()).unwrap();
        let out = tombstone_guarded(dir.path(), 4242, None);
        assert_eq!(out, TombstoneOutcome::Tombstoned);
        assert!(!dir.path().join("4242.json").exists(), "live row renamed away");
        assert!(dir.path().join("4242.json.tombstoned").exists(), "tombstone present");
    }

    /// CAS ADMITS a matching incarnation: the captured `started_at` still owns the
    /// row → tombstone proceeds.
    #[test]
    fn tombstone_guarded_admits_matching_incarnation() {
        let dir = tempdir().unwrap();
        let entry = full_entry(); // started_at = 1_700_000_000_000
        write_entry(dir.path(), &entry).unwrap();
        let out = tombstone_guarded(dir.path(), 4242, Some(1_700_000_000_000));
        assert_eq!(out, TombstoneOutcome::Tombstoned);
        assert!(dir.path().join("4242.json.tombstoned").exists());
    }

    /// CAS REFUSES a reused PID: a NEW incarnation (different `started_at`) owns the
    /// row now, so a stale recovery decision (captured the OLD `started_at`) must NOT
    /// tombstone it — the live successor is left UNTOUCHED. This is the P5.2 guard.
    #[test]
    fn tombstone_guarded_refuses_reused_pid_live_row() {
        let dir = tempdir().unwrap();
        let mut entry = full_entry();
        entry.started_at = Some(2_000_000_000_000); // the NEW incarnation on disk
        write_entry(dir.path(), &entry).unwrap();

        // Recovery captured the OLD incarnation (1_700_000_000_000) before a reconcile
        // window reused the PID. The guard must REFUSE.
        let out = tombstone_guarded(dir.path(), 4242, Some(1_700_000_000_000));
        assert_eq!(
            out,
            TombstoneOutcome::Refused {
                on_disk_started_at: Some(2_000_000_000_000)
            }
        );
        assert!(
            dir.path().join("4242.json").exists(),
            "the reused-PID live row must be LEFT INTACT (not tombstoned)"
        );
        assert!(
            !dir.path().join("4242.json.tombstoned").exists(),
            "no tombstone written for a refused live row"
        );
    }

    /// Absent live row → `Absent` (nothing to rename), and `ensure_tombstone_guarded`
    /// still synthesizes from captured — behavior-identical to `ensure_tombstone`.
    #[test]
    fn ensure_tombstone_guarded_none_expected_synthesizes_like_unguarded() {
        let dir = tempdir().unwrap();
        // No live <pid>.json → synthesize from captured (the ensure_tombstone branch).
        let captured = full_entry();
        let out = ensure_tombstone_guarded(dir.path(), 4242, Some(&captured), None);
        assert_eq!(out, TombstoneOutcome::Absent);
        let tomb = dir.path().join("4242.json.tombstoned");
        assert!(tomb.exists(), "synthesized tombstone present");
        // Round-trips the captured row (same as ensure_tombstone's synthesize path).
        let back: RegistryEntry =
            serde_json::from_slice(&std::fs::read(&tomb).unwrap()).unwrap();
        assert_eq!(back, captured);
    }

    // --- A6 §4.4 / red-team F5: backend + spawnedBy round-trip on the paths that
    // CAN fail. write_entry is covered by round_trip_full_entry above; here we
    // pin ensure_tombstone's SYNTHESIZE-FROM-CAPTURED branch (registry.rs:442+),
    // where a fresh serialize happens. The rename path (F5) is NOT targeted: a
    // rename never re-serializes, so a test there cannot fail.
    #[test]
    fn ensure_tombstone_synthesize_round_trips_backend_and_spawned_by() {
        let dir = tempdir().unwrap();
        // No live <pid>.json and no existing tombstone → ensure_tombstone takes
        // the synthesize-from-captured branch and serializes `captured`.
        let captured = full_entry(); // carries backend + spawned_by
        let pid = captured.pid.unwrap();
        ensure_tombstone(dir.path(), pid, Some(&captured));

        let tomb = dir.path().join(format!("{pid}.json.tombstoned"));
        assert!(tomb.exists(), "synthesize branch must write a tombstone");
        let text = fs::read_to_string(&tomb).unwrap();
        // The new lineage fields survived the synthesize serialize, camelCase.
        assert!(text.contains("\"backend\""), "json: {text}");
        assert!(text.contains("\"spawnedBy\""), "json: {text}");

        // Round-trips back to the same entry via the permissive read.
        let (back, degraded) =
            RegistryEntry::from_value(serde_json::from_str(&text).unwrap()).unwrap();
        assert_eq!(back.backend.as_deref(), Some("zmx"));
        assert_eq!(back.spawned_by.as_deref(), Some("orchestrator-1"));
        assert!(degraded.is_empty(), "clean row, nothing degraded");
    }

    #[test]
    fn written_json_is_camelcase_and_omits_none() {
        let dir = tempdir().unwrap();
        let mut entry = full_entry();
        // Drop a couple of fields to prove None is omitted from the file.
        entry.version = None;
        entry.kind = None;
        write_entry(dir.path(), &entry).unwrap();
        let text = fs::read_to_string(dir.path().join("4242.json")).unwrap();

        // camelCase keys present literally.
        assert!(text.contains("\"sessionId\""), "json: {text}");
        assert!(text.contains("\"startedAt\""), "json: {text}");
        assert!(text.contains("\"updatedAt\""), "json: {text}");
        assert!(text.contains("\"spawnedBy\""), "json: {text}");
        assert!(text.contains("\"backend\""), "json: {text}");
        // No snake_case leaked.
        assert!(!text.contains("session_id"), "json: {text}");
        assert!(!text.contains("spawned_by"), "json: {text}");
        // None fields omitted.
        assert!(!text.contains("\"version\""), "json: {text}");
        assert!(!text.contains("\"kind\""), "json: {text}");
        // 2-space indent, no trailing newline (TS JSON.stringify(.., null, 2)).
        assert!(text.contains("\n  \"pid\""), "json: {text}");
        assert!(!text.ends_with('\n'), "must have NO trailing newline");
    }

    // --- Permissive read ---

    #[test]
    fn legacy_entry_parses_with_none_for_new_fields() {
        // No backend / spawnedBy; an extra unknown field present.
        let blob = r#"{"pid":7,"sessionId":"s","legacyField":"ignored"}"#;
        let entry: RegistryEntry = serde_json::from_str(blob).unwrap();
        assert_eq!(entry.pid, Some(7));
        assert_eq!(entry.session_id, Some("s".into()));
        assert_eq!(entry.backend, None);
        assert_eq!(entry.spawned_by, None);
    }

    #[test]
    fn read_entries_skips_corrupt_keeps_good() {
        let dir = tempdir().unwrap();
        // Good live entry.
        write_entry(dir.path(), &full_entry()).unwrap();
        // Corrupt file — must be skipped, scan must not fail.
        fs::write(dir.path().join("999.json"), b"{not json").unwrap();
        // A non-.json file — ignored.
        fs::write(dir.path().join("notes.txt"), b"hi").unwrap();

        let scanned = read_entries(dir.path(), false);
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].entry.pid, Some(4242));
        assert!(!scanned[0].tombstoned);
    }

    #[test]
    fn read_entry_returns_none_for_corrupt_and_missing() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("5.json"), b"{not json").unwrap();
        assert_eq!(read_entry(dir.path(), 5), None); // corrupt
        assert_eq!(read_entry(dir.path(), 6), None); // missing
    }

    #[test]
    fn read_entries_missing_dir_is_empty() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(read_entries(&missing, true).is_empty());
    }

    #[test]
    fn read_entries_tombstone_flag_and_inclusion() {
        let dir = tempdir().unwrap();
        write_entry(dir.path(), &full_entry()).unwrap();
        assert!(tombstone(dir.path(), 4242));

        // Excluded by default.
        let live_only = read_entries(dir.path(), false);
        assert!(live_only.is_empty());

        // Included when asked, flagged tombstoned.
        let with_tomb = read_entries(dir.path(), true);
        assert_eq!(with_tomb.len(), 1);
        assert!(with_tomb[0].tombstoned);
        assert_eq!(with_tomb[0].entry.pid, Some(4242));
    }

    #[test]
    fn dirty_state_corpus_parses_or_fails_cleanly() {
        let dir = fixtures_dir();

        // clean / legacy / missing-field all parse permissively.
        for name in ["clean", "legacy", "missing-field"] {
            let bytes = fs::read(dir.join(format!("{name}.json")))
                .unwrap_or_else(|_| panic!("fixture {name}.json must exist at {dir:?}"));
            let parsed: Result<RegistryEntry, _> = serde_json::from_slice(&bytes);
            assert!(parsed.is_ok(), "{name}.json should parse, got {parsed:?}");
        }

        // corrupt is a clean failure — Err, no panic.
        let corrupt = fs::read(dir.join("corrupt.json")).unwrap();
        let parsed: Result<RegistryEntry, _> = serde_json::from_slice(&corrupt);
        assert!(parsed.is_err(), "corrupt.json must fail cleanly");
    }

    // --- Per-field-permissive read (A4 pass-(b) F3) ---

    /// Build a wrong-typed-field JSON blob by overriding one camelCase key on an
    /// otherwise well-typed object, then parse it via the read path and return the
    /// surviving entry plus the degraded list.
    fn read_blob(json: &str) -> (RegistryEntry, Vec<&'static str>) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("100.json");
        fs::write(&path, json).unwrap();
        parse_file(&path).expect("row must SURVIVE per-field-permissive read")
    }

    #[test]
    fn wrong_typed_started_at_string_degrades_to_none_row_survives() {
        // string where i64 startedAt is declared.
        let (e, degraded) =
            read_blob(r#"{"pid":100,"sessionId":"s","startedAt":"12345","status":"busy"}"#);
        assert_eq!(e.started_at, None, "wrong-typed startedAt degrades to None");
        assert_eq!(degraded, vec!["startedAt"]);
        // Well-typed siblings preserved.
        assert_eq!(e.pid, Some(100));
        assert_eq!(e.session_id, Some("s".into()));
        assert_eq!(e.status, Some("busy".into()));
    }

    #[test]
    fn wrong_typed_updated_at_string_degrades() {
        let (e, degraded) = read_blob(r#"{"pid":100,"updatedAt":"99"}"#);
        assert_eq!(e.updated_at, None);
        assert_eq!(degraded, vec!["updatedAt"]);
        assert_eq!(e.pid, Some(100));
    }

    #[test]
    fn wrong_typed_pid_string_degrades() {
        // string "123" where i64 pid is declared — NO string→number coercion.
        let (e, degraded) = read_blob(r#"{"pid":"123","sessionId":"s"}"#);
        assert_eq!(
            e.pid, None,
            "string pid degrades to None, not coerced to 123"
        );
        assert_eq!(degraded, vec!["pid"]);
        assert_eq!(e.session_id, Some("s".into()));
    }

    #[test]
    fn wrong_typed_status_number_degrades() {
        let (e, degraded) = read_blob(r#"{"pid":100,"status":7}"#);
        assert_eq!(e.status, None);
        assert_eq!(degraded, vec!["status"]);
        assert_eq!(e.pid, Some(100));
    }

    #[test]
    fn wrong_typed_name_number_degrades() {
        let (e, degraded) = read_blob(r#"{"pid":100,"name":42}"#);
        assert_eq!(e.name, None);
        assert_eq!(degraded, vec!["name"]);
    }

    #[test]
    fn wrong_typed_spawned_by_number_degrades() {
        let (e, degraded) = read_blob(r#"{"pid":100,"spawnedBy":99}"#);
        assert_eq!(e.spawned_by, None);
        assert_eq!(degraded, vec!["spawnedBy"]);
    }

    // codex P1, R1 (codex-p1-spec section 3.1) — MUTATION EVIDENCE. A wrong-typed
    // `"provider": 7` DEGRADES: the row SURVIVES (still parses) and "provider"
    // appears in the degraded list. Dropping the `field!(entry.provider, ...)` row
    // in `from_value` reds this test — the field would be silently lost (and a
    // whole-struct parse would drop the whole row instead of degrading it).
    #[test]
    fn wrong_typed_provider_number_degrades() {
        let (e, degraded) = read_blob(r#"{"pid":100,"provider":7}"#);
        assert_eq!(e.provider, None, "wrong-typed provider degrades to None");
        assert_eq!(e.pid, Some(100), "the row SURVIVES (sibling preserved)");
        assert_eq!(degraded, vec!["provider"]);
    }

    // codex P1, R1 (codex-p1-spec section 3.1) — the on-disk key is the single
    // lowercase word `provider` (NOT camelCased by `rename_all`). Pinned, not
    // assumed: a populated provider round-trips through write+read and the file
    // carries the literal `"provider"` key.
    #[test]
    fn provider_on_disk_key_is_lowercase_word_and_round_trips() {
        let dir = tempdir().unwrap();
        let entry = full_entry(); // provider = Some("claude-code")
        write_entry(dir.path(), &entry).unwrap();
        let text = fs::read_to_string(dir.path().join("4242.json")).unwrap();
        assert!(text.contains("\"provider\""), "json: {text}");
        let back = read_entry(dir.path(), 4242).unwrap();
        assert_eq!(back.provider.as_deref(), Some("claude-code"));
    }

    // === codex P2 W4 (codex-p2-spec section 7.2) — RegistryEntry.endpoint ===

    // MUTATION EVIDENCE (§13 "endpoint field! row dropped"): a wrong-typed
    // `"endpoint": 7` DEGRADES — the row SURVIVES (still parses) and "endpoint"
    // appears in the degraded list. Dropping the `field!(entry.endpoint, ...)` row
    // in `from_value` reds this test (the endpoint would silently vanish; a
    // whole-struct parse would drop the whole row instead of degrading it).
    #[test]
    fn wrong_typed_endpoint_number_degrades() {
        let (e, degraded) = read_blob(r#"{"pid":100,"endpoint":7}"#);
        assert_eq!(e.endpoint, None, "wrong-typed endpoint degrades to None");
        assert_eq!(e.pid, Some(100), "the row SURVIVES (sibling preserved)");
        assert_eq!(degraded, vec!["endpoint"]);
    }

    // The on-disk key is the single lowercase word `endpoint` (NOT camelCased by
    // `rename_all`); a populated endpoint round-trips through write+read and the
    // file carries the literal `"endpoint"` key (pinned, not assumed).
    #[test]
    fn endpoint_on_disk_key_is_lowercase_word_and_round_trips() {
        let dir = tempdir().unwrap();
        let entry = codex_entry(); // endpoint = Some("ws://127.0.0.1:18951")
        write_entry(dir.path(), &entry).unwrap();
        let text = fs::read_to_string(dir.path().join("90909.json")).unwrap();
        assert!(text.contains("\"endpoint\""), "json: {text}");
        // The value round-trips through the permissive read.
        let back = read_entry(dir.path(), 90909).unwrap();
        assert_eq!(back, entry);
        assert_eq!(back.endpoint.as_deref(), Some("ws://127.0.0.1:18951"));
        assert_eq!(back.provider.as_deref(), Some("codex"));
    }

    // MUTATION EVIDENCE (§13 "claude-row byte-stability broken by endpoint
    // field"): a claude row (endpoint None) serializes with NO `endpoint` key —
    // existing rows / tombstones / goldens stay byte-stable. `full_entry` carries
    // endpoint None, so the written JSON must not mention `endpoint`, and the row
    // round-trips absent-stays-absent.
    #[test]
    fn absent_endpoint_stays_absent_and_byte_stable() {
        let dir = tempdir().unwrap();
        let entry = full_entry(); // a claude-shaped row: endpoint None
        assert_eq!(entry.endpoint, None);
        write_entry(dir.path(), &entry).unwrap();
        let text = fs::read_to_string(dir.path().join("4242.json")).unwrap();
        assert!(
            !text.contains("endpoint"),
            "a None endpoint must NOT appear on disk (byte-stability): {text}"
        );
        // Absent stays absent across the round-trip.
        let back = read_entry(dir.path(), 4242).unwrap();
        assert_eq!(back.endpoint, None);
        assert_eq!(back, entry);
    }

    // === scoped-ACP-CC: RegistryEntry.transport byte-stability (the same R1 /
    // provider / endpoint discipline) ===

    // A wrong-typed `"transport": 1` DEGRADES (row survives, "transport" named)
    // instead of dropping the whole row. Mutation evidence: removing the
    // `field!(entry.transport, ...)` line in from_value reds this.
    #[test]
    fn wrong_typed_transport_number_degrades() {
        let (e, degraded) = read_blob(r#"{"pid":100,"transport":1}"#);
        assert_eq!(e.transport, None, "wrong-typed transport degrades to None");
        assert_eq!(e.pid, Some(100), "the row SURVIVES (sibling preserved)");
        assert_eq!(degraded, vec!["transport"]);
    }

    // The on-disk key is the single lowercase word `transport` (NOT camelCased by
    // `rename_all`); a populated transport round-trips and the file carries the
    // literal `"transport"` key (pinned, not assumed).
    #[test]
    fn transport_on_disk_key_is_lowercase_word_and_round_trips() {
        let dir = tempdir().unwrap();
        let mut entry = codex_entry();
        entry.transport = Some("pty".into()); // a degraded-to-floor row
        write_entry(dir.path(), &entry).unwrap();
        let text = fs::read_to_string(dir.path().join("90909.json")).unwrap();
        assert!(text.contains("\"transport\""), "json: {text}");
        let back = read_entry(dir.path(), 90909).unwrap();
        assert_eq!(back, entry);
        assert_eq!(back.transport.as_deref(), Some("pty"));
    }

    // MUTATION EVIDENCE (byte-stability): a healthy row (transport None) serializes
    // with NO `transport` key — existing rows / tombstones / goldens stay
    // byte-stable. `full_entry` carries transport None; the JSON must not mention
    // `transport`, and the row round-trips absent-stays-absent. Removing the
    // `skip_serializing_if` on the field reds this.
    #[test]
    fn absent_transport_stays_absent_and_byte_stable() {
        let dir = tempdir().unwrap();
        let entry = full_entry(); // a healthy claude row: transport None
        assert_eq!(entry.transport, None);
        write_entry(dir.path(), &entry).unwrap();
        let text = fs::read_to_string(dir.path().join("4242.json")).unwrap();
        assert!(
            !text.contains("transport"),
            "a None transport must NOT appear on disk (byte-stability): {text}"
        );
        let back = read_entry(dir.path(), 4242).unwrap();
        assert_eq!(back.transport, None);
        assert_eq!(back, entry);
    }

    // A captured CODEX-SHAPED entry tombstones WITH its endpoint (ensure_tombstone
    // synthesize-from-captured branch re-serializes `captured`, carrying provider
    // + endpoint automatically via the struct). The kill path (W7) captures the
    // row and tombstones it; this proves the endpoint survives that synthesis.
    #[test]
    fn ensure_tombstone_synthesize_carries_codex_endpoint() {
        let dir = tempdir().unwrap();
        let captured = codex_entry(); // provider "codex" + endpoint
        let pid = captured.pid.unwrap();
        // No live <pid>.json and no existing tombstone → synthesize branch.
        ensure_tombstone(dir.path(), pid, Some(&captured));
        let tomb = dir.path().join(format!("{pid}.json.tombstoned"));
        assert!(tomb.exists(), "synthesize branch must write a tombstone");
        let text = fs::read_to_string(&tomb).unwrap();
        assert!(text.contains("\"endpoint\""), "tombstone json: {text}");
        assert!(text.contains("\"provider\""), "tombstone json: {text}");
        let (back, degraded) =
            RegistryEntry::from_value(serde_json::from_str(&text).unwrap()).unwrap();
        assert_eq!(back.endpoint.as_deref(), Some("ws://127.0.0.1:18951"));
        assert_eq!(back.provider.as_deref(), Some("codex"));
        assert!(degraded.is_empty(), "clean codex row, nothing degraded");
    }

    #[test]
    fn multiple_wrong_typed_fields_all_named_set_wise() {
        let (e, degraded) = read_blob(r#"{"pid":"x","sessionId":"s","startedAt":"t","status":3}"#);
        assert_eq!(e.pid, None);
        assert_eq!(e.started_at, None);
        assert_eq!(e.status, None);
        assert_eq!(
            e.session_id,
            Some("s".into()),
            "well-typed sibling preserved"
        );
        let got: std::collections::BTreeSet<_> = degraded.into_iter().collect();
        let want: std::collections::BTreeSet<_> =
            ["pid", "startedAt", "status"].into_iter().collect();
        assert_eq!(got, want);
    }

    #[test]
    fn valid_json_wrong_everything_survives_all_defaults() {
        // Every field wrong-typed, but valid JSON object → row survives, all None.
        let (e, degraded) = read_blob(
            r#"{"pid":"a","sessionId":1,"cwd":2,"startedAt":"b","updatedAt":"c",
                "status":3,"name":4,"version":5,"kind":6,"entrypoint":7,
                "backend":8,"spawnedBy":9}"#,
        );
        assert_eq!(
            e,
            RegistryEntry::default(),
            "all fields degraded to default"
        );
        assert_eq!(degraded.len(), 12, "every declared field is named degraded");
    }

    #[test]
    fn clean_row_has_empty_degraded_list() {
        let (e, degraded) =
            read_blob(r#"{"pid":100,"sessionId":"s","startedAt":1,"status":"busy"}"#);
        assert!(degraded.is_empty(), "well-typed row has no degraded fields");
        assert_eq!(e.pid, Some(100));
        assert_eq!(e.started_at, Some(1));
    }

    #[test]
    fn missing_field_is_not_degraded() {
        // Absent (not wrong-typed) fields stay default but are NOT in degraded.
        let (e, degraded) = read_blob(r#"{"pid":100}"#);
        assert!(degraded.is_empty(), "MISSING is not WRONG-TYPED");
        assert_eq!(e.started_at, None);
        assert_eq!(e.pid, Some(100));
    }

    #[test]
    fn non_json_blob_still_cleanly_skipped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("100.json");
        fs::write(&path, b"{not json at all").unwrap();
        assert!(parse_file(&path).is_none(), "invalid JSON is a clean skip");
    }

    #[test]
    fn valid_non_object_json_cleanly_skipped() {
        // Valid JSON but a top-level array / number is not a registry row.
        let dir = tempdir().unwrap();
        let path = dir.path().join("100.json");
        fs::write(&path, b"[1,2,3]").unwrap();
        assert!(
            parse_file(&path).is_none(),
            "non-object JSON is a clean skip"
        );
    }

    #[test]
    fn mixed_dir_wrong_typed_and_clean_both_present() {
        let dir = tempdir().unwrap();
        // One clean entry (via the write path).
        write_entry(dir.path(), &full_entry()).unwrap();
        // One wrong-typed entry: string startedAt — must still appear.
        fs::write(
            dir.path().join("100.json"),
            r#"{"pid":100,"sessionId":"degraded-one","startedAt":"12345"}"#,
        )
        .unwrap();

        let scanned = read_entries(dir.path(), false);
        assert_eq!(scanned.len(), 2, "both clean AND wrong-typed rows present");

        let degraded_row = scanned
            .iter()
            .find(|s| s.entry.pid == Some(100))
            .expect("wrong-typed row must be present");
        assert_eq!(degraded_row.entry.started_at, None);
        assert_eq!(degraded_row.degraded, vec!["startedAt"]);

        let clean_row = scanned
            .iter()
            .find(|s| s.entry.pid == Some(4242))
            .expect("clean row must be present");
        assert!(clean_row.degraded.is_empty());
    }

    #[test]
    fn read_entries_carries_empty_degraded_for_clean_rows() {
        let dir = tempdir().unwrap();
        write_entry(dir.path(), &full_entry()).unwrap();
        let scanned = read_entries(dir.path(), false);
        assert_eq!(scanned.len(), 1);
        assert!(scanned[0].degraded.is_empty());
    }

    #[test]
    fn tombstoned_entry_carries_degraded() {
        let dir = tempdir().unwrap();
        // Hand-write a wrong-typed tombstone file (pid in filename is well-formed).
        fs::write(
            dir.path().join("55.json.tombstoned"),
            r#"{"pid":"x","sessionId":"dead","startedAt":"y"}"#,
        )
        .unwrap();
        let tombs = get_tombstoned_entries(dir.path());
        assert_eq!(tombs.len(), 1);
        assert_eq!(tombs[0].pid, 55, "pid still parsed from filename");
        assert_eq!(tombs[0].data.pid, None, "wrong-typed body pid degraded");
        let got: std::collections::BTreeSet<_> = tombs[0].degraded.iter().copied().collect();
        let want: std::collections::BTreeSet<_> = ["pid", "startedAt"].into_iter().collect();
        assert_eq!(got, want);
    }

    // --- SB_DEBUG observability gate ---
    //
    // The stderr WRITE is a process-global side effect; rather than capture
    // process stderr (awkward + racy under parallel tests), we assert the DECISION
    // FUNCTION `debug_warn` that produces the warning string. It returns `Some`
    // (and writes) only when SB_DEBUG=1, `None` (silent) otherwise. We serialize
    // the env-var mutation behind a mutex so parallel tests don't race on it.

    #[test]
    fn debug_warn_silent_by_default_and_gated_on() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        // Default (unset): silent — returns None, writes nothing.
        std::env::remove_var("SB_DEBUG");
        assert_eq!(debug_warn("hello"), None, "silent by default");

        // SB_DEBUG=1: emits a formatted line.
        std::env::set_var("SB_DEBUG", "1");
        let line = debug_warn("degraded [\"startedAt\"]").expect("gate on → Some");
        assert!(line.starts_with("qd[registry]: "), "line: {line}");
        assert!(line.contains("startedAt"), "line: {line}");

        // Any other value is NOT the gate (only exact "1").
        std::env::set_var("SB_DEBUG", "0");
        assert_eq!(debug_warn("x"), None, "only SB_DEBUG=1 opens the gate");

        std::env::remove_var("SB_DEBUG");
    }

    /// W4 corpus row: the on-disk wrong-typed fixture (string startedAt + string
    /// pid) SURVIVES the engine read with both fields named degraded.
    #[test]
    fn dirty_state_wrong_typed_fixture_survives() {
        let path = fixtures_dir().join("wrong-typed.json");
        let (entry, degraded) =
            parse_file(&path).expect("wrong-typed.json row must SURVIVE per-field-permissive read");
        // Wrong-typed fields degraded.
        assert_eq!(entry.pid, None, "string pid degraded to None");
        assert_eq!(entry.started_at, None, "string startedAt degraded to None");
        // Well-typed siblings preserved (realistic row).
        assert_eq!(entry.session_id.as_deref(), Some("wrong-typed-1"));
        assert_eq!(entry.status.as_deref(), Some("busy"));
        // Degraded list names exactly the two wrong-typed fields (set-wise).
        let got: std::collections::BTreeSet<_> = degraded.into_iter().collect();
        let want: std::collections::BTreeSet<_> = ["pid", "startedAt"].into_iter().collect();
        assert_eq!(got, want);
    }

    // --- Tombstone ---

    #[test]
    fn tombstone_renames_and_reports_absence() {
        let dir = tempdir().unwrap();
        write_entry(dir.path(), &full_entry()).unwrap();
        assert!(tombstone(dir.path(), 4242));
        assert!(dir.path().join("4242.json.tombstoned").exists());
        assert!(!dir.path().join("4242.json").exists());
        // Absent live file → false.
        assert!(!tombstone(dir.path(), 4242));
        assert!(!tombstone(dir.path(), 9999));
    }

    #[test]
    fn ensure_tombstone_renames_live_file() {
        let dir = tempdir().unwrap();
        write_entry(dir.path(), &full_entry()).unwrap();
        ensure_tombstone(dir.path(), 4242, None);
        assert!(dir.path().join("4242.json.tombstoned").exists());
        assert!(!dir.path().join("4242.json").exists());
    }

    #[test]
    fn ensure_tombstone_is_idempotent() {
        let dir = tempdir().unwrap();
        write_entry(dir.path(), &full_entry()).unwrap();
        ensure_tombstone(dir.path(), 4242, None);
        // Second call: tombstone already exists → no-op, no panic, file intact.
        ensure_tombstone(dir.path(), 4242, Some(&full_entry()));
        let text = fs::read_to_string(dir.path().join("4242.json.tombstoned")).unwrap();
        let back: RegistryEntry = serde_json::from_str(&text).unwrap();
        assert_eq!(back, full_entry());
    }

    #[test]
    fn ensure_tombstone_synthesizes_from_captured() {
        let dir = tempdir().unwrap();
        // No live file at all (Claude removed it on graceful shutdown).
        let captured = full_entry();
        ensure_tombstone(dir.path(), 4242, Some(&captured));
        let path = dir.path().join("4242.json.tombstoned");
        assert!(path.exists());
        let back: RegistryEntry =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, captured);
    }

    #[test]
    fn ensure_tombstone_no_live_no_captured_is_noop() {
        let dir = tempdir().unwrap();
        ensure_tombstone(dir.path(), 4242, None);
        assert!(!dir.path().join("4242.json.tombstoned").exists());
    }

    #[test]
    fn get_tombstoned_entries_parses_pid_and_data() {
        let dir = tempdir().unwrap();
        write_entry(dir.path(), &full_entry()).unwrap();
        tombstone(dir.path(), 4242);
        // A live (non-tombstoned) entry must NOT appear.
        let mut other = full_entry();
        other.pid = Some(11);
        write_entry(dir.path(), &other).unwrap();

        let tombs = get_tombstoned_entries(dir.path());
        assert_eq!(tombs.len(), 1);
        assert_eq!(tombs[0].pid, 4242);
        assert_eq!(tombs[0].data, full_entry());
        assert!(tombs[0].mtime_ms > 0);
        assert!(tombs[0].path.ends_with("4242.json.tombstoned"));
    }

    // --- Atomic claim concurrency gate ---

    #[test]
    fn claim_round_trip_release_then_reclaim() {
        let dir = tempdir().unwrap();
        let claims = dir.path().join("claims");
        let c = claim_name(&claims, "alpha", b"payload-1", &|_| true, &|_| None).unwrap();
        // Second claim of the same name fails with the existing payload
        // (unparseable payload → never reaped, whatever the predicate says).
        match claim_name(&claims, "alpha", b"payload-2", &|_| false, &|_| None) {
            Err(ClaimError::AlreadyClaimed { existing_payload }) => {
                assert_eq!(existing_payload, b"payload-1");
            }
            other => panic!("expected AlreadyClaimed, got {other:?}"),
        }
        // Release, then re-claim succeeds.
        c.release().unwrap();
        let c2 = claim_name(&claims, "alpha", b"payload-3", &|_| true, &|_| None).unwrap();
        c2.release().unwrap();
    }

    /// Red-team r4 F1 (names case-insensitive for uniqueness): case-variants
    /// encode to ONE claim stem, so `WORKER` racing `worker` serializes at the
    /// same O_EXCL file — the second claimant loses (unparseable payload →
    /// never reaped, predicate irrelevant). Release frees BOTH spellings.
    #[test]
    fn claim_case_variants_share_one_claim_file() {
        let dir = tempdir().unwrap();
        let claims = dir.path().join("claims");
        let c = claim_name(&claims, "worker", b"payload-lower", &|_| true, &|_| None).unwrap();
        match claim_name(&claims, "WORKER", b"payload-upper", &|_| false, &|_| None) {
            Err(ClaimError::AlreadyClaimed { existing_payload }) => {
                assert_eq!(existing_payload, b"payload-lower");
            }
            other => panic!("expected AlreadyClaimed for the case-variant, got {other:?}"),
        }
        c.release().unwrap();
        let c2 = claim_name(&claims, "WORKER", b"payload-after", &|_| true, &|_| None).unwrap();
        c2.release().unwrap();
    }

    #[test]
    fn claim_encoding_is_traversal_safe_and_rejects_empty() {
        let dir = tempdir().unwrap();
        let claims = dir.path().join("claims");
        // Traversal attempt encodes to a safe stem INSIDE claims_dir: the
        // separators are %-escaped, so the file cannot escape, and the stem
        // is a direct child of claims_dir.
        let c = claim_name(&claims, "../../etc/passwd", b"x", &|_| true, &|_| None).unwrap();
        let path = c.path().to_path_buf();
        assert!(path.starts_with(&claims), "claim escaped dir: {path:?}");
        assert_eq!(
            path.parent().unwrap(),
            claims,
            "stem must be a direct child"
        );
        c.release().unwrap();

        // Only the genuinely EMPTY name is rejected ("/../" is a valid,
        // distinct name post-ADD-8 — it encodes losslessly).
        match claim_name(&claims, "", b"x", &|_| true, &|_| None) {
            Err(ClaimError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::InvalidInput),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    /// ADD-8 (redteam-retro #4): the encoding is COLLISION-FREE — the two
    /// names the lossy sanitizer used to collapse onto one stem now claim
    /// CONCURRENTLY and BOTH succeed, with distinct claim files.
    #[test]
    fn claim_collision_free_distinct_names_both_win_concurrently() {
        let dir = tempdir().unwrap();
        let claims = dir.path().join("claims");
        let claims_a = claims.clone();
        let claims_b = claims.clone();
        let a = std::thread::spawn(move || {
            claim_name(&claims_a, "etcpasswd", b"a", &|_| true, &|_| None)
        });
        let b = std::thread::spawn(move || {
            claim_name(&claims_b, "../../etc/passwd", b"b", &|_| true, &|_| None)
        });
        let ca = a
            .join()
            .unwrap()
            .expect("'etcpasswd' must win its own claim");
        let cb = b
            .join()
            .unwrap()
            .expect("'../../etc/passwd' must win its own distinct claim");
        assert_ne!(
            ca.path(),
            cb.path(),
            "distinct names must yield distinct claim files"
        );
        ca.release().unwrap();
        cb.release().unwrap();
    }

    #[test]
    fn claim_concurrency_exactly_one_winner_same_name() {
        let dir = tempdir().unwrap();
        let claims = Arc::new(dir.path().join("claims"));
        // Pre-create the dir so all threads race only on the O_EXCL open.
        fs::create_dir_all(&*claims).unwrap();

        const N: usize = 8;
        let barrier = Arc::new(Barrier::new(N));
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let claims = Arc::clone(&claims);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait(); // maximize contention
                    let payload = format!("thread-{i}");
                    claim_name(&claims, "racey", payload.as_bytes(), &|_| true, &|_| None).is_ok()
                })
            })
            .collect();

        let ok_count = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|&won| won)
            .count();
        assert_eq!(ok_count, 1, "exactly one thread must win the same name");
    }

    #[test]
    fn claim_concurrency_distinct_names_all_win() {
        let dir = tempdir().unwrap();
        let claims = Arc::new(dir.path().join("claims"));
        fs::create_dir_all(&*claims).unwrap();

        const N: usize = 8;
        let barrier = Arc::new(Barrier::new(N));
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let claims = Arc::clone(&claims);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    claim_name(&claims, &format!("name-{i}"), b"x", &|_| true, &|_| None).is_ok()
                })
            })
            .collect();

        let ok_count = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|&won| won)
            .count();
        assert_eq!(ok_count, N, "distinct names must all win");
    }

    // --- P0 redfix F2: stale-claim dead-holder reap ---

    /// A parseable claim whose holder pid is DEAD is reaped and the new claimer
    /// WINS (the SIGKILL-mid-boot unbrick). The winner's payload replaces it.
    #[test]
    fn claim_dead_holder_is_reaped_and_new_claimer_wins() {
        let dir = tempdir().unwrap();
        let claims = dir.path().join("claims");
        let stale = claim_name(
            &claims,
            "sess",
            br#"{"pid":4242,"name":"sess"}"#,
            &|_| true,
            &|_| None,
        )
        .unwrap();
        // The SIGKILLed holder: the file stays (NameClaim does not auto-release
        // on drop; release() was never called — exactly the brick scenario).
        let path = stale.path().to_path_buf();
        drop(stale);

        let c = claim_name(
            &claims,
            "sess",
            b"winner",
            &|pid| {
                assert_eq!(pid, 4242, "predicate sees the HOLDER's pid");
                false // dead
            },
            &|_| None,
        )
        .expect("dead-holder claim must be reaped; new claimer wins");
        assert_eq!(fs::read(&path).unwrap(), b"winner");
        c.release().unwrap();
    }

    /// An ALIVE holder is refused exactly as before — no reap.
    #[test]
    fn claim_alive_holder_is_refused() {
        let dir = tempdir().unwrap();
        let claims = dir.path().join("claims");
        let payload: &[u8] = br#"{"pid":4242,"name":"sess"}"#;
        let _c = claim_name(&claims, "sess", payload, &|_| true, &|_| None).unwrap();
        match claim_name(&claims, "sess", b"contender", &|_| true, &|_| None) {
            Err(ClaimError::AlreadyClaimed { existing_payload }) => {
                assert_eq!(existing_payload, payload);
            }
            other => panic!("expected AlreadyClaimed, got {other:?}"),
        }
    }

    /// An UNPARSEABLE payload (no pid to check) is never reaped — refused, and
    /// the predicate is not even consulted.
    #[test]
    fn claim_unparseable_payload_is_refused_not_reaped() {
        let dir = tempdir().unwrap();
        let claims = dir.path().join("claims");
        let _c = claim_name(&claims, "sess", b"not-json", &|_| true, &|_| None).unwrap();
        match claim_name(
            &claims,
            "sess",
            b"contender",
            &|_| panic!("predicate must not run on an unparseable payload"),
            &|_| None,
        ) {
            Err(ClaimError::AlreadyClaimed { existing_payload }) => {
                assert_eq!(existing_payload, b"not-json");
            }
            other => panic!("expected AlreadyClaimed, got {other:?}"),
        }
    }

    /// Reap-race: the retry create hits EEXIST again (a live racer recreated the
    /// claim in the remove→create window) → AlreadyClaimed, NOT a second reap.
    /// Simulated deterministically: the claims dir is made read-only so the
    /// reaper's remove silently fails and its retry O_EXCL sees the file again.
    #[test]
    fn claim_reap_race_second_eexist_is_refused() {
        let dir = tempdir().unwrap();
        let claims = dir.path().join("claims");
        let stale = claim_name(
            &claims,
            "sess",
            br#"{"pid":4242,"name":"sess"}"#,
            &|_| true,
            &|_| None,
        )
        .unwrap();
        drop(stale); // file stays — no auto-release

        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&claims, fs::Permissions::from_mode(0o555)).unwrap();
        let res = claim_name(&claims, "sess", b"contender", &|_| false, &|_| None);
        fs::set_permissions(&claims, fs::Permissions::from_mode(0o755)).unwrap();
        match res {
            Err(ClaimError::AlreadyClaimed { existing_payload }) => {
                assert_eq!(
                    existing_payload, br#"{"pid":4242,"name":"sess"}"#,
                    "the surviving claim is the racer's, surfaced as the holder"
                );
            }
            other => panic!("expected AlreadyClaimed on the second EEXIST, got {other:?}"),
        }
    }

    // --- qb punch B4 item 10: exec-proof claim identity ((pid, start-time)) ---

    /// PIN: a RECYCLED-PID claim — holder pid is alive, but its current
    /// occupant started AFTER the claimed start (beyond the kill-path slack) —
    /// is reaped, not honored. Pre-B4 this claim looked live forever (the
    /// pid-reuse window): is_alive(pid) alone cannot tell claimant from
    /// stranger.
    #[test]
    fn claim_recycled_pid_is_reaped_and_new_claimer_wins() {
        let dir = tempdir().unwrap();
        let claims = dir.path().join("claims");
        let claimed_start: i64 = 1_700_000_000_000;
        let payload =
            format!(r#"{{"pid":4242,"start":{claimed_start},"timestamp":0,"name":"sess"}}"#);
        let stale = claim_name(&claims, "sess", payload.as_bytes(), &|_| true, &|_| None).unwrap();
        let path = stale.path().to_path_buf();
        drop(stale); // SIGKILLed claimant; pid later recycled by a stranger

        let occupant_start = claimed_start + crate::kill::START_TIME_SLACK_MS + 1;
        let c = claim_name(
            &claims,
            "sess",
            b"winner",
            &|_| true, // the recycled pid IS alive
            &|pid| {
                assert_eq!(pid, 4242, "probe sees the HOLDER's pid");
                Some(occupant_start) // ...but its occupant started too late
            },
        )
        .expect("recycled-pid claim must be reaped; new claimer wins");
        assert_eq!(fs::read(&path).unwrap(), b"winner");
        c.release().unwrap();
    }

    /// PIN: a GENUINE live holder — pid alive AND current start within the
    /// slack of the claimed start — still refuses. Boundary included: exactly
    /// claimed + slack is OURS (the kill-path one-sided convention).
    #[test]
    fn claim_genuine_live_holder_pid_and_start_match_is_refused() {
        let dir = tempdir().unwrap();
        let claims = dir.path().join("claims");
        let claimed_start: i64 = 1_700_000_000_000;
        let payload =
            format!(r#"{{"pid":4242,"start":{claimed_start},"timestamp":0,"name":"sess"}}"#);
        let _c = claim_name(&claims, "sess", payload.as_bytes(), &|_| true, &|_| None).unwrap();

        for probe_start in [
            claimed_start,                                    // exact match
            claimed_start + crate::kill::START_TIME_SLACK_MS, // boundary: within slack
            claimed_start - 1_000, // probe jitter the other way (etime resolution)
        ] {
            match claim_name(&claims, "sess", b"contender", &|_| true, &|_| {
                Some(probe_start)
            }) {
                Err(ClaimError::AlreadyClaimed { existing_payload }) => {
                    assert_eq!(existing_payload, payload.as_bytes());
                }
                other => {
                    panic!("genuine holder (probe start {probe_start}) must refuse, got {other:?}")
                }
            }
        }
    }

    /// PIN (backward-compat): an OLD-FORMAT claim with NO `"start"` field —
    /// written by a pre-B4 binary mid-cutover — still parses and falls back to
    /// the is-alive-only check: an alive holder refuses (no regression), and
    /// the start probe is NEVER consulted.
    #[test]
    fn claim_old_format_no_start_field_is_alive_only_fallback() {
        let dir = tempdir().unwrap();
        let claims = dir.path().join("claims");
        let payload: &[u8] = br#"{"pid":4242,"timestamp":0,"name":"sess"}"#;
        let _c = claim_name(&claims, "sess", payload, &|_| true, &|_| None).unwrap();
        match claim_name(&claims, "sess", b"contender", &|_| true, &|_| {
            panic!("probe must not run on an old-format (no-start) claim")
        }) {
            Err(ClaimError::AlreadyClaimed { existing_payload }) => {
                assert_eq!(existing_payload, payload);
            }
            other => panic!("expected AlreadyClaimed, got {other:?}"),
        }
        // And the DEAD-holder reap still works on the old format (the F2
        // behavior is unchanged): pid dead → reaped.
        let c = claim_name(&claims, "sess", b"winner", &|_| false, &|_| None)
            .expect("dead old-format holder must still be reaped");
        c.release().unwrap();
    }

    /// PIN: an UNVERIFIABLE probe (`proc_start` = None on a live pid — a `ps`
    /// hiccup) honors the claim. "Cannot verify identity" is not "stale";
    /// reaping here could steal a live create's name.
    #[test]
    fn claim_unverifiable_start_probe_is_refused_not_reaped() {
        let dir = tempdir().unwrap();
        let claims = dir.path().join("claims");
        let payload: &[u8] = br#"{"pid":4242,"start":1700000000000,"timestamp":0,"name":"sess"}"#;
        let _c = claim_name(&claims, "sess", payload, &|_| true, &|_| None).unwrap();
        match claim_name(&claims, "sess", b"contender", &|_| true, &|_| None) {
            Err(ClaimError::AlreadyClaimed { existing_payload }) => {
                assert_eq!(existing_payload, payload);
            }
            other => panic!("expected AlreadyClaimed on probe-None, got {other:?}"),
        }
    }

    /// PIN: on a DEAD holder the probe is never consulted — the is-alive arm
    /// decides first (probe cost/noise stays off the common reap path).
    #[test]
    fn claim_dead_holder_reaped_without_probing_start() {
        let dir = tempdir().unwrap();
        let claims = dir.path().join("claims");
        let payload: &[u8] = br#"{"pid":4242,"start":1700000000000,"timestamp":0,"name":"sess"}"#;
        let stale = claim_name(&claims, "sess", payload, &|_| true, &|_| None).unwrap();
        drop(stale);
        let c = claim_name(&claims, "sess", b"winner", &|_| false, &|_| {
            panic!("probe must not run on a dead holder")
        })
        .expect("dead holder reaped via the is-alive arm alone");
        c.release().unwrap();
    }

    /// B4 S4 PIN: the hoisted `claim_payload` writer emits BOTH protocol shapes
    /// — `start` present (exec-proof) and absent (probe-failed / pre-B4) — and
    /// the absent shape is exactly what `claim_name`'s start arm treats as
    /// is-alive-only. Pins the writer beside the parser so they cannot drift.
    #[test]
    fn claim_payload_emits_both_protocol_shapes() {
        assert_eq!(
            claim_payload(4242, Some(1_700_000_000_000), 42, "sess"),
            r#"{"pid":4242,"start":1700000000000,"timestamp":42,"name":"sess"}"#
        );
        assert_eq!(
            claim_payload(4242, None, 42, "sess"),
            r#"{"pid":4242,"timestamp":42,"name":"sess"}"#
        );
        // Round-trips through the parser the reap path uses.
        let v: serde_json::Value =
            serde_json::from_str(&claim_payload(4242, Some(99), 0, "n")).unwrap();
        assert_eq!(v["pid"].as_i64(), Some(4242));
        assert_eq!(v["start"].as_i64(), Some(99));
        let v2: serde_json::Value =
            serde_json::from_str(&claim_payload(4242, None, 0, "n")).unwrap();
        assert_eq!(v2.get("start"), None, "absent shape carries no start field");
    }

    /// WS-R R3a-Step-2: the incarnation-aware writer is backward-compatible.
    /// Incarnation 0 is byte-identical to the legacy `claim_payload` (no field),
    /// and a non-zero incarnation appends `,"incarnation":N` before the brace.
    #[test]
    fn claim_payload_incarnation_is_additive_and_legacy_compatible() {
        // Incarnation 0 => EXACT legacy bytes (no "incarnation" field).
        assert_eq!(
            claim_payload_with_incarnation(4242, Some(99), 42, "sess", 0),
            claim_payload(4242, Some(99), 42, "sess"),
            "incarnation 0 must be byte-identical to the legacy writer"
        );
        assert_eq!(
            claim_payload_with_incarnation(4242, None, 42, "sess", 0),
            claim_payload(4242, None, 42, "sess"),
            "incarnation 0 (no start) must be byte-identical to the legacy writer"
        );
        // A non-zero incarnation appends the field at the end (additive).
        assert_eq!(
            claim_payload_with_incarnation(4242, Some(99), 42, "sess", 3),
            r#"{"pid":4242,"start":99,"timestamp":42,"name":"sess","incarnation":3}"#
        );
        assert_eq!(
            claim_payload_with_incarnation(4242, None, 42, "sess", 7),
            r#"{"pid":4242,"timestamp":42,"name":"sess","incarnation":7}"#
        );
    }

    /// WS-R R3a-Step-2 backward-compat control: a LEGACY claim file (no
    /// incarnation field) reads back as incarnation 0; a new one round-trips.
    #[test]
    fn claim_incarnation_reads_absent_as_zero_and_roundtrips() {
        // Legacy payload (the exact pre-R3a shape) => 0.
        let legacy = claim_payload(4242, Some(99), 42, "sess");
        assert_eq!(claim_incarnation(legacy.as_bytes()), 0, "legacy claim => incarnation 0");
        // Unparseable blob => 0 (serde(default) semantics).
        assert_eq!(claim_incarnation(b"not json"), 0, "corrupt claim => incarnation 0");
        // A new payload round-trips its incarnation.
        let p = claim_payload_with_incarnation(4242, Some(99), 42, "sess", 5);
        assert_eq!(claim_incarnation(p.as_bytes()), 5);
    }

    /// WS-R R3a-Step-2: `next_claim_incarnation` is monotonic. A missing claim is
    /// the first claim (=> 1); a prior claim at N yields N+1; a legacy claim file
    /// (no field, reads 0) yields 1.
    #[test]
    fn next_claim_incarnation_is_monotonic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let claims = dir.path();
        // No prior claim => first incarnation 1.
        assert_eq!(next_claim_incarnation(claims, "alpha"), 1);
        // Write a claim at incarnation 4; next must be 5.
        let stem = claim_file_name("alpha").unwrap();
        std::fs::write(
            claims.join(&stem),
            claim_payload_with_incarnation(100, Some(1), 0, "alpha", 4),
        )
        .unwrap();
        assert_eq!(next_claim_incarnation(claims, "alpha"), 5);
        // A LEGACY claim file (no incarnation field) reads 0 => next is 1.
        std::fs::write(claims.join(&stem), claim_payload(100, Some(1), 0, "alpha")).unwrap();
        assert_eq!(
            next_claim_incarnation(claims, "alpha"),
            1,
            "a legacy claim with no incarnation field reaps to incarnation 1"
        );
    }

    /// WS-R R3c item-1: `claim_name_with_incarnation` stamps a monotonic incarnation
    /// fence read INSIDE the O_EXCL critical section (TOCTOU-safe, clause 7). A fresh
    /// claim is incarnation 1; a reclaim over a DEAD holder bumps to 2 — the prior
    /// incarnation read from the reaped file, not pre-read before the create; a live
    /// holder refuses reclaim (no bump).
    ///
    /// NON-VACUITY (distinct revert seam): make the reap branch write incarnation 1
    /// instead of `next_claim_incarnation(...)` (i.e. drop the read-INSIDE bump) →
    /// the reclaim's incarnation-2 assertion REDs (the stale-writer name fence
    /// becomes vacuous — a writer at N could stomp N).
    #[test]
    fn claim_name_with_incarnation_bumps_monotonically_inside_the_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let claims = dir.path();
        // A payload builder for a given holder pid, stamping whatever incarnation
        // the claim assigns inside the section.
        let mk = |pid: u32| {
            move |inc: u64| {
                claim_payload_with_incarnation(pid, Some(1_000), 1_000, "alpha", inc).into_bytes()
            }
        };

        // Fresh claim by a LIVE holder (pid 111) → incarnation 1.
        let (_c1, inc1) =
            claim_name_with_incarnation(claims, "alpha", &|p| p == 111, &|_| None, &mk(111))
                .expect("fresh claim");
        assert_eq!(inc1, 1, "first claim is incarnation 1");
        let body1 = std::fs::read(claims.join("alpha.claim")).unwrap();
        assert_eq!(claim_incarnation(&body1), 1, "incarnation 1 stamped in the file");

        // The holder (pid 111) DIES → a reclaim reaps it and bumps the fence to 2,
        // read from the reaped file INSIDE the section (NOT pre-read).
        let (_c2, inc2) =
            claim_name_with_incarnation(claims, "alpha", &|_p| false, &|_| None, &mk(222))
                .expect("reclaim over dead holder");
        assert_eq!(inc2, 2, "reclaim over a dead holder bumps the fence to 2");
        let body2 = std::fs::read(claims.join("alpha.claim")).unwrap();
        assert_eq!(claim_incarnation(&body2), 2, "incarnation 2 stamped after reap");

        // A reclaim over a LIVE holder (pid 222) is REFUSED — the name is held, no
        // fence bump (a live successor is never stomped).
        match claim_name_with_incarnation(claims, "alpha", &|p| p == 222, &|_| None, &mk(333)) {
            Err(ClaimError::AlreadyClaimed { .. }) => {}
            other => panic!("a live holder must refuse reclaim, got {other:?}"),
        }
        // Still incarnation 2 (the refused reclaim wrote nothing).
        let body3 = std::fs::read(claims.join("alpha.claim")).unwrap();
        assert_eq!(claim_incarnation(&body3), 2, "a refused reclaim leaves the fence at 2");
    }

    /// B4 S3 PIN: `claim_file_name` returns the ENCODED on-disk basename — for a
    /// case-variant name `MyAgent` the file is `myagent.claim` (case-folded),
    /// NOT `MyAgent.claim`. The recovery hint MUST print this so `rm` works on a
    /// case-sensitive fs. Percent-escaping is also covered (a `/` → `%2F`).
    #[test]
    fn claim_file_name_is_the_encoded_on_disk_basename() {
        assert_eq!(claim_file_name("MyAgent").as_deref(), Some("myagent.claim"));
        assert_eq!(claim_file_name("worker").as_deref(), Some("worker.claim"));
        // Percent-escape for a non-whitelisted byte (defense-in-depth; create
        // charset-validates names, but the encoder is total).
        assert_eq!(claim_file_name("a/b").as_deref(), Some("a%2Fb.claim"));
        assert_eq!(claim_file_name(""), None);
    }

    // --- P0 redfix F1: bind-row selection (pick_live_named_row) ---

    fn named_row(
        name: &str,
        pid: Option<i64>,
        sid: Option<&str>,
        tombstoned: bool,
    ) -> ScannedEntry {
        ScannedEntry {
            entry: RegistryEntry {
                pid,
                session_id: sid.map(str::to_string),
                name: Some(name.to_string()),
                ..RegistryEntry::default()
            },
            tombstoned,
            degraded: vec![],
        }
    }

    /// Arm 1 (the F1 regression): a crash-leftover DEAD row sharing the name —
    /// listed FIRST, the read-dir-order trap — must lose to the alive row.
    #[test]
    fn pick_live_named_row_one_alive_wins_over_dead_namesake() {
        let rows = vec![
            named_row("wk", Some(111), Some("uuid-stale"), false), // dead leftover, first
            named_row("wk", Some(222), Some("uuid-live"), false),  // the booted session
            named_row("other", Some(333), Some("uuid-other"), false),
        ];
        let pick = pick_live_named_row(&rows, "wk", &|pid| pid == 222 || pid == 333);
        assert_eq!(
            pick,
            LiveNamePick::One {
                session_id: "uuid-live".to_string()
            }
        );
    }

    /// Arm 2: zero alive name-matches (all dead, or no pid at all) → NoneBindable.
    /// A lone alive row WITHOUT a sessionId is also NoneBindable (nothing to
    /// bind to yet), and a tombstoned alive-pid row never counts.
    #[test]
    fn pick_live_named_row_none_alive_or_no_sid_is_none_bindable() {
        let dead = vec![
            named_row("wk", Some(111), Some("uuid-a"), false),
            named_row("wk", None, Some("uuid-b"), false), // no pid = never alive
        ];
        assert_eq!(
            pick_live_named_row(&dead, "wk", &|_| false),
            LiveNamePick::NoneBindable
        );
        let no_sid = vec![named_row("wk", Some(222), None, false)];
        assert_eq!(
            pick_live_named_row(&no_sid, "wk", &|_| true),
            LiveNamePick::NoneBindable
        );
        let empty_sid = vec![named_row("wk", Some(222), Some(""), false)];
        assert_eq!(
            pick_live_named_row(&empty_sid, "wk", &|_| true),
            LiveNamePick::NoneBindable
        );
        let tombstone = vec![named_row("wk", Some(222), Some("uuid-t"), true)];
        assert_eq!(
            pick_live_named_row(&tombstone, "wk", &|_| true),
            LiveNamePick::NoneBindable
        );
    }

    /// Arm 3: >1 ALIVE rows claiming the name → Ambiguous, never a silent pick.
    #[test]
    fn pick_live_named_row_two_alive_is_ambiguous() {
        let rows = vec![
            named_row("wk", Some(111), Some("uuid-a"), false),
            named_row("wk", Some(222), Some("uuid-b"), false),
        ];
        assert_eq!(
            pick_live_named_row(&rows, "wk", &|_| true),
            LiveNamePick::Ambiguous { count: 2 }
        );
    }

    // ===================================================================
    // WP-B2b-1 — atomic write (§H.6) + CAS-guarded set_status
    // ===================================================================

    /// Helper: list any leftover tmp files (`.<pid>.json.tmp.*`) in a dir.
    fn tmp_litter(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|d| d.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".json.tmp."))
            .collect()
    }

    // --- Test #1: atomicity (§H.6) ---

    /// (a) A successful `write_entry` leaves NO `.tmp` litter and the live
    /// `<pid>.json` is complete valid JSON. (c) The live file round-trips through
    /// `read_entry` unchanged (byte parity preserved through the atomic path).
    #[test]
    fn write_entry_atomic_no_tmp_litter_and_round_trips() {
        let dir = tempdir().unwrap();
        let entry = full_entry();
        write_entry(dir.path(), &entry).unwrap();

        // (a) no tmp litter.
        assert!(
            tmp_litter(dir.path()).is_empty(),
            "atomic write must leave no .tmp file behind"
        );
        // live file is complete valid JSON.
        let text = fs::read_to_string(dir.path().join("4242.json")).unwrap();
        let _: serde_json::Value =
            serde_json::from_str(&text).expect("live file is complete valid JSON");
        // byte parity: pretty, 2-space indent, no trailing newline (TS shape).
        assert!(text.contains("\n  \"pid\""), "2-space indent: {text}");
        assert!(!text.ends_with('\n'), "no trailing newline (TS parity)");
        // (c) round-trips unchanged.
        let back = read_entry(dir.path(), 4242).unwrap();
        assert_eq!(back, entry);
    }

    /// (b) Torn-write proof: simulate a crash BETWEEN tmp-write and rename (write
    /// a tmp file ourselves and never rename it) → the pre-existing `<pid>.json`
    /// is still the OLD complete row, never a partial/torn one. A reader sees the
    /// old complete row.
    #[test]
    fn write_entry_torn_write_leaves_old_row_intact() {
        let dir = tempdir().unwrap();
        // Establish the OLD complete row.
        let mut old = full_entry();
        old.status = Some("idle".into());
        write_entry(dir.path(), &old).unwrap();
        let old_bytes = fs::read(dir.path().join("4242.json")).unwrap();

        // Simulate the crash: a NEW row was being serialized to a tmp file but the
        // process died before the rename. Write a (here deliberately truncated /
        // "torn") tmp file directly; do NOT rename it.
        let torn_tmp = dir
            .path()
            .join(format!(".4242.json.tmp.{}.crash", std::process::id()));
        fs::write(&torn_tmp, b"{\"pid\":4242,\"status\":\"bu").unwrap(); // torn

        // The live <pid>.json is UNTOUCHED — still the old complete row.
        let live_bytes = fs::read(dir.path().join("4242.json")).unwrap();
        assert_eq!(
            live_bytes, old_bytes,
            "torn tmp must not affect the live row"
        );
        // And it still parses as the OLD entry (never partial).
        let back = read_entry(dir.path(), 4242).unwrap();
        assert_eq!(back.status.as_deref(), Some("idle"));
        assert_eq!(back, old);
    }

    // --- Test #2: CAS / no-split-brain (keystone false-accept guard) ---

    /// A foreign incarnation owns the row (on-disk `started_at=1000`); a writer
    /// that believes it owns incarnation 2000 must be REJECTED and the on-disk row
    /// must be byte-for-byte UNCHANGED (status NOT flipped). This is the keystone
    /// false-accept guard.
    #[test]
    fn set_status_cas_rejects_foreign_incarnation() {
        let dir = tempdir().unwrap();
        let mut row = full_entry();
        row.started_at = Some(1000);
        row.status = Some("idle".into());
        write_entry(dir.path(), &row).unwrap();
        let before = fs::read(dir.path().join("4242.json")).unwrap();

        let outcome = set_status(dir.path(), 4242, Some(2000), "busy", 9_999).unwrap();
        assert_eq!(
            outcome,
            StatusWriteOutcome::Rejected {
                on_disk_started_at: Some(1000)
            }
        );
        // On-disk row byte-for-byte UNCHANGED (status not flipped, no heartbeat).
        let after = fs::read(dir.path().join("4242.json")).unwrap();
        assert_eq!(before, after, "rejected CAS must not touch the on-disk row");
        let back = read_entry(dir.path(), 4242).unwrap();
        assert_eq!(back.status.as_deref(), Some("idle"));
    }

    // --- Test #3: CAS accept (false-reject guard) ---

    /// Matching `expected_started_at` → Written; status updated, every other field
    /// preserved; `started_at: None` on disk → adopted → Written.
    #[test]
    fn set_status_cas_accepts_matching_and_none_incarnation() {
        // Matching stamp.
        let dir = tempdir().unwrap();
        let mut row = full_entry();
        row.started_at = Some(1000);
        row.status = Some("idle".into());
        write_entry(dir.path(), &row).unwrap();
        let outcome = set_status(dir.path(), 4242, Some(1000), "busy", 12345).unwrap();
        assert_eq!(outcome, StatusWriteOutcome::Written);
        let back = read_entry(dir.path(), 4242).unwrap();
        assert_eq!(back.status.as_deref(), Some("busy"));
        assert_eq!(back.updated_at, Some(12345));
        // every other field preserved.
        assert_eq!(back.session_id, row.session_id);
        assert_eq!(back.cwd, row.cwd);
        assert_eq!(back.started_at, Some(1000));
        assert_eq!(back.name, row.name);
        assert_eq!(back.spawned_by, row.spawned_by);
        assert_eq!(back.provider, row.provider);

        // None on disk → adopted.
        let dir2 = tempdir().unwrap();
        let mut row2 = full_entry();
        row2.started_at = None;
        write_entry(dir2.path(), &row2).unwrap();
        let outcome2 = set_status(dir2.path(), 4242, Some(7777), "busy", 1).unwrap();
        assert_eq!(outcome2, StatusWriteOutcome::Written);
        assert_eq!(
            read_entry(dir2.path(), 4242).unwrap().status.as_deref(),
            Some("busy")
        );
    }

    // --- Test #4: NoRow ---

    /// `set_status` on an absent `<pid>.json` → NoRow, no file created.
    #[test]
    fn set_status_absent_row_is_norow_no_file_created() {
        let dir = tempdir().unwrap();
        let outcome = set_status(dir.path(), 4242, Some(1000), "busy", 1).unwrap();
        assert_eq!(outcome, StatusWriteOutcome::NoRow);
        assert!(
            !dir.path().join("4242.json").exists(),
            "status updates never CREATE a row"
        );
        assert!(tmp_litter(dir.path()).is_empty());
    }

    // --- Test #5: idempotency ---

    /// `set_status(.., "busy", ..)` twice → status stable "busy", exactly one
    /// `<pid>.json`, valid JSON; only `updated_at` differs.
    #[test]
    fn set_status_is_idempotent() {
        let dir = tempdir().unwrap();
        let mut row = full_entry();
        row.started_at = Some(1000);
        write_entry(dir.path(), &row).unwrap();

        assert_eq!(
            set_status(dir.path(), 4242, Some(1000), "busy", 100).unwrap(),
            StatusWriteOutcome::Written
        );
        let first = read_entry(dir.path(), 4242).unwrap();
        assert_eq!(
            set_status(dir.path(), 4242, Some(1000), "busy", 200).unwrap(),
            StatusWriteOutcome::Written
        );
        let second = read_entry(dir.path(), 4242).unwrap();

        assert_eq!(second.status.as_deref(), Some("busy"));
        // exactly one <pid>.json, no tmp litter.
        assert!(tmp_litter(dir.path()).is_empty());
        assert!(dir.path().join("4242.json").exists());
        // the only delta is updated_at.
        assert_eq!(first.updated_at, Some(100));
        assert_eq!(second.updated_at, Some(200));
        let mut a = first.clone();
        a.updated_at = None;
        let mut b = second.clone();
        b.updated_at = None;
        assert_eq!(a, b, "only updated_at may differ between idempotent calls");
    }

    // --- Test #6: concurrency k>=2 ---

    /// Two-or-more threads each call `set_status` (same matching incarnation,
    /// statuses from a small set) on the same pid: no corruption, final file is
    /// valid JSON, status is one of the written set, no leftover tmp files. Models
    /// the `claim_concurrency_*` idiom already in the crate.
    #[test]
    fn set_status_concurrency_no_corruption() {
        let dir = tempdir().unwrap();
        let path = Arc::new(dir.path().to_path_buf());
        let mut row = full_entry();
        row.started_at = Some(1000);
        write_entry(&path, &row).unwrap();

        const N: usize = 8;
        let statuses = ["busy", "idle", "offline"];
        let barrier = Arc::new(Barrier::new(N));
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let path = Arc::clone(&path);
                let barrier = Arc::clone(&barrier);
                let st = statuses[i % statuses.len()];
                thread::spawn(move || {
                    barrier.wait();
                    set_status(&path, 4242, Some(1000), st, 1000 + i as i64).unwrap()
                })
            })
            .collect();
        for h in handles {
            // Every concurrent write adopts the matching incarnation.
            assert_eq!(h.join().unwrap(), StatusWriteOutcome::Written);
        }
        // Final file: valid JSON, status in the set, no tmp litter.
        let final_entry = read_entry(&path, 4242).expect("final file is valid + parses");
        assert!(statuses.contains(&final_entry.status.as_deref().unwrap()));
        assert_eq!(final_entry.started_at, Some(1000));
        assert!(
            tmp_litter(&path).is_empty(),
            "no leftover .tmp files after concurrent writes"
        );
    }
}
