# Quickstart

Agents use dispatch to discover, spawn, and message agents running in claude code, codex, pi, or opencode across one or more machines. 

Have your agent `brew install made-by-quorum/tap/quorum-dispatch` and `qd --help` to get started. 

## About

**dispatch** is the session/engine of the Quorum suite — it launches and
multiplexes agent sessions, hosts the relay channel, and loads plugins. It is
installed as the **`qd`** binary (state under `~/.quorum/dispatch`, overridable
via `QD_HOME`). This repository hosts the engine workspace:

- **`dispatch`** — the engine crate (`crates/dispatch`); builds the `qd` binary.
- **`qrmux`** — the embedded terminal multiplexer (`crates/qrmux`).
- **`golden`** / **`fakerepl`** — the golden-test harness and the deterministic
  fake REPL the suites drive (`crates/golden`, `crates/fakerepl`).
