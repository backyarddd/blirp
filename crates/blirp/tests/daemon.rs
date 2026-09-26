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
    // DNS rebinding: a page on another name that resolves to 127.0.0.1.
    let r = h
        .http
        .get(h.url("/api/health"))
        .bearer_auth(&h.token)
        .header("Host", format!("rebind.example:{}", h.daemon.port))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(
        r.json::<ErrorBody>().await.unwrap().error.code,
        "forbidden_host"
    );
    let r = h.http.get(h.url("/")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-frame-options"], "DENY");
    assert!(
        r.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'self'")
    );

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

    // Register a folder; files API stays inside it. Not inside the data
    // dir (BLIRP_HOME), which the files API never serves.
    let work = tempfile::tempdir().unwrap();
    let proj = work.path().join("proj");
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

    // Deleting takes the subagents along; the fork stays, unlinked.
    let r = h
        .send(reqwest::Method::DELETE, "/api/sessions/top", json!({}))
        .await;
    assert_eq!(r.status(), 204);
    let r = h
        .send(reqwest::Method::DELETE, "/api/sessions/top", json!({}))
        .await;
    assert_eq!(r.status(), 404);
    assert_eq!(
        ids(h.get("/api/sessions?include_children=true").await),
        ["fork"]
    );
    // A running session is refused.
    store
        .insert_session(&Session {
            status: SessionStatus::Working,
            ..mk("live", None, SessionOrigin::External, 4)
        })
        .unwrap();
    let r = h
        .send(reqwest::Method::DELETE, "/api/sessions/live", json!({}))
        .await;
    assert_eq!(r.status(), 409);
    // Another machine's session: stop, resume and delete are all forwarded
    // to it and fail the same specific way when it cannot be reached.
    store
        .insert_session(&Session {
            machine_id: "other-machine".into(),
            ..mk("theirs", None, SessionOrigin::External, 5)
        })
        .unwrap();
    for (method, path) in [
        (reqwest::Method::POST, "/api/sessions/theirs/stop"),
        (reqwest::Method::POST, "/api/sessions/theirs/resume"),
        (reqwest::Method::DELETE, "/api/sessions/theirs"),
    ] {
        let r = h.send(method, path, json!({})).await;
        assert_eq!(r.status(), 409, "{path}");
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(body["error"]["code"], "machine_unreachable", "{path}");
    }
    h.daemon.shutdown().await.unwrap();
}

/// Every mutating route with the right it needs (§11). A route missing
/// here is a route nobody checked.
const MUTATING_ROUTES: &[(&str, &str, Need)] = &[
    ("PATCH", "/api/settings", Need::Admin),
    ("POST", "/api/sync/hub/enable", Need::Admin),
    ("POST", "/api/sync/hub/disable", Need::Admin),
    ("POST", "/api/sync/invite", Need::Admin),
    ("POST", "/api/sync/join", Need::Admin),
    ("POST", "/api/sync/join/preview", Need::Admin),
    ("POST", "/api/devices/browser-invite", Need::Admin),
    ("PATCH", "/api/devices/d1", Need::Admin),
    ("DELETE", "/api/devices/d1", Need::Admin),
    ("DELETE", "/api/machines/m1", Need::Admin),
    ("POST", "/api/machines/m1/clone", Need::Control),
    ("POST", "/api/agents/claude/hooks/install", Need::Admin),
    ("POST", "/api/agents/claude/hooks/uninstall", Need::Admin),
    ("POST", "/api/hooks/claude/Stop", Need::Admin),
    ("POST", "/api/sessions/s1/open", Need::Admin),
    ("POST", "/api/sessions", Need::Control),
    ("DELETE", "/api/sessions/s1", Need::Control),
    ("POST", "/api/sessions/s1/worktree/remove", Need::Control),
    ("PATCH", "/api/sessions/s1", Need::Control),
    ("POST", "/api/sessions/s1/stop", Need::Control),
    ("POST", "/api/sessions/s1/resume", Need::Control),
    ("POST", "/api/sessions/s1/distill", Need::Control),
    ("POST", "/api/projects", Need::Control),
    ("PATCH", "/api/projects/p1", Need::Control),
    ("DELETE", "/api/projects/p1", Need::Control),
    ("POST", "/api/projects/p1/merge", Need::Control),
    ("PUT", "/api/projects/p1/brief", Need::Control),
    ("POST", "/api/projects/p1/brief/revert", Need::Control),
    ("POST", "/api/projects/p1/records", Need::Control),
    ("PATCH", "/api/projects/p1/records/r1", Need::Control),
    ("DELETE", "/api/projects/p1/records/r1", Need::Control),
    ("POST", "/api/projects/p1/wiki", Need::Control),
    ("PUT", "/api/projects/p1/wiki/w1", Need::Control),
    ("DELETE", "/api/projects/p1/wiki/w1", Need::Control),
    ("POST", "/api/projects/p1/resources", Need::Control),
    ("PATCH", "/api/projects/p1/resources/r1", Need::Control),
    ("DELETE", "/api/projects/p1/resources/r1", Need::Control),
    ("POST", "/api/suggestions/x1/accept", Need::Control),
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Need {
    Admin,
    Control,
}

/// A LAN portal browser device (never admin) and its cookie.
fn portal_device(h: &Harness, control: bool) -> String {
    use sha2::Digest as _;
    let token = format!("{:064x}", u128::from(control) + 7);
    let now = blirp_core::now_ms();
    h.daemon
        .state
        .store
        .upsert_device(&blirp_core::model::Device {
            id: format!("dev-{control}"),
            name: "phone".into(),
            kind: blirp_core::model::DeviceKind::Browser,
            token_hash: Some(hex::encode(sha2::Sha256::digest(token.as_bytes()))),
            node_id: None,
            created_at: now,
            last_seen: now,
            revoked: false,
            can_control_terminals: control,
        })
        .unwrap();
    format!("blirp_device={token}")
}

async fn portal_call(
    app: &axum::Router,
    method: &str,
    path: &str,
    cookie: &str,
) -> (u16, serde_json::Value) {
    use tower::ServiceExt as _;
    let req = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("cookie", cookie)
        .header("content-type", "application/json")
        .body(axum::body::Body::from("{}"))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

// Portal devices are never admins; without terminal control they are
// read-only. Every mutating route refuses them before looking at the body.
#[tokio::test]
async fn portal_devices_get_only_their_rights() {
    let h = Harness::start().await;
    let app = blirp::api::portal_router(h.daemon.state.clone());
    let viewer = portal_device(&h, false);
    let controller = portal_device(&h, true);

    for (method, path, need) in MUTATING_ROUTES {
        let (status, body) = portal_call(&app, method, path, &viewer).await;
        assert_eq!(status, 403, "viewer {method} {path}: {body}");
        let want = if *need == Need::Admin {
            "admin_only"
        } else {
            "control_not_allowed"
        };
        assert_eq!(body["error"]["code"], want, "viewer {method} {path}");

        let (status, body) = portal_call(&app, method, path, &controller).await;
        if *need == Need::Admin {
            assert_eq!(status, 403, "controller {method} {path}: {body}");
            assert_eq!(body["error"]["code"], "admin_only", "{method} {path}");
        } else {
            assert!(
                status != 403 && status != 401,
                "controller {method} {path}: {status} {body}"
            );
        }
    }
    // Loopback-only routes do not exist on the portal.
    for path in ["/api/daemon/shutdown", "/api/ws-ticket"] {
        let (status, _) = portal_call(&app, "POST", path, &controller).await;
        assert_eq!(status, 404, "{path}");
    }
    // The runtime token is no credential on the portal.
    let (status, _) = portal_call(&app, "GET", "/api/health", "").await;
    assert_eq!(status, 401);
    let r = {
        use tower::ServiceExt as _;
        app.clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/health")
                    .header("authorization", format!("Bearer {}", h.token))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    };
    assert_eq!(r.status(), 401);

    // Health tells each client what it may do.
    for (cookie, control) in [(&viewer, false), (&controller, true)] {
        let (status, body) = portal_call(&app, "GET", "/api/health", cookie).await;
        assert_eq!(status, 200);
        assert_eq!(
            body["capabilities"],
            json!({"admin": false, "control_terminals": control, "local": false})
        );
    }
    let local: Health = h.get("/api/health").await;
    assert_eq!(
        local.capabilities,
        blirp_core::model::Capabilities {
            admin: true,
            control_terminals: true,
            local: true
        }
    );
    // The local client passes the admin checks.
    let r = h
        .send(reqwest::Method::PATCH, "/api/settings", json!({}))
        .await;
    assert_eq!(r.status(), 200);
    h.daemon.shutdown().await.unwrap();
}

async fn ws_with(url: String, header: (&'static str, String)) -> Ws {
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert(header.0, header.1.parse().unwrap());
    tokio_tungstenite::connect_async(req).await.unwrap().0
}

// Terminal socket protocol details: read-only clients are told so, a
// resize is not echoed to its sender, and both close directions complete
// the close handshake with the documented codes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_socket_protocol() {
    let h = Harness::start().await;
    let proj = h._home.path().join("proto");
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

    // A portal device without terminal control gets `readonly` after the snapshot.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let portal = listener.local_addr().unwrap();
    let app = blirp::api::portal_router(h.daemon.state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    let viewer = portal_device(&h, false);
    let mut v = ws_with(
        format!("ws://{portal}/api/terminals/{}/ws", session.id),
        ("Cookie", viewer),
    )
    .await;
    assert!(matches!(
        next_text(&mut v).await,
        TerminalServerMessage::Snapshot { .. }
    ));
    assert!(matches!(
        next_text(&mut v).await,
        TerminalServerMessage::Readonly
    ));
    drop(v);

    let mut a = h.ws(&session.id).await;
    let mut b = h.ws(&session.id).await;
    next_text(&mut a).await;
    next_text(&mut b).await;
    let resize = json!({"type": "resize", "cols": 90, "rows": 20}).to_string();
    a.send(Message::Text(resize.into())).await.unwrap();
    let TerminalServerMessage::Resize { cols, rows } = next_text(&mut b).await else {
        panic!("the other client must hear about the resize");
    };
    assert_eq!((cols, rows), (90, 20));
    let input = json!({"type": "input", "data": "echo AFT$()ER-RESIZE\r"}).to_string();
    a.send(Message::Text(input.into())).await.unwrap();
    let mut screen = vt100::Parser::new(20, 90, 100);
    let control = read_until(&mut a, &mut screen, "AFTER-RESIZE").await;
    assert!(
        !control
            .iter()
            .any(|m| matches!(m, TerminalServerMessage::Resize { .. })),
        "resize echoed to its sender: {control:?}"
    );

    // Client-initiated close: the server answers it.
    a.close(None).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), a.next())
        .await
        .unwrap();
    assert!(matches!(reply, Some(Ok(Message::Close(_)))), "{reply:?}");

    // Server-initiated close after the exit frame: 1000 "exited".
    let stop = format!("/api/sessions/{}/stop", session.id);
    assert_eq!(
        h.send(reqwest::Method::POST, &stop, json!({}))
            .await
            .status(),
        202
    );
    let close = loop {
        match tokio::time::timeout(Duration::from_secs(20), b.next()).await {
            Ok(Some(Ok(Message::Close(f)))) => break f,
            Ok(Some(Ok(_))) => continue,
            other => panic!("expected a close frame, got {other:?}"),
        }
    };
    let close = close.expect("close frame without code");
    assert_eq!(u16::from(close.code), 1000);
    assert_eq!(close.reason.as_str(), "exited");
    h.daemon.shutdown().await.unwrap();
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let st = blirp_core::process::command("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(st.status.success(), "git {args:?}: {st:?}");
}

// A worktree added for a launch whose session row could not be written is
// removed again: no session would ever own it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_launch_removes_its_worktree() {
    let h = Harness::start().await;
    let repo = h._home.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "init"]);
    // Every session insert fails after the worktree was added.
    let db = rusqlite::Connection::open(h._home.path().join("blirp.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER no_sessions BEFORE INSERT ON sessions BEGIN SELECT RAISE(ABORT, 'no'); END;",
    )
    .unwrap();
    let r = h
        .send(
            reqwest::Method::POST,
            "/api/sessions",
            json!({"cwd": repo, "agent": "shell", "worktree": true}),
        )
        .await;
    assert_eq!(r.status(), 500);
    let out = blirp_core::process::command("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let list = String::from_utf8_lossy(&out.stdout);
    assert_eq!(list.matches("worktree ").count(), 1, "{list}");
    let dirs: Vec<_> = walk(&h._home.path().join("worktrees"));
    assert!(dirs.is_empty(), "{dirs:?}");
    h.daemon.shutdown().await.unwrap();
}

/// Files under `dir` (recursive).
fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

// A session's worktree is removed on request once the session ended, never
// with uncommitted work unless forced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worktrees_are_removed_only_when_clean_or_forced() {
    let h = Harness::start().await;
    let repo = h._home.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "init"]);
    let r = h
        .send(
            reqwest::Method::POST,
            "/api/sessions",
            json!({"cwd": repo, "agent": "shell", "worktree": true}),
        )
        .await;
    assert_eq!(r.status(), 201, "{:?}", r.text().await);
    let s: Session = r.json().await.unwrap();
    let wt = std::path::PathBuf::from(s.worktree.clone().expect("worktree"));
    assert!(wt.join("a.txt").is_file());
    let remove = format!("/api/sessions/{}/worktree/remove", s.id);
    let code = |r: reqwest::Response| async move {
        let status = r.status().as_u16();
        (
            status,
            r.json::<ErrorBody>().await.map(|e| e.error.code).ok(),
        )
    };

    let r = h.send(reqwest::Method::POST, &remove, json!({})).await;
    assert_eq!(code(r).await, (409, Some("session_live".into())));
    let stop = format!("/api/sessions/{}/stop", s.id);
    assert_eq!(
        h.send(reqwest::Method::POST, &stop, json!({}))
            .await
            .status(),
        202
    );
    wait_status(&h, &s.id, SessionStatus::Completed).await;
    wait_no_terminal(&h, &s.id).await;

    std::fs::write(wt.join("scratch.txt"), "work\n").unwrap();
    let r = h.send(reqwest::Method::POST, &remove, json!({})).await;
    assert_eq!(code(r).await, (409, Some("worktree_dirty".into())));
    assert!(wt.is_dir());

    let r = h
        .send(reqwest::Method::POST, &remove, json!({"force": true}))
        .await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    let s: Session = h.get(&format!("/api/sessions/{}", s.id)).await;
    assert_eq!(s.worktree, None);
    assert!(!wt.exists());
    let r = h.send(reqwest::Method::POST, &remove, json!({})).await;
    assert_eq!(code(r).await, (409, Some("no_worktree".into())));
    h.daemon.shutdown().await.unwrap();
}

/// Loopback listener auth (§11): bearer token or a WebSocket ticket, never
/// a cookie (cookies ignore ports, so every local server would get it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_auth_takes_no_cookies() {
    let h = Harness::start().await;
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // Old login links move the token into the fragment and set no cookie.
    let r = no_redirect
        .get(h.url(&format!("/auth?token={}", h.token)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], format!("/#token={}", h.token));
    assert!(r.headers().get("set-cookie").is_none());
    let r = no_redirect
        .get(h.url("/auth?token=wrong"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // The cookie of older versions is refused, and cleared wherever it shows up.
    let legacy = format!("blirp_session={}", h.token);
    for path in ["/api/health", "/", "/auth?token=wrong"] {
        let r = no_redirect
            .get(h.url(path))
            .header("Cookie", &legacy)
            .send()
            .await
            .unwrap();
        if path.starts_with("/api/") {
            assert_eq!(r.status(), 401, "{path}");
        }
        let cleared = r
            .headers()
            .get_all("set-cookie")
            .iter()
            .any(|v| v.to_str().unwrap().starts_with("blirp_session=; Max-Age=0"));
        assert!(cleared, "{path}: legacy cookie not cleared");
    }
    let r = no_redirect.get(h.url("/")).send().await.unwrap();
    assert!(r.headers().get("set-cookie").is_none());
    // The portal's device cookie means nothing here.
    let r = no_redirect
        .get(h.url("/api/health"))
        .header("Cookie", format!("blirp_device={}", h.token))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // WebSocket tickets: minted with the bearer token, one path, once.
    let hr = &h;
    let ticket = |path: &'static str| async move {
        let r = hr
            .send(
                reqwest::Method::POST,
                "/api/ws-ticket",
                json!({ "path": path }),
            )
            .await;
        assert_eq!(r.status(), 200, "{path}");
        r.json::<serde_json::Value>().await.unwrap()["ticket"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let ws_url = |path: &str, t: &str| format!("ws://127.0.0.1:{}{path}?ticket={t}", h.daemon.port);
    let refused = |url: String| async move {
        match tokio_tungstenite::connect_async(url).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(r)) => r.status().as_u16(),
            Err(e) => panic!("unexpected error {e}"),
            Ok(_) => panic!("upgrade must be refused"),
        }
    };
    let events = "/api/events/ws";
    let t = ticket(events).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(events, &t))
        .await
        .unwrap();
    ws.close(None).await.unwrap();
    assert_eq!(refused(ws_url(events, &t)).await, 401, "reused ticket");
    let t = ticket("/api/terminals/s1/ws").await;
    assert_eq!(refused(ws_url(events, &t)).await, 401, "other path");
    assert_eq!(refused(ws_url(events, "0".repeat(32).as_str())).await, 401);
    assert_eq!(
        refused(format!("ws://127.0.0.1:{}{events}", h.daemon.port)).await,
        401
    );
    // A ticket is no credential for plain requests.
    let t = ticket(events).await;
    let r = no_redirect
        .get(h.url(&format!("/api/health?ticket={t}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    // Minting needs the token, and only WebSocket paths are accepted.
    let r = no_redirect
        .post(h.url("/api/ws-ticket"))
        .json(&json!({ "path": events }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = h
        .send(
            reqwest::Method::POST,
            "/api/ws-ticket",
            json!({ "path": "/api/health" }),
        )
        .await;
    assert_eq!(r.status(), 400);
    // The DNS-rebinding check still comes first.
    let r = h
        .http
        .post(h.url("/api/ws-ticket"))
        .bearer_auth(&h.token)
        .header("Host", format!("rebind.example:{}", h.daemon.port))
        .json(&json!({ "path": events }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    h.daemon.shutdown().await.unwrap();
}

// A taken port fails the start instead of moving to another port that
// clients do not know (and leaving the known one to whoever holds it).
#[tokio::test]
async fn taken_port_fails_startup() {
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = squatter.local_addr().unwrap().port();
    let home = tempfile::tempdir().unwrap();
    let err = Daemon::start(DaemonOptions {
        paths: Paths::at(home.path()),
        port: Some(port),
        ingest: None,
    })
    .await
    .err()
    .expect("start must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains(&format!("port {port} on 127.0.0.1 is already in use")),
        "{msg}"
    );
    assert!(msg.contains("config.toml"), "{msg}");
    assert!(!home.path().join("runtime.json").exists());
    drop(squatter);
}
