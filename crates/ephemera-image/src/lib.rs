pub mod cloudinit;

use anyhow::{bail, Context, Result};
use ephemera_core::{config::Config, model::BackendKind, process::run_checked};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, path::{Path, PathBuf}};
use tokio::{fs as async_fs, io::AsyncWriteExt};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildImageRequest {
    pub source: String,
    pub output: PathBuf,
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default)]
    pub size_gib: Option<u64>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub ssh_key: Option<String>,
}
fn default_format() -> String { "qcow2".into() }

#[derive(Debug, Clone, Serialize)]
pub struct BuildImageResult {
    pub output: PathBuf,
    pub format: String,
}

async fn fetch_if_needed(cfg: &Config, source: &str) -> Result<PathBuf> {
    if !source.starts_with("http://") && !source.starts_with("https://") {
        return Ok(PathBuf::from(source));
    }
    let downloads = cfg.state_dir.join("downloads");
    fs::create_dir_all(&downloads)?;
    let name = source.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("base.img");
    let dest = downloads.join(name);
    if dest.exists() { return Ok(dest); }
    let mut resp = Client::new().get(source).send().await?.error_for_status()?;
    let mut f = async_fs::File::create(&dest).await?;
    while let Some(chunk) = resp.chunk().await? { f.write_all(&chunk).await?; }
    Ok(dest)
}

fn verify_sha256(path: &Path, wanted: &str) -> Result<()> {
    let bytes = fs::read(path)?;
    let got = format!("{:x}", Sha256::digest(&bytes));
    if !got.eq_ignore_ascii_case(wanted) { bail!("sha256 mismatch: expected {wanted}, got {got}"); }
    Ok(())
}

pub async fn build_image(cfg: &Config, req: &BuildImageRequest) -> Result<BuildImageResult> {
    let src = fetch_if_needed(cfg, &req.source).await?;
    if let Some(hash) = &req.sha256 { verify_sha256(&src, hash)?; }
    if let Some(parent) = req.output.parent() { fs::create_dir_all(parent)?; }

    run_checked(&cfg.qemu_img_binary, &[
        "convert".into(), "-O".into(), req.format.clone(),
        src.display().to_string(), req.output.display().to_string(),
    ]).await?;
    if let Some(size) = req.size_gib {
        run_checked(&cfg.qemu_img_binary, &[
            "resize".into(), req.output.display().to_string(), format!("{}G", size)
        ]).await?;
    }

    if req.hostname.is_some() || !req.packages.is_empty() || !req.commands.is_empty() || req.ssh_key.is_some() {
        let mut a = vec!["-a".into(), req.output.display().to_string()];
        if let Some(h) = &req.hostname { a.extend(["--hostname".into(), h.clone()]); }
        if !req.packages.is_empty() { a.extend(["--install".into(), req.packages.join(",")]); }
        for c in &req.commands { a.extend(["--run-command".into(), c.clone()]); }
        if let Some(key) = &req.ssh_key {
            let key_file = req.output.with_extension("builder.pub");
            fs::write(&key_file, key).context("writing temporary ssh key")?;
            a.extend(["--ssh-inject".into(), format!("root:file:{}", key_file.display())]);
            let result = run_checked(&cfg.virt_customize_binary, &a).await;
            let _ = fs::remove_file(key_file);
            result?;
        } else {
            run_checked(&cfg.virt_customize_binary, &a).await?;
        }
    }
    Ok(BuildImageResult { output: req.output.clone(), format: req.format.clone() })
}

async fn image_format(cfg: &Config, image: &Path) -> String {
    ephemera_core::process::output_checked(&cfg.qemu_img_binary, &[
        "info".into(), "--output=json".into(), image.display().to_string()
    ]).await.ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("format").and_then(|f| f.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "qcow2".into())
}

pub async fn clone_for_vm(cfg: &Config, base: &Path, backend: BackendKind, out: &Path, size_gib: Option<u64>) -> Result<()> {
    let base_fmt = image_format(cfg, base).await;
    match backend {
        BackendKind::Qemu => {
            // Cheap disposable copy-on-write layer.
            run_checked(&cfg.qemu_img_binary, &[
                "create".into(), "-f".into(), "qcow2".into(),
                "-F".into(), base_fmt,
                "-b".into(), base.canonicalize()?.display().to_string(),
                out.display().to_string(),
            ]).await?;
        }
        BackendKind::CloudHypervisor | BackendKind::Firecracker => {
            // Firecracker expects a raw block image. Cloud Hypervisor is also kept raw here
            // for a predictable common fast path. Reflink makes raw clones nearly instant on
            // XFS/Btrfs; cp transparently falls back when reflinks are unavailable.
            if base_fmt == "raw" {
                run_checked("cp", &[
                    "--reflink=auto".into(), "--sparse=always".into(),
                    base.display().to_string(), out.display().to_string(),
                ]).await?;
            } else {
                run_checked(&cfg.qemu_img_binary, &[
                    "convert".into(), "-O".into(), "raw".into(),
                    base.display().to_string(), out.display().to_string(),
                ]).await?;
            }
        }
    }
    if let Some(size) = size_gib {
        run_checked(&cfg.qemu_img_binary, &[
            "resize".into(), out.display().to_string(), format!("{}G", size)
        ]).await?;
    }
    Ok(())
}
