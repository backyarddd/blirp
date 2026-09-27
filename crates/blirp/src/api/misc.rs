//! Health, machines, search, settings, agents and the server event stream.

use super::{Admin, ApiError, ApiJson, ApiQuery, ApiResult, Principal, blocking};
use crate::state::SharedState;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use blirp_core::model::{
    AgentInfo, DistillStatus, Health, Machine, SearchHitKind, SearchResults, ServerEvent,
    SettingsPatch, SettingsView, SummarizerPick,
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
        .route("/api/settings/summarizer", get(summarizer))
        .route("/api/agents", get(agents))
        .route("/api/events/ws", get(events_ws))
}

/// Routes mounted on the loopback listener only: never reachable from LAN
/// portal devices or requests relayed by the sync proxy.
pub fn local_routes() -> Router<SharedState> {
    Router::new().route("/api/daemon/shutdown", post(shutdown))
}

/// Graceful stop for clients without a signal path to the daemon (the
/// detached Windows daemon has no console): same as SIGTERM / Ctrl+C.
async fn shutdown(State(s): State<SharedState>, _: Admin) -> ApiResult<StatusCode> {
    tracing::info!("shutdown requested over the API");
    s.stop_requested.notify_one();
    Ok(StatusCode::ACCEPTED)
}

pub(super) async fn health(State(s): State<SharedState>, principal: Principal) -> Json<Health> {
    // The role changes at runtime (hub enable, pairing); the config is current.
    let role = s.config().sync.role;
    Json(Health {
        version: env!("CARGO_PKG_VERSION").to_string(),
        role,
        machine: blirp_core::model::Machine {
            role,
            ..s.machine.clone()
        },
        capabilities: principal.capabilities(),
        keep_awake: s.keep_awake.held(),
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
    let (values, used) = blocking(move || {
        Ok((
            store.all_settings()?,
            crate::memory::distill::budget_used(&store)?,
        ))
    })
    .await?;
    let config = s.config();
    let distill = DistillStatus {
        budget_used: u32::try_from(used).unwrap_or(u32::MAX),
        budget_limit: config.memory.daily_distill_limit,
        ..s.distiller.status(blirp_core::now_ms())
    };
    Ok(Json(SettingsView {
        config,
        values,
        distill,
    }))
}

/// What `memory.summarizer = "auto"` resolves to now. Its own route, not
/// part of `GET /api/settings`: a cache miss runs local login probes
/// (about a second).
async fn summarizer(State(s): State<SharedState>) -> ApiResult<Json<SummarizerPick>> {
    crate::memory::distill::resolve_auto(&s)
        .await
        .map(Json)
        .map_err(|e| ApiError::internal("resolving the summarizer", e))
}

async fn patch_settings(
    State(s): State<SharedState>,
    _: Admin,
    ApiJson(patch): ApiJson<SettingsPatch>,
) -> ApiResult<Json<SettingsView>> {
    // Role changes (hub enable/disable, join, leave) write the config too.
    let guard = crate::sync::lock_transition(&s).await;
    let state = s.clone();
    let before = s.config().sync;
    blocking(move || {
        if let Some(edited) = patch.config {
            let current = state.config();
            let mut cfg = match patch.base {
                Some(base) => current
                    .with_changes(&base, &edited)
                    .map_err(|e| ApiError::bad_request(e.to_string()))?,
                None => edited,
            };
            cfg.sync.role = current.sync.role;
            cfg.sync.hub = current.sync.hub;
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
    let after = s.config().sync;
    if after.allow_hub_control != before.allow_hub_control {
        crate::sync::proxied_rights_changed(&s);
    }
    let discovery = if after.lan_discovery != before.lan_discovery {
        crate::sync::apply_discovery_config(&s).await
    } else {
        Ok(())
    };
    // Apply portal changes (portal.lan, lan_port) now, not at the next start.
    crate::sync::apply_portal_config(&s).await?;
    drop(guard);
    discovery?;
    get_settings(State(s)).await
}

const AGENTS_TTL: Duration = Duration::from_secs(60);

pub(super) async fn agents(State(s): State<SharedState>) -> ApiResult<Json<Vec<AgentInfo>>> {
    // `blirp agents set-token` writes the token without telling the daemon:
    // a list detected before it changed (claude's login too) is stale.
    let paths = s.paths.clone();
    let stamp = blocking(move || Ok(blirp_core::claude_token::stamp(&paths))).await?;
    {
        let cache = s
            .agents_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, seen, list)) = cache.as_ref()
            && at.elapsed() < AGENTS_TTL
            && *seen == stamp
        {
            return Ok(Json(list.clone()));
        }
    }
    let config = s.config();
    let paths = s.paths.clone();
    let list = blocking(move || Ok(crate::agents::detect_all(&config, &paths))).await?;
    *s.agents_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((Instant::now(), stamp, list.clone()));
    Ok(Json(list))
}

async fn events_ws(
    State(s): State<SharedState>,
    principal: Principal,
    ws: WebSocketUpgrade,
) -> Response {
    let shutdown = crate::sync::connection_shutdown(&s, &principal);
    ws.on_upgrade(move |socket| push_events(s, socket, shutdown))
}

async fn push_events(
    s: SharedState,
    mut socket: WebSocket,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut rx = s.events.subscribe();
    // None: the client closed (or vanished); Some: we close with that code.
    let ours = loop {
        tokio::select! {
            ev = rx.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    // Missed events: tell the client to refetch.
                    Err(RecvError::Lagged(_)) => ServerEvent::Resync,
                    Err(RecvError::Closed) => break Some(super::WS_GOING_AWAY),
                };
                let text = match serde_json::to_string(&ev) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!(error = %e, "serializing server event");
                        continue;
                    }
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break None;
                }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break None,
                // Clients have nothing to say on this stream; pings are answered by axum.
                Some(Ok(_)) => {}
            },
            _ = shutdown.changed() => break Some(super::WS_GOING_AWAY),
        }
    };
    super::close_ws(&mut socket, ours).await;
}
