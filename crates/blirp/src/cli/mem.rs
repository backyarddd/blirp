//! `blirp mem search|brief|show|recent` and `blirp hooks install|uninstall|status`.
//! Memory commands read the database read-only, so they work without the daemon.

use crate::hooks::install::{self, Homes};
use crate::memory::render::{clip, date, parse_summary};
use anyhow::{Context as _, bail};
use blirp_core::model::{RecordStatus, SearchHitKind, SearchResults};
use blirp_core::paths::Paths;
use blirp_core::store::{RecordFilter, Store};
use clap::Subcommand;
use serde_json::json;
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum MemCommand {
    /// Full-text search over records and transcripts.
    Search {
        /// Words to search for.
        #[arg(required = true)]
        query: Vec<String>,
        /// Project id (default: the project of the current folder).
        #[arg(long)]
        project: Option<String>,
        /// Search every project.
        #[arg(long, conflicts_with = "project")]
        all: bool,
        /// Only `record` or `event` hits.
        #[arg(long)]
        kind: Option<SearchHitKind>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Show the project brief and active records.
    Brief {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show one session: summary and transcript events.
    Show {
        session_id: String,
        #[arg(long, default_value_t = 200)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// List recent sessions of the project.
    Recent {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum HooksCommand {
    /// Add blirp hooks and the blirp MCP server to agents' user configs.
    Install {
        /// Only this agent (default: every supported agent found on PATH).
        #[arg(long)]
        agent: Option<String>,
    },
    /// Remove exactly the entries `install` added.
    Uninstall {
        #[arg(long)]
        agent: Option<String>,
    },
    /// Show the global integration state per agent.
    Status {
        #[arg(long)]
        agent: Option<String>,
    },
}

fn open(paths: &Paths) -> anyhow::Result<Store> {
    Store::open_read_only(&paths.db_file())
        .with_context(|| format!("open {}", paths.db_file().display()))
}

fn project_of(store: &Store, paths: &Paths, arg: Option<String>) -> anyhow::Result<String> {
    if let Some(p) = arg {
        return Ok(p);
    }
    let cwd = std::env::current_dir()?;
    let machine = store
        .machine_id()?
        .context("no machine id yet; start the blirp daemon once")?;
    match store.find_project_for_path(&machine, &cwd, Some(&paths.workspaces_dir()))? {
        Some(p) => Ok(p.id),
        None => bail!(
            "{} is not inside a blirp project; pass --project <id>",
            cwd.display()
        ),
    }
}

fn print_json(v: &impl serde::Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn run_mem(paths: &Paths, cmd: MemCommand) -> anyhow::Result<ExitCode> {
    let store = open(paths)?;
    match cmd {
        MemCommand::Search {
            query,
            project,
            all,
            kind,
            limit,
            json,
        } => {
            let project = if all {
                None
            } else {
                Some(project_of(&store, paths, project)?)
            };
            let q = query.join(" ");
            let hits =
                store.search(&q, project.as_deref(), kind, i64::from(limit.clamp(1, 200)))?;
            if json {
                print_json(&SearchResults { hits })?;
            } else if hits.is_empty() {
                println!("no results for {q:?}");
            } else {
                for h in hits {
                    let snippet = one_line(&h.snippet.replace(['\u{2}', '\u{3}'], ""));
                    let what = match h.kind {
                        SearchHitKind::Record => format!(
                            "record {} {:?}",
                            h.record_id.unwrap_or_default(),
                            h.title.unwrap_or_default()
                        ),
                        SearchHitKind::Event => format!(
                            "session {} #{} ({})",
                            h.session_id.unwrap_or_default(),
                            h.seq.unwrap_or(0),
                            h.agent.unwrap_or_default()
                        ),
                    };
                    println!("{}  {what}\n    {snippet}", date(h.ts));
                }
            }
        }
        MemCommand::Brief { project, json } => {
            let pid = project_of(&store, paths, project)?;
            let p = store.live_project(&pid)?;
            let brief = store.get_brief(&pid)?;
            let records = store.list_records(
                &pid,
                &RecordFilter {
                    status: Some(RecordStatus::Active),
                    kind: None,
                },
            )?;
            if json {
                print_json(&json!({"project": p, "brief": brief, "records": records}))?;
            } else {
                println!("# {} ({})\n", p.name, p.id);
                match brief {
                    Some(b) => println!("{}\n", b.body_md.trim()),
                    None => println!("(no brief yet)\n"),
                }
                for r in records {
                    println!(
                        "[{}] {}{}",
                        r.kind,
                        r.title,
                        if r.pinned { " (pinned)" } else { "" }
                    );
                    if !r.body.trim().is_empty() {
                        println!("    {}", clip(&one_line(&r.body), 300));
                    }
                }
            }
        }
        MemCommand::Show {
            session_id,
            limit,
            json,
        } => {
            let s = store
                .get_session(&session_id)?
                .with_context(|| format!("session {session_id} not found"))?;
            let (events, _) = store.events_page(&session_id, 0, i64::from(limit.clamp(1, 1000)))?;
            if json {
                print_json(&json!({"session": s, "events": events}))?;
            } else {
                println!(
                    "{} · {} · {} · {}\n{}",
                    s.id,
                    s.agent,
                    date(s.started_at),
                    s.status,
                    s.title.as_deref().unwrap_or("(untitled)")
                );
                if let Some(sum) = parse_summary(&s) {
                    if let Some(t) = sum.summary {
                        println!("\n{t}");
                    }
                    if let Some(e) = sum.error {
                        println!("\nlast distill failed: {}", e.message);
                    }
                }
                println!();
                for e in events {
                    println!("[{} {}] {}", e.seq, e.kind, clip(e.text.trim(), 2000));
                }
            }
        }
        MemCommand::Recent {
            project,
            limit,
            json,
        } => {
            let pid = project_of(&store, paths, project)?;
            let sessions = store.recent_sessions(&pid, i64::from(limit.clamp(1, 200)))?;
            if json {
                print_json(&sessions)?;
            } else if sessions.is_empty() {
                println!("no sessions");
            } else {
                for s in sessions {
                    println!(
                        "{}  {}  {:<8} {:<9} {}",
                        date(s.started_at),
                        s.id,
                        s.agent,
                        s.status,
                        s.title.as_deref().unwrap_or("(untitled)")
                    );
                    if let Some(t) = parse_summary(&s).and_then(|x| x.summary) {
                        println!("    {}", clip(&one_line(&t), 300));
                    }
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn run_hooks(cmd: HooksCommand) -> anyhow::Result<ExitCode> {
    let homes = Homes::from_env().context("cannot determine the home directory")?;
    let (agent, action) = match cmd {
        HooksCommand::Install { agent } => (agent, "install"),
        HooksCommand::Uninstall { agent } => (agent, "uninstall"),
        HooksCommand::Status { agent } => (agent, "status"),
    };
    let config = blirp_core::config::Config::default();
    let agents: Vec<String> = match agent {
        Some(a) => {
            if !install::SUPPORTED.contains(&a.as_str()) {
                bail!(
                    "global integration is not supported for {a} (supported: {})",
                    install::SUPPORTED.join(", ")
                );
            }
            vec![a]
        }
        None => install::SUPPORTED
            .iter()
            .filter(|a| {
                action != "install"
                    || crate::agents::Agent::resolve(a, &config).is_ok_and(|x| x.path.is_some())
            })
            .map(|a| a.to_string())
            .collect(),
    };
    // Written into the agents' own config files, so it must outlive this process.
    let exe = match action {
        "install" => Some(crate::memory::persistent_exe(crate::memory::blirp_exe())?),
        _ => None,
    };
    let mut failed = false;
    for a in agents {
        let result = match (action, &exe) {
            ("install", Some(exe)) => install::install(&a, &homes, exe),
            ("uninstall", _) => install::uninstall(&a, &homes),
            _ => Ok(install::status(&a, &homes)),
        };
        match result {
            Ok(s) => {
                println!("{a:<9} hooks {:<13} mcp {}", s.hooks, s.mcp);
                if let Some(d) = s.detail
                    && action == "install"
                {
                    println!("          {d}");
                }
            }
            Err(e) => {
                failed = true;
                eprintln!("{a:<9} error: {e:#}");
            }
        }
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
