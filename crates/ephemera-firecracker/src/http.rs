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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    /// Spawns a one-shot fake Firecracker: accepts a single connection,
    /// reads (and discards) the request, then runs `respond` to write
    /// whatever response bytes the test wants — in whatever order/timing it
    /// wants — without ever closing the connection itself (Firecracker
    /// doesn't; a client that waits for connection-close instead of
    /// Content-Length hangs against it, which is exactly the bug this
    /// module exists to avoid).
    fn spawn_fake_firecracker<F, Fut>(respond: F) -> std::path::PathBuf
    where
        F: FnOnce(tokio::net::UnixStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("firecracker.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Drain the request so the client's writes don't block on a full pipe.
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            respond(stream).await;
            // Keep dir (and the socket file) alive for the test's duration
            // by leaking it here; the tempdir is cleaned up when the whole
            // test process exits, which is fine for a test.
            std::mem::forget(dir);
        });
        sock_path
    }

    #[tokio::test]
    async fn succeeds_on_204_with_connection_kept_open() {
        // This is the exact scenario that hung the original implementation:
        // a real 204 response, connection left open afterward (Firecracker's
        // actual behavior). A client waiting for EOF would block here until
        // the caller's timeout fired despite the request having succeeded.
        let sock = spawn_fake_firecracker(|mut stream| async move {
            stream.write_all(b"HTTP/1.1 204 \r\nServer: Firecracker API\r\nConnection: keep-alive\r\n\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await; // never closes on its own
        });

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            request(&sock, "PATCH", "/vm", Some(&serde_json::json!({"state": "Paused"})), Duration::from_secs(5)),
        )
        .await;

        assert!(result.is_ok(), "request() should return well before the connection ever closes");
        result.unwrap().unwrap();
    }

    #[tokio::test]
    async fn body_arriving_in_a_later_read_is_still_assembled_correctly() {
        let sock = spawn_fake_firecracker(|mut stream| async move {
            stream.write_all(b"HTTP/1.1 400 \r\nContent-Length: 13\r\n\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await; // body shows up in a separate read
            stream.write_all(b"bad-request!!").await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let err = request(&sock, "PUT", "/actions", None, Duration::from_secs(5)).await.unwrap_err();
        assert!(err.to_string().contains("bad-request!!"), "error should carry the exact body text: {err}");
    }

    #[tokio::test]
    async fn errors_cleanly_if_peer_closes_before_headers_complete() {
        let sock = spawn_fake_firecracker(|mut stream| async move {
            stream.write_all(b"HTTP/1.1 20").await.unwrap(); // truncated mid-header
            // stream drops here, closing the connection
        });

        let err = request(&sock, "GET", "/", None, Duration::from_secs(5)).await.unwrap_err();
        assert!(err.to_string().contains("closed the connection"), "unexpected error: {err}");
    }
}
