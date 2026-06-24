//! Negative control (spec §9, gate item 6) — feature-gated `mutation-evidence`.
//!
//! Proves the parity test is NOT vacuously green: deliberately-WRONG outputs MUST
//! differ from the frozen golden. Each test PASSES by asserting INEQUALITY (the
//! mutated render != the golden) — so a green run here is evidence the comparator
//! catches the mutation. Run:
//!
//!   scripts/build-lock.sh cargo test -p dispatch --features mutation-evidence
//!
//! Three mutation classes:
//!   1. status mapping (busy → idle)     — a value mutation.
//!   2. dropped `socketDir`              — a field-omission mutation.
//!   3. reordered JSON keys              — a mutation a NAIVE (parse-and-compare)
//!      comparator would MISS but byte-compare catches.

#![cfg(feature = "mutation-evidence")]

mod common;

use std::path::PathBuf;

use dispatch::join::{self, JoinOpts};
use dispatch::model::SessionStatus;
use dispatch::render;

use common::{basic_mux, basic_process_table, empty_probe, env_with_zmx_dir, TestHome};

fn golden_text(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("missing golden {path:?}: {e}"))
}

/// A coded `home-basic` run plus the two volatile prefixes for normalization (so
/// the rendered text matches the path-normalized golden).
struct Coded {
    sessions: Vec<dispatch::model::Session>,
    strays: Vec<dispatch::stray::Stray>,
    home_dir: PathBuf,
    zmx_canonical: PathBuf,
}

impl Coded {
    fn normalize(&self, text: &str) -> String {
        text.replace(&self.home_dir.to_string_lossy().into_owned(), "<HOME>")
            .replace(&self.zmx_canonical.to_string_lossy().into_owned(), "<ZMX>")
    }
    fn rendered(&self) -> String {
        self.normalize(&render::to_pretty(&render::ls_json(
            &self.sessions,
            &self.strays,
        )))
    }
}

fn basic_all_sessions() -> Coded {
    let home = TestHome::from_fixture("home-basic");
    home.freeze_basic_mtimes();
    let home_dir = home.paths.home.clone();
    let tmp_root = tempfile::tempdir().unwrap();
    let canonical = tmp_root.path().join("canonical").join("zmx-501");
    let legacy = tmp_root.path().join("legacy-ctx").join("zmx-501");
    std::fs::create_dir_all(&canonical).unwrap();
    std::fs::create_dir_all(&legacy).unwrap();
    let env = env_with_zmx_dir(&canonical);
    let mux = basic_mux(&canonical, &legacy);
    let pt = basic_process_table();
    let probe = empty_probe();
    let clock = dispatch::effects::FixedClock(1_717_500_300_000);
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: true,
        limit: None,
    };
    let inputs = join::gather(
        &home.paths,
        &mux,
        &env,
        &pt,
        &probe,
        &clock,
        tmp_root.path(),
        None, // hermetic: suppress the machine-global XDG-family scan.
        opts,
    );
    let (mut sessions, strays) = join::join_with_strays(&inputs, opts);
    join::assign_codes(&mut sessions);
    Coded {
        sessions,
        strays,
        home_dir,
        zmx_canonical: canonical,
    }
}

/// Sanity: the UNMUTATED render MUST equal the golden (proves the harness is
/// wired right — otherwise the inequality assertions below are meaningless).
#[test]
fn baseline_matches_golden() {
    let run = basic_all_sessions();
    assert_eq!(
        run.rendered(),
        golden_text("ls-basic.json"),
        "baseline must match golden"
    );
}

/// Mutation 1: status busy → idle. Caught.
#[test]
fn negative_control_status_mutation_caught() {
    let mut run = basic_all_sessions();
    for s in &mut run.sessions {
        if s.status == SessionStatus::Busy {
            s.status = SessionStatus::Idle;
        }
    }
    assert_ne!(
        run.rendered(),
        golden_text("ls-basic.json"),
        "status busy→idle mutation MUST be caught by the parity comparator"
    );
}

/// Mutation 2: drop socketDir. Caught.
#[test]
fn negative_control_dropped_field_caught() {
    let mut run = basic_all_sessions();
    for s in &mut run.sessions {
        s.socket_dir = None;
    }
    assert_ne!(
        run.rendered(),
        golden_text("ls-basic.json"),
        "dropped socketDir MUST be caught"
    );
}

/// Mutation 3: reorder two JSON keys. A naive parse-and-compare comparator would
/// MISS this (same key/value set); byte-compare catches it. Caught.
#[test]
fn negative_control_key_reorder_caught_by_byte_compare() {
    let run = basic_all_sessions();
    let value = render::ls_json(&run.sessions, &run.strays);
    // Build a key-reordered variant of the FIRST object by swapping its first two
    // keys, preserving every key/value pair (only the ORDER changes).
    let arr = value.as_array().unwrap();
    let mut new_arr: Vec<serde_json::Value> = Vec::new();
    for (i, obj) in arr.iter().enumerate() {
        if i == 0 {
            let m = obj.as_object().unwrap();
            let mut reordered = serde_json::Map::new();
            let keys: Vec<&String> = m.keys().collect();
            assert!(keys.len() >= 2, "need >=2 keys to reorder");
            // Insert key[1] before key[0], then the rest in order.
            reordered.insert(keys[1].clone(), m[keys[1]].clone());
            reordered.insert(keys[0].clone(), m[keys[0]].clone());
            for k in &keys[2..] {
                reordered.insert((*k).clone(), m[*k].clone());
            }
            new_arr.push(serde_json::Value::Object(reordered));
        } else {
            new_arr.push(obj.clone());
        }
    }
    // Normalize the volatile temp paths so ONLY the key-order differs from golden.
    let mutated = run.normalize(&render::to_pretty(&serde_json::Value::Array(new_arr)));
    let golden = golden_text("ls-basic.json");

    // Byte-compare CATCHES the reorder.
    assert_ne!(
        mutated, golden,
        "key reorder MUST be caught by byte-compare"
    );

    // And confirm a NAIVE comparator (parse then compare values) would MISS it:
    // the two parse to structurally-equal JSON (serde_json::Value with
    // preserve_order compares key ORDER too, so we compare via a sorted view).
    let golden_val: serde_json::Value = serde_json::from_str(&golden).unwrap();
    let mutated_val: serde_json::Value = serde_json::from_str(&mutated).unwrap();
    assert_eq!(
        sorted_keys_repr(&golden_val),
        sorted_keys_repr(&mutated_val),
        "a key-order-insensitive comparator would MISS the reorder (so byte-compare is load-bearing)"
    );
}

/// Render a JSON value with object keys SORTED, so two values differing only in
/// key order compare equal — modeling a naive comparator that ignores order.
fn sorted_keys_repr(v: &serde_json::Value) -> String {
    fn sort(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for k in keys {
                    out.insert(k.clone(), sort(&m[k]));
                }
                serde_json::Value::Object(out)
            }
            serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&sort(v)).unwrap()
}
