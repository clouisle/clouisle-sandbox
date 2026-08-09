//! 沙盒生命周期 handler（FR-01）。

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderValue, StatusCode};
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
) -> Result<axum::response::Response, ApiError> {
    req.spec.tenant_id = Some(principal.tenant_id.clone());
    if let Err(errors) = req.spec.validate() {
        return Err(ApiError(ClouisleError::validation(format!(
            "invalid spec: {errors:?}"
        ))));
    }

    let reservation = if state.manage_resources {
        Some(state.pool.admit(&req.spec).await?)
    } else {
        None
    };
    let id = uuid::Uuid::now_v7().to_string();
    let mut sandbox = Sandbox::new(id.clone(), req.spec.clone());
    sandbox.transition(SandboxEvent::Start)?;
    state.store.create_sandbox(&sandbox).await?;
    tracing::info!(sandbox_id = %id, "sandbox admitted");

    // A cache miss is always asynchronous. This keeps registry/network work
    // out of the request even when the caller asked for a synchronous create.
    let image_cached = match state.vmm.image_cache_hit(&req.spec).await {
        Ok(hit) => hit,
        Err(error) => {
            tracing::warn!(sandbox_id = %id, %error, "image cache probe failed; queueing create");
            false
        }
    };
    let wait_for_ready = req.sync && image_cached;
    let provision = provision_sandbox(state.clone(), sandbox.clone(), reservation);
    if wait_for_ready {
        let running = provision.await.map_err(ApiError)?;
        return Ok((StatusCode::CREATED, Json(redact_secrets(running))).into_response());
    }

    tokio::spawn(async move {
        if let Err(error) = provision.await {
            tracing::error!(sandbox_id = %id, %error, "asynchronous sandbox provisioning failed");
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        [(
            axum::http::header::RETRY_AFTER,
            HeaderValue::from_static("1"),
        )],
        Json(redact_secrets(sandbox)),
    )
    .into_response())
}

async fn provision_sandbox(
    state: AppState,
    mut sandbox: Sandbox,
    reservation: Option<clouisle_scheduler::Reservation>,
) -> Result<Sandbox, ClouisleError> {
    let id = sandbox.id.clone();
    #[cfg(target_os = "linux")]
    if state.manage_network
        && let Err(error) = state.firewall.create_network(&id).await
    {
        mark_failed(&state, &id, &error, None).await;
        return Err(error);
    }

    let handle = match state.vmm.create(&id, &sandbox.spec).await {
        Ok(handle) => handle,
        Err(error) => {
            mark_failed(&state, &id, &error, None).await;
            return Err(error);
        }
    };
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
    if let Err(error) = state.store.update_sandbox_vmm_meta(&id, &vmm_meta).await {
        let error: ClouisleError = error.into();
        mark_failed(&state, &id, &error, Some(&handle)).await;
        return Err(error);
    }

    if let Err(error) = state.vmm.start(&handle).await {
        mark_failed(&state, &id, &error, Some(&handle)).await;
        return Err(error);
    }

    #[cfg(target_os = "linux")]
    if state.manage_network {
        let gateway = format!("{}/30", clouisle_net::netns::gateway_ip(&id));
        let allow = if sandbox.spec.network.enabled {
            sandbox.spec.network.allow_egress.clone()
        } else {
            Vec::new()
        };
        if let Err(error) = state
            .firewall
            .setup_sandbox_network(&id, &gateway, &allow)
            .await
        {
            mark_failed(&state, &id, &error, Some(&handle)).await;
            return Err(error);
        }
    }

    let start_timeout = tokio::time::Duration::from_secs(sandbox.spec.start_timeout_secs);
    let hello = tokio::time::timeout(
        start_timeout,
        state.agent.connect_and_hello(&handle, &id),
    )
    .await;
    let conn = match hello {
        Ok(Ok(conn)) => conn,
        Ok(Err(error)) => {
            mark_failed(&state, &id, &error, Some(&handle)).await;
            return Err(error);
        }
        Err(_) => {
            let error = ClouisleError::timeout(format!(
                "sandbox {id} agent hello timeout after {}s",
                sandbox.spec.start_timeout_secs
            ));
            mark_failed(&state, &id, &error, Some(&handle)).await;
            return Err(error);
        }
    };

    if let Err(error) = materialize_secrets(conn.as_ref(), &sandbox).await {
        mark_failed(&state, &id, &error, Some(&handle)).await;
        return Err(error);
    }
    if !sandbox.spec.init_command.is_empty() {
        let result = match conn
            .exec(
                sandbox.spec.init_command.clone(),
                sandbox.spec.env.clone(),
                None,
                sandbox.spec.init_timeout_ms,
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                mark_failed(&state, &id, &error, Some(&handle)).await;
                return Err(error);
            }
        };
        if result.exit_code != 0 {
            let stderr = String::from_utf8_lossy(&result.stderr);
            let error = ClouisleError::new(
                clouisle_core::ErrorKind::Vmm,
                format!(
                    "initialization command exited with code {}{}",
                    result.exit_code,
                    if stderr.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", stderr.trim())
                    }
                ),
            );
            mark_failed(&state, &id, &error, Some(&handle)).await;
            return Err(error);
        }
    }

    sandbox.transition(SandboxEvent::AgentHello)?;
    if let Err(error) = state
        .store
        .update_sandbox_status_message(&id, &SandboxStatus::Running, None)
        .await
    {
        let error: ClouisleError = error.into();
        mark_failed(&state, &id, &error, Some(&handle)).await;
        return Err(error);
    }
    state
        .store
        .update_sandbox_expiry(&id, sandbox.expires_at)
        .await?;
    if let Some(reservation) = reservation {
        state.reservations.lock().await.insert(id, reservation);
    }
    Ok(sandbox)
}

async fn mark_failed(
    state: &AppState,
    id: &str,
    error: &ClouisleError,
    handle: Option<&clouisle_vmm::VmHandle>,
) {
    if let Err(store_error) = state
        .store
        .update_sandbox_status_message(id, &SandboxStatus::Error, Some(&error.message))
        .await
    {
        tracing::error!(sandbox_id = %id, %store_error, "failed to persist sandbox error");
    }
    if let Some(handle) = handle {
        let _ = state.vmm.stop(handle, clouisle_vmm::StopMode::Force).await;
    }
    #[cfg(target_os = "linux")]
    if state.manage_network {
        let _ = state.firewall.teardown_sandbox_network(id).await;
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
