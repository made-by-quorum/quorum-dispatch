//! pi session-transcript JSONL reading + path math (mirrors
//! [`crate::provider::codex::rollout`]) — permissive (L8) and, crucially,
//! **tolerant of the lazy-write window**.
//!
//! **Lazy-write law (session-manager.js:638-667).** pi's `_persist()` defers
//! ALL disk writes until the first ASSISTANT message:
//! `fileEntries.some(e => e.type === "message" && e.message.role ===
//! "assistant")`. Until then the `<ts>_<uuid>.jsonl` file does **not exist** —
//! only the in-memory buffer holds the header + any user/system entries. The
//! *directory* (`--<enc-cwd>--/`) IS created eagerly at construction; the FILE
//! appears only on the first assistant reply, written atomically with
//! `openSync(path,"wx")`.
//!
//! Consequence for this module: **"no file" ≠ "no session".** A missing path is
//! a pre-first-turn session, never a lost one — every reader here degrades a
//! missing/empty/torn file to "nothing yet", never an error.
//!
//! **On-disk shape.** One JSON object per line: line 1 is the
//! [`SessionHeader`] (`type:"session"`), the rest are [`SessionEntry`] lines
//! (`type:"message"|"compaction"|...`), each carrying a `parentId` tree link
//! (`null` = root). `CURRENT_SESSION_VERSION = 3` (a SESSION-file version,
//! unrelated to the RPC wire, which is unversioned).

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The first line of a session file (session-manager.d.ts:5-12). Permissive.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: Option<u64>,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
}

/// A non-header session line (session-manager.d.ts:17-101). We read the shared
/// base (`type`/`id`/`parentId`/`timestamp`); the per-kind payload is left
/// unread (permissive). Variants on the wire: `message`, `thinking_level_change`,
/// `model_change`, `compaction`, `branch_summary`, `custom`, `custom_message`,
/// `label`, `session_info`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionEntry {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    pub timestamp: String,
}

/// One parsed file line — a [`SessionHeader`], a [`SessionEntry`], or a line we
/// couldn't classify (kept so a single bad line never discards the good ones).
#[derive(Debug, Clone)]
pub enum FileLine {
    Header(SessionHeader),
    Entry(SessionEntry),
    /// An unparseable / torn line (skipped by readers; retained for counts).
    Unknown,
}

/// Read every line of a session file into [`FileLine`]s. **Lazy-write tolerant:**
/// a missing or unreadable file is `Vec::new()` (a pre-first-turn session), NOT
/// an error. A torn/garbage line degrades to [`FileLine::Unknown`] and is
/// skipped — a partial trailing line never discards good records (L8).
pub fn read_lines(path: &Path) -> Vec<FileLine> {
    let Ok(text) = std::fs::read_to_string(path) else {
        // No file yet = lazy-write window = nothing to read, NOT a failure.
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            out.push(FileLine::Unknown);
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("session") => match serde_json::from_value(v) {
                Ok(h) => out.push(FileLine::Header(h)),
                Err(_) => out.push(FileLine::Unknown),
            },
            Some(_) => match serde_json::from_value(v) {
                Ok(e) => out.push(FileLine::Entry(e)),
                Err(_) => out.push(FileLine::Unknown),
            },
            None => out.push(FileLine::Unknown),
        }
    }
    out
}

/// A parsed session filename: `<fileTimestamp>_<sessionId>.jsonl` where
/// `fileTimestamp = timestamp.replace(/[:.]/g,"-")` (session-manager.js:580) —
/// so it is NOT a raw ISO string (colons + the dot become dashes). The
/// `session_id` is the trailing UUID (UUIDs carry no `_`, so the LAST `_` splits
/// timestamp from id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiSessionName {
    pub file_timestamp: String,
    pub session_id: String,
}

/// Parse `<ts>_<uuid>.jsonl` → ([`PiSessionName`]). `None` for any name that is
/// not a `*.jsonl` with a `_`-separated id (permissive: a non-session file is a
/// skip, never a crash).
pub fn parse_filename(name: &str) -> Option<PiSessionName> {
    let stem = name.strip_suffix(".jsonl")?;
    // Split on the LAST '_': the UUID has no underscores, the ts has dashes.
    let (ts, id) = stem.rsplit_once('_')?;
    if ts.is_empty() || id.is_empty() {
        return None;
    }
    Some(PiSessionName {
        file_timestamp: ts.to_string(),
        session_id: id.to_string(),
    })
}

/// Encode a cwd into pi's session sub-dir name (session-manager.js:221-226):
/// `--${cwd.replace(/^[/\\]/,"").replace(/[/\\:]/g,"-")}--`. The leading
/// separator is stripped, then every `/ \ :` becomes `-`, wrapped in `-- --`.
/// e.g. `/home/pete/x` → `--home-pete-x--`.
pub fn encode_cwd_dir(cwd: &str) -> String {
    // Strip a single leading '/' or '\\'.
    let body = cwd
        .strip_prefix('/')
        .or_else(|| cwd.strip_prefix('\\'))
        .unwrap_or(cwd);
    let mapped: String = body
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == ':' { '-' } else { c })
        .collect();
    format!("--{mapped}--")
}

/// Locate a session file by id under the sessions root, **lazy-write tolerant**.
/// Scans `<root>/--<enc-cwd>--/<ts>_<uuid>.jsonl`; if `cwd` is known we look only
/// in that encoded dir, else we scan every `--*--` dir. Returns `None` when no
/// matching file exists YET (the pre-first-turn window) — the caller must treat
/// that as "session not yet flushed", not "no session".
pub fn find_session_file(root: &Path, id: &str, cwd: Option<&str>) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = match cwd {
        Some(c) => vec![root.join(encode_cwd_dir(c))],
        None => std::fs::read_dir(root)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
    };
    for dir in dirs {
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for f in files.flatten() {
            let fname = f.file_name().to_string_lossy().into_owned();
            if let Some(parsed) = parse_filename(&fname) {
                if parsed.session_id == id {
                    return Some(f.path());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reads_as_empty_not_error() {
        // The lazy-write window: the file does not exist yet.
        let lines = read_lines(Path::new("/no/such/pi/session.jsonl"));
        assert!(lines.is_empty());
    }

    #[test]
    fn parses_header_then_entries_skipping_a_torn_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"id\":\"u1\",\"timestamp\":\"t\",\"cwd\":\"/w\",\"version\":3}\n\
             {\"type\":\"message\",\"id\":\"e1\",\"parentId\":null,\"timestamp\":\"t2\"}\n\
             {torn json\n\
             {\"type\":\"compaction\",\"id\":\"e2\",\"parentId\":\"e1\",\"timestamp\":\"t3\"}\n",
        )
        .unwrap();
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 4);
        assert!(matches!(&lines[0], FileLine::Header(h) if h.id == "u1" && h.version == Some(3)));
        assert!(matches!(&lines[1], FileLine::Entry(e) if e.kind == "message" && e.parent_id.is_none()));
        assert!(matches!(&lines[2], FileLine::Unknown));
        assert!(matches!(&lines[3], FileLine::Entry(e) if e.parent_id.as_deref() == Some("e1")));
    }

    #[test]
    fn filename_splits_on_the_last_underscore() {
        // ts has dashes (from the :/. replacement), the uuid is the trailing id.
        let p = parse_filename("2026-06-29T14-03-22-491Z_019e9f4b-adb9-7ec1-b4ed-08247847426a.jsonl")
            .unwrap();
        assert_eq!(p.session_id, "019e9f4b-adb9-7ec1-b4ed-08247847426a");
        assert_eq!(p.file_timestamp, "2026-06-29T14-03-22-491Z");
        assert!(parse_filename("not-a-session.txt").is_none());
        assert!(parse_filename("noseparator.jsonl").is_none());
    }

    #[test]
    fn cwd_dir_encoding_matches_pi() {
        assert_eq!(encode_cwd_dir("/home/pete/x"), "--home-pete-x--");
        // Windows-ish: `:` AND `\` each map to a dash (pi's regex replaces every
        // [/\\:] independently, no collapsing), so the adjacent `C:\` → `C--`.
        assert_eq!(encode_cwd_dir("C:\\proj\\a"), "--C--proj-a--");
    }

    #[test]
    fn find_returns_none_in_the_lazy_write_window() {
        let root = tempfile::tempdir().unwrap();
        // Dir exists (created eagerly) but no <ts>_<uuid>.jsonl yet.
        std::fs::create_dir_all(root.path().join(encode_cwd_dir("/w"))).unwrap();
        assert!(find_session_file(root.path(), "u1", Some("/w")).is_none());
    }

    #[test]
    fn find_locates_a_flushed_file_by_id() {
        let root = tempfile::tempdir().unwrap();
        let d = root.path().join(encode_cwd_dir("/w"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("2026-06-29T00-00-00-000Z_uuid-9.jsonl"), "{}").unwrap();
        let found = find_session_file(root.path(), "uuid-9", Some("/w")).unwrap();
        assert!(found.ends_with("2026-06-29T00-00-00-000Z_uuid-9.jsonl"));
        // Unknown cwd → scan-all still finds it.
        assert!(find_session_file(root.path(), "uuid-9", None).is_some());
    }
}
