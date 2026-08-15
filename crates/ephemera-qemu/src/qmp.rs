// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! Minimal QMP client: connects fresh for each call (QEMU's QMP chardev
//! handles that fine for the low command rate here), does the required
//! capabilities-negotiation handshake, sends one command, and returns its
//! `return` value. Every connect/read/write is bounded by `timeout` — the
//! original draft this was ported from had none, which meant a wedged QEMU
//! process could hang the caller forever.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub async fn execute(socket: &Path, command: &str, args: Option<Value>, timeout: Duration) -> Result<Value> {
    tokio::time::timeout(timeout, execute_inner(socket, command, args))
        .await
        .with_context(|| format!("QMP {command} timed out after {timeout:?}"))?
}

async fn execute_inner(socket: &Path, command: &str, args: Option<Value>) -> Result<Value> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to QMP socket {}", socket.display()))?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Greeting: QEMU sends {"QMP": {...}} immediately on connect.
    let mut greeting = String::new();
    reader.read_line(&mut greeting).await.context("reading QMP greeting")?;
    let greeting: Value = serde_json::from_str(&greeting).context("parsing QMP greeting")?;
    if greeting.get("QMP").is_none() {
        bail!("unexpected QMP greeting: {greeting}");
    }

    // Capabilities negotiation is required before any other command works.
    write_half
        .write_all(b"{\"execute\":\"qmp_capabilities\"}\n")
        .await
        .context("sending qmp_capabilities")?;
    let mut cap_response = String::new();
    reader.read_line(&mut cap_response).await.context("reading qmp_capabilities response")?;
    let cap_response: Value = serde_json::from_str(&cap_response).context("parsing qmp_capabilities response")?;
    if let Some(err) = cap_response.get("error") {
        bail!("qmp_capabilities rejected: {err}");
    }

    let mut request = json!({"execute": command});
    if let Some(args) = args {
        request["arguments"] = args;
    }
    write_half
        .write_all(format!("{request}\n").as_bytes())
        .await
        .with_context(|| format!("sending QMP command {command}"))?;

    // QEMU may interleave asynchronous "event" lines before the command's
    // own reply; skip those rather than treating the first line as the
    // answer (the draft's real gap: it discarded events in an unbounded
    // loop with no cap — here the outer `timeout` in `execute` is the cap).
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.context("reading QMP response")? == 0 {
            bail!("QEMU closed the QMP connection without responding to {command}");
        }
        let response: Value = serde_json::from_str(&line).with_context(|| format!("parsing QMP response: {line}"))?;
        if response.get("event").is_some() {
            continue;
        }
        if let Some(err) = response.get("error") {
            bail!("QMP {command} failed: {err}");
        }
        return Ok(response.get("return").cloned().unwrap_or(Value::Null));
    }
}
