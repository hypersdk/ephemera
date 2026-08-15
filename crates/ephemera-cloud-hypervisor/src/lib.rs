use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use ephemera_core::{
    backend::{path_arg, LaunchContext, LaunchResult, VmBackend},
    config::Config,
    model::{BackendKind, CreateVmRequest, NetworkSpec},
    process::spawn_logged,
};

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
        NetworkSpec::User { .. } => bail!("Cloud Hypervisor backend requires network.mode=none or tap in this MVP"),
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
        let child = spawn_logged(&cfg.cloud_hypervisor_binary, &args, &ctx.log_path).await?;
        let pid = child.id().context("Cloud Hypervisor exited before PID was available")?;
        Ok(LaunchResult { pid, control_socket: Some(ctx.workspace.join("ch-api.sock")) })
    }
}
