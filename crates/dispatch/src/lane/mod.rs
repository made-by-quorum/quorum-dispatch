//! The qd-side seam onto **qw**.
//!
//! The contract, its DTOs, the fixture and the conformance suite all live in the
//! `quorum-qw` crate now — this module is only what must stay on qd's side of the
//! boundary: the cross-checks that need to see BOTH packages at once, which
//! `quorum-qw` cannot host because it must not depend on `dispatch`.
//!
//! The seven `LaneOps` implementations moved into `quorum-qw` too
//! (`quorum_qw::lanes`), as their delegation targets did. NO SEAM IS LEFT ON THIS
//! SIDE: stage-3 phase 3B moved all five carrier bodies into
//! `quorum_qw::delivery`, `deliver` calls them directly, and `quorum_qw::Carriers`
//! / `RealUnifiedBackend` / `lane_ops_with_carriers` are deleted. No `qw` code
//! calls into the `qd` binary.

use quorum_qw::contract::LaneOps;
use quorum_qw::effects::Env;
use quorum_qw::lane::Lane;
use quorum_qw::paths::QdPaths;

/// The env var that forces the IN-PROCESS lane instead of the `qw` subprocess.
///
/// An escape hatch, not a mode. It exists because the wire's failure mode is a
/// missing sibling binary, and a developer bisecting an unrelated bug should be
/// able to take the process boundary out of the picture in one variable rather
/// than by editing code. Production leaves it unset.
pub const INPROCESS_ENV: &str = "QD_QW_INPROCESS";

/// **The one place `qd` obtains a lane.**
///
/// Every verb that drives a session goes through here, and that is the point:
/// with one constructor, "does qd talk to qw in-process or over a wire" is one
/// decision in one place rather than thirteen call sites that could drift apart.
/// The `provider_gate`-style source scan that pins it is
/// [`crate::lane::gate`].
///
/// # Why the default is the subprocess
///
/// The split is only real if the ordinary path takes it. A default of
/// in-process with an opt-in wire would mean the boundary is exercised only by
/// whoever remembers to opt in, and every drift across it would be found by a
/// user rather than by the suite. So the wire is the default and the in-process
/// lane is the escape hatch — [`INPROCESS_ENV`].
///
/// # What a missing `qw` does
///
/// It fails loudly, naming the path it looked for. It does **not** silently fall
/// back to the in-process lane: a fallback would mean a machine with a missing or
/// half-installed `qw` runs a *different architecture* from the one that was
/// tested, and nothing would say so. That is the same ruling
/// `tests/common/p0bins.rs`'s `qrmux_bin()` already makes for its sibling binary
/// ("PANICS with a build hint if absent — never a silent skip").
pub fn open<'a>(lane: Lane, env: &'a dyn Env, paths: QdPaths) -> Box<dyn LaneOps + 'a> {
    if env.var(INPROCESS_ENV).is_some_and(|v| v == "1") {
        return Box::new(quorum_qw::lanes::lane_ops(lane, env, paths));
    }
    match quorum_qw::wire::client::WireLane::new(lane) {
        Ok(w) => Box::new(w),
        // Resolution failed before a process could even be spawned (no
        // `current_exe`). Report it through the lane's own error channel on the
        // first call rather than panicking here — every caller already renders
        // `LaneError`, and a panic would replace a verb's own wording with a
        // backtrace.
        Err(e) => Box::new(UnavailableLane { error: e }),
    }
}

/// A lane that answers every call with the reason it could not be reached.
///
/// Exists so [`open`] can be infallible at the call site. A `Result` there would
/// put a `?` in thirteen verbs for a case none of them can do anything about
/// except print it — which is precisely what this does, through the channel they
/// already handle.
struct UnavailableLane {
    error: quorum_qw::contract::LaneError,
}

impl LaneOps for UnavailableLane {
    fn start(
        &self,
        _req: &quorum_qw::contract::StartRequest,
    ) -> Result<quorum_qw::contract::SessionHandle, quorum_qw::contract::LaneError> {
        Err(self.error.clone())
    }
    fn wake(
        &self,
        _id: &quorum_qw::contract::SessionId,
        _render: quorum_qw::launch::RenderMode,
        _cwd_override: Option<String>,
    ) -> Result<quorum_qw::contract::WakeOutcome, quorum_qw::contract::LaneError> {
        Err(self.error.clone())
    }
    fn kill(
        &self,
        _id: &quorum_qw::contract::SessionId,
    ) -> Result<quorum_qw::contract::KillReport, quorum_qw::contract::LaneError> {
        Err(self.error.clone())
    }
    fn list(&self) -> Result<quorum_qw::contract::Listing, quorum_qw::contract::LaneError> {
        Err(self.error.clone())
    }
    fn health(
        &self,
        _id: &quorum_qw::contract::SessionId,
    ) -> Result<quorum_qw::contract::Health, quorum_qw::contract::LaneError> {
        Err(self.error.clone())
    }
    fn receive_path(
        &self,
        _id: &quorum_qw::contract::SessionId,
    ) -> Result<quorum_qw::contract::ReceivePath, quorum_qw::contract::LaneError> {
        Err(self.error.clone())
    }
    fn deliver(
        &self,
        _id: &quorum_qw::contract::SessionId,
        _msg: &quorum_qw::contract::Message,
        _policy: &quorum_qw::contract::DeliverPolicy,
    ) -> Result<quorum_qw::contract::Receipt, quorum_qw::contract::LaneError> {
        Err(self.error.clone())
    }
    fn await_terminal(
        &self,
        _id: &quorum_qw::contract::SessionId,
        _message_id: &quorum_qw::contract::MessageId,
        _budget_ms: u64,
    ) -> Result<quorum_qw::contract::Terminal, quorum_qw::contract::LaneError> {
        Err(self.error.clone())
    }
    fn recover(
        &self,
        _at: &quorum_qw::contract::LedgerAddress,
        _message_id: &quorum_qw::contract::MessageId,
    ) -> Result<quorum_qw::contract::Terminal, quorum_qw::contract::LaneError> {
        Err(self.error.clone())
    }
    fn await_idle(
        &self,
        _id: &quorum_qw::contract::SessionId,
        _budget_ms: u64,
    ) -> Result<quorum_qw::contract::TurnState, quorum_qw::contract::LaneError> {
        Err(self.error.clone())
    }
    fn resolved(
        &self,
        _at: &quorum_qw::contract::LedgerAddress,
        _message_id: &quorum_qw::contract::MessageId,
    ) -> Result<Option<quorum_qw::contract::Terminal>, quorum_qw::contract::LaneError> {
        Err(self.error.clone())
    }
    fn attach(
        &self,
        _id: &quorum_qw::contract::SessionId,
    ) -> Result<i32, quorum_qw::contract::LaneError> {
        Err(self.error.clone())
    }
}

#[cfg(test)]
mod gate {
    //! **The stage-4 gate: no verb constructs a lane.**
    //!
    //! `11-stage3-plan.md` ruling D7. The property the split actually cares about
    //! is behavioural, not a dependency edge: qd still LINKS `quorum-qw` for
    //! twenty-five re-exported modules, and retiring that is the `join.rs` split
    //! plus the registry — a separate project. What must hold now is that every
    //! lane operation a verb performs goes over the wire.
    //!
    //! So this scans the verb layer for `lane_ops(`, the in-process constructor.
    //! Exactly one production site may name it: [`super::open`], which is the
    //! chokepoint the escape hatch lives in.
    //!
    //! A source scan rather than a type-level ban because `quorum_qw::lanes` is
    //! and must stay public — `qw`'s own binary calls it, and so does the escape
    //! hatch. There is no visibility that admits those two and excludes the verbs.
    //! The `provider_gate` precedent is the same shape and exists for the same
    //! reason.

    use std::path::{Path, PathBuf};

    /// The in-process constructor. A verb naming this has bypassed the boundary.
    const IN_PROCESS_CTOR: &str = "lane_ops(";

    /// The ONE production file allowed to name it: this module.
    const CHOKEPOINT: &str = "lane/mod.rs";

    pub(super) fn src_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
    }

    pub(super) fn rust_sources(root: &Path) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("src/ is readable") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let rel = path
                        .strip_prefix(root)
                        .expect("under root")
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((rel, path));
                }
            }
        }
        out.sort();
        out
    }

    /// Text before the file's first `#[cfg(test)]`.
    ///
    /// Deliberately cruder than `provider_gate`'s brace-matched region finder, and
    /// safe in the opposite direction: cutting at the FIRST marker can only make
    /// this gate scan LESS than the whole file, never more, so it cannot produce a
    /// false failure. A test-module construction of a real lane is legitimate —
    /// several unit tests drive `LaneImpl` directly on purpose.
    pub(super) fn production_half(src: &str) -> &str {
        match src.find("#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        }
    }

    #[test]
    fn no_verb_constructs_an_in_process_lane() {
        let root = src_root();
        let mut offenders: Vec<String> = Vec::new();

        for (rel, path) in rust_sources(&root) {
            if rel == CHOKEPOINT {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("source is readable UTF-8");
            for (n, line) in production_half(&src).lines().enumerate() {
                // Skip line comments, including the tombstones that NAME the
                // retired constructor in prose.
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains(IN_PROCESS_CTOR) {
                    offenders.push(format!("{rel}:{}: {}", n + 1, line.trim()));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "these production sites construct an IN-PROCESS lane, bypassing the qd/qw \
             process boundary:\n  {}\n\nUse `dispatch::lane::open(lane, env, paths)` \
             instead — it returns a `WireLane` that talks to the `qw` binary, and it is \
             the one place the in-process escape hatch ({}) is honoured.",
            offenders.join("\n  "),
            super::INPROCESS_ENV
        );
    }

    /// The chokepoint must actually BE one — if `open` stopped naming the
    /// in-process constructor, the escape hatch would be silently dead and the
    /// test above would pass vacuously for the wrong reason.
    #[test]
    fn the_chokepoint_still_holds_the_escape_hatch() {
        let src = std::fs::read_to_string(src_root().join(CHOKEPOINT))
            .expect("this module is readable");
        let prod = production_half(&src);
        assert!(
            prod.contains(IN_PROCESS_CTOR),
            "lane/mod.rs no longer constructs an in-process lane — either the escape \
             hatch was removed (then delete it from the gate too, deliberately) or it \
             moved, in which case the gate is now pointing at the wrong file"
        );
        assert!(
            prod.contains("WireLane::new"),
            "lane/mod.rs must construct a WireLane — without it `open` is not a boundary"
        );
    }
}

#[cfg(test)]
mod create_gate {
    //! **The gate that stops [`super::gate`] passing vacuously for `qd start`.**
    //!
    //! `gate` asks "does a verb construct an in-process lane?" and scans for
    //! `lane_ops(`. For twelve of the thirteen lane operations that is the whole
    //! question. For the thirteenth it was not, and the miss is worth stating
    //! exactly, because it is a shape that will recur:
    //!
    //! > **`qd start` never constructed a lane, so it never tripped the scan — and
    //! > it never called `LaneOps::start` either.** It called
    //! > `quorum_qw::create::run_new` and four sibling cores DIRECTLY, as library
    //! > functions. `LaneOps::start` had zero callers in the entire `qd` binary,
    //! > `StartRequest` appeared only in `UnavailableLane`'s stub, and `lane::gate`
    //! > was green throughout.
    //!
    //! A scan for the wrong constructor cannot see a call that constructs nothing.
    //! So this one scans for the CORES instead: the qw functions that create or
    //! revive a session. A verb that reaches one of them has performed a lane
    //! operation without a lane, whatever it did or did not construct on the way.
    //!
    //! Two lists, because there are two different properties:
    //!
    //! 1. [`SESSION_CORES`] — the create/revive entry points. **Zero** in the verb
    //!    layer, and the one exemption is written down with its ruling.
    //! 2. [`SESSION_MODULES`] — the qw modules those cores live in, counted per
    //!    file. This is the residual "the verb layer still NAMES session management
    //!    at all" debt: an exact number per file, each with a reason, and a total
    //!    that may only go down. The `ledger_gate` / `provider_gate` model.
    //!
    //! List 2 is what catches the smuggling route list 1 cannot: an `import` of a
    //! core under a new alias, or a core this file has not heard of yet, still
    //! makes its module's count go up.

    use super::gate::{production_half, rust_sources, src_root};

    /// The verb layer. Everything under it is CLI code; nothing under it may
    /// create or revive a session by itself.
    const VERB_LAYER: &str = "bin/qd/";

    /// **The qw cores that create or revive a session.** A verb naming one of
    /// these has done a lane operation without a lane.
    ///
    /// Call-shaped (`name(`) so a type import cannot trip them — the module counts
    /// below are what covers imports. The set is the seven create arms' cores plus
    /// the revive cores `LaneOps::wake` drives, which is the same population by
    /// construction: `quorum_qw::lanes` is the only thing that may call them.
    const SESSION_CORES: &[&str] = &[
        // create
        "create_run_new(",
        "run_new_daemon(",
        "create_codex_tui(",
        "create_pi_tui(",
        "plan_pi_tui(",
        "create_pi_session(",
        "create_acp_daemon(",
        // revive
        "revive_codex_tui(",
        "revive_pi_tui(",
        "resume_codex(",
        "resume_pi(",
        "resume_acp(",
        // the sixth delivery body, exempted below
        "prime_new_session(",
    ];

    /// **BY OWNERSHIP.** A verb site that names a core and is allowed to, with the
    /// ruling that allows it. `(rel path, needle, why)`.
    ///
    /// One entry. Adding a second is a decision, not a fix.
    const CORE_EXEMPT: &[(&str, &str, &str)] = &[(
        "bin/qd/verbs/lifecycle.rs",
        "prime_new_session(",
        "qd start -p's PRIMING SEND. `quorum_qw::delivery::priming`'s header records \
         why this one body could be neither a `LaneOps::start` nor a `LaneOps::deliver` \
         call, and the ruling stands: `start` returns at boot-ready and the -p turn is a \
         POST-boot delivery that runs after qd's own bind phase, and `deliver` would pick \
         a carrier (relay over pane), stamp the wrong verb, use the wrong budget and mint \
         terminals on arms this path deliberately leaves open. It moved by OWNERSHIP \
         instead — a core in qw, a printing wrapper here — which is what makes it a \
         wrapper call rather than a create.",
    )];

    /// **The qw session-management modules**, as they are spelled from the verb
    /// layer. Counted rather than banned, because two of them are still legitimately
    /// named for TYPES and one for the `-p` wrapper.
    const SESSION_MODULES: &[&str] = &[
        "create::",
        "create_daemon::",
        "codex::pane::",
        "pi::pane::",
        "pi::daemon::",
        "acp::daemon::",
        "priming::",
    ];

    /// **The pin.** Exact count per verb-layer file, and why each one is there.
    ///
    /// A number that goes UP is a verb reaching back into session management. A
    /// number that goes DOWN is progress and should be written in here. `qd start`
    /// itself came from 14 to these 7 — five per-lane create wrappers, two pane
    /// adapters and an `OwnedPaneDeps` bundle deleted — when the create became one
    /// `LaneOps::start` call.
    const MODULE_PINS: &[(&str, usize, &str)] = &[
        (
            "bin/qd/verbs/lifecycle.rs",
            7,
            "4x `priming::` — the -p wrapper (see CORE_EXEMPT); 2x `codex::pane::` and \
             1x `pi::pane::` — TYPE and helper names only: `CodexTuiError`/`PiTuiError` \
             for `codex_tui_failure_line`/`pi_tui_failure_line`, which `verbs/resume.rs` \
             calls, and `viewer_pane_name` for the codex viewer (ruling J keeps the \
             viewer in qd: it owns no row and is not a session).",
        ),
        (
            "bin/qd/verbs/ls.rs",
            1,
            "1x `create_daemon::` — `real_cmdline_probe`, a /proc cmdline read. It \
             creates nothing; `ls` uses it to decide whether a recorded pid is still the \
             daemon it was.",
        ),
        (
            "bin/qd/verbs/resume.rs",
            2,
            "1x `codex::pane::` + 1x `pi::pane::` — `revive_preconditions`, the two \
             refusals that must fire AHEAD of the lane call (the ORDER pin the deleted \
             `revive_pi_tui` wrapper's doc named). Pure predicates: they resolve nothing \
             and revive nothing.",
        ),
    ];

    /// The total, so a NEW file cannot quietly open a second front. May only go
    /// down.
    const MODULE_TOTAL: usize = 10;

    fn counts() -> Vec<(String, usize, Vec<String>)> {
        let root = src_root();
        let mut out = Vec::new();
        for (rel, path) in rust_sources(&root) {
            if !rel.starts_with(VERB_LAYER) {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("source is readable UTF-8");
            let mut n = 0usize;
            let mut cores: Vec<String> = Vec::new();
            for (i, line) in production_half(&src).lines().enumerate() {
                // Comments are prose, including the tombstones that NAME every
                // core this change deleted. Same rule as `super::gate`.
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if SESSION_MODULES.iter().any(|m| line.contains(m)) {
                    n += 1;
                }
                for c in SESSION_CORES {
                    if line.contains(c)
                        && !CORE_EXEMPT
                            .iter()
                            .any(|(f, needle, _)| *f == rel && needle == c)
                    {
                        cores.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                    }
                }
            }
            if n > 0 || !cores.is_empty() {
                out.push((rel, n, cores));
            }
        }
        out
    }

    /// **No verb creates or revives a session by itself.**
    #[test]
    fn no_verb_calls_a_session_core_directly() {
        let offenders: Vec<String> = counts().into_iter().flat_map(|(_, _, c)| c).collect();
        assert!(
            offenders.is_empty(),
            "these verb-layer sites call a qw session CORE directly, which is a lane \
             operation performed without a lane:\n  {}\n\nRoute it through \
             `LaneOps` — `dispatch::lane::open(..)`, now the ONE constructor for \
             every verb INCLUDING `qd start` (ruling D6 landed and \
             `open_for_create` is gone). If the call genuinely cannot go through \
             the trait, add it to CORE_EXEMPT **with the ruling that says so**, \
             the way the `-p` priming send is.",
            offenders.join("\n  ")
        );
    }

    /// **The residual module debt is exactly what is written down.**
    #[test]
    fn the_verb_layers_session_module_debt_is_pinned() {
        let measured = counts();
        let mut total = 0usize;
        let mut wrong: Vec<String> = Vec::new();
        for (rel, n, _) in &measured {
            total += n;
            match MODULE_PINS.iter().find(|(f, ..)| f == rel) {
                Some((_, pinned, _)) if pinned == n => {}
                Some((_, pinned, why)) => wrong.push(format!(
                    "{rel}: pinned {pinned}, measured {n}\n      pin reason: {why}"
                )),
                None => wrong.push(format!(
                    "{rel}: NOT PINNED, measured {n} — a verb file that names qw session \
                     management and is not in MODULE_PINS"
                )),
            }
        }
        for (rel, pinned, _) in MODULE_PINS {
            if !measured.iter().any(|(r, ..)| r == rel) {
                wrong.push(format!(
                    "{rel}: pinned {pinned}, measured 0 — the debt is GONE; delete its pin"
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "the verb layer's qw session-management surface moved:\n  {}\n\nEvery count \
             here is a fact with a written reason. A count that went UP is a verb \
             reaching back into session management — route it through `LaneOps` \
             instead. A count that went DOWN is progress: update the pin and say what \
             retired.",
            wrong.join("\n  ")
        );
        assert!(
            total <= MODULE_TOTAL,
            "MODULE_TOTAL is {MODULE_TOTAL} and the verb layer now names qw session \
             management {total} times. This number may only go DOWN."
        );
        assert_eq!(
            total, MODULE_TOTAL,
            "the total dropped to {total} — good. Lower MODULE_TOTAL to match, so the \
             ratchet keeps holding."
        );
    }

    /// **`qd start` reaches qw through the trait.**
    ///
    /// The positive half, and the one the two scans above cannot state: they can
    /// only prove the verb does not do it ITSELF. A `run_new` that created nothing
    /// at all would satisfy both.
    #[test]
    fn qd_start_creates_through_lane_ops() {
        let src = std::fs::read_to_string(src_root().join("bin/qd/verbs/lifecycle.rs"))
            .expect("the start verb is readable");
        let prod = production_half(&src);
        assert!(
            prod.contains("lane::open(lane, &env, paths.clone())"),
            "qd start must obtain its lane from `dispatch::lane::open` — the SAME \
             constructor as every other verb. It had a seam of its own \
             (`open_for_create`, which returned the in-process lane) only while \
             ruling D6 was outstanding; now that `qw` carries `qrmux-server`, \
             `acp-daemon` and `pi-daemon`, a create executes inside `qw` and there \
             is nothing left for a second seam to decide."
        );
        assert!(
            prod.contains("ops.start(&req)"),
            "qd start must CREATE by calling `LaneOps::start`. Without this the two \
             scans above pass for a verb that creates nothing."
        );
        assert!(
            prod.contains("quorum_qw::lane::Lane::for_create("),
            "the create's lane must come from `Lane::for_create` — the routing table \
             that replaced the five-arm ordered if-chain, whose ordering was enforced \
             only by a comment and two of whose mis-swaps were SILENT"
        );
        assert!(
            prod.contains("quorum_qw::lanes::create_prompt_refusal(lane)"),
            "whether a lane takes a create-time prompt must be asked of qw, not \
             re-listed here — a second copy of that table drifts into either a dropped \
             prompt or a refused create"
        );
    }

    /// There is exactly ONE in-process lane left, and it is the developer escape
    /// hatch. Pinned so a second cannot reappear quietly.
    ///
    /// This test used to pin TWO and require the survivor to name ruling D6 — the
    /// create seam, `open_for_create`, which returned the in-process lane because
    /// six of the seven lanes' creates re-exec `current_exe()` into a resident verb
    /// the `qw` binary did not carry. D6 has landed: `qw` dispatches `qrmux-server`,
    /// `acp-daemon` and `pi-daemon`, `open_for_create` is deleted, and `qd start`
    /// takes [`open`] like every other verb. The count going 2 → 1 IS the ratchet,
    /// and it may only go down.
    #[test]
    fn the_escape_hatch_is_the_only_in_process_lane() {
        let src = std::fs::read_to_string(src_root().join("lane/mod.rs"))
            .expect("this module is readable");
        let prod = production_half(&src);
        assert_eq!(
            prod.matches("quorum_qw::lanes::lane_ops(").count(),
            1,
            "exactly ONE production site may build an in-process lane: `open`'s \
             QD_QW_INPROCESS escape hatch. A second is a new bypass and needs its \
             own ruling — the last one that existed, `open_for_create`, needed \
             ruling D6 to justify it and was deleted the day D6 landed."
        );
        assert!(
            !prod.contains("open_for_create"),
            "`open_for_create` is deleted (D6 landed). Reintroducing it is a new \
             create-only bypass of the wire and needs its own ruling, not this one."
        );
    }
}

#[cfg(test)]
mod registry_agreement_tests {
    //! The cross-check that replaces today's comment-only sync between the lane
    //! list and the provider registry (`conformance/ids.rs:42` cites
    //! `provider.rs:368-388` in prose, with nothing enforcing it).
    //!
    //! This test cannot live in `quorum-lane` — that crate must not depend on
    //! `dispatch`. It lives here, where both are visible.

    use quorum_qw::lane::{Harness, Lane, Mode};

    use crate::provider::{provider_for, row_hosting, Hosting};

    #[test]
    fn every_harness_resolves_in_the_provider_registry() {
        for h in Harness::ALL {
            assert!(
                provider_for(h.provider_id()).is_some(),
                "{:?} claims provider id {:?}, which provider_for does not resolve",
                h,
                h.provider_id()
            );
        }
    }

    #[test]
    fn every_registered_provider_id_maps_to_a_harness() {
        // The registry's own list, from provider.rs:456-478.
        for id in [
            "claude-code",
            "codex",
            "acp/claude-code",
            "opencode",
            "acp/opencode",
            "pi",
        ] {
            assert!(
                Harness::from_provider_id(id).is_some(),
                "provider_for resolves {id:?} but no Harness claims it"
            );
        }
    }

    #[test]
    fn unknown_ids_agree_in_both_directions() {
        assert!(provider_for("fixture-daemon").is_none());
        assert!(Harness::from_provider_id("fixture-daemon").is_none());
    }

    /// `lane_for` must be a drop-in for the eleven duplicated
    /// `row_hosting(&session.provider, session.hosting.as_deref())` expressions.
    ///
    /// It agrees everywhere EXCEPT two cases where it is deliberately STRICTER,
    /// both of which are latent robustness gaps in `row_hosting` rather than
    /// differences of opinion. `row_hosting` returns the row's token
    /// unconditionally and only consults the provider when the token is absent or
    /// unparseable, so it will happily hand back:
    ///
    ///   1. a topology for a provider that does not exist — defeating its own
    ///      doc's stated intent ("an UNKNOWN provider id → None, so the caller
    ///      keeps its own unknown-provider refusal rather than being handed a
    ///      made-up topology", `provider.rs:121-125`), which only holds when the
    ///      row has no hosting field; and
    ///   2. a topology the harness cannot have — a daemon-hosted claude or a
    ///      pane-hosted ACP bridge, neither of which has an implementation
    ///      behind it.
    #[test]
    fn lane_for_agrees_with_row_hosting_except_where_it_is_deliberately_stricter() {
        let providers = [
            "claude-code",
            "codex",
            "pi",
            "acp/claude-code",
            "acp/opencode",
            "opencode",
            "nonsense",
            "",
        ];
        let tokens = [
            None,
            Some("mux-pane"),
            Some("daemon"),
            // The two single-harness modes. Both are in this list so the
            // divergence rules below are exercised against them — in particular
            // rule (2), which is what proves a row claiming a topology its
            // harness cannot have degrades to that harness's default instead of
            // being handed a lane with no implementation behind it.
            Some("app-server"),
            Some("extension"),
            Some("garbage"),
            Some(""),
        ];

        let as_hosting = |m: Mode| match m {
            Mode::Pane => Hosting::MuxPane,
            Mode::Daemon => Hosting::Daemon,
            Mode::Extension => Hosting::Extension,
            Mode::AppServer => Hosting::AppServer,
        };

        let mut diverged = 0usize;

        for p in providers {
            for t in tokens {
                let old = row_hosting(p, t);
                let new = quorum_qw::lane_for(p, t).map(|l| as_hosting(l.mode));

                let harness = Harness::from_provider_id(p);
                let claimed = t.and_then(Mode::from_hosting_token);

                match (harness, claimed) {
                    // (1) unknown provider: lane_for refuses outright.
                    (None, _) => {
                        assert_eq!(new, None, "lane_for must refuse unknown provider {p:?}");
                        if old.is_some() {
                            diverged += 1;
                        }
                    }
                    // (2) known provider claiming an impossible topology.
                    (Some(h), Some(m)) if !h.supports(m) => {
                        assert_eq!(old, Some(as_hosting(m)), "row_hosting takes the row's word");
                        assert_eq!(
                            new,
                            Some(as_hosting(h.row_default_mode())),
                            // ROW default, not create default: this whole test
                            // is about how a row ON DISK re-derives, and that
                            // derivation is frozen (DEC-3). When the create
                            // default moves, this assertion and the divergence
                            // count below must both stay exactly as they are —
                            // if they move, the freeze has been broken.
                            "lane_for degrades an impossible claim to the harness row default"
                        );
                        diverged += 1;
                    }
                    // Everything else must be identical — this is the drop-in case.
                    _ => assert_eq!(
                        old, new,
                        "lane_for must match row_hosting for provider={p:?} hosting={t:?}"
                    ),
                }
            }
        }

        // Pin the divergence count so a future change to either function that
        // widens or narrows the gap shows up here rather than silently.
        //
        //   (1) unknown provider ("nonsense", "") x 4 parseable tokens      = 8
        //   (2) a known harness claiming a topology it cannot have:
        //         claude-code  x {daemon, app-server, extension}            = 3
        //         codex        x {extension}                                = 1
        //         pi           x {app-server}                               = 1
        //         acp/claude-code, acp/opencode, opencode
        //                      x {mux-pane, app-server, extension}  3 x 3   = 9
        //                                                           TOTAL  = 22
        //
        // The two single-harness modes are the bulk of (2), and that is the
        // point: `app-server` and `extension` each exist for exactly one harness,
        // so every OTHER harness claiming one must degrade rather than be handed
        // a lane with nothing behind it.
        assert_eq!(diverged, 22, "the deliberate-divergence set changed");
    }

    #[test]
    fn the_nine_lanes_cover_every_provider_and_topology_in_use() {
        // codex appears three times and pi three times; claude only pane; acp
        // only daemon.
        let ids: Vec<String> = Lane::ALL.iter().map(|l| l.id()).collect();
        for expected in [
            "claude-code/mux-pane",
            "codex/mux-pane",
            "codex/daemon",
            // The attachable codex residence. Distinct from `codex/daemon` by its
            // `hosting` stamp alone — same process, same protocol, different
            // answer to "may a human open a terminal on it?".
            "codex/app-server",
            "pi/mux-pane",
            "pi/daemon",
            // The control-channel lane: pi's TUI in a pane that `qd send` can
            // also drive. Pane-shaped, but its own lane because the carrier
            // differs.
            "pi/extension",
            "acp/claude-code/daemon",
            "acp/opencode/daemon",
        ] {
            assert!(ids.iter().any(|i| i == expected), "missing lane {expected}");
        }
        assert_eq!(ids.len(), 9);
    }
}
