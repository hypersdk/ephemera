// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use ephemera_core::model::{BackendKind, CreateVmRequest, VmRecord, VmStatus};
use ephemera_image::{self as image, BuildImageRequest};
use ephemera_scheduler::VmManager;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

#[derive(Debug)]
struct ApiError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError { fn from(e: E) -> Self { Self(e.into()) } }
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, Json(json!({"error": format!("{:#}", self.0)}))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

pub fn router(manager: Arc<VmManager>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { Json(json!({"ok": true})) }))
        .route("/metrics", get(metrics))
        .route("/v1/vms", post(create_vm).get(list_vms))
        .route("/v1/vms/{id}", get(get_vm).delete(delete_vm))
        .route("/v1/vms/{id}/stop", post(stop_vm))
        .route("/v1/vms/{id}/pause", post(pause_vm))
        .route("/v1/vms/{id}/resume", post(resume_vm))
        .route("/v1/vms/{id}/agent", post(agent_exec))
        .route("/v1/images/build", post(build_image))
        .layer(TraceLayer::new_for_http())
        .with_state(manager)
}

async fn metrics(State(m): State<Arc<VmManager>>) -> Response {
    let body = render_metrics(&m.list().await);
    (StatusCode::OK, [("content-type", "text/plain; version=0.0.4; charset=utf-8")], body).into_response()
}

/// Pure text-rendering, kept separate from the handler so it's unit-testable
/// without spinning up a VmManager/axum app.
fn render_metrics(vms: &[VmRecord]) -> String {
    let mut out = String::new();

    out.push_str("# HELP ephemera_vms_total Number of VMs known to this ephemera instance, by status.\n");
    out.push_str("# TYPE ephemera_vms_total gauge\n");
    for status in [VmStatus::Creating, VmStatus::Running, VmStatus::Paused, VmStatus::Stopped, VmStatus::Failed] {
        let count = vms.iter().filter(|v| v.status == status).count();
        out.push_str(&format!("ephemera_vms_total{{status=\"{}\"}} {count}\n", status_label(status)));
    }

    out.push_str("# HELP ephemera_vms_by_backend Number of VMs known to this ephemera instance, by backend.\n");
    out.push_str("# TYPE ephemera_vms_by_backend gauge\n");
    for backend in [BackendKind::Qemu, BackendKind::CloudHypervisor, BackendKind::Firecracker] {
        let count = vms.iter().filter(|v| v.backend == backend).count();
        out.push_str(&format!("ephemera_vms_by_backend{{backend=\"{}\"}} {count}\n", backend_label(backend)));
    }

    out.push_str("# HELP ephemera_vms_agent_enabled Number of VMs with the vsock guest agent enabled.\n");
    out.push_str("# TYPE ephemera_vms_agent_enabled gauge\n");
    let agent_enabled = vms.iter().filter(|v| v.request.agent.as_ref().is_some_and(|a| a.enabled)).count();
    out.push_str(&format!("ephemera_vms_agent_enabled {agent_enabled}\n"));

    out
}

fn status_label(s: VmStatus) -> &'static str {
    match s {
        VmStatus::Creating => "creating",
        VmStatus::Running => "running",
        VmStatus::Paused => "paused",
        VmStatus::Stopped => "stopped",
        VmStatus::Failed => "failed",
    }
}

fn backend_label(b: BackendKind) -> &'static str {
    match b {
        BackendKind::Qemu => "qemu",
        BackendKind::CloudHypervisor => "cloud-hypervisor",
        BackendKind::Firecracker => "firecracker",
        BackendKind::Auto => "auto",
    }
}

async fn create_vm(State(m): State<Arc<VmManager>>, Json(req): Json<CreateVmRequest>) -> ApiResult<impl IntoResponse> {
    Ok((StatusCode::CREATED, Json(m.create(req).await?)))
}
async fn list_vms(State(m): State<Arc<VmManager>>) -> Json<serde_json::Value> {
    Json(json!({"items": m.list().await}))
}
async fn get_vm(State(m): State<Arc<VmManager>>, Path(id): Path<Uuid>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!(m.get(id).await?)))
}
async fn stop_vm(State(m): State<Arc<VmManager>>, Path(id): Path<Uuid>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!(m.stop(id).await?)))
}
async fn pause_vm(State(m): State<Arc<VmManager>>, Path(id): Path<Uuid>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!(m.pause(id).await?)))
}
async fn resume_vm(State(m): State<Arc<VmManager>>, Path(id): Path<Uuid>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!(m.resume(id).await?)))
}

#[derive(Deserialize)]
struct ExecRequest {
    command: String,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}
async fn agent_exec(
    State(m): State<Arc<VmManager>>,
    Path(id): Path<Uuid>,
    Json(req): Json<ExecRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let response = m.exec(id, req.command, req.timeout_seconds).await?;
    Ok(Json(json!(response)))
}
async fn delete_vm(State(m): State<Arc<VmManager>>, Path(id): Path<Uuid>) -> ApiResult<StatusCode> {
    m.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn build_image(State(m): State<Arc<VmManager>>, Json(req): Json<BuildImageRequest>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!(image::build_image(&m.cfg, &req).await?)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ephemera_core::model::{AgentSpec, CreateVmRequest, NetworkSpec};
    use std::path::PathBuf;

    fn fixture(backend: BackendKind, status: VmStatus, agent_enabled: bool) -> VmRecord {
        VmRecord {
            id: Uuid::new_v4(),
            name: "fixture".into(),
            backend,
            status,
            pid: None,
            created_at: chrono::Utc::now(),
            expires_at: None,
            workspace: PathBuf::from("/tmp/x"),
            disk: PathBuf::from("/tmp/x/root.qcow2"),
            seed_disk: None,
            tap_name: None,
            control_socket: None,
            log_path: PathBuf::from("/tmp/x/console.log"),
            error: None,
            request: CreateVmRequest {
                name: "fixture".into(),
                backend,
                image: PathBuf::from("/tmp/base.qcow2"),
                vcpus: 1,
                memory_mib: 512,
                disk_size_gib: None,
                kernel: None,
                initrd: None,
                firmware: None,
                kernel_args: None,
                network: NetworkSpec::None,
                cloud_init: None,
                ttl_seconds: None,
                extra_args: vec![],
                agent: agent_enabled.then(|| AgentSpec { enabled: true, port: 17777 }),
            },
            guest_cid: None,
        }
    }

    #[test]
    fn counts_by_status_and_backend() {
        let vms = vec![
            fixture(BackendKind::Qemu, VmStatus::Running, true),
            fixture(BackendKind::Qemu, VmStatus::Paused, false),
            fixture(BackendKind::CloudHypervisor, VmStatus::Running, false),
            fixture(BackendKind::Firecracker, VmStatus::Failed, false),
        ];
        let out = render_metrics(&vms);

        assert!(out.contains("ephemera_vms_total{status=\"running\"} 2"));
        assert!(out.contains("ephemera_vms_total{status=\"paused\"} 1"));
        assert!(out.contains("ephemera_vms_total{status=\"stopped\"} 0"));
        assert!(out.contains("ephemera_vms_total{status=\"failed\"} 1"));

        assert!(out.contains("ephemera_vms_by_backend{backend=\"qemu\"} 2"));
        assert!(out.contains("ephemera_vms_by_backend{backend=\"cloud-hypervisor\"} 1"));
        assert!(out.contains("ephemera_vms_by_backend{backend=\"firecracker\"} 1"));

        assert!(out.contains("ephemera_vms_agent_enabled 1"));
    }

    #[test]
    fn empty_fleet_still_renders_zeroed_gauges() {
        let out = render_metrics(&[]);
        assert!(out.contains("ephemera_vms_total{status=\"running\"} 0"));
        assert!(out.contains("ephemera_vms_agent_enabled 0"));
    }
}
