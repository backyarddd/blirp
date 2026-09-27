//! Scanner of one working copy: walks the folder without following
//! symlinks, applies the exclusion layers ([`super::rules`]) and the caps,
//! and hashes candidate files (BLAKE3) with a cache keyed by size, mtime
//! and file id. Nothing here writes anything.

use super::path::{self, TMP_PREFIX};
use super::rules::{Frame, KEY_CHECK_HEAD, KEY_CHECK_MAX, Reason, Rules, Verdict};
use crate::paths::path_key;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// A file whose mtime lies within this window of the time it was hashed may
/// have changed again within the filesystem's timestamp resolution after
/// hashing (git's "racy" rule), so its cached hash is not trusted.
pub const RACY_NS: i64 = 2_000_000_000;
/// A blirp temp file older than this is left over from a crash.
pub const STALE_TEMP_SECS: u64 = 24 * 3600;

#[derive(Debug, Clone)]
pub struct ScanConfig {
    pub max_file_bytes: u64,
    pub max_root_bytes: u64,
    pub max_files: usize,
    /// blirp's data folder: never scanned into.
    pub data_dir: Option<PathBuf>,
}

/// A file or symlink that passed the exclusion layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: String,
    pub kind: FoundKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoundKind {
    File {
        size: u64,
        mtime_ns: i64,
        /// Inode on unix, creation time on Windows: a file replaced by
        /// another one with the same size and mtime still differs.
        file_id: i64,
        mode_x: bool,
        /// `.blirpignore` re-includes it (checked again for key content).
        reincluded: bool,
    },
    /// Target text, relative and inside the root.
    Link { target: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Excluded {
    pub path: String,
    pub reason: Reason,
    pub dir: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanState {
    Ok,
    /// A git operation is in progress (`.git/index.lock`, `rebase-*`).
    Busy,
    /// Over `max_root_bytes` or `max_files`: the whole root is paused.
    TooLarge,
}

#[derive(Debug, Clone)]
pub struct Scan {
    pub found: Vec<Found>,
    pub excluded: Vec<Excluded>,
    /// Secrets that `.blirpignore` re-includes (by name).
    pub reincluded_secrets: Vec<String>,
    /// Folders (wire paths, "" for the root) that could not be listed
    /// completely: nothing below them may be read as deleted.
    pub unreadable: Vec<String>,
    /// blirp temp files a crash left behind (older than a day).
    pub stale_temp: Vec<PathBuf>,
    /// Bytes of the found files.
    pub bytes: u64,
    pub state: ScanState,
}

/// Whether a git operation in `root` (a work tree) holds its index.
pub fn git_busy(root: &Path) -> bool {
    let git = root.join(".git");
    if !git.is_dir() {
        return false;
    }
    git.join("index.lock").exists()
        || git.join("rebase-merge").exists()
        || git.join("rebase-apply").exists()
}

fn mtime_ns(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn file_id(m: &std::fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.ino() as i64
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        m.creation_time() as i64
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = m;
        0
    }
}

fn is_exec(m: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = m;
        false
    }
}

/// `target` of a symlink at `link` (wire path) when it is relative and
/// stays inside the root; links leaving the root are never synced. `..`
/// may only lead the target: after a name, that name could itself be a
/// link, and the text would no longer say where the link goes (`a -> .`,
/// `b -> a/..` is the root's parent). With `..` only in front, every link
/// resolves from the real folder it lives in, so chains stay inside too.
pub fn link_inside(link: &str, target: &Path) -> Option<String> {
    if target.is_absolute() || target.has_root() {
        return None;
    }
    let raw = target.to_str()?.replace('\\', "/");
    let mut depth: Vec<&str> = link.split('/').collect();
    depth.pop();
    let mut named = false;
    for c in raw.split('/') {
        match c {
            "" | "." => {}
            ".." if named => return None,
            ".." => {
                depth.pop()?;
            }
            c => {
                named = true;
                depth.push(c);
            }
        }
    }
    let resolved = depth.join("/");
    if !resolved.is_empty() && path::check(&resolved).is_err() {
        return None;
    }
    Some(raw)
}

struct Walk<'a> {
    root: &'a Path,
    rules: Rules,
    cfg: &'a ScanConfig,
    data_key: Option<PathBuf>,
    out: Scan,
    files: usize,
}

impl Walk<'_> {
    fn exclude(&mut self, wire: String, reason: Reason, dir: bool) {
        self.out.excluded.push(Excluded {
            path: wire,
            reason,
            dir,
        });
    }

    /// Returns false once a cap is hit (the walk stops).
    fn dir(&mut self, dir: &Path, rel: &str, frames: &mut Vec<Frame>) -> bool {
        frames.push(Frame::load(dir));
        let folder = rel.trim_end_matches('/').to_string();
        let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(dir) {
            Ok(r) => {
                let mut ok = Vec::new();
                for e in r {
                    match e {
                        Ok(e) => ok.push(e),
                        Err(_) => self.out.unreadable.push(folder.clone()),
                    }
                }
                ok
            }
            Err(e) => {
                tracing::debug!(dir = %dir.display(), error = %e, "cannot list folder; skipped");
                self.out.unreadable.push(folder);
                frames.pop();
                return true;
            }
        };
        entries.sort_by_key(std::fs::DirEntry::file_name);
        let mut go_on = true;
        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str().map(str::to_string) else {
                let shown = format!("{rel}{}", entry.file_name().to_string_lossy());
                self.exclude(shown, Reason::Unsupported, false);
                continue;
            };
            let wire = format!("{rel}{name}");
            // Layer 1: never synced, never listed.
            if name.starts_with(TMP_PREFIX) {
                let old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age.as_secs() > STALE_TEMP_SECS);
                if old
                    && entry
                        .file_type()
                        .is_ok_and(|t| t.is_file() || t.is_symlink())
                {
                    self.out.stale_temp.push(entry.path());
                }
                continue;
            }
            if path::is_vcs_component(&name) {
                continue;
            }
            if path::check(&wire).is_err() {
                self.exclude(wire, Reason::Unsupported, false);
                continue;
            }
            let abs = entry.path();
            let Ok(ft) = entry.file_type() else {
                self.out.unreadable.push(wire);
                continue;
            };
            let is_dir = ft.is_dir();
            if is_dir
                && self
                    .data_key
                    .as_ref()
                    .is_some_and(|d| path_key(Path::new(&wire)) == *d)
            {
                continue;
            }
            let meta = if ft.is_file() {
                entry.metadata().ok()
            } else {
                None
            };
            let size = meta.as_ref().map_or(0, std::fs::Metadata::len);
            let verdict = self.rules.decide(frames, &wire, &abs, is_dir, size);
            let reincluded = match verdict {
                Verdict::Exclude(reason) => {
                    self.exclude(wire, reason, is_dir);
                    continue;
                }
                Verdict::IncludeSecret => {
                    self.out.reincluded_secrets.push(wire.clone());
                    true
                }
                Verdict::Reincluded => true,
                Verdict::Include => false,
            };
            if is_dir {
                if !self.dir(&abs, &format!("{wire}/"), frames) {
                    go_on = false;
                    break;
                }
                continue;
            }
            if ft.is_symlink() {
                match std::fs::read_link(&abs)
                    .ok()
                    .and_then(|t| link_inside(&wire, &t))
                {
                    Some(target) => self.out.found.push(Found {
                        path: wire,
                        kind: FoundKind::Link { target },
                    }),
                    None => self.exclude(wire, Reason::Unsupported, false),
                }
                continue;
            }
            let Some(meta) = meta else {
                self.exclude(wire, Reason::Unsupported, false);
                continue;
            };
            if size > self.cfg.max_file_bytes {
                self.exclude(wire, Reason::TooLarge, false);
                continue;
            }
            self.files += 1;
            self.out.bytes += size;
            if self.files > self.cfg.max_files || self.out.bytes > self.cfg.max_root_bytes {
                self.out.state = ScanState::TooLarge;
                go_on = false;
                break;
            }
            self.out.found.push(Found {
                path: wire,
                kind: FoundKind::File {
                    size,
                    mtime_ns: mtime_ns(&meta),
                    file_id: file_id(&meta),
                    mode_x: is_exec(&meta),
                    reincluded,
                },
            });
        }
        frames.pop();
        go_on
    }
}

/// Walk `root` (an existing folder). `git` tells whether it is a git work
/// tree. Found entries are sorted by path.
pub fn scan(root: &Path, git: bool, cfg: &ScanConfig) -> std::io::Result<Scan> {
    let meta = std::fs::symlink_metadata(root)?;
    if !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "the project folder is not a folder",
        ));
    }
    let mut walk = Walk {
        root,
        rules: Rules::new(root, git),
        cfg,
        data_key: data_dir_inside(root, cfg.data_dir.as_deref()),
        out: Scan {
            found: Vec::new(),
            excluded: Vec::new(),
            reincluded_secrets: Vec::new(),
            unreadable: Vec::new(),
            stale_temp: Vec::new(),
            bytes: 0,
            state: ScanState::Ok,
        },
        files: 0,
    };
    if git && git_busy(root) {
        walk.out.state = ScanState::Busy;
        return Ok(walk.out);
    }
    let mut frames = vec![Frame::git_exclude(walk.root)];
    walk.dir(root, "", &mut frames);
    walk.out.found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(walk.out)
}

/// Where the data folder sits inside `root`, as a key of its `/`-separated
/// relative path. Both sides are resolved first: the walk only knows paths
/// relative to the root, and the root may be spelled through a symlink
/// (`/var` on macOS) or an 8.3 short name on Windows.
fn data_dir_inside(root: &Path, data: Option<&Path>) -> Option<PathBuf> {
    let resolve = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let rel = resolve(data?)
        .strip_prefix(resolve(root))
        .ok()?
        .to_path_buf();
    let wire: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    (!wire.is_empty()).then(|| path_key(Path::new(&wire.join("/"))))
}

/// A hash cache row: what a file looked like when it was hashed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cached {
    pub size: u64,
    pub mtime_ns: i64,
    pub file_id: i64,
    pub hash: String,
    /// A private key header was found in its first bytes.
    pub secret: bool,
    /// When it was hashed (unix ms).
    pub checked_at: i64,
}

impl Cached {
    /// Whether this row still describes a file found now: same size,
    /// mtime and id, and it was hashed more than [`RACY_NS`] after its
    /// mtime.
    pub fn fresh(&self, size: u64, mtime_ns: i64, file_id: i64) -> bool {
        self.size == size
            && self.mtime_ns == mtime_ns
            && self.file_id == file_id
            && self
                .checked_at
                .saturating_mul(1_000_000)
                .saturating_sub(mtime_ns)
                > RACY_NS
    }
}

/// Hash a file: BLAKE3 of the whole content, and whether its head holds a
/// private key (checked for files up to 1 MiB).
pub fn hash_file(file: &Path) -> std::io::Result<(String, u64, bool)> {
    let mut f = std::fs::File::open(file)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut head: Vec<u8> = Vec::with_capacity(KEY_CHECK_HEAD);
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if head.len() < KEY_CHECK_HEAD {
            let take = (KEY_CHECK_HEAD - head.len()).min(n);
            head.extend_from_slice(&buf[..take]);
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    let secret = total <= KEY_CHECK_MAX && super::rules::looks_like_private_key(&head);
    Ok((hasher.finalize().to_hex().to_string(), total, secret))
}

/// A found file with its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashed {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub mode_x: bool,
    pub content: Content,
}

/// What a synced path holds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Content {
    Blob(String),
    Link(String),
}

/// Result of [`hash_all`].
#[derive(Debug, Default)]
pub struct HashPass {
    pub files: Vec<Hashed>,
    /// Cache rows to store (new or rehashed files).
    pub cache_updates: Vec<(String, Cached)>,
    /// Private keys found by content (excluded unless re-included).
    pub secrets: Vec<String>,
    /// Re-included by `.blirpignore` although a private key was found.
    pub reincluded_secrets: Vec<String>,
    /// Files that vanished or could not be read meanwhile.
    pub unreadable: Vec<String>,
}

/// Hash every found file, reusing `cache` rows that are still fresh.
/// `now_ms` is the scan time stored with new rows. Runs on the calling
/// thread; the engine calls it from the blocking pool behind a semaphore.
pub fn hash_all(
    root: &Path,
    found: &[Found],
    cache: &HashMap<String, Cached>,
    now_ms: i64,
) -> HashPass {
    let mut out = HashPass::default();
    for f in found {
        match &f.kind {
            FoundKind::Link { target } => out.files.push(Hashed {
                path: f.path.clone(),
                size: 0,
                mtime_ns: 0,
                mode_x: false,
                content: Content::Link(target.clone()),
            }),
            FoundKind::File {
                size,
                mtime_ns,
                file_id,
                mode_x,
                reincluded,
            } => {
                let hit = cache
                    .get(&f.path)
                    .filter(|c| c.fresh(*size, *mtime_ns, *file_id))
                    .map(|c| (c.hash.clone(), c.secret));
                let (hash, secret) = match hit {
                    Some(h) => h,
                    None => match hash_file(&path::to_local(root, &f.path)) {
                        Ok((hash, read, secret)) if read == *size => {
                            out.cache_updates.push((
                                f.path.clone(),
                                Cached {
                                    size: *size,
                                    mtime_ns: *mtime_ns,
                                    file_id: *file_id,
                                    hash: hash.clone(),
                                    secret,
                                    checked_at: now_ms,
                                },
                            ));
                            (hash, secret)
                        }
                        // Changed while being read: the next pass sees it settled.
                        Ok(_) | Err(_) => {
                            out.unreadable.push(f.path.clone());
                            continue;
                        }
                    },
                };
                if secret {
                    if *reincluded {
                        out.reincluded_secrets.push(f.path.clone());
                    } else {
                        out.secrets.push(f.path.clone());
                        continue;
                    }
                }
                out.files.push(Hashed {
                    path: f.path.clone(),
                    size: *size,
                    mtime_ns: *mtime_ns,
                    mode_x: *mode_x,
                    content: Content::Blob(hash),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ScanConfig {
        ScanConfig {
            max_file_bytes: 1000,
            max_root_bytes: 1 << 30,
            max_files: 100_000,
            data_dir: None,
        }
    }

    fn paths(s: &Scan) -> Vec<&str> {
        s.found.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn walks_with_every_layer() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        for d in [".git/hooks", "src", "node_modules/x", "keep"] {
            std::fs::create_dir_all(r.join(d)).unwrap();
        }
        std::fs::write(r.join(".git/hooks/post-checkout"), "x").unwrap();
        std::fs::write(r.join("src/a.rs"), "fn a() {}").unwrap();
        std::fs::write(r.join("node_modules/x/i.js"), "x").unwrap();
        std::fs::write(r.join(".env"), "K=V").unwrap();
        std::fs::write(r.join(".env.example"), "K=").unwrap();
        std::fs::write(r.join("big.bin"), vec![0u8; 1001]).unwrap();
        std::fs::write(r.join(".blirp-tmp-abc"), "x").unwrap();
        std::fs::write(r.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(r.join("x.log"), "x").unwrap();
        std::fs::write(r.join("keep/k.txt"), "k").unwrap();
        let s = scan(r, true, &cfg()).unwrap();
        assert_eq!(s.state, ScanState::Ok);
        assert_eq!(
            paths(&s),
            [".env.example", ".gitignore", "keep/k.txt", "src/a.rs"]
        );
        let ex: Vec<(&str, Reason, bool)> = s
            .excluded
            .iter()
            .map(|e| (e.path.as_str(), e.reason, e.dir))
            .collect();
        assert!(ex.contains(&(".env", Reason::Secret, false)), "{ex:?}");
        assert!(
            ex.contains(&("node_modules", Reason::Ignored, true)),
            "{ex:?}"
        );
        assert!(ex.contains(&("x.log", Reason::Ignored, false)), "{ex:?}");
        assert!(ex.contains(&("big.bin", Reason::TooLarge, false)), "{ex:?}");
        assert!(
            !ex.iter()
                .any(|e| e.0.starts_with(".git") && e.0 != ".gitignore"),
            "{ex:?}"
        );
        assert!(!ex.iter().any(|e| e.0.starts_with(".blirp-tmp")), "{ex:?}");
    }

    #[test]
    fn caps_pause_the_root_and_git_ops_pause_scanning() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        for i in 0..5 {
            std::fs::write(r.join(format!("f{i}")), "x").unwrap();
        }
        let s = scan(
            r,
            false,
            &ScanConfig {
                max_files: 4,
                ..cfg()
            },
        )
        .unwrap();
        assert_eq!(s.state, ScanState::TooLarge);
        let s = scan(
            r,
            false,
            &ScanConfig {
                max_root_bytes: 3,
                ..cfg()
            },
        )
        .unwrap();
        assert_eq!(s.state, ScanState::TooLarge);
        std::fs::create_dir_all(r.join(".git")).unwrap();
        std::fs::write(r.join(".git/index.lock"), "").unwrap();
        assert_eq!(scan(r, true, &cfg()).unwrap().state, ScanState::Busy);
        std::fs::remove_file(r.join(".git/index.lock")).unwrap();
        std::fs::create_dir(r.join(".git/rebase-merge")).unwrap();
        assert_eq!(scan(r, true, &cfg()).unwrap().state, ScanState::Busy);
    }

    #[test]
    fn data_dir_is_never_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join("data")).unwrap();
        std::fs::write(r.join("data/runtime.json"), "{}").unwrap();
        std::fs::write(r.join("a.txt"), "a").unwrap();
        let s = scan(
            r,
            false,
            &ScanConfig {
                data_dir: Some(r.join("data")),
                ..cfg()
            },
        )
        .unwrap();
        assert_eq!(paths(&s), ["a.txt"]);
        assert!(s.excluded.is_empty(), "{:?}", s.excluded);
    }

    /// The root may be spelled through a symlink while the data folder is
    /// given resolved (or the other way round): it is still skipped.
    #[cfg(unix)]
    #[test]
    fn data_dir_is_skipped_when_the_root_is_spelled_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("data")).unwrap();
        std::fs::write(real.join("data/runtime.json"), "{}").unwrap();
        std::fs::write(real.join("a.txt"), "a").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let s = scan(
            &link,
            false,
            &ScanConfig {
                data_dir: Some(real.join("data")),
                ..cfg()
            },
        )
        .unwrap();
        assert_eq!(paths(&s), ["a.txt"]);
    }

    #[test]
    fn hashing_uses_the_cache_and_the_racy_rule() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::write(r.join("a"), "hello").unwrap();
        std::fs::write(r.join("key.txt"), "-----BEGIN PRIVATE KEY-----\nabc").unwrap();
        let s = scan(r, false, &cfg()).unwrap();
        let FoundKind::File {
            size,
            mtime_ns,
            file_id,
            ..
        } = s.found[0].kind.clone()
        else {
            panic!("file expected")
        };
        let now_ms = mtime_ns / 1_000_000 + 10_000;
        let pass = hash_all(r, &s.found, &HashMap::new(), now_ms);
        assert_eq!(pass.secrets, ["key.txt"]);
        assert_eq!(pass.files.len(), 1);
        assert_eq!(
            pass.files[0].content,
            Content::Blob(crate::files::hash_bytes(b"hello"))
        );
        assert_eq!(pass.cache_updates.len(), 2);

        // A fresh row is reused: a wrong cached hash shows it was not reread.
        let mut cache: HashMap<String, Cached> = pass.cache_updates.into_iter().collect();
        if let Some(c) = cache.get_mut("a") {
            c.hash = "f".repeat(64);
        }
        let again = hash_all(r, &s.found, &cache, now_ms);
        assert_eq!(again.files[0].content, Content::Blob("f".repeat(64)));
        assert!(again.cache_updates.is_empty());

        // Hashed within 2 s of its mtime: not trusted, hashed again.
        if let Some(c) = cache.get_mut("a") {
            c.checked_at = mtime_ns / 1_000_000 + 1_000;
        }
        let racy = hash_all(r, &s.found, &cache, now_ms);
        assert_eq!(
            racy.files[0].content,
            Content::Blob(crate::files::hash_bytes(b"hello"))
        );
        let row = Cached {
            size,
            mtime_ns,
            file_id,
            hash: String::new(),
            secret: false,
            checked_at: mtime_ns / 1_000_000 + 2_001,
        };
        assert!(row.fresh(size, mtime_ns, file_id));
        assert!(!row.fresh(size + 1, mtime_ns, file_id));
        assert!(!row.fresh(size, mtime_ns + 1, file_id));
        assert!(!row.fresh(size, mtime_ns, file_id + 1));
    }

    #[test]
    fn reincluded_key_files_are_synced_and_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::write(r.join(".blirpignore"), "!deploy.pem\n!notes.txt\n").unwrap();
        std::fs::write(r.join("deploy.pem"), "x").unwrap();
        std::fs::write(r.join("notes.txt"), "-----BEGIN EC PRIVATE KEY-----").unwrap();
        let s = scan(r, false, &cfg()).unwrap();
        assert_eq!(s.reincluded_secrets, ["deploy.pem"]);
        let pass = hash_all(r, &s.found, &HashMap::new(), crate::now_ms());
        assert_eq!(pass.reincluded_secrets, ["notes.txt"]);
        assert!(pass.secrets.is_empty());
    }

    #[test]
    fn links_stay_inside() {
        assert_eq!(
            link_inside("a/l", Path::new("../b")).as_deref(),
            Some("../b")
        );
        assert_eq!(link_inside("a/l", Path::new("x/y")).as_deref(), Some("x/y"));
        assert_eq!(link_inside("l", Path::new("../out")), None);
        assert_eq!(link_inside("a/l", Path::new("../../out")), None);
        assert_eq!(link_inside("l", Path::new("/etc/passwd")), None);
        assert_eq!(link_inside("l", Path::new(".git/config")), None);
        // A name followed by `..` could be a link itself.
        assert_eq!(link_inside("b", Path::new("a/..")), None);
        assert_eq!(link_inside("d/b", Path::new("../x/../y")), None);
        assert_eq!(link_inside("a", Path::new(".")).as_deref(), Some("."));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_recorded_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path().join("root");
        std::fs::create_dir_all(r.join("d")).unwrap();
        std::fs::write(dir.path().join("secret"), "s").unwrap();
        std::os::unix::fs::symlink("d", r.join("in")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), r.join("out")).unwrap();
        std::os::unix::fs::symlink(dir.path(), r.join("updir")).unwrap();
        let s = scan(&r, false, &cfg()).unwrap();
        assert_eq!(
            s.found,
            [Found {
                path: "in".into(),
                kind: FoundKind::Link { target: "d".into() }
            }]
        );
        assert_eq!(s.excluded.len(), 2);
    }
}
