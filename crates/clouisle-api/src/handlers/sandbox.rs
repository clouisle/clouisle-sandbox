//! 沙盒生命周期 handler（FR-01）。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use clouisle_core::{ClouisleError, Sandbox, SandboxEvent, SandboxSpec, SandboxStatus};
use tracing::{info, warn};
use tracing;

use crate::error::{validation_errors, ApiError};
use crate::state::AppState;

/// `POST /api/v1/sandboxes` 请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct CreateSandboxRequest {
    #[serde(flatten)]
    pub spec: SandboxSpec,
    /// 同步等待就绪（默认 true）。false 时立即返回 Pending（Phase 2 镜像异步）。
    #[serde(default = "default_sync")]
    pub sync: bool,
}

fn default_sync() -> bool {
    true
}

/// 列表查询参数。
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub status: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub items: Vec<Sandbox>,
    pub total: usize,
}

/// 创建沙盒。
pub async fn create_sandbox(
    State(state): State<AppState>,
    Json(req): Json<CreateSandboxRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // 1. 校验 spec
    if let Err(errors) = req.spec.validate() {
        return Err(ApiError(ClouisleError::validation(format!(
            "invalid spec: {errors:?}"
        ))));
    }

    // 2. 准入控制（预留资源）
    let _reservation = state.pool.admit(&req.spec).await?;

    // 3. 建沙盒记录
    let id = uuid::Uuid::now_v7().to_string();
    let mut sandbox = Sandbox::new(id.clone(), req.spec.clone());
    sandbox.transition(SandboxEvent::Start)?;
    state.store.create_sandbox(&sandbox).await?;
    tracing::info!(sandbox_id = %id, "sandbox admitted");

    // 4. VMM create + start
    let start = std::time::Instant::now();
    let handle = match state.vmm.create(&req.spec).await {
        Ok(h) => h,
        Err(e) => {
            state
                .store
                .update_sandbox_status(&id, &SandboxStatus::Error)
                .await
                .ok();
            return Err(e.into());
        }
    };

    // 回填 vmm_meta
    let vmm_meta = clouisle_core::VmmMeta {
        backend: handle.backend.clone(),
        pid: handle.pid,
        api_socket: handle.api_socket.clone(),
        vsock_socket: handle.vsock_socket.clone(),
        vmm_id: Some(handle.id.clone()),
        extra: Default::default(),
    };
    sandbox.vmm_meta = vmm_meta.clone();
    state.store.update_sandbox_vmm_meta(&id, &vmm_meta).await.ok();

    if let Err(e) = state.vmm.start(&handle).await {
        state
            .store
            .update_sandbox_status(&id, &SandboxStatus::Error)
            .await
            .ok();
        return Err(e.into());
    }

    // 5. 配置网络隔离（Linux only）
    #[cfg(target_os = "linux")]
    {
        let veth_host_ip = format!("192.168.{}.1/30", (id.as_bytes()[0] as u16) % 255);
        let allow = req.spec.network.allow_egress.clone();
        if let Err(e) = state.firewall.setup_sandbox_network(&id, &veth_host_ip, &allow).await {
            warn!(sandbox_id = %id, error = %e, "firewall setup failed (non-fatal)");
        }
    }

    // 6. 等 agent hello（Start → Running）
    let start_timeout = tokio::time::Duration::from_secs(req.spec.start_timeout_secs);
    let hello = tokio::time::timeout(start_timeout, state.agent.connect_and_hello(&handle, &id)).await;

    match hello {
        Ok(Ok(_conn)) => {
            sandbox.transition(SandboxEvent::AgentHello)?;
            state
                .store
                .update_sandbox_status(&id, &SandboxStatus::Running)
                .await?;
            let dur = start.elapsed().as_millis() as f64;
            clouisle_obs::metrics::record_sandbox_create(
                clouisle_obs::metrics::SloBucket::ColdStart,
                dur,
            );
            tracing::info!(sandbox_id = %id, duration_ms = dur, "sandbox running");
            Ok((StatusCode::CREATED, Json(sandbox)))
        }
        Ok(Err(e)) => {
            state
                .store
                .update_sandbox_status(&id, &SandboxStatus::Error)
                .await
                .ok();
            let _ = state.vmm.stop(&handle, clouisle_vmm::StopMode::Force).await;
            Err(e.into())
        }
        Err(_elapsed) => {
            state
                .store
                .update_sandbox_status(&id, &SandboxStatus::Error)
                .await
                .ok();
            let _ = state.vmm.stop(&handle, clouisle_vmm::StopMode::Force).await;
            Err(ApiError(ClouisleError::timeout(format!(
                "sandbox {id} agent hello timeout after {}s",
                req.spec.start_timeout_secs
            ))))
        }
    }
}

/// `GET /api/v1/sandboxes/{id}`
pub async fn get_sandbox(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Sandbox>, ApiError> {
    let sb = state.store.get_sandbox(&id).await?;
    Ok(Json(sb))
}

/// `GET /api/v1/sandboxes`
pub async fn list_sandboxes(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    let status = q.status.as_deref().map(|s| match s {
        "pending" => SandboxStatus::Pending,
        "starting" => SandboxStatus::Starting,
        "running" => SandboxStatus::Running,
        "stopping" => SandboxStatus::Stopping,
        "stopped" => SandboxStatus::Stopped,
        "error" => SandboxStatus::Error,
        _ => SandboxStatus::Pending, // 未知过滤条件按空处理由调用方感知
    });

    let all = state.store.list_sandboxes(status).await?;
    let total = all.len();
    let offset = q.offset.unwrap_or(0).min(all.len());
    let limit = q.limit.unwrap_or(100).max(1);
    let items = all.into_iter().skip(offset).take(limit).collect();
    Ok(Json(ListResponse { items, total }))
}

/// `DELETE /api/v1/sandboxes/{id}`
pub async fn delete_sandbox(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let sb = state.store.get_sandbox(&id).await?;

    // 若 VMM 在跑，先停
    if let Some(pid) = sb.vmm_meta.pid {
        let handle = clouisle_vmm::VmHandle {
            id: sb.vmm_meta.vmm_id.clone().unwrap_or_else(|| id.clone()),
            backend: sb.vmm_meta.backend.clone(),
            pid: Some(pid),
            api_socket: sb.vmm_meta.api_socket.clone(),
            vsock_socket: sb.vmm_meta.vsock_socket.clone(),
        };
        let _ = state.vmm.stop(&handle, clouisle_vmm::StopMode::Force).await;
    }

    state.store.delete_sandbox(&id).await?;
    // 清理网络隔离（Linux only）
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = state.firewall.teardown_sandbox_network(&id).await {
            warn!(sandbox_id = %id, error = %e, "firewall teardown failed");
        }
    }
    Ok(StatusCode::NO_CONTENT)
}