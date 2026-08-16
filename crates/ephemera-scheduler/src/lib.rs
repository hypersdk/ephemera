// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Context, Result};
use chrono::{Duration, Utc};
use ephemera_core::{
    backend::{LaunchContext, VmBackend},
    config::Config,
    model::{BackendKind, ClaimOverrides, CloudInitSpec, CreateVmRequest, PoolRecord, PoolSpec, StorageBackend, VmRecord, VmStatus},
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

/// How long a VM may sit in `Creating` status before `reconcile()` assumes
/// its creating process crashed and reclaims it. Generous on purpose —
/// nothing in a normal `create()` should take anywhere near this long, even
/// under load (the slowest real path observed, a non-reflinkable Firecracker
/// raw-disk clone, took under a minute) — the cost of guessing wrong is a
/// legitimately-slow create getting cut off, so this errs long.
const STUCK_CREATING_GRACE: Duration = Duration::seconds(300);

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
    /// Serializes catalog.json read-modify-write cycles — add/remove/
    /// rename/clone all load the whole file, mutate, and write it back, so
    /// two concurrent calls need to not interleave.
    catalog_lock: AsyncMutex<()>,
}

impl VmManager {
    pub fn new(cfg: Config) -> Result<Arc<Self>> {
        cfg.ensure_dirs()?;
        let store = Arc::new(Store::load(&cfg.state_dir)?);
        let pools = Arc::new(PoolStore::load(&cfg.state_dir)?);
        // Best-effort: delegating cgroup controllers needs to write under
        // /sys/fs/cgroup, which isn't available in every environment this
        // constructor runs in (e.g. an unprivileged `cargo test`) — a
        // failure here shouldn't block VmManager from doing everything
        // else, only resource-control/metrics for VMs it launches.
        if let Err(e) = ephemera_cgroup::ensure_delegation() {
            tracing::warn!(error = %e, "failed to delegate cgroup controllers — resource control/metrics will be unavailable");
        }
        Ok(Arc::new(Self {
            cfg,
            store,
            pools,
            backfill_locks: AsyncMutex::new(HashMap::new()),
            catalog_lock: AsyncMutex::new(()),
        }))
    }

    /// Register a new image catalog entry — see `ephemera_image::catalog::add_entry`.
    pub async fn add_catalog_entry(&self, name: String, source: String, format: String) -> Result<ephemera_image::catalog::CatalogEntry> {
        let _guard = self.catalog_lock.lock().await;
        ephemera_image::catalog::add_entry(&self.cfg, name, source, format).await
    }

    /// Remove an image catalog entry — see `ephemera_image::catalog::remove_entry`.
    pub async fn remove_catalog_entry(&self, name: &str) -> Result<()> {
        let _guard = self.catalog_lock.lock().await;
        ephemera_image::catalog::remove_entry(&self.cfg, name)
    }

    /// Rename an image catalog entry — see `ephemera_image::catalog::rename_entry`.
    pub async fn rename_catalog_entry(&self, name: &str, new_name: &str) -> Result<ephemera_image::catalog::CatalogEntry> {
        let _guard = self.catalog_lock.lock().await;
        ephemera_image::catalog::rename_entry(&self.cfg, name, new_name)
    }

    /// Clone an image catalog entry under a new name — see `ephemera_image::catalog::clone_entry`.
    pub async fn clone_catalog_entry(&self, name: &str, target_name: &str) -> Result<ephemera_image::catalog::CatalogEntry> {
        let _guard = self.catalog_lock.lock().await;
        ephemera_image::catalog::clone_entry(&self.cfg, name, target_name)
    }

    /// Export a catalog entry's resolved file to `dest` — see `ephemera_image::catalog::export_entry`.
    pub async fn export_catalog_entry(&self, name: &str, dest: &std::path::Path) -> Result<()> {
        // No write to catalog.json here, but still serialized against
        // add/remove/rename/clone so a concurrent rename can't yank the
        // entry out from under an in-flight export's lookup.
        let _guard = self.catalog_lock.lock().await;
        ephemera_image::catalog::export_entry(&self.cfg, name, dest).await
    }

    /// Toggle a catalog entry's read-only flag — see `ephemera_image::catalog::set_read_only`.
    pub async fn set_catalog_read_only(&self, name: &str, read_only: bool) -> Result<ephemera_image::catalog::CatalogEntry> {
        let _guard = self.catalog_lock.lock().await;
        ephemera_image::catalog::set_read_only(&self.cfg, name, read_only)
    }

    /// Remove orphaned cached downloads — see `ephemera_image::catalog::clean_downloads`.
    pub async fn clean_catalog_downloads(&self) -> Result<Vec<String>> {
        let _guard = self.catalog_lock.lock().await;
        ephemera_image::catalog::clean_downloads(&self.cfg)
    }

    /// Create the VM's cgroup and migrate `pid` into it, storing the
    /// resulting path on `record`. Best-effort and non-fatal: a VM whose
    /// cgroup setup fails still runs — it just can't be resource-controlled
    /// or have its metrics read later (`set_resources`/`freeze`/`metrics`/
    /// `pressure` all report a clear "no cgroup" error rather than a
    /// confusing failure deeper in the cgroupfs).
    fn attach_cgroup(id: Uuid, pid: u32, record: &mut VmRecord) {
        match ephemera_cgroup::CgroupManager::create_and_migrate(&id.to_string(), pid) {
            Ok(mgr) => record.cgroup_path = Some(mgr.path().to_path_buf()),
            Err(e) => tracing::warn!(vm = %id, error = %e, "failed to create cgroup for VM — resource control/metrics unavailable for it"),
        }
    }

    /// `req.cloud_init` with a mount runcmd appended per `req.shared_folders`
    /// entry — writing a real `/etc/fstab` line (not just a one-shot `mount`
    /// command) so the share keeps working across a later stop/start, since
    /// cloud-init's own `runcmd` module only replays on a *new* instance-id,
    /// not a relaunch of the same VM (see `VmManager::start`'s doc comment).
    /// `Some` even when the caller passed no `cloud_init` at all, as long as
    /// there's at least one share to mount — otherwise the share would be
    /// attached to the guest but never actually reachable inside it.
    fn effective_cloud_init(req: &CreateVmRequest) -> Option<CloudInitSpec> {
        if req.shared_folders.is_empty() {
            return req.cloud_init.clone();
        }
        let mut ci = req.cloud_init.clone().unwrap_or_default();
        for (i, share) in req.shared_folders.iter().enumerate() {
            let tag = format!("fs{i}");
            let path = &share.guest_path;
            ci.runcmd.push(format!("mkdir -p {path}"));
            ci.runcmd.push(format!("grep -qF ' {path} ' /etc/fstab || echo '{tag} {path} virtiofs defaults 0 0' >> /etc/fstab"));
            ci.runcmd.push(format!("mount {path}"));
        }
        Some(ci)
    }

    fn cgroup_manager(&self, vm: &VmRecord) -> Result<ephemera_cgroup::CgroupManager> {
        let path = vm.cgroup_path.clone().with_context(|| {
            format!("VM {} has no cgroup (not running, or cgroup setup failed at launch)", vm.id)
        })?;
        Ok(ephemera_cgroup::CgroupManager::from_path(path)?)
    }

    /// Apply a partial set of cgroup v2 resource-control settings — only
    /// the fields set in `patch` are touched.
    pub async fn set_resources(&self, id: Uuid, patch: ephemera_core::model::ResourcePatch) -> Result<()> {
        let vm = self.get(id).await?;
        let mgr = self.cgroup_manager(&vm)?;
        if let Some(percent) = patch.cpu_quota_percent {
            mgr.cpu().set_max(&ephemera_cgroup::CpuMax::from_percent(percent as u64))?;
        }
        if let Some(bytes) = patch.memory_max_bytes {
            mgr.memory().set_max(bytes)?;
        }
        if let Some(weight) = patch.io_weight {
            mgr.io().set_weight(weight as u64)?;
        }
        if let Some(max) = patch.pids_max {
            mgr.pids().set_max(max)?;
        }
        if let Some(cpus) = &patch.cpuset_cpus {
            mgr.cpuset().set_cpus(cpus)?;
        }
        Ok(())
    }

    /// The cpuset currently pinned via `set_resources`'s `cpuset_cpus`, or
    /// empty if never set (cgroup default: unrestricted).
    pub async fn get_cpuset(&self, id: Uuid) -> Result<Vec<u32>> {
        let vm = self.get(id).await?;
        Ok(self.cgroup_manager(&vm)?.cpuset().get_cpus()?)
    }

    pub async fn freeze(&self, id: Uuid) -> Result<()> {
        let vm = self.get(id).await?;
        Ok(self.cgroup_manager(&vm)?.freezer().freeze()?)
    }

    pub async fn thaw(&self, id: Uuid) -> Result<()> {
        let vm = self.get(id).await?;
        Ok(self.cgroup_manager(&vm)?.freezer().thaw()?)
    }

    pub async fn is_frozen(&self, id: Uuid) -> Result<bool> {
        let vm = self.get(id).await?;
        Ok(self.cgroup_manager(&vm)?.freezer().is_frozen()?)
    }

    /// Point-in-time CPU/memory/disk usage, read from the VM's cgroup.
    pub async fn metrics(&self, id: Uuid) -> Result<ephemera_core::model::VmMetrics> {
        let vm = self.get(id).await?;
        let mgr = self.cgroup_manager(&vm)?;

        let cpu_stat = mgr.cpu().get_stat()?;
        let num_cpus = mgr.cpuset().get_cpus_effective().ok().filter(|c| !c.is_empty()).map(|c| c.len() as u64).unwrap_or_else(|| {
            std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1)
        });
        let uptime_secs = std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| s.split_whitespace().next().map(str::to_string))
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(1.0);
        let total_usec = (uptime_secs * 1_000_000.0) as u64 * num_cpus;
        let cpu_usage_percent =
            if total_usec == 0 { 0.0 } else { (cpu_stat.usage_usec as f64 / total_usec as f64 * 100.0).clamp(0.0, 100.0 * num_cpus as f64) };

        let memory_usage_bytes = mgr.memory().get_current()?;

        let (disk_read_bytes, disk_write_bytes) = mgr.io().get_stat().map(|stats| {
            stats.iter().fold((0u64, 0u64), |(r, w), s| (r + s.rbytes, w + s.wbytes))
        }).unwrap_or((0, 0));

        Ok(ephemera_core::model::VmMetrics { cpu_usage_percent, memory_usage_bytes, disk_read_bytes, disk_write_bytes })
    }

    /// PSI pressure stats for the VM's cgroup.
    pub async fn pressure(&self, id: Uuid) -> Result<ephemera_core::model::VmPressure> {
        let vm = self.get(id).await?;
        let mgr = self.cgroup_manager(&vm)?;
        let cpu = mgr.cpu().get_pressure().ok();
        let mem = mgr.memory().get_pressure().ok();
        let io = mgr.io().get_pressure().ok();
        Ok(ephemera_core::model::VmPressure {
            cpu_some: cpu.map(|p| p.some),
            memory_some: mem.as_ref().map(|p| p.some.clone()),
            memory_full: mem.and_then(|p| p.full),
            io_some: io.as_ref().map(|p| p.some.clone()),
            io_full: io.and_then(|p| p.full),
        })
    }

    pub async fn create(self: &Arc<Self>, mut req: CreateVmRequest) -> Result<VmRecord> {
        // Resolve BackendKind::Auto before anything else — everything below
        // (the disk filename, the persisted record, the launch dispatch)
        // assumes a concrete backend and must never see Auto.
        req.backend = resolve_backend(&req, &self.cfg);
        // Resolved before policy so allowed_image_dirs governs the actual
        // downloaded/verified file a catalog alias points to, not the
        // alias string itself.
        req.image = ephemera_image::catalog::resolve(&self.cfg, &req.image).await.context("resolving image from catalog")?;
        validate_policy(&req, &self.cfg)?;
        // Every storage backend except CephRbd points `image` at a real
        // filesystem entry (a file for Default/Nbd, a block device for
        // LvmThin) — CephRbd's `image` is a `pool/image` reference with no
        // local path to check at all.
        if req.storage != StorageBackend::CephRbd && !req.image.exists() {
            bail!("base image does not exist: {}", req.image.display());
        }
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
            jail_path: None,
            vsock_socket: None,
            cgroup_path: None,
            netns: None,
            lvm_lv: None,
            nbd_pid: None,
            virtiofsd_pids: Vec::new(),
        };
        // Deciding the CID and reserving it happen as one atomic, locked
        // operation in the store — see ephemera-storage::Store::insert_with_cid
        // for why a separate "list, then insert" pair isn't safe across
        // concurrent `ephemera` processes.
        let mut record = self.store.insert_with_cid(placeholder, needs_cid, FIRST_GUEST_CID).await?;
        let guest_cid = record.guest_cid;

        let result: Result<()> = async {
            let agent_token = req.agent.as_ref().and_then(|a| a.token.as_deref());
            let provisioned = ephemera_image::storage::provision(
                &self.cfg, &req.image, req.backend, req.storage, &workspace, &disk, req.disk_size_gib, id, agent_token,
            ).await.context("provisioning VM disk")?;
            record.disk = provisioned.disk.clone();
            record.lvm_lv = provisioned.lvm_lv.clone();
            record.nbd_pid = provisioned.nbd_pid;
            let effective_cloud_init = Self::effective_cloud_init(&req);
            let seed = match &effective_cloud_init {
                Some(ci) => Some(ephemera_image::cloudinit::build_seed(&self.cfg, &workspace, ci).await?),
                None => None,
            };
            let network = ephemera_network::prepare(&self.cfg, id, &req.network).await?;
            record.tap_name = network.tap_name.clone();
            record.netns = network.netns.clone();
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
                disk: record.disk.clone(),
                seed_disk: seed,
                log_path: log_path.clone(),
                network,
                guest_cid,
                vsock_socket,
                disk_format: ephemera_image::storage::disk_format(req.backend, req.storage),
                nbd_export: provisioned.nbd_export,
            };
            let launch = backend(req.backend)?.launch(&self.cfg, &req, &ctx).await?;
            record.pid = Some(launch.pid);
            record.control_socket = launch.control_socket;
            record.jail_path = launch.jail_path;
            record.vsock_socket = launch.vsock_socket;
            record.virtiofsd_pids = launch.virtiofsd_pids;
            Self::attach_cgroup(id, launch.pid, &mut record);
            record.status = VmStatus::Running;
            Ok(())
        }.await;

        if let Err(e) = result {
            if let Some(tap) = &record.tap_name { let _ = ephemera_network::cleanup(id, &req.network, tap, record.netns.as_deref()).await; }
            // A later step (network prep, launch) can fail after the disk
            // was already provisioned — don't leak the LV/qemu-nbd process.
            if let Some(lv) = &record.lvm_lv { let _ = ephemera_image::storage::cleanup_lvm_lv(lv).await; }
            if let Some(pid) = record.nbd_pid { let _ = ephemera_image::storage::cleanup_nbd(pid).await; }
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

    /// Relaunch a `Stopped` VM from its existing disk/seed — unlike
    /// `create`, this skips image cloning, guest-agent token injection, and
    /// cloud-init seed generation, since all of that already happened the
    /// first time this record was created and is still sitting on disk.
    /// Only network device prep is redone (the tap/macvtap was torn down on
    /// stop). Added for consumers keyed by a name/register-then-start model
    /// (zyvor-fabric's `driver-core::VMDriver::start`) that need to resume a
    /// VM without repeating create-time work.
    pub async fn start(self: &Arc<Self>, id: Uuid) -> Result<VmRecord> {
        let mut vm = self.get(id).await?;
        if vm.status == VmStatus::Running {
            return Ok(vm);
        }
        // A CephRbd disk is a `rbd:pool/image:...` URI, not a real
        // filesystem path — there's nothing on the local filesystem to
        // check `exists()` against. LvmThin (a block device) and Nbd (the
        // local qcow2 file the export serves) both really do live on disk.
        if vm.request.storage != StorageBackend::CephRbd && !vm.disk.exists() {
            bail!("cannot start {id}: disk no longer exists at {}", vm.disk.display());
        }

        let result: Result<()> = async {
            let network = ephemera_network::prepare(&self.cfg, id, &vm.request.network).await?;
            vm.tap_name = network.tap_name.clone();
            vm.netns = network.netns.clone();

            let vsock_socket = match (vm.guest_cid, vm.backend) {
                (Some(_), BackendKind::Qemu) => None,
                (Some(_), _) => Some(vm.workspace.join("vsock.sock")),
                (None, _) => None,
            };
            // `StorageBackend::Nbd`'s qemu-nbd export is left running across
            // stop/start (see `VmRecord::nbd_pid`), so its socket is already
            // there to reattach to — nothing to reprovision.
            let nbd_export = (vm.request.storage == StorageBackend::Nbd).then(|| vm.workspace.join("nbd.sock"));

            let ctx = LaunchContext {
                id,
                workspace: vm.workspace.clone(),
                disk: vm.disk.clone(),
                seed_disk: vm.seed_disk.clone(),
                log_path: vm.log_path.clone(),
                network,
                guest_cid: vm.guest_cid,
                vsock_socket,
                disk_format: ephemera_image::storage::disk_format(vm.backend, vm.request.storage),
                nbd_export,
            };
            let launch = backend(vm.backend)?.launch(&self.cfg, &vm.request, &ctx).await?;
            vm.pid = Some(launch.pid);
            vm.control_socket = launch.control_socket;
            vm.jail_path = launch.jail_path;
            vm.vsock_socket = launch.vsock_socket;
            vm.virtiofsd_pids = launch.virtiofsd_pids;
            Self::attach_cgroup(id, launch.pid, &mut vm);
            vm.status = VmStatus::Running;
            vm.error = None;
            Ok(())
        }
        .await;

        if let Err(e) = result {
            if let Some(tap) = &vm.tap_name {
                let _ = ephemera_network::cleanup(id, &vm.request.network, tap, vm.netns.as_deref()).await;
            }
            vm.status = VmStatus::Failed;
            vm.error = Some(format!("{e:#}"));
            self.store.update(vm.clone()).await?;
            return Err(e);
        }
        self.store.update(vm.clone()).await?;
        Ok(vm)
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
        if let Some(tap) = &vm.tap_name { let _ = ephemera_network::cleanup(id, &vm.request.network, tap, vm.netns.as_deref()).await; }
        vm.netns = None;
        // virtiofsd instances aren't reattachable the way qemu-nbd's export
        // is (see the Nbd comment in `start`) — a fresh set gets spawned on
        // the next `start`, so tear these down unconditionally here.
        for pid in vm.virtiofsd_pids.drain(..) {
            if process::process_alive(pid).await {
                let _ = process::terminate_pid(pid).await;
            }
        }
        // cgroup v2 requires a cgroup to be empty (no PIDs left in
        // cgroup.procs) before rmdir succeeds — safe here since the process
        // is confirmed dead by this point either way (graceful exit,
        // terminate_pid, or it just never had one to begin with).
        if let Some(cgroup_path) = vm.cgroup_path.take() {
            if let Ok(mgr) = ephemera_cgroup::CgroupManager::from_path(cgroup_path) {
                if let Err(e) = mgr.remove() {
                    tracing::warn!(vm = %id, error = %e, "failed to remove VM cgroup");
                }
            }
        }
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

    /// Write a file into the guest over the vsock agent — see
    /// `AgentRequest::PutFile`.
    pub async fn put_file(&self, id: Uuid, path: String, content_base64: String, mode: Option<u32>) -> Result<ephemera_guest_protocol::AgentResponse> {
        let vm = self.get(id).await?;
        ephemera_vsock_client::call(&vm, AgentRequest::PutFile { path, content_base64, mode }, ephemera_vsock_client::DEFAULT_CALL_TIMEOUT).await
    }

    /// Read a file from the guest over the vsock agent — see
    /// `AgentRequest::GetFile`.
    pub async fn get_file(&self, id: Uuid, path: String) -> Result<ephemera_guest_protocol::AgentResponse> {
        let vm = self.get(id).await?;
        ephemera_vsock_client::call(&vm, AgentRequest::GetFile { path }, ephemera_vsock_client::DEFAULT_CALL_TIMEOUT).await
    }

    /// Open an interactive shell on the guest over the vsock agent — see
    /// `ephemera_vsock_client::open_shell`. The returned stream is raw PTY
    /// traffic once the handshake completes; callers relay it themselves
    /// (e.g. `ephemera-api`'s WebSocket console endpoint).
    pub async fn open_console(&self, id: Uuid, cols: u16, rows: u16) -> Result<ephemera_vsock_client::ConsoleStream> {
        let vm = self.get(id).await?;
        ephemera_vsock_client::open_shell(&vm, cols, rows, ephemera_vsock_client::DEFAULT_CALL_TIMEOUT).await
    }

    pub async fn delete(&self, id: Uuid) -> Result<()> {
        let mut vm = self.get(id).await?;
        // A Paused VM is not "already stopped" — its process is fully
        // alive with vCPUs suspended (real bug found on real hardware: a
        // warm pool's never-claimed, still-Paused members were getting
        // their workspace/disk deleted out from under a live QEMU process,
        // because this only checked for Running, leaking an orphaned
        // process on every `delete_pool` and every pool-member cleanup).
        // `pid.is_some()` is the right test for "there's a process to kill"
        // regardless of which of those two statuses got it there.
        if vm.pid.is_some() {
            vm = self.stop(id).await.context("stopping VM before delete")?;
        }
        // Defense in depth: `stop()` already falls back from graceful
        // shutdown to a waited SIGTERM/SIGKILL, so this should never fire —
        // but if it somehow does, refuse to reclaim the disk out from under
        // a process that's still actually running, rather than silently
        // deleting it and leaking an untracked orphan (the original shape
        // of the bug above, one layer deeper).
        if let Some(pid) = vm.pid {
            if process::process_alive(pid).await {
                bail!("refusing to delete {id}: pid {pid} is still alive after stop");
            }
        }
        let vm = self.store.remove(id).await?.context("VM vanished")?;
        // These three point at live state outside `workspace` (a
        // still-active LV, a still-running qemu-nbd process, a Ceph clone)
        // that only `delete` ever reclaims — `stop` deliberately leaves them
        // alone, same as it leaves the disk file itself alone, so a stopped
        // VM can be `start`ed again without reprovisioning storage.
        if let Some(lv) = &vm.lvm_lv {
            if let Err(e) = ephemera_image::storage::cleanup_lvm_lv(lv).await {
                tracing::warn!(vm = %id, error = %e, "failed to remove LVM thin snapshot");
            }
        }
        if let Some(pid) = vm.nbd_pid {
            if let Err(e) = ephemera_image::storage::cleanup_nbd(pid).await {
                tracing::warn!(vm = %id, error = %e, "failed to stop qemu-nbd export");
            }
        }
        if vm.request.storage == StorageBackend::CephRbd {
            if let Some(pool_image) = ephemera_image::storage::ceph_rbd_ref(&vm.disk) {
                if let Err(e) = ephemera_image::storage::cleanup_ceph_rbd(&self.cfg, &pool_image).await {
                    tracing::warn!(vm = %id, error = %e, "failed to remove Ceph RBD clone (unverified backend)");
                }
            }
        }
        if vm.workspace.exists() { fs::remove_dir_all(vm.workspace)?; }
        // Firecracker-jailer VMs place resources under a separate chroot
        // tree (`cfg.jailer.chroot_base_dir`, not `state_dir`) that
        // `workspace` above never covers.
        if let Some(jail_path) = &vm.jail_path {
            if jail_path.exists() { fs::remove_dir_all(jail_path)?; }
        }
        Ok(())
    }

    pub async fn reconcile(&self) -> Result<()> {
        let now = Utc::now();
        for vm in self.store.list().await {
            if vm.status == VmStatus::Running {
                if let Some(pid) = vm.pid {
                    if !process::process_alive(pid).await {
                        let mut vm = vm;
                        vm.status = VmStatus::Stopped;
                        vm.pid = None;
                        if let Some(tap) = &vm.tap_name { let _ = ephemera_network::cleanup(vm.id, &vm.request.network, tap, vm.netns.as_deref()).await; }
                        vm.netns = None;
                        if let Some(cgroup_path) = vm.cgroup_path.take() {
                            if let Ok(mgr) = ephemera_cgroup::CgroupManager::from_path(cgroup_path) {
                                let _ = mgr.remove();
                            }
                        }
                        self.store.update(vm).await?;
                    }
                }
            } else if vm.status == VmStatus::Creating && now - vm.created_at > STUCK_CREATING_GRACE {
                // `create()` sets this placeholder's status to Running or
                // Failed as its very last step — a record still Creating
                // this long after `created_at` means the process running
                // that `create()` call was killed or crashed mid-flight
                // (real trigger: `ephemera serve` killed while a pool
                // backfill's `create()` was in progress) before it could
                // reach either outcome. Nothing else will ever finish or
                // clean up this placeholder, so reclaim it here rather than
                // leave permanent litter in the store.
                tracing::warn!(vm=%vm.id, "cleaning up a VM stuck in Creating status — its creating process likely crashed");
                let _ = self.delete(vm.id).await;
            }
        }
        Ok(())
    }

    /// Runs `reconcile()`, TTL cleanup, and pool backfill on every tick.
    /// Pool backfill in particular *needs* this: `create_pool`/
    /// `claim_from_pool` also fire off a best-effort background top-up via
    /// `tokio::spawn`, but that only actually finishes if the calling
    /// process stays alive long enough — true for a request handled inside
    /// this `serve` process, but NOT true for a one-shot `ephemera pool
    /// create`/`claim` CLI invocation, which exits (killing every task it
    /// spawned, finished or not) right after printing its result. This tick
    /// is the backstop that keeps every pool topped up regardless of which
    /// process's claim/create under-filled it — the same reason TTL cleanup
    /// lives here rather than in `delete`'s caller.
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
                for pool in me.pools.list().await {
                    if pool.members.len() < pool.size {
                        me.spawn_backfill(pool.name);
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

    /// Blocking variant of the backfill that `create_pool`/`claim_from_pool`
    /// otherwise only fire off in the background: waits for `name`'s pool
    /// to actually reach its target size before returning. Meant for
    /// one-shot callers — the CLI's `ephemera pool create` uses this so the
    /// pool is genuinely ready by the time that (short-lived) process
    /// exits, without depending on a separately-running `ephemera serve`
    /// daemon's reaper tick to finish the job later. A REST caller inside
    /// `serve` has no need for this — the process outlives the background
    /// task either way.
    pub async fn backfill_pool_sync(self: &Arc<Self>, name: &str) -> Result<()> {
        self.backfill_pool(name).await
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

            // `create()` returns as soon as the VMM process is spawned, long
            // before the guest OS has finished booting — pausing right here
            // would freeze it mid-boot, before its guest-agent has even
            // started. Confirmed on real hardware: a member paused this
            // early comes back from `claim`'s resume still mid-boot, and
            // `exec` fails (connection reset/timeout) for as long as the
            // boot has left to run — the exact opposite of what a *warm*
            // pool is for. Wait for the agent to actually answer a ping
            // first, so a paused member is a genuinely finished, ready VM.
            if vm.request.agent.as_ref().is_some_and(|a| a.enabled) {
                if let Err(e) = wait_for_agent_ready(&vm).await {
                    let _ = self.delete(vm.id).await;
                    return Err(e).context("waiting for new pool member's guest agent to become ready");
                }
            }

            let paused = match self.pause(vm.id).await {
                Ok(paused) => paused,
                Err(e) => {
                    // The VM was created successfully but never made it into
                    // any pool's members — clean it up rather than abandon
                    // an untracked, unpaused VM outside all pool accounting
                    // (e.g. `launch` can report success even though the
                    // spawned VMM process crashes moments later for its own
                    // reasons, which then makes `pause`'s QMP connect fail).
                    let _ = self.delete(vm.id).await;
                    return Err(e).context("pausing new pool member");
                }
            };
            if !self.pools.push_member(name, paused.id).await? {
                // Pool was deleted while this member was being created.
                let _ = self.delete(paused.id).await;
                return Ok(());
            }
        }
    }
}

/// How long a pool backfill will wait for a freshly-created member's guest
/// agent to answer a ping before giving up. Generous on purpose — the
/// slowest real boot observed this session (a whole-disk-extracted
/// Firecracker rootfs hitting systemd's local-fs.target timeout) took
/// ~140s; ordinary QEMU boots are much faster, but this errs long rather
/// than abandon a legitimately-slow-but-fine boot.
const POOL_MEMBER_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Polls the guest agent with `Ping` until it answers or
/// `POOL_MEMBER_READY_TIMEOUT` elapses. See `backfill_pool`'s call site for
/// why this matters: pausing a pool member before its agent is reachable
/// freezes it mid-boot, which is not "ready," just "created."
async fn wait_for_agent_ready(vm: &VmRecord) -> Result<()> {
    let deadline = tokio::time::Instant::now() + POOL_MEMBER_READY_TIMEOUT;
    loop {
        if ephemera_vsock_client::ping(vm, std::time::Duration::from_secs(5)).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("guest agent on {} never became reachable within {POOL_MEMBER_READY_TIMEOUT:?}", vm.id);
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
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
            storage: StorageBackend::Default,
            shared_folders: vec![],
        }
    }

    #[test]
    fn effective_cloud_init_is_unchanged_with_no_shares() {
        let r = req(BackendKind::Qemu, None, None);
        assert!(VmManager::effective_cloud_init(&r).is_none());
    }

    #[test]
    fn effective_cloud_init_synthesizes_mount_commands_with_no_prior_cloud_init() {
        let mut r = req(BackendKind::Qemu, None, None);
        r.shared_folders = vec![ephemera_core::model::SharedFolder {
            host_path: "/srv/data".into(),
            guest_path: "/mnt/data".into(),
            read_only: false,
        }];
        let ci = VmManager::effective_cloud_init(&r).unwrap();
        let joined = ci.runcmd.join("\n");
        assert!(joined.contains("mkdir -p /mnt/data"));
        assert!(joined.contains("fs0 /mnt/data virtiofs defaults 0 0"));
        assert!(joined.contains("mount /mnt/data"));
    }

    #[test]
    fn effective_cloud_init_appends_to_an_existing_cloud_init() {
        let mut r = req(BackendKind::Qemu, None, None);
        r.cloud_init = Some(CloudInitSpec { runcmd: vec!["echo hi".to_string()], ..Default::default() });
        r.shared_folders = vec![ephemera_core::model::SharedFolder {
            host_path: "/srv/data".into(),
            guest_path: "/mnt/data".into(),
            read_only: true,
        }];
        let ci = VmManager::effective_cloud_init(&r).unwrap();
        assert_eq!(ci.runcmd[0], "echo hi");
        assert!(ci.runcmd.iter().any(|c| c.contains("/mnt/data")));
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
