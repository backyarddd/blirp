//! Memory engine over HTTP: hook ingress, injection, continue_from handoff,
//! distill endpoint, agent integration status and MCP over Streamable HTTP.
//! Uses a temp BLIRP_HOME; never touches real agent configs (read-only
//! status probes aside).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use blirp::daemon::{Daemon, DaemonOptions};
use blirp_core::model::{
    AgentInfo, ErrorBody, Event, EventKind, InjectMode, Injection, Session, SessionOrigin,
    SessionStatus,
};
use blirp_core::paths::Paths;
use serde_json::{Value, json};
use std::time::Duration;

struct Harness {
    home: tempfile::TempDir,
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
            home,
        }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        let r = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "GET {path}: {}", r.status());
        r.json().await.unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn hook(&self, agent: &str, event: &str, body: Value) -> Value {
        let r = self
            .post(&format!("/api/hooks/{agent}/{event}"), body)
            .await;
        assert!(r.status().is_success(), "{event}: {}", r.status());
        r.json().await.unwrap()
    }

    /// A registered project folder with a brief; returns (folder, project id).
    async fn project(&self, name: &str) -> (String, String) {
        let dir = self.home.path().join("work").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dunce::canonicalize(&dir).unwrap().display().to_string();
        let r = self.post("/api/projects", json!({"path": path})).await;
        assert_eq!(r.status(), 201);
        let p: Value = r.json().await.unwrap();
        let id = p["id"].as_str().unwrap().to_string();
        let r = self
            .http
            .put(format!("{}/api/projects/{id}/brief", self.base))
            .bearer_auth(&self.token)
            .json(&json!({"body_md": format!("{name} is a test project.")}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
        (path, id)
    }
}

#[tokio::test]
async fn hook_lifecycle_of_an_external_session() {
    let h = Harness::start().await;
    let (dir, pid) = h.project("demo").await;
    let payload = |extra: Value| {
        let mut p =
            json!({"session_id": "ext-1", "cwd": dir, "transcript_path": "C:/t/ext-1.jsonl"});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"payload": p, "global": true})
    };

    let r = h
        .hook(
            "claude",
            "SessionStart",
            payload(json!({"source": "startup"})),
        )
        .await;
    let ctx = r["additional_context"].as_str().unwrap();
    assert!(ctx.starts_with("# blirp memory: demo"), "{ctx}");
    assert!(ctx.contains("demo is a test project."));
    let sid = r["session_id"].as_str().unwrap().to_string();
    let s: Session = h.get(&format!("/api/sessions/{sid}")).await;
    assert_eq!(
        (s.origin, s.status),
        (SessionOrigin::External, SessionStatus::Idle)
    );
    assert_eq!(s.project_id, pid);
    assert_eq!(s.agent_session_id.as_deref(), Some("ext-1"));
    assert_eq!(s.transcript_path.as_deref(), Some("C:/t/ext-1.jsonl"));

    for (event, extra, want) in [
        (
            "UserPromptSubmit",
            json!({"prompt": "hi"}),
            SessionStatus::Working,
        ),
        (
            "Notification",
            json!({"notification_type": "permission_prompt"}),
            SessionStatus::Waiting,
        ),
        ("Stop", json!({}), SessionStatus::Idle),
        (
            "SessionEnd",
            json!({"reason": "clear"}),
            SessionStatus::Idle,
        ),
        (
            "SessionEnd",
            json!({"reason": "prompt_input_exit"}),
            SessionStatus::Completed,
        ),
    ] {
        let r = h.hook("claude", event, payload(extra)).await;
        assert_eq!(r["session_id"], sid.as_str());
        assert!(r["additional_context"].is_null());
        let s: Session = h.get(&format!("/api/sessions/{sid}")).await;
        assert_eq!(s.status, want, "{event}");
    }
    // Resuming the external session makes it live again, same row.
    let r = h
        .hook(
            "claude",
            "SessionStart",
            payload(json!({"source": "resume"})),
        )
        .await;
    assert_eq!(r["session_id"], sid.as_str());
    let s: Session = h.get(&format!("/api/sessions/{sid}")).await;
    assert_eq!((s.status, s.ended_at), (SessionStatus::Idle, None));

    // Hooks without an agent session id or folder are ignored, not errors.
    let r = h.hook("claude", "Stop", json!({"payload": {}})).await;
    assert!(r["session_id"].is_null());
    let r = h.post("/api/hooks/vim/Stop", json!({})).await;
    assert_eq!(r.status(), 400);

    // Injection by folder.
    let inj: Injection = h.get(&format!("/api/inject?cwd={}", urlencode(&dir))).await;
    assert!(inj.markdown.contains("demo is a test project."));
    h.daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn hooks_of_headless_runs_create_no_session() {
    let h = Harness::start().await;
    let (dir, _pid) = h.project("bots").await;
    let body = |asid: &str, headless: bool| json!({"payload": {"session_id": asid, "cwd": dir}, "global": true, "headless": headless});
    let projects: Value = h.get("/api/projects").await;
    for event in ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"] {
        let r = h.hook("claude", event, body("bot-1", true)).await;
        assert!(r["session_id"].is_null(), "{event}: {r}");
        if event == "SessionStart" {
            // An app someone chats in over the Agent SDK still gets memory.
            let ctx = r["additional_context"].as_str().unwrap_or_default();
            assert!(ctx.contains("bots is a test project."), "{r}");
        } else {
            assert!(r["additional_context"].is_null(), "{event}: {r}");
        }
    }
    // A folder of no project: no memory, and no project is created.
    let elsewhere = h.home.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let r = h
        .hook(
            "claude",
            "SessionStart",
            json!({"payload": {"session_id": "bot-2", "cwd": elsewhere}, "global": true, "headless": true}),
        )
        .await;
    assert!(
        r["session_id"].is_null() && r["additional_context"].is_null(),
        "{r}"
    );
    let after: Value = h.get("/api/projects").await;
    assert_eq!(projects, after, "no project created");
    let sessions: Value = h.get("/api/sessions").await;
    assert!(
        !sessions.to_string().contains("bot-1"),
        "no row for a scripted run: {sessions}"
    );
    // A session blirp already tracks (e.g. `claude -p --resume` of an
    // interactive one) keeps its updates.
    let r = h
        .hook("claude", "SessionStart", body("human-1", false))
        .await;
    let sid = r["session_id"].as_str().unwrap().to_string();
    let r = h
        .hook("claude", "UserPromptSubmit", body("human-1", true))
        .await;
    assert_eq!(r["session_id"], sid.as_str());
    let s: Session = h.get(&format!("/api/sessions/{sid}")).await;
    assert_eq!(s.status, SessionStatus::Working);
    h.daemon.shutdown().await.unwrap();
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn continue_from_builds_a_handoff_and_distill_endpoint() {
    let h = Harness::start().await;
    let (dir, _pid) = h.project("handoff").await;
    let r = h
        .hook(
            "codex",
            "SessionStart",
            json!({"payload": {"session_id": "cx-1", "cwd": dir}}),
        )
        .await;
    let src = r["session_id"].as_str().unwrap().to_string();

    // No transcript yet: nothing to distill.
    let r = h
        .post(&format!("/api/sessions/{src}/distill"), json!({}))
        .await;
    assert_eq!(r.status(), 409);
    assert_eq!(
        r.json::<ErrorBody>().await.unwrap().error.code,
        "nothing_to_distill"
    );
    let r = h.post("/api/sessions/nope/distill", json!({})).await;
    assert_eq!(r.status(), 404);

    let store = h.daemon.state.store.clone();
    for (seq, kind, text) in [
        (1, EventKind::User, "implement the frobnicator"),
        (
            2,
            EventKind::Assistant,
            "frobnicator implemented in src/frob.rs",
        ),
    ] {
        store
            .insert_event(Event {
                session_id: src.clone(),
                seq,
                ts: seq,
                kind,
                text: text.into(),
                meta: None,
            })
            .unwrap();
    }

    let r = h
        .post(
            "/api/sessions",
            json!({"agent": "shell", "continue_from": src}),
        )
        .await;
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap_or_default());
    let s: Session = r.json().await.unwrap();
    assert_eq!(s.parent_session_id.as_deref(), Some(src.as_str()));
    assert_eq!(s.cwd, dir);
    let launch = h.daemon.state.paths.launch_dir(&s.id).unwrap();
    let handoff = std::fs::read_to_string(launch.join("handoff.md")).unwrap();
    assert!(handoff.contains("# blirp handoff"), "{handoff}");
    assert!(handoff.contains("**User:** implement the frobnicator"));
    let memory = std::fs::read_to_string(launch.join("memory.md")).unwrap();
    assert!(memory.starts_with("# blirp memory: handoff") && memory.contains("# blirp handoff"));
    // The UI's memory panel shows exactly what was injected.
    let inj: Injection = h.get(&format!("/api/inject?session={}", s.id)).await;
    assert_eq!(inj.markdown, memory);

    let r = h
        .post(&format!("/api/sessions/{src}/distill"), json!({}))
        .await;
    assert_eq!(r.status(), 202);

    let r = h
        .post(&format!("/api/sessions/{}/stop", s.id), json!({}))
        .await;
    assert!(r.status().is_success());
    let r = h
        .post(
            "/api/sessions",
            json!({"agent": "shell", "continue_from": "missing"}),
        )
        .await;
    assert_eq!(r.status(), 404);
    h.daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn agents_report_integration_and_mcp_http_needs_the_token() {
    let h = Harness::start().await;
    let agents: Vec<AgentInfo> = h.get("/api/agents").await;
    let claude = agents.iter().find(|a| a.id == "claude").unwrap();
    assert_eq!(claude.integration.inject, InjectMode::Hook);
    let aider = agents.iter().find(|a| a.id == "aider").unwrap();
    assert_eq!(aider.integration.inject, InjectMode::Flag);
    let r = h.post("/api/agents/pi/hooks/install", json!({})).await;
    assert_eq!(r.status(), 422);

    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}});
    let r = h
        .http
        .post(format!("{}/mcp", h.base))
        .header("Accept", "application/json, text/event-stream")
        .json(&init)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = h
        .http
        .post(format!("{}/mcp", h.base))
        .bearer_auth(&h.token)
        .header("Accept", "application/json, text/event-stream")
        .json(&init)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut body = String::new();
    let mut r = r;
    while let Ok(Ok(Some(chunk))) = tokio::time::timeout(Duration::from_secs(5), r.chunk()).await {
        body.push_str(&String::from_utf8_lossy(&chunk));
        if body.contains("serverInfo") {
            break;
        }
    }
    assert!(body.contains("\"name\":\"blirp\""), "{body}");
    h.daemon.shutdown().await.unwrap();
}

// Users can turn launch-time injection off, globally or per agent; launches
// and SessionStart hooks both respect it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injection_can_be_turned_off() {
    let h = Harness::start().await;
    let (dir, _) = h.project("quiet").await;
    let set = |memory: Value| {
        let h = &h;
        async move {
            let mut view: Value = h.get("/api/settings").await;
            for (k, v) in memory.as_object().unwrap() {
                view["config"]["memory"][k] = v.clone();
            }
            let r = h
                .http
                .patch(format!("{}/api/settings", h.base))
                .bearer_auth(&h.token)
                .json(&json!({"config": view["config"]}))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success(), "{}", r.status());
        }
    };
    let start = |sid: &'static str| {
        let dir = dir.clone();
        let h = &h;
        async move {
            h.hook(
                "claude",
                "SessionStart",
                json!({"payload": {"session_id": sid, "cwd": dir, "source": "startup"}, "global": true}),
            )
            .await
        }
    };

    set(json!({"inject_disabled_agents": ["claude", "shell"]})).await;
    assert!(start("q-1").await["additional_context"].is_null());
    // A shell launch writes an empty memory file.
    let r = h
        .post("/api/sessions", json!({"cwd": dir, "agent": "shell"}))
        .await;
    assert_eq!(r.status(), 201);
    let s: Session = r.json().await.unwrap();
    let memory = h.home.path().join("launch").join(&s.id).join("memory.md");
    assert_eq!(std::fs::read_to_string(memory).unwrap(), "");

    set(json!({"inject_disabled_agents": [], "inject": false})).await;
    assert!(start("q-2").await["additional_context"].is_null());

    set(json!({"inject": true})).await;
    let ctx = start("q-3").await;
    assert!(
        ctx["additional_context"]
            .as_str()
            .unwrap()
            .contains("quiet is a test project.")
    );
    h.daemon.shutdown().await.unwrap();
}
