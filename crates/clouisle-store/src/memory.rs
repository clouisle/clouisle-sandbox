//! InMemoryStore: 用于测试的纯内存实现。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use clouisle_core::{ExecutionRecord, Sandbox, SandboxStatus};

use super::store_trait::{Store, StoreError, StoreResult};

#[derive(Debug, Clone, Default)]
pub struct InMemoryStore {
    sandboxes: Arc<RwLock<HashMap<String, Sandbox>>>,
    executions: Arc<RwLock<HashMap<String, ExecutionRecord>>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for InMemoryStore {
    async fn create_sandbox(&self, sandbox: &Sandbox) -> StoreResult<()> {
        let mut map = self.sandboxes.write().await;
        if map.contains_key(&sandbox.id) {
            return Err(StoreError::Conflict(format!(
                "sandbox {} already exists",
                sandbox.id
            )));
        }
        map.insert(sandbox.id.clone(), sandbox.clone());
        Ok(())
    }

    async fn get_sandbox(&self, id: &str) -> StoreResult<Sandbox> {
        let map = self.sandboxes.read().await;
        map.get(id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(format!("sandbox {id} not found")))
    }

    async fn update_sandbox_status(&self, id: &str, status: &SandboxStatus) -> StoreResult<()> {
        let mut map = self.sandboxes.write().await;
        let sb = map
            .get_mut(id)
            .ok_or_else(|| StoreError::NotFound(format!("sandbox {id} not found")))?;
        sb.status = *status;
        sb.updated_at = chrono::Utc::now();
        if *status == SandboxStatus::Running {
            sb.ready_at = Some(sb.updated_at);
        }
        Ok(())
    }

    async fn update_sandbox_vmm_meta(
        &self,
        id: &str,
        vmm_meta: &clouisle_core::VmmMeta,
    ) -> StoreResult<()> {
        let mut map = self.sandboxes.write().await;
        let sb = map
            .get_mut(id)
            .ok_or_else(|| StoreError::NotFound(format!("sandbox {id} not found")))?;
        sb.vmm_meta = vmm_meta.clone();
        sb.updated_at = chrono::Utc::now();
        Ok(())
    }

    async fn list_sandboxes(&self, status: Option<SandboxStatus>) -> StoreResult<Vec<Sandbox>> {
        let map = self.sandboxes.read().await;
        let all: Vec<Sandbox> = map.values().cloned().collect();
        match status {
            Some(s) => Ok(all.into_iter().filter(|sb| sb.status == s).collect()),
            None => Ok(all),
        }
    }

    async fn delete_sandbox(&self, id: &str) -> StoreResult<()> {
        let mut map = self.sandboxes.write().await;
        map.remove(id)
            .ok_or_else(|| StoreError::NotFound(format!("sandbox {id} not found")))?;
        Ok(())
    }

    async fn save_execution(&self, record: &ExecutionRecord) -> StoreResult<()> {
        let mut map = self.executions.write().await;
        map.insert(record.id.clone(), record.clone());
        Ok(())
    }

    async fn get_execution(&self, id: &str) -> StoreResult<ExecutionRecord> {
        let map = self.executions.read().await;
        map.get(id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(format!("execution {id} not found")))
    }

    async fn list_executions(&self, sandbox_id: &str) -> StoreResult<Vec<ExecutionRecord>> {
        let map = self.executions.read().await;
        Ok(map
            .values()
            .filter(|r| r.sandbox_id == sandbox_id)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clouisle_core::{ExecutionSpec, ImageRef, Sandbox, SandboxEvent, SandboxSpec};

    fn make_sandbox(id: &str) -> Sandbox {
        Sandbox::new(
            id.into(),
            SandboxSpec {
                image: ImageRef::new("alpine:latest"),
                ..SandboxSpec::default()
            },
        )
    }

    fn make_exec(id: &str, sbx: &str) -> ExecutionRecord {
        ExecutionRecord {
            id: id.into(),
            sandbox_id: sbx.into(),
            spec: ExecutionSpec {
                argv: vec!["echo".into()],
                env: Default::default(),
                cwd: None,
                timeout_ms: 1000,
            },
            exit_code: 0,
            stdout: bytes::Bytes::new(),
            stderr: bytes::Bytes::new(),
            started_at: chrono::Utc::now(),
            finished_at: chrono::Utc::now(),
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
            node_id: None,
        }
    }

    #[tokio::test]
    async fn create_and_get() {
        let s = InMemoryStore::new();
        let sb = make_sandbox("sbx-1");
        s.create_sandbox(&sb).await.unwrap();
        let got = s.get_sandbox("sbx-1").await.unwrap();
        assert_eq!(got.id, "sbx-1");
    }

    #[tokio::test]
    async fn duplicate_create_conflict() {
        let s = InMemoryStore::new();
        let sb = make_sandbox("sbx-1");
        s.create_sandbox(&sb).await.unwrap();
        assert!(s.create_sandbox(&sb).await.is_err());
    }

    #[tokio::test]
    async fn update_status() {
        let s = InMemoryStore::new();
        let mut sb = make_sandbox("sbx-1");
        s.create_sandbox(&sb).await.unwrap();
        s.update_sandbox_status("sbx-1", &SandboxStatus::Running)
            .await
            .unwrap();
        let got = s.get_sandbox("sbx-1").await.unwrap();
        assert_eq!(got.status, SandboxStatus::Running);
    }

    #[tokio::test]
    async fn list_filter_by_status() {
        let s = InMemoryStore::new();
        for i in 0..5 {
            let mut sb = make_sandbox(&format!("sbx-{i}"));
            if i < 3 {
                sb.transition(SandboxEvent::Start).unwrap();
                sb.transition(SandboxEvent::AgentHello).unwrap();
            }
            s.create_sandbox(&sb).await.unwrap();
        }
        let running = s
            .list_sandboxes(Some(SandboxStatus::Running))
            .await
            .unwrap();
        assert_eq!(running.len(), 3);
    }

    #[tokio::test]
    async fn delete_and_get_not_found() {
        let s = InMemoryStore::new();
        let sb = make_sandbox("sbx-1");
        s.create_sandbox(&sb).await.unwrap();
        s.delete_sandbox("sbx-1").await.unwrap();
        assert!(s.get_sandbox("sbx-1").await.is_err());
    }

    #[tokio::test]
    async fn execution_crud() {
        let s = InMemoryStore::new();
        let rec = make_exec("exec-1", "sbx-1");
        s.save_execution(&rec).await.unwrap();
        let got = s.get_execution("exec-1").await.unwrap();
        assert_eq!(got.exit_code, 0);
        let list = s.list_executions("sbx-1").await.unwrap();
        assert_eq!(list.len(), 1);
    }
}
