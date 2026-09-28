//! The git worktrees blirp made for sessions (§7) under
//! `BLIRP_HOME/worktrees`: listed by `blirp worktrees list` (reading the
//! database, no daemon needed) and `GET /api/worktrees`, pruned by
//! `POST /api/worktrees/prune`.

use crate::api::{ApiError, ApiResult};
use crate::state::SharedState;
use blirp_core::git;
use blirp_core::model::{PrunedWorktree, Session, WorktreeInfo, WorktreeState};
use blirp_core::paths::Paths;
use blirp_core::store::Store;
use std::path::{Path, PathBuf};

/// The state of one worktree folder on disk.
fn inspect(path: &Path) -> (WorktreeState, i64, Option<String>) {
    if !path.is_dir() {
        return (WorktreeState::Missing, 0, None);
    }
    match git::status(path) {
        Ok(st) if st.entries.is_empty() => (WorktreeState::Clean, 0, None),
        Ok(st) => (
            WorktreeState::Changed,
            i64::try_from(st.entries.len()).unwrap_or(i64::MAX),
            None,
        ),
        Err(e) => (WorktreeState::Unknown, 0, Some(e.to_string())),
    }
}

/// Sessions of this machine with a worktree (none before the daemon's
/// first start recorded the machine id).
fn sessions(store: &Store) -> blirp_core::store::Result<Vec<Session>> {
    match store.machine_id()? {
        Some(machine) => store.sessions_with_worktree(&machine),
        None => Ok(Vec::new()),
    }
}

/// Worktree folders (`worktrees/<project>/<name>`) no session refers to.
fn orphans(paths: &Paths, known: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(paths.worktrees_dir()) else {
        return out;
    };
    for project in projects.filter_map(Result::ok) {
        let Ok(names) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for wt in names.filter_map(Result::ok).map(|e| e.path()) {
            let canonical = dunce::canonicalize(&wt).unwrap_or_else(|_| wt.clone());
            if wt.is_dir() && !known.contains(&canonical) {
                out.push(wt);
            }
        }
    }
    out.sort();
    out
}

/// Every session worktree of this machine (most recent session first),
/// then the folders no session refers to.
pub fn list(store: &Store, paths: &Paths) -> blirp_core::store::Result<Vec<WorktreeInfo>> {
    let sessions = sessions(store)?;
    let known: Vec<PathBuf> = sessions
        .iter()
        .filter_map(|s| s.worktree.as_deref())
        .map(|w| dunce::canonicalize(w).unwrap_or_else(|_| PathBuf::from(w)))
        .collect();
    let orphans = orphans(paths, &known);
    let mut out: Vec<WorktreeInfo> = sessions
        .into_iter()
        .map(|s| {
            let path = s.worktree.clone().unwrap_or_default();
            let (state, changes, error) = inspect(Path::new(&path));
            WorktreeInfo {
                path,
                session: Some(s),
                state,
                changes,
                error,
            }
        })
        .collect();
    out.extend(orphans.into_iter().map(|p| {
        let (state, changes, error) = inspect(&p);
        WorktreeInfo {
            path: p.display().to_string(),
            session: None,
            state,
            changes,
            error,
        }
    }));
    Ok(out)
}

/// Remove the worktree of every ended session that has no uncommitted
/// changes (untracked files count) or whose folder is already gone;
/// branches are kept. Running sessions, changes and an unknown state keep
/// it, with the reason. Folders without a session are never removed.
/// Blocking.
pub fn prune(state: &SharedState) -> ApiResult<Vec<PrunedWorktree>> {
    let mut out = Vec::new();
    for s in sessions(&state.store)? {
        let path = s.worktree.clone().unwrap_or_default();
        let keep = |reason: String| PrunedWorktree {
            path: path.clone(),
            session_id: s.id.clone(),
            removed: false,
            reason: Some(reason),
        };
        if s.status.is_live() || state.terminals.get(&s.id).is_some() {
            out.push(keep(format!("session {} is running", s.id)));
            continue;
        }
        match inspect(Path::new(&path)) {
            (WorktreeState::Changed, n, _) => {
                out.push(keep(format!("{n} uncommitted change(s)")));
                continue;
            }
            (WorktreeState::Unknown, _, e) => {
                out.push(keep(format!("status unknown: {}", e.unwrap_or_default())));
                continue;
            }
            (WorktreeState::Clean | WorktreeState::Missing, ..) => {}
        }
        match crate::sessions::remove_worktree(state, &s.id, false) {
            Ok(_) => out.push(PrunedWorktree {
                path: path.clone(),
                session_id: s.id.clone(),
                removed: true,
                reason: None,
            }),
            Err(ApiError { message, .. }) => out.push(keep(message)),
        }
    }
    Ok(out)
}
