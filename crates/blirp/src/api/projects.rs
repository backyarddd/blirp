//! Projects: list, register, rename, delete, merge, memory view.

use super::{ApiError, ApiJson, ApiPath, ApiResult, Control, blocking};
use crate::state::SharedState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use blirp_core::model::{
    CreateProject, MergeProject, PatchProject, ProjectMemory, ProjectSummary, RecordStatus,
    ServerEvent,
};
use blirp_core::store::RecordFilter;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/projects", get(list).post(create))
        .route(
            "/api/projects/{id}",
            get(get_one).patch(rename).delete(remove),
        )
        .route("/api/projects/{id}/merge", post(merge))
        .route("/api/projects/{id}/memory", get(memory))
}

async fn list(State(s): State<SharedState>) -> ApiResult<Json<Vec<ProjectSummary>>> {
    let (store, machine) = (s.store.clone(), s.machine.id.clone());
    Ok(Json(
        blocking(move || Ok(store.list_project_summaries(&machine)?)).await?,
    ))
}

async fn get_one(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<ProjectSummary>> {
    let (store, machine) = (s.store.clone(), s.machine.id.clone());
    Ok(Json(
        blocking(move || Ok(store.project_summary(&id, &machine)?)).await?,
    ))
}

async fn create(
    State(s): State<SharedState>,
    _: Control,
    ApiJson(body): ApiJson<CreateProject>,
) -> ApiResult<(StatusCode, Json<ProjectSummary>)> {
    // A relative folder would resolve against the daemon's own directory.
    if !std::path::Path::new(&body.path).is_absolute() {
        return Err(ApiError::bad_request("path must be an absolute path"));
    }
    let (store, machine) = (s.store.clone(), s.machine.id.clone());
    let summary = blocking(move || {
        let p = store.register_project(
            &machine,
            std::path::Path::new(&body.path),
            body.name.as_deref(),
        )?;
        Ok(store.project_summary(&p.id, &machine)?)
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
    let (store, machine) = (s.store.clone(), s.machine.id.clone());
    let summary = blocking(move || {
        store.rename_project(&id, &body.name)?;
        Ok(store.project_summary(&id, &machine)?)
    })
    .await?;
    s.emit(ServerEvent::ProjectUpdated {
        project_id: summary.project.id.clone(),
    });
    Ok(Json(summary))
}

async fn remove(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<StatusCode> {
    let store = s.store.clone();
    let pid = id.clone();
    blocking(move || Ok(store.delete_project(&pid)?)).await?;
    s.emit(ServerEvent::ProjectUpdated { project_id: id });
    Ok(StatusCode::NO_CONTENT)
}

async fn merge(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<MergeProject>,
) -> ApiResult<Json<ProjectSummary>> {
    let (store, machine) = (s.store.clone(), s.machine.id.clone());
    let from = id.clone();
    let summary = blocking(move || {
        let into = store.merge_projects(&from, &body.into)?;
        Ok(store.project_summary(&into.id, &machine)?)
    })
    .await?;
    s.emit(ServerEvent::ProjectUpdated { project_id: id });
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
