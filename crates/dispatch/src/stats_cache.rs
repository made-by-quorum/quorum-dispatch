//! lsview A1: a persistent, provider-shared per-transcript stats cache.
//!
//! ## Why
//!
//! `qd ls` gathers stats for EVERY transcript on disk on every invocation
//! (`join::gather` is exhaustive; the `-n` cap is applied later in the pure
//! join). Each stat is a full `fs::read_to_string` + parse of the transcript
//! (`jsonl::read_stats`, `provider::codex::rollout::read_stats`). On a busy host
//! that is a lot of cold reads per `ls`. This cache eliminates them: a transcript
//! whose `(path, mtime, size)` is unchanged since the last `ls` is served from
//! the cache WITHOUT re-reading its content.
//!
//! ## The seam, not the provider
//!
//! The cache lives at the single point where per-transcript stats are ACQUIRED —
//! the `read_stats` call sites inside `join::gather_with_dirs` / `gather_codex`.
//! It is deliberately **provider-agnostic**: [`StatsCache::get_or_read`] takes the
//! actual read as an injected closure (`|path| jsonl::read_stats(path, true)` for
//! claude, `|path| codex::rollout::read_stats(path, true)` for codex). There is
//! NO per-provider cache code — a later provider gets caching for free by routing
//! its acquisition through the same seam. That injected closure is ALSO the
//! **read-counter test-seam**: a warm call never invokes it, so "a warm call does
//! zero transcript-content reads" is a hermetic assertion (a test injects a
//! counting reader; no strace/dtruss).
//!
//! ## Always store full; serve subsets
//!
//! The cache always stores the FULL stats (previews included) and serves a subset
//! on request: a `include_preview == false` request gets the stored stats with
//! `last_turns` nulled. Both `read_stats` impls gate ONLY `last_turns` on
//! `include_preview` (every other field is computed identically), so a served
//! subset is byte-identical to a direct `read_stats(path, include_preview)`. This
//! sidesteps the condition-5 hazard class entirely: a preview-less hit can never
//! be served where previews were requested, because the store ALWAYS has previews
//! (STANDARD condition 5).
//!
//! ## Durability + concurrency (the `marks.jsonl` precedent)
//!
//! The snapshot lives in the dispatch **state dir** (`QdPaths::state_dir`, the
//! same dir `marks.jsonl` uses) as a schema-versioned JSON snapshot, written
//! atomically via a tmp-file + `fs::rename` (the `registry::atomic_write`
//! precedent). Two concurrent `qd ls` invocations therefore never corrupt the
//! file and never error — a reader sees the OLD or the NEW complete snapshot,
//! never a torn one; last-writer-wins (STANDARD condition 4). A snapshot that is
//! missing, corrupt (torn/truncated/garbage), or written by an older schema
//! version loads as EMPTY → a silent full rebuild, never an error surfaced to
//! `ls`, never a wrong row (STANDARD condition 3).
//!
//! ## Accepted residual
//!
//! An in-place same-size, same-mtime content rewrite is undetectable under the
//! `(path, mtime, size)` key. Real transcripts are append-only; this is a
//! deliberate consequence of the parent spec's key choice — noted, not worked.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::jsonl::JsonlStats;
use crate::model::{SessionStatus, TurnPreview};

/// On-disk snapshot schema version. Bump on ANY incompatible change to the stored
/// shape; an older/newer value loads as empty → silent rebuild (condition 3).
///
/// v2 (lsview A1 F1): [`CacheEntry`] gained the `status` slot (the memoized codex
/// live-row status). The bump is load-bearing for RS1.6: a v1 snapshot has no
/// `status` field, so without the bump `#[serde(default)]` would silently read
/// every old entry as `StatusSlot::Absent` — harmless by itself, but the bump
/// makes a pre-fix snapshot rebuild wholesale, so NO pre-fix entry is ever
/// served and an absent field can never masquerade as a derived status.
const SCHEMA_VERSION: u32 = 2;

/// The snapshot filename under the state dir (sibling of `marks.jsonl`).
const CACHE_FILENAME: &str = "ls-stats-cache.json";

// ===========================================================================
// Serializable mirror of JsonlStats / TurnPreview.
//
// `TurnPreview.role` is `&'static str`, which cannot be `Deserialize`d directly,
// so the on-disk form uses owned DTOs and converts. This keeps the serde surface
// entirely inside this module — `model.rs` / `jsonl.rs` are untouched, so no
// rendered-output behavior can shift (condition 7).
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TurnPreviewDto {
    role: String,
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StatsDto {
    turns: u64,
    tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    user_named: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_turns: Option<Vec<TurnPreviewDto>>,
}

impl StatsDto {
    /// Snapshot a FULL `JsonlStats` (previews included) into the on-disk DTO.
    fn from_full(s: &JsonlStats) -> Self {
        StatsDto {
            turns: s.turns,
            tokens: s.tokens,
            name: s.name.clone(),
            user_named: s.user_named,
            last_timestamp: s.last_timestamp.clone(),
            git_branch: s.git_branch.clone(),
            cwd: s.cwd.clone(),
            last_turns: s.last_turns.as_ref().map(|turns| {
                turns
                    .iter()
                    .map(|t| TurnPreviewDto {
                        role: t.role.to_string(),
                        text: t.text.clone(),
                        timestamp: t.timestamp.clone(),
                    })
                    .collect()
            }),
        }
    }

    /// Rehydrate to a FULL `JsonlStats`. Returns `None` when a preview `role` is
    /// not one of the two known static strings — a per-entry corruption guard: an
    /// unrecognized role makes the entry un-representable, so the caller treats it
    /// as a miss and re-reads (stronger than the whole-file rebuild condition 3
    /// requires, and it can never mint a wrong `role`).
    fn to_full(&self) -> Option<JsonlStats> {
        let last_turns = match &self.last_turns {
            None => None,
            Some(turns) => {
                let mut out = Vec::with_capacity(turns.len());
                for t in turns {
                    let role = role_static(&t.role)?;
                    out.push(TurnPreview {
                        role,
                        text: t.text.clone(),
                        timestamp: t.timestamp.clone(),
                    });
                }
                Some(out)
            }
        };
        Some(JsonlStats {
            turns: self.turns,
            tokens: self.tokens,
            name: self.name.clone(),
            user_named: self.user_named,
            last_timestamp: self.last_timestamp.clone(),
            git_branch: self.git_branch.clone(),
            cwd: self.cwd.clone(),
            last_turns,
        })
    }
}

/// Map a stored role string back to the `&'static str` the model uses. Only
/// `"user"` / `"assistant"` are ever written; anything else is corruption.
fn role_static(s: &str) -> Option<&'static str> {
    match s {
        "user" => Some("user"),
        "assistant" => Some("assistant"),
        _ => None,
    }
}

/// The memoized codex live-row status carried alongside a rollout's stats
/// (lsview A1 F1). Three states, kept DISTINCT so a status-aware warm hit never
/// serves a status the reader did not derive:
///   - `Absent` — status was NEVER derived for this entry: it was produced by a
///     stats-only read (the claude sites, or the codex COLD-enrichment site). A
///     status-aware read MISSES on it and re-derives, then UPGRADES the entry.
///     This is the RS1.3 cold→live corner: a cold-cached rollout that later goes
///     live re-derives its status rather than conflating "never derived" with
///     "derived None" (which would serve a phantom Idle over a real Busy).
///   - `Derived(None)` — status WAS derived and `derive_status` returned None
///     (no turn anchors / unreadable tail); the join falls back to Idle. Served
///     warm, indistinguishable from a fresh `None`.
///   - `Derived(Some(status))` — derived Busy/Idle/…, stored as the lowercase
///     surface string (`SessionStatus::as_str`); served warm.
///
/// The serde surface stays entirely inside this module (no `SessionStatus` serde
/// derive), matching the `TurnPreviewDto` / `role_static` discipline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
enum StatusSlot {
    #[default]
    Absent,
    Derived(Option<String>),
}

impl StatusSlot {
    /// Build a slot from a freshly-derived status (what `derive_status` returned).
    fn from_derived(status: Option<SessionStatus>) -> Self {
        StatusSlot::Derived(status.map(|s| s.as_str().to_owned()))
    }

    /// The status to serve on a warm status-aware HIT, or `None` when this slot
    /// cannot be served warm (status was never derived, or a stored string is
    /// unrecognized) → the caller MISSES and re-derives. The OUTER `Option` is
    /// "servable?"; the INNER is the derived value (`None` = no-anchors → the
    /// join's Idle fallback). Mirrors `StatsDto::to_full`'s corrupt-guard: an
    /// unrepresentable stored value degrades to a re-read, never a wrong row.
    fn servable(&self) -> Option<Option<SessionStatus>> {
        match self {
            StatusSlot::Absent => None,
            StatusSlot::Derived(None) => Some(None),
            StatusSlot::Derived(Some(s)) => SessionStatus::parse(s).map(Some),
        }
    }
}

/// One cache entry: the invalidation key `(mtime_ms, size)` + the full stats +
/// the memoized codex live-row status (`Absent` for a stats-only entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    mtime_ms: i64,
    size: u64,
    stats: StatsDto,
    /// The memoized codex live-row status. `#[serde(default)]` → a schema-2 entry
    /// missing the field loads as `Absent` (safe: a status-aware read re-derives).
    /// A pre-fix (schema-1) snapshot never reaches this — the schema bump makes it
    /// load empty (rebuild) — so an absent field can never be misread as a derived
    /// status (RS1.6).
    #[serde(default)]
    status: StatusSlot,
}

/// The on-disk snapshot: a schema tag + path→entry map. Path keys are strings
/// (JSON object keys); the in-memory form uses `PathBuf`.
#[derive(Debug, Serialize, Deserialize)]
struct Snapshot {
    schema: u32,
    entries: HashMap<String, CacheEntry>,
}

// ===========================================================================
// The cache.
// ===========================================================================

/// A persistent, provider-shared per-transcript stats cache. Built once per
/// `gather`, consulted at every stats-acquisition site, persisted once at the end.
pub struct StatsCache {
    state_dir: PathBuf,
    /// path → entry. Seeded from the on-disk snapshot at [`load`], updated in place
    /// as transcripts are read/refreshed this run.
    ///
    /// [`load`]: StatsCache::load
    entries: HashMap<PathBuf, CacheEntry>,
    /// Paths consulted this run. Persist writes ONLY these, which self-prunes
    /// entries for transcripts that no longer exist (a deleted file is never
    /// re-touched → dropped from the next snapshot).
    touched: HashSet<PathBuf>,
    /// True when the on-disk snapshot was unusable (missing/corrupt/old-schema)
    /// and this run started from an empty map — i.e. a full rebuild is underway.
    loaded_empty: bool,
    /// Something changed (a miss, a refresh, or a prune) → the snapshot is stale
    /// and must be rewritten on [`persist`]. A pure-hit run leaves this false and
    /// skips the write entirely (no needless contention).
    ///
    /// [`persist`]: StatsCache::persist
    dirty: bool,
    /// Instrument: warm HITS this run (served without a content read).
    hits: usize,
    /// Instrument: transcript-content READS this run (reader-closure invocations).
    /// The headline condition — "a warm call does zero transcript-content reads" —
    /// is `reads` staying flat across a warm pass over an unchanged fleet.
    reads: usize,
}

impl StatsCache {
    /// Load the snapshot from `<state_dir>/ls-stats-cache.json`. A missing,
    /// unreadable, corrupt, or old-schema file yields an EMPTY cache (silent
    /// rebuild — condition 3); never an error.
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(CACHE_FILENAME);
        let (entries, loaded_empty) = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Snapshot>(&bytes) {
                Ok(snap) if snap.schema == SCHEMA_VERSION => (
                    snap.entries
                        .into_iter()
                        .map(|(k, v)| (PathBuf::from(k), v))
                        .collect(),
                    false,
                ),
                // Parse error, wrong schema, or a torn/truncated tail → rebuild.
                _ => (HashMap::new(), true),
            },
            // Missing / unreadable → cold start (rebuild).
            Err(_) => (HashMap::new(), true),
        };
        StatsCache {
            state_dir: state_dir.to_path_buf(),
            entries,
            touched: HashSet::new(),
            loaded_empty,
            dirty: false,
            hits: 0,
            reads: 0,
        }
    }

    /// Return stats for `path` at the requested `include_preview`.
    ///
    /// HIT (the entry exists and `(mtime_ms, size)` are unchanged): returns the
    /// stored stats WITHOUT invoking `read_full` — zero transcript-content reads.
    ///
    /// MISS (absent, or `mtime`/`size` changed — an append/rotate/rewrite):
    /// invokes `read_full(path)` exactly once — which MUST read FULL stats
    /// (previews included, i.e. `read_stats(path, true)`) — stores it, and serves
    /// the subset.
    ///
    /// A stat failure (the transcript was deleted/rotated out from under us) never
    /// errors: the stale entry is dropped and `read_full` is called, which for a
    /// missing file returns the zeroed default exactly as today (condition 2).
    ///
    /// `include_preview == false` nulls `last_turns` on the way out, matching a
    /// direct `read_stats(path, false)` byte-for-byte.
    pub fn get_or_read<F>(&mut self, path: &Path, include_preview: bool, read_full: F) -> JsonlStats
    where
        F: FnOnce(&Path) -> JsonlStats,
    {
        let Some((size, mtime_ms)) = self.stat_key(path) else {
            // Deleted / rotated / not a regular file: never error the ls; the
            // stale entry was dropped by `stat_key`. Fall back to the reader
            // (default for a missing file), COUNTING the read (RS2.1: the counter
            // observes EVERY reader invocation the gather performs).
            self.reads += 1;
            return read_full(path);
        };

        // HIT: key matches AND the stored entry rehydrates cleanly.
        if let Some(entry) = self.entries.get(path) {
            if entry.mtime_ms == mtime_ms && entry.size == size {
                if let Some(full) = entry.stats.to_full() {
                    self.hits += 1;
                    self.touched.insert(path.to_path_buf());
                    return subset(full, include_preview);
                }
                // else: a corrupt entry (bad role) → fall through to a re-read.
            }
        }

        // MISS: read FULL, store (stats-only → status `Absent`), serve the subset.
        self.reads += 1;
        let full = read_full(path);
        self.entries.insert(
            path.to_path_buf(),
            CacheEntry {
                mtime_ms,
                size,
                stats: StatsDto::from_full(&full),
                status: StatusSlot::Absent,
            },
        );
        self.touched.insert(path.to_path_buf());
        self.dirty = true;
        subset(full, include_preview)
    }

    /// Like [`get_or_read`], but the reader ALSO derives the codex live-row status
    /// from the SAME single content read, and that status is memoized ALONGSIDE
    /// the stats in the SAME `(path, mtime, size)` entry (lsview A1 F1). A warm
    /// status-aware HIT serves BOTH stats and status with ZERO content reads —
    /// closing F1's gap where the codex live-row status read slipped past the
    /// cache and the counter.
    ///
    /// A HIT requires the entry to (a) key-match, (b) rehydrate its stats, AND
    /// (c) carry a DERIVED status (`StatusSlot::servable` is `Some`). An entry
    /// produced by a stats-only read ([`get_or_read`]: the cold-enrichment site)
    /// has status `Absent` → a status MISS → the reader runs, UPGRADING the entry
    /// to carry the derived status (RS1.3 cold→live corner: a status a stats-only
    /// entry never held is re-derived and COUNTED, never conflated with a derived
    /// `None`). A memoized `Derived(None)` behaves exactly like a fresh `None`
    /// (served warm; the join falls back to Idle).
    ///
    /// The reader MUST derive both from ONE parse (the `derive_status` +
    /// `read_stats_from_lines` pair over a single `read_lines`), so a MISS is one
    /// content read, not two (RS1.5: cold never regresses — it improves 2→1).
    pub fn get_or_read_with_status<F>(
        &mut self,
        path: &Path,
        include_preview: bool,
        read_full: F,
    ) -> (JsonlStats, Option<SessionStatus>)
    where
        F: FnOnce(&Path) -> (JsonlStats, Option<SessionStatus>),
    {
        let Some((size, mtime_ms)) = self.stat_key(path) else {
            // Deleted / rotated: never error; count the fallback read (RS2.1).
            self.reads += 1;
            return read_full(path);
        };

        // HIT: key matches, stats rehydrate, AND a status was derived for it.
        if let Some(entry) = self.entries.get(path) {
            if entry.mtime_ms == mtime_ms && entry.size == size {
                if let (Some(status), Some(full)) =
                    (entry.status.servable(), entry.stats.to_full())
                {
                    self.hits += 1;
                    self.touched.insert(path.to_path_buf());
                    return (subset(full, include_preview), status);
                }
                // else: key matches but status is `Absent` (a stats-only entry)
                // or the entry is corrupt → fall through to a re-read + upgrade.
            }
        }

        // MISS: ONE content read yields both; store stats + derived status.
        self.reads += 1;
        let (full, status) = read_full(path);
        self.entries.insert(
            path.to_path_buf(),
            CacheEntry {
                mtime_ms,
                size,
                stats: StatsDto::from_full(&full),
                status: StatusSlot::from_derived(status),
            },
        );
        self.touched.insert(path.to_path_buf());
        self.dirty = true;
        (subset(full, include_preview), status)
    }

    /// Resolve `(size, mtime_ms)` for `path` when it is a present regular file.
    /// On a stat failure (deleted / rotated / not a regular file) drop any stale
    /// entry and return `None` — the caller falls back to the reader without
    /// erroring (condition 2). Metadata only; never reads content.
    fn stat_key(&mut self, path: &Path) -> Option<(u64, i64)> {
        match std::fs::metadata(path).ok().filter(|m| m.is_file()) {
            Some(meta) => Some((meta.len(), mtime_ms(&meta))),
            None => {
                if self.entries.remove(path).is_some() {
                    self.dirty = true;
                }
                None
            }
        }
    }

    /// Write the snapshot atomically (tmp-file + `fs::rename`) IF anything changed
    /// this run. Writes ONLY touched entries, self-pruning transcripts that no
    /// longer exist. Best-effort: any I/O error is swallowed — persistence is an
    /// optimization, never a reason to fail an `ls` (conditions 2/3/4).
    pub fn persist(&self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let entries: HashMap<String, CacheEntry> = self
            .touched
            .iter()
            .filter_map(|p| {
                self.entries
                    .get(p)
                    .map(|e| (p.to_string_lossy().into_owned(), e.clone()))
            })
            .collect();
        let snapshot = Snapshot {
            schema: SCHEMA_VERSION,
            entries,
        };
        let bytes = serde_json::to_vec(&snapshot)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        atomic_write(&self.state_dir, &self.state_dir.join(CACHE_FILENAME), &bytes)
    }

    /// Warm hits served this run (no content read).
    pub fn hits(&self) -> usize {
        self.hits
    }

    /// Transcript-content reads this run (reader-closure invocations). The
    /// zero-warm-reads instrument.
    pub fn transcript_reads(&self) -> usize {
        self.reads
    }

    /// Whether this run started from an empty/unusable snapshot (a full rebuild).
    pub fn loaded_empty(&self) -> bool {
        self.loaded_empty
    }

    /// A one-line hit/miss/rebuild summary for the `QD_CACHE_STATS` stderr hook.
    pub fn debug_line(&self) -> String {
        format!(
            "[stats-cache] hits={} reads={} entries={} rebuilt={}",
            self.hits,
            self.reads,
            self.touched.len(),
            self.loaded_empty,
        )
    }
}

/// Serve a subset of a full (preview-carrying) stats: drop previews when they
/// were not requested, matching `read_stats(path, false)` exactly.
fn subset(mut full: JsonlStats, include_preview: bool) -> JsonlStats {
    if !include_preview {
        full.last_turns = None;
    }
    full
}

/// File mtime as epoch ms — the SAME derivation `jsonl::scan_all` uses (an
/// unavailable mtime → 0). Metadata only; never reads file content.
fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Atomically write `bytes` to `final_path` (which lives in `dir`) via a
/// process-unique tmp file + `fs::rename`. Mirrors `registry::atomic_write`
/// (§H.6): a same-directory rename is atomic on POSIX, so a concurrent reader
/// sees the old or the new COMPLETE file, never a torn one; the tmp name is
/// unique per (pid, monotonic counter) so two in-process writers never collide;
/// the tmp is best-effort removed on any error so a crash mid-write leaves no
/// litter.
fn atomic_write(dir: &Path, final_path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_NONCE: AtomicU64 = AtomicU64::new(0);

    std::fs::create_dir_all(dir)?;
    let nonce = TMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp_path = dir.join(format!(
        ".{}.tmp.{}.{}",
        CACHE_FILENAME,
        std::process::id(),
        nonce
    ));
    let write_res = (|| -> io::Result<()> {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(bytes)?;
        Ok(())
    })();
    if let Err(e) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp_path, final_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;

    // ---- fixtures ------------------------------------------------------------

    /// A minimal but rule-exercising claude transcript: one turn, occupancy, a
    /// user-named title, a branch/cwd, and a user + assistant preview.
    fn transcript(body_marker: &str) -> String {
        [
            format!(r#"{{"type":"user","cwd":"/w","gitBranch":"main","timestamp":"2026-06-04T10:00:00.000Z","message":{{"content":"hello {body_marker}"}}}}"#),
            r#"{"type":"custom-title","customTitle":"My Title","timestamp":"2026-06-04T10:00:01.000Z"}"#.to_string(),
            r#"{"type":"assistant","timestamp":"2026-06-04T10:00:02.000Z","message":{"content":[{"type":"text","text":"reply"}],"usage":{"input_tokens":1000,"cache_read_input_tokens":500,"cache_creation_input_tokens":200,"output_tokens":9}}}"#.to_string(),
            r#"{"type":"system","subtype":"turn_duration","timestamp":"2026-06-04T10:00:03.000Z"}"#.to_string(),
        ]
        .join("\n")
    }

    fn write(path: &Path, contents: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    /// A fixture fleet of N transcripts + a state dir, all under one tempdir.
    struct Fleet {
        _tmp: TempDir,
        state_dir: PathBuf,
        files: Vec<PathBuf>,
    }

    fn fleet(n: usize) -> Fleet {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        let mut files = Vec::new();
        for i in 0..n {
            let p = tmp.path().join("proj").join(format!("s{i}.jsonl"));
            write(&p, &transcript(&format!("f{i}")));
            files.push(p);
        }
        Fleet {
            _tmp: tmp,
            state_dir,
            files,
        }
    }

    /// A counting reader — the read-counter test-seam. Wraps the real
    /// `jsonl::read_stats` and counts every invocation (= every transcript-content
    /// read). The closure it hands out is what a warm call must NEVER invoke.
    #[derive(Clone)]
    struct CountingReader {
        count: Arc<AtomicUsize>,
    }
    impl CountingReader {
        fn new() -> Self {
            CountingReader {
                count: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn count(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }
        /// Read FULL stats (previews included), counting the content read.
        fn read(&self, path: &Path) -> JsonlStats {
            self.count.fetch_add(1, Ordering::SeqCst);
            crate::jsonl::read_stats(path, true)
        }
    }

    /// The status-aware analog of [`CountingReader`] (lsview A1 F1): returns FULL
    /// stats PLUS a chosen derived status, counting each invocation. Models the
    /// codex live-row reader (one `read_lines` pass → stats + `derive_status`).
    /// The `status` it hands back is what a real `derive_status` would return, so
    /// a test can make a warm reader return a DIFFERENT status than the cold one
    /// stored — a served value that matches the COLD status proves the cache, not
    /// the reader, produced it.
    #[derive(Clone)]
    struct CountingStatusReader {
        count: Arc<AtomicUsize>,
        status: Option<SessionStatus>,
    }
    impl CountingStatusReader {
        fn new(status: Option<SessionStatus>) -> Self {
            CountingStatusReader {
                count: Arc::new(AtomicUsize::new(0)),
                status,
            }
        }
        fn count(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }
        fn read(&self, path: &Path) -> (JsonlStats, Option<SessionStatus>) {
            self.count.fetch_add(1, Ordering::SeqCst);
            (crate::jsonl::read_stats(path, true), self.status)
        }
    }

    // ---- condition 6: a warm call does ZERO transcript-content reads ---------

    #[test]
    fn warm_call_does_zero_transcript_reads() {
        let f = fleet(5);
        let reader = CountingReader::new();

        // COLD pass: every file is read exactly once.
        let mut cache = StatsCache::load(&f.state_dir);
        assert!(cache.loaded_empty(), "no snapshot yet → rebuild");
        for path in &f.files {
            cache.get_or_read(path, false, |p| reader.read(p));
        }
        assert_eq!(reader.count(), 5, "cold: one read per file");
        assert_eq!(cache.transcript_reads(), 5);
        assert_eq!(cache.hits(), 0);
        cache.persist().unwrap();

        // WARM pass: a FRESH cache instance (proves it survives the process
        // boundary via the snapshot), same unchanged files.
        let reads_before = reader.count();
        let mut warm = StatsCache::load(&f.state_dir);
        assert!(!warm.loaded_empty(), "snapshot present → not a rebuild");
        for path in &f.files {
            warm.get_or_read(path, false, |p| reader.read(p));
        }
        // The headline assertion — two independent instruments agree.
        assert_eq!(
            reader.count(),
            reads_before,
            "warm pass invoked the reader ZERO times"
        );
        assert_eq!(warm.transcript_reads(), 0, "cache saw ZERO reads");
        assert_eq!(warm.hits(), 5, "every file was a warm hit");
    }

    #[test]
    fn warm_hit_serves_correct_stats() {
        let f = fleet(3);
        let reader = CountingReader::new();
        let mut cold = StatsCache::load(&f.state_dir);
        let cold_stats: Vec<JsonlStats> = f
            .files
            .iter()
            .map(|p| cold.get_or_read(p, true, |pp| reader.read(pp)))
            .collect();
        cold.persist().unwrap();

        let mut warm = StatsCache::load(&f.state_dir);
        for (i, path) in f.files.iter().enumerate() {
            let served = warm.get_or_read(path, true, |p| reader.read(p));
            // Warm-served == cold-computed == a fresh direct read.
            assert_eq!(served, cold_stats[i], "warm stats match cold");
            assert_eq!(
                served,
                crate::jsonl::read_stats(path, true),
                "warm stats match a fresh direct read"
            );
        }
        assert_eq!(warm.transcript_reads(), 0, "all warm hits");
    }

    // ---- lsview A1 F1: codex live-row STATUS served through the ONE cache -----

    #[test]
    fn warm_status_read_is_counted_then_served_warm() {
        // RS2.1 + RS1.2: a status-aware read is COUNTED at the same seam as a
        // stats read, its status is memoized in the same entry, and a warm pass
        // serves BOTH stats and status with ZERO content reads.
        let f = fleet(3);
        let reader = CountingStatusReader::new(Some(SessionStatus::Busy));

        // COLD: one content read per file (stats + status from ONE pass), counted.
        let mut cold = StatsCache::load(&f.state_dir);
        for p in &f.files {
            let (_stats, status) = cold.get_or_read_with_status(p, false, |pp| reader.read(pp));
            assert_eq!(status, Some(SessionStatus::Busy));
        }
        assert_eq!(reader.count(), 3, "cold: one read per file");
        assert_eq!(
            cold.transcript_reads(),
            3,
            "the counter observes the STATUS read (RS2.1) — not just stats"
        );
        cold.persist().unwrap();

        // WARM (fresh cache from the snapshot): a reader that would return a
        // DIFFERENT status if invoked. A served Busy proves the CACHE produced it;
        // a flat count proves the reader was never called.
        let warm_reader = CountingStatusReader::new(Some(SessionStatus::Idle));
        let mut warm = StatsCache::load(&f.state_dir);
        for p in &f.files {
            let (_stats, status) = warm.get_or_read_with_status(p, false, |pp| warm_reader.read(pp));
            assert_eq!(
                status,
                Some(SessionStatus::Busy),
                "warm serves the CACHED status, never the reader's Idle"
            );
        }
        assert_eq!(warm_reader.count(), 0, "warm invoked the reader ZERO times");
        assert_eq!(warm.transcript_reads(), 0, "cache saw ZERO reads — stats AND status");
        assert_eq!(warm.hits(), 3);
    }

    #[test]
    fn memoized_none_status_served_warm_as_none() {
        // RS1.3: a memoized derived-`None` (no anchors / unreadable tail) behaves
        // EXACTLY like a fresh `None` — served warm (a real HIT, not a miss), the
        // join's Idle fallback, never a re-read.
        let f = fleet(1);
        let p = &f.files[0];
        let reader = CountingStatusReader::new(None);
        let mut cold = StatsCache::load(&f.state_dir);
        let (_s, status) = cold.get_or_read_with_status(p, false, |pp| reader.read(pp));
        assert_eq!(status, None, "derive returned None");
        cold.persist().unwrap();

        // A warm reader that WOULD flip to Busy if re-invoked.
        let warm_reader = CountingStatusReader::new(Some(SessionStatus::Busy));
        let mut warm = StatsCache::load(&f.state_dir);
        let (_s, status) = warm.get_or_read_with_status(p, false, |pp| warm_reader.read(pp));
        assert_eq!(status, None, "memoized derived-None served warm, not re-derived to Busy");
        assert_eq!(warm_reader.count(), 0, "a derived-None is a real HIT, not a miss");
        assert_eq!(warm.transcript_reads(), 0);
        assert_eq!(warm.hits(), 1);
    }

    #[test]
    fn stats_only_entry_forces_a_status_rederive() {
        // RS1.3 cold→live corner: an entry cached by a STATS-ONLY read (the codex
        // cold-enrichment site) carries status `Absent`. A later status-aware read
        // over the UNCHANGED file MUST miss on status and re-derive (counted),
        // never serve a phantom the reader never produced. Then the UPGRADED entry
        // serves the derived status warm on the next pass.
        let f = fleet(1);
        let p = &f.files[0];

        // Stats-only cold read → an entry with status Absent.
        let stats_reader = CountingReader::new();
        let mut cold = StatsCache::load(&f.state_dir);
        cold.get_or_read(p, false, |pp| stats_reader.read(pp));
        assert_eq!(cold.transcript_reads(), 1);
        cold.persist().unwrap();

        // Status-aware read over the UNCHANGED file: the Absent status forces a
        // MISS and a re-derive — one counted read, no phantom.
        let status_reader = CountingStatusReader::new(Some(SessionStatus::Busy));
        let mut warm = StatsCache::load(&f.state_dir);
        let (_s, status) = warm.get_or_read_with_status(p, false, |pp| status_reader.read(pp));
        assert_eq!(status, Some(SessionStatus::Busy), "re-derived, not a phantom Idle/None");
        assert_eq!(status_reader.count(), 1, "an Absent-status entry forces a status re-derive");
        assert_eq!(warm.transcript_reads(), 1, "the re-derive IS counted");
        assert_eq!(warm.hits(), 0, "an Absent-status entry is a status MISS, not a hit");
        warm.persist().unwrap();

        // The entry now carries the derived status → a subsequent warm pass is a
        // pure hit (zero reads): the upgrade stuck across the snapshot boundary.
        let after = CountingStatusReader::new(Some(SessionStatus::Idle));
        let mut warm2 = StatsCache::load(&f.state_dir);
        let (_s, status2) = warm2.get_or_read_with_status(p, false, |pp| after.read(pp));
        assert_eq!(status2, Some(SessionStatus::Busy), "upgraded entry serves the derived status warm");
        assert_eq!(after.count(), 0);
        assert_eq!(warm2.transcript_reads(), 0);
        assert_eq!(warm2.hits(), 1);
    }

    #[test]
    fn pre_fix_v1_snapshot_rebuilds_never_phantom_status() {
        // RS1.6 / upgrade corner (i): a PRE-FIX (schema 1) snapshot has entries
        // with NO `status` field. The schema bump makes the whole snapshot rebuild,
        // so an absent status field can NEVER be read as `derived: none` and served
        // as a phantom status. If the bump were skipped and `#[serde(default)]`
        // silently loaded these as `Absent`, this would still not serve a phantom
        // (Absent → re-derive) — but the bump is the airtight defense proven here.
        let f = fleet(1);
        let p = &f.files[0];
        let cache_file = f.state_dir.join(CACHE_FILENAME);
        std::fs::create_dir_all(&f.state_dir).unwrap();

        // A syntactically valid v1 entry (pre-fix CacheEntry shape: no `status`),
        // keyed to the real file's (mtime, size) so it WOULD hit if it loaded.
        let meta = std::fs::metadata(p).unwrap();
        let size = meta.len();
        let mtime_ms = meta
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let v1 = format!(
            r#"{{"schema":1,"entries":{{"{}":{{"mtime_ms":{mtime_ms},"size":{size},"stats":{{"turns":9,"tokens":0,"user_named":false}}}}}}}}"#,
            p.to_string_lossy(),
        );
        std::fs::write(&cache_file, v1).unwrap();

        let reader = CountingStatusReader::new(Some(SessionStatus::Busy));
        let mut cache = StatsCache::load(&f.state_dir);
        assert!(cache.loaded_empty(), "a v1 snapshot rebuilds wholesale (schema bump)");
        let (stats, status) = cache.get_or_read_with_status(p, false, |pp| reader.read(pp));
        assert_eq!(stats.turns, 1, "fresh stats, not the stale v1 turns=9");
        assert_eq!(status, Some(SessionStatus::Busy), "freshly derived, never a phantom");
        assert_eq!(reader.count(), 1, "the rebuild re-read (the v1 entry never served)");
    }

    // ---- condition 1: append between calls refreshes -------------------------

    #[test]
    fn append_between_calls_refreshes_via_size() {
        // The SIZE axis of the invalidation key: an append grows the file, so the
        // key changes and stale content is never served — even if the mtime
        // resolution were too coarse to notice.
        let f = fleet(1);
        let path = &f.files[0];
        let reader = CountingReader::new();

        let mut cold = StatsCache::load(&f.state_dir);
        let before = cold.get_or_read(path, false, |p| reader.read(p));
        assert_eq!(before.turns, 1);
        cold.persist().unwrap();

        // Append a second turn (size grows).
        let mut appended = std::fs::read_to_string(path).unwrap();
        appended.push_str(
            "\n{\"type\":\"system\",\"subtype\":\"turn_duration\",\"timestamp\":\"2026-06-04T10:00:09.000Z\"}",
        );
        std::fs::write(path, &appended).unwrap();

        let mut warm = StatsCache::load(&f.state_dir);
        let reads_before = reader.count();
        let after = warm.get_or_read(path, false, |p| reader.read(p));
        assert_eq!(reader.count(), reads_before + 1, "append → cache MISS → re-read");
        assert_eq!(after.turns, 2, "refreshed stats reflect the append");
        assert_eq!(warm.hits(), 0);
    }

    #[test]
    fn same_size_newer_mtime_refreshes_via_mtime() {
        // The MTIME axis of the invalidation key: a same-SIZE content change with
        // a newer mtime is still a miss. (A same-size, same-mtime rewrite is the
        // ACCEPTED residual and is deliberately NOT tested as detectable.)
        let f = fleet(1);
        let path = &f.files[0];
        let reader = CountingReader::new();

        let mut cold = StatsCache::load(&f.state_dir);
        cold.get_or_read(path, false, |p| reader.read(p));
        cold.persist().unwrap();

        // Rewrite with a DIFFERENT same-length body ("f0" → "XY"), then push the
        // mtime strictly forward so the key's mtime component changes.
        let original = std::fs::read_to_string(path).unwrap();
        let rewritten = original.replacen("hello f0", "hello XY", 1);
        assert_eq!(rewritten.len(), original.len(), "same size on purpose");
        let old_mtime = std::fs::metadata(path).unwrap().modified().unwrap();
        std::fs::write(path, &rewritten).unwrap();
        let newer = old_mtime + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(newer)
            .unwrap();

        let mut warm = StatsCache::load(&f.state_dir);
        let reads_before = reader.count();
        warm.get_or_read(path, false, |p| reader.read(p));
        assert_eq!(
            reader.count(),
            reads_before + 1,
            "same-size newer-mtime → cache MISS → re-read"
        );
        assert_eq!(warm.hits(), 0);
    }

    // ---- condition 5: include_preview correct in BOTH orders -----------------

    #[test]
    fn preview_then_no_preview_and_vice_versa() {
        // Order A: cold WITHOUT previews, warm WITH previews.
        {
            let f = fleet(2);
            let reader = CountingReader::new();
            let mut cold = StatsCache::load(&f.state_dir);
            for p in &f.files {
                let s = cold.get_or_read(p, false, |pp| reader.read(pp));
                assert!(s.last_turns.is_none(), "no-preview request → no previews");
            }
            cold.persist().unwrap();

            let mut warm = StatsCache::load(&f.state_dir);
            let reads_before = reader.count();
            for p in &f.files {
                let s = warm.get_or_read(p, true, |pp| reader.read(pp));
                // The store always has previews → a preview request is served warm.
                assert_eq!(
                    s.last_turns,
                    crate::jsonl::read_stats(p, true).last_turns,
                    "warm preview request returns correct previews"
                );
                assert!(s.last_turns.is_some());
            }
            assert_eq!(reader.count(), reads_before, "served previews with ZERO reads");
        }
        // Order B: cold WITH previews, warm WITHOUT previews.
        {
            let f = fleet(2);
            let reader = CountingReader::new();
            let mut cold = StatsCache::load(&f.state_dir);
            for p in &f.files {
                cold.get_or_read(p, true, |pp| reader.read(pp));
            }
            cold.persist().unwrap();

            let mut warm = StatsCache::load(&f.state_dir);
            let reads_before = reader.count();
            for p in &f.files {
                let s = warm.get_or_read(p, false, |pp| reader.read(pp));
                assert!(s.last_turns.is_none(), "no-preview request → previews nulled");
                // Non-preview fields still correct.
                assert_eq!(s, crate::jsonl::read_stats(p, false));
            }
            assert_eq!(reader.count(), reads_before, "served subset with ZERO reads");
        }
    }

    // ---- condition 3: missing / corrupt / old-schema → silent rebuild --------

    #[test]
    fn missing_snapshot_rebuilds() {
        let f = fleet(3);
        let reader = CountingReader::new();
        let mut cache = StatsCache::load(&f.state_dir); // no file yet
        assert!(cache.loaded_empty());
        for p in &f.files {
            cache.get_or_read(p, false, |pp| reader.read(pp));
        }
        assert_eq!(cache.transcript_reads(), 3, "cold rebuild reads all");
    }

    #[test]
    fn corrupt_snapshot_rebuilds_silently() {
        let f = fleet(3);
        let reader = CountingReader::new();
        // Seed a valid snapshot first.
        {
            let mut c = StatsCache::load(&f.state_dir);
            for p in &f.files {
                c.get_or_read(p, false, |pp| reader.read(pp));
            }
            c.persist().unwrap();
        }
        let cache_file = f.state_dir.join(CACHE_FILENAME);
        assert!(cache_file.exists());

        for garbage in [
            &b"not json at all"[..],
            &b"{\"schema\":1,\"entries\":{ truncated"[..], // torn write
            &b"\x00\x01\x02\x03"[..],                       // binary garbage
            &b""[..],                                        // empty
        ] {
            std::fs::write(&cache_file, garbage).unwrap();
            let reader2 = CountingReader::new();
            let mut cache = StatsCache::load(&f.state_dir);
            assert!(cache.loaded_empty(), "corrupt file → rebuild flagged");
            for p in &f.files {
                let s = cache.get_or_read(p, false, |pp| reader2.read(pp));
                // The rebuilt rows are correct, never wrong, never an error.
                assert_eq!(s, crate::jsonl::read_stats(p, false));
            }
            assert_eq!(reader2.count(), 3, "every file re-read after corruption");
        }
    }

    #[test]
    fn old_schema_snapshot_rebuilds() {
        let f = fleet(2);
        let cache_file = f.state_dir.join(CACHE_FILENAME);
        // A syntactically valid snapshot but from a FUTURE/OTHER schema version.
        std::fs::create_dir_all(&f.state_dir).unwrap();
        std::fs::write(
            &cache_file,
            br#"{"schema":9999,"entries":{"/some/path.jsonl":{"mtime_ms":1,"size":2,"stats":{"turns":7,"tokens":0,"user_named":false}}}}"#,
        )
        .unwrap();

        let reader = CountingReader::new();
        let mut cache = StatsCache::load(&f.state_dir);
        assert!(cache.loaded_empty(), "schema mismatch → rebuild");
        for p in &f.files {
            let s = cache.get_or_read(p, false, |pp| reader.read(pp));
            assert_eq!(s.turns, 1, "correct fresh rows, not the stale schema-9999 turns=7");
        }
        assert_eq!(reader.count(), 2);
    }

    // ---- condition 2: delete / rotate never errors ---------------------------

    #[test]
    fn delete_never_errors() {
        let f = fleet(2);
        let reader = CountingReader::new();
        let mut cold = StatsCache::load(&f.state_dir);
        for p in &f.files {
            cold.get_or_read(p, false, |pp| reader.read(pp));
        }
        cold.persist().unwrap();

        // Delete one transcript out from under the cache.
        std::fs::remove_file(&f.files[0]).unwrap();

        let mut warm = StatsCache::load(&f.state_dir);
        // The deleted file: no panic, no error, zeroed-default stats (as today).
        let gone = warm.get_or_read(&f.files[0], false, |pp| reader.read(pp));
        assert_eq!(gone, JsonlStats::default(), "deleted → zeroed default, no error");
        // The survivor still serves warm.
        let alive = warm.get_or_read(&f.files[1], false, |pp| reader.read(pp));
        assert_eq!(alive.turns, 1);
        // Persist prunes the deleted entry (touched set no longer includes it).
        warm.persist().unwrap();
        let reloaded = StatsCache::load(&f.state_dir);
        assert!(
            !reloaded.entries.contains_key(&f.files[0]),
            "deleted transcript pruned from the snapshot"
        );
    }

    // ---- condition 4: concurrency-safe (atomic write, last-writer-wins) ------

    #[test]
    fn concurrent_persist_never_corrupts() {
        let f = fleet(4);
        // Seed once so both writers have entries.
        {
            let reader = CountingReader::new();
            let mut c = StatsCache::load(&f.state_dir);
            for p in &f.files {
                c.get_or_read(p, false, |pp| reader.read(pp));
            }
            c.persist().unwrap();
        }

        // Many concurrent load→read→persist cycles (the two-concurrent-`qd ls`
        // shape, amplified). None may corrupt the file or panic.
        let state_dir = f.state_dir.clone();
        let files = f.files.clone();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let state_dir = state_dir.clone();
                let files = files.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        let reader = CountingReader::new();
                        let mut cache = StatsCache::load(&state_dir);
                        // A concurrent reader NEVER errors and NEVER sees a torn
                        // snapshot: load always yields a usable cache.
                        for p in &files {
                            cache.get_or_read(p, false, |pp| reader.read(pp));
                        }
                        // Force a write every cycle to maximize rename contention.
                        cache.dirty = true;
                        cache.persist().unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        // The final snapshot is intact and usable — a full warm hit fleet.
        let reader = CountingReader::new();
        let mut final_cache = StatsCache::load(&state_dir);
        assert!(!final_cache.loaded_empty(), "snapshot survived the storm");
        for p in &files {
            final_cache.get_or_read(p, false, |pp| reader.read(pp));
        }
        assert_eq!(reader.count(), 0, "final snapshot serves every row warm");
        assert_eq!(final_cache.hits(), 4);
    }

    // ---- pure-hit run skips the write ----------------------------------------

    #[test]
    fn pure_hit_run_is_not_dirty() {
        let f = fleet(2);
        let reader = CountingReader::new();
        {
            let mut c = StatsCache::load(&f.state_dir);
            for p in &f.files {
                c.get_or_read(p, false, |pp| reader.read(pp));
            }
            c.persist().unwrap();
        }
        let mut warm = StatsCache::load(&f.state_dir);
        for p in &f.files {
            warm.get_or_read(p, false, |pp| reader.read(pp));
        }
        assert!(!warm.dirty, "an all-hit run stays clean → persist is a no-op");
    }
}
