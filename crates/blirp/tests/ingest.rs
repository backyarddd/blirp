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
use blirp_core::store::{HubPage, NonProjectDirs, Store};
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
    root: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
    store: Arc<Store>,
    engine: Arc<Engine>,
    emitted: Arc<Mutex<Vec<ServerEvent>>>,
    // Last: fields drop in order, and Windows cannot remove the folder
    // while the handles above are open.
    _tmp: tempfile::TempDir,
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
        // An actual project folder (§5): sessions there make a project.
        std::fs::write(cwd.join("package.json"), "{}").unwrap();
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
            IngestEnv {
                // `root/tmp` stands in for the temp folder.
                non_projects: NonProjectDirs::auto(Some(home.clone()), vec![root.join("tmp")])
                    .with_workspaces(&blirp_home.join("workspaces")),
                ..IngestEnv::at_home(&home, &blirp_home)
            },
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
    // The compaction summary's time: the agent's context filled up then.
    assert_eq!(s.compacted_at, Some(1_767_261_608_000));
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
    assert_eq!(child.compacted_at, None);
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
        title_updated_at: 0,
        project_updated_at: 0,
        compacted_at: None,
        context_near_full_at: None,
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

/// `n` user lines of a Claude transcript, with or without `cwd`.
fn claude_lines(h: &H, from: usize, n: usize, with_cwd: bool) -> String {
    (from..from + n)
        .map(|i| {
            let l = claude_line(&format!("u{i}"), &format!("prompt number {i}"), h);
            let l = if with_cwd {
                l
            } else {
                let at = l.find(r#","cwd":""#).unwrap();
                let value = at + 8;
                let end = value + l[value..].find('"').unwrap() + 1;
                format!("{}{}", &l[..at], &l[end..])
            };
            format!("{l}\n")
        })
        .collect()
}

// A transcript longer than one ingest batch is flushed mid-read; the row
// must still be created with the transcript's cwd (project, BLIRP_HOME
// exclusion), not the home folder.
#[test]
fn long_transcripts_are_filed_by_their_cwd() {
    let h = H::new();
    // cwd on every line.
    h.put(
        &format!(".claude/projects/x/{CLAUDE_SID}.jsonl"),
        claude_lines(&h, 0, 1100, true).as_bytes(),
    );
    // cwd only after the first batch.
    let late = "aaaaaaaa-1111-4111-8111-111111111111";
    let mut text = claude_lines(&h, 0, 1100, false);
    text.push_str(&claude_lines(&h, 1100, 5, true));
    h.put(&format!(".claude/projects/x/{late}.jsonl"), text.as_bytes());
    h.pass();
    let machine = h.store.machine_id().unwrap().unwrap();
    let project = h
        .store
        .find_project_for_path(&machine, &h.cwd, None)
        .unwrap()
        .expect("the cwd became a project");
    for asid in [CLAUDE_SID, late] {
        let s = h.session("claude", asid);
        assert_eq!(Path::new(&s.cwd), h.cwd, "{asid}");
        assert_eq!(s.project_id, project.id, "{asid}");
    }
    let s = h.session("claude", CLAUDE_SID);
    assert_eq!(h.store.max_event_seq(&s.id).unwrap(), 1099 * 1024);

    // blirp's own long run is skipped as a whole.
    let h = H::new();
    let scratch = h.root.join("blirp").join("distill").join("run-1");
    std::fs::create_dir_all(&scratch).unwrap();
    let own = H { cwd: scratch, ..h };
    own.put(
        &format!(".claude/projects/x/{CLAUDE_SID}.jsonl"),
        claude_lines(&own, 0, 1100, true).as_bytes(),
    );
    own.pass();
    assert!(
        own.store
            .session_by_agent_id("claude", CLAUDE_SID)
            .unwrap()
            .is_none()
    );
}

/// One source holding two sessions: A's cwd is reported before any of its
/// events, then a batch of B's events forces a mid-read flush, then A's
/// event arrives.
struct TwoSessions(PathBuf);

impl blirp::ingest::Adapter for TwoSessions {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    fn scan(&self, _: &Store) -> blirp::ingest::Result<Vec<blirp::ingest::Source>> {
        Ok(vec![blirp::ingest::Source {
            key: "two".into(),
            path: self.0.clone(),
            fingerprint: "1".into(),
            mtime_ms: 1,
            item: None,
        }])
    }
    fn ingest(
        &self,
        _: &blirp::ingest::Source,
        _: Option<blirp::ingest::Cursor>,
        sink: &mut dyn blirp::ingest::EventSink,
    ) -> blirp::ingest::Result<blirp::ingest::Cursor> {
        let meta = |cwd: &Path| blirp::ingest::SessionMeta {
            cwd: Some(cwd.display().to_string()),
            ..Default::default()
        };
        let ev = |seq: i64| blirp::ingest::NormEvent {
            seq,
            ts: Some(1),
            kind: EventKind::User,
            text: "hi".into(),
            meta: None,
        };
        sink.session("a", meta(&self.0));
        sink.session("b", meta(&self.0));
        for seq in 0..1000 {
            sink.event("b", ev(seq))?;
        }
        sink.event("a", ev(0))?;
        blirp::ingest::Cursor::from_state(&json!({}))
    }
}

// A cwd reported before the session's first event survives a mid-read
// flush triggered by another session of the same source.
#[test]
fn early_cwd_survives_another_sessions_flush() {
    let h = H::new();
    let machine = h.store.list_machines().unwrap().remove(0);
    let engine = Engine::with_adapters(
        h.store.clone(),
        machine,
        IngestEnv::at_home(&h.home, &h.root.join("blirp")),
        vec![Box::new(TwoSessions(h.cwd.clone()))],
        Arc::new(|_| {}),
    );
    let stats = engine.run(&Work::all(1));
    assert_eq!(stats["claude"].failed, 0, "{stats:?}");
    for asid in ["a", "b"] {
        assert_eq!(Path::new(&h.session("claude", asid).cwd), h.cwd, "{asid}");
    }
}

// Rows filed under the home folder by the old mid-read flush are re-filed
// once: the repair re-reads their transcripts from the start. A later
// incremental read never moves a row (a `cd` is not a new start).
#[test]
fn rows_filed_under_home_are_repaired_once() {
    let h = H::new();
    let path = h.put(
        &format!(".claude/projects/x/{CLAUDE_SID}.jsonl"),
        claude_lines(&h, 0, 3, false).as_bytes(),
    );
    h.pass();
    let home = h.session("claude", CLAUDE_SID);
    assert_eq!(Path::new(&home.cwd), h.home);
    append(&path, claude_lines(&h, 3, 2, true).as_bytes());
    h.pass();
    let s = h.session("claude", CLAUDE_SID);
    assert_eq!(
        (s.cwd.as_str(), s.project_id.as_str()),
        (home.cwd.as_str(), home.project_id.as_str())
    );

    h.engine.repair_home_filed();
    h.pass();
    let s = h.session("claude", CLAUDE_SID);
    assert_eq!(Path::new(&s.cwd), h.cwd);
    assert_ne!(s.project_id, home.project_id);
    assert_eq!(h.events(&s).len(), 5, "re-read added no duplicates");
    // Once only.
    h.store
        .modify_session(&s.id, |s| s.cwd = home.cwd.clone())
        .unwrap();
    h.engine.repair_home_filed();
    h.pass();
    assert_eq!(h.session("claude", CLAUDE_SID).cwd, home.cwd);
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
        title_updated_at: 0,
        project_updated_at: 0,
        compacted_at: None,
        context_near_full_at: None,
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

// A `compacted` rollout line (synthetic, in the format of a real codex 0.153
// rollout): the history was replaced by a compacted one.
#[test]
fn codex_compaction_marks_the_session() {
    let h = H::new();
    let path = h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T09-00-00-{CODEX_SID}.jsonl"),
        h.fill(&fixture("codex/rollout.jsonl")).as_bytes(),
    );
    h.pass();
    assert_eq!(h.session("codex", CODEX_SID).compacted_at, None);
    append(&path, fixture("codex/compacted.jsonl").as_bytes());
    h.pass();
    let s = h.session("codex", CODEX_SID);
    assert_eq!(s.compacted_at, Some(1_767_345_000_000)); // 2026-01-02T09:10:00Z
    let ev = h.events(&s);
    let n = ev.len();
    assert_eq!(
        (ev[n - 2].kind, ev[n - 2].text.as_str()),
        (EventKind::Summary, "conversation compacted")
    );
    assert_eq!(ev[n - 1].text, "Continuing after the compaction.");
}

// A fork (a Codex Desktop subagent) starts with a copy of its parent's
// rollout (synthetic, in the format of real codex 0.153 forks): the parent's
// `session_meta`, messages and compactions up to
// `subagent_history_start_ordinal`. The fork holds only its own events and
// hangs under its parent, also when the parent is ingested after it.
#[test]
fn codex_fork_skips_the_parents_copied_history_and_links_to_it() {
    let h = H::new();
    let fork = "0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4f0f";
    let path = h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T10-00-00-{fork}.jsonl"),
        b"",
    );
    let text = h.fill(&fixture("codex/fork.jsonl"));
    let lines: Vec<&str> = text.lines().collect();
    let first: String = lines[..7].iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, first).unwrap();
    h.pass();
    let s = h.session("codex", fork);
    assert_eq!(s.parent_session_id, None, "the parent is not known yet");
    assert_eq!(s.title.as_deref(), Some("subagent (Feynman): rename_check"));
    assert_eq!(
        s.compacted_at, None,
        "the parent's compaction is not the fork's"
    );
    let texts = |s: &Session| -> Vec<String> { h.events(s).into_iter().map(|e| e.text).collect() };
    assert_eq!(texts(&s), ["Checking the scripts as the subagent."]);

    // The parent shows up, then the fork goes on: linked, own events only.
    h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T09-00-00-{CODEX_SID}.jsonl"),
        h.fill(&fixture("codex/rollout.jsonl")).as_bytes(),
    );
    h.pass();
    let parent = h.session("codex", CODEX_SID);
    append(
        &path,
        lines[7..]
            .iter()
            .map(|l| format!("{l}\n"))
            .collect::<String>()
            .as_bytes(),
    );
    h.pass();
    let s = h.session("codex", fork);
    assert_eq!(s.parent_session_id.as_deref(), Some(parent.id.as_str()));
    assert_eq!(s.project_id, parent.project_id);
    assert_eq!(s.compacted_at, Some(1_767_349_800_000)); // 2026-01-02T10:30:00Z
    assert_eq!(
        texts(&s),
        [
            "Checking the scripts as the subagent.",
            "The subagent renamed build.sh."
        ]
    );
    assert_eq!(
        (s.tokens_in, s.tokens_out),
        (400, 20),
        "the fork's own usage"
    );
    assert!(parent.title.is_some() && parent.parent_session_id.is_none());
}

// Codex reports each call's input and the model's context window: crossing
// 90% of it marks the session once per crossing (staying above does not
// move the mark; dropping below and crossing again does).
#[test]
fn codex_nearly_full_context_marks_the_session_once_per_crossing() {
    let h = H::new();
    let path = h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T09-00-00-{CODEX_SID}.jsonl"),
        h.fill(&fixture("codex/rollout.jsonl")).as_bytes(),
    );
    h.pass();
    assert_eq!(h.session("codex", CODEX_SID).context_near_full_at, None);
    append(&path, fixture("codex/context.jsonl").as_bytes());
    h.pass();
    let s = h.session("codex", CODEX_SID);
    // 09:21 (90.5%), not 09:22 (still above).
    assert_eq!(s.context_near_full_at, Some(1_767_345_660_000));
    assert_eq!((s.tokens_in, s.tokens_out), (500_000, 700));
    let below = r#"{"timestamp":"2026-01-02T09:23:00.000Z","ordinal":33,"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":40000},"model_context_window":200000}}}"#;
    let again = r#"{"timestamp":"2026-01-02T09:40:00.000Z","ordinal":34,"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":190000},"model_context_window":200000}}}"#;
    append(&path, format!("{below}\n").as_bytes());
    h.pass();
    assert_eq!(
        h.session("codex", CODEX_SID).context_near_full_at,
        Some(1_767_345_660_000)
    );
    append(&path, format!("{again}\n").as_bytes());
    h.pass();
    assert_eq!(
        h.session("codex", CODEX_SID).context_near_full_at,
        Some(1_767_346_800_000)
    );
}

// Codex subagents ingested by an earlier build: a fork holds its parent's
// copied history, no parent link, the parent's title, and records the
// distiller made from it. The one-time repair drops the copy (a replicated
// truncation), links the parent on the next read, fixes the title and
// removes records only that fork's distill produced; it runs once.
#[test]
fn codex_subagent_repair_cleans_up_what_earlier_builds_ingested() {
    use blirp_core::model::{Record, RecordKind, RecordStatus};
    use blirp_core::store::Change;
    let h = H::new();
    let fork = "0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4f0f";
    h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T09-00-00-{CODEX_SID}.jsonl"),
        h.fill(&fixture("codex/rollout.jsonl")).as_bytes(),
    );
    h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T10-00-00-{fork}.jsonl"),
        h.fill(&fixture("codex/fork.jsonl")).as_bytes(),
    );
    h.pass();
    let parent = h.session("codex", CODEX_SID);
    let s = h.session("codex", fork);
    // What the earlier build stored: the copied turns (lines 3 and 4), the
    // parent's first prompt as title, no link.
    for (seq, kind, text) in [
        (3 * 1024, EventKind::User, "Rename the build script"),
        (
            4 * 1024,
            EventKind::Assistant,
            "Renamed and updated build.sh.",
        ),
    ] {
        h.store
            .apply(Change::Event(Event {
                session_id: s.id.clone(),
                seq,
                ts: 1,
                kind,
                text: text.into(),
                meta: None,
            }))
            .unwrap();
    }
    h.store
        .modify_session(&s.id, |x| {
            x.title.clone_from(&parent.title);
            x.parent_session_id = None;
            // Summarized with the copied turns.
            x.summary = Some(json!({"summary": "renamed the build script", "distilled_at": 120}));
            x.distilled_through_seq = 8 * 1024;
        })
        .unwrap();
    h.store
        .modify_session(&parent.id, |x| {
            x.summary = Some(json!({"summary": "parent", "distilled_at": 150}));
        })
        .unwrap();
    let record = |id: &str, created: i64, pinned: bool, by: &str| Record {
        id: id.into(),
        project_id: s.project_id.clone(),
        kind: RecordKind::Decision,
        title: id.into(),
        body: String::new(),
        status: RecordStatus::Active,
        pinned,
        source_session_id: Some(s.id.clone()),
        created_at: created,
        updated_at: created,
        updated_by: by.into(),
    };
    for r in [
        record("seen-by-parent", 100, false, "distiller"),
        record("fork-only", 200, false, "distiller"),
        record("pinned", 200, true, "distiller"),
        record("edited", 200, false, "user"),
    ] {
        h.store.apply(Change::Record(r)).unwrap();
    }
    // A rollout that cannot be read holds up nothing.
    let bad = h.put(
        ".codex/sessions/2026/01/02/rollout-2026-01-02T08-00-00-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4bad.jsonl",
        b"{}\n",
    );
    #[cfg(windows)]
    let _held = {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&bad)
            .unwrap()
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    let outbox = h.outbox_len();

    // Upgraded from that build: the repair has not run (every pass ran it
    // above, before anything was stored).
    let key = "ingest.repair.codex_subagents".to_string();
    h.store
        .set_settings(&std::collections::BTreeMap::from([(key.clone(), None)]))
        .unwrap();
    // A node waits until a pull reached the hub's head: the hub may still
    // be sending rows the repair changes.
    let machine_id = h.store.machine_id().unwrap().unwrap();
    let mut m = h.store.get_machine(&machine_id).unwrap().unwrap();
    m.role = MachineRole::Node;
    h.store.upsert_machine(&m).unwrap();
    h.engine.repair_codex_subagents();
    assert_eq!(h.store.get_setting(&key).unwrap(), None);
    assert_eq!(
        h.events(&h.session("codex", fork)).len(),
        4,
        "not repaired yet"
    );
    h.store
        .node_apply_pull(
            "hub",
            &HubPage {
                own_seen: 0,
                entries: Vec::new(),
                up_to: 1,
                more: false,
            },
        )
        .unwrap();
    h.engine.repair_codex_subagents();
    let s = h.session("codex", fork);
    let texts: Vec<String> = h.events(&s).into_iter().map(|e| e.text).collect();
    assert_eq!(
        texts,
        [
            "Checking the scripts as the subagent.",
            "The subagent renamed build.sh."
        ]
    );
    assert_eq!(s.title.as_deref(), Some("subagent (Feynman): rename_check"));
    // Its summary came from the copy too: summarized again from its own.
    assert_eq!(
        (s.summary.clone(), s.distilled_through_seq),
        (None, blirp_core::model::NOT_DISTILLED)
    );
    let left: Vec<String> = h
        .store
        .list_records(&s.project_id, &Default::default())
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    let mut left = left;
    left.sort();
    assert_eq!(left, ["edited", "pinned", "seen-by-parent"]);
    let queued: Vec<String> = h
        .store
        .outbox_after(0, 1_000_000)
        .unwrap()
        .into_iter()
        .skip(outbox)
        .map(|e| format!("{}/{}", e.entity, e.op))
        .collect();
    assert!(
        queued.contains(&"event_floors/upsert".to_string()),
        "{queued:?}"
    );
    assert!(queued.contains(&"records/delete".to_string()), "{queued:?}");

    // The next pass re-reads the fork and links it; the copy stays out.
    #[cfg(windows)]
    drop(_held);
    std::fs::remove_file(&bad).unwrap();
    h.pass();
    let s = h.session("codex", fork);
    assert_eq!(s.parent_session_id.as_deref(), Some(parent.id.as_str()));
    assert_eq!(h.events(&s).len(), 2);

    // Once only.
    h.store
        .apply(Change::Record(record("later", 300, false, "distiller")))
        .unwrap();
    h.engine.repair_codex_subagents();
    assert!(
        h.store
            .list_records(&s.project_id, &Default::default())
            .unwrap()
            .iter()
            .any(|r| r.id == "later")
    );
}

// Forks of codex versions that do not write `subagent_history_start_ordinal`
// are recognized by the parent's `session_meta` in second place; the copy is
// stamped at the fork's start.
#[test]
fn codex_fork_without_a_start_ordinal_skips_the_copy_by_time() {
    let h = H::new();
    let fork = "0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4f0f";
    let text = h
        .fill(&fixture("codex/fork.jsonl"))
        .replace(",\"subagent_history_start_ordinal\":5", "")
        .replace(
            "\"forked_from_id\":\"0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b\",",
            "",
        );
    assert!(!text.contains("subagent_history_start_ordinal") && !text.contains("forked_from_id"));
    h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T10-00-00-{fork}.jsonl"),
        text.as_bytes(),
    );
    h.pass();
    let s = h.session("codex", fork);
    let texts: Vec<String> = h.events(&s).into_iter().map(|e| e.text).collect();
    assert_eq!(
        texts,
        [
            "Checking the scripts as the subagent.",
            "The subagent renamed build.sh."
        ]
    );
    assert_eq!(s.compacted_at, Some(1_767_349_800_000));
}

// What a fork copied from its parent (the parent's `session_meta` and its
// turns) is not a run or a turn of the fork: a `codex exec` fork with one
// turn of its own is a scripted run, whatever its parent was.
#[test]
fn codex_fork_is_judged_by_its_own_runs_only() {
    let h = H::new();
    let fork = "0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4f0f";
    let turn = |ordinal: u32, t: &str| {
        format!(
            r#"{{"timestamp":"2026-01-02T10:00:{t}Z","ordinal":{ordinal},"type":"turn_context","payload":{{"turn_id":"t{ordinal}","model":"gpt-5-codex"}}}}"#
        )
    };
    let mut lines: Vec<String> = h
        .fill(&fixture("codex/fork.jsonl"))
        .replace(
            r#""source":{"subagent":{"thread_spawn":{"parent_thread_id":"0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b","depth":1}}}"#,
            r#""source":"exec""#,
        )
        .lines()
        .map(str::to_string)
        .collect();
    assert!(lines[0].contains(r#""source":"exec""#) && lines[1].contains(r#""source":"vscode""#));
    // The parent's copied turns, then the fork's one turn.
    lines.insert(2, turn(2, "00.000"));
    lines.insert(3, turn(3, "00.001"));
    lines.insert(6, turn(6, "04.000"));
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    // With `subagent_history_start_ordinal`, and an older fork without it
    // (told by the parent's `session_meta`, its copy by time).
    let older = text
        .replace(",\"subagent_history_start_ordinal\":5", "")
        .replace(
            "\"forked_from_id\":\"0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b\",",
            "",
        );
    assert!(!older.contains("subagent_history_start_ordinal"));
    for (i, text) in [text, older].iter().enumerate() {
        let id = format!("{}{i}", &fork[..fork.len() - 1]);
        h.put(
            &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T10-00-00-{id}.jsonl"),
            text.replace(fork, &id).as_bytes(),
        );
        h.pass();
        assert!(
            h.store.session_by_agent_id("codex", &id).unwrap().is_none(),
            "{i}: the parent's interactive run and turns do not keep a scripted fork"
        );
    }
}

#[test]
fn codex_rollouts_in_scratch_folders_create_no_project() {
    let h = H::new();
    let cwds = [
        (h.root.join("tmp").join("sandbox"), true),
        (h.home.join(".codex").join("worktrees").join("w1"), true),
        (h.home.join(".tool").join("profiles").join("default"), true),
        (
            h.home
                .join("Documents")
                .join("Codex")
                .join("2026-01-02")
                .join("new-chat"),
            true,
        ),
        (h.home.clone(), true),
        // No git, no project marker: a chat.
        (h.root.join("work").join("notes"), true),
        (h.cwd.clone(), false),
    ];
    let mut ids = Vec::new();
    for (i, (cwd, _)) in cwds.iter().enumerate() {
        std::fs::create_dir_all(cwd).unwrap();
        let sid = format!("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a{i:02}");
        let raw = cwd.to_string_lossy().into_owned();
        let json = serde_json::to_string(&raw).unwrap();
        let text = fixture("codex/rollout.jsonl")
            .replace(CODEX_SID, &sid)
            .replace("{{CWD}}", &json[1..json.len() - 1])
            .replace("{{SECRET}}", "x");
        h.put(
            &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T09-00-0{i}-{sid}.jsonl"),
            text.as_bytes(),
        );
        ids.push(sid);
    }
    h.pass();
    let home_project = h.store.home_project_id().unwrap().unwrap();
    for ((cwd, scratch), sid) in cwds.iter().zip(&ids) {
        let s = h.session("codex", sid);
        assert_eq!(s.project_id == home_project, *scratch, "{}", cwd.display());
    }
    // Only the real folder became a project (next to Home).
    let names: Vec<String> = h
        .store
        .list_project_summaries("")
        .unwrap()
        .into_iter()
        .filter(|p| !p.is_home)
        .map(|p| p.project.name)
        .collect();
    assert_eq!(names, ["proj"]);
}

#[test]
fn scratch_projects_from_before_are_retired_once_a_node_is_in_sync() {
    let h = H::new();
    let chat = h
        .home
        .join("Documents")
        .join("Codex")
        .join("2026-01-02")
        .join("chat");
    std::fs::create_dir_all(&chat).unwrap();
    // As blirp 0.1.0 filed it: a project for the chat folder.
    let machine_id = h
        .store
        .get_setting("machine_id")
        .unwrap()
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let old = NonProjectDirs {
        home: Some(h.home.clone()),
        ..NonProjectDirs::default()
    };
    let p = h
        .store
        .resolve_project_with(&machine_id, "test-box", &chat, &old)
        .unwrap()
        .project;
    let raw = chat.to_string_lossy().into_owned();
    let json = serde_json::to_string(&raw).unwrap();
    h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T09-00-00-{CODEX_SID}.jsonl"),
        fixture("codex/rollout.jsonl")
            .replace("{{CWD}}", &json[1..json.len() - 1])
            .replace("{{SECRET}}", "x")
            .as_bytes(),
    );
    // A node waits until a pull reached the hub's head.
    let mut m = h.store.get_machine(&machine_id).unwrap().unwrap();
    m.role = MachineRole::Node;
    h.store.upsert_machine(&m).unwrap();
    h.pass();
    assert_eq!(h.session("codex", CODEX_SID).project_id, p.id);
    h.pass();
    assert!(!h.store.get_project(&p.id).unwrap().unwrap().deleted);
    h.store
        .node_apply_pull(
            "hub",
            &HubPage {
                own_seen: 0,
                entries: Vec::new(),
                up_to: 1,
                more: false,
            },
        )
        .unwrap();
    h.pass();
    assert!(h.store.get_project(&p.id).unwrap().unwrap().deleted);
    let home = h.store.home_project_id().unwrap().unwrap();
    assert_eq!(h.session("codex", CODEX_SID).project_id, home);
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

    // A message whose parts are not written yet waits for them.
    let now = blirp_core::now_ms();
    db.execute_batch(&format!(
        "INSERT INTO message VALUES ('msg_005','ses_parent',{now},{now},'{{\"role\":\"user\"}}');
         UPDATE session SET time_updated = time_updated + 1 WHERE id = 'ses_parent';"
    ))
    .unwrap();
    h.pass();
    assert_eq!(h.events(&s).len(), 9);
    db.execute_batch(&format!(
        "INSERT INTO part VALUES ('prt_011','msg_005','ses_parent',{now},{now},'{{\"type\":\"text\",\"text\":\"Parts came later\"}}');
         UPDATE session SET time_updated = time_updated + 1 WHERE id = 'ses_parent';"
    ))
    .unwrap();
    h.pass();
    let ev = h.events(&s);
    assert_eq!((ev.len(), ev[9].text.as_str()), (10, "Parts came later"));

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
    assert_eq!(s.compacted_at, Some(1_767_528_007_000));
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
    for m in messages {
        // Content addressed like Cursor's store: identical messages share a blob.
        use sha2::Digest as _;
        let id: [u8; 32] = sha2::Sha256::digest(m.to_string().as_bytes()).into();
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

    // The same message again (same blob) is a new turn, not a duplicate.
    more.push(json!({"role": "assistant", "content": [{"type": "text", "text": "ok"}]}));
    more.push(json!({"role": "user", "content": "<user_query>\nand metrics\n</user_query>"}));
    write_cursor_store(&h, &db, &more);
    h.pass();
    let ev = h.events(&s);
    assert_eq!(
        ev[7..].iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
        ["ok", "and metrics"]
    );

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
    // A message added after the edit is stored, past every earlier seq.
    v["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role": "user", "content": [{"type": "text", "text": "after the edit"}]}));
    std::fs::write(&path, v.to_string()).unwrap();
    h.pass();
    let after = h.events(&s);
    assert_eq!(after.len(), before.len() + 1);
    let last = after.last().unwrap();
    assert_eq!(last.text, "after the edit");
    assert!(last.seq > before.last().unwrap().seq);
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
    // Ingest filled the session the hook created.
    let s = store
        .session_by_agent_id("claude", CLAUDE_SID)
        .unwrap()
        .unwrap();
    assert_eq!(s.id, hook_session);
    assert_eq!(s.title.as_deref(), Some("Greeting helper"));
    // A standalone daemon queues nothing for replication.
    assert!(store.outbox_after(0, 1_000_000).unwrap().is_empty());
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
        title_updated_at: 0,
        project_updated_at: 0,
        compacted_at: None,
        context_near_full_at: None,
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

// A project without folders works in blirp's workspace inside BLIRP_HOME:
// its transcripts are ingested (unlike blirp's own runs there) and filed
// under the project, before the scratch rules that would make a folder in
// BLIRP_HOME a chat.
#[test]
fn workspace_transcripts_file_under_their_project() {
    let h = H::new();
    let project = h.store.create_project("Design", None).unwrap();
    let ws = blirp_core::paths::Paths::at(h.root.join("blirp"))
        .ensure_workspace(&project.id)
        .unwrap();
    let raw = ws.to_string_lossy().into_owned();
    let json = serde_json::to_string(&raw).unwrap();
    h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T09-00-00-{CODEX_SID}.jsonl"),
        fixture("codex/rollout.jsonl")
            .replace("{{CWD}}", &json[1..json.len() - 1])
            .replace("{{SECRET}}", "x")
            .as_bytes(),
    );
    h.pass();
    let s = h.session("codex", CODEX_SID);
    assert_eq!(s.project_id, project.id);
    assert_eq!(Path::new(&s.cwd), ws);
    // Still no folder rows: nothing for the cleanup to look at.
    assert!(h.store.project_paths(&project.id).unwrap().is_empty());
    h.engine.retire_non_projects();
    assert!(!h.store.get_project(&project.id).unwrap().unwrap().deleted);
    assert_eq!(h.session("codex", CODEX_SID).project_id, project.id);
}

// A subagent transcript that arrives after its parent was moved to another
// project files under the parent's project, not by its own folder.
#[test]
fn subagents_follow_their_moved_parent() {
    let h = H::new();
    let dir = format!(".claude/projects/C--work-proj/{CLAUDE_SID}");
    h.put(
        &format!("{dir}.jsonl"),
        h.fill(&fixture("claude/session.jsonl")).as_bytes(),
    );
    h.pass();
    let parent = h.session("claude", CLAUDE_SID);
    let elsewhere = h.store.create_project("Elsewhere", None).unwrap();
    let machine = h.store.machine_id().unwrap().unwrap();
    h.store
        .move_session(&parent.id, Some(&elsewhere.id), &machine, "test-box")
        .unwrap();
    h.put(
        &format!("{dir}/subagents/agent-a1.jsonl"),
        h.fill(&fixture("claude/agent-a1.jsonl")).as_bytes(),
    );
    h.put(
        &format!("{dir}/subagents/agent-a1.meta.json"),
        fixture("claude/agent-a1.meta.json").as_bytes(),
    );
    h.pass();
    let child = h.session("claude", &format!("{CLAUDE_SID}:agent-a1"));
    assert_eq!(child.parent_session_id.as_deref(), Some(parent.id.as_str()));
    assert_eq!(child.project_id, elsewhere.id);
    // More of the parent's transcript never moves it back.
    h.pass();
    assert_eq!(h.session("claude", CLAUDE_SID).project_id, elsewhere.id);
}

// Re-filing a row that was filed under the home folder never undoes a move
// the user made meanwhile: it only corrects the folder.
#[test]
fn refiling_keeps_a_session_the_user_moved() {
    let h = H::new();
    let path = h.put(
        &format!(".claude/projects/x/{CLAUDE_SID}.jsonl"),
        claude_lines(&h, 0, 3, false).as_bytes(),
    );
    h.pass();
    let home = h.session("claude", CLAUDE_SID);
    assert_eq!(Path::new(&home.cwd), h.home);
    let mine = h.store.create_project("Mine", None).unwrap();
    let machine = h.store.machine_id().unwrap().unwrap();
    h.store
        .move_session(&home.id, Some(&mine.id), &machine, "test-box")
        .unwrap();
    append(&path, claude_lines(&h, 3, 2, true).as_bytes());
    h.engine.repair_home_filed();
    h.pass();
    let s = h.session("claude", CLAUDE_SID);
    assert_eq!(Path::new(&s.cwd), h.cwd);
    assert_eq!(s.project_id, mine.id);
}

// A subagent whose parent sits in a removed project is filed by its own
// folder (a write into the removed project would be refused and stall the
// transcript), and one whose parent's project was merged follows the merge.
#[test]
fn subagents_of_a_parent_in_a_removed_project_still_ingest() {
    for merged in [false, true] {
        let h = H::new();
        let dir = format!(".claude/projects/C--work-proj/{CLAUDE_SID}");
        h.put(
            &format!("{dir}.jsonl"),
            h.fill(&fixture("claude/session.jsonl")).as_bytes(),
        );
        h.pass();
        let parent = h.session("claude", CLAUDE_SID);
        let target = h.store.create_project("Target", None).unwrap();
        if merged {
            h.store
                .merge_projects(&parent.project_id, &target.id)
                .unwrap();
        } else {
            h.store.delete_project(&parent.project_id, "m").unwrap();
        }
        h.put(
            &format!("{dir}/subagents/agent-a1.jsonl"),
            h.fill(&fixture("claude/agent-a1.jsonl")).as_bytes(),
        );
        h.put(
            &format!("{dir}/subagents/agent-a1.meta.json"),
            fixture("claude/agent-a1.meta.json").as_bytes(),
        );
        h.pass();
        let child = h.session("claude", &format!("{CLAUDE_SID}:agent-a1"));
        if merged {
            assert_eq!(child.project_id, target.id);
        } else {
            assert_ne!(child.project_id, parent.project_id);
            assert!(
                !h.store
                    .get_project(&child.project_id)
                    .unwrap()
                    .unwrap()
                    .deleted
            );
        }
    }
}

// ---------------------------------------------------------------- headless runs

/// The Claude fixture as a `claude -p` run (`entrypoint` `sdk-cli`).
fn claude_print_run(h: &H) -> String {
    h.fill(&fixture("claude/session.jsonl"))
        .replace(r#""entrypoint":"cli""#, r#""entrypoint":"sdk-cli""#)
}

/// A user line of `entrypoint` in turn `uuid` (its `promptId`), with
/// `extra` JSON fields (e.g. an `origin`).
fn claude_line_from(h: &H, uuid: &str, text: &str, entrypoint: &str) -> String {
    claude_line_with(h, uuid, text, entrypoint, "")
}

fn claude_line_with(h: &H, uuid: &str, text: &str, entrypoint: &str, extra: &str) -> String {
    let l = claude_line(uuid, text, h);
    format!(
        "{},\"entrypoint\":\"{entrypoint}\",\"promptId\":\"{uuid}\"{extra}}}\n",
        l.strip_suffix('}').unwrap()
    )
}

fn project_count(h: &H) -> usize {
    let machine = h.store.machine_id().unwrap().unwrap();
    h.store.list_project_summaries(&machine).unwrap().len()
}

#[test]
fn headless_claude_runs_make_no_session_or_project() {
    let h = H::new();
    let projects = project_count(&h);
    let path = h.put(
        &format!(".claude/projects/x/{CLAUDE_SID}.jsonl"),
        claude_print_run(&h).as_bytes(),
    );
    h.pass();
    let none = |h: &H| {
        h.store
            .session_by_agent_id("claude", CLAUDE_SID)
            .unwrap()
            .is_none()
    };
    assert!(none(&h));
    assert_eq!(project_count(&h), projects, "no project for a scripted run");
    // A background task's notification is no prompt of the user.
    append(
        &path,
        claude_line_with(
            &h,
            "n1",
            "<task-notification> <task-id>t1</task-id> done </task-notification>",
            "sdk-cli",
            r#","origin":{"kind":"task-notification"}"#,
        )
        .as_bytes(),
    );
    h.pass();
    assert!(none(&h));

    // A second prompt (an Agent SDK app someone chats in, `-p --resume`):
    // the whole transcript becomes a session, the part read while it
    // looked scripted included.
    append(
        &path,
        claude_line_from(&h, "p2", "and now the tests", "sdk-cli").as_bytes(),
    );
    h.pass();
    h.pass();
    let s = h.session("claude", CLAUDE_SID);
    let ev = h.events(&s);
    let texts: Vec<&str> = ev.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(texts.len(), 11, "{texts:?}");
    assert!(texts[0].starts_with("Add a greeting helper"), "{texts:?}");
    assert_eq!(ev[9].kind, EventKind::System, "{texts:?}");
    assert_eq!(
        (ev[10].kind, ev[10].text.as_str()),
        (EventKind::User, "and now the tests")
    );
}

/// Subagents inherit the parent's `sdk-*` entrypoint and have one prompt:
/// they follow their parent, whichever file is read first.
#[test]
fn claude_subagents_follow_their_parent() {
    let h = H::new();
    let sub = |sid: &str| {
        h.fill(&fixture("claude/agent-a1.jsonl"))
            .replace(r#""entrypoint":"cli""#, r#""entrypoint":"sdk-cli""#)
            .replace(CLAUDE_SID, sid)
    };
    // An Agent SDK app someone chats in: its subagent is written once during
    // the first prompt, while the run still looks scripted, and never again.
    let chat = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let main = h.put(
        &format!(".claude/projects/y/{chat}.jsonl"),
        claude_print_run(&h).replace(CLAUDE_SID, chat).as_bytes(),
    );
    h.put(
        &format!(".claude/projects/y/{chat}/subagents/agent-a1.jsonl"),
        sub(chat).as_bytes(),
    );
    h.pass();
    let sub_asid = format!("{chat}:agent-a1");
    assert!(
        h.store
            .session_by_agent_id("claude", &sub_asid)
            .unwrap()
            .is_none()
    );
    // A second prompt makes it a session; the skipped subagent follows.
    append(
        &main,
        claude_line_from(&h, "p2", "and the docs", "sdk-cli").as_bytes(),
    );
    h.pass();
    h.pass();
    let parent = h.session("claude", chat);
    let child = h.session("claude", &sub_asid);
    assert_eq!(child.parent_session_id.as_deref(), Some(parent.id.as_str()));
    let child_events = h.events(&child).len();
    assert!(child_events > 0);

    // A subagent read before its session's transcript existed.
    let first = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    h.put(
        &format!(".claude/projects/y/{first}/subagents/agent-a1.jsonl"),
        sub(first).as_bytes(),
    );
    h.pass();
    h.put(
        &format!(".claude/projects/y/{first}.jsonl"),
        format!(
            "{}{}",
            claude_print_run(&h).replace(CLAUDE_SID, first),
            claude_line_from(&h, "p2", "and the docs", "sdk-cli")
        )
        .as_bytes(),
    );
    h.pass();
    h.pass();
    let s = h.session("claude", &format!("{first}:agent-a1"));
    assert_eq!(h.events(&s).len(), child_events);

    // Read after its stored parent: kept at once.
    let later = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    h.put(
        &format!(".claude/projects/y/{later}.jsonl"),
        format!(
            "{}{}",
            claude_print_run(&h).replace(CLAUDE_SID, later),
            claude_line_from(&h, "p2", "and the docs", "sdk-cli")
        )
        .as_bytes(),
    );
    h.pass();
    h.put(
        &format!(".claude/projects/y/{later}/subagents/agent-a1.jsonl"),
        sub(later).as_bytes(),
    );
    h.pass();
    let s = h.session("claude", &format!("{later}:agent-a1"));
    // The same full history as a subagent never skipped.
    assert_eq!(h.events(&s).len(), child_events);

    // A scripted run's subagent is skipped with it.
    let bot = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    h.put(
        &format!(".claude/projects/y/{bot}.jsonl"),
        claude_print_run(&h).replace(CLAUDE_SID, bot).as_bytes(),
    );
    h.put(
        &format!(".claude/projects/y/{bot}/subagents/agent-a1.jsonl"),
        sub(bot).as_bytes(),
    );
    h.pass();
    for asid in [bot.to_string(), format!("{bot}:agent-a1")] {
        assert!(
            h.store
                .session_by_agent_id("claude", &asid)
                .unwrap()
                .is_none(),
            "{asid}"
        );
    }
}

#[test]
fn claude_scripted_run_resumed_interactively_becomes_a_session() {
    let h = H::new();
    let path = h.put(
        &format!(".claude/projects/x/{CLAUDE_SID}.jsonl"),
        claude_print_run(&h).as_bytes(),
    );
    h.pass();
    append(
        &path,
        claude_line_from(&h, "u1", "take a look yourself", "cli").as_bytes(),
    );
    h.pass();
    h.pass();
    let s = h.session("claude", CLAUDE_SID);
    assert_eq!(h.events(&s).len(), 10);
}

#[test]
fn older_claude_transcripts_without_entrypoint_are_interactive() {
    let h = H::new();
    let old = h
        .fill(&fixture("claude/session.jsonl"))
        .replace(r#","entrypoint":"cli""#, "");
    assert!(!old.contains("entrypoint"));
    let path = h.put(
        &format!(".claude/projects/x/{CLAUDE_SID}.jsonl"),
        old.as_bytes(),
    );
    h.pass();
    let s = h.session("claude", CLAUDE_SID);
    // Continued with `claude -p --resume`: still the same session.
    append(
        &path,
        claude_line_from(&h, "p1", "one more thing", "sdk-cli").as_bytes(),
    );
    h.pass();
    let again = h.session("claude", CLAUDE_SID);
    assert_eq!(again.id, s.id);
    assert_eq!(h.events(&again).len(), 10);
}

#[test]
fn claude_harness_lines_are_system_events() {
    let h = H::new();
    let path = put_claude(&h);
    h.pass();
    for (id, text, extra) in [
        (
            "n1",
            "<task-notification> <task-id>t1</task-id> </task-notification>",
            r#","origin":{"kind":"task-notification"}"#,
        ),
        (
            "c1",
            "The coordinator sent a message while you were working",
            r#","origin":{"kind":"coordinator"},"isMeta":true"#,
        ),
        (
            "n2",
            "<task-notification> old format </task-notification>",
            "",
        ),
        ("h1", "a typed prompt", r#","origin":{"kind":"human"}"#),
    ] {
        append(
            &path,
            claude_line_with(&h, id, text, "cli", extra).as_bytes(),
        );
    }
    h.pass();
    let s = h.session("claude", CLAUDE_SID);
    let kinds: Vec<EventKind> = h.events(&s).iter().rev().take(4).map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        [
            EventKind::User,
            EventKind::System,
            EventKind::System,
            EventKind::System
        ]
    );
}

#[test]
fn interactive_claude_sessions_scripted_later_are_kept() {
    let h = H::new();
    let path = put_claude(&h);
    h.pass();
    let s = h.session("claude", CLAUDE_SID);
    assert_eq!(h.events(&s).len(), 9);
    append(
        &path,
        claude_line_from(&h, "p1", "scripted follow-up", "sdk-cli").as_bytes(),
    );
    h.pass();
    let again = h.session("claude", CLAUDE_SID);
    assert_eq!(again.id, s.id);
    assert_eq!(h.events(&again).len(), 10);
    // Its interactive subagent is a session too.
    h.session("claude", &format!("{CLAUDE_SID}:agent-a1"));
}

#[test]
fn headless_runs_blirp_launched_or_hook_created() {
    let h = H::new();
    let machine = h.store.machine_id().unwrap().unwrap();
    let project = h.store.resolve_project("m", "box", &h.cwd).unwrap().project;
    let row = |id: &str, asid: &str, origin: SessionOrigin| Session {
        id: id.into(),
        project_id: project.id.clone(),
        machine_id: machine.clone(),
        agent: "claude".into(),
        agent_session_id: Some(asid.into()),
        origin,
        cwd: h.cwd.display().to_string(),
        title: None,
        status: SessionStatus::Idle,
        branch: None,
        worktree: None,
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
        title_updated_at: 0,
        project_updated_at: 0,
        compacted_at: None,
        context_near_full_at: None,
    };
    // A blirp launch whose transcript says headless keeps its session.
    h.store
        .insert_session(&row("launched", CLAUDE_SID, SessionOrigin::Blirp))
        .unwrap();
    h.put(
        &format!(".claude/projects/x/{CLAUDE_SID}.jsonl"),
        claude_print_run(&h).as_bytes(),
    );
    // A row a global hook created for another scripted run is dropped once
    // its transcript is read.
    let other = "77777777-7777-4777-8777-777777777777";
    h.store
        .insert_session(&row("hooked", other, SessionOrigin::External))
        .unwrap();
    h.put(
        &format!(".claude/projects/x/{other}.jsonl"),
        claude_print_run(&h).replace(CLAUDE_SID, other).as_bytes(),
    );
    // One whose hook made a project for its folder takes that project along.
    let bot_dir = h.root.join("work").join("hookbot");
    std::fs::create_dir_all(&bot_dir).unwrap();
    std::fs::write(bot_dir.join("package.json"), "{}").unwrap();
    let bot_project = h
        .store
        .resolve_project_with(&machine, "test-box", &bot_dir, &NonProjectDirs::default())
        .unwrap()
        .project;
    let lone = "99999999-9999-4999-8999-999999999999";
    h.store
        .insert_session(&Session {
            project_id: bot_project.id.clone(),
            ..row("hooked-lone", lone, SessionOrigin::External)
        })
        .unwrap();
    h.put(
        &format!(".claude/projects/x/{lone}.jsonl"),
        claude_print_run(&h).replace(CLAUDE_SID, lone).as_bytes(),
    );
    // A row that already has history is never dropped by a read (only the
    // one-time cleanup, which reads the whole transcript, removes rows).
    let kept = "88888888-8888-4888-8888-888888888888";
    h.store
        .insert_session(&row("history", kept, SessionOrigin::External))
        .unwrap();
    h.store
        .insert_event(Event {
            session_id: "history".into(),
            seq: 0,
            ts: 1,
            kind: EventKind::User,
            text: "earlier work".into(),
            meta: None,
        })
        .unwrap();
    h.put(
        &format!(".claude/projects/x/{kept}.jsonl"),
        claude_print_run(&h).replace(CLAUDE_SID, kept).as_bytes(),
    );
    h.pass();
    assert!(h.store.get_session("history").unwrap().is_some());
    let s = h.session("claude", CLAUDE_SID);
    assert_eq!(s.id, "launched");
    assert_eq!(h.events(&s).len(), 9);
    assert!(h.store.get_session("hooked").unwrap().is_none());
    assert!(h.store.get_session("hooked-lone").unwrap().is_none());
    assert!(
        h.store
            .get_project(&bot_project.id)
            .unwrap()
            .unwrap()
            .deleted
    );
    assert!(!h.store.get_project(&project.id).unwrap().unwrap().deleted);
    assert!(
        h.store
            .session_by_agent_id("claude", other)
            .unwrap()
            .is_none()
    );
    // Later hooks of that run find it known headless and create no row.
    let key = |asid: &str| {
        h.home
            .join(".claude")
            .join("projects")
            .join("x")
            .join(format!("{asid}.jsonl"))
            .to_string_lossy()
            .into_owned()
    };
    assert!(blirp::ingest::known_headless(
        &h.store,
        "claude",
        &key(other)
    ));
    put_claude(&h);
    h.pass();
    let interactive = h
        .home
        .join(".claude")
        .join("projects")
        .join("C--work-proj")
        .join(format!("{CLAUDE_SID}.jsonl"));
    assert!(!blirp::ingest::known_headless(
        &h.store,
        "claude",
        &interactive.to_string_lossy()
    ));
    assert!(!blirp::ingest::known_headless(
        &h.store,
        "claude",
        "never read"
    ));
}

#[test]
fn codex_exec_runs_make_no_session() {
    let h = H::new();
    let projects = project_count(&h);
    let exec = h
        .fill(&fixture("codex/rollout.jsonl"))
        .replace(
            r#""originator":"codex_cli_rs""#,
            r#""originator":"codex_exec""#,
        )
        .replace(r#""source":"cli""#, r#""source":"exec""#);
    assert!(exec.contains(r#""source":"exec""#));
    h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T09-00-00-{CODEX_SID}.jsonl"),
        exec.as_bytes(),
    );
    h.pass();
    assert!(
        h.store
            .session_by_agent_id("codex", CODEX_SID)
            .unwrap()
            .is_none()
    );
    assert_eq!(project_count(&h), projects);

    // Resumed (codex writes no new `session_meta`): a second turn makes it
    // a conversation, stored with everything read before.
    let path = h
        .home
        .join(".codex/sessions/2026/01/02")
        .join(format!("rollout-2026-01-02T09-00-00-{CODEX_SID}.jsonl"));
    append(
        &path,
        format!(
            "{}\n{}",
            r#"{"timestamp":"2026-01-02T09:04:59.000Z","type":"turn_context","payload":{"turn_id":"t2","model":"gpt-5-codex"}}"#,
            fixture("codex/append.jsonl")
        )
        .as_bytes(),
    );
    h.pass();
    h.pass();
    let s = h.session("codex", CODEX_SID);
    let texts: Vec<String> = h.events(&s).into_iter().map(|e| e.text).collect();
    assert!(texts[1].contains("Rename the build script"), "{texts:?}");
    assert_eq!(texts.last().unwrap(), "Also update the README");
}

// A Codex subagent of a `codex exec` run is part of that scripted run: no
// session of its own (it would sit without a parent). Once the run turns
// into a conversation, the subagent is read again and hangs under it.
#[test]
fn codex_subagents_of_a_scripted_run_go_with_it() {
    let h = H::new();
    let fork = "0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4f0f";
    let exec = h
        .fill(&fixture("codex/rollout.jsonl"))
        .replace(r#""source":"cli""#, r#""source":"exec""#);
    assert!(exec.contains(r#""source":"exec""#));
    let parent = h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T09-00-00-{CODEX_SID}.jsonl"),
        exec.as_bytes(),
    );
    // Read before its parent in the pass or after: the parent's rollout
    // decides.
    h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T10-00-00-{fork}.jsonl"),
        h.fill(&fixture("codex/fork.jsonl")).as_bytes(),
    );
    h.pass();
    let none = |id: &str| h.store.session_by_agent_id("codex", id).unwrap().is_none();
    assert!(none(CODEX_SID) && none(fork));

    append(
        &parent,
        format!(
            "{}\n{}",
            r#"{"timestamp":"2026-01-02T09:04:59.000Z","type":"turn_context","payload":{"turn_id":"t2","model":"gpt-5-codex"}}"#,
            fixture("codex/append.jsonl")
        )
        .as_bytes(),
    );
    h.pass();
    h.pass();
    let p = h.session("codex", CODEX_SID);
    let s = h.session("codex", fork);
    assert_eq!(s.parent_session_id.as_deref(), Some(p.id.as_str()));
}

// Earlier builds stored such a subagent without a parent: the one-time
// cleanup removes it with the scripted run.
#[test]
fn stored_codex_subagents_of_a_scripted_run_are_removed_once() {
    let h = H::new();
    let fork = "0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4f0f";
    let sub = h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T10-00-00-{fork}.jsonl"),
        h.fill(&fixture("codex/fork.jsonl")).as_bytes(),
    );
    let machine = h.store.machine_id().unwrap().unwrap();
    let project = h
        .store
        .resolve_project_with(&machine, "test-box", &h.cwd, &NonProjectDirs::default())
        .unwrap()
        .project;
    h.store
        .insert_session(&Session {
            id: "sub".into(),
            project_id: project.id,
            machine_id: machine,
            agent: "codex".into(),
            agent_session_id: Some(fork.into()),
            origin: SessionOrigin::External,
            cwd: h.cwd.display().to_string(),
            title: None,
            status: SessionStatus::Completed,
            branch: None,
            worktree: None,
            transcript_path: Some(sub.display().to_string()),
            started_at: 1_000,
            ended_at: Some(2_000),
            last_activity_at: 2_000,
            exit_code: None,
            summary: None,
            distilled_through_seq: 0,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            parent_session_id: None,
            stopped_by_user: false,
            title_updated_at: 0,
            project_updated_at: 0,
            compacted_at: None,
            context_near_full_at: None,
        })
        .unwrap();
    // The parent's rollout records a `codex exec` run.
    let exec = h
        .fill(&fixture("codex/rollout.jsonl"))
        .replace(r#""source":"cli""#, r#""source":"exec""#);
    h.put(
        &format!(".codex/sessions/2026/01/02/rollout-2026-01-02T09-00-00-{CODEX_SID}.jsonl"),
        exec.as_bytes(),
    );
    h.engine.remove_headless();
    assert!(
        h.store
            .session_by_agent_id("codex", fork)
            .unwrap()
            .is_none()
    );
}

/// `fixture` with its placeholders filled for a transcript run in `dir`.
fn fixture_in(name: &str, dir: &Path) -> String {
    let json = serde_json::to_string(&dir.to_string_lossy()).unwrap();
    fixture(name)
        .replace("{{CWD}}", &json[1..json.len() - 1])
        .replace("{{SECRET}}", "x")
}

/// Sessions of scripted runs stored by an earlier version are removed once,
/// like a user delete, with the distiller's records from them and the
/// projects ingest made only for them; nothing else is touched.
#[test]
fn ingested_headless_runs_are_removed_once() {
    use blirp_core::model::{Record, RecordKind, RecordStatus};
    use blirp_core::store::Change;

    let h = H::new();
    let machine = h.store.machine_id().unwrap().unwrap();
    let folder = |name: &str| {
        let d = h.root.join("work").join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("package.json"), "{}").unwrap();
        dunce::canonicalize(d).unwrap()
    };
    let auto_project = |dir: &Path| {
        h.store
            .resolve_project_with(&machine, "test-box", dir, &NonProjectDirs::default())
            .unwrap()
            .project
    };
    let print = |dir: &Path| {
        fixture_in("claude/session.jsonl", dir)
            .replace(r#""entrypoint":"cli""#, r#""entrypoint":"sdk-cli""#)
    };
    let row = |id: &str, agent: &str, pid: &str, origin: SessionOrigin, t: Option<&Path>| {
        let s = Session {
            id: id.into(),
            project_id: pid.into(),
            machine_id: machine.clone(),
            agent: agent.into(),
            agent_session_id: Some(format!("asid-{id}")),
            origin,
            cwd: h.cwd.display().to_string(),
            title: None,
            status: SessionStatus::Completed,
            branch: None,
            worktree: None,
            transcript_path: t.map(|t| t.display().to_string()),
            started_at: 1_000,
            ended_at: Some(2_000),
            last_activity_at: 2_000,
            exit_code: None,
            summary: None,
            distilled_through_seq: 0,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            parent_session_id: None,
            stopped_by_user: false,
            title_updated_at: 0,
            project_updated_at: 0,
            compacted_at: None,
            context_near_full_at: None,
        };
        h.store.insert_session(&s).unwrap();
        s
    };
    let record = |id: &str, pid: &str, source: &str, by: &str, pinned: bool| {
        let r = Record {
            id: id.into(),
            project_id: pid.into(),
            kind: RecordKind::Decision,
            title: format!("title {id}"),
            body: String::new(),
            status: RecordStatus::Active,
            pinned,
            source_session_id: Some(source.into()),
            created_at: 1,
            updated_at: 1,
            updated_by: by.into(),
        };
        h.store.apply(Change::Record(r)).unwrap();
    };

    // An interactive project with a real session, a bot run filed into it,
    // and a blirp launch whose transcript is scripted.
    let proj = auto_project(&h.cwd);
    let interactive = put_claude(&h);
    row(
        "human",
        "claude",
        &proj.id,
        SessionOrigin::External,
        Some(&interactive),
    );
    let bot_in_proj = h.put(".claude/projects/p/bot1.jsonl", print(&h.cwd).as_bytes());
    row(
        "bot1",
        "claude",
        &proj.id,
        SessionOrigin::External,
        Some(&bot_in_proj),
    );
    record("r-bot-distiller", &proj.id, "bot1", "distiller", false);
    record("r-bot-user", &proj.id, "bot1", "user", false);
    record("r-bot-pinned", &proj.id, "bot1", "distiller", true);
    record("r-human", &proj.id, "human", "distiller", false);
    // A subagent of the kept session looks scripted on its own (it inherits
    // an sdk entrypoint from `-p --resume` and has one prompt): it stays.
    let mut human_sub = row(
        "human-sub",
        "claude",
        &proj.id,
        SessionOrigin::External,
        Some(&bot_in_proj),
    );
    human_sub.parent_session_id = Some("human".into());
    h.store.apply(Change::Session(human_sub)).unwrap();
    row(
        "launched",
        "claude",
        &proj.id,
        SessionOrigin::Blirp,
        Some(&bot_in_proj),
    );

    // A project ingest made only for bot runs (with a subagent and a
    // codex exec run) is removed with them.
    let bots_dir = folder("bots");
    let bots = auto_project(&bots_dir);
    let bot2 = h.put(".claude/projects/b/bot2.jsonl", print(&bots_dir).as_bytes());
    row(
        "bot2",
        "claude",
        &bots.id,
        SessionOrigin::External,
        Some(&bot2),
    );
    let mut sub = row(
        "bot2-sub",
        "claude",
        &bots.id,
        SessionOrigin::External,
        None,
    );
    sub.parent_session_id = Some("bot2".into());
    h.store.apply(Change::Session(sub)).unwrap();
    let exec = h.put(
        ".codex/sessions/2026/01/02/rollout-2026-01-02T09-00-00-exec.jsonl",
        fixture_in("codex/rollout.jsonl", &bots_dir)
            .replace(r#""source":"cli""#, r#""source":"exec""#)
            .as_bytes(),
    );
    row(
        "exec",
        "codex",
        &bots.id,
        SessionOrigin::External,
        Some(&exec),
    );
    record("r-bots", &bots.id, "bot2", "distiller", false);

    // A project the user registered keeps existing without its bot run.
    let mine_dir = folder("mine");
    let mine = h.store.register_project(&machine, &mine_dir, None).unwrap();
    let bot3 = h.put(".claude/projects/m/bot3.jsonl", print(&mine_dir).as_bytes());
    row(
        "bot3",
        "claude",
        &mine.id,
        SessionOrigin::External,
        Some(&bot3),
    );
    // A row whose transcript is gone cannot be judged and stays.
    row(
        "gone",
        "claude",
        &mine.id,
        SessionOrigin::External,
        Some(&h.home.join("missing.jsonl")),
    );
    // "gone" was distilled after a bot run's record existed: it may have
    // reached the same record (the distiller adds no duplicate), so it stays.
    record("r-bot3-shared", &mine.id, "bot3", "distiller", false);
    h.store
        .modify_session("gone", |s| {
            s.summary = Some(json!({"summary": "s", "distilled_at": 5, "through_seq": 1}));
        })
        .unwrap();
    // A `codex exec` run resumed for a second turn is a conversation.
    let exec2 = h.put(
        ".codex/sessions/2026/01/02/rollout-2026-01-02T09-00-00-exec2.jsonl",
        format!(
            "{}{}\n",
            fixture_in("codex/rollout.jsonl", &mine_dir).replace(r#""source":"cli""#, r#""source":"exec""#),
            r#"{"timestamp":"2026-01-02T09:04:59.000Z","type":"turn_context","payload":{"turn_id":"t2"}}"#
        )
        .as_bytes(),
    );
    row(
        "exec2",
        "codex",
        &mine.id,
        SessionOrigin::External,
        Some(&exec2),
    );
    // Another machine's session is that machine's to judge.
    h.store
        .upsert_machine(&Machine {
            id: "other".into(),
            name: "laptop".into(),
            os: "linux".into(),
            role: MachineRole::Node,
            last_seen: 1,
            revoked: false,
        })
        .unwrap();
    let mut foreign = row(
        "foreign",
        "claude",
        &mine.id,
        SessionOrigin::External,
        Some(&bot3),
    );
    foreign.machine_id = "other".into();
    h.store.apply(Change::Session(foreign)).unwrap();

    let outbox = h.outbox_len();
    h.engine.remove_headless();
    for id in ["exec2", "foreign"] {
        assert!(h.store.get_session(id).unwrap().is_some(), "{id} kept");
    }
    assert!(
        h.store
            .list_records(&mine.id, &blirp_core::store::RecordFilter::default())
            .unwrap()
            .iter()
            .any(|r| r.id == "r-bot3-shared" && r.source_session_id.is_none())
    );
    assert!(
        !h.store
            .outbox_after(0, 1_000_000)
            .unwrap()
            .into_iter()
            .skip(outbox)
            .any(|e| e.key == "foreign"),
        "nothing queued for another machine's session"
    );

    let exists = |id: &str| h.store.get_session(id).unwrap().is_some();
    for id in ["bot1", "bot2", "bot2-sub", "exec", "bot3"] {
        assert!(!exists(id), "{id} removed");
    }
    for id in ["human", "human-sub", "launched", "gone"] {
        assert!(exists(id), "{id} kept");
    }
    let records: Vec<String> = h
        .store
        .list_records(&proj.id, &blirp_core::store::RecordFilter::default())
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    let mut records = records;
    records.sort();
    assert_eq!(records, ["r-bot-pinned", "r-bot-user", "r-human"]);
    assert!(h.store.get_project(&bots.id).unwrap().unwrap().deleted);
    assert!(!h.store.get_project(&proj.id).unwrap().unwrap().deleted);
    assert!(!h.store.get_project(&mine.id).unwrap().unwrap().deleted);
    // Replicated like a user delete.
    let deletes: Vec<_> = h
        .store
        .outbox_after(0, 1_000_000)
        .unwrap()
        .into_iter()
        .skip(outbox)
        .filter(|e| e.op == "delete")
        .map(|e| (e.entity, e.key))
        .collect();
    for id in ["bot1", "bot2", "exec", "bot3"] {
        assert!(
            deletes.contains(&("sessions".into(), id.into())),
            "{id}: {deletes:?}"
        );
    }
    assert!(deletes.contains(&("records".into(), "r-bots".into())));

    // Their transcripts are read again from the start and stay out.
    h.pass();
    h.pass();
    for asid in ["bot1", "bot2", "bot3"] {
        assert!(
            h.store
                .session_by_agent_id("claude", asid)
                .unwrap()
                .is_none(),
            "{asid} came back"
        );
    }
    // Idempotent, and it runs once.
    let again = h
        .store
        .remove_headless_sessions(&machine, &["bot2".into(), "exec".into()])
        .unwrap();
    assert_eq!(again, blirp_core::store::HeadlessCleanup::default());
    row(
        "later",
        "claude",
        &mine.id,
        SessionOrigin::External,
        Some(&bot3),
    );
    h.engine.remove_headless();
    assert!(exists("later"));
}

#[test]
fn the_harness_leaves_no_folder_behind() {
    let h = H::new();
    let rel = format!(".claude/projects/C--work-proj/{CLAUDE_SID}.jsonl");
    let line = claude_line("u-1", "first prompt", &h);
    h.put(&rel, format!("{line}\n").as_bytes());
    h.pass();
    let root = h.root.clone();
    drop(h);
    // Windows refuses to remove a folder with open handles, and TempDir
    // ignores that: every run leaked one.
    assert!(!root.exists(), "{} left behind", root.display());
}
