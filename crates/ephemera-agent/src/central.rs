// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

//! Central fleet registry + proxy. Deliberately doesn't depend on
//! `ephemera-core` — it treats `CreateVmRequest`/`VmRecord` bodies as opaque
//! JSON and just forwards them to the right node's own `ephemera serve`,
//! the same way `ephemera-kube`'s client does. This keeps the binary small
//! and means it never goes stale against the request/record schema; the
//! cost is no server-side validation beyond "is this valid JSON" — a node's
//! own `ephemera serve` still does the real validation.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

/// A node is considered unhealthy (excluded from placement, and reported
/// but flagged in `GET /fleet/nodes`) once its last heartbeat is older
/// than this — generous relative to the node agent's own default 10s
/// heartbeat interval, so a couple of missed beats under load doesn't
/// falsely evict a node that's still fine.
const HEALTHY_WINDOW_SECS: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub name: String,
    pub ephemera_url: String,
    pub vcpus_total: u32,
    pub memory_mib_total: u64,
    pub vm_count: usize,
    pub last_seen: DateTime<Utc>,
}

impl NodeInfo {
    fn healthy(&self) -> bool {
        (Utc::now() - self.last_seen).num_seconds() < HEALTHY_WINDOW_SECS
    }
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub ephemera_url: String,
    pub vcpus_total: u32,
    pub memory_mib_total: u64,
    pub vm_count: usize,
}

struct AppError(StatusCode, String);
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
impl<E: std::fmt::Display> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(StatusCode::BAD_GATEWAY, e.to_string())
    }
}

#[derive(Clone)]
struct Fleet {
    nodes: Arc<Mutex<HashMap<String, NodeInfo>>>,
    http: reqwest::Client,
}

pub fn router() -> Router {
    let fleet = Fleet { nodes: Arc::new(Mutex::new(HashMap::new())), http: reqwest::Client::new() };
    Router::new()
        .route("/healthz", get(|| async { Json(json!({"ok": true})) }))
        .route("/fleet/register", post(register))
        .route("/fleet/nodes", get(list_nodes))
        .route("/fleet/vms", post(create_vm).get(list_vms))
        .route("/fleet/vms/{node}/{id}", axum::routing::delete(delete_vm))
        .with_state(fleet)
}

async fn register(State(fleet): State<Fleet>, Json(req): Json<RegisterRequest>) -> Json<Value> {
    let mut nodes = fleet.nodes.lock().await;
    nodes.insert(req.name.clone(), NodeInfo {
        name: req.name,
        ephemera_url: req.ephemera_url,
        vcpus_total: req.vcpus_total,
        memory_mib_total: req.memory_mib_total,
        vm_count: req.vm_count,
        last_seen: Utc::now(),
    });
    Json(json!({"ok": true}))
}

async fn list_nodes(State(fleet): State<Fleet>) -> Json<Value> {
    let nodes = fleet.nodes.lock().await;
    let items: Vec<Value> = nodes.values().map(|n| {
        json!({
            "name": n.name,
            "ephemera_url": n.ephemera_url,
            "vcpus_total": n.vcpus_total,
            "memory_mib_total": n.memory_mib_total,
            "vm_count": n.vm_count,
            "last_seen": n.last_seen,
            "healthy": n.healthy(),
        })
    }).collect();
    Json(json!({"items": items}))
}

/// Picks the healthy node with the fewest VMs (a simple, real placement
/// policy — not the sole "auto" story this project has: see
/// `ephemera_scheduler::resolve_backend` for per-VM backend auto-selection,
/// a different axis entirely from *which host* runs it). Ties broken by
/// name for determinism, not load-balanced randomness.
fn pick_least_loaded(nodes: &HashMap<String, NodeInfo>) -> Option<NodeInfo> {
    nodes.values().filter(|n| n.healthy()).min_by_key(|n| (n.vm_count, n.name.clone())).cloned()
}

/// `POST /fleet/vms`. Body is a normal `CreateVmRequest` JSON, optionally
/// with a top-level `"node"` field naming an exact node to target — when
/// absent, the least-loaded healthy node is chosen. The response wraps the
/// created `VmRecord` with which node it actually landed on, since the
/// caller has no other way to find that back out (a fleet-wide `VmRecord`
/// alone doesn't say where it lives).
async fn create_vm(State(fleet): State<Fleet>, Json(mut body): Json<Value>) -> Result<Json<Value>, AppError> {
    let requested_node = body.as_object_mut().and_then(|o| o.remove("node")).and_then(|v| v.as_str().map(str::to_string));

    let target = {
        let nodes = fleet.nodes.lock().await;
        match requested_node {
            Some(name) => nodes.get(&name).cloned()
                .ok_or_else(|| AppError(StatusCode::BAD_REQUEST, format!("no registered node named '{name}'")))?,
            None => pick_least_loaded(&nodes)
                .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "no healthy nodes registered".into()))?,
        }
    };

    let resp = fleet.http.post(format!("{}/v1/vms", target.ephemera_url)).json(&body).send().await?;
    let status = resp.status();
    let record: Value = resp.json().await?;
    if !status.is_success() {
        return Err(AppError(StatusCode::BAD_GATEWAY, format!("node '{}' rejected create: {record}", target.name)));
    }
    Ok(Json(json!({"node": target.name, "vm": record})))
}

/// `GET /fleet/vms`. Aggregates `GET /v1/vms` from every currently-healthy
/// node, tagging each record with which node it came from. Best-effort —
/// a node that fails to respond is skipped (with a warning), not a hard
/// failure of the whole call; a partial fleet view beats none.
async fn list_vms(State(fleet): State<Fleet>) -> Json<Value> {
    let targets: Vec<NodeInfo> = { fleet.nodes.lock().await.values().filter(|n| n.healthy()).cloned().collect() };
    let mut items = Vec::new();
    for node in targets {
        match fleet.http.get(format!("{}/v1/vms", node.ephemera_url)).send().await {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(body) => {
                    if let Some(node_items) = body.get("items").and_then(|v| v.as_array()) {
                        for mut vm in node_items.clone() {
                            if let Some(obj) = vm.as_object_mut() {
                                obj.insert("node".into(), json!(node.name));
                            }
                            items.push(vm);
                        }
                    }
                }
                Err(e) => tracing::warn!(node = %node.name, error = %e, "failed to parse node's VM list"),
            },
            Err(e) => tracing::warn!(node = %node.name, error = %e, "failed to reach node for fleet-wide list"),
        }
    }
    Json(json!({"items": items}))
}

async fn delete_vm(State(fleet): State<Fleet>, Path((node, id)): Path<(String, String)>) -> Result<StatusCode, AppError> {
    let target = {
        let nodes = fleet.nodes.lock().await;
        nodes.get(&node).cloned().ok_or_else(|| AppError(StatusCode::BAD_REQUEST, format!("no registered node named '{node}'")))?
    };
    let resp = fleet.http.delete(format!("{}/v1/vms/{}", target.ephemera_url, id)).send().await?;
    let status = resp.status();
    if status.is_success() {
        Ok(StatusCode::NO_CONTENT)
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(AppError(StatusCode::BAD_GATEWAY, format!("node '{node}' rejected delete: {body}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, vm_count: usize, age_secs: i64) -> NodeInfo {
        NodeInfo {
            name: name.into(),
            ephemera_url: format!("http://{name}"),
            vcpus_total: 4,
            memory_mib_total: 8192,
            vm_count,
            last_seen: Utc::now() - chrono::Duration::seconds(age_secs),
        }
    }

    #[test]
    fn picks_the_healthy_node_with_the_fewest_vms() {
        let mut nodes = HashMap::new();
        nodes.insert("a".to_string(), node("a", 3, 0));
        nodes.insert("b".to_string(), node("b", 1, 0));
        nodes.insert("c".to_string(), node("c", 2, 0));
        let picked = pick_least_loaded(&nodes).unwrap();
        assert_eq!(picked.name, "b");
    }

    #[test]
    fn skips_stale_nodes_even_if_they_have_the_fewest_vms() {
        let mut nodes = HashMap::new();
        nodes.insert("stale".to_string(), node("stale", 0, HEALTHY_WINDOW_SECS + 5));
        nodes.insert("fresh".to_string(), node("fresh", 5, 0));
        let picked = pick_least_loaded(&nodes).unwrap();
        assert_eq!(picked.name, "fresh");
    }

    #[test]
    fn returns_none_when_no_node_is_healthy() {
        let mut nodes = HashMap::new();
        nodes.insert("stale".to_string(), node("stale", 0, HEALTHY_WINDOW_SECS + 5));
        assert!(pick_least_loaded(&nodes).is_none());
    }

    #[test]
    fn ties_break_by_name_for_determinism() {
        let mut nodes = HashMap::new();
        nodes.insert("z".to_string(), node("z", 1, 0));
        nodes.insert("a".to_string(), node("a", 1, 0));
        let picked = pick_least_loaded(&nodes).unwrap();
        assert_eq!(picked.name, "a");
    }
}
