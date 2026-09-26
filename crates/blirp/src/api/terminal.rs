//! `/api/terminals/:id/ws` (§6). Framing:
//! - server -> client: text frames are JSON [`TerminalServerMessage`]
//!   (`snapshot` first, `resize`, `exit`); binary frames are raw PTY output.
//! - client -> server: binary frames are raw input bytes; text frames are
//!   JSON [`TerminalClientMessage`] (`input {data}` or `resize {cols, rows}`).

use super::{ApiError, ApiResult};
use crate::pty::{Snapshot, TermEvent, Terminal};
use crate::state::SharedState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use blirp_core::model::{TerminalClientMessage, TerminalServerMessage};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

pub async fn attach(
    State(s): State<SharedState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let term = s.terminals.get(&id).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "terminal_not_found",
            "session has no live terminal",
        )
    })?;
    let shutdown = s.shutdown.clone();
    Ok(ws
        .max_message_size(1024 * 1024)
        .on_upgrade(move |socket| session(term, socket, shutdown)))
}

fn text(msg: &TerminalServerMessage) -> Option<Message> {
    match serde_json::to_string(msg) {
        Ok(t) => Some(Message::Text(t.into())),
        Err(e) => {
            tracing::error!(error = %e, "serializing terminal message");
            None
        }
    }
}

fn snapshot_msg(s: Snapshot) -> Option<Message> {
    text(&TerminalServerMessage::Snapshot {
        cols: s.cols,
        rows: s.rows,
        data: s.data,
    })
}

async fn send(socket: &mut WebSocket, msg: Option<Message>) -> bool {
    match msg {
        Some(m) => socket.send(m).await.is_ok(),
        None => true,
    }
}

async fn session(
    term: Arc<Terminal>,
    mut socket: WebSocket,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let (snap, mut rx, exited) = term.attach();
    if !send(&mut socket, snapshot_msg(snap)).await {
        return;
    }
    if let Some(info) = exited {
        let msg = text(&TerminalServerMessage::Exit {
            status: info.status,
            exit_code: Some(info.code),
        });
        send(&mut socket, msg).await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    }
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let ok = match ev {
                    Ok(TermEvent::Data(bytes)) => socket.send(Message::Binary(bytes)).await.is_ok(),
                    Ok(TermEvent::Resize { cols, rows }) => {
                        send(&mut socket, text(&TerminalServerMessage::Resize { cols, rows })).await
                    }
                    Ok(TermEvent::Exit(info)) => {
                        let msg = text(&TerminalServerMessage::Exit {
                            status: info.status,
                            exit_code: Some(info.code),
                        });
                        send(&mut socket, msg).await;
                        break;
                    }
                    // Too slow to keep up: start over from a fresh snapshot.
                    Err(RecvError::Lagged(_)) => {
                        let (snap, new_rx, _) = term.attach();
                        rx = new_rx;
                        send(&mut socket, snapshot_msg(snap)).await
                    }
                    Err(RecvError::Closed) => false,
                };
                if !ok {
                    break;
                }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(b))) => term.write(b.to_vec()),
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<TerminalClientMessage>(&t) {
                    Ok(TerminalClientMessage::Input { data }) => term.write(data.into_bytes()),
                    Ok(TerminalClientMessage::Resize { cols, rows }) => {
                        if let Err(e) = term.resize(cols, rows) {
                            tracing::debug!(session = %term.session_id, error = %e, "resize rejected");
                        }
                    }
                    Err(e) => {
                        tracing::debug!(session = %term.session_id, error = %e, "ignoring malformed terminal frame");
                    }
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            },
            _ = shutdown.changed() => break,
        }
    }
    // Best effort: the peer may already be gone.
    let _ = socket.send(Message::Close(None)).await;
}
