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
