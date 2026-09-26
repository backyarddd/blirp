//! `blirp worktrees list|prune`: the git worktrees blirp created for
//! sessions (§7) under `BLIRP_HOME/worktrees`.

use super::Client;
use anyhow::Context as _;
use blirp_core::git;
use blirp_core::model::Session;
use blirp_core::paths::Paths;
use blirp_core::store::Store;
use clap::Subcommand;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum WorktreesCommand {
    /// List blirp worktrees with their session and uncommitted changes.
    List,
    /// Remove the worktrees of ended sessions that have no uncommitted
    /// changes (branches are kept). Needs the daemon.
    Prune,
}

/// State of one session worktree on disk.
enum Disk {
    Missing,
    /// Number of uncommitted changes (untracked files included).
    Changes(usize),
    Unknown(String),
}

fn inspect(path: &Path) -> Disk {
    if !path.is_dir() {
        return Disk::Missing;
    }
    match git::status(path) {
        Ok(st) => Disk::Changes(st.entries.len()),
        Err(e) => Disk::Unknown(e.to_string()),
    }
}

fn sessions(paths: &Paths) -> anyhow::Result<Vec<Session>> {
    let store = Store::open_read_only(&paths.db_file())
        .with_context(|| format!("open {}", paths.db_file().display()))?;
    let Some(machine) = store.machine_id()? else {
        return Ok(Vec::new());
    };
    Ok(store.sessions_with_worktree(&machine)?)
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

pub fn list(paths: &Paths) -> anyhow::Result<ExitCode> {
    let sessions = sessions(paths)?;
    let known: Vec<PathBuf> = sessions
        .iter()
        .filter_map(|s| s.worktree.as_deref())
        .map(|w| dunce::canonicalize(w).unwrap_or_else(|_| PathBuf::from(w)))
        .collect();
    let orphans = orphans(paths, &known);
    if sessions.is_empty() && orphans.is_empty() {
        println!("no blirp worktrees");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<36}  {:<10}  {:<12}  PATH",
        "SESSION", "STATUS", "CHANGES"
    );
    for s in &sessions {
        let path = s.worktree.as_deref().unwrap_or_default();
        let changes = match inspect(Path::new(path)) {
            Disk::Missing => "missing".to_string(),
            Disk::Changes(0) => "clean".to_string(),
            Disk::Changes(n) => format!("{n} changed"),
            Disk::Unknown(e) => format!("unknown ({e})"),
        };
        println!("{:<36}  {:<10}  {:<12}  {path}", s.id, s.status, changes);
    }
    for o in orphans {
        println!(
            "{:<36}  {:<10}  {:<12}  {}",
            "(no session)",
            "-",
            "-",
            o.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn prune(paths: &Paths) -> anyhow::Result<ExitCode> {
    let client = Client::connect(paths).await?;
    let sessions = tokio::task::spawn_blocking({
        let paths = paths.clone();
        move || sessions(&paths)
    })
    .await??;
    let (mut removed, mut kept) = (0, 0);
    for s in sessions {
        let path = PathBuf::from(s.worktree.clone().unwrap_or_default());
        if s.status.is_live() {
            println!("keep    {} (session {} is running)", path.display(), s.id);
            kept += 1;
            continue;
        }
        let disk = tokio::task::spawn_blocking({
            let path = path.clone();
            move || inspect(&path)
        })
        .await?;
        match disk {
            Disk::Changes(n) if n > 0 => {
                println!("keep    {} ({n} uncommitted change(s))", path.display());
                kept += 1;
                continue;
            }
            Disk::Unknown(e) => {
                println!("keep    {} (status unknown: {e})", path.display());
                kept += 1;
                continue;
            }
            Disk::Missing | Disk::Changes(_) => {}
        }
        let route = format!("/api/sessions/{}/worktree/remove", s.id);
        match client
            .send(reqwest::Method::POST, &route, Some(serde_json::json!({})))
            .await
        {
            Ok(_) => {
                println!("removed {}", path.display());
                removed += 1;
            }
            Err(e) => {
                println!("keep    {} ({e:#})", path.display());
                kept += 1;
            }
        }
    }
    println!("{removed} removed, {kept} kept");
    Ok(ExitCode::SUCCESS)
}
