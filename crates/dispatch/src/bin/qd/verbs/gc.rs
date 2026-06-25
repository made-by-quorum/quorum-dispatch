//! REAL `sb gc` backend (spec §5.2; TS `commands/gc.ts`).
//!
//! The pure deciders (age-gating, candidate classification, trash naming, purge
//! gating) live in `dispatch::gc`; this verb drives the fs scan, the trash move (copy
//! + meta + unlink), and the `--list-trash` / `--recover` / `--purge` modes.
//!
//! L10 is structural: every candidate is a FILE keyed by a specific DEAD identity
//! (a session id whose registry PID is dead, a sidecar whose PID is dead + port
//! unresponsive). Never patterns, never a live/attached session. Exits 0/1.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use clap::ArgMatches;

use dispatch::effects::{is_pid_alive, Clock, Env, RealClock, RealEnv};
use dispatch::gc::{
    format_bytes, iso_stamp_from, jsonl_is_candidate, relative_time, shorten_path, should_purge,
    tombstone_is_candidate, trash_name, type_token, Candidate, CandidateType, TrashMeta,
};
use dispatch::paths::SbPaths;
use dispatch::registry::{get_tombstoned_entries, read_entries};

/// `sb gc` — mutually-exclusive modes, then the default scan→trash run.
pub fn run(m: &ArgMatches) -> i32 {
    let dry_run = m.get_flag("dry-run");
    let list_trash = m.get_flag("list-trash");
    let recover = m.get_one::<String>("recover");
    let purge = m.get_flag("purge");

    // Mutually exclusive modes (gc.ts:495-499).
    let mode_count = [dry_run, list_trash, recover.is_some(), purge]
        .iter()
        .filter(|b| **b)
        .count();
    if mode_count > 1 {
        eprintln!(
            "Options --dry-run, --list-trash, --recover, and --purge are mutually exclusive."
        );
        return 1;
    }

    let env = RealEnv;
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("sb gc: HOME is not set — cannot resolve the trash dirs.");
            return 1;
        }
    };
    let clock = RealClock;

    let cc_trash = home.join(".claude").join("trash");
    let oc_trash = home.join(".opencode").join("trash");

    if list_trash {
        return list_trash_mode(&cc_trash, &oc_trash, &home, clock.now_ms());
    }
    if let Some(item) = recover {
        return recover_mode(item, &cc_trash, &oc_trash, &home);
    }
    if purge {
        return purge_mode(&cc_trash, &oc_trash, &home, &clock);
    }

    // Default: scan → (dry-run print | trash move).
    gc_run(&home, &env, &clock, dry_run)
}

/// The default GC scan + (print | move) (gc.ts:506-580).
fn gc_run(home: &Path, env: &RealEnv, clock: &RealClock, dry_run: bool) -> i32 {
    let paths = SbPaths::from_home(home);
    let now = clock.now_ms();

    eprintln!("Scanning for GC candidates...");

    // Build the live-session-id set: a registry PID alive OR under a live zmx
    // wrapper (gc.ts:91-113). We approximate "under a live zmx wrapper" by the
    // registry PID being alive — the join's zmx liveness is the same kill -0.
    let registry = read_entries(&paths.sessions_dir, false);
    let mut live_session_ids: HashSet<String> = HashSet::new();
    for scanned in &registry {
        if let (Some(pid), Some(sid)) = (scanned.entry.pid, &scanned.entry.session_id) {
            if is_pid_alive(pid as i32) {
                live_session_ids.insert(sid.clone());
            }
        }
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    // 1. Dead CC JSONL (≥7d, PID dead) — scan the projects dir.
    candidates.extend(scan_dead_jsonl(&paths.projects_dir, now, &live_session_ids));

    // 2. Old tombstones (≥7d).
    for t in get_tombstoned_entries(&paths.sessions_dir) {
        if tombstone_is_candidate(t.mtime_ms, now) {
            candidates.push(Candidate {
                kind: CandidateType::CcTombstone,
                path: t.path.clone(),
                // TS reason: `tombstoned session, killed ${relativeTime(t.mtime)}`
                // (gc.ts:221). Deterministic via the injected Clock — byte-parity.
                reason: format!(
                    "tombstoned session, killed {}",
                    relative_time(t.mtime_ms, now)
                ),
                size: 0,
                identifier: t
                    .data
                    .session_id
                    .clone()
                    .unwrap_or_else(|| t.pid.to_string()),
            });
        }
    }

    // 3. OC sidecars / logs. The Rust engine never constructs OpenCode rows
    //    (named exclusion, spec §2) and the join carries no OC server state, so
    //    the OC sidecar/log scan is structurally present but yields nothing here
    //    (the servers dir is normally absent). We still scan it so a real OC
    //    deployment's stale files are collected if present.
    let oc_servers = home.join(".opencode").join("servers");
    candidates.extend(scan_orphaned_oc_logs(&oc_servers));

    if candidates.is_empty() {
        println!("No GC candidates found. Everything is clean.");
        return 0;
    }

    // Display candidates.
    println!("\nGC candidates:\n");
    for c in &candidates {
        println!(
            "  {:<12}{} ({})",
            c.kind.label(),
            shorten_path(&c.path, home),
            format_bytes(c.size)
        );
        println!("              {}", c.reason);
        println!();
    }
    let n = candidates.len();
    println!("{n} item{} to prune.", if n == 1 { "" } else { "s" });

    if dry_run {
        println!("\n(dry run — no changes made. Run without --dry-run to move to trash.)");
        return 0;
    }

    // Move to trash.
    println!();
    let mut moved = 0;
    let _ = env;
    for c in &candidates {
        if move_to_trash(c, home, clock) {
            println!("  ✓ {} → trash", shorten_path(&c.path, home));
            moved += 1;
        }
    }
    println!(
        "\n{moved} item{} moved to trash. Use `sb gc --list-trash` to see, `sb gc --recover <name>` to restore.",
        if moved == 1 { "" } else { "s" }
    );
    0
}

/// Scan the projects dir for dead CC JSONL transcripts (gc.ts:117-145).
fn scan_dead_jsonl(
    projects_dir: &Path,
    now_ms: i64,
    live_session_ids: &HashSet<String>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    let Ok(dirs) = fs::read_dir(projects_dir) else {
        return out;
    };
    for dent in dirs.flatten() {
        let dir_path = dent.path();
        let Ok(files) = fs::read_dir(&dir_path) else {
            continue;
        };
        for f in files.flatten() {
            let name = f.file_name();
            let name = name.to_string_lossy();
            let Some(session_id) = name.strip_suffix(".jsonl") else {
                continue;
            };
            let full = f.path();
            let Ok(meta) = fs::metadata(&full) else {
                continue;
            };
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if jsonl_is_candidate(session_id, mtime_ms, now_ms, live_session_ids) {
                out.push(Candidate {
                    kind: CandidateType::CcJsonl,
                    path: full,
                    // TS reason: `dead session, last active ${relativeTime(s.mtime)}`
                    // (gc.ts:135). Deterministic via the injected now_ms — byte-parity.
                    reason: format!(
                        "dead session, last active {}",
                        relative_time(mtime_ms, now_ms)
                    ),
                    size: meta.len(),
                    identifier: session_id.to_string(),
                });
            }
        }
    }
    out
}

/// Scan for orphaned OC logs — a `.log` with no matching `.json` sidecar
/// (gc.ts:178-200).
fn scan_orphaned_oc_logs(servers_dir: &Path) -> Vec<Candidate> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(servers_dir) else {
        return out;
    };
    let entries: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let json_files: HashSet<&str> = entries
        .iter()
        .filter(|f| f.ends_with(".json"))
        .map(|s| s.as_str())
        .collect();
    for f in &entries {
        let Some(port) = f.strip_suffix(".log") else {
            continue;
        };
        if json_files.contains(format!("{port}.json").as_str()) {
            continue;
        }
        let full = servers_dir.join(f);
        let size = fs::metadata(&full).map(|m| m.len()).unwrap_or(0);
        out.push(Candidate {
            kind: CandidateType::OcLog,
            path: full,
            reason: "orphaned log, no matching sidecar".to_string(),
            size,
            identifier: port.to_string(),
        });
    }
    out
}

/// Move one candidate to trash: write meta, copy, then unlink the original
/// (gc.ts:271-300). Returns true on success.
fn move_to_trash(c: &Candidate, home: &Path, clock: &RealClock) -> bool {
    let trash_dir = if c.kind.is_claude() {
        home.join(".claude").join("trash")
    } else {
        home.join(".opencode").join("trash")
    };
    let timestamp = iso_stamp_from(&iso_now(clock));
    let name = trash_name(&timestamp, &c.identifier, c.kind);
    let trash_path = trash_dir.join(&name);
    let meta_path = trash_dir.join(format!("{name}_meta.json"));

    if fs::create_dir_all(&trash_dir).is_err() {
        return false;
    }
    let meta = TrashMeta {
        original_path: c.path.to_string_lossy().into_owned(),
        reason: c.reason.clone(),
        pruned_at: iso_now(clock),
        kind: type_token(c.kind).to_string(),
        size: c.size,
    };
    let Ok(meta_json) = serde_json::to_string_pretty(&meta) else {
        return false;
    };
    // Write metadata first, then copy, then unlink original (only after both
    // succeed). Clean up partial trash on failure.
    if fs::write(&meta_path, meta_json).is_err() {
        return false;
    }
    if fs::copy(&c.path, &trash_path).is_err() {
        let _ = fs::remove_file(&meta_path);
        return false;
    }
    if fs::remove_file(&c.path).is_err() {
        let _ = fs::remove_file(&meta_path);
        let _ = fs::remove_file(&trash_path);
        eprintln!("  ✗ Failed to trash {}", shorten_path(&c.path, home));
        return false;
    }
    true
}

/// `--list-trash` (gc.ts:302-346).
fn list_trash_mode(cc: &Path, oc: &Path, home: &Path, now_ms: i64) -> i32 {
    let mut items: Vec<(String, TrashMeta, PathBuf)> = Vec::new();
    for dir in [cc, oc] {
        if let Ok(rd) = fs::read_dir(dir) {
            for dent in rd.flatten() {
                let fname = dent.file_name();
                let fname = fname.to_string_lossy();
                let Some(base) = fname.strip_suffix("_meta.json") else {
                    continue;
                };
                if let Ok(text) = fs::read_to_string(dent.path()) {
                    if let Ok(meta) = serde_json::from_str::<TrashMeta>(&text) {
                        items.push((base.to_string(), meta, dir.to_path_buf()));
                    }
                }
            }
        }
    }
    if items.is_empty() {
        println!("Trash is empty.");
        return 0;
    }
    // Sort by pruned-at, newest first (string ISO compare is chronological).
    items.sort_by(|a, b| b.1.pruned_at.cmp(&a.1.pruned_at));
    println!("Trash contents:\n");
    for (name, meta, _) in &items {
        println!("  {name}");
        println!(
            "    From:   {}",
            shorten_path(Path::new(&meta.original_path), home)
        );
        println!("    Reason: {}", meta.reason);
        // TS prints an `Age:` line (gc.ts:333) — relativeTime(prunedAt), made
        // deterministic via the injected now_ms. An unparseable stamp → "0s ago".
        let age = relative_time(parse_iso_ms(&meta.pruned_at).unwrap_or(now_ms), now_ms);
        println!("    Age:    {age}");
        println!("    Size:   {}", format_bytes(meta.size));
        println!();
    }
    let n = items.len();
    println!(
        "{n} item{} in trash. Use `--recover <name>` to restore, `--purge` to delete items >30d.",
        if n == 1 { "" } else { "s" }
    );
    0
}

/// `--recover <item>` (gc.ts:348-419). Name/prefix match; refuses if the original
/// path already exists.
fn recover_mode(query: &str, cc: &Path, oc: &Path, home: &Path) -> i32 {
    let mut matches: Vec<(String, TrashMeta, PathBuf, String)> = Vec::new();
    for dir in [cc, oc] {
        if let Ok(rd) = fs::read_dir(dir) {
            for dent in rd.flatten() {
                let fname = dent.file_name();
                let fname = fname.to_string_lossy();
                let Some(base) = fname.strip_suffix("_meta.json") else {
                    continue;
                };
                if base == query || base.to_lowercase().contains(&query.to_lowercase()) {
                    if let Ok(text) = fs::read_to_string(dent.path()) {
                        if let Ok(meta) = serde_json::from_str::<TrashMeta>(&text) {
                            matches.push((
                                base.to_string(),
                                meta,
                                dir.to_path_buf(),
                                fname.into_owned(),
                            ));
                        }
                    }
                }
            }
        }
    }
    if matches.is_empty() {
        eprintln!("No trash item matching \"{query}\".");
        eprintln!("Run `sb gc --list-trash` to see available items.");
        return 1;
    }
    if matches.len() > 1 {
        eprintln!("Ambiguous — \"{query}\" matches {} items:", matches.len());
        for (name, meta, _, _) in &matches {
            eprintln!(
                "  {name}  (from {})",
                shorten_path(Path::new(&meta.original_path), home)
            );
        }
        eprintln!("\nSpecify a more precise name.");
        return 1;
    }
    let (name, meta, dir, meta_file) = &matches[0];
    let trash_file = dir.join(name);
    let meta_path = dir.join(meta_file);
    let original = Path::new(&meta.original_path);

    // Refuse if the original already exists (gc.ts:393-401).
    if original.exists() {
        eprintln!(
            "Cannot recover: {} already exists.",
            shorten_path(original, home)
        );
        eprintln!("Remove or rename the existing file first.");
        return 1;
    }
    // Trash file must still exist (gc.ts:403-411).
    if !trash_file.exists() {
        eprintln!("Trash file missing: {}", trash_file.display());
        eprintln!("The metadata exists but the file is gone. Cleaning up metadata.");
        let _ = fs::remove_file(&meta_path);
        return 1;
    }
    // Restore: ensure parent dir, copy back, remove trash + meta.
    if let Some(parent) = original.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::copy(&trash_file, original).is_err() {
        eprintln!("Failed to recover: copy failed.");
        return 1;
    }
    let _ = fs::remove_file(&trash_file);
    let _ = fs::remove_file(&meta_path);
    println!("✓ Recovered {}", shorten_path(original, home));
    0
}

/// `--purge` (gc.ts:421-470): delete trash items older than 30 days.
fn purge_mode(cc: &Path, oc: &Path, home: &Path, clock: &RealClock) -> i32 {
    let now = clock.now_ms();
    let mut purged = 0;
    let mut remaining = 0;
    for dir in [cc, oc] {
        if let Ok(rd) = fs::read_dir(dir) {
            for dent in rd.flatten() {
                let fname = dent.file_name();
                let fname = fname.to_string_lossy();
                let Some(base) = fname.strip_suffix("_meta.json") else {
                    continue;
                };
                let Ok(text) = fs::read_to_string(dent.path()) else {
                    continue;
                };
                let Ok(meta) = serde_json::from_str::<TrashMeta>(&text) else {
                    continue;
                };
                let pruned_at_ms = parse_iso_ms(&meta.pruned_at).unwrap_or(now);
                if should_purge(pruned_at_ms, now) {
                    let trash_file = dir.join(base);
                    let _ = fs::remove_file(&trash_file);
                    let _ = fs::remove_file(dent.path());
                    println!(
                        "  ✓ Purged {base} ({})",
                        shorten_path(Path::new(&meta.original_path), home)
                    );
                    purged += 1;
                } else {
                    remaining += 1;
                }
            }
        }
    }
    if purged == 0 && remaining == 0 {
        println!("Trash is empty. Nothing to purge.");
    } else if purged == 0 {
        println!(
            "No items older than 30 days. {remaining} item{} remain.",
            if remaining == 1 { "" } else { "s" }
        );
    } else {
        println!(
            "\nPurged {purged} item{}. {remaining} item{} remain (< 30 days).",
            if purged == 1 { "" } else { "s" },
            if remaining == 1 { "" } else { "s" }
        );
    }
    0
}

/// Current time as an ISO-8601 string (`new Date().toISOString()` shape:
/// `YYYY-MM-DDTHH:MM:SS.mmmZ`). Built from the injected Clock's epoch ms so the
/// timestamp stays testable; the format is UTC.
fn iso_now(clock: &RealClock) -> String {
    iso_from_ms(clock.now_ms())
}

/// Format epoch ms as an ISO-8601 UTC string. Hand-rolled (no chrono dep) —
/// civil-date from a Unix timestamp via the standard days-from-epoch algorithm.
fn iso_from_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

/// Parse the ms-precision UTC `pruned_at` ISO string back to epoch ms (inverse
/// of [`iso_from_ms`]). Permissive: an unparseable string yields None.
fn parse_iso_ms(iso: &str) -> Option<i64> {
    // Expect YYYY-MM-DDTHH:MM:SS(.mmm)?Z
    let bytes = iso.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let y: i64 = iso.get(0..4)?.parse().ok()?;
    let mo: i64 = iso.get(5..7)?.parse().ok()?;
    let d: i64 = iso.get(8..10)?.parse().ok()?;
    let h: i64 = iso.get(11..13)?.parse().ok()?;
    let mi: i64 = iso.get(14..16)?.parse().ok()?;
    let s: i64 = iso.get(17..19)?.parse().ok()?;
    let millis: i64 = if iso.len() >= 23 && iso.as_bytes().get(19) == Some(&b'.') {
        iso.get(20..23)?.parse().ok()?
    } else {
        0
    };
    let days = days_from_civil(y, mo, d);
    Some(((days * 86_400 + h * 3600 + mi * 60 + s) * 1000) + millis)
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date from days since the Unix epoch (inverse of [`days_from_civil`]).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trips_epoch_ms() {
        // 2026-06-05T01:23:45.678Z → ms → back.
        let ms = 1_780_622_625_678;
        let iso = iso_from_ms(ms);
        assert!(iso.ends_with('Z'), "iso: {iso}");
        assert_eq!(parse_iso_ms(&iso), Some(ms));
    }

    #[test]
    fn iso_from_ms_known_value() {
        // Unix epoch.
        assert_eq!(iso_from_ms(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn iso_stamp_replaces_for_trash_name() {
        let stamp = iso_stamp_from(&iso_from_ms(0));
        assert_eq!(stamp, "1970-01-01T00-00-00");
    }
}
