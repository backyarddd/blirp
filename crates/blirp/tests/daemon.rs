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
    daemon: Daemon,
    http: reqwest::Client,
    base: String,
    token: String,
    // Last: fields drop in order, and Windows cannot remove the folder
    // while the handles above are open.
    _home: tempfile::TempDir,
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

async fn upload(h: &Harness, id: &str, name: &str, body: Vec<u8>) -> reqwest::Response {
    let mut url = reqwest::Url::parse(&h.url(&format!("/api/sessions/{id}/uploads"))).unwrap();
    url.query_pairs_mut().append_pair("name", name);
    h.http
        .post(url)
        .bearer_auth(&h.token)
        .header("content-type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .unwrap()
}

// Files pasted or dropped into a terminal land in the session's upload
// folder on this machine, and go away with the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_uploads() {
    let h = Harness::start().await;
    let proj = h._home.path().join("paste");
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
    let dir = std::path::absolute(
        h.daemon
            .state
            .paths
            .session_uploads_dir(&session.id)
            .unwrap(),
    )
    .unwrap();

    let png = b"\x89PNG\r\n\x1a\nfake".to_vec();
    let r = upload(&h, &session.id, "../../Screen Shot.png", png.clone()).await;
    assert_eq!(r.status(), 201, "{:?}", r.text().await);
    let up: blirp_core::model::UploadedFile = r.json().await.unwrap();
    let saved = std::path::PathBuf::from(&up.path);
    assert_eq!(saved.parent().unwrap(), dir);
    assert!(up.path.ends_with("-Screen_Shot.png"), "{}", up.path);
    assert_eq!(std::fs::read(&saved).unwrap(), png);
    assert_eq!(up.size, png.len() as u64);
    assert!(up.quoted.contains("Screen_Shot.png"));

    // One byte over the limit is refused before anything is written, by its
    // declared length or while reading. In-process: over TCP the early
    // answer races the client still sending the body.
    let app = blirp::api::proxy_router(h.daemon.state.clone())
        .layer(axum::Extension(blirp::api::Principal::local()));
    for declared in [true, false] {
        use tower::ServiceExt as _;
        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/sessions/{}/uploads?name=big.bin", session.id));
        if declared {
            req = req.header("content-length", blirp::uploads::MAX_BYTES + 1);
        }
        let body = axum::body::Body::from(vec![0; blirp::uploads::MAX_BYTES + 1]);
        let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        assert_eq!(resp.status(), 413, "declared {declared}");
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let err: ErrorBody = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(err.error.code, "file_too_large");
    }
    // Over TCP, a client that sends its whole body before reading still gets
    // the refusal (the rest of the body is drained, not reset): too large,
    // or for a session that is not running.
    let over = blirp::uploads::MAX_BYTES + (1 << 20);
    for (target, size, chunked, status, code) in [
        (session.id.as_str(), over, false, 413, "file_too_large"),
        (session.id.as_str(), over, true, 413, "file_too_large"),
        (
            "no-such-session",
            20 << 20,
            false,
            404,
            "terminal_not_found",
        ),
        ("no-such-session", 20 << 20, true, 404, "terminal_not_found"),
    ] {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", h.daemon.port))
            .await
            .unwrap();
        let length = if chunked {
            "Transfer-Encoding: chunked".to_string()
        } else {
            format!("Content-Length: {size}")
        };
        let head = format!(
            "POST /api/sessions/{target}/uploads?name=big.bin HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
             Authorization: Bearer {}\r\n{length}\r\n\r\n",
            h.daemon.port, h.token
        );
        tcp.write_all(head.as_bytes()).await.unwrap();
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..size / chunk.len() {
            if chunked {
                tcp.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await
                    .unwrap();
            }
            tcp.write_all(&chunk).await.unwrap();
            if chunked {
                tcp.write_all(b"\r\n").await.unwrap();
            }
        }
        if chunked {
            tcp.write_all(b"0\r\n\r\n").await.unwrap();
        }
        let mut answer = String::new();
        let mut buf = vec![0u8; 4096];
        while !answer.contains(code) {
            let n = tokio::time::timeout(Duration::from_secs(20), tcp.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            if n == 0 {
                break;
            }
            answer.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        let what = format!("{target} chunked {chunked}: {answer}");
        assert!(answer.starts_with(&format!("HTTP/1.1 {status}")), "{what}");
        assert!(answer.contains(code), "{what}");
    }
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

    // Only running sessions take uploads.
    let r = upload(&h, "no-such-session", "a.png", png.clone()).await;
    assert_eq!(r.status(), 404);
    let err: ErrorBody = r.json().await.unwrap();
    assert_eq!(err.error.code, "terminal_not_found");

    let stop = format!("/api/sessions/{}/stop", session.id);
    assert_eq!(
        h.send(reqwest::Method::POST, &stop, json!({}))
            .await
            .status(),
        202
    );
    wait_status(&h, &session.id, SessionStatus::Completed).await;
    wait_no_terminal(&h, &session.id).await;
    // Attaching after the process ended gets its exit and a normal close,
    // not a 404 the client would keep retrying.
    let mut ws = h.ws(&session.id).await;
    assert!(matches!(
        next_text(&mut ws).await,
        TerminalServerMessage::Exit {
            status: SessionStatus::Completed,
            exit_code: None
        }
    ));
    match tokio::time::timeout(Duration::from_secs(20), ws.next()).await {
        Ok(Some(Ok(Message::Close(Some(f))))) => assert_eq!(u16::from(f.code), 1000),
        other => panic!("expected a close frame, got {other:?}"),
    }
    let mut req = format!(
        "ws://127.0.0.1:{}/api/terminals/no-such-session/ws",
        h.daemon.port
    )
    .into_client_request()
    .unwrap();
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", h.token).parse().unwrap(),
    );
    match tokio_tungstenite::connect_async(req).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => assert_eq!(r.status(), 404),
        other => panic!("unknown session must be refused: {:?}", other.map(|_| ())),
    }
    let r = upload(&h, &session.id, "late.png", png).await;
    assert_eq!(r.status(), 404);

    let r = h
        .send(
            reqwest::Method::DELETE,
            &format!("/api/sessions/{}", session.id),
            json!({}),
        )
        .await;
    assert_eq!(r.status(), 204);
    assert!(!dir.exists(), "uploads removed with the session");
    let Harness { daemon, _home, .. } = h;
    daemon.shutdown().await.unwrap();
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
        title_updated_at: 0,
        project_updated_at: 0,
        compacted_at: None,
        context_near_full_at: None,
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
        (reqwest::Method::POST, "/api/sessions/theirs/uploads"),
    ] {
        let r = h.send(method, path, json!({})).await;
        assert_eq!(r.status(), 409, "{path}");
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(body["error"]["code"], "machine_unreachable", "{path}");
    }

    // The list holds every machine's sessions, live ones first, then by
    // last activity; `machine=` narrows it and cursors walk every page.
    store
        .modify_session("fork", |s| s.last_activity_at = 10)
        .unwrap();
    assert_eq!(
        ids(h.get("/api/sessions").await),
        ["live", "fork", "theirs"]
    );
    let mine = &h.daemon.state.machine.id;
    assert_eq!(
        ids(h.get(&format!("/api/sessions?machine={mine}")).await),
        ["live", "fork"]
    );
    let p1: SessionsPage = h.get("/api/sessions?limit=2").await;
    let cursor = p1.next_cursor.clone().unwrap();
    assert_eq!(ids(p1), ["live", "fork"]);
    let p2: SessionsPage = h
        .get(&format!("/api/sessions?limit=2&cursor={cursor}"))
        .await;
    assert!(p2.next_cursor.is_none());
    assert_eq!(ids(p2), ["theirs"]);
    let r = h
        .http
        .get(h.url("/api/sessions?cursor=5:theirs"))
        .bearer_auth(&h.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    h.daemon.shutdown().await.unwrap();
}

/// claude's headless login token: stored and cleared by a local admin,
/// reported as a flag, never sent back.
#[tokio::test]
async fn claude_login_token_is_stored_but_never_returned() {
    use blirp_core::model::AgentInfo;
    const SECRET: &str = "sk-ant-oat01-test-only-not-a-real-token";
    let h = Harness::start().await;
    let paths = h.daemon.state.paths.clone();
    let claude = |list: Vec<AgentInfo>| list.into_iter().find(|a| a.id == "claude").unwrap();

    let before = claude(h.get("/api/agents").await);
    assert_eq!(before.token.as_ref().map(|t| t.stored), Some(false));
    // Only claude has one; other agents report nothing.
    let list: Vec<AgentInfo> = h.get("/api/agents").await;
    assert!(
        list.iter()
            .filter(|a| a.id != "claude")
            .all(|a| a.token.is_none())
    );

    for (body, code) in [
        (json!({"token": "  "}), "invalid_request"),
        (json!({"token": "two words"}), "invalid_request"),
        (json!({"token": SECRET, "extra": 1}), "invalid_request"),
    ] {
        let r = h
            .send(reqwest::Method::PUT, "/api/agents/claude/token", body)
            .await;
        assert_eq!(r.status(), 400);
        let e: ErrorBody = r.json().await.unwrap();
        assert_eq!(e.error.code, code);
    }
    let r = h
        .send(
            reqwest::Method::PUT,
            "/api/agents/codex/token",
            json!({"token": SECRET}),
        )
        .await;
    assert_eq!(r.status(), 422);
    assert!(!paths.claude_token_file().exists());

    let r = h
        .send(
            reqwest::Method::PUT,
            "/api/agents/claude/token",
            json!({"token": format!("{SECRET}
")}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(!text.contains(SECRET), "{text}");
    let info: AgentInfo = serde_json::from_str(&text).unwrap();
    assert_eq!(info.token.map(|t| t.stored), Some(true));
    assert_eq!(
        blirp_core::claude_token::read(&paths).unwrap().as_deref(),
        Some(SECRET)
    );
    for path in ["/api/agents", "/api/settings", "/api/health"] {
        let text = h
            .http
            .get(h.url(path))
            .bearer_auth(&h.token)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!text.contains(SECRET), "{path}: {text}");
    }

    let r = h
        .send(
            reqwest::Method::DELETE,
            "/api/agents/claude/token",
            json!({}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert!(!paths.claude_token_file().exists());
    // `blirp agents set-token` writes the file directly; the next listing
    // sees it without waiting for the cached one to expire.
    blirp_core::claude_token::store(&paths, SECRET).unwrap();
    let after = claude(h.get("/api/agents").await);
    assert_eq!(after.token.map(|t| t.stored), Some(true));
    h.daemon.shutdown().await.unwrap();
}

/// Every mutating route with the right it needs (§11). A route missing
/// here is a route nobody checked.
const MUTATING_ROUTES: &[(&str, &str, Need)] = &[
    ("PATCH", "/api/settings", Need::Admin),
    ("POST", "/api/update/check", Need::Admin),
    ("POST", "/api/sync/hub/enable", Need::Admin),
    ("POST", "/api/sync/hub/disable", Need::Admin),
    ("POST", "/api/sync/invite", Need::Admin),
    ("POST", "/api/sync/join", Need::Admin),
    ("POST", "/api/sync/join/preview", Need::Admin),
    ("POST", "/api/sync/leave", Need::Admin),
    ("POST", "/api/devices/browser-invite", Need::Admin),
    ("PATCH", "/api/devices/d1", Need::Admin),
    ("DELETE", "/api/devices/d1", Need::Admin),
    ("DELETE", "/api/machines/m1", Need::Admin),
    ("POST", "/api/machines/m1/clone", Need::Control),
    ("POST", "/api/agents/claude/hooks/install", Need::Admin),
    ("POST", "/api/agents/claude/hooks/uninstall", Need::Admin),
    ("PUT", "/api/agents/claude/token", Need::Admin),
    ("DELETE", "/api/agents/claude/token", Need::Admin),
    ("POST", "/api/hooks/claude/Stop", Need::Admin),
    ("POST", "/api/sessions/s1/open", Need::Admin),
    ("POST", "/api/sessions", Need::Control),
    ("DELETE", "/api/sessions/s1", Need::Control),
    ("POST", "/api/sessions/s1/worktree/remove", Need::Control),
    ("PATCH", "/api/sessions/s1", Need::Control),
    ("POST", "/api/sessions/s1/stop", Need::Control),
    ("POST", "/api/sessions/s1/resume", Need::Control),
    ("POST", "/api/sessions/s1/distill", Need::Control),
    ("POST", "/api/sessions/s1/uploads", Need::Control),
    ("POST", "/api/projects", Need::Control),
    ("PATCH", "/api/projects/p1", Need::Control),
    ("DELETE", "/api/projects/p1", Need::Admin),
    ("POST", "/api/projects/p1/merge", Need::Admin),
    ("POST", "/api/projects/p1/restore", Need::Admin),
    ("POST", "/api/projects/p1/open", Need::Admin),
    ("POST", "/api/projects/p1/folders", Need::Control),
    ("PUT", "/api/projects/p1/brief", Need::Control),
    ("POST", "/api/projects/p1/brief/revert", Need::Control),
    ("POST", "/api/projects/p1/records", Need::Control),
    ("PATCH", "/api/projects/p1/records/r1", Need::Control),
    ("DELETE", "/api/projects/p1/records/r1", Need::Control),
    ("POST", "/api/projects/p1/wiki", Need::Control),
    ("PUT", "/api/projects/p1/wiki/w1", Need::Control),
    ("DELETE", "/api/projects/p1/wiki/w1", Need::Control),
    ("POST", "/api/projects/p1/wiki/w1/restore", Need::Control),
    ("POST", "/api/projects/p1/wiki/w1/rename", Need::Control),
    ("POST", "/api/projects/p1/resources", Need::Control),
    ("PATCH", "/api/projects/p1/resources/r1", Need::Control),
    ("DELETE", "/api/projects/p1/resources/r1", Need::Control),
    ("POST", "/api/suggestions/x1/accept", Need::Control),
    ("POST", "/api/files/pause", Need::Control),
    ("POST", "/api/files/start-now", Need::Control),
    ("PUT", "/api/projects/p1/files-sync", Need::Control),
    ("POST", "/api/projects/p1/files-sync/apply", Need::Control),
    ("POST", "/api/projects/p1/files-sync/held", Need::Control),
    (
        "DELETE",
        "/api/projects/p1/files-sync/roots/r1",
        Need::Control,
    ),
    ("POST", "/api/machines/m1/files/download", Need::Control),
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Need {
    Admin,
    Control,
}

/// A LAN portal browser device (never admin) and its cookie. It may read
/// project files when it may control terminals (see the Files checks).
fn portal_device(h: &Harness, control: bool) -> String {
    portal_device_with(h, control, control)
}

fn portal_device_with(h: &Harness, control: bool, files: bool) -> String {
    use sha2::Digest as _;
    let token = format!("{:064x}", u128::from(control) + 2 * u128::from(files) + 7);
    let now = blirp_core::now_ms();
    h.daemon
        .state
        .store
        .upsert_device(&blirp_core::model::Device {
            id: format!("dev-{control}-{files}"),
            name: "phone".into(),
            kind: blirp_core::model::DeviceKind::Browser,
            token_hash: Some(hex::encode(sha2::Sha256::digest(token.as_bytes()))),
            node_id: None,
            created_at: now,
            last_seen: now,
            revoked: false,
            can_control_terminals: control,
            can_access_files: files,
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
    for path in [
        "/api/daemon/shutdown",
        "/api/ws-ticket",
        "/api/update/apply",
    ] {
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
            json!({"admin": false, "control_terminals": control, "local": false, "files": control})
        );
    }
    let local: Health = h.get("/api/health").await;
    assert_eq!(
        local.capabilities,
        blirp_core::model::Capabilities {
            admin: true,
            control_terminals: true,
            local: true,
            files: true,
        }
    );
    // Project files need the Files permission, also for reading and for a
    // device that may control terminals.
    let no_files = portal_device_with(&h, true, false);
    for (method, path) in [
        ("GET", "/api/projects/p1/files"),
        ("GET", "/api/projects/p1/files/content?path=a"),
        ("GET", "/api/projects/p1/git/diff"),
        ("GET", "/api/projects/p1/files-sync"),
        ("GET", "/api/projects/p1/files-sync/preview"),
        ("GET", "/api/files/status"),
        ("POST", "/api/projects/p1/files-sync/apply"),
        ("POST", "/api/machines/m1/files/download"),
    ] {
        for cookie in [&viewer, &no_files] {
            let (status, body) = portal_call(&app, method, path, cookie).await;
            if cookie == &viewer && method == "POST" {
                assert_eq!(
                    body["error"]["code"], "control_not_allowed",
                    "{method} {path}"
                );
            } else {
                assert_eq!(status, 403, "{method} {path}: {body}");
                assert_eq!(
                    body["error"]["code"], "files_not_allowed",
                    "{method} {path}"
                );
            }
        }
        let (status, body) = portal_call(&app, method, path, &controller).await;
        assert!(
            status != 403 && status != 401,
            "{method} {path}: {status} {body}"
        );
    }
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
    // Output already in flight (e.g. a shell prompt) may arrive before the reply.
    a.close(None).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match a.next().await {
                Some(Ok(Message::Binary(_) | Message::Text(_))) => continue,
                other => break other,
            }
        }
    })
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

    // The token is never taken from a query string or a cookie.
    let r = no_redirect
        .get(h.url(&format!("/auth?token={}", h.token)))
        .send()
        .await
        .unwrap();
    assert!(!r.status().is_redirection(), "no login redirect route");
    assert!(r.headers().get("set-cookie").is_none());
    let r = no_redirect
        .get(h.url(&format!("/api/health?token={}", h.token)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = no_redirect
        .get(h.url("/api/health"))
        .header("Cookie", format!("blirp_session={}", h.token))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
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

/// A local stand-in for the GitHub releases API: `GET /releases/latest`
/// answers `tag` and counts the requests.
async fn fake_releases(
    tag: &'static str,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let app = axum::Router::new().route(
        "/releases/latest",
        axum::routing::get(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async move {
                axum::Json(json!({
                    "tag_name": tag,
                    "html_url": format!("https://example.invalid/releases/{tag}"),
                    "assets": [],
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/releases", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, hits)
}

type Spawned =
    std::sync::Arc<std::sync::Mutex<Vec<(std::path::PathBuf, Vec<String>, std::path::PathBuf)>>>;

/// Overrides that make this daemon a script install whose updater is only
/// recorded, never run.
fn fake_install(h: &Harness, base: &str) -> (std::path::PathBuf, Spawned) {
    let cli = h.daemon.state.paths.home().join("bin").join("blirp");
    let spawned: Spawned = Default::default();
    let record = spawned.clone();
    h.daemon
        .state
        .updates
        .set_overrides(blirp::update::Overrides {
            base_url: Some(base.to_string()),
            cli: Some(cli.clone()),
            spawn: Some(std::sync::Arc::new(move |exe, args, dir| {
                record
                    .lock()
                    .unwrap()
                    .push((exe.to_path_buf(), args.to_vec(), dir.to_path_buf()));
                Ok(())
            })),
        });
    (cli, spawned)
}

async fn post_status(h: &Harness, path: &str) -> (u16, serde_json::Value) {
    let r = h.send(reqwest::Method::POST, path, json!({})).await;
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(serde_json::Value::Null))
}

#[tokio::test]
async fn update_status_check_and_apply() {
    use blirp_core::model::{UpdateOutcome, UpdateStatus};
    use std::sync::atomic::Ordering;
    let h = Harness::start().await;
    let (base, hits) = fake_releases("v99.0.0").await;
    h.daemon
        .state
        .updates
        .set_overrides(blirp::update::Overrides {
            base_url: Some(base.clone()),
            ..Default::default()
        });

    // Not a script install (a test binary has no receipt): nothing to apply.
    let st: UpdateStatus = h.get("/api/update").await;
    assert!(st.available && st.enabled && !st.self_update, "{st:?}");
    assert_eq!(st.latest.as_deref(), Some("99.0.0"));
    assert_eq!(
        st.notes_url.as_deref(),
        Some("https://example.invalid/releases/v99.0.0")
    );
    assert!(st.checked_at.is_some() && st.error.is_none() && st.last_update.is_none());
    let (code, body) = post_status(&h, "/api/update/apply").await;
    assert_eq!(
        (code, body["error"]["code"].as_str()),
        (409, Some("not_self_update"))
    );

    // Check now within a minute of the last ask is answered from it.
    let (code, body) = post_status(&h, "/api/update/check").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["latest"], "99.0.0");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // A script install: the updater is the installed CLI, pinned to the
    // release the UI showed, with this daemon's data dir.
    let (cli, spawned) = fake_install(&h, &base);
    let (code, body) = post_status(&h, "/api/update/apply").await;
    assert_eq!(code, 202, "{body}");
    {
        let spawned = spawned.lock().unwrap();
        assert_eq!(spawned.len(), 1);
        let (exe, args, dir) = &spawned[0];
        assert_eq!(*exe, cli);
        assert_eq!(args, &["update", "--version", "99.0.0"]);
        assert_eq!(dir, h.daemon.state.paths.home());
    }
    // One updater at a time, until it records an outcome.
    let (code, body) = post_status(&h, "/api/update/apply").await;
    assert_eq!(
        (code, body["error"]["code"].as_str()),
        (409, Some("update_in_progress"))
    );
    let failed = UpdateOutcome {
        from: blirp::update::CURRENT.into(),
        to: Some("99.0.0".into()),
        installed: false,
        ok: false,
        error: Some("updating to 99.0.0 failed: disk full".into()),
        finished_at: blirp_core::now_ms() + 1,
    };
    blirp::update::record_outcome(&h.daemon.state.paths, &failed).unwrap();
    let st: UpdateStatus = h.get("/api/update").await;
    assert_eq!(st.last_update.as_ref(), Some(&failed));
    assert!(st.self_update);
    let (code, body) = post_status(&h, "/api/update/apply").await;
    assert_eq!(code, 202, "{body}");
    assert_eq!(spawned.lock().unwrap().len(), 2);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "apply reuses the last check"
    );

    // Checks off: no check, no update.
    let mut cfg = h.daemon.state.config();
    cfg.update.check = false;
    h.daemon.state.set_config(cfg);
    for path in ["/api/update/check", "/api/update/apply"] {
        let (code, body) = post_status(&h, path).await;
        assert_eq!(
            (code, body["error"]["code"].as_str()),
            (409, Some("update_checks_off")),
            "{path}"
        );
    }
    let st: UpdateStatus = h.get("/api/update").await;
    assert!(!st.enabled && st.latest.is_none() && !st.available);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    h.daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn update_apply_needs_a_newer_release() {
    use blirp_core::model::UpdateStatus;
    let h = Harness::start().await;
    let (base, _) = fake_releases(concat!("v", env!("CARGO_PKG_VERSION"))).await;
    let (_, spawned) = fake_install(&h, &base);
    let st: UpdateStatus = h.get("/api/update").await;
    assert!(!st.available && st.self_update, "{st:?}");
    let (code, body) = post_status(&h, "/api/update/apply").await;
    assert_eq!(
        (code, body["error"]["code"].as_str()),
        (409, Some("no_update"))
    );
    h.daemon.shutdown().await.unwrap();

    // Offline: the error is reported and nothing can be applied.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/releases", closed.local_addr().unwrap());
    drop(closed);
    let h = Harness::start().await;
    let (_, offline) = fake_install(&h, &base);
    let st: UpdateStatus = h.get("/api/update").await;
    assert!(st.latest.is_none() && !st.available, "{st:?}");
    assert!(
        st.error
            .as_deref()
            .is_some_and(|e| e.contains("/releases/latest")),
        "{st:?}"
    );
    let (code, body) = post_status(&h, "/api/update/apply").await;
    assert_eq!(
        (code, body["error"]["code"].as_str()),
        (409, Some("no_update"))
    );
    assert!(spawned.lock().unwrap().is_empty() && offline.lock().unwrap().is_empty());
    h.daemon.shutdown().await.unwrap();
}

// A project without folders: created with a name and brief, its sessions
// start in this machine's blirp workspace, a folder picked for a session
// joins it, removing that folder keeps the project, and a session moves to
// Chats and back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folderless_projects_start_in_their_workspace() {
    let h = Harness::start().await;
    let post = |path: String, body: serde_json::Value| {
        let h = &h;
        async move { h.send(reqwest::Method::POST, &path, body).await }
    };
    assert_eq!(post("/api/projects".into(), json!({})).await.status(), 400);
    let r = post(
        "/api/projects".into(),
        json!({"name": "Design", "brief": "Screens for the app."}),
    )
    .await;
    assert_eq!(r.status(), 201);
    let p: ProjectSummary = r.json().await.unwrap();
    assert!(p.paths.is_empty() && !p.project.chats);
    let ws = Paths::at(h._home.path())
        .workspace_dir(&p.project.id)
        .unwrap();
    assert_eq!(p.workspace.as_deref(), Some(ws.to_str().unwrap()));
    let memory: ProjectMemory = h
        .get(&format!("/api/projects/{}/memory", p.project.id))
        .await;
    assert_eq!(memory.brief.unwrap().body_md, "Screens for the app.");

    let r = post(
        "/api/sessions".into(),
        json!({"project_id": p.project.id, "agent": "shell"}),
    )
    .await;
    assert_eq!(r.status(), 201);
    let s: Session = r.json().await.unwrap();
    assert_eq!(s.project_id, p.project.id);
    assert!(ws.is_dir());
    assert_eq!(
        dunce::canonicalize(&s.cwd).unwrap(),
        dunce::canonicalize(&ws).unwrap()
    );

    // Another folder only with add_folder; it then belongs to the project.
    let place = h._home.path().join("place");
    std::fs::create_dir(&place).unwrap();
    let launch = json!({"project_id": p.project.id, "cwd": place, "agent": "shell"});
    assert_eq!(
        post("/api/sessions".into(), launch.clone()).await.status(),
        400
    );
    let mut with_add = launch;
    with_add["add_folder"] = json!(true);
    let r = post("/api/sessions".into(), with_add).await;
    assert_eq!(r.status(), 201);
    let s2: Session = r.json().await.unwrap();
    let p2: ProjectSummary = h.get(&format!("/api/projects/{}", p.project.id)).await;
    assert_eq!(p2.paths.len(), 1);
    assert!(p2.workspace.is_none());

    // Removing its only folder keeps the project; sessions use the workspace again.
    let r = post(
        format!("/api/projects/{}/folders/remove", p.project.id),
        json!({"path": p2.paths[0].path}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let p3: ProjectSummary = r.json().await.unwrap();
    assert!(p3.paths.is_empty() && p3.workspace.is_some());
    assert_eq!(
        post(
            format!("/api/projects/{}/folders/remove", p.project.id),
            json!({"path": p2.paths[0].path}),
        )
        .await
        .status(),
        404
    );

    // Nothing here looks like a chat; the offer can be dismissed.
    let offered: Vec<ProjectSummary> = h.get("/api/projects/chat-candidates").await;
    assert!(offered.is_empty());
    assert_eq!(
        post("/api/projects/chat-candidates/dismiss".into(), json!({}))
            .await
            .status(),
        204
    );

    // To Chats and back.
    let r = post(
        format!("/api/sessions/{}/move", s2.id),
        json!({"project_id": null}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let moved: Session = r.json().await.unwrap();
    let projects: Vec<ProjectSummary> = h.get("/api/projects").await;
    let chats = projects
        .iter()
        .find(|x| x.project.id == moved.project_id)
        .unwrap();
    assert!(chats.project.chats && chats.is_home && chats.workspace.is_none());
    let r = post(
        format!("/api/sessions/{}/move", s2.id),
        json!({"project_id": p.project.id}),
    )
    .await;
    assert_eq!(r.json::<Session>().await.unwrap().project_id, p.project.id);

    for id in [&s.id, &s2.id] {
        post(format!("/api/sessions/{id}/stop"), json!({})).await;
        wait_status(&h, id, SessionStatus::Completed).await;
    }
    let Harness { daemon, _home, .. } = h;
    daemon.shutdown().await.unwrap();
}

// Delete moves a project to the Trash with its sessions hidden; restore
// brings both back. Folders are added only when they exist, projects are
// opened only in their own folders, and Chats keeps its name.
// Seqs start at 0 (an ingested transcript's first line): the first page of
// events, without `after`, includes it.
#[tokio::test]
async fn events_start_with_seq_zero() {
    use blirp_core::model::{Event, EventKind, EventsPage};
    let h = Harness::start().await;
    let p = bare_project(&h, "Events").await;
    let r = h
        .send(
            reqwest::Method::POST,
            "/api/sessions",
            json!({"project_id": p, "agent": "shell"}),
        )
        .await;
    assert_eq!(r.status(), 201);
    let s: Session = r.json().await.unwrap();
    for seq in [0, 1, 1024] {
        h.daemon
            .state
            .store
            .insert_event(Event {
                session_id: s.id.clone(),
                seq,
                ts: seq,
                kind: EventKind::User,
                text: format!("event {seq}"),
                meta: None,
            })
            .unwrap();
    }
    let page: EventsPage = h
        .get(&format!("/api/sessions/{}/events?limit=2", s.id))
        .await;
    let seqs: Vec<i64> = page.items.iter().map(|e| e.seq).collect();
    assert_eq!((seqs, page.next_after), (vec![0, 1], Some(1)));
    let page: EventsPage = h
        .get(&format!("/api/sessions/{}/events?after=1", s.id))
        .await;
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.next_after, None);
    h.send(
        reqwest::Method::POST,
        &format!("/api/sessions/{}/stop", s.id),
        json!({}),
    )
    .await;
}

/// A folderless project for a test.
async fn bare_project(h: &Harness, name: &str) -> String {
    let r = h
        .send(
            reqwest::Method::POST,
            "/api/projects",
            json!({ "name": name }),
        )
        .await;
    assert_eq!(r.status(), 201);
    r.json::<ProjectSummary>().await.unwrap().project.id
}

// A record is archived with a status patch and moved to another project by
// its project_id (a changed column of the replicated row); never into Chats.
#[tokio::test]
async fn records_archive_and_move_to_another_project() {
    let h = Harness::start().await;
    let a = bare_project(&h, "A").await;
    let b = bare_project(&h, "B").await;
    let r = h
        .send(
            reqwest::Method::POST,
            &format!("/api/projects/{a}/records"),
            json!({"kind": "note", "title": "Moves", "body": "x"}),
        )
        .await;
    let rec: Record = r.json().await.unwrap();
    let path = format!("/api/projects/{a}/records/{}", rec.id);
    let patch = |body: serde_json::Value| h.send(reqwest::Method::PATCH, &path, body);
    let r = patch(json!({"status": "archived"})).await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<Record>().await.unwrap().status,
        blirp_core::model::RecordStatus::Archived
    );
    let r = patch(json!({"project_id": "no-such-project"})).await;
    assert_eq!(r.status(), 404);
    // A session in the home folder is a chat: this machine's Chats bucket.
    let state = &h.daemon.state;
    let chats = state
        .store
        .resolve_project(
            &state.machine.id,
            &state.machine.name,
            &std::env::home_dir().unwrap(),
        )
        .unwrap()
        .project
        .id;
    let r = patch(json!({ "project_id": chats })).await;
    assert_eq!(r.status(), 400);

    let r = patch(json!({"project_id": b, "status": "active"})).await;
    assert_eq!(r.status(), 200);
    let moved: Record = r.json().await.unwrap();
    assert_eq!(moved.project_id, b);
    let in_a: Vec<Record> = h.get(&format!("/api/projects/{a}/records")).await;
    let in_b: Vec<Record> = h.get(&format!("/api/projects/{b}/records")).await;
    assert!(in_a.is_empty());
    assert_eq!(in_b.len(), 1);
    // It belongs to B now: A's path no longer finds it.
    assert_eq!(patch(json!({"pinned": true})).await.status(), 404);
}

// Deleted wiki pages are listed with ?deleted=true and come back with
// restore; rename gives a live page a new slug, refusing one another page
// (also a deleted one) holds.
#[tokio::test]
async fn wiki_pages_restore_and_rename() {
    use blirp_core::model::WikiPage;
    let h = Harness::start().await;
    let p = bare_project(&h, "Wiki").await;
    let wiki = format!("/api/projects/{p}/wiki");
    for (slug, title) in [("setup", "Setup"), ("old", "Old")] {
        let r = h
            .send(
                reqwest::Method::POST,
                &wiki,
                json!({"slug": slug, "title": title, "body_md": "text"}),
            )
            .await;
        assert_eq!(r.status(), 201);
    }
    let r = h
        .send(reqwest::Method::DELETE, &format!("{wiki}/old"), json!({}))
        .await;
    assert_eq!(r.status(), 204);
    let deleted: Vec<WikiPage> = h.get(&format!("{wiki}?deleted=true")).await;
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].slug, "old");
    let live: Vec<WikiPage> = h.get(&wiki).await;
    assert_eq!(live.len(), 1);

    let rename = |from: &str, to: &str| {
        let (path, body) = (format!("{wiki}/{from}/rename"), json!({ "slug": to }));
        let h = &h;
        async move { h.send(reqwest::Method::POST, &path, body).await }
    };
    // Held by a deleted page, invalid, or missing.
    assert_eq!(rename("setup", "old").await.status(), 409);
    assert_eq!(rename("setup", "Bad Slug").await.status(), 400);
    assert_eq!(rename("nope", "fine").await.status(), 404);
    let r = rename("setup", "getting-started").await;
    assert_eq!(r.status(), 200);
    let renamed: WikiPage = r.json().await.unwrap();
    assert_eq!(
        (renamed.slug.as_str(), renamed.title.as_str()),
        ("getting-started", "Setup")
    );
    let r = h
        .http
        .get(h.url(&format!("{wiki}/setup")))
        .bearer_auth(&h.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    let restore_path = format!("{wiki}/old/restore");
    let restore = || h.send(reqwest::Method::POST, &restore_path, json!({}));
    let r = restore().await;
    assert_eq!(r.status(), 200);
    assert!(!r.json::<WikiPage>().await.unwrap().deleted);
    let live: Vec<WikiPage> = h.get(&wiki).await;
    assert_eq!(live.len(), 2);
    // Only a deleted page is restored; a live one now holds the slug.
    assert_eq!(restore().await.status(), 404);
    assert_eq!(rename("getting-started", "old").await.status(), 409);
}

// A folder to register must be on a local disk (a network path would make
// Windows connect to its server with the user's credentials) and must not be
// the home folder or contain it.
#[tokio::test]
async fn project_folders_refuse_network_paths_and_the_home_folder() {
    let h = Harness::start().await;
    let work = tempfile::tempdir().unwrap();
    let r = h
        .send(
            reqwest::Method::POST,
            "/api/projects",
            json!({"path": work.path(), "name": "Ok"}),
        )
        .await;
    assert_eq!(r.status(), 201);
    let p: ProjectSummary = r.json().await.unwrap();
    let home = dunce::canonicalize(std::env::home_dir().unwrap()).unwrap();
    let mut refused = vec![home.clone(), home.parent().unwrap().to_path_buf()];
    if cfg!(windows) {
        refused.extend(
            [
                r"\\blirp-test.invalid\share",
                r"\\?\UNC\blirp-test.invalid\share",
            ]
            .map(std::path::PathBuf::from),
        );
    }
    for path in refused {
        let r = h
            .send(
                reqwest::Method::POST,
                "/api/projects",
                json!({"path": path}),
            )
            .await;
        assert_eq!(r.status(), 400, "create {}", path.display());
        let r = h
            .send(
                reqwest::Method::POST,
                &format!("/api/projects/{}/folders", p.project.id),
                json!({"path": path}),
            )
            .await;
        assert_eq!(r.status(), 400, "add folder {}", path.display());
    }
}

#[tokio::test]
async fn project_trash_restore_folders_and_open() {
    let h = Harness::start().await;
    let work = tempfile::tempdir().unwrap();
    let dir = work.path().join("proj");
    let extra = work.path().join("extra");
    std::fs::create_dir(&dir).unwrap();
    std::fs::create_dir(&extra).unwrap();
    let r = h
        .send(
            reqwest::Method::POST,
            "/api/projects",
            json!({"path": dir, "name": "Proj"}),
        )
        .await;
    let p: ProjectSummary = r.json().await.unwrap();
    let id = p.project.id.clone();
    let r = h
        .send(
            reqwest::Method::POST,
            "/api/sessions",
            json!({"project_id": id, "agent": "shell"}),
        )
        .await;
    assert_eq!(r.status(), 201);
    let s: Session = r.json().await.unwrap();
    h.send(
        reqwest::Method::POST,
        &format!("/api/sessions/{}/stop", s.id),
        json!({}),
    )
    .await;
    wait_status(&h, &s.id, SessionStatus::Completed).await;

    // Add folder: absolute, existing folders only.
    let add = |path: serde_json::Value| {
        let (h, id) = (&h, id.clone());
        async move {
            h.send(
                reqwest::Method::POST,
                &format!("/api/projects/{id}/folders"),
                json!({ "path": path }),
            )
            .await
        }
    };
    assert_eq!(add(json!("relative")).await.status(), 400);
    assert_eq!(add(json!(work.path().join("missing"))).await.status(), 400);
    let r = add(json!(extra)).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<ProjectSummary>().await.unwrap().paths.len(), 2);

    // Open: only this project's folders on this machine.
    let r = h
        .send(
            reqwest::Method::POST,
            &format!("/api/projects/{id}/open"),
            json!({"target": "folder", "path": work.path()}),
        )
        .await;
    assert_eq!(r.status(), 400);
    let r = h
        .send(
            reqwest::Method::POST,
            "/api/projects/nope/open",
            json!({"target": "folder", "path": dir}),
        )
        .await;
    assert_eq!(r.status(), 404);

    // To the Trash: gone from projects and session lists, listed in the Trash.
    let r = h
        .send(
            reqwest::Method::DELETE,
            &format!("/api/projects/{id}"),
            json!({}),
        )
        .await;
    assert_eq!(r.status(), 204);
    let live: Vec<ProjectSummary> = h.get("/api/projects").await;
    assert!(live.iter().all(|x| x.project.id != id));
    let trash: Vec<ProjectSummary> = h.get("/api/projects?deleted=true").await;
    assert_eq!(trash.len(), 1);
    assert_eq!(trash[0].project.id, id);
    let page: blirp_core::model::SessionsPage = h.get("/api/sessions").await;
    assert!(page.items.iter().all(|x| x.id != s.id));
    // The session itself is still there.
    let _: blirp_core::model::SessionDetail = h.get(&format!("/api/sessions/{}", s.id)).await;

    // Restore: the session is listed again and this machine's folders are back.
    let r = h
        .send(
            reqwest::Method::POST,
            &format!("/api/projects/{id}/restore"),
            json!({}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let back: ProjectSummary = r.json().await.unwrap();
    assert!(!back.project.deleted);
    assert_eq!(back.paths.len(), 2);
    let page: blirp_core::model::SessionsPage = h.get("/api/sessions").await;
    assert!(page.items.iter().any(|x| x.id == s.id));
    let trash: Vec<ProjectSummary> = h.get("/api/projects?deleted=true").await;
    assert!(trash.is_empty());
    let r = h
        .send(
            reqwest::Method::POST,
            &format!("/api/projects/{id}/restore"),
            json!({}),
        )
        .await;
    assert_eq!(r.status(), 404);

    // Chats cannot be renamed.
    let r = h
        .send(
            reqwest::Method::POST,
            &format!("/api/sessions/{}/move", s.id),
            json!({"project_id": null}),
        )
        .await;
    let chats = r.json::<Session>().await.unwrap().project_id;
    let r = h
        .send(
            reqwest::Method::PATCH,
            &format!("/api/projects/{chats}"),
            json!({"name": "Mine"}),
        )
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<ErrorBody>().await.unwrap().error.code,
        "invalid_request"
    );

    let Harness { daemon, _home, .. } = h;
    daemon.shutdown().await.unwrap();
}
