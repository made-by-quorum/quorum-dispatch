//! `provider::pi::tui` — identity + on-disk facts for a pi session running as a
//! TUI in a mux pane (`qd start <name> --provider pi --interactive`).
//!
//! THE CODEX PROBLEM, AND WHY PI DOES NOT HAVE IT. [`super::super::codex::tui`]
//! exists because a codex TUI discloses no identity at launch: it opens its
//! rollout at the first human interaction (measured: a session left at its
//! composer ran 164 seconds with nothing on disk), so `qd start` cannot wait for
//! a thread id, and the id has to be DISCOVERED later by attribution — with a
//! whole unique-or-nothing apparatus to keep a stranger's conversation from being
//! adopted.
//!
//! pi needs none of that, because pi lets the LAUNCHER NAME THE SESSION:
//!
//! ```text
//! --session-id <id>    Use exact project session ID, creating it if missing
//! ```
//!
//! Verified at source (pi 0.80.2, `dist/main.js` `createSessionManager`): with
//! `--session-id`, pi looks for a local session with that EXACT id; finding one it
//! OPENS it, finding none it creates a session carrying that id
//! (`SessionManager.create(cwd, sessionDir, { id })`). So identity is DICTATED,
//! not discovered — and the same flag serves both lanes:
//!
//!   - **fresh start** — we mint an id, pass it, and the row is identified from
//!     the first instant. There is no window in which the session cannot be
//!     addressed, no backfill in the gather step, and structurally no way to
//!     adopt someone else's conversation.
//!   - **revive** — we pass the row's recorded id back, and pi reopens that exact
//!     conversation.
//!
//! MINTING, AND THE ONE HAZARD THAT SURVIVES. Because `--session-id` OPENS an
//! existing session rather than failing, a minted id that collided with a session
//! already in this project directory would silently adopt it — the codex
//! misattribution hazard, re-entering through the one door left open. Two guards
//! close it: [`mint_session_id`] draws a v4 UUID (collision-free by construction),
//! and the create path additionally refuses to launch when
//! [`session_id_is_taken`] says that id already exists here. The second is
//! belt-and-braces over the first, and costs one directory read.
//!
//! WHEN THE TRANSCRIPT EXISTS (verified at source, pi 0.80.2,
//! `dist/core/session-manager.js`). `_persist(entry)` runs on EVERY appended
//! entry, and branches on whether the buffer already holds an assistant message:
//!
//! ```text
//! hasAssistant = fileEntries.some(e => e.type === "message" && e.message.role === "assistant")
//!   !hasAssistant && !flushed  → buffer in memory; NO FILE ON DISK
//!   !hasAssistant &&  flushed  → appendFileSync(entry)
//!    hasAssistant && !flushed  → openSync(file,"wx"); write ALL buffered entries; flushed = true
//!    hasAssistant &&  flushed  → appendFileSync(entry)
//! ```
//!
//! Two consequences this lane depends on, and one correction:
//!
//!   1. **A fresh session writes nothing until its first assistant reply.** The
//!      header, and the user message that provoked the reply, land together at
//!      that moment. Everything downstream that reads the transcript — turns,
//!      preview, `qd send`'s landing verify — is blind until then, and says so
//!      rather than guessing.
//!   2. **After that, every entry appends IMMEDIATELY.** A session reopened from
//!      an existing file (`setSessionFile` → `flushed = true`) appends from its
//!      very first entry, before any assistant reply.
//!   3. **CORRECTION.** `qrmux::attended::driver` recorded pi's transcript as
//!      "append-on-exit", and gated PTY delivery off partly on that basis. It is
//!      not: it is append-per-entry, deferred only until the first assistant
//!      message exists. A live user message IS observable on disk, which is what
//!      lets pi carry [`qrmux::attended::fire::AcceptanceSignal::Landing`] — the
//!      same landing-as-acceptance proof codex uses.
//!
//! L8/L9a: every read here is permissive (an unreadable sessions root is "not
//! taken", never fatal) and the sessions root is INJECTED — nothing here resolves
//! a home.

use std::path::Path;

/// Is `id` a session id pi will accept?
///
/// Ported verbatim from pi 0.80.2 `dist/core/session-manager.js`
/// `assertValidSessionId`:
///
/// ```text
/// /^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$/
/// ```
///
/// i.e. non-empty, alphanumeric/`.`/`_`/`-` throughout, and alphanumeric at both
/// ends. We check it OURSELVES rather than letting pi reject the argv, because
/// pi's rejection is a `process.exit(1)` inside a freshly-spawned mux pane: the
/// pane would flash and die, and the create path would report a boot failure
/// naming nothing useful. Checking here turns that into a refusal that names the
/// id.
pub fn is_valid_session_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false; // empty
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    let Some(last) = rest.last() else {
        return true; // a single alphanumeric char is valid
    };
    if !last.is_ascii_alphanumeric() {
        return false;
    }
    rest.iter()
        .all(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
}

/// Mint a session id for a fresh interactive pi session: a v4 UUID.
///
/// WHY A UUID and not qd's own 8-char stable id ([`crate::idstore`]): this value
/// is handed to `--session-id`, which OPENS an existing session of that name
/// instead of failing, so a collision is not a loud error but a silent adoption
/// of someone else's conversation. A v4 UUID makes that collision-free by
/// construction; an 8-char id from a 32-symbol alphabet does not (~1.1e12, and
/// the two id spaces would then also be conflated).
///
/// The generation replicates the crate's existing v4 pattern
/// (`relay_server::random_uuid_v4`, itself replicated into `idstore::random_id`)
/// — urandom, with a pid+nanos fallback so we never emit an empty or
/// duplicate-prone id when `/dev/urandom` is unreadable. The output is
/// hyphenated lowercase hex, which [`is_valid_session_id`] accepts.
pub fn mint_session_id() -> String {
    let mut bytes = [0u8; 16];
    if read_urandom(&mut bytes).is_err() {
        // Degenerate fallback: derive 16 bytes from pid + nanos. Not
        // cryptographic; the urandom path is the norm.
        let seed = (std::process::id() as u128) << 64
            | (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
                & 0xFFFF_FFFF_FFFF_FFFF);
        bytes.copy_from_slice(&seed.to_le_bytes());
    }
    // RFC 4122: version 4 in the high nibble of byte 6; variant 10xx in byte 8.
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11],
        bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// Fill `buf` from `/dev/urandom` (the `relay_server`/`idstore` pattern; both
/// copies are private to their modules).
fn read_urandom(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

/// Does this pi binary support `--session-id`?
///
/// WHY THIS PREFLIGHT EXISTS, and why the lane must not launch without it.
/// `--session-id` is the entire mechanism of the interactive lane: it is how qd
/// names the session, how the row is identified from birth, and how a revive
/// reopens the same conversation. It is also NOT ancient — pi 0.74.2 has no such
/// flag, and answers `Error: Unknown option: --session-id` and exits.
///
/// Without this check the failure is genuinely awful to diagnose, because it
/// happens INSIDE a freshly-spawned mux pane: pi prints its error to a pane
/// nobody is attached to and dies instantly, the pane dies with it, and `qd start`
/// reports whatever its attachability verify happened to observe — a message about
/// panes and registries that says nothing about the actual cause. The user is left
/// with a create path that "doesn't work" and no thread to pull. Found exactly that
/// way, against an asdf-managed pi 0.74.2 that shadowed a correctly-installed
/// 0.80.2 on PATH.
///
/// A CAPABILITY PROBE, not a version compare, and deliberately so. What the lane
/// needs is the flag, not a number: pinning to [`super::pin::PINNED_VERSION`] would
/// refuse a future 0.81 that supports `--session-id` perfectly well, and would
/// still pass a hypothetical build that reported the right version without the
/// flag. `--help` names its own options, so we ask it directly. (The exact-version
/// pin still exists and still matters — it guards the RPC wire the daemon lane
/// rides, which is a different contract; see [`super::pin`].)
///
/// Costs one `pi --help` — measured at ~340ms against a real pi, paid once per
/// interactive create, never on the daemon lane. `Err` carries a description of
/// WHY we could not tell (the binary is missing, unrunnable, or timed out), which
/// the caller reports verbatim rather than guessing.
///
/// BOUNDED, and that is not defensive padding. This probe RUNS whatever
/// `QD_PI_BIN`/PATH resolves to, and a binary that does not recognise `--help` is
/// entirely free to sit there — a stand-in that `exec sleep`s is the obvious case,
/// but so is anything waiting on a terminal. An unbounded `output()` would then
/// hang `qd start` forever, which is a strictly worse failure than the dead pane
/// this check exists to prevent: at least the dead pane came back. So the child is
/// polled to a deadline and killed with an EXACT-CHILD kill on expiry (never a
/// group signal — the `floor::run_floor_turn` binding), and a timeout is an
/// honest "could not tell", not a verdict in either direction.
pub fn supports_session_id(pi_bin: &Path) -> Result<bool, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new(pi_bin)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {} --help: {e}", pi_bin.display()))?;

    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{} --help did not answer within {}s",
                        pi_bin.display(),
                        PROBE_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("waiting on {} --help: {e}", pi_bin.display()));
            }
        }
    }

    // Read only AFTER the child has exited, so neither pipe can block us. `--help`
    // output is far below the pipe buffer, so nothing is lost to that ordering.
    // pi prints help to stdout; stderr is read too so a build that routes it there
    // is not misread as "flag absent" (the safe direction is to SEE it).
    let mut text = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut text);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut text);
    }
    Ok(text.contains("--session-id"))
}

/// How long the capability probe waits for `pi --help`. Generous against the
/// ~340ms a real pi takes (node startup is not fast, and a loaded machine is
/// slower still), while keeping a non-answering binary from wedging `qd start`.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// This pi binary's self-reported version, best-effort, for error messages only.
///
/// Never a gate — [`supports_session_id`] is the gate. This exists so a refusal can
/// say WHICH pi it found, which is the fact that makes a shadowed-binary problem
/// (an old pi earlier on PATH than the intended one) solvable rather than
/// mysterious. `None` when the binary cannot be run or says nothing useful.
pub fn probe_version(pi_bin: &Path) -> Option<String> {
    // Only ever reached after `supports_session_id` has already run this binary to
    // completion within the probe timeout, so a plain `output()` here cannot be the
    // thing that wedges a create. Still `None` on any failure — this decorates an
    // error message and must never become one.
    let out = std::process::Command::new(pi_bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(v).filter(|v| !v.is_empty())
}

/// Does a pi session with this EXACT id already exist for this cwd?
///
/// The pre-launch guard on the one hazard `--session-id` leaves open (see the
/// module doc): pi would silently OPEN such a session instead of creating ours,
/// pointing this row's transcript, turns and `qd stop` at a conversation we did
/// not start.
///
/// Scoped to the cwd bucket because that is pi's own scope — `main.js` resolves
/// the flag through `findLocalSessionByExactId(id, cwd, sessionDir)`, i.e. the
/// project directory, not the whole store. `cwd` is CANONICALIZED first: pi
/// encodes the cwd its process RESOLVED into the directory name
/// (`--private-tmp-foo--`), while the caller passes what it was given
/// (`/tmp/foo`) — see [`crate::provider::canonical_dir`].
///
/// Permissive (L8): an unreadable/missing root reads as NOT taken. That is the
/// safe direction here — the guard is a second line of defence behind a v4 UUID,
/// and a store we cannot read is also one pi is not resolving a session out of.
pub fn session_id_is_taken(sessions_root: &Path, cwd: &str, id: &str) -> bool {
    let cwd = crate::provider::canonical_dir(cwd);
    super::session::find_session_file(sessions_root, id, Some(&cwd)).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // --- is_valid_session_id: pi's regex, both directions --------------------

    #[test]
    fn accepts_the_ids_we_actually_mint() {
        for _ in 0..100 {
            let id = mint_session_id();
            assert!(is_valid_session_id(&id), "minted id must be valid: {id:?}");
        }
    }

    #[test]
    fn minted_ids_have_the_canonical_uuid_v4_shape() {
        let id = mint_session_id();
        assert_eq!(id.len(), 36, "uuid len: {id}");
        let groups: Vec<&str> = id.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "8-4-4-4-12: {id}"
        );
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        // Version 4 and the RFC 4122 variant nibble.
        assert_eq!(groups[2].chars().next(), Some('4'), "version nibble: {id}");
        assert!(
            matches!(groups[3].chars().next(), Some('8' | '9' | 'a' | 'b')),
            "variant nibble: {id}"
        );
    }

    #[test]
    fn minted_ids_do_not_repeat() {
        // A collision would mean silently adopting another session (see the module
        // doc) — the property the whole mint-a-uuid choice rests on.
        let a = mint_session_id();
        let b = mint_session_id();
        assert_ne!(a, b);
    }

    #[test]
    fn accepts_the_shapes_pis_regex_accepts() {
        for ok in [
            "a",
            "A9",
            "019e9f4b-adb9-7ec1-b4ed-08247847426a",
            "my.session_name-1",
            "0",
        ] {
            assert!(is_valid_session_id(ok), "should accept: {ok:?}");
        }
    }

    #[test]
    fn rejects_the_shapes_pis_regex_rejects() {
        // Each of these makes pi `process.exit(1)` in the freshly-spawned pane; we
        // refuse first so the failure names the id instead of a dead pane.
        for bad in [
            "",         // empty
            "-lead",    // must START alphanumeric
            ".lead",    //
            "trail-",   // must END alphanumeric
            "trail_",   //
            "has space",
            "has/slash",
            "has:colon",
        ] {
            assert!(!is_valid_session_id(bad), "should reject: {bad:?}");
        }
    }

    // --- supports_session_id: the capability preflight -----------------------
    //
    // This gate exists because pi 0.74.2 has no `--session-id` and dies inside a
    // freshly-spawned pane, which is a miserable failure to diagnose. The stubs
    // below stand in for the three answers a binary can give.

    fn write_stub(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[test]
    fn a_pi_that_advertises_the_flag_is_accepted() {
        let tmp = TempDir::new().unwrap();
        let bin = write_stub(
            tmp.path(),
            "pi-new",
            "#!/bin/sh\necho '  --session-id <id>   Use exact project session ID'\n",
        );
        assert_eq!(supports_session_id(&bin), Ok(true));
    }

    #[test]
    fn a_pi_without_the_flag_is_refused_not_launched() {
        // pi 0.74.2's actual shape: a help text listing --session and
        // --session-dir, but no --session-id.
        let tmp = TempDir::new().unwrap();
        let bin = write_stub(
            tmp.path(),
            "pi-old",
            "#!/bin/sh\necho '  --session <path|id>   Use specific session file'\n\
             echo '  --session-dir <dir>   Directory for session storage'\n",
        );
        assert_eq!(supports_session_id(&bin), Ok(false));
    }

    #[test]
    fn help_on_stderr_still_counts_as_advertising_the_flag() {
        // The safe direction is to SEE the flag: a build that routes help to
        // stderr must not be misread as lacking it.
        let tmp = TempDir::new().unwrap();
        let bin = write_stub(
            tmp.path(),
            "pi-stderr",
            "#!/bin/sh\necho '  --session-id <id>' >&2\n",
        );
        assert_eq!(supports_session_id(&bin), Ok(true));
    }

    #[test]
    fn a_missing_binary_is_cannot_tell_not_a_verdict() {
        // Neither `true` (which would restore the dead-pane failure) nor `false`
        // (which would block a working setup we simply could not probe).
        let tmp = TempDir::new().unwrap();
        let err = supports_session_id(&tmp.path().join("no-such-pi")).unwrap_err();
        assert!(err.contains("could not run"), "unhelpful error: {err}");
    }

    /// A binary that never answers `--help` must TIME OUT, not wedge the create.
    ///
    /// THE REGRESSION THIS GUARDS, found the hard way: the first version of this
    /// probe used a plain `Command::output()`, and the test stand-in `exec sleep
    /// 600`s on every argv — so `qd start` hung for ten minutes instead of
    /// failing. An unbounded probe is a strictly worse failure than the dead pane
    /// it was added to prevent; at least the dead pane came back.
    ///
    /// MUTATION EVIDENCE: replacing the poll loop with `output()` hangs this test.
    #[test]
    fn a_binary_that_never_answers_times_out_rather_than_wedging() {
        let tmp = TempDir::new().unwrap();
        let bin = write_stub(tmp.path(), "pi-wedged", "#!/bin/sh\nexec sleep 600\n");

        // Deliberately asserts the BOUND, not just the error: the point is that
        // this returns at all.
        let started = std::time::Instant::now();
        let err = supports_session_id(&bin).unwrap_err();
        let elapsed = started.elapsed();

        assert!(err.contains("did not answer"), "unhelpful error: {err}");
        assert!(
            elapsed < PROBE_TIMEOUT + std::time::Duration::from_secs(5),
            "probe took {elapsed:?} — it must be bounded by PROBE_TIMEOUT"
        );
    }

    // --- session_id_is_taken: the anti-adoption guard -------------------------

    /// Write a session file the way PI would: under the bucket for the cwd its
    /// own process RESOLVED. On macOS a `TempDir` lives under `/var/folders/...`,
    /// which resolves to `/private/var/folders/...` — so a helper that skipped
    /// this would write to a bucket the resolved lookup never reads, and every
    /// assertion below would pass or fail for the wrong reason.
    fn write_session(root: &Path, cwd: &str, ts: &str, id: &str) {
        let resolved = crate::provider::canonical_dir(cwd);
        let dir = root.join(super::super::session::encode_cwd_dir(&resolved));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{ts}_{id}.jsonl")), "{}\n").unwrap();
    }

    #[test]
    fn an_existing_session_in_this_cwd_is_taken() {
        let tmp = TempDir::new().unwrap();
        let work = tmp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let root = tmp.path().join("sessions");
        let cwd = work.to_string_lossy().into_owned();
        write_session(&root, &cwd, "2026-08-07T00-00-00-000Z", "taken-id");

        assert!(session_id_is_taken(&root, &cwd, "taken-id"));
        assert!(!session_id_is_taken(&root, &cwd, "some-other-id"));
    }

    #[test]
    fn the_same_id_in_a_different_cwd_is_not_taken() {
        // pi resolves --session-id through `findLocalSessionByExactId(id, cwd, …)`
        // — the PROJECT directory. A session of this id under another project is
        // not the one pi would open, so it must not block our launch.
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("proj-a");
        let b = tmp.path().join("proj-b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let root = tmp.path().join("sessions");
        write_session(&root, &a.to_string_lossy(), "2026-08-07T00-00-00-000Z", "id-9");

        assert!(session_id_is_taken(&root, &a.to_string_lossy(), "id-9"));
        assert!(!session_id_is_taken(&root, &b.to_string_lossy(), "id-9"));
    }

    #[test]
    fn a_cwd_spelled_through_a_symlink_still_reads_as_taken() {
        // THE defect codex's lane hit end-to-end, which bites pi HARDER: pi encodes
        // the RESOLVED cwd into the directory NAME, so an unnormalized compare does
        // not merely fail to match — it looks in a directory that does not exist.
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real-work");
        fs::create_dir_all(&real).unwrap();
        let link = tmp.path().join("linked-work");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let root = tmp.path().join("sessions");
        // pi writes under the resolved path...
        write_session(&root, &real.to_string_lossy(), "2026-08-07T00-00-00-000Z", "id-sym");
        // ...while the caller passes the path it was given.
        assert!(
            session_id_is_taken(&root, &link.to_string_lossy(), "id-sym"),
            "the same directory spelled two ways must still see the session"
        );
    }

    #[test]
    fn a_missing_store_is_not_taken_and_not_an_error() {
        let tmp = TempDir::new().unwrap();
        assert!(!session_id_is_taken(
            &tmp.path().join("nope"),
            "/work",
            "id-1"
        ));
    }
}
