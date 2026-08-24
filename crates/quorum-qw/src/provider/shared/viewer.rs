//! `provider/shared/viewer` — the human VIEWER pane: a second client on a
//! daemon-hosted session's residence, in a mux pane of its own.
//!
//! # What a viewer is
//!
//! A daemon-hosted session has no terminal. Two harnesses in this crate answer
//! that with the same move: their residence is a SERVER, so a human can be given
//! a second client of it instead of a terminal the session never had.
//!
//! - `codex/app-server` — `codex --remote <ws-url> resume <thread-id>` joins the
//!   `codex app-server` qd already spawned for the session.
//! - `acp/opencode` — `opencode attach <http-url> --session <id>` joins the HTTP
//!   server `opencode acp` runs inside the ACP bridge qd already spawned.
//!
//! Nothing stops and nothing converts: the agent keeps driving its session
//! throughout, and the human is simply another client on the same server.
//!
//! # THE VIEWER IS NOT A SESSION
//!
//! It gets a mux pane (so it can be detached and re-attached, over SSH or from a
//! phone, like any other pane) but NO registry row: it owns no thread, has no
//! identity, and its death means nothing. A second attach reuses a live viewer
//! rather than stacking another. That is why both halves here take a SESSION NAME
//! and nothing else — a viewer has no id to be looked up by.
//!
//! # Why this is shared rather than codex's
//!
//! It was codex's, and the doc said so: "Lives here, not in the verb, because it
//! is a CODEX AFFORDANCE: no other lane has a viewer." That sentence was true
//! when it was written and is not true now — pinning `opencode acp`'s own HTTP
//! port gave `acp/opencode` a joinable residence too, and both lanes need the
//! same pane name and the same reap. Neither of these two functions ever
//! contained a line about codex: one formats a name, the other kills a pane. So
//! they move, by this directory's own test (two harnesses RUN the code, not
//! merely resemble each other), and `provider::codex` re-exports them so every
//! existing call site keeps resolving.
//!
//! What did NOT move is what actually knows a harness: which argv a viewer runs,
//! and which refusals a row must clear before one is opened. Those stay in the
//! lane, one arm per harness.

use std::path::PathBuf;

use crate::effects::Env;

/// The mux-pane name of a HUMAN VIEWER opened on a session.
///
/// Distinct from the session's own name because a viewer is NOT the session — it
/// is a second process looking at it, with its own lifetime.
///
/// `.view` rather than something unmintable: qrmux restricts pane names to
/// `a-zA-Z0-9_-.`, so there is no separator available that a session name could
/// not also contain. A user COULD therefore have a real session literally named
/// `foo.view`, and reusing a pane on name alone would attach them to that instead
/// of a viewer on `foo`. The lane's reuse check closes that with a REGISTRY
/// guard (pane present + no live row claiming the name ⇒ ours), because the
/// embedded mux cannot report a pane's command line to check the argv against.
pub fn pane_name(session_name: &str) -> String {
    format!("{session_name}.view")
}

/// Best-effort reap of a session's human-viewer pane.
///
/// The viewer is not the session and holds no row, but it is a live TUI pointed
/// at a server the kill just killed — left behind it would sit there rendering a
/// dead connection, and its name would block the next viewer. So every kill arm
/// whose lane can host a viewer calls this immediately after the group reap.
///
/// Silent by design at every step: a session with no viewer is the overwhelmingly
/// common case, and a viewer that cannot be reaped must never turn a successful
/// `qd stop` into a failure — the session itself is already down. (An earlier
/// version routed its backend parse through the verb's `common::select_backend`,
/// which PRINTS on a bogus `QD_MUX`; parsing directly restores the silence the
/// doc claimed.)
pub fn reap_pane(env: &dyn Env, home: &std::path::Path, session_name: &str) {
    let pane = pane_name(session_name);
    let Ok(backend) = crate::mux_selector::parse_backend(env) else {
        return;
    };
    let Ok(mux) = crate::mux_selector::select_mux(backend, home, env) else {
        return;
    };
    let dirs: Vec<PathBuf> = match backend {
        crate::mux_selector::Backend::Embedded => {
            match crate::qrmux_dir::resolve_qrmux_dir(home, env) {
                Ok(d) => vec![d],
                Err(_) => return,
            }
        }
        crate::mux_selector::Backend::Zmx => vec![quorum_core::zmx_dir::resolve_zmx_dir(env)],
    };
    for d in dirs {
        if mux
            .list(&d)
            .unwrap_or_default()
            .into_iter()
            .any(|z| z.name == pane)
        {
            let _ = mux.kill(&d, &pane);
        }
    }
}
