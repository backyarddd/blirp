//! Length-prefixed JSON frames: `u32` big-endian length, then JSON bytes.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Hard cap for any frame (a replication batch is <= 4 MiB of entries).
pub const MAX_FRAME: usize = 8 << 20;
/// Cap for handshake and control frames.
pub const MAX_CONTROL_FRAME: usize = 64 << 10;

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("stream i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {size} bytes exceeds the {limit} byte limit")]
    TooLarge { size: usize, limit: usize },
    #[error("malformed frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("peer closed the stream")]
    Closed,
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    msg: &T,
) -> Result<(), WireError> {
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_FRAME {
        return Err(WireError::TooLarge {
            size: body.len(),
            limit: MAX_FRAME,
        });
    }
    // Checked above: body.len() <= MAX_FRAME < u32::MAX.
    let len = u32::try_from(body.len()).map_err(|_| WireError::TooLarge {
        size: body.len(),
        limit: MAX_FRAME,
    })?;
    let mut buf = Vec::with_capacity(body.len() + 4);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// Read one frame of at most `limit` bytes. A clean end of stream before a
/// frame starts is [`WireError::Closed`]. Not cancel safe.
pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
    limit: usize,
) -> Result<T, WireError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(WireError::Closed),
        Err(e) => return Err(e.into()),
    }
    let size = u32::from_be_bytes(len) as usize;
    if size > limit.min(MAX_FRAME) {
        return Err(WireError::TooLarge {
            size,
            limit: limit.min(MAX_FRAME),
        });
    }
    let mut body = vec![0u8; size];
    r.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn roundtrip_and_limits() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        write_frame(&mut a, &serde_json::json!({"x": 1}))
            .await
            .unwrap();
        let v: serde_json::Value = read_frame(&mut b, 100).await.unwrap();
        assert_eq!(v["x"], 1);

        write_frame(&mut a, &"y".repeat(200)).await.unwrap();
        let err = read_frame::<_, String>(&mut b, 100).await.unwrap_err();
        assert!(
            matches!(err, WireError::TooLarge { size: 202, .. }),
            "{err}"
        );

        let (a, mut b) = tokio::io::duplex(64);
        drop(a);
        let err = read_frame::<_, String>(&mut b, 100).await.unwrap_err();
        assert!(matches!(err, WireError::Closed), "{err}");
    }
}
