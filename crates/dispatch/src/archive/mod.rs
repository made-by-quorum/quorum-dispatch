//! The S3-compatible object-store client library (persist-relocation:
//! formerly also home to `qd backup`'s orchestration and dispatch's own
//! `[archive]` config — both retired; transcript persistence now lives in
//! frame as `qf persist`, see `frame/src/persist.rs`). This module owns:
//!
//!   - `credentials`: the standard S3 credential chain (env vars →
//!     `~/.aws/credentials` profile). Credentials never live in config.
//!   - `sigv4` + `http`: a hand-rolled AWS SigV4 signer over a minimal
//!     synchronous HTTP/1.1 client (see `http.rs` module doc for why this is
//!     hand-rolled rather than reqwest-based).
//!   - `s3`: the GET/PUT object client built from the above — reused as a
//!     library by both dispatch's own event-adjacent needs and frame
//!     (frame links this crate as a library for exactly this client;
//!     `frame/src/engine.rs` stays the sole module that spawns `qd` as a
//!     process — importing this library is a compile-time link, not a
//!     process boundary).
//!
//! Mechanism only: nothing here decides WHETHER or WHEN to run, or reads any
//! config file — every caller resolves its own config and passes plain
//! values in.

pub mod credentials;
pub mod http;
pub mod s3;
pub mod sigv4;
