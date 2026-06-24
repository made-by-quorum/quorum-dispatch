use serde::{Deserialize, Serialize};

/// Connection mode for Connect message.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ConnectMode {
    /// Create or attach (Open subcommand)
    CreateOrAttach,
    /// Create only, fail if exists (New subcommand)
    CreateOnly,
    /// Attach only, fail if doesn't exist (Attach subcommand)
    AttachOnly,
}

/// Message sent from a client to the server.
///
/// **Protocol-version note (v2):** variants `GetHistory` and `CreateDetached`
/// were added in protocol v2. Adding enum variants changes the bincode layout,
/// so [`crate::protocol::PROTOCOL_VERSION`] was bumped to 2 (see PROTOCOL.md
/// "Versioning rule"). The preamble shape and `ServerMsg::Error`'s variant
/// index are FROZEN across versions and were NOT touched.
///
/// **Protocol-version note (v3):** `Hello` was APPENDED at the tail (bincode
/// fixint variant indices are positional — appending is layout-safe for the
/// frozen `ServerMsg::Error` index, inserting would not be). It is the
/// capability-exchange frame the v2 versioning rule reserved (PROTOCOL.md §3).
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum ClientMsg {
    /// Keyboard input from client
    Input(Vec<u8>),
    /// Terminal resized
    Resize { cols: u16, rows: u16 },
    /// Client wants to detach
    Detach,
    /// Request session list
    ListSessions,
    /// Create or attach to session
    Connect {
        name: String,
        history: usize,
        cols: u16,
        rows: u16,
        mode: ConnectMode,
    },
    /// Kill a session
    KillSession { name: String },
    /// Request a full screen refresh (e.g. on focus-in)
    RefreshScreen,
    /// Out-of-band one-shot input: write `data` to the named session's PTY
    /// without attaching (no eviction, no screen subscription). B1 `send` verb.
    SendInput { name: String, data: Vec<u8> },
    /// **v2.** One-shot scrollback request: return the named session's history
    /// as a single [`ServerMsg::History`] with NO attach, no screen
    /// subscription, no eviction. Library-grade read path for the embedded-mux
    /// `history` verb (C1 D1).
    GetHistory { name: String },
    /// **v2.** Create a detached session running `shell_cmd` (via
    /// `["bash","-lc",<shell_cmd>]`) with an EXPLICIT working directory `cwd`,
    /// without attaching. The daemon acks with a `Connected`-class response.
    /// `cwd`/`shell_cmd` are explicit because the daemon's own cwd is arbitrary
    /// after `setsid` — inheriting it would be wrong by construction (C1 D1/R27).
    CreateDetached {
        name: String,
        shell_cmd: String,
        cwd: std::path::PathBuf,
        history: usize,
    },
    /// **v3.** Capability advertisement. MUST be the first frame the client
    /// sends after the 5-byte preamble on EVERY connection (PROTOCOL.md §3.2
    /// negotiation order). Any other first frame → framed
    /// `ServerMsg::Error("protocol error: expected Hello as first frame")` and
    /// close. `caps` is the kebab-case capability set the client advertises;
    /// v3 ships with an EMPTY required set, so a conforming client sends
    /// `caps: vec![]`. Unknown caps are ignored by the peer (forward compat).
    /// Defensive bounds (pre-auth frame): ≤64 caps, each ≤64 bytes, charset
    /// `[a-z0-9-]+`; violation → framed Error, close.
    ///
    /// APPENDED at the tail — see the enum-level v3 note.
    Hello { caps: Vec<String> },
    /// **WP-B2b.** Launch a HEADLESS `claude -p` turn for session `name` (the
    /// daemon's single-session identity), spawning the stream-json child + reader
    /// and republishing its facts. `prompt` is the single `-p PROMPT`;
    /// `resume_session_id` continues an existing claude session (`--resume`), else
    /// a fresh launch. A dedicated verb (NOT an overload of `Connect`/`ConnectMode`,
    /// which carry PTY attach semantics) so the PTY paths stay byte-unchanged.
    /// APPENDED at the tail (bincode positional indices — see the v3 note; the
    /// frozen `ServerMsg::Error` index 4 is untouched).
    ///
    /// **WP-B5-i.** `cwd` + `claude_args` are APPENDED at the TAIL of this struct
    /// variant (positional bincode fields — appending after `resume_session_id`
    /// keeps byte-stability; the frozen `ServerMsg::Error` index-4 contract is
    /// untouched). `cwd` is the per-session working directory the daemon spawns
    /// the child in (`None` = inherit the daemon's own cwd, today's behaviour);
    /// `claude_args` are extra passthrough flags appended AFTER the daemon's
    /// resolved posture flags (flags-first byte-stability). Threaded end-to-end
    /// from `sb start` so the daemon stops resolving cwd/posture entirely itself.
    LaunchHeadless {
        name: String,
        prompt: String,
        resume_session_id: Option<String>,
        cwd: Option<String>,
        claude_args: Vec<String>,
    },
    /// **WP-B2b.** Subscribe to session `name`'s headless republish stream: after
    /// the v3 Hello handshake the daemon registers this connection as a socket
    /// subscriber and relays `ServerMsg::Republish*` frames (Ready/TurnEnd/Status/
    /// End) until the turn ends or the subscriber lags. A long-lived relay (like
    /// `Connect`), session-addressed (capacity-1 identity + claim-reset). APPENDED
    /// at the tail.
    SubscribeRepublish { name: String },
}

/// Message sent from the server to a client.
///
/// `Clone` is derived (WP-B2b-2a) so the socket republish fan-out can hand each
/// subscriber its own copy of a mapped frame. This is a derive-only addition — it
/// does NOT change the bincode wire layout or any variant index (the frozen
/// `Error` index-4 contract is untouched).
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub enum ServerMsg {
    /// Full screen redraw (ANSI bytes)
    ScreenUpdate(Vec<u8>),
    /// Scrollback history on reattach
    History(Vec<Vec<u8>>),
    /// Session list response
    SessionList(Vec<SessionInfo>),
    /// Session ended (shell exited)
    SessionEnded,
    /// Error
    Error(String),
    /// Connected successfully
    Connected { name: String, new_session: bool },
    /// Session killed successfully
    SessionKilled { name: String },
    /// OSC passthrough (notifications, clipboard, etc.) — written directly to outer terminal
    Passthrough(Vec<u8>),
    /// Acknowledgement that out-of-band `SendInput` bytes were written to the PTY.
    InputSent { name: String, bytes: usize },
    /// **v3.** The server's response to [`ClientMsg::Hello`] — MUST be the
    /// server's FIRST reply frame on every connection (PROTOCOL.md §3.2). `caps`
    /// is the daemon's advertised capability set (empty in v3). `session` is the
    /// session identity this daemon serves (its `--session` arg, M2): the client
    /// MUST verify it equals the session it intended to reach (identity belt
    /// against socket-file swap/rename races). Present even before the session is
    /// created (the §4.1 claim window).
    ///
    /// APPENDED at the tail — see the `ClientMsg` enum-level v3 note; the same
    /// append-not-insert discipline applies (`Error` index 4 frozen).
    Hello { caps: Vec<String>, session: String },
    /// **WP-B2b.** Headless republish: the producer is up and has bound its
    /// identity (`session_id`) — mapped from `Republish::Ready`. APPENDED at the
    /// tail (bincode positional indices — see the v3 note; `Error` index 4 frozen).
    RepublishReady { session_id: String },
    /// **WP-B2b.** Headless republish turn-end (the `result` event) — mapped from
    /// `Republish::TurnEnd`. A MUST-KEEP frame: never coalesced/dropped on a slow
    /// subscriber (the subscriber is disconnected instead). APPENDED at the tail.
    RepublishTurnEnd {
        session_id: String,
        is_error: bool,
        stop_reason: Option<String>,
    },
    /// **WP-B2b.** Headless republish coalescible status (e.g. `idle`/`busy`) — a
    /// coalescible frame that MAY be dropped for a lagging subscriber. APPENDED at
    /// the tail.
    RepublishStatus { status: String },
    /// **WP-B2b.** Headless republish terminal — breaker/EOF/lagged. A MUST-KEEP
    /// frame: it is the relay's stop signal. APPENDED at the tail.
    RepublishEnd { outcome: String },
}

/// Snapshot of a session's metadata, used in list responses.
///
/// `created` was added in protocol v2 (additive field; layout change → version
/// bump). The daemon knows the spawn instant, so it populates `created` as Unix
/// epoch seconds; `None` records "spawn time not known" (e.g. clock error). The
/// embedded-mux adapter maps this onto `MuxSession.created`; the rest of
/// `MuxSession` is synthesized adapter-side (named in ADR 0008, divergence row
/// D-LISTRAW).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub name: String,
    pub pid: u32,
    pub cols: u16,
    pub rows: u16,
    /// Session spawn time as Unix epoch seconds, populated daemon-side. `None`
    /// when the spawn instant could not be read (clock error). v2 additive.
    pub created: Option<u64>,
}
