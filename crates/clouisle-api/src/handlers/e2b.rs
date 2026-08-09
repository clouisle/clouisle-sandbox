use axum::Json;
use axum::body::to_bytes;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use clouisle_core::{ClouisleError, Sandbox, SandboxStatus};

use crate::auth::Principal;
use crate::e2b::{
    E2bConnectRequest, E2bCreateRequest, E2bPauseRequest, E2bResumeRequest, E2bSandbox,
    E2bTimeoutRequest, expiry_from_now, from_sandbox, to_spec,
};
use crate::error::ApiError;
use crate::handlers::sandbox::{CreateSandboxRequest, create_sandbox};
use crate::state::AppState;

fn handle_for(sandbox: &Sandbox) -> clouisle_vmm::VmHandle {
    clouisle_vmm::VmHandle {
        id: sandbox
            .vmm_meta
            .vmm_id
            .clone()
            .unwrap_or_else(|| sandbox.id.clone()),
        backend: sandbox.vmm_meta.backend.clone(),
        pid: sandbox.vmm_meta.pid,
        api_socket: sandbox.vmm_meta.api_socket.clone(),
        vsock_socket: sandbox.vmm_meta.vsock_socket.clone(),
        vsock_cid: sandbox.vmm_meta.vsock_cid,
    }
}

async fn get_owned(
    state: &AppState,
    principal: &Principal,
    id: &str,
) -> Result<Sandbox, ApiError> {
    let sandbox = state.store.get_sandbox(id).await?;
    state.auth.require_tenant(principal, &sandbox)?;
    Ok(sandbox)
}

async fn create_response_to_model(response: Response) -> Result<(StatusCode, Sandbox), ApiError> {
    let status = response.status();
    let body = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .map_err(|error| ApiError(ClouisleError::internal(format!("read create response: {error}"))))?;
    let sandbox = serde_json::from_slice(&body).map_err(|error| {
        ApiError(ClouisleError::internal(format!("decode create response: {error}")))
    })?;
    Ok((status, sandbox))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(request): Json<E2bCreateRequest>,
) -> Result<Response, ApiError> {
    let spec = to_spec(request, principal.tenant_id.clone()).map_err(ClouisleError::validation)?;
    let (status, sandbox) = create_response_to_model(
        create_sandbox(
            State(state),
            Extension(principal),
            Json(CreateSandboxRequest { spec, sync: true }),
        )
        .await?,
    )
    .await?;
    Ok((status, Json(from_sandbox(&sandbox))).into_response())
}

pub async fn list(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<Vec<E2bSandbox>>, ApiError> {
    let sandboxes = state
        .store
        .list_sandboxes(None)
        .await?
        .into_iter()
        .filter(|sandbox| {
            sandbox.spec.tenant_id.as_deref() == Some(principal.tenant_id.as_str())
                && matches!(sandbox.status, SandboxStatus::Running | SandboxStatus::Paused)
        })
        .map(|sandbox| from_sandbox(&sandbox))
        .collect();
    Ok(Json(sandboxes))
}

pub async fn get(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<E2bSandbox>, ApiError> {
    Ok(Json(from_sandbox(
        &get_owned(&state, &principal, &id).await?,
    )))
}

pub async fn connect(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(request): Json<E2bConnectRequest>,
) -> Result<Response, ApiError> {
    let sandbox = get_owned(&state, &principal, &id).await?;
    let was_paused = sandbox.status == SandboxStatus::Paused;
    let sandbox = resume_if_needed(&state, sandbox, request.timeout).await?;
    let status = if was_paused {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(from_sandbox(&sandbox))).into_response())
}

pub async fn pause(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    _request: Option<Json<E2bPauseRequest>>,
) -> Result<StatusCode, ApiError> {
    let sandbox = get_owned(&state, &principal, &id).await?;
    if sandbox.status == SandboxStatus::Paused {
        return Ok(StatusCode::NO_CONTENT);
    }
    if sandbox.status != SandboxStatus::Running {
        return Err(ApiError(ClouisleError::invalid_state(format!(
            "sandbox {id} is not running"
        ))));
    }
    state.vmm.pause(&handle_for(&sandbox)).await?;
    state
        .store
        .update_sandbox_status_message(&id, &SandboxStatus::Paused, None)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn resume(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(request): Json<E2bResumeRequest>,
) -> Result<Response, ApiError> {
    let sandbox = get_owned(&state, &principal, &id).await?;
    let sandbox = resume_if_needed(&state, sandbox, request.timeout.unwrap_or(15)).await?;
    Ok((StatusCode::CREATED, Json(from_sandbox(&sandbox))).into_response())
}

async fn resume_if_needed(
    state: &AppState,
    sandbox: Sandbox,
    timeout: u64,
) -> Result<Sandbox, ApiError> {
    let id = sandbox.id.clone();
    if sandbox.status == SandboxStatus::Paused {
        state.vmm.resume(&handle_for(&sandbox)).await?;
        state
            .store
            .update_sandbox_status_message(&id, &SandboxStatus::Starting, None)
            .await?;
        let hello = tokio::time::timeout(
            std::time::Duration::from_secs(sandbox.spec.start_timeout_secs),
            state
                .agent
                .connect_and_hello(&handle_for(&sandbox), &id),
        )
        .await
        .map_err(|_| ClouisleError::timeout("sandbox resume agent hello timed out"))?
        .map_err(ApiError)?;
        drop(hello);
        state
            .store
            .update_sandbox_status_message(&id, &SandboxStatus::Running, None)
            .await?;
    } else if sandbox.status != SandboxStatus::Running {
        return Err(ApiError(ClouisleError::invalid_state(format!(
            "sandbox {id} cannot resume from {}",
            sandbox.status
        ))));
    }
    state
        .store
        .update_sandbox_expiry(&id, Some(expiry_from_now(timeout)))
        .await?;
    Ok(state.store.get_sandbox(&id).await?)
}

pub async fn set_timeout(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(request): Json<E2bTimeoutRequest>,
) -> Result<StatusCode, ApiError> {
    let sandbox = get_owned(&state, &principal, &id).await?;
    if sandbox.status != SandboxStatus::Running && sandbox.status != SandboxStatus::Paused {
        return Err(ApiError(ClouisleError::invalid_state(format!(
            "sandbox {id} is not active"
        ))));
    }
    state
        .store
        .update_sandbox_expiry(&id, Some(expiry_from_now(request.timeout)))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn refresh(
    state: State<AppState>,
    principal: Extension<Principal>,
    id: Path<String>,
    request: Json<E2bTimeoutRequest>,
) -> Result<StatusCode, ApiError> {
    set_timeout(state, principal, id, request).await
}

pub async fn v2_list(
    state: State<AppState>,
    principal: Extension<Principal>,
) -> Result<Json<Vec<E2bSandbox>>, ApiError> {
    list(state, principal).await
}

#[derive(Debug, Deserialize)]
pub struct E2bListDirRequest {
    pub path: String,
    #[serde(default)]
    pub depth: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct E2bEntryInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub path: String,
    pub size: u64,
    pub mode: u32,
}

#[derive(Debug, Serialize)]
pub struct E2bListDirResponse {
    pub entries: Vec<E2bEntryInfo>,
}

#[derive(Debug, Deserialize)]
pub struct E2bProcessStartRequest {
    pub process: E2bProcessConfig,
}

#[derive(Debug, Deserialize)]
pub struct E2bProcessConfig {
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub envs: std::collections::HashMap<String, String>,
    pub cwd: Option<String>,
}

fn sandbox_id_from_headers(headers: &axum::http::HeaderMap) -> Result<String, ApiError> {
    headers
        .get("e2b-sandbox-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ApiError(ClouisleError::validation("E2b-Sandbox-Id is required")))
}

pub async fn upload_file(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<crate::handlers::files::FsQuery>,
    body: axum::body::Bytes,
) -> Result<Json<Vec<E2bEntryInfo>>, ApiError> {
    crate::handlers::files::validate_path(&query.path).map_err(ApiError)?;
    if body.len() > crate::handlers::files::MAX_UPLOAD_BYTES {
        return Err(ApiError(ClouisleError::validation("file is too large")));
    }
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    let conn = crate::handlers::files::get_conn(&state, &sandbox_id, &principal)
        .await
        .map_err(ApiError)?;
    conn.write_file(&query.path, body, 0o644)
        .await
        .map_err(ApiError)?;
    Ok(Json(vec![E2bEntryInfo {
        name: query.path.rsplit('/').next().unwrap_or_default().into(),
        entry_type: "file".into(),
        path: query.path,
        size: 0,
        mode: 0o644,
    }]))
}

pub async fn download_file(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<crate::handlers::files::FsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    crate::handlers::files::validate_path(&query.path).map_err(ApiError)?;
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    let conn = crate::handlers::files::get_conn(&state, &sandbox_id, &principal)
        .await
        .map_err(ApiError)?;
    let data = conn.read_file(&query.path).await.map_err(ApiError)?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        data,
    ))
}

pub async fn list_dir(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bListDirRequest>,
) -> Result<Json<E2bListDirResponse>, ApiError> {
    crate::handlers::files::validate_path(&request.path).map_err(ApiError)?;
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    let conn = crate::handlers::files::get_conn(&state, &sandbox_id, &principal)
        .await
        .map_err(ApiError)?;
    let entries = conn.list_dir(&request.path).await.map_err(ApiError)?;
    Ok(Json(E2bListDirResponse {
        entries: entries
            .into_iter()
            .map(|entry| E2bEntryInfo {
                name: entry.name,
                entry_type: if entry.is_dir { "directory" } else { "file" }.into(),
                path: request.path.clone(),
                size: entry.size,
                mode: entry.mode,
            })
            .collect(),
    }))
}

#[derive(Debug, Serialize)]
struct E2bProcessResponse {
    event: E2bProcessEvent,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum E2bProcessEvent {
    Start { start: E2bProcessStart },
    Data { data: E2bProcessData },
    End { end: E2bProcessEnd },
}

#[derive(Debug, Serialize)]
struct E2bProcessStart {
    pid: u32,
}

#[derive(Debug, Serialize)]
struct E2bProcessData {
    stdout: String,
}

#[derive(Debug, Serialize)]
struct E2bProcessEnd {
    exited: bool,
    status: String,
}

pub async fn process_start(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bProcessStartRequest>,
) -> Result<Response, ApiError> {
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    if request.process.cmd.trim().is_empty() {
        return Err(ApiError(ClouisleError::validation("process.cmd is required")));
    }
    let conn = crate::handlers::files::get_conn(&state, &sandbox_id, &principal)
        .await
        .map_err(ApiError)?;
    let mut argv = Vec::with_capacity(request.process.args.len() + 1);
    argv.push(request.process.cmd);
    argv.extend(request.process.args);
    let result = conn
        .exec(
            argv,
            request.process.envs,
            request.process.cwd,
            30_000,
        )
        .await
        .map_err(ApiError)?;
    let mut events = vec![E2bProcessResponse {
        event: E2bProcessEvent::Start {
            start: E2bProcessStart { pid: 0 },
        },
    }];
    if !result.stdout.is_empty() {
        events.push(E2bProcessResponse {
            event: E2bProcessEvent::Data {
                data: E2bProcessData {
                    stdout: String::from_utf8_lossy(&result.stdout).into_owned(),
                },
            },
        });
    }
    events.push(E2bProcessResponse {
        event: E2bProcessEvent::End {
            end: E2bProcessEnd {
                exited: true,
                status: format!("exit status {}", result.exit_code),
            },
        },
    });
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/connect+json")],
        Json(events),
    )
        .into_response())
}
