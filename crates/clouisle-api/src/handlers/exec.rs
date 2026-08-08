//! 命令执行 handler（FR-02）：同步 + 流式 + 历史查询。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use clouisle_core::{ClouisleError, ExecutionRecord, ExecutionSpec, truncate_output};

use crate::error::ApiError;
use crate::state::AppState;

/// 由 VmmMeta 构造 VmHandle。
pub(crate) fn meta_to_handle(
    meta: &clouisle_core::VmmMeta,
    sandbox_id: &str,
) -> clouisle_vmm::VmHandle {
    clouisle_vmm::VmHandle {
        id: meta
            .vmm_id
            .clone()
            .unwrap_or_else(|| sandbox_id.to_string()),
        backend: meta.backend.clone(),
        pid: meta.pid,
        api_socket: meta.api_socket.clone(),
        vsock_socket: meta.vsock_socket.clone(),
        vsock_cid: meta.vsock_cid,
    }
}

/// `POST .../exec` 请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// 执行超时（毫秒）
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// 流式模式（SSE）
    #[serde(default)]
    pub stream: bool,
}

fn default_timeout() -> u64 {
    30_000
}

/// `GET .../exec/{exec_id}` 请求。
#[derive(Debug, Deserialize)]
pub struct ExecQuery {
    pub limit: Option<usize>,
}

/// 执行响应（同步模式）。
#[derive(Debug, Serialize)]
pub struct ExecResponse {
    pub exec_id: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
}

/// 检查沙盒可执行，返回 vmm handle。
async fn ensure_executable(
    state: &AppState,
    sandbox_id: &str,
) -> Result<clouisle_core::Sandbox, ApiError> {
    let sb = state.store.get_sandbox(sandbox_id).await?;
    if !sb.is_executable() {
        return Err(ApiError(ClouisleError::invalid_state(format!(
            "sandbox {sandbox_id} is not running (status={})",
            sb.status
        ))));
    }
    Ok(sb)
}

/// 同步执行。
pub async fn exec_sync(
    State(state): State<AppState>,
    Path(sandbox_id): Path<String>,
    Json(req): Json<ExecRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let spec = ExecutionSpec {
        argv: req.argv.clone(),
        env: req.env.clone(),
        cwd: req.cwd.clone(),
        timeout_ms: req.timeout_ms,
    };
    spec.validate()?;

    let sb = ensure_executable(&state, &sandbox_id).await?;
    let handle = meta_to_handle(&sb.vmm_meta, &sandbox_id);

    let conn = state.agent.connect_and_hello(&handle, &sandbox_id).await?;
    let result = conn
        .exec(
            req.argv.clone(),
            req.env.clone(),
            req.cwd.clone(),
            req.timeout_ms,
        )
        .await?;

    // 持久化执行记录
    let exec_id = uuid::Uuid::now_v7().to_string();
    let (stdout, stdout_truncated) = truncate_output(&result.stdout);
    let (stderr, stderr_truncated) = truncate_output(&result.stderr);
    let record = ExecutionRecord {
        id: exec_id.clone(),
        sandbox_id: sandbox_id.clone(),
        spec,
        exit_code: result.exit_code,
        stdout,
        stderr,
        started_at: chrono::Utc::now(),
        finished_at: chrono::Utc::now(),
        timed_out: result.exit_code == -1,
        stdout_truncated,
        stderr_truncated,
        node_id: None,
    };
    state.store.save_execution(&record).await?;
    clouisle_obs::metrics::record_exec_duration(result.duration_ms as f64);

    let resp = ExecResponse {
        exec_id,
        exit_code: result.exit_code,
        stdout: String::from_utf8_lossy(&result.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&result.stderr).into_owned(),
        duration_ms: result.duration_ms,
        timed_out: result.exit_code == -1,
        stdout_truncated,
        stderr_truncated,
    };
    Ok((StatusCode::OK, Json(resp)))
}

/// 流式执行（SSE）。
pub async fn exec_stream(
    State(state): State<AppState>,
    Path(sandbox_id): Path<String>,
    Json(req): Json<ExecRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // 简化：mock 模式下仍走同步路径，但以 SSE 格式返回。
    let spec = ExecutionSpec {
        argv: req.argv.clone(),
        env: req.env.clone(),
        cwd: req.cwd.clone(),
        timeout_ms: req.timeout_ms,
    };
    spec.validate()?;
    let sb = ensure_executable(&state, &sandbox_id).await?;
    let handle = meta_to_handle(&sb.vmm_meta, &sandbox_id);
    let conn = state.agent.connect_and_hello(&handle, &sandbox_id).await?;
    let result = conn
        .exec(req.argv, req.env, req.cwd, req.timeout_ms)
        .await?;

    let stream = tokio_stream_fallback(result);
    Ok(axum::response::Sse::new(stream))
}

fn tokio_stream_fallback(
    result: clouisle_core::execution::ExecutionResult,
) -> impl futures::Stream<Item = std::result::Result<axum::response::sse::Event, std::convert::Infallible>>
{
    use futures::stream;
    let mut events = Vec::new();
    if !result.stdout.is_empty() {
        events.push(Ok(axum::response::sse::Event::default()
            .event("stdout")
            .data(String::from_utf8_lossy(&result.stdout))));
    }
    if !result.stderr.is_empty() {
        events.push(Ok(axum::response::sse::Event::default()
            .event("stderr")
            .data(String::from_utf8_lossy(&result.stderr))));
    }
    events.push(Ok(axum::response::sse::Event::default()
        .event("exit")
        .data(format!("{}", result.exit_code))));
    stream::iter(events)
}

/// `GET .../exec` 历史。
pub async fn list_executions(
    State(state): State<AppState>,
    Path(sandbox_id): Path<String>,
    Query(_q): Query<ExecQuery>,
) -> Result<Json<Vec<ExecutionRecord>>, ApiError> {
    let list = state.store.list_executions(&sandbox_id).await?;
    Ok(Json(list))
}

/// `GET .../exec/{exec_id}` 单条。
pub async fn get_execution(
    State(state): State<AppState>,
    Path((sandbox_id, exec_id)): Path<(String, String)>,
) -> Result<Json<ExecutionRecord>, ApiError> {
    let rec = state.store.get_execution(&exec_id).await?;
    if rec.sandbox_id != sandbox_id {
        return Err(ApiError(ClouisleError::not_found(format!(
            "execution {exec_id} not in sandbox {sandbox_id}"
        ))));
    }
    Ok(Json(rec))
}
