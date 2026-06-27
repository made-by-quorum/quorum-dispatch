# sb-rust

A Rust rewrite of **sb**. This repository hosts two crates:

- **`sb`** — the engine crate (`crates/sb`). The core sb engine.
- **`qrmux`** — the mux crate (`crates/qrmux`).

At this stage the crates are **content-free scaffolding**: they exist to anchor the
Cargo workspace and a green two-platform CI. Engine and mux logic land in later phases.

## Status

Phase 0a: scaffold + CI. The workspace builds and tests clean on macOS (arm64) and
Linux (x86_64) in GitHub Actions.

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
crates/sb/            engine crate
crates/qrmux/         mux crate
scripts/build-lock.sh build mutex (mkdir-based, stale-recovery)
.github/workflows/    CI (macOS arm64 + Linux x86_64)
doc/adr/              architecture decision records
```

## License

MIT.
