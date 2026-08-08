//! vsock serve 模式（Stage 0.6）：接收 host 帧，执行命令，流式返回。
//!
//! macOS 上无 AF_VSOCK，此处实现可测试的帧处理逻辑（exec 分发），
//! 真正的 vsock 绑定在 Linux（`#[cfg(target_os = "linux")]`）分支补充。

use std::collections::HashMap;

use bytes::Bytes;
use clouisle_proto::{Frame, FrameDecoder};

use crate::errors::AgentResult;
#[cfg(target_os = "linux")]
use crate::errors::AgentError;

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
        } => Ok(run_exec(&id, argv, env, cwd, timeout_ms)),
        other => Ok(vec![Frame::Error {
            message: format!("unrecognized host frame: {other:?}"),
            code: 2,
        }]),
    }
}

/// 在本地执行命令（CI 无 AF_VSOCK，用 std::process 模拟 guest 执行）。
fn run_exec(
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

    // 简化：spawn + wait，无 token 级 select（真实实现用 tokio + killpg）
    match cmd.output() {
        Ok(out) => {
            let mut frames = Vec::new();
            if !out.stdout.is_empty() {
                frames.push(Frame::Stdout {
                    id: id.into(),
                    chunk: Bytes::from(out.stdout),
                });
            }
            if !out.stderr.is_empty() {
                frames.push(Frame::Stderr {
                    id: id.into(),
                    chunk: Bytes::from(out.stderr),
                });
            }
            frames.push(Frame::Exited {
                id: id.into(),
                code: out.status.code().unwrap_or(-1),
            });
            frames
        }
        Err(e) => vec![Frame::Error {
            message: format!("spawn failed: {e}"),
            code: 3,
        }],
    }
}

/// 服务主循环原型（读一帧测一帧）。真实版本在 Linux 上用 vsock socket + tokio。
pub async fn serve_loop<R, W>(reader: &mut R, writer: &mut W) -> AgentResult<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        let frame = clouisle_proto::codec::read_frame(reader).await?;
        if let Frame::ExecReq {
            id,
            argv,
            env,
            cwd,
            timeout_ms,
        } = frame
        {
            // 流式写回
            for f in run_exec(&id, argv, env, cwd, timeout_ms) {
                clouisle_proto::codec::write_frame(writer, &f).await?;
            }
        } else {
            // Echo 简单帧
            match frame {
                Frame::Ping => clouisle_proto::codec::write_frame(writer, &Frame::Pong).await?,
                Frame::Hello { .. } => clouisle_proto::codec::write_frame(
                    writer,
                    &Frame::Hello {
                        agent_version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                )
                .await?,
                _ => {
                    clouisle_proto::codec::write_frame(
                        writer,
                        &Frame::Error {
                            message: "unsupported".into(),
                            code: 4,
                        },
                    )
                    .await?
                }
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
    let addr = format!("0.0.0.0:{AGENT_PORT}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| AgentError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
    tracing::info!("agent listening on TCP {addr}");

    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .map_err(|e| AgentError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
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
}
