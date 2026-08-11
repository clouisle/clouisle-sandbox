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

async fn materialize_volume_mounts(
    state: &AppState,
    conn: &dyn crate::agent::AgentConnection,
    sandbox: &Sandbox,
) -> Result<(), ClouisleError> {
    for mount in &sandbox.spec.volume_mounts {
        crate::handlers::files::validate_path(&mount.target)?;
        let volume = state
            .e2b
            .volume_by_name(
                sandbox.spec.tenant_id.as_deref().unwrap_or_default(),
                &mount.name,
            )
            .await
            .map_err(|error| ClouisleError::not_found(error.to_string()))?;
        for (path, file) in volume.files {
            let relative = path.trim_start_matches('/');
            let target = if relative.is_empty() {
                mount.target.clone()
            } else {
                format!("{}/{}", mount.target.trim_end_matches('/'), relative)
            };
            crate::handlers::files::validate_path(&target)?;
            conn.write_file(&target, bytes::Bytes::from(file.content), file.mode)
                .await?;
        }
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
    // docker-dev 后端专属预检（allowlist/资源/restart 限制）。
    #[cfg(target_os = "linux")]
    if !state.vmm.capabilities().vsock && !state.vmm.capabilities().snapshot {
        clouisle_vmm::docker_dev::validate_dev_spec(&req.spec).map_err(ApiError)?;
    }

    let reservation = if state.manage_resources {
        Some(state.pool.admit(&req.spec).await?)
    } else {
        None
    };
    let id = uuid::Uuid::now_v7().to_string();
    let warm_slot = state.warm_pool.acquire(&req.spec).await;
    let mut sandbox = Sandbox::new(id.clone(), req.spec.clone());
    sandbox.transition(SandboxEvent::Start)?;
    if let Err(error) = state.store.create_sandbox(&sandbox).await {
        if let Some(slot) = warm_slot {
            let _ = state.warm_pool.discard(slot).await;
        }
        return Err(error.into());
    }
    if let Some(slot) = warm_slot.as_ref() {
        state
            .warm_slots
            .lock()
            .await
            .insert(id.clone(), slot.clone());
    }
    tracing::info!(sandbox_id = %id, warm = warm_slot.is_some(), "sandbox admitted");

    // A cache miss is always asynchronous. This keeps registry/network work
    // out of the request even when the caller asked for a synchronous create.
    let image_cached = if warm_slot.is_some() {
        true
    } else {
        match state.vmm.image_cache_hit(&req.spec).await {
            Ok(hit) => hit,
            Err(error) => {
                tracing::warn!(sandbox_id = %id, %error, "image cache probe failed; queueing create");
                false
            }
        }
    };
    let wait_for_ready = req.sync && image_cached;
    let provision = run_provision_with_slot(state.clone(), sandbox.clone(), reservation, warm_slot);
    if wait_for_ready {
        let running = provision.await.map_err(ApiError)?;
        return Ok((StatusCode::CREATED, Json(redact_secrets(running))).into_response());
    }

    let log_id = id.clone();
    tokio::spawn(async move {
        if let Err(error) = provision.await {
            tracing::error!(sandbox_id = %log_id, %error, "asynchronous sandbox provisioning failed");
        }
    });
    let location = format!("/api/v1/sandboxes/{id}")
        .parse::<HeaderValue>()
        .expect("sandbox UUID forms a valid location header");
    Ok((
        StatusCode::ACCEPTED,
        [
            (
                axum::http::header::RETRY_AFTER,
                HeaderValue::from_static("1"),
            ),
            (axum::http::header::LOCATION, location),
        ],
        Json(redact_secrets(sandbox)),
    )
        .into_response())
}

pub async fn run_provision(
    state: AppState,
    sandbox: Sandbox,
    reservation: Option<clouisle_scheduler::Reservation>,
) -> Result<Sandbox, ClouisleError> {
    run_provision_with_slot(state, sandbox, reservation, None).await
}

pub async fn run_provision_with_slot(
    state: AppState,
    sandbox: Sandbox,
    reservation: Option<clouisle_scheduler::Reservation>,
    warm_slot: Option<clouisle_pool::PoolSlot>,
) -> Result<Sandbox, ClouisleError> {
    run_provision_with_slot_and_snapshot(state, sandbox, reservation, warm_slot, None, None).await
}

pub async fn run_provision_from_snapshot(
    state: AppState,
    sandbox: Sandbox,
    reservation: Option<clouisle_scheduler::Reservation>,
    snapshot: clouisle_vmm::SnapshotPaths,
    subnet: Option<(u16, u16)>,
) -> Result<Sandbox, ClouisleError> {
    run_provision_with_slot_and_snapshot(state, sandbox, reservation, None, Some(snapshot), subnet)
        .await
}

async fn run_provision_with_slot_and_snapshot(
    state: AppState,
    sandbox: Sandbox,
    reservation: Option<clouisle_scheduler::Reservation>,
    warm_slot: Option<clouisle_pool::PoolSlot>,
    snapshot: Option<clouisle_vmm::SnapshotPaths>,
    inherited_subnet: Option<(u16, u16)>,
) -> Result<Sandbox, ClouisleError> {
    let id = sandbox.id.clone();
    if !state.provisioning.lock().await.insert(id.clone()) {
        if let Some(slot) = state.warm_slots.lock().await.remove(&id) {
            let _ = state.warm_pool.discard(slot).await;
        }
        return Err(ClouisleError::invalid_state(format!(
            "sandbox {id} already has a provisioning job"
        )));
    }
    let result = provision_sandbox(
        state.clone(),
        sandbox,
        reservation,
        warm_slot,
        snapshot,
        inherited_subnet,
    )
    .await;
    if result.is_err() {
        state.reservations.lock().await.remove(&id);
        if let Some(slot) = state.warm_slots.lock().await.remove(&id) {
            let _ = state.warm_pool.discard(slot).await;
        }
    }
    state.provisioning.lock().await.remove(&id);
    result
}
async fn provision_sandbox(
    state: AppState,
    mut sandbox: Sandbox,
    reservation: Option<clouisle_scheduler::Reservation>,
    warm_slot: Option<clouisle_pool::PoolSlot>,
    snapshot: Option<clouisle_vmm::SnapshotPaths>,
    inherited_subnet: Option<(u16, u16)>,
) -> Result<Sandbox, ClouisleError> {
    let id = sandbox.id.clone();
    // 快照快路径：无 warm slot / 显式快照时，认领预热快照并继承其子网。
    let mut inherited_snapshot = snapshot.clone();
    let mut inherited_subnet: Option<(u16, u16)> = inherited_subnet;
    if warm_slot.is_none()
        && snapshot.is_none()
        && let Some((paths, subnet)) = state.claim_snapshot(&sandbox.spec.pool_key(), &id).await
    {
        inherited_snapshot = Some(paths);
        inherited_subnet = Some(subnet);
    }
    #[cfg(target_os = "linux")]
    if state.manage_network
        && let Err(error) = state
            .firewall
            .create_network_in_subnet(&id, inherited_subnet)
            .await
    {
        mark_failed(&state, &id, &error, None).await;
        return Err(error);
    }

    let mut handle = if let Some(slot) = warm_slot.as_ref() {
        slot.vm_handle.clone()
    } else if let Some(snapshot) = inherited_snapshot.as_ref() {
        match tokio::time::timeout(
            std::time::Duration::from_secs(300),
            state.vmm.restore(&id, &sandbox.spec, snapshot),
        )
        .await
        {
            Ok(Ok(handle)) => handle,
            Ok(Err(error)) => {
                mark_failed(&state, &id, &error, None).await;
                return Err(error);
            }
            Err(_) => {
                let error = ClouisleError::timeout("snapshot restore timed out after 300s");
                mark_failed(&state, &id, &error, None).await;
                return Err(error);
            }
        }
    } else {
        match tokio::time::timeout(
            std::time::Duration::from_secs(300),
            state.vmm.create(&id, &sandbox.spec),
        )
        .await
        {
            Ok(Ok(handle)) => handle,
            Ok(Err(error)) => {
                mark_failed(&state, &id, &error, None).await;
                return Err(error);
            }
            Err(_) => {
                let error = ClouisleError::timeout("image and VMM creation timed out after 300s");
                mark_failed(&state, &id, &error, None).await;
                return Err(error);
            }
        }
    };
    if let Some((a, b)) = inherited_subnet {
        handle.subnet = Some((a, b));
        sandbox
            .vmm_meta
            .extra
            .insert("subnet".into(), format!("{a}.{b}"));
    }
    let mut vmm_meta = clouisle_core::VmmMeta {
        backend: handle.backend.clone(),
        owner_id: handle.owner_id.clone(),
        pid: handle.pid,
        api_socket: handle.api_socket.clone(),
        vsock_socket: handle.vsock_socket.clone(),
        vsock_cid: handle.vsock_cid,
        vmm_id: Some(handle.id.clone()),
        extra: sandbox.vmm_meta.extra.clone(),
    };
    sandbox.vmm_meta = vmm_meta.clone();
    if let Err(error) = state.store.update_sandbox_vmm_meta(&id, &vmm_meta).await {
        let error: ClouisleError = error.into();
        mark_failed(&state, &id, &error, Some(&handle)).await;
        return Err(error);
    }
    if let Some(owner_id) = handle.owner_id.as_deref()
        && let Err(error) = state.store.update_sandbox_node(&id, Some(owner_id)).await
    {
        let error: ClouisleError = error.into();
        mark_failed(&state, &id, &error, Some(&handle)).await;
        return Err(error);
    }

    if warm_slot.is_none()
        && inherited_snapshot.is_none()
        && let Err(error) = state.vmm.start(&handle).await
    {
        mark_failed(&state, &id, &error, Some(&handle)).await;
        return Err(error);
    }

    #[cfg(target_os = "linux")]
    if state.manage_network {
        let gateway = match inherited_subnet {
            Some((a, b)) => format!("10.{a}.{b}.1/30"),
            None => format!("{}/30", clouisle_net::netns::gateway_ip(&id)),
        };
        let allow = if sandbox.spec.network.enabled {
            sandbox.spec.network.allow_egress.clone()
        } else {
            Vec::new()
        };
        if let Err(error) = state
            .firewall
            .setup_sandbox_network(
                &id,
                &gateway,
                &allow,
                &sandbox.spec.network.deny_egress,
                sandbox.spec.resources.bandwidth_mbps,
            )
            .await
        {
            mark_failed(&state, &id, &error, Some(&handle)).await;
            return Err(error);
        }
    }

    let start_timeout = tokio::time::Duration::from_secs(sandbox.spec.start_timeout_secs);
    let hello =
        tokio::time::timeout(start_timeout, state.agent.connect_and_hello(&handle, &id)).await;
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
    // guest 内资源限制（cgroup v2 pids.max）；失败不阻断 provision（记录）。
    if let Err(error) = conn.apply_limits(sandbox.spec.resources.pids_max).await {
        tracing::warn!(sandbox_id = %id, %error, "apply guest pids limit failed");
    }
    if let Err(error) = materialize_volume_mounts(&state, conn.as_ref(), &sandbox).await {
        mark_failed(&state, &id, &error, Some(&handle)).await;
        return Err(error);
    }
    if !sandbox.spec.init_command.is_empty() {
        let mut init_env = sandbox.spec.env.clone();
        init_env.extend(sandbox.spec.init_env.clone());
        let result = match conn
            .exec(
                sandbox.spec.init_command.clone(),
                init_env,
                sandbox.spec.init_cwd.clone(),
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
    if vmm_meta.extra.remove("recovery_attempts").is_some() {
        state.store.update_sandbox_vmm_meta(&id, &vmm_meta).await?;
        sandbox.vmm_meta = vmm_meta;
    }
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
        let subnet = handle.and_then(|h| h.subnet);
        let _ = state.firewall.teardown_sandbox_network(id, subnet).await;
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
        Some("paused") => Some(SandboxStatus::Paused),
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
    let warm_slot = state.warm_slots.lock().await.remove(&id);
    if let Some(slot) = warm_slot.as_ref() {
        if sb.status == SandboxStatus::Paused {
            let _ = state.vmm.resume(&slot.vm_handle).await;
        }
        if let Err(error) = state.warm_pool.release(slot.clone()).await {
            tracing::warn!(sandbox_id = %id, %error, "failed to return warm slot");
        }
    } else {
        // The VMM owner may be remote, so PID absence must not skip deletion.
        let handle = clouisle_vmm::VmHandle {
            id: sb.vmm_meta.vmm_id.clone().unwrap_or_else(|| id.clone()),
            backend: sb.vmm_meta.backend.clone(),
            owner_id: sb.vmm_meta.owner_id.clone(),
            pid: sb.vmm_meta.pid,
            api_socket: sb.vmm_meta.api_socket.clone(),
            vsock_socket: sb.vmm_meta.vsock_socket.clone(),
            vsock_cid: sb.vmm_meta.vsock_cid,
            subnet: None,
        };
        if sb.vmm_meta.vmm_id.is_some() {
            let _ = state.vmm.stop(&handle, clouisle_vmm::StopMode::Force).await;
        }
    }

    state.store.delete_sandbox(&id).await?;
    state.reservations.lock().await.remove(&id);
    state.release_snapshot(&id).await;
    // 清理网络隔离（Linux only）
    #[cfg(target_os = "linux")]
    if state.manage_network
        && let Err(error) = state
            .firewall
            .teardown_sandbox_network(&id, sb.vmm_meta.inherited_subnet())
            .await
    {
        tracing::warn!(sandbox_id = %id, error = %error, "firewall teardown failed");
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn recover_sandbox(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    let sandbox = state.store.get_sandbox(&id).await?;
    state.auth.require_tenant(&principal, &sandbox)?;
    if sandbox.status == SandboxStatus::Running
        && state
            .vmm
            .probe(&clouisle_vmm::VmHandle {
                id: sandbox
                    .vmm_meta
                    .vmm_id
                    .clone()
                    .unwrap_or_else(|| id.clone()),
                backend: sandbox.vmm_meta.backend.clone(),
                owner_id: sandbox.vmm_meta.owner_id.clone(),
                pid: sandbox.vmm_meta.pid,
                api_socket: sandbox.vmm_meta.api_socket.clone(),
                vsock_socket: sandbox.vmm_meta.vsock_socket.clone(),
                vsock_cid: sandbox.vmm_meta.vsock_cid,
                subnet: None,
            })
            .await
            .unwrap_or(false)
    {
        return Ok((StatusCode::OK, Json(redact_secrets(sandbox))).into_response());
    }
    if let Some(slot) = state.warm_slots.lock().await.remove(&id) {
        let _ = state.warm_pool.discard(slot).await;
    }
    #[cfg(target_os = "linux")]
    if state.manage_network {
        let _ = state
            .firewall
            .teardown_sandbox_network(&id, sandbox.vmm_meta.inherited_subnet())
            .await;
    }

    if sandbox.vmm_meta.vmm_id.is_some() {
        let _ = state
            .vmm
            .stop(
                &clouisle_vmm::VmHandle {
                    id: sandbox
                        .vmm_meta
                        .vmm_id
                        .clone()
                        .unwrap_or_else(|| id.clone()),
                    backend: sandbox.vmm_meta.backend.clone(),
                    owner_id: sandbox.vmm_meta.owner_id.clone(),
                    pid: sandbox.vmm_meta.pid,
                    api_socket: sandbox.vmm_meta.api_socket.clone(),
                    vsock_socket: sandbox.vmm_meta.vsock_socket.clone(),
                    vsock_cid: sandbox.vmm_meta.vsock_cid,
                    subnet: None,
                },
                clouisle_vmm::StopMode::Force,
            )
            .await;
    }
    let reservation = if state.manage_resources {
        if state.reservations.lock().await.contains_key(&id) {
            None
        } else {
            Some(state.pool.admit(&sandbox.spec).await?)
        }
    } else {
        None
    };
    state
        .store
        .update_sandbox_vmm_meta(&id, &clouisle_core::VmmMeta::default())
        .await?;
    state
        .store
        .update_sandbox_status_message(&id, &SandboxStatus::Starting, None)
        .await?;
    if !state.provisioning.lock().await.insert(id.clone()) {
        return Err(ApiError(ClouisleError::invalid_state(
            "sandbox recovery is already running",
        )));
    }
    let state_for_task = state.clone();
    let job_id = id.clone();
    let sandbox_for_task = state.store.get_sandbox(&id).await?;
    tokio::spawn(async move {
        let result = provision_sandbox(
            state_for_task.clone(),
            sandbox_for_task,
            reservation,
            None,
            None,
            None,
        )
        .await;
        if let Err(error) = result {
            state_for_task.reservations.lock().await.remove(&job_id);
            tracing::error!(sandbox_id = %job_id, %error, "sandbox recovery failed");
        }
        state_for_task.provisioning.lock().await.remove(&job_id);
    });
    let current = state.store.get_sandbox(&id).await?;
    Ok((
        StatusCode::ACCEPTED,
        [(
            axum::http::header::RETRY_AFTER,
            HeaderValue::from_static("1"),
        )],
        Json(redact_secrets(current)),
    )
        .into_response())
}
