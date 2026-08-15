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
use ephemera_storage::Store;
use std::{fs, sync::Arc};
use uuid::Uuid;

pub fn backend(kind: BackendKind) -> Box<dyn VmBackend> {
    match kind {
        BackendKind::Qemu => Box::new(ephemera_qemu::QemuBackend),
        BackendKind::CloudHypervisor => Box::new(ephemera_cloud_hypervisor::CloudHypervisorBackend),
        BackendKind::Firecracker => Box::new(ephemera_firecracker::FirecrackerBackend),
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

    pub async fn create(self: &Arc<Self>, req: CreateVmRequest) -> Result<VmRecord> {
        if !req.image.exists() { bail!("base image does not exist: {}", req.image.display()); }
        let id = Uuid::new_v4();
        let workspace = self.cfg.state_dir.join("instances").join(id.to_string());
        fs::create_dir_all(&workspace)?;
        let disk = workspace.join(if req.backend == BackendKind::Qemu { "root.qcow2" } else { "root.raw" });
        let log_path = workspace.join("console.log");
        let expires_at = req.ttl_seconds.map(|s| Utc::now() + Duration::seconds(s as i64));
        let mut record = VmRecord {
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
        };
        self.store.insert(record.clone()).await?;

        let result: Result<()> = async {
            ephemera_image::clone_for_vm(&self.cfg, &req.image, req.backend, &disk, req.disk_size_gib).await?;
            let seed = match &req.cloud_init {
                Some(ci) => Some(ephemera_image::cloudinit::build_seed(&self.cfg, &workspace, ci).await?),
                None => None,
            };
            let network = ephemera_network::prepare(&self.cfg, id, &req.network).await?;
            record.tap_name = network.tap_name.clone();
            record.seed_disk = seed.clone();

            let ctx = LaunchContext {
                id,
                workspace: workspace.clone(),
                disk: disk.clone(),
                seed_disk: seed,
                log_path: log_path.clone(),
                network,
            };
            let launch = backend(req.backend).launch(&self.cfg, &req, &ctx).await?;
            record.pid = Some(launch.pid);
            record.control_socket = launch.control_socket;
            record.status = VmStatus::Running;
            Ok(())
        }.await;

        if let Err(e) = result {
            if let Some(tap) = &record.tap_name { let _ = ephemera_network::cleanup_tap(tap).await; }
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
                process::terminate_pid(pid).await?;
            }
        }
        if let Some(tap) = &vm.tap_name { let _ = ephemera_network::cleanup_tap(tap).await; }
        vm.status = VmStatus::Stopped;
        vm.pid = None;
        self.store.update(vm.clone()).await?;
        Ok(vm)
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
                        if let Some(tap) = &vm.tap_name { let _ = ephemera_network::cleanup_tap(tap).await; }
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
