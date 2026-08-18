// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ephemera_core::{config::Config, model::CloudInitSpec, process::run_checked};
use std::{fs, path::{Path, PathBuf}};

fn yaml_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `static_net`, when `ci.static_network` is set, is `(guest_cidr, gateway)`
/// -- e.g. `("169.254.12.35/28", "169.254.12.33")` -- from
/// `ephemera_network::netns::NetnsHandle`/`PreparedNetwork`. Callers must
/// run network prep *before* this so that's available; `None` is only
/// valid when `ci.static_network` is false (checked by the caller, not
/// re-validated here, since which networking modes have a known address is
/// this module's caller's concern, not cloud-init seed-building's).
pub async fn build_seed(cfg: &Config, dir: &Path, ci: &CloudInitSpec, static_net: Option<(&str, &str)>) -> Result<PathBuf> {
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
    if !ci.write_files.is_empty() {
        body.push_str("write_files:\n");
        for f in &ci.write_files {
            body.push_str(&format!("  - path: {}\n", yaml_quote(&f.path)));
            if let Some(perms) = &f.permissions {
                body.push_str(&format!("    permissions: {}\n", yaml_quote(perms)));
            }
            body.push_str("    encoding: b64\n");
            body.push_str(&format!("    content: {}\n", yaml_quote(&B64.encode(f.content.as_bytes()))));
        }
    }

    fs::write(&user_data, body).context("writing cloud-init user-data")?;
    fs::write(&meta_data, format!("instance-id: {}\nlocal-hostname: {}\n", hostname, hostname))?;

    let mut args = vec!["--disk-format".to_string(), "raw".to_string()];
    let network_config = dir.join("network-config");
    if ci.static_network {
        let (cidr, gateway) = static_net
            .context("static_network is set but no address was prepared -- network prep must run before build_seed")?;
        // `match: {name: en*}` rather than a specific interface name: which
        // predictable name (enp0s1, ens3, eth0, ...) a given kernel/udev
        // combination assigns isn't known ahead of boot, and this is the
        // netplan-supported way to say "whichever the single real NIC is".
        let netcfg = format!(
            "network:\n  version: 2\n  ethernets:\n    guestnet0:\n      match:\n        name: en*\n      dhcp4: false\n      addresses:\n        - {cidr}\n      routes:\n        - to: default\n          via: {gateway}\n"
        );
        fs::write(&network_config, netcfg).context("writing cloud-init network-config")?;
        args.push("--network-config".into());
        args.push(network_config.display().to_string());
    }
    args.push(seed.display().to_string());
    args.push(user_data.display().to_string());
    args.push(meta_data.display().to_string());

    run_checked(&cfg.cloud_localds_binary, &args).await?;
    Ok(seed)
}
