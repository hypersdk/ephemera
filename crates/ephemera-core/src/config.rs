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
    pub auth: AuthConfig,
    pub jailer: JailerConfig,
    pub catalog: CatalogConfig,
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
            auth: AuthConfig::default(),
            jailer: JailerConfig::default(),
            catalog: CatalogConfig::default(),
        }
    }
}

/// Named, checksummed, optionally-signed base images — see
/// `ephemera_image::catalog`. `path: None` (the default) disables the
/// catalog entirely: `CreateVmRequest.image` is always treated as a literal
/// path/URL, exactly like before this existed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CatalogConfig {
    pub path: Option<PathBuf>,
    /// Base64-encoded Ed25519 public keys. Empty (the default) means
    /// catalog entries don't need to be signed at all. Non-empty means
    /// *every* catalog entry used to create a VM must carry a valid
    /// signature from one of these keys — there is no per-entry opt-out.
    pub trusted_signers: Vec<String>,
}

/// Firecracker-only: runs the VM through Firecracker's own `jailer` binary
/// (chroot, uid/gid drop, cgroups) instead of exec'ing `firecracker`
/// directly. `enabled: false` (the default) is a full no-op — every
/// Firecracker VM launches exactly as it did before this existed. QEMU and
/// Cloud Hypervisor have no jailer equivalent and ignore this entirely.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JailerConfig {
    pub enabled: bool,
    pub jailer_binary: String,
    /// The uid/gid `jailer` drops privileges to after chrooting — must be
    /// non-root and (for a real security boundary) not shared with any
    /// other tenant's jail. Defaults match the values commonly used in
    /// Firecracker's own getting-started docs; change them for anything
    /// beyond a single-tenant host.
    pub uid: u32,
    pub gid: u32,
    /// Base directory jailer creates `<exec-file-name>/<vm-id>/root/` under.
    /// Must be on the same filesystem as `state_dir` for the hardlink-based
    /// resource placement in `ephemera-firecracker` to avoid falling back
    /// to a full copy of the (potentially multi-GB) rootfs.
    pub chroot_base_dir: PathBuf,
}

impl Default for JailerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            jailer_binary: "jailer".into(),
            uid: 123,
            gid: 100,
            chroot_base_dir: "/srv/jailer".into(),
        }
    }
}

/// REST API bearer-token auth, enforced by `ephemera-api`'s auth middleware.
/// Empty `tokens` (the default) disables auth entirely — every request is
/// treated as [`Role::Admin`] — matching the pre-auth MVP behavior for
/// operators who haven't opted in. This is separate from, and unrelated to,
/// the per-VM vsock guest-agent token (`AgentSpec::token`): that one
/// protects the guest from other host processes; this one protects the
/// daemon's own HTTP surface from callers who shouldn't reach it at all.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AuthConfig {
    pub tokens: Vec<ApiToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToken {
    pub token: String,
    pub role: Role,
    #[serde(default)]
    pub name: Option<String>,
}

/// `Admin` can do anything (create/stop/pause/resume/exec/delete/build
/// images). `ReadOnly` can only list/get VMs — a valid token of either role
/// is enough to satisfy `/metrics`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Admin,
    ReadOnly,
}

/// Constant-time comparison so a mismatched API token can't be brute-forced
/// via response-time measurement.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
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
