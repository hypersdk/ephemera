// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

mod qmp;

use anyhow::{Context, Result};
use async_trait::async_trait;
use ephemera_core::{
    backend::{path_arg, LaunchContext, LaunchResult, VmBackend},
    config::Config,
    model::{BackendKind, CreateVmRequest, NetworkSpec, VmRecord},
    process::spawn_logged,
};
use std::time::Duration;

const QMP_TIMEOUT: Duration = Duration::from_secs(10);

pub struct QemuBackend;

pub fn build_args(req: &CreateVmRequest, ctx: &LaunchContext) -> Result<Vec<String>> {
    // A `StorageBackend::Nbd` disk isn't opened as a local file at all — it's
    // attached via QEMU's native nbd: block client against the qemu-nbd
    // export this VM owns. Every other storage backend (including the
    // Default qcow2 overlay) opens `ctx.disk` directly, just with a format
    // that varies by backend (see `ephemera_image::storage::disk_format`).
    let disk_drive = match &ctx.nbd_export {
        Some(socket) => format!("file=nbd:unix:{},if=virtio,format=raw", socket.display()),
        None => format!("file={},if=virtio,format={},cache=none,aio=native", path_arg(&ctx.disk), ctx.disk_format),
    };
    let mut a = vec![
        "-enable-kvm".into(),
        "-machine".into(), "q35,accel=kvm".into(),
        "-cpu".into(), "host".into(),
        "-smp".into(), req.vcpus.to_string(),
        "-m".into(), req.memory_mib.to_string(),
        "-nodefaults".into(),
        "-display".into(), "none".into(),
        "-serial".into(), "stdio".into(),
        "-drive".into(), disk_drive,
    ];

    if let Some(seed) = &ctx.seed_disk {
        a.extend(["-drive".into(), format!("file={},if=virtio,format=raw,readonly=on", path_arg(seed))]);
    }

    match &ctx.network.spec {
        NetworkSpec::None => {}
        NetworkSpec::User { forwards } => {
            let mut netdev = "user,id=net0".to_string();
            for f in forwards {
                netdev.push_str(&format!(",hostfwd={}:127.0.0.1:{}-:{}", f.protocol, f.host_port, f.guest_port));
            }
            a.extend(["-netdev".into(), netdev, "-device".into(), "virtio-net-pci,netdev=net0".into()]);
        }
        NetworkSpec::Tap { tap_name, mac, .. } => {
            if let Some(tap) = tap_name {
                a.extend(["-netdev".into(), format!("tap,id=net0,ifname={tap},script=no,downscript=no")]);
                let dev = mac.as_ref().map(|m| format!("virtio-net-pci,netdev=net0,mac={m}"))
                    .unwrap_or_else(|| "virtio-net-pci,netdev=net0".into());
                a.extend(["-device".into(), dev]);
            }
        }
        NetworkSpec::Macvtap { mac, .. } => {
            let fd = ctx.network.macvtap_fd.context("macvtap network was not prepared")?;
            a.extend(["-netdev".into(), format!("tap,id=net0,fd={fd}")]);
            let dev = mac.as_ref().map(|m| format!("virtio-net-pci,netdev=net0,mac={m}"))
                .unwrap_or_else(|| "virtio-net-pci,netdev=net0".into());
            a.extend(["-device".into(), dev]);
        }
    }

    if req.agent.as_ref().is_some_and(|a| a.enabled) {
        if let Some(cid) = ctx.guest_cid {
            a.extend(["-device".into(), format!("vhost-vsock-pci,guest-cid={cid}")]);
        }
    }

    if let Some(kernel) = &req.kernel {
        a.extend(["-kernel".into(), path_arg(kernel)]);
        if let Some(initrd) = &req.initrd { a.extend(["-initrd".into(), path_arg(initrd)]); }
        if let Some(kargs) = &req.kernel_args { a.extend(["-append".into(), kargs.clone()]); }
    }

    let qmp = ctx.workspace.join("qmp.sock");
    a.extend(["-qmp".into(), format!("unix:{},server=on,wait=off", qmp.display())]);
    a.extend(req.extra_args.clone());
    Ok(a)
}

#[async_trait]
impl VmBackend for QemuBackend {
    fn kind(&self) -> BackendKind { BackendKind::Qemu }

    async fn launch(&self, cfg: &Config, req: &CreateVmRequest, ctx: &LaunchContext) -> Result<LaunchResult> {
        let args = build_args(req, ctx)?;
        let (program, args) = ephemera_core::process::netns_wrap(ctx.network.netns.as_deref(), &cfg.qemu_binary, &args);
        let spawned = spawn_logged(&program, &args, &ctx.log_path).await;
        // The child inherits the macvtap fd across exec (or spawn failed and
        // there's nothing to inherit); either way the parent's copy is done.
        if let Some(fd) = ctx.network.macvtap_fd {
            ephemera_core::process::close_fd(fd);
        }
        let child = spawned?;
        let pid = child.id().context("QEMU exited before PID was available")?;
        Ok(LaunchResult { pid, control_socket: Some(ctx.workspace.join("qmp.sock")), jail_path: None, vsock_socket: None })
    }

    async fn pause(&self, _cfg: &Config, vm: &VmRecord) -> Result<()> {
        qmp::execute(&vm.workspace.join("qmp.sock"), "stop", None, QMP_TIMEOUT).await?;
        Ok(())
    }

    async fn resume(&self, _cfg: &Config, vm: &VmRecord) -> Result<()> {
        qmp::execute(&vm.workspace.join("qmp.sock"), "cont", None, QMP_TIMEOUT).await?;
        Ok(())
    }

    async fn graceful_shutdown(&self, _cfg: &Config, vm: &VmRecord) -> Result<()> {
        qmp::execute(&vm.workspace.join("qmp.sock"), "system_powerdown", None, QMP_TIMEOUT).await?;
        Ok(())
    }
}
