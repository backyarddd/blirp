//! `/api/terminals/:id/ws` (§6). Framing:
//! - server -> client: text frames are JSON [`TerminalServerMessage`]
//!   (`snapshot` first, `resize`, `exit`); binary frames are raw PTY output.
//! - client -> server: binary frames are raw input bytes; text frames are
//!   JSON [`TerminalClientMessage`] (`input {data}` or `resize {cols, rows}`).
//!
//! Callers without terminal control (browser devices without
//! `can_control_terminals`, proxied read-only requests) get a view-only
//! stream: their input and resize frames are dropped. A session running on
//! another machine is attached through the hub (see `crate::sync`).

use super::{ApiError, ApiPath, ApiResult, Principal};
use crate::pty::{ExitInfo, Snapshot, TermEvent, Terminal};
use crate::state::SharedState;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use blirp_core::model::{TerminalClientMessage, TerminalServerMessage};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

pub async fn attach(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    principal: Principal,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let shutdown = crate::sync::connection_shutdown(&s, &principal);
    let ws = ws.max_message_size(1024 * 1024);
    let Some(term) = s.terminals.get(&id) else {
        if let Some(machine) = crate::sync::remote_machine_of(&s, &id).await? {
            let remote = crate::sync::connect_terminal(&s, &machine, &id, &principal).await?;
            return Ok(ws.on_upgrade(move |socket| {
                crate::sync::relay_terminal(socket, remote, principal.control, shutdown)
            }));
        }
        // The process ended between the client seeing it live and attaching
        // (the exit is recorded before the terminal leaves the registry):
        // report the exit like a live terminal would, so the client shows it
        // instead of retrying a 404.
        let store = s.store.clone();
        let sid = id.clone();
        let ended = super::blocking(move || Ok(store.get_session(&sid)?))
            .await?
            .filter(|sess| !sess.status.is_live());
        if let Some(sess) = ended {
            let exit = text(&TerminalServerMessage::Exit {
                status: sess.status,
                exit_code: sess.exit_code,
            });
            return Ok(ws.on_upgrade(move |mut socket| async move {
                if send(&mut socket, exit).await {
                    super::close_ws(&mut socket, Some(super::WS_DONE)).await;
                }
            }));
        }
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "terminal_not_found",
            "session has no live terminal",
        ));
    };
    let control = principal.control;
    Ok(ws.on_upgrade(move |socket| session(term, socket, shutdown, control)))
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
        windows_pty: crate::pty::windows_pty(),
    })
}

async fn send(socket: &mut WebSocket, msg: Option<Message>) -> bool {
    match msg {
        Some(m) => socket.send(m).await.is_ok(),
        None => true,
    }
}

fn exit_msg(info: ExitInfo) -> Option<Message> {
    text(&TerminalServerMessage::Exit {
        status: info.status,
        exit_code: info.code,
    })
}

/// Frames that (re)start a client: a snapshot, plus `exit` when the process
/// already ended (`true`: the socket closes after them). Used on attach and
/// whenever the client fell behind.
fn start_frames(term: &Terminal) -> (Vec<Message>, broadcast::Receiver<TermEvent>, bool) {
    let (snap, rx, exited) = term.attach();
    let mut frames: Vec<Message> = snapshot_msg(snap).into_iter().collect();
    if let Some(info) = exited {
        frames.extend(exit_msg(info));
    }
    (frames, rx, exited.is_some())
}

/// Send `frames`; false when the peer is gone.
async fn send_all(socket: &mut WebSocket, frames: Vec<Message>) -> bool {
    for f in frames {
        if socket.send(f).await.is_err() {
            return false;
        }
    }
    true
}

/// Distinguishes attached clients, so a resize is not echoed to its sender.
static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);

/// How a terminal stream ended.
enum End {
    /// The client closed the socket (or vanished).
    Client,
    /// The process exited, or the terminal went away.
    Exited,
    /// The daemon is shutting down or the client's access changed.
    Shutdown,
}

async fn session(
    term: Arc<Terminal>,
    mut socket: WebSocket,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    control: bool,
) {
    let me = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
    let (mut frames, mut rx, ended) = start_frames(&term);
    if !control {
        // Right after the snapshot: this client's input is ignored.
        frames.extend(text(&TerminalServerMessage::Readonly));
    }
    let end = if !send_all(&mut socket, frames).await {
        End::Client
    } else if ended {
        End::Exited
    } else {
        stream(&term, &mut socket, &mut rx, &mut shutdown, control, me).await
    };
    match end {
        End::Client => super::close_ws(&mut socket, None).await,
        End::Exited => super::close_ws(&mut socket, Some(super::WS_DONE)).await,
        End::Shutdown => super::close_ws(&mut socket, Some(super::WS_GOING_AWAY)).await,
    }
}

async fn stream(
    term: &Arc<Terminal>,
    socket: &mut WebSocket,
    rx: &mut broadcast::Receiver<TermEvent>,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
    control: bool,
    me: u64,
) -> End {
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let ok = match ev {
                    Ok(TermEvent::Data(bytes)) => socket.send(Message::Binary(bytes)).await.is_ok(),
                    Ok(TermEvent::Resize { by, .. }) if by == me => true,
                    Ok(TermEvent::Resize { cols, rows, .. }) => {
                        send(socket, text(&TerminalServerMessage::Resize { cols, rows })).await
                    }
                    Ok(TermEvent::Exit(info)) => {
                        return if send(socket, exit_msg(info)).await {
                            End::Exited
                        } else {
                            End::Client
                        };
                    }
                    // Too slow to keep up: start over from a fresh snapshot,
                    // which ends the stream when the process exited meanwhile
                    // (its exit event may be among the dropped ones).
                    Err(RecvError::Lagged(_)) => {
                        let (frames, new_rx, ended) = start_frames(term);
                        *rx = new_rx;
                        if !send_all(socket, frames).await {
                            return End::Client;
                        }
                        if ended {
                            return End::Exited;
                        }
                        true
                    }
                    Err(RecvError::Closed) => return End::Exited,
                };
                if !ok {
                    return End::Client;
                }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(_) | Message::Text(_))) if !control => {}
                Some(Ok(Message::Binary(b))) => term.client_input(b.to_vec()),
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<TerminalClientMessage>(&t) {
                    Ok(TerminalClientMessage::Input { data }) => term.client_input(data.into_bytes()),
                    Ok(TerminalClientMessage::Resize { cols, rows }) => {
                        // ResizePseudoConsole can wait for the PTY reader:
                        // never on an async worker.
                        let t = term.clone();
                        match tokio::task::spawn_blocking(move || t.resize(cols, rows, me)).await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                tracing::debug!(session = %term.session_id, error = %e, "resize rejected");
                            }
                            Err(e) => {
                                tracing::error!(session = %term.session_id, error = %e, "resize task failed");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(session = %term.session_id, error = %e, "ignoring malformed terminal frame");
                    }
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return End::Client,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            },
            _ = shutdown.changed() => return End::Shutdown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::SpawnRequest;
    use std::time::{Duration, Instant};

    fn frame(m: &Message) -> TerminalServerMessage {
        match m {
            Message::Text(t) => serde_json::from_str(t).unwrap(),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    // A client that fell behind after the process exited must get the exit
    // and a closed stream, not a snapshot and a silent socket.
    #[test]
    fn resync_after_exit_reports_the_exit() {
        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd.exe", &["/d", "/c", "exit 3"])
        } else {
            ("/bin/sh", &["-c", "exit 3"])
        };
        let term = Terminal::spawn(
            "t-resync",
            SpawnRequest {
                program: program.into(),
                args: args.iter().map(Into::into).collect(),
                cwd: std::env::temp_dir(),
                env: Vec::new(),
                env_remove: Vec::new(),
                cols: 80,
                rows: 24,
            },
            |_, _| {},
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !term.has_exited() {
            assert!(Instant::now() < deadline, "process did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
        let (frames, _rx, ended) = start_frames(&term);
        assert!(ended);
        assert_eq!(frames.len(), 2);
        assert!(matches!(
            frame(&frames[0]),
            TerminalServerMessage::Snapshot { .. }
        ));
        assert!(matches!(
            frame(&frames[1]),
            TerminalServerMessage::Exit {
                exit_code: Some(3),
                ..
            }
        ));
    }
}
