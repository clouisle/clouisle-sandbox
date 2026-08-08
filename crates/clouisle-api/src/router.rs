//! Axum 路由构建。

use axum::routing::{delete, get, post};
use axum::Router;

use crate::handlers;
use crate::middleware;
use crate::middleware_auth;
use crate::state::AppState;

/// 构建全部 API 路由。
pub fn build_router(state: AppState) -> Router {
    Router::new()
        // 沙盒生命周期
        .route("/api/v1/sandboxes", post(handlers::sandbox::create_sandbox))
        .route("/api/v1/sandboxes", get(handlers::sandbox::list_sandboxes))
        .route(
            "/api/v1/sandboxes/{id}",
            get(handlers::sandbox::get_sandbox),
        )
        .route(
            "/api/v1/sandboxes/{id}",
            delete(handlers::sandbox::delete_sandbox),
        )
        // 命令执行
        .route(
            "/api/v1/sandboxes/{id}/exec",
            post(handlers::exec::exec_sync),
        )
        .route(
            "/api/v1/sandboxes/{id}/exec/stream",
            post(handlers::exec::exec_stream),
        )
        .route(
            "/api/v1/sandboxes/{id}/exec/{exec_id}",
            get(handlers::exec::get_execution),
        )
        .route(
            "/api/v1/sandboxes/{id}/exec",
            get(handlers::exec::list_executions),
        )
        // 文件传输（Phase 2）
        .route(
            "/api/v1/sandboxes/{id}/files/upload",
            post(handlers::files::upload_file),
        )
        .route(
            "/api/v1/sandboxes/{id}/files/download",
            get(handlers::files::download_file),
        )
        .route(
            "/api/v1/sandboxes/{id}/files/ls",
            get(handlers::files::list_files),
        )
        // 可观测性
        .route("/health", get(handlers::health::health))
        .route("/health/live", get(handlers::health::liveness))
        .route("/health/ready", get(handlers::health::readiness))
        .route("/metrics", get(handlers::health::metrics))
        // 中间件：认证在外层，请求 ID 在内层
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware_auth::auth_middleware,
        ))
        .layer(axum::middleware::from_fn(middleware::request_id))
        .with_state(state)
}