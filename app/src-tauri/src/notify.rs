//! Native OS notifications for the daemon's web UI. WebView2 and WKWebView
//! do not deliver the Web Notification API to the OS, so the SPA calls the
//! `notify` command instead (capability `daemon-ui`, the only IPC the remote
//! origin gets). Toasts go through tauri-plugin-notification (notify-rust).
//!
//! Windows only shows a toast whose AppUserModelID is registered. The plugin
//! sends every toast as the bundle identifier, and neither the script
//! install's Start Menu shortcut nor a portable copy registers it, so Windows
//! dropped them silently. `register` writes the ID under
//! `HKCU\Software\Classes\AppUserModelId` at startup (what unpackaged apps do);
//! `blirp uninstall` removes it again.

use crate::{MAIN, Shell, lock};
use serde::Serialize;
use std::sync::Arc;
use tauri::{AppHandle, Manager, State, UserAttentionType, Webview};
use tauri_plugin_notification::NotificationExt as _;

/// Longest title/body passed on; the OS truncates long text anyway.
const MAX_TITLE: usize = 120;
const MAX_BODY: usize = 400;

#[derive(Serialize)]
pub(crate) struct NotifyReport {
    /// Which OS service shows it, for Settings > Notifications' diagnostics.
    backend: &'static str,
    /// Why it may not appear (e.g. the Windows registration failed).
    problem: Option<String>,
}

const BACKEND: &str = if cfg!(windows) {
    "Windows notifications"
} else if cfg!(target_os = "macos") {
    "macOS Notification Center"
} else {
    "the desktop notification daemon (D-Bus)"
};

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Show an OS notification and, when the window is not focused, flash the
/// taskbar button / bounce the dock icon (seen even under Focus Assist).
#[tauri::command]
pub(crate) fn notify(
    app: AppHandle,
    webview: Webview,
    shell: State<'_, Arc<Shell>>,
    title: String,
    body: String,
) -> Result<NotifyReport, String> {
    // The capability already limits this to http://127.0.0.1:*; only the
    // daemon's own UI may use it.
    if !webview.url().is_ok_and(|u| shell.is_daemon_url(&u)) {
        return Err("not allowed from this page".into());
    }
    app.notification()
        .builder()
        .title(clip(&title, MAX_TITLE))
        .body(clip(&body, MAX_BODY))
        .show()
        .map_err(|e| e.to_string())?;
    if let Some(win) = app.get_webview_window(MAIN)
        && !win.is_focused().unwrap_or(false)
        && let Err(e) = win.request_user_attention(Some(UserAttentionType::Informational))
    {
        tracing::warn!(error = %e, "request user attention");
    }
    Ok(NotifyReport {
        backend: BACKEND,
        problem: lock(&shell.notify_problem).clone(),
    })
}

/// Register the bundle identifier as a toast sender for this user (display
/// name and icon). Returns why it failed, if it did.
#[cfg(windows)]
pub(crate) fn register(app: &AppHandle, paths: &blirp_core::paths::Paths) -> Option<String> {
    let key = format!(
        r"HKCU\Software\Classes\AppUserModelId\{}",
        app.config().identifier
    );
    let icon = paths.home().join("desktop-icon.png");
    let mut values = vec![("DisplayName", "blirp".to_string())];
    match std::fs::write(&icon, include_bytes!("../icons/128x128.png")) {
        Ok(()) => values.push(("IconUri", icon.display().to_string())),
        // Toasts still show, with a generic icon.
        Err(e) => tracing::warn!(error = %e, path = %icon.display(), "write notification icon"),
    }
    let reg = std::env::var_os("SystemRoot").map_or_else(
        || std::path::PathBuf::from("reg.exe"),
        |r| std::path::Path::new(&r).join(r"System32\reg.exe"),
    );
    for (name, value) in values {
        let out = blirp_core::process::command(&reg)
            .args(["add", &key, "/v", name, "/t", "REG_SZ", "/d", &value, "/f"])
            .stdin(std::process::Stdio::null())
            .output();
        let problem = match out {
            Ok(o) if o.status.success() => continue,
            Ok(o) => format!(
                "reg add failed ({}): {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => format!("cannot run reg.exe: {e}"),
        };
        tracing::error!(problem, "register notification sender");
        return Some(format!(
            "blirp could not register itself as a notification sender ({problem}); \
             Windows may drop its notifications"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::Url;
    use tauri::utils::acl::RemoteUrlPattern;

    #[test]
    fn clips_on_char_boundaries() {
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcd", 3), "abc…");
        assert_eq!(clip("ééé", 2), "éé…");
    }

    /// The daemon's UI (any loopback port) gets the capability; nothing else.
    #[test]
    fn daemon_ui_capability_matches_only_loopback_http() {
        let cap: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/daemon-ui.json")).unwrap();
        let urls = cap["remote"]["urls"].as_array().unwrap();
        let patterns: Vec<RemoteUrlPattern> = urls
            .iter()
            .map(|u| u.as_str().unwrap().parse().unwrap())
            .collect();
        let allowed = |s: &str| {
            let u = Url::parse(s).unwrap();
            patterns.iter().any(|p| p.test(&u))
        };
        assert!(allowed("http://127.0.0.1:47770/"));
        assert!(allowed("http://127.0.0.1:51234/sessions/abc?x=1#y"));
        assert!(!allowed("https://127.0.0.1:47770/"));
        assert!(!allowed("http://localhost:47770/"));
        assert!(!allowed("http://192.168.0.5:47770/"));
        assert!(!allowed("http://127.0.0.1.evil.example/"));
        assert_eq!(
            cap["permissions"],
            serde_json::json!(["allow-notify"]),
            "the remote UI gets nothing but notify"
        );
    }
}
