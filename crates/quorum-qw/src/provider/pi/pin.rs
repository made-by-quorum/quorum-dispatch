//! Item 6 — the pi version PIN + the RPC-shape SMOKE CHECK (impl-plan §3).
//!
//! pi's RPC protocol is UNVERSIONED on the wire (zero `protocolVersion` anywhere
//! in `dist/`), and its command surface MOVED 19→29 across 0.x minors — so we
//! cannot negotiate a version at runtime. Instead we pin two ways:
//!   1. **Exact npm version** — [`PIN_SPEC`] (`@earendil-works/pi-coding-agent@0.80.2`,
//!      no caret) in the provisioning manifest that populates `~/.npm-pi-global`.
//!   2. **RPC-shape smoke check** (read-only, credential-free, runs in CI /
//!      build-health) — [`run_pin_smoke`]: assert `pi --version` == [`PINNED_VERSION`],
//!      the `RpcCommand` union is EXACTLY [`EXPECTED_RPC_COMMAND_COUNT`] (29) members,
//!      ZERO `protocolVersion`, and the `rpc-types.d.ts` hash == [`PINNED_D_TS_SHA256`].
//!      Any RPC-surface drift flips the hash and FORCES a re-review before the pin
//!      is bumped.
//!
//! This is lighter than codex's `version::PINNED` parse-gate (which guarded a
//! parse landmine); pi's risk is plain protocol drift, and the hash pin catches it.
//!
//! The pure parsers ([`rpc_command_count`], [`has_protocol_version`],
//! [`version_matches`], [`sha256_hex`]) are unit-tested; the live `pi --version`
//! exec is the RUN part (executes only when [`run_pin_smoke`] is invoked against an
//! installed pi — CI/build-health/the item-7 live slot), never read-and-assume.

use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

/// The exact npm version pin (no caret) for the provisioning manifest.
pub const PIN_SPEC: &str = "@earendil-works/pi-coding-agent@0.80.2";

/// The pinned pi version `pi --version` must report.
pub const PINNED_VERSION: &str = "0.80.2";

/// The exact `RpcCommand` union member count at the pin (the 19→29 drift A1→a3 is
/// the proof the surface moves between versions; any change re-reviews).
pub const EXPECTED_RPC_COMMAND_COUNT: usize = 29;

/// The pinned SHA-256 of `dist/modes/rpc/rpc-types.d.ts` at 0.80.2 (computed over
/// the file's raw bytes). A drift flips this and forces an RPC-surface re-review.
pub const PINNED_D_TS_SHA256: &str =
    "ff730c95491a767371b6ce342d8414301095800620f350068bbf84dc6761df33";

/// Does `pi --version` output report the pinned version? (substring match — pi
/// prints a bare `0.80.2`, but tolerate a `pi 0.80.2`-style prefix too.)
pub fn version_matches(version_output: &str) -> bool {
    version_output
        .trim()
        .split_whitespace()
        .any(|t| t == PINNED_VERSION)
        || version_output.trim() == PINNED_VERSION
}

/// Extract the `export type RpcCommand = …` union block (up to the next top-level
/// `export ` declaration) so we count ONLY the command discriminants, never the
/// `RpcResponse`/`RpcSessionState` blocks that follow.
fn rpc_command_block(src: &str) -> Option<&str> {
    let start = src.find("export type RpcCommand =")?;
    let rest = &src[start..];
    // Skip the declaration's own line, then stop at the next top-level `export `.
    let after_decl = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
    let tail = &rest[after_decl..];
    let end = tail
        .find("\nexport ")
        .map(|i| after_decl + i)
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Count the `RpcCommand` union's `type: "…"` discriminants — the live command
/// count. Returns 0 if the union block is absent (a drift the smoke check fails on).
pub fn rpc_command_count(d_ts_src: &str) -> usize {
    match rpc_command_block(d_ts_src) {
        Some(block) => block.matches("type: \"").count(),
        None => 0,
    }
}

/// Is `protocolVersion` present ANYWHERE in the typings? (pi's protocol is
/// unversioned on the wire; a non-zero count means the surface changed.)
pub fn has_protocol_version(d_ts_src: &str) -> bool {
    d_ts_src.contains("protocolVersion")
}

/// Lowercase-hex SHA-256 of raw bytes (matches `sha256sum`).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The smoke-check outcome — each field is OBSERVED evidence, not an assertion
/// (RUN-not-read: the verdict rests on these, and they are inspectable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinReport {
    /// The raw `pi --version` output (trimmed).
    pub version_output: String,
    pub version_ok: bool,
    pub command_count: usize,
    pub command_count_ok: bool,
    pub has_protocol_version: bool,
    pub d_ts_sha256: String,
    pub hash_ok: bool,
}

impl PinReport {
    /// All four shape invariants hold (version + count + no-protocolVersion + hash).
    pub fn ok(&self) -> bool {
        self.version_ok && self.command_count_ok && !self.has_protocol_version && self.hash_ok
    }
    /// A human-readable summary of any failures (empty when `ok()`).
    pub fn failures(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.version_ok {
            out.push(format!(
                "version {:?} != pinned {PINNED_VERSION}",
                self.version_output
            ));
        }
        if !self.command_count_ok {
            out.push(format!(
                "RpcCommand union has {} members, expected {EXPECTED_RPC_COMMAND_COUNT}",
                self.command_count
            ));
        }
        if self.has_protocol_version {
            out.push("rpc-types.d.ts now contains protocolVersion (was unversioned)".to_string());
        }
        if !self.hash_ok {
            out.push(format!(
                "rpc-types.d.ts sha256 {} != pinned {PINNED_D_TS_SHA256} (RPC surface drifted — re-review)",
                self.d_ts_sha256
            ));
        }
        out
    }
}

/// RUN the pin smoke check against an installed pi (RUN-not-read; credential-free).
/// Execs `<pi_bin> --version` and hashes/parses `d_ts_path`. Returns the evidence
/// report; the caller (CI / build-health / item-7) rules on `report.ok()`.
pub fn run_pin_smoke(pi_bin: &Path, d_ts_path: &Path) -> Result<PinReport, String> {
    let version_output = match Command::new(pi_bin).arg("--version").output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(e) => return Err(format!("exec {} --version: {e}", pi_bin.display())),
    };
    let bytes =
        std::fs::read(d_ts_path).map_err(|e| format!("read {}: {e}", d_ts_path.display()))?;
    let src = String::from_utf8_lossy(&bytes);
    let command_count = rpc_command_count(&src);
    let d_ts_sha256 = sha256_hex(&bytes);
    Ok(PinReport {
        version_ok: version_matches(&version_output),
        version_output,
        command_count_ok: command_count == EXPECTED_RPC_COMMAND_COUNT,
        command_count,
        has_protocol_version: has_protocol_version(&src),
        hash_ok: d_ts_sha256 == PINNED_D_TS_SHA256,
        d_ts_sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal RpcCommand union + a trailing RpcResponse block, to prove the count
    // excludes everything after the union (the real file has 29; here we assert the
    // PARSER, with a 3-member fixture).
    const FIXTURE: &str = r#"
export type RpcCommand = {
    type: "prompt";
    message: string;
} | {
    type: "abort";
} | {
    type: "get_state";
};
export interface RpcSessionState {
    type: "response";
    command: "prompt";
}
"#;

    #[test]
    fn counts_only_the_command_union_discriminants() {
        // 3 union members; the RpcResponse `type: "response"` is NOT counted.
        assert_eq!(rpc_command_count(FIXTURE), 3);
    }

    #[test]
    fn protocol_version_detection() {
        assert!(!has_protocol_version(FIXTURE));
        assert!(has_protocol_version(
            "interface X { protocolVersion: number }"
        ));
    }

    #[test]
    fn version_match_bare_and_prefixed() {
        assert!(version_matches("0.80.2"));
        assert!(version_matches("pi 0.80.2"));
        assert!(!version_matches("0.80.1"));
        assert!(!version_matches("0.81.0"));
    }

    #[test]
    fn sha256_is_deterministic_hex() {
        let a = sha256_hex(b"hello");
        assert_eq!(a.len(), 64);
        assert_eq!(a, sha256_hex(b"hello"));
        assert_ne!(a, sha256_hex(b"world"));
    }

    #[test]
    fn report_ok_requires_all_four_invariants() {
        let good = PinReport {
            version_output: "0.80.2".into(),
            version_ok: true,
            command_count: 29,
            command_count_ok: true,
            has_protocol_version: false,
            d_ts_sha256: PINNED_D_TS_SHA256.into(),
            hash_ok: true,
        };
        assert!(good.ok());
        assert!(good.failures().is_empty());

        let drifted = PinReport {
            command_count: 31,
            command_count_ok: false,
            ..good.clone()
        };
        assert!(!drifted.ok());
        assert_eq!(drifted.failures().len(), 1);
    }
}
