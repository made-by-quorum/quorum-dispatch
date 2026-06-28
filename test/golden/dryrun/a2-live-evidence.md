# A2 pass-(a) LIVE gate — evidence file

**QA agent:** sbr-pa2-qa (M4). **Date:** 2026-06-04.
**Branch:** `phase/a2-zmx-adapter` @ tip `1504ccb` (fetched + reset in worktree).
**Host:** brano.local, arm64/Darwin. **zmx:** 0.6.0 (`/opt/homebrew/bin/zmx`,
`socket_dir` self-reports `/tmp/claude-501/zmx-501`). **claude:** 2.1.163.

This file is the gate artifact. Written as the gate runs, not retrospectively.
Each row: command(s) + key output + PASS / FAIL / BLOCKED-ENV.

## Safety preamble (MANDATORY, ran FIRST)

**Refusal belt green BEFORE any live boot** — spec §10 hard rule.

```
$ bash test/golden/selftest/test_jail_refusal.sh
... 17 passed, 0 failed (EXIT=0)
```

Rows include: unestablished/assert refused, corrupt/home-real-home refused,
corrupt/{qdhome,zmxdir}-prod-path refused, guard/bare-name refused,
pid/unregistered-raw-kill refused, lima/brano-fail-closed refused,
killsession/bare-name refused. **PASS** — belt is armed.

Real-home baseline: `~/.claude/sessions` has 723 rows at gate start. The
real-home belt must show this set UNTOUCHED after every live boot.

**macOS real-claude boot budget: ≤3.** Tally maintained at the bottom of this file.

---

## Row 1 — Workspace green (re-attest on branch tip)

```
$ QD_RUST_LOCK_TIMEOUT=3600 scripts/build-lock.sh cargo test --workspace
  golden lib            4 passed
  golden dirty_state    5 passed
  qd lib              222 passed   <- A2 create/boot/preflight/launch/zmx_mux units
  qd bin                0 passed
  create_claim_race     2 passed   <- multi-PROCESS race winner + claims_dir derivation
  parity                6 passed
  roundtrip             1 passed
  qrmux lib             1 passed
  GRAND TOTAL: 241 passed; 0 failed

$ cargo clippy --workspace --all-targets -- -D warnings   -> CLIPPY_EXIT=0 (no warnings)
$ cargo fmt --check                                        -> FMT_EXIT=0
```
**PASS** — 241/241, clippy -D warnings clean, fmt clean on tip 1504ccb.

## Row 6 — Vendoring (fetch-zmx.sh, 7 rows incl. negative controls)

```
$ bash test/golden/selftest/test_fetch_zmx.sh
ok happy/exit-zero ; happy/tarball-present ; happy/hash-matches-mirror
ok corrupt-mirror/refuses (non-zero) ; corrupt-mirror/no-blob-left
ok tampered-sums/refuses (non-zero) ; tampered-sums/no-blob-left
--- 7 passed, 0 failed ---
```
**PASS** — corrupted-hash + tampered-SUMS negative controls both REFUSE and leave
no blob. Pin = zmx 0.6.0.

## Row 7 — Preflight unit matrix (L3)

```
$ QD_RUST_LOCK_TIMEOUT=3600 scripts/build-lock.sh cargo test -p qd preflight
  real_060_help_advertises_send ........... ok
  old_05x_shaped_help_lacks_send_is_no .... ok
  garbage_output_is_unknown ............... ok
  empty_output_is_unknown ................. ok
  command_not_found_is_unknown_never_no ... ok
  wedged_zmx_carries_timeout_and_does_not_hang ... ok
  run_prose_send_command_must_not_false_positive ok
  word_boundaries_match_ts_regexes ........ ok
  (+ guidance_strings_match_ts, assert_capable_blocks_only_on_definite_no, ...)
  14 passed; 0 failed
```
**PASS** — full matrix: real 0.6.0 help / 0.5.x-shaped / garbage / empty /
command-not-found→Unknown / wedged-zmx timeout. Count = 14.

## Row 9 (part) — 0a + 0b selftests on branch tip

```
$ bash test/golden/selftest/run_selftests.sh
  test_jail_refusal     17 passed
  test_normalize        (passed)
  test_timeout_budget    4 passed
  test_record_gate       2 passed
  test_fetch_zmx         7 passed
  ALL SELFTESTS PASSED
```
Workspace suite (covers 0a normalize + A1 registry/jsonl/zmx_dir) = 241/241 (row 1).
**PASS** — ratchet-back holds on the branch tip. (Post-merge main re-run is the lead's.)

## Row 10a — Mutation evidence: zero-keystroke assert has TEETH (offline)

```
$ QD_RUST_LOCK_TIMEOUT=3600 scripts/build-lock.sh cargo test -p qd boot::
  boot::tests::stock_boot_zero_keystrokes ............................. ok
  boot::tests::mutation_evidence_injected_enter_fails_zero_keystroke_assert ok
  boot::tests::unmatched_dialog_fails_immediately_zero_keystrokes ..... ok
  boot::tests::dev_channels_dialog_answered_once_then_boots .......... ok
  boot::tests::dev_channels_dialog_persists_two_sends_then_fail ...... ok
  22 passed; 0 failed
```
The committed `mutation_evidence_injected_enter_fails_zero_keystroke_assert`
INJECTS an Enter into the audited send-log and asserts the zero-keystroke check
FAILS — proving the assert in `stock_boot_zero_keystrokes` is not vacuous.
**PASS** (code-level half of row 5 / row 10a). Live half recorded in Row 2/5 below.

---

## Row 2 + Row 4 + Row 5(live stock half) — BOOT A (real-claude #1 of 3)

Driver: `test/golden/dryrun/a2-live-row2.sh` (built `target/debug/qd` + real zmx
0.6.0, in-jail, seeded claude state via probe method, STOCK flags
`QD_CLAUDE_FLAGS=--dangerously-skip-permissions` → no dev-channels → dialog-free).

### Row 4 — concurrent create race (N=4, same name)
```
child 0 exit=1 ; child 1 exit=1 ; child 2 exit=1 ; child 3 exit=0
wins=1 losers=3
loser stderr (all 3): "qd new: name '<NAME>' is being created by another process
  (claim held: {"pid":13627,"timestamp":1780615005009,"name":"<NAME>"}). No session was created."
zmx list in-jail: EXACTLY ONE task (name=<NAME> pid=13869)
tasks matching NAME = 1
```
**PASS** — exactly one winner; losers fail at the O_EXCL claim with the holder
payload named; exactly one zmx task. (Losers fail at the claim BEFORE
start_detached → they spawn NO claude. Boot A = exactly 1 real-claude boot.)

### Row 5 (live stock half) — dialog-free boot, ZERO keystrokes
```
winner PID file (JAILED home): .../home/.claude/sessions/13913.json
  {"pid":13913,...,"version":"2.1.163",...,"status":"idle",...,"name":"<NAME>"}
  status=idle
zmx history: clean "Welcome back!" Claude Code v2.1.163 boot screen — NO
  folder-trust dialog, NO dev-channels dialog present in scrollback.
```
**PASS (live half).** Stock boot reached `status:idle` with no dialog interaction
visible in `zmx history` and zero answerer keystrokes. Code-level half (the
mutation-evidenced `stock_boot_zero_keystrokes`) re-attested in Row 10a.
HONESTY NOTE (decomposition recorded as required): the literal "ZERO `zmx send`
calls" assertion is the OFFLINE audited test (boot.rs); the LIVE evidence is the
absence of any dialog on screen + idle reached without an answerer firing.

### Row 2 — send / reattach / kill (REUSING the boot-A winner session)
```
send (raw inject, NO trailing CR — no turn, no API spend):
  jail_zmx send <NAME> "QA_LIVE_MARKER_13565"
  history composer line: "❯ QA_LIVE_MARKER_13565"
  SEND-ASSERT: PASS (marker in APPLICATION OUTPUT per ADD-6, not echo)

reattach: jail_zmx history <NAME> → 23-line boot-screen backlog (Welcome banner)
  after session detached the whole time. REATTACH-ASSERT: PASS

kill: zmx task removed (qd kill verb DEFERRED to gc phase per bin/qd.rs — RECORDED
  EXCLUSION, not silent skip; used the zmx-side kill primitive the belt wraps).
  zmx list after: "no sessions found". KILL-ASSERT: PASS
```
REAL-HOME BELT: sessions 723→723, no leaked rows. **HOLDS.**

**Row 2 PASS / Row 4 PASS / Row 5 live-half PASS.** Boots spent: 1/3.

OBSERVATION for lead (cosmetic, non-blocking): jailed home shows a `claude install`
setup warning (`~/.local/bin/claude` absent in jail); claude still booted via PATH
`claude` and reached idle. Does not affect any assertion.

---

## Row 3 — `--agent` fail-closed (LIVE, in-jail)

Driver: `test/golden/dryrun/a2-live-row3.sh`. (positive control = real-claude #2 of 3)

### Negative — unresolvable agent, NO boot
```
$ qd new <NAME> --cwd <work> --agent bogus-agent-xyz
exit=1
stderr: "qd new: agent definition 'bogus-agent-xyz' does not resolve
  (/tmp/.../agents/bogus-agent-xyz.md) — refusing to boot a generic session
  (fail-closed). No session was created."
zmx list AFTER: "no sessions found" (UNCHANGED — no task, no boot)
```
**PASS** — nonzero exit, stderr names the agent AND the exact path tried, zmx list
unchanged (the divergence-from-TS deliverable: TS would blind-forward --agent and
boot a generic session; Rust refuses).

### Positive control — resolvable agent boots
```
$ printf ... > <agents_dir>/real-helper.md ; export QD_SPAWN_AGENTS_DIR=<agents_dir>
$ qd new <NAME2> --cwd <work> --agent real-helper
exit=0 ; stdout: Started detached session "<NAME2>"
zmx list: name=<NAME2> pid=46634 (one task)
```
**PASS** — resolvable agent passes the gate and creates the session.
REAL-HOME BELT: 723→723. HOLDS. Boots spent: 2/3.

---

## Row 4 — configured-flag boot + answerer (LIVE, boot B = real-claude #3 of 3)

Driver: `test/golden/dryrun/a2-live-row4.sh`. DEFAULT flags (no QD_CLAUDE_FLAGS
override → built-in `--dangerously-skip-permissions
--dangerously-load-development-channels server:relay`). Seeded
cachedGrowthBookFeatures from real home (287 keys, probe-3) + channels symlink to
~/work/cc-relay.

```
$ qd new <NAME> --cwd <work>   (default flags, dev-channels ON)
exit=0 ; stdout: Started detached session "<NAME>"
zmx history scrollback INCLUDES:
  "▎ Channels (experimental) messages from server:relay inject directly in this
     session · restart without --dangerously-load-development-channels to stop"
  "▎ server:relay · no MCP server configured with that name"
zmx list: one task (pid=65485)
PID file: status=idle
REAL-HOME BELT: 723→723 HOLDS
```

**PASS (configured-flag boot half) — with a recorded caveat.** The dev-channels
feature DID engage (channels-experimental banner + server:relay injection visible
in application output), exit 0, status idle. This proves the built qd boots
correctly with the configured (dev-channels) flag set and that the GrowthBook gate
opened in-jail.

**CAVEAT for the lead (honest, important):** NO consent dialog rendered, so the
LIVE answerer-against-a-real-dialog path was NOT exercised by this boot. Cause:
the probe-3 seed includes `"dangerouslyLoadDevelopmentChannels": true` in
`.claude.json`, which claude 2.1.163 treats as PRE-ACCEPTED consent (state, not
keystroke) — exactly ADR 0005 point 3. The dialog is suppressed; the
EventBootWaiter sees the PID file appear with no dialog on screen → zero
keystrokes → success. To force a LIVE dialog the seed would have to OMIT that key,
costing a 4th real-claude boot — OVER the ≤3 budget. Per spec §11.5 the answerer
path is therefore attested OFFLINE (next block); the live dialog-answer variant is
DEFERRED as a budget exclusion, NOT a failure. This is NOT BLOCKED-ENV (the gate
opened) — it is "answerer live-exercise deferred for boot-budget".

### Row 4 / Row 5 answerer — OFFLINE re-attestation (the M3 dialog coverage)
```
$ cargo test -p qd boot::
  dev_channels_dialog_answered_once_then_boots ....... ok  (matched dialog → ONE \r → boots)
  dev_channels_dialog_persists_two_sends_then_fail ... ok  (≤2 sends then loud FAIL)
  detect_dialog_matched_dev_channels ................. ok  (verbatim captured dialog text)
  strip_ansi_on_captured_dev_channels_dialog ......... ok
  named_dialogs_has_exactly_one_dev_channels_entry ... ok  (exactly one named dialog)
```
These use the M3 fixture = the dialog text captured VERBATIM during the lead's
probe boots ("WARNING: Loading development channels" / "1. I am using this for
local development" / "2. Exit"). **PASS** — answerer content-match + single-shot +
≤1 retry + loud-fail all covered offline against real captured dialog bytes.

## Row 5 — unmatched-dialog loud-fail (OFFLINE attestation)

```
$ cargo test -p qd boot::
  unmatched_dialog_fails_immediately_zero_keystrokes ... ok
  detect_dialog_unmatched_folder_trust ................. ok
  stock_boot_zero_keystrokes ........................... ok
  mutation_evidence_injected_enter_fails_zero_keystroke_assert ... ok
```
**PASS (offline).** An unmatched dialog (e.g. folder-trust) → FAIL LOUD naming
`qd attach`, ZERO keystrokes. The LIVE variant (un-pre-trusted cwd → loud fail) is
OPTIONAL per spec §11.5 and would cost a boot ATTEMPT — DEFERRED to stay within
the ≤3 real-claude boot budget. Mutation evidence (inject Enter → assert fails)
re-attested here = row 10a teeth.

Boots spent after Row 4: 3/3 (AT BUDGET — no further real-claude boots taken).

---

## Row 10b — Mutation evidence: claim-race has TEETH

Mutated `crates/qd/src/create.rs` claim step to claim a UNIQUE per-PID name
(`<name>-qa-mut-<pid>`) instead of the shared name → the O_EXCL claim never
collides → every racer "wins" (the claim is effectively disabled).

```
# MUTATED:
$ cargo test -p qd --test create_claim_race create_path_claim_race_...
  thread '...' panicked at crates/qd/tests/create_claim_race.rs:219:5:
  assertion `left == right` failed: exactly ONE process must win the claim (got 6)
  test result: FAILED. 0 passed; 1 failed

$ git checkout crates/qd/src/create.rs    # REVERTED

# REVERTED:
$ cargo test -p qd --test create_claim_race
  create_path_claim_race_exactly_one_winner_across_processes ... ok
  claims_dir_is_under_claude_root ... ok
  test result: ok. 2 passed; 0 failed
```
**PASS** — with the claim disabled the multi-PROCESS race reports 6 winners (FAIL);
reverted, exactly 1 winner (green). `git diff create.rs` = 0 lines after revert.
The race test is non-vacuous. (Row 10a zero-keystroke mutation re-attested in Row 5.)

## Row 11 — L9a injected-home discipline (no real-home resolution)

- A1's `common::assert_not_real_home(&home)` helper is invoked by BOTH the race
  parent AND each re-exec'd child in `create_claim_race.rs` (lines 104, 173) —
  every offline test home is asserted to be a tempdir, never `~`.
- LIVE jail evidence (rows 2/3/4): the REAL-HOME BELT held on EVERY boot
  (sessions 723→723, zero leaked rows grepped from `$JAIL_REAL_HOME/.claude/sessions`).
  The jail's positive-sandbox belt (refusal selftest 17/17) refuses any var
  resolving outside `qdrg-runs/`, and HOME is jailed (ADD-4).
**PASS** — nothing under test resolved under the real home, offline or live.

---

## Row 8 — Linux live smoke (Lima `sbtest`, aarch64/vz)

Driver: `test/golden/dryrun/a2-lima-smoke.sh`. VM: aarch64 / Linux / hostname
`lima-sbtest`, rust 1.95.0, zmx 0.6.0 at `/usr/local/bin/zmx`. qd built IN-VM
(`CARGO_TARGET_DIR=/tmp/qd-vm-target`; the worktree mount is read-only so target
went VM-local). FAKE claude via `CLAUDE_BIN` (writes a valid registry row then
`sleep 600`) — exercises the FULL create path + real zmx + boot waiter at ZERO
claude auth/cost.

```
=== ENV === aarch64 / Linux / lima-sbtest
CREATE: qd new → exit=0, "Started detached session"
zmx list: one task (name=...-lima pid=199561)
PID file (jailed home): {"...","status":"idle",...} ← EventBootWaiter polled it
SEND: "LIMA_SMOKE_MARKER" present in application output
RSS (KB): zmx daemon ~3660  | fake-claude(sleep) ~6356  | qd exits after detach
KILL (real zmx): task gone from list; wrapper PID 199561 is DEAD
VERDICT: create_exit=0 task=1 pidfile=present kill_after=0 → PASS
```
**PASS** — full create→send→kill against real zmx 0.6.0 on Linux/aarch64 with the
boot waiter reaching idle off the fake-claude registry row.

Recorded notes:
- **real-claude-on-Linux DEFERRED** (auth/backend env = A4 scope) — recorded exclusion.
- The Lima sentinel `/etc/qd-rust-lima` is ABSENT, so `jail_require_destructive_ok`
  would fail-closed. The smoke does NOT use it — it uses the standard in-jail
  prefix-guarded kill (allowed on any host). No destructive-gated op was attempted.
  (Heads-up for the lead: if a future row needs the destructive gate in this VM,
  the sentinel must be planted first.)
- VM target/lock/jail dirs cleaned post-run; no stray jail procs.

---

## Bonus — macOS fake-claude create/kill cycle (real zmx, ZERO real-claude boot)

Driver: `test/golden/dryrun/a2-mac-fakeclaude.sh`. Folds the fake-claude trick
into macOS (spec: "real zmx + fake claude costs nothing and hardens row 2").
```
create exit=0 "Started detached session ...-fake"
zmx tasks=1 → kill → tasks=0 ; real-home belt 723→723
MAC-FAKE-CLAUDE: PASS
```
**PASS** — a second independent macOS create→kill path (different from the live
race boot), via real zmx + fake CLAUDE_BIN. No real-claude boot consumed.

---

## Boot budget tally (macOS real-claude)

| # | Boot | Row(s) | Outcome |
|---|------|--------|---------|
| 1 | race winner `...-race` | 2, 4, 5(live stock) | idle, reused for send/reattach/kill |
| 2 | `...-helper` (agent positive control) | 3 | created, exit 0 |
| 3 | `...-cfg` (configured dev-channels) | 4 | idle, channels engaged |

**TOTAL = 3 / 3. AT BUDGET. No 4th real-claude boot taken.**
Fake-claude cycles (macOS bonus + Lima) consumed ZERO real-claude boots.

## Per-row result table

| Row | What | Result |
|-----|------|--------|
| 1 | Workspace green (test 241/241 + clippy -D + fmt) | **PASS** |
| 2 | LIVE create/send/reattach/kill vs real zmx 0.6.0 | **PASS** (kill via zmx primitive; `qd kill` verb deferred to gc — recorded exclusion) |
| 3 | `--agent` fail-closed (neg + pos control) | **PASS** |
| 4 | Concurrent create — exactly one winner | **PASS** |
| 5 | Dialog-free boot: stock zero-keystroke (live+offline); unmatched loud-fail (offline) | **PASS** |
| 6 | Vendoring fetch-zmx.sh (7 rows, neg controls) | **PASS** |
| 7 | Preflight unit matrix (14) | **PASS** |
| 8 | Linux live smoke (Lima aarch64, fake claude) | **PASS** (real-claude-on-Linux deferred = A4) |
| 9 | Ratchet-back (0a+0b selftests + A1 suite, branch tip) | **PASS** |
| 10a | Mutation: zero-keystroke assert has teeth | **PASS** (offline) |
| 10b | Mutation: claim-race has teeth (disable→6 winners→FAIL; revert→PASS) | **PASS** |
| 11 | L9a injected-home discipline (no real-home resolution) | **PASS** |

### Recorded exclusions / deferrals (NOT silent skips)
- **Configured-flag LIVE answerer-against-real-dialog (Row 4):** the dev-channels
  boot SUCCEEDED with channels engaged, but the consent dialog was suppressed by
  the pre-accept seed (`dangerouslyLoadDevelopmentChannels: true`, ADR 0005 pt 3).
  Live dialog-answer exercise DEFERRED (would cost a 4th boot, over budget);
  attested offline via the M3 captured-dialog fixtures. NOT a failure, NOT
  BLOCKED-ENV (the gate opened).
- **Unmatched-dialog LIVE variant (Row 5):** OPTIONAL per spec; deferred for boot
  budget; covered offline (`unmatched_dialog_fails_immediately_zero_keystrokes`).
- **`qd kill` verb (Row 2):** deferred to gc phase per `bin/qd.rs` (A2 lands only
  the zmx-side kill primitives). Kill asserted via the zmx primitive the belt wraps.
- **real-claude-on-Linux (Row 8):** A4 scope (auth/backend env).

### Safety summary
- Refusal belt GREEN (17/17) BEFORE any live boot.
- REAL-HOME belt held on EVERY live boot AND every fake-claude cycle: `~/.claude/
  sessions` 723→723 throughout, zero leaked prefixed rows (grep-checked each run).
- All live ops in-jail (positive-sandbox enforced); every jail torn down.
- No safety-rule near-misses.

### QA FINDING for the lead — teardown leaks the detached claude child PID
After the live boots, two `claude` processes (`...-helper` from this gate's boot
C, and `...-p5d` from an EARLIER lead probe run) were found ALIVE and orphaned
(PPID=1) in already-torn-down jails: the `jail_teardown` belt does `zmx kill`
(kills the zmx WRAPPER) but the boot's detached claude CHILD is NOT in the jail's
PID registry, so it survives the wrapper and the `rm -rf JAIL_ROOT`. QA reaped
them manually (SIGTERM left them in state `T`; SIGKILL reaped). All prefixed,
all under qdrg-runs — no production process touched, real-home belt held 723→723
throughout. This is NOT a gate failure (the SESSIONS are correctly killed; zmx
list goes empty), but it IS a jail-hygiene gap: orphaned claude procs accumulate
across runs. **Recommendation to lead:** either register the spawned claude PID
in the jail (via `findZmxWrapperForPid` ancestry — A2 already ports the primitive)
so teardown kills the child, or have teardown SIGKILL any `claude --name
<JAIL_PREFIX>*` survivors. Relevant because the gc phase (`qd kill` + reconcile)
will need exactly this child-reaping anyway.

**GATE (pass-a) RECOMMENDATION: ACCEPT.** All 11 rows PASS; 3/3 boot budget
respected; mutation evidence non-vacuous; deferrals recorded with offline cover.
Pass-(b) corpus parity (dir-resolution / preflight golden / list-history golden)
remains the lead's at PINNED_TS_COMMIT before tagging `phase-a2`.



---

## LEAD REVIEW CORRECTION (sbr-pa2-lead, 2026-06-04 ~22:3x ET) — Row 4 verdict UPGRADED

QA's Row-4 caveat ("pre-accept seed suppressed the dialog; live answerer not
exercised") is **inverted**. Two deconfounding probe boots (unauthenticated,
zero API cost, jailed, belt held):

- **Probe 6** — settings.json `allowedChannels:["server:relay"]` ONLY, dangerous
  flag on, raw zmx (no qd): dialog APPEARS and blocks. `allowedChannels` is not a
  pre-acceptor.
- **Probe 7** — QA's row-4 seed EXACTLY (`.claude.json` `dangerouslyLoad
  DevelopmentChannels:true` + GrowthBook flags + trust; no settings.json), raw
  zmx (no qd, NO answerer): dialog APPEARS and blocks for 30s+ (PID file never
  written). With probe 3 (same key + dialog appeared) this eliminates the
  pre-accept hypothesis entirely.

Therefore QA's boot B — identical seed, launched through the BUILT qd — got past
the dialog because **the EventBootWaiter's delegated-consent answerer detected
and answered it, live**. A dismissed dialog is overwritten in-place (no
scrollback line is produced), which is why `zmx history` showed no trace —
scrollback absence is evidence of DISMISSAL, not non-rendering, for in-place TUI
frames.

**Corrected verdicts:**
- Row 4 (configured-flag boot + answerer): **FULL LIVE PASS** — dialog rendered,
  answerer fired (bounded single-shot), session reached idle, channels engaged.
  The "answerer live-exercise deferred" exclusion is WITHDRAWN.
- 5d verdict UNCHANGED and now live-proven: no settings/state pre-acceptance
  exists; the answerer is required and works against the real dialog.

**Jail-leak note (QA concern 1):** confirmed real for dialog-blocked boots in
QA's run; post-run sweep 2026-06-04 ~22:3x finds ZERO orphaned `claude --name
qdrg-*` processes (QA reaped theirs; lead probes 6/7 children died with their
wrappers). Carry: gc-phase must close teardown's claude-child gap (specific-PID
children of the wrapper, never patterns — L10); noted for the orchestrator.
