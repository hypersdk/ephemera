// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Context, Result};
use std::{fs::OpenOptions, path::Path, process::Stdio};
use tokio::process::{Child, Command};

pub async fn run_checked(program: &str, args: &[String]) -> Result<()> {
    let out = Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("starting {program}"))?;
    if !out.status.success() {
        bail!(
            "{} failed ({}): {}",
            program,
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Like `run_checked`, but bounded — used for calls to a VMM's own control
/// CLI (e.g. `ch-remote`), where a wedged VMM must not hang the caller
/// forever.
pub async fn run_checked_timeout(program: &str, args: &[String], timeout: std::time::Duration) -> Result<()> {
    tokio::time::timeout(timeout, run_checked(program, args))
        .await
        .with_context(|| format!("{program} timed out after {timeout:?}"))?
}

pub async fn output_checked(program: &str, args: &[String]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("starting {program}"))?;
    if !out.status.success() {
        bail!("{} failed: {}", program, String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// When `netns` is `Some`, rewrites `(program, args)` into `("ip", ["netns",
/// "exec", netns, program, ...args])` so the eventual `spawn_logged` call
/// launches the VMM (or, for a jailed Firecracker VM, `jailer` — `ip netns
/// exec` + `setns()` + `exec()` compose fine, the whole process tree stays
/// in the namespace across jailer's own exec into Firecracker) inside that
/// network namespace instead of the host's default one. A no-op passthrough
/// when `netns` is `None`, so every backend can call this unconditionally.
pub fn netns_wrap(netns: Option<&str>, program: &str, args: &[String]) -> (String, Vec<String>) {
    match netns {
        Some(ns) => {
            let mut wrapped = vec!["netns".to_string(), "exec".to_string(), ns.to_string(), program.to_string()];
            wrapped.extend(args.iter().cloned());
            ("ip".to_string(), wrapped)
        }
        None => (program.to_string(), args.to_vec()),
    }
}

pub async fn spawn_logged(program: &str, args: &[String], log: &Path) -> Result<Child> {
    let stdout = OpenOptions::new().create(true).append(true).open(log)?;
    let stderr = stdout.try_clone()?;
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        // Detach into its own process group so the VMM survives independent of
        // whatever session/terminal invoked `ephemera` (e.g. an SSH command
        // session ending must not SIGHUP a VM that's meant to keep running).
        .process_group(0)
        .spawn()
        .with_context(|| format!("spawning {program}"))
}

/// Polls `process_alive` every 100ms, up to `attempts` times, returning
/// `true` as soon as the process is gone (or `false` if it's still alive
/// after the last attempt). Public so callers like a graceful-shutdown path
/// can wait a bounded amount before falling back to a forceful stop.
pub async fn wait_for_exit(pid: u32, attempts: u32) -> bool {
    for _ in 0..attempts {
        if !process_alive(pid).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    false
}

/// Sends SIGTERM and waits for the process to actually exit (escalating to
/// SIGKILL after a grace period) so callers can safely reclaim resources the
/// process held, e.g. a TAP device's file descriptor, once this returns.
pub async fn terminate_pid(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await
        .context("running kill")?;
    if !status.success() {
        bail!("failed to terminate pid {pid}");
    }
    if wait_for_exit(pid, 50).await {
        return Ok(());
    }
    let _ = Command::new("kill").arg("-KILL").arg(pid.to_string()).status().await;
    if wait_for_exit(pid, 20).await {
        return Ok(());
    }
    bail!("pid {pid} did not exit after SIGTERM/SIGKILL");
}

/// Closes a raw fd handed off to a VMM child (e.g. a macvtap device fd) once
/// it has been inherited across exec; the parent no longer needs its copy.
pub fn close_fd(fd: i32) {
    unsafe {
        libc::close(fd);
    }
}

pub async fn process_alive(pid: u32) -> bool {
    // A dead pid is an expected, common result (e.g. while polling for exit
    // in terminate_pid), so capture output rather than let `kill`'s "No such
    // process" message leak to our stderr on every negative check.
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netns_wrap_passes_through_unchanged_when_no_namespace() {
        let (program, args) = netns_wrap(None, "qemu-system-x86_64", &["-m".into(), "512".into()]);
        assert_eq!(program, "qemu-system-x86_64");
        assert_eq!(args, vec!["-m".to_string(), "512".to_string()]);
    }

    #[test]
    fn netns_wrap_prefixes_ip_netns_exec_when_namespaced() {
        let (program, args) = netns_wrap(Some("eph-abcd1234"), "qemu-system-x86_64", &["-m".into(), "512".into()]);
        assert_eq!(program, "ip");
        assert_eq!(
            args,
            vec!["netns", "exec", "eph-abcd1234", "qemu-system-x86_64", "-m", "512"]
        );
    }
}
