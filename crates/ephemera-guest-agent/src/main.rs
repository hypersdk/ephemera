// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! Runs inside the guest: accepts one AF_VSOCK connection at a time (spawning
//! a thread per connection), reads one newline-delimited JSON request, acts
//! on it, and writes one newline-delimited JSON response.

use anyhow::Result;
use clap::Parser;
use ephemera_guest_protocol::DEFAULT_PORT;
#[cfg(target_os = "linux")]
use anyhow::Context;
#[cfg(target_os = "linux")]
use ephemera_guest_protocol::{
    constant_time_eq, decode_line, encode_line, AgentRequest, AgentResponse, Envelope,
    DEFAULT_EXEC_TIMEOUT_SECS, MAX_FILE_TRANSFER_BYTES, TOKEN_FILE_PATH,
};
#[cfg(target_os = "linux")]
use base64::{engine::general_purpose::STANDARD as B64, Engine};
#[cfg(target_os = "linux")]
use std::io::{BufRead, BufReader, Write};
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "ephemera-guest-agent", version, about = "Zyvor Ephemera in-guest agent")]
struct Cli {
    /// AF_VSOCK port to listen on.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u32,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    run_server(cli.port)
}

#[cfg(target_os = "linux")]
fn run_server(port: u32) -> Result<()> {
    use std::os::fd::FromRawFd;

    let expected_token: Arc<Option<String>> = Arc::new(
        std::fs::read_to_string(TOKEN_FILE_PATH)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    );
    if expected_token.is_some() {
        eprintln!("ephemera-guest-agent: token found at {TOKEN_FILE_PATH}, requests must authenticate");
    } else {
        eprintln!("ephemera-guest-agent: WARNING no token at {TOKEN_FILE_PATH} — running unauthenticated, any vsock caller can run commands as root");
    }

    let listener_fd = unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
        if fd < 0 {
            anyhow::bail!("socket(AF_VSOCK): {}", std::io::Error::last_os_error());
        }

        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_cid = libc::VMADDR_CID_ANY;
        addr.svm_port = port;

        if libc::bind(
            fd,
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        ) != 0
        {
            anyhow::bail!("bind(vsock port={port}): {}", std::io::Error::last_os_error());
        }
        if libc::listen(fd, 16) != 0 {
            anyhow::bail!("listen: {}", std::io::Error::last_os_error());
        }
        fd
    };

    eprintln!("ephemera-guest-agent listening on vsock port {port}");

    loop {
        let conn_fd = unsafe { libc::accept(listener_fd, std::ptr::null_mut(), std::ptr::null_mut()) };
        if conn_fd < 0 {
            eprintln!("accept failed: {}", std::io::Error::last_os_error());
            continue;
        }
        let expected_token = expected_token.clone();
        std::thread::spawn(move || {
            let file = unsafe { std::fs::File::from_raw_fd(conn_fd) };
            if let Err(e) = handle_connection(file, &expected_token) {
                eprintln!("connection error: {e:#}");
            }
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn run_server(_port: u32) -> Result<()> {
    anyhow::bail!("ephemera-guest-agent only supports Linux (AF_VSOCK)")
}

#[cfg(target_os = "linux")]
fn handle_connection(file: std::fs::File, expected_token: &Option<String>) -> Result<()> {
    let mut writer = file.try_clone().context("cloning connection fd")?;
    let mut reader = BufReader::new(file);

    let mut line = String::new();
    if reader.read_line(&mut line).context("reading request")? == 0 {
        return Ok(()); // peer closed without sending anything
    }
    let envelope: Envelope = decode_line(&line).context("parsing request")?;

    if let Some(expected) = expected_token {
        let authorized = envelope.token.as_deref().is_some_and(|got| constant_time_eq(expected, got));
        if !authorized {
            writer.write_all(encode_line(&AgentResponse::Error { message: "unauthorized: missing or incorrect token".into() })?.as_bytes())?;
            writer.flush()?;
            return Ok(());
        }
    }

    let response = match envelope.request {
        AgentRequest::Ping => AgentResponse::Pong,
        AgentRequest::Exec { command, timeout_seconds } => {
            exec_with_timeout(&command, Duration::from_secs(timeout_seconds.unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS)))
        }
        AgentRequest::PutFile { path, content_base64, mode } => put_file(&path, &content_base64, mode),
        AgentRequest::GetFile { path } => get_file(&path),
        AgentRequest::Shutdown => {
            writer.write_all(encode_line(&AgentResponse::ShuttingDown)?.as_bytes())?;
            writer.flush()?;
            let _ = Command::new("shutdown").args(["-h", "now"]).spawn();
            return Ok(());
        }
    };

    writer.write_all(encode_line(&response)?.as_bytes())?;
    writer.flush()?;
    Ok(())
}

/// Runs `command` under `/bin/sh -c`, killing it if it runs longer than
/// `timeout` (the draft this was ported from accepted a timeout field but
/// never enforced it — this does). stdout/stderr are drained on their own
/// threads concurrently with the wait loop below: reading them only after
/// the child exits would deadlock on any command whose output exceeds the
/// pipe buffer before it's done (child blocks writing to a full pipe that
/// nothing is draining, so it never exits, so `try_wait` never returns).
#[cfg(target_os = "linux")]
fn exec_with_timeout(command: &str, timeout: Duration) -> AgentResponse {
    let mut child = match Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return AgentResponse::Error { message: format!("spawning command: {e}") },
    };

    let stdout_pipe = child.stdout.take().expect("stdout was piped");
    let stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let mut pipe = stdout_pipe;
        let _ = pipe.read_to_string(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let mut pipe = stderr_pipe;
        let _ = pipe.read_to_string(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return AgentResponse::Error { message: format!("waiting on command: {e}") },
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    match status {
        Some(status) => AgentResponse::Exec { exit_code: status.code().unwrap_or(-1), stdout, stderr },
        None => AgentResponse::Error {
            message: format!("command exceeded {}s timeout and was killed", timeout.as_secs()),
        },
    }
}

#[cfg(target_os = "linux")]
fn put_file(path: &str, content_base64: &str, mode: Option<u32>) -> AgentResponse {
    let bytes = match B64.decode(content_base64) {
        Ok(b) => b,
        Err(e) => return AgentResponse::Error { message: format!("content is not valid base64: {e}") },
    };
    if bytes.len() > MAX_FILE_TRANSFER_BYTES {
        return AgentResponse::Error {
            message: format!("content is {} bytes, exceeds the {}-byte transfer limit", bytes.len(), MAX_FILE_TRANSFER_BYTES),
        };
    }
    if let Some(parent) = std::path::Path::new(path).parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return AgentResponse::Error { message: format!("creating {}: {e}", parent.display()) };
        }
    }
    if let Err(e) = std::fs::write(path, &bytes) {
        return AgentResponse::Error { message: format!("writing {path}: {e}") };
    }
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(mode.unwrap_or(0o644));
    if let Err(e) = std::fs::set_permissions(path, perms) {
        return AgentResponse::Error { message: format!("setting permissions on {path}: {e}") };
    }
    AgentResponse::FileWritten
}

#[cfg(target_os = "linux")]
fn get_file(path: &str) -> AgentResponse {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return AgentResponse::Error { message: format!("reading {path}: {e}") },
    };
    if metadata.len() as usize > MAX_FILE_TRANSFER_BYTES {
        return AgentResponse::Error {
            message: format!("{path} is {} bytes, exceeds the {}-byte transfer limit", metadata.len(), MAX_FILE_TRANSFER_BYTES),
        };
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return AgentResponse::Error { message: format!("reading {path}: {e}") },
    };
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o777;
    AgentResponse::FileContent { content_base64: B64.encode(&bytes), mode }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn put_file_creates_parent_dirs_and_sets_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.yaml");
        let resp = put_file(path.to_str().unwrap(), &B64.encode(b"hello"), Some(0o600));
        assert!(matches!(resp, AgentResponse::FileWritten));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn put_file_rejects_invalid_base64() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x");
        let resp = put_file(path.to_str().unwrap(), "not-valid-base64!!!", None);
        assert!(matches!(resp, AgentResponse::Error { .. }));
        assert!(!path.exists());
    }

    #[test]
    fn get_file_round_trips_put_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        put_file(path.to_str().unwrap(), &B64.encode(b"hello world"), Some(0o640));

        match get_file(path.to_str().unwrap()) {
            AgentResponse::FileContent { content_base64, mode } => {
                assert_eq!(B64.decode(&content_base64).unwrap(), b"hello world");
                assert_eq!(mode, 0o640);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn get_file_errors_on_missing_path() {
        let resp = get_file("/nonexistent/path/for/sure");
        assert!(matches!(resp, AgentResponse::Error { .. }));
    }
}
