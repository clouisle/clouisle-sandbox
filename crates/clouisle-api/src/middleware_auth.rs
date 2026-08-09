//! 认证中间件：校验每个请求的 Authorization header。
//!
//! 受保护路径需要 `Bearer <key>`。`/health` 与 `/metrics` 例外（探测端点）。

use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::auth::Principal;
use crate::state::AppState;

/// 认证中间件。将 Principal 存入 request extensions。
///
/// 使用 state 提取真实 AppState；应用时需用 `from_fn_with_state`。
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    // Kubernetes probes and Prometheus scraping must remain unauthenticated.
    if matches!(
        path,
        "/health" | "/health/live" | "/health/ready" | "/metrics"
    ) {
        return next.run(req).await;
    }

    // Development bypass is explicit on the authenticator; production instances
    // created by main use a fail-closed authenticator.
    if state.auth.is_empty().await && state.auth.allows_anonymous_dev() {
        req.extensions_mut().insert(Principal::dev());
        return next.run(req).await;
    }

    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    match state.auth.authenticate(header).await {
        Ok(principal) => {
            if matches!(
                req.method(),
                &Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE
            ) && let Err(e) = state.auth.require_write(&principal)
            {
                let (status, body) = crate::error::into_error_response(e);
                return (status, body).into_response();
            }
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
        Err(e) => {
            let (status, body) = crate::error::into_error_response(e);
            (status, body).into_response()
        }
    }
}

/// 提取当前请求的 Principal。
pub fn current_principal(req: &Request) -> Option<&Principal> {
    req.extensions().get::<Principal>()
}
