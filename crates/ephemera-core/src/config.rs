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
    pub cloud_localds_binary: String,
    pub virt_customize_binary: String,
    pub firecracker_binary: String,
    pub firecracker_kernel: Option<PathBuf>,
    pub cloud_hypervisor_firmware: Option<PathBuf>,
    pub default_bridge: Option<String>,
    pub reaper_interval_secs: u64,
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
            cloud_localds_binary: "cloud-localds".into(),
            virt_customize_binary: "virt-customize".into(),
            firecracker_binary: "firecracker".into(),
            firecracker_kernel: None,
            cloud_hypervisor_firmware: None,
            default_bridge: Some("vmbr0".into()),
            reaper_interval_secs: 5,
        }
    }
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
