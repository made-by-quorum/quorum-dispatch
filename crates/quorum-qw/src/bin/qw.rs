//! **`qw` — the session binary.**
//!
//! A thin process shell around [`quorum_qw::lanes`], on the
//! `recovery_coordinator` precedent (`dispatch/Cargo.toml:132-138`: "A SEPARATE
//! bin … so the `qd` binary stays byte-intact; it is a thin process shell around
//! `dispatch::recovery::coordinator_pass`"). Everything it knows how to do is a
//! [`quorum_qw::LaneOps`] method; everything it decides, it decides from its own
//! environment.
//!
//! # It lives in `quorum-qw`, not in the `dispatch` package
//!
//! Deliberately. A `qw` binary under `dispatch/src/bin/` would link the `dispatch`
//! library, and the dependency direction is the load-bearing rule of this whole
//! split: **qd depends on qw, never the reverse.** A qw that could see
//! `dispatch::` would be one refactor away from calling back into it, which is the
//! coupling the process boundary exists to make impossible. Putting the binary in
//! `quorum-qw` makes that structural rather than a matter of discipline — the
//! symbol is not reachable, so the mistake cannot compile.
//!
//! # Verbs
//!
//! - `serve` — speak the [`quorum_qw::wire`] protocol on stdin/stdout. This is
//!   what `WireLane` spawns.
//! - `attach <lane> <session-id>` — the terminal handoff. Not a wire call: it
//!   inherits stdio and hands the controlling terminal to the session. See
//!   `LaneOps::attach`.
//! - `build-profile` — required by `scripts/deploy-gate.sh:124`, which refuses to
//!   install a binary that cannot answer it.
//!
//! And the three HIDDEN resident entries — `qrmux-server`, `acp-daemon`,
//! `pi-daemon` — which are here because of what `current_exe()` means now
//! (`11-stage3-plan.md` ruling D6). `lanes.rs`'s `self_exe()`/`create_exe()` and
//! `embedded_mux.rs`'s `embedder_launch_spec()` stand a resident up by re-execing
//! `current_exe()` with one of those verbs; once every lane call runs inside this
//! process, `current_exe()` is **`qw`**, and a `qw` that could not answer them
//! failed every create and every revive that needs a resident. `qd` keeps its own
//! copies of all three: residents already running in the wild were spawned as
//! `qd <verb>`, and the cmdline-identity probes that reap them key on the VERB
//! substring rather than on the program — so both spellings are recognised as ours,
//! in both directions, across the change.
//!
//! Parsed by hand rather than with clap, like `qd`'s own pre-clap verbs
//! (`bin/qd/main.rs:55-73`): these are machine-spawned entries, not a user-facing
//! CLI, and there is no help surface to keep byte-stable.

use std::io::{self, BufReader};

use quorum_qw::contract::{LaneError, LaneOps, SessionId};
use quorum_qw::effects::RealEnv;
use quorum_qw::lane::Lane;
use quorum_qw::paths::QdPaths;

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    std::process::exit(run(&argv[1..]));
}

fn run(rest: &[String]) -> i32 {
    match rest.first().map(String::as_str) {
        Some(quorum_qw::wire::SERVE_VERB) => serve(),
        Some(quorum_qw::wire::ATTACH_VERB) => attach(&rest[1..]),
        Some("build-profile") => {
            // The deploy gate reads this to prove a staged binary is the build it
            // thinks it is. `qd`'s answer is at `bin/qd/main.rs:109-112`.
            println!("{}", build_profile());
            0
        }
        // The three hidden resident entries (ruling D6). Dispatched pre-clap for
        // the same reason `qd` dispatches them pre-clap: they are machine-spawned,
        // and a resident's argv must never be reshaped by a user-facing parser.
        // Each arm names the SAME const the cmdline-identity probe greps for, so a
        // resident spawned here is reaped by the code that reaps qd's.
        Some(quorum_qw::qrmux_server::VERB) => {
            quorum_qw::qrmux_server::run_qrmux_server("qw qrmux-server", &rest[1..])
        }
        Some(quorum_qw::provider::acp::residence::ADAPTER_VERB) => {
            quorum_qw::provider::acp::residence::run_adapter(&rest[1..])
        }
        Some(quorum_qw::provider::pi::residence::PI_ADAPTER_VERB) => {
            quorum_qw::provider::pi::residence::run_pi_adapter(&rest[1..])
        }
        Some("--version") | Some("-V") => {
            println!("qw {}", env!("CARGO_PKG_VERSION"));
            0
        }
        other => {
            eprintln!(
                "qw: unknown verb {:?}. qw is spawned by qd, not run directly; \
                 verbs are `serve`, `attach <lane> <id>`, `build-profile`, and the \
                 resident entries `qrmux-server`, `acp-daemon`, `pi-daemon`.",
                other.unwrap_or("")
            );
            2
        }
    }
}

fn build_profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

/// Resolve the effects a lane needs, from this process's own environment.
///
/// Nothing here crosses the wire, and that is the design: `Env` is the environ qd
/// handed us by inheritance, `QdPaths` derives from `HOME`/`QD_HOME`, and the mux
/// is selected from `QD_MUX` inside `lane_ops`. Sending any of them as frame data
/// would mean qd deciding something about a session, which is the coupling this
/// binary exists to end.
fn resolve_paths(env: &RealEnv) -> Result<QdPaths, String> {
    let home = quorum_qw::effects::Env::var(env, "HOME")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "qw: HOME is not set — cannot resolve the session state dir.".to_string())?;
    Ok(QdPaths::from_home_env(std::path::Path::new(&home), env))
}

fn serve() -> i32 {
    let env = RealEnv;
    let paths = match resolve_paths(&env) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    // stdin/stdout are the protocol; stderr was inherited from qd and belongs to
    // the user's terminal.
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    match quorum_qw::wire::server::serve(&env, &paths, &mut reader, &mut writer) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("qw serve: {e}");
            1
        }
    }
}

fn attach(args: &[String]) -> i32 {
    let (Some(lane_id), Some(session)) = (args.first(), args.get(1)) else {
        eprintln!("qw attach: usage: qw attach <lane> <session-id>");
        return 2;
    };
    let Some(lane) = Lane::from_id(lane_id) else {
        eprintln!("qw attach: {lane_id:?} is not a lane");
        return 2;
    };
    let env = RealEnv;
    let paths = match resolve_paths(&env) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let ops = quorum_qw::lanes::lane_ops(lane, &env, paths);
    match ops.attach(&SessionId(session.clone())) {
        Ok(code) => code,
        // The handoff never happened, so there is no attached program's exit code
        // to report. qd's `attach` verb owns the user-facing wording for these; qw
        // prints the detail and exits nonzero so the caller can distinguish "the
        // session ran and exited 1" from "there was nothing to attach to".
        Err(LaneError::NotSupported { reason, .. }) => {
            eprintln!("qw attach: {reason}");
            1
        }
        Err(e) => {
            eprintln!("qw attach: {e:?}");
            1
        }
    }
}
