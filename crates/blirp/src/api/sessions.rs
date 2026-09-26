//! Sessions: list, launch, detail, events, stop, resume, rename.

use super::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, Control, blocking};
use crate::state::SharedState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use blirp_core::model::{
    EventsPage, LaunchSession, PatchSession, RemoveWorktree, ServerEvent, Session, SessionDetail,
    SessionStatus, SessionsPage,
};
use blirp_core::store::SessionFilter;
use serde::Deserialize;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/sessions", get(list).post(launch))
        .route(
            "/api/sessions/{id}",
            get(detail).patch(patch).delete(remove),
        )
        .route("/api/sessions/{id}/events", get(events))
        .route("/api/sessions/{id}/stop", post(stop))
        .route("/api/sessions/{id}/resume", post(resume))
        .route("/api/sessions/{id}/worktree/remove", post(remove_worktree))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    project: Option<String>,
    status: Option<SessionStatus>,
    agent: Option<String>,
    machine: Option<String>,
    q: Option<String>,
    /// Only the subagent children of this session.
    parent: Option<String>,
    /// Include subagent children in an unfiltered list (default false).
    include_children: Option<bool>,
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
        hide_children: q.parent.is_none() && !q.include_children.unwrap_or(false),
        parent: q.parent,
        cursor: q.cursor,
        limit: q.limit.unwrap_or(50),
    };
    Ok(Json(blocking(move || Ok(store.list_sessions(&f)?)).await?))
}

async fn launch(
    State(s): State<SharedState>,
    Control(principal): Control,
    ApiJson(body): ApiJson<LaunchSession>,
) -> ApiResult<Response> {
    if let Some(m) = body.machine.clone()
        && m != s.machine.id
    {
        return crate::sync::launch_remote(&s, &m, &principal, &body).await;
    }
    let session = crate::sessions::launch(&s, body).await?;
    Ok(axum::response::IntoResponse::into_response((
        StatusCode::CREATED,
        Json(session),
    )))
}

/// The machine a session runs on when that is not this one.
async fn remote_machine(s: &SharedState, id: &str) -> ApiResult<Option<String>> {
    if s.terminals.get(id).is_some() {
        return Ok(None);
    }
    crate::sync::remote_machine_of(s, id).await
}

async fn detail(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<SessionDetail>> {
    let store = s.store.clone();
    blocking(move || {
        let session = store
            .get_session(&id)?
            .ok_or_else(|| ApiError::not_found("session"))?;
        let children_count = store.children_count(&id)?;
        Ok(SessionDetail {
            session,
            children_count,
        })
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
    ApiPath(id): ApiPath<String>,
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

async fn stop(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    Control(principal): Control,
) -> ApiResult<Response> {
    if let Some(m) = remote_machine(&s, &id).await? {
        let path = format!("/api/sessions/{id}/stop");
        return crate::sync::forward(&s, &m, &principal, axum::http::Method::POST, &path, None)
            .await;
    }
    crate::sessions::stop(&s, &id)?;
    Ok(axum::response::IntoResponse::into_response(
        StatusCode::ACCEPTED,
    ))
}

async fn resume(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    Control(principal): Control,
) -> ApiResult<Response> {
    if let Some(m) = remote_machine(&s, &id).await? {
        let path = format!("/api/sessions/{id}/resume");
        return crate::sync::forward(&s, &m, &principal, axum::http::Method::POST, &path, None)
            .await;
    }
    let session = crate::sessions::resume(&s, &id).await?;
    Ok(axum::response::IntoResponse::into_response(Json(session)))
}

async fn remove_worktree(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<RemoveWorktree>,
) -> ApiResult<Json<Session>> {
    let force = body.force.unwrap_or(false);
    let st = s.clone();
    let session = blocking(move || crate::sessions::remove_worktree(&st, &id, force)).await?;
    Ok(Json(session))
}

/// DELETE /api/sessions/:id: a session that is not running, with its
/// events and subagent sessions (replicated as a delete, §5).
async fn remove(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<StatusCode> {
    if s.terminals.get(&id).is_some() {
        return Err(ApiError::conflict(
            "session_live",
            "the session is running; stop it first",
        ));
    }
    let (store, paths, sid) = (s.store.clone(), s.paths.clone(), id.clone());
    blocking(move || {
        store.delete_session(&sid).map_err(|e| match e {
            blirp_core::store::StoreError::Conflict(m) => ApiError::conflict("session_live", m),
            other => other.into(),
        })?;
        // Launch files (memory, handoff) of this machine's launch.
        let dir = paths.launch_dir(&sid);
        if let Err(e) = std::fs::remove_dir_all(&dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(session = %sid, error = %e, "removing launch files failed");
        }
        Ok(())
    })
    .await?;
    s.emit(ServerEvent::SessionDeleted { session_id: id });
    Ok(StatusCode::NO_CONTENT)
}

async fn patch(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
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
