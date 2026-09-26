//! Projects, project paths, project resolution (§5) and merge.

use super::{Change, Result, Store, StoreError, all, apply_in, one};
use crate::git;
use crate::model::{Project, ProjectPath, ProjectPathInfo, ProjectSummary, Session};
use rusqlite::{Connection, Row, Transaction, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const HOME_PROJECT_KEY: &str = "home_project_id";

pub(super) fn project_row(r: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get("id")?,
        name: r.get("name")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        deleted: r.get("deleted")?,
    })
}

pub(super) fn path_row(r: &Row<'_>) -> rusqlite::Result<ProjectPath> {
    Ok(ProjectPath {
        project_id: r.get("project_id")?,
        machine_id: r.get("machine_id")?,
        path: r.get("path")?,
        git_remote: r.get("git_remote")?,
    })
}

/// Result of [`Store::resolve_project`].
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProject {
    pub project: Project,
    /// Project root on this machine; for the Home project, the cwd itself.
    pub root: PathBuf,
    pub is_home: bool,
    pub created: bool,
}

/// Folders that never become project roots (§5 step 3): the home folder,
/// its ancestors and filesystem roots. For sessions blirp did not start
/// (`auto`: ingested transcripts, hooks) also the places agents and tools
/// run scratch work in: hidden folders directly under home (tool data such
/// as `~/.codex`, `~/.claude`, `~/.blirp`), Codex desktop chat folders
/// (`<Documents>/Codex/<YYYY-MM-DD>/<chat>`, one per chat, for each folder
/// in `documents`) and `scratch` (temp and system folders). Registered
/// folders always win: resolution matches them before these rules apply.
#[derive(Debug, Clone, Default)]
pub struct NonProjectDirs {
    pub home: Option<PathBuf>,
    pub scratch: Vec<PathBuf>,
    /// Documents folders (`~/Documents` and the OS's, which may be
    /// redirected, e.g. to OneDrive).
    pub documents: Vec<PathBuf>,
    pub auto: bool,
}

impl NonProjectDirs {
    /// For sessions the user starts in blirp: only home and roots.
    pub fn launch() -> Self {
        Self {
            home: crate::paths::user_home().map(|h| canonical_or_same(&h)),
            ..Self::default()
        }
    }

    /// For ingested and hooked sessions, from this process's environment.
    pub fn from_process() -> Self {
        let mut scratch = vec![std::env::temp_dir()];
        if cfg!(windows) {
            scratch.extend(std::env::var_os("SystemRoot").map(PathBuf::from));
        } else {
            // `temp_dir` is `$TMPDIR` (per user on macOS); agents also use
            // these (macOS: `/private/tmp` after canonicalizing).
            scratch.extend([PathBuf::from("/tmp"), PathBuf::from("/var/tmp")]);
        }
        let mut dirs = Self::auto(crate::paths::user_home(), scratch);
        if let Some(d) = crate::paths::documents_dir().filter(|d| d.is_absolute()) {
            dirs.documents.push(canonical_or_same(&d));
        }
        dirs
    }

    /// Auto rules for `home` (with `home/Documents`) and `scratch`
    /// (canonicalized where they exist).
    pub fn auto(home: Option<PathBuf>, scratch: Vec<PathBuf>) -> Self {
        let home = home.map(|h| canonical_or_same(&h));
        Self {
            documents: home
                .iter()
                .map(|h| canonical_or_same(&h.join("Documents")))
                .collect(),
            home,
            scratch: scratch
                .into_iter()
                .filter(|d| d.is_absolute())
                .map(|d| canonical_or_same(&d))
                .collect(),
            auto: true,
        }
    }

    /// Whether `root` (absolute) must not be registered as a project.
    pub fn contains(&self, root: &Path) -> bool {
        use crate::paths::path_key;
        if root.parent().is_none() {
            return true;
        }
        let key = path_key(root);
        let home = self.home.as_deref().map(path_key);
        if home.as_ref().is_some_and(|h| h.starts_with(&key)) {
            return true;
        }
        if !self.auto {
            return false;
        }
        let under = |base: &Path| key.strip_prefix(path_key(base)).ok().map(Path::to_path_buf);
        let first = |rel: &Path| {
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        if self.scratch.iter().any(|d| under(d).is_some()) {
            return true;
        }
        if let Some(h) = &self.home
            && let Some(rel) = under(h)
            && first(&rel).first().is_some_and(|c| c.starts_with('.'))
        {
            return true;
        }
        self.documents.iter().filter_map(|d| under(d)).any(|rel| {
            let parts = first(&rel);
            parts.len() >= 2 && parts[0].eq_ignore_ascii_case("codex") && is_iso_date(&parts[1])
        })
    }
}

/// `YYYY-MM-DD` (the Codex desktop app's per-day chat folders).
fn is_iso_date(s: &str) -> bool {
    s.len() == 10
        && s.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        })
}

fn canonical_or_same(p: &Path) -> PathBuf {
    dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// A recorded folder in one spelling: no `\\?\` prefix, no trailing or
/// doubled separators, `.` and `..` resolved lexically, `/` as `\` on
/// Windows. For folders that no longer exist (existing ones are
/// canonicalized).
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in dunce::simplified(p).components() {
        match c {
            Component::CurDir => {}
            // At a root `pop` keeps the root, like the OS does.
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

fn canonical_dir(p: &Path) -> Result<PathBuf> {
    let c = dunce::canonicalize(p).map_err(|e| {
        StoreError::Invalid(format!("folder {} is not accessible: {e}", p.display()))
    })?;
    if !c.is_dir() {
        return Err(StoreError::Invalid(format!(
            "{} is not a folder",
            c.display()
        )));
    }
    Ok(c)
}

fn folder_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| p.display().to_string())
}

/// Longest registered path on this machine that contains `p` (case
/// insensitive on Windows).
pub(super) fn longest_prefix<'a>(paths: &'a [ProjectPath], p: &Path) -> Option<&'a ProjectPath> {
    let key = crate::paths::path_key(p);
    paths
        .iter()
        .filter(|pp| key.starts_with(crate::paths::path_key(Path::new(&pp.path))))
        .max_by_key(|pp| pp.path.len())
}

pub(super) fn live_local_paths(c: &Connection, machine_id: &str) -> Result<Vec<ProjectPath>> {
    all(
        c,
        "SELECT pp.* FROM project_paths pp JOIN projects p ON p.id = pp.project_id
         WHERE pp.machine_id = ?1 AND p.deleted = 0",
        params![machine_id],
        path_row,
    )
}

fn get_project_in(c: &Connection, id: &str) -> Result<Option<Project>> {
    one(
        c,
        "SELECT * FROM projects WHERE id = ?1",
        params![id],
        project_row,
    )
}

pub(super) fn live_project_in(c: &Connection, id: &str) -> Result<Project> {
    get_project_in(c, id)?
        .filter(|p| !p.deleted)
        .ok_or(StoreError::NotFound("project"))
}

/// `registered`: added by the user (Add folder). Such a project starts
/// with `updated_at = created_at + 1`, which tells it apart from one
/// resolution created (`updated_at = created_at` until its first edit) for
/// [`Store::retire_non_projects`] without a schema change.
fn new_project(tx: &Transaction<'_>, name: &str, registered: bool) -> Result<Project> {
    let now = crate::now_ms();
    let p = Project {
        id: crate::new_id(),
        name: name.to_string(),
        created_at: now,
        updated_at: now + i64::from(registered),
        deleted: false,
    };
    apply_in(tx, &Change::Project(p.clone()))?;
    Ok(p)
}

fn attach_path(
    tx: &Transaction<'_>,
    project_id: &str,
    machine_id: &str,
    path: &Path,
    remote: Option<String>,
) -> Result<()> {
    apply_in(
        tx,
        &Change::ProjectPath(ProjectPath {
            project_id: project_id.to_string(),
            machine_id: machine_id.to_string(),
            path: path.to_string_lossy().into_owned(),
            git_remote: remote,
        }),
    )?;
    Ok(())
}

impl Store {
    pub fn get_project(&self, id: &str) -> Result<Option<Project>> {
        self.read(|c| get_project_in(c, id))
    }

    /// A project that exists and is not deleted.
    pub fn live_project(&self, id: &str) -> Result<Project> {
        self.read(|c| live_project_in(c, id))
    }

    pub fn project_paths(&self, project_id: &str) -> Result<Vec<ProjectPath>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM project_paths WHERE project_id = ?1 ORDER BY machine_id, path",
                params![project_id],
                path_row,
            )
        })
    }

    /// This machine's folders for a project, as absolute paths.
    pub fn local_roots(&self, project_id: &str, machine_id: &str) -> Result<Vec<PathBuf>> {
        Ok(self
            .project_paths(project_id)?
            .into_iter()
            .filter(|p| p.machine_id == machine_id)
            .map(|p| PathBuf::from(p.path))
            .collect())
    }

    pub fn home_project_id(&self) -> Result<Option<String>> {
        Ok(self
            .get_setting(HOME_PROJECT_KEY)?
            .and_then(|v| v.as_str().map(str::to_string)))
    }

    /// All live projects with paths, git flag and session stats, most recently active first.
    pub fn list_project_summaries(&self, machine_id: &str) -> Result<Vec<ProjectSummary>> {
        let home = self.home_project_id()?;
        let (rows, paths) = self.read(|c| {
            let rows = all(
                c,
                "SELECT p.*,
                   (SELECT COUNT(*) FROM sessions s WHERE s.project_id = p.id) AS session_count,
                   (SELECT COUNT(*) FROM sessions s WHERE s.project_id = p.id
                      AND s.status IN ('starting','working','idle','waiting')) AS live_count,
                   (SELECT MAX(s.last_activity_at) FROM sessions s WHERE s.project_id = p.id) AS last_activity
                 FROM projects p WHERE p.deleted = 0
                 ORDER BY COALESCE(last_activity, p.updated_at) DESC",
                [],
                |r| {
                    Ok((
                        project_row(r)?,
                        r.get::<_, i64>("session_count")?,
                        r.get::<_, i64>("live_count")?,
                        r.get::<_, Option<i64>>("last_activity")?,
                    ))
                },
            )?;
            let paths = all(c, "SELECT * FROM project_paths ORDER BY path", [], path_row)?;
            Ok((rows, paths))
        })?;
        let mut by_project: HashMap<String, Vec<ProjectPathInfo>> = HashMap::new();
        for p in paths {
            let local = p.machine_id == machine_id;
            by_project
                .entry(p.project_id.clone())
                .or_default()
                .push(ProjectPathInfo {
                    is_git: local && git::find_git_root_fs(Path::new(&p.path)).is_some(),
                    local,
                    machine_id: p.machine_id,
                    path: p.path,
                    git_remote: p.git_remote,
                });
        }
        Ok(rows
            .into_iter()
            .map(
                |(project, session_count, live_session_count, last_activity_at)| {
                    let paths = by_project.remove(&project.id).unwrap_or_default();
                    ProjectSummary {
                        is_git: paths.iter().any(|p| p.is_git || p.git_remote.is_some()),
                        is_home: home.as_deref() == Some(project.id.as_str()),
                        project,
                        paths,
                        session_count,
                        live_session_count,
                        last_activity_at,
                    }
                },
            )
            .collect())
    }

    pub fn project_summary(&self, id: &str, machine_id: &str) -> Result<ProjectSummary> {
        self.list_project_summaries(machine_id)?
            .into_iter()
            .find(|p| p.project.id == id)
            .ok_or(StoreError::NotFound("project"))
    }

    /// §5 project resolution for a session the user starts in `cwd` on
    /// this machine.
    pub fn resolve_project(
        &self,
        machine_id: &str,
        machine_name: &str,
        cwd: &Path,
    ) -> Result<ResolvedProject> {
        self.resolve_project_with(machine_id, machine_name, cwd, &NonProjectDirs::launch())
    }

    /// §5 project resolution with explicit [`NonProjectDirs`] (auto rules
    /// for sessions blirp did not start).
    pub fn resolve_project_with(
        &self,
        machine_id: &str,
        machine_name: &str,
        cwd: &Path,
        dirs: &NonProjectDirs,
    ) -> Result<ResolvedProject> {
        let cwd = canonical_dir(cwd)?;
        let found =
            |paths: &[ProjectPath], p: &Path, c: &Connection| -> Result<Option<ResolvedProject>> {
                match longest_prefix(paths, p) {
                    Some(pp) => Ok(Some(ResolvedProject {
                        project: live_project_in(c, &pp.project_id)?,
                        root: PathBuf::from(&pp.path),
                        is_home: false,
                        created: false,
                    })),
                    None => Ok(None),
                }
            };
        // 1. Fast path without spawning git.
        if let Some(r) = self.read(|c| found(&live_local_paths(c, machine_id)?, &cwd, c))? {
            return Ok(r);
        }
        // 2. git (outside the write lock: it can take a while).
        let repo = match git::repo_info(&cwd) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "git inspection failed; treating folder as non-git");
                None
            }
        };
        self.write(|tx| {
            // Re-check under the lock so concurrent resolves never create duplicates.
            let paths = live_local_paths(tx, machine_id)?;
            if let Some(r) = found(&paths, &cwd, tx)? {
                return Ok(r);
            }
            let (root, remote) = match &repo {
                Some(info) => {
                    // Worktrees and subfolders resolve to the main repo root.
                    if let Some(r) = found(&paths, &info.main_root, tx)? {
                        return Ok(r);
                    }
                    (info.main_root.clone(), info.remote.clone())
                }
                None => (cwd.clone(), None),
            };
            // 3. Home dir, its ancestors (`C:\Users`, `/home`), filesystem
            // roots and (auto) scratch folders are never project roots.
            if dirs.contains(&root) {
                return Ok(ResolvedProject {
                    project: home_project(tx, machine_name)?,
                    root: cwd.clone(),
                    is_home: true,
                    created: false,
                });
            }
            if let Some(remote) = &remote {
                let other: Option<String> = one(
                    tx,
                    "SELECT pp.project_id FROM project_paths pp JOIN projects p ON p.id = pp.project_id
                     WHERE pp.git_remote = ?1 AND pp.machine_id != ?2 AND p.deleted = 0 LIMIT 1",
                    params![remote, machine_id],
                    |r| r.get(0),
                )?;
                if let Some(pid) = other {
                    attach_path(tx, &pid, machine_id, &root, Some(remote.clone()))?;
                    return Ok(ResolvedProject {
                        project: live_project_in(tx, &pid)?,
                        root,
                        is_home: false,
                        created: false,
                    });
                }
            }
            let project = new_project(tx, &folder_name(&root), false)?;
            attach_path(tx, &project.id, machine_id, &root, remote)?;
            Ok(ResolvedProject {
                project,
                root,
                is_home: false,
                created: true,
            })
        })
    }

    /// [`Store::resolve_project`] for ingested transcripts, whose folder may
    /// no longer exist (their history is still permanent). A missing folder
    /// matches registered paths by prefix; otherwise it becomes a non-git
    /// project named after the folder, registered at the path as recorded.
    pub fn resolve_project_lenient(
        &self,
        machine_id: &str,
        machine_name: &str,
        cwd: &Path,
        git_remote: Option<&str>,
        dirs: &NonProjectDirs,
    ) -> Result<ResolvedProject> {
        if cwd.is_dir() {
            return self.resolve_project_with(machine_id, machine_name, cwd, dirs);
        }
        if !cwd.is_absolute() {
            return Err(StoreError::Invalid(format!(
                "{} is not an absolute path",
                cwd.display()
            )));
        }
        let cwd = &normalize(cwd);
        self.write(|tx| {
            let paths = live_local_paths(tx, machine_id)?;
            if let Some(pp) = longest_prefix(&paths, cwd) {
                return Ok(ResolvedProject {
                    project: live_project_in(tx, &pp.project_id)?,
                    root: PathBuf::from(&pp.path),
                    is_home: false,
                    created: false,
                });
            }
            // A worktree or clone that is gone (e.g. under
            // `~/.codex/worktrees`): the transcript's remote names its repo.
            // This machine's folder of that repo first, else any machine's.
            if let Some(remote) = git_remote.and_then(crate::git::normalize_remote) {
                let pid: Option<String> = one(
                    tx,
                    "SELECT pp.project_id FROM project_paths pp JOIN projects p ON p.id = pp.project_id
                     WHERE pp.git_remote = ?1 AND p.deleted = 0
                     ORDER BY pp.machine_id != ?2 LIMIT 1",
                    params![remote, machine_id],
                    |r| r.get(0),
                )?;
                if let Some(pid) = pid {
                    return Ok(ResolvedProject {
                        project: live_project_in(tx, &pid)?,
                        root: cwd.to_path_buf(),
                        is_home: false,
                        created: false,
                    });
                }
            }
            if dirs.contains(cwd) {
                return Ok(ResolvedProject {
                    project: home_project(tx, machine_name)?,
                    root: cwd.to_path_buf(),
                    is_home: true,
                    created: false,
                });
            }
            let project = new_project(tx, &folder_name(cwd), false)?;
            attach_path(tx, &project.id, machine_id, cwd, None)?;
            Ok(ResolvedProject {
                project,
                root: cwd.to_path_buf(),
                is_home: false,
                created: true,
            })
        })
    }

    /// This machine's registered folders of live projects.
    pub fn local_project_paths(&self, machine_id: &str) -> Result<Vec<PathBuf>> {
        Ok(self
            .read(|c| live_local_paths(c, machine_id))?
            .into_iter()
            .map(|p| PathBuf::from(p.path))
            .collect())
    }

    /// Explicitly register `path` as a new project (Projects > Add folder).
    pub fn register_project(
        &self,
        machine_id: &str,
        path: &Path,
        name: Option<&str>,
    ) -> Result<Project> {
        let path = canonical_dir(path)?;
        let remote = git::repo_info(&path).ok().flatten().and_then(|r| r.remote);
        let name = match name.map(str::trim) {
            Some("") => return Err(StoreError::Invalid("name must not be empty".into())),
            Some(n) => n.to_string(),
            None => folder_name(&path),
        };
        self.write(|tx| {
            let key = path.to_string_lossy();
            if let Some(pid) = one::<String>(
                tx,
                "SELECT pp.project_id FROM project_paths pp JOIN projects p ON p.id = pp.project_id
                 WHERE pp.machine_id = ?1 AND pp.path = ?2 AND p.deleted = 0",
                params![machine_id, key],
                |r| r.get(0),
            )? {
                return Err(StoreError::Conflict(format!(
                    "{key} is already registered to project {pid}"
                )));
            }
            let project = new_project(tx, &name, true)?;
            attach_path(tx, &project.id, machine_id, &path, remote)?;
            Ok(project)
        })
    }

    pub fn rename_project(&self, id: &str, name: &str) -> Result<Project> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 200 {
            return Err(StoreError::Invalid("name must be 1-200 characters".into()));
        }
        self.write(|tx| {
            let mut p = live_project_in(tx, id)?;
            p.name = name.to_string();
            p.updated_at = crate::now_ms();
            apply_in(tx, &Change::Project(p.clone()))?;
            Ok(p)
        })
    }

    /// Soft-delete a project. Its folders are unregistered on every machine:
    /// each one drops them when it applies the delete (see `write_row`).
    /// Sessions and memory stay in the database.
    pub fn delete_project(&self, id: &str) -> Result<()> {
        self.write(|tx| {
            let mut p = live_project_in(tx, id)?;
            p.deleted = true;
            p.updated_at = crate::now_ms();
            apply_in(tx, &Change::Project(p))?;
            Ok(())
        })
    }

    /// Move `from`'s paths, sessions, records, wiki pages, resources and
    /// suggestions into `into`, then soft-delete `from`. The brief moves only
    /// when `into` has none. Clashing wiki slugs get a numeric suffix.
    pub fn merge_projects(&self, from: &str, into: &str) -> Result<Project> {
        if from == into {
            return Err(StoreError::Invalid(
                "cannot merge a project into itself".into(),
            ));
        }
        self.write(|tx| merge_in(tx, from, into, false))
    }

    /// Cleanup for projects that resolution created before `dirs` said
    /// their folder is no project (§5): each is merged into this machine's
    /// Home project, where resolution now files such sessions, without its
    /// folders or brief. Only projects that show no sign of the user:
    /// created by resolution and never renamed or merged into (`updated_at
    /// = created_at`; registered ones start one higher), named after their
    /// folder, at least one session and every session this machine's and
    /// not started in blirp, every folder on this machine, not git with a
    /// remote (another machine may have joined it by that remote) and
    /// matched by `dirs`, no wiki pages or resources, and only unpinned
    /// distiller records and distiller brief versions. Sessions, records and
    /// suggestions move to Home; the project is soft-deleted, so every
    /// change replicates. Returns the merged projects (as they were).
    pub fn retire_non_projects(
        &self,
        machine_id: &str,
        machine_name: &str,
        dirs: &NonProjectDirs,
    ) -> Result<Vec<Project>> {
        self.write(|tx| {
            let home_id: Option<String> = one(
                tx,
                "SELECT value_json FROM settings WHERE key = ?1",
                params![HOME_PROJECT_KEY],
                |r| r.get::<_, String>(0),
            )?
            .and_then(|v| serde_json::from_str(&v).ok());
            let candidates = all(
                tx,
                "SELECT p.* FROM projects p
                 WHERE p.deleted = 0 AND p.updated_at = p.created_at AND p.id != ?2
                   AND EXISTS (SELECT 1 FROM project_paths pp WHERE pp.project_id = p.id)
                   AND EXISTS (SELECT 1 FROM sessions s WHERE s.project_id = p.id)
                   AND NOT EXISTS (SELECT 1 FROM project_paths pp WHERE pp.project_id = p.id
                                   AND (pp.machine_id != ?1 OR pp.git_remote IS NOT NULL))
                   AND NOT EXISTS (SELECT 1 FROM sessions s WHERE s.project_id = p.id
                                   AND (s.machine_id != ?1 OR s.origin != 'external'))
                   AND NOT EXISTS (SELECT 1 FROM records r WHERE r.project_id = p.id
                                   AND (r.updated_by != ?3 OR r.pinned != 0))
                   AND NOT EXISTS (SELECT 1 FROM brief_history b WHERE b.project_id = p.id
                                   AND b.updated_by != ?3)
                   AND NOT EXISTS (SELECT 1 FROM wiki_pages w WHERE w.project_id = p.id)
                   AND NOT EXISTS (SELECT 1 FROM resources x WHERE x.project_id = p.id)",
                params![machine_id, home_id.unwrap_or_default(), super::BY_DISTILLER],
                project_row,
            )?;
            let mut retired = Vec::new();
            for p in candidates {
                let paths = all(
                    tx,
                    "SELECT * FROM project_paths WHERE project_id = ?1",
                    params![p.id],
                    path_row,
                )?;
                let named_after = paths
                    .iter()
                    .any(|pp| folder_name(Path::new(&pp.path)) == p.name);
                if !named_after
                    || !paths
                        .iter()
                        .all(|pp| dirs.contains(&normalize(Path::new(&pp.path))))
                {
                    continue;
                }
                let home = home_project(tx, machine_name)?;
                merge_in(tx, &p.id, &home.id, true)?;
                retired.push(p);
            }
            Ok(retired)
        })
    }

    pub fn sessions_of_project(&self, project_id: &str) -> Result<Vec<Session>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM sessions WHERE project_id = ?1 ORDER BY started_at DESC",
                params![project_id],
                super::sessions::session_row,
            )
        })
    }
}

/// See [`Store::merge_projects`]; `retire` leaves `from`'s folders (they go
/// with the deleted project) and brief behind.
fn merge_in(tx: &Transaction<'_>, from: &str, into: &str, retire: bool) -> Result<Project> {
    let mut src = live_project_in(tx, from)?;
    let mut dst = live_project_in(tx, into)?;
    let now = crate::now_ms();
    let mut changes = Vec::new();
    // Folders left behind are dropped with the deleted project.
    if !retire {
        for mut pp in all(
            tx,
            "SELECT * FROM project_paths WHERE project_id = ?1",
            params![from],
            path_row,
        )? {
            pp.project_id = into.to_string();
            changes.push(Change::ProjectPath(pp));
        }
    }
    for mut s in all(
        tx,
        "SELECT * FROM sessions WHERE project_id = ?1",
        params![from],
        super::sessions::session_row,
    )? {
        s.project_id = into.to_string();
        changes.push(Change::Session(s));
    }
    for mut r in all(
        tx,
        "SELECT * FROM records WHERE project_id = ?1",
        params![from],
        super::memory::record_row,
    )? {
        r.project_id = into.to_string();
        changes.push(Change::Record(r));
    }
    for mut r in all(
        tx,
        "SELECT * FROM resources WHERE project_id = ?1",
        params![from],
        super::memory::resource_row,
    )? {
        r.project_id = into.to_string();
        changes.push(Change::Resource(r));
    }
    let mut taken: std::collections::HashSet<String> = all(
        tx,
        "SELECT slug FROM wiki_pages WHERE project_id = ?1",
        params![into],
        |r| r.get(0),
    )?
    .into_iter()
    .collect();
    for mut w in all(
        tx,
        "SELECT * FROM wiki_pages WHERE project_id = ?1",
        params![from],
        super::memory::wiki_row,
    )? {
        if taken.contains(&w.slug) {
            let base = w.slug.clone();
            w.slug = (2..)
                .map(|n| format!("{base}-{n}"))
                .find(|s| !taken.contains(s))
                .unwrap_or(base);
        }
        taken.insert(w.slug.clone());
        w.project_id = into.to_string();
        w.updated_at = now;
        changes.push(Change::WikiPage(w));
    }
    if !retire {
        let dst_brief = super::memory::get_brief_in(tx, into)?;
        if let (None, Some(b)) = (dst_brief, super::memory::get_brief_in(tx, from)?) {
            super::memory::put_brief_in(tx, into, &b.body_md, &b.updated_by)?;
        }
    }
    for c in &changes {
        apply_in(tx, c)?;
    }
    tx.execute(
        "UPDATE suggestions SET project_id = ?1 WHERE project_id = ?2",
        params![into, from],
    )?;
    src.deleted = true;
    src.updated_at = now;
    apply_in(tx, &Change::Project(src))?;
    dst.updated_at = now;
    apply_in(tx, &Change::Project(dst.clone()))?;
    Ok(dst)
}

/// This machine's Home project, created on first use.
fn home_project(tx: &Transaction<'_>, machine_name: &str) -> Result<Project> {
    let id: Option<String> = one(
        tx,
        "SELECT value_json FROM settings WHERE key = ?1",
        params![HOME_PROJECT_KEY],
        |r| r.get::<_, String>(0),
    )?
    .and_then(|v| serde_json::from_str::<String>(&v).ok());
    if let Some(id) = id
        && let Some(p) = get_project_in(tx, &id)?.filter(|p| !p.deleted)
    {
        return Ok(p);
    }
    let p = new_project(tx, &format!("Home ({machine_name})"), false)?;
    tx.execute(
        "INSERT INTO settings(key, value_json) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
        params![HOME_PROJECT_KEY, serde_json::to_string(&p.id)?],
    )?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::super::tests::temp_store;
    use super::*;
    use crate::git::tests::run_git;
    use crate::model::{Record, RecordKind, RecordStatus, SessionOrigin};
    use crate::store::BY_DISTILLER;

    fn launch(home: &Path) -> NonProjectDirs {
        NonProjectDirs {
            home: Some(home.to_path_buf()),
            ..NonProjectDirs::default()
        }
    }

    /// A temp root with `home/` and `tmp/` (the scratch folder), canonical.
    fn auto_env() -> (tempfile::TempDir, Store, PathBuf, NonProjectDirs) {
        let (dir, store) = temp_store();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("tmp")).unwrap();
        let dirs = NonProjectDirs::auto(Some(root.join("home")), vec![root.join("tmp")]);
        (dir, store, root, dirs)
    }

    #[test]
    fn scratch_folders_never_become_projects() {
        let (_d, store, root, dirs) = auto_env();
        let home = root.join("home");
        let scratch = [
            root.join("tmp/agent-run/work"),
            home.join(".codex/worktrees/abc/app"),
            home.join(".some-tool/profiles/default"),
            home.join("Documents/Codex/2026-01-02/new-chat"),
            home.join("documents/codex/2026-01-02"),
        ];
        for s in &scratch {
            std::fs::create_dir_all(s).unwrap();
            let r = store.resolve_project_with("m", "box", s, &dirs).unwrap();
            assert!(r.is_home && !r.created, "{}", s.display());
            // A folder that no longer exists behaves the same.
            let gone = s.join("gone");
            let r = store
                .resolve_project_lenient("m", "box", &gone, None, &dirs)
                .unwrap();
            assert!(r.is_home && !r.created, "{}", gone.display());
        }
        let home_r = store
            .resolve_project_with("m", "box", &home, &dirs)
            .unwrap();
        assert!(home_r.is_home);
        assert_eq!(store.list_project_summaries("m").unwrap().len(), 1);

        // Real folders, also next to the Codex chats, are projects.
        for real in [
            home.join("code/app"),
            home.join("Documents/Codex/notes"),
            home.join("Documents/Game"),
        ] {
            std::fs::create_dir_all(&real).unwrap();
            let r = store
                .resolve_project_with("m", "box", &real, &dirs)
                .unwrap();
            assert!(r.created && !r.is_home, "{}", real.display());
        }

        // Sessions the user starts in blirp keep the plain rules.
        let r = store
            .resolve_project_with("m", "box", &scratch[0], &launch(&home))
            .unwrap();
        assert!(r.created && !r.is_home);

        // A registered folder wins over every rule, also for its subfolders.
        let tool = home.join(".some-tool/profiles/default");
        let p = store.register_project("m", &tool, Some("Tool")).unwrap();
        std::fs::create_dir_all(tool.join("sub")).unwrap();
        let r = store
            .resolve_project_with("m", "box", &tool.join("sub"), &dirs)
            .unwrap();
        assert_eq!(r.project.id, p.id);
    }

    #[test]
    fn redirected_documents_and_normalized_paths() {
        let (_d, _store, root, mut dirs) = auto_env();
        let chat = root.join("cloud/Documents/Codex/2026-01-02/chat");
        assert!(!dirs.contains(&chat));
        dirs.documents.push(root.join("cloud/Documents"));
        assert!(dirs.contains(&chat));
        assert!(!dirs.contains(&root.join("cloud/Documents/Codex")));
        assert!(!dirs.contains(&root.join("cloud/Documents/Game")));

        let n = normalize(&root.join("a/b/../c/./d/"));
        assert_eq!(n, root.join("a/c/d"));
        #[cfg(unix)]
        {
            let p = NonProjectDirs::from_process();
            assert!(p.contains(Path::new("/tmp/agent/run")));
            assert!(p.contains(Path::new("/var/tmp/agent")));
        }
    }

    #[test]
    fn missing_folders_in_other_spellings_resolve_to_one_project() {
        let (_d, store, root, dirs) = auto_env();
        let gone = root.join("home/old-project");
        let first = store
            .resolve_project_lenient("m", "box", &gone, None, &dirs)
            .unwrap();
        assert!(first.created);
        let s = gone.display().to_string();
        let mut spellings = vec![format!("{s}/"), format!("{s}/sub/../sub")];
        if cfg!(windows) {
            spellings.push(format!(r"\\?\{s}"));
            spellings.push(s.replace('\\', "/"));
            spellings.push(s.to_uppercase());
        } else {
            spellings.push(s.replace("/old-project", "//old-project"));
        }
        for sp in spellings {
            let r = store
                .resolve_project_lenient("m", "box", Path::new(&sp), None, &dirs)
                .unwrap();
            assert_eq!(r.project.id, first.project.id, "{sp}");
            assert!(!r.created);
        }
        assert_eq!(store.list_project_summaries("m").unwrap().len(), 1);
    }

    fn external(id: &str, project: &str, machine: &str) -> Session {
        let mut s = super::super::sessions::tests::session(id, project, 1);
        s.machine_id = machine.into();
        s.origin = SessionOrigin::External;
        s
    }

    fn record(id: &str, project: &str, by: &str, pinned: bool) -> Record {
        Record {
            id: id.into(),
            project_id: project.into(),
            kind: RecordKind::Decision,
            title: format!("title {id}"),
            body: String::new(),
            status: RecordStatus::Active,
            pinned,
            source_session_id: None,
            created_at: 1,
            updated_at: 1,
            updated_by: by.into(),
        }
    }

    #[test]
    fn retire_merges_only_untouched_scratch_projects_into_home() {
        let (_d, store, root, launch_dirs) = auto_env();
        // Projects as the old rules created them.
        let old = launch(&root.join("home"));
        let make = |rel: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(&p).unwrap();
            store
                .resolve_project_with("m", "box", &p, &old)
                .unwrap()
                .project
        };
        let chat = make("home/Documents/Codex/2026-01-02/chat");
        let tmp = make("tmp/run");
        let renamed = make("tmp/renamed");
        let user_record = make("tmp/user-record");
        let pinned = make("tmp/pinned");
        let user_brief = make("tmp/user-brief");
        let launched = make("tmp/launched");
        let foreign_session = make("tmp/foreign-session");
        let real = make("home/code/app");
        let no_session = make("tmp/no-session");
        let remote = make("tmp/remote");
        // Another machine may have joined a project by its remote.
        let mut pp = store.project_paths(&remote.id).unwrap().remove(0);
        pp.git_remote = Some("example.com/acme/app".into());
        store.apply(Change::ProjectPath(pp)).unwrap();
        // Registered by the user (Add folder): marked, never retired.
        std::fs::create_dir_all(root.join("tmp/added")).unwrap();
        let added = store
            .register_project("m", &root.join("tmp/added"), None)
            .unwrap();
        assert_eq!(added.updated_at, added.created_at + 1);
        // Not named after its folder (e.g. created under another name).
        let odd = make("tmp/odd");
        let mut odd_row = store.get_project(&odd.id).unwrap().unwrap();
        odd_row.name = "Other".into();
        store
            .write(|tx| {
                tx.execute(
                    "UPDATE projects SET name = ?1 WHERE id = ?2",
                    params![odd_row.name, odd_row.id],
                )?;
                Ok(())
            })
            .unwrap();
        store.set_replication(true).unwrap();
        for (i, p) in [
            &renamed,
            &user_record,
            &pinned,
            &user_brief,
            &remote,
            &added,
            &odd,
        ]
        .into_iter()
        .enumerate()
        {
            store
                .insert_session(&external(&format!("k{i}"), &p.id, "m"))
                .unwrap();
        }

        store
            .insert_session(&external("s1", &chat.id, "m"))
            .unwrap();
        store.insert_session(&external("s2", &tmp.id, "m")).unwrap();
        store
            .create_record(record("r1", &chat.id, BY_DISTILLER, false))
            .unwrap();
        store
            .put_brief(&chat.id, "chat brief", BY_DISTILLER)
            .unwrap();
        store.rename_project(&renamed.id, "Mine").unwrap();
        store
            .create_record(record("r2", &user_record.id, "user", false))
            .unwrap();
        store
            .create_record(record("r3", &pinned.id, BY_DISTILLER, true))
            .unwrap();
        store.put_brief(&user_brief.id, "mine", "user").unwrap();
        let mut blirp_session = external("s3", &launched.id, "m");
        blirp_session.origin = SessionOrigin::Blirp;
        store.insert_session(&blirp_session).unwrap();
        store
            .insert_session(&external("s4", &foreign_session.id, "other"))
            .unwrap();
        store
            .insert_session(&external("s5", &real.id, "m"))
            .unwrap();
        let before = store.outbox_head().unwrap();

        let mut retired: Vec<String> = store
            .retire_non_projects("m", "box", &launch_dirs)
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        retired.sort();
        assert_eq!(retired, ["chat", "run"]);
        let home = store.home_project_id().unwrap().unwrap();
        for gone in [&chat, &tmp] {
            let p = store.get_project(&gone.id).unwrap().unwrap();
            assert!(p.deleted);
            assert!(store.project_paths(&gone.id).unwrap().is_empty());
        }
        assert!(store.project_paths(&home).unwrap().is_empty());
        let moved: Vec<String> = store
            .sessions_of_project(&home)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(moved.len(), 2);
        let recs = store.list_records(&home, &Default::default()).unwrap();
        assert_eq!(recs.len(), 1);
        // The retired projects' briefs stay behind.
        assert!(store.get_brief(&home).unwrap().is_none());
        for kept in [
            &renamed,
            &user_record,
            &pinned,
            &user_brief,
            &launched,
            &foreign_session,
            &real,
            &no_session,
            &remote,
            &added,
            &odd,
        ] {
            assert!(!store.get_project(&kept.id).unwrap().unwrap().deleted);
        }

        // Every change is queued for the hub: the deletes, the moved
        // sessions and records.
        let queued = store.outbox_after(before, 100).unwrap();
        let has =
            |entity: &str, key: &str| queued.iter().any(|e| e.entity == entity && e.key == key);
        assert!(has("projects", &chat.id) && has("projects", &tmp.id));
        assert!(has("sessions", "s1") && has("sessions", "s2"));
        assert!(has("records", "r1"));

        // A session resolved before the cleanup cannot land in a retired
        // project; its caller resolves again.
        assert!(matches!(
            store.insert_session(&external("late", &chat.id, "m")),
            Err(StoreError::Conflict(_))
        ));
        // Sessions already there stay writable.
        store.delete_project(&real.id).unwrap();
        let mut s5 = store.get_session("s5").unwrap().unwrap();
        s5.title = Some("still writable".into());
        store.apply(Change::Session(s5)).unwrap();

        // Idempotent.
        assert!(
            store
                .retire_non_projects("m", "box", &launch_dirs)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn non_git_folder_is_a_project_and_subfolders_resolve_to_it() {
        let (dir, store) = temp_store();
        let proj = dir.path().join("plain-folder");
        std::fs::create_dir_all(proj.join("scripts/deep")).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();

        let r = store
            .resolve_project_with("m1", "box", &proj, &launch(&home))
            .unwrap();
        assert!(r.created && !r.is_home);
        assert_eq!(r.project.name, "plain-folder");
        assert_eq!(r.root, dunce::canonicalize(&proj).unwrap());

        let sub = store
            .resolve_project_with("m1", "box", &proj.join("scripts/deep"), &launch(&home))
            .unwrap();
        assert_eq!(sub.project.id, r.project.id);
        assert!(!sub.created);

        // Other machines do not share non-git paths.
        let other = store
            .resolve_project_with("m2", "b2", &proj, &launch(&home))
            .unwrap();
        assert_ne!(other.project.id, r.project.id);
    }

    #[test]
    fn home_and_root_map_to_home_project() {
        let (dir, store) = temp_store();
        let home = dunce::canonicalize(dir.path()).unwrap();
        let a = store
            .resolve_project_with("m1", "box", &home, &launch(&home))
            .unwrap();
        assert!(a.is_home);
        assert_eq!(a.project.name, "Home (box)");
        let root = home.ancestors().last().unwrap().to_path_buf();
        let b = store
            .resolve_project_with("m1", "box", &root, &launch(&home))
            .unwrap();
        assert!(b.is_home);
        assert_eq!(a.project.id, b.project.id);
        // Any ancestor of home (`C:\Users`, `/home`) is not a project either.
        let parent = home.parent().unwrap();
        let p = store
            .resolve_project_with("m1", "box", parent, &launch(&home))
            .unwrap();
        assert!(p.is_home && !p.created);
        assert_eq!(p.project.id, a.project.id);
        assert_eq!(
            store.home_project_id().unwrap().as_deref(),
            Some(a.project.id.as_str())
        );
        // The home dir was not registered, so a later subfolder still gets its own project.
        std::fs::create_dir(home.join("child")).unwrap();
        let c = store
            .resolve_project_with("m1", "box", &home.join("child"), &launch(&home))
            .unwrap();
        assert!(!c.is_home && c.created);
    }

    #[test]
    fn git_repos_worktrees_and_remotes() {
        if !git::is_installed() {
            eprintln!("git missing; skipping");
            return;
        }
        let (dir, store) = temp_store();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let repo = dir.path().join("app");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        run_git(&repo, &["init"]);
        run_git(
            &repo,
            &["remote", "add", "origin", "https://github.com/Acme/App.git"],
        );
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);

        // A subfolder of an unregistered repo resolves to the repo root.
        let r = store
            .resolve_project_with("m1", "box", &repo.join("src"), &launch(&home))
            .unwrap();
        assert!(r.created);
        assert_eq!(r.root, dunce::canonicalize(&repo).unwrap());
        let paths = store.project_paths(&r.project.id).unwrap();
        assert_eq!(paths[0].git_remote.as_deref(), Some("github.com/acme/app"));

        // A linked worktree outside the repo resolves to the same project.
        let wt = dir.path().join("wt");
        git::worktree_add(&repo, &wt, "blirp/x").unwrap();
        let w = store
            .resolve_project_with("m1", "box", &wt, &launch(&home))
            .unwrap();
        assert_eq!(w.project.id, r.project.id);
        assert!(!w.created);

        // Another machine with a clone of the same remote attaches to the same project.
        let clone = dir.path().join("clone");
        std::fs::create_dir(&clone).unwrap();
        run_git(&clone, &["init"]);
        run_git(
            &clone,
            &["remote", "add", "origin", "git@github.com:acme/app.git"],
        );
        let c = store
            .resolve_project_with("m2", "b2", &clone, &launch(&home))
            .unwrap();
        assert_eq!(c.project.id, r.project.id);
        assert!(!c.created);
        assert_eq!(store.project_paths(&r.project.id).unwrap().len(), 2);
    }

    #[test]
    fn codex_worktrees_resolve_to_their_repo() {
        if !git::is_installed() {
            eprintln!("git missing; skipping");
            return;
        }
        let (_d, store, root, dirs) = auto_env();
        let repo = root.join("home/code/app");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init"]);
        run_git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://example.com/acme/app.git",
            ],
        );
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);
        let wt = root.join("home/.codex/worktrees/abcd/app");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        git::worktree_add(&repo, &wt, "codex/x").unwrap();

        let r = store.resolve_project_with("m", "box", &wt, &dirs).unwrap();
        assert!(r.created && !r.is_home);
        assert_eq!(r.root, repo);

        // Gone: the transcript's remote finds the repo's project.
        let gone = root.join("home/.codex/worktrees/ef01/app");
        let g = store
            .resolve_project_lenient(
                "m",
                "box",
                &gone,
                Some("git@example.com:acme/app.git"),
                &dirs,
            )
            .unwrap();
        assert_eq!(g.project.id, r.project.id);
        // Without one it is scratch.
        let h = store
            .resolve_project_lenient("m", "box", &gone, None, &dirs)
            .unwrap();
        assert!(h.is_home);
    }

    #[test]
    fn register_rename_merge_delete() {
        let (dir, store) = temp_store();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let pa = store.register_project("m", &a, None).unwrap();
        let pb = store.register_project("m", &b, Some("Bee")).unwrap();
        assert_eq!(pb.name, "Bee");
        assert!(matches!(
            store.register_project("m", &a, None),
            Err(StoreError::Conflict(_))
        ));
        assert!(matches!(
            store.register_project("m", &a.join("nope"), None),
            Err(StoreError::Invalid(_))
        ));
        assert_eq!(store.rename_project(&pa.id, " Ay ").unwrap().name, "Ay");

        store.put_brief(&pa.id, "brief a", "user").unwrap();
        store
            .create_wiki_page(&pa.id, "intro", "A", "a", "user")
            .unwrap();
        store
            .create_wiki_page(&pb.id, "intro", "B", "b", "user")
            .unwrap();

        let merged = store.merge_projects(&pa.id, &pb.id).unwrap();
        assert_eq!(merged.id, pb.id);
        assert!(store.get_project(&pa.id).unwrap().unwrap().deleted);
        assert_eq!(store.project_paths(&pb.id).unwrap().len(), 2);
        assert_eq!(store.get_brief(&pb.id).unwrap().unwrap().body_md, "brief a");
        let mut slugs: Vec<String> = store
            .list_wiki(&pb.id)
            .unwrap()
            .into_iter()
            .map(|w| w.slug)
            .collect();
        slugs.sort();
        assert_eq!(slugs, ["intro", "intro-2"]);
        // Resolving the old folder now lands in the merged project.
        let r = store
            .resolve_project_with("m", "box", &a, &NonProjectDirs::default())
            .unwrap();
        assert_eq!(r.project.id, pb.id);

        store.delete_project(&pb.id).unwrap();
        assert!(store.project_paths(&pb.id).unwrap().is_empty());
        assert!(store.list_project_summaries("m").unwrap().is_empty());
    }
}
