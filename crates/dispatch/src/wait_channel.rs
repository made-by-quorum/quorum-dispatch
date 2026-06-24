//! Live `sb wait` republish subscriber (WP-B2b-2b, deliverables 2+3) — the REAL
//! socket subscriber that backs BOTH B3 channel seams ([`ChannelStatusSource`] +
//! [`ChannelTurnSource`]) off ONE connection, so "channel-down" is a SINGLE shared
//! truth (the §6.0 invariant the §H.2 purity test pins).
//!
//! `sb wait` is a CLIENT process: it connects to the session's sbmux daemon socket,
//! does the v3 handshake, sends [`ClientMsg::SubscribeRepublish`], and a background
//! thread drains `ServerMsg::Republish*` frames into shared state. The two B3 seams
//! read that state (non-blocking); the disk reads (`pid.json` + the transcript
//! barrier) survive ONLY as the channel-down fallback.
//!
//! CHANNEL-DOWN = `connect_ok == false` = "we never received a single `Republish*`
//! frame" (no socket / no daemon / the session is NOT a headless daemon session /
//! the daemon refused with an `Error`). That is the COMMON case for an ordinary
//! claude session today (no headless republish stream), and it degrades to EXACTLY
//! today's disk-keyed wait — the live channel takes over only for a real headless
//! session (proven by the integration test).

use crate::wait::{
    ChannelStatus, ChannelStatusObservation, ChannelStatusSource, ChannelTurnObservation,
    ChannelTurnSource,
};
use sbmux::protocol::{encode, ClientMsg, FrameReader, ServerMsg};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Bound on the connect + v3 handshake + subscribe send. A socket that exists but
/// whose daemon is unresponsive must not hang `sb wait`; on timeout the channel is
/// simply Down (disk fallback).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Shared state the reader thread updates and the two seam handles read.
#[derive(Debug)]
struct SubState {
    /// True once ≥1 `Republish*` frame has arrived — the single channel-up truth.
    /// Until then (and forever if the daemon refuses / the session is not
    /// headless) BOTH seams report Down → the disk fallback answers.
    connect_ok: bool,
    /// Last known control status (defaults Busy: `sb wait` is only called on a
    /// non-idle session, so a freshly-subscribed turn is presumed in flight).
    status: ChannelStatus,
    /// The expected turn's `result` (TurnEnd) was republished, OR a clean EOF
    /// (`RepublishEnd{"Completed"}`) closed the stream — the turn completed.
    result_seen: bool,
    /// WP-B-CS-2 ADDITIVE: the observe-dashboard control-fact snapshot, folded from
    /// the SAME `Republish*` frames. Purely additive — it does not feed
    /// `connect_ok`/`status`/`result_seen` or the wait seams; it only carries the
    /// richer STATE (turn count, last result, in-turn) the read-only dashboard
    /// renders. CONTROL FACTS ONLY (§2a): `DashboardState` has no assistant-text
    /// path.
    dashboard: crate::observe::DashboardState,
    /// WP-B-CS-2-LIVE: the observe run-loop's per-frame TAP — `Some` only when the
    /// subscriber was built via [`ChannelSubscriber::connect_observing`]; `None` for
    /// the `sb wait` path (so wait never grows this queue: byte-unchanged behaviour).
    /// CONTROL FACTS ONLY by construction: [`apply_frame`] enqueues ONLY the four
    /// `Republish*` control facts here — content frames (ScreenUpdate/History/
    /// Passthrough/Error) are NEVER tapped, so the run-loop's cutover gate is fed
    /// control facts only. The run-loop drains it each tick, keeping it bounded.
    tap: Option<VecDeque<ServerMsg>>,
}

impl SubState {
    fn new() -> Self {
        Self {
            connect_ok: false,
            status: ChannelStatus::Busy,
            result_seen: false,
            dashboard: crate::observe::DashboardState::default(),
            tap: None,
        }
    }

    /// The observe-path variant: identical to [`Self::new`] but with the per-frame
    /// control-fact TAP armed (`sb connect` observe run-loop only).
    fn new_observing() -> Self {
        Self {
            tap: Some(VecDeque::new()),
            ..Self::new()
        }
    }
}

/// Is `msg` one of the four `Republish*` CONTROL FACTS? Only these are tapped for
/// the observe run-loop's cutover gate — every content-bearing frame is excluded
/// here, so the control-facts-only hard line holds at the tap boundary by
/// construction.
fn is_republish_control(msg: &ServerMsg) -> bool {
    matches!(
        msg,
        ServerMsg::RepublishReady { .. }
            | ServerMsg::RepublishTurnEnd { .. }
            | ServerMsg::RepublishStatus { .. }
            | ServerMsg::RepublishEnd { .. }
    )
}

/// A live republish subscriber. Owns the background reader thread + the shared
/// state; drop signals the thread to stop and joins it. Hand the two seam handles
/// ([`Self::status_source`] / [`Self::turn_source`]) to the wait deps; keep THIS
/// alive for the loop's duration.
pub struct ChannelSubscriber {
    state: Arc<Mutex<SubState>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ChannelSubscriber {
    /// Connect to `socket_path` and subscribe to `expected_session`'s republish
    /// stream on a background thread. Returns immediately; the channel reports Down
    /// until the first `Republish*` frame lands (so early polls fall back to disk —
    /// pure when the channel is genuinely carrying the signal).
    pub fn connect(socket_path: PathBuf, expected_session: String) -> Self {
        Self::connect_with_state(socket_path, expected_session, SubState::new())
    }

    /// WP-B-CS-2-LIVE — the OBSERVE-path subscriber: identical to [`Self::connect`]
    /// but with the per-frame control-fact TAP armed, so the `sb connect` observe
    /// run-loop can drain the live `Republish*` stream into its cutover gate
    /// (`gate.observe(frame)` per frame). `sb wait` keeps using [`Self::connect`]
    /// (tap disabled) — byte-unchanged.
    pub fn connect_observing(socket_path: PathBuf, expected_session: String) -> Self {
        Self::connect_with_state(socket_path, expected_session, SubState::new_observing())
    }

    fn connect_with_state(socket_path: PathBuf, expected_session: String, init: SubState) -> Self {
        let state = Arc::new(Mutex::new(init));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = state.clone();
        let thread_stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name("sb-wait-republish".into())
            .spawn(move || {
                // A dedicated current-thread runtime isolates the async socket I/O
                // from the synchronous wait loop (which blocks on RealSleeper).
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => return, // no runtime → channel stays Down (disk fallback)
                };
                rt.block_on(reader_loop(
                    socket_path,
                    expected_session,
                    thread_state,
                    thread_stop,
                ));
            })
            .ok();
        Self {
            state,
            stop,
            handle,
        }
    }

    /// Block up to `timeout` for the channel to reach a DECISION — used by the
    /// entry-idle gate (RESIDUAL #1) so a real headless turn's control status is
    /// observed VIA THE CHANNEL rather than racing to a premature disk fallback.
    ///
    /// Returns:
    /// - [`ChannelStatusObservation::Live`] the moment `connect_ok` flips true (the
    ///   first `Republish*` frame landed) — the live control status.
    /// - [`ChannelStatusObservation::Down`] as soon as the reader thread FINISHES
    ///   without ever connecting (no socket / daemon refused with `Error` / EOF) —
    ///   so a non-headless session (thread finishes fast) returns Down promptly,
    ///   adding no latency; or when `timeout` elapses first (a present-but-wedged
    ///   daemon). Down ⇒ the disk fallback answers (sanctioned channel-down mode).
    pub fn await_status(&self, timeout: Duration) -> ChannelStatusObservation {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(live) = self.live_status() {
                return live;
            }
            // Reader finished without connecting ⇒ definitively Down (re-check state
            // first: it may have flipped connect_ok on a terminal frame just before
            // finishing).
            let finished = self
                .handle
                .as_ref()
                .map(|h| h.is_finished())
                .unwrap_or(true);
            if finished {
                return self.live_status().unwrap_or(ChannelStatusObservation::Down);
            }
            if Instant::now() >= deadline {
                return ChannelStatusObservation::Down;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// `Some(Live(status))` iff the channel is up (`connect_ok`); `None` otherwise
    /// (so the caller can distinguish "up" from "not yet / never").
    fn live_status(&self) -> Option<ChannelStatusObservation> {
        let st = self.state.lock().expect("wait subscriber state poisoned");
        st.connect_ok
            .then_some(ChannelStatusObservation::Live(st.status))
    }

    /// The channel-status seam (R1-a).
    pub fn status_source(&self) -> Box<dyn ChannelStatusSource> {
        Box::new(SeamHandle {
            state: self.state.clone(),
        })
    }

    /// The turn-completion seam (item 1) — the SAME shared state, so channel-down
    /// is one truth across both seams.
    pub fn turn_source(&self) -> Box<dyn ChannelTurnSource> {
        Box::new(SeamHandle {
            state: self.state.clone(),
        })
    }

    /// WP-B-CS-2 ADDITIVE observation accessor — a SNAPSHOT of the observe
    /// dashboard's latest control facts (folded from the same `Republish*` stream
    /// the wait seams read). The read-only `sb connect` OBSERVE dashboard renders
    /// this. Purely additive: it neither changes nor depends on the wait seams'
    /// `connect_ok`/`status`/`result_seen` semantics. CONTROL FACTS ONLY (§2a) —
    /// the snapshot has no assistant-text field by construction.
    pub fn dashboard_snapshot(&self) -> crate::observe::DashboardState {
        self.state
            .lock()
            .expect("wait subscriber state poisoned")
            .dashboard
            .clone()
    }

    /// WP-B-CS-2-LIVE — drain the per-frame control-fact TAP (observe path only):
    /// every `Republish*` control fact that has landed since the last drain, in
    /// arrival order, for the run-loop to fold into its cutover gate. Returns empty
    /// when the tap is disabled (`sb wait` path) or no new frame has arrived.
    /// CONTROL FACTS ONLY: the tap never carries a content frame (see
    /// [`is_republish_control`]).
    pub fn drain_control_frames(&self) -> Vec<ServerMsg> {
        let mut st = self.state.lock().expect("wait subscriber state poisoned");
        match st.tap.as_mut() {
            Some(q) => q.drain(..).collect(),
            None => Vec::new(),
        }
    }
}

impl Drop for ChannelSubscriber {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// One handle onto the shared state, implementing BOTH seam traits (each `Box<dyn
/// ..>` the deps hold is one of these over a clone of the same `Arc`).
struct SeamHandle {
    state: Arc<Mutex<SubState>>,
}

impl ChannelStatusSource for SeamHandle {
    fn poll_status(&self) -> ChannelStatusObservation {
        let st = self.state.lock().expect("wait subscriber state poisoned");
        if !st.connect_ok {
            return ChannelStatusObservation::Down;
        }
        ChannelStatusObservation::Live(st.status)
    }
}

impl ChannelTurnSource for SeamHandle {
    fn poll_result(&self) -> ChannelTurnObservation {
        let st = self.state.lock().expect("wait subscriber state poisoned");
        if !st.connect_ok {
            return ChannelTurnObservation::Down;
        }
        if st.result_seen {
            ChannelTurnObservation::Seen
        } else {
            ChannelTurnObservation::NotYet
        }
    }
}

/// Connect, handshake, subscribe, and drain `Republish*` frames into the shared
/// state until EOF / a terminal `RepublishEnd` / a stop signal. Any failure leaves
/// `connect_ok == false` (channel Down → disk fallback).
async fn reader_loop(
    socket_path: PathBuf,
    expected_session: String,
    state: Arc<Mutex<SubState>>,
    stop: Arc<AtomicBool>,
) {
    // Connect + handshake + subscribe, all bounded. On any failure the channel
    // simply stays Down (disk fallback) — never an error surfaced to the wait.
    match tokio::time::timeout(
        CONNECT_TIMEOUT,
        connect_and_subscribe(&socket_path, &expected_session),
    )
    .await
    {
        Ok(Some((stream, fr))) => drain_frames(stream, fr, &state, &stop).await,
        _ => {} // connect/handshake/subscribe failed or timed out → Down
    }
}

/// Connect, do the v3 handshake (verifying the session identity belt), and send
/// `SubscribeRepublish`. Returns the stream + the `FrameReader` (which may already
/// hold pipelined bytes) on success, `None` on any failure.
async fn connect_and_subscribe(
    socket_path: &PathBuf,
    expected_session: &str,
) -> Option<(tokio::net::UnixStream, FrameReader)> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    let mut s = UnixStream::connect(socket_path).await.ok()?;
    sbmux::protocol::write_preamble(&mut s).await.ok()?;
    let hello = encode(&ClientMsg::Hello { caps: vec![] }).ok()?;
    s.write_all(&hello).await.ok()?;
    // Read ServerHello; verify the session identity belt.
    let mut fr = FrameReader::new();
    loop {
        if !fr.fill_from(&mut s).await.ok()? {
            return None; // EOF before ServerHello
        }
        if let Some(msg) = fr.decode_next::<ServerMsg>().ok()? {
            match msg {
                ServerMsg::Hello { session, .. } => {
                    // Identity belt: a mismatched daemon → treat as Down.
                    if !session.is_empty() && session != expected_session {
                        return None;
                    }
                    break;
                }
                _ => return None, // protocol violation → Down
            }
        }
    }
    let sub = encode(&ClientMsg::SubscribeRepublish {
        name: expected_session.to_string(),
    })
    .ok()?;
    s.write_all(&sub).await.ok()?;
    Some((s, fr))
}

/// Drain `Republish*` frames into the shared state. The FIRST `Republish*` frame
/// flips `connect_ok` true (channel up); a non-republish frame (e.g. an `Error`
/// "no headless session") leaves it false (Down → disk fallback).
async fn drain_frames(
    mut stream: tokio::net::UnixStream,
    mut fr: FrameReader,
    state: &Arc<Mutex<SubState>>,
    stop: &Arc<AtomicBool>,
) {
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        // Decode anything already buffered first.
        loop {
            match fr.decode_next::<ServerMsg>() {
                Ok(Some(msg)) => {
                    if apply_frame(state, msg) {
                        return; // terminal RepublishEnd
                    }
                }
                Ok(None) => break,
                Err(_) => return, // decode error → stop (channel state frozen)
            }
        }
        // Then block (briefly) for more bytes, re-checking the stop flag.
        match tokio::time::timeout(Duration::from_millis(200), fr.fill_from(&mut stream)).await {
            Ok(Ok(true)) => {}       // got bytes → loop to decode
            Ok(Ok(false)) => return, // EOF
            Ok(Err(_)) => return,    // read error
            Err(_) => {}             // timeout → re-check stop, keep waiting
        }
    }
}

/// Apply one frame to the shared state. Returns true if it was the terminal
/// `RepublishEnd` (the reader should stop). Only `Republish*` frames flip
/// `connect_ok` — an `Error`/other frame leaves the channel Down.
fn apply_frame(state: &Arc<Mutex<SubState>>, msg: ServerMsg) -> bool {
    let mut st = state.lock().expect("wait subscriber state poisoned");
    // WP-B-CS-2 ADDITIVE: fold the frame into the observe-dashboard snapshot
    // FIRST (a read-only fold; control facts only). This is purely additive — the
    // existing wait seam logic below is byte-unchanged, so `await_status`/
    // `poll_status`/`poll_result` semantics are untouched.
    st.dashboard.apply(&msg);
    // WP-B-CS-2-LIVE: additively TAP the four control facts for the observe
    // run-loop's cutover gate (armed only on the observe path). §2a-safe: a content
    // frame is never enqueued. The wait-seam fold below stays byte-unchanged.
    if is_republish_control(&msg) {
        if let Some(q) = st.tap.as_mut() {
            q.push_back(msg.clone());
        }
    }
    match msg {
        ServerMsg::RepublishReady { .. } => {
            st.connect_ok = true;
            st.status = ChannelStatus::Busy;
            false
        }
        ServerMsg::RepublishTurnEnd { is_error, .. } => {
            st.connect_ok = true;
            st.result_seen = true;
            // A clean turn-end → idle; an error turn-end → offline (fail-closed).
            st.status = if is_error {
                ChannelStatus::Offline
            } else {
                ChannelStatus::Idle
            };
            false
        }
        ServerMsg::RepublishStatus { status } => {
            st.connect_ok = true;
            st.status = match status.as_str() {
                "busy" => ChannelStatus::Busy,
                "idle" => ChannelStatus::Idle,
                _ => ChannelStatus::Offline,
            };
            false
        }
        ServerMsg::RepublishEnd { outcome } => {
            st.connect_ok = true;
            // A clean EOF (`Completed`) closes the stream on a finished turn — even
            // if we subscribed too late to see the TurnEnd, this proves completion
            // (idle + result-seen). Any other outcome (Aborted/NoTurn/breaker/
            // lagged) is fail-closed offline.
            if outcome == "Completed" {
                st.result_seen = true;
                st.status = ChannelStatus::Idle;
            } else {
                st.status = ChannelStatus::Offline;
            }
            true // terminal
        }
        // Any non-republish frame (e.g. Error "no headless session 'x'"): NOT a
        // channel-up signal — leave connect_ok false so both seams report Down and
        // the disk fallback answers (today's behavior for a non-headless session).
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wait::{ChannelTurnObservation, ChannelTurnSource};

    fn st() -> Arc<Mutex<SubState>> {
        Arc::new(Mutex::new(SubState::new()))
    }
    fn snap(s: &Arc<Mutex<SubState>>) -> (bool, ChannelStatus, bool) {
        let g = s.lock().unwrap();
        (g.connect_ok, g.status, g.result_seen)
    }
    fn seam(s: &Arc<Mutex<SubState>>) -> SeamHandle {
        SeamHandle { state: s.clone() }
    }

    /// WP-B-CS-2 — the dashboard snapshot is fed THROUGH the same `apply_frame`
    /// seam, ADDITIVELY: feeding synthetic `Republish*` frames builds the
    /// dashboard STATE while the existing wait-seam fields stay EXACTLY as the
    /// pre-dashboard fold left them. Pins that the additive fold neither changes
    /// nor is changed by `connect_ok`/`status`/`result_seen`.
    #[test]
    fn dashboard_is_fed_through_apply_frame_additively() {
        let s = st();
        apply_frame(
            &s,
            ServerMsg::RepublishReady {
                session_id: "d".into(),
            },
        );
        apply_frame(
            &s,
            ServerMsg::RepublishTurnEnd {
                session_id: "d".into(),
                is_error: false,
                stop_reason: Some("end_turn".into()),
            },
        );
        let g = s.lock().unwrap();
        // Existing wait seam semantics — UNCHANGED by the additive dashboard fold.
        assert_eq!(
            (g.connect_ok, g.status, g.result_seen),
            (true, ChannelStatus::Idle, true)
        );
        // The dashboard snapshot carries the richer control facts.
        assert_eq!(g.dashboard.turn_count, 1);
        assert_eq!(
            g.dashboard.last_result,
            Some(crate::observe::TurnResult::Ok)
        );
        assert_eq!(g.dashboard.session_id.as_deref(), Some("d"));
        // §2a: a content-bearing frame moves neither the seams nor the dashboard.
        drop(g);
        let before = s.lock().unwrap().dashboard.clone();
        apply_frame(&s, ServerMsg::ScreenUpdate(b"ASSISTANT TEXT".to_vec()));
        assert_eq!(
            s.lock().unwrap().dashboard,
            before,
            "content frame is inert"
        );
    }

    /// WP-B-CS-2-LIVE — the observe-path control-fact TAP collects ONLY the four
    /// `Republish*` frames (§2a-safe at the tap boundary), in arrival order, and the
    /// wait-path state (tap disabled) collects NOTHING (byte-unchanged). The
    /// fix-shaped mutation (drop the `is_republish_control` guard so content frames
    /// enter the tap) would leak a content frame into the run-loop and red this.
    #[test]
    fn observe_tap_collects_control_facts_only() {
        // Observe path: tap armed.
        let s = Arc::new(Mutex::new(SubState::new_observing()));
        apply_frame(
            &s,
            ServerMsg::RepublishReady {
                session_id: "d".into(),
            },
        );
        // A content frame must NOT enter the tap.
        apply_frame(&s, ServerMsg::ScreenUpdate(b"ASSISTANT TEXT".to_vec()));
        apply_frame(
            &s,
            ServerMsg::RepublishStatus {
                status: "busy".into(),
            },
        );
        apply_frame(&s, ServerMsg::History(vec![b"more text".to_vec()]));
        apply_frame(
            &s,
            ServerMsg::RepublishTurnEnd {
                session_id: "d".into(),
                is_error: false,
                stop_reason: Some("end_turn".into()),
            },
        );
        let tapped: Vec<ServerMsg> = s.lock().unwrap().tap.as_mut().unwrap().drain(..).collect();
        assert_eq!(
            tapped.len(),
            3,
            "only the 3 control facts are tapped: {tapped:?}"
        );
        assert!(
            tapped.iter().all(super::is_republish_control),
            "the tap must carry control facts only (no content): {tapped:?}"
        );

        // Wait path: tap disabled → nothing collected, regardless of frames.
        let w = st(); // SubState::new() → tap None
        apply_frame(
            &w,
            ServerMsg::RepublishReady {
                session_id: "d".into(),
            },
        );
        assert!(w.lock().unwrap().tap.is_none(), "wait path arms no tap");
    }

    // --- frame → state mapping (apply_frame) -------------------------------

    #[test]
    fn ready_sets_busy_and_connects() {
        let s = st();
        let terminal = apply_frame(
            &s,
            ServerMsg::RepublishReady {
                session_id: "x".into(),
            },
        );
        assert!(!terminal, "Ready is not terminal");
        assert_eq!(snap(&s), (true, ChannelStatus::Busy, false));
        // Seam reflects it: status Live(Busy), turn NotYet.
        assert_eq!(
            seam(&s).poll_status(),
            ChannelStatusObservation::Live(ChannelStatus::Busy)
        );
        assert_eq!(seam(&s).poll_result(), ChannelTurnObservation::NotYet);
    }

    #[test]
    fn turn_end_clean_is_idle_and_result_seen() {
        let s = st();
        let terminal = apply_frame(
            &s,
            ServerMsg::RepublishTurnEnd {
                session_id: "x".into(),
                is_error: false,
                stop_reason: Some("end_turn".into()),
            },
        );
        assert!(!terminal, "TurnEnd is not terminal (the stream stays open)");
        assert_eq!(snap(&s), (true, ChannelStatus::Idle, true));
        assert_eq!(seam(&s).poll_result(), ChannelTurnObservation::Seen);
    }

    #[test]
    fn turn_end_error_is_offline_fail_closed() {
        let s = st();
        apply_frame(
            &s,
            ServerMsg::RepublishTurnEnd {
                session_id: "x".into(),
                is_error: true,
                stop_reason: None,
            },
        );
        // is_error ⇒ Offline (fail-closed), but the result WAS seen.
        assert_eq!(snap(&s), (true, ChannelStatus::Offline, true));
        assert_eq!(
            seam(&s).poll_status(),
            ChannelStatusObservation::Live(ChannelStatus::Offline)
        );
    }

    #[test]
    fn status_frame_maps_busy_idle_and_other_offline() {
        for (raw, want) in [
            ("busy", ChannelStatus::Busy),
            ("idle", ChannelStatus::Idle),
            ("weird", ChannelStatus::Offline),
        ] {
            let s = st();
            apply_frame(&s, ServerMsg::RepublishStatus { status: raw.into() });
            assert_eq!(snap(&s).1, want, "status {raw:?}");
            assert!(snap(&s).0, "any Republish* frame connects the channel");
        }
    }

    #[test]
    fn end_completed_is_terminal_idle_result_seen() {
        // Subscribe-after-TurnEnd: a clean EOF proves completion even if the TurnEnd
        // frame was missed.
        let s = st();
        let terminal = apply_frame(
            &s,
            ServerMsg::RepublishEnd {
                outcome: "Completed".into(),
            },
        );
        assert!(terminal, "RepublishEnd is terminal");
        assert_eq!(snap(&s), (true, ChannelStatus::Idle, true));
        assert_eq!(seam(&s).poll_result(), ChannelTurnObservation::Seen);
    }

    #[test]
    fn end_aborted_is_terminal_offline_no_result() {
        let s = st();
        let terminal = apply_frame(
            &s,
            ServerMsg::RepublishEnd {
                outcome: "Aborted".into(),
            },
        );
        assert!(terminal);
        // Offline, result NOT seen (fail-closed).
        assert_eq!(snap(&s), (true, ChannelStatus::Offline, false));
    }

    #[test]
    fn non_republish_frame_leaves_channel_down() {
        // An Error reply ("no headless session 'x'") is NOT a channel-up signal:
        // connect_ok stays false ⇒ both seams report Down ⇒ disk fallback answers.
        let s = st();
        let terminal = apply_frame(&s, ServerMsg::Error("no headless session 'x'".into()));
        assert!(terminal, "a non-republish frame stops the reader");
        assert_eq!(snap(&s).0, false, "connect_ok stays false");
        assert_eq!(seam(&s).poll_status(), ChannelStatusObservation::Down);
        assert_eq!(seam(&s).poll_result(), ChannelTurnObservation::Down);
    }

    #[test]
    fn seams_report_down_before_any_frame() {
        // Fresh state (no frame yet) ⇒ Down on both seams (the connect race window:
        // the disk fallback answers until the channel is proven up).
        let s = st();
        assert_eq!(seam(&s).poll_status(), ChannelStatusObservation::Down);
        assert_eq!(seam(&s).poll_result(), ChannelTurnObservation::Down);
    }

    // --- await_status: absent socket decides Down fast (no hang) -------------

    #[test]
    fn await_status_down_when_socket_absent() {
        // No daemon socket ⇒ the reader thread connect-fails and FINISHES; await_status
        // returns Down promptly (well within the timeout — proving the is_finished
        // short-circuit, not a full-timeout wait).
        let sub = ChannelSubscriber::connect(
            PathBuf::from("/nonexistent/sb-wait-test/missing.sock"),
            "ghost".to_string(),
        );
        let t = Instant::now();
        let obs = sub.await_status(Duration::from_secs(5));
        assert_eq!(obs, ChannelStatusObservation::Down);
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "absent socket must decide Down fast (finished-thread short-circuit), took {:?}",
            t.elapsed()
        );
    }
}
