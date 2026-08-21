//! Infrastructure shared by **qd** (routing and transport) and **qw** (session
//! management).
//!
//! The qd/qw split cuts through a layer that belongs to neither side: qd needs it
//! to resolve targets and render output, qw needs it to run sessions. Putting it
//! in either package would force a dependency in one direction and a duplicate in
//! the other, so it lives here and both depend on it — the same idiom already
//! used by `quorum-delivery-events`, `quorum-submit-discipline` and
//! `quorum-dispositions`.
//!
//! The set is closed and self-contained: `exec` depends on nothing, `effects` on
//! `exec`, `paths` and `zmx_dir` on `effects`, and `model` on nothing. No module
//! here reaches back into the code being split, which is what makes this a leaf
//! rather than a second copy of dispatch.

/// Which gather-time discovery reads FAILED, as distinct from which ones
/// legitimately found nothing.
///
/// Neither package's: **qw produces** it — the gather step that does the mux /
/// `ps` / registry reads records its failures here — and **qd consumes** it, in
/// `render`'s "unknown (ps unavailable)" fields and in `send`'s refusal, which
/// must say "could not determine" rather than assert an absence it never
/// observed. Putting it in qw would make qd's rendering layer name a qw type for
/// a struct with no session-management in it; putting it in qd would invert the
/// dependency at gather time. Dependency-free (std + libc), so it lives with the
/// leaves.
pub mod discovery;
pub mod effects;
pub mod exec;
pub mod fmt;
/// Stable session ids — an append-only mint ledger plus a pure fold.
///
/// Neither package's: **qw mints** (during create / resume / fork / bind, all
/// session lifecycle) and **qd folds to resolve** (`resolve_to_uuid`,
/// `holder_display_id`, and the target resolution behind every verb). Putting it
/// in qw would put an RPC on every target resolution; putting it in qd would
/// invert the dependency at mint time. It carries no session-management and no
/// presentation logic — an id ledger and a fold — so it lives here.
pub mod idstore;
pub mod model;
pub mod paths;
/// API-key-shaped redaction for the engine `send-initiated` `content_preview`
/// (ADD-20 §6.1).
///
/// Neither package's: a dependency-free hand-rolled scanner with no `use crate::`
/// edge at all. qd's `send` verb feeds it the sent text; qw's delivery bodies do
/// the same once they move. A leaf, so it lives with the leaves.
pub mod redact;
pub mod timefmt;
pub mod zmx_dir;
