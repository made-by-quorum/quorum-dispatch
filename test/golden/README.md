# Golden-master oracle harness (Phase 0b)

A recorded, regression-detecting golden-master oracle for the qd→Rust rewrite.
Two layers: **byte-golden fixtures** (captured PTY/JSON traces, normalized,
compared) and **property/invariant liveness tests** (synthetic stress fixtures
asserting invariants, not byte-equality).

**Status: PART 1 (pin-independent).** The harness, scripts, ADRs, synthetic
layer-2 fixtures, coverage-matrix scaffold, LESSONS ledger, scenario scripts, and
the mutation-test teeth are built and proven on synthetic + dry-run data. **PART 2
(real corpus recording) is BLOCKED** until a pinned TS commit is chosen and
supplied to `record.sh` as `PINNED_TS_COMMIT` (the TS fix-wave must settle first,
so the oracle does not bake in unfixed behavior). See `coverage-matrix.md`.

## The jail contract (ABSOLUTE)

The org's REAL TypeScript qd runs on this machine (brano). This harness MUST be
invisible to it. `lib/jail.sh` establishes a per-run hermetic environment — own
**HOME** (load-bearing: TS qd keys its registry on `homedir()`, ADD-4), QD_HOME,
ZMX_DIR, XDG_*, TMPDIR, relay port + socket prefix — all under a per-run temp dir.
It uses POSITIVE sandbox detection (it REQUIRES every isolation var to resolve
under the run dir) and FAILS CLOSED. Kill/gc refuse any session name not under the
`qdrg-<runid>-` prefix; raw-kill only registered PIDs. Teardown reaps the jail's
own prefixed sessions (qd gc + zmx kill) so a detached jailed daemon never leaks.

`selftest/test_jail_refusal.sh` proves the refusals fire (production paths, bare
names, unregistered PIDs, Lima destructive gate fail-closed on brano).

## Layout

```
lib/jail.sh          hermetic env + production-path REFUSAL (ported from qd-qa safety.sh)
lib/normalize.sh     normalizers (timestamps/pids/paths/runids/port/ansi-chunks) — ADR 0003
lib/compare.sh       comparator classes (byte-exact + semantic invariants) — ADR 0004
lib/check_python.sh  python3 floor enforcement (>= 3.6, ADR 0002)
recorder/record_pty.py  PTY recorder (ported from spike: capture + inject + sigwinch)
verify.sh            the asserter: normalize -> compare -> per-case TIMEOUT BUDGET
record.sh            Part-2 recorder wrapper (GATED: refuses without PINNED_TS_COMMIT)
scenarios/           one script per corpus entry (budget + class + fixture path)
fixtures/layer2/     synthetic Part-1 fixtures: ansi-burst, sigwinch-storm, dirty-state
dryrun/              Part-1 throwaway captures, ALL marked DRYRUN-NOT-ORACLE
mutation/            mutation-test teeth: inject divergences, assert verify catches all
selftest/            unit tests (normalize, jail refusal, timeout budget, record gate)
coverage-matrix.md   every required surface, UNticked until Part 2
```

## How to verify (Part 1)

```sh
# All self-tests (jail refusal, normalizers, timeout budget, record gate):
test/golden/selftest/run_selftests.sh

# Synthetic layer-2 fixtures end-to-end:
test/golden/fixtures/layer2/run_layer2.sh
./scripts/build-lock.sh cargo test -p golden        # dirty-state JSON corpus

# The mutation teeth (prove the oracle bites on synthetic data):
test/golden/mutation/run_mutation.sh

# Dry-run scenarios against CURRENT TS main (DRYRUN-NOT-ORACLE evidence):
test/golden/dryrun/run_dryrun.sh
```

## How to record (Part 2 — GATED)

Recording golden expectations is BLOCKED until `PINNED_TS_COMMIT` is set. When the
pin lands:

```sh
PINNED_TS_COMMIT=<sha> ZMX_VERSION=0.6.0 \
  test/golden/record.sh --scenario test/golden/scenarios/<entry>.sh
```

`record.sh` writes the RAW capture AND the NORMALIZED expectation side by side
(`fixtures/<corpus>/raw/` + `fixtures/<corpus>/normalized/`) and stamps
`RECORDED-FROM`. It REFUSES to run without a pin (fail-closed, exit 70).

## Failure taxonomy (verify.sh)

| Exit | Meaning |
|------|---------|
| 0 | pass |
| 1 | DIFF — comparison failed (bytes / invariant) |
| 2 | DEADLINE — scenario exceeded its timeout budget (LIVENESS regression) |
| 3 | JAIL — jail refused to establish (fail-closed) |
| 64 | USAGE — bad invocation / python floor unmet |

A DEADLINE is deliberately distinct from a DIFF: a hang that would *eventually*
produce matching bytes is still caught.

## Provenance: RECORDED-FROM / MATCH-PROOF and platform siblings

Each corpus dir carries provenance metadata:

- `RECORDED-FROM` — the canonical stamp (pin, zmx version, host, and — for
  stub-backed rows — `stub_sha256` + `stub_version`).
- `MATCH-PROOF` — the double-record proof (sha256 of both raws, the matched
  normalized form, the normalizer, the scenario, and the stub).

**Commingled dual-platform corpora** (e.g. `zmx-dir-resolution` holds a macOS
recording `resolution.txt` AND a Linux recording `resolution-linux.txt`) cannot be
attested by a single bare stamp — a bare `RECORDED-FROM` can only describe ONE
host. Such corpora therefore carry **per-platform provenance siblings**:
`RECORDED-FROM.<platform>` + `MATCH-PROOF.<platform>` (e.g. `.macos`, `.linux`).
The bare `RECORDED-FROM`/`MATCH-PROOF` is the canonical (conventionally
first-platform) one; the siblings carry each platform's own pin, host, stub_sha,
and the match-proof hashes for THAT platform's files.

These siblings are **load-bearing, not documentary**:

1. **Admission** (`fixture_admit.sh` in `FA_PLATFORMS="macos linux"` mode) verifies
   each platform set: pin match + secret-scan + **hash-pairing** (the
   `MATCH-PROOF.<platform>` normalized/rawA/rawB sha256 must match ACTUAL staging
   files), so a platform's proof is provably about real fixture files. A whole
   commingled corpus is thus auditable through ONE admit call.
2. **Replay** (`verify.sh`) resolves the platform-appropriate sibling by host
   (`uname`: Linux→`.linux`, Darwin→`.macos`, else the bare stamp) and asserts the
   INSTALLED stub's sha matches the stamped `stub_sha256` — so a replay can never
   silently drive a different stub than the golden was recorded against (R1).

**Re-stamp without re-record** (R1 precedent, orc-2 2026-06-05): a stub edit that
is provably DORMANT (env-gated + default-byte-identical + replay-verified: the
most-stub-sensitive rows byte-match the golden under the edited stub with seams
unset) may have its `stub_sha256`/`stub_version` stamps updated WITHOUT re-recording
(evidence: `fixtures/.restamp-evidence.txt`). ANY edit touching default-path
behaviour = re-record the backed rows, no exceptions. Extension (orc-3
2026-06-05, A4 pass-(b)): after a SANCTIONED default-path stub change, each
non-re-recorded stub-backed row is replayed under the committed new stub and
sha-compared against its golden — byte-match → re-stamp citing the ruling;
mismatch → the row joins the re-record set (evidence:
`dryrun/passb-restamp-evidence.txt`, driver `dryrun/passb-restamp-replay.sh`).

**Lima note (in-VM pinned-TS clones):** `git clone --local` hardlinks cannot
cross the virtiofs mount boundary ("Invalid cross-device link"), and a `file://`
clone misses a pin that is unreachable from the source's local heads (the pin
may live only under a remote-tracking ref — `--local` works on the host because
it copies the whole object store). To get a pin-verified clone into the VM:
mint it on the host with `prep_pinned_ts.sh`, tar it (sans `node_modules`)
through the mounted path, untar in VM `/tmp`, re-verify `git rev-parse HEAD`
in-VM, and `bun install` there (platform-native deps).

**Lima prerequisite (build-lock):** `record.sh` invokes the HOST build-lock at
`$REPO_TOP/scripts/build-lock.sh` (the host-wide recording mutex, G3). For an
in-VM RECORDING leg the script must exist at that path inside the VM checkout
too. The light in-VM REPLAY leg (`verify.sh --scenario`/`--replay`, no recording)
does NOT touch the build-lock, so a replay-only VM (the W4.2 pattern) needs only
the test/golden tree + a pinned-TS clone, not the full `scripts/` tree.
