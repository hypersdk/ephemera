use anyhow::{Context, Result};
use ephemera_core::model::VmRecord;
use std::{collections::HashMap, fs, path::{Path, PathBuf}};
use tokio::sync::RwLock;
use uuid::Uuid;

pub struct Store {
    path: PathBuf,
    inner: RwLock<HashMap<Uuid, VmRecord>>,
}

impl Store {
    pub fn load(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join("vms.json");
        let map = if path.exists() {
            let raw = fs::read_to_string(&path)?;
            serde_json::from_str(&raw).context("parsing VM state")?
        } else {
            HashMap::new()
        };
        Ok(Self { path, inner: RwLock::new(map) })
    }

    async fn persist_locked(&self, map: &HashMap<Uuid, VmRecord>) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(map)?)?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }

    pub async fn insert(&self, vm: VmRecord) -> Result<()> {
        let mut g = self.inner.write().await;
        g.insert(vm.id, vm);
        self.persist_locked(&g).await
    }

    pub async fn update(&self, vm: VmRecord) -> Result<()> {
        self.insert(vm).await
    }

    pub async fn get(&self, id: Uuid) -> Option<VmRecord> {
        self.inner.read().await.get(&id).cloned()
    }

    pub async fn list(&self) -> Vec<VmRecord> {
        let mut v: Vec<_> = self.inner.read().await.values().cloned().collect();
        v.sort_by_key(|r| r.created_at);
        v
    }

    pub async fn remove(&self, id: Uuid) -> Result<Option<VmRecord>> {
        let mut g = self.inner.write().await;
        let old = g.remove(&id);
        self.persist_locked(&g).await?;
        Ok(old)
    }
}
