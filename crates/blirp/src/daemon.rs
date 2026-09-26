//! Daemon process (§4): single-instance lock, runtime.json, HTTP server,
//! status monitor, graceful shutdown, `--detach`.

use crate::state::{AppState, SharedState};
use anyhow::{Context as _, bail};
use blirp_core::config::Config;
use blirp_core::model::{Machine, SessionStatus};
use blirp_core::paths::{Paths, RuntimeInfo};
use blirp_core::store::{MACHINE_ID_KEY, Store};
use std::collections::HashMap;
use std::fs::File;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const STATUS_TICK: Duration = Duration::from_millis(500);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub struct DaemonOptions {
    pub paths: Paths,
    /// Overrides `daemon.port`; `Some(0)` binds an ephemeral port.
    pub port: Option<u16>,
    /// Where transcript ingest (§8) looks for agent stores; `None` disables
    /// ingest (tests that must not read the real user's agent stores).
    pub ingest: Option<crate::ingest::IngestEnv>,
}

pub struct Daemon {
    pub state: SharedState,
    pub port: u16,
    shutdown_tx: watch::Sender<bool>,
    server: JoinHandle<std::io::Result<()>>,
    monitor: JoinHandle<()>,
    ingest: Option<crate::ingest::IngestService>,
    // Held for the daemon's lifetime; the OS releases it if the process dies.
    _lock: File,
}

fn acquire_lock(paths: &Paths) -> anyhow::Result<File> {
    let path = paths.lock_file();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open lock file {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            let hint = match RuntimeInfo::read(paths) {
                Ok(Some(r)) => format!(" (pid {}, port {})", r.pid, r.port),
                _ => String::new(),
            };
            bail!(
                "another blirp daemon is already running for {}{hint}",
                paths.home().display()
            )
        }
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("lock {}", path.display()))
        }
    }
}

/// This machine's row. Its id is the iroh endpoint id (§10); an older
/// install that used a local uuid is rebound to it once.
fn load_machine(store: &Store, config: &Config, id: String) -> anyhow::Result<Machine> {
    match store.get_setting(MACHINE_ID_KEY)? {
        Some(serde_json::Value::String(old)) if old == id => {}
        Some(serde_json::Value::String(old)) => {
            tracing::info!(%old, new = %id, "moving this machine to its endpoint id");
            store.rebind_machine(&old, &id)?;
            store.set_setting(MACHINE_ID_KEY, &serde_json::Value::String(id.clone()))?;
        }
        _ => store.set_setting(MACHINE_ID_KEY, &serde_json::Value::String(id.clone()))?,
    }
    let machine = Machine {
        id,
        name: config.machine.name.clone(),
        os: std::env::consts::OS.to_string(),
        role: config.sync.role,
        last_seen: blirp_core::now_ms(),
        revoked: false,
    };
    store.upsert_machine(&machine)?;
    Ok(machine)
}

async fn bind(port: u16) -> anyhow::Result<TcpListener> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    match TcpListener::bind(addr).await {
        Ok(l) => Ok(l),
        Err(e) if port != 0 && e.kind() == std::io::ErrorKind::AddrInUse => {
            tracing::warn!(port, "port in use; falling back to an ephemeral port");
            Ok(TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?)
        }
        Err(e) => Err(e).with_context(|| format!("bind 127.0.0.1:{port}")),
    }
}

impl Daemon {
    /// Start serving. Returns once the listener is bound and runtime.json written.
    pub async fn start(opts: DaemonOptions) -> anyhow::Result<Daemon> {
        crate::install_crypto_provider();
        let paths = opts.paths;
        let ingest_env = opts.ingest;
        paths.ensure_dirs()?;
        let lock = acquire_lock(&paths)?;
        let config = Config::load_or_init(&paths.config_file())?;
        let store = Arc::new(
            Store::open(&paths.db_file())
                .with_context(|| format!("open database {}", paths.db_file().display()))?,
        );
        let identity = blirp_sync::identity::load_or_create(&paths.identity_key())?;
        let machine = load_machine(&store, &config, blirp_sync::identity::machine_id(&identity))?;
        let token = blirp_core::random_hex::<32>().context("generate token")?;
        let listener = bind(opts.port.unwrap_or(config.daemon.port)).await?;
        let port = listener.local_addr()?.port();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let state: SharedState = Arc::new(AppState::new(
            paths.clone(),
            store,
            config,
            machine,
            token.clone(),
            port,
            shutdown_rx.clone(),
        ));
        state.sync.set_identity(identity);
        crate::sessions::mark_detached(&state)?;
        crate::memory::distill::Distiller::start(state.clone());

        let app = crate::api::router(state.clone());
        let mut server_shutdown = shutdown_rx.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    // The flag only ever flips to true; Err means the sender is
                    // gone, which also means shut down.
                    let _ = server_shutdown.changed().await;
                })
                .await
        });

        let monitor_state = state.clone();
        let mut monitor_shutdown = shutdown_rx;
        let monitor = tokio::spawn(async move {
            let mut last: HashMap<String, SessionStatus> = HashMap::new();
            let mut tick = tokio::time::interval(STATUS_TICK);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let st = monitor_state.clone();
                        let owned = std::mem::take(&mut last);
                        match tokio::task::spawn_blocking(move || {
                            let mut owned = owned;
                            crate::sessions::refresh_statuses(&st, &mut owned);
                            owned
                        })
                        .await
                        {
                            Ok(l) => last = l,
                            Err(e) => tracing::error!(error = %e, "status monitor task failed"),
                        }
                    }
                    _ = monitor_shutdown.changed() => break,
                }
            }
        });

        crate::sync::start(&state).await;
        let ingest = ingest_env.map(|env| {
            let emit_state = state.clone();
            let engine = crate::ingest::Engine::new(
                state.store.clone(),
                state.machine.clone(),
                env,
                Arc::new(move |e| emit_state.emit(e)),
            );
            crate::ingest::IngestService::start(Arc::new(engine))
        });

        RuntimeInfo {
            pid: std::process::id(),
            port,
            token,
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: blirp_core::now_ms(),
        }
        .write(&paths)?;
        tracing::info!(port, home = %paths.home().display(), "blirp daemon listening on 127.0.0.1");
        Ok(Daemon {
            state,
            port,
            shutdown_tx,
            server,
            monitor,
            ingest,
            _lock: lock,
        })
    }

    pub fn token(&self) -> &str {
        &self.state.token
    }

    /// Stop accepting requests, end sessions (they become `detached`), close
    /// sockets and remove runtime.json.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        let terms = self.state.terminals.all();
        for t in &terms {
            t.kill_for_shutdown();
        }
        let _ = self.shutdown_tx.send(true);
        crate::sync::stop(&self.state).await;
        if let Some(ingest) = self.ingest {
            ingest.shutdown().await;
        }
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        while !self.state.terminals.is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        match tokio::time::timeout_at(deadline, self.server).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => tracing::error!(error = %e, "server error during shutdown"),
            Ok(Err(e)) => tracing::error!(error = %e, "server task failed"),
            Err(_) => tracing::warn!("connections did not close in time; exiting anyway"),
        }
        if let Err(e) = self.monitor.await {
            tracing::error!(error = %e, "status monitor failed");
        }
        RuntimeInfo::remove_if_owned(&self.state.paths, std::process::id())?;
        tracing::info!("blirp daemon stopped");
        Ok(())
    }
}

/// Wait for Ctrl+C, SIGTERM (unix) or console close/shutdown (Windows).
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    r = tokio::signal::ctrl_c() => log_signal_err(r),
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot listen for SIGTERM");
                log_signal_err(tokio::signal::ctrl_c().await);
            }
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        match (windows::ctrl_close(), windows::ctrl_shutdown()) {
            (Ok(mut close), Ok(mut shut)) => {
                tokio::select! {
                    r = tokio::signal::ctrl_c() => log_signal_err(r),
                    _ = close.recv() => {}
                    _ = shut.recv() => {}
                }
            }
            _ => log_signal_err(tokio::signal::ctrl_c().await),
        }
    }
}

fn log_signal_err(r: std::io::Result<()>) {
    if let Err(e) = r {
        tracing::error!(error = %e, "waiting for Ctrl+C failed");
    }
}

/// `blirp daemon` in the foreground.
pub async fn run_foreground(paths: Paths, port: Option<u16>) -> anyhow::Result<()> {
    let ingest = crate::ingest::IngestEnv::from_process(paths.home());
    if ingest.is_none() {
        tracing::warn!("home directory unknown; transcript ingest disabled");
    }
    let daemon = Daemon::start(DaemonOptions {
        paths,
        port,
        ingest,
    })
    .await?;
    shutdown_signal().await;
    tracing::info!("shutting down");
    daemon.shutdown().await
}

/// Probe a running daemon via runtime.json + `/api/health`.
pub async fn running_daemon(paths: &Paths) -> Option<RuntimeInfo> {
    let info = RuntimeInfo::read(paths).ok().flatten()?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;
    let ok = client
        .get(format!("{}/api/health", info.base_url()))
        .bearer_auth(&info.token)
        .send()
        .await
        .is_ok_and(|r| r.status().is_success());
    ok.then_some(info)
}

/// `blirp daemon --detach`: start a background daemon and return once it is healthy.
pub async fn detach(paths: &Paths, port: Option<u16>) -> anyhow::Result<RuntimeInfo> {
    if let Some(info) = running_daemon(paths).await {
        return Ok(info);
    }
    let exe = std::env::current_exe().context("locate blirp executable")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .env(blirp_core::paths::HOME_ENV, paths.home())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(p) = port {
        cmd.args(["--port", &p.to_string()]);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        #[allow(unsafe_code)]
        // SAFETY: setsid is async-signal-safe and touches no Rust state in the
        // forked child; it detaches the daemon from the controlling terminal.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().context("spawn background daemon")?;
    let pid = child.id();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait()? {
            if let Some(info) = running_daemon(paths).await {
                // Lost a race with another starter; that daemon is fine.
                return Ok(info);
            }
            bail!(
                "daemon exited during startup ({status}); see {}",
                paths.logs_dir().display()
            );
        }
        if let Some(info) = running_daemon(paths).await
            && info.pid == pid
        {
            return Ok(info);
        }
        if std::time::Instant::now() > deadline {
            bail!(
                "daemon did not become healthy within 20s; see {}",
                paths.logs_dir().display()
            );
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Tracing to `logs/blirpd.<date>.log` (daily, keep 7) and stderr.
pub fn init_logging(paths: &Paths) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    use tracing_subscriber::{EnvFilter, fmt};

    paths.ensure_dirs()?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("blirpd")
        .filename_suffix("log")
        .max_log_files(7)
        .build(paths.logs_dir())
        .context("create log file")?;
    let (file, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("BLIRP_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(file).with_ansi(false))
        .with(fmt::layer().with_writer(std::io::stderr))
        .try_init()
        .context("install log subscriber")?;
    Ok(guard)
}
