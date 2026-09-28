//! Sessions: list, launch, detail, events, stop, resume, rename, move.

use super::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, Control, blocking};
use crate::state::SharedState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use blirp_core::model::{
    EventsPage, LaunchSession, MoveSession, PatchSession, PrunedWorktree, RemoveWorktree,
    ServerEvent, Session, SessionDetail, SessionStatus, SessionsPage, WorktreeInfo,
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
        .route("/api/sessions/{id}/move", post(move_session))
        .route("/api/worktrees", get(worktrees))
        .route("/api/worktrees/prune", post(prune_worktrees))
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
    let (online, offline): (Vec<_>, Vec<_>) = s
        .sync
        .presence(&s.machine.id)
        .into_values()
        .partition(|p| p.online);
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
        local_machine: Some(s.machine.id.clone()),
        online: online.into_iter().map(|p| p.machine_id).collect(),
        offline: offline.into_iter().map(|p| p.machine_id).collect(),
    };
    Ok(Json(blocking(move || Ok(store.list_sessions(&f)?)).await?))
}

async fn launch(
    State(s): State<SharedState>,
    Control(principal): Control,
    ApiJson(body): ApiJson<LaunchSession>,
) -> ApiResult<Response> {
    // A launch can wait for a summary (up to 90 s with `continue_from`);
    // run it as its own task, so a client that goes away meanwhile (a
    // closed tab, a dropped forward) cannot stop it halfway.
    tokio::spawn(async move {
        if let Some(m) = body.machine.clone()
            && m != s.machine.id
        {
            return launch_remote(&s, &m, &principal, &body).await;
        }
        let session = crate::sessions::launch(&s, body).await?;
        Ok(axum::response::IntoResponse::into_response((
            StatusCode::CREATED,
            Json(session),
        )))
    })
    .await
    .map_err(|e| ApiError::internal("launch task", e))?
}

/// Forward a launch to machine `m`. A handoff from a session of this
/// machine gets its summary refreshed here first (only this machine can
/// distill it) and pushed toward the target, which renders the pack.
async fn launch_remote(
    s: &SharedState,
    m: &str,
    principal: &crate::api::Principal,
    body: &LaunchSession,
) -> ApiResult<Response> {
    let _handoff = match &body.continue_from {
        Some(src) => crate::memory::handoff::before_forward(s, src).await?.0,
        None => None,
    };
    crate::sync::launch_remote(s, m, principal, body).await
}

/// The machine a session runs on when that is not this one.
pub(super) async fn remote_machine(s: &SharedState, id: &str) -> ApiResult<Option<String>> {
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
            store.events_page(&id, q.after.unwrap_or(-1), q.limit.unwrap_or(200))?;
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

/// `GET /api/worktrees`: this machine's session worktrees and their state
/// (`blirp worktrees list`).
async fn worktrees(State(s): State<SharedState>) -> ApiResult<Json<Vec<WorktreeInfo>>> {
    let (store, paths) = (s.store.clone(), s.paths.clone());
    let list = blocking(move || Ok(crate::worktrees::list(&store, &paths)?)).await?;
    Ok(Json(list))
}

/// `POST /api/worktrees/prune`: remove the worktrees of ended sessions
/// without changes (`blirp worktrees prune`).
async fn prune_worktrees(
    State(s): State<SharedState>,
    _: Control,
) -> ApiResult<Json<Vec<PrunedWorktree>>> {
    let st = s.clone();
    let out = blocking(move || crate::worktrees::prune(&st)).await?;
    Ok(Json(out))
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
    Control(principal): Control,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Response> {
    // A session belongs to its machine (§10): only that machine deletes it.
    if let Some(m) = remote_machine(&s, &id).await? {
        let path = format!("/api/sessions/{id}");
        let resp =
            crate::sync::forward(&s, &m, &principal, axum::http::Method::DELETE, &path, None)
                .await?;
        if resp.status().is_success() {
            // Gone here now, not once the owner's delete replicates; the
            // tombstone keeps rows still in flight from bringing it back. A
            // local failure is not the caller's: the owner deleted it and
            // replication removes it here too.
            let (store, sid) = (s.store.clone(), id.clone());
            match blocking(move || {
                store.apply_remote(&blirp_core::store::Change::DeleteSession { id: sid })?;
                Ok(())
            })
            .await
            {
                Ok(()) => s.emit(ServerEvent::SessionDeleted { session_id: id }),
                Err(e) => {
                    tracing::warn!(session = %id, error = %e.message, "removing the remotely deleted session failed");
                }
            }
        }
        return Ok(resp);
    }
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
        let dir = paths
            .launch_dir(&sid)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        if let Err(e) = std::fs::remove_dir_all(&dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(session = %sid, error = %e, "removing launch files failed");
        }
        crate::uploads::remove_session(&paths, &sid);
        Ok(())
    })
    .await?;
    s.emit(ServerEvent::SessionDeleted { session_id: id });
    Ok(axum::response::IntoResponse::into_response(
        StatusCode::NO_CONTENT,
    ))
}

/// A rename or move of a session launched from here on another machine goes
/// to that machine, and its answer updates the copy here. Its row may not
/// have reached the hub yet: the hub would reject the change made here (§10
/// ownership, no row to change yet), and this machine's own pending write
/// would then hold back its pulls of the owner's rows for that session.
/// Sessions that came through replication are changed here as before, also
/// while their machine is offline. None: not launched from here.
async fn forward_to_launch_machine(
    s: &SharedState,
    id: &str,
    principal: &super::Principal,
    method: axum::http::Method,
    path: &str,
    body: Vec<u8>,
    apply: impl FnOnce(&mut Session, Session) + Send + 'static,
) -> ApiResult<Option<(Response, Option<(Session, Session)>)>> {
    let Some(m) = s.sync.remote_of(id) else {
        return Ok(None);
    };
    let resp = crate::sync::forward(s, &m, principal, method, path, Some(body)).await?;
    if !resp.status().is_success() {
        return Ok(Some((resp, None)));
    }
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, 16 << 20)
        .await
        .map_err(|e| ApiError::internal("reading proxied response", e))?;
    let owner: Session = serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::internal("remote machine answered an unexpected body", e))?;
    let (store, sid) = (s.store.clone(), id.to_string());
    let updated =
        blocking(move || Ok(store.update_remote_copy(&sid, |c| apply(c, owner))?)).await?;
    if let Some((_, after)) = &updated {
        s.emit(ServerEvent::SessionUpdated {
            session: after.clone(),
        });
    }
    let resp = Response::from_parts(parts, axum::body::Body::from(bytes));
    Ok(Some((resp, updated)))
}

async fn patch(
    State(s): State<SharedState>,
    Control(principal): Control,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<PatchSession>,
) -> ApiResult<Response> {
    let title = body
        .title
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    if title.as_ref().is_some_and(|t| t.chars().count() > 300) {
        return Err(ApiError::bad_request(
            "title must be at most 300 characters",
        ));
    }
    let json = serde_json::to_vec(&PatchSession {
        title: title.clone(),
    })
    .map_err(|e| ApiError::internal("encoding the rename", e))?;
    let path = format!("/api/sessions/{id}");
    let forwarded = forward_to_launch_machine(
        &s,
        &id,
        &principal,
        axum::http::Method::PATCH,
        &path,
        json,
        |c, owner| {
            c.title = owner.title;
            c.title_updated_at = owner.title_updated_at;
        },
    )
    .await?;
    if let Some((resp, _)) = forwarded {
        return Ok(resp);
    }
    let store = s.store.clone();
    let session = blocking(move || Ok(store.modify_session(&id, |s| s.title = title)?)).await?;
    s.emit(ServerEvent::SessionUpdated {
        session: session.clone(),
    });
    Ok(axum::response::IntoResponse::into_response(Json(session)))
}

/// POST /api/sessions/:id/move: file a session (any machine's: another
/// machine may re-point a session's project, §10) in another project, or
/// in Chats with `project_id: null`.
async fn move_session(
    State(s): State<SharedState>,
    Control(principal): Control,
    ApiPath(id): ApiPath<String>,
    ApiJson(body): ApiJson<MoveSession>,
) -> ApiResult<Response> {
    let json = serde_json::to_vec(&body).map_err(|e| ApiError::internal("encoding the move", e))?;
    let path = format!("/api/sessions/{id}/move");
    let forwarded = forward_to_launch_machine(
        &s,
        &id,
        &principal,
        axum::http::Method::POST,
        &path,
        json,
        |c, owner| {
            c.project_id = owner.project_id;
            c.project_updated_at = owner.project_updated_at;
        },
    )
    .await?;
    if let Some((resp, updated)) = forwarded {
        if let Some((before, after)) = updated {
            for project_id in [before.project_id, after.project_id] {
                s.emit(ServerEvent::ProjectUpdated { project_id });
            }
        }
        return Ok(resp);
    }
    let st = s.clone();
    let (session, from) = blocking(move || {
        let from = st
            .store
            .get_session(&id)?
            .ok_or_else(|| ApiError::not_found("session"))?
            .project_id;
        let session = st.store.move_session(
            &id,
            body.project_id.as_deref(),
            &st.machine.id,
            &st.machine.name,
        )?;
        Ok((session, from))
    })
    .await?;
    s.emit(ServerEvent::SessionUpdated {
        session: session.clone(),
    });
    for project_id in [from, session.project_id.clone()] {
        s.emit(ServerEvent::ProjectUpdated { project_id });
    }
    Ok(axum::response::IntoResponse::into_response(Json(session)))
}
