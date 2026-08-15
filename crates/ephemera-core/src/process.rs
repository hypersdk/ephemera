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

pub async fn spawn_logged(program: &str, args: &[String], log: &Path) -> Result<Child> {
    let stdout = OpenOptions::new().create(true).append(true).open(log)?;
    let stderr = stdout.try_clone()?;
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| format!("spawning {program}"))
}

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
    Ok(())
}

pub async fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}
