//! Control-plane node registry endpoints.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use std::collections::HashSet;

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
    let observed: HashSet<&str> = node.sandbox_ids.iter().map(String::as_str).collect();
    for sandbox_id in &node.sandbox_ids {
        if let Ok(sandbox) = state.store.get_sandbox(sandbox_id).await
            && sandbox.node_id.as_deref() != Some(node.info.node_id.as_str())
        {
            state
                .store
                .update_sandbox_node(sandbox_id, Some(&node.info.node_id))
                .await?;
        }
    }
    for sandbox in state.store.list_sandboxes(None).await? {
        if sandbox.node_id.as_deref() == Some(node.info.node_id.as_str())
            && sandbox.status.is_active()
            && !observed.contains(sandbox.id.as_str())
        {
            state
                .store
                .update_sandbox_status_message(
                    &sandbox.id,
                    &clouisle_core::SandboxStatus::Error,
                    Some("node heartbeat no longer reports this sandbox"),
                )
                .await?;
            state.reservations.lock().await.remove(&sandbox.id);
        }
    }
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
