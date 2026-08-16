// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use ephemera_core::{
    backend::{path_arg, LaunchContext, LaunchResult, VmBackend},
    config::Config,
    model::{BackendKind, CreateVmRequest, NetworkSpec, VmRecord},
    process::{run_checked_timeout, spawn_logged},
};
use std::time::Duration;

const CH_REMOTE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct CloudHypervisorBackend;

pub fn build_args(cfg: &Config, req: &CreateVmRequest, ctx: &LaunchContext) -> Result<Vec<String>> {
    let api = ctx.workspace.join("ch-api.sock");
    let mut a = vec![
        "--api-socket".into(), api.display().to_string(),
        "--cpus".into(), format!("boot={}", req.vcpus),
        "--memory".into(), format!("size={}M", req.memory_mib),
        "--disk".into(), format!("path={}", path_arg(&ctx.disk)),
        "--serial".into(), format!("file={}", ctx.log_path.display()),
        "--console".into(), "off".into(),
    ];
    if let Some(seed) = &ctx.seed_disk {
        a.extend(["--disk".into(), format!("path={},readonly=on", path_arg(seed))]);
    }

    match &ctx.network.spec {
        NetworkSpec::None => {}
        NetworkSpec::Tap { tap_name: Some(tap), mac, .. } => {
            let mut n = format!("tap={tap}");
            if let Some(mac) = mac { n.push_str(&format!(",mac={mac}")); }
            a.extend(["--net".into(), n]);
        }
        NetworkSpec::Tap { tap_name: None, .. } => bail!("tap network was not prepared"),
        NetworkSpec::Macvtap { mac, .. } => {
            let fd = ctx.network.macvtap_fd.context("macvtap network was not prepared")?;
            let mut n = format!("fd={fd}");
            if let Some(mac) = mac { n.push_str(&format!(",mac={mac}")); }
            a.extend(["--net".into(), n]);
        }
        NetworkSpec::User { .. } => bail!("Cloud Hypervisor backend requires network.mode=none, tap, or macvtap in this MVP"),
    }

    if req.agent.as_ref().is_some_and(|ag| ag.enabled) {
        let cid = ctx.guest_cid.context("agent enabled but no vsock CID was assigned")?;
        let socket = ctx.vsock_socket.as_ref().context("agent enabled but no vsock socket path was assigned")?;
        a.extend(["--vsock".into(), format!("cid={cid},socket={}", path_arg(socket))]);
    }

    if let Some(kernel) = &req.kernel {
        a.extend(["--kernel".into(), path_arg(kernel)]);
        if let Some(initrd) = &req.initrd { a.extend(["--initramfs".into(), path_arg(initrd)]); }
        if let Some(kargs) = &req.kernel_args { a.extend(["--cmdline".into(), kargs.clone()]); }
    } else if let Some(fw) = req.firmware.as_ref().or(cfg.cloud_hypervisor_firmware.as_ref()) {
        a.extend(["--firmware".into(), path_arg(fw)]);
    } else {
        bail!("Cloud Hypervisor needs req.kernel for direct boot or firmware/config cloud_hypervisor_firmware");
    }

    a.extend(req.extra_args.clone());
    Ok(a)
}

#[async_trait]
impl VmBackend for CloudHypervisorBackend {
    fn kind(&self) -> BackendKind { BackendKind::CloudHypervisor }

    async fn launch(&self, cfg: &Config, req: &CreateVmRequest, ctx: &LaunchContext) -> Result<LaunchResult> {
        let args = build_args(cfg, req, ctx)?;
        let (program, args) = ephemera_core::process::netns_wrap(ctx.network.netns.as_deref(), &cfg.cloud_hypervisor_binary, &args);
        let spawned = spawn_logged(&program, &args, &ctx.log_path).await;
        if let Some(fd) = ctx.network.macvtap_fd {
            ephemera_core::process::close_fd(fd);
        }
        let child = spawned?;
        let pid = child.id().context("Cloud Hypervisor exited before PID was available")?;
        Ok(LaunchResult { pid, control_socket: Some(ctx.workspace.join("ch-api.sock")), jail_path: None, vsock_socket: ctx.vsock_socket.clone(), virtiofsd_pids: Vec::new() })
    }

    async fn pause(&self, cfg: &Config, vm: &VmRecord) -> Result<()> {
        ch_remote(cfg, vm, "pause").await
    }

    async fn resume(&self, cfg: &Config, vm: &VmRecord) -> Result<()> {
        ch_remote(cfg, vm, "resume").await
    }

    async fn graceful_shutdown(&self, cfg: &Config, vm: &VmRecord) -> Result<()> {
        ch_remote(cfg, vm, "shutdown").await
    }
}

async fn ch_remote(cfg: &Config, vm: &VmRecord, subcommand: &str) -> Result<()> {
    let api = vm.workspace.join("ch-api.sock");
    run_checked_timeout(
        &cfg.ch_remote_binary,
        &["--api-socket".into(), api.display().to_string(), subcommand.into()],
        CH_REMOTE_TIMEOUT,
    )
    .await
}
