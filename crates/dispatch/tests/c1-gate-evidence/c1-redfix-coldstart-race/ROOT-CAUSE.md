# C1 redfix — g_coldstart NONDETERMINISTIC (macOS) root cause + fix

## The red
CI run 27049722958 (phase/c1-swap @ 5f0297f, doc-only over passing 2562cdf):
macOS `g_coldstart` FAILED at the CHAIN step — `qd send:pty` exit 1,
stderr "Session is not in zmx — cannot send." with `ls` GREEN
(`lists cold-sess=true`), pre-spawn-free precondition GREEN, mutation
control GREEN.

## Verdict: (a) — a propagation race. NOT (b).

### (b) refuted by audit
`qd ls` and `qd send:pty` share the SAME backend-aware session-resolution path:
`common::all_sessions` (crates/qd/src/bin/qd/verbs/common.rs:100) parses QD_MUX
ONCE, builds the embedded mux AND `MuxDirs::embedded(qrmux_dir)`, and scans that
dir via `join::gather_with_dirs`. There is NO unconditional `resolve_zmx_dir` in
the resolution path (send.rs:141 `resolve_zmx_dir` is only the `op_dir` fallback,
reached only AFTER `zmx_name` is Some). The path is fully backend-aware; `ls` and
`send` cannot diverge on backend. So the divergence is TEMPORAL, not structural.

### (a) proven by reproduction + instrumented capture
Reproduced locally: 15/30 fail single-threaded; ~50% under CPU load (3x `yes`).
Identical stderr to CI.

Engine instrumentation (QD_DIAG_JOIN) printed, at the FAILING `send`'s gather:

    [DIAG gather] mux pids = [("cold-sess", 7176)]      <- mux session IS listed
    [DIAG gather] reg pid 7180 ppid-chain = [7180]      <- ppid walk STOPS at 7180

External `ps` taken microseconds later:

    reg pid 7180 ancestors(3)=[7176, 7100]
    7100  1     qd qrmux-server --socket-dir .../qrmux       (daemon)
    7176  7100  bash -lc '...fake-claude.sh ...'             (MUX SESSION PID)
    7180  7176  cat                                          (REGISTRY PID)

The mux session pid is 7176 (the `bash -lc` parent shell). The registry pid is
7180 (the claude/cat child). The match in `join_sessions` links a live registry
row to its mux session by pid-equality OR a 3-level ppid-ANCESTOR walk. That walk
needs `ppid_map[7180] = 7176`. The engine's single `ps -eo pid=,ppid=` snapshot
DID NOT CONTAIN pid 7180's edge — the `cat` child was forked microseconds before
the snapshot, after the registry row (written by fake-claude's `printf` BEFORE
`exec cat`) was already on disk. So:
  - `ls` sees the session (registry row alone gives the NAME) -> GREEN.
  - `send` finds `zmx_name = None` (ancestry walk missed) -> "not mux-live".

This is a REAL user-facing product bug: a user running `qd new` then immediately
`qd send` hits the same window — claude's registry row lands before its ppid edge
to the mux-tracked shell is visible in `ps`.

## The fix (eliminates the race window by DESIGN)

The embedded qrmux daemon tracks each session by NAME, and the registry row
carries that same name. So for the EMBEDDED lane, link a live registry row to its
mux session BY NAME when the pid/ancestor walk comes up empty — deterministic,
no dependence on `ps` propagation.

- `JoinInputs.match_live_by_name` (join.rs) — backend-keyed flag. Set true ONLY
  for `MuxDirs::Embedded` in `gather_with_dirs`. The zmx lane keeps it FALSE so
  output stays TS-byte-identical (zmx tracks the shell; by-name merging an
  unmatched zmx session into a live row would change the faithful output).
- `join_sessions` live-row loop — after the pid/ancestor walk, if still unmatched
  and `match_live_by_name`, match an unused mux session by the registry row name.

### Deterministic gate arm (structurally forces the adverse ordering)
`join::tests::embedded_by_name_links_live_row_when_ppid_edge_invisible` — a live
row pid 7180, mux pid 7176, EMPTY `ppid_map` (the `ps` snapshot missed the child),
`match_live_by_name=true`: asserts `zmx_name = Some("cold-sess")`. This is the
exact failure shape, with the race window removed by construction (no ps edge at
all), so determinism follows by design, not timing luck.
`join::tests::zmx_lane_does_not_name_match_live_rows_byte_stable` — the negative
control: same shape, zmx lane (flag false), asserts the live row stays unmatched
and the zmx session remains a separate ZmxOnly row (byte-stable).

## Wrong-layer error text fix
"Session is not in zmx" under embedded named the wrong layer. Now backend-keyed
(send.rs + common::send_backend_label):
  - embedded: "Session has no live qrmux session — cannot send (it may still be
    starting up; retry in a moment)."
  - zmx (BYTE-STABLE): "Session is not in zmx — cannot send."

## Loop counts
- PRE-FIX:  15/30 FAIL single-threaded; ~50% under CPU load. (CI: intermittent.)
- POST-FIX: 30/30 PASS under the same 3x-`yes` CPU load that produced ~50% pre-fix.
- Full workspace suite: GREEN (qd lib 622 incl. 2 new join tests).
- clippy --workspace --all-targets -D warnings: clean. cargo fmt --all --check: clean.
