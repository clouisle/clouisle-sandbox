//! clouisle-api: Axum HTTP server（Phase 1 控制平面 MVP）。

pub mod agent;
pub mod auth;
pub mod error;
pub mod handlers;
pub mod metrics;
pub mod middleware;
pub mod middleware_auth;
pub mod node_client;
pub mod router;
pub mod state;
pub mod e2b;

pub use router::build_router;
pub use state::{AppState, ImageJobRegistry};
