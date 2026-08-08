//! Prometheus 指标注册与导出（FR-10 基础）。
//!
//! 提供 `record_*` 函数包裹 `metrics` crate。

use std::sync::OnceLock;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

fn init_recorder() {
    let _ = HANDLE.get_or_init(|| {
        PrometheusBuilder::new()
            .set_buckets_for_metric(
                Matcher::Suffix("sandbox_create_duration_seconds".to_string()),
                &[0.05, 0.1, 0.2, 0.5, 1.0, 2.0],
            )
            .expect("buckets")
            .install_recorder()
            .expect("install recorder")
    });
}

/// 初始化（幂等）。
pub fn init() {
    init_recorder();
}

/// 渲染 Prometheus 文本。
pub fn render() -> String {
    init_recorder();
    HANDLE.get().expect("handle").render()
}

/// 记录 API 请求。
pub fn record_api_request(method: &str, path: &str, status: u16, duration_ms: f64) {
    init_recorder();
    metrics::counter!("clouisle_api_requests_total", "method" => method.to_string(), "path" => path.to_string(), "status" => status.to_string())
        .increment(1);
    metrics::histogram!("clouisle_api_request_duration_seconds", "method" => method.to_string())
        .record(duration_ms / 1000.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn render_not_empty() {
        record_api_request("GET", "/test", 200, 12.3);
        let s = render();
        assert!(!s.is_empty());
    }
}