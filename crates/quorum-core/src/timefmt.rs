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
