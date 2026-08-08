//! clouisle-core: 领域类型、状态机、错误类型。
//!
//! 本 crate 是纯逻辑层，**不做任何 I/O**，全平台可测（macOS / Linux）。
//! 所有状态转换、配置校验、SLO 枚举、审计哈希链数据结构都定义在这里。

pub mod error;
pub mod execution;
pub mod resources;
pub mod sandbox;
pub mod timing;
pub mod types;

pub use error::{ClouisleError, ErrorKind, Result};
pub use execution::{ExecutionRecord, ExecutionResult, ExecutionSpec, truncate_output};
pub use resources::{Resources, ValidationError};
pub use sandbox::{Sandbox, SandboxEvent, SandboxStatus, SandboxSpec};
pub use sandbox::{ImageRef, NetworkConfig, MountSpec, SecretSpec, RestartPolicy};
pub use sandbox::model::VmmMeta;
pub use types::{DirEntry, NodeInfo, NodeStatus, TenantId};