//! Core session model shared across the data layer.
//!
//! Mirrors the TS `Session` interface (src/session.ts:94-126). Field names keep
//! the TS camelCase on the serialized surface (render.rs owns serialization —
//! these structs deliberately do NOT derive Serialize: the ls --json key order
//! and undefined-omission semantics are branch-of-construction dependent in TS,
//! so render.rs builds JSON explicitly).

/// Session status. TS: `"idle" | "busy" | "shell" | "cold" | "killed"`
/// (src/session.ts:100). `idle`/`busy`/`shell` are live; `cold` (JSONL-only
/// history) and `killed` (tombstoned) are dead — see `resolve::is_live_status`
/// (src/session.ts:1165-1173, NEW-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Idle,
    Busy,
    Shell,
    Cold,
    Killed,
}

impl SessionStatus {
    /// The exact lowercase string TS uses on every surface.
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Idle => "idle",
            SessionStatus::Busy => "busy",
            SessionStatus::Shell => "shell",
            SessionStatus::Cold => "cold",
            SessionStatus::Killed => "killed",
        }
    }

    /// Permissive parse from a registry status string. Unknown strings map to
    /// `None` (callers decide the fallback) — never a panic (L8).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(SessionStatus::Idle),
            "busy" => Some(SessionStatus::Busy),
            "shell" => Some(SessionStatus::Shell),
            "cold" => Some(SessionStatus::Cold),
            "killed" => Some(SessionStatus::Killed),
            _ => None,
        }
    }
}

/// A preview of one conversation turn (TS `TurnPreview`, src/session.ts:88-92).
#[derive(Debug, Clone, PartialEq)]
pub struct TurnPreview {
    /// "user" | "assistant"
    pub role: &'static str,
    pub text: String,
    pub timestamp: Option<String>,
}

/// The joined session row (TS `Session`, src/session.ts:94-126).
///
/// `which_branch` records WHICH construction path produced this row (live
/// registry / cold JSONL / unmatched zmx / tombstone) because the TS object
/// literals differ per branch in key SET and key ORDER, and ls --json parity
/// must replicate that (render.rs).
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub name: Option<String>,
    pub user_named: Option<bool>,
    /// Empty string for zmx-only sessions with no JSONL match (TS uses "").
    pub session_id: String,
    /// 3-char short code, assigned post-join by `codes::assign_short_codes`.
    /// RETIRING (P0 wave-1): codes no longer drive display (`ls` shows the
    /// stable-id prefix) or resolution (the code tier is gone); the field stays
    /// for `ls --json` back-compat (external consumers parse it).
    pub code: Option<String>,
    /// Engine-minted stable 8-char id (idstore.rs), keyed by the provider
    /// session UUID — filled post-join from a read-only `idstore::fold`. `None`
    /// when the session has no provider session id (ZmxOnly rows) or no mint
    /// yet (`qd ls` lazily backfills).
    pub qd_id: Option<String>,
    pub pid: Option<i64>,
    pub status: SessionStatus,
    pub zmx_name: Option<String>,
    pub zmx_clients: Option<u32>,
    /// The zmx socket dir this session's task was actually FOUND in (Bug D /
    /// cs-owns-session-identity, src/session.ts:103-110). Populated from the
    /// cross-dir scan, NOT the caller's env. Per-session ops MUST target this
    /// dir. None for cold sessions with no live zmx task.
    pub socket_dir: Option<String>,
    pub relay_port: Option<u16>,
    pub turns: u64,
    pub tokens: u64,
    pub cwd: Option<String>,
    /// Epoch milliseconds (TS `Date`); render formats as ISO-8601 (Date.toJSON).
    pub last_active_ms: Option<i64>,
    pub version: Option<String>,
    /// Epoch milliseconds (TS `Date`).
    pub started_at_ms: Option<i64>,
    pub git_branch: Option<String>,
    pub jsonl_path: Option<String>,
    pub last_turns: Option<Vec<TurnPreview>>,
    /// The session's provider id (e.g. "claude-code"). codex P1, R1
    /// (codex-p1-spec section 3): now `String` (was `&'static str`) because the
    /// value FLOWS from the persisted registry `provider` field at the read-back
    /// boundary — absent on disk defaults to "claude-code" in the join, never on
    /// disk. Rows that derive from the claude transcript / mux-pane lanes carry
    /// the literal structurally (no provider source exists on them).
    pub provider: String,
    /// The `entrypoint` discriminant the registry row carries (WP-B5-i):
    /// `Some("headless")` for an agent-driven headless `qd start`, `None` for an
    /// interactive row. Flows from the persisted registry `entrypoint` field at
    /// the join's LiveRegistry boundary (exactly like `provider`/`cwd`/`version`);
    /// all other construction branches (cold JSONL / zmx-only / tombstone) carry
    /// `None` (no registry row to source it). NOT serialized on the `ls --json`
    /// surface (render.rs builds JSON explicitly — adding this field is parity-safe).
    /// Consumed by the `qd ls` daemon-down render gate (WP-B5-ii-a guarantee (ii),
    /// `liveness::gated_ls_status_headless`) to scope the gate to headless rows.
    pub entrypoint: Option<String>,
    /// WP-B5-iii obl-4: a FORK's lineage pointer — the PARENT instance's stable
    /// qdId. Filled post-join by `idstore::fill_lineage` from the `lineage` event
    /// the fork recorded (keyed by the fork's provider session_id), via the SAME
    /// read-only fold pass `qd ls` runs (`common::all_sessions`) — so a fork's
    /// parent is discoverable through the live resolver path. `None` for every
    /// non-fork row. NOT serialized on `ls --json` (render.rs builds JSON
    /// explicitly — parity-safe, exactly like `entrypoint`; an output lineage
    /// field is a B7-owned golden-delta, deferred). STRICTLY the parent pointer —
    /// never the fork's own qdId (that stays `qd_id`).
    pub lineage: Option<String>,
    /// codex-interactive: the row's recorded HOSTING topology, flowed from the
    /// persisted registry `hosting` field at the join's LiveRegistry/Tombstoned
    /// boundary (exactly like `provider`/`entrypoint`). `None` on every other
    /// construction branch (cold JSONL / zmx-only have no registry row to source
    /// it) AND on every row whose provider has only one topology — absent means
    /// "use the provider's structural hosting", which is why nothing but the
    /// codex lanes ever needs to read it.
    ///
    /// Consumers must go through [`crate::provider::row_hosting`] rather than
    /// matching this string: that helper owns the absent⇒provider-default rule,
    /// so a caller cannot accidentally treat an absent field as "not a daemon".
    /// NOT serialized on `ls --json` (render.rs builds JSON explicitly — the same
    /// parity-safe treatment as `entrypoint`/`lineage`).
    pub hosting: Option<String>,
    pub which_branch: SessionBranch,
}

/// Which TS construction branch a row came from — drives render key-order
/// parity (see render.rs and src/session.ts:913-933 / 960-977 / 981-995 /
/// 1022-1038).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionBranch {
    /// Live row from the PID registry (session.ts:913-933).
    LiveRegistry,
    /// Cold row from a JSONL transcript (session.ts:960-977).
    ColdJsonl,
    /// zmx task with no registry/JSONL match (session.ts:981-995).
    ZmxOnly,
    /// Tombstoned registry entry, `--all` only (session.ts:1022-1038).
    Tombstoned,
}

/// Relay sidecar health record (TS `RelayHealth`, src/session.ts:152-157).
#[derive(Debug, Clone, PartialEq)]
pub struct RelayHealth {
    pub session_id: String,
    pub port: u16,
    pub pid: i32,
    pub status: String,
}
