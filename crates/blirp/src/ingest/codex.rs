//! Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl[.zst]` and
//! `archived_sessions/` (`$CODEX_HOME` when set). Verified against real data
//! (plain JSONL; `.zst` handled by the shared reader).
//!
//! Lines are `{timestamp, type, payload}`. Conversation comes from
//! `response_item` payloads (`event_msg` user/agent messages duplicate them
//! and are skipped); token totals from the latest `token_usage_record` /
//! `event_msg.token_count`, which are cumulative.

use super::emit::Emit;
use super::jsonl::{FilePos, Lines};
use super::pricing::{self, Usage};
use super::text::{self, content_text, parse_ts};
use super::{
    Adapter, Cursor, EventSink, IngestEnv, Launches, Result, SessionMeta, Source, file_sources,
    walk_files,
};
use blirp_core::model::EventKind;
use blirp_core::store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Codex {
    home: PathBuf,
}

impl Codex {
    pub fn new(env: &IngestEnv) -> Self {
        Self {
            home: env
                .var_path("CODEX_HOME")
                .unwrap_or_else(|| env.home.join(".codex")),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(flatten)]
    pos: FilePos,
    asid: Option<String>,
    model: Option<String>,
    usage: Option<Tokens>,
    /// call_id -> tool name, to label outputs.
    #[serde(default)]
    calls: HashMap<String, String>,
    #[serde(default)]
    launch: Option<Launches>,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
struct Tokens {
    input: i64,
    cached: i64,
    output: i64,
}

fn is_rollout(p: &Path) -> bool {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    name.starts_with("rollout-") && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
}

/// What rollout line `v` says about how the session runs ([`Launches`]):
/// `session_meta.source` is `exec` for `codex exec` (the TUI says `cli`, the
/// desktop app and IDE `vscode`, subagents an object), and every turn
/// writes a `turn_context`. A resumed session may continue its rollout without a new
/// `session_meta`; an exec run continued that way shows as a second turn.
pub(crate) fn launch_line(v: &Value, l: &mut Launches) {
    match v.get("type").and_then(Value::as_str) {
        Some("session_meta") => {
            l.run(v.pointer("/payload/source").and_then(Value::as_str) == Some("exec"));
        }
        Some("turn_context") => l.turn(None),
        _ => {}
    }
}

/// Every line [`launch_line`] looks at contains one of these.
pub(crate) const LAUNCH_NEEDLES: &[&[u8]] = &[b"\"session_meta\"", b"\"turn_context\""];

/// Session id from `rollout-<date>-<uuid>.jsonl`, used until `session_meta`.
fn id_from_name(p: &Path) -> Option<String> {
    let name = p.file_name()?.to_string_lossy();
    let stem = name.trim_end_matches(".zst").trim_end_matches(".jsonl");
    // The uuid is the last 36 chars.
    (stem.len() > 36).then(|| stem[stem.len() - 36..].to_string())
}

/// User-role messages Codex injects itself: AGENTS.md instructions and
/// context blocks wrapped in one XML-ish element (`<environment_context>`,
/// `<recommended_plugins>`, `<codex_internal_context>`, ...).
fn is_injected(s: &str) -> bool {
    let t = s.trim();
    if t.starts_with("# AGENTS.md") || (t.starts_with("# Instructions") && t.contains("AGENTS")) {
        return true;
    }
    let Some(rest) = t.strip_prefix('<') else {
        return false;
    };
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .collect();
    !name.is_empty() && (t.ends_with(&format!("</{name}>")) || name == "turn_aborted")
}

fn tokens_of(u: &Value) -> Option<Tokens> {
    let n = |k: &str| u.get(k).and_then(Value::as_i64);
    Some(Tokens {
        input: n("input_tokens")?,
        cached: n("cached_input_tokens").unwrap_or(0),
        output: n("output_tokens").unwrap_or(0),
    })
}

/// Tool output: a plain string, a JSON string with an `output` field, or
/// an array of text items.
fn output_text(v: &Value) -> String {
    match v {
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(o)) => o
                .get("output")
                .and_then(Value::as_str)
                .map_or_else(|| s.clone(), str::to_string),
            _ => s.clone(),
        },
        other => content_text(other),
    }
}

impl Adapter for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![
            self.home.join("sessions"),
            self.home.join("archived_sessions"),
        ]
    }

    fn scan(&self, _store: &Store) -> Result<Vec<Source>> {
        let mut files = Vec::new();
        for r in self.roots() {
            files.extend(walk_files(&r, 4, &is_rollout));
        }
        Ok(file_sources(files))
    }

    fn ingest(
        &self,
        src: &Source,
        cursor: Option<Cursor>,
        sink: &mut dyn EventSink,
    ) -> Result<Cursor> {
        let mut st: State = Cursor::state_of(cursor.as_ref());
        let compressed = src.path.extension().is_some_and(|e| e == "zst");
        let mut lines = Lines::open(&src.path, &st.pos, compressed)?;
        if lines.reset {
            st = State::default();
        }
        let mut launch = Launches::resume(st.launch.take(), st.pos.line);
        let fallback = id_from_name(&src.path).unwrap_or_else(|| src.key.clone());
        launch.report(
            false,
            sink,
            &st.asid.clone().unwrap_or_else(|| fallback.clone()),
        );
        let mut meta = SessionMeta {
            transcript_path: Some(src.path.display().to_string()),
            ..SessionMeta::default()
        };
        let mut reported = false;
        lines.for_each(|ix, raw| {
            let v: Value = match serde_json::from_slice(raw) {
                Ok(v) => v,
                Err(e) => {
                    sink.warn(&format!("skipping unparseable line: {e}"));
                    return Ok(());
                }
            };
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
            let p = v.get("payload").unwrap_or(&Value::Null);
            if ty == "session_meta" && st.asid.is_none() {
                st.asid = p
                    .get("id")
                    .or_else(|| p.get("session_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            let before = launch.is_headless();
            launch_line(&v, &mut launch);
            launch.report(
                before,
                sink,
                &st.asid.clone().unwrap_or_else(|| fallback.clone()),
            );
            let ts = v.get("timestamp").and_then(parse_ts);
            if let Some(t) = ts {
                meta.started_at = Some(meta.started_at.map_or(t, |s| s.min(t)));
            }
            match ty {
                "session_meta" => {
                    if let Some(c) = p.get("cwd").and_then(Value::as_str) {
                        meta.cwd = Some(c.to_string());
                    }
                    let git = p.get("git");
                    if let Some(b) = git.and_then(|g| g.get("branch")).and_then(Value::as_str) {
                        meta.branch = Some(b.to_string());
                    }
                    if let Some(u) = git
                        .and_then(|g| g.get("repository_url"))
                        .and_then(Value::as_str)
                    {
                        meta.git_remote = Some(u.to_string());
                    }
                    let asid = st.asid.clone().unwrap_or_else(|| fallback.clone());
                    super::report_cwd(sink, &asid, &meta, &mut reported);
                    return Ok(());
                }
                "turn_context" => {
                    // session_meta's cwd (where the session started) wins.
                    if meta.cwd.is_none()
                        && let Some(c) = p.get("cwd").and_then(Value::as_str)
                    {
                        meta.cwd = Some(c.to_string());
                        let asid = st.asid.clone().unwrap_or_else(|| fallback.clone());
                        super::report_cwd(sink, &asid, &meta, &mut reported);
                    }
                    if let Some(m) = p.get("model").and_then(Value::as_str) {
                        st.model = Some(m.to_string());
                    }
                    return Ok(());
                }
                "token_usage_record" => {
                    if let Some(t) = p.get("thread_token_usage").and_then(tokens_of) {
                        st.usage = Some(t);
                    }
                    return Ok(());
                }
                "event_msg" => {
                    if p.get("type").and_then(Value::as_str) == Some("token_count")
                        && let Some(t) = p
                            .get("info")
                            .and_then(|i| i.get("total_token_usage"))
                            .and_then(tokens_of)
                    {
                        st.usage = Some(t);
                    }
                    return Ok(());
                }
                "response_item" => {}
                _ => return Ok(()),
            }
            let asid = st.asid.clone().unwrap_or_else(|| fallback.clone());
            let mut e = Emit::line(sink, &asid, ix);
            response_item(p, ts, &mut e, &mut st, &mut meta)
        })?;
        st.pos = lines.pos().clone();
        let asid = st.asid.clone().unwrap_or(fallback);
        if let Some(t) = st.usage {
            meta.tokens_in = Some(t.input);
            meta.tokens_out = Some(t.output);
            let usage = Usage {
                input: t.input - t.cached,
                output: t.output,
                cache_read: t.cached,
                cache_write: 0,
            };
            meta.cost_usd = Some(pricing::estimate(st.model.as_deref().unwrap_or(""), usage));
        }
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

fn response_item(
    p: &Value,
    ts: Option<i64>,
    e: &mut Emit<'_>,
    st: &mut State,
    meta: &mut SessionMeta,
) -> Result<()> {
    let s = |k: &str| p.get(k).and_then(Value::as_str);
    match s("type").unwrap_or("") {
        "message" => {
            let body = content_text(p.get("content").unwrap_or(&Value::Null));
            match s("role").unwrap_or("") {
                "user" => {
                    if is_injected(&body) {
                        e.text(ts, EventKind::System, &body, None)
                    } else {
                        if meta.first_prompt.is_none() && text::title_from_prompt(&body).is_some() {
                            meta.first_prompt = Some(body.clone());
                        }
                        e.text(ts, EventKind::User, &body, None)
                    }
                }
                "assistant" => e.text(
                    ts,
                    EventKind::Assistant,
                    &body,
                    st.model.as_ref().map(|m| json!({ "model": m })),
                ),
                // developer/system instructions are prompt plumbing.
                _ => Ok(()),
            }
        }
        "function_call" | "custom_tool_call" | "local_shell_call" => {
            let name = s("name").unwrap_or(if s("type") == Some("local_shell_call") {
                "shell"
            } else {
                "tool"
            });
            let call_id = s("call_id");
            let args = match p
                .get("arguments")
                .or_else(|| p.get("input"))
                .or_else(|| p.get("action"))
            {
                Some(Value::String(a)) => {
                    serde_json::from_str::<Value>(a).unwrap_or_else(|_| Value::String(a.clone()))
                }
                Some(v) => v.clone(),
                None => Value::Null,
            };
            if let Some(id) = call_id {
                st.calls.insert(id.to_string(), name.to_string());
            }
            e.tool_call(ts, name, call_id, &args)
        }
        "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
            let call_id = s("call_id");
            let name = call_id.and_then(|id| st.calls.remove(id));
            let out = output_text(p.get("output").unwrap_or(&Value::Null));
            e.tool_result(ts, name.as_deref(), call_id, &out, false, None)
        }
        "web_search_call" => {
            let action = p.get("action").unwrap_or(&Value::Null);
            e.tool_call(ts, "web_search", None, action)
        }
        "agent_message" => {
            let body = content_text(p.get("content").unwrap_or(&Value::Null));
            e.text(
                ts,
                EventKind::Assistant,
                &body,
                Some(json!({ "author": p.get("author"), "recipient": p.get("recipient") })),
            )
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_context_is_not_a_prompt() {
        assert!(is_injected(
            "<environment_context>
<cwd>/x</cwd>
</environment_context>"
        ));
        assert!(is_injected(
            "<recommended_plugins>
- a
</recommended_plugins>
"
        ));
        assert!(is_injected(
            "# AGENTS.md instructions for /x
..."
        ));
        assert!(!is_injected("fix <b>this</b> please"));
        assert!(!is_injected("<b>bold</b> and more text after"));
        assert_eq!(
            id_from_name(Path::new(
                "rollout-2026-01-01T00-00-00-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.jsonl"
            ))
            .as_deref(),
            Some("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b")
        );
    }
}
