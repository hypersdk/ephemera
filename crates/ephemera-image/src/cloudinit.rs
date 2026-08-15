// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use ephemera_core::{config::Config, model::CloudInitSpec, process::run_checked};
use std::{fs, path::{Path, PathBuf}};

fn yaml_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub async fn build_seed(cfg: &Config, dir: &Path, ci: &CloudInitSpec) -> Result<PathBuf> {
    let user_data = dir.join("user-data");
    let meta_data = dir.join("meta-data");
    let seed = dir.join("seed.img");
    let hostname = ci.hostname.clone().unwrap_or_else(|| "ephemera-vm".into());
    let user = ci.user.clone().unwrap_or_else(|| "cloud".into());

    let mut body = String::from("#cloud-config\n");
    body.push_str(&format!("hostname: {}\n", yaml_quote(&hostname)));
    body.push_str("users:\n");
    body.push_str("  - default\n");
    body.push_str(&format!("  - name: {}\n    sudo: ALL=(ALL) NOPASSWD:ALL\n    shell: /bin/bash\n", yaml_quote(&user)));
    if !ci.ssh_authorized_keys.is_empty() {
        body.push_str("    ssh_authorized_keys:\n");
        for key in &ci.ssh_authorized_keys {
            body.push_str(&format!("      - {}\n", yaml_quote(key)));
        }
    }
    if !ci.packages.is_empty() {
        body.push_str("package_update: true\npackages:\n");
        for p in &ci.packages { body.push_str(&format!("  - {}\n", yaml_quote(p))); }
    }
    if !ci.runcmd.is_empty() {
        body.push_str("runcmd:\n");
        for cmd in &ci.runcmd { body.push_str(&format!("  - [ bash, -lc, {} ]\n", yaml_quote(cmd))); }
    }

    fs::write(&user_data, body).context("writing cloud-init user-data")?;
    fs::write(&meta_data, format!("instance-id: {}\nlocal-hostname: {}\n", hostname, hostname))?;

    run_checked(&cfg.cloud_localds_binary, &[
        "--disk-format".into(), "raw".into(),
        seed.display().to_string(),
        user_data.display().to_string(),
        meta_data.display().to_string(),
    ]).await?;
    Ok(seed)
}
