# qrmux wire protocol

Version: **4** (preamble byte `0x04`). Status: B2 deliverable; C1 (D1) bumped
to v2 for the embedded-mux library surface; WS-C bumped to v3 for the
capability-exchange (`Hello`) frame; P4DB drive-burn bumped to v4 because
removing the middle `ClientMsg::LaunchHeadless` variant shifted a positional
bincode index. Consumed via `trait Mux`.

## Why a version byte exists (read this first)

Protocol-version skew is a **named failure class and one of the reasons this
rewrite exists**: a client and daemon built from different commits can disagree
about message layout, and without a version gate the daemon misparses frames
into the wrong variants — *silent corruption, not a clean error*. The TS
lineage hit exactly this class of bug. Therefore the protocol carries a
version byte **from day 1** (spec `exec/b2-spec.md`, deliverable #3), checked
before any frame is interpreted, with mismatch producing a clean framed
refusal — never a hang, never a silent drop, never a misparse.

## Connection lifecycle

```
client                                daemon
  |-- connect(unix socket) ------------>|
  |-- preamble: "QRMX" + version (5B) ->|   (frozen shape, see below)
  |                                     |-- version OK? no → framed Error, close
  |-- frame: ClientMsg::Hello ---------->|   (v3 — MUST be the first frame)
  |<-- frame: ServerMsg::Hello ----------|   (v3 — MUST be the server's first frame)
  |-- frame: ClientMsg::Connect ------->|
  |<-- frame: ServerMsg::Connected -----|
  |-- frame: ClientMsg::ConfirmSize ---->|   (when initial-size-confirm-v1 negotiated)
  |<-- frame: History* / HistoryLogical-|   (capability-gated replay)
  |<-- frame: ServerMsg::ScreenUpdate --|   (initial render)
  |<====== bidirectional frames =======>|   (Input/Resize/Detach ↔ ScreenUpdate/…)
```

The **`Hello` exchange (v3)** runs once per connection, immediately after the
preamble and before any request frame (full wire layout + negotiation order in
"§3 The Hello capability-exchange frame"). One-shot commands (`ListSessions`,
`KillSession`, `SendInput`, and the v2 `GetHistory` / `CreateDetached`) follow
the preamble + Hello, then send one request frame, read one response frame, and
close. `GetHistory` receives one or more `HistoryLogical` frames when both peers
advertise `history-logical-v1`; with `history-logical-stream-v1`, an empty
`HistoryLogical` frame explicitly completes the response. Otherwise it receives
the unchanged `History` response.

## Preamble (FROZEN FOREVER)

The first 5 bytes on every connection, client → server:

| bytes | content |
|-------|---------|
| 0–3   | magic `QRMX` (`0x51 0x52 0x4D 0x58`) |
| 4     | protocol version (current: `0x04`) |

The preamble's **shape is frozen forever** — it must never grow, shrink, or
move, precisely so any future version can read any past version's preamble.
Everything *after* the preamble is version-gated. Implementation:
`src/protocol/handshake.rs`.

Server behavior on preamble:
- **Match** → proceed to framed protocol.
- **Version mismatch** → framed `ServerMsg::Error("protocol version mismatch:
  client vX, server vY — refusing connection")`, then close. Symmetric: an
  old daemon refuses a new client the same way.
- **Bad magic** → framed `ServerMsg::Error("not an qrmux client (bad magic …)")`,
  then close.
- **EOF before 5 bytes** → quiet close (liveness probes connect-and-drop).
  The same quiet-close rule covers EOF *after* the preamble but *before* the
  first frame (a post-preamble Hello probe that drops without sending — §3.2).

## Frame format

After the preamble, all messages are length-prefixed bincode:

| bytes | content |
|-------|---------|
| 0–3   | payload length, u32 big-endian |
| 4–…   | bincode payload (`DefaultOptions` + **fixint** encoding, 16 MiB limit) |

- Max frame size 16 MiB (`MAX_FRAME_SIZE`) — oversized length prefixes are
  rejected before allocation (OOM defense).
- Fixint encoding is load-bearing: top-level `bincode::serialize` (varint)
  is NOT compatible. All paths go through `protocol::codec`.

## §3 The Hello capability-exchange frame (introduced in v3)

The preamble byte gates *which* version is speaking; the **`Hello` exchange**
negotiates *what that version can do*. It is the first thing on the wire after
the preamble, on every connection.

### §3.2 Wire layout + negotiation order (normative)

New message variants, **appended at the tail** of their enums (bincode fixint
variant indices are positional, so appending — never inserting — preserves the
frozen `ServerMsg::Error` index 4):

```rust
// ClientMsg — appended after CreateDetached
/// v3. MUST be the first frame after the preamble on every connection.
Hello { caps: Vec<String> }

// ServerMsg — appended after InputSent
/// v3. The server's first frame in response to ClientMsg::Hello.
/// `session` = the session identity this daemon serves (its --session arg),
/// present even before the session is created (claim window).
Hello { caps: Vec<String>, session: String }
```

Negotiation order:

1. client → server: 5-byte preamble (unchanged; mismatch = framed refusal, §3.4).
2. client → server: `ClientMsg::Hello { caps }` — **MUST be the first frame**.
   Any other first frame → server replies framed
   `ServerMsg::Error("protocol error: expected Hello as first frame")` and
   closes.
3. server → client: `ServerMsg::Hello { caps, session }` — **MUST be the
   server's first frame**. The client verifies `session` equals the session it
   intended to reach (identity belt against socket-file swap/rename races);
   mismatch = client-side named error, close. *(M1: `session` is transitional —
   the daemon sends an empty string until M2 wires the `--session` arg.)*
4. Normal protocol follows, restricted to the INTERSECTION of advertised caps
   (§3.3).

**Pipelining:** the client MAY send its first request immediately after its
Hello without waiting for the server Hello, iff the request uses only
baseline-v3 surface; it MUST still read the server Hello as the first reply
frame. (Connections are per-operation and short-lived; the exchange is one
extra sub-ms round trip at most.) A pipelined reader therefore reads TWO reply
frames — the ServerHello then the response — and MUST use the multi-frame
`FrameReader`, never `read_one_message` (which discards bytes after the first
frame).

**EOF before any frame:** a post-preamble connect-and-drop (a liveness/readiness
probe) → quiet close, no error (same rule as the preamble EOF).

**Defensive bounds (pre-auth frame, OOM-defense posture consistent with
`MAX_FRAME_SIZE`):** caps list ≤ 64 entries, each ≤ 64 bytes, kebab-case
`[a-z0-9-]+`; violation = framed Error, close.

### §3.3 Capability semantics (the skew-storm killer)

- Capability names are kebab-case strings in the REGISTRY TABLE below (one row
  per cap: name, introduced-version, gated surface). The baseline surface does
  not require a capability; registered capabilities gate only their additive
  variants.
- Unknown caps are IGNORED by both sides (forward compatibility).
- **Gate-on-cap rule (normative): a side MUST NOT send an enum variant or field
  whose cap the peer did not advertise.** Appended-variant decode of an unknown
  index is a clean framed decode error under fixint — never a misparse — but
  the gate-on-cap rule means it cannot happen between conforming peers.
- A side that REQUIRES a cap the peer lacks degrades or fails NAMED
  per-operation (e.g. "session 'x' daemon lacks capability 'foo' — restart that
  session to upgrade"), never a connection-level refusal, never silence.

**Capability registry (current v4):**

| capability | introduced | gated surface |
|------------|-----------|---------------|
| `history-logical-v1` | v4, additive/no bump | `ServerMsg::HistoryLogical` for attach replay and `GetHistory` |
| `history-logical-stream-v1` | v4, additive/no bump | multi-frame `HistoryLogical` plus an empty completion frame |
| `initial-size-confirm-v1` | v4, additive/no bump | post-`Connected` `ClientMsg::ConfirmSize` before initial snapshot |

`history-logical-stream-v1` depends on mutual `history-logical-v1`, because it
changes the transport contract for `HistoryLogical`. `initial-size-confirm-v1`
is independent: mutual support alone enables confirmation, regardless of the
history representation selected for the attach.

#### `history-logical-v1`

A capable client includes `history-logical-v1` in `ClientMsg::Hello.caps`; a
capable server lists the same string in `ServerMsg::Hello.caps`. The server may
send `HistoryLogical` only when the client advertised the capability. A client
that did not advertise it receives the existing `History` variant with its
existing bytes and chunking unchanged. Unknown valid kebab-case capabilities
remain accepted and ignored.

The additive message is appended at the tail of `ServerMsg`; no existing
variant index or field changes, so this addition does **not** bump v4:

```rust
HistoryLogical(Vec<LogicalHistoryChunk>)

struct LogicalHistoryChunk {
    cells: Vec<LogicalCell>,
    end_of_line: bool,
}

struct LogicalCell {
    ch: char,
    display_width: u8,
    combining: String,
    style: Style,
    wide_early_padding: bool,
}
```

Chunks remain in physical-row order. The client concatenates cells across
chunks and emits CRLF only when `end_of_line` is true; tail trimming and style
rendering operate on the completed logical line, not independently on each
chunk. This preserves a captured logical line across a different client width.
The codec has a 16 MiB per-message cap. When both peers also advertise
`history-logical-stream-v1`, the server splits the accepted chunk sequence into
individually bounded non-empty `HistoryLogical` frames and sends an empty
`HistoryLogical(Vec::new())` as the explicit completion marker. The receiver
accumulates cells until that marker, then applies the original `end_of_line`
flags. Splitting may divide an oversized physical-row chunk at a cell boundary;
only its final fragment retains the original `end_of_line`. Thus a protocol
frame boundary never adds a CRLF, joins two logical lines, or changes cells.

#### `initial-size-confirm-v1`

An attach client advertising this capability samples its terminal once for
`Connect`, waits for `Connected`, samples again, and immediately sends the
appended `ConfirmSize { cols, rows }` client variant. A server advertising the
capability sends `Connected` but takes no logical-history or initial-screen
snapshot until it has applied `ConfirmSize`. If the dimensions disagree, it
uses the ordinary PTY/screen resize path and then emits the existing full
initial repaint at the confirmed geometry. A client or server lacking the cap
keeps the pre-existing handshake unchanged. The server waits at most five
seconds for the post-`Connected` confirmation. On timeout, EOF, a malformed
frame, or an unexpected message, it proceeds with the initial snapshot at the
already-applied `Connect` geometry; `Connect` remains the confirmed fallback
sample, so the server never renders at a width with no client sample.

Compatibility is symmetric and additive: new client → older v4 server gets an
empty server capability set and legacy `History`; old client → new server still
advertises `history-logical-v1` (a server always advertises what it supports)
but, seeing no client capability, sends legacy `History`; new/new uses
`HistoryLogical`.

### §3.4 Refusal semantics

- **Preamble version mismatch:** unchanged mechanics — framed
  `ServerMsg::Error("protocol version mismatch: client vX, server vY — refusing
  connection")`, close. Symmetric old/new.
- **Engine surfacing is PER-SESSION (v3):** "stale qrmux daemon for session
  '<name>' at <dir> (vX vs vY); kill or restart THAT session" — never
  fleet-wide advice, NEVER auto-kill (kills are per-target user commands —
  ADD-12 / A14-2).
- **Hello-violation refusals:** §3.2 steps 2/3.
- **Cap-missing:** per-operation named degradation (§3.3) — not a refusal.

### §3.5 v2 → v3 changelog

- `ClientMsg::Hello { caps }` + `ServerMsg::Hello { caps, session }` (appended
  at the tail of their enums — `Error` index 4 untouched).
- Hello-first handshake normative on every connection.
- Capability registry table (initially: no required caps).
- Versioning rule amended: breaking-only bumps; additive-by-capability (below).
- Daemon topology (M2/M3): one daemon per SESSION binding `<dir>/<name>.sock`
  (was: per socket-dir binding `qrmux.sock`). Wire verb SHAPES unchanged from v2
  otherwise.

## Message types

`ClientMsg` (client → daemon):

| variant | purpose |
|---------|---------|
| `Connect { name, history, cols, rows, mode }` | create/attach a session (`mode`: CreateOrAttach / CreateOnly / AttachOnly) |
| `Input(Vec<u8>)` | keyboard bytes (attached connections) |
| `Resize { cols, rows }` | client terminal resized → daemon TIOCSWINSZ → child SIGWINCH |
| `Detach` | clean detach |
| `ListSessions` | session list request |
| `KillSession { name }` | terminate a session |
| `SendInput { name, data }` | **one-shot** input: bytes to the named session's PTY with NO attach, no eviction (B1 `send` verb) |
| `RefreshScreen` | full redraw request (e.g. focus-in) |
| `GetHistory { name }` *(v2)* | **one-shot** content read → `HistoryLogical` when negotiated, otherwise unchanged `History` (scrollback + current visible rows); NO attach, no eviction, no screen subscription (C1 D1; see "GetHistory composition") |
| `CreateDetached { name, shell_cmd, cwd, history }` *(v2)* | create a detached session running `["bash","-lc",<shell_cmd>]` in EXPLICIT `cwd`, no attach → `Connected`-class ack (C1 D1/R27) |
| `Hello { caps }` *(v3)* | capability advertisement — **MUST be the first frame** after the preamble on every connection (see §3) |
| `ConfirmSize { cols, rows }` | post-`Connected` current-size confirmation gated by `initial-size-confirm-v1` |

`ServerMsg` (daemon → client):

| variant | purpose |
|---------|---------|
| `Connected { name, new_session }` | attach confirmed |
| `History(Vec<Vec<u8>>)` | scrollback replay on attach, chunked under the frame limit |
| `HistoryLogical(Vec<LogicalHistoryChunk>)` | cell-exact logical history gated by `history-logical-v1`; chunk order and `end_of_line` framing are preserved; an empty frame completes `history-logical-stream-v1` |
| `ScreenUpdate(Vec<u8>)` | ANSI render of the current screen |
| `SessionList(Vec<SessionInfo>)` | list response (`name`, child `pid`, `cols`, `rows`, `created` *(v2, daemon-populated Unix epoch seconds, `Option<u64>`)*) |
| `SessionEnded` | child exited |
| `SessionKilled { name }` | kill confirmed |
| `Error(String)` | framed error — including version refusals |
| `Passthrough(Vec<u8>)` | OSC passthrough (clipboard, notifications) |
| `InputSent { name, bytes }` | `SendInput` ack |
| `Hello { caps, session }` *(v3)* | response to `ClientMsg::Hello` — **MUST be the server's first reply frame**; `session` = the session identity this daemon serves (see §3) |

## Frozen surface (cross-version contract)

Beyond the preamble, exactly one thing is frozen across ALL versions:
**`ServerMsg::Error` keeps enum variant index 4 with a single String field**,
so any client can decode any server's refusal frame. Everything else may
change between versions — that's what the version byte gates.

## Versioning rule

**Amended in v3 (WS-C):** bump `PROTOCOL_VERSION` only on **BREAKING** changes —
mutating an existing variant/field layout, the frame format, or Hello
semantics. ADDITIVE evolution = append a variant/field at the tail + register a
new capability string, with **NO bump** (the gate-on-cap rule, §3.3, keeps
conforming peers from ever sending a variant the other can't decode).

**Honest scope of the storm-mitigation:** additive evolution avoids bumps
entirely — no refusal storm exists to trigger. Breaking changes still bump and
refuse cleanly, as the v4 drive-burn change demonstrates. Refusals are
per-session with the session named (§3.4), so other sessions' PTY worlds keep
running. The capability frame removes bumps for additive surfaces; it does not
abolish breaking-bump refusals.

*(Pre-v3 rule, retained as history: v1/v2 bumped on ANY layout change and had
no forward-compat negotiation — mismatch = refusal, "upgrade not degrade",
because qrmux client + daemon ship from the same repo. v3 builds the
capability-exchange frame the v2 note reserved.)*

### v1 → v2 (C1 D1)

v2 added three layout changes, hence the bump from 1:

- `ClientMsg::GetHistory { name }` — one-shot content read (see below).
- `ClientMsg::CreateDetached { name, shell_cmd, cwd, history }` — detached spawn.
- `SessionInfo::created: Option<u64>` — additive, daemon-populated spawn time.

The 5-byte preamble shape and `ServerMsg::Error`'s variant index 4 were NOT
touched (see "Preamble" and "Frozen surface"), so a stale daemon and a new
client always agree on how to read the refusal frame.

### v2 → v3 (WS-C)

v3 added the `Hello` capability-exchange frame and made the Hello-first
handshake normative — a Hello-semantics change, hence the bump from 2. The full
wire layout, negotiation order, capability registry, and refusal semantics are
in "§3 The Hello capability-exchange frame"; the changelog bullet list is §3.5.
The 5-byte preamble shape and `ServerMsg::Error`'s variant index 4 were again
NOT touched — the `Hello` variants are appended at the tail of their enums.

### v3 → v4 (P4DB drive-burn)

v4 removed `ClientMsg::LaunchHeadless` from the middle of the enum. That
shifted `SubscribeRepublish`'s positional bincode index from 12 to 11, a
breaking layout mutation, so the preamble version bumped from 3 to 4. The
5-byte preamble shape and `ServerMsg::Error` index 4 remain unchanged.

`history-logical-v1` and `ServerMsg::HistoryLogical` were subsequently added
under v4 without another bump: the variant is tail-appended and cannot be sent
unless the client advertises the capability.

#### GetHistory composition (content inspection, NOT replay)

The `History` or negotiated `HistoryLogical` returned by `GetHistory` covers,
in order:

1. **Scrollback lines** — every scrollback row rendered to ANSI, exactly as the
   attach-replay `get_history` produces (same rows, same order).
2. **Current visible-screen rows** — each visible row rendered to ANSI, top to
   bottom, appended after the scrollback.

**Trim rule:** trailing visible rows that render to an EMPTY line (a blank row —
the renderer emits an empty byte string for these) are trimmed from the end, so
the reply ends at the last visible row that has content. A styled-but-spaces row
is NOT empty and is kept. Empty lines *between* content are preserved — only the
trailing run of blank visible rows is dropped. Scrollback is never trimmed.

**Why visible rows are included (differs from attach-replay):** the primary
consumer is the engine boot answerer (`crates/qd` boot path), which ANSI-strips
the history and content-matches *dialog text*. Dialogs sit on the VISIBLE screen
and usually never scroll out, so a scrollback-only reply would be blind to
exactly what the answerer exists to find. `GetHistory` is a **content-inspection**
op, not a replay op, so it returns what is on screen *now* plus what scrolled
past.

**Altscreen:** attach-replay deliberately returns EMPTY history while an
alt-screen app is up (re-injecting main-screen scrollback into a fullscreen app's
outer terminal would corrupt the replay — see "Altscreen (Divergence #1)").
`GetHistory` intentionally DIFFERS: it keeps the same scrollback portion AND
includes the visible (alt) screen rows, because the answerer must still see a
dialog rendered by a fullscreen app. This divergence is by design and load-
bearing for the boot answerer.

## Altscreen (Divergence #1 — ADR-0004)

qrmux is a **screen-model mux**: the vte performer consumes DEC modes
1049/47/1047 server-side and re-renders. Altscreen mode bytes are **never
forwarded** to clients; reattach during a fullscreen app shows the app's
content rendered as a normal screen, and reattach after exit shows the
pre-app primary screen. (Same architecture class as zmx/ghostty_vt.)

## Vertical shrink (Divergence #3 — content-preserving resize)

On vertical shrink (not in alt screen), qrmux moves displaced top rows INTO
scrollback (after consuming blank rows below the cursor), symmetric with the
grow path's restore-from-scrollback. The B1/retach lineage DISCARDED bottom
rows on shrink; that destroyed streamed content under resize churn (G3 storm:
~2% line loss, tee-oracle-proven) and was changed deliberately.

**C1 comparator note:** when the pass (b) corpus replays resize scenarios,
scrollback contents around shrink events are a SEMANTIC-CLASS comparison
(backlog-completeness / scroll-intact), NOT byte-exact vs the TS/zmx
recording — fixture and qrmux may legitimately differ on whether displaced
rows land in scrollback. An empirical probe of zmx's own shrink behavior
(ghostty_vt reflow semantics) is open for pass (b); if zmx also preserves,
this divergence note narrows to the B1 lineage only.

## Assertion audit rule (ADD-6)

macOS's tty line discipline drops ECHO bytes under input flood,
mux-independently. **Echo-sensitive checks must key on application output,
never PTY echo.** Anything testing this protocol's input path (e.g. paste
bursts through `SendInput`) asserts on what the application received/emitted,
not on echo render. Evidence chain: B1 decision memo divergence #2 + ADD-6.

## Socket layout (dir resolution tiers)

Socket dir resolves in **two tiers** (`src/server/socket.rs`):

1. `$XDG_RUNTIME_DIR/qrmux`
2. `<qdHome>/mux` where `qdHome = $QD_HOME || $HOME/.quorum/dispatch`

Socket file: `<dir>/qrmux.sock`; lockfile `<dir>/qrmux.lock`.

**De-/tmp'd fallback (C1 D1, checkpoint rider R-B / ADD-14):** the legacy
`/tmp/qrmux-{uid}` fallback is GONE — no shipped binary writes under literal
`/tmp` anymore. Tier 2 **honors `QD_HOME`** (implementer choice, recommended by
the C1 spec and named in ADR 0008): the standalone `socket.rs` fallback mirrors
the engine's `resolve_qrmux_dir`, so a relocated engine state dir or an
QD_HOME-only jail moves the mux dir too, and engine + standalone agree fully.
`$HOME/.quorum/dispatch` is used only when QD_HOME is unset; if neither XDG_RUNTIME_DIR,
QD_HOME, nor HOME is set, resolution fails with a named error.

A **`sun_path`-length guard** runs at resolve time: if `<dir>/qrmux.sock` would
exceed the platform `sockaddr_un` capacity (104 bytes, the smaller of
macOS/Linux), resolution fails with an error naming the remedy ("set
XDG_RUNTIME_DIR … or shorten QD_HOME/HOME"), instead of an opaque `bind()`
failure.

**Override seam (C1 D1/R26):** the engine resolves the dir itself and passes it
per-call into the client ops via `socket_dir_for`/`socket_path_for`/
`lock_path_for(Some(dir))`, AND propagates it to the daemon via
`server --socket-dir <dir>` (so the override crosses the process boundary —
otherwise the daemon would re-resolve from env and silently disagree). The
no-arg `socket_dir`/`socket_path`/`lock_path` are `None`-passing wrappers, so
standalone-CLI behavior is unchanged.

Dir is created 0700; symlinked socket dirs are refused; wrong permissions are
repaired. The tier SET vs the pinned TS `resolveZmxDir` is reconciled in
ADR 0008 (carry C1c resolves there: engine tier policy == qrmux contract).
