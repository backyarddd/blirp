//! `blirp worktrees list|prune`: the git worktrees blirp created for
//! sessions (§7) under `BLIRP_HOME/worktrees`.

use super::Client;
use anyhow::Context as _;
use blirp_core::model::{PrunedWorktree, WorktreeState};
use blirp_core::paths::Paths;
use blirp_core::store::Store;
use clap::Subcommand;
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum WorktreesCommand {
    /// List blirp worktrees with their session and uncommitted changes.
    List,
    /// Remove the worktrees of ended sessions that have no uncommitted
    /// changes (branches are kept). Needs the daemon.
    Prune,
}

pub fn list(paths: &Paths) -> anyhow::Result<ExitCode> {
    let store = Store::open_read_only(&paths.db_file())
        .with_context(|| format!("open {}", paths.db_file().display()))?;
    let list = crate::worktrees::list(&store, paths)?;
    if list.is_empty() {
        println!("no blirp worktrees");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<36}  {:<10}  {:<12}  PATH",
        "SESSION", "STATUS", "CHANGES"
    );
    for w in list {
        let changes = match w.state {
            WorktreeState::Missing => "missing".to_string(),
            WorktreeState::Clean => "clean".to_string(),
            WorktreeState::Changed => format!("{} changed", w.changes),
            WorktreeState::Unknown => format!("unknown ({})", w.error.unwrap_or_default()),
        };
        match &w.session {
            Some(s) => println!("{:<36}  {:<10}  {changes:<12}  {}", s.id, s.status, w.path),
            None => println!(
                "{:<36}  {:<10}  {:<12}  {}",
                "(no session)", "-", "-", w.path
            ),
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn prune(paths: &Paths) -> anyhow::Result<ExitCode> {
    let client = Client::connect(paths).await?;
    let out: Vec<PrunedWorktree> = client
        .send(
            reqwest::Method::POST,
            "/api/worktrees/prune",
            Some(serde_json::json!({})),
        )
        .await?
        .json()
        .await?;
    let removed = out.iter().filter(|w| w.removed).count();
    for w in &out {
        match &w.reason {
            None if w.removed => println!("removed {}", w.path),
            reason => println!(
                "keep    {} ({})",
                w.path,
                reason.as_deref().unwrap_or("kept")
            ),
        }
    }
    println!("{removed} removed, {} kept", out.len() - removed);
    Ok(ExitCode::SUCCESS)
}
