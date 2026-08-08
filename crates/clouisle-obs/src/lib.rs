//! clouisle-obs: 可观测性（FR-10）。

pub mod metrics;
pub mod tracing_setup;

pub use metrics::MetricsRecorder;
pub use tracing_setup::{init_logging, init_otel};