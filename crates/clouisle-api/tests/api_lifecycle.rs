//! API 端到端集成测试（HTTP 层行为）。
//!
//! 产品只保留 `FirecrackerVmm`（Linux+KVM）。此处 HTTP 层测试需要 `Vmm` 实现
//! 来驱动校验/路由/状态码层；`TestVmm` 是**仅测试夹具**（`#[cfg(test)]`），
//! 功能等价于内存状态机，不依赖 KVM，不随产品发布。

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use base64::Engine;

use clouisle_api::{AppState, agent, auth, build_router};
use clouisle_core::{Result, SandboxSpec};
use clouisle_scheduler::ResourcePool;
use clouisle_store::InMemoryStore;
use clouisle_vmm::{
    SnapshotKind, SnapshotPaths, StopMode, VmHandle, VmStats, Vmm, VmmCapabilities,
};
use tower::ServiceExt;

/// 仅测试夹具：内存状态机 VMM（不依赖 KVM，非产品后端）。
#[derive(Clone)]
pub struct TestVmm {
    running: Arc<std::sync::atomic::AtomicUsize>,
    probe_alive: Arc<std::sync::atomic::AtomicBool>,
}

impl TestVmm {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_probe_alive(probe_alive: bool) -> Self {
        let vmm = Self::default();
        vmm.probe_alive
            .store(probe_alive, std::sync::atomic::Ordering::SeqCst);
        vmm
    }
}

impl Default for TestVmm {
    fn default() -> Self {
        Self {
            running: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            probe_alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }
}

#[async_trait]
impl Vmm for TestVmm {
    async fn create(&self, _: &str, spec: &SandboxSpec) -> Result<VmHandle> {
        if spec.image.reference == "missing:latest" {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            return Err(clouisle_core::ClouisleError::not_found("image not found"));
        }
        Ok(VmHandle {
            id: uuid::Uuid::now_v7().to_string(),
            backend: "test".into(),
            owner_id: None,
            pid: None,
            api_socket: None,
            vsock_socket: None,
            vsock_cid: None,
            subnet: None,
        })
    }
    async fn image_cache_hit(&self, spec: &SandboxSpec) -> Result<bool> {
        Ok(spec.image.reference != "missing:latest")
    }
    async fn prefetch_image(&self, spec: &SandboxSpec) -> Result<()> {
        if spec.image.reference == "missing:latest" {
            return Err(clouisle_core::ClouisleError::not_found("image not found"));
        }
        Ok(())
    }
    async fn probe(&self, _: &VmHandle) -> Result<bool> {
        Ok(self.probe_alive.load(std::sync::atomic::Ordering::SeqCst))
    }
    async fn start(&self, _h: &VmHandle) -> Result<()> {
        self.running
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn pause(&self, _h: &VmHandle) -> Result<()> {
        Ok(())
    }
    async fn resume(&self, _h: &VmHandle) -> Result<()> {
        Ok(())
    }
    async fn snapshot(&self, _h: &VmHandle, _k: SnapshotKind, _o: &SnapshotPaths) -> Result<()> {
        Ok(())
    }
    async fn restore(&self, _: &str, _s: &SandboxSpec, _f: &SnapshotPaths) -> Result<VmHandle> {
        Ok(VmHandle {
            id: uuid::Uuid::now_v7().to_string(),
            backend: "test".into(),
            owner_id: None,
            pid: None,
            api_socket: None,
            vsock_socket: None,
            vsock_cid: None,
            subnet: None,
        })
    }
    async fn stop(&self, _h: &VmHandle, _m: StopMode) -> Result<()> {
        self.running
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn stats(&self, _h: &VmHandle) -> Result<VmStats> {
        Ok(VmStats::default())
    }
    fn capabilities(&self) -> VmmCapabilities {
        VmmCapabilities {
            snapshot: true,
            vsock: true,
            balloon: false,
        }
    }
}

fn test_state() -> AppState {
    test_state_with_vmm(Arc::new(TestVmm::new()))
}

fn test_state_with_vmm(vmm: Arc<dyn Vmm>) -> AppState {
    let store = Arc::new(InMemoryStore::new());
    let pool = Arc::new(ResourcePool::new(AppState::host_capacity(), 200));
    let agent_conn: Arc<dyn agent::AgentConnector> = Arc::new(agent::MockAgentConnector);
    AppState {
        e2b: Arc::new(clouisle_api::E2bControlPlane::new()),
        store,
        warm_pool: Arc::new(clouisle_pool::Pool::new(0, 60, vmm.clone())),
        vmm,
        warm_slots: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        pool,
        draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        reservations: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        image_jobs: Arc::new(clouisle_api::ImageJobRegistry::new()),
        e2b_tokens: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        processes: Arc::new(clouisle_api::state::ProcessRegistry::default()),
        snapshots: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        subnet_alloc: clouisle_net::netns::SubnetAllocator::new(),
        provisioning: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
        manage_resources: true,
        agent: agent_conn,
        auth: Arc::new(auth::Authenticator::new()),
        #[cfg(target_os = "linux")]
        firewall: Arc::new(clouisle_net::FirewallManager::new()),
        #[cfg(target_os = "linux")]
        manage_network: false,
        version: "test",
    }
}

fn app() -> Router {
    build_router(test_state())
}

async fn post_json(
    app: &Router,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 10 * 1024 * 1024).await.unwrap();
    let val = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, val)
}

async fn get(app: &Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 10 * 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn delete(app: &Router, uri: &str) -> StatusCode {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    resp.status()
}

#[tokio::test]
async fn full_lifecycle_create_exec_delete() {
    let app = app();
    let (status, body) = post_json(
        &app,
        "/api/v1/sandboxes",
        serde_json::json!({
            "image": { "reference": "alpine:latest" },
            "resources": { "vcpu": 1, "memory_mb": 256, "disk_mb": 512 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create should 201, got {body}");
    let id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["status"], "running");

    let (status, body) = get(&app, &format!("/api/v1/sandboxes/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "running");

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/sandboxes/{id}/exec"),
        serde_json::json!({ "argv": ["echo", "hello"], "timeout_ms": 5000 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "exec should 200, got {body}");
    assert_eq!(body["exit_code"], 0);
    assert_eq!(body["stdout"], "hello\n");

    let status = delete(&app, &format!("/api/v1/sandboxes/{id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = get(&app, &format!("/api/v1/sandboxes/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn invalid_spec_rejected() {
    let app = app();
    let (status, body) = post_json(
        &app,
        "/api/v1/sandboxes",
        serde_json::json!({
            "image": { "reference": "alpine" },
            "resources": { "vcpu": 0, "memory_mb": 256, "disk_mb": 512 }
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "vcpu=0 should 400, got {body}"
    );
    assert_eq!(body["error"]["code"], "VALIDATION");

    let (status, _) = post_json(
        &app,
        "/api/v1/sandboxes",
        serde_json::json!({
            "resources": { "vcpu": 1, "memory_mb": 256, "disk_mb": 512 }
        }),
    )
    .await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "missing image should be client error, got {status}"
    );
}

#[tokio::test]
async fn delete_nonexistent_404() {
    let app = app();
    let status = delete(&app, "/api/v1/sandboxes/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn exec_on_nonrunning_sandbox_conflict() {
    let app = app();
    let (status, body) = post_json(
        &app,
        "/api/v1/sandboxes",
        serde_json::json!({
            "image": { "reference": "alpine" },
            "resources": { "vcpu": 1, "memory_mb": 64, "disk_mb": 512 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = body["id"].as_str().unwrap().to_string();

    let _ = delete(&app, &format!("/api/v1/sandboxes/{id}")).await;
    let (status, _) = post_json(
        &app,
        &format!("/api/v1/sandboxes/{id}/exec"),
        serde_json::json!({ "argv": ["echo"], "timeout_ms": 1000 }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_and_filter() {
    let app = app();
    for i in 0..3 {
        let (status, _) = post_json(
            &app,
            "/api/v1/sandboxes",
            serde_json::json!({
                "image": { "reference": format!("img-{i}") },
                "resources": { "vcpu": 1, "memory_mb": 64, "disk_mb": 512 }
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, body) = get(&app, "/api/v1/sandboxes").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 3);
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn health_and_metrics() {
    let app = app();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["store"], "ok");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn request_id_reflected() {
    let app = app();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/health")
                .header("x-request-id", "test-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.headers().get("x-request-id").unwrap(), "test-123");
}

#[tokio::test]
async fn concurrent_creates_no_crosstalk() {
    let app = app();
    let mut handles = Vec::new();
    for i in 0..20 {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            post_json(
                &app,
                "/api/v1/sandboxes",
                serde_json::json!({
                    "image": { "reference": format!("batch-{i}") },
                    "resources": { "vcpu": 1, "memory_mb": 64, "disk_mb": 512 }
                }),
            )
            .await
            .0
        }));
    }
    let mut created = 0;
    for h in handles {
        if h.await.unwrap() == StatusCode::CREATED {
            created += 1;
        }
    }
    let expected = AppState::host_capacity().vcpu.min(20) as usize;
    assert_eq!(
        created, expected,
        "admission must hold capacity permits across concurrent creates"
    );
}

#[tokio::test]
async fn exec_history_recorded() {
    let app = app();
    let (_, body) = post_json(
        &app,
        "/api/v1/sandboxes",
        serde_json::json!({
            "image": { "reference": "alpine" },
            "resources": { "vcpu": 1, "memory_mb": 64, "disk_mb": 512 }
        }),
    )
    .await;
    let id = body["id"].as_str().unwrap().to_string();

    let (_, body) = post_json(
        &app,
        &format!("/api/v1/sandboxes/{id}/exec"),
        serde_json::json!({ "argv": ["echo", "x"], "timeout_ms": 5000 }),
    )
    .await;
    let exec_id = body["exec_id"].as_str().unwrap().to_string();

    let (status, body) = get(&app, &format!("/api/v1/sandboxes/{id}/exec")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);

    let (status, body) = get(&app, &format!("/api/v1/sandboxes/{id}/exec/{exec_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["exit_code"], 0);
}

#[tokio::test]
async fn sandbox_environment_is_inherited_and_exec_environment_overrides_it() {
    let app = app();
    let (_, body) = post_json(
        &app,
        "/api/v1/sandboxes",
        serde_json::json!({
            "image": { "reference": "alpine" },
            "resources": { "vcpu": 1, "memory_mb": 64, "disk_mb": 512 },
            "env": { "FROM_SANDBOX": "sandbox", "OVERRIDE": "old" }
        }),
    )
    .await;
    let id = body["id"].as_str().unwrap();
    let (status, inherited) = post_json(
        &app,
        &format!("/api/v1/sandboxes/{id}/exec"),
        serde_json::json!({ "argv": ["printenv", "FROM_SANDBOX"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inherited["stdout"], "sandbox\n");
    let (status, overridden) = post_json(
        &app,
        &format!("/api/v1/sandboxes/{id}/exec"),
        serde_json::json!({ "argv": ["printenv", "OVERRIDE"], "env": { "OVERRIDE": "request" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(overridden["stdout"], "request\n");
}

#[tokio::test]
async fn secrets_are_redacted_and_path_like_names_are_rejected() {
    let app = app();
    let (status, created) = post_json(
        &app,
        "/api/v1/sandboxes",
        serde_json::json!({
            "image": { "reference": "alpine" },
            "secrets": [{ "name": "TOKEN", "value": "must-not-leak" }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["spec"]["secrets"][0]["value"], "[REDACTED]");
    let (_, invalid) = post_json(
        &app,
        "/api/v1/sandboxes",
        serde_json::json!({
            "image": { "reference": "alpine" },
            "secrets": [{ "name": "../escape", "value": "x" }]
        }),
    )
    .await;
    assert_eq!(invalid["error"]["code"], "VALIDATION");
}

#[tokio::test]
async fn execution_history_limit_and_unknown_status_are_validated() {
    let app = app();
    let (_, body) = post_json(
        &app,
        "/api/v1/sandboxes",
        serde_json::json!({ "image": { "reference": "alpine" } }),
    )
    .await;
    let id = body["id"].as_str().unwrap();
    for _ in 0..2 {
        let (status, _) = post_json(
            &app,
            &format!("/api/v1/sandboxes/{id}/exec"),
            serde_json::json!({ "argv": ["echo", "x"] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, limited) = get(&app, &format!("/api/v1/sandboxes/{id}/exec?limit=1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(limited.as_array().unwrap().len(), 1);
    let (status, _) = get(&app, "/api/v1/sandboxes?status=unknown").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn production_authentication_enforces_scope_and_tenant_ownership() {
    let authenticator = auth::Authenticator::new_production();
    authenticator
        .register("owner-key", "tenant-a", auth::Scope::Full)
        .await;
    authenticator
        .register("reader-key", "tenant-b", auth::Scope::Read)
        .await;
    authenticator
        .register("other-key", "tenant-b", auth::Scope::Full)
        .await;
    let mut state = test_state();
    state.auth = Arc::new(authenticator);
    let app = build_router(state);

    let unauthenticated = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/sandboxes")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let readonly_create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/sandboxes")
                .header("authorization", "Bearer reader-key")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image": { "reference": "alpine" },
                        "resources": { "vcpu": 1, "memory_mb": 64, "disk_mb": 512 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(readonly_create.status(), StatusCode::FORBIDDEN);

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/sandboxes")
                .header("authorization", "Bearer owner-key")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image": { "reference": "alpine" },
                        "resources": { "vcpu": 1, "memory_mb": 64, "disk_mb": 512 }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let body = to_bytes(created.into_body(), 1024 * 1024).await.unwrap();
    let id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    for path in ["/health", "/health/live", "/health/ready", "/metrics"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{path} must remain public"
        );
    }

    let cross_tenant = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/sandboxes/{id}"))
                .header("authorization", "Bearer other-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cross_tenant.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cache_miss_create_returns_before_image_work_finishes() {
    let app = app();
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(80),
        post_json(
            &app,
            "/api/v1/sandboxes",
            serde_json::json!({
                "image": { "reference": "missing:latest" },
                "resources": { "vcpu": 1, "memory_mb": 64, "disk_mb": 512 }
            }),
        ),
    )
    .await
    .expect("cache misses must not block the HTTP request");
    assert_eq!(result.0, StatusCode::ACCEPTED);
    assert_eq!(result.1["status"], "starting");
}
#[tokio::test]
async fn cache_miss_create_includes_polling_headers() {
    let app = app();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/sandboxes")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "image": { "reference": "missing:latest" } }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(response.headers()["retry-after"], "1");
    let location = response.headers()["location"].to_str().unwrap().to_string();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let sandbox: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        location,
        format!("/api/v1/sandboxes/{}", sandbox["id"].as_str().unwrap())
    );
}

#[tokio::test]
async fn missing_image_create_eventually_reports_error() {
    let app = app();
    let (_, body) = post_json(
        &app,
        "/api/v1/sandboxes",
        serde_json::json!({ "image": { "reference": "missing:latest" } }),
    )
    .await;
    let id = body["id"].as_str().unwrap();
    let failed = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let (_, sandbox) = get(&app, &format!("/api/v1/sandboxes/{id}")).await;
            if sandbox["status"] == "error" {
                break sandbox;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("missing image must settle as an error");
    assert_eq!(failed["terminal_message"], "image not found");
}

#[tokio::test]
async fn e2b_create_rejects_unknown_and_image_less_templates() {
    let state = test_state();
    state
        .e2b
        .create_template("dev", Some("empty-template"), None, false, None)
        .await
        .unwrap();
    let app = build_router(state);

    let (status, body) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({ "templateID": "plain-template" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unexpected body: {body}");

    let (status, body) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({ "templateID": "empty-template" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unexpected body: {body}");
    assert_eq!(body["error"]["code"], "VALIDATION");
}

#[tokio::test]
async fn e2b_v2_list_filters_metadata_and_reports_running_total() {
    let app = app();
    let (status, created) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({
            "templateID": "alpine:latest",
            "metadata": {"role": "worker"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["state"], "running");

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v2/sandboxes?metadata=role%3Dworker&state=running&limit=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-total-running"], "1");
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let listed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["metadata"]["role"], "worker");
}

#[tokio::test]
async fn e2b_volume_mount_materializes_persisted_files() {
    let state = test_state();
    let volume = state.e2b.create_volume("dev", "data").await.unwrap();
    let volume_id = volume["volumeID"].as_str().unwrap().to_owned();
    state
        .e2b
        .put_volume_file(
            "dev",
            &volume_id,
            "/hello.txt",
            b"from-volume".to_vec(),
            Default::default(),
        )
        .await
        .unwrap();
    let app = build_router(state);
    let (status, created) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({
            "templateID": "alpine:latest",
            "volumeMounts": [{"name": "data", "path": "/mnt/data"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let sandbox_id = created["sandboxID"].as_str().unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files?path=/mnt/data/hello.txt")
                .header("e2b-sandbox-id", sandbox_id)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap(),
        "from-volume"
    );
}
#[tokio::test]
async fn initialization_command_failure_does_not_report_running() {
    let app = app();
    let (status, body) = post_json(
        &app,
        "/api/v1/sandboxes",
        serde_json::json!({
            "image": { "reference": "alpine:latest" },
            "init_command": ["sh", "-c", "exit 7"],
            "resources": { "vcpu": 1, "memory_mb": 64, "disk_mb": 512 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["code"], "VMM");
}

#[tokio::test]
async fn explicit_recovery_reprovisions_failed_sandbox() {
    let state = test_state();
    let store = state.store.clone();
    let app = build_router(state);
    let (_, created) = post_json(
        &app,
        "/api/v1/sandboxes",
        serde_json::json!({ "image": { "reference": "alpine:latest" } }),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    store
        .update_sandbox_status_message(
            &id,
            &clouisle_core::SandboxStatus::Error,
            Some("injected runtime failure"),
        )
        .await
        .unwrap();

    let (status, body) = post_json(
        &app,
        &format!("/api/v1/sandboxes/{id}/recover"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "starting");

    let recovered = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let (_, sandbox) = get(&app, &format!("/api/v1/sandboxes/{id}")).await;
            if sandbox["status"] == "running" {
                break sandbox;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("recovery must finish");
    assert!(recovered["terminal_message"].is_null());
}

#[tokio::test]
async fn reconciliation_marks_unreachable_runtime_error() {
    let state = test_state_with_vmm(Arc::new(TestVmm::with_probe_alive(false)));
    let store = state.store.clone();
    let mut sandbox = clouisle_core::Sandbox::new("dead-runtime".into(), SandboxSpec::default());
    sandbox.status = clouisle_core::SandboxStatus::Running;
    sandbox.vmm_meta.vmm_id = Some("runtime-1".into());
    sandbox.vmm_meta.backend = "test".into();
    store.create_sandbox(&sandbox).await.unwrap();

    clouisle_api::state::reconcile_sandboxes(&state).await;
    let persisted = store.get_sandbox(&sandbox.id).await.unwrap();
    assert_eq!(persisted.status, clouisle_core::SandboxStatus::Error);
    assert_eq!(
        persisted.terminal_message.as_deref(),
        Some("persisted sandbox runtime is not reachable")
    );
}

#[tokio::test]
async fn reconciliation_restarts_on_failure_sandbox() {
    let state = test_state();
    let store = state.store.clone();
    let mut spec = SandboxSpec::default();
    spec.restart_policy = clouisle_core::RestartPolicy::OnFailure;
    let mut sandbox = clouisle_core::Sandbox::new("restart-runtime".into(), spec);
    sandbox.status = clouisle_core::SandboxStatus::Error;
    store.create_sandbox(&sandbox).await.unwrap();

    clouisle_api::state::reconcile_sandboxes(&state).await;
    let recovered = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let sandbox = store.get_sandbox("restart-runtime").await.unwrap();
            if sandbox.status == clouisle_core::SandboxStatus::Running {
                break sandbox;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restart policy must reprovision the sandbox");
    assert!(!recovered.vmm_meta.extra.contains_key("recovery_attempts"));
}
#[tokio::test]
async fn reconciliation_promotes_ready_starting_runtime() {
    let state = test_state();
    let store = state.store.clone();
    let mut sandbox =
        clouisle_core::Sandbox::new("starting-runtime".into(), SandboxSpec::default());
    sandbox.status = clouisle_core::SandboxStatus::Starting;
    sandbox.vmm_meta.vmm_id = Some("runtime-2".into());
    sandbox.vmm_meta.backend = "test".into();
    store.create_sandbox(&sandbox).await.unwrap();

    clouisle_api::state::reconcile_sandboxes(&state).await;
    let persisted = store.get_sandbox(&sandbox.id).await.unwrap();
    assert_eq!(persisted.status, clouisle_core::SandboxStatus::Running);
}

#[tokio::test]
async fn reconciliation_does_not_exceed_recovery_limit() {
    let state = test_state();
    let store = state.store.clone();
    let mut spec = SandboxSpec::default();
    spec.restart_policy = clouisle_core::RestartPolicy::Always;
    let mut sandbox = clouisle_core::Sandbox::new("retry-limit".into(), spec);
    sandbox.status = clouisle_core::SandboxStatus::Error;
    sandbox
        .vmm_meta
        .extra
        .insert("recovery_attempts".into(), "3".into());
    store.create_sandbox(&sandbox).await.unwrap();

    clouisle_api::state::reconcile_sandboxes(&state).await;
    tokio::task::yield_now().await;
    let persisted = store.get_sandbox("retry-limit").await.unwrap();
    assert_eq!(persisted.status, clouisle_core::SandboxStatus::Error);
    assert_eq!(
        persisted.vmm_meta.extra.get("recovery_attempts"),
        Some(&"3".to_string())
    );
}

#[tokio::test]
async fn node_heartbeat_missing_sandbox_marks_error() {
    let state = test_state();
    let store = state.store.clone();
    let mut sandbox =
        clouisle_core::Sandbox::new("heartbeat-missing".into(), SandboxSpec::default());
    sandbox.status = clouisle_core::SandboxStatus::Running;
    sandbox.node_id = Some("node-a".into());
    store.create_sandbox(&sandbox).await.unwrap();

    let node = clouisle_core::RegisteredNode {
        info: clouisle_core::NodeInfo {
            node_id: "node-a".into(),
            hostname: "node-a".into(),
            total_vcpu: 4,
            total_memory_mb: 4096,
            total_disk_mb: 10240,
            kvm_available: true,
            kernel_version: "test".into(),
            firecracker_version: "test".into(),
            labels: Default::default(),
        },
        endpoint: "http://node-a:9090".into(),
        status: clouisle_core::NodeStatus::Ready,
        last_heartbeat_ms: chrono::Utc::now().timestamp_millis(),
        allocated_vcpu: 0,
        allocated_memory_mb: 0,
        running_sandboxes: 0,
        sandbox_ids: Vec::new(),
    };
    assert!(
        clouisle_api::handlers::nodes::upsert_node(axum::extract::State(state), axum::Json(node),)
            .await
            .is_ok()
    );

    let persisted = store.get_sandbox(&sandbox.id).await.unwrap();
    assert_eq!(persisted.status, clouisle_core::SandboxStatus::Error);
    assert_eq!(
        persisted.terminal_message.as_deref(),
        Some("node heartbeat no longer reports this sandbox")
    );
}

#[tokio::test]
async fn readiness_fails_while_draining() {
    let state = test_state();
    state
        .draining
        .store(true, std::sync::atomic::Ordering::Release);
    let app = build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/health/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn image_prefetch_returns_observable_job() {
    let app = app();
    let (status, body) = post_json(
        &app,
        "/api/v1/images/prefetch",
        serde_json::json!({ "image": { "reference": "alpine:latest" } }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let job_id = body["job_id"].as_str().unwrap();
    let (status, body) = get(&app, &format!("/api/v1/images/prefetch/{job_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(matches!(
        body["status"].as_str(),
        Some("queued" | "running" | "succeeded")
    ));
}

#[tokio::test]
async fn image_prefetch_failure_is_observable() {
    let app = app();
    let (_, body) = post_json(
        &app,
        "/api/v1/images/prefetch",
        serde_json::json!({ "image": { "reference": "missing:latest" } }),
    )
    .await;
    let job_id = body["job_id"].as_str().unwrap();
    let failed = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let (_, body) = get(&app, &format!("/api/v1/images/prefetch/{job_id}")).await;
            if body["status"] == "failed" {
                break body;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("prefetch failure must be persisted");
    assert_eq!(failed["error"], "image not found");
}

#[tokio::test]
async fn e2b_platform_sandbox_lifecycle_contract() {
    let app = app();
    let (status, body) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({
            "templateID": "alpine:latest",
            "timeout": 60,
            "metadata": { "owner": "test" },
            "envVars": { "E2B_TEST": "yes" }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = body["sandboxID"].as_str().unwrap().to_string();
    assert_eq!(body["templateID"], "alpine:latest");
    assert!(body["envdVersion"].is_string());

    let (status, connected) = post_json(
        &app,
        &format!("/sandboxes/{id}/connect"),
        serde_json::json!({ "timeout": 60 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(connected["sandboxID"], id);

    let network_update = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/sandboxes/{id}/network"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "allowInternetAccess": true,
                        "allowOut": ["example.com"],
                        "denyOut": ["192.0.2.0/24"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(network_update.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/sandboxes/{id}/pause"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/sandboxes/{id}/resume"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let resumed: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
    assert_eq!(resumed["sandboxID"], id);

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/sandboxes/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn e2b_auto_resume_runs_before_file_access() {
    let app = app();
    let (_, created) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({
            "templateID": "alpine:latest",
            "autoPause": true,
            "autoResume": true,
            "timeout": 60
        }),
    )
    .await;
    let id = created["sandboxID"].as_str().unwrap().to_owned();
    let paused = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/sandboxes/{id}/pause"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::NO_CONTENT);

    let upload = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/files?path=/work/auto-resume.txt")
                .header("e2b-sandbox-id", &id)
                .body(Body::from("resumed"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::OK);
}

#[tokio::test]
async fn e2b_envd_files_and_process_contract() {
    let app = app();
    let (_, created) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({ "templateID": "alpine:latest", "timeout": 60 }),
    )
    .await;
    let id = created["sandboxID"].as_str().unwrap().to_string();
    assert!(created["envdAccessToken"].as_str().is_some());

    let upload = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/files?path=/work/hello.txt")
                .header("e2b-sandbox-id", &id)
                .body(Body::from("hello"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::OK);

    let download = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files?path=/work/hello.txt")
                .header("e2b-sandbox-id", &id)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(to_bytes(download.into_body(), 1024).await.unwrap(), "hello");

    let list = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/filesystem.Filesystem/ListDir")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":"/work"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);

    let process = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/Start")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(r#"{"process":{"cmd":"echo","args":["hello"]}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(process.status(), StatusCode::OK);
    assert_eq!(
        process.headers().get("content-type").unwrap(),
        "application/connect+json"
    );
    let events = String::from_utf8(to_bytes(process.into_body(), 1024).await.unwrap().to_vec())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let pid = events[0]["event"]["start"]["pid"].as_u64().unwrap();
    assert!(pid > 0);
    assert_eq!(events[1]["event"]["data"]["stdout"], "aGVsbG8K");
    assert_eq!(events[2]["event"]["end"]["exitCode"], 0);

    let listed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/List")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let listed_status = listed.status();
    let listed_body = to_bytes(listed.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        listed_status,
        StatusCode::OK,
        "list failed with body: {}",
        String::from_utf8_lossy(&listed_body)
    );
    let processes: serde_json::Value = serde_json::from_slice(&listed_body).unwrap();
    assert!(
        processes["processes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|process| process["pid"].as_u64() == Some(pid))
    );

    let connected = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/Connect")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(format!(r#"{{"process":{{"pid":{pid}}}}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    let connected_status = connected.status();
    let connected_body = to_bytes(connected.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        connected_status,
        StatusCode::OK,
        "connect failed with body: {}",
        String::from_utf8_lossy(&connected_body)
    );
    let connected_events = String::from_utf8(connected_body.to_vec())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        connected_events[0]["event"]["start"]["pid"].as_u64(),
        Some(pid)
    );
    assert_eq!(
        connected_events.last().unwrap()["event"]["end"]["exitCode"],
        0
    );

    let missing = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/Connect")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(r#"{"process":{"pid":99999}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn e2b_cloud_volume_content_roundtrip() {
    let app = app();
    let (status, created) = post_json(&app, "/volumes", serde_json::json!({"name": "data"})).await;
    assert_eq!(status, StatusCode::CREATED);
    let volume_id = created["volumeID"].as_str().unwrap();
    let put = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/volumecontent/{volume_id}/file?path=/hello.txt"))
                .header("content-type", "application/octet-stream")
                .body(Body::from("hello volume"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::OK);
    let get_response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/volumecontent/{volume_id}/file?path=/hello.txt"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(get_response.into_body(), 1024).await.unwrap(),
        "hello volume"
    );
}

#[tokio::test]
async fn e2b_create_accepts_initialization_command() {
    let app = app();
    let (status, body) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({
            "templateID": "alpine:latest",
            "initCommand": ["sh", "-c", "exit 0"],
            "timeout": 60
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["state"], "running");
}

#[tokio::test]
async fn e2b_timeout_refresh_and_legacy_list() {
    let app = app();
    let (_, created) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({ "templateID": "alpine:latest", "timeout": 60 }),
    )
    .await;
    let id = created["sandboxID"].as_str().unwrap().to_owned();

    let timeout = post_json(
        &app,
        &format!("/sandboxes/{id}/timeout"),
        serde_json::json!({ "timeout": 120 }),
    )
    .await;
    assert_eq!(timeout.0, StatusCode::NO_CONTENT);
    let refreshed = post_json(
        &app,
        &format!("/sandboxes/{id}/refresh"),
        serde_json::json!({ "timeout": 300 }),
    )
    .await;
    assert_eq!(refreshed.0, StatusCode::NO_CONTENT);
    let refreshes = post_json(
        &app,
        &format!("/sandboxes/{id}/refreshes"),
        serde_json::json!({ "timeout": 30 }),
    )
    .await;
    assert_eq!(refreshes.0, StatusCode::NO_CONTENT);

    let (_, sandbox) = get(&app, &format!("/sandboxes/{id}")).await;
    let end_at = sandbox["endAt"].as_str().unwrap();
    let parsed = chrono::DateTime::parse_from_rfc3339(end_at).unwrap();
    assert!(parsed > chrono::Utc::now(), "timeout must extend the TTL");

    let (status, body) = get(&app, "/sandboxes").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.as_array()
            .unwrap()
            .iter()
            .any(|sandbox| sandbox["sandboxID"] == id),
        "legacy list must include the running sandbox"
    );
}

#[tokio::test]
async fn e2b_filesystem_rpc_contract() {
    let app = app();
    let (_, created) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({ "templateID": "alpine:latest" }),
    )
    .await;
    let id = created["sandboxID"].as_str().unwrap().to_owned();

    // E2B create 走异步 provision（CI 高负载下可能仍在 Starting）；等待 running。
    for _ in 0..100 {
        let status = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/sandboxes/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(status.into_body(), 1024 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if value["state"] == "running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let make_dir = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/filesystem.Filesystem/MakeDir")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":"/tmp/clouisle-rpc-dir"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(make_dir.status(), StatusCode::OK);
    let make_body = to_bytes(make_dir.into_body(), 1024 * 1024).await.unwrap();
    let make: serde_json::Value = serde_json::from_slice(&make_body).unwrap();
    assert_eq!(make["entry"]["type"], "FILE_TYPE_DIRECTORY");

    let stat = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/filesystem.Filesystem/Stat")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":"/tmp/clouisle-rpc-dir"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stat.status(), StatusCode::OK);
    let stat_body = to_bytes(stat.into_body(), 1024 * 1024).await.unwrap();
    let stat: serde_json::Value = serde_json::from_slice(&stat_body).unwrap();
    assert_eq!(stat["entry"]["type"], "FILE_TYPE_DIRECTORY");
    assert_eq!(stat["entry"]["path"], "/tmp/clouisle-rpc-dir");

    let move_file = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/filesystem.Filesystem/Move")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"source":"/tmp/clouisle-rpc-dir","destination":"/tmp/clouisle-rpc-moved"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(move_file.status(), StatusCode::OK);

    let remove = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/filesystem.Filesystem/Remove")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":"/tmp/clouisle-rpc-moved"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(remove.status(), StatusCode::OK);

    let missing = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/filesystem.Filesystem/Stat")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":"/tmp/clouisle-rpc-missing"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn e2b_filesystem_watcher_rpc_contract() {
    let app = app();
    let (_, created) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({ "templateID": "alpine:latest" }),
    )
    .await;
    let id = created["sandboxID"].as_str().unwrap().to_owned();

    let watcher = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/filesystem.Filesystem/CreateWatcher")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":"/work","recursive":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(watcher.status(), StatusCode::OK);
    let watcher_body = to_bytes(watcher.into_body(), 1024 * 1024).await.unwrap();
    let watcher: serde_json::Value = serde_json::from_slice(&watcher_body).unwrap();
    let watcher_id = watcher["watcherId"].as_str().unwrap().to_owned();

    let events = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/filesystem.Filesystem/GetWatcherEvents")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "watcherId": watcher_id }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(events.status(), StatusCode::OK);
    assert_eq!(events.headers()["content-type"], "application/json");

    let removed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/filesystem.Filesystem/RemoveWatcher")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "watcherId": watcher_id }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn e2b_envd_envs_and_init() {
    let app = app();
    let (_, created) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({
            "templateID": "alpine:latest",
            "envVars": { "INITIAL": "one" }
        }),
    )
    .await;
    let id = created["sandboxID"].as_str().unwrap().to_owned();

    let (status, envs) = get_with_header(&app, "/envs", &id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(envs["INITIAL"], "one");

    let init = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/init")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "envVars": { "REPLACED": "two" } }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(init.status(), StatusCode::NO_CONTENT);

    let (_, envs) = get_with_header(&app, "/envs", &id).await;
    assert_eq!(envs["REPLACED"], "two");
    assert!(envs.get("INITIAL").is_none(), "/init replaces env vars");
}

#[tokio::test]
async fn e2b_cloud_teams_keys_and_templates_http() {
    let app = app();
    let (status, team) = post_json(&app, "/teams", serde_json::json!({"name": "acme"})).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(team["teamID"], "dev");

    let (status, key) = post_json(
        &app,
        "/api-keys",
        serde_json::json!({ "name": "ci-key", "scope": "full" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let raw_key = key["key"].as_str().unwrap().to_owned();
    assert!(raw_key.starts_with("e2b_"));

    // Registering the key disables anonymous dev mode; authenticate explicitly.
    let auth_header = format!("Bearer {raw_key}");

    let (status, token) = post_json_with_auth(
        &app,
        "/access-tokens",
        serde_json::json!({ "name": "sandbox-token" }),
        &auth_header,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(token["token"].as_str().unwrap().starts_with("e2b_access_"));

    let (status, template) = post_json_with_auth(
        &app,
        "/v2/templates",
        serde_json::json!({ "name": "python", "image": "docker.io/library/python:3.12" }),
        &auth_header,
    )
    .await;
    assert!(
        matches!(status, StatusCode::CREATED | StatusCode::ACCEPTED),
        "template create must 201 or 202 (async build), got {status}: {template}"
    );
    let template_id = template["templateID"].as_str().unwrap();

    let (status, listed) = get_with_auth(&app, "/templates", &auth_header).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["templateID"] == template_id),
        "template must be listed"
    );
}

async fn post_json_with_auth(
    app: &Router,
    uri: &str,
    body: serde_json::Value,
    auth: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", auth)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 10 * 1024 * 1024).await.unwrap();
    let val = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, val)
}

async fn get_with_auth(app: &Router, uri: &str, auth: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("authorization", auth)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
async fn e2b_envd_compose_and_unavailable_operations() {
    let app = app();
    let (_, created) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({ "templateID": "alpine:latest" }),
    )
    .await;
    let id = created["sandboxID"].as_str().unwrap().to_owned();

    let upload = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/files?path=/work/part-a.txt")
                .header("e2b-sandbox-id", &id)
                .body(Body::from("part-a"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::OK);

    let compose = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/files/compose")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "source_paths": ["/work/part-a.txt"],
                        "destination": "/work/combined.txt"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(compose.status(), StatusCode::OK);
    let compose_body = to_bytes(compose.into_body(), 1024 * 1024).await.unwrap();
    let compose: serde_json::Value = serde_json::from_slice(&compose_body).unwrap();
    assert_eq!(compose["path"], "/work/combined.txt");

    // Unimplemented envd cgroup/rootfs operations must be explicit 501s,
    // never silent 200s. Process control endpoints are implemented and routed.
    for uri in ["/freeze", "/unfreeze", "/collapse", "/fsfreeze", "/fsthaw"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("e2b-sandbox-id", &id)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_IMPLEMENTED,
            "{uri} must be an explicit 501"
        );
    }
}

#[tokio::test]
async fn e2b_process_stdin_signal_and_pty_roundtrip() {
    let app = app();
    let (_, created) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({ "templateID": "alpine:latest" }),
    )
    .await;
    let id = created["sandboxID"].as_str().unwrap().to_owned();

    // 1. 启动带 stdin 的 cat，随后经 SendInput 写入、CloseStdin 触发 EOF。
    let start = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/Start")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(
                    serde_json::json!({"process":{"cmd":"cat","stdin":true}}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(start.status(), StatusCode::OK);

    let listed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/List")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let listed_body = to_bytes(listed.into_body(), 1024 * 1024).await.unwrap();
    let listed: serde_json::Value = serde_json::from_slice(&listed_body).unwrap();
    let pid = listed["processes"][0]["pid"].as_u64().unwrap();

    let sent = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/SendInput")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(
                    serde_json::json!({
                        "process": {"pid": pid},
                        "input": {"stdin": base64::engine::general_purpose::STANDARD.encode(b"echo-me\n")}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sent.status(), StatusCode::OK);

    let closed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/CloseStdin")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(
                    serde_json::json!({"process": {"pid": pid}}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(closed.status(), StatusCode::OK);

    let body = to_bytes(start.into_body(), 1024 * 1024).await.unwrap();
    let events = String::from_utf8(body.to_vec())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events[0]["event"]["start"]["pid"].as_u64(), Some(pid));
    let echoed = events
        .iter()
        .find_map(|event| event["event"]["data"]["stdout"].as_str())
        .unwrap();
    assert_eq!(
        echoed,
        base64::engine::general_purpose::STANDARD.encode(b"echo-me\n")
    );
    assert_eq!(events.last().unwrap()["event"]["end"]["exitCode"], 0);

    // 2. 信号投递终止 sleep。
    let start = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/Start")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(
                    serde_json::json!({"process":{"cmd":"sleep","args":["60"],"stdin":false}})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let listed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/List")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let listed_body = to_bytes(listed.into_body(), 1024 * 1024).await.unwrap();
    let listed: serde_json::Value = serde_json::from_slice(&listed_body).unwrap();
    let sleep_pid = listed["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|process| process["config"]["cmd"] == "sleep")
        .and_then(|process| process["pid"].as_u64())
        .unwrap();

    let signalled = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/SendSignal")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(
                    serde_json::json!({
                        "process": {"pid": sleep_pid},
                        "signal": "SIGNAL_SIGKILL"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(signalled.status(), StatusCode::OK);
    let body = to_bytes(start.into_body(), 1024 * 1024).await.unwrap();
    let events = String::from_utf8(body.to_vec())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events.last().unwrap()["event"]["end"]["exitCode"], -1);

    // 3. PTY 模式：输出合并编码为 data.pty；Update 调整尺寸成功。
    let start = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/Start")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(
                    serde_json::json!({
                        "process": {
                            "cmd": "sh",
                            "args": ["-c", "echo pty-out; echo pty-err >&2"],
                            "pty": {"cols": 80, "rows": 24}
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let listed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/List")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let listed_body = to_bytes(listed.into_body(), 1024 * 1024).await.unwrap();
    let listed: serde_json::Value = serde_json::from_slice(&listed_body).unwrap();
    let pty_pid = listed["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|process| process["config"]["cmd"] == "sh")
        .and_then(|process| process["pid"].as_u64())
        .unwrap();
    let updated = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/Update")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(
                    serde_json::json!({
                        "process": {"pid": pty_pid},
                        "pty": {"size": {"cols": 132, "rows": 43}}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(updated.status(), StatusCode::OK);
    let body = to_bytes(start.into_body(), 1024 * 1024).await.unwrap();
    let events = String::from_utf8(body.to_vec())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let pty_output = events
        .iter()
        .filter_map(|event| event["event"]["data"]["pty"].as_str())
        .fold(Vec::new(), |mut acc, chunk| {
            if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(chunk) {
                acc.extend_from_slice(&decoded);
            }
            acc
        });
    let text = String::from_utf8_lossy(&pty_output);
    assert!(text.contains("pty-out"), "pty output: {text:?}");
    assert_eq!(events.last().unwrap()["event"]["end"]["exitCode"], 0);
}

#[tokio::test]
async fn e2b_cloud_control_plane_http_crud() {
    let app = app();
    let (status, team) = post_json(&app, "/teams", serde_json::json!({"name": "acme"})).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(team["teamID"], "dev");

    let (status, key) = post_json(
        &app,
        "/api-keys",
        serde_json::json!({ "name": "ci", "scope": "full" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let raw_key = key["key"].as_str().unwrap().to_owned();
    let key_id = key["id"].as_str().unwrap().to_owned();
    let auth = format!("Bearer {raw_key}");

    let (status, listed) = get_with_auth(&app, "/api-keys", &auth).await;
    assert_eq!(status, StatusCode::OK);
    assert!(listed.as_array().unwrap().len() >= 1);

    let (status, _) = patch_with_auth(
        &app,
        &format!("/api-keys/{key_id}"),
        serde_json::json!({ "name": "renamed" }),
        &auth,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = post_json_with_auth(
        &app,
        "/access-tokens",
        serde_json::json!({ "name": "tok" }),
        &auth,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, template) = post_json_with_auth(
        &app,
        "/v2/templates",
        serde_json::json!({ "name": "python", "image": "docker.io/library/python:3.12" }),
        &auth,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let template_id = template["templateID"].as_str().unwrap().to_owned();
    let build_id = template["buildID"].as_str().unwrap().to_owned();

    let (status, _) = post_json_with_auth(
        &app,
        "/templates/tags",
        serde_json::json!({ "templateID": template_id, "tags": ["ml", "py"] }),
        &auth,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, tags) =
        get_with_auth(&app, &format!("/templates/{template_id}/tags"), &auth).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        tags["tags"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("ml"))
    );

    let (status, _) = post_json_with_auth(
        &app,
        &format!("/v2/templates/{template_id}/builds/{build_id}"),
        serde_json::json!({}),
        &auth,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, build) = get_with_auth(
        &app,
        &format!("/templates/{template_id}/builds/{build_id}/status"),
        &auth,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(build["buildID"], build_id);
    let (status, _) = get_with_auth(
        &app,
        &format!("/templates/{template_id}/builds/{build_id}/logs"),
        &auth,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, volume) = post_json_with_auth(
        &app,
        "/volumes",
        serde_json::json!({ "name": "data" }),
        &auth,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let volume_id = volume["volumeID"].as_str().unwrap().to_owned();
    let (status, _) = get_with_auth(&app, &format!("/volumes/{volume_id}"), &auth).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = post_json_with_auth(
        &app,
        "/api/v1/nodes",
        serde_json::json!({
            "info": {"node_id": "node-x", "hostname": "node-x", "total_vcpu": 8,
                     "total_memory_mb": 16384, "total_disk_mb": 102400, "kvm_available": true,
                     "kernel_version": "6.8", "firecracker_version": "1.10.1"},
            "endpoint": "http://node-x:9090", "status": "ready",
            "last_heartbeat_ms": chrono::Utc::now().timestamp_millis() + 60_000, "allocated_vcpu": 0,
            "allocated_memory_mb": 0, "running_sandboxes": 0
        }),
        &auth,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, nodes) = get_with_auth(&app, "/nodes", &auth).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        nodes
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["nodeID"] == "node-x")
    );

    let (status, _) = get_with_auth(&app, &format!("/teams/dev/metrics"), &auth).await;
    assert_eq!(status, StatusCode::OK);

    // 资源删除闭环。
    let (status, _) = delete_with_auth(&app, &format!("/volumes/{volume_id}"), &auth).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = delete_with_auth(&app, &format!("/api-keys/{key_id}"), &auth).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

async fn post_process(
    app: &Router,
    uri: &str,
    sandbox_id: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("e2b-sandbox-id", sandbox_id)
                .header("content-type", "application/connect+json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn post_process_with_auth(
    app: &Router,
    uri: &str,
    sandbox_id: &str,
    body: serde_json::Value,
    auth: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", auth)
                .header("e2b-sandbox-id", sandbox_id)
                .header("content-type", "application/connect+json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn patch_with_auth(
    app: &Router,
    uri: &str,
    body: serde_json::Value,
    auth: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(uri)
                .header("authorization", auth)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn delete_with_auth(app: &Router, uri: &str, auth: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .header("authorization", auth)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
async fn e2b_multitenant_isolation_matrix() {
    let authenticator = auth::Authenticator::new_production();
    authenticator
        .register("tenant-a-key", "tenant-a", auth::Scope::Full)
        .await;
    authenticator
        .register("tenant-b-key", "tenant-b", auth::Scope::Full)
        .await;
    let mut state = test_state();
    state.auth = Arc::new(authenticator);
    let app = build_router(state);

    let (_, created) = post_json_with_auth(
        &app,
        "/sandboxes",
        serde_json::json!({ "templateID": "alpine:latest" }),
        "Bearer tenant-a-key",
    )
    .await;
    let id = created["sandboxID"].as_str().unwrap().to_owned();

    // tenant-b 对 tenant-a 的沙盒：所有资源端点必须 404，不泄露存在性。
    let (status, _) = get_with_auth(&app, &format!("/sandboxes/{id}"), "Bearer tenant-b-key").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) =
        delete_with_auth(&app, &format!("/sandboxes/{id}"), "Bearer tenant-b-key").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = post_process_with_auth(
        &app,
        "/process.Process/Start",
        &id,
        serde_json::json!({"process":{"cmd":"echo","args":["hi"]}}),
        "Bearer tenant-b-key",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "process start must be tenant-scoped"
    );

    let (status, _) = post_process_with_auth(
        &app,
        "/process.Process/List",
        &id,
        serde_json::json!({}),
        "Bearer tenant-b-key",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "process list must be tenant-scoped"
    );

    // envd /files 端点同样按沙盒归属隔离。
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files?path=/work/a.txt")
                .header("authorization", "Bearer tenant-b-key")
                .header("e2b-sandbox-id", &id)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 所有者租户完全可访问。
    let (status, _) = get_with_auth(&app, &format!("/sandboxes/{id}"), "Bearer tenant-a-key").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn e2b_malformed_protocol_inputs() {
    let app = app();
    let (_, created) = post_json(
        &app,
        "/sandboxes",
        serde_json::json!({ "templateID": "alpine:latest" }),
    )
    .await;
    let id = created["sandboxID"].as_str().unwrap().to_owned();

    // 坏 JSON（语法错误）→ 400。
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/Start")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // 空 cmd → 400。
    let (status, _) = post_json(
        &app,
        "/process.Process/Start",
        serde_json::json!({"process":{"cmd":"  ","stdin":false}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // cat 进程：Start 响应是流式 body，只取 status，最后统一读取。
    let cat_start = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/Start")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(
                    serde_json::json!({"process":{"cmd":"cat"}}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cat_start.status(), StatusCode::OK);
    let listed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/process.Process/List")
                .header("e2b-sandbox-id", &id)
                .header("content-type", "application/connect+json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let listed_body = to_bytes(listed.into_body(), 1024 * 1024).await.unwrap();
    let listed: serde_json::Value = serde_json::from_slice(&listed_body).unwrap();
    let pid = listed["processes"][0]["pid"].as_u64().unwrap();
    let (status, _) = post_process(
        &app,
        "/process.Process/SendInput",
        &id,
        serde_json::json!({
            "process": {"pid": pid},
            "input": {"stdin": "!!!not-base64!!!"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 未知信号 → 400。
    let (status, _) = post_process(
        &app,
        "/process.Process/SendSignal",
        &id,
        serde_json::json!({
            "process": {"pid": pid},
            "signal": "SIGNAL_SIGSEGV"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 不存在的进程 → 404。
    let (status, _) = post_process(
        &app,
        "/process.Process/SendSignal",
        &id,
        serde_json::json!({
            "process": {"pid": 999999},
            "signal": 9
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // 非 PTY 进程 Update → 400。
    let (status, _) = post_process(
        &app,
        "/process.Process/Update",
        &id,
        serde_json::json!({
            "process": {"pid": pid},
            "pty": {"size": {"cols": 80, "rows": 24}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 清理：关闭 stdin 结束 cat，避免泄漏宿主进程。
    let (status, _) = post_process(
        &app,
        "/process.Process/CloseStdin",
        &id,
        serde_json::json!({"process": {"pid": pid}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 读取 cat 的流式响应至结束（stdin 已关闭，进程退出）。
    let body = to_bytes(cat_start.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("\"exitCode\""),
        "expected end event, got {text:?}"
    );
}

async fn get_with_header(
    app: &Router,
    uri: &str,
    sandbox_id: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("e2b-sandbox-id", sandbox_id)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}
