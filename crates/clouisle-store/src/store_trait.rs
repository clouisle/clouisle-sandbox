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
    /// 更新沙盒的 VMM 元数据（创建后回填）。
    async fn update_sandbox_vmm_meta(
        &self,
        id: &str,
        vmm_meta: &clouisle_core::VmmMeta,
    ) -> StoreResult<()>;
    async fn list_sandboxes(&self, status: Option<SandboxStatus>) -> StoreResult<Vec<Sandbox>>;
    async fn delete_sandbox(&self, id: &str) -> StoreResult<()>;

    // ---- 执行记录 ----
    async fn save_execution(&self, record: &ExecutionRecord) -> StoreResult<()>;
    async fn get_execution(&self, id: &str) -> StoreResult<ExecutionRecord>;
    async fn list_executions(&self, sandbox_id: &str) -> StoreResult<Vec<ExecutionRecord>>;
}
