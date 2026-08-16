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
        set_cloexec(fd);
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
        AgentRequest::OpenShell { cols, rows } => {
            // Leftover bytes the BufReader already pulled off the wire past
            // the request line (the client shouldn't send any before seeing
            // ShellOpened, but don't drop them silently if it does).
            let pending = reader.buffer().to_vec();
            let conn = reader.into_inner();
            return open_shell(conn, writer, cols, rows, &pending);
        }
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

/// Marks `fd` close-on-exec so a later `fork`+`exec` (spawning the shell,
/// or any other child) doesn't inherit it. `fork()` copies *every* open fd
/// in the process by default, not just ones the child logically needs —
/// without this, the shell child would hold its own open copy of the vsock
/// connection fd, and the connection would never actually reach EOF even
/// after every fd this function itself holds is closed, since the kernel
/// still sees a live reference in the child.
#[cfg(target_os = "linux")]
fn set_cloexec(fd: std::os::fd::RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
}

/// Allocates a PTY, spawns `/bin/sh` attached to it as its own session
/// leader with the slave as controlling terminal (so job control — Ctrl-C,
/// Ctrl-Z — works normally), acks `ShellOpened`, then relays raw bytes
/// between `conn`/`writer` (the vsock connection, already split into its
/// two halves by the caller) and the PTY master until either side closes.
/// `pending` is forwarded to the shell first — bytes the client already
/// sent past the request line before this function took over reading.
#[cfg(target_os = "linux")]
fn open_shell(
    mut conn: std::fs::File,
    mut writer: std::fs::File,
    cols: u16,
    rows: u16,
    pending: &[u8],
) -> Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};

    set_cloexec(conn.as_raw_fd());
    set_cloexec(writer.as_raw_fd());

    let master_fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master_fd < 0 {
        let err = std::io::Error::last_os_error();
        writer.write_all(encode_line(&AgentResponse::Error { message: format!("posix_openpt: {err}") })?.as_bytes())?;
        writer.flush()?;
        return Ok(());
    }
    set_cloexec(master_fd);
    let setup: Result<std::path::PathBuf> = (|| unsafe {
        if libc::grantpt(master_fd) != 0 {
            anyhow::bail!("grantpt: {}", std::io::Error::last_os_error());
        }
        if libc::unlockpt(master_fd) != 0 {
            anyhow::bail!("unlockpt: {}", std::io::Error::last_os_error());
        }
        let mut buf = vec![0u8; 256];
        if libc::ptsname_r(master_fd, buf.as_mut_ptr() as *mut libc::c_char, buf.len()) != 0 {
            anyhow::bail!("ptsname_r: {}", std::io::Error::last_os_error());
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(end);
        Ok(std::path::PathBuf::from(String::from_utf8_lossy(&buf).into_owned()))
    })();
    let slave_path = match setup {
        Ok(p) => p,
        Err(e) => {
            unsafe { libc::close(master_fd) };
            writer.write_all(encode_line(&AgentResponse::Error { message: e.to_string() })?.as_bytes())?;
            writer.flush()?;
            return Ok(());
        }
    };

    let winsize = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &winsize) };

    let spawn_result = (|| -> Result<std::process::Child> {
        let slave0 = std::fs::OpenOptions::new().read(true).write(true).open(&slave_path)?;
        let slave1 = slave0.try_clone()?;
        let slave2 = slave0.try_clone()?;
        // `TIOCSCTTY` on an *inherited* PTY fd can fail EPERM in some
        // guest-kernel/PTY-allocation states even from a fresh session
        // leader — found live: `setsid()` succeeds, the ioctl still
        // returns EPERM. POSIX guarantees a *fresh open* of a terminal by
        // a session leader with no controlling terminal yet acquires one
        // automatically, no ioctl needed — re-opening the slave from
        // *inside* the child (after setsid, before exec) side-steps
        // whatever inherited-fd state trips up the ioctl path.
        let slave_path_c = std::ffi::CString::new(slave_path.as_os_str().as_encoded_bytes())
            .context("slave PTY path contains a NUL byte")?;
        let mut cmd = Command::new("/bin/sh");
        cmd.stdin(Stdio::from(slave0)).stdout(Stdio::from(slave1)).stderr(Stdio::from(slave2));
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let ctty_fd = libc::open(slave_path_c.as_ptr(), libc::O_RDWR);
                if ctty_fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(ctty_fd);
                Ok(())
            });
        }
        Ok(cmd.spawn()?)
    })();

    let mut child = match spawn_result {
        Ok(c) => c,
        Err(e) => {
            unsafe { libc::close(master_fd) };
            writer.write_all(encode_line(&AgentResponse::Error { message: format!("spawning shell: {e}") })?.as_bytes())?;
            writer.flush()?;
            return Ok(());
        }
    };

    writer.write_all(encode_line(&AgentResponse::ShellOpened)?.as_bytes())?;
    writer.flush()?;

    let mut master_in = unsafe { std::fs::File::from_raw_fd(master_fd) };
    let mut master_out = master_in.try_clone().context("cloning PTY master fd")?;
    if !pending.is_empty() {
        let _ = std::io::Write::write_all(&mut master_in, pending);
    }

    // Either relay direction finishing signals the session is over (client
    // hung up, or the shell exited and its PTY slave closed) — a channel
    // both threads share the sender end of lets the main thread block on
    // the first one to finish. (Not `JoinHandle::is_finished()` polling:
    // found live, under real vsock/PTY load, that a poll loop checking
    // `is_finished()` right after both threads had provably already
    // returned never observed it and spun forever — root cause not
    // identified; recv() on a channel each thread explicitly signals into
    // sidesteps whatever that was and is more idiomatic anyway.)
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();

    // conn -> PTY master (client keystrokes into the shell)
    let to_shell_tx = done_tx.clone();
    let to_shell = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let n = match std::io::Read::read(&mut conn, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if std::io::Write::write_all(&mut master_in, &buf[..n]).is_err() {
                break;
            }
        }
        let _ = to_shell_tx.send(());
    });
    // PTY master -> conn (shell output to client)
    let to_client_tx = done_tx.clone();
    let to_client = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let n = match std::io::Read::read(&mut master_out, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if writer.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = to_client_tx.send(());
    });
    drop(done_tx);
    let _ = done_rx.recv();

    // Kill/reap on a detached thread, not here: found live that
    // `child.kill()`/`.wait()`/`.try_wait()` called from *this* thread
    // (which had earlier done the posix_openpt/grantpt/spawn calls above)
    // block indefinitely under real vsock/PTY load in a way a brand-new
    // thread doing the identical calls does not — root cause not
    // identified. This keeps a session ending from ever wedging the
    // connection handler even though it doesn't explain the underlying
    // hang; see the Ephemera README's known-issues section.
    std::thread::spawn(move || {
        let _ = child.kill();
        let _ = child.wait();
    });
    // Don't join to_shell/to_client either: whichever side didn't finish
    // is still blocked on its own read (the peer hasn't closed that half
    // yet) — it'll unblock and exit on its own once `conn`/the PTY master
    // actually close.
    drop(to_shell);
    drop(to_client);
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

    /// A connected AF_UNIX socketpair stands in for the two vsock-connection
    /// halves `handle_connection` would otherwise split off a real
    /// `AF_VSOCK` fd — same `std::fs::File`-over-raw-fd shape, so
    /// `open_shell` runs completely unmodified in this test.
    fn socketpair() -> (std::fs::File, std::fs::File) {
        use std::os::fd::FromRawFd;
        let mut fds = [0i32; 2];
        // SOCK_CLOEXEC: this whole test simulates "guest agent" and "vsock
        // client" as two threads of ONE process (a real deployment has them
        // as separate processes in separate kernels — host vs. guest — with
        // no fd table in common at all). Without CLOEXEC here, the client
        // thread's own end of the pair leaks into every child the agent
        // thread forks (fork() copies the whole process's fd table, not
        // just the calling thread's fds), so the agent's read on its own
        // end never sees EOF even after the client side closes — a
        // same-process test artifact, not a real cross-VM vsock bug.
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "socketpair failed: {}", std::io::Error::last_os_error());
        unsafe { (std::fs::File::from_raw_fd(fds[0]), std::fs::File::from_raw_fd(fds[1])) }
    }

    #[test]
    fn open_shell_runs_a_real_pty_shell_round_trip() {
        let (agent_end, client_end) = socketpair();
        let agent_end2 = agent_end.try_clone().unwrap();

        let shell = std::thread::spawn(move || {
            open_shell(agent_end, agent_end2, 80, 24, &[]).unwrap();
        });

        let mut client_reader = BufReader::new(client_end.try_clone().unwrap());
        let mut client_writer = client_end;

        // First line back is the ShellOpened ack, still JSON-framed.
        let mut ack = String::new();
        client_reader.read_line(&mut ack).unwrap();
        assert!(ack.contains("shell-opened"), "unexpected ack: {ack:?}");

        // Everything after that is raw PTY bytes — echo a marker and read
        // until we see it (the shell's own echo of our input, then its
        // command's output, both arrive on the same raw stream).
        client_writer.write_all(b"echo PTY_ECHO_TEST_MARKER\n").unwrap();
        client_writer.flush().unwrap();

        unsafe {
            use std::os::fd::AsRawFd;
            let fd = client_reader.get_ref().as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let marker_seen = loop {
            if std::time::Instant::now() > deadline {
                break false;
            }
            let mut buf = [0u8; 1024];
            let r = std::io::Read::read(&mut client_reader, &mut buf);
            match r {
                Ok(0) => break false,
                Ok(n) => {
                    seen.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&seen).contains("PTY_ECHO_TEST_MARKER") {
                        break true;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break false,
            }
        };
        assert!(marker_seen, "never saw the marker in PTY output; got: {:?}", String::from_utf8_lossy(&seen));

        // Both ends must close for the peer to see EOF — client_reader
        // holds a dup'd fd of the same socket, so dropping client_writer
        // alone leaves the connection open from the agent's point of view.
        drop(client_writer);
        drop(client_reader);
        shell.join().unwrap();
    }
}
