//! 沙盒领域模型与状态机（FR-01）。

use serde::{Deserialize, Serialize};

pub mod model;
pub mod spec;
pub mod state;

pub use model::Sandbox;
pub use spec::SandboxSpec;
pub use state::{SandboxEvent, SandboxStatus};

/// 沙盒 ID 别名（UUID v7 字符串）。
pub type SandboxId = String;

/// 镜像引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    /// 完整镜像引用，如 `docker.io/library/python:3.11-slim`
    pub reference: String,
    /// 已解析的 digest（sha256:...），可选
    pub digest: Option<String>,
}

impl ImageRef {
    pub fn new(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            digest: None,
        }
    }

    /// 缓存键：优先 digest，否则 reference 自身哈希。
    pub fn cache_key(&self) -> String {
        self.digest
            .clone()
            .unwrap_or_else(|| self.reference.clone())
    }
}

/// 网络配置（FR-05 / FR-08）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// 是否启用网络。false = 完全离线沙盒。
    pub enabled: bool,
    /// 出站域名白名单。空 = 拒绝全部出站。
    pub allow_egress: Vec<String>,
    /// 出站 IP/CIDR deny rules; allow rules take precedence at the firewall.
    #[serde(default)]
    pub deny_egress: Vec<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_egress: Vec::new(),
            deny_egress: Vec::new(),
        }
    }
}

/// 卷挂载（FR-12）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSpec {
    /// 宿主机源路径
    pub source: String,
    /// guest 内目标路径
    pub target: String,
    /// 是否只读
    pub readonly: bool,
}

/// E2B control-plane volume mounted at a guest path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMountSpec {
    /// Volume name in the owning tenant.
    pub name: String,
    /// Absolute guest destination path.
    pub target: String,
}

/// 密钥注入（SR-06）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretSpec {
    /// 密钥名（guest 内可见为 `/run/secrets/<name>`）
    pub name: String,
    pub value: String,
}

/// 重启策略（AR-02）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    #[default]
    Never,
    OnFailure,
    Always,
}

impl RestartPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            RestartPolicy::Never => "never",
            RestartPolicy::OnFailure => "on_failure",
            RestartPolicy::Always => "always",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_ref_default_cache_key() {
        let r = ImageRef::new("docker.io/library/python");
        assert_eq!(r.cache_key(), "docker.io/library/python");
    }

    #[test]
    fn image_ref_digest_cache_key() {
        let r = ImageRef {
            reference: "python:3.11".into(),
            digest: Some("sha256:abc123".into()),
        };
        assert_eq!(r.cache_key(), "sha256:abc123");
    }

    #[test]
    fn network_default_no_egress() {
        let n = NetworkConfig::default();
        assert!(n.enabled);
        assert!(n.allow_egress.is_empty());
    }

    #[test]
    fn restart_policy_str_roundtrip() {
        assert_eq!(RestartPolicy::Never.as_str(), "never");
        assert_eq!(RestartPolicy::OnFailure.as_str(), "on_failure");
        assert_eq!(RestartPolicy::Always.as_str(), "always");
    }

    #[test]
    fn restart_policy_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&RestartPolicy::OnFailure).unwrap(),
            "\"on_failure\""
        );
        let parsed: RestartPolicy = serde_json::from_str("\"on_failure\"").unwrap();
        assert_eq!(parsed, RestartPolicy::OnFailure);
    }
}
