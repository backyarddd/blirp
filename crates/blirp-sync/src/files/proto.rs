//! `blirp/files/1` messages and the chunked blob encoding.
//!
//! Every bidirectional stream starts with an [`Open`] frame. `Hello` opens
//! the control stream (strict request/response of [`Req`] and [`Reply`]);
//! `PutBlob` and `GetBlob` open one stream per blob transfer. The hub sends
//! [`Notify`] frames on one unidirectional stream whenever a root changes.
//!
//! Blob bytes follow a header frame as chunks: a 4-byte big-endian length,
//! then one zstd frame of at most [`CHUNK`] raw bytes; a zero length ends
//! the blob. Raw offsets make both directions resumable.

use crate::wire::WireError;
use blirp_core::files::{ChangeResult, FileChange, FilesMode, GitManifest, IndexEntry, RootInfo};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Raw bytes per chunk.
pub const CHUNK: usize = 1 << 20;
/// Largest compressed chunk accepted (zstd never grows 1 MiB this much).
pub const MAX_ZCHUNK: usize = CHUNK + (64 << 10);
/// Entries per index page.
pub const INDEX_PAGE: usize = 5000;
/// Bytes per index page (paths may be long).
pub const INDEX_PAGE_BYTES: usize = 4 << 20;
/// Changes per commit batch.
pub const MAX_CHANGES: usize = 1000;
/// Hashes per `have` request.
pub const MAX_HAVE: usize = 5000;
/// Blob transfers a node runs at once.
pub const STREAMS: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Open {
    Hello { versions: Vec<u32> },
    PutBlob { hash: String, len: u64 },
    GetBlob { hash: String, offset: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Req {
    Roots,
    Index {
        root_id: String,
        after: i64,
    },
    Have {
        hashes: Vec<String>,
    },
    Commit {
        root_id: String,
        /// Origin only: its folder, registering the root.
        root_path: Option<String>,
        manifest: Option<GitManifest>,
        changes: Vec<FileChange>,
    },
    DeleteRoot {
        root_id: String,
    },
    SetMode {
        project_id: String,
        mode: FilesMode,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingBlob {
    pub hash: String,
    /// Bytes the hub already has (resume from here).
    pub have: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Welcome {
        version: u32,
        quota: u64,
        used: u64,
        project_modes: HashMap<String, FilesMode>,
    },
    RootList {
        roots: Vec<RootInfo>,
        project_modes: HashMap<String, FilesMode>,
    },
    IndexPage {
        entries: Vec<IndexEntry>,
        head: i64,
        more: bool,
        /// The root's incarnation: pages of another one do not combine.
        incarnation: String,
    },
    Missing {
        blobs: Vec<MissingBlob>,
    },
    CommitResult {
        results: Vec<ChangeResult>,
        head: i64,
        incarnation: String,
    },
    /// Blob streams: the hub continues an upload at `offset`.
    Ready {
        offset: u64,
    },
    /// Blob streams: header of a download (raw length).
    Blob {
        len: u64,
    },
    Done,
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notify {
    pub root_id: String,
    /// -1: the root was deleted.
    pub head: i64,
}

/// Write one chunk (compressed) of `raw`; an empty slice ends the blob.
/// Returns the bytes put on the wire.
pub async fn write_chunk<W: AsyncWrite + Unpin>(w: &mut W, raw: &[u8]) -> Result<usize, WireError> {
    if raw.is_empty() {
        w.write_all(&0u32.to_be_bytes()).await?;
        w.flush().await?;
        return Ok(4);
    }
    let z = zstd::bulk::compress(raw, super::blobs::ZSTD_LEVEL)?;
    let len = u32::try_from(z.len()).map_err(|_| WireError::TooLarge {
        size: z.len(),
        limit: MAX_ZCHUNK,
    })?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&z).await?;
    Ok(z.len() + 4)
}

/// Read one chunk; `None` at the end of the blob.
pub async fn read_chunk<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>, WireError> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 {
        return Ok(None);
    }
    if n > MAX_ZCHUNK {
        return Err(WireError::TooLarge {
            size: n,
            limit: MAX_ZCHUNK,
        });
    }
    let mut z = vec![0u8; n];
    r.read_exact(&mut z).await?;
    // Bounded: a chunk never expands past CHUNK (no decompression bombs).
    let raw = zstd::bulk::decompress(&z, CHUNK)?;
    Ok(Some(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn chunks_roundtrip_and_bombs_are_refused() {
        let (mut a, mut b) = tokio::io::duplex(1 << 22);
        let data = vec![7u8; CHUNK];
        write_chunk(&mut a, &data).await.unwrap();
        write_chunk(&mut a, b"tail").await.unwrap();
        write_chunk(&mut a, &[]).await.unwrap();
        assert_eq!(read_chunk(&mut b).await.unwrap().unwrap(), data);
        assert_eq!(read_chunk(&mut b).await.unwrap().unwrap(), b"tail");
        assert!(read_chunk(&mut b).await.unwrap().is_none());

        // A frame that inflates past CHUNK is an error, not an allocation.
        let bomb = zstd::bulk::compress(&vec![0u8; CHUNK * 4], 3).unwrap();
        let mut framed = (bomb.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(&bomb);
        let mut r = &framed[..];
        assert!(read_chunk(&mut r).await.is_err());
        let mut huge = &((MAX_ZCHUNK as u32) + 1).to_be_bytes()[..];
        assert!(matches!(
            read_chunk(&mut huge).await,
            Err(WireError::TooLarge { .. })
        ));
    }
}
