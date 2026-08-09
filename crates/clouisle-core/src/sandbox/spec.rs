//! 沙盒创建规格（SandboxSpec）与校验。

use serde::{Deserialize, Serialize};

use crate::resources::{Resources, ValidationError};
use crate::sandbox::{ImageRef, MountSpec, NetworkConfig, RestartPolicy, SecretSpec};

/// 创建沙盒的规格。对应 `POST /api/v1/sandboxes` 的请求体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub image: ImageRef,
    #[serde(default)]
    pub resources: Resources,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    #[serde(default)]
    pub secrets: Vec<SecretSpec>,
    /// 沙盒租期（秒），到期强制销毁。None = 永不过期。
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    /// 启动超时（秒），默认 10s。
    #[serde(default = "default_start_timeout")]
    pub start_timeout_secs: u64,
    /// 环境变量注入。
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// One-time argv executed after the guest agent is ready and before the
    /// sandbox is exposed as running.
    #[serde(default)]
    pub init_command: Vec<String>,
    /// Maximum time allowed for the initialization command.
    #[serde(default = "default_init_timeout_ms")]
    pub init_timeout_ms: u64,
    /// 节点选择标签（Phase 3 多节点调度）。
    #[serde(default)]
    pub node_selector: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub restart_policy: RestartPolicy,
    /// 租户 ID（Phase 3 多租户）。
    #[serde(default)]
    pub tenant_id: Option<String>,
}

fn default_start_timeout() -> u64 {
    10
}
fn default_init_timeout_ms() -> u64 {
    30_000
}

impl Default for SandboxSpec {
    fn default() -> Self {
        SandboxSpec {
            image: ImageRef::new("docker.io/library/alpine:latest"),
            resources: Resources::default(),
            network: NetworkConfig::default(),
            mounts: Vec::new(),
            secrets: Vec::new(),
            ttl_secs: None,
            start_timeout_secs: 10,
            env: std::collections::HashMap::new(),
            init_command: Vec::new(),
            init_timeout_ms: default_init_timeout_ms(),
            node_selector: std::collections::HashMap::new(),
            restart_policy: RestartPolicy::default(),
            tenant_id: None,
        }
    }
}

impl SandboxSpec {
    /// 校验规格。返回字段级错误列表（API-003 / API-004）。
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();

        if self.image.reference.trim().is_empty() {
            errors.push(ValidationError::new("image", "image is required"));
        }

        if let Err(mut resource_errors) = self.resources.validate() {
            errors.append(&mut resource_errors);
        }

        if self.start_timeout_secs == 0 || self.start_timeout_secs > 300 {
            errors.push(ValidationError::new(
                "start_timeout_secs",
                "start_timeout_secs must be in [1, 300]",
            ));
        }

        if self.init_command.iter().any(|part| part.trim().is_empty()) {
            errors.push(ValidationError::new(
                "init_command",
                "init_command entries must not be empty",
            ));
        }
        if self.init_command.is_empty() && self.init_timeout_ms != default_init_timeout_ms() {
            errors.push(ValidationError::new(
                "init_timeout_ms",
                "init_timeout_ms is only valid with init_command",
            ));
        }
        if self.init_timeout_ms == 0 || self.init_timeout_ms > 300_000 {
            errors.push(ValidationError::new(
                "init_timeout_ms",
                "init_timeout_ms must be in [1, 300000]",
            ));
        }

        let mut secret_names = std::collections::HashSet::new();
        for secret in &self.secrets {
            if secret.name.is_empty()
                || secret.name == "."
                || secret.name == ".."
                || secret.name.contains(['/', '\\', '\0'])
            {
                errors.push(ValidationError::new(
                    "secrets",
                    "secret names must be non-empty file names without path separators",
                ));
            } else if !secret_names.insert(&secret.name) {
                errors.push(ValidationError::new(
                    "secrets",
                    "secret names must be unique",
                ));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// 计算资源指纹，用于 warm pool 分桶（同资源 -> 同池）。
    pub fn resources_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        // 用 serde 序列化资源的确定性部分，避免字段顺序差异
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.resources.vcpu.hash(&mut hasher);
        self.resources.memory_mb.hash(&mut hasher);
        self.resources.disk_mb.hash(&mut hasher);
        self.resources.pids_max.hash(&mut hasher);
        hasher.finish()
    }

    /// Warm pool 分桶键：image digest + resources hash。
    pub fn pool_key(&self) -> String {
        format!("{}:{}", self.image.cache_key(), self.resources_hash())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_spec() -> SandboxSpec {
        SandboxSpec::default()
    }

    #[test]
    fn valid_spec_passes() {
        assert!(valid_spec().validate().is_ok());
    }

    #[test]
    fn missing_image_rejected() {
        let mut s = valid_spec();
        s.image = ImageRef::new("");
        let errs = s.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "image"));
    }

    #[test]
    fn invalid_resources_included() {
        let mut s = valid_spec();
        s.resources.vcpu = 0;
        let errs = s.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.field == "vcpu"));
    }

    #[test]
    fn pool_key_distinct_by_resources() {
        let a = valid_spec();
        let mut b = valid_spec();
        b.resources.vcpu = 2;
        assert_ne!(a.pool_key(), b.pool_key());
    }

    #[test]
    fn pool_key_same_for_equal() {
        let a = valid_spec();
        let b = valid_spec();
        assert_eq!(a.pool_key(), b.pool_key());
    }

    #[test]
    fn rejects_path_like_or_duplicate_secret_names() {
        let mut spec = valid_spec();
        spec.secrets = vec![
            SecretSpec {
                name: "../escape".into(),
                value: "x".into(),
            },
            SecretSpec {
                name: "same".into(),
                value: "x".into(),
            },
            SecretSpec {
                name: "same".into(),
                value: "y".into(),
            },
        ];
        let errors = spec.validate().unwrap_err();
        assert_eq!(
            errors
                .iter()
                .filter(|error| error.field == "secrets")
                .count(),
            2
        );
    }
}
