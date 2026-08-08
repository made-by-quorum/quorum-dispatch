//! Versioned disposition-record schema (qd–qf transition v1, R8–R11).
//!
//! A pure LEAF crate (std + serde/serde_json only; NO `dispatch`/`qrmux` dep).
//! It owns the Rust reflections of the wire shapes contracted in
//! `dispatch/doc/formats/dispatch-transport-formats.md` — the format doc is the
//! authority; these structs REFLECT it, not the other way round:
//!
//! - [`Envelope`]         — a `log.jsonl` row (§1: an envelope qd ORIGINATED).
//! - [`DispositionEvent`] — a `dispositions.jsonl` row: ONE typed witnessed
//!   EVENT (§2, R8/R8a/R8b). The file is an append-only log of witnessed
//!   moments, never state records. The five [`EventKind`] types are `accepted`
//!   / `attempted` / `queued` / `delivered` / `delivery-failed`; `reason` is
//!   REQUIRED on `delivery-failed`, FORBIDDEN on every other type.
//! - [`SummaryRecord`]    — the PUBLISHED `qd dispositions` DEFAULT output row
//!   (§3): one per `correlation_id`, carrying the coarse 4-state
//!   [`SummaryState`] (`pending`/`delivered`/`failed`/`expired`, UNCHANGED per
//!   guard 2) plus `last_event`/`witness` (paired-null, R11.1) and the analytics
//!   fields. `--events` emits the raw [`DispositionEvent`] funnel instead.
//!
//! State is a VIEW, always: [`project_summary`] folds the event log into
//! summaries; [`has_delivered`] is the idempotence + delivered-view predicate
//! ("a delivered event exists," never "any terminal"). The state precedence
//! lives in an isolated `derive_state` (RATIFIED, R10: delivered > expired >
//! failed > pending).
//!
//! plus the torn-tail-tolerant JSONL parsers ([`parse_log`],
//! [`parse_dispositions`] — the latter also enforces the per-event-type
//! `reason` invariant on read).
//!
//! Naming is RULED (R9): `origin` (the origin host id; with `authored_at`, the
//! ORIGIN timeline) vs `witness` (the witnessing host id; with `witnessed_at`,
//! the WITNESS timeline) — the N10 split, expressed as field naming.
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
//! no feature gate, no hand-built Map. (The nullable columns on
//! [`SummaryRecord`] — `last_event` / `last_attempt_at` / `first_delivered_at`
//! / `expires_at` / `witness` — are STABLE columns emitted as JSON `null` when
//! absent, deliberately NOT skipped, so the DuckDB projection sees a fixed
//! schema.)

mod parse;
mod project;
mod record;

pub use parse::{parse_dispositions, parse_log, ReadResult};
pub use project::{has_delivered, project_one, project_summary};
pub use record::{
    DispositionEvent, Envelope, EventKind, SummaryRecord, SummaryState,
};
