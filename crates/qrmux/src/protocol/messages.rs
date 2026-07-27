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
///
/// **Protocol-version note (v4):** `LaunchHeadless` (the one-off `claude -p`
/// stream-json drive verb) was REMOVED from the MIDDLE of this enum (P4DB
/// drive-burn) — it sat immediately before `SubscribeRepublish`. Removing a
/// middle variant shifts `SubscribeRepublish`'s positional bincode index
/// (12 → 11), a layout-MUTATING change, so [`crate::protocol::PROTOCOL_VERSION`]
/// was bumped to 4: a skewed peer now refuses cleanly at the version gate rather
/// than misframing `SubscribeRepublish` as the removed verb. The frozen
/// `ServerMsg::Error` index 4 and the 5-byte preamble are untouched.
///
/// **Protocol-version note (v5, attended-UX M1):** the polite-delivery surface
/// was APPENDED at the tail of both enums — `ClientMsg::PendingDelivery` and
/// `ClientMsg::DeliverNow`; `ServerMsg::DeliveryQueued` and
/// `ServerMsg::DeliveryOutcome`. Appending preserves every existing positional
/// bincode index (the frozen `ServerMsg::Error` index 4 stays index 4);
/// [`crate::protocol::PROTOCOL_VERSION`] was bumped to 5 so a skewed peer refuses
/// cleanly at the version gate rather than misframing these frames. The 5-byte
/// preamble is untouched. This is the qd↔mux delivery contract M2/M3/M4 consume.
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
    /// The baseline surface has no required capability. Current session clients
    /// advertise `history-logical-v1`; discovery/liveness probes may still send
    /// `caps: vec![]`. Unknown caps are ignored by the peer (forward compat).
    /// Defensive bounds (pre-auth frame): ≤64 caps, each ≤64 bytes, charset
    /// `[a-z0-9-]+`; violation → framed Error, close.
    ///
    /// APPENDED at the tail — see the enum-level v3 note.
    Hello { caps: Vec<String> },
    /// **WP-B2b.** Subscribe to session `name`'s republish stream: after the v3
    /// Hello handshake the daemon registers this connection as a socket subscriber
    /// and relays `ServerMsg::Republish*` frames (Ready/TurnEnd/Status/End) until
    /// the turn ends or the subscriber lags. A long-lived relay (like `Connect`),
    /// session-addressed (capacity-1 identity + claim-reset).
    ///
    /// P4DB drive-burn: the one-off `claude -p` stream-json producer that fed this
    /// stream was removed (the `LaunchHeadless` verb + the daemon-side headless
    /// session map). The verb is RETAINED because the load-bearing `qd wait` channel
    /// (`dispatch::wait_channel::ChannelSubscriber`) still sends it; with no producer
    /// the daemon answers "no session to subscribe to", and `qd wait` falls back to
    /// its documented disk-poll (channel-DOWN) path. See the surviving consumers in
    /// `dispatch::observe` (the dashboard fold) and `dispatch::wait_channel`.
    SubscribeRepublish { name: String },
    /// Additive attach-only width confirmation. A client that negotiated
    /// `initial-size-confirm-v1` sends this immediately after `Connected`,
    /// before it consumes the initial history/screen snapshot. The server does
    /// not emit that snapshot until these dimensions have been applied.
    /// APPENDED so every existing variant index remains unchanged.
    ///
    /// (Rebase note: this capability-gated variant landed on `main` WITHOUT a
    /// version bump — it is gated by the `initial-size-confirm-v1` Hello cap, not
    /// the version gate. It is kept AHEAD of the v5 delivery variants below so its
    /// already-shipped positional index is preserved; the v5 variants append after.)
    ConfirmSize { cols: u16, rows: u16 },
    /// **v5 (attended-UX M1).** qd hands a send to the mux's polite-delivery
    /// surface, replacing raw `SendInput` orchestration on the attended path.
    /// The mux write-ahead-spools it (the durable acceptance write point — the
    /// send exists durably before the sender's queued receipt is returned),
    /// arms the journal-dependent countdown, and eventually fires it over the
    /// PTY via the shared submit discipline, emitting exactly ONE terminal per
    /// `send_id` to the authoritative delivery ledger.
    ///
    /// Fields carry everything the mux terminal + restart reconciliation need
    /// WITHOUT the sender (which may have exited): `send_id` (qd-minted
    /// `"{pid}-{epoch_ms}-{n}"`), the `data` bytes, `content_sha256`/`content_len`
    /// for landing correlation, the resolved `transcript` path + pre-fire
    /// `transcript_offset` for the landing/recovery window, the ledger `session`
    /// (sessionId) + qrmux `name` (the addressed session; also the envelope
    /// `name`), and `priority` (shortens the countdown ceiling). This is the
    /// deliberate, delivery-surface-only exception to the advisory stream's
    /// no-`send_id` rule (that stream stays send_id-free and untouched).
    ///
    /// APPENDED at the tail — see the enum-level v5 note.
    PendingDelivery {
        send_id: String,
        data: Vec<u8>,
        content_sha256: String,
        content_len: u64,
        transcript: Option<String>,
        transcript_offset: Option<u64>,
        session: Option<String>,
        name: String,
        priority: bool,
    },
    /// **v5 (attended-UX M1).** The deliver-now control: fire the named session's
    /// held send immediately, WITHOUT resetting the countdown and WITHOUT
    /// entering the journal (it is a control signal to the timer task, never a
    /// human keystroke). `send_id` of `None` targets the session's
    /// earliest-accepted held send; `Some` targets that specific send. M2 binds
    /// the key; the control reaches the timer task.
    ///
    /// APPENDED at the tail — see the enum-level v5 note.
    DeliverNow {
        name: String,
        send_id: Option<String>,
    },
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
    /// Cell-exact logical history, gated by the additive
    /// `history-logical-v1` Hello capability. Each chunk is one frozen
    /// physical row; `end_of_line` is the only authority for appending CRLF.
    /// APPENDED at the tail so every existing variant index remains unchanged.
    ///
    /// (Rebase note: capability-gated `main` variant, no version bump; kept ahead
    /// of the v5 delivery variants below to preserve its shipped positional index.)
    HistoryLogical(Vec<crate::screen::LogicalHistoryChunk>),
    /// **v5 (attended-UX M1).** The queued receipt at handoff-ack for a
    /// [`ClientMsg::PendingDelivery`]. NON-TERMINAL: it confirms the send is
    /// durably spooled (write-ahead), not that it landed. A no-`--wait` sender
    /// leaves after this; the mux still resolves the send to exactly one
    /// terminal later (it WRITES the terminal to the ledger; this frame is only
    /// the ack). qd's `send_path:"busy-queued"` record keys off this. APPENDED
    /// at the tail (see the `ClientMsg` enum-level v5 note; `Error` index 4
    /// frozen).
    DeliveryQueued { send_id: String },
    /// **v5 (attended-UX M1).** The outcome notification a `--wait` sender (or a
    /// subscriber) consumes to learn the terminal. `terminal_kind` is the
    /// delivery-event kind string the mux emitted to the ledger (sourced from
    /// the shared `quorum-delivery-events` vocabulary — never a locally-minted
    /// string). This is the WATCH channel, NOT the writer: the mux still WRITES
    /// the terminal to the authoritative ledger; a `--wait` sender never writes
    /// it. APPENDED at the tail.
    DeliveryOutcome {
        send_id: String,
        terminal_kind: String,
    },
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
