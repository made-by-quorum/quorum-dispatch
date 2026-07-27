//! The dimension registry — one battery, defined once, uniform dimensions
//! D1–D7 parameterized over the five lanes (T1, QS-3). The registry is the
//! **manifest**: the cell set plus the per-(lane, cell) applicability map. Its
//! content hash is the [`ManifestDigest`] A2's window eligibility keys on — any
//! change to cells or applicability starts a fresh window.
//!
//! C-1 owns the manifest and the operands it carries; C-3's A10 v5 (a run's
//! observation set == the manifest-authorized domain, checked over the assembled
//! corpus) *consumes* [`Battery::permits`] and [`Battery::applicable_domain`].
//! Uniformity (QS-3) — the same dimension/cell list for every lane, variance
//! only as declared n-a — is checkable here via [`Battery::uniformity_check`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::evidence::CellResolution;
use super::ids::{AggregationVersion, CellId, Lane, ManifestDigest};

/// The seven dimensions. D4 is defined-but-inactive: excluded from every
/// denominator, never counted as passed (it activates when the ACP↔TUI switch
/// machinery exists). Gating = {D1, D2, D6}.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Dimension {
    D1Lifecycle,
    D2Receipts,
    D3BusyQueue,
    D4ModeSwitch,
    D5Transcript,
    D6FailureHonesty,
    D7EnvMatrix,
}

impl Dimension {
    pub const ALL: [Dimension; 7] = [
        Dimension::D1Lifecycle,
        Dimension::D2Receipts,
        Dimension::D3BusyQueue,
        Dimension::D4ModeSwitch,
        Dimension::D5Transcript,
        Dimension::D6FailureHonesty,
        Dimension::D7EnvMatrix,
    ];

    /// Gating dimensions (the "100%"): D1 teardown, D2 receipts, D6 failure-honesty.
    pub fn is_gating(self) -> bool {
        matches!(
            self,
            Dimension::D1Lifecycle | Dimension::D2Receipts | Dimension::D6FailureHonesty
        )
    }

    /// Active dimensions contribute to denominators; D4 (mode-switch) does NOT —
    /// it is defined but inactive and never counted as passed.
    pub fn is_active(self) -> bool {
        !matches!(self, Dimension::D4ModeSwitch)
    }

    pub fn code(self) -> &'static str {
        match self {
            Dimension::D1Lifecycle => "d1",
            Dimension::D2Receipts => "d2",
            Dimension::D3BusyQueue => "d3",
            Dimension::D4ModeSwitch => "d4",
            Dimension::D5Transcript => "d5",
            Dimension::D6FailureHonesty => "d6",
            Dimension::D7EnvMatrix => "d7",
        }
    }
}

/// A concrete cell within a dimension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub id: CellId,
    pub dimension: Dimension,
    pub title: String,
}

/// How a (lane, cell) may resolve in this manifest. The operand C-3's v5 reads:
/// a run's resolution for a cell must be *permitted* by the manifest, and every
/// non-`OutOfDomain` cell must resolve to exactly one [`CellResolution`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "applicability", rename_all = "kebab-case")]
pub enum Applicability {
    /// A live measurement is owed: the cell must resolve to an observed
    /// Pass/Fail/Blocked (blocked only for a genuine per-box gate).
    Required,
    /// The manifest permits a declared NotApplicable here (with a
    /// stranger-acceptable reason) — the dimension genuinely does not apply.
    NaPermitted { reason: String },
    /// The manifest permits a BoxCoverageRecord here — runnable only elsewhere.
    CoveragePermitted,
    /// Not in this lane's domain at all — must NOT appear in the run's grid.
    OutOfDomain,
}

impl Applicability {
    pub fn in_domain(&self) -> bool {
        !matches!(self, Applicability::OutOfDomain)
    }
}

/// The battery: the cell set + the per-(lane, cell) applicability map + the
/// aggregation version. Its digest is the manifest identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Battery {
    pub cells: Vec<Cell>,
    /// (lane provider-id, cell id) → applicability. A `BTreeMap` so serialization
    /// (hence the digest) is canonical. Serialized as a `Vec` of triples: a JSON
    /// object cannot use a tuple as a key ("key must be a string"), so a plain
    /// derive would fail to round-trip through JSON — which C-3's corpus (which
    /// embeds the battery as the commit-derived manifest) requires for the
    /// fresh-reader byte-match. `BTreeMap` iteration is key-sorted, so the `Vec` is
    /// canonical too.
    #[serde(with = "applicability_serde")]
    pub applicability: BTreeMap<(String, String), Applicability>,
    pub aggregation_version: AggregationVersion,
}

/// Serialize the tuple-keyed applicability map as a canonical `Vec` of triples so
/// it round-trips through JSON (a JSON object cannot key on a tuple).
mod applicability_serde {
    use super::Applicability;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        map: &BTreeMap<(String, String), Applicability>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let v: Vec<(&String, &String, &Applicability)> =
            map.iter().map(|((a, b), app)| (a, b, app)).collect();
        v.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<(String, String), Applicability>, D::Error> {
        let v: Vec<(String, String, Applicability)> = Vec::deserialize(d)?;
        Ok(v.into_iter().map(|(a, b, app)| ((a, b), app)).collect())
    }
}

impl Battery {
    /// A cell by id.
    pub fn cell(&self, id: &CellId) -> Option<&Cell> {
        self.cells.iter().find(|c| &c.id == id)
    }

    /// The applicability of a (lane, cell); `OutOfDomain` if unmapped.
    pub fn applicability(&self, lane: Lane, cell: &CellId) -> &Applicability {
        self.applicability
            .get(&(lane.provider_id().to_string(), cell.0.clone()))
            .unwrap_or(&Applicability::OutOfDomain)
    }

    /// The manifest-authorized (lane, cell) domain: every in-domain pair. A run
    /// must resolve each of these to exactly one [`CellResolution`] (A2(d)
    /// fullness — the builder enforces it; an unresolved pair is a C-1 defect).
    pub fn applicable_domain(&self) -> Vec<(Lane, CellId)> {
        let mut out = Vec::new();
        for lane in Lane::ALL {
            for cell in &self.cells {
                if self.applicability(lane, &cell.id).in_domain() {
                    out.push((lane, cell.id.clone()));
                }
            }
        }
        out
    }

    /// Whether a resolution is permitted by the manifest for a (lane, cell) —
    /// the operand C-3's v5 evaluates over the corpus. `OutOfDomain` permits
    /// nothing (the cell must not appear); `Required` forbids n-a and coverage;
    /// `NaPermitted` additionally permits a declared NotApplicable; and
    /// `CoveragePermitted` additionally permits a coverage record.
    pub fn permits(&self, lane: Lane, cell: &CellId, resolution: &CellResolution) -> bool {
        use super::evidence::Outcome;
        match (self.applicability(lane, cell), resolution) {
            (Applicability::OutOfDomain, _) => false,
            // A declared NotApplicable outcome is permitted only where the
            // manifest declares n-a.
            (app, CellResolution::Observed(o)) => match &o.outcome {
                Outcome::NotApplicable { .. } => matches!(app, Applicability::NaPermitted { .. }),
                // Pass/Fail/Blocked are permitted for any in-domain cell.
                _ => app.in_domain(),
            },
            (app, CellResolution::Coverage(_)) => matches!(app, Applicability::CoveragePermitted),
        }
    }

    /// A content hash over the cell registry + applicability map + aggregation
    /// version (M-1). Canonical: cells are sorted by id, the applicability map is
    /// a `BTreeMap` (sorted keys), so two readers of the same battery derive the
    /// same digest.
    pub fn manifest_digest(&self) -> ManifestDigest {
        let mut cells = self.cells.clone();
        cells.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        // A deterministic view — never depends on Vec insertion order. The
        // applicability map's key is a tuple, which `serde_json` cannot use as
        // a JSON object key (JSON object keys must be strings) — flatten to a
        // `Vec` of entries instead; `BTreeMap` iteration is already key-sorted,
        // so this stays canonical.
        #[derive(Serialize)]
        struct View<'a> {
            cells: &'a [Cell],
            applicability: Vec<(&'a (String, String), &'a Applicability)>,
            aggregation_version: &'a AggregationVersion,
        }
        let view = View {
            cells: &cells,
            applicability: self.applicability.iter().collect(),
            aggregation_version: &self.aggregation_version,
        };
        let json = serde_json::to_vec(&view).expect("battery view serializes");
        let digest = Sha256::digest(&json);
        ManifestDigest(format!("sha256:{:x}", digest))
    }

    /// Uniformity (QS-3): every lane sees the SAME cell list; a lane's variance
    /// appears ONLY as a declared n-a (or coverage), never as a missing cell or a
    /// silently-narrower battery. Returns the (lane, cell) pairs that are wholly
    /// absent from the map (a uniformity breach) — empty means uniform.
    pub fn uniformity_check(&self) -> Result<(), Vec<String>> {
        let mut breaches = Vec::new();
        for lane in Lane::ALL {
            for cell in &self.cells {
                let key = (lane.provider_id().to_string(), cell.id.0.clone());
                if !self.applicability.contains_key(&key) {
                    breaches.push(format!(
                        "uniformity breach: lane {} has no declared applicability for cell {} — a lane's variance must be a DECLARED n-a, never an absent cell",
                        lane.provider_id(),
                        cell.id.0
                    ));
                }
            }
        }
        if breaches.is_empty() {
            Ok(())
        } else {
            Err(breaches)
        }
    }
}

/// The canonical C-1 battery: D1/D2/D3/D5/D6 cells absorbed from the existing
/// seeds at their proven strength (the pi 12-item rubric, the codex hand-copy,
/// the 3-phase + door-failure cells, the F1 harnesses — R9 C-1). D4
/// (mode-switch) and D7 (env-matrix) are deliberately absent from this list:
/// D4 is defined-but-inactive crate-wide (never counted, [`Dimension::is_active`]
/// excludes it) and carries no cells anywhere; D7 has ZERO existing seed
/// coverage today (confirmed by exhaustive search of the seed sources) — its
/// cells are new discriminating cells that exist to fail, which is C-2's
/// remit, not an absorption.
///
/// **QS-3 uniformity is structural here**: every cell id below is
/// lane-agnostic (`d1.boot-readiness`, never `d1.pi-boot-readiness`) — the
/// SAME cell list applies to every lane, per [`Battery::uniformity_check`].
/// Where a lane's underlying mechanism genuinely differs (pi's own wire
/// protocol vs the ACP `session/*` primitives), that is expressed as the
/// cell's declared applicability for that lane, never as a different cell id.
///
/// **Applicability judgment calls, recorded here (no out-of-band knowledge —
/// every reason a stranger would need is in the `NaPermitted` string itself):**
/// - `claude-code` (the bare, non-ACP lane) has **zero** existing-seed coverage
///   across every source file the extraction searched — every seed test
///   touches `pi`, `codex`, or an `acp/*` lane. The dimension genuinely
///   applies to `claude-code` (it has the same start/turn/stop lifecycle), so
///   cells are `Required` here, not `NaPermitted` — an unresolved cell is a
///   correctly-surfaced C-1 defect (open coverage work), never papered over.
/// - `acp/opencode` shares `AcpHost`/`client.rs` with `acp/claude-code` for
///   most of these mechanisms per `provider.rs`'s registration, but the
///   extraction found no seed that independently EXERCISES opencode's bridge
///   — so these are `Required`, not assumed-passing by compositional
///   inference; a shared code path is not a substitute for a shed cell
///   observation (that would be exactly QS-2's "constructed from where
///   nothing ran").
/// - ACP-protocol-specific mechanisms (`session/cancel` semantics, ACP queue
///   capacity, `session/load` resume) have no analogous primitive in pi's or
///   codex's own wire protocol — those get `NaPermitted` for `pi`/`codex`
///   with the protocol-difference reason stated, which IS a stranger-
///   acceptable n-a (a different transport, not a coverage gap).
pub fn conformance_battery() -> Battery {
    use Applicability::{NaPermitted, Required};

    let cell = |id: &str, dim: Dimension, title: &str| Cell {
        id: CellId(id.to_string()),
        dimension: dim,
        title: title.to_string(),
    };

    let cells = vec![
        // D1 — lifecycle
        cell(
            "d1.boot-readiness",
            Dimension::D1Lifecycle,
            "provider reaches ready via a real live handshake (not a fixture)",
        ),
        cell(
            "d1.launch-addressing",
            Dimension::D1Lifecycle,
            "the launch row carries real addressing fields and a fresh `qd info` resolves it",
        ),
        cell(
            "d1.transport-is-our-daemon",
            Dimension::D1Lifecycle,
            "the live process backing a session is confirmed OUR daemon, not some other process",
        ),
        cell(
            "d1.multiplex-concurrent-distinct-residents",
            Dimension::D1Lifecycle,
            "two concurrently-started sessions get distinct, independently addressable residents",
        ),
        cell(
            "d1.reconnect-resolves-via-registry",
            Dimension::D1Lifecycle,
            "a fresh, separate `qd info` process re-addresses a live session through the on-disk registry",
        ),
        cell(
            "d1.liveness-process-alive",
            Dimension::D1Lifecycle,
            "`qd ls` liveness reflects a real OS-level pid-alive check, never a cached field",
        ),
        cell(
            "d1.teardown-reaps-process-group",
            Dimension::D1Lifecycle,
            "`qd stop` reaps the resident's full process group (pgid), tombstones the row, leaves no survivors",
        ),
        cell(
            "d1.resume-same-session-id",
            Dimension::D1Lifecycle,
            "resume spawns a new process but re-establishes the SAME session id (no re-mint)",
        ),
        // D2 — receipts
        cell(
            "d2.turn-phase-sequence-strict-order",
            Dimension::D2Receipts,
            "one send→wait produces send-initiated → turn-accepted → message-seen in strict append order, correlated by one send_id",
        ),
        cell(
            "d2.delivery-log-consistency-under-home-override",
            Dimension::D2Receipts,
            "phase receipts land in exactly one delivery log even under a QD_HOME override — no orphaned log",
        ),
        cell(
            "d2.turn-correlation-and-completion",
            Dimension::D2Receipts,
            "a turn's reply correlates back to the exact injected turn id, and completion is independently confirmed (protocol + visible)",
        ),
        // D3 — busy-queue
        cell(
            "d3.queue-overflow-honors-configured-capacity",
            Dimension::D3BusyQueue,
            "a host honors its configured (non-default) outbound-queue capacity; overflow fails loud past exactly that bound",
        ),
        cell(
            "d3.queue-slot-released-no-leak",
            Dimension::D3BusyQueue,
            "an in-flight slot is released (no leak) after its turn resolves, including via client-initiated cancellation",
        ),
        // D5 — transcript
        cell(
            "d5.resume-jsonl-continuity-and-recall",
            Dimension::D5Transcript,
            "post-resume, the transcript grows the SAME session file (never forks) and the model recalls pre-stop context — grounded in transcript mtime/jsonl, never `qd info` cached fields",
        ),
        cell(
            "d5.transcript-tee-captures-assistant-text",
            Dimension::D5Transcript,
            "the transcript tee captures the model's literal reply text (Pete's-view feed fidelity)",
        ),
        // D6 — failure-honesty (seed subset; C-2 adds new injection cells)
        cell(
            "d6.cold-target-send-fails-loud-with-terminal",
            Dimension::D6FailureHonesty,
            "sending to an unreachable resident exits non-zero AND records a truthful send-failed terminal in the delivery log",
        ),
        cell(
            "d6.bridge-death-detection-no-false-positive",
            Dimension::D6FailureHonesty,
            "a liveness check never reports a transiently-busy child as dead; it reports dead only once the child has genuinely exited and been reaped",
        ),
        cell(
            "d6.self-terminate-on-wedged-child",
            Dimension::D6FailureHonesty,
            "when a required child process is killed out from under a daemon, the daemon self-terminates rather than lingering falsely live",
        ),
        cell(
            "d6.cancel-maps-to-truthful-terminal",
            Dimension::D6FailureHonesty,
            "a client-initiated cancellation resolves as a truthful cancelled terminal, never disguised as end-turn or a protocol error",
        ),
        // === C-2 additions ===
        // D6 — failure-honesty INJECTION cells (real injected failure → truthful
        // terminal + loud error). Distinct failure modes from the seed cells above.
        cell(
            "d6.dead-relay-sidecar",
            Dimension::D6FailureHonesty,
            "sending through a relay whose HTTP sidecar endpoint is dead/unreachable exits non-zero AND records a truthful send-failed terminal — never a phantom success",
        ),
        cell(
            "d6.sender-killed-mid-send",
            Dimension::D6FailureHonesty,
            "when the sender process is killed mid-send, the delivery log shows no falsely-completed/seen terminal — only a truthful incomplete/absent terminal, never a phantom success",
        ),
        cell(
            "d6.wedged-daemon",
            Dimension::D6FailureHonesty,
            "sending to a live-but-wedged (hung) target daemon fails loud within the configured timeout and records a truthful terminal — never hangs forever, never falsely succeeds",
        ),
        cell(
            "d6.partial-write",
            Dimension::D6FailureHonesty,
            "a truncated/partial delivery-log record is detected as malformed and never read back as a complete terminal — no silent acceptance of a partial write as success",
        ),
        // D6 — advertised-surface honesty sweep (doc-drift is a known live failure
        // class). Both discrimination arms live in one cell (per conf-build-coord-4):
        // honestly-disclosed-unsupported flags PASS, advertised-as-functional-but-
        // always-erroring flags FAIL.
        cell(
            "d6.advertised-surface-honesty",
            Dimension::D6FailureHonesty,
            "every flag the curated help advertises is either functional or discloses that it is unsupported — no flag is advertised as a working feature while the handler unconditionally rejects it, and no documented flag silently no-ops",
        ),
        // D7 — env-matrix: the qd attach client (qrmux) nested inside a real outer
        // mux. Required only on claude-code (the sole Hosting::MuxPane lane — the
        // one with a terminal-grid attach surface); the daemon lanes have "no
        // terminal to attach" (lifecycle.rs:68), so the property has no referent.
        // Render code is server-side + provider-invariant (VTE Screen/Grid); qd's
        // claude MuxPane attaches via the IDENTICAL attach_session path
        // (embedded_mux.rs:461) the standalone qrmux binary uses — so these are
        // driven with the real qrmux binary nested in tmux + a plain shell pane.
        cell(
            "d7.nested-render-correctness",
            Dimension::D7EnvMatrix,
            "the qrmux attach client nested inside a real outer mux (tmux/screen) renders known glyphs at known grid cells — a readable, selectable grid, asserted via capture-pane",
        ),
        cell(
            "d7.detach-chord-reaches-qrmux",
            Dimension::D7EnvMatrix,
            "the hardcoded detach chord (Ctrl+\\, 0x1c) sent through the outer mux reaches the inner qrmux client and detaches it (resident survives); a non-chord byte does NOT detach",
        ),
        cell(
            "d7.mouse-passthrough",
            Dimension::D7EnvMatrix,
            "mouse-mode enable sequences set by the inner app are replayed to a client attaching through the outer mux (mouse passthrough survives the nesting)",
        ),
        cell(
            "d7.term-terminfo-honest",
            Dimension::D7EnvMatrix,
            "the TERM the inner pane sees is truthfully the pinned xterm-256color the PTY provides — tested by reading it honestly through the nesting, not asserted",
        ),
        cell(
            "d7.altscreen-enter-exit-across-nesting",
            Dimension::D7EnvMatrix,
            "alt-screen enter/exit renders correctly across the nesting — the main grid is restored on exit, no residue from the alt buffer",
        ),
        cell(
            "d7.cross-size-reattach-both-directions",
            Dimension::D7EnvMatrix,
            "reattach at a different size (grow AND shrink), main and alt screen, with non-empty scrollback, yields a correct readable grid — the SC-1 gap; expected RED while the qrmux cross-width reflow garble is live",
        ),
        cell(
            "d7.zellij-nested-render-coverage",
            Dimension::D7EnvMatrix,
            "the zellij-nested attach render/detach/resize axis (named in the C-2 target alongside tmux/screen) — a structured, re-entrant coverage entry for the zellij outer mux",
        ),
    ];

    let mut applicability = BTreeMap::new();
    let mut put = |lane: Lane, id: &str, app: Applicability| {
        applicability.insert((lane.provider_id().to_string(), id.to_string()), app);
    };

    for lane in Lane::ALL {
        // D1 cells that apply uniformly across every lane's start/stop
        // lifecycle — no lane has a domain reason to exclude them; unresolved
        // today (claude-code bare, acp/opencode) is a visible C-1 defect, not
        // a silent narrowing.
        for id in [
            "d1.boot-readiness",
            "d1.launch-addressing",
            "d1.transport-is-our-daemon",
            "d1.multiplex-concurrent-distinct-residents",
            "d1.reconnect-resolves-via-registry",
            "d1.liveness-process-alive",
            "d1.teardown-reaps-process-group",
        ] {
            put(lane, id, Required);
        }

        // Resume is an ACP session/load primitive; codex has no analogous
        // mechanism in its own wire protocol today (a genuine protocol
        // difference, not a coverage gap — stranger-acceptable n-a).
        //
        // CORRECTION (conf-build-coord-3, 2026-07-15): pi was ALSO marked
        // NaPermitted here with the same reason ("no analogous resume/
        // session-load primitive measured by any existing seed") — that
        // reason is now factually false. `qd resume <name> --no-attach`
        // genuinely works for pi and re-establishes the SAME session id,
        // measured by a real seed: the d5.resume-jsonl-continuity-and-recall
        // pi driver (commit 13fc1ea), manually verified by hand before that
        // driver was ever written. An incorrect NaPermitted with a false
        // reason is worse than an honest undriven gap — it asserts
        // something about the system that isn't true, exactly what QS-2's
        // adversarial-reviewer standard exists to catch (just from the
        // false-negative direction rather than a fabricated green). Moved
        // pi into the `_` (Required) arm.
        match lane {
            Lane::Codex => put(
                lane,
                "d1.resume-same-session-id",
                NaPermitted {
                    reason: "codex's wire protocol has no analogous resume/session-load primitive measured by any existing seed".into(),
                },
            ),
            _ => put(lane, "d1.resume-same-session-id", Required),
        }

        // D2: the 3-phase receipt sequence and delivery-log consistency are
        // ACP-family delivery-log mechanisms; pi and codex do not route
        // through the same delivery-log store per any existing seed.
        //
        // CORRECTION (conf-build-coord-3, 2026-07-15, confirming grep on
        // record): claude-code-bare ALSO NaPermitted here — traced
        // `qd send:relay`'s dispatch (`src/bin/qd/verbs/send_relay.rs::run`):
        // it branches to `run_codex_send`/`run_acp_send`/`run_pi_send` for
        // those three providers specifically, each using
        // `emit_daemon_send_events`/`emit_daemon_seen` (the 3-phase
        // "send-initiated -> turn-accepted -> message-seen" mechanism these
        // cells measure). Everything else, including claude-code-bare,
        // falls through to `emit_relay_send_events` instead — a
        // structurally different, generic relay-message path with its own
        // "send-initiated -> relay-delivered" shape (relay-delivered is
        // non-terminal; no `message-seen`-equivalent terminal exists on
        // this path). Not a missing feature — a genuinely different
        // mechanism bare claude-code was never going to produce the
        // ACP-shaped delivery log for.
        match lane {
            Lane::Pi | Lane::Codex | Lane::ClaudeCode => {
                for id in [
                    "d2.turn-phase-sequence-strict-order",
                    "d2.delivery-log-consistency-under-home-override",
                ] {
                    put(
                        lane,
                        id,
                        NaPermitted {
                            reason: format!(
                                "{} does not route turns through the ACP-family delivery-log store any existing seed measures",
                                lane.provider_id()
                            ),
                        },
                    );
                }
            }
            _ => {
                for id in [
                    "d2.turn-phase-sequence-strict-order",
                    "d2.delivery-log-consistency-under-home-override",
                ] {
                    put(lane, id, Required);
                }
            }
        }
        // Turn-correlation-and-completion is domain-general (every lane
        // correlates a reply to its turn) — required everywhere.
        put(lane, "d2.turn-correlation-and-completion", Required);

        // D3: queue-capacity + slot-release are ACP host mechanisms
        // (`AcpHost`'s in-flight bookkeeping); pi/codex have no measured
        // analogous queue primitive.
        //
        // CORRECTION (conf-build-coord-3, 2026-07-15, confirming grep on
        // record): claude-code-bare ALSO NaPermitted — grepped every `fn
        // run_*` in `send_relay.rs`: no `run_claude_send` exists at all, so
        // claude-code-bare never calls `Provider::inject` and never touches
        // `AcpHost`/`OutboundQueue`. Its relay path (`emit_relay_send_events`)
        // is a direct message-drop with no backpressure/capacity concept
        // found anywhere in that code path.
        match lane {
            Lane::Pi | Lane::Codex | Lane::ClaudeCode => {
                for id in [
                    "d3.queue-overflow-honors-configured-capacity",
                    "d3.queue-slot-released-no-leak",
                ] {
                    put(
                        lane,
                        id,
                        NaPermitted {
                            reason: format!(
                                "{} has no measured analogous outbound-queue/in-flight primitive to the ACP host's",
                                lane.provider_id()
                            ),
                        },
                    );
                }
            }
            _ => {
                for id in [
                    "d3.queue-overflow-honors-configured-capacity",
                    "d3.queue-slot-released-no-leak",
                ] {
                    put(lane, id, Required);
                }
            }
        }

        // D5: transcript continuity/recall and the tee are domain-general —
        // every lane has a transcript, required everywhere.
        for id in [
            "d5.resume-jsonl-continuity-and-recall",
            "d5.transcript-tee-captures-assistant-text",
        ] {
            put(lane, id, Required);
        }

        // D6: cold-target send-fail is a daemon-honesty property — Required on
        // every lane whose cold-target fixture ("a live pid with no recorded
        // endpoint") is distinctive enough to trip the not-reachable door
        // (pi/codex/acp). It is NOT required on claude-code-bare: those rows
        // NEVER carry an endpoint even when perfectly healthy (registry field
        // doc: "claude rows NEVER carry it"), so the fixture's
        // live-pid-no-endpoint shape is indistinguishable from a normal healthy
        // row on this lane — the fixture cannot distinguish cold from healthy,
        // so there is no meaningful mechanism to drive (harness.rs run_cell
        // correspondingly has no ClaudeCode arm for this cell; the driver is
        // deliberately absent, not merely unwritten). Reclassified NaPermitted
        // for ClaudeCode, 2026-07-15 (conf-build-coord-3, run-proof-oracle
        // cross-seam finding), same class as the self-terminate-on-wedged-child
        // and bridge-death corrections below — a Required-but-permanently-
        // unwireable cell would ship an honest gap as a silent narrowing, so
        // the honest form is a DECLARED n-a with the mechanism reason stated.
        match lane {
            Lane::ClaudeCode => put(
                lane,
                "d6.cold-target-send-fails-loud-with-terminal",
                NaPermitted {
                    reason: "claude-code-bare rows never carry an endpoint field even when healthy, so the cold-target fixture's mechanism (a live pid with no recorded endpoint) is indistinguishable from a normal healthy row on this lane — it cannot distinguish a cold resident from a live one, so there is no meaningful mechanism to drive here".to_string(),
                },
            ),
            _ => put(lane, "d6.cold-target-send-fails-loud-with-terminal", Required),
        }

        // Self-terminate-on-wedged-child, cancel-maps-to-truthful-terminal, and
        // bridge-death-detection are ACP session/cancel + bridge-child
        // primitives with no pi/codex analog per any existing seed.

        // self-terminate-on-wedged-child. REVISED 2026-07-15 (conf-build-coord-3
        // ruling, after the run-proof/pre-land oracles found the prior blanket
        // reason false). The old reason ("no wire::serve-equivalent loop —
        // grepped `fn serve` only in acp/wire.rs") rested on a NAIVE grep that
        // missed `serve_pi` (provider/pi/residence.rs:552), a real dispatch-owned
        // resident serve loop. The three non-ACP lanes are NOT alike here — they
        // split three ways on WHO owns the resident:
        //   - pi: dispatch OWNS the resident (`qd pi-daemon` runs `serve_pi`, owns
        //     a `pi --mode rpc` child; the recorded pid is the RESIDENT's, not the
        //     child's). serve_pi has NO child-confirmed-dead guard (it swallows
        //     get_state errors), and `qd info` derives `live` from the resident
        //     pid, so a dead child lingers FALSELY-LIVE. The property genuinely
        //     APPLIES and is genuinely VIOLATED → Required, driven live, reports
        //     FAIL (live-probed 2026-07-15). A red cell demotes pi's tier, does
        //     not block. (The underlying qd product defect is escalated
        //     separately; out of C-1 scope.)
        //   - codex: NOT dispatch-owned — the codex `app-server` is a
        //     provider-owned, self-listening detached process (launch_plan =
        //     [codex, app-server] + --listen; qd is a mere ws client, and the
        //     recorded pid IS the app-server itself). No qd-owned daemon-with-
        //     killable-child to self-terminate; app-server death is honestly
        //     not-live. NaPermitted, TRUE reason cited.
        //   - claude-code-bare: Hosting::MuxPane — the claude CLI is itself the
        //     single tracked leaf process in a qd-owned mux pane; there is no qd
        //     daemon owning a separate child, and liveness keys on the claude
        //     process's own pid. "Self-terminate on wedged child" has no referent.
        //     NaPermitted, TRUE reason cited.
        match lane {
            Lane::Pi => put(lane, "d6.self-terminate-on-wedged-child", Required),
            Lane::Codex => put(
                lane,
                "d6.self-terminate-on-wedged-child",
                NaPermitted {
                    reason: "codex is not qd-hosted as a serve loop: the codex app-server is a provider-owned, self-listening detached process (launch_plan = [codex, app-server] + --listen) and qd is only a ws client whose recorded pid IS the app-server itself — there is no qd-owned daemon-with-killable-child to self-terminate, and app-server death is honestly not-live".to_string(),
                },
            ),
            Lane::ClaudeCode => put(
                lane,
                "d6.self-terminate-on-wedged-child",
                NaPermitted {
                    reason: "claude-code (bare) is Hosting::MuxPane — the claude CLI is itself the single tracked leaf process in a qd-owned mux pane, with no qd daemon owning a separate child, so 'self-terminate on wedged child' has no referent and liveness keyed on the claude process's own pid is honest".to_string(),
                },
            ),
            _ => put(lane, "d6.self-terminate-on-wedged-child", Required),
        }

        // CORRECTION (conf-build-coord-3, 2026-07-15, confirming grep on
        // record): claude-code-bare ALSO NaPermitted for both — no
        // `AcpHost`, no `acp_client` usage anywhere in claude-code-bare's
        // send/commission path (same absence already grounding
        // self-terminate-on-wedged-child's correction above) — no bridge
        // child process to detect death of, no session/cancel RPC to map.
        match lane {
            Lane::Pi | Lane::Codex | Lane::ClaudeCode => {
                put(
                    lane,
                    "d6.bridge-death-detection-no-false-positive",
                    NaPermitted {
                        reason: format!(
                            "{} has no ACP bridge child process for this liveness primitive to apply to",
                            lane.provider_id()
                        ),
                    },
                );
                put(
                    lane,
                    "d6.cancel-maps-to-truthful-terminal",
                    NaPermitted {
                        reason: format!(
                            "{} has no measured ACP session/cancel-equivalent primitive",
                            lane.provider_id()
                        ),
                    },
                );
            }
            _ => {
                put(lane, "d6.bridge-death-detection-no-false-positive", Required);
                put(lane, "d6.cancel-maps-to-truthful-terminal", Required);
            }
        }

        // === C-2 applicability ===

        // D6 advertised-surface honesty: a property of the qd CLI surface every
        // lane drives (each lane exercises its lane-relevant documented flags;
        // `start --attach` is a general start flag advertised for every lane, so
        // the drift arm is uniform). Required everywhere.
        put(lane, "d6.advertised-surface-honesty", Required);

        // D6 injection cells — per-lane routing grounded in the send:relay path
        // trace (send_relay.rs run_with_client).
        //
        // dead-relay-sidecar: ONLY claude-code routes through the CcRelay HTTP
        // sidecar POST (send_relay.rs:135/542 → relay_http.rs). The daemon lanes
        // route to their ws/ACP inject ladders BEFORE the relay-port-None check
        // (send_relay.rs:72/86/98) and carry no relay port, so a dead relay
        // sidecar has no referent for them.
        match lane {
            Lane::ClaudeCode => put(lane, "d6.dead-relay-sidecar", Required),
            _ => put(
                lane,
                "d6.dead-relay-sidecar",
                NaPermitted {
                    reason: format!(
                        "{} routes send:relay to its daemon ws/ACP inject ladder before the relay-port check and carries no relay port — it never opens a CcRelay HTTP sidecar POST, so the dead-relay-sidecar transport door has no referent on this lane",
                        lane.provider_id()
                    ),
                },
            ),
        }

        // sender-killed-mid-send + wedged-daemon: daemon-inject-path honesty
        // properties (a delivery-log send-failed door / no sender-written success
        // terminal). The daemon lanes (codex/acp/pi) have the daemon inject path;
        // claude-code-bare routes through the relay POST — stderr-only failure,
        // no delivery-log door — so that transport-door gap is instead covered by
        // d6.dead-relay-sidecar, and these two have no daemon-path referent here.
        match lane {
            Lane::ClaudeCode => {
                put(
                    lane,
                    "d6.sender-killed-mid-send",
                    NaPermitted {
                        reason: "claude-code-bare's send:relay writes no success terminal on the relay path (only the non-terminal send-initiated/relay-delivered, and only after the POST returns a message_id — emit_relay_send_events, send_relay.rs:155), so there is no sender-written success terminal a mid-send kill could falsely produce; the phantom-terminal property has no positive referent on this lane".to_string(),
                    },
                );
                put(
                    lane,
                    "d6.wedged-daemon",
                    NaPermitted {
                        reason: "the wedged-daemon defense (fixed 5s connect + 30s request timeout on the ws/ACP inject → loud exit + a send-failed delivery-log door) is a daemon-inject-path property; claude-code-bare has no daemon inject path — it routes through the relay POST, bounded by a transport read-timeout with a stderr-only failure and no delivery-log door (that transport-door gap is covered by d6.dead-relay-sidecar)".to_string(),
                    },
                );
            }
            _ => {
                put(lane, "d6.sender-killed-mid-send", Required);
                put(lane, "d6.wedged-daemon", Required);
            }
        }

        // partial-write: transport-agnostic — every lane writes and reads the same
        // <uuid>.events.jsonl through EventWriter / parse_events (events.rs); the
        // torn-tail / malformed-line rejection is shared. Required everywhere.
        put(lane, "d6.partial-write", Required);

        // D7 env-matrix (all 7): Required ONLY on claude-code (bare) — the sole
        // Hosting::MuxPane lane (provider.rs:60-64), the one with a qd-owned mux
        // pane and thus a terminal-grid attach surface. Every other lane is
        // Hosting::Daemon: a provider-owned daemon thread with "no terminal to
        // attach" (lifecycle.rs:68 says so outright for codex; pi/acp report the
        // same Hosting::Daemon), so the attach-client env-matrix / nested-render
        // property has NO referent there — a stranger-acceptable NaPermitted, not
        // a coverage gap. (Whether a daemon lane exhibits render coverage through
        // some OTHER surface — e.g. `qd live` tailing — is preregistered for the
        // per-cell why-holder; if found, this reason is revised, not papered over.)
        let d7_cells = [
            "d7.nested-render-correctness",
            "d7.detach-chord-reaches-qrmux",
            "d7.mouse-passthrough",
            "d7.term-terminfo-honest",
            "d7.altscreen-enter-exit-across-nesting",
            "d7.cross-size-reattach-both-directions",
            "d7.zellij-nested-render-coverage",
        ];
        match lane {
            Lane::ClaudeCode => {
                for id in d7_cells {
                    put(lane, id, Required);
                }
            }
            _ => {
                for id in d7_cells {
                    put(
                        lane,
                        id,
                        NaPermitted {
                            reason: format!(
                                "{} is Hosting::Daemon — a provider-owned daemon thread with no qd-owned mux pane and no terminal to attach (lifecycle.rs:68), so the qrmux attach-client env-matrix / nested-render property has no referent on this lane",
                                lane.provider_id()
                            ),
                        },
                    );
                }
            }
        }
    }

    Battery {
        cells,
        applicability,
        aggregation_version: AggregationVersion("agg-v1".into()),
    }
}

#[cfg(test)]
mod battery_tests {
    use super::*;

    #[test]
    fn conformance_battery_is_uniform_across_every_lane() {
        conformance_battery().uniformity_check().unwrap();
    }

    #[test]
    fn conformance_battery_manifest_digest_is_deterministic() {
        let a = conformance_battery().manifest_digest();
        let b = conformance_battery().manifest_digest();
        assert_eq!(a, b);
    }

    #[test]
    fn every_na_permitted_carries_a_nonempty_stranger_acceptable_reason() {
        for (key, app) in &conformance_battery().applicability {
            if let Applicability::NaPermitted { reason } = app {
                assert!(
                    !reason.trim().is_empty(),
                    "empty n-a reason for {key:?} — QS-3 requires a declared, stranger-acceptable reason"
                );
            }
        }
    }

    #[test]
    fn no_dimension_is_wholly_missing_from_the_battery() {
        // Every active dimension the plan names (D1/D2/D3/D5/D6) has at least
        // one cell — a silently-absent dimension would be a bigger uniformity
        // breach than a single missing cell.
        let battery = conformance_battery();
        for dim in [
            Dimension::D1Lifecycle,
            Dimension::D2Receipts,
            Dimension::D3BusyQueue,
            Dimension::D5Transcript,
            Dimension::D6FailureHonesty,
        ] {
            assert!(
                battery.cells.iter().any(|c| c.dimension == dim),
                "dimension {dim:?} has zero cells in the canonical battery"
            );
        }
    }
}
