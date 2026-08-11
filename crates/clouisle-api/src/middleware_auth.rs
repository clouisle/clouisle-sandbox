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
fn volume_credential_path(path: &str) -> bool {
    path.starts_with("/volumecontent/")
        && !matches!(
            path,
            "/volumecontent/health" | "/volumecontent/init" | "/volumecontent/metrics"
        )
}
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
    if let Some(admin_token) = std::env::var_os("CLOUISLE_ADMIN_TOKEN")
        && req
            .headers()
            .get("x-admin-token")
            .or_else(|| req.headers().get("authorization"))
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                let credential = value.strip_prefix("Bearer ").unwrap_or(value);
                credential == admin_token.to_string_lossy()
            })
    {
        req.extensions_mut().insert(Principal::dev());
        return next.run(req).await;
    }
    // Development bypass is allowed only when the caller did not present a credential.
    if state.auth.is_empty().await
        && state.auth.allows_anonymous_dev()
        && req.headers().get("authorization").is_none()
        && req.headers().get("x-api-key").is_none()
        && req.headers().get("x-access-token").is_none()
    {
        req.extensions_mut().insert(Principal::dev());
        return next.run(req).await;
    }
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| {
            req.headers()
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .map(|value| format!("Bearer {value}"))
        })
        .or_else(|| {
            req.headers()
                .get("x-access-token")
                .and_then(|value| value.to_str().ok())
                .map(|value| format!("Bearer {value}"))
        });

    match state.auth.authenticate(auth_header.as_deref()).await {
        Ok(principal) => {
            if principal.volume_id.is_some() && !volume_credential_path(path) {
                let (status, body) =
                    crate::error::into_error_response(clouisle_core::ClouisleError::new(
                        clouisle_core::ErrorKind::Forbidden,
                        "volume credential is limited to its volume content API",
                    ));
                return (status, body).into_response();
            }
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
        Err(error) => {
            if let Some(credential) = auth_header
                .as_deref()
                .and_then(|value| value.strip_prefix("Bearer "))
                && let Some(principal) = state.e2b.authenticate(credential).await
            {
                if principal.volume_id.is_some() && !volume_credential_path(path) {
                    let (status, body) =
                        crate::error::into_error_response(clouisle_core::ClouisleError::new(
                            clouisle_core::ErrorKind::Forbidden,
                            "volume credential is limited to its volume content API",
                        ));
                    return (status, body).into_response();
                }
                if matches!(
                    req.method(),
                    &Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE
                ) && let Err(write_error) = state.auth.require_write(&principal)
                {
                    let (status, body) = crate::error::into_error_response(write_error);
                    return (status, body).into_response();
                }
                req.extensions_mut().insert(principal);
                return next.run(req).await;
            }
            let (status, body) = crate::error::into_error_response(error);
            (status, body).into_response()
        }
    }
}

/// 提取当前请求的 Principal。
pub fn current_principal(req: &Request) -> Option<&Principal> {
    req.extensions().get::<Principal>()
}
