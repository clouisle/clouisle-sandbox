//! API 错误映射：`ClouisleError` → Axum HTTP 响应。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use clouisle_core::{ClouisleError, ErrorKind};

/// 统一错误响应体：嵌套 `error` 供自定义 API 客户端，顶层 `code`/`message`
/// 供官方 E2B SDK（其 Error schema 在顶层读取）。
#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
    pub code: String,
    pub message: String,
    pub details: serde_json::Value,
}

#[derive(Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    pub details: serde_json::Value,
}

/// 将 ClouisleError 转为 Axum 响应。
pub fn into_error_response(e: ClouisleError) -> (StatusCode, Json<ErrorResponse>) {
    let status = match e.kind {
        ErrorKind::Validation => StatusCode::BAD_REQUEST,
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        ErrorKind::ResourceExhausted => StatusCode::INSUFFICIENT_STORAGE,
        ErrorKind::InvalidState => StatusCode::CONFLICT,
        ErrorKind::Unauthenticated => StatusCode::UNAUTHORIZED,
        ErrorKind::Forbidden => StatusCode::FORBIDDEN,
        ErrorKind::QuotaExceeded => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let code = e.kind.to_string();
    let message = e.message;
    let body = ErrorResponse {
        code: code.clone(),
        message: message.clone(),
        details: serde_json::Value::Null,
        error: ErrorBody {
            code,
            message,
            details: serde_json::Value::Null,
        },
    };
    (status, Json(body))
}

/// 将 ClouisleError 转为 Axum 响应（IntoResponse 版本）。
pub fn into_response(e: ClouisleError) -> Response {
    let (status, body) = into_error_response(e);
    (status, body).into_response()
}

/// 便捷：字段级校验错误列表 → 400。
pub fn validation_errors(errors: &[clouisle_core::ValidationError]) -> Response {
    let body = ErrorResponse {
        error: ErrorBody {
            code: "VALIDATION".into(),
            message: "request validation failed".to_string(),
            details: serde_json::json!({ "errors": errors }),
        },
        code: "VALIDATION".into(),
        message: "request validation failed".to_string(),
        details: serde_json::json!({ "errors": errors }),
    };
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

/// 包装错误类型，用于 handler 返回 `Result<_, ApiError>`。
pub struct ApiError(pub ClouisleError);

impl From<ClouisleError> for ApiError {
    fn from(e: ClouisleError) -> Self {
        ApiError(e)
    }
}

impl From<clouisle_store::StoreError> for ApiError {
    fn from(e: clouisle_store::StoreError) -> Self {
        ApiError(ClouisleError::from(e))
    }
}

impl From<crate::e2b_cloud::ControlPlaneError> for ApiError {
    fn from(error: crate::e2b_cloud::ControlPlaneError) -> Self {
        use crate::e2b_cloud::ControlPlaneError;
        let mapped = match error {
            ControlPlaneError::NotFound(message) => ClouisleError::not_found(message),
            ControlPlaneError::Conflict(message) => ClouisleError::invalid_state(message),
            ControlPlaneError::Validation(message) => ClouisleError::validation(message),
            ControlPlaneError::Persistence(message) => ClouisleError::with_source(
                ErrorKind::Store,
                message,
                std::io::Error::other("E2B control-plane persistence failure"),
            ),
        };
        ApiError(mapped)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        into_response(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_maps_404() {
        let e = ClouisleError::not_found("no such sandbox");
        let (status, body) = into_error_response(e);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body.error.code, "NOT_FOUND");
    }

    #[test]
    fn validation_maps_400() {
        let e = ClouisleError::validation("bad spec");
        let (status, _) = into_error_response(e);
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn resource_exhausted_maps_507() {
        let e = ClouisleError::resource_exhausted("no cpu");
        let (status, _) = into_error_response(e);
        assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
    }
}
