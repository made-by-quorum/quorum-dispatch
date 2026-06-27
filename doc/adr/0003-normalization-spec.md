# ADR 0003: Capture normalization spec

**Status:** Accepted (orchestrator-reviewed 2026-06-04)
**Date:** 2026-06-04

## Context

Golden captures are timing- and environment-sensitive: the same logical qd/zmx
behavior emits different bytes across runs (timestamps, PIDs, hermetic temp
paths, per-run ids/ports, and non-semantic PTY chunk boundaries). A byte-exact
comparator would false-diff on all of these. But over-normalization is worse than
under-normalization: erasing a load-bearing byte (an exit code, an alt-screen
toggle, a CR-vs-LF, a backlog line, a JSON contract field) silently blinds the
oracle to a real regression — the exact failure the golden test exists to catch.

This ADR specifies precisely WHAT is normalized and WHAT is never touched. The
normalizers live in `test/golden/lib/normalize.sh`; each rule has a unit test in
`test/golden/selftest/test_normalize.sh` proving (a) it collapses its noise AND
(b) it preserves load-bearing bytes.

## Decision

### Normalized (volatile, non-semantic) → stable tokens

| Class | Rule | Token | Notes |
|-------|------|-------|-------|
| Timestamps | ISO-8601 (`...T..:..:..(.fff)?(Z|±..:..)?`), clock-with-frac (`HH:MM:SS.ffffff`), epoch-ms (13 digits at a non-digit boundary) | `<TS>` | The PID-file *appearance* event is preserved structurally elsewhere (see comparator ADR); only the timestamp value is tokenized. **CONTEXT-GUARD (W1 P6b):** a clock-with-frac value preceded by a DURATION label (`duration\|elapsed\|took\|dur\|interval\|timeout\|runtime\|wall`) is a load-bearing elapsed VALUE and is PRESERVED (see §W1 below). |
| PIDs | `pid=N` / `pid N` / `pid:N` (labeled), and `/N.json` registry files | `<PID>` | Only PID-LABELED numbers and `<pid>.json`. Bare integers are NOT collapsed — coordinates/counts are load-bearing. |
| Hermetic paths | `<JAIL_ROOT>/sb_home` → `<SB_HOME>`, `/zmx` → `<ZMX_DIR>`, `/xdg_*` → `<XDG_*>`, jailed `/tmp` → `<TMPDIR>`, residual `<JAIL_ROOT>` | tokens | Suffix after the dir (e.g. `zmx-<uid>` resolution structure) is preserved — it carries the Bug-D resolution-order meaning. **DISTINCTNESS (W1 P6c):** an UNJAILED host `/tmp/…` path (a jail-escape) → distinct `<HOST_TMP>`, NOT `<TMPDIR>` (see §W1). |
| zmx uid | the LIVE test uid in a `zmx-<uid>` path component / a `uid=<uid>` field | `<ZMX_UID>` | **W1 P7.** Tokenized ONLY when the uid equals the live test uid; a WRONG uid survives + diffs (see §W1). |
| Run ids / session prefix | `sbrg-<runid>-` → `sbrg-<RUNID>-`; bare `<runid>` → `<RUNID>` | tokens | |
| Relay port | the per-run port in a PORT-BEARING context only | `<RELAY_PORT>` | **CONTEXT-GUARD (W1 P6a):** JSON `"…port": N`, URL `host:N`, or labeled `port[=: ]N` only; a bare integer coincidentally equal to the port is PRESERVED. A longer number merely containing the port digits is preserved (explicit non-digit boundary). |
| ANSI chunk boundaries | a run of ≥2 consecutive bare SGR resets (`ESC[0m ESC[0m...`) collapses to one | — | The most conservative possible coalescer. Literal-string replace (not regex/gsub), so it cannot touch alt-screen or cursor sequences. |

### NEVER normalized (load-bearing — the comparator depends on these)

- **exit codes** (captured in the `.exit` sidecar, compared raw)
- **alt-screen sequences** (`?1049h/l`, `?47h/l`, `?1047h/l`, `2J`, `3J`)
- **CR (`\r`) vs LF (`\n`)** distinction
- **cursor-move sequences** including their numeric coordinates (repaint fidelity)
- **backlog line content and order**
- **JSON field presence/values that carry contract** (`status`, `clients`, `name`, …)

### Portability constraint (load-bearing for CI)

BSD sed (macOS CI) has no `\b` word boundary and awk `gsub` treats its needle as a
regex. Both were found to silently no-op / misfire during development. The spec
therefore mandates: explicit `(^|[^0-9])…($|[^0-9])` boundaries instead of `\b`,
and literal `index`/`substr` replacement for the ANSI coalescer. The unit tests
run under `/bin/bash` 3.2 to enforce this.

### W1 delta-strength context-guards (panel findings P6a/b/c, P7)

The cross-model panel review (0b-panel-dispositions, findings P6/P7) showed the
v1 normalizers, while NEVER erasing the load-bearing classes above, were broader
than necessary in four spots — broad enough that a materially-wrong engine could
normalize INTO a green match (a "false green"). Delta-strength Wave 1 NARROWS each
rule with an explicit context-guard. **Each guard only narrows matching** — it
never broadens — so every committed v1 golden re-derives byte-identically EXCEPT
the one row that legitimately gains the new `<ZMX_UID>` token (P7), which is a
ruled re-derivation, not a silent regeneration.

- **P6a — relay-port context-guard (gemini false-green).** The v1 port rule
  tokenized the per-run port at ANY non-digit boundary, so a buggy count/index
  that coincidentally emitted the port value was scrubbed to `<RELAY_PORT>` and a
  numeric regression went green. The port is now tokenized ONLY in a port-bearing
  context: JSON `"<…>port": N`, URL `host:N` (incl. `://host:N/…`), or a labeled
  `port[ =:]N`. A BARE integer equal to the port in any other position SURVIVES.
  Both directions are unit-tested (`p6a/*`).

- **P6b — clock-with-frac context-guard (deepseek finding).** `HH:MM:SS.ffffff`
  also matches a legitimate DURATION/elapsed VALUE, which is load-bearing (erasing
  it blinds the oracle to a timing regression printed as a value). A clock
  preceded by a duration label (`duration|elapsed|took|dur|interval|timeout|`
  `runtime|wall`) is PRESERVED; genuine timestamps (DLINE log lines, line-leading
  clocks, the ISO `T` form) still collapse. BSD sed has no lookbehind, so the
  implementation injects a non-printing sentinel (SOH) INSIDE the duration's clock
  to defeat the clock pattern, then strips it — a sentinel merely PRECEDING the
  clock would not work (the clock's own digit run stays intact). Unit-tested
  (`p6b/*`).

- **P6c — HOST_TMP distinctness (gpt jail-escape finding).** The jail's own temp
  dir collapses to `<TMPDIR>` (a hermetic, jailed path). An UNJAILED host `/tmp/…`
  path in a capture is a JAIL-ESCAPE — the engine wrote outside the jail — and
  must NOT collapse into the SAME token a hermetic golden expects (that would let
  an escape normalize into green). After the jail substitutions consume every
  jailed path, any residual bare `/tmp/…` is unjailed by construction and is
  tokenized to a DISTINCT `<HOST_TMP>`, so it can never match a `<TMPDIR>`-expecting
  golden — the escape stays visible and DIFFS. **Design choice (the filter
  architecture):** normalize.sh's rules are pure stdin→stdout filters with no
  failure channel, so the "fail the capture" alternative the panel floated does
  not fit; the distinct-token design achieves the same guarantee (a jail-escape
  cannot normalize into green) within the filter contract, and composes cleanly as
  a final substitution inside `normalize_paths`. The left boundary (start-of-line
  or a non-path char) prevents a word merely CONTAINING `tmp` (`foo_tmp`, `/xtmp`)
  from false-matching. Unit-tested (`p6c/*`).

- **P7 — `<ZMX_UID>` token (grok finding + red-team uid-501 NIT).** resolveZmxDir's
  TMPDIR-collapse tier appends `zmx-<uid>` (utils.ts:68-82); the recorded golden
  embeds the recording host's uid. Leaving the literal makes the row host-locked;
  tokenizing it UNCONDITIONALLY lets a broken engine that hard-codes the WRONG uid
  (`zmx-0`, `zmx-999999`) ALSO tokenize and pass green. The fix tokenizes the uid
  to `<ZMX_UID>` ONLY when it equals the LIVE test uid (`id -u`, threaded through
  `normalize_all` as a 4th arg, default `${JAIL_UID:-$(id -u)}`): a wrong uid is
  not the live uid, stays literal, and DIFFS against the `<ZMX_UID>` the golden
  expects. One rule serves both portability (correct uid → stable token) and
  correctness (wrong uid → survives + diffs). The same rule tokenizes the bare
  `uid=<uid>` field the resolution row records, for the same property. Explicit
  non-digit boundaries (BSD sed, no `\b`) so `501` does not partial-match `5010`.
  Both directions unit-tested (`p7/*`). **Proof-chain note:** this is the ONE W1
  rule that changes a committed golden's re-derived form (the Linux
  `resolution-linux.txt` row, whose normalized text holds literal `zmx-501` /
  `uid=501`). Per the W1 re-derivation procedure that change is REPORTED for a lead
  ruling, NOT silently regenerated.

## Consequences

- A re-run of the same scenario produces identical normalized output (no false
  diffs from time/pid/path/port/runid churn).
- A genuine regression in any load-bearing byte is preserved into the comparison
  and caught. The mutation test (`run_mutation.sh`) proves dropped-CR and
  altered-text divergences survive normalization and are flagged.
- The ANSI coalescer is intentionally minimal. If future real captures show
  non-semantic re-chunking that this rule does not cover, the rule is EXTENDED
  with a new unit test proving it still preserves alt-screen/cursor/CR — never
  broadened speculatively.
- Normalizers are pure stdin→stdout filters, individually unit-tested, so the
  comparator and mutation layers can reason about them in isolation.
