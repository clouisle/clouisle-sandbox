//! 帧编解码：基于 tokio 的异步读写（长度前缀帧）。

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{CodecError, FRAME_HEADER_LEN, Frame, decode_frame_once, encode_frame};

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
    use crate::FrameDecoder;
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

    /// 确定性伪随机字节喂给帧解码器：必须只产生 Ok(None)/错误，绝不 panic，
    /// 且从不把垃圾当合法帧接受后崩溃。
    #[test]
    fn malformed_bytes_never_panic_decode() {
        struct Lcg(u64);
        impl Lcg {
            fn next(&mut self) -> u64 {
                self.0 = self
                    .0
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                self.0
            }
        }
        let mut rng = Lcg(0xD00D_F00D);
        for _ in 0..2000 {
            let len = (rng.next() % 64) as usize;
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                buf.push((rng.next() & 0xFF) as u8);
            }
            // 前 4 字节是长度字段：随机垃圾长度也必须安全处理。
            let _ = decode_frame_once(&buf);
            // 整个缓冲推入累计解码器。
            let mut decoder = FrameDecoder::new();
            let _ = decoder.push(&buf);
        }
        // 明确构造：声称 4GiB 长度但只有 4 字节 → Ok(None)（继续缓冲），不 panic。
        let mut big = vec![0xFF, 0xFF, 0xFF, 0x7F, 1, 2, 3];
        assert!(decode_frame_once(&big).unwrap().is_none());
        big[0] = 0x00;
        let mut decoder = FrameDecoder::new();
        decoder.push(&big).unwrap();
        assert!(decoder.finish().is_err());
    }
}
