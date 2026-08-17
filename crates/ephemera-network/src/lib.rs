// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

pub mod netns;

use anyhow::{bail, Context, Result};
use ephemera_core::{backend::PreparedNetwork, config::Config, model::NetworkSpec, process::run_checked};
use std::ffi::CString;
use uuid::Uuid;

pub async fn prepare(cfg: &Config, id: Uuid, spec: &NetworkSpec) -> Result<PreparedNetwork> {
    match spec {
        NetworkSpec::None | NetworkSpec::User { .. } => Ok(PreparedNetwork {
            spec: spec.clone(),
            tap_name: None,
            macvtap_fd: None,
            netns: None,
            dhcp_leasefile: None,
        }),
        NetworkSpec::Tap { tap_name, bridge, mac, netns: use_netns } if *use_netns => {
            let handle = netns::prepare(id, mac.as_deref()).await.context("preparing network namespace")?;
            Ok(PreparedNetwork {
                spec: NetworkSpec::Tap { tap_name: Some(handle.tap_name.clone()), bridge: bridge.clone(), mac: mac.clone(), netns: true },
                tap_name: Some(handle.tap_name),
                macvtap_fd: None,
                netns: Some(handle.netns),
                dhcp_leasefile: Some(handle.dhcp_leasefile),
            })
        }
        NetworkSpec::Tap { tap_name, bridge, mac, .. } => {
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
                spec: NetworkSpec::Tap { tap_name: Some(tap.clone()), bridge, mac: mac.clone(), netns: false },
                tap_name: Some(tap),
                macvtap_fd: None,
                netns: None,
                dhcp_leasefile: None,
            })
        }
        NetworkSpec::Macvtap { parent, macvtap_mode, mac } => {
            let name = format!("eph{}", &id.simple().to_string()[..8]);
            if name.len() > 15 { bail!("macvtap interface name must be <= 15 characters"); }
            let mvmode = macvtap_mode.clone().unwrap_or_else(|| "bridge".into());

            let created: Result<i32> = async {
                run_checked("ip", &[
                    "link".into(), "add".into(), "link".into(), parent.clone(),
                    "name".into(), name.clone(), "type".into(), "macvtap".into(), "mode".into(), mvmode.clone(),
                ]).await?;
                if let Some(m) = mac {
                    run_checked("ip", &["link".into(), "set".into(), "dev".into(), name.clone(), "address".into(), m.clone()]).await?;
                }
                run_checked("ip", &["link".into(), "set".into(), "dev".into(), name.clone(), "up".into()]).await?;
                open_macvtap_fd(&name).await
            }.await;

            match created {
                Ok(fd) => Ok(PreparedNetwork {
                    spec: spec.clone(),
                    tap_name: Some(name),
                    macvtap_fd: Some(fd),
                    netns: None,
                    dhcp_leasefile: None,
                }),
                Err(e) => {
                    let _ = cleanup_macvtap(&name).await;
                    Err(e)
                }
            }
        }
    }
}

/// Opens the macvtap character device (`/dev/tap<ifindex>`) for `name`
/// without O_CLOEXEC, so the fd survives exec into the spawned VMM and can
/// be passed on the command line (`-netdev tap,fd=N` / `--net fd=N`).
async fn open_macvtap_fd(name: &str) -> Result<i32> {
    let ifindex = std::fs::read_to_string(format!("/sys/class/net/{name}/ifindex"))
        .with_context(|| format!("reading ifindex for {name}"))?;
    let dev_path = format!("/dev/tap{}", ifindex.trim());
    let c_path = CString::new(dev_path.clone()).context("device path contains a NUL byte")?;

    // SAFETY: c_path is a valid, NUL-terminated C string for the lifetime of
    // this call; libc::open with O_RDWR only (no O_CLOEXEC) is what makes
    // this fd inheritable by the child VMM process across fork+exec.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        bail!("opening {dev_path}: {}", std::io::Error::last_os_error());
    }
    Ok(fd)
}

/// Dispatches cleanup of the ephemeral network device recorded for a VM,
/// based on which kind it is (TAP vs. macvtap use different deletion
/// commands).
pub async fn cleanup(id: Uuid, spec: &NetworkSpec, tap_name: &str, netns_name: Option<&str>) -> Result<()> {
    // Deleting the namespace tears down everything inside it (the tap
    // included) — no separate tap cleanup needed, and calling
    // cleanup_tap/cleanup_macvtap for a namespaced tap would fail anyway
    // (it doesn't exist in the host's own namespace to delete).
    if let Some(ns) = netns_name {
        return netns::cleanup(id, ns).await;
    }
    match spec {
        NetworkSpec::Macvtap { .. } => cleanup_macvtap(tap_name).await,
        _ => cleanup_tap(tap_name).await,
    }
}

pub async fn cleanup_tap(tap: &str) -> Result<()> {
    let _ = run_checked("ip", &["link".into(), "set".into(), "dev".into(), tap.into(), "down".into()]).await;
    let _ = run_checked("ip", &["tuntap".into(), "del".into(), "dev".into(), tap.into(), "mode".into(), "tap".into()]).await;
    Ok(())
}

pub async fn cleanup_macvtap(name: &str) -> Result<()> {
    let _ = run_checked("ip", &["link".into(), "del".into(), name.into()]).await;
    Ok(())
}
