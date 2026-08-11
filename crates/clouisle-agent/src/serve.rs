//! vsock serve 模式（Stage 0.6）：接收 host 帧，执行命令，流式返回。
//!
//! macOS 上无 AF_VSOCK，此处实现可测试的帧处理逻辑（exec 分发），
//! 真正的 vsock 绑定在 Linux（`#[cfg(target_os = "linux")]`）分支补充。

use std::collections::HashMap;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use clouisle_proto::{Frame, FrameDecoder};

#[cfg(target_os = "linux")]
use crate::errors::AgentError;
use crate::errors::AgentResult;

/// 在内存解码缓冲上处理一字节块中的完整帧，返回响应帧。
pub fn process_frames(decoder: &mut FrameDecoder, data: &[u8]) -> AgentResult<Vec<Frame>> {
    let frames = decoder.push(data)?;
    let mut responses = Vec::new();
    for frame in frames {
        responses.extend(handle_frame(frame)?);
    }
    Ok(responses)
}

/// 处理单条 host 帧，产生响应帧流。
/// 该同步辅助仅供无 socket 的单元测试使用；TCP 服务使用 `run_exec`。
pub fn handle_frame(frame: Frame) -> AgentResult<Vec<Frame>> {
    match frame {
        Frame::Ping => Ok(vec![Frame::Pong]),
        Frame::Hello { .. } => Ok(vec![Frame::Hello {
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
        }]),
        Frame::ExecReq {
            id,
            argv,
            env,
            cwd,
            timeout_ms,
        } => Ok(run_exec_sync(&id, argv, env, cwd, timeout_ms)),
        Frame::ApplyLimits { pids_max, .. } => crate::limits::apply_pids_max(pids_max)
            .map(|_| vec![Frame::ControlOk])
            .map_err(AgentError::Command),
        other => Ok(vec![Frame::Error {
            message: format!("unrecognized host frame: {other:?}"),
            code: 2,
        }]),
    }
}

/// 在本地同步执行命令，仅供无 socket 的单元测试使用。
fn run_exec_sync(
    id: &str,
    argv: Vec<String>,
    env: HashMap<String, String>,
    cwd: Option<String>,
    _timeout_ms: u64,
) -> Vec<Frame> {
    if argv.is_empty() {
        return vec![Frame::Exited {
            id: id.into(),
            code: -1,
        }];
    }
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]).envs(env);
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    match cmd.output() {
        Ok(out) => output_frames(id, out.status.code().unwrap_or(-1), out.stdout, out.stderr),
        Err(e) => vec![Frame::Error {
            message: format!("spawn failed: {e}"),
            code: 3,
        }],
    }
}

fn output_frames(id: &str, code: i32, stdout: Vec<u8>, stderr: Vec<u8>) -> Vec<Frame> {
    let mut frames = Vec::new();
    if !stdout.is_empty() {
        frames.push(Frame::Stdout {
            id: id.into(),
            chunk: Bytes::from(stdout),
        });
    }
    if !stderr.is_empty() {
        frames.push(Frame::Stderr {
            id: id.into(),
            chunk: Bytes::from(stderr),
        });
    }
    frames.push(Frame::Exited {
        id: id.into(),
        code,
    });
    frames
}

/// 在 guest 中异步执行命令，超时后杀死整个命令进程组。
async fn run_exec(
    id: &str,
    argv: Vec<String>,
    env: HashMap<String, String>,
    cwd: Option<String>,
    timeout_ms: u64,
) -> Vec<Frame> {
    use tokio::io::AsyncReadExt;

    if argv.is_empty() {
        return vec![Frame::Exited {
            id: id.into(),
            code: -1,
        }];
    }

    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .envs(env)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return vec![Frame::Error {
                message: format!("spawn failed: {e}"),
                code: 3,
            }];
        }
    };
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes).await;
        bytes
    });
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes).await;
        bytes
    });

    let code = match tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        child.wait(),
    )
    .await
    {
        Ok(Ok(status)) => status.code().unwrap_or(-1),
        Ok(Err(e)) => {
            return vec![Frame::Error {
                message: format!("wait failed: {e}"),
                code: 3,
            }];
        }
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = pid {
                // `process_group(0)` creates a group led by the child PID.
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            -1
        }
    };
    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();
    output_frames(id, code, stdout, stderr)
}

fn guest_path(path: &str) -> Result<std::path::PathBuf, String> {
    use std::path::Component;

    if path.is_empty() {
        return Err("path is required".into());
    }
    let parsed = std::path::Path::new(path);
    if parsed
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(format!("path traversal is not allowed: {path}"));
    }
    Ok(parsed.to_path_buf())
}

fn write_file(path: String, mode: u32, content: Bytes) -> Result<Frame, String> {
    let target = guest_path(&path)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create parent directory: {e}"))?;
    }
    std::fs::write(&target, content).map_err(|e| format!("write {path}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod {path}: {e}"))?;
    }
    Ok(Frame::WriteFileResult { path })
}

fn read_file(path: String, offset: u64, length: u64) -> Result<Frame, String> {
    let target = guest_path(&path)?;
    let content = std::fs::read(&target).map_err(|e| format!("read {path}: {e}"))?;
    let start = usize::try_from(offset).map_err(|_| "offset too large".to_string())?;
    if start > content.len() {
        return Err(format!("offset beyond end of file: {path}"));
    }
    let requested = usize::try_from(length).unwrap_or(usize::MAX);
    let end = start.saturating_add(requested).min(content.len());
    Ok(Frame::ReadFileResult {
        path,
        content: Bytes::copy_from_slice(&content[start..end]),
    })
}

fn list_dir(path: String) -> Result<Frame, String> {
    let target = guest_path(&path)?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&target).map_err(|e| format!("list {path}: {e}"))? {
        let entry = entry.map_err(|e| format!("read directory entry: {e}"))?;
        let metadata = entry.metadata().map_err(|e| format!("stat entry: {e}"))?;
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        #[cfg(unix)]
        let mode = metadata.permissions().mode();
        #[cfg(not(unix))]
        let mode = 0;
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or_default();
        entries.push(clouisle_proto::DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            size: metadata.len(),
            mode,
            mtime,
            is_dir: metadata.is_dir(),
        });
    }
    Ok(Frame::ListDirResult { entries })
}

/// 长生命周期进程的可寻址控制句柄（guest 全局注册表条目）。
/// 控制帧（stdin/signal/resize）可按 `id` 从任意连接路由到这里。
struct RunningProcess {
    pid: u32,
    /// 可关闭的 stdin 写端。`close_stdin` 置 None 即关闭管道 → 子进程收到 EOF。
    stdin: Option<Arc<tokio::sync::Mutex<Option<tokio::process::ChildStdin>>>>,
    pty_master: Option<Arc<OwnedFd>>,
}

type ProcessRegistry = HashMap<String, Arc<RunningProcess>>;

fn processes() -> &'static Arc<std::sync::Mutex<ProcessRegistry>> {
    static PROCESSES: OnceLock<Arc<std::sync::Mutex<ProcessRegistry>>> = OnceLock::new();
    PROCESSES.get_or_init(|| Arc::new(std::sync::Mutex::new(HashMap::new())))
}

#[cfg(unix)]
fn set_nonblocking(fd: i32) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err("fcntl F_GETFL failed".into());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err("fcntl F_SETFL O_NONBLOCK failed".into());
    }
    Ok(())
}

/// 分配 PTY，返回 (master, slave)。
#[cfg(unix)]
fn open_pty(cols: u16, rows: u16) -> Result<(OwnedFd, OwnedFd), String> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &winsize,
        )
    };
    if ret != 0 {
        return Err(format!(
            "openpty failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok((unsafe { OwnedFd::from_raw_fd(master) }, unsafe {
        OwnedFd::from_raw_fd(slave)
    }))
}

/// 启动交互式进程：可选 stdin 管道或 PTY。返回控制句柄与子进程。
#[cfg(unix)]
fn spawn_interactive(
    argv: &[String],
    env: &HashMap<String, String>,
    cwd: Option<&str>,
    stdin_open: bool,
    pty: Option<clouisle_proto::PtyConfig>,
) -> Result<(Arc<RunningProcess>, tokio::process::Child), String> {
    if argv.is_empty() {
        return Err("argv is required".into());
    }
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]).envs(env);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let (stdin_handle, pty_master) = if let Some(pty_cfg) = pty {
        let (master, slave) = open_pty(pty_cfg.cols, pty_cfg.rows)?;
        set_nonblocking(master.as_raw_fd())?;
        let slave_in = slave
            .try_clone()
            .map_err(|e| format!("clone pty slave: {e}"))?;
        let slave_out = slave
            .try_clone()
            .map_err(|e| format!("clone pty slave: {e}"))?;
        cmd.stdin(std::process::Stdio::from(slave_in))
            .stdout(std::process::Stdio::from(slave_out))
            .stderr(std::process::Stdio::from(slave));
        // 子进程成为会话组长并把 pty 设为控制终端。
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                let _ = libc::ioctl(0, libc::TIOCSCTTY as _, 0usize);
                Ok(())
            });
        }
        (None, Some(Arc::new(master)))
    } else if stdin_open {
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        cmd.process_group(0);
        (None, None)
    } else {
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        cmd.process_group(0);
        (None, None)
    };
    let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", argv[0]))?;
    let pid = child.id().unwrap_or(0);
    let stdin_handle = match stdin_handle {
        Some(handle) => Some(handle),
        None => {
            if pty.is_none() && stdin_open {
                child
                    .stdin
                    .take()
                    .map(|stdin| Arc::new(tokio::sync::Mutex::new(Some(stdin))))
            } else {
                None
            }
        }
    };
    Ok((
        Arc::new(RunningProcess {
            pid,
            stdin: stdin_handle,
            pty_master,
        }),
        child,
    ))
}

/// 把子进程输出泵成 `Stdout`/`Stderr`/`Exited` 帧，结束后注销注册表条目。
async fn pump_process<W>(
    id: String,
    handle: Arc<RunningProcess>,
    mut child: tokio::process::Child,
    writer: Arc<tokio::sync::Mutex<W>>,
) where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    if let Some(master) = handle.pty_master.clone() {
        // PTY 模式：stdout/stderr 在从机上合并，读 master 泵成 Stdout 帧。
        let master_clone = match master.try_clone() {
            Ok(clone) => clone,
            Err(_) => return,
        };
        let _ = set_nonblocking(master_clone.as_raw_fd());
        if let Ok(async_fd) = tokio::io::unix::AsyncFd::new(master_clone) {
            let mut buf = [0u8; 8192];
            loop {
                let mut guard = match async_fd.readable().await {
                    Ok(guard) => guard,
                    Err(_) => break,
                };
                let result = guard.try_io(|inner| {
                    let n = unsafe {
                        libc::read(
                            inner.as_raw_fd(),
                            buf.as_mut_ptr() as *mut libc::c_void,
                            buf.len(),
                        )
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                });
                match result {
                    Ok(Ok(0)) => break, // EOF
                    Ok(Ok(n)) => {
                        let chunk = Bytes::copy_from_slice(&buf[..n]);
                        if clouisle_proto::codec::write_frame(
                            &mut *writer.lock().await,
                            &Frame::Stdout {
                                id: id.clone(),
                                chunk,
                            },
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        guard.clear_ready();
                        continue;
                    }
                    Ok(Err(_)) | Err(_) => break,
                }
            }
        }
    } else {
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        if let Some(mut stdout) = stdout.take() {
            let id_out = id.clone();
            let writer_out = writer.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 8192];
                loop {
                    match stdout.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = Bytes::copy_from_slice(&buf[..n]);
                            if clouisle_proto::codec::write_frame(
                                &mut *writer_out.lock().await,
                                &Frame::Stdout {
                                    id: id_out.clone(),
                                    chunk,
                                },
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            });
        }
        if let Some(mut stderr) = stderr.take() {
            let id_err = id.clone();
            let writer_err = writer.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 8192];
                loop {
                    match stderr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = Bytes::copy_from_slice(&buf[..n]);
                            if clouisle_proto::codec::write_frame(
                                &mut *writer_err.lock().await,
                                &Frame::Stderr {
                                    id: id_err.clone(),
                                    chunk,
                                },
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            });
        }
    }
    let code = child
        .wait()
        .await
        .map(|status| status.code().unwrap_or(-1))
        .unwrap_or(-1);
    // 注销：stdin 被关闭，控制帧将得到 not-found。
    if let Ok(mut registry) = processes().lock() {
        registry.remove(&id);
    }
    let _ =
        clouisle_proto::codec::write_frame(&mut *writer.lock().await, &Frame::Exited { id, code })
            .await;
}

fn lookup_process(id: &str) -> Option<Arc<RunningProcess>> {
    processes().lock().ok()?.get(id).cloned()
}

async fn send_stdin(id: &str, chunk: Bytes) -> Result<(), String> {
    let handle = lookup_process(id).ok_or_else(|| format!("process {id} not found"))?;
    let Some(stdin) = handle.stdin.as_ref() else {
        return Err(format!("process {id} has no open stdin"));
    };
    use tokio::io::AsyncWriteExt;
    let mut guard = stdin.lock().await;
    let Some(writer) = guard.as_mut() else {
        return Err(format!("process {id} stdin is closed"));
    };
    writer
        .write_all(&chunk)
        .await
        .map_err(|e| format!("write stdin {id}: {e}"))
}

async fn close_stdin(id: &str) -> Result<(), String> {
    let handle = lookup_process(id).ok_or_else(|| format!("process {id} not found"))?;
    let Some(stdin) = handle.stdin.as_ref() else {
        return Ok(());
    };
    // 置 None 丢弃写端 → 管道关闭 → 子进程读到 EOF。
    *stdin.lock().await = None;
    Ok(())
}

#[cfg(unix)]
fn send_signal(id: &str, signal: clouisle_proto::ProcessSignal) -> Result<(), String> {
    let handle = lookup_process(id).ok_or_else(|| format!("process {id} not found"))?;
    let ret = unsafe { libc::kill(-(handle.pid as i32), signal.as_i32()) };
    if ret != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound {
        return Err(format!(
            "kill {}: {}",
            handle.pid,
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn resize_pty(id: &str, cols: u16, rows: u16) -> Result<(), String> {
    let handle = lookup_process(id).ok_or_else(|| format!("process {id} not found"))?;
    let Some(master) = handle.pty_master.as_ref() else {
        return Err(format!("process {id} has no pty"));
    };
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ret = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &winsize) };
    if ret != 0 {
        return Err(format!(
            "TIOCSWINSZ failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// 服务主循环：处理帧，流式返回。
pub async fn serve_loop<R, W>(reader: &mut R, writer: W) -> AgentResult<()>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let writer = Arc::new(tokio::sync::Mutex::new(writer));
    loop {
        let frame = clouisle_proto::codec::read_frame(reader).await?;
        match frame {
            Frame::ApplyLimits { pids_max, .. } => {
                let response = match crate::limits::apply_pids_max(pids_max) {
                    Ok(()) => Frame::ControlOk,
                    Err(message) => Frame::Error { message, code: 3 },
                };
                clouisle_proto::codec::write_frame(&mut *writer.lock().await, &response).await?;
            }
            Frame::ExecReq {
                id,
                argv,
                env,
                cwd,
                timeout_ms,
            } => {
                let responses = run_exec(&id, argv, env, cwd, timeout_ms).await;
                for response in responses {
                    clouisle_proto::codec::write_frame(&mut *writer.lock().await, &response)
                        .await?;
                }
            }
            Frame::ProcessStart {
                id,
                argv,
                env,
                cwd,
                timeout_ms,
                stdin,
                pty,
            } => match spawn_interactive(&argv, &env, cwd.as_deref(), stdin, pty) {
                Ok((handle, child)) => {
                    let pid = handle.pid;
                    let registered = match processes().lock() {
                        Ok(mut registry) => {
                            registry.insert(id.clone(), handle.clone());
                            true
                        }
                        Err(_) => false,
                    };
                    if !registered {
                        clouisle_proto::codec::write_frame(
                            &mut *writer.lock().await,
                            &Frame::Error {
                                message: "process registry poisoned".into(),
                                code: 3,
                            },
                        )
                        .await?;
                        continue;
                    }
                    clouisle_proto::codec::write_frame(
                        &mut *writer.lock().await,
                        &Frame::ProcessStarted {
                            id: id.clone(),
                            pid,
                        },
                    )
                    .await?;
                    if timeout_ms > 0 {
                        let timeout_pid = pid;
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)).await;
                            unsafe {
                                libc::kill(-(timeout_pid as i32), libc::SIGKILL);
                            }
                        });
                    }
                    let pump_writer = writer.clone();
                    tokio::spawn(pump_process(id, handle, child, pump_writer));
                }
                Err(message) => {
                    clouisle_proto::codec::write_frame(
                        &mut *writer.lock().await,
                        &Frame::Error { message, code: 3 },
                    )
                    .await?;
                }
            },
            Frame::Stdin { id, chunk } => {
                let response = match send_stdin(&id, chunk).await {
                    Ok(()) => Frame::ControlOk,
                    Err(message) => Frame::Error { message, code: 3 },
                };
                clouisle_proto::codec::write_frame(&mut *writer.lock().await, &response).await?;
            }
            Frame::StdinEof { id } => {
                let response = match close_stdin(&id).await {
                    Ok(()) => Frame::ControlOk,
                    Err(message) => Frame::Error { message, code: 3 },
                };
                clouisle_proto::codec::write_frame(&mut *writer.lock().await, &response).await?;
            }
            Frame::Signal { id, signal } => {
                let response = match send_signal(&id, signal) {
                    Ok(()) => Frame::ControlOk,
                    Err(message) => Frame::Error { message, code: 3 },
                };
                clouisle_proto::codec::write_frame(&mut *writer.lock().await, &response).await?;
            }
            Frame::Resize { id, cols, rows } => {
                let response = match resize_pty(&id, cols, rows) {
                    Ok(()) => Frame::ControlOk,
                    Err(message) => Frame::Error { message, code: 3 },
                };
                clouisle_proto::codec::write_frame(&mut *writer.lock().await, &response).await?;
            }
            Frame::WriteFile {
                path,
                mode,
                content,
            } => {
                let response = write_file(path, mode, content)
                    .map(|frame| vec![frame])
                    .unwrap_or_else(|message| vec![Frame::Error { message, code: 3 }]);
                for response in response {
                    clouisle_proto::codec::write_frame(&mut *writer.lock().await, &response)
                        .await?;
                }
            }
            Frame::ReadFile {
                path,
                offset,
                length,
            } => {
                let response = read_file(path, offset, length)
                    .map(|frame| vec![frame])
                    .unwrap_or_else(|message| vec![Frame::Error { message, code: 3 }]);
                for response in response {
                    clouisle_proto::codec::write_frame(&mut *writer.lock().await, &response)
                        .await?;
                }
            }
            Frame::ListDir { path } => {
                let response = list_dir(path)
                    .map(|frame| vec![frame])
                    .unwrap_or_else(|message| vec![Frame::Error { message, code: 3 }]);
                for response in response {
                    clouisle_proto::codec::write_frame(&mut *writer.lock().await, &response)
                        .await?;
                }
            }
            Frame::Ping => {
                clouisle_proto::codec::write_frame(&mut *writer.lock().await, &Frame::Pong).await?;
            }
            Frame::Hello { .. } => {
                clouisle_proto::codec::write_frame(
                    &mut *writer.lock().await,
                    &Frame::Hello {
                        agent_version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                )
                .await?;
            }
            other => {
                let response = Frame::Error {
                    message: format!("unsupported frame: {other:?}"),
                    code: 4,
                };
                clouisle_proto::codec::write_frame(&mut *writer.lock().await, &response).await?;
            }
        }
    }
}

/// guest agent 监听的端口（通过 TAP 网络 TCP 通信）。
pub const AGENT_PORT: u16 = 5201;

/// serve 模式入口：
/// - Linux：绑定 TCP 端口 5201 在所有接口，通过 TAP/veth 对接受 host 连接。
/// - 其他平台（macOS 测试）：占位返回。
#[cfg(target_os = "linux")]
pub async fn run_serve() -> AgentResult<()> {
    run_serve_with(ServeConfig::default()).await
}

/// serve 配置：`skip_network_config` 仅用于 Docker 开发容器（DockerDevVmm
/// 注入的 agent 作为容器 PID 1，无需 Firecracker 静态网络配置）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ServeConfig {
    pub skip_network_config: bool,
}

#[cfg(target_os = "linux")]
pub async fn run_serve_with(config: ServeConfig) -> AgentResult<()> {
    if !config.skip_network_config {
        crate::init::configure_network()
            .await
            .map_err(AgentError::Command)?;
    }

    let addr = format!("0.0.0.0:{AGENT_PORT}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| AgentError::Io(std::io::Error::other(e)))?;
    tracing::info!("agent listening on TCP {addr}");

    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .map_err(|e| AgentError::Io(std::io::Error::other(e)))?;
        tracing::info!("connection from {peer}");
        let (mut reader, writer) = tokio::io::split(stream);
        tokio::spawn(async move {
            if let Err(e) = serve_loop(&mut reader, writer).await {
                tracing::warn!(error = %e, "serve_loop ended");
            }
        });
    }
}

/// serve 模式入口（macOS/测试：无 AF_VSOCK，占位）。
#[cfg(not(target_os = "linux"))]
pub async fn run_serve_with(_config: ServeConfig) -> AgentResult<()> {
    // macOS/测试环境：serve 由外部 vsock 触发；此函数仅为 lib 导出占位。
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clouisle_proto::codec::{read_frame, write_frame};
    use tokio::io::duplex;

    #[test]
    fn ping_pong() {
        let frames = handle_frame(Frame::Ping).unwrap();
        assert!(matches!(frames[0], Frame::Pong));
    }

    #[test]
    fn exec_echo_frames() {
        let mut out = Vec::new();
        let mut d = FrameDecoder::new();
        let req = Frame::ExecReq {
            id: "e1".into(),
            argv: vec!["echo".into(), "hi".into()],
            env: HashMap::new(),
            cwd: None,
            timeout_ms: 5000,
        };
        process_frames(&mut d, &clouisle_proto::encode_frame(&req).unwrap())
            .unwrap()
            .into_iter()
            .for_each(|_| {});
        // 这是 process_frames 直接吞了 host->guest；改为在 serve_loop 内部带 ID 分发
        // 这里改用 handle_frame 直接测 run_exec:
        let frames = handle_frame(req).unwrap();
        for f in &frames {
            match f {
                Frame::Stdout { chunk, .. } => {
                    out.extend_from_slice(chunk);
                }
                Frame::Exited { code, .. } => assert_eq!(*code, 0),
                _ => {}
            }
        }
        assert_eq!(out, b"hi\n");
    }

    #[tokio::test]
    async fn serve_loop_exec() {
        let (mut a, b) = duplex(128);
        // 拆出独立读写半部
        let (mut br, bw) = tokio::io::split(b);
        let server = tokio::spawn(async move { serve_loop(&mut br, bw).await });
        // a 是 client 端：发 exec
        write_frame(
            &mut a,
            &Frame::ExecReq {
                id: "e1".into(),
                argv: vec!["echo".into(), "yo".into()],
                env: HashMap::new(),
                cwd: None,
                timeout_ms: 5000,
            },
        )
        .await
        .unwrap();
        let n = read_frame(&mut a).await.unwrap();
        assert!(matches!(n, Frame::Stdout { .. }));
        let e = read_frame(&mut a).await.unwrap();
        match e {
            Frame::Exited { code, .. } => assert_eq!(code, 0),
            other => panic!("expected Exited, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn serve_loop_file_roundtrip_and_traversal_rejection() {
        let path = format!("/tmp/clouisle-agent-test-{}.txt", std::process::id());
        let (mut client, server_side) = duplex(4096);
        let (mut reader, writer) = tokio::io::split(server_side);
        let server = tokio::spawn(async move { serve_loop(&mut reader, writer).await });

        write_frame(
            &mut client,
            &Frame::WriteFile {
                path: path.clone(),
                mode: 0o600,
                content: Bytes::from_static(b"hello"),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame(&mut client).await.unwrap(),
            Frame::WriteFileResult { .. }
        ));

        write_frame(
            &mut client,
            &Frame::ReadFile {
                path: path.clone(),
                offset: 1,
                length: 3,
            },
        )
        .await
        .unwrap();
        match read_frame(&mut client).await.unwrap() {
            Frame::ReadFileResult { content, .. } => assert_eq!(&content[..], b"ell"),
            other => panic!("expected ReadFileResult, got {other:?}"),
        }

        write_frame(
            &mut client,
            &Frame::WriteFile {
                path: "/tmp/../escape".into(),
                mode: 0o600,
                content: Bytes::from_static(b"no"),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame(&mut client).await.unwrap(),
            Frame::Error { .. }
        ));

        let _ = std::fs::remove_file(path);
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serve_loop_timeout_kills_process_group() {
        let (mut client, server_side) = duplex(4096);
        let (mut reader, writer) = tokio::io::split(server_side);
        let server = tokio::spawn(async move { serve_loop(&mut reader, writer).await });
        write_frame(
            &mut client,
            &Frame::ExecReq {
                id: "timeout".into(),
                argv: vec!["sh".into(), "-c".into(), "sleep 5".into()],
                env: HashMap::new(),
                cwd: None,
                timeout_ms: 30,
            },
        )
        .await
        .unwrap();
        let frame = read_frame(&mut client).await.unwrap();
        assert!(matches!(frame, Frame::Exited { code: -1, .. }));
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serve_loop_stdin_echo_and_eof() {
        let (mut client, server_side) = duplex(8192);
        let (mut reader, writer) = tokio::io::split(server_side);
        let server = tokio::spawn(async move { serve_loop(&mut reader, writer).await });
        write_frame(
            &mut client,
            &Frame::ProcessStart {
                id: "interactive-cat".into(),
                argv: vec!["cat".into()],
                env: HashMap::new(),
                cwd: None,
                timeout_ms: 0,
                stdin: true,
                pty: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame(&mut client).await.unwrap(),
            Frame::ProcessStarted {
                id,
                pid
            } if id == "interactive-cat" && pid > 0
        ));

        write_frame(
            &mut client,
            &Frame::Stdin {
                id: "interactive-cat".into(),
                chunk: Bytes::from_static(b"hello-stdin\n"),
            },
        )
        .await
        .unwrap();
        write_frame(
            &mut client,
            &Frame::StdinEof {
                id: "interactive-cat".into(),
            },
        )
        .await
        .unwrap();

        let mut echoed = Vec::new();
        let mut code = None;
        for _ in 0..6 {
            match read_frame(&mut client).await.unwrap() {
                Frame::Stdout { id, chunk } if id == "interactive-cat" => {
                    echoed.extend_from_slice(&chunk);
                }
                Frame::Exited { code: c, .. } => {
                    code = Some(c);
                    break;
                }
                Frame::ControlOk => {}
                other => panic!("unexpected frame: {other:?}"),
            }
        }
        assert_eq!(echoed, b"hello-stdin\n");
        assert_eq!(code, Some(0));
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serve_loop_signal_kills_process() {
        let (mut client, server_side) = duplex(8192);
        let (mut reader, writer) = tokio::io::split(server_side);
        let server = tokio::spawn(async move { serve_loop(&mut reader, writer).await });
        write_frame(
            &mut client,
            &Frame::ProcessStart {
                id: "interactive-sleep".into(),
                argv: vec!["sleep".into(), "60".into()],
                env: HashMap::new(),
                cwd: None,
                timeout_ms: 0,
                stdin: false,
                pty: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame(&mut client).await.unwrap(),
            Frame::ProcessStarted { .. }
        ));

        write_frame(
            &mut client,
            &Frame::Signal {
                id: "interactive-sleep".into(),
                signal: clouisle_proto::ProcessSignal::Sigkill,
            },
        )
        .await
        .unwrap();

        loop {
            match read_frame(&mut client).await.unwrap() {
                Frame::ControlOk => {}
                Frame::Exited { id, code } => {
                    assert_eq!(id, "interactive-sleep");
                    assert_eq!(code, -1);
                    break;
                }
                other => panic!("expected Exited, got {other:?}"),
            }
        }
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serve_loop_pty_merges_output() {
        let (mut client, server_side) = duplex(8192);
        let (mut reader, writer) = tokio::io::split(server_side);
        let server = tokio::spawn(async move { serve_loop(&mut reader, writer).await });
        write_frame(
            &mut client,
            &Frame::ProcessStart {
                id: "interactive-pty".into(),
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "echo pty-out; echo pty-err >&2".into(),
                ],
                env: HashMap::new(),
                cwd: None,
                timeout_ms: 0,
                stdin: true,
                pty: Some(clouisle_proto::PtyConfig { cols: 80, rows: 24 }),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame(&mut client).await.unwrap(),
            Frame::ProcessStarted { .. }
        ));

        let mut output = Vec::new();
        let mut code = None;
        for _ in 0..8 {
            match read_frame(&mut client).await.unwrap() {
                Frame::Stdout { id, chunk } if id == "interactive-pty" => {
                    output.extend_from_slice(&chunk);
                }
                Frame::Exited { code: c, .. } => {
                    code = Some(c);
                    break;
                }
                other => panic!("unexpected frame: {other:?}"),
            }
        }
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("pty-out"), "pty stdout missing: {text:?}");
        assert!(text.contains("pty-err"), "pty stderr missing: {text:?}");
        assert_eq!(code, Some(0));
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serve_loop_resize_pty_ok() {
        let (mut client, server_side) = duplex(8192);
        let (mut reader, writer) = tokio::io::split(server_side);
        let server = tokio::spawn(async move { serve_loop(&mut reader, writer).await });
        write_frame(
            &mut client,
            &Frame::ProcessStart {
                id: "interactive-resize".into(),
                argv: vec!["sleep".into(), "5".into()],
                env: HashMap::new(),
                cwd: None,
                timeout_ms: 0,
                stdin: true,
                pty: Some(clouisle_proto::PtyConfig { cols: 80, rows: 24 }),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame(&mut client).await.unwrap(),
            Frame::ProcessStarted { .. }
        ));

        write_frame(
            &mut client,
            &Frame::Resize {
                id: "interactive-resize".into(),
                cols: 132,
                rows: 43,
            },
        )
        .await
        .unwrap();
        // 无错误帧返回；随后终止进程收尾。
        write_frame(
            &mut client,
            &Frame::Signal {
                id: "interactive-resize".into(),
                signal: clouisle_proto::ProcessSignal::Sigkill,
            },
        )
        .await
        .unwrap();
        loop {
            match read_frame(&mut client).await.unwrap() {
                Frame::ControlOk => {}
                Frame::Exited { .. } => break,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
        server.abort();
    }
}
