// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Result};
use ephemera_core::{backend::PreparedNetwork, config::Config, model::NetworkSpec, process::run_checked};
use uuid::Uuid;

pub async fn prepare(cfg: &Config, id: Uuid, spec: &NetworkSpec) -> Result<PreparedNetwork> {
    match spec {
        NetworkSpec::None | NetworkSpec::User { .. } => Ok(PreparedNetwork {
            spec: spec.clone(),
            tap_name: None,
        }),
        NetworkSpec::Tap { tap_name, bridge, mac } => {
            let tap = tap_name.clone().unwrap_or_else(|| format!("eph{}", &id.simple().to_string()[..8]));
            if tap.len() > 15 { bail!("tap interface name must be <= 15 characters"); }
            let bridge = bridge.clone().or_else(|| cfg.default_bridge.clone());

            run_checked("ip", &["tuntap".into(), "add".into(), "dev".into(), tap.clone(), "mode".into(), "tap".into()]).await?;
            run_checked("ip", &["link".into(), "set".into(), "dev".into(), tap.clone(), "up".into()]).await?;
            if let Some(br) = &bridge {
                if let Err(e) = run_checked("ip", &["link".into(), "set".into(), tap.clone(), "master".into(), br.clone()]).await {
                    let _ = cleanup_tap(&tap).await;
                    return Err(e);
                }
            }
            Ok(PreparedNetwork {
                spec: NetworkSpec::Tap { tap_name: Some(tap.clone()), bridge, mac: mac.clone() },
                tap_name: Some(tap),
            })
        }
    }
}

pub async fn cleanup_tap(tap: &str) -> Result<()> {
    let _ = run_checked("ip", &["link".into(), "set".into(), "dev".into(), tap.into(), "down".into()]).await;
    let _ = run_checked("ip", &["tuntap".into(), "del".into(), "dev".into(), tap.into(), "mode".into(), "tap".into()]).await;
    Ok(())
}
