# Codex pin log

Every re-pin of the codex binary, in order. Append a section per re-pin; never
edit a past one.

## Why this file exists

The pin (`crates/dispatch/tests/fixtures/codex-schema/VERSION.pin`, surfaced as
`provider::codex::version::PINNED`) is the version qd's wire contract was
*verified* against. Everything downstream trusts it: `qd start --provider codex`
refuses to boot against a binary whose major/minor drifts from it, and the
committed schema fixture is that binary's `generate-json-schema` dump.

The ceremony (`scripts/codex-schema-diff.sh` header) requires a **named re-mint**
— re-pinning is deliberate, never silent. But "named commit" leaves the reasoning
in a commit message, where nobody looks when the next drift appears. This file is
the durable answer to the two questions a re-pin actually raises later:

1. **What moved in the wire schema, and did any of it touch the surface qd
   binds?** A re-pin that skipped that review is indistinguishable, after the
   fact, from one that did it.
2. **What was the pin verified against?** The fixture is regenerated from
   whatever binary happened to be installed on one developer's machine. That
   provenance is invisible in the diff.

A re-pin entry should let a future reader reproduce the judgement, not just
observe that a number changed.

---

## 0.143.0-alpha.14 → 0.146.1 — 2026-08-06

**Trigger.** `qd start --provider codex` refused to boot: *"codex 0.146.1
detected, pinned 0.143 — run the schema fixture-diff and re-pin"*. The installed
binary had moved three minors ahead of the pin, so every codex start required
`QD_CODEX_UNPINNED=1`.

**Regenerated from.** `codex-cli 0.146.1`, installed at
`~/.asdf/installs/nodejs/20.18.2/bin/codex` (npm `@openai/codex`, darwin-arm64
vendor binary). Generated jailed (`env -i`, isolated `CODEX_HOME`/XDG/TMPDIR)
exactly as `codex-schema-diff.sh` does, then canonicalized (sorted keys, 2-space
indent) the same way the fixture is stored.

**Drift observed.** 275 schema files; **73 differing, 8 new, 0 removed**. New
files: `AppsInstalledParams/Response`, `AppsReadParams/Response`,
`EnvironmentConnectionNotification`,
`ExternalAgentConfigImportHistoryRecordParams/Response`,
`RawResponseCompletedNotification`. The v2 rollup grew from 506 to 537
definitions.

**Review — the part that matters.** The drift is large but lands entirely
*outside* the surface qd binds. qd speaks eight methods plus one notification:

    initialize · initialized · thread/start · thread/resume
    turn/start · turn/steer · turn/interrupt · thread/status/changed

Every corresponding definition in `codex_app_server_protocol.v2.schemas.json` was
compared old-vs-new and is **byte-identical**: `InitializeParams`,
`InitializeCapabilities`, `ThreadStartParams/Response/Source`,
`ThreadResumeParams/Response`, `ThreadResumeInitialTurnsPageParams`,
`TurnStartParams/Response`, `TurnSteerParams/Response`,
`TurnInterruptParams/Response`, `ThreadStatus`,
`ThreadStatusChangedNotification`, `ThreadStartedNotification`,
`TurnStartedNotification`. Zero qd-relevant deltas.

The 73 changed files are approvals/permissions envelopes, plugin and app
surfaces, external-agent config import, and the aggregate rollups that contain
them — none of which qd reads or writes.

**Also verified live, against 0.146.1 (not just schema-diffed).** The wire was
exercised end to end, which the schema diff alone cannot do: `qd start --provider
codex` → `thread/start` returned a thread id; `qd send` → `turn/start` drove a
real turn and the reply landed in the rollout; the rollout parsed correctly
through the existing taxonomy (`session_meta`, `event_msg`/`agent_message`); and
`qd stop` group-reaped the daemon.

**Fallout fixed.** Four tests keyed on the pin value
(`pinned_is_the_fixture_version`, `sniff_exact_against_pinned_binary`,
`sniff_drifted_binary_is_breaking`, `sniff_patch_drift`) plus the daemon-side
version fixtures in `create_daemon.rs` / `resume_daemon.rs`.

⚠ **One coverage loss was nearly silent.** The old pin was a *pre-release*
string (`0.143.0-alpha.14`), and `sniff_exact_against_pinned_binary` was
doubling as the regression test for pre-release tag parsing — the bug where a
`-alpha.N` suffix sniffed Unparseable → VersionUnknown → create blocked outright.
Re-pinning to the plain `0.146.1` would have deleted that coverage without a
single test going red. It is now its own test,
`prerelease_tag_parses_to_its_core_not_unparseable`, independent of whatever is
pinned. **If a future re-pin changes the pin's SHAPE (pre-release ↔ release),
check what else was riding on that shape.**

**Residual risk.** The fixture now reflects one machine's binary. CI has no codex
installed, so `codex-schema-diff.sh` is developer-run only — the pin is trusted,
not continuously verified.
