//! `provider::pi::store` — read-only pi on-disk session reading.
//!
//! A one-file directory today, but a deliberate seam: this is where pi's
//! on-disk transcript knowledge lives, kept separate from BOTH lanes that
//! reach a live pi ([`crate::provider::pi::daemon`], [`crate::provider::pi::pty`])
//! because it is the only piece either lane's caller can read cold, with no
//! process, no socket, no resident — the `qd list` / cold-scan / `qd resume`
//! identity-check path.
//!
//! [`session`] is permissive session-JSONL reading + path math, tolerant of
//! pi's lazy-write window ("no file on disk" ≠ "no session exists" — pi does
//! not flush a session file until the first assistant reply) and of BOTH
//! layouts pi writes (bucketed under `--<enc-cwd>--/`, or flat when a session
//! dir is handed to pi explicitly).

pub mod session;
