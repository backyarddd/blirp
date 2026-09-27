//! Replication protocol (§10, `blirp/sync/1`).
//!
//! The node dials the hub and opens one bidirectional stream for strict
//! request/response: `hello` -> `welcome`, then any number of
//! `push {entries}` -> `push_ack {acked}` and `pull {after}` -> `page`.
//! A node leaving the hub sends `leave` -> `left`: the hub revokes it (the
//! connection's TLS-authenticated endpoint id, never an id from the message).
//! The hub opens one unidirectional stream on which it sends
//! `notify {head}` whenever `hub_log` grows, so the node pulls promptly,
//! and (protocol 2) `presence {machines}` whenever a machine connects or
//! disconnects and every minute while connected. Presence is runtime state:
//! it is never logged or replicated as a change.
//! Batches are at most [`MAX_BATCH_ENTRIES`] entries / [`MAX_BATCH_BYTES`].
//! Every apply and its cursor move happen in one SQLite transaction
//! (see `blirp_core::store::sync`), so a crash at any point resumes cleanly
//! and re-sent entries are ignored.

use crate::wire::{MAX_CONTROL_FRAME, MAX_FRAME, read_frame, write_frame};
use crate::{Result, SyncError, blocking};
use blirp_core::model::Machine;
use blirp_core::store::{HubPage, Store, WireEntry};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

pub const MAX_BATCH_ENTRIES: usize = 500;
pub const MAX_BATCH_BYTES: usize = 4 << 20;
/// How often the local outbox is checked for new writes.
// Polling; a store-level change notification would cut latency.
pub const OUTBOX_POLL: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Error code of a pull whose cursor went back behind the compacted log.
pub const RESYNC_REQUIRED: &str = "resync_required";

/// Whether a machine has a live sync connection to the hub, as the hub sees
/// it. `last_seen`: connect, disconnect or the latest minute while connected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    pub machine_id: String,
    pub online: bool,
    pub last_seen: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeMsg {
    Hello {
        versions: Vec<u32>,
        machine: Machine,
    },
    Push {
        entries: Vec<WireEntry>,
    },
    Pull {
        after: i64,
    },
    /// This machine leaves the hub: revoke it.
    Leave,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubMsg {
    Welcome {
        version: u32,
        hub_machine_id: String,
        head: i64,
    },
    PushAck {
        acked: i64,
    },
    Page {
        page: HubPage,
    },
    Notify {
        head: i64,
    },
    /// Every paired machine's presence, the hub included (protocol 2).
    Presence {
        machines: Vec<Presence>,
    },
    /// Answer to `leave`: the machine is revoked.
    Left,
    Error {
        code: String,
        message: String,
        /// With `unsupported_version`: the sync versions the hub speaks
        /// (absent from older hubs, which speak only older ones).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        versions: Option<Vec<u32>>,
    },
}

fn remote_err(code: String, message: String) -> SyncError {
    SyncError::Remote { code, message }
}

/// How a node's sync session with the hub ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubSessionEnd {
    Disconnected,
    /// The node left the hub and is now revoked.
    Left,
}

/// Hub side of one node connection. Returns when the node disconnects or
/// leaves. `on_exchange(logged)` runs after every answered request;
/// `logged` is true when a push added rows (to wake other nodes).
pub async fn serve_hub(
    conn: Connection,
    store: Arc<Store>,
    own_id: String,
    node_id: String,
    mut head: watch::Receiver<i64>,
    mut presence: watch::Receiver<Vec<Presence>>,
    on_exchange: impl Fn(bool) + Send + Sync + 'static,
) -> Result<HubSessionEnd> {
    let (mut send, mut recv) = conn.accept_bi().await.map_err(SyncError::connection)?;
    let hello: NodeMsg = read_frame(&mut recv, MAX_FRAME).await?;
    let NodeMsg::Hello { versions, machine } = hello else {
        return Err(SyncError::Protocol("expected hello".into()));
    };
    let Some(version) = crate::negotiate_from(crate::SYNC_VERSIONS, &versions) else {
        write_frame(
            &mut send,
            &HubMsg::Error {
                code: "unsupported_version".into(),
                message: format!(
                    "the hub speaks sync protocol {:?}, this machine {versions:?}: \
                     update blirp on both to the same release",
                    crate::SYNC_VERSIONS
                ),
                versions: Some(crate::SYNC_VERSIONS.to_vec()),
            },
        )
        .await?;
        return Err(SyncError::ReleaseMismatch {
            peer_older: crate::sync_peer_older(&versions),
        });
    };
    if machine.id != node_id {
        return Err(SyncError::Protocol("hello names another machine".into()));
    }
    let current = *head.borrow_and_update();
    write_frame(
        &mut send,
        &HubMsg::Welcome {
            version,
            hub_machine_id: own_id.clone(),
            head: current,
        },
    )
    .await?;

    // Notifications on their own stream so they never interleave with replies.
    let mut notify = conn.open_uni().await.map_err(SyncError::connection)?;
    write_frame(&mut notify, &HubMsg::Notify { head: current }).await?;
    let presence_frames = version >= 2;
    let notifier = tokio::spawn(async move {
        if presence_frames {
            let machines = presence.borrow_and_update().clone();
            if write_frame(&mut notify, &HubMsg::Presence { machines })
                .await
                .is_err()
            {
                return;
            }
        }
        loop {
            // Only `changed()` waits in the select: frame writes are not
            // cancel safe.
            let msg = tokio::select! {
                r = head.changed() => match r {
                    Ok(()) => HubMsg::Notify { head: *head.borrow_and_update() },
                    Err(_) => break,
                },
                r = presence.changed(), if presence_frames => match r {
                    Ok(()) => HubMsg::Presence { machines: presence.borrow_and_update().clone() },
                    Err(_) => break,
                },
            };
            if write_frame(&mut notify, &msg).await.is_err() {
                break;
            }
        }
    });

    let result = async {
        loop {
            let msg: NodeMsg = match read_frame(&mut recv, MAX_FRAME).await {
                Ok(m) => m,
                Err(crate::wire::WireError::Closed) => return Ok(HubSessionEnd::Disconnected),
                Err(e) => return Err(e.into()),
            };
            let reply = match msg {
                NodeMsg::Push { entries } => {
                    if entries.len() > MAX_BATCH_ENTRIES {
                        return Err(SyncError::Protocol(format!(
                            "push of {} entries exceeds {MAX_BATCH_ENTRIES}",
                            entries.len()
                        )));
                    }
                    let (st, own, node) = (store.clone(), own_id.clone(), node_id.clone());
                    let out = blocking(move || Ok(st.hub_ingest(&own, &node, &entries)?)).await?;
                    on_exchange(out.inserted > 0);
                    HubMsg::PushAck { acked: out.acked }
                }
                NodeMsg::Pull { after } => {
                    let (st, own, node) = (store.clone(), own_id.clone(), node_id.clone());
                    let page = blocking(move || {
                        // Our own writes must be logged before we page.
                        st.hub_flush_own(&own)?;
                        // The node has applied everything up to `after`:
                        // compaction may go that far for it.
                        if !st.hub_record_pull(&node, after)? {
                            return Ok(None);
                        }
                        Ok(Some(st.hub_page(
                            &node,
                            after,
                            MAX_BATCH_ENTRIES,
                            MAX_BATCH_BYTES,
                        )?))
                    })
                    .await?;
                    on_exchange(false);
                    match page {
                        Some(page) => HubMsg::Page { page },
                        None => {
                            tracing::warn!(node = %node_id, after, "node's sync position went back behind the compacted log");
                            HubMsg::Error {
                                code: RESYNC_REQUIRED.into(),
                                message: "this machine's sync position went back behind the hub's \
                                          compacted log (its database lost recent changes); \
                                          resync needed: leave the hub and pair again"
                                    .into(),
                                versions: None,
                            }
                        }
                    }
                }
                NodeMsg::Leave => {
                    // Only ever the authenticated peer of this connection.
                    let (st, node) = (store.clone(), node_id.clone());
                    blocking(move || crate::service::revoke_machine(&st, &node)).await?;
                    on_exchange(false);
                    write_frame(&mut send, &HubMsg::Left).await?;
                    let _ = send.finish();
                    // Dropping the connection now could discard the unsent
                    // frame; the node closes it once it has read `left`.
                    let _ = tokio::time::timeout(Duration::from_secs(1), conn.closed()).await;
                    return Ok(HubSessionEnd::Left);
                }
                NodeMsg::Hello { .. } => {
                    return Err(SyncError::Protocol("duplicate hello".into()));
                }
            };
            write_frame(&mut send, &reply).await?;
        }
    }
    .await;
    notifier.abort();
    result
}

/// Node side of a connected sync session: handshake, then push/pull until
/// the connection fails or `shutdown` flips. `on_synced` runs after every
/// successful exchange, `on_presence` for every presence frame (only from a
/// hub speaking protocol 2).
pub async fn run_node(
    conn: &Connection,
    store: Arc<Store>,
    machine: Machine,
    hub_id: String,
    mut shutdown: watch::Receiver<bool>,
    on_synced: impl Fn() + Send + Sync,
    on_presence: impl Fn(Vec<Presence>) + Send + Sync + 'static,
) -> Result<()> {
    let (mut send, mut recv) = handshake(conn, machine, &hub_id).await?;

    // Hub notifications arrive on a uni stream read by its own task (frame
    // reads are not cancel safe, so they must not sit in a select).
    let (head_tx, mut head_rx) = watch::channel(0i64);
    let uni_conn = conn.clone();
    let notifier = tokio::spawn(async move {
        let Ok(mut uni) = uni_conn.accept_uni().await else {
            return;
        };
        loop {
            match read_frame::<RecvStream, HubMsg>(&mut uni, MAX_FRAME).await {
                Ok(HubMsg::Notify { head }) => {
                    head_tx.send_replace(head);
                }
                Ok(HubMsg::Presence { machines }) => on_presence(machines),
                _ => break,
            }
        }
    });

    let result = async {
        let mut tick = tokio::time::interval(OUTBOX_POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut need_pull = true;
        loop {
            push_pending(&mut send, &mut recv, &store, &hub_id, None).await?;
            if need_pull {
                pull_all(&mut send, &mut recv, &store, &hub_id).await?;
                need_pull = false;
            }
            on_synced();
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let st = store.clone();
                        let hub = hub_id.clone();
                        let pending = blocking(move || {
                            Ok(st.outbox_head()? > st.sync_cursors(&hub)?.last_pushed_origin_seq)
                        })
                        .await?;
                        if pending {
                            break;
                        }
                    }
                    changed = head_rx.changed() => {
                        if changed.is_err() {
                            return Err(SyncError::Connection("hub notification stream closed".into()));
                        }
                        let head = *head_rx.borrow_and_update();
                        let st = store.clone();
                        let hub = hub_id.clone();
                        let pulled = blocking(move || Ok(st.sync_cursors(&hub)?.last_pulled_hub_seq)).await?;
                        if head > pulled {
                            need_pull = true;
                            break;
                        }
                    }
                    _ = shutdown.changed() => return Ok(()),
                    reason = conn.closed() => return Err(SyncError::connection(reason)),
                }
            }
        }
    }
    .await;
    notifier.abort();
    result
}

/// Node: leave the hub. Pushes what is still queued, starting new batches
/// until `push_until` (what is left stays in the outbox), then asks the hub
/// to revoke this machine.
pub async fn leave(
    conn: &Connection,
    store: Arc<Store>,
    machine: Machine,
    hub_id: &str,
    push_until: tokio::time::Instant,
) -> Result<()> {
    let (mut send, mut recv) = handshake(conn, machine, hub_id).await?;
    push_pending(&mut send, &mut recv, &store, hub_id, Some(push_until)).await?;
    write_frame(&mut send, &NodeMsg::Leave).await?;
    match timed(read_frame(&mut recv, MAX_CONTROL_FRAME)).await? {
        HubMsg::Left => Ok(()),
        HubMsg::Error { code, message, .. } => Err(remote_err(code, message)),
        other => Err(SyncError::Protocol(format!("unexpected {other:?}"))),
    }
}

/// Node: open the request stream and exchange hello/welcome with `hub_id`.
async fn handshake(
    conn: &Connection,
    machine: Machine,
    hub_id: &str,
) -> Result<(SendStream, RecvStream)> {
    let (mut send, mut recv) = conn.open_bi().await.map_err(SyncError::connection)?;
    write_frame(
        &mut send,
        &NodeMsg::Hello {
            versions: crate::SYNC_VERSIONS.to_vec(),
            machine,
        },
    )
    .await?;
    match timed(read_frame(&mut recv, MAX_CONTROL_FRAME)).await? {
        HubMsg::Welcome { hub_machine_id, .. } if hub_machine_id == hub_id => Ok((send, recv)),
        HubMsg::Welcome { hub_machine_id, .. } => Err(SyncError::Protocol(format!(
            "expected hub {hub_id}, got {hub_machine_id}"
        ))),
        // A hub that sends no `versions` predates them: it speaks older ones.
        HubMsg::Error { code, versions, .. } if code == "unsupported_version" => {
            Err(SyncError::ReleaseMismatch {
                peer_older: crate::sync_peer_older(&versions.unwrap_or_default()),
            })
        }
        HubMsg::Error { code, message, .. } => Err(remote_err(code, message)),
        other => Err(SyncError::Protocol(format!("unexpected {other:?}"))),
    }
}

async fn timed<T>(
    f: impl std::future::Future<Output = Result<T, crate::wire::WireError>>,
) -> Result<T> {
    tokio::time::timeout(REQUEST_TIMEOUT, f)
        .await
        .map_err(|_| SyncError::Connection("hub did not answer in time".into()))?
        .map_err(Into::into)
}

/// Push the outbox in batches. With `until`, no batch starts after it.
async fn push_pending(
    send: &mut SendStream,
    recv: &mut RecvStream,
    store: &Arc<Store>,
    hub_id: &str,
    until: Option<tokio::time::Instant>,
) -> Result<()> {
    loop {
        let st = store.clone();
        let hub = hub_id.to_string();
        let batch = blocking(move || {
            let after = st.sync_cursors(&hub)?.last_pushed_origin_seq;
            Ok(st.outbox_batch(after, MAX_BATCH_ENTRIES, MAX_BATCH_BYTES)?)
        })
        .await?;
        let Some(last) = batch.last().map(|e| e.origin_seq) else {
            return Ok(());
        };
        if until.is_some_and(|u| tokio::time::Instant::now() >= u) {
            return Ok(());
        }
        write_frame(send, &NodeMsg::Push { entries: batch }).await?;
        match timed(read_frame(recv, MAX_CONTROL_FRAME)).await? {
            HubMsg::PushAck { acked } if acked >= last => {
                let st = store.clone();
                let hub = hub_id.to_string();
                blocking(move || Ok(st.set_pushed_cursor(&hub, acked)?)).await?;
            }
            HubMsg::PushAck { acked } => {
                return Err(SyncError::Protocol(format!(
                    "hub acked {acked}, expected at least {last}"
                )));
            }
            HubMsg::Error { code, message, .. } => return Err(remote_err(code, message)),
            other => return Err(SyncError::Protocol(format!("unexpected {other:?}"))),
        }
    }
}

async fn pull_all(
    send: &mut SendStream,
    recv: &mut RecvStream,
    store: &Arc<Store>,
    hub_id: &str,
) -> Result<()> {
    loop {
        let st = store.clone();
        let hub = hub_id.to_string();
        let after = blocking(move || Ok(st.sync_cursors(&hub)?.last_pulled_hub_seq)).await?;
        write_frame(send, &NodeMsg::Pull { after }).await?;
        let page = match timed(read_frame(recv, MAX_FRAME)).await? {
            HubMsg::Page { page } => page,
            HubMsg::Error { code, message, .. } => return Err(remote_err(code, message)),
            other => return Err(SyncError::Protocol(format!("unexpected {other:?}"))),
        };
        if page.entries.len() > MAX_BATCH_ENTRIES {
            return Err(SyncError::Protocol("oversized page".into()));
        }
        let more = page.more;
        let st = store.clone();
        let hub = hub_id.to_string();
        blocking(move || Ok(st.node_apply_pull(&hub, &page)?)).await?;
        if !more {
            return Ok(());
        }
    }
}
