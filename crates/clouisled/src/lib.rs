//! clouisled: 节点代理（Phase 3）。
//!
//! 本机沙盒的**唯一权威**。负责：
//! - 节点注册与心跳上报
//! - VMM 生命周期管理（本机）
//! - 资源核算（本机）
//! - 漂移收敛（reconciler）

pub mod agent;
pub mod node;
pub mod reconciler;
pub mod server;

pub use agent::{NodeAgent, NodeAgentConfig};
pub use node::{HeartbeatReport, NodeRegistration};
pub use reconciler::Reconciler;
