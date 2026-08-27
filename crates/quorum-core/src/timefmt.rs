//! Epoch-millisecond timestamp formatting.
//!
//! Shared infrastructure, not presentation. The consumers stamp WIRE fields:
//! `events` (the delivery ledger `Envelope.ts`), `idstore` (the `ids.jsonl`
//! mint/bind log), `telemetry` (`marks.jsonl`), `relay_server`, and `archive`
//! (the SigV4 `x-amz-date` header). These lived in `render.rs`, which made them
//! look like output formatting; only one consumer ever was.
//!
//! They belong here rather than in either package because their consumers land on
//! BOTH sides of the qd/qw boundary — and one, `telemetry`, is qw-bound, so
//! leaving the formatter in qd would have created a qw -> qd dependency the
//! moment telemetry moved.

// --- Date formatting ---

/// Epoch ms → `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC), replicating JS `Date.toJSON`
/// (`toISOString`), which is ALWAYS ms-precision UTC. No chrono — civil-date
/// math (Howard Hinnant). Verified vs bun (`new Date(ms).toJSON()`).
pub fn epoch_ms_to_iso(ms: i64) -> String {
    let (y, mo, d, h, mi, s, milli) = civil_from_epoch_ms(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

/// Epoch ms → AWS SigV4 `x-amz-date` long form `YYYYMMDDTHHMMSSZ` (UTC, no
/// milliseconds, no separators). `crate::archive::sigv4` signs against
/// exactly this string; the first 8 chars double as the SigV4 date stamp.
pub fn epoch_ms_to_amz_date(ms: i64) -> String {
    let (y, mo, d, h, mi, s, _milli) = civil_from_epoch_ms(ms);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// Epoch ms → en-US `toLocaleString()` form `M/D/YYYY, H:MM:SS AM/PM` in UTC.
///
/// NORMALIZATION-CLASS (spec §8): the real TS output is locale + timezone
/// dependent (`Date.toLocaleString()` with no args). The 0b comparator normalizes
/// these lines; we emit a DETERMINISTIC en-US/UTC form so the Rust output is
/// stable and byte-exact only POST-normalization. Verified vs bun:
///   `bun -e 'console.log(new Date(1717530000000).toLocaleString("en-US",{timeZone:"UTC"}))'`
///     → 6/4/2024, 3:40:00 PM
/// Rules: no leading zero on month/day/hour; zero-padded minute/second; 12-hour
/// with AM/PM; midnight → 12 AM, noon → 12 PM.
pub fn epoch_ms_to_en_us_locale(ms: i64) -> String {
    let (y, mo, d, h24, mi, s, _milli) = civil_from_epoch_ms(ms);
    let (h12, ampm) = match h24 {
        0 => (12, "AM"),
        1..=11 => (h24, "AM"),
        12 => (12, "PM"),
        _ => (h24 - 12, "PM"),
    };
    format!("{mo}/{d}/{y}, {h12}:{mi:02}:{s:02} {ampm}")
}

/// Decompose epoch ms (UTC) into (year, month, day, hour, min, sec, milli).
fn civil_from_epoch_ms(ms: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let total_secs = ms.div_euclid(1000);
    let milli = ms.rem_euclid(1000) as u32;
    let days = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400);
    let hour = (secs_of_day / 3600) as u32;
    let min = ((secs_of_day % 3600) / 60) as u32;
    let sec = (secs_of_day % 60) as u32;
    let (y, mo, d) = civil_from_days(days);
    (y, mo, d, hour, min, sec, milli)
}

/// Inverse of days-from-civil (Howard Hinnant). days since 1970-01-01 → (y,m,d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC) → epoch ms. The STRICT inverse of
/// [`epoch_ms_to_iso`]: it accepts that exact fixed-width shape and NOTHING
/// else — any deviation (wrong length, a wrong separator, a missing/other
/// suffix than `Z`, a non-digit in any field, an out-of-range component such as
/// month 13 or Feb 30) returns `None`.
///
/// WHY STRICTNESS IS LOAD-BEARING (not fastidiousness): the consumer is the
/// `ids.jsonl` in-flight-mint gate ([`crate::idstore::IdMap::newest_unbound_mint_ms`]),
/// and `ids.jsonl` FIXTURES across this repo write placeholder stamps like
/// `"ts":"t"`. A lenient (or lexicographic) reading would rank `"t"` NEWER than
/// any real timestamp and would gate the `ls` backfill forever. Here an
/// unparseable stamp yields `None` ⇒ that line contributes no in-flight
/// evidence ⇒ the gate FAILS OPEN to today's unconditional-mint behavior, which
/// is the safe direction: the worst case is the pre-existing race, never a
/// permanently id-less `ls`.
///
/// Out-of-shape input is a `None`, never a panic: every field is bounds-checked
/// against the byte length before it is read.
pub fn iso_to_epoch_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    // Exactly `YYYY-MM-DDTHH:MM:SS.mmmZ` — 24 bytes, fixed separators.
    if b.len() != 24 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    if b[13] != b':' || b[16] != b':' || b[19] != b'.' || b[23] != b'Z' {
        return None;
    }
    let field = |lo: usize, hi: usize| -> Option<i64> {
        let mut acc: i64 = 0;
        for &c in &b[lo..hi] {
            if !c.is_ascii_digit() {
                return None;
            }
            acc = acc * 10 + i64::from(c - b'0');
        }
        Some(acc)
    };
    let year = field(0, 4)?;
    let month = field(5, 7)?;
    let day = field(8, 10)?;
    let hour = field(11, 13)?;
    let min = field(14, 16)?;
    let sec = field(17, 19)?;
    let milli = field(20, 23)?; // 3 digits ⇒ already in [0, 999]
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // `epoch_ms_to_iso` never emits hour 24, minute 60 or a leap second, so a
    // strict inverse rejects them.
    if hour > 23 || min > 59 || sec > 59 {
        return None;
    }
    let days = days_from_civil(year, month as u32, day as u32);
    // Range-check the DATE by round-tripping through the existing decomposer:
    // Feb 30 / Apr 31 / day 0 all land on a different civil date than the one
    // written, so this rejects them without a second calendar table.
    if civil_from_days(days) != (year, month as u32, day as u32) {
        return None;
    }
    Some((days * 86_400 + hour * 3600 + min * 60 + sec) * 1000 + milli)
}

/// days-from-civil (Howard Hinnant) — the forward direction of
/// [`civil_from_days`]: (y, m, d) → days since 1970-01-01. Valid for any civil
/// date in the proleptic Gregorian calendar; a caller passing an out-of-range
/// day gets a well-defined (but different) date back, which is how
/// [`iso_to_epoch_ms`] detects it.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 }); // [0, 11]
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `epoch_ms_to_iso` → `iso_to_epoch_ms` is the identity on real instants,
    /// including epoch 0 and a PRE-1970 (negative) one — the div_euclid/
    /// rem_euclid pairing in both directions is what makes the negative arm
    /// work, so it is pinned here rather than assumed.
    #[test]
    fn iso_round_trips_for_real_instants() {
        for ms in [
            0_i64,
            1,
            999,
            1000,
            1_717_530_000_000,   // 2024-06-04T15:40:00.000Z
            1_772_205_426_146,   // a 2026 stamp with ms
            -1,                  // 1969-12-31T23:59:59.999Z
            -86_400_000,         // 1969-12-31T00:00:00.000Z
            -2_208_988_800_000,  // 1900-01-01T00:00:00.000Z
            951_782_400_000,     // 2000-02-29 (leap day)
            4_102_444_799_999,   // 2099-12-31T23:59:59.999Z
        ] {
            let iso = epoch_ms_to_iso(ms);
            assert_eq!(
                iso_to_epoch_ms(&iso),
                Some(ms),
                "round trip failed for {ms} ({iso})"
            );
        }
    }

    /// The strictness that keeps the in-flight-mint gate from wedging: every
    /// off-shape stamp — the repo's `"t"` fixture placeholder foremost — is
    /// UNPARSEABLE, not "very old" and not "very new".
    #[test]
    fn iso_rejects_every_off_shape_stamp() {
        for bad in [
            "t",                        // the ids.jsonl fixture placeholder
            "",                         // empty
            "2026-06-21T15:17:06",      // truncated (no .mmm, no Z)
            "2026-06-21T15:17:06.146",  // truncated (no Z)
            "2026-06-21T15:17:06.146+00:00", // non-Z suffix (too long anyway)
            "2026-06-21T15:17:06.146z", // lowercase suffix
            "2026-06-21 15:17:06.146Z", // space instead of T (plausible, wrong)
            "2026/06/21T15:17:06.146Z", // wrong date separators
            "2026-06-21T15:17:06,146Z", // comma instead of the ms dot
            "2026-13-01T00:00:00.000Z", // month 13
            "2026-00-01T00:00:00.000Z", // month 0
            "2026-02-30T00:00:00.000Z", // Feb 30 (calendar-invalid)
            "2026-06-00T00:00:00.000Z", // day 0
            "2026-06-21T24:00:00.000Z", // hour 24
            "2026-06-21T15:60:00.000Z", // minute 60
            "2026-06-21T15:17:60.000Z", // leap second (never emitted)
            "20x6-06-21T15:17:06.146Z", // non-numeric year
            "2026-06-21T15:17:06.14zZ", // non-numeric ms
        ] {
            assert_eq!(iso_to_epoch_ms(bad), None, "should have rejected {bad:?}");
        }
    }

    /// A non-leap year rejects Feb 29 while the leap year accepts it — the
    /// round-trip range check is a real calendar check, not a `day <= 31` pass.
    #[test]
    fn iso_leap_day_is_calendar_checked() {
        assert_eq!(iso_to_epoch_ms("2026-02-29T00:00:00.000Z"), None);
        assert!(iso_to_epoch_ms("2024-02-29T00:00:00.000Z").is_some());
        assert_eq!(iso_to_epoch_ms("1900-02-29T00:00:00.000Z"), None); // century, not leap
        assert!(iso_to_epoch_ms("2000-02-29T00:00:00.000Z").is_some()); // 400-year leap
    }
}
