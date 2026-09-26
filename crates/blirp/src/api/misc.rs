//! Health, machines, search, settings, agents and the server event stream.

use super::{ApiError, ApiJson, ApiQuery, ApiResult, blocking};
use crate::state::SharedState;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use blirp_core::model::{
    AgentInfo, Health, Machine, SearchHitKind, SearchResults, ServerEvent, SettingsPatch,
    SettingsView,
};
use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio::sync::broadcast::error::RecvError;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/machines", get(machines))
        .route("/api/search", get(search))
        .route("/api/settings", get(get_settings).patch(patch_settings))
        .route("/api/agents", get(agents))
        .route("/api/events/ws", get(events_ws))
        .route("/api/daemon/shutdown", post(shutdown))
}

/// Graceful stop for clients without a signal path to the daemon (the
/// detached Windows daemon has no console): same as SIGTERM / Ctrl+C.
async fn shutdown(State(s): State<SharedState>) -> StatusCode {
    tracing::info!("shutdown requested over the API");
    s.stop_requested.notify_one();
    StatusCode::ACCEPTED
}

async fn health(State(s): State<SharedState>) -> Json<Health> {
    Json(Health {
        version: env!("CARGO_PKG_VERSION").to_string(),
        role: s.machine.role,
        machine: s.machine.clone(),
    })
}

async fn machines(State(s): State<SharedState>) -> ApiResult<Json<Vec<Machine>>> {
    let store = s.store.clone();
    Ok(Json(blocking(move || Ok(store.list_machines()?)).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchQuery {
    q: String,
    project: Option<String>,
    kind: Option<SearchHitKind>,
    limit: Option<i64>,
}

async fn search(
    State(s): State<SharedState>,
    ApiQuery(q): ApiQuery<SearchQuery>,
) -> ApiResult<Json<SearchResults>> {
    let store = s.store.clone();
    let hits = blocking(move || {
        Ok(store.search(&q.q, q.project.as_deref(), q.kind, q.limit.unwrap_or(50))?)
    })
    .await?;
    Ok(Json(SearchResults { hits }))
}

async fn get_settings(State(s): State<SharedState>) -> ApiResult<Json<SettingsView>> {
    let store = s.store.clone();
    let values = blocking(move || Ok(store.all_settings()?)).await?;
    Ok(Json(SettingsView {
        config: s.config(),
        values,
    }))
}

async fn patch_settings(
    State(s): State<SharedState>,
    ApiJson(patch): ApiJson<SettingsPatch>,
) -> ApiResult<Json<SettingsView>> {
    let state = s.clone();
    blocking(move || {
        if let Some(cfg) = patch.config {
            cfg.validate()
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
            cfg.save(&state.paths.config_file())
                .map_err(|e| ApiError::internal("writing config.toml", e))?;
            state.set_config(cfg);
            // Custom agents may have changed.
            *state
                .agents_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
        if let Some(values) = patch.values {
            state.store.set_settings(&values)?;
        }
        Ok(())
    })
    .await?;
    get_settings(State(s)).await
}

const AGENTS_TTL: Duration = Duration::from_secs(60);

async fn agents(State(s): State<SharedState>) -> ApiResult<Json<Vec<AgentInfo>>> {
    {
        let cache = s
            .agents_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, list)) = cache.as_ref()
            && at.elapsed() < AGENTS_TTL
        {
            return Ok(Json(list.clone()));
        }
    }
    let config = s.config();
    let list = blocking(move || Ok(crate::agents::detect_all(&config))).await?;
    *s.agents_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((Instant::now(), list.clone()));
    Ok(Json(list))
}

async fn events_ws(State(s): State<SharedState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| push_events(s, socket))
}

async fn push_events(s: SharedState, mut socket: WebSocket) {
    let mut rx = s.events.subscribe();
    let mut shutdown = s.shutdown.clone();
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    // Missed events: tell the client to refetch.
                    Err(RecvError::Lagged(_)) => ServerEvent::Resync,
                    Err(RecvError::Closed) => break,
                };
                let text = match serde_json::to_string(&ev) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(error = %e, "serializing server event");
                        continue;
                    }
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                // Clients have nothing to say on this stream; pings are answered by axum.
                Some(Ok(_)) => {}
            },
            _ = shutdown.changed() => break,
        }
    }
    // Best effort: the peer may already be gone.
    let _ = socket.send(Message::Close(None)).await;
}
