//! Memory-engine endpoints (§11): hook ingress, rendered injection, manual
//! distill, global integration install/uninstall and MCP over HTTP.

use super::{ApiError, ApiJson, ApiQuery, ApiResult, Principal, blocking};
use crate::hooks::{HookIngress, HookReply};
use crate::memory::launch::MEMORY_FILE;
use crate::memory::render::render_injection;
use crate::state::SharedState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use blirp_core::model::{AgentInfo, Injection};
use serde::Deserialize;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/hooks/{agent}/{event}", post(hook))
        .route("/api/inject", get(inject))
        .route("/api/sessions/{id}/distill", post(distill))
        .route("/api/agents/{id}/hooks/{action}", post(agent_hooks))
}

/// `/mcp`, mounted on the loopback listener only (see `api::build`).
pub fn mcp_routes(state: &SharedState) -> Router<SharedState> {
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut shutdown = state.shutdown.clone();
    let c = cancel.clone();
    tokio::spawn(async move {
        // Ends open MCP streams so graceful shutdown is not held up by them.
        let _ = shutdown.changed().await;
        c.cancel();
    });
    Router::new().nest_service("/mcp", crate::mcp::http_service(state.clone(), cancel))
}

async fn hook(
    State(s): State<SharedState>,
    principal: Principal,
    Path((agent, event)): Path<(String, String)>,
    ApiJson(body): ApiJson<HookIngress>,
) -> ApiResult<Json<HookReply>> {
    // Only `blirp hook` on this machine (runtime token) reports agent events.
    principal.require_admin()?;
    let st = s.clone();
    blocking(move || crate::hooks::handle(&st, &agent, &event, body))
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InjectQuery {
    session: Option<String>,
    cwd: Option<String>,
    /// Accepted for symmetry with launch; the injection is agent-independent.
    #[allow(dead_code)]
    agent: Option<String>,
}

async fn inject(
    State(s): State<SharedState>,
    ApiQuery(q): ApiQuery<InjectQuery>,
) -> ApiResult<Json<Injection>> {
    let st = s.clone();
    blocking(move || {
        let max = st.config().memory.inject_max_chars as usize;
        if let Some(id) = &q.session {
            let session = st
                .store
                .get_session(id)?
                .ok_or_else(|| ApiError::not_found("session"))?;
            // What the session was actually given at launch, when it was launched here.
            if let Ok(markdown) = std::fs::read_to_string(st.paths.launch_dir(id).join(MEMORY_FILE))
            {
                return Ok(Injection { markdown });
            }
            let markdown = render_injection(&st.store, &session.project_id, Some(id), max)?;
            return Ok(Injection { markdown });
        }
        let cwd = q
            .cwd
            .ok_or_else(|| ApiError::bad_request("session or cwd is required"))?;
        let project = st
            .store
            .find_project_for_path(&st.machine.id, std::path::Path::new(&cwd))?
            .ok_or_else(|| ApiError::not_found("project for this folder"))?;
        Ok(Injection {
            markdown: render_injection(&st.store, &project.id, None, max)?,
        })
    })
    .await
    .map(Json)
}

async fn distill(
    State(s): State<SharedState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    // Runs the summarizer agent on this machine.
    principal.require_control()?;
    let store = s.store.clone();
    let sid = id.clone();
    let events = blocking(move || {
        store
            .get_session(&sid)?
            .ok_or_else(|| ApiError::not_found("session"))?;
        Ok(store.max_event_seq(&sid)?)
    })
    .await?;
    if events == 0 {
        return Err(ApiError::conflict(
            "nothing_to_distill",
            "the session has no transcript events yet",
        ));
    }
    s.distiller.enqueue(&id, true);
    Ok(StatusCode::ACCEPTED)
}

async fn agent_hooks(
    State(s): State<SharedState>,
    principal: Principal,
    Path((id, action)): Path<(String, String)>,
) -> ApiResult<Json<AgentInfo>> {
    // Writes the user's global agent configuration.
    principal.require_admin()?;
    let install = match action.as_str() {
        "install" => true,
        "uninstall" => false,
        _ => return Err(ApiError::not_found("action")),
    };
    if !crate::hooks::install::SUPPORTED.contains(&id.as_str()) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported",
            format!("global integration is not supported for {id}"),
        ));
    }
    let config = s.config();
    let agent_id = id.clone();
    let info = blocking(move || {
        let homes = crate::hooks::install::Homes::from_env()
            .ok_or_else(|| ApiError::internal("locating the home directory", "no home"))?;
        let result = if install {
            crate::hooks::install::install(&agent_id, &homes, &crate::memory::blirp_exe())
        } else {
            crate::hooks::install::uninstall(&agent_id, &homes)
        };
        result.map_err(|e| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "integration_failed",
                format!("{e:#}"),
            )
        })?;
        crate::agents::detect_all(&config)
            .into_iter()
            .find(|a| a.id == agent_id)
            .ok_or_else(|| ApiError::not_found("agent"))
    })
    .await?;
    s.invalidate_agents();
    Ok(Json(info))
}
