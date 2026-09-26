//! Sync runtime: the iroh endpoint, the accept loop routing by ALPN, the
//! hub's peer table and the node's reconnecting session with its hub.

use crate::pair::{self, InviteBook, MachineMeta, PairIds, Ticket};
use crate::proxy::{self, ProxyOpen, ProxyPrincipal, ProxyReply, ProxyStream};
use crate::wire::{MAX_CONTROL_FRAME, read_frame, write_frame};
use crate::{
    ALPN_PAIR, ALPN_PROXY, ALPN_SYNC, HUB_MARKER, MDNS_SERVICE, Result, SyncError, blocking, repl,
};
use blirp_core::model::{Device, DeviceKind, Machine, MachineRole};
use blirp_core::store::{Change, Store};
use iroh::endpoint::{Connection, Incoming, RecvStream, SendStream, VarInt, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::{Notify as TokioNotify, watch};
use tokio::task::JoinHandle;

/// Serves one proxied stream with the local API (implemented by the daemon).
pub type ProxyServe = Arc<
    dyn Fn(ProxyStream, ProxyPrincipal) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;
/// Called whenever the runtime status changes.
pub type StatusHook = Arc<dyn Fn() + Send + Sync>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const PAIR_TIMEOUT: Duration = Duration::from_secs(60);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const LAN_DISCOVERY: Duration = Duration::from_secs(5);
/// QUIC application close codes.
const CLOSE_OK: u32 = 0;
const CLOSE_FORBIDDEN: u32 = 403;

#[derive(Debug, Clone)]
pub enum Role {
    Hub,
    /// `hub` holds the hub's endpoint id plus the last known addresses.
    Node {
        hub: EndpointAddr,
    },
}

pub struct StartOptions {
    pub secret: SecretKey,
    /// `default`, `disabled` or a relay URL (§12).
    pub relay: String,
    /// mDNS on the local network (`sync.lan_discovery`).
    pub lan_discovery: bool,
    pub store: Arc<Store>,
    /// This machine's row; its id is the endpoint id.
    pub machine: Machine,
    pub role: Role,
    pub proxy: ProxyServe,
    pub on_status: StatusHook,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeStatus {
    /// Node: sync session with the hub is up. Hub: endpoint running.
    pub connected: bool,
    pub last_sync_at: Option<i64>,
    pub last_error: Option<String>,
    /// Hub: nodes with a live connection.
    pub peers: usize,
}

/// A created invite, ready to show.
#[derive(Debug, Clone)]
pub struct InviteInfo {
    pub invite: String,
    /// `XXXX-XXXX`
    pub code: String,
    pub uri: String,
    pub expires_at: i64,
}

/// Result of pairing with a hub.
#[derive(Debug, Clone)]
pub struct Joined {
    pub hub: EndpointAddr,
    pub hub_meta: MachineMeta,
}

#[derive(Default)]
struct Peer {
    conns: Vec<Connection>,
    proxy: Option<Connection>,
}

struct Inner {
    ep: Endpoint,
    /// Relays are configured (else `online()` never resolves).
    relays: bool,
    store: Arc<Store>,
    own_id: String,
    machine: Machine,
    role: Role,
    invites: InviteBook,
    head: watch::Sender<i64>,
    kick: TokioNotify,
    peers: Mutex<HashMap<String, Peer>>,
    hub_proxy: Mutex<Option<Connection>>,
    status: Mutex<RuntimeStatus>,
    proxy: ProxyServe,
    on_status: StatusHook,
    shutdown: watch::Receiver<bool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Plain data guarded by short critical sections; poisoning cannot leave
    // it inconsistent.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct SyncService {
    inner: Arc<Inner>,
    shutdown_tx: watch::Sender<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for SyncService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncService")
            .field("id", &self.inner.own_id)
            .field("role", &self.inner.role)
            .finish()
    }
}

/// Bind an endpoint with this machine's identity, relay mode and, when
/// `lan_discovery` is on, mDNS. Hubs mark themselves so nodes can find them.
async fn bind(
    secret: &SecretKey,
    relay: &str,
    lan_discovery: bool,
    alpns: Vec<Vec<u8>>,
    hub: bool,
) -> Result<(Endpoint, Option<MdnsAddressLookup>)> {
    let mut builder = match relay {
        // n0 relays plus n0 DNS address publishing/lookup, so peers are found
        // by id alone.
        "default" => Endpoint::builder(presets::N0),
        "disabled" => Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled),
        url => {
            let url = url
                .parse()
                .map_err(|e| SyncError::Bind(format!("invalid relay URL {url:?}: {e}")))?;
            Endpoint::builder(presets::Minimal).relay_mode(RelayMode::custom([url]))
        }
    }
    .secret_key(secret.clone())
    .alpns(alpns);
    let mdns = if lan_discovery {
        match MdnsAddressLookup::builder()
            .service_name(MDNS_SERVICE)
            .build(secret.public())
        {
            Ok(m) => {
                builder = builder.address_lookup(m.clone());
                Some(m)
            }
            Err(e) => {
                tracing::warn!(error = %e, "local network discovery unavailable");
                None
            }
        }
    } else {
        None
    };
    if hub {
        let marker = HUB_MARKER
            .parse()
            .map_err(|e| SyncError::Bind(format!("hub marker: {e}")))?;
        builder = builder.user_data_for_address_lookup(marker);
    }
    let ep = builder
        .bind()
        .await
        .map_err(|e| SyncError::Bind(e.to_string()))?;
    Ok((ep, mdns))
}

fn close(conn: &Connection, code: u32, reason: &[u8]) {
    conn.close(VarInt::from_u32(code), reason);
}

impl SyncService {
    pub async fn start(opts: StartOptions) -> Result<SyncService> {
        let hub = matches!(opts.role, Role::Hub);
        let alpns = if hub {
            vec![ALPN_PAIR.to_vec(), ALPN_SYNC.to_vec(), ALPN_PROXY.to_vec()]
        } else {
            vec![ALPN_PROXY.to_vec()]
        };
        let (ep, _mdns) = bind(&opts.secret, &opts.relay, opts.lan_discovery, alpns, hub).await?;
        let own_id = ep.id().to_string();
        if own_id != opts.machine.id {
            return Err(SyncError::Bind(format!(
                "machine id {} does not match the identity key {own_id}",
                opts.machine.id
            )));
        }
        let (shutdown_tx, shutdown) = watch::channel(false);
        let store = opts.store.clone();
        let head = blocking(move || Ok(store.hub_head()?)).await?;
        let inner = Arc::new(Inner {
            ep,
            store: opts.store,
            own_id,
            relays: opts.relay != "disabled",
            machine: opts.machine,
            role: opts.role,
            invites: InviteBook::default(),
            head: watch::channel(head).0,
            kick: TokioNotify::new(),
            peers: Mutex::new(HashMap::new()),
            hub_proxy: Mutex::new(None),
            status: Mutex::new(RuntimeStatus {
                connected: hub,
                ..Default::default()
            }),
            proxy: opts.proxy,
            on_status: opts.on_status,
            shutdown,
        });
        let mut tasks = vec![tokio::spawn(accept_loop(inner.clone()))];
        if hub {
            tasks.push(tokio::spawn(hub_log_loop(inner.clone())));
        } else {
            tasks.push(tokio::spawn(node_loop(inner.clone())));
        }
        tracing::info!(id = %inner.own_id, role = ?inner.role, "sync endpoint started");
        Ok(SyncService {
            inner,
            shutdown_tx,
            tasks: Mutex::new(tasks),
        })
    }

    pub fn id(&self) -> &str {
        &self.inner.own_id
    }

    pub fn is_hub(&self) -> bool {
        matches!(self.inner.role, Role::Hub)
    }

    pub fn status(&self) -> RuntimeStatus {
        let mut s = lock(&self.inner.status).clone();
        s.peers = lock(&self.inner.peers)
            .values()
            .filter(|p| !p.conns.is_empty())
            .count();
        s
    }

    /// Hub: create an invite valid for 10 minutes.
    pub async fn create_invite(&self) -> Result<InviteInfo> {
        if !self.is_hub() {
            return Err(SyncError::Unavailable("only a hub creates invites".into()));
        }
        // Give the endpoint a moment to learn its relay and public addresses
        // so the ticket works beyond the LAN; local addresses suffice otherwise.
        if self.inner.relays {
            let _ = tokio::time::timeout(Duration::from_secs(5), self.inner.ep.online()).await;
        }
        let created = self.inner.invites.create(blirp_core::now_ms())?;
        let invite = Ticket {
            addr: self.inner.ep.addr(),
            invite_id: created.invite_id,
        }
        .encode()?;
        Ok(InviteInfo {
            uri: pair::join_uri(&invite, &created.code),
            code: pair::format_code(&created.code),
            invite,
            expires_at: created.expires_at,
        })
    }

    /// Hub: drop every live connection of `node_id` (after it was revoked).
    pub fn disconnect(&self, node_id: &str) {
        close_peer(&self.inner, node_id);
        (self.inner.on_status)();
    }

    /// Open a proxied HTTP stream to `target`'s API: through the hub on a
    /// node, directly to the node on the hub.
    pub async fn open_proxy(&self, target: &str, control: bool, via: &str) -> Result<ProxyStream> {
        if target == self.inner.own_id {
            return Err(SyncError::Protocol("target is this machine".into()));
        }
        let conn = match &self.inner.role {
            Role::Node { .. } => lock(&self.inner.hub_proxy)
                .clone()
                .ok_or_else(|| SyncError::Unavailable("not connected to the hub".into()))?,
            Role::Hub => lock(&self.inner.peers)
                .get(target)
                .and_then(|p| p.proxy.clone())
                .ok_or_else(|| {
                    SyncError::Unavailable(format!("machine {target} is not connected"))
                })?,
        };
        let (send, recv) = conn.open_bi().await.map_err(SyncError::connection)?;
        proxy::request(
            send,
            recv,
            &ProxyOpen {
                version: 1,
                target: target.to_string(),
                control,
                via: via.to_string(),
            },
        )
        .await
    }

    /// Node: leave the hub. Stops syncing, pushes what is still queued
    /// (starting new batches for at most `push_for`), then asks the hub to
    /// revoke this machine. Call [`Self::shutdown`] afterwards, also when
    /// this fails.
    pub async fn leave(&self, push_for: Duration) -> Result<()> {
        let Role::Node { hub } = &self.inner.role else {
            return Err(SyncError::Unavailable("only a node leaves a hub".into()));
        };
        // The regular session must not push concurrently.
        self.stop_tasks().await;
        let push_until = tokio::time::Instant::now() + push_for;
        let conn = connect(&self.inner.ep, hub, ALPN_SYNC).await?;
        let result = repl::leave(
            &conn,
            self.inner.store.clone(),
            self.inner.machine.clone(),
            &hub.id.to_string(),
            push_until,
        )
        .await;
        close(&conn, CLOSE_OK, b"left");
        result
    }

    async fn stop_tasks(&self) {
        let _ = self.shutdown_tx.send(true);
        let tasks: Vec<JoinHandle<()>> = lock(&self.tasks).drain(..).collect();
        for t in &tasks {
            t.abort();
        }
        for t in tasks {
            let _ = t.await;
        }
    }

    /// Stop all tasks and close the endpoint. Idempotent.
    pub async fn shutdown(&self) {
        self.stop_tasks().await;
        let conns: Vec<Connection> = lock(&self.inner.peers)
            .drain()
            .flat_map(|(_, p)| p.conns.into_iter().chain(p.proxy))
            .chain(lock(&self.inner.hub_proxy).take())
            .collect();
        for c in conns {
            close(&c, CLOSE_OK, b"shutdown");
        }
        self.inner.ep.close().await;
        tracing::info!("sync endpoint stopped");
    }
}

/// Pair this machine with a hub using a temporary endpoint. Without an
/// invite, the only hub advertising itself on the LAN is used (which needs
/// `lan_discovery`).
pub async fn join(
    secret: &SecretKey,
    relay: &str,
    lan_discovery: bool,
    invite: Option<&str>,
    code: &str,
    me: MachineMeta,
) -> Result<Joined> {
    pair::normalize_code(code)?;
    let ticket = invite.map(Ticket::decode).transpose()?;
    let (ep, mdns) = bind(secret, relay, lan_discovery, Vec::new(), false).await?;
    let result = async {
        let (addr, invite_id) = match ticket {
            Some(t) => (t.addr, Some(t.invite_id)),
            None => (discover_hub(mdns.as_ref()).await?, None),
        };
        let conn = tokio::time::timeout(CONNECT_TIMEOUT, ep.connect(addr.clone(), ALPN_PAIR))
            .await
            .map_err(|_| SyncError::Connect {
                what: "the hub".into(),
                message: "timed out".into(),
            })?
            .map_err(|e| SyncError::Connect {
                what: "the hub".into(),
                message: e.to_string(),
            })?;
        let (mut send, mut recv) = conn.open_bi().await.map_err(SyncError::connection)?;
        let ids = PairIds {
            node: *secret.public().as_bytes(),
            hub: *conn.remote_id().as_bytes(),
        };
        let hub_meta = tokio::time::timeout(
            PAIR_TIMEOUT,
            pair::node_pair(&mut recv, &mut send, ids, invite_id.as_deref(), code, &me),
        )
        .await
        .map_err(|_| SyncError::Connection("pairing timed out".into()))??;
        let _ = send.finish();
        close(&conn, CLOSE_OK, b"paired");
        Ok(Joined {
            hub: EndpointAddr::from_parts(conn.remote_id(), addr.addrs),
            hub_meta,
        })
    }
    .await;
    ep.close().await;
    result
}

async fn discover_hub(mdns: Option<&MdnsAddressLookup>) -> Result<EndpointAddr> {
    use futures_util::StreamExt as _;
    let mdns = mdns.ok_or(pair::PairError::NoHubFound)?;
    let mut events = mdns.subscribe().await;
    let mut hubs: HashMap<EndpointId, EndpointAddr> = HashMap::new();
    let deadline = tokio::time::Instant::now() + LAN_DISCOVERY;
    while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, events.next()).await {
        match ev {
            DiscoveryEvent::Discovered { endpoint_info, .. }
                if endpoint_info
                    .user_data()
                    .is_some_and(|d| d.as_ref() == HUB_MARKER) =>
            {
                hubs.insert(
                    endpoint_info.endpoint_id,
                    endpoint_info.into_endpoint_addr(),
                );
            }
            DiscoveryEvent::Expired { endpoint_id } => {
                hubs.remove(&endpoint_id);
            }
            _ => {}
        }
    }
    let mut it = hubs.into_values();
    match (it.next(), it.next()) {
        (Some(addr), None) => Ok(addr),
        (None, _) => Err(pair::PairError::NoHubFound.into()),
        (Some(_), Some(_)) => Err(pair::PairError::MultipleHubs.into()),
    }
}

// ---------------------------------------------------------------- accept

async fn accept_loop(inner: Arc<Inner>) {
    while let Some(incoming) = inner.ep.accept().await {
        let inner = inner.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_incoming(&inner, incoming).await {
                tracing::debug!(error = %e, "incoming connection ended");
            }
        });
    }
}

async fn handle_incoming(inner: &Arc<Inner>, incoming: Incoming) -> Result<()> {
    let mut accepting = incoming.accept().map_err(SyncError::connection)?;
    let alpn = accepting.alpn().await.map_err(SyncError::connection)?;
    let conn = accepting.await.map_err(SyncError::connection)?;
    let remote = conn.remote_id().to_string();
    match (alpn.as_slice(), &inner.role) {
        (ALPN_PAIR, Role::Hub) => pair_incoming(inner, conn).await,
        (ALPN_SYNC, Role::Hub) => {
            if !authorized(inner, &remote).await? {
                close(&conn, CLOSE_FORBIDDEN, b"revoked or unknown machine");
                return Ok(());
            }
            lock(&inner.peers)
                .entry(remote.clone())
                .or_default()
                .conns
                .push(conn.clone());
            touch_device(inner, &remote);
            (inner.on_status)();
            let kick = inner.clone();
            let result = repl::serve_hub(
                conn.clone(),
                inner.store.clone(),
                inner.own_id.clone(),
                remote.clone(),
                inner.head.subscribe(),
                move |logged| {
                    lock(&kick.status).last_sync_at = Some(blirp_core::now_ms());
                    if logged {
                        kick.kick.notify_one();
                    }
                },
            )
            .await;
            forget_conn(inner, &remote, &conn);
            if matches!(result, Ok(repl::HubSessionEnd::Left)) {
                tracing::info!(node = %remote, "machine left the hub; revoked");
                close_peer(inner, &remote);
                // Its revoked machine row replicates to the other nodes.
                inner.kick.notify_one();
            }
            (inner.on_status)();
            result.map(|_| ())
        }
        (ALPN_PROXY, Role::Hub) => {
            if !authorized(inner, &remote).await? {
                close(&conn, CLOSE_FORBIDDEN, b"revoked or unknown machine");
                return Ok(());
            }
            {
                let mut peers = lock(&inner.peers);
                let peer = peers.entry(remote.clone()).or_default();
                if let Some(old) = peer.proxy.replace(conn.clone()) {
                    close(&old, CLOSE_OK, b"replaced");
                }
            }
            let result = serve_proxy_conn(inner, &conn, &remote).await;
            forget_conn(inner, &remote, &conn);
            result
        }
        (ALPN_PROXY, Role::Node { hub }) if conn.remote_id() == hub.id => {
            serve_proxy_conn(inner, &conn, &remote).await
        }
        _ => {
            close(&conn, CLOSE_FORBIDDEN, b"forbidden");
            Ok(())
        }
    }
}

/// Hub: close every live connection of a revoked machine.
fn close_peer(inner: &Inner, node_id: &str) {
    if let Some(peer) = lock(&inner.peers).remove(node_id) {
        for c in peer.conns.iter().chain(peer.proxy.iter()) {
            close(c, CLOSE_FORBIDDEN, b"revoked");
        }
        tracing::info!(node = %node_id, "closed connections of revoked machine");
    }
}

fn forget_conn(inner: &Inner, remote: &str, conn: &Connection) {
    let mut peers = lock(&inner.peers);
    if let Some(p) = peers.get_mut(remote) {
        p.conns.retain(|c| c.stable_id() != conn.stable_id());
        if p.proxy
            .as_ref()
            .is_some_and(|c| c.stable_id() == conn.stable_id())
        {
            p.proxy = None;
        }
        if p.conns.is_empty() && p.proxy.is_none() {
            peers.remove(remote);
        }
    }
}

/// Hub: `remote` is a paired, non-revoked machine.
async fn authorized(inner: &Inner, remote: &str) -> Result<bool> {
    let store = inner.store.clone();
    let id = remote.to_string();
    blocking(move || Ok(store.machine_device(&id)?.is_some())).await
}

/// Hub: may requests relayed for `remote` control terminals.
async fn may_control(inner: &Inner, remote: &str) -> Result<bool> {
    match &inner.role {
        // A node trusts its hub's assertion (the hub enforces per device).
        Role::Node { hub } => Ok(hub.id.to_string() == remote),
        Role::Hub => {
            let store = inner.store.clone();
            let id = remote.to_string();
            blocking(move || {
                Ok(store
                    .machine_device(&id)?
                    .is_some_and(|d| d.can_control_terminals))
            })
            .await
        }
    }
}

fn touch_device(inner: &Inner, remote: &str) {
    let store = inner.store.clone();
    let id = remote.to_string();
    tokio::task::spawn_blocking(move || {
        let r = store.machine_device(&id).and_then(|d| match d {
            Some(d) => store.touch_device(&d.id, blirp_core::now_ms()),
            None => Ok(()),
        });
        if let Err(e) = r {
            tracing::warn!(error = %e, "updating device last_seen failed");
        }
    });
}

async fn pair_incoming(inner: &Arc<Inner>, conn: Connection) -> Result<()> {
    let (mut send, mut recv) = conn.accept_bi().await.map_err(SyncError::connection)?;
    let node_id = conn.remote_id();
    let ids = PairIds {
        node: *node_id.as_bytes(),
        hub: *inner.ep.id().as_bytes(),
    };
    let me = MachineMeta {
        name: inner.machine.name.clone(),
        os: inner.machine.os.clone(),
    };
    let store = inner.store.clone();
    let node = node_id.to_string();
    let outcome = tokio::time::timeout(
        PAIR_TIMEOUT,
        pair::hub_pair(
            &mut recv,
            &mut send,
            ids,
            &inner.invites,
            &me,
            blirp_core::now_ms(),
            move |meta| async move {
                blocking(move || register_node(&store, &node, &meta))
                    .await
                    .map_err(|e| e.to_string())
            },
        ),
    )
    .await;
    let _ = send.finish();
    // Let the node read the last frame; it closes the connection.
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
    match outcome {
        Ok(Ok(meta)) => {
            tracing::info!(node = %node_id, name = %meta.name, "paired machine");
            (inner.on_status)();
            Ok(())
        }
        Ok(Err(e)) => {
            tracing::warn!(node = %node_id, error = %e, "pairing failed");
            Err(e.into())
        }
        Err(_) => Err(SyncError::Connection("pairing timed out".into())),
    }
}

/// Hub: store a newly paired machine (device + machine row). Machines are
/// trusted to control terminals by default.
fn register_node(store: &Store, node_id: &str, meta: &MachineMeta) -> Result<()> {
    let now = blirp_core::now_ms();
    let existing = store.device_by_node_id(node_id)?;
    store.upsert_device(&Device {
        id: existing
            .as_ref()
            .map_or_else(blirp_core::new_id, |d| d.id.clone()),
        name: meta.name.clone(),
        kind: DeviceKind::Machine,
        token_hash: None,
        node_id: Some(node_id.to_string()),
        created_at: existing.as_ref().map_or(now, |d| d.created_at),
        last_seen: now,
        revoked: false,
        can_control_terminals: true,
    })?;
    store.apply(Change::Machine(Machine {
        id: node_id.to_string(),
        name: meta.name.clone(),
        os: meta.os.clone(),
        role: MachineRole::Node,
        last_seen: now,
        revoked: false,
    }))?;
    Ok(())
}

/// Hub: revoke a paired machine (its device and its replicated machine row;
/// the caller closes its connections). Revoked devices lose terminal
/// control too; pairing again restores it.
pub fn revoke_machine(store: &Store, node_id: &str) -> Result<()> {
    for d in store.list_devices()? {
        if d.node_id.as_deref() == Some(node_id) && !d.revoked {
            store.upsert_device(&Device {
                revoked: true,
                can_control_terminals: false,
                ..d
            })?;
        }
    }
    if let Some(m) = store.get_machine(node_id)? {
        store.apply(Change::Machine(Machine {
            revoked: true,
            last_seen: m.last_seen.max(blirp_core::now_ms()),
            ..m
        }))?;
    }
    Ok(())
}

// ---------------------------------------------------------------- proxy

async fn serve_proxy_conn(inner: &Arc<Inner>, conn: &Connection, remote: &str) -> Result<()> {
    loop {
        let (send, recv) = tokio::select! {
            s = conn.accept_bi() => s.map_err(SyncError::connection)?,
            _ = wait_shutdown(inner.shutdown.clone()) => return Ok(()),
        };
        let inner = inner.clone();
        let remote = remote.to_string();
        tokio::spawn(async move {
            if let Err(e) = proxy_stream(&inner, send, recv, &remote).await {
                tracing::debug!(peer = %remote, error = %e, "proxy stream ended");
            }
        });
    }
}

async fn wait_shutdown(mut rx: watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

async fn proxy_stream(
    inner: &Arc<Inner>,
    mut send: SendStream,
    mut recv: RecvStream,
    remote: &str,
) -> Result<()> {
    let open: ProxyOpen = tokio::time::timeout(
        Duration::from_secs(20),
        read_frame(&mut recv, MAX_CONTROL_FRAME),
    )
    .await
    .map_err(|_| SyncError::Connection("proxy open timed out".into()))??;
    if crate::negotiate(&[open.version]).is_none() {
        write_frame(
            &mut send,
            &ProxyReply::err("unsupported_version", "unsupported proxy version"),
        )
        .await?;
        return Ok(());
    }
    let control = open.control && may_control(inner, remote).await?;
    if open.target == inner.own_id {
        write_frame(&mut send, &ProxyReply::ok()).await?;
        (inner.proxy)(
            tokio::io::join(recv, send),
            ProxyPrincipal {
                control,
                via: open.via,
            },
        )
        .await;
        return Ok(());
    }
    if !matches!(inner.role, Role::Hub) {
        write_frame(
            &mut send,
            &ProxyReply::err("not_found", "this machine only serves itself"),
        )
        .await?;
        return Ok(());
    }
    let target_conn = lock(&inner.peers)
        .get(&open.target)
        .and_then(|p| p.proxy.clone());
    let Some(target_conn) = target_conn else {
        write_frame(
            &mut send,
            &ProxyReply::err(
                "machine_offline",
                "that machine is not connected to the hub",
            ),
        )
        .await?;
        return Ok(());
    };
    let relayed = async {
        let (ts, tr) = target_conn.open_bi().await.map_err(SyncError::connection)?;
        proxy::request(
            ts,
            tr,
            &ProxyOpen {
                version: 1,
                target: open.target.clone(),
                control,
                via: open.via.clone(),
            },
        )
        .await
    }
    .await;
    match relayed {
        Ok(mut target) => {
            write_frame(&mut send, &ProxyReply::ok()).await?;
            let mut source = tokio::io::join(recv, send);
            tokio::io::copy_bidirectional(&mut source, &mut target)
                .await
                .map_err(SyncError::connection)?;
            Ok(())
        }
        Err(SyncError::Remote { code, message }) => {
            write_frame(&mut send, &ProxyReply::err(&code, message)).await?;
            Ok(())
        }
        Err(e) => {
            write_frame(
                &mut send,
                &ProxyReply::err("machine_unreachable", e.to_string()),
            )
            .await?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- hub log

/// Hub: log this machine's own writes promptly and wake node notifiers.
async fn hub_log_loop(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(repl::OUTBOX_POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut shutdown = inner.shutdown.clone();
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = inner.kick.notified() => {}
            _ = shutdown.changed() => return,
        }
        let store = inner.store.clone();
        let own = inner.own_id.clone();
        match blocking(move || {
            store.hub_flush_own(&own)?;
            Ok(store.hub_head()?)
        })
        .await
        {
            Ok(head) => {
                inner.head.send_if_modified(|h| {
                    let changed = *h != head;
                    *h = head;
                    changed
                });
            }
            Err(e) => tracing::error!(error = %e, "logging local changes to hub_log failed"),
        }
    }
}

// ---------------------------------------------------------------- node

async fn node_loop(inner: Arc<Inner>) {
    let Role::Node { hub } = inner.role.clone() else {
        return;
    };
    let mut backoff = BACKOFF_MIN;
    let mut shutdown = inner.shutdown.clone();
    loop {
        let started = Instant::now();
        let result = node_session(&inner, &hub).await;
        if *shutdown.borrow() {
            return;
        }
        let message = match &result {
            Ok(()) => "connection closed".to_string(),
            Err(e) => e.to_string(),
        };
        tracing::warn!(error = %message, "hub connection lost; retrying");
        {
            let mut st = lock(&inner.status);
            st.connected = false;
            st.last_error = Some(message);
        }
        (inner.on_status)();
        if started.elapsed() > BACKOFF_MAX {
            backoff = BACKOFF_MIN;
        }
        let jitter =
            Duration::from_millis(u64::try_from(blirp_core::now_ms().rem_euclid(500)).unwrap_or(0));
        tokio::select! {
            _ = tokio::time::sleep(backoff + jitter) => {}
            _ = shutdown.changed() => return,
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn connect(ep: &Endpoint, hub: &EndpointAddr, alpn: &[u8]) -> Result<Connection> {
    let conn = tokio::time::timeout(CONNECT_TIMEOUT, ep.connect(hub.clone(), alpn))
        .await
        .map_err(|_| SyncError::Connect {
            what: "the hub".into(),
            message: "timed out".into(),
        })?
        .map_err(|e| SyncError::Connect {
            what: "the hub".into(),
            message: e.to_string(),
        })?;
    Ok(conn)
}

async fn node_session(inner: &Arc<Inner>, hub: &EndpointAddr) -> Result<()> {
    let sync = connect(&inner.ep, hub, ALPN_SYNC).await?;
    let proxy_conn = match connect(&inner.ep, hub, ALPN_PROXY).await {
        Ok(c) => c,
        Err(e) => {
            close(&sync, CLOSE_OK, b"proxy failed");
            return Err(e);
        }
    };
    *lock(&inner.hub_proxy) = Some(proxy_conn.clone());
    let proxy_task = {
        let inner = inner.clone();
        let conn = proxy_conn.clone();
        let hub_id = hub.id.to_string();
        tokio::spawn(async move {
            if let Err(e) = serve_proxy_conn(&inner, &conn, &hub_id).await {
                tracing::debug!(error = %e, "hub proxy connection ended");
            }
        })
    };
    let hook = inner.clone();
    let result = repl::run_node(
        &sync,
        inner.store.clone(),
        inner.machine.clone(),
        hub.id.to_string(),
        inner.shutdown.clone(),
        move || {
            let changed = {
                let mut st = lock(&hook.status);
                let was = st.connected;
                st.connected = true;
                st.last_error = None;
                st.last_sync_at = Some(blirp_core::now_ms());
                !was
            };
            // Periodic syncs are not news; only report the transition.
            if changed {
                (hook.on_status)();
            }
        },
    )
    .await;
    proxy_task.abort();
    lock(&inner.hub_proxy).take();
    let reason = sync.close_reason().map(|r| r.to_string());
    close(&sync, CLOSE_OK, b"bye");
    close(&proxy_conn, CLOSE_OK, b"bye");
    match (result, reason) {
        (Err(_), Some(r)) if r.contains("revoked") => Err(SyncError::Unavailable(
            "this machine was revoked by the hub; pair it again".into(),
        )),
        (r, _) => r,
    }
}
