# Conventions

Repo-wide rules for sb-rust. These are binding for all phases.

## Permissive schema parsing

All schema structs (anything deserialized from external/persisted data) **must use
permissive parsing**:

- Annotate the struct with `#[serde(default)]` (or per-field defaults) so missing
  fields fall back to defaults rather than failing.
- Model optional fields as `Option<T>`.
- Do **not** use `#[serde(deny_unknown_fields)]`.

Rationale: data written by older/newer versions, or by adjacent tools, must not
hard-fail deserialization. Forward/backward compatibility beats strictness here.

(No schema structs exist yet in the Phase 0a scaffold; this rule governs the code
that introduces them.)

## Build invocations go through the lock

All `cargo build` / `cargo test` (and later golden-recording / live-mux) calls go
through `scripts/build-lock.sh`. See README and `doc/adr/0001-build-lock-mkdir.md`.
