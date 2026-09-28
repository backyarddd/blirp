//! Per-machine views for starting sessions on any paired machine (cloud
//! sessions): its health (keep-awake), agents (with login state), folders
//! and `git clone`. `/api/machines/:id/...` for another machine is
//! forwarded to it through the hub (§10), where the same route answers for
//! itself; relayed requests carry the caller's rights, never credentials.

use super::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, Control, Principal, blocking};
use crate::clone::{self, CloneError};
use crate::state::SharedState;
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use blirp_core::model::{CloneRepo, MachineDir, MachineDirs};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Folders returned per listing.
const MAX_DIRS: usize = 2000;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/machines/{id}/health", get(health))
        .route("/api/machines/{id}/agents", get(agents))
        .route("/api/machines/{id}/dirs", get(dirs))
        .route("/api/machines/{id}/clone", post(clone_repo))
        .route("/api/machines/{id}/clone/{job}", get(clone_job))
}

async fn health(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    principal: Principal,
) -> ApiResult<Response> {
    if id == s.machine.id {
        return Ok(super::misc::health(State(s), principal)
            .await
            .into_response());
    }
    crate::sync::forward(&s, &id, &principal, Method::GET, "/api/health", None).await
}

async fn agents(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    principal: Principal,
) -> ApiResult<Response> {
    if id == s.machine.id {
        return super::misc::agents(State(s))
            .await
            .map(IntoResponse::into_response);
    }
    crate::sync::forward(&s, &id, &principal, Method::GET, "/api/agents", None).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirsQuery {
    /// Absolute folder inside the home folder; empty for the home folder.
    path: Option<String>,
    /// Include hidden folders (dot-folders; hidden attribute on Windows).
    hidden: Option<bool>,
}

/// `GET /api/machines/:id/dirs`: folder names only (never files or their
/// contents), inside that machine's home. Needs `control`: it is for
/// choosing where to start a session.
async fn dirs(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    Control(principal): Control,
    ApiQuery(q): ApiQuery<DirsQuery>,
) -> ApiResult<Response> {
    let hidden = q.hidden.unwrap_or(false);
    if id != s.machine.id {
        let mut url = relay_url(&format!("/api/machines/{id}/dirs"))?;
        {
            let mut pairs = url.query_pairs_mut();
            if let Some(p) = &q.path {
                pairs.append_pair("path", p);
            }
            pairs.append_pair("hidden", if hidden { "true" } else { "false" });
        }
        let path = path_and_query(&url);
        return crate::sync::forward(&s, &id, &principal, Method::GET, &path, None).await;
    }
    let home = home()?;
    let machine = s.machine.id.clone();
    let listing = blocking(move || list_dirs(&machine, &home, q.path.as_deref(), hidden)).await?;
    Ok(Json(listing).into_response())
}

fn relay_url(path: &str) -> ApiResult<reqwest::Url> {
    reqwest::Url::parse(&format!("http://blirp.remote{path}"))
        .map_err(|e| ApiError::bad_request(format!("invalid machine id: {e}")))
}

fn path_and_query(url: &reqwest::Url) -> String {
    match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    }
}

fn home() -> ApiResult<PathBuf> {
    blirp_core::paths::user_home().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_home",
            "this machine's home folder is unknown",
        )
    })
}

fn outside_home() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "path_outside_home",
        "only folders inside the home folder can be listed",
    )
}

fn is_hidden(name: &str, meta: Option<&std::fs::Metadata>) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        if meta.is_some_and(|m| m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0) {
            return true;
        }
    }
    #[cfg(not(windows))]
    let _ = meta;
    name.starts_with('.')
}

/// The folders in `path` (default `home`), which must resolve inside
/// `home`. Symlinked folders are left out, so nothing listed leads outside.
fn list_dirs(
    machine: &str,
    home: &Path,
    path: Option<&str>,
    hidden: bool,
) -> ApiResult<MachineDirs> {
    let home =
        dunce::canonicalize(home).map_err(|e| ApiError::internal("reading the home folder", e))?;
    let dir = match path.map(str::trim).filter(|p| !p.is_empty()) {
        None => home.clone(),
        Some(p) => {
            if !blirp_core::paths::is_local_absolute(Path::new(p)) {
                return Err(ApiError::bad_request(
                    "path must be an absolute folder on this machine (not a network path)",
                ));
            }
            dunce::canonicalize(p).map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => ApiError::not_found("folder"),
                _ => ApiError::bad_request(format!("cannot open folder: {e}")),
            })?
        }
    };
    if !dir.starts_with(&home) {
        return Err(outside_home());
    }
    let read = std::fs::read_dir(&dir).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotADirectory => ApiError::bad_request("path is not a folder"),
        _ => ApiError::bad_request(format!("cannot list folder: {e}")),
    })?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in read {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, "skipping unreadable folder entry");
                continue;
            }
        };
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !hidden && is_hidden(&name, entry.metadata().ok().as_ref()) {
            continue;
        }
        if entries.len() == MAX_DIRS {
            truncated = true;
            break;
        }
        let p = entry.path();
        entries.push(MachineDir {
            is_git: p.join(".git").exists(),
            path: p.display().to_string(),
            name,
        });
    }
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    let parent = (dir != home)
        .then(|| dir.parent().map(|p| p.display().to_string()))
        .flatten();
    Ok(MachineDirs {
        machine_id: machine.to_string(),
        home: home.display().to_string(),
        path: dir.display().to_string(),
        parent,
        entries,
        truncated,
    })
}

fn clone_error(e: CloneError) -> ApiError {
    match e {
        CloneError::Invalid(m) => ApiError::bad_request(m),
        e @ CloneError::Exists(_) => ApiError::conflict("already_exists", e.to_string()),
    }
}

/// The clone URL of a project's git folder on this machine.
fn project_remote(s: &SharedState, project: &str) -> ApiResult<String> {
    s.store.live_project(project)?;
    s.store
        .local_roots(project, &s.machine.id)?
        .iter()
        .find_map(|root| blirp_core::git::remote_url(root))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "no_git_remote",
                "this project has no git folder with a remote on this machine",
            )
        })
}

/// `POST /api/machines/:id/clone`: `git clone` on that machine into
/// `<parent or ~/blirp>/<name>` in the background (202 with the job).
/// A `project_id` is resolved here, where its folder is; credentials in the
/// URL are removed before it is forwarded or used.
async fn clone_repo(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    Control(principal): Control,
    ApiJson(body): ApiJson<CloneRepo>,
) -> ApiResult<Response> {
    let url = match (body.url, body.project_id) {
        (Some(u), _) => u,
        (None, Some(project)) => {
            let st = s.clone();
            blocking(move || project_remote(&st, &project)).await?
        }
        (None, None) => return Err(ApiError::bad_request("url or project_id is required")),
    };
    let url = clone::sanitize_url(&url).map_err(clone_error)?;
    if id != s.machine.id {
        let req = CloneRepo {
            url: Some(url),
            project_id: None,
            parent: body.parent,
            name: body.name,
        };
        let json = serde_json::to_vec(&req).map_err(|e| ApiError::internal("encoding clone", e))?;
        let path = path_and_query(&relay_url(&format!("/api/machines/{id}/clone"))?);
        return crate::sync::forward(&s, &id, &principal, Method::POST, &path, Some(json)).await;
    }
    let name = match body.name {
        Some(n) => n.trim().to_string(),
        None => clone::repo_name(&url).ok_or_else(|| {
            ApiError::bad_request("cannot tell a folder name from the URL; give a name")
        })?,
    };
    let home = home()?;
    let dest = blocking(move || {
        clone::destination(&home, body.parent.as_deref(), &name).map_err(clone_error)
    })
    .await?;
    let job = s
        .clones
        .start(&s.machine.id, url, dest)
        .map_err(|e| ApiError::internal("starting git clone", e))?;
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

async fn clone_job(
    State(s): State<SharedState>,
    ApiPath((id, job)): ApiPath<(String, String)>,
    principal: Principal,
) -> ApiResult<Response> {
    if id != s.machine.id {
        let path = path_and_query(&relay_url(&format!("/api/machines/{id}/clone/{job}"))?);
        return crate::sync::forward(&s, &id, &principal, Method::GET, &path, None).await;
    }
    s.clones
        .get(&job)
        .map(|j| Json(j).into_response())
        .ok_or_else(|| ApiError::not_found("clone job"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_only_inside_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        for d in ["b-repo/.git", "a", ".hidden", "a/inner"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        std::fs::write(home.join("secret.txt"), "x").unwrap();
        std::fs::create_dir_all(dir.path().join("outside")).unwrap();

        let l = list_dirs("m", &home, None, false).unwrap();
        let names: Vec<_> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["a", "b-repo"],
            "files and hidden folders are left out"
        );
        assert!(l.entries[1].is_git && !l.entries[0].is_git);
        assert_eq!(l.parent, None);
        assert!(!l.truncated);

        let l = list_dirs("m", &home, None, true).unwrap();
        assert!(l.entries.iter().any(|e| e.name == ".hidden"));

        let a = l.entries.iter().find(|e| e.name == "a").unwrap();
        let inner = list_dirs("m", &home, Some(&a.path), false).unwrap();
        assert_eq!(inner.entries[0].name, "inner");
        assert_eq!(inner.parent.as_deref(), Some(l.home.as_str()));

        let outside = dir.path().join("outside");
        let parent = format!("{}/..", home.display());
        for bad in [outside.to_str().unwrap(), parent.as_str()] {
            assert_eq!(
                list_dirs("m", &home, Some(bad), false).unwrap_err().code,
                "path_outside_home",
                "{bad}"
            );
        }
        assert_eq!(
            list_dirs("m", &home, Some("a"), false).unwrap_err().code,
            "invalid_request",
            "relative paths are refused"
        );
        let file = home.join("secret.txt");
        assert!(list_dirs("m", &home, file.to_str(), false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_folders_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(dir.path(), home.join("up")).unwrap();
        assert!(
            list_dirs("m", &home, None, true)
                .unwrap()
                .entries
                .is_empty()
        );
        let up = home.join("up");
        assert_eq!(
            list_dirs("m", &home, up.to_str(), false).unwrap_err().code,
            "path_outside_home"
        );
    }
}
