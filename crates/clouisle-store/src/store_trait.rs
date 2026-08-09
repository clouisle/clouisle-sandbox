//! Store trait — 沙盒与执行记录的 CRUD。

use async_trait::async_trait;

use clouisle_core::{ClouisleError, ErrorKind, ExecutionRecord, Sandbox, SandboxStatus};

pub type StoreResult<T> = std::result::Result<T, StoreError>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("internal: {0}")]
    Internal(String),
}

impl From<StoreError> for ClouisleError {
    fn from(e: StoreError) -> Self {
        match &e {
            StoreError::NotFound(msg) => ClouisleError::not_found(msg),
            StoreError::Conflict(msg) => ClouisleError::invalid_state(msg),
            _ => ClouisleError::with_source(ErrorKind::Store, e.to_string(), e),
        }
    }
}

#[async_trait]
pub trait Store: Send + Sync {
    // ---- 沙盒 ----
    async fn create_sandbox(&self, sandbox: &Sandbox) -> StoreResult<()>;
    async fn get_sandbox(&self, id: &str) -> StoreResult<Sandbox>;
    async fn update_sandbox_status(&self, id: &str, status: &SandboxStatus) -> StoreResult<()>;

    /// Update lifecycle state and persist a human-readable failure/recovery reason.
    async fn update_sandbox_status_message(
        &self,
        id: &str,
        status: &SandboxStatus,
        message: Option<&str>,
    ) -> StoreResult<()>;
    /// Persist the runtime owner so restart reconciliation can find the node.
    async fn update_sandbox_node(&self, id: &str, node_id: Option<&str>) -> StoreResult<()>;
    /// 更新沙盒的 VMM 元数据（创建后回填）。
    async fn update_sandbox_vmm_meta(
        &self,
        id: &str,
        vmm_meta: &clouisle_core::VmmMeta,
    ) -> StoreResult<()>;
    /// Persist the expiry deadline after a sandbox becomes ready.
    async fn update_sandbox_expiry(
        &self,
        id: &str,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> StoreResult<()>;
    async fn list_sandboxes(&self, status: Option<SandboxStatus>) -> StoreResult<Vec<Sandbox>>;
    async fn delete_sandbox(&self, id: &str) -> StoreResult<()>;

    // ---- Node registry ----
    /// Upsert a node registration or heartbeat lease.
    async fn upsert_node(&self, node: &clouisle_core::RegisteredNode) -> StoreResult<()>;
    /// Return nodes whose leases have not expired at `now_ms`.
    async fn list_ready_nodes(
        &self,
        now_ms: i64,
    ) -> StoreResult<Vec<clouisle_core::RegisteredNode>>;

    // ---- 执行记录 ----
    async fn save_execution(&self, record: &ExecutionRecord) -> StoreResult<()>;
    async fn get_execution(&self, id: &str) -> StoreResult<ExecutionRecord>;
    async fn list_executions(&self, sandbox_id: &str) -> StoreResult<Vec<ExecutionRecord>>;
}
