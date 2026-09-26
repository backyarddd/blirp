//! blirp desktop shell (§15): a thin Tauri window around the web UI served by
//! the daemon. It starts the daemon (bundled `blirp` sidecar) when needed,
//! logs the window in via `/#token=` (the SPA keeps the token in its own
//! origin's storage), forwards `blirp://join/...` deep links and keeps a
//! tray icon. Updates are `blirp update` (the web UI's
//! Settings > About says when one is available); the app has no updater.
//!
//! The remote UI gets no Tauri IPC: only the bundled loading page may call
//! the three commands below (see `capabilities/main.json`).

mod daemon;

use blirp_core::paths::{Paths, RuntimeInfo};
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
    /// Runtime token the window was last signed in with. Every daemon start
    /// issues a new one, so a different token in runtime.json means the SPA's
    /// stored token is dead (401, "Sign in required").
    token: Mutex<Option<String>>,
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
            Ok(info) => sign_in(&app, &shell, &info),
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

/// Where to sign the window in to `info`'s daemon: the SPA route it shows
/// (kept across a daemon restart), else the start page. The SPA takes the
/// token out of the fragment and keeps the rest of the URL.
fn login_url(info: &RuntimeInfo, route: Option<&Url>) -> String {
    let (path, query) = route.map_or(("/", None), |u| (u.path(), u.query()));
    let query = query.map(|q| format!("?{q}")).unwrap_or_default();
    format!("{}{path}{query}#token={}", info.base_url(), info.token)
}

/// Whether the window, signed in to `origin` with `token`, is signed in to
/// the daemon `info` describes.
fn signed_in_to(origin: Option<&str>, token: Option<&str>, info: &RuntimeInfo) -> bool {
    origin == Some(info.base_url().as_str()) && token == Some(info.token.as_str())
}

/// Log the window in to a healthy daemon, keeping the SPA route it shows.
fn sign_in(app: &AppHandle, shell: &Shell, info: &RuntimeInfo) {
    let route = app
        .get_webview_window(MAIN)
        .and_then(|w| w.url().ok())
        .filter(|u| shell.is_daemon_url(u));
    let url = login_url(info, route.as_ref());
    // The origin first: navigation to it is only allowed once it is known.
    let previous = lock(&shell.origin).replace(info.base_url());
    if navigate(app, &url) {
        *lock(&shell.token) = Some(info.token.clone());
        shell.set_phase(Phase::Ready);
        return;
    }
    // Not signed in: the next focus, reopen or page load tries again. The
    // window still shows the old origin (or the loading page), which must
    // stay allowed.
    *lock(&shell.token) = None;
    *lock(&shell.origin) = previous.clone();
    if previous.is_none() {
        shell.set_phase(Phase::Failed {
            message: "the blirp UI could not be opened in this window; see the desktop log".into(),
        });
    }
}

/// Sign the window in again when the daemon restarted since it was signed in
/// (`blirp update`, `blirp stop` and a new start, a crash the autostart
/// service recovered): the new daemon is healthy but has a new token, so the
/// SPA would stay on "Sign in required". Runs on focus, reopen and page loads
/// (the SPA's "Try again" reloads), so it reads runtime.json first and only
/// asks the daemon when that changed.
async fn refresh_sign_in(app: &AppHandle, shell: &Shell) {
    if shell.starting.load(Ordering::SeqCst) {
        return;
    }
    let on_ui = app
        .get_webview_window(MAIN)
        .and_then(|w| w.url().ok())
        .is_some_and(|u| shell.is_daemon_url(&u));
    // The loading page belongs to `start`, which signs in when it is done,
    // unless that sign-in failed (not signed in, nothing starting): retry.
    if !on_ui && lock(&shell.token).is_some() {
        return;
    }
    let current = |info: &RuntimeInfo| {
        signed_in_to(
            lock(&shell.origin).as_deref(),
            lock(&shell.token).as_deref(),
            info,
        )
    };
    match RuntimeInfo::read(&shell.paths) {
        Ok(Some(info)) if !current(&info) => {}
        // Same daemon, or none (stopped: the SPA says it cannot reach it).
        Ok(_) => return,
        Err(e) => {
            tracing::warn!(error = %e, "unreadable runtime.json");
            return;
        }
    }
    // runtime.json is written before the daemon answers; wait for health.
    let Some(info) = daemon::probe(&shell.paths).await else {
        return;
    };
    if !current(&info) && !shell.starting.load(Ordering::SeqCst) {
        tracing::info!(
            port = info.port,
            "daemon restarted; signing the window in again"
        );
        sign_in(app, shell, &info);
    }
}

/// Returns whether the main window started loading `url`.
fn navigate(app: &AppHandle, url: &str) -> bool {
    let Some(win) = app.get_webview_window(MAIN) else {
        return false;
    };
    match Url::parse(url) {
        Ok(u) => win
            .navigate(u)
            .inspect_err(|e| tracing::error!(error = %e, "navigate main window"))
            .is_ok(),
        Err(e) => {
            tracing::error!(error = %e, "invalid navigation url");
            false
        }
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

/// Tray "Open" / reopen: show the window, restart the daemon if it died and
/// sign in again if it restarted.
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
        } else {
            refresh_sign_in(&app, &shell).await;
        }
    });
}

// -------------------------------------------------------------- deep links

/// `blirp://join/<ticket>#<code>` -> the SPA's pairing screen, prefilled.
/// Any web page can open such a link, so the SPA never pairs from it on its
/// own: the user confirms the hub's id and its rights first.
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
            // Already logged in: the UI holds the token, go straight there.
            // A failure is logged; the join form is only a prefill.
            Some(o) if on_ui => {
                navigate(app, &format!("{o}{route}"));
            }
            _ => *lock(&shell.pending_route) = Some(route),
        }
    }
}

fn on_page_loaded(win: &WebviewWindow, url: &Url) {
    let shell = win.state::<Arc<Shell>>().inner().clone();
    if !shell.is_daemon_url(url) {
        return;
    }
    tracing::info!(path = url.path(), "ui loaded");
    let app = win.app_handle().clone();
    let check = shell.clone();
    tauri::async_runtime::spawn(async move { refresh_sign_in(&app, &check).await });
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
        token: Mutex::new(None),
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

            // macOS registers blirp:// from Info.plist. Linux (AppImage) and
            // Windows (the install script's portable app has no installer to
            // do it) register at runtime, pointing at this executable.
            #[cfg(any(target_os = "linux", windows))]
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
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != MAIN {
                return;
            }
            match event {
                // Closing the window hides it; the daemon, its sessions and
                // the tray stay. Tray "Quit" ends everything.
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                // E.g. back from the terminal that ran `blirp update`.
                tauri::WindowEvent::Focused(true) => {
                    let app = window.app_handle().clone();
                    tauri::async_runtime::spawn(async move {
                        let shell = app.state::<Arc<Shell>>().inner().clone();
                        refresh_sign_in(&app, &shell).await;
                    });
                }
                _ => {}
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

    fn runtime(port: u16, token: &str) -> RuntimeInfo {
        RuntimeInfo {
            pid: 1,
            port,
            token: token.into(),
            version: "0.1.0".into(),
            started_at: 0,
        }
    }

    #[test]
    fn restarted_daemon_needs_a_new_sign_in() {
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let o = Some("http://127.0.0.1:47770");
        assert!(signed_in_to(o, Some(&a), &runtime(47770, &a)));
        // Restart: same port, new token.
        assert!(!signed_in_to(o, Some(&a), &runtime(47770, &b)));
        // Restart on another port.
        assert!(!signed_in_to(o, Some(&a), &runtime(47771, &a)));
        // Never signed in.
        assert!(!signed_in_to(None, None, &runtime(47770, &a)));
    }

    #[test]
    fn sign_in_keeps_the_route() {
        let t = "c".repeat(64);
        let info = runtime(47771, &t);
        assert_eq!(
            login_url(&info, None),
            format!("http://127.0.0.1:47771/#token={t}")
        );
        let route = Url::parse("http://127.0.0.1:47770/settings/sync?join=x#y").unwrap();
        assert_eq!(
            login_url(&info, Some(&route)),
            format!("http://127.0.0.1:47771/settings/sync?join=x#token={t}")
        );
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
