//! DeepSeek Harness (`dsh`): `~/.dsh/sessions/<encoded-cwd>/session-<id>/
//! session.v3.jsonl.zstd` (`$DSH_HOME/sessions` when set). The zstd event
//! log was decoded and inspected on the reference machine: line 1 is
//! `{type:"session", id, cwd, createdAt, ...}`, then `{type, seq, time,
//! data}` events. Used: `user/message`, `assistant/message` (text; usage),
//! `tool/call` (file edits from edit/write arguments), `tool/result`,
//! `system/message`, `session/title`. Other event types are lifecycle
//! bookkeeping and skipped; other log versions are skipped with one log.

use super::emit::Emit;
use super::jsonl::{FilePos, Lines};
use super::pricing::{self, Usage};
use super::text::{self, content_text, parse_ts};
use super::{Adapter, Cursor, EventSink, IngestEnv, Result, SessionMeta, Source, walk_files};
use blirp_core::model::EventKind;
use blirp_core::store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const LOG_NAME: &str = "session.v3.jsonl.zstd";

pub struct Dsh {
    root: PathBuf,
}

impl Dsh {
    pub fn new(env: &IngestEnv) -> Self {
        let base = env
            .var_path("DSH_HOME")
            .unwrap_or_else(|| env.home.join(".dsh"));
        Self {
            root: base.join("sessions"),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(flatten)]
    pos: FilePos,
    asid: Option<String>,
    cwd: Option<String>,
    started_at: Option<i64>,
    tokens_in: i64,
    tokens_out: i64,
    cost: f64,
    model: Option<String>,
    #[serde(default)]
    calls: HashMap<String, String>,
}

fn is_session_log(p: &Path) -> bool {
    p.file_name().is_some_and(|n| {
        let n = n.to_string_lossy();
        n.starts_with("session.v") && n.ends_with(".jsonl.zstd")
    })
}

impl Adapter for Dsh {
    fn id(&self) -> &'static str {
        "dsh"
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn scan(&self, _store: &Store) -> Result<Vec<Source>> {
        let mut out = Vec::new();
        for p in walk_files(&self.root, 3, &is_session_log) {
            if p.file_name().is_some_and(|n| n == LOG_NAME) {
                out.extend(Source::file(&p));
            } else {
                tracing::debug!(path = %p.display(), "skipping dsh log of an unknown version");
            }
        }
        Ok(out)
    }

    fn ingest(
        &self,
        src: &Source,
        cursor: Option<Cursor>,
        sink: &mut dyn EventSink,
    ) -> Result<Cursor> {
        let mut st: State = Cursor::state_of(cursor.as_ref());
        let mut lines = Lines::open(&src.path, &st.pos, true)?;
        if lines.reset {
            st = State::default();
        }
        let fallback = src
            .path
            .parent()
            .and_then(Path::file_name)
            .map(|n| {
                n.to_string_lossy()
                    .trim_start_matches("session-")
                    .to_string()
            })
            .unwrap_or_else(|| src.key.clone());
        let mut meta = SessionMeta {
            transcript_path: Some(src.path.display().to_string()),
            ..SessionMeta::default()
        };
        lines.for_each(|ix, raw| {
            let v: Value = match serde_json::from_slice(raw) {
                Ok(v) => v,
                Err(e) => {
                    sink.warn(&format!("skipping unparseable event: {e}"));
                    return Ok(());
                }
            };
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
            if ty == "session" {
                if st.asid.is_none() {
                    st.asid = v.get("id").and_then(Value::as_str).map(str::to_string);
                }
                st.cwd = v.get("cwd").and_then(Value::as_str).map(str::to_string);
                st.started_at = v.get("createdAt").and_then(parse_ts);
                return Ok(());
            }
            let ts = v.get("time").and_then(parse_ts);
            let d = v.get("data").unwrap_or(&Value::Null);
            let asid = st.asid.clone().unwrap_or_else(|| fallback.clone());
            let mut e = Emit::line(sink, &asid, ix);
            event(ty, d, ts, &mut e, &mut st, &mut meta)
        })?;
        st.pos = lines.pos().clone();
        let asid = st.asid.clone().unwrap_or(fallback);
        meta.cwd.clone_from(&st.cwd);
        meta.started_at = st.started_at;
        meta.tokens_in = Some(st.tokens_in);
        meta.tokens_out = Some(st.tokens_out);
        meta.cost_usd = Some(st.cost);
        meta.model.clone_from(&st.model);
        sink.session(&asid, meta);
        let mut c = Cursor::from_state(&st)?;
        c.retry = lines.partial;
        Ok(c)
    }
}

fn event(
    ty: &str,
    d: &Value,
    ts: Option<i64>,
    e: &mut Emit<'_>,
    st: &mut State,
    meta: &mut SessionMeta,
) -> Result<()> {
    match ty {
        "user/message" => {
            let body = content_text(d.get("content").unwrap_or(&Value::Null));
            let from_user = d
                .get("source")
                .and_then(|s| s.get("kind"))
                .and_then(Value::as_str)
                == Some("user");
            if from_user {
                if meta.first_prompt.is_none() && text::title_from_prompt(&body).is_some() {
                    meta.first_prompt = Some(body.clone());
                }
                e.text(ts, EventKind::User, &body, None)
            } else {
                // Plugin/skill catalog context injected as a user turn.
                e.text(ts, EventKind::System, &body, None)
            }
        }
        "assistant/message" => {
            let msg = d.get("message").unwrap_or(&Value::Null);
            let model = msg
                .get("source")
                .and_then(|s| s.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if model.is_some() {
                st.model.clone_from(&model);
            }
            if let Some(u) = d.get("usage") {
                let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
                let (input, cached, output) =
                    (n("inputTokens"), n("cacheReadTokens"), n("outputTokens"));
                st.tokens_in += input;
                st.tokens_out += output;
                st.cost += pricing::estimate(
                    model.as_deref().unwrap_or(""),
                    Usage {
                        input: (input - cached).max(0),
                        output,
                        cache_read: cached,
                        cache_write: 0,
                    },
                );
            }
            // Tool calls arrive as their own `tool/call` events.
            if let Some(Value::Array(blocks)) = msg.get("content") {
                for b in blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                {
                    e.text(
                        ts,
                        EventKind::Assistant,
                        b.get("text").and_then(Value::as_str).unwrap_or(""),
                        model.as_ref().map(|m| json!({ "model": m })),
                    )?;
                }
            }
            Ok(())
        }
        "tool/call" => {
            let name = d.get("name").and_then(Value::as_str).unwrap_or("tool");
            let id = d.get("callId").and_then(Value::as_str);
            if let Some(id) = id {
                st.calls.insert(id.to_string(), name.to_string());
            }
            e.tool_call(ts, name, id, d.get("arguments").unwrap_or(&Value::Null))
        }
        "tool/result" => {
            let msg = d.get("message").unwrap_or(&Value::Null);
            if let Some(Value::Array(blocks)) = msg.get("content") {
                for b in blocks {
                    let id = b.get("toolCallId").and_then(Value::as_str);
                    let name = id.and_then(|i| st.calls.remove(i));
                    let out = content_text(b.get("content").unwrap_or(&Value::Null));
                    let is_error = b.get("isError").and_then(Value::as_bool).unwrap_or(false)
                        || d.get("error").is_some();
                    e.tool_result(ts, name.as_deref(), id, &out, is_error, None)?;
                }
            }
            // `meta.diffs` repeats the edit/write call's path, which
            // `tool_call` already recorded as a file_edit.
            Ok(())
        }
        "system/message" => {
            let body = content_text(
                d.get("message")
                    .and_then(|m| m.get("content"))
                    .unwrap_or(&Value::Null),
            );
            e.text(ts, EventKind::System, &body, None)
        }
        "session/title" => {
            if let Some(t) = d.get("title").and_then(Value::as_str) {
                meta.title = Some(t.to_string());
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
