//! M3: JSONL stats/scan, ported from TS src/session.ts:431-605.
//!
//! All functions take injected **root paths** (`&Path`) — NOTHING here resolves
//! the real home (lesson L9a; see [`crate::paths::QdPaths`]). Permissive parse
//! throughout (L8): a bad JSON line is skipped, never fatal; a partial trailing
//! line must not discard the good records already accumulated.

use std::fs;
use std::path::{Path, PathBuf};

use crate::model::TurnPreview;

/// `cwdToProjectPath` (src/session.ts:433-435): replace ALL `/` with `-`.
/// Claude Code's projects-dir layout slugifies the cwd this way.
pub fn cwd_to_project_path(cwd: &str) -> String {
    cwd.replace('/', "-")
}

/// `findJsonlPath` (src/session.ts:437-461). Two tiers:
///   1. If `cwd` is known, derive the project dir from it and stat that exact
///      `<projects_dir>/<cwd-slug>/<session_id>.jsonl` first.
///   2. Fallback: scan every project dir for a `<session_id>.jsonl`.
///
/// Stat failures fall through silently (TS `try { await stat } catch {}`).
pub fn find_jsonl_path(
    projects_dir: &Path,
    session_id: &str,
    cwd: Option<&str>,
) -> Option<PathBuf> {
    if let Some(cwd) = cwd {
        let project_path = cwd_to_project_path(cwd);
        let p = projects_dir
            .join(project_path)
            .join(format!("{session_id}.jsonl"));
        if fs::metadata(&p).is_ok() {
            return Some(p);
        }
    }
    // Fallback: search all project dirs (TS readdir(PROJECTS_DIR)).
    if let Ok(entries) = fs::read_dir(projects_dir) {
        for entry in entries.flatten() {
            let p = entry.path().join(format!("{session_id}.jsonl"));
            if fs::metadata(&p).is_ok() {
                return Some(p);
            }
        }
    }
    None
}

/// Stats extracted from a single transcript (TS `JsonlStats`, src/session.ts:77-86).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JsonlStats {
    pub turns: u64,
    pub tokens: u64,
    pub name: Option<String>,
    pub user_named: bool,
    pub last_timestamp: Option<String>,
    pub git_branch: Option<String>,
    pub cwd: Option<String>,
    pub last_turns: Option<Vec<TurnPreview>>,
}

/// `getJsonlStats` (src/session.ts:463-568). Per-line JSON parse; a bad line is
/// skipped (TS `try { JSON.parse } catch {}`) so a partial trailing line never
/// discards good records (L8).
///
/// Token count = CONTEXT OCCUPANCY (Pete feedback #5): how full the context
/// window is RIGHT NOW, not a lifetime-cumulative estimate. The claude transcript
/// has no canonical total, so we DERIVE it from the LAST `assistant` line that
/// carries a `message.usage`: `input_tokens + cache_read_input_tokens +
/// cache_creation_input_tokens` = the context that was re-sent on that turn =
/// the current window fill. This DROPS after a compaction (the re-sent context
/// shrinks), which is exactly the actionable "how full am I" semantic. We do NOT
/// sum `output_tokens` (lifetime-generated) and do NOT sum `cache_read` across
/// turns (that re-counts the re-sent context every turn and explodes to
/// millions). Permissive (L8): old transcripts lacking `usage` → 0.
pub fn read_stats(jsonl_path: &Path, include_preview: bool) -> JsonlStats {
    let mut stats = JsonlStats::default();
    // TS: try { content = readFile } catch {} → an unreadable file yields the
    // zeroed default (turns:0, tokens:0).
    let Ok(content) = fs::read_to_string(jsonl_path) else {
        return stats;
    };

    let mut recent_user_texts: Vec<TurnPreview> = Vec::new();
    let mut recent_assistant_texts: Vec<TurnPreview> = Vec::new();
    // Last-wins: the occupancy from the most recent assistant turn that has usage.
    let mut last_usage_tokens: Option<u64> = None;

    // TS splits on "\n"; a partial trailing line is just another line that may
    // fail to parse and gets skipped — the good records above it survive (L8).
    for line in content.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue; // TS catch {} — skip a bad line, keep going.
        };

        let obj_type = obj.get("type").and_then(|v| v.as_str());

        // turns++ on a turn_duration system record.
        if obj_type == Some("system")
            && obj.get("subtype").and_then(|v| v.as_str()) == Some("turn_duration")
        {
            stats.turns += 1;
        }

        // Context occupancy (Pete #5): LAST assistant turn's re-sent context.
        // `message.usage` carries `{input_tokens, cache_creation_input_tokens,
        // cache_read_input_tokens, output_tokens}`; the window fill on that turn is
        // input + cache_read + cache_creation (NOT output, NOT a cross-turn sum).
        // Last-wins so we end on the most recent usable turn. Runs regardless of
        // `include_preview` (occupancy is always rendered). L8: missing fields → 0.
        if obj_type == Some("assistant") {
            if let Some(usage) = obj.get("message").and_then(|m| m.get("usage")) {
                let f = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                let occ = f("input_tokens")
                    + f("cache_read_input_tokens")
                    + f("cache_creation_input_tokens");
                last_usage_tokens = Some(occ);
            }
        }

        // Name precedence: agent-name / custom-title set userNamed=true; ai-title
        // only fills in when NOT already user-named (and never sets userNamed).
        if obj_type == Some("agent-name") {
            if let Some(n) = obj.get("agentName").and_then(|v| v.as_str()) {
                if !n.is_empty() {
                    stats.name = Some(n.to_string());
                    stats.user_named = true;
                }
            }
        } else if obj_type == Some("custom-title") {
            if let Some(n) = obj.get("customTitle").and_then(|v| v.as_str()) {
                if !n.is_empty() {
                    stats.name = Some(n.to_string());
                    stats.user_named = true;
                }
            }
        } else if obj_type == Some("ai-title") && !stats.user_named {
            if let Some(n) = obj.get("aiTitle").and_then(|v| v.as_str()) {
                if !n.is_empty() {
                    stats.name = Some(n.to_string());
                }
            }
        }

        // lastTimestamp = last seen obj.timestamp on ANY record type.
        if let Some(ts) = obj.get("timestamp").and_then(|v| v.as_str()) {
            if !ts.is_empty() {
                stats.last_timestamp = Some(ts.to_string());
            }
        }

        // gitBranch: LAST user record with a gitBranch wins.
        if obj_type == Some("user") {
            if let Some(b) = obj.get("gitBranch").and_then(|v| v.as_str()) {
                if !b.is_empty() {
                    stats.git_branch = Some(b.to_string());
                }
            }
            // cwd: FIRST user record with a cwd wins (TS `&& !stats.cwd` guard).
            if stats.cwd.is_none() {
                if let Some(c) = obj.get("cwd").and_then(|v| v.as_str()) {
                    if !c.is_empty() {
                        stats.cwd = Some(c.to_string());
                    }
                }
            }
        }

        if include_preview {
            // User preview: string content, or the FIRST text block in an array.
            if obj_type == Some("user") {
                if let Some(content) = obj.get("message").and_then(|m| m.get("content")) {
                    let text: Option<&str> = if let Some(s) = content.as_str() {
                        Some(s)
                    } else if let Some(arr) = content.as_array() {
                        arr.iter()
                            .find(|b| {
                                b.get("type").and_then(|v| v.as_str()) == Some("text")
                                    && b.get("text")
                                        .and_then(|v| v.as_str())
                                        .is_some_and(|t| !t.is_empty())
                            })
                            .and_then(|b| b.get("text").and_then(|v| v.as_str()))
                    } else {
                        None
                    };
                    if let Some(text) = text {
                        if !text.is_empty() {
                            recent_user_texts.push(TurnPreview {
                                role: "user",
                                text: slice_200(text),
                                timestamp: obj
                                    .get("timestamp")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_string),
                            });
                        }
                    }
                }
            }
            // Assistant content preview: EACH text block separately.
            if obj_type == Some("assistant") {
                if let Some(content) = obj.get("message").and_then(|m| m.get("content")) {
                    // TS: Array.isArray ? content : [content].
                    let blocks: Vec<&serde_json::Value> = match content.as_array() {
                        Some(arr) => arr.iter().collect(),
                        None => vec![content],
                    };
                    for b in blocks {
                        if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                            if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                if !t.is_empty() {
                                    recent_assistant_texts.push(TurnPreview {
                                        role: "assistant",
                                        text: slice_200(t),
                                        timestamp: obj
                                            .get("timestamp")
                                            .and_then(|v| v.as_str())
                                            .map(str::to_string),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if include_preview {
        // Merge user+assistant, stable-sort by timestamp ascending, take LAST 6.
        // TS: allTurns.sort((a,b) => new Date(a.timestamp||0).getTime() -
        //     new Date(b.timestamp||0).getTime()).slice(-6).
        // Missing timestamp → epoch 0 (TS `new Date(undefined||0)`). DIVERGENCE
        // NOTE: an *invalid* (non-empty but unparseable) timestamp string yields
        // NaN in TS, which makes every sort comparison false → an unstable,
        // engine-defined order. Fixtures use valid RFC3339 timestamps so this is
        // not exercised; we parse what we can and treat unparseable as epoch 0.
        // Rust's sort_by_key is STABLE, matching TS Array.sort's spec'd stability.
        let mut all_turns: Vec<TurnPreview> = recent_user_texts;
        all_turns.extend(recent_assistant_texts);
        all_turns.sort_by_key(|t| t.timestamp.as_deref().map(parse_ts_ms).unwrap_or(0));
        let n = all_turns.len();
        let start = n.saturating_sub(6);
        stats.last_turns = Some(all_turns.split_off(start));
    }

    // tokens = current context occupancy (see fn doc): the last assistant turn's
    // re-sent context. No usage on any line (old transcript) → 0 (L8 degrade).
    stats.tokens = last_usage_tokens.unwrap_or(0);

    stats
}

/// Truncate to the first 200 chars. DIVERGENCE NOTE: TS `String.slice(0, 200)`
/// counts UTF-16 code units; we count `char`s (Unicode scalar values). The two
/// differ only for non-ASCII text, which is rare in the frozen fixtures; the
/// fixtures freeze the expected output (fixture-frozen detail).
fn slice_200(s: &str) -> String {
    s.chars().take(200).collect()
}

/// Parse a timestamp to epoch ms for stable sorting. Best-effort: handles the
/// RFC3339 / ISO-8601 forms the fixtures use. Anything unparseable → 0 (matches
/// TS's epoch-0 floor for the missing case; see the divergence note above).
fn parse_ts_ms(ts: &str) -> i64 {
    if ts.is_empty() {
        return 0;
    }
    parse_iso8601_utc_ms(ts).unwrap_or(0)
}

/// Parse "YYYY-MM-DDTHH:MM:SS[.fff][Z]" to epoch milliseconds (UTC). Returns
/// None on any shape we don't recognise. Only used for preview ORDERING, so
/// exactness beyond the fixtures is unnecessary (no chrono dependency).
pub(crate) fn parse_iso8601_utc_ms(ts: &str) -> Option<i64> {
    let bytes = ts.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let num = |a: usize, b: usize| -> Option<i64> { ts.get(a..b)?.parse::<i64>().ok() };
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let min = num(14, 16)?;
    let sec = num(17, 19)?;
    // Optional ".fff" fractional seconds.
    let mut millis = 0i64;
    let rest = &ts[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            // First 3 digits = milliseconds (pad/truncate).
            let mut ms_str = digits;
            ms_str.truncate(3);
            while ms_str.len() < 3 {
                ms_str.push('0');
            }
            millis = ms_str.parse().ok()?;
        }
    }
    let days = days_from_civil(year, month, day)?;
    let secs = days * 86_400 + hour * 3600 + min * 60 + sec;
    Some(secs * 1000 + millis)
}

/// Days since 1970-01-01 for a civil (proleptic Gregorian) date. Howard
/// Hinnant's algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// One transcript file's metadata (TS shape from `scanAllJsonlFiles`,
/// src/session.ts:572-580).
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptMeta {
    pub session_id: String,
    pub path: PathBuf,
    /// File mtime in epoch ms (TS keeps a `Date`).
    pub mtime_ms: i64,
    /// The project dir name (slug) the transcript lives in.
    pub project_dir: String,
}

/// `scanAllJsonlFiles` (src/session.ts:570-605). Every `<dir>/<x>.jsonl` under
/// `projects_dir`; stat failures are skipped (TS per-file `try { stat } catch {}`).
pub fn scan_all(projects_dir: &Path) -> Vec<TranscriptMeta> {
    let mut results = Vec::new();
    let Ok(dirs) = fs::read_dir(projects_dir) else {
        return results;
    };
    for dir_entry in dirs.flatten() {
        let dir_path = dir_entry.path();
        let project_dir = dir_entry.file_name().to_string_lossy().into_owned();
        let Ok(files) = fs::read_dir(&dir_path) else {
            continue;
        };
        for file_entry in files.flatten() {
            let fname = file_entry.file_name().to_string_lossy().into_owned();
            if !fname.ends_with(".jsonl") {
                continue;
            }
            let session_id = fname.trim_end_matches(".jsonl").to_string();
            let full_path = file_entry.path();
            let Ok(meta) = fs::metadata(&full_path) else {
                continue; // stat failure → skip (TS catch {}).
            };
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            results.push(TranscriptMeta {
                session_id,
                path: full_path,
                mtime_ms,
                project_dir: project_dir.clone(),
            });
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn cwd_slug_replaces_all_slashes() {
        assert_eq!(
            cwd_to_project_path("/home/u/work/qd-rust"),
            "-home-u-work-qd-rust"
        );
    }

    #[test]
    fn read_stats_exercises_every_rule() {
        // A synthetic transcript hitting: turn counting, name precedence
        // (ai-title must NOT override an earlier user-named name), first-cwd /
        // last-branch, preview merge + last-6, partial trailing line skip.
        let lines = [
            // user record sets cwd (FIRST) + gitBranch + a preview text.
            r#"{"type":"user","cwd":"/a/first","gitBranch":"main","timestamp":"2026-06-04T10:00:00.000Z","message":{"content":"hello one"}}"#,
            // turn_duration → turns++
            r#"{"type":"system","subtype":"turn_duration","timestamp":"2026-06-04T10:00:01.000Z"}"#,
            // user-named via custom-title.
            r#"{"type":"custom-title","customTitle":"My Title","timestamp":"2026-06-04T10:00:02.000Z"}"#,
            // ai-title must NOT override the user-named title.
            r#"{"type":"ai-title","aiTitle":"Robot Title","timestamp":"2026-06-04T10:00:03.000Z"}"#,
            // assistant with array content, two text blocks → two previews; usage
            // → occupancy = input+cache_read+cache_creation (NOT output).
            r#"{"type":"assistant","timestamp":"2026-06-04T10:00:04.000Z","message":{"content":[{"type":"text","text":"a-one"},{"type":"text","text":"a-two"}],"usage":{"input_tokens":1000,"cache_read_input_tokens":500,"cache_creation_input_tokens":200,"output_tokens":99}}}"#,
            // second user record: cwd IGNORED (first wins), branch UPDATED (last wins), preview text.
            r#"{"type":"user","cwd":"/a/second","gitBranch":"feature","timestamp":"2026-06-04T10:00:05.000Z","message":{"content":[{"type":"text","text":"hello two"}]}}"#,
            // another turn_duration → turns = 2
            r#"{"type":"system","subtype":"turn_duration","timestamp":"2026-06-04T10:00:06.000Z"}"#,
            // user records to push preview past 6 entries.
            r#"{"type":"user","timestamp":"2026-06-04T10:00:07.000Z","message":{"content":"u3"}}"#,
            r#"{"type":"user","timestamp":"2026-06-04T10:00:08.000Z","message":{"content":"u4"}}"#,
            r#"{"type":"user","timestamp":"2026-06-04T10:00:09.000Z","message":{"content":"u5"}}"#,
            r#"{"type":"user","timestamp":"2026-06-04T10:00:10.000Z","message":{"content":"u6"}}"#,
            r#"{"type":"user","timestamp":"2026-06-04T10:00:11.000Z","message":{"content":"u7"}}"#,
            // a bad line in the middle is skipped.
            r#"{ this is not valid json"#,
        ];
        // Partial trailing line (no newline, truncated) must be skipped without
        // discarding the good records above it.
        let content = format!("{}\n{{\"type\":\"user\",\"cwd\"", lines.join("\n"));

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.jsonl");
        write_file(&path, &content);

        let stats = read_stats(&path, true);
        assert_eq!(stats.turns, 2, "two turn_duration records");
        assert_eq!(
            stats.name.as_deref(),
            Some("My Title"),
            "ai-title must not override user-named"
        );
        assert!(stats.user_named);
        assert_eq!(stats.cwd.as_deref(), Some("/a/first"), "first cwd wins");
        assert_eq!(
            stats.git_branch.as_deref(),
            Some("feature"),
            "last branch wins"
        );
        assert_eq!(
            stats.last_timestamp.as_deref(),
            Some("2026-06-04T10:00:11.000Z"),
            "last timestamp on any record"
        );

        // Preview merge + last-6: collected previews in timestamp order are
        // hello one, a-one, a-two, hello two, u3, u4, u5, u6, u7 → last 6.
        let turns = stats.last_turns.expect("previews present");
        assert_eq!(turns.len(), 6);
        let texts: Vec<&str> = turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["hello two", "u3", "u4", "u5", "u6", "u7"]);

        // Occupancy = the (single) assistant turn's re-sent context:
        // input(1000) + cache_read(500) + cache_creation(200) = 1700. Output is
        // NOT counted.
        assert_eq!(stats.tokens, 1700);
    }

    #[test]
    fn occupancy_is_last_assistant_turn_not_a_cross_turn_sum() {
        // Two assistant turns with usage; the LAST one wins (current window fill).
        // A naive sum of cache_read across turns would be 600+9000 = wrong; the
        // answer is purely the last turn's input+cache_read+cache_creation.
        let lines = [
            r#"{"type":"assistant","timestamp":"2026-06-04T10:00:00.000Z","message":{"content":[{"type":"text","text":"t1"}],"usage":{"input_tokens":400,"cache_read_input_tokens":600,"cache_creation_input_tokens":0,"output_tokens":50}}}"#,
            r#"{"type":"assistant","timestamp":"2026-06-04T10:00:01.000Z","message":{"content":[{"type":"text","text":"t2"}],"usage":{"input_tokens":2000,"cache_read_input_tokens":9000,"cache_creation_input_tokens":1000,"output_tokens":120}}}"#,
        ];
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.jsonl");
        let content = lines.join("\n");
        write_file(&path, &content);
        let stats = read_stats(&path, false);
        // last turn: 2000 + 9000 + 1000 = 12000.
        assert_eq!(stats.tokens, 12000);
    }

    #[test]
    fn occupancy_zero_when_no_usage_present() {
        // Old transcript: assistant lines with no `usage` → degrade to 0, never
        // a panic or a fallback to a word-count.
        let content = r#"{"type":"assistant","timestamp":"2026-06-04T10:00:00.000Z","message":{"content":[{"type":"text","text":"hi"}]}}"#;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.jsonl");
        write_file(&path, content);
        let stats = read_stats(&path, false);
        assert_eq!(stats.tokens, 0);
    }

    #[test]
    fn ai_title_used_when_not_user_named() {
        let content = r#"{"type":"ai-title","aiTitle":"AI Name"}"#;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.jsonl");
        write_file(&path, content);
        let stats = read_stats(&path, false);
        assert_eq!(stats.name.as_deref(), Some("AI Name"));
        assert!(!stats.user_named, "ai-title does not set user_named");
    }

    #[test]
    fn agent_name_takes_precedence_and_blocks_later_ai_title() {
        let content = [
            r#"{"type":"agent-name","agentName":"Agent X"}"#,
            r#"{"type":"ai-title","aiTitle":"Override Me"}"#,
        ]
        .join("\n");
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.jsonl");
        write_file(&path, &content);
        let stats = read_stats(&path, false);
        assert_eq!(stats.name.as_deref(), Some("Agent X"));
        assert!(stats.user_named);
    }

    #[test]
    fn user_string_content_preview() {
        let content = r#"{"type":"user","timestamp":"2026-06-04T10:00:00.000Z","message":{"content":"plain string body"}}"#;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.jsonl");
        write_file(&path, content);
        let stats = read_stats(&path, true);
        let turns = stats.last_turns.unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].text, "plain string body");
    }

    #[test]
    fn slice_truncates_to_200_chars() {
        let long = "x".repeat(500);
        let content = format!(
            r#"{{"type":"user","timestamp":"2026-06-04T10:00:00.000Z","message":{{"content":"{long}"}}}}"#
        );
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.jsonl");
        write_file(&path, &content);
        let stats = read_stats(&path, true);
        let turns = stats.last_turns.unwrap();
        assert_eq!(turns[0].text.chars().count(), 200);
    }

    #[test]
    fn unreadable_file_yields_zeroed_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("does-not-exist.jsonl");
        let stats = read_stats(&path, true);
        assert_eq!(stats, JsonlStats::default());
    }

    #[test]
    fn find_jsonl_path_cwd_tier() {
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path();
        let cwd = "/home/user/proj";
        let slug = cwd_to_project_path(cwd);
        let sid = "abc-123";
        let path = projects.join(&slug).join(format!("{sid}.jsonl"));
        write_file(&path, "{}");
        let found = find_jsonl_path(projects, sid, Some(cwd));
        assert_eq!(found, Some(path));
    }

    #[test]
    fn find_jsonl_path_scan_tier() {
        // cwd-derived dir does not contain it; fallback scan finds it elsewhere.
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path();
        let sid = "xyz-789";
        let path = projects
            .join("-some-other-dir")
            .join(format!("{sid}.jsonl"));
        write_file(&path, "{}");
        let found = find_jsonl_path(projects, sid, Some("/wrong/cwd"));
        assert_eq!(found, Some(path));
    }

    #[test]
    fn find_jsonl_path_missing() {
        let tmp = TempDir::new().unwrap();
        write_file(&tmp.path().join("-d").join("other.jsonl"), "{}");
        assert_eq!(find_jsonl_path(tmp.path(), "nope", None), None);
    }

    #[test]
    fn scan_all_collects_every_jsonl() {
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path();
        write_file(&projects.join("-dir-a").join("s1.jsonl"), "{}");
        write_file(&projects.join("-dir-a").join("s2.jsonl"), "{}");
        write_file(&projects.join("-dir-b").join("s3.jsonl"), "{}");
        // Non-jsonl file ignored.
        write_file(&projects.join("-dir-b").join("notes.txt"), "x");
        let mut metas = scan_all(projects);
        metas.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        let ids: Vec<&str> = metas.iter().map(|m| m.session_id.as_str()).collect();
        assert_eq!(ids, vec!["s1", "s2", "s3"]);
        let s3 = metas.iter().find(|m| m.session_id == "s3").unwrap();
        assert_eq!(s3.project_dir, "-dir-b");
        assert!(s3.mtime_ms > 0);
    }

    #[test]
    fn scan_all_missing_projects_dir_is_empty() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope");
        assert!(scan_all(&missing).is_empty());
    }

    #[test]
    fn iso8601_parse_orders_correctly() {
        let a = parse_iso8601_utc_ms("2026-06-04T10:00:00.000Z").unwrap();
        let b = parse_iso8601_utc_ms("2026-06-04T10:00:01.000Z").unwrap();
        assert!(b > a);
        assert_eq!(b - a, 1000);
        // 1970 epoch sanity.
        assert_eq!(parse_iso8601_utc_ms("1970-01-01T00:00:00Z").unwrap(), 0);
    }
}
