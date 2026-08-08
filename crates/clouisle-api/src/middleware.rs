//! 中间件：请求 ID、计时、tracing span。

use std::time::Instant;

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

use crate::metrics;

/// 请求 ID 中间件：X-Request-Id 透传或生成 UUID v7。
pub async fn request_id(
    req: Request,
    next: Next,
) -> Response {
    let req_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    tracing::info_span!("request", request_id = %req_id).in_scope(|| {});

    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let start = Instant::now();

    let mut resp = next.run(req).await;

    let status = resp.status();
    let header = HeaderValue::from_str(&req_id).unwrap_or_else(|_| HeaderValue::from_static("unknown"));
    resp.headers_mut().insert("x-request-id", header);

    let duration_ms = start.elapsed().as_millis() as f64;
    metrics::record_api_request(&method.to_string(), &path, status.as_u16(), duration_ms);
    tracing::debug!(request_id = %req_id, method = %method, path = %path, status = status.as_u16(), duration_ms, "request completed");

    resp
}