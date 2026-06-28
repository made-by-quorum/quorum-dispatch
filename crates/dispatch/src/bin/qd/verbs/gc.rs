//! REAL `qd gc` backend (spec §5.2; TS `commands/gc.ts`).
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
    format_bytes, jsonl_is_candidate, move_to_trash_at, parse_iso_ms, relative_time, shorten_path,
    should_purge, tombstone_is_candidate, Candidate, CandidateType, TrashMeta,
};
use dispatch::paths::QdPaths;
use dispatch::registry::{get_tombstoned_entries, read_entries};

/// `qd gc` — mutually-exclusive modes, then the default scan→trash run.
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
            eprintln!("qd gc: HOME is not set — cannot resolve the trash dirs.");
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
    // from_home_env so the presence `state_dir` honors QD_HOME (the recovery rows +
    // liveness locks live under `<QD_HOME>/state`, not the HOME default). The
    // `.claude` dirs (sessions/projects/relay/inbox) stay HOME-derived regardless.
    let paths = QdPaths::from_home_env(home, env);
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

    // 4. WS-B / B1 — collectible relay-inbox messages (ack+TTL+GC). Presence-gated
    //    via the BANKED `dispatch::presence` surface (read-only). The legacy classes
    //    above keep their `is_pid_alive` live-set UNCHANGED — only this class is
    //    presence-gated. Walks ONLY `inbox_dir` (the `.json` collision guard vs
    //    OcSidecar is structural — no other scanner walks `inbox_dir`).
    candidates.extend(dispatch::inbox_gc::scan_collectible(
        &paths.inbox_dir,
        &paths.state_dir,
        now,
    ));

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
    for c in &candidates {
        if move_to_trash(c, home, clock) {
            println!("  ✓ {} → trash", shorten_path(&c.path, home));
            moved += 1;
        }
    }
    println!(
        "\n{moved} item{} moved to trash. Use `qd gc --list-trash` to see, `qd gc --recover <name>` to restore.",
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

/// Move one candidate to trash — delegates to the canonical
/// [`dispatch::gc::move_to_trash_at`] (copy-then-unlink, `gc.ts:271-300`) so the
/// verb and the relay sweeper share ONE trash implementation. Returns true on
/// success. The clock is injected (the driver supplies `now_ms`).
fn move_to_trash(c: &Candidate, home: &Path, clock: &RealClock) -> bool {
    let trash_dir = if c.kind.is_claude() {
        home.join(".claude").join("trash")
    } else {
        home.join(".opencode").join("trash")
    };
    let ok = move_to_trash_at(
        &c.path,
        &trash_dir,
        c.kind,
        &c.identifier,
        &c.reason,
        c.size,
        clock.now_ms(),
    );
    if !ok {
        eprintln!("  ✗ Failed to trash {}", shorten_path(&c.path, home));
    }
    ok
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
        eprintln!("Run `qd gc --list-trash` to see available items.");
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

#[cfg(test)]
mod tests {
    use super::*;
    use dispatch::gc::iso_from_ms;
    use std::fs;
    use tempfile::tempdir;

    const RECV_MS: i64 = 1_780_000_000_000;
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;

    fn write_inbox(dir: &Path, id: &str, json: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{id}.json")), json).unwrap();
    }

    /// A legacy backlog file: ONLY the 4 legacy fields (no to_session/acked_at_ms).
    fn legacy_json(id: &str, recv_ms: i64) -> String {
        format!(
            r#"{{"text":"Reply with exactly: RELAY_OK","from_session":"sender","message_id":"{id}","received_at":"{}"}}"#,
            iso_from_ms(recv_ms)
        )
    }

    #[test]
    fn extension_collision_guard_inbox_vs_oc() {
        // An inbox *.json is classified RelayInbox by the inbox-dir scanner; the
        // OC-log scanner (which walks the OC servers dir, never inbox_dir) cannot
        // mis-claim it (§3.1 T5 — the guard is structural, by owning directory).
        let tmp = tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        write_inbox(&inbox, "relay-x", &legacy_json("relay-x", RECV_MS));
        let now = RECV_MS + 14 * DAY_MS;
        let inbox_cands = dispatch::inbox_gc::scan_collectible(&inbox, &tmp.path().join("state"), now);
        assert_eq!(inbox_cands.len(), 1);
        assert_eq!(inbox_cands[0].kind, CandidateType::RelayInbox);
        // The OC scanner pointed at the SAME dir finds no OC logs (a .json with no
        // orphaned .log) — it never claims the inbox file as an OC class.
        assert!(
            scan_orphaned_oc_logs(&inbox).is_empty(),
            "OC scanner never claims an inbox .json"
        );
    }

    #[test]
    fn legacy_class_trash_unchanged_no_regression() {
        // Behavior-preservation: a CcJsonl candidate still trashes to ~/.claude/trash
        // with ext .jsonl + meta kind "cc-jsonl" — the move_to_trash delegation to the
        // shared engine primitive did NOT alter the legacy classes' on-disk shape.
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        let src = home.join("dead.jsonl");
        fs::write(&src, b"line\n").unwrap();
        let c = Candidate {
            kind: CandidateType::CcJsonl,
            path: src.clone(),
            reason: "dead session".to_string(),
            size: 5,
            identifier: "sid-123".to_string(),
        };
        assert!(move_to_trash(&c, home, &RealClock));
        assert!(!src.exists());
        let trash = home.join(".claude").join("trash");
        let entries: Vec<String> = fs::read_dir(&trash)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let meta_file = entries
            .iter()
            .find(|n| n.ends_with("_meta.json"))
            .expect("meta written");
        let meta: TrashMeta =
            serde_json::from_str(&fs::read_to_string(trash.join(meta_file)).unwrap()).unwrap();
        assert_eq!(meta.kind, "cc-jsonl");
        assert!(
            entries
                .iter()
                .any(|n| n.ends_with(".jsonl") && !n.ends_with("_meta.json")),
            "trashed file keeps the .jsonl extension"
        );
    }
}
