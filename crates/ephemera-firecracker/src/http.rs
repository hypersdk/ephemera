// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! Minimal HTTP/1.1-over-Unix-socket client for Firecracker's API. Firecracker
//! has no crate-friendly UDS transport (`reqwest` doesn't speak Unix
//! sockets), so this is hand-rolled — but bounded by `timeout` and reading
//! incrementally (headers, then exactly `Content-Length` more bytes) rather
//! than waiting for the peer to close the connection: Firecracker keeps its
//! API connections open (a `Connection: close` request header is only a
//! hint, and Firecracker doesn't act on it), so a `read_to_end`-based client
//! — which is what the draft this was ported from used, and what an earlier
//! version of this file still effectively did despite claiming otherwise —
//! just hangs until the caller's own timeout fires, even though the request
//! already succeeded.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub async fn request(socket: &Path, method: &str, path: &str, body: Option<&Value>, timeout: Duration) -> Result<()> {
    tokio::time::timeout(timeout, request_inner(socket, method, path, body))
        .await
        .with_context(|| format!("Firecracker {method} {path} timed out after {timeout:?}"))?
}

async fn request_inner(socket: &Path, method: &str, path: &str, body: Option<&Value>) -> Result<()> {
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to Firecracker API socket {}", socket.display()))?;

    let body_bytes = body.map(|b| serde_json::to_vec(b)).transpose()?.unwrap_or_default();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body_bytes.len()
    );
    if !body_bytes.is_empty() {
        request.push_str("Content-Type: application/json\r\n");
    }
    request.push_str("\r\n");

    stream.write_all(request.as_bytes()).await.context("writing request")?;
    stream.write_all(&body_bytes).await.context("writing request body")?;

    // Read incrementally until the header terminator shows up, buffering
    // whatever body bytes happen to arrive in the same read as a bonus —
    // never wait for the connection to close.
    let mut raw = Vec::new();
    let boundary = loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.context("reading response headers")?;
        if n == 0 {
            bail!("Firecracker closed the connection before sending a complete response header");
        }
        raw.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&raw[..boundary]).into_owned();
    let mut body = raw.split_off(boundary + 4);

    let mut lines = head.lines();
    let status_line = lines.next().context("malformed HTTP response: no status line")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .context("malformed HTTP status line")?
        .parse()
        .context("non-numeric HTTP status code")?;

    let content_length: usize = lines
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);

    // We may already have some (or all) of the body from the header read
    // above; only read more if there's a known-length shortfall.
    while body.len() < content_length {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.context("reading response body")?;
        if n == 0 {
            bail!("Firecracker closed the connection with only {}/{content_length} body bytes sent", body.len());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    // The loop above only exits once body.len() >= content_length; trim any
    // extra bytes the last chunk happened to carry past the body boundary.
    body.truncate(content_length);

    if !(200..300).contains(&status) {
        bail!("Firecracker {method} {path} -> HTTP {status}: {}", String::from_utf8_lossy(&body));
    }
    Ok(())
}
