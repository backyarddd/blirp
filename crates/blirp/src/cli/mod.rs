//! Command line (§4).

mod later;
mod mem;

use anyhow::{Context as _, bail};
use blirp_core::model::{Health, SessionsPage};
use blirp_core::paths::{Paths, RuntimeInfo};
use clap::{Parser, Subcommand};
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "blirp",
    version,
    about = "Workspace and memory for CLI coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
    /// Open the UI in the browser (logged in).
    Open,
    /// List recent sessions.
    Sessions {
        /// Only sessions of this project id.
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
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
    #[command(flatten)]
    Later(later::LaterCommand),
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    // Hooks must stay fast and always succeed: no runtime, no logging, no
    // failure exit even without a home directory.
    if let Command::Hook {
        agent,
        event,
        global,
    } = &cli.command
    {
        crate::hooks::main(agent, event, *global);
        return ExitCode::SUCCESS;
    }
    let paths = match Paths::resolve() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    match cli.command {
        Command::Later(cmd) => later::run(&cmd),
        Command::Mem(cmd) => report(mem::run_mem(&paths, cmd)),
        Command::Hooks(cmd) => report(mem::run_hooks(cmd)),
        command => run_async(command, paths),
    }
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
            crate::daemon::run_foreground(paths, port).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Status => status(&paths).await,
        Command::Open => open(&paths).await,
        Command::Sessions { project, limit } => sessions(&paths, project, limit).await,
        Command::Doctor => doctor(&paths).await,
        Command::Mcp => {
            crate::mcp::serve_stdio(paths).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Later(_) | Command::Hook { .. } | Command::Mem(_) | Command::Hooks(_) => {
            Ok(ExitCode::from(2))
        }
    }
}

struct Client {
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
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
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
    let url = format!(
        "{}/auth?token={}",
        client.info.base_url(),
        client.info.token
    );
    open_browser(&url).context("launch browser")?;
    println!("Opened {} in your browser", client.info.base_url());
    Ok(ExitCode::SUCCESS)
}

async fn sessions(paths: &Paths, project: Option<String>, limit: u32) -> anyhow::Result<ExitCode> {
    let client = Client::connect(paths).await?;
    let mut path = format!("/api/sessions?limit={limit}");
    if let Some(p) = project {
        path.push_str(&format!("&project={p}"));
    }
    let page: SessionsPage = client.get(&path).await?;
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
    let agents = tokio::task::spawn_blocking(move || crate::agents::detect_all(&config)).await?;
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

    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
