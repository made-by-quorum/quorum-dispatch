//! Versioned disposition-record schema (qd–qf transition v1).
//!
//! A pure LEAF crate (std + serde/serde_json only; NO `dispatch`/`qrmux` dep).
//! It owns the Rust reflections of the three wire shapes contracted in
//! `dispatch/doc/formats/dispatch-transport-formats.md` — the format doc is the
//! authority; these structs REFLECT it, not the other way round:
//!
//! - [`Envelope`]        — a `log.jsonl` row (§1: an envelope qd ORIGINATED).
//! - [`Disposition`]     — a stored `dispositions.jsonl` row (§2: a witnessed
//!   TERMINAL fact — only `delivered`/`failed` are ever stored).
//! - [`EmittedRecord`]   — the PUBLISHED `qd dispositions` output row (§3: the
//!   one data shape frame projects over; the 4-state view `pending` /
//!   `delivered` / `failed` / `expired`).
//!
//! plus the torn-tail-tolerant JSONL parsers ([`parse_log`],
//! [`parse_dispositions`]) and the pure left-join [`project`] that computes the
//! emitted record stream from an envelope stream ⟕ a disposition stream.
//!
//! # Wire byte-exactness — WHY serde-derive suffices here (unlike delivery-events)
//!
//! `quorum-delivery-events` hand-builds a `serde_json::Map` and inserts keys in
//! a pinned order because its record shape is DYNAMIC — a fixed envelope head
//! merged with a per-`Payload`-variant tail, with keys omitted/added at runtime.
//! An `IndexMap` (serde_json `preserve_order`) is required there or a `BTreeMap`
//! would sort the keys and change the bytes.
//!
//! Here every wire shape is a FIXED-SHAPE `struct`. `serde_json` serializes a
//! struct's fields **in field-declaration order, deterministically** — this is a
//! property of the `Serialize` derive walking the struct's fields top-to-bottom,
//! and it holds REGARDLESS of the `preserve_order` feature (which only reorders
//! `Value`/`Map` iteration, never struct-field emission). So declaring each
//! struct's fields in the exact wire order the format doc pins, plus
//! `#[serde(skip_serializing_if = "Option::is_none")]` on the omittable `reason`,
//! gives byte-exact output BY CONSTRUCTION with a plain `#[derive(Serialize)]` —
//! no feature gate, no hand-built Map. (`witnessed_at` on [`EmittedRecord`] is a
//! STABLE column and is emitted as JSON `null` when absent — it is deliberately
//! NOT skipped.)

mod parse;
mod project;
mod record;

pub use parse::{parse_dispositions, parse_log, ReadResult};
pub use project::{project, project_one};
pub use record::{Disposition, EmittedRecord, Envelope, RecordState, StoredState};
