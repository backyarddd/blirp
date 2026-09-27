//! Content-addressed blob storage on disk: `<dir>/blobs/ab/cd/<hash>.zst`
//! (zstd level 3). Uploads arrive as raw bytes appended to
//! `<dir>/tmp/<hash>.part`, so an interrupted upload resumes at the part's
//! length; a finished part is verified against its BLAKE3 hash, compressed,
//! fsynced and renamed into place. Nodes use the same layout for partial
//! downloads.

use blirp_core::files::is_hash;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// zstd level for blobs at rest and on the wire.
pub const ZSTD_LEVEL: i32 = 3;

/// Raw content of a stored blob.
pub type BlobReader = zstd::stream::read::Decoder<'static, std::io::BufReader<std::fs::File>>;

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("not a content hash")]
    BadHash,
    #[error("content does not match its hash")]
    Mismatch,
    #[error("upload offset {offset} does not match the {have} bytes received")]
    Offset { offset: u64, have: u64 },
    #[error("blob storage: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn checked(hash: &str) -> Result<(), BlobError> {
        if is_hash(hash) {
            Ok(())
        } else {
            Err(BlobError::BadHash)
        }
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.dir
            .join("blobs")
            .join(&hash[..2])
            .join(&hash[2..4])
            .join(format!("{hash}.zst"))
    }

    /// Partial upload (hub) or download (node) of `hash`.
    pub fn part_path(&self, hash: &str) -> Result<PathBuf, BlobError> {
        Self::checked(hash)?;
        Ok(self.dir.join("tmp").join(format!("{hash}.part")))
    }

    pub fn has(&self, hash: &str) -> bool {
        is_hash(hash) && self.blob_path(hash).is_file()
    }

    /// Bytes of `hash` received so far.
    pub fn part_len(&self, hash: &str) -> u64 {
        self.part_path(hash)
            .ok()
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len())
    }

    /// Append raw bytes at `offset` (must equal what was received so far).
    pub fn append(&self, hash: &str, offset: u64, raw: &[u8]) -> Result<u64, BlobError> {
        let part = self.part_path(hash)?;
        if let Some(p) = part.parent() {
            std::fs::create_dir_all(p)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&part)?;
        let have = f.metadata()?.len();
        if have != offset {
            return Err(BlobError::Offset { offset, have });
        }
        f.write_all(raw)?;
        Ok(have + raw.len() as u64)
    }

    /// Drop a partial transfer.
    pub fn discard_part(&self, hash: &str) {
        if let Ok(p) = self.part_path(hash) {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Verify the finished part of `hash` (`len` bytes) and store it.
    /// Returns the stored (compressed) size. A mismatch drops the part.
    pub fn finish(&self, hash: &str, len: u64) -> Result<u64, BlobError> {
        let part = self.part_path(hash)?;
        let result = std::fs::File::open(&part)
            .map_err(BlobError::from)
            .and_then(|f| self.store_from(hash, f, Some(len)));
        let _ = std::fs::remove_file(&part);
        result
    }

    /// Store the content of `src` (a local file) as `hash`, verified while
    /// it is copied. Returns (size, stored size).
    pub fn import(&self, hash: &str, src: &Path) -> Result<(u64, u64), BlobError> {
        Self::checked(hash)?;
        let f = std::fs::File::open(src)?;
        let size = f.metadata()?.len();
        let stored = self.store_from(hash, f, Some(size))?;
        Ok((size, stored))
    }

    /// Compress `src` into place, checking its hash (and length) on the way.
    fn store_from(
        &self,
        hash: &str,
        mut src: impl Read,
        len: Option<u64>,
    ) -> Result<u64, BlobError> {
        Self::checked(hash)?;
        let dest = self.blob_path(hash);
        let parent = dest.parent().ok_or(BlobError::BadHash)?;
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            ".{hash}.{}.tmp",
            blirp_core::random_hex::<6>().map_err(BlobError::Io)?
        ));
        let result = (|| {
            let out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            let mut enc = zstd::stream::Encoder::new(out, ZSTD_LEVEL)?;
            let mut hasher = blake3::Hasher::new();
            let mut buf = vec![0u8; 256 * 1024];
            let mut total = 0u64;
            loop {
                let n = src.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                enc.write_all(&buf[..n])?;
                total += n as u64;
            }
            if hasher.finalize().to_hex().as_str() != hash || len.is_some_and(|l| l != total) {
                return Err(BlobError::Mismatch);
            }
            let out = enc.finish()?;
            out.sync_all()?;
            let stored = out.metadata()?.len();
            drop(out);
            std::fs::rename(&tmp, &dest)?;
            Ok(stored)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Stream the raw content of a stored blob.
    pub fn open(&self, hash: &str) -> Result<Option<BlobReader>, BlobError> {
        Self::checked(hash)?;
        match std::fs::File::open(self.blob_path(hash)) {
            Ok(f) => Ok(Some(zstd::stream::Decoder::new(f)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn remove(&self, hash: &str) {
        if is_hash(hash) {
            let _ = std::fs::remove_file(self.blob_path(hash));
        }
    }

    /// Bytes held by partial transfers.
    pub fn parts_bytes(&self) -> u64 {
        std::fs::read_dir(self.dir.join("tmp")).map_or(0, |rd| {
            rd.flatten()
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
    }

    /// Remove partial transfers not touched for `age`.
    pub fn sweep_parts(&self, age: Duration) -> usize {
        let Ok(rd) = std::fs::read_dir(self.dir.join("tmp")) else {
            return 0;
        };
        let now = SystemTime::now();
        let mut n = 0;
        for e in rd.flatten() {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|d| d > age);
            if old && std::fs::remove_file(e.path()).is_ok() {
                n += 1;
            }
        }
        n
    }

    /// Blob files (and stale temp files) older than `age` that `known` does
    /// not list: left behind by a crash between the rename and the database
    /// row.
    pub fn sweep_orphans(&self, age: Duration, known: &dyn Fn(&str) -> bool) -> usize {
        let now = SystemTime::now();
        let mut n = 0;
        let old = |p: &Path| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|d| d > age)
        };
        let Ok(l1) = std::fs::read_dir(self.dir.join("blobs")) else {
            return 0;
        };
        for a in l1.flatten() {
            let Ok(l2) = std::fs::read_dir(a.path()) else {
                continue;
            };
            for b in l2.flatten() {
                let Ok(l3) = std::fs::read_dir(b.path()) else {
                    continue;
                };
                for f in l3.flatten() {
                    let p = f.path();
                    let name = f.file_name().to_string_lossy().into_owned();
                    let keep = name
                        .strip_suffix(".zst")
                        .is_some_and(|h| is_hash(h) && known(h));
                    if !keep && old(&p) && std::fs::remove_file(&p).is_ok() {
                        n += 1;
                    }
                }
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blirp_core::files::hash_bytes;

    #[test]
    fn upload_resumes_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let b = BlobStore::new(dir.path());
        let data = b"hello world, this is a blob".repeat(100);
        let h = hash_bytes(&data);
        assert!(!b.has(&h));
        assert_eq!(b.append(&h, 0, &data[..100]).unwrap(), 100);
        // A wrong offset is refused; the part keeps its length.
        assert!(matches!(
            b.append(&h, 50, &data[..10]),
            Err(BlobError::Offset { have: 100, .. })
        ));
        assert_eq!(b.part_len(&h), 100);
        b.append(&h, 100, &data[100..]).unwrap();
        let stored = b.finish(&h, data.len() as u64).unwrap();
        assert!(stored > 0 && stored < data.len() as u64);
        assert!(b.has(&h));
        assert_eq!(b.part_len(&h), 0);
        let mut back = Vec::new();
        b.open(&h).unwrap().unwrap().read_to_end(&mut back).unwrap();
        assert_eq!(back, data);
        // Content that does not match its hash is dropped.
        let other = hash_bytes(b"other");
        b.append(&other, 0, b"tampered").unwrap();
        assert!(matches!(b.finish(&other, 8), Err(BlobError::Mismatch)));
        assert!(!b.has(&other));
        assert_eq!(b.part_len(&other), 0);
        assert!(matches!(b.append("../x", 0, b""), Err(BlobError::BadHash)));
        assert!(b.open(&other).unwrap().is_none());
    }

    #[test]
    fn import_and_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let b = BlobStore::new(dir.path().join("files"));
        let src = dir.path().join("f");
        std::fs::write(&src, "content").unwrap();
        let h = hash_bytes(b"content");
        assert_eq!(b.import(&h, &src).unwrap().0, 7);
        assert!(matches!(
            b.import(&hash_bytes(b"x"), &src),
            Err(BlobError::Mismatch)
        ));
        assert_eq!(b.sweep_orphans(Duration::ZERO, &|x| x == h), 0);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(b.sweep_orphans(Duration::ZERO, &|_| false), 1);
        assert!(!b.has(&h));
        b.append(&h, 0, b"c").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(b.sweep_parts(Duration::ZERO), 1);
    }
}
