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
use std::path::PathBuf;
use std::time::Duration;

const QMP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for each `virtiofsd` to create its listening socket
/// before giving up and launching QEMU anyway (which would then fail to
/// connect with a clear error, rather than this hanging indefinitely).
const VIRTIOFSD_SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

pub struct QemuBackend;

/// One `virtiofsd` instance's device-facing identity, resolved before QEMU
/// itself is built/launched — `(tag, socket_path)`, index-ordered with
/// `req.shared_folders` (`tag` is always `"fs{index}"`).
pub type VirtiofsSocket = (String, PathBuf);

pub fn build_args(req: &CreateVmRequest, ctx: &LaunchContext, virtiofs_sockets: &[VirtiofsSocket]) -> Result<Vec<String>> {
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
        // Fixed, well-known path within this VM's own workspace — no port
        // allocation, no collision bookkeeping needed. Consumers (e.g.
        // zyvor-fabric's VNC proxy) derive the same path themselves from
        // `VmRecord::workspace`, already exposed via the REST API.
        "-vnc".into(), format!("unix:{}", path_arg(&ctx.workspace.join("vnc.sock"))),
        "-serial".into(), "stdio".into(),
        "-drive".into(), disk_drive,
    ];

    if let Some(seed) = &ctx.seed_disk {
        a.extend(["-drive".into(), format!("file={},if=virtio,format=raw,readonly=on", path_arg(seed))]);
    }

    // virtiofs requires the guest's RAM to be backed by shared memory, not
    // QEMU's default anonymous allocation — `vhost-user-fs-pci` otherwise
    // fails to attach. `-m` above still sets the *size*; this object is
    // what makes the *backing* shareable with the virtiofsd process(es).
    if !virtiofs_sockets.is_empty() {
        a.extend(["-object".into(), format!("memory-backend-memfd,id=mem,size={}M,share=on", req.memory_mib)]);
        a.extend(["-numa".into(), "node,memdev=mem".into()]);
    }
    for (i, (tag, socket)) in virtiofs_sockets.iter().enumerate() {
        a.extend(["-chardev".into(), format!("socket,id=vfsock{i},path={}", path_arg(socket))]);
        a.extend(["-device".into(), format!("vhost-user-fs-pci,queue-size=1024,chardev=vfsock{i},tag={tag}")]);
    }

    match &ctx.network.spec {
        NetworkSpec::None => {}
        NetworkSpec::User { forwards } => {
            let mut netdev = "user,id=net0".to_string();
            // Bind to all interfaces, not just loopback: these forwards
            // exist specifically so a caller outside the host (e.g. SSH
            // from a laptop) can reach the guest -- 127.0.0.1 would make
            // every exposed port reachable only from processes already on
            // the host itself, defeating the feature entirely.
            for f in forwards {
                netdev.push_str(&format!(",hostfwd={}:0.0.0.0:{}-:{}", f.protocol, f.host_port, f.guest_port));
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

/// Spawns one `virtiofsd` per `req.shared_folders` entry, in order, each
/// listening on its own socket under `ctx.workspace`. On any failure,
/// already-spawned instances from this call are killed before returning —
/// callers never have to reconcile a partial set themselves.
async fn spawn_virtiofsd_instances(
    cfg: &Config,
    req: &CreateVmRequest,
    ctx: &LaunchContext,
) -> Result<(Vec<u32>, Vec<VirtiofsSocket>)> {
    let mut pids = Vec::new();
    let mut sockets = Vec::new();
    for (i, share) in req.shared_folders.iter().enumerate() {
        let socket = ctx.workspace.join(format!("virtiofs-{i}.sock"));
        let tag = format!("fs{i}");
        let mut args = vec![
            "--socket-path".to_string(), path_arg(&socket),
            "--shared-dir".to_string(), path_arg(&share.host_path),
        ];
        if share.read_only {
            args.push("--readonly".to_string());
        }
        let log = ctx.workspace.join(format!("virtiofsd-{i}.log"));
        let spawn_result = spawn_logged(&cfg.virtiofsd_binary, &args, &log)
            .await
            .with_context(|| format!("spawning virtiofsd for shared_folders[{i}] ({})", share.host_path.display()));
        let child = match spawn_result {
            Ok(c) => c,
            Err(e) => {
                kill_pids(&pids);
                return Err(e);
            }
        };
        let Some(pid) = child.id() else {
            kill_pids(&pids);
            anyhow::bail!("virtiofsd for shared_folders[{i}] exited before PID was available");
        };
        // virtiofsd creates its listening socket asynchronously after
        // startup; QEMU connects as the vhost-user client and needs it to
        // already exist. Not finding it within the timeout isn't fatal
        // here — QEMU will fail to connect with its own clear error, which
        // beats hanging this launch indefinitely on a stuck virtiofsd.
        let deadline = tokio::time::Instant::now() + VIRTIOFSD_SOCKET_TIMEOUT;
        while !socket.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        pids.push(pid);
        sockets.push((tag, socket));
    }
    Ok((pids, sockets))
}

fn kill_pids(pids: &[u32]) {
    for pid in pids {
        unsafe {
            libc::kill(*pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

#[async_trait]
impl VmBackend for QemuBackend {
    fn kind(&self) -> BackendKind { BackendKind::Qemu }

    async fn launch(&self, cfg: &Config, req: &CreateVmRequest, ctx: &LaunchContext) -> Result<LaunchResult> {
        let (virtiofsd_pids, virtiofs_sockets) = match spawn_virtiofsd_instances(cfg, req, ctx).await {
            Ok(v) => v,
            Err(e) => {
                if let Some(fd) = ctx.network.macvtap_fd {
                    ephemera_core::process::close_fd(fd);
                }
                return Err(e);
            }
        };

        let args = build_args(req, ctx, &virtiofs_sockets)?;
        let (program, args) = ephemera_core::process::netns_wrap(ctx.network.netns.as_deref(), &cfg.qemu_binary, &args);
        let spawned = spawn_logged(&program, &args, &ctx.log_path).await;
        // The child inherits the macvtap fd across exec (or spawn failed and
        // there's nothing to inherit); either way the parent's copy is done.
        if let Some(fd) = ctx.network.macvtap_fd {
            ephemera_core::process::close_fd(fd);
        }
        let child = match spawned {
            Ok(c) => c,
            Err(e) => {
                kill_pids(&virtiofsd_pids);
                return Err(e);
            }
        };
        let Some(pid) = child.id() else {
            kill_pids(&virtiofsd_pids);
            anyhow::bail!("QEMU exited before PID was available");
        };
        Ok(LaunchResult { pid, control_socket: Some(ctx.workspace.join("qmp.sock")), jail_path: None, vsock_socket: None, virtiofsd_pids })
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

#[cfg(test)]
mod tests {
    use super::*;
    use ephemera_core::backend::PreparedNetwork;
    use ephemera_core::model::NetworkSpec;

    fn req(memory_mib: u64) -> CreateVmRequest {
        CreateVmRequest {
            name: "fixture".into(),
            backend: BackendKind::Qemu,
            image: "/tmp/base.qcow2".into(),
            vcpus: 1,
            memory_mib,
            disk_size_gib: None,
            kernel: None,
            initrd: None,
            firmware: None,
            kernel_args: None,
            network: NetworkSpec::None,
            cloud_init: None,
            ttl_seconds: None,
            extra_args: vec![],
            agent: None,
            storage: ephemera_core::model::StorageBackend::Default,
            shared_folders: vec![],
        }
    }

    fn ctx() -> LaunchContext {
        LaunchContext {
            id: uuid::Uuid::nil(),
            workspace: "/tmp/eph-fixture".into(),
            disk: "/tmp/eph-fixture/root.qcow2".into(),
            seed_disk: None,
            log_path: "/tmp/eph-fixture/console.log".into(),
            network: PreparedNetwork { spec: NetworkSpec::None, tap_name: None, macvtap_fd: None, netns: None },
            guest_cid: None,
            vsock_socket: None,
            disk_format: "qcow2".into(),
            nbd_export: None,
        }
    }

    #[test]
    fn no_shares_means_no_virtiofs_args() {
        let args = build_args(&req(2048), &ctx(), &[]).unwrap();
        assert!(!args.iter().any(|a| a.contains("memory-backend-memfd")));
        assert!(!args.iter().any(|a| a.contains("vhost-user-fs-pci")));
        assert!(args.iter().any(|a| a == "2048"));
    }

    #[test]
    fn shares_add_shared_memory_backend_and_one_device_per_share() {
        let sockets: Vec<VirtiofsSocket> = vec![
            ("fs0".to_string(), "/tmp/eph-fixture/virtiofs-0.sock".into()),
            ("fs1".to_string(), "/tmp/eph-fixture/virtiofs-1.sock".into()),
        ];
        let args = build_args(&req(4096), &ctx(), &sockets).unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("memory-backend-memfd,id=mem,size=4096M,share=on"));
        assert!(joined.contains("numa node,memdev=mem"));
        assert!(joined.contains("chardev=vfsock0,tag=fs0"));
        assert!(joined.contains("chardev=vfsock1,tag=fs1"));
        assert_eq!(args.iter().filter(|a| a.as_str() == "vhost-user-fs-pci,queue-size=1024,chardev=vfsock0,tag=fs0").count(), 1);
    }
}
