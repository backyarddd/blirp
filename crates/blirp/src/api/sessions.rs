//! Sessions: list, launch, detail, events, stop, resume, rename.

use super::{ApiError, ApiJson, ApiQuery, ApiResult, blocking};
use crate::state::SharedState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use blirp_core::model::{
    EventsPage, LaunchSession, PatchSession, ServerEvent, Session, SessionStatus, SessionsPage,
};
use blirp_core::store::SessionFilter;
use serde::Deserialize;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/sessions", get(list).post(launch))
        .route("/api/sessions/{id}", get(detail).patch(patch))
        .route("/api/sessions/{id}/events", get(events))
        .route("/api/sessions/{id}/stop", post(stop))
        .route("/api/sessions/{id}/resume", post(resume))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    project: Option<String>,
    status: Option<SessionStatus>,
    agent: Option<String>,
    machine: Option<String>,
    q: Option<String>,
    cursor: Option<String>,
    limit: Option<i64>,
}

async fn list(
    State(s): State<SharedState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<SessionsPage>> {
    let store = s.store.clone();
    let f = SessionFilter {
        project_id: q.project,
        status: q.status,
        agent: q.agent,
        machine_id: q.machine,
        q: q.q,
        cursor: q.cursor,
        limit: q.limit.unwrap_or(50),
    };
    Ok(Json(blocking(move || Ok(store.list_sessions(&f)?)).await?))
}

async fn launch(
    State(s): State<SharedState>,
    ApiJson(body): ApiJson<LaunchSession>,
) -> ApiResult<(StatusCode, Json<Session>)> {
    let session = crate::sessions::launch(&s, body).await?;
    Ok((StatusCode::CREATED, Json(session)))
}

async fn detail(State(s): State<SharedState>, Path(id): Path<String>) -> ApiResult<Json<Session>> {
    let store = s.store.clone();
    blocking(move || {
        store
            .get_session(&id)?
            .ok_or_else(|| ApiError::not_found("session"))
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsQuery {
    after: Option<i64>,
    limit: Option<i64>,
}

async fn events(
    State(s): State<SharedState>,
    Path(id): Path<String>,
    ApiQuery(q): ApiQuery<EventsQuery>,
) -> ApiResult<Json<EventsPage>> {
    let store = s.store.clone();
    blocking(move || {
        store
            .get_session(&id)?
            .ok_or_else(|| ApiError::not_found("session"))?;
        let (items, next_after) =
            store.events_page(&id, q.after.unwrap_or(0), q.limit.unwrap_or(200))?;
        Ok(EventsPage { items, next_after })
    })
    .await
    .map(Json)
}

async fn stop(State(s): State<SharedState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    crate::sessions::stop(&s, &id)?;
    Ok(StatusCode::ACCEPTED)
}

async fn resume(State(s): State<SharedState>, Path(id): Path<String>) -> ApiResult<Json<Session>> {
    crate::sessions::resume(&s, &id).await.map(Json)
}

async fn patch(
    State(s): State<SharedState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<PatchSession>,
) -> ApiResult<Json<Session>> {
    let title = body
        .title
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    if title.as_ref().is_some_and(|t| t.chars().count() > 300) {
        return Err(ApiError::bad_request(
            "title must be at most 300 characters",
        ));
    }
    let store = s.store.clone();
    let session = blocking(move || Ok(store.modify_session(&id, |s| s.title = title)?)).await?;
    s.emit(ServerEvent::SessionUpdated {
        session: session.clone(),
    });
    Ok(Json(session))
}
