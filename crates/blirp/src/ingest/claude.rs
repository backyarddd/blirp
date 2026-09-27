//! Claude Code: `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`
//! (`$CLAUDE_CONFIG_DIR/projects` when set). Verified against real data.
//!
//! Subagent transcripts (`<sessionId>/subagents/agent-<id>.jsonl`) become
//! child sessions (`agent_session_id = "<sessionId>:agent-<id>"`,
//! `parent_session_id` = the parent's row). Large tool outputs spilled to
//! `<sessionId>/tool-results/*.txt` are linked by path in the result's meta
//! with a 4 KiB preview, never inlined.

use super::emit::Emit;
use super::jsonl::{FilePos, Lines};
use super::pricing::{self, Usage};
use super::text::{self, TOOL_RESULT_MAX, content_text, parse_ts};
use super::{
    Adapter, Cursor, EventSink, IngestEnv, Launches, Result, SessionMeta, Source, file_sources,
    walk_files,
};
use blirp_core::model::EventKind;
use blirp_core::store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};

pub struct Claude {
    root: PathBuf,
}

impl Claude {
    pub fn new(env: &IngestEnv) -> Self {
        let base = env
            .var_path("CLAUDE_CONFIG_DIR")
            .unwrap_or_else(|| env.home.join(".claude"));
        Self {
            root: base.join("projects"),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(flatten)]
    pos: FilePos,
    /// Usage of one API message repeats on every line of that message.
    last_msg_id: Option<String>,
    usage: SumUsage,
    cost_est: f64,
    cost_reported: Option<f64>,
    model: Option<String>,
    #[serde(default)]
    launch: Option<Launches>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SumUsage {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
}

/// (agent_session_id, parent agent_session_id) for a transcript path.
fn ids(path: &Path) -> Option<(String, Option<String>)> {
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let parent = path.parent()?;
    if parent.file_name().is_some_and(|n| n == "subagents") {
        let sid = parent.parent()?.file_name()?.to_string_lossy().into_owned();
        return Some((format!("{sid}:{stem}"), Some(sid)));
    }
    Some((stem, None))
}

fn i64_at(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}

/// First 4 KiB of a spilled tool-result file.
fn preview_file(path: &Path) -> Option<String> {
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(TOOL_RESULT_MAX as u64)
        .read_to_end(&mut buf)
        .ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// A spilled-output path mentioned in a tool result, if any.
fn spilled_path(output: &str) -> Option<String> {
    let at = output.find("tool-results")?;
    let start = output[..at]
        .rfind(|c: char| c.is_whitespace() || c == '"' || c == '(')
        .map_or(0, |i| i + 1);
    let end = output[at..]
        .find(|c: char| c.is_whitespace() || c == '"' || c == ')')
        .map_or(output.len(), |i| at + i);
    let p = output[start..end].trim_end_matches(['.', ',']);
    p.ends_with(".txt").then(|| p.to_string())
}

fn tool_result_output(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(c @ Value::Array(_)) => content_text(c),
        _ => String::new(),
    }
}

/// What conversation line `v` says about how the session runs
/// ([`Launches`]): its `entrypoint` is `sdk-cli` for `claude -p` and
/// `sdk-ts`/`sdk-py` for the Agent SDKs; interactive ones are `cli`,
/// `claude-vscode`, `claude-desktop`, ...; older Claude Code wrote none (an
/// interactive session). A prompt of the user starts a turn (`promptId`).
pub(crate) fn launch_line(v: &Value, l: &mut Launches) {
    let ty = v.get("type").and_then(Value::as_str);
    if !matches!(ty, Some("user" | "assistant")) {
        return;
    }
    l.run(
        v.get("entrypoint")
            .and_then(Value::as_str)
            .is_some_and(|ep| ep.starts_with("sdk-")),
    );
    if ty == Some("user") && is_prompt(v) {
        l.turn(v.get("promptId").and_then(Value::as_str));
    }
}

/// A user line the harness wrote, not the user: `isMeta`, or an `origin`
/// other than `human` (`task-notification` for background-task results,
/// `coordinator`, `peer`, `auto-continuation`, ...).
fn is_harness(v: &Value) -> bool {
    v.get("isMeta").and_then(Value::as_bool) == Some(true)
        || v.pointer("/origin/kind")
            .and_then(Value::as_str)
            .is_some_and(|k| k != "human")
}

/// A user line that is a prompt: typed (or pasted) by the user, not a tool
/// result or harness message.
fn is_prompt(v: &Value) -> bool {
    if is_harness(v) || v.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    match v.pointer("/message/content") {
        Some(Value::String(s)) => !is_system_text(s),
        Some(Value::Array(blocks)) => {
            blocks
                .iter()
                .any(|b| match b.get("type").and_then(Value::as_str) {
                    Some("text") => b
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|t| !is_system_text(t)),
                    Some("image") => true,
                    _ => false,
                })
        }
        _ => false,
    }
}

/// Plumbing a user line carries that is not something the user typed.
fn is_system_text(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with("<local-command-stdout>")
        || t.starts_with("<local-command-stderr>")
        || t.starts_with("<local-command-caveat>")
        || t.starts_with("<system-reminder>")
        || t.starts_with("<task-notification>")
        || t.starts_with("Caveat: The messages below")
}

impl Adapter for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn scan(&self, _store: &Store) -> Result<Vec<Source>> {
        let root = self.root.clone();
        let files = walk_files(&self.root, 3, &|p| {
            let Some(parent) = p.parent() else {
                return false;
            };
            p.extension().is_some_and(|e| e == "jsonl")
                && (parent.parent() == Some(root.as_path())
                    || parent.file_name().is_some_and(|n| n == "subagents"))
        });
        Ok(file_sources(files))
    }

    fn ingest(
        &self,
        src: &Source,
        cursor: Option<Cursor>,
        sink: &mut dyn EventSink,
    ) -> Result<Cursor> {
        let mut st: State = Cursor::state_of(cursor.as_ref());
        let Some((asid, parent)) = ids(&src.path) else {
            return Cursor::from_state(&st);
        };
        let mut lines = Lines::open(&src.path, &st.pos, false)?;
        if lines.reset {
            st = State::default();
        }
        let mut launch = Launches::resume(st.launch.take(), st.pos.line);
        // A subagent inherits its parent's entrypoint and has one prompt: it
        // is scripted only when its parent's transcript is (the parent may
        // not be stored yet when this file is read first).
        if parent.is_some()
            && !launch.settled()
            && let Some(main) = parent_transcript(&src.path)
            && matches!(super::transcript_is_headless("claude", &main), Ok(false))
        {
            launch.run(false);
        }
        launch.report(false, sink, &asid);
        let mut meta = SessionMeta {
            parent: parent.clone(),
            transcript_path: Some(src.path.display().to_string()),
            ..SessionMeta::default()
        };
        if parent.is_some() {
            meta.title = subagent_title(&src.path);
        }
        let mut reported = false;
        lines.for_each(|ix, raw| {
            let v: Value = match serde_json::from_slice(raw) {
                Ok(v) => v,
                Err(e) => {
                    sink.warn(&format!("skipping unparseable line: {e}"));
                    return Ok(());
                }
            };
            let before = launch.is_headless();
            launch_line(&v, &mut launch);
            launch.report(before, sink, &asid);
            let mut e = Emit::line(sink, &asid, ix);
            line(&v, &mut e, &mut st, &mut meta)?;
            super::report_cwd(sink, &asid, &meta, &mut reported);
            Ok(())
        })?;
        st.pos = lines.pos().clone();
        if meta.cwd.is_none() && st.pos.line > 0 {
            meta.cwd = decoded_cwd(&src.path);
        }
        meta.tokens_in = Some(st.usage.input + st.usage.cache_read + st.usage.cache_write);
        meta.tokens_out = Some(st.usage.output);
        meta.cost_usd = Some(st.cost_reported.unwrap_or(st.cost_est));
        meta.model = st.model.clone();
        let reread = launch.finish(&mut meta);
        sink.session(&asid, meta);
        if reread {
            st = State::default();
        }
        st.launch = Some(launch);
        let mut c = Cursor::from_state(&st)?;
        c.retry = lines.partial || reread;
        Ok(c)
    }
}

/// Subagent transcripts of the session transcript `<dir>/<sessionId>.jsonl`
/// (`<dir>/<sessionId>/subagents/*.jsonl`); none for a subagent's own.
pub(crate) fn subagent_transcripts(path: &Path) -> Vec<PathBuf> {
    if path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "subagents")
    {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(path.with_extension("").join("subagents")) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect()
}

/// `<dir>/<sessionId>.jsonl` for a subagent transcript
/// `<dir>/<sessionId>/subagents/agent-<id>.jsonl`.
fn parent_transcript(path: &Path) -> Option<PathBuf> {
    let session_dir = path.parent()?.parent()?;
    let name = session_dir.file_name()?.to_string_lossy();
    Some(session_dir.with_file_name(format!("{name}.jsonl")))
}

fn subagent_title(path: &Path) -> Option<String> {
    let meta_path = path.with_extension("meta.json");
    let v: Value = serde_json::from_slice(&std::fs::read(meta_path).ok()?).ok()?;
    let kind = v
        .get("agentType")
        .and_then(Value::as_str)
        .unwrap_or("agent");
    let desc = v.get("description").and_then(Value::as_str)?;
    Some(format!("subagent ({kind}): {desc}"))
}

/// Fallback when no line carries `cwd`: decode the project directory name.
fn decoded_cwd(path: &Path) -> Option<String> {
    let mut dir = path.parent()?;
    if dir.file_name().is_some_and(|n| n == "subagents") {
        dir = dir.parent()?.parent()?;
    }
    let name = dir.file_name()?.to_string_lossy();
    super::decode_dashed_dir(&name).map(|p| p.display().to_string())
}

fn line(v: &Value, e: &mut Emit<'_>, st: &mut State, meta: &mut SessionMeta) -> Result<()> {
    let ts = v.get("timestamp").and_then(parse_ts);
    // Where the session started; a later `cd` does not move it.
    if meta.cwd.is_none()
        && let Some(cwd) = v
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
    {
        meta.cwd = Some(cwd.to_string());
    }
    if let Some(b) = v
        .get("gitBranch")
        .and_then(Value::as_str)
        .filter(|b| !b.is_empty() && *b != "HEAD")
    {
        meta.branch = Some(b.to_string());
    }
    if let Some(t) = ts {
        meta.started_at = Some(meta.started_at.map_or(t, |s| s.min(t)));
    }
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "user" => user_line(v, ts, e, meta),
        "assistant" => assistant_line(v, ts, e, st),
        "attachment" => {
            let a = v.get("attachment").unwrap_or(&Value::Null);
            if a.get("type").and_then(Value::as_str) == Some("compact_file_reference")
                && let Some(file) = a.get("filename").and_then(Value::as_str)
            {
                let preview = preview_file(Path::new(file)).unwrap_or_default();
                e.tool_result(
                    ts,
                    None,
                    None,
                    &preview,
                    false,
                    Some(json!({ "result_file": file, "display_path": a.get("displayPath") })),
                )?;
            }
            Ok(())
        }
        "system" => {
            if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
                let m = v.get("compactMetadata");
                e.push(
                    ts,
                    EventKind::System,
                    "conversation compacted".into(),
                    m.map(|m| json!({ "trigger": m.get("trigger"), "pre_tokens": m.get("preTokens") })),
                )?;
            }
            Ok(())
        }
        "summary" => {
            if let Some(s) = v.get("summary").and_then(Value::as_str) {
                meta.title = Some(s.to_string());
            }
            Ok(())
        }
        "ai-title" => {
            if let Some(s) = v.get("aiTitle").and_then(Value::as_str) {
                meta.title = Some(s.to_string());
            }
            Ok(())
        }
        "cost-state" => {
            if let Some(c) = v.get("totalCostUSD").and_then(Value::as_f64) {
                st.cost_reported = Some(c);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn user_line(v: &Value, ts: Option<i64>, e: &mut Emit<'_>, meta: &mut SessionMeta) -> Result<()> {
    let content = v
        .get("message")
        .and_then(|m| m.get("content"))
        .unwrap_or(&Value::Null);
    if v.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
        return e.text(ts, EventKind::Summary, &content_text(content), None);
    }
    if is_harness(v) {
        return e.text(ts, EventKind::System, &content_text(content), None);
    }
    let mut user_text = |e: &mut Emit<'_>, s: &str| -> Result<()> {
        if is_system_text(s) {
            return e.text(ts, EventKind::System, s, None);
        }
        if meta.first_prompt.is_none() && text::title_from_prompt(s).is_some() {
            meta.first_prompt = Some(s.to_string());
        }
        e.text(ts, EventKind::User, s, None)
    };
    match content {
        Value::String(s) => user_text(e, s),
        Value::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" => {
                        if let Some(s) = b.get("text").and_then(Value::as_str) {
                            user_text(e, s)?;
                        }
                    }
                    "image" => e.text(ts, EventKind::User, "[image]", None)?,
                    "tool_result" => {
                        let out = tool_result_output(b);
                        let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                        let extra = spilled_path(&out).map(|p| json!({ "result_file": p }));
                        e.tool_result(
                            ts,
                            None,
                            b.get("tool_use_id").and_then(Value::as_str),
                            &out,
                            is_error,
                            extra,
                        )?;
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn assistant_line(v: &Value, ts: Option<i64>, e: &mut Emit<'_>, st: &mut State) -> Result<()> {
    let msg = v.get("message").unwrap_or(&Value::Null);
    let model = msg
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.starts_with('<'))
        .map(str::to_string);
    if let Some(m) = &model {
        st.model = Some(m.clone());
    }
    let msg_id = msg.get("id").and_then(Value::as_str).map(str::to_string);
    if let Some(u) = msg.get("usage")
        && (msg_id.is_none() || msg_id != st.last_msg_id)
    {
        let usage = Usage {
            input: i64_at(u, "input_tokens"),
            output: i64_at(u, "output_tokens"),
            cache_read: i64_at(u, "cache_read_input_tokens"),
            cache_write: i64_at(u, "cache_creation_input_tokens"),
        };
        st.usage.input += usage.input;
        st.usage.output += usage.output;
        st.usage.cache_read += usage.cache_read;
        st.usage.cache_write += usage.cache_write;
        st.cost_est += pricing::estimate(model.as_deref().unwrap_or(""), usage);
        st.last_msg_id.clone_from(&msg_id);
    }
    let Some(Value::Array(blocks)) = msg.get("content") else {
        return Ok(());
    };
    for b in blocks {
        match b.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                let t = b.get("text").and_then(Value::as_str).unwrap_or("");
                e.text(
                    ts,
                    EventKind::Assistant,
                    t,
                    model.as_ref().map(|m| json!({ "model": m })),
                )?;
            }
            "tool_use" => {
                let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                e.tool_call(
                    ts,
                    name,
                    b.get("id").and_then(Value::as_str),
                    b.get("input").unwrap_or(&Value::Null),
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_ids_and_spills() {
        let p = Path::new("/r/proj/s1/subagents/agent-ab12.jsonl");
        assert_eq!(ids(p), Some(("s1:agent-ab12".into(), Some("s1".into()))));
        assert_eq!(
            ids(Path::new("/r/proj/s1.jsonl")),
            Some(("s1".into(), None))
        );
        let out =
            "Output too large. Full output saved to: C:\\x\\s1\\tool-results\\ab.txt\n\nPreview";
        assert_eq!(
            spilled_path(out).as_deref(),
            Some("C:\\x\\s1\\tool-results\\ab.txt")
        );
        assert_eq!(spilled_path("no spill"), None);
    }
}
