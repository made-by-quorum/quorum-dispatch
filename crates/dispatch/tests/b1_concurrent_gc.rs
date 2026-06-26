//! WS-B / B1 — TRACKED regression gate for FINDING F1 (the red-team's GC-vs-GC
//! no-loss hole). Promotes the red-team's `b1_redteam.rs` case3b/case3c repros
//! (which are an untracked evidence harness) into a permanent, committed gate.
//!
//! The hole: two GC drivers with NO cross-driver lock (the shipped `qd gc` verb +
//! the opt-in relay 30 s sweeper — §3.5; or two `qd gc` runs) over the SHARED inbox
//! dir could, when both pick the same file in the same wall-clock SECOND, compute
//! the SAME trash name (`iso_stamp_from` truncates to seconds) and have the losing
//! `move_to_trash_at`'s `remove_file(src)=ENOENT` cleanup delete the WINNER's trash
//! copy (F1a: hard delete — message in NEITHER inbox nor trash) and/or strip the
//! shared `_meta.json` (F1b: an unrecoverable orphan, since `--list-trash`/
//! `--recover`/`--purge` all key off `_meta.json`).
//!
//! Both tests are RED on `d60e289` (pre-fix) and GREEN after the fix
//! (`gc::move_to_trash_at`: per-call nonce + O_EXCL writes + ENOENT-safe cleanup).
//! EVERY fixture is a synthetic `tempfile::tempdir()` — this NEVER reads, mutates,
//! or sweeps the real `~/.claude/channels/relay/inbox`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dispatch::gc::{iso_from_ms, move_to_trash_at, CandidateType, InboxRecord};
use dispatch::inbox_gc::sweep_inbox_once;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const RECV_MS: i64 = 1_780_000_000_000;

fn write_inbox(dir: &Path, id: &str, recv_ms: i64) {
    std::fs::create_dir_all(dir).unwrap();
    let json = format!(
        r#"{{"text":"Reply with exactly: RELAY_OK","from_session":"snd","message_id":"{id}","received_at":"{}"}}"#,
        iso_from_ms(recv_ms)
    );
    std::fs::write(dir.join(format!("{id}.json")), json).unwrap();
}

/// The set of message_ids that are GENUINELY RECOVERABLE from trash: enumerate the
/// `_meta.json` sidecars (the ONLY handle `--list-trash`/`--recover`/`--purge` use),
/// derive each data file by stripping `_meta.json` (exactly as `recover_mode` does),
/// and require the data file to exist AND parse. A bytes-present file with no meta is
/// NOT recoverable — it is the F1b orphan, and is excluded here on purpose.
fn recoverable_ids(trash: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(rd) = std::fs::read_dir(trash) else {
        return out;
    };
    for e in rd.flatten() {
        let fname = e.file_name().to_string_lossy().into_owned();
        let Some(base) = fname.strip_suffix("_meta.json") else {
            continue; // not a sidecar
        };
        let data = trash.join(base);
        let Ok(bytes) = std::fs::read(&data) else {
            continue; // meta with no data file — not recoverable
        };
        let rec: InboxRecord =
            serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("TORN trash file {base}: {e}"));
        out.insert(rec.message_id);
    }
    out
}

// ───────────────────────── F1a — no hard delete ─────────────────────────────
// Two concurrent GC drivers over the SAME inbox dir, all files expired & unaddressed
// (so every file is collectible and there is no presence protection). After both
// drivers finish, NO message may end up in NEITHER inbox nor (recoverable) trash.
#[test]
fn f1a_concurrent_gc_drivers_no_hard_delete() {
    let now = RECV_MS + 30 * DAY_MS; // both drivers share this second → name would collide pre-fix
    for iteration in 0..32 {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let state = tmp.path().join("state");
        let trash = tmp.path().join("trash");
        std::fs::create_dir_all(&state).unwrap();

        const N: usize = 200;
        let ids: Vec<String> = (0..N).map(|i| format!("relay-{iteration}-{i}")).collect();
        for id in &ids {
            write_inbox(&inbox, id, RECV_MS); // unaddressed, expired, never delivered
        }

        // Two GC drivers race, released together for maximal same-second overlap.
        let start = Arc::new(AtomicBool::new(false));
        let mut drivers = Vec::new();
        for _ in 0..2 {
            let (i, s, t, g) = (inbox.clone(), state.clone(), trash.clone(), start.clone());
            drivers.push(std::thread::spawn(move || {
                while !g.load(Ordering::Acquire) {}
                sweep_inbox_once(&i, &s, &t, now)
            }));
        }
        start.store(true, Ordering::Release);
        for d in drivers {
            d.join().expect("a GC driver must never panic");
        }

        let recoverable = recoverable_ids(&trash);
        for id in &ids {
            let in_inbox = inbox.join(format!("{id}.json")).exists();
            let in_trash = recoverable.contains(id);
            assert!(
                in_inbox || in_trash,
                "F1a NO-LOSS VIOLATED: {id} is in NEITHER inbox nor recoverable trash \
                 after two concurrent GC drivers (iter {iteration})"
            );
        }
    }
}

// ───────────────────────── F1b — stays recoverable ──────────────────────────
// Deterministic (no threads): two `move_to_trash_at` calls for the SAME id in the
// SAME second — the exact loser-strips-winner interleaving. Call 1 trashes the
// message; call 2's src is already gone. The trashed message must remain RECOVERABLE
// (its `_meta.json` intact, data file enumerable & byte-identical) — no orphan.
#[test]
fn f1b_concurrent_trash_stays_recoverable() {
    let tmp = tempfile::tempdir().unwrap();
    let inbox = tmp.path().join("inbox");
    let trash = tmp.path().join("trash");
    write_inbox(&inbox, "relay-shared", RECV_MS);
    let src = inbox.join("relay-shared.json");
    let original = std::fs::read(&src).unwrap();
    let now = RECV_MS + 30 * DAY_MS;

    // Winner: trashes the message (meta + copy land, src unlinked).
    assert!(
        move_to_trash_at(&src, &trash, CandidateType::RelayInbox, "relay-shared", "expired", 10, now),
        "the first trash move succeeds"
    );
    // Loser: same id + same now (same second). Its src is already gone — must not
    // strip the winner's entry.
    let _ = move_to_trash_at(&src, &trash, CandidateType::RelayInbox, "relay-shared", "expired", 10, now);

    // The message is still recoverable: a `_meta.json` is present, its data file
    // exists and is byte-identical to the original. (Pre-fix the shared meta was
    // stripped → recoverable set empty → orphan.)
    let recoverable = recoverable_ids(&trash);
    assert!(
        recoverable.contains("relay-shared"),
        "F1b ORPHAN: the trashed message is not recoverable — its `_meta.json` was stripped \
         by a concurrent same-second trash of the same id (recoverable = {recoverable:?})"
    );

    // And no orphan in the other direction: every data file has a matching sidecar,
    // and the surviving copy's bytes are intact.
    let mut data_files = 0;
    for e in std::fs::read_dir(&trash).unwrap().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.ends_with("_meta.json") {
            continue;
        }
        data_files += 1;
        let meta = trash.join(format!("{n}_meta.json"));
        assert!(meta.exists(), "trash data file {n} has no `_meta.json` sidecar (orphan)");
        assert_eq!(std::fs::read(e.path()).unwrap(), original, "recoverable copy is byte-identical");
    }
    assert!(data_files >= 1, "at least one recoverable trash copy survives");
}
