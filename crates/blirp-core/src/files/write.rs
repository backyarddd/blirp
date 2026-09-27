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

    /// Whether the file at `target` is what `expect` says.
    fn matches(&self, wire: &str, target: &Path, expect: &Expect) -> Result<bool, WriteError> {
        // A file deleted while a scanner still has it open lingers as
        // "delete pending" and answers access denied for a moment.
        let meta = match retry_busy(|| std::fs::symlink_metadata(target)) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io(wire)(e)),
        };
        Ok(match (expect, meta) {
            (Expect::Any, _) => true,
            (Expect::Absent, m) => m.is_none(),
            (Expect::Blob(h), Some(m)) if m.is_file() => {
                retry_busy(|| super::scan::hash_file(target))
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
        retry_busy(|| std::fs::rename(tmp, target)).map_err(io(wire))
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
                retry_busy(|| std::fs::rename(&tmp, &target)).map_err(io(wire))
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
        match retry_busy(|| std::fs::remove_file(&target)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io(wire)(e)),
        }
    }
}

/// Windows refuses to replace or delete a file another process has open
/// without delete sharing (virus scanners and indexers open fresh files
/// briefly): retry a few times before reporting it.
fn retry_busy<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut left = if cfg!(windows) { 20 } else { 0 };
    loop {
        match f() {
            // Access denied, or ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION.
            Err(e)
                if left > 0
                    && (e.kind() == std::io::ErrorKind::PermissionDenied
                        || matches!(e.raw_os_error(), Some(32 | 33))) =>
            {
                left -= 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            r => return r,
        }
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
}
