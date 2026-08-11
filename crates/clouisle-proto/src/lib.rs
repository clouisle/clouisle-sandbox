//! clouisle-proto: host <-> guest vsock 帧协议定义（Stage 0.6）。
//!
//! 协议：长度前缀帧（`u32 len` BE + postcard 编码的 `Frame`）。不上 gRPC，
//! 减少依赖与启动开销。帧类型见 [`Frame`]。

use std::collections::HashMap;

use bytes::Bytes;
use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};

pub mod codec;

/// 长度前缀帧头大小（u32 BE）。
pub const FRAME_HEADER_LEN: usize = 4;

/// 帧类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Frame {
    /// 连接建立后 agent 首先发送。
    Hello { agent_version: String },
    /// host 发送：执行命令。
    ExecReq {
        id: String,
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
        timeout_ms: u64,
    },
    /// host 发送：启动长生命周期进程（可选 stdin 与 PTY）。进程输出继续以
    /// `Stdout`/`Stderr`/`Exited` 帧按 `id` 寻址；控制帧（`Stdin`、`Signal`、
    /// `Resize`）可在任意连接上按同一 `id` 发送，由 guest 侧全局注册表路由。
    ProcessStart {
        id: String,
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
        timeout_ms: u64,
        /// 保持 stdin 打开（0 为关闭）。PTY 模式下恒为真。
        stdin: bool,
        /// Some 时在 guest 分配 PTY，子进程以该 PTY 为 stdin/stdout/stderr。
        pty: Option<PtyConfig>,
    },
    /// agent -> host: 进程已启动（`pid` 为 guest 侧进程号，供信号投递）。
    ProcessStarted { id: String, pid: u32 },
    /// host -> agent: 向进程 stdin 写入一块数据。
    Stdin { id: String, chunk: Bytes },
    /// host -> agent: 关闭进程 stdin（EOF）。
    StdinEof { id: String },
    /// host -> agent: 向进程进程组投递信号。
    Signal { id: String, signal: ProcessSignal },
    /// host -> agent: 调整 PTY 终端尺寸。
    Resize { id: String, cols: u16, rows: u16 },
    /// agent -> host: 控制帧（Stdin/StdinEof/Signal/Resize）成功执行。
    ControlOk,
    /// agent -> host: 标准输出块。
    Stdout { id: String, chunk: Bytes },
    /// agent -> host: 标准错误块。
    Stderr { id: String, chunk: Bytes },
    /// agent -> host: 进程退出。
    Exited { id: String, code: i32 },
    /// host 心跳探测。
    Ping,
    /// agent 回应。
    Pong,
    /// 时钟同步（host -> guest）。
    SyncTime { unix_nanos: i64 },
    /// 文件传输：写文件。
    WriteFile {
        path: String,
        mode: u32,
        content: Bytes,
    },
    /// guest -> host: 写文件结果。
    WriteFileResult { path: String },
    /// 文件传输：读文件。
    ReadFile {
        path: String,
        offset: u64,
        length: u64,
    },
    /// 文件传输：读文件响应。
    ReadFileResult { path: String, content: Bytes },
    /// 文件传输：列目录。
    ListDir { path: String },
    /// 文件传输：列目录响应。
    ListDirResult { entries: Vec<DirEntry> },
    /// 通用错误响应。
    Error { message: String, code: u32 },
    /// 密钥注入（SR-06）。
    SetSecret { name: String, value: Bytes },
    /// host -> guest: 施加资源限制（cgroup v2）。None 字段不修改。
    ApplyLimits {
        pids_max: Option<u32>,
        /// 保留：guest 内网卡限速（当前由 host netns tc 施加）。
        bandwidth_mbps: Option<u32>,
    },
}

/// 目录条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub size: u64,
    pub mode: u32,
    pub mtime: i64,
    pub is_dir: bool,
}

/// PTY 分配配置（进程启动时）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtyConfig {
    pub cols: u16,
    pub rows: u16,
}

/// 进程信号（与 POSIX 编号一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ProcessSignal {
    Sigterm = 15,
    Sigkill = 9,
    Sigint = 2,
}

impl ProcessSignal {
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

impl TryFrom<u8> for ProcessSignal {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            15 => Ok(ProcessSignal::Sigterm),
            9 => Ok(ProcessSignal::Sigkill),
            2 => Ok(ProcessSignal::Sigint),
            _ => Err(()),
        }
    }
}

/// 帧编解码错误。
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame truncated: expected {expected} bytes, got {got}")]
    FrameTruncated { expected: usize, got: usize },
    #[error("postcard decode failed: {0}")]
    Decode(String),
    #[error("io error: {0}")]
    Io(std::io::Error),
}

impl From<postcard::Error> for CodecError {
    fn from(e: postcard::Error) -> Self {
        CodecError::Decode(e.to_string())
    }
}

impl From<std::io::Error> for CodecError {
    fn from(e: std::io::Error) -> Self {
        CodecError::Io(e)
    }
}

/// 将帧编码为 `[u32 BE 长度 | postcard 内容]`。
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, CodecError> {
    let payload = postcard::to_allocvec(frame)?;
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// 从缓冲区解码出第一个完整帧，返回 `(frame, consumed)`。
///
/// 若字节不足一帧的头部或载荷，返回 `Ok(None)`（调用方继续缓冲）。
pub fn decode_frame_once(buf: &[u8]) -> Result<Option<(Frame, usize)>, CodecError> {
    if buf.len() < FRAME_HEADER_LEN {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let total = FRAME_HEADER_LEN + len;
    if buf.len() < total {
        return Ok(None);
    }
    let frame: Frame = postcard::from_bytes(&buf[FRAME_HEADER_LEN..total])?;
    Ok(Some((frame, total)))
}

/// 便捷：从 stream 逐字节累积缓冲区解码（供测试与简单场景）。
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// 推进一个字节块，返回解码出的所有完整帧。
    pub fn push(&mut self, data: &[u8]) -> Result<Vec<Frame>, CodecError> {
        self.buf.extend_from_slice(data);
        let mut frames = Vec::new();
        while let Some((frame, consumed)) = decode_frame_once(&self.buf)? {
            frames.push(frame);
            self.buf.drain(..consumed);
        }
        Ok(frames)
    }

    /// 清空当前缓冲（流结束时调用，检测半截帧）。
    pub fn finish(self) -> Result<(), CodecError> {
        if !self.buf.is_empty() {
            Err(CodecError::FrameTruncated {
                expected: FRAME_HEADER_LEN,
                got: self.buf.len(),
            })
        } else {
            Ok(())
        }
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// 供调试/测试的 Hello 便捷构造。
pub fn hello_frame(version: &str) -> Frame {
    Frame::Hello {
        agent_version: version.to_string(),
    }
}

/// 时间戳辅助。
pub fn now_unix_nanos() -> i64 {
    let now: DateTime<Utc> = Utc::now();
    now.timestamp_nanos_opt().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(f: &Frame) {
        let enc = encode_frame(f).unwrap();
        // 验证头部长度正确
        let len = u32::from_be_bytes([enc[0], enc[1], enc[2], enc[3]]) as usize;
        assert_eq!(len, enc.len() - FRAME_HEADER_LEN);
        let (dec, consumed) = decode_frame_once(&enc).unwrap().unwrap();
        assert_eq!(consumed, enc.len());
        // 比较编码后字节一致（postcard 确定性）
        assert_eq!(enc, encode_frame(&dec).unwrap());
    }

    #[test]
    fn roundtrip_all_frame_types() {
        roundtrip(&Frame::Hello {
            agent_version: "0.1.0".into(),
        });
        roundtrip(&Frame::ExecReq {
            id: "e-1".into(),
            argv: vec!["echo".into(), "hi".into()],
            env: [("FOO".into(), "bar".into())].into_iter().collect(),
            cwd: Some("/tmp".into()),
            timeout_ms: 2000,
        });
        roundtrip(&Frame::ProcessStart {
            id: "p-1".into(),
            argv: vec!["sh".into()],
            env: Default::default(),
            cwd: None,
            timeout_ms: 0,
            stdin: true,
            pty: Some(PtyConfig { cols: 80, rows: 24 }),
        });
        roundtrip(&Frame::ProcessStarted {
            id: "p-1".into(),
            pid: 42,
        });
        roundtrip(&Frame::Stdin {
            id: "p-1".into(),
            chunk: Bytes::from("data"),
        });
        roundtrip(&Frame::StdinEof { id: "p-1".into() });
        roundtrip(&Frame::Signal {
            id: "p-1".into(),
            signal: ProcessSignal::Sigterm,
        });
        roundtrip(&Frame::Resize {
            id: "p-1".into(),
            cols: 120,
            rows: 40,
        });
        roundtrip(&Frame::ControlOk);
        roundtrip(&Frame::Stdout {
            id: "e-1".into(),
            chunk: Bytes::from("hello\n"),
        });
        roundtrip(&Frame::Stderr {
            id: "e-1".into(),
            chunk: Bytes::from("err\n"),
        });
        roundtrip(&Frame::Exited {
            id: "e-1".into(),
            code: 7,
        });
        roundtrip(&Frame::Ping);
        roundtrip(&Frame::Pong);
        roundtrip(&Frame::SyncTime {
            unix_nanos: now_unix_nanos(),
        });
        roundtrip(&Frame::WriteFile {
            path: "/work/a.txt".into(),
            mode: 0o644,
            content: Bytes::from("data"),
        });
        roundtrip(&Frame::ReadFile {
            path: "/work/b.bin".into(),
            offset: 0,
            length: 100,
        });
        roundtrip(&Frame::ListDir {
            path: "/work".into(),
        });
        roundtrip(&Frame::ListDirResult {
            entries: vec![DirEntry {
                name: "x".into(),
                size: 10,
                mode: 0o644,
                mtime: 0,
                is_dir: false,
            }],
        });
        roundtrip(&Frame::Error {
            message: "not found".into(),
            code: 404,
        });
        roundtrip(&Frame::WriteFileResult {
            path: "/work/a.txt".into(),
        });
        roundtrip(&Frame::SetSecret {
            name: "key".into(),
            value: Bytes::from("secret"),
        });
    }

    #[test]
    fn frame_truncated_header() {
        let enc = encode_frame(&Frame::Ping).unwrap();
        let mut short = enc[..2].to_vec();
        match decode_frame_once(&short) {
            Ok(None) => {}
            other => panic!("expected Ok(None), got {other:?}"),
        }
        short.extend_from_slice(&enc[2..]);
        assert!(decode_frame_once(&short).unwrap().is_some());
    }

    #[test]
    fn frame_decoder_accumulates() {
        let mut d = FrameDecoder::new();
        let f1 = encode_frame(&Frame::Ping).unwrap();
        let f2 = encode_frame(&Frame::Pong).unwrap();
        let mut both = f1.clone();
        both.extend_from_slice(&f2);

        // 逐字节喂，应最终得到 2 帧
        let mut got = Vec::new();
        for b in both {
            got.extend(d.push(&[b]).unwrap());
        }
        assert!(matches!(got[0], Frame::Ping));
        assert!(matches!(got[1], Frame::Pong));
    }

    #[test]
    fn decoder_finish_detects_half_frame() {
        let mut d = FrameDecoder::new();
        let enc = encode_frame(&Frame::Ping).unwrap();
        d.push(&enc[..4]).unwrap(); // 只有头部无载荷
        assert!(d.finish().is_err());
    }

    #[test]
    fn hello_roundtrip() {
        let h = hello_frame("0.1.0");
        let enc = encode_frame(&h).unwrap();
        let (dec, _) = decode_frame_once(&enc).unwrap().unwrap();
        match dec {
            Frame::Hello { agent_version } => assert_eq!(agent_version, "0.1.0"),
            other => panic!("expected Hello, got {:?}", other),
        }
    }
}
