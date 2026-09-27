//! Copies of a root on another machine (design §5), also the hub copy for
//! cloud sessions (§8): clone the git remote and check out the origin's
//! HEAD when possible, write the hub's files on top and apply its
//! tombstones, then register the folder as this machine's folder of the
//! project and as a working copy (it uploads its edits live; it takes the
//! hub's changes on "Update from hub" and when a session starts in it).

use super::copy::{self, Copy};
use super::engine::Engine;
use crate::state::SharedState;
use blirp_core::files::{GitManifest, RootInfo};
use blirp_core::model::{CloneState, DownloadJob};
use blirp_core::process;
use blirp_core::store::{CopyMode, FileCopy};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Finished jobs kept for polling.
const KEEP: usize = 20;

#[derive(Default)]
pub struct Downloads {
    jobs: Arc<Mutex<HashMap<String, DownloadJob>>>,
}

fn lock(m: &Mutex<HashMap<String, DownloadJob>>) -> MutexGuard<'_, HashMap<String, DownloadJob>> {
    // Jobs are replaced whole; a poisoned map is still consistent.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why a download cannot start.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
}

/// Whether a root is a folderless project's workspace
/// (`BLIRP_HOME/workspaces/<project id>` on its origin, §5). Its copy goes
/// to this machine's workspace of the project (the root's current project:
/// a merged project's workspace follows the merge), without a folder row.
pub fn is_workspace(origin_path: &str) -> bool {
    let parts: Vec<&str> = origin_path
        .split(['/', '\\'])
        .filter(|p| !p.is_empty())
        .collect();
    matches!(parts.as_slice(), [.., "workspaces", id] if blirp_core::is_safe_id(id))
}

/// Folder name for a copy: the origin folder's name when it is a valid
/// name, else the project's, else `project`.
pub(crate) fn folder_name(origin_path: &str, project_name: &str) -> String {
    let last = origin_path
        .split(['/', '\\'])
        .rfind(|p| !p.is_empty())
        .unwrap_or("");
    if crate::clone::valid_name(last) {
        return last.to_string();
    }
    let cleaned: String = project_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').to_string();
    if crate::clone::valid_name(&cleaned) {
        cleaned
    } else {
        "project".into()
    }
}

fn git(dir: &Path, args: &[&str]) -> bool {
    let mut c = process::command("git");
    c.arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    process::run(c, Duration::from_secs(120), 64 * 1024).is_ok_and(|o| o.success())
}

/// Check out the origin's HEAD in a fresh clone. Returns a note for the
/// user when that was not possible.
fn checkout(dest: &Path, m: &GitManifest) -> Option<String> {
    let head = m.head_sha.as_deref();
    let branch = m.branch.as_deref();
    if let Some(sha) = head
        && git(dest, &["cat-file", "-e", &format!("{sha}^{{commit}}")])
    {
        let ok = match branch {
            Some(b) => git(dest, &["checkout", "-q", "-B", b, sha]),
            None => git(dest, &["checkout", "-q", "--detach", sha]),
        };
        if ok {
            if let Some(b) = branch {
                // Best effort: track the remote branch when it exists.
                let _ = git(
                    dest,
                    &[
                        "branch",
                        "-q",
                        "--set-upstream-to",
                        &format!("origin/{b}"),
                        b,
                    ],
                );
            }
            return None;
        }
    }
    if let Some(b) = branch {
        let _ = git(dest, &["checkout", "-q", b]);
    }
    Some(
        "the origin's last commit is not on the remote yet: its unpushed commits appear as local changes here"
            .into(),
    )
}

impl Downloads {
    pub fn get(&self, id: &str) -> Option<DownloadJob> {
        lock(&self.jobs).get(id).cloned()
    }

    fn update(
        jobs: &Arc<Mutex<HashMap<String, DownloadJob>>>,
        id: &str,
        f: impl FnOnce(&mut DownloadJob),
    ) {
        if let Some(j) = lock(jobs).get_mut(id) {
            f(j);
        }
    }

    /// Start a copy of `root` at `dest` (checked by the caller: missing or
    /// an empty folder).
    pub fn start(
        &self,
        state: &SharedState,
        engine: Arc<Engine>,
        root: RootInfo,
        dest: PathBuf,
    ) -> Result<DownloadJob, DownloadError> {
        let id = blirp_core::random_hex::<16>()
            .map_err(|e| DownloadError::Unavailable(e.to_string()))?;
        let job = DownloadJob {
            id: id.clone(),
            machine_id: state.machine.id.clone(),
            root_id: root.root_id.clone(),
            dest: dest.display().to_string(),
            state: CloneState::Running,
            progress: None,
            error: None,
            note: None,
            started_at: blirp_core::now_ms(),
            finished_at: None,
        };
        {
            let mut jobs = lock(&self.jobs);
            let dest_key = blirp_core::paths::path_key(&dest);
            if jobs.values().any(|j| {
                j.state == CloneState::Running
                    && (j.root_id == root.root_id
                        || blirp_core::paths::path_key(Path::new(&j.dest)) == dest_key)
            }) {
                return Err(DownloadError::Conflict(
                    "a copy of this folder is being made already".into(),
                ));
            }
            while jobs.len() >= KEEP {
                let oldest = jobs
                    .values()
                    .filter(|j| j.state != CloneState::Running)
                    .min_by_key(|j| j.started_at)
                    .map(|j| j.id.clone());
                match oldest {
                    Some(o) => jobs.remove(&o),
                    None => {
                        return Err(DownloadError::Unavailable(
                            "too many downloads are running".into(),
                        ));
                    }
                };
            }
            jobs.insert(id.clone(), job.clone());
        }
        let jobs = self.jobs.clone();
        let st = state.clone();
        tokio::spawn(async move {
            let result = run(&st, &engine, &root, &dest, &jobs, &id).await;
            Downloads::update(&jobs, &id, |j| {
                j.finished_at = Some(blirp_core::now_ms());
                match &result {
                    Ok(note) => {
                        j.state = CloneState::Done;
                        j.note = note.clone();
                        j.progress = None;
                    }
                    Err(e) => {
                        j.state = CloneState::Failed;
                        j.error = Some(e.clone());
                    }
                }
            });
            match result {
                Ok(_) => {
                    tracing::info!(dest = %dest.display(), "downloaded a copy of a project folder")
                }
                Err(e) => {
                    tracing::warn!(dest = %dest.display(), error = %e, "downloading a project folder failed")
                }
            }
            engine.rescan();
        });
        Ok(job)
    }
}

async fn run(
    st: &SharedState,
    engine: &Arc<Engine>,
    root: &RootInfo,
    dest: &Path,
    jobs: &Arc<Mutex<HashMap<String, DownloadJob>>>,
    id: &str,
) -> Result<Option<String>, String> {
    let progress = |line: String| Downloads::update(jobs, id, |j| j.progress = Some(line));
    // Checked again right before anything is written: another download or
    // a folder filled meanwhile must not be written over.
    {
        let (store, d) = (st.store.clone(), dest.to_path_buf());
        let taken = tokio::task::spawn_blocking(move || {
            let key = dunce::canonicalize(&d)
                .unwrap_or_else(|_| d.clone())
                .display()
                .to_string();
            Ok::<bool, blirp_core::store::StoreError>(
                // An empty folder the engine noted as an origin of its own
                // (a workspace never uploaded) may become the copy.
                store
                    .file_copy(&key)?
                    .is_some_and(|c| !c.origin || !c.incarnation.is_empty())
                    || !super::api::empty_or_missing(&d),
            )
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        if taken {
            return Err(format!(
                "{} is no longer empty or already holds a copy",
                dest.display()
            ));
        }
    }
    let manifest = root.manifest.clone().unwrap_or_default();
    let mut note = None;
    // The manifest comes from another machine: network remotes only (a
    // local path or file:// would clone this machine's own repositories).
    let remote = manifest
        .remote
        .as_deref()
        .and_then(|u| crate::clone::sanitize_url(u).ok());
    let cloned = match remote {
        Some(url) if blirp_core::git::is_installed() => {
            progress(format!("Cloning {url}"));
            let (d, u, jobs2, id2) = (
                dest.to_path_buf(),
                url.clone(),
                jobs.clone(),
                id.to_string(),
            );
            let r = tokio::task::spawn_blocking(move || {
                crate::clone::run_clone(&u, &d, &|line| {
                    Downloads::update(&jobs2, &id2, |j| j.progress = Some(line.to_string()));
                })
            })
            .await
            .map_err(|e| e.to_string())?;
            match r {
                Ok(()) => {
                    let (d, m) = (dest.to_path_buf(), manifest.clone());
                    note = tokio::task::spawn_blocking(move || checkout(&d, &m))
                        .await
                        .map_err(|e| e.to_string())?;
                    true
                }
                Err(e) => {
                    tracing::warn!(error = %e, "cloning for a copy failed; copying plain files");
                    note = Some(
                        "the git remote could not be cloned here: plain files, no git history"
                            .into(),
                    );
                    false
                }
            }
        }
        Some(_) => {
            note = Some("git is not installed here: plain files, no git history".into());
            false
        }
        None => {
            if root.manifest.is_some() {
                note = Some("the origin has no git remote: plain files, no git history".into());
            }
            false
        }
    };
    let workspace = is_workspace(&root.path) && {
        let (store, pid) = (st.store.clone(), root.project_id.clone());
        tokio::task::spawn_blocking(move || store.project_paths(&pid).map(|p| p.is_empty()))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?
    };
    if workspace {
        // Owner-only, like every workspace.
        st.paths
            .ensure_workspace(&root.project_id)
            .map_err(|e| format!("cannot create the workspace: {e}"))?;
    } else if !cloned {
        std::fs::create_dir_all(dest)
            .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    }
    let canonical = dunce::canonicalize(dest).map_err(|e| e.to_string())?;
    let key = canonical.display().to_string();
    // Register first (the overlay's bases belong to this copy), pending:
    // the engine uploads nothing from it until the hub's files are all
    // written. Folder and copy rows go in one transaction, so the folder is
    // never seen without its copy row (it would count as an origin). A
    // workspace stays folderless: the project gets no folder row.
    let (store, pid, machine, k) = (
        st.store.clone(),
        root.project_id.clone(),
        st.machine.id.clone(),
        key.clone(),
    );
    let row = FileCopy {
        path: k,
        root_id: root.root_id.clone(),
        origin: false,
        seen: 0,
        created_at: blirp_core::now_ms(),
        mode: CopyMode::Pending,
        incarnation: root.incarnation.clone(),
    };
    tokio::task::spawn_blocking(move || {
        let project = (!workspace).then_some((pid.as_str(), machine.as_str()));
        store.register_download(&row, project)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    st.emit(blirp_core::model::ServerEvent::ProjectUpdated {
        project_id: root.project_id.clone(),
    });
    progress("Writing the hub's files".into());
    let copy = Copy {
        key: key.clone(),
        root_id: root.root_id.clone(),
        origin: false,
        incarnation: root.incarnation.clone(),
    };
    let (jobs2, id2) = (jobs.clone(), id.to_string());
    let shown = move |done: usize, total: usize| {
        Downloads::update(&jobs2, &id2, |j| {
            j.progress = Some(format!("Downloading files from the hub: {done} of {total}"));
        });
    };
    let report = copy::apply_with(&engine.env, &copy, true, Some(&shown))
        .await
        .map_err(|e| e.to_string())?;
    if !report.failed.is_empty() {
        tracing::warn!(
            failed = report.failed.len(),
            "some files of a new copy could not be written"
        );
        return Err(format!(
            "{} file(s) could not be written; the copy does not sync until Update from hub completes it",
            report.failed.len()
        ));
    }
    let store = st.store.clone();
    tokio::task::spawn_blocking(move || store.finish_file_copy(&key))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    if !report.skipped.is_empty() {
        let n = report.skipped.len();
        let more = format!("{n} file(s) cannot be held on this system and were skipped");
        note = Some(match note {
            Some(n) => format!("{n}; {more}"),
            None => more,
        });
    }
    Ok(note)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A clone checks out the origin's HEAD, or says why it could not.
    #[test]
    fn checkout_takes_the_origin_head() {
        if !blirp_core::git::is_installed() {
            eprintln!("skipped: git not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let run = |args: &[&str]| {
            let mut c = process::command("git");
            c.current_dir(&src)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args);
            let out = process::run(c, Duration::from_secs(30), 64 * 1024).unwrap();
            assert!(out.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(src.join("a.txt"), "1").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "one"]);
        let first = run(&["rev-parse", "HEAD"]);
        std::fs::write(src.join("a.txt"), "2").unwrap();
        run(&["commit", "-q", "-am", "two"]);
        let dest = dir.path().join("dest");
        crate::clone::run_clone(&src.display().to_string(), &dest, &|_| {}).unwrap();
        let m = GitManifest {
            remote: None,
            branch: Some("main".into()),
            head_sha: Some(first.clone()),
            upstream_sha: None,
        };
        assert_eq!(checkout(&dest, &m), None);
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "1");
        let missing = GitManifest {
            head_sha: Some("0123456789abcdef0123456789abcdef01234567".into()),
            ..m
        };
        assert!(checkout(&dest, &missing).is_some_and(|n| n.contains("unpushed")));
    }

    #[test]
    fn names_and_workspaces() {
        assert_eq!(folder_name("/home/u/code/my-app", "x"), "my-app");
        assert_eq!(folder_name(r"C:\Users\u\My App", "My App"), "My-App");
        assert_eq!(folder_name("/", "..."), "project");
        assert!(is_workspace("/home/u/.blirp/workspaces/p1"));
        assert!(is_workspace(r"C:\Users\u\.blirp\workspaces\p1"));
        assert!(!is_workspace("/home/u/workspaces/../x"));
        assert!(!is_workspace("/home/u/code/p1"));
    }
}
