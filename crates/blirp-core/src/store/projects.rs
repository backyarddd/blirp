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

fn path_row(r: &Row<'_>) -> rusqlite::Result<ProjectPath> {
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

/// Longest registered path on this machine that contains `p`.
fn longest_prefix<'a>(paths: &'a [ProjectPath], p: &Path) -> Option<&'a ProjectPath> {
    paths
        .iter()
        .filter(|pp| p.starts_with(&pp.path))
        .max_by_key(|pp| pp.path.len())
}

fn live_local_paths(c: &Connection, machine_id: &str) -> Result<Vec<ProjectPath>> {
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

fn live_project_in(c: &Connection, id: &str) -> Result<Project> {
    get_project_in(c, id)?
        .filter(|p| !p.deleted)
        .ok_or(StoreError::NotFound("project"))
}

fn new_project(tx: &Transaction<'_>, name: &str) -> Result<Project> {
    let now = crate::now_ms();
    let p = Project {
        id: crate::new_id(),
        name: name.to_string(),
        created_at: now,
        updated_at: now,
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

    /// §5 project resolution for a session starting in `cwd` on this machine.
    pub fn resolve_project(
        &self,
        machine_id: &str,
        machine_name: &str,
        cwd: &Path,
    ) -> Result<ResolvedProject> {
        let home = crate::paths::user_home().and_then(|h| dunce::canonicalize(h).ok());
        self.resolve_project_with_home(machine_id, machine_name, cwd, home.as_deref())
    }

    pub(crate) fn resolve_project_with_home(
        &self,
        machine_id: &str,
        machine_name: &str,
        cwd: &Path,
        home: Option<&Path>,
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
            // 3. Home dir, its ancestors (`C:\Users`, `/home`) and filesystem
            // roots are never project roots.
            if root.parent().is_none() || home.is_some_and(|h| h.starts_with(&root)) {
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
            let project = new_project(tx, &folder_name(&root))?;
            attach_path(tx, &project.id, machine_id, &root, remote)?;
            Ok(ResolvedProject {
                project,
                root,
                is_home: false,
                created: true,
            })
        })
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
            let project = new_project(tx, &name)?;
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

    /// Soft-delete a project and unregister its folders on every machine.
    /// Sessions and memory stay in the database.
    pub fn delete_project(&self, id: &str) -> Result<()> {
        self.write(|tx| {
            let mut p = live_project_in(tx, id)?;
            let paths = all(
                tx,
                "SELECT * FROM project_paths WHERE project_id = ?1",
                params![id],
                path_row,
            )?;
            for pp in paths {
                apply_in(
                    tx,
                    &Change::DeleteProjectPath {
                        machine_id: pp.machine_id,
                        path: pp.path,
                    },
                )?;
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
            let mut src = live_project_in(tx, from)?;
            let mut dst = live_project_in(tx, into)?;
            let now = crate::now_ms();
            let mut changes = Vec::new();
            for mut pp in all(
                tx,
                "SELECT * FROM project_paths WHERE project_id = ?1",
                params![from],
                path_row,
            )? {
                pp.project_id = into.to_string();
                changes.push(Change::ProjectPath(pp));
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
            let dst_brief = super::memory::get_brief_in(tx, into)?;
            if let (None, Some(mut b)) = (dst_brief, super::memory::get_brief_in(tx, from)?) {
                b.project_id = into.to_string();
                b.version = 1;
                b.updated_at = now;
                changes.push(Change::Brief(b));
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
    let p = new_project(tx, &format!("Home ({machine_name})"))?;
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

    #[test]
    fn non_git_folder_is_a_project_and_subfolders_resolve_to_it() {
        let (dir, store) = temp_store();
        let proj = dir.path().join("plain-folder");
        std::fs::create_dir_all(proj.join("scripts/deep")).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();

        let r = store
            .resolve_project_with_home("m1", "box", &proj, Some(&home))
            .unwrap();
        assert!(r.created && !r.is_home);
        assert_eq!(r.project.name, "plain-folder");
        assert_eq!(r.root, dunce::canonicalize(&proj).unwrap());

        let sub = store
            .resolve_project_with_home("m1", "box", &proj.join("scripts/deep"), Some(&home))
            .unwrap();
        assert_eq!(sub.project.id, r.project.id);
        assert!(!sub.created);

        // Other machines do not share non-git paths.
        let other = store
            .resolve_project_with_home("m2", "b2", &proj, Some(&home))
            .unwrap();
        assert_ne!(other.project.id, r.project.id);
    }

    #[test]
    fn home_and_root_map_to_home_project() {
        let (dir, store) = temp_store();
        let home = dunce::canonicalize(dir.path()).unwrap();
        let a = store
            .resolve_project_with_home("m1", "box", &home, Some(&home))
            .unwrap();
        assert!(a.is_home);
        assert_eq!(a.project.name, "Home (box)");
        let root = home.ancestors().last().unwrap().to_path_buf();
        let b = store
            .resolve_project_with_home("m1", "box", &root, Some(&home))
            .unwrap();
        assert!(b.is_home);
        assert_eq!(a.project.id, b.project.id);
        // Any ancestor of home (`C:\Users`, `/home`) is not a project either.
        let parent = home.parent().unwrap();
        let p = store
            .resolve_project_with_home("m1", "box", parent, Some(&home))
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
            .resolve_project_with_home("m1", "box", &home.join("child"), Some(&home))
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
            .resolve_project_with_home("m1", "box", &repo.join("src"), Some(&home))
            .unwrap();
        assert!(r.created);
        assert_eq!(r.root, dunce::canonicalize(&repo).unwrap());
        let paths = store.project_paths(&r.project.id).unwrap();
        assert_eq!(paths[0].git_remote.as_deref(), Some("github.com/acme/app"));

        // A linked worktree outside the repo resolves to the same project.
        let wt = dir.path().join("wt");
        git::worktree_add(&repo, &wt, "blirp/x").unwrap();
        let w = store
            .resolve_project_with_home("m1", "box", &wt, Some(&home))
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
            .resolve_project_with_home("m2", "b2", &clone, Some(&home))
            .unwrap();
        assert_eq!(c.project.id, r.project.id);
        assert!(!c.created);
        assert_eq!(store.project_paths(&r.project.id).unwrap().len(), 2);
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
            .resolve_project_with_home("m", "box", &a, None)
            .unwrap();
        assert_eq!(r.project.id, pb.id);

        store.delete_project(&pb.id).unwrap();
        assert!(store.project_paths(&pb.id).unwrap().is_empty());
        assert!(store.list_project_summaries("m").unwrap().is_empty());
    }
}
