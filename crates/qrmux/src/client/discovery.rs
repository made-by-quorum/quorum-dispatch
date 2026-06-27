//! WS-C M3a/M3b: discovery probe (spec §4.3, zmx-shaped). The engine's
//! `list`/`list_raw` call `scan_sessions` (M3b flip); the legacy
//! `list_sessions_data` shared-daemon enumeration is RETIRED.
//!
//! `scan_sessions` reads the resolved socket dir, filters the `.sock` leaves
//! (EXCLUDING the legacy `qrmux.sock`), and probes each socket with a connect,
//! preamble, Hello, and ListSessions (via [`FrameReader`], NEVER
//! `read_one_message` — two reply frames when pipelined, red-team M6). A row
//! surfaces IFF ListSessions returns at least one row (an unclaimed daemon is
//! invisible, red-team M3). CONSISTENT ConnectionRefused across confirmation
//! retries (punch item 16: 3 probes over ~350ms — one refusal can be a live
//! daemon under a full backlog) skips the row and unlinks THAT socket only
//! (never the `.log`, red-team M9); a timeout/other error skips the row with
//! NO delete (the zmx busy-daemon rule).
//!
//! **Concurrency choice: SEQUENTIAL.** Each socket is probed in turn. This is the
//! simplest correct shape and is sufficient for the M3a milestone; the §7 G-SOAK
//! gate (M5) records `ls` latency at N≥10 and a bounded-concurrency variant is a
//! measured optimization deferred to that evidence, not assumed here.
//!
//! **No `canonicalize()` anywhere (§4.4 invariant):** the dir is taken as resolved
//! by [`socket_dir_for`] and the leaf paths are joined verbatim; identity is the
//! ServerHello.session check against the leaf name, never a path-canonical compare.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use crate::protocol::{self, ClientMsg, FrameReader, ServerMsg};
use crate::server::socket::socket_dir_for;

/// The legacy shared-daemon socket leaf — EXCLUDED from the per-session scan
/// (§4.3, §5.3): it is never probed as a session.
const LEGACY_LEAF: &str = "qrmux.sock";

/// The `.sock` suffix stripped to recover a session name from a leaf.
const SOCK_SUFFIX: &str = ".sock";

/// Per-socket probe timeout (§4.3 "bounded timeout"). A busy daemon that misses
/// this deadline is SKIPPED but NOT deleted (zmx busy-daemon rule) — only a
/// definitive ConnectionRefused unlinks.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Re-exported alias for the protocol session snapshot (`name`, `pid`, dims,
/// `created`). `scan_sessions` returns these — one per LIVE, CLAIMED session.
pub use crate::protocol::SessionInfo;

/// Outcome of probing a single socket (drives the per-target hygiene rule).
enum ProbeOutcome {
    /// The daemon answered ListSessions with ≥1 row — surface them.
    Rows(Vec<SessionInfo>),
    /// The daemon answered but reported 0 rows (unclaimed/claim-window) — the
    /// row is INVISIBLE (red-team M3). No hygiene action.
    Empty,
    /// ConnectionRefused — but ONE refusal is NOT death (punch item 16,
    /// b3-kill-spec): on macOS a LIVE daemon whose listen backlog is full
    /// refuses, and unlinking a live daemon's socket orphans it permanently
    /// (wrong victim). The unlink fires only after
    /// [`probe_with_death_confirmation`] sees CONSISTENT refusal across
    /// retries with backoff — positive death evidence, not a single degraded
    /// observation. Skip the row AND unlink THIS socket (per-target hygiene,
    /// socket-only — never the `.log`, red-team M9).
    Dead,
    /// Timeout or any other error: skip the row, DO NOT delete (a busy daemon
    /// missing a deadline must never be orphaned, §4.3).
    Skip,
}

/// Scan `socket_dir` for live per-session daemons (§4.3). Returns one
/// [`SessionInfo`] per LIVE, CLAIMED session (≥1 ListSessions row). Performs
/// per-target stale-socket cleanup (ConnectionRefused only). `None` resolves the
/// env socket-dir tiers (same as the daemon/launcher).
pub async fn scan_sessions(socket_dir: Option<&Path>) -> anyhow::Result<Vec<SessionInfo>> {
    let dir = socket_dir_for(socket_dir)?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        // A missing dir = no sessions (the daemon dir is created lazily).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "failed to read socket dir {:?}: {}",
                dir,
                e
            ))
        }
    };

    // Collect (name, path) for every `*.sock` leaf EXCLUDING the legacy
    // `qrmux.sock` (§4.3 step 1). Sequential probe order (named choice).
    let mut targets: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(leaf) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if leaf == LEGACY_LEAF {
            continue; // legacy shared socket — never a session
        }
        let Some(name) = leaf.strip_suffix(SOCK_SUFFIX) else {
            continue; // not a `*.sock` leaf
        };
        if name.is_empty() {
            continue; // a bare `.sock` leaf is not a valid session name
        }
        targets.push((name.to_string(), path));
    }

    let mut rows: Vec<SessionInfo> = Vec::new();
    for (name, path) in targets {
        match probe_with_death_confirmation(&path, &name).await {
            ProbeOutcome::Rows(mut r) => rows.append(&mut r),
            ProbeOutcome::Empty | ProbeOutcome::Skip => {}
            ProbeOutcome::Dead => {
                // Per-target stale-socket cleanup: unlink THIS socket only. NEVER
                // the `.log` (red-team M9: a read-path discoverer must not widen
                // its write surface to debug files a human may be tailing).
                if let Err(e) = std::fs::remove_file(&path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::debug!(path = ?path, error = %e, "failed to clean stale socket");
                    }
                }
            }
        }
    }
    Ok(rows)
}

/// punch item 16 (b3-kill-spec): backoffs between death-confirmation
/// re-probes — 3 probes total (1 + 2 retries) over ~350ms. A macOS daemon
/// refusing under a full listen backlog accepts again once the queue drains
/// (the tokio accept loop drains in well under these backoffs); a genuinely
/// dead socket refuses all three times. Cost: a true stale socket's cleanup
/// is ~350ms slower per scan — read-path latency, never a correctness risk.
const DEAD_CONFIRM_BACKOFF_MS: [u64; 2] = [100, 250];

/// punch item 16: the unlink-eligibility wrapper around [`probe_socket`].
/// `Dead` (ConnectionRefused) escalates to the caller ONLY when every retry
/// also refuses — the orc-pinned invariant: destruction (the socket unlink)
/// requires POSITIVE death evidence, and a single refusal is one degraded
/// observation, not evidence (a live daemon under a full backlog refuses).
/// Any non-refused retry outcome (rows / empty / skip) is returned as-is:
/// the socket survives and the row logic proceeds normally. Timeouts never
/// reach here as `Dead` — they stay `Skip` (never delete), unchanged.
async fn probe_with_death_confirmation(path: &Path, name: &str) -> ProbeOutcome {
    let mut outcome = probe_socket(path, name).await;
    for backoff_ms in DEAD_CONFIRM_BACKOFF_MS {
        if !matches!(outcome, ProbeOutcome::Dead) {
            return outcome;
        }
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        outcome = probe_socket(path, name).await;
    }
    outcome
}

/// Probe one socket: connect + preamble + Hello + ListSessions, bounded by
/// [`PROBE_TIMEOUT`]. Identity-checks ServerHello.session against the leaf name.
async fn probe_socket(path: &Path, name: &str) -> ProbeOutcome {
    match tokio::time::timeout(PROBE_TIMEOUT, probe_socket_inner(path, name)).await {
        Ok(outcome) => outcome,
        // Timed out: a busy daemon missing the deadline — skip, do NOT delete.
        Err(_) => ProbeOutcome::Skip,
    }
}

async fn probe_socket_inner(path: &Path, name: &str) -> ProbeOutcome {
    let mut stream = match UnixStream::connect(path).await {
        Ok(s) => s,
        // ConnectionRefused = definitively dead → unlink THIS socket.
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return ProbeOutcome::Dead,
        // ENOENT (socket vanished between read_dir and connect) or anything else
        // → skip, no delete.
        Err(_) => return ProbeOutcome::Skip,
    };

    if protocol::write_preamble(&mut stream).await.is_err() {
        return ProbeOutcome::Skip;
    }
    // Pipeline Hello + ListSessions, then read TWO reply frames (ServerHello,
    // then SessionList) via FrameReader — NEVER read_one_message (which would
    // discard the second frame, red-team M6).
    let hello = match protocol::encode(&ClientMsg::Hello { caps: vec![] }) {
        Ok(b) => b,
        Err(_) => return ProbeOutcome::Skip,
    };
    let list = match protocol::encode(&ClientMsg::ListSessions) {
        Ok(b) => b,
        Err(_) => return ProbeOutcome::Skip,
    };
    if stream.write_all(&hello).await.is_err() || stream.write_all(&list).await.is_err() {
        return ProbeOutcome::Skip;
    }

    let mut frames = FrameReader::new();
    // Frame 1: ServerHello (identity belt).
    match read_frame(&mut frames, &mut stream).await {
        Some(ServerMsg::Hello { session, .. }) => {
            // G-ISOL negative-control seam (spec §7): under `QRMUX_TEST_SHARED=1`
            // ALL names collapse onto the ONE `shared.sock` daemon whose
            // ServerHello.session is its launch identity, never the "shared" leaf,
            // so the discovery identity belt is relaxed — the shared daemon's rows
            // (which carry their real session names) must surface for the engine's
            // post-create attachability check. Inert in production (env unset).
            if session != name && !crate::server::socket::shared_fate_test_mode() {
                // Socket-file swap/rename: a daemon serving a DIFFERENT name is
                // bound on this leaf. Skip (do NOT surface, do NOT delete — it is
                // a live daemon; deleting would orphan it).
                tracing::warn!(
                    leaf = %name,
                    actual = %session,
                    "qrmux discovery: socket leaf identity mismatch — skipping"
                );
                return ProbeOutcome::Skip;
            }
        }
        Some(ServerMsg::Error(_)) | Some(_) | None => return ProbeOutcome::Skip,
    }
    // Frame 2: SessionList.
    match read_frame(&mut frames, &mut stream).await {
        Some(ServerMsg::SessionList(rows)) => {
            // A row surfaces IFF ListSessions returns ≥1 row (red-team M3): an
            // unclaimed daemon (0 rows) is INVISIBLE.
            if rows.is_empty() {
                ProbeOutcome::Empty
            } else {
                ProbeOutcome::Rows(rows)
            }
        }
        Some(ServerMsg::Error(_)) | Some(_) | None => ProbeOutcome::Skip,
    }
}

/// Read one decoded frame from the stream via the FrameReader, returning `None`
/// on EOF or any read/decode error (the caller maps `None` to Skip).
async fn read_frame(frames: &mut FrameReader, stream: &mut UnixStream) -> Option<ServerMsg> {
    loop {
        match frames.decode_next::<ServerMsg>() {
            Ok(Some(msg)) => return Some(msg),
            Ok(None) => {}
            Err(_) => return None,
        }
        match frames.fill_from(stream).await {
            Ok(true) => {}
            Ok(false) | Err(_) => return None,
        }
    }
}
