//! C-1 full-pipeline live proof: for each already-live-probed lane, commission
//! a run → redeem it (run-start, minting the launch nonce) → execute its
//! `d1.boot-readiness` cell through `conformance::harness` → build a real
//! `RunArtifact` → serialize → mark the journal entry `Completed`, citing the
//! artifact's own content digest (R6-1: publication IS completion). Proves
//! the WHOLE schema-to-artifact chain end-to-end, not just the harness's raw
//! subprocess logic in isolation (which is separately proven per lane —
//! see `conformance::harness`'s doc comments and the commits that landed
//! each driver).
//!
//! Every lane's `RunEntry` lands in the SAME `AuthorityJournal` (one ordinal
//! domain — F-3/N-1), but each lane gets its OWN commissioned run: this
//! mirrors the serialized, one-daemon-at-a-time live protocol
//! (conf-build-coord-2/mc-5, 2026-07-14) — commission → start → execute →
//! teardown → complete, THEN the next lane's run begins — never two lanes'
//! runs overlapping in wall-clock.
//!
//! Gated on `QD_C1_LIVE=1` — a no-op otherwise, so the default suite never
//! spins a live daemon (same convention as `codex_conformance_live.rs` and
//! every other `*_live.rs` seed).
//!
//! **Two `#[test]` fns live in this file** (the five-lane grid + the
//! claude-code-only lane below) — the Rust test harness runs `#[test]` fns
//! CONCURRENTLY by default, which can overlap two lanes' live daemons and
//! break the one-daemon-at-a-time serialize discipline. ALWAYS pass `--
//! --test-threads=1` explicitly on every invocation (there is no ambient
//! default that provides this — a bare compiled-binary run, exactly like
//! `cargo test`, is multi-threaded unless told otherwise). Live-caught
//! 2026-07-14: an invocation that omitted this flag DID run two lanes'
//! daemons concurrently for a moment (harmless here — RAM still recovered
//! above baseline, zero residual processes — but it was luck, not design;
//! don't rely on individual footprints staying small enough to absorb it).
//!
//! claude-code (bare) IS included, LAST in the lane order: its
//! `d1.boot-readiness` cell boots through `bond commission`'s zero-completion
//! warm-leaf mechanism (see `harness::claude_code::boot_readiness`'s doc
//! comment for the full story — `qd start`'s prompt-driven shapes are both
//! refused, and bypassing `qd` to call the model directly was correctly
//! rejected as measuring the wrong thing). No token concern applies — this
//! is re-runnable like every other lane.

#[path = "common/live_gate.rs"]
mod live_gate;

use dispatch::conformance::journal::{AuthorityJournal, TerminalState};
use dispatch::conformance::registry::conformance_battery;
use dispatch::conformance::{
    harness, AggregationVersion, BoxId, CellId, CommissioningHeader, Lane, LaneScope,
    ManifestDigest, RunArtifact, RunArtifactBuilder, RunId, RunKind, RunMode,
};

fn live() -> bool {
    live_gate::conformance_gate_truthy("QD_C1_LIVE", "conformance-c1")
}

/// Point the harness at THIS worktree's freshly-built `qd`
/// (`env!("CARGO_BIN_EXE_qd")`, available only inside a test binary) instead of
/// whatever `qd` is on `PATH` (which can be days-stale — live-caught 2026-07-14).
///
/// **Thread-safety:** this mutates PROCESS-GLOBAL env via `set_var`, and is NOT
/// restored (there is no meaningful prior value — `QD_BIN` exists only to steer
/// these live tests). That is sound ONLY because this whole file is mandated to
/// run `--test-threads=1` (see the module doc — the daemon-serialization
/// discipline already requires it; a concurrent run would also race this
/// mutation). Centralized here so the process-global write has ONE call site
/// with the invariant stated, rather than three bare `set_var`s that silently
/// imply thread-safety they do not have.
fn set_qd_bin() {
    std::env::set_var("QD_BIN", env!("CARGO_BIN_EXE_qd"));
}

/// Descriptive timestamp only (never a precedence input — see journal.rs).
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

/// Commission → redeem → execute EVERY cell in `cells` (same run, same
/// session, one qd/bond start+stop cycle PER cell — each cell's driver owns
/// its own process lifecycle) → build → complete ONE lane's run against
/// `journal`, sequentially. Returns the finished artifact. Panics (via
/// `expect`) on any structural violation — a full-pipeline proof that
/// silently swallowed a defect would be worse than no proof at all.
fn run_one_lane_cells(
    journal: &mut AuthorityJournal,
    lane: Lane,
    cells: &[CellId],
    manifest_digest: &ManifestDigest,
    run_index: usize,
) -> RunArtifact {
    let run_id = RunId(format!(
        "c1-pipeline-{}-{}-{run_index}",
        lane.provider_id().replace('/', "-"),
        std::process::id()
    ));
    let session_name = run_id.0.clone();

    let mut tok_n = 0u64;
    let tuple = journal
        .commission_run(
            run_id.clone(),
            LaneScope::one(lane),
            BoxId("lima".into()),
            std::env::var("QD_C1_RELEASE_COMMIT").unwrap_or_else(|_| "unknown".into()),
            manifest_digest.clone(),
            AggregationVersion("agg-v1".into()),
            RunKind::Evidence,
            "c1-executor (b45d4thh)",
            &mut || {
                tok_n += 1;
                format!("c1-pipeline-tok-{}-{run_index}-{tok_n}", lane.provider_id())
            },
        )
        .expect("commission_run");

    let mut nonce_n = 0u64;
    let nonce = journal
        .start_run(&tuple.run, "c1-runner (b45d4thh)", &mut || {
            nonce_n += 1;
            format!(
                "c1-pipeline-nonce-{}-{run_index}-{nonce_n}",
                lane.provider_id()
            )
        })
        .expect("start_run");

    let header = CommissioningHeader::new(tuple, nonce);
    let mut builder = RunArtifactBuilder::new(header, descriptive_now());

    for cell in cells {
        // Each cell gets its own uniquely-named session so two cells' start
        // cycles never collide (a cell may itself start+stop a real daemon).
        let cell_session = format!("{session_name}-{}", cell.0.replace('.', "-"));
        let outcome = harness::run_cell(lane, cell, builder.runner(), &cell_session)
            .unwrap_or_else(|| panic!("{}: no harness driver for {} yet", lane.provider_id(), cell.0));
        builder.observe(lane, cell.clone(), RunMode::Automated, outcome);
    }

    // HONEST BOUNDARY (do not read this as full-domain completeness): `applicable`
    // is derived from EXACTLY the cells this call just drove, so build()'s
    // "every applicable cell resolves" guard is, HERE, only a within-run
    // consistency check (each driven cell produced exactly one resolution + its
    // proof binds to this header) — it structurally CANNOT catch a Required cell
    // that was silently dropped from the drive list, because a dropped cell never
    // enters `applicable`. Full-domain completeness — that the run covered every
    // cell the battery manifest declares Required for this lane — is corpus-scope
    // C-3 (A10 v5) against the assembled corpus, NOT this per-run builder.
    // `Battery::applicable_domain()` is the manifest-authoritative source C-3 uses
    // for that; it is deliberately NOT fed here, so this call proves only what it
    // can (driven-cell fullness), never implying the stronger claim.
    let applicable: Vec<(Lane, CellId)> = cells.iter().map(|c| (lane, c.clone())).collect();
    let artifact = builder
        .build(&applicable)
        .expect("build (within-run driven-cell fullness + nonce/token consistency; full-domain completeness is C-3)");
    // C-4 (a): deposit the built artifact into the shared run dir (if configured)
    // so the fail-closed minting invoker can aggregate it; C-5's measured run
    // consumes the same deposits. No-op when unset (normal dev runs).
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

#[test]
fn c1_pipeline_five_lanes_sequential_grid() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 full-pipeline live proof");
        return;
    }

    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let base_cells = [
        CellId("d1.boot-readiness".to_string()),
        CellId("d1.teardown-reaps-process-group".to_string()),
        CellId("d1.launch-addressing".to_string()),
        CellId("d1.transport-is-our-daemon".to_string()),
        CellId("d1.liveness-process-alive".to_string()),
        CellId("d1.reconnect-resolves-via-registry".to_string()),
    ];
    // d1.resume-same-session-id: codex resolves NotApplicable (proven,
    // zero-token — see registry.rs's NaPermitted declaration), so it still
    // gets it here. pi was ALSO NaPermitted here until 2026-07-15, when
    // conf-build-coord-3 reclassified it to Required after
    // d5.resume-jsonl-continuity-and-recall's pi driver disproved the old
    // NaPermitted reason (qd resume genuinely works and preserves the
    // session id) — pi's resume cell now needs a real prior turn to revive
    // into (manually confirmed: a cred-free resume hangs), so it's a
    // token-drawing cell, folded into c1_pipeline_resume_and_d1resume_pi
    // alongside d5's cell rather than proven in this zero-token grid.
    // AcpClaudeCode/Opencode/ClaudeCode's drivers are deliberately UNWIRED
    // (see harness::run_cell's comment) — reclassified as a token cell
    // 2026-07-14 (conf-build-coord-3 ruling) after `qd resume` genuinely
    // refused a zero-completion boot ("no resumable transcript"); folded
    // into the D2/D5 token-drawing tranche instead of proven here.
    let resume_cell = CellId("d1.resume-same-session-id".to_string());

    // Serialized, one lane's run at a time — matches the live-run protocol
    // exactly. claude-code is LAST and deliberately last: everything else has
    // already been proven live-clean, so if anything upstream were to panic,
    // claude-code's cells never fire.
    let lanes = [
        Lane::Pi,
        Lane::Codex,
        Lane::AcpClaudeCode,
        Lane::Opencode,
        Lane::ClaudeCode,
    ];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 pipeline: {} ===", lane.provider_id());
        let mut cells = base_cells.to_vec();
        if matches!(lane, Lane::Codex) {
            cells.push(resume_cell.clone());
        }
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }

    journal
        .integrity_check()
        .expect("journal integrity across all five lanes' runs");

    // Four lanes must have actually passed both cells (live-probed clean
    // 2026-07-14, including claude-code once its driver was corrected to the
    // zero-completion bond-commission warm-leaf boot — no token concern
    // remains, so it is hard-asserted like every other lane); codex is
    // expected Blocked on both (same version-pin gate on this box, fires
    // before either cell's daemon exists).
    for artifact in &artifacts {
        for obs in artifact.observations() {
            match (obs.lane, obs.cell.0.as_str()) {
                // codex declares NaPermitted for resume (no analogous
                // primitive per the registry) — the harness must actually
                // produce NotApplicable, never Pass/Blocked/Fail here.
                (Lane::Codex, "d1.resume-same-session-id") => assert!(
                    matches!(obs.outcome, dispatch::conformance::Outcome::NotApplicable { .. }),
                    "{}/{}: expected NotApplicable, got {:?}",
                    obs.lane.provider_id(),
                    obs.cell.0,
                    obs.outcome
                ),
                (Lane::Codex, _) => assert!(
                    obs.outcome.is_blocked(),
                    "codex/{}: must resolve Blocked (version-pin gate), got {:?}",
                    obs.cell.0,
                    obs.outcome
                ),
                _ => assert!(
                    obs.outcome.is_pass(),
                    "{}/{}: expected Pass, got {:?}",
                    obs.lane.provider_id(),
                    obs.cell.0,
                    obs.outcome
                ),
            }
        }
    }

    eprintln!(
        "C-1 pipeline proof OK: {} runs, journal integrity clean",
        artifacts.len()
    );
}

/// The claude-code lane, ALONE, through the same journal-backed pipeline —
/// deliberately separate from the five-lane test above so it can run without
/// redundantly re-booting the four other lanes (useful when iterating on
/// just this lane's driver). Zero-token (`bond commission` warm-leaf boot),
/// so this is freely re-runnable like every other lane's own isolated test.
#[test]
fn c1_pipeline_claude_code_lane_only() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 claude-code-lane live proof");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    // d1.resume-same-session-id deliberately excluded: reclassified as a
    // token cell 2026-07-14 (conf-build-coord-3 ruling) — see
    // harness::run_cell's comment and c1_pipeline_five_lanes_sequential_grid
    // above.
    let cells = [
        CellId("d1.boot-readiness".to_string()),
        CellId("d1.teardown-reaps-process-group".to_string()),
        CellId("d1.launch-addressing".to_string()),
        CellId("d1.transport-is-our-daemon".to_string()),
        CellId("d1.liveness-process-alive".to_string()),
        CellId("d1.reconnect-resolves-via-registry".to_string()),
    ];

    let mut journal = AuthorityJournal::new();
    let artifact = run_one_lane_cells(&mut journal, Lane::ClaudeCode, &cells, &manifest_digest, 0);
    journal.integrity_check().expect("journal integrity");

    for obs in artifact.observations() {
        eprintln!("claude-code (bare): {}: {:?}", obs.cell.0, obs.outcome);
        assert!(
            obs.outcome.is_pass(),
            "claude-code bare/{}: expected Pass, got {:?}",
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d1.multiplex-concurrent-distinct-residents` on `pi` ALONE — deliberately
/// isolated from every other test in this file. This is the ONE cell whose
/// own claim requires two concurrent daemons, a genuine, explicitly-flagged
/// exception to the one-daemon-at-a-time serialize discipline every other
/// cell holds to (see `harness::multiplex_concurrent_via_cli`'s doc
/// comment). Cleared by mc-5 for the pi lane ONLY, 2026-07-14 (the lightest,
/// cred-free case) — every other lane's concurrent-pair footprint needs its
/// own re-check before a driver gets wired into `run_cell` for it.
#[test]
fn c1_pipeline_multiplex_pi_only() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 multiplex-concurrent live proof");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d1.multiplex-concurrent-distinct-residents".to_string())];

    let mut journal = AuthorityJournal::new();
    let artifact = run_one_lane_cells(&mut journal, Lane::Pi, &cells, &manifest_digest, 0);
    journal.integrity_check().expect("journal integrity");

    let obs = artifact.observations().next().expect("one observation");
    eprintln!("pi: {}: {:?}", obs.cell.0, obs.outcome);
    assert!(
        obs.outcome.is_pass(),
        "pi/{}: expected Pass, got {:?}",
        obs.cell.0,
        obs.outcome
    );
}

/// `d1.multiplex-concurrent-distinct-residents` on `codex`. Wired in
/// `run_cell` but never actually fired live this whole composite (mc-5
/// only cleared the pi pair) — the acceptance-candidate table reported it
/// as "wired/unconfirmed" rather than assuming a status by pattern-match.
/// Safe/cheap to confirm: codex's version-pin gate fires on the FIRST of
/// the two `qd start` attempts, before any daemon exists — same zero-RAM,
/// zero-token shape as every other codex D1 cell, so this needs no
/// separate mc-5 clearance (unlike a real 2-daemon lane).
#[test]
fn c1_pipeline_multiplex_codex_only() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 codex multiplex live proof");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d1.multiplex-concurrent-distinct-residents".to_string())];

    let mut journal = AuthorityJournal::new();
    let artifact = run_one_lane_cells(&mut journal, Lane::Codex, &cells, &manifest_digest, 0);
    journal.integrity_check().expect("journal integrity");

    let obs = artifact.observations().next().expect("one observation");
    eprintln!("codex: {}: {:?}", obs.cell.0, obs.outcome);
    assert!(
        obs.outcome.is_blocked(),
        "codex/{}: expected Blocked (version-pin gate), got {:?}",
        obs.cell.0,
        obs.outcome
    );
}

/// `d1.multiplex-concurrent-distinct-residents` on `acp/claude-code`.
/// mc-5-cleared 2026-07-15 (3-lane ask): SERIALIZED across lanes — run
/// this test alone (`--exact`), measure envelope immediately before, and
/// only proceed to the opencode/claude-code variants once this one is
/// confirmed clean. Two concurrent ACP bridge residents, ~10s transient,
/// torn down together.
#[test]
fn c1_pipeline_multiplex_acp_claude_code_only() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 acp/claude-code multiplex live proof");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d1.multiplex-concurrent-distinct-residents".to_string())];

    let mut journal = AuthorityJournal::new();
    let artifact = run_one_lane_cells(&mut journal, Lane::AcpClaudeCode, &cells, &manifest_digest, 0);
    journal.integrity_check().expect("journal integrity");

    let obs = artifact.observations().next().expect("one observation");
    eprintln!("acp/claude-code: {}: {:?}", obs.cell.0, obs.outcome);
    assert!(
        obs.outcome.is_pass(),
        "acp/claude-code/{}: expected Pass, got {:?}",
        obs.cell.0,
        obs.outcome
    );
}

/// `d1.multiplex-concurrent-distinct-residents` on `acp/opencode`.
/// mc-5-cleared, same serialize-across-lanes discipline — run second,
/// after acp/claude-code confirms clean.
#[test]
fn c1_pipeline_multiplex_opencode_only() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 acp/opencode multiplex live proof");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d1.multiplex-concurrent-distinct-residents".to_string())];

    let mut journal = AuthorityJournal::new();
    let artifact = run_one_lane_cells(&mut journal, Lane::Opencode, &cells, &manifest_digest, 0);
    journal.integrity_check().expect("journal integrity");

    let obs = artifact.observations().next().expect("one observation");
    eprintln!("acp/opencode: {}: {:?}", obs.cell.0, obs.outcome);
    assert!(
        obs.outcome.is_pass(),
        "acp/opencode/{}: expected Pass, got {:?}",
        obs.cell.0,
        obs.outcome
    );
}

/// `d1.multiplex-concurrent-distinct-residents` on bare `claude-code` —
/// the HEAVIEST of the 3 (~900MB per mc-5's estimate). mc-5-cleared but
/// GATED: hold and re-flag if avail is <~1.8G when this fires. Run last,
/// after the two ACP lanes confirm clean.
#[test]
fn c1_pipeline_multiplex_claude_code_only() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 claude-code multiplex live proof (heaviest of the 3, gated at <1.8G avail)");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d1.multiplex-concurrent-distinct-residents".to_string())];

    let mut journal = AuthorityJournal::new();
    let artifact = run_one_lane_cells(&mut journal, Lane::ClaudeCode, &cells, &manifest_digest, 0);
    journal.integrity_check().expect("journal integrity");

    let obs = artifact.observations().next().expect("one observation");
    eprintln!("claude-code: {}: {:?}", obs.cell.0, obs.outcome);
    assert!(
        obs.outcome.is_pass(),
        "claude-code/{}: expected Pass, got {:?}",
        obs.cell.0,
        obs.outcome
    );
}

/// `d6.cold-target-send-fails-loud-with-terminal` across the four
/// daemon-hosted lanes it's wired for (pi/codex/acp-claude-code/
/// acp-opencode — bare claude-code deliberately excluded, see
/// `harness::claude_code`'s module doc). Cred-free and fully jailed (no
/// real daemon, no real registry touched) — safe to run serialized like
/// every other multi-lane test, no special clearance needed unlike the
/// multiplex cell (this one never puts two live things up at once, and
/// nothing it does touches the real environment).
///
/// Sets `QD_BIN` to Cargo's `env!("CARGO_BIN_EXE_qd")` (the binary freshly
/// built from THIS worktree, only available in an actual test binary — not
/// to `harness.rs`'s library code) before running: live-caught 2026-07-14,
/// this cell's underlying `emit_door_failure` behavior is newer than the
/// deployed `qd` on `PATH` (5 days stale relative to this worktree) — the
/// exact same fixture that failed against the deployed binary passed
/// cleanly once pointed at the worktree's own build. See
/// `harness::qd_bin`'s doc comment for the full story.
#[test]
fn c1_pipeline_cold_target_four_lanes() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 cold-target live proof");
        return;
    }
    set_qd_bin();
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d6.cold-target-send-fails-loud-with-terminal".to_string())];
    let lanes = [Lane::Pi, Lane::Codex, Lane::AcpClaudeCode, Lane::Opencode];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 cold-target: {} ===", lane.provider_id());
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }
    journal.integrity_check().expect("journal integrity across all four lanes' runs");

    for artifact in &artifacts {
        let obs = artifact.observations().next().expect("one observation");
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d6.bridge-death-detection-no-false-positive` on the two ACP-family
/// lanes it's `Required` for. Pure in-process `AcpHost` drive — no `qd`
/// subprocess, no bridge binary, no node dependency — so no `QD_BIN`
/// override is needed here.
#[test]
fn c1_pipeline_bridge_death_two_lanes() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 bridge-death live proof");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d6.bridge-death-detection-no-false-positive".to_string())];
    let lanes = [Lane::AcpClaudeCode, Lane::Opencode];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 bridge-death: {} ===", lane.provider_id());
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }
    journal.integrity_check().expect("journal integrity across both lanes' runs");

    for artifact in &artifacts {
        let obs = artifact.observations().next().expect("one observation");
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d2.turn-phase-sequence-strict-order` on the two ACP-family lanes it's
/// `Required` for. Cred-free, deterministic (canned fake bridge, no
/// model/network). Drives real `qd acp-daemon`/`send:relay`/`wait`
/// subprocesses (like `d6.cold-target-send-fails-loud-with-terminal`) —
/// sets `QD_BIN` to this worktree's own build so the same stale-deployed-
/// binary trap that cell hit can't silently recur here.
#[test]
fn c1_pipeline_turn_phase_sequence_two_lanes() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 turn-phase-sequence live proof");
        return;
    }
    set_qd_bin();
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d2.turn-phase-sequence-strict-order".to_string())];
    let lanes = [Lane::AcpClaudeCode, Lane::Opencode];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 turn-phase-sequence: {} ===", lane.provider_id());
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }
    journal.integrity_check().expect("journal integrity across both lanes' runs");

    for artifact in &artifacts {
        let obs = artifact.observations().next().expect("one observation");
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d2.delivery-log-consistency-under-home-override` on the two ACP-family
/// lanes it's `Required` for. Cred-free, deterministic, no live gate. Same
/// `QD_BIN` reasoning as [`c1_pipeline_turn_phase_sequence_two_lanes`].
#[test]
fn c1_pipeline_delivery_log_home_override_two_lanes() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 delivery-log-home-override live proof");
        return;
    }
    set_qd_bin();
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d2.delivery-log-consistency-under-home-override".to_string())];
    let lanes = [Lane::AcpClaudeCode, Lane::Opencode];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 delivery-log-home-override: {} ===", lane.provider_id());
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }
    journal.integrity_check().expect("journal integrity across both lanes' runs");

    for artifact in &artifacts {
        let obs = artifact.observations().next().expect("one observation");
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d2.turn-correlation-and-completion` on the two ACP-family lanes (the
/// pi/codex/claude-code-bare drivers for this Required-everywhere cell are
/// not yet written — see `harness::run_cell`'s doc comment). REAL model
/// turns — draws from mc-5's cleared D2/D5 token-tranche budget
/// (2026-07-15). A single short "reply with exactly PONG" exchange per
/// lane, per that clearance.
#[test]
fn c1_pipeline_turn_correlation_two_lanes() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 turn-correlation live proof (draws real tokens)");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d2.turn-correlation-and-completion".to_string())];
    let lanes = [Lane::AcpClaudeCode, Lane::Opencode];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 turn-correlation: {} ===", lane.provider_id());
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }
    journal.integrity_check().expect("journal integrity across both lanes' runs");

    for artifact in &artifacts {
        let obs = artifact.observations().next().expect("one observation");
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d5.transcript-tee-captures-assistant-text` on the two ACP-family lanes
/// (same scope note as the turn-correlation cell above). REAL model turns.
#[test]
fn c1_pipeline_transcript_tee_two_lanes() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 transcript-tee live proof (draws real tokens)");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d5.transcript-tee-captures-assistant-text".to_string())];
    let lanes = [Lane::AcpClaudeCode, Lane::Opencode];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 transcript-tee: {} ===", lane.provider_id());
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }
    journal.integrity_check().expect("journal integrity across both lanes' runs");

    for artifact in &artifacts {
        let obs = artifact.observations().next().expect("one observation");
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d2.turn-correlation-and-completion` + `d5.transcript-tee-captures-assistant-text`
/// on the `pi` lane (real credentialed pi turns, `exclusivity-inferred`
/// receipt strength) and the `codex` lane (expected `Blocked` on this box —
/// version-pin gate). Draws real tokens on `pi` only.
#[test]
fn c1_pipeline_turn_correlation_and_tee_pi_codex() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 pi/codex turn-correlation+tee live proof (draws real tokens on pi)");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [
        CellId("d2.turn-correlation-and-completion".to_string()),
        CellId("d5.transcript-tee-captures-assistant-text".to_string()),
    ];
    let lanes = [Lane::Pi, Lane::Codex];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 turn-correlation+tee: {} ===", lane.provider_id());
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }
    journal.integrity_check().expect("journal integrity across both lanes' runs");

    for artifact in &artifacts {
        for obs in artifact.observations() {
            let expect_blocked = obs.lane == Lane::Codex;
            if expect_blocked {
                assert!(
                    obs.outcome.is_blocked(),
                    "{}/{}: expected Blocked (version-pin gate), got {:?}",
                    obs.lane.provider_id(),
                    obs.cell.0,
                    obs.outcome
                );
            } else {
                assert!(
                    obs.outcome.is_pass(),
                    "{}/{}: expected Pass, got {:?}",
                    obs.lane.provider_id(),
                    obs.cell.0,
                    obs.outcome
                );
            }
        }
    }
}

/// `d2.turn-correlation-and-completion` + `d5.transcript-tee-captures-assistant-text`
/// on the bare `claude-code` lane. REAL model turn via `bond commission` +
/// `qd send:relay` + `qd wait` (see `harness::claude_code::run_one_claude_code_turn`'s
/// doc comment for the mechanism-discovery story — the originally-ruled
/// `qd start --attach` path is genuinely unimplemented on this engine).
#[test]
fn c1_pipeline_turn_correlation_and_tee_claude_code() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 claude-code turn-correlation+tee live proof (draws real tokens)");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [
        CellId("d2.turn-correlation-and-completion".to_string()),
        CellId("d5.transcript-tee-captures-assistant-text".to_string()),
    ];

    let mut journal = AuthorityJournal::new();
    let artifact = run_one_lane_cells(&mut journal, Lane::ClaudeCode, &cells, &manifest_digest, 0);
    for obs in artifact.observations() {
        eprintln!("{}: {}: {:?}", Lane::ClaudeCode.provider_id(), obs.cell.0, obs.outcome);
    }
    journal.integrity_check().expect("journal integrity");

    for obs in artifact.observations() {
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d1.resume-same-session-id` on the two ACP-family lanes — the LAST cell
/// in the entire 19x5 matrix. REAL model turn (x1 per lane — the
/// structural precondition `qd resume` needs to revive into). Manually
/// verified end-to-end by hand before this driver was written: `qd
/// send:relay` genuinely works against a PLAIN `qd start`-booted ACP
/// resident (not just the D2 3-phase daemon fixture's `qd acp-daemon`
/// boot path), for both lanes.
#[test]
fn c1_pipeline_resume_same_session_id_acp_two_lanes() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 ACP resume-same-session-id live proof (draws real tokens, x1 per lane — LAST cell in the matrix)");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d1.resume-same-session-id".to_string())];
    let lanes = [Lane::AcpClaudeCode, Lane::Opencode];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 resume-same-session-id: {} ===", lane.provider_id());
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }
    journal.integrity_check().expect("journal integrity across both lanes' runs");

    for artifact in &artifacts {
        let obs = artifact.observations().next().expect("one observation");
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d5.resume-jsonl-continuity-and-recall` on the two ACP-family lanes it's
/// `Required` for. REAL model turns (x2 per lane — a codeword stated on
/// one bridge process, recalled on a fresh one after `session/load`).
/// Draws real tokens — mc-5-cleared D2/D5 tranche budget.
#[test]
fn c1_pipeline_resume_recall_two_lanes() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 resume-recall live proof (draws real tokens, x2 per lane)");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d5.resume-jsonl-continuity-and-recall".to_string())];
    let lanes = [Lane::AcpClaudeCode, Lane::Opencode];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 resume-recall: {} ===", lane.provider_id());
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }
    journal.integrity_check().expect("journal integrity across both lanes' runs");

    for artifact in &artifacts {
        let obs = artifact.observations().next().expect("one observation");
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d1.resume-same-session-id` + `d5.resume-jsonl-continuity-and-recall`
/// on the `pi` lane. Both REAL, both draw their own turn(s) independently
/// (d1: 1 turn; d5: 2 turns). Manually verified end-to-end by hand before
/// either driver was written — `qd resume` genuinely works for pi and
/// preserves the session id, but a cred-free resume (no prior real turn)
/// hangs, which is why d1 needed reclassifying from NaPermitted to
/// Required (registry.rs's correction comment, 2026-07-15) rather than
/// staying zero-token.
#[test]
fn c1_pipeline_resume_and_d1resume_pi() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 pi resume live proof (draws real tokens, x3 total)");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [
        CellId("d1.resume-same-session-id".to_string()),
        CellId("d5.resume-jsonl-continuity-and-recall".to_string()),
    ];

    let mut journal = AuthorityJournal::new();
    let artifact = run_one_lane_cells(&mut journal, Lane::Pi, &cells, &manifest_digest, 0);
    for obs in artifact.observations() {
        eprintln!("{}: {}: {:?}", Lane::Pi.provider_id(), obs.cell.0, obs.outcome);
    }
    journal.integrity_check().expect("journal integrity");

    for obs in artifact.observations() {
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d5.resume-jsonl-continuity-and-recall` on the bare `claude-code` lane
/// — the LAST cell in the whole C-1 composite. REAL model turns (x2).
/// Manually verified end-to-end by hand before this driver was written:
/// bond commission -> real turn (codeword) -> bond decommission -> qd
/// resume --no-attach (the same mechanism the existing
/// resume_same_session_id driver already proves preserves the session
/// id) -> real turn (recall) -> single continuous transcript jsonl.
#[test]
fn c1_pipeline_resume_recall_claude_code() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 claude-code resume-recall live proof (draws real tokens, x2 — LAST cell in the composite)");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d5.resume-jsonl-continuity-and-recall".to_string())];

    let mut journal = AuthorityJournal::new();
    let artifact = run_one_lane_cells(&mut journal, Lane::ClaudeCode, &cells, &manifest_digest, 0);
    for obs in artifact.observations() {
        eprintln!("{}: {}: {:?}", Lane::ClaudeCode.provider_id(), obs.cell.0, obs.outcome);
    }
    journal.integrity_check().expect("journal integrity");

    for obs in artifact.observations() {
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d3.queue-slot-released-no-leak` on the two ACP-family lanes it's
/// `Required` for. No pre-existing named seed — authored fresh as the
/// natural companion to the overflow cell (see
/// `harness::queue_slot_released_via_host`'s doc comment). Pure in-process
/// `AcpHost` + fake-bridge drive — no `QD_BIN` override needed.
#[test]
fn c1_pipeline_queue_slot_released_two_lanes() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 queue-slot-released live proof");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d3.queue-slot-released-no-leak".to_string())];
    let lanes = [Lane::AcpClaudeCode, Lane::Opencode];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 queue-slot-released: {} ===", lane.provider_id());
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }
    journal.integrity_check().expect("journal integrity across both lanes' runs");

    for artifact in &artifacts {
        let obs = artifact.observations().next().expect("one observation");
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d6.cancel-maps-to-truthful-terminal` on the two ACP-family lanes it's
/// `Required` for. Pure in-process `AcpHost` + python3 fixture drive (fixture
/// peer only, no live claude turn) — no `QD_BIN` override needed.
#[test]
fn c1_pipeline_cancel_mapping_two_lanes() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 cancel-mapping live proof");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d6.cancel-maps-to-truthful-terminal".to_string())];
    let lanes = [Lane::AcpClaudeCode, Lane::Opencode];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 cancel-mapping: {} ===", lane.provider_id());
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }
    journal.integrity_check().expect("journal integrity across both lanes' runs");

    for artifact in &artifacts {
        let obs = artifact.observations().next().expect("one observation");
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d6.self-terminate-on-wedged-child` on the two ACP-family lanes with a
/// wired driver (see `harness::self_terminate_on_wedged_child_via_host`'s
/// doc comment for the flagged pi/codex applicability question — this test
/// deliberately covers only the two lanes with a real mechanism to prove).
/// Pure in-process `wire::serve` + real `AcpHost` bridge-stand-in drive — no
/// `QD_BIN` override needed. Bounded at up to ~10s worst-case (two 5s serve
/// self-terminate windows) — still cred-free, deterministic, no live gate.
#[test]
fn c1_pipeline_wedged_child_two_lanes() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 wedged-child live proof");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d6.self-terminate-on-wedged-child".to_string())];
    let lanes = [Lane::AcpClaudeCode, Lane::Opencode];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 wedged-child: {} ===", lane.provider_id());
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }
    journal.integrity_check().expect("journal integrity across both lanes' runs");

    for artifact in &artifacts {
        let obs = artifact.observations().next().expect("one observation");
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}

/// `d6.self-terminate-on-wedged-child` on the `pi` lane — a REAL live probe
/// (mc-5-cleared single daemon, 2026-07-15). Unlike the two ACP lanes above
/// (which PASS via `wire::serve`'s confirmed-dead-child self-terminate guard),
/// pi has NO such guard: `serve_pi` swallows `get_state` errors, and `qd info`
/// derives `live` from the DAEMON pid, so a pi daemon whose `pi` child is killed
/// out from under it lingers FALSELY-LIVE (`live:true, status:idle`). This test
/// starts a real pi daemon, kills its child, and asserts the HONEST current
/// outcome — a FAIL — locking in the finding. Reclassified Required 2026-07-15
/// (conf-build-coord-3) after the prior NaPermitted reason was found false.
/// If this ever starts returning Pass, the underlying qd pi-liveness product
/// defect (escalated separately, out of C-1 scope) was fixed, and this cell's
/// expectation must be revisited. Needs a real pi daemon (QD_C1_LIVE); pi is
/// cred-free, so no additional creds. Zero-residual: the driver tears the
/// daemon + child down.
#[test]
fn c1_pipeline_self_terminate_pi() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 pi self-terminate live proof");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d6.self-terminate-on-wedged-child".to_string())];

    let mut journal = AuthorityJournal::new();
    let artifact = run_one_lane_cells(&mut journal, Lane::Pi, &cells, &manifest_digest, 0);
    journal.integrity_check().expect("journal integrity for the pi self-terminate run");
    let obs = artifact.observations().next().expect("one observation");
    eprintln!("pi: {}: {:?}", obs.cell.0, obs.outcome);
    assert!(
        obs.outcome.is_fail(),
        "pi/{}: expected a FAIL — pi lingers falsely-live when its child dies (the honest current \
         outcome); got {:?}. A Pass here would mean the underlying qd pi-liveness defect was fixed \
         — revisit this cell's expectation if so.",
        obs.cell.0,
        obs.outcome
    );
}

/// `d3.queue-overflow-honors-configured-capacity` on the two ACP-family
/// lanes it's `Required` for. Pure in-process `AcpHost` + fake-bridge drive
/// (deterministic node script, no model/network) — no `QD_BIN` override
/// needed.
#[test]
fn c1_pipeline_queue_overflow_two_lanes() {
    if !live() {
        eprintln!("QD_C1_LIVE != 1 — skipping the C-1 queue-overflow live proof");
        return;
    }
    let battery = conformance_battery();
    let manifest_digest = battery.manifest_digest();
    let cells = [CellId("d3.queue-overflow-honors-configured-capacity".to_string())];
    let lanes = [Lane::AcpClaudeCode, Lane::Opencode];

    let mut journal = AuthorityJournal::new();
    let mut artifacts = Vec::new();
    for (i, lane) in lanes.into_iter().enumerate() {
        eprintln!("=== C-1 queue-overflow: {} ===", lane.provider_id());
        let artifact = run_one_lane_cells(&mut journal, lane, &cells, &manifest_digest, i);
        for obs in artifact.observations() {
            eprintln!("{}: {}: {:?}", lane.provider_id(), obs.cell.0, obs.outcome);
        }
        artifacts.push(artifact);
    }
    journal.integrity_check().expect("journal integrity across both lanes' runs");

    for artifact in &artifacts {
        let obs = artifact.observations().next().expect("one observation");
        assert!(
            obs.outcome.is_pass(),
            "{}/{}: expected Pass, got {:?}",
            obs.lane.provider_id(),
            obs.cell.0,
            obs.outcome
        );
    }
}
