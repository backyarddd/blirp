//! `BLIRP_HOME` layout (§3) and `runtime.json`.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const HOME_ENV: &str = "BLIRP_HOME";

#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    #[error("cannot determine the user's home directory; set {HOME_ENV}")]
    NoHome,
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid runtime file {path}: {source}")]
    Runtime {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> PathsError {
    let path = path.to_path_buf();
    move |source| PathsError::Io {
        action,
        path,
        source,
    }
}

/// The user's home directory (`%USERPROFILE%` on Windows, `$HOME` on unix).
pub fn user_home() -> Option<PathBuf> {
    std::env::home_dir().filter(|p| !p.as_os_str().is_empty())
}

#[derive(Debug, Clone)]
pub struct Paths {
    home: PathBuf,
}

impl Paths {
    /// `$BLIRP_HOME` if set and non-empty, else `~/.blirp`.
    pub fn resolve() -> Result<Self, PathsError> {
        match std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
            Some(v) => Ok(Self::at(PathBuf::from(v))),
            None => Ok(Self::at(
                user_home().ok_or(PathsError::NoHome)?.join(".blirp"),
            )),
        }
    }

    pub fn at(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }
    pub fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }
    pub fn db_file(&self) -> PathBuf {
        self.home.join("blirp.db")
    }
    pub fn runtime_file(&self) -> PathBuf {
        self.home.join("runtime.json")
    }
    pub fn identity_key(&self) -> PathBuf {
        self.home.join("identity.key")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.home.join("daemon.lock")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.home.join("logs")
    }
    pub fn worktrees_dir(&self) -> PathBuf {
        self.home.join("worktrees")
    }
    pub fn launch_dir(&self, session_id: &str) -> PathBuf {
        self.home.join("launch").join(session_id)
    }

    /// Create the data dir and its fixed subdirectories.
    pub fn ensure_dirs(&self) -> Result<(), PathsError> {
        for dir in [
            self.home.clone(),
            self.logs_dir(),
            self.worktrees_dir(),
            self.home.join("launch"),
        ] {
            std::fs::create_dir_all(&dir).map_err(io("create", &dir))?;
        }
        Ok(())
    }
}

/// Contents of `runtime.json`: how local clients find and authenticate to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub pid: u32,
    pub port: u16,
    pub token: String,
    pub version: String,
    pub started_at: i64,
}

impl RuntimeInfo {
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn read(paths: &Paths) -> Result<Option<Self>, PathsError> {
        let path = paths.runtime_file();
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|source| PathsError::Runtime { path, source }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io("read", &path)(e)),
        }
    }

    /// Atomically write `runtime.json` with owner-only permissions. On Windows
    /// the file inherits the ACL of the user profile directory, which already
    /// restricts access to the user.
    pub fn write(&self, paths: &Paths) -> Result<(), PathsError> {
        let path = paths.runtime_file();
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(self).map_err(|source| PathsError::Runtime {
            path: path.clone(),
            source,
        })?;
        write_private(&tmp, &json)?;
        std::fs::rename(&tmp, &path).map_err(io("rename", &path))
    }

    /// Remove `runtime.json` if it still describes this process.
    pub fn remove_if_owned(paths: &Paths, pid: u32) -> Result<(), PathsError> {
        if matches!(Self::read(paths), Ok(Some(info)) if info.pid == pid) {
            let path = paths.runtime_file();
            std::fs::remove_file(&path).map_err(io("remove", &path))?;
        }
        Ok(())
    }
}

/// Write a file readable only by the current user (0600 on unix).
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), PathsError> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(io("open", path))?;
    f.write_all(bytes).map_err(io("write", path))?;
    f.sync_all().map_err(io("sync", path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        paths.ensure_dirs().unwrap();
        assert_eq!(RuntimeInfo::read(&paths).unwrap(), None);
        let info = RuntimeInfo {
            pid: 42,
            port: 47770,
            token: "ab".repeat(32),
            version: "0.1.0".into(),
            started_at: 1,
        };
        info.write(&paths).unwrap();
        assert_eq!(RuntimeInfo::read(&paths).unwrap(), Some(info));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths.runtime_file())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        RuntimeInfo::remove_if_owned(&paths, 7).unwrap();
        assert!(paths.runtime_file().exists());
        RuntimeInfo::remove_if_owned(&paths, 42).unwrap();
        assert!(!paths.runtime_file().exists());
    }
}
