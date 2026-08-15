// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! cgroup v2 resource control and metrics for Ephemera-managed VMs.
//!
//! Vendored from zyvor-fabric's `zyvor-fabric-cgroup` crate (see the
//! systemd-removal migration plan, Phase 5) rather than shared via a
//! cross-repo dependency — the logic is small (~1600 lines), has no
//! external dependencies beyond serde/thiserror/tracing, and cgroup v2's
//! kernel ABI is stable, so a vendored copy is simpler than the
//! cross-repo-crate machinery a shared dependency would need. The one real
//! difference from the zyvor-fabric original: that crate only ever *reads*
//! an already-existing cgroup (systemd-machined creates
//! `machine-{name}.scope` itself); Ephemera has no systemd-machined
//! equivalent, so `CgroupManager::create_and_migrate` below creates the
//! cgroup and moves the launched PID into it directly.
//!
//! # Usage
//!
//! ```no_run
//! use ephemera_cgroup::CgroupManager;
//!
//! ephemera_cgroup::ensure_delegation().unwrap();
//! let mgr = CgroupManager::create_and_migrate("some-vm-id", 12345).unwrap();
//! let mem_usage = mgr.memory().get_current().unwrap();
//! ```

mod error;
mod util;

pub mod cpu;
pub mod cpuset;
pub mod freezer;
pub mod io;
pub mod memory;
pub mod pids;
pub mod pressure;

pub use cpu::{CpuController, CpuMax, CpuStat};
pub use cpuset::{format_set, parse_set, CpusetController};
pub use error::{CgroupError, Result};
pub use freezer::FreezerController;
pub use io::{DeviceId, IoController, IoMax, IoStat};
pub use memory::{MemoryController, MemoryEvents, MemoryStats};
pub use pids::PidsController;
pub use pressure::{PressureRecord, PressureStats};

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const CGROUP2_ROOT: &str = "/sys/fs/cgroup";
const EPHEMERA_SLICE: &str = "/sys/fs/cgroup/ephemera.slice";

/// Controllers every VM cgroup needs delegated down from the cgroup2 root —
/// covers every `ResourceControlDriver`/`ResourceStatsDriver` operation
/// this crate exposes (cpu.max/weight, memory.max/min/low/high, io.max/
/// weight, pids.max, cpuset.cpus, the freezer, and PSI pressure files,
/// which don't need their own delegation but live alongside the others).
const DELEGATED_CONTROLLERS: &[&str] = &["cpu", "memory", "io", "pids", "cpuset"];

/// Resolved cgroup path.
#[derive(Debug, Clone)]
pub struct CgroupPath(PathBuf);

impl CgroupPath {
    /// Create a cgroup path for a VM: `ephemera.slice/{id}.scope`.
    pub fn for_vm(id: &str) -> Self {
        let path = PathBuf::from(EPHEMERA_SLICE).join(format!("{id}.scope"));
        Self(path)
    }

    /// Create a cgroup path from an arbitrary path.
    pub fn from_path(path: PathBuf) -> Self {
        Self(path)
    }

    /// Check whether the cgroup directory exists.
    pub fn exists(&self) -> bool {
        self.0.is_dir()
    }

    /// Get the underlying path.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Cgroup-level events from cgroup.events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CgroupEvents {
    pub populated: bool,
    pub frozen: bool,
}

/// Enable `DELEGATED_CONTROLLERS` in `cgroup.subtree_control` at the cgroup2
/// root and at `ephemera.slice`, so VM-scope cgroups created underneath can
/// use them. A cgroup can only use a controller if every ancestor has
/// enabled it for its children — on a systemd host this delegation happens
/// automatically for systemd-managed slices, but nothing does it for
/// `ephemera.slice` since it isn't systemd-managed, so Ephemera has to do
/// it itself. Idempotent (writing an already-enabled controller is a
/// harmless no-op) — call this once at daemon startup, or lazily before
/// the first VM launch.
pub fn ensure_delegation() -> Result<()> {
    std::fs::create_dir_all(EPHEMERA_SLICE).map_err(|e| CgroupError::WriteFailed {
        path: PathBuf::from(EPHEMERA_SLICE),
        source: e,
    })?;

    for root in [CGROUP2_ROOT, EPHEMERA_SLICE] {
        let subtree_control = PathBuf::from(root).join("cgroup.subtree_control");
        let available = util::read_cgroup_file(&PathBuf::from(root).join("cgroup.controllers"))
            .unwrap_or_default();
        for controller in DELEGATED_CONTROLLERS {
            if !available.split_whitespace().any(|c| c == *controller) {
                continue; // not compiled into this kernel — nothing to enable
            }
            // Enabling a controller that's already enabled is a harmless
            // no-op; enabling one that isn't available in the parent errors,
            // which the `available` check above already filters out.
            if let Err(e) = util::write_cgroup_file(&subtree_control, &format!("+{controller}")) {
                tracing::warn!(root, controller, error = %e, "failed to delegate cgroup controller");
            }
        }
    }
    Ok(())
}

/// Entry point for cgroup v2 management of one VM.
///
/// Provides access to all controllers for a single cgroup.
pub struct CgroupManager {
    path: CgroupPath,
}

impl CgroupManager {
    /// Open a manager for an already-created VM cgroup (validates it exists).
    pub fn for_vm(id: &str) -> Result<Self> {
        let path = CgroupPath::for_vm(id);
        if !path.exists() {
            return Err(CgroupError::NotFound(path.0));
        }
        Ok(Self { path })
    }

    /// Create a manager from an arbitrary path (validates path exists).
    pub fn from_path(path: PathBuf) -> Result<Self> {
        let cpath = CgroupPath::from_path(path);
        if !cpath.exists() {
            return Err(CgroupError::NotFound(cpath.0));
        }
        Ok(Self { path: cpath })
    }

    /// Create `ephemera.slice/{id}.scope` and move `pid` into it. Call this
    /// once, immediately after the VMM process is launched — moving a PID
    /// into a cgroup only moves that single process (not children it may
    /// fork later, which inherit their parent's cgroup automatically), so
    /// this should happen before the VMM has forked anything heavy.
    pub fn create_and_migrate(id: &str, pid: u32) -> Result<Self> {
        let path = CgroupPath::for_vm(id);
        std::fs::create_dir_all(&path.0).map_err(|e| CgroupError::WriteFailed {
            path: path.0.clone(),
            source: e,
        })?;
        util::write_cgroup_file(&path.0.join("cgroup.procs"), &pid.to_string())?;
        tracing::debug!(id, pid, path = %path.0.display(), "created VM cgroup and migrated pid");
        Ok(Self { path })
    }

    /// Remove the VM's cgroup. cgroup v2 requires the cgroup to already be
    /// empty (no PIDs left in cgroup.procs, i.e. the VMM process has
    /// already exited) — call this after confirming the process is gone.
    pub fn remove(&self) -> Result<()> {
        std::fs::remove_dir(&self.path.0).map_err(|e| CgroupError::WriteFailed {
            path: self.path.0.clone(),
            source: e,
        })?;
        tracing::debug!(path = %self.path.0.display(), "removed VM cgroup");
        Ok(())
    }

    /// Get the cgroup path.
    pub fn path(&self) -> &Path {
        self.path.path()
    }

    /// CPU controller.
    pub fn cpu(&self) -> CpuController {
        CpuController::new(self.path.0.clone())
    }

    /// Memory controller.
    pub fn memory(&self) -> MemoryController {
        MemoryController::new(self.path.0.clone())
    }

    /// I/O controller.
    pub fn io(&self) -> IoController {
        IoController::new(self.path.0.clone())
    }

    /// PIDs controller.
    pub fn pids(&self) -> PidsController {
        PidsController::new(self.path.0.clone())
    }

    /// Cpuset controller.
    pub fn cpuset(&self) -> CpusetController {
        CpusetController::new(self.path.0.clone())
    }

    /// Freezer controller.
    pub fn freezer(&self) -> FreezerController {
        FreezerController::new(self.path.0.clone())
    }

    /// Read cgroup.controllers to list available controllers.
    pub fn available_controllers(&self) -> Result<Vec<String>> {
        let file = self.path.0.join("cgroup.controllers");
        let content = util::read_cgroup_file(&file)?;
        Ok(content.split_whitespace().map(String::from).collect())
    }

    /// Read cgroup.events.
    pub fn events(&self) -> Result<CgroupEvents> {
        let file = self.path.0.join("cgroup.events");
        let content = util::read_cgroup_file(&file)?;
        let mut populated = false;
        let mut frozen = false;
        for line in content.lines() {
            let line = line.trim();
            if let Some(val) = line.strip_prefix("populated ") {
                populated = val.trim() == "1";
            } else if let Some(val) = line.strip_prefix("frozen ") {
                frozen = val.trim() == "1";
            }
        }
        Ok(CgroupEvents { populated, frozen })
    }
}
