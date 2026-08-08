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

/// 沙盒网络配置状态。
#[derive(Debug)]
struct SandboxNet {
    ns_name: String,
    veth_ns: String,
    guest_ip: String,
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

    /// 创建沙盒 netns 拓扑（VMM 启动前调用）。
    /// 返回 `(ns_name, veth_ns, guest_ip)`。
    pub async fn create_network(
        &self,
        sandbox_id: &str,
    ) -> Result<(String, String, String), ClouisleError> {
        let info = netns::create_netns(sandbox_id)?;
        Ok((
            info.ns_name.clone(),
            info.veth_ns.clone(),
            info.guest_ip.clone(),
        ))
    }

    /// 为沙盒配置网络隔离（netns 内 nftables + DNS 代理）。
    pub async fn setup_sandbox_network(
        &self,
        sandbox_id: &str,
        veth_host_ip: &str,
        allow_egress: &[String],
    ) -> Result<(), ClouisleError> {
        let ns = netns::ns_name(sandbox_id);
        let veth_ns = netns::short_name(sandbox_id, "vn");

        // 0. 将 Firecracker 创建的 tap0 加入网桥
        if let Err(e) = netns::attach_tap(sandbox_id) {
            tracing::warn!(sandbox_id = %sandbox_id, error = %e, "attach tap0 to br0 failed");
        }

        // 1. netns 内加载 nftables 规则
        nftables::setup_ruleset(sandbox_id, &ns, &veth_ns, veth_host_ip)?;

        // 2. 启动 DNS 代理（在 netns 内监听 10.0.0.1:53）
        // 使用独立 OS 线程 + setns 进入沙盒 netns，避免污染 tokio 线程池
        let proxy = DnsProxy::new(allow_egress.to_vec());
        let proxy_srv = proxy.clone();
        let ns_path = format!("/var/run/netns/{ns}");
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let cancel_rx = std::sync::Mutex::new(Some(cancel_rx));
        std::thread::Builder::new()
            .name(format!("dns-{}", netns::short_name(sandbox_id, "")))
            .spawn(move || {
                // 进入沙盒 netns
                let file = match std::fs::File::open(&ns_path) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(ns = %ns_path, error = %e, "dns: open netns failed");
                        return;
                    }
                };
                if let Err(e) = nix::sched::setns(&file, nix::sched::CloneFlags::CLONE_NEWNET) {
                    tracing::warn!(ns = %ns_path, error = %e, "dns: setns failed");
                    return;
                }
                // 在 netns 内创建独立 tokio runtime
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(error = %e, "dns: create runtime failed");
                        return;
                    }
                };
                let cancel = cancel_rx.lock().unwrap().take().unwrap();
                rt.block_on(async {
                    tokio::select! {
                        _ = cancel => {
                            tracing::debug!(ns = %ns_path, "dns proxy cancelled");
                        }
                        result = proxy_srv.serve("10.0.0.1") => {
                            if let Err(e) = result {
                                tracing::warn!(ns = %ns_path, error = %e, "dns proxy stopped");
                            }
                        }
                    }
                });
            })
            .map_err(|e| ClouisleError::io(format!("spawn dns thread: {e}")))?;

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
                ns_name: ns,
                veth_ns,
                guest_ip: netns::guest_ip(sandbox_id),
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

        // 2. 删除 netns 内 nftables
        let ns = netns::ns_name(sandbox_id);
        let _ = nftables::teardown_ruleset(sandbox_id, &ns);

        // 3. 删除 netns（自动清理 TAP/veth/路由）
        let _ = netns::delete_netns(sandbox_id);

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
        mgr.teardown_sandbox_network("test-sbx").await.unwrap();
    }
}
