//! 认证与授权（SR-05 / SR-06）。
//!
//! API key（`Authorization: Bearer <key>`）+ 租户隔离 + scope 校验。
//! Phase 3 基础实现：key 以 argon2 哈希存库，此处提供纯内存版便于测试。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use clouisle_core::{ClouisleError, ErrorKind};

/// API key 作用域。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// 完整权限（创建/删除/exec）
    Full,
    /// 只读（list/get/health）
    Read,
}

impl Scope {
    pub fn from_str(s: &str) -> Self {
        match s {
            "read" => Scope::Read,
            "full" | "admin" => Scope::Full,
            _ => Scope::Read,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Full => "full",
            Scope::Read => "read",
        }
    }
}

/// 认证主体。
#[derive(Debug, Clone)]
pub struct Principal {
    pub tenant_id: String,
    pub scope: Scope,
}

impl Principal {
    /// 开发模式的默认 principal。
    pub fn dev() -> Self {
        Self {
            tenant_id: "dev".into(),
            scope: Scope::Full,
        }
    }
}

/// 认证器：API key → Principal。
#[derive(Debug, Clone, Default)]
pub struct Authenticator {
    keys: Arc<RwLock<HashMap<String, Principal>>>,
}

impl Authenticator {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否为空（无注册 key → dev 模式）。
    pub async fn is_empty(&self) -> bool {
        self.keys.read().await.is_empty()
    }

    /// 注册 API key。
    pub async fn register(&self, key: &str, tenant_id: &str, scope: Scope) {
        let mut keys = self.keys.write().await;
        keys.insert(
            key.to_string(),
            Principal {
                tenant_id: tenant_id.to_string(),
                scope,
            },
        );
    }

    /// 校验 Authorization header。
    pub async fn authenticate(&self, header: Option<&str>) -> Result<Principal, ClouisleError> {
        let header = header.ok_or_else(|| {
            ClouisleError::new(ErrorKind::Unauthenticated, "missing Authorization header")
        })?;
        let key = header
            .strip_prefix("Bearer ")
            .ok_or_else(|| ClouisleError::new(ErrorKind::Unauthenticated, "invalid auth scheme"))?;
        let keys = self.keys.read().await;
        keys.get(key).cloned().ok_or_else(|| {
            ClouisleError::new(ErrorKind::Unauthenticated, "invalid API key")
        })
    }

    /// 检查 scope 是否足够。
    pub fn require_write(&self, principal: &Principal) -> Result<(), ClouisleError> {
        if principal.scope == Scope::Full {
            Ok(())
        } else {
            Err(ClouisleError::new(
                ErrorKind::Forbidden,
                "read-only token cannot perform this operation",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_token_rejected() {
        let auth = Authenticator::new();
        let err = auth.authenticate(None).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unauthenticated);
    }

    #[tokio::test]
    async fn invalid_token_rejected() {
        let auth = Authenticator::new();
        let err = auth
            .authenticate(Some("Bearer garbage"))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unauthenticated);
    }

    #[tokio::test]
    async fn valid_token_accepted() {
        let auth = Authenticator::new();
        auth.register("secret-key", "tenant-a", Scope::Full).await;
        let p = auth.authenticate(Some("Bearer secret-key")).await.unwrap();
        assert_eq!(p.tenant_id, "tenant-a");
        assert_eq!(p.scope, Scope::Full);
    }

    #[tokio::test]
    async fn wrong_scheme_rejected() {
        let auth = Authenticator::new();
        auth.register("k", "t", Scope::Full).await;
        let err = auth.authenticate(Some("Basic k")).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unauthenticated);
    }

    #[tokio::test]
    async fn read_scope_cannot_write() {
        let auth = Authenticator::new();
        auth.register("ro", "t", Scope::Read).await;
        let p = auth.authenticate(Some("Bearer ro")).await.unwrap();
        let err = auth.require_write(&p).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Forbidden);
    }

    #[tokio::test]
    async fn full_scope_can_write() {
        let auth = Authenticator::new();
        auth.register("rw", "t", Scope::Full).await;
        let p = auth.authenticate(Some("Bearer rw")).await.unwrap();
        assert!(auth.require_write(&p).is_ok());
    }

    #[tokio::test]
    async fn tenant_isolation() {
        let auth = Authenticator::new();
        auth.register("a-key", "tenant-a", Scope::Full).await;
        auth.register("b-key", "tenant-b", Scope::Full).await;
        let a = auth.authenticate(Some("Bearer a-key")).await.unwrap();
        let b = auth.authenticate(Some("Bearer b-key")).await.unwrap();
        assert_ne!(a.tenant_id, b.tenant_id);
    }

    #[test]
    fn scope_str() {
        assert_eq!(Scope::Full.as_str(), "full");
        assert_eq!(Scope::Read.as_str(), "read");
        assert_eq!(Scope::from_str("admin"), Scope::Full);
    }
}