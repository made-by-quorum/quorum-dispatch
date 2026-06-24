//! M1: parse_zmx_output, TS src/session.ts:293-323.
//!
//! Port of parseZmxOutput: parse the tab-separated `key=value` lines `zmx list`
//! emits into [`MuxSession`] rows. A leading "→" or "->" marks the CURRENT
//! session. Lines without a `name=` field are dropped (TS only pushes when
//! `fields.name` is truthy).

use crate::mux::MuxSession;

/// Port of parseZmxOutput, session.ts:293-323.
///
/// Each non-blank line is split on TABs into `key=value` parts; the current-marker
/// ("→"/"->") prefix is stripped from the first part. A line is emitted only if it
/// carries a `name=` (TS `if (fields.name)`), so garbage / header / blank lines are
/// silently skipped — matching the permissive-parse discipline (L8): never panic on
/// junk input from an external tool.
pub fn parse_zmx_output(text: &str) -> Vec<MuxSession> {
    let mut sessions = Vec::new();
    // TS: text.trim().split("\n") — trims the whole blob, then splits on '\n'.
    for line in text.trim().split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        // current = line.startsWith("→") || line.startsWith("->")
        let current = line.starts_with('\u{2192}') || line.starts_with("->");

        let mut fields: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for part in line.split('\t') {
            // cleaned = part.replace(/^→\s*|^->\s*/, "").trim()
            let cleaned = strip_current_marker(part).trim();
            // eq = cleaned.indexOf("="); if (eq > 0) { key = [0,eq), val = (eq+1,..] }
            // eq > 0 (not >= 0): a leading "=foo" with empty key is rejected, matching TS.
            if let Some(eq) = cleaned.find('=') {
                if eq > 0 {
                    fields.insert(cleaned[..eq].to_string(), cleaned[eq + 1..].to_string());
                }
            }
        }

        // Only emit when name is present AND non-empty (TS `if (fields.name)` —
        // an empty-string name is falsy in JS, so it is dropped too).
        let Some(name) = fields.get("name").filter(|n| !n.is_empty()) else {
            continue;
        };

        sessions.push(MuxSession {
            name: name.clone(),
            // parseInt(fields.pid || "0"): empty/missing → "0" → 0.
            pid: parse_int_or_zero(fields.get("pid")),
            clients: parse_int_or_zero(fields.get("clients")).max(0) as u32,
            created: parse_int_or_zero(fields.get("created")) as i64,
            start_dir: fields.get("start_dir").cloned().unwrap_or_default(),
            cmd: fields.get("cmd").cloned().unwrap_or_default(),
            current,
            // socket_dir is tagged by the cross-dir scan (mux.rs), NOT the raw line
            // (session.ts:236 maps it on AFTER parseZmxOutput).
            socket_dir: None,
            // ended/exit_code: TS `fields.x ? parseInt(fields.x) : undefined` — present
            // (non-empty) → parsed, else None. A present-but-garbage value parses to
            // NaN in JS; here it becomes None (Rust has no NaN integer), a faithful
            // "couldn't read a number" outcome — documented divergence, edge-only.
            ended: parse_int_present(fields.get("ended")).map(|n| n as i64),
            exit_code: parse_int_present(fields.get("exit_code")),
            // zmxStatus = fields.status (may be undefined → None).
            zmx_status: fields.get("status").cloned(),
            err: fields.get("err").cloned(),
        });
    }
    sessions
}

/// Strip a leading "→" (optionally followed by whitespace) or "->" (optionally
/// followed by whitespace), mirroring the TS regex `/^→\s*|^->\s*/`.
fn strip_current_marker(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix('\u{2192}') {
        return rest.trim_start();
    }
    if let Some(rest) = s.strip_prefix("->") {
        return rest.trim_start();
    }
    s
}

/// JS `parseInt(x || "0")` for the required numeric fields (pid/clients/created).
/// Missing/empty → 0. Otherwise parse the leading integer prefix (optional sign +
/// leading ASCII decimal digits), exactly like `parseInt("12abc") === 12`. A value
/// with no leading digits is NaN in JS; we return 0 (the nearest non-NaN integer,
/// since these fields are non-optional i32/u32) — an edge case that does not occur
/// in well-formed zmx output.
fn parse_int_or_zero(v: Option<&String>) -> i32 {
    match v.map(String::as_str) {
        None | Some("") => 0,
        Some(s) => parse_int_prefix(s).unwrap_or(0),
    }
}

/// JS `fields.x ? parseInt(fields.x) : undefined` for the optional numeric fields.
/// Absent or empty → None. Present → the leading-integer-prefix parse, or None if
/// there is no leading integer (JS NaN, faithfully mapped to None here).
fn parse_int_present(v: Option<&String>) -> Option<i32> {
    match v.map(String::as_str) {
        None | Some("") => None,
        Some(s) => parse_int_prefix(s),
    }
}

/// Parse the leading integer prefix the way JS `parseInt(str, 10)` does for the
/// shapes zmx emits: skip leading ASCII whitespace, accept one optional `+`/`-`,
/// then consume leading ASCII decimal digits; the value is those digits. Returns
/// None when no digit follows (JS would yield NaN).
///
/// NOTE: full JS parseInt also honors a `0x` hex prefix and other radix quirks;
/// zmx numeric fields are plain base-10 integers, so we deliberately do NOT
/// replicate hex parsing — it would mis-read a (nonexistent) `0x..` pid. Scoped to
/// the real input shape.
fn parse_int_prefix(s: &str) -> Option<i32> {
    let s = s.trim_start();
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut neg = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        neg = bytes[i] == b'-';
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None; // no digits → NaN in JS
    }
    // Parse the digit run; saturate on overflow (JS would carry full precision as a
    // float, but zmx pids/timestamps fit comfortably — saturation only guards junk).
    let digits = &s[start..i];
    let mag: i64 = digits.parse().unwrap_or(i64::MAX);
    let signed = if neg { -mag } else { mag };
    Some(signed.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real-looking multi-line `zmx list` blob: a current session, a healthy
    // detached one, an ended task, an unreachable one, a header/garbage line, and
    // a blank line.
    const FIXTURE: &str = "name=alpha\tpid=1234\tclients=2\tcreated=1700000000\tstart_dir=/home/u/work\tcmd=claude --resume
→ name=beta\tpid=5678\tclients=1\tcreated=1700000100\tstart_dir=/home/u/p\tcmd=claude
name=gamma\tpid=4321\tclients=0\tcreated=1700000200\tstart_dir=/tmp\tcmd=bash\tended=1700000300\texit_code=0

name=delta\tpid=9999\tclients=0\tcreated=1700000400\tstart_dir=/tmp\tcmd=zsh\tstatus=unreachable\terr=cannot reach socket
this is not a session line, no name field here
";

    #[test]
    fn parses_realistic_listing() {
        let out = parse_zmx_output(FIXTURE);
        // 4 named lines; the garbage and blank lines are dropped.
        assert_eq!(out.len(), 4);
        assert_eq!(
            out.iter().map(|z| z.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta", "gamma", "delta"]
        );
    }

    #[test]
    fn fields_parsed_correctly() {
        let out = parse_zmx_output(FIXTURE);
        let alpha = &out[0];
        assert_eq!(alpha.pid, 1234);
        assert_eq!(alpha.clients, 2);
        assert_eq!(alpha.created, 1_700_000_000);
        assert_eq!(alpha.start_dir, "/home/u/work");
        assert_eq!(alpha.cmd, "claude --resume");
        assert!(!alpha.current);
        assert_eq!(alpha.ended, None);
        assert_eq!(alpha.exit_code, None);
        assert_eq!(alpha.zmx_status, None);
        assert_eq!(alpha.err, None);
    }

    #[test]
    fn current_marker_detected() {
        let out = parse_zmx_output(FIXTURE);
        let beta = out.iter().find(|z| z.name == "beta").unwrap();
        assert!(beta.current, "→ prefix marks current");
        // And the marker did NOT bleed into the first field's key.
        assert_eq!(beta.pid, 5678);
    }

    #[test]
    fn ascii_arrow_marker_also_detected() {
        let line = "-> name=x\tpid=7";
        let out = parse_zmx_output(line);
        assert_eq!(out.len(), 1);
        assert!(out[0].current);
        assert_eq!(out[0].pid, 7);
    }

    #[test]
    fn ended_task_carries_ended_and_exit_code() {
        let out = parse_zmx_output(FIXTURE);
        let gamma = out.iter().find(|z| z.name == "gamma").unwrap();
        assert_eq!(gamma.ended, Some(1_700_000_300));
        assert_eq!(gamma.exit_code, Some(0));
    }

    #[test]
    fn unreachable_status_and_err_carried() {
        let out = parse_zmx_output(FIXTURE);
        let delta = out.iter().find(|z| z.name == "delta").unwrap();
        assert_eq!(delta.zmx_status.as_deref(), Some("unreachable"));
        assert_eq!(delta.err.as_deref(), Some("cannot reach socket"));
    }

    #[test]
    fn garbage_and_blank_lines_skipped() {
        // Lines without name= are dropped; no panic.
        let out = parse_zmx_output("\n\nrandom junk\n\tkey=val\nname=\tpid=1\n");
        // "name=" with empty value is falsy in JS → dropped.
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn missing_numeric_fields_default_to_zero() {
        // parseInt(undefined || "0") === 0 for pid/clients/created.
        let out = parse_zmx_output("name=solo\tcmd=claude");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pid, 0);
        assert_eq!(out[0].clients, 0);
        assert_eq!(out[0].created, 0);
        assert_eq!(out[0].start_dir, "");
    }

    #[test]
    fn parse_int_prefix_matches_js_parseint() {
        // Verified against `bun -e 'parseInt("12abc")' === 12` etc.
        assert_eq!(parse_int_prefix("12abc"), Some(12)); // bun: 12
        assert_eq!(parse_int_prefix("007"), Some(7)); // bun: 7
        assert_eq!(parse_int_prefix("  34 "), Some(34)); // bun: 34
        assert_eq!(parse_int_prefix("-5"), Some(-5)); // bun: -5
        assert_eq!(parse_int_prefix("+5"), Some(5)); // bun: 5
        assert_eq!(parse_int_prefix("12.9"), Some(12)); // bun: 12 (stops at '.')
        assert_eq!(parse_int_prefix("abc"), None); // bun: NaN
        assert_eq!(parse_int_prefix(""), None);
    }

    #[test]
    fn garbage_numeric_pid_becomes_zero() {
        // A non-numeric pid → JS NaN → here 0 (documented divergence, edge-only).
        let out = parse_zmx_output("name=x\tpid=notanumber");
        assert_eq!(out[0].pid, 0);
    }
}
