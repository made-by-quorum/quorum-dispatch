//! The stage-3 gate: **qd may not name qw's delivery log.**
//!
//! `09-ledger-split.md` rules the delivery ledger into two files, and the load-
//! bearing half of that ruling is a negative: qd's log holds send *intent*, qw's
//! holds *delivery and terminal*, and **qd does not read qw's**. A ruling stated
//! as "we no longer do X" survives exactly as long as nobody does X again, so it
//! is measured here rather than remembered.
//!
//! # What counts as naming it
//!
//! [`LEDGER_TOKENS`] — `events_path`, `events_dir`, `read_merged`,
//! `parse_events`, `EventWriter::for_key`. Every one of them is a symbol whose
//! subject is `<state>/sessions/<key>.events.jsonl`: the two path constructors,
//! the two readers, and the writer constructor. Their intent-log twins —
//! `intent_path`, `intent_dir`, `read_intent_merged`, `EventWriter::for_intent` —
//! are deliberately DIFFERENT NAMES rather than a `which_log:` parameter, which
//! is what makes this scan possible at all: a parameter would put the choice of
//! log in a variable, where no source scan could see it.
//!
//! `parse_events` is on the list even though it is the shared vocabulary parser
//! and reads whatever text it is handed. That is the point: it is the function a
//! `read_to_string(events_path(..))` is followed by, and pinning it means the one
//! qd module that legitimately parses ledger lines has to say which tree it read
//! them from.
//!
//! # Ratchet, per file, with a written reason
//!
//! The [`PINS`] table is the `provider_gate::PINS` pattern: an EXACT count per
//! file plus one line saying why that file may name the log. Exactness cuts both
//! ways on purpose — deleting a hit reds too, with a message telling you to
//! ratchet the pin down. A file that is not in the table may contain ZERO hits,
//! so a NEW qd verb cannot start reading qw's log quietly; it has to add a row
//! and write the sentence.
//!
//! Two prefixes are load-bearing, and
//! [`debt_lives_only_in_files_marked_as_debt`] checks them:
//!
//! - `LEDGER DEBT:` — a qd verb still reaching into qw's delivery log. Counts
//!   toward [`LEDGER_DEBT_TOTAL`], which may only go DOWN. Every row names the
//!   work that retires it.
//! - `BY OWNERSHIP:` — this file names the delivery log because the delivery log
//!   is its subject. Exempt from the debt total, still pinned exactly.
//!
//! # Scope
//!
//! This crate's `src/` — the `dispatch` library and the `qd` binary. Not
//! `quorum-qw`: that crate OWNS the delivery log, and counting its writes would
//! be counting the answer rather than the problem.

use std::path::{Path, PathBuf};

// ===========================================================================
// The pinned allowlist.
// ===========================================================================

/// One pinned file: an exact count plus the one-line reason.
struct Pin {
    /// Path relative to this crate's `src/`.
    path: &'static str,
    /// EXACT number of production lines naming a [`LEDGER_TOKENS`] symbol.
    hits: usize,
    /// Why this file may name qw's delivery log. See the module docs for the two
    /// load-bearing prefixes.
    reason: &'static str,
}

/// The allowlist. Anything not here may name the delivery log ZERO times.
const PINS: &[Pin] = &[
    // ---- qd's own intent log ------------------------------------------------
    Pin {
        path: "bin/qd/verbs/intent.rs",
        hits: 1,
        reason: "BY OWNERSHIP: `parse_events` over qd's OWN intent tree. This module is \
                 where the intent log's layout lives — it names `intent_dir`, never \
                 `events_dir` — and it is the only qd file that reads ledger lines at all",
    },
    // ---- the delivery log's legitimate residents ----------------------------
    Pin {
        path: "conformance/harness.rs",
        hits: 5,
        reason: "BY OWNERSHIP: the live-execution conformance harness builds a jail and \
                 asserts against the delivery log it produced; reading the artifact under \
                 test is the job",
    },
    Pin {
        path: "relay_server/mod.rs",
        hits: 2,
        reason: "BY OWNERSHIP: the relay server's RECIPIENT observer is a qw-side writer \
                 that happens to run in qd's relay process — `09-ledger-split.md` assigns \
                 its records to qw's log and it needs no change",
    },
    // ---- the debt -----------------------------------------------------------
    //
    // EMPTY, and the table is where that is recorded. The last two rows were
    // `bin/qd/verbs/lifecycle.rs` (2) and `bin/qd/verbs/send.rs` (1); both are
    // deleted rather than zeroed, because a pin measured at 0 is an allowance
    // nobody is using and the gate above reds on one. What retired them:
    //
    // - `new -p`'s priming send is `quorum_qw::delivery::priming` — the sixth
    //   delivery body, moved by OWNERSHIP rather than by address. It could NOT
    //   become a lane call, and that module's header is where the evidence lives:
    //   `LaneImpl::start_claude_pane` refuses `StartRequest::prompt` on a stated
    //   ruling and returns before the bind phase the `-p` turn runs after, and
    //   `LaneOps::deliver` would re-pick the carrier (relay over pane), restamp
    //   the verb the recovery sweep keys on, and mint terminals on arms this path
    //   deliberately leaves open. The `priming-readiness-timeout` emitter went
    //   with it, still keyed `byname-<name>`. What stayed in `qd start` is what is
    //   qd's: the INTENT record (D11), and `map_deliver_outcome`'s exit contract.
    // - `qd send:pty --wait`'s ZMX anchor loop is
    //   `quorum_qw::delivery::pty::run_anchor_wait`, with its write-free embedded
    //   twin. The `wait`/`raw`/`full` cut stays exactly where phase 3B put it —
    //   the banner, the five closing words and the stdout body are still the
    //   verb's, because a line written in three pieces across 120 seconds cannot
    //   be a `Notes` entry — but everything the loop WROTE crossed with it.
];

/// Hits still sitting in `LEDGER DEBT:` rows. Monotonically non-increasing: each
/// phase that retires a row ratchets this down. `BY OWNERSHIP:` rows are excluded
/// by design — they name the delivery log because it is their subject.
///
/// **9 → 3 → 0.** Ruling D2's `await_idle` took all six of `bin/qd/verbs/wait.rs`'s
/// into `quorum_qw::idle` and `quorum_qw::delivery::pi`; the last three followed
/// the priming send and the `--wait` anchor loop into `quorum_qw::delivery`. Every
/// row that is left says `BY OWNERSHIP:`, and every one of them writes or reads the
/// delivery log because the delivery log is its subject.
///
/// **Zero is not the end of the gate, it is the start of its second job.** Up to
/// here the number was a ratchet on work in progress; from here it is a floor. A
/// new qd verb that reaches into qw's delivery log has to add a row AND raise this
/// constant, which the test below refuses — so the ban is now enforced by the one
/// thing a "we no longer do X" ruling never has on its own: something that fails.
const LEDGER_DEBT_TOTAL: usize = 0;

/// This gate's own source. Its token list is a list of the very symbols it bans,
/// so pinning it would make every edit to the gate a pin update. Excluded by name
/// rather than by a `#[cfg(test)]` accident.
const SELF_PATH: &str = "ledger_gate.rs";

/// The symbols whose subject is qw's delivery log.
///
/// Order matters only for the failure message. `EventWriter::for_key` is spelled
/// with its type because the bare `for_key` would also match a hypothetical
/// unrelated helper, and a gate that reds on the wrong thing gets disabled.
const LEDGER_TOKENS: &[&str] = &[
    "events_path",
    "events_dir",
    "read_merged",
    "parse_events",
    "EventWriter::for_key",
];

/// The prefix marking a row as work still to do.
const DEBT_PREFIX: &str = "LEDGER DEBT:";
/// The prefix marking a row as legitimate and permanent.
const OWNERSHIP_PREFIX: &str = "BY OWNERSHIP:";

// ===========================================================================
// The scanner.
// ===========================================================================

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_sources(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("src/ is readable") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path
                    .strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, path));
            }
        }
    }
    out.sort();
    out
}

/// Text before the file's first `#[cfg(test)]`.
///
/// The same crude cut `lane::gate` takes, safe in the same direction: cutting at
/// the FIRST marker can only make the scan see LESS than the whole file, never
/// more, so it cannot produce a false failure. A test reading a jail's delivery
/// log to assert what a verb wrote is legitimate — that is how most of this
/// crate's ledger proofs work.
fn production_half(src: &str) -> &str {
    match src.find("#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    }
}

/// The production lines of `src` that name a delivery-log symbol, as
/// `(line_number, trimmed_text)`.
///
/// Line comments are skipped — including the tombstones that name the retired
/// reads in prose, which is most of what survives a migration like this one. A
/// line is counted ONCE however many tokens it carries; the unit is "a place that
/// touches the log", not "a mention".
fn ledger_hits(src: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (n, line) in production_half(src).lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        if LEDGER_TOKENS.iter().any(|t| line.contains(t)) {
            out.push((n + 1, line.trim().to_string()));
        }
    }
    out
}

fn pin_for(rel: &str) -> Option<&'static Pin> {
    PINS.iter().find(|p| p.path == rel)
}

// ===========================================================================
// The gate.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate. Every file's measured count must equal its pin, and a file with
    /// no pin must have no hits.
    #[test]
    fn only_pinned_files_name_qws_delivery_log() {
        let root = src_root();
        let mut measured: Vec<(String, usize, Vec<(usize, String)>)> = Vec::new();

        for (rel, path) in rust_sources(&root) {
            if rel == SELF_PATH {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("source is readable UTF-8");
            let hits = ledger_hits(&src);
            if !hits.is_empty() {
                measured.push((rel, hits.len(), hits));
            }
        }

        let mut problems: Vec<String> = Vec::new();
        for (rel, n, hits) in &measured {
            match pin_for(rel) {
                None => problems.push(format!(
                    "{rel}: {n} unpinned reference(s) to qw's delivery log — this file is \
                     not in PINS, so it may contain none:\n      {}",
                    hits.iter()
                        .map(|(l, t)| format!("{rel}:{l}: {t}"))
                        .collect::<Vec<_>>()
                        .join("\n      ")
                )),
                Some(p) if p.hits != *n => problems.push(format!(
                    "{rel}: pinned at {} reference(s), measured {n}{}",
                    p.hits,
                    if *n < p.hits {
                        " — a reference was RETIRED; ratchet the pin down (and \
                         LEDGER_DEBT_TOTAL with it if this is a debt row)"
                    } else {
                        " — a NEW reference to qw's delivery log was added"
                    }
                )),
                Some(_) => {}
            }
        }
        // A pin whose file has gone to zero (or gone away) must be deleted, not
        // left standing: a stale row is an allowance nobody is using and the next
        // reader cannot tell it from a live one.
        for p in PINS {
            if !measured.iter().any(|(rel, _, _)| rel == p.path) {
                problems.push(format!(
                    "{}: pinned at {} reference(s), measured 0 — delete the row",
                    p.path, p.hits
                ));
            }
        }

        assert!(
            problems.is_empty(),
            "qd's view of the DELIVERY ledger drifted from its pins:\n  {}\n\n\
             `09-ledger-split.md`: the ledger is two logs and qd does not read qw's. \
             qd's own records go through `dispatch::events`'s intent surface — \
             `intent_path`, `intent_dir`, `read_intent_merged`, \
             `ReaderCtx::read_intent`, `EventWriter::for_intent` — and anything qd \
             needs from the far side crosses as `LaneOps::resolved`, \
             `LaneOps::await_terminal`, `LaneOps::await_idle` or \
             `LaneOps::recover`.\n\n\
             Measured table, paste-ready:\n{}",
            problems.join("\n  "),
            measured
                .iter()
                .map(|(rel, n, _)| format!("    {rel}: {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    /// Debt may shrink but never spread: only rows that say `LEDGER DEBT:` may
    /// hold debt, and their sum is pinned.
    #[test]
    fn debt_lives_only_in_files_marked_as_debt() {
        for p in PINS {
            assert!(
                p.reason.starts_with(DEBT_PREFIX) || p.reason.starts_with(OWNERSHIP_PREFIX),
                "{}: a pin's reason must start with {DEBT_PREFIX:?} (work still to do) or \
                 {OWNERSHIP_PREFIX:?} (this file's subject IS the delivery log). Got: {}",
                p.path,
                p.reason
            );
        }
        let debt: usize = PINS
            .iter()
            .filter(|p| p.reason.starts_with(DEBT_PREFIX))
            .map(|p| p.hits)
            .sum();
        assert_eq!(
            debt, LEDGER_DEBT_TOTAL,
            "qd verbs still holding a delivery-log reference: {debt}, pinned at \
             {LEDGER_DEBT_TOTAL}. This number may only go DOWN, and it is now ZERO — \
             every write and every read of qw's delivery log is in `quorum-qw`. A new \
             `LEDGER DEBT:` row is a qd verb reaching back across the split, so adding \
             one means either moving the body into `quorum-qw` (the shape \
             `quorum_qw::delivery` and `quorum_qw::idle` established) or making the \
             ledger question a `LaneOps` call."
        );
    }

    /// **The anti-vacuity test.** Everything above passes trivially if the
    /// scanner stops matching — a broken comment cut, a token list that no longer
    /// spells a real symbol — so the scanner is exercised against text whose
    /// answer is known.
    ///
    /// `lane::gate` needed the same guard for the same reason, and it is the half
    /// of a source-scan gate that is easy to leave out: a gate that cannot fail
    /// is indistinguishable from a gate that passes.
    #[test]
    fn the_scanner_actually_matches() {
        let fixture = "\
fn a() {\n\
    let p = dispatch::events::events_path(&state_dir, key);\n\
    // events_path in a comment is prose, not a read\n\
    let r = dispatch::events::parse_events(&text).records;\n\
    let w = EventWriter::for_key(&state_dir, key, None, None);\n\
    let i = dispatch::events::intent_path(&state_dir, key);\n\
    let j = EventWriter::for_intent(&state_dir, key, None, None);\n\
}\n\
#[cfg(test)]\n\
mod t {\n\
    let hidden = dispatch::events::read_merged(&state_dir, None, None);\n\
}\n";
        let hits = ledger_hits(fixture);
        let lines: Vec<usize> = hits.iter().map(|(l, _)| *l).collect();
        assert_eq!(
            lines,
            vec![2, 4, 5],
            "the scanner must catch the three real reads/writes, skip the comment, \
             skip BOTH intent-log twins, and stop at #[cfg(test)]; got {hits:?}"
        );
    }

    /// The token list must keep naming symbols that exist. A rename in
    /// `quorum-qw` would otherwise leave this gate green and blind — the failure
    /// mode a source scan is most exposed to, because nothing about it is
    /// type-checked.
    #[test]
    fn the_tokens_still_name_real_symbols() {
        let events = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../quorum-qw/src/events.rs")
            .canonicalize()
            .expect("quorum-qw/src/events.rs is where the ledger surface lives");
        let src = std::fs::read_to_string(&events).expect("readable");
        for tok in LEDGER_TOKENS {
            // `EventWriter::for_key` is defined as `fn for_key`; the rest are
            // free functions defined as `fn <name>`.
            let sym = tok.rsplit("::").next().unwrap();
            assert!(
                src.contains(&format!("fn {sym}(")),
                "ledger token {tok:?} no longer names a function in \
                 quorum-qw/src/events.rs — the gate is scanning for a symbol that was \
                 renamed or deleted, which makes it silently vacuous"
            );
        }
        // And the intent twins the failure message tells people to use must exist
        // too, or the advice is a dead end.
        for sym in ["intent_path", "intent_dir", "read_intent_merged", "for_intent"] {
            assert!(
                src.contains(&format!("fn {sym}(")),
                "the intent-log twin {sym:?} is gone from quorum-qw/src/events.rs"
            );
        }
    }
}
