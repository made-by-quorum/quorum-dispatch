# sbmux Integration Test Suite

Port of B1's 6-point harness scenarios (G1–G6) into Rust integration tests, plus
negative control scaffold. Stress validates the sbmux daemon under stress conditions
matching the B1 fork-decision baseline.

## The 6-Point Gate

Each scenario is a proof checkpoint for sbmux under production-like conditions:

- **G1 — `send` verb:** One-shot input (no attach) into a running session. Acceptance
  keyed on **app output** (not echo), per ADD-6 kernel divergence.
- **G2 — Altscreen stress:** vim/less under attached client. Validates render,
  restore-equivalence, scroll-intact, no-mode-leak per ADR-0004 (screen-model mux design).
- **G3 — SIGWINCH storm:** ≥50 rapid resizes during stream output. Checks propagation,
  no corruption, responsiveness.
- **G4 — Paste-burst:** 64KB single write to `cat`. Byte-exact recovery + app-output
  scrollback zero-drop (keyed on app, not echo per ADD-6).
- **G5 — 1M-line soak:** Memory plateau check (≤300MB macOS, ≤250MB Linux),
  responsiveness post-soak, backlog-completeness.
- **G6 — Reattach-replay:** Backlog-completeness, scroll-intact, no-altscreen-leak
  per ADR-0004 invariants. Runs standalone + as tail assertions on G2–G5.

**Negative control:** `SBMUX_TEST_BREAK=drop1000` breaker (drops every 1000th PTY byte)
must make G4 + G6 FAIL. If broken mux passes, harness lacks teeth.

## Running Tests

### All integration tests (scaffold form)

```bash
cd crates/sbmux
cargo test -p sbmux --test integration_tests -- --nocapture
```

### Individual scenario

```bash
cargo test -p sbmux --test integration_tests g1_send_verb -- --nocapture
cargo test -p sbmux --test integration_tests g2_altscreen_stress -- --nocapture
```

### Negative control (M3, currently ignored)

```bash
cargo test -p sbmux --test integration_tests negative_control_breaker -- --ignored --nocapture
```

### Unit tests within sbmux (M1 baseline: 778 passing)

```bash
cargo test -p sbmux --lib
```

## Jail Contract (ADD-4)

Every test runs under a hermetic per-run jail:

**Environment variables set:**
- `HOME` — load-bearing for sb registry (captured @ `$JAIL_ROOT/home`)
- `SB_HOME`, `ZMX_DIR` — sb-specific state
- `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_RUNTIME_DIR` — XDG dirs
- `TMPDIR` — per-run temp
- `SB_RUST_LOCK_DIR` — build lock (scoped to jail)

**Socket layout (sbmux-specific):**
- Daemon sockets live at `$XDG_RUNTIME_DIR/sbmux/<session-name>.sock`
- Socket dir is the scope for kill/gc during teardown

**Safety:**
- Jail prefix: `sbmux-<runid>-` on session names (guards against accidental production kills)
- Production-path refusal belt: actively forbids touching real HOME, `.sb`, `.claude`, etc.
- Fail-closed: if jail setup detects any production path, the test fails immediately

**Cleanup:**
- `setup_jail()` → creates jail, exports env
- `teardown_jail()` → kills sessions, sweeps sockets, removes jail_root
- Teardown is idempotent (safe to call multiple times)

## Assertion Classes

Reusable comparators across all scenarios (in `tests/lib/assertions.rs`):

```rust
assert_backlog_completeness(lines, expected_count, "test")
  → Verify lines produced while detached appear in replay (ADR-0004)

assert_scroll_intact(scrollback, sentinel, "test")
  → Verify pre-detach lines intact (ADR-0004)

assert_altscreen_replay(raw_bytes, expect_1049h, expect_1049l, "test")
  → Verify exact ?1049h/?1049l replay counts and zero legacy ?47/?1047
    (Divergence #1, REVERSED 2026-06-10: performer absorbs the app's mode
    bytes; renderer replays alt-screen state per client)

assert_no_drop(sent_bytes, received_output, app_output_check, "test")
  → Verify bytes recovered (app-output-keyed per ADD-6, not echo)

assert_responsive(session_name, jail_env, "test")
  → Sentinel round-trip post-soak (stub for M3)
```

Each assertion emits structured `[test] PASS` / `[test] FAIL` with context.

## Named Divergences (Carry from B1)

### Divergence #1: Altscreen absorb-and-rerender (G2 amendment)

sbmux is a **screen-model mux** (not a passthrough). The vte performer consumes
DEC modes 1049/47/1047 server-side and re-renders; never forwards altscreen modes
to clients. Same architecture class as zmx (ghostty_vt server model).

**G2 asserts invariants (ADR-0004 intent), not literal byte forwarding:**
- render + restore-equivalence + scroll-intact + no-leak (mode bytes AND content)

**Citation:** ADR-0004, B1 decision memo (lead ruling #1), reattach-replay logic.

### Divergence #2: Kernel tty echo loss under flood (ADD-6, macOS-specific)

macOS tty line discipline **drops ECHO bytes under input flood**, mux-independently.
Evidence: bare PTY with NO mux echoed 246/2000 tokens at 72KB burst; sbmux ingest
instrumented lossless every run; app-output complete (2000/2000). The mux cannot
replay bytes the kernel never delivered.

**Consequence:** All assertions on echo data in sbmux must be keyed on
**APPLICATION OUTPUT**, never echo bytes. This is a **macOS-only divergence**
(Linux does not exhibit; kernel 6.17 echo complete on same test).

**Citations:** ADD-6, B1 decision memo (lead ruling #2, evidence chain), G4 comments.

## Artifact Capture (M3)

Integration test runs produce evidence artifacts:

```
target/test-evidence/<runid>/
  ├── g1/
  │   ├── send.raw         — raw PTY capture
  │   ├── assertions.log   — assert results
  ├── g2/
  │   ├── vim.raw
  │   ├── reattach.raw
  │   └── assertions.log
  ├── g4/
  │   ├── burst.raw        — 64KB capture
  │   ├── g4_ingest_counters.txt
  │   ├── g4_echo_divergence.txt (macOS)
  │   └── assertions.log
  ├── g5/
  │   ├── rss.csv          — [timestamp, rss_kb] samples
  │   ├── soak.DONE        — completion marker
  │   └── assertions.log
  └── negctl/
      ├── G4/
      ├── G6/
      └── negctl.txt       — PASS/FAIL verdict
```

Evidence tree supports:
- **Reproduction:** cold QA can re-run gate from evidence alone
- **Debugging:** raw captures for protocol/rendering analysis
- **Trending:** RSS samples for leak detection
- **Verification:** assertion logs for deterministic re-checks

## Roadmap

- **M2 (current):** Jail setup, assertions, scaffold G1–G6, negative control skeleton
- **M3 (gate):** Implement G1–G6 scenarios, run full gate, fix rounds within timebox
- **M4 (macOS + Linux):** Platform testing and re-budgeting
- **M5 (polish):** PROTOCOL.md finalization, module docs, war-story preservation
- **M6 (closure):** PR review, merge, tag `phase-b2` at pass (b) close

## References

- **Spec:** `exec/b2-spec.md` (deliverable #4, ground rule 3)
- **B1 baseline:** `~/work/sb-rust-b1/gate/evidence/attempt2/` (cold-verified)
- **B1 decision memo:** `exec/b1-decision-memo.md` (6/6 green, fork recommendation)
- **Jail pattern:** `test/golden/lib/jail.sh` (tag phase-0b-part1, ADD-4)
- **Addenda:** `exec/plan-addenda.md` (ADD-1 through ADD-6)
- **ADR-0004:** Session reattach / screen model invariants
