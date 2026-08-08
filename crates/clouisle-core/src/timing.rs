//! 启动时延 SLO 定义与计时（ADR-002）。

use serde::{Deserialize, Serialize};

/// SLO 类别（ADR-002 统一计时定义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SloKind {
    /// 从 warm pool 取已就绪沙盒
    PoolAlloc,
    /// 从内存快照 restore
    WarmStart,
    /// 完整冷启动（镜像已缓存）
    ColdStart,
    /// 首次拉镜像 + 构建 rootfs（非 SLO）
    ImagePull,
}

impl SloKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SloKind::PoolAlloc => "pool_alloc",
            SloKind::WarmStart => "warm_start",
            SloKind::ColdStart => "cold_start",
            SloKind::ImagePull => "image_pull",
        }
    }

    /// P50 / P95 目标（毫秒）。
    pub fn targets_p95_ms(&self) -> u64 {
        match self {
            SloKind::PoolAlloc => 50,
            SloKind::WarmStart => 100,
            SloKind::ColdStart => 200,
            SloKind::ImagePull => 0, // 非 SLO
        }
    }
}

impl std::fmt::Display for SloKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// 启动时延分解追踪。在各阶段打时间戳。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootTrace {
    pub sandbox_id: String,
    /// API 收到请求
    pub t_request: Option<u64>,
    /// scratch 就绪
    pub t_scratch_ready: Option<u64>,
    /// TAP 就绪
    pub t_tap_ready: Option<u64>,
    /// VMM 进程 spawn
    pub t_proc_spawned: Option<u64>,
    /// Firecracker API 配置完成
    pub t_api_configured: Option<u64>,
    /// InstanceStart 发出
    pub t_instance_start: Option<u64>,
    /// agent Hello 收到
    pub t_agent_hello: Option<u64>,
}

impl BootTrace {
    pub fn new(sandbox_id: impl Into<String>) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            t_request: None,
            t_scratch_ready: None,
            t_tap_ready: None,
            t_proc_spawned: None,
            t_api_configured: None,
            t_instance_start: None,
            t_agent_hello: None,
        }
    }

    /// 记录当前单调时钟（毫秒）。
    pub fn mark_request(&mut self) {
        self.t_request = Some(Self::now_ms());
    }
    pub fn mark_scratch(&mut self) {
        self.t_scratch_ready = Some(Self::now_ms());
    }
    pub fn mark_tap(&mut self) {
        self.t_tap_ready = Some(Self::now_ms());
    }
    pub fn mark_spawned(&mut self) {
        self.t_proc_spawned = Some(Self::now_ms());
    }
    pub fn mark_configured(&mut self) {
        self.t_api_configured = Some(Self::now_ms());
    }
    pub fn mark_start(&mut self) {
        self.t_instance_start = Some(Self::now_ms());
    }
    pub fn mark_hello(&mut self) {
        self.t_agent_hello = Some(Self::now_ms());
    }

    fn now_ms() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// 总耗时（t_request → t_agent_hello）。
    pub fn total_ms(&self) -> Option<u64> {
        match (self.t_request, self.t_agent_hello) {
            (Some(a), Some(b)) => Some(b.saturating_sub(a)),
            _ => None,
        }
    }

    /// 各阶段耗时分解（毫秒）。
    pub fn breakdown(&self) -> Vec<(String, Option<u64>)> {
        let mut out = Vec::new();
        let mut prev = None;
        for (name, cur) in [
            ("request", self.t_request),
            ("scratch", self.t_scratch_ready),
            ("tap", self.t_tap_ready),
            ("spawn", self.t_proc_spawned),
            ("configure", self.t_api_configured),
            ("start", self.t_instance_start),
            ("hello", self.t_agent_hello),
        ] {
            if let (Some(p), Some(c)) = (prev, cur) {
                out.push((name.to_string(), Some(c.saturating_sub(p))));
            }
            prev = cur;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slo_str() {
        assert_eq!(SloKind::PoolAlloc.as_str(), "pool_alloc");
        assert_eq!(SloKind::ColdStart.to_string(), "cold_start");
    }

    #[test]
    fn slo_targets() {
        assert_eq!(SloKind::PoolAlloc.targets_p95_ms(), 50);
        assert_eq!(SloKind::WarmStart.targets_p95_ms(), 100);
        assert_eq!(SloKind::ColdStart.targets_p95_ms(), 200);
    }

    #[test]
    fn boot_trace_total() {
        let mut t = BootTrace::new("sbx-1");
        assert_eq!(t.total_ms(), None);
        t.t_request = Some(1000);
        t.t_agent_hello = Some(1300);
        assert_eq!(t.total_ms(), Some(300));
    }

    #[test]
    fn boot_trace_breakdown() {
        let mut t = BootTrace::new("sbx-1");
        t.t_request = Some(1000);
        t.t_scratch_ready = Some(1050);
        t.t_instance_start = Some(1100);
        t.t_agent_hello = Some(1150);
        let b = t.breakdown();
        // 只有连续都有时间值的阶段才出现
        assert!(b.iter().any(|(n, d)| n == "scratch" && *d == Some(50)));
        // start 不出现因为 tap/spawn/configure 没有时间值
        assert!(b.iter().any(|(n, d)| n == "hello" && *d == Some(50)));
        // 没有时间点的阶段不应出现
        assert!(!b.iter().any(|(n, _)| n == "tap"));
    }
}