//! 多节点调度：Filter + Score 两阶段（FR-11 / ER-01）。

use std::collections::HashMap;

use clouisle_core::{NodeInfo, SandboxSpec};

/// 放置策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementStrategy {
    /// 最少负载（默认），加权 CPU:mem = 1:1
    LeastLoaded,
    /// 装箱，提高密度
    BestFit,
    /// 跨节点分散
    Spread,
}

/// 节点打分上下文：当前分配量（来自心跳上报）。
#[derive(Debug, Clone)]
pub struct NodeAllocation {
    pub node_id: String,
    pub allocated_vcpu: u16,
    pub allocated_memory_mb: u64,
    /// 当前运行沙盒数
    pub sandbox_count: usize,
}

/// Filter 阶段：排除不可用节点。
pub fn filter_nodes<'a>(
    nodes: &'a [NodeInfo],
    allocations: &[NodeAllocation],
    spec: &SandboxSpec,
) -> Vec<&'a NodeInfo> {
    nodes
        .iter()
        .filter(|n| {
            // 必须 KVM 可用
            if !n.kvm_available {
                return false;
            }
            // 必须满足 node_selector
            if !matches_selector(n, &spec.node_selector) {
                return false;
            }
            // 资源必须足够
            if let Some(a) = allocations.iter().find(|a| a.node_id == n.node_id) {
                let avail_vcpu = n.total_vcpu.saturating_sub(a.allocated_vcpu);
                let avail_mem = n.total_memory_mb.saturating_sub(a.allocated_memory_mb);
                if spec.resources.vcpu > avail_vcpu {
                    return false;
                }
                if spec.resources.memory_mb as u64 > avail_mem {
                    return false;
                }
            }
            true
        })
        .collect()
}

/// 检查 node_selector 是否全部匹配节点 labels。
pub fn matches_selector(
    node: &NodeInfo,
    selector: &HashMap<String, String>,
) -> bool {
    selector
        .iter()
        .all(|(k, v)| node.labels.get(k) == Some(v))
}

/// Score 阶段：按策略打分（分数越低越优先）。
pub fn score_nodes<'a>(
    nodes: &[&'a NodeInfo],
    allocations: &[NodeAllocation],
    strategy: PlacementStrategy,
) -> Vec<(&'a NodeInfo, f64)> {
    nodes
        .iter()
        .map(|n| {
            let alloc = allocations.iter().find(|a| a.node_id == n.node_id);
            let load = load_fraction(n, alloc);
            let score = match strategy {
                PlacementStrategy::LeastLoaded => load,
                PlacementStrategy::BestFit => {
                    // 满分先选最接近装满的
                    1.0 - load + load * 0.5
                }
                PlacementStrategy::Spread => {
                    // 加分项：沙盒数少的优先，削弱负载影响
                    let count_penalty = alloc.map(|a| a.sandbox_count as f64 * 0.1).unwrap_or(0.0);
                    load + count_penalty
                }
            };
            (*n, score)
        })
        .collect()
}

fn load_fraction(node: &NodeInfo, alloc: Option<&NodeAllocation>) -> f64 {
    let alloc = match alloc {
        Some(a) => a,
        None => return 0.0,
    };
    if node.total_vcpu == 0 || node.total_memory_mb == 0 {
        return 1.0;
    }
    let cpu_load = alloc.allocated_vcpu as f64 / node.total_vcpu as f64;
    let mem_load = alloc.allocated_memory_mb as f64 / node.total_memory_mb as f64;
    (cpu_load + mem_load) / 2.0
}

/// 完整放置：filter → sort by score → 返回最佳节点。
pub fn place<'a>(
    nodes: &'a [NodeInfo],
    allocations: &[NodeAllocation],
    spec: &SandboxSpec,
    strategy: PlacementStrategy,
) -> Option<&'a NodeInfo> {
    let filtered = filter_nodes(nodes, allocations, spec);
    let mut scored = score_nodes(&filtered, allocations, strategy);
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.first().map(|(n, _)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn node(id: &str, kvm: bool, labels: &[(&str, &str)]) -> NodeInfo {
        NodeInfo {
            node_id: id.into(),
            hostname: format!("{id}.example.com"),
            total_vcpu: 16,
            total_memory_mb: 32768,
            total_disk_mb: 102400,
            kvm_available: kvm,
            kernel_version: "6.1".into(),
            firecracker_version: "1.4".into(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn alloca(id: &str, cpu: u16, mem: u64, count: usize) -> NodeAllocation {
        NodeAllocation {
            node_id: id.into(),
            allocated_vcpu: cpu,
            allocated_memory_mb: mem,
            sandbox_count: count,
        }
    }

    #[test]
    fn filter_kvm_only() {
        let nodes = vec![node("a", true, &[]), node("b", false, &[])];
        let spec = SandboxSpec::default();
        assert_eq!(filter_nodes(&nodes, &[], &spec).len(), 1);
    }

    #[test]
    fn filter_by_selector() {
        let mut labels = std::collections::HashMap::<String, String>::new();
        labels.insert("tier".to_string(), "gpu".to_string());
        let nodes = vec![node("a", true, &labels.iter().map(|(k,v)|(k.as_str(),v.as_str())).collect::<Vec<_>>())];
        let mut spec = SandboxSpec::default();
        spec.node_selector.insert("tier".into(), "gpu".into());
        assert_eq!(filter_nodes(&nodes, &[], &spec).len(), 1);

        spec.node_selector.insert("tier".into(), "cpu".into());
        assert_eq!(filter_nodes(&nodes, &[], &spec).len(), 0);
    }

    #[test]
    fn filter_by_resources() {
        let nodes = vec![node("a", true, &[])];
        let mut spec = SandboxSpec::default();
        spec.resources.vcpu = 8;
        // 节点 16 vcpu，分配 12 → 剩 4，不够 8 → 排除
        let alloc = vec![alloca("a", 12, 0, 0)];
        assert_eq!(filter_nodes(&nodes, &alloc, &spec).len(), 0);
        // 分配 4 → 剩 12，够 8 → 保留
        let alloc = vec![alloca("a", 4, 0, 0)];
        assert_eq!(filter_nodes(&nodes, &alloc, &spec).len(), 1);
    }

    #[test]
    fn least_loaded_picks_lowest() {
        let nodes = vec![node("a", true, &[]), node("b", true, &[])];
        let alloc = vec![alloca("a", 12, 20000, 3), alloca("b", 4, 8192, 1)];
        let spec = SandboxSpec::default();
        let best = place(&nodes, &alloc, &spec, PlacementStrategy::LeastLoaded).unwrap();
        assert_eq!(best.node_id, "b");
    }

    #[test]
    fn matches_selector_works() {
        let mut labels = std::collections::HashMap::<String, String>::new();
        labels.insert("k".into(), "v".into());
        let n = node("a", true, &labels.iter().map(|(k,v)|(k.as_str(),v.as_str())).collect::<Vec<_>>());
        let mut sel = HashMap::new();
        sel.insert("k".into(), "v".into());
        assert!(matches_selector(&n, &sel));
        sel.insert("other".into(), "x".into());
        assert!(!matches_selector(&n, &sel));
    }

    #[test]
    fn best_fit_differs_from_least_loaded() {
        let nodes = vec![node("a", true, &[]), node("b", true, &[])];
        let alloc = vec![alloca("a", 8, 16384, 2), alloca("b", 4, 8192, 5)];
        let spec = SandboxSpec::default();
        let ll = place(&nodes, &alloc, &spec, PlacementStrategy::LeastLoaded).unwrap();
        let bf = place(&nodes, &alloc, &spec, PlacementStrategy::BestFit).unwrap();
        assert_eq!(ll.node_id, "b");
        assert_eq!(bf.node_id, "a");
    }
}