// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use crate::model::BackendKind;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::{Path, PathBuf}};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: String,
    pub state_dir: PathBuf,
    pub run_dir: PathBuf,
    pub qemu_binary: String,
    pub qemu_img_binary: String,
    pub cloud_hypervisor_binary: String,
    pub ch_remote_binary: String,
    pub cloud_localds_binary: String,
    pub virt_customize_binary: String,
    pub firecracker_binary: String,
    pub firecracker_kernel: Option<PathBuf>,
    pub cloud_hypervisor_firmware: Option<PathBuf>,
    pub default_bridge: Option<String>,
    pub reaper_interval_secs: u64,
    pub policy: Policy,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7788".into(),
            state_dir: "/var/lib/ephemera".into(),
            run_dir: "/run/ephemera".into(),
            qemu_binary: "qemu-system-x86_64".into(),
            qemu_img_binary: "qemu-img".into(),
            cloud_hypervisor_binary: "cloud-hypervisor".into(),
            ch_remote_binary: "ch-remote".into(),
            cloud_localds_binary: "cloud-localds".into(),
            virt_customize_binary: "virt-customize".into(),
            firecracker_binary: "firecracker".into(),
            firecracker_kernel: None,
            cloud_hypervisor_firmware: None,
            default_bridge: Some("vmbr0".into()),
            reaper_interval_secs: 5,
            policy: Policy::default(),
        }
    }
}

/// Admission limits enforced by `ephemera_scheduler::validate_policy` before
/// a VM is created. Every field defaults to unrestricted (`None`), so an
/// operator opts in to only the limits they want by setting them in
/// `[policy]` — an empty/absent `[policy]` table behaves exactly like the
/// pre-policy MVP.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Policy {
    pub max_vcpus: Option<u8>,
    pub max_memory_mib: Option<u64>,
    pub max_disk_gib: Option<u64>,
    /// If set, every request must specify a `ttl_seconds` at or below this
    /// value — an unbounded (`ttl_seconds: null`) VM is rejected too, since
    /// the whole point of a TTL cap is that nothing can run forever.
    pub max_ttl_seconds: Option<u64>,
    /// If set, only these backends may be used (checked against the
    /// already-resolved backend, so `"auto"` is checked as whatever it
    /// resolved to, not as `"auto"` itself).
    pub allowed_backends: Option<Vec<BackendKind>>,
    /// If set, the request's `image` must be underneath one of these
    /// directories (plain path-prefix check, not a symlink-resistant
    /// containment guarantee — sufficient to stop tenants pointing at
    /// arbitrary host paths, not a sandboxing boundary).
    pub allowed_image_dirs: Option<Vec<PathBuf>>,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else { return Ok(Self::default()); };
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.state_dir)?;
        fs::create_dir_all(&self.run_dir)?;
        Ok(())
    }
}
