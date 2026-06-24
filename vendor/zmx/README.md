# vendored zmx 0.6.0

This directory pins the exact zmx source sb is built and tested against. The
pin is enforced by sha256: `scripts/fetch-zmx.sh` retrieves the tarball from
this in-repo mirror (or an override URL) and REFUSES, non-zero, on any hash
mismatch. The mirror IS the pin — nothing else decides what "zmx 0.6.0" means
to this repo.

## Provenance

- **Upstream:** https://github.com/neurosnap/zmx, tag `v0.6.0`.
- **Mirrored file:** `zmx-0.6.0.tar.gz` — the upstream **source** tarball.
- **Fetched from:** https://github.com/neurosnap/zmx/archive/refs/tags/v0.6.0.tar.gz
- **Date fetched:** 2026-06-04.
- **sha256:** `4b5a155a0956abb812ab52fb5d65e63cc1d745eee566ea7fd8930393284d4673`
  (recorded in `SHA256SUMS`, `shasum -a 256 -c` format).
- **Size:** 1176708 bytes (~1.12 MiB) — small enough to commit the blob directly;
  the release-asset fallback in a2-spec §9 was NOT needed.

### Tarball-stability finding (double-download)

GitHub's `/archive/refs/tags/<tag>.tar.gz` endpoint generates tarballs on the
fly, so byte-stability is not contractually guaranteed. We downloaded it TWICE
on 2026-06-04 and both copies were byte-identical (same sha256, same 1176708
bytes). The first download is what is committed here and is therefore the
canonical pin regardless of any future upstream re-generation: once mirrored,
`fetch-zmx.sh` only ever trusts THIS file's hash.

Note: the v0.6.0 GitHub *release* also publishes platform binary tarballs
(`zmx-0.6.0-<os>-<arch>.tar.gz` + `.sha256`). We deliberately mirror the
**source** tarball, not a per-platform binary, so the pin is one artifact across
macOS/Linux and aarch64/x86_64. Installer/bootstrap UX that turns this source
into an installed `zmx` binary is A5's concern (a2-spec §3); A2 ships only the
mirror + fetch + verify.

## Why pinned to 0.6.0

sb drives Claude sessions by writing keystrokes into the session's PTY via
`zmx send <name>`. The `send` subcommand (alongside `history`/`wait`) is what
0.6.0 advertises and what 0.5.x lacks. Per `LESSONS.md` L3, an older zmx
silently no-ops every keystroke sb types: `sb new` boots, the auto-Enter never
lands, the PID file never appears, and the boot loop times out "not found"
~40s later — a HANG, not an error. Pinning 0.6.0 (and preflighting the `send`
subcommand at runtime, L3) is how sb refuses to drive a zmx that cannot be
driven.

## How to re-mirror / bump the pin

The tarball and `SHA256SUMS` are a matched pair — update them TOGETHER in one
commit, never one without the other:

1. Download the new source tarball:
   `curl -fsSL -o vendor/zmx/zmx-<VER>.tar.gz \
      https://github.com/neurosnap/zmx/archive/refs/tags/v<VER>.tar.gz`
2. (Bump only) `git rm vendor/zmx/zmx-<OLDVER>.tar.gz`.
3. Regenerate the checksum file from inside this dir so the recorded name is the
   bare filename (`shasum -c` matches on the bare name):
   `( cd vendor/zmx && shasum -a 256 zmx-<VER>.tar.gz > SHA256SUMS )`
4. Verify it round-trips: `( cd vendor/zmx && shasum -a 256 -c SHA256SUMS )`.
5. Update this README's provenance block (URL, date, sha256, size, stability
   re-check) and the version pin everywhere the spec names it.
6. Re-run the A-phase gate: `scripts/fetch-zmx.sh` green +
   `test/golden/selftest/test_fetch_zmx.sh` (the negative control proves the
   refusal path still fires). Bumping zmx is an A-phase change, not a silent
   dependency drift — the gate re-runs before merge.
