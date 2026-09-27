//! Files pasted or dropped into a terminal (§6): saved under
//! `BLIRP_HOME/uploads/<session>/` on the machine that runs the session, so
//! the agent there can open them by path. Owner-only, never in the database
//! (so never replicated), removed with the session and after `RETENTION`.

use crate::state::SharedState;
use anyhow::Context as _;
use blirp_core::paths::{Paths, create_private_dir};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Largest file accepted, enforced before anything is written.
pub const MAX_BYTES: usize = 25 << 20;
/// Most bytes one session's upload folder may hold.
pub const SESSION_QUOTA: u64 = 500 << 20;
/// Uploads older than this are deleted by the prune task.
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
/// Longest sanitized file name in bytes (file systems allow 255 per name,
/// and the timestamp prefix comes on top).
const MAX_NAME_BYTES: usize = 120;
/// Longest extension kept when a long name is shortened.
const MAX_EXT_BYTES: usize = 16;

/// A safe single file name from whatever the client sent: only its last
/// path component, letters, digits, `.`, `-` and `_` (anything else becomes
/// `_`), no leading or trailing dots, at most `MAX_NAME_BYTES` with the
/// extension kept. Never empty, never `.` or `..`, never a path.
pub fn sanitize_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut clean = String::with_capacity(base.len());
    let mut replaced = false;
    for c in base.chars() {
        if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
            clean.push(c);
            replaced = false;
        } else if !replaced {
            // A run of unsafe characters becomes one `_`.
            clean.push('_');
            replaced = true;
        }
    }
    // No hidden files, no `.`/`..`, no trailing dots (Windows drops them).
    let clean = clean.trim_matches('.');
    if clean.is_empty() {
        return "file".into();
    }
    if clean.len() <= MAX_NAME_BYTES {
        return clean.to_string();
    }
    let (stem, ext) = match clean.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && e.len() <= MAX_EXT_BYTES => (s, Some(e)),
        _ => (clean, None),
    };
    let budget = MAX_NAME_BYTES - ext.map_or(0, |e| e.len() + 1);
    let mut end = budget.min(stem.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    let stem = stem[..end].trim_end_matches('.');
    match ext {
        Some(e) => format!("{stem}.{e}"),
        None => stem.to_string(),
    }
}

/// `path` the way dropping a file onto a terminal types it: Windows
/// Terminal wraps paths in double quotes, macOS and Linux terminals escape
/// with backslashes. Agents (Claude Code, Codex) recognize both forms. A
/// path that needs neither stays unchanged. Names are sanitized, so only a
/// data dir with spaces or symbols ever needs quoting.
pub fn quote_path(path: &str, windows: bool) -> String {
    let plain = |c: char| c.is_alphanumeric() || matches!(c, '/' | '.' | '-' | '_' | ':');
    if windows {
        // Windows paths cannot contain `"`, so wrapping is always enough.
        if path.chars().all(|c| plain(c) || c == '\\') {
            path.to_string()
        } else {
            format!("\"{path}\"")
        }
    } else {
        let mut out = String::with_capacity(path.len());
        for c in path.chars() {
            if !plain(c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }
}

/// The session's upload folder is full (`SESSION_QUOTA`).
#[derive(Debug, thiserror::Error)]
#[error("this session's uploads would exceed {} MB; delete the session or wait for old uploads to expire", .0 >> 20)]
pub struct QuotaExceeded(pub u64);

/// Write `bytes` to a new owner-only file `uploads/<session>/<ms>-<name>`
/// and return its absolute path. Never overwrites: a name taken in the same
/// millisecond gets a counter. Refused with `QuotaExceeded` when the folder
/// would hold more than `SESSION_QUOTA`.
pub fn save(
    paths: &Paths,
    session: &str,
    name: &str,
    bytes: &[u8],
    now_ms: i64,
) -> anyhow::Result<PathBuf> {
    save_within(paths, session, name, bytes, now_ms, SESSION_QUOTA)
}

fn save_within(
    paths: &Paths,
    session: &str,
    name: &str,
    bytes: &[u8],
    now_ms: i64,
    quota: u64,
) -> anyhow::Result<PathBuf> {
    let dir = paths.session_uploads_dir(session)?;
    let name = sanitize_name(name);
    create_private_dir(&dir)?;
    // Note: check then write, so concurrent uploads to one session can
    // overshoot by the files in flight (the SPA uploads one at a time); a
    // per-session lock if that ever matters.
    if used_bytes(&dir)? + bytes.len() as u64 > quota {
        return Err(QuotaExceeded(quota).into());
    }
    let mut recreated = false;
    let mut n = 0u32;
    loop {
        let file = if n == 0 {
            format!("{now_ms}-{name}")
        } else {
            format!("{now_ms}-{n}-{name}")
        };
        let path = dir.join(file);
        match create_new_private(&path) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(bytes).and_then(|()| f.sync_all()) {
                    drop(f);
                    if let Err(re) = std::fs::remove_file(&path) {
                        tracing::warn!(error = %re, "removing a partial upload failed");
                    }
                    return Err(e).with_context(|| format!("write {}", path.display()));
                }
                return std::path::absolute(&path)
                    .with_context(|| format!("absolute path of {}", path.display()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
            // The prune task removed the empty folder in between.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !recreated => {
                recreated = true;
                create_private_dir(&dir)?;
            }
            Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
        }
    }
}

/// Bytes in the regular files of `dir`.
fn used_bytes(dir: &Path) -> anyhow::Result<u64> {
    let mut used = 0;
    for f in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let f = f.with_context(|| format!("read {}", dir.display()))?;
        if let Ok(m) = f.metadata()
            && m.is_file()
        {
            used += m.len();
        }
    }
    Ok(used)
}

fn create_new_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Delete a session's uploads (the session was deleted).
pub fn remove_session(paths: &Paths, session: &str) {
    let Ok(dir) = paths.session_uploads_dir(session) else {
        return;
    };
    if let Err(e) = std::fs::remove_dir_all(&dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(session, error = %e, "removing uploads failed");
    }
}

/// Delete uploads last modified before `cutoff` and session folders left
/// empty. Only regular files directly inside `uploads/<session>/` folders
/// are touched; symlinks are never followed. Returns the files removed.
pub fn prune(paths: &Paths, cutoff: SystemTime) -> usize {
    let root = paths.uploads_dir();
    let sessions = match std::fs::read_dir(&root) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            tracing::warn!(error = %e, "reading the uploads folder failed");
            return 0;
        }
    };
    let mut removed = 0;
    for dir in sessions.flatten() {
        if !dir.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for f in files.flatten() {
            let old = f
                .metadata()
                .is_ok_and(|m| m.file_type().is_file() && m.modified().is_ok_and(|t| t < cutoff));
            if !old {
                continue;
            }
            match std::fs::remove_file(f.path()) {
                Ok(()) => removed += 1,
                Err(e) => tracing::warn!(error = %e, "pruning an upload failed"),
            }
        }
        // Fails while files remain, which is fine.
        let _ = std::fs::remove_dir(dir.path());
    }
    removed
}

/// Prune at start and then hourly until shutdown.
pub fn start_pruning(state: SharedState) {
    let mut shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(PRUNE_EVERY);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let paths = state.paths.clone();
                    let run = tokio::task::spawn_blocking(move || {
                        let cutoff = SystemTime::now()
                            .checked_sub(RETENTION)
                            .unwrap_or(SystemTime::UNIX_EPOCH);
                        prune(&paths, cutoff)
                    });
                    match run.await {
                        Ok(0) => {}
                        Ok(n) => tracing::info!(files = n, "pruned old uploads"),
                        Err(e) => tracing::error!(error = %e, "upload prune task failed"),
                    }
                }
                _ = shutdown.changed() => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn names_are_single_safe_components() {
        for (raw, want) in [
            ("shot.png", "shot.png"),
            (
                "Screen Shot 2026-01-02 at 10.00.00.png",
                "Screen_Shot_2026-01-02_at_10.00.00.png",
            ),
            ("../../etc/passwd", "passwd"),
            ("..\\..\\Windows\\win.ini", "win.ini"),
            ("C:\\Users\\x\\a b.txt", "a_b.txt"),
            ("/abs/path/", "file"),
            ("..", "file"),
            (".", "file"),
            ("", "file"),
            (".bashrc", "bashrc"),
            ("name.", "name"),
            ("a:b|c?*<>\"d.jpg", "a_b_c_d.jpg"),
            ("\u{202e}gpj.exe", "_gpj.exe"),
            ("__init__.py", "__init__.py"),
            ("image.PNG", "image.PNG"),
            ("日本語.png", "日本語.png"),
        ] {
            assert_eq!(sanitize_name(raw), want, "{raw:?}");
        }
        let long = format!("{}.png", "a".repeat(500));
        let s = sanitize_name(&long);
        assert_eq!(s.len(), MAX_NAME_BYTES);
        assert!(s.ends_with(".png"));
        let wide = "é".repeat(200);
        let s = sanitize_name(&wide);
        assert!(s.len() <= MAX_NAME_BYTES && s.chars().all(|c| c == 'é'));
    }

    #[test]
    fn quoting_follows_the_platform() {
        let p = "/home/u/.blirp/uploads/s/1-a.png";
        assert_eq!(quote_path(p, false), p);
        assert_eq!(
            quote_path("/home/Jane Doe/x (1).png", false),
            "/home/Jane\\ Doe/x\\ \\(1\\).png"
        );
        let w = "C:\\Users\\u\\.blirp\\uploads\\s\\1-a.png";
        assert_eq!(quote_path(w, true), w);
        assert_eq!(
            quote_path("C:\\Users\\Jane Doe\\a.png", true),
            "\"C:\\Users\\Jane Doe\\a.png\""
        );
    }

    #[test]
    fn saves_inside_the_session_folder_only() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::at(tmp.path());
        let root = std::path::absolute(paths.uploads_dir().join("S1")).unwrap();
        for name in [
            "../../evil.txt",
            "..\\..\\evil.txt",
            "/etc/evil.txt",
            "..",
            "",
        ] {
            let p = save(&paths, "S1", name, b"x", 42).unwrap();
            assert_eq!(p.parent().unwrap(), root, "{name:?}");
            assert!(p.is_absolute());
        }
        assert!(!tmp.path().join("evil.txt").exists());
        // Same name in the same millisecond: a new file, never an overwrite.
        let a = save(&paths, "S1", "a.png", b"one", 7).unwrap();
        let b = save(&paths, "S1", "a.png", b"two", 7).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.file_name().unwrap(), "7-a.png");
        assert_eq!(b.file_name().unwrap(), "7-1-a.png");
        assert_eq!(std::fs::read(&a).unwrap(), b"one");
        // Session ids that are not one safe component are refused.
        for bad in ["../S1", "a/b", "", "S1\\x"] {
            assert!(save(&paths, bad, "a.png", b"x", 1).is_err(), "{bad:?}");
        }
        remove_session(&paths, "S1");
        assert!(!root.exists());
    }

    #[test]
    fn a_full_session_folder_refuses_more() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::at(tmp.path());
        save_within(&paths, "S1", "a.bin", &[0; 60], 1, 100).unwrap();
        let err = save_within(&paths, "S1", "b.bin", &[0; 41], 2, 100).unwrap_err();
        assert!(err.downcast_ref::<QuotaExceeded>().is_some(), "{err:#}");
        // Exactly full is fine; other sessions have their own quota.
        save_within(&paths, "S1", "c.bin", &[0; 40], 3, 100).unwrap();
        save_within(&paths, "S2", "d.bin", &[0; 100], 4, 100).unwrap();
        assert_eq!(
            std::fs::read_dir(paths.uploads_dir().join("S1"))
                .unwrap()
                .count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn uploads_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::at(tmp.path());
        let p = save(&paths, "S1", "a.png", b"x", 1).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&p), 0o600);
        assert_eq!(mode(p.parent().unwrap()), 0o700);
    }

    #[test]
    fn prune_removes_old_files_and_empty_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::at(tmp.path());
        let old = save(&paths, "S1", "old.png", b"x", 1).unwrap();
        let fresh = save(&paths, "S2", "new.png", b"x", 2).unwrap();
        let week_ago = SystemTime::now() - RETENTION - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(week_ago)
            .unwrap();
        // A file outside any session folder is left alone.
        let stray = paths.uploads_dir().join("stray.txt");
        std::fs::write(&stray, b"x").unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"x").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&outside)
            .unwrap()
            .set_modified(week_ago)
            .unwrap();

        let cutoff = SystemTime::now() - RETENTION;
        assert_eq!(prune(&paths, cutoff), 1);
        assert!(!old.exists());
        assert!(!old.parent().unwrap().exists(), "empty folder removed");
        assert!(fresh.exists());
        assert!(stray.exists() && outside.exists());
        assert_eq!(prune(&Paths::at(tmp.path().join("none")), cutoff), 0);
    }
}
