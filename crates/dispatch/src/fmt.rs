//! M1: pure formatters, TS src/session.ts:1243-1275.
//!
//! Port of relativeTime / shortenPath / truncateId / formatTokens. All four are
//! pure: `relative_time` takes `now_ms` as a parameter (the TS reads `Date.now()`
//! inline, but the Rust port keeps the Clock seam outside this module — callers
//! pass the injected clock's value); `shorten_path` takes `home` explicitly
//! (the TS closes over module-level `HOME = homedir()`, which is exactly the
//! load-bearing real-home access L9a forbids inside deciders).

/// Render `relative_time` as the TS does (src/session.ts:1243-1254): floor
/// division through s → m → h → d. `now_ms` is injected (TS reads `Date.now()`).
pub fn relative_time(date_ms: i64, now_ms: i64) -> String {
    // Port of relativeTime, session.ts:1243-1254. diff in ms, floored at each tier.
    let diff = now_ms - date_ms;
    let seconds = diff.div_euclid(1000);
    if seconds < 60 {
        return format!("{seconds}s ago");
    }
    let minutes = seconds.div_euclid(60);
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes.div_euclid(60);
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours.div_euclid(24);
    format!("{days}d ago")
}

/// Port of shortenPath, session.ts:1256-1265. The Dropbox CloudStorage prefix is
/// checked FIRST (it is a longer, more specific prefix that also starts with
/// HOME), then the plain home → "~". `home` is injected (TS closes over the
/// module-level `HOME = homedir()`, the real-home access L9a forbids in deciders).
pub fn shorten_path(p: &str, home: &str) -> String {
    // Dropbox CloudStorage special-case FIRST — it begins with HOME, so the plain
    // home check below would otherwise eat the prefix and yield ~/Library/...
    let dropbox_prefix = format!("{home}/Library/CloudStorage/Dropbox");
    if let Some(rest) = p.strip_prefix(&dropbox_prefix) {
        return format!("~/Dropbox{rest}");
    }
    if let Some(rest) = p.strip_prefix(home) {
        return format!("~{rest}");
    }
    p.to_string()
}

/// Port of truncateId, session.ts:1267-1269. TS `(id ?? "").slice(0, len)` —
/// here the empty-string case is just an empty `&str`. `slice` is by UTF-16 code
/// units in JS; session ids are hex/ascii so a char-prefix matches. Default len 8.
pub fn truncate_id(id: &str, len: usize) -> String {
    id.chars().take(len).collect()
}

/// `truncate_id` with the TS default `len = 8` (session.ts:1267).
pub fn truncate_id_default(id: &str) -> String {
    truncate_id(id, 8)
}

/// Port of formatTokens, session.ts:1271-1275. `>=1M → "{:.1}M"`, `>=1k →
/// "{:.1}k"`, else the plain integer. The `.toFixed(1)` is replicated by
/// [`js_to_fixed_1`] (Rust's native `{:.1}` rounds half-to-EVEN, JS rounds
/// half-AWAY-from-zero on exact ties — see that fn).
pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{}M", js_to_fixed_1(n as f64 / 1_000_000.0))
    } else if n >= 1_000 {
        format!("{}k", js_to_fixed_1(n as f64 / 1_000.0))
    } else {
        n.to_string()
    }
}

/// Replicate JavaScript `Number.prototype.toFixed(1)`.
///
/// Rust's native `format!("{:.1}", x)` agrees with JS `toFixed(1)` for nearly
/// every value because both round the TRUE f64 value (not the source decimal):
/// e.g. `1.15` is stored as `1.1499999…` so both yield `1.1`, and `1.95` is
/// `1.9499999…` so both yield `1.9`. The ONLY divergence is on values whose f64
/// is an EXACT tenths-tie (`1.25`, `2.25`, `3.75`, …, the exactly-representable
/// quarters that land on `.x5`): there JS rounds the tie away from zero (`1.25`
/// → `1.3`) while Rust rounds to even (`1.25` → `1.2`).
///
/// We detect a true tie by Rust's exact decimal expansion (`{:.30}` prints the
/// f64's real value: a tie shows a `5` in the 2nd fractional place followed by
/// all zeros) and round it half-up; everything else delegates to native `{:.1}`.
///
/// Verified empirically against `bun` (the production runtime), e.g.:
///   `bun -e 'console.log((1.05).toFixed(1))'`   → 1.1   (Rust native: 1.1, agree)
///   `bun -e 'console.log((1.25).toFixed(1))'`   → 1.3   (Rust native: 1.2, FIXED here)
///   `bun -e 'console.log((1.15).toFixed(1))'`   → 1.1   (Rust native: 1.1, agree)
///   `bun -e 'console.log((1.95).toFixed(1))'`   → 1.9   (Rust native: 1.9, agree)
///   `bun -e 'console.log((2.05).toFixed(1))'`   → 2.0   (Rust native: 2.0, agree)
/// and against the formatTokens path:
///   1250 → "1.3k", 2250 → "2.3k" (the two ties), 1950000 → "1.9M", 1049 → "1.0k".
fn js_to_fixed_1(x: f64) -> String {
    let neg = x.is_sign_negative() && x != 0.0;
    let a = x.abs();
    // Exact expansion: Rust prints the f64's true value, so a genuine tenths-tie
    // is the ONLY case where digit[1] == '5' and every later digit is '0'.
    let exact = format!("{a:.30}");
    let dot = exact
        .find('.')
        .expect("{:.30} always emits a decimal point");
    let frac = &exact.as_bytes()[dot + 1..];
    let is_tie = frac.len() >= 2 && frac[1] == b'5' && frac[2..].iter().all(|&b| b == b'0');

    let s = if is_tie {
        // True tie → JS rounds half-AWAY-from-zero (up, since we work on |x|).
        let d1 = u32::from(frac[0] - b'0');
        let int_part: u64 = exact[..dot]
            .parse()
            .expect("integer part is decimal digits");
        let bumped = d1 + 1;
        if bumped == 10 {
            format!("{}.0", int_part + 1)
        } else {
            format!("{int_part}.{bumped}")
        }
    } else {
        // Native formatter already matches JS for every non-tie value.
        format!("{a:.1}")
    };
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- relative_time: floor division at each tier (session.ts:1243-1254) ---

    #[test]
    fn relative_time_tiers() {
        let now = 1_000_000_000_000;
        assert_eq!(relative_time(now, now), "0s ago");
        assert_eq!(relative_time(now - 30_000, now), "30s ago");
        assert_eq!(relative_time(now - 59_999, now), "59s ago"); // floor: 59.999s -> 59s
        assert_eq!(relative_time(now - 60_000, now), "1m ago");
        assert_eq!(relative_time(now - 90_000, now), "1m ago"); // floor 1.5m -> 1m
        assert_eq!(relative_time(now - 59 * 60_000, now), "59m ago");
        assert_eq!(relative_time(now - 60 * 60_000, now), "1h ago");
        assert_eq!(relative_time(now - 23 * 3_600_000, now), "23h ago");
        assert_eq!(relative_time(now - 24 * 3_600_000, now), "1d ago");
        assert_eq!(relative_time(now - 10 * 24 * 3_600_000, now), "10d ago");
    }

    // --- shorten_path: Dropbox prefix FIRST, then plain home (session.ts:1256-1265) ---

    #[test]
    fn shorten_path_dropbox_first() {
        let home = "/home/u";
        assert_eq!(
            shorten_path("/home/u/Library/CloudStorage/Dropbox/ail/x", home),
            "~/Dropbox/ail/x"
        );
    }

    #[test]
    fn shorten_path_plain_home() {
        let home = "/home/u";
        assert_eq!(shorten_path("/home/u/work/qd", home), "~/work/qd");
    }

    #[test]
    fn shorten_path_outside_home_untouched() {
        let home = "/home/u";
        assert_eq!(shorten_path("/tmp/zmx-501", home), "/tmp/zmx-501");
    }

    #[test]
    fn shorten_path_dropbox_takes_precedence_over_home() {
        // The Dropbox prefix begins with HOME; if the plain-home branch ran first
        // it would yield "~/Library/CloudStorage/Dropbox/..." — the FIRST check guards that.
        let home = "/home/u";
        let p = "/home/u/Library/CloudStorage/Dropbox/file";
        assert_eq!(shorten_path(p, home), "~/Dropbox/file");
    }

    // --- truncate_id (session.ts:1267-1269) ---

    #[test]
    fn truncate_id_default_len() {
        assert_eq!(truncate_id_default("0123456789abcdef"), "01234567");
        assert_eq!(truncate_id_default(""), "");
        assert_eq!(truncate_id_default("abc"), "abc"); // shorter than len
    }

    #[test]
    fn truncate_id_custom_len() {
        assert_eq!(truncate_id("0123456789", 4), "0123");
    }

    // --- format_tokens: boundaries verified against `bun`, outputs recorded ---
    //
    // Verification command (bun is the production runtime):
    //   bun -e 'function f(n){if(n>=1e6)return (n/1e6).toFixed(1)+"M";
    //           if(n>=1e3)return (n/1e3).toFixed(1)+"k";return String(n);}
    //           [999,1000,1049,1050,1250,1500,2250,2500,9999,1000000,1234567,1950000,1000500]
    //           .forEach(n=>console.log(n,f(n)))'
    // Actual bun outputs recorded inline below.

    #[test]
    fn format_tokens_plain_below_1k() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999"); // bun: 999
    }

    #[test]
    fn format_tokens_k_boundary() {
        assert_eq!(format_tokens(1000), "1.0k"); // bun: 1.0k
        assert_eq!(format_tokens(1049), "1.0k"); // bun: 1.0k (1.049 -> 1.0)
        assert_eq!(format_tokens(1050), "1.1k"); // bun: 1.1k
        assert_eq!(format_tokens(1500), "1.5k"); // bun: 1.5k
        assert_eq!(format_tokens(2500), "2.5k"); // bun: 2.5k
        assert_eq!(format_tokens(9999), "10.0k"); // bun: 10.0k
    }

    #[test]
    fn format_tokens_js_tie_cases() {
        // The two values where JS toFixed(1) (half-away) diverges from Rust native
        // {:.1} (half-even). bun: 1250 -> 1.3k, 2250 -> 2.3k.
        assert_eq!(format_tokens(1250), "1.3k");
        assert_eq!(format_tokens(2250), "2.3k");
    }

    #[test]
    fn format_tokens_m_boundary() {
        assert_eq!(format_tokens(1_000_000), "1.0M"); // bun: 1.0M
        assert_eq!(format_tokens(1_000_500), "1.0M"); // bun: 1.0M (1.0005 -> 1.0)
        assert_eq!(format_tokens(1_234_567), "1.2M"); // bun: 1.2M
        assert_eq!(format_tokens(1_950_000), "1.9M"); // bun: 1.9M (NOT 2.0M)
    }

    #[test]
    fn js_to_fixed_1_matches_bun_raw_cases() {
        // Raw .toFixed(1) values, each recorded from `bun -e 'console.log((v).toFixed(1))'`.
        assert_eq!(js_to_fixed_1(1.05), "1.1"); // bun: 1.1
        assert_eq!(js_to_fixed_1(2.05), "2.0"); // bun: 2.0
        assert_eq!(js_to_fixed_1(1.95), "1.9"); // bun: 1.9
        assert_eq!(js_to_fixed_1(1.15), "1.1"); // bun: 1.1
        assert_eq!(js_to_fixed_1(1.25), "1.3"); // bun: 1.3 (exact tie -> up)
        assert_eq!(js_to_fixed_1(1.35), "1.4"); // bun: 1.4
        assert_eq!(js_to_fixed_1(1.45), "1.4"); // bun: 1.4
        assert_eq!(js_to_fixed_1(1.55), "1.6"); // bun: 1.6
        assert_eq!(js_to_fixed_1(1.65), "1.6"); // bun: 1.6
        assert_eq!(js_to_fixed_1(2.25), "2.3"); // bun: 2.3 (exact tie -> up)
        assert_eq!(js_to_fixed_1(3.75), "3.8"); // bun: 3.8 (exact tie -> up)
                                                // 9.95 in f64 is 9.9499… (NOT a tie), so JS gives 9.9 — verified:
                                                //   bun -e 'console.log((9.95).toFixed(1))' -> 9.9
        assert_eq!(js_to_fixed_1(9.95), "9.9"); // bun: 9.9
                                                // 9.96 rounds up across the integer boundary; verify the carry path
                                                //   bun -e 'console.log((9.96).toFixed(1))' -> 10.0
        assert_eq!(js_to_fixed_1(9.96), "10.0"); // bun: 10.0 (native {:.1}, no tie)
                                                 // An exact tie that carries: 9.95 isn't one, but check a synthetic carry via
                                                 // the tie path is covered by format_tokens(9999) -> "10.0k" already.
    }
}
