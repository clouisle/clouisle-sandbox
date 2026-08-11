use axum::Json;
use axum::body::{Body, to_bytes};
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};

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
        owner_id: sandbox.vmm_meta.owner_id.clone(),
        pid: sandbox.vmm_meta.pid,
        api_socket: sandbox.vmm_meta.api_socket.clone(),
        vsock_socket: sandbox.vmm_meta.vsock_socket.clone(),
        vsock_cid: sandbox.vmm_meta.vsock_cid,
        subnet: None,
    }
}

async fn get_owned(state: &AppState, principal: &Principal, id: &str) -> Result<Sandbox, ApiError> {
    let sandbox = state.store.get_sandbox(id).await?;
    state.auth.require_tenant(principal, &sandbox)?;
    Ok(sandbox)
}

async fn create_response_to_model(response: Response) -> Result<(StatusCode, Sandbox), ApiError> {
    let status = response.status();
    let body = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .map_err(|error| {
            ApiError(ClouisleError::internal(format!(
                "read create response: {error}"
            )))
        })?;
    let sandbox = serde_json::from_slice(&body).map_err(|error| {
        ApiError(ClouisleError::internal(format!(
            "decode create response: {error}"
        )))
    })?;
    Ok((status, sandbox))
}

async fn e2b_response(state: &AppState, sandbox: &Sandbox) -> Result<E2bSandbox, ApiError> {
    let mut response = from_sandbox(sandbox);
    response.envd_access_token = Some(
        state
            .ensure_e2b_access_token(
                &sandbox.id,
                sandbox.spec.tenant_id.as_deref().unwrap_or_default(),
            )
            .await?,
    );
    Ok(response)
}

pub async fn create(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(request): Json<E2bCreateRequest>,
) -> Result<Response, ApiError> {
    let template_id = request.template_id.clone();
    let snapshot_id = request.snapshot_id.clone();
    let mut spec =
        to_spec(request, principal.tenant_id.clone()).map_err(ClouisleError::validation)?;
    match state.e2b.template(&principal.tenant_id, &template_id).await {
        Ok(template) => {
            let image = template.image_reference.ok_or_else(|| {
                ApiError(ClouisleError::validation(
                    "template has no OCI image reference",
                ))
            })?;
            spec.image.reference = image;
        }
        Err(crate::e2b_cloud::ControlPlaneError::NotFound(_))
            if template_id.contains(':')
                || template_id.contains('/')
                || template_id.contains('@') => {}
        Err(error) => return Err(ApiError::from(error)),
    }
    if let Some(snapshot_id) = snapshot_id {
        if !state.vmm.capabilities().snapshot {
            return Err(ApiError(ClouisleError::invalid_state(
                "snapshot restore is unavailable",
            )));
        }
        let snapshot = state
            .e2b
            .snapshot(&principal.tenant_id, &snapshot_id)
            .await
            .map_err(ApiError::from)?;
        let reservation = if state.manage_resources {
            Some(state.pool.admit(&spec).await?)
        } else {
            None
        };
        let id = uuid::Uuid::now_v7().to_string();
        let mut sandbox = Sandbox::new(id.clone(), spec);
        sandbox.transition(clouisle_core::SandboxEvent::Start)?;
        state.store.create_sandbox(&sandbox).await?;
        let state_for_task = state.clone();
        let sandbox_for_task = sandbox.clone();
        let paths = clouisle_vmm::SnapshotPaths {
            state_path: snapshot.state_path,
            mem_path: snapshot.memory_path,
        };
        tokio::spawn(async move {
            if let Err(error) = crate::handlers::sandbox::run_provision_from_snapshot(
                state_for_task,
                sandbox_for_task,
                reservation,
                paths,
                None,
            )
            .await
            {
                tracing::error!(sandbox_id = %id, %error, "snapshot clone provisioning failed");
            }
        });
        let response = e2b_response(&state, &sandbox).await?;
        return Ok((StatusCode::ACCEPTED, Json(response)).into_response());
    }
    let (status, sandbox) = create_response_to_model(
        create_sandbox(
            State(state.clone()),
            Extension(principal.clone()),
            Json(CreateSandboxRequest { spec, sync: true }),
        )
        .await?,
    )
    .await?;
    let response = e2b_response(&state, &sandbox).await?;
    Ok((status, Json(response)).into_response())
}

#[derive(Debug, Default, Deserialize)]
pub struct E2bPaginationQuery {
    #[serde(rename = "nextToken")]
    pub next_token: Option<usize>,
    pub limit: Option<usize>,
    pub metadata: Option<String>,
    pub state: Option<String>,
}

async fn list_inner(
    state: AppState,
    principal: Principal,
    query: E2bPaginationQuery,
    running_only: bool,
) -> Result<Response, ApiError> {
    let requested_states = query.state.as_deref().map(|value| {
        value
            .split(',')
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>()
    });
    let running_requested = running_only
        || requested_states
            .as_ref()
            .is_some_and(|states| states.iter().any(|state| state == "running"));
    let metadata_filter = query.metadata.as_deref();
    let mut all = state
        .store
        .list_sandboxes(None)
        .await?
        .into_iter()
        .filter(|sandbox| {
            sandbox.spec.tenant_id.as_deref() == Some(principal.tenant_id.as_str())
                && sandbox.status != SandboxStatus::Stopped
                && (!running_only || sandbox.status == SandboxStatus::Running)
                && requested_states.as_ref().is_none_or(|states| {
                    let state = match sandbox.status {
                        SandboxStatus::Pending | SandboxStatus::Starting => "starting",
                        SandboxStatus::Running => "running",
                        SandboxStatus::Paused => "paused",
                        SandboxStatus::Stopping => "stopping",
                        SandboxStatus::Stopped => "killed",
                        SandboxStatus::Error => "error",
                    };
                    states.iter().any(|wanted| wanted == state)
                })
                && metadata_filter.is_none_or(|filter| {
                    filter
                        .split('&')
                        .filter(|pair| !pair.is_empty())
                        .all(|pair| {
                            let Some((key, value)) = pair.split_once('=') else {
                                return false;
                            };
                            sandbox
                                .spec
                                .metadata
                                .get(key)
                                .is_some_and(|actual| actual == value)
                        })
                })
        })
        .map(|sandbox| from_sandbox(&sandbox))
        .collect::<Vec<_>>();
    let total_running = if running_requested {
        all.iter()
            .filter(|sandbox| sandbox.state == "running")
            .count()
    } else {
        0
    };
    let offset = query.next_token.unwrap_or(0).min(all.len());
    let limit = query.limit.unwrap_or(100).clamp(1, 100);
    let end = (offset + limit).min(all.len());
    let next = (end < all.len()).then(|| end.to_string());
    let mut response = Json(all.drain(offset..end).collect::<Vec<_>>()).into_response();
    if let Some(next) = next
        && let Ok(value) = axum::http::HeaderValue::from_str(&next)
    {
        response.headers_mut().insert("x-next-token", value);
    }
    if running_requested
        && let Ok(value) = axum::http::HeaderValue::from_str(&total_running.to_string())
    {
        response.headers_mut().insert("x-total-running", value);
    }
    Ok(response)
}

pub async fn list(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<E2bPaginationQuery>,
) -> Result<Response, ApiError> {
    list_inner(state, principal, query, true).await
}

pub async fn get(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<E2bSandbox>, ApiError> {
    let sandbox = get_owned(&state, &principal, &id).await?;
    Ok(Json(e2b_response(&state, &sandbox).await?))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let sandbox = get_owned(&state, &principal, &id).await?;
    let tenant_id = sandbox.spec.tenant_id.clone().unwrap_or_default();
    let status = crate::handlers::sandbox::delete_sandbox(
        State(state.clone()),
        Extension(principal),
        Path(id.clone()),
    )
    .await?;
    state.remove_e2b_access_token(&id, &tenant_id).await;
    state.processes.remove_sandbox(&id).await;
    Ok(status)
}
pub async fn connect(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    request: Option<Json<E2bConnectRequest>>,
) -> Result<Response, ApiError> {
    let sandbox = get_owned(&state, &principal, &id).await?;
    let was_paused = sandbox.status == SandboxStatus::Paused;
    let timeout = request
        .map(|Json(request)| request.timeout.unwrap_or(15))
        .unwrap_or(15);
    let sandbox = resume_if_needed(&state, sandbox, timeout).await?;
    let status = if was_paused {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let response = e2b_response(&state, &sandbox).await?;
    Ok((status, Json(response)).into_response())
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
    request: Option<Json<E2bResumeRequest>>,
) -> Result<Response, ApiError> {
    let sandbox = get_owned(&state, &principal, &id).await?;
    let timeout = request
        .map(|Json(request)| request.timeout.unwrap_or(15))
        .unwrap_or(15);
    let sandbox = resume_if_needed(&state, sandbox, timeout).await?;
    let response = e2b_response(&state, &sandbox).await?;
    Ok((StatusCode::CREATED, Json(response)).into_response())
}

async fn resume_if_needed(
    state: &AppState,
    sandbox: Sandbox,
    timeout: u64,
) -> Result<Sandbox, ApiError> {
    let id = sandbox.id.clone();
    let was_paused = sandbox.status == SandboxStatus::Paused;
    let reservation = if was_paused
        && state.manage_resources
        && !state.reservations.lock().await.contains_key(&id)
    {
        Some(state.pool.admit(&sandbox.spec).await?)
    } else {
        None
    };
    if was_paused {
        state.vmm.resume(&handle_for(&sandbox)).await?;
        state
            .store
            .update_sandbox_status_message(&id, &SandboxStatus::Starting, None)
            .await?;
        let hello = tokio::time::timeout(
            std::time::Duration::from_secs(sandbox.spec.start_timeout_secs),
            state.agent.connect_and_hello(&handle_for(&sandbox), &id),
        )
        .await
        .map_err(|_| ClouisleError::timeout("sandbox resume agent hello timed out"))?
        .map_err(ApiError)?;
        hello.ping().await.map_err(ApiError)?;
        state
            .store
            .update_sandbox_status_message(&id, &SandboxStatus::Running, None)
            .await?;
        if let Some(reservation) = reservation {
            state
                .reservations
                .lock()
                .await
                .insert(id.clone(), reservation);
        }
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

#[derive(Debug, Deserialize)]
pub struct E2bNetworkUpdateRequest {
    #[serde(rename = "allowOut", default)]
    pub allow_out: Vec<String>,
    #[serde(rename = "allow_internet_access", alias = "allowInternetAccess")]
    pub allow_internet_access: Option<bool>,
    #[serde(rename = "denyOut", default)]
    pub deny_out: Vec<String>,
    #[serde(default)]
    pub rules: Option<serde_json::Value>,
}

pub async fn update_network(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
    Json(request): Json<E2bNetworkUpdateRequest>,
) -> Result<StatusCode, ApiError> {
    let sandbox = get_owned(&state, &principal, &id).await?;
    if sandbox.status != SandboxStatus::Running {
        return Err(ApiError(ClouisleError::invalid_state(format!(
            "sandbox {id} is not running"
        ))));
    }
    if request.rules.as_ref().is_some_and(|rules| !rules.is_null()) {
        return Err(ApiError(ClouisleError::validation(
            "network transform rules are not supported by the local firewall",
        )));
    }
    let mut spec = sandbox.spec;
    if let Some(enabled) = request.allow_internet_access {
        spec.network.enabled = enabled;
    }
    spec.network.allow_egress = request.allow_out;
    spec.network.deny_egress = request.deny_out;
    state.store.update_sandbox_spec(&id, &spec).await?;
    #[cfg(target_os = "linux")]
    if state.manage_network {
        state
            .firewall
            .update_sandbox_network(
                &id,
                spec.network.enabled,
                &spec.network.allow_egress,
                &spec.network.deny_egress,
            )
            .await?;
    }
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
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<E2bPaginationQuery>,
) -> Result<Response, ApiError> {
    list_inner(state, principal, query, false).await
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
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub pty: Option<clouisle_proto::PtyConfig>,
    /// 默认 true（proto 语义）：保持 stdin 打开。
    #[serde(default = "default_true_stdin")]
    pub stdin: bool,
}

fn default_true_stdin() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct E2bProcessConnectRequest {
    pub process: E2bProcessSelector,
}

#[derive(Debug, Deserialize)]
pub struct E2bProcessSelector {
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub tag: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct E2bProcessInput {
    #[serde(default)]
    pub stdin: Option<String>,
    #[serde(default)]
    pub pty: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct E2bSendInputRequest {
    pub process: E2bProcessSelector,
    pub input: E2bProcessInput,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum E2bSignal {
    Name(String),
    Num(u8),
}

impl E2bSignal {
    fn into_signal(self) -> Result<clouisle_proto::ProcessSignal, ApiError> {
        use clouisle_proto::ProcessSignal;
        let signal = match self {
            E2bSignal::Num(9) => ProcessSignal::Sigkill,
            E2bSignal::Num(15) => ProcessSignal::Sigterm,
            E2bSignal::Num(2) => ProcessSignal::Sigint,
            E2bSignal::Name(name) => match name.as_str() {
                "SIGNAL_SIGKILL" => ProcessSignal::Sigkill,
                "SIGNAL_SIGTERM" => ProcessSignal::Sigterm,
                "SIGNAL_SIGINT" => ProcessSignal::Sigint,
                _ => {
                    return Err(ApiError(ClouisleError::validation(format!(
                        "unsupported signal: {name}"
                    ))));
                }
            },
            _ => {
                return Err(ApiError(ClouisleError::validation(
                    "unsupported signal number",
                )));
            }
        };
        Ok(signal)
    }
}

#[derive(Debug, Deserialize)]
pub struct E2bSendSignalRequest {
    pub process: E2bProcessSelector,
    pub signal: E2bSignal,
}

#[derive(Debug, Deserialize)]
pub struct E2bCloseStdinRequest {
    pub process: E2bProcessSelector,
}

#[derive(Debug, Deserialize)]
pub struct E2bResizeRequest {
    pub process: E2bProcessSelector,
    #[serde(default)]
    pub pty: Option<E2bPtySize>,
}

#[derive(Debug, Deserialize)]
pub struct E2bPtySize {
    pub size: E2bSize,
}

#[derive(Debug, Deserialize)]
pub struct E2bSize {
    pub cols: u16,
    pub rows: u16,
}

fn sandbox_id_from_headers(headers: &axum::http::HeaderMap) -> Result<String, ApiError> {
    headers
        .get("e2b-sandbox-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ApiError(ClouisleError::validation("E2b-Sandbox-Id is required")))
}

async fn resume_for_io(
    state: &AppState,
    principal: &Principal,
    sandbox_id: &str,
) -> Result<(), ApiError> {
    let sandbox = get_owned(state, principal, sandbox_id).await?;
    if sandbox.status == SandboxStatus::Paused && sandbox.spec.auto_resume {
        resume_if_needed(state, sandbox, 15).await?;
    }
    Ok(())
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
    let size = body.len() as u64;
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    resume_for_io(&state, &principal, &sandbox_id).await?;
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
        size,
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
    resume_for_io(&state, &principal, &sandbox_id).await?;
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
    resume_for_io(&state, &principal, &sandbox_id).await?;
    let conn = crate::handlers::files::get_conn(&state, &sandbox_id, &principal)
        .await
        .map_err(ApiError)?;
    let entries = conn.list_dir(&request.path).await.map_err(ApiError)?;
    Ok(Json(E2bListDirResponse {
        entries: entries
            .into_iter()
            .map(|entry| {
                let path = if request.path == "/" {
                    format!("/{}", entry.name)
                } else {
                    format!("{}/{}", request.path.trim_end_matches('/'), entry.name)
                };
                E2bEntryInfo {
                    name: entry.name,
                    entry_type: if entry.is_dir { "directory" } else { "file" }.into(),
                    path,
                    size: entry.size,
                    mode: entry.mode,
                }
            })
            .collect(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct E2bFilesystemPathRequest {
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct E2bFilesystemMoveRequest {
    pub source: String,
    pub destination: String,
}

async fn exec_filesystem(
    state: &AppState,
    principal: &Principal,
    headers: &axum::http::HeaderMap,
    argv: Vec<String>,
) -> Result<clouisle_core::execution::ExecutionResult, ApiError> {
    let sandbox_id = sandbox_id_from_headers(headers)?;
    resume_for_io(state, principal, &sandbox_id).await?;
    let conn = crate::handlers::files::get_conn(state, &sandbox_id, principal)
        .await
        .map_err(ApiError)?;
    // `stat -c %F` output is locale-dependent; pin C so filesystem RPC
    // parsing stays deterministic regardless of the guest locale.
    let mut env = std::collections::HashMap::new();
    env.insert("LC_ALL".to_string(), "C".to_string());
    conn.exec(argv, env, None, 30_000).await.map_err(ApiError)
}

fn filesystem_entry(path: &str, entry_type: &str, size: u64, mode: u32) -> serde_json::Value {
    json!({
        "name": path.rsplit('/').next().filter(|name| !name.is_empty()).unwrap_or("/"),
        "type": entry_type,
        "path": path,
        "size": size,
        "mode": mode,
        "permissions": format!("{:o}", mode),
        "owner": "",
        "group": "",
    })
}

pub async fn filesystem_stat(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bFilesystemPathRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::handlers::files::validate_path(&request.path).map_err(ApiError)?;
    let result = exec_filesystem(
        &state,
        &principal,
        &headers,
        vec![
            "stat".into(),
            "-c".into(),
            "%F|%s|%a".into(),
            request.path.clone(),
        ],
    )
    .await?;
    if result.exit_code != 0 {
        return Err(ApiError(ClouisleError::not_found(format!(
            "path not found: {}",
            request.path
        ))));
    }
    let fields = String::from_utf8_lossy(&result.stdout);
    let mut fields = fields.trim().split('|');
    let kind = if fields.next().unwrap_or_default().contains("directory") {
        "FILE_TYPE_DIRECTORY"
    } else {
        "FILE_TYPE_FILE"
    };
    let size = fields
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mode = u32::from_str_radix(fields.next().unwrap_or("644"), 8).unwrap_or(0o644);
    Ok(Json(
        json!({"entry": filesystem_entry(&request.path, kind, size, mode)}),
    ))
}

pub async fn filesystem_make_dir(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bFilesystemPathRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::handlers::files::validate_path(&request.path).map_err(ApiError)?;
    let result = exec_filesystem(
        &state,
        &principal,
        &headers,
        vec!["mkdir".into(), "-p".into(), request.path.clone()],
    )
    .await?;
    if result.exit_code != 0 {
        return Err(ApiError(ClouisleError::internal(
            String::from_utf8_lossy(&result.stderr).trim().to_string(),
        )));
    }
    Ok(Json(
        json!({"entry": filesystem_entry(&request.path, "FILE_TYPE_DIRECTORY", 0, 0o755)}),
    ))
}

pub async fn filesystem_move(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bFilesystemMoveRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::handlers::files::validate_path(&request.source).map_err(ApiError)?;
    crate::handlers::files::validate_path(&request.destination).map_err(ApiError)?;
    let result = exec_filesystem(
        &state,
        &principal,
        &headers,
        vec!["mv".into(), request.source, request.destination.clone()],
    )
    .await?;
    if result.exit_code != 0 {
        return Err(ApiError(ClouisleError::internal(
            String::from_utf8_lossy(&result.stderr).trim().to_string(),
        )));
    }
    Ok(Json(
        json!({"entry": filesystem_entry(&request.destination, "FILE_TYPE_FILE", 0, 0o644)}),
    ))
}

pub async fn filesystem_remove(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bFilesystemPathRequest>,
) -> Result<StatusCode, ApiError> {
    crate::handlers::files::validate_path(&request.path).map_err(ApiError)?;
    let result = exec_filesystem(
        &state,
        &principal,
        &headers,
        vec!["rm".into(), "-rf".into(), request.path],
    )
    .await?;
    if result.exit_code != 0 {
        return Err(ApiError(ClouisleError::internal(
            String::from_utf8_lossy(&result.stderr).trim().to_string(),
        )));
    }
    Ok(StatusCode::OK)
}

#[derive(Debug, Deserialize)]
pub struct E2bWatcherRequest {
    pub path: String,
    #[serde(default)]
    pub recursive: bool,
    #[serde(default, rename = "includeEntry")]
    pub include_entry: bool,
}

#[derive(Debug, Deserialize)]
pub struct E2bWatcherIdRequest {
    #[serde(rename = "watcherId")]
    pub watcher_id: String,
}

pub async fn filesystem_create_watcher(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bWatcherRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::handlers::files::validate_path(&request.path).map_err(ApiError)?;
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    let _ = get_owned(&state, &principal, &sandbox_id).await?;
    let watcher = state
        .e2b
        .create_watcher(
            &principal.tenant_id,
            &sandbox_id,
            &request.path,
            request.recursive,
            request.include_entry,
        )
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({"watcherId": watcher.watcher_id})))
}

pub async fn filesystem_get_watcher_events(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(request): Json<E2bWatcherIdRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let events = state
        .e2b
        .watcher_events(&principal.tenant_id, &request.watcher_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({"events": events})))
}

pub async fn filesystem_remove_watcher(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(request): Json<E2bWatcherIdRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .e2b
        .remove_watcher(&principal.tenant_id, &request.watcher_id)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn filesystem_watch_dir(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bWatcherRequest>,
) -> Result<Response, ApiError> {
    let watcher =
        filesystem_create_watcher(State(state), Extension(principal), headers, Json(request))
            .await?;
    let watcher_id = watcher.0["watcherId"].as_str().unwrap_or_default();
    let payload = json!({"event": {"start": {}, "watcherId": watcher_id}});
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/connect+json")],
        format!("{}\n", payload),
    )
        .into_response())
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
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pty: Option<String>,
}

#[derive(Debug, Serialize)]
struct E2bProcessEnd {
    #[serde(rename = "exitCode")]
    exit_code: i32,
    exited: bool,
    status: String,
}

static NEXT_PROCESS_ID: AtomicU32 = AtomicU32::new(1);

fn encode_process_event(
    event: crate::agent::ExecStreamEvent,
    pty: bool,
) -> Result<crate::state::ProcessEventRecord, ApiError> {
    let (payload, terminal) = match event {
        crate::agent::ExecStreamEvent::Stdout(chunk) => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&chunk);
            let data = if pty {
                E2bProcessData {
                    stdout: None,
                    stderr: None,
                    pty: Some(encoded),
                }
            } else {
                E2bProcessData {
                    stdout: Some(encoded),
                    stderr: None,
                    pty: None,
                }
            };
            (
                serde_json::to_vec(&E2bProcessResponse {
                    event: E2bProcessEvent::Data { data },
                }),
                false,
            )
        }
        crate::agent::ExecStreamEvent::Stderr(chunk) => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&chunk);
            let data = if pty {
                E2bProcessData {
                    stdout: None,
                    stderr: None,
                    pty: Some(encoded),
                }
            } else {
                E2bProcessData {
                    stdout: None,
                    stderr: Some(encoded),
                    pty: None,
                }
            };
            (
                serde_json::to_vec(&E2bProcessResponse {
                    event: E2bProcessEvent::Data { data },
                }),
                false,
            )
        }
        crate::agent::ExecStreamEvent::Exit(code) => (
            serde_json::to_vec(&E2bProcessResponse {
                event: E2bProcessEvent::End {
                    end: E2bProcessEnd {
                        exit_code: code,
                        exited: true,
                        status: format!("exit status {code}"),
                    },
                },
            }),
            true,
        ),
        crate::agent::ExecStreamEvent::Error(message) => (
            serde_json::to_vec(&json!({"error": {"message": message}})),
            true,
        ),
    };
    let mut payload = payload.map_err(|error| {
        ApiError(ClouisleError::internal(format!(
            "encode process event: {error}"
        )))
    })?;
    payload.push(b'\n');
    Ok(crate::state::ProcessEventRecord {
        payload: Bytes::from(payload),
        terminal,
    })
}

async fn stream_session(
    session: std::sync::Arc<crate::state::ProcessSession>,
) -> Result<Response, ApiError> {
    let (history, mut updates) = session.snapshot_and_subscribe().await;
    let (body_tx, body_rx) =
        tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(32);
    let mut terminal_replayed = false;
    for event in history {
        let terminal = event.terminal;
        if body_tx.send(Ok(event.payload)).await.is_err() {
            return Err(ApiError(ClouisleError::internal("process stream closed")));
        }
        if terminal {
            terminal_replayed = true;
            break;
        }
    }
    if !terminal_replayed {
        tokio::spawn(async move {
            while let Ok(event) = updates.recv().await {
                let terminal = event.terminal;
                if body_tx.send(Ok(event.payload)).await.is_err() || terminal {
                    break;
                }
            }
        });
    }
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/connect+json")],
        Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(body_rx)),
    )
        .into_response())
}

struct ProcessLaunch {
    argv: Vec<String>,
    env: std::collections::HashMap<String, String>,
    cwd: Option<String>,
    timeout_ms: u64,
    config: serde_json::Value,
    tag: Option<String>,
    stdin: bool,
    pty: Option<clouisle_proto::PtyConfig>,
}

async fn stream_process(
    state: AppState,
    sandbox_id: String,
    conn: Box<dyn crate::agent::AgentConnection>,
    launch: ProcessLaunch,
) -> Result<Response, ApiError> {
    let pid = NEXT_PROCESS_ID.fetch_add(1, Ordering::Relaxed);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(32);
    let guest_id = conn
        .start_process(
            launch.argv,
            launch.env,
            launch.cwd,
            launch.timeout_ms,
            launch.stdin,
            launch.pty,
        )
        .await?;
    let session = state
        .processes
        .create(
            &sandbox_id,
            pid,
            launch.config,
            launch.tag,
            guest_id.clone(),
            launch.pty.is_some(),
        )
        .await;
    let start = serde_json::to_vec(&E2bProcessResponse {
        event: E2bProcessEvent::Start {
            start: E2bProcessStart { pid },
        },
    })
    .map_err(|error| {
        ApiError(ClouisleError::internal(format!(
            "encode process start: {error}"
        )))
    })?;
    let mut start = start;
    start.push(b'\n');
    session
        .publish(crate::state::ProcessEventRecord {
            payload: Bytes::from(start),
            terminal: false,
        })
        .await;
    let response = stream_session(session.clone()).await?;
    tokio::spawn(async move {
        let producer =
            tokio::spawn(async move { conn.stream_process_events(&guest_id, event_tx).await });
        while let Some(event) = event_rx.recv().await {
            let terminal = matches!(
                &event,
                crate::agent::ExecStreamEvent::Exit(_) | crate::agent::ExecStreamEvent::Error(_)
            );
            if let Ok(encoded) = encode_process_event(event, session.pty) {
                session.publish(encoded).await;
            }
            if terminal {
                break;
            }
        }
        if let Ok(Err(error)) = producer.await
            && let Ok(encoded) = encode_process_event(
                crate::agent::ExecStreamEvent::Error(error.message),
                session.pty,
            )
        {
            session.publish(encoded).await;
        }
    });
    Ok(response)
}

pub async fn process_start(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bProcessStartRequest>,
) -> Result<Response, ApiError> {
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    resume_for_io(&state, &principal, &sandbox_id).await?;
    let E2bProcessConfig {
        cmd,
        args,
        envs,
        cwd,
        timeout,
        tag,
        pty,
        stdin,
    } = request.process;
    if cmd.trim().is_empty() {
        return Err(ApiError(ClouisleError::validation(
            "process.cmd is required",
        )));
    }
    let conn = crate::handlers::files::get_conn(&state, &sandbox_id, &principal)
        .await
        .map_err(ApiError)?;
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(cmd);
    argv.extend(args);
    stream_process(
        state,
        sandbox_id,
        conn,
        ProcessLaunch {
            config: json!({"cmd": argv[0], "args": &argv[1..], "envs": envs, "cwd": cwd, "tag": tag, "pty": pty, "stdin": stdin}),
            tag,
            argv,
            env: envs,
            cwd,
            timeout_ms: timeout.unwrap_or(30_000),
            stdin,
            pty,
        },
    )
    .await
}

pub async fn process_list(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    let _ = get_owned(&state, &principal, &sandbox_id).await?;
    let processes = state
        .processes
        .list(&sandbox_id)
        .await
        .into_iter()
        .map(|session| {
            json!({
                "config": session.config,
                "pid": session.pid,
                "tag": null,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"processes": processes})))
}

pub async fn envd_init(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<StatusCode, ApiError> {
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    let sandbox = get_owned(&state, &principal, &sandbox_id).await?;
    if let Some(env_vars) = body.get("envVars").and_then(serde_json::Value::as_object) {
        let mut spec = sandbox.spec;
        spec.env = env_vars
            .iter()
            .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
            .collect();
        state.store.update_sandbox_spec(&sandbox_id, &spec).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn envd_envs(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
) -> Result<Json<std::collections::HashMap<String, String>>, ApiError> {
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    Ok(Json(
        get_owned(&state, &principal, &sandbox_id).await?.spec.env,
    ))
}

pub async fn envd_metrics(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    let sandbox = get_owned(&state, &principal, &sandbox_id).await?;
    let stats = state
        .vmm
        .stats(&handle_for(&sandbox))
        .await
        .unwrap_or_default();
    Ok(Json(serde_json::json!({
        "cpuCount": sandbox.spec.resources.vcpu,
        "cpuUsedPct": stats.vcpu_usage.unwrap_or(0.0),
        "memUsed": stats.mem_used_mb.unwrap_or(0) * 1024 * 1024,
        "memTotal": sandbox.spec.resources.memory_mb as u64 * 1024 * 1024,
        "diskUsed": 0,
        "diskTotal": sandbox.spec.resources.disk_mb as u64 * 1024 * 1024,
    })))
}

pub async fn envd_operation_unavailable() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

#[derive(Debug, Deserialize)]
pub struct E2bComposeRequest {
    pub source_paths: Vec<String>,
    pub destination: String,
}

pub async fn envd_compose(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bComposeRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if request.source_paths.is_empty() || request.destination.is_empty() {
        return Err(ApiError(ClouisleError::validation(
            "source_paths and destination are required",
        )));
    }
    crate::handlers::files::validate_path(&request.destination).map_err(ApiError)?;
    for path in &request.source_paths {
        crate::handlers::files::validate_path(path).map_err(ApiError)?;
    }
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    resume_for_io(&state, &principal, &sandbox_id).await?;
    let conn = crate::handlers::files::get_conn(&state, &sandbox_id, &principal)
        .await
        .map_err(ApiError)?;
    let mut content = Vec::new();
    for path in &request.source_paths {
        content.extend_from_slice(&conn.read_file(path).await.map_err(ApiError)?);
    }
    conn.write_file(&request.destination, bytes::Bytes::from(content), 0o644)
        .await
        .map_err(ApiError)?;
    for path in &request.source_paths {
        let _ = conn
            .exec(
                vec!["rm".into(), "-f".into(), path.clone()],
                std::collections::HashMap::new(),
                None,
                30_000,
            )
            .await;
    }
    Ok(Json(
        serde_json::json!({"path": request.destination, "name": request.destination.rsplit('/').next().unwrap_or_default(), "type": "file"}),
    ))
}

pub async fn process_connect(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bProcessConnectRequest>,
) -> Result<Response, ApiError> {
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    resume_for_io(&state, &principal, &sandbox_id).await?;
    let selector = request.process;
    let session = if let Some(pid) = selector.pid {
        state.processes.get(&sandbox_id, pid).await
    } else if let Some(tag) = selector.tag {
        state.processes.get_by_tag(&sandbox_id, &tag).await
    } else {
        None
    };
    let Some(session) = session else {
        return Err(ApiError(ClouisleError::not_found(format!(
            "process not found in sandbox {sandbox_id}"
        ))));
    };
    stream_session(session).await
}

async fn resolve_process_session(
    state: &AppState,
    principal: &Principal,
    sandbox_id: &str,
    selector: E2bProcessSelector,
) -> Result<std::sync::Arc<crate::state::ProcessSession>, ApiError> {
    let _ = get_owned(state, principal, sandbox_id).await?;
    let session = if let Some(pid) = selector.pid {
        state.processes.get(sandbox_id, pid).await
    } else if let Some(tag) = selector.tag {
        state.processes.get_by_tag(sandbox_id, &tag).await
    } else {
        None
    };
    session.ok_or_else(|| {
        ApiError(ClouisleError::not_found(format!(
            "process not found in sandbox {sandbox_id}"
        )))
    })
}

pub async fn process_send_input(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bSendInputRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    resume_for_io(&state, &principal, &sandbox_id).await?;
    let session = resolve_process_session(&state, &principal, &sandbox_id, request.process).await?;
    let encoded = request.input.stdin.or(request.input.pty).ok_or_else(|| {
        ApiError(ClouisleError::validation(
            "input.stdin or input.pty is required",
        ))
    })?;
    let chunk = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| {
            ApiError(ClouisleError::validation(format!(
                "input must be base64: {error}"
            )))
        })?;
    let conn = crate::handlers::files::get_conn(&state, &sandbox_id, &principal)
        .await
        .map_err(ApiError)?;
    conn.send_stdin(&session.guest_id, bytes::Bytes::from(chunk))
        .await?;
    Ok(Json(json!({})))
}

pub async fn process_stream_input(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // StreamInput 单消息：`{"event":{"data":{"input":{"stdin":"base64"}}}}`。
    let input = request
        .get("event")
        .and_then(|event| event.get("data"))
        .and_then(|data| data.get("input"))
        .cloned()
        .ok_or_else(|| {
            ApiError(ClouisleError::validation(
                "expected event.data.input message",
            ))
        })?;
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    resume_for_io(&state, &principal, &sandbox_id).await?;
    let session = resolve_process_session(
        &state,
        &principal,
        &sandbox_id,
        E2bProcessSelector {
            pid: input
                .get("process")
                .and_then(|process| process.get("pid"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok()),
            tag: input
                .get("process")
                .and_then(|process| process.get("tag"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        },
    )
    .await?;
    let encoded = input
        .get("input")
        .and_then(|inner| inner.get("stdin"))
        .or_else(|| input.get("input").and_then(|inner| inner.get("pty")))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ApiError(ClouisleError::validation("input.stdin is required")))?;
    let chunk = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| {
            ApiError(ClouisleError::validation(format!(
                "input must be base64: {error}"
            )))
        })?;
    let conn = crate::handlers::files::get_conn(&state, &sandbox_id, &principal)
        .await
        .map_err(ApiError)?;
    conn.send_stdin(&session.guest_id, bytes::Bytes::from(chunk))
        .await?;
    Ok(Json(json!({})))
}

pub async fn process_send_signal(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bSendSignalRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    resume_for_io(&state, &principal, &sandbox_id).await?;
    let session = resolve_process_session(&state, &principal, &sandbox_id, request.process).await?;
    let signal = request.signal.into_signal()?;
    let conn = crate::handlers::files::get_conn(&state, &sandbox_id, &principal)
        .await
        .map_err(ApiError)?;
    conn.send_signal(&session.guest_id, signal).await?;
    Ok(Json(json!({})))
}

pub async fn process_close_stdin(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bCloseStdinRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    resume_for_io(&state, &principal, &sandbox_id).await?;
    let session = resolve_process_session(&state, &principal, &sandbox_id, request.process).await?;
    let conn = crate::handlers::files::get_conn(&state, &sandbox_id, &principal)
        .await
        .map_err(ApiError)?;
    conn.close_stdin(&session.guest_id).await?;
    Ok(Json(json!({})))
}

pub async fn process_update(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: axum::http::HeaderMap,
    Json(request): Json<E2bResizeRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sandbox_id = sandbox_id_from_headers(&headers)?;
    resume_for_io(&state, &principal, &sandbox_id).await?;
    let session = resolve_process_session(&state, &principal, &sandbox_id, request.process).await?;
    if !session.pty {
        return Err(ApiError(ClouisleError::validation(format!(
            "process {} is not a PTY session",
            session.pid
        ))));
    }
    let Some(size) = request.pty else {
        return Err(ApiError(ClouisleError::validation(
            "pty size is required for update",
        )));
    };
    let conn = crate::handlers::files::get_conn(&state, &sandbox_id, &principal)
        .await
        .map_err(ApiError)?;
    conn.resize_pty(&session.guest_id, size.size.cols, size.size.rows)
        .await?;
    Ok(Json(json!({})))
}
