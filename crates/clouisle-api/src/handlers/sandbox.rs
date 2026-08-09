//! 沙盒生命周期 handler（FR-01）。

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use clouisle_core::{ClouisleError, Sandbox, SandboxEvent, SandboxSpec, SandboxStatus};

use crate::auth::Principal;
use crate::error::ApiError;
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

fn redact_secrets(mut sandbox: Sandbox) -> Sandbox {
    for secret in &mut sandbox.spec.secrets {
        secret.value = "[REDACTED]".to_string();
    }
    sandbox
}

async fn materialize_secrets(
    conn: &dyn crate::agent::AgentConnection,
    sandbox: &Sandbox,
) -> Result<(), ClouisleError> {
    for secret in &sandbox.spec.secrets {
        conn.write_file(
            &format!("/run/secrets/{}", secret.name),
            bytes::Bytes::copy_from_slice(secret.value.as_bytes()),
            0o600,
        )
        .await?;
    }
    Ok(())
}

pub async fn create_sandbox(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(mut req): Json<CreateSandboxRequest>,
) -> Result<impl IntoResponse, ApiError> {
    req.spec.tenant_id = Some(principal.tenant_id.clone());
    // 1. 校验 spec
    if let Err(errors) = req.spec.validate() {
        return Err(ApiError(ClouisleError::validation(format!(
            "invalid spec: {errors:?}"
        ))));
    }

    // Remote nodes own their own pools; local VMMs retain permits here.
    let reservation = if state.manage_resources {
        Some(state.pool.admit(&req.spec).await?)
    } else {
        None
    };

    // 3. 建沙盒记录
    let id = uuid::Uuid::now_v7().to_string();
    let mut sandbox = Sandbox::new(id.clone(), req.spec.clone());
    sandbox.transition(SandboxEvent::Start)?;
    state.store.create_sandbox(&sandbox).await?;
    tracing::info!(sandbox_id = %id, "sandbox admitted");

    // 4. 创建管理面 TAP；network.enabled=false 仍需 TCP agent 通道。
    #[cfg(target_os = "linux")]
    if state.manage_network
        && let Err(error) = state.firewall.create_network(&id).await
    {
        state
            .store
            .update_sandbox_status(&id, &SandboxStatus::Error)
            .await
            .ok();
        return Err(error.into());
    }

    // 5. VMM create + start
    let start = std::time::Instant::now();
    let handle = match state.vmm.create(&id, &req.spec).await {
        Ok(h) => h,
        Err(e) => {
            state
                .store
                .update_sandbox_status(&id, &SandboxStatus::Error)
                .await
                .ok();
            #[cfg(target_os = "linux")]
            if state.manage_network {
                let _ = state.firewall.teardown_sandbox_network(&id).await;
            }
            return Err(e.into());
        }
    };

    // 回填 vmm_meta
    let vmm_meta = clouisle_core::VmmMeta {
        backend: handle.backend.clone(),
        pid: handle.pid,
        api_socket: handle.api_socket.clone(),
        vsock_socket: handle.vsock_socket.clone(),
        vsock_cid: handle.vsock_cid,
        vmm_id: Some(handle.id.clone()),
        extra: Default::default(),
    };
    sandbox.vmm_meta = vmm_meta.clone();
    state
        .store
        .update_sandbox_vmm_meta(&id, &vmm_meta)
        .await
        .ok();

    if let Err(e) = state.vmm.start(&handle).await {
        state
            .store
            .update_sandbox_status(&id, &SandboxStatus::Error)
            .await
            .ok();
        let _ = state.vmm.stop(&handle, clouisle_vmm::StopMode::Force).await;
        #[cfg(target_os = "linux")]
        if state.manage_network {
            let _ = state.firewall.teardown_sandbox_network(&id).await;
        }
        return Err(e.into());
    }

    // 6. nftables 以 allowlist 控制出站；禁网时 allowlist 为空。
    #[cfg(target_os = "linux")]
    if state.manage_network {
        let gateway = format!("{}/30", clouisle_net::netns::gateway_ip(&id));
        let allow = if req.spec.network.enabled {
            req.spec.network.allow_egress.clone()
        } else {
            Vec::new()
        };
        if let Err(error) = state
            .firewall
            .setup_sandbox_network(&id, &gateway, &allow)
            .await
        {
            tracing::error!(sandbox_id = %id, error = %error, "firewall setup failed");
            state
                .store
                .update_sandbox_status(&id, &SandboxStatus::Error)
                .await
                .ok();
            let _ = state.vmm.stop(&handle, clouisle_vmm::StopMode::Force).await;
            let _ = state.firewall.teardown_sandbox_network(&id).await;
            return Err(error.into());
        }
    }

    // 7. 等 agent hello（Start → Running）
    let start_timeout = tokio::time::Duration::from_secs(req.spec.start_timeout_secs);
    let hello =
        tokio::time::timeout(start_timeout, state.agent.connect_and_hello(&handle, &id)).await;
    match hello {
        Ok(Ok(conn)) => {
            if let Err(error) = materialize_secrets(conn.as_ref(), &sandbox).await {
                state
                    .store
                    .update_sandbox_status(&id, &SandboxStatus::Error)
                    .await
                    .ok();
                let _ = state.vmm.stop(&handle, clouisle_vmm::StopMode::Force).await;
                #[cfg(target_os = "linux")]
                let _ = state.firewall.teardown_sandbox_network(&id).await;
                return Err(error.into());
            }
            sandbox.transition(SandboxEvent::AgentHello)?;
            state
                .store
                .update_sandbox_status(&id, &SandboxStatus::Running)
                .await?;
            state
                .store
                .update_sandbox_expiry(&id, sandbox.expires_at)
                .await?;
            let dur = start.elapsed().as_millis() as f64;
            clouisle_obs::metrics::record_sandbox_create(
                clouisle_obs::metrics::SloBucket::ColdStart,
                dur,
            );
            tracing::info!(sandbox_id = %id, duration_ms = dur, "sandbox running");
            if let Some(reservation) = reservation {
                state
                    .reservations
                    .lock()
                    .await
                    .insert(id.clone(), reservation);
            }
            Ok((StatusCode::CREATED, Json(redact_secrets(sandbox))))
        }
        Ok(Err(e)) => {
            state
                .store
                .update_sandbox_status(&id, &SandboxStatus::Error)
                .await
                .ok();
            let _ = state.vmm.stop(&handle, clouisle_vmm::StopMode::Force).await;
            #[cfg(target_os = "linux")]
            let _ = state.firewall.teardown_sandbox_network(&id).await;
            Err(e.into())
        }
        Err(_elapsed) => {
            state
                .store
                .update_sandbox_status(&id, &SandboxStatus::Error)
                .await
                .ok();
            let _ = state.vmm.stop(&handle, clouisle_vmm::StopMode::Force).await;
            #[cfg(target_os = "linux")]
            let _ = state.firewall.teardown_sandbox_network(&id).await;
            Err(ApiError(ClouisleError::timeout(format!(
                "sandbox {id} agent hello timeout after {}s",
                req.spec.start_timeout_secs
            ))))
        }
    }
}

pub async fn get_sandbox(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<Sandbox>, ApiError> {
    let sb = state.store.get_sandbox(&id).await?;
    state.auth.require_tenant(&principal, &sb)?;
    Ok(Json(redact_secrets(sb)))
}

pub async fn list_sandboxes(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    let status = match q.status.as_deref() {
        None => None,
        Some("pending") => Some(SandboxStatus::Pending),
        Some("starting") => Some(SandboxStatus::Starting),
        Some("running") => Some(SandboxStatus::Running),
        Some("stopping") => Some(SandboxStatus::Stopping),
        Some("stopped") => Some(SandboxStatus::Stopped),
        Some("error") => Some(SandboxStatus::Error),
        Some(value) => {
            return Err(ApiError(ClouisleError::validation(format!(
                "unknown sandbox status: {value}"
            ))));
        }
    };

    let all = state
        .store
        .list_sandboxes(status)
        .await?
        .into_iter()
        .filter(|sandbox| sandbox.spec.tenant_id.as_deref() == Some(principal.tenant_id.as_str()))
        .map(redact_secrets)
        .collect::<Vec<_>>();
    let total = all.len();
    let offset = q.offset.unwrap_or(0).min(all.len());
    let limit = q.limit.unwrap_or(100).max(1);
    let items = all.into_iter().skip(offset).take(limit).collect();
    Ok(Json(ListResponse { items, total }))
}

pub async fn delete_sandbox(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let sb = state.store.get_sandbox(&id).await?;
    state.auth.require_tenant(&principal, &sb)?;

    // The VMM owner may be remote, so PID absence must not skip deletion.
    let handle = clouisle_vmm::VmHandle {
        id: sb.vmm_meta.vmm_id.clone().unwrap_or_else(|| id.clone()),
        backend: sb.vmm_meta.backend.clone(),
        pid: sb.vmm_meta.pid,
        api_socket: sb.vmm_meta.api_socket.clone(),
        vsock_socket: sb.vmm_meta.vsock_socket.clone(),
        vsock_cid: sb.vmm_meta.vsock_cid,
    };
    if sb.vmm_meta.vmm_id.is_some() {
        let _ = state.vmm.stop(&handle, clouisle_vmm::StopMode::Force).await;
    }

    state.store.delete_sandbox(&id).await?;
    state.reservations.lock().await.remove(&id);
    // 清理网络隔离（Linux only）
    #[cfg(target_os = "linux")]
    if state.manage_network
        && let Err(error) = state.firewall.teardown_sandbox_network(&id).await
    {
        tracing::warn!(sandbox_id = %id, error = %error, "firewall teardown failed");
    }
    Ok(StatusCode::NO_CONTENT)
}
