//! Child D (opencode D1) — the `acp/claude-code` transport-loss disposition:
//! REFUSE and surface (the `codex`/`acp/opencode` HumanRecoveryOnly class),
//! with the session's identity first preserved in the qd-owned tombstone
//! store ([`crate::tombstone`]; clerk-4's Arm-B ratification, bond note
//! 01KX01BY7G — the Arm-A auto-deliver redesign was DECLINED).
//!
//! This is the ONLY new behavior on a loss: one best-effort identity write +
//! one observability line. The refusal surface and exit code are the caller's
//! existing refusal, unchanged — and for any provider other than
//! `acp/claude-code` this module is a no-op, so codex/opencode refusals stay
//! byte-identical (their identity rows are not at issue here).
//!
//! Moved out of `bin/qd/verbs/acp_loss.rs` with [`super::acp::send_acp`], its
//! only caller. Its one behavioural change is the module-wide one: it RETURNS
//! its observability line as a note instead of printing it. See [`super`].

use crate::effects::Env;
use crate::model::Session;
use crate::paths::QdPaths;
use crate::tombstone::{self, IdentityTombstone};

/// The one provider whose loss identity is preserved (the named divergence's
/// scope — matches the retired floor's `floor_eligible` scope exactly).
const TOMBSTONE_PROVIDER: &str = "acp/claude-code";

/// Best-effort: record `session`'s identity in the qd-owned tombstone store
/// before the caller refuses. Never changes the caller's exit path — a store
/// failure yields a note and is otherwise swallowed (the refusal must land
/// regardless).
///
/// Scope gate FIRST: any provider but `acp/claude-code` returns `None`
/// immediately (no write, no output — S4's byte-identical refusal for
/// codex/opencode).
///
/// The store root is `<QD_HOME || ~/.quorum/dispatch>/state/tombstones/`
/// (resolved via [`QdPaths::from_home_env`], the `ids.jsonl` precedent — NOT
/// the caller's `.claude`-layout paths, which ignore QD_HOME) — a directory the
/// claude CLI's dead-pid janitor never reads, so the record survives the ~1s reap
/// of the session's own `~/.claude/sessions/<pid>.json` row and beyond.
///
/// The registry row is re-read here (by pid) for the recorded ACP `endpoint`:
/// at loss time the janitor may already have reaped it — then the endpoint is
/// simply absent and everything else still lands from the resolved `Session`.
///
/// Returns the observability line the caller prints, or `None` when there is
/// nothing to say (out of scope, or no resolvable HOME).
///
/// `verb` is the CALLER's command word, and the line is `qd <verb>:`-attributed
/// for the reason [`crate::delivery::CarrierError::line`] takes one: this body
/// is SHARED between the send seam ([`super::acp::send_acp`]) and the wait seam
/// ([`crate::idle::await_idle_acp`]), so no fixed word can be right for both.
/// It used to say a bare `qd:`, which was defensible while ONE process wrote
/// every line and this one was common to both paths; after the qd/qw split the
/// writer is the `qw` child, so `qd:` names a process that did not write it.
/// `qw:` would be worse: the user typed `qd send:relay` or `qd wait` and has
/// never typed `qw` — naming the helper binary would attribute the line to a
/// command no one ran. So it carries the verb the user typed, exactly like the
/// refusal it precedes.
pub fn preserve_identity(
    env: &dyn Env,
    session: &Session,
    reason: &str,
    verb: &str,
) -> Option<String> {
    if session.provider != TOMBSTONE_PROVIDER {
        return None;
    }
    // No HOME ⇒ no resolvable store; the caller's refusal still lands.
    let home = env.var("HOME").filter(|s| !s.is_empty())?;
    let paths = QdPaths::from_home_env(std::path::Path::new(&home), env);

    let endpoint = session
        .pid
        .filter(|&p| p != 0)
        .and_then(|pid| crate::registry::read_entry(&paths.sessions_dir, pid))
        .and_then(|e| e.endpoint)
        .filter(|s| !s.is_empty());

    let record = IdentityTombstone {
        session_id: session.session_id.clone(),
        name: session.name.clone(),
        pid: session.pid,
        cwd: session.cwd.clone(),
        provider: session.provider.clone(),
        endpoint,
        transcript: session.jsonl_path.clone(),
        loss_reason: reason.to_string(),
        ..IdentityTombstone::default()
    };
    Some(
        match tombstone::record_loss(&paths.state_dir, record, tombstone::wall_now_ms()) {
            Ok(path) => {
                let label = session
                    .name
                    .clone()
                    .unwrap_or_else(|| session.session_id.clone());
                format!(
                    "qd {verb}: acp: session \"{label}\" identity preserved at {}",
                    path.display()
                )
            }
            Err(e) => {
                format!(
                    "qd {verb}: acp: could not preserve the session identity ({e}) — refusing anyway"
                )
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::RealEnv;
    use crate::model::{SessionBranch, SessionStatus};

    // The scope gate is the S4 guard: a non-acp/claude-code provider must be a
    // total no-op (no store resolution, no write, no output). Behavioral proof
    // lives in the integration lane (tests/acp_fallback.rs drives the store
    // through QD_HOME); this pins the gate itself.
    #[test]
    fn preserve_identity_is_a_no_op_for_other_providers() {
        for provider in ["codex", "acp/opencode", "claude-code", "pi", ""] {
            let session = Session {
                provider: provider.to_string(),
                session_id: "sess-noop".to_string(),
                ..blank_session()
            };
            // Must not panic, must not write anywhere, and must say NOTHING (the
            // gate returns before resolving a store path).
            assert!(preserve_identity(&RealEnv, &session, "test reason", "wait").is_none());
        }
    }

    // THE ATTRIBUTION, pinned on BOTH seams. The line used to open with a bare
    // `qd:` — a process name, and after the qd/qw split not even the process
    // that writes it. It now opens with the command the user typed, and this
    // row proves the SAME body answers differently for the two callers, which
    // is the property a hardcoded verb (of any spelling) cannot have.
    #[test]
    fn the_identity_line_is_attributed_to_the_callers_verb() {
        let home = tempfile::tempdir().unwrap();
        let mut vars = std::collections::HashMap::new();
        vars.insert(
            "HOME".to_string(),
            home.path().to_string_lossy().to_string(),
        );
        let env = crate::effects::MapEnv { vars, uid: 501 };
        let session = Session {
            provider: TOMBSTONE_PROVIDER.to_string(),
            session_id: "sess-verb-attribution".to_string(),
            name: Some("wk".to_string()),
            ..blank_session()
        };

        // The wait seam (`crate::idle`'s adapter).
        let wait = preserve_identity(&env, &session, "loss at wait entry", "wait")
            .expect("in scope + a resolvable HOME ⇒ a line");
        assert!(
            wait.starts_with("qd wait: acp: session \"wk\" identity preserved at "),
            "the wait seam's line must open `qd wait:`, got: {wait}"
        );

        // The send seam (`super::acp::send_acp`), same body, different command.
        let send = preserve_identity(&env, &session, "loss at send entry", "send:relay")
            .expect("in scope + a resolvable HOME ⇒ a line");
        assert!(
            send.starts_with("qd send:relay: acp: session \"wk\" identity preserved at "),
            "the send seam's line must open `qd send:relay:`, got: {send}"
        );

        // And nothing anywhere still says the bare, process-named `qd:`.
        for line in [&wait, &send] {
            assert!(
                !line.starts_with("qd: "),
                "the pre-fix bare `qd:` prefix must be gone, got: {line}"
            );
        }
    }

    fn blank_session() -> Session {
        Session {
            name: None,
            user_named: None,
            session_id: String::new(),
            code: None,
            qd_id: None,
            pid: None,
            status: SessionStatus::Cold,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: None,
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: String::new(),
            entrypoint: None,
            lineage: None,
            hosting: None,
            which_branch: SessionBranch::LiveRegistry,
        }
    }
}
