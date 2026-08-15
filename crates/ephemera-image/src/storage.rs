// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! Pluggable disk-provisioning backends — see `ephemera_core::model::StorageBackend`
//! for what each variant means and its known limitations. `StorageBackend::Default`
//! delegates straight to `crate::clone_for_vm`, unchanged from before this module
//! existed; the other three are implemented here.

use crate::clone_for_vm;
use anyhow::{bail, Context, Result};
use ephemera_core::{
    config::Config,
    model::{BackendKind, StorageBackend},
    process::{run_checked, terminate_pid},
};
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn short_id(id: Uuid) -> String { id.simple().to_string()[..8].to_string() }

/// Result of provisioning a VM's disk: what to hand the VMM, plus whatever
/// live state (an LV, a subprocess) `VmManager::delete` needs to tear down
/// later. `disk`/`lvm_lv`/`nbd_pid` map straight onto the matching
/// `VmRecord` fields; `nbd_export` is consumed immediately by the caller to
/// build `LaunchContext` and isn't itself persisted (the path is
/// deterministic — `workspace.join("nbd.sock")` — so it's rederived instead
/// of stored twice).
pub struct ProvisionedDisk {
    pub disk: PathBuf,
    pub lvm_lv: Option<PathBuf>,
    pub nbd_export: Option<PathBuf>,
    pub nbd_pid: Option<u32>,
}

/// Block format the VMM should open `disk` as. QEMU's `StorageBackend::Default`
/// overlay is qcow2; everything else (raw block devices, rbd: URIs, NBD
/// exports, and Cloud Hypervisor/Firecracker's always-raw disks) is raw.
pub fn disk_format(vmm: BackendKind, storage: StorageBackend) -> String {
    match (storage, vmm) {
        (StorageBackend::Default, BackendKind::Qemu) => "qcow2".into(),
        _ => "raw".into(),
    }
}

/// `agent_token`, when set, is injected into the disk (via
/// `crate::inject_guest_agent_token`) as the last step of provisioning,
/// before anything else gets a chance to open/lock/export it — critical for
/// `StorageBackend::Nbd`: injecting *after* `qemu-nbd` starts exporting the
/// file races that export's own write lock (found on real hardware — see
/// `provision_nbd`).
#[allow(clippy::too_many_arguments)]
pub async fn provision(
    cfg: &Config,
    base: &Path,
    vmm: BackendKind,
    storage: StorageBackend,
    workspace: &Path,
    disk_out: &Path,
    size_gib: Option<u64>,
    id: Uuid,
    agent_token: Option<&str>,
) -> Result<ProvisionedDisk> {
    match storage {
        StorageBackend::Default => {
            clone_for_vm(cfg, base, vmm, disk_out, size_gib).await?;
            if let Some(token) = agent_token {
                crate::inject_guest_agent_token(disk_out, token).await?;
            }
            Ok(ProvisionedDisk { disk: disk_out.to_path_buf(), lvm_lv: None, nbd_export: None, nbd_pid: None })
        }
        StorageBackend::LvmThin => provision_lvm_thin(cfg, base, vmm, id, agent_token).await,
        StorageBackend::Nbd => provision_nbd(cfg, base, vmm, workspace, disk_out, agent_token).await,
        StorageBackend::CephRbd => provision_ceph_rbd(cfg, base, vmm, id, agent_token).await,
    }
}

async fn provision_lvm_thin(cfg: &Config, base: &Path, vmm: BackendKind, id: Uuid, agent_token: Option<&str>) -> Result<ProvisionedDisk> {
    if vmm == BackendKind::Firecracker && cfg.jailer.enabled {
        bail!("storage=lvm-thin is not supported under the Firecracker jailer (its chroot/hardlink resource placement doesn't extend to shared block devices) — use direct Firecracker, QEMU, or Cloud Hypervisor");
    }
    let vg = base.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str())
        .with_context(|| format!("storage=lvm-thin requires image to be a /dev/<vg>/<lv> path to an existing thin LV, got {}", base.display()))?;
    let base_lv = base.file_name().and_then(|s| s.to_str())
        .with_context(|| format!("storage=lvm-thin requires image to be a /dev/<vg>/<lv> path to an existing thin LV, got {}", base.display()))?;
    let snap = format!("eph-{}", short_id(id));

    run_checked("lvcreate", &[
        "--snapshot".into(), "--name".into(), snap.clone(),
        // LVM sets a persistent "activation skip" flag on every new thin
        // snapshot by default (a safety guard against accidentally
        // mounting a filesystem with a duplicate UUID alongside its
        // origin). Found on real hardware: without this, the immediately
        // following `lvchange -ay` exits 0 but silently does nothing — no
        // dm device is ever created, and the VM later fails to boot with
        // "device does not exist". `--setactivationskip n` creates the
        // snapshot without that flag in the first place, so plain `-ay`
        // actually activates it.
        "--setactivationskip".into(), "n".into(),
        format!("{vg}/{base_lv}"),
    ]).await.with_context(|| format!("creating LVM thin snapshot {vg}/{snap} from {vg}/{base_lv}"))?;

    if let Err(e) = run_checked("lvchange", &["-ay".into(), format!("{vg}/{snap}")]).await {
        let _ = run_checked("lvremove", &["-f".into(), format!("{vg}/{snap}")]).await;
        return Err(e).with_context(|| format!("activating LVM thin snapshot {vg}/{snap}"));
    }

    let dev = PathBuf::from(format!("/dev/{vg}/{snap}"));
    // `lvchange -ay` returns as soon as the kernel dm target is activated —
    // it does NOT wait for udev to finish processing the resulting uevent
    // and create the human-friendly `/dev/<vg>/<lv>` symlink (only
    // `/dev/mapper/<vg>-<lv>` and `/dev/dm-N` are guaranteed to exist the
    // instant this call returns). Found on real hardware: guestkit's
    // `add_drive` on this path failed with "Image file does not exist"
    // moments after a successful `lvchange -ay`, because it ran before udev
    // had caught up. Poll for the symlink rather than trust the command's
    // exit status alone.
    if let Err(e) = wait_for_device_node(&dev).await {
        let _ = run_checked("lvchange", &["-an".into(), format!("{vg}/{snap}")]).await;
        let _ = run_checked("lvremove", &["-f".into(), format!("{vg}/{snap}")]).await;
        return Err(e);
    }
    if let Some(token) = agent_token {
        if let Err(e) = crate::inject_guest_agent_token(&dev, token).await {
            let _ = run_checked("lvchange", &["-an".into(), format!("{vg}/{snap}")]).await;
            let _ = run_checked("lvremove", &["-f".into(), format!("{vg}/{snap}")]).await;
            return Err(e);
        }
    }
    Ok(ProvisionedDisk { disk: dev.clone(), lvm_lv: Some(dev), nbd_export: None, nbd_pid: None })
}

async fn wait_for_device_node(dev: &Path) -> Result<()> {
    for _ in 0..50 {
        if dev.exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    bail!("LVM activated {} but its device node never appeared within 5s (udev lag?)", dev.display())
}

/// Torn down only by `VmManager::delete` (see `VmRecord::lvm_lv`) — a
/// stopped VM keeps its snapshot LV, exactly like a `Default`-storage VM
/// keeps its qcow2/raw file across stop/start.
pub async fn cleanup_lvm_lv(lv: &Path) -> Result<()> {
    let _ = run_checked("lvchange", &["-an".into(), lv.display().to_string()]).await;
    run_checked("lvremove", &["-f".into(), lv.display().to_string()]).await
        .with_context(|| format!("removing LVM thin snapshot {}", lv.display()))
}

async fn provision_nbd(cfg: &Config, base: &Path, vmm: BackendKind, workspace: &Path, disk_out: &Path, agent_token: Option<&str>) -> Result<ProvisionedDisk> {
    if vmm != BackendKind::Qemu {
        bail!("storage=nbd is only supported under the QEMU backend (QEMU has a native nbd: block client; Cloud Hypervisor and Firecracker have no equivalent)");
    }
    // Same disposable qcow2 overlay as StorageBackend::Default under QEMU —
    // it's just served over NBD instead of opened directly.
    clone_for_vm(cfg, base, vmm, disk_out, None).await?;
    // Inject the guest-agent token BEFORE qemu-nbd starts exporting the
    // file, not after. Found on real hardware: `qemu-nbd --persistent`
    // takes an exclusive write lock on the image for as long as it's
    // running; guestkit's own (separate) qemu-nbd-based mount used by
    // `inject_guest_agent_token` then fails with "Failed to get 'write'
    // lock" / "NBD device did not become ready" if it runs after this
    // export is already up.
    if let Some(token) = agent_token {
        crate::inject_guest_agent_token(disk_out, token).await?;
    }

    let socket = workspace.join("nbd.sock");
    let pid_file = workspace.join("nbd.pid");
    run_checked("qemu-nbd", &[
        "--fork".into(), "--persistent".into(), "--format=qcow2".into(),
        format!("--socket={}", socket.display()),
        format!("--pid-file={}", pid_file.display()),
        disk_out.display().to_string(),
    ]).await.context("starting qemu-nbd export")?;

    let pid = read_pid_file_retrying(&pid_file).await?;
    Ok(ProvisionedDisk { disk: disk_out.to_path_buf(), lvm_lv: None, nbd_export: Some(socket), nbd_pid: Some(pid) })
}

async fn read_pid_file_retrying(path: &Path) -> Result<u32> {
    for _ in 0..30 {
        if let Ok(s) = tokio::fs::read_to_string(path).await {
            if let Ok(pid) = s.trim().parse::<u32>() {
                return Ok(pid);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    bail!("qemu-nbd did not write a pid file at {} within 3s", path.display())
}

/// Torn down only by `VmManager::delete` (see `VmRecord::nbd_pid`) — the
/// `qemu-nbd` export stays up across stop/start, same reasoning as the LVM
/// snapshot above.
pub async fn cleanup_nbd(pid: u32) -> Result<()> {
    terminate_pid(pid).await
}

/// **Unverified**: implemented against the real `rbd` CLI and QEMU's native
/// `rbd:` block driver, but never run against an actual Ceph cluster — none
/// was available in this project's test environment. See
/// `model::StorageBackend::CephRbd`.
async fn provision_ceph_rbd(cfg: &Config, base: &Path, vmm: BackendKind, id: Uuid, agent_token: Option<&str>) -> Result<ProvisionedDisk> {
    if vmm != BackendKind::Qemu {
        bail!("storage=ceph-rbd is only implemented for the QEMU backend (QEMU has a native rbd: block driver when built against librbd; Cloud Hypervisor and Firecracker have no built-in equivalent) — and even for QEMU, this path has not been verified against a real Ceph cluster");
    }
    if agent_token.is_some() {
        bail!("storage=ceph-rbd does not support automatic guest-agent token injection (guestkit needs a local file or block device, not a rbd: URI, and this combination has never been exercised) — bake the token into the base RBD image yourself, or use storage=default/lvm-thin/nbd with an agent-enabled VM");
    }
    let base_ref = base.to_str().context("ceph-rbd image reference must be valid UTF-8 'pool/image'")?;
    let (pool, image) = base_ref.split_once('/')
        .with_context(|| format!("storage=ceph-rbd requires image to be a 'pool/image' reference, got {base_ref}"))?;
    let clone_name = format!("eph-{}", short_id(id));

    let mut clone_args = vec!["clone".to_string(), format!("{pool}/{image}@ephemera-base"), format!("{pool}/{clone_name}")];
    clone_args.push(format!("--id={}", cfg.storage.ceph_user));
    if let Some(conf) = &cfg.storage.ceph_conf {
        clone_args.push(format!("--conf={}", conf.display()));
    }
    run_checked("rbd", &clone_args).await.with_context(|| {
        format!("cloning Ceph RBD image {pool}/{image}@ephemera-base -> {pool}/{clone_name} (NOTE: storage=ceph-rbd has not been verified against a real Ceph cluster in this project)")
    })?;

    let mut uri = format!("rbd:{pool}/{clone_name}");
    uri.push_str(&format!(":id={}", cfg.storage.ceph_user));
    if let Some(conf) = &cfg.storage.ceph_conf {
        uri.push_str(&format!(":conf={}", conf.display()));
    }
    Ok(ProvisionedDisk { disk: PathBuf::from(uri), lvm_lv: None, nbd_export: None, nbd_pid: None })
}

/// **Unverified**, same caveat as `provision_ceph_rbd`. `pool_image` is
/// `"pool/clone-name"`, extracted from the `rbd:` URI stored on `VmRecord.disk`.
pub async fn cleanup_ceph_rbd(cfg: &Config, pool_image: &str) -> Result<()> {
    let mut args = vec!["rm".to_string(), pool_image.to_string()];
    args.push(format!("--id={}", cfg.storage.ceph_user));
    if let Some(conf) = &cfg.storage.ceph_conf {
        args.push(format!("--conf={}", conf.display()));
    }
    run_checked("rbd", &args).await.with_context(|| format!("removing Ceph RBD clone {pool_image}"))
}

/// Extracts `"pool/clone-name"` back out of a `rbd:pool/clone-name:id=...`
/// URI, for `VmManager::delete` to pass to `cleanup_ceph_rbd`.
pub fn ceph_rbd_ref(disk: &Path) -> Option<String> {
    let s = disk.to_str()?.strip_prefix("rbd:")?;
    Some(s.split(':').next().unwrap_or(s).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_format_is_qcow2_only_for_default_storage_under_qemu() {
        assert_eq!(disk_format(BackendKind::Qemu, StorageBackend::Default), "qcow2");
        assert_eq!(disk_format(BackendKind::CloudHypervisor, StorageBackend::Default), "raw");
        assert_eq!(disk_format(BackendKind::Firecracker, StorageBackend::Default), "raw");
        assert_eq!(disk_format(BackendKind::Qemu, StorageBackend::LvmThin), "raw");
        assert_eq!(disk_format(BackendKind::Qemu, StorageBackend::Nbd), "raw");
        assert_eq!(disk_format(BackendKind::Qemu, StorageBackend::CephRbd), "raw");
    }

    #[test]
    fn ceph_rbd_ref_extracts_pool_and_clone_name_from_the_uri() {
        let uri = PathBuf::from("rbd:mypool/eph-abcd1234:id=admin:conf=/etc/ceph/ceph.conf");
        assert_eq!(ceph_rbd_ref(&uri).as_deref(), Some("mypool/eph-abcd1234"));
    }

    #[test]
    fn ceph_rbd_ref_returns_none_for_a_non_rbd_path() {
        assert_eq!(ceph_rbd_ref(&PathBuf::from("/var/lib/ephemera/instances/x/root.raw")), None);
    }
}
