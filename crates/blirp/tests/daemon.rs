//! End-to-end: run the daemon in-process on a temp BLIRP_HOME and an
//! ephemeral port, drive it over HTTP and WebSocket.

// Test helpers panic on failure by design.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use blirp::daemon::{Daemon, DaemonOptions};
use blirp_core::model::{
    ErrorBody, Health, ProjectMemory, ProjectSummary, Record, SearchResults, Session,
    SessionStatus, TerminalServerMessage,
};
use blirp_core::paths::{Paths, RuntimeInfo};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Harness {
    _home: tempfile::TempDir,
    daemon: Daemon,
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Harness {
    async fn start() -> Harness {
        let home = tempfile::tempdir().unwrap();
        let daemon = Daemon::start(DaemonOptions {
            paths: Paths::at(home.path()),
            port: Some(0),
            ingest: None,
        })
        .await
        .unwrap();
        Harness {
            base: format!("http://127.0.0.1:{}", daemon.port),
            token: daemon.token().to_string(),
            http: reqwest::Client::new(),
            daemon,
            _home: home,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        let r = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "GET {path}: {}", r.status());
        r.json().await.unwrap()
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: serde_json::Value,
    ) -> reqwest::Response {
        self.http
            .request(method, self.url(path))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn ws(&self, session_id: &str) -> Ws {
        let url = format!(
            "ws://127.0.0.1:{}/api/terminals/{session_id}/ws",
            self.daemon.port
        );
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", self.token).parse().unwrap(),
        );
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        ws
    }
}

async fn next_text(ws: &mut Ws) -> TerminalServerMessage {
    loop {
        match tokio::time::timeout(Duration::from_secs(20), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => return serde_json::from_str(&t).unwrap(),
            Ok(Some(Ok(_))) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

/// Read terminal output until the rendered screen contains `needle`,
/// answering cursor-position queries the way xterm.js would.
async fn read_until(
    ws: &mut Ws,
    screen: &mut vt100::Parser,
    needle: &str,
) -> Vec<TerminalServerMessage> {
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut control = Vec::new();
    while !screen.screen().contents().contains(needle) {
        let left = deadline.saturating_duration_since(Instant::now());
        let msg = tokio::time::timeout(left, ws.next())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out waiting for {needle:?}; screen:\n{}",
                    screen.screen().contents()
                )
            });
        match msg {
            Some(Ok(Message::Binary(b))) => {
                screen.process(&b);
                if b.windows(4).any(|w| w == b"\x1b[6n") {
                    let (r, c) = screen.screen().cursor_position();
                    let reply = format!("\x1b[{};{}R", r + 1, c + 1);
                    ws.send(Message::Binary(reply.into_bytes().into()))
                        .await
                        .unwrap();
                }
            }
            Some(Ok(Message::Text(t))) => control.push(serde_json::from_str(&t).unwrap()),
            Some(Ok(_)) => {}
            other => panic!(
                "socket ended before {needle:?}: {other:?}; screen:\n{}",
                screen.screen().contents()
            ),
        }
    }
    control
}

async fn wait_status(h: &Harness, id: &str, want: SessionStatus) -> Session {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let s: Session = h.get(&format!("/api/sessions/{id}")).await;
        if s.status == want {
            return s;
        }
        assert!(Instant::now() < deadline, "session stuck in {:?}", s.status);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_session_over_websocket() {
    let h = Harness::start().await;
    let proj = h._home.path().join("my-folder");
    std::fs::create_dir(&proj).unwrap();

    let resp = h
        .send(
            reqwest::Method::POST,
            "/api/sessions",
            json!({"cwd": proj, "agent": "shell", "cols": 100, "rows": 30}),
        )
        .await;
    assert_eq!(resp.status(), 201, "{:?}", resp.text().await);
    let session: Session = resp.json().await.unwrap();
    assert_eq!(session.status, SessionStatus::Starting);
    assert_eq!(session.agent, "shell");

    let mut ws = h.ws(&session.id).await;
    let TerminalServerMessage::Snapshot { cols, rows, .. } = next_text(&mut ws).await else {
        panic!("first frame must be a snapshot");
    };
    assert_eq!((cols, rows), (100, 30));

    // `$()` expands to nothing in pwsh, sh, bash, zsh and fish, so the marker
    // only appears once the shell has actually run the command.
    let input = json!({"type": "input", "data": "echo MARK$()ER-42\r"}).to_string();
    ws.send(Message::Text(input.into())).await.unwrap();
    let mut screen = vt100::Parser::new(30, 100, 1000);
    read_until(&mut ws, &mut screen, "MARKER-42").await;

    // A second client gets the marker in its snapshot.
    let mut ws2 = h.ws(&session.id).await;
    let TerminalServerMessage::Snapshot { data, .. } = next_text(&mut ws2).await else {
        panic!("first frame must be a snapshot");
    };
    let mut replay = vt100::Parser::new(30, 100, 1000);
    replay.process(data.as_bytes());
    assert!(replay.screen().contents().contains("MARKER-42"), "{data:?}");
    drop(ws2);

    let working: Session = h.get(&format!("/api/sessions/{}", session.id)).await;
    assert!(working.status.is_live(), "{:?}", working.status);

    let r = h
        .send(
            reqwest::Method::POST,
            &format!("/api/sessions/{}/stop", session.id),
            json!({}),
        )
        .await;
    assert_eq!(r.status(), 202);
    let exit = loop {
        match next_text(&mut ws).await {
            TerminalServerMessage::Exit { status, exit_code } => break (status, exit_code),
            _ => continue,
        }
    };
    assert_eq!(exit.0, SessionStatus::Completed);
    let done = wait_status(&h, &session.id, SessionStatus::Completed).await;
    assert!(done.ended_at.is_some() && done.exit_code.is_some());

    // Stopping again is a conflict; resuming relaunches in the same row.
    let r = h
        .send(
            reqwest::Method::POST,
            &format!("/api/sessions/{}/stop", session.id),
            json!({}),
        )
        .await;
    assert_eq!(r.status(), 409);
    let r = h
        .send(
            reqwest::Method::POST,
            &format!("/api/sessions/{}/resume", session.id),
            json!({}),
        )
        .await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    let resumed: Session = r.json().await.unwrap();
    assert_eq!(resumed.status, SessionStatus::Starting);
    assert!(resumed.ended_at.is_none());

    // An initial prompt is typed once output settles, then submitted.
    let r = h
        .send(
            reqwest::Method::POST,
            "/api/sessions",
            json!({"project_id": session.project_id, "agent": "shell", "prompt": "echo PROM$()PT-7"}),
        )
        .await;
    assert_eq!(r.status(), 201, "{:?}", r.text().await);
    let prompted: Session = r.json().await.unwrap();
    assert_eq!(prompted.project_id, session.project_id);
    let mut ws3 = h.ws(&prompted.id).await;
    next_text(&mut ws3).await;
    let mut screen = vt100::Parser::new(32, 120, 1000);
    read_until(&mut ws3, &mut screen, "PROMPT-7").await;

    // Daemon shutdown ends live sessions as detached and removes runtime.json.
    let paths = Paths::at(h._home.path());
    assert!(RuntimeInfo::read(&paths).unwrap().is_some());
    let store_path = paths.db_file();
    let Harness { daemon, _home, .. } = h;
    daemon.shutdown().await.unwrap();
    assert!(RuntimeInfo::read(&paths).unwrap().is_none());
    let store = blirp_core::store::Store::open(&store_path).unwrap();
    let s = store.get_session(&session.id).unwrap().unwrap();
    assert_eq!(s.status, SessionStatus::Detached);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_auth_projects_memory_files() {
    let h = Harness::start().await;

    // Auth and CSRF.
    let r = h.http.get(h.url("/api/health")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let err: ErrorBody = r.json().await.unwrap();
    assert_eq!(err.error.code, "unauthorized");
    let health: Health = h.get("/api/health").await;
    assert_eq!(health.version, env!("CARGO_PKG_VERSION"));
    let r = h
        .http
        .post(h.url("/api/projects"))
        .bearer_auth(&h.token)
        .header("Origin", "http://evil.example")
        .json(&json!({"path": "."}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = h.http.get(h.url("/")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-frame-options"], "DENY");
    assert!(
        r.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'self'")
    );
    let r = h
        .http
        .get(h.url(&format!("/auth?token={}", h.token)))
        .send()
        .await
        .unwrap();
    // reqwest follows the redirect to `/`; the cookie was set on the 303.
    assert_eq!(r.status(), 200);
    let r = h.http.get(h.url("/auth?token=wrong")).send().await.unwrap();
    assert_eq!(r.status(), 401);

    // Register a folder; files API stays inside it.
    let proj = h._home.path().join("proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::write(proj.join("src/main.rs"), "fn main() {}\n").unwrap();
    let r = h
        .send(
            reqwest::Method::POST,
            "/api/projects",
            json!({"path": proj, "name": "Proj"}),
        )
        .await;
    assert_eq!(r.status(), 201);
    let p: ProjectSummary = r.json().await.unwrap();
    assert!(!p.is_git);
    let id = p.project.id.clone();
    let list: Vec<ProjectSummary> = h.get("/api/projects").await;
    assert_eq!(list.len(), 1);

    let listing: serde_json::Value = h.get(&format!("/api/projects/{id}/files?path=src")).await;
    assert_eq!(listing["entries"][0]["path"], "src/main.rs");
    let content: serde_json::Value = h
        .get(&format!(
            "/api/projects/{id}/files/content?path=src/main.rs"
        ))
        .await;
    assert_eq!(content["content"], "fn main() {}\n");
    let r = h
        .http
        .get(h.url(&format!(
            "/api/projects/{id}/files/content?path=../config.toml"
        )))
        .bearer_auth(&h.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let err: ErrorBody = r.json().await.unwrap();
    assert_eq!(err.error.code, "path_outside_root");
    let r = h
        .http
        .get(h.url(&format!("/api/projects/{id}/git")))
        .bearer_auth(&h.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(r.json::<ErrorBody>().await.unwrap().error.code, "not_git");

    // Memory: brief, records, search.
    let r = h
        .send(
            reqwest::Method::PUT,
            &format!("/api/projects/{id}/brief"),
            json!({"body_md": "Uses SQLite."}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let r = h
        .send(
            reqwest::Method::POST,
            &format!("/api/projects/{id}/records"),
            json!({"kind": "decision", "title": "Adopt WAL journaling", "body": "Readers never block the writer."}),
        )
        .await;
    assert_eq!(r.status(), 201);
    let rec: Record = r.json().await.unwrap();
    let mem: ProjectMemory = h.get(&format!("/api/projects/{id}/memory")).await;
    assert_eq!(mem.brief.unwrap().version, 1);
    assert_eq!(mem.records.len(), 1);
    let hits: SearchResults = h.get("/api/search?q=journal").await;
    assert_eq!(hits.hits[0].record_id.as_deref(), Some(rec.id.as_str()));
    let r = h
        .send(
            reqwest::Method::POST,
            &format!("/api/projects/{id}/records"),
            json!({"kind": "nope", "title": "x", "body": ""}),
        )
        .await;
    assert_eq!(r.status(), 422);
    assert_eq!(
        r.json::<ErrorBody>().await.unwrap().error.code,
        "invalid_request"
    );

    // Unknown endpoints answer 404 with the standard error shape.
    let r = h
        .send(reqwest::Method::POST, "/api/no-such-endpoint", json!({}))
        .await;
    assert_eq!(r.status(), 404);
    assert_eq!(r.json::<ErrorBody>().await.unwrap().error.code, "not_found");

    // Unknown agent is rejected before anything is spawned.
    let r = h
        .send(
            reqwest::Method::POST,
            "/api/sessions",
            json!({"cwd": proj, "agent": "vim"}),
        )
        .await;
    assert_eq!(r.status(), 400);

    h.daemon.shutdown().await.unwrap();
}

// The desktop tray stops the daemon over the API (no signal path on Windows).
#[tokio::test]
async fn shutdown_endpoint_requests_stop() {
    let h = Harness::start().await;
    let r = h
        .http
        .post(h.url("/api/daemon/shutdown"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = h
        .send(reqwest::Method::POST, "/api/daemon/shutdown", json!({}))
        .await;
    assert_eq!(r.status(), 202);
    tokio::time::timeout(
        Duration::from_secs(5),
        h.daemon.state.stop_requested.notified(),
    )
    .await
    .expect("stop was not requested");
    h.daemon.shutdown().await.unwrap();
}
