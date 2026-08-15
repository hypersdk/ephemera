use anyhow::{Context, Result};
use async_trait::async_trait;
use ephemera_core::{
    backend::{path_arg, LaunchContext, LaunchResult, VmBackend},
    config::Config,
    model::{BackendKind, CreateVmRequest, NetworkSpec},
    process::spawn_logged,
};

pub struct QemuBackend;

pub fn build_args(req: &CreateVmRequest, ctx: &LaunchContext) -> Vec<String> {
    let mut a = vec![
        "-enable-kvm".into(),
        "-machine".into(), "q35,accel=kvm".into(),
        "-cpu".into(), "host".into(),
        "-smp".into(), req.vcpus.to_string(),
        "-m".into(), req.memory_mib.to_string(),
        "-nodefaults".into(),
        "-display".into(), "none".into(),
        "-serial".into(), "stdio".into(),
        "-drive".into(), format!("file={},if=virtio,format=qcow2,cache=none,aio=native", path_arg(&ctx.disk)),
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
    }

    if let Some(kernel) = &req.kernel {
        a.extend(["-kernel".into(), path_arg(kernel)]);
        if let Some(initrd) = &req.initrd { a.extend(["-initrd".into(), path_arg(initrd)]); }
        if let Some(kargs) = &req.kernel_args { a.extend(["-append".into(), kargs.clone()]); }
    }

    let qmp = ctx.workspace.join("qmp.sock");
    a.extend(["-qmp".into(), format!("unix:{},server=on,wait=off", qmp.display())]);
    a.extend(req.extra_args.clone());
    a
}

#[async_trait]
impl VmBackend for QemuBackend {
    fn kind(&self) -> BackendKind { BackendKind::Qemu }

    async fn launch(&self, cfg: &Config, req: &CreateVmRequest, ctx: &LaunchContext) -> Result<LaunchResult> {
        let args = build_args(req, ctx);
        let child = spawn_logged(&cfg.qemu_binary, &args, &ctx.log_path).await?;
        let pid = child.id().context("QEMU exited before PID was available")?;
        Ok(LaunchResult { pid, control_socket: Some(ctx.workspace.join("qmp.sock")) })
    }
}
