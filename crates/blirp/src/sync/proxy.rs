//! HTTP over `blirp/proxy/1` streams: serving proxied requests with this
//! daemon's own router, forwarding requests and terminal WebSockets to
//! other machines.

use crate::api::{ApiError, ApiResult, Principal};
use crate::state::SharedState;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket};
use axum::http::{Method, Request, StatusCode, header};
use axum::response::Response;
use blirp_core::model::{ErrorBody, LaunchSession, MachineRole, Session};
use blirp_sync::SyncError;
use blirp_sync::proxy::{ProxyPrincipal, ProxyStream};
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

/// Largest proxied response body accepted.
const MAX_BODY: usize = 16 << 20;
/// Same cap as local terminal frames (§6).
const MAX_WS_FRAME: usize = 1 << 20;

pub type RemoteTerminal = WebSocketStream<ProxyStream>;

/// Serve one proxied stream as an HTTP/1.1 connection to the local API.
pub(super) async fn serve(state: SharedState, stream: ProxyStream, p: ProxyPrincipal) {
    // A node is controlled from elsewhere only when its user opted in (§10).
    let sync = state.config().sync;
    let opted_in = sync.role != MachineRole::Node || sync.allow_hub_control;
    let principal = Principal {
        control: p.control && opted_in,
        admin: false,
        device: None,
        label: p.via,
    };
    // Built per stream: caching it in the state would make the state own
    // itself (router -> state -> router).
    let router = crate::api::proxy_router(state).layer(axum::Extension(principal));
    let service = hyper_util::service::TowerToHyperService::new(router);
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await
    {
        tracing::debug!(error = %e, "proxied connection ended");
    }
}

fn proxy_error(e: SyncError) -> ApiError {
    match e {
        SyncError::Unavailable(m) => ApiError::conflict("machine_unreachable", m),
        SyncError::Remote { code, message } if code == "machine_offline" => {
            ApiError::conflict("machine_unreachable", message)
        }
        other => ApiError::new(
            StatusCode::BAD_GATEWAY,
            "proxy_failed",
            format!("remote machine did not answer: {other}"),
        ),
    }
}

async fn open(state: &SharedState, machine: &str, principal: &Principal) -> ApiResult<ProxyStream> {
    let svc = state.sync.service().ok_or_else(|| {
        ApiError::conflict(
            "machine_unreachable",
            "this machine is not connected to a hub",
        )
    })?;
    let via = format!("{} on {}", principal.label, state.machine.name);
    svc.open_proxy(machine, principal.control, &via)
        .await
        .map_err(proxy_error)
}

/// Send one request to `machine`'s API and return its response. Only the
/// method, path and JSON body are forwarded (never local credentials).
pub async fn forward(
    state: &SharedState,
    machine: &str,
    principal: &Principal,
    method: Method,
    path: &str,
    json: Option<Vec<u8>>,
) -> ApiResult<Response> {
    let stream = open(state, machine, principal).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| proxy_error(SyncError::Connection(e.to_string())))?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!(error = %e, "proxy client connection ended");
        }
    });
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "blirp.remote");
    let body = match json {
        Some(b) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(b)
        }
        None => Body::empty(),
    };
    let req = req
        .body(body)
        .map_err(|e| ApiError::internal("building proxied request", e))?;
    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| proxy_error(SyncError::Connection(e.to_string())))?;
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(Body::new(body), MAX_BODY)
        .await
        .map_err(|e| proxy_error(SyncError::Connection(e.to_string())))?;
    let mut out = Response::new(Body::from(bytes));
    *out.status_mut() = parts.status;
    if let Some(ct) = parts.headers.get(header::CONTENT_TYPE) {
        out.headers_mut().insert(header::CONTENT_TYPE, ct.clone());
    }
    Ok(out)
}

/// `POST /api/sessions` with `machine` set to another machine.
pub async fn launch_remote(
    state: &SharedState,
    machine: &str,
    principal: &Principal,
    body: &LaunchSession,
) -> ApiResult<Response> {
    let json = serde_json::to_vec(body).map_err(|e| ApiError::internal("encoding launch", e))?;
    let resp = forward(
        state,
        machine,
        principal,
        Method::POST,
        "/api/sessions",
        Some(json),
    )
    .await?;
    if !resp.status().is_success() {
        return Ok(resp);
    }
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|e| ApiError::internal("reading proxied response", e))?;
    match serde_json::from_slice::<Session>(&bytes) {
        Ok(session) => state.sync.remember_remote(&session.id, machine),
        Err(e) => tracing::warn!(error = %e, "remote launch returned an unexpected body"),
    }
    Ok(Response::from_parts(parts, Body::from(bytes)))
}

/// Open the terminal WebSocket of `session` on `machine` through the hub.
pub async fn connect_terminal(
    state: &SharedState,
    machine: &str,
    session: &str,
    principal: &Principal,
) -> ApiResult<RemoteTerminal> {
    let stream = open(state, machine, principal).await?;
    let req = format!("ws://blirp.remote/api/terminals/{session}/ws")
        .into_client_request()
        .map_err(|e| ApiError::internal("building terminal request", e))?;
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_FRAME))
        .max_frame_size(Some(MAX_WS_FRAME));
    match tokio_tungstenite::client_async_with_config(req, stream, Some(config)).await {
        Ok((ws, _)) => Ok(ws),
        Err(tungstenite::Error::Http(resp)) => {
            let status = resp.status();
            let parsed = resp
                .body()
                .as_deref()
                .and_then(|b| serde_json::from_slice::<ErrorBody>(b).ok());
            let message = parsed.as_ref().map_or_else(
                || format!("remote answered {status}"),
                |e| e.error.message.clone(),
            );
            let code = match parsed.as_ref().map(|e| e.error.code.as_str()) {
                Some("terminal_not_found") => "terminal_not_found",
                Some("unauthorized") => "unauthorized",
                _ => "remote_error",
            };
            Err(ApiError::new(status, code, message))
        }
        Err(e) => Err(proxy_error(SyncError::Connection(e.to_string()))),
    }
}

fn to_remote(m: Message) -> Option<tungstenite::Message> {
    match m {
        Message::Text(t) => Some(tungstenite::Message::Text(t.as_str().into())),
        Message::Binary(b) => Some(tungstenite::Message::Binary(b)),
        // Pings are answered per hop; close is handled by the caller.
        Message::Ping(_) | Message::Pong(_) | Message::Close(_) => None,
    }
}

fn to_local(m: tungstenite::Message) -> Option<Message> {
    match m {
        tungstenite::Message::Text(t) => Some(Message::Text(t.as_str().into())),
        tungstenite::Message::Binary(b) => Some(Message::Binary(b)),
        _ => None,
    }
}

/// Relay frames between a local client and a remote terminal. Input and
/// resize frames are dropped for callers without control (the remote side
/// enforces the same via the proxy principal).
pub async fn relay_terminal(
    mut socket: WebSocket,
    remote: RemoteTerminal,
    control: bool,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    use tungstenite::protocol::CloseFrame;
    use tungstenite::protocol::frame::coding::CloseCode;
    let (mut rtx, mut rrx) = remote.split();
    // Who ended the relay: the local client (None) or the remote side /
    // this daemon (the close to pass on to the local client).
    let ours: Option<(u16, String)> = loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break None,
                Some(Ok(m)) => {
                    if !control {
                        continue;
                    }
                    if let Some(m) = to_remote(m)
                        && rtx.send(m).await.is_err()
                    {
                        break Some((1011, "remote terminal unreachable".into()));
                    }
                }
            },
            msg = rrx.next() => match msg {
                // The remote daemon's own close (exit, shutdown) goes on unchanged.
                Some(Ok(tungstenite::Message::Close(Some(f)))) => {
                    break Some((u16::from(f.code), f.reason.as_str().to_string()));
                }
                Some(Ok(tungstenite::Message::Close(None))) => break Some((1000, String::new())),
                None | Some(Err(_)) => break Some((1011, "remote terminal unreachable".into())),
                Some(Ok(m)) => {
                    if let Some(m) = to_local(m)
                        && socket.send(m).await.is_err()
                    {
                        break None;
                    }
                }
            },
            _ = shutdown.changed() => {
                let (code, reason) = crate::api::WS_GOING_AWAY;
                break Some((code, reason.to_string()));
            }
        }
    };
    // Best effort: the remote side may already be gone.
    let frame = CloseFrame {
        code: CloseCode::Normal,
        reason: "".into(),
    };
    let _ = rtx.send(tungstenite::Message::Close(Some(frame))).await;
    crate::api::close_ws(&mut socket, ours.as_ref().map(|(c, r)| (*c, r.as_str()))).await;
}
