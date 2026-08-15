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
use std::{io::ErrorKind, path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::UnixStream;

pub async fn execute(socket: &Path, command: &str, args: Option<Value>, timeout: Duration) -> Result<Value> {
    tokio::time::timeout(timeout, execute_inner(socket, command, args))
        .await
        .with_context(|| format!("QMP {command} timed out after {timeout:?}"))?
}

/// True for I/O errors worth retrying the *whole handshake* (not just the
/// connect) from scratch. QEMU is started with `-qmp
/// unix:...,server=on,wait=off`, so by the time `launch()` returns its pid,
/// the socket file may not exist yet (`NotFound`/`ConnectionRefused` — the
/// listener isn't up), AND, observed on real hardware, a connection made
/// very early can be accepted and then dropped mid-handshake while QEMU's
/// device model is still initializing (`ConnectionReset`/`BrokenPipe`/a
/// clean EOF where a response line was expected). Both cases mean "ask
/// again shortly," not "QEMU rejected this."
fn is_transient_startup_error(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::NotFound | ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof
    )
}

type QmpHalves = (BufReader<ReadHalf<UnixStream>>, WriteHalf<UnixStream>);

/// Connect + read the greeting + negotiate capabilities, retrying the
/// *entire* sequence from a fresh connection on any transient error (see
/// `is_transient_startup_error`) — never just the failed step, since a
/// half-completed handshake on a reset connection can't be resumed. Bounded
/// only by the caller's overall `timeout` via the wrapping
/// `tokio::time::timeout` in `execute`. Never retries once the actual
/// command has been sent (see `execute_inner`) — a command like
/// `system_powerdown` isn't safe to risk sending twice.
async fn handshake_retrying(socket: &Path) -> Result<QmpHalves> {
    let mut delay = Duration::from_millis(20);
    loop {
        match try_handshake(socket).await {
            Ok(halves) => return Ok(halves),
            Err(e) if is_transient_startup_error(e.kind()) => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_millis(500));
            }
            Err(e) => return Err(e).with_context(|| format!("QMP handshake with {}", socket.display())),
        }
    }
}

async fn try_handshake(socket: &Path) -> std::io::Result<QmpHalves> {
    let stream = UnixStream::connect(socket).await?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    // Greeting: QEMU sends {"QMP": {...}} immediately on connect.
    let mut greeting = String::new();
    if reader.read_line(&mut greeting).await? == 0 {
        return Err(std::io::Error::new(ErrorKind::UnexpectedEof, "connection closed before QMP greeting"));
    }
    if serde_json::from_str::<Value>(&greeting).ok().and_then(|v| v.get("QMP").cloned()).is_none() {
        return Err(std::io::Error::new(ErrorKind::InvalidData, format!("unexpected QMP greeting: {greeting:?}")));
    }

    // Capabilities negotiation is required before any other command works.
    write_half.write_all(b"{\"execute\":\"qmp_capabilities\"}\n").await?;
    let mut cap_response = String::new();
    if reader.read_line(&mut cap_response).await? == 0 {
        return Err(std::io::Error::new(ErrorKind::UnexpectedEof, "connection closed before qmp_capabilities response"));
    }
    if let Some(err) = serde_json::from_str::<Value>(&cap_response).ok().and_then(|v| v.get("error").cloned()) {
        return Err(std::io::Error::new(ErrorKind::InvalidData, format!("qmp_capabilities rejected: {err}")));
    }

    Ok((reader, write_half))
}

async fn execute_inner(socket: &Path, command: &str, args: Option<Value>) -> Result<Value> {
    let (mut reader, mut write_half) = handshake_retrying(socket).await?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    async fn serve_one_command(listener: UnixListener, command: &'static str) {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        write_half.write_all(b"{\"QMP\":{\"version\":{}}}\n").await.unwrap();
        let mut caps = String::new();
        reader.read_line(&mut caps).await.unwrap();
        write_half.write_all(b"{\"return\":{}}\n").await.unwrap();

        let mut req = String::new();
        reader.read_line(&mut req).await.unwrap();
        let req: Value = serde_json::from_str(&req).unwrap();
        assert_eq!(req["execute"], command);
        write_half.write_all(b"{\"return\":{\"status\":\"ok\"}}\n").await.unwrap();

        // Keep the connection open briefly so the client's read isn't racing
        // a closed socket; then just let it drop.
        let mut buf = [0u8; 1];
        let _ = tokio::time::timeout(Duration::from_millis(50), reader.read(&mut buf)).await;
    }

    #[tokio::test]
    async fn succeeds_immediately_when_the_socket_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qmp.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(serve_one_command(listener, "stop"));

        let result = execute(&path, "stop", None, Duration::from_secs(5)).await.unwrap();
        assert_eq!(result["status"], "ok");
        server.await.unwrap();
    }

    /// Regression test: the real bug was that `create()` returns as soon as
    /// QEMU is spawned (`-qmp ...,wait=off`), before QEMU has necessarily
    /// created the QMP socket file at all — a caller pausing immediately
    /// afterward (a warm pool's backfill loop, with no delay in between)
    /// hit "connecting to QMP socket: No such file or directory" on real
    /// hardware. This simulates that exact window: the socket file doesn't
    /// exist yet when `execute` is first called.
    #[tokio::test]
    async fn retries_the_connect_until_the_socket_appears() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qmp.sock");
        // Deliberately do NOT bind yet — execute() must retry past ENOENT.
        let path_for_server = path.clone();
        let server = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let listener = UnixListener::bind(&path_for_server).unwrap();
            serve_one_command(listener, "cont").await;
        });

        let result = execute(&path, "cont", None, Duration::from_secs(5)).await.unwrap();
        assert_eq!(result["status"], "ok");
        server.await.unwrap();
    }

    /// Regression test for a second, later-stage instance of the same real
    /// bug: on real hardware, even after the connect-retry fix above, a
    /// warm pool's create-then-immediately-pause sequence still failed —
    /// this time the connection was accepted and the greeting arrived, but
    /// QEMU reset the connection while still negotiating capabilities
    /// (device model not fully up yet). Simulates a listener that resets
    /// the first connection mid-handshake and only completes a real
    /// handshake on the second — `execute` must retry the *whole* thing,
    /// not just the connect.
    #[tokio::test]
    async fn retries_the_whole_handshake_after_a_mid_handshake_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qmp.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            // First connection: send the greeting, then drop without ever
            // answering qmp_capabilities — the client sees EOF/reset mid-handshake.
            let (stream, _) = listener.accept().await.unwrap();
            let (_read_half, mut write_half) = stream.into_split();
            write_half.write_all(b"{\"QMP\":{\"version\":{}}}\n").await.unwrap();
            drop(write_half);

            // Second connection: a normal, complete handshake + command.
            serve_one_command(listener, "stop").await;
        });

        let result = execute(&path, "stop", None, Duration::from_secs(5)).await.unwrap();
        assert_eq!(result["status"], "ok");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn gives_up_once_the_overall_timeout_elapses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qmp.sock"); // never created
        let err = execute(&path, "stop", None, Duration::from_millis(150)).await.unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "unexpected error: {err:#}");
    }
}
