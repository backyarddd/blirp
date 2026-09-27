//! Node side of `blirp/files/1`, and [`FileHub`]: what the file engine
//! talks to. On a node that is the hub over the network ([`RemoteHub`]); on
//! the hub itself the same service in-process ([`LocalHub`]).

use super::blobs::BlobStore;
use super::hub::{HubError, HubFiles};
use super::proto::{
    CHUNK, INDEX_PAGE, MAX_HAVE, MissingBlob, Notify, Open, Reply, Req, STREAMS, read_chunk,
    write_chunk,
};
use crate::wire::{MAX_CONTROL_FRAME, MAX_FRAME, read_frame, write_frame};
use crate::{Result, SyncError, blocking};
use blirp_core::files::{ChangeResult, FileChange, FilesMode, GitManifest, IndexEntry, RootInfo};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{Mutex, Semaphore};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Code of a hub that does not speak `blirp/files/1` yet.
pub const HUB_OUTDATED: &str = "hub_outdated";

/// Opens a connection to the hub with the files ALPN.
pub type Connector =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Connection>> + Send>> + Send + Sync>;
/// Called for every change notification from the hub.
pub type NotifyHook = Arc<dyn Fn(Notify) + Send + Sync>;

fn remote(code: &str, message: impl Into<String>) -> SyncError {
    SyncError::Remote {
        code: code.to_string(),
        message: message.into(),
    }
}

fn hub_err(e: HubError) -> SyncError {
    remote(e.code(), e.to_string())
}

/// Upload bandwidth limit shared by all transfers (`files.upload_kbps`).
#[derive(Debug)]
pub struct Rate {
    bytes_per_sec: AtomicU64,
    state: Mutex<(f64, Instant)>,
}

impl Rate {
    pub fn new(kbps: u32) -> Self {
        Self {
            bytes_per_sec: AtomicU64::new(u64::from(kbps) * 125),
            state: Mutex::new((0.0, Instant::now())),
        }
    }

    pub fn set(&self, kbps: u32) {
        self.bytes_per_sec
            .store(u64::from(kbps) * 125, Ordering::Relaxed);
    }

    /// Account for `n` bytes, sleeping while the budget is overdrawn.
    pub async fn take(&self, n: usize) {
        let bps = self.bytes_per_sec.load(Ordering::Relaxed);
        if bps == 0 {
            return;
        }
        let wait = {
            let mut s = self.state.lock().await;
            let now = Instant::now();
            // Up to one second of burst.
            s.0 = (s.0 + now.duration_since(s.1).as_secs_f64() * bps as f64).min(bps as f64);
            s.1 = now;
            s.0 -= n as f64;
            (s.0 < 0.0).then(|| Duration::from_secs_f64(-s.0 / bps as f64))
        };
        if let Some(w) = wait {
            tokio::time::sleep(w).await;
        }
    }
}

/// What the hub said when a connection opened.
#[derive(Debug, Clone, Default)]
pub struct Welcome {
    pub quota: u64,
    pub used: u64,
    /// Largest file the hub accepts (0: not known).
    pub max_file: u64,
    pub project_modes: HashMap<String, FilesMode>,
}

struct Control {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    notifier: tokio::task::JoinHandle<()>,
    welcome: Welcome,
}

impl Drop for Control {
    fn drop(&mut self) {
        self.notifier.abort();
    }
}

/// The hub over the network.
pub struct RemoteHub {
    connect: Connector,
    control: Mutex<Option<Control>>,
    streams: Semaphore,
    rate: Rate,
    parts: BlobStore,
    on_notify: NotifyHook,
    /// The hub's file size limit from the last connection (0: none yet).
    max_file: std::sync::atomic::AtomicU64,
}

impl RemoteHub {
    /// `parts` holds partial downloads.
    pub fn new(
        connect: Connector,
        parts: BlobStore,
        upload_kbps: u32,
        on_notify: NotifyHook,
    ) -> Self {
        Self {
            connect,
            control: Mutex::new(None),
            streams: Semaphore::new(STREAMS),
            rate: Rate::new(upload_kbps),
            parts,
            on_notify,
            max_file: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn rate(&self) -> &Rate {
        &self.rate
    }

    async fn open(&self) -> Result<Control> {
        let conn = (self.connect)().await.map_err(|e| match e {
            // A hub without blirp/files/1 fails the TLS handshake with
            // "no application protocol" (alert 120).
            SyncError::Connect { message, .. }
                if {
                    let m = message.to_ascii_lowercase();
                    m.contains("application protocol")
                        || m.contains("alpn")
                        || m.contains("error 120")
                } =>
            {
                remote(HUB_OUTDATED, "update the hub to sync project files")
            }
            other => other,
        })?;
        let (mut send, mut recv) = conn.open_bi().await.map_err(SyncError::connection)?;
        write_frame(
            &mut send,
            &Open::Hello {
                versions: crate::PROTOCOL_VERSIONS.to_vec(),
            },
        )
        .await?;
        let welcome = match timed(read_frame(&mut recv, MAX_FRAME)).await? {
            Reply::Welcome {
                quota,
                used,
                max_file,
                project_modes,
                ..
            } => {
                self.max_file
                    .store(max_file, std::sync::atomic::Ordering::Relaxed);
                Welcome {
                    quota,
                    used,
                    max_file,
                    project_modes,
                }
            }
            Reply::Error { code, message } => return Err(remote(&code, message)),
            other => return Err(SyncError::Protocol(format!("unexpected {other:?}"))),
        };
        let uni_conn = conn.clone();
        let hook = self.on_notify.clone();
        let notifier = tokio::spawn(async move {
            let Ok(mut uni) = uni_conn.accept_uni().await else {
                return;
            };
            while let Ok(n) = read_frame::<_, Notify>(&mut uni, MAX_CONTROL_FRAME).await {
                hook(n);
            }
        });
        Ok(Control {
            conn,
            send,
            recv,
            notifier,
            welcome,
        })
    }

    /// The live connection, opening one (with its control stream) if needed.
    async fn connection(&self) -> Result<Connection> {
        let mut c = self.control.lock().await;
        if let Some(ctl) = c.as_ref().filter(|ctl| ctl.conn.close_reason().is_none()) {
            return Ok(ctl.conn.clone());
        }
        let ctl = self.open().await?;
        let conn = ctl.conn.clone();
        *c = Some(ctl);
        Ok(conn)
    }

    /// One request on the control stream. Any failure drops the connection
    /// so the next call starts fresh.
    async fn request(&self, req: &Req) -> Result<Reply> {
        let mut c = self.control.lock().await;
        if c.as_ref()
            .is_none_or(|ctl| ctl.conn.close_reason().is_some())
        {
            *c = Some(self.open().await?);
        }
        let Some(ctl) = c.as_mut() else {
            return Err(SyncError::Unavailable("not connected".into()));
        };
        let out = async {
            write_frame(&mut ctl.send, req).await?;
            timed(read_frame(&mut ctl.recv, MAX_FRAME)).await
        }
        .await;
        match out {
            Ok(Reply::Error { code, message }) => Err(remote(&code, message)),
            Ok(r) => Ok(r),
            Err(e) => {
                *c = None;
                Err(e)
            }
        }
    }

    pub async fn welcome(&self) -> Result<Welcome> {
        self.connection().await?;
        let c = self.control.lock().await;
        Ok(c.as_ref().map(|c| c.welcome.clone()).unwrap_or_default())
    }

    async fn upload(&self, hash: &str, src: &Path, len: u64) -> Result<()> {
        let _permit = self
            .streams
            .acquire()
            .await
            .map_err(|e| SyncError::Unavailable(e.to_string()))?;
        let conn = self.connection().await?;
        let (mut send, mut recv) = conn.open_bi().await.map_err(SyncError::connection)?;
        write_frame(
            &mut send,
            &Open::PutBlob {
                hash: hash.to_string(),
                len,
            },
        )
        .await?;
        let offset = match timed(read_frame(&mut recv, MAX_CONTROL_FRAME)).await? {
            Reply::Ready { offset } => offset,
            Reply::Error { code, message } => return Err(remote(&code, message)),
            other => return Err(SyncError::Protocol(format!("unexpected {other:?}"))),
        };
        let mut f = tokio::fs::File::open(src)
            .await
            .map_err(|e| SyncError::Unavailable(format!("reading a file to upload: {e}")))?;
        f.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| SyncError::Unavailable(format!("reading a file to upload: {e}")))?;
        let mut sent = offset;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let mut n = 0;
            while n < CHUNK {
                let k = f.read(&mut buf[n..]).await.map_err(|e| {
                    SyncError::Unavailable(format!("reading a file to upload: {e}"))
                })?;
                if k == 0 {
                    break;
                }
                n += k;
            }
            if n == 0 {
                break;
            }
            sent += n as u64;
            if sent > len {
                // Grew since it was hashed; the next pass sends the new one.
                return Err(remote("file_changed", "the file changed while uploading"));
            }
            let wire = write_chunk(&mut send, &buf[..n]).await?;
            self.rate.take(wire).await;
        }
        write_chunk(&mut send, &[]).await?;
        let _ = send.finish();
        match timed(read_frame(&mut recv, MAX_CONTROL_FRAME)).await? {
            Reply::Done => Ok(()),
            Reply::Ready { .. } => Err(remote("file_changed", "the file changed while uploading")),
            Reply::Error { code, message } => Err(remote(&code, message)),
            other => Err(SyncError::Protocol(format!("unexpected {other:?}"))),
        }
    }

    async fn download(&self, hash: &str) -> Result<PathBuf> {
        let part = self
            .parts
            .part_path(hash)
            .map_err(|e| SyncError::Protocol(e.to_string()))?;
        let _part = self.parts.lock_part(hash).await;
        let _permit = self
            .streams
            .acquire()
            .await
            .map_err(|e| SyncError::Unavailable(e.to_string()))?;
        let mut have = self.parts.part_len(hash);
        let conn = self.connection().await?;
        let (mut send, mut recv) = conn.open_bi().await.map_err(SyncError::connection)?;
        write_frame(
            &mut send,
            &Open::GetBlob {
                hash: hash.to_string(),
                offset: have,
            },
        )
        .await?;
        let _ = send.finish();
        let len = match timed(read_frame(&mut recv, MAX_CONTROL_FRAME)).await? {
            Reply::Blob { len } => len,
            Reply::Error { code, message } if code == "bad_offset" => {
                self.parts.discard_part(hash);
                return Err(remote(&code, message));
            }
            Reply::Error { code, message } => return Err(remote(&code, message)),
            other => return Err(SyncError::Protocol(format!("unexpected {other:?}"))),
        };
        // The part exists from here on, so empty content (no chunk) verifies.
        let (parts, h) = (self.parts.clone(), hash.to_string());
        blocking(move || {
            parts
                .start_part(&h)
                .map_err(|e| SyncError::Unavailable(e.to_string()))
        })
        .await?;
        while let Some(raw) = read_chunk(&mut recv).await? {
            if have + raw.len() as u64 > len {
                self.parts.discard_part(hash);
                return Err(SyncError::Protocol("blob longer than announced".into()));
            }
            let (parts, h) = (self.parts.clone(), hash.to_string());
            have = blocking(move || {
                parts
                    .append(&h, have, &raw)
                    .map_err(|e| SyncError::Unavailable(e.to_string()))
            })
            .await?;
        }
        if have != len {
            return Err(SyncError::Connection("blob download ended early".into()));
        }
        verify_part(&self.parts, hash, part, len).await
    }
}

/// Check a finished download against its hash and hand it over as a file
/// of the caller's own; a bad one is dropped. Runs under the part's lock.
async fn verify_part(parts: &BlobStore, hash: &str, part: PathBuf, len: u64) -> Result<PathBuf> {
    let (p, h, store) = (part, hash.to_string(), parts.clone());
    let taken = blocking(move || {
        let ok = blirp_core::files::scan::hash_file(&p)
            .is_ok_and(|(got, size, _)| got == h && size == len);
        if !ok {
            return Ok(None);
        }
        store
            .take_part(&h)
            .map(Some)
            .map_err(|e| SyncError::Unavailable(e.to_string()))
    })
    .await?;
    if let Some(own) = taken {
        Ok(own)
    } else {
        parts.discard_part(hash);
        Err(remote(
            "hash_mismatch",
            "downloaded content does not match its hash",
        ))
    }
}

async fn timed<T>(f: impl Future<Output = Result<T, crate::wire::WireError>>) -> Result<T> {
    tokio::time::timeout(REQUEST_TIMEOUT, f)
        .await
        .map_err(|_| SyncError::Connection("the hub did not answer in time".into()))?
        .map_err(Into::into)
}

/// The hub's own engine: the file service in-process.
pub struct LocalHub {
    pub hub: Arc<HubFiles>,
    pub machine_id: String,
    pub machine_name: String,
    pub parts: BlobStore,
}

/// Result of a commit: per-change results and the root's new head, or the
/// hub's refusal of the whole batch (`SyncError::Remote` with its code).
#[derive(Debug, Clone)]
pub struct Committed {
    pub results: Vec<ChangeResult>,
    pub head: i64,
    pub incarnation: String,
}

/// A root's index as the file engine reads it.
#[derive(Debug, Clone, Default)]
pub struct RootIndex {
    pub entries: Vec<IndexEntry>,
    pub head: i64,
    pub incarnation: String,
}

/// The hub as the file engine sees it.
#[derive(Clone)]
pub enum FileHub {
    Local(Arc<LocalHub>),
    Remote(Arc<RemoteHub>),
}

impl FileHub {
    /// Remove this machine's partial downloads (and downloaded files left
    /// unused by a crash) not touched for a day. Blocking.
    pub fn sweep_downloads(&self) -> usize {
        let parts = match self {
            Self::Remote(r) => &r.parts,
            Self::Local(l) => &l.parts,
        };
        parts.sweep_parts(super::hub::PART_AGE)
    }

    /// The hub's file size limit as last heard (0: not known yet). Never
    /// connects: safe to ask while holding locks.
    pub fn max_file(&self) -> u64 {
        match self {
            Self::Remote(r) => r.max_file.load(std::sync::atomic::Ordering::Relaxed),
            Self::Local(l) => l.hub.max_file(),
        }
    }

    pub async fn welcome(&self) -> Result<Welcome> {
        match self {
            Self::Remote(r) => r.welcome().await,
            Self::Local(l) => {
                let h = l.hub.clone();
                blocking(move || {
                    Ok(Welcome {
                        quota: h.quota(),
                        used: h.usage().map_err(hub_err)?,
                        max_file: h.max_file(),
                        project_modes: h.modes().map_err(hub_err)?,
                    })
                })
                .await
            }
        }
    }

    pub async fn roots(&self) -> Result<(Vec<RootInfo>, HashMap<String, FilesMode>)> {
        match self {
            Self::Remote(r) => match r.request(&Req::Roots).await? {
                Reply::RootList {
                    roots,
                    project_modes,
                } => Ok((roots, project_modes)),
                other => Err(SyncError::Protocol(format!("unexpected {other:?}"))),
            },
            Self::Local(l) => {
                let h = l.hub.clone();
                blocking(move || h.roots().map_err(hub_err)).await
            }
        }
    }

    /// Every entry of `root` after `after` (one per path, its newest
    /// version, even when a path changed between pages), the head and the
    /// root's incarnation.
    pub async fn index(&self, root: &str, after: i64) -> Result<RootIndex> {
        let mut by_path: HashMap<String, IndexEntry> = HashMap::new();
        let mut from = after;
        let mut head = 0;
        let mut incarnation: Option<String> = None;
        loop {
            let (page, page_head, more, inc) = match self {
                Self::Remote(r) => match r
                    .request(&Req::Index {
                        root_id: root.to_string(),
                        after: from,
                    })
                    .await?
                {
                    Reply::IndexPage {
                        entries,
                        head,
                        more,
                        incarnation,
                    } => (entries, head, more, incarnation),
                    other => return Err(SyncError::Protocol(format!("unexpected {other:?}"))),
                },
                Self::Local(l) => {
                    let (h, id) = (l.hub.clone(), root.to_string());
                    let slice =
                        blocking(move || h.index(&id, from, INDEX_PAGE).map_err(hub_err)).await?;
                    let more = slice.entries.len() == INDEX_PAGE;
                    (slice.entries, slice.head, more, slice.incarnation)
                }
            };
            match &incarnation {
                Some(i) if *i != inc => {
                    return Err(remote("root_replaced", "the hub copy was made again"));
                }
                _ => incarnation = Some(inc),
            }
            head = head.max(page_head);
            let last = page.last().map(|e| e.version);
            for e in page {
                match by_path.get(&e.path) {
                    Some(old) if old.version >= e.version => {}
                    _ => {
                        by_path.insert(e.path.clone(), e);
                    }
                }
            }
            match last {
                Some(v) if more && v > from => from = v,
                _ => {
                    let mut entries: Vec<IndexEntry> = by_path.into_values().collect();
                    entries.sort_by_key(|e| e.version);
                    return Ok(RootIndex {
                        entries,
                        head,
                        incarnation: incarnation.unwrap_or_default(),
                    });
                }
            }
        }
    }

    /// Of `hashes`, those the hub lacks (with the bytes it has of each).
    pub async fn missing(&self, hashes: &[String]) -> Result<Vec<MissingBlob>> {
        let mut out = Vec::new();
        for batch in hashes.chunks(MAX_HAVE) {
            match self {
                Self::Remote(r) => match r
                    .request(&Req::Have {
                        hashes: batch.to_vec(),
                    })
                    .await?
                {
                    Reply::Missing { blobs } => out.extend(blobs),
                    other => return Err(SyncError::Protocol(format!("unexpected {other:?}"))),
                },
                Self::Local(l) => {
                    let (h, b) = (l.hub.clone(), batch.to_vec());
                    let m = blocking(move || h.missing(&b).map_err(hub_err)).await?;
                    out.extend(m.into_iter().map(|(hash, have)| MissingBlob { hash, have }));
                }
            }
        }
        Ok(out)
    }

    /// Upload the content of `src` (`len` bytes) as `hash`. The hub
    /// verifies it; a file that changed since it was hashed fails.
    pub async fn upload(&self, hash: &str, src: &Path, len: u64) -> Result<()> {
        match self {
            Self::Remote(r) => r.upload(hash, src, len).await,
            Self::Local(l) => {
                let (h, hash, src) = (l.hub.clone(), hash.to_string(), src.to_path_buf());
                blocking(move || h.import(&hash, &src, len).map_err(hub_err)).await
            }
        }
    }

    /// Download `hash` into a verified local file (a partial download
    /// resumes). The caller removes it once used.
    pub async fn download(&self, hash: &str) -> Result<PathBuf> {
        match self {
            Self::Remote(r) => r.download(hash).await,
            Self::Local(l) => {
                let (h, parts, id) = (l.hub.clone(), l.parts.clone(), hash.to_string());
                let part = parts
                    .part_path(hash)
                    .map_err(|e| SyncError::Protocol(e.to_string()))?;
                let _part = l.parts.lock_part(hash).await;
                let (p, len) = blocking(move || {
                    let (len, mut reader) = h
                        .open(&id)
                        .map_err(hub_err)?
                        .ok_or_else(|| remote("not_found", "the hub has no such blob"))?;
                    if let Some(parent) = part.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| SyncError::Unavailable(e.to_string()))?;
                    }
                    let mut out =
                        blirp_core::files::write::retry_busy(|| std::fs::File::create(&part))
                            .map_err(|e| SyncError::Unavailable(e.to_string()))?;
                    std::io::copy(&mut reader, &mut out)
                        .map_err(|e| SyncError::Unavailable(e.to_string()))?;
                    Ok((part, len))
                })
                .await?;
                verify_part(&l.parts, hash, p, len).await
            }
        }
    }

    /// Commit a batch. The writer is this machine (the connection's
    /// identity on the hub).
    pub async fn commit(
        &self,
        root_id: &str,
        root_path: Option<&str>,
        incarnation: Option<&str>,
        manifest: Option<&GitManifest>,
        changes: Vec<FileChange>,
    ) -> Result<Committed> {
        match self {
            Self::Remote(r) => match r
                .request(&Req::Commit {
                    root_id: root_id.to_string(),
                    root_path: root_path.map(str::to_string),
                    incarnation: incarnation.map(str::to_string),
                    manifest: manifest.cloned(),
                    changes,
                })
                .await?
            {
                Reply::CommitResult {
                    results,
                    head,
                    incarnation,
                } => Ok(Committed {
                    results,
                    head,
                    incarnation,
                }),
                other => Err(SyncError::Protocol(format!("unexpected {other:?}"))),
            },
            Self::Local(l) => {
                let l = l.clone();
                let (root, path, inc, manifest) = (
                    root_id.to_string(),
                    root_path.map(str::to_string),
                    incarnation.map(str::to_string),
                    manifest.cloned(),
                );
                blocking(move || {
                    match l
                        .hub
                        .commit(
                            &l.machine_id,
                            &l.machine_name,
                            &root,
                            path.as_deref(),
                            inc.as_deref(),
                            manifest.as_ref(),
                            &changes,
                        )
                        .map_err(hub_err)?
                    {
                        Ok(o) => Ok(Committed {
                            results: o.results,
                            head: o.head,
                            incarnation: o.incarnation,
                        }),
                        Err(refused) => Err(remote(refused.code(), refused.to_string())),
                    }
                })
                .await
            }
        }
    }

    pub async fn delete_root(&self, root_id: &str) -> Result<()> {
        match self {
            Self::Remote(r) => r
                .request(&Req::DeleteRoot {
                    root_id: root_id.to_string(),
                })
                .await
                .map(|_| ()),
            Self::Local(l) => {
                let (h, id) = (l.hub.clone(), root_id.to_string());
                blocking(move || h.delete_root(&id).map_err(hub_err)).await
            }
        }
    }

    pub async fn set_mode(&self, project_id: &str, mode: FilesMode) -> Result<()> {
        match self {
            Self::Remote(r) => r
                .request(&Req::SetMode {
                    project_id: project_id.to_string(),
                    mode,
                })
                .await
                .map(|_| ()),
            Self::Local(l) => {
                let (h, id) = (l.hub.clone(), project_id.to_string());
                blocking(move || h.set_mode(&id, mode).map_err(hub_err)).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rate_limits_on_average() {
        let r = Rate::new(8); // 1000 bytes/s
        let t = Instant::now();
        r.take(1000).await;
        r.take(500).await;
        assert!(
            t.elapsed() >= Duration::from_millis(1400),
            "{:?}",
            t.elapsed()
        );
        let free = Rate::new(0);
        let t = Instant::now();
        free.take(usize::MAX).await;
        assert!(t.elapsed() < Duration::from_millis(100));
    }
}
