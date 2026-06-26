//! Rust relay server — `sb relay:serve` (M2: localhost HTTP server + lifecycle).
//!
//! This module is the server half of the relay system, replacing the bun/TS
//! `~/work/cc-relay/server.ts` (374L). The existing Rust `CcRelay` client
//! (`relay_http.rs`, FROZEN) is the reference consumer; this server speaks the
//! mirror-image HTTP it expects.
//!
//! ## Phasing
//! - **M1:** `RelayState` + pure delivery logic + unit tests (no sockets). DONE.
//! - **M2:** `RelayServer` shared container (`Mutex<RelayState>` + Condvar
//!   plus immutable config); the `relay:serve` boot lifecycle (port find, session
//!   id, sidecar write, signal cleanup); the HTTP/1.1 listener + the health,
//!   message, replies, inbox, and 404 endpoints; the proactive TTL sweeper
//!   (P-F4); and an in-process test-spawn entry. NO MCP stdio yet. DONE.
//! - **M3 (this):** the MCP-over-stdio JSON-RPC 2.0 loop (`mcp.rs` — initialize /
//!   tools/list / tools/call seam / notifications/initialized / unknown-method
//!   error / malformed skip / EOF cleanup); the `notify_channel_seam` in `http.rs`
//!   gets its real emit (the outbound `notifications/claude/channel`, P-B4); the
//!   shared `Mutex<Stdout>` that serializes every MCP stdout write (P-G6); `run()`
//!   restructured to run the stdin loop on the MAIN thread alongside the HTTP
//!   listener + sweeper threads.
//! - **M4:** Reply delivery + push-back + loop belt + Condvar resolve + the
//!   concurrency red-team (P-G1..G6).
//! - **M5:** Hardening (log rotation, EPIPE, disk), migration, 4b differential.
//!
//! ## M4: full reply-delivering relay (tools/call delivery is LIVE)
//! `relay:serve` speaks the MCP handshake (`initialize` + `tools/list` +
//! `notifications/initialized`), emits the outbound `notifications/claude/channel`
//! on each `POST /message` (P-B4), serves the HTTP endpoints, AND now runs the REAL
//! reply delivery: `tools/call name=reply` → [`RelayServer::deliver_reply`]
//! (buffer-first P-E1 → resolve a parked waiter P-E2 / push-back to the origin
//! sidecar P-E3 / loop-prevention belt P-E4 / origin guards P-E5 / honest
//! NOT-DELIVERED P-E6). `deliver_reply` is the ONE delivery code path — invoked by
//! the MCP loop AND drivable in-process at high concurrency by the red-teamer / QA
//! via [`spawn_for_test`]'s handle. The central concurrency invariant (P-G6) holds:
//! the state lock is gathered+released BEFORE any network IO (push-back) or stdout.
//!
//! ## Verb registration
//! `relay:serve` is a HIDDEN, machine-spawned verb (spawned by Claude Code's MCP
//! config, never typed by a human). Dispatched pre-clap in main.rs (same pattern
//! as `sbmux-server`).
//!
//! ## Concurrency foundation (spec P-G3 — ONE lock, no ordering)
//! [`RelayServer`] holds the M1 `RelayState` behind a SINGLE `Mutex` paired with
//! ONE `Condvar`, plus immutable config (session id, port, pid, paths, the
//! injectable park deadline). Held as `Arc<RelayServer>` and cloned into each
//! connection thread + the sweeper thread. There is exactly one lock, so there is
//! no lock-ordering to get wrong (P-G3); the discipline is "never hold it across a
//! blocking syscall" (enforced in `http.rs` — see its module doc).

pub mod http;
pub mod mcp;
pub mod register;
pub mod state;

use std::ffi::CString;
use std::io::Stdout;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::bootstrap::real_command_exists;
use crate::effects::{Env, RealEnv};
use crate::exec::{Exec, RealExec};
use crate::paths::SbPaths;
use state::{decide_delivery, origin_from_inbox, DeliveryDecision, OriginRec, RelayState};

use crate::relay::{read_sidecars, RelayContract};
use crate::relay_http::CcRelay;

/// Default port-scan base when `RELAY_PORT_BASE` is unset (server.ts:82
/// `Number(process.env.RELAY_PORT_BASE) || 8900`). P-D1.
const DEFAULT_PORT_BASE: u16 = 8900;

/// How many ports to scan from the base before giving up (server.ts:69
/// `port < start + 100`). P-D1.
const PORT_SCAN_SPAN: u16 = 100;

/// Production long-poll park deadline = 120s (P-A3/P-F3, server.ts:312
/// `setTimeout(..., 120_000)`). The deadline is INJECTABLE (P-F3b): tests pass a
/// smaller budget through the SAME code path via [`RelayServer::reply_park_timeout`].
const PROD_REPLY_PARK_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the proactive TTL sweeper wakes to evict expired `resolved` entries
/// (P-F4). server.ts uses a per-entry 5-min `setTimeout`; we instead poll on a
/// coarse interval — 30s is far finer than the 5-min TTL, so a buffered-but-never-
/// retried reply lives at most TTL + ~30s before eviction, which bounds the map.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// B2 item 4 fix (ii): how long `deliver_reply` parks for the resolved
/// long-poll's WRITE-OUTCOME ack before treating the waiter as dead and falling
/// through to push-back. The handshake is in-process (the parked thread's
/// probe + socket write is microseconds); 250ms is generous headroom, and the
/// bound only bites when the waiter is gone/wedged.
const WAITER_ACK_TIMEOUT: Duration = Duration::from_millis(250);

/// Red-team W1: how long a SECOND same-id `deliver_reply` parks waiting for an
/// open verify window to close before proceeding anyway (never wedge). Derived
/// as [`WAITER_ACK_TIMEOUT`] (the maximum life of a verify window — its owner
/// always closes it within that bound) plus an equal scheduling margin.
const VERIFY_SERIALIZE_TIMEOUT: Duration = Duration::from_millis(500);

/// Production request-read wall-clock budget = 10s (the M2 `REQUEST_READ_TIMEOUT`
/// const in `http.rs`, now INJECTABLE — orc carry 4). Threaded into the request
/// reader via [`RelayServer::request_read_timeout`] so the QA slow-drip harness row
/// can pass a SHORT budget through the SAME code path (same pattern as the `park`
/// deadline). Production passes this 10s; tests pass e.g. a few hundred ms.
const PROD_REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// §X.3.4 (3-phase delivery) — how often the recipient-side received-observer polls
/// the recipient's transcript for a landed relay `message_id`. Relay on-received
/// latency is UNBOUNDED (§X.6): this is a low-frequency liveness poll, NOT a
/// timeout-bounded wait — the promise stays PENDING until the record lands.
const RECEIVED_OBSERVE_POLL_MS: u64 = 1000;

/// The shared relay server: the M1 `RelayState` behind ONE `Mutex` + ONE
/// `Condvar`, plus immutable per-process config. Held as `Arc<RelayServer>` and
/// cloned into every connection thread + the sweeper (spec §2, P-G3).
pub struct RelayServer {
    /// The ONLY lock over relay state (pending waiters, resolved buffer, origin
    /// FIFO, mint seq). P-G3: a single lock means no lock-ordering hazard.
    pub state: Mutex<RelayState>,
    /// Wakes parked `/replies` long-polls when a `reply` resolves (M4). Paired
    /// with `state` (the canonical `Mutex`+`Condvar` park pattern).
    pub cvar: Condvar,
    /// `sessionId` advertised on `/health` + the sidecar (P-D3). Immutable.
    pub session_id: String,
    /// The bound port (after `find_port`). Immutable.
    pub port: u16,
    /// This process's pid (sidecar key + `/health`). Immutable.
    pub pid: u32,
    /// Resolved state-dir layout (`relay_dir` for the sidecar, `inbox_dir` for
    /// `/message` persistence + `/inbox`). Immutable.
    pub paths: SbPaths,
    /// The `/replies` long-poll park deadline. Production = 120s; tests inject a
    /// smaller budget through the SAME park code (P-F3b — not a forked branch).
    pub reply_park_timeout: Duration,
    /// The `POST`/request-read wall-clock budget (the M2 `REQUEST_READ_TIMEOUT`,
    /// now injectable — orc carry 4). Production = 10s; the QA slow-drip row passes
    /// a SHORT budget through the SAME request reader (not a forked branch).
    pub request_read_timeout: Duration,
    /// The ONE writer for the MCP stdout stream (P-G6). EVERY MCP stdout write —
    /// the JSON-RPC response writer in the stdin loop AND the outbound
    /// `notifications/claude/channel` emit from a `/message` HTTP thread — goes
    /// through THIS lock, so two writers never interleave a line. It is a SEPARATE
    /// lock from `state`: stdout is NEVER written while holding the state lock
    /// (P-G6 — the central concurrency invariant on the notification path). Held as
    /// `Mutex<Stdout>` rather than a raw handle so the line-write is atomic w.r.t.
    /// other writers; a stuck/slow Claude-Code stdout consumer can stall ONLY this
    /// lock, never relay state.
    pub stdout: Mutex<Stdout>,
}

impl RelayServer {
    /// Build the shared server container (state empty, config from the boot path).
    /// `park` = the `/replies` long-poll deadline (P-F3b); `request_read_timeout` =
    /// the request-read wall-clock budget (orc carry 4) — both injected through the
    /// SAME production code paths.
    fn new(
        session_id: String,
        port: u16,
        pid: u32,
        paths: SbPaths,
        park: Duration,
        request_read_timeout: Duration,
    ) -> Arc<Self> {
        // Seed the mint seq with a pid/random-derived base (M5a item iv) so two
        // FRESH servers booting in the same ms don't both start at seq 1 and mint
        // an identical first id (which would collide in the SHARED inbox dir). ONLY
        // the seq position is seeded — the epoch position is untouched (orc ruling).
        Arc::new(RelayServer {
            state: Mutex::new(RelayState::with_seq_seed(mint_seq_seed(pid))),
            cvar: Condvar::new(),
            session_id,
            port,
            pid,
            paths,
            reply_park_timeout: park,
            request_read_timeout,
            stdout: Mutex::new(std::io::stdout()),
        })
    }

    /// Spawn the proactive sweeper thread. It wakes every `SWEEP_INTERVAL` and does
    /// TWO sweeps each cycle:
    /// 1. **resolved-reply TTL eviction** (P-F4): evict expired `resolved` entries.
    ///    The EAGER counterpart to `peek_resolved`'s lazy eviction — a buffered
    ///    reply that is never re-GET would otherwise leak forever. Holds the state
    ///    lock ONLY for the `sweep_expired` call (no IO under the lock — P-G6).
    /// 2. **stale-sidecar sweep** (M5a item iii): remove `<relay_dir>/<pid>.json`
    ///    files whose pid is provably DEAD. SIGKILL'd relays leave their sidecar
    ///    behind (the SIGTERM/SIGINT unlink can't fire on SIGKILL), polluting
    ///    discovery — the 06-08 incident measured 166 sidecars vs 82 listeners. This
    ///    is FILE IO and touches NO relay state, so it runs with the state lock
    ///    NEVER held (P-G6). Detached; lives as long as the process.
    fn spawn_sweeper(self: &Arc<Self>) {
        let server = Arc::clone(self);
        // WS-B / B1 (3): the throttled inbox-TTL pass is OPT-IN. A live relay that
        // self-bounds its inbox is the steady state, but auto-sweeping on deploy
        // would be an UNGATED destructive sweep of the ~2019-file production backlog
        // — and acp-super2's standing directive is "NO destructive sweep without a
        // reviewed dry-run". So the pass is DARK unless `QD_RELAY_INBOX_SWEEP=1`; the
        // coordinator/supervisor flips it on AFTER the dry-run is reviewed + the
        // oracle passes. Read once at spawn (the relay is a real long-running
        // process; this is a behavioral toggle, not a path — distinct from the L9a
        // home/SB_HOME seam discipline).
        let inbox_sweep_enabled = std::env::var("QD_RELAY_INBOX_SWEEP")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        std::thread::spawn(move || {
            let mut cycle: u64 = 0;
            loop {
                std::thread::sleep(SWEEP_INTERVAL);
                cycle = cycle.wrapping_add(1);
                // (1) resolved-reply TTL eviction — state lock, no IO under it.
                {
                    // Poison-resilience (cond 4): recover the guard so a panic in any
                    // other critical section can't permanently disable the sweeper
                    // (which would let `resolved` grow unbounded).
                    let mut state = server.state.lock().unwrap_or_else(|p| p.into_inner());
                    let _evicted = state.sweep_expired(Instant::now());
                    // lock dropped here BEFORE the file-IO sidecar sweep (P-G6).
                }
                // (2) stale-sidecar sweep (item iii) — pure file IO, NO state lock held.
                let _removed =
                    sweep_stale_sidecars(&server.paths.relay_dir, server.pid, pid_is_alive);

                // (3) WS-B / B1 — throttled inbox-TTL GC pass (opt-in; see above).
                // Pure file IO + READ-ONLY presence; NO state lock is held across it
                // (P-G6) — it never touches relay state. Throttled to one pass per
                // `INBOX_SWEEP_EVERY_N` cycles (10 min) — far finer than the 7-day TTL.
                if inbox_sweep_enabled && cycle % INBOX_SWEEP_EVERY_N == 0 {
                    let now_ms = now_epoch_ms();
                    let trash_dir = server.paths.home.join(".claude").join("trash");
                    let _moved = crate::inbox_gc::sweep_inbox_once(
                        &server.paths.inbox_dir,
                        &server.paths.state_dir,
                        &trash_dir,
                        now_ms,
                    );
                }
            }
        });
    }
}

/// Cycles between throttled inbox-TTL sweeper passes (30 s × 20 = 10 min). The TTL
/// bound is 7 days, so a 10-minute cadence is ample and keeps the live relay's
/// per-pass cost negligible.
const INBOX_SWEEP_EVERY_N: u64 = 20;

/// Wall-clock epoch ms for the sweeper's injected-now (the relay is a real
/// long-running process; the inbox deciders stay pure — this is the one driver that
/// supplies the real clock, mirroring `qd gc`'s `RealClock`).
fn now_epoch_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Newest-N cap on the push-back sidecar probe loop (server.ts:171
/// `if (candidates.length > 8) candidates.length = 8`). A polluted relay_dir with
/// many stale namesake sidecars cannot make a single reply probe an unbounded
/// number of `/health` round-trips; the newest 8 is far beyond any real restart
/// churn. P-E3.
const PUSHBACK_CANDIDATE_CAP: usize = 8;

/// Short per-candidate `/health` + POST budget for the push-back probe
/// (server.ts:174 `AbortSignal.timeout(1500)` for health, :181 `5000` for the
/// POST). We use the frozen `CcRelay` client's SHORT-read endpoints (`health` /
/// `send_message`), which already carry their own bounded timeouts; this ms value
/// is what we pass as the `health` budget so a dead/stale candidate fails fast.
const PUSHBACK_HEALTH_TIMEOUT_MS: u64 = 1500;

/// The OUTCOME of [`RelayServer::deliver_reply`]: the honest result string the MCP
/// tool surfaces to Claude Code, plus the `is_error` flag that becomes the tool
/// result's `isError`. `is_error == true` is the HONEST NOT-DELIVERED contract
/// (P-E6 / cond 3): the reply reached NO ONE as a live delivery — it is only
/// buffered for `--wait` retries — and the guidance string tells the model to send
/// a fresh message. `is_error == false` means a real delivery happened (a parked
/// waiter was resolved, or a push-back POST succeeded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryOutcome {
    /// The result text (becomes the single text-content of the MCP tool result).
    pub text: String,
    /// `true` → the tool result carries `isError:true` (NOT-DELIVERED, P-E6).
    pub is_error: bool,
}

impl RelayServer {
    /// Deliver a `reply` tool call. The ONE delivery code path — invoked by
    /// `mcp::tools_call_result` AND drivable in-process at high concurrency by the
    /// red-teamer / QA via [`spawn_for_test`]'s handle (`handle.server.deliver_reply`).
    ///
    /// Algorithm (the M4 plan + the 8 binding conditions):
    /// 1. **buffer-first UNCONDITIONAL (P-E1 / cond 1):** the very first state-lock
    ///    action is `buffer_reply(message_id, text, now+RESOLVED_TTL)`, BEFORE any
    ///    decision, so a `--wait` client whose long-poll HTTP response is lost can
    ///    idempotently re-GET `/replies/<id>` within the TTL window.
    /// 2. **gather decision UNDER the lock, ACT with it RELEASED (P-G6 / cond 2):**
    ///    under the lock we (a) buffer, (b) read `has_waiter`, (c) look up the
    ///    in-memory origin, and (d) if a waiter is parked, `notify_all()` (cheap,
    ///    correct under the lock). We then DROP the lock. ALL network IO (the
    ///    push-back `/health` probe + `/message` POST) and any stdout happen with
    ///    the lock RELEASED. NO network IO or stdout is EVER done while holding the
    ///    state lock — the central red-team property.
    /// 3. **origin inbox fallback OUTSIDE the lock (cond 5):** if the in-mem origin
    ///    lookup MISSES, we drop the lock and read `<inbox>/<id>.json`
    ///    ([`origin_from_inbox`] — file IO, no lock), re-deriving `is_reply` from
    ///    the persisted text. The captured `has_waiter` is unchanged by this (a
    ///    waiter that registers after we release is covered by the buffer-first
    ///    write — it will peek the buffer when it parks/wakes).
    /// 4. **act on the pure [`decide_delivery`]:**
    ///    - `ResolveWaiter` → `notify_all` already fired under the lock; then
    ///      RESOLVE-AND-VERIFY (B2 item 4): park bounded for the parked
    ///      thread's write-outcome ack — ack → DELIVERED (observed); nack /
    ///      timeout → deregister the dead waiter and RE-DECIDE with
    ///      `waiter_present=false` (falls through to push-back).
    ///    - `LoopPrevented` (cond 6 / P-E4) → `is_error` true, NO push-back.
    ///    - `NotAddressable{reason}` (P-E5) → `is_error` true.
    ///    - `PushBack{origin}` → enumerate the origin's sidecars (lock RELEASED),
    ///      verify identity via `/health`, POST `[REPLY to <id>] <text>`; first
    ///      success → DELIVERED; all fail → honest NOT-DELIVERED.
    ///    - `NoOrigin` → honest NOT-DELIVERED (P-E6).
    /// 5. **clear the inbox file (server.ts:222-223):** AFTER the inbox fallback may
    ///    have read it, unlink `<inbox>/<id>.json` (best-effort — file IO, no lock).
    pub fn deliver_reply(&self, message_id: &str, text: &str) -> DeliveryOutcome {
        // ---- LOCK SPAN #1: buffer-first + capture decision inputs + notify. ----
        // Everything here is CHEAP, in-memory, and IO-free (P-G6). The lock is
        // released at the end of this block, BEFORE any network IO / stdout.
        let (waiter_present, in_mem_origin) = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());

            // (0) SERIALIZE same-id verify windows (red-team W1). verify_pending
            // is a per-id SET and acks a per-id MAP carrying no identity of
            // WHICH text the socket write delivered — two concurrent same-id
            // deliver_reply calls would cross-wire the single ack (60/200
            // forced rounds inverted truth: the delivered replier told
            // NOT-DELIVERED → duplicate resend; the never-seen one told
            // DELIVERED → silent loss), and the unconditional buffer_reply
            // below would overwrite the open window's text mid-resolve. A
            // second reply arriving while a window is OPEN therefore PARKS on
            // the Condvar until the window closes (unmark notifies), bounded
            // by [`VERIFY_SERIALIZE_TIMEOUT`] — on expiry it proceeds anyway
            // (never wedge). It then proceeds FRESH: re-buffer, re-decide with
            // current state — sequentially the first reply resolves the waiter
            // truthfully and the second finds the waiter gone → push-back;
            // BOTH verdicts are observations. Lock discipline: the park is the
            // Condvar wait (lock atomically released, P-G3); no IO under the
            // lock (P-G6).
            if state.verify_open(message_id) {
                let bound = Instant::now() + VERIFY_SERIALIZE_TIMEOUT;
                while state.verify_open(message_id) {
                    let remaining = match bound.checked_duration_since(Instant::now()) {
                        Some(d) if !d.is_zero() => d,
                        _ => break, // bound expired — proceed anyway, never wedge
                    };
                    let (guard, _r) = self
                        .cvar
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(|p| p.into_inner());
                    state = guard;
                }
            }

            // (1) buffer-first, UNCONDITIONAL (P-E1 / cond 1) — BEFORE deciding.
            let deadline = Instant::now() + state::RESOLVED_TTL;
            state.buffer_reply(message_id.to_string(), text.to_string(), deadline);

            // (2) capture the decision inputs while we hold the lock.
            let waiter_present = state.has_waiter(message_id);
            let in_mem_origin = state.origin_for(message_id);

            // (2d) a parked /replies long-poll resolves by peeking the buffer we
            // just wrote, woken by this notify. notify_all UNDER the lock is fine
            // (cheap, correct — cond 2). We notify_all (not _one) so a spurious
            // re-park of a different waiter cannot strand this one. The buffer-first
            // write above is the belt; this wake is the suspenders (P-G1).
            //
            // B2 item 4 fix (ii): the verify marker is set UNDER THE SAME LOCK
            // ACQUISITION as the buffer write + notify, so whatever wakes the
            // parked thread, it sees the marker before it can offer its write
            // outcome — the offer is never dropped on the common live-waiter
            // path.
            if waiter_present {
                state.mark_verify(message_id);
                self.cvar.notify_all();
            }

            (waiter_present, in_mem_origin)
            // lock dropped here — NO IO has happened under it.
        };

        // ---- origin INBOX FALLBACK (cond 5): file IO, lock RELEASED. ----
        // Only read the inbox file when the in-mem map missed (server.ts:114-115:
        // in-mem first, file fallback second). We do NOT re-acquire the lock for the
        // decision: `decide_delivery` is pure and `waiter_present` was captured
        // under the lock above.
        let origin: Option<OriginRec> = match in_mem_origin {
            Some(rec) => Some(rec),
            None => origin_from_inbox(&self.paths.inbox_dir, message_id),
        };

        // ---- DECIDE (pure) then ACT (lock released — P-G6). ----
        let mut decision = decide_delivery(
            message_id,
            waiter_present,
            origin.as_ref(),
            &self.session_id,
        );

        // B2 item 4 fix (ii) — RESOLVE-AND-VERIFY. A registered waiter is no
        // longer trusted as proof of delivery (the diagnosed stale-waiter
        // body-loss: a sender that timed out / died leaves its marker parked,
        // and the resolve writes into a dead socket). Wait BOUNDED for the
        // parked thread's actual WRITE OUTCOME: ack(written) → DELIVERED,
        // truthful. Nack or timeout → deregister the dead waiter and RE-DECIDE
        // with waiter_present=false, falling through to push-back/honest
        // NOT-DELIVERED. The replier's answer is then always an observation,
        // never an optimistic inference.
        if decision == DeliveryDecision::ResolveWaiter && !self.verify_waiter_delivery(message_id) {
            decision = decide_delivery(message_id, false, origin.as_ref(), &self.session_id);
        }

        let outcome = match decision {
            // P-E2: the parked /replies was woken by notify_all above, re-peeked
            // the buffer, and ACKED a successful socket write (verified just
            // above). DELIVERED — now an observed fact, not an inference.
            DeliveryDecision::ResolveWaiter => DeliveryOutcome {
                text: format!(
                    "DELIVERED: a sender was waiting on {message_id} (long-poll resolved)."
                ),
                is_error: false,
            },
            // P-E4 / cond 6: the inbound was itself a [REPLY to ...] — refuse to
            // auto-post (ping-pong belt). is_error=true, NO push-back.
            DeliveryDecision::LoopPrevented => DeliveryOutcome {
                text: format!(
                    "NOT DELIVERED: loop prevention — the inbound message {message_id} is itself a \
                     pushed-back {prefix}...] reply; replies to replies never auto-post. {tail}",
                    prefix = state::REPLY_PREFIX,
                    tail = FRESH_MESSAGE_GUIDANCE,
                ),
                is_error: true,
            },
            // P-E5: origin unknown / cli / self — not addressable. is_error=true.
            DeliveryDecision::NotAddressable { reason } => DeliveryOutcome {
                text: format!("NOT DELIVERED: {reason}. {FRESH_MESSAGE_GUIDANCE}"),
                is_error: true,
            },
            // P-E3: real addressable origin — push the reply back to its sidecar
            // (network IO, lock RELEASED). first success → DELIVERED; all fail →
            // honest NOT-DELIVERED.
            DeliveryDecision::PushBack { origin } => match self.push_back(&origin, message_id, text)
            {
                Some(detail) => DeliveryOutcome {
                    text: format!(
                        "DELIVERED: no waiting sender; {detail}. It arrives there as a channel \
                         message marked {prefix}{message_id}] — the recipient will not reply-tool it back.",
                        prefix = state::REPLY_PREFIX,
                    ),
                    is_error: false,
                },
                None => not_delivered_outcome(
                    message_id,
                    &format!("no live sidecar found for origin session {origin}"),
                ),
            },
            // P-E6: no origin recorded at all (unknown id / predates this sidecar,
            // no inbox file). Honest NOT-DELIVERED.
            DeliveryDecision::NoOrigin => not_delivered_outcome(
                message_id,
                &format!(
                    "no origin recorded for {message_id} (unknown id, or it predates this sidecar \
                     and left no inbox file)"
                ),
            ),
        };

        // ---- clear the inbox file AFTER the fallback may have read it. ----
        // server.ts:222-223 (best-effort unlink; file IO, lock released).
        let inbox_path = self.paths.inbox_dir.join(format!("{message_id}.json"));
        let _ = std::fs::remove_file(inbox_path);

        outcome
    }

    /// B2 item 4 fix (ii): park BOUNDED for the resolved long-poll's write
    /// outcome. Returns `true` iff the parked `/replies` thread ACKED a
    /// successful response write to a live peer; `false` on an explicit nack
    /// from the RESOLVED-WRITE path (its pre-write liveness probe found the
    /// peer dead, or the write failed — the only live nack channel; the hangup
    /// tick DEREGISTERS the waiter instead, its belt offer provably dropped
    /// today — see the http.rs hangup-branch comment) or on the
    /// [`WAITER_ACK_TIMEOUT`] elapsing with no ack (a wedged or lost waiter).
    ///
    /// On the false path the waiter registration is REMOVED so the dead marker
    /// cannot shadow the caller's push-back fall-through. (If the parked thread
    /// was merely slow — the rare timeout race — it may still serve its client
    /// from the resolved buffer afterwards; a rare double delivery is accepted
    /// over the loss class this fixes — per the phase-2 ruling.)
    ///
    /// Lock discipline: the wait parks on the SAME Condvar with the lock
    /// atomically released (P-G3); no IO happens here (P-G6). The in-process
    /// handshake is microseconds in the live case; the 250ms bound only bites
    /// when the waiter is gone.
    fn verify_waiter_delivery(&self, message_id: &str) -> bool {
        let deadline = Instant::now() + WAITER_ACK_TIMEOUT;
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let verified = loop {
            if let Some(written) = state.take_ack(message_id) {
                break written;
            }
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(d) if !d.is_zero() => d,
                _ => break false, // no ack within the bound → treat as undelivered
            };
            let (guard, _r) = self
                .cvar
                .wait_timeout(state, remaining)
                .unwrap_or_else(|p| p.into_inner());
            state = guard;
        };
        // Close the verify window (drains any late ack) and, on failure,
        // deregister the dead/wedged waiter so it cannot shadow push-back.
        state.unmark_verify(message_id);
        if !verified {
            state.remove_waiter(message_id);
        }
        // Red-team W1: the window CLOSE wakes any same-id deliver_reply parked
        // in the serialization wait (deliver_reply step 0) — under the lock,
        // cheap, correct.
        self.cvar.notify_all();
        verified
    }

    /// Push a reply back to the ORIGIN session's sidecar as a NEW `/message`
    /// (server.ts:156-191). Called by [`deliver_reply`] with the state lock
    /// RELEASED (P-G6 — this is all network + file IO).
    ///
    /// Enumerate the origin's sidecars from `relay_dir` (via the frozen
    /// `read_sidecars`), filter to `sessionId == origin`, sort NEWEST-first, cap at
    /// [`PUSHBACK_CANDIDATE_CAP`]. For each candidate, verify identity via `/health`
    /// (a stale sidecar — dead pid / reused port — must never receive the reply),
    /// then POST `[REPLY to <id>] <text>` via `CcRelay::send_message` with THIS
    /// session as `from_session`. Returns `Some(detail)` on the FIRST successful
    /// POST, `None` if every candidate is dead/mismatched (→ honest NOT-DELIVERED).
    ///
    /// `read_sidecars` already drops the `startedAt` field, so we re-read it here to
    /// sort newest-first (the freshest sidecar is the live one after a restart).
    fn push_back(&self, origin: &str, message_id: &str, text: &str) -> Option<String> {
        // Enumerate candidates with their startedAt for newest-first ordering. We
        // read the sidecar files directly (read_sidecars discards startedAt); both
        // are tolerant of an unreadable/parse-broken file (skip it).
        let mut candidates: Vec<(u16, String)> = Vec::new();
        for relay in read_sidecars(&self.paths.relay_dir) {
            if relay.session_id != origin {
                continue;
            }
            let started_at = read_sidecar_started_at(&self.paths.relay_dir, relay.pid as u32);
            candidates.push((relay.port, started_at));
        }
        // NEWEST first (server.ts:168 sorts startedAt descending). ISO-8601 strings
        // compare lexicographically in time order, so a plain reverse string sort
        // is correct; a missing startedAt ("") sorts oldest.
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        candidates.truncate(PUSHBACK_CANDIDATE_CAP);

        let client = CcRelay::new();
        let pushed_text = format!("{prefix}{message_id}] {text}", prefix = state::REPLY_PREFIX);
        for (port, _started) in candidates {
            // Verify identity via /health (server.ts:174-176): a stale sidecar
            // whose port was reused by a different session must not get the reply.
            match client.health(port, PUSHBACK_HEALTH_TIMEOUT_MS) {
                Ok(h) if h.session_id == origin => {}
                // dead candidate / identity mismatch → try the next sidecar.
                _ => continue,
            }
            // POST the reply as a NEW /message on the origin's sidecar.
            if let Ok(new_id) = client.send_message(port, &pushed_text, &self.session_id) {
                return Some(format!(
                    "posted to session {origin} (port {port}) as new message {new_id}"
                ));
            }
            // POST failed on a verified-live candidate — try the next one
            // (server.ts returns on a non-2xx, but trying the next live sidecar is
            // strictly more robust and never delivers twice: the first success
            // returns immediately).
        }
        None
    }
}

/// The verbatim FRESH-MESSAGE guidance tail (P-E6 / cond 3, server.ts:218-219):
/// the text is buffered for `--wait` retries only; to ACTUALLY reach the other
/// session the model must send a fresh message and restate its substance. Appended
/// to every NOT-DELIVERED outcome so the model is never left thinking a failed
/// reply silently succeeded.
const FRESH_MESSAGE_GUIDANCE: &str = "The text is buffered 5 minutes for --wait retries only. To actually reach the other session, send a fresh message — sb send:relay <session-name-or-id> — and restate your substance.";

/// Build the HONEST NOT-DELIVERED outcome (P-E6 / cond 3): `is_error = true` +
/// the reason + the fresh-message guidance. NEVER a silent success — the reply
/// reached no one as a live delivery (it is only buffered for `--wait` retries).
fn not_delivered_outcome(message_id: &str, reason: &str) -> DeliveryOutcome {
    DeliveryOutcome {
        text: format!(
            "NOT DELIVERED: no sender is waiting on {message_id} and push-back failed ({reason}). \
             {FRESH_MESSAGE_GUIDANCE}"
        ),
        is_error: true,
    }
}

/// Read just the `startedAt` field of a sidecar `<relay_dir>/<pid>.json` for the
/// push-back newest-first sort (server.ts:164 `d.startedAt`). Returns `""` when the
/// file is absent / unreadable / carries no string `startedAt` (sorts oldest).
fn read_sidecar_started_at(relay_dir: &Path, pid: u32) -> String {
    let path = relay_dir.join(format!("{pid}.json"));
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let Ok(data) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return String::new();
    };
    data.get("startedAt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------------------
// M5a item iv — mint seq seed
// ---------------------------------------------------------------------------

/// Headroom mask for the seq seed (M5a item iv). We seed the seq into the low 32
/// bits only, leaving the high 32 bits of the `u64` seq as monotonic-increment
/// headroom — a single process would have to mint > 4 billion ids before the seq
/// could approach `u64::MAX` (impossible for a relay's lifetime), so the seeded
/// `seq += 1` mint never overflows.
const SEQ_SEED_MASK: u64 = 0xFFFF_FFFF;

/// Derive the per-process mint-seq SEED (M5a item iv). Mixes the pid with 8 random
/// bytes from `/dev/urandom` so two FRESH servers (even with adjacent pids, even
/// booting the same ms) get DIFFERENT seq bases — killing the shared-inbox
/// filename collision that two `relay-<ms>-1` ids would cause. Masked to the low 32
/// bits (`SEQ_SEED_MASK`) so the minted seq stays a modest, all-digits number with
/// vast `u64` headroom for the monotonic increment.
///
/// ★ This seeds ONLY the seq (position 3 of `relay-<ms>-<seq>`). The epoch
/// (position 2) is the real `now_ms`, untouched — fleet consumers that parse the
/// embedded mint epoch are unaffected (orc ruling iv). Falls back to a
/// pid+nanos mix if `/dev/urandom` is unreadable (same posture as `random_uuid_v4`).
fn mint_seq_seed(pid: u32) -> u64 {
    let mut rand_bytes = [0u8; 8];
    let rand: u64 = if read_urandom(&mut rand_bytes).is_ok() {
        u64::from_le_bytes(rand_bytes)
    } else {
        // Degenerate fallback: pid + nanos (never panics; urandom is the norm).
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        (pid as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ nanos
    };
    // Mix the pid in so the seed varies with the process even if two procs somehow
    // read correlated randomness; mask to the low 32 bits for headroom.
    (rand ^ (pid as u64).wrapping_mul(0x9E37_79B9)) & SEQ_SEED_MASK
}

// ---------------------------------------------------------------------------
// M5a item iii — stale-sidecar sweep (the incident fix)
// ---------------------------------------------------------------------------

/// Liveness check for a pid via `kill(pid, 0)` (the conventional "does this process
/// exist?" probe — sends no signal, just checks permission/existence). M5a item iii.
///
/// Returns:
/// - `true`  — the process EXISTS (kill returns 0), OR the kill failed with EPERM
///   (the process exists but we don't own it — alive-but-not-ours; KEEP its
///   sidecar, NEVER remove a live peer's). CONSERVATIVE: any non-ESRCH outcome is
///   treated as alive.
/// - `false` — ONLY when kill fails with ESRCH (no such process) — provably DEAD.
///
/// This is the ONLY place real `libc::kill` is called; the staleness DECISION lives
/// in the pure, testable [`is_sidecar_stale`] (injected liveness fn).
fn pid_is_alive(pid: u32) -> bool {
    // pid 0 is not a real relay pid (sidecars always carry a real pid); treat a
    // 0/garbage pid as "uncertain" → alive (conservative — never remove on doubt).
    if pid == 0 {
        return true;
    }
    // SAFETY: `kill` with signal 0 performs no action beyond the existence/permission
    // check; it has no memory-safety implications.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true; // process exists and we may signal it.
    }
    // rc == -1: inspect errno. ESRCH = no such process (DEAD). Anything else
    // (EPERM = exists-but-not-ours, or any unexpected errno) → treat as ALIVE.
    let err = std::io::Error::last_os_error();
    err.raw_os_error() != Some(libc::ESRCH)
}

/// PURE staleness decision for a sidecar (M5a item iii) — unit-testable without
/// real processes via the injected `is_alive` fn.
///
/// A sidecar is STALE (safe to remove) IFF:
/// - its pid is NOT our own pid (NEVER sweep our own sidecar), AND
/// - `is_alive(pid)` returns `false` (the pid is PROVABLY dead — ESRCH).
///
/// CRITICAL SAFETY (the whole point of item iii): a live peer's sidecar must NEVER
/// be removed (a false-positive would break discovery for a healthy session). So
/// staleness requires `is_alive` to AFFIRMATIVELY report dead; any uncertainty
/// (EPERM / unexpected errno) makes `is_alive` return `true` → NOT stale → KEEP.
pub fn is_sidecar_stale(pid: u32, own_pid: u32, is_alive: impl Fn(u32) -> bool) -> bool {
    if pid == own_pid {
        return false; // never our own
    }
    !is_alive(pid)
}

/// Sweep STALE sidecars from `relay_dir` (M5a item iii). For each `<pid>.json`,
/// parse the pid (the `pid` field, falling back to the filename stem), and if
/// [`is_sidecar_stale`] says it is provably dead (and not our own), `remove_file`
/// it. Returns the count removed.
///
/// FILE IO ONLY — no relay state lock is touched (called with the lock released:
/// from the boot path before any server exists, and from the sweeper thread AFTER
/// dropping the state guard). Best-effort: an unreadable/unparseable file or a
/// failed unlink is skipped, never fatal. `is_alive` is injected so the sweep is
/// testable with a fake liveness oracle.
fn sweep_stale_sidecars(relay_dir: &Path, own_pid: u32, is_alive: impl Fn(u32) -> bool) -> usize {
    let entries = match std::fs::read_dir(relay_dir) {
        Ok(e) => e,
        Err(_) => return 0, // missing/unreadable dir → nothing to sweep.
    };
    let mut removed = 0usize;
    for dent in entries.flatten() {
        let path = dent.path();
        // Only *.json sidecars (mirror read_sidecars).
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // Resolve the pid: prefer the record's `pid` field; fall back to the
        // filename stem (sidecars are named `<pid>.json`). A file we can't pin a
        // pid to is left ALONE (conservative — never remove on uncertainty).
        let Some(pid) = sidecar_pid(&path) else {
            continue;
        };
        if is_sidecar_stale(pid, own_pid, &is_alive) {
            // Provably dead + not our own → remove (best-effort).
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// Resolve the pid a sidecar belongs to (M5a item iii helper): the record's `pid`
/// field if numeric+non-zero, else the filename stem parsed as a pid. Returns
/// `None` when neither yields a usable pid (the sweep then leaves the file alone).
fn sidecar_pid(path: &Path) -> Option<u32> {
    // Prefer the in-file pid (authoritative — what read_sidecars/push-back use).
    if let Ok(bytes) = std::fs::read(path) {
        if let Ok(data) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(p) = data.get("pid").and_then(|v| v.as_u64()) {
                if p != 0 && p <= u32::MAX as u64 {
                    return Some(p as u32);
                }
            }
        }
    }
    // Fallback: the filename stem (sidecars are written as `<pid>.json`).
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|p| *p != 0)
}

/// Entry point for `sb relay:serve`. Boots the real HTTP server (M2).
pub fn run() -> i32 {
    run_with_env(&RealEnv)
}

/// Testable boot variant: accepts an injected `Env` (L9a / ADD-4 seam discipline
/// — never read `std::env` directly). Boots port-find → state init → sidecar
/// write → signal handlers → HTTP listener + sweeper, then BLOCKS on the accept
/// loop (the listener never returns under normal operation).
pub fn run_with_env(env: &dyn Env) -> i32 {
    let home: PathBuf = match env.var("HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => {
            eprintln!("relay:serve: HOME not set");
            return 1;
        }
    };
    let paths = SbPaths::from_home_env(&home, env);

    // SELF-HEAL ON USE (relay-path hardening): the strongest guarantee against a
    // moved/upgraded `sb` orphaning its own MCP registration. Claude Code spawns
    // THIS process via the `relay.command` it stored in `~/.claude.json` — an
    // ABSOLUTE PATH to the `sb` binary. If that binary was later moved or replaced
    // and the stored path went stale, every NEW session would fail to spawn its
    // relay. Here, at the start of EVERY relay boot, we compare the stored command
    // against the binary actually running (`current_exe`); a mismatch (or a stored
    // path that no longer exists) is re-pointed at the running binary in place.
    // So the very act of a session starting its relay corrects the stale entry —
    // even bootstrap-less, even if `sb update` was never run. Best-effort and
    // NON-FATAL: it never blocks or fails the boot (the common steady state is a
    // cheap file-read + stat with NO subprocess; we shell to `claude` only on a
    // genuine mismatch). It only CORRECTS an existing entry — it never fabricates
    // a registration the user never asked for (consent discipline).
    self_heal_registration(&home, &RealExec, |c| real_command_exists(&RealExec, c));

    // Session id (P-D3): CLAUDE_CODE_SESSION_ID or a random uuid (server.ts:35).
    let session_id = env
        .var("CLAUDE_CODE_SESSION_ID")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(random_uuid_v4);

    // Port find (P-D1): RELAY_PORT_BASE || 8900, scan +100 (server.ts:68-82).
    let port_base = env
        .var("RELAY_PORT_BASE")
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|p| *p != 0)
        .unwrap_or(DEFAULT_PORT_BASE);
    let listener = match find_port(port_base) {
        Some((listener, _port)) => listener,
        // FAIL-FAST on genuine port exhaustion (M5a item v): NO retry loop, NO
        // spin/respawn (a hang/spin is the worst failure mode on this box — it was
        // the vicious-cycle half of the 06-08 incident). One honest line, exit 1.
        // The range is the frozen client-probe range 8900-8999 (do NOT widen — a
        // parked Pete-decision); the periodic stale-sidecar sweep (item iii) is what
        // relieves the pressure that causes exhaustion, not a wider range.
        None => {
            eprintln!(
                "relay:serve: no free relay port in {port_base}-{} ({PORT_SCAN_SPAN}-port range \
                 exhausted) — too many live relays, or stale sidecars holding ports; not retrying",
                port_base.saturating_add(PORT_SCAN_SPAN - 1)
            );
            return 1;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let pid = std::process::id();

    // Stale-sidecar sweep ON BOOT (M5a item iii (a)): clean up dead relays' leftover
    // <pid>.json files BEFORE writing our own, so a fleet that accumulated SIGKILL
    // orphans (166 sidecars vs 82 listeners in the 06-08 incident) self-heals at
    // each boot. Never touches our own pid (we haven't written it yet, but pass it
    // for safety). Pure file IO — no state lock exists yet at this point anyway.
    let _removed = sweep_stale_sidecars(&paths.relay_dir, pid, pid_is_alive);

    // Sidecar (P-D2): <relay_dir>/<pid>.json = {port, pid, sessionId, startedAt}.
    if let Err(e) = write_sidecar(&paths, port, pid, &session_id) {
        eprintln!("relay:serve: failed to write sidecar: {e}");
        // Non-fatal in bun (it logs + continues, server.ts:349-350) — match that.
    }

    // Signal cleanup (P-D4): unlink the sidecar on SIGTERM/SIGINT, exit 0.
    install_signal_cleanup(&sidecar_path(&paths, pid));

    let server = RelayServer::new(
        session_id.clone(),
        port,
        pid,
        paths,
        PROD_REPLY_PARK_TIMEOUT,
        PROD_REQUEST_READ_TIMEOUT,
    );
    server.spawn_sweeper();

    eprintln!(
        "relay: session={session_id} port={port} (M3: MCP stdio + HTTP; tools/call delivery = M4)"
    );

    // Spawn the HTTP accept loop on its OWN thread (the listener serves forever),
    // then run the MCP JSON-RPC stdin loop on THIS (the main) thread. The stdin
    // loop blocks reading lines from stdin until EOF (parent / Claude Code gone —
    // normal, P-F2); the HTTP listener + the sweeper keep running until process
    // exit. When the stdin loop returns (EOF) we run the SAME cleanup as a signal
    // (unlink the sidecar) and exit 0 — CC has gone away, there is nothing left to
    // serve. (M2 blocked on the accept loop here; M3 moves HTTP to a thread so the
    // MCP loop owns the main thread + stdin.)
    let http_server = Arc::clone(&server);
    std::thread::spawn(move || http::serve(listener, http_server));

    // §X.3.4 (3-phase delivery, relay on-received) — spawn the long-lived
    // recipient-side transcript observer. It emits `message-seen` into THIS
    // recipient's own delivery log when a relay `message_id` lands in the
    // transcript (the recipient pulled it into working context). Decoupled from the
    // POST — unbounded latency (§X.6), NOT a timeout. Best-effort: a poll/IO error
    // never affects serving; the thread dies with the process at EOF. One-way
    // invariant intact (events only into dispatch's own log; nothing crosses the wire).
    {
        let state_dir = server.paths.state_dir.clone();
        let projects_dir = server.paths.projects_dir.clone();
        let sid = server.session_id.clone();
        // Scope emitted `message-seen` to ids THIS recipient genuinely received
        // (symmetric with `emit_seen_failed_for_unpulled`) — defense-in-depth
        // behind the structural wrapper-attribute matcher so a body-quoted or
        // sibling-pasted `message_id="…"` for a never-received message can never
        // fire a phantom on-received terminal (WRONG-FIRE).
        let accept_server = Arc::clone(&server);
        std::thread::spawn(move || {
            run_received_observer(&state_dir, &projects_dir, &sid, |id| {
                let st = accept_server
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                st.tracked_message_ids().iter().any(|t| t == id)
            })
        });
    }

    // The MCP loop reads from the real stdin and writes responses through the
    // shared `Mutex<Stdout>`. It returns on EOF.
    mcp::serve_stdio(&server);

    // §X.3.5 (3-phase delivery) — the recipient session-close bookend (P-F2: EOF =
    // CC gone). Emit `seen-failed{recipient-gone}` for every message THIS session
    // received that NEVER landed in the transcript — a genuine recipient-gone,
    // never latency. The final transcript scan is the race guard: the session is
    // gone, so no `message-seen` can follow → a `seen-failed` and a `message-seen`
    // for one send_id can never both land (§X.3.5). Best-effort; before cleanup.
    emit_seen_failed_for_unpulled(&server);

    // EOF on stdin = parent/CC gone (P-F2). Run the same cleanup the signal handler
    // does (unlink the sidecar) and exit 0. The signal handler installed above does
    // the async-signal-safe version; here we are in normal context, so a plain
    // `remove_file` is fine (and idempotent if the signal already fired).
    let _ = std::fs::remove_file(sidecar_path(&server.paths, server.pid));
    0
}

/// §X.3.4 (3-phase delivery, relay on-received) — the recipient-side transcript
/// observer body. A long-lived poll loop over THIS recipient's own Claude
/// transcript; for each `type:"user"` record carrying a relay `message_id="<id>"`,
/// emits ONE `message-seen` into the recipient's delivery log
/// (`<state>/sessions/<uuid>.events.jsonl`).
///
/// - Matcher = `message_id` SUBSTRING (the landed record is channel-wrapped, NOT
///   the pty byte-exact match, §X.3.4). `content_sha256` = sha256(extracted inner
///   body) — ADVISORY recipient-side (bond resolves by `send_id` only, §X.4).
/// - `send_id` = the `message_id` recovered verbatim from the landed record (§X.4).
/// - Latency is normal/UNBOUNDED (§X.6): no timeout — PENDING until it lands.
/// - Scope: `accept(id)` gates emission to ids THIS recipient genuinely received
///   (production passes a `tracked_message_ids` membership check — the same scoping
///   `emit_seen_failed_for_unpulled` applies). Defense-in-depth behind the
///   structural matcher: even a sibling-pasted wrapper for a never-received id
///   cannot fire a phantom terminal.
/// - Dedup: a `seen` set emits at most one `message-seen` per `message_id`; the
///   byte offset is advanced ONLY to the last newline boundary (a record still being
///   written by CC is re-read intact next poll), and a transcript shrink/rotation or
///   a non-boundary offset safely restarts from 0 (the `seen` set keeps it idempotent).
/// - Best-effort: every failure is swallowed; the loop never panics out.
fn run_received_observer(
    state_dir: &Path,
    projects_dir: &Path,
    session_id: &str,
    accept: impl Fn(&str) -> bool,
) {
    let writer = crate::events::EventWriter::for_key(
        state_dir,
        session_id,
        Some(session_id.to_string()),
        None,
    );
    let clock = crate::effects::RealClock;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut offset: usize = 0;
    loop {
        if let Some(path) = crate::jsonl::find_jsonl_path(projects_dir, session_id, None) {
            if let Ok(content) = std::fs::read_to_string(&path) {
                // Rotation/rewrite (shrank) or a non-boundary offset → restart at 0;
                // the `seen` set keeps reprocessing idempotent (one emit per id).
                if content.len() < offset || !content.is_char_boundary(offset) {
                    offset = 0;
                }
                for p in crate::sendpty::parse_jsonl_slice(&content[offset..]) {
                    let rec: crate::sendpty::JsonlRecord =
                        serde_json::from_value(p.value.clone()).unwrap_or_default();
                    if let Some(text) = crate::sendpty::user_record_text(&rec) {
                        for (mid, body) in extract_relay_messages(&text) {
                            // Scope to genuinely-received ids, then dedup per id.
                            if accept(&mid) && seen.insert(mid.clone()) {
                                crate::events::warn_emit(
                                    &writer,
                                    &clock,
                                    &crate::events::Payload::MessageSeen {
                                        send_id: mid,
                                        content_sha256: crate::events::sha256_hex(body.as_bytes()),
                                    },
                                );
                            }
                        }
                    }
                }
                // Advance only to the last COMPLETE line so a record still being
                // written by CC at poll time is re-read intact next poll (a raw
                // `content.len()` would split it mid-line and never re-parse it).
                offset = content.rfind('\n').map_or(offset, |i| i + 1);
            }
        }
        std::thread::sleep(Duration::from_millis(RECEIVED_OBSERVE_POLL_MS));
    }
}

/// Extract the relay `(message_id, inner_body)` delivery from a landed
/// channel-wrapped user record. A relay delivery is ONE `<channel … message_id="ID">
/// BODY</channel>` wrapper that Claude Code injects as a `type:"user"` record; its
/// BODY is the sender's verbatim, arbitrary message — which routinely contains the
/// literal `message_id="…"`, bare `<channel `/`</channel>` text, and whole QUOTED
/// `<channel>` wrappers (one or several — a "here's what X and Y said" forward).
///
/// **Why this is deliberately NOT a structural body parse (load-bearing — three
/// prior wrong-fires).** Any attempt to find delivery boundaries *inside* the body
/// is fundamentally ambiguous: a body can forge any `<channel>`/`</channel>`
/// sequence (a plain mention, a nested quote, a 2nd+ quote, a bare/stray close), and
/// each ambiguity leaked a phantom `message-seen` for an UNRELATED `send_id` — a
/// WRONG-FIRE (the single forbidden outcome). So we recover the id ONLY from the
/// **FIRST/outermost** `<channel ` opening tag's ATTRIBUTES and treat the entire
/// remainder as opaque body — we never parse the body for ids. This is robust *by
/// construction* against the whole class.
///
/// Trade-off (SAFE): if Claude Code ever batches several deliveries as siblings in
/// ONE user record, only the first is recovered; the rest stay PENDING (§X.6) — a
/// false-NEGATIVE, never a wrong-fire. (Each relay message is normally its own
/// record.) The observer additionally scopes emission to ids this recipient
/// genuinely received (`accept`). Residual (documented for cc-5): a record whose
/// FIRST `<channel ` is a quoted wrapper of a received-but-unpulled id — narrow
/// (agents nest quotes in prose, not lead with a raw wrapper) and best closed at the
/// live layer (e.g. the inbox-file consumed-on-pull cross-check).
///
/// `content_sha256` is over the EXTRACTED inner body (§X.3.4 — `sha256(wrapped) ≠
/// sha256(body)`); recipient-side it is ADVISORY (bond resolves by `send_id`).
/// Returns at most one pair. No `message_id` attribute → none.
fn extract_relay_messages(text: &str) -> Vec<(String, String)> {
    const OPEN: &str = "<channel ";
    const CLOSE: &str = "</channel>";
    const KEY: &str = "message_id=\"";
    let Some(tpos) = text.find(OPEN) else {
        return Vec::new();
    };
    let after_open = &text[tpos + OPEN.len()..];
    let Some(gt) = after_open.find('>') else {
        return Vec::new();
    };
    let attrs = &after_open[..gt]; // the FIRST opening tag's attributes ONLY
    let region = &after_open[gt + 1..]; // everything after that tag's `>` (opaque body)
    let mid = attrs
        .find(KEY)
        .and_then(|k| {
            let v = &attrs[k + KEY.len()..];
            v.find('"').map(|e| v[..e].to_string())
        })
        .unwrap_or_default();
    if mid.is_empty() {
        return Vec::new();
    }
    // Advisory body: up to the LAST `</channel>` (the outer close of a well-formed
    // single wrapper), else the whole remainder. Never used as a join key (§X.4).
    let body = region
        .rfind(CLOSE)
        .map(|c| region[..c].to_string())
        .unwrap_or_else(|| region.to_string());
    vec![(mid, body)]
}

/// The set of relay `message_id`s that DID land in this recipient's transcript
/// (appear in a `type:"user"` record). Used as the §X.3.5 race guard at close.
fn landed_message_ids(projects_dir: &Path, session_id: &str) -> std::collections::HashSet<String> {
    let mut landed = std::collections::HashSet::new();
    if let Some(path) = crate::jsonl::find_jsonl_path(projects_dir, session_id, None) {
        if let Ok(content) = std::fs::read_to_string(&path) {
            for p in crate::sendpty::parse_jsonl_slice(&content) {
                let rec: crate::sendpty::JsonlRecord =
                    serde_json::from_value(p.value.clone()).unwrap_or_default();
                if let Some(text) = crate::sendpty::user_record_text(&rec) {
                    for (mid, _) in extract_relay_messages(&text) {
                        landed.insert(mid);
                    }
                }
            }
        }
    }
    landed
}

/// §X.3.5 (3-phase delivery) — emit `seen-failed{recipient-gone}` at the recipient
/// session-close bookend for every message THIS session received that never landed
/// in the transcript. Scoped to THIS session's own received message_ids
/// ([`RelayState::tracked_message_ids`]) — NOT the shared per-home inbox dir — so a
/// co-homed session's still-pending message is never wrongly failed. The final
/// transcript scan ([`landed_message_ids`]) is the race guard (§X.3.5): a landed id
/// was/will be a `message-seen`, so only an absent id is a genuine recipient-gone,
/// and the two terminals can never both land for one send_id. Best-effort.
fn emit_seen_failed_for_unpulled(server: &RelayServer) {
    let session_id = &server.session_id;
    let paths = &server.paths;
    let tracked: Vec<String> = {
        let state = server.state.lock().unwrap_or_else(|p| p.into_inner());
        state.tracked_message_ids()
    };
    if tracked.is_empty() {
        return;
    }
    let landed = landed_message_ids(&paths.projects_dir, session_id);
    let writer = crate::events::EventWriter::for_key(
        &paths.state_dir,
        session_id,
        Some(session_id.to_string()),
        None,
    );
    let clock = crate::effects::RealClock;
    for mid in tracked {
        if !landed.contains(&mid) {
            crate::events::warn_emit(
                &writer,
                &clock,
                &crate::events::Payload::SeenFailed {
                    send_id: mid,
                    reason: "recipient-gone".to_string(),
                },
            );
        }
    }
}

/// SELF-HEAL the relay MCP registration at relay-boot time (relay-path
/// hardening v2, owner ruling). Reads `~/.claude.json` and — ONLY if the stored
/// `relay.command` is genuinely BROKEN (an ABSOLUTE path naming a file that no
/// longer exists) — repairs it by re-pointing to the BARE `sb` command via
/// `claude mcp add` (remove-then-add, the idempotent re-point in
/// [`register::register_relay`]).
///
/// This is now purely a BACKSTOP for a broken LEGACY absolute-path entry. The
/// form we register today is the bare `sb` (resolved via PATH), which never goes
/// stale on a binary move — so self-heal must NOT touch it (rewriting bare →
/// absolute would re-introduce the very staleness class this closes). A bare
/// command, or an absolute path that still exists, is VALID and left alone.
///
/// SEAMS (L9a): `home` (resolved from the injected `env` by the caller) locates
/// the config; the `claude mcp` shell-out + the `claude`-on-PATH probe go through
/// the injected `exec`; the stored-path existence probe is the injected
/// `command_exists`. The pure decision is [`register::relay_command_is_stale`].
///
/// CONTRACT: best-effort, NON-FATAL, NON-INTERACTIVE. It NEVER blocks, prompts,
/// or fails the relay boot. The common steady state (a bare entry, or an existing
/// absolute one) is a cheap file-read (and a stat only for the absolute case)
/// with NO subprocess. It only REPAIRS a broken existing registration — it never
/// creates one (consent: an unregistered relay is left alone, exactly as
/// bootstrap's non-TTY path does). `claude` absent → nothing to drive → no-op.
fn self_heal_registration(home: &Path, exec: &impl Exec, command_exists: impl Fn(&str) -> bool) {
    // Read Claude Code's user-scope config. Absent/unreadable → nothing stored to
    // heal (a fresh box registers via bootstrap, not here).
    let claude_json = match std::fs::read_to_string(home.join(".claude.json")) {
        Ok(s) => s,
        Err(_) => return,
    };

    // Pure decision: only a registered-AND-broken (stale absolute path) entry
    // warrants a repair. A bare command — or an existing absolute path — is valid.
    if !register::relay_command_is_stale(&claude_json, &command_exists) {
        return;
    }

    // Broken legacy entry confirmed. We repair by driving `claude mcp` — so
    // `claude` must be on PATH. Absent → leave the (broken) entry;
    // bootstrap/`relay:register` can fix it once Claude Code is installed. Never
    // fatal.
    if !command_exists("claude") {
        eprintln!(
            "relay: registration points at a stale `sb` path but `claude` is not on PATH — \
             cannot self-heal now; run `sb relay:repoint` after installing Claude Code."
        );
        return;
    }

    match register::register_relay(exec) {
        Ok(()) => eprintln!(
            "relay: self-healed a broken registration — re-pointed at the bare `{}` command.",
            register::RELAY_BARE_COMMAND
        ),
        Err(e) => eprintln!("relay: self-heal re-point failed ({e}); left the existing entry."),
    }
}

/// Find a free port by BIND-TESTING from `start`, scanning up to `+PORT_SCAN_SPAN`
/// (server.ts:68-82). Returns the BOUND listener (so there is no bind/use race —
/// the bun version re-binds after the scan, a TOCTOU we avoid by keeping the
/// listener we tested) + its port. `None` if the whole span is taken. P-D1.
fn find_port(start: u16) -> Option<(TcpListener, u16)> {
    for offset in 0..PORT_SCAN_SPAN {
        let port = start.checked_add(offset)?;
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
            return Some((listener, port));
        }
    }
    None
}

/// Sidecar path `<relay_dir>/<pid>.json` (P-D2, server.ts:339).
fn sidecar_path(paths: &SbPaths, pid: u32) -> PathBuf {
    paths.relay_dir.join(format!("{pid}.json"))
}

/// Write the sidecar `<relay_dir>/<pid>.json` = `{port, pid, sessionId, startedAt}`
/// (P-D2, server.ts:338-347). This is the shape `relay::read_sidecars` parses
/// (port + sessionId truthy required) — FROZEN. `startedAt` is an ISO-8601 UTC
/// timestamp (server.ts uses `new Date().toISOString()`; we use the crate's
/// `epoch_ms_to_iso`, the verified-vs-bun `toISOString` port).
fn write_sidecar(paths: &SbPaths, port: u16, pid: u32, session_id: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(&paths.relay_dir)?;
    let started_at = crate::render::epoch_ms_to_iso(now_ms());
    let record = serde_json::json!({
        "port": port,
        "pid": pid,
        "sessionId": session_id,
        "startedAt": started_at,
    });
    std::fs::write(sidecar_path(paths, pid), record.to_string())
}

/// Current wall-clock epoch ms (for `startedAt`). The server is a real
/// long-running process; the real clock is correct here (spec §2 "Time").
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Random uuid (P-D3) — no `uuid` crate in-tree; mint a v4 from /dev/urandom
// ---------------------------------------------------------------------------

/// Mint a random RFC-4122 v4 UUID string (server.ts:35 `crypto.randomUUID()`).
///
/// The crate has NO `uuid` dependency (and the workspace posture is no-new-deps),
/// so we read 16 random bytes from `/dev/urandom`, set the version (4) + variant
/// (RFC-4122) bits, and format the canonical `8-4-4-4-12` hyphenated lowercase
/// hex. This is only ever a FALLBACK — CLAUDE_CODE_SESSION_ID is set in the real
/// machine-spawn path; the uuid covers the cli/dev path (and tests). On the
/// (essentially impossible) `/dev/urandom` read failure we fall back to a
/// pid+time-seeded value so a session id is ALWAYS produced (non-empty is a
/// `/health` contract requirement).
fn random_uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    if read_urandom(&mut bytes).is_err() {
        // Degenerate fallback: derive 16 bytes from pid + nanos so we never emit
        // an empty/duplicate-prone id. Not cryptographic, but the urandom path is
        // the norm; this only fires if /dev/urandom is unreadable.
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

/// Fill `buf` with bytes from `/dev/urandom`.
fn read_urandom(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

// ---------------------------------------------------------------------------
// Signal cleanup (P-D4) — unlink sidecar on SIGTERM/SIGINT, exit 0
// ---------------------------------------------------------------------------

/// The sidecar path, pre-encoded as a NUL-terminated C string for the signal
/// handler. Set ONCE at boot; read by the async-signal handler. Storing the
/// pre-built CString is what makes the handler async-signal-SAFE: it does NO
/// allocation / formatting in signal context — only `libc::unlink` + `_exit`,
/// both async-signal-safe syscalls.
static SIDECAR_CPATH: OnceLock<CString> = OnceLock::new();

/// Install SIGTERM + SIGINT handlers that unlink the sidecar and `_exit(0)`
/// (P-D4, server.ts:357-358: both signals `cleanupRelayFile(); process.exit(0)`).
///
/// The crate has `libc` (no `signal_hook`/`ctrlc` dep; workspace no-new-deps
/// posture). We use `libc::signal` to register `on_signal`. The handler is kept
/// async-signal-safe: the only work is `unlink(precomputed_cpath)` + `_exit(0)` —
/// no locks, no allocation, no `println!`. The path is pre-encoded into
/// `SIDECAR_CPATH` here (in normal context) precisely so the handler need not
/// build it.
fn install_signal_cleanup(path: &Path) {
    // Pre-encode the path; if it somehow contains an interior NUL we skip handler
    // install rather than risk a bad unlink (paths from SbPaths never do).
    if let Ok(cpath) = CString::new(path.as_os_str().as_encoded_bytes()) {
        let _ = SIDECAR_CPATH.set(cpath);
    }
    // SAFETY: registering a signal handler is sound; `on_signal` is
    // async-signal-safe (only unlink + _exit, no allocation/locks).
    unsafe {
        let handler = on_signal as *const () as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

/// Async-signal-safe handler: unlink the pre-built sidecar path, then `_exit(0)`
/// (P-D4 exits 0 on SIGTERM/SIGINT). Does NOTHING that is not async-signal-safe.
extern "C" fn on_signal(_sig: libc::c_int) {
    // SAFETY: `unlink` + `_exit` are async-signal-safe. We read a CString set once
    // at boot (no mutation after install) — sound to read its pointer here.
    unsafe {
        if let Some(cpath) = SIDECAR_CPATH.get() {
            libc::unlink(cpath.as_ptr());
        }
        libc::_exit(0);
    }
}

// ---------------------------------------------------------------------------
// Test-spawn entry (for the §4 QA harness — drives the SAME listener/endpoints)
// ---------------------------------------------------------------------------

/// A handle to an in-process server spawned by [`RelayServer::spawn_for_test`].
/// Carries the bound `port` (the QA harness points the real `CcRelay` client at
/// it) and an `Arc<RelayServer>` for shutdown/inspection.
///
/// NOTE: the listener thread is detached. `shutdown()` drops the harness's
/// references; the OS reclaims the bound port when the process exits (tests are
/// short-lived processes). The accept loop is intentionally NOT force-killed —
/// there is no clean cross-platform way to interrupt a blocking `accept()`
/// without a self-pipe, and the test process tear-down reclaims everything. This
/// matches the spec's "minimal but real" bar: the SAME `http::serve` + endpoint
/// code runs as in production, not a fake.
pub struct TestServerHandle {
    /// The OS-assigned bound port (the harness connects the real client here).
    pub port: u16,
    /// The shared server (so a test can inspect/lock state if needed).
    pub server: Arc<RelayServer>,
}

impl TestServerHandle {
    /// Release the harness's handle. The accept loop runs until process exit
    /// (see the struct doc — no force-interrupt of a blocking accept).
    pub fn shutdown(self) {
        // Dropping `self` drops the harness's `Arc`; the detached listener thread
        // keeps its own clone. Explicit method so tests read clearly.
    }
}

impl RelayServer {
    /// Spawn the server IN-PROCESS on `home` + `port_base`, returning a handle
    /// with the bound port. For the §4 QA harness: it exercises the SAME
    /// `http::serve` + endpoint code as production (NOT a fake), WITHOUT the real
    /// signal/stdin machinery (no signal handlers installed; no MCP loop).
    ///
    /// `park` is the `/replies` long-poll deadline injected through the SAME park
    /// code path (P-F3b) — tests pass e.g. 200ms so scenario 6 (408 timeout) runs
    /// fast; production passes 120s. `request_read_timeout` is the request-read
    /// wall-clock budget (orc carry 4), injected the same way — the slow-drip QA row
    /// passes a SHORT budget; other rows pass the production 10s. Pass a
    /// `RELAY_PORT_BASE` OUTSIDE 8900-9000 (jail discipline) or 0 for an OS-assigned
    /// ephemeral port.
    ///
    /// Writes a real sidecar (so push-back discovery tests — M4 — work) and a
    /// real inbox dir under `home`. Hermetic: everything is under the injected
    /// temp `home`. NOTE: the in-process test spawn drives ONLY the HTTP half (no
    /// MCP stdin loop — the B-group MCP harness spawns the real binary as a
    /// subprocess instead, see `mcp.rs` + the report).
    pub fn spawn_for_test(
        home: &Path,
        port_base: u16,
        park: Duration,
        request_read_timeout: Duration,
    ) -> TestServerHandle {
        let paths = SbPaths::from_home(home);
        let session_id = random_uuid_v4();
        let (listener, port) = if port_base == 0 {
            // OS-assigned ephemeral port (matches tests/relay_contract.rs's
            // 127.0.0.1:0 — never a fixed port, so parallel tests never collide).
            let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral test port");
            let p = l.local_addr().unwrap().port();
            (l, p)
        } else {
            find_port(port_base).expect("find free test port from base")
        };
        let pid = std::process::id();
        // Real sidecar (non-fatal on failure, as in production).
        let _ = write_sidecar(&paths, port, pid, &session_id);

        let server = RelayServer::new(session_id, port, pid, paths, park, request_read_timeout);
        server.spawn_sweeper();

        // Detached listener thread — same accept loop production uses.
        let listener_server = Arc::clone(&server);
        std::thread::spawn(move || http::serve(listener, listener_server));

        TestServerHandle { port, server }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::ScriptedExec;
    use crate::relay::RelayContract;
    use crate::relay_http::CcRelay;

    // ===== self-heal on relay boot (relay-path hardening) =====

    #[test]
    fn self_heal_repairs_a_broken_absolute_path() {
        // Seed a `~/.claude.json` whose relay command is a legacy ABSOLUTE path
        // naming a file that is gone — the only BROKEN shape under v2. Self-heal
        // must drive `claude mcp add` to re-point it at the bare `dispatch`.
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"mcpServers":{"relay":{"command":"/old/gone/sb","args":["relay:serve"]}}}"#,
        )
        .unwrap();
        let exec = ScriptedExec::new().on(
            "claude",
            &["mcp", "add"],
            Some(0),
            "Added stdio MCP server relay",
            "",
        );
        // `claude` present; the stored bogus absolute path does NOT exist.
        let command_exists = |c: &str| c == "claude";
        self_heal_registration(home.path(), &exec, command_exists);
        // It re-points to the BARE `dispatch` (NOT an absolute path).
        assert!(
            exec.ran(
                "claude",
                &[
                    "mcp",
                    "add",
                    "-s",
                    "user",
                    "relay",
                    "--",
                    "dispatch",
                    "relay:serve"
                ]
            ),
            "broken registration must be re-pointed to bare `dispatch` via `claude mcp add`: {:?}",
            exec.log()
        );
        // And it remove-then-adds (the idempotent re-point) BEFORE the add.
        assert!(exec.ran("claude", &["mcp", "remove", "-s", "user", "relay"]));
    }

    #[test]
    fn self_heal_noop_for_bare_command() {
        // The form we register today: bare `sb`. It NEVER goes stale (resolved via
        // PATH), so self-heal must leave it untouched — NOT rewrite it to an
        // absolute path (which would re-introduce the staleness class). NO shell-out.
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"mcpServers":{"relay":{"command":"sb","args":["relay:serve"]}}}"#,
        )
        .unwrap();
        let exec = ScriptedExec::new();
        self_heal_registration(home.path(), &exec, |c| c == "claude");
        assert!(
            exec.log().is_empty(),
            "a bare-command registration must not be rewritten: {:?}",
            exec.log()
        );
    }

    #[test]
    fn self_heal_noop_for_existing_absolute_path() {
        // A legacy absolute-path entry that still EXISTS is valid — leave it alone.
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"mcpServers":{"relay":{"command":"/here/sb"}}}"#,
        )
        .unwrap();
        let exec = ScriptedExec::new();
        // claude present AND the stored absolute path exists → not broken.
        self_heal_registration(home.path(), &exec, |c| c == "claude" || c == "/here/sb");
        assert!(
            exec.log().is_empty(),
            "an existing absolute-path registration must not be disturbed: {:?}",
            exec.log()
        );
    }

    #[test]
    fn self_heal_noop_when_unregistered() {
        // No relay entry → self-heal NEVER fabricates a registration (consent).
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"mcpServers":{"playwright":{}}}"#,
        )
        .unwrap();
        let exec = ScriptedExec::new();
        self_heal_registration(home.path(), &exec, |c| c == "claude");
        assert!(
            exec.log().is_empty(),
            "unregistered relay must not shell out: {:?}",
            exec.log()
        );
    }

    #[test]
    fn self_heal_noop_when_no_config() {
        // Absent `~/.claude.json` → nothing stored to heal, no subprocess.
        let home = tempfile::tempdir().unwrap();
        let exec = ScriptedExec::new();
        self_heal_registration(home.path(), &exec, |c| c == "claude");
        assert!(exec.log().is_empty());
    }

    #[test]
    fn self_heal_noop_when_claude_absent() {
        // Stale entry but `claude` not on PATH → cannot drive `claude mcp`; leave
        // it (a notice is printed to stderr) and NEVER shell out.
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"mcpServers":{"relay":{"command":"/old/gone/sb"}}}"#,
        )
        .unwrap();
        let exec = ScriptedExec::new();
        self_heal_registration(home.path(), &exec, |_| false);
        assert!(
            exec.log().is_empty(),
            "claude-absent must not shell out: {:?}",
            exec.log()
        );
    }

    #[test]
    fn random_uuid_v4_has_canonical_shape() {
        let id = random_uuid_v4();
        // 8-4-4-4-12 hyphenated lowercase hex = 36 chars.
        assert_eq!(id.len(), 36, "uuid len: {id}");
        let groups: Vec<&str> = id.split('-').collect();
        assert_eq!(groups.len(), 5);
        assert_eq!(groups[0].len(), 8);
        assert_eq!(groups[1].len(), 4);
        assert_eq!(groups[2].len(), 4);
        assert_eq!(groups[3].len(), 4);
        assert_eq!(groups[4].len(), 12);
        // Version 4 nibble + RFC-4122 variant.
        assert_eq!(&groups[2][..1], "4", "version-4 nibble: {id}");
        assert!(
            matches!(&groups[3][..1], "8" | "9" | "a" | "b"),
            "RFC-4122 variant: {id}"
        );
        // All hex, lowercase.
        assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
        assert_eq!(id, id.to_lowercase());
    }

    #[test]
    fn random_uuid_v4_is_unique_across_calls() {
        let a = random_uuid_v4();
        let b = random_uuid_v4();
        assert_ne!(a, b, "two uuids must differ");
    }

    #[test]
    fn find_port_picks_a_free_port_from_base() {
        // Bind nothing special; an ephemeral high base should be free. We assert
        // the returned listener is actually bound and the port is in range.
        let base = 28900; // OUTSIDE 8900-9000 (jail discipline).
        let (listener, port) = find_port(base).expect("a free port near base");
        assert!(
            (base..base + PORT_SCAN_SPAN).contains(&port),
            "port {port} not in scan span from {base}"
        );
        assert_eq!(listener.local_addr().unwrap().port(), port);
    }

    #[test]
    fn find_port_skips_a_taken_base() {
        // Occupy `base`; find_port must return `base+1` (or later free port).
        let base = 28950;
        let _occupied = TcpListener::bind(("127.0.0.1", base)).expect("occupy base");
        let (_listener, port) = find_port(base).expect("a free port past the taken base");
        assert!(port > base, "must skip the occupied base port: got {port}");
    }

    /// M5a item v — find_port FAIL-FAST on a fully-exhausted range. Occupy EVERY
    /// port in a high (out-of-jail) scan span, then assert find_port returns `None`
    /// cleanly (no hang, no spin — the boot path turns this into a one-line eprintln
    /// + exit 1). We hold the whole span bound for the duration of the assertion.
    #[test]
    fn find_port_fail_fast_returns_none_when_range_exhausted() {
        // A high base far OUTSIDE the 8900-8999 jail range so we never touch the
        // real fleet's ports. Bind every port in the span.
        let base = 39000u16;
        let mut held = Vec::with_capacity(PORT_SCAN_SPAN as usize);
        for offset in 0..PORT_SCAN_SPAN {
            // If a port is already taken by something else, the span isn't fully ours
            // — skip the whole test rather than risk a flake (no false red).
            match TcpListener::bind(("127.0.0.1", base + offset)) {
                Ok(l) => held.push(l),
                Err(_) => return,
            }
        }
        // Every port in the span is now bound → genuine exhaustion → None, FAST.
        let started = Instant::now();
        let result = find_port(base);
        assert!(
            result.is_none(),
            "a fully-exhausted range must fail-fast with None, not a port"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "find_port must return promptly on exhaustion (no spin/retry)"
        );
        drop(held);
    }

    /// In-process smoke (spec gate): spawn_for_test, then hit /health with the
    /// REAL `CcRelay` client and assert the `RelayHealth` round-trips. Proves the
    /// listener + /health work end-to-end against the frozen client.
    #[test]
    fn smoke_health_roundtrips_with_real_client() {
        let tmp = std::env::temp_dir().join(format!("relay-m2-smoke-{}", std::process::id()));
        let handle = RelayServer::spawn_for_test(
            &tmp,
            0,
            Duration::from_millis(200),
            Duration::from_secs(10),
        );
        let client = CcRelay::new();

        // Retry briefly: the detached listener thread may not have called accept()
        // the instant spawn_for_test returns.
        let mut health = None;
        for _ in 0..50 {
            if let Ok(h) = client.health(handle.port, 1000) {
                health = Some(h);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let health = health.expect("/health must round-trip via the real client");
        assert_eq!(health.port, handle.port);
        assert!(!health.session_id.is_empty(), "sessionId must be non-empty");
        assert_eq!(health.status, "ok");
        assert_eq!(health.pid as u32, std::process::id());

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Scenario 6 (time-scaled): a `/replies/<id>` with no buffered reply and no
    /// resolver (M2 has no reply tool) parks for the INJECTED budget, then 408s.
    /// Exercises the production park code path with a small injected deadline
    /// (P-F3b). The client surfaces 408 as a `ServerError` (non-2xx).
    #[test]
    fn replies_times_out_to_408_with_injected_budget() {
        let tmp = std::env::temp_dir().join(format!("relay-m2-408-{}", std::process::id()));
        let handle = RelayServer::spawn_for_test(
            &tmp,
            0,
            Duration::from_millis(150),
            Duration::from_secs(10),
        );
        let client = CcRelay::new();

        // Wait for the listener to be up.
        for _ in 0..50 {
            if client.health(handle.port, 1000).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        // fetch_reply gives the long-poll its full budget; the server parks
        // `reply_park_timeout` (150ms) then returns 408 → client ServerError.
        let started = Instant::now();
        let result = client.fetch_reply(handle.port, "relay-1-1", 5000);
        let elapsed = started.elapsed();
        assert!(
            result.is_err(),
            "a 408 timeout must surface as a client error, got {result:?}"
        );
        // The park honored the INJECTED 150ms budget, not the production 120s.
        assert!(
            elapsed < Duration::from_secs(5),
            "park must honor the injected budget, took {elapsed:?}"
        );

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // -----------------------------------------------------------------------
    // M4 delivery tests (deliver_reply + origin inbox fallback + poison)
    // -----------------------------------------------------------------------

    use crate::relay_server::state::origin_from_inbox;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A process-unique temp home for a delivery test (avoids cross-test
    /// sidecar/inbox collisions in the shared relay_dir/inbox_dir layout).
    fn unique_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("relay-m4-{tag}-{}-{n}", std::process::id()))
    }

    /// Build an in-process server directly (no HTTP listener) for delivery unit
    /// tests that drive `deliver_reply` straight, not over the socket. Writes a real
    /// inbox dir under `home`. Park/read budgets are short (unused by these paths).
    fn bare_server(home: &Path) -> Arc<RelayServer> {
        let paths = SbPaths::from_home(home);
        RelayServer::new(
            "self-session".to_string(),
            0,
            std::process::id(),
            paths,
            Duration::from_millis(50),
            Duration::from_secs(10),
        )
    }

    /// Write an inbox file `<inbox>/<id>.json` the way `http::handle_message` does.
    fn write_inbox(server: &RelayServer, message_id: &str, text: &str, from_session: &str) {
        std::fs::create_dir_all(&server.paths.inbox_dir).unwrap();
        let record = serde_json::json!({
            "text": text,
            "from_session": from_session,
            "message_id": message_id,
            "received_at": "2026-01-01T00:00:00.000Z",
        });
        std::fs::write(
            server.paths.inbox_dir.join(format!("{message_id}.json")),
            record.to_string(),
        )
        .unwrap();
    }

    // --- origin_from_inbox fallback (cond 5) ---

    #[test]
    fn origin_from_inbox_reads_persisted_from_and_rederives_is_reply() {
        let home = unique_home("inbox-fallback");
        let server = bare_server(&home);
        // A plain (non-reply) persisted message.
        write_inbox(&server, "relay-1-1", "hello", "session-A");
        let rec = origin_from_inbox(&server.paths.inbox_dir, "relay-1-1")
            .expect("present inbox file yields an origin");
        assert_eq!(rec.from, "session-A");
        assert!(!rec.is_reply, "plain text → is_reply false");

        // A persisted [REPLY to ...] message re-derives is_reply = true.
        write_inbox(&server, "relay-2-1", "[REPLY to relay-9-9] yo", "session-B");
        let rec2 = origin_from_inbox(&server.paths.inbox_dir, "relay-2-1").expect("present");
        assert_eq!(rec2.from, "session-B");
        assert!(
            rec2.is_reply,
            "[REPLY to ...] text → is_reply re-derived true"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn origin_from_inbox_absent_file_returns_none() {
        let home = unique_home("inbox-absent");
        let server = bare_server(&home);
        // No file written.
        assert!(origin_from_inbox(&server.paths.inbox_dir, "no-such-id").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn origin_from_inbox_missing_from_session_returns_none() {
        let home = unique_home("inbox-nofrom");
        let server = bare_server(&home);
        std::fs::create_dir_all(&server.paths.inbox_dir).unwrap();
        // A file with no string from_session (server.ts:118 typeof guard → None).
        std::fs::write(
            server.paths.inbox_dir.join("relay-3-1.json"),
            r#"{"text":"hi","message_id":"relay-3-1"}"#,
        )
        .unwrap();
        assert!(origin_from_inbox(&server.paths.inbox_dir, "relay-3-1").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    // --- deliver_reply per branch ---

    #[test]
    fn deliver_no_origin_is_honest_not_delivered() {
        let home = unique_home("no-origin");
        let server = bare_server(&home);
        // No origin recorded (in-mem) and no inbox file → NoOrigin → P-E6.
        let out = server.deliver_reply("relay-never-1", "the substance");
        assert!(out.is_error, "no-origin reply must be is_error (P-E6)");
        assert!(out.text.starts_with("NOT DELIVERED"), "{}", out.text);
        assert!(out.text.contains("no origin recorded"), "{}", out.text);
        assert!(out.text.contains("send a fresh message"), "{}", out.text);
        // buffer-first: the text is buffered even on the not-delivered path so a
        // re-peek (a dropped --wait client's re-GET) still returns it (cond 1).
        let mut state = server.state.lock().unwrap();
        assert_eq!(
            state.peek_resolved("relay-never-1", Instant::now()),
            Some("the substance".to_string()),
            "buffer-first: text must be buffered even when not delivered"
        );
        drop(state);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn deliver_loop_prevented_when_origin_is_reply() {
        let home = unique_home("loop");
        let server = bare_server(&home);
        // Record an origin whose inbound message was itself a [REPLY to ...].
        {
            let mut state = server.state.lock().unwrap();
            state.record_origin("relay-r-1".into(), "session-X".into(), true);
        }
        let out = server.deliver_reply("relay-r-1", "would ping-pong");
        assert!(out.is_error, "loop-prevented reply must be is_error (P-E4)");
        assert!(out.text.contains("loop prevention"), "{}", out.text);
        // Buffer-first still holds.
        let mut state = server.state.lock().unwrap();
        assert_eq!(
            state.peek_resolved("relay-r-1", Instant::now()),
            Some("would ping-pong".to_string())
        );
        drop(state);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn deliver_not_addressable_unknown_cli_self() {
        let home = unique_home("notaddr");
        let server = bare_server(&home); // session_id = "self-session"
        for (id, from) in [
            ("relay-u-1", "unknown"),
            ("relay-c-1", "cli"),
            ("relay-s-1", "self-session"),
        ] {
            {
                let mut state = server.state.lock().unwrap();
                state.record_origin(id.into(), from.into(), false);
            }
            let out = server.deliver_reply(id, "x");
            assert!(out.is_error, "origin {from} must be not-addressable (P-E5)");
            assert!(out.text.starts_with("NOT DELIVERED"), "{}", out.text);
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn deliver_push_back_no_live_sidecar_falls_through_to_not_delivered() {
        let home = unique_home("pushback-dead");
        let server = bare_server(&home);
        // Origin is a real addressable session → PushBack — but there is NO live
        // sidecar for it (relay_dir has none matching), so push-back exhausts and we
        // fall through to the honest NOT-DELIVERED (P-E6).
        {
            let mut state = server.state.lock().unwrap();
            state.record_origin("relay-p-1".into(), "session-other".into(), false);
        }
        let out = server.deliver_reply("relay-p-1", "reply text");
        assert!(out.is_error, "no live sidecar → not-delivered");
        assert!(
            out.text
                .contains("no live sidecar found for origin session session-other"),
            "{}",
            out.text
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn deliver_uses_inbox_fallback_when_in_mem_origin_misses() {
        let home = unique_home("fallback-deliver");
        let server = bare_server(&home);
        // No in-mem origin, but a persisted inbox file pins origin = session-other.
        // → PushBack decision (no live sidecar) → not-delivered, proving the inbox
        // fallback fed the decision (otherwise it'd be NoOrigin, a different reason).
        write_inbox(&server, "relay-fb-1", "plain", "session-other");
        let out = server.deliver_reply("relay-fb-1", "reply");
        assert!(out.is_error);
        assert!(
            out.text
                .contains("no live sidecar found for origin session session-other"),
            "inbox fallback must feed the PushBack decision: {}",
            out.text
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn deliver_clears_inbox_file_after_read() {
        let home = unique_home("clear-inbox");
        let server = bare_server(&home);
        write_inbox(&server, "relay-clr-1", "plain", "session-other");
        let inbox_file = server.paths.inbox_dir.join("relay-clr-1.json");
        assert!(inbox_file.exists(), "inbox file present before deliver");
        let _ = server.deliver_reply("relay-clr-1", "reply");
        assert!(
            !inbox_file.exists(),
            "inbox file must be unlinked after deliver (server.ts:222-223)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    // --- ResolveWaiter: in-process parked-waiter resolution (the red-team recipe) ---

    /// The IN-PROCESS recipe the red-teamer/QA use: spawn the server, park a real
    /// `/replies` long-poll via `CcRelay::fetch_reply` on a thread, then resolve it
    /// from another thread via `deliver_reply`. Proves the parked waiter is woken by
    /// notify_all and returns the buffered text (P-E2 / P-G1).
    #[test]
    fn deliver_resolves_a_parked_replies_long_poll_in_process() {
        let home = unique_home("resolve-waiter");
        // Long park budget so the waiter is genuinely parked when we resolve it.
        let handle =
            RelayServer::spawn_for_test(&home, 0, Duration::from_secs(5), Duration::from_secs(10));
        let port = handle.port;
        let server = Arc::clone(&handle.server);

        // Wait for the listener.
        let client = CcRelay::new();
        for _ in 0..50 {
            if client.health(port, 1000).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        // Park a /replies long-poll for an id on a thread (the "sender waiting").
        let poll = std::thread::spawn(move || CcRelay::new().fetch_reply(port, "relay-w-1", 5000));

        // Give the long-poll time to register its waiter + park on the Condvar.
        // Spin until the server reports the waiter, so the resolve races the park
        // correctly (deliver should win whether it lands before or after the park).
        for _ in 0..200 {
            {
                let state = server.state.lock().unwrap();
                if state.has_waiter("relay-w-1") {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // Resolve via the ONE delivery path.
        let out = server.deliver_reply("relay-w-1", "the awaited reply");
        assert!(!out.is_error, "resolved waiter is a delivery, not error");
        assert!(out.text.contains("long-poll resolved"), "{}", out.text);

        // The parked long-poll must return the buffered text.
        let reply = poll.join().expect("poll thread").expect("fetch_reply ok");
        assert_eq!(
            reply.text.as_deref(),
            Some("the awaited reply"),
            "parked /replies must return the resolved text"
        );

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&home);
    }

    // --- poison-resilience (cond 4: recover-guards) ---

    /// Poison the state Mutex (a panic while holding the guard in a scoped thread),
    /// then prove a subsequent `deliver_reply` + a direct state lock still work —
    /// the recover-guards posture (`.unwrap_or_else(|p| p.into_inner())`) means a
    /// panic mid-critical-section does NOT brick the relay fleet-wide (cond 4).
    #[test]
    fn poisoned_state_lock_is_recovered_and_delivery_still_works() {
        let home = unique_home("poison");
        let server = bare_server(&home);

        // Poison the Mutex: panic while holding the guard.
        let s2 = Arc::clone(&server);
        let _ = std::thread::spawn(move || {
            let _guard = s2.state.lock().unwrap();
            panic!("intentional poison");
        })
        .join();
        assert!(server.state.is_poisoned(), "mutex must now be poisoned");

        // deliver_reply must still function (it recovers the poisoned guard).
        let out = server.deliver_reply("relay-poison-1", "after poison");
        assert!(out.is_error, "no-origin → not-delivered (still functions)");
        assert!(out.text.starts_with("NOT DELIVERED"), "{}", out.text);

        // And a subsequent lock recovery returns the buffered text (buffer-first ran
        // despite the poison).
        let mut state = server.state.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(
            state.peek_resolved("relay-poison-1", Instant::now()),
            Some("after poison".to_string())
        );
        drop(state);
        let _ = std::fs::remove_dir_all(&home);
    }

    // -----------------------------------------------------------------------
    // M5a item iv — mint seq seed
    // -----------------------------------------------------------------------

    #[test]
    fn mint_seq_seed_is_bounded_to_low_32_bits() {
        // The seed must fit the low 32 bits (headroom invariant) so the seeded
        // `seq += 1` mint can never approach u64::MAX.
        for pid in [1u32, 42, 1174, u32::MAX] {
            let seed = mint_seq_seed(pid);
            assert!(
                seed <= SEQ_SEED_MASK,
                "seed {seed} for pid {pid} must be masked to the low 32 bits"
            );
        }
    }

    #[test]
    fn mint_seq_seed_varies_across_pids() {
        // Different pids should (with overwhelming probability — urandom + pid mix)
        // produce different seeds. We assert a representative spread is not all-equal
        // (a degenerate all-equal would defeat the divergence purpose).
        let seeds: Vec<u64> = (1u32..=8).map(mint_seq_seed).collect();
        let first = seeds[0];
        assert!(
            seeds.iter().any(|s| *s != first),
            "seeds across pids must not be uniformly identical: {seeds:?}"
        );
    }

    // -----------------------------------------------------------------------
    // M5a item iii — is_sidecar_stale (pure) + sweep_stale_sidecars
    // -----------------------------------------------------------------------

    #[test]
    fn is_sidecar_stale_dead_pid_is_stale() {
        // is_alive=false (provably dead, ESRCH) and not our own → STALE.
        assert!(is_sidecar_stale(4242, 1, |_| false));
    }

    #[test]
    fn is_sidecar_stale_live_pid_is_not_stale() {
        // is_alive=true (process exists) → KEEP, never stale.
        assert!(!is_sidecar_stale(4242, 1, |_| true));
    }

    #[test]
    fn is_sidecar_stale_uncertain_eperm_is_kept() {
        // The EPERM (alive-but-not-ours) case surfaces as is_alive=true from
        // pid_is_alive — modeled here as the fn returning true → KEEP. CONSERVATIVE:
        // any uncertainty keeps the sidecar (never remove a possibly-live peer).
        assert!(
            !is_sidecar_stale(4242, 1, |_| true),
            "uncertain liveness (EPERM modeled as alive) must KEEP the sidecar"
        );
    }

    #[test]
    fn is_sidecar_stale_never_removes_own_pid() {
        // Even if the liveness oracle (wrongly) said dead, our OWN pid is never
        // stale — we must never sweep our own sidecar.
        assert!(
            !is_sidecar_stale(777, 777, |_| false),
            "own pid must never be considered stale"
        );
    }

    #[test]
    fn sweep_stale_sidecars_removes_only_dead_keeps_live_and_own() {
        let home = unique_home("sweep-sidecars");
        let relay_dir = home.join(".claude").join("relay");
        std::fs::create_dir_all(&relay_dir).unwrap();

        // Write four sidecars: a DEAD peer, a LIVE peer, OUR own, and a non-pid junk.
        let dead_pid = 111u32;
        let live_pid = 222u32;
        let own_pid = 333u32;
        for (pid, sid) in [(dead_pid, "dead"), (live_pid, "live"), (own_pid, "self")] {
            let rec = serde_json::json!({
                "port": 28000 + pid, "pid": pid, "sessionId": sid,
                "startedAt": "2026-01-01T00:00:00.000Z",
            });
            std::fs::write(relay_dir.join(format!("{pid}.json")), rec.to_string()).unwrap();
        }
        // A non-json file and a json with no pin-able pid must be left alone.
        std::fs::write(relay_dir.join("notes.txt"), "ignore me").unwrap();

        // Liveness oracle: only `live_pid` is alive. (own_pid is excluded by the
        // own-pid guard regardless of the oracle.)
        let removed = sweep_stale_sidecars(&relay_dir, own_pid, |pid| pid == live_pid);

        assert_eq!(removed, 1, "exactly the one dead-peer sidecar is removed");
        assert!(
            !relay_dir.join(format!("{dead_pid}.json")).exists(),
            "dead peer sidecar must be removed"
        );
        assert!(
            relay_dir.join(format!("{live_pid}.json")).exists(),
            "live peer sidecar must SURVIVE (never remove a healthy peer)"
        );
        assert!(
            relay_dir.join(format!("{own_pid}.json")).exists(),
            "our own sidecar must SURVIVE the sweep"
        );
        assert!(
            relay_dir.join("notes.txt").exists(),
            "non-json files are left alone"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn sweep_stale_sidecars_missing_dir_is_zero() {
        let home = unique_home("sweep-missing");
        let removed = sweep_stale_sidecars(&home.join("nope"), 1, |_| false);
        assert_eq!(removed, 0, "missing relay_dir → nothing to sweep");
    }

    #[test]
    fn sweep_stale_sidecars_falls_back_to_filename_pid() {
        // A sidecar whose JSON lacks a usable `pid` field still gets its pid from the
        // filename stem (sidecars are named `<pid>.json`) — a dead one is swept.
        let home = unique_home("sweep-fnamepid");
        let relay_dir = home.join(".claude").join("relay");
        std::fs::create_dir_all(&relay_dir).unwrap();
        // No `pid` field in the record; filename stem 999 is the dead pid.
        std::fs::write(
            relay_dir.join("999.json"),
            r#"{"port":28999,"sessionId":"s","startedAt":"x"}"#,
        )
        .unwrap();
        let removed = sweep_stale_sidecars(&relay_dir, 1, |_| false);
        assert_eq!(removed, 1, "filename-stem pid 999 (dead) is swept");
        assert!(!relay_dir.join("999.json").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn pid_is_alive_reports_self_alive_and_unused_pid_dead() {
        // Our own pid is unambiguously alive.
        assert!(pid_is_alive(std::process::id()), "self pid must be alive");
        // pid 0 is treated as uncertain → alive (conservative).
        assert!(pid_is_alive(0), "pid 0 is conservatively alive");
        // A very-high pid that is essentially never allocated should read as dead
        // (ESRCH). Guard the assertion: if the OS somehow has it, skip (no false red).
        let improbable = u32::MAX - 1;
        if unsafe { libc::kill(improbable as libc::pid_t, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            assert!(
                !pid_is_alive(improbable),
                "an unallocated pid must read as dead (ESRCH)"
            );
        }
    }

    // =======================================================================
    // §X (3-phase delivery) — Tier-2 seam-integration proof of the RECIPIENT
    // side (Group B): the real private observer functions emit real records
    // into a real `<state>/sessions/<uuid>.events.jsonl`, tailed from disk and
    // asserted against the PINNED-EVENT-CONTRACT §X.3 shapes. These cover the
    // GAP cases I1 (recipient `message-seen`), I3 (latency PENDING), I4
    // (`seen-failed{recipient-gone}`). No record is hand-written: every line is
    // produced by `run_received_observer` / `emit_seen_failed_for_unpulled`.
    //
    // The transcript fixture is a real `type:"user"` Claude record carrying the
    // relay channel wrapper `<channel … message_id="X">BODY</channel>` exactly as
    // the relay MCP delivers it; the observer parses it via the SAME
    // `user_record_text` + `extract_relay_messages` it uses in production.
    // =======================================================================

    use crate::events::{parse_events, EventRecord};
    use std::time::Instant as StdInstant;

    /// Build the recipient's Claude transcript path under a projects dir and write
    /// the given JSONL records there (one per line). Mirrors Claude Code's layout
    /// `<projects_dir>/<slug>/<session_id>.jsonl` (the fallback-scan tier finds it).
    fn write_recipient_transcript(projects_dir: &Path, session_id: &str, lines: &[String]) {
        let proj = projects_dir.join("-tmp-recipient-cwd");
        std::fs::create_dir_all(&proj).unwrap();
        let path = proj.join(format!("{session_id}.jsonl"));
        let body = if lines.is_empty() {
            String::new()
        } else {
            lines.join("\n") + "\n"
        };
        std::fs::write(&path, body).unwrap();
    }

    /// APPEND one record to the recipient transcript (models Claude Code's real
    /// transcript growth — the observer advances a byte offset and reads only the
    /// appended tail, so a late landing must arrive as an append, not a rewrite).
    fn append_recipient_record(projects_dir: &Path, session_id: &str, line: &str) {
        use std::io::Write;
        let path = projects_dir
            .join("-tmp-recipient-cwd")
            .join(format!("{session_id}.jsonl"));
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    /// A real `type:"user"` transcript record carrying a relay-delivered channel
    /// message (the wrapper the relay MCP injects), serialized as one JSONL line.
    fn user_record_with_channel(message_id: &str, body: &str) -> String {
        let wrapped =
            format!("<channel source=\"relay\" from_session=\"sess-A\" message_id=\"{message_id}\">{body}</channel>");
        serde_json::json!({
            "type": "user",
            "message": { "content": wrapped },
        })
        .to_string()
    }

    /// Tail the recipient's real delivery log and return its parsed records.
    fn tail_recipient_events(state_dir: &Path, session_id: &str) -> Vec<EventRecord> {
        let path = state_dir
            .join("sessions")
            .join(format!("{session_id}.events.jsonl"));
        match std::fs::read_to_string(&path) {
            Ok(s) => parse_events(&s).records,
            Err(_) => Vec::new(),
        }
    }

    /// When `DISPATCH_PROOF_DIR` is set, copy the REAL tailed `events.jsonl` for this
    /// session to `<DISPATCH_PROOF_DIR>/<dest>` — the evidence file the oracle reads.
    /// No-op otherwise (so the test stays hermetic in normal runs).
    fn dump_proof(state_dir: &Path, session_id: &str, dest: &str) {
        if let Ok(dir) = std::env::var("DISPATCH_PROOF_DIR") {
            let src = state_dir
                .join("sessions")
                .join(format!("{session_id}.events.jsonl"));
            if let Ok(raw) = std::fs::read_to_string(&src) {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(std::path::Path::new(&dir).join(dest), raw);
            }
        }
    }

    /// Poll the recipient log until `pred` holds or the deadline passes. Returns
    /// the final records either way (the caller asserts).
    fn poll_recipient_events(
        state_dir: &Path,
        session_id: &str,
        timeout: Duration,
        pred: impl Fn(&[EventRecord]) -> bool,
    ) -> Vec<EventRecord> {
        let start = StdInstant::now();
        loop {
            let recs = tail_recipient_events(state_dir, session_id);
            if pred(&recs) || start.elapsed() >= timeout {
                return recs;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // ----- extract_relay_messages (the recipient-side matcher, §X.3.4) --------

    /// The matcher extracts (message_id, inner_body) from a channel-wrapped record
    /// and the body is exactly what is between the open tag's `>` and `</channel>`
    /// (no trim) — and the inner-body hash ≠ the wrapped-text hash (advisory hash,
    /// §X.3.4 / U3).
    #[test]
    fn x_extract_relay_messages_recovers_id_and_inner_body() {
        let body = "the priming payload — line2";
        let wrapped = format!(
            "<channel source=\"relay\" from_session=\"sess-A\" message_id=\"relay-1781000000001-3\">{body}</channel>"
        );
        let got = extract_relay_messages(&wrapped);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "relay-1781000000001-3", "message_id verbatim");
        assert_eq!(got[0].1, body, "inner body extracted exactly, no trim");
        assert_ne!(
            crate::events::sha256_hex(got[0].1.as_bytes()),
            crate::events::sha256_hex(wrapped.as_bytes()),
            "advisory hash is over the inner body, NOT the wrapped text"
        );
    }

    /// WRONG-FIRE regression (the single forbidden outcome): a relay message whose
    /// BODY merely MENTIONS the literal `message_id="…"` substring (an agent quoting
    /// an id — routine good-faith) must NOT yield that id. Only the `<channel>`
    /// wrapper ATTRIBUTE counts; a body-anchored substring match would fire a phantom
    /// `message-seen` for an unrelated, never-delivered `send_id`.
    #[test]
    fn x_extract_ignores_message_id_mentioned_in_body() {
        let wrapped = "<channel source=\"relay\" from_session=\"sess-A\" \
             message_id=\"relay-100-1\">FYI I saw message_id=\"relay-200-2\" land \
             earlier — please ack.</channel>";
        let got = extract_relay_messages(wrapped);
        assert_eq!(
            got.len(),
            1,
            "only the wrapper id, not the body mention: {got:?}"
        );
        assert_eq!(got[0].0, "relay-100-1");
        assert!(
            !got.iter().any(|(id, _)| id == "relay-200-2"),
            "the body-mentioned id must NEVER be extracted (wrong-fire vector)"
        );
    }

    /// WRONG-FIRE regression: a NESTED quoted `<channel>` block inside a delivered
    /// message body (the natural shape of one agent forwarding/quoting another's
    /// relay message) is absorbed into the outer record's body — only the OUTER
    /// wrapper's `message_id` is recovered.
    #[test]
    fn x_extract_nested_quoted_wrapper_yields_only_outer() {
        let wrapped = "<channel source=\"relay\" from_session=\"A\" \
             message_id=\"relay-outer-9\">here is what cc-2 sent: \
             <channel source=\"relay\" from_session=\"cc-2\" message_id=\"relay-inner-4\">do \
             the thing</channel> — advise.</channel>";
        let got = extract_relay_messages(wrapped);
        assert_eq!(got.len(), 1, "only the outer wrapper id: {got:?}");
        assert_eq!(got[0].0, "relay-outer-9");
        assert!(
            !got.iter().any(|(id, _)| id == "relay-inner-4"),
            "the nested quoted id must NOT be extracted"
        );
    }

    /// WRONG-FIRE regression (the round-2 red-team repro): a forwarded "here's what X
    /// and Y said" digest nests TWO+ quoted `<channel>` wrappers in one body. A
    /// first-close-anchored matcher would re-sync after the first inner `</channel>`
    /// and leak the SECOND quoted id as a phantom sibling. Depth-matching absorbs the
    /// entire nested span — only the OUTER (depth-0) delivery id is recovered.
    #[test]
    fn x_extract_ignores_second_quoted_wrapper_in_multiquote_body() {
        let wrapped = "<channel source=\"relay\" from_session=\"A\" \
             message_id=\"relay-genuine-1\">Status. cc-2 said: <channel source=\"relay\" \
             from_session=\"cc-2\" message_id=\"relay-100-2\">first</channel> and cc-3 said: \
             <channel source=\"relay\" from_session=\"cc-3\" message_id=\"relay-100-3\">second\
             </channel>. advise.</channel>";
        let got = extract_relay_messages(wrapped);
        assert_eq!(
            got.len(),
            1,
            "only the outer delivery id, not the quoted ones: {got:?}"
        );
        assert_eq!(got[0].0, "relay-genuine-1");
        assert!(
            !got.iter()
                .any(|(id, _)| id == "relay-100-2" || id == "relay-100-3"),
            "NEITHER quoted id (1st or 2nd+) may be extracted — wrong-fire vector"
        );
    }

    /// WRONG-FIRE regression (round-3 red-team repro): a genuine delivery body that
    /// contains a BARE/stray `</channel>` (a sender pasting a log line or discussing
    /// the relay protocol — good-faith) followed later by a quoted wrapper. A
    /// close-counting parse terminates the outer wrapper at the bare close and
    /// mis-reads the later quoted id as a fresh delivery. First-wrapper-only never
    /// parses the body, so the quoted id is never extracted.
    #[test]
    fn x_extract_bare_close_then_quoted_wrapper_yields_only_outer() {
        let wrapped = "<channel source=\"relay\" from_session=\"A\" \
             message_id=\"relay-deliv-2\">Status. The wrapper looks like </channel> in \
             our logs. earlier: <channel source=\"relay\" from_session=\"A\" \
             message_id=\"relay-99999-5\">do task X</channel></channel>";
        let got = extract_relay_messages(wrapped);
        assert_eq!(got.len(), 1, "only the outer delivery id: {got:?}");
        assert_eq!(got[0].0, "relay-deliv-2");
        assert!(
            !got.iter().any(|(id, _)| id == "relay-99999-5"),
            "a quoted id after a bare </channel> must NOT be extracted (wrong-fire)"
        );
    }

    /// Batched siblings in ONE record: only the FIRST/outermost delivery id is
    /// recovered — the conservative, robust-by-construction choice (see the matcher
    /// doc). A batched 2nd+ sibling stays PENDING (a SAFE false-negative, §X.6), never
    /// a wrong-fire. (Each relay message is normally its own user record.)
    #[test]
    fn x_extract_batched_siblings_recovers_only_the_first() {
        let wrapped = "<channel source=\"relay\" from_session=\"A\" \
             message_id=\"relay-a-1\">first</channel><channel source=\"relay\" \
             from_session=\"B\" message_id=\"relay-b-2\">second</channel>";
        let got = extract_relay_messages(wrapped);
        assert_eq!(got.len(), 1, "only the first delivery id: {got:?}");
        assert_eq!(got[0].0, "relay-a-1");
        assert!(
            !got.iter().any(|(id, _)| id == "relay-b-2"),
            "a batched 2nd sibling is not recovered (PENDING, safe), never a wrong-fire"
        );
    }

    /// Malformed input is graceful: a `<channel >` with no `message_id` attribute
    /// yields no pair; a wrapper missing its `</channel>` yields an empty body (the
    /// recipient-side hash is advisory). Neither panics.
    #[test]
    fn x_extract_malformed_is_graceful() {
        assert!(
            extract_relay_messages("<channel source=\"relay\">no id here</channel>").is_empty()
        );
        let got = extract_relay_messages(
            "<channel source=\"relay\" message_id=\"relay-x-1\">unterminated body",
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "relay-x-1");
        assert_eq!(
            got[0].1, "unterminated body",
            "no depth-matched </channel> → remaining text is the advisory body"
        );
        assert!(extract_relay_messages("plain text, no channel wrapper at all").is_empty());
    }

    // ----- I1 (recipient side): message-seen on transcript landing -----------

    /// I1 (recipient half): a relay `message_id` that lands in the recipient
    /// transcript drives the REAL `run_received_observer` to emit exactly one
    /// `message-seen` terminal into the recipient's own `events.jsonl`, keyed by
    /// `send_id == message_id` recovered verbatim from the landed record. Tailed
    /// from disk. (The sender half — send-initiated + relay-delivered — is proven
    /// in the I1 e2e harness `i1_relay_happy_*` below.)
    #[test]
    fn i1_recipient_message_seen_emitted_on_transcript_landing() {
        let home = unique_home("i1-recv");
        let paths = SbPaths::from_home(&home);
        let session_id = random_uuid_v4();
        let message_id = "relay-1781000000100-7";
        let body = "hello from the sender";

        // The recipient pulled the relay message into context (a real user record).
        write_recipient_transcript(
            &paths.projects_dir,
            &session_id,
            &[user_record_with_channel(message_id, body)],
        );

        // Drive the REAL observer (its production body) in a thread; tail the log.
        let sd = paths.state_dir.clone();
        let pd = paths.projects_dir.clone();
        let sid = session_id.clone();
        let h = std::thread::spawn(move || run_received_observer(&sd, &pd, &sid, |_| true));

        let recs =
            poll_recipient_events(&paths.state_dir, &session_id, Duration::from_secs(5), |r| {
                r.iter().any(|e| e.event == "message-seen")
            });
        // Observer loops forever; we detach it (process tear-down reclaims it).
        drop(h);

        let seen: Vec<&EventRecord> = recs.iter().filter(|e| e.event == "message-seen").collect();
        assert_eq!(seen.len(), 1, "exactly one message-seen; got {recs:?}");
        let m = seen[0];
        assert_eq!(
            m.send_id().as_deref(),
            Some(message_id),
            "send_id == message_id recovered verbatim from the landed record (§X.4)"
        );
        // content_sha256 = sha256(extracted inner body) — advisory recipient-side.
        let raw = std::fs::read_to_string(
            paths
                .state_dir
                .join("sessions")
                .join(format!("{session_id}.events.jsonl")),
        )
        .unwrap();
        let line = raw.lines().find(|l| l.contains("message-seen")).unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["event"], "message-seen");
        assert_eq!(v["v"], 1, "per-record v stays 1 (no envelope field added)");
        assert_eq!(
            v["content_sha256"].as_str().unwrap(),
            crate::events::sha256_hex(body.as_bytes()),
            "content_sha256 over the EXTRACTED inner body"
        );
        assert!(
            v.get("session").is_some(),
            "keyed to recipient uuid (session present)"
        );

        dump_proof(
            &paths.state_dir,
            &session_id,
            "I1-relay-happy-recipient.jsonl",
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// I1 dedup: a second poll cycle over the SAME landed record emits NO second
    /// `message-seen` (the `seen` set keeps it idempotent) — the invariant "exactly
    /// one terminal per send_id" survives re-scans.
    #[test]
    fn i1_recipient_message_seen_is_deduped_across_rescans() {
        let home = unique_home("i1-dedup");
        let paths = SbPaths::from_home(&home);
        let session_id = random_uuid_v4();
        let message_id = "relay-1781000000200-2";
        write_recipient_transcript(
            &paths.projects_dir,
            &session_id,
            &[user_record_with_channel(message_id, "x")],
        );
        let sd = paths.state_dir.clone();
        let pd = paths.projects_dir.clone();
        let sid = session_id.clone();
        let h = std::thread::spawn(move || run_received_observer(&sd, &pd, &sid, |_| true));
        // Wait for the first emit, then let several poll cycles (1000ms each) pass.
        poll_recipient_events(&paths.state_dir, &session_id, Duration::from_secs(5), |r| {
            r.iter().any(|e| e.event == "message-seen")
        });
        std::thread::sleep(Duration::from_millis(2300)); // ≥2 more poll cycles
        drop(h);
        let recs = tail_recipient_events(&paths.state_dir, &session_id);
        let n = recs.iter().filter(|e| e.event == "message-seen").count();
        assert_eq!(
            n, 1,
            "dedup: exactly one message-seen across re-scans; got {recs:?}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    // ----- I3 (latency PENDING): NOT pulled → no terminal; then pull → seen ----

    /// I3: a recipient that has NOT pulled the message (the `message_id` is absent
    /// from the transcript) yields NO `message-seen` AND NO `seen-failed` while the
    /// session is alive — the promise stays PENDING. Then the message lands and
    /// `message-seen` appears. Proves latency ≠ failure. Both halves use the real
    /// `run_received_observer`; `seen-failed` is NEVER emitted by the observer
    /// (that is session-close only — Group B item 4), so absence is structural.
    #[test]
    fn i3_latency_pending_then_seen() {
        let home = unique_home("i3");
        let paths = SbPaths::from_home(&home);
        let session_id = random_uuid_v4();
        let message_id = "relay-1781000000300-5";

        // Phase 1: an EMPTY transcript (message in intake, not yet pulled).
        write_recipient_transcript(&paths.projects_dir, &session_id, &[]);
        let sd = paths.state_dir.clone();
        let pd = paths.projects_dir.clone();
        let sid = session_id.clone();
        let h = std::thread::spawn(move || run_received_observer(&sd, &pd, &sid, |_| true));

        // Give the observer >=2 full poll cycles over the not-yet-landed transcript.
        std::thread::sleep(Duration::from_millis(2300));
        let pending = tail_recipient_events(&paths.state_dir, &session_id);
        assert!(
            pending.is_empty(),
            "I3 PENDING: no message-seen AND no seen-failed while unpulled; got {pending:?}"
        );

        // Phase 2: the recipient pulls it in (the record now APPENDS — the real
        // transcript-growth model; the observer reads the appended tail).
        append_recipient_record(
            &paths.projects_dir,
            &session_id,
            &user_record_with_channel(message_id, "delivered late, still fine"),
        );
        let recs =
            poll_recipient_events(&paths.state_dir, &session_id, Duration::from_secs(5), |r| {
                r.iter().any(|e| e.event == "message-seen")
            });
        drop(h);
        let seen: Vec<&EventRecord> = recs.iter().filter(|e| e.event == "message-seen").collect();
        assert_eq!(
            seen.len(),
            1,
            "I3: message-seen appears AFTER the pull; got {recs:?}"
        );
        assert_eq!(seen[0].send_id().as_deref(), Some(message_id));
        assert!(
            !recs.iter().any(|e| e.event == "seen-failed"),
            "I3: latency NEVER emits seen-failed"
        );
        dump_proof(&paths.state_dir, &session_id, "I3-latency-pending.jsonl");
        let _ = std::fs::remove_dir_all(&home);
    }

    // ----- I4 (seen-failed): session close with the id still unpulled ----------

    /// Build a bare server whose state TRACKS a received (non-reply) message_id, and
    /// whose recipient transcript does / does not contain it, to drive
    /// `emit_seen_failed_for_unpulled` (the real session-close bookend).
    fn server_tracking(home: &Path, message_id: &str) -> Arc<RelayServer> {
        let server = bare_server(home);
        {
            let mut st = server.state.lock().unwrap();
            // record_origin with is_reply=false → tracked_message_ids includes it.
            st.record_origin(message_id.to_string(), "sess-A".to_string(), false);
        }
        server
    }

    /// I4: the recipient session CLOSES with a tracked `message_id` still absent
    /// from the transcript → the REAL `emit_seen_failed_for_unpulled` writes exactly
    /// one `seen-failed{reason="recipient-gone"}` terminal, keyed by send_id, into
    /// the recipient log. Tailed from disk.
    #[test]
    fn i4_seen_failed_on_close_when_unpulled() {
        let home = unique_home("i4");
        let message_id = "relay-1781000000400-9";
        let server = server_tracking(&home, message_id);
        // Recipient transcript exists but does NOT contain the message_id (unpulled).
        write_recipient_transcript(
            &server.paths.projects_dir,
            &server.session_id,
            &[user_record_with_channel(
                "relay-OTHER-1",
                "unrelated chatter",
            )],
        );

        // The real session-close bookend (run() calls this just before cleanup).
        emit_seen_failed_for_unpulled(&server);

        let recs = tail_recipient_events(&server.paths.state_dir, &server.session_id);
        let failed: Vec<&EventRecord> = recs.iter().filter(|e| e.event == "seen-failed").collect();
        assert_eq!(failed.len(), 1, "exactly one seen-failed; got {recs:?}");
        assert_eq!(
            failed[0].send_id().as_deref(),
            Some(message_id),
            "seen-failed carries the mandatory send_id (§X.3.5)"
        );
        let raw = std::fs::read_to_string(
            server
                .paths
                .state_dir
                .join("sessions")
                .join(format!("{}.events.jsonl", server.session_id)),
        )
        .unwrap();
        let line = raw.lines().find(|l| l.contains("seen-failed")).unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["event"], "seen-failed");
        assert_eq!(v["reason"], "recipient-gone");
        assert_eq!(v["v"], 1);
        // No message-seen for the same id (the two terminals never both land, §X.3.5).
        assert!(
            !recs.iter().any(|e| e.event == "message-seen"),
            "I4: no message-seen for the unpulled id"
        );
        dump_proof(
            &server.paths.state_dir,
            &server.session_id,
            "I4-seen-failed.jsonl",
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// I4 race-guard / I3-vs-I4 discrimination: if the tracked message DID land in
    /// the transcript by close-time, the final-scan race guard SUPPRESSES
    /// `seen-failed` (a landed id was/will be a message-seen, never recipient-gone).
    /// This is the structural proof that I4 does NOT fire merely from I3 latency that
    /// later resolved.
    #[test]
    fn i4_seen_failed_suppressed_when_id_landed_before_close() {
        let home = unique_home("i4-guard");
        let message_id = "relay-1781000000500-1";
        let server = server_tracking(&home, message_id);
        // The id DID land before close (the recipient pulled it).
        write_recipient_transcript(
            &server.paths.projects_dir,
            &server.session_id,
            &[user_record_with_channel(message_id, "pulled before close")],
        );
        emit_seen_failed_for_unpulled(&server);
        let recs = tail_recipient_events(&server.paths.state_dir, &server.session_id);
        assert!(
            !recs.iter().any(|e| e.event == "seen-failed"),
            "race guard: a landed id must NOT produce seen-failed; got {recs:?}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// I4 scope guard: a co-homed session's pending message (NOT tracked by THIS
    /// session's origins) must NOT be failed — `tracked_message_ids` scopes
    /// `seen-failed` to this session's own received ids (else first-terminal-wins
    /// would wrong-fire on another session's still-pending send).
    #[test]
    fn i4_seen_failed_scoped_to_this_sessions_tracked_ids() {
        let home = unique_home("i4-scope");
        // This server tracks NOTHING (no record_origin) → nothing to fail.
        let server = bare_server(&home);
        write_recipient_transcript(&server.paths.projects_dir, &server.session_id, &[]);
        emit_seen_failed_for_unpulled(&server);
        let recs = tail_recipient_events(&server.paths.state_dir, &server.session_id);
        assert!(
            recs.is_empty(),
            "scope guard: no tracked ids → no seen-failed at all; got {recs:?}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
