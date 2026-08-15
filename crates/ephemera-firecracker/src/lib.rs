// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

mod http;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use ephemera_core::{
    backend::{path_arg, LaunchContext, LaunchResult, VmBackend},
    config::Config,
    model::{BackendKind, CreateVmRequest, NetworkSpec, VmRecord},
    process::spawn_logged,
};
use serde_json::json;
use std::{fs, time::Duration};

const API_TIMEOUT: Duration = Duration::from_secs(10);

pub struct FirecrackerBackend;

fn config_json(cfg: &Config, req: &CreateVmRequest, ctx: &LaunchContext) -> Result<serde_json::Value> {
    let kernel = req.kernel.as_ref().or(cfg.firecracker_kernel.as_ref())
        .context("Firecracker requires a Linux kernel via request.kernel or config.firecracker_kernel")?;
    let boot_args = req.kernel_args.clone().unwrap_or_else(||
        "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw".into());

    let mut drives = vec![json!({
        "drive_id": "rootfs",
        "path_on_host": path_arg(&ctx.disk),
        "is_root_device": true,
        "is_read_only": false
    })];
    if let Some(seed) = &ctx.seed_disk {
        drives.push(json!({
            "drive_id": "seed",
            "path_on_host": path_arg(seed),
            "is_root_device": false,
            "is_read_only": true
        }));
    }

    let mut root = json!({
        "boot-source": {
            "kernel_image_path": path_arg(kernel),
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
        let socket = ctx.vsock_socket.as_ref().context("agent enabled but no vsock socket path was assigned")?;
        root.as_object_mut().unwrap().insert(
            "vsock".into(),
            json!({"guest_cid": cid, "uds_path": path_arg(socket)}),
        );
    }

    Ok(root)
}

#[async_trait]
impl VmBackend for FirecrackerBackend {
    fn kind(&self) -> BackendKind { BackendKind::Firecracker }

    async fn launch(&self, cfg: &Config, req: &CreateVmRequest, ctx: &LaunchContext) -> Result<LaunchResult> {
        let api = ctx.workspace.join("firecracker.sock");
        let cfg_path = ctx.workspace.join("firecracker.json");
        fs::write(&cfg_path, serde_json::to_vec_pretty(&config_json(cfg, req, ctx)?)?)?;
        let args = vec![
            "--api-sock".into(), api.display().to_string(),
            "--config-file".into(), cfg_path.display().to_string(),
        ];
        let child = spawn_logged(&cfg.firecracker_binary, &args, &ctx.log_path).await?;
        let pid = child.id().context("Firecracker exited before PID was available")?;
        Ok(LaunchResult { pid, control_socket: Some(api) })
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
            &vm.workspace.join("firecracker.sock"),
            "PUT",
            "/actions",
            Some(&json!({"action_type": "SendCtrlAltDel"})),
            API_TIMEOUT,
        )
        .await
    }
}

async fn set_vm_state(vm: &VmRecord, state: &str) -> Result<()> {
    http::request(
        &vm.workspace.join("firecracker.sock"),
        "PATCH",
        "/vm",
        Some(&json!({"state": state})),
        API_TIMEOUT,
    )
    .await
}
