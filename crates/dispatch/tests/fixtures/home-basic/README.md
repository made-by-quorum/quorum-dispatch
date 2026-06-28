# `home-basic` fixture

A frozen `$HOME`-shaped tree for the A1 `ls --json` / `info` golden-parity tests
(spec §9). The integration tests build an `QdPaths::from_home(<this dir>)`, run
`join::gather` + `join::join_with_strays` + `render::ls_json` / `render::info_text`
over it, and assert byte-equality with the frozen files under `../../golden/`.

## What it freezes

`.claude/sessions/` — the PID registry:
- `1001.json` — a LIVE entry WITH `backend` + `spawnedBy` (the new schema fields,
  brief deliverable 3). name `alpha-worker`, status `busy`, cwd `/work/projA`.
- `1002.json` — a LIVE entry WITHOUT `backend`/`spawnedBy` (LEGACY shape — proves
  permissive read + minimal write round-trip). name `beta-legacy`, status `idle`.
- `1003.json.tombstoned` — a TOMBSTONE (status `killed` in the join). name
  `gamma-killed`. Only surfaces with `--all` (`include_tombstoned`).

`.claude/projects/` — transcripts (`<cwd-slug>/<sessionId>.jsonl`):
- `-work-projA/live-aaaa-0001.jsonl` — matches live 1001 (turns, gitBranch
  last-wins `feature/alpha`, user+assistant previews).
- `-work-projB/live-bbbb-0002.jsonl` — matches live 1002.
- `-work-projC/dead-cccc-0003.jsonl` — matches the tombstone 1003.
- `-work-projD/dead-dddd-0004.jsonl` — a DEAD-COLD transcript: no registry, no
  live process → appears ONLY as a `cold` row, NEVER a stray. user-named
  (`agent-name: delta-cold`) so it survives the default-view filter.
- `-work-projE/stray-eeee-0005.jsonl` — a STRAY: no registry entry, but the test's
  `FixtureProcessTable` reports a live claude proc with cwd `/work/projE`. The
  decider badges it `unmanaged`. user-named (`agent-name: epsilon-stray`).

`.claude/relay/` — relay sidecars:
- `r1.json` — `{port:8901, sessionId:"live-aaaa-0001", pid:2001}`. The test's
  ppid_map chains relay pid 2001 → claude pid 1001, so the ancestry match assigns
  `relayPort: 8901` to the `alpha-worker` row.

## zmx-list (NOT a file — canned in the test)

`FixtureMux` is fed canned `zmx list` text in the integration test:
- the canonical dir has `alpha-zmx` (pid 1500, ancestor of claude 1001 via the
  test ppid_map) + an ENDED task (filtered out) ;
- a legacy dir repeats `alpha-zmx` with a different pid (canonical-wins dedupe).

## Determinism

Transcript file mtimes are NON-deterministic on checkout, and the TS cold-row
sort + the cold `lastActive` fallback read mtime. The test therefore COPIES this
tree into a tempdir and sets each transcript's mtime explicitly (via libc
`utimes`) before running the pipeline, so the golden output is byte-stable. Every
transcript also carries an explicit final `timestamp`, so `lastActive` for live
rows comes from `updatedAt` and for cold rows from the JSONL `lastTimestamp` (not
mtime) — mtime only drives the cold-row SORT, which the test pins.

## Pass (b)

Pass (a) freezes this against canned fixtures + the 0b dryrun capture
(provisional). Pass (b) re-runs parity vs the pinned TS corpus; any field whose
surface the fix-wave changes (esp. the PROVISIONAL stray shape) will be
regenerated here. This fixture is regenerate-friendly by design.
