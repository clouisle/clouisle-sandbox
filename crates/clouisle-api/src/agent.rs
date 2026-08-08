//! Agent 连接抽象：连接 guest agent 并等待 Hello（Stage 0.6 的 host 侧语义）。

use async_trait::async_trait;

use clouisle_core::{ClouisleError, Result};
use clouisle_vmm::VmHandle;

/// 连接器 trait：给定 VMM handle 和 sandbox_id，连接 vsock 并完成 Hello 握手。
#[async_trait]
pub trait AgentConnector: Send + Sync {
    /// 建立连接、等待 Hello 帧。返回连接句柄。
    async fn connect_and_hello(
        &self,
        handle: &VmHandle,
        sandbox_id: &str,
    ) -> Result<Box<dyn AgentConnection>>;
}

/// 一次已建立的 agent 连接（用于 exec / ping / 文件传输）。
#[async_trait]
pub trait AgentConnection: Send + Sync {
    /// 连接所属沙盒 ID。
    fn sandbox_id(&self) -> &str;
    /// 执行命令（一次性模式：等命令结束返回结果）。
    async fn exec(
        &self,
        argv: Vec<String>,
        env: std::collections::HashMap<String, String>,
        cwd: Option<String>,
        timeout_ms: u64,
    ) -> Result<clouisle_core::execution::ExecutionResult>;

    /// 写文件（FR-07）。
    async fn write_file(&self, path: &str, content: bytes::Bytes, mode: u32) -> Result<()>;

    /// 读文件（FR-07），返回内容。
    async fn read_file(&self, path: &str) -> Result<bytes::Bytes>;

    /// 列目录（FR-07）。
    async fn list_dir(&self, path: &str) -> Result<Vec<clouisle_core::types::DirEntry>>;

    /// 心跳 ping。
    async fn ping(&self) -> Result<()>;
}

/// Mock 连接器：不真的连 vsock，直接模拟 agent 行为（macOS 可跑）。
pub struct MockAgentConnector;

#[async_trait]
impl AgentConnector for MockAgentConnector {
    async fn connect_and_hello(
        &self,
        _handle: &VmHandle,
        sandbox_id: &str,
    ) -> Result<Box<dyn AgentConnection>> {
        Ok(Box::new(MockAgentConnection {
            sandbox_id: sandbox_id.to_string(),
        }))
    }
}

/// Mock 连接：用宿主机本地进程执行命令（模拟 guest exec）。
pub struct MockAgentConnection {
    sandbox_id: String,
}

#[async_trait]
impl AgentConnection for MockAgentConnection {
    fn sandbox_id(&self) -> &str {
        &self.sandbox_id
    }

    async fn exec(
        &self,
        argv: Vec<String>,
        env: std::collections::HashMap<String, String>,
        cwd: Option<String>,
        timeout_ms: u64,
    ) -> Result<clouisle_core::execution::ExecutionResult> {
        if argv.is_empty() {
            return Err(clouisle_core::ClouisleError::validation("argv empty"));
        }
        let start = std::time::Instant::now();
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .envs(env)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        let mut child = cmd.spawn().map_err(|e| {
            clouisle_core::ClouisleError::new(
                clouisle_core::ErrorKind::Vmm,
                format!("spawn {argv:?}: {e}"),
            )
        })?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        let (out_bytes, err_bytes, status, timed_out) = tokio::select! {
            status = child.wait() => {
                // 等输出读完（一次模式）
                use tokio::io::AsyncReadExt;
                let mut o = tokio::io::BufReader::new(stdout);
                let mut e = tokio::io::BufReader::new(stderr);
                let _ = tokio::join!(o.read_to_end(&mut out_buf), e.read_to_end(&mut err_buf));
                (out_buf.clone(), err_buf.clone(), status.ok(), false)
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                (out_buf, err_buf, None, true)
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        Ok(clouisle_core::execution::ExecutionResult {
            exit_code: if timed_out {
                -1
            } else {
                status.unwrap_or_default().code().unwrap_or(-1)
            },
            stdout: bytes::Bytes::from(out_bytes),
            stderr: bytes::Bytes::from(err_bytes),
            duration_ms,
        })
    }

    async fn ping(&self) -> Result<()> {
        Ok(())
    }

    async fn write_file(&self, path: &str, content: bytes::Bytes, mode: u32) -> Result<()> {
        // Mock 模式：写入宿主机临时文件，模拟 guest 文件写入
        let dir = std::env::temp_dir()
            .join("clouisle-mock-fs")
            .join(&self.sandbox_id);
        std::fs::create_dir_all(&dir).ok();
        // 路径穿越防护
        let path = path.trim_start_matches('/');
        if path.split('/').any(|seg| seg == "..") {
            return Err(ClouisleError::validation(format!(
                "path escapes sandbox root: {path}"
            )));
        }
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&target, &content)
            .map_err(|e| ClouisleError::io(format!("write {path}: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode));
        }
        Ok(())
    }

    async fn read_file(&self, path: &str) -> Result<bytes::Bytes> {
        let dir = std::env::temp_dir()
            .join("clouisle-mock-fs")
            .join(&self.sandbox_id);
        let path = path.trim_start_matches('/');
        if path.split('/').any(|seg| seg == "..") {
            return Err(ClouisleError::validation(format!(
                "path escapes sandbox root: {path}"
            )));
        }
        let target = dir.join(path);
        let data = std::fs::read(&target)
            .map_err(|e| ClouisleError::not_found(format!("read {path}: {e}")))?;
        Ok(bytes::Bytes::from(data))
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<clouisle_core::DirEntry>> {
        let dir = std::env::temp_dir()
            .join("clouisle-mock-fs")
            .join(&self.sandbox_id);
        let path = path.trim_start_matches('/');
        if path.split('/').any(|seg| seg == "..") {
            return Err(ClouisleError::validation(format!(
                "path escapes sandbox root: {path}"
            )));
        }
        let target = dir.join(path);
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&target)
            .map_err(|e| ClouisleError::not_found(format!("read_dir {path}: {e}")))?
        {
            let entry = entry.map_err(|e| ClouisleError::io(e.to_string()))?;
            let meta = entry
                .metadata()
                .map_err(|e| ClouisleError::io(e.to_string()))?;
            entries.push(clouisle_core::DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                size: meta.len(),
                mode: 0,
                mtime: 0,
                is_dir: meta.is_dir(),
            });
        }
        Ok(entries)
    }
}

/// 独立的文件系统后端（Mock 用宿主机本地文件系统模拟 guest 文件系统）。
/// 每个沙盒映射到 `<tmpdir>/clouisle-mock-fs/<sandbox_id>/`。
#[derive(Debug, Clone, Default)]
pub struct MockFsBackend;

impl MockFsBackend {
    pub fn new() -> Self {
        Self
    }

    fn sandbox_dir(&self, sandbox_id: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join("clouisle-mock-fs");
        let dir = base.join(sandbox_id);
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn resolve(&self, sandbox_id: &str, path: &str) -> Result<std::path::PathBuf> {
        let root = self.sandbox_dir(sandbox_id);
        let rel = path.trim_start_matches('/');
        // 路径穿越防护
        let resolved = root.join(rel);
        let canon_root = root.canonicalize().unwrap_or(root.clone());
        let canon_resolved = resolved.canonicalize().unwrap_or(resolved.clone());
        if !canon_resolved.starts_with(&canon_root) {
            return Err(ClouisleError::validation(format!(
                "path escapes sandbox root: {path}"
            )));
        }
        Ok(resolved)
    }
}

/// 线程安全的文件系统连接（每个 sandbox 一个实例）。
pub struct MockFsConnection {
    sandbox_id: String,
}

#[async_trait]
impl AgentConnection for MockFsConnection {
    fn sandbox_id(&self) -> &str {
        &self.sandbox_id
    }

    async fn exec(
        &self,
        _argv: Vec<String>,
        _env: std::collections::HashMap<String, String>,
        _cwd: Option<String>,
        _timeout_ms: u64,
    ) -> Result<clouisle_core::execution::ExecutionResult> {
        Err(ClouisleError::invalid_state(
            "exec not supported on fs-only connection",
        ))
    }

    async fn write_file(&self, path: &str, content: bytes::Bytes, mode: u32) -> Result<()> {
        let backend = MockFsBackend::new();
        let target = backend.resolve(&self.sandbox_id, path)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ClouisleError::io(format!("mkdir {parent:?}: {e}")))?;
        }
        std::fs::write(&target, &content)
            .map_err(|e| ClouisleError::io(format!("write {path}: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                .map_err(|e| ClouisleError::io(format!("chmod {path}: {e}")))?;
        }
        Ok(())
    }

    async fn read_file(&self, path: &str) -> Result<bytes::Bytes> {
        let backend = MockFsBackend::new();
        let target = backend.resolve(&self.sandbox_id, path)?;
        let data = std::fs::read(&target)
            .map_err(|e| ClouisleError::not_found(format!("read {path}: {e}")))?;
        Ok(bytes::Bytes::from(data))
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<clouisle_core::DirEntry>> {
        let backend = MockFsBackend::new();
        let target = backend.resolve(&self.sandbox_id, path)?;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&target)
            .map_err(|e| ClouisleError::not_found(format!("read_dir {path}: {e}")))?
        {
            let entry = entry.map_err(|e| ClouisleError::io(e.to_string()))?;
            let meta = entry
                .metadata()
                .map_err(|e| ClouisleError::io(e.to_string()))?;
            entries.push(clouisle_core::DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                size: meta.len(),
                mode: {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        meta.permissions().mode()
                    }
                    #[cfg(not(unix))]
                    {
                        0
                    }
                },
                mtime: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                is_dir: meta.is_dir(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    async fn ping(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_exec_echo() {
        let conn = MockAgentConnection {
            sandbox_id: "test-sbx".into(),
        };
        let r = conn
            .exec(
                vec!["echo".into(), "hello".into()],
                Default::default(),
                None,
                5000,
            )
            .await
            .unwrap();
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.stdout.as_ref(), b"hello\n");
    }

    #[tokio::test]
    async fn mock_exec_env_injection() {
        let conn = MockAgentConnection {
            sandbox_id: "test-sbx".into(),
        };
        let mut env = std::collections::HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        let r = conn
            .exec(
                vec!["sh".into(), "-c".into(), "echo $FOO".into()],
                env,
                None,
                5000,
            )
            .await
            .unwrap();
        assert_eq!(r.stdout.as_ref(), b"bar\n");
    }

    #[tokio::test]
    async fn mock_exec_cwd() {
        let conn = MockAgentConnection {
            sandbox_id: "test-sbx".into(),
        };
        let dir = std::env::temp_dir();
        let r = conn
            .exec(
                vec!["pwd".into()],
                Default::default(),
                Some(dir.to_string_lossy().into_owned()),
                5000,
            )
            .await
            .unwrap();
        let out = String::from_utf8_lossy(&r.stdout);
        assert!(
            out.contains(dir.file_name().unwrap().to_str().unwrap()),
            "expected cwd {} in output {out:?}",
            dir.display()
        );
    }

    #[tokio::test]
    async fn mock_exec_exit_code() {
        let conn = MockAgentConnection {
            sandbox_id: "test-sbx".into(),
        };
        let r = conn
            .exec(
                vec!["sh".into(), "-c".into(), "exit 7".into()],
                Default::default(),
                None,
                5000,
            )
            .await
            .unwrap();
        assert_eq!(r.exit_code, 7);
    }

    #[tokio::test]
    async fn mock_exec_timeout() {
        let conn = MockAgentConnection {
            sandbox_id: "test-sbx".into(),
        };
        let start = std::time::Instant::now();
        let r = conn
            .exec(
                vec!["sleep".into(), "5".into()],
                Default::default(),
                None,
                300,
            )
            .await
            .unwrap();
        assert!(start.elapsed().as_millis() < 3000);
        assert_eq!(r.exit_code, -1); // timed out marker
    }

    #[tokio::test]
    async fn mock_exec_stderr() {
        let conn = MockAgentConnection {
            sandbox_id: "test-sbx".into(),
        };
        let r = conn
            .exec(
                vec!["sh".into(), "-c".into(), "echo err >&2".into()],
                Default::default(),
                None,
                5000,
            )
            .await
            .unwrap();
        assert_eq!(r.stderr.as_ref(), b"err\n");
        assert!(r.stdout.is_empty());
    }

    #[tokio::test]
    async fn mock_connector_hello() {
        let conn = MockAgentConnector;
        let handle = clouisle_vmm::VmHandle {
            id: "x".into(),
            backend: "mock".into(),
            pid: None,
            api_socket: None,
            vsock_socket: None,
        };
        let c = conn.connect_and_hello(&handle, "test-sbx").await.unwrap();
        assert!(c.ping().await.is_ok());
    }
}
