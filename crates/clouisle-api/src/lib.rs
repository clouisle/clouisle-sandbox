//! clouisle-api: Axum HTTP server（Phase 1 控制平面 MVP）。

pub mod agent;
pub mod auth;
pub mod error;
pub mod handlers;
pub mod middleware;
pub mod middleware_auth;
pub mod metrics;
pub mod router;
pub mod state;

pub use state::AppState;
pub use router::build_router;