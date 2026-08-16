// Copyright 2026 Zyvor
// SPDX-License-Identifier: Apache-2.0

use axum::{
    extract::{Path, Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Extension, Json, Router,
};
use ephemera_core::{
    config::{constant_time_eq, Role},
    model::{BackendKind, ClaimOverrides, CreateVmRequest, PoolSpec, VmRecord, VmStatus},
};
use ephemera_image::{self as image, BuildImageRequest};
use ephemera_scheduler::VmManager;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

#[derive(Debug)]
struct ApiError { status: StatusCode, message: String }
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self { Self { status: StatusCode::BAD_REQUEST, message: format!("{:#}", e.into()) } }
}
impl ApiError {
    fn forbidden(message: impl Into<String>) -> Self { Self { status: StatusCode::FORBIDDEN, message: message.into() } }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// Bounces a request without a valid bearer token; requests that do have
/// one carry their resolved `Role` onward as a request extension. When
/// `cfg.auth.tokens` is empty, auth is off entirely and every request is
/// treated as `Role::Admin` — this is what makes an un-opted-in deployment
/// behave exactly like the pre-auth MVP.
async fn auth_middleware(State(m): State<Arc<VmManager>>, mut req: Request, next: Next) -> Response {
    if req.uri().path() == "/healthz" {
        return next.run(req).await;
    }
    let role = if m.cfg.auth.tokens.is_empty() {
        Some(Role::Admin)
    } else {
        let presented = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "));
        presented.and_then(|t| {
            m.cfg.auth.tokens.iter().find(|entry| constant_time_eq(&entry.token, t)).map(|entry| entry.role)
        })
    };
    match role {
        Some(role) => {
            req.extensions_mut().insert(role);
            next.run(req).await
        }
        None => (StatusCode::UNAUTHORIZED, Json(json!({"error": "missing or invalid bearer token"}))).into_response(),
    }
}

fn require_admin(role: Role) -> ApiResult<()> {
    if role != Role::Admin {
        return Err(ApiError::forbidden("admin role required"));
    }
    Ok(())
}

pub fn router(manager: Arc<VmManager>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { Json(json!({"ok": true})) }))
        .route("/metrics", get(metrics))
        .route("/v1/vms", post(create_vm).get(list_vms))
        .route("/v1/vms/{id}", get(get_vm).delete(delete_vm))
        .route("/v1/vms/{id}/start", post(start_vm))
        .route("/v1/vms/{id}/stop", post(stop_vm))
        .route("/v1/vms/{id}/pause", post(pause_vm))
        .route("/v1/vms/{id}/resume", post(resume_vm))
        .route("/v1/vms/{id}/resources", post(set_vm_resources))
        .route("/v1/vms/{id}/freeze", post(freeze_vm))
        .route("/v1/vms/{id}/thaw", post(thaw_vm))
        .route("/v1/vms/{id}/frozen", get(vm_frozen))
        .route("/v1/vms/{id}/stats", get(vm_stats))
        .route("/v1/vms/{id}/pressure", get(vm_pressure))
        .route("/v1/vms/{id}/logs", get(vm_logs))
        .route("/v1/vms/{id}/agent", post(agent_exec))
        .route("/v1/images/build", post(build_image))
        .route("/v1/images/catalog", post(add_catalog_entry))
        .route("/v1/images/catalog/{name}", delete(remove_catalog_entry))
        .route("/v1/images/catalog/{name}/rename", post(rename_catalog_entry))
        .route("/v1/images/catalog/{name}/clone", post(clone_catalog_entry))
        .route("/v1/images/catalog/{name}/export", post(export_catalog_entry))
        .route("/v1/images/catalog", get(list_catalog))
        .route("/v1/pools", post(create_pool).get(list_pools))
        .route("/v1/pools/{name}", get(get_pool).delete(delete_pool))
        .route("/v1/pools/{name}/claim", post(claim_pool))
        .layer(middleware::from_fn_with_state(manager.clone(), auth_middleware))
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

async fn create_vm(State(m): State<Arc<VmManager>>, Extension(role): Extension<Role>, Json(req): Json<CreateVmRequest>) -> ApiResult<impl IntoResponse> {
    require_admin(role)?;
    Ok((StatusCode::CREATED, Json(m.create(req).await?)))
}
#[derive(Deserialize)]
struct ListVmsQuery {
    /// Exact-match filter on `VmRecord.name`. Added for zyvor-fabric's
    /// `EphemeraDriver`, which is keyed by name (systemd-machined's model)
    /// while `VmRecord` is keyed by `Uuid` — this lets the driver resolve a
    /// name to a record server-side instead of pulling the full list on
    /// every lookup.
    #[serde(default)]
    name: Option<String>,
}

async fn list_vms(State(m): State<Arc<VmManager>>, Query(q): Query<ListVmsQuery>) -> Json<serde_json::Value> {
    let mut items = m.list().await;
    if let Some(name) = q.name {
        items.retain(|vm| vm.name == name);
    }
    Json(json!({"items": items}))
}
async fn get_vm(State(m): State<Arc<VmManager>>, Path(id): Path<Uuid>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!(m.get(id).await?)))
}
async fn start_vm(State(m): State<Arc<VmManager>>, Extension(role): Extension<Role>, Path(id): Path<Uuid>) -> ApiResult<Json<serde_json::Value>> {
    require_admin(role)?;
    Ok(Json(json!(m.start(id).await?)))
}
async fn stop_vm(State(m): State<Arc<VmManager>>, Extension(role): Extension<Role>, Path(id): Path<Uuid>) -> ApiResult<Json<serde_json::Value>> {
    require_admin(role)?;
    Ok(Json(json!(m.stop(id).await?)))
}
async fn pause_vm(State(m): State<Arc<VmManager>>, Extension(role): Extension<Role>, Path(id): Path<Uuid>) -> ApiResult<Json<serde_json::Value>> {
    require_admin(role)?;
    Ok(Json(json!(m.pause(id).await?)))
}
async fn resume_vm(State(m): State<Arc<VmManager>>, Extension(role): Extension<Role>, Path(id): Path<Uuid>) -> ApiResult<Json<serde_json::Value>> {
    require_admin(role)?;
    Ok(Json(json!(m.resume(id).await?)))
}

async fn set_vm_resources(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
    Json(patch): Json<ephemera_core::model::ResourcePatch>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    m.set_resources(id, patch).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn freeze_vm(State(m): State<Arc<VmManager>>, Extension(role): Extension<Role>, Path(id): Path<Uuid>) -> ApiResult<StatusCode> {
    require_admin(role)?;
    m.freeze(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn thaw_vm(State(m): State<Arc<VmManager>>, Extension(role): Extension<Role>, Path(id): Path<Uuid>) -> ApiResult<StatusCode> {
    require_admin(role)?;
    m.thaw(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn vm_frozen(State(m): State<Arc<VmManager>>, Path(id): Path<Uuid>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({"frozen": m.is_frozen(id).await?})))
}

async fn vm_stats(State(m): State<Arc<VmManager>>, Path(id): Path<Uuid>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!(m.metrics(id).await?)))
}

async fn vm_pressure(State(m): State<Arc<VmManager>>, Path(id): Path<Uuid>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!(m.pressure(id).await?)))
}

#[derive(Debug, Deserialize)]
struct LogsQuery {
    #[serde(default)]
    follow: bool,
    #[serde(default = "default_log_lines")]
    lines: usize,
}
fn default_log_lines() -> usize { 100 }

/// `GET /v1/vms/{id}/logs?lines=N&follow=true` — tail-follow the VM's
/// captured console output (`VmRecord.log_path`) as a plain-text chunked
/// stream, one line per chunk. Raw serial output has no journald-equivalent
/// structure (no per-line priority/unit), so unlike `machinectl-driver`'s
/// `journalctl --output=json` this is deliberately unstructured — the
/// caller (`ephemera-driver::LogDriver`) assigns a constant priority when
/// wrapping lines into `driver-core::LogEntry`.
async fn vm_logs(
    State(m): State<Arc<VmManager>>,
    Path(id): Path<Uuid>,
    Query(q): Query<LogsQuery>,
) -> ApiResult<Response> {
    let record = m.get(id).await?;
    let path = record.log_path;
    let follow = q.follow;
    let lines = q.lines.max(1);

    let stream = async_stream::stream! {
        use std::collections::VecDeque;
        use tokio::io::{AsyncBufReadExt, BufReader};

        let file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                yield Ok::<_, std::io::Error>(bytes::Bytes::from(format!("error opening log: {e}\n")));
                return;
            }
        };
        let mut reader = BufReader::new(file);
        let mut line = String::new();

        // First pass: keep only the last `lines` lines already on disk.
        let mut tail: VecDeque<String> = VecDeque::with_capacity(lines);
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    if tail.len() == lines {
                        tail.pop_front();
                    }
                    tail.push_back(std::mem::take(&mut line));
                }
                Err(_) => break,
            }
        }
        for l in tail {
            yield Ok::<_, std::io::Error>(bytes::Bytes::from(l));
        }

        if follow {
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => tokio::time::sleep(std::time::Duration::from_millis(300)).await,
                    Ok(_) => yield Ok::<_, std::io::Error>(bytes::Bytes::from(std::mem::take(&mut line))),
                    Err(_) => break,
                }
            }
        }
    };

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(axum::body::Body::from_stream(stream))
        .unwrap())
}

#[derive(Deserialize)]
struct ExecRequest {
    command: String,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}
async fn agent_exec(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
    Json(req): Json<ExecRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(role)?;
    let response = m.exec(id, req.command, req.timeout_seconds).await?;
    Ok(Json(json!(response)))
}
async fn delete_vm(State(m): State<Arc<VmManager>>, Extension(role): Extension<Role>, Path(id): Path<Uuid>) -> ApiResult<StatusCode> {
    require_admin(role)?;
    m.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn build_image(State(m): State<Arc<VmManager>>, Extension(role): Extension<Role>, Json(req): Json<BuildImageRequest>) -> ApiResult<Json<serde_json::Value>> {
    require_admin(role)?;
    Ok(Json(json!(image::build_image(&m.cfg, &req).await?)))
}

/// Read-only (no role check beyond a valid token, like other GET routes) —
/// signing itself is a CLI/offline operation (`ephemera catalog sign`), not
/// exposed here, so private keys never touch this API's surface.
async fn list_catalog(State(m): State<Arc<VmManager>>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({"items": image::catalog::list_with_verification(&m.cfg)?})))
}

#[derive(Deserialize)]
struct AddCatalogEntryRequest {
    name: String,
    /// Local path or `http(s)://` URL — see `CatalogEntry::source`.
    source: String,
    #[serde(default = "default_catalog_format")]
    format: String,
}
fn default_catalog_format() -> String { "qcow2".into() }

async fn add_catalog_entry(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Json(req): Json<AddCatalogEntryRequest>,
) -> ApiResult<impl IntoResponse> {
    require_admin(role)?;
    let entry = m.add_catalog_entry(req.name, req.source, req.format).await?;
    Ok((StatusCode::CREATED, Json(json!(entry))))
}

async fn remove_catalog_entry(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    m.remove_catalog_entry(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RenameCatalogEntryRequest {
    new_name: String,
}
async fn rename_catalog_entry(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
    Json(req): Json<RenameCatalogEntryRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(role)?;
    let entry = m.rename_catalog_entry(&name, &req.new_name).await?;
    Ok(Json(json!(entry)))
}

#[derive(Deserialize)]
struct CloneCatalogEntryRequest {
    target_name: String,
}
async fn clone_catalog_entry(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
    Json(req): Json<CloneCatalogEntryRequest>,
) -> ApiResult<impl IntoResponse> {
    require_admin(role)?;
    let entry = m.clone_catalog_entry(&name, &req.target_name).await?;
    Ok((StatusCode::CREATED, Json(json!(entry))))
}

#[derive(Deserialize)]
struct ExportCatalogEntryRequest {
    path: PathBuf,
}
async fn export_catalog_entry(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
    Json(req): Json<ExportCatalogEntryRequest>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    m.export_catalog_entry(&name, &req.path).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_pool(State(m): State<Arc<VmManager>>, Extension(role): Extension<Role>, Json(spec): Json<PoolSpec>) -> ApiResult<impl IntoResponse> {
    require_admin(role)?;
    Ok((StatusCode::CREATED, Json(m.create_pool(spec).await?)))
}
async fn list_pools(State(m): State<Arc<VmManager>>) -> Json<serde_json::Value> {
    Json(json!({"items": m.list_pools().await}))
}
async fn get_pool(State(m): State<Arc<VmManager>>, Path(name): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!(m.get_pool(&name).await?)))
}
async fn delete_pool(State(m): State<Arc<VmManager>>, Extension(role): Extension<Role>, Path(name): Path<String>) -> ApiResult<StatusCode> {
    require_admin(role)?;
    m.delete_pool(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn claim_pool(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
    Json(overrides): Json<ClaimOverrides>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(role)?;
    Ok(Json(json!(m.claim_from_pool(&name, overrides).await?)))
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
                agent: agent_enabled.then(|| AgentSpec { enabled: true, port: 17777, token: None }),
                storage: Default::default(),
            },
            guest_cid: None,
            jail_path: None,
            vsock_socket: None,
            cgroup_path: None,
            netns: None,
            lvm_lv: None,
            nbd_pid: None,
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

    mod auth {
        use super::*;
        use axum::body::Body;
        use axum::http::Request;
        use ephemera_core::config::{ApiToken, AuthConfig, Config};
        use tower::ServiceExt;

        fn manager(auth: AuthConfig) -> Arc<VmManager> {
            let dir = tempfile::tempdir().unwrap();
            // Leak: the tempdir must outlive every VmManager call in the
            // test, and these tests are short-lived processes anyway.
            let dir = Box::leak(Box::new(dir));
            let cfg = Config {
                state_dir: dir.path().join("state"),
                run_dir: dir.path().join("run"),
                auth,
                ..Config::default()
            };
            VmManager::new(cfg).unwrap()
        }

        async fn status_for(app: Router, method: &str, uri: &str, bearer: Option<&str>) -> StatusCode {
            request(app, method, uri, bearer, None).await
        }

        /// `body` is sent as `application/json` when present — needed for
        /// any route whose handler takes a `Json<T>` extractor, since axum
        /// rejects with 415 during extraction (before the handler body, and
        /// so before this module's `require_admin` role check, ever runs)
        /// if the request has no JSON content-type at all.
        async fn request(app: Router, method: &str, uri: &str, bearer: Option<&str>, body: Option<&str>) -> StatusCode {
            let mut builder = Request::builder().method(method).uri(uri);
            if let Some(t) = bearer {
                builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
            }
            let body = match body {
                Some(b) => {
                    builder = builder.header(header::CONTENT_TYPE, "application/json");
                    Body::from(b.to_string())
                }
                None => Body::empty(),
            };
            let req = builder.body(body).unwrap();
            app.oneshot(req).await.unwrap().status()
        }

        #[tokio::test]
        async fn auth_disabled_allows_unauthenticated_requests() {
            let app = router(manager(AuthConfig::default()));
            assert_eq!(status_for(app, "GET", "/v1/vms", None).await, StatusCode::OK);
        }

        #[tokio::test]
        async fn missing_token_is_rejected_when_auth_is_enabled() {
            let auth = AuthConfig { tokens: vec![ApiToken { token: "secret".into(), role: Role::Admin, name: None }] };
            let app = router(manager(auth));
            assert_eq!(status_for(app, "GET", "/v1/vms", None).await, StatusCode::UNAUTHORIZED);
        }

        #[tokio::test]
        async fn wrong_token_is_rejected() {
            let auth = AuthConfig { tokens: vec![ApiToken { token: "secret".into(), role: Role::Admin, name: None }] };
            let app = router(manager(auth));
            assert_eq!(status_for(app, "GET", "/v1/vms", Some("nope")).await, StatusCode::UNAUTHORIZED);
        }

        #[tokio::test]
        async fn healthz_is_reachable_without_a_token_even_when_auth_is_enabled() {
            let auth = AuthConfig { tokens: vec![ApiToken { token: "secret".into(), role: Role::Admin, name: None }] };
            let app = router(manager(auth));
            assert_eq!(status_for(app, "GET", "/healthz", None).await, StatusCode::OK);
        }

        const VALID_CREATE_BODY: &str = r#"{"name":"t","backend":"qemu","image":"/does/not/exist.qcow2"}"#;

        #[tokio::test]
        async fn readonly_token_can_list_but_not_create() {
            let auth = AuthConfig { tokens: vec![ApiToken { token: "ro".into(), role: Role::ReadOnly, name: None }] };
            let app = router(manager(auth));
            assert_eq!(status_for(app.clone(), "GET", "/v1/vms", Some("ro")).await, StatusCode::OK);
            assert_eq!(request(app, "POST", "/v1/vms", Some("ro"), Some(VALID_CREATE_BODY)).await, StatusCode::FORBIDDEN);
        }

        #[tokio::test]
        async fn admin_token_passes_auth_for_a_mutating_route() {
            let auth = AuthConfig { tokens: vec![ApiToken { token: "admin".into(), role: Role::Admin, name: None }] };
            let app = router(manager(auth));
            // The image path doesn't exist, so this fails downstream with
            // 400 — the point is it's NOT 401/403, i.e. the Admin token
            // cleared the auth layer and reached VmManager::create.
            let status = request(app, "POST", "/v1/vms", Some("admin"), Some(VALID_CREATE_BODY)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
    }
}
