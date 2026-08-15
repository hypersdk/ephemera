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
use ephemera_guest_protocol::{decode_line, encode_line, AgentRequest, AgentResponse};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Dials `vm`'s guest agent and returns its response to `request`, bounded
/// by `timeout` (the whole round trip: connect + handshake + exchange).
pub async fn call(vm: &VmRecord, request: AgentRequest, timeout: Duration) -> Result<AgentResponse> {
    let port = vm
        .request
        .agent
        .as_ref()
        .filter(|a| a.enabled)
        .map(|a| a.port)
        .context("guest agent is not enabled for this VM")?;
    let cid = vm.guest_cid.context("VM has no vsock CID assigned")?;

    tokio::time::timeout(timeout, async {
        match vm.backend {
            BackendKind::Qemu => native_vsock_call(cid, port, &request).await,
            BackendKind::CloudHypervisor | BackendKind::Firecracker => {
                let socket = vm.workspace.join("vsock.sock");
                uds_proxy_call(&socket, port, &request).await
            }
        }
    })
    .await
    .context("guest agent call timed out")?
}

async fn uds_proxy_call(socket: &std::path::Path, guest_port: u32, request: &AgentRequest) -> Result<AgentResponse> {
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
        .write_all(encode_line(request)?.as_bytes())
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
async fn native_vsock_call(cid: u32, port: u32, request: &AgentRequest) -> Result<AgentResponse> {
    let request = request.clone();
    tokio::task::spawn_blocking(move || native_vsock_call_blocking(cid, port, &request))
        .await
        .context("vsock worker thread panicked")?
}

#[cfg(target_os = "linux")]
fn native_vsock_call_blocking(cid: u32, port: u32, request: &AgentRequest) -> Result<AgentResponse> {
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

        file.write_all(encode_line(request)?.as_bytes())
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
async fn native_vsock_call(_cid: u32, _port: u32, _request: &AgentRequest) -> Result<AgentResponse> {
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
