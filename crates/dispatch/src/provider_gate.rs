//! The stage-2 gate: a source scan that pins where provider strings may appear,
//! and drives *routing* on them to zero.
//!
//! # Why a naive literal count is the WRONG measure
//!
//! `02-qw-split.md` set the success criterion as "zero provider string literals
//! outside the lane module". Measured honestly that criterion can never be met,
//! and chasing it would damage the codebase: the only way to reach zero is to
//! obfuscate the strings that legitimately name providers, which trades a
//! greppable fact for a hidden one. `06-stage2-plan.md` corrects it. Provider
//! literals fall into three buckets that must be judged differently:
//!
//! 1. **Enumeration by purpose.** Naming a provider IS the job here. The
//!    conformance harness spends 67 literals building `qd start --provider codex`
//!    ARGV for a live-execution harness; `conformance/ids.rs` spells the five
//!    lane ids for serde; `render.rs` writes one into a synthesised JSON row.
//!    Every one of those is a provider named as *data* — an argv token, a wire
//!    spelling, a manifest row, a default. None of them decides anything. These
//!    STAY.
//! 2. **The provider module itself.** `provider.rs` and `provider/` are where
//!    per-provider knowledge is *supposed* to live. They now sit in `quorum-qw`,
//!    outside the tree this gate scans, which is why this crate's pin table has
//!    no bucket-2 rows — the qd/qw split moved bucket 2 out of reach by
//!    construction, and that is the strongest form the allowlist could take.
//! 3. **Routing.** Deciding *what to do* from a provider string: which revive,
//!    which carrier, which kill strategy, which status source. **This is the
//!    target, and this is what goes to zero.**
//!
//! The distinction is mechanical, which is what makes it gate-able: a routing hit
//! sits in a COMPARISON position — `== "codex"`, `!= "claude-code"`,
//! `.starts_with("acp/")`, or a `match` arm head — while an enumeration hit sits
//! in an ARGUMENT, LITERAL or DEFAULT position. [`classify`] implements exactly
//! that rule, and [`classifier_separates_routing_from_enumeration`] pins its
//! behaviour on hand-written examples so the classifier itself cannot drift.
//!
//! # Ratchet, not a bare classifier
//!
//! The classifier alone would be a gate with a blind spot: routing can be written
//! in a shape the regex does not know (a `HashMap` keyed by provider id, a helper
//! that takes the string and returns an enum). So this gate pins BOTH numbers per
//! file — total provider literals AND the routing subset — as EXACT counts. A new
//! literal of any shape reds the build; so does a new file that mentions a
//! provider at all. Exactness cuts both ways on purpose: deleting routing also
//! reds, with a message telling you to ratchet the pin DOWN. A ratchet that is
//! honest about what it cannot classify beats a classifier that quietly passes.
//!
//! Bucket-3 rows carry a `ROUTING DEBT:` reason and are the work list for the
//! remaining stage-2 phases; [`routing_lives_only_in_files_marked_as_debt`]
//! refuses to let routing appear anywhere else, so debt can shrink but never
//! spread.
//!
//! # Where shape and purpose disagree
//!
//! Two files compare against provider ids *as their job*: `conformance/tier.rs`
//! IS the provider-id -> `Lane` parse table, and `conformance/harness.rs` picks
//! which real lane script to run. Shape says bucket 3; purpose says bucket 1, and
//! purpose wins — a parse table that may not compare against the thing it parses
//! is not a parse table. Those rows say `ENUMERATION BY PURPOSE:` and say why.
//! The escape hatch is deliberately expensive: it is per FILE, it must be written
//! out, and the count stays exact, so using it to smuggle in a verb branch means
//! writing a false sentence next to the branch. That is the honest failure mode —
//! a gate cannot stop a determined author, only make the evasion legible.
//!
//! # Scope
//!
//! This crate's `src/` — the `dispatch` library and the `qd` binary, i.e. every
//! VERB. Deliberately not `quorum-qw`: that crate IS the provider and lane
//! implementation, and counting literals there would be counting the answer, not
//! the problem.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ===========================================================================
// The pinned allowlist.
// ===========================================================================

/// One pinned file: exact counts plus the one-line reason the plan requires.
struct Pin {
    /// Path relative to this crate's `src/`.
    path: &'static str,
    /// EXACT number of provider-id string literals in production code.
    literals: usize,
    /// How many of those sit in a routing (comparison / match-arm) position.
    routing: usize,
    /// Why this file may name providers — one line, per the plan. Two prefixes
    /// are load-bearing and checked by [`routing_lives_only_in_files_marked_as_debt`]:
    ///
    /// - `ROUTING DEBT:` — bucket 3. A verb deciding behaviour from a provider
    ///   string. Counts toward [`ROUTING_DEBT_TOTAL`]; retiring it is the
    ///   remaining stage-2 work.
    /// - `ENUMERATION BY PURPOSE:` — bucket 1 wearing bucket-3 SHAPE. The file's
    ///   job IS to map a provider id onto the thing that names it, so it compares
    ///   against provider strings on purpose and forever. Exempt from the debt
    ///   total, still pinned to an exact count so it cannot grow quietly.
    ///
    /// Any other reason text asserts `routing == 0`.
    reason: &'static str,
}

/// The allowlist. Anything not here may contain ZERO provider literals.
///
/// Grouped by layer so the shape of the debt is readable. The failure message
/// prints a path-sorted, paste-ready version of the measured table, so a
/// legitimate change is a copy rather than a hand recount.
const PINS: &[Pin] = &[
    // ---- library ------------------------------------------------------------
    Pin {
        path: "join.rs",
        literals: 13,
        routing: 5,
        reason: "ROUTING DEBT: status/turns derivation and the acp/ row class; the other 8 \
                 are cold-row synthesis defaults, pure data — see 10-join-split.md, \
                 deliberately not started",
    },
    Pin {
        path: "relay_server/mod.rs",
        literals: 1,
        routing: 0,
        reason: "`command_exists(\"claude\")` — the EXECUTABLE on PATH, not a provider id",
    },
    Pin {
        path: "relay_server/register.rs",
        literals: 4,
        routing: 0,
        reason: "`exec.run(\"claude\", …)` argv building the `claude mcp add/remove` \
                 registration calls",
    },
    Pin {
        path: "render.rs",
        literals: 1,
        routing: 0,
        reason: "the `provider` field's default value in a synthesised JSON row — data, not a \
                 branch",
    },
    Pin {
        path: "resolve.rs",
        literals: 1,
        routing: 1,
        reason: "ROUTING DEBT: `acp_floor_original` filters the acp/ row class out of a \
                 name collision — an acp-shaped rule inside the generic resolver",
    },
    Pin {
        path: "setup/harness.rs",
        literals: 8,
        routing: 0,
        reason: "`qd setup`'s HARNESS-DETECTION table (R15/C2): 4 are the canonical id \
                 spellings in `HarnessId::as_str` — the one place each name is written, \
                 which `program`/`label` derive from — and 4 are `pi` as a FILENAME in \
                 npm-global install paths (C5). Detection asks \"is this program on this \
                 machine\", never \"which lane do I open\": no verb branches on any of \
                 these, and lane/provider selection stays behind `LaneOps`",
    },
    // ---- conformance: bucket 1, naming providers IS the job -----------------
    Pin {
        path: "conformance/harness.rs",
        literals: 67,
        routing: 8,
        reason: "ENUMERATION BY PURPOSE: a live-execution harness — 59 of these are \
                 `qd start --provider <id>` ARGV and per-lane probe arguments, and the 8 \
                 comparison-shaped ones pick which real lane script to run or assert a \
                 provider field in captured output",
    },
    Pin {
        path: "conformance/ids.rs",
        literals: 10,
        routing: 0,
        reason: "the `Lane` wire spellings: five `#[serde(rename)]` and their five `as_str` \
                 mirrors — the canonical id table itself",
    },
    Pin {
        path: "conformance/tier.rs",
        literals: 6,
        routing: 6,
        reason: "ENUMERATION BY PURPOSE: the provider-id -> `Lane` parse table; comparing \
                 against every id is what a parse table IS",
    },
    Pin {
        path: "conformance/tier_doc.rs",
        literals: 5,
        routing: 0,
        reason: "the documented lane-id list rendered into the tier doc — a coverage manifest \
                 row per lane",
    },
    Pin {
        path: "conformance/tier_tests.rs",
        literals: 3,
        routing: 0,
        reason: "a whole-file test module (`#[cfg(test)] mod tier_tests;` in conformance/mod.rs), \
                 so its fixtures are invisible to the in-file cfg(test) skip",
    },
    // ---- the qd binary ------------------------------------------------------
    Pin {
        path: "bin/qd/help.rs",
        literals: 1,
        routing: 0,
        reason: "`help::provider_list` renders `Harness::ALL` for the help table and spells                  Opencode with its CLI alias — the set comes from qw, only the spelling is here",
    },
    Pin {
        path: "bin/qd/main.rs",
        literals: 3,
        routing: 0,
        reason: "the default `provider` field on a synthesised row, plus two \
                 `real_command_exists(&exec, \"claude\")` PATH probes",
    },
    Pin {
        path: "bin/qd/verbs/adopt.rs",
        literals: 2,
        routing: 2,
        reason: "ROUTING DEBT: bare-process discovery by cmdline, and a claude-only adopt gate",
    },
    Pin {
        path: "bin/qd/verbs/attach.rs",
        literals: 3,
        routing: 3,
        reason: "ROUTING DEBT: the Cold-outcome dispatch — plan phase 5's `attach_plan`, which \
                 `contract.rs` deliberately did not build (see the checklist note)",
    },
    Pin {
        path: "bin/qd/verbs/bootstrap.rs",
        literals: 1,
        routing: 0,
        reason: "`real_command_exists(&exec, \"claude\")` — a PATH probe for the executable",
    },
    Pin {
        path: "bin/qd/verbs/common.rs",
        literals: 2,
        routing: 2,
        reason: "ROUTING DEBT: `refuse_unknown_provider`'s allow-list arm decides refuse-vs-\
                 proceed for five verbs",
    },
    Pin {
        path: "bin/qd/verbs/lifecycle.rs",
        literals: 4,
        routing: 3,
        reason: "ROUTING DEBT: RATCHETED 7/6 -> 4/3. The create if-chain is GONE: `qd start` routes with \
                 `Lane::for_create` and creates with `LaneOps::start`, so the five ordered \
                 branches whose order was enforced only by a comment are one table in \
                 `quorum_qw::lanes`. What is left is three REFUSALS that must fire before any \
                 create is attempted — `--interactive` on acp/*, `--fork` on codex, and the \
                 `--provider` default — plus the acp mask on the lane's interactive flag, \
                 which reproduces the retired chain's own behaviour and is commented as such",
    },
    Pin {
        path: "bin/qd/verbs/ls.rs",
        literals: 10,
        routing: 9,
        reason: "ROUTING DEBT: liveness gates and the two duplicated provider-badge tables — \
                 plan phase 1's `list()`, still on the old path",
    },
    Pin {
        path: "bin/qd/verbs/resume.rs",
        literals: 1,
        routing: 1,
        reason: "ROUTING DEBT: one claude-only arm left of the six revive routes; the rest now \
                 go through `LaneOps::wake`",
    },
    // `bin/qd/verbs/send.rs` had a row here — 1 literal, 0 routing, the
    // `provider_for("claude-code")` transcript-path default. It went to zero when
    // stage-3 phase 3B moved `run_send_pty_resolved`'s delivery half into
    // `quorum_qw::delivery::pty`; what is left in that file is the `--wait`
    // reply-capture shell, which names no provider at all. Deleted per the gate's
    // own STALE PIN rule. Routing debt is unmoved (the row carried none).
    Pin {
        path: "bin/qd/verbs/send_relay.rs",
        literals: 12,
        routing: 10,
        reason: "ROUTING DEBT: the `send:relay` verb's own per-provider fan-out plus \
                 `provider_uses_relay_fast_path`'s duplicate classification. The four \
                 carrier BODIES moved to `quorum_qw::delivery` (stage-3 phase 3B), which \
                 took their 4 enumeration literals with them; the fan-out that CHOOSES \
                 between them is the verb's own and is what stays to be retired",
    },
    Pin {
        path: "bin/qd/verbs/send_unified.rs",
        literals: 1,
        routing: 1,
        reason: "ROUTING DEBT: one codex arm left; `select_carrier`, `RealWaker`, \
                 `dispatch_selected`, `UnifiedCarrier` and `RealUnifiedBackend` are \
                 already gone",
    },
    Pin {
        path: "bin/qd/verbs/update.rs",
        literals: 1,
        routing: 0,
        reason: "`real_command_exists(exec, \"claude\")` — a PATH probe for the executable",
    },
    Pin {
        path: "bin/qd/verbs/wait.rs",
        literals: 2,
        routing: 1,
        reason: "ROUTING DEBT: the codex ENTRY gate. `qd wait`'s four turn-completion \
                 strategies became `LaneOps::await_idle` (ruling D2), which routes on the \
                 LANE — 5 routing branches to 1. The survivor reads the JOIN's \
                 `session.status`, which for a codex row is the gather's rollout-tail fold \
                 and is qd's alone; retired if the join ever crosses the boundary",
    },
];

/// Routing hits still in the verb layer, summed over the `ROUTING DEBT:` rows
/// only. Monotonically non-increasing: each stage-2 phase that retires a row
/// ratchets this down. `ENUMERATION BY PURPOSE:` rows are excluded by design —
/// they compare against provider ids because that is their job.
///
/// 41 -> 38 when `qd start`'s create became a `LaneOps::start` call: the
/// five-arm ordered if-chain that routed `--provider`/`--interactive` to a
/// per-lane create wrapper is `Lane::for_create` now, and its three
/// provider-string branches went with it.
const ROUTING_DEBT_TOTAL: usize = 38;

/// The gate's own source: its token list and its classifier fixtures are provider
/// literals by necessity, and pinning them would make every edit to the gate a
/// pin update. Excluded by name rather than by a `#[cfg(test)]` accident.
const SELF_PATH: &str = "provider_gate.rs";

/// Provider ids, plus the `acp/` family prefix that `starts_with` routing keys
/// on. `"claude"` is the claude-code EXECUTABLE name rather than a provider id;
/// it is counted anyway, because `program != Some("claude")` is a routing branch
/// in every sense that matters and hiding it would defeat the gate.
const PROVIDER_TOKENS: &[&str] = &[
    "claude",
    "claude-code",
    "codex",
    "pi",
    "opencode",
    "acp/claude-code",
    "acp/opencode",
    "acp/",
];

// ===========================================================================
// The scanner.
// ===========================================================================

/// Where a provider literal sits, and therefore which bucket it falls in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Comparison or match-arm position — bucket 3, the target.
    Routing,
    /// Argument / literal / default position — bucket 1, allowed to stay.
    Enumeration,
}

/// One counted literal.
#[derive(Debug)]
struct Hit {
    line: usize,
    kind: Kind,
    /// The source line, trimmed — for the failure message.
    text: String,
}

/// A byte-for-byte mask of a Rust source: comments and string CONTENTS replaced
/// by spaces, quotes and code kept. Brace counting and prefix/suffix inspection
/// are only safe on this, never on the raw text — a `{` inside a doc comment or a
/// `"` inside a string would otherwise steer the scan.
struct Masked {
    /// Same length as the input, same UTF-8 validity (whole bytes are blanked).
    text: String,
    /// `(open_quote_offset, after_close_quote_offset, content)` per string literal.
    literals: Vec<(usize, usize, String)>,
}

/// Mask comments and string bodies in one pass, recording the string literals.
///
/// Handles what this codebase actually contains: `//` and nested `/* */`, raw
/// strings with any hash count, byte strings, escapes, and the `'a` lifetime vs
/// `'x'` char-literal ambiguity (a lifetime must NOT open a quote state).
fn mask(src: &str) -> Masked {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut literals = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                while i < b.len() && b[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                let mut depth = 1usize;
                out[i] = b' ';
                out[i + 1] = b' ';
                i += 2;
                while i < b.len() && depth > 0 {
                    if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                        depth += 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                    } else if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                        depth -= 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                    } else {
                        out[i] = b' ';
                        i += 1;
                    }
                }
            }
            b'r' | b'b' if raw_prefix(b, i).is_some() => {
                let (hashes, start) = raw_prefix(b, i).unwrap();
                // start points at the opening quote.
                let open = start;
                let mut j = start + 1;
                let body = j;
                loop {
                    if j >= b.len() {
                        break;
                    }
                    if b[j] == b'"' && b[j + 1..].iter().take(hashes).all(|c| *c == b'#') {
                        break;
                    }
                    j += 1;
                }
                let content = String::from_utf8_lossy(&b[body..j.min(b.len())]).into_owned();
                for k in body..j.min(b.len()) {
                    out[k] = b' ';
                }
                let end = (j + 1 + hashes).min(b.len());
                literals.push((open, end, content));
                i = end;
            }
            b'"' => {
                let open = i;
                let mut j = i + 1;
                while j < b.len() {
                    if b[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if b[j] == b'"' {
                        break;
                    }
                    j += 1;
                }
                let end = (j + 1).min(b.len());
                let content = String::from_utf8_lossy(&b[i + 1..j.min(b.len())]).into_owned();
                for k in i + 1..j.min(b.len()) {
                    out[k] = b' ';
                }
                literals.push((open, end, content));
                i = end;
            }
            b'\'' => {
                // `'x'` / `'\n'` / `'\u{1}'` is a char literal; `'static` is a
                // lifetime. Only the former may contain a quote character.
                if let Some(end) = char_literal_end(b, i) {
                    for k in i + 1..end - 1 {
                        out[k] = b' ';
                    }
                    i = end;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    Masked {
        text: String::from_utf8(out).expect("blanking whole bytes preserves UTF-8"),
        literals,
    }
}

/// If `i` opens a raw/byte string, return `(hash_count, offset_of_open_quote)`.
fn raw_prefix(b: &[u8], i: usize) -> Option<(usize, usize)> {
    // Must not be the tail of an identifier (`for_r`, `b` in `b.len()`).
    if i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') {
        return None;
    }
    let mut j = i;
    if b[j] == b'b' {
        j += 1;
    }
    if j < b.len() && b[j] == b'r' {
        j += 1;
        let mut hashes = 0usize;
        while j < b.len() && b[j] == b'#' {
            hashes += 1;
            j += 1;
        }
        if j < b.len() && b[j] == b'"' {
            return Some((hashes, j));
        }
        return None;
    }
    if j < b.len() && b[j] == b'"' && b[i] == b'b' {
        return Some((0, j));
    }
    None
}

/// End offset (exclusive) of a char literal starting at `i`, or `None` for a
/// lifetime.
fn char_literal_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if j >= b.len() {
        return None;
    }
    if b[j] == b'\\' {
        j += 1;
        while j < b.len() && b[j] != b'\'' {
            j += 1;
        }
        return if j < b.len() { Some(j + 1) } else { None };
    }
    // One char (possibly multi-byte) then a closing quote.
    let mut k = j + 1;
    while k < b.len() && (b[k] & 0xC0) == 0x80 {
        k += 1;
    }
    if k < b.len() && b[k] == b'\'' {
        Some(k + 1)
    } else {
        None
    }
}

/// Byte ranges covered by `#[cfg(test)]` items — a whole inline `mod tests { … }`
/// or a single `mod x;` declaration. Cutting at the FIRST `#[cfg(test)]` (the
/// obvious shortcut) is wrong here: `ls.rs` has two `#[cfg(test)] fn` helpers 400
/// lines above its test module, and `conformance/mod.rs` declares three test-only
/// modules ahead of its `pub use` block.
fn test_regions(masked: &str) -> Vec<(usize, usize)> {
    const MARK: &str = "#[cfg(test)]";
    let b = masked.as_bytes();
    let mut regions = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = masked[from..].find(MARK) {
        let start = from + rel;
        let mut i = start + MARK.len();
        let mut depth = 0usize;
        let mut end = b.len();
        while i < b.len() {
            match b[i] {
                b';' if depth == 0 => {
                    end = i + 1;
                    break;
                }
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        regions.push((start, end));
        from = end.max(start + MARK.len());
    }
    regions
}

/// Routing = the literal is being COMPARED against. Everything else is a provider
/// named as data.
///
/// `prefix`/`suffix` are the masked line text either side of the literal, so a
/// neighbouring string shows up as empty quotes and cannot be mistaken for code.
fn classify(prefix: &str, suffix: &str) -> Kind {
    let p = prefix.trim_end();
    let s = suffix.trim_start();

    // `x.starts_with("acp/")`, `x.eq("codex")` — the receiver is the provider.
    for m in [
        ".starts_with(",
        ".ends_with(",
        ".eq(",
        ".eq_ignore_ascii_case(",
        ".strip_prefix(",
        ".contains(",
    ] {
        if p.ends_with(m) {
            return Kind::Routing;
        }
    }

    // `p == "codex"` / `p != Some("claude")`, and the mirrored `"codex" == p`.
    let mut lhs = p;
    loop {
        let t = lhs.trim_end();
        if let Some(rest) = t.strip_suffix("Some(") {
            lhs = rest;
            continue;
        }
        lhs = t;
        break;
    }
    if lhs.ends_with("==") || lhs.ends_with("!=") {
        return Kind::Routing;
    }
    if s.starts_with("==") || s.starts_with("!=") {
        return Kind::Routing;
    }

    // A match-arm head. Two shapes occur: the arm on its own line (nothing but
    // earlier alternatives to its left), and the inline `match p { "a" => …` /
    // `matches!(p, "a" | "b")` form (an arm opener to its left). Both require a
    // `=>` or a `|` to the RIGHT, which is what keeps `f(x, "codex")` — same
    // trailing comma, no arm — on the enumeration side.
    let own_line = p
        .trim_start()
        .chars()
        .all(|c| c == '"' || c == '|' || c.is_whitespace());
    let inline = p.ends_with('{') || p.ends_with(',') || p.ends_with('|');
    if (own_line || inline) && (s.starts_with("=>") || s.starts_with('|')) {
        return Kind::Routing;
    }
    // The tail alternative of an inline alternation, whose `=>` may be far to the
    // right: `matches!(p, "pi" | "codex")`. Requiring a closing quote before the
    // `|` is what distinguishes it from a closure parameter list (`.map(|x| …`).
    if p.ends_with('|') && p.trim_end_matches('|').trim_end().ends_with('"') {
        return Kind::Routing;
    }

    Kind::Enumeration
}

/// Every production provider literal in one file, classified.
fn scan(src: &str) -> Vec<Hit> {
    let masked = mask(src);
    let regions = test_regions(&masked.text);
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(src.match_indices('\n').map(|(i, _)| i + 1))
        .collect();

    let mut hits = Vec::new();
    for (open, end, content) in &masked.literals {
        if !PROVIDER_TOKENS.contains(&content.as_str()) {
            continue;
        }
        if regions.iter().any(|(a, b)| *open >= *a && *open < *b) {
            continue;
        }
        let line_idx = line_starts.partition_point(|s| *s <= *open) - 1;
        let line_start = line_starts[line_idx];
        let line_end = line_starts
            .get(line_idx + 1)
            .map(|n| n - 1)
            .unwrap_or(src.len());
        let prefix = &masked.text[line_start..*open];
        let suffix = &masked.text[(*end).min(line_end)..line_end];
        hits.push(Hit {
            line: line_idx + 1,
            kind: classify(prefix, suffix),
            text: src[line_start..line_end].trim().to_string(),
        });
    }
    hits
}

/// This crate's `src/`, from the manifest dir — the same anchor
/// `tests/resolve_beyond_cap.rs` uses to walk `src/bin/qd/verbs`.
fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `src/`, keyed by its path relative to `src/`.
fn all_sources(root: &Path) -> BTreeMap<String, PathBuf> {
    let mut found = BTreeMap::new();
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
                    .into_owned();
                found.insert(rel, path);
            }
        }
    }
    found
}

/// `(literals, routing)` per file, for every file that has any.
fn measure() -> BTreeMap<String, (usize, usize, Vec<Hit>)> {
    let root = src_root();
    let mut out = BTreeMap::new();
    for (rel, path) in all_sources(&root) {
        if rel == SELF_PATH {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("source is readable UTF-8");
        let hits = scan(&src);
        if hits.is_empty() {
            continue;
        }
        let routing = hits.iter().filter(|h| h.kind == Kind::Routing).count();
        out.insert(rel, (hits.len(), routing, hits));
    }
    out
}

// ===========================================================================
// The gate.
// ===========================================================================

#[test]
fn provider_literals_match_the_pinned_allowlist() {
    let measured = measure();
    let pinned: BTreeMap<&str, &Pin> = PINS.iter().map(|p| (p.path, p)).collect();

    let mut problems: Vec<String> = Vec::new();

    for (rel, (literals, routing, hits)) in &measured {
        match pinned.get(rel.as_str()) {
            None => problems.push(format!(
                "NEW FILE naming providers: {rel} ({literals} literals, {routing} routing).\n\
                 Classify it: if the literals are enumeration (argv, wire spelling, default), add \
                 a pin with a one-line reason. If any is ROUTING, it belongs behind `LaneOps` \
                 instead.\n{}",
                render(hits)
            )),
            Some(pin) if pin.literals != *literals || pin.routing != *routing => problems.push(
                format!(
                    "PIN DRIFT in {rel}: pinned {}/{} (literals/routing), measured {literals}/{routing}.\n\
                     If the count went UP, the new literal needs justifying — a routing branch \
                     belongs behind `LaneOps`. If it went DOWN, ratchet the pin down.\n{}",
                    pin.literals,
                    pin.routing,
                    render(hits)
                ),
            ),
            Some(_) => {}
        }
    }

    for pin in PINS {
        if !measured.contains_key(pin.path) {
            problems.push(format!(
                "STALE PIN: {} no longer has any provider literal. Delete its row.",
                pin.path
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "provider-literal gate ({} problem(s)).\n\n{}\n\nMeasured table:\n{}",
        problems.len(),
        problems.join("\n\n"),
        table(&measured)
    );
}

/// One reason prefix per bucket, and only bucket 3 may carry debt.
const DEBT: &str = "ROUTING DEBT:";
const BY_PURPOSE: &str = "ENUMERATION BY PURPOSE:";

#[test]
fn routing_lives_only_in_files_marked_as_debt() {
    let unlabelled: Vec<&str> = PINS
        .iter()
        .filter(|p| {
            p.routing > 0 && !p.reason.starts_with(DEBT) && !p.reason.starts_with(BY_PURPOSE)
        })
        .map(|p| p.path)
        .collect();
    assert!(
        unlabelled.is_empty(),
        "these pins hold comparison-shaped provider literals but claim to be plain \
         enumeration. Either the branch belongs behind `LaneOps` (prefix the reason \
         `{DEBT}`) or naming providers IS the file's job (prefix it `{BY_PURPOSE}` and say \
         why): {unlabelled:?}"
    );

    let idle: Vec<&str> = PINS
        .iter()
        .filter(|p| p.routing == 0 && (p.reason.starts_with(DEBT) || p.reason.starts_with(BY_PURPOSE)))
        .map(|p| p.path)
        .collect();
    assert!(
        idle.is_empty(),
        "these pins are labelled for routing they no longer have — drop the prefix: {idle:?}"
    );

    let debt: usize = PINS
        .iter()
        .filter(|p| p.reason.starts_with(DEBT))
        .map(|p| p.routing)
        .sum();
    assert_eq!(
        debt, ROUTING_DEBT_TOTAL,
        "routing debt moved. It may only go DOWN — update ROUTING_DEBT_TOTAL when a stage-2 \
         phase retires a row, and never up to admit a new branch."
    );
}

/// The classifier is the whole gate, so pin its RULE, not just its output: these
/// are the shapes that occur in this tree, one per bucket, hand-labelled.
#[test]
fn classifier_separates_routing_from_enumeration() {
    for (src, want, why) in [
        (
            "if session.provider == \"codex\" { a() } else { b() }",
            Kind::Routing,
            "equality picks a code path",
        ),
        (
            "if session.provider != \"claude-code\" { return 1; }",
            Kind::Routing,
            "inequality picks a code path",
        ),
        (
            "if program != Some(\"claude\") { return; }",
            Kind::Routing,
            "the Option wrapper does not make it data",
        ),
        (
            "if s.provider.starts_with(\"acp/\") { acp() }",
            Kind::Routing,
            "prefix test on the provider is a class branch",
        ),
        (
            "match p {\n    \"claude-code\" | \"opencode\" => None,\n    o => refuse(o),\n}",
            Kind::Routing,
            "a match arm head IS a branch, including its alternatives",
        ),
        (
            "if matches!(p, \"pi\" | \"codex\") { daemon() }",
            Kind::Routing,
            "an inline arm is still an arm",
        ),
        (
            "emit(&name, session, msg, &send_id, \"pi\");",
            Kind::Enumeration,
            "a trailing argument shares the arm's comma but has no `=>` after it",
        ),
        (
            "        \"pi\" => vec![Lane::Pi],",
            Kind::Routing,
            "an arm head on its own line, as the tier parse table writes it",
        ),
        (
            "cmd.args([\"start\", name, \"--provider\", \"codex\"]);",
            Kind::Enumeration,
            "an argv token naming a provider as data",
        ),
        (
            "let p = provider.as_deref().unwrap_or(\"claude-code\");",
            Kind::Enumeration,
            "a default is not a decision about behaviour",
        ),
        (
            "let entry = Session { provider: \"pi\".to_string(), ..d };",
            Kind::Enumeration,
            "a field VALUE — the provider named as the row's data",
        ),
        (
            "if !real_command_exists(&exec, \"claude\") { return 2; }",
            Kind::Enumeration,
            "the literal is the probe's argument; the branch is on the probe's ANSWER",
        ),
        (
            "let t = derive_tier(\"acp/claude-code\", transport.as_deref(), alive);",
            Kind::Enumeration,
            "an argument, even when the call's result is then compared",
        ),
    ] {
        let hits = scan(src);
        assert!(!hits.is_empty(), "no provider literal found in {src:?}");
        // Every literal on the line shares the verdict — a match arm's second
        // alternative is as much a branch as its first.
        for hit in &hits {
            assert_eq!(hit.kind, want, "{why}: {src:?}");
        }
    }
}

/// Comments and test modules are excluded, and the exclusion is not the naive
/// "cut at the first `#[cfg(test)]`" — `ls.rs` has two `#[cfg(test)] fn` helpers
/// 400 lines above its test module, and `conformance/mod.rs` declares three
/// test-only modules ahead of live code.
#[test]
fn scanner_skips_comments_and_test_items() {
    let src = "\
// if p == \"codex\" { }
/// doc: `p == \"pi\"`
/* block \"claude-code\" */
#[cfg(test)]
fn helper() { let _ = \"codex\"; }
fn live() -> bool { p == \"pi\" }
#[cfg(test)]
mod tier_tests;
fn also_live() -> bool { p == \"opencode\" }
#[cfg(test)]
mod tests { const X: &str = \"claude-code\"; }
";
    let hits = scan(src);
    let seen: Vec<(usize, Kind)> = hits.iter().map(|h| (h.line, h.kind)).collect();
    assert_eq!(
        seen,
        vec![(6, Kind::Routing), (9, Kind::Routing)],
        "only the two production lines count; got {hits:#?}"
    );
}

fn render(hits: &[Hit]) -> String {
    hits.iter()
        .map(|h| {
            format!(
                "    {:>5}  {:<11} {}",
                h.line,
                match h.kind {
                    Kind::Routing => "ROUTING",
                    Kind::Enumeration => "enumeration",
                },
                h.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Paste-ready pin rows, so a legitimate change is a copy rather than a recount.
fn table(measured: &BTreeMap<String, (usize, usize, Vec<Hit>)>) -> String {
    measured
        .iter()
        .map(|(rel, (lit, routing, _))| {
            format!(
                "    Pin {{ path: {rel:?}, literals: {lit}, routing: {routing}, reason: \"\" }},"
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
