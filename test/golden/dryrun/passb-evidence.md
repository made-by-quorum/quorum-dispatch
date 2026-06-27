# A4 pass-(b) closure replay — evidence (sbr-pa4-lead2)

Date: Fri Jun 5 08:53 EDT 2026 (system `date`; see journal note on the clock
anomaly vs prior entry stamps). SUT: Rust `qd` built from main @ 959dcbd
(debug, `scripts/build-lock.sh cargo build -p qd -p fakerepl`, worktree
`~/work/wt-a4-passb`). Verification basis: LOCAL runs only (GitHub Actions
down — billing; Pete ruled keep-going). All replays serial (host memory
pressure WARN). Jail: rule 9 + ADD-4 hermetic env via lib/jail.sh throughout.

## Row results (verify.sh replays, TMPDIR=/tmp — see finding F2)

| Row | Class | Result |
|---|---|---|
| exit-codes | exit-code | PASS (rc=0) |
| ls-info-json (exit-0 leg of the exit-code row) | byte-exact | PASS (rc=0) |
| new-session-trace (boot-readiness EVENT) | boot-readiness-event | **DIFF** — F1 |
| send-pty-paste-burst | semantic-submit-discipline | **DIFF** — F1 cascade |
| relay-health | byte-exact (contract) | **DIFF** — F1 cascade; 3/4 contract lines byte-identical, only `ls-join relay_joined_rows/has_relayPort` differ (0 vs 1) |
| new_went_busy_exit.sh Level-2 (canonical `bash $SCN` invocation, A4_RUN_LIVE path) | exit-contract | **7/7 PASS** at 959dcbd |
| relay_contract.rs (W2-adjacent re-check) | cargo test | **11/11 PASS** |

## F1 — stub dialog-text fidelity gap (root cause of all three DIFFs)

- `lib/stub_claude/stub_claude.py` (main(), citation #2) renders its boot popup
  as `"  Development channels\r\n  Press Enter to continue\r\n> "` and BLOCKS
  on stdin for a dismiss-Enter BEFORE writing the PID file.
- The Rust boot waiter (ADR 0005 dialog-free boot, sanctioned divergence)
  answers ONLY content-matched named dialogs: marker `"Enter to confirm"` +
  match text `"WARNING: Loading development channels"` (boot.rs named_dialogs(),
  text captured from the REAL claude dialog, A2 2026-06-04 journal).
- The stub's popup text matches NEITHER → detect_dialog → NoDialog → ZERO
  keystrokes (exactly the ADR 0005 §2 contract: an unmatched dialog is never
  answered) → stub blocks forever → PID file never appears → boot-readiness
  row red; paste-burst red (no turns drivable); relay-health red ONLY at the
  ls-join line (join needs the registry row the blocked stub never wrote —
  sidecar shape + /health + POST /message all byte-match through the Rust
  client).
- TS recordings were green because TS's blind-Enter loop dismisses ANY text.
  The recorded EVENT expectations are correct; the STUB does not say what the
  real dialog says. Engine NOT at fault: live R5/R6/R7 real-claude boots green
  (pid file + turns), and Level-2 fakerepl 7/7 green at 959dcbd (this run).
- Diagnostic chain: passb-diag-paste-burst.sh (scn-out all-zero capture),
  passb-diag-boot.sh / passb-diag-argv.sh (stub never invoked pre-F2 fix; then
  blocked post-F2 fix), passb-diag-row.sh (full jail snapshots: stub alive w/
  sidecar fork, no sessions/ or projects/ dirs ever created under jail HOME;
  real-home belt checked — no sbrg files in /home/u/.claude/sessions, 735
  entries baseline). DEV-TIME evidence, uncommitted jail state reproduced on
  demand.

## F2 — jail-base path length vs the Unix socket cap (environmental)

- With this session's shell `TMPDIR=/var/folders/.../T/` the jail base is ~82
  chars; zmx derives a session-name cap from the ~104-byte socket-path limit →
  cap ~20 bytes < the 27-byte `sbrg-<runid>-*` names → `zmx run` prints
  `error: session name is too long` **but exits 0**; qd's I6 Bug-D scan then
  correctly reports NotAttachable. Engine behaves per contract; zmx's
  error+exit-0 is a quirk worth an upstream note.
- Workaround used (replays): invoke with TMPDIR=/tmp (jail base 38 chars) —
  matches how prior recordings ran.
- Residual marginality: verify.sh-wrapped SELF-JAILING scenarios double-jail
  (`/tmp/sbrg-runs/<id>/tmp/sbrg-runs/<id>/zmx`, ~74 chars) → socket path
  ~108 bytes with 20-digit runids → fails; passes with shorter pids/RANDOM.
  The pass-(a) 7/7 Level-2 runs were marginal on runid digit count. Canonical
  invocation (`bash $SCN`, single jail — what run_selftests.sh does) is safe.
  Harness hardening candidate (A7): jail_establish fail-closed when the
  prospective socket path exceeds the cap; pin a short jail base.

## Disposition

STOP per brief on the three red rows: no fixture edits, no re-batch. Resolution
options for the orc ruling: (a) stub edit to render the REAL captured dialog
lines (touches default-path behavior → re-record the stub-backed rows per the
README re-stamp rule) — recommended; (b) named replay-time exception. Soak
ledger UNCHANGED (N=42 — no real-claude rows run; fakerepl/stub boots are not
ledger rows).

---

# RESOLUTION (post-rulings, same day) — orc-3 option (a) + F3 sanction extension

Rulings executed: relay-1780664243755-2 (option a, A4 executes), rider
relay-1780664284225-3 (strings-only scope + membership check),
relay-1780664987618-6 (F3 timestamp-shape sanction + re-stamp mechanism +
engine finding routed to A1 pass-(b)). 0b lead review: F1 verdict WITHIN
SANCTION (relay-1780665036185-7); F3 criteria (deterministic constants +
ISO-form membership check) honored. W1 merged first (main 048df06, merged into
this branch) so proofs cite the post-W1 normalizer.

**Stub fixes (1.5.1 → 1.7.0):** F1 = popup text replaced with the REAL captured
dev-channels dialog verbatim (boot.rs DEV_CHANNELS_TAIL). F3 = PID-file
startedAt/updatedAt now the deterministic epoch-ms constant FIXED_TS_MS
(1767225600000); JSONL timestamps stay ISO (real transcript shape).

**Membership checks (both, non-vacuous, grep -a over all 104 committed fixture
files):** old dialog strings = ZERO hits; old ISO timestamp form = ZERO hits.
Re-record set = exactly the 3 boot-blocked rows.

**Re-record (pin 8c59ec4, prep-verified clone, TMPDIR=/tmp):** new-session-trace,
send-pty-paste-burst, relay-health — each double-recorded + admitted via
fixture_admit (record.sh, fresh jails). Replay vs the RUST binary (959dcbd
crates; W1 is harness-only — `git diff 959dcbd..048df06 -- crates/` empty):
ALL THREE PASS through verify.sh incl. the stub-provenance belt.

**Re-stamp pass (ruled mechanism, R1-precedent extension) — the 5 non-re-recorded
stub-backed rows:** fresh-jail replay under committed 1.7.0, record.sh-identical
normalize, sha-compare vs committed golden (passb-restamp-replay.sh →
passb-restamp-evidence.txt): attach-detach-reattach, build-claude-cmd, history,
zmx-dir-resolution(.macos), ALL BYTE-MATCH on brano; zmx-dir-resolution(.linux)
BYTE-MATCH in-VM (Lima sbtest, jail rooted /run/user/501, pin-verified clone
transferred from the brano prep clone — git-local hardlinks cannot cross
virtiofs; rev-parse re-verified in-VM). Stamps updated (stub_sha256 +
stub_version + restamped= line citing the ruling) in all 12 RECORDED-FROM/
MATCH-PROOF files. Confirmation: verify.sh PASS on all 4 macOS rows + the
Linux row in-VM (provenance belt green against 1.7.0).

**Teeth after the change:** run_selftests.sh ALL PASSED; mutation synthetic
10/10; mutation REAL 27/27 vs the re-recorded corpus.

**Engine finding (NOT fixed here, ruled to A1 pass-(b)):** Rust whole-row serde
drop on a wrong-TYPED registry field; named in the coverage-matrix A4
divergence table until fixed; W4 dirty-state corpus row noted.
