// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

pub mod catalog;
pub mod cloudinit;

use anyhow::{bail, Context, Result};
use ephemera_core::{config::Config, model::BackendKind, process::run_checked};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, path::{Path, PathBuf}};
use tokio::{fs as async_fs, io::AsyncWriteExt};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildImageRequest {
    pub source: String,
    pub output: PathBuf,
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default)]
    pub size_gib: Option<u64>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub ssh_key: Option<String>,
    /// Files to place directly into the image before any virt-customize
    /// step runs (e.g. a compiled `ephemera-guest-agent` binary and its
    /// systemd unit). Applied via `guestkit` — a host-side file's
    /// permission bits are preserved on copy, so a binary already marked
    /// executable stays executable; no separate chmod step is needed.
    #[serde(default)]
    pub copy_in: Vec<CopyIn>,
    /// systemd unit names to `systemctl enable` via guestkit's chroot
    /// command exec, in the same session as `copy_in` — independent of
    /// virt-customize, whose libguestfs appliance needs outbound networking
    /// (via `passt`) that isn't available/working on every host.
    #[serde(default)]
    pub enable_services: Vec<String>,
}
fn default_format() -> String { "qcow2".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyIn {
    pub src: PathBuf,
    pub dest: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildImageResult {
    pub output: PathBuf,
    pub format: String,
}

pub(crate) async fn fetch_if_needed(cfg: &Config, source: &str) -> Result<PathBuf> {
    if !source.starts_with("http://") && !source.starts_with("https://") {
        return Ok(PathBuf::from(source));
    }
    let downloads = cfg.state_dir.join("downloads");
    fs::create_dir_all(&downloads)?;
    let name = source.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("base.img");
    let dest = downloads.join(name);
    if dest.exists() { return Ok(dest); }
    let mut resp = Client::new().get(source).send().await?.error_for_status()?;
    let mut f = async_fs::File::create(&dest).await?;
    while let Some(chunk) = resp.chunk().await? { f.write_all(&chunk).await?; }
    Ok(dest)
}

pub(crate) fn verify_sha256(path: &Path, wanted: &str) -> Result<()> {
    let bytes = fs::read(path)?;
    let got = format!("{:x}", Sha256::digest(&bytes));
    if !got.eq_ignore_ascii_case(wanted) { bail!("sha256 mismatch: expected {wanted}, got {got}"); }
    Ok(())
}

pub async fn build_image(cfg: &Config, req: &BuildImageRequest) -> Result<BuildImageResult> {
    let src = fetch_if_needed(cfg, &req.source).await?;
    if let Some(hash) = &req.sha256 { verify_sha256(&src, hash)?; }
    if let Some(parent) = req.output.parent() { fs::create_dir_all(parent)?; }

    run_checked(&cfg.qemu_img_binary, &[
        "convert".into(), "-O".into(), req.format.clone(),
        src.display().to_string(), req.output.display().to_string(),
    ]).await?;
    if let Some(size) = req.size_gib {
        run_checked(&cfg.qemu_img_binary, &[
            "resize".into(), req.output.display().to_string(), format!("{}G", size)
        ]).await?;
    }

    if !req.copy_in.is_empty() || !req.enable_services.is_empty() {
        inject_files(&req.output, req.copy_in.clone(), req.enable_services.clone()).await?;
    }

    if req.hostname.is_some() || !req.packages.is_empty() || !req.commands.is_empty() || req.ssh_key.is_some() {
        let mut a = vec!["-a".into(), req.output.display().to_string()];
        if let Some(h) = &req.hostname { a.extend(["--hostname".into(), h.clone()]); }
        if !req.packages.is_empty() { a.extend(["--install".into(), req.packages.join(",")]); }
        for c in &req.commands { a.extend(["--run-command".into(), c.clone()]); }
        if let Some(key) = &req.ssh_key {
            let key_file = req.output.with_extension("builder.pub");
            fs::write(&key_file, key).context("writing temporary ssh key")?;
            a.extend(["--ssh-inject".into(), format!("root:file:{}", key_file.display())]);
            let result = run_checked(&cfg.virt_customize_binary, &a).await;
            let _ = fs::remove_file(key_file);
            result?;
        } else {
            run_checked(&cfg.virt_customize_binary, &a).await?;
        }
    }
    Ok(BuildImageResult { output: req.output.clone(), format: req.format.clone() })
}

/// Copies `files` into `image` and `systemctl enable`s `services`, via
/// `guestkit` (qemu-nbd mount + chroot — no libguestfs appliance, so this
/// works even where `virt-customize`'s network-capable appliance doesn't;
/// see the doc comment on `enable_services`). `Guestfs`'s methods are
/// synchronous/blocking, so this runs on a blocking-pool thread rather than
/// stalling the async runtime for however long the mount+copy takes.
async fn inject_files(image: &Path, files: Vec<CopyIn>, services: Vec<String>) -> Result<()> {
    let image = image.to_path_buf();
    tokio::task::spawn_blocking(move || inject_files_blocking(&image, &files, &services))
        .await
        .context("guestkit worker thread panicked")?
}

fn inject_files_blocking(image: &Path, files: &[CopyIn], services: &[String]) -> Result<()> {
    use guestkit::Guestfs;

    let mut g = Guestfs::new().context("creating guestfs handle")?;
    g.add_drive(image).with_context(|| format!("adding drive {}", image.display()))?;
    g.launch().context("launching guestfs")?;

    let roots = g.inspect_os().context("inspecting guest OS")?;
    let root = roots.first().context("no operating system found in image")?;
    let mounts = g.inspect_get_mountpoints(root).context("getting mountpoints")?;
    for (mountpoint, device) in &mounts {
        g.mount(device, mountpoint)
            .with_context(|| format!("mounting {device} at {mountpoint}"))?;
    }

    for file in files {
        let src = file.src.to_str().context("copy_in src path is not valid UTF-8")?;
        g.upload(src, &file.dest)
            .with_context(|| format!("copying {} to {} in image", file.src.display(), file.dest))?;
    }
    for service in services {
        g.command(&["systemctl", "enable", service])
            .with_context(|| format!("enabling {service}"))?;
    }

    let _ = g.umount_all();
    g.shutdown().context("shutting down guestfs")?;
    Ok(())
}

/// Writes `token` to [`ephemera_guest_protocol::TOKEN_FILE_PATH`] inside
/// `disk` (an instance's own already-cloned disk — a qcow2 CoW overlay for
/// QEMU, or a full raw clone for Cloud Hypervisor/Firecracker; either way,
/// this never touches the shared base image). Runs before the VM's first
/// boot, so `ephemera-guest-agent`'s systemd unit sees the token file
/// already in place when it starts. Mode 0600 root-owned — same posture as
/// an SSH host key, since anything able to read it inside the guest could
/// impersonate an authenticated caller.
pub async fn inject_guest_agent_token(disk: &Path, token: &str) -> Result<()> {
    let disk = disk.to_path_buf();
    let token = token.to_string();
    tokio::task::spawn_blocking(move || inject_guest_agent_token_blocking(&disk, &token))
        .await
        .context("guestkit worker thread panicked")?
}

fn inject_guest_agent_token_blocking(disk: &Path, token: &str) -> Result<()> {
    use ephemera_guest_protocol::TOKEN_FILE_PATH;
    use guestkit::Guestfs;

    let mut g = Guestfs::new().context("creating guestfs handle")?;
    g.add_drive(disk).with_context(|| format!("adding drive {}", disk.display()))?;
    g.launch().context("launching guestfs")?;

    let roots = g.inspect_os().context("inspecting guest OS")?;
    let root = roots.first().context("no operating system found in image")?;
    let mounts = g.inspect_get_mountpoints(root).context("getting mountpoints")?;
    for (mountpoint, device) in &mounts {
        g.mount(device, mountpoint)
            .with_context(|| format!("mounting {device} at {mountpoint}"))?;
    }

    g.write(TOKEN_FILE_PATH, token.as_bytes())
        .with_context(|| format!("writing {TOKEN_FILE_PATH}"))?;
    g.chmod(0o600, TOKEN_FILE_PATH).context("chmod guest-agent token file")?;

    let _ = g.umount_all();
    g.shutdown().context("shutting down guestfs")?;
    Ok(())
}

async fn image_format(cfg: &Config, image: &Path) -> String {
    ephemera_core::process::output_checked(&cfg.qemu_img_binary, &[
        "info".into(), "--output=json".into(), image.display().to_string()
    ]).await.ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("format").and_then(|f| f.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "qcow2".into())
}

pub async fn clone_for_vm(cfg: &Config, base: &Path, backend: BackendKind, out: &Path, size_gib: Option<u64>) -> Result<()> {
    let base_fmt = image_format(cfg, base).await;
    match backend {
        BackendKind::Qemu => {
            // Cheap disposable copy-on-write layer.
            run_checked(&cfg.qemu_img_binary, &[
                "create".into(), "-f".into(), "qcow2".into(),
                "-F".into(), base_fmt,
                "-b".into(), base.canonicalize()?.display().to_string(),
                out.display().to_string(),
            ]).await?;
        }
        BackendKind::CloudHypervisor | BackendKind::Firecracker => {
            // Firecracker expects a raw block image. Cloud Hypervisor is also kept raw here
            // for a predictable common fast path. Reflink makes raw clones nearly instant on
            // XFS/Btrfs; cp transparently falls back when reflinks are unavailable.
            if base_fmt == "raw" {
                run_checked("cp", &[
                    "--reflink=auto".into(), "--sparse=always".into(),
                    base.display().to_string(), out.display().to_string(),
                ]).await?;
            } else {
                run_checked(&cfg.qemu_img_binary, &[
                    "convert".into(), "-O".into(), "raw".into(),
                    base.display().to_string(), out.display().to_string(),
                ]).await?;
            }
        }
        BackendKind::Auto => bail!("VM has an unresolved BackendKind::Auto — this is a bug, backend selection must happen before cloning its disk"),
    }
    if let Some(size) = size_gib {
        run_checked(&cfg.qemu_img_binary, &[
            "resize".into(), out.display().to_string(), format!("{}G", size)
        ]).await?;
    }
    Ok(())
}
