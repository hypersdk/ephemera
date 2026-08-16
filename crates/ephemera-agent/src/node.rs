// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! The per-host half: periodically reports this node's identity, capacity,
//! and current VM count to a central `ephemera-agent central` registry.
//! Runs alongside a local `ephemera serve` — it never touches VMs directly,
//! only reads `GET /v1/vms` off the local REST API to count them.

use anyhow::{Context, Result};
use serde_json::json;
use std::time::Duration;

pub struct NodeConfig {
    pub name: String,
    pub central_url: String,
    /// Address this agent itself uses to reach the local `ephemera serve`
    /// for counting VMs — almost always a loopback address
    /// (`http://127.0.0.1:7788`).
    pub ephemera_url: String,
    /// Address a REMOTE central registry should use to reach this same
    /// `ephemera serve` — this has to be this host's real, externally
    /// routable address, not `ephemera_url` unchanged. Found running
    /// central and two node agents across two separate physical hosts
    /// before this field existed: both nodes' default `--ephemera-url`
    /// (`http://127.0.0.1:...`) got reported to central verbatim, which
    /// would have made central's create/list/delete proxy calls for
    /// "node-b" actually hit whatever `ephemera serve` is listening on
    /// central's OWN host — silently wrong, not an error, since central
    /// happens to run its own node agent + local ephemera too.
    /// `advertise_url` exists so a node's own view of its ephemera
    /// (`ephemera_url`) and the outside view central needs
    /// (`advertise_url`) can differ.
    pub advertise_url: String,
    pub interval: Duration,
}

/// Runs forever, heartbeating every `cfg.interval`. Logs and keeps going on
/// a failed beat (the central registry marking this node stale after
/// `HEALTHY_WINDOW_SECS` is the correct outcome of a *sustained* outage —
/// a node agent that gave up after one failed request would make a
/// transient network blip permanently evict an otherwise-fine node).
pub async fn run(cfg: NodeConfig) {
    let http = reqwest::Client::new();
    let mut tick = tokio::time::interval(cfg.interval);
    loop {
        tick.tick().await;
        if let Err(e) = beat(&http, &cfg).await {
            tracing::warn!(error = %e, "heartbeat failed, will retry next tick");
        } else {
            tracing::debug!(node = %cfg.name, "heartbeat sent");
        }
    }
}

async fn beat(http: &reqwest::Client, cfg: &NodeConfig) -> Result<()> {
    let vm_count = local_vm_count(http, &cfg.ephemera_url).await.context("counting local VMs")?;
    let vcpus_total = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let memory_mib_total = total_memory_mib().unwrap_or(0);

    let body = json!({
        "name": cfg.name,
        "ephemera_url": cfg.advertise_url,
        "vcpus_total": vcpus_total,
        "memory_mib_total": memory_mib_total,
        "vm_count": vm_count,
    });
    let resp = http.post(format!("{}/fleet/register", cfg.central_url)).json(&body).send().await
        .context("sending heartbeat")?;
    if !resp.status().is_success() {
        anyhow::bail!("central rejected heartbeat: {}", resp.status());
    }
    Ok(())
}

async fn local_vm_count(http: &reqwest::Client, ephemera_url: &str) -> Result<usize> {
    let resp = http.get(format!("{ephemera_url}/v1/vms")).send().await.context("GET /v1/vms")?;
    let body: serde_json::Value = resp.json().await.context("parsing /v1/vms response")?;
    Ok(body.get("items").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0))
}

/// Real total RAM off `/proc/meminfo` (`MemTotal:` is always the first
/// line) — this project is Linux-only throughout, same as every other
/// host-introspection call in it (cgroups, `/proc/uptime`, ...).
fn total_memory_mib() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = content.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024)
}
