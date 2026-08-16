// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

pub mod catalog;
pub mod cloudinit;
pub mod storage;

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
    /// Files to place directly into the image (e.g. a compiled
    /// `ephemera-guest-agent` binary and its systemd unit). Applied via
    /// `guestkit` — a host-side file's permission bits are preserved on
    /// copy, so a binary already marked executable stays executable; no
    /// separate chmod step is needed.
    #[serde(default)]
    pub copy_in: Vec<CopyIn>,
    /// systemd unit names to `systemctl enable` via guestkit's chroot
    /// command exec, in the same session as every other customization step.
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

    let needs_customize = !req.copy_in.is_empty() || !req.enable_services.is_empty()
        || req.hostname.is_some() || !req.packages.is_empty() || !req.commands.is_empty() || req.ssh_key.is_some();
    if needs_customize {
        customize_image(req.output.clone(), req.clone()).await?;
    }
    Ok(BuildImageResult { output: req.output.clone(), format: req.format.clone() })
}

/// Applies every customization field on `req` (`copy_in`, `enable_services`,
/// `hostname`, `packages`, `commands`, `ssh_key`) in one `guestkit` session —
/// qemu-nbd mount + chroot, no libguestfs appliance. `Guestfs`'s methods are
/// synchronous/blocking, so this runs on a blocking-pool thread rather than
/// stalling the async runtime for however long the mount+customize takes.
///
/// **Known limitation**: `guestkit::Guestfs::command` chroots without
/// bind-mounting `/proc`, `/sys`, or `/dev` from the host first (unlike a
/// real booted appliance). Simple packages install fine; a package whose
/// postinst script depends on `/proc` (common for kernel/systemd-adjacent
/// packages) can fail here where a full-appliance tool wouldn't. No
/// workaround today beyond passing an equivalent `commands` entry that
/// bind-mounts what a specific package needs before installing it.
async fn customize_image(image: PathBuf, req: BuildImageRequest) -> Result<()> {
    tokio::task::spawn_blocking(move || customize_image_blocking(&image, &req))
        .await
        .context("guestkit worker thread panicked")?
}

fn customize_image_blocking(image: &Path, req: &BuildImageRequest) -> Result<()> {
    use guestkit::Guestfs;

    let mut g = Guestfs::new().context("creating guestfs handle")?;
    g.add_drive(image).with_context(|| format!("adding drive {}", image.display()))?;
    g.launch().context("launching guestfs")?;

    let roots = g.inspect_os().context("inspecting guest OS")?;
    let root = roots.first().context("no operating system found in image")?.clone();
    let mounts = g.inspect_get_mountpoints(&root).context("getting mountpoints")?;
    for (mountpoint, device) in &mounts {
        g.mount(device, mountpoint)
            .with_context(|| format!("mounting {device} at {mountpoint}"))?;
    }

    for file in &req.copy_in {
        let src = file.src.to_str().context("copy_in src path is not valid UTF-8")?;
        g.upload(src, &file.dest)
            .with_context(|| format!("copying {} to {} in image", file.src.display(), file.dest))?;
    }

    if let Some(hostname) = &req.hostname {
        g.write("/etc/hostname", format!("{hostname}\n").as_bytes())
            .context("writing /etc/hostname")?;
    }

    if !req.packages.is_empty() {
        install_packages_with_dns(&mut g, &req.packages)?;
    }

    for cmd in &req.commands {
        g.sh_raw(cmd).with_context(|| format!("running command: {cmd}"))?;
    }

    for service in &req.enable_services {
        g.command(&["systemctl", "enable", service])
            .with_context(|| format!("enabling {service}"))?;
    }

    if let Some(key) = &req.ssh_key {
        inject_ssh_key(&mut g, key)?;
    }

    let _ = g.umount_all();
    g.shutdown().context("shutting down guestfs")?;
    Ok(())
}

/// Installs `packages`, temporarily staging the host's `/etc/resolv.conf`
/// into the guest first. `guestkit`'s chroot exec (see [`install_packages`])
/// runs in the host's network namespace, but every package manager still
/// resolves hostnames using the *guest's own* `/etc/resolv.conf` — and on a
/// stock cloud image that's a symlink to `/run/systemd/resolve/...`, which
/// doesn't exist outside a running systemd instance. Without a real
/// `/etc/resolv.conf` in place, every fetch fails with "Temporary failure
/// resolving" (confirmed against a real Ubuntu 24.04 image) even though
/// networking itself works fine. The staged file is removed afterward
/// (restored, if the guest had a real, non-symlink one of its own) — cloud
/// images regenerate their own resolver config on first boot regardless.
fn install_packages_with_dns(g: &mut guestkit::Guestfs, packages: &[String]) -> Result<()> {
    let original = g.read_file("/etc/resolv.conf").ok();
    let host_resolv = fs::read("/etc/resolv.conf").context("reading host /etc/resolv.conf")?;
    // A stock cloud image's /etc/resolv.conf is typically a *dangling*
    // symlink (e.g. to /run/systemd/resolve/stub-resolv.conf, which doesn't
    // exist outside a running systemd instance). `Guestfs::rm`/`write`
    // resolve the guest path to a host path but then use plain `fs`
    // calls, which follow symlinks — for a dangling one that means ENOENT,
    // and for a non-dangling absolute-target one it would mean writing
    // through the raw target path on the *host*, escaping the mount
    // entirely. Removing it via a chroot `rm -f` first sidesteps both: the
    // chroot resolves the path against the guest root, not the host's.
    let _ = g.command(&["rm", "-f", "/etc/resolv.conf"]);
    g.write("/etc/resolv.conf", &host_resolv).context("staging /etc/resolv.conf for package install")?;

    let result = install_packages(g, packages);

    let _ = g.command(&["rm", "-f", "/etc/resolv.conf"]);
    match &original {
        Some(bytes) => { let _ = g.write("/etc/resolv.conf", bytes); }
        None => {}
    }
    result
}

/// Installs `packages` via the guest's own package manager. The manager is
/// detected by actually exec'ing `command -v <tool>` inside the chroot
/// (`apt-get`/`tdnf`/`dnf`/`yum`/`pacman`, checked in that order) rather
/// than via `guestkit`'s `inspect_get_package_management`, whose
/// presence-check constructs `<root>/usr/bin/<tool>` from the abstract
/// device-rooted `root` identifier `inspect_os` returns — that string isn't
/// a real filesystem path, so the check always misses even when the binary
/// is genuinely installed (confirmed against a real Ubuntu 24.04 image,
/// which has `/usr/bin/apt` but was reported as `dpkg`). Exec'ing inside
/// the chroot sidesteps that bug entirely.
fn install_packages(g: &mut guestkit::Guestfs, packages: &[String]) -> Result<()> {
    let pkgs: Vec<&str> = packages.iter().map(String::as_str).collect();
    let tool = ["apt-get", "tdnf", "dnf", "yum", "pacman"].into_iter().find(|tool| {
        g.command(&["sh", "-c", &format!("command -v {tool}")])
            .map(|out| !out.trim().is_empty())
            .unwrap_or(false)
    });
    match tool {
        Some("apt-get") => {
            g.command(&["apt-get", "update"]).context("apt-get update")?;
            let mut args = vec!["apt-get", "install", "-y"];
            args.extend(pkgs);
            g.command(&args).context("apt-get install")?;
        }
        Some(t @ ("tdnf" | "dnf" | "yum")) => {
            let mut args = vec![t, "install", "-y"];
            args.extend(pkgs);
            g.command(&args).context("package install")?;
        }
        Some("pacman") => {
            // A fresh Arch image ships with an empty pacman keyring, so
            // every install fails signature verification until it's
            // initialized — real, standard Arch chroot bootstrapping, not
            // specific to this bare chroot (confirmed against a real Arch
            // Linux cloud image).
            g.command(&["pacman-key", "--init"]).context("pacman-key --init")?;
            g.command(&["pacman-key", "--populate", "archlinux"]).context("pacman-key --populate")?;
            let mut args = vec!["pacman", "-Sy", "--noconfirm"];
            args.extend(pkgs);
            let result = with_staged_mtab(g, |g| g.command(&args).context("pacman install").map(|_| ()));
            // pacman-key/pacman spawn a gpg-agent that double-forks and
            // detaches, inheriting the chroot's root directory — guestkit's
            // chroot exec only waits on the direct child, so the detached
            // agent leaks with open files under the mount, which blocks
            // the unmount at the end of customize_image_blocking. There's
            // no PID namespace isolation (chroot doesn't create one), so
            // it's a real, host-visible process — reap it with a host-side
            // `pkill`, not `g.command`: a `pkill` run *inside* the chroot
            // needs `/proc` to enumerate processes, which this bare chroot
            // doesn't have (the same limitation noted on
            // `customize_image_blocking`), so it would silently match
            // nothing. Pattern is scoped to pacman-key's specific homedir
            // to avoid touching an unrelated gpg-agent on the host.
            let _ = std::process::Command::new("pkill")
                .args(["-9", "-f", "gpg-agent --homedir /etc/pacman.d/gnupg"])
                .status();
            result?;
        }
        _ => bail!(
            "cannot install packages: no supported package manager found in the guest \
             (checked apt-get/tdnf/dnf/yum/pacman) — install them via an equivalent \
             `commands` entry instead"
        ),
    }
    Ok(())
}

/// Runs `f` with a synthetic `/etc/mtab` in place, restoring/removing it
/// afterward. `pacman` refuses to run at all without a readable
/// `/etc/mtab` (it parses it to work out which filesystem the install
/// target lives on) — on a real Arch system that's a symlink to
/// `/proc/self/mounts`, which this bare chroot doesn't have (no `/proc`
/// bind-mount, the same limitation noted on [`customize_image_blocking`]).
/// A single plausible-looking `rw` entry is enough to satisfy the parse;
/// pacman doesn't need it to reflect the real mount table. Confirmed
/// necessary against a real Arch Linux cloud image (`apt-get`/`dnf` never
/// needed this, so it's scoped to the pacman path only).
fn with_staged_mtab(
    g: &mut guestkit::Guestfs,
    f: impl FnOnce(&mut guestkit::Guestfs) -> Result<()>,
) -> Result<()> {
    let original = g.read_file("/etc/mtab").ok();
    let _ = g.command(&["rm", "-f", "/etc/mtab"]);
    g.write("/etc/mtab", b"rootfs / rootfs rw 0 0\n").context("staging /etc/mtab for pacman")?;

    let result = f(g);

    let _ = g.command(&["rm", "-f", "/etc/mtab"]);
    if let Some(bytes) = &original {
        let _ = g.write("/etc/mtab", bytes);
    }
    result
}

/// Authorizes `key` for root login by appending it to
/// `/root/.ssh/authorized_keys`, creating the file/directory if needed.
/// Matches the prior `virt-customize --ssh-inject root:file:...` behavior.
fn inject_ssh_key(g: &mut guestkit::Guestfs, key: &str) -> Result<()> {
    g.command(&["mkdir", "-p", "/root/.ssh"]).context("creating /root/.ssh")?;
    g.command(&["chmod", "700", "/root/.ssh"]).context("chmod /root/.ssh")?;
    let mut content = g.command(&["cat", "/root/.ssh/authorized_keys"]).unwrap_or_default();
    if !content.is_empty() && !content.ends_with('\n') { content.push('\n'); }
    content.push_str(key.trim());
    content.push('\n');
    g.write("/root/.ssh/authorized_keys", content.as_bytes()).context("writing authorized_keys")?;
    g.command(&["chmod", "600", "/root/.ssh/authorized_keys"]).context("chmod authorized_keys")?;
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
    if std::env::var("GUESTKIT_DEBUG").is_ok() {
        g.set_debug(true);
        g.set_trace(true);
    }
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
