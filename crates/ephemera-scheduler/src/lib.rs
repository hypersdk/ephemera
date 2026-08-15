// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Context, Result};
use chrono::{Duration, Utc};
use ephemera_core::{
    backend::{LaunchContext, VmBackend},
    config::Config,
    model::{BackendKind, CreateVmRequest, VmRecord, VmStatus},
    process,
};
use ephemera_guest_protocol::AgentRequest;
use ephemera_storage::Store;
use std::{fs, sync::Arc};
use uuid::Uuid;

/// Linux reserves vsock CIDs 0–2 (hypervisor/local/host); guest CIDs start
/// at 3 and must be unique across the whole host.
const FIRST_GUEST_CID: u32 = 3;

const GRACEFUL_SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

pub fn backend(kind: BackendKind) -> Result<Box<dyn VmBackend>> {
    Ok(match kind {
        BackendKind::Qemu => Box::new(ephemera_qemu::QemuBackend),
        BackendKind::CloudHypervisor => Box::new(ephemera_cloud_hypervisor::CloudHypervisorBackend),
        BackendKind::Firecracker => Box::new(ephemera_firecracker::FirecrackerBackend),
        BackendKind::Auto => bail!("VM has an unresolved BackendKind::Auto — this is a bug, backend selection must happen before dispatch"),
    })
}

/// Picks a concrete backend for `BackendKind::Auto`, preferring Firecracker
/// (fastest microVM start) when a direct-boot kernel is available, then
/// Cloud Hypervisor when a kernel or firmware is available, falling back to
/// QEMU (works with just a disk image, no kernel/firmware required — the
/// only one of the three that boots via its own BIOS/UEFI). Any non-`Auto`
/// request passes through unchanged. Called once, as the very first step of
/// `create()`, before the resolved kind is ever persisted or dispatched on.
pub fn resolve_backend(req: &CreateVmRequest, cfg: &Config) -> BackendKind {
    if req.backend != BackendKind::Auto {
        return req.backend;
    }
    let firecracker_ok = req.kernel.is_some() || cfg.firecracker_kernel.is_some();
    let cloud_hypervisor_ok = req.kernel.is_some() || req.firmware.is_some() || cfg.cloud_hypervisor_firmware.is_some();
    if firecracker_ok {
        BackendKind::Firecracker
    } else if cloud_hypervisor_ok {
        BackendKind::CloudHypervisor
    } else {
        BackendKind::Qemu
    }
}

pub struct VmManager {
    pub cfg: Config,
    pub store: Arc<Store>,
}

impl VmManager {
    pub fn new(cfg: Config) -> Result<Arc<Self>> {
        cfg.ensure_dirs()?;
        let store = Arc::new(Store::load(&cfg.state_dir)?);
        Ok(Arc::new(Self { cfg, store }))
    }

    pub async fn create(self: &Arc<Self>, mut req: CreateVmRequest) -> Result<VmRecord> {
        // Resolve BackendKind::Auto before anything else — everything below
        // (the disk filename, the persisted record, the launch dispatch)
        // assumes a concrete backend and must never see Auto.
        req.backend = resolve_backend(&req, &self.cfg);
        if !req.image.exists() { bail!("base image does not exist: {}", req.image.display()); }
        let id = Uuid::new_v4();
        let workspace = self.cfg.state_dir.join("instances").join(id.to_string());
        fs::create_dir_all(&workspace)?;
        let disk = workspace.join(if req.backend == BackendKind::Qemu { "root.qcow2" } else { "root.raw" });
        let log_path = workspace.join("console.log");
        let expires_at = req.ttl_seconds.map(|s| Utc::now() + Duration::seconds(s as i64));
        let needs_cid = req.agent.as_ref().is_some_and(|a| a.enabled);

        let placeholder = VmRecord {
            id,
            name: req.name.clone(),
            backend: req.backend,
            status: VmStatus::Creating,
            pid: None,
            created_at: Utc::now(),
            expires_at,
            workspace: workspace.clone(),
            disk: disk.clone(),
            seed_disk: None,
            tap_name: None,
            control_socket: None,
            log_path: log_path.clone(),
            error: None,
            request: req.clone(),
            guest_cid: None,
        };
        // Deciding the CID and reserving it happen as one atomic, locked
        // operation in the store — see ephemera-storage::Store::insert_with_cid
        // for why a separate "list, then insert" pair isn't safe across
        // concurrent `ephemera` processes.
        let mut record = self.store.insert_with_cid(placeholder, needs_cid, FIRST_GUEST_CID).await?;
        let guest_cid = record.guest_cid;

        let result: Result<()> = async {
            ephemera_image::clone_for_vm(&self.cfg, &req.image, req.backend, &disk, req.disk_size_gib).await?;
            let seed = match &req.cloud_init {
                Some(ci) => Some(ephemera_image::cloudinit::build_seed(&self.cfg, &workspace, ci).await?),
                None => None,
            };
            let network = ephemera_network::prepare(&self.cfg, id, &req.network).await?;
            record.tap_name = network.tap_name.clone();
            record.seed_disk = seed.clone();

            // QEMU talks straight to the guest_cid over a real kernel vsock
            // device; Cloud Hypervisor/Firecracker instead proxy vsock over
            // a UDS the VMM creates at launch, so only they need a path.
            let vsock_socket = match (guest_cid, req.backend) {
                (Some(_), BackendKind::Qemu) => None,
                (Some(_), _) => Some(workspace.join("vsock.sock")),
                (None, _) => None,
            };

            let ctx = LaunchContext {
                id,
                workspace: workspace.clone(),
                disk: disk.clone(),
                seed_disk: seed,
                log_path: log_path.clone(),
                network,
                guest_cid,
                vsock_socket,
            };
            let launch = backend(req.backend)?.launch(&self.cfg, &req, &ctx).await?;
            record.pid = Some(launch.pid);
            record.control_socket = launch.control_socket;
            record.status = VmStatus::Running;
            Ok(())
        }.await;

        if let Err(e) = result {
            if let Some(tap) = &record.tap_name { let _ = ephemera_network::cleanup(&req.network, tap).await; }
            record.status = VmStatus::Failed;
            record.error = Some(format!("{e:#}"));
            self.store.update(record.clone()).await?;
            return Err(e);
        }
        self.store.update(record.clone()).await?;
        Ok(record)
    }

    pub async fn list(&self) -> Vec<VmRecord> { self.store.list().await }

    pub async fn get(&self, id: Uuid) -> Result<VmRecord> {
        self.store.get(id).await.context("VM not found")
    }

    pub async fn stop(&self, id: Uuid) -> Result<VmRecord> {
        let mut vm = self.get(id).await?;
        if let Some(pid) = vm.pid {
            if process::process_alive(pid).await {
                // Ask the VMM to shut the guest down cleanly first; only
                // force-kill if it doesn't exit within the grace period (or
                // the VMM's control channel didn't respond at all).
                let asked_nicely = match backend(vm.backend) {
                    Ok(b) => b.graceful_shutdown(&self.cfg, &vm).await.is_ok(),
                    Err(_) => false,
                };
                let exited = asked_nicely && process::wait_for_exit(pid, (GRACEFUL_SHUTDOWN_WAIT.as_millis() / 100) as u32).await;
                if !exited && process::process_alive(pid).await {
                    process::terminate_pid(pid).await?;
                }
            }
        }
        if let Some(tap) = &vm.tap_name { let _ = ephemera_network::cleanup(&vm.request.network, tap).await; }
        vm.status = VmStatus::Stopped;
        vm.pid = None;
        self.store.update(vm.clone()).await?;
        Ok(vm)
    }

    pub async fn pause(&self, id: Uuid) -> Result<VmRecord> {
        let mut vm = self.get(id).await?;
        backend(vm.backend)?.pause(&self.cfg, &vm).await?;
        vm.status = VmStatus::Paused;
        self.store.update(vm.clone()).await?;
        Ok(vm)
    }

    pub async fn resume(&self, id: Uuid) -> Result<VmRecord> {
        let mut vm = self.get(id).await?;
        backend(vm.backend)?.resume(&self.cfg, &vm).await?;
        vm.status = VmStatus::Running;
        self.store.update(vm.clone()).await?;
        Ok(vm)
    }

    pub async fn exec(&self, id: Uuid, command: String, timeout_seconds: Option<u64>) -> Result<ephemera_guest_protocol::AgentResponse> {
        let vm = self.get(id).await?;
        let wait = std::time::Duration::from_secs(timeout_seconds.unwrap_or(ephemera_guest_protocol::DEFAULT_EXEC_TIMEOUT_SECS) + 5);
        ephemera_vsock_client::call(&vm, AgentRequest::Exec { command, timeout_seconds }, wait).await
    }

    pub async fn delete(&self, id: Uuid) -> Result<()> {
        let vm = self.get(id).await?;
        if vm.status == VmStatus::Running { let _ = self.stop(id).await; }
        let vm = self.store.remove(id).await?.context("VM vanished")?;
        if vm.workspace.exists() { fs::remove_dir_all(vm.workspace)?; }
        Ok(())
    }

    pub async fn reconcile(&self) -> Result<()> {
        for mut vm in self.store.list().await {
            if vm.status == VmStatus::Running {
                if let Some(pid) = vm.pid {
                    if !process::process_alive(pid).await {
                        vm.status = VmStatus::Stopped;
                        vm.pid = None;
                        if let Some(tap) = &vm.tap_name { let _ = ephemera_network::cleanup(&vm.request.network, tap).await; }
                        self.store.update(vm).await?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn start_reaper(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(me.cfg.reaper_interval_secs.max(1)));
            loop {
                tick.tick().await;
                if let Err(e) = me.reconcile().await { tracing::warn!(error=?e, "reconcile failed"); }
                let now = Utc::now();
                for vm in me.store.list().await {
                    if vm.expires_at.is_some_and(|t| t <= now) {
                        if let Err(e) = me.delete(vm.id).await { tracing::warn!(vm=%vm.id, error=?e, "TTL cleanup failed"); }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ephemera_core::model::NetworkSpec;

    fn req(backend: BackendKind, kernel: Option<&str>, firmware: Option<&str>) -> CreateVmRequest {
        CreateVmRequest {
            name: "fixture".into(),
            backend,
            image: "/tmp/base.qcow2".into(),
            vcpus: 1,
            memory_mib: 512,
            disk_size_gib: None,
            kernel: kernel.map(Into::into),
            initrd: None,
            firmware: firmware.map(Into::into),
            kernel_args: None,
            network: NetworkSpec::None,
            cloud_init: None,
            ttl_seconds: None,
            extra_args: vec![],
            agent: None,
        }
    }

    #[test]
    fn non_auto_backend_passes_through_unchanged() {
        let cfg = Config::default();
        for backend in [BackendKind::Qemu, BackendKind::CloudHypervisor, BackendKind::Firecracker] {
            let r = req(backend, None, None);
            assert_eq!(resolve_backend(&r, &cfg), backend);
        }
    }

    #[test]
    fn auto_prefers_firecracker_when_request_supplies_a_kernel() {
        let cfg = Config::default();
        let r = req(BackendKind::Auto, Some("/boot/vmlinux"), None);
        assert_eq!(resolve_backend(&r, &cfg), BackendKind::Firecracker);
    }

    #[test]
    fn auto_prefers_firecracker_when_config_has_a_default_kernel() {
        let mut cfg = Config::default();
        cfg.firecracker_kernel = Some("/boot/vmlinux".into());
        let r = req(BackendKind::Auto, None, None);
        assert_eq!(resolve_backend(&r, &cfg), BackendKind::Firecracker);
    }

    #[test]
    fn auto_falls_back_to_cloud_hypervisor_when_only_firmware_is_available() {
        let cfg = Config::default();
        let r = req(BackendKind::Auto, None, Some("/usr/share/hypervisor-fw"));
        assert_eq!(resolve_backend(&r, &cfg), BackendKind::CloudHypervisor);
    }

    #[test]
    fn auto_falls_back_to_cloud_hypervisor_when_config_has_default_firmware() {
        let mut cfg = Config::default();
        cfg.cloud_hypervisor_firmware = Some("/usr/share/hypervisor-fw".into());
        let r = req(BackendKind::Auto, None, None);
        assert_eq!(resolve_backend(&r, &cfg), BackendKind::CloudHypervisor);
    }

    #[test]
    fn auto_falls_back_to_qemu_with_nothing_configured() {
        let cfg = Config::default();
        let r = req(BackendKind::Auto, None, None);
        assert_eq!(resolve_backend(&r, &cfg), BackendKind::Qemu);
    }

    #[test]
    fn backend_rejects_unresolved_auto() {
        assert!(backend(BackendKind::Auto).is_err());
    }
}
