//! `blirp stop` and `blirp logs` (§4).

use anyhow::{Context as _, bail};
use blirp_core::paths::{Paths, RuntimeInfo};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// A graceful stop ends sessions (as `detached`) within the daemon's own
/// 5 s grace; this leaves room for closing sockets and the database.
const GRACEFUL: Duration = Duration::from_secs(15);
const AFTER_KILL: Duration = Duration::from_secs(5);

/// Whether a daemon holds `daemon.lock`. The OS releases the lock when the
/// process dies, so unlike a pid from runtime.json this can never point at
/// an unrelated process that reused the pid.
fn lock_held(paths: &Paths) -> anyhow::Result<bool> {
    let path = paths.lock_file();
    let file = match std::fs::OpenOptions::new().write(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
    };
    match file.try_lock() {
        // Dropping the file releases the lock again.
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("lock {}", path.display()))
        }
    }
}

async fn wait_released(paths: &Paths, within: Duration) -> anyhow::Result<bool> {
    let deadline = Instant::now() + within;
    loop {
        if !lock_held(paths)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `POST /api/daemon/shutdown`; false when the daemon did not accept it.
async fn request_shutdown(info: &RuntimeInfo) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    else {
        return false;
    };
    match client
        .post(format!("{}/api/daemon/shutdown", info.base_url()))
        .bearer_auth(&info.token)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => true,
        Ok(r) => {
            eprintln!("daemon refused the shutdown request: {}", r.status());
            false
        }
        Err(e) => {
            eprintln!("daemon did not answer: {e}");
            false
        }
    }
}

/// `blirp stop`: graceful shutdown over the local API (sessions end as
/// `detached`), then a hard kill of the pid in runtime.json if the daemon
/// still holds its lock after the grace period.
pub async fn stop(paths: &Paths) -> anyhow::Result<ExitCode> {
    let info = RuntimeInfo::read(paths)?;
    if !lock_held(paths)? {
        // A crashed daemon may have left runtime.json behind.
        if let Some(i) = info {
            RuntimeInfo::remove_if_owned(paths, i.pid)?;
        }
        println!("blirp daemon is not running ({})", paths.home().display());
        return Ok(ExitCode::SUCCESS);
    }
    let Some(info) = info else {
        bail!(
            "a daemon holds {} but runtime.json is missing; stop it with your OS tools",
            paths.lock_file().display()
        );
    };
    let requested = request_shutdown(&info).await;
    if requested && wait_released(paths, GRACEFUL).await? {
        println!("blirp daemon stopped (pid {})", info.pid);
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!(
        "daemon (pid {}) did not stop {}; killing it",
        info.pid,
        if requested { "in time" } else { "on request" }
    );
    kill_pid(info.pid).with_context(|| format!("kill pid {}", info.pid))?;
    if !wait_released(paths, AFTER_KILL).await? {
        bail!("daemon (pid {}) is still running", info.pid);
    }
    RuntimeInfo::remove_if_owned(paths, info.pid)?;
    println!("blirp daemon killed (pid {})", info.pid);
    Ok(ExitCode::SUCCESS)
}

/// Hard-kill a process (SIGKILL / TerminateProcess).
#[allow(unsafe_code)]
fn kill_pid(pid: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let pid = i32::try_from(pid).map_err(std::io::Error::other)?;
        // SAFETY: kill has no memory-safety preconditions.
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
            let e = std::io::Error::last_os_error();
            // Already gone is the goal.
            if e.raw_os_error() != Some(libc::ESRCH) {
                return Err(e);
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };
        // SAFETY: OpenProcess has no pointer arguments; a null handle is an error.
        let h = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if h.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `h` is a live process handle opened above with
        // PROCESS_TERMINATE and closed exactly once below.
        let ok = unsafe { TerminateProcess(h, 1) };
        let err = std::io::Error::last_os_error();
        // SAFETY: see above.
        unsafe { CloseHandle(h) };
        if ok == 0 { Err(err) } else { Ok(()) }
    }
}

/// Newest daemon log (`blirpd.<date>.log`; dates sort as text).
fn current_log(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("blirpd.") && n.ends_with(".log"))
        })
        .max()
}

/// The last `n` lines of `path`, reading at most its last 4 MiB.
fn tail(path: &Path, n: usize) -> anyhow::Result<(String, u64)> {
    const WINDOW: u64 = 4 << 20;
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(WINDOW);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    let keep = &lines[lines.len().saturating_sub(n)..];
    let mut out = keep.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Ok((out, start + buf.len() as u64))
}

/// `blirp logs [-n N] [-f]`: the end of the current daemon log; `-f`
/// keeps printing new lines and follows the daily rotation.
pub fn logs(paths: &Paths, lines: usize, follow: bool) -> anyhow::Result<ExitCode> {
    let dir = paths.logs_dir();
    let mut stdout = std::io::stdout();
    let mut current = current_log(&dir);
    let mut pos = 0;
    match &current {
        Some(p) => {
            let (text, end) = tail(p, lines)?;
            stdout.write_all(text.as_bytes())?;
            pos = end;
        }
        None if !follow => {
            eprintln!("no daemon log yet in {}", dir.display());
            return Ok(ExitCode::FAILURE);
        }
        None => {}
    }
    if !follow {
        return Ok(ExitCode::SUCCESS);
    }
    stdout.flush()?;
    loop {
        if let Some(p) = &current {
            // Truncated or replaced: start over.
            let len = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            if len < pos {
                pos = 0;
            }
            if len > pos {
                let mut f = std::fs::File::open(p)?;
                f.seek(SeekFrom::Start(pos))?;
                let mut buf = Vec::new();
                f.read_to_end(&mut buf)?;
                pos += buf.len() as u64;
                stdout.write_all(&buf)?;
                stdout.flush()?;
            }
        }
        // A new day's file: continue there once the old one is drained.
        let newest = current_log(&dir);
        if newest.is_some() && newest != current {
            current = newest;
            pos = 0;
            continue;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_log_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blirpd.2026-09-24.log"), "old\n").unwrap();
        std::fs::write(dir.path().join("desktop.2026-09-26.log"), "other\n").unwrap();
        let today = dir.path().join("blirpd.2026-09-25.log");
        std::fs::write(&today, "a\nb\nc\n").unwrap();
        assert_eq!(current_log(dir.path()), Some(today.clone()));
        let (text, end) = tail(&today, 2).unwrap();
        assert_eq!(text, "b\nc\n");
        assert_eq!(end, 6);
    }

    #[test]
    fn kill_pid_ends_a_process() {
        let mut child = if cfg!(windows) {
            std::process::Command::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        } else {
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap()
        };
        kill_pid(child.id()).unwrap();
        let begin = Instant::now();
        child.wait().unwrap();
        assert!(begin.elapsed() < Duration::from_secs(10));
    }
}
