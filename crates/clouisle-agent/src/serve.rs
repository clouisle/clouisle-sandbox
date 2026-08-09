//! vsock serve 模式（Stage 0.6）：接收 host 帧，执行命令，流式返回。
//!
//! macOS 上无 AF_VSOCK，此处实现可测试的帧处理逻辑（exec 分发），
//! 真正的 vsock 绑定在 Linux（`#[cfg(target_os = "linux")]`）分支补充。

use std::collections::HashMap;

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

/// 服务主循环：每次请求都产生确定性的响应帧。
pub async fn serve_loop<R, W>(reader: &mut R, writer: &mut W) -> AgentResult<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        let frame = clouisle_proto::codec::read_frame(reader).await?;
        let responses = match frame {
            Frame::ExecReq {
                id,
                argv,
                env,
                cwd,
                timeout_ms,
            } => run_exec(&id, argv, env, cwd, timeout_ms).await,
            Frame::WriteFile {
                path,
                mode,
                content,
            } => write_file(path, mode, content)
                .map(|frame| vec![frame])
                .unwrap_or_else(|message| vec![Frame::Error { message, code: 3 }]),
            Frame::ReadFile {
                path,
                offset,
                length,
            } => read_file(path, offset, length)
                .map(|frame| vec![frame])
                .unwrap_or_else(|message| vec![Frame::Error { message, code: 3 }]),
            Frame::ListDir { path } => list_dir(path)
                .map(|frame| vec![frame])
                .unwrap_or_else(|message| vec![Frame::Error { message, code: 3 }]),
            Frame::Ping => vec![Frame::Pong],
            Frame::Hello { .. } => vec![Frame::Hello {
                agent_version: env!("CARGO_PKG_VERSION").to_string(),
            }],
            other => vec![Frame::Error {
                message: format!("unsupported frame: {other:?}"),
                code: 4,
            }],
        };
        for response in responses {
            clouisle_proto::codec::write_frame(writer, &response).await?;
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
    crate::init::configure_network()
        .await
        .map_err(AgentError::Command)?;

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
        let (mut reader, mut writer) = tokio::io::split(stream);
        tokio::spawn(async move {
            if let Err(e) = serve_loop(&mut reader, &mut writer).await {
                tracing::warn!(error = %e, "serve_loop ended");
            }
        });
    }
}

/// serve 模式入口（macOS/测试：无 AF_VSOCK，占位）。
#[cfg(not(target_os = "linux"))]
pub async fn run_serve() -> AgentResult<()> {
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
        let (mut br, mut bw) = tokio::io::split(b);
        let server = tokio::spawn(async move { serve_loop(&mut br, &mut bw).await });
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
        let (mut reader, mut writer) = tokio::io::split(server_side);
        let server = tokio::spawn(async move { serve_loop(&mut reader, &mut writer).await });

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
        let (mut reader, mut writer) = tokio::io::split(server_side);
        let server = tokio::spawn(async move { serve_loop(&mut reader, &mut writer).await });
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
}
