//! Child D (opencode D1) — the `acp/claude-code` transport-loss disposition:
//! REFUSE and surface (the `codex`/`acp/opencode` HumanRecoveryOnly class),
//! with the session's identity first preserved in the qd-owned tombstone
//! store (`dispatch::tombstone`; clerk-4's Arm-B ratification, bond note
//! 01KX01BY7G — the Arm-A auto-deliver redesign was DECLINED).
//!
//! This is the ONLY new behavior on a loss: one best-effort identity write +
//! one observability line. The refusal surface and exit code are the caller's
//! existing closures, unchanged — and for any provider other than
//! `acp/claude-code` this module is a no-op, so codex/opencode refusals stay
//! byte-identical (their identity rows are not at issue here).

use dispatch::effects::{Env, RealEnv};
use dispatch::model::Session;
use dispatch::paths::QdPaths;
use dispatch::tombstone::{self, IdentityTombstone};

/// The one provider whose loss identity is preserved (the named divergence's
/// scope — matches the retired floor's `floor_eligible` scope exactly).
const TOMBSTONE_PROVIDER: &str = "acp/claude-code";

/// Best-effort: record `session`'s identity in the qd-owned tombstone store
/// before the caller refuses. Never changes the caller's exit path — a store
/// failure is logged and swallowed (the refusal must land regardless).
///
/// Scope gate FIRST: any provider but `acp/claude-code` returns immediately
/// (no write, no output — S4's byte-identical refusal for codex/opencode).
///
/// The store root is `<QD_HOME || ~/.quorum/dispatch>/state/tombstones/`
/// (resolved via [`QdPaths::from_home_env`], the `ids.jsonl` precedent — NOT
/// `paths_from_home`, which ignores QD_HOME) — a directory the claude CLI's
/// dead-pid janitor never reads, so the record survives the ~1s reap of the
/// session's own `~/.claude/sessions/<pid>.json` row and beyond.
///
/// The registry row is re-read here (by pid) for the recorded ACP `endpoint`:
/// at loss time the janitor may already have reaped it — then the endpoint is
/// simply absent and everything else still lands from the resolved `Session`.
pub fn preserve_identity(session: &Session, reason: &str) {
    if session.provider != TOMBSTONE_PROVIDER {
        return;
    }
    let env = RealEnv;
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        // No HOME ⇒ no resolvable store; the caller's refusal still lands.
        return;
    };
    let paths = QdPaths::from_home_env(std::path::Path::new(&home), &env);

    let endpoint = session
        .pid
        .filter(|&p| p != 0)
        .and_then(|pid| dispatch::registry::read_entry(&paths.sessions_dir, pid))
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
    match tombstone::record_loss(&paths.state_dir, record, tombstone::wall_now_ms()) {
        Ok(path) => {
            let label = session
                .name
                .clone()
                .unwrap_or_else(|| session.session_id.clone());
            eprintln!(
                "qd: acp: session \"{label}\" identity preserved at {}",
                path.display()
            );
        }
        Err(e) => {
            eprintln!("qd: acp: could not preserve the session identity ({e}) — refusing anyway");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            // Must not panic, must not write anywhere (nothing to assert on
            // disk without a store path — the gate returns before resolving one).
            preserve_identity(&session, "test reason");
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
            status: dispatch::model::SessionStatus::Cold,
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
            which_branch: dispatch::model::SessionBranch::LiveRegistry,
        }
    }
}
