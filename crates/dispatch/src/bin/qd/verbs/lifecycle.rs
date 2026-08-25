//! REAL lifecycle backends: attach / new / info / live (spec §3).

use std::path::PathBuf;

use clap::ArgMatches;

use dispatch::boot::RealSleeper;
use dispatch::effects::{Clock, Env, RealClock, RealEnv};
use dispatch::events;
use dispatch::exec::RealExec;
use dispatch::join::JoinOpts;
use dispatch::launch::capture_backend_env;
use dispatch::model::{Session, SessionStatus};
use dispatch::provider::codex::pane::CodexTuiError;
use dispatch::provider::pi::pane::PiTuiError;
use dispatch::zmx_dir::{legacy_zmx_dirs, resolve_zmx_dir, XdgFamily};
use quorum_qw::delivery::{priming, render_notes, CarrierError};

use super::common;

// --- attach mechanic (commands/lifecycle.ts:355-395) ---
//
// `attach_resolved` + `AttachOutcome` USED to live here: a shared mechanic that
// ran the id-collision preflight, the daemon redirect (with the codex-viewer
// exception), the unknown-provider refusal and the live pane handoff, then handed
// the COLD case back to its one caller to route.
//
// Both are DELETED. `qd attach` is the lane layer's first production caller now:
// it derives the `Lane` from the row and the handoff is `LaneOps::attach`, whose
// `NotSupported` / `Cold` answers replace the outcome enum. The three pieces that
// are genuinely qd's — the collision preflight, the codex viewer
// ([`attach_codex_viewer`], kept per ruling J) and every message — moved to
// `verbs/attach.rs`, which was their only caller. Nothing here was shared: the
// "shared mechanic" had exactly one call site for its whole life.

/// The backend-selected create dirs (C1 D2/D3): `(canonical, legacy)`.
///
/// The embedded lane creates in its single qrmux dir (legacy EMPTY); the zmx lane
/// keeps the canonical + cross-dir legacy scan (Bug-D). Extracted verbatim from
/// `run_start` when the codex-interactive lane became a second caller — a create
/// path that resolved these differently would create sessions the other lane's
/// scans cannot see, which is the Bug-D class this pairing exists to prevent.
/// `Err(code)` carries the already-printed exit code.
fn resolve_create_dirs(
    backend: dispatch::mux_selector::Backend,
    home: &std::path::Path,
    env: &RealEnv,
) -> Result<(PathBuf, Vec<PathBuf>), i32> {
    match backend {
        dispatch::mux_selector::Backend::Zmx => {
            let canonical = resolve_zmx_dir(env);
            // PRODUCTION: scan `/tmp` AND the env-derived XDG family (independent
            // axes; ADD-9b red-team BLOCKER 1). A14-2(c): the surviving READ scan
            // honors QD_TEST_SCAN_ROOTS (test lanes only; production = literal /tmp).
            let scan_roots =
                dispatch::zmx_dir::legacy_scan_roots(env, std::path::Path::new("/tmp"));
            let xdg = XdgFamily::from_env(env, env.uid());
            let legacy = legacy_zmx_dirs(env.uid(), &canonical, &scan_roots, Some(&xdg));
            Ok((canonical, legacy))
        }
        dispatch::mux_selector::Backend::Embedded => {
            let canonical = match dispatch::qrmux_dir::resolve_qrmux_dir(home, env) {
                Ok(d) => d,
                Err(msg) => {
                    eprintln!("qd start: {msg}");
                    return Err(1);
                }
            };
            Ok((canonical, Vec::new())) // embedded: single dir, legacy EMPTY.
        }
    }
}

// --- new (A2 run_new path, commands/lifecycle.ts:707-809) ---

/// `qd start <name> [claudeArgs...]` (P0 W1: today's `new` renamed, qb
/// spec-cli §11; the retired `new` verb errors in verbs/stubs.rs and never
/// reaches this backend) — A2's detached create (run_new), now WITH
/// A4 `-p/--prompt` + `--model` DELIVERY (spec §3.4) and the went-busy EXIT
/// CONTRACT (§3.5: 0=accepted, 10=stalled, 1=infra/other). `--port` DEFERRED
/// (parked, and no longer advertised — FTUE punch R6); A6 makes `--via <name>`
/// LIVE — F1 caller-capture + backends.json profile composition thread the
/// backend env into create (spec §2.2 + §3.2).
///
/// FTUE punch R19 + R20 add the two ends a human notices. R20: an OMITTED
/// `--provider` is a question at a terminal (asked from the harnesses actually
/// installed) and claude-code everywhere else. R19: a successful create at a
/// terminal ENDS INSIDE the session, rather than printing a `qd attach <name>`
/// the user then has to type at a session they are already looking at. Neither
/// touches the agent surface: both are gated on `crate::driver`, which answers
/// Agent for every marker-carrying or piped caller.
/// punch item 11: the fork-target transcript preflight. Returns `Some(error)`
/// when the target has NO transcript on disk (claude: `transcript_path` over
/// the projects-dir for that sid), `None` when the fork is legal. Status-blind
/// by design: a tombstoned (stopped) session WHOSE TRANSCRIPT EXISTS is a
/// legal fork source (the fork reads the transcript, not the process) —
/// pinned by unit test.
///
/// S4: the root is `paths.projects_dir` DIRECTLY — by this point a codex fork
/// is refused upstream and same-provider has run, so only claude reaches here;
/// passing the claude root matches the three existing claude-site call sites
/// (wait.rs / send.rs / join.rs). The provider-generic `transcript_root(fx)`
/// seam returns if codex forks ever become legal.
fn fork_transcript_missing_error(
    provider_impl: &dyn dispatch::provider::Provider,
    paths: &dispatch::paths::QdPaths,
    target: &Session,
) -> Option<String> {
    let root = &paths.projects_dir;
    // F4 (red-team r1): an EXISTING-but-unenumerable projects root (permission
    // failure) must not produce a FALSE "no transcript exists" claim —
    // find_jsonl_path maps a read_dir error to not-found. The preflight is an
    // improvement layer, never a gate that lies: SKIP it (fail-open to the
    // pre-B1 behavior — the fork proceeds and any genuine absence surfaces
    // downstream). An ABSENT root keeps refusing: "no transcript exists" is
    // then a true statement (claude never wrote one).
    if root.exists() && std::fs::read_dir(root).is_err() {
        return None;
    }
    let key = dispatch::provider::SessionKey {
        id: &target.session_id,
        name: target.name.as_deref(),
        cwd: target.cwd.as_deref(),
        pid: target.pid,
    };
    if provider_impl.transcript_path(root, &key).is_some() {
        return None;
    }
    let display = target.name.as_deref().unwrap_or(&target.session_id);
    Some(format!(
        "qd start: cannot fork \"{display}\" — no transcript exists for session id \
         {} (looked under {}). The session never produced a transcript (it may have \
         been created but never used). Pick another --fork source, or start fresh \
         without --fork.",
        target.session_id,
        root.display()
    ))
}

/// WP-B5-iii Mechanism S (`FORK-IDENTITY-SPEC.md` §3/§4/§5a): seed a forked
/// transcript for `target` at a FRESH fork UUID under `cwd`'s projects slug, and
/// return `(fork_uuid, optional staleness notice)`. The fork then launches with
/// `--resume <fork_uuid>` in `cwd` — claude resolves `--resume` purely by
/// `<cwd-slug>/<uuid>.jsonl` (no session index; `WP-B5iii-TRANSCRIPT-TRACE.md`
/// exp F-SEED), so the seed must live under the FORK's launch-cwd slug. The
/// parent transcript is read O_RDONLY only (never mutated). Identity rides the
/// pre-minted `fork_uuid`: the caller `mint_or_get(fork_uuid)`s the fork's OWN
/// qdId (option A — never the parent's). Mechanism S is uniform (default + the
/// `--turn` rewind) per the lead's Q4-FOLLOWUP ruling; it touches ZERO banked
/// daemon/qrmux/protocol/argv surface (the fidelity gate proves it equivalent to
/// native `--fork-session`).
fn seed_fork_transcript(
    provider_impl: &dyn dispatch::provider::Provider,
    paths: &dispatch::paths::QdPaths,
    cwd: &std::path::Path,
    target: &Session,
    turn: Option<usize>,
    env: &dyn Env,
) -> Result<(String, Option<String>), i32> {
    let display = target
        .name
        .as_deref()
        .unwrap_or(&target.session_id)
        .to_string();
    // Locate the parent's DURABLE transcript via the provider seam (the same
    // resolver the transcript-existence preflight used).
    let key = dispatch::provider::SessionKey {
        id: &target.session_id,
        name: target.name.as_deref(),
        cwd: target.cwd.as_deref(),
        pid: target.pid,
    };
    let Some(parent_path) = provider_impl.transcript_path(&paths.projects_dir, &key) else {
        eprintln!("qd start: cannot fork \"{display}\" — its transcript is unreadable.");
        return Err(1);
    };
    let text = match std::fs::read_to_string(&parent_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "qd start: cannot read the source transcript at {}: {e}.",
                parent_path.display()
            );
            return Err(1);
        }
    };
    let records = dispatch::fork_seed::parse_records(&text);
    let point = match turn {
        Some(n) => dispatch::fork_seed::ForkPoint::Turn(n),
        None => dispatch::fork_seed::ForkPoint::Latest,
    };
    let resolved = match dispatch::fork_seed::resolve(&records, point) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("qd start: cannot fork \"{display}\" — {e}");
            return Err(1);
        }
    };
    // Option A: the fork's session-id is known PRE-spawn (we mint it), so the
    // seed is named by it AND the fork's own qdId is minted from it.
    let fork_uuid = dispatch::fork_seed::new_fork_uuid();
    let seed = dispatch::fork_seed::rekey_truncate(&records, resolved.boundary_idx, &fork_uuid);
    // Write under the FORK's launch-cwd slug (claude resolves --resume by it).
    let slug = dispatch::jsonl::cwd_to_project_path(&cwd.to_string_lossy());
    let dir = paths.projects_dir.join(slug);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "qd start: cannot create projects dir {}: {e}",
            dir.display()
        );
        return Err(1);
    }
    let seed_path = dir.join(format!("{fork_uuid}.jsonl"));
    if let Err(e) = std::fs::write(&seed_path, dispatch::fork_seed::to_jsonl(&seed)) {
        eprintln!(
            "qd start: cannot write the forked transcript {}: {e}",
            seed_path.display()
        );
        return Err(1);
    }
    // WP-B5-iii obl-4: record the fork→parent lineage pointer (the PARENT's qdId,
    // keyed by the fork's uuid — works for BOTH the interactive and headless
    // launch paths, no daemon/protocol change). Best-effort + additive: a lineage
    // hiccup never fails the launch; only recorded when the parent has a stable
    // qdId. STRICTLY the parent pointer — the fork's OWN qdId is minted elsewhere.
    if let Some(parent_qd) = target.qd_id.as_deref() {
        if let Ok(ids_path) = common::ids_store_path(env) {
            if let Err(e) =
                dispatch::idstore::record_lineage(&ids_path, &fork_uuid, parent_qd, &RealClock)
            {
                eprintln!("qd start: WARNING — fork lineage not recorded: {e}");
            }
        }
    }
    Ok((fork_uuid, resolved.staleness_report()))
}

/// The `--provider` names the unknown-provider refusal lists — **derived from
/// `Harness::ALL`, so the advertised list and the accepted set cannot drift.**
///
/// The accepted set is qw's (`Harness::from_provider_id`, which is also what
/// `Lane::for_create` routes on); this is the same set, spelled for a human. A
/// line that advertised a provider the engine refuses — or refused one it
/// advertises — is exactly the drift a hand-maintained literal invites, and this
/// engine had two copies of that literal.
///
/// The BYTES are unchanged. `DISPLAY_ORDER` is the order the message has always
/// used (not `Harness::ALL`'s), and the `match` is exhaustive with no wildcard —
/// so adding a harness in qw fails to compile HERE until someone decides how it
/// is named to a user. `supported_provider_names_covers_every_harness` asserts
/// the order list is the full set.
fn supported_provider_names() -> String {
    use quorum_qw::lane::Harness;
    const DISPLAY_ORDER: [Harness; 4] = [
        Harness::ClaudeCode,
        Harness::Codex,
        Harness::Pi,
        Harness::Opencode,
    ];
    // No alias parenthetical any more. `opencode` used to read `opencode (=
    // acp/opencode)` because the name a user typed and the id qd stored were
    // different strings; ACP-as-a-mode collapsed them, and every harness now has
    // exactly one spelling.
    DISPLAY_ORDER
        .iter()
        .map(|h| h.provider_id().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The refusal for a `--provider` value this engine cannot place.
///
/// TWO different mistakes reach here and they deserve different sentences. A
/// value with no `/` is a program name that does not name a program, and the fix
/// is to pick one that does. A value WITH a `/` is a lane id, and if its first
/// segment names a real program then the program is fine and the topology is
/// what is wrong — telling that caller "unknown provider: claude-code/daemon"
/// invites them to doubt `claude-code`, which is not the problem, and hides the
/// only useful fact: which lanes that harness actually has.
///
/// The lane list is DERIVED from `Lane::ALL`, so a harness that gains a lane
/// starts being offered one here without anybody remembering to add it.
fn unplaceable_provider_message(arg: &str) -> String {
    use quorum_qw::lane::{Harness, Lane};
    if let Some((program, _)) = arg.split_once('/') {
        if let Some(h) = Harness::from_provider_id(program) {
            let lanes = Lane::ALL
                .iter()
                .filter(|l| l.harness == h)
                .map(|l| l.id())
                .collect::<Vec<_>>()
                .join(", ");
            return format!(
                "qd start: \"{arg}\" is not a lane — {program} has no such topology. \
                 {program}'s lanes are: {lanes}."
            );
        }
    }
    format!(
        "qd start: unknown provider \"{arg}\" — this engine supports: {}. \
         (A lane can be named directly too, e.g. --provider codex/daemon.)",
        supported_provider_names()
    )
}

/// What `qd start` boots when nothing and nobody says otherwise — and the answer
/// R20 stops treating as a decision. It is still the right FALLBACK (it is the
/// harness the relay is native to), it was just never a CHOICE the user was
/// offered.
const DEFAULT_PROVIDER: &str = "claude-code";

/// The HARNESS a detected binary belongs to.
///
/// The detector and the router keep separate vocabularies, for separate and
/// legitimate reasons: `HarnessId` is what `qd setup` probes for on PATH, and
/// `Harness` is what `Lane::for_create` routes on. R20's prompt is the first
/// place the two meet, so the crossing gets exactly one exhaustive, wildcard-free
/// match — a fifth harness added to the detector then fails to compile HERE
/// until someone says which harness it starts.
///
/// **The two sets are the same size now, and that is a result rather than a
/// coincidence.** `HarnessId` has always had four entries and no ACP row,
/// because `qd setup` probes for PROGRAMS and there is no `acp` program to find —
/// the bridge runs claude, or opencode. `Harness` had five, and this function was
/// the seam where a four-program world met a five-harness one. ACP being a lane
/// makes the router agree with what the detector always said.
fn harness_for_detected(id: dispatch::setup::harness::HarnessId) -> quorum_qw::lane::Harness {
    use dispatch::setup::harness::HarnessId;
    use quorum_qw::lane::Harness;
    match id {
        HarnessId::ClaudeCode => Harness::ClaudeCode,
        HarnessId::Codex => Harness::Codex,
        HarnessId::Pi => Harness::Pi,
        HarnessId::Opencode => Harness::Opencode,
    }
}

/// `--provider` spelling for a detected harness — asked of the router, never
/// spelled here.
///
/// The naive version of this function is a second table of provider-id string
/// literals, and it would still be wrong: `HarnessId::as_str` answers `"claude"`
/// — the BINARY setup probes for — where the provider id is `"claude-code"`, the
/// program. That trap is one arm of four.
///
/// It used to be two. `Harness::Opencode` answered `"acp/opencode"` where the
/// detector said `"opencode"`, and the gap was not an oversight: while ACP was a
/// harness the provider id had to carry the transport, so it could not also be
/// the program name the detector finds on PATH. Now it is, and the arm is no
/// longer a trap.
///
/// Deriving the string from [`quorum_qw::lane::Harness::provider_id`] — the same
/// method the unknown-provider refusal above derives its advertised list from —
/// means the prompt cannot offer a spelling the parser rejects, because it is
/// reading the parser's own answer.
fn provider_id_for_harness(id: dispatch::setup::harness::HarnessId) -> &'static str {
    harness_for_detected(id).provider_id()
}

/// FTUE punch **R20** — resolve an OMITTED `--provider` by asking, where there is
/// someone to ask.
///
/// # The complaint
///
/// A flagless `qd start wk` silently booted claude-code. A user who installed
/// codex, or pi, got the wrong harness AND no indication that there had been a
/// choice — the worst shape a default can take, because nothing about the
/// outcome tells you a different one existed.
///
/// # What replaces it
///
/// The harnesses `qd setup` found (`verbs::setup::present_harnesses` — the same
/// probe, never a second one; see C4) are offered as a numbered list with the
/// default on Enter. Three shapes, and only one of them is a question:
///
/// - **nobody to ask** — no TTY, a pipe, a script, an agent session, or `--json`
///   — answers `None` and the caller keeps [`DEFAULT_PROVIDER`]. This is the half
///   of R20 that is not negotiable: a prompt in a context that cannot answer one
///   is a HANG, which is worse than the wrong default by a wide margin. The
///   driver resolves it, with [`DriverOverride::None`] so that `--interactive`
///   (which every commissioned agent seat passes) cannot talk its way into a
///   question.
/// - **exactly one harness installed** — there is no choice to offer, so it is
///   taken and SAID rather than asked. Silent when that one is claude-code: the
///   sentence would be announcing the default to a user who has nothing else.
/// - **two or more** — the actual prompt.
///
/// A caller who typed `--provider` never reaches here at all.
///
/// # Never hangs, never loops
///
/// `prompt_line` answers `None` on EOF, which takes the default immediately.
/// Unrecognised input is re-asked at most [`PROVIDER_PROMPT_TRIES`] times and
/// then takes the default: a start must always terminate in a session or an
/// error, never in an argument with the user about menu syntax.
fn resolve_provider_by_asking(
    home: &std::path::Path,
    env: &RealEnv,
    name: &str,
    json_out: bool,
) -> Option<String> {
    // --json means stdout is a document; a menu printed into it is corruption.
    // (A --json caller is almost certainly non-interactive anyway, so this is a
    // belt to the driver's braces — but "almost certainly" is not a contract.)
    if json_out {
        return None;
    }
    if crate::driver::resolve_driver_real(crate::driver::DriverOverride::None, env)
        != crate::driver::Driver::Human
    {
        return None;
    }

    let found = super::setup::present_harnesses(home, env, &RealExec);
    let (first, rest) = found.split_first()?;
    if rest.is_empty() {
        let id = provider_id_for_harness(*first);
        if id == DEFAULT_PROVIDER {
            return None;
        }
        println!(
            "qd start: {} is the only agent harness on this machine — starting \"{name}\" \
             with --provider {id}. (`qd setup` lists what qd looked for.)",
            first.label()
        );
        return Some(id.to_string());
    }

    // The default is claude-code when it is installed — the fallback this
    // function exists to stop being SILENT, not to stop being the default — and
    // otherwise the first harness found, in setup's report order.
    let default_idx = found
        .iter()
        .position(|h| provider_id_for_harness(*h) == DEFAULT_PROVIDER)
        .unwrap_or(0);

    println!("qd start: which harness should \"{name}\" run?");
    let width = found
        .iter()
        .map(|h| h.label().chars().count())
        .max()
        .unwrap_or(0);
    for (i, h) in found.iter().enumerate() {
        let mark = if i == default_idx { "  (default)" } else { "" };
        println!(
            "  {}) {:<width$}  --provider {}{mark}",
            i + 1,
            h.label(),
            provider_id_for_harness(*h),
            width = width
        );
    }

    let default_id = provider_id_for_harness(found[default_idx]);
    for _ in 0..PROVIDER_PROMPT_TRIES {
        let Some(answer) = super::super::tty::prompt_line(&format!(
            "  Number or name [{}]: ",
            default_idx + 1
        )) else {
            break; // EOF — take the default rather than spin on a closed stdin.
        };
        if answer.is_empty() {
            break;
        }
        // A number picks a row; anything else is matched against the two names
        // the line above showed — the harness label and the provider id — because
        // a user who reads "--provider codex" and types `codex` has answered the
        // question correctly and should not be told otherwise.
        let picked = answer
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=found.len()).contains(n))
            .map(|n| found[n - 1])
            .or_else(|| {
                found.iter().copied().find(|h| {
                    answer.eq_ignore_ascii_case(provider_id_for_harness(*h))
                        || answer.eq_ignore_ascii_case(h.as_str())
                        || answer.eq_ignore_ascii_case(h.label())
                })
            });
        if let Some(h) = picked {
            return Some(provider_id_for_harness(h).to_string());
        }
        // A LANE id is a legitimate answer even though the menu does not offer
        // one. The menu asks "which harness", and a caller who already knows they
        // want `codex/daemon` has answered a more specific version of the same
        // question — refusing it because the rows only listed programs would be
        // pedantry, and `--provider` itself takes the form.
        //
        // Returned VERBATIM rather than reduced to its harness: the whole content
        // of the answer is the topology, and `Lane::for_create` is what reads it.
        // Not filtered against the DETECTED set either — a lane id names a
        // harness explicitly, so there is no inference to protect, and refusing an
        // installed-but-undetected harness here would be a second, worse accept-set.
        if quorum_qw::lane::Lane::from_id(&answer).is_some() {
            return Some(answer);
        }
        println!("  \"{answer}\" is not one of the choices.");
    }
    println!("  Using {default_id}.");
    Some(default_id.to_string())
}

/// How many times the R20 harness prompt re-asks before taking its default. A
/// mistyped answer deserves another go; an unattended terminal echoing garbage
/// does not deserve an infinite loop between a user and their session.
const PROVIDER_PROMPT_TRIES: usize = 3;

pub fn run_new(m: &ArgMatches) -> i32 {
    let env = RealEnv;
    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("qd start: HOME is not set — cannot resolve the session state dir.");
            return 1;
        }
    };

    let name = m
        .get_one::<String>("name")
        .expect("required by clap")
        .clone();
    let cwd_opt = m.get_one::<String>("cwd").map(PathBuf::from);
    // P0 start-surface rework (STATE 21 ruling): `--resume` is GONE from start
    // (the resume verb owns same-participant wake); `--fork <session>` is the
    // one transcript-seeded start — a NEW participant forked from an existing
    // session's transcript. The value is resolved below via the standard
    // target pipeline (name | full qd id | unambiguous prefix).
    let fork_target = m.get_one::<String>("fork").cloned();
    // WP-B5-iii (FORK-IDENTITY-SPEC §4): `--turn N` = REWIND-ONLY (1-based), only
    // meaningful with `--fork`. Omitted ⇒ latest safe boundary. A non-positive or
    // non-numeric value, or `--turn` without `--fork`, is an error-that-teaches.
    let fork_turn: Option<usize> = match m.get_one::<String>("turn") {
        None => None,
        Some(s) => match s.parse::<usize>() {
            Ok(n) if n >= 1 => Some(n),
            _ => {
                eprintln!(
                    "qd start: --turn must be a positive integer (the conversational-turn \
                     ordinal to rewind the fork to)."
                );
                return 1;
            }
        },
    };
    if fork_turn.is_some() && fork_target.is_none() {
        eprintln!(
            "qd start: --turn is only valid together with --fork (it rewinds the fork to a \
             past conversational-turn boundary)."
        );
        return 1;
    }
    // FTUE punch R19: the opt-OUT, replacing the A5-deferred `--attach` opt-in.
    // A start at a terminal now ends inside the session it created; this returns
    // instead. It is only ever a veto — see `crate::driver::attaches_after_start`,
    // which owns the rest of the decision.
    let no_attach = m.get_flag("no-attach");
    // Lifecycle-collapse A-1 (spec D4): machine-readable identity output. Under
    // --json the human "Started detached session" line moves to stderr and
    // stdout carries exactly one JSON object (success identity, or the A-2
    // error object on a bind-arm failure).
    let json_out = m.get_flag("json");
    // Lifecycle-collapse A-3 (spec D5, Pete's ruling): relay readiness is
    // DEFAULT-ON for start; --no-await-relay is the opt-out. Precedence:
    // flag > QD_BOOT_AWAIT_RELAY env (the transition alias: "1"/"true" was the
    // old opt-in and is now redundant; "0"/"false" is an explicit env opt-out —
    // the jailed test harnesses' central lever) > default ON.
    let await_relay = if m.get_flag("no-await-relay") {
        false
    } else {
        !env.var("QD_BOOT_AWAIT_RELAY")
            .is_some_and(|v| v == "0" || v.eq_ignore_ascii_case("false"))
    };
    // WP-B-CS-1 (D2): the driver-mode override flags (auto-detect escape hatch).
    let headless_flag = m.get_flag("headless");
    let interactive_flag = m.get_flag("interactive");
    let extension_flag = m.get_flag("extension");
    // DEC-2: the opt-back-in to the headless resident, now that it is not what a
    // flagless start makes. See the topology block below.
    let daemon_flag = m.get_flag("daemon");
    let app_server_flag = m.get_flag("app-server");
    let acp_flag = m.get_flag("acp");
    let agent = m.get_one::<String>("agent").cloned();
    // `qd start --agent <name>` is RETIRED. The old static-agent path resolved
    // `~/.quorum/dispatch/plugins/core/agents/<name>.md` and fail-closed booted that role;
    // role/agent CONTENT now lives in the work-model plugin and spawning is
    // `frame commission <role>.md`, never the engine's native flag. Refuse here —
    // BEFORE preflight/claim/launch, so nothing is created — with the teaching
    // error (the new/kill/attach retirement pattern). NOTHING uses --agent.
    if agent.is_some() {
        return super::stubs::run_start_agent_retired();
    }
    let prompt = m.get_one::<String>("prompt").cloned();
    let model = m.get_one::<String>("model").cloned();
    let provider = m.get_one::<String>("provider").cloned();
    let port = m.get_one::<String>("port").cloned();
    let claude_args: Vec<String> = m
        .get_many::<String>("claudeArgs")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    // E2 guard (Effort A / Reading B, atomic2-impl-plan.md §4): the SINGLE
    // production `claudeArgs` chokepoint. REFUSE (loud, fail-closed) any forbidden
    // claude flag {-p, --print, --output-format, --input-format} in the trailing
    // positional — dispatch only creates tracked, attachable sessions, never a
    // one-off `claude --print` run. This fires BEFORE the provider/codex/acp early
    // returns and the headless/interactive lane split, so it covers EVERY lane and
    // provider with one check, before anything is minted/claimed/spawned. It
    // inspects ONLY `claude_args` — never qd's own `--prompt` (the two-`-p` trap
    // boundary: `qd start <name> -p <prompt>` stays a valid tracked agent turn).
    let forbidden = dispatch::launch::forbidden_in(&claude_args);
    if !forbidden.is_empty() {
        eprintln!(
            "{}",
            dispatch::launch::forbidden_flag_teaching_error(&forbidden[0])
        );
        return 1;
    }
    // --via <name> (spec §3.2): now LIVE (no longer a no-op). Resolved against
    // backends.json below; threads the composed backend env into create.
    let via = m.get_one::<String>("via").cloned();
    // punch item 7: per-session render mode (flag > render-default config >
    // inline). Resolved here, injected as a birth property by create.rs via the
    // shared launch_env_pairs assembly. Meaningless for the codex daemon path
    // (no mux pane to render) — silently unused there.
    let render = common::resolve_render_mode(m, &env);

    // A-OC.1: `--provider opencode` is UN-PARKED — it resolves to the acp/opencode provider
    // below and rides the acp/-prefix daemon create path. `--port` STAYS parked (the legacy
    // opencode-ws port; the acp/opencode residence allocates its own loopback port).
    if port.is_some() {
        eprintln!("qd start: --port is not yet supported in the Rust engine (parked).");
        return 1;
    }
    // codex P1, R1 (codex-p1-spec section 3.3): FAIL-CLOSED on an unknown
    // --provider value (orc-14-ENDORSED change; ADD-13(4); Hardening-#1 pattern).
    // Today's silent fall-through boots claude on garbage input; this exits 1
    // BEFORE preflight/claim (no state). None / "claude-code" / "codex" proceed
    // (the opencode/--port honest-error above stays FIRST + byte-identical). codex
    // P2 W4: codex is now a supported value (GATE-R RULED (A) daemon-thread).
    //
    // The SET is qw's and the WORDING is qd's, and that split is the point.
    // `parse_provider_arg` is exactly what `Lane::for_create` routes on, so asking
    // it here means the accept-set can no longer drift from the set that actually
    // has lanes. The message text and the exit code stay on this side, where
    // user-facing wording belongs.
    //
    // It accepts a LANE id as well as a program name, which is why the refusal
    // below teaches both forms: `--provider codex/daemon` is a legitimate way to
    // ask for a lane, and a user who mistypes one should not be told the program
    // is unknown.
    if let Some(p) = provider.as_deref() {
        if quorum_qw::lane::parse_provider_arg(p).is_none() {
            eprintln!("{}", unplaceable_provider_message(p));
            return 1;
        }
    }
    // --- FTUE punch R20: an omitted --provider is a QUESTION, not an assumption ---
    // Placed AFTER the accept-set check (so a typed-and-wrong `--provider` still
    // fails on its own terms, unasked) and BEFORE every resolution below, because
    // the answer decides which provider `--fork`, `--interactive` and the lane are
    // validated against. `None` back means "nobody to ask" and the
    // `unwrap_or(DEFAULT_PROVIDER)` below is unchanged from what a flagless start
    // has always done. See `resolve_provider_by_asking`.
    let provider = match provider {
        Some(p) => Some(p),
        None => resolve_provider_by_asking(&home, &env, &name, json_out),
    };

    // codex P1 W3 (codex-p1-spec section 7.1 step 2): resolve the provider ONCE
    // from the validated value (None ⇒ "claude-code"). The W1 fail-closed check
    // above already guarantees the value is "claude-code" here, so `provider_for`
    // resolves; the defensive None arm re-prints the SAME loud unknown-provider
    // error rather than panicking (it is structurally unreachable given the check,
    // but a fail-closed exit is the only honest posture if the two ever drift).
    let provider_id = provider.as_deref().unwrap_or(DEFAULT_PROVIDER);
    // The `Provider` impl is looked up by the id the REGISTRY keys on, which is
    // not always the string the user typed: `--provider codex/daemon` names a
    // lane, and `provider_for` is a registry of programs. Try the typed string
    // first — it is what the legacy `acp/*` spellings need, and they resolve the
    // bridge impl rather than the bare program's — then fall back to the harness's
    // canonical id for a lane-id argument.
    //
    // This impl is read for exactly one thing below (`--fork`'s transcript
    // resolution), so the fallback is not load-bearing today; it is written this
    // way so that a lane-id argument cannot arrive at a `None` and be reported as
    // an unknown provider it plainly is not.
    let provider_impl = dispatch::provider::provider_for(provider_id).or_else(|| {
        quorum_qw::lane::parse_provider_arg(provider_id)
            .and_then(|(h, _)| dispatch::provider::provider_for(h.provider_id()))
    });
    let Some(provider_impl) = provider_impl else {
        eprintln!(
            "qd start: unknown provider \"{provider_id}\" — this engine supports: {}.",
            supported_provider_names()
        );
        return 1;
    };
    // P0 qafix R2 (orc ruling 2026-06-10), kept across the start-surface rework:
    // codex start has NO transcript seed — `run_new_codex_daemon` never receives
    // --fork, so it used to be DROPPED silently pre-validation. Errors-that-teach:
    // refuse loudly, naming what codex doesn't support and the working revive
    // path. (The R2 --resume refusal arm died with the flag itself — `start
    // --resume` is now an unknown option at parse.)
    // codex-interactive: `--interactive` means "give me a TUI in a pane I can
    // attach to". claude has always had one and codex now does too, but acp/* and
    // pi are daemon-hosted with NO terminal to host — their harnesses speak stdio
    // protocols to a dispatch-owned adapter, not a screen. Silently ignoring the
    // flag would promise an attachable session and deliver a daemon, so refuse
    // BEFORE anything is claimed or spawned and name the lane that does exist.
    //
    // pi-interactive: pi is no longer in this refusal. It has a real TUI (a bare
    // `pi`, the mode `--mode rpc` opts OUT of), so `--interactive` now means for
    // pi exactly what it means for claude and codex. acp/* keeps the refusal: an
    // ACP bridge is a protocol adapter with no terminal of its own at all.
    //
    // ASKED OF THE HARNESS, not of the provider STRING, and that repairs a bug
    // this refusal carried for as long as it existed. It used to read
    // `provider_id.starts_with("acp/")`, which caught `--provider
    // acp/claude-code --interactive` and MISSED `--provider opencode
    // --interactive` — the alias for the same bridge — which slipped past and
    // was silently downgraded to a daemon, exit 0. There is no string to test
    // now: `Harness::supports(Mode::Pane)` is the question that was always being
    // asked, and no spelling can walk past it.
    if interactive_flag
        && quorum_qw::lane::Harness::from_provider_id(provider_id)
            .is_some_and(|h| !h.supports(quorum_qw::lane::Mode::Pane))
    {
        eprintln!(
            "qd start: --interactive is not supported with --provider {provider_id} — it is \
             daemon-hosted and has no terminal to attach. Start it without --interactive \
             and drive it with \"qd send {name} <text>\". (--interactive is available for \
             claude-code, codex and pi.)"
        );
        return 1;
    }
    if provider_id == "codex" && fork_target.is_some() {
        eprintln!(
            "qd start: --fork is not supported with --provider codex — codex start \
             always begins a new thread (no transcript branching). To revive a stopped \
             codex session, use \"qd resume <name>\"."
        );
        return 1;
    }
    // -p/--prompt + --model delivery now LANDS (spec §3.4) — no more honest error.

    let cwd =
        cwd_opt.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let paths = dispatch::paths::QdPaths::from_home(&home);

    // --- P0 start-surface rework: resolve the `--fork <session>` target -------
    // The standard target pipeline (common::all_sessions + resolve_or_die — the
    // same loud, house-shaped ambiguity/not-found errors resume/attach print).
    // JoinOpts mirror resume's (include_all so an auto-named/cold target
    // resolves; include_tombstoned so a stopped session's transcript is
    // forkable). The resolved session's provider UUID seeds the forked launch
    // (claude: `--resume <uuid> --fork-session`); the forked boot mints a NEW
    // provider UUID, so identity is always mint_unbound + bind-at-boot-confirm
    // below (the qafix R1 live-collision preflight is gone for start: forking a
    // LIVE session is legal and safe — a new UUID, a new participant).
    let (fork_uuid, fork_staleness): (Option<String>, Option<String>) = match fork_target.as_deref()
    {
        None => (None, None),
        Some(query) => {
            // Resolve through the sealed uncapped entry. D-2 accept-set: forking a
            // stopped session's transcript is a primary revival use (fork mints a
            // NEW session from the dead one's history), so no post-resolve
            // rejection — fork acts on a tombstone directly. Uncapped + include_all
            // so an auto-named / cold target far outside the display cap resolves.
            let target = match common::resolve_session_uncapped(query) {
                Ok(s) => s,
                Err(code) => return code,
            };
            let target = &target;
            // A ZmxOnly row (pane with no registry/transcript identity) carries
            // an EMPTY session_id — there is no transcript to fork.
            if target.session_id.is_empty() {
                eprintln!(
                    "qd start: session \"{}\" has no provider session id — nothing to fork.",
                    target.name.as_deref().unwrap_or(query)
                );
                return 1;
            }
            // P0 red-team r2 (lead-adjudicated, errors-that-teach): the fork
            // TARGET must be same-provider. Without this, a codex/opencode
            // target's UUID is handed to `claude --resume <uuid> --fork-session`
            // and fails downstream as a confusing empty boot — the symmetric
            // twin of the codex-START --fork refusal above.
            if target.provider != provider_id {
                eprintln!(
                    "qd start: cannot fork \"{}\" — it is a {} session and the new \
                     session's provider is {} (transcripts don't fork across providers).",
                    target.name.as_deref().unwrap_or(query),
                    target.provider,
                    provider_id
                );
                return 1;
            }
            // punch item 11: transcript-existence preflight. A transcript-less
            // target used to pass preflight and die downstream as an
            // undiagnostic BootTimeout (the forked claude exits when --resume
            // finds nothing). Resolve the target's transcript through the
            // provider seam (claude: the projects-dir JSONL for that sid) and
            // refuse LOUDLY before any mint/claim/launch. Tombstoned targets
            // WITH a transcript stay legal (the fork reads the transcript) —
            // this checks transcript existence, never liveness/status.
            if let Some(msg) = fork_transcript_missing_error(provider_impl, &paths, target) {
                eprintln!("{msg}");
                return 1;
            }
            // WP-B5-iii Mechanism S: seed a NEW forked transcript at a fresh fork
            // uuid and resume THAT (not the parent) — the parent stays untouched
            // (O_RDONLY). The fork's identity rides the pre-minted uuid (option A:
            // `mint_or_get(fork_uuid)` below mints the fork's OWN qdId, never the
            // parent's). Uniform for the default (latest safe) + `--turn` rewind.
            match seed_fork_transcript(provider_impl, &paths, &cwd, target, fork_turn, &env) {
                Ok((uuid, stale)) => (Some(uuid), stale),
                Err(code) => return code,
            }
        }
    };
    // §5a: never silently fork stale — surface the gap when the chosen safe
    // boundary lags the live head (e.g. the source is mid-flight on a tool).
    if let Some(notice) = &fork_staleness {
        eprintln!("qd start: {notice}");
    }

    // --- THE LANE, AND THE ONE CREATE CALL ---------------------------------
    //
    // A five-arm ordered `if`-chain stood here, and below it five per-lane create
    // wrappers. All of it is GONE. Every one of those wrappers assembled the same
    // shape — resolve the backend / socket dirs / mux / ids store out of the
    // environment, build the core's deps struct, call the core, print — and that
    // assembly is qw's work, not a verb's: `quorum_qw::lanes` has held all seven
    // create arms since stage-2 phase 3 and until now had **no caller at all**.
    //
    // `qd start` was the last lane operation a qd verb performed in-process, and
    // the one neither gate could see: `dispatch::lane::gate` scans for the
    // in-process lane constructor, and create never went through a lane to begin
    // with, so it passed VACUOUSLY for this verb. It does not any more — see
    // `dispatch::lane::create_gate`, which counts the qw session-management cores a
    // verb still names directly.
    //
    // WHAT STAYS ON THIS SIDE is everything above and below this block, and it is
    // exactly the inventory `quorum_qw::lanes`' own header lists: the clap parse
    // and the `claudeArgs` forbidden-flag chokepoint, the parked `--port` refusal
    // (R6 unadvertised it; the refusal is why the flag still parses),
    // `--fork` target resolution and its transcript seed (they need qd's
    // fuzzy resolver), the driver auto-detect and its `Headless` refusal, `--via`
    // credential materialisation, every `--json` emission, the bind phase, the
    // relay-presence warning, telemetry, and the claude lane's post-boot `-p`
    // delivery with its went-busy exit mapping.
    //
    // THE ACP MASK IS GONE, and its removal is the point rather than a tidy-up.
    //
    // A `let interactive_lane = interactive_flag && !id.starts_with("acp/")` stood
    // here, and it existed to PRESERVE a bug: the refusal above used to test the
    // string the user typed, so `--provider acp/claude-code --interactive` was
    // caught while `--provider opencode --interactive` — the alias for the same
    // bridge — slipped past and was silently downgraded to a daemon, exit 0. The
    // mask reproduced that downgrade so the two spellings at least behaved alike.
    //
    // Both halves are repaired. The refusal asks `Harness::supports(Mode::Pane)`,
    // which no spelling can walk past, and `Lane::for_create` refuses a pinned
    // spelling that disagrees with an explicit flag — so `--provider
    // acp/claude-code --interactive` yields no lane rather than a masked one.
    // Nothing needs to mask a flag it can simply honour or refuse.
    // The topologies, as ONE value. `--daemon`, `--extension` and `--interactive`
    // are pairwise conflicting at parse, so at most one arm's condition is ever
    // true and this chain's ORDER is not load-bearing — it reads top-down as
    // "the most specific request wins" and clap has already guaranteed there is
    // only one. See `Lane::for_create` on why the parameter is an enum rather
    // than a row of bools.
    //
    // `--daemon` is listed first because it is the one that undoes a DEFAULT
    // rather than overriding another flag: it is how `codex/daemon` and
    // `pi/daemon` are reached at all now that a bare start makes
    // `codex/app-server` and `pi/extension` (DEC-2/DEC-4).
    let topology = if daemon_flag {
        quorum_qw::lane::CreateTopology::Daemon
    } else if acp_flag {
        quorum_qw::lane::CreateTopology::Acp
    } else if app_server_flag {
        quorum_qw::lane::CreateTopology::AppServer
    } else if extension_flag {
        quorum_qw::lane::CreateTopology::Extension
    } else if interactive_flag {
        quorum_qw::lane::CreateTopology::Interactive
    } else {
        quorum_qw::lane::CreateTopology::Default
    };
    // Routed on the string the USER TYPED, not on `provider_impl.id()`. The two
    // differ for exactly one input and it matters twice: `provider_for("opencode")`
    // resolves an impl whose internal id is still the legacy `acp/opencode`, so
    // routing on the impl would (a) make every refusal below quote a spelling the
    // user did not type, and (b) treat a plain `--provider opencode` as though it
    // had pinned a lane. What a caller typed is what pins a lane — that is the
    // whole content of `harness_and_pinned_mode`.
    let Some(lane) = quorum_qw::lane::Lane::for_create(provider_id, topology) else {
        // A LEGACY `acp/*` spelling plus a flag naming a different lane. Checked
        // FIRST because every message below assumes the provider named only a
        // program, and this one named a lane outright — so `--provider
        // acp/claude-code --daemon` would otherwise be told "claude-code has no
        // daemon lane", which is true and beside the point.
        //
        // This case became REACHABLE with the remodel and was not before: the
        // old engine masked `--interactive` for `acp/*` rather than refusing it,
        // so the pair produced a daemon and exit 0. Refusing it is the repair.
        //
        // TWO spellings pin a lane and they need different remedies. An explicit
        // `codex/daemon` is a CURRENT way to say it, so the advice is to drop
        // whichever half is wrong. A legacy `acp/claude-code` is not, so the
        // advice also names what it has become — a caller reading it may not know
        // the spelling moved.
        if let Some((harness, Some(pinned))) =
            quorum_qw::lane::parse_provider_arg(provider_id)
        {
            let named = quorum_qw::lane::Lane::new(harness, pinned)
                .map(|l| l.id())
                .unwrap_or_else(|| pinned.hosting_token().to_string());
            let is_legacy = quorum_qw::lane::harness_and_pinned_mode(provider_id)
                .is_some_and(|(_, p)| p.is_some());
            if is_legacy {
                eprintln!(
                    "qd start: --provider {provider_id} already names the {named} lane, and \
                     the topology flag you passed names a different one. That spelling is \
                     the older way to say \"{named}\"; drop the flag, or name the lane you \
                     want directly."
                );
            } else {
                eprintln!(
                    "qd start: --provider {provider_id} already names the {named} lane, and \
                     the topology flag you passed names a different one. Drop the flag, or \
                     name the lane you want — not both."
                );
            }
            return 1;
        }
        // `--extension` on a harness that has no extension lane lands here, and
        // it is one of the two reachable cases: every other path was validated
        // above. Named specifically, because "unknown provider" would be a lie
        // about a provider the engine supports perfectly well.
        if extension_flag {
            eprintln!(
                "qd start: --extension is pi's alone (provider \"{}\" has no extension lane). \
                 It rides pi's own extension loader, which no other harness here has.",
                provider_id
            );
            return 1;
        }
        // `--acp` on a harness with no ACP adapter, or on a legacy `acp/*`
        // spelling that already pins a DIFFERENT lane. Two distinct refusals, and
        // the difference is worth stating: codex and pi have no adapter wired up
        // in qd YET (a missing adapter, not a missing affordance — ACP is a
        // general protocol), while an outright contradiction is a caller asking
        // for two lanes at once.
        if acp_flag {
            eprintln!(
                "qd start: --acp is not available for --provider {} — no ACP adapter is \
                 wired up for that harness in qd yet. It is available for claude-code \
                 (claude-code/acp) and opencode (opencode/acp).",
                provider_id
            );
            return 1;
        }
        // The other one: `--daemon` on claude-code. It is REFUSED rather than
        // ignored, and the refusal is not a check — it is `Lane::new` answering
        // `None` for `(ClaudeCode, Daemon)`, the same mechanism that refuses
        // `--interactive` for `acp/*`. Ignoring it would be the exact failure
        // `Lane::for_create` was built to make unrepresentable: the caller asks
        // for a headless resident, gets an attached pane, exit 0.
        // `--app-server` on a harness that has no app-server lane. Same
        // mechanism as the two below: `Lane::new` answers `None`, and naming the
        // provider beats "unknown provider" for a provider the engine supports.
        if app_server_flag {
            eprintln!(
                "qd start: --app-server is codex's alone (provider \"{}\" has no app-server \
                 lane). It names a specific residence — `codex app-server --listen ws://…` \
                 with a `codex --remote` viewer able to join it — which no other harness \
                 here has.",
                provider_id
            );
            return 1;
        }
        // `--daemon` on a harness with no headless lane. TWO harnesses reach here
        // now and they are refused for different reasons, so the sentence is
        // chosen rather than shared: claude has no headless form at all, while
        // opencode HAS one — its ACP bridge — and simply does not spell it
        // `daemon`. Telling an opencode user "there is no headless opencode"
        // would be false and would send them looking for the wrong thing.
        if daemon_flag {
            let why = match quorum_qw::lane::Harness::from_provider_id(provider_id) {
                Some(quorum_qw::lane::Harness::Opencode) => {
                    "opencode's only residence IS its ACP bridge, so \"opencode/daemon\" \
                     names nothing. Start it without --daemon (or with --acp, which says \
                     the same thing)."
                }
                _ => {
                    "claude-code has no daemon lane at all. It is a TUI in a mux pane and \
                     nothing else: there is no headless claude to host, so \
                     \"claude-code/daemon\" is not a lane this engine can build. Start it \
                     without --daemon."
                }
            };
            eprintln!(
                "qd start: --daemon is not supported with --provider {provider_id} — {why} \
                 (--daemon is for --provider codex and pi, whose default lanes are \
                 codex/app-server and pi/extension.)"
            );
            return 1;
        }
        // Structurally unreachable: the accept-set was validated above against the
        // SAME `parse_provider_arg` that `for_create` routes on. Fail closed
        // with the same loud line rather than panic — the defensive-arm posture the
        // `provider_for` lookup directly above already takes.
        eprintln!(
            "qd start: unknown provider \"{}\" — this engine supports: {}.",
            provider_id,
            supported_provider_names()
        );
        return 1;
    };
    // THE CLAUDE PANE LANE — `claude-code/mux-pane` — and NOT the claude-code
    // harness. The distinction is the whole point of this binding.
    //
    // It was spelled `lane.harness == ClaudeCode` while claude had exactly one lane,
    // and that spelling stopped being true when ACP became a MODE rather than a
    // harness: `claude-code/acp` answers `ClaudeCode` too. It is a headless
    // bridge-hosted resident — no pane, no mux, no `claude` argv, no composer to
    // type into — so every phase gated on this binding is one it must not enter.
    // Each of them is the PANE launch's alone: the driver route below (a bare
    // agent start of a RESIDENT is not a no-op turn and must not be refused as
    // one), the QD_MUX preflight, `--via`, `--fork`, the trailing claudeArgs, and
    // the four post-create phases at the bottom — bind, relay-presence warning,
    // telemetry stamp, `-p` priming send. The ACP lane belongs with the other
    // daemon-hosted creates, which render one line and return; that is exactly
    // what it got while it was spelled `acp/claude-code`, and the rename must not
    // have moved it.
    //
    // `CLAUDE_PANE` rather than an inline two-field test because the constant
    // exists for this exact class of guard — see its doc for the sibling case it
    // was minted to fix.
    let claude_pane = lane == quorum_qw::lane::CLAUDE_PANE;

    // --- FTUE punch R19: does this start END INSIDE the session it makes? -----
    //
    // Resolved HERE, once, because the lane is the last of the four inputs to
    // arrive and every exit below has to agree on the answer. The decision itself
    // is `crate::driver::attaches_after_start`, where the reasoning lives; the
    // two non-obvious arguments are assembled here:
    //
    // - `no_attach || headless_flag`. `--no-attach` is the R19 opt-out. `--headless`
    //   is folded into it because it already MEANS "I am not a terminal" on every
    //   other surface, and honouring it as an attach veto costs nothing on the
    //   lanes where it is otherwise inert (codex/pi) while keeping the flag's one
    //   meaning intact.
    // - `is_pane() || is_app_server()`. The five lanes with a terminal to hand
    //   over: claude's pane, codex's pane, codex's app server (attachable through
    //   a second client — `Lane::is_app_server` is spelled as a predicate for
    //   exactly this reason), pi's pane and pi's extension pane. The other three —
    //   codex/daemon, pi/daemon, acp/* — have no terminal at all, and an attach
    //   attempt would reach nothing but the daemon-redirect error.
    //
    // The driver is resolved with `DriverOverride::None`: see the function's docs
    // for why `--interactive` must not be allowed to force this.
    let attach_after_start = crate::driver::attaches_after_start(
        crate::driver::resolve_driver_real(crate::driver::DriverOverride::None, &env),
        no_attach || headless_flag,
        prompt.is_some(),
        lane.is_pane() || lane.is_app_server(),
    );

    // --- WP-B-CS-1 (D2): driver auto-detect routing (claude PANE lane only) ----
    // I/O mode follows who DRIVES (S-B-COMMAND-SURFACE-RULINGS). A HUMAN caller →
    // today's interactive native-TUI create path. An AGENT caller → refused below.
    // `--headless`/`--interactive` override the auto-detect. It runs ONLY for the
    // claude PANE lane and it runs HERE, after the routing decision, because that
    // is where the old chain reached it: the daemon/pane arms returned above it
    // and never consulted the driver at all. `claude-code/acp` is one of those
    // arms: a daemon-hosted resident is precisely what an agent may start bare,
    // and `RefuseNoPrompt` — whose reason is that a headless `claude -p ""` is a
    // degenerate no-op turn — describes nothing that lane does.
    //
    // `crate::driver` reads WHO IS DRIVING — a qd-binary fact, never a lane concern
    // — which is why it stays on this side of the call.
    if claude_pane {
        match crate::driver::start_route(
            crate::driver::resolve_driver_real(
                crate::driver::DriverOverride::from_flags(headless_flag, interactive_flag),
                &env,
            ),
            prompt.is_some(),
        ) {
            // Human → fall through to the create below (unchanged).
            crate::driver::StartRoute::Interactive => {}
            // Fork B: a bare agent/headless start with no `-p` is a usage error (a
            // headless `claude -p ""` is a degenerate no-op turn). Teach the working
            // re-entry verbs.
            crate::driver::StartRoute::RefuseNoPrompt => {
                eprintln!(
                    "qd start: agent/headless start requires -p <prompt> (a bare headless \
                     start is a no-op turn). To re-enter an existing session use \
                     \"qd resume <name>\" or \"qd attach <name>\"."
                );
                return 1;
            }
            // Agent + prompt → P4DB drive-burn (§6): the vestigial `qd start`→headless
            // lane spawned a one-off `claude -p … --output-format stream-json` run. That
            // drive is REMOVED. Refuse at the routing level with a teaching error,
            // consistent with A5PW's refuse-one-off-print philosophy (the E2 chokepoint
            // at the top of this fn already refuses `-p`/`--print` in the trailing
            // claudeArgs; this closes the remaining auto-detected agent-launch path).
            // Nothing is spawned; nonzero exit.
            crate::driver::StartRoute::Headless => {
                eprintln!(
                    "qd start: dispatch does not spawn one-off `claude -p` stream-json runs. \
                     For a one-off print run, invoke `claude -p \"<prompt>\"` directly. To start a \
                     tracked, attachable session use an interactive start (`qd start <name> \
                     --interactive`); to re-enter an existing session use `qd resume <name>` or \
                     `qd attach <name>`."
                );
                return 1;
            }
        }
    }

    // A bogus QD_MUX is qd's refusal to WORD and qd's exit code to choose — the
    // split `common::build_mux`'s own doc draws ("what stays HERE is what has always
    // been qd's half and only qd's half: printing the failure and choosing the exit
    // code"). It is a pure parse of one env var, no session touched.
    //
    // CLAUDE PANE LANE ONLY, and the placement is the reason: the old chain reached
    // `select_backend` exactly here, AFTER the driver route and BEFORE the `--via`
    // composition, so a bogus QD_MUX beat a bad `--via` to stderr. The pane lanes
    // deliberately do NOT get this preflight — `pi/mux-pane`'s `--session-id`
    // capability refusal has to land ahead of any backend resolution (the order its
    // arm pins), so their selector failure comes back through the lane instead,
    // carrying `QD_MUX_INVALID_EXIT` on `LaneError::StartFailed::exit_code`.
    if claude_pane {
        if let Err(code) = common::select_backend(&env) {
            return code;
        }
    }

    // --- F1 capture + --via composition (spec §2.2 + §3.2) -------------------
    // Capture the caller's backend env (lifecycle.ts:874) via the injected Env
    // seam (L9a — never raw std::env). When --via is given, overlay the resolved
    // backends.json profile (profile-wins, §3.2.3). The result is the env-key set
    // the create writes to the 0600 self-deleting file. EMPTY ⇒ byte-zero change.
    //
    // qd RESOLVES and qw RECEIVES — the rule `StartRequest::env`'s doc states: a
    // `--via` profile's credentials are env pairs that by contract never touch
    // argv, and the resolver (`dispatch::secrets`, backends.json) is qd's.
    //
    // Claude PANE lane only, because that is the only create that has ever consumed
    // it: `--via` on a codex/pi/acp start has always been a no-op, and composing it
    // here would newly bake credentials into a resident's env.
    let (backend_env, backend_env_unset) = if claude_pane {
        match compose_backend_env(&env, &home, via.as_deref()) {
            Ok(set) => set,
            Err(code) => return code, // the helper already printed the loud error.
        }
    } else {
        (Vec::new(), Vec::new())
    };

    // --- `-p` at create: which lanes take it, and what qd says when they do not ---
    //
    // The set is `quorum_qw::lanes::create_prompt_refusal`, NOT a list kept here.
    // Three lanes deliver the first turn in-core (codex/daemon and both acp/*), and
    // for them the prompt rides the request. The other four refuse it, and qd does
    // NOT simply forward and print the refusal: for the three pane/daemon lanes it
    // says the session was created anyway and where to type the prompt, and for
    // claude it runs the whole post-boot priming send itself (see the `-p` block at
    // the bottom of this function, and `quorum_qw::delivery::priming`'s header for
    // why that send can be neither `start` nor `deliver`).
    //
    // So the decision is "does this lane take one", and it has exactly one owner.
    let prompt_refused = quorum_qw::lanes::create_prompt_refusal(lane).is_some();
    if prompt.as_deref().is_some_and(|s| !s.is_empty()) && prompt_refused && !claude_pane {
        // The wording is qd's — a lane has no user to talk to — and each line names
        // the lane's own reason plus the working re-entry. Unchanged, verbatim, from
        // the three wrappers this replaced.
        match (lane.harness, lane.mode) {
            (quorum_qw::lane::Harness::Codex, quorum_qw::lane::Mode::Pane) => eprintln!(
                "qd start: --provider codex --interactive ignores -p at create (the codex TUI has \
                 no verifiable submit path yet). The session is created; type the prompt after \
                 \"qd attach {name}\"."
            ),
            (quorum_qw::lane::Harness::Pi, quorum_qw::lane::Mode::Pane) => eprintln!(
                "qd start: --provider pi --interactive ignores -p at create (pi writes no \
                 transcript until its first assistant reply, so a create-time submit cannot be \
                 verified). The session is created; type the prompt after \"qd attach {name}\"."
            ),
            (quorum_qw::lane::Harness::Pi, quorum_qw::lane::Mode::Daemon) => eprintln!(
                "qd start: --provider pi ignores -p at create (tier-a create is turn-free). To drive a \
                 pi turn, send to the running session: qd send:relay {name} \"<prompt>\"."
            ),
            // No other lane both refuses a prompt and reaches here — claude is
            // excluded above because it DELIVERS the prompt, just not at create.
            _ => {}
        }
    }

    let req = quorum_qw::contract::StartRequest {
        cwd: cwd.clone(),
        name: name.clone(),
        model: model.clone(),
        // `--fork` is a CLAUDE mechanism end to end: the seed is a claude transcript,
        // rekeyed at a fresh uuid and resumed by a PLAIN `--resume`. codex refuses it
        // loudly above; pi and acp have always DROPPED it silently, and handing a
        // claude fork uuid to `pi --load-session` would ask pi to load a session that
        // does not exist. So it reaches the request only on the lane that has one.
        resume: fork_uuid
            .clone()
            .filter(|_| claude_pane)
            .map(quorum_qw::contract::SessionId),
        // Same rule. `claudeArgs` is the claude launch's trailing argv; the codex
        // DAEMON arm has always been handed an EMPTY passthrough and the other four
        // cores have no passthrough field at all, so forwarding it here would newly
        // push a `--` tail into codex's app-server argv.
        passthrough: if claude_pane {
            claude_args.clone()
        } else {
            Vec::new()
        },
        prompt: if prompt_refused { None } else { prompt.clone() },
        await_relay,
        env: backend_env,
        env_unset: backend_env_unset,
        render,
    };

    // The SAME constructor every other verb uses: a create goes over the wire and
    // executes inside `qw`. It went through `dispatch::lane::open_for_create` — the
    // in-process lane — for exactly as long as ruling D6 was outstanding, because
    // six of the seven create arms re-exec `current_exe()` into `qrmux-server`,
    // `acp-daemon` or `pi-daemon` and the `qw` binary carried none of the three. It
    // carries all three now, so the seam had nothing left to decide.
    let ops = dispatch::lane::open(lane, &env, paths.clone());
    let handle = match ops.start(&req) {
        Ok(h) => h,
        Err(e) => {
            let (line, code, boot_phase) = start_failure(&e);
            // §5.1 / D6: a BootTimeout on the -p flow emits a positive
            // priming-readiness-timeout to the BYNAME file (no sessionId exists on
            // a failed boot) BEFORE the existing loud exit. ONLY when -p was
            // requested (a bare `qd new` boot timeout keeps today's behavior
            // exactly). The existing stderr/exit are UNCHANGED.
            //
            // `phase` arrives TYPED on the error — `LaneError::StartFailed::boot_phase`
            // — which is the whole point of `boot::BootPhase` (m-4, ack3-spec §8): the
            // old `detail.contains("did not reach idle")` string-match is gone and a
            // process boundary must not quietly restore it.
            //
            // The best-effort bind of the pre-minted id that used to sit beside this
            // is now the MINTER's, in the lane's claude arm: it owns the ids path and
            // the id, and neither survives on the error a caller receives.
            if prompt.is_some() {
                if let Some(phase) = boot_phase {
                    priming::emit_priming_timeout(&env, &RealClock, &home, &name, phase);
                }
            }
            eprintln!("{line}");
            // THE ESCAPE-HATCH POINTER (16-default-lane-switch.md, DEC-2 / B2).
            //
            // Both new default lanes need something the old defaults did not:
            // `pi/extension` needs a mux pane, a live pty and a drained
            // terminal, and `codex/app-server` wants a mux to put a viewer in.
            // A create that fails in a no-mux context — CI, a bare ssh session,
            // a container — is therefore failing for a reason the caller can fix
            // in one flag, and saying so is the difference between a default
            // change and a capability that just disappeared.
            //
            // Only on the two flipped lanes, and only when the flag was not
            // already given: `--daemon` cannot be the remedy for a failure the
            // daemon lane itself produced, and repeating a flag the caller
            // passed reads as the engine not listening. The lane is the key, as
            // everywhere else here — not the provider id, because
            // `--provider codex --interactive` failing has nothing to do with
            // this.
            let flipped_default = matches!(
                (lane.harness, lane.mode),
                (quorum_qw::lane::Harness::Pi, quorum_qw::lane::Mode::Extension)
                    | (
                        quorum_qw::lane::Harness::Codex,
                        quorum_qw::lane::Mode::AppServer
                    )
            );
            if flipped_default && !daemon_flag {
                eprintln!(
                    "qd start: \"{}\" is the default lane for --provider {}, and it needs a \
                     mux pane. If this is CI, a bare ssh session or any no-mux context, \
                     \"qd start {} --provider {} --daemon\" starts the headless {} lane \
                     instead — no pane, no TTY, nothing to attach.",
                    lane.id(),
                    lane.harness.provider_id(),
                    name,
                    lane.harness.provider_id(),
                    quorum_qw::lane::Mode::Daemon.hosting_token(),
                );
            }
            // A-1: a --json caller always gets one machine object on stdout.
            // Pre-bind create/boot failures use the catch-all class
            // "start-failed" (the three RULED classes — unbound | ambiguous |
            // diverged — are the bind phase's, below); the recipe treats any
            // other class as fail-to-operator. CLAUDE PANE LANE ONLY, exactly as
            // before: no other create arm has ever emitted a --json object.
            if json_out && claude_pane {
                let obj = serde_json::json!({
                    "error": {
                        "class": "start-failed",
                        "session": { "name": name, "pid": serde_json::Value::Null },
                        "message": line,
                    }
                });
                println!("{obj}");
            }
            return code;
        }
    };

    // Non-fatal notices the create produced, VERBATIM and already attributed.
    // Today's only producer is the acp arm, whose two `AcpWarning`s the verb used
    // to receive through a `warn` callback — see `SessionHandle::notes` for why a
    // lane returns them instead of printing them.
    for note in &handle.notes {
        eprintln!("{note}");
    }

    if !claude_pane {
        // The EIGHT lanes with nothing after the create — every lane but claude's
        // pane. One line each, byte-for-byte the line its wrapper printed, and exit 0.
        //
        // Eight, not seven: `claude-code/acp` reaches here now. It always should
        // have — its `(_, Mode::Acp)` arm below was written to serve both ACP
        // lanes — but the harness-shaped guard this `if` used to carry sent it up
        // the pane path instead and left that arm reachable only by opencode.
        //
        // EXHAUSTIVE, deliberately. This was a six-arm match with a `_ => {}`
        // catch-all, and the catch-all is what let `pi/extension` and
        // `codex/app-server` ship printing NOTHING: an opt-in lane whose success
        // was silence, which became a visible regression the moment both became
        // defaults. A wildcard here does not merely permit that — it guarantees
        // it, because adding a lane is the one edit that will never make the
        // compiler mention this file. The impossible pairs get named arms too
        // (see `verbs/resume.rs`, whose match is exhaustive for the same
        // reason), so that "there is no such lane" is written down rather than
        // absorbed.
        match (lane.harness, lane.mode) {
            (quorum_qw::lane::Harness::Codex, quorum_qw::lane::Mode::Pane) => println!(
                "Started codex session \"{name}\" — attach with \"qd attach {name}\""
            ),
            (quorum_qw::lane::Harness::Pi, quorum_qw::lane::Mode::Pane) => println!(
                "Started pi session \"{name}\" (session {}) — attach with \"qd attach {name}\"",
                // ALWAYS present on this lane — a pi row is identified from birth,
                // which is exactly what the codex pane outcome cannot claim.
                handle.id.as_ref().map(|i| i.0.as_str()).unwrap_or_default()
            ),
            (quorum_qw::lane::Harness::Codex, quorum_qw::lane::Mode::Daemon) => {
                println!("Started detached codex session \"{name}\"")
            }
            (quorum_qw::lane::Harness::Pi, quorum_qw::lane::Mode::Daemon) => {
                println!("Started detached pi session \"{name}\"")
            }
            (_, quorum_qw::lane::Mode::Acp) => {
                println!("Started detached acp session \"{name}\"")
            }
            // The extension lane names BOTH channels, because it is the one lane
            // that has both: a human attaches to the pane, and an agent drives
            // the same session over its control channel without attaching to
            // anything. A line naming only `qd attach` would read as "this is
            // the pi TUI lane", which is exactly the confusion the lane exists to
            // resolve — and it is the wording `qd resume` already uses for it
            // (`verbs/resume.rs`). The session id rides along for the same reason
            // it does on the pane arm: a pi row is identified from birth.
            (quorum_qw::lane::Harness::Pi, quorum_qw::lane::Mode::Extension) => println!(
                "Started pi session \"{name}\" (session {}) — attach with \"qd attach {name}\", \
                 or drive it with \"qd send {name}\"",
                handle.id.as_ref().map(|i| i.0.as_str()).unwrap_or_default()
            ),
            // The one daemon lane with a terminal to open. Everything else about
            // the create is `codex/daemon`'s — same spawn, same process — so the
            // verdict line is byte-identical to it and only the follow-on
            // differs. Telling a user to `qd send` to a session they could also
            // be WATCHING leaves the whole affordance unmentioned, which is why
            // `qd resume` prints the same second line.
            (quorum_qw::lane::Harness::Codex, quorum_qw::lane::Mode::AppServer) => {
                println!("Started detached codex session \"{name}\"");
                println!("Open a terminal on it with \"qd attach {name}\".");
            }
            // claude's PANE is excluded by the enclosing `if` — it has four phases
            // after the create and prints its own line down there, not here. Its ACP
            // sibling is NOT excluded: it renders through the `(_, Mode::Acp)` arm
            // above, like every other daemon-hosted create.
            (quorum_qw::lane::Harness::ClaudeCode, quorum_qw::lane::Mode::Pane) => {}
            // Not lanes; `Lane::new` refuses each of these, so `for_create` cannot
            // have produced one. Named rather than wildcarded so that making any
            // of them real is an edit somebody has to make HERE, on purpose.
            (quorum_qw::lane::Harness::Codex, quorum_qw::lane::Mode::Extension)
            | (quorum_qw::lane::Harness::Pi, quorum_qw::lane::Mode::AppServer)
            | (
                quorum_qw::lane::Harness::ClaudeCode,
                quorum_qw::lane::Mode::Daemon
                | quorum_qw::lane::Mode::Extension
                | quorum_qw::lane::Mode::AppServer,
            )
            | (
                quorum_qw::lane::Harness::Opencode,
                quorum_qw::lane::Mode::Pane
                | quorum_qw::lane::Mode::Daemon
                | quorum_qw::lane::Mode::Extension
                | quorum_qw::lane::Mode::AppServer,
            ) => {}
        }
        // R19: the pane lanes end where `qd attach <name>` would have. The
        // verdict line above still names that command, and stays: it is the
        // truth for the NEXT time, and it is what a `--no-attach` or scripted
        // caller — which is every caller that reaches it without attaching —
        // needs to read.
        if attach_after_start {
            return super::attach::attach_after_create(&name, render);
        }
        return 0;
    }

    // =======================================================================
    // The claude lane's four phases AFTER the create. None of them is a lane
    // operation; every one is qd's own.
    // =======================================================================

    let clock = RealClock;
    let sleeper = RealSleeper;
    let exec = RealExec;
    // The stable id the create MINTED, read off its return value. Before
    // `SessionHandle::qd_id` the only way to learn it was to have minted it — which
    // is what put the mint on the wrong side of this call for the four lanes whose
    // cores mint internally.
    let qd_session_id = handle.qd_id.clone().unwrap_or_default();
    let ids_path = match common::ids_store_path(&env) {
        Ok(p) => p,
        Err(code) => return code,
    };

    // Under --json the human line moves to stderr: stdout carries exactly one
    // machine object (the identity on success, the A-2 error object on a
    // bind-arm failure). Human output without the flag is byte-unchanged.
    if json_out {
        eprintln!("Started detached session \"{name}\"");
    } else {
        println!("Started detached session \"{name}\"");
    }

    // --- Lifecycle-collapse A-2: the FOURTH boot micro-phase — bind --------
    // Supersedes the P0 wave-2 warn-only bind-at-boot-confirm block: exit 0
    // now GUARANTEES the pre-minted stable id is bound to the live registry
    // row (spec D4). The four short-of-bound arms are ruled per F3 (see
    // `dispatch::bindphase`): NoneBindable/bind-Err retry to the budget —
    // pinned by reference to the existing boot-phase timeout, one knob
    // (BootTimeouts::pid_phase_ms) — Ambiguous and Diverged fail immediately.
    // A failure leaves the session RUNNING (killing it would not help; the I6
    // posture: say exactly what was left) and exits 1; the exit-code meanings
    // are unchanged (bind failure = "any other failure" = 1).
    let bound = {
        let timeouts = dispatch::boot::BootTimeouts::default();
        let alive = |pid: i64| dispatch::effects::is_pid_alive(pid as i32);
        dispatch::bindphase::run_bind_phase(
            &paths.sessions_dir,
            &ids_path,
            &name,
            &qd_session_id,
            &clock,
            &sleeper,
            &alive,
            timeouts.pid_phase_ms,
            timeouts.poll_ms,
        )
    };
    let bound = match bound {
        Ok(ok) => ok,
        Err(fail) => {
            let (pid, registry_id): (Option<i64>, Option<String>) = match &fail {
                dispatch::bindphase::BindPhaseFailure::Unbound { pid, last_bind_err } => {
                    let detail = last_bind_err
                        .as_ref()
                        .map(|e| format!(" (last id-store error: {e})"))
                        .unwrap_or_default();
                    eprintln!(
                        "qd start: session \"{name}\" booted but its stable id \
                         {qd_session_id} is still UNBOUND — the registry row never \
                         carried a sessionId within the boot-phase budget{detail}. \
                         The session IS RUNNING; inspect with `qd ls` or `qd attach \
                         {name}`. A caller may retry ONCE after stopping it."
                    );
                    (*pid, None)
                }
                dispatch::bindphase::BindPhaseFailure::Ambiguous { count } => {
                    eprintln!(
                        "qd start: {count} RUNNING sessions claim the name \"{name}\" — \
                         refusing to bind stable id {qd_session_id} to either, and \
                         NEVER retrying (retrying a duplicated name mints a third \
                         same-name session). The just-started session IS RUNNING; \
                         resolve the duplicate (`qd ls` + `qd stop`) before trusting \
                         name addressing."
                    );
                    (None, None)
                }
                dispatch::bindphase::BindPhaseFailure::Diverged {
                    registry_session_id,
                    existing_id,
                    pid,
                } => {
                    eprintln!(
                        "qd start: stable-id divergence for session \"{name}\": env \
                         carries {qd_session_id}, registry session \
                         {registry_session_id} already maps to {existing_id} — \
                         sessions disagree; `qd ls` will surface {existing_id}. The \
                         session IS RUNNING; operator attention required."
                    );
                    (*pid, Some(existing_id.clone()))
                }
            };
            if json_out {
                // The ruled machine error object (spec A-2): the prime recipe
                // branches on `class` — unbound → stop-and-retry once;
                // ambiguous → never retry; diverged → fail to operator.
                let mut ids = serde_json::json!({ "env": qd_session_id });
                if let Some(reg) = registry_id {
                    ids["registry"] = serde_json::Value::String(reg);
                }
                let obj = serde_json::json!({
                    "error": {
                        "class": fail.class(),
                        "session": { "name": name, "pid": pid },
                        "ids": ids,
                    }
                });
                println!("{obj}");
            }
            return 1;
        }
    };
    // --- Lifecycle-collapse A-1 (spec D4): machine-readable identity --------
    // Emitted only after the bind phase: exit 0 ⇒ the printed id is bound.
    if json_out {
        let live = bound
            .pid
            .map(|p| dispatch::effects::is_pid_alive(p as i32))
            .unwrap_or(true);
        let obj = serde_json::json!({
            "name": name,
            "qdId": qd_session_id,
            "sessionId": bound.session_id,
            "status": bound.status,
            "live": live,
        });
        println!("{obj}");
    }

    // --- Warranty belt (2026-06-11): warn at BIRTH if the session is born
    // transport-less. The new engine's relay is a user-scope MCP Claude Code
    // loads from `~/.claude.json` (registered by `qd relay:register`); if that
    // step was skipped (the P0-cutover failure class), every session boots with
    // no relay and the gap stays SILENT until the first `send:relay`. Catch it
    // here. CHEAP (a bounded file read + parse, NO `claude` subprocess),
    // NON-FATAL, never blocks or fails start: an unreadable/unparseable config or
    // a present registration ⇒ say nothing (no false nag); only a PERSISTENTLY
    // absent relay warns. Best-effort by the same contract as the binds above.
    //
    // WP-B (#2): the read is a BOUNDED STABLE-READ behind the `RelayPresence`
    // interceptor — `~/.claude.json` is the user-global file concurrent claude
    // sessions atomically rewrite ~10×/turn with no lock, so a lone read can land
    // in a lost-update window and see the relay transiently absent. The
    // stable-read re-reads across a bounded window and nags ONLY on a stable
    // (persistent) absence; a transient absent / instability ⇒ say nothing. This
    // hardens the EXISTING config-presence read (no new disk-as-status read, §6.0).
    {
        use dispatch::relay_presence::{RelayPresence, StableRelayPresence};
        let claude_json = home.join(".claude.json");
        if StableRelayPresence::for_claude_json(&claude_json)
            .check()
            .should_warn()
        {
            eprintln!(
                "WARNING: session \"{name}\" is up but no user-scope relay MCP is \
                 registered with Claude Code — `qd send:relay` to it will fail. \
                 Register once with `qd relay:register` (see doc/DEPLOY.md), then \
                 new sessions pick it up."
            );
        }
    }

    // --- A6 §4.2: telemetry create stamp (lead integration step) -------------
    // Placed AFTER successful boot-verify (the session is REAL — a failed boot
    // never stamps) and BEFORE the -p delivery branch (red-team F4: the -p path
    // returns early via map_deliver_outcome; a stamp below it would never run on
    // that path, and a Stalled exit-10 session DOES get its events — it booted).
    // Both appends are best-effort by contract (spec §4.1): a failure warns to
    // stderr and NEVER changes this verb's exit code.
    {
        // spawned_by: resolve the CREATING session via the shared ppid walk
        // (spec §4.2; absent — not empty — when the caller is a human shell).
        let creator = dispatch::telemetry::find_caller_session(&paths, &exec);
        // sessionId: SINGLE non-blocking read attempt of the new session's
        // registry entry — claude-code may not have written it yet (it owns
        // <pid>.json); absent is fine, the fold name-keys until then (§4.3).
        let session_id = dispatch::registry::read_entries(&paths.sessions_dir, false)
            .into_iter()
            .find(|s| s.entry.name.as_deref() == Some(name.as_str()))
            .and_then(|s| s.entry.session_id);
        let ev = dispatch::telemetry::CreateEvent {
            name: name.clone(),
            session_id: session_id.clone(),
            spawned_by: creator.as_ref().and_then(|c| c.name.clone()),
            spawned_by_session_id: creator.as_ref().and_then(|c| c.session_id.clone()),
            backend: via.clone(),
        };
        if let Err(e) = dispatch::telemetry::append_create_event(&env, &clock, &ev) {
            eprintln!("WARNING: telemetry create-event append failed (non-fatal): {e}");
        }
        // The uniform create-invoked line (spec §4.1 — all FOUR verbs, F9).
        if let Err(e) = dispatch::telemetry::append_invoked(
            &env,
            &clock,
            "create",
            session_id.as_deref(),
            Some(name.as_str()),
        ) {
            eprintln!("WARNING: telemetry invoked append failed (non-fatal): {e}");
        }
    }

    // --- §3.4 delivery: -p (post boot-ready) ---------------------------------
    // The session is created + idle here. `-p` runs the load-robust
    // deliver_prompt and PARTICIPATES in the went-busy exit contract (§3.5).
    //
    // warranty #2 (2026-06-11): the post-boot `/model <m>` delivery that used to
    // run HERE is REMOVED. `--model` is now a LAUNCH FLAG (build_new_extra_args,
    // a birth property of the session) — current Claude Code's `/model` slash
    // command PERSISTS as the shared global default ("saved as your default for
    // new sessions"), so the old delivery polluted the default a later plain
    // session would inherit, AND combined with `-p` it dropped the prompt (the
    // /model submit + settle left the composer such that the -p body never
    // landed → exit 10). As a launch flag the model is set before boot, touches
    // no shared state, and the -p path runs unencumbered. (Supersedes ADR 0009's
    // /model carve-out — see ADR 0009 superseding note.)

    // -p → deliver_prompt with DELIVER_TIMEOUT_S (15 — NOT send:pty's 120, N9).
    if let Some(p) = &prompt {
        // TWO RECORDS, ONE ID — ruling D11, and the ONE half of this send that is
        // qd's. `qd delivery:recover`'s sweep reads qd's INTENT log, so without the
        // record below a `new -p` priming send would silently drop out of recovery
        // altogether. It is written FIRST and hands its id to the body, because an
        // intent record can only correlate with an id that already exists (D10).
        //
        // Everything the send then DOES — the delivery-log `send-initiated` with
        // its recovery keys, the chunked type-through, `chunks-delivered`, the W8
        // verify and its two positive terminals, and the WatchGuard across both —
        // is `quorum_qw::delivery::priming`'s. It is qw-owned delivery work and it
        // was the last of it still running out of a qd verb; that module's header
        // records why it could not instead become a `LaneOps::start` or
        // `LaneOps::deliver` call.
        //
        // THE MUX AND THE SOCKET DIR. The mux is built HERE and only here, because
        // this send — not the create — is what still needs one on qd's side. The
        // socket dir is NOT re-resolved: it is `SessionHandle::socket_dir`, the
        // canonical dir the create actually landed in, carried back for exactly this
        // (a re-resolved `ZMX_DIR` may point elsewhere — ADR 0009, Bug D).
        //
        // events key: sessionId if resolvable NOW (a non-blocking registry read),
        // else byname(name) — the key choice is STICKY for ALL of this send's
        // events (§4.1), across BOTH logs, which is why it is resolved once HERE
        // and handed to the body rather than read again on the other side.
        let ev_session_id = dispatch::registry::read_entries(&paths.sessions_dir, false)
            .into_iter()
            .find(|s| s.entry.name.as_deref() == Some(name.as_str()))
            .and_then(|s| s.entry.session_id);
        let send_id = super::intent::record_send_intent(
            &env,
            &clock,
            ev_session_id.as_deref(),
            Some(&name),
            events::verb_str(true),
            p,
        );

        let backend = match common::select_backend(&env) {
            Ok(b) => b,
            Err(code) => return code,
        };
        let mux = match common::build_mux(backend, &home, &env) {
            Ok(m) => m,
            Err(code) => return code,
        };
        let Some(socket_dir) = handle.socket_dir.as_deref() else {
            // Unreachable: every pane create fills it. Refuse rather than guess a
            // dir — typing a prompt into the wrong pane is worse than not typing it.
            eprintln!(
                "qd start: session \"{name}\" was created but the lane reported no socket \
                 dir, so the -p prompt was not delivered. The session IS RUNNING; type \
                 the prompt after \"qd attach {name}\"."
            );
            return 1;
        };
        let deps = priming::PrimingDeps {
            env: &env,
            clock: &clock,
            sleeper: &sleeper,
            mux: mux.as_ref(),
            paths: &paths,
            home: &home,
            socket_dir,
        };
        let params = priming::PrimingParams {
            name: &name,
            prompt: p,
            session_id: ev_session_id.as_deref(),
            send_id: &send_id,
        };
        // `map_err` + print — the wrapper this whole split is made of. The notes
        // are the two degraded-verify WARNINGs; the refusal is the ONE loud
        // truncation error, and its exit code is the core's, not this verb's guess.
        return match priming::prime_new_session(&deps, &params) {
            Ok(primed) => {
                render_notes(&primed.notes);
                map_deliver_outcome(primed.deliver, &name)
            }
            Err(refused) => {
                render_notes(&refused.notes);
                if let Some(line) = refused.error.line("start") {
                    eprintln!("{line}");
                }
                refused.error.exit_code()
            }
        };
    }

    // R19: the claude lane's handoff. It is the LAST thing the verb does — after
    // the bind phase, the relay warranty check and the telemetry stamp — because
    // every one of those either guarantees or records something about a session
    // that must be true before a human is put inside it, and none of them can run
    // once this call has taken the terminal.
    //
    // Unreachable with a prompt: the `-p` block above returns unconditionally,
    // and `attaches_after_start` would answer `false` here anyway. Both, on
    // purpose — the exit contract of a `-p` start (§3.5) is the caller's answer
    // and an attach must not be able to overwrite it from either direction.
    if attach_after_start {
        return super::attach::attach_after_create(&name, render);
    }

    0
}

/// The LINE, the EXIT CODE and the BOOT PHASE a failed create reports.
///
/// Three facts, and the reason each one is carried rather than derived is on
/// [`quorum_qw::contract::LaneError::StartFailed`]. What is here is only the
/// fallback: `start` answers `StartFailed` for every real create failure, and the
/// other arm exists because two of `LaneError`'s variants are still reachable —
/// `NotSupported` from a lane combination that does not exist, and `Transport` from
/// a lane that could not be opened at all. Printing a `{e:?}` for either would put
/// a Rust enum in front of a user.
fn start_failure(
    e: &quorum_qw::contract::LaneError,
) -> (String, i32, Option<dispatch::boot::BootPhase>) {
    match e {
        quorum_qw::contract::LaneError::StartFailed {
            detail,
            exit_code,
            boot_phase,
        } => (detail.clone(), *exit_code, *boot_phase),
        other => (format!("qd start: {other}"), 1, None),
    }
}

// `bind_minted_id_best_effort` lived HERE. It is GONE, and it moved rather than
// died: it is `quorum_qw::lanes::LaneImpl::bind_minted_id_best_effort` now, in
// the claude create arm, because it is the MINTER's repair. The verb no longer
// holds the ids-store path or the pre-minted id — the create mints internally and
// reports the id back on `SessionHandle::qd_id` — and neither survives on the
// error a failed create hands back. It stayed silent across the move: the loud
// boot error still owns stderr, byte-stable.

// codex-interactive, use case 2: the viewer PANE NAME moved to
// `dispatch::provider::codex::pane::viewer_pane_name`, together with the
// `reap_viewer_pane` that stage-2 phase 3 handed to the codex lane's `kill` — a
// viewer is a codex affordance, and both halves of it belong on the same side of
// the boundary. Its war story travelled with it: qrmux's pane-name charset leaves
// no separator a session name could not also contain, so pane REUSE is gated on
// the pane's COMMAND matching our viewer argv and NEVER on its name (see
// [`attach_codex_viewer`]).
use dispatch::provider::codex::pane::viewer_pane_name;

/// codex-interactive, use case 2: attach a human TUI to a LIVE daemon-hosted
/// codex session, WITHOUT stopping or converting it.
///
/// THE MECHANISM (verified live against codex-cli 0.146.1). The codex TUI is
/// itself an app-server client — `codex --remote <ws-url>` points it at an
/// EXISTING app server instead of bootstrapping its own. qd already spawns
/// exactly such a server per daemon session and records its address in the row's
/// `endpoint` (`ws://127.0.0.1:<port>`, a loopback ws URL, which is the form
/// `--remote` accepts). So the human's TUI and the agent's RPC client become two
/// clients of ONE app server, driving ONE thread.
///
/// Proven end to end before this was written: an agent-side `qd send` drove a
/// turn over RPC, then `codex --remote <endpoint> resume <thread-id>` rendered
/// that exact exchange in a TUI; only one app-server process ever existed (ours);
/// and the daemon row was still idle with its thread id afterwards.
///
/// WHY THIS BEATS STOP-AND-CONVERT. The obvious way to give a human a terminal on
/// an agent's session is to stop the daemon and relaunch the thread as a TUI pane.
/// That costs the agent its session and permanently changes the row's topology —
/// a debugging action with side effects on the thing being debugged. Here nothing
/// stops, nothing converts, and the agent keeps driving throughout.
///
/// THE VIEWER IS NOT A SESSION. It gets a mux pane (so it can be detached and
/// re-attached, over SSH or from a phone, like any other qrmux pane) but NO
/// registry row: it owns no thread, has no identity, and its death means nothing.
/// A second `qd attach` reuses a live viewer rather than stacking another.
///
/// Returns `None` when this row cannot host a viewer (no endpoint or no thread
/// id) so the caller falls back to the ordinary daemon redirect.
pub fn attach_codex_viewer(session: &Session) -> Option<i32> {
    let env = RealEnv;
    let name = session.name.as_deref().filter(|n| !n.is_empty())?;
    let thread_id = Some(session.session_id.as_str()).filter(|s| !s.is_empty())?;
    // The endpoint lives on the registry row, not the joined Session — re-read it
    // by pid, the same way the wait verb resolves the live channel.
    let paths = common::paths_from_home(&env).ok()?;
    let endpoint = session
        .pid
        .filter(|&p| p != 0)
        .and_then(|pid| dispatch::registry::read_entry(&paths.sessions_dir, pid))
        .and_then(|e| e.endpoint)
        .filter(|s| !s.is_empty())?;

    let backend = common::select_backend(&env).ok()?;
    let (canonical, _legacy) = resolve_create_dirs(backend, &paths.home, &env).ok()?;
    let mux = match common::build_mux(backend, &paths.home, &env) {
        Ok(m) => m,
        Err(code) => return Some(code),
    };
    let pane = viewer_pane_name(name);

    // Reuse a live viewer: attaching twice should land in the same window, not
    // stack a second TUI on the same thread.
    //
    // Identity is by NAME, with a guard — and it has to be, because the embedded
    // mux cannot tell us more. qrmux's `SessionInfo` carries no command line, so
    // `MuxSession::cmd` is synthesized EMPTY under the default backend; a
    // "does this pane run our argv?" check would be dead code that silently never
    // matched (and it did: it re-created every time, then failed on the taken
    // name).
    //
    // THE GUARD closes the case a name check alone would get wrong. Nothing but
    // this function creates a `<name>.view` pane, but a user COULD have started a
    // real session literally called that — and handing them its terminal when
    // they asked for a viewer on `<name>` would be a silent wrong-window. A real
    // session has a live REGISTRY ROW; a viewer never does. So: pane present +
    // no row claiming that name ⇒ ours.
    let pane_present = mux
        .list(&canonical)
        .unwrap_or_default()
        .into_iter()
        .any(|z| z.name == pane);
    let claimed_by_a_real_session = dispatch::registry::read_entries(&paths.sessions_dir, false)
        .into_iter()
        .any(|s| {
            !s.tombstoned
                && s.entry.name.as_deref() == Some(pane.as_str())
                && s.entry
                    .pid
                    .is_some_and(|p| p != 0 && dispatch::effects::is_pid_alive(p as i32))
        });
    if pane_present && claimed_by_a_real_session {
        eprintln!(
            "qd attach: cannot open a viewer on \"{name}\" — a live session is already \
             named \"{pane}\", which is the name a viewer needs. Rename or stop it, or \
             attach to \"{pane}\" directly if that is what you meant."
        );
        return Some(1);
    }
    let already_live = pane_present;

    if !already_live {
        // argv = `codex --remote <ws endpoint> resume <thread-id>`. `--remote`
        // binds the TUI to OUR app server; `resume <id>` selects the agent's
        // thread on it (an explicit UUID bypasses codex's session picker, which
        // by default hides non-interactive sessions — exactly the kind an agent
        // creates).
        let argv = vec![
            dispatch::provider::codex::codex_bin(&env),
            "--remote".to_string(),
            endpoint.clone(),
            "resume".to_string(),
            thread_id.to_string(),
        ];
        let cmd = dispatch::launch::build_claude_cmd_from_argv(&argv);
        let cwd = session
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        match mux.run_detached(&canonical, &pane, &cmd, &cwd) {
            Ok(res) if res.status == Some(0) => {}
            Ok(res) => {
                eprintln!(
                    "qd attach: could not open a viewer on \"{name}\" (exit {:?}): {}",
                    res.status,
                    res.stderr.trim()
                );
                return Some(1);
            }
            Err(e) => {
                eprintln!("qd attach: could not open a viewer on \"{name}\": {e}");
                return Some(1);
            }
        }
        println!("Opened a viewer on \"{name}\" (thread {thread_id}); attaching...");
    }

    Some(match mux.attach(&canonical, &pane) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("qd attach: {e}");
            1
        }
    })
}

/// Stamp the CLI verb onto a [`CodexTuiError`] (and, below, a [`PiTuiError`]).
///
/// The pre-split `create_codex_tui`/`revive_codex_tui` threaded a `verb: &str`
/// through every `eprintln!` so one body could say `qd start:` / `qd resume:` /
/// `qd attach:` / `qd send:` depending on the caller. The verb names the command
/// the USER typed, so it stayed on this side of the split when the choreography
/// moved to `dispatch::provider::codex::pane`: the error variants carry the facts,
/// this formatter carries the attribution.
///
/// The `Create` arm is printed VERBATIM. Its inner `create::NewError` is already
/// attributed (`qd start: …` / `ERROR: …`) and the pre-split verb printed it with
/// a bare `eprintln!("{e}")` from EVERY caller — so re-stamping it here would
/// change a line the move is meant to leave byte-identical.
pub(super) fn codex_tui_failure_line(verb: &str, e: &CodexTuiError) -> String {
    if e.is_self_attributed() {
        format!("{e}")
    } else {
        format!("qd {verb}: {e}")
    }
}

// `revive_codex_tui` — the verb-layer codex-pane revive wrapper — lived HERE,
// and is DELETED rather than left unused, exactly as its pi twin `revive_pi_tui`
// was one lane over. The note below `revive_pi_tui` used to say this one
// survived ONLY because `send_unified::RealWaker` called it; `RealWaker` is gone
// with the rest of qd's duplicated routing, so the last caller went with it.
// [`quorum_qw::contract::LaneOps::wake`]'s `(Codex, Pane)` arm drives the SAME
// `dispatch::provider::codex::pane::revive_codex_tui` core this wrapped.

// ===========================================================================
// pi-interactive — the mux-pane pi TUI lane (`--provider pi --interactive`).
// ===========================================================================

/// Stamp the CLI verb onto a [`PiTuiError`] — the pi twin of
/// [`codex_tui_failure_line`], and self-attributed `Create` errors are printed
/// verbatim for the same reason. See that function's doc.
pub(super) fn pi_tui_failure_line(verb: &str, e: &PiTuiError) -> String {
    if e.is_self_attributed() {
        format!("{e}")
    } else {
        format!("qd {verb}: {e}")
    }
}

// `revive_pi_tui` — the verb-layer pi-pane revive wrapper — lived HERE, and is
// DELETED rather than left unused: `qd resume` was its last caller, and
// [`quorum_qw::contract::LaneOps::wake`]'s `(Pi, Pane)` arm is what that verb calls
// now. Its codex twin `revive_codex_tui` is gone too, one stage later: it
// survived only while `send_unified::RealWaker` still called it, and that waker
// died with qd's duplicated routing. The two refusals it ran ahead of dep
// resolution —
// the ORDER pin its doc named — are kept, in `verbs/resume.rs`, ahead of the lane
// call.

// The FIVE per-lane create wrappers lived here — `run_new_codex_tui`,
// `run_new_pi_tui`, `run_new_codex_daemon`, `run_new_pi_daemon`,
// `run_new_acp_daemon` — plus the two adapters and the `OwnedPaneDeps` bundle
// that fed them. All DELETED, on the same reasoning that retired
// `revive_codex_tui` and `revive_pi_tui` one stage earlier: their bodies were
// effect assembly a library can own, and `quorum_qw::lanes`' seven create arms
// own it now. `qd start` calls `LaneOps::start` once and renders the answer.
//
// What did NOT move is in `run_new` where it always was: the per-lane `-p`
// notices (a lane has no user to talk to), the six success lines, and every
// refusal that has to fire before a create is attempted.

/// §3.5 WENT-BUSY EXIT CONTRACT (HARDENING #3, ADR 0008): map the three-way
/// [`DeliverOutcome`] of an `qd new -p` delivery to the sanctioned exit codes.
///
///   - [`DeliverOutcome::Accepted`] → **0**, stdout `Prompt delivered to "<n>"`.
///   - [`DeliverOutcome::Stalled`]  → **10**, stderr the WARNING block (the
///     session EXISTS; the turn-start is unconfirmed) — the deliberate divergence
///     from TS's always-0 (lifecycle.ts:921-931) so an external spawn caller can
///     tell "made, not running" apart from "made + running".
///   - [`DeliverOutcome::PidFileMissing`] → **1** (R1: a vanished PID file is an
///     INFRA failure, NOT a stall; routing it to 10 would lie to that caller).
///
/// The TS WARNING wording (lifecycle.ts:923-930) is ported verbatim for the
/// Stalled branch; PidFileMissing gets its own infra-distinguishing stderr.
fn map_deliver_outcome(outcome: dispatch::submit::DeliverOutcome, name: &str) -> i32 {
    use dispatch::submit::DeliverOutcome;
    match outcome {
        DeliverOutcome::Accepted => {
            println!("Prompt delivered to \"{name}\"");
            0
        }
        DeliverOutcome::Stalled => {
            // VERBATIM TS WARNING block (qa/hardening@3dd9f1e:src/commands/
            // lifecycle.ts:923-930) — the session exists, the prompt may sit
            // unsubmitted in the composer. Exit 10 is the Rust-only contract.
            eprintln!(
                "WARNING: Prompt sent to \"{name}\" but session did not go busy.\n\
                 The prompt may be in the composer but not submitted.\n  \
                 Attach: qd attach {name}"
            );
            10
        }
        DeliverOutcome::PidFileMissing => {
            // R1: NOT a stall — the registry row vanished post-boot (infra). Exit
            // 1, with stderr that distinguishes it from the Stalled (10) case.
            eprintln!(
                "ERROR: Prompt sent to \"{name}\" but the session's PID file vanished \
                 after boot — the registry row disappeared (infra failure, not a stall).\n  \
                 Check: qd ls\n  Attach: qd attach {name}"
            );
            1
        }
    }
}

/// F1 capture + `--via` composition for `qd new` (spec §2.2 + §3.2). Returns the
/// composed backend-env key set create.rs writes to the 0600 self-deleting file,
/// or an exit code on a loud failure (the loud message is already printed).
///
/// Without `--via`: just the caller capture (lifecycle.ts:874). With `--via`:
///   1. the name passes an UNCONDITIONAL S2-style charset check (fresh surface —
///      never ruling-dependent; the name reaches the unknown-name error string),
///   2. read+parse `<qdHome>/state/backends.json` (O_NOFOLLOW, perm WARN, §3.1),
///   3. resolve the profile (loud unknown-name listing known names only, §3.2.2),
///   4. compose profile-wins + credential-slot exclusivity (§3.2.3), resolving a
///      `secret` credential via the name-agnostic `get_secret` (keychain→file,
///      NO env-override tier — §3.2.3 / red-team F3/F10).
///
/// The credential VALUE flows ONLY through the returned vec (then the 0600 file);
/// it never touches argv, never any error/log string here.
/// A7 F12: (composed backend-env pairs, whitelist keys to `unset -v` in the
/// env file). The unset list is empty for non-`--via` sessions (F1 parity).
type BackendEnvComposition = (Vec<(String, String)>, Vec<String>);

fn compose_backend_env(
    env: &RealEnv,
    home: &std::path::Path,
    via: Option<&str>,
) -> Result<BackendEnvComposition, i32> {
    // F1: capture the caller's whitelisted backend env (injected Env seam, L9a).
    let captured = capture_backend_env(env);

    let Some(via_name) = via else {
        // no --via → plain capture (byte-zero when empty), NO unsets: the F1
        // surface stays TS-parity (utils.ts:668-680 sources only, never unsets).
        return Ok((captured, Vec::new()));
    };

    // The --via name must pass charset validation BEFORE any use (spec §3.2;
    // unconditional fresh surface). Reuse the ported S2 guard so the rule and
    // message match the session-name S2 exactly.
    if let Some(msg) = dispatch::resume::validate_session_name(via_name) {
        eprintln!("qd start --via: invalid backend name: {msg}");
        return Err(1);
    }

    // Resolve <qdHome>/state via QD_HOME-honoring paths (same as marks.jsonl);
    // independent of the create path's own `paths` so nothing else changes.
    let state_paths = dispatch::paths::QdPaths::from_home_env(home, env);
    let file_path = dispatch::backends::backends_file_path(&state_paths.state_dir);

    let (file, perm_warning) = match dispatch::backends::read_backends_file(&file_path) {
        Ok(ok) => ok,
        Err(e) => {
            eprintln!("{e}");
            return Err(1);
        }
    };
    if let Some(w) = perm_warning {
        eprintln!("{}", w.message());
    }

    let backend = match file.resolve(via_name) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            return Err(1);
        }
    };

    // The secret resolver: the name-agnostic get_secret (keychain→file, ADR-0010
    // locked-fallback intact). NO env-override tier (profile-wins determinism).
    let composed = with_real_secret_deps(env, |deps| {
        dispatch::backends::compose_via_env(via_name, &captured, &backend, &|key| {
            dispatch::secrets::get_secret(key, deps)
        })
    });
    match composed {
        Ok(set) => {
            // A7 F12 fix: for a --via session the child's whitelisted env must be
            // EXACTLY the composed set. Whitelist keys NOT in the set get explicit
            // `unset -v` lines in the env file — removal from the composed pairs
            // alone leaves the caller-inherited/profile-re-exported value riding
            // (observed live on Lima 2026-06-05: real caller ANTHROPIC_API_KEY
            // reached a profile-secret child; macOS scenario leg was vacuously
            // green because no caller key was exported).
            let unset: Vec<String> = dispatch::launch::BACKEND_ENV_KEYS
                .iter()
                .filter(|k| !set.iter().any(|(sk, _)| sk == *k))
                .map(|k| k.to_string())
                .collect();
            Ok((set, unset))
        }
        Err(e) => {
            // e.g. SecretMissing — names the key + `qd config set` hint, no value.
            eprintln!("{e}");
            Err(1)
        }
    }
}

/// Build a real [`dispatch::secrets::SecretDeps`] over the production seams and run
/// `f`. Mirrors `survey.rs`/`config.rs`'s real-fs closures (chmod-600 file
/// backend, per-process notice guards). The `--via` path only ever READS, but
/// the locked-keychain fallback may file-read, so the standard real closures are
/// used. The secret value returned by `f` is held only in memory.
fn with_real_secret_deps<R>(
    env: &RealEnv,
    f: impl FnOnce(&dispatch::secrets::SecretDeps) -> R,
) -> R {
    use std::fs;
    use std::sync::atomic::AtomicBool;
    let exec = RealExec;
    let notice = AtomicBool::new(false);
    let locked_diag = AtomicBool::new(false);

    let read_file = |p: &str| fs::read_to_string(p).ok();
    let write_file = |p: &str, text: &str| {
        if let Some(parent) = std::path::Path::new(p).parent() {
            let _ = fs::create_dir_all(parent);
        }
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut fh) = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(p)
        {
            use std::io::Write;
            let _ = fh.write_all(text.as_bytes());
        }
    };
    let chmod = |p: &str, mode: u32| {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(p, fs::Permissions::from_mode(mode));
    };
    let file_exists = |p: &str| std::path::Path::new(p).exists();
    let keychain_available = || dispatch::secrets::real_keychain_available(&exec);
    let deps = dispatch::secrets::SecretDeps {
        platform: std::env::consts::OS,
        env,
        exec: &exec,
        keychain_available: &keychain_available,
        read_file: &read_file,
        write_file: &write_file,
        chmod: &chmod,
        file_exists: &file_exists,
        fallback_notice_emitted: &notice,
        locked_diag_emitted: &locked_diag,
    };
    f(&deps)
}

// --- info (commands/status.ts:560-662) ---

/// `qd info <session>` — A1 render/jsonl per commands/status.ts:561-660 field order. The
/// OpenCode lastTurns fetch branch is skipped (parked); unknown session →
/// resolveOrDie error + exit 1.
///
/// P0 spec-w8: `--json` prints ONE json object (render::info_json — the
/// point-resolution surface an outside consumer joins against) instead of the human text.
/// Resolution is the standard pipeline UNCHANGED — the not-found/ambiguity
/// error paths above the branch stay loud on stderr + exit 1 and emit NO json.
pub fn run_info(m: &ArgMatches) -> i32 {
    let query = m.get_one::<String>("session").expect("required by clap");
    let json = m.get_flag("json");

    // info resolves through the sealed uncapped entry (include_preview=true to
    // render the preview turns, commands/status.ts:564-567) and reuses the gathered
    // LIST for the qdId shortest-unique prefix below — exactly as ls does. As a READ
    // verb it never rejects a tombstone: showing info about a stopped session is its
    // job (the cap axes stay hardcoded in the sealed entry, so info stays uncapped).
    let (session, sessions, health) = match common::resolve_session_uncapped_in_list_with_health(
        query, true,
    ) {
        Ok(pair) => pair,
        Err(code) => return code,
    };
    let session = &session;

    if json {
        // qdIdPrefix: shortest-unique among the resolved session LIST, the
        // same computation ls uses (idstore::prefix_map).
        let prefixes = dispatch::idstore::prefix_map(&sessions);
        // live: status-live AND pid-alive-where-a-pid-exists, through the real
        // is_pid_alive effects seam (semantics + pins: resolve::is_live_with_pid).
        let alive = |p: i32| dispatch::effects::is_pid_alive(p);
        let live = dispatch::resolve::is_live_with_pid(session.status, session.pid, &alive);
        println!(
            "{}",
            dispatch::render::to_pretty(&dispatch::render::info_json(session, &prefixes, live))
        );
        return 0;
    }

    let now = RealClock.now_ms();
    // A6 §4.4: load the telemetry fold BEST-EFFORT (any read error → empty map)
    // and pass it. Empty fold ≡ today's bytes exactly (additive-only, G-A1).
    let fold = dispatch::telemetry::fold_from_env(&RealEnv);
    let fold_ref = if fold.is_empty() { None } else { Some(&fold) };
    print!(
        "{}",
        dispatch::render::info_text_full(session, now, fold_ref, &health)
    );
    0
}

// --- live (commands/status.ts:394-556) ---

/// `qd live` — the 2s-refresh TUI + 3-char-code keystroke→attach (commands/status.ts:395-
/// 560). REAL interactive path for a TTY; for non-TTY, TS crashes with a Bun
/// ReferenceError (corpus 33-*). We do NOT replicate the stack trace: print the
/// header then a clean one-line error to stderr and exit 1.
//
// parity exclusion pending lead ruling — TS crashes non-TTY (corpus 33-*)
pub fn run_live(m: &ArgMatches) -> i32 {
    let all = m.get_flag("all");

    let is_tty = unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
        && unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };

    if !is_tty {
        // TS header to stdout (the clear + title, commands/status.ts:433-434), then a clean
        // error to stderr (NOT the Bun stack trace) + exit 1.
        print!("\x1b[2J\x1b[H");
        println!("qd live  type code to attach · q to quit\n");
        eprintln!("qd live: requires an interactive terminal (TTY).");
        return 1;
    }

    // Interactive: render loop (2s) + raw-mode keystroke capture. Implemented with
    // std + termios via libc (no new crate dep). 3-char [a-z0-9] code → attach;
    // q / Ctrl-C → quit (commands/status.ts:525-555).
    run_live_interactive(all)
}

fn run_live_interactive(all: bool) -> i32 {
    use std::io::Read;
    use std::time::{Duration, Instant};

    // Enter raw mode (port of process.stdin.setRawMode(true), commands/status.ts:413).
    let mut termios = match RawMode::enter() {
        Some(t) => t,
        None => {
            eprintln!("qd live: could not enter raw terminal mode.");
            return 1;
        }
    };

    let opts = JoinOpts {
        include_all: all,
        include_tombstoned: all,
        include_preview: false,
        limit: Some(if all { 50 } else { 20 }),
    };

    let mut last_refresh = Instant::now()
        .checked_sub(Duration::from_secs(3))
        .unwrap_or_else(Instant::now);
    let mut code_buf = String::new();
    let mut current: Vec<Session> = Vec::new();
    let mut stdin = std::io::stdin();

    loop {
        if last_refresh.elapsed() >= Duration::from_secs(2) {
            current = common::all_sessions(opts).unwrap_or_default();
            print!("\x1b[2J\x1b[H");
            println!("qd live  type code to attach · q to quit\n");
            if current.is_empty() {
                println!("No sessions found.");
            } else {
                print!("{}", super::ls::render_table_for_live(&current));
            }
            use std::io::Write;
            let _ = std::io::stdout().flush();
            last_refresh = Instant::now();
        }

        // Non-blocking-ish read with a short timeout via poll on stdin fd.
        let mut buf = [0u8; 1];
        if poll_stdin(250) {
            if stdin.read(&mut buf).unwrap_or(0) == 0 {
                continue;
            }
            let key = buf[0];
            // q or Ctrl-C → quit (commands/status.ts:528).
            if key == b'q' || key == 0x03 {
                termios.restore();
                print!("\x1b[2J\x1b[H");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                return 0;
            }
            // Collect [a-z0-9] for short-code selection (commands/status.ts:542-554).
            let ch = (key as char).to_ascii_lowercase();
            if ch.is_ascii_alphanumeric() {
                code_buf.push(ch);
                if code_buf.len() >= 3 {
                    let sel = code_buf.clone();
                    code_buf.clear();
                    if let Some(match_) = current.iter().find(|s| s.code.as_deref() == Some(&sel)) {
                        let m = match_.clone();
                        termios.restore();
                        print!("\x1b[2J\x1b[H");
                        let label = m.name.clone().unwrap_or_else(|| m.session_id.clone());
                        if m.status == SessionStatus::Cold {
                            // W2: resume IS first-class now (no longer "not supported").
                            // The live picker runs in raw termios mid-loop; rather than
                            // re-enter the full revive choreography from here, point the
                            // user at the dedicated verbs — `qd attach` for the human
                            // revive+attach, `qd resume` for the agent cold-revive.
                            print!("\x1b[2J\x1b[H");
                            eprintln!(
                                "Session \"{label}\" is cold (not running). Revive it with:\n  \
                                 qd attach {label}    (human: revive and attach)\n  \
                                 qd resume {label}    (agent: revive to drivable)"
                            );
                            return 1;
                        }
                        println!("Attaching to \"{label}\"...");
                        return attach_session(&m);
                    }
                }
            }
        }
    }
}

/// Attach to a resolved live session from the live picker (the legacy picker
/// helper's claude-code path).
fn attach_session(s: &Session) -> i32 {
    let Some(zmx_name) = s.zmx_name.as_deref() else {
        eprintln!(
            "Session \"{}\" is not in zmx.",
            s.name.as_deref().unwrap_or(&s.session_id)
        );
        return 1;
    };
    let env = RealEnv;
    let canonical = resolve_zmx_dir(&env);
    let dir = s
        .socket_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or(canonical);
    // Backend-selected mux (C1 D3). An attachable session carries its socket_dir
    // (tagged by the backend's list), so the embedded lane targets the qrmux dir.
    let mux = match common::real_mux() {
        Ok(m) => m,
        Err(code) => return code,
    };
    match mux.attach(&dir, zmx_name) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("qd live: {e}");
            1
        }
    }
}

// --- raw-mode terminal handling (termios via libc; no new crate dep) ---

struct RawMode {
    orig: libc::termios,
    active: bool,
}

impl RawMode {
    fn enter() -> Option<RawMode> {
        unsafe {
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut orig) != 0 {
                return None;
            }
            let mut raw = orig;
            // cfmakeraw-lite: disable canonical mode + echo (status.ts setRawMode).
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(RawMode { orig, active: true })
        }
    }

    fn restore(&mut self) {
        if self.active {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.orig);
            }
            self.active = false;
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Poll stdin for readable data with a timeout (ms). Returns true if readable.
fn poll_stdin(timeout_ms: i32) -> bool {
    unsafe {
        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        libc::poll(&mut pfd, 1, timeout_ms) > 0 && (pfd.revents & libc::POLLIN) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::map_deliver_outcome;
    use dispatch::submit::DeliverOutcome;

    // -------------------------------------------------------------------------
    // FTUE punch R20: the detector's vocabulary and the parser's must agree.
    // -------------------------------------------------------------------------

    /// Every harness `qd setup` can DETECT maps to a `--provider` spelling this
    /// engine can START — and to the RIGHT one. Deriving the string from
    /// `Harness::provider_id` makes the first half structural; the second half
    /// is what this test is actually for, because a crossed arm
    /// (`HarnessId::Pi => Harness::Codex`) yields a perfectly valid provider id
    /// for the wrong harness, and the user would get codex from a menu row
    /// labelled pi.
    ///
    /// The link between the two vocabularies is the program name: the detector
    /// finds `pi` on PATH and the router calls that lane `pi`; `claude` becomes
    /// `claude-code`; `opencode` becomes `acp/opencode`. Containment is the
    /// strongest relation that holds across all four, and it is enough to catch
    /// every crossing — no harness's program name is a substring of another's
    /// provider id.
    ///
    /// FIX-SHAPED MUTATION: swap any two arms of `harness_for_detected` and this
    /// REDs on the containment assert.
    #[test]
    fn provider_ids_for_detected_harnesses_are_all_startable() {
        use dispatch::setup::harness::HarnessId;
        for id in HarnessId::ALL {
            let provider_id = super::provider_id_for_harness(*id);
            assert!(
                quorum_qw::lane::Harness::from_provider_id(provider_id).is_some(),
                "R20 would offer `--provider {provider_id}` for {id:?}, which start refuses",
            );
            assert!(
                provider_id.contains(id.as_str()),
                "R20 would label `--provider {provider_id}` as {id:?} — a crossed arm in \
                 harness_for_detected",
            );
        }
    }

    /// The fallback is still claude-code, and it is still a provider the engine
    /// accepts — R20 changed when the default is REACHED (never, at a terminal,
    /// without being offered the alternatives), not what it is.
    #[test]
    fn the_default_provider_is_a_real_provider() {
        assert_eq!(super::DEFAULT_PROVIDER, "claude-code");
        assert!(
            quorum_qw::lane::Harness::from_provider_id(super::DEFAULT_PROVIDER).is_some()
        );
    }

    // -------------------------------------------------------------------------
    // punch item 11: fork-target transcript preflight.
    // -------------------------------------------------------------------------
    use super::fork_transcript_missing_error;
    use dispatch::model::{Session, SessionBranch, SessionStatus};

    /// A minimal fork-target Session fixture (the ls.rs pattern).
    fn fork_target(sid: &str, cwd: Option<&str>, status: SessionStatus) -> Session {
        Session {
            name: Some("src".to_string()),
            user_named: Some(true),
            session_id: sid.to_string(),
            code: None,
            qd_id: None,
            pid: None,
            status,
            zmx_name: None,
            zmx_clients: None,
            socket_dir: None,
            relay_port: None,
            turns: 0,
            tokens: 0,
            cwd: cwd.map(String::from),
            last_active_ms: None,
            version: None,
            started_at_ms: None,
            git_branch: None,
            jsonl_path: None,
            last_turns: None,
            provider: "claude-code".to_string(),
            entrypoint: None,
            lineage: None,
            hosting: None,
            which_branch: SessionBranch::ColdJsonl,
        }
    }

    /// Tombstoned/stopped WITH a transcript on disk = a LEGAL fork (the fork
    /// reads the transcript, not the process). Status-blind by design.
    #[test]
    fn fork_preflight_tombstoned_with_transcript_is_legal() {
        let home = tempfile::tempdir().unwrap();
        let paths = dispatch::paths::QdPaths::from_home(home.path());
        // Plant the transcript where the claude provider seam resolves it
        // (projects_dir scan tier — any project subdir holding <sid>.jsonl).
        let proj = paths.projects_dir.join("-work-proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("uuid-tomb.jsonl"), "{}\n").unwrap();
        let target = fork_target("uuid-tomb", Some("/work/proj"), SessionStatus::Killed);
        let err =
            fork_transcript_missing_error(&dispatch::provider::ClaudeProvider, &paths, &target);
        assert_eq!(err, None, "tombstoned-with-transcript must stay forkable");
    }

    /// Transcript-less target → immediate teaching error naming the target, the
    /// sid, the searched root, and what to do. (The call site returns exit 1
    /// BEFORE any mint/claim/launch — no boot wait can ever start.)
    /// F4 (red-team r1): an EXISTING-but-unreadable projects root must NOT
    /// produce the false "no transcript exists" claim — the preflight SKIPS
    /// (fail-open, pre-B1 behavior) rather than refuse with a lie. Unix-only
    /// (chmod-000 semantics; root bypasses DAC so skip under uid 0).
    #[cfg(unix)]
    #[test]
    fn fork_preflight_fails_open_on_unreadable_projects_root() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores mode bits — the staging is meaningless.
        }
        let home = tempfile::tempdir().unwrap();
        let paths = dispatch::paths::QdPaths::from_home(home.path());
        std::fs::create_dir_all(&paths.projects_dir).unwrap();
        std::fs::set_permissions(&paths.projects_dir, std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let target = fork_target("uuid-hidden", Some("/work/proj"), SessionStatus::Cold);
        let err =
            fork_transcript_missing_error(&dispatch::provider::ClaudeProvider, &paths, &target);
        // Restore perms FIRST so the tempdir can clean up even if we assert-fail.
        std::fs::set_permissions(&paths.projects_dir, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(
            err, None,
            "an unreadable root must fail OPEN (skip), never claim 'no transcript exists'"
        );
    }

    #[test]
    fn fork_preflight_transcript_less_is_teaching_error() {
        let home = tempfile::tempdir().unwrap();
        let paths = dispatch::paths::QdPaths::from_home(home.path());
        let target = fork_target("uuid-ghost", Some("/work/proj"), SessionStatus::Cold);
        let msg =
            fork_transcript_missing_error(&dispatch::provider::ClaudeProvider, &paths, &target)
                .expect("transcript-less fork must be refused");
        // Error-text pins: target display name, sid, searched root, guidance.
        assert!(msg.contains("qd start: cannot fork \"src\""), "{msg}");
        assert!(
            msg.contains("no transcript exists for session id uuid-ghost"),
            "{msg}"
        );
        assert!(
            msg.contains(&paths.projects_dir.display().to_string()),
            "names the searched root: {msg}"
        );
        assert!(msg.contains("Pick another --fork source"), "{msg}");
        assert!(msg.contains("start fresh without --fork"), "{msg}");
    }

    // §3.5 exit contract: the three-way DeliverOutcome → exit mapping, all three
    // ways (ADR 0008). Pure return-code assertions; the stdout/stderr wording is
    // covered by the golden new_went_busy_exit.sh scenario (M4 Level 2).
    #[test]
    fn deliver_outcome_accepted_is_0() {
        assert_eq!(map_deliver_outcome(DeliverOutcome::Accepted, "wk"), 0);
    }

    #[test]
    fn deliver_outcome_stalled_is_10() {
        assert_eq!(map_deliver_outcome(DeliverOutcome::Stalled, "wk"), 10);
    }

    #[test]
    fn deliver_outcome_pidfile_missing_is_1_not_10() {
        // R1: a vanished PID file is infra (exit 1), explicitly NOT the stalled
        // code 10 — routing it to 10 would lie to an external spawn caller.
        assert_eq!(map_deliver_outcome(DeliverOutcome::PidFileMissing, "wk"), 1);
    }

    // -------------------------------------------------------------------------
    // §5.1 / G3 — priming-readiness-timeout emission (M3). The three rows that
    // drove `emit_priming_timeout` FOLLOWED it into
    // `quorum_qw::delivery::priming` — the emitter is qw's now, and a test that
    // asserts what landed in the delivery log belongs on the side that writes it.
    // The row below stays: it asserts `NewError`'s HUMAN surface, which is the
    // create seam's and is read by this verb.
    // -------------------------------------------------------------------------
    use dispatch::boot::BootPhase;
    use dispatch::create::NewError;

    /// m-4 ZERO-SURFACE ASSERT (orc rider, ack3-spec §8): adding the typed `phase`
    /// to `NewError::BootTimeout` must NOT change the human surface. The Display
    /// output is byte-identical for BOTH phases (phase is not printed), and the
    /// create-path exit code stays 1.
    #[test]
    fn boot_timeout_display_byte_identical_both_phases_exit_unchanged() {
        let expected = "ERROR: Session \"wk\" did not reach idle state within timeout.\n\
                        The zmx session exists but Claude Code may not have booted.\n  \
                        Check: qd ls\n  Attach: qd attach wk\n  (boot detail here)";
        for phase in [BootPhase::PidFile, BootPhase::Idle] {
            let err = NewError::BootTimeout {
                name: "wk".to_string(),
                phase,
                detail: "boot detail here".to_string(),
            };
            assert_eq!(format!("{err}"), expected, "Display must not vary by phase");
            assert_eq!(err.exit_code(), 1, "BootTimeout stays on the exit-1 lane");
        }
    }

    use super::supported_provider_names;

    /// The unknown-provider line, pinned to the byte.
    ///
    /// It USED to read `claude-code, codex, acp/claude-code, pi, opencode (=
    /// acp/opencode)` — five entries for four programs, one of which named a
    /// transport and one of which had to carry a parenthetical explaining that
    /// what you type and what qd stores are different strings. Both oddities were
    /// the same modelling error, and ACP-as-a-lane removed it: four programs,
    /// four names, no aliases to explain.
    ///
    /// The legacy `acp/*` spellings still PARSE (rows and scripts predate the
    /// remodel). They are deliberately not advertised here — this line answers
    /// "what may I type", and offering two spellings for one thing is how the
    /// split began.
    #[test]
    fn supported_provider_names_is_one_name_per_program() {
        assert_eq!(supported_provider_names(), "claude-code, codex, pi, opencode");
        assert_eq!(
            format!(
                "qd start: unknown provider \"{}\" — this engine supports: {}.",
                "weird", supported_provider_names()
            ),
            "qd start: unknown provider \"weird\" — this engine supports: claude-code, codex, \
             pi, opencode."
        );
    }

    /// ...and it names EVERY harness qw will accept. An id the engine takes but
    /// does not advertise is as much a drift bug as one it advertises and refuses.
    #[test]
    fn supported_provider_names_covers_every_harness() {
        let rendered = supported_provider_names();
        for h in quorum_qw::lane::Harness::ALL {
            assert!(
                rendered.contains(h.provider_id()),
                "{:?} ({}) is accepted by Harness::from_provider_id but not advertised in {rendered:?}",
                h,
                h.provider_id()
            );
        }
        // Every advertised name is one a user may actually type…
        for name in rendered.split(", ") {
            assert!(
                quorum_qw::lane::Harness::from_provider_id(name).is_some(),
                "{name:?} is advertised but not accepted"
            );
        }
        // …and the legacy spellings still parse WITHOUT being advertised, which
        // is the whole compat contract in two lines.
        for legacy in ["acp/claude-code", "acp/opencode"] {
            assert!(
                quorum_qw::lane::Harness::from_provider_id(legacy).is_some(),
                "{legacy} must keep parsing — it is on disk in rows written before the remodel"
            );
            assert!(
                !rendered.contains(legacy),
                "{legacy} still parses but must NOT be advertised: one name per program"
            );
        }
    }
}
