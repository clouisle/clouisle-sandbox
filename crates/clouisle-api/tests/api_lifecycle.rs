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
}

impl TestVmm {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for TestVmm {
    fn default() -> Self {
        Self {
            running: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Vmm for TestVmm {
    async fn create(&self, _: &str, spec: &SandboxSpec) -> Result<VmHandle> {
        if spec.image.reference == "missing:latest" {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        Ok(VmHandle {
            id: uuid::Uuid::now_v7().to_string(),
            backend: "test".into(),
            pid: None,
            api_socket: None,
            vsock_socket: None,
            vsock_cid: None,
        })
    }
    async fn image_cache_hit(&self, spec: &SandboxSpec) -> Result<bool> {
        Ok(spec.image.reference != "missing:latest")
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
            pid: None,
            api_socket: None,
            vsock_socket: None,
            vsock_cid: None,
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
    let store = Arc::new(InMemoryStore::new());
    let pool = Arc::new(ResourcePool::new(AppState::host_capacity(), 200));
    let vmm: Arc<dyn Vmm> = Arc::new(TestVmm::new());
    let agent_conn: Arc<dyn agent::AgentConnector> = Arc::new(agent::MockAgentConnector);
    AppState {
        store,
        vmm,
        pool,
        reservations: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        image_jobs: Arc::new(clouisle_api::ImageJobRegistry::new()),
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
    assert!(matches!(body["status"].as_str(), Some("queued" | "running" | "succeeded")));
}
