//! Control-plane node registry endpoints.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use clouisle_core::RegisteredNode;

use crate::error::ApiError;
use crate::state::AppState;

/// Upsert a node registration or heartbeat lease.
pub async fn upsert_node(
    State(state): State<AppState>,
    Json(node): Json<RegisteredNode>,
) -> Result<StatusCode, ApiError> {
    if node.endpoint.is_empty() {
        return Err(ApiError(clouisle_core::ClouisleError::validation(
            "node endpoint is required",
        )));
    }
    state.store.upsert_node(&node).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List nodes with a currently valid heartbeat lease.
pub async fn list_ready_nodes(
    State(state): State<AppState>,
) -> Result<Json<Vec<RegisteredNode>>, ApiError> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    // The caller supplies a heartbeat deadline in every record; the initial
    // rollout treats any current heartbeat as live for 15 seconds.
    Ok(Json(state.store.list_ready_nodes(now_ms - 15_000).await?))
}
