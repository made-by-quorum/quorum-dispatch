//! Versioned disposition-record schema (qd–qf transition v1, R8–R14).
//!
//! A pure LEAF crate (std + serde/serde_json only; NO `dispatch`/`qrmux` dep).
//! It owns the Rust reflections of the wire shapes contracted in
//! `dispatch/doc/formats/dispatch-transport-formats.md` — the format doc is the
//! authority; these structs REFLECT it, not the other way round:
//!
//! - [`Envelope`]         — a `log.jsonl` row (§1: an envelope qd ORIGINATED).
//!   UNCHANGED by R14 — the single normalized HOME of `origin`/`authored_at`.
//! - [`DispositionEvent`] — a `dispositions.jsonl` row: ONE typed EVENT, as a
//!   DISCRIMINATED UNION (§2, R8/R8a/R8b + R14.5). The file is an append-only
//!   log of recorded moments, never state records. The five [`EventKind`] types
//!   are `attempted` / `queued` / `delivered` / `delivery-failed` / `refused`;
//!   `delivery-failed` and `refused` each carry a REQUIRED machine `class`, the
//!   other three carry only the common fields (`v`, `correlation_id`,
//!   `created_at`) — the per-variant field exists ONLY on its variant BY
//!   CONSTRUCTION (R14.5, the type system replaces the old runtime forbidden
//!   check). Event rows are FULLY NORMALIZED (R14.2, invariant N13): no
//!   `witness`, no copied `origin`, no copied `authored_at`.
//! - [`SummaryRecord`]    — the PUBLISHED `qd dispositions` DEFAULT output row
//!   (§3): one per `correlation_id`, carrying the coarse 4-state
//!   [`SummaryState`] (`pending`/`delivered`/`failed`/`expired`, UNCHANGED per
//!   guard 2) plus `last_event` (nullable) and the analytics fields.
//!   `origin`/`authored_at`/`expires_at` come ONLY from the joined envelope, so
//!   an orphan-event summary is honestly null across all three (R14.2).
//!   `--events` emits the raw [`DispositionEvent`] funnel instead.
//!
//! State is a VIEW, always: [`project_summary`] folds the event log into
//! summaries; [`has_delivered`] is the idempotence + delivered-view predicate
//! ("a delivered event exists," never "any terminal"). The state precedence
//! lives in an isolated `derive_state` (RATIFIED, R10: delivered > expired >
//! failed > pending; `refused` folds pending-class per R14.3).
//!
//! plus the torn-tail-tolerant JSONL parsers ([`parse_log`],
//! [`parse_dispositions`] — the latter's discriminated-union invariants ride on
//! [`DispositionEvent`]'s own `Deserialize`).
//!
//! # R14 normalization (invariant N13)
//!
//! We never denormalize; views live over the data model, never inside it. Event
//! rows carry no provenance columns — provenance is the CONTAINER a row lives in
//! (a local file ⇒ this host; a mirror ⇒ the path's host), attached as a
//! computed `source` column at union/emission (a view concern). `origin` and
//! `authored_at` live once, on the [`Envelope`]; events JOIN by `correlation_id`.
//! `created_at` = when THIS host recorded the event (R14.1) — for outcome events
//! that is OBSERVATION time (no retro-dating).
//!
//! # Wire byte-exactness — the two mechanisms in this crate
//!
//! [`Envelope`] and [`SummaryRecord`] are FIXED-SHAPE structs: `serde_json`
//! serializes a struct's fields **in field-declaration order, deterministically**
//! — a property of the `Serialize` derive walking fields top-to-bottom, holding
//! REGARDLESS of the `preserve_order` feature (which only reorders `Value`/`Map`
//! iteration, never struct-field emission). So declaring each struct's fields in
//! the exact wire order the format doc pins gives byte-exact output BY
//! CONSTRUCTION with a plain `#[derive(Serialize)]`. The nullable columns on
//! [`SummaryRecord`] (`last_event` / `last_attempt_at` / `first_delivered_at` /
//! `expires_at` / `authored_at` / `origin`) are STABLE columns emitted as JSON
//! `null` when absent, deliberately NOT skipped, so the DuckDB projection sees a
//! fixed schema.
//!
//! [`DispositionEvent`] is a DISCRIMINATED UNION whose wire order must be
//! `{v, correlation_id, event, created_at, [class]}` — `v` FIRST (version-marker
//! convention), `event` THIRD (matching the envelope). serde's internally tagged
//! enum (`#[serde(tag = "event")]`) puts the tag FIRST, which is the wrong order,
//! so the crate hand-writes `Serialize`/`Deserialize` for it: `Serialize` emits
//! the fixed head then the per-variant `class` tail (last, omitted on the plain
//! variants); `Deserialize` reads keys order-independently but rejects a
//! duplicate key, an unknown/foreign field, a missing common field, and a
//! per-variant tail that is missing where required or present where forbidden —
//! the discriminated-union shape, enforced on read. This is the same
//! "hand-controlled key order" reason `quorum-delivery-events` builds a pinned
//! `Map`; here the head is fixed and only the tail varies, so a manual
//! `SerializeStruct` suffices without an `IndexMap`.

mod parse;
mod project;
mod record;

pub use parse::{parse_dispositions, parse_log, ReadResult};
pub use project::{has_delivered, project_one, project_summary};
pub use record::{
    DispositionEvent, Envelope, EventKind, SummaryRecord, SummaryState,
};
