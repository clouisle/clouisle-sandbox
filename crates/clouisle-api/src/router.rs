//! Axum 路由构建。

use axum::Router;
use axum::routing::{delete, get, patch, post, put};

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
        .route(
            "/api/v1/sandboxes/{id}/recover",
            post(handlers::sandbox::recover_sandbox),
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
            get(handlers::e2b::get).delete(handlers::e2b::delete),
        )
        .route(
            "/sandboxes/{sandbox_id}/connect",
            post(handlers::e2b::connect),
        )
        .route(
            "/sandboxes/{sandbox_id}/refreshes",
            post(handlers::e2b::refresh),
        )
        .route("/sandboxes/{sandbox_id}/pause", post(handlers::e2b::pause))
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
        .route(
            "/sandboxes/{sandbox_id}/network",
            put(handlers::e2b::update_network),
        )
        // E2B envd-compatible filesystem/process Connect endpoints.
        .route(
            "/files",
            post(handlers::e2b::upload_file)
                .put(handlers::e2b::upload_file)
                .get(handlers::e2b::download_file),
        )
        .route(
            "/filesystem.Filesystem/ListDir",
            post(handlers::e2b::list_dir),
        )
        .route("/process.Process/Start", post(handlers::e2b::process_start))
        .route(
            "/filesystem.Filesystem/Stat",
            post(handlers::e2b::filesystem_stat),
        )
        .route(
            "/filesystem.Filesystem/MakeDir",
            post(handlers::e2b::filesystem_make_dir),
        )
        .route(
            "/filesystem.Filesystem/Move",
            post(handlers::e2b::filesystem_move),
        )
        .route(
            "/filesystem.Filesystem/Remove",
            post(handlers::e2b::filesystem_remove),
        )
        .route(
            "/filesystem.Filesystem/WatchDir",
            post(handlers::e2b::filesystem_watch_dir),
        )
        .route(
            "/filesystem.Filesystem/CreateWatcher",
            post(handlers::e2b::filesystem_create_watcher),
        )
        .route(
            "/filesystem.Filesystem/GetWatcherEvents",
            post(handlers::e2b::filesystem_get_watcher_events),
        )
        .route(
            "/filesystem.Filesystem/RemoveWatcher",
            post(handlers::e2b::filesystem_remove_watcher),
        )
        .route("/process.Process/List", post(handlers::e2b::process_list))
        .route(
            "/process.Process/Connect",
            post(handlers::e2b::process_connect),
        )
        .route(
            "/process.Process/Update",
            post(handlers::e2b::process_update),
        )
        .route(
            "/process.Process/StreamInput",
            post(handlers::e2b::process_stream_input),
        )
        .route(
            "/process.Process/SendInput",
            post(handlers::e2b::process_send_input),
        )
        .route(
            "/process.Process/SendSignal",
            post(handlers::e2b::process_send_signal),
        )
        .route(
            "/process.Process/CloseStdin",
            post(handlers::e2b::process_close_stdin),
        )
        .route("/init", post(handlers::e2b::envd_init))
        .route("/envs", get(handlers::e2b::envd_envs))
        .route("/files/compose", post(handlers::e2b::envd_compose))
        .route("/freeze", post(handlers::e2b::envd_operation_unavailable))
        .route("/unfreeze", post(handlers::e2b::envd_operation_unavailable))
        .route("/collapse", post(handlers::e2b::envd_operation_unavailable))
        .route("/fsfreeze", post(handlers::e2b::envd_operation_unavailable))
        .route("/fsthaw", post(handlers::e2b::envd_operation_unavailable))
        // E2B cloud-control-plane resources.
        .route(
            "/teams",
            get(handlers::e2b_cloud::list_teams).post(handlers::e2b_cloud::create_team),
        )
        .route(
            "/teams/{teamID}/members",
            get(handlers::e2b_cloud::team_members),
        )
        .route(
            "/teams/{teamID}/metrics",
            get(handlers::e2b_cloud::team_metrics),
        )
        .route(
            "/teams/{teamID}/metrics/max",
            get(handlers::e2b_cloud::team_metrics_max),
        )
        .route(
            "/api-keys",
            get(handlers::e2b_cloud::list_api_keys).post(handlers::e2b_cloud::create_api_key),
        )
        .route(
            "/api-keys/{apiKeyID}",
            patch(handlers::e2b_cloud::update_api_key).delete(handlers::e2b_cloud::delete_api_key),
        )
        .route(
            "/access-tokens",
            post(handlers::e2b_cloud::create_access_token),
        )
        .route(
            "/access-tokens/{accessTokenID}",
            delete(handlers::e2b_cloud::delete_access_token),
        )
        .route(
            "/volumes",
            get(handlers::e2b_cloud::list_volumes).post(handlers::e2b_cloud::create_volume),
        )
        .route(
            "/volumes/{volumeID}",
            get(handlers::e2b_cloud::get_volume).delete(handlers::e2b_cloud::delete_volume),
        )
        .route(
            "/volumecontent/{volumeID}/path",
            get(handlers::e2b_cloud::volume_path),
        )
        .route(
            "/volumecontent/{volumeID}/dir",
            get(handlers::e2b_cloud::volume_dir),
        )
        .route(
            "/volumecontent/{volumeID}/file",
            get(handlers::e2b_cloud::volume_file).put(handlers::e2b_cloud::volume_file),
        )
        .route(
            "/v3/templates",
            post(handlers::e2b_cloud::create_template_v3),
        )
        .route(
            "/v2/templates",
            get(handlers::e2b_cloud::list_templates).post(handlers::e2b_cloud::create_template_v2),
        )
        .route(
            "/templates",
            get(handlers::e2b_cloud::list_templates).post(handlers::e2b_cloud::create_template_v2),
        )
        .route(
            "/templates/{templateID}/files/{hash}",
            get(handlers::e2b_cloud::upload_template_file),
        )
        .route(
            "/templates/{templateID}",
            get(handlers::e2b_cloud::list_template_builds),
        )
        .route(
            "/templates/{templateID}/builds/{buildID}",
            post(handlers::e2b_cloud::start_template_build),
        )
        .route(
            "/v2/templates/{templateID}/builds/{buildID}",
            post(handlers::e2b_cloud::build_template_v2),
        )
        .route(
            "/v2/templates/{templateID}",
            patch(handlers::e2b_cloud::update_template),
        )
        .route(
            "/templates/{templateID}/builds/{buildID}/status",
            get(handlers::e2b_cloud::get_build_status),
        )
        .route(
            "/templates/{templateID}/builds/{buildID}/logs",
            get(handlers::e2b_cloud::get_build_logs),
        )
        .route(
            "/templates/tags",
            post(handlers::e2b_cloud::assign_template_tags),
        )
        .route(
            "/templates/{templateID}/tags",
            get(handlers::e2b_cloud::list_template_tags),
        )
        .route(
            "/templates/aliases/{alias}",
            get(handlers::e2b_cloud::resolve_template_alias),
        )
        .route("/nodes", get(handlers::e2b_cloud::list_nodes))
        .route("/nodes/{nodeID}", get(handlers::e2b_cloud::get_node))
        .route(
            "/sandboxes/metrics",
            get(handlers::e2b_cloud::list_sandbox_metrics),
        )
        .route(
            "/sandboxes/{sandboxID}/logs",
            get(handlers::e2b_cloud::sandbox_logs),
        )
        .route(
            "/v2/sandboxes/{sandboxID}/logs",
            get(handlers::e2b_cloud::sandbox_logs),
        )
        .route(
            "/sandboxes/{sandboxID}/metrics",
            get(handlers::e2b_cloud::sandbox_metrics),
        )
        .route(
            "/sandboxes/{sandboxID}/fork",
            post(handlers::e2b_cloud::fork_sandbox),
        )
        .route(
            "/sandboxes/{sandboxID}/snapshots",
            post(handlers::e2b_cloud::create_snapshot),
        )
        .route("/snapshots", get(handlers::e2b_cloud::list_snapshots))
        .route(
            "/admin/teams/{teamID}/sandboxes/kill",
            post(handlers::e2b_cloud::admin_kill_team),
        )
        .route(
            "/admin/teams/{teamID}/builds/cancel",
            post(handlers::e2b_cloud::admin_cancel_builds),
        )
        .route(
            "/admin/teams/{teamID}/api-keys",
            post(handlers::e2b_cloud::admin_create_key),
        )
        .route(
            "/admin/teams/{teamID}/api-keys/{apiKeyID}",
            delete(handlers::e2b_cloud::admin_delete_key),
        )
        .route(
            "/volumecontent/health",
            get(handlers::e2b_cloud::volume_content_health),
        )
        .route(
            "/volumecontent/init",
            post(handlers::e2b_cloud::volume_content_init),
        )
        .route(
            "/volumecontent/metrics",
            get(handlers::e2b_cloud::volume_content_metrics),
        )
        .route("/healthz", get(handlers::e2b_cloud::health_204))
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
