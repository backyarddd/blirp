//! Safe writes into a working copy. Every write:
//!
//! - takes a wire path that passes [`path::check`] and is valid on this OS,
//! - walks the parent folders from the root without following symlinks
//!   (a symlinked or junctioned parent refuses the write),
//! - never lands in blirp's data folder unless the root itself is there
//!   (folderless-project workspaces),
//! - writes a temp file `.blirp-tmp-<rand>` next to the target, fsyncs it,
//!   re-checks that the target still is what the caller expects (absent,
//!   or the content it last synced) and only then moves it into place: a
//!   new file with a hard link (fails if the name appeared meanwhile), an
//!   existing one with a rename.
//!
//! A target that no longer matches is reported as [`WriteError::Changed`]
//! and left alone: the caller writes a conflict copy instead.

use super::path::{self, TMP_PREFIX};
use crate::paths::path_key;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// The path is not safe to write (logged by the caller, never retried).
    #[error("refused to write {path}: {why}")]
    Refused { path: String, why: String },
    /// The local file is not what the caller expected anymore.
    #[error("{0} changed locally")]
    Changed(String),
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// What the target must be right before it is replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    Absent,
    /// A regular file with this BLAKE3 hash.
    Blob(String),
    /// A symlink with this target text.
    Link(String),
    /// Whatever is there (only for [`remove`] of a path known gone).
    Any,
}

/// Where writes may go.
#[derive(Debug, Clone)]
pub struct Target<'a> {
    /// The working copy folder, canonical.
    pub root: &'a Path,
    /// blirp's data folder (`BLIRP_HOME`).
    pub data_dir: Option<&'a Path>,
    /// Shared retry time of the pass (None: each operation its own).
    pub retry: Option<&'a RetryBudget>,
}

fn refused(wire: &str, why: impl Into<String>) -> WriteError {
    WriteError::Refused {
        path: wire.to_string(),
        why: why.into(),
    }
}

fn io(wire: &str) -> impl Fn(std::io::Error) -> WriteError + '_ {
    move |source| WriteError::Io {
        path: wire.to_string(),
        source,
    }
}

impl Target<'_> {
    fn check_wire(&self, wire: &str) -> Result<(), WriteError> {
        path::check(wire).map_err(|e| refused(wire, e.to_string()))?;
        if !path::valid_here(wire) {
            return Err(refused(wire, "the name is not valid on this system"));
        }
        if let Some(data) = self.data_dir {
            // Both canonical: a root reached through another spelling
            // (a link, short names, a different case) is still caught.
            let canon =
                |p: &Path| path_key(&dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));
            let (data, root) = (canon(data), canon(self.root));
            let target = path_key(&path::to_local(&root, wire));
            if target.starts_with(&data) && !root.starts_with(&data) {
                return Err(refused(wire, "inside blirp's data folder"));
            }
        }
        Ok(())
    }

    /// The parent folder of `wire`, every component a real folder (created
    /// when missing with `create`). `None`: missing and not created.
    fn parent(&self, wire: &str, create: bool) -> Result<Option<PathBuf>, WriteError> {
        let mut cur = self.root.to_path_buf();
        let parts: Vec<&str> = wire.split('/').collect();
        for c in &parts[..parts.len() - 1] {
            let next = cur.join(c);
            match std::fs::symlink_metadata(&next) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(refused(wire, "a parent folder is a symlink"));
                }
                Ok(m) if m.is_dir() => {}
                Ok(_) => return Err(WriteError::Changed(wire.to_string())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if !create {
                        return Ok(None);
                    }
                    match std::fs::create_dir(&next) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(io(wire)(e)),
                    }
                    // Whatever is there now must be a real folder.
                    let m = std::fs::symlink_metadata(&next).map_err(io(wire))?;
                    if m.file_type().is_symlink() || !m.is_dir() {
                        return Err(refused(wire, "a parent folder is a symlink"));
                    }
                }
                Err(e) => return Err(io(wire)(e)),
            }
            cur = next;
        }
        Ok(Some(cur))
    }

    fn retry<T>(&self, path: &Path, f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
        match self.retry {
            Some(b) => retry_busy_at(b, Some(path), f),
            None => retry_busy_at(&RetryBudget::new(RETRY_BUDGET), Some(path), f),
        }
    }

    /// Whether the file at `target` is what `expect` says.
    fn matches(&self, wire: &str, target: &Path, expect: &Expect) -> Result<bool, WriteError> {
        // A file deleted while a scanner still has it open lingers as
        // "delete pending" and answers access denied for a moment.
        let meta = match self.retry(target, || std::fs::symlink_metadata(target)) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io(wire)(e)),
        };
        Ok(match (expect, meta) {
            (Expect::Any, _) => true,
            (Expect::Absent, m) => m.is_none(),
            (Expect::Blob(h), Some(m)) if m.is_file() => {
                self.retry(target, || super::scan::hash_file(target))
                    .map_err(io(wire))?
                    .0
                    == *h
            }
            (Expect::Link(t), Some(m)) if m.file_type().is_symlink() => std::fs::read_link(target)
                .ok()
                .and_then(|p| p.to_str().map(|s| s.replace('\\', "/")))
                .is_some_and(|s| s == *t),
            _ => false,
        })
    }

    /// Current state of `wire`, re-checked against `expect`.
    pub fn is(&self, wire: &str, expect: &Expect) -> Result<bool, WriteError> {
        self.check_wire(wire)?;
        let Some(parent) = self.parent(wire, false)? else {
            return Ok(matches!(expect, Expect::Absent | Expect::Any));
        };
        let name = wire.rsplit('/').next().unwrap_or(wire);
        self.matches(wire, &parent.join(name), expect)
    }

    /// Write `src` to `wire` if the target is still `expect`.
    pub fn write(
        &self,
        wire: &str,
        expect: &Expect,
        src: &mut dyn Read,
        mode_x: bool,
        mtime_ms: Option<i64>,
    ) -> Result<(), WriteError> {
        self.write_inner(wire, expect, src, None, mode_x, mtime_ms)
    }

    /// [`Self::write`] of content that must hash to `hash` (checked before
    /// anything is moved into place).
    pub fn write_verified(
        &self,
        wire: &str,
        expect: &Expect,
        src: &mut dyn Read,
        hash: &str,
        mode_x: bool,
        mtime_ms: Option<i64>,
    ) -> Result<(), WriteError> {
        self.write_inner(wire, expect, src, Some(hash), mode_x, mtime_ms)
    }

    fn write_inner(
        &self,
        wire: &str,
        expect: &Expect,
        src: &mut dyn Read,
        verify: Option<&str>,
        mode_x: bool,
        mtime_ms: Option<i64>,
    ) -> Result<(), WriteError> {
        self.check_wire(wire)?;
        let parent = self
            .parent(wire, true)?
            .ok_or_else(|| WriteError::Changed(wire.to_string()))?;
        let name = wire.rsplit('/').next().unwrap_or(wire);
        let target = parent.join(name);
        if !self.matches(wire, &target, expect)? {
            return Err(WriteError::Changed(wire.to_string()));
        }
        let tmp = parent.join(format!(
            "{TMP_PREFIX}{}",
            crate::random_hex::<8>().map_err(io(wire))?
        ));
        let result = (|| {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(io(wire))?;
            let mut hasher = blake3::Hasher::new();
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = src.read(&mut buf).map_err(io(wire))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                f.write_all(&buf[..n]).map_err(io(wire))?;
            }
            f.flush().map_err(io(wire))?;
            if verify.is_some_and(|h| hasher.finalize().to_hex().as_str() != h) {
                return Err(refused(wire, "the content does not match its hash"));
            }
            #[cfg(unix)]
            if mode_x {
                use std::os::unix::fs::PermissionsExt;
                let mut p = f.metadata().map_err(io(wire))?.permissions();
                p.set_mode(p.mode() | ((p.mode() & 0o444) >> 2));
                f.set_permissions(p).map_err(io(wire))?;
            }
            #[cfg(not(unix))]
            let _ = mode_x;
            // A time the OS cannot represent is left as it is (now).
            if let Some(t) = mtime_ms.and_then(|m| u64::try_from(m).ok()).and_then(|ms| {
                std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ms))
            }) {
                f.set_modified(t).map_err(io(wire))?;
            }
            f.sync_all().map_err(io(wire))?;
            drop(f);
            self.place(wire, &tmp, &target, expect)
        })();
        if tmp.exists() {
            let _ = std::fs::remove_file(&tmp);
        }
        if result.is_ok() {
            sync_dir(&parent);
        }
        result
    }

    /// Move the finished temp file onto `target` after checking once more.
    fn place(
        &self,
        wire: &str,
        tmp: &Path,
        target: &Path,
        expect: &Expect,
    ) -> Result<(), WriteError> {
        let stat = |p: &Path| {
            std::fs::symlink_metadata(p)
                .ok()
                .map(|m| (m.len(), m.modified().ok()))
        };
        let before = stat(target);
        if !self.matches(wire, target, expect)? {
            return Err(WriteError::Changed(wire.to_string()));
        }
        // Written to while it was being checked: leave it alone.
        if stat(target) != before {
            return Err(WriteError::Changed(wire.to_string()));
        }
        if *expect == Expect::Absent {
            match std::fs::hard_link(tmp, target) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(WriteError::Changed(wire.to_string()));
                }
                // No hard links on this filesystem (FAT, some network
                // shares): fall back to rename after the check above.
                Err(_) => {}
            }
        }
        self.retry(target, || replace(tmp, target))
            .map_err(io(wire))
    }

    /// Create or replace a symlink (unix; Windows links are not recreated).
    pub fn link(&self, wire: &str, expect: &Expect, link_target: &str) -> Result<(), WriteError> {
        self.check_wire(wire)?;
        if super::scan::link_inside(wire, Path::new(link_target)).is_none() {
            return Err(refused(wire, "the link leads outside the folder"));
        }
        if cfg!(not(unix)) {
            return Err(refused(wire, "symlinks are not recreated on this system"));
        }
        let parent = self
            .parent(wire, true)?
            .ok_or_else(|| WriteError::Changed(wire.to_string()))?;
        let name = wire.rsplit('/').next().unwrap_or(wire);
        let target = parent.join(name);
        let tmp = parent.join(format!(
            "{TMP_PREFIX}{}",
            crate::random_hex::<8>().map_err(io(wire))?
        ));
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(link_target, &tmp).map_err(io(wire));
        #[cfg(not(unix))]
        let made: Result<(), WriteError> =
            Err(refused(wire, "symlinks are not recreated on this system"));
        let result = made.and_then(|()| {
            if self.matches(wire, &target, expect)? {
                self.retry(&target, || std::fs::rename(&tmp, &target))
                    .map_err(io(wire))
            } else {
                Err(WriteError::Changed(wire.to_string()))
            }
        });
        if std::fs::symlink_metadata(&tmp).is_ok() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Remove `wire` if it still is `expect` (already gone is fine).
    pub fn remove(&self, wire: &str, expect: &Expect) -> Result<(), WriteError> {
        self.check_wire(wire)?;
        let Some(parent) = self.parent(wire, false)? else {
            return Ok(());
        };
        let name = wire.rsplit('/').next().unwrap_or(wire);
        let target = parent.join(name);
        match std::fs::symlink_metadata(&target) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io(wire)(e)),
            Ok(m) if m.is_dir() => return Err(WriteError::Changed(wire.to_string())),
            Ok(_) => {}
        }
        if !self.matches(wire, &target, expect)? {
            return Err(WriteError::Changed(wire.to_string()));
        }
        match self.retry(&target, || remove(&target)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io(wire)(e)),
        }
    }
}

/// Windows can refuse to replace or delete a file (access denied) that it
/// still lets be renamed: Defender's real-time protection does that to
/// files it is looking at, for seconds at a time under load. Such a
/// target is moved aside under a temp name first (never a read-only one,
/// which stays refused).
fn denied_but_movable(e: &std::io::Error, target: &Path) -> Option<PathBuf> {
    if !cfg!(windows) || e.kind() != std::io::ErrorKind::PermissionDenied {
        return None;
    }
    if std::fs::symlink_metadata(target).map_or(true, |m| m.permissions().readonly()) {
        return None;
    }
    let aside = target
        .parent()?
        .join(format!("{TMP_PREFIX}{}", crate::random_hex::<8>().ok()?));
    std::fs::rename(target, &aside).ok()?;
    Some(aside)
}

/// Move `tmp` onto `target`; see [`denied_but_movable`]. What was moved
/// aside is removed, or left to the scan's sweep of old temp files while
/// it is still held; it comes back if the new file cannot go in.
fn replace(tmp: &Path, target: &Path) -> std::io::Result<()> {
    let e = match std::fs::rename(tmp, target) {
        Err(e) => e,
        ok => return ok,
    };
    let Some(aside) = denied_but_movable(&e, target) else {
        return Err(e);
    };
    match std::fs::rename(tmp, target) {
        Ok(()) => {
            let _ = std::fs::remove_file(&aside);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::rename(&aside, target);
            Err(e)
        }
    }
}

/// Remove `target`; see [`denied_but_movable`].
fn remove(target: &Path) -> std::io::Result<()> {
    let e = match std::fs::remove_file(target) {
        Err(e) => e,
        ok => return ok,
    };
    let Some(aside) = denied_but_movable(&e, target) else {
        return Err(e);
    };
    let _ = std::fs::remove_file(&aside);
    Ok(())
}

/// Time Windows retries may spend in all: one per pass (an apply, a
/// restore), so many files denied at once cost the budget once, not each.
#[derive(Debug)]
pub struct RetryBudget {
    left_ms: std::sync::atomic::AtomicU64,
}

/// What one pass may wait for files other processes hold.
pub const RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

impl RetryBudget {
    pub fn new(total: std::time::Duration) -> Self {
        Self {
            left_ms: std::sync::atomic::AtomicU64::new(
                u64::try_from(total.as_millis()).unwrap_or(u64::MAX),
            ),
        }
    }

    /// Start a new pass with `total`.
    pub fn reset(&self, total: std::time::Duration) {
        self.left_ms.store(
            u64::try_from(total.as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Spend `ms` if that much is left.
    fn take(&self, ms: u64) -> bool {
        self.left_ms
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |left| left.checked_sub(ms),
            )
            .is_ok()
    }
}

/// [`retry_busy_at`] with a budget of its own.
pub fn retry_busy<T>(f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    retry_busy_at(&RetryBudget::new(RETRY_BUDGET), None, f)
}

/// Windows refuses to replace or delete a file another process has open
/// without delete sharing (virus scanners and indexers open fresh files
/// briefly, sometimes for over a second under load), and a file just
/// removed can linger "delete pending": retry with a growing pause while
/// `budget` lasts. A lasting denial (`path` is read-only) is reported at
/// once; when the budget runs out, what the path looked like is logged.
pub fn retry_busy_at<T>(
    budget: &RetryBudget,
    path: Option<&Path>,
    mut f: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut pause_ms = 20u64;
    loop {
        let e = match f() {
            Err(e)
                if cfg!(windows)
                    // Access denied, or ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION.
                    && (e.kind() == std::io::ErrorKind::PermissionDenied
                        || matches!(e.raw_os_error(), Some(32 | 33))) =>
            {
                e
            }
            r => return r,
        };
        let meta = path.map(std::fs::symlink_metadata);
        if let Some(Ok(m)) = &meta
            && m.permissions().readonly()
        {
            return Err(e);
        }
        if !budget.take(pause_ms) {
            tracing::warn!(
                path = ?path,
                os_error = ?e.raw_os_error(),
                attributes = ?meta.as_ref().map(|m| m.as_ref().map(attributes).map_err(|e| e.raw_os_error())),
                "a file stayed locked; giving up for this pass"
            );
            return Err(e);
        }
        std::thread::sleep(std::time::Duration::from_millis(pause_ms));
        pause_ms = (pause_ms * 2).min(250);
    }
}

/// File attributes for diagnostics (Windows), else the mode bits.
fn attributes(m: &std::fs::Metadata) -> u32 {
    #[cfg(windows)]
    {
        std::os::windows::fs::MetadataExt::file_attributes(m)
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::PermissionsExt::mode(&m.permissions())
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = m;
        0
    }
}

/// Make a rename durable (unix; Windows has no directory handles for this).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::hash_bytes;

    fn t(root: &Path) -> Target<'_> {
        Target {
            root,
            data_dir: None,
            retry: None,
        }
    }

    #[test]
    fn writes_new_and_replaces_only_what_was_expected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let w = t(&root);
        w.write(
            "a/b/c.txt",
            &Expect::Absent,
            &mut &b"one"[..],
            false,
            Some(1_000_000),
        )
        .unwrap();
        assert_eq!(std::fs::read(root.join("a/b/c.txt")).unwrap(), b"one");
        let m = std::fs::metadata(root.join("a/b/c.txt")).unwrap();
        assert_eq!(
            m.modified().unwrap(),
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_000_000)
        );
        // Not absent anymore.
        assert!(matches!(
            w.write("a/b/c.txt", &Expect::Absent, &mut &b"x"[..], false, None),
            Err(WriteError::Changed(_))
        ));
        // Replaced only when it is the expected content.
        assert!(matches!(
            w.write(
                "a/b/c.txt",
                &Expect::Blob(hash_bytes(b"other")),
                &mut &b"x"[..],
                false,
                None
            ),
            Err(WriteError::Changed(_))
        ));
        w.write(
            "a/b/c.txt",
            &Expect::Blob(hash_bytes(b"one")),
            &mut &b"two"[..],
            false,
            None,
        )
        .unwrap();
        assert_eq!(std::fs::read(root.join("a/b/c.txt")).unwrap(), b"two");
        assert!(
            w.is("a/b/c.txt", &Expect::Blob(hash_bytes(b"two")))
                .unwrap()
        );
        // Content that does not match its hash never lands.
        assert!(matches!(
            w.write_verified(
                "v.txt",
                &Expect::Absent,
                &mut &b"x"[..],
                &hash_bytes(b"y"),
                false,
                None
            ),
            Err(WriteError::Refused { .. })
        ));
        assert!(!root.join("v.txt").exists());
        w.write_verified(
            "v.txt",
            &Expect::Absent,
            &mut &b"x"[..],
            &hash_bytes(b"x"),
            false,
            None,
        )
        .unwrap();
        std::fs::remove_file(root.join("v.txt")).unwrap();
        // No temp files left behind.
        let left: Vec<_> = std::fs::read_dir(root.join("a/b")).unwrap().collect();
        assert_eq!(left.len(), 1);
        // Remove only when unchanged.
        assert!(matches!(
            w.remove("a/b/c.txt", &Expect::Blob(hash_bytes(b"one"))),
            Err(WriteError::Changed(_))
        ));
        w.remove("a/b/c.txt", &Expect::Blob(hash_bytes(b"two")))
            .unwrap();
        assert!(!root.join("a/b/c.txt").exists());
        w.remove("a/b/c.txt", &Expect::Any).unwrap();
        w.remove("nope/x", &Expect::Any).unwrap();
    }

    #[test]
    fn unsafe_paths_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let w = t(&root);
        for bad in [
            "../x",
            ".git/hooks/post-checkout",
            "a/.GIT/config",
            "/abs",
            "a\\b",
            ".blirp-tmp-1",
        ] {
            assert!(
                matches!(
                    w.write(bad, &Expect::Absent, &mut &b"x"[..], false, None),
                    Err(WriteError::Refused { .. })
                ),
                "{bad}"
            );
        }
        assert!(!root.join(".git").exists());
        // A file where a folder is expected is not replaced.
        std::fs::write(root.join("f"), "x").unwrap();
        assert!(matches!(
            w.write("f/x", &Expect::Absent, &mut &b"x"[..], false, None),
            Err(WriteError::Changed(_))
        ));
        // The data folder, unless the root is inside it.
        let data = root.join("data");
        std::fs::create_dir(&data).unwrap();
        let guarded = Target {
            root: &root,
            data_dir: Some(&data),
            retry: None,
        };
        assert!(matches!(
            guarded.write("data/x", &Expect::Absent, &mut &b"x"[..], false, None),
            Err(WriteError::Refused { .. })
        ));
        let ws = data.join("workspaces").join("p");
        std::fs::create_dir_all(&ws).unwrap();
        let inside = Target {
            root: &ws,
            data_dir: Some(&data),
            retry: None,
        };
        inside
            .write("x", &Expect::Absent, &mut &b"x"[..], false, None)
            .unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_invalid_names_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        for bad in ["CON", "aux.c", "a.", "x:y"] {
            assert!(
                matches!(
                    t(&root).write(bad, &Expect::Absent, &mut &b"x"[..], false, None),
                    Err(WriteError::Refused { .. })
                ),
                "{bad}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_parents_are_refused_and_links_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap().join("root");
        let outside = root.parent().unwrap().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
        let w = t(&root);
        assert!(matches!(
            w.write("sub/x", &Expect::Absent, &mut &b"x"[..], false, None),
            Err(WriteError::Refused { .. })
        ));
        assert!(!outside.join("x").exists());
        w.write(
            "run.sh",
            &Expect::Absent,
            &mut &b"#!/bin/sh"[..],
            true,
            None,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            std::fs::metadata(root.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        w.link("l", &Expect::Absent, "run.sh").unwrap();
        assert_eq!(
            std::fs::read_link(root.join("l")).unwrap(),
            Path::new("run.sh")
        );
        assert!(matches!(
            w.link("l2", &Expect::Absent, "../../etc"),
            Err(WriteError::Refused { .. })
        ));
    }

    /// A file Windows lets be renamed but not replaced or deleted (here a
    /// running program's image, as real-time protection holds files it
    /// scans) is still replaced and removed, without waiting on it.
    #[cfg(windows)]
    #[test]
    fn a_file_that_cannot_be_replaced_in_place_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let held = dir.path().join("held.exe");
        let system = std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        std::fs::copy(system.join("System32").join("PING.EXE"), &held).unwrap();
        let mut run = crate::process::command(&held)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // The signature seen under load: replace denied, rename allowed.
        let probe = dir.path().join("probe");
        std::fs::write(&probe, "x").unwrap();
        let denied = std::fs::rename(&probe, &held).unwrap_err();
        assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);

        let w = t(dir.path());
        let started = std::time::Instant::now();
        let new = b"new content";
        w.write_verified(
            "held.exe",
            &Expect::Any,
            &mut &new[..],
            &hash_bytes(new),
            false,
            None,
        )
        .unwrap();
        assert_eq!(std::fs::read(&held).unwrap(), new);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));

        // Deleting such a file works too: it is moved aside.
        std::fs::copy(
            system.join("System32").join("PING.EXE"),
            dir.path().join("b.exe"),
        )
        .unwrap();
        let mut run2 = crate::process::command(dir.path().join("b.exe"))
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        w.remove("b.exe", &Expect::Any).unwrap();
        assert!(!dir.path().join("b.exe").exists());
        for r in [&mut run, &mut run2] {
            let _ = r.kill();
            let _ = r.wait();
        }
    }
}
