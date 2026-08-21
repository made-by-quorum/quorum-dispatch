# ADR 0020: `qw` is a sibling binary, not a fourth package

**Status:** **Accepted** — ruled by the user on 2026-08-16. (It was Proposed for
one day: nothing in the qd/qw split depended on it, but packaging must not drift
into a new shape by side effect, so the ruling was asked for explicitly.) The
packaging changes below shipped in the same change that accepted it, along with
the corrections to `02-qw-split.md` and the README that this ADR calls for.
**Date:** 2026-08-16
**Relates-to:** [ADR-0018](0018-rust-stack-packaging-and-install.md) (packaging &
install for the Rust qd stack); `doc/tbd/provider-architecture/02-qw-split.md`
(the split proposal, stage 5); `doc/tbd/provider-architecture/11-stage3-plan.md`
(what actually shipped).

## Context

The qd/qw split has landed. `qd` no longer performs session work in its own
process: it addresses a lane through `quorum_qw::wire::client::WireLane`, which
spawns the **`qw` binary** and speaks line-delimited JSON over its stdio. The
delivery ledger is two physical files. `qw` is a real, separate executable.

`doc/tbd/provider-architecture/README.md` has carried this as an open decision
needing a human call:

> **Two binaries contradicts ADR-0018** ("one pin, one binary, no fourth
> package"). That ruling needs an explicit reversal with its own reasoning, not a
> side-effect drift.

### The contradiction does not exist, and the quotation is the reason it looked like it did

**ADR-0018 does not contain the phrase "one binary."** Its decision heading is
*"One entry point, no fourth package"*, and every supporting sentence is about
**packages, repos and pins** — the three independently-versioned artifacts `qd`,
`qb` and plugins, and the risk of a user running an incompatible combination:

> **`qd` is the single entry point and the carrier of the pin. You choose one
> version (the `qd` tag); that determines the other two.**

> **Why no fourth package:** a meta-installer would re-implement `qd`'s carrier
> role with a new moving part and its own version.

The phrase "one pin, one binary, no fourth package" was coined in
`02-qw-split.md:146` as a paraphrase of that ruling, and propagated from there
into the README's open-decisions list. The nearest thing in the tree to the
literal claim is a comment in the Homebrew formula
(`packaging/homebrew/quorum-dispatch.rb:16`, "Single binary: `cargo build
--release -p quorum-dispatch --bin qd`; no bun, no node"), which is describing
*that formula's build step* and contrasting it with the archived TypeScript
stack's runtime dependencies — not ruling on how many executables the package
installs.

So the open decision was against a sentence nobody wrote. Recording that is the
point of this ADR: a paraphrase hardened into a constraint, and it sat on the
critical path of a stage for weeks.

### What `qw` actually is, against ADR-0018's three tests

| ADR-0018's concern | `qw` |
|---|---|
| A **fourth package** with its own version | No. `qw` is a `[[bin]]` target of the existing `quorum-qw` crate, in this repo, built from the same source tree. |
| A **second pin** the user has to match | No. The `qd` tag still determines everything; `qw` is built from the same commit by construction. |
| A **second entry point** the user installs or invokes | No. Users never run `qw`. `qd` spawns it. Its verbs (`serve`, `attach`, `build-profile`) are machine entry points, dispatched pre-clap exactly like `qd`'s own hidden `qrmux-server` / `relay:serve` / `acp-daemon` / `pi-daemon`. |

The precedent is already in the tree and predates this split: `quorum-dispatch`
ships four `[[bin]]` targets (`qd`, `recovery_coordinator`,
`mint-conformance-grid`, and two feature-gated test targets), and
`recovery_coordinator` is described in `Cargo.toml` as "A SEPARATE bin from `qd`
… so the `qd` binary stays byte-intact." Shipping more than one executable from
one package is the existing norm, not a new departure.

## Decision

**ADR-0018 stands unamended in its decision. `qw` falls inside it, not against
it.** One package, one pin, one entry point; `qw` is an implementation detail of
that package, invisible to the user.

Two obligations follow, and both are load-bearing rather than cosmetic:

1. **`qw` must be installed beside `qd`, in the same directory, always.**
   `WireLane` resolves `qw` as a **sibling of `current_exe()`** and deliberately
   **never searches `PATH`** (`wire/client.rs::resolve_qw`). A `qw` found on
   `PATH` could come from a different install than the `qd` that is running,
   which is exactly the version skew the wire handshake exists to catch.
   Sibling-resolution makes the common case correct by construction: one
   directory, one install, one pin.

2. **A missing `qw` must stay a loud failure.** It already is — `qd` reports the
   path it looked for and refuses, and never falls back to an in-process lane. A
   fallback would mean a machine with a half-installed `qw` silently runs a
   *different architecture* from the one that was tested. This is the same ruling
   `tests/common/p0bins.rs`'s `qrmux_bin()` already makes for its own sibling
   binary ("PANICS with a build hint if absent — never a silent skip").

### What must change in packaging, and why each is a real gap

Verified against the tree at the time of writing:

| Site | Today | Needed |
|---|---|---|
| `install.sh:70` | builds `-p quorum-dispatch -p frame -p qrm -p qbt --bins` | **`quorum-qw` is not in the list, so `qw` is never built.** Add it. |
| `install.sh:73-75` | `for b in qd qf qrm qbt` existence check | add `qw` |
| `install.sh:79-83` | four explicit `install -m 0755` lines | install `qw` into the same `$QBIN` |
| `install.sh:131` | user-facing `binaries : $QBIN/{qd,qf,qrm,qbt}` | `qw` is not user-facing; either omit it or mark it internal — do not advertise a verb surface users should not call |
| `packaging/homebrew/quorum-dispatch.rb:37` | `"--bin", "qd"` then `bin.install "target/release/qd"` | build and install both; the singular `--bin qd` is the one hard-coded blocker |
| `qrm/src/verbs.rs:138,478` | `for name in ["qd","qf","qbt"]` → `place_colocated_binary` | **`qw` must be placed, or an installed `qd` finds no sibling.** This is the highest-consequence one: qrm is what puts binaries in `~/.quorum/bin`. |
| `qrm/src/doctor.rs:318` | PATH resolution check over `["qd","qf","qrm","qbt"]` | `qw` is not on `PATH` by design — the doctor should check it is a sibling of `qd`, not that it resolves on `PATH` |
| `scripts/deploy-gate.sh` | generic by argument; calls `build-profile` on the staged binary | already satisfied — `qw` answers `build-profile` for this reason |

There is **no CI file to update**: `.github/workflows/` holds only a README, and
re-adding a `.yml` is forbidden without a separate affordable-CI decision.

### What shipped on acceptance (2026-08-16)

Every row above was re-verified against the tree before being changed, and every
one still held. Three things came out differently from the table, and they are
the interesting ones:

- **`install.sh:131` — presentation.** `qw` is listed, but on its own
  `internal :` line rather than inside the `binaries : {qd,qf,qrm,qbt}` brace
  set, worded "qd spawns this; it is not a command you run". Silence was the
  other option and it was rejected: an operator who lists `~/.quorum/bin` will
  see the file either way, and an unexplained executable invites exactly the
  wrong guess (that it is a command, or that it is cruft to delete). Naming it
  as internal costs one line and forecloses both. The advertised command set is
  unchanged.
- **The doctor's two checks are now two different checks.** `qw` gets a
  `sibling` check, not a `resolve` check, and the distinction is keyed off a
  binary's class in `qrm/src/binaries.rs` rather than off a name. Its FAIL case
  is specific: `qd` installed *without* `qw` beside it is a `Fail`, not a
  `Warn` — that machine's `qd` cannot open a lane at all. Neither installed is a
  `Skip`, as on any fresh box.
- **`qw` picked up half the deploy gate.** It answers `build-profile` but has
  no timeable read verb (`serve`/`attach` block on stdin by design), so
  `place_colocated_binary` and the doctor gate it on profile alone rather than
  not at all. This is not gold-plating: since the split, the session work whose
  unoptimized slowness caused the 2026-07-07 outage runs **inside `qw`**, so a
  debug-profile `qw` is that same incident class, now in the binary the gate
  used not to cover.

The two-class distinction itself lives in `qrm/src/binaries.rs`: one table of
rows (`name`, `exposure`, `placed_from_sibling`, `answers_build_profile`), with
`Exposure::{UserFacing, ColocatedInternal}` as the thing the differing checks
branch on. The four sites that used to hard-code a name list — the status list,
the two placement loops, the doctor's resolve loop — now ask that table, so a
future binary is one row rather than four edits that can disagree.

## Consequences

- Stage 5 of `02-qw-split.md` ("then reconsider packaging") is **smaller than it
  looked**: an install-list change and a Homebrew formula change, not a
  reversal of a ruling.
- `qrm`'s notion of "installed binaries" splits in two: **user-facing** (`qd`,
  `qf`, `qrm`, `qbt` — on `PATH`, smoke-tested, advertised) and **colocated
  internal** (`qw` — beside `qd`, never on `PATH`, never advertised). Today those
  are one list; the doctor's checks differ between them, so the distinction has
  to be real in the code, not a comment.
- The two binaries must be built and shipped **from the same commit**, which
  cargo gives us for free in-repo and which the wire's version handshake enforces
  at runtime as a second belt. If a future release process ever ships them
  independently, the handshake refuses loudly rather than misbehaving — that is
  by design and should not be "fixed" by loosening it.
- **`02-qw-split.md:146` and the README's open-decisions entry are wrong as
  written** and should be corrected in the same change that accepts this ADR,
  rather than left as a second source of truth.
- Unaffected by this ADR and still genuinely open: whether `qd` should stop
  *linking* `quorum-qw` as a library. That is the `join.rs` split plus the
  registry (`10-join-split.md`), an internal-architecture question with no
  packaging consequence — one package ships regardless.
