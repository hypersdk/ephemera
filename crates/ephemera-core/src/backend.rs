// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use crate::{config::Config, model::{BackendKind, CreateVmRequest, VmRecord}};
use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PreparedNetwork {
    pub spec: crate::model::NetworkSpec,
    /// Name of the ephemeral network device to remove on stop (a TAP or a
    /// macvtap link, depending on `spec`).
    pub tap_name: Option<String>,
    /// For `NetworkSpec::Macvtap`: a pre-opened, exec-inheritable fd for the
    /// device's `/dev/tapN` character device, passed to the VMM as `fd=`.
    pub macvtap_fd: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct LaunchContext {
    pub id: uuid::Uuid,
    pub workspace: PathBuf,
    pub disk: PathBuf,
    pub seed_disk: Option<PathBuf>,
    pub log_path: PathBuf,
    pub network: PreparedNetwork,
    /// AF_VSOCK CID for the guest agent, when `request.agent.enabled`.
    pub guest_cid: Option<u32>,
    /// Path for the VMM's vsock proxy UDS (Cloud Hypervisor/Firecracker
    /// only — QEMU exposes a real kernel vsock device and needs no proxy).
    pub vsock_socket: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct LaunchResult {
    pub pid: u32,
    pub control_socket: Option<PathBuf>,
}

#[async_trait]
pub trait VmBackend: Send + Sync {
    fn kind(&self) -> BackendKind;
    async fn launch(&self, cfg: &Config, req: &CreateVmRequest, ctx: &LaunchContext) -> Result<LaunchResult>;
    async fn pause(&self, cfg: &Config, vm: &VmRecord) -> Result<()>;
    async fn resume(&self, cfg: &Config, vm: &VmRecord) -> Result<()>;
    async fn graceful_shutdown(&self, cfg: &Config, vm: &VmRecord) -> Result<()>;
}

pub fn path_arg(p: &Path) -> String { p.display().to_string() }
