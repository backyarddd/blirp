//! Two in-process daemons on temp homes, iroh with relays disabled (direct
//! localhost/LAN addresses only, never the public relay): pairing,
//! replication, restart resume, remote launch + terminal attach through the
//! hub, revocation, and browser devices on the TLS portal.

// Test helpers panic on failure by design.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use blirp::api::Principal;
use blirp::daemon::{Daemon, DaemonOptions};
use blirp_core::model::{
    BrowserInvite, Device, ErrorBody, Health, ProjectMemory, ProjectSummary, Record, Session,
    SyncInvite, SyncStatus, TerminalServerMessage,
};
use blirp_core::paths::Paths;
use blirp_core::store::{PulledEntry, Store};
use futures_util::{SinkExt, StreamExt};
use reqwest::Method;
use serde_json::{Value, json};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

struct Node {
    home: std::path::PathBuf,
    daemon: Daemon,
    http: reqwest::Client,
    base: String,
}

impl Node {
    async fn start(home: &Path, name: &str, portal_port: Option<u16>) -> Node {
        let config = match portal_port {
            Some(p) => format!(
                "[machine]\nname = \"{name}\"\n[sync]\nrelay = \"disabled\"\n[portal]\nlan = true\nlan_port = {p}\n"
            ),
            None => format!("[machine]\nname = \"{name}\"\n[sync]\nrelay = \"disabled\"\n"),
        };
        if !home.join("config.toml").exists() {
            std::fs::create_dir_all(home).unwrap();
            std::fs::write(home.join("config.toml"), config).unwrap();
        }
        Self::restart(home).await
    }

    async fn restart(home: &Path) -> Node {
        let daemon = Daemon::start(DaemonOptions {
            paths: Paths::at(home),
            port: Some(0),
            ingest: None,
        })
        .await
        .unwrap();
        Node {
            home: home.to_path_buf(),
            base: format!("http://127.0.0.1:{}", daemon.port),
            http: reqwest::Client::new(),
            daemon,
        }
    }

    fn id(&self) -> String {
        self.daemon.state.machine.id.clone()
    }

    async fn req(&self, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        let mut r = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(self.daemon.token())
            .timeout(Duration::from_secs(90));
        if let Some(b) = body {
            r = r.json(&b);
        }
        r.send().await.unwrap()
    }

    async fn ok<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> T {
        let r = self.req(method.clone(), path, body).await;
        let status = r.status();
        let text = r.text().await.unwrap();
        assert!(status.is_success(), "{method} {path}: {status} {text}");
        serde_json::from_str(&text).unwrap()
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        self.ok(Method::GET, path, None).await
    }

    fn store(&self) -> Store {
        Store::open(&self.home.join("blirp.db")).unwrap()
    }

    async fn project(&self, dir: &Path) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let p: ProjectSummary = self
            .ok(Method::POST, "/api/projects", Some(json!({"path": dir})))
            .await;
        p.project.id
    }

    async fn record(&self, project: &str, title: &str) -> Record {
        self.ok(
            Method::POST,
            &format!("/api/projects/{project}/records"),
            Some(json!({"kind": "note", "title": title, "body": "synced"})),
        )
        .await
    }

    async fn has_record(&self, project: &str, id: &str) -> bool {
        let r = self
            .req(
                Method::GET,
                &format!("/api/projects/{project}/memory"),
                None,
            )
            .await;
        r.status().is_success()
            && r.json::<ProjectMemory>()
                .await
                .unwrap()
                .records
                .iter()
                .any(|r| r.id == id)
    }
}

async fn eventually<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(45);
    while !f().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Minimal HTTPS client pinning the portal certificate by fingerprint.
struct Tls {
    port: u16,
    config: Arc<rustls::ClientConfig>,
}

#[derive(Debug)]
struct Pinned {
    fingerprint: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use sha2::Digest;
        let fp = sha2::Sha256::digest(end_entity.as_ref())
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":");
        if fp == self.fingerprint {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!("fingerprint {fp}")))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl Tls {
    fn new(port: u16, fingerprint: &str) -> Tls {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned {
                fingerprint: fingerprint.to_string(),
                provider,
            }))
            .with_no_client_auth();
        Tls {
            port,
            config: Arc::new(config),
        }
    }

    async fn connect(&self) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
            .await
            .unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        tokio_rustls::TlsConnector::from(self.config.clone())
            .connect(name, tcp)
            .await
            .unwrap()
    }

    /// `wss://` connection authenticated by a device cookie.
    async fn ws(
        &self,
        path: &str,
        cookie: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>
    {
        let mut req = format!("wss://127.0.0.1:{}{path}", self.port)
            .into_client_request()
            .unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (ws, _) = tokio_tungstenite::client_async(req, self.connect().await)
            .await
            .unwrap();
        ws
    }

    async fn req(
        &self,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        body: Option<Value>,
    ) -> (u16, axum::http::HeaderMap, Vec<u8>) {
        let stream = self.connect().await;
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .unwrap();
        tokio::spawn(conn);
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", format!("127.0.0.1:{}", self.port))
            .header("accept", "application/json, text/event-stream");
        if let Some(c) = cookie {
            req = req.header("cookie", c);
        }
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                axum::body::Body::from(b.to_string())
            }
            None => axum::body::Body::empty(),
        };
        let resp = sender.send_request(req.body(body).unwrap()).await.unwrap();
        let (parts, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(axum::body::Body::new(body), 1 << 20)
            .await
            .unwrap();
        (parts.status.as_u16(), parts.headers, bytes.to_vec())
    }
}

async fn sync_status(n: &Node) -> SyncStatus {
    n.get("/api/sync/status").await
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_replicate_proxy_revoke_and_portal() {
    let tmp = tempfile::tempdir().unwrap();
    let portal_port = free_port();
    let a = Node::start(&tmp.path().join("a"), "hub-a", Some(portal_port)).await;
    let b_home = tmp.path().join("b");
    let mut b = Node::start(&b_home, "node-b", None).await;

    // Machine ids are endpoint ids (64 hex chars).
    assert_eq!(a.id().len(), 64);
    let h: Health = b.get("/api/health").await;
    assert_eq!(h.machine.id, b.id());

    // ---- hub + pairing
    let st: SyncStatus = a.ok(Method::POST, "/api/sync/hub/enable", None).await;
    assert_eq!(st.role.as_str(), "hub");
    assert_eq!(st.hub.as_deref(), Some(a.id().as_str()));
    let inv: SyncInvite = a.ok(Method::POST, "/api/sync/invite", None).await;
    assert!(inv.invite.starts_with("blirp1-") && inv.uri.starts_with("blirp://join/"));
    assert_eq!(inv.code.len(), 9);

    let wrong = if inv.code.starts_with('A') {
        "BBBB-BBBB"
    } else {
        "AAAA-AAAA"
    };
    let r = b
        .req(
            Method::POST,
            "/api/sync/join",
            Some(json!({"invite": inv.invite, "code": wrong})),
        )
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<ErrorBody>().await.unwrap().error.code,
        "wrong_code"
    );

    let st: SyncStatus = b
        .ok(
            Method::POST,
            "/api/sync/join",
            Some(json!({"invite": inv.invite, "code": inv.code.to_lowercase()})),
        )
        .await;
    assert_eq!(st.role.as_str(), "node");
    assert_eq!(st.hub.as_deref(), Some(a.id().as_str()));
    // Single use.
    let r = b
        .req(
            Method::POST,
            "/api/sync/join",
            Some(json!({"invite": inv.invite, "code": inv.code})),
        )
        .await;
    assert_eq!(r.status(), 409, "already paired");
    let devices: Vec<Device> = a.get("/api/devices").await;
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].node_id.as_deref(), Some(b.id().as_str()));
    assert!(devices[0].can_control_terminals);

    eventually("node connected", || async {
        sync_status(&b).await.connected
    })
    .await;

    // ---- push: B -> A
    let pb = b.project(&tmp.path().join("work-b")).await;
    let rb = b.record(&pb, "from b").await;
    eventually("B's record on A", || a.has_record(&pb, &rb.id)).await;
    let machines: Vec<blirp_core::model::Machine> = a.get("/api/machines").await;
    assert!(
        machines
            .iter()
            .any(|m| m.id == b.id() && m.name == "node-b")
    );

    // ---- pull: A -> B (live notification)
    let ra = a.record(&pb, "from a").await;
    eventually("A's record on B", || b.has_record(&pb, &ra.id)).await;

    // No echo: B never queued A's record, and every B entry is logged once.
    let b_store = b.store();
    let b_outbox = b_store.outbox_batch(0, 100_000, usize::MAX).unwrap();
    assert!(!b_outbox.iter().any(|e| e.key == ra.id));
    eventually("B's outbox acknowledged", || async {
        sync_status(&b).await.pending_outbox == 0
    })
    .await;
    let a_store = a.store();
    let log = a_store.hub_page("nobody", 0, 100_000, usize::MAX).unwrap();
    let from_b: Vec<i64> = log
        .entries
        .iter()
        .filter_map(|e| match e {
            PulledEntry::Remote {
                origin_machine,
                entry,
                ..
            } if *origin_machine == b.id() => Some(entry.origin_seq),
            _ => None,
        })
        .collect();
    // Contiguous up to what B pushed: nothing lost, nothing logged twice.
    // (B's outbox itself is pruned once the hub logged its entries.)
    let b_pushed = b_store
        .sync_cursors(&a.id())
        .unwrap()
        .last_pushed_origin_seq;
    assert_eq!(from_b, (from_b[0]..=b_pushed).collect::<Vec<_>>());

    // ---- restart resume: A writes while B is down; B catches up after.
    b.daemon.shutdown().await.unwrap();
    let ra2 = a.record(&pb, "while b was down").await;
    b = Node::restart(&b_home).await;
    eventually("catch-up after restart", || b.has_record(&pb, &ra2.id)).await;
    let rb2 = b.record(&pb, "after restart").await;
    eventually("B's new record on A", || a.has_record(&pb, &rb2.id)).await;

    // ---- proxy: GET forwarded from B to A's API through the hub link.
    let resp = blirp::sync::forward(
        &b.daemon.state,
        &a.id(),
        &Principal::local(),
        Method::GET,
        "/api/health",
        None,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let health: Health = serde_json::from_slice(&body).unwrap();
    assert_eq!(health.machine.id, a.id());
    // Stopping a daemon is local-only: not mounted on the proxy router, even
    // for a relayed admin principal.
    let resp = blirp::sync::forward(
        &b.daemon.state,
        &a.id(),
        &Principal::local(),
        Method::POST,
        "/api/daemon/shutdown",
        None,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 404);
    // Admin routes stay admin-only through the proxy: a relayed request is
    // never admin, so no machine sets another's claude login token.
    let resp = blirp::sync::forward(
        &b.daemon.state,
        &a.id(),
        &Principal::local(),
        Method::PUT,
        "/api/agents/claude/token",
        Some(br#"{"token":"relayed-token"}"#.to_vec()),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 403);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(err["error"]["code"], "admin_only");
    assert!(!a.daemon.state.paths.claude_token_file().exists());

    // ---- remote launch on A from B, then attach its terminal from B.
    let a_dir = tmp.path().join("work-a");
    std::fs::create_dir_all(&a_dir).unwrap();
    let r = b
        .req(
            Method::POST,
            "/api/sessions",
            Some(
                json!({"cwd": a_dir, "agent": "shell", "machine": a.id(), "cols": 100, "rows": 30}),
            ),
        )
        .await;
    assert_eq!(r.status(), 201, "{:?}", r.text().await);
    let session: Session = r.json().await.unwrap();
    assert_eq!(session.machine_id, a.id());
    let url = format!(
        "{}/api/terminals/{}/ws",
        b.base.replace("http", "ws"),
        session.id
    );
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", b.daemon.token()).parse().unwrap(),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(30), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Text(t) = first else {
        panic!("expected snapshot, got {first:?}")
    };
    assert!(matches!(
        serde_json::from_str::<TerminalServerMessage>(&t).unwrap(),
        TerminalServerMessage::Snapshot {
            cols: 100,
            rows: 30,
            ..
        }
    ));
    let input = json!({"type": "input", "data": "echo REM$()OTE-9\r"}).to_string();
    ws.send(Message::Text(input.into())).await.unwrap();
    let mut screen = vt100::Parser::new(30, 100, 1000);
    let deadline = Instant::now() + Duration::from_secs(45);
    while !screen.screen().contents().contains("REMOTE-9") {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                screen.process(&bytes);
                if bytes.windows(4).any(|w| w == b"\x1b[6n") {
                    let (r, c) = screen.screen().cursor_position();
                    let reply = format!("\x1b[{};{}R", r + 1, c + 1);
                    ws.send(Message::Binary(reply.into_bytes().into()))
                        .await
                        .unwrap();
                }
            }
            Ok(Some(Ok(_))) => {}
            other => panic!(
                "remote terminal ended: {other:?}\n{}",
                screen.screen().contents()
            ),
        }
    }
    let r = b
        .req(
            Method::POST,
            &format!("/api/sessions/{}/stop", session.id),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status(), 202);
    drop(ws);

    // ---- the node has not opted in to hub control: the hub may read it
    // but not launch or change anything on it.
    let b_dir = tmp.path().join("work-b2");
    std::fs::create_dir_all(&b_dir).unwrap();
    let r = a
        .req(
            Method::POST,
            "/api/sessions",
            Some(json!({"cwd": b_dir, "agent": "shell", "machine": b.id()})),
        )
        .await;
    assert_eq!(r.status(), 403);
    assert_eq!(
        r.json::<ErrorBody>().await.unwrap().error.code,
        "control_not_allowed"
    );
    let resp = blirp::sync::forward(
        &a.daemon.state,
        &b.id(),
        &Principal::local(),
        Method::GET,
        "/api/health",
        None,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);

    // ---- LAN portal: one-time invite -> device cookie over TLS, with the
    // certificate pinned to the fingerprint the API reports.
    let st: SyncStatus = a.get("/api/sync/status").await;
    let portal = st.portal_url.clone().expect("portal url");
    let fp = st.portal_cert_fingerprint.clone().expect("fingerprint");
    assert_eq!(fp.len(), 95);
    let port: u16 = portal.rsplit(':').next().unwrap().parse().unwrap();
    assert_eq!(port, portal_port);
    let tls = Tls::new(port, &fp);
    let invite: BrowserInvite = a
        .ok(Method::POST, "/api/devices/browser-invite", None)
        .await;
    let login = &invite.url[invite.url.find("/device-login").unwrap()..];
    let (status, _, _) = tls.req("GET", "/api/health", None, None).await;
    assert_eq!(status, 401, "runtime token is not accepted on the portal");
    let (status, headers, _) = tls.req("GET", login, None, None).await;
    assert_eq!(status, 303);
    let cookie = headers["set-cookie"].to_str().unwrap().to_string();
    for attr in ["HttpOnly", "Secure", "SameSite=Strict"] {
        assert!(cookie.contains(attr), "{cookie}");
    }
    let cookie = cookie.split(';').next().unwrap().to_string();
    let (status, _, _) = tls.req("GET", login, None, None).await;
    assert_eq!(status, 401, "invite is single use");
    let (status, _, _) = tls
        .req("GET", "/api/sync/status", Some(&cookie), None)
        .await;
    assert_eq!(status, 200);
    // Browser devices start without terminal control and cannot manage the hub.
    let (status, _, _) = tls
        .req(
            "POST",
            "/api/sessions",
            Some(&cookie),
            Some(json!({"cwd": a_dir, "agent": "shell"})),
        )
        .await;
    assert_eq!(status, 403);
    let (status, _, _) = tls
        .req("POST", "/api/sync/invite", Some(&cookie), None)
        .await;
    assert_eq!(status, 403);
    // Hook ingress and global integration are for the machine's own clients.
    let (status, _, _) = tls
        .req(
            "POST",
            "/api/agents/claude/hooks/install",
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, 403);
    // Stopping the daemon is local-only: not mounted on the portal.
    let (status, _, _) = tls
        .req("POST", "/api/daemon/shutdown", Some(&cookie), None)
        .await;
    assert_eq!(status, 404);
    // `/mcp` is local-only: not mounted on the portal, even with a loopback Host.
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}});
    let (_, _, body) = tls.req("POST", "/mcp", Some(&cookie), Some(init)).await;
    assert!(
        !String::from_utf8_lossy(&body).contains("serverInfo"),
        "MCP answered on the LAN portal"
    );
    let devices: Vec<Device> = a.get("/api/devices").await;
    let browser = devices
        .iter()
        .find(|d| d.kind.as_str() == "browser")
        .unwrap();
    assert!(!browser.can_control_terminals);
    // A live event stream of the device ends as soon as it is revoked.
    let mut events = tls.ws("/api/events/ws", &cookie).await;
    let r = a
        .req(
            Method::DELETE,
            &format!("/api/devices/{}", browser.id),
            None,
        )
        .await;
    assert_eq!(r.status(), 204);
    loop {
        match tokio::time::timeout(Duration::from_secs(10), events.next())
            .await
            .expect("revoked device's socket stays open")
        {
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
            Some(Ok(_)) => {}
        }
    }
    let (status, _, _) = tls
        .req("GET", "/api/sync/status", Some(&cookie), None)
        .await;
    assert_eq!(status, 401, "revoked browser device");

    // ---- revocation of B closes its connection immediately.
    let r = a
        .req(Method::DELETE, &format!("/api/machines/{}", b.id()), None)
        .await;
    assert_eq!(r.status(), 204);
    eventually("B disconnected", || async {
        !sync_status(&b).await.connected
    })
    .await;
    let rb3 = b.record(&pb, "after revoke").await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!a.has_record(&pb, &rb3.id).await);
    let machines: Vec<blirp_core::model::Machine> = a.get("/api/machines").await;
    assert!(machines.iter().any(|m| m.id == b.id() && m.revoked));

    b.daemon.shutdown().await.unwrap();
    a.daemon.shutdown().await.unwrap();
}

// A settings save from a copy read before "Enable hub" (the UI's snapshot)
// must not write the old role back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_saved_from_a_stale_copy_keep_the_role() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Node::start(&tmp.path().join("a"), "hub-a", None).await;
    let stale: Value = a.get("/api/settings").await;
    assert_eq!(stale["config"]["sync"]["role"], "standalone");
    let _: SyncStatus = a.ok(Method::POST, "/api/sync/hub/enable", None).await;

    // Rename the machine from the stale copy, with and without its base.
    let mut edited = stale["config"].clone();
    edited["machine"]["name"] = json!("renamed");
    let view: Value = a
        .ok(
            Method::PATCH,
            "/api/settings",
            Some(json!({"config": edited, "base": stale["config"]})),
        )
        .await;
    assert_eq!(view["config"]["sync"]["role"], "hub");
    assert_eq!(view["config"]["machine"]["name"], "renamed");
    edited["portal"]["lan_port"] = json!(free_port());
    let view: Value = a
        .ok(
            Method::PATCH,
            "/api/settings",
            Some(json!({"config": edited})),
        )
        .await;
    assert_eq!(view["config"]["sync"]["role"], "hub");
    assert_eq!(
        view["config"]["portal"]["lan_port"],
        edited["portal"]["lan_port"]
    );

    assert_eq!(sync_status(&a).await.role.as_str(), "hub");
    let on_disk = std::fs::read_to_string(a.home.join("config.toml")).unwrap();
    assert!(on_disk.contains("role = \"hub\""), "{on_disk}");
    a.daemon.shutdown().await.unwrap();
}

/// Whether something accepts TCP connections on `port` (loopback).
async fn listening(port: u16) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
}

// Settings changes to the LAN portal apply live: no daemon restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn portal_follows_settings_live() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Node::start(&tmp.path().join("a"), "hub-a", None).await;
    let st: SyncStatus = a.ok(Method::POST, "/api/sync/hub/enable", None).await;
    assert!(st.portal_url.is_none());

    let set_portal = |lan: bool, port: u16| {
        let a = &a;
        async move {
            let mut view: Value = a.get("/api/settings").await;
            view["config"]["portal"] = json!({"lan": lan, "lan_port": port});
            let _: Value = a
                .ok(
                    Method::PATCH,
                    "/api/settings",
                    Some(json!({"config": view["config"]})),
                )
                .await;
            sync_status(a).await.portal_url
        }
    };
    let port_of = |url: Option<String>| -> Option<u16> {
        url.map(|u| u.rsplit(':').next().unwrap().parse().unwrap())
    };

    let p1 = free_port();
    assert_eq!(port_of(set_portal(true, p1).await), Some(p1));
    assert!(listening(p1).await);

    // A new port moves the listener.
    let p2 = free_port();
    assert_eq!(port_of(set_portal(true, p2).await), Some(p2));
    assert!(listening(p2).await);
    assert!(!listening(p1).await, "old portal port still open");

    // Hub enable on an existing hub re-applies the portal config too.
    let st: SyncStatus = a.ok(Method::POST, "/api/sync/hub/enable", None).await;
    assert_eq!(port_of(st.portal_url), Some(p2));

    // Turning it off closes it.
    assert_eq!(set_portal(false, p2).await, None);
    assert!(!listening(p2).await);

    // A port that is taken is reported, and the setting is kept.
    let taken = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let busy = taken.local_addr().unwrap().port();
    let mut view: Value = a.get("/api/settings").await;
    view["config"]["portal"] = json!({"lan": true, "lan_port": busy});
    let r = a
        .req(
            Method::PATCH,
            "/api/settings",
            Some(json!({"config": view["config"]})),
        )
        .await;
    assert_eq!(r.status(), 409);
    let view: Value = a.get("/api/settings").await;
    assert_eq!(view["config"]["portal"]["lan_port"], busy);
    a.daemon.shutdown().await.unwrap();
}

async fn set_lan_discovery(n: &Node, on: bool) -> reqwest::Response {
    let mut view: Value = n.get("/api/settings").await;
    view["config"]["sync"]["lan_discovery"] = json!(on);
    n.req(
        Method::PATCH,
        "/api/settings",
        Some(json!({"config": view["config"]})),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lan_discovery_follows_settings_live() {
    let tmp = tempfile::tempdir().unwrap();

    // Standalone: saved, no endpoint started; a join without an invite
    // needs discovery and is refused before anything is contacted.
    let s = Node::start(&tmp.path().join("s"), "solo", None).await;
    assert!(set_lan_discovery(&s, false).await.status().is_success());
    assert!(s.daemon.state.sync.service().is_none());
    let view: Value = s.get("/api/settings").await;
    assert_eq!(view["config"]["sync"]["lan_discovery"], false);
    let r = s
        .req(
            Method::POST,
            "/api/sync/join",
            Some(json!({"invite": "", "code": "ABCD-EFGH"})),
        )
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<ErrorBody>().await.unwrap().error.code,
        "invite_required"
    );
    assert_eq!(sync_status(&s).await.role.as_str(), "standalone");
    s.daemon.shutdown().await.unwrap();

    // Hub: the running endpoint restarts with the new setting, same role.
    let h = Node::start(&tmp.path().join("h"), "hub-h", None).await;
    let _: SyncStatus = h.ok(Method::POST, "/api/sync/hub/enable", None).await;
    let before = h.daemon.state.sync.service().unwrap();
    // A save that does not change it leaves the endpoint alone.
    assert!(set_lan_discovery(&h, true).await.status().is_success());
    assert!(Arc::ptr_eq(
        &before,
        &h.daemon.state.sync.service().unwrap()
    ));
    assert!(set_lan_discovery(&h, false).await.status().is_success());
    let after = h.daemon.state.sync.service().unwrap();
    assert!(!Arc::ptr_eq(&before, &after), "endpoint was not restarted");
    assert!(after.is_hub());
    assert_eq!(after.id(), h.id());
    let st = sync_status(&h).await;
    assert_eq!(st.role.as_str(), "hub");
    assert!(st.connected);
    // Invites still work without discovery.
    let _: SyncInvite = h.ok(Method::POST, "/api/sync/invite", None).await;
    let view: Value = h.get("/api/settings").await;
    assert_eq!(view["config"]["sync"]["lan_discovery"], false);
    assert_eq!(view["config"]["sync"]["role"], "hub");
    // And back on.
    assert!(set_lan_discovery(&h, true).await.status().is_success());
    assert!(!Arc::ptr_eq(
        &after,
        &h.daemon.state.sync.service().unwrap()
    ));
    assert!(sync_status(&h).await.connected);
    h.daemon.shutdown().await.unwrap();
}
