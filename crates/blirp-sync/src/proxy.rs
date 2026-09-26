//! Remote proxy transport (§10, `blirp/proxy/1`).
//!
//! Every node keeps one proxy connection to its hub; either side opens
//! bidirectional streams on it. A stream starts with an `open` frame naming
//! the target machine and whether the requester may control terminals,
//! answered by a `reply` frame. After an ok reply the stream carries plain
//! HTTP/1.1 (including WebSocket upgrades) to the target daemon's own API
//! router. The hub serves streams addressed to itself and relays the rest
//! byte for byte to the target node's proxy connection, capping `control`
//! by what it knows about the requester.

use crate::wire::{MAX_CONTROL_FRAME, read_frame, write_frame};
use crate::{Result, SyncError};
use iroh::endpoint::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};

/// A proxied byte stream (read half, write half).
pub type ProxyStream = tokio::io::Join<RecvStream, SendStream>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyOpen {
    pub version: u32,
    /// Machine id whose API serves the stream.
    pub target: String,
    /// The requester may send terminal input and start/stop sessions.
    pub control: bool,
    /// Human-readable requester, for logs.
    pub via: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyReply {
    pub ok: bool,
    pub code: Option<String>,
    pub message: Option<String>,
}

impl ProxyReply {
    pub fn ok() -> Self {
        Self {
            ok: true,
            code: None,
            message: None,
        }
    }

    pub fn err(code: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            code: Some(code.into()),
            message: Some(message.into()),
        }
    }
}

/// Who a served proxy stream acts for, as established by the transport.
#[derive(Debug, Clone)]
pub struct ProxyPrincipal {
    pub control: bool,
    pub via: String,
}

/// Send `open` on a fresh stream and wait for the reply.
pub(crate) async fn request(
    mut send: SendStream,
    mut recv: RecvStream,
    open: &ProxyOpen,
) -> Result<ProxyStream> {
    write_frame(&mut send, open).await?;
    let reply: ProxyReply = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        read_frame(&mut recv, MAX_CONTROL_FRAME),
    )
    .await
    .map_err(|_| SyncError::Connection("proxy peer did not answer".into()))??;
    if !reply.ok {
        return Err(SyncError::Remote {
            code: reply.code.unwrap_or_else(|| "proxy_failed".into()),
            message: reply.message.unwrap_or_default(),
        });
    }
    Ok(tokio::io::join(recv, send))
}
