//! Helpers for running short-lived child processes (git, `--version` probes).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `std::process::Command` that never flashes a console window on Windows.
/// The daemon may run detached without a console; without `CREATE_NO_WINDOW`
/// every git call would pop up a window.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[derive(Debug)]
pub struct Output {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("`{0}` not found")]
    NotFound(String),
    #[error("`{program}` timed out after {secs}s")]
    Timeout { program: String, secs: u64 },
    #[error("failed to run `{program}`: {source}")]
    Io {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

/// Run `cmd` with stdin closed, capture output (capped at `max_output` bytes per
/// stream) and kill it after `timeout`.
pub fn run(mut cmd: Command, timeout: Duration, max_output: usize) -> Result<Output, RunError> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => RunError::NotFound(program.clone()),
        _ => RunError::Io {
            program: program.clone(),
            source: e,
        },
    })?;
    // Drain both pipes on threads so a chatty child can never block on a full pipe.
    let out = child.stdout.take().map(|s| drain(s, max_output));
    let err = child.stderr.take().map(|s| drain(s, max_output));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                if let Err(e) = child.kill() {
                    tracing::warn!(%program, error = %e, "failed to kill timed-out child");
                }
                // Reap it so no zombie is left behind; the result is irrelevant.
                let _ = child.wait();
                return Err(RunError::Timeout {
                    program,
                    secs: timeout.as_secs(),
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(source) => return Err(RunError::Io { program, source }),
        }
    };
    let join = |h: Option<std::thread::JoinHandle<Vec<u8>>>| {
        h.and_then(|h| h.join().ok()).unwrap_or_default()
    };
    Ok(Output {
        status: status.code(),
        stdout: join(out),
        stderr: join(err),
    })
}

fn drain(mut r: impl Read + Send + 'static, cap: usize) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
        buf
    })
}

/// Resolve a program name against `PATH`. On Windows only real launchable
/// files count (`.exe`, `.com`, `.cmd`, `.bat`, then `.ps1`); the extensionless
/// POSIX shims npm drops next to `.cmd` shims are skipped.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    which_in(name, std::env::split_paths(&path))
}

pub fn which_in(name: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    if name.contains(['/', '\\']) {
        let p = Path::new(name);
        return p.is_file().then(|| p.to_path_buf());
    }
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".com", ".cmd", ".bat", ".ps1"]
    } else {
        &[""]
    };
    let dirs: Vec<PathBuf> = dirs.into_iter().collect();
    // Extension priority beats PATH order on Windows, so a `.ps1` early in PATH
    // never shadows a `.cmd` shim for the same tool.
    for ext in exts {
        for dir in &dirs {
            let has_ext = Path::new(name).extension().is_some();
            let candidate = if has_ext && cfg!(windows) {
                dir.join(name)
            } else {
                dir.join(format!("{name}{ext}"))
            };
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_skips_extensionless_shims_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tool"), "#!/bin/sh").unwrap();
        #[cfg(windows)]
        {
            std::fs::write(dir.path().join("tool.ps1"), "").unwrap();
            std::fs::write(dir.path().join("tool.cmd"), "").unwrap();
            let found = which_in("tool", [dir.path().to_path_buf()]).unwrap();
            assert_eq!(found.extension().unwrap(), "cmd");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.path().join("tool");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(which_in("tool", [dir.path().to_path_buf()]).unwrap(), p);
        }
        assert!(which_in("missing", [dir.path().to_path_buf()]).is_none());
    }

    #[test]
    fn run_times_out() {
        let cmd = if cfg!(windows) {
            let mut c = command("powershell");
            c.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 10"]);
            c
        } else {
            let mut c = command("sleep");
            c.arg("10");
            c
        };
        let err = run(cmd, Duration::from_millis(300), 1024).unwrap_err();
        assert!(matches!(err, RunError::Timeout { .. }));
    }
}
