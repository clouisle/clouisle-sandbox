//! PostgreSQL 实现（ADR-007 Phase 3）。用 `tokio-postgres`。
//!
//! 需要运行中的 Postgres 实例才能完整测试。
//! 设置 `CLOUISLE_TEST_PG` 环境变量为连接字符串（如 `host=localhost user=postgres`）启用集成测试。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use clouisle_core::{ExecutionRecord, ExecutionSpec, Sandbox, SandboxStatus, VmmMeta};
use tokio_postgres::{Client, NoTls, Row};

use super::store_trait::{Store, StoreError, StoreResult};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sandboxes (
    id TEXT PRIMARY KEY,
    spec_json TEXT NOT NULL,
    status TEXT NOT NULL,
    vmm_meta_json TEXT NOT NULL DEFAULT '{}',
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    ready_at BIGINT,
    expires_at BIGINT,
    terminal_message TEXT,
    node_id TEXT
);
CREATE TABLE IF NOT EXISTS executions (
    id TEXT PRIMARY KEY,
    sandbox_id TEXT NOT NULL,
    spec_json TEXT NOT NULL,
    exit_code INTEGER NOT NULL,
    stdout BYTEA NOT NULL,
    stderr BYTEA NOT NULL,
    started_at BIGINT NOT NULL,
    finished_at BIGINT NOT NULL,
    timed_out BOOLEAN NOT NULL DEFAULT false,
    stdout_truncated BOOLEAN NOT NULL DEFAULT false,
    stderr_truncated BOOLEAN NOT NULL DEFAULT false,
    node_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_exec_sandbox ON executions(sandbox_id);
CREATE TABLE IF NOT EXISTS nodes (
    node_id TEXT PRIMARY KEY,
    node_json TEXT NOT NULL,
    status TEXT NOT NULL,
    last_heartbeat_ms BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_nodes_ready ON nodes(status, last_heartbeat_ms);
";

/// PostgreSQL 存储。
pub struct PostgresStore {
    client: std::sync::Arc<tokio::sync::Mutex<Client>>,
}

unsafe impl Send for PostgresStore {}
unsafe impl Sync for PostgresStore {}

impl PostgresStore {
    pub async fn connect(conn_string: &str) -> Result<Self, StoreError> {
        let (client, connection) = tokio_postgres::connect(conn_string, NoTls)
            .await
            .map_err(|e| StoreError::Internal(format!("postgres connect: {e}")))?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!("postgres connection lost: {e}");
            }
        });
        client
            .batch_execute(SCHEMA)
            .await
            .map_err(|e| StoreError::Internal(format!("postgres schema: {e}")))?;
        Ok(Self {
            client: std::sync::Arc::new(tokio::sync::Mutex::new(client)),
        })
    }

    pub async fn connect_from_env() -> Option<Self> {
        let conn = std::env::var("CLOUISLE_TEST_PG").ok()?;
        Self::connect(&conn).await.ok()
    }
}

fn row_to_sandbox(row: &Row) -> StoreResult<Sandbox> {
    let spec_json: String = row
        .try_get(1)
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let status_str: String = row
        .try_get(2)
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let vmm_json: String = row
        .try_get(3)
        .map_err(|e| StoreError::Internal(e.to_string()))?;

    let spec: clouisle_core::SandboxSpec = serde_json::from_str(&spec_json)
        .map_err(|e| StoreError::Internal(format!("bad spec: {e}")))?;
    let vmm_meta: VmmMeta = serde_json::from_str(&vmm_json)
        .map_err(|e| StoreError::Internal(format!("bad vmm: {e}")))?;
    let status = match status_str.as_str() {
        "pending" => SandboxStatus::Pending,
        "starting" => SandboxStatus::Starting,
        "running" => SandboxStatus::Running,
        "stopping" => SandboxStatus::Stopping,
        "stopped" => SandboxStatus::Stopped,
        "error" => SandboxStatus::Error,
        other => return Err(StoreError::Internal(format!("bad status: {other}"))),
    };
    Ok(Sandbox {
        id: row
            .try_get(0)
            .map_err(|e| StoreError::Internal(e.to_string()))?,
        spec,
        status,
        created_at: DateTime::from_timestamp_millis(row.try_get::<_, i64>(4).unwrap_or(0))
            .unwrap_or_else(Utc::now),
        updated_at: DateTime::from_timestamp_millis(row.try_get::<_, i64>(5).unwrap_or(0))
            .unwrap_or_else(Utc::now),
        ready_at: row
            .try_get::<_, Option<i64>>(6)
            .ok()
            .flatten()
            .and_then(DateTime::from_timestamp_millis),
        expires_at: row
            .try_get::<_, Option<i64>>(7)
            .ok()
            .flatten()
            .and_then(DateTime::from_timestamp_millis),
        vmm_meta,
        terminal_message: row.try_get(8).ok().flatten(),
        node_id: row.try_get(9).ok().flatten(),
    })
}

fn exec_from_row(row: &Row) -> StoreResult<ExecutionRecord> {
    let spec_json: String = row
        .try_get(2)
        .map_err(|e| StoreError::Internal(e.to_string()))?;
    let spec: ExecutionSpec =
        serde_json::from_str(&spec_json).map_err(|e| StoreError::Internal(e.to_string()))?;
    Ok(ExecutionRecord {
        id: row
            .try_get(0)
            .map_err(|e| StoreError::Internal(e.to_string()))?,
        sandbox_id: row
            .try_get(1)
            .map_err(|e| StoreError::Internal(e.to_string()))?,
        spec,
        exit_code: row
            .try_get(3)
            .map_err(|e| StoreError::Internal(e.to_string()))?,
        stdout: bytes::Bytes::from(
            row.try_get::<_, Vec<u8>>(4)
                .map_err(|e| StoreError::Internal(e.to_string()))?,
        ),
        stderr: bytes::Bytes::from(
            row.try_get::<_, Vec<u8>>(5)
                .map_err(|e| StoreError::Internal(e.to_string()))?,
        ),
        started_at: DateTime::from_timestamp_millis(row.try_get::<_, i64>(6).unwrap_or(0))
            .unwrap_or_else(Utc::now),
        finished_at: DateTime::from_timestamp_millis(row.try_get::<_, i64>(7).unwrap_or(0))
            .unwrap_or_else(Utc::now),
        timed_out: row.try_get::<_, bool>(8).unwrap_or(false),
        stdout_truncated: row.try_get::<_, bool>(9).unwrap_or(false),
        stderr_truncated: row.try_get::<_, bool>(10).unwrap_or(false),
        node_id: row.try_get(11).ok().flatten(),
    })
}

#[async_trait]
impl Store for PostgresStore {
    async fn create_sandbox(&self, sandbox: &Sandbox) -> StoreResult<()> {
        let client = self.client.lock().await;
        let spec_json = serde_json::to_string(&sandbox.spec)
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let vmm_json = serde_json::to_string(&sandbox.vmm_meta)
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        client.execute(
            "INSERT INTO sandboxes (id,spec_json,status,vmm_meta_json,created_at,updated_at,ready_at,expires_at,terminal_message,node_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
            &[&sandbox.id, &spec_json, &sandbox.status.as_str().to_string(), &vmm_json,
              &sandbox.created_at.timestamp_millis(), &sandbox.updated_at.timestamp_millis(),
              &sandbox.ready_at.map(|t| t.timestamp_millis()),
              &sandbox.expires_at.map(|t| t.timestamp_millis()),
              &sandbox.terminal_message, &sandbox.node_id],
        ).await.map_err(|e| {
            if e.code().map(|c| c.code() == "23505").unwrap_or(false) {
                StoreError::Conflict(format!("sandbox {} exists", sandbox.id))
            } else {
                StoreError::Internal(format!("pg insert: {e}"))
            }
        })?;
        Ok(())
    }

    async fn get_sandbox(&self, id: &str) -> StoreResult<Sandbox> {
        let client = self.client.lock().await;
        let row = client.query_opt(
            "SELECT id,spec_json,status,vmm_meta_json,created_at,updated_at,ready_at,expires_at,terminal_message,node_id FROM sandboxes WHERE id=$1",
            &[&id],
        ).await.map_err(|e| StoreError::Internal(e.to_string()))?;
        let row = row.ok_or_else(|| StoreError::NotFound(format!("sandbox {id}")))?;
        row_to_sandbox(&row)
    }

    async fn update_sandbox_status(&self, id: &str, status: &SandboxStatus) -> StoreResult<()> {
        let client = self.client.lock().await;
        let n = client
            .execute(
                "UPDATE sandboxes SET status=$1, updated_at=$2 WHERE id=$3",
                &[
                    &status.as_str().to_string(),
                    &Utc::now().timestamp_millis(),
                    &id,
                ],
            )
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("sandbox {id}")));
        }
        Ok(())
    }

    async fn update_sandbox_vmm_meta(&self, id: &str, vmm_meta: &VmmMeta) -> StoreResult<()> {
        let client = self.client.lock().await;
        let vmm_json =
            serde_json::to_string(vmm_meta).map_err(|e| StoreError::Internal(e.to_string()))?;
        let n = client
            .execute(
                "UPDATE sandboxes SET vmm_meta_json=$1, updated_at=$2 WHERE id=$3",
                &[&vmm_json, &Utc::now().timestamp_millis(), &id],
            )
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("sandbox {id}")));
        }
        Ok(())
    }

    async fn update_sandbox_expiry(
        &self,
        id: &str,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> StoreResult<()> {
        let client = self.client.lock().await;
        let updated = client
            .execute(
                "UPDATE sandboxes SET expires_at=$1, updated_at=$2 WHERE id=$3",
                &[
                    &expires_at.map(|value| value.timestamp_millis()),
                    &Utc::now().timestamp_millis(),
                    &id,
                ],
            )
            .await
            .map_err(|error| StoreError::Internal(error.to_string()))?;
        if updated == 0 {
            return Err(StoreError::NotFound(format!("sandbox {id}")));
        }
        Ok(())
    }

    async fn list_sandboxes(&self, status: Option<SandboxStatus>) -> StoreResult<Vec<Sandbox>> {
        let client = self.client.lock().await;
        let query = "SELECT id,spec_json,status,vmm_meta_json,created_at,updated_at,ready_at,expires_at,terminal_message,node_id FROM sandboxes";
        let rows = match status {
            Some(s) => {
                client
                    .query(
                        &format!("{query} WHERE status=$1"),
                        &[&s.as_str().to_string()],
                    )
                    .await
            }
            None => client.query(query, &[]).await,
        }
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        rows.iter().map(row_to_sandbox).collect()
    }

    async fn delete_sandbox(&self, id: &str) -> StoreResult<()> {
        let client = self.client.lock().await;
        let n = client
            .execute("DELETE FROM sandboxes WHERE id=$1", &[&id])
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("sandbox {id}")));
        }
        Ok(())
    }

    async fn upsert_node(&self, node: &clouisle_core::RegisteredNode) -> StoreResult<()> {
        let json = serde_json::to_string(node).map_err(|e| StoreError::Internal(e.to_string()))?;
        let status =
            serde_json::to_string(&node.status).map_err(|e| StoreError::Internal(e.to_string()))?;
        self.client.lock().await.execute(
            "INSERT INTO nodes(node_id,node_json,status,last_heartbeat_ms) VALUES($1,$2,$3,$4) ON CONFLICT(node_id) DO UPDATE SET node_json=EXCLUDED.node_json,status=EXCLUDED.status,last_heartbeat_ms=EXCLUDED.last_heartbeat_ms",
            &[&node.info.node_id, &json, &status, &node.last_heartbeat_ms],
        ).await.map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn list_ready_nodes(
        &self,
        now_ms: i64,
    ) -> StoreResult<Vec<clouisle_core::RegisteredNode>> {
        let rows = self
            .client
            .lock()
            .await
            .query(
                "SELECT node_json FROM nodes WHERE status=$1 AND last_heartbeat_ms >= $2",
                &[&"\"ready\"", &now_ms],
            )
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        rows.iter()
            .map(|row| {
                let json: String = row
                    .try_get(0)
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
                serde_json::from_str(&json).map_err(|e| StoreError::Internal(e.to_string()))
            })
            .collect()
    }

    async fn save_execution(&self, record: &ExecutionRecord) -> StoreResult<()> {
        let client = self.client.lock().await;
        let spec_json =
            serde_json::to_string(&record.spec).map_err(|e| StoreError::Internal(e.to_string()))?;
        client.execute(
            "INSERT INTO executions (id,sandbox_id,spec_json,exit_code,stdout,stderr,started_at,finished_at,timed_out,stdout_truncated,stderr_truncated,node_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
            &[&record.id, &record.sandbox_id, &spec_json, &record.exit_code,
              &record.stdout.to_vec(), &record.stderr.to_vec(),
              &record.started_at.timestamp_millis(), &record.finished_at.timestamp_millis(),
              &record.timed_out, &record.stdout_truncated, &record.stderr_truncated, &record.node_id],
        ).await.map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn get_execution(&self, id: &str) -> StoreResult<ExecutionRecord> {
        let client = self.client.lock().await;
        let row = client.query_opt(
            "SELECT id,sandbox_id,spec_json,exit_code,stdout,stderr,started_at,finished_at,timed_out,stdout_truncated,stderr_truncated,node_id FROM executions WHERE id=$1",
            &[&id],
        ).await.map_err(|e| StoreError::Internal(e.to_string()))?;
        let row = row.ok_or_else(|| StoreError::NotFound(format!("execution {id}")))?;
        exec_from_row(&row)
    }

    async fn list_executions(&self, sandbox_id: &str) -> StoreResult<Vec<ExecutionRecord>> {
        let client = self.client.lock().await;
        let rows = client.query(
            "SELECT id,sandbox_id,spec_json,exit_code,stdout,stderr,started_at,finished_at,timed_out,stdout_truncated,stderr_truncated,node_id FROM executions WHERE sandbox_id=$1 ORDER BY started_at DESC",
            &[&sandbox_id],
        ).await.map_err(|e| StoreError::Internal(e.to_string()))?;
        rows.iter().map(exec_from_row).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn integration_gated() {
        if std::env::var("CLOUISLE_TEST_PG").is_err() {
            eprintln!("SKIP: set CLOUISLE_TEST_PG to run Postgres integration test");
            return;
        }
        let store = PostgresStore::connect_from_env().await.unwrap();
        let sb =
            clouisle_core::Sandbox::new("pg-sbx-1".into(), clouisle_core::SandboxSpec::default());
        store.create_sandbox(&sb).await.unwrap();
        let got = store.get_sandbox("pg-sbx-1").await.unwrap();
        assert_eq!(got.id, "pg-sbx-1");
        store.delete_sandbox("pg-sbx-1").await.unwrap();
    }
}
