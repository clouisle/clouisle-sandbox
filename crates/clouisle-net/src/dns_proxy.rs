//! DNS 白名单代理（FR-05 / ADR-006 Phase 2）。

use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct DnsRule {
    pub domain: String,
}

#[derive(Debug, Clone)]
pub struct DnsProxy {
    allowed: Arc<RwLock<HashSet<String>>>,
}

impl DnsProxy {
    pub fn new(domains: Vec<String>) -> Self {
        Self {
            allowed: Arc::new(RwLock::new(domains.into_iter().collect())),
        }
    }

    pub async fn is_allowed(&self, domain: &str) -> bool {
        let allowed = self.allowed.read().await;
        allowed.iter().any(|d| domain.ends_with(d.as_str()))
            || allowed.contains(domain)
    }

    pub async fn update(&self, domains: Vec<String>) {
        let mut allowed = self.allowed.write().await;
        allowed.clear();
        allowed.extend(domains);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exact_match() {
        let proxy = DnsProxy::new(vec!["pypi.org".into()]);
        assert!(proxy.is_allowed("pypi.org").await);
        assert!(!proxy.is_allowed("google.com").await);
    }

    #[tokio::test]
    async fn subdomain_match() {
        let proxy = DnsProxy::new(vec!["python.org".into()]);
        assert!(proxy.is_allowed("files.python.org").await);
    }

    #[tokio::test]
    async fn empty_list_denies_all() {
        let proxy = DnsProxy::new(vec![]);
        assert!(!proxy.is_allowed("anything.com").await);
    }
}
