//! `/api/projects/:id/files-sync/*`: file sync settings, status and
//! actions of a project (docs/project-files.md).

use crate::api::{ApiError, ApiPath, ApiQuery, ApiResult, blocking};
use crate::state::SharedState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use blirp_core::model::FilesPreview;
use serde::Deserialize;
use std::path::PathBuf;

pub fn routes() -> Router<SharedState> {
    Router::new().route("/api/projects/{id}/files-sync/preview", get(preview))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RootQuery {
    /// One of the project's folders on this machine (absolute); default the first.
    #[serde(default)]
    root: Option<String>,
}

/// The project's folder on this machine: `root` when given (it must be
/// one), else the first.
pub(crate) fn local_root(s: &SharedState, id: &str, root: Option<&str>) -> ApiResult<PathBuf> {
    s.store.live_project(id)?;
    let roots = s.store.local_roots(id, &s.machine.id)?;
    match root {
        Some(r) => roots
            .into_iter()
            .find(|p| p.as_os_str() == r)
            .ok_or_else(|| {
                ApiError::bad_request("root is not a folder of this project on this machine")
            }),
        None => roots.into_iter().next().ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "no_local_folder",
                "project has no folder on this machine",
            )
        }),
    }
}

/// `GET /api/projects/:id/files-sync/preview?root=`: a dry run of what
/// uploading this folder would send. Nothing leaves the machine; the hashes
/// it computes stay in the local cache for the real upload.
async fn preview(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<RootQuery>,
) -> ApiResult<Json<FilesPreview>> {
    let _gate = s
        .files
        .hash_gate
        .acquire()
        .await
        .map_err(|e| ApiError::internal("waiting for the file scanner", e))?;
    let st = s.clone();
    blocking(move || {
        let root = local_root(&st, &id, q.root.as_deref())?;
        let project = st.store.live_project(&id)?;
        let never = super::local::never_synced(&st.store, st.paths.home(), &project, &root);
        let cfg = super::local::scan_config(&st.config().files, st.paths.home());
        let key = root.display().to_string();
        let ls = super::local::scan_copy(&st.store, &key, &root, &cfg).map_err(|e| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "root_missing",
                format!("project folder is not accessible: {e}"),
            )
        })?;
        Ok(Json(super::local::preview(&ls, &root, never)))
    })
    .await
}
