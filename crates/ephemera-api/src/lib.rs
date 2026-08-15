// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use ephemera_core::model::CreateVmRequest;
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
