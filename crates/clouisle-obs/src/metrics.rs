//! Metrics 注册与收集（FR-10）。

use metrics::counter;
use metrics::gauge;
use metrics::histogram;

/// 沙箱创建的服务等级桶。
#[derive(Debug, Clone, Copy)]
pub enum SloBucket {
    PoolAlloc,
    WarmStart,
    ColdStart,
}

impl SloBucket {
    pub fn as_str(&self) -> &'static str {
        match self {
            SloBucket::PoolAlloc => "pool_alloc",
            SloBucket::WarmStart => "warm_start",
            SloBucket::ColdStart => "cold_start",
        }
    }
}

/// 全局 metrics 注册句柄（供 lib.rs 再导出）。
#[derive(Debug, Default, Clone, Copy)]
pub struct MetricsRecorder;

pub fn record_sandbox_create(bucket: SloBucket, duration_ms: f64) {
    let _ = bucket;
    histogram!("clouisle.sandbox.create.duration").record(duration_ms / 1000.0);
    counter!("clouisle.sandbox.create.total").increment(1);
}

pub fn set_sandbox_count(status: &str, count: i64) {
    gauge!("clouisle.sandbox.count", "status" => status.to_string()).set(count as f64);
}

pub fn record_exec_duration(duration_ms: f64) {
    histogram!("clouisle.exec.duration").record(duration_ms / 1000.0);
}

pub fn record_api_request(method: &str, path: &str, status: u16, duration_ms: f64) {
    counter!(
        "clouisle.api.requests",
        "method" => method.to_string(),
        "path" => path.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
    histogram!("clouisle.api.duration", "method" => method.to_string())
        .record(duration_ms / 1000.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slo_bucket_str() {
        assert_eq!(SloBucket::ColdStart.as_str(), "cold_start");
        assert_eq!(SloBucket::PoolAlloc.as_str(), "pool_alloc");
    }
}