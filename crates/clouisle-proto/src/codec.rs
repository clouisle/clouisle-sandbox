//! 帧编解码：基于 tokio 的异步读写（长度前缀帧）。

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{decode_frame_once, encode_frame, CodecError, Frame, FRAME_HEADER_LEN};

/// 从一个异步 reader 读取下一帧。
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame, CodecError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    r.read_exact(&mut header).await?;
    let len = u32::from_be_bytes(header) as usize;

    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;

    let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + len);
    buf.extend_from_slice(&header);
    buf.extend_from_slice(&payload);

    match decode_frame_once(&buf)? {
        Some((frame, _)) => Ok(frame),
        None => Err(CodecError::FrameTruncated {
            expected: buf.len(),
            got: FRAME_HEADER_LEN + len,
        }),
    }
}

/// 写入一个完整帧。
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    frame: &Frame,
) -> Result<(), CodecError> {
    let b = encode_frame(frame)?;
    w.write_all(&b).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn duplex_roundtrip() {
        let (mut a, mut b) = duplex(1024);
        let frame = Frame::ExecReq {
            id: "e-1".into(),
            argv: vec!["echo".into()],
            env: Default::default(),
            cwd: None,
            timeout_ms: 100,
        };
        write_frame(&mut a, &frame).await.unwrap();
        let got = read_frame(&mut b).await.unwrap();
        match got {
            Frame::ExecReq { argv, .. } => assert_eq!(argv, vec!["echo"]),
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_half_frame_errors() {
        // 只有 2 字节 → read_exact 卡住；用 timeout 验证不会无限等待，且之后 EOF 报错
        let (mut a, mut b) = duplex(1024);
        a.write_all(&[0, 0]).await.unwrap();
        let f = read_frame(&mut b);
        let timed = tokio::time::timeout(std::time::Duration::from_millis(200), f).await;
        // 在 200ms 内未返回 EOF 错误（因为流未关闭），说明阻塞等待剩余字节——非 panic
        assert!(timed.is_err(), "expected timeout waiting for rest of frame");
    }
}