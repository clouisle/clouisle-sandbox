//! Agent 连接抽象：连接 guest agent 并等待 Hello（Stage 0.6 的 host 侧语义）。

use async_trait::async_trait;
#[cfg(any(test, feature = "test-utils"))]
use std::sync::Arc;

use clouisle_core::Result;
use clouisle_core::{ClouisleError, ErrorKind};
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

/// Incremental command output forwarded to HTTP SSE or node gRPC clients.
#[derive(Debug)]
pub enum ExecStreamEvent {
    Stdout(bytes::Bytes),
    Stderr(bytes::Bytes),
    Exit(i32),
    Error(String),
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

    /// Execute while yielding guest output chunks in arrival order.
    async fn exec_stream(
        &self,
        argv: Vec<String>,
        env: std::collections::HashMap<String, String>,
        cwd: Option<String>,
        timeout_ms: u64,
        events: tokio::sync::mpsc::Sender<ExecStreamEvent>,
    ) -> Result<()> {
        let result = self.exec(argv, env, cwd, timeout_ms).await?;
        if !result.stdout.is_empty() {
            let _ = events.send(ExecStreamEvent::Stdout(result.stdout)).await;
        }
        if !result.stderr.is_empty() {
            let _ = events.send(ExecStreamEvent::Stderr(result.stderr)).await;
        }
        let _ = events.send(ExecStreamEvent::Exit(result.exit_code)).await;
        Ok(())
    }

    /// 写文件（FR-07）。
    async fn write_file(&self, path: &str, content: bytes::Bytes, mode: u32) -> Result<()>;
    /// 施加 guest 内资源限制（cgroup v2 pids.max）。None 不修改。
    async fn apply_limits(&self, pids_max: Option<u32>) -> Result<()>;

    /// 读文件（FR-07），返回内容。
    async fn read_file(&self, path: &str) -> Result<bytes::Bytes>;

    /// 列目录（FR-07）。
    async fn list_dir(&self, path: &str) -> Result<Vec<clouisle_core::types::DirEntry>>;

    /// 心跳 ping。
    async fn ping(&self) -> Result<()>;

    /// 启动长生命周期进程（可选 stdin/PTY），握手完成后立即返回 guest 帧 id
    /// （供 send_stdin/send_signal/resize_pty 寻址）。输出随后经
    /// [`AgentConnection::stream_process_events`] 转发。
    async fn start_process(
        &self,
        argv: Vec<String>,
        env: std::collections::HashMap<String, String>,
        cwd: Option<String>,
        timeout_ms: u64,
        stdin: bool,
        pty: Option<clouisle_proto::PtyConfig>,
    ) -> Result<String>;

    /// 在同一连接上把运行中进程的输出泵成事件，直到 Exited。
    async fn stream_process_events(
        &self,
        process_id: &str,
        events: tokio::sync::mpsc::Sender<ExecStreamEvent>,
    ) -> Result<()>;

    /// 向运行中进程的 stdin 写入数据。
    async fn send_stdin(&self, process_id: &str, chunk: bytes::Bytes) -> Result<()>;

    /// 关闭运行中进程的 stdin（EOF）。
    async fn close_stdin(&self, process_id: &str) -> Result<()>;

    /// 向运行中进程的进程组投递信号。
    async fn send_signal(
        &self,
        process_id: &str,
        signal: clouisle_proto::ProcessSignal,
    ) -> Result<()>;

    /// 调整运行中进程的 PTY 尺寸。
    async fn resize_pty(&self, process_id: &str, cols: u16, rows: u16) -> Result<()>;
}

/// guest 文件系统错误的 API 侧映射：ENOENT 类错误映射 404，其余映射 VMM 错误。
fn guest_fs_error(op: &str, path: &str, message: &str) -> ClouisleError {
    let lower = message.to_ascii_lowercase();
    if lower.contains("no such file")
        || lower.contains("not found")
        || lower.contains("no such directory")
    {
        ClouisleError::not_found(format!("guest {op} {path}: {message}"))
    } else {
        ClouisleError::new(
            clouisle_core::ErrorKind::Vmm,
            format!("guest {op} {path}: {message}"),
        )
    }
}

/// Mock 连接器：测试用，仅在 test 或 test-utils feature 下编译。
/// 不真的连 vsock，直接模拟 agent 行为。
#[cfg(any(test, feature = "test-utils"))]
pub struct MockAgentConnector;

#[cfg(any(test, feature = "test-utils"))]
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

#[cfg(any(test, feature = "test-utils"))]
/// Mock 连接：用宿主机本地进程执行命令（模拟 guest exec）。
pub struct MockAgentConnection {
    sandbox_id: String,
}

#[cfg(any(test, feature = "test-utils"))]
struct MockRunningProcess {
    pid: u32,
    pty: bool,
    stdin: Arc<tokio::sync::Mutex<Option<tokio::process::ChildStdin>>>,
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
}

#[cfg(any(test, feature = "test-utils"))]
fn mock_processes()
-> &'static parking_lot::Mutex<std::collections::HashMap<String, Arc<MockRunningProcess>>> {
    use std::sync::OnceLock;
    static PROCESSES: OnceLock<
        parking_lot::Mutex<std::collections::HashMap<String, Arc<MockRunningProcess>>>,
    > = OnceLock::new();
    PROCESSES.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(any(test, feature = "test-utils"))]
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

    async fn apply_limits(&self, _pids_max: Option<u32>) -> Result<()> {
        Ok(())
    }

    async fn start_process(
        &self,
        argv: Vec<String>,
        env: std::collections::HashMap<String, String>,
        cwd: Option<String>,
        _timeout_ms: u64,
        stdin: bool,
        pty: Option<clouisle_proto::PtyConfig>,
    ) -> Result<String> {
        if argv.is_empty() {
            return Err(clouisle_core::ClouisleError::validation("argv empty"));
        }
        let id = uuid::Uuid::now_v7().to_string();
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]).envs(env);
        if stdin {
            cmd.stdin(std::process::Stdio::piped());
        } else {
            cmd.stdin(std::process::Stdio::null());
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| {
            clouisle_core::ClouisleError::new(
                clouisle_core::ErrorKind::Vmm,
                format!("spawn {argv:?}: {e}"),
            )
        })?;
        let pid = child.id().unwrap_or(0);
        let stdin_handle = child
            .stdin
            .take()
            .map(|stdin| Arc::new(tokio::sync::Mutex::new(Some(stdin))));
        mock_processes().lock().insert(
            id.clone(),
            Arc::new(MockRunningProcess {
                pid,
                pty: pty.is_some(),
                stdin: stdin_handle.unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(None))),
                child: tokio::sync::Mutex::new(Some(child)),
            }),
        );
        Ok(id)
    }

    async fn stream_process_events(
        &self,
        process_id: &str,
        events: tokio::sync::mpsc::Sender<ExecStreamEvent>,
    ) -> Result<()> {
        let process = mock_processes()
            .lock()
            .get(process_id)
            .cloned()
            .ok_or_else(|| ClouisleError::not_found(format!("process {process_id}")))?;
        let mut child = process
            .child
            .lock()
            .await
            .take()
            .ok_or_else(|| ClouisleError::invalid_state("process already reaped"))?;
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");
        let id_out = process_id.to_string();
        let timeout_ms = 0u64;
        let _ = timeout_ms;
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let events_out = events.clone();
            let events_err = events.clone();
            let stdout_task = tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                loop {
                    match stdout.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if events_out
                                .send(ExecStreamEvent::Stdout(bytes::Bytes::copy_from_slice(
                                    &buf[..n],
                                )))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            });
            let stderr_task = tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                loop {
                    match stderr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if events_err
                                .send(ExecStreamEvent::Stderr(bytes::Bytes::copy_from_slice(
                                    &buf[..n],
                                )))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            });
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            let code = child
                .wait()
                .await
                .map(|status| status.code().unwrap_or(-1))
                .unwrap_or(-1);
            mock_processes().lock().remove(&id_out);
            let _ = events.send(ExecStreamEvent::Exit(code)).await;
        });
        Ok(())
    }

    async fn send_stdin(&self, process_id: &str, chunk: bytes::Bytes) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let process = mock_processes()
            .lock()
            .get(process_id)
            .cloned()
            .ok_or_else(|| ClouisleError::not_found(format!("process {process_id}")))?;
        let mut guard = process.stdin.lock().await;
        let Some(writer) = guard.as_mut() else {
            return Err(ClouisleError::invalid_state("stdin is closed"));
        };
        writer
            .write_all(&chunk)
            .await
            .map_err(|e| ClouisleError::io(format!("mock stdin write: {e}")))
    }

    async fn close_stdin(&self, process_id: &str) -> Result<()> {
        let process = mock_processes()
            .lock()
            .get(process_id)
            .cloned()
            .ok_or_else(|| ClouisleError::not_found(format!("process {process_id}")))?;
        *process.stdin.lock().await = None;
        Ok(())
    }

    async fn send_signal(
        &self,
        process_id: &str,
        signal: clouisle_proto::ProcessSignal,
    ) -> Result<()> {
        let process = mock_processes()
            .lock()
            .get(process_id)
            .cloned()
            .ok_or_else(|| ClouisleError::not_found(format!("process {process_id}")))?;
        #[cfg(unix)]
        unsafe {
            libc::kill(-(process.pid as i32), signal.as_i32());
        }
        Ok(())
    }

    async fn resize_pty(&self, process_id: &str, _cols: u16, _rows: u16) -> Result<()> {
        let process = mock_processes()
            .lock()
            .get(process_id)
            .cloned()
            .ok_or_else(|| ClouisleError::not_found(format!("process {process_id}")))?;
        if !process.pty {
            return Err(ClouisleError::invalid_state(format!(
                "process {process_id} has no pty"
            )));
        }
        Ok(())
    }

    async fn write_file(&self, path: &str, content: bytes::Bytes, mode: u32) -> Result<()> {
        // Mock 模式：写入宿主机临时文件，模拟 guest 文件写入
        let dir = std::env::temp_dir()
            .join("clouisle-mock-fs")
            .join(&self.sandbox_id);
        std::fs::create_dir_all(&dir).ok(); // 路径穿越防护
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
        std::fs::create_dir_all(&dir)
            .map_err(|e| ClouisleError::io(format!("mkdir sandbox: {e}")))?;
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

#[cfg(any(test, feature = "test-utils"))]
// 独立的文件系统后端（Mock 用宿主机本地文件系统模拟 guest 文件系统）。
// 每个沙盒映射到 `<tmpdir>/clouisle-mock-fs/<sandbox_id>/`。
#[derive(Debug, Clone, Default)]
pub struct MockFsBackend;

#[cfg(any(test, feature = "test-utils"))]
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

#[cfg(any(test, feature = "test-utils"))]
/// 线程安全的文件系统连接（每个 sandbox 一个实例）。
pub struct MockFsConnection {
    sandbox_id: String,
}

#[cfg(any(test, feature = "test-utils"))]
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

    async fn start_process(
        &self,
        _argv: Vec<String>,
        _env: std::collections::HashMap<String, String>,
        _cwd: Option<String>,
        _timeout_ms: u64,
        _stdin: bool,
        _pty: Option<clouisle_proto::PtyConfig>,
    ) -> Result<String> {
        Err(ClouisleError::invalid_state(
            "process streaming not supported on fs-only connection",
        ))
    }

    async fn stream_process_events(
        &self,
        _process_id: &str,
        _events: tokio::sync::mpsc::Sender<ExecStreamEvent>,
    ) -> Result<()> {
        Err(ClouisleError::invalid_state(
            "process streaming not supported on fs-only connection",
        ))
    }

    async fn send_stdin(&self, _process_id: &str, _chunk: bytes::Bytes) -> Result<()> {
        Err(ClouisleError::invalid_state(
            "process control not supported on fs-only connection",
        ))
    }

    async fn close_stdin(&self, _process_id: &str) -> Result<()> {
        Err(ClouisleError::invalid_state(
            "process control not supported on fs-only connection",
        ))
    }

    async fn send_signal(
        &self,
        _process_id: &str,
        _signal: clouisle_proto::ProcessSignal,
    ) -> Result<()> {
        Err(ClouisleError::invalid_state(
            "process control not supported on fs-only connection",
        ))
    }

    async fn resize_pty(&self, _process_id: &str, _cols: u16, _rows: u16) -> Result<()> {
        Err(ClouisleError::invalid_state(
            "process control not supported on fs-only connection",
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

    async fn apply_limits(&self, _pids_max: Option<u32>) -> Result<()> {
        Ok(())
    }
}

/// guest agent 监听的 vsock 端口（与 clouisle-agent 约定）。
#[cfg(target_os = "linux")]
pub const AGENT_PORT: u32 = 5201;

/// 真实 vsock 连接器（Linux）：通过 AF_VSOCK 连 guest CID:5201，完成 Hello 握手。
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct VsockAgentConnector {
    /// 连接与握手超时
    pub connect_timeout: std::time::Duration,
}

#[cfg(target_os = "linux")]
impl Default for VsockAgentConnector {
    fn default() -> Self {
        Self {
            connect_timeout: std::time::Duration::from_secs(10),
        }
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl AgentConnector for VsockAgentConnector {
    async fn connect_and_hello(
        &self,
        handle: &VmHandle,
        sandbox_id: &str,
    ) -> Result<Box<dyn AgentConnection>> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};

        // 通过 guest 的 TAP 网络 IP (每沙盒独立网段 10.{a}.{b}.2) 进行 TCP 通信。
        // 不使用 vsock（需要 guest 内核驱动），TCP 隧道经 veth pair 跨 netns。
        // 快照预热继承路径用显式子网；否则按 sandbox_id 派生。
        let guest_ip = match handle.subnet {
            Some((a, b)) => format!("10.{a}.{b}.2"),
            None => clouisle_net::netns::guest_ip(sandbox_id),
        };
        let guest_addr = format!("{guest_ip}:5201")
            .parse::<std::net::SocketAddr>()
            .map_err(|e| ClouisleError::io(format!("invalid guest addr {guest_ip}:5201: {e}")))?;
        let deadline = tokio::time::Instant::now() + self.connect_timeout;
        loop {
            let last_err = match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                tokio::net::TcpStream::connect(guest_addr),
            )
            .await
            {
                Ok(Ok(stream)) => {
                    let conn = VsockFrameConnection {
                        sandbox_id: sandbox_id.to_string(),
                        stream: tokio::sync::Mutex::new(tokio::io::BufStream::new(stream)),
                    };

                    write_frame(
                        &mut *conn.stream.lock().await,
                        &Frame::Hello {
                            agent_version: env!("CARGO_PKG_VERSION").to_string(),
                        },
                    )
                    .await
                    .map_err(|e| ClouisleError::io(format!("write Hello to guest: {e}")))?;

                    let resp = read_frame(&mut *conn.stream.lock().await)
                        .await
                        .map_err(|e| {
                            ClouisleError::io(format!("read Hello response from guest: {e}"))
                        })?;
                    if !matches!(resp, Frame::Hello { .. }) {
                        return Err(ClouisleError::invalid_state(
                            "expected Hello from guest, got unexpected frame",
                        ));
                    }
                    return Ok(Box::new(conn));
                }
                Ok(Err(e)) => e.to_string(),
                Err(_) => "connect timed out".into(),
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(ClouisleError::new(
                    clouisle_core::ErrorKind::Vmm,
                    format!("guest TCP connect to {guest_ip}:5201 failed: {last_err}"),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }
}

/// Docker 开发后端连接器：经 `clouisle-dev-mgmt` 网络 TCP 连接容器名:5201，
/// 复用帧协议与 Hello 握手。容器名即 handle.id（DockerDevVmm 设置）。
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct DockerDevAgentConnector {
    pub connect_timeout: std::time::Duration,
}

#[cfg(target_os = "linux")]
impl Default for DockerDevAgentConnector {
    fn default() -> Self {
        Self {
            connect_timeout: std::time::Duration::from_secs(15),
        }
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl AgentConnector for DockerDevAgentConnector {
    async fn connect_and_hello(
        &self,
        handle: &VmHandle,
        sandbox_id: &str,
    ) -> Result<Box<dyn AgentConnection>> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};

        // mgmt 网络内 Docker DNS 解析容器名（ToSocketAddrs 支持 hostname）。
        use std::net::ToSocketAddrs;
        let mut addrs = (handle.id.as_str(), 5201u16)
            .to_socket_addrs()
            .map_err(|e| {
                ClouisleError::io(format!("resolve dev container {}:5201: {e}", handle.id))
            })?;
        let guest_addr = addrs.next().ok_or_else(|| {
            ClouisleError::io(format!("no address for dev container {}:5201", handle.id))
        })?;
        let deadline = tokio::time::Instant::now() + self.connect_timeout;
        loop {
            let last_err = match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                tokio::net::TcpStream::connect(guest_addr),
            )
            .await
            {
                Ok(Ok(stream)) => {
                    let conn = VsockFrameConnection {
                        sandbox_id: sandbox_id.to_string(),
                        stream: tokio::sync::Mutex::new(tokio::io::BufStream::new(stream)),
                    };
                    write_frame(
                        &mut *conn.stream.lock().await,
                        &Frame::Hello {
                            agent_version: env!("CARGO_PKG_VERSION").to_string(),
                        },
                    )
                    .await
                    .map_err(|e| ClouisleError::io(format!("write Hello to dev container: {e}")))?;
                    let resp = read_frame(&mut *conn.stream.lock().await)
                        .await
                        .map_err(|e| {
                            ClouisleError::io(format!("read Hello from dev container: {e}"))
                        })?;
                    if !matches!(resp, Frame::Hello { .. }) {
                        return Err(ClouisleError::invalid_state(
                            "expected Hello from dev container agent",
                        ));
                    }
                    return Ok(Box::new(conn));
                }
                Ok(Err(e)) => e,
                Err(_) => {
                    return Err(ClouisleError::timeout(format!(
                        "docker-dev agent hello timeout after {}s",
                        self.connect_timeout.as_secs()
                    )));
                }
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(ClouisleError::timeout(format!(
                    "docker-dev agent hello timeout after {}s",
                    self.connect_timeout.as_secs()
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let _ = last_err;
        }
    }
}

/// 真实 guest 连接：通过 TCP 与 guest agent 通信（在 TAP 网络 10.0.0.2:5201）。
/// 绕过 vsock 内核驱动依赖，使用 TAP/veth 对进行 TCP 隧道。
#[cfg(target_os = "linux")]
pub struct VsockFrameConnection {
    sandbox_id: String,
    stream: tokio::sync::Mutex<tokio::io::BufStream<tokio::net::TcpStream>>,
}

#[cfg(target_os = "linux")]
#[async_trait]
impl AgentConnection for VsockFrameConnection {
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
        let (tx, mut rx) = tokio::sync::mpsc::channel(4_096);
        let started = std::time::Instant::now();
        self.exec_stream(argv, env, cwd, timeout_ms, tx).await?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = -1;
        while let Some(event) = rx.recv().await {
            match event {
                ExecStreamEvent::Stdout(chunk) => stdout.extend_from_slice(&chunk),
                ExecStreamEvent::Stderr(chunk) => stderr.extend_from_slice(&chunk),
                ExecStreamEvent::Exit(code) => exit_code = code,
                ExecStreamEvent::Error(message) => {
                    return Err(ClouisleError::new(ErrorKind::Vmm, message));
                }
            }
        }
        Ok(clouisle_core::execution::ExecutionResult {
            exit_code,
            stdout: bytes::Bytes::from(stdout),
            stderr: bytes::Bytes::from(stderr),
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }

    async fn exec_stream(
        &self,
        argv: Vec<String>,
        env: std::collections::HashMap<String, String>,
        cwd: Option<String>,
        timeout_ms: u64,
        events: tokio::sync::mpsc::Sender<ExecStreamEvent>,
    ) -> Result<()> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};
        let id = uuid::Uuid::now_v7().to_string();
        write_frame(
            &mut *self.stream.lock().await,
            &Frame::ExecReq {
                id: id.clone(),
                argv,
                env,
                cwd,
                timeout_ms,
            },
        )
        .await
        .map_err(|error| ClouisleError::io(format!("send streamed ExecReq: {error}")))?;
        loop {
            let frame = read_frame(&mut *self.stream.lock().await)
                .await
                .map_err(|error| {
                    ClouisleError::io(format!("read streamed exec response: {error}"))
                })?;
            let event = match frame {
                Frame::Stdout {
                    id: frame_id,
                    chunk,
                } if frame_id == id => ExecStreamEvent::Stdout(chunk),
                Frame::Stderr {
                    id: frame_id,
                    chunk,
                } if frame_id == id => ExecStreamEvent::Stderr(chunk),
                Frame::Exited { id: frame_id, code } if frame_id == id => {
                    let _ = events.send(ExecStreamEvent::Exit(code)).await;
                    return Ok(());
                }
                Frame::Error { message, .. } => {
                    return Err(ClouisleError::new(ErrorKind::Vmm, message));
                }
                _ => {
                    return Err(ClouisleError::invalid_state(
                        "unexpected streamed guest frame",
                    ));
                }
            };
            if events.send(event).await.is_err() {
                return Ok(());
            }
        }
    }

    async fn write_file(&self, path: &str, content: bytes::Bytes, mode: u32) -> Result<()> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};

        write_frame(
            &mut *self.stream.lock().await,
            &Frame::WriteFile {
                path: path.to_string(),
                mode,
                content,
            },
        )
        .await
        .map_err(|e| ClouisleError::io(format!("send WriteFile {path}: {e}")))?;

        match read_frame(&mut *self.stream.lock().await)
            .await
            .map_err(|e| ClouisleError::io(format!("read WriteFile response {path}: {e}")))?
        {
            Frame::WriteFileResult { .. } => Ok(()),
            Frame::Error { message, .. } => Err(ClouisleError::io(format!(
                "guest write_file {path}: {message}"
            ))),
            other => Err(ClouisleError::invalid_state(format!(
                "unexpected frame for WriteFile: {other:?}"
            ))),
        }
    }

    async fn read_file(&self, path: &str) -> Result<bytes::Bytes> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};

        write_frame(
            &mut *self.stream.lock().await,
            &Frame::ReadFile {
                path: path.to_string(),
                offset: 0,
                length: u64::MAX,
            },
        )
        .await
        .map_err(|e| ClouisleError::io(format!("send ReadFile {path}: {e}")))?;

        let resp = read_frame(&mut *self.stream.lock().await)
            .await
            .map_err(|e| ClouisleError::io(format!("read ReadFile response: {e}")))?;
        match resp {
            Frame::ReadFileResult { content, .. } => Ok(content),
            Frame::Error { message, .. } => Err(guest_fs_error("read_file", path, &message)),
            other => Err(ClouisleError::invalid_state(format!(
                "unexpected frame for ReadFile: {other:?}"
            ))),
        }
    }

    async fn list_dir(&self, path: &str) -> Result<Vec<clouisle_core::DirEntry>> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};

        write_frame(
            &mut *self.stream.lock().await,
            &Frame::ListDir {
                path: path.to_string(),
            },
        )
        .await
        .map_err(|e| ClouisleError::io(format!("send ListDir {path}: {e}")))?;

        let resp = read_frame(&mut *self.stream.lock().await)
            .await
            .map_err(|e| ClouisleError::io(format!("read ListDir response: {e}")))?;
        match resp {
            Frame::ListDirResult { entries } => Ok(entries
                .into_iter()
                .map(|e| clouisle_core::DirEntry {
                    name: e.name,
                    size: e.size,
                    mode: e.mode,
                    mtime: e.mtime,
                    is_dir: e.is_dir,
                })
                .collect()),
            Frame::Error { message, .. } => Err(guest_fs_error("list_dir", path, &message)),
            other => Err(ClouisleError::invalid_state(format!(
                "unexpected frame for ListDir: {other:?}"
            ))),
        }
    }

    async fn ping(&self) -> Result<()> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};

        write_frame(&mut *self.stream.lock().await, &Frame::Ping)
            .await
            .map_err(|e| ClouisleError::io(format!("send Ping: {e}")))?;
        let resp = read_frame(&mut *self.stream.lock().await)
            .await
            .map_err(|e| ClouisleError::io(format!("read Pong: {e}")))?;
        match resp {
            Frame::Pong => Ok(()),
            Frame::Error { message, .. } => Err(ClouisleError::new(
                clouisle_core::ErrorKind::Vmm,
                format!("guest ping: {message}"),
            )),
            other => Err(ClouisleError::invalid_state(format!(
                "unexpected frame for Ping: {other:?}"
            ))),
        }
    }

    async fn apply_limits(&self, pids_max: Option<u32>) -> Result<()> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};

        write_frame(
            &mut *self.stream.lock().await,
            &Frame::ApplyLimits {
                pids_max,
                bandwidth_mbps: None,
            },
        )
        .await
        .map_err(|e| ClouisleError::io(format!("send ApplyLimits: {e}")))?;
        let resp = read_frame(&mut *self.stream.lock().await)
            .await
            .map_err(|e| ClouisleError::io(format!("read ApplyLimits response: {e}")))?;
        match resp {
            Frame::ControlOk => Ok(()),
            Frame::Error { message, .. } => Err(ClouisleError::new(
                clouisle_core::ErrorKind::Vmm,
                format!("guest apply_limits: {message}"),
            )),
            other => Err(ClouisleError::invalid_state(format!(
                "unexpected frame for ApplyLimits: {other:?}"
            ))),
        }
    }

    async fn start_process(
        &self,
        argv: Vec<String>,
        env: std::collections::HashMap<String, String>,
        cwd: Option<String>,
        timeout_ms: u64,
        stdin: bool,
        pty: Option<clouisle_proto::PtyConfig>,
    ) -> Result<String> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};
        let id = uuid::Uuid::now_v7().to_string();
        write_frame(
            &mut *self.stream.lock().await,
            &Frame::ProcessStart {
                id: id.clone(),
                argv,
                env,
                cwd,
                timeout_ms,
                stdin,
                pty,
            },
        )
        .await
        .map_err(|error| ClouisleError::io(format!("send ProcessStart: {error}")))?;
        let started = read_frame(&mut *self.stream.lock().await)
            .await
            .map_err(|error| ClouisleError::io(format!("read ProcessStarted: {error}")))?;
        match started {
            Frame::ProcessStarted { id: got, pid: _pid } if got == id => Ok(id),
            Frame::Error { message, .. } => Err(ClouisleError::new(ErrorKind::Vmm, message)),
            other => Err(ClouisleError::invalid_state(format!(
                "unexpected frame for ProcessStart: {other:?}"
            ))),
        }
    }

    async fn stream_process_events(
        &self,
        process_id: &str,
        events: tokio::sync::mpsc::Sender<ExecStreamEvent>,
    ) -> Result<()> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::read_frame;
        loop {
            let frame = read_frame(&mut *self.stream.lock().await)
                .await
                .map_err(|error| {
                    ClouisleError::io(format!("read interactive process output: {error}"))
                })?;
            match frame {
                Frame::Stdout {
                    id: frame_id,
                    chunk,
                } if frame_id == process_id => {
                    if events.send(ExecStreamEvent::Stdout(chunk)).await.is_err() {
                        return Err(ClouisleError::invalid_state("process stream closed"));
                    }
                }
                Frame::Stderr {
                    id: frame_id,
                    chunk,
                } if frame_id == process_id => {
                    if events.send(ExecStreamEvent::Stderr(chunk)).await.is_err() {
                        return Err(ClouisleError::invalid_state("process stream closed"));
                    }
                }
                Frame::Exited { id: frame_id, code } if frame_id == process_id => {
                    let _ = events.send(ExecStreamEvent::Exit(code)).await;
                    return Ok(());
                }
                Frame::Error { message, .. } => {
                    return Err(ClouisleError::new(ErrorKind::Vmm, message));
                }
                other => {
                    return Err(ClouisleError::invalid_state(format!(
                        "unexpected interactive frame: {other:?}"
                    )));
                }
            }
        }
    }

    async fn send_stdin(&self, process_id: &str, chunk: bytes::Bytes) -> Result<()> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};
        write_frame(
            &mut *self.stream.lock().await,
            &Frame::Stdin {
                id: process_id.to_string(),
                chunk,
            },
        )
        .await
        .map_err(|e| ClouisleError::io(format!("send Stdin: {e}")))?;
        match read_frame(&mut *self.stream.lock().await)
            .await
            .map_err(|e| ClouisleError::io(format!("read Stdin ack: {e}")))?
        {
            Frame::ControlOk => Ok(()),
            Frame::Error { message, .. } => Err(ClouisleError::new(
                ErrorKind::Vmm,
                format!("guest stdin: {message}"),
            )),
            other => Err(ClouisleError::invalid_state(format!(
                "unexpected frame for Stdin: {other:?}"
            ))),
        }
    }

    async fn close_stdin(&self, process_id: &str) -> Result<()> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};
        write_frame(
            &mut *self.stream.lock().await,
            &Frame::StdinEof {
                id: process_id.to_string(),
            },
        )
        .await
        .map_err(|e| ClouisleError::io(format!("send StdinEof: {e}")))?;
        match read_frame(&mut *self.stream.lock().await)
            .await
            .map_err(|e| ClouisleError::io(format!("read StdinEof ack: {e}")))?
        {
            Frame::ControlOk => Ok(()),
            Frame::Error { message, .. } => Err(ClouisleError::new(
                ErrorKind::Vmm,
                format!("guest stdin eof: {message}"),
            )),
            other => Err(ClouisleError::invalid_state(format!(
                "unexpected frame for StdinEof: {other:?}"
            ))),
        }
    }

    async fn send_signal(
        &self,
        process_id: &str,
        signal: clouisle_proto::ProcessSignal,
    ) -> Result<()> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};
        write_frame(
            &mut *self.stream.lock().await,
            &Frame::Signal {
                id: process_id.to_string(),
                signal,
            },
        )
        .await
        .map_err(|e| ClouisleError::io(format!("send Signal: {e}")))?;
        match read_frame(&mut *self.stream.lock().await)
            .await
            .map_err(|e| ClouisleError::io(format!("read Signal ack: {e}")))?
        {
            Frame::ControlOk => Ok(()),
            Frame::Error { message, .. } => Err(ClouisleError::new(
                ErrorKind::Vmm,
                format!("guest signal: {message}"),
            )),
            other => Err(ClouisleError::invalid_state(format!(
                "unexpected frame for Signal: {other:?}"
            ))),
        }
    }

    async fn resize_pty(&self, process_id: &str, cols: u16, rows: u16) -> Result<()> {
        use clouisle_proto::Frame;
        use clouisle_proto::codec::{read_frame, write_frame};
        write_frame(
            &mut *self.stream.lock().await,
            &Frame::Resize {
                id: process_id.to_string(),
                cols,
                rows,
            },
        )
        .await
        .map_err(|e| ClouisleError::io(format!("send Resize: {e}")))?;
        match read_frame(&mut *self.stream.lock().await)
            .await
            .map_err(|e| ClouisleError::io(format!("read Resize ack: {e}")))?
        {
            Frame::ControlOk => Ok(()),
            Frame::Error { message, .. } => Err(ClouisleError::new(
                ErrorKind::Vmm,
                format!("guest resize: {message}"),
            )),
            other => Err(ClouisleError::invalid_state(format!(
                "unexpected frame for Resize: {other:?}"
            ))),
        }
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
            owner_id: None,
            pid: None,
            api_socket: None,
            vsock_socket: None,
            vsock_cid: None,
            subnet: None,
        };
        let c = conn.connect_and_hello(&handle, "test-sbx").await.unwrap();
        assert!(c.ping().await.is_ok());
    }
}
