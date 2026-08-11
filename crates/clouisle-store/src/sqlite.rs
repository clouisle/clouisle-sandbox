//! SQLite 实现（ADR-007）。WAL 模式，`synchronous=NORMAL`，`foreign_keys=ON`。

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use clouisle_core::{ExecutionRecord, ExecutionSpec, Sandbox, SandboxSpec, SandboxStatus, VmmMeta};
use rusqlite::{Connection, Row};
use tokio::sync::Mutex;

use super::store_trait::{Store, StoreError, StoreResult};

/// SQLite 存储。写操作串行化（单写者），WAL 保证读并发。
#[derive(Clone)]
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

// rusqlite::Connection 不是 Send。这里包在 Mutex 里并手动标记 Send。
// 真实工程应使用 `tokio-rusqlite` 的专用 blocking runtime；此最小实现
// 用于 macOS 跑通单测，Phase 3 由 Postgres 取代。
unsafe impl Send for SqliteStore {}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sandboxes (
    id TEXT PRIMARY KEY,
    spec_json TEXT NOT NULL,
    status TEXT NOT NULL,
    vmm_meta_json TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    ready_at INTEGER,
    expires_at INTEGER,
    terminal_message TEXT,
    node_id TEXT
);
CREATE TABLE IF NOT EXISTS executions (
    id TEXT PRIMARY KEY,
    sandbox_id TEXT NOT NULL,
    spec_json TEXT NOT NULL,
    exit_code INTEGER NOT NULL,
    stdout BLOB NOT NULL,
    stderr BLOB NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER NOT NULL,
    timed_out INTEGER NOT NULL DEFAULT 0,
    stdout_truncated INTEGER NOT NULL DEFAULT 0,
    stderr_truncated INTEGER NOT NULL DEFAULT 0,
    node_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_exec_sandbox ON executions(sandbox_id);
CREATE TABLE IF NOT EXISTS nodes (
    node_id TEXT PRIMARY KEY,
    node_json TEXT NOT NULL,
    status TEXT NOT NULL,
    last_heartbeat_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_nodes_ready ON nodes(status, last_heartbeat_ms);
";

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> StoreResult<()> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| StoreError::Sqlite(error.to_string()))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| StoreError::Sqlite(error.to_string()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| StoreError::Sqlite(error.to_string()))?;
    if !columns.iter().any(|existing| existing == column) {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))
        .map_err(|error| StoreError::Sqlite(error.to_string()))?;
    }
    Ok(())
}

struct RowData {
    id: String,
    spec_json: String,
    status: String,
    vmm_meta_json: String,
    created_at: i64,
    updated_at: i64,
    ready_at: Option<i64>,
    expires_at: Option<i64>,
    terminal_message: Option<String>,
    node_id: Option<String>,
}

impl SqliteStore {
    /// 打开（或创建）数据库文件。
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        let conn =
            Connection::open(path.as_ref()).map_err(|e| StoreError::Sqlite(e.to_string()))?;
        Self::init(conn)
    }

    /// 打开内存数据库（测试用）。
    pub fn open_in_memory() -> StoreResult<Self> {
        let conn = Connection::open_in_memory().map_err(|e| StoreError::Sqlite(e.to_string()))?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> StoreResult<Self> {
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        add_column_if_missing(
            &conn,
            "sandboxes",
            "vmm_meta_json",
            "TEXT NOT NULL DEFAULT '{}'",
        )?;
        add_column_if_missing(&conn, "sandboxes", "ready_at", "INTEGER")?;
        add_column_if_missing(&conn, "sandboxes", "expires_at", "INTEGER")?;
        add_column_if_missing(&conn, "sandboxes", "terminal_message", "TEXT")?;
        add_column_if_missing(&conn, "sandboxes", "node_id", "TEXT")?;
        add_column_if_missing(
            &conn,
            "executions",
            "timed_out",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column_if_missing(
            &conn,
            "executions",
            "stdout_truncated",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column_if_missing(
            &conn,
            "executions",
            "stderr_truncated",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column_if_missing(&conn, "executions", "node_id", "TEXT")?;
        add_column_if_missing(
            &conn,
            "nodes",
            "last_heartbeat_ms",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        Ok(SqliteStore {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

fn row_data_from(r: &Row) -> rusqlite::Result<RowData> {
    Ok(RowData {
        id: r.get(0)?,
        spec_json: r.get(1)?,
        status: r.get(2)?,
        vmm_meta_json: r.get(3)?,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
        ready_at: r.get(6)?,
        expires_at: r.get(7)?,
        terminal_message: r.get(8)?,
        node_id: r.get(9)?,
    })
}

fn row_to_sandbox(row: RowData) -> StoreResult<Sandbox> {
    let spec: clouisle_core::SandboxSpec = serde_json::from_str(&row.spec_json)
        .map_err(|e| StoreError::Internal(format!("bad spec_json: {e}")))?;
    let vmm_meta: VmmMeta = serde_json::from_str(&row.vmm_meta_json)
        .map_err(|e| StoreError::Internal(format!("bad vmm_meta_json: {e}")))?;
    let status = match row.status.as_str() {
        "pending" => SandboxStatus::Pending,
        "starting" => SandboxStatus::Starting,
        "running" => SandboxStatus::Running,
        "paused" => SandboxStatus::Paused,
        "stopping" => SandboxStatus::Stopping,
        "stopped" => SandboxStatus::Stopped,
        "error" => SandboxStatus::Error,
        other => return Err(StoreError::Internal(format!("bad status: {other}"))),
    };
    Ok(Sandbox {
        id: row.id,
        spec,
        status,
        created_at: DateTime::from_timestamp_millis(row.created_at).unwrap_or_else(Utc::now),
        updated_at: DateTime::from_timestamp_millis(row.updated_at).unwrap_or_else(Utc::now),
        ready_at: row.ready_at.and_then(DateTime::from_timestamp_millis),
        expires_at: row.expires_at.and_then(DateTime::from_timestamp_millis),
        vmm_meta,
        terminal_message: row.terminal_message.clone(),
        node_id: row.node_id.clone(),
    })
}

#[async_trait]
impl Store for SqliteStore {
    async fn create_sandbox(&self, sandbox: &Sandbox) -> StoreResult<()> {
        let conn = self.conn.lock().await;
        let spec_json = serde_json::to_string(&sandbox.spec)
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let vmm_meta_json = serde_json::to_string(&sandbox.vmm_meta)
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        conn.execute(
            "INSERT INTO sandboxes (id, spec_json, status, vmm_meta_json, created_at, updated_at, ready_at, expires_at, terminal_message, node_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                sandbox.id,
                spec_json,
                sandbox.status.as_str(),
                vmm_meta_json,
                sandbox.created_at.timestamp_millis(),
                sandbox.updated_at.timestamp_millis(),
                sandbox.ready_at.map(|t| t.timestamp_millis()),
                sandbox.expires_at.map(|t| t.timestamp_millis()),
                sandbox.terminal_message,
                sandbox.node_id,
            ],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.contains("UNIQUE constraint failed") => {
                StoreError::Conflict(format!("sandbox {} already exists", sandbox.id))
            }
            other => StoreError::Sqlite(other.to_string()),
        })?;
        Ok(())
    }

    async fn get_sandbox(&self, id: &str) -> StoreResult<Sandbox> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT id, spec_json, status, vmm_meta_json, created_at, updated_at, ready_at, expires_at, terminal_message, node_id
                 FROM sandboxes WHERE id = ?1",
            )
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        let row = stmt.query_row([id], row_data_from).map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound(format!("sandbox {id}")),
            other => StoreError::Sqlite(other.to_string()),
        })?;
        row_to_sandbox(row)
    }

    async fn update_sandbox_status(&self, id: &str, status: &SandboxStatus) -> StoreResult<()> {
        let conn = self.conn.lock().await;
        let now = Utc::now().timestamp_millis();
        let ready_at = (*status == SandboxStatus::Running).then_some(now);
        let n = conn
            .execute(
                "UPDATE sandboxes SET status=?1, ready_at=COALESCE(ready_at,?2), updated_at=?3 WHERE id=?4",
                rusqlite::params![status.as_str(), ready_at, now, id],
            )
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("sandbox {id}")));
        }
        Ok(())
    }

    async fn update_sandbox_spec(&self, id: &str, spec: &SandboxSpec) -> StoreResult<()> {
        let conn = self.conn.lock().await;
        let spec_json =
            serde_json::to_string(spec).map_err(|error| StoreError::Internal(error.to_string()))?;
        let updated = conn
            .execute(
                "UPDATE sandboxes SET spec_json=?1, updated_at=?2 WHERE id=?3",
                rusqlite::params![spec_json, Utc::now().timestamp_millis(), id],
            )
            .map_err(|error| StoreError::Sqlite(error.to_string()))?;
        if updated == 0 {
            return Err(StoreError::NotFound(format!("sandbox {id}")));
        }
        Ok(())
    }

    async fn update_sandbox_status_message(
        &self,
        id: &str,
        status: &SandboxStatus,
        message: Option<&str>,
    ) -> StoreResult<()> {
        let conn = self.conn.lock().await;
        let now = Utc::now().timestamp_millis();
        let ready_at = (*status == SandboxStatus::Running).then_some(now);
        let updated = conn
            .execute(
                "UPDATE sandboxes SET status=?1, terminal_message=?2, ready_at=COALESCE(ready_at,?3), updated_at=?4 WHERE id=?5",
                rusqlite::params![status.as_str(), message, ready_at, now, id],
            )
            .map_err(|error| StoreError::Sqlite(error.to_string()))?;
        if updated == 0 {
            return Err(StoreError::NotFound(format!("sandbox {id}")));
        }
        Ok(())
    }

    async fn update_sandbox_node(&self, id: &str, node_id: Option<&str>) -> StoreResult<()> {
        let conn = self.conn.lock().await;
        let updated = conn
            .execute(
                "UPDATE sandboxes SET node_id=?1, updated_at=?2 WHERE id=?3",
                rusqlite::params![node_id, Utc::now().timestamp_millis(), id],
            )
            .map_err(|error| StoreError::Sqlite(error.to_string()))?;
        if updated == 0 {
            return Err(StoreError::NotFound(format!("sandbox {id}")));
        }
        Ok(())
    }

    async fn update_sandbox_vmm_meta(&self, id: &str, vmm_meta: &VmmMeta) -> StoreResult<()> {
        let conn = self.conn.lock().await;
        let vmm_meta_json =
            serde_json::to_string(vmm_meta).map_err(|e| StoreError::Internal(e.to_string()))?;
        let now = Utc::now().timestamp_millis();
        let n = conn
            .execute(
                "UPDATE sandboxes SET vmm_meta_json=?1, updated_at=?2 WHERE id=?3",
                rusqlite::params![vmm_meta_json, now, id],
            )
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
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
        let conn = self.conn.lock().await;
        let updated = conn
            .execute(
                "UPDATE sandboxes SET expires_at=?1, updated_at=?2 WHERE id=?3",
                rusqlite::params![
                    expires_at.map(|value| value.timestamp_millis()),
                    Utc::now().timestamp_millis(),
                    id
                ],
            )
            .map_err(|error| StoreError::Sqlite(error.to_string()))?;
        if updated == 0 {
            return Err(StoreError::NotFound(format!("sandbox {id}")));
        }
        Ok(())
    }

    async fn list_sandboxes(&self, status: Option<SandboxStatus>) -> StoreResult<Vec<Sandbox>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT id, spec_json, status, vmm_meta_json, created_at, updated_at, ready_at, expires_at, terminal_message, node_id FROM sandboxes")
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        let rows: Vec<RowData> = stmt
            .query_map([], row_data_from)
            .map_err(|e| StoreError::Sqlite(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();
        let mut out: Vec<Sandbox> = rows
            .into_iter()
            .filter_map(|r| row_to_sandbox(r).ok())
            .collect();
        if let Some(s) = status {
            out.retain(|sb| sb.status == s);
        }
        Ok(out)
    }

    async fn delete_sandbox(&self, id: &str) -> StoreResult<()> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute("DELETE FROM sandboxes WHERE id=?1", [id])
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("sandbox {id}")));
        }
        Ok(())
    }

    async fn upsert_node(&self, node: &clouisle_core::RegisteredNode) -> StoreResult<()> {
        let json = serde_json::to_string(node).map_err(|e| StoreError::Internal(e.to_string()))?;
        self.conn.lock().await.execute(
            "INSERT INTO nodes(node_id,node_json,status,last_heartbeat_ms) VALUES (?1,?2,?3,?4) ON CONFLICT(node_id) DO UPDATE SET node_json=excluded.node_json,status=excluded.status,last_heartbeat_ms=excluded.last_heartbeat_ms",
            rusqlite::params![node.info.node_id, json, serde_json::to_string(&node.status).unwrap_or_default(), node.last_heartbeat_ms],
        ).map_err(|e| StoreError::Sqlite(e.to_string()))?;
        Ok(())
    }

    async fn list_ready_nodes(
        &self,
        now_ms: i64,
    ) -> StoreResult<Vec<clouisle_core::RegisteredNode>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT node_json FROM nodes WHERE status='\"ready\"' AND last_heartbeat_ms >= ?1",
            )
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        stmt.query_map([now_ms], |row| row.get::<_, String>(0))
            .map_err(|e| StoreError::Sqlite(e.to_string()))?
            .map(|row| {
                row.map_err(|e| StoreError::Sqlite(e.to_string()))
                    .and_then(|json| {
                        serde_json::from_str(&json).map_err(|e| StoreError::Internal(e.to_string()))
                    })
            })
            .collect()
    }

    async fn save_execution(&self, record: &ExecutionRecord) -> StoreResult<()> {
        let conn = self.conn.lock().await;
        let spec_json =
            serde_json::to_string(&record.spec).map_err(|e| StoreError::Internal(e.to_string()))?;
        conn.execute(
            "INSERT INTO executions (id, sandbox_id, spec_json, exit_code, stdout, stderr, started_at, finished_at, timed_out, stdout_truncated, stderr_truncated, node_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                record.id,
                record.sandbox_id,
                spec_json,
                record.exit_code,
                record.stdout.to_vec(),
                record.stderr.to_vec(),
                record.started_at.timestamp_millis(),
                record.finished_at.timestamp_millis(),
                record.timed_out as i32,
                record.stdout_truncated as i32,
                record.stderr_truncated as i32,
                record.node_id,
            ],
        )
        .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        Ok(())
    }

    async fn get_execution(&self, id: &str) -> StoreResult<ExecutionRecord> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT id, sandbox_id, spec_json, exit_code, stdout, stderr, started_at, finished_at, timed_out, stdout_truncated, stderr_truncated, node_id
                 FROM executions WHERE id = ?1",
            )
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        stmt.query_row([id], |r| {
            let spec_json: String = r.get(2)?;
            let spec: ExecutionSpec = serde_json::from_str(&spec_json).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok(ExecutionRecord {
                id: r.get(0)?,
                sandbox_id: r.get(1)?,
                spec,
                exit_code: r.get(3)?,
                stdout: bytes::Bytes::from(r.get::<_, Vec<u8>>(4)?),
                stderr: bytes::Bytes::from(r.get::<_, Vec<u8>>(5)?),
                started_at: DateTime::from_timestamp_millis(r.get::<_, i64>(6)?)
                    .unwrap_or_else(Utc::now),
                finished_at: DateTime::from_timestamp_millis(r.get::<_, i64>(7)?)
                    .unwrap_or_else(Utc::now),
                timed_out: r.get::<_, i32>(8)? != 0,
                stdout_truncated: r.get::<_, i32>(9)? != 0,
                stderr_truncated: r.get::<_, i32>(10)? != 0,
                node_id: r.get(11)?,
            })
        })
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound(format!("execution {id}")),
            other => StoreError::Sqlite(other.to_string()),
        })
    }

    async fn list_executions(&self, sandbox_id: &str) -> StoreResult<Vec<ExecutionRecord>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT id, sandbox_id, spec_json, exit_code, stdout, stderr, started_at, finished_at, timed_out, stdout_truncated, stderr_truncated, node_id
                 FROM executions WHERE sandbox_id = ?1 ORDER BY started_at DESC",
            )
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        let rows = stmt
            .query_map([sandbox_id], |r| {
                let spec_json: String = r.get(2)?;
                let spec: ExecutionSpec = serde_json::from_str(&spec_json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                Ok(ExecutionRecord {
                    id: r.get(0)?,
                    sandbox_id: r.get(1)?,
                    spec,
                    exit_code: r.get(3)?,
                    stdout: bytes::Bytes::from(r.get::<_, Vec<u8>>(4)?),
                    stderr: bytes::Bytes::from(r.get::<_, Vec<u8>>(5)?),
                    started_at: DateTime::from_timestamp_millis(r.get::<_, i64>(6)?)
                        .unwrap_or_else(Utc::now),
                    finished_at: DateTime::from_timestamp_millis(r.get::<_, i64>(7)?)
                        .unwrap_or_else(Utc::now),
                    timed_out: r.get::<_, i32>(8)? != 0,
                    stdout_truncated: r.get::<_, i32>(9)? != 0,
                    stderr_truncated: r.get::<_, i32>(10)? != 0,
                    node_id: r.get(11)?,
                })
            })
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| StoreError::Sqlite(e.to_string()))?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clouisle_core::{ImageRef, Sandbox, SandboxEvent, SandboxSpec};

    fn make_sandbox(id: &str) -> Sandbox {
        Sandbox::new(
            id.into(),
            SandboxSpec {
                image: ImageRef::new("alpine:latest"),
                ..SandboxSpec::default()
            },
        )
    }

    #[tokio::test]
    async fn create_get_delete_sqlite() {
        let s = SqliteStore::open_in_memory().unwrap();
        let sb = make_sandbox("sbx-1");
        s.create_sandbox(&sb).await.unwrap();
        let got = s.get_sandbox("sbx-1").await.unwrap();
        assert_eq!(got.id, "sbx-1");
        assert_eq!(got.status, SandboxStatus::Pending);
        s.delete_sandbox("sbx-1").await.unwrap();
        assert!(s.get_sandbox("sbx-1").await.is_err());
    }

    #[tokio::test]
    async fn status_update_persists() {
        let s = SqliteStore::open_in_memory().unwrap();
        let mut sb = make_sandbox("sbx-1");
        sb.transition(SandboxEvent::Start).unwrap();
        sb.transition(SandboxEvent::AgentHello).unwrap();
        s.create_sandbox(&sb).await.unwrap();
        s.update_sandbox_status("sbx-1", &SandboxStatus::Stopping)
            .await
            .unwrap();
        let got = s.get_sandbox("sbx-1").await.unwrap();
        assert_eq!(got.status, SandboxStatus::Stopping);
    }

    #[tokio::test]
    async fn expiry_update_persists() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_sandbox(&make_sandbox("sbx-1")).await.unwrap();
        let expires_at =
            DateTime::from_timestamp_millis(Utc::now().timestamp_millis() + 60_000).unwrap();
        store
            .update_sandbox_expiry("sbx-1", Some(expires_at))
            .await
            .unwrap();
        assert_eq!(
            store.get_sandbox("sbx-1").await.unwrap().expires_at,
            Some(expires_at)
        );
    }

    #[tokio::test]
    async fn list_filter_sqlite() {
        let s = SqliteStore::open_in_memory().unwrap();
        for i in 0..5 {
            let mut sb = make_sandbox(&format!("sbx-{i}"));
            if i < 2 {
                sb.transition(SandboxEvent::Start).unwrap();
                sb.transition(SandboxEvent::AgentHello).unwrap();
            }
            s.create_sandbox(&sb).await.unwrap();
        }
        let running = s
            .list_sandboxes(Some(SandboxStatus::Running))
            .await
            .unwrap();
        assert_eq!(running.len(), 2);
        let all = s.list_sandboxes(None).await.unwrap();
        assert_eq!(all.len(), 5);
    }

    #[tokio::test]
    async fn running_status_sets_and_retains_ready_at() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .create_sandbox(&make_sandbox("ready-sbx"))
            .await
            .unwrap();
        store
            .update_sandbox_status("ready-sbx", &SandboxStatus::Running)
            .await
            .unwrap();
        let ready_at = store.get_sandbox("ready-sbx").await.unwrap().ready_at;
        assert!(ready_at.is_some());
        store
            .update_sandbox_status_message(
                "ready-sbx",
                &SandboxStatus::Error,
                Some("failed after start"),
            )
            .await
            .unwrap();
        assert_eq!(
            store.get_sandbox("ready-sbx").await.unwrap().ready_at,
            ready_at
        );
    }

    #[tokio::test]
    async fn duplicate_sqlite_conflict() {
        let s = SqliteStore::open_in_memory().unwrap();
        let sb = make_sandbox("sbx-1");
        s.create_sandbox(&sb).await.unwrap();
        let err = s.create_sandbox(&sb).await.unwrap_err();
        assert!(matches!(err, StoreError::Conflict(_)));
    }

    #[tokio::test]
    async fn execution_roundtrip_sqlite() {
        let s = SqliteStore::open_in_memory().unwrap();
        let sb = make_sandbox("sbx-1");
        s.create_sandbox(&sb).await.unwrap();
        let rec = ExecutionRecord {
            id: "exec-1".into(),
            sandbox_id: "sbx-1".into(),
            spec: ExecutionSpec {
                argv: vec!["echo".into(), "hi".into()],
                env: Default::default(),
                cwd: None,
                timeout_ms: 1000,
            },
            exit_code: 7,
            stdout: bytes::Bytes::from_static(b"hi\n"),
            stderr: bytes::Bytes::new(),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
            node_id: None,
        };
        s.save_execution(&rec).await.unwrap();
        let got = s.get_execution("exec-1").await.unwrap();
        assert_eq!(got.exit_code, 7);
        assert_eq!(got.stdout.as_ref(), b"hi\n");
        let list = s.list_executions("sbx-1").await.unwrap();
        assert_eq!(list.len(), 1);
    }
}
