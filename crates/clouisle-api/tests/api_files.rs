//! 文件传输 API 集成测试（FR-07）。

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};

use clouisle_api::{AppState, agent, auth, build_router};
use clouisle_core::Result;
use clouisle_scheduler::ResourcePool;
use clouisle_store::InMemoryStore;
use clouisle_vmm::{
    SnapshotKind, SnapshotPaths, StopMode, VmHandle, VmStats, Vmm, VmmCapabilities,
};
use tower::ServiceExt;

#[derive(Clone)]
struct TestVmm(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait]
impl Vmm for TestVmm {
    async fn create(&self, _: &str, _: &clouisle_core::SandboxSpec) -> Result<VmHandle> {
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
    async fn start(&self, _: &VmHandle) -> Result<()> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn pause(&self, _: &VmHandle) -> Result<()> {
        Ok(())
    }
    async fn resume(&self, _: &VmHandle) -> Result<()> {
        Ok(())
    }
    async fn snapshot(&self, _: &VmHandle, _k: SnapshotKind, _o: &SnapshotPaths) -> Result<()> {
        Ok(())
    }
    async fn restore(
        &self,
        _: &str,
        _: &clouisle_core::SandboxSpec,
        _: &SnapshotPaths,
    ) -> Result<VmHandle> {
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
    async fn stop(&self, _: &VmHandle, _m: StopMode) -> Result<()> {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn stats(&self, _: &VmHandle) -> Result<VmStats> {
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

fn app() -> Router {
    let store = Arc::new(InMemoryStore::new());
    let pool = Arc::new(ResourcePool::new(AppState::host_capacity(), 200));
    let vmm: Arc<dyn Vmm> = Arc::new(TestVmm(Arc::new(std::sync::atomic::AtomicUsize::new(0))));
    let agent_conn: Arc<dyn agent::AgentConnector> = Arc::new(agent::MockAgentConnector);
    build_router(AppState {
        e2b: Arc::new(clouisle_api::E2bControlPlane::new()),
        store,
        warm_pool: Arc::new(clouisle_pool::Pool::new(0, 60, vmm.clone())),
        vmm,
        warm_slots: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        pool,
        reservations: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        image_jobs: Arc::new(clouisle_api::ImageJobRegistry::new()),
        e2b_tokens: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        processes: Arc::new(clouisle_api::state::ProcessRegistry::default()),
        snapshots: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        subnet_alloc: clouisle_net::netns::SubnetAllocator::new(),
        draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        provisioning: Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new())),
        manage_resources: true,
        agent: agent_conn,
        auth: Arc::new(auth::Authenticator::new()),
        #[cfg(target_os = "linux")]
        firewall: Arc::new(clouisle_net::FirewallManager::new()),
        #[cfg(target_os = "linux")]
        manage_network: false,
        version: "test",
    })
}

/// 使用唯一的测试 ID 作为沙盒 ID，避免并发测试在 `/tmp/clouisle-mock-fs/` 下的冲突。
#[allow(dead_code)]
fn unique_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

async fn create_sandbox(app: &Router, resource_name: &str) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/sandboxes")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "image": {"reference": resource_name},
                        "resources": {"vcpu": 1, "memory_mb": 64, "disk_mb": 512}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn upload_download_roundtrip() {
    let app = app();
    let id = create_sandbox(&app, "alpine").await;

    // 上传
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/v1/sandboxes/{id}/files/upload?path=/work/a.txt"
                ))
                .header("content-type", "application/octet-stream")
                .body(Body::from("hello file content"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 下载回读
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/v1/sandboxes/{id}/files/download?path=/work/a.txt"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(bytes.as_ref(), b"hello file content");
}

#[tokio::test]
async fn list_files_after_upload() {
    let app = app();
    let id = create_sandbox(&app, "alpine").await;

    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/v1/sandboxes/{id}/files/upload?path=/work/a.txt"
                ))
                .body(Body::from("x"))
                .unwrap(),
        )
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/sandboxes/{id}/files/ls?path=/work"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let names: Vec<String> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"a.txt".to_string()));
}

#[tokio::test]
async fn download_missing_file_404() {
    let app = app();
    let id = create_sandbox(&app, "alpine").await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/v1/sandboxes/{id}/files/download?path=/work/missing.txt"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn path_traversal_rejected() {
    let app = app();
    let id = create_sandbox(&app, "alpine").await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/v1/sandboxes/{id}/files/upload?path=/work/../../etc/passwd"
                ))
                .body(Body::from("evil"))
                .unwrap(),
        )
        .await
        .unwrap();
    // 路径穿越应被拒绝（400 或 404），且不写入宿主 /etc/passwd
    assert_ne!(resp.status(), StatusCode::OK);
}
