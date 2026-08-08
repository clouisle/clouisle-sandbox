//! 节点与租户类型（Phase 3 多节点）。

use serde::{Deserialize, Serialize};

/// 租户 ID。
pub type TenantId = String;

/// 节点状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// 在线（心跳正常）
    Ready,
    /// 心跳超时
    Unreachable,
    /// 已下线
    Down,
    /// 正在排水（升级/关停）
    Draining,
}

/// 目录条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub size: u64,
    pub mode: u32,
    pub mtime: i64,
    pub is_dir: bool,
}

/// 节点信息（`clouisled` 注册时上报）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub node_id: String,
    pub hostname: String,
    /// 总 vCPU
    pub total_vcpu: u16,
    /// 总内存（MiB）
    pub total_memory_mb: u64,
    /// 总磁盘（MiB）
    pub total_disk_mb: u64,
    /// 是否可用 KVM
    pub kvm_available: bool,
    pub kernel_version: String,
    pub firecracker_version: String,
    #[serde(default)]
    pub labels: std::collections::HashMap<String, String>,
}

/// 节点运行时状态（心跳上报）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatusReport {
    pub node_id: String,
    /// 已分配 vCPU
    pub allocated_vcpu: u16,
    /// 已分配内存（MiB）
    pub allocated_memory_mb: u64,
    /// 运行中的沙盒 ID
    pub running_sandboxes: Vec<String>,
    /// warm pool 就绪数（按 bucket）
    pub pool_ready: std::collections::HashMap<String, u32>,
    pub load_avg: [f64; 3],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_status_str() {
        assert_eq!(
            serde_json::to_string(&NodeStatus::Ready).unwrap(),
            "\"ready\""
        );
        assert_eq!(
            serde_json::to_string(&NodeStatus::Unreachable).unwrap(),
            "\"unreachable\""
        );
    }
}
