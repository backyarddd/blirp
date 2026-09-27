//! Memory injection (§9 Inject) and handoff pack rendering.
//!
//! Output is deterministic for a given memory state (stable ordering, no
//! timestamps of "now"), so agent prompt caches keep hitting.

use blirp_core::model::{
    Event, EventKind, Record, RecordKind, RecordStatus, Session, SessionSummary,
};
use blirp_core::store::{RecordFilter, Store, StoreError};
use std::collections::HashMap;

pub const MAX_PINNED: usize = 8;
pub const MAX_OPEN_THREADS: usize = 10;
pub const MAX_PLANS: usize = 5;
pub const MAX_DECISIONS: usize = 8;
pub const MAX_GOTCHAS: usize = 5;
pub const MAX_SESSIONS: usize = 3;
/// Longest single list item; longer bodies are cut with an ellipsis.
const MAX_ITEM_CHARS: usize = 400;
pub const HANDOFF_MAX_CHARS: usize = 12_000;
pub const HANDOFF_TURNS: i64 = 12;
const MAX_TURN_CHARS: usize = 1_500;

pub const TOOLS_LINE: &str = "Tools: search older history with the blirp MCP tools (mem_search, mem_session, mem_recent, mem_record) or `blirp mem search \"<query>\"`.";

/// Truncate to at most `max` chars, ending with an ellipsis when cut.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Collapse whitespace (newlines included) to single spaces.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// UTC `YYYY-MM-DD` of a unix-ms timestamp.
pub fn date(ms: i64) -> String {
    // Civil-from-days (Howard Hinnant), valid for the whole i64 day range we use.
    let z = ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn record_line(r: &Record) -> String {
    let body = one_line(&r.body);
    let text = if body.is_empty() {
        format!("- {}", one_line(&r.title))
    } else {
        format!("- **{}**: {body}", one_line(&r.title))
    };
    clip(&text, MAX_ITEM_CHARS)
}

/// A pinned record of any kind, labelled with its kind.
fn pinned_line(r: &Record) -> String {
    let label = r.kind.as_str().replace('_', " ");
    let line = record_line(r);
    let rest = line.strip_prefix("- ").unwrap_or(&line);
    clip(&format!("- [{label}] {rest}"), MAX_ITEM_CHARS)
}

pub fn parse_summary(s: &Session) -> Option<SessionSummary> {
    s.summary
        .as_ref()
        .and_then(|v| serde_json::from_value::<SessionSummary>(v.clone()).ok())
}

fn session_line(s: &Session, machines: &HashMap<String, String>) -> String {
    let summary = parse_summary(s).and_then(|x| x.summary).unwrap_or_default();
    let title = s
        .title
        .as_deref()
        .map(one_line)
        .unwrap_or_else(|| "(untitled)".into());
    let machine = machines.get(&s.machine_id).map_or("?", String::as_str);
    let mut line = format!(
        "- {} · {} · {} · {}",
        date(s.started_at),
        s.agent,
        machine,
        title
    );
    if !summary.trim().is_empty() {
        line.push_str(": ");
        line.push_str(&one_line(&summary));
    }
    clip(&line, MAX_ITEM_CHARS + 200)
}

/// Adds blocks in priority order while they fit the budget.
struct Budget {
    out: String,
    left: usize,
}

impl Budget {
    fn push(&mut self, block: &str) -> bool {
        let n = block.chars().count() + 1;
        if n > self.left {
            return false;
        }
        self.out.push_str(block);
        self.out.push('\n');
        self.left -= n;
        true
    }

    /// A heading plus as many lines as fit; nothing when no line fits.
    fn section(&mut self, heading: &str, lines: &[String]) {
        let Some(first) = lines.first() else { return };
        if heading.chars().count() + first.chars().count() + 3 > self.left {
            return;
        }
        self.push("");
        self.push(heading);
        for l in lines {
            if !self.push(l) {
                break;
            }
        }
    }
}

/// Active records of a project in the store's stable order (pinned first,
/// then most recently updated), ties broken by id.
fn active_records(store: &Store, project_id: &str) -> Result<Vec<Record>, StoreError> {
    let mut v = store.list_records(
        project_id,
        &RecordFilter {
            status: Some(RecordStatus::Active),
            kind: None,
        },
    )?;
    v.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then(b.updated_at.cmp(&a.updated_at))
            .then(a.id.cmp(&b.id))
    });
    Ok(v)
}

fn machine_names(store: &Store) -> Result<HashMap<String, String>, StoreError> {
    Ok(store
        .list_machines()?
        .into_iter()
        .map(|m| (m.id, m.name))
        .collect())
}

/// §9 `render_injection`: project memory as markdown, at most `max_chars`
/// characters. `exclude_session` (the session being started) is left out of
/// "Recent sessions".
pub fn render_injection(
    store: &Store,
    project_id: &str,
    exclude_session: Option<&str>,
    max_chars: usize,
) -> Result<String, StoreError> {
    let project = store.live_project(project_id)?;
    // Chats are unrelated conversations: nothing to remember across them.
    if project.chats {
        return Ok(String::new());
    }
    let brief = store.get_brief(project_id)?;
    let records = active_records(store, project_id)?;
    let machines = machine_names(store)?;
    let sessions: Vec<Session> = store
        .recent_sessions(project_id, 30)?
        .into_iter()
        .filter(|s| Some(s.id.as_str()) != exclude_session)
        .filter(|s| s.title.is_some() || parse_summary(s).and_then(|x| x.summary).is_some())
        .take(MAX_SESSIONS)
        .collect();

    // Pinned records of any kind (notes included) come first; a record shown
    // there is not repeated in its kind's section.
    let pinned: Vec<&Record> = records
        .iter()
        .filter(|r| r.pinned)
        .take(MAX_PINNED)
        .collect();
    let pinned_lines: Vec<String> = pinned.iter().map(|r| pinned_line(r)).collect();
    let lines = |kind: RecordKind, max: usize| -> Vec<String> {
        records
            .iter()
            .filter(|r| r.kind == kind && !pinned.iter().any(|p| p.id == r.id))
            .take(max)
            .map(record_line)
            .collect()
    };
    let threads = lines(RecordKind::OpenThread, MAX_OPEN_THREADS);
    let plans = lines(RecordKind::Plan, MAX_PLANS);
    let decisions = lines(RecordKind::Decision, MAX_DECISIONS);
    let gotchas = lines(RecordKind::Gotcha, MAX_GOTCHAS);
    let session_lines: Vec<String> = sessions
        .iter()
        .map(|s| session_line(s, &machines))
        .collect();

    let header = clip(&format!("# blirp memory: {}", one_line(&project.name)), 200);
    let footer_len = TOOLS_LINE.chars().count() + 2;
    let mut b = Budget {
        out: String::new(),
        left: max_chars.saturating_sub(footer_len),
    };
    b.push(&header);
    let brief_text = brief
        .as_ref()
        .map(|b| b.body_md.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "_No project brief yet._".into());
    // The brief may use at most half the budget so the lists always get room.
    let brief_cap = (b.left / 2).max(100);
    b.push(&clip(&brief_text, brief_cap));
    b.section("## Pinned", &pinned_lines);
    b.section("## Open threads", &threads);
    b.section("## Active plans", &plans);
    b.section("## Recent decisions", &decisions);
    b.section("## Gotchas", &gotchas);
    b.section("## Recent sessions", &session_lines);
    let mut out = b.out;
    out.push('\n');
    out.push_str(TOOLS_LINE);
    out.push('\n');
    Ok(clip(&out, max_chars))
}

/// Paths touched in a session: the distilled file list plus `file_edit` events.
fn files_touched(summary: Option<&SessionSummary>, events: &[Event]) -> Vec<String> {
    let mut files: Vec<String> = summary.map(|s| s.files.clone()).unwrap_or_default();
    for e in events.iter().filter(|e| e.kind == EventKind::FileEdit) {
        let meta = e.meta.as_ref();
        let mut found: Vec<String> = ["path", "file_path", "file"]
            .iter()
            .filter_map(|k| meta.and_then(|m| m.get(*k)).and_then(|v| v.as_str()))
            .map(str::to_string)
            .collect();
        if let Some(arr) = meta.and_then(|m| m.get("paths")).and_then(|v| v.as_array()) {
            found.extend(arr.iter().filter_map(|v| v.as_str().map(str::to_string)));
        }
        if found.is_empty()
            && let Some(first) = e.text.lines().next()
        {
            found.push(first.trim().to_string());
        }
        files.extend(found);
    }
    let mut seen = std::collections::HashSet::new();
    files.retain(|f| !f.is_empty() && seen.insert(f.clone()));
    files.truncate(50);
    files
}

/// §9 handoff pack for continuing `source` in a new session: brief, summary,
/// last turns, files touched and open threads, capped at `HANDOFF_MAX_CHARS`.
pub fn render_handoff(store: &Store, source: &Session) -> Result<String, StoreError> {
    let brief = store.get_brief(&source.project_id)?;
    let summary = parse_summary(source);
    let turns = store.last_events(&source.id, &["user", "assistant"], HANDOFF_TURNS)?;
    let edits = store.last_events(&source.id, &["file_edit"], 500)?;
    let threads: Vec<String> = active_records(store, &source.project_id)?
        .iter()
        .filter(|r| r.kind == RecordKind::OpenThread)
        .take(MAX_OPEN_THREADS)
        .map(record_line)
        .collect();
    let files = files_touched(summary.as_ref(), &edits);

    let title = source.title.as_deref().unwrap_or("(untitled)");
    let mut head = format!(
        "# blirp handoff: continue session \"{}\" ({}, {}, {})\n",
        one_line(title),
        source.agent,
        date(source.started_at),
        source.cwd
    );
    if let Some(text) = summary.as_ref().and_then(|s| s.summary.as_deref()) {
        head.push_str("\n## Summary\n");
        head.push_str(text.trim());
        head.push('\n');
    }
    let mut tail = String::new();
    if !files.is_empty() {
        tail.push_str("\n## Files touched\n");
        for f in &files {
            tail.push_str(&format!("- {f}\n"));
        }
    }
    if !threads.is_empty() {
        tail.push_str("\n## Open threads\n");
        for t in &threads {
            tail.push_str(t);
            tail.push('\n');
        }
    }
    let turn_blocks: Vec<String> = turns
        .iter()
        .map(|e| {
            let who = if e.kind == EventKind::User {
                "User"
            } else {
                "Assistant"
            };
            format!("**{who}:** {}\n", clip(e.text.trim(), MAX_TURN_CHARS))
        })
        .collect();

    // Budget: summary/head and tail are kept; oldest turns go first, then the brief.
    let fixed = head.chars().count() + tail.chars().count() + 40;
    let mut left = HANDOFF_MAX_CHARS.saturating_sub(fixed);
    let mut kept: Vec<&String> = Vec::new();
    for t in turn_blocks.iter().rev() {
        let n = t.chars().count() + 1;
        if n > left {
            break;
        }
        left -= n;
        kept.push(t);
    }
    kept.reverse();
    let mut out = head;
    if let Some(b) = brief
        .as_ref()
        .map(|b| b.body_md.trim())
        .filter(|b| !b.is_empty())
        && left > 200
    {
        out.push_str("\n## Brief\n");
        out.push_str(&clip(b, left - 20));
        out.push('\n');
    }
    if !kept.is_empty() {
        out.push_str("\n## Last turns\n");
        for t in kept {
            out.push_str(t);
        }
    }
    out.push_str(&tail);
    Ok(clip(&out, HANDOFF_MAX_CHARS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::testutil::*;

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_758_758_400_000), "2025-09-25");
        assert_eq!(date(951_782_400_000), "2000-02-29");
    }

    #[test]
    fn golden_injection_is_stable_and_ordered() {
        let (_d, store, pid) = project_store("Demo");
        store
            .put_brief(&pid, "Demo is a CLI.\nUse cargo.", "user")
            .unwrap();
        let t0 = 1_758_758_400_000;
        record(
            &store,
            &pid,
            RecordKind::OpenThread,
            "Fix flaky test",
            "in ci",
            t0,
            false,
        );
        record(
            &store,
            &pid,
            RecordKind::OpenThread,
            "Pinned thread",
            "",
            t0 - 5,
            true,
        );
        record(
            &store,
            &pid,
            RecordKind::Decision,
            "Use SQLite",
            "WAL mode",
            t0 + 1,
            false,
        );
        record(
            &store,
            &pid,
            RecordKind::Gotcha,
            "Windows paths",
            "use dunce",
            t0,
            false,
        );
        record(
            &store,
            &pid,
            RecordKind::Note,
            "Release checklist",
            "tag, then publish",
            t0 - 1,
            true,
        );
        record(
            &store,
            &pid,
            RecordKind::Note,
            "Unpinned note",
            "never injected",
            t0 + 2,
            false,
        );
        record(
            &store,
            &pid,
            RecordKind::Plan,
            "Ship v1",
            "sync, then portal",
            t0 + 3,
            false,
        );
        let mut s = session("s1", &pid, t0);
        s.title = Some("Set up CI".into());
        s.summary = Some(serde_json::json!({"summary": "Added a workflow."}));
        store.insert_session(&s).unwrap();
        let mut current = session("s2", &pid, t0 + 10);
        current.title = Some("current".into());
        store.insert_session(&current).unwrap();

        let a = render_injection(&store, &pid, Some("s2"), 8000).unwrap();
        let b = render_injection(&store, &pid, Some("s2"), 8000).unwrap();
        assert_eq!(a, b);
        let expected = "# blirp memory: Demo
Demo is a CLI.
Use cargo.

## Pinned
- [note] **Release checklist**: tag, then publish
- [open thread] Pinned thread

## Open threads
- **Fix flaky test**: in ci

## Active plans
- **Ship v1**: sync, then portal

## Recent decisions
- **Use SQLite**: WAL mode

## Gotchas
- **Windows paths**: use dunce

## Recent sessions
- 2025-09-25 · shell · box · Set up CI: Added a workflow.

Tools: search older history with the blirp MCP tools (mem_search, mem_session, mem_recent, mem_record) or `blirp mem search \"<query>\"`.
";
        assert_eq!(a, expected);
    }

    // Subagent children would push real sessions out of "Recent sessions";
    // their parent's entry covers them.
    #[test]
    fn recent_sessions_leave_out_subagents() {
        let (_d, store, pid) = project_store("P");
        let mut parent = session("parent", &pid, 1000);
        parent.title = Some("Parent work".into());
        store.insert_session(&parent).unwrap();
        for i in 0..3 {
            let mut child = session(&format!("child{i}"), &pid, 2000 + i);
            child.origin = blirp_core::model::SessionOrigin::External;
            child.parent_session_id = Some("parent".into());
            child.title = Some(format!("subagent {i}"));
            store.insert_session(&child).unwrap();
        }
        let md = render_injection(&store, &pid, None, 8000).unwrap();
        assert!(md.contains("Parent work"), "{md}");
        assert!(!md.contains("subagent"), "{md}");
    }

    #[test]
    fn caps_are_enforced() {
        let (_d, store, pid) = project_store("Big");
        store
            .put_brief(&pid, &"brief ".repeat(5000), "user")
            .unwrap();
        for i in 0..30 {
            record(
                &store,
                &pid,
                RecordKind::OpenThread,
                &format!("thread {i:02}"),
                &"x".repeat(1000),
                1000 + i,
                false,
            );
            record(
                &store,
                &pid,
                RecordKind::Decision,
                &format!("decision {i:02}"),
                "d",
                1000 + i,
                false,
            );
            record(
                &store,
                &pid,
                RecordKind::Gotcha,
                &format!("gotcha {i:02}"),
                "g",
                1000 + i,
                false,
            );
            record(
                &store,
                &pid,
                RecordKind::Plan,
                &format!("plan {i:02}"),
                "p",
                1000 + i,
                false,
            );
            record(
                &store,
                &pid,
                RecordKind::Note,
                &format!("pinned {i:02}"),
                "n",
                1000 + i,
                true,
            );
        }
        let big = render_injection(&store, &pid, None, 100_000).unwrap();
        assert_eq!(big.matches("- **thread").count(), MAX_OPEN_THREADS);
        assert_eq!(big.matches("- **decision").count(), MAX_DECISIONS);
        assert_eq!(big.matches("- **gotcha").count(), MAX_GOTCHAS);
        assert_eq!(big.matches("- **plan").count(), MAX_PLANS);
        assert_eq!(big.matches("- [note] **pinned").count(), MAX_PINNED);
        // Pinned comes first, newest first.
        assert!(big.find("## Pinned").unwrap() < big.find("## Open threads").unwrap());
        assert!(big.find("pinned 29").unwrap() < big.find("pinned 28").unwrap());
        // Newest first within a section.
        assert!(big.find("thread 29").unwrap() < big.find("thread 28").unwrap());
        for cap in [500, 2000, 8000] {
            let out = render_injection(&store, &pid, None, cap).unwrap();
            assert!(out.chars().count() <= cap, "{cap}: {}", out.chars().count());
            assert!(out.starts_with("# blirp memory: Big"));
            assert!(out.trim_end().ends_with(TOOLS_LINE));
        }
    }

    #[test]
    fn handoff_pack_is_capped_and_keeps_latest_turns() {
        let (_d, store, pid) = project_store("P");
        store.put_brief(&pid, "the brief", "user").unwrap();
        let mut src = session("src", &pid, 1000);
        src.title = Some("Refactor parser".into());
        src.summary =
            Some(serde_json::json!({"summary": "Split the parser.", "files": ["src/parse.rs"]}));
        store.insert_session(&src).unwrap();
        for i in 1..=40 {
            let kind = if i % 2 == 1 {
                EventKind::User
            } else {
                EventKind::Assistant
            };
            event(
                &store,
                "src",
                i,
                kind,
                &format!("turn {i} {}", "y".repeat(3000)),
            );
        }
        event(&store, "src", 41, EventKind::FileEdit, "src/lex.rs");
        record(
            &store,
            &pid,
            RecordKind::OpenThread,
            "Finish lexer",
            "",
            5,
            false,
        );
        let pack = render_handoff(&store, &src).unwrap();
        assert!(pack.chars().count() <= HANDOFF_MAX_CHARS);
        assert!(pack.contains("Split the parser."));
        assert!(pack.contains("turn 40 "));
        assert!(!pack.contains("turn 1 "));
        assert!(pack.contains("- src/parse.rs") && pack.contains("- src/lex.rs"));
        assert!(pack.contains("Finish lexer"));
        assert!(pack.matches("**User:**").count() + pack.matches("**Assistant:**").count() <= 12);
    }
}
