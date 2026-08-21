//! claude launch-command construction (spec §4 deliverable 6; LESSONS **L9**
//! carrier). Ported from `src/utils.ts:181-238` + HARDENING #4b (flags-as-config,
//! spec §7), with the war-story comments carried forward (LESSONS rule 1).

use crate::effects::Env;
use std::path::Path;

/// Built-in default claude flags (TS `CLAUDE_FLAGS`, utils.ts:186).
/// `--dangerously-load-development-channels` takes `server:relay` as its ARGUMENT
/// (the value that loads `~/.claude/channels/`; verified 2026-06-04, spec §7).
///
/// ⚠ SAFE BY DEFAULT — R22 ruling (FTUE punch list, 2026-08-20). This vector used
/// to lead with `--dangerously-skip-permissions`, in TS parity with
/// `src/utils.ts:186`. That was the FLEET's posture — a rack of operator-owned
/// worker sessions — baked in as the FIRST-RUN default, so a brand-new user who
/// typed `qd start` got a claude with every approval prompt suppressed on their
/// own machine, having agreed to nothing. Nothing about being launched by qd makes
/// a session more trusted than one the user starts by hand, and `claude` itself
/// makes that flag an explicit, typed-out choice. So does qd now.
///
/// DO NOT "restore parity" by putting the flag back — this is a deliberate,
/// one-way divergence from the TS pin. The `test/golden/` corpus still records the
/// TS vector; it is now evidence of what the OPT-IN path produces, not of what the
/// default does (pinned both ways by the unit test
/// `default_is_the_ts_vector_minus_the_bypass_flag` below).
///
/// The dev-channels PAIR stays: it is what loads `~/.claude/channels/` and makes
/// `server:relay` — the whole point of a qd-launched session — reachable. It is
/// "dangerously-" named for channel loading, not for permissions.
///
/// OPT IN through the surfaces that already exist ([`resolve_claude_flags`]) —
/// they REPLACE this default rather than append to it, so spell the WHOLE vector:
///
/// ```text
/// QD_CLAUDE_FLAGS="--dangerously-skip-permissions --dangerously-load-development-channels server:relay" qd start wk
/// ```
///
/// or, once per machine, in the qd config toml:
///
/// ```text
/// claude_flags = "--dangerously-skip-permissions --dangerously-load-development-channels server:relay"
/// ```
const DEFAULT_FLAGS: &[&str] = &["--dangerously-load-development-channels", "server:relay"];

// --- Effort A / Reading B: the forbidden-claude-flag matcher --------------------
//
// THE no-dispatch-spawned-`claude --print` guard (atomic2-impl-plan.md §3). The
// forbidden set F = { -p, --print, --output-format, --input-format } (and their
// value/attached/cluster forms) defines whether a run is a one-off headless/print
// turn and what I/O contract claude uses — exactly what dispatch must never inject
// into its OWN spawns. This is the SINGLE source of truth shared by all 3 guard
// loci: E2 (`lifecycle.rs run_start`, REFUSE), H9 (`daemon_headless.rs resolve`,
// REFUSE-framed), and CF ([`claude_flags`] below, STRIP+warn). A 4th forward site
// that bypasses it is caught by the structural source-canary + the behavioral
// surface matrix (atomic2-impl-plan.md §5/§6).

/// Long forbidden flags (exact, no abbreviation in claude). Effort A / Reading B.
const FORBIDDEN_LONG: &[&str] = &["--print", "--output-format", "--input-format"];

/// Is `tok` a forbidden claude flag (CLUSTER-AWARE, fail-closed)? Verified against
/// live claude v2.1.195 short-flag grammar (atomic2-impl-plan.md §3.1/§3.2):
///   - long form → strip `--`, split on `=`, EXACT name match (no abbreviation).
///   - short cluster → walk chars: `p` (== --print) trips; `n` (--name, REQUIRED
///     value) stops the walk (the rest of the token is the name, e.g. `-nProp`);
///     every other char (c/d/h/r/v/w + unknowns) is treated NON-consuming so a
///     later `p` still trips (a safe over-refusal for `-dp`/`-rp`/`-wp`, and
///     fail-closed against grammar drift — see §3.2 rationale).
pub fn is_forbidden_claude_flag(tok: &str) -> bool {
    if let Some(rest) = tok.strip_prefix("--") {
        // long form
        let name = rest.split('=').next().unwrap_or(rest); // --output-format=json -> output-format
        return FORBIDDEN_LONG.iter().any(|f| &f[2..] == name);
    }
    if let Some(short) = tok.strip_prefix('-') {
        // short cluster (single dash, len>1)
        if short.is_empty() {
            return false; // bare "-" = stdin, not a flag
        }
        for ch in short.chars() {
            match ch {
                'p' => return true,  // standalone -p == --print
                'n' => return false, // -n (--name, REQUIRED value): rest is the name, stop walking
                _ => continue,       // c/d/h/r/v/w + unknowns: NON-consuming (fail-closed)
            }
        }
    }
    false // positional / value (no leading dash) is not a flag
}

/// Every offending token in `args` (for the E2/H9 teaching messages). Empty ⇒ clean.
pub fn forbidden_in(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| is_forbidden_claude_flag(a))
        .cloned()
        .collect()
}

/// CF partition: split `flags` into `(kept, dropped)` — `dropped` is every forbidden
/// token, `kept` is everything else (preserves order within each partition).
pub fn strip_forbidden(flags: Vec<String>) -> (Vec<String>, Vec<String>) {
    flags
        .into_iter()
        .partition(|f| !is_forbidden_claude_flag(f))
}

/// The E2 teaching error (atomic2-impl-plan.md §2.1) for a forbidden flag refused at
/// `qd start`. Disambiguates the two-`-p` trap (qd's `--prompt` vs claude's `--print`).
pub fn forbidden_flag_teaching_error(flag: &str) -> String {
    format!(
        "qd start: cannot forward \"{flag}\" to a dispatch-launched session. dispatch only \
         creates tracked, attachable sessions; -p/--print/--output-format/--input-format would \
         make a one-off headless run.\n\
         • For a one-off print run: use `claude -p …` directly. • For a tracked agent turn: \
         `qd start <name> -p <prompt>` (that -p is qd's prompt flag, not claude's)."
    )
}

/// Resolve the claude binary dynamically — NEVER a per-machine absolute path.
/// (Port of `CLAUDE_BIN`, utils.ts:181-185.)
///
/// Hardcoding the origin Mac's /home/u/.local/bin/claude broke cross-machine
/// spawn: on a Linux box that path doesn't exist, so `bash -lc '/home/u/...'`
/// failed → every session hung in "Waiting for ready". Set CLAUDE_BIN to override.
///
/// `?Sized` so a `&dyn Env` (an injected seam held behind a trait object, as the
/// revive/create deps structs hold it) is accepted alongside a concrete `&RealEnv`
/// — the bare `&impl Env` form silently excludes the trait-object case.
pub fn claude_bin(env: &(impl Env + ?Sized)) -> String {
    env.var("CLAUDE_BIN")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "claude".to_string())
}

/// Resolve the claude flags (HARDENING #4b / spec §7, L9 carrier). Resolution
/// precedence:
///   1. `QD_CLAUDE_FLAGS` env (whitespace-split), if set and non-empty.
///   2. `claude_flags` string key in the config toml at `config_toml_path`
///      (whitespace-split), if present.
///   3. built-in default (`DEFAULT_FLAGS`, TS parity).
///
/// Permissive (L8): a missing file or missing key simply falls through; a config
/// read never hard-fails the launch.
///
/// `?Sized` so a `&dyn Env` (an injected seam held behind a trait object, as the
/// revive/create deps structs hold it) is accepted alongside a concrete `&RealEnv`
/// — the bare `&impl Env` form silently excludes the trait-object case.
pub fn claude_flags(env: &(impl Env + ?Sized), config_toml_path: &Path) -> Vec<String> {
    // CF guard (atomic2-impl-plan.md §4): resolve the base/operator flag vector by
    // precedence, then STRIP the forbidden injections ONCE before returning at every
    // path. STRIP (not refuse) here — a stale env/config entry must not turn into a
    // launch outage; the security property (the forbidden flag never reaches argv) is
    // unconditional. `DEFAULT_FLAGS` contains no member of F, so the default path is
    // byte-unchanged in behavior. The launcher's OWN required `-p`/`--output-format`
    // are hardcoded in `headless.rs HeadlessLaunch::argv()` AFTER these flags, so
    // stripping here cannot break the tracked headless launch (separability proof).
    let resolved = resolve_claude_flags(env, config_toml_path);
    let (mut kept, dropped) = strip_forbidden(resolved);
    if !dropped.is_empty() {
        eprintln!(
            "qd: ignoring forbidden claude flag(s) {dropped:?} from QD_CLAUDE_FLAGS/config.toml \
             — dispatch does not inject --print/output-format into its own spawns"
        );
    }
    // External adoption installs a Stop hook in a generated settings file and
    // launches through the ordinary resume machinery. Keep the path as one argv
    // element (rather than placing it in whitespace-split QD_CLAUDE_FLAGS), so
    // HOME/QD_HOME paths containing spaces remain exact. `--settings` is not a
    // headless/print flag and is intentionally outside the forbidden set.
    if let Some(settings) = env.var("QD_ADOPTION_SETTINGS").filter(|s| !s.is_empty()) {
        kept.push("--settings".to_string());
        kept.push(settings);
    }
    kept
}

/// Resolve the base claude flags by precedence — BEFORE the CF forbidden-flag strip
/// applied in [`claude_flags`]. Split out so the strip runs ONCE over every return
/// path (env / config / default), not duplicated per branch.
///
/// REPLACE, never append — and that is load-bearing twice over. It is what lets an
/// operator NARROW the vector (ADR 0006's stated purpose: a machine that must not
/// load dev channels sets `QD_CLAUDE_FLAGS` to just the flags it wants), and since
/// R22 it is also the carrier for the bypass OPT-IN: a caller who wants
/// `--dangerously-skip-permissions` back names it here, alongside whatever else it
/// wants, and gets exactly the vector it typed. R22 deliberately added NO new flag
/// or env var for the bypass — a second surface would mean two ways to spell one
/// posture and a merge rule between them, and the honest cost of turning approval
/// prompts off should be typing out the flag that turns them off. See
/// [`DEFAULT_FLAGS`] for the exact spellings.
fn resolve_claude_flags(env: &(impl Env + ?Sized), config_toml_path: &Path) -> Vec<String> {
    // 1. env override.
    if let Some(raw) = env.var("QD_CLAUDE_FLAGS") {
        let flags = split_ws(&raw);
        if !flags.is_empty() {
            return flags;
        }
    }
    // 2. config toml `claude_flags = "..."`.
    if let Ok(contents) = std::fs::read_to_string(config_toml_path) {
        if let Some(raw) = parse_claude_flags_key(&contents) {
            let flags = split_ws(&raw);
            if !flags.is_empty() {
                return flags;
            }
        }
    }
    // 3. built-in default.
    DEFAULT_FLAGS.iter().map(|s| s.to_string()).collect()
}

/// Hand-parse the single `claude_flags = "..."` key from a config.toml. We do NOT
/// add a toml-crate dependency for one read-side key (A2 adds no `qd config`
/// surface; Cargo.toml has no toml dep — keeping the dep tree minimal). This is a
/// deliberately minimal line-scan: find a non-comment line `claude_flags = "VALUE"`
/// (single OR double quotes), return VALUE. Anything fancier (multi-line, tables,
/// escapes) is out of scope for this one key.
fn parse_claude_flags_key(contents: &str) -> Option<String> {
    for line in contents.lines() {
        let line = line.trim_start();
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("claude_flags") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim();
        // Strip matching surrounding quotes (double or single).
        let val = rest
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| rest.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
        if let Some(v) = val {
            return Some(v.to_string());
        }
    }
    None
}

/// Whitespace-split (JS `.split(/\s+/).filter(Boolean)` equivalent): split on any
/// run of ASCII whitespace, dropping empties.
fn split_ws(s: &str) -> Vec<String> {
    s.split_whitespace().map(|t| t.to_string()).collect()
}

// --- Render mode (qb punch item 7: alt-screen inline default) ------------------
//
// Inline rendering = fleet default; alt-screen = per-session opt-in (STATE 64/65,
// Pete-pinned). Mechanism = launch-time BIRTH PROPERTY: the engine injects
// `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` into the child env via the existing
// session-env machinery (`launch_env_pairs` → env file → dot-source prefix) —
// the QD_SESSION_ID pattern, third instance of the birth-property lesson.
// NEVER the settings.json env block (live Claude Code sessions hot-apply that
// file and corrupt — the original freeze reason).

/// The Claude Code env var that DISABLES alt-screen rendering (value "1").
/// Injected at launch when the resolved render mode is [`RenderMode::Inline`].
pub const ALT_SCREEN_DISABLE_KEY: &str = "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN";

/// The Claude Code env var that FORCES session-registration persistence even
/// for a nested child (value "1"). Injected UNCONDITIONALLY into every pane env
/// (the birth-property lesson, fourth instance — see [`launch_env_pairs`]).
///
/// WHY: claude ≥2.1.172 SKIPS writing its `~/.claude/sessions/<pid>.json`
/// registration row when `CLAUDE_CODE_CHILD_SESSION=1` is in its env. A spawn
/// issued from inside a claude session (any Bash-tool shell) inherits
/// CHILD_SESSION into the pane through the mux daemon, so the engine's boot
/// waiter polls a row that is never written → 40s timeout, healthy-but-
/// unregistered TUI (board STATE 130). A qd-spawned session is a real, top-level
/// session that MUST register; forcing persistence is the fix. Setting FORCE is
/// deliberately preferred over UNSETTING CHILD_SESSION — it preserves whatever
/// else keys off the nesting signal while still guaranteeing the row.
pub const FORCE_SESSION_PERSISTENCE_KEY: &str = "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE";

/// The `render-default` config key (file tier; see `secrets::PLAIN_FILE_KEYS`).
pub const RENDER_DEFAULT_KEY: &str = "render-default";

/// How a session renders: inline (scrollback — the fleet default, so phone/SSH
/// attach can scroll) or alt-screen (fullscreen, per-session opt-in).
///
/// SERIALIZABLE because it crosses the qd/qw boundary as plain data: qd resolves
/// the mode (flags, then `render-default` config — a read that lives behind
/// `crate::secrets`, deliberately on qd's side) and hands the VALUE to
/// [`crate::contract::DeliverPolicy`] / [`crate::contract::LaneOps::wake`]. The
/// same shape [`crate::create::NewParams::render`] already uses. The wire tokens
/// are the config file's own (`inline` / `alt-screen`), so a serialized policy
/// reads the way the config that produced it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RenderMode {
    #[default]
    Inline,
    AltScreen,
}

/// Resolve the effective render mode. Precedence (punch item 7, pinned by
/// tests): per-start flag > `render-default` config > built-in default (inline).
///
/// `flag` is the per-start request (`--alt-screen` / `--inline`), `None` when
/// neither flag was given. `config_value` is the raw `render-default` config
/// value, `None` when unset. An UNKNOWN config value falls through to inline
/// (permissive read, L8 — `qd config set` rejects bad values at write time, so
/// this only happens on a hand-edited file).
pub fn resolve_render_mode(flag: Option<RenderMode>, config_value: Option<&str>) -> RenderMode {
    if let Some(f) = flag {
        return f;
    }
    match config_value {
        Some("alt-screen") => RenderMode::AltScreen,
        _ => RenderMode::Inline,
    }
}

/// NOTE: `render_default_from_config` used to live here. It read the plain,
/// non-secret `render-default` key, which coupled this module — the one
/// `provider.rs` calls for `claude_bin` / `claude_flags` / `build_new_extra_args`
/// — to `crate::secrets`, and dragged the whole config/credential surface into
/// the session-management dependency closure. Every caller was already CLI code,
/// and [`resolve_render_mode`] takes the resolved value as a plain parameter, so
/// the read moved to `bin/qd/verbs/common.rs` and is HANDED here instead.

/// R2 (override-never-inherit — the D1-site-4 / A7-F12 lesson, third
/// instance): the env-file `unset -v` keys a render mode demands. An
/// [`RenderMode::AltScreen`] launch must EXPLICITLY unset
/// `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN` — merely omitting the export leaves
/// the child INHERITING the var from its spawn environment (after cutover the
/// fleet runs inline, so parent environments carry it: an agent in an inline
/// session running `qd start child --alt-screen` would get an INLINE child).
/// Inline needs no unset — its export is an explicit set that clobbers
/// anything inherited. The birth property is asserted BOTH directions, never
/// by absence.
pub fn render_env_unsets(render: RenderMode) -> Vec<String> {
    match render {
        RenderMode::Inline => vec![],
        RenderMode::AltScreen => vec![ALT_SCREEN_DISABLE_KEY.to_string()],
    }
}

/// Options for a `qd new` launch (mirrors the TS `opts` object, utils.ts:219).
#[derive(Debug, Default, Clone)]
pub struct NewOpts {
    pub resume: Option<String>,
    pub fork: bool,
    pub agent: Option<String>,
    /// `--model <m>` launch flag (warranty #2): a per-session birth property,
    /// replacing the post-boot `/model` delivery that polluted the global
    /// default and raced `-p`. See `LaunchRequest::model`.
    pub model: Option<String>,
}

/// Build the `extra` args for a `qd new` claude launch (port of `buildNewExtraArgs`,
/// utils.ts:217-230).
///
/// The session --name plus resume/fork/agent options, then any PASS-THROUGH claude
/// flags forwarded from the shell wrapper (everything after `--`). Flags already
/// baked into `flags` are skipped so we never double-pass e.g.
/// --dangerously-skip-permissions. Pure + injected `flags`-aware → testable.
pub fn build_new_extra_args(
    name: &str,
    opts: &NewOpts,
    claude_args: &[String],
    flags: &[String],
) -> Vec<String> {
    let mut extra = vec!["--name".to_string(), name.to_string()];
    if let Some(resume) = &opts.resume {
        extra.push("--resume".to_string());
        extra.push(resume.clone());
    }
    if opts.fork {
        extra.push("--fork-session".to_string());
    }
    if let Some(agent) = &opts.agent {
        extra.push("--agent".to_string());
        extra.push(agent.clone());
    }
    // --model as a LAUNCH FLAG (warranty #2): per-session, no global-default
    // pollution, no post-boot `/model` delivery (which also raced `-p`).
    if let Some(model) = &opts.model {
        extra.push("--model".to_string());
        extra.push(model.clone());
    }
    for a in claude_args {
        if !flags.iter().any(|f| f == a) {
            extra.push(a.clone());
        }
    }
    extra
}

/// Build the claude launch command string (port of `buildClaudeCmd`,
/// utils.ts:232-238).
///
/// `command` (bash builtin) bypasses the claude() wrapper function — so launching
/// under `bash -lc` never recurses into `qd new` — and PATH-resolves the real
/// binary on ANY machine. An explicit absolute CLAUDE_BIN also runs fine via
/// `command`.
///
/// Each token is single-quoted with NO escaping of embedded quotes (TS does
/// `.map(a => `'${a}'`)`, utils.ts:236) — byte-parity over robustness. A token
/// containing a literal `'` would produce shell-broken output; that matches TS
/// exactly and is intentional (parity is the contract, not robustness).
pub fn build_claude_cmd(bin: &str, flags: &[String], extra: &[String]) -> String {
    let mut tokens: Vec<&str> = Vec::with_capacity(1 + flags.len() + extra.len());
    tokens.push(bin);
    tokens.extend(flags.iter().map(String::as_str));
    tokens.extend(extra.iter().map(String::as_str));
    build_claude_cmd_from_tokens(tokens)
}

/// Shell-assemble a FULLY-RESOLVED argv token list (`[bin, ...flags, ...extra]`)
/// into the `command '...'` string — byte-identical to [`build_claude_cmd`] for
/// the same `[bin] + flags + extra` concatenation.
///
/// codex P1 W3 (codex-p1-spec section 7.1): the provider seam yields the
/// PRE-QUOTE argv ([`crate::provider::LaunchPlan::argv`] = `[bin, ...flags,
/// ...extra]`); this is the SHELL-assembly step the mux owns (the single-quoting +
/// `command` prefix), kept HERE so the create path quotes the provider's argv
/// EXACTLY as it quoted the hand-built (bin, flags, extra) triple — the byte
/// identity the existing create/launch goldens prove. The two entry points share
/// `build_claude_cmd_from_tokens` so there is ONE quoting rule, not two that can
/// drift.
pub fn build_claude_cmd_from_argv(argv: &[String]) -> String {
    build_claude_cmd_from_tokens(argv.iter().map(String::as_str).collect())
}

/// The ONE quoting rule (utils.ts:236): each token single-quoted with NO escaping
/// of embedded quotes, joined by space, behind the `command` builtin. Shared by
/// [`build_claude_cmd`] (split bin/flags/extra) and [`build_claude_cmd_from_argv`]
/// (a pre-concatenated argv) so the assembly cannot drift between the two.
fn build_claude_cmd_from_tokens(tokens: Vec<&str>) -> String {
    let clear_adoption_settings = tokens.iter().any(|token| *token == "--settings");
    let quoted: Vec<String> = tokens.iter().map(|a| format!("'{a}'")).collect();
    let prefix = if clear_adoption_settings {
        // QD_ADOPTION_SETTINGS is a qd-to-qd transport variable, not a Claude
        // session birth property. Clear it in the launch shell before exec so
        // Bash tools and nested qd launches cannot inherit this session's hook.
        "unset -v QD_ADOPTION_SETTINGS; "
    } else {
        ""
    };
    format!("{prefix}command {}", quoted.join(" "))
}

// --- Session backend-env propagation (F1 fix; --via foundation) ----------------
//
// Ported from `utils.ts:514-707` (BACKEND_ENV_KEYS, captureBackendEnv,
// sessionEnvFilePath, shellSingleQuoteEscape, writeSessionEnvFile,
// removeSessionEnvFile, sessionEnvPrefix). A2's create path never wired this in
// Rust; A5's resume needs it, so it lands HERE (the launch home, mirroring TS's
// utils.ts placement) and is available to a future create wiring too.
//
// zmx launches sessions via `zmx run <name> bash -lc <claudeCmd>`. The session
// inherits zmx's spawn env (login shell), NOT the `qd new`/`resume` caller's
// env. So ANTHROPIC_BASE_URL / ANTHROPIC_API_KEY exported in the runner shell
// (e.g. to route through claude-code-router) never reach claude. Fix: capture
// the whitelisted backend vars at resume time, write them to a per-session
// 0600 file (NEVER in argv where `ps`/`zmx ls` would expose the key), and prefix
// the bash -lc cmd with a self-deleting dot-source block.

use std::path::PathBuf;

/// Whitelisted backend env var names (single source of truth for F1 / --via;
/// `BACKEND_ENV_KEYS`, utils.ts:533-538).
pub const BACKEND_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_MODEL",
];

/// PURE. Capture whitelisted backend vars from the injected env, returning only
/// present + non-empty keys in `BACKEND_ENV_KEYS` order (`captureBackendEnv`,
/// utils.ts:569-579).
///
/// `?Sized` so a `&dyn Env` (an injected seam held behind a trait object, as the
/// revive/create deps structs hold it) is accepted alongside a concrete `&RealEnv`
/// — the bare `&impl Env` form silently excludes the trait-object case.
pub fn capture_backend_env(env: &(impl Env + ?Sized)) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for key in BACKEND_ENV_KEYS {
        if let Some(val) = env.var(key) {
            if !val.is_empty() {
                out.push((key.to_string(), val));
            }
        }
    }
    out
}

/// PURE. The full per-session env-file pair set for one launch: the F1/`--via`
/// backend pairs + the punch-7 render-mode birth property + the wave-2
/// `QD_SESSION_ID` identity pair (P0 spec-w2-env D1, both sites).
/// `QD_SESSION_ID` lands LAST and is never part of the capture whitelist — it
/// is engine-asserted, never inherited.
///
/// `qd_session_id` is `Some` whenever the verb minted/resolved a stable id:
/// ALWAYS on resume/revive (the UUID is known, `mint_or_get` ran) and on every
/// production `qd start` (unbound pre-mint); `None` only in unit fixtures.
///
/// `render` (punch item 7): [`RenderMode::Inline`] injects
/// `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` (the fleet-default birth property);
/// [`RenderMode::AltScreen`] injects NOTHING (the var is simply absent, so
/// Claude Code keeps its native fullscreen rendering). This is the ONE shared
/// env-assembly point for every launch site (start / resume / attach), so
/// per-site divergence is structurally impossible.
///
/// `CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1` rides EVERY launch unconditionally
/// (see [`FORCE_SESSION_PERSISTENCE_KEY`]) — independent of render mode, backend
/// routing, and id state — because the nested-spawn registration skip it defeats
/// (board STATE 130) has nothing to do with any of those. Ordering is load-
/// bearing for the goldens: backend pairs FIRST, then the engine birth
/// properties (FORCE, then alt-screen), then `QD_SESSION_ID` LAST.
pub fn launch_env_pairs(
    backend_env: Vec<(String, String)>,
    qd_session_id: Option<String>,
    render: RenderMode,
) -> Vec<(String, String)> {
    let mut pairs = backend_env;
    pairs.push((FORCE_SESSION_PERSISTENCE_KEY.to_string(), "1".to_string()));
    if render == RenderMode::Inline {
        pairs.push((ALT_SCREEN_DISABLE_KEY.to_string(), "1".to_string()));
    }
    if let Some(id) = qd_session_id {
        pairs.push(("QD_SESSION_ID".to_string(), id));
    }
    pairs
}

/// The per-session env file path, `<home>/.quorum/dispatch/session-env/<name>.env`
/// (`sessionEnvFilePath`, utils.ts:584-586). HOME-rooted via the injected `home`
/// (L9a — never the real home directly).
pub fn session_env_file_path(home: &Path, session_name: &str) -> PathBuf {
    home.join(".quorum")
        .join("dispatch")
        .join("session-env")
        .join(format!("{session_name}.env"))
}

/// Single-quote-escape a value so it is safe inside `export K='v'`
/// (`shellSingleQuoteEscape`, utils.ts:594-596): each `'` → `'\''`.
pub fn shell_single_quote_escape(value: &str) -> String {
    value.replace('\'', "'\\''")
}

/// Write the captured backend vars to the per-session env file
/// (`writeSessionEnvFile`, utils.ts:611-637).
///
/// SECURITY:
///   - Values NEVER appear in argv (file, not inline K=V — no `ps`/`zmx ls`).
///   - chmod 0600. The file is unlinked first (S1: forces O_CREAT so the 0600
///     mode applies, and a planted symlink is removed rather than followed).
///   - Self-deleting at boot via [`session_env_prefix`].
///
/// Returns the written path. Caller is responsible for validating the name
/// (S2 / [`crate::resume::validate_session_name`]) BEFORE calling so the path
/// cannot traverse.
pub fn write_session_env_file(
    home: &Path,
    session_name: &str,
    captured: &[(String, String)],
) -> std::io::Result<PathBuf> {
    write_session_env_file_with_unsets(home, session_name, captured, &[])
}

/// Like [`write_session_env_file`] but the body STARTS with `unset -v` lines for
/// `unset_keys` before the exports. A7 F12 fix: `compose_via_env` dropping the
/// other credential slot from the COMPOSED pairs only kept the env FILE from
/// setting it — the child still INHERITED the caller's exported var, and the
/// `bash -lc` LOGIN shell re-exports anything a profile sets (both vectors
/// observed live on Lima 2026-06-05: a caller-exported ANTHROPIC_API_KEY rode
/// into a `--via` profile-secret session next to the profile's AUTH_TOKEN).
/// An explicit `unset -v` in the dot-sourced file runs AFTER profile loading
/// and kills both vectors. Non-`--via` callers use the plain fn (empty unsets,
/// byte-identical file body — the F1 capture surface stays TS-parity).
pub fn write_session_env_file_with_unsets(
    home: &Path,
    session_name: &str,
    captured: &[(String, String)],
    unset_keys: &[String],
) -> std::io::Result<PathBuf> {
    use std::io::Write;
    let file_path = session_env_file_path(home, session_name);
    let dir = home.join(".quorum").join("dispatch").join("session-env");
    std::fs::create_dir_all(&dir)?;
    // S1: unlink first so the subsequent create applies mode 0600 at O_CREAT and
    // any pre-existing symlink is removed (not followed). ENOENT is fine.
    let _ = std::fs::remove_file(&file_path);
    let mut lines: Vec<String> = Vec::new();
    for k in unset_keys {
        // Keys come from BACKEND_ENV_KEYS (static whitelist) — never values,
        // never caller input; safe to interpolate bare.
        lines.push(format!("unset -v {k}"));
    }
    lines.extend(
        captured
            .iter()
            .map(|(k, v)| format!("export {k}='{}'", shell_single_quote_escape(v))),
    );
    let body: String = lines.join("\n");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&file_path)?;
    f.write_all(body.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(file_path)
}

/// Best-effort remove the per-session env file (`removeSessionEnvFile`,
/// utils.ts:646-653). Under normal operation the file self-deletes at boot
/// (D1); this covers a launch that fails before the prefix runs. Failure is
/// swallowed (a leftover file is a minor key-at-rest issue, not a correctness
/// failure).
pub fn remove_session_env_file(home: &Path, session_name: &str) {
    let _ = std::fs::remove_file(session_env_file_path(home, session_name));
}

/// Produce the self-deleting dot-source prefix that bakes the env file into a
/// `bash -lc` string (`sessionEnvPrefix`, utils.ts:668-680). Empty only when
/// there are NO exports AND NO unsets (no file, no prefix, zero behavior
/// change) — an unset-only file (R2) must still be sourced.
///
/// D1 (self-delete): source fail-closed (exit 97 if the source fails — avoids
/// silently billing a wrong backend), then `rm -f` best-effort AFTER source.
/// The key VALUE never appears in the prefix — only the file PATH does.
/// validateSessionName guarantees the name (and thus the path) carries no `'`
/// that would break the surrounding single-quote context.
pub fn session_env_prefix(
    home: &Path,
    session_name: &str,
    captured: &[(String, String)],
    unset_keys: &[String],
) -> String {
    // R2: an UNSET-ONLY file (an --alt-screen launch with nothing else to
    // export) must still be sourced — the prefix is empty only when there is
    // genuinely nothing to apply (no exports AND no unsets).
    if captured.is_empty() && unset_keys.is_empty() {
        return String::new();
    }
    let f = session_env_file_path(home, session_name);
    let f = f.to_string_lossy();
    format!(
        "{{ . '{f}'; }} || {{ echo 'qd: env file source failed, aborting session' >&2; exit 97; }}; rm -f -- '{f}'; "
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::MapEnv;

    fn env(pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            uid: 501,
        }
    }

    fn default_flags() -> Vec<String> {
        DEFAULT_FLAGS.iter().map(|s| s.to_string()).collect()
    }

    /// The bypass OPT-IN spelling, exactly as a user types it into
    /// `QD_CLAUDE_FLAGS` / `claude_flags` (R22). This is also the pre-R22 TS
    /// vector, which is why the frozen `test/golden/` captures still match it.
    const BYPASS_OPT_IN: &str =
        "--dangerously-skip-permissions --dangerously-load-development-channels server:relay";

    fn bypass_opt_in_flags() -> Vec<String> {
        split_ws(BYPASS_OPT_IN)
    }

    #[test]
    fn claude_bin_defaults_to_claude() {
        assert_eq!(claude_bin(&MapEnv::default()), "claude");
        assert_eq!(
            claude_bin(&env(&[("CLAUDE_BIN", "/opt/claude")])),
            "/opt/claude"
        );
        // Empty env value falls back to "claude".
        assert_eq!(claude_bin(&env(&[("CLAUDE_BIN", "")])), "claude");
    }

    #[test]
    fn claude_flags_precedence_env_over_config_over_default() {
        let nonexistent = Path::new("/tmp/does-not-exist-qd-config.toml");

        // Default when nothing set.
        assert_eq!(
            claude_flags(&MapEnv::default(), nonexistent),
            default_flags()
        );

        // Env override wins, whitespace-split.
        let e = env(&[("QD_CLAUDE_FLAGS", "--foo  --bar baz")]);
        assert_eq!(claude_flags(&e, nonexistent), vec!["--foo", "--bar", "baz"]);
    }

    #[test]
    fn adoption_settings_are_appended_as_one_allowed_argv_element() {
        let settings = "/home with space/.quorum/adoption/settings.json";
        let e = env(&[
            (
                "QD_CLAUDE_FLAGS",
                "--dangerously-skip-permissions --dangerously-load-development-channels server:relay",
            ),
            ("QD_ADOPTION_SETTINGS", settings),
        ]);
        let flags = claude_flags(&e, Path::new("/does/not/exist"));
        assert_eq!(
            flags,
            vec![
                "--dangerously-skip-permissions",
                "--dangerously-load-development-channels",
                "server:relay",
                "--settings",
                settings,
            ]
        );
        assert!(!is_forbidden_claude_flag("--settings"));
        let command = build_claude_cmd("claude", &flags, &[]);
        assert!(command.starts_with("unset -v QD_ADOPTION_SETTINGS; command "));
    }

    #[test]
    fn claude_flags_reads_config_key() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(
            &cfg,
            "# a comment\nother_key = \"x\"\nclaude_flags = \"--alpha --beta\"\n",
        )
        .unwrap();
        // No env override → config wins over default.
        assert_eq!(
            claude_flags(&MapEnv::default(), &cfg),
            vec!["--alpha", "--beta"]
        );
        // Env still wins over config.
        let e = env(&[("QD_CLAUDE_FLAGS", "--env-only")]);
        assert_eq!(claude_flags(&e, &cfg), vec!["--env-only"]);
    }

    #[test]
    fn claude_flags_missing_key_falls_through_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(&cfg, "unrelated = \"value\"\n").unwrap();
        assert_eq!(claude_flags(&MapEnv::default(), &cfg), default_flags());
    }

    #[test]
    fn parse_claude_flags_key_handles_quotes_and_comments() {
        assert_eq!(
            parse_claude_flags_key("claude_flags = \"--a --b\"\n"),
            Some("--a --b".to_string())
        );
        assert_eq!(
            parse_claude_flags_key("claude_flags = '--single'\n"),
            Some("--single".to_string())
        );
        // Commented-out key is ignored.
        assert_eq!(parse_claude_flags_key("# claude_flags = \"--x\"\n"), None);
        // Absent key.
        assert_eq!(parse_claude_flags_key("foo = 1\n"), None);
    }

    // --- Effort A / Reading B: forbidden-flag matcher (atomic2-impl-plan.md §6) ---

    /// Long forms TRIP: exact name AND attached `=value` (the name still matches;
    /// `--print=x` trips on the name even though claude itself rejects it).
    #[test]
    fn matcher_long_forms_trip() {
        for t in [
            "--print",
            "--output-format",
            "--input-format",
            "--output-format=json",
            "--input-format=stream-json",
            "--print=x",
        ] {
            assert!(is_forbidden_claude_flag(t), "long form must trip: {t}");
        }
    }

    /// Short bare/attached TRIP: `-p` (boolean) and `-pHELLO` (a cluster starting p).
    #[test]
    fn matcher_short_bare_and_attached_trip() {
        assert!(is_forbidden_claude_flag("-p"));
        assert!(is_forbidden_claude_flag("-pHELLO"));
    }

    /// Short CLUSTERS reaching `p` TRIP — incl. the review-BLOCKER class (`-cp` etc.)
    /// and the safe over-refusals (`-dp`/`-rp`/`-wp`, live `--debug/--resume/
    /// --worktree "p"` but conservatively refused, §3.2).
    #[test]
    fn matcher_clusters_reaching_p_trip() {
        for t in ["-cp", "-pc", "-pd", "-vp", "-hp", "-dp", "-rp", "-wp"] {
            assert!(is_forbidden_claude_flag(t), "cluster must trip: {t}");
        }
    }

    /// Value-payload NEGATIVES must NOT trip: `-n` (--name, required value) swallows
    /// the rest, so a session name carrying a lowercase `p` still launches.
    #[test]
    fn matcher_name_value_payloads_do_not_trip() {
        for t in ["-nProp", "-nPxyz", "-nmyProp", "-npipeline"] {
            assert!(
                !is_forbidden_claude_flag(t),
                "name payload must NOT trip: {t}"
            );
        }
    }

    /// Benign tokens must NOT trip: known booleans, value flags, names, positionals.
    #[test]
    fn matcher_benign_tokens_do_not_trip() {
        for t in [
            "--model",
            "opus",
            "--add-dir",
            "/tmp/x",
            "-c",
            "-n",
            "name",
            "-",
            "",
            "--mcp-config",
            "--include-partial-messages",
            "--replay-user-messages",
        ] {
            assert!(
                !is_forbidden_claude_flag(t),
                "benign token must NOT trip: {t}"
            );
        }
    }

    /// Documented SAFE over-refusal (Minor #1): a VALUE token that itself starts
    /// `-p`/`--print` (e.g. an `--mcp-config -profile.json` path value) trips the
    /// matcher — it scans tokens, not flag-vs-value position. Over-refusal in the
    /// safe direction; pinned so the behavior is intentional, not accidental.
    #[test]
    fn matcher_value_position_over_refuses_documented() {
        // A SHORT value token starting `-p` (a `-profile.json` path value) trips —
        // the matcher scans tokens, not flag-vs-value position. Safe over-refusal.
        assert!(is_forbidden_claude_flag("-profile.json"));
        // A LONG token is EXACT-name-matched only (no abbreviation/prefix), so an
        // unrelated `--print-ish-path` does NOT trip — the long arm stays precise.
        assert!(!is_forbidden_claude_flag("--print-ish-path"));
    }

    /// `forbidden_in` collects EVERY offending token (for the teaching message),
    /// and is empty for a clean arg vector.
    #[test]
    fn forbidden_in_collects_all_offenders() {
        let args: Vec<String> = ["--model", "opus", "--print", "-cp", "--add-dir", "/x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(forbidden_in(&args), vec!["--print", "-cp"]);
        let clean: Vec<String> = ["--model", "opus", "-nProp"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(forbidden_in(&clean).is_empty());
    }

    /// `strip_forbidden` partitions into (kept, dropped), preserving order.
    #[test]
    fn strip_forbidden_partitions_kept_and_dropped() {
        let flags: Vec<String> = [
            "--model",
            "opus",
            "--output-format",
            "json",
            "-vp",
            "--add-dir",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (kept, dropped) = strip_forbidden(flags);
        // `json` is a positional value (no leading dash) → kept (a known safe
        // orphan; CF documents the value-orphan limitation).
        assert_eq!(kept, vec!["--model", "opus", "json", "--add-dir"]);
        assert_eq!(dropped, vec!["--output-format", "-vp"]);
    }

    /// CF guard: `claude_flags` STRIPS a forbidden flag injected via QD_CLAUDE_FLAGS,
    /// keeping the benign ones — the session still launches with a clean vector.
    #[test]
    fn claude_flags_strips_forbidden_from_env() {
        let nonexistent = Path::new("/tmp/does-not-exist-qd-config.toml");
        let e = env(&[(
            "QD_CLAUDE_FLAGS",
            "--model opus --output-format json --print",
        )]);
        // `--output-format` and `--print` dropped; `--model opus json` kept
        // (`json` is an orphaned value, the documented CF limitation).
        assert_eq!(
            claude_flags(&e, nonexistent),
            vec!["--model", "opus", "json"]
        );
    }

    /// CF guard: `claude_flags` STRIPS a forbidden flag from config.toml `claude_flags`.
    #[test]
    fn claude_flags_strips_forbidden_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(&cfg, "claude_flags = \"--add-dir /x -p\"\n").unwrap();
        // `-p` dropped; `--add-dir /x` survives.
        assert_eq!(
            claude_flags(&MapEnv::default(), &cfg),
            vec!["--add-dir", "/x"]
        );
    }

    /// CF guard: the DEFAULT path is byte-unchanged — `DEFAULT_FLAGS` has no member
    /// of F, so a clean default resolve passes through untouched.
    #[test]
    fn claude_flags_default_path_unchanged_by_strip() {
        let nonexistent = Path::new("/tmp/does-not-exist-qd-config.toml");
        assert_eq!(
            claude_flags(&MapEnv::default(), nonexistent),
            default_flags()
        );
    }

    // --- R22: safe by default, bypass is opt-in ---------------------------------

    /// THE ruling, asserted on the CONSTANT rather than on a resolved vector, so it
    /// holds no matter which resolution path a caller took. A fresh machine with no
    /// env and no config gets a claude that still ASKS. The dev-channels pair is
    /// asserted present in the same breath — it is the half of the old vector that
    /// must NOT be collateral damage (it is what makes `server:relay` load).
    #[test]
    fn default_flags_do_not_bypass_permissions_but_keep_dev_channels() {
        assert!(
            !DEFAULT_FLAGS.contains(&"--dangerously-skip-permissions"),
            "R22: the built-in default must not suppress approval prompts on a \
             user's machine — the bypass is opt-in. Read the DEFAULT_FLAGS doc \
             comment before changing this: {DEFAULT_FLAGS:?}"
        );
        assert_eq!(
            DEFAULT_FLAGS,
            &["--dangerously-load-development-channels", "server:relay"],
            "the dev-channels flag and its ARGUMENT are a pair and both stay"
        );

        // And end-to-end through the real resolver, on a machine with nothing set.
        let flags = claude_flags(&MapEnv::default(), Path::new("/tmp/no-such-config.toml"));
        assert!(!flags.iter().any(|f| f == "--dangerously-skip-permissions"));
        assert!(flags.iter().any(|f| f == "server:relay"));
    }

    /// The OPT-IN still works, through BOTH pre-existing surfaces and with no new
    /// flag or env var — the escape hatch R22 routed the bypass through. Note the
    /// user spells the WHOLE vector: these surfaces REPLACE the default, so the
    /// dev-channels pair has to be named too or it is genuinely gone (that is the
    /// narrowing power ADR 0006 wanted, not a bug).
    #[test]
    fn bypass_is_opt_in_via_env_and_via_config_key() {
        let nonexistent = Path::new("/tmp/does-not-exist-qd-config.toml");

        // 1. QD_CLAUDE_FLAGS.
        let e = env(&[("QD_CLAUDE_FLAGS", BYPASS_OPT_IN)]);
        assert_eq!(claude_flags(&e, nonexistent), bypass_opt_in_flags());

        // 2. `claude_flags` in the config toml (the per-machine spelling).
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(&cfg, format!("claude_flags = \"{BYPASS_OPT_IN}\"\n")).unwrap();
        assert_eq!(
            claude_flags(&MapEnv::default(), &cfg),
            bypass_opt_in_flags()
        );

        // The bypass flag is NOT in the forbidden set F — the CF strip must never
        // eat it, or the opt-in would silently no-op (the failure mode a user
        // would report as "I set the flag and it did nothing").
        assert!(!is_forbidden_claude_flag("--dangerously-skip-permissions"));

        // Opting in alongside a forbidden flag: the bypass survives, F still dies.
        let e = env(&[("QD_CLAUDE_FLAGS", "--dangerously-skip-permissions --print")]);
        assert_eq!(
            claude_flags(&e, nonexistent),
            vec!["--dangerously-skip-permissions"]
        );
    }

    #[test]
    fn build_new_extra_args_name_first_then_opts_then_passthrough() {
        let flags = default_flags();
        let opts = NewOpts {
            resume: Some("sess-1".to_string()),
            fork: true,
            agent: Some("reviewer".to_string()),
            model: None,
        };
        let extra = build_new_extra_args(
            "myname",
            &opts,
            &["--model".to_string(), "opus".to_string()],
            &flags,
        );
        assert_eq!(
            extra,
            vec![
                "--name",
                "myname",
                "--resume",
                "sess-1",
                "--fork-session",
                "--agent",
                "reviewer",
                "--model",
                "opus",
            ]
        );
    }

    #[test]
    fn build_new_extra_args_dedupes_against_flags() {
        // R22: the dedupe is against the RESOLVED flag vector, so exercise it with
        // an OPT-IN vector — `--dangerously-skip-permissions` is no longer in the
        // default, and a pass-through only double-passes if the resolved flags
        // actually carry it.
        let flags = bypass_opt_in_flags();
        let extra = build_new_extra_args(
            "n",
            &NewOpts::default(),
            &[
                "--dangerously-skip-permissions".to_string(),
                "--extra".to_string(),
            ],
            &flags,
        );
        assert_eq!(extra, vec!["--name", "n", "--extra"]);
        // Mirror: with the SAFE default vector the same pass-through is NOT a
        // duplicate, so it rides through to argv (the user asked for it by hand).
        let extra = build_new_extra_args(
            "n",
            &NewOpts::default(),
            &[
                "--dangerously-skip-permissions".to_string(),
                "--extra".to_string(),
            ],
            &default_flags(),
        );
        assert_eq!(
            extra,
            vec!["--name", "n", "--dangerously-skip-permissions", "--extra"]
        );
    }

    // Pete feedback #6 (duplicate session ids) regression pin. qd does NOT mint its
    // own id — a claude fork's id-freshness rides ENTIRELY on `--fork-session` being
    // emitted on the fork path (`qd start <name> --fork <session>`): claude mints a fresh UUID
    // only when it sees that flag. Without it every fork would INHERIT the parent's
    // UUID → two sessions, same id. This pins the flag on (fork) and its ABSENCE off
    // (no fork), so a silent regression that drops it reds here.
    /// warranty #2: --model rides the launch argv as a flag (per-session birth
    /// property), positioned after --agent and before any passthrough. This is
    /// the dimension whose ABSENCE made the golden-argv test blind to the model
    /// regression — now the argv carries it.
    #[test]
    fn model_emits_launch_flag_after_agent_before_passthrough() {
        let flags = default_flags();
        let opts = NewOpts {
            resume: None,
            fork: false,
            agent: Some("reviewer".to_string()),
            model: Some("claude-fable-5".to_string()),
        };
        let extra = build_new_extra_args("wk", &opts, &["--foo".to_string()], &flags);
        // --model <m> present as adjacent tokens.
        assert!(
            extra.windows(2).any(|w| w == ["--model", "claude-fable-5"]),
            "model is a launch flag: {extra:?}"
        );
        // ordering: --agent reviewer ... --model claude-fable-5 ... --foo.
        let pos = |needle: &str| extra.iter().position(|t| t == needle).unwrap();
        assert!(
            pos("--agent") < pos("--model"),
            "agent before model: {extra:?}"
        );
        assert!(
            pos("--model") < pos("--foo"),
            "model before passthrough: {extra:?}"
        );
    }

    /// No --model ⇒ no model tokens (a plain session inherits the launch-time
    /// default; nothing is emitted and nothing post-boot mutates it).
    #[test]
    fn no_model_emits_no_model_tokens() {
        let flags = default_flags();
        let opts = NewOpts {
            resume: None,
            fork: false,
            agent: None,
            model: None,
        };
        let extra = build_new_extra_args("wk", &opts, &[], &flags);
        assert!(!extra.iter().any(|t| t == "--model"), "no model: {extra:?}");
    }

    #[test]
    fn fork_emits_fork_session_so_a_forked_id_is_minted_fresh() {
        let flags = default_flags();
        let opts = NewOpts {
            resume: Some("parent-uuid".to_string()),
            fork: true,
            agent: None,
            model: None,
        };
        let extra = build_new_extra_args("child", &opts, &[], &flags);
        assert!(
            extra.windows(2).any(|w| w == ["--resume", "parent-uuid"]),
            "fork resumes the parent: {extra:?}"
        );
        assert!(
            extra.iter().any(|a| a == "--fork-session"),
            "fork MUST emit --fork-session so claude mints a fresh id: {extra:?}"
        );
    }

    #[test]
    fn no_fork_omits_fork_session() {
        let flags = default_flags();
        let opts = NewOpts {
            resume: Some("parent-uuid".to_string()),
            fork: false,
            agent: None,
            model: None,
        };
        let extra = build_new_extra_args("child", &opts, &[], &flags);
        assert!(
            !extra.iter().any(|a| a == "--fork-session"),
            "a non-fork resume must NOT carry --fork-session: {extra:?}"
        );
    }

    #[test]
    fn build_claude_cmd_single_quotes_each_token_no_escaping() {
        let cmd = build_claude_cmd(
            "claude",
            &default_flags(),
            &["--name".to_string(), "x".to_string()],
        );
        assert_eq!(
            cmd,
            "command 'claude' '--dangerously-load-development-channels' \
             'server:relay' '--name' 'x'"
        );
    }

    /// PARITY, re-aimed by R22. `GOLDEN` is the frozen 0b capture byte-for-byte
    /// (test/golden/dryrun/captures/build_claude_cmd/capture.normalized) — the TS
    /// pin's argv, `<NAME>` and all. It no longer describes the DEFAULT, because
    /// the default no longer carries the bypass flag; it describes the OPT-IN.
    ///
    /// So this test now proves BOTH halves of the R22 ruling against the one
    /// frozen artifact: the quoting/order machinery is byte-unchanged (feed it the
    /// opt-in vector and the old golden reappears exactly), and the default is that
    /// same vector minus exactly one token. A regression that puts the flag back in
    /// `DEFAULT_FLAGS` reds the second half; a regression in the quoting rule or
    /// flag ORDER reds the first. Deleting the golden to "fix" either one throws
    /// away the only byte-level record of the assembly contract.
    #[test]
    fn default_is_the_ts_vector_minus_the_bypass_flag() {
        const GOLDEN: &str = "command 'claude' '--dangerously-skip-permissions' '--dangerously-load-development-channels' 'server:relay' '--name' '<NAME>'";
        let cmd = |env: &MapEnv| {
            let bin = claude_bin(env);
            let flags = claude_flags(env, Path::new("/tmp/no-such-config.toml"));
            let extra = build_new_extra_args("<NAME>", &NewOpts::default(), &[], &flags);
            build_claude_cmd(&bin, &flags, &extra)
        };

        // OPT-IN: the user types the whole vector → the frozen golden, byte-exact.
        assert_eq!(cmd(&env(&[("QD_CLAUDE_FLAGS", BYPASS_OPT_IN)])), GOLDEN);

        // DEFAULT: the same string with the bypass token (and its quotes and the
        // one separating space) removed — nothing else moved.
        let expected_default = GOLDEN.replace("'--dangerously-skip-permissions' ", "");
        assert_eq!(cmd(&MapEnv::default()), expected_default);
        assert!(
            !expected_default.contains("dangerously-skip-permissions"),
            "sanity: the removal actually removed it"
        );
    }

    // --- F1 session-env mechanism (utils.ts:514-707) ---

    #[test]
    fn capture_backend_env_only_present_nonempty_in_order() {
        let e = env(&[
            ("ANTHROPIC_MODEL", "opus"),
            ("ANTHROPIC_BASE_URL", "http://router"),
            ("ANTHROPIC_API_KEY", ""), // empty → skipped
            ("UNRELATED", "x"),        // not whitelisted → skipped
        ]);
        let cap = capture_backend_env(&e);
        // BACKEND_ENV_KEYS order: BASE_URL, API_KEY, AUTH_TOKEN, MODEL.
        assert_eq!(
            cap,
            vec![
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    "http://router".to_string()
                ),
                ("ANTHROPIC_MODEL".to_string(), "opus".to_string()),
            ]
        );
    }

    #[test]
    fn capture_backend_env_empty_when_none_set() {
        assert!(capture_backend_env(&MapEnv::default()).is_empty());
    }

    #[test]
    fn shell_single_quote_escape_breaks_out_safely() {
        assert_eq!(shell_single_quote_escape("a'b"), "a'\\''b");
        assert_eq!(shell_single_quote_escape("plain"), "plain");
    }

    #[test]
    fn session_env_file_path_is_home_rooted() {
        let p = session_env_file_path(Path::new("/jail/home"), "mysess");
        assert_eq!(
            p,
            Path::new("/jail/home/.quorum/dispatch/session-env/mysess.env")
        );
    }

    #[test]
    fn session_env_prefix_empty_when_no_capture() {
        // Empty ONLY when there are no exports AND no unsets.
        assert_eq!(session_env_prefix(Path::new("/h"), "s", &[], &[]), "");
    }

    /// R2: an UNSET-ONLY pair set (an --alt-screen launch with no backend env
    /// and no id) still produces a sourcing prefix — keying emptiness on the
    /// exports alone would skip the file and the `unset -v` would never apply.
    #[test]
    fn session_env_prefix_nonempty_for_unsets_only() {
        let unsets = vec![ALT_SCREEN_DISABLE_KEY.to_string()];
        let prefix = session_env_prefix(Path::new("/jail/home"), "mysess", &[], &unsets);
        assert!(
            prefix.contains("/jail/home/.quorum/dispatch/session-env/mysess.env"),
            "unset-only file must be sourced: {prefix:?}"
        );
        assert!(prefix.contains("exit 97"));
    }

    /// R2 (override-never-inherit): AltScreen demands the explicit unset;
    /// Inline demands none (its export clobbers anything inherited).
    #[test]
    fn render_env_unsets_alt_screen_unsets_inline_does_not() {
        assert_eq!(
            render_env_unsets(RenderMode::AltScreen),
            vec![ALT_SCREEN_DISABLE_KEY.to_string()]
        );
        assert!(render_env_unsets(RenderMode::Inline).is_empty());
    }

    #[test]
    fn session_env_prefix_self_deletes_and_hides_values() {
        let cap = vec![("ANTHROPIC_API_KEY".to_string(), "sk-secret".to_string())];
        let prefix = session_env_prefix(Path::new("/jail/home"), "mysess", &cap, &[]);
        // The file PATH appears; the VALUE never does.
        assert!(prefix.contains("/jail/home/.quorum/dispatch/session-env/mysess.env"));
        assert!(
            !prefix.contains("sk-secret"),
            "value must not leak into prefix"
        );
        // Source fail-closed (exit 97) + rm -f self-delete (D1).
        assert!(prefix.contains("exit 97"));
        assert!(prefix.contains("rm -f --"));
    }

    #[cfg(unix)]
    #[test]
    fn write_session_env_file_is_0600_and_self_consistent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cap = vec![
            ("ANTHROPIC_BASE_URL".to_string(), "http://r".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "a'b".to_string()),
        ];
        let path = write_session_env_file(dir.path(), "sess", &cap).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "env file must be chmod 0600");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("export ANTHROPIC_BASE_URL='http://r'"));
        // The single quote in the value is escaped.
        assert!(body.contains("export ANTHROPIC_API_KEY='a'\\''b'"));

        // Re-write (S1 unlink-first path) replaces cleanly, still 0600.
        let path2 = write_session_env_file(dir.path(), "sess", &cap).unwrap();
        assert_eq!(path, path2);
        let mode2 = std::fs::metadata(&path2).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode2, 0o600);

        // remove is best-effort and idempotent.
        remove_session_env_file(dir.path(), "sess");
        assert!(!path.exists());
        remove_session_env_file(dir.path(), "sess"); // no panic on absent
    }

    // --- punch item 7: render-mode birth property -------------------------

    /// Default (inline) launches inject CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1
    /// into the env-pair set; the ONLY delta vs the pre-punch-7 set is that one
    /// var, and QD_SESSION_ID still lands LAST.
    #[test]
    fn inline_render_injects_alt_screen_disable_var() {
        let pairs = launch_env_pairs(
            vec![("ANTHROPIC_BASE_URL".to_string(), "http://r".to_string())],
            Some("ab3kx9mq".to_string()),
            RenderMode::Inline,
        );
        assert_eq!(
            pairs,
            vec![
                ("ANTHROPIC_BASE_URL".to_string(), "http://r".to_string()),
                (FORCE_SESSION_PERSISTENCE_KEY.to_string(), "1".to_string()),
                (ALT_SCREEN_DISABLE_KEY.to_string(), "1".to_string()),
                ("QD_SESSION_ID".to_string(), "ab3kx9mq".to_string()),
            ]
        );
    }

    /// --alt-screen launches OMIT the alt-screen-disable var entirely (the
    /// opt-out is "not injected", never "=0"). FORCE-persistence still rides
    /// (it is render-independent) and QD_SESSION_ID still lands last.
    #[test]
    fn alt_screen_render_omits_disable_var() {
        let pairs = launch_env_pairs(
            vec![("ANTHROPIC_BASE_URL".to_string(), "http://r".to_string())],
            Some("ab3kx9mq".to_string()),
            RenderMode::AltScreen,
        );
        assert_eq!(
            pairs,
            vec![
                ("ANTHROPIC_BASE_URL".to_string(), "http://r".to_string()),
                (FORCE_SESSION_PERSISTENCE_KEY.to_string(), "1".to_string()),
                ("QD_SESSION_ID".to_string(), "ab3kx9mq".to_string()),
            ]
        );
        assert!(!pairs.iter().any(|(k, _)| k == ALT_SCREEN_DISABLE_KEY));
    }

    /// Inline injection holds even with NO backend env and NO id — the two
    /// engine birth properties (FORCE then alt-screen) ride every launch, so the
    /// env file is always non-empty.
    #[test]
    fn inline_render_injects_even_with_no_other_pairs() {
        let pairs = launch_env_pairs(vec![], None, RenderMode::Inline);
        assert_eq!(
            pairs,
            vec![
                (FORCE_SESSION_PERSISTENCE_KEY.to_string(), "1".to_string()),
                (ALT_SCREEN_DISABLE_KEY.to_string(), "1".to_string()),
            ]
        );
    }

    /// Item-1 pin (board STATE 130): `CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1`
    /// rides EVERY launch UNCONDITIONALLY — every render mode, with or without
    /// backend routing, with or without a minted id — and exactly once. This is
    /// the carrier for the nested-spawn registration fix: a spawn that inherits
    /// CHILD_SESSION must still force-write its registry row, or the boot waiter
    /// times out. Fails (var absent) against the pre-fix assembly.
    #[test]
    fn force_session_persistence_rides_every_launch_unconditionally() {
        let force_count = |pairs: &[(String, String)]| {
            pairs
                .iter()
                .filter(|(k, v)| k == FORCE_SESSION_PERSISTENCE_KEY && v == "1")
                .count()
        };
        // Every combination of {render mode} × {id present?} × {backend present?}.
        for render in [RenderMode::Inline, RenderMode::AltScreen] {
            for id in [None, Some("ab3kx9mq".to_string())] {
                for backend in [
                    vec![],
                    vec![("ANTHROPIC_BASE_URL".to_string(), "http://r".to_string())],
                ] {
                    let pairs = launch_env_pairs(backend.clone(), id.clone(), render);
                    assert_eq!(
                        force_count(&pairs),
                        1,
                        "FORCE_SESSION_PERSISTENCE=1 must appear exactly once \
                         (render={render:?}, id={id:?}, backend={backend:?}): {pairs:?}"
                    );
                }
            }
        }
    }

    /// Precedence pin (punch item 7): per-start flag > render-default config >
    /// built-in default (inline). Both flag directions beat both config values
    /// (the --inline mirror flag exists so config=alt-screen still has a
    /// per-session way back to inline).
    #[test]
    fn render_mode_precedence_flag_over_config_over_default() {
        use RenderMode::*;
        // Built-in default: inline.
        assert_eq!(resolve_render_mode(None, None), Inline);
        // Config flips the default for the machine.
        assert_eq!(resolve_render_mode(None, Some("alt-screen")), AltScreen);
        assert_eq!(resolve_render_mode(None, Some("inline")), Inline);
        // Flag beats config — BOTH directions.
        assert_eq!(
            resolve_render_mode(Some(AltScreen), Some("inline")),
            AltScreen
        );
        assert_eq!(
            resolve_render_mode(Some(Inline), Some("alt-screen")),
            Inline
        );
        assert_eq!(resolve_render_mode(Some(AltScreen), None), AltScreen);
        // Unknown config value (hand-edited file) falls through to inline (L8).
        assert_eq!(resolve_render_mode(None, Some("fullscreen")), Inline);
        assert_eq!(resolve_render_mode(None, Some("")), Inline);
    }

    // --- A7 F12 fix: explicit unset lines kill inherited/profile-re-exported
    // credential slots (Lima 2026-06-05 finding: composed-pair removal alone let
    // a caller-exported ANTHROPIC_API_KEY ride into a profile-secret child).
    #[test]
    fn env_file_with_unsets_emits_unset_before_exports() {
        let dir = tempfile::tempdir().unwrap();
        let cap = vec![
            ("ANTHROPIC_BASE_URL".to_string(), "http://r".to_string()),
            ("ANTHROPIC_AUTH_TOKEN".to_string(), "sk-FAKE-t".to_string()),
        ];
        let unsets = vec![
            "ANTHROPIC_API_KEY".to_string(),
            "ANTHROPIC_MODEL".to_string(),
        ];
        let path = write_session_env_file_with_unsets(dir.path(), "sess", &cap, &unsets).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let unset_pos = body.find("unset -v ANTHROPIC_API_KEY").expect("unset line");
        assert!(body.contains("unset -v ANTHROPIC_MODEL"));
        let export_pos = body.find("export ANTHROPIC_BASE_URL").expect("export line");
        // Order is load-bearing: unsets FIRST (an unset after the export would
        // delete the freshly-set value).
        assert!(
            unset_pos < export_pos,
            "unset lines must precede exports: {body}"
        );
        // The dropped slot is never re-exported.
        assert!(!body.contains("export ANTHROPIC_API_KEY"));
    }

    #[test]
    fn env_file_without_unsets_is_byte_identical_to_legacy_shape() {
        // PARITY GUARD: the non-via (F1) surface must not change — the plain fn
        // and the with-unsets fn with empty unsets produce identical bodies, and
        // the body contains NO unset lines (utils.ts:668-680 never unsets).
        let dir = tempfile::tempdir().unwrap();
        let cap = vec![("ANTHROPIC_API_KEY".to_string(), "k".to_string())];
        let p1 = write_session_env_file(dir.path(), "s1", &cap).unwrap();
        let b1 = std::fs::read_to_string(&p1).unwrap();
        let p2 = write_session_env_file_with_unsets(dir.path(), "s2", &cap, &[]).unwrap();
        let b2 = std::fs::read_to_string(&p2).unwrap();
        assert_eq!(b1, b2);
        assert!(!b1.contains("unset"));
        assert_eq!(b1, "export ANTHROPIC_API_KEY='k'\n");
    }
}
