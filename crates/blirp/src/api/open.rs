//! `POST /api/sessions/:id/open {target}` and `POST /api/projects/:id/open
//! {target, path}`: show a session's or project's folder in the OS file
//! manager or the user's editor. The program is spawned detached (no
//! console window); the request returns once it started and did not fail
//! right away. Terminal editors are never used: the daemon has no terminal
//! to show them in.

use super::{Admin, ApiError, ApiJson, ApiPath, ApiResult, blocking};
use crate::agents::PlatformCommand;
use crate::state::SharedState;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use blirp_core::model::{OpenProject, OpenSession, OpenTarget};
use blirp_core::process;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/sessions/{id}/open", post(open))
        .route("/api/projects/{id}/open", post(open_project))
}

/// Only this machine's folders of the project, or its blirp workspace:
/// the path is never an arbitrary folder chosen by the caller.
async fn open_project(
    State(s): State<SharedState>,
    _: Admin,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<OpenProject>,
) -> ApiResult<StatusCode> {
    let st = s.clone();
    blocking(move || {
        let project = st.store.live_project(&id)?;
        let wanted = blirp_core::paths::path_key(std::path::Path::new(&body.path));
        let mut allowed = st.store.local_roots(&id, &st.machine.id)?;
        if !project.chats
            && let Ok(ws) = st.paths.workspace_dir(&id)
        {
            allowed.push(ws);
        }
        let dir = allowed
            .into_iter()
            .find(|p| blirp_core::paths::path_key(p) == wanted)
            .ok_or_else(|| {
                ApiError::bad_request("path is not a folder of this project on this machine")
            })?;
        open_dir(dir, body.target)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn open(
    State(s): State<SharedState>,
    _: Admin,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<OpenSession>,
) -> ApiResult<StatusCode> {
    // Opens a window on this machine's desktop: its own app or CLI only.
    let (store, machine) = (s.store.clone(), s.machine.id.clone());
    blocking(move || {
        let session = store
            .get_session(&id)?
            .ok_or_else(|| ApiError::not_found("session"))?;
        if session.machine_id != machine {
            return Err(ApiError::bad_request(
                "the session's folder is on another machine",
            ));
        }
        open_dir(PathBuf::from(&session.cwd), body.target)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Show `dir` with the file manager or the editor (404 `folder_missing`
/// when it no longer exists).
fn open_dir(dir: PathBuf, target: OpenTarget) -> ApiResult<()> {
    if !dir.is_dir() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "folder_missing",
            format!("folder {} does not exist", dir.display()),
        ));
    }
    let (program, mut args) = match target {
        OpenTarget::Folder => file_manager(),
        OpenTarget::Editor => editor().unwrap_or_else(file_manager),
    };
    args.push(dir.into_os_string());
    // Shims (`code.cmd`) are wrapped like agent launches, with the folder
    // among the escaped arguments.
    let cmd =
        crate::agents::wrap_for_platform(std::path::Path::new(&program), args).map_err(|e| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "open_failed",
                e.to_string(),
            )
        })?;
    spawn_detached(cmd)
}

/// The platform's "show this folder" command.
fn file_manager() -> (OsString, Vec<OsString>) {
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    (program.into(), Vec::new())
}

/// Graphical editors looked for on PATH when `$VISUAL` / `$EDITOR` name none.
const GUI_EDITORS: &[&str] = &["code", "cursor", "codium", "zed", "subl"];

/// `$VISUAL`, then `$EDITOR`, then a known graphical editor, whichever
/// resolves to a program and is not a terminal editor.
fn editor() -> Option<(OsString, Vec<OsString>)> {
    let from_env = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find_map(|v| {
            let (program, args) = split_command(&v)?;
            if is_terminal_editor(&program, &args) {
                return None;
            }
            Some((process::which(&program)?, args))
        });
    let (path, args) = from_env.or_else(|| {
        GUI_EDITORS
            .iter()
            .find_map(|e| Some((process::which(e)?, Vec::new())))
    })?;
    let args = args.into_iter().map(OsString::from).collect();
    Some((path.into_os_string(), args))
}

/// Editors that need a terminal (vim, nano, emacs -nw, ...). Started by the
/// daemon they would run invisibly, with no terminal attached.
fn is_terminal_editor(program: &str, args: &[String]) -> bool {
    const TERMINAL: &[&str] = &[
        "vi",
        "vim",
        "nvim",
        "lvim",
        "view",
        "vim.basic",
        "vim.tiny",
        "nano",
        "rnano",
        "pico",
        "micro",
        "hx",
        "helix",
        "kak",
        "joe",
        "jed",
        "ne",
        "ed",
        "ex",
        "mg",
        "mcedit",
        "zile",
        "jove",
        "amp",
    ];
    // Windows paths too, whatever this platform's separator.
    let base = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let base = base.to_ascii_lowercase();
    let name = base.strip_suffix(".exe").unwrap_or(&base);
    let no_window = args
        .iter()
        .any(|a| matches!(a.as_str(), "-nw" | "-t" | "--tty" | "--no-window-system"));
    TERMINAL.contains(&name) || (name.starts_with("emacs") && no_window)
}

/// Split an editor variable (`code -w`, `"C:\Program Files\x\x.exe" -n`) into
/// program and arguments. A value that is itself an existing file is taken
/// whole, so unquoted paths with spaces work.
fn split_command(v: &str) -> Option<(String, Vec<String>)> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    if std::path::Path::new(v).is_file() {
        return Some((v.to_string(), Vec::new()));
    }
    let (program, rest) = match v.strip_prefix('"') {
        Some(q) => {
            let end = q.find('"')?;
            (q[..end].to_string(), &q[end + 1..])
        }
        None => {
            let mut parts = v.splitn(2, char::is_whitespace);
            let program = parts.next()?.to_string();
            (program, parts.next().unwrap_or(""))
        }
    };
    let args = rest.split_whitespace().map(str::to_string).collect();
    Some((program, args))
}

/// How long an opener may take to fail before the request succeeds anyway
/// (`xdg-open` / `open` hand over and exit; editors keep running).
const EARLY_EXIT: Duration = Duration::from_millis(1500);

/// Start `command` and report it failing at once (nothing to open with, no
/// desktop session); a thread reaps it when it exits later.
fn spawn_detached(command: PlatformCommand) -> ApiResult<()> {
    let name = command.program.to_string_lossy().into_owned();
    let mut cmd = command.std_command();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| {
        tracing::warn!(program = %name, error = %e, "open: spawn failed");
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "open_failed",
            format!("could not start {name}: {e}"),
        )
    })?;
    // explorer.exe exits with 1 even when it opened the folder.
    let checked = !name.to_ascii_lowercase().ends_with("explorer.exe");
    let deadline = Instant::now() + EARLY_EXIT;
    while checked && Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                tracing::warn!(program = %name, %status, "open: program failed");
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "open_failed",
                    format!(
                        "{name} could not open the folder ({status}); this machine may have no \
                         desktop session or no program to open folders with"
                    ),
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                tracing::debug!(program = %name, error = %e, "open: wait failed");
                break;
            }
        }
    }
    std::thread::spawn(move || {
        if let Err(e) = child.wait() {
            tracing::debug!(program = %name, error = %e, "open: wait failed");
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_editors_are_recognized() {
        let t = |p: &str, a: &[&str]| {
            is_terminal_editor(p, &a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        assert!(t("vim", &[]));
        assert!(t("/usr/bin/nvim", &[]));
        assert!(t(r"C:\Tools\Nano.EXE", &[]));
        assert!(t("emacs", &["-nw"]));
        assert!(t("emacsclient", &["-t"]));
        assert!(!t("emacs", &[]));
        assert!(!t("gvim", &[]));
        assert!(!t("code", &["-w"]));
        assert!(!t(r"C:\Program Files\Notepad++\notepad++.exe", &[]));
    }

    #[test]
    fn editor_variables_split() {
        let s = |v: &str| split_command(v);
        assert_eq!(s("code -w"), Some(("code".into(), vec!["-w".into()])));
        assert_eq!(s("  vim  "), Some(("vim".into(), vec![])));
        assert_eq!(
            s(r#""C:\Program Files\Ed\ed.exe" -n --x"#),
            Some((
                r"C:\Program Files\Ed\ed.exe".into(),
                vec!["-n".into(), "--x".into()]
            ))
        );
        assert_eq!(s(""), None);
        assert_eq!(s(r#""unterminated"#), None);
        let dir = tempfile::tempdir().unwrap();
        let spaced = dir.path().join("my editor");
        std::fs::write(&spaced, "").unwrap();
        let whole = spaced.to_string_lossy().into_owned();
        assert_eq!(s(&whole), Some((whole.clone(), vec![])));
    }
}
