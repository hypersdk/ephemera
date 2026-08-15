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
use ephemera_core::model::{PoolRecord, VmRecord};
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

/// Same flock-per-operation, read-fresh-under-lock discipline as [`Store`]
/// (see the module doc comment for why that matters across separate CLI
/// processes), applied to warm-pool records instead of VM records, keyed by
/// pool name and persisted in its own `pools.json`/`pools.lock` pair. Kept
/// as a separate small type rather than a generic `Store<K, V>` — the two
/// have different enough key types and call sites that a shared abstraction
/// would cost more in indirection than the ~40 lines it'd save.
pub struct PoolStore {
    path: PathBuf,
    lock_path: PathBuf,
}

impl PoolStore {
    pub fn load(state_dir: &Path) -> Result<Self> {
        Ok(Self {
            path: state_dir.join("pools.json"),
            lock_path: state_dir.join("pools.lock"),
        })
    }

    fn read_map(path: &Path) -> Result<HashMap<String, PoolRecord>> {
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let raw = fs::read_to_string(path).context("reading pool state")?;
        if raw.trim().is_empty() {
            return Ok(HashMap::new());
        }
        serde_json::from_str(&raw).context("parsing pool state")
    }

    async fn with_exclusive<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut HashMap<String, PoolRecord>) -> T + Send + 'static,
        T: Send + 'static,
    {
        let path = self.path.clone();
        let lock_path = self.lock_path.clone();
        tokio::task::spawn_blocking(move || -> Result<T> {
            let lock_file = fs::OpenOptions::new().create(true).write(true).open(&lock_path)
                .context("opening pool store lock file")?;
            if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                bail!("locking pool store: {}", std::io::Error::last_os_error());
            }
            let mut map = Self::read_map(&path)?;
            let result = f(&mut map);
            let tmp = path.with_extension("json.tmp");
            fs::write(&tmp, serde_json::to_vec_pretty(&map)?).context("writing pool state")?;
            fs::rename(&tmp, &path).context("renaming pool state")?;
            Ok(result)
        })
        .await
        .context("pool store worker thread panicked")?
    }

    async fn with_shared<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&HashMap<String, PoolRecord>) -> T + Send + 'static,
        T: Send + 'static,
    {
        let path = self.path.clone();
        let lock_path = self.lock_path.clone();
        tokio::task::spawn_blocking(move || -> Result<T> {
            let lock_file = fs::OpenOptions::new().create(true).write(true).open(&lock_path)
                .context("opening pool store lock file")?;
            if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_SH) } != 0 {
                bail!("locking pool store: {}", std::io::Error::last_os_error());
            }
            let map = Self::read_map(&path)?;
            Ok(f(&map))
        })
        .await
        .context("pool store worker thread panicked")?
    }

    pub async fn insert(&self, pool: PoolRecord) -> Result<()> {
        self.with_exclusive(move |m| {
            m.insert(pool.name.clone(), pool);
        })
        .await
    }

    pub async fn get(&self, name: &str) -> Option<PoolRecord> {
        let name = name.to_string();
        self.with_shared(move |m| m.get(&name).cloned()).await.ok().flatten()
    }

    pub async fn list(&self) -> Vec<PoolRecord> {
        self.with_shared(|m| {
            let mut v: Vec<_> = m.values().cloned().collect();
            v.sort_by_key(|p| p.name.clone());
            v
        })
        .await
        .unwrap_or_default()
    }

    pub async fn remove(&self, name: &str) -> Result<Option<PoolRecord>> {
        let name = name.to_string();
        self.with_exclusive(move |m| m.remove(&name)).await
    }

    /// Atomically pops one member id off `name`'s pool (so two concurrent
    /// claims can never receive the same VM) and returns it, or `None` if
    /// the pool has no ready members right now.
    pub async fn pop_member(&self, name: &str) -> Result<Option<Uuid>> {
        let name = name.to_string();
        self.with_exclusive(move |m| m.get_mut(&name).and_then(|p| p.members.pop())).await
    }

    /// Atomically appends a freshly-backfilled member id to `name`'s pool.
    /// A no-op (not an error) if the pool was deleted concurrently — the
    /// backfill task that produced `member` should then just clean it up
    /// itself rather than resurrect a deleted pool.
    pub async fn push_member(&self, name: &str, member: Uuid) -> Result<bool> {
        let name = name.to_string();
        self.with_exclusive(move |m| match m.get_mut(&name) {
            Some(p) => { p.members.push(member); true }
            None => false,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ephemera_core::model::{BackendKind, CreateVmRequest, NetworkSpec, VmStatus};
    use std::collections::HashSet;

    fn fixture_record(name: &str) -> VmRecord {
        let id = Uuid::new_v4();
        VmRecord {
            id,
            name: name.to_string(),
            backend: BackendKind::Qemu,
            status: VmStatus::Creating,
            pid: None,
            created_at: Utc::now(),
            expires_at: None,
            workspace: PathBuf::from("/tmp/does-not-matter"),
            disk: PathBuf::from("/tmp/does-not-matter/root.qcow2"),
            seed_disk: None,
            tap_name: None,
            control_socket: None,
            log_path: PathBuf::from("/tmp/does-not-matter/console.log"),
            error: None,
            request: CreateVmRequest {
                name: name.to_string(),
                backend: BackendKind::Qemu,
                image: PathBuf::from("/tmp/base.qcow2"),
                vcpus: 1,
                memory_mib: 512,
                disk_size_gib: None,
                kernel: None,
                initrd: None,
                firmware: None,
                kernel_args: None,
                network: NetworkSpec::None,
                cloud_init: None,
                ttl_seconds: None,
                extra_args: vec![],
                agent: None,
                storage: Default::default(),
            },
            guest_cid: None,
            jail_path: None,
            vsock_socket: None,
            cgroup_path: None,
            netns: None,
            lvm_lv: None,
            nbd_pid: None,
        }
    }

    #[tokio::test]
    async fn insert_get_list_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(dir.path()).unwrap();
        let vm = fixture_record("a");
        let id = vm.id;

        store.insert(vm.clone()).await.unwrap();
        assert_eq!(store.get(id).await.unwrap().name, "a");
        assert_eq!(store.list().await.len(), 1);

        let removed = store.remove(id).await.unwrap();
        assert_eq!(removed.unwrap().id, id);
        assert!(store.get(id).await.is_none());
        assert_eq!(store.list().await.len(), 0);
    }

    #[tokio::test]
    async fn a_second_store_instance_sees_the_first_ones_writes() {
        // Simulates two separate `ephemera` CLI processes pointed at the
        // same state_dir: each gets its own Store, loaded independently.
        let dir = tempfile::tempdir().unwrap();
        let store_a = Store::load(dir.path()).unwrap();
        let vm = fixture_record("from-a");
        store_a.insert(vm.clone()).await.unwrap();

        let store_b = Store::load(dir.path()).unwrap();
        let seen = store_b.get(vm.id).await;
        assert!(seen.is_some(), "a fresh Store instance must see another instance's writes");
        assert_eq!(seen.unwrap().name, "from-a");
    }

    #[tokio::test]
    async fn insert_with_cid_skips_used_cids() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(dir.path()).unwrap();

        let mut taken = fixture_record("taken-3");
        taken.guest_cid = Some(3);
        store.insert(taken).await.unwrap();

        let assigned = store.insert_with_cid(fixture_record("wants-cid"), true, 3).await.unwrap();
        assert_eq!(assigned.guest_cid, Some(4), "CID 3 is taken, so the next VM must get 4");
    }

    #[tokio::test]
    async fn insert_with_cid_is_race_free_across_concurrent_processes() {
        // Regression test for a real bug: allocating a CID via list() and
        // inserting via a separate insert() call let two concurrent
        // processes both compute the same "lowest free" CID before either
        // persisted, so they'd collide (confirmed via a live 4-way
        // concurrent `ephemera create` stress test before this was fixed).
        // Each spawned task here gets its OWN Store — a fresh `load()`, not
        // a shared handle — to accurately simulate separate OS processes
        // racing on the same vms.json rather than just concurrent tasks
        // sharing one in-process Store.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();

        let mut handles = Vec::new();
        for i in 0..8 {
            let path = path.clone();
            handles.push(tokio::spawn(async move {
                let store = Store::load(&path).unwrap();
                store
                    .insert_with_cid(fixture_record(&format!("race-{i}")), true, 3)
                    .await
                    .unwrap()
            }));
        }

        let mut cids = Vec::new();
        for h in handles {
            cids.push(h.await.unwrap().guest_cid.expect("CID must be assigned"));
        }

        let unique: HashSet<u32> = cids.iter().copied().collect();
        assert_eq!(unique.len(), cids.len(), "all concurrently-assigned CIDs must be distinct: {cids:?}");

        let store = Store::load(&path).unwrap();
        assert_eq!(store.list().await.len(), 8, "no concurrent write should have been silently lost");
    }

    fn fixture_pool(name: &str) -> ephemera_core::model::PoolRecord {
        ephemera_core::model::PoolRecord {
            name: name.to_string(),
            size: 2,
            template: fixture_record("template").request,
            members: vec![],
        }
    }

    #[tokio::test]
    async fn pool_insert_get_list_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = PoolStore::load(dir.path()).unwrap();
        store.insert(fixture_pool("a")).await.unwrap();

        assert_eq!(store.get("a").await.unwrap().size, 2);
        assert_eq!(store.list().await.len(), 1);
        assert!(store.remove("a").await.unwrap().is_some());
        assert!(store.get("a").await.is_none());
    }

    #[tokio::test]
    async fn push_and_pop_member_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = PoolStore::load(dir.path()).unwrap();
        store.insert(fixture_pool("a")).await.unwrap();

        let id = Uuid::new_v4();
        assert!(store.push_member("a", id).await.unwrap());
        assert_eq!(store.get("a").await.unwrap().members, vec![id]);

        let popped = store.pop_member("a").await.unwrap();
        assert_eq!(popped, Some(id));
        assert!(store.get("a").await.unwrap().members.is_empty());
        assert_eq!(store.pop_member("a").await.unwrap(), None, "popping an empty pool must not error");
    }

    #[tokio::test]
    async fn push_member_on_deleted_pool_is_a_harmless_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let store = PoolStore::load(dir.path()).unwrap();
        // No insert — "a" was never created (or was already deleted).
        assert!(!store.push_member("a", Uuid::new_v4()).await.unwrap());
    }

    #[tokio::test]
    async fn pop_member_is_race_free_across_concurrent_processes() {
        // Same regression shape as insert_with_cid_is_race_free_across_concurrent_processes:
        // each task gets its OWN PoolStore (a fresh load(), simulating a
        // separate `ephemera pool claim` CLI invocation), racing to pop
        // members off a pool pre-seeded with 8 — a real bug here would let
        // two concurrent claims hand out the same VM id.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let store = PoolStore::load(&path).unwrap();
        let mut seeded = fixture_pool("a");
        seeded.members = (0..8).map(|_| Uuid::new_v4()).collect();
        let all_ids: HashSet<Uuid> = seeded.members.iter().copied().collect();
        store.insert(seeded).await.unwrap();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let path = path.clone();
            handles.push(tokio::spawn(async move {
                let store = PoolStore::load(&path).unwrap();
                store.pop_member("a").await.unwrap()
            }));
        }
        let mut popped = Vec::new();
        for h in handles {
            popped.push(h.await.unwrap().expect("every claim should get a member — 8 popped from 8 seeded"));
        }

        let unique: HashSet<Uuid> = popped.iter().copied().collect();
        assert_eq!(unique.len(), popped.len(), "no two concurrent claims may pop the same member: {popped:?}");
        assert_eq!(unique, all_ids);
        assert!(store.get("a").await.unwrap().members.is_empty());
    }
}
