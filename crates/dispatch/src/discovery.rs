//! Discovery health: which gather-time reads FAILED, as distinct from which
//! ones legitimately found nothing.
//!
//! ## Why this exists
//!
//! Carrier selection in `qd send` is a PURE function of one registry/join
//! snapshot — it deliberately does no probing (see
//! `bin/qd/verbs/send_unified.rs`). That is the right shape, but it means every
//! fact the refusal reports must already be ON the snapshot. Before this module
//! the gather step discarded its `io::Result`s:
//!
//! ```ignore
//! let ppid_map = pt.ppid_map().unwrap_or_default();
//! ```
//!
//! so a `ps` that failed with `EPERM` (the ordinary case under a sandbox) was
//! indistinguishable from a host with no processes. The ancestry walk then
//! matched no relay, every claude row got `relay_port: None`, and `qd send`
//! asserted a fact it had never established: *"it has no live receive path"*.
//!
//! [`DiscoveryHealth`] carries the failures forward so the refusal can say
//! "could not determine" — and say WHY — instead. The rule the whole module
//! serves: **never convert `EPERM` into "not found"**.
//!
//! ## What it is NOT
//!
//! It is not a sandbox detector. `qd` reports what it observed (`ps` failed,
//! permission denied) and the remedy that follows from it; it does not claim to
//! know which policy denied the read.

use std::fmt;
use std::io;

/// How a discovery read failed. Only the permission class changes what a caller
/// should DO about it, so that is the only distinction drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// `EPERM` / `EACCES` — the read was refused, so the answer is UNKNOWN.
    /// Nothing may be inferred about what would have been found.
    PermissionDenied,
    /// Any other `io::Error` (spawn failure, missing binary, I/O error). Also
    /// leaves the answer unknown, but with no escalation remedy to suggest.
    Other,
}

impl FailureKind {
    fn classify(err: &io::Error) -> Self {
        match err.raw_os_error() {
            Some(libc::EPERM) | Some(libc::EACCES) => Self::PermissionDenied,
            _ => match err.kind() {
                io::ErrorKind::PermissionDenied => Self::PermissionDenied,
                _ => Self::Other,
            },
        }
    }
}

/// One failed discovery read: what we tried to acquire, how it failed, and the
/// underlying OS error text preserved verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireFailure {
    /// The concrete thing that failed, in the user's terms (`"ps"`,
    /// `"zmx list"`) — not the Rust function name.
    pub source: &'static str,
    /// What the failure means for the caller.
    pub kind: FailureKind,
    /// The underlying error, preserved (`"Operation not permitted (os error 1)"`).
    pub detail: String,
}

impl AcquireFailure {
    pub fn new(source: &'static str, err: &io::Error) -> Self {
        Self {
            source,
            kind: FailureKind::classify(err),
            detail: err.to_string(),
        }
    }

    pub fn is_permission_denied(&self) -> bool {
        self.kind == FailureKind::PermissionDenied
    }
}

impl fmt::Display for AcquireFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {}", self.source, self.detail)
    }
}

/// The gather step's acquisition record. `None` means the read SUCCEEDED (an
/// empty result from a successful read is a real answer, not a degradation).
///
/// Default = everything succeeded, so every existing `JoinInputs` construction
/// and fixture keeps its exact meaning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryHealth {
    /// `ps -eo pid=,ppid=,command=` — feeds the relay ancestry walk. Its
    /// failure is the one that silently nulls `relay_port` on every row.
    pub process_table: Option<AcquireFailure>,
    /// The mux `list` seam — feeds `zmx_name` / `socket_dir` (the PTY carrier).
    pub mux_list: Option<AcquireFailure>,
    /// The claude-process census (stray discovery). Cosmetic by itself, but a
    /// corroborating signal that process inspection is blocked.
    pub claude_procs: Option<AcquireFailure>,
}

impl DiscoveryHealth {
    fn failures(&self) -> impl Iterator<Item = &AcquireFailure> {
        [&self.process_table, &self.mux_list, &self.claude_procs]
            .into_iter()
            .flatten()
    }

    /// Any discovery read failed → absent facts are UNDETERMINED, not absent.
    pub fn is_degraded(&self) -> bool {
        self.failures().next().is_some()
    }

    /// At least one failure was a permission refusal, so an escalation remedy
    /// (run outside the sandbox / with elevated permissions) actually applies.
    pub fn permission_denied(&self) -> bool {
        self.failures().any(AcquireFailure::is_permission_denied)
    }

    /// Whether the RECEIVE-PATH facts specifically (`relay_port` via the `ps`
    /// ancestry walk, `zmx_name`/`socket_dir` via the mux list) were acquired.
    /// A `claude_procs` failure alone does not make a receive path unknowable.
    pub fn receive_path_undetermined(&self) -> bool {
        self.process_table.is_some() || self.mux_list.is_some()
    }

    /// One-line evidence summary: `"ps failed: Operation not permitted (os
    /// error 1)"`, joined with `"; "` when several reads failed.
    ///
    /// Deduplicated: the process table and the claude census are BOTH served by
    /// `ps`, so one refusal fails both and would otherwise be reported twice
    /// with identical text. The reader needs each distinct cause once.
    pub fn evidence(&self) -> String {
        let mut seen: Vec<String> = Vec::new();
        for line in self.failures().map(|f| f.to_string()) {
            if !seen.contains(&line) {
                seen.push(line);
            }
        }
        seen.join("; ")
    }

    /// The remedy line to print after the evidence, when one applies.
    pub fn hint(&self) -> Option<&'static str> {
        self.permission_denied().then_some(
            "process inspection was denied — this is what a sandbox looks like; \
             rerun outside the sandbox or with elevated command permissions.",
        )
    }

    /// The value a field should RENDER as when its acquiring read failed:
    /// `"unknown (ps unavailable)"` rather than a bare `"-"`, which would claim
    /// the field is genuinely absent. `None` when the read succeeded and the
    /// caller's own absent-value rendering is correct.
    pub fn unknown_label(failure: &Option<AcquireFailure>) -> Option<String> {
        failure
            .as_ref()
            .map(|f| format!("unknown ({} unavailable)", f.source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eperm() -> io::Error {
        io::Error::from_raw_os_error(libc::EPERM)
    }

    fn enoent() -> io::Error {
        io::Error::from_raw_os_error(libc::ENOENT)
    }

    #[test]
    fn default_health_is_not_degraded() {
        let h = DiscoveryHealth::default();
        assert!(!h.is_degraded());
        assert!(!h.permission_denied());
        assert!(!h.receive_path_undetermined());
        assert_eq!(h.evidence(), "");
        assert_eq!(h.hint(), None);
    }

    #[test]
    fn eperm_classifies_as_permission_denied_and_earns_a_hint() {
        let h = DiscoveryHealth {
            process_table: Some(AcquireFailure::new("ps", &eperm())),
            ..Default::default()
        };
        assert!(h.is_degraded());
        assert!(h.permission_denied());
        assert!(h.receive_path_undetermined());
        assert!(h.evidence().starts_with("ps failed: "));
        assert!(h.hint().is_some());
    }

    /// A non-permission failure still makes the answer UNKNOWN — it just has no
    /// escalation remedy, so no hint is offered.
    #[test]
    fn other_errno_is_degraded_but_offers_no_escalation_hint() {
        let h = DiscoveryHealth {
            process_table: Some(AcquireFailure::new("ps", &enoent())),
            ..Default::default()
        };
        assert!(h.is_degraded());
        assert!(!h.permission_denied());
        assert_eq!(h.hint(), None);
    }

    /// The census failing alone does not make a receive path unknowable — only
    /// the two reads that actually feed carrier selection do.
    #[test]
    fn claude_procs_failure_alone_does_not_undetermine_the_receive_path() {
        let h = DiscoveryHealth {
            claude_procs: Some(AcquireFailure::new("ps", &eperm())),
            ..Default::default()
        };
        assert!(h.is_degraded());
        assert!(!h.receive_path_undetermined());
    }

    /// One `ps` refusal fails two reads. The reader needs the cause once, not
    /// the same sentence twice.
    #[test]
    fn evidence_reports_a_shared_cause_only_once() {
        let h = DiscoveryHealth {
            process_table: Some(AcquireFailure::new("ps", &eperm())),
            claude_procs: Some(AcquireFailure::new("ps", &eperm())),
            mux_list: None,
        };
        let e = h.evidence();
        assert_eq!(e.matches("ps failed").count(), 1, "{e}");
        assert!(!e.contains("; "), "nothing to join for one distinct cause: {e}");
    }

    #[test]
    fn evidence_joins_every_failed_read() {
        let h = DiscoveryHealth {
            process_table: Some(AcquireFailure::new("ps", &eperm())),
            mux_list: Some(AcquireFailure::new("zmx list", &eperm())),
            claude_procs: None,
        };
        let e = h.evidence();
        assert!(e.contains("ps failed"), "{e}");
        assert!(e.contains("zmx list failed"), "{e}");
        assert!(e.contains("; "), "{e}");
    }

    #[test]
    fn unknown_label_names_the_source_that_failed() {
        let f = Some(AcquireFailure::new("ps", &eperm()));
        assert_eq!(
            DiscoveryHealth::unknown_label(&f).as_deref(),
            Some("unknown (ps unavailable)")
        );
        assert_eq!(DiscoveryHealth::unknown_label(&None), None);
    }
}
