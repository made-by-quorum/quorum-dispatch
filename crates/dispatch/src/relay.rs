//! M4: relay sidecar read + PID-ancestry match.
//!
//! Ports the sidecar half of `getRelayPorts` (TS src/session.ts:159-183) plus the
//! relay→claude PID-ancestry match (src/session.ts:845-873). The HTTP `/health`
//! port-scan FALLBACK (src/session.ts:185-212) is NOT ported here — it goes
//! through the [`crate::effects::RelayProbe`] seam.
//!
//! ADD-5 contract note: the sidecar-file read + the relay `/health` shape + the
//! `zmx list` join is the engine CONTRACT surface; the TRANSPORT (the actual
//! HTTP scan) is a DRIVER. A1 ports the sidecar read and defines the probe seam;
//! A4 implements the real probe (client-of-contract per ADD-5).

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::effects::RelayProbe;
use crate::model::RelayHealth;

/// A resolved reply from the relay `/replies/<id>` endpoint.
///
/// The relay returns JSON shaped either `{ "text": "..." }` (the reply landed)
/// or `{ "error": "..." }` (the session reported a failure). The verb keys on
/// which field is populated (send.ts:455-460): an `error` field → "Error: <e>"
/// exit 1; otherwise print `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayReply {
    /// The reply text (`data.text`), if present.
    pub text: Option<String>,
    /// An error reported by the session (`data.error`), if present.
    pub error: Option<String>,
}

/// Failure classes the [`RelayContract`] surfaces. The send:relay verb's retry
/// policy keys on the CLASS, not the message (ADD-5): only `ConnectionFailed`
/// drives the ≤3-retry loop (send.ts:461-471); `Timeout` ends the wait
/// immediately ("Timed out waiting for reply." exit 1, send.ts:462-465).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayError {
    /// The read budget elapsed before a full response arrived. For
    /// `fetch_reply` this is the long-poll budget (`timeout_ms`); the verb maps
    /// it to the "Timed out" exit, never a retry (the TS `AbortError` branch,
    /// send.ts:462-465).
    Timeout,
    /// Could not establish (or lost) the TCP connection — connect timed out,
    /// connection refused, or the socket dropped mid-exchange. This is the ONLY
    /// class the verb retries (TS's generic `catch` non-abort branch,
    /// send.ts:466-471).
    ConnectionFailed,
    /// A response arrived but could not be parsed (malformed status line /
    /// headers / body, or non-JSON where JSON was required).
    BadResponse,
    /// The server returned a non-2xx HTTP status; the string carries the status
    /// line / body context for the operator.
    ServerError(String),
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayError::Timeout => write!(f, "timed out"),
            RelayError::ConnectionFailed => write!(f, "connection failed"),
            RelayError::BadResponse => write!(f, "bad response"),
            RelayError::ServerError(s) => write!(f, "server error: {s}"),
        }
    }
}

impl std::error::Error for RelayError {}

/// The relay client CONTRACT (ADD-5): the engine surface a transport adapter
/// implements. Tests assert THIS surface only (the sidecar shape, `/message`,
/// `/replies/<id>`, `/health`) so a transport swap survives.
///
/// Ports the three relay HTTP calls the TS makes: `POST /message`
/// (send.ts:413-426), `GET /replies/<id>` (send.ts:444-460), and `GET /health`
/// (the port-scan probe, session.ts:189-204).
pub trait RelayContract {
    /// `POST /message` with JSON `{text, from_session}`; returns `message_id`
    /// from the response body (send.ts:413-426). SHORT read timeout.
    fn send_message(&self, port: u16, text: &str, from_session: &str)
        -> Result<String, RelayError>;

    /// `GET /replies/<id>` long-poll: the read timeout is the caller's full
    /// `timeout_ms` budget (send.ts:444-446 gives the fetch the whole
    /// AbortController window — a uniform short timeout would abort mid-poll).
    fn fetch_reply(
        &self,
        port: u16,
        message_id: &str,
        timeout_ms: u64,
    ) -> Result<RelayReply, RelayError>;

    /// `GET /health` (session.ts:189-204): returns the relay's self-reported
    /// health record. SHORT read timeout (used by the port-scan probe).
    fn health(&self, port: u16, timeout_ms: u64) -> Result<RelayHealth, RelayError>;
}

/// Sidecar half of `getRelayPorts` (src/session.ts:159-183).
///
/// Read every `*.json` in `relay_dir`; each must carry a truthy `port` AND a
/// truthy `sessionId` to count (TS `if (data.port && data.sessionId)`). `pid`
/// defaults to 0 (TS `data.pid || 0`), `status` is always the literal `"ok"`
/// for sidecar entries. Unparseable / non-conforming files are skipped (TS
/// per-file `catch {}`); a missing/unreadable dir yields an empty vec (TS outer
/// `catch {}`).
pub fn read_sidecars(relay_dir: &Path) -> Vec<RelayHealth> {
    let rd = match fs::read_dir(relay_dir) {
        Ok(rd) => rd,
        // Missing/unreadable dir → [] (TS outer catch).
        Err(_) => return Vec::new(),
    };
    let mut relays = Vec::new();
    for dent in rd.flatten() {
        let fname = dent.file_name();
        let fname = fname.to_string_lossy();
        if !fname.ends_with(".json") {
            continue;
        }
        // Per-file catch {}: an unreadable/unparseable sidecar is skipped.
        let Ok(bytes) = fs::read(dent.path()) else {
            continue;
        };
        let Ok(data) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        // TS `if (data.port && data.sessionId)`: both must be present + truthy.
        // port: a non-zero number; sessionId: a non-empty string.
        let port = data.get("port").and_then(|v| v.as_u64());
        let session_id = data.get("sessionId").and_then(|v| v.as_str());
        let (Some(port), Some(session_id)) = (port, session_id) else {
            continue;
        };
        if port == 0 || session_id.is_empty() {
            // JS treats 0 / "" as falsy → skipped by the `&&` guard.
            continue;
        }
        // data.pid || 0 — missing/zero/non-numeric pid defaults to 0.
        let pid = data.get("pid").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        relays.push(RelayHealth {
            session_id: session_id.to_string(),
            port: port as u16,
            pid,
            status: "ok".to_string(),
        });
    }
    relays
}

/// `getRelayPorts` (src/session.ts:159-183): sidecars if any, else the HTTP
/// port-scan fallback through the probe seam.
///
/// TS returns the sidecars when `relays.length > 0`; otherwise falls back to
/// `scanRelayPorts()`. Here the fallback is the injected [`RelayProbe`] (A1
/// fixture-backed; A4 wires the real HTTP scan per ADD-5).
pub fn get_relay_ports(relay_dir: &Path, probe: &dyn RelayProbe) -> Vec<RelayHealth> {
    let sidecars = read_sidecars(relay_dir);
    if !sidecars.is_empty() {
        return sidecars;
    }
    probe.scan()
}

/// Relay→claude PID-ancestry match (src/session.ts:845-873). PURE.
///
/// For each relay, walk UP from `r.pid` for at most 5 levels; EVERY ancestor on
/// the way maps to `r.port` (TS `relayByClaudePid.set(parent, port)` at each
/// step — not just the final one). The walk stops when the parent is missing OR
/// `<= 1` (init/no-parent). A later relay's mapping overwrites an earlier one
/// for a shared ancestor (last-writer-wins, matching the TS `Map.set` order).
///
/// Returns pid → port (the claude pid, or any of its ancestors, → relay port).
pub fn match_by_ancestry(
    relays: &[RelayHealth],
    ppid_map: &HashMap<i32, i32>,
) -> HashMap<i32, u16> {
    let mut relay_by_claude_pid: HashMap<i32, u16> = HashMap::new();
    for r in relays {
        let mut current = r.pid;
        for _ in 0..5 {
            let Some(&parent) = ppid_map.get(&current) else {
                break;
            };
            if parent <= 1 {
                break;
            }
            // EVERY ancestor maps to this relay's port (TS sets on each step).
            relay_by_claude_pid.insert(parent, r.port);
            current = parent;
        }
    }
    relay_by_claude_pid
}

/// A successful fast relay lookup (port of the TS return shape,
/// session.ts:1278-1280): the relay port plus the matched session's id + name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastRelayMatch {
    pub port: u16,
    pub pid: i32,
    pub session_id: String,
    pub name: String,
}

/// One live PID-registry entry the fast lookup matches against (the fields of a
/// TS `PidEntry` it reads: name, sessionId, pid). Built by the bin layer from
/// [`crate::registry::read_entries`].
#[derive(Debug, Clone)]
pub struct PidEntryRef {
    pub name: Option<String>,
    pub session_id: Option<String>,
    pub pid: i32,
}

/// PURE port of `fastRelayLookup` (session.ts:1277-1318): resolve a relay port
/// for a session by name WITHOUT a full `getAllSessions`.
///
/// 1. Match a pid entry by name: EXACT (case-insensitive) first, then PREFIX
///    (session.ts:1283-1286). The first exact match wins; absent that, the first
///    prefix match.
/// 2. Match a relay to that entry by PID ancestry: walk UP from each relay's pid
///    ≤5 levels; if any ancestor == the matched entry's pid, that relay's port
///    is the answer (session.ts:1300-1313). The walk stops at a missing parent
///    or `parent <= 1`.
///
/// Returns `None` when no name matches, no relays exist, or no relay's ancestry
/// reaches the matched entry (the verb then falls back to a full session scan).
///
/// `query` is the user's session query; `ppid_map` is the `ps -eo pid=,ppid=`
/// map (same source the join uses).
pub fn fast_relay_lookup(
    query: &str,
    pid_entries: &[PidEntryRef],
    relays: &[RelayHealth],
    ppid_map: &HashMap<i32, i32>,
) -> Option<FastRelayMatch> {
    let q = query.to_lowercase();

    // Exact (case-insensitive) name match, then prefix (session.ts:1283-1286).
    let matched = pid_entries
        .iter()
        .find(|p| p.name.as_deref().map(|n| n.to_lowercase()) == Some(q.clone()))
        .or_else(|| {
            pid_entries.iter().find(|p| {
                p.name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().starts_with(&q))
            })
        })?;

    // No relays → no port (session.ts:1290).
    if relays.is_empty() {
        return None;
    }

    // Walk each relay's ancestry ≤5 levels; an ancestor == match.pid wins
    // (session.ts:1300-1313).
    for r in relays {
        let mut current = r.pid;
        for _ in 0..5 {
            let Some(&parent) = ppid_map.get(&current) else {
                break;
            };
            if parent <= 1 {
                break;
            }
            if parent == matched.pid {
                return Some(FastRelayMatch {
                    port: r.port,
                    pid: matched.pid,
                    // TS: `match.sessionId` (may be undefined → empty string).
                    session_id: matched.session_id.clone().unwrap_or_default(),
                    // TS: `match.name || query`.
                    name: matched
                        .name
                        .clone()
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| query.to_string()),
                });
            }
            current = parent;
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write(path: &Path, contents: &str) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    // --- read_sidecars ---

    #[test]
    fn reads_valid_sidecar() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join("a.json"),
            r#"{"port":8901,"sessionId":"sess-1","pid":4242}"#,
        );
        let relays = read_sidecars(dir.path());
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0].session_id, "sess-1");
        assert_eq!(relays[0].port, 8901);
        assert_eq!(relays[0].pid, 4242);
        assert_eq!(relays[0].status, "ok");
    }

    #[test]
    fn pid_defaults_to_zero_when_missing() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join("a.json"),
            r#"{"port":8901,"sessionId":"sess-1"}"#,
        );
        let relays = read_sidecars(dir.path());
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0].pid, 0, "missing pid → 0 (TS data.pid || 0)");
    }

    #[test]
    fn skips_missing_port_or_session_id() {
        let dir = tempdir().unwrap();
        // No port.
        write(&dir.path().join("a.json"), r#"{"sessionId":"s"}"#);
        // No sessionId.
        write(&dir.path().join("b.json"), r#"{"port":8901}"#);
        // port 0 (falsy).
        write(&dir.path().join("c.json"), r#"{"port":0,"sessionId":"s"}"#);
        // empty sessionId (falsy).
        write(
            &dir.path().join("d.json"),
            r#"{"port":8901,"sessionId":""}"#,
        );
        assert!(read_sidecars(dir.path()).is_empty());
    }

    #[test]
    fn skips_unparseable_and_non_json_files() {
        let dir = tempdir().unwrap();
        write(&dir.path().join("bad.json"), "{not json");
        write(&dir.path().join("notes.txt"), "ignored");
        write(
            &dir.path().join("good.json"),
            r#"{"port":8901,"sessionId":"s"}"#,
        );
        let relays = read_sidecars(dir.path());
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0].session_id, "s");
    }

    #[test]
    fn missing_dir_is_empty() {
        let dir = tempdir().unwrap();
        assert!(read_sidecars(&dir.path().join("nope")).is_empty());
    }

    // --- get_relay_ports fallback ---

    #[test]
    fn get_relay_ports_returns_sidecars_when_present() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join("a.json"),
            r#"{"port":8901,"sessionId":"s"}"#,
        );
        // Probe would return something different — must NOT be consulted.
        let probe = crate::effects::FixtureRelayProbe(vec![RelayHealth {
            session_id: "from-probe".into(),
            port: 9999,
            pid: 0,
            status: "ok".into(),
        }]);
        let relays = get_relay_ports(dir.path(), &probe);
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0].session_id, "s");
    }

    #[test]
    fn get_relay_ports_falls_back_to_probe_when_no_sidecars() {
        let dir = tempdir().unwrap();
        let probe = crate::effects::FixtureRelayProbe(vec![RelayHealth {
            session_id: "from-probe".into(),
            port: 9999,
            pid: 7,
            status: "ok".into(),
        }]);
        let relays = get_relay_ports(dir.path(), &probe);
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0].session_id, "from-probe");
    }

    // --- match_by_ancestry ---

    fn relay(pid: i32, port: u16) -> RelayHealth {
        RelayHealth {
            session_id: format!("s-{pid}"),
            port,
            pid,
            status: "ok".into(),
        }
    }

    #[test]
    fn maps_every_ancestor_up_to_five_levels() {
        // relay pid 100; chain 100→200→300→400→500→600→700 (six links). Only the
        // first 5 ancestors should be mapped.
        let ppid: HashMap<i32, i32> = [
            (100, 200),
            (200, 300),
            (300, 400),
            (400, 500),
            (500, 600),
            (600, 700),
        ]
        .into_iter()
        .collect();
        let map = match_by_ancestry(&[relay(100, 8901)], &ppid);
        // 5 ancestors: 200,300,400,500,600. NOT the relay pid 100 itself, and
        // NOT 700 (would be the 6th level).
        assert_eq!(map.get(&200), Some(&8901));
        assert_eq!(map.get(&300), Some(&8901));
        assert_eq!(map.get(&400), Some(&8901));
        assert_eq!(map.get(&500), Some(&8901));
        assert_eq!(map.get(&600), Some(&8901));
        assert_eq!(map.get(&700), None, "6th level beyond the 5-level cap");
        assert_eq!(map.get(&100), None, "relay pid itself is not an ancestor");
        assert_eq!(map.len(), 5);
    }

    #[test]
    fn stops_at_parent_le_one() {
        // 100 → 50 → 1 (init). The walk stops BEFORE mapping 1.
        let ppid: HashMap<i32, i32> = [(100, 50), (50, 1)].into_iter().collect();
        let map = match_by_ancestry(&[relay(100, 8901)], &ppid);
        assert_eq!(map.get(&50), Some(&8901));
        assert_eq!(map.get(&1), None, "parent <= 1 stops the walk");
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn stops_at_missing_parent() {
        // 100 → 50, then 50 has no parent.
        let ppid: HashMap<i32, i32> = [(100, 50)].into_iter().collect();
        let map = match_by_ancestry(&[relay(100, 8901)], &ppid);
        assert_eq!(map.get(&50), Some(&8901));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn empty_relays_yields_empty_map() {
        let ppid: HashMap<i32, i32> = [(100, 50)].into_iter().collect();
        assert!(match_by_ancestry(&[], &ppid).is_empty());
    }

    // --- fast_relay_lookup ---

    fn entry(name: &str, sid: &str, pid: i32) -> PidEntryRef {
        PidEntryRef {
            name: Some(name.to_string()),
            session_id: Some(sid.to_string()),
            pid,
        }
    }

    #[test]
    fn fast_lookup_exact_name_and_ancestry() {
        // claude pid 1001 owns the session "alpha"; relay pid 2001 → 1001.
        let entries = vec![entry("alpha", "sess-a", 1001)];
        let relays = vec![relay(2001, 8901)];
        let ppid: HashMap<i32, i32> = [(2001, 1001), (1001, 500)].into_iter().collect();
        let m = fast_relay_lookup("alpha", &entries, &relays, &ppid).unwrap();
        assert_eq!(m.port, 8901);
        assert_eq!(m.session_id, "sess-a");
        assert_eq!(m.name, "alpha");
    }

    #[test]
    fn fast_lookup_is_case_insensitive() {
        let entries = vec![entry("Alpha", "sess-a", 1001)];
        let relays = vec![relay(2001, 8901)];
        let ppid: HashMap<i32, i32> = [(2001, 1001)].into_iter().collect();
        let m = fast_relay_lookup("ALPHA", &entries, &relays, &ppid).unwrap();
        assert_eq!(m.port, 8901);
        // name comes from the entry, original casing preserved.
        assert_eq!(m.name, "Alpha");
    }

    #[test]
    fn fast_lookup_exact_beats_prefix() {
        // Query "al": "al" is an exact match for entry 2 and a prefix of "alpha".
        // Exact must win (TS find-exact-first).
        let entries = vec![entry("alpha", "sess-a", 1001), entry("al", "sess-b", 1002)];
        let relays = vec![relay(2001, 8901), relay(2002, 8902)];
        let ppid: HashMap<i32, i32> = [(2001, 1001), (2002, 1002)].into_iter().collect();
        let m = fast_relay_lookup("al", &entries, &relays, &ppid).unwrap();
        assert_eq!(
            m.session_id, "sess-b",
            "exact 'al' match, not prefix 'alpha'"
        );
        assert_eq!(m.port, 8902);
    }

    #[test]
    fn fast_lookup_prefix_when_no_exact() {
        let entries = vec![entry("alphabet", "sess-a", 1001)];
        let relays = vec![relay(2001, 8901)];
        let ppid: HashMap<i32, i32> = [(2001, 1001)].into_iter().collect();
        let m = fast_relay_lookup("alpha", &entries, &relays, &ppid).unwrap();
        assert_eq!(m.session_id, "sess-a");
    }

    #[test]
    fn fast_lookup_no_name_match_is_none() {
        let entries = vec![entry("alpha", "sess-a", 1001)];
        let relays = vec![relay(2001, 8901)];
        let ppid: HashMap<i32, i32> = [(2001, 1001)].into_iter().collect();
        assert!(fast_relay_lookup("zeta", &entries, &relays, &ppid).is_none());
    }

    #[test]
    fn fast_lookup_no_relays_is_none() {
        let entries = vec![entry("alpha", "sess-a", 1001)];
        let ppid: HashMap<i32, i32> = [(2001, 1001)].into_iter().collect();
        assert!(fast_relay_lookup("alpha", &entries, &[], &ppid).is_none());
    }

    #[test]
    fn fast_lookup_no_ancestry_link_is_none() {
        // Name matches, relay exists, but no ancestry path reaches the entry pid.
        let entries = vec![entry("alpha", "sess-a", 1001)];
        let relays = vec![relay(2001, 8901)];
        let ppid: HashMap<i32, i32> = [(2001, 9999)].into_iter().collect();
        assert!(fast_relay_lookup("alpha", &entries, &relays, &ppid).is_none());
    }

    #[test]
    fn fast_lookup_ancestry_caps_at_five_levels() {
        // relay 2001 → a → b → c → d → e (5th) == match.pid is fine; a 6th-level
        // match would NOT be found.
        let entries = vec![entry("alpha", "sess-a", 60)];
        let relays = vec![relay(2001, 8901)];
        // 2001→10→20→30→40→50→60: 60 is the 6th ancestor → must NOT match.
        let ppid: HashMap<i32, i32> =
            [(2001, 10), (10, 20), (20, 30), (30, 40), (40, 50), (50, 60)]
                .into_iter()
                .collect();
        assert!(fast_relay_lookup("alpha", &entries, &relays, &ppid).is_none());

        // Shorten to 5 levels: 2001→10→20→30→40→60: 60 is the 5th → matches.
        let entries5 = vec![entry("alpha", "sess-a", 60)];
        let ppid5: HashMap<i32, i32> = [(2001, 10), (10, 20), (20, 30), (30, 40), (40, 60)]
            .into_iter()
            .collect();
        let m = fast_relay_lookup("alpha", &entries5, &relays, &ppid5).unwrap();
        assert_eq!(m.port, 8901);
    }
}
