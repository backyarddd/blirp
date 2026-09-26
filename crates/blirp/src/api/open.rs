//! `POST /api/sessions/:id/open {target}`: show a session's folder in the OS
//! file manager or the user's editor. The program is spawned detached; the
//! request returns as soon as it started.

use super::{Admin, ApiError, ApiJson, ApiPath, ApiResult, blocking};
use crate::agents::PlatformCommand;
use crate::state::SharedState;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use blirp_core::model::{OpenSession, OpenTarget};
use blirp_core::process;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;

pub fn routes() -> Router<SharedState> {
    Router::new().route("/api/sessions/{id}/open", post(open))
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
        let dir = PathBuf::from(&session.cwd);
        if !dir.is_dir() {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "folder_missing",
                format!("session folder {} no longer exists", session.cwd),
            ));
        }
        let (program, mut args) = match body.target {
            OpenTarget::Folder => file_manager(),
            OpenTarget::Editor => editor().unwrap_or_else(file_manager),
        };
        args.push(dir.into_os_string());
        // Shims (`code.cmd`) are wrapped like agent launches, with the
        // folder among the escaped arguments.
        let cmd = crate::agents::wrap_for_platform(std::path::Path::new(&program), args).map_err(
            |e| {
                ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "open_failed",
                    e.to_string(),
                )
            },
        )?;
        spawn_detached(cmd)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
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

/// `$VISUAL`, then `$EDITOR`, then `code`, whichever resolves to a program.
fn editor() -> Option<(OsString, Vec<OsString>)> {
    let from_env = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find_map(|v| {
            let (program, args) = split_command(&v)?;
            Some((process::which(&program)?, args))
        });
    let (path, args) = from_env.or_else(|| Some((process::which("code")?, Vec::new())))?;
    let args = args.into_iter().map(OsString::from).collect();
    Some((path.into_os_string(), args))
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

/// Start `command` without waiting for it; a thread reaps it when it exits.
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
