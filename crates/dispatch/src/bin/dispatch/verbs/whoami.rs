//! `sb whoami` (alias `name`) — REAL (findCallerSession port, commands/status.ts:159-210).
//!
//! P0 wave-2 (spec-w2-env D2): whoami PREFERS `SB_SESSION_ID` (the env-carried
//! identity the engine injects at every launch — works at ANY process depth and
//! for detached processes), resolving it through the idstore fold → provider
//! UUID → registry row. It falls back to the original ppid-chain walk
//! (`findCallerSession`, commands/status.ts:159-184: walk up to 10 levels
//! reading `<SESSIONS_DIR>/<pid>.json` per ancestor) when the var is unset OR
//! unresolvable (malformed id / not in the idstore). `--json` says which path
//! answered: `"identitySource": "env" | "ppid"`.

use clap::ArgMatches;
use serde_json::json;

use dispatch::effects::{Env, RealEnv};
use dispatch::exec::RealExec;
use dispatch::idstore::IdMap;
use dispatch::paths::SbPaths;
use dispatch::registry::RegistryEntry;

/// One resolved identity + which path answered it.
struct Answer {
    name: Option<String>,
    session_id: String,
    pid: Option<i64>,
    sb_id: Option<String>,
    source: &'static str, // "env" | "ppid"
}

/// The PURE resolution core (the testable seam — D2's ">10 levels deep" test
/// stubs `walk` to fail and proves the env path answers alone).
///
/// Env path: a well-formed `SB_SESSION_ID` that the idstore fold maps to a
/// provider UUID answers as `source: "env"`; name/pid come from the live
/// registry row carrying that UUID when one exists (a cold session still
/// answers — sessionId + sbId are known without a row). Otherwise (unset,
/// malformed, or unmapped id) fall back to `walk` — the ppid-chain registry
/// hit — as `source: "ppid"`, with sbId joined read-only from the fold.
fn resolve_identity(
    env_id: Option<String>,
    ids: &IdMap,
    row_for_uuid: &dyn Fn(&str) -> Option<RegistryEntry>,
    walk: &dyn Fn() -> Option<RegistryEntry>,
) -> Option<Answer> {
    if let Some(raw) = env_id.filter(|s| !s.is_empty()) {
        // The SHARED resolution chain (idstore::resolve_to_uuid — normalize →
        // shape gate → by_id, unbound mint = None): whoami and the send:relay
        // from_session derivation answer "what engine identity resolves?"
        // identically by construction (S4, the name-consistency lesson).
        if let Some(uuid) = dispatch::idstore::resolve_to_uuid(ids, &raw) {
            let row = row_for_uuid(&uuid);
            return Some(Answer {
                name: row.as_ref().and_then(|r| r.name.clone()),
                session_id: uuid,
                pid: row.as_ref().and_then(|r| r.pid),
                sb_id: Some(dispatch::idstore::normalize(&raw)),
                source: "env",
            });
        }
        // Malformed / unmapped → fall through to the walk (never guess).
    }
    let entry = walk()?;
    let sb_id = entry
        .session_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|sid| ids.by_session.get(sid))
        .cloned();
    Some(Answer {
        name: entry.name,
        session_id: entry.session_id.unwrap_or_default(),
        pid: entry.pid,
        sb_id,
        source: "ppid",
    })
}

pub fn run(m: &ArgMatches) -> i32 {
    let json = m.get_flag("json");

    let env = RealEnv;
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => h,
        None => {
            eprintln!("Not running inside a Claude Code session");
            return 1;
        }
    };
    let paths = SbPaths::from_home(std::path::Path::new(&home));

    // READ-ONLY idstore fold (whoami never mints) — SB_HOME-honoring path.
    let state_dir = SbPaths::from_home_env(std::path::Path::new(&home), &env).state_dir;
    let ids = dispatch::idstore::fold(&dispatch::idstore::ids_path(&state_dir));

    let row_for_uuid = |uuid: &str| -> Option<RegistryEntry> {
        dispatch::registry::read_entries(&paths.sessions_dir, false)
            .into_iter()
            .filter(|s| !s.tombstoned)
            .map(|s| s.entry)
            .find(|e| e.session_id.as_deref() == Some(uuid))
    };
    // find_caller_session/ps_ppid are hoisted into dispatch::telemetry (A6 F11) so
    // create.rs and whoami share ONE implementation — the same ppid-walk over
    // the injected Exec seam, now as this fn's FALLBACK (D2).
    let walk = || dispatch::telemetry::find_caller_session(&paths, &RealExec);

    match resolve_identity(env.var("SB_SESSION_ID"), &ids, &row_for_uuid, &walk) {
        Some(ans) => {
            if json {
                // commands/status.ts:201-206 — {name: name||null, sessionId, pid},
                // plus the P0 additive keys sbId (when mapped) + identitySource.
                let mut out = json!({
                    "name": ans.name.clone().filter(|n| !n.is_empty()),
                    "sessionId": ans.session_id,
                    "pid": ans.pid,
                    "identitySource": ans.source,
                });
                if let Some(id) = &ans.sb_id {
                    out["sbId"] = json!(id);
                }
                println!("{out}");
            } else {
                // commands/status.ts:208 — name || sessionId.
                let label = ans.name.filter(|n| !n.is_empty()).unwrap_or(ans.session_id);
                println!("{label}");
            }
            0
        }
        None => {
            eprintln!("Not running inside a Claude Code session");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(text: &str) -> IdMap {
        dispatch::idstore::fold_str(text)
    }

    fn row(name: &str, uuid: &str, pid: i64) -> RegistryEntry {
        RegistryEntry {
            pid: Some(pid),
            session_id: Some(uuid.to_string()),
            name: Some(name.to_string()),
            ..Default::default()
        }
    }

    const STORE: &str = concat!(
        r#"{"v":1,"ts":"t","event":"mint","id":"ab3kx9mq","session_id":"uuid-env","name":"wk"}"#,
        "\n",
        r#"{"v":1,"ts":"t","event":"mint","id":"cd47qrst","session_id":"uuid-walk","name":"pp"}"#,
        "\n",
    );

    /// D2's point: the env path answers at ANY process depth — here the
    /// ppid-walk seam is stubbed to FAIL (simulating >10 levels / detached),
    /// and the env id still resolves end-to-end.
    #[test]
    fn env_id_answers_with_walk_stubbed_to_fail() {
        let ids = ids(STORE);
        let rows = |uuid: &str| (uuid == "uuid-env").then(|| row("wk", "uuid-env", 4242));
        let walk = || -> Option<RegistryEntry> { panic!("walk must not run on the env path") };
        let ans = resolve_identity(Some("ab3kx9mq".into()), &ids, &rows, &walk).unwrap();
        assert_eq!(ans.source, "env");
        assert_eq!(ans.session_id, "uuid-env");
        assert_eq!(ans.name.as_deref(), Some("wk"));
        assert_eq!(ans.pid, Some(4242));
        assert_eq!(ans.sb_id.as_deref(), Some("ab3kx9mq"));
    }

    /// A cold session (no registry row) still answers on the env path —
    /// sessionId + sbId are known without a row; name/pid are absent.
    #[test]
    fn env_id_without_registry_row_still_answers() {
        let ids = ids(STORE);
        let rows = |_: &str| -> Option<RegistryEntry> { None };
        let walk = || -> Option<RegistryEntry> { None };
        let ans = resolve_identity(Some("AB3KX9MQ".into()), &ids, &rows, &walk).unwrap();
        assert_eq!(ans.source, "env", "case-insensitive id resolves");
        assert_eq!(ans.session_id, "uuid-env");
        assert_eq!(ans.name, None);
        assert_eq!(ans.pid, None);
    }

    #[test]
    fn unset_env_falls_back_to_ppid_walk() {
        let ids = ids(STORE);
        let rows = |_: &str| -> Option<RegistryEntry> { panic!("env path must not run") };
        let walk = || Some(row("pp", "uuid-walk", 99));
        let ans = resolve_identity(None, &ids, &rows, &walk).unwrap();
        assert_eq!(ans.source, "ppid");
        assert_eq!(ans.session_id, "uuid-walk");
        // sbId joined read-only from the fold on the walk path too.
        assert_eq!(ans.sb_id.as_deref(), Some("cd47qrst"));
    }

    /// Unresolvable env values (malformed shape, unmapped id, empty) fall back
    /// to the walk rather than guessing.
    #[test]
    fn malformed_or_unmapped_env_id_falls_back() {
        let ids = ids(STORE);
        let rows = |_: &str| -> Option<RegistryEntry> { None };
        let walk = || Some(row("pp", "uuid-walk", 99));
        for bad in ["not-an-id!", "zzzzzzzz", "", "ab3kx9m"] {
            let ans = resolve_identity(Some(bad.to_string()), &ids, &rows, &walk).unwrap();
            assert_eq!(ans.source, "ppid", "{bad:?} must fall back");
            assert_eq!(ans.session_id, "uuid-walk");
        }
    }

    #[test]
    fn nothing_resolves_returns_none() {
        let ids = ids("");
        let rows = |_: &str| -> Option<RegistryEntry> { None };
        let walk = || -> Option<RegistryEntry> { None };
        assert!(resolve_identity(None, &ids, &rows, &walk).is_none());
        assert!(resolve_identity(Some("ab3kx9mq".into()), &ids, &rows, &walk).is_none());
    }
}
