//! Axum 路由构建。

use axum::Router;
use axum::routing::{delete, get, post};

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
        // Asynchronous OCI image preparation.
        .route(
            "/api/v1/images/prefetch",
            post(handlers::images::prefetch_images),
        )
        .route(
            "/api/v1/images/prefetch/{job_id}",
            get(handlers::images::get_prefetch_job),
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
        // Authenticated node registry and heartbeat lease updates.
        .route("/api/v1/nodes", post(handlers::nodes::upsert_node))
        .route("/api/v1/nodes", get(handlers::nodes::list_ready_nodes))
        // E2B public sandbox platform compatibility.
        .route("/sandboxes", post(handlers::e2b::create))
        .route("/sandboxes", get(handlers::e2b::list))
        .route("/v2/sandboxes", get(handlers::e2b::v2_list))
        .route(
            "/sandboxes/{sandbox_id}",
            get(handlers::e2b::get).delete(handlers::sandbox::delete_sandbox),
        )
        .route(
            "/sandboxes/{sandbox_id}/connect",
            post(handlers::e2b::connect),
        )
        .route(
            "/sandboxes/{sandbox_id}/pause",
            post(handlers::e2b::pause),
        )
        .route(
            "/sandboxes/{sandbox_id}/resume",
            post(handlers::e2b::resume),
        )
        .route(
            "/sandboxes/{sandbox_id}/timeout",
            post(handlers::e2b::set_timeout),
        )
        .route(
            "/sandboxes/{sandbox_id}/refresh",
            post(handlers::e2b::refresh),
        )
        // E2B envd-compatible filesystem/process Connect endpoints.
        .route(
            "/files",
            post(handlers::e2b::upload_file).get(handlers::e2b::download_file),
        )
        .route(
            "/filesystem.Filesystem/ListDir",
            post(handlers::e2b::list_dir),
        )
        .route("/process.Process/Start", post(handlers::e2b::process_start))
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
