//! 健康检查与 metrics 端点。

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub store: String,
    pub version: &'static str,
}

/// `GET /health` — 基本存活检查。
pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let store_status = match state.store.list_sandboxes(None).await {
        Ok(_) => "ok",
        Err(_) => "error",
    };
    let status = if store_status == "ok" {
        "ok"
    } else {
        "degraded"
    };
    let code = if store_status == "ok" {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let resp = HealthResponse {
        status: status.into(),
        store: store_status.into(),
        version: state.version,
    };
    (code, Json(resp))
}

/// `GET /health/live` — 进程存活探针（AR-04）。
pub async fn liveness() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "status": "alive" })),
    )
}

/// `GET /health/ready` — 就绪探针（DB 可达 + 至少 1 节点 ready）。
/// Phase 3 单机：store 可用即 ready。
pub async fn readiness(State(state): State<AppState>) -> impl IntoResponse {
    let store_ok = state.store.list_sandboxes(None).await.is_ok();
    if store_ok {
        (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "ready" })),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "status": "not_ready" })),
        )
    }
}

/// `GET /metrics` — Prometheus 格式指标。
pub async fn metrics() -> impl IntoResponse {
    let body = crate::metrics::render();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}
