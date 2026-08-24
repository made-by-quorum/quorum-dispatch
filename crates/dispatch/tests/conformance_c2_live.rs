//! C-2 live RUN-proof: the discriminating new cells (D7 env-matrix + D6
//! injections + advertised-surface). Each cell is driven through the SAME
//! commissioned journal→artifact pipeline C-1 proves (`conformance_c1_live.rs`)
//! — a `Pass`/`Fail`/`Blocked`/`NotApplicable` only ever mints through a
//! commissioned [`Runner`]; no fabricated results.
//!
//! Gated on `QD_C2_LIVE=1` (a no-op otherwise, exactly like the C-1 seed and
//! every other `*_live.rs`). ALWAYS pass `-- --test-threads=1`: several cells
//! spin a real `qrmux`/`tmux` nesting or a real `qd` subprocess, and the Rust
//! harness runs `#[test]` fns concurrently by default (which would overlap the
//! live fixtures).
//!
//! **Binary provenance (shared-CARGO_TARGET_DIR false-cache trap).** Two binaries
//! are driven: `qd` (via `QD_BIN` = `env!("CARGO_BIN_EXE_qd")`, this test binary's
//! own freshly-built qd) and `qrmux` (via `QRMUX_BIN`). `qrmux` is a SEPARATE
//! crate, so `env!("CARGO_BIN_EXE_qrmux")` is unavailable here — instead the
//! sibling binary in the SAME target dir is derived from `CARGO_BIN_EXE_qd`
//! (`.../debug/qd` → `.../debug/qrmux`), or an explicit `QRMUX_BIN` override wins.
//! `qrmux_bin_checked()` asserts the binary exists and prints its mtime + size so
//! the operator can confirm it was rebuilt from THIS worktree before any D7
//! RUN-proof counts (run `cargo build -p qrmux` into the shared target first).
//!
//! **D7 applicability.** The D7 cells are `Required` on claude-code (the sole
//! `Hosting::MuxPane` lane) and `NaPermitted` on the four `Hosting::Daemon`
//! lanes. The claude-code D7 drivers exercise the standalone `qrmux` binary
//! nested in a real tmux with a plain shell pane — the render/reflow/attach path
//! is server-side and provider-invariant, and qd's claude MuxPane attaches via
//! the identical `attach_session` path (embedded_mux.rs:461), so this is a
//! faithful proxy for the claude attach surface with NO token/cred cost.

#[path = "common/live_gate.rs"]
mod live_gate;

use dispatch::conformance::journal::{AuthorityJournal, TerminalState};
use dispatch::conformance::registry::conformance_battery;
use dispatch::conformance::{
    harness, AggregationVersion, BoxId, CellId, CommissioningHeader, Lane, LaneScope,
    ManifestDigest, RunArtifact, RunArtifactBuilder, RunId, RunKind, RunMode,
};

fn live() -> bool {
    live_gate::conformance_gate_truthy("QD_C2_LIVE", "conformance-c2")
}

/// Point the harness at THIS worktree's freshly-built `qd` (see the module doc on
/// the shared-target trap; identical to `conformance_c1_live.rs::set_qd_bin`).
fn set_qd_bin() {
    std::env::set_var("QD_BIN", env!("CARGO_BIN_EXE_qd"));
}

/// Resolve, check, and export the `qrmux` binary for the D7 cells. Prints its
/// provenance (mtime + size) so a stale binary is caught before it counts.
fn set_qrmux_bin() {
    let qrmux = std::env::var("QRMUX_BIN").unwrap_or_else(|_| {
        // Sibling binary in the same target dir as this test's qd.
        env!("CARGO_BIN_EXE_qd").replace("/qd", "/qrmux")
    });
    let meta = std::fs::metadata(&qrmux).unwrap_or_else(|e| {
        panic!(
            "qrmux binary {qrmux:?} not found ({e}) — build it into the shared target first: \
             `cargo build -p qrmux` (then re-run). D7 cells drive the real qrmux binary."
        )
    });
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    eprintln!(
        "[c2] QRMUX_BIN={qrmux} (size={} bytes, mtime_epoch={mtime}) — fingerprint before trusting a D7 RUN-proof",
        meta.len()
    );
    std::env::set_var("QRMUX_BIN", qrmux);
}

fn descriptive_now() -> String {
    std::process::Command::new("date")
        .arg("-u")
        .arg("+%Y-%m-%dT%H:%M:%SZ")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Commission → start → execute each cell → build → complete ONE lane's run,
/// against `journal`, sequentially (the C-1 pattern). Returns the finished
/// artifact; panics on any structural violation.
fn run_one_lane_cells(
    journal: &mut AuthorityJournal,
    lane: Lane,
    cells: &[CellId],
    manifest_digest: &ManifestDigest,
    run_index: usize,
) -> RunArtifact {
    let run_id = RunId(format!(
        "c2-pipeline-{}-{}-{run_index}",
        lane.id().replace('/', "-"),
        std::process::id()
    ));
    let session_name = run_id.0.clone();

    let mut tok_n = 0u64;
    let tuple = journal
        .commission_run(
            run_id.clone(),
            LaneScope::one(lane),
            BoxId("lima".into()),
            std::env::var("QD_C2_RELEASE_COMMIT").unwrap_or_else(|_| "unknown".into()),
            manifest_digest.clone(),
            AggregationVersion("agg-v1".into()),
            RunKind::Evidence,
            "c2-executor-1",
            &mut || {
                tok_n += 1;
                format!("c2-pipeline-tok-{}-{run_index}-{tok_n}", lane.id())
            },
        )
        .expect("commission_run");

    let mut nonce_n = 0u64;
    let nonce = journal
        .start_run(&tuple.run, "c2-runner-1", &mut || {
            nonce_n += 1;
            format!("c2-pipeline-nonce-{}-{run_index}-{nonce_n}", lane.id())
        })
        .expect("start_run");

    let header = CommissioningHeader::new(tuple, nonce);
    let mut builder = RunArtifactBuilder::new(header, descriptive_now());

    for cell in cells {
        let cell_session = format!("{session_name}-{}", cell.0.replace('.', "-"));
        let outcome = harness::run_cell(lane, cell, builder.runner(), &cell_session)
            .unwrap_or_else(|| panic!("{}: no harness driver for {} yet", lane.id(), cell.0));
        builder.observe(lane, cell.clone(), RunMode::Automated, outcome);
    }

    let applicable: Vec<(Lane, CellId)> = cells.iter().map(|c| (lane, c.clone())).collect();
    let artifact = builder
        .build(&applicable)
        .expect("build (within-run driven-cell fullness)");
    // C-4 (a): deposit into the shared run dir (if configured) for the minting invoker.
    if let Ok(run_dir) = std::env::var("QD_CONFORMANCE_RUNDIR") {
        dispatch::conformance::persist_artifact(std::path::Path::new(&run_dir), &artifact)
            .expect("persist conformance artifact to QD_CONFORMANCE_RUNDIR");
    }
    let json = serde_json::to_vec(&artifact).expect("artifact serializes");
    let artifact_digest = {
        use sha2::{Digest, Sha256};
        dispatch::conformance::ArtifactDigest(format!("sha256:{:x}", Sha256::digest(&json)))
    };
    journal
        .mark_terminal(&run_id, TerminalState::Completed { artifact_digest })
        .expect("mark_terminal Completed");
    artifact
}

/// `d6.partial-write` on all five lanes — the delivery-log reader rejects a
/// truncated record (transport-agnostic; every lane Required). Cred-free,
/// daemon-free, deterministic — a pure reader test over real files.
#[test]
fn c2_partial_write_all_lanes() {
    if !live() {
        eprintln!("QD_C2_LIVE != 1 — skipping c2 partial-write");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d6.partial-write".to_string())];
    let lanes = [
        Lane::Pi,
        Lane::Codex,
        Lane::ClaudeCodeAcp,
        Lane::Opencode,
        Lane::ClaudeCode,
    ];
    let mut journal = AuthorityJournal::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.id(), obs.cell.0, obs.outcome);
            assert!(
                obs.outcome.is_pass(),
                "{}/{}: partial-write must PASS (reader rejects the truncated record), got {:?}",
                lane.id(),
                obs.cell.0,
                obs.outcome
            );
        }
    }
    journal.integrity_check().expect("journal integrity");
}

/// `d6.advertised-surface-honesty` on all five lanes. EXPECTED FAIL on today's
/// tree: `start --attach` / `start --port` are advertised as functional in the
/// curated help yet unconditionally exit-1 (doc-drift). The `stop --force`
/// honest-disclosure control is the PASS arm proving the detector discriminates
/// — both arms are in each observation's evidence.
#[test]
fn c2_advertised_surface_all_lanes() {
    if !live() {
        eprintln!("QD_C2_LIVE != 1 — skipping c2 advertised-surface");
        return;
    }
    set_qd_bin();
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d6.advertised-surface-honesty".to_string())];
    let lanes = [
        Lane::Pi,
        Lane::Codex,
        Lane::ClaudeCodeAcp,
        Lane::Opencode,
        Lane::ClaudeCode,
    ];
    let mut journal = AuthorityJournal::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.id(), obs.cell.0, obs.outcome);
            // EXPECTED FAIL while the start --attach/--port drift is live. A Pass
            // here would mean the advertised surface was corrected (revisit).
            assert!(
                obs.outcome.is_fail(),
                "{}/{}: expected FAIL (start --attach/--port advertised-but-broken doc-drift is live); got {:?}. A Pass means the drift was fixed — revisit this expectation.",
                lane.id(),
                obs.cell.0,
                obs.outcome
            );
        }
    }
    journal.integrity_check().expect("journal integrity");
}

/// D7 on the four lanes with no terminal of their own — every D7 cell resolves
/// NotApplicable (nothing to attach a qrmux client to). That is the four of the
/// five that are not `claude-code/mux-pane`: two ACP bridges and two headless
/// residents, which `Lane::is_pane` answers `false` for alike. Cred-free, no
/// fixture. Confirms the no-terminal guard resolves (never a None fall-through,
/// never a silent narrowing).
#[test]
fn c2_d7_daemon_lanes_not_applicable() {
    if !live() {
        eprintln!("QD_C2_LIVE != 1 — skipping c2 d7 daemon-NA");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let d7_cells: Vec<CellId> = [
        "d7.nested-render-correctness",
        "d7.detach-chord-reaches-qrmux",
        "d7.mouse-passthrough",
        "d7.term-terminfo-honest",
        "d7.altscreen-enter-exit-across-nesting",
        "d7.cross-size-reattach-both-directions",
        "d7.zellij-nested-render-coverage",
    ]
    .iter()
    .map(|s| CellId(s.to_string()))
    .collect();
    let lanes = [Lane::Pi, Lane::Codex, Lane::ClaudeCodeAcp, Lane::Opencode];
    let mut journal = AuthorityJournal::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        let artifact = run_one_lane_cells(&mut journal, lane, &d7_cells, &manifest_digest, i);
        for obs in artifact.observations() {
            assert!(
                matches!(obs.outcome, dispatch::conformance::Outcome::NotApplicable { .. }),
                "{}/{}: expected NotApplicable (the lane has no terminal of its own), got {:?}",
                lane.id(),
                obs.cell.0,
                obs.outcome
            );
        }
    }
    journal.integrity_check().expect("journal integrity");
}

/// D7 render family on claude-code (the MuxPane lane) — real qrmux nested in a
/// real tmux, plain shell pane. The five clean-observable property cells PASS
/// (render/detach/TERM/altscreen/mouse), the zellij cell resolves Blocked (no
/// zellij on this box), and `d7.cross-size-reattach-both-directions` is EXPECTED
/// FAIL (the SC-1 gap — qrmux truncates rather than reflowing on a width change;
/// live-garble discrimination per B-1). Needs a real qrmux binary (QD_C2_LIVE +
/// a built qrmux); nested in tmux, so a real terminal is not required.
#[test]
fn c2_d7_claude_render_family() {
    if !live() {
        eprintln!("QD_C2_LIVE != 1 — skipping c2 d7 claude render family");
        return;
    }
    set_qrmux_bin();
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    // Each cell in its own single-cell run so one flaky nesting never masks the
    // others, and the cross-size RED is isolated as its own artifact.
    let pass_cells = [
        "d7.nested-render-correctness",
        "d7.detach-chord-reaches-qrmux",
        "d7.term-terminfo-honest",
        "d7.altscreen-enter-exit-across-nesting",
        "d7.mouse-passthrough",
    ];
    let mut journal = AuthorityJournal::new();
    let mut idx = 0;
    for id in pass_cells {
        let cells = [CellId(id.to_string())];
        let artifact =
            run_one_lane_cells(&mut journal, Lane::ClaudeCode, &cells, &manifest_digest, idx);
        idx += 1;
        for obs in artifact.observations() {
            eprintln!("claude-code: {}: {:?}", obs.cell.0, obs.outcome);
            assert!(
                obs.outcome.is_pass(),
                "claude-code/{}: expected Pass (nested render property holds), got {:?}",
                obs.cell.0,
                obs.outcome
            );
        }
    }

    // zellij → structured Blocked (per-box gate; no zellij on lima).
    {
        let cells = [CellId("d7.zellij-nested-render-coverage".to_string())];
        let artifact =
            run_one_lane_cells(&mut journal, Lane::ClaudeCode, &cells, &manifest_digest, idx);
        idx += 1;
        for obs in artifact.observations() {
            eprintln!("claude-code: {}: {:?}", obs.cell.0, obs.outcome);
            assert!(
                obs.outcome.is_blocked(),
                "claude-code/{}: expected Blocked (no zellij on this box), got {:?}",
                obs.cell.0,
                obs.outcome
            );
        }
    }

    // cross-size-reattach → EXPECTED FAIL (the SC-1 garble discriminator).
    {
        let cells = [CellId("d7.cross-size-reattach-both-directions".to_string())];
        let artifact =
            run_one_lane_cells(&mut journal, Lane::ClaudeCode, &cells, &manifest_digest, idx);
        for obs in artifact.observations() {
            eprintln!("claude-code: {}: {:?}", obs.cell.0, obs.outcome);
            assert!(
                obs.outcome.is_fail(),
                "claude-code/{}: expected FAIL (SC-1 cross-width reflow garble is live — qrmux truncates rather than reflowing); got {:?}. A Pass means the qrmux reflow was fixed — then discriminate against C-4's D7 mutant per B-1.",
                obs.cell.0,
                obs.outcome
            );
        }
    }

    journal.integrity_check().expect("journal integrity across the claude d7 family");
}

/// `d6.wedged-daemon` on the daemon lanes — a live identity-verified resident whose
/// endpoint is CAMPED (a hang-server that accepts the TCP connect but never services
/// the ws upgrade) must make send:relay fail LOUD within a bounded time AND record a
/// truthful send-failed terminal. Cred-free + jailed (no real daemon; a python3
/// identity process passes the acp/pi cmdline fence, the in-process HangServer camps
/// the endpoint port). Each observation asserts the wedge accepted the connection
/// (injection proof).
///
/// Per-lane, honestly split (grounded in the send-path trace):
///  - acp/{claude-code,opencode} + pi PASS: AcpConnection/PiRemote::connect set the
///    read/write timeout BEFORE the tungstenite handshake → the camped upgrade fails
///    at the 5s bound → loud exit + a send-failed delivery-log terminal.
///  - codex FAIL (an HONEST finding): WsAppServer::connect (ws.rs:73) calls bare
///    `tungstenite::connect` and applies the read timeout only AFTER it returns, so a
///    camped upgrade read is UNBOUNDED → the send hangs forever (the bounded harness
///    group-kills it and records the FAIL). A real qd product defect — codex lacks
///    the pre-handshake timeout acp/pi have; the product FIX is out of C-2 scope,
///    surfaced + documented durably. A Pass on codex would mean the ws connect was
///    bounded (the defect fixed) — revisit then.
#[test]
fn c2_wedged_daemon_lanes() {
    if !live() {
        eprintln!("QD_C2_LIVE != 1 — skipping c2 wedged-daemon");
        return;
    }
    set_qd_bin();
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d6.wedged-daemon".to_string())];
    // acp/pi bound the wedged handshake (PASS); codex does NOT (unbounded → FAIL).
    let lanes = [Lane::ClaudeCodeAcp, Lane::Opencode, Lane::Pi, Lane::Codex];
    let mut journal = AuthorityJournal::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.id(), obs.cell.0, obs.outcome);
            if lane == Lane::Codex {
                assert!(
                    obs.outcome.is_fail(),
                    "codex/{}: expected FAIL (WsAppServer::connect ws.rs:73 does not bound a camped ws-upgrade handshake → the send hangs unbounded — a real qd defect surfaced honestly); got {:?}. A Pass means the ws connect was bounded (defect fixed) — revisit.",
                    obs.cell.0,
                    obs.outcome
                );
            } else {
                assert!(
                    obs.outcome.is_pass(),
                    "{}/{}: wedged-daemon must PASS (fail loud within the timeout + truthful send-failed terminal), got {:?}",
                    lane.id(),
                    obs.cell.0,
                    obs.outcome
                );
            }
        }
    }
    journal.integrity_check().expect("journal integrity");
}

/// `d6.sender-killed-mid-send` on the daemon lanes — the sender is killed
/// (`kill -KILL -- -<pgid>`) while blocked on the wedge; the delivery log must
/// carry NO falsely-completed/seen terminal. Cred-free + jailed.
#[test]
fn c2_sender_killed_lanes() {
    if !live() {
        eprintln!("QD_C2_LIVE != 1 — skipping c2 sender-killed");
        return;
    }
    set_qd_bin();
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d6.sender-killed-mid-send".to_string())];
    let lanes = [Lane::ClaudeCodeAcp, Lane::Opencode, Lane::Pi, Lane::Codex];
    let mut journal = AuthorityJournal::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.id(), obs.cell.0, obs.outcome);
            assert!(
                obs.outcome.is_pass(),
                "{}/{}: sender-killed must PASS (no phantom success terminal), got {:?}",
                lane.id(),
                obs.cell.0,
                obs.outcome
            );
        }
    }
    journal.integrity_check().expect("journal integrity");
}

/// `d6.dead-relay-sidecar` on claude-code — an honest FAIL: a dead relay sidecar
/// makes send:relay fail loud but record NO delivery-log terminal (the claude
/// relay-transport door is stderr-only). EXPECTED FAIL. The driver's no-relay
/// CONTROL arm proves the detector sees a send-failed terminal when one is
/// written, so the finding's absence is real.
#[test]
fn c2_dead_relay_sidecar_claude() {
    if !live() {
        eprintln!("QD_C2_LIVE != 1 — skipping c2 dead-relay-sidecar");
        return;
    }
    set_qd_bin();
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d6.dead-relay-sidecar".to_string())];
    let mut journal = AuthorityJournal::new();
    let artifact = run_one_lane_cells(&mut journal, Lane::ClaudeCode, &cells, &manifest_digest, 0);
    for obs in artifact.observations() {
        eprintln!("claude-code: {}: {:?}", obs.cell.0, obs.outcome);
        assert!(
            obs.outcome.is_fail(),
            "claude-code/{}: expected FAIL (dead relay sidecar → loud error but no delivery-log terminal — honest gap); got {:?}. A Pass means the product now writes a terminal on relay-transport failure — revisit.",
            obs.cell.0,
            obs.outcome
        );
    }
    journal.integrity_check().expect("journal integrity");
}
