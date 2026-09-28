//! Command line (§4).

mod agents;
mod install;
mod lifecycle;
mod manage;
mod mem;
mod server;
pub mod service;
mod skills;
mod sync;
mod worktrees;

use anyhow::{Context as _, bail};
use blirp_core::model::{AgentAuth, AgentInfo, Health, SessionsPage};
use blirp_core::paths::{Paths, RuntimeInfo};
use clap::{Parser, Subcommand};
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "blirp",
    version,
    about = "Workspace and memory for CLI coding agents",
    long_about = "Workspace and memory for CLI coding agents.\n\nWithout a command, blirp \
                  starts the daemon and opens the desktop app (or the browser UI)."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon (API, terminals, memory).
    Daemon {
        /// Start in the background and return once it is healthy.
        #[arg(long)]
        detach: bool,
        /// Port to listen on (overrides config; 0 = ephemeral).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Show whether the daemon is running.
    Status,
    /// Start the daemon in the background unless it is running: through the
    /// autostart service when one is installed, else like `daemon --detach`.
    Start,
    /// Stop the daemon gracefully (sessions end as detached); kills it if it
    /// does not stop within 15 s.
    Stop,
    /// Print the end of the daemon log.
    Logs {
        /// Number of lines to print.
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: usize,
        /// Keep printing new lines (Ctrl+C to quit).
        #[arg(short, long)]
        follow: bool,
    },
    /// Open the UI in the browser (logged in).
    Open,
    /// List recent sessions, or rename, move, delete or stop one.
    #[command(args_conflicts_with_subcommands = true)]
    Sessions {
        #[command(subcommand)]
        action: Option<manage::SessionsAction>,
        /// Only sessions of this project id.
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
        /// Print JSON (the API's sessions page).
        #[arg(long)]
        json: bool,
    },
    /// List, rename, delete (to the Trash), restore or merge projects.
    #[command(subcommand)]
    Projects(manage::ProjectsCommand),
    /// Check the installation.
    Doctor,
    /// Search and show project memory.
    #[command(subcommand)]
    Mem(mem::MemCommand),
    /// Hook entry point used by agents (always exits 0 within 2 s).
    Hook {
        agent: String,
        event: String,
        /// Entry installed by `blirp hooks install` (not a per-launch config).
        #[arg(long)]
        global: bool,
    },
    /// MCP server over stdio (spawned by agents).
    Mcp,
    /// Install or remove global agent hooks and MCP registration.
    #[command(subcommand)]
    Hooks(mem::HooksCommand),
    /// Install or remove blirp's Agent Skills (SKILL.md) for agents.
    #[command(subcommand)]
    Skills(skills::SkillsCommand),
    /// Headless agent login: a `claude setup-token` token for the claude
    /// sessions and summarizer the daemon starts.
    #[command(subcommand)]
    Agents(agents::AgentsCommand),
    /// Pair this machine with a hub: `blirp pair <invite> <code>`, or just
    /// `blirp pair <code>` to find the hub on the local network.
    Pair {
        /// Invite (`blirp1-...`), join link (`blirp://join/...`) or, alone, the code.
        first: String,
        /// Code shown on the hub (`XXXX-XXXX`).
        code: Option<String>,
    },
    /// Make this machine the hub other machines pair with.
    Hub {
        #[command(subcommand)]
        action: sync::HubAction,
    },
    /// Write a consistent copy of the database (safe while the daemon runs).
    Backup {
        /// Where to write it; must not exist yet.
        file: std::path::PathBuf,
    },
    /// Paired machines and browser devices (hub).
    Devices {
        #[command(subcommand)]
        action: sync::DevicesAction,
    },
    /// Git worktrees blirp created for sessions.
    #[command(subcommand)]
    Worktrees(worktrees::WorktreesCommand),
    /// Install or remove autostart of the daemon at login.
    Service {
        #[command(subcommand)]
        cmd: service::ServiceCommand,
    },
    /// Start the daemon and open the desktop app, or the browser UI when the
    /// app is not installed (same as `blirp` without a command).
    App,
    /// Update blirp (and the desktop app installed with it) to the latest release.
    Update {
        /// Only check: exit 0 when up to date, 10 when an update is available.
        #[arg(long)]
        check: bool,
        /// Install this version instead of the latest (allows downgrades).
        #[arg(long, value_name = "X.Y.Z")]
        version: Option<String>,
    },
    /// Remove blirp: stops the daemon, removes autostart, agent hooks, blirp's
    /// skills and the installed files. Your data in ~/.blirp stays unless --purge.
    Uninstall {
        /// Also delete the data folder (memory, sessions, settings).
        #[arg(long)]
        purge: bool,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
}

pub fn main() -> ExitCode {
    crate::install_crypto_provider();
    let cli = Cli::parse();
    // Hooks must stay fast and always succeed: no runtime, no logging, no
    // failure exit even without a home directory.
    if let Some(Command::Hook {
        agent,
        event,
        global,
    }) = &cli.command
    {
        crate::hooks::main(agent, event, *global);
        return ExitCode::SUCCESS;
    }
    // Executables an update renamed aside because they were running.
    #[cfg(windows)]
    if let Some(dir) = crate::memory::blirp_exe().parent() {
        crate::update::install::cleanup_old(dir);
    }
    let command = cli.command.unwrap_or(Command::App);
    let paths = match Paths::resolve() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    forget_inherited_claude_token(&paths);
    match command {
        Command::Mem(cmd) => report(mem::run_mem(&paths, cmd)),
        Command::Logs { lines, follow } => report(lifecycle::logs(&paths, lines, follow)),
        Command::Worktrees(worktrees::WorktreesCommand::List) => report(worktrees::list(&paths)),
        Command::Hooks(cmd) => report(mem::run_hooks(cmd)),
        Command::Skills(cmd) => report(skills::run(cmd)),
        Command::Agents(cmd) => report(agents::run(&paths, cmd)),
        Command::Backup { file } => report(server::backup(&paths, &file)),
        command => run_async(command, paths),
    }
}

/// Run from inside a claude session blirp started (`blirp daemon --detach`,
/// `blirp update`), this process inherits that session's copy of the stored
/// login token. Dropped here, before any thread exists, so the daemon this
/// starts, and everything that daemon starts, never inherits it: claude
/// still gets the stored token (read at each spawn, so set-token and
/// clear-token keep working), other programs get nothing.
#[allow(unsafe_code)]
fn forget_inherited_claude_token(paths: &Paths) {
    if blirp_core::claude_token::env_is_stored_copy(paths) {
        // SAFETY: called from `main` before the async runtime or any other
        // thread is started, so nothing reads the environment concurrently.
        unsafe { std::env::remove_var(blirp_core::claude_token::ENV) };
    }
}

/// Ask `question` on the terminal; without one (a script, a pipe) the
/// command fails and names `--yes`.
pub(crate) fn confirm(question: &str) -> anyhow::Result<bool> {
    use std::io::{BufRead as _, IsTerminal as _, Write as _};
    if !std::io::stdin().is_terminal() {
        bail!("{question} Pass --yes to confirm without a prompt.");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

fn report(r: anyhow::Result<ExitCode>) -> ExitCode {
    r.unwrap_or_else(|e| {
        eprintln!("error: {e:#}");
        ExitCode::FAILURE
    })
}

fn run_async(command: Command, paths: Paths) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = rt.block_on(run(command, paths));
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cmd: Command, paths: Paths) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::Daemon { detach: true, port } => {
            let info = crate::daemon::detach(&paths, port).await?;
            println!(
                "blirp daemon running (pid {}, port {})",
                info.pid, info.port
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Daemon {
            detach: false,
            port,
        } => {
            let _guard = crate::daemon::init_logging(&paths)?;
            if let Err(e) = crate::daemon::run_foreground(paths.clone(), port).await {
                // A healthy daemon already serves this data dir, so there is
                // nothing to do. Exit 0: launchd (KeepAlive SuccessfulExit =
                // false) and systemd (Restart=on-failure) then stop restarting
                // this duplicate every few seconds.
                if e.downcast_ref::<crate::daemon::AlreadyRunning>().is_some()
                    && crate::daemon::running_daemon(&paths).await.is_some()
                {
                    tracing::info!(error = format!("{e:#}"), "not starting a second daemon");
                    eprintln!("blirp: {e:#}");
                    return Ok(ExitCode::SUCCESS);
                }
                // A detached daemon's stderr goes nowhere; the log is where
                // `--detach` and the desktop app tell the user to look.
                tracing::error!(error = format!("{e:#}"), "daemon failed");
                return Err(e);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Status => status(&paths).await,
        Command::Start => lifecycle::start(&paths).await,
        Command::Stop => lifecycle::stop(&paths).await,
        Command::Open => open(&paths).await,
        Command::Sessions {
            action: Some(action),
            ..
        } => manage::sessions(&Client::connect(&paths).await?, action).await,
        Command::Sessions {
            action: None,
            project,
            limit,
            json,
        } => sessions(&paths, project, limit, json).await,
        Command::Projects(cmd) => manage::projects(&Client::connect(&paths).await?, cmd).await,
        Command::Doctor => doctor(&paths).await,
        Command::Mcp => {
            crate::mcp::serve_stdio(paths).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Pair { first, code } => {
            sync::pair(&Client::connect(&paths).await?, first, code).await
        }
        Command::Hub {
            action: sync::HubAction::Setup { lan },
        } => server::hub_setup(&paths, lan).await,
        Command::Hub { action } => sync::hub(&Client::connect(&paths).await?, action).await,
        Command::Devices { action } => sync::devices(&Client::connect(&paths).await?, action).await,
        Command::Service { cmd } => service::run(&cmd, &paths).await,
        Command::Worktrees(worktrees::WorktreesCommand::Prune) => worktrees::prune(&paths).await,
        Command::App => install::launch(&paths).await,
        Command::Update { check, version } => install::update(&paths, check, version).await,
        Command::Uninstall { purge, yes } => install::uninstall(&paths, purge, yes).await,
        // Dispatched synchronously in `main` before the runtime starts.
        Command::Hook { .. }
        | Command::Mem(_)
        | Command::Hooks(_)
        | Command::Skills(_)
        | Command::Agents(_)
        | Command::Backup { .. }
        | Command::Logs { .. }
        | Command::Worktrees(worktrees::WorktreesCommand::List) => Ok(ExitCode::from(2)),
    }
}

pub(crate) struct Client {
    info: RuntimeInfo,
    http: reqwest::Client,
}

impl Client {
    async fn connect(paths: &Paths) -> anyhow::Result<Client> {
        let info = crate::daemon::running_daemon(paths)
            .await
            .context("blirp daemon is not running; start it with `blirp daemon --detach`")?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Client { info, http })
    }

    /// Send a request; non-2xx answers become errors with the API message.
    pub(crate) async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> anyhow::Result<reqwest::Response> {
        let mut req = self
            .http
            .request(method.clone(), format!("{}{path}", self.info.base_url()))
            .bearer_auth(&self.info.token)
            // Pairing waits for the hub (connect + handshake).
            .timeout(Duration::from_secs(120));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        match serde_json::from_str::<blirp_core::model::ErrorBody>(&text) {
            Ok(e) => bail!("{} ({})", e.error.message, e.error.code),
            Err(_) => bail!("{method} {path} failed: {status} {text}"),
        }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let resp = self
            .http
            .get(format!("{}{path}", self.info.base_url()))
            .bearer_auth(&self.info.token)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!(
                "GET {path} failed: {} {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }
        Ok(resp.json().await?)
    }
}

async fn status(paths: &Paths) -> anyhow::Result<ExitCode> {
    let Some(info) = crate::daemon::running_daemon(paths).await else {
        println!("blirp daemon is not running ({})", paths.home().display());
        return Ok(ExitCode::FAILURE);
    };
    let client = Client::connect(paths).await?;
    let health: Health = client.get("/api/health").await?;
    println!("blirp daemon running");
    println!("  version  {}", health.version);
    println!("  pid      {}", info.pid);
    println!("  url      {}", info.base_url());
    println!("  machine  {} ({})", health.machine.name, health.machine.id);
    println!("  role     {}", health.role);
    println!("  data     {}", paths.home().display());
    Ok(ExitCode::SUCCESS)
}

fn open_browser(url: &str) -> std::io::Result<()> {
    let mut cmd = if cfg!(windows) {
        let mut c = blirp_core::process::command("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = blirp_core::process::command("open");
        c.arg(url);
        c
    } else {
        let mut c = blirp_core::process::command("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

async fn open(paths: &Paths) -> anyhow::Result<ExitCode> {
    let client = Client::connect(paths).await?;
    // The token rides in the fragment, which the browser never sends to a
    // server; the UI stores it and removes it from the address bar.
    let url = format!("{}/#token={}", client.info.base_url(), client.info.token);
    open_browser(&url).context("launch browser")?;
    println!("Opened {} in your browser", client.info.base_url());
    Ok(ExitCode::SUCCESS)
}

async fn sessions(
    paths: &Paths,
    project: Option<String>,
    limit: u32,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let client = Client::connect(paths).await?;
    // Only the path and query are sent; the base just makes the URL parse.
    let mut url = reqwest::Url::parse("http://blirp.local/api/sessions")?;
    url.query_pairs_mut()
        .append_pair("limit", &limit.to_string());
    if let Some(p) = project {
        // An unknown id is an error (as in `blirp mem`), not "no sessions".
        let mut check = reqwest::Url::parse("http://blirp.local/")?;
        check
            .path_segments_mut()
            .map_err(|()| anyhow::anyhow!("building the project path"))?
            .extend(["api", "projects", p.as_str()]);
        client
            .send(reqwest::Method::GET, check.path(), None)
            .await?;
        url.query_pairs_mut().append_pair("project", &p);
    }
    let path = format!("{}?{}", url.path(), url.query().unwrap_or_default());
    let page: SessionsPage = client.get(&path).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(ExitCode::SUCCESS);
    }
    if page.items.is_empty() {
        println!("no sessions");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<36}  {:<10}  {:<10}  TITLE / FOLDER",
        "ID", "STATUS", "AGENT"
    );
    for s in page.items {
        let label = s.title.unwrap_or(s.cwd);
        println!("{:<36}  {:<10}  {:<10}  {}", s.id, s.status, s.agent, label);
    }
    Ok(ExitCode::SUCCESS)
}

async fn doctor(paths: &Paths) -> anyhow::Result<ExitCode> {
    let mut failed = false;
    let mut check = |ok: bool, name: &str, detail: String| {
        println!("[{}] {name}: {detail}", if ok { " ok " } else { "FAIL" });
        failed |= !ok;
    };

    let writable = paths
        .ensure_dirs()
        .map_err(anyhow::Error::from)
        .and_then(|()| {
            let probe = paths.home().join(".doctor-probe");
            std::fs::write(&probe, b"ok")?;
            std::fs::remove_file(&probe)?;
            Ok(())
        });
    check(
        writable.is_ok(),
        "data dir writable",
        match &writable {
            Ok(()) => paths.home().display().to_string(),
            Err(e) => format!("{}: {e}", paths.home().display()),
        },
    );

    let cfg = if paths.config_file().exists() {
        std::fs::read_to_string(paths.config_file())
            .map_err(anyhow::Error::from)
            .and_then(|t| Ok(blirp_core::config::Config::parse(&t, &paths.config_file())?))
    } else {
        Ok(blirp_core::config::Config::default())
    };
    check(
        cfg.is_ok(),
        "config",
        match &cfg {
            Ok(_) => paths.config_file().display().to_string(),
            Err(e) => e.to_string(),
        },
    );

    let db_path = paths.db_file();
    let db = tokio::task::spawn_blocking(move || -> anyhow::Result<(i64, String)> {
        let store = blirp_core::store::Store::open(&db_path)?;
        Ok((store.schema_version()?, store.quick_check()?))
    })
    .await?;
    check(
        matches!(&db, Ok((_, qc)) if qc == "ok"),
        "database",
        match &db {
            Ok((v, qc)) => format!("schema v{v}, quick_check {qc}"),
            Err(e) => format!("{e:#}"),
        },
    );

    let daemon = crate::daemon::running_daemon(paths).await;
    check(
        daemon.is_some(),
        "daemon reachable",
        match &daemon {
            Some(i) => format!("{} (pid {})", i.base_url(), i.pid),
            None => "not running; start it with `blirp daemon --detach`".into(),
        },
    );

    let git = tokio::task::spawn_blocking(blirp_core::git::is_installed).await?;
    check(
        git,
        "git",
        if git {
            "installed".into()
        } else {
            "not found on PATH (git features disabled)".into()
        },
    );

    let config = cfg.unwrap_or_default();
    let client = match &daemon {
        Some(_) => Client::connect(paths).await.ok(),
        None => None,
    };
    for line in server::doctor_lines(&config, client.as_ref()).await {
        println!("{line}");
    }
    println!(
        "[info] LAN discovery: {}",
        lan_discovery_line(&config, paths, daemon.is_some())
    );
    let p = paths.clone();
    let agents =
        tokio::task::spawn_blocking(move || crate::agents::detect_all(&config, &p)).await?;
    let found: Vec<String> = agents
        .iter()
        .filter(|a| a.installed && a.id != "shell")
        .map(|a| match &a.version {
            Some(v) => format!("{} ({v})", a.id),
            None => a.id.clone(),
        })
        .collect();
    // Having no agent installed is legal (shell sessions still work); report only.
    println!(
        "[info] agents: {}",
        if found.is_empty() {
            "none detected".into()
        } else {
            found.join(", ")
        }
    );

    // What matters is the daemon's view (its environment, its keychain
    // access); this shell's is the fallback when it is not running.
    let from_daemon = match &client {
        Some(c) => c.get::<Vec<AgentInfo>>("/api/agents").await.ok(),
        None => None,
    };
    let claude = match from_daemon {
        Some(list) => list
            .into_iter()
            .find(|a| a.id == "claude")
            .map(|a| (a, "the daemon")),
        None => agents
            .iter()
            .find(|a| a.id == "claude")
            .cloned()
            .map(|a| (a, "this shell")),
    };
    if let Some((a, seen_by)) = claude.filter(|(a, _)| a.installed) {
        println!("[info] claude auth: {}", claude_auth_line(&a, seen_by));
    }

    match skills::home_targets() {
        Ok(targets) => {
            for t in targets {
                println!("[info] skills {}: {}", t.label, skills::summary(&t));
            }
        }
        Err(e) => println!("[info] skills: {e:#}"),
    }

    match crate::ingest::IngestEnv::from_process(paths.home()) {
        Some(env) => {
            let db_path = paths.db_file();
            let status = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let store = blirp_core::store::Store::open(&db_path)?;
                Ok(crate::ingest::engine::status(&store, &env))
            })
            .await?;
            match status {
                Ok(list) => {
                    for s in list {
                        println!("[info] ingest {}: {}", s.id, ingest_line(&s));
                    }
                }
                Err(e) => println!("[info] ingest: status unavailable ({e:#})"),
            }
        }
        None => println!("[info] ingest: home directory unknown, ingest disabled"),
    }

    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// LAN discovery (mDNS) state for `blirp doctor`, with a current mDNS send
/// failure from the running daemon's log.
fn lan_discovery_line(
    config: &blirp_core::config::Config,
    paths: &Paths,
    daemon_running: bool,
) -> String {
    use blirp_core::model::MachineRole;
    if !config.sync.lan_discovery {
        return "off (sync.lan_discovery = false); pairing needs the hub's invite".into();
    }
    let mut line = match config.sync.role {
        MachineRole::Standalone => "on; used once this machine is a hub or paired".to_string(),
        _ => "on".to_string(),
    };
    if let Some(failure) = daemon_running
        .then(|| lifecycle::recent_mdns_failure(paths))
        .flatten()
    {
        line.push_str(&format!("; the daemon cannot send mDNS ({failure})"));
    }
    if cfg!(target_os = "macos") {
        line.push_str(
            "; macOS: needs the Local Network permission (System Settings > Privacy & Security > \
             Local Network), see docs/troubleshooting.md",
        );
    }
    line
}

/// How claude sessions started by the daemon log in, for `blirp doctor`.
fn claude_auth_line(a: &AgentInfo, seen_by: &str) -> String {
    let token = a.token.as_ref();
    let source = if token.is_some_and(|t| t.env) {
        "CLAUDE_CODE_OAUTH_TOKEN from the environment"
    } else if token.is_some_and(|t| t.stored) {
        "stored login token (`blirp agents set-token claude`)"
    } else {
        "keychain or credentials file (no login token stored)"
    };
    let state = match &a.auth {
        Some(AgentAuth {
            logged_in: true,
            method,
        }) => format!(
            "logged in ({})",
            method.as_deref().unwrap_or("unknown method")
        ),
        Some(_) => "not logged in".to_string(),
        None => "login state unknown".to_string(),
    };
    let hint = if token.is_some_and(|t| t.env || t.stored) {
        ""
    } else {
        "; for a headless hub run `claude setup-token`, then `blirp agents set-token claude`"
    };
    format!("{source}, {state} as seen by {seen_by}{hint}")
}

/// `root <path>, <n> sources, last ingest <time>` for `blirp doctor`.
fn ingest_line(s: &crate::ingest::engine::AdapterStatus) -> String {
    use chrono::TimeZone as _;
    let root = match s.roots_found.as_slice() {
        [] if !s.has_roots => "scans registered project folders".to_string(),
        [] => "no store found".to_string(),
        roots => format!(
            "root {}",
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let sources = match &s.sources {
        Ok(n) => format!("{n} sources"),
        Err(e) => format!("scan failed: {e}"),
    };
    let last = s
        .last_ingest_at
        .and_then(|ms| chrono::Local.timestamp_millis_opt(ms).single())
        .map_or_else(
            || "never ingested".to_string(),
            |t| format!("last ingest {}", t.format("%Y-%m-%d %H:%M:%S")),
        );
    format!("{root}, {sources}, {last}")
}
