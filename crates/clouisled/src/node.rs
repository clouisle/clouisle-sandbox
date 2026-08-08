//! 节点注册与心跳模型（Phase 3 数据格式）。

use serde::{Deserialize, Serialize};

use clouisle_core::NodeInfo;

/// `clouisled` 启动时向 apiserver 注册的载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRegistration {
    pub node: NodeInfo,
}

/// 心跳上报载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatReport {
    pub node_id: String,
    pub allocated_vcpu: u16,
    pub allocated_memory_mb: u64,
    pub running_sandboxes: Vec<String>,
    pub pool_ready: std::collections::HashMap<String, u32>,
    pub load_avg: [f64; 3],
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn registration_serializes() {
        let reg = NodeRegistration {
            node: NodeInfo {
                node_id: "node-1".into(),
                hostname: "host1".into(),
                total_vcpu: 16,
                total_memory_mb: 65536,
                total_disk_mb: 102400,
                kvm_available: true,
                kernel_version: "6.1".into(),
                firecracker_version: "1.4".into(),
                labels: HashMap::new(),
            },
        };
        let json = serde_json::to_string(&reg).unwrap();
        assert!(json.contains("node-1"));
    }

    #[test]
    fn heartbeat_roundtrip() {
        let hb = HeartbeatReport {
            node_id: "node-1".into(),
            allocated_vcpu: 8,
            allocated_memory_mb: 32768,
            running_sandboxes: vec!["sbx-1".into()],
            pool_ready: HashMap::new(),
            load_avg: [0.5, 0.4, 0.3],
        };
        let json = serde_json::to_string(&hb).unwrap();
        let back: HeartbeatReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.node_id, "node-1");
    }
}