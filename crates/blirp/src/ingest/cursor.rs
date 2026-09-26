//! Cursor CLI (`cursor-agent`), two stores under `~/.cursor`
//! (`$CURSOR_CONFIG_DIR` when set):
//!
//! - `chats/<md5(cwd)>/<chatId>/store.db`: SQLite, `meta` row `0` is
//!   hex-encoded JSON (`agentId`, `latestRootBlobId`, `name`, `createdAt`,
//!   `lastUsedModel`); `blobs` is a content-addressed tree whose JSON leaves
//!   are messages (`role`, AI-SDK style content) and whose binary nodes list
//!   child blob ids (protobuf field 1, 32 bytes each). Preferred when present.
//! - `projects/<encoded-cwd>/agent-transcripts/[<id>/]<id>.jsonl`: one
//!   `{role, message: {content}}` per line (Anthropic blocks), no
//!   timestamps; subagents in `<parent>/subagents/<child>.jsonl`.
//!
//! The transcript layout was verified on this machine (one sparse file);
//! the store.db layout follows community documentation of the format and is
//! parsed defensively. The Cursor editor's own `state.vscdb` is not an agent
//! CLI store and is not ingested.

use super::emit::Emit;
use super::jsonl::{FilePos, Lines};
use super::opencode::{open_ro, retry_busy};
use super::text::{self, content_text};
use super::{
    Adapter, Cursor as IngestCursor, EventSink, IngestEnv, Result, SessionMeta, Source, walk_files,
};
use blirp_core::model::EventKind;
use blirp_core::store::Store;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

pub struct Cursor {
    dir: PathBuf,
}

impl Cursor {
    pub fn new(env: &IngestEnv) -> Self {
        Self {
            dir: env
                .var_path("CURSOR_CONFIG_DIR")
                .unwrap_or_else(|| env.home.join(".cursor")),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TranscriptState {
    #[serde(flatten)]
    pos: FilePos,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreState {
    root: String,
    next_seq: i64,
    /// Message blobs already stored (16-hex-char prefixes).
    #[serde(default)]
    seen: BTreeSet<String>,
}

/// Text inside `<user_query>`, the part of a Cursor user turn the user typed.
fn user_query(s: &str) -> Option<&str> {
    let start = s.find("<user_query>")? + "<user_query>".len();
    let end = s[start..]
        .find("</user_query>")
        .map_or(s.len(), |e| start + e);
    Some(s[start..end].trim())
}

/// A user turn: (kind, text). Context-only turns become system events.
fn classify_user(s: &str) -> (EventKind, String) {
    match user_query(s) {
        Some(q) => (EventKind::User, q.to_string()),
        None if s.trim_start().starts_with('<') => (EventKind::System, s.to_string()),
        None => (EventKind::User, s.to_string()),
    }
}

fn store_db_source(db: &Path) -> Option<Source> {
    let mut src = Source::file(db)?;
    let wal = PathBuf::from(format!("{}-wal", db.display()));
    if let Some(w) = Source::file(&wal) {
        src.fingerprint = format!("{}|{}", src.fingerprint, w.fingerprint);
        src.mtime_ms = src.mtime_ms.max(w.mtime_ms);
    }
    // A database, not a plain file: changes are picked up by a rescan,
    // which fingerprints the WAL too.
    src.item = Some("store.db".into());
    Some(src)
}

impl Adapter for Cursor {
    fn id(&self) -> &'static str {
        "cursor"
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.dir.join("chats"), self.dir.join("projects")]
    }

    fn scan(&self, _store: &Store) -> Result<Vec<Source>> {
        let dbs = walk_files(&self.dir.join("chats"), 2, &|p| {
            p.file_name().is_some_and(|n| n == "store.db")
        });
        let chat_ids: HashSet<String> = dbs
            .iter()
            .filter_map(|p| {
                p.parent()?
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .collect();
        let mut out: Vec<Source> = dbs.iter().filter_map(|p| store_db_source(p)).collect();
        let projects = self.dir.join("projects");
        let Ok(rd) = std::fs::read_dir(&projects) else {
            return Ok(out);
        };
        for proj in rd.flatten() {
            let transcripts = proj.path().join("agent-transcripts");
            if !transcripts.is_dir() {
                continue;
            }
            for f in walk_files(&transcripts, 3, &|p| {
                p.extension().is_some_and(|e| e == "jsonl")
            }) {
                let stem = f
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !chat_ids.contains(&stem)
                    && let Some(s) = Source::file(&f)
                {
                    out.push(s);
                }
            }
        }
        Ok(out)
    }

    fn ingest(
        &self,
        src: &Source,
        cursor: Option<IngestCursor>,
        sink: &mut dyn EventSink,
    ) -> Result<IngestCursor> {
        if src.path.file_name().is_some_and(|n| n == "store.db") {
            ingest_store(src, cursor, sink)
        } else {
            ingest_transcript(src, cursor, sink)
        }
    }
}

fn ingest_transcript(
    src: &Source,
    cursor: Option<IngestCursor>,
    sink: &mut dyn EventSink,
) -> Result<IngestCursor> {
    let mut st: TranscriptState = IngestCursor::state_of(cursor.as_ref());
    let asid = src
        .path
        .file_stem()
        .map_or_else(|| src.key.clone(), |s| s.to_string_lossy().into_owned());
    let mut meta = SessionMeta {
        transcript_path: Some(src.path.display().to_string()),
        ..SessionMeta::default()
    };
    let parent_dir = src.path.parent();
    if parent_dir
        .and_then(Path::file_name)
        .is_some_and(|n| n == "subagents")
    {
        meta.parent = parent_dir
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned());
    }
    // projects/<encoded-cwd>/agent-transcripts/...
    meta.cwd = src
        .path
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "agent-transcripts"))
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|n| super::decode_dashed_dir(&n.to_string_lossy()))
        .map(|p| p.display().to_string());
    super::report_cwd(sink, &asid, &meta, &mut false);
    let mut lines = Lines::open(&src.path, &st.pos, false)?;
    lines.for_each(|ix, raw| {
        let v: Value = match serde_json::from_slice(raw) {
            Ok(v) => v,
            Err(e) => {
                sink.warn(&format!("skipping unparseable line: {e}"));
                return Ok(());
            }
        };
        let role = v.get("role").and_then(Value::as_str).unwrap_or("");
        let content = v
            .get("message")
            .and_then(|m| m.get("content"))
            .unwrap_or(&Value::Null);
        let mut e = Emit::line(sink, &asid, ix);
        match role {
            "user" => {
                if let Value::Array(blocks) = content {
                    for b in blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                    {
                        let out = match b.get("content") {
                            Some(Value::String(s)) => s.clone(),
                            Some(c) => content_text(c),
                            None => String::new(),
                        };
                        e.tool_result(
                            None,
                            None,
                            b.get("tool_use_id").and_then(Value::as_str),
                            &out,
                            false,
                            None,
                        )?;
                    }
                }
                let (kind, body) = classify_user(&content_text(content));
                if kind == EventKind::User
                    && meta.first_prompt.is_none()
                    && text::title_from_prompt(&body).is_some()
                {
                    meta.first_prompt = Some(body.clone());
                }
                e.text(None, kind, &body, None)
            }
            "assistant" => assistant_blocks(content, &mut e),
            _ => Ok(()),
        }
    })?;
    st.pos = lines.pos().clone();
    sink.session(&asid, meta);
    let mut c = IngestCursor::from_state(&st)?;
    c.retry = lines.partial;
    Ok(c)
}

/// Anthropic-style (`text`, `tool_use`) and AI-SDK-style (`tool-call`) blocks.
fn assistant_blocks(content: &Value, e: &mut Emit<'_>) -> Result<()> {
    let Value::Array(blocks) = content else {
        return e.text(None, EventKind::Assistant, &content_text(content), None);
    };
    for b in blocks {
        let s = |k: &str| b.get(k).and_then(Value::as_str);
        match s("type").unwrap_or("") {
            "text" => e.text(None, EventKind::Assistant, s("text").unwrap_or(""), None)?,
            "tool_use" => e.tool_call(
                None,
                s("name").unwrap_or("tool"),
                s("id"),
                b.get("input").unwrap_or(&Value::Null),
            )?,
            "tool-call" => e.tool_call(
                None,
                s("toolName").unwrap_or("tool"),
                s("toolCallId"),
                b.get("args")
                    .or_else(|| b.get("input"))
                    .unwrap_or(&Value::Null),
            )?,
            _ => {}
        }
    }
    Ok(())
}

/// Child blob ids and embedded JSON of a binary tree node (protobuf wire
/// format: field 1 = 32-byte child id; length-delimited JSON = message).
fn parse_node(data: &[u8]) -> (Vec<String>, Vec<Vec<u8>>) {
    fn varint(d: &[u8], i: &mut usize) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *d.get(*i)?;
            *i += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    let (mut children, mut json) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < data.len() {
        let Some(tag) = varint(data, &mut i) else {
            break;
        };
        match tag & 7 {
            0 => {
                if varint(data, &mut i).is_none() {
                    break;
                }
            }
            1 => i += 8,
            5 => i += 4,
            2 => {
                let Some(len) = varint(data, &mut i).and_then(|l| usize::try_from(l).ok()) else {
                    break;
                };
                let Some(bytes) = data.get(i..i.saturating_add(len)) else {
                    break;
                };
                i += len;
                if tag >> 3 == 1 && len == 32 {
                    children.push(hex::encode(bytes));
                } else if bytes.first() == Some(&b'{') {
                    json.push(bytes.to_vec());
                }
            }
            _ => break,
        }
    }
    (children, json)
}

fn ingest_store(
    src: &Source,
    cursor: Option<IngestCursor>,
    sink: &mut dyn EventSink,
) -> Result<IngestCursor> {
    let mut st: StoreState = IngestCursor::state_of(cursor.as_ref());
    let conn = open_ro(&src.path)?;
    let raw: Option<String> = retry_busy(|| {
        conn.query_row("SELECT value FROM meta WHERE key = '0'", [], |r| r.get(0))
            .optional()
    })?;
    let Some(raw) = raw else {
        return IngestCursor::from_state(&st);
    };
    let head: Value = hex::decode(raw.trim())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let s = |k: &str| head.get(k).and_then(Value::as_str);
    let dir = src.path.parent().unwrap_or(Path::new(""));
    let asid = s("agentId")
        .map(str::to_string)
        .or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| src.key.clone());
    let side: Value = std::fs::read(dir.join("meta.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let mut meta = SessionMeta {
        cwd: side.get("cwd").and_then(Value::as_str).map(str::to_string),
        title: s("name")
            .filter(|n| !n.trim().is_empty() && !n.starts_with("New "))
            .map(str::to_string),
        model: s("lastUsedModel").map(str::to_string),
        started_at: head
            .get("createdAt")
            .and_then(text::parse_ts)
            .or_else(|| side.get("createdAtMs").and_then(text::parse_ts)),
        transcript_path: Some(src.path.display().to_string()),
        ..SessionMeta::default()
    };
    let Some(root) = s("latestRootBlobId").map(str::to_string) else {
        sink.session(&asid, meta);
        return IngestCursor::from_state(&st);
    };
    // Depth-first from the root; children in order.
    let mut messages: Vec<(String, Value)> = Vec::new();
    let mut stack = vec![root.clone()];
    let mut visited = HashSet::new();
    while let Some(id) = stack.pop() {
        if !visited.insert(id.clone()) || visited.len() > 100_000 {
            continue;
        }
        let data: Option<Vec<u8>> = retry_busy(|| {
            conn.query_row("SELECT data FROM blobs WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .optional()
        })?;
        let Some(data) = data else { continue };
        if data.first() == Some(&b'{') {
            match serde_json::from_slice::<Value>(&data) {
                Ok(v) => messages.push((id.chars().take(16).collect(), v)),
                Err(_) => sink.warn("skipping unparseable message blob"),
            }
            continue;
        }
        let (children, embedded) = parse_node(&data);
        for j in embedded {
            if let Ok(v) = serde_json::from_slice::<Value>(&j) {
                messages.push((format!("{:016x}", text::fnv64(&j)), v));
            }
        }
        stack.extend(children.into_iter().rev());
    }
    super::report_cwd(sink, &asid, &meta, &mut false);
    let mut e = Emit::counter(sink, &asid, st.next_seq);
    for (key, m) in &messages {
        if st.seen.contains(key) {
            continue;
        }
        let content = m.get("content").unwrap_or(&Value::Null);
        match m.get("role").and_then(Value::as_str).unwrap_or("") {
            "user" => {
                let (kind, body) = classify_user(&content_text(content));
                if kind == EventKind::User
                    && meta.first_prompt.is_none()
                    && text::title_from_prompt(&body).is_some()
                {
                    meta.first_prompt = Some(body.clone());
                }
                e.text(None, kind, &body, None)?;
            }
            "assistant" => assistant_blocks(content, &mut e)?,
            "tool" => {
                if let Value::Array(blocks) = content {
                    for b in blocks {
                        let out = match b.get("result") {
                            Some(Value::String(s)) => s.clone(),
                            Some(r) => {
                                let t = content_text(r);
                                if t.is_empty() { r.to_string() } else { t }
                            }
                            None => String::new(),
                        };
                        e.tool_result(
                            None,
                            b.get("toolName").and_then(Value::as_str),
                            b.get("toolCallId").and_then(Value::as_str),
                            &out,
                            b.get("isError").and_then(Value::as_bool).unwrap_or(false),
                            None,
                        )?;
                    }
                }
            }
            _ => {}
        }
        st.seen.insert(key.clone());
    }
    st.next_seq = e.next_seq();
    st.root = root;
    sink.session(&asid, meta);
    IngestCursor::from_state(&st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_turns_and_tree_nodes() {
        assert_eq!(
            classify_user("<user_info>x</user_info>\n<user_query>\nfix it\n</user_query>"),
            (EventKind::User, "fix it".into())
        );
        assert_eq!(classify_user("<rules>r</rules>").0, EventKind::System);
        let mut node = vec![0x0a, 0x20];
        node.extend([0xab; 32]);
        node.extend([0x12, 0x02, b'{', b'}']);
        let (children, json) = parse_node(&node);
        assert_eq!(children, [hex::encode([0xab; 32])]);
        assert_eq!(json, [b"{}".to_vec()]);
    }
}
