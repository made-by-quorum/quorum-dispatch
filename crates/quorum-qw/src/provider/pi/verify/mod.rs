//! `provider::pi::verify` — live-daemon verification harnesses for pi.
//!
//! **Not a production path.** Nothing under this module is reachable from a
//! `qd` verb; every file here drives a REAL `pi --mode rpc` (or a stub standing
//! in for one) from a test/evidence wrapper and asserts OBSERVED effects —
//! registry rows, process trees, endpoints, wire frames — never a return
//! string, never read-and-assume. They are library-shaped (not bare
//! `tests/*.rs`) so their pure helpers stay unit-tested even though the
//! end-to-end runs are gated behind `QD_PI_LIVE=1` / similar env gates.
//!
//! Grouped together because all three answer "does the shipped pi adapter
//! actually hold up," at three different angles on the same item-7 rubric:
//!   - [`conformance`] — the tier-a RUN-not-read conformance harness: drives
//!     real `qd` verbs by name against a live pi and asserts each verb's
//!     observed effect against the 8 tier-a rubric items (transport, launch,
//!     boot, teardown, auth/config, concurrency, liveness, maturity).
//!   - [`chaos`] — kills, races and resource-exhausts a live resident to prove
//!     the teardown/liveness machinery degrades instead of leaking or hanging.
//!   - [`redteam`] — C-RED, the adversarial-JSONL battery: malformed/hostile
//!     frames at the PA6/PA7 wire seams, asserting degrade-not-crash and
//!     correct status/error mapping rather than a raw panic or leaked
//!     `TypeError`.

pub mod chaos;
pub mod conformance;
pub mod redteam;
