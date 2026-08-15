// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! Per-VM network namespaces: real isolation (separate routing table,
//! iptables, interface list — not just a shared L2 segment like the
//! tap-on-a-shared-bridge mode), built from a veth pair (host <-> namespace)
//! NATed to the host's own connectivity, plus a small internal bridge
//! inside the namespace joining the veth's namespace end to the VM's own
//! tap device.
//!
//! Topology (host on the left, the VM's namespace on the right):
//!
//! ```text
//!   host default netns                    │  VM's netns
//!                                          │
//!   <vethh> 169.254.X.1/30  <───veth pair───>  <vethn> ── <br> ── <tap> ── VM
//!        │                                │       169.254.X.2/30 on <br>
//!   iptables MASQUERADE                   │  default route via 169.254.X.1
//!   (POSTROUTING -s 169.254.X.0/30)       │
//! ```
//!
//! The /30 index `X` is derived deterministically from the VM's id (low 6
//! bits, giving 64 possible subnets in 169.254.0.0/16) rather than tracked
//! in any allocation table — simple, and collision-free for the realistic
//! number of concurrent namespaced VMs one host would run, at the cost of a
//! theoretical collision at higher concurrency (documented, not solved,
//! same tradeoff this project already makes for a few other MVP-scoped
//! allocators).

use anyhow::{Context, Result};
use ephemera_core::process::run_checked;
use uuid::Uuid;

pub struct NetnsHandle {
    pub netns: String,
    pub tap_name: String,
}

fn short_id(id: Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

fn subnet_octet(id: Uuid) -> u8 {
    (id.as_u128() % 64) as u8 * 4
}

/// Creates the namespace, veth pair, internal bridge, and tap device
/// described in the module doc, and wires up NAT so the VM can reach
/// outbound. Best-effort torn down (via `cleanup`) if any step fails partway
/// through.
pub async fn prepare(id: Uuid, mac: Option<&str>) -> Result<NetnsHandle> {
    let short = short_id(id);
    let netns = format!("eph-{short}");
    let veth_host = format!("vh{short}");
    let veth_ns = format!("vn{short}");
    let bridge = format!("br{short}");
    let tap = format!("tap{short}");
    let octet = subnet_octet(id);
    let host_ip = format!("169.254.{octet}.1/30");
    let ns_ip = format!("169.254.{octet}.2/30");
    let ns_subnet = format!("169.254.{octet}.0/30");
    let gateway = format!("169.254.{octet}.1");

    let result: Result<()> = async {
        run_checked("ip", &["netns".into(), "add".into(), netns.clone()]).await
            .context("creating network namespace")?;
        run_checked("ip", &["link".into(), "add".into(), veth_host.clone(), "type".into(), "veth".into(), "peer".into(), "name".into(), veth_ns.clone()]).await
            .context("creating veth pair")?;
        run_checked("ip", &["link".into(), "set".into(), veth_ns.clone(), "netns".into(), netns.clone()]).await
            .context("moving veth namespace-end into the namespace")?;
        run_checked("ip", &["addr".into(), "add".into(), host_ip, "dev".into(), veth_host.clone()]).await
            .context("assigning host-end veth address")?;
        run_checked("ip", &["link".into(), "set".into(), veth_host.clone(), "up".into()]).await
            .context("bringing up host-end veth")?;

        // Everything from here on runs inside the namespace via `ip netns
        // exec <ns> ip ...` — note the second `ip`: `ip netns exec` runs an
        // arbitrary command line inside the namespace, it doesn't assume
        // that command is `ip` itself, so the program name has to be
        // supplied again as part of the wrapped args.
        let in_ns = |args: Vec<String>| {
            let mut full = vec!["netns".to_string(), "exec".to_string(), netns.clone(), "ip".to_string()];
            full.extend(args);
            full
        };
        run_checked("ip", &in_ns(vec!["link".into(), "set".into(), "lo".into(), "up".into()])).await
            .context("bringing up loopback in namespace")?;
        run_checked("ip", &in_ns(vec!["link".into(), "add".into(), bridge.clone(), "type".into(), "bridge".into()])).await
            .context("creating internal bridge in namespace")?;
        run_checked("ip", &in_ns(vec!["link".into(), "set".into(), veth_ns.clone(), "master".into(), bridge.clone()])).await
            .context("attaching veth namespace-end to internal bridge")?;
        run_checked("ip", &in_ns(vec!["tuntap".into(), "add".into(), "dev".into(), tap.clone(), "mode".into(), "tap".into()])).await
            .context("creating tap device in namespace")?;
        if let Some(m) = mac {
            run_checked("ip", &in_ns(vec!["link".into(), "set".into(), "dev".into(), tap.clone(), "address".into(), m.to_string()])).await
                .context("setting tap MAC address")?;
        }
        run_checked("ip", &in_ns(vec!["link".into(), "set".into(), tap.clone(), "master".into(), bridge.clone()])).await
            .context("attaching tap to internal bridge")?;
        run_checked("ip", &in_ns(vec!["addr".into(), "add".into(), ns_ip, "dev".into(), bridge.clone()])).await
            .context("assigning internal bridge address")?;
        run_checked("ip", &in_ns(vec!["link".into(), "set".into(), veth_ns.clone(), "up".into()])).await
            .context("bringing up veth namespace-end")?;
        run_checked("ip", &in_ns(vec!["link".into(), "set".into(), tap.clone(), "up".into()])).await
            .context("bringing up tap")?;
        run_checked("ip", &in_ns(vec!["link".into(), "set".into(), bridge.clone(), "up".into()])).await
            .context("bringing up internal bridge")?;
        run_checked("ip", &in_ns(vec!["route".into(), "add".into(), "default".into(), "via".into(), gateway])).await
            .context("adding default route in namespace")?;

        // Host-level: allow the namespace to reach the outside world.
        run_checked("sysctl", &["-w".into(), "net.ipv4.ip_forward=1".into()]).await
            .context("enabling IP forwarding")?;
        // Idempotent: skip if the rule from a previous run (crash before
        // cleanup) is somehow still present, rather than accumulate duplicates.
        let exists = run_checked("iptables", &["-t".into(), "nat".into(), "-C".into(), "POSTROUTING".into(), "-s".into(), ns_subnet.clone(), "-j".into(), "MASQUERADE".into()]).await.is_ok();
        if !exists {
            run_checked("iptables", &["-t".into(), "nat".into(), "-A".into(), "POSTROUTING".into(), "-s".into(), ns_subnet, "-j".into(), "MASQUERADE".into()]).await
                .context("adding NAT rule")?;
        }
        Ok(())
    }.await;

    match result {
        Ok(()) => Ok(NetnsHandle { netns, tap_name: tap }),
        Err(e) => {
            let _ = cleanup(id, &netns).await;
            Err(e)
        }
    }
}

/// Deletes the namespace (which cascades: every interface inside it,
/// including the veth namespace-end and — since deleting either end of a
/// veth pair deletes both — the host-side veth end too) and removes the NAT
/// rule. Best-effort: logs nothing on its own, callers already log/ignore
/// per the existing `cleanup_tap`/`cleanup_macvtap` convention.
pub async fn cleanup(id: Uuid, netns: &str) -> Result<()> {
    let octet = subnet_octet(id);
    let ns_subnet = format!("169.254.{octet}.0/30");
    let _ = run_checked("iptables", &["-t".into(), "nat".into(), "-D".into(), "POSTROUTING".into(), "-s".into(), ns_subnet, "-j".into(), "MASQUERADE".into()]).await;
    let _ = run_checked("ip", &["netns".into(), "del".into(), netns.into()]).await;
    Ok(())
}
