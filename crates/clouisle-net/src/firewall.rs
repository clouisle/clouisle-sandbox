//! 防火墙编排器：联动 netns + nftables + DNS 代理。
//!
//! 沙盒创建时：
//! 1. 创建沙盒 netns（`clo-<hash>`）+ veth pair + TAP（[`netns::create_netns`]）
//! 2. 在 netns 内加载 nftables 规则（per-netns，对宿主零影响）
//! 3. 在 netns 内启动 DNS 代理（监听 10.0.0.1:53）
//!
//! 沙盒删除时：
//! 1. 停止 DNS 代理
//! 2. 删除 netns 内 nftables 表
//! 3. 删除 netns（自动清理 TAP/veth/路由）

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use clouisle_core::ClouisleError;

use crate::dns_proxy::DnsProxy;
use crate::netns;
use crate::nftables;

/// 运行中的 DNS 代理句柄。
#[derive(Debug)]
struct DnsHandle {
    proxy: DnsProxy,
    cancel: tokio::sync::oneshot::Sender<()>,
}

/// 防火墙编排器。
#[derive(Debug, Clone)]
pub struct FirewallManager {
    nets: Arc<RwLock<HashMap<String, ()>>>,
    dns_proxies: Arc<Mutex<HashMap<String, DnsHandle>>>,
}

impl FirewallManager {
    pub fn new() -> Self {
        Self {
            nets: Arc::new(RwLock::new(HashMap::new())),
            dns_proxies: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 创建沙盒 netns 拓扑（VMM 启动前调用）。
    /// 返回 `(ns_name, veth_ns, guest_ip)`。
    pub async fn create_network(
        &self,
        sandbox_id: &str,
    ) -> Result<(String, String, String), ClouisleError> {
        self.create_network_in_subnet(sandbox_id, None).await
    }

    /// 用显式子网创建 netns 拓扑（快照预热继承路径）。
    pub async fn create_network_in_subnet(
        &self,
        sandbox_id: &str,
        subnet: Option<(u16, u16)>,
    ) -> Result<(String, String, String), ClouisleError> {
        let info = netns::create_netns_in_subnet(sandbox_id, subnet)?;
        Ok((
            info.ns_name.clone(),
            info.veth_ns.clone(),
            info.guest_ip.clone(),
        ))
    }

    /// 为沙盒配置网络隔离（netns 内 nftables + DNS 代理 + 带宽限速）。
    pub async fn setup_sandbox_network(
        &self,
        sandbox_id: &str,
        veth_host_ip: &str,
        allow_egress: &[String],
        deny_egress: &[String],
        bandwidth_mbps: Option<u32>,
    ) -> Result<(), ClouisleError> {
        let ns = netns::ns_name(sandbox_id);
        let veth_ns = netns::short_name(sandbox_id, "vn");

        netns::attach_tap(sandbox_id)?;
        nftables::setup_ruleset(sandbox_id, &ns, &veth_ns, veth_host_ip)?;
        nftables::setup_host_egress_with_policy(sandbox_id, allow_egress, deny_egress)?;
        netns::apply_bandwidth_limit(sandbox_id, bandwidth_mbps)?;
        let proxy = DnsProxy::with_sandbox(allow_egress.to_vec(), Some(sandbox_id.to_string()));
        let proxy_srv = proxy.clone();
        let dns_addr = veth_host_ip
            .split_once('/')
            .map_or_else(|| veth_host_ip.to_string(), |(addr, _)| addr.to_string());
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel_rx => tracing::debug!(bind = %dns_addr, "dns proxy cancelled"),
                result = proxy_srv.serve(&dns_addr) => {
                    if let Err(e) = result {
                        tracing::warn!(bind = %dns_addr, error = %e, "dns proxy stopped");
                    }
                }
            }
        });

        self.dns_proxies.lock().await.insert(
            sandbox_id.to_string(),
            DnsHandle {
                proxy,
                cancel: cancel_tx,
            },
        );
        self.nets.write().await.insert(sandbox_id.to_string(), ());
        Ok(())
    }

    /// Replace the DNS and host firewall policy without restarting the sandbox network.
    pub async fn update_sandbox_network(
        &self,
        sandbox_id: &str,
        enabled: bool,
        allow_egress: &[String],
        deny_egress: &[String],
    ) -> Result<(), ClouisleError> {
        let proxy = {
            let proxies = self.dns_proxies.lock().await;
            proxies
                .get(sandbox_id)
                .map(|handle| handle.proxy.clone())
                .ok_or_else(|| {
                    ClouisleError::invalid_state(format!(
                        "sandbox network {sandbox_id} is not active"
                    ))
                })?
        };
        proxy
            .replace_allowed(if enabled {
                allow_egress.to_vec()
            } else {
                Vec::new()
            })
            .await;
        nftables::teardown_host_egress(sandbox_id)?;
        let deny = if enabled {
            deny_egress.to_vec()
        } else {
            vec!["0.0.0.0/0".to_string()]
        };
        nftables::setup_host_egress_with_policy(
            sandbox_id,
            if enabled { allow_egress } else { &[] },
            &deny,
        )
    }

    /// 删除沙盒的网络隔离环境。`subnet` 为快照继承子网（继承路径用显式子网，
    /// 否则按 sandbox_id 派生）；None 表示按 sandbox_id 派生。
    pub async fn teardown_sandbox_network(
        &self,
        sandbox_id: &str,
        subnet: Option<(u16, u16)>,
    ) -> Result<(), ClouisleError> {
        // 1. 停止 DNS 代理
        if let Some(handle) = self.dns_proxies.lock().await.remove(sandbox_id) {
            let _ = handle.cancel.send(());
        }

        // 2. 删除 guest netns 与宿主 veth 出站策略。
        let ns = netns::ns_name(sandbox_id);
        let _ = nftables::teardown_ruleset(sandbox_id, &ns);
        let _ = nftables::teardown_host_egress(sandbox_id);
        let _ = netns::delete_netns(sandbox_id, subnet);

        // 4. 清理状态
        self.nets.write().await.remove(sandbox_id);

        Ok(())
    }

    /// 放行一个 IP（由 DNS 解析回调触发，netns 内）。
    pub async fn allow_ip(
        &self,
        sandbox_id: &str,
        ip: &str,
        ttl_secs: u64,
    ) -> Result<(), ClouisleError> {
        let ns = netns::ns_name(sandbox_id);
        nftables::allow_ip(sandbox_id, &ns, ip, ttl_secs)
    }

    /// 获取沙盒的 DNS 代理（供外部调用）。
    pub async fn dns_proxy(&self, sandbox_id: &str) -> Option<DnsProxy> {
        self.dns_proxies
            .lock()
            .await
            .get(sandbox_id)
            .map(|h| h.proxy.clone())
    }
}

impl Default for FirewallManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn setup_teardown_noop() {
        // 在非 Linux 上只验证不 panic
        let mgr = FirewallManager::new();
        mgr.teardown_sandbox_network("test-sbx", None)
            .await
            .unwrap();
    }
}
