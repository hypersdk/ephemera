// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! Dials a booted VM's guest agent over AF_VSOCK and exchanges one
//! request/response pair.
//!
//! QEMU exposes a real kernel `vhost-vsock-pci` device, so the host connects
//! via a native `AF_VSOCK` socket straight to the guest's CID. Cloud
//! Hypervisor and Firecracker instead expose a Unix domain socket that
//! proxies vsock connections: the host connects to that UDS and sends
//! `CONNECT <port>\n`, which the VMM answers with `OK <n>\n` before the
//! socket becomes a raw byte-stream to the guest's listening port.

use anyhow::{bail, Context, Result};
use ephemera_core::model::{BackendKind, VmRecord};
use ephemera_guest_protocol::{decode_line, encode_line, AgentRequest, AgentResponse, Envelope};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Dials `vm`'s guest agent and returns its response to `request`, bounded
/// by `timeout` (the whole round trip: connect + handshake + exchange).
/// `vm.request.agent.token` (if set — see `AgentSpec::token`) rides along in
/// every request automatically; callers never need to supply it themselves.
pub async fn call(vm: &VmRecord, request: AgentRequest, timeout: Duration) -> Result<AgentResponse> {
    let agent = vm.request.agent.as_ref().filter(|a| a.enabled).context("guest agent is not enabled for this VM")?;
    let port = agent.port;
    let envelope = Envelope::new(agent.token.clone(), request);
    let cid = vm.guest_cid.context("VM has no vsock CID assigned")?;

    tokio::time::timeout(timeout, async {
        match vm.backend {
            BackendKind::Qemu => native_vsock_call(cid, port, &envelope).await,
            BackendKind::CloudHypervisor | BackendKind::Firecracker => {
                let socket = vm.vsock_socket.as_deref().context("VM has no vsock proxy socket recorded")?;
                uds_proxy_call(socket, port, &envelope).await
            }
            BackendKind::Auto => bail!("VM has an unresolved BackendKind::Auto — this is a bug, backend selection must happen before a VM is persisted"),
        }
    })
    .await
    .context("guest agent call timed out")?
}

async fn uds_proxy_call(socket: &std::path::Path, guest_port: u32, envelope: &Envelope) -> Result<AgentResponse> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to vsock proxy socket {}", socket.display()))?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    write_half
        .write_all(format!("CONNECT {guest_port}\n").as_bytes())
        .await
        .context("sending vsock CONNECT")?;

    let mut ack = String::new();
    reader.read_line(&mut ack).await.context("reading vsock CONNECT ack")?;
    let ack = ack.trim();
    if !ack.to_ascii_uppercase().starts_with("OK") {
        bail!("vsock proxy refused CONNECT {guest_port}: {ack:?}");
    }

    write_half
        .write_all(encode_line(envelope)?.as_bytes())
        .await
        .context("writing agent request")?;

    let mut response_line = String::new();
    reader
        .read_line(&mut response_line)
        .await
        .context("reading agent response")?;
    if response_line.is_empty() {
        bail!("guest agent closed the connection without responding");
    }
    decode_line(&response_line).context("parsing agent response")
}

#[cfg(target_os = "linux")]
async fn native_vsock_call(cid: u32, port: u32, envelope: &Envelope) -> Result<AgentResponse> {
    let envelope = envelope.clone();
    tokio::task::spawn_blocking(move || native_vsock_call_blocking(cid, port, &envelope))
        .await
        .context("vsock worker thread panicked")?
}

#[cfg(target_os = "linux")]
fn native_vsock_call_blocking(cid: u32, port: u32, envelope: &Envelope) -> Result<AgentResponse> {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;

    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
        if fd < 0 {
            bail!("socket(AF_VSOCK): {}", std::io::Error::last_os_error());
        }
        let mut file = std::fs::File::from_raw_fd(fd);

        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_cid = cid;
        addr.svm_port = port;

        let rc = libc::connect(
            fd,
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        );
        if rc != 0 {
            bail!(
                "connect(vsock cid={cid} port={port}): {}",
                std::io::Error::last_os_error()
            );
        }

        // Bound the blocking read/write calls below at the socket level,
        // since this runs off the async runtime with no other cancellation.
        let tv = libc::timeval { tv_sec: 10, tv_usec: 0 };
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );

        file.write_all(encode_line(envelope)?.as_bytes())
            .context("writing agent request over vsock")?;

        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = file.read(&mut byte).context("reading agent response over vsock")?;
            if n == 0 || byte[0] == b'\n' {
                break;
            }
            buf.push(byte[0]);
        }
        if buf.is_empty() {
            bail!("guest agent closed the vsock connection without responding");
        }
        decode_line(&String::from_utf8_lossy(&buf)).context("parsing agent response")
    }
}

#[cfg(not(target_os = "linux"))]
async fn native_vsock_call(_cid: u32, _port: u32, _envelope: &Envelope) -> Result<AgentResponse> {
    bail!("native AF_VSOCK is only supported on Linux")
}

/// Convenience wrapper: sends `AgentRequest::Ping`, returns `Ok(())` if the
/// agent answered `Pong`.
pub async fn ping(vm: &VmRecord, timeout: Duration) -> Result<()> {
    match call(vm, AgentRequest::Ping, timeout).await? {
        AgentResponse::Pong => Ok(()),
        AgentResponse::Error { message } => bail!("guest agent error: {message}"),
        other => bail!("unexpected response to ping: {other:?}"),
    }
}

pub const DEFAULT_CALL_TIMEOUT: Duration = DEFAULT_TIMEOUT;

/// A live, already-authenticated interactive shell session — see
/// [`open_shell`]. Implements `AsyncRead`/`AsyncWrite` directly: after
/// construction there is no more protocol framing, just the guest's raw
/// PTY byte stream (matches `AgentRequest::OpenShell`'s doc comment on the
/// guest-agent side of this same connection).
pub enum ConsoleStream {
    Native(NativeVsockStream),
    Proxy(UnixStream),
}

impl tokio::io::AsyncRead for ConsoleStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ConsoleStream::Native(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            ConsoleStream::Proxy(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for ConsoleStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            ConsoleStream::Native(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            ConsoleStream::Proxy(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ConsoleStream::Native(s) => std::pin::Pin::new(s).poll_flush(cx),
            ConsoleStream::Proxy(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            ConsoleStream::Native(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            ConsoleStream::Proxy(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Opens an interactive shell on `vm` and returns the raw byte stream once
/// the guest agent has acked `ShellOpened` — everything read from/written
/// to the returned stream after that point is PTY traffic, not JSON. Bound
/// only the handshake by `timeout`; the returned stream itself has no
/// timeout (an interactive session has no natural deadline).
pub async fn open_shell(vm: &VmRecord, cols: u16, rows: u16, timeout: Duration) -> Result<ConsoleStream> {
    let agent = vm.request.agent.as_ref().filter(|a| a.enabled).context("guest agent is not enabled for this VM")?;
    let port = agent.port;
    let envelope = Envelope::new(agent.token.clone(), AgentRequest::OpenShell { cols, rows });

    tokio::time::timeout(timeout, async {
        match vm.backend {
            BackendKind::Qemu => {
                let cid = vm.guest_cid.context("VM has no vsock CID assigned")?;
                open_shell_native(cid, port, &envelope).await
            }
            BackendKind::CloudHypervisor | BackendKind::Firecracker => {
                let socket = vm.vsock_socket.as_deref().context("VM has no vsock proxy socket recorded")?;
                open_shell_proxy(socket, port, &envelope).await
            }
            BackendKind::Auto => bail!("VM has an unresolved BackendKind::Auto — this is a bug, backend selection must happen before a VM is persisted"),
        }
    })
    .await
    .context("opening interactive shell timed out")?
}

async fn open_shell_proxy(socket: &std::path::Path, guest_port: u32, envelope: &Envelope) -> Result<ConsoleStream> {
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to vsock proxy socket {}", socket.display()))?;

    stream.write_all(format!("CONNECT {guest_port}\n").as_bytes()).await.context("sending vsock CONNECT")?;

    // Read the CONNECT ack and the ShellOpened response line-by-line first
    // (both still JSON/text-framed), then hand back the same stream raw —
    // no BufReader wrapper, since anything it buffered ahead past those
    // two lines would otherwise be silently dropped instead of forwarded.
    let mut line = Vec::new();
    read_line_raw(&mut stream, &mut line).await.context("reading vsock CONNECT ack")?;
    let ack = String::from_utf8_lossy(&line);
    if !ack.trim().to_ascii_uppercase().starts_with("OK") {
        bail!("vsock proxy refused CONNECT {guest_port}: {:?}", ack.trim());
    }

    stream.write_all(encode_line(envelope)?.as_bytes()).await.context("writing OpenShell request")?;

    line.clear();
    read_line_raw(&mut stream, &mut line).await.context("reading ShellOpened response")?;
    if line.is_empty() {
        bail!("guest agent closed the connection without responding");
    }
    match decode_line::<AgentResponse>(&String::from_utf8_lossy(&line)).context("parsing ShellOpened response")? {
        AgentResponse::ShellOpened => Ok(ConsoleStream::Proxy(stream)),
        AgentResponse::Error { message } => bail!("guest agent error: {message}"),
        other => bail!("unexpected response to OpenShell: {other:?}"),
    }
}

/// Reads one `\n`-terminated line a single byte at a time — deliberately
/// not `AsyncBufReadExt::read_line`, which would risk consuming raw PTY
/// bytes into its internal buffer past the ShellOpened line with no way to
/// hand them back once this function returns.
async fn read_line_raw(stream: &mut UnixStream, out: &mut Vec<u8>) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 || byte[0] == b'\n' {
            return Ok(());
        }
        out.push(byte[0]);
    }
}

#[cfg(target_os = "linux")]
pub struct NativeVsockStream(tokio::io::unix::AsyncFd<std::fs::File>);

#[cfg(target_os = "linux")]
impl tokio::io::AsyncRead for NativeVsockStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        loop {
            let mut guard = match self.0.poll_read_ready(cx) {
                std::task::Poll::Ready(Ok(g)) => g,
                std::task::Poll::Ready(Err(e)) => return std::task::Poll::Ready(Err(e)),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            };
            match guard.try_io(|inner| {
                use std::io::Read;
                inner.get_ref().read(buf.initialize_unfilled())
            }) {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return std::task::Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return std::task::Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl tokio::io::AsyncWrite for NativeVsockStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        loop {
            let mut guard = match self.0.poll_write_ready(cx) {
                std::task::Poll::Ready(Ok(g)) => g,
                std::task::Poll::Ready(Err(e)) => return std::task::Poll::Ready(Err(e)),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            };
            match guard.try_io(|inner| {
                use std::io::Write;
                inner.get_ref().write(buf)
            }) {
                Ok(result) => return std::task::Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }
    fn poll_flush(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[cfg(target_os = "linux")]
async fn open_shell_native(cid: u32, port: u32, envelope: &Envelope) -> Result<ConsoleStream> {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;

    let envelope = envelope.clone();
    let file: std::fs::File = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
        unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
            if fd < 0 {
                bail!("socket(AF_VSOCK): {}", std::io::Error::last_os_error());
            }
            let mut file = std::fs::File::from_raw_fd(fd);

            let mut addr: libc::sockaddr_vm = std::mem::zeroed();
            addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
            addr.svm_cid = cid;
            addr.svm_port = port;
            if libc::connect(fd, &addr as *const libc::sockaddr_vm as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t) != 0 {
                bail!("connect(vsock cid={cid} port={port}): {}", std::io::Error::last_os_error());
            }

            let tv = libc::timeval { tv_sec: 15, tv_usec: 0 };
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv as *const _ as *const libc::c_void, std::mem::size_of::<libc::timeval>() as libc::socklen_t);
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_SNDTIMEO, &tv as *const _ as *const libc::c_void, std::mem::size_of::<libc::timeval>() as libc::socklen_t);

            file.write_all(encode_line(&envelope)?.as_bytes()).context("writing OpenShell request")?;

            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = file.read(&mut byte).context("reading ShellOpened response")?;
                if n == 0 || byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            if line.is_empty() {
                bail!("guest agent closed the vsock connection without responding");
            }
            match decode_line::<AgentResponse>(&String::from_utf8_lossy(&line)).context("parsing ShellOpened response")? {
                AgentResponse::ShellOpened => {}
                AgentResponse::Error { message } => bail!("guest agent error: {message}"),
                other => bail!("unexpected response to OpenShell: {other:?}"),
            }

            // Handshake is done blocking (simplest for the line-at-a-time
            // read above); the interactive session itself needs to be
            // async so it can run concurrently with a WebSocket relay —
            // clear the blocking-call timeouts and switch to nonblocking
            // for AsyncFd, which requires it.
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &libc::timeval { tv_sec: 0, tv_usec: 0 } as *const _ as *const libc::c_void, std::mem::size_of::<libc::timeval>() as libc::socklen_t);
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);

            Ok(file)
        }
    })
    .await
    .context("vsock worker thread panicked")??;

    Ok(ConsoleStream::Native(NativeVsockStream(tokio::io::unix::AsyncFd::new(file)?)))
}

#[cfg(not(target_os = "linux"))]
async fn open_shell_native(_cid: u32, _port: u32, _envelope: &Envelope) -> Result<ConsoleStream> {
    bail!("native AF_VSOCK is only supported on Linux")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    /// Stands in for a Cloud Hypervisor/Firecracker vsock proxy: accepts
    /// one connection, answers the CONNECT handshake, decodes one
    /// OpenShell request, acks ShellOpened, then echoes everything after
    /// that byte-for-byte — enough to prove `open_shell_proxy` correctly
    /// stops JSON-framing at the right point and hands back a stream that
    /// carries genuinely raw bytes from there on.
    async fn fake_proxy_echo_server(path: std::path::PathBuf) {
        let listener = UnixListener::bind(&path).unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();

        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).await.unwrap();
            if byte[0] == b'\n' { break; }
            line.push(byte[0]);
        }
        assert!(String::from_utf8_lossy(&line).starts_with("CONNECT "));
        stream.write_all(b"OK 1234\n").await.unwrap();

        line.clear();
        loop {
            stream.read_exact(&mut byte).await.unwrap();
            if byte[0] == b'\n' { break; }
            line.push(byte[0]);
        }
        let req: AgentRequest = decode_line(&String::from_utf8_lossy(&line)).unwrap();
        assert!(matches!(req, AgentRequest::OpenShell { .. }));
        stream.write_all(encode_line(&AgentResponse::ShellOpened).unwrap().as_bytes()).await.unwrap();

        let mut buf = [0u8; 1024];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stream.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn open_shell_proxy_hands_back_a_raw_stream_after_the_handshake() {
        let path = std::env::temp_dir().join(format!("eph-vsock-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let server = tokio::spawn(fake_proxy_echo_server(path.clone()));
        // Give the listener a moment to bind before connecting.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let envelope = Envelope::new(None, AgentRequest::OpenShell { cols: 80, rows: 24 });
        let mut stream = open_shell_proxy(&path, 17777, &envelope).await.unwrap();

        stream.write_all(b"echo hi\n").await.unwrap();
        let mut buf = [0u8; 8];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"echo hi\n");

        drop(stream);
        let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
        let _ = std::fs::remove_file(&path);
    }
}
