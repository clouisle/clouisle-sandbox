//! 防火墙编排器：联动 netns + nftables + DNS 白名单代理。
//!
//! 沙盒创建时：
//! 1. 创建 netns（`clo-<id>`）
//! 2. 配置 TAP + veth + IP
//! 3. 加载 nftables 规则集（默认 drop + 白名单 + SNAT）
//! 4. 启动 DNS 代理（监听听 10.0.0.1:53）
//!
//! 沙盒删除时：
//! 1. 停止 DNS 代理
//! 2. 删除 nftables 表
//! 3. 删除 netns

use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use clouisle_core::ClouisleError;

use crate::dns_proxy::DnsProxy;
use crate::netns;
use crate::nftables;

/// 沙盒网络配置状态。
#[derive(Debug)]
struct SandboxNet {
    #[allow(dead_code)]
    veth_host: String,
    #[allow(dead_code)]
    veth_ns: String,
    #[allow(dead_code)]
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

    /// 为沙盒创建完整网络隔离环境。
    ///
    /// `veth_host_ip`：宿主机侧 veth 的 IP（如 `192.168.100.1/30`），
    /// 每个沙盒不同，用于区分流量。
    pub async fn setup_sandbox_network(
        &self,
        sandbox_id: &str,
        veth_host_ip: &str,
        allow_egress: &[String],
    ) -> Result<(), ClouisleError> {
        // 1. 创建 netns + TAP + veth
        let (veth_host, veth_ns) = netns::create_netns(sandbox_id, veth_host_ip)?;

        // 2. 加载 nftables 规则
        nftables::setup_ruleset(sandbox_id)?;

        // 3. 创建 DNS 代理
        let proxy = DnsProxy::new(allow_egress.to_vec());
        let ns_name = netns::short_name(sandbox_id, "");
        let ns_full = format!("clo-{ns_name}");

        // 4. 启动 DNS 代理在 netns 内（监听 10.0.0.1:53）
        // 使用独立 OS 线程 + 独立 tokio runtime，避免 setns 污染主线程池
        let proxy_srv = proxy.clone();
        let ns_path = format!("/var/run/netns/{ns_full}");
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let cancel_rx = std::sync::Mutex::new(Some(cancel_rx));
        std::thread::Builder::new()
            .name(format!("dns-{ns_name}"))
            .spawn(move || {
                // 进入 netns
                let ns_path = ns_path.as_str();
                // 打开 /var/run/netns/<name> 并 setns(CLONE_NEWNET)
                let file = match std::fs::File::open(ns_path) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(ns = ns_path, error = %e, "dns: open netns failed");
                        return;
                    }
                };
                let fd = file.as_raw_fd();
                let ret = unsafe { libc::setns(fd, libc::CLONE_NEWNET) };
                if ret != 0 {
                    tracing::warn!(ns = ns_path, error = %std::io::Error::last_os_error(), "dns: setns failed");
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
                            tracing::debug!(ns = ns_path, "dns proxy cancelled");
                        }
                        result = proxy_srv.serve("10.0.0.1") => {
                            if let Err(e) = result {
                                tracing::warn!(ns = ns_path, error = %e, "dns proxy stopped");
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
                veth_host,
                veth_ns,
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

        // 3. 删除 netns（自动清理所有设备）
        let _ = netns::delete_netns(sandbox_id);

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
        self.dns_proxies.lock().await.get(sandbox_id).map(|h| h.proxy.clone())
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
