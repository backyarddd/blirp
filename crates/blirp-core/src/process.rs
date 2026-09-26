//! Helpers for running short-lived child processes (git, `--version` probes).

use crate::proc_tree::ProcessTree;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
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

/// After the child exits, its output pipes get this long to reach EOF. A
/// grandchild that inherited them (a background helper) can keep them open
/// indefinitely; the output read so far is returned then.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Run `cmd` with stdin closed, capture output (capped at `max_output` bytes per
/// stream) and kill its whole process tree after `timeout`.
pub fn run(mut cmd: Command, timeout: Duration, max_output: usize) -> Result<Output, RunError> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own group, so a timeout can kill everything it started.
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => RunError::NotFound(program.clone()),
        _ => RunError::Io {
            program: program.clone(),
            source: e,
        },
    })?;
    #[cfg(windows)]
    let tree = {
        use std::os::windows::io::AsRawHandle;
        ProcessTree::for_process_handle_detached(child.as_raw_handle())
    };
    #[cfg(unix)]
    let tree = ProcessTree::for_process_group(child.id());
    // Drain both pipes on threads so a chatty child can never block on a full pipe.
    let out = child.stdout.take().map(|s| Drain::start(s, max_output));
    let err = child.stderr.take().map(|s| Drain::start(s, max_output));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // The whole tree: killing only the direct child (a `cmd.exe`
                // wrapper, a shell) would leave its children running and
                // holding the pipes.
                tree.force_kill();
                if let Err(e) = child.kill() {
                    tracing::debug!(%program, error = %e, "killing timed-out child");
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
    let drained = Instant::now() + DRAIN_GRACE;
    let collect = |d: Option<Drain>| d.map(|d| d.finish(drained)).unwrap_or_default();
    Ok(Output {
        status: status.code(),
        stdout: collect(out),
        stderr: collect(err),
    })
}

/// A pipe read to EOF on a thread into a shared buffer, so a reader that
/// never sees EOF still yields what arrived.
struct Drain {
    buf: Arc<Mutex<Vec<u8>>>,
    done: mpsc::Receiver<()>,
}

impl Drain {
    fn start(mut r: impl Read + Send + 'static, cap: usize) -> Drain {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let (tx, done) = mpsc::channel();
        let shared = buf.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match r.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut b = shared.lock().unwrap_or_else(PoisonError::into_inner);
                        let room = cap.saturating_sub(b.len());
                        b.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
            // The receiver may have stopped waiting; nothing to report then.
            let _ = tx.send(());
        });
        Drain { buf, done }
    }

    /// Wait for EOF until `deadline`, then take what was read. A reader still
    /// blocked ends by itself once the last writer closes the pipe.
    fn finish(self, deadline: Instant) -> Vec<u8> {
        let _ = self
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
        std::mem::take(&mut *self.buf.lock().unwrap_or_else(PoisonError::into_inner))
    }
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

    // A background grandchild that inherited stdout must not hang the
    // caller once the direct child has exited.
    #[test]
    fn run_returns_when_a_grandchild_keeps_the_pipe_open() {
        let cmd = if cfg!(windows) {
            let mut c = command("cmd.exe");
            c.args(["/d", "/c", "start /b ping -n 30 127.0.0.1 >nul & echo done"]);
            c
        } else {
            let mut c = command("sh");
            c.args(["-c", "sleep 30 & echo done"]);
            c
        };
        let begin = Instant::now();
        let out = run(cmd, Duration::from_secs(20), 1024).unwrap();
        assert!(
            begin.elapsed() < Duration::from_secs(15),
            "{:?}",
            begin.elapsed()
        );
        assert!(out.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("done"));
    }

    // A timeout kills the whole tree, not only the direct child: the
    // grandchild never gets to write its marker.
    #[test]
    fn timeout_kills_the_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        // A background grandchild writes `marker` after 2 s; the direct
        // child lives for `main_secs`.
        let tree = |main_secs: u32| {
            if cfg!(windows) {
                std::fs::write(
                    dir.path().join("grand.cmd"),
                    "@ping -n 3 127.0.0.1 >nul
@echo x>\"%~dp0marker\"
",
                )
                .unwrap();
                let mut c = command("cmd.exe");
                c.current_dir(dir.path()).args([
                    "/d".to_string(),
                    "/c".to_string(),
                    format!(
                        "start /b grand.cmd & ping -n {} 127.0.0.1 >nul",
                        main_secs + 1
                    ),
                ]);
                c
            } else {
                let mut c = command("sh");
                c.current_dir(dir.path()).args([
                    "-c".to_string(),
                    format!("(sleep 2; touch marker) & sleep {main_secs}"),
                ]);
                c
            }
        };
        // Control: left alone, the grandchild does write it.
        run(tree(4), Duration::from_secs(20), 1024).unwrap();
        assert!(marker.exists(), "control run wrote no marker");
        std::fs::remove_file(&marker).unwrap();

        let err = run(tree(30), Duration::from_millis(800), 1024).unwrap_err();
        assert!(matches!(err, RunError::Timeout { .. }));
        std::thread::sleep(Duration::from_secs(3));
        assert!(!marker.exists(), "the grandchild outlived the timeout");
    }
}
