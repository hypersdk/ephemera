// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! Every `ephemera create`/`stop`/`pause`/... invocation is a fresh, one-shot
//! CLI process (not just concurrent tasks inside one `serve` daemon), so an
//! in-memory cache populated once at startup is not enough to stay correct:
//! two such processes racing would each load a stale view, each write back
//! only their own change, and the loser's write — or, more subtly, a value
//! like a vsock CID allocation that depended on seeing the other's write —
//! would silently vanish. Every operation here instead takes an OS-level
//! `flock` on a dedicated lock file and re-reads `vms.json` fresh under that
//! lock before mutating and writing it back, so state is coordinated across
//! processes, not just within one.

use anyhow::{bail, Context, Result};
use ephemera_core::model::VmRecord;
use std::{collections::HashMap, fs, os::unix::io::AsRawFd, path::{Path, PathBuf}};
use uuid::Uuid;

pub struct Store {
    path: PathBuf,
    lock_path: PathBuf,
}

impl Store {
    pub fn load(state_dir: &Path) -> Result<Self> {
        Ok(Self {
            path: state_dir.join("vms.json"),
            lock_path: state_dir.join("vms.lock"),
        })
    }

    fn read_map(path: &Path) -> Result<HashMap<Uuid, VmRecord>> {
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let raw = fs::read_to_string(path).context("reading VM state")?;
        if raw.trim().is_empty() {
            return Ok(HashMap::new());
        }
        serde_json::from_str(&raw).context("parsing VM state")
    }

    /// Runs `f` against a freshly-read map while holding an exclusive lock,
    /// then persists whatever `f` left the map as.
    async fn with_exclusive<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut HashMap<Uuid, VmRecord>) -> T + Send + 'static,
        T: Send + 'static,
    {
        let path = self.path.clone();
        let lock_path = self.lock_path.clone();
        tokio::task::spawn_blocking(move || -> Result<T> {
            let lock_file = fs::OpenOptions::new().create(true).write(true).open(&lock_path)
                .context("opening store lock file")?;
            if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                bail!("locking store: {}", std::io::Error::last_os_error());
            }
            let mut map = Self::read_map(&path)?;
            let result = f(&mut map);
            let tmp = path.with_extension("json.tmp");
            fs::write(&tmp, serde_json::to_vec_pretty(&map)?).context("writing VM state")?;
            fs::rename(&tmp, &path).context("renaming VM state")?;
            Ok(result)
        })
        .await
        .context("store worker thread panicked")?
    }

    /// Runs `f` against a freshly-read map while holding a shared (read)
    /// lock — coordinates with concurrent `with_exclusive` writers without
    /// blocking other concurrent readers against each other.
    async fn with_shared<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&HashMap<Uuid, VmRecord>) -> T + Send + 'static,
        T: Send + 'static,
    {
        let path = self.path.clone();
        let lock_path = self.lock_path.clone();
        tokio::task::spawn_blocking(move || -> Result<T> {
            let lock_file = fs::OpenOptions::new().create(true).write(true).open(&lock_path)
                .context("opening store lock file")?;
            if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_SH) } != 0 {
                bail!("locking store: {}", std::io::Error::last_os_error());
            }
            let map = Self::read_map(&path)?;
            Ok(f(&map))
        })
        .await
        .context("store worker thread panicked")?
    }

    pub async fn insert(&self, vm: VmRecord) -> Result<()> {
        self.with_exclusive(move |m| {
            m.insert(vm.id, vm);
        })
        .await
    }

    /// Inserts `record`, first assigning it the lowest vsock CID `>= first_cid`
    /// not already used by another stored VM, when `needs_cid` is set — done
    /// under the *same* exclusive lock as the insert itself. Deciding the CID
    /// via `list()` and inserting via a separate `insert()` call would still
    /// race across two concurrent processes (both could read the same "free"
    /// CID before either persists); folding both into one locked operation
    /// closes that window.
    pub async fn insert_with_cid(&self, mut record: VmRecord, needs_cid: bool, first_cid: u32) -> Result<VmRecord> {
        self.with_exclusive(move |m| {
            if needs_cid {
                let used: std::collections::HashSet<u32> = m.values().filter_map(|v| v.guest_cid).collect();
                let mut candidate = first_cid;
                while used.contains(&candidate) {
                    candidate += 1;
                }
                record.guest_cid = Some(candidate);
            }
            m.insert(record.id, record.clone());
            record
        })
        .await
    }

    pub async fn update(&self, vm: VmRecord) -> Result<()> {
        self.insert(vm).await
    }

    pub async fn get(&self, id: Uuid) -> Option<VmRecord> {
        self.with_shared(move |m| m.get(&id).cloned()).await.ok().flatten()
    }

    pub async fn list(&self) -> Vec<VmRecord> {
        self.with_shared(|m| {
            let mut v: Vec<_> = m.values().cloned().collect();
            v.sort_by_key(|r| r.created_at);
            v
        })
        .await
        .unwrap_or_default()
    }

    pub async fn remove(&self, id: Uuid) -> Result<Option<VmRecord>> {
        self.with_exclusive(move |m| m.remove(&id)).await
    }
}
