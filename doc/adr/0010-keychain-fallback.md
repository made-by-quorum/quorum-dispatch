# ADR 0010: locked-keychain auto-fallback to the file backend — a sanctioned divergence that fixes a TS headless bug

> **Disambiguation:** if a `test/golden/` artifact (scenario header, stub
> citation table, sanctioned-divergence discussion) sent you to "ADR-0010", it
> means the SANCTIONED-DIVERGENCES ADR, renumbered to
> [`0011-sanctioned-divergences.md`](0011-sanctioned-divergences.md) at PR #24.
> THIS document is the A5 secret-store keychain-fallback ADR.

**Status:** Accepted (A5; orc-2 ruling relay-1780637432033-2 — design APPROVED
with the rider below)
**Date:** 2026-06-05

## Context

The TS `qd config set` is **broken in headless / agent contexts**. The inbox bug
(`doc/inbox/2026-06-04-config-set-keychain-headless.md` in the TS repo, filed
2026-06-04 by qd-rust-plan) captured the live failure:

> **Two-arg form** (`qd config set openrouter-key <value>`): keychain backend
> selected, then write fails: `keychain write failed (exit 36) — security:
> SecKeychainItemCreateFromContent (<default>): User interaction is not
> allowed.` The keychain probe says "available" but the keychain is LOCKED in
> headless contexts (agent sessions, SSH), so every write fails after backend
> selection.

The root cause: the TS keychain probe (`realKeychainAvailable`,
`0d0fa9e:src/secrets.ts:321-330`) checks only that the `security` binary is
*invocable* — it does NOT check that the keychain is *unlocked*. On macOS, an
agent/SSH session inherits a locked login keychain; `security
add-generic-password` then fails with OSStatus **-25308**
(`errSecInteractionNotAllowed`), whose CLI text is `User interaction is not
allowed`. The TS has no fallback, so the secret cannot be stored at all — the
documented workaround is `QD_SECRET_BACKEND=file qd config set <key> <value>`.

The inbox bug explicitly routes the fix to the A5 Rust port: "the Rust port must
implement locked-keychain fallback, and its gate should include a
headless-context config-set test."

## Decision

When the keychain backend is **SELECTED** (the macOS+`security` default — NOT
when `QD_SECRET_BACKEND=keychain` is set) and a `security` operation hits the
locked / no-interaction signature, retry the operation on the **file backend**
and print a one-per-process notice.

**Detection signature (string-primary).** Fallback fires if and only if the
`security` invocation's **stderr contains the literal string `User interaction is
not allowed`** — the documented `errSecInteractionNotAllowed` text, and the exact
string the inbox bug captured live. The live exit code observed was **36** (the
inbox quotes `keychain write failed (exit 36)`); this ADR records exit-36 as a
**corroborating live observation, NOT an asserted contract** — the code is never
consulted in the detection path. The G-C3 PATH-shim fake `security` emits the
string *and* exit 36, mirroring the live capture, but only the string is the key.

**Probe-on-failure, not probe-on-every-op.** Detection is failure-driven: the
happy path pays zero overhead and triggers no extra keychain prompts. A
`security show-keychain-info` pre-probe was **REJECTED** — it can itself trigger a
UI prompt in some states and adds a per-op subprocess.

**Env-forced keychain NEVER falls back.** `QD_SECRET_BACKEND=keychain` is an
explicit operator demand for the OS-encrypted backend; a silent downgrade to a
weaker file copy would betray that intent. Under a lock, the env-forced **SET**
path fails **loud (exit 1)** with an actionable message (unlock the keychain or
use `QD_SECRET_BACKEND=file`). This is the security-conservative reading.

**Env-forced keychain + locked on a GET — diagnostic-stderr-only divergence
(orc-2 ruling `relay-1780639217973-4`, "middle path c").** A GET cannot fail
loud the way a SET does without breaking presence-probing scripts (a great many
operators run `qd config get <key>` purely to branch on set/not-set). So under
env-forced `QD_SECRET_BACKEND=keychain` + the detected locked signature on a
GET:

- **stdout and exit are UNCHANGED — exact TS parity.** The value resolves to
  None, which the caller maps to `<key>: not set.` with **exit 0**. Presence
  probes stay unbroken.
- **BUT one attributable stderr diagnostic line is emitted** when the null is
  attributable to the detected-locked signature (and only then), verbatim:

  > `warning: keychain is locked — a key may exist but is inaccessible (QD_SECRET_BACKEND=keychain is env-forced; unlock or unset to use fallback).`

- **Once per process.** The line is gated by its OWN once-per-process flag
  (`locked_diag_emitted`, separate from the fallback notice's flag) so that
  `secret_backend_info`'s per-key get sweep — which probes every known key —
  prints the line ONCE, not once per key. This is the "ONE attributable stderr
  diagnostic" the ruling specifies.
- **Richer resolve API.** `resolve_secret` now returns a `locked: bool` that is
  true ONLY on this env-forced + locked arm, so callers (survey, M5) can
  distinguish ABSENT from INACCESSIBLE in their own reporting. It is `false` in
  every other outcome, including the non-env-forced fallback (there the value is
  read from the file, so it is accessible, not locked).

**Rationale / divergence class.** A silent null conflates ABSENT with
INACCESSIBLE for an operator who *explicitly pinned* the backend — a TS
diagnostic deficiency we deliberately do NOT reproduce (ADD-9a:
working+diagnosed > silently-identical). This is a **named divergence,
diagnostic-stderr-only**: stdout and exit keep TS parity; only an additive
stderr line is new. It is corpus-invisible (keychain rows are daytime-deferred).
The SET-under-env-forced-lock path already fails loud (unchanged by this ruling)
and is verified, not modified.

**Non-signature `security` failures keep TS semantics.** `get` on a non-zero
exit (item not found) is still "not set"; a `set` failure carrying any *other*
stderr surfaces loud as `keychain write failed (exit N) -- <stderr>`. Only the
no-interaction signature triggers fallback.

**Per-operation fallback covers INFO reads too.** `secret_backend_info`'s
keys-set enumeration does a per-key `get`; under a lock each `get` hits the
signature and falls back to a file read. Consequences for `qd config path`:

- The `Backend:` line reports the **SELECTION** (`keychain`) — that is the host's
  configured tier, and reporting `file` would hide the locked-keychain reality.
- The `Keys set:` line reports the **EFFECTIVE** state — keys read from the
  fallback file. The fallback notice prints **once per process**.

**`config set` success line reports the ACTUAL storing backend.** When fallback
fires, the success line is `Stored <key> (backend: file)` — truthful, because the
value really landed in the file. (The TS reports the storing backend; lying
`keychain` would be both false and a worse divergence than the additive notice.)

## Rider (orc-2, ruling relay-1780637432033-2) — security posture

This rider is MANDATORY per the orc-2 design approval.

**(a) File-backend permission posture.** The fallback file backend enforces
`chmod 600` on **every write** — both create and update — via the
`write_secrets_table` chmod call (`0d0fa9e:src/secrets.ts:131-133`, ported in
`crates/qd/src/secrets.rs`). The production `write_file` closure additionally
creates the file `O_CREAT | mode 0600` up front, so there is no window where the
file exists group/other-readable. This is asserted in the gate rows G-C1
(headless file round-trip) and G-C3 (the locked-keychain fallback write), and in
the unit rows `file_set_*_chmods_600_*` and
`fallback_selected_keychain_locked_set_writes_file_and_chmods_600`.

**(b) Threat-model delta (explicit).** The fallback trades secret-at-rest
strength for headless availability, and the trade is signposted by the notice
line. The delta:

| | keychain-at-rest | file-at-rest (the fallback) |
|---|---|---|
| Storage | OS-encrypted keychain DB | plaintext in `~/.quorum/dispatch/config.toml` |
| Access gate | keychain unlock (login password / Touch ID) | filesystem ACL only (`0600`, owner-only) |
| At-rest protection | encrypted; useless without unlock | readable by anyone who can read the file as the owner (root, a compromised process running as the user, an unencrypted-disk image, a backup) |

**The file backend is WEAKER than the keychain backend.** `0600` keeps the secret
off other local users and off group/other, but it is plaintext on disk with no
unlock gate. The fallback exists because the alternative — the TS status quo — is
that the secret **cannot be stored at all** in a headless context, which pushes
users to the strictly-worse `export OPENROUTER_API_KEY=...` in shell rc files
(world-history, often committed). The notice line
(`qd config: keychain locked (headless?) — falling back to file backend
(~/.quorum/dispatch/config.toml).`) makes the downgrade visible at the moment it happens, and
env precedence (`OPENROUTER_API_KEY` always wins) means an operator who wants the
secret to live only in process memory can still do so.

## Consequences

- **Named divergences from TS** (A5 spec §9, finalized in the M6 matrix):
  1. Locked-keychain auto-fallback-to-file (this ADR). TS has no fallback; its
     headless `config set` is the inbox bug. Includes: the `set` success line
     reports the ACTUAL storing backend under fallback; the info-read
     (path / keys-set) per-op fallback; AND the env-forced + locked **GET**
     diagnostic-stderr-only line (orc-2 ruling `relay-1780639217973-4`) —
     stdout/exit stay TS-parity, one once-per-process stderr `warning:` line is
     added, and `resolve_secret` gains a `locked: bool` so survey/M5 can
     distinguish absent from inaccessible.
  2. `config set` non-TTY no-value → loud exit 1 (A5 §3.3; TS attempts the
     hidden prompt and breaks under zmx — inbox bug #1). Message:
     `qd config set: stdin is not a TTY; pass the value as an argument or use
     QD_SECRET_BACKEND=file qd config set <key> <value>.`

- The notice is **additive output** vs TS — golden shapes that compare `config
  set` / `config path` stderr under a locked keychain account for it.

- The `select_backend` invalid-value behavior is matched to TS exactly: an
  `QD_SECRET_BACKEND` value other than `file` / `keychain` is **silently ignored**
  (falls through to the platform default, no stderr notice). Matrix note recorded.

- **Deferred:** a real (unlocked) keychain round-trip is a supervised DAYTIME
  check (gate row G-C5, ledgered) — the overnight gate never invokes the real
  `security` binary (test discipline: injected exec / PATH-shim only).

## Alternatives considered

- **`security show-keychain-info` pre-probe** (the inbox's first suggestion) —
  REJECTED: can itself trigger a UI prompt in some lock states, and adds a
  per-op subprocess to the happy path.
- **Probe-write to a throwaway keychain item** (the inbox's second suggestion) —
  REJECTED: same UI-prompt hazard, plus it pollutes the keychain with probe
  items.
- **Exit-code (36) as the detection key** — REJECTED: exit codes from `security`
  are not a documented stable contract; the OSStatus text is. Exit 36 is kept as
  a corroborating live observation only (and the G-N1 negative control proves a
  same-exit-code-but-different-string failure does NOT fall back).
- **Reporting `keychain` on the `config set` success line under fallback** —
  REJECTED: false, and a worse divergence than the truthful `(backend: file)` +
  the notice.
