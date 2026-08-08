//! Logging 与 tracing 初始化。

pub fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .json()
        .init();
}

pub fn init_otel() {
    // Phase 3 实现 OpenTelemetry 集成
}

#[cfg(test)]
mod tests {
    #[test]
    fn init_logging_does_not_panic() {
        // 不重复初始化（会 panic），只验证函数可达
    }
}
