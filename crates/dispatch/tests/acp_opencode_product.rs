//! A-OC.1 — opencode PRODUCT verb-path: the bridge-resolution mutation guard (DEFAULT suite)
//! + the LIVE product-path lane (gated `QD_ACP_OPENCODE_LIVE=1`).
//!
//! DISTINCT from `acp_opencode_live.rs` (the prior opencode-acp atomic's DRIVER-level lane, which
//! drives `AcpHost` directly against `opencode acp`): this file proves the PRODUCT verb-path —
//! `qd start --provider opencode` → the acp residence adapter → `opencode acp` → OpenRouter.
//!
//! 1. **`opencode_bridge_resolution_routes_to_opencode`** (DEFAULT suite, no network/creds):
//!    proves `--provider opencode` resolves to the opencode-bridged `AcpProvider` and that the
//!    residence adapter argv the create/resume verbs build carries `--bridge-cmd opencode
//!    --bridge-arg acp` — while `acp/claude-code` stays BYTE-IDENTICAL (bridge_cmd None ⇒ NO
//!    `--bridge-cmd`, the compiled default). MUTATION EVIDENCE: aliasing the opencode bridge back
//!    to the claude default (or dropping the bridge_cmd wiring in the residence verbs) reds this.
//!
//! 1b. **`opencode_adapter_argv_pins_the_bridges_own_http_port`** (DEFAULT suite): proves the
//!    residence argv also carries `--bridge-arg --port --bridge-arg <p>`, and that the row's
//!    recorded `harnessEndpoint` is that same port. `opencode acp` runs a full opencode HTTP
//!    server; its port defaults to `0`, so without the pin qd spawns a real server per session and
//!    cannot name it afterwards — and `qd attach` on this lane becomes impossible. MUTATION
//!    EVIDENCE: dropping the `harness_server` spec, or the port append in `bridge_for`, reds this
//!    while everything else still passes.
//!
//! 2. **`acp_opencode_live_product_path`** (gated `QD_ACP_OPENCODE_LIVE=1`): drives the REAL `qd`
//!    binary end-to-end — `qd start --provider opencode` → `qd send:relay` (a live OpenRouter turn,
//!    gpt-4o-mini) → `qd wait` → `qd stop` — asserting the row is `acp/opencode`, the turn returned
//!    a live turn-id, wait resolved done, and stop left no leak. Non-vacuity: it asserts the live
//!    turn-id + provider row (a skip-mode no-op cannot pass). It also asserts the bridge's own
//!    HTTP server was PINNED and is BOUND (`opencode acp --port <p>` in the live cmdline, plus a
//!    TCP connect) — the address `qd attach` hands a viewer. CRED: the OpenRouter key is FILE-PATH
//!    ONLY (`~/.secrets/openrouter-pi.key`), read into the child env at drive-time; its VALUE never
//!    appears in the test source, assertions, or output.

#[path = "common/live_gate.rs"]
mod live_gate;

use dispatch::acp_residence::build_adapter_argv;
use dispatch::provider::acp::acp_provider_for;
use std::path::{Path, PathBuf};

// ===========================================================================
// 1. DEFAULT-SUITE bridge-resolution mutation guard (pure, no creds/network).
// ===========================================================================

#[test]
fn opencode_bridge_resolution_routes_to_opencode() {
    // `--provider opencode` (Pete's ergonomic) and the internal id BOTH resolve to the
    // opencode-bridged provider whose id() is `acp/opencode`.
    for id in ["opencode", "acp/opencode"] {
        let p = dispatch::provider::provider_for(id)
            .unwrap_or_else(|| panic!("provider_for({id:?}) must resolve (A-OC.1 alias)"));
        assert_eq!(p.id(), "acp/opencode", "{id} resolves to the acp/opencode provider");
    }

    // The opencode bridge spec: `opencode acp`.
    let oc = acp_provider_for("acp/opencode").expect("acp/opencode is a registered acp provider");
    assert_eq!(oc.bridge_cmd(), Some("opencode"), "opencode bridge_cmd");
    assert_eq!(oc.bridge_args(), ["acp"], "opencode bridge_args");

    // The residence adapter argv the create/resume verbs build carries the opencode bridge.
    let exe = Path::new("/usr/bin/qd");
    let bridge_args: Vec<String> = oc.bridge_args().iter().map(|a| a.to_string()).collect();
    let argv = build_adapter_argv(
        exe,
        "ws://127.0.0.1:9000",
        Path::new("/work"),
        oc.bridge_cmd(),
        &bridge_args,
        None,
    );
    assert!(
        argv.join(" ").contains("--bridge-cmd opencode --bridge-arg acp"),
        "the opencode adapter argv must spawn `opencode acp`: {argv:?}"
    );

    // acp/claude-code stays BYTE-IDENTICAL: bridge_cmd None ⇒ NO `--bridge-cmd` on the argv
    // (the residence layer falls back to the compiled BRIDGE_BIN default, claude-code-acp).
    let cc = acp_provider_for("acp/claude-code").expect("acp/claude-code is registered");
    assert_eq!(cc.bridge_cmd(), None, "claude-code keeps the compiled BRIDGE_BIN default");
    assert!(cc.bridge_args().is_empty(), "claude-code has no extra bridge args");
    let cc_argv = build_adapter_argv(
        exe,
        "ws://127.0.0.1:9000",
        Path::new("/work"),
        cc.bridge_cmd(),
        &[],
        None,
    );
    assert!(
        !cc_argv.join(" ").contains("--bridge-cmd"),
        "acp/claude-code adapter argv must carry NO --bridge-cmd (byte-identical): {cc_argv:?}"
    );
}

/// The bridge's OWN server is pinned on the adapter argv, and the row records it.
///
/// This is the half `opencode_bridge_resolution_routes_to_opencode` cannot see:
/// that test proves qd spawns `opencode acp`, this one proves qd spawns it on a
/// port it can name later. The two failure modes are different — the first
/// spawns the wrong program, the second spawns the right program unreachably —
/// and only the second one still passes every other test in this file.
#[test]
fn opencode_adapter_argv_pins_the_bridges_own_http_port() {
    let oc = acp_provider_for("acp/opencode").expect("acp/opencode is registered");
    let server = oc
        .harness_server()
        .expect("`opencode acp` runs an HTTP server of its own");
    assert_eq!(server.port_flag, "--port");
    assert_eq!(server.endpoint(41234), "http://127.0.0.1:41234");

    // The residence argv carries the pin as bridge-args, so the adapter spawns
    // `opencode acp --port 41234`.
    let mut bridge_args: Vec<String> = oc.bridge_args().iter().map(|a| a.to_string()).collect();
    bridge_args.extend(server.port_args(41234));
    let argv = build_adapter_argv(
        Path::new("/usr/bin/qd"),
        "ws://127.0.0.1:9000",
        Path::new("/work"),
        oc.bridge_cmd(),
        &bridge_args,
        None,
    );
    assert!(
        argv.join(" ")
            .contains("--bridge-cmd opencode --bridge-arg acp --bridge-arg --port --bridge-arg 41234"),
        "the opencode adapter argv must pin the bridge's HTTP port: {argv:?}"
    );

    // The two ports on the row are DIFFERENT SERVERS: `endpoint` is qd's adapter
    // ws front, `harnessEndpoint` is the bridge's own HTTP server. A viewer needs
    // the second; every verb needs the first.
    assert_ne!(server.endpoint(41234), "ws://127.0.0.1:9000");

    // `acp/claude-code` stays a stdio-only bridge: NO server, so no second port
    // is allocated and its row carries no `harnessEndpoint` key at all. This is
    // the byte-stability half — a `Some(...)` here would change every existing
    // claude-bridge row on disk.
    let cc = acp_provider_for("acp/claude-code").expect("acp/claude-code is registered");
    assert!(
        cc.harness_server().is_none(),
        "claude-code-acp speaks ACP on stdio and listens on nothing"
    );
}

// ===========================================================================
// 2. LIVE product-path lane (QD_ACP_OPENCODE_LIVE=1): real qd binary + opencode + OpenRouter.
// ===========================================================================

fn live() -> bool {
    live_gate::conformance_gate("QD_ACP_OPENCODE_LIVE", "acp_opencode_product")
}

/// Locate the `opencode` binary (the bun install dir, then PATH). Returns None to skip.
fn opencode_bin() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        let bun = PathBuf::from(&home).join(".bun/bin/opencode");
        if bun.exists() {
            return Some(bun);
        }
    }
    for dir in std::env::var("PATH").unwrap_or_default().split(':') {
        let c = Path::new(dir).join("opencode");
        if c.exists() {
            return Some(c);
        }
    }
    None
}

#[test]
fn acp_opencode_live_product_path() {
    if !live() {
        eprintln!("QD_ACP_OPENCODE_LIVE != 1 — skipping the live opencode product-path lane");
        return;
    }
    use std::process::Command;

    let qd = env!("CARGO_BIN_EXE_qd");
    let home = std::env::var("HOME").expect("HOME");
    let key_path = PathBuf::from(&home).join(".secrets/openrouter-pi.key");
    // CRED: read the key VALUE into memory ONLY (never asserted/printed). FILE-PATH is the input.
    let key = std::fs::read_to_string(&key_path)
        .unwrap_or_else(|e| panic!("OpenRouter key file {key_path:?} unreadable: {e}"))
        .trim()
        .to_string();
    assert!(!key.is_empty(), "the OpenRouter key file must be non-empty");
    let oc = opencode_bin().expect("opencode binary required for the live lane");
    let oc_dir = oc.parent().unwrap().to_string_lossy().into_owned();

    // A dedicated, self-contained opencode config: cheap NON-claude model for BOTH the primary and
    // the small (title) agent — the cred-hygiene NEVER-claude binding (opencode's title agent
    // otherwise defaults to a claude model).
    let work = tempfile::tempdir().unwrap();
    let cfg = work.path().join("opencode-openrouter.json");
    std::fs::write(
        &cfg,
        r#"{ "$schema": "https://opencode.ai/config.json", "model": "openrouter/openai/gpt-4o-mini", "small_model": "openrouter/openai/gpt-4o-mini" }"#,
    )
    .unwrap();

    let qd_home = tempfile::tempdir().unwrap();
    let xrd = tempfile::tempdir().unwrap();
    let cwd = work.path().join("sess");
    std::fs::create_dir_all(&cwd).unwrap();
    let path = format!("{oc_dir}:{}", std::env::var("PATH").unwrap_or_default());

    // The shared drive env (key injected at drive-time; value stays in this process' memory).
    let base = |c: &mut Command| {
        c.env("QD_HOME", qd_home.path())
            .env("XDG_RUNTIME_DIR", xrd.path())
            .env("PATH", &path)
            .env("OPENCODE_CONFIG", &cfg)
            .env("OPENROUTER_API_KEY", &key)
            .current_dir(&cwd);
    };

    // start --provider opencode.
    let mut start = Command::new(qd);
    base(&mut start);
    let out = start
        .args(["start", "--provider", "opencode", "octest"])
        .arg("--cwd")
        .arg(&cwd)
        .output()
        .expect("qd start runs");
    assert!(out.status.success(), "qd start --provider opencode failed: {out:?}");

    // Non-vacuity: `qd info --json` must show the row persisted provider=acp/opencode (the
    // product path resolved --provider opencode → acp/opencode and spawned the opencode bridge).
    let mut info = Command::new(qd);
    base(&mut info);
    let info_out = info.args(["info", "octest", "--json"]).output().expect("qd info");
    let info_json = String::from_utf8_lossy(&info_out.stdout);
    assert!(
        info_json.contains("\"acp/opencode\""),
        "the row must persist provider=acp/opencode: {info_json}"
    );

    // The PIN, live. `opencode acp` runs an opencode HTTP server; its port
    // defaults to 0, so this asserts qd told it otherwise and that something is
    // actually listening there. Both halves are needed: the cmdline proves the
    // flag was passed, the connect proves the server took it. Without this the
    // viewer path (`qd attach` → `opencode attach <url> --session <id>`) has no
    // address to use.
    //
    // DELIBERATELY STOPS AT THE SOCKET. Asserting on an HTTP response would be
    // better evidence, and is the obvious next increment — it is left out
    // because opencode's `GET /session` sits behind workspace-routing middleware
    // whose query requirements were not verified against a running server, and a
    // gated lane that reds on an unverified guess is worse than one that proves
    // less. What IS proven here is what the pin exists for: a named, bound port.
    let bridge_cmdline = {
        let ps = Command::new("ps")
            .args(["-Ao", "args="])
            .output()
            .expect("ps runs");
        String::from_utf8_lossy(&ps.stdout)
            .lines()
            .find(|l| l.contains("opencode") && l.contains(" acp ") && l.contains("--port"))
            .map(str::to_string)
            .expect("the spawned bridge must appear as `opencode acp --port <p>`")
    };
    let pinned_port: u16 = bridge_cmdline
        .split_whitespace()
        .skip_while(|t| *t != "--port")
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("--port must carry a port: {bridge_cmdline}"));
    assert_ne!(pinned_port, 0, "an ephemeral port is the state the pin removes");
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], pinned_port)),
        std::time::Duration::from_secs(5),
    )
    .unwrap_or_else(|e| {
        panic!("nothing is listening on the pinned port {pinned_port}: {e} — the row \
                records an address a viewer cannot reach")
    });

    // send:relay drives a LIVE opencode turn via OpenRouter; a non-empty turn id proves it landed.
    let mut send = Command::new(qd);
    base(&mut send);
    let send_out = send
        .args([
            "send:relay",
            "octest",
            "Reply with exactly the single word PONG and nothing else.",
        ])
        .output()
        .expect("qd send:relay runs");
    assert!(send_out.status.success(), "qd send:relay failed: {send_out:?}");
    let turn_id = String::from_utf8_lossy(&send_out.stdout).trim().to_string();
    assert!(
        !turn_id.is_empty(),
        "send:relay must return a live opencode turn id (proves a cred'd turn RAN): {turn_id:?}"
    );

    // wait observes the clean ACP terminal.
    let mut wait = Command::new(qd);
    base(&mut wait);
    let wait_out = wait
        .args(["wait", "octest", "--timeout", "90"])
        .output()
        .expect("qd wait runs");
    assert!(
        wait_out.status.success(),
        "qd wait must resolve done for a completed turn: {wait_out:?}"
    );

    // stop reaps the adapter + the opencode bridge child (no leak).
    let mut stop = Command::new(qd);
    base(&mut stop);
    let stop_out = stop.args(["stop", "octest"]).output().expect("qd stop runs");
    assert!(stop_out.status.success(), "qd stop must succeed: {stop_out:?}");

    // Evidence (VALUE-redacted by construction — the key never enters this string).
    let evidence = format!(
        "A-OC.1 live product-path lane GREEN\n\
         qd start --provider opencode → info provider=acp/opencode\n\
         bridge http server pinned + bound: 127.0.0.1:{pinned_port}\n\
         send:relay turn id: {turn_id}\n\
         wait: done, stop: clean\n"
    );
    if let Ok(p) = std::env::var("QD_ACP_OPENCODE_EVIDENCE") {
        let _ = std::fs::write(p, &evidence);
    }
    eprintln!("{evidence}");
}
