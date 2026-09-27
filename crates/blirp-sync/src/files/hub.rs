//! The hub's file service: roots, index, blobs, compare-and-set commits,
//! quota and garbage collection. The `blirp/files/1` server serves nodes
//! with it; the hub's own file engine calls it in-process. Every method
//! blocks (SQLite and disk); async callers use the blocking pool.

use super::blobs::{BlobError, BlobReader, BlobStore};
use blirp_core::files::{FileChange, FilesMode, GitManifest, RootInfo, is_hash, is_root_id};
use blirp_core::store::{CommitInput, CommitOutcome, CommitRefused, IndexSlice, Store, StoreError};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::broadcast;

/// Tombstones are kept this long (design §4).
pub const TOMBSTONE_MS: i64 = 90 * 24 * 3600 * 1000;
/// Blobs younger than this are never collected: a commit using them may be
/// on its way.
pub const BLOB_GRACE_MS: i64 = 3600 * 1000;
/// Partial uploads untouched this long are dropped.
pub const PART_AGE: Duration = Duration::from_secs(24 * 3600);
/// Largest upload accepted until the daemon sets `files.max_file_mb`.
const DEFAULT_MAX_FILE: u64 = 50 << 20;

/// Stored bytes as the database has them (0 when it cannot be read; the
/// counter then only grows from what this process stores).
fn store_usage(store: &Store) -> i64 {
    store.hub_blob_usage().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "reading project file storage use failed");
        0
    })
}

/// History rows dropped per step when the quota is tight.
const QUOTA_PRUNE_STEP: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("the hub's storage for project files is full")]
    Quota,
    #[error("{0}")]
    Blob(#[from] BlobError),
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Invalid(String),
    #[error("the file is larger than the hub accepts")]
    TooLarge,
    #[error("the hub has no such root")]
    UnknownRoot,
    #[error("turn file sync off for this project before deleting its hub copy")]
    NotPaused,
}

impl HubError {
    /// Stable code for the wire and the API.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Quota => "hub_quota",
            Self::Blob(BlobError::Mismatch) => "hash_mismatch",
            Self::Blob(BlobError::Offset { .. }) => "bad_offset",
            Self::Blob(BlobError::BadHash) | Self::Invalid(_) => "invalid_request",
            Self::Blob(BlobError::Io(_)) | Self::Store(_) => "internal",
            Self::TooLarge => "too_large",
            Self::UnknownRoot => "unknown_root",
            Self::NotPaused => "files_on",
        }
    }
}

/// A root changed: its new head (-1 when it was deleted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootChanged {
    pub root_id: String,
    pub head: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcStats {
    pub history: usize,
    pub tombstones: usize,
    pub blobs: usize,
}

pub struct HubFiles {
    store: Arc<Store>,
    blobs: BlobStore,
    quota: AtomicU64,
    keep_ms: AtomicI64,
    changed: broadcast::Sender<RootChanged>,
    /// Serializes blob files and their rows (store, collect): a blob being
    /// stored is never removed by a collection running at the same time.
    blob_lock: std::sync::Mutex<()>,
    /// Bytes the stored blobs take (kept in step with `file_blobs`).
    stored: AtomicI64,
    /// Uploads under way: bytes set aside per hash until they finish or
    /// stop, so parallel uploads cannot overrun the quota together.
    reserved: std::sync::Mutex<HashMap<String, u64>>,
    /// Largest file the hub accepts (`files.max_file_mb`).
    max_file: AtomicU64,
    /// Tests move the clock forward to age blobs past the grace period.
    #[cfg(test)]
    skew_ms: AtomicI64,
}

impl std::fmt::Debug for HubFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubFiles")
            .field("dir", &self.blobs.dir())
            .finish()
    }
}

impl HubFiles {
    /// `dir` is `BLIRP_HOME/files`; `quota` in bytes, `keep_ms` how long
    /// replaced versions stay.
    pub fn new(store: Arc<Store>, dir: &Path, quota: u64, keep_ms: i64) -> Self {
        let store_for_usage = store.clone();
        Self {
            store,
            blobs: BlobStore::new(dir),
            quota: AtomicU64::new(quota),
            keep_ms: AtomicI64::new(keep_ms),
            changed: broadcast::channel(256).0,
            blob_lock: std::sync::Mutex::new(()),
            stored: AtomicI64::new(store_usage(&store_for_usage)),
            reserved: std::sync::Mutex::new(HashMap::new()),
            max_file: AtomicU64::new(DEFAULT_MAX_FILE),
            #[cfg(test)]
            skew_ms: AtomicI64::new(0),
        }
    }

    fn now(&self) -> i64 {
        #[cfg(test)]
        return blirp_core::now_ms() + self.skew_ms.load(Ordering::Relaxed);
        #[cfg(not(test))]
        blirp_core::now_ms()
    }

    /// Largest upload accepted, in bytes.
    pub fn set_max_file(&self, bytes: u64) {
        self.max_file.store(bytes, Ordering::Relaxed);
    }

    pub fn max_file(&self) -> u64 {
        self.max_file.load(Ordering::Relaxed)
    }

    fn reserved_guard(&self) -> std::sync::MutexGuard<'_, HashMap<String, u64>> {
        // Plain map; a poisoned one is still consistent.
        self.reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// An upload of `hash` finished or stopped: its reservation ends.
    pub fn release(&self, hash: &str) {
        self.reserved_guard().remove(hash);
    }

    fn count_stored(&self, delta: i64) {
        self.stored.fetch_add(delta, Ordering::Relaxed);
    }

    pub fn set_limits(&self, quota: u64, keep_ms: i64) {
        self.quota.store(quota, Ordering::Relaxed);
        self.keep_ms.store(keep_ms, Ordering::Relaxed);
    }

    pub fn quota(&self) -> u64 {
        self.quota.load(Ordering::Relaxed)
    }

    fn blob_guard(&self) -> std::sync::MutexGuard<'_, ()> {
        // Guards no data; a poisoned lock is as good as a fresh one.
        self.blob_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Bytes the stored blobs take plus what running uploads set aside.
    pub fn usage(&self) -> Result<u64, HubError> {
        let reserved = self.reserved_guard();
        Ok(self
            .stored_bytes()
            .saturating_add(self.pending_bytes(&reserved, None)))
    }

    /// Disk taken or set aside by uploads: a running one counts what it
    /// reserved, a stopped one what its part holds (until it resumes or
    /// the daily sweep drops it). `resuming`: an upload about to reserve
    /// its full length, whose part that length already covers.
    fn pending_bytes(&self, reserved: &HashMap<String, u64>, resuming: Option<&str>) -> u64 {
        let parts: u64 = self
            .blobs
            .parts()
            .into_iter()
            .filter(|(h, _)| !reserved.contains_key(h) && Some(h.as_str()) != resuming)
            .map(|(_, n)| n)
            .sum();
        parts.saturating_add(reserved.values().sum())
    }

    fn stored_bytes(&self) -> u64 {
        u64::try_from(self.stored.load(Ordering::Relaxed)).unwrap_or(0)
    }

    /// Roots with their totals, and every project's mode.
    pub fn roots(&self) -> Result<(Vec<RootInfo>, HashMap<String, FilesMode>), HubError> {
        Ok((self.store.hub_file_roots()?, self.store.hub_file_modes()?))
    }

    pub fn modes(&self) -> Result<HashMap<String, FilesMode>, HubError> {
        Ok(self.store.hub_file_modes()?)
    }

    pub fn set_mode(&self, project: &str, mode: FilesMode) -> Result<(), HubError> {
        if !blirp_core::is_safe_id(project) {
            return Err(HubError::Invalid("invalid project id".into()));
        }
        self.store.hub_set_file_mode(project, mode)?;
        Ok(())
    }

    pub fn index(&self, root: &str, after: i64, limit: usize) -> Result<IndexSlice, HubError> {
        if !is_root_id(root) {
            return Err(HubError::Invalid("invalid root id".into()));
        }
        self.store
            .hub_file_index(root, after, limit)?
            .ok_or(HubError::UnknownRoot)
    }

    /// For each hash the hub does not have: how many bytes of it arrived
    /// already (resume offset).
    pub fn missing(&self, hashes: &[String]) -> Result<Vec<(String, u64)>, HubError> {
        if let Some(bad) = hashes.iter().find(|h| !is_hash(h)) {
            return Err(HubError::Invalid(format!("invalid hash {bad:.16}")));
        }
        let known = self.store.hub_blobs_known(hashes)?;
        // A commit will use them: keep them out of the next collection.
        let known_list: Vec<String> = known.iter().cloned().collect();
        self.store
            .hub_touch_blobs(&known_list, blirp_core::now_ms())?;
        Ok(hashes
            .iter()
            .filter(|h| !known.contains(*h) || !self.blobs.has(h))
            .map(|h| (h.clone(), self.blobs.part_len(h)))
            .collect())
    }

    /// Set aside `len` bytes for `hash`: checked and recorded under the
    /// reservation lock, so parallel uploads cannot overrun the quota
    /// together. When they do not fit, old history is dropped, step by step
    /// and only as far as needed, and only if dropping all of it could make
    /// room at all. Pruning runs without the reservation lock (collecting
    /// takes the blob lock, which stores hold while they release), and the
    /// check is made again after every step.
    fn reserve(&self, hash: &str, len: u64) -> Result<(), HubError> {
        let quota = self.quota();
        let mut checked_reclaimable = false;
        loop {
            {
                let mut reserved = self.reserved_guard();
                reserved.remove(hash);
                let used = self
                    .stored_bytes()
                    .saturating_add(self.pending_bytes(&reserved, Some(hash)));
                if used.saturating_add(len) <= quota {
                    reserved.insert(hash.to_string(), len);
                    return Ok(());
                }
                if !checked_reclaimable {
                    let before = self.now() - BLOB_GRACE_MS;
                    let reclaimable =
                        u64::try_from(self.store.hub_blob_reclaimable(before)?).unwrap_or(0);
                    if used.saturating_sub(reclaimable).saturating_add(len) > quota {
                        return Err(HubError::Quota);
                    }
                    checked_reclaimable = true;
                }
            }
            let dropped = self.store.hub_prune_oldest_history(QUOTA_PRUNE_STEP)?;
            let collected = self.collect(self.now())?;
            if dropped == 0 && collected == 0 {
                return Err(HubError::Quota);
            }
        }
    }

    /// Start (or resume) an upload of `len` bytes: returns the offset to
    /// continue from. Checks the quota first.
    pub fn begin_put(&self, hash: &str, len: u64) -> Result<u64, HubError> {
        if !is_hash(hash) {
            return Err(HubError::Invalid("invalid hash".into()));
        }
        if len > self.max_file() {
            return Err(HubError::TooLarge);
        }
        let have = self.blobs.part_len(hash);
        if have > len {
            self.blobs.discard_part(hash);
            return self.begin_put(hash, len);
        }
        self.reserve(hash, len)?;
        // The part exists from here on, so empty content (no chunk) finishes.
        if let Err(e) = self.blobs.start_part(hash) {
            self.release(hash);
            return Err(e.into());
        }
        Ok(have)
    }

    pub fn put_chunk(&self, hash: &str, offset: u64, raw: &[u8]) -> Result<u64, HubError> {
        Ok(self.blobs.append(hash, offset, raw)?)
    }

    /// Drop a partial upload (the sender broke the protocol).
    pub fn abort_put(&self, hash: &str) {
        self.blobs.discard_part(hash);
        self.release(hash);
    }

    /// The part of `hash` holds `len` bytes: verify and store it.
    pub fn finish_put(&self, hash: &str, len: u64) -> Result<(), HubError> {
        let result = self.store_locked(
            || {
                let stored = self.blobs.finish(hash, len)?;
                Ok((len, stored))
            },
            hash,
        );
        // Released only after the blob lock is dropped: `reserve` holds the
        // reservation lock while it may collect (which takes the blob lock).
        self.release(hash);
        result
    }

    /// Run `put` (writing blob `hash`, returning its raw and stored size)
    /// and record it, under the blob lock.
    fn store_locked(
        &self,
        put: impl FnOnce() -> Result<(u64, u64), HubError>,
        hash: &str,
    ) -> Result<(), HubError> {
        let _g = self.blob_guard();
        let (size, stored) = put()?;
        let stored = i64::try_from(stored).unwrap_or(i64::MAX);
        if self.store.hub_blob_added(
            hash,
            i64::try_from(size).unwrap_or(i64::MAX),
            stored,
            blirp_core::now_ms(),
        )? {
            self.count_stored(stored);
        }
        Ok(())
    }

    /// The hub's own engine: store a local file as `hash` (verified).
    pub fn import(&self, hash: &str, src: &Path, len: u64) -> Result<(), HubError> {
        if self.store.hub_blob_size(hash)?.is_some() && self.blobs.has(hash) {
            self.store
                .hub_touch_blobs(&[hash.to_string()], blirp_core::now_ms())?;
            return Ok(());
        }
        if len > self.max_file() {
            return Err(HubError::TooLarge);
        }
        self.reserve(hash, len)?;
        let result = self.store_locked(|| Ok(self.blobs.import(hash, src)?), hash);
        self.release(hash);
        result
    }

    /// Raw length and content of a stored blob.
    pub fn open(&self, hash: &str) -> Result<Option<(u64, BlobReader)>, HubError> {
        if !is_hash(hash) {
            return Err(HubError::Invalid("invalid hash".into()));
        }
        let Some(size) = self.store.hub_blob_size(hash)? else {
            return Ok(None);
        };
        Ok(self
            .blobs
            .open(hash)?
            .map(|r| (u64::try_from(size).unwrap_or(0), r)))
    }

    /// Apply a commit batch from `machine_id` (the authenticated writer).
    #[allow(clippy::too_many_arguments)]
    pub fn commit(
        &self,
        machine_id: &str,
        machine_name: &str,
        root_id: &str,
        claim_path: Option<&str>,
        incarnation: Option<&str>,
        manifest: Option<&GitManifest>,
        changes: &[FileChange],
    ) -> Result<Result<CommitOutcome, CommitRefused>, HubError> {
        let out = self.store.hub_file_commit(&CommitInput {
            root_id,
            machine_id,
            machine_name,
            claim_path,
            incarnation,
            manifest,
            changes,
            now: blirp_core::now_ms(),
        })?;
        if let Ok(o) = &out {
            let _ = self.changed.send(RootChanged {
                root_id: root_id.to_string(),
                head: o.head,
            });
        }
        Ok(out)
    }

    /// "Delete hub copy": only when the project's file sync is off or the
    /// origin machine was revoked. Blobs go with the next collection.
    pub fn delete_root(&self, root_id: &str) -> Result<(), HubError> {
        let info = self
            .store
            .hub_file_root(root_id)?
            .ok_or(HubError::UnknownRoot)?;
        let mode = self.store.hub_file_mode(&info.project_id)?;
        if mode != FilesMode::Off && !info.origin_revoked {
            return Err(HubError::NotPaused);
        }
        self.store.hub_delete_file_root(root_id)?;
        let _ = self.changed.send(RootChanged {
            root_id: root_id.to_string(),
            head: -1,
        });
        Ok(())
    }

    /// Remove unreferenced blobs older than the grace period.
    fn collect(&self, now: i64) -> Result<usize, HubError> {
        let before = now - BLOB_GRACE_MS;
        let unused = self.store.hub_unreferenced_blobs(before)?;
        let _g = self.blob_guard();
        // Re-checked in the delete's own transaction: a commit that started
        // using one meanwhile keeps it.
        let gone = self.store.hub_forget_blobs(&unused, before)?;
        for (h, stored) in &gone {
            self.blobs.remove(h);
            self.count_stored(-stored);
        }
        Ok(gone.len())
    }

    /// Daily retention and mark-and-sweep: old history and tombstones,
    /// unreferenced blobs, stale partial uploads and orphaned files.
    pub fn gc(&self, now: i64) -> Result<GcStats, HubError> {
        let keep = self.keep_ms.load(Ordering::Relaxed);
        let (history, tombstones) = self.store.hub_prune_files(now - keep, now - TOMBSTONE_MS)?;
        let blobs = self.collect(now)?;
        self.blobs.sweep_parts(PART_AGE);
        let store = self.store.clone();
        let _g = self.blob_guard();
        self.blobs
            .sweep_orphans(Duration::from_millis(BLOB_GRACE_MS as u64), &|h| {
                store.hub_blob_size(h).ok().flatten().is_some()
            });
        Ok(GcStats {
            history,
            tombstones,
            blobs,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RootChanged> {
        self.changed.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blirp_core::files::{ChangeOp, ChangeResult, hash_bytes};

    fn put(path: &str, base: i64, data: &[u8]) -> FileChange {
        FileChange {
            path: path.into(),
            base_version: base,
            op: ChangeOp::Put {
                hash: hash_bytes(data),
                size: data.len() as i64,
                mode_x: false,
                mtime: 0,
            },
        }
    }

    struct T {
        _dir: tempfile::TempDir,
        hub: HubFiles,
        root: String,
        folder: String,
        src: std::path::PathBuf,
    }

    fn setup(quota: u64) -> T {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("db")).unwrap());
        let folder = dir.path().join("p");
        std::fs::create_dir(&folder).unwrap();
        let p = store.register_project("m", &folder, None).unwrap();
        let folder = store.project_paths(&p.id).unwrap()[0].path.clone();
        let hub = HubFiles::new(store, &dir.path().join("files"), quota, 1000);
        T {
            root: blirp_core::files::root_id("m", &folder),
            src: dir.path().join("src"),
            _dir: dir,
            hub,
            folder,
        }
    }

    impl T {
        fn upload(&self, data: &[u8]) -> Result<(), HubError> {
            std::fs::write(&self.src, data).unwrap();
            self.hub
                .import(&hash_bytes(data), &self.src, data.len() as u64)
        }
    }

    #[test]
    fn blobs_then_commit_then_gc() {
        let t = setup(1 << 30);
        let mut rx = t.hub.subscribe();
        let a = b"version a".repeat(10);
        // Resumable network upload.
        let h = hash_bytes(&a);
        assert_eq!(
            t.hub.missing(std::slice::from_ref(&h)).unwrap(),
            [(h.clone(), 0)]
        );
        assert_eq!(t.hub.begin_put(&h, a.len() as u64).unwrap(), 0);
        t.hub.put_chunk(&h, 0, &a[..5]).unwrap();
        assert_eq!(
            t.hub.missing(std::slice::from_ref(&h)).unwrap(),
            [(h.clone(), 5)]
        );
        assert_eq!(t.hub.begin_put(&h, a.len() as u64).unwrap(), 5);
        t.hub.put_chunk(&h, 5, &a[5..]).unwrap();
        t.hub.finish_put(&h, a.len() as u64).unwrap();
        assert!(t.hub.missing(std::slice::from_ref(&h)).unwrap().is_empty());
        let out = t
            .hub
            .commit(
                "m",
                "m",
                &t.root,
                Some(&t.folder),
                None,
                None,
                &[put("a.txt", 0, &a)],
            )
            .unwrap()
            .unwrap();
        assert_eq!(out.results, [ChangeResult::Ok { version: 1 }]);
        assert_eq!(rx.try_recv().unwrap().head, 1);
        let (len, mut r) = t.hub.open(&h).unwrap().unwrap();
        let mut back = Vec::new();
        std::io::Read::read_to_end(&mut r, &mut back).unwrap();
        assert_eq!((len, back), (a.len() as u64, a.clone()));

        // Replaced: kept in history until retention drops it, then collected.
        t.upload(b"b").unwrap();
        t.hub
            .commit(
                "m",
                "m",
                &t.root,
                None,
                None,
                None,
                &[put("a.txt", 1, b"b")],
            )
            .unwrap()
            .unwrap();
        let later = blirp_core::now_ms() + BLOB_GRACE_MS + 10_000;
        let s = t.hub.gc(later).unwrap();
        assert_eq!((s.history, s.blobs), (1, 1));
        assert!(t.hub.open(&h).unwrap().is_none());
    }

    #[test]
    fn quota_prunes_history_before_refusing() {
        let t = setup(1_000);
        // Random bytes do not compress: stored size ~ raw size.
        let noise = |seed: &[u8], n: usize| {
            let mut v = vec![0u8; n];
            blake3::Hasher::new()
                .update(seed)
                .finalize_xof()
                .fill(&mut v);
            v
        };
        let big = noise(b"one", 600);
        t.upload(&big).unwrap();
        t.hub
            .commit(
                "m",
                "m",
                &t.root,
                Some(&t.folder),
                None,
                None,
                &[put("f", 0, &big)],
            )
            .unwrap()
            .unwrap();
        // The file still uses the blob: nothing to prune, refused.
        let big2 = noise(b"two", 700);
        assert!(matches!(t.upload(&big2), Err(HubError::Quota)));
        assert_eq!(
            t.hub.begin_put(&hash_bytes(&big2), 700).unwrap_err().code(),
            "hub_quota"
        );
        // A refused upload leaves no part behind.
        assert!(!t.hub.blobs.part_path(&hash_bytes(&big2)).unwrap().exists());
        // Deleting a hub copy needs the project turned off first.
        assert!(matches!(
            t.hub.delete_root(&t.root),
            Err(HubError::NotPaused)
        ));
        let project = t.hub.roots().unwrap().0[0].project_id.clone();
        t.hub.set_mode(&project, FilesMode::Off).unwrap();
        t.hub.delete_root(&t.root).unwrap();
        assert!(matches!(
            t.hub.index(&t.root, 0, 10),
            Err(HubError::UnknownRoot)
        ));
    }

    #[test]
    fn parallel_uploads_near_the_quota_never_deadlock() {
        let t = Arc::new(setup(3_000));
        let noise = |seed: &str| {
            let mut v = vec![0u8; 400];
            blake3::Hasher::new()
                .update(seed.as_bytes())
                .finalize_xof()
                .fill(&mut v);
            v
        };
        // Six versions of one file: five in history, one current.
        for v in 0..6i64 {
            let data = noise(&format!("v{v}"));
            t.upload(&data).unwrap();
            t.hub
                .commit(
                    "m",
                    "m",
                    &t.root,
                    (v == 0).then_some(t.folder.as_str()),
                    None,
                    None,
                    &[put("f", v, &data)],
                )
                .unwrap()
                .unwrap();
        }
        // Past the grace period: history is reclaimable, so reservations
        // prune and collect (blob lock) while others store (blob lock) and
        // release (reservation lock).
        t.hub.skew_ms.store(2 * BLOB_GRACE_MS, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        for i in 0..8 {
            let (t, tx) = (t.clone(), tx.clone());
            std::thread::spawn(move || {
                for j in 0..3 {
                    let data = noise(&format!("t{i}-{j}"));
                    let src = t.src.with_extension(format!("{i}-{j}"));
                    std::fs::write(&src, &data).unwrap();
                    let r = t.hub.import(&hash_bytes(&data), &src, data.len() as u64);
                    assert!(matches!(r, Ok(()) | Err(HubError::Quota)), "{r:?}");
                }
                tx.send(()).unwrap();
            });
        }
        drop(tx);
        for _ in 0..8 {
            rx.recv_timeout(Duration::from_secs(60))
                .expect("uploads deadlocked");
        }
        assert!(t.hub.usage().unwrap() <= 3_000);
    }

    #[test]
    fn an_upload_near_the_quota_resumes() {
        let t = setup(1_000);
        let data = vec![7u8; 700];
        let h = hash_bytes(&data);
        assert_eq!(t.hub.begin_put(&h, 700).unwrap(), 0);
        t.hub.put_chunk(&h, 0, &data[..400]).unwrap();
        // The stream broke: the reservation ends, the part stays.
        t.hub.release(&h);
        assert_eq!(t.hub.usage().unwrap(), 400);
        // Its own part counts once, inside the 700 it reserves again.
        assert_eq!(t.hub.begin_put(&h, 700).unwrap(), 400);
        assert_eq!(t.hub.usage().unwrap(), 700);
        t.hub.put_chunk(&h, 400, &data[400..]).unwrap();
        t.hub.finish_put(&h, 700).unwrap();
        assert!(t.hub.missing(std::slice::from_ref(&h)).unwrap().is_empty());
    }

    #[test]
    fn bad_input_is_refused() {
        let t = setup(1 << 20);
        assert!(matches!(
            t.hub.missing(&["../x".into()]),
            Err(HubError::Invalid(_))
        ));
        assert!(matches!(
            t.hub.begin_put("zz", 1),
            Err(HubError::Invalid(_))
        ));
        assert!(matches!(t.hub.index("x", 0, 1), Err(HubError::Invalid(_))));
        assert!(matches!(
            t.hub.set_mode("../p", FilesMode::On),
            Err(HubError::Invalid(_))
        ));
        // A blob whose bytes do not match is not stored.
        let h = hash_bytes(b"real");
        t.hub.begin_put(&h, 4).unwrap();
        t.hub.put_chunk(&h, 0, b"fake").unwrap();
        assert_eq!(t.hub.finish_put(&h, 4).unwrap_err().code(), "hash_mismatch");
        assert_eq!(t.hub.missing(std::slice::from_ref(&h)).unwrap(), [(h, 0)]);
    }
}
