//! pi (pi-mono coding agent): `~/.pi/agent/sessions/--<cwd>--/<ts>_<id>.jsonl`
//! (`$PI_CODING_AGENT_DIR/sessions` when set). Implemented from the
//! published session format (v3); pi is not installed on the reference
//! machine.
//!
//! Entries form a tree (branches, `/tree`); blirp keeps every entry in file
//! order, abandoned branches included, since history is permanent.

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
use std::path::{Path, PathBuf};

pub struct Pi {
    root: PathBuf,
}

impl Pi {
    pub fn new(env: &IngestEnv) -> Self {
        let base = env
            .var_path("PI_CODING_AGENT_DIR")
            .unwrap_or_else(|| env.home.join(".pi").join("agent"));
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
    tokens_in: i64,
    tokens_out: i64,
    cost: f64,
    model: Option<String>,
}

/// Session id from `<timestamp>_<id>.jsonl`.
fn id_from_path(p: &Path) -> Option<String> {
    let stem = p.file_stem()?.to_string_lossy().into_owned();
    Some(match stem.split_once('_') {
        Some((_, id)) if !id.is_empty() => id.to_string(),
        _ => stem,
    })
}

fn add_usage(u: &Value, st: &mut State, model: Option<&str>) {
    let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    let usage = Usage {
        input: n("input"),
        output: n("output"),
        cache_read: n("cacheRead"),
        cache_write: n("cacheWrite"),
    };
    st.tokens_in += usage.input + usage.cache_read + usage.cache_write;
    st.tokens_out += usage.output;
    st.cost += u
        .get("cost")
        .and_then(|c| c.get("total"))
        .and_then(Value::as_f64)
        .unwrap_or_else(|| pricing::estimate(model.unwrap_or(""), usage));
}

impl Adapter for Pi {
    fn id(&self) -> &'static str {
        "pi"
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn scan(&self, _store: &Store) -> Result<Vec<Source>> {
        Ok(file_sources(walk_files(&self.root, 2, &|p| {
            p.extension().is_some_and(|e| e == "jsonl")
        })))
    }

    fn ingest(
        &self,
        src: &Source,
        cursor: Option<Cursor>,
        sink: &mut dyn EventSink,
    ) -> Result<Cursor> {
        let mut st: State = Cursor::state_of(cursor.as_ref());
        let mut lines = Lines::open(&src.path, &st.pos, false)?;
        if lines.reset {
            st = State::default();
        }
        let fallback = id_from_path(&src.path).unwrap_or_else(|| src.key.clone());
        let mut meta = SessionMeta {
            transcript_path: Some(src.path.display().to_string()),
            ..SessionMeta::default()
        };
        lines.for_each(|ix, raw| {
            let v: Value = match serde_json::from_slice(raw) {
                Ok(v) => v,
                Err(e) => {
                    sink.warn(&format!("skipping unparseable line: {e}"));
                    return Ok(());
                }
            };
            let ts = v.get("timestamp").and_then(parse_ts);
            if let Some(t) = ts {
                meta.started_at = Some(meta.started_at.map_or(t, |s| s.min(t)));
            }
            let s = |k: &str| v.get(k).and_then(Value::as_str);
            match s("type").unwrap_or("") {
                "session" => {
                    if st.asid.is_none() {
                        st.asid = s("id").map(str::to_string);
                    }
                    meta.cwd = s("cwd").map(str::to_string);
                    meta.parent = s("parentSession").and_then(|p| id_from_path(Path::new(p)));
                    return Ok(());
                }
                "model_change" => {
                    st.model = s("modelId").map(str::to_string);
                    return Ok(());
                }
                "session_info" => {
                    if let Some(n) = s("name") {
                        meta.title = Some(n.to_string());
                    }
                    return Ok(());
                }
                "usage" => {
                    if let Some(u) = v.get("usage") {
                        let model = s("model").map(str::to_string).or_else(|| st.model.clone());
                        add_usage(u, &mut st, model.as_deref());
                    }
                    return Ok(());
                }
                _ => {}
            }
            let asid = st.asid.clone().unwrap_or_else(|| fallback.clone());
            let mut e = Emit::line(sink, &asid, ix);
            match s("type").unwrap_or("") {
                "message" => message(
                    v.get("message").unwrap_or(&Value::Null),
                    ts,
                    &mut e,
                    &mut st,
                    &mut meta,
                ),
                "compaction" | "branch_summary" => {
                    e.text(ts, EventKind::Summary, s("summary").unwrap_or(""), None)?;
                    if let Some(u) = v.get("usage") {
                        let model = st.model.clone();
                        add_usage(u, &mut st, model.as_deref());
                    }
                    Ok(())
                }
                "custom_message" => e.text(
                    ts,
                    EventKind::System,
                    &content_text(v.get("content").unwrap_or(&Value::Null)),
                    Some(json!({ "custom_type": v.get("customType") })),
                ),
                _ => Ok(()),
            }
        })?;
        st.pos = lines.pos().clone();
        let asid = st.asid.clone().unwrap_or(fallback);
        meta.tokens_in = Some(st.tokens_in);
        meta.tokens_out = Some(st.tokens_out);
        meta.cost_usd = Some(st.cost);
        meta.model = st.model.clone();
        sink.session(&asid, meta);
        let mut c = Cursor::from_state(&st)?;
        c.retry = lines.partial;
        Ok(c)
    }
}

fn message(
    m: &Value,
    ts: Option<i64>,
    e: &mut Emit<'_>,
    st: &mut State,
    meta: &mut SessionMeta,
) -> Result<()> {
    let ts = m.get("timestamp").and_then(parse_ts).or(ts);
    let content = m.get("content").unwrap_or(&Value::Null);
    match m.get("role").and_then(Value::as_str).unwrap_or("") {
        "user" => {
            let body = content_text(content);
            if meta.first_prompt.is_none() && text::title_from_prompt(&body).is_some() {
                meta.first_prompt = Some(body.clone());
            }
            e.text(ts, EventKind::User, &body, None)
        }
        "assistant" => {
            let model = m.get("model").and_then(Value::as_str).map(str::to_string);
            if model.is_some() {
                st.model.clone_from(&model);
            }
            if let Some(u) = m.get("usage") {
                add_usage(u, st, model.as_deref());
            }
            if let Value::Array(blocks) = content {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text" => e.text(
                            ts,
                            EventKind::Assistant,
                            b.get("text").and_then(Value::as_str).unwrap_or(""),
                            model.as_ref().map(|m| json!({ "model": m })),
                        )?,
                        "toolCall" => e.tool_call(
                            ts,
                            b.get("name").and_then(Value::as_str).unwrap_or("tool"),
                            b.get("id").and_then(Value::as_str),
                            b.get("arguments").unwrap_or(&Value::Null),
                        )?,
                        _ => {}
                    }
                }
            }
            if let Some(err) = m.get("errorMessage").and_then(Value::as_str) {
                e.text(ts, EventKind::System, &format!("error: {err}"), None)?;
            }
            Ok(())
        }
        "toolResult" => e.tool_result(
            ts,
            m.get("toolName").and_then(Value::as_str),
            m.get("toolCallId").and_then(Value::as_str),
            &content_text(content),
            m.get("isError").and_then(Value::as_bool).unwrap_or(false),
            None,
        ),
        "bashExecution" => {
            let cmd = m.get("command").and_then(Value::as_str).unwrap_or("");
            e.tool_call(ts, "bash", None, &json!({ "command": cmd }))?;
            let out = m.get("output").and_then(Value::as_str).unwrap_or("");
            let failed = m
                .get("exitCode")
                .and_then(Value::as_i64)
                .is_some_and(|c| c != 0);
            e.tool_result(
                ts,
                Some("bash"),
                None,
                out,
                failed,
                Some(json!({ "user_initiated": true })),
            )
        }
        "custom" => e.text(ts, EventKind::System, &content_text(content), None),
        _ => Ok(()),
    }
}
