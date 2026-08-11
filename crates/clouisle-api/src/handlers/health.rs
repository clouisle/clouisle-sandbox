//! 健康检查与 metrics 端点。

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Serialize;

use crate::state::AppState;
use std::sync::atomic::Ordering;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub store: String,
    pub version: &'static str,
}

/// `GET /health` — 基本存活检查。
pub async fn health(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if headers.get("e2b-sandbox-id").is_some() {
        return StatusCode::NO_CONTENT.into_response();
    }
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
    (code, Json(resp)).into_response()
}

/// `GET /health/live` — 进程存活探针（AR-04）。
pub async fn liveness() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "status": "alive" })),
    )
}

/// `GET /health/ready` — DB 可达且服务未进入优雅退出。
/// 多节点状态由 reconciler 与节点租约单独收敛。
pub async fn readiness(State(state): State<AppState>) -> impl IntoResponse {
    let store_ok = state.store.list_sandboxes(None).await.is_ok();
    let ready = store_ok && !state.draining.load(Ordering::Acquire);
    if ready {
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
pub async fn metrics(headers: HeaderMap) -> impl IntoResponse {
    if headers.get("e2b-sandbox-id").is_some() {
        return Json(serde_json::json!({"cpuCount": 0, "cpuUsedPct": 0.0, "memUsed": 0, "memTotal": 0, "diskUsed": 0, "diskTotal": 0})).into_response();
    }
    let body = crate::metrics::render();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
        .into_response()
}
