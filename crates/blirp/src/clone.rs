//! `git clone` on this machine for a session that should run here (cloud
//! sessions): `POST /api/machines/:id/clone`. The clone runs as this
//! daemon's user with that user's git credentials (credential helper, SSH
//! keys); nothing is prompted for and no credential travels with the
//! request: URLs are accepted without userinfo secrets only.

use blirp_core::model::{CloneJob, CloneState};
use blirp_core::proc_tree::ProcessTree;
use blirp_core::process;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// A clone that takes longer than this is killed.
const TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Finished jobs kept for polling.
const KEEP: usize = 20;
/// Last lines of git's output kept for the error message.
const ERROR_LINES: usize = 12;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CloneError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0} already exists")]
    Exists(String),
}

fn invalid(m: impl Into<String>) -> CloneError {
    CloneError::Invalid(m.into())
}

/// Validate a clone URL and remove credentials from it. Accepted: `https://`,
/// `http://`, `ssh://`, `git://` URLs and scp-like `user@host:path`. Local
/// paths, `file://` and transport helpers (`ext::`) are refused; so is
/// anything that could be read as a git option.
pub fn sanitize_url(url: &str) -> Result<String, CloneError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(invalid("the repository URL is empty"));
    }
    if url.starts_with('-') || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid("the repository URL contains invalid characters"));
    }
    if let Some((scheme, rest)) = url.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "https" | "http" | "ssh" | "git") {
            return Err(invalid(format!(
                "{scheme}:// URLs cannot be cloned on another machine"
            )));
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = match authority.rsplit_once('@') {
            // Keep an ssh login name, drop anything that is a secret: https
            // userinfo is a token or password, and so is `user:pass`.
            Some((user, host)) if scheme == "ssh" && !user.contains(':') => {
                format!("{user}@{host}")
            }
            Some((_, host)) => host.to_string(),
            None => authority.to_string(),
        };
        if host.is_empty() || path.is_empty() {
            return Err(invalid("the repository URL has no host or path"));
        }
        return Ok(format!("{scheme}://{host}/{path}"));
    }
    // scp-like `[user@]host:path`; `C:\x`, `./x` or `/x` are local paths.
    match url.split_once(':') {
        Some((left, path))
            if left.len() > 1
                && !left.contains(['/', '\\'])
                && !left.contains("::")
                && !path.is_empty()
                && !path.starts_with(':')
                && !path.starts_with('\\') =>
        {
            let host = left.rsplit('@').next().unwrap_or(left);
            if host.is_empty() || left.matches('@').count() > 1 {
                return Err(invalid("the repository URL has an invalid host"));
            }
            Ok(url.to_string())
        }
        _ => Err(invalid(
            "only network URLs (https, ssh, git or user@host:path) can be cloned on another machine",
        )),
    }
}

/// Folder name for a clone of `url`: its last path segment without `.git`.
pub fn repo_name(url: &str) -> Option<String> {
    let path = url.rsplit([':', '/']).find(|s| !s.is_empty())?;
    let name = path.strip_suffix(".git").unwrap_or(path);
    valid_name(name).then(|| name.to_string())
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Destination of a clone: `<parent>/<name>`, where `parent` (default
/// `<home>/blirp`, created when missing) must be a folder inside `home`
/// and the destination must not exist yet.
pub fn destination(home: &Path, parent: Option<&str>, name: &str) -> Result<PathBuf, CloneError> {
    if !valid_name(name) {
        return Err(invalid(
            "the folder name may only use letters, digits, '-', '_' and '.', and must not start with '.'",
        ));
    }
    let home = dunce::canonicalize(home)
        .map_err(|e| invalid(format!("the home folder is not accessible: {e}")))?;
    let parent = match parent.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => {
            let p = Path::new(p);
            if !p.is_absolute() {
                return Err(invalid("the parent folder must be an absolute path"));
            }
            dunce::canonicalize(p)
                .map_err(|e| invalid(format!("the parent folder is not accessible: {e}")))?
        }
        None => {
            let p = home.join("blirp");
            std::fs::create_dir_all(&p)
                .map_err(|e| invalid(format!("cannot create {}: {e}", p.display())))?;
            p
        }
    };
    if !parent.starts_with(&home) {
        return Err(invalid("the parent folder must be inside the home folder"));
    }
    if !parent.is_dir() {
        return Err(invalid("the parent folder is not a folder"));
    }
    let dest = parent.join(name);
    if dest.symlink_metadata().is_ok() {
        return Err(CloneError::Exists(dest.display().to_string()));
    }
    Ok(dest)
}

#[derive(Default)]
pub struct CloneJobs {
    jobs: Arc<Mutex<HashMap<String, CloneJob>>>,
}

fn lock(m: &Mutex<HashMap<String, CloneJob>>) -> MutexGuard<'_, HashMap<String, CloneJob>> {
    // Jobs are replaced whole; a poisoned map is still consistent.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl CloneJobs {
    pub fn get(&self, id: &str) -> Option<CloneJob> {
        lock(&self.jobs).get(id).cloned()
    }

    /// Start `git clone <url> <dest>` in the background and return its job.
    pub fn start(&self, machine_id: &str, url: String, dest: PathBuf) -> anyhow::Result<CloneJob> {
        let id = blirp_core::random_hex::<16>()?;
        let job = CloneJob {
            id: id.clone(),
            machine_id: machine_id.to_string(),
            url: url.clone(),
            dest: dest.display().to_string(),
            state: CloneState::Running,
            progress: None,
            error: None,
            started_at: blirp_core::now_ms(),
            finished_at: None,
        };
        {
            let mut jobs = lock(&self.jobs);
            // Forget the oldest finished jobs.
            while jobs.len() >= KEEP {
                let oldest = jobs
                    .values()
                    .filter(|j| j.state != CloneState::Running)
                    .min_by_key(|j| j.started_at)
                    .map(|j| j.id.clone());
                match oldest {
                    Some(o) => jobs.remove(&o),
                    None => anyhow::bail!("too many clones are running"),
                };
            }
            jobs.insert(id.clone(), job.clone());
        }
        let jobs = self.jobs.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("clone-{id}"))
            .spawn(move || {
                let update = |f: &dyn Fn(&mut CloneJob)| {
                    if let Some(j) = lock(&jobs).get_mut(&id) {
                        f(j);
                    }
                };
                let result = run_clone(&url, &dest, &|line| {
                    update(&|j| j.progress = Some(line.to_string()));
                });
                update(&|j| {
                    j.finished_at = Some(blirp_core::now_ms());
                    match &result {
                        Ok(()) => j.state = CloneState::Done,
                        Err(e) => {
                            j.state = CloneState::Failed;
                            j.error = Some(e.clone());
                        }
                    }
                });
                match &result {
                    Ok(()) => tracing::info!(dest = %dest.display(), "clone finished"),
                    Err(e) => tracing::warn!(dest = %dest.display(), error = %e, "clone failed"),
                }
            });
        if let Err(e) = spawned {
            lock(&self.jobs).remove(&job.id);
            return Err(e.into());
        }
        Ok(job)
    }
}

/// Run the clone, reporting progress lines; `Err` carries git's last output.
pub(crate) fn run_clone(url: &str, dest: &Path, progress: &dyn Fn(&str)) -> Result<(), String> {
    let mut cmd = process::command("git");
    cmd.args([
        // Transport helpers can run arbitrary commands.
        "-c",
        "protocol.ext.allow=never",
        "clone",
        "--progress",
        "--",
        url,
    ])
    .arg(dest)
    .env("GIT_TERMINAL_PROMPT", "0")
    // Git Credential Manager on Windows must not open a sign-in window.
    .env("GCM_INTERACTIVE", "never")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    // ssh must fail instead of asking for a password or host key.
    if std::env::var_os("GIT_SSH_COMMAND").is_none() && std::env::var_os("GIT_SSH").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "git is not installed on this machine".to_string(),
        _ => format!("cannot run git: {e}"),
    })?;
    #[cfg(windows)]
    let tree = {
        use std::os::windows::io::AsRawHandle;
        ProcessTree::for_process_handle_detached(child.as_raw_handle())
    };
    #[cfg(unix)]
    let tree = ProcessTree::for_process_group(child.id());
    let tail: Arc<Mutex<Vec<String>>> = Arc::default();
    let reader = child.stderr.take().map(|mut err| {
        let tail = tail.clone();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut line = Vec::new();
            let push = |line: &mut Vec<u8>| {
                let text = String::from_utf8_lossy(line).trim().to_string();
                line.clear();
                if text.is_empty() {
                    return;
                }
                let _ = tx.send(text.clone());
                let mut t = tail.lock().unwrap_or_else(PoisonError::into_inner);
                // Progress rewrites one line with \r; keep only its latest state.
                if t.last().is_some_and(|l| same_phase(l, &text)) {
                    t.pop();
                }
                t.push(text);
                if t.len() > ERROR_LINES {
                    t.remove(0);
                }
            };
            loop {
                match err.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        for &b in &buf[..n] {
                            if b == b'\r' || b == b'\n' {
                                push(&mut line);
                            } else if line.len() < 1024 {
                                line.push(b);
                            }
                        }
                    }
                }
            }
            push(&mut line);
        });
        (handle, rx)
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some((_, rx)) = &reader {
            while let Ok(line) = rx.try_recv() {
                progress(&line);
            }
        }
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) if Instant::now() >= deadline => {
                tree.force_kill();
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!(
                    "git clone did not finish within {} minutes",
                    TIMEOUT.as_secs() / 60
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => break Err(format!("waiting for git failed: {e}")),
        }
    };
    if let Some((handle, _)) = reader
        && handle.join().is_err()
    {
        tracing::warn!("clone output reader panicked");
    }
    let status = status?;
    if status.success() {
        return Ok(());
    }
    let tail = tail
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .join("\n");
    Err(if tail.is_empty() {
        format!("git clone failed ({status})")
    } else {
        tail
    })
}

/// Two progress lines of the same phase (`Receiving objects:  12% ...`).
fn same_phase(a: &str, b: &str) -> bool {
    match (a.split_once(':'), b.split_once(':')) {
        (Some((pa, ra)), Some((pb, _))) => {
            pa == pb && ra.trim_start().starts_with(|c: char| c.is_ascii_digit())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_validated_and_stripped_of_credentials() {
        for (input, want) in [
            ("https://github.com/o/r.git", "https://github.com/o/r.git"),
            (
                "https://x-access-token:ghp_secret@github.com/o/r",
                "https://github.com/o/r",
            ),
            ("https://user@example.com/o/r", "https://example.com/o/r"),
            ("ssh://git@host:22/o/r.git", "ssh://git@host:22/o/r.git"),
            ("ssh://git:pw@host/o/r.git", "ssh://host/o/r.git"),
            ("git@github.com:o/r.git", "git@github.com:o/r.git"),
            ("github.com:o/r", "github.com:o/r"),
            (" git://host/r ", "git://host/r"),
        ] {
            assert_eq!(sanitize_url(input).as_deref(), Ok(want), "{input}");
        }
        for bad in [
            "",
            "--upload-pack=touch /tmp/x",
            "-c x",
            "file:///etc",
            "ext::sh -c touch% /tmp/x",
            "ext::sh",
            "/home/me/repo",
            "./repo",
            r"C:\repo",
            "https://host",
            "https://github.com/o/r x",
            "a@b@c:repo",
        ] {
            assert!(sanitize_url(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn repo_names() {
        assert_eq!(
            repo_name("https://github.com/o/blirp.git").as_deref(),
            Some("blirp")
        );
        assert_eq!(
            repo_name("git@github.com:o/my-repo").as_deref(),
            Some("my-repo")
        );
        assert_eq!(repo_name("https://github.com/o/r/").as_deref(), Some("r"));
        assert_eq!(repo_name("https://github.com/o/.hidden"), None);
    }

    #[test]
    fn destination_stays_in_home_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("code/taken")).unwrap();
        let d = destination(&home, None, "repo").unwrap();
        assert!(d.ends_with("blirp/repo"), "{}", d.display());
        assert!(d.parent().unwrap().is_dir(), "~/blirp is created");
        let code = home.join("code");
        let d = destination(&home, code.to_str(), "repo").unwrap();
        assert!(d.ends_with("code/repo"));
        assert!(matches!(
            destination(&home, code.to_str(), "taken"),
            Err(CloneError::Exists(_))
        ));
        assert!(destination(&home, dir.path().to_str(), "repo").is_err());
        assert!(destination(&home, Some("code"), "repo").is_err());
        for bad in ["", "..", ".git", "a/b", r"a\b"] {
            assert!(destination(&home, None, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn progress_lines_of_one_phase_collapse() {
        assert!(same_phase(
            "Receiving objects:  10% (1/10)",
            "Receiving objects:  20% (2/10)"
        ));
        assert!(!same_phase(
            "Cloning into 'x'...",
            "fatal: repository not found"
        ));
    }

    // A real clone of a local repository through the job API (the URL
    // check is bypassed: local paths are refused for remote requests).
    #[test]
    fn clone_job_runs_to_completion_and_reports_failures() {
        if !blirp_core::git::is_installed() {
            eprintln!("skipped: git not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let git = |args: &[&str]| {
            let mut c = process::command("git");
            c.current_dir(&src).args(args);
            assert!(
                process::run(c, Duration::from_secs(30), 4096)
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q"]);
        std::fs::write(src.join("a.txt"), "hi").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ]);
        let jobs = CloneJobs::default();
        let wait = |id: &str| {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let j = jobs.get(id).unwrap();
                if j.state != CloneState::Running {
                    return j;
                }
                assert!(Instant::now() < deadline, "clone did not finish");
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        let dest = dir.path().join("dest");
        let job = jobs
            .start("m", src.display().to_string(), dest.clone())
            .unwrap();
        let done = wait(&job.id);
        assert_eq!(done.state, CloneState::Done, "{:?}", done.error);
        assert!(dest.join("a.txt").is_file());
        let job = jobs
            .start(
                "m",
                dir.path().join("missing").display().to_string(),
                dir.path().join("d2"),
            )
            .unwrap();
        let failed = wait(&job.id);
        assert_eq!(failed.state, CloneState::Failed);
        assert!(failed.error.is_some_and(|e| !e.is_empty()));
    }
}
