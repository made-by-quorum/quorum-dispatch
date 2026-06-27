//! `qd ping [session]` — REAL backend (A5 M5), ported from
//! `0d0fa9e:src/commands/ping.ts:357-388` (registerPingCommand action). The pure
//! classifier + resolution/sweep live in [`dispatch::ping`]; this is the thin binding:
//! gather the session list (the engine-native `getAllSessions({includeAll:true})`
//! + assignShortCodes), read the clock, dispatch single vs sweep, and emit.
//!
//! Arg validation (no session AND no --prefix → usage stderr + exit 3) stays REAL
//! here — it was the A3 honest stub in stubs.rs; A5 moves the real verb here while
//! preserving that exit-3 byte-for-byte.

use super::common;
use clap::ArgMatches;
use dispatch::effects::{Clock, RealClock};
use dispatch::join::JoinOpts;
use dispatch::ping::{run_health_prefix, run_health_single, Classification};

/// `registerPingCommand` action (ping.ts:367-388). Resolve the engine-native
/// session list once, then single-session or prefix-sweep. Exit per the FROZEN
/// classifier contract (band 0–4); 3 only on the no-target usage error or an
/// ambiguous NAME.
pub fn run(m: &ArgMatches) -> i32 {
    let session = m.get_one::<String>("session");
    let prefix = m.get_one::<String>("prefix");
    let json = m.get_flag("json");

    // No target → usage error, exit 3 (ping.ts:381-385; FROZEN — preserves the
    // A3 stubs.rs byte-shape).
    if prefix.is_none() && session.is_none() {
        eprintln!("qd ping: provide a <session> or --prefix <p>");
        return Classification::Error.exit_code();
    }

    // getAllSessions({includeAll:true}) + assignShortCodes (ping.ts:369-371).
    let opts = JoinOpts {
        include_all: true,
        include_tombstoned: true,
        include_preview: false,
        limit: None,
    };
    let sessions = match common::all_sessions(opts) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let now_ms = RealClock.now_ms();

    let res = if let Some(p) = prefix {
        run_health_prefix(p, &sessions, now_ms, json)
    } else {
        // session is Some (the no-target case returned above).
        run_health_single(session.unwrap(), &sessions, now_ms, json)
    };

    if !res.stdout.is_empty() {
        print!("{}", res.stdout);
    }
    if !res.stderr.is_empty() {
        eprint!("{}", res.stderr);
    }
    res.exit_code
}
