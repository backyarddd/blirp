//! Hub side of `blirp/files/1`: one connection per node (already checked
//! with `authorized()`), any number of streams. The writer of every commit
//! is the connection's TLS-authenticated machine, never an id from a
//! message.

use super::hub::{HubError, HubFiles};
use super::proto::{
    CHUNK, INDEX_PAGE, INDEX_PAGE_BYTES, MAX_CHANGES, MAX_HAVE, MissingBlob, Notify, Open, Reply,
    Req, read_chunk, write_chunk,
};
use crate::wire::{MAX_CONTROL_FRAME, MAX_FRAME, WireError, read_frame, write_frame};
use crate::{Result, SyncError, blocking};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
/// Streams served per connection at once.
const STREAMS_PER_CONN: usize = super::proto::STREAMS * 2 + 1;

fn err(e: &HubError) -> Reply {
    if matches!(
        e,
        HubError::Store(_) | HubError::Blob(super::blobs::BlobError::Io(_))
    ) {
        tracing::warn!(error = %e, "file sync request failed on the hub");
    }
    Reply::Error {
        code: e.code().to_string(),
        message: e.to_string(),
    }
}

async fn run<T: Send + 'static>(
    f: impl FnOnce() -> std::result::Result<T, HubError> + Send + 'static,
) -> Result<std::result::Result<T, HubError>> {
    blocking(move || Ok(f())).await
}

/// Serve one node's file connection until it closes.
pub async fn serve(
    conn: Connection,
    hub: Arc<HubFiles>,
    machine_id: String,
    machine_name: String,
) -> Result<()> {
    let mut notifier: Option<tokio::task::JoinHandle<()>> = None;
    // Streams a machine may have open at once (a node uses 1 + STREAMS):
    // more wait, so one machine cannot fill the hub with partial uploads.
    let streams = Arc::new(tokio::sync::Semaphore::new(STREAMS_PER_CONN));
    let result = loop {
        let Ok(permit) = streams.clone().acquire_owned().await else {
            break Ok(());
        };
        let (send, recv) = match conn.accept_bi().await {
            Ok(s) => s,
            Err(e) => break Err(SyncError::connection(e)),
        };
        if notifier.is_none() {
            notifier = Some(spawn_notifier(&conn, &hub));
        }
        let (hub, id, name) = (hub.clone(), machine_id.clone(), machine_name.clone());
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = stream(send, recv, hub, &id, &name).await {
                tracing::debug!(node = %id, error = %e, "file stream ended");
            }
        });
    };
    if let Some(n) = notifier {
        n.abort();
    }
    result
}

/// Root changes on a unidirectional stream, so they never interleave with
/// replies.
fn spawn_notifier(conn: &Connection, hub: &Arc<HubFiles>) -> tokio::task::JoinHandle<()> {
    let conn = conn.clone();
    let mut rx = hub.subscribe();
    tokio::spawn(async move {
        let Ok(mut uni) = conn.open_uni().await else {
            return;
        };
        loop {
            let n = match rx.recv().await {
                Ok(c) => Notify {
                    root_id: c.root_id,
                    head: c.head,
                },
                // Missed changes: nodes refetch what they show on any notify.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Notify {
                    root_id: String::new(),
                    head: 0,
                },
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            if write_frame(&mut uni, &n).await.is_err() {
                return;
            }
        }
    })
}

async fn stream(
    mut send: SendStream,
    mut recv: RecvStream,
    hub: Arc<HubFiles>,
    machine_id: &str,
    machine_name: &str,
) -> Result<()> {
    let open: Open = tokio::time::timeout(OPEN_TIMEOUT, read_frame(&mut recv, MAX_CONTROL_FRAME))
        .await
        .map_err(|_| SyncError::Connection("file stream open timed out".into()))??;
    match open {
        Open::Hello { versions } => {
            let Some(version) = crate::negotiate(&versions) else {
                write_frame(
                    &mut send,
                    &Reply::Error {
                        code: "unsupported_version".into(),
                        message: format!("hub speaks {:?}", crate::PROTOCOL_VERSIONS),
                    },
                )
                .await?;
                return Ok(());
            };
            let h = hub.clone();
            let welcome = match run(move || Ok((h.quota(), h.usage()?, h.modes()?))).await? {
                Ok((quota, used, project_modes)) => Reply::Welcome {
                    version,
                    quota,
                    used,
                    project_modes,
                },
                Err(e) => err(&e),
            };
            write_frame(&mut send, &welcome).await?;
            control(send, recv, hub, machine_id, machine_name).await
        }
        Open::PutBlob { hash, len } => put_blob(send, recv, hub, hash, len).await,
        Open::GetBlob { hash, offset } => get_blob(send, hub, hash, offset).await,
    }
}

async fn control(
    mut send: SendStream,
    mut recv: RecvStream,
    hub: Arc<HubFiles>,
    machine_id: &str,
    machine_name: &str,
) -> Result<()> {
    loop {
        let req: Req = match read_frame(&mut recv, MAX_FRAME).await {
            Ok(r) => r,
            Err(WireError::Closed) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let h = hub.clone();
        let reply = match req {
            Req::Roots => match run(move || h.roots()).await? {
                Ok((roots, project_modes)) => Reply::RootList {
                    roots,
                    project_modes,
                },
                Err(e) => err(&e),
            },
            Req::Index { root_id, after } => {
                match run(move || h.index(&root_id, after, INDEX_PAGE)).await? {
                    Ok((mut entries, head)) => {
                        let full = entries.len() == INDEX_PAGE;
                        // Keep the frame well under the cap even with long paths.
                        let mut bytes = 0usize;
                        let keep = entries
                            .iter()
                            .take_while(|e| {
                                // Links carry their target: measure it all.
                                bytes +=
                                    serde_json::to_vec(e).map_or(usize::MAX / 2, |v| v.len() + 8);
                                bytes <= INDEX_PAGE_BYTES
                            })
                            .count()
                            .max(1)
                            .min(entries.len());
                        let trimmed = keep < entries.len();
                        entries.truncate(keep);
                        Reply::IndexPage {
                            entries,
                            head,
                            more: full || trimmed,
                        }
                    }
                    Err(e) => err(&e),
                }
            }
            Req::Have { hashes } => {
                if hashes.len() > MAX_HAVE {
                    return Err(SyncError::Protocol("too many hashes".into()));
                }
                match run(move || h.missing(&hashes)).await? {
                    Ok(m) => Reply::Missing {
                        blobs: m
                            .into_iter()
                            .map(|(hash, have)| MissingBlob { hash, have })
                            .collect(),
                    },
                    Err(e) => err(&e),
                }
            }
            Req::Commit {
                root_id,
                root_path,
                manifest,
                changes,
            } => {
                if changes.len() > MAX_CHANGES {
                    return Err(SyncError::Protocol("commit batch too large".into()));
                }
                let (id, name) = (machine_id.to_string(), machine_name.to_string());
                match run(move || {
                    h.commit(
                        &id,
                        &name,
                        &root_id,
                        root_path.as_deref(),
                        manifest.as_ref(),
                        &changes,
                    )
                })
                .await?
                {
                    Ok(Ok(out)) => Reply::CommitResult {
                        results: out.results,
                        head: out.head,
                    },
                    Ok(Err(refused)) => Reply::Error {
                        code: refused.code().into(),
                        message: refused.to_string(),
                    },
                    Err(e) => err(&e),
                }
            }
            Req::DeleteRoot { root_id } => match run(move || h.delete_root(&root_id)).await? {
                Ok(()) => {
                    tracing::info!(node = %machine_id, "hub copy of a project folder deleted");
                    Reply::Done
                }
                Err(e) => err(&e),
            },
            Req::SetMode { project_id, mode } => {
                match run(move || h.set_mode(&project_id, mode)).await? {
                    Ok(()) => Reply::Done,
                    Err(e) => err(&e),
                }
            }
        };
        write_frame(&mut send, &reply).await?;
    }
}

async fn put_blob(
    mut send: SendStream,
    mut recv: RecvStream,
    hub: Arc<HubFiles>,
    hash: String,
    len: u64,
) -> Result<()> {
    let (h, hs) = (hub.clone(), hash.clone());
    let mut offset = match run(move || h.begin_put(&hs, len)).await? {
        Ok(o) => o,
        Err(e) => {
            write_frame(&mut send, &err(&e)).await?;
            return Ok(());
        }
    };
    write_frame(&mut send, &Reply::Ready { offset }).await?;
    while let Some(raw) = read_chunk(&mut recv).await? {
        if offset + raw.len() as u64 > len {
            write_frame(
                &mut send,
                &Reply::Error {
                    code: "invalid_request".into(),
                    message: "more bytes than announced".into(),
                },
            )
            .await?;
            let (h, hs) = (hub.clone(), hash.clone());
            run(move || {
                h.abort_put(&hs);
                Ok(())
            })
            .await?
            .ok();
            return Ok(());
        }
        let (h, hs) = (hub.clone(), hash.clone());
        offset = match run(move || h.put_chunk(&hs, offset, &raw)).await? {
            Ok(o) => o,
            Err(e) => {
                write_frame(&mut send, &err(&e)).await?;
                return Ok(());
            }
        };
    }
    let reply = if offset == len {
        let (h, hs) = (hub.clone(), hash.clone());
        match run(move || h.finish_put(&hs, len)).await? {
            Ok(()) => Reply::Done,
            Err(e) => err(&e),
        }
    } else {
        // Ended early: the part stays for a resume.
        Reply::Ready { offset }
    };
    write_frame(&mut send, &reply).await?;
    let _ = send.finish();
    Ok(())
}

async fn get_blob(
    mut send: SendStream,
    hub: Arc<HubFiles>,
    hash: String,
    offset: u64,
) -> Result<()> {
    let h = hub.clone();
    let opened = run(move || h.open(&hash)).await?;
    let (len, mut reader) = match opened {
        Ok(Some(b)) => b,
        Ok(None) => {
            write_frame(
                &mut send,
                &Reply::Error {
                    code: "not_found".into(),
                    message: "the hub has no such blob".into(),
                },
            )
            .await?;
            return Ok(());
        }
        Err(e) => {
            write_frame(&mut send, &err(&e)).await?;
            return Ok(());
        }
    };
    if offset > len {
        write_frame(
            &mut send,
            &Reply::Error {
                code: "bad_offset".into(),
                message: "offset past the end".into(),
            },
        )
        .await?;
        return Ok(());
    }
    write_frame(&mut send, &Reply::Blob { len }).await?;
    let mut skip = offset;
    loop {
        let (r, buf) = blocking(move || {
            let mut buf = vec![0u8; CHUNK];
            let mut n = 0;
            while n < CHUNK {
                match reader.read(&mut buf[n..]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(e) => return Err(SyncError::Connection(format!("reading blob: {e}"))),
                }
            }
            buf.truncate(n);
            Ok((reader, buf))
        })
        .await?;
        reader = r;
        if buf.is_empty() {
            break;
        }
        let start = usize::try_from(skip.min(buf.len() as u64)).unwrap_or(buf.len());
        skip -= start as u64;
        if start < buf.len() {
            write_chunk(&mut send, &buf[start..]).await?;
        }
    }
    write_chunk(&mut send, &[]).await?;
    let _ = send.finish();
    Ok(())
}
