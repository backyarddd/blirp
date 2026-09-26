//! Gemini CLI: `~/.gemini/tmp/<project>/chats/session-*.jsonl` (legacy
//! single-object `session-*.json`; subagents in `chats/<parentId>/*.jsonl`).
//! `$GEMINI_CLI_HOME/.gemini` when set. Implemented from the gemini-cli
//! source (`chatRecordingService`); not installed on the reference machine.
//!
//! A message is re-appended with the same `id` whenever it gains tool calls,
//! results or token counts, and `$rewindTo` / `$set.messages` rewrite
//! history. So this adapter dedupes by message id and tool-call id kept in
//! its cursor and allocates seqs from a counter; a rewritten file is re-read
//! from the start without re-emitting what was already stored. The project
//! folder comes from `tmp/<project>/.project_root` or `~/.gemini/projects.json`.

use super::emit::Emit;
use super::jsonl::{FilePos, Lines};
use super::pricing::{self, Usage};
use super::text::{self, content_text, parse_ts};
use super::{
    Adapter, Cursor, EventSink, IngestEnv, Result, SessionMeta, Source, file_sources, walk_files,
};
use blirp_core::model::EventKind;
use blirp_core::store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub struct Gemini {
    dir: PathBuf,
}

impl Gemini {
    pub fn new(env: &IngestEnv) -> Self {
        let home = env
            .var_path("GEMINI_CLI_HOME")
            .unwrap_or_else(|| env.home.clone());
        Self {
            dir: home.join(".gemini"),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Seen {
    #[serde(default)]
    text: bool,
    #[serde(default)]
    tokens: bool,
    #[serde(default)]
    calls: BTreeSet<String>,
    #[serde(default)]
    results: BTreeSet<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(flatten)]
    pos: FilePos,
    next_seq: i64,
    asid: Option<String>,
    #[serde(default)]
    seen: BTreeMap<String, Seen>,
    tokens_in: i64,
    tokens_out: i64,
    cost: f64,
    model: Option<String>,
}

/// Map of project short id / hash dir -> project root.
fn project_roots(gemini_dir: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(bytes) = std::fs::read(gemini_dir.join("projects.json")) else {
        return out;
    };
    if let Ok(v) = serde_json::from_slice::<Value>(&bytes)
        && let Some(Value::Object(m)) = v.get("projects")
    {
        for (path, id) in m {
            if let Some(id) = id.as_str() {
                out.insert(id.to_string(), path.clone());
            }
        }
    }
    out
}

impl Gemini {
    /// `tmp/<project>` directory of a chat file.
    fn project_dir(&self, chat: &Path) -> Option<PathBuf> {
        let tmp = self.dir.join("tmp");
        let rel = chat.strip_prefix(&tmp).ok()?;
        Some(tmp.join(rel.components().next()?))
    }

    fn cwd_for(&self, chat: &Path) -> Option<String> {
        let pdir = self.project_dir(chat)?;
        if let Ok(root) = std::fs::read_to_string(pdir.join(".project_root")) {
            let root = root.trim();
            if !root.is_empty() {
                return Some(root.to_string());
            }
        }
        let id = pdir.file_name()?.to_string_lossy().into_owned();
        project_roots(&self.dir).remove(&id)
    }
}

/// One message record (possibly a re-append of an earlier one).
fn message(m: &Value, e: &mut Emit<'_>, st: &mut State, meta: &mut SessionMeta) -> Result<()> {
    let Some(id) = m.get("id").and_then(Value::as_str) else {
        return Ok(());
    };
    let ts = m.get("timestamp").and_then(parse_ts);
    if let Some(t) = ts {
        meta.started_at = Some(meta.started_at.map_or(t, |s| s.min(t)));
    }
    let ty = m.get("type").and_then(Value::as_str).unwrap_or("");
    let model = m.get("model").and_then(Value::as_str).map(str::to_string);
    if model.is_some() {
        st.model.clone_from(&model);
    }
    let mut seen = st.seen.remove(id).unwrap_or_default();
    if !seen.text {
        let body = content_text(
            m.get("displayContent")
                .filter(|d| !d.is_null())
                .or(m.get("content"))
                .unwrap_or(&Value::Null),
        );
        if !body.trim().is_empty() {
            seen.text = true;
            match ty {
                "user" => {
                    if meta.first_prompt.is_none() && text::title_from_prompt(&body).is_some() {
                        meta.first_prompt = Some(body.clone());
                    }
                    e.text(ts, EventKind::User, &body, None)?;
                }
                "gemini" => e.text(
                    ts,
                    EventKind::Assistant,
                    &body,
                    model.as_ref().map(|m| json!({ "model": m })),
                )?,
                "info" | "error" | "warning" => {
                    e.text(ts, EventKind::System, &format!("{ty}: {body}"), None)?
                }
                _ => {}
            }
        }
    }
    if let Some(Value::Array(calls)) = m.get("toolCalls") {
        for c in calls {
            let Some(cid) = c.get("id").and_then(Value::as_str) else {
                continue;
            };
            let name = c.get("name").and_then(Value::as_str).unwrap_or("tool");
            let cts = c.get("timestamp").and_then(parse_ts).or(ts);
            if seen.calls.insert(cid.to_string()) {
                e.tool_call(cts, name, Some(cid), c.get("args").unwrap_or(&Value::Null))?;
            }
            let status = c.get("status").and_then(Value::as_str).unwrap_or("");
            let done = matches!(status, "success" | "error" | "cancelled");
            if done && seen.results.insert(cid.to_string()) {
                let out = match c.get("result") {
                    Some(r) if !r.is_null() => result_text(r),
                    _ => c.get("resultDisplay").map(content_text).unwrap_or_default(),
                };
                e.tool_result(
                    cts,
                    Some(name),
                    Some(cid),
                    &out,
                    status != "success",
                    Some(json!({ "status": status })),
                )?;
            }
        }
    }
    if !seen.tokens
        && let Some(t) = m.get("tokens").filter(|t| !t.is_null())
    {
        seen.tokens = true;
        let n = |k: &str| t.get(k).and_then(Value::as_i64).unwrap_or(0);
        let (input, cached, output) = (n("input"), n("cached"), n("output") + n("thoughts"));
        st.tokens_in += input;
        st.tokens_out += output;
        st.cost += pricing::estimate(
            model.as_deref().or(st.model.as_deref()).unwrap_or(""),
            Usage {
                input: input - cached,
                output,
                cache_read: cached,
                cache_write: 0,
            },
        );
    }
    st.seen.insert(id.to_string(), seen);
    Ok(())
}

/// Gemini function responses: `[{functionResponse: {response: {output}}}]`.
fn result_text(r: &Value) -> String {
    let mut out = Vec::new();
    let items: Vec<&Value> = match r {
        Value::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    for it in items {
        if let Some(resp) = it.get("functionResponse").and_then(|f| f.get("response")) {
            match resp.get("output").or_else(|| resp.get("error")) {
                Some(Value::String(s)) => out.push(s.clone()),
                Some(v) => out.push(v.to_string()),
                None => out.push(resp.to_string()),
            }
        } else {
            let t = content_text(it);
            if !t.is_empty() {
                out.push(t);
            }
        }
    }
    out.join("\n")
}

impl Adapter for Gemini {
    fn id(&self) -> &'static str {
        "gemini"
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.dir.join("tmp")]
    }

    fn scan(&self, _store: &Store) -> Result<Vec<Source>> {
        Ok(file_sources(walk_files(&self.dir.join("tmp"), 3, &|p| {
            let in_chats = p
                .ancestors()
                .skip(1)
                .take(2)
                .any(|a| a.file_name().is_some_and(|n| n == "chats"));
            let ext_ok = p.extension().is_some_and(|e| e == "jsonl" || e == "json");
            in_chats && ext_ok
        })))
    }

    fn ingest(
        &self,
        src: &Source,
        cursor: Option<Cursor>,
        sink: &mut dyn EventSink,
    ) -> Result<Cursor> {
        let mut st: State = Cursor::state_of(cursor.as_ref());
        let fallback = src
            .path
            .file_stem()
            .map_or_else(|| src.key.clone(), |s| s.to_string_lossy().into_owned());
        let mut meta = SessionMeta {
            cwd: self.cwd_for(&src.path),
            transcript_path: Some(src.path.display().to_string()),
            ..SessionMeta::default()
        };
        // Subagent chats live in chats/<parentSessionId>/<id>.jsonl.
        if let Some(parent) = src
            .path
            .parent()
            .filter(|p| p.file_name().is_some_and(|n| n != "chats"))
        {
            meta.parent = parent.file_name().map(|n| n.to_string_lossy().into_owned());
        }
        let mut partial = false;
        if src.path.extension().is_some_and(|e| e == "json") {
            // Legacy: the whole conversation in one JSON object.
            let v: Value = match serde_json::from_slice(&std::fs::read(&src.path)?) {
                Ok(v) => v,
                Err(e) => {
                    // Possibly mid-write; try again on the next change.
                    sink.warn(&format!("unreadable session file: {e}"));
                    let mut c = Cursor::from_state(&st)?;
                    c.retry = true;
                    return Ok(c);
                }
            };
            header(&v, &mut st, &mut meta);
            let asid = st.asid.clone().unwrap_or_else(|| fallback.clone());
            let mut e = Emit::counter(sink, &asid, st.next_seq);
            if let Some(Value::Array(msgs)) = v.get("messages") {
                for m in msgs {
                    message(m, &mut e, &mut st, &mut meta)?;
                }
            }
            st.next_seq = e.next_seq();
        } else {
            let mut lines = Lines::open(&src.path, &st.pos, false)?;
            // A rewritten file restarts at 0; the seen-set prevents duplicates.
            lines.for_each(|_, raw| {
                let v: Value = match serde_json::from_slice(raw) {
                    Ok(v) => v,
                    Err(e) => {
                        sink.warn(&format!("skipping unparseable line: {e}"));
                        return Ok(());
                    }
                };
                if v.get("$rewindTo").is_some() {
                    return Ok(());
                }
                if let Some(set) = v.get("$set") {
                    if let Some(Value::String(s)) = set.get("summary") {
                        meta.title = Some(s.clone());
                    }
                    let asid = st.asid.clone().unwrap_or_else(|| fallback.clone());
                    let mut e = Emit::counter(sink, &asid, st.next_seq);
                    if let Some(Value::Array(msgs)) = set.get("messages") {
                        for m in msgs {
                            message(m, &mut e, &mut st, &mut meta)?;
                        }
                    }
                    st.next_seq = e.next_seq();
                    return Ok(());
                }
                if v.get("sessionId").is_some() && v.get("type").is_none() {
                    header(&v, &mut st, &mut meta);
                    return Ok(());
                }
                let asid = st.asid.clone().unwrap_or_else(|| fallback.clone());
                let mut e = Emit::counter(sink, &asid, st.next_seq);
                message(&v, &mut e, &mut st, &mut meta)?;
                st.next_seq = e.next_seq();
                Ok(())
            })?;
            st.pos = lines.pos().clone();
            partial = lines.partial;
        }
        let asid = st.asid.clone().unwrap_or(fallback);
        meta.tokens_in = Some(st.tokens_in);
        meta.tokens_out = Some(st.tokens_out);
        meta.cost_usd = Some(st.cost);
        meta.model = st.model.clone();
        sink.session(&asid, meta);
        let mut c = Cursor::from_state(&st)?;
        c.retry = partial;
        Ok(c)
    }
}

fn header(v: &Value, st: &mut State, meta: &mut SessionMeta) {
    if st.asid.is_none() {
        st.asid = v
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    if let Some(t) = v.get("startTime").and_then(parse_ts) {
        meta.started_at = Some(t);
    }
    if let Some(Value::String(s)) = v.get("summary") {
        meta.title = Some(s.clone());
    }
}
