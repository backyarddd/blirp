//! blirp desktop shell (§15): a thin Tauri window around the web UI served by
//! the daemon. It starts the daemon (bundled `blirp` sidecar) when needed,
//! logs the window in via `/auth?token=`, forwards `blirp://join/...` deep
//! links, keeps a tray icon, and installs signed updates.
//!
//! The remote UI gets no Tauri IPC: only the bundled loading page may call
//! the three commands below (see `capabilities/main.json`).

mod daemon;

use blirp_core::paths::Paths;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::{NewWindowResponse, PageLoadEvent};
use tauri::{AppHandle, Manager, State, Url, WebviewWindow, WebviewWindowBuilder, Wry};
use tauri_plugin_deep_link::DeepLinkExt as _;
use tauri_plugin_dialog::{DialogExt as _, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt as _;
use tauri_plugin_updater::UpdaterExt as _;

const MAIN: &str = "main";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Plain data; a panic elsewhere cannot leave it half-written.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum Phase {
    Starting,
    Ready,
    Failed { message: String },
}

struct Shell {
    paths: Paths,
    phase: Mutex<Phase>,
    starting: AtomicBool,
    /// Daemon origin (`http://127.0.0.1:<port>`) once known.
    origin: Mutex<Option<String>>,
    /// The bundled loading page, to return to when the daemon must restart.
    local_url: Mutex<Option<Url>>,
    /// SPA route from a deep link, opened once the UI is logged in.
    pending_route: Mutex<Option<String>>,
    tray_status: Mutex<Option<MenuItem<Wry>>>,
}

impl Shell {
    fn set_phase(&self, phase: Phase) {
        let text = match &phase {
            Phase::Starting => "Daemon: starting…".to_string(),
            Phase::Ready => match lock(&self.origin).as_deref() {
                Some(o) => format!("Daemon: running at {o}"),
                None => "Daemon: running".to_string(),
            },
            Phase::Failed { .. } => "Daemon: failed to start".to_string(),
        };
        if let Some(item) = lock(&self.tray_status).as_ref()
            && let Err(e) = item.set_text(text)
        {
            tracing::warn!(error = %e, "update tray status");
        }
        *lock(&self.phase) = phase;
    }

    fn is_daemon_url(&self, url: &Url) -> bool {
        lock(&self.origin)
            .as_deref()
            .is_some_and(|o| url.origin().ascii_serialization() == o)
    }
}

fn is_local_page(url: &Url) -> bool {
    // Bundled assets: tauri://localhost (macOS/Linux), http://tauri.localhost (Windows).
    url.scheme() == "tauri" || url.host_str() == Some("tauri.localhost")
}

// ----------------------------------------------------------------- startup

#[derive(Serialize)]
struct StartupView {
    #[serde(flatten)]
    phase: Phase,
    log_dir: String,
}

#[tauri::command]
fn startup_state(shell: State<'_, Arc<Shell>>) -> StartupView {
    StartupView {
        phase: lock(&shell.phase).clone(),
        log_dir: shell.paths.logs_dir().display().to_string(),
    }
}

#[tauri::command]
fn retry(app: AppHandle) {
    start(app);
}

#[tauri::command]
fn open_logs(app: AppHandle, shell: State<'_, Arc<Shell>>) -> Result<(), String> {
    let dir = shell.paths.logs_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    app.opener()
        .open_path(dir.display().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Ensure the daemon runs, then log the window in. No-op while a start is
/// already in flight.
fn start(app: AppHandle) {
    let shell = app.state::<Arc<Shell>>().inner().clone();
    if shell.starting.swap(true, Ordering::SeqCst) {
        return;
    }
    shell.set_phase(Phase::Starting);
    tauri::async_runtime::spawn(async move {
        let result = daemon::ensure(&shell.paths).await;
        match result {
            Ok(info) => {
                *lock(&shell.origin) = Some(info.base_url());
                shell.set_phase(Phase::Ready);
                let url = format!("{}/auth?token={}", info.base_url(), info.token);
                navigate(&app, &url);
            }
            Err(e) => {
                tracing::error!(error = format!("{e:#}"), "daemon start failed");
                shell.set_phase(Phase::Failed {
                    message: format!("{e:#}"),
                });
                // The loading page polls the phase and shows the error.
                show_local_page(&app, &shell);
            }
        }
        shell.starting.store(false, Ordering::SeqCst);
    });
}

fn navigate(app: &AppHandle, url: &str) {
    let Some(win) = app.get_webview_window(MAIN) else {
        return;
    };
    match Url::parse(url) {
        Ok(u) => {
            if let Err(e) = win.navigate(u) {
                tracing::error!(error = %e, "navigate main window");
            }
        }
        Err(e) => tracing::error!(error = %e, "invalid navigation url"),
    }
}

fn show_local_page(app: &AppHandle, shell: &Shell) {
    let Some(win) = app.get_webview_window(MAIN) else {
        return;
    };
    let on_local = win.url().is_ok_and(|u| is_local_page(&u));
    if !on_local && let Some(url) = lock(&shell.local_url).clone() {
        let _ = win
            .navigate(url)
            .inspect_err(|e| tracing::error!(error = %e, "show loading page"));
    }
}

fn show_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(MAIN) {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// Tray "Open" / reopen: show the window and restart the daemon if it died.
fn reopen(app: &AppHandle) {
    show_window(app);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let shell = app.state::<Arc<Shell>>().inner().clone();
        let on_local = app
            .get_webview_window(MAIN)
            .and_then(|w| w.url().ok())
            .is_some_and(|u| is_local_page(&u));
        // The daemon died, or it is up again but the window still shows the
        // error page.
        if on_local || daemon::probe(&shell.paths).await.is_none() {
            show_local_page(&app, &shell);
            start(app);
        }
    });
}

// -------------------------------------------------------------- deep links

/// `blirp://join/<ticket>#<code>` -> the SPA's pairing screen, prefilled.
fn join_route(url: &Url) -> Option<String> {
    if url.scheme() != "blirp" || url.host_str() != Some("join") {
        return None;
    }
    let ticket = url.path().trim_start_matches('/');
    let valid = |s: &str, max: usize| {
        !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if !valid(ticket, 4096) {
        return None;
    }
    let mut route = Url::parse("http://x/settings/sync").ok()?;
    route.query_pairs_mut().append_pair("join", ticket);
    if let Some(code) = url.fragment().filter(|c| valid(c, 32)) {
        route.query_pairs_mut().append_pair("code", code);
    }
    Some(format!(
        "{}?{}",
        route.path(),
        route.query().unwrap_or_default()
    ))
}

fn handle_deep_links(app: &AppHandle, urls: Vec<Url>) {
    let shell = app.state::<Arc<Shell>>().inner().clone();
    for url in urls {
        let Some(route) = join_route(&url) else {
            tracing::warn!(scheme = url.scheme(), "ignoring unsupported deep link");
            continue;
        };
        show_window(app);
        let origin = lock(&shell.origin).clone();
        let on_ui = app
            .get_webview_window(MAIN)
            .and_then(|w| w.url().ok())
            .is_some_and(|u| shell.is_daemon_url(&u));
        match origin {
            // Already logged in: the session cookie is set, go straight there.
            Some(o) if on_ui => navigate(app, &format!("{o}{route}")),
            _ => *lock(&shell.pending_route) = Some(route),
        }
    }
}

fn on_page_loaded(win: &WebviewWindow, url: &Url) {
    let shell = win.state::<Arc<Shell>>().inner().clone();
    if !shell.is_daemon_url(url) || url.path() == "/auth" {
        return;
    }
    tracing::info!(path = url.path(), "ui loaded");
    let pending = lock(&shell.pending_route).take();
    if let (Some(route), Some(origin)) = (pending, lock(&shell.origin).clone()) {
        navigate(win.app_handle(), &format!("{origin}{route}"));
    }
}

// -------------------------------------------------------------------- quit

fn confirm_quit(app: &AppHandle) {
    let handle = app.clone();
    app.dialog()
        .message(
            "Quitting stops the blirp daemon and ends every running agent session.\n\n\
             To keep sessions running, just close the window instead.",
        )
        .title("Quit blirp?")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Quit blirp".into(),
            "Cancel".into(),
        ))
        .show(move |quit| {
            if quit {
                tauri::async_runtime::spawn(async move {
                    let shell = handle.state::<Arc<Shell>>().inner().clone();
                    if let Err(e) = daemon::stop(&shell.paths).await {
                        tracing::error!(error = format!("{e:#}"), "stop daemon");
                        handle
                            .dialog()
                            .message(format!("Could not stop the daemon: {e:#}"))
                            .title("blirp")
                            .kind(MessageDialogKind::Error)
                            .show(|_| {});
                        return;
                    }
                    handle.exit(0);
                });
            }
        });
}

// ----------------------------------------------------------------- updates

async fn check_for_update(app: AppHandle) -> anyhow::Result<()> {
    let Some(update) = app.updater()?.check().await? else {
        return Ok(());
    };
    tracing::info!(version = %update.version, "update available");
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .message(format!(
            "blirp {} is available (you have {}).\n\nInstalling stops the daemon and ends \
             running agent sessions; blirp restarts afterwards.",
            update.version, update.current_version
        ))
        .title("Update available")
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Install and restart".into(),
            "Later".into(),
        ))
        .show(move |ok| {
            let _ = tx.send(ok);
        });
    if !rx.await.unwrap_or(false) {
        return Ok(());
    }
    // The user asked for it: failures from here on are shown, not just logged.
    if let Err(e) = install_update(&app, update).await {
        tracing::error!(error = format!("{e:#}"), "update install failed");
        // The daemon may already be stopped; bring the UI back either way.
        start(app.clone());
        app.dialog()
            .message(format!("The update could not be installed: {e:#}"))
            .title("blirp")
            .kind(MessageDialogKind::Error)
            .show(|_| {});
    }
    Ok(())
}

async fn install_update(
    app: &AppHandle,
    update: tauri_plugin_updater::Update,
) -> anyhow::Result<()> {
    let bytes = update.download(|_, _| {}, || {}).await?;
    // The installer replaces the sidecar, which a running daemon keeps locked
    // on Windows; stop it first everywhere so the new version starts cleanly.
    let shell = app.state::<Arc<Shell>>().inner().clone();
    daemon::stop(&shell.paths).await?;
    update.install(bytes)?;
    app.restart();
}

// -------------------------------------------------------------------- tray

fn build_tray(app: &AppHandle, shell: &Shell) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open blirp", true, None::<&str>)?;
    let status = MenuItem::with_id(app, "status", "Daemon: starting…", false, None::<&str>)?;
    let close = MenuItem::with_id(
        app,
        "close",
        "Close window (sessions keep running)",
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(
        app,
        "quit",
        "Quit blirp (stop daemon and sessions)…",
        true,
        None::<&str>,
    )?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &status, &sep, &close, &quit])?;
    *lock(&shell.tray_status) = Some(status);

    let mut tray = TrayIconBuilder::with_id("blirp")
        .tooltip("blirp")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => reopen(app),
            "close" => {
                if let Some(w) = app.get_webview_window(MAIN) {
                    let _ = w.hide();
                }
            }
            "quit" => confirm_quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                reopen(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

// --------------------------------------------------------------------- run

fn init_logging(paths: &Paths) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("desktop")
        .filename_suffix("log")
        .max_log_files(7)
        .build(paths.logs_dir())
        .inspect_err(|e| eprintln!("blirp: cannot open desktop log: {e}"))
        .ok()?;
    let (file, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_env("BLIRP_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file)
                .with_ansi(false),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()
        .inspect_err(|e| eprintln!("blirp: cannot install logger: {e}"))
        .ok()?;
    Some(guard)
}

pub fn run() -> anyhow::Result<()> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let _log_guard = init_logging(&paths);

    let shell = Arc::new(Shell {
        paths,
        phase: Mutex::new(Phase::Starting),
        starting: AtomicBool::new(false),
        origin: Mutex::new(None),
        local_url: Mutex::new(None),
        pending_route: Mutex::new(None),
        tray_status: Mutex::new(None),
    });

    let app = tauri::Builder::default()
        // Must be first: a second launch (or a deep link) focuses this instance.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            reopen(app);
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::all()
                        - tauri_plugin_window_state::StateFlags::VISIBLE,
                )
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(shell.clone())
        .invoke_handler(tauri::generate_handler![startup_state, retry, open_logs])
        .setup(move |app| {
            let handle = app.handle().clone();
            let config = app
                .config()
                .app
                .windows
                .iter()
                .find(|w| w.label == MAIN)
                .cloned()
                .ok_or("tauri.conf.json has no main window")?;
            let nav_shell = shell.clone();
            let nav_app = handle.clone();
            let popup_app = handle.clone();
            let win = WebviewWindowBuilder::from_config(&handle, &config)?
                // Only the loading page and the daemon's UI render in the
                // window; every other link opens in the default browser.
                .on_navigation(move |url| {
                    if is_local_page(url) || nav_shell.is_daemon_url(url) {
                        return true;
                    }
                    open_external(&nav_app, url);
                    false
                })
                .on_new_window(move |url, _features| {
                    open_external(&popup_app, &url);
                    NewWindowResponse::Deny
                })
                .on_page_load(|win, payload| {
                    if payload.event() == PageLoadEvent::Finished {
                        on_page_loaded(&win, payload.url());
                    }
                })
                .build()?;
            *lock(&shell.local_url) = win.url().ok();

            build_tray(&handle, &shell)?;

            // Installers register blirp:// on Windows and macOS; AppImages and
            // Windows dev builds have to do it at runtime.
            #[cfg(any(target_os = "linux", all(windows, debug_assertions)))]
            if let Err(e) = app.deep_link().register_all() {
                tracing::warn!(error = %e, "register blirp:// links");
            }
            let dl_app = handle.clone();
            app.deep_link().on_open_url(move |event| {
                handle_deep_links(&dl_app, event.urls());
            });
            match app.deep_link().get_current() {
                Ok(Some(urls)) => handle_deep_links(&handle, urls),
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "read launch deep link"),
            }

            start(handle.clone());

            if !cfg!(debug_assertions) {
                let up = handle.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = check_for_update(up).await {
                        tracing::warn!(error = format!("{e:#}"), "update check failed");
                    }
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window hides it; the daemon, its sessions and the
            // tray stay. Tray "Quit" ends everything.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && window.label() == MAIN
            {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())?;

    // The window is only ever hidden, so an exit request comes from Cmd+Q,
    // logout or tray Quit; let it through (the daemon keeps running unless
    // Quit stopped it).
    app.run(|_app, _event| {
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = _event {
            reopen(_app);
        }
    });
    Ok(())
}

fn open_external(app: &AppHandle, url: &Url) {
    if !matches!(url.scheme(), "http" | "https" | "mailto") {
        tracing::warn!(scheme = url.scheme(), "blocked navigation");
        return;
    }
    if let Err(e) = app.opener().open_url(url.as_str(), None::<&str>) {
        tracing::error!(error = %e, "open link in browser");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_links() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            join_route(&u("blirp://join/blirp1-abcDEF234#K7QD-M2XP")).as_deref(),
            Some("/settings/sync?join=blirp1-abcDEF234&code=K7QD-M2XP")
        );
        assert_eq!(
            join_route(&u("blirp://join/blirp1-abc")).as_deref(),
            Some("/settings/sync?join=blirp1-abc")
        );
        // Bad code is dropped, the ticket still prefills.
        assert_eq!(
            join_route(&u("blirp://join/blirp1-abc#a%20b")).as_deref(),
            Some("/settings/sync?join=blirp1-abc")
        );
        assert_eq!(join_route(&u("blirp://join/")), None);
        assert_eq!(join_route(&u("blirp://join/a/b")), None);
        assert_eq!(join_route(&u("blirp://evil/x")), None);
        assert_eq!(join_route(&u("https://join/x")), None);
    }

    #[test]
    fn local_pages() {
        assert!(is_local_page(
            &Url::parse("tauri://localhost/index.html").unwrap()
        ));
        assert!(is_local_page(
            &Url::parse("http://tauri.localhost/index.html").unwrap()
        ));
        assert!(!is_local_page(
            &Url::parse("http://127.0.0.1:47770/").unwrap()
        ));
    }
}
