//! P3/T3.4 guard: the hidden `relay:serve` verb (NOT listed in
//! COMMAND-SURFACE.md) MUST survive the sb→dispatch rename. It is dispatched
//! pre-clap by a string-literal match in the bin entrypoint, so a refactor —
//! or a careless rename — could silently drop it and take relay down with it
//! (org memory 01KVB4NATAF1RCCK4JA5S8PBSX). These assertions pin the dispatch
//! arms to the bin source. Runtime behavior (a real `dispatch relay:serve` binding
//! a port + speaking MCP) is exercised by the `relay_server_*` suites, which
//! spawn `CARGO_BIN_EXE_qd relay:serve` against a hermetic HOME.

const MAIN_SRC: &str = include_str!("../src/bin/qd/main.rs");

#[test]
fn relay_serve_verb_is_dispatched_post_rename() {
    // The verb name is binary-independent (a string literal), so the rename
    // does not touch it — but it must still route to the relay server entry.
    assert!(
        MAIN_SRC.contains(r#"Some("relay:serve") => return dispatch::relay_server::run()"#),
        "the `relay:serve` dispatch arm was dropped or renamed — relay would break"
    );
}

#[test]
fn relay_register_and_repoint_verbs_survive_post_rename() {
    // T4.4 cutover repoint relies on `dispatch relay:register` (alias repoint).
    assert!(
        MAIN_SRC.contains(r#"Some("relay:register") | Some("relay:repoint")"#),
        "the `relay:register`/`relay:repoint` dispatch arm was dropped"
    );
}
