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
        Self {
            running: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Vmm for TestVmm {
    async fn create(&self, _spec: &SandboxSpec) -> Result<VmHandle> {
        Ok(VmHandle {
            id: uuid::Uuid::now_v7().to_string(),
            backend: "test".into(),
            pid: None,
            api_socket: None,
            vsock_socket: None,
        })
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
    async fn restore(&self, _s: &SandboxSpec, _f: &SnapshotPaths) -> Result<VmHandle> {
        Ok(VmHandle {
            id: uuid::Uuid::now_v7().to_string(),
            backend: "test".into(),
            pid: None,
            api_socket: None,
            vsock_socket: None,
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
        agent: agent_conn,
        auth: Arc::new(auth::Authenticator::new()),
        #[cfg(target_os = "linux")]
        firewall: Arc::new(clouisle_net::FirewallManager::new()),
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
    assert_eq!(created, 20, "all 20 should create");
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
