//! Daemon side of machine sync (§10): role lifecycle, `/api/sync/*`,
//! `/api/devices/*`, `DELETE /api/machines/:id`, and the remote API /
//! terminal proxy (see [`proxy`]).

mod proxy;

pub use proxy::{connect_terminal, forward, forward_body, launch_remote, relay_terminal};

use crate::api::{Admin, ApiError, ApiJson, ApiPath, ApiResult, Principal, blocking};
use crate::state::SharedState;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use blirp_core::config::Config;
use blirp_core::model::{
    BrowserInvite, Device, DeviceKind, JoinHub, JoinPreview, JoinPreviewRequest, LeftHub, Machine,
    MachineRole, PatchDevice, ServerEvent, SyncInvite, SyncStatus,
};
use blirp_sync::pair::{MachineMeta, PairError, Ticket};
use blirp_sync::service::{ProxyServe, StatusHook};
use blirp_sync::{Role, SyncError, SyncService};
use iroh::{EndpointAddr, SecretKey};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

/// `settings` key holding the hub's last known address (JSON `EndpointAddr`).
const HUB_ADDR_KEY: &str = "sync.hub_addr";

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Plain maps/options replaced whole; poisoning cannot corrupt them.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct SyncState {
    service: Mutex<Option<Arc<SyncService>>>,
    /// Serializes role changes (enable/disable/join/leave).
    transition: tokio::sync::Mutex<()>,
    identity: OnceLock<SecretKey>,
    /// Sessions launched on other machines from here: id -> machine id,
    /// so their terminals can be attached before the row replicates.
    remote_sessions: Mutex<HashMap<String, String>>,
    pub(crate) portal: crate::portal::PortalState,
    /// Ids of devices that were revoked or changed permissions.
    device_changes: tokio::sync::broadcast::Sender<String>,
}

impl Default for SyncState {
    fn default() -> Self {
        Self {
            service: Mutex::default(),
            transition: tokio::sync::Mutex::default(),
            identity: OnceLock::new(),
            remote_sessions: Mutex::default(),
            portal: crate::portal::PortalState::default(),
            device_changes: tokio::sync::broadcast::channel(64).0,
        }
    }
}

impl SyncState {
    pub fn set_identity(&self, key: SecretKey) {
        // Set once at startup; a second call would be a bug, keep the first.
        if self.identity.set(key).is_err() {
            tracing::warn!("identity already set");
        }
    }

    fn identity(&self) -> ApiResult<SecretKey> {
        self.identity
            .get()
            .cloned()
            .ok_or_else(|| ApiError::internal("sync", "identity key not loaded"))
    }

    pub fn service(&self) -> Option<Arc<SyncService>> {
        lock(&self.service).clone()
    }

    pub(crate) fn remember_remote(&self, session: &str, machine: &str) {
        lock(&self.remote_sessions).insert(session.to_string(), machine.to_string());
    }

    fn remote_of(&self, session: &str) -> Option<String> {
        lock(&self.remote_sessions).get(session).cloned()
    }

    /// Other machines' connection state by id: `true` online, `false`
    /// offline; unknown machines are missing (see
    /// [`SyncService::presence`]). Empty when sync is off.
    pub fn presence(&self, own_id: &str) -> HashMap<String, blirp_sync::repl::Presence> {
        let mut out = self.service().map(|s| s.presence()).unwrap_or_default();
        out.remove(own_id);
        out
    }
}

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/sync/status", get(get_status))
        .route("/api/sync/hub/enable", post(hub_enable))
        .route("/api/sync/hub/disable", post(hub_disable))
        .route("/api/sync/invite", post(invite))
        .route("/api/sync/join", post(join))
        .route("/api/sync/join/preview", post(join_preview))
        .route("/api/sync/leave", post(leave))
        .route("/api/devices", get(list_devices))
        .route("/api/devices/browser-invite", post(browser_invite))
        .route(
            "/api/devices/{id}",
            delete(revoke_device).patch(patch_device),
        )
        .route("/api/machines/{id}", delete(revoke_machine))
        .route("/api/sync/{*rest}", axum::routing::any(unknown))
        .route("/api/devices/{id}/{*rest}", axum::routing::any(unknown))
}

/// Held while the role changes or anything else writes the config, so a
/// settings save never races a role change.
pub(crate) async fn lock_transition(s: &SharedState) -> tokio::sync::MutexGuard<'_, ()> {
    s.sync.transition.lock().await
}

/// Start, stop or move the LAN portal to match a changed config. The caller
/// holds [`lock_transition`].
pub(crate) async fn apply_portal_config(s: &SharedState) -> ApiResult<()> {
    crate::portal::sync_with_config(s)
        .await
        .map_err(|e| portal_failed(&e))
}

/// `sync.lan_discovery` changed: restart a running sync endpoint so mDNS
/// starts or stops now, not at the next daemon start. A failed restart
/// leaves sync stopped (shown in the sync status) and answers 502
/// `sync_failed`; the saved config is kept. The caller holds
/// [`lock_transition`].
pub(crate) async fn apply_discovery_config(s: &SharedState) -> ApiResult<()> {
    if s.sync.service().is_none() {
        return Ok(());
    }
    stop_service(s).await;
    start_service(s).await.map_err(|e| {
        tracing::error!(error = %e.message, "sync did not restart");
        emit_status(s);
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "sync_failed",
            format!("sync could not restart: {}", e.message),
        )
    })
}

/// The LAN portal could not start (port taken, certificate); the config
/// that asked for it is kept.
fn portal_failed(e: &anyhow::Error) -> ApiError {
    tracing::warn!(error = %format!("{e:#}"), "LAN portal failed to start");
    ApiError::new(
        StatusCode::CONFLICT,
        "portal_failed",
        format!("the LAN portal could not start: {e:#}"),
    )
}

async fn unknown() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such endpoint")
}

// ---------------------------------------------------------------- lifecycle

/// Start sync (and the LAN portal on a hub) for the configured role.
/// Failures are logged and reported in the status; the daemon keeps running.
pub async fn start(state: &SharedState) {
    let on = state.config().sync.role != MachineRole::Standalone;
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || store.set_replication(on)).await {
        Ok(Ok(0)) => {}
        Ok(Ok(n)) => tracing::info!(rows = n, "queued existing data for replication"),
        Ok(Err(e)) => tracing::error!(error = %e, "switching replication failed"),
        Err(e) => tracing::error!(error = %e, "switching replication failed"),
    }
    if let Err(e) = start_service(state).await {
        tracing::error!(error = %e.message, "sync did not start");
    }
    if let Err(e) = crate::portal::sync_with_config(state).await {
        tracing::error!(error = %format!("{e:#}"), "LAN portal did not start");
    }
}

/// Stop sync and the portal (daemon shutdown).
pub async fn stop(state: &SharedState) {
    crate::portal::stop(state).await;
    crate::files::stop(state).await;
    let svc = lock(&state.sync.service).take();
    if let Some(svc) = svc {
        svc.shutdown().await;
    }
}

fn hub_addr(state: &SharedState, config: &Config) -> ApiResult<EndpointAddr> {
    let id = config
        .sync
        .hub
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("sync.hub is not set"))?;
    let id: iroh::EndpointId = id.parse().map_err(|e| {
        ApiError::bad_request(format!("sync.hub {id:?} is not an endpoint id: {e}"))
    })?;
    let saved = state
        .store
        .get_setting(HUB_ADDR_KEY)?
        .and_then(|v| serde_json::from_value::<EndpointAddr>(v).ok())
        .filter(|a| a.id == id);
    Ok(saved.unwrap_or_else(|| EndpointAddr::new(id)))
}

async fn start_service(state: &SharedState) -> ApiResult<()> {
    let config = state.config();
    let role = match config.sync.role {
        MachineRole::Standalone => return Ok(()),
        MachineRole::Hub => Role::Hub,
        MachineRole::Node => {
            let st = state.clone();
            let cfg = config.clone();
            Role::Node {
                hub: blocking(move || hub_addr(&st, &cfg)).await?,
            }
        }
    };
    let files = matches!(role, Role::Hub).then(|| crate::files::hub_files(state));
    let weak = Arc::downgrade(state);
    let proxy: ProxyServe = Arc::new(move |stream, principal| {
        let weak = weak.clone();
        Box::pin(async move {
            if let Some(st) = weak.upgrade() {
                proxy::serve(st, stream, principal).await;
            }
        })
    });
    let weak = Arc::downgrade(state);
    let on_status: StatusHook = Arc::new(move || {
        if let Some(st) = weak.upgrade() {
            emit_status(&st);
        }
    });
    let svc = SyncService::start(blirp_sync::StartOptions {
        secret: state.sync.identity()?,
        relay: config.sync.relay.clone(),
        lan_discovery: config.sync.lan_discovery,
        store: state.store.clone(),
        machine: Machine {
            role: config.sync.role,
            ..state.machine.clone()
        },
        role,
        proxy,
        on_status,
        files,
    })
    .await
    .map_err(|e| sync_error("starting sync", e))?;
    let svc = Arc::new(svc);
    let old = lock(&state.sync.service).replace(svc.clone());
    if let Some(old) = old {
        old.shutdown().await;
    }
    crate::files::start(state, &svc).await;
    emit_status(state);
    Ok(())
}

async fn stop_service(state: &SharedState) {
    crate::files::stop(state).await;
    let svc = lock(&state.sync.service).take();
    if let Some(svc) = svc {
        svc.shutdown().await;
    }
}

/// Persist a role change: config.toml, the in-memory config and this
/// machine's (replicated) row. `allow_hub_control`, when given, is written
/// in the same config update (join, leave).
async fn set_role(
    state: &SharedState,
    role: MachineRole,
    hub: Option<String>,
    allow_hub_control: Option<bool>,
) -> ApiResult<()> {
    let st = state.clone();
    blocking(move || {
        let mut cfg = st.config();
        cfg.sync.role = role;
        cfg.sync.hub = hub;
        if let Some(allow) = allow_hub_control {
            cfg.sync.allow_hub_control = allow;
        }
        cfg.save(&st.paths.config_file())
            .map_err(|e| ApiError::internal("writing config.toml", e))?;
        st.set_config(cfg);
        // Standalone queues nothing; becoming a hub or node queues what
        // exists (events in batches from the status tick).
        st.store.set_replication(role != MachineRole::Standalone)?;
        st.store.upsert_machine(&Machine {
            role,
            last_seen: blirp_core::now_ms(),
            ..st.machine.clone()
        })?;
        Ok(())
    })
    .await
}

pub(crate) fn emit_status(state: &SharedState) {
    let st = state.clone();
    tokio::spawn(async move {
        match status(&st).await {
            Ok(status) => st.emit(ServerEvent::SyncUpdated { status }),
            Err(e) => tracing::warn!(error = %e.message, "computing sync status failed"),
        }
    });
}

pub async fn status(state: &SharedState) -> ApiResult<SyncStatus> {
    let config = state.config();
    let rt = state.sync.service().map(|s| s.status()).unwrap_or_default();
    let hub = match config.sync.role {
        MachineRole::Hub => Some(state.machine.id.clone()),
        MachineRole::Node => config.sync.hub.clone(),
        MachineRole::Standalone => None,
    };
    let pending_outbox = match (&config.sync.role, &hub) {
        (MachineRole::Node, Some(h)) => {
            let store = state.store.clone();
            let h = h.clone();
            blocking(move || Ok(store.pending_outbox(&h)?)).await?
        }
        _ => 0,
    };
    // A machine revoked since it was refused no longer needs updating here.
    let outdated_machines = {
        let store = state.store.clone();
        let ids = rt.outdated.clone();
        blocking(move || {
            let mut out = Vec::new();
            for id in ids {
                if store.machine_device(&id)?.is_some() {
                    out.push(id);
                }
            }
            Ok(out)
        })
        .await?
    };
    let portal = crate::portal::info(state);
    Ok(SyncStatus {
        role: config.sync.role,
        machine_id: state.machine.id.clone(),
        hub,
        connected: rt.connected,
        last_sync_at: rt.last_sync_at,
        pending_outbox,
        portal_url: portal.as_ref().map(|p| p.0.clone()),
        portal_cert_fingerprint: portal.map(|p| p.1),
        relay_url: state.sync.service().and_then(|s| s.home_relay()),
        update_needed: rt.update_needed,
        outdated_machines,
    })
}

fn sync_error(context: &str, e: SyncError) -> ApiError {
    match e {
        SyncError::Pair(p) => pair_error(p),
        SyncError::Connect { message, .. } => ApiError::new(
            StatusCode::BAD_GATEWAY,
            "hub_unreachable",
            format!("cannot reach the hub: {message}"),
        ),
        SyncError::Unavailable(m) => ApiError::conflict("machine_unreachable", m),
        other => ApiError::internal(context, other),
    }
}

fn pair_error(e: PairError) -> ApiError {
    let code = match &e {
        PairError::InvalidCode => "invalid_code",
        PairError::InvalidInvite(_) => "invalid_invite",
        PairError::UnknownInvite => "unknown_invite",
        PairError::InviteRequired => "invite_required",
        PairError::Expired => "invite_expired",
        PairError::TooManyAttempts => "too_many_attempts",
        PairError::WrongCode => "wrong_code",
        PairError::UnsupportedVersion => "unsupported_version",
        PairError::NoHubFound => "no_hub_found",
        PairError::MultipleHubs => "multiple_hubs",
        PairError::Refused(_) => "pairing_refused",
        PairError::Protocol(_) | PairError::Wire(_) | PairError::Random(_) => "pairing_failed",
    };
    ApiError::new(StatusCode::BAD_REQUEST, code, e.to_string())
}

// ---------------------------------------------------------------- handlers

async fn get_status(State(s): State<SharedState>) -> ApiResult<Json<SyncStatus>> {
    status(&s).await.map(Json)
}

async fn hub_enable(State(s): State<SharedState>, _: Admin) -> ApiResult<Json<SyncStatus>> {
    let _guard = s.sync.transition.lock().await;
    match s.config().sync.role {
        MachineRole::Node => {
            return Err(ApiError::conflict(
                "paired_node",
                "this machine is paired with a hub; leave it before becoming a hub",
            ));
        }
        MachineRole::Hub if s.sync.service().is_some() => {}
        _ => {
            set_role(&s, MachineRole::Hub, None, None).await?;
            if let Err(e) = start_service(&s).await {
                set_role(&s, MachineRole::Standalone, None, None).await?;
                return Err(e);
            }
        }
    }
    // Also when already a hub: the portal settings may have changed.
    crate::portal::sync_with_config(&s)
        .await
        .map_err(|e| portal_failed(&e))?;
    status(&s).await.map(Json)
}

async fn hub_disable(State(s): State<SharedState>, _: Admin) -> ApiResult<Json<SyncStatus>> {
    let _guard = s.sync.transition.lock().await;
    if s.config().sync.role != MachineRole::Hub {
        return Err(ApiError::conflict("not_hub", "this machine is not a hub"));
    }
    stop_service(&s).await;
    crate::portal::stop(&s).await;
    set_role(&s, MachineRole::Standalone, None, None).await?;
    emit_status(&s);
    status(&s).await.map(Json)
}

async fn invite(State(s): State<SharedState>, _: Admin) -> ApiResult<Json<SyncInvite>> {
    let svc = s
        .sync
        .service()
        .filter(|svc| svc.is_hub())
        .ok_or_else(|| ApiError::conflict("not_hub", "enable the hub first"))?;
    let inv = svc
        .create_invite()
        .await
        .map_err(|e| sync_error("creating invite", e))?;
    Ok(Json(SyncInvite {
        invite: inv.invite,
        code: inv.code,
        uri: inv.uri,
        expires_at: inv.expires_at,
    }))
}

async fn join(
    State(s): State<SharedState>,
    _: Admin,
    ApiJson(body): ApiJson<JoinHub>,
) -> ApiResult<Json<SyncStatus>> {
    let _guard = s.sync.transition.lock().await;
    let body_allow_hub_control = body.allow_hub_control;
    let config = s.config();
    if config.sync.role != MachineRole::Standalone {
        return Err(ApiError::conflict(
            "already_synced",
            format!("this machine is already a {}", config.sync.role),
        ));
    }
    let (invite, code) = match blirp_sync::pair::parse_join_uri(&body.invite) {
        Some((i, c)) => (
            i,
            if body.code.trim().is_empty() {
                c
            } else {
                body.code
            },
        ),
        None => (body.invite, body.code),
    };
    let invite = Some(invite.trim().to_string()).filter(|i| !i.is_empty());
    if invite.is_none() && !config.sync.lan_discovery {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invite_required",
            "LAN discovery is off (sync.lan_discovery = false); pair with the hub's invite",
        ));
    }
    let joined = blirp_sync::service::join(
        &s.sync.identity()?,
        &config.sync.relay,
        config.sync.lan_discovery,
        invite.as_deref(),
        &code,
        MachineMeta {
            name: config.machine.name.clone(),
            os: std::env::consts::OS.to_string(),
        },
    )
    .await
    .map_err(|e| sync_error("pairing", e))?;
    let hub_id = joined.hub.id.to_string();
    let addr = serde_json::to_value(&joined.hub)
        .map_err(|e| ApiError::internal("saving hub address", e))?;
    let store = s.store.clone();
    blocking(move || Ok(store.set_setting(HUB_ADDR_KEY, &addr)?)).await?;
    set_role(
        &s,
        MachineRole::Node,
        Some(hub_id.clone()),
        body_allow_hub_control,
    )
    .await?;
    tracing::info!(hub = %hub_id, name = %joined.hub_meta.name, "paired with hub");
    start_service(&s).await?;
    status(&s).await.map(Json)
}

/// `POST /api/sync/join/preview`: the hub an invite points at, read from
/// the invite alone. Pairing verifies that the hub holds this id's key. Its
/// name is sent only after the pairing code is proven (§10), so it cannot
/// be shown before joining without spending one of the code's attempts.
async fn join_preview(
    _: Admin,
    ApiJson(body): ApiJson<JoinPreviewRequest>,
) -> ApiResult<Json<JoinPreview>> {
    let invite = blirp_sync::pair::parse_join_uri(&body.invite)
        .map(|(i, _)| i)
        .unwrap_or(body.invite);
    let invite = invite.trim();
    if invite.is_empty() {
        return Ok(Json(JoinPreview { hub_id: None }));
    }
    let ticket = Ticket::decode(invite).map_err(pair_error)?;
    Ok(Json(JoinPreview {
        hub_id: Some(ticket.addr.id.to_string()),
    }))
}

async fn list_devices(State(s): State<SharedState>) -> ApiResult<Json<Vec<Device>>> {
    let store = s.store.clone();
    Ok(Json(blocking(move || Ok(store.list_devices()?)).await?))
}

async fn browser_invite(State(s): State<SharedState>, _: Admin) -> ApiResult<Json<BrowserInvite>> {
    crate::portal::create_invite(&s).map(Json)
}

/// Revoke a paired machine on the hub: device + (replicated) machine row,
/// then drop its live connections.
async fn revoke_node(s: &SharedState, node_id: &str) -> ApiResult<()> {
    let store = s.store.clone();
    let id = node_id.to_string();
    blocking(move || {
        blirp_sync::service::revoke_machine(&store, &id)
            .map_err(|e| ApiError::internal("revoking the machine", e))
    })
    .await?;
    if let Some(svc) = s.sync.service() {
        svc.disconnect(node_id);
    }
    emit_status(s);
    Ok(())
}

async fn revoke_device(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    _: Admin,
) -> ApiResult<StatusCode> {
    let store = s.store.clone();
    let did = id.clone();
    let device = blocking(move || {
        store
            .get_device(&did)?
            .ok_or_else(|| ApiError::not_found("device"))
    })
    .await?;
    match (device.kind, device.node_id.clone()) {
        (DeviceKind::Machine, Some(node)) => revoke_node(&s, &node).await?,
        _ => {
            let store = s.store.clone();
            blocking(move || {
                Ok(store.upsert_device(&Device {
                    revoked: true,
                    ..device
                })?)
            })
            .await?;
        }
    }
    device_changed(&s, &id);
    Ok(StatusCode::NO_CONTENT)
}

async fn patch_device(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    _: Admin,
    ApiJson(body): ApiJson<PatchDevice>,
) -> ApiResult<Json<Device>> {
    let store = s.store.clone();
    let did = id.clone();
    let device = blocking(move || {
        let d = store
            .get_device(&did)?
            .ok_or_else(|| ApiError::not_found("device"))?;
        if body.can_control_terminals.is_none() && body.can_access_files.is_none() {
            return Err(ApiError::bad_request("nothing to change"));
        }
        let d = Device {
            can_control_terminals: body
                .can_control_terminals
                .unwrap_or(d.can_control_terminals),
            can_access_files: body.can_access_files.unwrap_or(d.can_access_files),
            ..d
        };
        store.upsert_device(&d)?;
        Ok(d)
    })
    .await?;
    // Live connections reopen with the new rights.
    device_changed(&s, &id);
    if let (DeviceKind::Machine, Some(node), Some(svc)) =
        (device.kind, device.node_id.as_deref(), s.sync.service())
    {
        svc.disconnect(node);
    }
    Ok(Json(device))
}

/// `DELETE /api/machines/:id`: on the hub, revoke that machine.
async fn revoke_machine(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    _: Admin,
) -> ApiResult<StatusCode> {
    let _guard = s.sync.transition.lock().await;
    let config = s.config();
    if id == s.machine.id {
        return Err(ApiError::bad_request("a machine cannot revoke itself"));
    }
    match config.sync.role {
        MachineRole::Hub => {
            let store = s.store.clone();
            let mid = id.clone();
            let known = blocking(move || {
                Ok(store.get_machine(&mid)?.is_some() || store.device_by_node_id(&mid)?.is_some())
            })
            .await?;
            if !known {
                return Err(ApiError::not_found("machine"));
            }
            revoke_node(&s, &id).await?;
        }
        _ => {
            return Err(ApiError::conflict(
                "not_hub",
                "only the hub can revoke other machines (a node leaves with POST /api/sync/leave)",
            ));
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// How long leaving keeps starting push batches, and the bound for the
/// whole exchange with the hub.
const LEAVE_PUSH: Duration = Duration::from_secs(5);
const LEAVE_TIMEOUT: Duration = Duration::from_secs(10);

/// `POST /api/sync/leave` (node): push what is still queued, have the hub
/// revoke this machine, then become standalone. An unreachable hub does not
/// block leaving; the answer then warns that the hub still lists this
/// machine.
async fn leave(State(s): State<SharedState>, _: Admin) -> ApiResult<Json<LeftHub>> {
    let _guard = s.sync.transition.lock().await;
    let config = s.config();
    let Some(hub) = config
        .sync
        .hub
        .clone()
        .filter(|_| config.sync.role == MachineRole::Node)
    else {
        return Err(ApiError::conflict(
            "not_node",
            "this machine is not paired with a hub",
        ));
    };
    let revoked = match s.sync.service() {
        None => Err("sync is not running".to_string()),
        Some(svc) => match tokio::time::timeout(LEAVE_TIMEOUT, svc.leave(LEAVE_PUSH)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("the hub did not answer in time".to_string()),
        },
    };
    stop_service(&s).await;
    let store = s.store.clone();
    let hub_id = hub.clone();
    // Read before the role change empties the outbox.
    let unsynced = blocking(move || {
        let n = store.pending_outbox(&hub_id)?;
        store.set_settings(&std::collections::BTreeMap::from([(
            HUB_ADDR_KEY.to_string(),
            None,
        )]))?;
        Ok(n)
    })
    .await?;
    // A consent for this hub, not for the next one joined.
    set_role(&s, MachineRole::Standalone, None, Some(false)).await?;
    emit_status(&s);
    let lost = if unsynced > 0 {
        format!(" {unsynced} change(s) made here had not reached the hub and were not synced.")
    } else {
        String::new()
    };
    let warning = match revoked {
        Ok(()) if lost.is_empty() => None,
        Ok(()) => Some(format!("The hub revoked this machine.{lost}")),
        Err(e) => {
            tracing::warn!(hub = %hub, error = %e, "left the hub without reaching it");
            Some(format!(
                "Could not reach the hub ({e}), so it still lists this machine as paired. \
                 Revoke it on the hub under Settings > Machines & Sync.{lost}"
            ))
        }
    };
    tracing::info!(hub = %hub, "left the hub");
    Ok(Json(LeftHub {
        status: status(&s).await?,
        warning,
    }))
}

/// Shutdown signal for one long-lived connection (WebSocket): flips on
/// daemon shutdown and, for a browser device, as soon as that device is
/// revoked or its permissions change (the client reconnects with its
/// current rights).
pub fn connection_shutdown(
    state: &SharedState,
    principal: &Principal,
) -> tokio::sync::watch::Receiver<bool> {
    let device = match (&principal.device, principal.admin) {
        (Some(d), _) => d.clone(),
        // Relayed by the sync proxy: its rights follow `sync.allow_hub_control`.
        (None, false) => PROXIED.to_string(),
        (None, true) => return state.shutdown.clone(),
    };
    let (tx, rx) = tokio::sync::watch::channel(*state.shutdown.borrow());
    let mut global = state.shutdown.clone();
    let mut changes = state.sync.device_changes.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                r = changes.recv() => match r {
                    Ok(id) if id != device => continue,
                    // Ours, or missed messages: close to be safe.
                    _ => break,
                },
                _ = global.changed() => break,
                _ = tx.closed() => return,
            }
        }
        let _ = tx.send(true);
    });
    rx
}

fn device_changed(s: &SharedState, id: &str) {
    // No live connections is fine.
    let _ = s.sync.device_changes.send(id.to_string());
}

/// `device_changes` key of connections relayed by the sync proxy.
const PROXIED: &str = "proxied";

/// `sync.allow_hub_control` changed: relayed connections (terminals, event
/// streams) close and reopen with the new rights.
pub(crate) fn proxied_rights_changed(s: &SharedState) {
    device_changed(s, PROXIED);
}

/// The machine a session runs on, when that is another machine.
pub async fn remote_machine_of(state: &SharedState, session: &str) -> ApiResult<Option<String>> {
    if let Some(m) = state.sync.remote_of(session) {
        return Ok(Some(m));
    }
    let store = state.store.clone();
    let id = session.to_string();
    let local = state.machine.id.clone();
    blocking(move || {
        Ok(store
            .get_session(&id)?
            .map(|s| s.machine_id)
            .filter(|m| *m != local))
    })
    .await
}
