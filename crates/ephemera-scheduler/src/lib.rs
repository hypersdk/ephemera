// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Context, Result};
use chrono::{Duration, Utc};
use ephemera_core::{
    backend::{LaunchContext, VmBackend},
    config::Config,
    model::{BackendKind, ClaimOverrides, CreateVmRequest, PoolRecord, PoolSpec, VmRecord, VmStatus},
    process,
};
use ephemera_guest_protocol::AgentRequest;
use ephemera_storage::{PoolStore, Store};
use std::{collections::HashMap, fs, sync::Arc};
use tokio::sync::Mutex as AsyncMutex;
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

/// Admission check for `cfg.policy`, run once resolved (see `resolve_backend`)
/// but before any disk/network work — a rejected request should be cheap.
fn validate_policy(req: &CreateVmRequest, cfg: &Config) -> Result<()> {
    let p = &cfg.policy;
    if let Some(max) = p.max_vcpus {
        if req.vcpus > max {
            bail!("request vcpus ({}) exceeds policy max_vcpus ({max})", req.vcpus);
        }
    }
    if let Some(max) = p.max_memory_mib {
        if req.memory_mib > max {
            bail!("request memory_mib ({}) exceeds policy max_memory_mib ({max})", req.memory_mib);
        }
    }
    if let Some(max) = p.max_disk_gib {
        if let Some(disk) = req.disk_size_gib {
            if disk > max {
                bail!("request disk_size_gib ({disk}) exceeds policy max_disk_gib ({max})");
            }
        }
    }
    if let Some(max) = p.max_ttl_seconds {
        match req.ttl_seconds {
            Some(ttl) if ttl > max => bail!("request ttl_seconds ({ttl}) exceeds policy max_ttl_seconds ({max})"),
            None => bail!("policy requires ttl_seconds to be set (max_ttl_seconds={max}); unbounded VMs are not allowed"),
            _ => {}
        }
    }
    if let Some(allowed) = &p.allowed_backends {
        if !allowed.contains(&req.backend) {
            bail!("backend {:?} is not permitted by policy allowed_backends {:?}", req.backend, allowed);
        }
    }
    if let Some(dirs) = &p.allowed_image_dirs {
        if !dirs.iter().any(|d| req.image.starts_with(d)) {
            bail!("image {} is not under any policy allowed_image_dirs {:?}", req.image.display(), dirs);
        }
    }
    Ok(())
}

pub struct VmManager {
    pub cfg: Config,
    pub store: Arc<Store>,
    pub pools: Arc<PoolStore>,
    /// One mutex per pool name, created on demand, so concurrent backfill
    /// triggers for the *same* pool (e.g. `create_pool` and a `claim` racing
    /// each other) serialize instead of both creating members past `size`;
    /// backfills for *different* pools still run fully in parallel.
    backfill_locks: AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl VmManager {
    pub fn new(cfg: Config) -> Result<Arc<Self>> {
        cfg.ensure_dirs()?;
        let store = Arc::new(Store::load(&cfg.state_dir)?);
        let pools = Arc::new(PoolStore::load(&cfg.state_dir)?);
        Ok(Arc::new(Self { cfg, store, pools, backfill_locks: AsyncMutex::new(HashMap::new()) }))
    }

    pub async fn create(self: &Arc<Self>, mut req: CreateVmRequest) -> Result<VmRecord> {
        // Resolve BackendKind::Auto before anything else — everything below
        // (the disk filename, the persisted record, the launch dispatch)
        // assumes a concrete backend and must never see Auto.
        req.backend = resolve_backend(&req, &self.cfg);
        validate_policy(&req, &self.cfg)?;
        if !req.image.exists() { bail!("base image does not exist: {}", req.image.display()); }
        let id = Uuid::new_v4();
        let workspace = self.cfg.state_dir.join("instances").join(id.to_string());
        fs::create_dir_all(&workspace)?;
        let disk = workspace.join(if req.backend == BackendKind::Qemu { "root.qcow2" } else { "root.raw" });
        let log_path = workspace.join("console.log");
        let expires_at = req.ttl_seconds.map(|s| Utc::now() + Duration::seconds(s as i64));
        let needs_cid = req.agent.as_ref().is_some_and(|a| a.enabled);
        // Every agent-enabled VM gets a token whether the caller supplied
        // one or not — generated here (before `placeholder` is built) so
        // the persisted record always reflects the token actually burned
        // into the guest's disk below, never a stale/absent one.
        if needs_cid {
            let agent = req.agent.as_mut().expect("needs_cid implies req.agent is Some");
            if agent.token.is_none() {
                agent.token = Some(Uuid::new_v4().to_string());
            }
        }

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
            if let Some(token) = req.agent.as_ref().and_then(|a| a.token.as_deref()) {
                ephemera_image::inject_guest_agent_token(&disk, token).await
                    .context("injecting guest-agent auth token into instance disk")?;
            }
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

    // ---- Warm VM pools ----
    //
    // A pool keeps `size` VMs booted from `template` sitting `Paused`,
    // ready to be handed out by `claim_from_pool` in resume time (already
    // fast — see the "Pause, resume, and exec" README section) instead of
    // full create time. Pool membership (`PoolStore`) and VM lifecycle
    // (`Store`) are separate, separately-locked stores; the invariant kept
    // between them is "every id in `PoolRecord::members` is a `Paused`
    // `VmRecord` not claimed by anyone else," maintained by always popping
    // a member (removing it from that invariant) before doing anything
    // with it, and always pushing a newly-created member only after it's
    // fully paused and ready.

    pub async fn create_pool(self: &Arc<Self>, spec: PoolSpec) -> Result<PoolRecord> {
        if spec.size == 0 {
            bail!("pool size must be at least 1");
        }
        if self.pools.get(&spec.name).await.is_some() {
            bail!("pool '{}' already exists", spec.name);
        }
        let record = PoolRecord { name: spec.name.clone(), size: spec.size, template: spec.template, members: vec![] };
        self.pools.insert(record.clone()).await?;
        self.spawn_backfill(record.name.clone());
        Ok(record)
    }

    pub async fn list_pools(&self) -> Vec<PoolRecord> {
        self.pools.list().await
    }

    pub async fn get_pool(&self, name: &str) -> Result<PoolRecord> {
        self.pools.get(name).await.context("pool not found")
    }

    pub async fn delete_pool(&self, name: &str) -> Result<()> {
        let record = self.pools.remove(name).await?.context("pool not found")?;
        for id in record.members {
            let _ = self.delete(id).await;
        }
        Ok(())
    }

    /// Pops one ready member off `name`'s pool, resumes it (fast — the
    /// member was already fully booted and paused ahead of time), applies
    /// `overrides`, and triggers a backfill to replace it. Fails with a
    /// clear "no ready members" error rather than falling back to a slow
    /// synchronous create — a caller who wants that can just call
    /// `create()` directly instead of `claim_from_pool`.
    pub async fn claim_from_pool(self: &Arc<Self>, name: &str, overrides: ClaimOverrides) -> Result<VmRecord> {
        let Some(id) = self.pools.pop_member(name).await? else {
            bail!("pool '{name}' has no ready members right now — try again shortly, or increase its size");
        };
        self.spawn_backfill(name.to_string());

        let mut vm = match self.resume(id).await {
            Ok(vm) => vm,
            Err(e) => {
                // Already popped, so no one else can claim it — clean up
                // rather than leak a paused-but-broken VM outside any
                // pool's accounting.
                let _ = self.delete(id).await;
                return Err(e).context("resuming claimed pool member");
            }
        };
        if let Some(new_name) = overrides.name {
            vm.request.name = new_name.clone();
            vm.name = new_name;
        }
        vm.request.ttl_seconds = overrides.ttl_seconds;
        vm.expires_at = overrides.ttl_seconds.map(|s| Utc::now() + Duration::seconds(s as i64));
        self.store.update(vm.clone()).await?;
        Ok(vm)
    }

    fn spawn_backfill(self: &Arc<Self>, pool_name: String) {
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.backfill_pool(&pool_name).await {
                tracing::warn!(pool = %pool_name, error = ?e, "pool backfill failed");
            }
        });
    }

    async fn backfill_lock(&self, name: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.backfill_locks.lock().await;
        locks.entry(name.to_string()).or_insert_with(|| Arc::new(AsyncMutex::new(()))).clone()
    }

    async fn backfill_pool(self: &Arc<Self>, name: &str) -> Result<()> {
        // Serializes backfill runs for THIS pool only (create_pool's
        // initial fill and a claim's replenishment can race each other);
        // backfills for other pools take a different lock and proceed
        // concurrently.
        let lock = self.backfill_lock(name).await;
        let _guard = lock.lock().await;

        loop {
            let Some(record) = self.pools.get(name).await else { return Ok(()) }; // pool deleted meanwhile
            if record.members.len() >= record.size {
                return Ok(());
            }
            let mut req = record.template.clone();
            req.name = format!("{name}-pool-{}", Uuid::new_v4());
            req.ttl_seconds = None; // a paused pool member must never expire on its own
            let vm = self.create(req).await.context("creating pool member")?;
            let paused = self.pause(vm.id).await.context("pausing new pool member")?;
            if !self.pools.push_member(name, paused.id).await? {
                // Pool was deleted while this member was being created.
                let _ = self.delete(paused.id).await;
                return Ok(());
            }
        }
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

    fn req_with(vcpus: u8, memory_mib: u64, disk_size_gib: Option<u64>, ttl_seconds: Option<u64>, backend: BackendKind, image: &str) -> CreateVmRequest {
        let mut r = req(backend, None, None);
        r.vcpus = vcpus;
        r.memory_mib = memory_mib;
        r.disk_size_gib = disk_size_gib;
        r.ttl_seconds = ttl_seconds;
        r.image = image.into();
        r
    }

    #[test]
    fn empty_policy_allows_anything() {
        let cfg = Config::default();
        let r = req_with(64, 1_000_000, Some(9999), None, BackendKind::Qemu, "/anywhere/x.qcow2");
        assert!(validate_policy(&r, &cfg).is_ok());
    }

    #[test]
    fn policy_rejects_over_vcpu_limit() {
        let mut cfg = Config::default();
        cfg.policy.max_vcpus = Some(4);
        let r = req_with(8, 512, None, None, BackendKind::Qemu, "/x.qcow2");
        assert!(validate_policy(&r, &cfg).is_err());
    }

    #[test]
    fn policy_rejects_over_memory_limit() {
        let mut cfg = Config::default();
        cfg.policy.max_memory_mib = Some(2048);
        let r = req_with(1, 4096, None, None, BackendKind::Qemu, "/x.qcow2");
        assert!(validate_policy(&r, &cfg).is_err());
    }

    #[test]
    fn policy_rejects_over_disk_limit_but_allows_unset_disk() {
        let mut cfg = Config::default();
        cfg.policy.max_disk_gib = Some(50);
        let over = req_with(1, 512, Some(100), None, BackendKind::Qemu, "/x.qcow2");
        assert!(validate_policy(&over, &cfg).is_err());
        let unset = req_with(1, 512, None, None, BackendKind::Qemu, "/x.qcow2");
        assert!(validate_policy(&unset, &cfg).is_ok());
    }

    #[test]
    fn policy_with_ttl_cap_rejects_both_unbounded_and_over_cap() {
        let mut cfg = Config::default();
        cfg.policy.max_ttl_seconds = Some(3600);
        let unbounded = req_with(1, 512, None, None, BackendKind::Qemu, "/x.qcow2");
        assert!(validate_policy(&unbounded, &cfg).is_err());
        let too_long = req_with(1, 512, None, Some(7200), BackendKind::Qemu, "/x.qcow2");
        assert!(validate_policy(&too_long, &cfg).is_err());
        let ok = req_with(1, 512, None, Some(1800), BackendKind::Qemu, "/x.qcow2");
        assert!(validate_policy(&ok, &cfg).is_ok());
    }

    #[test]
    fn policy_restricts_allowed_backends() {
        let mut cfg = Config::default();
        cfg.policy.allowed_backends = Some(vec![BackendKind::Firecracker]);
        let qemu = req_with(1, 512, None, None, BackendKind::Qemu, "/x.qcow2");
        assert!(validate_policy(&qemu, &cfg).is_err());
        let fc = req_with(1, 512, None, None, BackendKind::Firecracker, "/x.qcow2");
        assert!(validate_policy(&fc, &cfg).is_ok());
    }

    #[test]
    fn policy_restricts_allowed_image_dirs() {
        let mut cfg = Config::default();
        cfg.policy.allowed_image_dirs = Some(vec!["/var/lib/ephemera/images".into()]);
        let outside = req_with(1, 512, None, None, BackendKind::Qemu, "/tmp/evil.qcow2");
        assert!(validate_policy(&outside, &cfg).is_err());
        let inside = req_with(1, 512, None, None, BackendKind::Qemu, "/var/lib/ephemera/images/base.qcow2");
        assert!(validate_policy(&inside, &cfg).is_ok());
    }
}
