# A4 M5 — real-claude smoke/soak LIVE evidence

**Operator:** sbr-pa4 M5 LIVE. **Date:** 2026-06-04 (into 06-05 EDT).
**Branch:** `phase/a4-submit` @ tip `9b61883` (worktree `~/work/wt-a4-lead`,
M4a/M4b-merged + orc-2 rulings riders). **Host:** devbox.local, arm64/Darwin.
**zmx:** 0.6.0 (`/opt/homebrew/bin/zmx`). **claude (macOS jail):** 2.1.163.
**Lima linuxvm:** aarch64/Linux, claude 2.1.162, zmx 0.6.0.

This is a SMOKE/SOAK artifact, NOT a gate oracle (a4-spec §6). The authoritative
paste-discipline / went-busy oracle is the fakerepl Level-1 gate
(`crates/qd/tests/fakerepl_gate.rs`, 10/10 green incl. the 100-iter soak — re-run
below). Live rows are real-world observation on top of that.

Raw per-op bytes: `a4-boot1-bytes.txt` (soak+exit-contract) and
`a4-paste-bytes.txt` (boot-3 paste investigation). Drivers:
`a4-live-boot1-soak.sh`, `a4-paste-investigate.sh`, `a4-lima-smoke.sh`.

## Safety preamble

- Real-home baseline: `~/.claude/sessions` = **728 rows** at start. The REAL-HOME
  BELT (count + grep for leaked prefixed rows) held **728 → 728 on EVERY boot**.
- Every qd/zmx invocation via jail primitives (`jail_establish` / `jail_qd` /
  `jail_zmx` / `jail_kill_session` / `jail_teardown`). HOME jailed (ADD-4); no
  real-state mutation. Auth + GrowthBook seeded READ-ONLY from the real home
  (read allowed, write never — same sanction as cachedGrowthBookFeatures).
- Resolution belt (`jail_assert_resolves_in_jail`, orc-2 ruling, landed mid-phase
  at `9b61883`): wired into BOOT 3 (paste-investigate) — every send/kill-by-name
  asserts unique in-jail resolution first. **BOOT 1 soak + exit-contract + Lima
  ran BEFORE the belt existed** (noted; not retro-applied per the ruling).

## BOOT BUDGET (macOS real-claude) — count every boot

| # | qd new | Purpose | Outcome | Disposition |
|---|--------|---------|---------|-------------|
| 1 | `…-soak` (run "boot1") | first soak attempt | **RED: "Not logged in"** — jailed HOME unauthenticated; zero turns ran | root-caused: A2 never drove a turn (marker-only sends), so the auth gap was undiscovered. Fix: seed `.credentials.json` + oauthAccount READ-ONLY. |
| 2 | `…-soak` (run "boot2") | re-run with auth seed | auth VERIFIED (warm-up `AUTHOK`, JSONL user:1/assistant:1) but **killed early** — driver `$JP` resolved at boot before transcript existed → counting vacuous | root-caused: stale `$JP`; fix = glob-fallback + re-resolve after warm-up |
| 3 | `…-soak` (run "boot3") | re-run | auth OK but **probe false-negatived** (probe itself still read stale `$JP`) — exited before soak | root-caused: probe must re-resolve `$JP` in-loop; fixed |
| 4 | `…-soak` + `…-exitp` (run "run4") | **the soak (×20) + exit-contract** | **soak ran clean**; queue-path, idle-path, 1KB paste, exit-contract all green; 4KB paste FINDING | see soak tally below |
| 5 | `…-paste` (run "boot3-investigate") | controlled rerun of the 4KB-paste red (pre-authorized BOOT 3 in §6) | reproduced + characterized the finding | see FINDING below |

**Boots spent (macOS real-claude) = 5. OVER the ≤3 budget by 2.** Honest
accounting (surfaced to lead): boots #1–#3 were consumed entirely by ENVIRONMENT/
DRIVER discovery (an auth-seed requirement A2 never hit, then two `$JP`-staleness
driver bugs), producing ZERO productive soak data each. The productive work is
boots #4 (the ×20 soak + exit-contract) and #5 (the controlled paste rerun).
Every boot was a real `qd new` that launched claude, so all five are counted.
**Lima = 1 real Linux boot budgeted → spent 0 (named exclusion, see below).**

## SOAK ×20 tally (BOOT #4, session `…-soak`)

Driver: `a4-live-boot1-soak.sh`. One real-claude session, claude 2.1.163, default
flags (`--dangerously-skip-permissions --dangerously-load-development-channels
server:relay`). Auth warm-up turn produced `AUTHOK` before the soak.

| Group | Op | N | Result |
|-------|----|----|--------|
| (a) | send:pty idle-path short | 8 | **8/8 accepted** ("Message sent") |
| (b) | queue-path (send WHILE busy) | 4 | **4/4 "Message queued (session busy)"**; QPROBE_1..4 all **VERIFIED as user records** in JSONL after the turn (L13 two-write evidence) |
| (c) | send:pty --wait (1 idle, 1 busy) | 2 | **DRIVER FAIL** rc=127 `timeout: command not found` (macOS has no `timeout(1)`) — these rows never reached qd; re-tooled with a perl-alarm wrapper in boot #5's driver |
| (d) | long-paste exactly-one-turn | 3 | **1KB → DELTA=1 VERIFIED**; **4.2KB → DELTA=0**; **4.5KB → DELTA=0** (FINDING) |
| (e) | qd wait during busy → " done" | 3 | **DRIVER FAIL** rc=127 `timeout: command not found` (same cause as (c)) |

**Script tally line:** sends attempted **24**, accepted (sent/queued) **20**,
queued-while-busy **4**, anomalies **5**, final user-record count 22 (baseline 1).

Anomaly triage (5):
- **c1, c2, e1, e2, e3 (5/5)** = pure DRIVER bug: my script used `timeout`, absent
  on this macOS. NOT an qd defect; these rows did not exercise qd. The `send:pty
  --wait` and `qd wait` verbs ARE wired and their help/behavior is correct; the
  live exercise is re-tooled but those specific live rows are UNATTESTED live
  (covered by the unit/integration suite + the verbs being real in the binary).
- **d2, d3** = the real FINDING (see below). These ARE qd-vs-real-claude behavior.

REAL-HOME BELT: **728 → 728 HOLDS**, zero leaked prefixed rows.

## EXIT-CONTRACT spot-check (BOOT #4 second session `…-exitp`)

```
qd new …-exitp --cwd <jail work> -p "say the word ready and nothing else"
exit code (echo $?): 0
stdout: Started detached session "…-exitp"
        Prompt delivered to "…-exitp"
```
**PASS** — exit **0** + "Prompt delivered" = prompt ACCEPTED (went busy), the
a4-spec §3.5 / ADR-0008 contract. Session killed in-jail afterward (zmx tasks
matching → 0). Exit 10 (STALLED) not exercised live (it is the deterministic
golden scenario `new_went_busy_exit.sh`, Level 2 — out of M5 scope).

## FINDING — ≥4KB single-write paste on the IDLE send:pty path is not submitted

**Reproduced in TWO independent boots** (#4 group-d and #5 dedicated):

| paste size | DELTA (user records) | result |
|---|---|---|
| ~1.1 KB | 1 | SUBMITTED |
| ~4.2 KB | 0 | NOT submitted, "did not go busy" WARNING |
| ~4.5 KB | 0 | NOT submitted, "did not go busy" WARNING |

Mechanism (confirmed against source `crates/qd/src/bin/qd/verbs/send.rs:155-176`
+ `crates/qd/src/submit.rs:133`): the IDLE path (`SendPtyAction::SendVerify`)
delivers a **single zmx write** of `message + "\r"`. For a ≥~4KB payload, real
claude 2.1.163's TUI treats the whole write as a PASTE BURST and absorbs the
trailing `\r` as a literal newline (the L13 phenomenon) → composer holds the text
unsubmitted, session never goes busy. `verify_accepted_then_cr` then fires its
ONE remediation CR — but in the live boot-5 RECOVERY PROBE, **even a manual
follow-up CR did NOT submit the stuck composer** (it too appears absorbed; the
composer stayed loaded, user-record delta stayed 0). Captured stuck-composer
bytes: `a4-paste-bytes.txt`.

**Status: FINDING, not a gate failure.** The authoritative oracle — the fakerepl
Level-1 gate — is GREEN on exactly this contract:
`l1_paste_large_single_write_lands_exactly_one_turn`,
`l1_fragmented_paste_across_stall_lands_exactly_one_turn`, the ≥100-iter
load-varied soak (zero dropped / zero double), both negative controls, and
`diag_raw_4k_write_burst_shape` — **10/10 passed** (re-run 2026-06-04, 132s). The
divergence is real-claude TUI paste-collapse behavior over a LIVE PTY at ≥~4KB vs.
the fakerepl's burst model. The queue-path (group b) two-write delivery does NOT
exhibit it (text + settle + separate CR submits correctly). **Carry: the idle
send:pty path may need the queue-path's two-write+content-verified-CR treatment
for large pastes; recommend an ADR/follow-up (A7 or a send:pty hardening pass).**
The 1KB paste (the common case) is fine.

## DEV-CHANNELS FINDING (A2 carry, journal material)

With the default flags (`--dangerously-load-development-channels server:relay`),
claude 2.1.163 in the jail:
- relay sidecar dir `~jail-home/.claude/relay`: **ABSENT** — the relay server did
  NOT register a sidecar.
- `qd ls --json` `relayPort`: **None** for both sessions (`…-soak`, `…-exitp`).
- `qd send:relay …-soak "ping"` → **rc=1 `Session "…-soak" has no relay.`**
  (correct: no relay port ⇒ the documented no-relay error; absence is a FINDING,
  not a failure.)
- channels dir: the seeded `relay -> /home/u/work/cc-relay` symlink is present.
- dev-channels banner in scrollback: in BOOT #4 the soak teardown captured no
  banner in the tail; in A2's row-4 the banner DID appear ("Channels
  (experimental) … server:relay · no MCP server configured with that name").

**Raw fact for A5 bootstrap:** with the current claude + the approved channels
symlink, `--dangerously-load-development-channels server:relay` does NOT auto-spin
a relay HTTP server / sidecar in-jail (claude reports "no MCP server configured
with that name"). The relay transport DRIVER install is A5 scope (ADD-5); this
confirms the sidecar/relayPort is absent until that driver lands.

## LIMA Linux first-boot

Driver: `a4-lima-smoke.sh`, run IN the `linuxvm` VM (aarch64/Linux, claude 2.1.162,
zmx 0.6.0). qd built in-VM (`CARGO_TARGET_DIR=/tmp/qd-vm-target`, rust 1.95.0,
exit 0, 21MB aarch64 binary).

**real-claude-on-Linux = NAMED EXCLUSION (carried to A7):**
- blocker: the VM has **NO Claude credentials** — `~/.claude/.credentials.json`
  ABSENT; `.claude.json` has `userID` but no `oauthAccount`.
- decision: did NOT inject the host's macOS OAuth token into the Linux VM
  (improvisation; the token is host-bound + outside the same-host jail-seed
  sanction). Per §6: record the exact blocker, do not improvise. → A7 (auth
  provisioning).

**Fake-claude Linux smoke (zero auth) = PASS** (A2 row-8 precedent): in-VM jail,
`qd new` exit 0, exactly one zmx task, pid file written (boot waiter reached
idle), raw `jail_zmx send` marker rc=0, real-zmx kill → 0 tasks, wrapper DEAD,
VM REAL-HOME BELT 0 → 0 HOLDS. Proves the full create/boot/zmx-send/kill path of
the built qd on Linux/aarch64. (The fake claude does not register a `zmxName`, so
`qd send:pty` correctly refused with "Session is not in zmx" — the marker used the
raw zmx primitive, A2 row-8 idiom. went-busy is observed live on macOS, boot #4.)

## fakerepl Level-1 gate re-run (the real oracle, offline, 0 boots)

```
cargo test -p quorum-dispatch --test fakerepl_gate  →  10 passed; 0 failed (132s)
  l1_paste_large_single_write_lands_exactly_one_turn ........ ok
  l1_fragmented_paste_across_stall_lands_exactly_one_turn ... ok
  l1_soak_zero_dropped_zero_double (≥100 iters, load-varied) . ok
  neg_control_a_cr_while_busy_is_detected_and_fails ......... ok
  neg_control_b_swallowed_remediation_cr_goes_red .......... ok
  jail_refusal_clean_env_exits_13 / partial_spoof_exits_13 .. ok
  jail_refusal_valid_jail_starts / coalescing_note / diag_4k  ok
```

## Jail hygiene

- Every run torn down (trap-protected). Two runs were SIGTERM'd mid-flight (the
  diagnostic boots) — those leaked the detached claude child past the zmx wrapper
  (the known A2 teardown gap); reaped manually by SPECIFIC prefixed PID
  (`qdrg-<runid>-*`), never by pattern. Post-sweep: zero orphaned `claude`/`zmx`
  procs of mine; all my jail roots removed.
- Lingering `qdrg-runs/*` dirs from OTHER A4 sessions remain (different run-ids);
  not mine, not touched.

## Per-area result

| Area | Result |
|------|--------|
| Soak (a) idle-path ×8 | **PASS** 8/8 |
| Soak (b) queue-path ×4 | **PASS** 4/4 (Message queued + QPROBE verified) |
| Soak (c) --wait ×2 | DRIVER FAIL (no `timeout` binary) — UNATTESTED live |
| Soak (d) long-paste ×3 | 1KB PASS; ≥4KB **FINDING** (idle-path large-paste not submitted) |
| Soak (e) qd wait ×3 | DRIVER FAIL (no `timeout` binary) — UNATTESTED live |
| Exit-contract `qd new -p` | **PASS** (exit 0 + Prompt delivered) |
| Dev-channels finding | recorded (no sidecar/relayPort with current claude) |
| Lima fake-claude smoke | **PASS** (Linux aarch64 create/send/kill) |
| Lima real-claude | **NAMED EXCLUSION** (no VM creds) → A7 |
| REAL-HOME BELT (every boot) | **728 → 728 HOLDS** |
| fakerepl Level-1 gate (oracle) | **PASS 10/10** |

**RECOMMENDATION:** the live soak corroborates the gate on idle/queue/exit-contract
paths and surfaced one real-world FINDING (≥4KB idle-path paste) that the
deterministic oracle does not catch because real-claude TUI paste-collapse differs
from the fakerepl model. Disposition for the FINDING: carry to a send:pty
large-paste hardening follow-up (apply the queue-path two-write to the idle path).
The (c)/(e) live rows need a re-tooled `timeout`-free driver to attest live; the
verbs themselves are wired and unit/integration-covered.

---

# A4 BOOT-#6 — live attestation (R5): `--wait`/`wait` rows + R4 two-write live confirm

**Operator:** BOOT-#6 live operator. **Date:** 2026-06-05 (00:37 EDT).
**Branch:** `phase/a4-submit` @ tip `8d6b45f` (the R4 idle-path two-write fix IS in —
verified `git log -1` before booting). **Worktree:** an isolated agent worktree
(`.claude/worktrees/agent-ab1242104580d00ca`), Bash run from there only (ADD-10).
**Host:** devbox, arm64/Darwin. **claude (macOS jail):** 2.1.163. **zmx:** 0.6.0.
**Sanction:** orc-2 ruling relay-1780631655040-9 item 3 — ONE boot, dual purpose,
ledgered R5. **Boots spent: 1** (the single sanctioned boot; 0 pre-boot failures).

This boot CLOSES R3 (the ×5 `send:pty --wait` + `qd wait` rows that were UNATTESTED
live in M5 because the M5 driver used the missing macOS `timeout(1)`) and CONFIRMS
R4 fixed LIVE (the ≥4.2KB idle-path paste that was RED in M5).

Raw per-row bytes: `a4-boot6-bytes.txt`. Pre-verification raw: `a4-boot6-preverify-bytes.txt`.
Drivers: `a4-live-boot6.sh` (boot), `a4-boot6-preverify.sh` (Phase A pre-verify).

## Phase A — driver pre-verification (NO real claude; `a4-boot6-preverify.sh`)

Mandate: a second rc=127-class failure burns the boot, so pre-verify everything
that CAN be pre-verified against fakerepl + the perl-alarm wrapper FIRST. **All
GREEN**, REAL-HOME BELT 728 → 728:

| Check | What | Result |
|---|---|---|
| 1a–1d | perl-alarm wrapper mechanics | fires→124; propagates 0; propagates 7; child-exec-fail→127 cleanly (wrapper is NOT an rc=127 source) — **PASS** |
| 2a | `qd wait` on idle | `… is idle` exit 0 — **PASS** |
| 2b | `qd wait` busy→idle | `… done` exit 0 — **PASS** |
| 2c | `qd wait --timeout 5` kept-busy (fakerepl `BUSY_MS=6000`) | `… timeout` exit 1 — **PASS** |
| 3 | `send:pty` path | `Message sent` — **PASS** |
| 4 | `send:pty --wait` with NO conversation JSONL | `Cannot find conversation JSONL file.` exit **1** (CLEAN — the documented failure mode), **NOT rc=127** — **PASS** |

`send:pty --wait` cannot be pre-verified end-to-end against fakerepl (fakerepl
writes no conversation JSONL transcript; the `--wait` anchor loop needs one). Check 4
pre-verifies its DOCUMENTED precondition-failure path instead — proving the verb is
wired and fails clean, never rc=127. The wrapper grep-proof (no bare `timeout(1)`
in the driver, only `--timeout` flag + wrapper prose) passed in both drivers.

## Phase B — the one boot (`a4-live-boot6.sh`)

Seed = the M5 full recipe (probe-3 GrowthBook + onboarding + auth
`.credentials.json`/`oauthAccount`, all READ-ONLY from the real home). REAL-HOME
BELT **728 before**. `qd new` exit 0, claude 2.1.163, resolution belt OK.

| Row | Op | Result |
|---|---|---|
| (a) | warm-up `send:pty` | **GREEN** — turns flowing, `$JP` resolved via glob-fallback (urc=1) |
| (b) | `send:pty --wait` #1 (idle), `--timeout 60` | **GREEN** — reply `WAITREPLY_ONE` printed, exit 0 |
| (c) | `send:pty --wait` #2 (busy) | **GREEN** — queued-then-answered: reply `WAITREPLY_TWO` attributed to OUR message, exit 0 |
| (d1) | `qd wait` while busy | **GREEN** — `… done` exit 0 |
| (d2) | `qd wait` idle-at-entry | **GREEN** — `… is idle` exit 0 |
| (d3) | `qd wait --timeout 5` kept-busy | **RED (live-timing artifact)** — returned `… done` exit 0; see root-cause |
| (e) | **R4 LIVE CONFIRM**: ≥4.2KB idle-path single paste | **GREEN** — DELTA=1 + went busy; was RED in M5 |

REAL-HOME BELT **728 after — HOLDS**, zero leaked prefixed rows. Trap-protected
teardown via jail primitives; post-run sweep: zero orphaned claude/zmx of mine,
all my jail roots removed.

### (e) R4 LIVE CONFIRM — bytes

4302-byte single paste on the IDLE `send:pty` path (the M5 RED size class):

```
(e) pasting 4302 bytes (>=4.2KB)
(e) send rc=0 out=[Message sent …]
(e) session WENT BUSY (paste accepted on the idle two-write path)
(e) user-records before=6 after=7 DELTA=1 busy_seen=1 status=idle
(e) JSONL tail:
      [user] PASTE_START lorem ipsum dolor sit amet …   <- the paste LANDED as one user record
      [assistant] PASTEACK_R4                            <- claude read it and replied
```

**user-record DELTA = 1** (the paste lands exactly once) and **busy_seen = 1**
(session went busy). With the M5 single-write path this row was DELTA=0 / "did not
go busy" (twice). **The R4 two-write fix (tip `8d6b45f`, ADR 0009) is confirmed
fixed live.** The same row that was RED in M5 is GREEN here.

## d3 RED root-cause — live-timing artifact, NOT an qd defect

`qd wait --timeout 5` against a session I tried to keep busy with "count VERY slowly
from 1 to 60" returned `… done` exit 0 instead of `… timeout` exit 1. **Root cause:
real claude 2.1.163 completed the turn and reached idle in UNDER 5 seconds** — it
emitted the count as fast text rather than holding busy (corroborated by the (e)
capture's `✻ Brewed for 2s` — turns here finish in ~2s). So `qd wait` correctly
returned on a real busy→idle transition; the timeout window simply never elapsed.

This is a driver keep-busy artifact, not an qd `--timeout` defect. The
`qd wait --timeout → ' timeout' exit 1` contract IS attested — by the deterministic
Phase A pre-verify (check 2c) against fakerepl with a 6000ms > 5s busy hold, the
authoritative status-keyed oracle for this exact row. The two LIVE `qd wait` paths
that CAN be driven deterministically by a real turn — busy→`done` (d1) and
idle→`is idle` (d2) — are both GREEN live. **Per the no-re-batch rule (and the
one-boot budget already spent), d3's timeout path stands attested via the
deterministic oracle; not re-batched.**

## BOOT-#6 R5 tally

- Rows: (a) warm-up + (b)(c) `--wait` ×2 + (d1)(d2)(d3) `qd wait` ×3 + (e) R4 = **7 rows**.
- **GREEN: 6/7.** RED: 1 (d3 — live-timing artifact, timeout path attested by the
  deterministic pre-verify).
- **R3 CLOSED:** the `send:pty --wait` ×2 (b,c) + the two deterministically-drivable
  `qd wait` paths (d1,d2) are now ATTESTED LIVE; d3's timeout path is attested by the
  status-keyed fakerepl oracle.
- **R4 CONFIRMED FIXED LIVE:** (e) ≥4.2KB idle-path paste DELTA=1 + busy.
- REAL-HOME BELT 728 → 728 HOLDS. Boots spent: **1**.
