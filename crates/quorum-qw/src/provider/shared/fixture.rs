//! [`FixtureDaemonProvider`] (codex-p1-spec section 6.1) — the R3 second impl.
//!
//! A COMPILED-IN fixture (the [`crate::mux::FixtureMux`] precedent — NOT
//! `#[cfg(test)]`, so the build itself enforces the trait fits a non-claude
//! shape), deliberately NON-claude on EVERY concern. It is codex-daemon-SHAPED
//! (thread-id identity, daemon hosting, notification-shaped status, date-keyed
//! transcripts, turn/start inject) but carries ZERO codex code — the shapes are
//! invented minimal fixtures, not a port of any codex client.
//!
//! THE TEETH (R3): if hosting this impl forces a claude-shaped contortion in the
//! trait — a pid requirement, a cwd-slug transcript key, a shared status parser,
//! a port/sidecar in `inject` — the TRAIT is wrong and gets fixed, not the
//! fixture (codex-p1-spec section 6.2, binding). Every "deliberately non-claude"
//! choice below is a standing answer to the ADD-8 "the trait only fits claude"
//! refutation.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::boot::BootFailure;
use crate::create::BootWaiter;
use crate::jsonl::{JsonlStats, TranscriptMeta};
use crate::model::{SessionStatus, TurnPreview};
use crate::provider::{
    Hosting, InjectError, LaunchPlan, LaunchRequest, Provider, ProviderFx, SessionKey,
};

/// A daemon-shaped fixture provider (codex-p1-spec section 6.1).
///
/// Holds internal fixture state for the boot-readiness record + the inject turn
/// queue (a daemon owns turns server-side; claude owns nothing here). The state
/// is `RefCell`-wrapped so `&self` trait methods can mutate it — the conformance
/// suite drives inject/steer against ONE provider value.
///
/// `transcript_root` (codex-p2-spec section 6.4) is a CONSTRUCTOR-HELD root — the
/// cleanest non-claude shape: it is provider-OWNED state, NOT derived from
/// `fx.paths` (the claude shape) and NOT derived from `fx.env` (the codex shape).
/// `Default`/`ready()` use a fixed sentinel; [`FixtureDaemonProvider::with_root`]
/// pins it to a seeded TempDir for the conformance run. This proves the trait's
/// `transcript_root(&ProviderFx)` does NOT force a claude-shaped `fx.paths` read.
pub struct FixtureDaemonProvider {
    state: RefCell<DaemonState>,
    /// The provider-owned transcript root (constructor-held; `fx`-independent).
    root: PathBuf,
}

impl Default for FixtureDaemonProvider {
    fn default() -> Self {
        Self {
            state: RefCell::new(DaemonState::default()),
            // A fixed sentinel root — the fixture's own state, not from `fx`.
            root: PathBuf::from("/fixture-daemon/rollouts"),
        }
    }
}

/// The fixture's in-memory daemon state — readiness handshake + the turn queue.
#[derive(Default)]
struct DaemonState {
    /// initialize-response-shaped readiness record (boot): set once the daemon
    /// "handshake" lands. The fixture marks it ready at construction-via-handshake
    /// so the conformance boot drive succeeds; a test can clear it to model a
    /// daemon that never handshakes.
    handshake_ready: bool,
    /// The next turn id to mint (turn/start-shaped enqueue returns it).
    next_turn: u64,
    /// The id of the currently-active turn, if any (a steer targets it with an
    /// expected-turn-id precondition; a stale id → typed InjectError).
    active_turn: Option<u64>,
}

impl FixtureDaemonProvider {
    /// A fresh daemon fixture whose handshake has ALREADY landed (boot ready).
    /// This is the conformance default — boot drive reaches readiness. Carries
    /// the fixed sentinel `transcript_root`.
    pub fn ready() -> Self {
        Self {
            state: RefCell::new(DaemonState {
                handshake_ready: true,
                next_turn: 1,
                active_turn: None,
            }),
            ..Default::default()
        }
    }

    /// A ready daemon fixture whose constructor-held `transcript_root` is `root`
    /// (codex-p2-spec section 6.4 — the conformance run seeds a TempDir tree and
    /// pins that this root is returned by `transcript_root`).
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            state: RefCell::new(DaemonState {
                handshake_ready: true,
                next_turn: 1,
                active_turn: None,
            }),
            root,
        }
    }

    /// A turn id the fixture currently considers active (for a steer
    /// precondition test). None until an inject has enqueued one.
    pub fn active_turn_id(&self) -> Option<String> {
        self.state.borrow().active_turn.map(turn_id)
    }

    /// Steer an in-flight turn (codex-p1-spec section 6.1): a steer-shaped inject
    /// variant carrying an EXPECTED turn id. If it matches the active turn the
    /// steer lands (returns the same turn id); a WRONG id → a typed
    /// [`InjectError::Precondition`] (the stale-precondition tooth, expressible
    /// WITHOUT any port/sidecar concept). Distinct from the plain `inject`
    /// (turn/start enqueue) so the conformance suite can exercise both.
    pub fn steer(&self, expected_turn_id: &str, _message: &str) -> Result<String, InjectError> {
        let st = self.state.borrow();
        match st.active_turn {
            Some(active) if turn_id(active) == expected_turn_id => Ok(turn_id(active)),
            Some(active) => Err(InjectError::Precondition(format!(
                "steer expected turn {expected_turn_id} but the active turn is {}",
                turn_id(active)
            ))),
            None => Err(InjectError::Precondition(format!(
                "steer expected turn {expected_turn_id} but no turn is active"
            ))),
        }
    }
}

/// The turn id minted for a turn number (turn/start return shape).
fn turn_id(n: u64) -> String {
    format!("turn-{n:08}")
}

impl Provider for FixtureDaemonProvider {
    fn id(&self) -> &'static str {
        "fixture-daemon"
    }

    fn hosting(&self) -> Hosting {
        // Daemon-hosted: the R4 proof the seam EXPRESSES a non-mux hosting mode.
        Hosting::Daemon
    }

    /// Direct argv (`["fixtured", "app-server"]`-shaped) — NO shell assembly, NO
    /// mux, NO pid-file, and it consumes NO claude config surface (it never reads
    /// `fx.env` or any config toml). The negative control
    /// `daemon_launch_plan_minimal_fx` drives this with an empty env + nonexistent
    /// config path and it still succeeds.
    fn launch_plan(&self, _fx: &ProviderFx, req: &LaunchRequest) -> LaunchPlan {
        let mut argv = vec!["fixtured".to_string(), "app-server".to_string()];
        // A daemon launch may carry passthrough directly (no claude flag dedupe,
        // no --name shell-assembly); this is shape, not behavior.
        argv.extend(req.passthrough.iter().cloned());
        LaunchPlan {
            argv,
            // The daemon expresses its readiness env directly (initialize-shaped),
            // not via a claude F1 session-env file — shape only.
            env: vec![("FIXTURED_MODE".to_string(), "app-server".to_string())],
        }
    }

    /// Readiness = a handshake/notification record in the fixture state
    /// (initialize-response-shaped), NOT a pid-file + dialog match. Returns a
    /// waiter that consults `handshake_ready` — NO mux, NO clock, NO pid scan.
    fn boot_waiter<'a>(&self, _fx: &'a ProviderFx<'a>) -> Box<dyn BootWaiter + 'a> {
        let ready = self.state.borrow().handshake_ready;
        Box::new(HandshakeWaiter { ready })
    }

    /// Accepts ONLY notification-shaped OBJECTS
    /// `{"method":"thread/status/changed","params":{"status":...}}`:
    ///   - `status == "idle"`                              → Idle
    ///   - `status == {"active":{"activeFlags":[...]}}`    → Busy
    ///
    /// A bare registry-style STRING (claude's raw shape) returns None — the
    /// negative control proving the parsers are NOT shared. NEVER reads pid/cwd.
    fn parse_status(&self, raw: &Value) -> Option<SessionStatus> {
        // A claude registry status STRING (e.g. "idle") is NOT a notification
        // object → None (cross-feed negative control).
        let obj = raw.as_object()?;
        if obj.get("method")?.as_str()? != "thread/status/changed" {
            return None;
        }
        let status = obj.get("params")?.get("status")?;
        // "idle" → Idle.
        if status.as_str() == Some("idle") {
            return Some(SessionStatus::Idle);
        }
        // {"active":{"activeFlags":[...]}} → Busy.
        if let Some(active) = status.get("active") {
            if active
                .get("activeFlags")
                .and_then(|f| f.as_array())
                .is_some()
            {
                return Some(SessionStatus::Busy);
            }
        }
        None
    }

    /// The constructor-held root (codex-p2-spec section 6.4) — NOT `fx.paths`
    /// (claude) and NOT `fx.env` (codex). `fx` is unused: the daemon's transcript
    /// root is provider-OWNED state, which is the negative control proving the
    /// trait method does not force a claude-shaped `fx.paths.projects_dir` read.
    fn transcript_root(&self, _fx: &ProviderFx) -> PathBuf {
        self.root.clone()
    }

    /// Keyed `<root>/YYYY/MM/DD/<thread-id>.jsonl` — date + id, NO cwd-slug
    /// ANYWHERE in the key (the negative control
    /// `daemon_transcript_path_has_no_cwd_component` asserts a key WITH a cwd set
    /// produces a path that does not contain it). `key.id` IS the thread id
    /// (uuidv7-shaped by daemon contract) and is used verbatim as the filename
    /// stem so it round-trips through `scan_transcripts`. `key.pid` is NEVER read.
    /// The fixture invents a fixed date bucket so a test can assert the literal
    /// path — a real daemon would key on the rollout's creation date.
    fn transcript_path(&self, state_root: &Path, key: &SessionKey) -> Option<PathBuf> {
        // Fixed date bucket (shape only): the rollout's date, invented stable.
        Some(
            state_root
                .join("2026")
                .join("06")
                .join("06")
                .join(format!("{}.jsonl", key.id)),
        )
    }

    /// Scan `<root>/YYYY/MM/DD/*.jsonl` — date-tree, not the claude cwd-slug
    /// project dirs. Shape-minimal: walks two levels of date dirs then collects
    /// `*.jsonl`. Permissive (L8): missing dirs contribute nothing.
    fn scan_transcripts(&self, state_root: &Path) -> Vec<TranscriptMeta> {
        let mut out = Vec::new();
        // <root>/YYYY/MM/DD/<thread>.jsonl — three nested date levels.
        let Ok(years) = std::fs::read_dir(state_root) else {
            return out;
        };
        for y in years.flatten() {
            let Ok(months) = std::fs::read_dir(y.path()) else {
                continue;
            };
            for mo in months.flatten() {
                let Ok(days) = std::fs::read_dir(mo.path()) else {
                    continue;
                };
                for d in days.flatten() {
                    let day_path = d.path();
                    let bucket = format!(
                        "{}/{}/{}",
                        y.file_name().to_string_lossy(),
                        mo.file_name().to_string_lossy(),
                        d.file_name().to_string_lossy()
                    );
                    let Ok(files) = std::fs::read_dir(&day_path) else {
                        continue;
                    };
                    for f in files.flatten() {
                        let fname = f.file_name().to_string_lossy().into_owned();
                        if !fname.ends_with(".jsonl") {
                            continue;
                        }
                        let thread = fname.trim_end_matches(".jsonl").to_string();
                        let mtime_ms = f
                            .metadata()
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|dd| dd.as_millis() as i64)
                            .unwrap_or(0);
                        out.push(TranscriptMeta {
                            session_id: thread,
                            path: f.path(),
                            mtime_ms,
                            // The "project dir" slot carries the DATE bucket
                            // (date-keyed), never a cwd slug — the shape contrast.
                            project_dir: bucket.clone(),
                        });
                    }
                }
            }
        }
        out
    }

    /// Parse a rollout-SHAPED transcript: a session-meta first line
    /// (`{"type":"session-meta","title":...}`) + turn events
    /// (`{"type":"turn","role":...,"text":...}`). NOT claude JSONL — invented
    /// minimal shape. Permissive (L8): a bad line is skipped.
    fn transcript_stats(&self, path: &Path, include_preview: bool) -> JsonlStats {
        let mut stats = JsonlStats::default();
        let Ok(content) = std::fs::read_to_string(path) else {
            return stats;
        };
        let mut previews: Vec<TurnPreview> = Vec::new();
        // Occupancy (Pete #5): a turn record may carry an explicit `tokens` field
        // (the current context fill); last-wins, mirroring the real readers which
        // take the last turn's window fill. No `tokens` field anywhere → 0.
        let mut last_occupancy: Option<u64> = None;
        for line in content.split('\n') {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(obj) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match obj.get("type").and_then(|t| t.as_str()) {
                Some("session-meta") => {
                    if let Some(title) = obj.get("title").and_then(|t| t.as_str()) {
                        if !title.is_empty() {
                            stats.name = Some(title.to_string());
                            stats.user_named = true;
                        }
                    }
                }
                Some("turn") => {
                    stats.turns += 1;
                    if let Some(ts) = obj.get("timestamp").and_then(|t| t.as_str()) {
                        if !ts.is_empty() {
                            stats.last_timestamp = Some(ts.to_string());
                        }
                    }
                    if let Some(tok) = obj.get("tokens").and_then(|t| t.as_u64()) {
                        last_occupancy = Some(tok);
                    }
                    if include_preview {
                        if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
                            let role = match obj.get("role").and_then(|r| r.as_str()) {
                                Some("assistant") => "assistant",
                                _ => "user",
                            };
                            previews.push(TurnPreview {
                                role,
                                text: text.chars().take(200).collect(),
                                timestamp: obj
                                    .get("timestamp")
                                    .and_then(|t| t.as_str())
                                    .map(str::to_string),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        if include_preview {
            let n = previews.len();
            stats.last_turns = Some(previews.split_off(n.saturating_sub(6)));
        }
        // Tokens = current context occupancy (Pete #5): the last turn's explicit
        // `tokens` field, mirroring the real readers' window-fill semantic.
        stats.tokens = last_occupancy.unwrap_or(0);
        stats
    }

    /// `["resume", <thread-id>]` — SAME id across resumes (a daemon appends to
    /// the same thread), NO `--fork-session` shape. The `fork` arg is IGNORED:
    /// a daemon thread has no fork-session concept (the contrast with claude's
    /// `--fork-session`). pid is NEVER read.
    fn resume_args(&self, key: &SessionKey, _fork: bool) -> Vec<String> {
        vec!["resume".to_string(), key.id.to_string()]
    }

    /// turn/start-shaped enqueue into the internal fixture state, returning a
    /// turn id (NOT a relay message id — no port, no sidecar, no RelayContract).
    /// The steer-shaped variant lives on [`FixtureDaemonProvider::steer`]. pid is
    /// NEVER read; `fx.relay*` is NEVER consulted.
    fn inject(
        &self,
        _fx: &ProviderFx,
        _key: &SessionKey,
        _message: &str,
        _from: &str,
    ) -> Result<String, InjectError> {
        let mut st = self.state.borrow_mut();
        let n = st.next_turn;
        st.next_turn += 1;
        st.active_turn = Some(n);
        Ok(turn_id(n))
    }
}

/// The daemon's boot waiter: readiness is the handshake record, not a pid file.
struct HandshakeWaiter {
    ready: bool,
}

impl BootWaiter for HandshakeWaiter {
    fn wait_ready(&self, name: &str) -> Result<(), BootFailure> {
        if self.ready {
            Ok(())
        } else {
            // A daemon that never handshaked — readiness via the handshake record,
            // never a pid-file timeout. Reuses the BootFailure shape (PidFile is
            // the closest phase; the detail names the handshake, not a pid file).
            Err(BootFailure {
                phase: crate::boot::BootPhase::PidFile,
                detail: format!("fixture daemon \"{name}\" never sent its initialize handshake"),
            })
        }
    }
}
