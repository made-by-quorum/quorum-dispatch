# A2 pass-(b) closure replay — evidence (sbr-pa2-lead2)

Date: Fri Jun 5 10:23-10:29 EDT 2026 (system `date`). SUT: Rust `sb` built from
main @ 7c8482d11f18300d993bf4dfdc62fb748952d2f1 (debug,
`scripts/build-lock.sh cargo build -p sb`, worktree `~/work/wt-a2-passb`,
binary sha256 783ea96436210322…). Verification basis: post-merge CI on main
@ 7c8482d = run 27019996043 (success) + the local verify.sh replays below.
All replays serial (host memory WARN-2). Jail: rule 9 + ADD-4 hermetic env via
lib/jail.sh, fresh jail per scenario; TMPDIR=/tmp (A4 F2 lesson). Real zmx
0.6.0 (Cellar binary sha256 edb5453805124fc7…, mtime 2026-05-16) in every
jail; claude = committed stub 1.7.0 (provenance belt asserted per row).

Invocation per row (from this worktree's test/golden):

    TMPDIR=/tmp SB_UNDER_TEST=$SB JAIL_SB_CMD=$SB \
      ./verify.sh --scenario scenarios/<row>.sh
    # $SB = ~/work/wt-a2-passb/target/debug/sb

## Row results (A2-owned corpus rows, verify.sh true exit codes)

| Row | Class | verify.sh rc | Result |
|---|---|---|---|
| zmx-dir-resolution (macOS ZMX_DIR tier, real zmx) | semantic-resolution-outcome | 0 | PASS (stub provenance OK, RECORDED-FROM.macos) |
| ls-info-json | byte-exact | 0 | PASS |
| history | semantic-backlog | 0 | PASS (stub provenance OK) |
| new-session-trace (+ boot-readiness-event dup row) | boot-readiness-event | 0 | PASS (stub provenance OK) |
| build-claude-cmd (default triple) | byte-exact | 0 | PASS (stub provenance OK) |
| build-claude-cmd-config (EXPECTATION-ONLY, W1 P8) | byte-exact (inline expectation) | 0 | **PASS — first-ever Rust drive** (see below) |

Verbatim run log appended at the bottom of this file.

## Linux rows (TMPDIR-collapse + Bug-D XDG_RUNTIME_DIR tiers) — covered at this exact SHA

NOT re-run here (memory WARN-2; redundant Lima cargo). A1 pass-(b) replayed
`zmx_dir_resolution_linux.sh` in-VM (Lima sbtest, aarch64) against a Rust sb
built IN-VM from main @ 7c8482d — the same SHA this closure pins — TODAY
10:13-10:17 EDT, verify.sh PASS with stub provenance OK against
RECORDED-FROM.linux. That one scenario carries BOTH Linux-only matrix rows.
Evidence: `~/work/ws/switchboard/rust/exec/a1-passb-linux-replay-evidence.txt`
(orc-3 ruling relay-1780668758577-9 item 2).

## Preflight row — EXCLUDED-R4 (cite, nothing recordable)

The matrix row stays EXCLUDED-no-stale-zmx-at-pin. Pass-(b) note: the LIVE
preflight path is empirically exercised anyway — every green scenario above
runs the create-path preflight against the real zmx 0.6.0's live `--help`
(110 lines) and got capability=Yes; the L3 negative path remains carried by
the preflight.rs unit matrix.

## F-A2b-1 — zmx-help unit-fixture provenance drift (engine NOT affected; no fixture edit made)

The M1 journal note "pass (b) re-records [the zmx-help fixture] against the
pin" was executed as a CHECK, and the check found drift:

- `crates/sb/src/preflight.rs` `ZMX_060_HELP` claims to be "The REAL zmx 0.6.0
  `--help` output". Live capture (brano, 2026-06-05, all of `--help`/`-h`/
  `help` identical): **110 lines** — the fixture froze only the first ~20
  (header + Commands block); the live output continues with Attach/History/
  Run/Send/Print/Write/Wait prose sections + an Environment-variables block
  (which corroborates the ZMX_DIR > XDG_RUNTIME_DIR > TMPDIR tier order).
- Byte detail: live emits `ctrl+\\` (TWO backslash bytes, zmx's own help
  string); the fixture string encodes ONE (`ctrl+\`).
- The zmx binary did NOT change since capture: Cellar mtime 2026-05-16
  (pre-capture), still 0.6.0 → the 2026-06-04 capture was truncated at
  capture time, not invalidated by an upgrade.
- Engine impact: NONE. Preflight parses the live help at runtime (proven by
  every green row above + the pass-(a) live gate); the fixture is a
  parser-shape unit fixture and the load-bearing Commands-block lines are
  byte-identical in fixture and live (modulo the ctrl+\ line, which the
  parser never reads).
- Disposition per brief (no fixture edits, no silent re-batch): REPORTED for
  orc ruling. Recommended fix (cheap, test-only): replace ZMX_060_HELP with
  the full 110-line live capture + correct the provenance comment; parser
  behavior already proven against the full text live.

## EXPECTATION-ONLY row driven (W1 P8) — tick proposal

`build_claude_cmd_config.sh` was sanctioned (ADR-0010 §a) as a RUST-TARGET row,
UNticked "until the Rust engine exists to DRIVE it". The Rust engine now
exists and drove it: a NON-DEFAULT flag supplied via the ADR-0006 config seam
(`SB_CLAUDE_FLAGS` tier 1) reached the launch argv byte-exactly
(`--dangerously-skip-permissions --name <NAME>`) — a constant-echo loader
would have emitted the default triple and DIFFed. Matrix tick (and whether it
joins the green gate) = orc ruling; the row's own header says it is never
auto-globbed, so ticking is matrix-text only.

## Boot accounting + belts

- Real-claude boots this closure: **ZERO** (corpus-first judgment held: stub
  1.7.0 renders the REAL captured dev-channels dialog verbatim, so the
  new-session-trace replay exercises the ADR-0005 delegated-consent answerer
  content-match + zero-keystroke discipline + PID-file readiness event
  in-jail; the live answerer was already live-proven at pass (a) by probes
  6+7 and re-proven by A4 R5-R7).
- Soak ledger UNCHANGED (no real-claude rows run).
- Real-home belt: jail.sh per-scenario (fresh jail each row, teardown trap);
  no sbrg residue checked via jail teardown paths.

## Verbatim run log (TMPDIR=/tmp, fresh jail per scenario)

    === scenario: zmx_dir_resolution ===
    [verify] scenario=zmx-dir-resolution class=semantic-resolution-outcome budget=45000ms
    [verify] stub provenance OK (RECORDED-FROM.macos: stub_sha256 matches installed stub)
    [verify] PASS: zmx-dir-resolution
    verify-rc=0
    === scenario: ls_info_json ===
    [verify] scenario=ls-info-json class=byte-exact budget=8000ms
    [verify] PASS: ls-info-json
    verify-rc=0
    === scenario: history ===
    [verify] scenario=history class=semantic-backlog budget=60000ms
    [verify] stub provenance OK (RECORDED-FROM: stub_sha256 matches installed stub)
    [verify] PASS: history
    verify-rc=0
    === scenario: new_session_trace ===
    [verify] scenario=new-session-trace class=boot-readiness-event budget=60000ms
    [verify] stub provenance OK (RECORDED-FROM: stub_sha256 matches installed stub)
    [verify] PASS: new-session-trace
    verify-rc=0
    === scenario: build_claude_cmd ===
    [verify] scenario=build-claude-cmd class=byte-exact budget=60000ms
    [verify] stub provenance OK (RECORDED-FROM: stub_sha256 matches installed stub)
    [verify] PASS: build-claude-cmd
    verify-rc=0
    === scenario: build_claude_cmd_config ===
    [verify] scenario=build-claude-cmd-config class=byte-exact budget=60000ms
    [verify] PASS: build-claude-cmd-config
    verify-rc=0

---

# RESOLUTION (orc-3 ruling relay-1780669929563-1, same day)

Replays ACCEPTED (5/5 + Linux cross-citation at identical SHA). Both rulings
granted and executed on this branch:

**Ruling 1 — P8 EXPECTATION-ONLY row TICKED** (coverage-matrix.md): annotated
"first driven vs the Rust engine by A2 pass-(b)", citing this evidence file.
Matrix-text only; the row stays out of the green gate / never auto-globbed.

**Ruling 2 — F-A2b-1 fixture re-record, on-branch (A4 close-clean precedent):**
`crates/sb/src/preflight.rs` `ZMX_060_HELP` replaced with the FULL 110-line
live `zmx --help` capture (2026-06-05, /opt/homebrew/bin/zmx 0.6.0), as a raw
string asserted byte-identical to the capture at splice time. Capture-quality
fix, NOT version drift: the Cellar binary mtime is 2026-05-16 — unchanged
since before the original 2026-06-04 capture, which had frozen only the first
~20 lines AND encoded one backslash on the detach line where live zmx emits
two (`ctrl+\\`, zmx's own help-string escaping — the named escaping artifact).
Test-only diff. Proof: `cargo test -p sb` full suite green post-change (incl.
`real_060_help_advertises_send` now parsing the full help, 14/14 preflight
tests); fmt + clippy -D warnings clean.

Tag `phase-a2` to be pushed AT THE MERGE SHA of this PR (A4 precedent,
orc-confirmed).
