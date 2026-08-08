//! 统一错误类型。所有 crate 的错误都聚合到 `ClouisleError`。

use std::fmt;

/// 错误类别，用于 HTTP 状态映射与日志分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 配置/输入校验失败（HTTP 400）
    Validation,
    /// 资源不足（HTTP 507）
    ResourceExhausted,
    /// 目标不存在（HTTP 404）
    NotFound,
    /// 状态冲突（HTTP 409）
    InvalidState,
    /// 未认证（HTTP 401）
    Unauthenticated,
    /// 无权限（HTTP 403）
    Forbidden,
    /// 配额超限（HTTP 429）
    QuotaExceeded,
    /// 内部错误（HTTP 500）
    Internal,
    /// 底层 IO 错误
    Io,
    /// 超时
    Timeout,
    /// VMM 层错误
    Vmm,
    /// 存储层错误
    Store,
    /// 网络层错误
    Network,
    /// 镜像层错误
    Image,
    /// 审计/安全层错误
    Security,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ErrorKind::Validation => "VALIDATION",
            ErrorKind::ResourceExhausted => "RESOURCE_EXHAUSTED",
            ErrorKind::NotFound => "NOT_FOUND",
            ErrorKind::InvalidState => "INVALID_STATE",
            ErrorKind::Unauthenticated => "UNAUTHENTICATED",
            ErrorKind::Forbidden => "FORBIDDEN",
            ErrorKind::QuotaExceeded => "QUOTA_EXCEEDED",
            ErrorKind::Internal => "INTERNAL",
            ErrorKind::Io => "IO",
            ErrorKind::Timeout => "TIMEOUT",
            ErrorKind::Vmm => "VMM",
            ErrorKind::Store => "STORE",
            ErrorKind::Network => "NETWORK",
            ErrorKind::Image => "IMAGE",
            ErrorKind::Security => "SECURITY",
        };
        write!(f, "{s}")
    }
}

/// 统一错误类型。
#[derive(Debug, thiserror::Error)]
#[error("[{kind}] {message}")]
pub struct ClouisleError {
    pub kind: ErrorKind,
    pub message: String,
    #[source]
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl ClouisleError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    pub fn with_source(
        kind: ErrorKind,
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Validation, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    pub fn resource_exhausted(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ResourceExhausted, message)
    }

    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidState, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Io, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Timeout, message)
    }
}

/// 便捷别名。
pub type Result<T> = std::result::Result<T, ClouisleError>;
