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

/// Enables the in-guest vsock agent (ping/exec/shutdown, no SSH needed).
/// `port` is the AF_VSOCK port the guest listens on, not a host TCP port.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_agent_port")]
    pub port: u32,
}
fn default_agent_port() -> u32 { 17777 }

impl Default for AgentSpec {
    fn default() -> Self { Self { enabled: false, port: default_agent_port() } }
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
}
