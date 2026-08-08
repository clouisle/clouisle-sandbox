//! OCI 镜像拉取与 ext4 构建（FR-06）。

use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct ImageSpec {
    pub reference: String,
    pub digest: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ImageManager {
    cache: Arc<RwLock<std::collections::HashMap<String, String>>>,
}

impl ImageManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn pull_and_build(&self, spec: &ImageSpec) -> Result<String, String> {
        let key = spec.digest.as_deref().unwrap_or(&spec.reference).to_string();
        let cache = self.cache.read().await;
        if let Some(path) = cache.get(&key) {
            return Ok(path.clone());
        }
        drop(cache);

        // 模拟构建（Phase 2 实现真实拉取）
        let path = format!("/tmp/clouisle-cache/{}.ext4", key.replace('/', "_"));
        let mut cache = self.cache.write().await;
        cache.insert(key, path.clone());
        Ok(path)
    }

    pub async fn cache_hit(&self, spec: &ImageSpec) -> bool {
        let key = spec.digest.as_deref().unwrap_or(&spec.reference).to_string();
        self.cache.read().await.contains_key(&key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn image_pull_and_cache() {
        let mgr = ImageManager::new();
        let spec = ImageSpec {
            reference: "alpine:latest".into(),
            digest: None,
        };
        let path = mgr.pull_and_build(&spec).await.unwrap();
        assert!(path.contains("alpine:latest"));
        assert!(mgr.cache_hit(&spec).await);

        // 第二次应命中缓存
        let path2 = mgr.pull_and_build(&spec).await.unwrap();
        assert_eq!(path, path2);
    }

    #[tokio::test]
    async fn digest_cache_key() {
        let mgr = ImageManager::new();
        let spec = ImageSpec {
            reference: "python:3.11".into(),
            digest: Some("sha256:abc".into()),
        };
        mgr.pull_and_build(&spec).await.unwrap();
        // 不同 reference 但同 digest 的 spec 应命中
        let spec2 = ImageSpec {
            reference: "python:3.11-slim".into(),
            digest: Some("sha256:abc".into()),
        };
        assert!(mgr.cache_hit(&spec2).await);
    }
}