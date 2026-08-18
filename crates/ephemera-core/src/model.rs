// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    Qemu,
    CloudHypervisor,
    Firecracker,
    /// Resolved to a concrete backend by `ephemera_scheduler::resolve_backend`
    /// as the very first step of `VmManager::create` — never persisted, and
    /// every other function taking a `BackendKind` (the backend dispatcher,
    /// image cloning, ...) assumes it never sees this variant.
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "mode")]
pub enum NetworkSpec {
    None,
    User {
        #[serde(default)]
        forwards: Vec<PortForward>,
    },
    Tap {
        #[serde(default)]
        tap_name: Option<String>,
        #[serde(default)]
        bridge: Option<String>,
        #[serde(default)]
        mac: Option<String>,
        /// Give this VM its own network namespace (veth pair + NAT to the
        /// host, an internal bridge joining the veth and the tap inside the
        /// namespace) instead of putting its tap directly on the shared
        /// host bridge named by `bridge` (which is ignored when this is
        /// true). Real isolation: the VM's own routing table, iptables, and
        /// interface list are separate from the host's and from every other
        /// namespaced VM's — not just a shared L2 segment. The VMM process
        /// itself is launched inside the namespace (`ip netns exec`) so it
        /// can see the tap at all.
        #[serde(default)]
        netns: bool,
    },
    /// A macvtap device on `parent`, giving the VM its own MAC directly on
    /// that link with no host bridge involved. Supported by the QEMU and
    /// Cloud Hypervisor backends only (attached via a pre-opened file
    /// descriptor); Firecracker has no fd-based tap attachment in its API.
    Macvtap {
        parent: String,
        /// macvtap link mode: bridge (default) | vepa | private | passthru
        #[serde(default)]
        macvtap_mode: Option<String>,
        #[serde(default)]
        mac: Option<String>,
    },
}

impl Default for NetworkSpec {
    fn default() -> Self { Self::User { forwards: vec![] } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortForward {
    pub host_port: u16,
    pub guest_port: u16,
    #[serde(default = "default_tcp")]
    pub protocol: String,
}
fn default_tcp() -> String { "tcp".into() }

/// Where and how a VM's writable disk is actually provisioned, independent
/// of which VMM backend boots it. `Default` (the empty/unset request field)
/// keeps today's per-VMM behavior unchanged: a qcow2 copy-on-write overlay
/// for QEMU, a reflinked-or-copied raw file for Cloud Hypervisor/Firecracker.
/// See `ephemera_image::storage` for how each variant is actually
/// provisioned and torn down.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum StorageBackend {
    #[default]
    Default,
    /// `request.image` must be the path to an existing LVM thin logical
    /// volume block device (e.g. `/dev/vg0/base-image`), itself backed by a
    /// thin pool. A new thin snapshot LV is created per VM and handed to
    /// the VMM directly as a raw block device: near-instant, real
    /// copy-on-write at the block layer, no filesystem/reflink involved.
    /// Not supported under the Firecracker jailer — its chroot/hardlink
    /// resource-placement model doesn't extend to shared block devices; use
    /// direct (non-jailed) Firecracker, QEMU, or Cloud Hypervisor instead.
    LvmThin,
    /// QEMU only. The per-VM disk is a normal qcow2 CoW overlay, exported
    /// over NBD via a `qemu-nbd` subprocess this VM owns (over a UNIX
    /// socket, not a TCP port) instead of being opened directly as a local
    /// file by QEMU — the same client/server split real remote/shared NBD
    /// storage uses, without requiring a separate storage host to exist in
    /// order to prove the mechanism end to end.
    Nbd,
    /// Ceph RBD. `request.image` is a `pool/image` reference to an existing
    /// RBD image; a snapshot on it named `ephemera-base` must already exist
    /// and be protected (`rbd snap protect`) for the per-VM clone to work.
    /// Verified end to end against a real Rook Ceph cluster: `rbd clone`
    /// produces a genuine per-VM thin clone, and QEMU boots a real guest to
    /// a login prompt straight off the `rbd:` URI — see
    /// `ephemera_image::storage::provision_ceph_rbd`. Does not support
    /// automatic guest-agent token injection (see that function).
    CephRbd,
}

/// Enables the in-guest vsock agent (ping/exec/shutdown, no SSH needed).
/// `port` is the AF_VSOCK port the guest listens on, not a host TCP port.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_agent_port")]
    pub port: u32,
    /// Shared secret the guest agent requires on every request. If left
    /// unset on a request with `enabled: true`, `VmManager::create`
    /// generates a random one and burns it into that VM's own disk before
    /// boot (see `ephemera_image::inject_guest_agent_token`) — every
    /// agent-enabled VM ends up authenticated by default, without the
    /// caller having to think about it.
    #[serde(default)]
    pub token: Option<String>,
}
fn default_agent_port() -> u32 { 17777 }

impl Default for AgentSpec {
    fn default() -> Self { Self { enabled: false, port: default_agent_port(), token: None } }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CloudInitSpec {
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub ssh_authorized_keys: Vec<String>,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub runcmd: Vec<String>,
    /// Configure the guest's network address statically via cloud-init
    /// (network-config v2) instead of leaving it to the guest's own DHCP
    /// client. Only meaningful for `NetworkSpec::Tap { netns: true, .. }`
    /// -- that's the only mode with a known address to inject before boot
    /// (see `ephemera_network::netns::NetnsHandle`). Ignored (no-op) for
    /// every other networking mode. The address is the same one DHCP mode
    /// would hand out anyway (both are pinned to the same reservation) --
    /// this just skips depending on the guest actually running a working
    /// DHCP client, which not every image does out of the box.
    #[serde(default)]
    pub static_network: bool,
    /// Files to write into the guest before first boot, via cloud-init's
    /// own `write_files` module -- e.g. dropping a systemd unit or app
    /// config without needing a custom image build.
    #[serde(default)]
    pub write_files: Vec<CloudInitFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudInitFile {
    pub path: String,
    pub content: String,
    /// Octal file mode, e.g. `"0644"`. Defaults to cloud-init's own default
    /// (`0644`) when unset.
    #[serde(default)]
    pub permissions: Option<String>,
}

/// A host directory shared into the guest via virtiofs, declared at create
/// time — there's no live "mount this now" equivalent for a real hardware
/// VM the way `machinectl bind` had for nspawn's shared-kernel containers
/// (see the systemd-removal migration plan's bind-mount notes). Requires
/// `virtiofsd` on the host `$PATH`; only supported by the QEMU backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedFolder {
    pub host_path: PathBuf,
    /// Where to mount it inside the guest. Auto-mounted via a generated
    /// cloud-init `runcmd` entry when `cloud_init` is set on the request
    /// (see `VmManager::create`); otherwise the guest must run
    /// `mount -t virtiofs <tag> <guest_path>` itself, where `<tag>` is
    /// this share's index in `shared_folders` (`"fs0"`, `"fs1"`, ...).
    pub guest_path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateVmRequest {
    pub name: String,
    pub backend: BackendKind,
    pub image: PathBuf,
    #[serde(default = "default_vcpus")]
    pub vcpus: u8,
    #[serde(default = "default_memory")]
    pub memory_mib: u64,
    #[serde(default)]
    pub disk_size_gib: Option<u64>,
    #[serde(default)]
    pub kernel: Option<PathBuf>,
    #[serde(default)]
    pub initrd: Option<PathBuf>,
    #[serde(default)]
    pub firmware: Option<PathBuf>,
    #[serde(default)]
    pub kernel_args: Option<String>,
    #[serde(default)]
    pub network: NetworkSpec,
    #[serde(default)]
    pub cloud_init: Option<CloudInitSpec>,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub agent: Option<AgentSpec>,
    #[serde(default)]
    pub storage: StorageBackend,
    #[serde(default)]
    pub shared_folders: Vec<SharedFolder>,
}
fn default_vcpus() -> u8 { 2 }
fn default_memory() -> u64 { 2048 }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VmStatus {
    Creating,
    Running,
    Paused,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmRecord {
    pub id: Uuid,
    pub name: String,
    pub backend: BackendKind,
    pub status: VmStatus,
    pub pid: Option<u32>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub workspace: PathBuf,
    pub disk: PathBuf,
    pub seed_disk: Option<PathBuf>,
    pub tap_name: Option<String>,
    pub control_socket: Option<PathBuf>,
    pub log_path: PathBuf,
    pub error: Option<String>,
    pub request: CreateVmRequest,
    /// Host-unique AF_VSOCK CID assigned when `request.agent` is enabled.
    #[serde(default)]
    pub guest_cid: Option<u32>,
    /// Firecracker-jailer only — see `backend::LaunchResult::jail_path`.
    #[serde(default)]
    pub jail_path: Option<PathBuf>,
    /// Host-visible path to the vsock proxy UDS (Cloud Hypervisor/
    /// Firecracker only) — see `backend::LaunchResult::vsock_socket`.
    #[serde(default)]
    pub vsock_socket: Option<PathBuf>,
    /// cgroup v2 path (`ephemera.slice/{id}.scope`) the launched VMM
    /// process was migrated into, once `VmManager` has done so —
    /// `None` until the first successful launch completes cgroup setup.
    #[serde(default)]
    pub cgroup_path: Option<PathBuf>,
    /// Name of this VM's private network namespace, when
    /// `NetworkSpec::Tap { netns: true, .. }` — see
    /// `ephemera_network::netns`. `None` for every other networking mode.
    #[serde(default)]
    pub netns: Option<String>,
    /// `StorageBackend::LvmThin` only: the thin snapshot LV device path
    /// (`/dev/<vg>/eph-<id>`) created for this VM, so `VmManager::delete`
    /// can `lvremove` it regardless of what `request.storage` says by the
    /// time the VM is deleted.
    #[serde(default)]
    pub lvm_lv: Option<PathBuf>,
    /// `StorageBackend::Nbd` only: pid of the `qemu-nbd` subprocess serving
    /// this VM's disk over `<workspace>/nbd.sock`, kept alive across
    /// stop/start (like the disk file itself) and reaped only on delete.
    #[serde(default)]
    pub nbd_pid: Option<u32>,
    /// PIDs of the `virtiofsd` processes backing `request.shared_folders`,
    /// one per share (same order), kept alive alongside the VM and killed
    /// on delete/stop by `VmManager` — see `LaunchResult::virtiofsd_pids`.
    #[serde(default)]
    pub virtiofsd_pids: Vec<u32>,
    /// Path to this VM's per-namespace dnsmasq lease file -- only set for
    /// `NetworkSpec::Tap { netns: true, .. }` VMs. `VmManager::get`/`list`
    /// use it to freshly resolve `guest_ip` on every read rather than
    /// trusting a value that could go stale as leases renew.
    #[serde(default)]
    pub dhcp_leasefile: Option<std::path::PathBuf>,
    /// The guest's IP address on its own private subnet, learned from
    /// `dhcp_leasefile` by MAC lookup -- only set for `NetworkSpec::Tap {
    /// netns: true, .. }` VMs (see `ephemera_network::netns`). `None` until
    /// the guest actually completes a DHCP handshake, and for every other
    /// networking mode. Recomputed on every read (see `dhcp_leasefile`
    /// above), not authoritative between reads.
    #[serde(default)]
    pub guest_ip: Option<String>,
}

/// A named template for a warm pool: `size` VMs matching `template` are
/// kept pre-booted-and-`Paused`, ready to be handed out by
/// `VmManager::claim_from_pool` in roughly resume-time (already fast — see
/// "Pause, resume, and exec") instead of full create-time. `template.name`
/// and `template.ttl_seconds` are ignored for pool members (a paused pool
/// member must never expire on its own; the claimed VM gets a fresh name/TTL
/// at claim time — see `ClaimOverrides`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolSpec {
    pub name: String,
    pub size: usize,
    pub template: CreateVmRequest,
}

/// Persisted state of a warm pool: which VM ids are currently reserved
/// members (booted, paused, unclaimed). A member id present here always
/// corresponds to a real `Paused` `VmRecord` in the main VM store — the two
/// are kept in sync by `VmManager`, not merged into one store, since pool
/// membership and VM lifecycle are different concerns with different
/// locking needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolRecord {
    pub name: String,
    pub size: usize,
    pub template: CreateVmRequest,
    #[serde(default)]
    pub members: Vec<Uuid>,
}

/// Applied to the VM handed back by a pool claim, replacing whatever the
/// template said for these two fields (a pool member is paused with no name
/// worth keeping and no TTL, precisely so it never expires while idle in
/// the pool).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClaimOverrides {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

/// cgroup v2 resource-control settings to apply to a running VM. Every
/// field is optional so a caller only touches what it actually wants to
/// change — see `VmManager::set_resources`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResourcePatch {
    /// CPU quota as a percentage of one core (200 = 2 full cores).
    #[serde(default)]
    pub cpu_quota_percent: Option<u32>,
    /// Memory limit in bytes.
    #[serde(default)]
    pub memory_max_bytes: Option<u64>,
    /// I/O weight (1-10000, default 100).
    #[serde(default)]
    pub io_weight: Option<u32>,
    /// Maximum number of PIDs in the VM's cgroup.
    #[serde(default)]
    pub pids_max: Option<u64>,
    /// Pin the VM to these host CPU cores.
    #[serde(default)]
    pub cpuset_cpus: Option<Vec<u32>>,
}

/// Point-in-time resource usage for a VM, read from its cgroup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmMetrics {
    /// CPU usage as a percentage of one core, averaged over the process's
    /// entire lifetime (not an instantaneous rate).
    pub cpu_usage_percent: f64,
    pub memory_usage_bytes: u64,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
}

/// PSI (Pressure Stall Information) for a VM's cgroup — see
/// `ephemera_cgroup::PressureStats` for field semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmPressure {
    pub cpu_some: Option<ephemera_cgroup::PressureRecord>,
    pub memory_some: Option<ephemera_cgroup::PressureRecord>,
    pub memory_full: Option<ephemera_cgroup::PressureRecord>,
    pub io_some: Option<ephemera_cgroup::PressureRecord>,
    pub io_full: Option<ephemera_cgroup::PressureRecord>,
}
