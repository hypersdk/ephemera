// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! Wire protocol for the Zyvor Ephemera guest agent: one JSON object per
//! line (newline-delimited, no length prefix), one request in, one response
//! out, over AF_VSOCK. This crate is compiled into both `ephemera-guest-agent`
//! (runs inside the guest) and `ephemera-vsock-client` (runs on the host), so
//! the two sides can never drift out of sync on the message shapes.

use serde::{Deserialize, Serialize};

/// Default AF_VSOCK port the guest agent listens on.
pub const DEFAULT_PORT: u32 = 17777;

/// Default exec timeout when a request doesn't specify one.
pub const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 30;

/// Where the guest agent looks for its shared-secret token (see
/// [`Envelope`]). Written into the guest's own disk *before* boot by
/// `ephemera_image::inject_guest_agent_token`, so it's already in place by
/// the time the agent's systemd unit starts. Absent -> the agent runs
/// unauthenticated (only true for VMs created before this existed, or with
/// `agent.enabled: false`).
pub const TOKEN_FILE_PATH: &str = "/etc/ephemera-guest-agent.token";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum AgentRequest {
    Ping,
    Exec {
        command: String,
        #[serde(default)]
        timeout_seconds: Option<u64>,
    },
    Shutdown,
}

/// Every request the agent authored by `ephemera-vsock-client` is wrapped in
/// this envelope. `token` is checked against the file at [`TOKEN_FILE_PATH`]
/// before `request` is acted on — this is what stops any *other* process on
/// the host (anything that can open a raw AF_VSOCK socket to the same CID,
/// bypassing the ephemera daemon/CLI entirely) from running commands in the
/// guest as root. It's a shared secret over a host-local transport, not a
/// substitute for REST-layer auth/RBAC (see `ephemera-api`'s `Role`) — those
/// answer different questions ("can this human/service call ephemera at
/// all") vs. ("is this vsock caller actually ephemera").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(flatten)]
    pub request: AgentRequest,
}

impl Envelope {
    pub fn new(token: Option<String>, request: AgentRequest) -> Self {
        Self { token, request }
    }
}

/// Constant-time comparison so a mismatched guest-agent token can't be
/// brute-forced via response-time measurement. Zero-dependency by design —
/// `ephemera-guest-agent` deliberately stays a minimal, small guest binary
/// (see its Cargo.toml), so this doesn't pull in a crypto crate for one
/// tiny comparison.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum AgentResponse {
    Pong,
    Exec {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    ShuttingDown,
    Error {
        message: String,
    },
}

/// Serializes `value` as one line of JSON terminated by `\n`, ready to write
/// directly to a socket.
pub fn encode_line<T: Serialize>(value: &T) -> serde_json::Result<String> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

/// Parses one previously-`encode_line`d JSON line (trailing newline
/// tolerated but not required).
pub fn decode_line<T: for<'de> Deserialize<'de>>(line: &str) -> serde_json::Result<T> {
    serde_json::from_str(line.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_only_identical_strings() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "wrong"));
        assert!(!constant_time_eq("secret", "secre"));
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn envelope_round_trips_with_flattened_request() {
        let env = Envelope::new(Some("tok".into()), AgentRequest::Exec { command: "echo hi".into(), timeout_seconds: Some(5) });
        let line = encode_line(&env).unwrap();
        assert!(line.contains("\"token\":\"tok\""));
        assert!(line.contains("\"op\":\"exec\""));
        let back: Envelope = decode_line(&line).unwrap();
        assert_eq!(back.token.as_deref(), Some("tok"));
        match back.request {
            AgentRequest::Exec { command, timeout_seconds } => {
                assert_eq!(command, "echo hi");
                assert_eq!(timeout_seconds, Some(5));
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn envelope_with_no_token_still_parses() {
        let line = "{\"op\":\"ping\"}\n";
        let env: Envelope = decode_line(line).unwrap();
        assert!(env.token.is_none());
        assert!(matches!(env.request, AgentRequest::Ping));
    }
}
