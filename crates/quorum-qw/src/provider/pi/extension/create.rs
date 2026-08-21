//! Create and revive for the `pi/extension` lane.
//!
//! # This is `pi/mux-pane` plus two things
//!
//! The lane is not a new create pipeline. It is the pi-TUI pane create —
//! [`plan_pi_tui`] then [`create_pi_tui`], the same capability preflight, the
//! same `--session-id` identity, the same anti-adoption guard, the same claim /
//! launch / verify choreography — with exactly two differences:
//!
//!   1. the launch carries `--quorum-sock <path>`, so the `quorum-lane`
//!      extension inside pi binds a control channel instead of staying inert;
//!   2. the row records `hosting: "extension"` and `endpoint: "unix://<path>"`.
//!
//! Everything else is shared code, deliberately. A second copy of the pi create
//! path is exactly the divergence this lane must not introduce.
//!
//! # Why the row fields are the highest-value lines here
//!
//! The row is what every LATER verb re-derives the lane from. Without
//! `hosting: "extension"` the session comes back as `pi/mux-pane` on the next
//! call, and `deliver` silently reverts to typing keystrokes into the PTY — a
//! working-looking session that quietly stopped using the channel it was
//! created for. Without the `endpoint`, nothing can find the socket. This is the
//! same lesson `codex/app-server`'s plan records as *its* highest-value line;
//! it is restated because it is the same trap, one lane over.
//!
//! # The `unix://` endpoint
//!
//! `endpoint` already carries `ws://127.0.0.1:<port>` for the codex daemon
//! lanes, so it is the field for "where this session's front door is". A
//! `unix://` scheme keeps it self-describing rather than storing a bare path
//! that reads like a file.

use std::path::PathBuf;

use crate::effects::Env;
use crate::provider::pane::PaneDeps;
use crate::registry;

use super::super::pty::pane::{
    create_pi_tui, plan_pi_tui, revive_preconditions, PiTuiError, PiTuiOutcome, PiTuiParams,
    PiTuiPlan,
};
use super::install;

/// The `endpoint` scheme for a control socket.
pub const ENDPOINT_SCHEME: &str = "unix://";

/// Render an endpoint for the row.
pub fn endpoint_for(path: &std::path::Path) -> String {
    format!("{ENDPOINT_SCHEME}{}", path.display())
}

/// Recover a socket path from a row's `endpoint`.
///
/// Permissive about the scheme being absent: a row hand-written by an operator
/// (or by an older build) that stores a bare path still resolves, because the
/// alternative is a session that cannot be reached for a cosmetic reason. It
/// refuses only what is unusable — an empty value, or one naming a different
/// transport.
pub fn socket_from_endpoint(endpoint: Option<&str>) -> Option<PathBuf> {
    let raw = endpoint.map(str::trim).filter(|e| !e.is_empty())?;
    if let Some(rest) = raw.strip_prefix(ENDPOINT_SCHEME) {
        return (!rest.is_empty()).then(|| PathBuf::from(rest));
    }
    if raw.contains("://") {
        return None; // ws:// etc — a different lane's row
    }
    Some(PathBuf::from(raw))
}

/// The socket a session uses, preferring what the row RECORDED over what the
/// path math would derive.
///
/// The order matters. Recomputing from `$TMPDIR` would silently address a
/// different socket than the live pi is serving whenever the environment
/// changed between create and now — a `qd` run from a different shell, a cron
/// job, a test harness with its own temp dir. The recorded value is what the
/// process actually bound; the derivation is only a fallback for rows written
/// before the endpoint existed.
pub fn socket_for(env: &dyn Env, endpoint: Option<&str>, session_id: &str) -> PathBuf {
    socket_from_endpoint(endpoint).unwrap_or_else(|| install::socket_path(env, session_id))
}

/// Everything one extension-lane launch needs to know beyond the pane plan.
pub struct ExtensionLaunch {
    pub plan: PiTuiPlan,
    /// Where the extension will bind, and what goes in the row's `endpoint`.
    pub socket: PathBuf,
}

/// Phase 1 for this lane: the pi-TUI plan, plus the extension on disk and a
/// socket directory to bind in.
///
/// Installing HERE, before anything is claimed or spawned, is deliberate. The
/// install can fail (no HOME, a read-only home) and a launch that proceeded
/// past that failure would produce a pane whose pi comes up perfectly and
/// simply never opens a channel — a session that looks created and cannot be
/// driven. Refusing before the claim keeps that unrepresentable.
pub fn plan_extension_launch(
    env: &dyn Env,
    params: &PiTuiParams,
) -> Result<ExtensionLaunch, PiTuiError> {
    let plan = plan_pi_tui(env, params)?;

    install::install(env).map_err(|e| PiTuiError::CapabilityProbeFailed {
        bin: install::installed_path(env)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.pi/agent/extensions".to_string()),
        why: e.to_string(),
    })?;
    install::ensure_socket_dir(env).map_err(|e| PiTuiError::CapabilityProbeFailed {
        bin: install::socket_dir(env).display().to_string(),
        why: e.to_string(),
    })?;

    let socket = install::socket_path(env, &plan.session_id);
    // Refuse an environment that cannot host a socket BEFORE the claim. `bind(2)`
    // fails inside the launched pi, where the launcher never sees it, so without
    // this the symptom is a healthy pane with a channel that never appears.
    if let Some(why) = install::socket_path_too_long(&socket) {
        return Err(PiTuiError::CapabilityProbeFailed {
            bin: install::socket_dir(env).display().to_string(),
            why,
        });
    }
    // THE line that makes the launched pi serve a channel. Without it the pane
    // comes up as an ordinary `pi/mux-pane` TUI wearing an `extension` row —
    // which is worse than either lane, because every later verb would then
    // address a channel that was never opened.
    let mut plan = plan;
    plan.control_socket = Some(socket.to_string_lossy().into_owned());
    // A socket left by a predecessor on this id would be connected to by the
    // readiness probe before the new pi ever binds, and the probe would pass
    // against a corpse. The extension also clears it, but the launcher clearing
    // it first is what makes the probe's success meaningful.
    install::remove_socket(&socket);

    Ok(ExtensionLaunch { plan, socket })
}

/// Phase 2: create the pane, then correct the row this lane owns.
///
/// # Why the row is PATCHED rather than written differently
///
/// [`create_pi_tui`] writes a complete, correct `pi/mux-pane` row. Teaching it
/// to write two different shapes would mean threading lane identity through the
/// shared create path — the coupling the whole qd/qw split exists to remove.
/// Re-reading and re-writing the row it just wrote costs one file round trip and
/// keeps the shared path unaware that this lane exists.
///
/// The patch is not optional and its failure is not cosmetic: a session left
/// with `hosting: "mux-pane"` is a session that silently reverts to PTY
/// keystrokes forever. So a failed patch fails the create.
pub fn create_extension_session(
    deps: &PaneDeps<'_>,
    launch: &ExtensionLaunch,
) -> Result<PiTuiOutcome, PiTuiError> {
    let out = create_pi_tui(deps, &launch.plan)?;

    let name = launch.plan.name.clone();
    let Some(mut row) = registry::read_entries(&deps.paths.sessions_dir, false)
        .into_iter()
        .map(|s| s.entry)
        .find(|e| e.session_id.as_deref() == Some(launch.plan.session_id.as_str()))
    else {
        return Err(PiTuiError::RowWriteFailed {
            name,
            detail: format!(
                "the pane was created but its registry row for session {} could not be re-read \
                 to record the control channel",
                launch.plan.session_id
            ),
        });
    };

    row.hosting = Some(crate::lane::Mode::Extension.hosting_token().to_string());
    row.endpoint = Some(endpoint_for(&launch.socket));

    if let Err(detail) = registry::write_entry(&deps.paths.sessions_dir, &row) {
        return Err(PiTuiError::RowWriteFailed {
            name,
            detail: format!("cannot record the control channel on the row: {detail}"),
        });
    }

    Ok(out)
}

/// Revive a stopped extension-lane session into the SAME pi session.
///
/// The pi-TUI revive semantics carry over verbatim — identity is carried, not
/// rediscovered, and there is no "never used" refusal because a pi row has had
/// its id since birth (see [`super::super::pty::pane::revive_pi_tui`] for the
/// full reasoning). What this adds is the same row correction the create makes,
/// for the same reason: a revived session that came back as `pi/mux-pane` would
/// have quietly lost its control channel.
pub fn revive_extension_session(
    deps: &PaneDeps<'_>,
    launch: &ExtensionLaunch,
    old_pid: Option<i64>,
) -> Result<PiTuiOutcome, PiTuiError> {
    revive_preconditions(Some(&launch.plan.name), &launch.plan.session_id)?;

    let out = create_extension_session(deps, launch)?;

    // Consume the prior tombstone so one session does not leave a dangling
    // second row — the same best-effort cleanup `revive_pi_tui` performs.
    if let Some(old_pid) = old_pid.filter(|&p| p != 0) {
        let _ = std::fs::remove_file(
            deps.paths
                .sessions_dir
                .join(format!("{old_pid}.json.tombstoned")),
        );
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_round_trips() {
        let p = PathBuf::from("/tmp/quorum-pi/abc.sock");
        assert_eq!(endpoint_for(&p), "unix:///tmp/quorum-pi/abc.sock");
        assert_eq!(socket_from_endpoint(Some(&endpoint_for(&p))), Some(p));
    }

    #[test]
    fn a_bare_path_endpoint_still_resolves() {
        assert_eq!(
            socket_from_endpoint(Some("/tmp/quorum-pi/abc.sock")),
            Some(PathBuf::from("/tmp/quorum-pi/abc.sock"))
        );
    }

    /// Another lane's row must not resolve to a socket. A `ws://` endpoint is a
    /// codex daemon's, and treating it as a filesystem path would have this
    /// lane trying to connect to a file named `ws:`.
    #[test]
    fn a_ws_endpoint_is_refused() {
        assert_eq!(socket_from_endpoint(Some("ws://127.0.0.1:5055")), None);
    }

    #[test]
    fn empty_and_absent_endpoints_are_none() {
        assert_eq!(socket_from_endpoint(None), None);
        assert_eq!(socket_from_endpoint(Some("")), None);
        assert_eq!(socket_from_endpoint(Some("   ")), None);
        assert_eq!(socket_from_endpoint(Some("unix://")), None);
    }

    /// The recorded endpoint WINS over derivation. This is the property that
    /// keeps a session reachable after `$TMPDIR` changes between two `qd` runs.
    #[test]
    fn recorded_endpoint_beats_derived_path() {
        struct E;
        impl Env for E {
            fn var(&self, k: &str) -> Option<String> {
                (k == "TMPDIR").then(|| "/somewhere/else".to_string())
            }
            fn uid(&self) -> u32 {
                0
            }
        }
        let got = socket_for(&E, Some("unix:///tmp/quorum-pi/real.sock"), "sid");
        assert_eq!(got, PathBuf::from("/tmp/quorum-pi/real.sock"));
    }

    #[test]
    fn derivation_is_the_fallback_when_the_row_says_nothing() {
        struct E;
        impl Env for E {
            fn var(&self, k: &str) -> Option<String> {
                (k == "TMPDIR").then(|| "/tmp".to_string())
            }
            fn uid(&self) -> u32 {
                0
            }
        }
        // Compared against the derivation itself, not a literal: the FILENAME
        // is a hash (see `install::socket_path` on the `sun_path` overrun that
        // forced it), and pinning the spelling here would just re-encode that
        // choice in a second place.
        assert_eq!(
            socket_for(&E, None, "sid"),
            install::socket_path(&E, "sid")
        );
        assert!(socket_for(&E, None, "sid").starts_with("/tmp/quorum-pi"));
    }
}
