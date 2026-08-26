//! Client integration helpers — daemon interaction, socket connection, command invocation.
//!
//! Provides high-level wrappers around qrmux CLI commands and socket-level
//! communication for test scenarios (G1–G6).
//!
//! War story: These helpers abstract PTY interaction and session management,
//! allowing tests to focus on semantic assertions (ADD-6: app-output-keyed).
//! Protocol layer (M3) uses length-prefixed bincode frames over Unix sockets (see codec.rs).

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use qrmux::protocol::codec::{encode, FrameReader};
use qrmux::protocol::{write_preamble, ClientMsg, ConnectMode, ServerMsg};

/// Protocol-level client for direct socket communication with qrmux daemon.
/// Implements length-prefixed bincode framing (see codec.rs for frame format).
/// `pub` so the WS-C M2 integration test can issue a raw `KillSession` verb (no
/// public kill helper exists; the M2 exit-on-end-via-kill arm needs it).
pub struct ProtocolClient {
    /// Connected Unix socket to daemon
    stream: tokio::net::UnixStream,
    /// Read buffer for framing protocol (length-prefix + payload)
    frame_reader: FrameReader,
}

impl ProtocolClient {
    /// Connect to the qrmux daemon socket and send the version preamble.
    /// Returns error if socket doesn't exist (daemon not running) or connection fails.
    pub async fn connect(socket_path: &Path) -> Result<Self, String> {
        // ECONNREFUSED-retry at connect (punch item 16, launcher-lane parallel).
        // `start_daemon_in_jail` returns on socket-FILE existence, not accept-
        // readiness; under full-suite load a freshly-spawned daemon can have its
        // socket bound yet momentarily refuse a connect before its accept loop is
        // scheduled — a transient ECONNREFUSED (os 111), not a dead daemon. One
        // refusal is not death: retry with backoff until it accepts. ENOENT
        // (socket gone) and any refusal past the deadline still fail honestly.
        let connect_deadline = Instant::now() + Duration::from_secs(10);
        let connect_result = loop {
            match UnixStream::connect(socket_path).await {
                Ok(s) => break Ok(s),
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionRefused
                        && Instant::now() < connect_deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => break Err(e),
            }
        };
        match connect_result {
            Ok(mut stream) => {
                tracing::debug!("[protocol] connected to socket: {}", socket_path.display());
                // Version preamble precedes all frames (PROTOCOL.md).
                write_preamble(&mut stream)
                    .await
                    .map_err(|e| format!("failed to write preamble: {}", e))?;
                // v3 Hello-first handshake (PROTOCOL.md §3.2): send Hello and
                // drain the ServerHello (the daemon's FIRST reply frame) before
                // any verb, so send_and_receive reads the verb's actual reply.
                let hello = encode(&ClientMsg::Hello { caps: vec![] })
                    .map_err(|e| format!("failed to encode Hello: {}", e))?;
                stream
                    .write_all(&hello)
                    .await
                    .map_err(|e| format!("failed to write Hello: {}", e))?;
                let mut frame_reader = FrameReader::new();
                loop {
                    if let Some(msg) = frame_reader
                        .decode_next::<ServerMsg>()
                        .map_err(|e| format!("ServerHello decode error: {}", e))?
                    {
                        match msg {
                            ServerMsg::Hello { .. } => break,
                            ServerMsg::Error(e) => {
                                return Err(format!("server refused Hello: {}", e))
                            }
                            other => return Err(format!("expected ServerHello, got {:?}", other)),
                        }
                    }
                    match frame_reader.fill_from(&mut stream).await {
                        Ok(true) => {}
                        Ok(false) => return Err("server closed before ServerHello".to_string()),
                        Err(e) => return Err(format!("ServerHello read error: {}", e)),
                    }
                }
                Ok(Self {
                    stream,
                    frame_reader,
                })
            }
            Err(e) => {
                let msg = format!(
                    "failed to connect to socket {} (is daemon running?): {}",
                    socket_path.display(),
                    e
                );
                tracing::error!("[protocol] {}", msg);
                Err(msg)
            }
        }
    }

    /// Send a client message and receive one server response.
    /// Uses the protocol's length-prefixed bincode frame format.
    pub async fn send_and_receive(&mut self, msg: ClientMsg) -> Result<ServerMsg, String> {
        // Encode message: length-prefix (4 bytes BE) + bincode payload
        let encoded = encode(&msg).map_err(|e| format!("failed to encode ClientMsg: {}", e))?;

        tracing::debug!(
            "[protocol] sending {} bytes (frame + payload)",
            encoded.len()
        );
        // A refusing server (e.g. version mismatch) may close after writing its
        // framed Error while we are still writing — surfacing EPIPE instead of
        // the refusal (observed on macOS CI). On write failure, fall through to
        // the read loop: the refusal frame may already be buffered. Only fail
        // if reading fails too.
        let write_result: Result<(), String> = async {
            self.stream
                .write_all(&encoded)
                .await
                .map_err(|e| format!("{}", e))?;
            self.stream.flush().await.map_err(|e| format!("{}", e))
        }
        .await;
        if let Err(we) = &write_result {
            tracing::debug!(
                "[protocol] write failed ({}); draining for a framed refusal",
                we
            );
        }

        // Read response loop: accumulate data until a complete frame is decodable
        loop {
            // Try to extract a complete frame from buffer
            if let Some(frame_bytes) = self
                .frame_reader
                .decode_next::<ServerMsg>()
                .map_err(|e| format!("frame decode error: {}", e))?
            {
                tracing::debug!(
                    "[protocol] received ServerMsg (type: {})",
                    match frame_bytes {
                        ServerMsg::Connected { .. } => "Connected",
                        ServerMsg::InputSent { .. } => "InputSent",
                        ServerMsg::Error(_) => "Error",
                        ServerMsg::SessionList(_) => "SessionList",
                        ServerMsg::History(_) => "History",
                        ServerMsg::ScreenUpdate(_) => "ScreenUpdate",
                        ServerMsg::SessionEnded => "SessionEnded",
                        ServerMsg::SessionKilled { .. } => "SessionKilled",
                        ServerMsg::Passthrough(_) => "Passthrough",
                        ServerMsg::Hello { .. } => "Hello",
                        // WP-B2b-2a: appended republish variants — debug-label
                        // passthrough (this helper does not exercise them).
                        ServerMsg::RepublishReady { .. } => "RepublishReady",
                        ServerMsg::RepublishTurnEnd { .. } => "RepublishTurnEnd",
                        ServerMsg::RepublishStatus { .. } => "RepublishStatus",
                        ServerMsg::RepublishEnd { .. } => "RepublishEnd",
                        ServerMsg::HistoryLogical(_) => "HistoryLogical",
                        // v5 (attended-UX M1): appended delivery variants —
                        // debug-label passthrough (this helper does not exercise them).
                        ServerMsg::DeliveryQueued { .. } => "DeliveryQueued",
                        ServerMsg::DeliveryOutcome { .. } => "DeliveryOutcome",
                    }
                );
                return Ok(frame_bytes);
            }

            // Need more data: read from socket
            match self.frame_reader.fill_from(&mut self.stream).await {
                Ok(true) => {}
                Ok(false) => {
                    return Err(match &write_result {
                        Err(we) => {
                            format!("connection closed by server (write had failed: {})", we)
                        }
                        Ok(()) => "connection closed by server".to_string(),
                    })
                }
                Err(e) => return Err(format!("frame reader error: {}", e)),
            }
        }
    }

    /// Connect to a session (create or attach based on mode).
    async fn connect_session(
        &mut self,
        session_name: &str,
        mode: ConnectMode,
    ) -> Result<(), String> {
        let msg = ClientMsg::Connect {
            name: session_name.to_string(),
            history: 10000,
            cols: 80,
            rows: 24,
            mode,
        };

        match self.send_and_receive(msg).await? {
            ServerMsg::Connected { name, new_session } => {
                tracing::info!(
                    "[protocol] connected to session '{}': new={}",
                    name,
                    new_session
                );
                Ok(())
            }
            ServerMsg::Error(e) => Err(format!("server error on Connect: {}", e)),
            other => Err(format!("unexpected response to Connect: {:?}", other)),
        }
    }

    /// Send input to the session (one-shot, no attach required).
    /// Used by G1/G4 to send data without establishing a full client connection.
    async fn send_input(&mut self, session_name: &str, data: Vec<u8>) -> Result<usize, String> {
        let msg = ClientMsg::SendInput {
            name: session_name.to_string(),
            data,
        };

        match self.send_and_receive(msg).await? {
            ServerMsg::InputSent { name, bytes } => {
                tracing::info!("[protocol] sent {} bytes to session '{}'", bytes, name);
                Ok(bytes)
            }
            ServerMsg::Error(e) => Err(format!("server error on SendInput: {}", e)),
            other => Err(format!("unexpected response to SendInput: {:?}", other)),
        }
    }

    /// Attach to a session and capture the replay the daemon sends on attach:
    /// `Connected` → `History`* → `ScreenUpdate`, then keep draining live
    /// `ScreenUpdate`/`History` frames for `drain` so output produced just after
    /// attach is captured too. This is the REAL reattach-replay surface — exactly
    /// what a human reattaching would see (G6 semantics).
    ///
    /// War story: the M3 placeholder this replaces returned an empty vec, which
    /// made every content assertion in the gate vacuous (caught in the
    /// 2026-06-04 audit; see exec/log/2026-06-04-b2.md). Do NOT stub this.
    async fn attach_and_capture(
        &mut self,
        session_name: &str,
        drain: Duration,
    ) -> Result<Captured, String> {
        self.connect_session(session_name, ConnectMode::CreateOrAttach)
            .await?;

        let mut cap = Captured::default();

        // Phase 1: read until the initial ScreenUpdate (replay tail marker).
        // Bounded so a silent daemon fails the test instead of hanging it.
        let replay_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let msg = tokio::time::timeout_at(replay_deadline, self.read_one())
                .await
                .map_err(|_| "timed out waiting for attach replay (History/ScreenUpdate)")??;
            match msg {
                ServerMsg::History(lines) => cap.history.extend(lines),
                ServerMsg::ScreenUpdate(bytes) => {
                    cap.screen = bytes;
                    break;
                }
                ServerMsg::Passthrough(_) => {}
                ServerMsg::Error(e) => return Err(format!("server error during replay: {}", e)),
                other => {
                    return Err(format!("unexpected frame during replay: {:?}", other));
                }
            }
        }

        // Phase 2: drain live updates for `drain` (output racing the attach).
        let drain_deadline = tokio::time::Instant::now() + drain;
        loop {
            match tokio::time::timeout_at(drain_deadline, self.read_one()).await {
                Err(_) => break, // drain window over
                Ok(Ok(ServerMsg::ScreenUpdate(bytes))) => cap.live.extend_from_slice(&bytes),
                Ok(Ok(ServerMsg::History(lines))) => cap.history.extend(lines),
                Ok(Ok(ServerMsg::Passthrough(_))) => {}
                Ok(Ok(ServerMsg::SessionEnded)) => break,
                Ok(Ok(other)) => return Err(format!("unexpected frame during drain: {:?}", other)),
                Ok(Err(e)) => return Err(e),
            }
        }

        Ok(cap)
    }

    /// Attach to a session and record the replay as a NON-COLLAPSING, ordered
    /// list of frames — one `RecordedFrame` per `ServerMsg` received, in exact
    /// wire order, up to and INCLUDING the first `ScreenUpdate` that settles the
    /// replay.  Frame boundaries are preserved (History frames are NOT merged),
    /// so frame-level structure assertions (R5(a)/(b)) and wire-order backlog
    /// checks (R0 via `Recorded::history_lines`) are possible.
    ///
    /// Contrast with `attach_and_capture`, which COLLAPSES all History frames
    /// into one flat `Vec` (correct for content assertions, lossy for structure).
    /// Both paths coexist; existing callers depend on the collapsing one.
    #[allow(dead_code)] // used only by the b3_replay test binary (shared #[path] lib)
    async fn record_attach_frames(&mut self, session_name: &str) -> Result<Recorded, String> {
        // Do NOT call connect_session here: it consumes the Connected frame.
        // The recorder must SEE Connected as the first ordered frame (R5(a)).
        let connect = ClientMsg::Connect {
            name: session_name.to_string(),
            history: 10000,
            cols: 80,
            rows: 24,
            mode: ConnectMode::CreateOrAttach,
        };
        self.send_msg(&connect).await?;

        let mut frames: Vec<RecordedFrame> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let msg = tokio::time::timeout_at(deadline, self.read_one())
                .await
                .map_err(|_| "timed out waiting for attach replay (record_attach_frames)")??;
            match msg {
                ServerMsg::Connected { name, new_session } => {
                    frames.push(RecordedFrame::Connected { name, new_session });
                }
                ServerMsg::History(lines) => frames.push(RecordedFrame::History(lines)),
                ServerMsg::ScreenUpdate(bytes) => {
                    frames.push(RecordedFrame::ScreenUpdate(bytes));
                    break; // replay settles at the first ScreenUpdate
                }
                ServerMsg::Passthrough(bytes) => {
                    frames.push(RecordedFrame::Passthrough(bytes));
                }
                ServerMsg::Error(e) => return Err(format!("server error during record: {}", e)),
                other => return Err(format!("unexpected frame during record: {:?}", other)),
            }
        }
        Ok(Recorded { frames })
    }

    /// Fire-and-forget: encode and send a ClientMsg without awaiting a response
    /// (used on attached connections where the response stream is continuous).
    async fn send_msg(&mut self, msg: &ClientMsg) -> Result<(), String> {
        let encoded = encode(msg).map_err(|e| format!("failed to encode ClientMsg: {}", e))?;
        self.stream
            .write_all(&encoded)
            .await
            .map_err(|e| format!("{}", e))?;
        self.stream.flush().await.map_err(|e| format!("{}", e))
    }

    /// Read exactly one ServerMsg frame (buffered).
    async fn read_one(&mut self) -> Result<ServerMsg, String> {
        loop {
            if let Some(msg) = self
                .frame_reader
                .decode_next::<ServerMsg>()
                .map_err(|e| format!("frame decode error: {}", e))?
            {
                return Ok(msg);
            }
            if !self
                .frame_reader
                .fill_from(&mut self.stream)
                .await
                .map_err(|e| format!("frame reader error: {}", e))?
            {
                return Err("connection closed by server".to_string());
            }
        }
    }
}

/// Everything a fresh attach captures from the daemon.
#[derive(Default, Debug)]
pub struct Captured {
    /// Scrollback history lines replayed on attach (raw cell text per line).
    pub history: Vec<Vec<u8>>,
    /// Initial screen render at attach (ANSI bytes).
    pub screen: Vec<u8>,
    /// Live ScreenUpdate bytes drained after attach (ANSI bytes).
    pub live: Vec<u8>,
}

impl Captured {
    /// All captured content as text, ANSI-stripped, history first.
    /// Use this for app-output-keyed sentinel/line assertions (ADD-6).
    pub fn text(&self) -> String {
        let mut out = String::new();
        for line in &self.history {
            out.push_str(&strip_ansi(&String::from_utf8_lossy(line)));
            out.push('\n');
        }
        out.push_str(&strip_ansi(&String::from_utf8_lossy(&self.screen)));
        out.push_str(&strip_ansi(&String::from_utf8_lossy(&self.live)));
        out
    }

    /// Raw (unstripped) bytes of screen + live, for escape-sequence assertions
    /// (e.g. no-altscreen-leak must see raw ?1049h if it leaks).
    pub fn raw_render(&self) -> Vec<u8> {
        let mut out = self.screen.clone();
        out.extend_from_slice(&self.live);
        out
    }
}

/// One server frame, recorded in wire order with its boundary intact.
/// Produced by the non-collapsing recorder (`record_attach`). Unlike
/// `Captured`, History frames are kept SEPARATE (one variant per frame), so
/// tests can assert frame-level structure (order, no-interleave, whole-line
/// framing) — R5's surface.
#[derive(Debug, Clone)]
#[allow(dead_code)] // consumed only by the b3_replay test binary (shared #[path] lib)
pub enum RecordedFrame {
    /// `ServerMsg::Connected` — must be the first frame on a fresh attach.
    Connected { name: String, new_session: bool },
    /// One `ServerMsg::History` frame, decoded to its line vector (boundary kept).
    History(Vec<Vec<u8>>),
    /// The settling `ServerMsg::ScreenUpdate` (raw ANSI repaint bytes).
    ScreenUpdate(Vec<u8>),
    /// An OSC `Passthrough` frame seen during replay (rare; recorded for honesty).
    Passthrough(Vec<u8>),
}

/// The ordered, non-collapsed frame list of a fresh attach's replay, captured
/// up to and including the first settling `ScreenUpdate`.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // consumed only by the b3_replay test binary (shared #[path] lib)
pub struct Recorded {
    /// Every frame received, in exact wire order. The last is the ScreenUpdate.
    pub frames: Vec<RecordedFrame>,
}

#[allow(dead_code)] // consumed only by the b3_replay test binary (shared #[path] lib)
impl Recorded {
    /// Count of `History` frames in the replay (R1(a)/(c) cardinality checks).
    pub fn history_frame_count(&self) -> usize {
        self.frames
            .iter()
            .filter(|f| matches!(f, RecordedFrame::History(_)))
            .count()
    }

    /// All History lines concatenated in wire order across frame boundaries —
    /// the exact input contract for `assert_backlog_ordered` (R0).
    pub fn history_lines(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for f in &self.frames {
            if let RecordedFrame::History(lines) = f {
                out.extend(lines.iter().cloned());
            }
        }
        out
    }

    /// The settling ScreenUpdate payload (the last frame), or None if absent.
    pub fn screen_update(&self) -> Option<&[u8]> {
        self.frames.iter().rev().find_map(|f| match f {
            RecordedFrame::ScreenUpdate(b) => Some(b.as_slice()),
            _ => None,
        })
    }
}

/// Sync wrapper: attach to `session_name` and record the replay as an ordered,
/// non-collapsing frame list (see `record_attach_frames`). Each call is a fresh
/// attach (evicts any prior attached test client — it IS the reattach path).
#[allow(dead_code)] // used only by the b3_replay test binary (shared #[path] lib)
pub fn record_attach(socket_path: &Path, session_name: &str) -> Result<Recorded, Box<dyn Error>> {
    let result = std::thread::spawn({
        let socket_path = socket_path.to_path_buf();
        let session_name = session_name.to_string();
        move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut client = ProtocolClient::connect(&socket_path).await?;
                client.record_attach_frames(&session_name).await
            })
        }
    })
    .join()
    .map_err(|_| "record thread panicked".to_string())?;

    result.map_err(|e| e.into())
}

/// Strip ANSI/VT escape sequences (CSI, OSC, ESC-singles) so sentinel substring
/// matching is robust against SGR attributes interleaved in screen renders.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // CSI: consume until final byte 0x40..=0x7E
                for c2 in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c2) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // OSC: consume until BEL or ST (ESC \)
                let mut prev_esc = false;
                for c2 in chars.by_ref() {
                    if c2 == '\u{7}' || (prev_esc && c2 == '\\') {
                        break;
                    }
                    prev_esc = c2 == '\u{1b}';
                }
            }
            _ => {
                // Single-char escape (e.g. ESC >, ESC =): consume one
                chars.next();
            }
        }
    }
    out
}

/// Sync wrapper: attach to a session and capture its replay + `drain_ms` of live
/// output. Each call is a fresh attach (evicts any prior attached test client —
/// acceptable for one-shot assertions; it IS the reattach path under test).
pub fn capture_session(
    socket_path: &Path,
    session_name: &str,
    drain_ms: u64,
) -> Result<Captured, Box<dyn Error>> {
    let result = std::thread::spawn({
        let socket_path = socket_path.to_path_buf();
        let session_name = session_name.to_string();
        move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut client = ProtocolClient::connect(&socket_path).await?;
                client
                    .attach_and_capture(&session_name, Duration::from_millis(drain_ms))
                    .await
            })
        }
    })
    .join()
    .map_err(|_| "capture thread panicked".to_string())?;

    result.map_err(|e| e.into())
}

/// Path to the qrmux binary under test. `CARGO_BIN_EXE_qrmux` is set by cargo
/// for integration tests of the crate that defines the binary — no hardcoded
/// paths (audit hygiene: the previous resolver hardcoded an absolute path).
pub fn qrmux_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_qrmux"))
}

/// RAII guard for a jailed daemon: kills the daemon process on drop so a
/// panicking test can't leak a live daemon past its jail (0b lesson — jailed
/// daemons leak; sweep by socket dir AND hold a kill guard).
pub struct DaemonGuard {
    pub pid: u32,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        // SIGTERM then SIGKILL via /bin/kill (no libc dev-dep needed).
        let _ = Command::new("/bin/kill")
            .arg("-TERM")
            .arg(self.pid.to_string())
            .status();
        std::thread::sleep(Duration::from_millis(100));
        let _ = Command::new("/bin/kill")
            .arg("-9")
            .arg(self.pid.to_string())
            .status();
    }
}

/// Kill orphaned daemons (our exact binary, reparented to init).
///
/// Identity matching is per-OS and FAIL-CLOSED (ADD-8 red-team F1):
/// - Linux: `ps comm` is the truncated 15-char basename and can never equal
///   an absolute path, which made the original sweep a silent no-op there.
///   Use the kernel's ground truth instead: `readlink /proc/<pid>/exe` must
///   equal our exact binary path. Any read failure/ambiguity → no kill.
/// - macOS: exact full-path `ps comm` match; ps truncation fails the match
///   CLOSED (conservative: may miss, never mis-kills).
///
/// Concurrent-run safety is structural either way: live runs' daemons are
/// parented to their test process, never ppid 1. Note the production daemon
/// DOES setsid (ppid 1 by design) — only the exact debug-binary-path match
/// keeps this reaper away from it, which is why ambiguity must never kill.
pub fn sweep_orphan_daemons() {
    let our_bin = qrmux_binary();
    let Ok(canon_ours) = our_bin.canonicalize() else {
        return; // can't establish our own identity → fail closed, kill nothing
    };
    let Ok(out) = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,comm="])
        .output()
    else {
        return;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (parts.next(), parts.next()) else {
            continue;
        };
        if ppid != "1" {
            continue;
        }
        let comm: String = parts.collect::<Vec<_>>().join(" ");
        let is_ours = if cfg!(target_os = "linux") {
            // Kernel ground truth; unreadable/raced → fail closed.
            std::fs::read_link(format!("/proc/{}/exe", pid))
                .ok()
                .and_then(|exe| exe.canonicalize().ok())
                .map(|exe| exe == canon_ours)
                .unwrap_or(false)
        } else {
            // Exact full-path comm match; truncation fails closed.
            std::path::Path::new(&comm)
                .canonicalize()
                .map(|c| c == canon_ours)
                .unwrap_or(false)
        };
        if is_ours {
            tracing::warn!("[daemon] reaping orphan daemon pid {} (ppid 1)", pid);
            let _ = Command::new("/bin/kill").args(["-9", pid]).status();
        }
    }
}

/// Is a process alive? (kill -0 semantics via /bin/kill.)
pub fn pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// List sessions on a daemon via the protocol. Returns SessionInfo snapshots
/// (name, child pid, dims) — the child pid feeds detach-by-construction's
/// "child survives" assertion.
pub fn list_sessions(
    socket_path: &Path,
) -> Result<Vec<qrmux::protocol::SessionInfo>, Box<dyn Error>> {
    let result = std::thread::spawn({
        let socket_path = socket_path.to_path_buf();
        move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut client = ProtocolClient::connect(&socket_path).await?;
                match client.send_and_receive(ClientMsg::ListSessions).await? {
                    ServerMsg::SessionList(list) => Ok(list),
                    other => Err(format!("unexpected response to ListSessions: {:?}", other)),
                }
            })
        }
    })
    .join()
    .map_err(|_| "list thread panicked".to_string())?;
    result.map_err(|e| e.into())
}

/// Create (or attach-create) a session and return once the daemon confirms it.
/// The attach used for creation is dropped immediately (detach-by-construction:
/// the session lives on with zero clients).
pub fn create_session(socket_path: &Path, session_name: &str) -> Result<(), Box<dyn Error>> {
    // capture with a zero drain does exactly this: Connect → replay → close.
    capture_session(socket_path, session_name, 0).map(|_| ())
}

// ============================================================================
// AttachedClient — persistent attached protocol client on a worker thread
// ============================================================================

enum AcCmd {
    Resize(u16, u16),
    Close,
}

/// A persistent ATTACHED client (holds the bridged connection open), driven
/// from sync test code. Needed by G3: `ClientMsg::Resize` is only valid on an
/// attached connection, and the storm must interleave resizes with live
/// streaming — a one-shot helper can't do that.
pub struct AttachedClient {
    cmd_tx: std::sync::mpsc::Sender<AcCmd>,
    text: std::sync::Arc<std::sync::Mutex<String>>,
    err: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl AttachedClient {
    /// Attach to `session_name` and start accumulating replay + live frames.
    /// Blocks until the attach replay (through the initial ScreenUpdate) lands.
    pub fn attach(socket_path: &Path, session_name: &str) -> Result<Self, Box<dyn Error>> {
        use std::sync::{mpsc, Arc, Mutex};
        let (cmd_tx, cmd_rx) = mpsc::channel::<AcCmd>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let text = Arc::new(Mutex::new(String::new()));
        let err = Arc::new(Mutex::new(None));

        let handle = std::thread::spawn({
            let socket_path = socket_path.to_path_buf();
            let session_name = session_name.to_string();
            let text = text.clone();
            let err = err.clone();
            move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let mut client = match ProtocolClient::connect(&socket_path).await {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };
                    match client
                        .attach_and_capture(&session_name, Duration::from_millis(50))
                        .await
                    {
                        Ok(cap) => {
                            text.lock().unwrap().push_str(&cap.text());
                            let _ = ready_tx.send(Ok(()));
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    }
                    // Event loop: drain commands, read live frames.
                    loop {
                        match cmd_rx.try_recv() {
                            Ok(AcCmd::Resize(cols, rows)) => {
                                if let Err(e) =
                                    client.send_msg(&ClientMsg::Resize { cols, rows }).await
                                {
                                    *err.lock().unwrap() = Some(e);
                                    return;
                                }
                            }
                            Ok(AcCmd::Close) => {
                                let _ = client.send_msg(&ClientMsg::Detach).await;
                                return;
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => {}
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                        }
                        match tokio::time::timeout(Duration::from_millis(20), client.read_one())
                            .await
                        {
                            Err(_) => {} // no frame this tick; loop to poll commands
                            Ok(Ok(ServerMsg::ScreenUpdate(bytes))) => {
                                text.lock()
                                    .unwrap()
                                    .push_str(&strip_ansi(&String::from_utf8_lossy(&bytes)));
                            }
                            Ok(Ok(ServerMsg::History(lines))) => {
                                let mut t = text.lock().unwrap();
                                for line in lines {
                                    t.push_str(&strip_ansi(&String::from_utf8_lossy(&line)));
                                    t.push('\n');
                                }
                            }
                            Ok(Ok(ServerMsg::Passthrough(_))) => {}
                            Ok(Ok(ServerMsg::SessionEnded)) => return,
                            Ok(Ok(other)) => {
                                *err.lock().unwrap() =
                                    Some(format!("unexpected live frame: {:?}", other));
                                return;
                            }
                            Ok(Err(e)) => {
                                // Connection closed (e.g. evicted) — record and stop.
                                *err.lock().unwrap() = Some(e);
                                return;
                            }
                        }
                    }
                });
            }
        });

        match ready_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "attach thread did not become ready within 10s".to_string())?
        {
            Ok(()) => Ok(Self {
                cmd_tx,
                text,
                err,
                handle: Some(handle),
            }),
            Err(e) => Err(e.into()),
        }
    }

    /// Send a resize through the attached connection (the real SIGWINCH path:
    /// daemon does TIOCSWINSZ on the PTY → child gets SIGWINCH).
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), Box<dyn Error>> {
        self.cmd_tx.send(AcCmd::Resize(cols, rows)).map_err(|e| {
            // Channel closed = worker exited; surface its recorded cause.
            match self.err.lock().unwrap().clone() {
                Some(cause) => format!("attached worker exited: {}", cause).into(),
                None => e.to_string().into(),
            }
        })
    }

    /// Snapshot of all text captured so far (replay + live, ANSI-stripped).
    pub fn captured_text(&self) -> String {
        self.text.lock().unwrap().clone()
    }

    /// Any background error (eviction, decode failure).
    pub fn error(&self) -> Option<String> {
        self.err.lock().unwrap().clone()
    }

    /// Detach cleanly and join the worker.
    pub fn close(mut self) {
        let _ = self.cmd_tx.send(AcCmd::Close);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for AttachedClient {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(AcCmd::Close);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Start a daemon in a jailed environment.
/// Waits for socket creation (max 5s timeout).
/// Returns (DaemonGuard, socket_path) — the guard kills the daemon on drop
/// so panicking tests can't leak daemons past their jail.
///
/// A deterministic PATH and TERM are appended if the jail env lacks them:
/// the daemon is spawned env-cleared, and its session shells need /usr/bin:/bin
/// for the gate scenarios' tools (less, tee, stty, seq).
pub fn start_daemon_in_jail(
    jail: &super::jail::Jail,
    env_vars: &[(String, String)],
    session: &str,
) -> Result<(DaemonGuard, PathBuf), Box<dyn Error>> {
    start_daemon_in_jail_with_stderr(jail, env_vars, session, None)
}

/// `start_daemon_in_jail` with the daemon's stderr captured to a file
/// (ACK-1 rows R-ARM / R-PRIV: the loud-arm warn and the payload-absence
/// assert both key on captured daemon stderr). `None` = discard (the
/// historical behavior).
///
/// WS-C M3b: spawns a PER-SESSION daemon (`qrmux server --socket-dir <dir>
/// --session <session>`) binding `<dir>/<session>.sock` and returns THAT socket.
/// The legacy shared-daemon mode (no `--session`, bound `qrmux.sock`) is RETIRED
/// (spec §1, §9). A generous default `QRMUX_CLAIM_TIMEOUT_MS` keeps the
/// freshly-spawned (still EMPTY) daemon alive until the test's first
/// session-addressed verb claims it; a caller-supplied `QRMUX_CLAIM_TIMEOUT_MS`
/// (the claim-reap arms) takes precedence (only the default is conditional).
pub fn start_daemon_in_jail_with_stderr(
    jail: &super::jail::Jail,
    env_vars: &[(String, String)],
    session: &str,
    stderr_file: Option<&Path>,
) -> Result<(DaemonGuard, PathBuf), Box<dyn Error>> {
    let mut env_vars: Vec<(String, String)> = env_vars.to_vec();
    if !env_vars.iter().any(|(k, _)| k == "PATH") {
        env_vars.push(("PATH".to_string(), "/usr/bin:/bin".to_string()));
    }
    if !env_vars.iter().any(|(k, _)| k == "TERM") {
        env_vars.push(("TERM".to_string(), "xterm-256color".to_string()));
    }
    if !env_vars.iter().any(|(k, _)| k == "QRMUX_CLAIM_TIMEOUT_MS") {
        env_vars.push(("QRMUX_CLAIM_TIMEOUT_MS".to_string(), "60000".to_string()));
    }
    let env_vars = &env_vars[..];

    // Orphan pre-sweep (orc item-3b): a SIGKILL'd test binary can leave a
    // daemon no guard or teardown ever reaps (DaemonGuard/Drop don't run on
    // SIGKILL). Reap any daemon running OUR binary whose parent is init
    // (ppid==1) — live runs' daemons are parented to their test process, so
    // this can never hit a concurrent run.
    sweep_orphan_daemons();

    let socket_path = jail.socket_dir.join(format!("{session}.sock"));

    // Remove stale socket if it exists
    let _ = fs::remove_file(&socket_path);

    tracing::info!(
        "[daemon] starting per-session qrmux server in jail, socket will be at: {}",
        socket_path.display()
    );

    let qrmux_bin = qrmux_binary();

    tracing::debug!("[daemon] using qrmux binary at: {}", qrmux_bin.display());

    // Spawn daemon as a child process (per-session `qrmux server --session`).
    let stderr: Stdio = match stderr_file {
        Some(p) => Stdio::from(std::fs::File::create(p)?),
        None => Stdio::null(),
    };
    let daemon = Command::new(&qrmux_bin)
        .arg("server")
        .arg("--socket-dir")
        .arg(&jail.socket_dir)
        .arg("--session")
        .arg(session)
        .env_clear()
        .envs(env_vars.iter().cloned())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()?;

    let daemon_pid = daemon.id();
    tracing::info!("[daemon] spawned with PID {}", daemon_pid);

    // Teardown-leak belt: record this daemon (pid + its --socket-dir identity) so a
    // future run can identity-reap it if this run dies before DaemonGuard/teardown.
    super::daemon_reaper::record_daemon_pid(
        &jail.jail_root,
        daemon_pid,
        &jail.socket_dir.to_string_lossy(),
    );

    // Detach: send the daemon to background (setsid happens inside qrmux server)
    // We hold the Child handle but allow it to daemonize
    std::mem::forget(daemon); // Drop without waiting to let it daemonize

    // Wait for socket creation (up to 5s)
    let start = Instant::now();
    let timeout = Duration::from_secs(5);
    loop {
        if socket_path.exists() {
            tracing::info!("[daemon] socket created at {}", socket_path.display());
            return Ok((DaemonGuard { pid: daemon_pid }, socket_path));
        }

        if start.elapsed() > timeout {
            return Err(format!(
                "daemon socket not created within {}s (path: {})",
                timeout.as_secs(),
                socket_path.display()
            )
            .into());
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Send data to a named session via protocol.
/// Async implementation; see sync wrapper `send_to_session`.
async fn send_to_session_async(
    socket_path: &Path,
    _env_vars: &[(String, String)],
    session_name: &str,
    data: &str,
) -> Result<(), String> {
    tracing::debug!(
        "[send] async sending {} bytes to session '{}' on socket: {}",
        data.len(),
        session_name,
        socket_path.display()
    );

    let mut client = ProtocolClient::connect(socket_path)
        .await
        .map_err(|e| e.to_string())?;

    let bytes_sent = client
        .send_input(session_name, data.as_bytes().to_vec())
        .await
        .map_err(|e| e.to_string())?;

    tracing::debug!("[send] async transmitted {} bytes", bytes_sent);
    Ok(())
}

/// Send data to a named session via protocol (synchronous wrapper).
/// M3: Uses protocol's SendInput message, which doesn't require session attachment.
pub fn send_to_session(
    socket_path: &Path,
    env_vars: &[(String, String)],
    session_name: &str,
    data: &str,
) -> Result<(), Box<dyn Error>> {
    tracing::debug!(
        "[send] sending {} bytes to session '{}' (socket: {})",
        data.len(),
        session_name,
        socket_path.display()
    );

    // Spawn blocking thread with its own tokio runtime
    let result = std::thread::spawn({
        let socket_path = socket_path.to_path_buf();
        let env_vars = env_vars.to_vec();
        let session_name = session_name.to_string();
        let data = data.to_string();
        move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(send_to_session_async(
                &socket_path,
                &env_vars,
                &session_name,
                &data,
            ))
        }
    })
    .join()
    .map_err(|_| "send thread panicked".to_string());

    match result {
        Ok(async_result) => async_result.map_err(|e| e.to_string().into()),
        Err(e) => Err(e.into()),
    }
}

/// Send raw bytes to a named session via protocol.
/// Async implementation; see sync wrapper `send_to_session_stdin`.
async fn send_to_session_stdin_async(
    socket_path: &Path,
    _env_vars: &[(String, String)],
    session_name: &str,
    bytes: &[u8],
) -> Result<(), String> {
    tracing::debug!(
        "[send] async sending {} bytes via stdin to session '{}' on socket: {}",
        bytes.len(),
        session_name,
        socket_path.display()
    );

    let mut client = ProtocolClient::connect(socket_path)
        .await
        .map_err(|e| e.to_string())?;

    let bytes_sent = client
        .send_input(session_name, bytes.to_vec())
        .await
        .map_err(|e| e.to_string())?;

    tracing::debug!(
        "[send] async stdin transmitted {} bytes (first 100: {:?})",
        bytes_sent,
        &bytes[..bytes.len().min(100)]
    );
    Ok(())
}

/// Send raw bytes to a session via protocol (synchronous wrapper).
/// M3: Binary-safe via SendInput message.
pub fn send_to_session_stdin(
    socket_path: &Path,
    env_vars: &[(String, String)],
    session_name: &str,
    bytes: &[u8],
) -> Result<(), Box<dyn Error>> {
    tracing::debug!(
        "[send] sending {} bytes via stdin to session '{}' on socket: {}",
        bytes.len(),
        session_name,
        socket_path.display()
    );

    // Spawn blocking thread with its own tokio runtime
    let result = std::thread::spawn({
        let socket_path = socket_path.to_path_buf();
        let env_vars = env_vars.to_vec();
        let session_name = session_name.to_string();
        let bytes = bytes.to_vec();
        move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(send_to_session_stdin_async(
                &socket_path,
                &env_vars,
                &session_name,
                &bytes,
            ))
        }
    })
    .join()
    .map_err(|_| "send thread panicked".to_string());

    match result {
        Ok(async_result) => async_result.map_err(|e| e.into()),
        Err(e) => Err(e.into()),
    }
}

/// Compute a real SHA-256 digest of data (hex). Evidence files cite these;
/// they must be reproducible by `shasum -a 256` on the artifact. (The previous
/// implementation was DefaultHasher mislabeled as SHA256 — audit finding.)
pub fn sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_known_vector() {
        // SHA-256("abc") — FIPS 180-2 test vector. Proves this is real SHA256,
        // not a stand-in hasher (audit finding: the old impl was DefaultHasher).
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("a\x1b[31mred\x1b[0mb"), "aredb");
        assert_eq!(strip_ansi("\x1b]0;title\x07text"), "text");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\x1b[2J\x1b[Hcleared"), "cleared");
    }
}
