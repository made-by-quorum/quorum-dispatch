//! M3 byte-identity (BUILD-DIRECTIVES §2b, primary source) — the `send:pty`
//! PRE-HANDOFF record.
//!
//! Under the single-writer split, qd's ONLY ledger write on the embedded path is
//! the `send-initiated` pre-handoff record; the mux owns every terminal. This test
//! pins the WIRE BYTES of that record as emitted through the DISPATCH crate's
//! `build_record_line` (re-exported from the `quorum-delivery-events` leaf crate).
//!
//! Why a DISPATCH-side test (the leaf crate has its own `golden_wire.rs`): the
//! `preserve_order` trap (BUILD-DIRECTIVES §2b) is a per-DEPENDENT feature — if
//! `dispatch/Cargo.toml` set `default-features = false` on the leaf dep, DISPATCH's
//! `build_record_line` calls would silently emit SORTED (alphabetical) keys and
//! compile green, while the leaf's own tests (which keep default features) stay
//! green. Only a test in THIS crate catches dispatch's dep misconfiguration. The
//! two asserts below RED under the sort: the full-line equality fails, and the
//! explicit key-order assert fails (`content_sha256` sorts AFTER `content_len`).

use dispatch::events::{build_record_line, sha256_hex, Envelope, Payload};

/// The exact `Payload::SendInitiated` `run_send_pty` builds pre-handoff (idle send,
/// resolvable transcript + preview) — the field SET and VALUES qd emits.
fn send_initiated() -> Payload {
    Payload::SendInitiated {
        send_id: "4242-1735689600000-0".to_string(),
        verb: "send:pty".to_string(),
        send_path: "idle".to_string(),
        content_sha256: sha256_hex(b"hello"),
        content_len: 5,
        chunks: 1,
        chunk_sha256s: vec![sha256_hex(b"hello")],
        chunk_sha256s_capped: false, // false ⇒ OMITTED (§2.3)
        transcript: Some("/t.jsonl".to_string()),
        transcript_offset: Some(0), // Some(0) ⇒ INCLUDED (present, value 0)
        content_preview: Some("hello".to_string()),
    }
}

fn fixed_envelope() -> Envelope {
    // Deterministic (no start_ms; fixed pid/seq/ts) so the wire bytes are frozen.
    Envelope {
        v: 1,
        ts: "2026-01-01T00:00:00.000Z".to_string(),
        pid: 4242,
        seq: 0,
        session: Some("sid-1".to_string()),
        name: None,
        start_ms: None,
    }
}

#[test]
fn send_initiated_pre_handoff_is_byte_identical_to_golden() {
    // FROZEN golden: envelope (v,ts,pid,seq,session?) then `event` then the §2.3
    // send-initiated field order — INSERTION order, the preserve_order contract.
    // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824.
    let golden = "{\"v\":1,\"ts\":\"2026-01-01T00:00:00.000Z\",\"pid\":4242,\"seq\":0,\
        \"session\":\"sid-1\",\"event\":\"send-initiated\",\
        \"send_id\":\"4242-1735689600000-0\",\"verb\":\"send:pty\",\"send_path\":\"idle\",\
        \"content_sha256\":\"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824\",\
        \"content_len\":5,\"chunks\":1,\
        \"chunk_sha256s\":[\"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824\"],\
        \"transcript\":\"/t.jsonl\",\"transcript_offset\":0,\"content_preview\":\"hello\"}";

    let line = build_record_line(&fixed_envelope(), &send_initiated(), 48);
    assert_eq!(
        line, golden,
        "pre-handoff send-initiated wire bytes drifted from the frozen golden \
         (a fork of the vocabulary, OR default-features=false on the leaf dep → SORTED keys)"
    );
}

#[test]
fn preserve_order_is_on_insertion_not_sorted() {
    // The anti-trap assertion: under `preserve_order` ON the keys are INSERTION-
    // ordered, so `content_sha256` precedes `content_len`. With the feature OFF the
    // map is a BTreeMap (alphabetical), which would put `content_len` first
    // ("content_len" < "content_sha256"). This RED's precisely on the §2b trap.
    let line = build_record_line(&fixed_envelope(), &send_initiated(), 48);
    let i_sha = line.find("\"content_sha256\"").expect("has content_sha256");
    let i_len = line.find("\"content_len\"").expect("has content_len");
    assert!(
        i_sha < i_len,
        "content_sha256 must precede content_len (insertion order / preserve_order ON); \
         sorted output would reverse them — the default-features=false trap"
    );
    // And `chunk_sha256s_capped:false` is OMITTED (not serialized as false).
    assert!(
        !line.contains("chunk_sha256s_capped"),
        "a false *_capped bool is omitted (§2.3), never emitted"
    );
}
