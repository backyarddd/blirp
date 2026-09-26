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
    // A Stop is recorded as intent, not as the kill's exit code.
    assert_eq!(exit, (SessionStatus::Completed, None));
    let done = wait_status(&h, &session.id, SessionStatus::Completed).await;
    assert!(done.ended_at.is_some());
    assert_eq!(done.exit_code, None);
    assert!(done.stopped_by_user);

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
    assert!(!resumed.stopped_by_user);

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

    // Folders must be absolute: relative ones would resolve against the
    // daemon's working directory (`src` exists relative to this test's).
    for (path, body) in [
        ("/api/projects", json!({"path": "src"})),
        ("/api/sessions", json!({"cwd": "src", "agent": "shell"})),
    ] {
        let r = h.send(reqwest::Method::POST, path, body).await;
        assert_eq!(r.status(), 400, "{path}");
        assert_eq!(
            r.json::<ErrorBody>().await.unwrap().error.code,
            "invalid_request"
        );
    }

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
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<ErrorBody>().await.unwrap().error.code,
        "invalid_request"
    );
    // Every extractor answers 400 with the error shape: JSON syntax, query
    // and path parameters.
    let r = h
        .http
        .post(h.url(&format!("/api/projects/{id}/records")))
        .bearer_auth(&h.token)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<ErrorBody>().await.unwrap().error.code,
        "invalid_request"
    );
    for path in ["/api/sessions?limit=many", "/api/sessions/%FF"] {
        let r = h
            .http
            .get(h.url(path))
            .bearer_auth(&h.token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{path}");
        assert_eq!(
            r.json::<ErrorBody>().await.unwrap().error.code,
            "invalid_request",
            "{path}"
        );
    }

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

async fn wait_no_terminal(h: &Harness, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while h.daemon.state.terminals.get(id).is_some() {
        assert!(Instant::now() < deadline, "terminal {id} never went away");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// Concurrent resumes of one session must start exactly one agent; the
// others are refused while the first is starting or running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_resumes_start_one_agent() {
    let h = Harness::start().await;
    let proj = h._home.path().join("race");
    std::fs::create_dir(&proj).unwrap();
    let r = h
        .send(
            reqwest::Method::POST,
            "/api/sessions",
            json!({"cwd": proj, "agent": "shell"}),
        )
        .await;
    assert_eq!(r.status(), 201);
    let session: Session = r.json().await.unwrap();
    let stop = format!("/api/sessions/{}/stop", session.id);
    assert_eq!(
        h.send(reqwest::Method::POST, &stop, json!({}))
            .await
            .status(),
        202
    );
    wait_status(&h, &session.id, SessionStatus::Completed).await;
    wait_no_terminal(&h, &session.id).await;

    for _ in 0..3 {
        let path = format!("/api/sessions/{}/resume", session.id);
        let codes: Vec<u16> = futures_util::future::join_all(
            (0..8).map(|_| h.send(reqwest::Method::POST, &path, json!({}))),
        )
        .await
        .into_iter()
        .map(|r| r.status().as_u16())
        .collect();
        assert_eq!(codes.iter().filter(|c| **c == 200).count(), 1, "{codes:?}");
        assert!(codes.iter().all(|c| *c == 200 || *c == 409), "{codes:?}");
        let live = h.daemon.state.terminals.all();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].session_id, session.id);

        // Stopping ends the one agent and frees the session for the next round.
        assert_eq!(
            h.send(reqwest::Method::POST, &stop, json!({}))
                .await
                .status(),
            202
        );
        wait_status(&h, &session.id, SessionStatus::Completed).await;
        wait_no_terminal(&h, &session.id).await;
        assert!(h.daemon.state.terminals.is_empty());
    }
    let Harness { daemon, _home, .. } = h;
    daemon.shutdown().await.unwrap();
}

// Ingested subagent sessions stay out of the default list; `parent=` lists
// them and the detail counts them.
#[tokio::test]
async fn subagent_children_are_filtered_and_counted() {
    use blirp_core::model::{SessionDetail, SessionOrigin, SessionsPage};
    let h = Harness::start().await;
    let store = &h.daemon.state.store;
    let proj = h._home.path().join("kids");
    std::fs::create_dir(&proj).unwrap();
    let project = store
        .register_project(&h.daemon.state.machine.id, &proj, None)
        .unwrap();
    let mk = |id: &str, parent: Option<&str>, origin: SessionOrigin, at: i64| Session {
        id: id.into(),
        project_id: project.id.clone(),
        machine_id: h.daemon.state.machine.id.clone(),
        agent: "claude".into(),
        agent_session_id: Some(format!("asid-{id}")),
        origin,
        cwd: proj.display().to_string(),
        title: None,
        status: SessionStatus::Completed,
        branch: None,
        worktree: None,
        transcript_path: None,
        started_at: at,
        ended_at: Some(at),
        last_activity_at: at,
        exit_code: None,
        summary: None,
        distilled_through_seq: 0,
        tokens_in: 0,
        tokens_out: 0,
        cost_usd: 0.0,
        parent_session_id: parent.map(str::to_string),
        stopped_by_user: false,
    };
    store
        .insert_session(&mk("top", None, SessionOrigin::External, 1))
        .unwrap();
    store
        .insert_session(&mk("fork", Some("top"), SessionOrigin::Blirp, 2))
        .unwrap();
    store
        .insert_session(&mk("sub", Some("top"), SessionOrigin::External, 3))
        .unwrap();
    let ids = |p: SessionsPage| p.items.into_iter().map(|s| s.id).collect::<Vec<_>>();
    assert_eq!(ids(h.get("/api/sessions").await), ["fork", "top"]);
    assert_eq!(
        ids(h.get("/api/sessions?include_children=true").await),
        ["sub", "fork", "top"]
    );
    assert_eq!(ids(h.get("/api/sessions?parent=top").await), ["sub"]);
    let d: SessionDetail = h.get("/api/sessions/top").await;
    assert_eq!((d.session.id.as_str(), d.children_count), ("top", 1));
    h.daemon.shutdown().await.unwrap();
}
