//! qd–qf W7 — `qd ls` FLEET MIRROR reads (READ-ONLY).
//!
//! `qd ls` local behavior is unchanged; W7 adds the ability to READ peers'
//! session snapshots (mover-written `remote/<host>/ls.json`) and surface them
//! ALWAYS annotated with the mirror's STALENESS (`now − witnessed_at`). A dead
//! replication pipeline is therefore visible at the surface you look at: an old
//! mirror shows a large "mirror age".
//!
//! ## Scope (deliberate, TRANSITION §3)
//! - `qd ls --host <h>` reads exactly one mirror (`remote/<h>/ls.json`).
//! - `qd ls --all` ADDITIVELY unions EVERY peer's mirror after the (unchanged)
//!   local uncapped+tombstone dump.
//! - qd does NOT WRITE its own `ls.json` in this pass — that publish is
//!   out-of-scope MOVER machinery (the mover runs `qd ls --json`, wraps the rows
//!   with `host` + `witnessed_at`, and writes the result to peers'
//!   `remote/<myhost>/ls.json`). W7 only DEFINES the format (see
//!   `doc/formats/dispatch-transport-formats.md` §4) and READS peers' mirrors.
//!
//! ## The mirror file (`remote/<host>/ls.json`)
//! ```json
//! { "v": 1, "host": "<host id>", "witnessed_at": <epoch-ms>, "sessions": [ <ls --json row>, … ] }
//! ```
//! A torn / missing / `v != 1` file ⇒ a NAMED refusal, never a panic (the
//! torn-tail-rule sibling for the whole-document JSON mirror: a mirror we cannot
//! trust is refused with a reason, never silently treated as absence-of-rows).

use dispatch::origin_send::Refusal;
use dispatch::paths::QdPaths;
use serde_json::Value;

/// A parsed peer mirror: the peer's `host` id, the snapshot's `witnessed_at`
/// (epoch-ms), and the peer's `qd ls --json` rows (opaque JSON values — the same
/// row shape this host emits, carried verbatim so a future row-schema change on a
/// peer never breaks the reader).
#[derive(Debug)]
pub struct Mirror {
    pub host: String,
    pub witnessed_at: i64,
    pub sessions: Vec<Value>,
}

impl Mirror {
    /// Staleness in ms: `now − witnessed_at`, floored at 0 (a mirror witnessed in
    /// the (skewed) future is not "negatively stale" — clamp so the DuckDB column
    /// and the human header never show a negative age).
    pub fn age_ms(&self, now_ms: i64) -> i64 {
        (now_ms - self.witnessed_at).max(0)
    }
}

/// The `remote/<host>/ls.json` mirror kind and its READ outcomes. Parametrized by
/// the host so refusals name it (`refused{no-fleet-state}` for `--host`, or a
/// `refused{torn-mirror}` for a corrupt file).
///
/// The parse is FAIL-CLOSED on every non-clean outcome (§common-framing torn-tail
/// discipline, lifted to whole-document JSON): a mirror we cannot fully validate
/// is refused with a NAMED reason, never partially read.
pub enum MirrorRead {
    /// The mirror parsed cleanly.
    Ok(Mirror),
    /// The mirror file is ABSENT — for `--host` this is the single-machine
    /// `no-fleet-state` contract (consistent with `qd send --host`); for `--all`
    /// an absent per-host file is simply skipped (a directory can outlive its
    /// file mid-rotation). Carries the host for the refusal.
    Absent { host: String },
    /// The mirror exists but is unreadable (IO error / not valid JSON / not an
    /// object / `v != 1` / missing/wrong-typed `host`/`witnessed_at`/`sessions`).
    /// A NAMED refusal, never a panic.
    Torn { host: String, why: String },
}

impl MirrorRead {
    /// Turn a non-Ok read into the appropriate [`Refusal`] (exit 12). `Absent` ⇒
    /// `refused{no-fleet-state}` (the `--host` single-machine contract, worded to
    /// match `qd send --host`); `Torn` ⇒ `refused{torn-mirror}`. `Ok` ⇒ `None`
    /// (the caller has a `Mirror`).
    pub fn into_refusal(self) -> Result<Mirror, Refusal> {
        match self {
            MirrorRead::Ok(m) => Ok(m),
            MirrorRead::Absent { host } => Err(Refusal::refused(
                "no-fleet-state",
                format!("no mirror for host \"{host}\" — no fleet state for it on this host"),
            )),
            MirrorRead::Torn { host, why } => Err(Refusal::refused(
                "torn-mirror",
                format!("mirror for host \"{host}\" is unreadable: {why}"),
            )),
        }
    }
}

/// READ + PARSE `remote/<host>/ls.json` into a [`MirrorRead`]. Pure over an
/// already-resolved `QdPaths` (the caller owns HOME/QD_HOME resolution) so it is
/// unit-testable against a jailed root. Never panics; never partially reads.
pub fn read_mirror(paths: &QdPaths, host: &str) -> MirrorRead {
    let path = paths.remote_ls_path(host);
    let bytes = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return MirrorRead::Absent {
                host: host.to_string(),
            };
        }
        Err(e) => {
            return MirrorRead::Torn {
                host: host.to_string(),
                why: format!("{e}"),
            };
        }
    };
    parse_mirror(host, &bytes)
}

/// The pure parser (bytes → [`MirrorRead`]), split out so it is directly
/// unit-testable without touching the filesystem. Validates: valid JSON, a
/// top-level object, `v == 1`, a string `host`, an integer `witnessed_at`, and an
/// array `sessions` (each element kept as an opaque `Value`).
pub fn parse_mirror(host: &str, bytes: &str) -> MirrorRead {
    let torn = |why: String| MirrorRead::Torn {
        host: host.to_string(),
        why,
    };
    let value: Value = match serde_json::from_str(bytes) {
        Ok(v) => v,
        Err(e) => return torn(format!("not valid JSON ({e})")),
    };
    let Some(obj) = value.as_object() else {
        return torn("top-level value is not a JSON object".to_string());
    };
    // Version marker (§common-framing): a reader rejecting an unknown `v` refuses
    // with a named reason, never guesses.
    match obj.get("v").and_then(Value::as_i64) {
        Some(1) => {}
        Some(other) => return torn(format!("unsupported mirror version v={other} (expected 1)")),
        None => return torn("missing or non-integer \"v\" field".to_string()),
    }
    let Some(mirror_host) = obj.get("host").and_then(Value::as_str) else {
        return torn("missing or non-string \"host\" field".to_string());
    };
    let Some(witnessed_at) = obj.get("witnessed_at").and_then(Value::as_i64) else {
        return torn("missing or non-integer \"witnessed_at\" field".to_string());
    };
    let Some(sessions) = obj.get("sessions").and_then(Value::as_array) else {
        return torn("missing or non-array \"sessions\" field".to_string());
    };
    MirrorRead::Ok(Mirror {
        host: mirror_host.to_string(),
        witnessed_at,
        sessions: sessions.clone(),
    })
}

/// Enumerate the peer host ids that have a `remote/<host>/` directory. Missing
/// `remote/` (single-machine, no fleet) ⇒ an empty vec (the caller then unions
/// nothing — `--all` stays byte-identical to the local-only dump). Sorted for a
/// deterministic union order across hosts. A non-directory entry under `remote/`
/// is skipped (only real per-host subdirs count).
pub fn peer_hosts(paths: &QdPaths) -> Vec<String> {
    let dir = paths.remote_dir();
    let mut hosts: Vec<String> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        // Absent remote/ ⇒ no peers (single machine): the additive union is a
        // no-op, so `--all` is byte-identical to today.
        Err(_) => Vec::new(),
    };
    hosts.sort();
    hosts
}

/// Annotate one peer session row (an opaque `Value`) with the mirror's identity +
/// staleness columns so the DuckDB `--json` projection can SEE a dead pipeline:
/// `host`, `mirror_witnessed_at` (epoch-ms), `mirror_age_ms` (`now − witnessed_at`).
/// A non-object row (shouldn't happen for a well-formed mirror, but never trust a
/// peer) is returned unchanged rather than dropped.
pub fn annotate_row(mut row: Value, host: &str, witnessed_at: i64, age_ms: i64) -> Value {
    if let Some(obj) = row.as_object_mut() {
        obj.insert("host".to_string(), Value::String(host.to_string()));
        obj.insert("mirror_witnessed_at".to_string(), Value::from(witnessed_at));
        obj.insert("mirror_age_ms".to_string(), Value::from(age_ms));
    }
    row
}

/// Human staleness header for a mirror, e.g.
/// `host peerbox — mirror age 5m12s (witnessed 2024-06-04T10:00:00.000Z)`.
/// `witnessed` is the ISO-8601 form (the same `epoch_ms_to_iso` the rest of qd
/// uses) so a human sees an absolute instant alongside the relative age.
pub fn staleness_header(host: &str, witnessed_at: i64, age_ms: i64) -> String {
    format!(
        "host {host} — mirror age {} (witnessed {})",
        human_age(age_ms),
        dispatch::render::epoch_ms_to_iso(witnessed_at),
    )
}

/// A compact human age (`5m12s`, `3s`, `2h05m`, `1d03h`) from a ms duration. Kept
/// local + simple (this is a header, not the general relative-time formatter):
/// largest two units, floored.
fn human_age(age_ms: i64) -> String {
    let secs = age_ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{:02}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_mirror(host: &str, witnessed_at: i64) -> String {
        format!(
            r#"{{"v":1,"host":"{host}","witnessed_at":{witnessed_at},"sessions":[{{"name":"wk","sessionId":"s1","status":"idle","provider":"claude-code"}}]}}"#
        )
    }

    #[test]
    fn parse_ok_extracts_host_witnessed_and_rows() {
        let m = match parse_mirror("peerbox", &sample_mirror("peerbox", 1_717_495_200_000)) {
            MirrorRead::Ok(m) => m,
            other => panic!("expected Ok, got a non-Ok read: {:?}", refusal_of(other)),
        };
        assert_eq!(m.host, "peerbox");
        assert_eq!(m.witnessed_at, 1_717_495_200_000);
        assert_eq!(m.sessions.len(), 1);
        assert_eq!(m.sessions[0]["name"], "wk");
    }

    #[test]
    fn age_ms_is_now_minus_witnessed_floored_at_zero() {
        let m = Mirror {
            host: "h".into(),
            witnessed_at: 1_000_000,
            sessions: vec![],
        };
        assert_eq!(m.age_ms(1_312_000), 312_000, "now − witnessed");
        // A future witnessed_at (clock skew) clamps to 0, never negative.
        assert_eq!(m.age_ms(999_000), 0, "future witness clamps to 0");
    }

    #[test]
    fn bad_version_is_torn_not_panic() {
        let body = r#"{"v":2,"host":"h","witnessed_at":1,"sessions":[]}"#;
        let why = refusal_why(parse_mirror("h", body));
        assert!(why.contains("version") && why.contains("2"), "why: {why}");
    }

    #[test]
    fn missing_fields_are_torn() {
        // no witnessed_at
        let why = refusal_why(parse_mirror("h", r#"{"v":1,"host":"h","sessions":[]}"#));
        assert!(why.contains("witnessed_at"), "why: {why}");
        // sessions wrong type
        let why = refusal_why(parse_mirror(
            "h",
            r#"{"v":1,"host":"h","witnessed_at":1,"sessions":5}"#,
        ));
        assert!(why.contains("sessions"), "why: {why}");
        // not an object
        let why = refusal_why(parse_mirror("h", "[]"));
        assert!(why.contains("not a JSON object"), "why: {why}");
        // not valid JSON at all
        let why = refusal_why(parse_mirror("h", "{not json"));
        assert!(why.contains("not valid JSON"), "why: {why}");
    }

    #[test]
    fn absent_maps_to_no_fleet_state_refusal() {
        let r = MirrorRead::Absent {
            host: "peerbox".into(),
        }
        .into_refusal()
        .unwrap_err();
        assert_eq!(r.class, "no-fleet-state");
        assert_eq!(r.exit_code(), 12);
        assert!(r.reason.contains("peerbox"), "reason: {}", r.reason);
    }

    #[test]
    fn torn_maps_to_torn_mirror_refusal() {
        let r = refusal_of(parse_mirror("h", "{bad")).unwrap();
        assert_eq!(r.class, "torn-mirror");
        assert_eq!(r.exit_code(), 12);
    }

    #[test]
    fn annotate_row_adds_host_and_staleness_columns() {
        let row = serde_json::json!({"name":"wk","status":"idle"});
        let out = annotate_row(row, "peerbox", 1_000_000, 312_000);
        assert_eq!(out["host"], "peerbox");
        assert_eq!(out["mirror_witnessed_at"], 1_000_000);
        assert_eq!(out["mirror_age_ms"], 312_000);
        // existing columns preserved.
        assert_eq!(out["name"], "wk");
        assert_eq!(out["status"], "idle");
    }

    #[test]
    fn human_age_tiers() {
        assert_eq!(human_age(3_000), "3s");
        assert_eq!(human_age(312_000), "5m12s");
        assert_eq!(human_age(7_500_000), "2h05m");
        assert_eq!(human_age(97_200_000), "1d03h");
    }

    #[test]
    fn staleness_header_names_host_age_and_iso_witness() {
        let h = staleness_header("peerbox", 1_717_495_200_000, 312_000);
        assert!(h.contains("host peerbox"), "{h}");
        assert!(h.contains("5m12s"), "{h}");
        assert!(h.contains("2024-06-04T10:00:00.000Z"), "{h}");
    }

    // --- test helpers ---
    fn refusal_of(r: MirrorRead) -> Option<Refusal> {
        r.into_refusal().err()
    }
    fn refusal_why(r: MirrorRead) -> String {
        refusal_of(r).map(|r| r.reason).unwrap_or_default()
    }
}
