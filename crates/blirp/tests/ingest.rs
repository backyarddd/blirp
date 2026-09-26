//! Transcript ingest (§8) against synthetic fixtures in `tests/fixtures/`:
//! normalized sessions and events per adapter, cursor resume, partial lines,
//! truncation, redaction, folder projects, linking, watcher and daemon wiring.

// Test helpers panic on failure by design.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use blirp::ingest::engine::Work;
use blirp::ingest::{Engine, IngestEnv, IngestService};
use blirp_core::model::{
    Event, EventKind, Machine, MachineRole, ServerEvent, Session, SessionOrigin, SessionStatus,
};
use blirp_core::store::Store;
use serde_json::json;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const CLAUDE_SID: &str = "11111111-1111-4111-8111-111111111111";
const CODEX_SID: &str = "0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
const PI_SID: &str = "22222222-2222-4222-8222-222222222222";
const GEMINI_SID: &str = "33333333-3333-4333-8333-333333333333";
const CURSOR_SID: &str = "44444444-4444-4444-8444-444444444444";
const AMP_SID: &str = "T-55555555-5555-4555-8555-555555555555";
const DSH_SID: &str = "session-66666666-6666-4666-8666-666666666666";

/// A fake GitHub token, assembled here so no secret-shaped string is committed.
fn secret() -> String {
    format!("ghp_{}", "Zq8x".repeat(9))
}

fn fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

struct H {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
    store: Arc<Store>,
    engine: Arc<Engine>,
    emitted: Arc<Mutex<Vec<ServerEvent>>>,
}

impl H {
    fn new() -> H {
        let tmp = tempfile::Builder::new()
            .prefix("blirpit")
            .tempdir()
            .unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let home = root.join("home");
        let cwd = root.join("work").join("proj");
        let blirp_home = root.join("blirp");
        for d in [&home, &cwd, &blirp_home] {
            std::fs::create_dir_all(d).unwrap();
        }
        let store = Arc::new(Store::open(&blirp_home.join("blirp.db")).unwrap());
        let machine = Machine {
            id: blirp_core::new_id(),
            name: "test-box".into(),
            os: std::env::consts::OS.into(),
            role: MachineRole::Standalone,
            last_seen: blirp_core::now_ms(),
            revoked: false,
        };
        store.upsert_machine(&machine).unwrap();
        store.set_setting("machine_id", &json!(machine.id)).unwrap();
        let emitted = Arc::new(Mutex::new(Vec::new()));
        let sink = emitted.clone();
        let engine = Engine::new(
            store.clone(),
            machine,
            IngestEnv::at_home(&home, &blirp_home),
            Arc::new(move |e| sink.lock().unwrap().push(e)),
        );
        H {
            _tmp: tmp,
            root,
            home,
            cwd,
            store,
            engine: Arc::new(engine),
            emitted,
        }
    }

    fn fill(&self, tpl: &str) -> String {
        let raw = self.cwd.to_string_lossy().into_owned();
        let json = serde_json::to_string(&raw).unwrap();
        let slashed = raw.replace('\\', "/");
        let uri = format!(
            "file://{}{}",
            if slashed.starts_with('/') { "" } else { "/" },
            slashed.replace(' ', "%20")
        );
        tpl.replace("{{CWD}}", &json[1..json.len() - 1])
            .replace("{{CWD_RAW}}", &raw.replace('\'', "''"))
            .replace("{{CWD_URI}}", &uri)
            .replace("{{SECRET}}", &secret())
            .replace("{{NOW_MS}}", &blirp_core::now_ms().to_string())
    }

    /// Write `content` to `rel` under the fake home.
    fn put(&self, rel: &str, content: &[u8]) -> PathBuf {
        let p = self.home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        p
    }

    fn pass(&self) {
        let n = self.engine.adapters().len();
        let stats = self.engine.run(&Work::all(n));
        for (id, s) in stats {
            assert_eq!(s.failed, 0, "{id} pass failed: {s:?}");
        }
    }

    fn session(&self, agent: &str, asid: &str) -> Session {
        self.store
            .session_by_agent_id(agent, asid)
            .unwrap()
            .unwrap_or_else(|| panic!("no {agent} session {asid}"))
    }

    fn events(&self, s: &Session) -> Vec<Event> {
        self.store.events_page(&s.id, -1, 1000).unwrap().0
    }

    fn outbox_len(&self) -> usize {
        self.store.outbox_after(0, 1_000_000).unwrap().len()
    }

    fn kinds(&self, s: &Session) -> Vec<EventKind> {
        self.events(s).iter().map(|e| e.kind).collect()
    }

    /// Checks every adapter must pass: redaction, folder project, origin.
    fn common(&self, s: &Session) {
        let events = self.events(s);
        assert!(!events.is_empty());
        let secret = secret();
        for e in &events {
            assert!(!e.text.contains(&secret), "secret leaked in {:?}", e.kind);
            if let Some(m) = &e.meta {
                assert!(!m.to_string().contains(&secret), "secret leaked in meta");
            }
        }
        assert!(
            events.iter().any(|e| e.text.contains("[REDACTED:github]")),
            "no redaction marker"
        );
        let seqs: Vec<i64> = events.iter().map(|e| e.seq).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(s.origin, SessionOrigin::External);
        let project = self.store.get_project(&s.project_id).unwrap().unwrap();
        assert_eq!(
            project.name, "proj",
            "non-git cwd is its own folder project"
        );
        let paths = self.store.project_paths(&s.project_id).unwrap();
        assert!(
            paths.iter().any(|p| Path::new(&p.path) == self.cwd),
            "{paths:?}"
        );
    }
}

fn append(path: &Path, content: &[u8]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(content).unwrap();
}

/// First `n` lines of `text`, newline-terminated.
fn head_lines(text: &str, n: usize) -> String {
    text.lines().take(n).map(|l| format!("{l}\n")).collect()
}

/// Appending half a line ingests nothing; completing it ingests it once.
fn check_partial_line(h: &H, path: &Path, s: &Session, line: &str) {
    let before = h.events(s).len();
    let (a, b) = line.split_at(line.len() / 2);
    append(path, a.as_bytes());
    h.pass();
    assert_eq!(h.events(s).len(), before, "partial line consumed");
    append(path, format!("{b}\n").as_bytes());
    h.pass();
    assert_eq!(h.events(s).len(), before + 1, "completed line not ingested");
}

/// Rewriting the file shorter re-reads it from 0 without duplicating or
/// losing stored events.
fn check_truncation(h: &H, path: &Path, s: &Session, keep_lines: usize) {
    let before = h.events(s);
    let text = std::fs::read_to_string(path).unwrap();
    std::fs::write(path, head_lines(&text, keep_lines)).unwrap();
    h.pass();
    assert_eq!(h.events(s), before, "truncation changed stored events");
}

// ---------------------------------------------------------------- claude

fn claude_line(uuid: &str, text: &str, h: &H) -> String {
    h.fill(&format!(
        r#"{{"parentUuid":null,"isSidechain":false,"type":"user","message":{{"role":"user","content":"{text}"}},"uuid":"{uuid}","timestamp":"2026-01-01T10:02:00.000Z","cwd":"{{{{CWD}}}}","sessionId":"{CLAUDE_SID}"}}"#
    ))
}

fn put_claude(h: &H) -> PathBuf {
    let dir = format!(".claude/projects/C--work-proj/{CLAUDE_SID}");
    h.put(
        &format!("{dir}/subagents/agent-a1.jsonl"),
        h.fill(&fixture("claude/agent-a1.jsonl")).as_bytes(),
    );
    h.put(
        &format!("{dir}/subagents/agent-a1.meta.json"),
        fixture("claude/agent-a1.meta.json").as_bytes(),
    );
    h.put(
        &format!("{dir}.jsonl"),
        h.fill(&fixture("claude/session.jsonl")).as_bytes(),
    )
}

// Hooks report transcript paths as the agent spells them; on Windows that
// may differ from the watched root in case and separators. The hint must
// still reach the adapter and continue the known source (not start a second
// one under the other spelling).
#[cfg(windows)]
#[test]
fn claude_hint_paths_match_case_insensitively() {
    let h = H::new();
    let rel = format!(".claude/projects/C--work-proj/{CLAUDE_SID}.jsonl");
    let line = |uuid: &str, text: &str| format!("{}\n", claude_line(uuid, text, &h));
    let path = h.put(&rel, line("u-1", "first prompt").as_bytes());
    h.pass();
    let s = h.session("claude", CLAUDE_SID);
    let before = h.events(&s).len();
    append(&path, line("u-2", "second prompt").as_bytes());

    let hinted = PathBuf::from(path.to_string_lossy().to_uppercase().replace('\\', "/"));
    let ix = h
        .engine
        .adapter_for(&hinted)
        .expect("hinted path outside every root");
    assert_eq!(h.engine.adapters()[ix].id(), "claude");
    let mut work = Work::default();
    work.paths.entry(ix).or_default().insert(hinted.clone());
    h.engine.run(&work);

    assert_eq!(h.events(&s).len(), before + 1);
    let key = |p: &Path| p.to_string_lossy().into_owned();
    assert!(
        h.store
            .get_cursor("claude", &key(&hinted))
            .unwrap()
            .is_none()
    );
    // The source as the scan spells it.
    let scanned = h
        .home
        .join(".claude")
        .join("projects")
        .join("C--work-proj")
        .join(format!("{CLAUDE_SID}.jsonl"));
    assert!(
        h.store
            .get_cursor("claude", &key(&scanned))
            .unwrap()
            .is_some()
    );
}

#[test]
fn claude_sessions_subagents_resume_partial_truncation() {
    let h = H::new();
    let main = put_claude(&h);
    h.pass();

    let s = h.session("claude", CLAUDE_SID);
    h.common(&s);
    assert_eq!(s.title.as_deref(), Some("Greeting helper"));
    assert_eq!(
        (s.tokens_in, s.tokens_out),
        (1110, 70),
        "usage repeated per line counts once"
    );
    assert!(
        (s.cost_usd - 0.42).abs() < 1e-9,
        "reported cost wins over the estimate"
    );
    assert_eq!(s.branch.as_deref(), Some("main"));
    assert_eq!(s.status, SessionStatus::Completed);
    assert_eq!(s.started_at, 1_767_261_600_000);
    assert_eq!(
        s.transcript_path.as_deref().map(Path::new),
        Some(main.as_path())
    );
    let ev = h.events(&s);
    let got: Vec<(i64, EventKind)> = ev.iter().map(|e| (e.seq, e.kind)).collect();
    assert_eq!(
        got,
        [
            (1024, EventKind::User),
            (3072, EventKind::Assistant),
            (4096, EventKind::ToolCall),
            (4097, EventKind::FileEdit),
            (5120, EventKind::ToolResult),
            (6144, EventKind::ToolResult),
            (9216, EventKind::System),
            (10240, EventKind::System),
            (11264, EventKind::Summary),
        ]
    );
    assert!(
        ev[0]
            .text
            .starts_with("Add a greeting helper\nUse token [REDACTED:github]")
    );
    assert_eq!(ev[2].text, "Write(src/greet.py)");
    assert_eq!(ev[3].meta.as_ref().unwrap()["path"], "src/greet.py");
    assert_eq!(
        ev[5].meta.as_ref().unwrap()["result_file"],
        "/tmp/s/tool-results/big.txt"
    );
    assert_eq!(ev[0].ts, 1_767_261_600_000);

    let child = h.session("claude", &format!("{CLAUDE_SID}:agent-a1"));
    assert_eq!(child.parent_session_id.as_deref(), Some(s.id.as_str()));
    assert_eq!(
        child.title.as_deref(),
        Some("subagent (Explore): Find files")
    );
    assert_eq!(h.kinds(&child), [EventKind::User, EventKind::Assistant]);
    let created = h
        .emitted
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, ServerEvent::SessionCreated { .. }))
        .count();
    assert_eq!(created, 2);

    // Unchanged sources cost nothing.
    let outbox = h.outbox_len();
    h.pass();
    assert_eq!(h.outbox_len(), outbox);

    // Resume after append.
    append(&main, h.fill(&fixture("claude/append.jsonl")).as_bytes());
    h.pass();
    let ev = h.events(&s);
    assert_eq!(ev.len(), 10);
    assert_eq!(
        (ev[9].seq, ev[9].text.as_str()),
        (12 * 1024, "Now add tests")
    );

    check_partial_line(&h, &main, &s, &claude_line("u7", "partial one", &h));
    check_truncation(&h, &main, &s, 3);
    // Identical content appended after the truncation dedupes.
    let original = h.fill(&fixture("claude/session.jsonl"));
    std::fs::write(&main, &original).unwrap();
    h.pass();
    assert_eq!(h.events(&s).len(), 11);
}

#[test]
fn claude_external_status_follows_activity() {
    let h = H::new();
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    // Timestamps "now": the session is live.
    let live = fixture("claude/session.jsonl")
        .lines()
        .map(|l| {
            let mut v: serde_json::Value = serde_json::from_str(l).unwrap();
            if v.get("timestamp").is_some() {
                v["timestamp"] = json!(now);
            }
            v.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    h.put(
        &format!(".claude/projects/x/{CLAUDE_SID}.jsonl"),
        h.fill(&live).as_bytes(),
    );
    h.pass();
    let s = h.session("claude", CLAUDE_SID);
    assert_eq!(s.status, SessionStatus::Working);
    assert_eq!(s.ended_at, None);
    h.engine.sweep_stale();
    assert_eq!(
        h.session("claude", CLAUDE_SID).status,
        SessionStatus::Working
    );
    h.store
        .modify_session(&s.id, |s| s.last_activity_at -= 3 * 60_000)
        .unwrap();
    h.engine.sweep_stale();
    let done = h.session("claude", CLAUDE_SID);
    assert_eq!(done.status, SessionStatus::Completed);
    assert_eq!(done.ended_at, Some(done.last_activity_at));
}

#[test]
fn claude_session_launched_by_blirp_keeps_its_fields() {
    let h = H::new();
    let resolved = h.store.resolve_project("m", "box", &h.cwd).unwrap();
    let launched = Session {
        id: blirp_core::new_id(),
        project_id: resolved.project.id.clone(),
        // Launched on this machine: ingest only touches local sessions.
        machine_id: h.store.machine_id().unwrap().unwrap(),
        agent: "claude".into(),
        agent_session_id: Some(CLAUDE_SID.into()),
        origin: SessionOrigin::Blirp,
        cwd: h.cwd.display().to_string(),
        title: None,
        status: SessionStatus::Idle,
        branch: None,
        worktree: Some("/wt/amber-otter-0000".into()),
        transcript_path: None,
        started_at: 1_767_261_590_000,
        ended_at: None,
        last_activity_at: 1_767_261_590_000,
        exit_code: None,
        summary: None,
        distilled_through_seq: 0,
        tokens_in: 0,
        tokens_out: 0,
        cost_usd: 0.0,
        parent_session_id: None,
        stopped_by_user: false,
    };
    h.store.insert_session(&launched).unwrap();
    put_claude(&h);
    h.pass();
    let s = h.session("claude", CLAUDE_SID);
    assert_eq!(s.id, launched.id);
    assert_eq!(s.origin, SessionOrigin::Blirp);
    assert_eq!(s.status, SessionStatus::Idle, "PTY-owned status untouched");
    assert_eq!(s.worktree, launched.worktree);
    assert_eq!(s.started_at, launched.started_at);
    assert_eq!(s.title.as_deref(), Some("Greeting helper"));
    assert_eq!(s.tokens_in, 1110);
    assert_eq!(h.events(&s).len(), 9);
}

#[test]
fn transcripts_of_blirp_own_runs_are_skipped() {
    let h = H::new();
    let scratch = h.root.join("blirp").join("distill");
    std::fs::create_dir_all(&scratch).unwrap();
    let raw = scratch.to_string_lossy().into_owned();
    let json = serde_json::to_string(&raw).unwrap();
    let text = fixture("claude/session.jsonl")
        .replace("{{CWD}}", &json[1..json.len() - 1])
        .replace("{{SECRET}}", "x");
    h.put(
        &format!(".claude/projects/x/{CLAUDE_SID}.jsonl"),
        text.as_bytes(),
    );
    h.pass();
    assert!(
        h.store
            .session_by_agent_id("claude", CLAUDE_SID)
            .unwrap()
            .is_none()
    );
}

// ---------------------------------------------------------------- codex

#[test]
fn codex_rollout_links_to_blirp_launch() {
    let h = H::new();
    let project = h.store.resolve_project("m", "box", &h.cwd).unwrap().project;
    let mk = |started_at: i64| Session {
        id: blirp_core::new_id(),
        project_id: project.id.clone(),
        machine_id: String::new(),
        agent: "codex".into(),
        agent_session_id: None,
        origin: SessionOrigin::Blirp,
        cwd: h.cwd.display().to_string(),
        title: None,
        status: SessionStatus::Working,
        branch: None,
        worktree: None,
        transcript_path: None,
        started_at,
        ended_at: None,
        last_activity_at: started_at,
        exit_code: None,
        summary: None,
        distilled_through_seq: 0,
        tokens_in: 0,
        tokens_out: 0,
        cost_usd: 0.0,
        parent_session_id: None,
        stopped_by_user: false,
    };
    // Machine id of the engine's machine.
    let machine = h.store.get_setting("machine_id").unwrap().unwrap();
    let machine = machine.as_str().unwrap().to_string();
    let first_event = 1_767_344_400_000; // 2026-01-02T09:00:00Z
    let too_early = Session {
        machine_id: machine.clone(),
        ..mk(first_event - 120_000)
    };
    let launched = Session {
        machine_id: machine,
        ..mk(first_event - 20_000)
    };
    h.store.insert_session(&too_early).unwrap();
    h.store.insert_session(&launched).unwrap();

    let path = h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T09-00-00-{CODEX_SID}.jsonl"),
        h.fill(&fixture("codex/rollout.jsonl")).as_bytes(),
    );
    h.pass();
    let s = h.session("codex", CODEX_SID);
    assert_eq!(s.id, launched.id, "linked by agent, cwd and launch time");
    assert_eq!(s.origin, SessionOrigin::Blirp);
    assert_eq!(
        s.status,
        SessionStatus::Working,
        "blirp-owned status untouched"
    );
    assert!(
        h.store
            .get_session(&too_early.id)
            .unwrap()
            .unwrap()
            .agent_session_id
            .is_none()
    );
    assert_eq!(s.title.as_deref(), Some("Rename the build script"));
    assert_eq!((s.tokens_in, s.tokens_out), (2000, 300));
    assert!(s.cost_usd > 0.0);
    let ev = h.events(&s);
    let got: Vec<(i64, EventKind)> = ev.iter().map(|e| (e.seq, e.kind)).collect();
    assert_eq!(
        got,
        [
            (3072, EventKind::System),
            (4096, EventKind::User),
            (7168, EventKind::ToolCall),
            (8192, EventKind::ToolResult),
            (9216, EventKind::ToolCall),
            (9217, EventKind::FileEdit),
            (10240, EventKind::ToolResult),
            (11264, EventKind::Assistant),
        ]
    );
    assert_eq!(ev[2].text, "shell(ls -la)");
    assert_eq!(ev[3].text, "build.sh\n");
    assert_eq!(ev[3].meta.as_ref().unwrap()["tool"], "shell");
    assert_eq!(ev[5].meta.as_ref().unwrap()["path"], "build.sh");
    assert!(ev[1].text.contains("[REDACTED:github]"));

    append(&path, fixture("codex/append.jsonl").as_bytes());
    h.pass();
    let s = h.session("codex", CODEX_SID);
    assert_eq!(
        (s.tokens_in, s.tokens_out),
        (3000, 400),
        "latest cumulative usage"
    );
    assert_eq!(h.events(&s).last().unwrap().text, "Also update the README");

    let line = r#"{"timestamp":"2026-01-02T09:06:00.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Updated."}]}}"#;
    check_partial_line(&h, &path, &s, line);
    check_truncation(&h, &path, &s, 5);
}

#[test]
fn codex_external_session_gets_folder_project() {
    let h = H::new();
    h.put(
        &format!(".codex/archived_sessions/rollout-2026-01-02T09-00-00-{CODEX_SID}.jsonl"),
        h.fill(&fixture("codex/rollout.jsonl")).as_bytes(),
    );
    h.pass();
    let s = h.session("codex", CODEX_SID);
    h.common(&s);
}

// ---------------------------------------------------------------- opencode

#[test]
fn opencode_sqlite_sessions() {
    let h = H::new();
    let db_path = h.home.join(".local/share/opencode/opencode.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch(&h.fill(&fixture("opencode/opencode.sql")))
        .unwrap();
    h.pass();

    let s = h.session("opencode", "ses_parent");
    h.common(&s);
    assert_eq!(
        s.title.as_deref(),
        Some("Refactor the parser"),
        "placeholder title ignored"
    );
    assert_eq!((s.tokens_in, s.tokens_out), (1400, 65));
    assert!((s.cost_usd - 0.05).abs() < 1e-9);
    assert_eq!(s.status, SessionStatus::Working, "updated just now");
    let ev = h.events(&s);
    let got: Vec<(i64, EventKind)> = ev.iter().map(|e| (e.seq, e.kind)).collect();
    assert_eq!(
        got,
        [
            (0, EventKind::User),
            (1, EventKind::Assistant),
            (2, EventKind::ToolCall),
            (3, EventKind::FileEdit),
            (4, EventKind::ToolResult),
            (5, EventKind::ToolCall),
            (6, EventKind::ToolResult),
        ],
        "synthetic parts skipped, streaming message held back"
    );
    assert_eq!(ev[3].meta.as_ref().unwrap()["path"], "src/parser.rs");
    assert_eq!(ev[6].meta.as_ref().unwrap()["is_error"], true);

    let child = h.session("opencode", "ses_child");
    assert_eq!(child.parent_session_id.as_deref(), Some(s.id.as_str()));
    assert_eq!(
        child.title.as_deref(),
        Some("Explore config (@explore subagent)")
    );

    // The streaming message completes.
    let done = r#"{"role":"assistant","time":{"created":1767427210000,"completed":1767427219000},"finish":"stop"}"#;
    db.execute("UPDATE message SET data = ?1 WHERE id = 'msg_003'", [done])
        .unwrap();
    db.execute(
        "UPDATE session SET time_updated = time_updated + 1 WHERE id = 'ses_parent'",
        [],
    )
    .unwrap();
    h.pass();
    let ev = h.events(&s);
    assert_eq!(
        (ev.len(), ev[7].text.as_str()),
        (8, "Running the tests now")
    );

    // Resume after new messages.
    db.execute_batch(
        "INSERT INTO message VALUES ('msg_004','ses_parent',1767427300000,1767427300000,'{\"role\":\"user\"}');
         INSERT INTO part VALUES ('prt_010','msg_004','ses_parent',1767427300000,1767427300000,'{\"type\":\"text\",\"text\":\"Add docs\"}');
         UPDATE session SET time_updated = time_updated + 1 WHERE id = 'ses_parent';",
    )
    .unwrap();
    h.pass();
    let ev = h.events(&s);
    assert_eq!(
        (ev.len(), ev[8].seq, ev[8].text.as_str()),
        (9, 8, "Add docs")
    );

    // Deleting a session in opencode never deletes blirp's copy.
    db.execute_batch("DELETE FROM part WHERE session_id = 'ses_child'; DELETE FROM message WHERE session_id = 'ses_child'; DELETE FROM session WHERE id = 'ses_child';").unwrap();
    h.pass();
    let child = h.session("opencode", "ses_child");
    assert_eq!(h.events(&child).len(), 2);
}

// ---------------------------------------------------------------- pi

#[test]
fn pi_sessions() {
    let h = H::new();
    let path = h.put(
        &format!(".pi/agent/sessions/--work-proj--/2026-01-04T12-00-00-000Z_{PI_SID}.jsonl"),
        h.fill(&fixture("pi/session.jsonl")).as_bytes(),
    );
    h.pass();
    let s = h.session("pi", PI_SID);
    h.common(&s);
    assert_eq!(s.title.as_deref(), Some("Flaky test fix"));
    assert_eq!((s.tokens_in, s.tokens_out), (650, 30));
    assert!((s.cost_usd - 0.00105).abs() < 1e-9, "pi's own cost");
    let got: Vec<(i64, EventKind)> = h.events(&s).iter().map(|e| (e.seq, e.kind)).collect();
    assert_eq!(
        got,
        [
            (3072, EventKind::User),
            (4096, EventKind::Assistant),
            (4097, EventKind::ToolCall),
            (5120, EventKind::ToolResult),
            (6144, EventKind::ToolCall),
            (6145, EventKind::FileEdit),
            (7168, EventKind::ToolResult),
            (9216, EventKind::Summary),
        ]
    );
    append(&path, fixture("pi/append.jsonl").as_bytes());
    h.pass();
    let ev = h.events(&s);
    assert_eq!(ev[ev.len() - 2].text, "bash(cargo test)");
    assert_eq!(ev[ev.len() - 1].text, "ok. 3 passed");

    let line = r#"{"type":"message","id":"e0000011","parentId":"e0000010","timestamp":"2026-01-04T12:06:00.000Z","message":{"role":"user","content":"thanks","timestamp":1767528360000}}"#;
    check_partial_line(&h, &path, &s, line);
    check_truncation(&h, &path, &s, 4);
}

// ---------------------------------------------------------------- gemini

#[test]
fn gemini_chats_dedupe_reappended_messages() {
    let h = H::new();
    h.put(
        ".gemini/tmp/proj-1/.project_root",
        h.cwd.to_string_lossy().as_bytes(),
    );
    let path = h.put(
        ".gemini/tmp/proj-1/chats/session-2026-01-05T08-00-33333333.jsonl",
        h.fill(&fixture("gemini/session.jsonl")).as_bytes(),
    );
    h.pass();
    let s = h.session("gemini", GEMINI_SID);
    h.common(&s);
    assert_eq!(s.title.as_deref(), Some("Changelog entry"));
    assert_eq!((s.tokens_in, s.tokens_out), (1000, 50));
    let got: Vec<(i64, EventKind)> = h.events(&s).iter().map(|e| (e.seq, e.kind)).collect();
    assert_eq!(
        got,
        [
            (0, EventKind::User),
            (1, EventKind::ToolCall),
            (2, EventKind::FileEdit),
            (3, EventKind::ToolResult),
            (4, EventKind::Assistant),
        ],
        "a message re-appended with results adds only the result"
    );
    assert_eq!(h.events(&s)[3].text, "Wrote CHANGELOG.md");

    append(&path, fixture("gemini/append.jsonl").as_bytes());
    h.pass();
    assert_eq!(h.events(&s).len(), 6);

    let line = r#"{"id":"m-user-3","timestamp":"2026-01-05T08:03:00.000Z","type":"user","content":"More"}"#;
    check_partial_line(&h, &path, &s, line);

    // Gemini rewrites the file (resume): nothing is stored twice.
    let before = h.events(&s);
    std::fs::write(&path, h.fill(&fixture("gemini/rewritten.jsonl"))).unwrap();
    h.pass();
    assert_eq!(h.events(&s), before);
    check_truncation(&h, &path, &s, 2);
}

// ---------------------------------------------------------------- cursor

/// Cursor's project directory name for a folder: separators become dashes.
fn cursor_dir_name(p: &Path) -> String {
    p.to_string_lossy()
        .replace(":\\", "-")
        .replace(['\\', '/'], "-")
        .trim_start_matches('-')
        .to_string()
}

#[test]
fn cursor_agent_transcripts() {
    let h = H::new();
    let id = "77777777-7777-4777-8777-777777777777";
    let path = h.put(
        &format!(
            ".cursor/projects/{}/agent-transcripts/{id}/{id}.jsonl",
            cursor_dir_name(&h.cwd)
        ),
        h.fill(&fixture("cursor/transcript.jsonl")).as_bytes(),
    );
    h.pass();
    let s = h.session("cursor", id);
    h.common(&s);
    assert_eq!(s.title.as_deref(), Some("List the TODOs"));
    let ev = h.events(&s);
    assert_eq!(
        ev.iter().map(|e| e.kind).collect::<Vec<_>>(),
        [
            EventKind::User,
            EventKind::Assistant,
            EventKind::ToolCall,
            EventKind::ToolResult,
            EventKind::Assistant
        ]
    );
    assert!(
        ev[0]
            .text
            .starts_with("List the TODOs\nkey [REDACTED:github]")
    );
    assert_eq!(ev[2].text, "Grep(TODO)");

    append(&path, fixture("cursor/append.jsonl").as_bytes());
    h.pass();
    assert_eq!(h.events(&s).last().unwrap().text, "Fix it");
    let line = r#"{"role":"assistant","message":{"content":[{"type":"text","text":"Fixed."}]}}"#;
    check_partial_line(&h, &path, &s, line);
    check_truncation(&h, &path, &s, 2);
}

/// A store.db holding `messages` under one root node.
fn write_cursor_store(h: &H, db: &Path, messages: &[serde_json::Value]) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS blobs (id TEXT PRIMARY KEY, data BLOB);
         CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    let mut root = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        let id = [u8::try_from(i + 1).unwrap(); 32];
        conn.execute(
            "INSERT OR REPLACE INTO blobs VALUES (?1, ?2)",
            rusqlite::params![hex::encode(id), m.to_string().into_bytes()],
        )
        .unwrap();
        root.extend([0x0a, 0x20]);
        root.extend(id);
    }
    let root_id = format!("{:064x}", messages.len());
    conn.execute(
        "INSERT OR REPLACE INTO blobs VALUES (?1, ?2)",
        rusqlite::params![root_id, root],
    )
    .unwrap();
    let meta = json!({
        "agentId": CURSOR_SID, "latestRootBlobId": root_id, "name": "Logging",
        "mode": "auto-run", "createdAt": 1_767_600_000_000i64, "lastUsedModel": "claude-4.5-sonnet"
    });
    conn.execute(
        "INSERT OR REPLACE INTO meta VALUES ('0', ?1)",
        [hex::encode(meta.to_string())],
    )
    .unwrap();
    let side = json!({"schemaVersion": 1, "createdAtMs": 1_767_600_000_000i64, "cwd": h.cwd});
    std::fs::write(db.with_file_name("meta.json"), side.to_string()).unwrap();
}

#[test]
fn cursor_chat_store_db() {
    let h = H::new();
    let db = h.home.join(format!(
        ".cursor/chats/0123456789abcdef0123456789abcdef/{CURSOR_SID}/store.db"
    ));
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let messages: Vec<serde_json::Value> =
        serde_json::from_str(&h.fill(&fixture("cursor/store-messages.json"))).unwrap();
    write_cursor_store(&h, &db, &messages);
    // The transcript of the same chat is ignored in favor of the store.
    h.put(
        &format!(".cursor/projects/x/agent-transcripts/{CURSOR_SID}/{CURSOR_SID}.jsonl"),
        h.fill(&fixture("cursor/transcript.jsonl")).as_bytes(),
    );
    h.pass();
    let s = h.session("cursor", CURSOR_SID);
    h.common(&s);
    assert_eq!(s.title.as_deref(), Some("Logging"));
    let ev = h.events(&s);
    assert_eq!(
        ev.iter().map(|e| (e.seq, e.kind)).collect::<Vec<_>>(),
        [
            (0, EventKind::User),
            (1, EventKind::Assistant),
            (2, EventKind::ToolCall),
            (3, EventKind::FileEdit),
            (4, EventKind::ToolResult),
            (5, EventKind::Assistant),
        ]
    );
    assert!(ev[0].text.starts_with("Add logging"));

    // A new turn: new root listing old and new messages.
    let mut more = messages.clone();
    more.push(json!({"role": "user", "content": "<user_query>\nand metrics\n</user_query>"}));
    write_cursor_store(&h, &db, &more);
    h.pass();
    let ev = h.events(&s);
    assert_eq!((ev.len(), ev[6].text.as_str()), (7, "and metrics"));

    // History rewritten shorter (e.g. a revert): nothing lost or duplicated.
    write_cursor_store(&h, &db, &more[..2]);
    h.pass();
    assert_eq!(h.events(&s), ev);
}

// ---------------------------------------------------------------- amp

#[test]
fn amp_threads() {
    let h = H::new();
    let path = h.put(
        &format!(".local/share/amp/threads/{AMP_SID}.json"),
        h.fill(&fixture("amp/thread.json")).as_bytes(),
    );
    h.pass();
    let s = h.session("amp", AMP_SID);
    h.common(&s);
    assert_eq!(s.title.as_deref(), Some("Tidy imports"));
    assert_eq!((s.tokens_in, s.tokens_out), (620, 35));
    let got: Vec<(i64, EventKind)> = h.events(&s).iter().map(|e| (e.seq, e.kind)).collect();
    assert_eq!(
        got,
        [
            (0, EventKind::User),
            (1024, EventKind::Assistant),
            (1025, EventKind::ToolCall),
            (1026, EventKind::FileEdit),
            (2048, EventKind::ToolResult),
            (3072, EventKind::Assistant),
        ],
        "the streaming message waits"
    );
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    v["messages"][4]["state"] = json!({"type": "complete"});
    std::fs::write(&path, v.to_string()).unwrap();
    h.pass();
    assert_eq!(h.events(&s).last().unwrap().text, "Also");

    // Partially written file: skipped until valid.
    let before = h.events(&s);
    std::fs::write(&path, &v.to_string()[..40]).unwrap();
    h.pass();
    assert_eq!(h.events(&s), before);
    // Thread edited down to two messages: nothing lost, nothing duplicated.
    v["messages"] = json!(v["messages"].as_array().unwrap()[..2].to_vec());
    std::fs::write(&path, v.to_string()).unwrap();
    h.pass();
    assert_eq!(h.events(&s), before);
}

// ---------------------------------------------------------------- aider

#[test]
fn aider_history_in_registered_folders() {
    let h = H::new();
    let machine = h.store.get_setting("machine_id").unwrap().unwrap();
    h.store
        .resolve_project(machine.as_str().unwrap(), "box", &h.cwd)
        .unwrap();
    let path = h.cwd.join(".aider.chat.history.md");
    std::fs::write(&path, h.fill(&fixture("aider/history.md"))).unwrap();
    h.pass();
    let sessions = h
        .store
        .list_sessions(&blirp_core::store::SessionFilter {
            agent: Some("aider".into()),
            limit: 10,
            ..Default::default()
        })
        .unwrap()
        .items;
    assert_eq!(sessions.len(), 1);
    let s = sessions[0].clone();
    h.common(&s);
    assert_eq!(s.title.as_deref(), Some("Add a --verbose flag"));
    assert_eq!((s.tokens_in, s.tokens_out, s.cost_usd), (2300, 150, 0.01));
    assert_eq!(
        h.kinds(&s),
        [
            EventKind::System,
            EventKind::User,
            EventKind::Assistant,
            EventKind::System,
            EventKind::FileEdit,
            EventKind::User,
            EventKind::Assistant,
        ],
        "the last block waits until it is complete"
    );

    // A new chat closes the previous one and starts a second session.
    append(&path, fixture("aider/append.md").as_bytes());
    h.pass();
    let s = h.store.get_session(&s.id).unwrap().unwrap();
    assert_eq!((s.tokens_in, s.tokens_out, s.cost_usd), (5300, 240, 0.03));
    let ev = h.events(&s);
    assert_eq!(ev.last().unwrap().text, "edited README.md");
    let all = h
        .store
        .list_sessions(&blirp_core::store::SessionFilter {
            agent: Some("aider".into()),
            limit: 10,
            ..Default::default()
        })
        .unwrap()
        .items;
    assert_eq!(all.len(), 2);
    let second = all.iter().find(|x| x.id != s.id).unwrap();
    assert_eq!(second.title.as_deref(), Some("Second session prompt"));
    assert_eq!(h.kinds(second).len(), 3, "trailing token report still open");

    // A quiet file flushes its last block.
    let old = SystemTime::now() - Duration::from_secs(120);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(old)
        .unwrap();
    h.pass();
    assert_eq!(h.kinds(second).len(), 4);
    let second = h.store.get_session(&second.id).unwrap().unwrap();
    assert_eq!(second.cost_usd, 0.001);

    // Truncated history: stored events stay, nothing is duplicated.
    let before = h.events(&s);
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, head_lines(&text, 12)).unwrap();
    h.pass();
    assert_eq!(h.events(&s), before);
}

// ---------------------------------------------------------------- dsh

#[test]
fn dsh_zstd_event_log() {
    let h = H::new();
    let frame = |s: &str| zstd::encode_all(s.as_bytes(), 3).unwrap();
    let path = h.put(
        &format!(".dsh/sessions/--work-proj--/{DSH_SID}/session.v3.jsonl.zstd"),
        &frame(&h.fill(&fixture("dsh/session.v3.jsonl"))),
    );
    // Unknown log versions are skipped.
    h.put(
        ".dsh/sessions/--work-proj--/session-x/session.v9.jsonl.zstd",
        &frame("{}\n"),
    );
    h.pass();
    let s = h.session("dsh", DSH_SID);
    h.common(&s);
    assert_eq!(s.title.as_deref(), Some("Version bump"));
    assert_eq!((s.tokens_in, s.tokens_out), (1700, 52));
    let got: Vec<(i64, EventKind)> = h.events(&s).iter().map(|e| (e.seq, e.kind)).collect();
    assert_eq!(
        got,
        [
            (2048, EventKind::System),
            (4096, EventKind::User),
            (6144, EventKind::ToolCall),
            (6145, EventKind::FileEdit),
            (7168, EventKind::ToolResult),
            (8192, EventKind::Assistant),
        ]
    );

    append(&path, &frame(&fixture("dsh/append.jsonl")));
    h.pass();
    assert_eq!(h.events(&s).last().unwrap().text, "Tag the release");

    // A frame still being written is ignored until complete.
    let before = h.events(&s);
    let torn = frame(
        "{\"type\":\"user/message\",\"seq\":12,\"time\":1767800200000,\"data\":{\"role\":\"user\",\"content\":\"x\",\"source\":{\"kind\":\"user\"}}}\n",
    );
    append(&path, &torn[..torn.len() / 2]);
    h.pass();
    assert_eq!(h.events(&s), before);

    // Rewritten shorter: re-read without duplicates.
    std::fs::write(
        &path,
        frame(&head_lines(&h.fill(&fixture("dsh/session.v3.jsonl")), 6)),
    )
    .unwrap();
    h.pass();
    assert_eq!(h.events(&s), before);
}

// ---------------------------------------------------------------- notifications

#[test]
fn updates_are_rate_limited_per_session() {
    let h = H::new();
    put_claude(&h);
    h.pass();
    let s = h.session("claude", CLAUDE_SID);
    h.emitted.lock().unwrap().clear();
    let n = h.engine.notifier();
    n.updated(s.clone());
    n.updated(s.clone());
    n.updated(s.clone());
    let count = |h: &H| {
        h.emitted
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, ServerEvent::SessionUpdated { .. }))
            .count()
    };
    // Created just now counts as the last send, so all three wait.
    assert!(count(&h) <= 1);
    std::thread::sleep(Duration::from_millis(1100));
    n.flush();
    let after = count(&h);
    assert!((1..=2).contains(&after), "{after}");
    n.flush();
    assert_eq!(count(&h), after, "nothing pending twice");
}

// ---------------------------------------------------------------- watcher and daemon

async fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_ingests_new_transcripts() {
    let h = H::new();
    std::fs::create_dir_all(h.home.join(".claude/projects")).unwrap();
    let svc = IngestService::start(h.engine.clone());
    // The startup scan has run once this is recorded.
    wait_for("startup scan", Duration::from_secs(10), || {
        h.store
            .get_setting("ingest.claude.last_at")
            .unwrap()
            .is_some()
    })
    .await;
    // Only the watcher can pick this up: the next rescan is minutes away.
    put_claude(&h);
    wait_for("watched transcript", Duration::from_secs(15), || {
        h.store
            .session_by_agent_id("claude", CLAUDE_SID)
            .unwrap()
            .is_some()
    })
    .await;
    let s = h.session("claude", CLAUDE_SID);
    wait_for("all events", Duration::from_secs(10), || {
        h.events(&s).len() == 9
    })
    .await;
    tokio::time::timeout(Duration::from_secs(10), svc.shutdown())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_runs_ingest_and_serves_sessions() {
    let h = H::new();
    put_claude(&h);
    let blirp_home = h.root.join("daemon-home");
    let daemon = blirp::daemon::Daemon::start(blirp::daemon::DaemonOptions {
        paths: blirp_core::paths::Paths::at(&blirp_home),
        port: Some(0),
        ingest: Some(IngestEnv::at_home(&h.home, &blirp_home)),
    })
    .await
    .unwrap();
    let url = format!("http://127.0.0.1:{}/api/sessions?agent=claude", daemon.port);
    let client = reqwest::Client::new();
    let start = Instant::now();
    loop {
        let page: blirp_core::model::SessionsPage = client
            .get(&url)
            .bearer_auth(daemon.token())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if page
            .items
            .iter()
            .any(|s| s.agent_session_id.as_deref() == Some(CLAUDE_SID))
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "daemon did not ingest"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    daemon.shutdown().await.unwrap();
}

/// A hook that reports `transcript_path` gets that transcript ingested at
/// once (memory's `IngestTrigger`), without waiting for a watcher or rescan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_transcript_path_is_ingested_promptly() {
    let h = H::new();
    let claude_dir = h.root.join("claude-config");
    let blirp_home = h.root.join("daemon-home");
    let mut env = IngestEnv::at_home(&h.home, &blirp_home);
    env.vars
        .insert("CLAUDE_CONFIG_DIR".into(), claude_dir.clone().into());
    let daemon = blirp::daemon::Daemon::start(blirp::daemon::DaemonOptions {
        paths: blirp_core::paths::Paths::at(&blirp_home),
        port: Some(0),
        ingest: Some(env),
    })
    .await
    .unwrap();
    let store = daemon.state.store.clone();
    wait_for("startup scan", Duration::from_secs(10), || {
        store
            .get_setting("ingest.claude.last_at")
            .unwrap()
            .is_some()
    })
    .await;

    // `projects/` did not exist at startup, so nothing watches it and the
    // next rescan is minutes away: only the hook hint can pick this up.
    let transcript = claude_dir
        .join("projects")
        .join("C--work-proj")
        .join(format!("{CLAUDE_SID}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, h.fill(&fixture("claude/session.jsonl"))).unwrap();

    let reply: serde_json::Value = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{}/api/hooks/claude/Stop",
            daemon.port
        ))
        .bearer_auth(daemon.token())
        .json(&json!({
            "cwd": h.cwd,
            "payload": {
                "session_id": CLAUDE_SID,
                "transcript_path": transcript,
                "cwd": h.cwd,
                "hook_event_name": "Stop",
            },
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let hook_session = reply["session_id"].as_str().unwrap().to_string();

    let start = Instant::now();
    wait_for("hinted transcript", Duration::from_secs(5), || {
        store.events_page(&hook_session, -1, 1000).unwrap().0.len() == 9
    })
    .await;
    assert!(start.elapsed() < Duration::from_secs(5));
    // Ingest filled the session the hook created, through the outbox.
    let s = store
        .session_by_agent_id("claude", CLAUDE_SID)
        .unwrap()
        .unwrap();
    assert_eq!(s.id, hook_session);
    assert_eq!(s.title.as_deref(), Some("Greeting helper"));
    let outbox = store.outbox_after(0, 1_000_000).unwrap();
    assert_eq!(outbox.iter().filter(|e| e.entity == "events").count(), 9);
    daemon.shutdown().await.unwrap();
}

/// Sessions replicated from another machine are never re-ingested or
/// re-linked here, even when their transcript is visible (synced config
/// dir): no rows change, nothing enters the outbox.
#[test]
fn transcripts_of_other_machines_sessions_are_left_alone() {
    let h = H::new();
    let project = h.store.register_project("elsewhere", &h.cwd, None).unwrap();
    let remote = Session {
        id: "remote-session".into(),
        project_id: project.id,
        machine_id: "elsewhere".into(),
        agent: "claude".into(),
        agent_session_id: Some(CLAUDE_SID.into()),
        origin: SessionOrigin::External,
        cwd: h.cwd.display().to_string(),
        title: Some("theirs".into()),
        status: SessionStatus::Completed,
        branch: None,
        worktree: None,
        transcript_path: None,
        started_at: 1,
        ended_at: Some(1),
        last_activity_at: 1,
        exit_code: None,
        summary: None,
        distilled_through_seq: 0,
        tokens_in: 0,
        tokens_out: 0,
        cost_usd: 0.0,
        parent_session_id: None,
        stopped_by_user: false,
    };
    h.store
        .apply_remote(&blirp_core::store::Change::Session(remote.clone()))
        .unwrap();
    let outbox = h.outbox_len();
    put_claude(&h);
    h.pass();

    assert_eq!(h.session("claude", CLAUDE_SID), remote);
    assert!(h.events(&remote).is_empty());
    // Its subagent belongs to the same machine and is skipped too.
    assert!(
        h.store
            .session_by_agent_id("claude", &format!("{CLAUDE_SID}:agent-a1"))
            .unwrap()
            .is_none()
    );
    assert_eq!(h.outbox_len(), outbox);
}
