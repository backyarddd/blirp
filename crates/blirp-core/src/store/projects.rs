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
        chats: r.get("chats")?,
        merged_into: r.get("merged_into")?,
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
    /// Project root on this machine; for Chats, the cwd itself.
    pub root: PathBuf,
    /// Filed in this machine's Chats (no project).
    pub is_home: bool,
    pub created: bool,
}

/// Files and folders that make a folder an actual project for sessions
/// blirp did not start (§5 step 3; git work trees count by `git`): build
/// manifests, other version control, and agent configuration someone
/// wrote for the folder. Matched case-insensitively.
const PROJECT_MARKERS: &[&str] = &[
    // Version control other than git.
    ".hg",
    ".svn",
    ".jj",
    // Agent configuration for this folder.
    ".mcp.json",
    "AGENTS.md",
    "CLAUDE.md",
    // Build manifests.
    "package.json",
    "deno.json",
    "deno.jsonc",
    "Cargo.toml",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "Pipfile",
    "requirements.txt",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "build.sbt",
    "Gemfile",
    "composer.json",
    "mix.exs",
    "Package.swift",
    "pubspec.yaml",
    "CMakeLists.txt",
    "meson.build",
    "stack.yaml",
    "deps.edn",
    "project.clj",
    "project.godot",
    "default.project.json",
];

/// File name extensions that mark a project the same way (.NET solutions
/// and projects, Haskell, Ruby gems, Xcode, Unreal).
const PROJECT_MARKER_EXTENSIONS: &[&str] = &[
    "sln",
    "csproj",
    "fsproj",
    "vbproj",
    "vcxproj",
    "cabal",
    "gemspec",
    "xcodeproj",
    "xcworkspace",
    "uproject",
];

fn has_project_marker(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        PROJECT_MARKERS
            .iter()
            .any(|m| m.eq_ignore_ascii_case(&name))
            || Path::new(name.as_ref())
                .extension()
                .map(|x| x.to_string_lossy())
                .is_some_and(|x| {
                    PROJECT_MARKER_EXTENSIONS
                        .iter()
                        .any(|m| m.eq_ignore_ascii_case(&x))
                })
    })
}

/// Folders that never become project roots (§5 step 3): the home folder,
/// its ancestors and filesystem roots. For sessions blirp did not start
/// (`auto`: ingested transcripts, hooks) also the places agents and tools
/// run scratch work in: hidden folders directly under home (tool data such
/// as `~/.codex`, `~/.claude`, `~/.blirp`), Codex desktop chat folders
/// (`<Documents>/Codex/<YYYY-MM-DD>/<chat>`, one per chat, for each folder
/// in `documents`), `scratch` (temp and system folders) and the Desktop,
/// Downloads and Documents folders themselves. Registered folders and
/// blirp workspaces always win: resolution matches them before these rules
/// apply.
#[derive(Debug, Clone, Default)]
pub struct NonProjectDirs {
    pub home: Option<PathBuf>,
    pub scratch: Vec<PathBuf>,
    /// Documents folders (`~/Documents` and the OS's, which may be
    /// redirected, e.g. to OneDrive).
    pub documents: Vec<PathBuf>,
    pub auto: bool,
    /// `BLIRP_HOME/workspaces`: a folder inside `<workspaces>/<project id>`
    /// belongs to that project (a project without folders, §5), before
    /// every other rule.
    pub workspaces: Option<PathBuf>,
}

impl NonProjectDirs {
    /// Map folders inside `dir` (`BLIRP_HOME/workspaces`) to their projects.
    #[must_use]
    pub fn with_workspaces(mut self, dir: &Path) -> Self {
        self.workspaces = Some(normalize(dir));
        self
    }

    /// `(project id, workspace root)` when `p` (absolute) is inside a
    /// project's workspace. Project ids are lowercase, like `path_key`.
    pub(super) fn workspace_of(&self, p: &Path) -> Option<(String, PathBuf)> {
        use crate::paths::path_key;
        let ws = self.workspaces.as_deref()?;
        let rel = path_key(p).strip_prefix(path_key(ws)).ok()?.to_path_buf();
        let id = rel.components().next()?.as_os_str().to_str()?.to_string();
        crate::is_safe_id(&id).then(|| (id.clone(), ws.join(id)))
    }

    /// The folder a session in `cwd` (not in a git work tree) makes its
    /// project root under the auto rules: the nearest folder from `cwd` up
    /// with a [`PROJECT_MARKERS`] entry, stopping at the first folder that
    /// [`Self::contains`] (home at the latest). None: the session is a chat.
    fn marker_root(&self, cwd: &Path) -> Option<PathBuf> {
        cwd.ancestors()
            .take_while(|d| !self.contains(d))
            .find(|d| has_project_marker(d))
            .map(Path::to_path_buf)
    }

    /// Whether `dir` (existing, absolute) is an actual project folder for
    /// sessions blirp did not start: in a git work tree, or at or below a
    /// folder with a project marker.
    fn is_project_folder(&self, dir: &Path) -> bool {
        git::find_git_root_fs(dir).is_some() || self.marker_root(dir).is_some()
    }

    /// For sessions the user starts in blirp: only home and roots.
    pub fn launch() -> Self {
        Self {
            home: crate::paths::user_home().map(|h| normalize(&h)),
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
            dirs.documents.push(normalize(&d));
        }
        dirs
    }

    /// Auto rules for `home` (with `home/Documents`) and `scratch`.
    /// Folders are kept [`normalize`]d, the form every path they are
    /// matched against has: `$TMPDIR` on macOS is `/private/var/folders/..`
    /// and a Windows `%TEMP%` in 8.3 form (`RUNNER~1`) is the long name.
    pub fn auto(home: Option<PathBuf>, scratch: Vec<PathBuf>) -> Self {
        let home = home.map(|h| normalize(&h));
        Self {
            documents: home
                .iter()
                .map(|h| normalize(&h.join("Documents")))
                .collect(),
            home,
            scratch: scratch
                .into_iter()
                .filter(|d| d.is_absolute())
                .map(|d| normalize(&d))
                .collect(),
            auto: true,
            workspaces: None,
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
        // Desktop, Downloads and Documents themselves (not their subfolders).
        let exact = |d: &Path| under(d).is_some_and(|rel| rel.as_os_str().is_empty());
        if self
            .home
            .iter()
            .flat_map(|h| [h.join("Desktop"), h.join("Downloads")])
            .chain(self.documents.iter().cloned())
            .any(|d| exact(&d))
        {
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

/// A recorded folder in the spelling [`canonical_dir`] gives existing ones:
/// [`lexical`], then its deepest existing ancestor canonicalized and the
/// rest kept. For folders that no longer exist: a gone `/tmp/run` on macOS
/// is `/private/tmp/run`, like the (canonical) roots and registered folders
/// it is matched against.
fn normalize(p: &Path) -> PathBuf {
    let out = lexical(p);
    for base in out.ancestors() {
        match dunce::canonicalize(base) {
            Ok(canon) => {
                return match out.strip_prefix(base) {
                    Ok(rest) if !rest.as_os_str().is_empty() => canon.join(rest),
                    _ => canon,
                };
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            // Anything else (e.g. an offline network share) fails the same
            // way further up, slowly; keep the lexical spelling.
            Err(_) => break,
        }
    }
    out
}

/// `p` without a `\\?\` prefix (`dunce` keeps `\\?\UNC\`), trailing or
/// doubled separators, with `.` and `..` resolved lexically and `/` as `\`
/// on Windows.
fn lexical(p: &Path) -> PathBuf {
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

fn check_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(StoreError::Invalid("name must be 1-200 characters".into()));
    }
    Ok(name)
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

/// `registered`: made by the user (New project, Add folder). Such a
/// project starts with `updated_at = created_at + 1`, which tells it apart
/// from one resolution created (`updated_at = created_at` until its first
/// edit) for [`Store::retire_non_projects`].
fn new_project(tx: &Transaction<'_>, name: &str, registered: bool) -> Result<Project> {
    let now = crate::now_ms();
    let p = Project {
        id: crate::new_id(),
        name: name.to_string(),
        created_at: now,
        updated_at: now + i64::from(registered),
        deleted: false,
        chats: false,
        merged_into: None,
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
                        // Filled in by the API, which knows BLIRP_HOME.
                        workspace: None,
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
        if let Some(r) = self.resolve_workspace(machine_id, machine_name, dirs, &cwd)? {
            return Ok(r);
        }
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
                // Sessions blirp did not start make a project only of an
                // actual project folder; the rest are chats.
                None if dirs.auto => match dirs.marker_root(&cwd) {
                    Some(root) => {
                        if let Some(r) = found(&paths, &root, tx)? {
                            return Ok(r);
                        }
                        (root, None)
                    }
                    None => (PathBuf::new(), None),
                },
                None => (cwd.clone(), None),
            };
            // 3. Home dir, its ancestors (`C:\Users`, `/home`), filesystem
            // roots and (auto) scratch folders are never project roots.
            if root.as_os_str().is_empty() || dirs.contains(&root) {
                return Ok(ResolvedProject {
                    project: home_project(tx, machine_id, machine_name)?,
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
        let spelled = lexical(cwd);
        let cwd = &normalize(cwd);
        if let Some(r) = self.resolve_workspace(machine_id, machine_name, dirs, cwd)? {
            return Ok(r);
        }
        self.write(|tx| {
            let paths = live_local_paths(tx, machine_id)?;
            // Rows recorded before gone folders were canonicalized (0.1.0)
            // hold the lexical spelling (`/tmp/x` on macOS, a mapped drive).
            if let Some(pp) =
                longest_prefix(&paths, cwd).or_else(|| longest_prefix(&paths, &spelled))
            {
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
            // A gone folder cannot show it was a project (auto rules).
            if dirs.auto || dirs.contains(cwd) {
                return Ok(ResolvedProject {
                    project: home_project(tx, machine_id, machine_name)?,
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
        let name = match name {
            Some(n) => check_name(n)?.to_string(),
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

    /// A project without folders (New project): its sessions start in a
    /// blirp workspace on each machine (§5). `brief` becomes its first brief
    /// version, written by the user.
    pub fn create_project(&self, name: &str, brief: Option<&str>) -> Result<Project> {
        let name = check_name(name)?;
        self.write(|tx| {
            let p = new_project(tx, name, true)?;
            if let Some(b) = brief.map(str::trim).filter(|b| !b.is_empty()) {
                super::memory::put_brief_in(tx, &p.id, b, "user")?;
            }
            Ok(p)
        })
    }

    /// Register `dir` (its git top level inside a repository) as a folder
    /// of `project_id` on this machine, for a session the user starts there
    /// in that project. Returns the registered root. A folder inside another
    /// project's folder here is a conflict; one already inside this
    /// project's folders is left as it is.
    pub fn add_project_folder(
        &self,
        project_id: &str,
        machine_id: &str,
        dir: &Path,
    ) -> Result<PathBuf> {
        self.add_project_folder_then(project_id, machine_id, dir, |_| Ok(()))
    }

    /// [`Self::add_project_folder`], running `then` in the same transaction
    /// once the folder is registered (or found registered to the project).
    pub(crate) fn add_project_folder_then(
        &self,
        project_id: &str,
        machine_id: &str,
        dir: &Path,
        then: impl FnOnce(&Transaction<'_>) -> Result<()>,
    ) -> Result<PathBuf> {
        let dir = canonical_dir(dir)?;
        let repo = git::repo_info(&dir).ok().flatten();
        let (root, remote) = match repo {
            Some(r) => (r.main_root, r.remote),
            None => (dir, None),
        };
        if NonProjectDirs::launch().contains(&root) {
            return Err(StoreError::Invalid(format!(
                "{} cannot be a project folder (home folder or a filesystem root)",
                root.display()
            )));
        }
        self.write(|tx| {
            let p = live_project_in(tx, project_id)?;
            if p.chats {
                return Err(StoreError::Invalid("Chats has no folders".into()));
            }
            let paths = live_local_paths(tx, machine_id)?;
            if let Some(pp) = longest_prefix(&paths, &root) {
                if pp.project_id == project_id {
                    then(tx)?;
                    return Ok(PathBuf::from(&pp.path));
                }
                let other = live_project_in(tx, &pp.project_id)?;
                return Err(StoreError::Conflict(format!(
                    "{} belongs to project \"{}\"; start the session there or merge the projects",
                    root.display(),
                    other.name
                )));
            }
            attach_path(tx, project_id, machine_id, &root, remote)?;
            then(tx)?;
            Ok(root)
        })
    }

    /// Unregister one of this machine's folders of a project. The project
    /// stays, also when it has no folder left: its sessions and memory are
    /// kept, and new sessions start in its blirp workspace.
    pub fn remove_project_folder(
        &self,
        project_id: &str,
        machine_id: &str,
        path: &str,
    ) -> Result<()> {
        self.write(|tx| {
            let mut p = live_project_in(tx, project_id)?;
            let n: Option<i64> = one(
                tx,
                "SELECT 1 FROM project_paths WHERE project_id = ?1 AND machine_id = ?2 AND path = ?3",
                params![project_id, machine_id, path],
                |r| r.get(0),
            )?;
            if n.is_none() {
                return Err(StoreError::NotFound(
                    "folder of this project on this machine",
                ));
            }
            apply_in(
                tx,
                &Change::DeleteProjectPath {
                    machine_id: machine_id.to_string(),
                    path: path.to_string(),
                },
            )?;
            // Edited by the user: never retired as a scratch project.
            p.updated_at = crate::now_ms();
            apply_in(tx, &Change::Project(p))?;
            Ok(())
        })
    }

    /// Move a session (with its ingested subagents and the records it
    /// produced in its old project) into `into`, or with `None` (or a Chats
    /// project) into the Chats of the session's machine. That bucket is
    /// created only by its own machine: another machine's session moves to
    /// Chats once that machine has one. Into Chats only the session's
    /// unpinned distiller records move; records someone wrote or pinned stay
    /// in the project.
    pub fn move_session(
        &self,
        session_id: &str,
        into: Option<&str>,
        machine_id: &str,
        machine_name: &str,
    ) -> Result<Session> {
        self.write(|tx| {
            let mut s = one(
                tx,
                "SELECT * FROM sessions WHERE id = ?1",
                params![session_id],
                super::sessions::session_row,
            )?
            .ok_or(StoreError::NotFound("session"))?;
            let explicit = match into {
                Some(id) => Some(live_project_in(tx, id)?).filter(|p| !p.chats),
                None => None,
            };
            let target = match explicit {
                Some(p) => p,
                None if s.machine_id == machine_id => home_project(tx, machine_id, machine_name)?,
                None => get_project_in(tx, &chats_id(&s.machine_id))?
                    .filter(|p| p.chats && !p.deleted)
                    .ok_or_else(|| {
                        StoreError::Conflict(
                            "the machine that ran this session has no Chats yet; move it there once it has synced a chat"
                                .into(),
                        )
                    })?,
            };
            if s.project_id == target.id {
                return Ok(s);
            }
            let from = std::mem::replace(&mut s.project_id, target.id.clone());
            let now = crate::now_ms();
            let mut changes = vec![Change::Session(s.clone())];
            for mut c in all(
                tx,
                "SELECT * FROM sessions WHERE parent_session_id = ?1 AND origin = 'external'
                   AND project_id = ?2",
                params![session_id, from],
                super::sessions::session_row,
            )? {
                c.project_id.clone_from(&target.id);
                changes.push(Change::Session(c));
            }
            for mut r in all(
                tx,
                "SELECT * FROM records WHERE source_session_id = ?1 AND project_id = ?2
                   AND (?3 = 0 OR (updated_by = ?4 AND pinned = 0))",
                params![session_id, from, target.chats, super::BY_DISTILLER],
                super::memory::record_row,
            )? {
                r.project_id.clone_from(&target.id);
                r.updated_at = now;
                changes.push(Change::Record(r));
            }
            for c in &changes {
                super::apply_move_in(tx, c)?;
            }
            // As stored, with the move's edit time (§10).
            Ok(one(
                tx,
                "SELECT * FROM sessions WHERE id = ?1",
                params![session_id],
                super::sessions::session_row,
            )?
            .unwrap_or(s))
        })
    }

    /// Give this machine its Chats bucket in place of blirp 0.1.0's Home
    /// project: an untouched Home is merged into it (sessions and records;
    /// the Home project is removed), one with memory someone wrote (renamed,
    /// a user brief version, a user or pinned record, a wiki page or a
    /// resource) or with another machine's sessions in it stays a normal
    /// project. Through `apply`, so every machine
    /// learns it. Returns whether anything changed.
    pub fn ensure_chats(&self, machine_id: &str, machine_name: &str) -> Result<bool> {
        let Some(id) = self.home_project_id()? else {
            return Ok(false);
        };
        if id == chats_id(machine_id) {
            return Ok(false);
        }
        self.write(|tx| {
            let Some(old) = get_project_in(tx, &id)?.filter(|p| !p.deleted) else {
                return Ok(false);
            };
            // A bucket under another id (an earlier 0.1.1 build, or this
            // machine's id before `rebind_machine`) joins the canonical one.
            // Not when it is another machine's (a database copied from
            // another machine): that one keeps it.
            if old.chats {
                let foreign: Option<i64> = one(
                    tx,
                    "SELECT 1 WHERE EXISTS (SELECT 1 FROM sessions WHERE project_id = ?1
                                            AND machine_id != ?2)
                       OR EXISTS (SELECT 1 FROM machines WHERE 'chats-' || id = ?1 AND id != ?2)",
                    params![old.id, machine_id],
                    |r| r.get(0),
                )?;
                let bucket = chats_bucket(tx, machine_id, machine_name)?;
                if foreign.is_none() {
                    merge_in(tx, &old.id, &bucket.id, true)?;
                }
                return Ok(true);
            }
            let authored: Option<i64> = one(
                tx,
                "SELECT 1 WHERE EXISTS (SELECT 1 FROM records WHERE project_id = ?1
                                        AND (updated_by != ?2 OR pinned != 0))
                   OR EXISTS (SELECT 1 FROM brief_history WHERE project_id = ?1 AND updated_by != ?2)
                   OR EXISTS (SELECT 1 FROM wiki_pages WHERE project_id = ?1)
                   OR EXISTS (SELECT 1 FROM resources WHERE project_id = ?1)
                   OR EXISTS (SELECT 1 FROM sessions WHERE project_id = ?1 AND machine_id != ?3)",
                params![old.id, super::BY_DISTILLER, machine_id],
                |r| r.get(0),
            )?;
            let untouched = authored.is_none() && old.name == format!("Home ({machine_name})");
            let bucket = chats_bucket(tx, machine_id, machine_name)?;
            if untouched {
                merge_in(tx, &old.id, &bucket.id, true)?;
            }
            Ok(true)
        })
    }

    /// A folder inside a blirp workspace: its project (or the project it
    /// was merged into), else (a removed project) Chats.
    fn resolve_workspace(
        &self,
        machine_id: &str,
        machine_name: &str,
        dirs: &NonProjectDirs,
        cwd: &Path,
    ) -> Result<Option<ResolvedProject>> {
        let Some((id, root)) = dirs.workspace_of(cwd) else {
            return Ok(None);
        };
        self.write(|tx| {
            let live = follow_merged(tx, &id)?;
            Ok(Some(match live {
                Some(project) => ResolvedProject {
                    project,
                    root,
                    is_home: false,
                    created: false,
                },
                None => ResolvedProject {
                    project: home_project(tx, machine_id, machine_name)?,
                    root: cwd.to_path_buf(),
                    is_home: true,
                    created: false,
                },
            }))
        })
    }

    pub fn rename_project(&self, id: &str, name: &str) -> Result<Project> {
        let name = check_name(name)?;
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
            if p.chats {
                return Err(StoreError::Invalid("Chats cannot be deleted".into()));
            }
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
        self.write(|tx| {
            if live_project_in(tx, from)?.chats || live_project_in(tx, into)?.chats {
                return Err(StoreError::Invalid(
                    "Chats is no project; move sessions with the session's Move instead".into(),
                ));
            }
            merge_in(tx, from, into, false)
        })
    }

    /// Cleanup for projects that resolution created before `dirs` said
    /// their folder is no project (§5): each is merged into this machine's
    /// Chats, where resolution now files such sessions, without its folders
    /// or brief. Only projects that show no sign of the user
    /// ([`untouched_projects`]) whose every folder matches the scratch rules
    /// ([`NonProjectDirs::contains`]); plain folders without a project
    /// marker are only offered ([`Store::chat_candidates`]), since blirp
    /// 0.1.0 did not mark projects the user added. Sessions, records and
    /// suggestions move to Chats; the project is soft-deleted, so every
    /// change replicates. Returns the merged projects (as they were).
    pub fn retire_non_projects(
        &self,
        machine_id: &str,
        machine_name: &str,
        dirs: &NonProjectDirs,
    ) -> Result<Vec<Project>> {
        self.write(|tx| {
            let mut retired = Vec::new();
            for (p, paths) in untouched_projects(tx, machine_id)? {
                if !paths
                    .iter()
                    .all(|pp| dirs.contains(&normalize(Path::new(&pp.path))))
                {
                    continue;
                }
                let home = home_project(tx, machine_id, machine_name)?;
                merge_in(tx, &p.id, &home.id, true)?;
                retired.push(p);
            }
            Ok(retired)
        })
    }

    /// Projects that look like chats under the auto rules and are offered
    /// to the user to move to Chats: untouched ([`untouched_projects`]),
    /// every folder existing and no actual project folder (no git work
    /// tree, no project marker at or above it). A folder that is gone or
    /// unreachable (an unmounted drive) cannot show that.
    pub fn chat_candidates(&self, machine_id: &str, dirs: &NonProjectDirs) -> Result<Vec<Project>> {
        let found = self.read(|c| untouched_projects(c, machine_id))?;
        Ok(found
            .into_iter()
            .filter(|(_, paths)| looks_like_chats(dirs, paths))
            .map(|(p, _)| p)
            .collect())
    }

    /// Move a whole project into this machine's Chats, as the cleanup does
    /// (the user confirmed it): sessions, records and suggestions move, its
    /// folders and brief stay with the removed project.
    pub fn move_project_to_chats(
        &self,
        id: &str,
        machine_id: &str,
        machine_name: &str,
        dirs: &NonProjectDirs,
    ) -> Result<()> {
        let refused = || {
            StoreError::Conflict(
                "this project is not one that looks like chats (it was used, edited, or has another machine's folders or sessions)".into(),
            )
        };
        let offered = |c: &Connection| -> Result<Option<Vec<ProjectPath>>> {
            Ok(untouched_projects(c, machine_id)?
                .into_iter()
                .find(|(p, _)| p.id == id)
                .map(|(_, paths)| paths))
        };
        if self.read(|c| live_project_in(c, id))?.chats {
            return Err(StoreError::Invalid("already Chats".into()));
        }
        // Filesystem checks outside the write lock; under it only that the
        // project is still untouched with the same folders.
        let paths = self.read(|c| offered(c))?.ok_or_else(refused)?;
        if !looks_like_chats(dirs, &paths) {
            return Err(refused());
        }
        self.write(|tx| {
            if offered(tx)?.as_ref() != Some(&paths) {
                return Err(refused());
            }
            let home = home_project(tx, machine_id, machine_name)?;
            merge_in(tx, id, &home.id, true)?;
            Ok(())
        })
    }

    /// `id`'s live project (Chats included), following merges; None when it
    /// was removed.
    pub fn current_project(&self, id: &str) -> Result<Option<Project>> {
        self.read(|c| current_project_in(c, id))
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

/// Projects that show no sign of the user, with their folders: created by
/// resolution and never renamed or merged into (`updated_at = created_at`;
/// ones the user made start one higher), no Chats, named after one of their
/// folders, at least one session and every session this machine's and not
/// started in blirp, every folder on this machine and none with a git
/// remote (another machine may have joined it by that remote), no wiki
/// pages or resources, only unpinned distiller records and distiller brief
/// versions.
fn untouched_projects(
    c: &Connection,
    machine_id: &str,
) -> Result<Vec<(Project, Vec<ProjectPath>)>> {
    let home_id: Option<String> = one(
        c,
        "SELECT value_json FROM settings WHERE key = ?1",
        params![HOME_PROJECT_KEY],
        |r| r.get::<_, String>(0),
    )?
    .and_then(|v| serde_json::from_str(&v).ok());
    let candidates = all(
        c,
        "SELECT p.* FROM projects p
         WHERE p.deleted = 0 AND p.chats = 0 AND p.updated_at = p.created_at AND p.id != ?2
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
           AND NOT EXISTS (SELECT 1 FROM resources x WHERE x.project_id = p.id)
         ORDER BY p.name",
        params![machine_id, home_id.unwrap_or_default(), super::BY_DISTILLER],
        project_row,
    )?;
    let mut out = Vec::new();
    for p in candidates {
        let paths = all(
            c,
            "SELECT * FROM project_paths WHERE project_id = ?1 ORDER BY machine_id, path",
            params![p.id],
            path_row,
        )?;
        if paths
            .iter()
            .any(|pp| folder_name(Path::new(&pp.path)) == p.name)
        {
            out.push((p, paths));
        }
    }
    Ok(out)
}

/// Folders of a plain-folder project that looks like chats: every one
/// exists, is no scratch place (those are retired on their own) and no
/// actual project folder.
fn looks_like_chats(dirs: &NonProjectDirs, paths: &[ProjectPath]) -> bool {
    paths.iter().all(|pp| {
        let p = normalize(Path::new(&pp.path));
        !dirs.contains(&p) && p.is_dir() && !dirs.is_project_folder(&p)
    })
}

/// `id`'s live project, following merges; None for a removed project or
/// Chats.
pub(super) fn follow_merged(c: &Connection, id: &str) -> Result<Option<Project>> {
    Ok(current_project_in(c, id)?.filter(|p| !p.chats))
}

/// `id`'s live project (Chats included), following merges; None for a
/// removed project.
fn current_project_in(c: &Connection, id: &str) -> Result<Option<Project>> {
    let mut id = id.to_string();
    // Merge chains are short; the bound only guards against a cycle.
    for _ in 0..16 {
        match get_project_in(c, &id)? {
            Some(p) if !p.deleted => return Ok(Some(p)),
            Some(Project {
                merged_into: Some(next),
                ..
            }) => id = next,
            _ => return Ok(None),
        }
    }
    Ok(None)
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
        super::apply_move_in(tx, c)?;
    }
    tx.execute(
        "UPDATE suggestions SET project_id = ?1 WHERE project_id = ?2",
        params![into, from],
    )?;
    src.deleted = true;
    src.merged_into = Some(into.to_string());
    src.updated_at = now;
    apply_in(tx, &Change::Project(src))?;
    dst.updated_at = now;
    apply_in(tx, &Change::Project(dst.clone()))?;
    Ok(dst)
}

fn chats_name(machine_name: &str) -> String {
    format!("Chats ({machine_name})")
}

/// The id of a machine's Chats bucket: fixed, so every machine finds any
/// machine's bucket.
fn chats_id(machine_id: &str) -> String {
    format!("chats-{machine_id}")
}

/// This machine's Chats project, created on first use (called Home before
/// 0.1.1, see [`Store::ensure_chats`]).
fn home_project(tx: &Transaction<'_>, machine_id: &str, machine_name: &str) -> Result<Project> {
    // Only the canonical bucket: one under another id is merged into it by
    // `ensure_chats` at start.
    chats_bucket(tx, machine_id, machine_name)
}

/// Create (or bring back) `chats-<machine_id>` and record it as this
/// machine's Chats.
fn chats_bucket(tx: &Transaction<'_>, machine_id: &str, machine_name: &str) -> Result<Project> {
    let id = chats_id(machine_id);
    let now = crate::now_ms();
    let p = match get_project_in(tx, &id)? {
        Some(p) if !p.deleted && p.chats => p,
        found => {
            let p = Project {
                id: id.clone(),
                name: chats_name(machine_name),
                created_at: found.as_ref().map_or(now, |p| p.created_at),
                updated_at: now,
                deleted: false,
                chats: true,
                merged_into: None,
            };
            apply_in(tx, &Change::Project(p.clone()))?;
            p
        }
    };
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

    /// Like [`NonProjectDirs::launch`] for `home`: in the form candidates
    /// are matched in (a tempdir under macOS `/var` is `/private/var`, a
    /// Windows `%TEMP%` may be 8.3), so the tests do not depend on the OS.
    fn launch(home: &Path) -> NonProjectDirs {
        NonProjectDirs {
            home: Some(normalize(home)),
            ..NonProjectDirs::default()
        }
    }

    /// Another spelling of existing folder `real`: a symlink (unix), the
    /// 8.3 short name (Windows; the long name where the volume has none).
    fn alias_of(real: &Path) -> PathBuf {
        #[cfg(unix)]
        {
            let link = real.with_file_name(format!(
                "{}-link",
                real.file_name().unwrap().to_string_lossy()
            ));
            std::os::unix::fs::symlink(real, &link).unwrap();
            link
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::{OsStrExt, OsStringExt};
            use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
            let wide: Vec<u16> = real.as_os_str().encode_wide().chain([0]).collect();
            let mut buf = vec![0u16; 1024];
            // SAFETY: `wide` is NUL-terminated; `buf` holds `buf.len()` u16s.
            #[allow(unsafe_code)]
            let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), 1024) };
            assert!(
                n > 0 && (n as usize) < buf.len(),
                "GetShortPathNameW failed"
            );
            PathBuf::from(std::ffi::OsString::from_wide(&buf[..n as usize]))
        }
    }

    #[test]
    fn roots_match_whatever_spelling_they_were_given_in() {
        let (_d, _store, root, _) = auto_env();
        let tmp = root.join("a long scratch folder");
        let other = root.join("another long folder");
        for d in [&tmp, &other] {
            std::fs::create_dir_all(d.join("run")).unwrap();
        }
        let (tmp_alias, other_alias) = (alias_of(&tmp), alias_of(&other));
        let home_alias = alias_of(&root.join("home"));
        // Existing roots, and a gone one below an existing folder.
        let dirs = NonProjectDirs::auto(
            Some(home_alias.clone()),
            vec![tmp_alias.clone(), other_alias.join("gone")],
        );
        assert!(dirs.contains(&canonical_dir(&tmp.join("run")).unwrap()));
        assert!(dirs.contains(&canonical_dir(&tmp_alias.join("run")).unwrap()));
        assert!(dirs.contains(&normalize(&tmp_alias.join("gone/x"))));
        assert!(dirs.contains(&normalize(&other_alias.join("gone/x"))));
        assert!(dirs.contains(&normalize(&other.join("gone"))));
        assert!(!dirs.contains(&canonical_dir(&other_alias.join("run")).unwrap()));
        assert!(dirs.contains(&canonical_dir(&root.join("home")).unwrap()));
        assert!(!launch(&root.join("home")).contains(&canonical_dir(&tmp).unwrap()));
        assert!(launch(&home_alias).contains(&canonical_dir(&root.join("home")).unwrap()));
    }

    #[test]
    fn the_process_temp_folder_is_scratch() {
        // `temp_dir()` as the OS spells it: `/var/folders/..` on macOS,
        // possibly 8.3 on Windows runners. Candidates arrive canonical.
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("run")).unwrap();
        let p = NonProjectDirs::from_process();
        assert!(p.contains(&canonical_dir(&t.path().join("run")).unwrap()));
        assert!(p.contains(&normalize(&t.path().join("gone/run"))));
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
        ];
        // Paths are case insensitive only on Windows (`paths::path_key`):
        // elsewhere `documents` can be another folder (macOS volumes can be
        // case sensitive; canonicalizing gives existing folders their case).
        let scratch: Vec<PathBuf> = scratch
            .into_iter()
            .chain(cfg!(windows).then(|| home.join("documents/codex/2026-01-02")))
            .collect();
        for s in &scratch {
            std::fs::create_dir_all(s).unwrap();
            // A project marker does not make scratch space a project.
            std::fs::write(s.join("package.json"), "{}").unwrap();
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

        // Actual project folders, also next to the Codex chats, are
        // projects: a marker (any case) makes one.
        for (real, marker) in [
            (home.join("code/app"), "Cargo.toml"),
            (home.join("Documents/Codex/notes"), "CLAUDE.md"),
            (home.join("Documents/Game"), "game.SLN"),
            (home.join("design"), ".mcp.json"),
        ] {
            std::fs::create_dir_all(&real).unwrap();
            std::fs::write(real.join(marker), "").unwrap();
            let r = store
                .resolve_project_with("m", "box", &real, &dirs)
                .unwrap();
            assert!(r.created && !r.is_home, "{}", real.display());
        }
        // A subfolder files under the folder with the marker, found or new.
        std::fs::create_dir_all(home.join("code/app/src/deep")).unwrap();
        let r = store
            .resolve_project_with("m", "box", &home.join("code/app/src/deep"), &dirs)
            .unwrap();
        assert!(!r.created && r.root == home.join("code/app"));
        std::fs::create_dir_all(home.join("code/lib/src")).unwrap();
        std::fs::write(home.join("code/lib/go.mod"), "").unwrap();
        let r = store
            .resolve_project_with("m", "box", &home.join("code/lib/src"), &dirs)
            .unwrap();
        assert!(r.created && r.root == home.join("code/lib"));
        // Folders without git or a marker are chats, as are Desktop,
        // Downloads and Documents themselves, even with a stray manifest.
        for chat in [
            home.join("notes"),
            home.join("code"),
            home.join("Desktop"),
            home.join("Downloads"),
            home.join("Documents"),
        ] {
            std::fs::create_dir_all(&chat).unwrap();
            if chat.ends_with("Desktop")
                || chat.ends_with("Downloads")
                || chat.ends_with("Documents")
            {
                std::fs::write(chat.join("package.json"), "{}").unwrap();
            }
            let r = store
                .resolve_project_with("m", "box", &chat, &dirs)
                .unwrap();
            assert!(r.is_home && !r.created, "{}", chat.display());
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
            // Gone folders match in canonical form, like the roots: through
            // a symlink here, `/tmp` -> `/private/tmp` on macOS.
            std::os::unix::fs::symlink(root.join("home"), root.join("link")).unwrap();
            assert_eq!(normalize(&root.join("link/gone")), root.join("home/gone"));
            let p = NonProjectDirs::from_process();
            assert!(p.contains(&normalize(Path::new("/tmp/agent/run"))));
            assert!(p.contains(&normalize(Path::new("/var/tmp/agent"))));
        }
    }

    #[test]
    fn missing_folders_in_other_spellings_resolve_to_one_project() {
        let (_d, store, root, dirs) = auto_env();
        let gone = root.join("home/old-project");
        std::fs::create_dir_all(&gone).unwrap();
        let first = store
            .resolve_project_with("m", "box", &gone, &launch(&root.join("home")))
            .unwrap();
        assert!(first.created);
        std::fs::remove_dir(&gone).unwrap();
        // A gone folder that no project had cannot show it was one: Chats.
        let chat = store
            .resolve_project_lenient("m", "box", &root.join("home/other"), None, &dirs)
            .unwrap();
        assert!(chat.is_home && !chat.created);
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
        assert_eq!(store.list_project_summaries("m").unwrap().len(), 2);
    }

    /// A gone folder recorded by 0.1.0 in its lexical spelling (through a
    /// symlink, like `/tmp` on macOS) still resolves to its project.
    #[cfg(unix)]
    #[test]
    fn gone_folders_recorded_in_lexical_spelling_keep_their_project() {
        let (_d, store, root, dirs) = auto_env();
        std::os::unix::fs::symlink(root.join("home"), root.join("link")).unwrap();
        let old = root.join("link/old-project");
        let p = store
            .write(|tx| {
                let p = new_project(tx, "old-project", false)?;
                attach_path(tx, &p.id, "m", &old, None)?;
                Ok(p)
            })
            .unwrap();
        for cwd in [old.clone(), old.join("sub")] {
            let r = store
                .resolve_project_lenient("m", "box", &cwd, None, &dirs)
                .unwrap();
            assert_eq!(r.project.id, p.id, "{}", cwd.display());
            assert!(!r.created);
        }
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
        std::fs::write(root.join("home/code/app/package.json"), "{}").unwrap();
        // Under a folder with a project marker: an actual project too.
        std::fs::create_dir_all(root.join("home/code/lib")).unwrap();
        std::fs::write(root.join("home/code/lib/Cargo.toml"), "").unwrap();
        let sub = make("home/code/lib/src");
        // A plain folder (no git, no marker), shaped as blirp 0.1.0 left
        // both an auto-created project and one the user added with Add
        // folder (that version did not mark those): only offered.
        let notes = make("home/notes");
        // Gone (or an unmounted drive): it cannot show it is no project.
        let gone = make("home/gone");
        std::fs::remove_dir(root.join("home/gone")).unwrap();
        // Made by the user without a folder: never a candidate.
        let bare = store.create_project("Bare", None).unwrap();
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
            &sub,
            &notes,
            &gone,
            &bare,
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
        assert!(store.get_project(&home).unwrap().unwrap().chats);
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
            &sub,
            &gone,
            &bare,
            &notes,
        ] {
            assert!(!store.get_project(&kept.id).unwrap().unwrap().deleted);
        }
        // The plain folder is offered, and moves only when the user says so.
        let offered: Vec<String> = store
            .chat_candidates("m", &launch_dirs)
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(offered, std::slice::from_ref(&notes.id));

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

        // Only an offered project moves: one someone used or renamed, or
        // with another machine's sessions, is refused.
        for touched in [&renamed, &foreign_session, &sub] {
            assert!(
                matches!(
                    store.move_project_to_chats(&touched.id, "m", "box", &launch_dirs),
                    Err(StoreError::Conflict(_))
                ),
                "{}",
                touched.name
            );
        }
        store
            .move_project_to_chats(&notes.id, "m", "box", &launch_dirs)
            .unwrap();
        let n = store.get_project(&notes.id).unwrap().unwrap();
        assert!(n.deleted);
        assert_eq!(n.merged_into.as_deref(), Some(home.as_str()));
        assert!(store.chat_candidates("m", &launch_dirs).unwrap().is_empty());
        assert!(matches!(
            store.move_project_to_chats(&home, "m", "box", &launch_dirs),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            store.delete_project(&home),
            Err(StoreError::Invalid(_))
        ));
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
        assert!(a.is_home && a.project.chats);
        assert_eq!(a.project.name, "Chats (box)");
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

    /// Changes `from` queued since `after`, applied on `to` as replication
    /// does.
    fn replicate(from: &Store, after: i64, to: &Store) -> i64 {
        let mut last = after;
        for e in from.outbox_after(after, 1000).unwrap() {
            let c: Change = serde_json::from_value(e.payload.clone()).unwrap();
            to.apply_remote(&c).unwrap();
            last = e.origin_seq;
        }
        last
    }

    #[test]
    fn projects_without_folders_replicate_and_keep_their_memory() {
        let (_d, store) = temp_store();
        let (_d2, other) = temp_store();
        store.set_replication(true).unwrap();
        assert!(matches!(
            store.create_project("  ", None),
            Err(StoreError::Invalid(_))
        ));
        let p = store
            .create_project(" Design ", Some("Screens for the app."))
            .unwrap();
        assert_eq!(p.name, "Design");
        assert!(!p.chats);
        // Made by the user: never retired as a scratch project.
        assert_eq!(p.updated_at, p.created_at + 1);
        let s = store.project_summary(&p.id, "m").unwrap();
        assert!(s.paths.is_empty() && !s.is_git && !s.is_home);
        assert_eq!(
            store.get_brief(&p.id).unwrap().unwrap().body_md,
            "Screens for the app."
        );

        replicate(&store, 0, &other);
        let there = other.project_summary(&p.id, "m2").unwrap();
        assert_eq!(there.project, p);
        assert!(there.paths.is_empty());
        assert_eq!(
            other.get_brief(&p.id).unwrap().unwrap().body_md,
            "Screens for the app."
        );
    }

    #[test]
    fn removing_the_last_folder_keeps_the_project() {
        let (dir, store) = temp_store();
        let a = dir.path().join("a");
        std::fs::create_dir(&a).unwrap();
        let p = store.register_project("m", &a, None).unwrap();
        store.put_brief(&p.id, "kept", "user").unwrap();
        let path = store.project_paths(&p.id).unwrap().remove(0).path;
        assert!(matches!(
            store.remove_project_folder(&p.id, "other-machine", &path),
            Err(StoreError::NotFound(_))
        ));
        store.remove_project_folder(&p.id, "m", &path).unwrap();
        let left = store.project_summary(&p.id, "m").unwrap();
        assert!(left.paths.is_empty() && !left.project.deleted);
        assert_eq!(store.get_brief(&p.id).unwrap().unwrap().body_md, "kept");
        // The folder is no longer the project's: it resolves anew.
        let r = store
            .resolve_project_with("m", "box", &a, &NonProjectDirs::default())
            .unwrap();
        assert_ne!(r.project.id, p.id);
    }

    fn auto_dirs(root: &Path) -> NonProjectDirs {
        NonProjectDirs::auto(Some(root.join("home")), vec![root.join("tmp")])
    }

    #[test]
    fn workspaces_belong_to_their_project_before_every_other_rule() {
        let (_d, store, root, dirs) = auto_env();
        // BLIRP_HOME inside a hidden home folder, as by default.
        let workspaces = root.join("home/.blirp/workspaces");
        let dirs = dirs.with_workspaces(&workspaces);
        let p = store.create_project("Design", None).unwrap();
        let ws = workspaces.join(&p.id);
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        let launch_ws = launch(&root.join("home")).with_workspaces(&workspaces);
        for (cwd, d) in [
            (ws.clone(), &dirs),
            (ws.join("sub"), &dirs),
            (ws.clone(), &launch_ws),
        ] {
            let r = store.resolve_project_with("m", "box", &cwd, d).unwrap();
            assert_eq!(r.project.id, p.id, "{}", cwd.display());
            assert!(!r.is_home && !r.created);
            assert_eq!(r.root, ws);
        }
        // Also once the folder is gone, in another spelling.
        let gone = ws.join("gone");
        let spelled = if cfg!(windows) {
            PathBuf::from(gone.display().to_string().to_uppercase())
        } else {
            gone.clone()
        };
        let r = store
            .resolve_project_lenient("m", "box", &spelled, None, &dirs)
            .unwrap();
        assert_eq!(r.project.id, p.id);
        // Read-only lookup (MCP, `blirp mem`, hooks).
        let found = store
            .find_project_for_path("m", &ws.join("sub"), Some(&workspaces))
            .unwrap();
        assert_eq!(found.map(|f| f.id), Some(p.id.clone()));
        // Without the mapping it would be a hidden home folder: Chats.
        let r = store
            .resolve_project_with("m", "box", &ws, &auto_dirs(&root))
            .unwrap();
        assert!(r.is_home);
        // No path rows: the cleanup never sees these projects.
        assert!(store.project_paths(&p.id).unwrap().is_empty());
        store.insert_session(&external("w1", &p.id, "m")).unwrap();
        assert!(
            store
                .retire_non_projects("m", "box", &dirs)
                .unwrap()
                .is_empty()
        );
        // A removed project's workspace is Chats.
        store.delete_project(&p.id).unwrap();
        let r = store.resolve_project_with("m", "box", &ws, &dirs).unwrap();
        assert!(r.is_home && r.project.chats);
    }

    #[test]
    fn a_folder_added_for_a_session_joins_the_project() {
        let (dir, store) = temp_store();
        let p = store.create_project("Game", None).unwrap();
        let place = dir.path().join("place");
        std::fs::create_dir_all(place.join("sub")).unwrap();
        let root = store
            .add_project_folder(&p.id, "m", &place.join("sub"))
            .unwrap();
        assert_eq!(root, dunce::canonicalize(place.join("sub")).unwrap());
        // Inside a folder it already has: nothing new.
        assert_eq!(
            store
                .add_project_folder(&p.id, "m", &place.join("sub"))
                .unwrap(),
            root
        );
        assert_eq!(store.project_paths(&p.id).unwrap().len(), 1);
        // Another project's folder is refused.
        let q = store.create_project("Other", None).unwrap();
        assert!(matches!(
            store.add_project_folder(&q.id, "m", &place.join("sub")),
            Err(StoreError::Conflict(_))
        ));
        // Chats have no folders.
        let chats = store
            .resolve_project_with("m", "box", dir.path(), &launch(dir.path()))
            .unwrap()
            .project;
        assert!(matches!(
            store.add_project_folder(&chats.id, "m", &place),
            Err(StoreError::Invalid(_))
        ));
    }

    #[test]
    fn sessions_move_between_chats_and_projects_with_what_they_made() {
        let (dir, store) = temp_store();
        let chats = store
            .resolve_project_with("m", "box", dir.path(), &launch(dir.path()))
            .unwrap()
            .project;
        assert_eq!(chats.id, "chats-m");
        let p = store.create_project("Real", None).unwrap();
        store
            .insert_session(&external("s", &chats.id, "m"))
            .unwrap();
        let mut child = external("c", &chats.id, "m");
        child.parent_session_id = Some("s".into());
        store.insert_session(&child).unwrap();
        let made = |id: &str, by: &str, pinned: bool| {
            let mut r = record(id, &chats.id, by, pinned);
            r.source_session_id = Some("s".into());
            store.create_record(r).unwrap();
        };
        made("r", BY_DISTILLER, false);
        made("mine", "user", false);
        made("pin", BY_DISTILLER, true);
        store
            .create_record(record("other", &chats.id, BY_DISTILLER, false))
            .unwrap();

        let moved = store.move_session("s", Some(&p.id), "m", "box").unwrap();
        assert_eq!(moved.project_id, p.id);
        assert_eq!(store.get_session("c").unwrap().unwrap().project_id, p.id);
        let ids = |pid: &str| {
            let mut v: Vec<String> = store
                .list_records(pid, &Default::default())
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect();
            v.sort();
            v
        };
        assert_eq!(ids(&p.id), ["mine", "pin", "r"]);
        assert!(matches!(
            store.move_session("s", Some("nope"), "m", "box"),
            Err(StoreError::NotFound(_))
        ));
        // A write built from the row before the move (a status tick) keeps
        // the project the move set.
        let mut stale = external("s", &chats.id, "m");
        stale.title = Some("late status".into());
        store.apply(Change::Session(stale)).unwrap();
        let now = store.get_session("s").unwrap().unwrap();
        assert_eq!(now.project_id, p.id);
        assert_eq!(now.title.as_deref(), Some("late status"));
        // Back to Chats: only the unpinned distiller records go along.
        let back = store.move_session("s", None, "m", "box").unwrap();
        assert_eq!(back.project_id, chats.id);
        assert_eq!(
            store.get_session("c").unwrap().unwrap().project_id,
            chats.id
        );
        assert_eq!(ids(&p.id), ["mine", "pin"]);

        // Another machine's session goes to that machine's Chats, which only
        // that machine creates.
        store
            .insert_session(&external("f", &p.id, "other"))
            .unwrap();
        assert!(matches!(
            store.move_session("f", None, "m", "box"),
            Err(StoreError::Conflict(_))
        ));
        let theirs = Project {
            id: "chats-other".into(),
            name: "Chats (laptop)".into(),
            created_at: 1,
            updated_at: 1,
            deleted: false,
            chats: true,
            merged_into: None,
        };
        store.apply_remote(&Change::Project(theirs)).unwrap();
        // Also when a Chats project is named explicitly.
        let f = store
            .move_session("f", Some(&chats.id), "m", "box")
            .unwrap();
        assert_eq!(f.project_id, "chats-other");
    }

    fn old_home(store: &Store, name: &str) -> Project {
        store
            .write(|tx| {
                let p = new_project(tx, name, false)?;
                tx.execute(
                    "INSERT INTO settings(key, value_json) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
                    params![HOME_PROJECT_KEY, serde_json::to_string(&p.id)?],
                )?;
                Ok(p)
            })
            .unwrap()
    }

    #[test]
    fn the_old_home_project_gives_way_to_chats_everywhere() {
        // Untouched: merged into the new bucket, on every machine.
        let (dir, store) = temp_store();
        let (_d2, other) = temp_store();
        store.set_replication(true).unwrap();
        let home = old_home(&store, "Home (box)");
        store.insert_session(&external("h", &home.id, "m")).unwrap();
        store
            .create_record(record("d", &home.id, BY_DISTILLER, false))
            .unwrap();
        let seq = replicate(&store, 0, &other);
        assert!(store.ensure_chats("m", "box").unwrap());
        assert!(!store.ensure_chats("m", "box").unwrap());
        let chats = store.get_project("chats-m").unwrap().unwrap();
        assert!(chats.chats && chats.name == "Chats (box)");
        assert_eq!(store.home_project_id().unwrap().as_deref(), Some("chats-m"));
        assert_eq!(
            store.get_session("h").unwrap().unwrap().project_id,
            "chats-m"
        );
        assert_eq!(
            store.get_record("d").unwrap().unwrap().project_id,
            "chats-m"
        );
        let gone = store.get_project(&home.id).unwrap().unwrap();
        assert!(gone.deleted);
        assert_eq!(gone.merged_into.as_deref(), Some("chats-m"));
        replicate(&store, seq, &other);
        assert_eq!(other.get_project("chats-m").unwrap().unwrap(), chats);
        assert!(other.get_project(&home.id).unwrap().unwrap().deleted);
        let r = store
            .resolve_project_with("m", "box", dir.path(), &launch(dir.path()))
            .unwrap();
        assert_eq!(r.project.id, "chats-m");
    }

    #[test]
    fn an_old_home_project_with_user_memory_stays_a_project() {
        for touch in ["renamed", "brief", "record", "pinned", "wiki", "foreign"] {
            let (_d, store) = temp_store();
            let name = if touch == "renamed" {
                "My home"
            } else {
                "Home (box)"
            };
            let home = old_home(&store, name);
            match touch {
                "brief" => {
                    store.put_brief(&home.id, "mine", "user").unwrap();
                }
                "record" => {
                    store
                        .create_record(record("u", &home.id, "user", false))
                        .unwrap();
                }
                "pinned" => {
                    store
                        .create_record(record("u", &home.id, BY_DISTILLER, true))
                        .unwrap();
                }
                "wiki" => {
                    store
                        .create_wiki_page(&home.id, "notes", "Notes", "x", "user")
                        .unwrap();
                }
                "foreign" => {
                    store
                        .insert_session(&external("f", &home.id, "other"))
                        .unwrap();
                }
                _ => {}
            }
            assert!(store.ensure_chats("m", "box").unwrap(), "{touch}");
            let kept = store.get_project(&home.id).unwrap().unwrap();
            assert!(!kept.deleted && !kept.chats, "{touch}");
            assert!(store.get_project("chats-m").unwrap().unwrap().chats);
            assert_eq!(store.home_project_id().unwrap().as_deref(), Some("chats-m"));
        }
    }

    // A bucket under another id (an earlier build, or this machine's id
    // before it was rebound) joins the canonical one at start.
    #[test]
    fn a_chats_bucket_under_another_id_joins_the_canonical_one() {
        let (_d, store) = temp_store();
        let old = store.write(|tx| home_project(tx, "old-id", "box")).unwrap();
        assert_eq!(old.id, "chats-old-id");
        store.insert_session(&external("c", &old.id, "m")).unwrap();
        assert!(store.ensure_chats("m", "box").unwrap());
        assert_eq!(store.home_project_id().unwrap().as_deref(), Some("chats-m"));
        assert_eq!(
            store.get_session("c").unwrap().unwrap().project_id,
            "chats-m"
        );
        let gone = store.get_project(&old.id).unwrap().unwrap();
        assert!(gone.deleted && gone.chats);
        assert!(!store.ensure_chats("m", "box").unwrap());

        // A copied database: the stored bucket is another known machine's,
        // or holds its sessions. It stays that machine's.
        for case in ["known machine", "foreign session"] {
            let (_d, store) = temp_store();
            let theirs = store
                .write(|tx| home_project(tx, "laptop", "laptop"))
                .unwrap();
            if case == "known machine" {
                store
                    .upsert_machine(&crate::model::Machine {
                        id: "laptop".into(),
                        name: "laptop".into(),
                        os: "linux".into(),
                        role: crate::model::MachineRole::Node,
                        last_seen: 1,
                        revoked: false,
                    })
                    .unwrap();
            } else {
                store
                    .insert_session(&external("x", &theirs.id, "someone"))
                    .unwrap();
            }
            assert!(store.ensure_chats("m", "box").unwrap(), "{case}");
            assert!(
                !store.get_project(&theirs.id).unwrap().unwrap().deleted,
                "{case}"
            );
            assert_eq!(store.home_project_id().unwrap().as_deref(), Some("chats-m"));
        }
    }

    #[test]
    fn the_chats_flag_is_sticky() {
        let (_d, store) = temp_store();
        let chats = store.write(|tx| home_project(tx, "m", "box")).unwrap();
        // A copy without the flag (an older machine's rename) that is newer
        // by time still cannot clear it.
        let stale = Project {
            name: "Renamed".into(),
            updated_at: chats.updated_at + 10,
            chats: false,
            ..chats.clone()
        };
        store.apply_remote(&Change::Project(stale)).unwrap();
        let now = store.get_project(&chats.id).unwrap().unwrap();
        assert!(now.chats);
        assert_eq!(now.name, "Renamed");
    }

    #[test]
    fn a_merged_projects_workspace_follows_it() {
        let (_d, store, root, dirs) = auto_env();
        let workspaces = root.join("home/.blirp/workspaces");
        let dirs = dirs.with_workspaces(&workspaces);
        let a = store.create_project("A", None).unwrap();
        let b = store.create_project("B", None).unwrap();
        let ws = workspaces.join(&a.id);
        std::fs::create_dir_all(&ws).unwrap();
        store.merge_projects(&a.id, &b.id).unwrap();
        let r = store.resolve_project_with("m", "box", &ws, &dirs).unwrap();
        assert_eq!(r.project.id, b.id);
        assert!(!r.is_home);
        let found = store
            .find_project_for_path("m", &ws, Some(&workspaces))
            .unwrap();
        assert_eq!(found.map(|p| p.id), Some(b.id.clone()));
        // Chats is no merge target or source.
        let chats = store.write(|tx| home_project(tx, "m", "box")).unwrap();
        assert!(matches!(
            store.merge_projects(&b.id, &chats.id),
            Err(StoreError::Invalid(_))
        ));
    }
}
