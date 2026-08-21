//! REAL `qd setup` backend (punch list **R15** + **C2**) — the first-run entry
//! a human runs after `brew install`.
//!
//! Thin binding over the pure [`dispatch::setup`] library, exactly as
//! `verbs/bootstrap.rs` binds [`dispatch::bootstrap`]: this file GATHERS facts
//! through the real seams (fs / `RealExec` / `$PATH` / TTY) and APPLIES the
//! remedies the pure [`assess`] attached. It contains no decisions — every
//! "is this ok" question is answered in the library, where it is unit-tested
//! against a temp home.
//!
//! # Exit-code contract
//!
//! **0** when everything required is in place, or was put in place this run;
//! **1** when something required is still missing after the fix pass — which
//! includes the cases `--fix` cannot resolve on its own (a broken Homebrew
//! install, an unparsable `~/.claude.json`). See
//! [`dispatch::setup::verdict::SetupReport::exit_code`].
//!
//! # Interactivity
//!
//! No new prompt dependency: the one prompt is `tty::prompt_yes_no_default_no`,
//! the same hand-rolled `[y/N]` read `qd bootstrap`'s consent-gated steps use.
//! A NON-TTY run never prompts and never hangs — it reports and exits, the same
//! posture as bootstrap. `--fix` and `-y` both mean "do not ask"; `--json`
//! implies report-only (see below).

use std::path::{Path, PathBuf};

use clap::ArgMatches;

use dispatch::effects::{Env, RealEnv};
use dispatch::exec::{Exec, RealExec};
use dispatch::setup::harness::{self, HarnessFacts, HarnessId, Presence};
use dispatch::setup::layout::{self, InstallChannel, QuorumLayout, COLOCATED_INTERNAL, SIBLING_ANCHOR};
use dispatch::setup::relay_pin;
use dispatch::setup::verdict::{Remedy, SetupReport, Status};
use dispatch::setup::{assess, to_json, SetupFacts};
use dispatch::shell_init::{rc_path, Shell};

use super::super::tty;

/// A bounded deadline for every harness probe. A wedged `codex`/`pi` must never
/// hang a setup run (the L3 discipline the zmx probe established).
const PROBE_TIMEOUT_MS: u64 = 10_000;

pub fn run(m: &ArgMatches) -> i32 {
    run_with(m.get_flag("fix"), m.get_flag("json"), m.get_flag("yes"))
}

fn run_with(fix: bool, json: bool, yes: bool) -> i32 {
    let code = run_report(fix, json, yes);

    // --- FTUE punch R23: a completed setup ENDS IN THE HELP -----------------
    //
    // Setup used to end by dropping the human back at a shell prompt having said
    // nothing about what to type there. The natural next keystroke is a bare
    // `qd`, which ran `ls` — so the first sentence a freshly set-up machine
    // spoke was "No sessions found.", which is true, useless, and reads like a
    // failure. Printing the four-verb table instead answers the question the
    // human actually has. (Bare `qd` prints this same table now, which makes the
    // two surfaces agree rather than making this one redundant: setup is also
    // reached directly, and a run that ends in silence is the defect.)
    //
    // ONLY ON SUCCESS: a failing setup's last words must be its own — the check
    // that is still red and the remedy under it — not a verb table pushing them
    // up the scrollback. And never under `--json`, where stdout is a document.
    if help_tail_follows(code, json) {
        println!("\n[setup] Setup is complete. Here is the whole session surface:");
        // `false`: this tail only follows an exit-0 run, which IS the finished
        // install — re-probing to print "not fully set up" under a report that
        // just said everything is in place would be the contradiction.
        print!("{}", crate::help::render_top(&crate::cli::build_cli(), false, false));
    }
    code
}

/// R23's whole decision, as a value: does the verb table follow this run?
///
/// Two lines of `&&`, split out because both conditions are load-bearing and
/// neither is obvious from the call site. It is also the shape a mutation
/// notices — flipping either clause reds `help_tail_only_follows_a_clean_human_run`.
fn help_tail_follows(exit_code: i32, json: bool) -> bool {
    exit_code == 0 && !json
}

/// The report/fix pass itself — everything `qd setup` did before R23 bolted the
/// help onto the end of a successful one.
fn run_report(fix: bool, json: bool, yes: bool) -> i32 {
    let env = RealEnv;
    let exec = RealExec;

    let home = match env.var("HOME").filter(|s| !s.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("qd setup: HOME is not set — cannot resolve the install layout.");
            return 1;
        }
    };

    let facts = gather(&home, &env, &exec);
    let report = assess(&facts);

    // --json is REPORT-ONLY, deliberately. `qd bootstrap` (which the engine-dir
    // remedy shells into) writes its own `[bootstrap]` lines to stdout, which
    // would corrupt the JSON document; and a machine-readable "what is the
    // state" query should not have side effects. A caller that wants both runs
    // `qd setup --json` and then `qd setup --fix`.
    if json {
        println!("{}", serde_json::to_string_pretty(&to_json(&facts, &report)).unwrap_or_default());
        return report.exit_code();
    }

    print!("{}", report.render());

    if report.fixable().is_empty() {
        return report.exit_code();
    }

    // Decide whether to apply. --fix / -y say yes outright; otherwise a TTY is
    // asked once, and a non-TTY is told what to run and exits.
    let interactive = tty::stdin_and_stdout_are_tty();
    let apply = if fix || yes {
        true
    } else if interactive && report.has_automatic_fixes() {
        tty::prompt_yes_no_default_no("[setup] Apply the fixes above now? [y/N] ")
    } else {
        false
    };

    if !apply {
        if !fix && !yes && !interactive && report.has_automatic_fixes() {
            println!("[setup] non-interactive: nothing was changed. Re-run `qd setup --fix`.");
        }
        return report.exit_code();
    }

    let applied = apply_fixes(&report);
    for line in &applied {
        println!("{line}");
    }

    // Re-gather and re-assess against the real machine rather than assuming the
    // fixes worked. This is also what makes `qd setup --fix` idempotent: the
    // second verdict is the one that rules the exit code.
    println!("[setup] --- after fixes ---");
    let facts = gather(&home, &env, &exec);
    let report = assess(&facts);
    print!("{}", report.render());
    report.exit_code()
}


// ---------------------------------------------------------------------------
// Gather — every real-world probe, in one place.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Gather — every real-world probe, in one place.
// ---------------------------------------------------------------------------

fn gather(home: &Path, env: &impl Env, exec: &impl Exec) -> SetupFacts {
    gather_inner(home, env, Some(exec))
}

/// Is this machine's install unfinished — i.e. would `qd setup` exit 1 right now?
///
/// This is what the top-level help asks before printing its one state-dependent
/// line, so it has to be CHEAP: it runs the same pure [`assess`] over the same
/// facts MINUS the harness probes, which are the only part of `gather` that
/// shells out (four `command -v` + four `--version` runs, each with a 10s
/// ceiling — fine for a verb a person waits on, absurd for `qd --help`).
///
/// Skipping them cannot change the answer, and that is a property of the
/// decision table rather than a hope: no harness check ever returns
/// [`Status::Fail`] (a harness you do not have is not a broken install), and
/// `Fail` is the only status [`SetupReport::exit_code`] gates on. The checks
/// that DO gate — layout, engine dir, qw sibling, placement, PATH, relay pin —
/// are filesystem and env reads, and every one of them still runs here.
pub fn install_is_incomplete() -> bool {
    let env = RealEnv;
    let Some(home) = env.var("HOME").filter(|h| !h.is_empty()).map(PathBuf::from) else {
        // No HOME is exactly the machine that needs setup — but it is also a
        // machine where we cannot resolve a single path to check, so say
        // nothing rather than guess. `qd setup` itself reports it properly.
        return false;
    };
    assess(&gather_inner::<RealEnv, RealExec>(&home, &env, None)).exit_code() != 0
}

/// The gather body. `exec: None` means "skip the harness probes" — see
/// [`install_is_incomplete`] for why that is a sound shortcut and not a
/// second, drifting definition of a finished install.
fn gather_inner<E: Env, X: Exec>(home: &Path, env: &E, exec: Option<&X>) -> SetupFacts {
    let quorum = QuorumLayout::resolve(home, env.var("QD_HOME").as_deref());
    let dirs_missing = quorum.owned_dirs().into_iter().filter(|d| !d.exists()).collect();
    // `qd bootstrap`'s output: the engine data dir + its state subdir.
    let engine_dir_present = quorum.dispatch_home.join("state").exists();

    let exe = std::env::current_exe().ok();
    let exe_dir = exe.as_ref().and_then(|e| e.parent()).map(Path::to_path_buf);
    let channel = match &exe {
        Some(e) => layout::detect_channel(e, &quorum, env.var("HOMEBREW_PREFIX").as_deref()),
        None => InstallChannel::Unknown,
    };
    let qw_beside_exe = exe_dir
        .as_ref()
        .is_some_and(|d| d.join(COLOCATED_INTERNAL).exists());
    let placed_qd = quorum.bin_path(SIBLING_ANCHOR).exists();
    let placed_qw = quorum.bin_path(COLOCATED_INTERNAL).exists();
    let placed_is_stale = channel.places_binaries()
        && exe_dir.as_ref().is_some_and(|src| {
            [SIBLING_ANCHOR, COLOCATED_INTERNAL]
                .iter()
                .any(|n| newer(&src.join(n), &quorum.bin_path(n)))
        });

    let path_dir = layout::path_dir_for(channel, exe_dir.as_deref(), &quorum);
    let path_var = env.var("PATH").unwrap_or_default();
    let path_dir_on_path =
        dispatch::setup::rc_block::path_contains(&path_var, &path_dir.to_string_lossy());

    let shell = env.var("SHELL").as_deref().and_then(Shell::from_name);
    let rc = shell.map(|s| rc_path(s, home, env));
    let rc_contents = rc.as_ref().and_then(|p| std::fs::read_to_string(p).ok());

    // ORDER MATTERS: probe the harnesses BEFORE reading `~/.claude.json`.
    // Claude Code writes that file on ANY invocation — including the
    // `claude --version` probe below — so reading it first would report
    // "absent" on a fresh HOME that has a perfectly good file by the time the
    // verdict is printed. Probing first means the pin state we report is the
    // one that exists after everything setup itself caused.
    let mut harnesses: Vec<HarnessFacts> = match exec {
        Some(exec) => HarnessId::ALL
            .iter()
            .map(|id| probe_harness(*id, home, env, exec))
            .collect(),
        None => Vec::new(),
    };

    let claude_json_path = home.join(".claude.json");
    let raw = std::fs::read_to_string(&claude_json_path).ok();
    let pin_state = relay_pin::classify(raw.as_deref());
    // Only meaningful for an absolute pin; `false` otherwise and unused.
    let pin_command_exists = match &pin_state {
        relay_pin::PinState::Entry { command, .. } if command.starts_with('/') => {
            Path::new(command).exists()
        }
        _ => false,
    };

    // The one piece of harness wiring that depends on the pin: Claude Code's.
    // Reported in full by the `relay-pin` check; cross-referenced onto the
    // harness line so that line is self-contained.
    if let Some(h) = harnesses
        .iter_mut()
        .find(|h| h.id == HarnessId::ClaudeCode && h.presence.found())
    {
        let pinned = matches!(pin_state, relay_pin::PinState::Entry { .. });
        h.wired = Some(pinned);
        h.wiring_note = if pinned {
            "relay pinned in ~/.claude.json".to_string()
        } else {
            "relay NOT pinned (see the relay-pin line)".to_string()
        };
    }

    SetupFacts {
        home: home.to_path_buf(),
        layout: quorum,
        dirs_missing,
        engine_dir_present,
        exe,
        channel,
        qw_beside_exe,
        placed_qd,
        placed_qw,
        placed_is_stale,
        path_dir,
        path_dir_on_path,
        rc_path: rc,
        rc_contents,
        claude_json_path,
        pin_state,
        pin_command_exists,
        harnesses,
        qc_plugin_registered: qc_plugin_registered(home),
    }
}

/// Is `a` strictly newer than `b`? Missing/unreadable mtimes answer `false`
/// (report nothing rather than nag about something we could not measure).
fn newer(a: &Path, b: &Path) -> bool {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (mtime(a), mtime(b)) {
        (Some(x), Some(y)) => x > y,
        _ => false,
    }
}

/// Resolve a program on `PATH` (`command -v`, the same probe
/// `dispatch::bootstrap::real_command_exists` uses) and return its path.
fn which(exec: &impl Exec, name: &str) -> Option<String> {
    let arg = format!("command -v '{}'", name.replace('\'', "'\\''"));
    let out = exec
        .run("sh", &["-c".to_string(), arg], &[], None, Some(PROBE_TIMEOUT_MS))
        .ok()?;
    if out.status != Some(0) {
        return None;
    }
    let p = out.stdout.trim();
    if p.is_empty() {
        None
    } else {
        Some(p.to_string())
    }
}

/// One-shot `<program> --version`, captured. `None` on any failure/timeout.
fn version_of(exec: &impl Exec, program: &str) -> Option<String> {
    let out = exec
        .run(program, &["--version".to_string()], &[], None, Some(PROBE_TIMEOUT_MS))
        .ok()?;
    if out.timed_out || out.status != Some(0) {
        return None;
    }
    Some(out.stdout.trim().to_string()).filter(|s| !s.is_empty())
}

/// Where ONE harness lives on this machine, or that it does not. The
/// presence half of [`probe_harness`], split out because it is the only half
/// [`present_harnesses`] needs — a version sniff costs a subprocess per harness
/// and answers a question `qd start`'s provider prompt never asks.
fn harness_presence(id: HarnessId, home: &Path, env: &impl Env, exec: &impl Exec) -> Presence {
    let on_path = which(exec, id.program());
    match (&on_path, id) {
        (Some(p), _) => Presence::OnPath {
            path: Some(p.clone()),
        },
        // C5: pi is normally installed npm-global, into a prefix that is not on
        // PATH — so "not on PATH" is not the same as "not installed". Only pi
        // gets the off-PATH sweep; the other three are installed onto PATH by
        // every documented path they have.
        (None, HarnessId::Pi) => {
            match harness::pi_candidates(home, env.var("NPM_CONFIG_PREFIX").as_deref())
                .into_iter()
                .find(|c| c.exists())
            {
                Some(p) => Presence::OffPath {
                    path: p.to_string_lossy().into_owned(),
                },
                None => Presence::Missing,
            }
        }
        (None, _) => Presence::Missing,
    }
}

/// FTUE punch **R20** — which harnesses this machine actually has, in report
/// order, for `qd start`'s "which harness?" prompt.
///
/// # Why start asks setup and not the other way round
///
/// C4's complaint is that harness detection "reaches only as far as `qd setup`":
/// the substrate exists, and nothing else consults it, so spawn time is where a
/// drifted or absent harness bites. R20 is the first other consultation, and the
/// point of routing it through this file is that there is exactly ONE probe. If
/// `qd start` grew its own `command -v` sweep it would immediately be a second
/// answer to "do you have pi", and the C5 off-PATH case — pi installed
/// npm-global into a prefix that is not on PATH — is precisely the kind of thing
/// a second answer gets wrong.
///
/// PRESENCE ONLY, deliberately. [`gather`] additionally runs `--version` on
/// everything it finds, which is four subprocesses with a ten-second ceiling
/// each; that is the right cost for a verb whose whole job is the report, and
/// the wrong cost for a prompt standing between a human and their session.
/// Nothing here can hang beyond [`PROBE_TIMEOUT_MS`] per `command -v`.
pub fn present_harnesses(home: &Path, env: &impl Env, exec: &impl Exec) -> Vec<HarnessId> {
    HarnessId::ALL
        .iter()
        .copied()
        .filter(|id| harness_presence(*id, home, env, exec).found())
        .collect()
}

/// Probe one harness: presence, version where the probe is cheap, and whether
/// qd's wiring for it is in place (C2's three questions).
fn probe_harness(id: HarnessId, home: &Path, env: &impl Env, exec: &impl Exec) -> HarnessFacts {
    let presence = harness_presence(id, home, env, exec);

    let mut f = HarnessFacts::new(id, presence);
    if !f.presence.found() {
        return f;
    }

    match id {
        HarnessId::ClaudeCode => {
            f.version = version_of(exec, id.program());
            // `wired`/`wiring_note` are filled in by `gather` AFTER this probe
            // has run — see the ORDER MATTERS note there.
        }
        HarnessId::Codex => {
            // REUSE, not re-derivation: codex's own `--version` sniff + 0.x
            // drift policy (quorum_qw::provider::codex::app_server::version).
            let outcome = dispatch::provider::codex::app_server::version::sniff(exec);
            let (v, ok, note) = harness::codex_verdict(&outcome);
            f.version = v;
            f.pin_ok = ok;
            f.pin_note = note;
            // codex needs no persistent wiring: qd spawns `codex app-server`
            // per session.
            f.wiring_note = "no wiring needed (qd spawns `codex app-server` per session)".into();
        }
        HarnessId::Pi => {
            // Probe the binary we actually FOUND, not a bare `pi` — for an
            // off-PATH install those are different programs.
            let bin = f
                .presence
                .path()
                .map(str::to_string)
                .unwrap_or_else(|| id.program().to_string());
            if let Some(out) = version_of(exec, &bin) {
                let (v, ok, note) = harness::pi_verdict(&out);
                f.version = v;
                f.pin_ok = ok;
                f.pin_note = note;
            }
            let pinned = env.var(harness::PI_BIN_ENV).filter(|s| !s.is_empty());
            f.wired = Some(pinned.is_some() || matches!(f.presence, Presence::OnPath { .. }));
            f.wiring_note = match pinned {
                Some(p) => format!("{}={p}", harness::PI_BIN_ENV),
                None => format!("{} unset (qd will run a bare `pi`)", harness::PI_BIN_ENV),
            };
        }
        HarnessId::Opencode => {
            f.version = version_of(exec, id.program());
            // C4 notes opencode "has literally nothing" — and needs nothing:
            // its only live transport is the shared ACP driver bridged to
            // `opencode acp`, spawned on demand.
            f.wiring_note = "no wiring needed (qd spawns `opencode acp` per session)".into();
        }
    }
    f
}

/// C17 FYI ONLY — detect whether the `qc`/charter Claude Code plugin is
/// registered. `qrm bootstrap` INSTALLS it (`wire_charter`); `qd setup`
/// deliberately does not — see [`dispatch::setup`]'s module doc.
fn qc_plugin_registered(home: &Path) -> Option<bool> {
    let registry = home.join(".claude/plugins/installed_plugins.json");
    let raw = match std::fs::read_to_string(&registry) {
        Ok(r) => r,
        // No registry at all is a definite answer — nothing is registered —
        // not an unknown. `None` is reserved for "there is a registry and we
        // could not read it", which is the only genuinely unknown case.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let plugins = v.get("plugins")?.as_object()?;
    Some(
        plugins
            .keys()
            .any(|k| k.contains("charter") || k.contains("quorum") || k.contains("qc")),
    )
}

// ---------------------------------------------------------------------------
// Apply — the only code in `qd setup` that writes anything.
// ---------------------------------------------------------------------------

/// Execute every automatic remedy the report attached, in report order.
/// Returns the `[setup]` lines to print. `Manual` remedies are skipped (the
/// report already printed them) — which is exactly why a `Fail` carrying one
/// survives the re-assess and keeps the exit code non-zero.
fn apply_fixes(report: &SetupReport) -> Vec<String> {
    let mut lines = Vec::new();
    for c in report.fixable() {
        let remedy = match &c.remedy {
            Some(r) if r.is_automatic() => r,
            _ => continue,
        };
        let outcome = match remedy {
            Remedy::CreateDirs(dirs) => create_dirs(dirs),
            Remedy::RunBootstrap => run_bootstrap(),
            Remedy::PlaceBinaries { src_dir, dst_dir, names } => {
                place_binaries(src_dir, dst_dir, names)
            }
            Remedy::WriteRcBlock { rc, bin_dir } => write_rc_block(rc, bin_dir),
            Remedy::WriteRelayPin { path, command } => relay_pin::wire_relay_pin(path, command)
                .map(|()| format!("relay pin written to {}", path.display())),
            Remedy::Manual(_) => unreachable!("filtered by is_automatic above"),
        };
        lines.push(match outcome {
            Ok(msg) => format!("[setup] [{}] {:<14} {msg}", Status::Fixed.glyph(), c.name),
            Err(e) => format!("[setup] [{}] {:<14} {e}", Status::Fail.glyph(), c.name),
        });
    }
    lines
}

fn create_dirs(dirs: &[PathBuf]) -> Result<String, String> {
    for d in dirs {
        std::fs::create_dir_all(d).map_err(|e| format!("creating {}: {e}", d.display()))?;
    }
    Ok(format!(
        "created {}",
        dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")
    ))
}

/// Hand the engine data dir to `qd bootstrap`, which owns it. R15: setup calls
/// into the existing path rather than reimplementing it — `bootstrap::run()`
/// prints its own `[bootstrap]` report, so the caller sees exactly what it did.
fn run_bootstrap() -> Result<String, String> {
    match super::bootstrap::run() {
        0 => Ok("`qd bootstrap` completed".to_string()),
        code => Err(format!("`qd bootstrap` exited {code}")),
    }
}

/// The COPY path — from-source only (the report never attaches this remedy for
/// any other channel; see `InstallChannel::places_binaries`). Both binaries go
/// together: placing `qd` without `qw` builds the exact ADR-0020 breakage the
/// sibling check exists to catch.
fn place_binaries(src_dir: &Path, dst_dir: &Path, names: &[String]) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dst_dir).map_err(|e| format!("creating {}: {e}", dst_dir.display()))?;
    let mut placed = Vec::new();
    for name in names {
        let src = src_dir.join(name);
        let dst = dst_dir.join(name);
        if !src.exists() {
            return Err(format!(
                "{} is not in {} — build it first (`cargo build -p quorum-qw --bin qw` for qw)",
                name,
                src_dir.display()
            ));
        }
        if std::fs::canonicalize(&src).ok() == std::fs::canonicalize(&dst).ok() {
            continue; // already in place — copying a file onto itself.
        }
        std::fs::copy(&src, &dst).map_err(|e| format!("installing {name}: {e}"))?;
        let mut perm = std::fs::metadata(&dst)
            .map_err(|e| format!("stat {}: {e}", dst.display()))?
            .permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&dst, perm).map_err(|e| format!("chmod {name}: {e}"))?;
        placed.push(name.clone());
    }
    Ok(if placed.is_empty() {
        format!("{} already current", dst_dir.display())
    } else {
        format!("placed {} in {}", placed.join(" + "), dst_dir.display())
    })
}

/// Upsert the managed PATH block. Creates the file (and the fish `conf.d`
/// parent, which does not exist until first use) when absent.
fn write_rc_block(rc: &Path, bin_dir: &Path) -> Result<String, String> {
    if let Some(parent) = rc.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let existing = std::fs::read_to_string(rc).unwrap_or_default();
    let out = dispatch::setup::rc_block::upsert_block(&existing, &bin_dir.to_string_lossy());
    std::fs::write(rc, out).map_err(|e| format!("writing {}: {e}", rc.display()))?;
    Ok(format!(
        "managed PATH block in {} — open a new shell (or `source` it) to pick it up",
        rc.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- R23: the help tail ------------------------------------------------

    /// The verb table follows a CLEAN, HUMAN run and nothing else. A failing
    /// setup keeps the last word (its red check and the remedy under it), and a
    /// `--json` run's stdout stays a single parseable document.
    #[test]
    fn help_tail_only_follows_a_clean_human_run() {
        assert!(help_tail_follows(0, false));
        assert!(!help_tail_follows(1, false), "a failure keeps the last word");
        assert!(!help_tail_follows(0, true), "--json stdout is a document");
        assert!(!help_tail_follows(1, true));
    }
}
