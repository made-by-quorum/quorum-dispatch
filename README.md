# dispatch

**dispatch** is the session/engine of the Quorum suite — it launches and
multiplexes agent sessions, hosts the relay channel, and loads plugins. It is
installed as the **`qd`** binary (state under `~/.quorum/dispatch`, overridable
via `QD_HOME`). This repository hosts the engine workspace:

- **`dispatch`** — the engine crate (`crates/dispatch`); builds the `qd` binary.
- **`qrmux`** — the embedded terminal multiplexer (`crates/qrmux`).
- **`golden`** / **`fakerepl`** — the golden-test harness and the deterministic
  fake REPL the suites drive (`crates/golden`, `crates/fakerepl`).

## Status

In active use across macOS (arm64) and Linux (x86_64), gated by a green
two-platform CI.

## pi provider — observer caveat (flag-c)

> ⚠️ **A second observer must not trust `qd info` / `qd ls` for a pi session while another
> `qd wait` holds that resident's sole connection.** A live pi resident serves a single
> connection. While a `qd wait <pi-session>` is camped on a resident, a concurrent,
> connectionless `qd info` / `qd ls` reads a **stale registry-cache snapshot** — it can report
> the wrong busy/idle `status` (e.g. `idle` while the session is genuinely busy), with exit code
> 0 and **no error and no staleness marker**.
>
> The busy/idle **gate itself is exact** for any reader that holds the connection and does the
> live `is_streaming` point-read — i.e. `qd wait` itself, and any turn-lifecycle gating built on
> it. The staleness affects only the *connectionless* observe path (`qd info` / `qd ls`) under a
> camped wait; it is registry-cache staleness, not a dropped event. **To gate pi liveness, use
> `qd wait` (or `is_streaming`), never the `qd info` / `qd ls` `status` field.** A staleness
> marker/error on the observe path is a planned fast-follow.

## Build

Toolchain is pinned to Rust 1.95 via `rust-toolchain.toml`.

```sh
# Direct:
cargo build --workspace
cargo test --workspace

# Through the build lock (serializes concurrent build/test invocations):
./scripts/build-lock.sh cargo build --workspace
./scripts/build-lock.sh cargo test --workspace
```

### The build lock

`scripts/build-lock.sh` is a host-wide mutex around cargo build/test invocations
(and, later, golden-recording / live-mux runs). It prevents concurrent runs from
racing on the shared `target` dir, and recovers automatically if a previous holder
died without releasing. It uses an atomic `mkdir` lock so it runs identically on
macOS and Linux (see `doc/adr/0001-build-lock-mkdir.md`). All build/test calls
should go through it.

## Conventions

See [CONVENTIONS.md](CONVENTIONS.md). Key rule: **all schema structs use permissive
parsing** — `#[serde(default)]` and `Option<T>` for fields, so unknown or missing
fields never hard-fail deserialization.

## Layout

```
Cargo.toml            workspace manifest
crates/dispatch/      engine crate (builds the qd binary)
crates/qrmux/         embedded mux crate
scripts/build-lock.sh build mutex (mkdir-based, stale-recovery)
.github/workflows/    CI (macOS arm64 + Linux x86_64)
doc/adr/              architecture decision records
```

## License

MIT.
