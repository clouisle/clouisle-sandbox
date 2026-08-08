//! 文件传输 handler（FR-07）。

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use clouisle_core::ClouisleError;

use crate::handlers::exec::meta_to_handle;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct FsQuery {
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct LsResponse {
    pub items: Vec<clouisle_core::DirEntry>,
}

/// 上传大小上限（50 MB）。
pub const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;

/// 检查沙盒可执行且拿到连接。
async fn get_conn(
    state: &AppState,
    sandbox_id: &str,
) -> Result<Box<dyn crate::agent::AgentConnection>, ClouisleError> {
    let sb = state.store.get_sandbox(sandbox_id).await?;
    if !sb.is_executable() {
        return Err(ClouisleError::invalid_state(format!(
            "sandbox {sandbox_id} is not running (status={})",
            sb.status
        )));
    }
    let handle = meta_to_handle(&sb.vmm_meta, sandbox_id);
    state.agent.connect_and_hello(&handle, sandbox_id).await
}

fn validate_path(path: &str) -> Result<(), ClouisleError> {
    if path.is_empty() {
        return Err(ClouisleError::validation("path is required"));
    }
    Ok(())
}

/// `POST .../files/upload?path=/work/file.txt` — body 为原始字节。
pub async fn upload_file(
    State(state): State<AppState>,
    Path(sandbox_id): Path<String>,
    Query(q): Query<FsQuery>,
    body: Bytes,
) -> Result<impl IntoResponse, crate::error::ApiError> {
    validate_path(&q.path).map_err(crate::error::ApiError)?;
    if body.len() > MAX_UPLOAD_BYTES {
        return Err(crate::error::ApiError(ClouisleError::validation(format!(
            "upload exceeds {MAX_UPLOAD_BYTES} bytes"
        ))));
    }
    let conn = get_conn(&state, &sandbox_id)
        .await
        .map_err(crate::error::ApiError)?;
    conn.write_file(&q.path, body, 0o644)
        .await
        .map_err(crate::error::ApiError)?;
    Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))))
}

/// `GET .../files/download?path=/work/output.txt`
pub async fn download_file(
    State(state): State<AppState>,
    Path(sandbox_id): Path<String>,
    Query(q): Query<FsQuery>,
) -> Result<impl IntoResponse, crate::error::ApiError> {
    validate_path(&q.path).map_err(crate::error::ApiError)?;
    let conn = get_conn(&state, &sandbox_id)
        .await
        .map_err(crate::error::ApiError)?;
    let data = conn
        .read_file(&q.path)
        .await
        .map_err(crate::error::ApiError)?;
    let filename = q.path.rsplit('/').next().unwrap_or("file");
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/octet-stream".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        data,
    ))
}

/// `GET .../files/ls?path=/work`
pub async fn list_files(
    State(state): State<AppState>,
    Path(sandbox_id): Path<String>,
    Query(q): Query<FsQuery>,
) -> Result<Json<LsResponse>, crate::error::ApiError> {
    validate_path(&q.path).map_err(crate::error::ApiError)?;
    let conn = get_conn(&state, &sandbox_id)
        .await
        .map_err(crate::error::ApiError)?;
    let entries = conn
        .list_dir(&q.path)
        .await
        .map_err(crate::error::ApiError)?;
    Ok(Json(LsResponse { items: entries }))
}
