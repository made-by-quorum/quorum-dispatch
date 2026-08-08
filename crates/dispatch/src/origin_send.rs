//! Origin-mode `qd send` durability helpers (qd–qf transition W3).
//!
//! The PURE, seamed core of write-then-deliver: mint the correlation-id ULID,
//! build the [`Envelope`] before delivery, parse `--expires`, and the reusable
//! [`Refusal`] type (contract §6 — the `{class,reason}` failure family W4's
//! inbound door + W6's named-ambiguous path build on). The witnessed
//! disposition EVENTS are authored at the call sites via the leaf crate's typed
//! constructors (`DispositionEvent::attempted` etc. — schema-per-event-type at
//! the authoring seam, R8a), so there is no event builder here. The IMPURE fs
//! half lives in [`crate::dispositions`] (the flock append writers); the bin
//! wiring that calls both lives in `bin/qd/verbs/send_unified.rs`.
//!
//! Everything here is a pure function of its inputs (a ULID takes the injected
//! [`Clock`] + a random source; nothing reads the real home / clock directly),
//! so it is unit-testable off the default floor and reusable across the inbound
//! (W4) and ambiguity (W6) doors.

use crate::dispositions::Envelope;
use crate::effects::Clock;

// ===========================================================================
// correlation-id ULID
// ===========================================================================

/// Crockford base32 alphabet (the ULID spec's alphabet — excludes I, L, O, U).
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The default `--expires` window: 12 hours in ms (format doc §1 `expires_at`).
pub const DEFAULT_EXPIRES_MS: i64 = 12 * 60 * 60 * 1000;

/// Mint a fresh **ULID** for an envelope's `correlation_id` (format doc §1: "qd's
/// own log ULID for bare sends"). 26 Crockford-base32 chars: a 48-bit big-endian
/// ms timestamp (from the injected [`Clock`], L9a) followed by 80 bits of
/// randomness. Lexicographically sortable by mint time, globally unique.
///
/// No `ulid` crate is pulled: the format is trivial and self-contained, and the
/// existing tree already hand-rolls its ids from `/dev/urandom` (idstore
/// `random_id`). This uses the SAME `/dev/urandom` source with a degenerate
/// pid+nanos fallback (never fails to produce an id).
pub fn mint_correlation_id(clock: &dyn Clock) -> String {
    mint_ulid_with(clock.now_ms(), &mut random_80_bits)
}

/// Seamed ULID minter: the timestamp + a randomness source are injected so a
/// unit test can pin the exact 26 chars. `now_ms` negative/overflowing is
/// clamped into the 48-bit field (a ULID timestamp is unsigned ms).
fn mint_ulid_with(now_ms: i64, rand10: &mut dyn FnMut() -> [u8; 10]) -> String {
    // 48-bit timestamp (6 bytes) + 80-bit randomness (10 bytes) = 128 bits.
    let ts = (now_ms.max(0) as u64) & 0x0000_FFFF_FFFF_FFFF;
    let r = rand10();
    let mut bytes = [0u8; 16];
    bytes[0] = (ts >> 40) as u8;
    bytes[1] = (ts >> 32) as u8;
    bytes[2] = (ts >> 24) as u8;
    bytes[3] = (ts >> 16) as u8;
    bytes[4] = (ts >> 8) as u8;
    bytes[5] = ts as u8;
    bytes[6..16].copy_from_slice(&r);
    encode_crockford_128(&bytes)
}

/// Encode a 128-bit value (16 bytes, big-endian) as 26 Crockford-base32 chars.
/// 128 bits / 5 bits-per-char = 25.6 → 26 chars; the top char carries only the
/// high 2 bits (ULID's canonical encoding), so the leading symbol is 0-7.
fn encode_crockford_128(bytes: &[u8; 16]) -> String {
    // Assemble into a u128, then peel 5 bits at a time from the top.
    let mut n: u128 = 0;
    for &b in bytes {
        n = (n << 8) | b as u128;
    }
    let mut out = [0u8; 26];
    // 26 groups of 5 bits, MSB-first: the first group is bits [127..125] padded
    // (only 128 bits, so shift 125 leaves the high 3 bits — canonical ULID uses
    // the top 2, which is what the mask yields since the value never exceeds 128
    // bits). Peel from position 25 (LSB) up for a straightforward loop.
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = 5 * (25 - i);
        let idx = ((n >> shift) & 0x1F) as usize;
        *slot = CROCKFORD[idx];
    }
    // Every byte is an ASCII Crockford symbol.
    String::from_utf8(out.to_vec()).expect("crockford symbols are ASCII")
}

/// 80 bits (10 bytes) from `/dev/urandom`, with a degenerate pid+nanos fallback
/// if it is unreadable (essentially impossible) — mirrors idstore's `random_id`
/// posture so a ULID is never un-mintable.
fn random_80_bits() -> [u8; 10] {
    let mut bytes = [0u8; 10];
    if read_urandom(&mut bytes).is_err() {
        let seed = (std::process::id() as u128) << 64
            | (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
                & 0xFFFF_FFFF_FFFF_FFFF);
        let sb = seed.to_le_bytes();
        bytes.copy_from_slice(&sb[..10]);
    }
    bytes
}

fn read_urandom(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

// ===========================================================================
// --expires duration parser
// ===========================================================================

/// Parse an `--expires` duration into MILLISECONDS (contract part C):
///   - a bare `<int>`      → seconds (e.g. `"45"` = 45s),
///   - `<int>s|m|h|d`      → seconds / minutes / hours / days (`"12h"`, `"30m"`).
///
/// Returns the duration in ms, or `Err(message)` for any other form (a SYNC
/// refusal — the caller renders it via [`Refusal`]). Rejects: empty, negative,
/// a non-integer magnitude, an unknown unit, a bare unit with no number, and
/// values that would overflow `i64` ms.
///
/// Hand-rolled (no `humantime` dep) — the grammar is one integer + one optional
/// unit char.
pub fn parse_expires(raw: &str) -> Result<i64, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty duration (expected e.g. 12h, 30m, 45s, 1d, or a bare integer of seconds)".to_string());
    }
    // Split a trailing unit letter off the magnitude, if present.
    let (num_str, unit_ms): (&str, i64) = match s.as_bytes().last() {
        Some(b's') | Some(b'S') => (&s[..s.len() - 1], 1_000),
        Some(b'm') | Some(b'M') => (&s[..s.len() - 1], 60_000),
        Some(b'h') | Some(b'H') => (&s[..s.len() - 1], 3_600_000),
        Some(b'd') | Some(b'D') => (&s[..s.len() - 1], 86_400_000),
        // No recognized unit suffix → bare integer = seconds.
        Some(c) if c.is_ascii_digit() => (s, 1_000),
        _ => {
            return Err(format!(
                "invalid duration {raw:?} — use an integer optionally suffixed s|m|h|d (e.g. 12h, 30m, 45s, 1d)"
            ));
        }
    };
    if num_str.is_empty() {
        return Err(format!(
            "duration {raw:?} has a unit but no number (e.g. 12h, not just h)"
        ));
    }
    let magnitude: i64 = num_str.parse().map_err(|_| {
        format!("invalid duration {raw:?} — {num_str:?} is not a non-negative integer")
    })?;
    if magnitude < 0 {
        return Err(format!("duration {raw:?} must not be negative"));
    }
    magnitude
        .checked_mul(unit_ms)
        .ok_or_else(|| format!("duration {raw:?} is too large"))
}

// ===========================================================================
// envelope builder (write-then-deliver)
// ===========================================================================

/// Build the origin-mode [`Envelope`] to append BEFORE delivery (write-then-
/// deliver, format doc §1). `target` is the RAW address string the caller gave
/// (operational record); `body` is the message verbatim; `origin` is the origin
/// host id ([`crate::dispositions::local_host`] — this qd originates, so it is
/// the origin). `expires_at = authored_at + expires_ms` (saturating, so a huge
/// `--expires` can never wrap negative).
///
/// There is deliberately NO disposition-event builder here: the witnessed
/// events (`attempted`/`queued`/`delivered`/…) are authored at the call sites
/// via the leaf crate's typed constructors, which enforce the per-event-type
/// `reason` invariant at the authoring seam (R8a).
pub fn build_envelope(
    correlation_id: String,
    authored_at: i64,
    expires_ms: i64,
    target: String,
    origin: String,
    body: String,
) -> Envelope {
    Envelope {
        v: 1,
        correlation_id,
        authored_at,
        expires_at: authored_at.saturating_add(expires_ms),
        target,
        origin,
        body,
    }
}

// ===========================================================================
// Refusal {class, reason} (contract §6) — the shared failure-family type
// ===========================================================================

/// The exit code for the `refused` failure family (contract §6). Distinct from
/// success (0), the generic/infra class (1), and `send:pty`'s write-failed 11
/// (ADD-18) — a machine reading a `qd send` exit can tell a door refusal apart
/// from a mid-flight write failure.
pub const EXIT_REFUSED: i32 = 12;

/// A refusal in the `{class, reason}` failure family (format doc §6, contract
/// §6). Covers the `refused` / `failed` / `expired` families W4's inbound door
/// (malformed / mis-addressed / past-expiry / ambiguous) and W6 (named
/// ambiguous) reuse. Minimal by design: a machine-readable stderr render + a
/// stable exit-code mapping.
///
/// `class` is the machine-readable family token (e.g. `"malformed"`,
/// `"self-send"`, `"wake"`, `"ambiguous"`); `reason` is the human sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub family: Family,
    pub class: String,
    pub reason: String,
}

/// The three failure families (contract §6). `Refused` is a synchronous door
/// refusal (bad address / self-send / ambiguous); `Failed` is a witnessed
/// non-delivery (`failed{wake}` etc.); `Expired` is a past-expiry drop. All three
/// map to [`EXIT_REFUSED`] as the non-success door code today, but the family is
/// carried so a consumer (and W4/W6) can distinguish them without parsing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Refused,
    Failed,
    Expired,
}

impl Family {
    /// The wire token for the family (the `refused{…}` / `failed{…}` /
    /// `expired{…}` head of the stderr render).
    pub fn token(self) -> &'static str {
        match self {
            Family::Refused => "refused",
            Family::Failed => "failed",
            Family::Expired => "expired",
        }
    }
}

impl Refusal {
    /// A `refused{class}` door refusal (bad address / self-send / ambiguous).
    pub fn refused(class: impl Into<String>, reason: impl Into<String>) -> Self {
        Refusal {
            family: Family::Refused,
            class: class.into(),
            reason: reason.into(),
        }
    }

    /// A `failed{class}` witnessed non-delivery (`failed{wake}` etc.).
    pub fn failed(class: impl Into<String>, reason: impl Into<String>) -> Self {
        Refusal {
            family: Family::Failed,
            class: class.into(),
            reason: reason.into(),
        }
    }

    /// An `expired{class}` past-expiry drop.
    pub fn expired(class: impl Into<String>, reason: impl Into<String>) -> Self {
        Refusal {
            family: Family::Expired,
            class: class.into(),
            reason: reason.into(),
        }
    }

    /// The stable, machine-readable stderr line (contract §6):
    /// `qd send: <family>{<class>}: <reason>`. Pinned by a unit test so W4/W6
    /// render identically.
    pub fn stderr_line(&self) -> String {
        format!(
            "qd send: {}{{{}}}: {}",
            self.family.token(),
            self.class,
            self.reason
        )
    }

    /// The exit code for this refusal — [`EXIT_REFUSED`] for every family today
    /// (a single distinct door code, not colliding with 1 or 11).
    pub fn exit_code(&self) -> i32 {
        EXIT_REFUSED
    }

    /// Print [`Self::stderr_line`] to stderr and return [`Self::exit_code`] — the
    /// one-call refusal the verb bodies use.
    pub fn emit(&self) -> i32 {
        eprintln!("{}", self.stderr_line());
        self.exit_code()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::FixedClock;

    // ---- ULID ---------------------------------------------------------------

    #[test]
    fn ulid_is_26_crockford_chars() {
        let id = mint_correlation_id(&FixedClock(1_700_000_000_000));
        assert_eq!(id.len(), 26, "ULID is 26 chars: {id}");
        assert!(
            id.bytes().all(|b| CROCKFORD.contains(&b)),
            "all Crockford symbols: {id}"
        );
    }

    #[test]
    fn ulid_timestamp_prefix_is_deterministic_for_a_fixed_clock() {
        // Same ms + same randomness ⇒ identical ULID; the first 10 chars encode
        // the 48-bit timestamp, so two mints at the same ms share that prefix
        // regardless of the random tail.
        let ts = 1_700_000_000_000i64;
        let a = mint_ulid_with(ts, &mut || [0u8; 10]);
        let b = mint_ulid_with(ts, &mut || [0xFFu8; 10]);
        assert_eq!(&a[..10], &b[..10], "timestamp prefix is clock-derived: {a} vs {b}");
        // A later timestamp sorts lexicographically after an earlier one.
        let later = mint_ulid_with(ts + 1000, &mut || [0u8; 10]);
        assert!(later > a, "ULIDs sort by mint time: {later} > {a}");
    }

    #[test]
    fn ulid_all_zero_and_all_one_bounds() {
        assert_eq!(mint_ulid_with(0, &mut || [0u8; 10]), "0".repeat(26));
        // Max 48-bit ts + max randomness → the top char is 7 (only 2 high bits
        // are populated in a 128-bit value) and the rest are Z.
        let max = mint_ulid_with(i64::MAX, &mut || [0xFFu8; 10]);
        assert_eq!(max.len(), 26);
        assert_eq!(&max[0..1], "7", "128-bit ceiling: leading symbol is 7");
        assert!(max[1..].bytes().all(|b| b == b'Z'), "rest are Z: {max}");
    }

    #[test]
    fn ulid_mints_are_unique_under_real_randomness() {
        let c = FixedClock(42);
        let a = mint_correlation_id(&c);
        let b = mint_correlation_id(&c);
        assert_ne!(a, b, "distinct random tails ⇒ distinct ULIDs even at one ms");
    }

    // ---- parse_expires ------------------------------------------------------

    #[test]
    fn parse_expires_units() {
        assert_eq!(parse_expires("45s"), Ok(45_000));
        assert_eq!(parse_expires("30m"), Ok(30 * 60_000));
        assert_eq!(parse_expires("12h"), Ok(12 * 3_600_000));
        assert_eq!(parse_expires("1d"), Ok(86_400_000));
        // Bare integer = seconds.
        assert_eq!(parse_expires("90"), Ok(90_000));
        assert_eq!(parse_expires("0"), Ok(0));
        // Uppercase units accepted.
        assert_eq!(parse_expires("2H"), Ok(2 * 3_600_000));
        // Whitespace trimmed.
        assert_eq!(parse_expires("  15m "), Ok(15 * 60_000));
    }

    #[test]
    fn parse_expires_default_constant_is_12h() {
        assert_eq!(DEFAULT_EXPIRES_MS, 12 * 3_600_000);
    }

    #[test]
    fn parse_expires_rejects_bad_forms() {
        for bad in ["", "   ", "h", "m", "abc", "12x", "1.5h", "-5", "-5m", "12h30m", "h12"] {
            assert!(parse_expires(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn parse_expires_overflow_is_rejected_not_wrapped() {
        // A magnitude that overflows i64 ms when scaled by the unit.
        assert!(parse_expires(&format!("{}d", i64::MAX)).is_err());
    }

    // ---- envelope builder ---------------------------------------------------

    #[test]
    fn build_envelope_stamps_expiry_and_carries_raw_fields() {
        let e = build_envelope(
            "01ABCID".into(),
            1000,
            DEFAULT_EXPIRES_MS,
            "alpha@brano".into(),
            "brano".into(),
            "hello world".into(),
        );
        assert_eq!(e.v, 1);
        assert_eq!(e.correlation_id, "01ABCID");
        assert_eq!(e.authored_at, 1000);
        assert_eq!(e.expires_at, 1000 + DEFAULT_EXPIRES_MS);
        assert_eq!(e.target, "alpha@brano", "raw caller address, verbatim");
        assert_eq!(e.origin, "brano");
        assert_eq!(e.body, "hello world", "body verbatim");
    }

    #[test]
    fn build_envelope_expiry_saturates_not_wraps() {
        let e = build_envelope("id".into(), i64::MAX - 5, DEFAULT_EXPIRES_MS, "t".into(), "o".into(), "b".into());
        assert_eq!(e.expires_at, i64::MAX, "saturating add, never negative");
    }

    // ---- Refusal ------------------------------------------------------------

    #[test]
    fn refusal_render_is_machine_stable() {
        assert_eq!(
            Refusal::refused("malformed", "target address is empty").stderr_line(),
            "qd send: refused{malformed}: target address is empty"
        );
        assert_eq!(
            Refusal::refused("self-send", "QD_SESSION_ID resolves to the target").stderr_line(),
            "qd send: refused{self-send}: QD_SESSION_ID resolves to the target"
        );
        assert_eq!(
            Refusal::failed("wake", "target could not be woken").stderr_line(),
            "qd send: failed{wake}: target could not be woken"
        );
        assert_eq!(
            Refusal::expired("past-expiry", "envelope expired before delivery").stderr_line(),
            "qd send: expired{past-expiry}: envelope expired before delivery"
        );
    }

    #[test]
    fn refusal_exit_code_is_distinct_from_success_generic_and_write_failed() {
        assert_eq!(EXIT_REFUSED, 12);
        assert_ne!(EXIT_REFUSED, 0);
        assert_ne!(EXIT_REFUSED, 1);
        assert_ne!(EXIT_REFUSED, 11, "must not collide with send:pty write-failed");
        for r in [
            Refusal::refused("a", "b"),
            Refusal::failed("a", "b"),
            Refusal::expired("a", "b"),
        ] {
            assert_eq!(r.exit_code(), EXIT_REFUSED);
        }
    }

    #[test]
    fn family_tokens_are_stable() {
        assert_eq!(Family::Refused.token(), "refused");
        assert_eq!(Family::Failed.token(), "failed");
        assert_eq!(Family::Expired.token(), "expired");
    }
}
