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
    /// Overrides `daemon.port`; `Some(0)` binds an ephemeral port. A taken
    /// port fails the start.
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

/// Bind the loopback listener. A taken port is an error, never a silent
/// move to another port: whoever holds it would otherwise answer at the
/// address clients and bookmarks know. `0` asks for a free port.
async fn bind(port: u16, config_file: &std::path::Path) -> anyhow::Result<TcpListener> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    match crate::bind_exclusive(addr).and_then(TcpListener::from_std) {
        Ok(l) => Ok(l),
        Err(e) if port != 0 && e.kind() == std::io::ErrorKind::AddrInUse => {
            let owner = tokio::task::spawn_blocking(move || port_owner(port))
                .await
                .ok()
                .flatten();
            let by = owner.map(|o| format!(" by {o}")).unwrap_or_default();
            bail!(
                "port {port} on 127.0.0.1 is already in use{by}. Stop that program, or set \
                 another port under [daemon] in {} (`port = 0` picks a free port at every \
                 start; clients find it in runtime.json)",
                config_file.display()
            )
        }
        Err(e) => Err(e).with_context(|| format!("bind 127.0.0.1:{port}")),
    }
}

/// Best effort: the program listening on TCP `port`, as `name (pid N)`.
/// Other users' processes are usually not visible; then `None`.
fn port_owner(port: u16) -> Option<String> {
    use blirp_core::process::{command, run};
    let text = |cmd| {
        run(cmd, Duration::from_secs(3), 1 << 20)
            .ok()
            .filter(|o| o.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    if cfg!(windows) {
        let mut ns = command("netstat");
        ns.args(["-ano", "-p", "TCP"]);
        let pid = netstat_listener(&text(ns)?, port)?;
        let mut tl = command("tasklist");
        tl.args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
        return Some(match text(tl).and_then(|t| tasklist_name(&t)) {
            Some(n) => format!("{n} (pid {pid})"),
            None => format!("pid {pid}"),
        });
    }
    let mut lsof = command("lsof");
    lsof.args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"]);
    if let Some(o) = text(lsof).and_then(|t| lsof_owner(&t)) {
        return Some(o);
    }
    let mut ss = command("ss");
    ss.args(["-Hltnp", &format!("sport = :{port}")]);
    text(ss).and_then(|t| ss_owner(&t))
}

/// Pid of the socket listening on `port` (127.0.0.1 or all interfaces) in
/// `netstat -ano -p TCP` output. Listening sockets have the foreign address
/// `0.0.0.0:0`; the state column is localized, so it is not read.
fn netstat_listener(out: &str, port: u16) -> Option<u32> {
    let local = [format!("127.0.0.1:{port}"), format!("0.0.0.0:{port}")];
    out.lines().find_map(|l| {
        let cols: Vec<&str> = l.split_whitespace().collect();
        match cols.as_slice() {
            [proto, addr, "0.0.0.0:0", .., pid]
                if proto.eq_ignore_ascii_case("tcp") && local.iter().any(|a| a == addr) =>
            {
                pid.parse().ok()
            }
            _ => None,
        }
    })
}

/// Image name from `tasklist /FO CSV /NH`: `"name.exe","1234",...`.
fn tasklist_name(out: &str) -> Option<String> {
    let line = out.lines().next()?.trim();
    let name = line.strip_prefix('"')?.split('"').next()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// `lsof -Fpc`: `p<pid>` and `c<command>` lines.
fn lsof_owner(out: &str) -> Option<String> {
    let pid = out.lines().find_map(|l| l.strip_prefix('p'))?;
    Some(match out.lines().find_map(|l| l.strip_prefix('c')) {
        Some(n) => format!("{n} (pid {pid})"),
        None => format!("pid {pid}"),
    })
}

/// `ss -Hltnp`: `... users:(("name",pid=1234,fd=3))`.
fn ss_owner(out: &str) -> Option<String> {
    let users = out.split_once("users:((\"")?.1;
    let (name, rest) = users.split_once('"')?;
    let pid = rest.split_once("pid=")?.1.split([',', ')']).next()?;
    Some(format!("{name} (pid {pid})"))
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
        let listener = bind(
            opts.port.unwrap_or(config.daemon.port),
            &paths.config_file(),
        )
        .await?;
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
                            // Coalesced status writes whose window passed (§5).
                            if let Err(e) = st.store.flush_deferred(blirp_core::now_ms()) {
                                tracing::warn!(error = %e, "queueing coalesced session updates failed");
                            }
                            // Events of a backfill after pairing (§10), in batches.
                            if let Err(e) = st.store.backfill_events(2000) {
                                tracing::warn!(error = %e, "queueing events for replication failed");
                            }
                            st.keep_awake.update(
                                st.terminals.all().len(),
                                st.config().keep_awake(),
                                std::time::Instant::now(),
                            );
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
            let service = crate::ingest::IngestService::start(Arc::new(engine));
            // Hooks reporting a transcript path get it ingested right away.
            state.set_ingest_trigger(service.trigger());
            service
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
        let state = self.state.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || {
            state.keep_awake.update(0, false, std::time::Instant::now());
        })
        .await
        {
            tracing::error!(error = %e, "releasing sleep prevention failed");
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
    tokio::select! {
        () = shutdown_signal() => tracing::info!("shutting down (signal)"),
        () = daemon.state.stop_requested.notified() => tracing::info!("shutting down (requested over the API)"),
    }
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
    // It becomes the daemon's working directory, so it must exist first.
    paths.ensure_dirs()?;
    let mut args = vec!["daemon".to_string()];
    if let Some(p) = port {
        args.extend(["--port".to_string(), p.to_string()]);
    }
    let mut child =
        spawn_background(&exe, &args, paths.home()).context("spawn background daemon")?;
    let pid = child.pid;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.exited()? {
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

/// The background daemon `--detach` started, watched only until it is up.
struct Background {
    pid: u32,
    #[cfg(unix)]
    child: std::process::Child,
    #[cfg(windows)]
    process: std::os::windows::io::OwnedHandle,
}

impl Background {
    /// `Some(exit status)` once the process has ended.
    #[cfg(unix)]
    fn exited(&mut self) -> std::io::Result<Option<String>> {
        Ok(self.child.try_wait()?.map(|s| s.to_string()))
    }

    /// `Some(exit status)` once the process has ended.
    #[cfg(windows)]
    #[allow(unsafe_code)]
    fn exited(&mut self) -> std::io::Result<Option<String>> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
        let h = self.process.as_raw_handle();
        // SAFETY: `h` is our open process handle (owned by `self`); a zero
        // timeout only polls, and the exit code is written to a local.
        unsafe {
            if WaitForSingleObject(h, 0) != WAIT_OBJECT_0 {
                return Ok(None);
            }
            let mut code = 0u32;
            if GetExitCodeProcess(h, &mut code) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Some(format!("exit code {code}")))
        }
    }
}

/// Start `exe args` detached from the caller: its own session / process
/// group, no console, `dir` as working directory (never the caller's
/// folder, which it would keep in use), `BLIRP_HOME` set, no std streams.
#[cfg(unix)]
fn spawn_background(
    exe: &std::path::Path,
    args: &[String],
    dir: &std::path::Path,
) -> std::io::Result<Background> {
    use std::os::unix::process::CommandExt;
    let mut cmd = blirp_core::process::command(exe);
    cmd.args(args)
        .current_dir(dir)
        .env(blirp_core::paths::HOME_ENV, dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
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
    let child = cmd.spawn()?;
    Ok(Background {
        pid: child.id(),
        child,
    })
}

/// Windows: `CreateProcessW` with `bInheritHandles = FALSE` and no std
/// handles. `std::process::Command` always lets the child inherit every
/// inheritable handle of this process, whatever it was handed by its own
/// caller (pipes it reads to EOF, log files); the daemon would hold them
/// for its lifetime and such a caller would hang.
#[cfg(windows)]
#[allow(unsafe_code)]
fn spawn_background(
    exe: &std::path::Path,
    args: &[String],
    dir: &std::path::Path,
) -> std::io::Result<Background> {
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        DETACHED_PROCESS, PROCESS_INFORMATION, STARTUPINFOW,
    };
    let wide = |s: &OsStr| -> Vec<u16> { s.encode_wide().chain([0]).collect() };
    // Paths cannot contain `"`; the arguments are plain tokens.
    let mut line = OsString::from("\"");
    line.push(exe);
    line.push("\"");
    for a in args {
        line.push(" ");
        line.push(a);
    }
    let mut line = wide(&line);
    // The caller's environment plus BLIRP_HOME, sorted like Windows keeps it.
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| !k.eq_ignore_ascii_case(blirp_core::paths::HOME_ENV))
        .collect();
    vars.push((
        blirp_core::paths::HOME_ENV.into(),
        dir.as_os_str().to_owned(),
    ));
    vars.sort_by_key(|(k, _)| k.to_ascii_uppercase());
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in &vars {
        block.extend(k.encode_wide());
        block.push(u16::from(b'='));
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    let dir_w = wide(dir.as_os_str());
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..STARTUPINFOW::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: every pointer is to a live, NUL-terminated buffer owned by
    // this frame (`line` is mutable as CreateProcessW requires); `si` is
    // initialized with its size and no std handles; on success the two
    // returned handles are owned here (the thread handle is closed, the
    // process handle moves into an OwnedHandle).
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            DETACHED_PROCESS
                | CREATE_NEW_PROCESS_GROUP
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT,
            block.as_ptr().cast(),
            dir_w.as_ptr(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both handles were just returned by a successful CreateProcessW
    // and are closed / owned exactly once.
    let process = unsafe {
        CloseHandle(pi.hThread);
        std::os::windows::io::OwnedHandle::from_raw_handle(pi.hProcess)
    };
    Ok(Background {
        pid: pi.dwProcessId,
        process,
    })
}

/// Tracing to `logs/blirpd.<date>.log` (daily, keep 7) and stderr.
pub fn init_logging(paths: &Paths) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    use crate::log_limit::{LogLimit, WithSuppressedCount};
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
    // The log file always; stderr only for a terminal. Under a service
    // manager stderr is captured to a file of its own (launchd.log, the
    // journal), which would duplicate every line, with color codes.
    use std::io::IsTerminal as _;
    let console = std::io::stderr().is_terminal().then(|| {
        fmt::layer()
            .with_writer(std::io::stderr)
            .event_format(WithSuppressedCount(fmt::format()))
    });
    tracing_subscriber::registry()
        .with(filter)
        // Lines that repeat at steady state (mDNS without the macOS Local
        // Network permission, retry loops): once per 10 min with a count.
        .with(LogLimit::default())
        .with(
            fmt::layer()
                .with_writer(file)
                .with_ansi(false)
                .event_format(WithSuppressedCount(fmt::format())),
        )
        .with(console)
        .try_init()
        .context("install log subscriber")?;
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_port_owners() {
        let netstat = "\r\nActive Connections\r\n\r\n  Proto  Local Address          Foreign Address        State           PID\r\n\
            \x20 TCP    127.0.0.1:47770        127.0.0.1:50000        ESTABLISHED     11\r\n\
            \x20 TCP    0.0.0.0:4777           0.0.0.0:0              LISTENING       12\r\n\
            \x20 TCP    127.0.0.1:47770        0.0.0.0:0              ABHÖREN         4242\r\n";
        assert_eq!(netstat_listener(netstat, 47770), Some(4242));
        assert_eq!(netstat_listener(netstat, 4777), Some(12));
        assert_eq!(netstat_listener(netstat, 1), None);
        assert_eq!(
            tasklist_name("\"node.exe\",\"4242\",\"Console\",\"1\",\"50,000 K\"\r\n").as_deref(),
            Some("node.exe")
        );
        assert_eq!(
            tasklist_name("INFO: No tasks are running which match the specified criteria.\r\n"),
            None
        );
        assert_eq!(
            lsof_owner("p4242\ncnode\nf20\n").as_deref(),
            Some("node (pid 4242)")
        );
        assert_eq!(lsof_owner(""), None);
        assert_eq!(
            ss_owner("LISTEN 0 511 127.0.0.1:47770 0.0.0.0:* users:((\"node\",pid=4242,fd=20))\n")
                .as_deref(),
            Some("node (pid 4242)")
        );
        assert_eq!(ss_owner("LISTEN 0 511 127.0.0.1:47770 0.0.0.0:*\n"), None);
    }
}
