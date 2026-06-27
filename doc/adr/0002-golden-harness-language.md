# ADR 0002: Golden harness uses a Python recorder + bash asserter (Option B)

**Status:** Accepted (orchestrator-reviewed 2026-06-04)
**Date:** 2026-06-04

## Context

The Phase 0b golden-master harness must drive qd/zmx through a real PTY, capture
raw bytes, normalize, compare, and enforce per-case timeout budgets. The spike
(`spike/empirical/`) proved the PTY mechanics in three small Python scripts
(`pty_capture.py`, `pty_drive.py`, `analyze.py`) and produced real `.raw`
captures. The project itself is Rust.

Spec §3.0 offers two options: (A) a Rust binary that owns the PTY via
`forkpty`/portable-pty, or (B) a thin Python recorder (reuse the proven PTY code)
driven by a Rust/shell asserter that owns jail + normalization + comparison +
budgets. The recommendation was to **lean B for Part 1** to de-risk the PTY
mechanics, unless `forkpty`+winsize+inject could be ported to Rust cleanly within
budget.

## Decision

**Option B.** The recorder (`test/golden/recorder/record_pty.py`) is a direct
port of the proven spike PTY code (capture + inject + SIGWINCH/storm + child
exit-code capture). The asserter, jail, normalizers, and comparators are
**bash 3.2** (`test/golden/lib/{jail,normalize,compare}.sh`, `verify.sh`) so they
run identically on macOS arm64 and Linux x86_64 CI with zero extra toolchain.

Rationale:
- The PTY mechanics are the highest-risk part to port and the spike already
  proved them byte-for-byte; reusing them removes that risk entirely.
- The timeout-budget, jail, normalization, and comparator layers are first-class
  and toolchain-independent (POSIX sed/awk/grep), matching the qd-qa battery's
  proven shell idiom that the jail is ported from.
- The one place a typed Rust carrier adds real value — the permissive-parse
  dirty-state corpus — IS in Rust (`crates/golden`), under `cargo test`.

Python is already a hard dependency of the recording path on every dev/CI host
(it ships with macOS and the Linux runners), so Option B adds no new runtime that
isn't already present for the recorder.

### Python floor: 3.6 (enforced)

The recorder (`record_pty.py`) uses only stdlib (`os`, `pty`, `select`, `signal`,
`fcntl`, `termios`, `struct`, `base64`) plus f-strings, so the minimum is
**Python 3.6**. The harness ENFORCES this floor before any recording:
`test/golden/lib/check_python.sh` (`check_python_floor`) is sourced and called by
`verify.sh`, `fixtures/layer2/run_layer2.sh`, and `dryrun/run_dryrun.sh`. Below
the floor (or python3 missing) it fails closed with a clear message and a usage
exit (64). The check is bash 3.2 compatible: it asks Python for `major minor` as
two integers and compares with integer-only shell arithmetic (no version-tuple
parsing in shell, no GNU-only tooling).

## Consequences

- Single proven PTY implementation; no reinvention of `forkpty`/winsize/inject.
- The asserter is plain shell: easy to read in a diff, trivially portable, and the
  mutation test attacks it directly (normalize/compare/budget are cleanly
  separable functions).
- Tradeoff: a Python dependency sits in the test path. Mitigated — it is recorder-
  only, and the dirty-state invariant (the one that must run as a unit) is Rust.
- A later phase MAY port the recorder to pure Rust (the spike's `retach` work
  shows portable-pty + vte reproduces the behavior). This ADR would then be
  superseded. The bash asserter/jail/normalizer layers are designed to be
  recorder-language-agnostic, so that port is isolated.
