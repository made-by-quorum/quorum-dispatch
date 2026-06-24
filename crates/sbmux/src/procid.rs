//! Process-identity capture for the **agent-pid identity token** (W2 /
//! SPEC-v2 §5.A / R2-8 / P1).
//!
//! The token is `(pid_start_ms, boot_id)` of the recorded `session-opened.pid`
//! — the **PTY child (the agent process), NOT the sbmux daemon** (events.rs
//! `SessionOpened.pid`, captured in `Session::new` right after the PTY spawn).
//! It is the recycle-defeating discriminator bond uses for liveness: a
//! kernel start-time pins the incarnation **within** a boot; a boot-id
//! disambiguates **across** reboots. Both are read on the SAME box the token is
//! later consumed on (SPEC-v2 §2 same-box invariant), so producer (dispatch) and
//! consumer (bond) derive identical values by construction.
//!
//! **Fail-safe everywhere.** Any read/parse failure → `None`; the field is then
//! omitted from `session-opened` (`skip_serializing_if`) and bond treats absence
//! as crash-dead (never false-LIVE).
//!
//! **Resolution is ms-FLOORED on BOTH platforms (N1/D1):** darwin's source is
//! sub-ms (`pbi_start_tvusec`) but floored to ms (`/1000`); linux is
//! clock-TICK-bounded (~10 ms at `CLK_TCK=100`). The encoding manufactures no
//! granularity finer than the source — the effective discriminator is ~1 ms on
//! darwin, ~10 ms on linux (this bounds same-ms/tick pid-reuse, not same-second).
//!
//! **NOT a hot-path shell-out (R9):** the daemon never shells `ps`; darwin uses
//! libproc, linux reads `/proc`, and `boot_id` is memoized in a `OnceLock`
//! (constant per boot — read once, not per session-open).

use std::sync::OnceLock;

/// Kernel start-time of `pid` (the agent/child) as Unix epoch **MILLISECONDS**,
/// or `None` on `pid == 0` / any read failure (fail-safe).
///
/// `pid == 0` is the documented benign case (the spawn API yielded no pid —
/// events.rs "0 only if the spawn API yielded no pid", R6) and is **silent**.
/// A read failure on a **non-zero** pid emits a `tracing::warn!` at the failure
/// site (the underlying cause is in scope, M6d/N4) and still returns `None` —
/// no API change, the cause is not threaded out to the caller.
pub fn pid_start_ms(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None; // R6: benign, silent.
    }
    match pid_start_ms_inner(pid) {
        Ok(ms) => Some(ms),
        Err(cause) => {
            tracing::warn!(pid, %cause, "procid: pid_start_ms read failed — fail-safe None");
            None
        }
    }
}

/// Per-boot-stable **opaque** id, compared by EXACT string equality only (no
/// tolerance, no quantization). Memoized in a `OnceLock` — constant per boot,
/// so this reads the source at most once per process. `None` if unreadable
/// (containers / permissions → fail-safe, R9).
pub fn boot_id() -> Option<String> {
    static BOOT_ID: OnceLock<Option<String>> = OnceLock::new();
    BOOT_ID.get_or_init(boot_id_uncached).clone()
}

// ---------------------------------------------------------------------------
// Darwin (macOS 10.12+): libproc start-time + kern.bootsessionuuid.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn pid_start_ms_inner(pid: u32) -> Result<u64, String> {
    // Exactly the call SPEC-v2 §5.A names for bond's fallback → producer
    // (dispatch) and consumer (bond) derive bit-identical values by construction.
    // Avoids the hand-rolled `kinfo_proc` FFI (layout differs x86_64 vs aarch64).
    use libproc::bsd_info::BSDInfo;
    use libproc::proc_pid::pidinfo;
    match pidinfo::<BSDInfo>(pid as i32, 0) {
        // Sub-ms source FLOORED to ms (D1): tvsec*1000 + tvusec/1000.
        Ok(bsd) => Ok((bsd.pbi_start_tvsec as u64) * 1000 + (bsd.pbi_start_tvusec as u64) / 1000),
        Err(e) => Err(e),
    }
}

#[cfg(target_os = "macos")]
fn boot_id_uncached() -> Option<String> {
    // `kern.bootsessionuuid` — a per-boot UUID string (macOS 10.12+). NEVER
    // `kern.boottime`: it re-disciplines under NTP / wake-from-sleep, so under
    // string-equality it would read false crash-dead.
    let mut buf = [0u8; 64];
    let mut len: nix::libc::size_t = buf.len();
    let rc = unsafe {
        nix::libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None; // unavailable / unreadable → fail-safe.
    }
    // `len` includes the trailing NUL — strip it (saturating for the len==0 edge).
    let end = len.saturating_sub(1).min(buf.len());
    std::str::from_utf8(&buf[..end])
        .ok()
        .map(|s| s.trim().to_string())
        // F4 (defensive, unreachable in practice): kern.bootsessionuuid is always a
        // 36-char UUID, but totalize the fail-safe property — an empty/whitespace
        // value must be None, never Some("") (two empties compare equal → false-LIVE).
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Linux: /proc/<pid>/stat start-time + /proc/sys/kernel/random/boot_id.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn pid_start_ms_inner(pid: u32) -> Result<u64, String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|e| format!("/proc/{pid}/stat: {e}"))?;
    let proc_stat =
        std::fs::read_to_string("/proc/stat").map_err(|e| format!("/proc/stat: {e}"))?;
    // CLK_TCK via sysconf — clock ticks per second.
    let clk_tck = unsafe { nix::libc::sysconf(nix::libc::_SC_CLK_TCK) };
    if clk_tck <= 0 {
        return Err(format!("sysconf(_SC_CLK_TCK) returned {clk_tck}"));
    }
    parse_start_ms(&stat, &proc_stat, clk_tck as u64)
        .ok_or_else(|| format!("could not parse start-time from /proc/{pid}/stat"))
}

#[cfg(target_os = "linux")]
fn boot_id_uncached() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
}

// ---------------------------------------------------------------------------
// Unsupported platforms: fail-safe None (universal-None is acceptable green
// ONLY here — documented as unsupported; N5).
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn pid_start_ms_inner(_pid: u32) -> Result<u64, String> {
    Err("unsupported platform (no libproc / /proc start-time provider)".to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn boot_id_uncached() -> Option<String> {
    None
}

// ---------------------------------------------------------------------------
// Pure linux-format parser, split from IO (N3 testability). Platform-independent
// (string math only) so the PUBLISHED cross-impl fixture vector (§5.6a /
// EVENT-CONTRACT.md) is asserted on EVERY host, including darwin.
// ---------------------------------------------------------------------------

/// Parse epoch-ms start-time from a `/proc/<pid>/stat` line + a `/proc/stat`
/// body + the clock tick rate. `None` on any missing/garbage input (fail-safe).
///
/// The `comm` field (field 2) may contain spaces and `)`, so the **LAST `)`**
/// ends it; the remainder splits on whitespace with its first token = field 3
/// (`state`), making field 22 (`starttime`, ticks since boot) **index 19**.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_start_ms(stat: &str, proc_stat: &str, clk_tck: u64) -> Option<u64> {
    if clk_tck == 0 {
        return None; // no div-by-zero.
    }
    let after_comm = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // field 22 (starttime) = index 19 (field 3 is index 0).
    let starttime_ticks: u64 = fields.get(19)?.parse().ok()?;
    let btime: u64 = proc_stat
        .lines()
        .find_map(|l| l.strip_prefix("btime "))
        .and_then(|v| v.trim().parse().ok())?;
    // Integer (floor) truncation — resolution is ONE TICK (~10 ms at CLK_TCK=100),
    // NOT a true millisecond (N1).
    Some(btime * 1000 + starttime_ticks * 1000 / clk_tck)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N5 / §5.6b done-criterion: the provider returns a real `Some` for a live
    /// pid (THIS process) and is self-consistent across reads (an immutable
    /// kernel start-time). UNIVERSAL `None` for a live pid on a supported
    /// platform is a BUILD FAILURE.
    #[test]
    fn pid_start_ms_live_self_pid_is_some_and_stable() {
        let me = std::process::id();
        let a = pid_start_ms(me);
        let b = pid_start_ms(me);
        assert_eq!(a, b, "start-time of a live pid is immutable across reads");
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(
            a.is_some(),
            "N5: provider MUST return Some for a live pid on a supported platform (got None)"
        );
    }

    /// §5.5: the provider returns a REAL value — a recent epoch-ms, not garbage.
    /// This test process started moments ago, so its start-time is in the recent
    /// past and never in the future (a cross-check on the real darwin/linux
    /// provider, stronger than self-consistency alone).
    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn pid_start_ms_self_is_a_recent_real_epoch_ms() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let start = pid_start_ms(std::process::id()).expect("N5: Some on a supported platform");
        assert!(
            start <= now_ms + 1000,
            "start-time must not be in the future: start={start} now={now_ms}"
        );
        assert!(
            start >= now_ms.saturating_sub(24 * 3600 * 1000),
            "start-time is a recent real epoch-ms: start={start} now={now_ms}"
        );
    }

    /// R6: `pid == 0` is the benign no-child case → silent `None`.
    #[test]
    fn pid_start_ms_zero_is_silent_none() {
        assert_eq!(pid_start_ms(0), None);
    }

    /// §5.5 fail-safe + M6d warn: a non-zero impossible pid (above pid_max on
    /// both platforms) → fail-safe `None` AND the warn fires (error injection).
    #[test]
    fn pid_start_ms_impossible_pid_is_none_and_warns() {
        // i32::MAX as u32 = 2147483647 — positive in both u32 and i32 (NOT
        // u32::MAX, whose i32 cast -1 is a special pid), above pid_max → ENOENT.
        const IMPOSSIBLE_PID: u32 = i32::MAX as u32;
        let cap = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(cap.clone())
            .with_max_level(tracing::Level::WARN)
            .without_time()
            .finish();
        let got = tracing::subscriber::with_default(subscriber, || pid_start_ms(IMPOSSIBLE_PID));
        assert_eq!(got, None, "impossible pid → fail-safe None");
        let logged = cap.contents();
        assert!(
            logged.contains("pid_start_ms read failed"),
            "M6d: a non-zero-pid read failure MUST warn; captured: {logged:?}"
        );
        assert!(
            logged.contains("2147483647"),
            "the warn names the offending pid; captured: {logged:?}"
        );
    }

    /// `boot_id` is `Some` on a supported platform and memoized (constant per
    /// boot — identical across calls).
    #[test]
    fn boot_id_is_some_and_stable() {
        let a = boot_id();
        let b = boot_id();
        assert_eq!(a, b, "boot_id is memoized — constant per boot");
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(
            a.is_some(),
            "boot_id MUST be readable on a supported platform (got None)"
        );
    }

    /// §5.4: boot_id is compared by EXACT string equality — an injected
    /// differing boot-id compares UNEQUAL (the across-reboot discriminator; a
    /// real reboot is a manual / CI-host test, documented as such).
    #[test]
    fn boot_id_string_equality_distinguishes_reboots() {
        let recorded = "550e8400-e29b-41d4-a716-446655440000";
        let live_after_reboot = "00000000-0000-0000-0000-000000000000";
        assert_ne!(
            recorded, live_after_reboot,
            "a different boot → unequal under string-equality (false crash-dead avoided only because absence is fail-safe)"
        );
        assert_eq!(recorded, recorded, "same boot → equal");
    }

    /// §5.6a CROSS-IMPL LINUX parser fixture — the vector PUBLISHED VERBATIM in
    /// doc/EVENT-CONTRACT.md so bond's track (#7) asserts the SAME vector. The
    /// `comm` contains `) (` and spaces (the last-`)` parse). Pure → asserted on
    /// every host. btime=1700000000 s, starttime=22200 ticks, CLK_TCK=100 →
    /// 1700000000*1000 + 22200*1000/100 = 1_700_000_222_000.
    #[test]
    fn parse_start_ms_published_fixture_vector() {
        let stat = "1234 (my )( proc) S 1 1 1 0 -1 4194560 100 0 0 0 10 5 0 0 20 0 1 0 22200 0 0";
        let proc_stat = "cpu  1 2 3 4 5 6 7\nbtime 1700000000\nprocesses 9999\n";
        assert_eq!(
            parse_start_ms(stat, proc_stat, 100),
            Some(1_700_000_222_000),
            "published cross-impl fixture vector (see doc/EVENT-CONTRACT.md)"
        );
    }

    /// §5.3 (M4/N1) value property: distinct kernel start-times ⇒ distinct
    /// `pid_start_ms`, so the consumer's equality check catches a recycled pid.
    /// Asserted via the pure parser (avoids the linux-tick / darwin-ms flooring
    /// pitfalls of spawning real children, N1/D1).
    #[test]
    fn parse_start_ms_distinct_starttimes_distinct_values() {
        let mk = |st: &str| format!("1 (c) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 0 0 1 0 {st}");
        let proc_stat = "btime 1700000000\n";
        let a = parse_start_ms(&mk("22200"), proc_stat, 100);
        let b = parse_start_ms(&mk("22300"), proc_stat, 100);
        assert!(a.is_some() && b.is_some());
        assert_ne!(
            a, b,
            "distinct start-times ⇒ distinct pid_start_ms (recycle-defeating)"
        );
    }

    /// The last-`)` rule + missing / garbage / zero-tick inputs → `None`
    /// (fail-safe), never a panic.
    #[test]
    fn parse_start_ms_robustness() {
        // Too few fields after comm → None.
        assert_eq!(parse_start_ms("1 (x) S 1 1", "btime 1\n", 100), None);
        // starttime present but NO btime line → None.
        let stat = "1234 (x) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 0 0 1 0 22200";
        assert_eq!(parse_start_ms(stat, "cpu 1 2 3\n", 100), None);
        // clk_tck == 0 → None (no div-by-zero).
        assert_eq!(parse_start_ms(stat, "btime 1700000000\n", 0), None);
        // No `)` at all → None.
        assert_eq!(parse_start_ms("garbage", "btime 1\n", 100), None);
    }

    // ----- WARN-capture helper (tracing-subscriber dev-dep, fmt feature) -----
    #[derive(Clone, Default)]
    struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl LogCapture {
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = LogCapture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
}
