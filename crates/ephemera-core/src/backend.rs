// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use crate::{config::Config, model::{BackendKind, CreateVmRequest}};
use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PreparedNetwork {
    pub spec: crate::model::NetworkSpec,
    pub tap_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LaunchContext {
    pub id: uuid::Uuid,
    pub workspace: PathBuf,
    pub disk: PathBuf,
    pub seed_disk: Option<PathBuf>,
    pub log_path: PathBuf,
    pub network: PreparedNetwork,
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
}

pub fn path_arg(p: &Path) -> String { p.display().to_string() }
