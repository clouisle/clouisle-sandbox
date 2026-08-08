//! 全局应用状态。

use std::sync::Arc;

use clouisle_core::Resources;
use clouisle_scheduler::ResourcePool;
use clouisle_store::Store;
use clouisle_vmm::Vmm;

use crate::agent::AgentConnector;
use crate::auth::Authenticator;

#[cfg(target_os = "linux")]
use clouisle_net::FirewallManager;

/// 应用状态（所有 handler 共享）。
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub vmm: Arc<dyn Vmm>,
    pub pool: Arc<ResourcePool>,
    pub agent: Arc<dyn AgentConnector>,
    pub auth: Arc<Authenticator>,
    #[cfg(target_os = "linux")]
    pub firewall: Arc<FirewallManager>,
    /// 服务版本
    pub version: &'static str,
}

impl AppState {
    /// 读取宿主机资源上限（macOS 上读取本机近似值；Linux 上从 /proc 读）。
    pub fn host_capacity() -> Resources {
        #[cfg(target_os = "linux")]
        {
            let vcpu = std::fs::read_to_string("/proc/cpuinfo")
                .map(|s| s.lines().filter(|l| l.starts_with("processor")).count() as u16)
                .unwrap_or(4)
                .max(1);
            let mem_kb = std::fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|s| {
                    s.lines().find(|l| l.starts_with("MemTotal")).and_then(|l| {
                        l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok())
                    })
                })
                .unwrap_or(8 * 1024 * 1024);
            Resources {
                vcpu,
                memory_mb: (mem_kb / 1024) as u32,
                disk_mb: 100 * 1024,
                ..Resources::default()
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // macOS 开发环境：模拟 8 vCPU / 16 GiB / 100 GiB
            Resources {
                vcpu: 8,
                memory_mb: 16 * 1024,
                disk_mb: 100 * 1024,
                ..Resources::default()
            }
        }
    }
}