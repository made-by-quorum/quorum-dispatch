//! M1: short codes (sha256→base36), TS src/session.ts:10-40.
//!
//! Port of shortCodeAt / assignShortCodes. Deterministic: the same session list
//! always produces the same codes (session.ts:23-26) — there is no clock, RNG,
//! or environment input, so the codes are a pure function of the session ids and
//! their order. That determinism is load-bearing for `ls --json` parity and for
//! a human re-running `qd ls` and getting the same 3-char handles.

use crate::model::Session;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Port of shortCodeAt, session.ts:15-20.
///
/// `byteOffset = (offset*4) % (hash.length - 4)`; sha256 is 32 bytes, so this is
/// `(offset*4) % 28`. We read a big-endian u32 at that byte offset, render it in
/// base36 (lowercase, like JS `Number.prototype.toString(36)`), take the first 3
/// chars, then right-pad with `'0'` to 3 (JS `slice(0,3).padEnd(3,"0")`).
///
/// Note offset 7 → `28 % 28 == 0`, so it reads the SAME u32 as offset 0 — the
/// collision loop below relies on this only as its terminal bound.
fn short_code_at(session_id: &str, offset: usize) -> String {
    let hash = Sha256::digest(session_id.as_bytes());
    let byte_offset = (offset * 4) % (hash.len() - 4); // (offset*4) % 28
                                                       // hash.readUInt32BE(byteOffset): big-endian u32 at byte_offset.
    let num = u32::from_be_bytes([
        hash[byte_offset],
        hash[byte_offset + 1],
        hash[byte_offset + 2],
        hash[byte_offset + 3],
    ]);
    let base36 = to_base36_lower(num);
    // slice(0,3).padEnd(3,"0"): first 3 chars, right-padded with '0' to width 3.
    let mut code: String = base36.chars().take(3).collect();
    while code.len() < 3 {
        code.push('0');
    }
    code
}

/// JS `Number.prototype.toString(36)` for a u32: lowercase digits 0-9a-z, no
/// leading zeros, "0" for zero (matches JS).
fn to_base36_lower(mut n: u32) -> String {
    if n == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    // Every byte pushed is an ASCII digit/letter.
    String::from_utf8(buf).expect("base36 digits are ASCII")
}

/// Port of assignShortCodes, session.ts:27-40.
///
/// Sessions with no session id get `"---"` (TS `if (!s.sessionId)`). Otherwise we
/// try hash offsets 0..7 while the candidate code is already used BY A DIFFERENT
/// session id — the same session id keeps its existing code (TS condition
/// `used.get(code) !== s.sessionId`), which is why re-listing is stable even with
/// duplicate ids in the input.
pub fn assign_short_codes(sessions: &mut [Session]) {
    let mut used: HashMap<String, String> = HashMap::new(); // code → sessionId
    for s in sessions.iter_mut() {
        if s.session_id.is_empty() {
            s.code = Some("---".to_string());
            continue;
        }
        let mut offset = 0usize;
        let mut code = short_code_at(&s.session_id, offset);
        // while used.has(code) && used.get(code) !== s.sessionId && offset < 7
        while used.get(&code).is_some_and(|owner| owner != &s.session_id) && offset < 7 {
            offset += 1;
            code = short_code_at(&s.session_id, offset);
        }
        used.insert(code.clone(), s.session_id.clone());
        s.code = Some(code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Session, SessionBranch, SessionStatus};

    /// Minimal Session carrying just the session_id (the only field codes reads).
    fn sess(id: &str) -> Session {
        Session {
            name: None,
            user_named: None,
            session_id: id.to_string(),
            code: None,
            sb_id: None,
            pid: None,
            status: SessionStatus::Cold,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "claude-code".to_string(),
            entrypoint: None,
            lineage: None,
            which_branch: SessionBranch::ColdJsonl,
        }
    }

    #[test]
    fn short_code_at_matches_bun() {
        // Cross-checked with `bun -e` against the real crypto runtime
        // (createHash("sha256")...readUInt32BE...toString(36).slice(0,3).padEnd(3,"0")):
        //   "abc123" offsets 0..7 -> u52,1k6,1qe,8e9,1sh,88n,afc,u52
        let expected = ["u52", "1k6", "1qe", "8e9", "1sh", "88n", "afc", "u52"];
        for (o, exp) in expected.iter().enumerate() {
            assert_eq!(&short_code_at("abc123", o), exp, "offset {o}");
        }
        // offset 7 wraps to the same u32 as offset 0 ((7*4)%28 == 0).
        assert_eq!(short_code_at("abc123", 0), short_code_at("abc123", 7));
    }

    #[test]
    fn empty_session_id_is_dashes() {
        let mut v = vec![sess("")];
        assign_short_codes(&mut v);
        assert_eq!(v[0].code.as_deref(), Some("---"));
    }

    #[test]
    fn deterministic_same_input_same_codes() {
        let mut a = vec![sess("session-one"), sess("abc123")];
        let mut b = vec![sess("session-one"), sess("abc123")];
        assign_short_codes(&mut a);
        assign_short_codes(&mut b);
        assert_eq!(
            a.iter().map(|s| s.code.clone()).collect::<Vec<_>>(),
            b.iter().map(|s| s.code.clone()).collect::<Vec<_>>()
        );
        // bun: "session-one" offset0 -> 1o9, "abc123" offset0 -> u52 (no collision).
        assert_eq!(a[0].code.as_deref(), Some("1o9"));
        assert_eq!(a[1].code.as_deref(), Some("u52"));
    }

    #[test]
    fn same_id_reuses_code_no_offset_bump() {
        // Two rows with the SAME session id: TS keeps the same code for both
        // (used.get(code) === s.sessionId short-circuits the collision loop).
        let mut v = vec![sess("abc123"), sess("abc123")];
        assign_short_codes(&mut v);
        assert_eq!(v[0].code, v[1].code);
        assert_eq!(v[0].code.as_deref(), Some("u52")); // offset 0, no bump
    }

    #[test]
    fn collision_bumps_offset() {
        // Brute-force a real collision: find two DISTINCT ids whose offset-0 code
        // matches, so the 2nd must bump to offset 1. This exercises the
        // used.has(code) && used.get(code) !== s.sessionId branch with real data.
        let (id_a, id_b) = find_offset0_collision();
        let code0_a = short_code_at(&id_a, 0);
        assert_eq!(
            short_code_at(&id_b, 0),
            code0_a,
            "precondition: they collide at offset 0"
        );

        let mut v = vec![sess(&id_a), sess(&id_b)];
        assign_short_codes(&mut v);
        // First keeps offset-0 code.
        assert_eq!(v[0].code.as_deref(), Some(code0_a.as_str()));
        // Second was bumped off the colliding code.
        assert_ne!(v[1].code.as_deref(), Some(code0_a.as_str()));
        // And it equals this id's offset-1 code (the first non-colliding offset here).
        assert_eq!(v[1].code.as_deref(), Some(short_code_at(&id_b, 1).as_str()));
    }

    #[test]
    fn codes_are_3_char_alnum() {
        for id in ["abc123", "session-one", "x"] {
            let c = short_code_at(id, 0);
            assert_eq!(c.chars().count(), 3, "code {c} for {id}");
            assert!(
                c.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit()),
                "code {c}"
            );
        }
    }

    /// Search for two distinct ids that produce the same offset-0 short code.
    /// 3-char base36 space is ~46k, so a few thousand probes collide easily.
    fn find_offset0_collision() -> (String, String) {
        let mut seen: HashMap<String, String> = HashMap::new();
        for i in 0..100_000u32 {
            let id = format!("collide-{i}");
            let code = short_code_at(&id, 0);
            if let Some(prev) = seen.get(&code) {
                if prev != &id {
                    return (prev.clone(), id);
                }
            }
            seen.insert(code, id);
        }
        panic!("no offset-0 collision found in 100k probes (statistically impossible)");
    }
}
