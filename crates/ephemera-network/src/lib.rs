// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

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
                macvtap_fd: None,
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
pub async fn cleanup(spec: &NetworkSpec, name: &str) -> Result<()> {
    match spec {
        NetworkSpec::Macvtap { .. } => cleanup_macvtap(name).await,
        _ => cleanup_tap(name).await,
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
