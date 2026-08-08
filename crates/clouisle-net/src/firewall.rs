//! 防火墙编排器：联动 TAP 设备 + nftables + DNS 代理。
//!
//! 沙盒创建时：
//! 1. 创建 host 侧 TAP 设备（`fc-<hash>`，Firecracker 直连）
//! 2. 加载 nftables 规则集（默认 drop 入站 + 出站白名单 + SNAT）
//! 3. 启动 DNS 代理（监听宿主 10.0.0.1:53）
//!
//! 沙盒删除时：
//! 1. 停止 DNS 代理
//! 2. 删除 nftables 表
//! 3. 删除 TAP 设备

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use clouisle_core::ClouisleError;

use crate::dns_proxy::DnsProxy;
use crate::netns;
use crate::nftables;

/// 沙盒网络配置状态。
#[derive(Debug)]
struct SandboxNet {
    tap: String,
    host_ip: String,
}

/// 运行中的 DNS 代理句柄。
#[derive(Debug)]
struct DnsHandle {
    proxy: DnsProxy,
    cancel: tokio::sync::oneshot::Sender<()>,
}

/// 防火墙编排器。
#[derive(Debug, Clone)]
pub struct FirewallManager {
    nets: Arc<RwLock<HashMap<String, SandboxNet>>>,
    dns_proxies: Arc<Mutex<HashMap<String, DnsHandle>>>,
}

impl FirewallManager {
    pub fn new() -> Self {
        Self {
            nets: Arc::new(RwLock::new(HashMap::new())),
            dns_proxies: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 创建 TAP 设备（VMM 启动前调用）。
    pub async fn create_network(&self, sandbox_id: &str) -> Result<String, ClouisleError> {
        netns::create_tap(sandbox_id)
    }

    /// 为沙盒创建完整网络隔离环境。
    ///
    /// `veth_host_ip`：宿主机侧 TAP 的 IP（如 `192.168.100.1/30`），
    /// 用于 SNAT 区分流量。
    pub async fn setup_sandbox_network(
        &self,
        sandbox_id: &str,
        _veth_host_ip: &str,
        allow_egress: &[String],
    ) -> Result<(), ClouisleError> {
        // 1. 获取已创建的 TAP 设备名
        let tap = netns::short_name(sandbox_id, "fc");

        // 2. 加载 nftables 规则（TAP 设备已存在）
        nftables::setup_ruleset(sandbox_id, &tap, _veth_host_ip)?;

        // 3. 创建并启动 DNS 代理（监听宿主 10.0.0.1:53）
        let proxy = DnsProxy::new(allow_egress.to_vec());
        let proxy_srv = proxy.clone();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let mut cancel_rx = cancel_rx;
        tokio::spawn(async move {
            tokio::select! {
                _ = &mut cancel_rx => {
                    tracing::debug!("dns proxy cancelled");
                }
                result = proxy_srv.serve("10.0.0.1") => {
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "dns proxy stopped");
                    }
                }
            }
        });

        // 记录 DNS 代理句柄
        self.dns_proxies.lock().await.insert(
            sandbox_id.to_string(),
            DnsHandle {
                proxy,
                cancel: cancel_tx,
            },
        );

        // 记录状态
        self.nets.write().await.insert(
            sandbox_id.to_string(),
            SandboxNet {
                tap,
                host_ip: veth_host_ip.to_string(),
            },
        );

        Ok(())
    }

    /// 删除沙盒的网络隔离环境。
    pub async fn teardown_sandbox_network(&self, sandbox_id: &str) -> Result<(), ClouisleError> {
        // 1. 停止 DNS 代理
        if let Some(handle) = self.dns_proxies.lock().await.remove(sandbox_id) {
            let _ = handle.cancel.send(());
        }

        // 2. 删除 nftables
        let _ = nftables::teardown_ruleset(sandbox_id);

        // 3. 删除 TAP 设备
        let _ = netns::delete_tap(sandbox_id);

        // 4. 清理状态
        self.nets.write().await.remove(sandbox_id);

        Ok(())
    }

    /// 放行一个 IP（由 DNS 解析回调触发）。
    pub async fn allow_ip(
        &self,
        sandbox_id: &str,
        ip: &str,
        ttl_secs: u64,
    ) -> Result<(), ClouisleError> {
        nftables::allow_ip(sandbox_id, ip, ttl_secs)
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
        mgr.teardown_sandbox_network("test-sbx").await.unwrap();
    }
}