// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

mod http;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use ephemera_core::{
    backend::{LaunchContext, LaunchResult, VmBackend},
    config::Config,
    model::{BackendKind, CreateVmRequest, NetworkSpec, VmRecord},
    process::{run_checked, spawn_logged},
};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

const API_TIMEOUT: Duration = Duration::from_secs(10);

pub struct FirecrackerBackend;

/// Where each resource is placed in the config JSON: as an absolute host
/// path when launching Firecracker directly, or as a path relative to the
/// jail's own chroot root (which Firecracker sees as `/`) when launching
/// through `jailer` — see `launch_jailed`.
struct ResourcePaths {
    kernel: PathBuf,
    rootfs: PathBuf,
    seed: Option<PathBuf>,
    vsock_uds: Option<PathBuf>,
}

fn config_json(req: &CreateVmRequest, ctx: &LaunchContext, paths: &ResourcePaths) -> Result<serde_json::Value> {
    let boot_args = req.kernel_args.clone().unwrap_or_else(||
        "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw".into());

    let mut drives = vec![json!({
        "drive_id": "rootfs",
        "path_on_host": paths.rootfs.display().to_string(),
        "is_root_device": true,
        "is_read_only": false
    })];
    if let Some(seed) = &paths.seed {
        drives.push(json!({
            "drive_id": "seed",
            "path_on_host": seed.display().to_string(),
            "is_root_device": false,
            "is_read_only": true
        }));
    }

    let mut root = json!({
        "boot-source": {
            "kernel_image_path": paths.kernel.display().to_string(),
            "boot_args": boot_args
        },
        "drives": drives,
        "machine-config": {
            "vcpu_count": req.vcpus,
            "mem_size_mib": req.memory_mib,
            "smt": false,
            "track_dirty_pages": false
        }
    });

    match &ctx.network.spec {
        NetworkSpec::None => {}
        NetworkSpec::Tap { tap_name: Some(tap), mac, .. } => {
            let guest_mac = mac.clone().unwrap_or_else(|| "06:00:AC:10:00:02".into());
            root.as_object_mut().unwrap().insert("network-interfaces".into(), json!([{
                "iface_id": "eth0",
                "guest_mac": guest_mac,
                "host_dev_name": tap
            }]));
        }
        NetworkSpec::Tap { tap_name: None, .. } => bail!("tap network was not prepared"),
        NetworkSpec::Macvtap { .. } => bail!(
            "Firecracker backend does not support macvtap: its API only accepts a host_dev_name \
             it opens itself via /dev/net/tun, with no fd-passing option for a macvtap character \
             device. Use network.mode=tap with a bridge, or mode=none."
        ),
        NetworkSpec::User { .. } => bail!("Firecracker backend requires network.mode=none or tap"),
    }

    if req.agent.as_ref().is_some_and(|a| a.enabled) {
        let cid = ctx.guest_cid.context("agent enabled but no vsock CID was assigned")?;
        let socket = paths.vsock_uds.as_ref().context("agent enabled but no vsock socket path was assigned")?;
        root.as_object_mut().unwrap().insert(
            "vsock".into(),
            json!({"guest_cid": cid, "uds_path": socket.display().to_string()}),
        );
    }

    Ok(root)
}

#[async_trait]
impl VmBackend for FirecrackerBackend {
    fn kind(&self) -> BackendKind { BackendKind::Firecracker }

    async fn launch(&self, cfg: &Config, req: &CreateVmRequest, ctx: &LaunchContext) -> Result<LaunchResult> {
        if cfg.jailer.enabled {
            launch_jailed(cfg, req, ctx).await
        } else {
            launch_direct(cfg, req, ctx).await
        }
    }

    async fn pause(&self, _cfg: &Config, vm: &VmRecord) -> Result<()> {
        set_vm_state(vm, "Paused").await
    }

    async fn resume(&self, _cfg: &Config, vm: &VmRecord) -> Result<()> {
        set_vm_state(vm, "Resumed").await
    }

    async fn graceful_shutdown(&self, _cfg: &Config, vm: &VmRecord) -> Result<()> {
        // x86_64-only action; Firecracker has no ARM equivalent in its
        // public API today. This project targets x86_64 hosts only.
        http::request(
            control_socket(vm)?,
            "PUT",
            "/actions",
            Some(&json!({"action_type": "SendCtrlAltDel"})),
            API_TIMEOUT,
        )
        .await
    }
}

async fn launch_direct(cfg: &Config, req: &CreateVmRequest, ctx: &LaunchContext) -> Result<LaunchResult> {
    let kernel = req.kernel.as_ref().or(cfg.firecracker_kernel.as_ref())
        .context("Firecracker requires a Linux kernel via request.kernel or config.firecracker_kernel")?;
    let paths = ResourcePaths {
        kernel: kernel.clone(),
        rootfs: ctx.disk.clone(),
        seed: ctx.seed_disk.clone(),
        vsock_uds: ctx.vsock_socket.clone(),
    };
    let api = ctx.workspace.join("firecracker.sock");
    let cfg_path = ctx.workspace.join("firecracker.json");
    fs::write(&cfg_path, serde_json::to_vec_pretty(&config_json(req, ctx, &paths)?)?)?;
    let args = vec![
        "--api-sock".into(), api.display().to_string(),
        "--config-file".into(), cfg_path.display().to_string(),
    ];
    let (program, args) = ephemera_core::process::netns_wrap(ctx.network.netns.as_deref(), &cfg.firecracker_binary, &args);
    let child = spawn_logged(&program, &args, &ctx.log_path).await?;
    let pid = child.id().context("Firecracker exited before PID was available")?;
    Ok(LaunchResult { pid, control_socket: Some(api), jail_path: None, vsock_socket: ctx.vsock_socket.clone(), virtiofsd_pids: Vec::new() })
}

/// Runs Firecracker through its own `jailer` binary (chroot, uid/gid drop,
/// cgroups) instead of exec'ing it directly. Verified by hand against the
/// real `jailer` on the test host before writing this: `jailer` itself only
/// creates the chroot directory and places (a copy/hardlink of) the
/// exec-file inside it — the *caller* is responsible for placing every
/// other resource the jailed process needs (kernel, rootfs, its own JSON
/// config) inside that same chroot beforehand, and referencing them by
/// their in-jail path (i.e. as Firecracker itself will see them, relative
/// to its own `/`) in that config. `jailer` execs straight into Firecracker
/// after chrooting (no intermediate fork), so the pid `spawn_logged` sees
/// for the `jailer` invocation IS Firecracker's real pid — every existing
/// pid-based lifecycle path (`process_alive`, `terminate_pid`, ...) needs
/// no changes to work with a jailed VM.
async fn launch_jailed(cfg: &Config, req: &CreateVmRequest, ctx: &LaunchContext) -> Result<LaunchResult> {
    let kernel = req.kernel.as_ref().or(cfg.firecracker_kernel.as_ref())
        .context("Firecracker requires a Linux kernel via request.kernel or config.firecracker_kernel")?;
    if !cfg.firecracker_binary.starts_with('/') {
        bail!(
            "jailer.enabled requires config.firecracker_binary to be an absolute path (jailer's \
             --exec-file needs a real path, not a bare command resolved via $PATH); got {:?}",
            cfg.firecracker_binary
        );
    }
    let exec_file_name = Path::new(&cfg.firecracker_binary)
        .file_name()
        .context("firecracker_binary has no file name component")?;

    // jailer's own convention: <chroot_base_dir>/<exec-file basename>/<id>/root/
    let chroot_root = cfg.jailer.chroot_base_dir
        .join(exec_file_name)
        .join(ctx.id.to_string())
        .join("root");
    fs::create_dir_all(&chroot_root).with_context(|| format!("creating jail chroot dir {}", chroot_root.display()))?;

    link_or_copy(kernel, &chroot_root.join("vmlinux")).context("placing kernel in jail")?;
    link_or_copy(&ctx.disk, &chroot_root.join("rootfs")).context("placing rootfs in jail")?;
    let seed_in_jail = match &ctx.seed_disk {
        Some(seed) => {
            link_or_copy(seed, &chroot_root.join("seed")).context("placing cloud-init seed in jail")?;
            Some(PathBuf::from("/seed"))
        }
        None => None,
    };
    // Not pre-placed: Firecracker (running inside the jail) creates this
    // socket itself at startup, same as it would outside a jail — the
    // in-jail path here just tells it *where*, under its own chrooted `/`.
    let vsock_in_jail = ctx.vsock_socket.as_ref().map(|_| PathBuf::from("/vsock.sock"));

    let paths = ResourcePaths {
        kernel: PathBuf::from("/vmlinux"),
        rootfs: PathBuf::from("/rootfs"),
        seed: seed_in_jail,
        vsock_uds: vsock_in_jail.clone(),
    };
    let cfg_json_path = chroot_root.join("config.json");
    fs::write(&cfg_json_path, serde_json::to_vec_pretty(&config_json(req, ctx, &paths)?)?)?;

    // Everything placed above is root-owned by default; Firecracker runs as
    // jailer.uid/gid after the chroot+setuid, so it needs access to all of it.
    run_checked("chown", &[
        "-R".into(), format!("{}:{}", cfg.jailer.uid, cfg.jailer.gid), chroot_root.display().to_string(),
    ]).await.context("chowning jail contents to the jailer uid/gid")?;

    let jailer_args = vec![
        "--id".into(), ctx.id.to_string(),
        "--exec-file".into(), cfg.firecracker_binary.clone(),
        "--uid".into(), cfg.jailer.uid.to_string(),
        "--gid".into(), cfg.jailer.gid.to_string(),
        "--chroot-base-dir".into(), cfg.jailer.chroot_base_dir.display().to_string(),
        "--".into(),
        "--api-sock".into(), "/run/firecracker.socket".into(),
        "--config-file".into(), "/config.json".into(),
    ];
    let (program, jailer_args) = ephemera_core::process::netns_wrap(ctx.network.netns.as_deref(), &cfg.jailer.jailer_binary, &jailer_args);
    let child = spawn_logged(&program, &jailer_args, &ctx.log_path).await?;
    let pid = child.id().context("jailer exited before PID was available")?;

    let vsock_socket_host = vsock_in_jail.as_ref().map(|_| chroot_root.join("vsock.sock"));
    Ok(LaunchResult {
        pid,
        control_socket: Some(chroot_root.join("run/firecracker.socket")),
        vsock_socket: vsock_socket_host,
        jail_path: Some(chroot_root),
        virtiofsd_pids: Vec::new(),
    })
}

/// Hardlinks `src` into `dst` (instant, no extra disk space — the common
/// case, since `jailer.chroot_base_dir` is expected on the same filesystem
/// as `state_dir`) and falls back to a real copy if that's not possible
/// (`EXDEV`, a chroot base on a different mount), matching the same
/// reflink-then-copy-fallback pattern `ephemera_image::clone_for_vm` already
/// uses for the same underlying reason.
fn link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    if fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    fs::copy(src, dst)
        .with_context(|| format!("copying {} to {} (hardlink failed, likely a cross-device chroot_base_dir)", src.display(), dst.display()))?;
    Ok(())
}

fn control_socket(vm: &VmRecord) -> Result<&Path> {
    vm.control_socket.as_deref().context("Firecracker VM has no control socket recorded")
}

async fn set_vm_state(vm: &VmRecord, state: &str) -> Result<()> {
    http::request(control_socket(vm)?, "PATCH", "/vm", Some(&json!({"state": state})), API_TIMEOUT).await
}
