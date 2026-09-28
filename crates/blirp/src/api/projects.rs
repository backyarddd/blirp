//! Projects: list, create (with or without a folder), rename, delete (to
//! the Trash) and restore, merge, folders, memory view.

use super::{Admin, ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, Control, blocking};
use crate::state::SharedState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use blirp_core::model::{
    AddProjectFolder, CreateProject, MergeProject, PatchProject, ProjectMemory, ProjectSummary,
    RecordStatus, RemoveProjectFolder, ServerEvent,
};
use blirp_core::store::RecordFilter;
use serde::Deserialize;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/projects", get(list).post(create))
        .route(
            "/api/projects/{id}",
            get(get_one).patch(rename).delete(remove),
        )
        .route("/api/projects/{id}/merge", post(merge))
        .route("/api/projects/{id}/restore", post(restore))
        .route("/api/projects/{id}/folders", post(add_folder))
        .route("/api/projects/{id}/folders/remove", post(remove_folder))
        .route("/api/projects/chat-candidates", get(chat_candidates))
        .route(
            "/api/projects/chat-candidates/dismiss",
            post(dismiss_chat_candidates),
        )
        .route("/api/projects/{id}/to-chats", post(to_chats))
        .route("/api/projects/{id}/memory", get(memory))
}

/// A project with no folder on any machine starts its sessions in this
/// machine's blirp workspace (§5); the summary names it.
fn with_workspace(s: &SharedState, mut p: ProjectSummary) -> ProjectSummary {
    if p.paths.is_empty() && !p.project.chats {
        p.workspace = s
            .paths
            .workspace_dir(&p.project.id)
            .ok()
            .map(|w| w.display().to_string());
    }
    p
}

/// A folder given to register: absolute (a relative one would resolve
/// against the daemon's own directory) and on a local disk, checked before
/// anything touches the filesystem (a network path would connect to its
/// server).
fn local_folder(path: &str) -> ApiResult<()> {
    if blirp_core::paths::is_local_absolute(std::path::Path::new(path)) {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "path must be an absolute path to a folder on this machine (not a network path)",
        ))
    }
}

fn summary(s: &SharedState, id: &str) -> ApiResult<ProjectSummary> {
    Ok(with_workspace(
        s,
        s.store.project_summary(id, &s.machine.id)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    /// The Trash: projects deleted by the user (not merged away).
    deleted: Option<bool>,
}

async fn list(
    State(s): State<SharedState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<Vec<ProjectSummary>>> {
    let st = s.clone();
    Ok(Json(
        blocking(move || {
            let projects = if q.deleted.unwrap_or(false) {
                st.store.list_trashed_projects(&st.machine.id)?
            } else {
                st.store.list_project_summaries(&st.machine.id)?
            };
            Ok(projects
                .into_iter()
                .map(|p| with_workspace(&st, p))
                .collect())
        })
        .await?,
    ))
}

async fn get_one(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<ProjectSummary>> {
    let st = s.clone();
    Ok(Json(blocking(move || summary(&st, &id)).await?))
}

async fn create(
    State(s): State<SharedState>,
    _: Control,
    ApiJson(body): ApiJson<CreateProject>,
) -> ApiResult<(StatusCode, Json<ProjectSummary>)> {
    if let Some(p) = &body.path {
        local_folder(p)?;
    }
    if let Some(b) = &body.brief {
        super::memory::check_len("brief", b)?;
    }
    let st = s.clone();
    let summary = blocking(move || {
        let store = &st.store;
        let p = match &body.path {
            Some(path) => {
                let p = store.register_project(
                    &st.machine.id,
                    std::path::Path::new(path),
                    body.name.as_deref(),
                )?;
                if let Some(b) = body
                    .brief
                    .as_deref()
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                {
                    store.put_brief(&p.id, b, "user")?;
                }
                p
            }
            None => {
                let name = body.name.as_deref().ok_or_else(|| {
                    ApiError::bad_request("name is required for a project without a folder")
                })?;
                store.create_project(name, body.brief.as_deref())?
            }
        };
        summary(&st, &p.id)
    })
    .await?;
    s.emit(ServerEvent::ProjectUpdated {
        project_id: summary.project.id.clone(),
    });
    Ok((StatusCode::CREATED, Json(summary)))
}

async fn rename(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<PatchProject>,
) -> ApiResult<Json<ProjectSummary>> {
    let st = s.clone();
    let summary = blocking(move || {
        st.store.rename_project(&id, &body.name)?;
        summary(&st, &id)
    })
    .await?;
    s.emit(ServerEvent::ProjectUpdated {
        project_id: summary.project.id.clone(),
    });
    Ok(Json(summary))
}

/// Move a project to the Trash (admin: it unregisters its folders on every
/// machine and hides its sessions until it is restored).
async fn remove(
    State(s): State<SharedState>,
    _: Admin,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<StatusCode> {
    let (store, machine) = (s.store.clone(), s.machine.id.clone());
    let pid = id.clone();
    blocking(move || Ok(store.delete_project(&pid, &machine)?)).await?;
    s.emit(ServerEvent::ProjectUpdated { project_id: id });
    Ok(StatusCode::NO_CONTENT)
}

/// Bring a project back from the Trash, with this machine's folders it had
/// when it was deleted here.
async fn restore(
    State(s): State<SharedState>,
    _: Admin,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<ProjectSummary>> {
    let st = s.clone();
    let summary = blocking(move || {
        st.store.restore_project(&id, &st.machine.id)?;
        summary(&st, &id)
    })
    .await?;
    s.emit(ServerEvent::ProjectUpdated {
        project_id: summary.project.id.clone(),
    });
    Ok(Json(summary))
}

async fn merge(
    State(s): State<SharedState>,
    _: Admin,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<MergeProject>,
) -> ApiResult<Json<ProjectSummary>> {
    let st = s.clone();
    let from = id.clone();
    let summary = blocking(move || {
        let into = st.store.merge_projects(&from, &body.into)?;
        summary(&st, &into.id)
    })
    .await?;
    s.emit(ServerEvent::ProjectUpdated { project_id: id });
    s.emit(ServerEvent::ProjectUpdated {
        project_id: summary.project.id.clone(),
    });
    Ok(Json(summary))
}

/// Local settings key: the user dismissed the chat candidates offer.
const CANDIDATES_DISMISSED: &str = "projects.chat_candidates.dismissed";

/// Projects earlier versions made for plain folders that look like chats
/// (§5), offered once to move to Chats; empty once dismissed.
async fn chat_candidates(State(s): State<SharedState>) -> ApiResult<Json<Vec<ProjectSummary>>> {
    let st = s.clone();
    Ok(Json(
        blocking(move || {
            if st.store.get_setting(CANDIDATES_DISMISSED)?.is_some() {
                return Ok(Vec::new());
            }
            let dirs = blirp_core::store::NonProjectDirs::from_process()
                .with_workspaces(&st.paths.workspaces_dir());
            st.store
                .chat_candidates(&st.machine.id, &dirs)?
                .into_iter()
                .map(|p| summary(&st, &p.id))
                .collect()
        })
        .await?,
    ))
}

async fn dismiss_chat_candidates(
    State(s): State<SharedState>,
    _: Control,
) -> ApiResult<StatusCode> {
    let store = s.store.clone();
    blocking(move || {
        Ok(store.set_setting(
            CANDIDATES_DISMISSED,
            &serde_json::json!(blirp_core::now_ms()),
        )?)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Move a whole project to Chats (the user confirmed): its sessions and
/// records move, its folders and brief stay with the removed project.
async fn to_chats(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<StatusCode> {
    let st = s.clone();
    let pid = id.clone();
    blocking(move || {
        let dirs = blirp_core::store::NonProjectDirs::from_process()
            .with_workspaces(&st.paths.workspaces_dir());
        Ok(st
            .store
            .move_project_to_chats(&pid, &st.machine.id, &st.machine.name, &dirs)?)
    })
    .await?;
    s.emit(ServerEvent::ProjectUpdated { project_id: id });
    Ok(StatusCode::NO_CONTENT)
}

/// Register an existing folder on this machine with the project (its git
/// top level inside a repository). One inside another project's folder here
/// is a conflict (409); one already inside this project's folders is left as
/// it is.
async fn add_folder(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<AddProjectFolder>,
) -> ApiResult<Json<ProjectSummary>> {
    local_folder(&body.path)?;
    let st = s.clone();
    let summary = blocking(move || {
        st.store
            .add_project_folder(&id, &st.machine.id, std::path::Path::new(&body.path))?;
        summary(&st, &id)
    })
    .await?;
    s.emit(ServerEvent::ProjectUpdated {
        project_id: summary.project.id.clone(),
    });
    Ok(Json(summary))
}

/// Unregister one of this machine's folders; the project stays, also with
/// no folder left.
async fn remove_folder(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<RemoveProjectFolder>,
) -> ApiResult<Json<ProjectSummary>> {
    let st = s.clone();
    let summary = blocking(move || {
        st.store
            .remove_project_folder(&id, &st.machine.id, &body.path)?;
        summary(&st, &id)
    })
    .await?;
    s.emit(ServerEvent::ProjectUpdated {
        project_id: summary.project.id.clone(),
    });
    Ok(Json(summary))
}

async fn memory(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<ProjectMemory>> {
    let store = s.store.clone();
    Ok(Json(
        blocking(move || {
            store.live_project(&id)?;
            Ok(ProjectMemory {
                brief: store.get_brief(&id)?,
                records: store.list_records(
                    &id,
                    &RecordFilter {
                        status: Some(RecordStatus::Active),
                        kind: None,
                    },
                )?,
                recent_sessions: store.recent_sessions(&id, 10)?,
            })
        })
        .await?,
    ))
}
