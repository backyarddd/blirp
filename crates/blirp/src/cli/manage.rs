//! `blirp projects ...` and `blirp sessions <action>`: the project and
//! session actions of the UI, through the daemon's API (§4).

use super::{Client, confirm};
use anyhow::Context as _;
use blirp_core::model::{ProjectSummary, Session, SessionDetail};
use clap::Subcommand;
use reqwest::Method;
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum ProjectsCommand {
    /// List projects (most recently active first).
    List {
        /// Print JSON (the API's project summaries).
        #[arg(long)]
        json: bool,
    },
    /// List the projects in the Trash (most recently deleted first).
    Trash {
        #[arg(long)]
        json: bool,
    },
    /// Rename a project.
    Rename {
        id: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Move a project to the Trash: hidden with its sessions, its folders
    /// unregistered on every synced machine. Files on disk are not touched.
    Delete {
        id: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Bring a project back from the Trash with its sessions and the folders
    /// it had on this machine.
    Restore {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Move everything of a project into another one, then delete it. Cannot
    /// be undone.
    Merge {
        id: String,
        /// The project that receives it.
        #[arg(long)]
        into: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum SessionsAction {
    /// Set a session's title; an empty title goes back to the default one.
    Rename {
        id: String,
        title: String,
        #[arg(long)]
        json: bool,
    },
    /// Move a session (with its subagent sessions and the records it
    /// produced) into another project, or into Chats.
    Move {
        id: String,
        /// Target project id.
        #[arg(long, required_unless_present = "chats", conflicts_with = "chats")]
        project: Option<String>,
        /// Move it into Chats (no project).
        #[arg(long)]
        chats: bool,
        #[arg(long)]
        json: bool,
    },
    /// Delete an ended session for good on every synced machine. The agent's
    /// own transcript file is not touched.
    Delete {
        id: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Stop a running session: its agent process and everything it started
    /// end. It can be resumed later.
    Stop {
        id: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
}

/// `/api/<segments...>`, each segment escaped as one path segment (an id
/// never turns into a path of its own).
fn api_path(segments: &[&str]) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse("http://blirp.local/")?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("building the API path"))?
        .push("api")
        .extend(segments);
    Ok(url.path().to_string())
}

fn print_json(v: &impl serde::Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

async fn call<T: serde::de::DeserializeOwned>(
    client: &Client,
    method: Method,
    segments: &[&str],
    body: Option<serde_json::Value>,
) -> anyhow::Result<T> {
    let resp = client.send(method, &api_path(segments)?, body).await?;
    Ok(resp.json().await?)
}

async fn call_empty(
    client: &Client,
    method: Method,
    segments: &[&str],
    body: Option<serde_json::Value>,
) -> anyhow::Result<()> {
    client.send(method, &api_path(segments)?, body).await?;
    Ok(())
}

/// Asked unless `--yes`; declined: nothing changes and the exit code is 1
/// (like `blirp uninstall`).
fn declined(yes: bool, question: &str) -> anyhow::Result<bool> {
    if yes || confirm(question)? {
        return Ok(false);
    }
    println!("Nothing was changed.");
    Ok(true)
}

fn project_table(list: &[ProjectSummary], trash: bool) {
    println!(
        "{:<36}  {:>8}  {:<10}  NAME / FOLDER",
        "ID",
        "SESSIONS",
        if trash { "DELETED" } else { "ACTIVE" }
    );
    for p in list {
        let when = if trash {
            Some(p.project.updated_at)
        } else {
            p.last_activity_at
        };
        let when = when
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map_or_else(|| "-".to_string(), |t| t.format("%Y-%m-%d").to_string());
        let folder = p
            .paths
            .iter()
            .find(|x| x.local)
            .map(|x| x.path.clone())
            .or_else(|| p.workspace.clone());
        let name = if p.project.chats {
            "Chats".to_string()
        } else {
            p.project.name.clone()
        };
        match folder {
            Some(f) => println!(
                "{:<36}  {:>8}  {when:<10}  {name}  ({f})",
                p.project.id, p.session_count
            ),
            None => println!(
                "{:<36}  {:>8}  {when:<10}  {name}",
                p.project.id, p.session_count
            ),
        }
    }
}

pub async fn projects(client: &Client, cmd: ProjectsCommand) -> anyhow::Result<ExitCode> {
    match cmd {
        ProjectsCommand::List { json } => {
            let list: Vec<ProjectSummary> = call(client, Method::GET, &["projects"], None).await?;
            if json {
                print_json(&list)?;
            } else if list.is_empty() {
                println!("no projects");
            } else {
                project_table(&list, false);
            }
        }
        ProjectsCommand::Trash { json } => {
            let path = format!("{}?deleted=true", api_path(&["projects"])?);
            let list: Vec<ProjectSummary> =
                client.send(Method::GET, &path, None).await?.json().await?;
            if json {
                print_json(&list)?;
            } else if list.is_empty() {
                println!("the Trash is empty");
            } else {
                project_table(&list, true);
            }
        }
        ProjectsCommand::Rename { id, name, json } => {
            let p: ProjectSummary = call(
                client,
                Method::PATCH,
                &["projects", &id],
                Some(serde_json::json!({ "name": name })),
            )
            .await?;
            if json {
                print_json(&p)?;
            } else {
                println!("Renamed {} to \"{}\"", p.project.id, p.project.name);
            }
        }
        ProjectsCommand::Delete { id, yes } => {
            let p = project(client, &id).await?;
            if declined(
                yes,
                &format!(
                    "Move project \"{}\" to the Trash? It is hidden with its sessions and its folders are unregistered on every synced machine. Files on disk are not touched; `blirp projects restore {}` brings it back.",
                    p.project.name, p.project.id
                ),
            )? {
                return Ok(ExitCode::FAILURE);
            }
            call_empty(client, Method::DELETE, &["projects", &id], None).await?;
            println!("Moved \"{}\" to the Trash", p.project.name);
        }
        ProjectsCommand::Restore { id, json } => {
            let p: ProjectSummary =
                call(client, Method::POST, &["projects", &id, "restore"], None).await?;
            if json {
                print_json(&p)?;
            } else {
                println!("Restored \"{}\"", p.project.name);
                for f in p.paths.iter().filter(|f| f.local) {
                    println!("  folder {}", f.path);
                }
            }
        }
        ProjectsCommand::Merge {
            id,
            into,
            yes,
            json,
        } => {
            let (from, to) = (project(client, &id).await?, project(client, &into).await?);
            if declined(
                yes,
                &format!(
                    "Merge \"{}\" into \"{}\"? Its folders, sessions, records, wiki pages and resources move there and \"{}\" is deleted. This cannot be undone.",
                    from.project.name, to.project.name, from.project.name
                ),
            )? {
                return Ok(ExitCode::FAILURE);
            }
            let p: ProjectSummary = call(
                client,
                Method::POST,
                &["projects", &id, "merge"],
                Some(serde_json::json!({ "into": into })),
            )
            .await?;
            if json {
                print_json(&p)?;
            } else {
                println!(
                    "Merged \"{}\" into \"{}\"",
                    from.project.name, p.project.name
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn project(client: &Client, id: &str) -> anyhow::Result<ProjectSummary> {
    call(client, Method::GET, &["projects", id], None).await
}

async fn session(client: &Client, id: &str) -> anyhow::Result<Session> {
    let d: SessionDetail = call(client, Method::GET, &["sessions", id], None).await?;
    Ok(d.session)
}

fn session_label(s: &Session) -> String {
    s.title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map_or_else(|| format!("{} in {}", s.agent, s.cwd), str::to_string)
}

pub async fn sessions(client: &Client, action: SessionsAction) -> anyhow::Result<ExitCode> {
    match action {
        SessionsAction::Rename { id, title, json } => {
            let title = title.trim();
            let body = serde_json::json!({ "title": (!title.is_empty()).then_some(title) });
            let s: Session = call(client, Method::PATCH, &["sessions", &id], Some(body)).await?;
            if json {
                print_json(&s)?;
            } else {
                println!("Renamed {} to \"{}\"", s.id, session_label(&s));
            }
        }
        SessionsAction::Move {
            id,
            project,
            chats: _,
            json,
        } => {
            // `--chats` is the only other way clap lets through.
            let target = match &project {
                Some(p) => Some(self::project(client, p).await?),
                None => None,
            };
            let body = serde_json::json!({ "project_id": project });
            let s: Session =
                call(client, Method::POST, &["sessions", &id, "move"], Some(body)).await?;
            if json {
                print_json(&s)?;
            } else {
                let to = target.map_or_else(
                    || "Chats".to_string(),
                    |p| format!("\"{}\"", p.project.name),
                );
                println!("Moved {} to {to}", s.id);
            }
        }
        SessionsAction::Delete { id, yes } => {
            let s = session(client, &id).await?;
            if declined(
                yes,
                &format!(
                    "Delete session \"{}\" for good on every synced machine, with its subagent \
                     sessions? The agent's own transcript file is not touched.",
                    session_label(&s)
                ),
            )? {
                return Ok(ExitCode::FAILURE);
            }
            call_empty(client, Method::DELETE, &["sessions", &id], None).await?;
            println!("Deleted {id}");
        }
        SessionsAction::Stop { id, yes } => {
            let s = session(client, &id).await?;
            if declined(
                yes,
                &format!(
                    "Stop session \"{}\"? Its agent process and everything it started end; it \
                     can be resumed later.",
                    session_label(&s)
                ),
            )? {
                return Ok(ExitCode::FAILURE);
            }
            call_empty(
                client,
                Method::POST,
                &["sessions", &id, "stop"],
                Some(serde_json::json!({})),
            )
            .await
            .with_context(|| format!("stopping session {id}"))?;
            println!("Stopping {id}");
        }
    }
    Ok(ExitCode::SUCCESS)
}
