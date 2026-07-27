//! `qd whoami` (alias `name`) — REAL (findCallerSession port, commands/status.ts:159-210).
//!
//! P0 wave-2 (spec-w2-env D2): whoami PREFERS `QD_SESSION_ID` (the env-carried
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
use dispatch::paths::QdPaths;
use dispatch::registry::RegistryEntry;

/// Resolution outcome from the identity resolution core.
#[derive(Debug)]
pub(super) enum Resolution {
    /// Env id resolved to UUID AND a live registry row exists.
    Full(Answer),
    /// Env id resolved to UUID but NO live registry row (partial/cold session).
    PartialCold(Answer),
    /// Env id is set but not resolvable (malformed or unmapped).
    Indeterminate,
    /// No qd-managed identity resolves at all.
    NotManaged,
}

/// One resolved identity + which path answered it.
#[derive(Debug)]
pub(super) struct Answer {
    pub name: Option<String>,
    pub session_id: String,
    pub pid: Option<i64>,
    pub started_at_ms: Option<i64>,
    pub qd_id: Option<String>,
    pub source: &'static str, // "env" | "ppid"
}

/// The PURE resolution core (the testable seam — D2's ">10 levels deep" test
/// stubs `walk` to fail and proves the env path answers alone).
///
/// Env path: a well-formed `QD_SESSION_ID` that the idstore fold maps to a
/// provider UUID answers as `source: "env"`; name/pid come from the live
/// registry row carrying that UUID when one exists (a cold session still
/// answers — sessionId + qdId are known without a row). Otherwise (unset,
/// malformed, or unmapped id) fall back to `walk` — the ppid-chain registry
/// hit — as `source: "ppid"`, with qdId joined read-only from the fold.
fn resolve_identity(
    env_id: Option<String>,
    ids: &IdMap,
    row_for_uuid: &dyn Fn(&str) -> Option<RegistryEntry>,
    walk: &dyn Fn() -> Option<RegistryEntry>,
) -> Resolution {
    if let Some(raw) = env_id.filter(|s| !s.is_empty()) {
        // Env id is SET — attempt resolution through the shared chain.
        if let Some(uuid) = dispatch::idstore::resolve_to_uuid(ids, &raw) {
            let qd_id = Some(dispatch::idstore::normalize(&raw));
            let row = row_for_uuid(&uuid);
            let ans = Answer {
                name: row.as_ref().and_then(|r| r.name.clone()),
                session_id: uuid,
                pid: row.as_ref().and_then(|r| r.pid),
                started_at_ms: row.as_ref().and_then(|r| r.started_at),
                qd_id,
                source: "env",
            };
            return if ans.name.as_ref().map(|n| !n.is_empty()).unwrap_or(false) || ans.pid.is_some()
            {
                // Live registry row with name or pid: fully resolved.
                Resolution::Full(ans)
            } else {
                // UUID known but no live row (or row has no name/pid): partial-cold.
                Resolution::PartialCold(ans)
            };
        }
        // Env id is set but unresolvable (malformed or unmapped): indeterminate.
        // Do NOT fall through to the ppid walk — that would silently attribute
        // a wrong identity to this caller.
        return Resolution::Indeterminate;
    }
    // Env id absent — try the ppid walk as the only remaining source.
    let Some(entry) = walk() else {
        return Resolution::NotManaged;
    };
    // A PPID-walk hit without a session UUID cannot be fully verified — treat as unmanaged.
    let Some(session_id) = entry.session_id.clone().filter(|s| !s.is_empty()) else {
        return Resolution::NotManaged;
    };
    let qd_id = ids.by_session.get(&session_id).cloned();
    Resolution::Full(Answer {
        name: entry.name,
        session_id,
        pid: entry.pid,
        started_at_ms: entry.started_at,
        qd_id,
        source: "ppid",
    })
}

/// The shipped four-state identity resolver with real storage supplied through
/// injected env/exec seams. `qd adopt` calls this function directly so it uses
/// the same QD_SESSION_ID-preferred path, conflict handling, and ppid fallback as
/// `qd whoami` rather than re-reading identity variables ad hoc.
pub(super) fn resolve_current_identity(
    env: &dyn Env,
    exec: &dyn dispatch::exec::Exec,
) -> Resolution {
    let Some(home) = env.var("HOME").filter(|s| !s.is_empty()) else {
        return Resolution::NotManaged;
    };
    let paths = QdPaths::from_home(std::path::Path::new(&home));
    let state_dir = QdPaths::from_home_env(std::path::Path::new(&home), env).state_dir;
    let ids = dispatch::idstore::fold(&dispatch::idstore::ids_path(&state_dir));

    let row_for_uuid = |uuid: &str| -> Option<RegistryEntry> {
        let mut rows: Vec<RegistryEntry> =
            dispatch::registry::read_entries(&paths.sessions_dir, false)
            .into_iter()
            .filter(|s| !s.tombstoned)
            .map(|s| s.entry)
            .filter(|e| e.session_id.as_deref() == Some(uuid))
            .collect();
        // Conflicting live rows for one UUID are PartialCold/indeterminate, never
        // a guessed first match. This is the whoami hardening contract.
        if rows.len() == 1 {
            rows.pop()
        } else {
            None
        }
    };
    let walk = || dispatch::telemetry::find_caller_session(&paths, exec);
    resolve_identity(env.var("QD_SESSION_ID"), &ids, &row_for_uuid, &walk)
}

pub fn run(m: &ArgMatches) -> i32 {
    let json = m.get_flag("json");

    let env = RealEnv;
    if env.var("HOME").filter(|s| !s.is_empty()).is_none() {
        eprintln!("Not running inside a Claude Code session");
        return 1;
    }
    match resolve_current_identity(&env, &RealExec) {
        Resolution::Full(ans) => {
            if json {
                let out = json!({
                    "name": ans.name.clone().filter(|n| !n.is_empty()),
                    "sessionId": ans.session_id,
                    "pid": ans.pid,
                    "identitySource": ans.source,
                    "qdId": ans.qd_id,
                });
                println!("{out}");
            } else {
                let label = ans.name.filter(|n| !n.is_empty()).unwrap_or(ans.session_id);
                println!("{label}");
            }
            0
        }
        Resolution::PartialCold(ans) => {
            if json {
                let out = json!({
                    "name": serde_json::Value::Null,
                    "sessionId": ans.session_id,
                    "pid": serde_json::Value::Null,
                    "identitySource": ans.source,
                    "qdId": ans.qd_id,
                });
                println!("{out}");
            } else {
                // Never promote UUID or historical label as the current name.
                let qd_id_part = ans.qd_id.as_deref().unwrap_or("?");
                println!(
                    "partial-cold: qdId={qd_id_part}, sessionId={}",
                    ans.session_id
                );
            }
            0
        }
        Resolution::Indeterminate => {
            eprintln!(
                "qd whoami: identity indeterminate (QD_SESSION_ID is set but cannot be resolved)"
            );
            1
        }
        Resolution::NotManaged => {
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
            started_at: Some(1_700_000_000_000),
            ..Default::default()
        }
    }

    const STORE: &str = concat!(
        r#"{"v":1,"ts":"t","event":"mint","id":"ab3kx9mq","session_id":"uuid-env","name":"wk"}"#,
        "\n",
        r#"{"v":1,"ts":"t","event":"mint","id":"cd47qrst","session_id":"uuid-walk","name":"pp"}"#,
        "\n",
    );

    /// D2: env path answers at any process depth; the walk seam is stubbed to FAIL.
    #[test]
    fn env_id_answers_with_walk_stubbed_to_fail() {
        let ids = ids(STORE);
        let rows = |uuid: &str| (uuid == "uuid-env").then(|| row("wk", "uuid-env", 4242));
        let walk = || -> Option<RegistryEntry> { panic!("walk must not run on the env path") };
        let ans = match resolve_identity(Some("ab3kx9mq".into()), &ids, &rows, &walk) {
            Resolution::Full(a) => a,
            r => panic!("expected Full, got {:?}", std::mem::discriminant(&r)),
        };
        assert_eq!(ans.source, "env");
        assert_eq!(ans.session_id, "uuid-env");
        assert_eq!(ans.name.as_deref(), Some("wk"));
        assert_eq!(ans.pid, Some(4242));
        assert_eq!(ans.qd_id.as_deref(), Some("ab3kx9mq"));
    }

    /// Cold session: env id resolves to UUID but NO live registry row → PartialCold.
    #[test]
    fn env_id_without_registry_row_is_partial_cold() {
        let ids = ids(STORE);
        let rows = |_: &str| -> Option<RegistryEntry> { None };
        let walk = || -> Option<RegistryEntry> { None };
        let ans = match resolve_identity(Some("AB3KX9MQ".into()), &ids, &rows, &walk) {
            Resolution::PartialCold(a) => a,
            r => panic!("expected PartialCold, got {:?}", std::mem::discriminant(&r)),
        };
        assert_eq!(ans.source, "env", "case-insensitive id resolves");
        assert_eq!(ans.session_id, "uuid-env");
        assert_eq!(ans.name, None);
        assert_eq!(ans.pid, None);
    }

    /// Absent env falls back to the ppid walk → Full (walk succeeded with live row).
    #[test]
    fn absent_env_falls_back_to_ppid_walk() {
        let ids = ids(STORE);
        let rows = |_: &str| -> Option<RegistryEntry> { panic!("env path must not run") };
        let walk = || Some(row("pp", "uuid-walk", 99));
        let ans = match resolve_identity(None, &ids, &rows, &walk) {
            Resolution::Full(a) => a,
            r => panic!("expected Full, got {:?}", std::mem::discriminant(&r)),
        };
        assert_eq!(ans.source, "ppid");
        assert_eq!(ans.session_id, "uuid-walk");
        assert_eq!(ans.qd_id.as_deref(), Some("cd47qrst"));
    }

    /// Malformed or unmapped env ids → Indeterminate (no fall-through to walk).
    #[test]
    fn malformed_or_unmapped_env_id_is_indeterminate() {
        let ids = ids(STORE);
        let rows = |_: &str| -> Option<RegistryEntry> { None };
        // Walk MUST NOT run when env id is set but unresolvable.
        let walk = || -> Option<RegistryEntry> {
            panic!("walk must not run when env id is set but unresolvable")
        };
        for bad in ["not-an-id!", "zzzzzzzz", "ab3kx9m"] {
            // non-empty bad values: malformed shape or unmapped id
            let r = resolve_identity(Some(bad.to_string()), &ids, &rows, &walk);
            assert!(
                matches!(r, Resolution::Indeterminate),
                "{bad:?} must be Indeterminate, not fall-through"
            );
        }
    }

    /// Empty env string is ignored (treated as absent) and falls to walk.
    #[test]
    fn empty_env_string_treated_as_absent() {
        let ids = ids(STORE);
        let rows = |_: &str| -> Option<RegistryEntry> { None };
        let walk = || Some(row("pp", "uuid-walk", 99));
        // empty string: filter(|s| !s.is_empty()) removes it → falls to walk
        let ans = match resolve_identity(Some("".into()), &ids, &rows, &walk) {
            Resolution::Full(a) => a,
            r => panic!(
                "expected Full from walk, got {:?}",
                std::mem::discriminant(&r)
            ),
        };
        assert_eq!(ans.source, "ppid");
    }

    /// Nothing resolves → NotManaged.
    #[test]
    fn nothing_resolves_returns_not_managed() {
        let ids = ids("");
        let rows = |_: &str| -> Option<RegistryEntry> { None };
        let walk = || -> Option<RegistryEntry> { None };
        assert!(matches!(
            resolve_identity(None, &ids, &rows, &walk),
            Resolution::NotManaged
        ));
        // Unmapped env id → Indeterminate (not NotManaged)
        assert!(matches!(
            resolve_identity(Some("ab3kx9mq".into()), &ids, &rows, &walk),
            Resolution::Indeterminate
        ));
    }

    /// A PPID registry hit without a stable session UUID is not a managed identity.
    #[test]
    fn ppid_walk_without_session_id_returns_not_managed() {
        let ids = ids(STORE);
        let rows = |_: &str| -> Option<RegistryEntry> { panic!("env path must not run") };
        let walk = || {
            Some(RegistryEntry {
                pid: Some(99),
                session_id: None,
                name: Some("pp".to_string()),
                ..Default::default()
            })
        };

        assert!(matches!(
            resolve_identity(None, &ids, &rows, &walk),
            Resolution::NotManaged
        ));
    }
}
