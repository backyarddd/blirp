//! Sync runtime: the iroh endpoint, the accept loop routing by ALPN, the
//! hub's peer table and the node's reconnecting session with its hub.

use crate::pair::{self, InviteBook, MachineMeta, PairIds, Ticket};
use crate::proxy::{self, ProxyOpen, ProxyPrincipal, ProxyReply, ProxyStream};
use crate::repl::Presence;
use crate::wire::{MAX_CONTROL_FRAME, read_frame, write_frame};
use crate::{
    ALPN_FILES, ALPN_PAIR, ALPN_PROXY, ALPN_SYNC, HUB_MARKER, MDNS_SERVICE, Result, SyncError,
    blocking, repl,
};
use blirp_core::model::{Device, DeviceKind, Machine, MachineRole, UpdateNeeded};
use blirp_core::store::{Change, Store};
use iroh::endpoint::{
    Connection, Incoming, PortmapperConfig, RecvStream, SendStream, VarInt, presets,
};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::net::Ipv4Addr;
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
/// Hub: how often connected machines' `last_seen` advances (and presence is
/// sent to the nodes) while nothing connects or disconnects.
const PRESENCE_EVERY: Duration = Duration::from_secs(60);
/// QUIC application close codes.
const CLOSE_OK: u32 = 0;
const CLOSE_FORBIDDEN: u32 = 403;
/// `blirp/files/1` connections one machine may hold on the hub at once.
const MAX_FILES_CONNS: usize = 3;

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
    /// Hub: the project file service (`blirp/files/1`); None turns the
    /// protocol off.
    pub files: Option<Arc<crate::files::HubFiles>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeStatus {
    /// Node: sync session with the hub is up. Hub: endpoint running.
    pub connected: bool,
    pub last_sync_at: Option<i64>,
    pub last_error: Option<String>,
    /// Hub: nodes with a live connection.
    pub peers: usize,
    /// Node: sync refused for another replicated schema (§10), the machine
    /// to update, set by the last failed handshake.
    pub update_needed: Option<UpdateNeeded>,
    /// Hub: paired machines refused for running an older release.
    pub outdated: BTreeSet<String>,
    /// Hub: paired machines refused for running a newer release.
    pub newer: BTreeSet<String>,
}

impl RuntimeStatus {
    /// Hub: `node` was refused for another release (`peer_older`: it runs
    /// the older one).
    fn refused(&mut self, node: &str, peer_older: bool) {
        self.forget(node);
        let set = if peer_older {
            &mut self.outdated
        } else {
            &mut self.newer
        };
        set.insert(node.to_string());
    }

    /// Hub: `node` synced, was revoked or reconnects: no refusal stands.
    fn forget(&mut self, node: &str) {
        self.outdated.remove(node);
        self.newer.remove(node);
    }
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
    /// `blirp/files/1` connections: closed with the others on revocation,
    /// but not what makes a machine online (its sync session does).
    files: Vec<Connection>,
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
    /// Hub: every paired machine's presence (runtime only, never replicated).
    /// Node: what the hub last reported, emptied when the connection drops.
    presence: Mutex<HashMap<String, Presence>>,
    /// Hub: the presence list sent to the nodes.
    presence_tx: watch::Sender<Vec<Presence>>,
    /// Node: when the connection to the hub was last known to be up.
    hub_seen: Mutex<Option<i64>>,
    proxy: ProxyServe,
    on_status: StatusHook,
    shutdown: watch::Receiver<bool>,
    files: Option<Arc<crate::files::HubFiles>>,
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
pub(crate) async fn bind(
    secret: &SecretKey,
    relay: &str,
    lan_discovery: bool,
    alpns: Vec<Vec<u8>>,
    hub: bool,
) -> Result<(Endpoint, Option<MdnsAddressLookup>)> {
    let loopback = crate::loopback_only();
    let (relay, lan_discovery) = if loopback {
        tracing::warn!("BLIRP_LOOPBACK_ONLY=1: sync is loopback-only (no relays, mDNS or LAN)");
        ("disabled", false)
    } else {
        (relay, lan_discovery)
    };
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
    if loopback {
        // IPv4 loopback only: `[::1]` is missing on some CI hosts, and the
        // default wildcard sockets are what raise firewall prompts.
        builder = builder
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| SyncError::Bind(e.to_string()))?
            .portmapper_config(PortmapperConfig::Disabled);
    }
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
            let mut a = vec![ALPN_PAIR.to_vec(), ALPN_SYNC.to_vec(), ALPN_PROXY.to_vec()];
            if opts.files.is_some() {
                a.push(ALPN_FILES.to_vec());
            }
            a
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
        // Hub: every paired machine is offline until it connects.
        let presence = if hub {
            let store = opts.store.clone();
            let own = opts.machine.id.clone();
            blocking(move || {
                Ok(store
                    .list_machines()?
                    .into_iter()
                    .filter(|m| m.id != own && !m.revoked)
                    .map(|m| {
                        let p = Presence {
                            machine_id: m.id.clone(),
                            online: false,
                            last_seen: m.last_seen,
                        };
                        (m.id, p)
                    })
                    .collect::<HashMap<_, _>>())
            })
            .await?
        } else {
            HashMap::new()
        };
        let inner = Arc::new(Inner {
            ep,
            store: opts.store,
            own_id,
            relays: opts.relay != "disabled" && !crate::loopback_only(),
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
            presence: Mutex::new(presence),
            presence_tx: watch::channel(Vec::new()).0,
            hub_seen: Mutex::new(None),
            proxy: opts.proxy,
            on_status: opts.on_status,
            shutdown,
            files: opts.files,
        });
        let mut tasks = vec![tokio::spawn(accept_loop(inner.clone()))];
        if hub {
            publish_presence(&inner);
            tasks.push(tokio::spawn(hub_log_loop(inner.clone())));
            tasks.push(tokio::spawn(hub_compact_loop(inner.clone())));
            tasks.push(tokio::spawn(hub_presence_loop(inner.clone())));
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

    /// The relay this endpoint is reachable through, if it has one yet.
    pub fn home_relay(&self) -> Option<String> {
        self.inner
            .ep
            .addr()
            .relay_urls()
            .next()
            .map(ToString::to_string)
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

    /// Which machines are connected, by machine id; a machine missing from
    /// the map is unknown. This machine is always online. A hub knows every
    /// paired machine. A node knows the hub (online while its connection is
    /// up) and, when the hub reports presence (sync protocol 2) and the
    /// connection is up, every other machine.
    pub fn presence(&self) -> HashMap<String, Presence> {
        let now = blirp_core::now_ms();
        let mut out = lock(&self.inner.presence).clone();
        if let Role::Node { hub } = &self.inner.role {
            let hub = hub.id.to_string();
            let connected = lock(&self.inner.status).connected;
            // A presence frame read just after the connection dropped must
            // not outlive it: without the hub the others are unknown.
            if !connected {
                out.clear();
            }
            let seen = *lock(&self.inner.hub_seen);
            out.insert(
                hub.clone(),
                Presence {
                    machine_id: hub,
                    online: connected,
                    last_seen: if connected { now } else { seen.unwrap_or(0) },
                },
            );
        }
        out.insert(
            self.inner.own_id.clone(),
            Presence {
                machine_id: self.inner.own_id.clone(),
                online: true,
                last_seen: now,
            },
        );
        out
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

    /// Node: a new connection to the hub for `blirp/files/1`.
    pub async fn connect_files(&self) -> Result<Connection> {
        match &self.inner.role {
            Role::Node { hub } => connect(&self.inner.ep, hub, ALPN_FILES).await,
            Role::Hub => Err(SyncError::Unavailable(
                "the hub serves project files itself".into(),
            )),
        }
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
            .flat_map(|(_, p)| p.conns.into_iter().chain(p.proxy).chain(p.files))
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
            refresh_online(inner, &remote);
            (inner.on_status)();
            let kick = inner.clone();
            let node = remote.clone();
            let result = repl::serve_hub(
                conn.clone(),
                inner.store.clone(),
                inner.own_id.clone(),
                remote.clone(),
                inner.head.subscribe(),
                inner.presence_tx.subscribe(),
                move |logged| {
                    let mut st = lock(&kick.status);
                    st.last_sync_at = Some(blirp_core::now_ms());
                    st.forget(&node);
                    drop(st);
                    if logged {
                        kick.kick.notify_one();
                    }
                },
            )
            .await;
            if let Err(SyncError::ReleaseMismatch { peer_older }) = &result {
                lock(&inner.status).refused(&remote, *peer_older);
            }
            forget_conn(inner, &remote, &conn);
            refresh_online(inner, &remote);
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
        (ALPN_FILES, Role::Hub) => {
            let Some(files) = inner.files.clone() else {
                close(&conn, CLOSE_FORBIDDEN, b"forbidden");
                return Ok(());
            };
            if !authorized(inner, &remote).await? {
                close(&conn, CLOSE_FORBIDDEN, b"revoked or unknown machine");
                return Ok(());
            }
            // Tracked with the machine's other connections, so revoking it
            // closes this one too.
            {
                let mut peers = lock(&inner.peers);
                let peer = peers.entry(remote.clone()).or_default();
                // A node needs one; a few allow reconnects to overlap.
                if peer.files.len() >= MAX_FILES_CONNS {
                    drop(peers);
                    close(&conn, CLOSE_FORBIDDEN, b"too many file connections");
                    return Ok(());
                }
                peer.files.push(conn.clone());
            }
            let store = inner.store.clone();
            let id = remote.clone();
            let name = blocking(move || Ok(store.get_machine(&id)?.map(|m| m.name)))
                .await?
                .unwrap_or_else(|| "machine".to_string());
            let result =
                crate::files::server::serve(conn.clone(), files, remote.clone(), name).await;
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
    // Revoked (or reconnecting with new rights): a refusal is re-recorded
    // if it happens again.
    lock(&inner.status).forget(node_id);
    if let Some(peer) = lock(&inner.peers).remove(node_id) {
        for c in peer
            .conns
            .iter()
            .chain(peer.proxy.iter())
            .chain(peer.files.iter())
        {
            close(c, CLOSE_FORBIDDEN, b"revoked");
        }
        tracing::info!(node = %node_id, "closed connections of revoked machine");
    }
}

fn forget_conn(inner: &Inner, remote: &str, conn: &Connection) {
    let mut peers = lock(&inner.peers);
    if let Some(p) = peers.get_mut(remote) {
        p.conns.retain(|c| c.stable_id() != conn.stable_id());
        p.files.retain(|c| c.stable_id() != conn.stable_id());
        if p.proxy
            .as_ref()
            .is_some_and(|c| c.stable_id() == conn.stable_id())
        {
            p.proxy = None;
        }
        if p.conns.is_empty() && p.proxy.is_none() && p.files.is_empty() {
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
        can_access_files: false,
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
    // Its position no longer bounds compaction; pairing again starts over.
    store.hub_forget_pull(node_id)?;
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

// ---------------------------------------------------------------- presence

/// Hub: `id` connected or lost a sync connection. Online means it still
/// has one; read and written under the peers lock, so a connect and a
/// disconnect racing each other cannot leave the wrong state behind.
fn refresh_online(inner: &Inner, id: &str) {
    {
        let peers = lock(&inner.peers);
        let online = peers.get(id).is_some_and(|p| !p.conns.is_empty());
        lock(&inner.presence).insert(
            id.to_string(),
            Presence {
                machine_id: id.to_string(),
                online,
                last_seen: blirp_core::now_ms(),
            },
        );
    }
    publish_presence(inner);
}

/// Hub: send the current presence (this machine included) to the nodes.
fn publish_presence(inner: &Inner) {
    let mut list: Vec<Presence> = lock(&inner.presence).values().cloned().collect();
    list.push(Presence {
        machine_id: inner.own_id.clone(),
        online: true,
        last_seen: blirp_core::now_ms(),
    });
    list.sort_by(|a, b| a.machine_id.cmp(&b.machine_id));
    inner.presence_tx.send_replace(list);
}

/// Hub: advance connected machines' `last_seen` every [`PRESENCE_EVERY`].
async fn hub_presence_loop(inner: Arc<Inner>) {
    let mut tick =
        tokio::time::interval_at(tokio::time::Instant::now() + PRESENCE_EVERY, PRESENCE_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut shutdown = inner.shutdown.clone();
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => return,
        }
        let now = blirp_core::now_ms();
        // Recomputed from the live connections, so presence heals itself
        // whatever happened between connect and disconnect bookkeeping.
        let changed = {
            let peers = lock(&inner.peers);
            let mut presence = lock(&inner.presence);
            let mut changed = false;
            for id in peers
                .iter()
                .filter(|(_, p)| !p.conns.is_empty())
                .map(|(id, _)| id)
            {
                if !presence.get(id).is_some_and(|x| x.online) {
                    changed = true;
                }
                presence.insert(
                    id.clone(),
                    Presence {
                        machine_id: id.clone(),
                        online: true,
                        last_seen: now,
                    },
                );
            }
            for p in presence.values_mut() {
                let online = peers
                    .get(&p.machine_id)
                    .is_some_and(|x| !x.conns.is_empty());
                if p.online && !online {
                    p.online = false;
                    p.last_seen = now;
                    changed = true;
                }
            }
            changed
        };
        publish_presence(&inner);
        if changed {
            (inner.on_status)();
        }
    }
}

/// Who is online in a presence map, comparable across reports.
fn online_set(m: &HashMap<String, Presence>) -> Vec<(String, bool)> {
    let mut v: Vec<_> = m
        .values()
        .map(|p| (p.machine_id.clone(), p.online))
        .collect();
    v.sort();
    v
}

/// Node: take the hub's presence report; a change of who is online is news.
fn take_presence(inner: &Inner, machines: Vec<Presence>) {
    let changed = {
        let mut p = lock(&inner.presence);
        let before = online_set(&p);
        *p = machines
            .into_iter()
            .map(|m| (m.machine_id.clone(), m))
            .collect();
        before != online_set(&p)
    };
    if changed {
        (inner.on_status)();
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

/// First `hub_log` compaction after the hub starts, then how often.
const COMPACT_FIRST: Duration = Duration::from_secs(60);
const COMPACT_EVERY: Duration = Duration::from_secs(60 * 60);
/// Rows per compaction write transaction, so pushes and pulls wait at most
/// one short batch for the writer.
const COMPACT_BATCH: usize = 1_000;

/// Hub: compact `hub_log` shortly after start, then hourly.
async fn hub_compact_loop(inner: Arc<Inner>) {
    let mut tick =
        tokio::time::interval_at(tokio::time::Instant::now() + COMPACT_FIRST, COMPACT_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut shutdown = inner.shutdown.clone();
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => return,
        }
        let store = inner.store.clone();
        match blocking(move || Ok(store.compact_hub_log(COMPACT_BATCH)?)).await {
            Ok(c) if c.removed + c.stripped > 0 => {
                tracing::info!(
                    removed = c.removed,
                    stripped = c.stripped,
                    "compacted hub_log"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "compacting hub_log failed"),
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
            if st.connected {
                *lock(&inner.hub_seen) = Some(blirp_core::now_ms());
            }
            st.connected = false;
            st.last_error = Some(message);
            st.update_needed = match &result {
                Err(SyncError::ReleaseMismatch { peer_older: true }) => Some(UpdateNeeded::Hub),
                Err(SyncError::ReleaseMismatch { peer_older: false }) => {
                    Some(UpdateNeeded::ThisMachine)
                }
                _ => None,
            };
        }
        // Other machines' presence came from the hub; unknown without it.
        lock(&inner.presence).clear();
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
    let presence_hook = inner.clone();
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
                st.update_needed = None;
                st.last_sync_at = Some(blirp_core::now_ms());
                !was
            };
            // Periodic syncs are not news; only report the transition.
            if changed {
                (hook.on_status)();
            }
        },
        move |machines| take_presence(&presence_hook, machines),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_follow_the_last_outcome() {
        let mut st = RuntimeStatus::default();
        st.refused("old", true);
        st.refused("new", false);
        assert_eq!(st.outdated, BTreeSet::from(["old".to_string()]));
        assert_eq!(st.newer, BTreeSet::from(["new".to_string()]));
        // Updated past the hub: now the hub is the older one.
        st.refused("old", false);
        assert!(st.outdated.is_empty());
        // Synced, revoked or reconnecting: forgotten.
        st.forget("old");
        st.forget("new");
        assert_eq!(st, RuntimeStatus::default());
    }

    // `.cargo/config.toml` sets BLIRP_LOOPBACK_ONLY for every `cargo test`.
    #[tokio::test]
    async fn loopback_only_binds_and_advertises_loopback() {
        assert!(
            crate::loopback_only(),
            "run through cargo so BLIRP_LOOPBACK_ONLY=1 is set"
        );
        let secret = SecretKey::from_bytes(&[7; 32]);
        let (ep, mdns) = bind(&secret, "default", true, Vec::new(), true)
            .await
            .unwrap();
        assert!(mdns.is_none(), "mDNS started");
        let bound = ep.bound_sockets();
        assert!(!bound.is_empty());
        assert!(bound.iter().all(|a| a.ip().is_loopback()), "{bound:?}");
        let addr = ep.addr();
        assert!(addr.relay_urls().next().is_none(), "relay configured");
        assert!(
            addr.ip_addrs().all(|a| a.ip().is_loopback()),
            "advertised {addr:?}"
        );
        ep.close().await;
    }
}
