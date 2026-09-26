//! Amp: one JSON file per thread, `~/.local/share/amp/threads/T-*.json`
//! (`$AMP_DATA_DIR/threads` when set). Implemented from public
//! descriptions of the thread format; Amp is not installed on the reference
//! machine.
//!
//! Amp rewrites the whole file, so the cursor is the number of messages
//! consumed and seqs are `message_index * 1024 + n`. A trailing assistant
//! message that is still streaming is left for the next change.

use super::emit::Emit;
use super::pricing::{self, Usage};
use super::text::{self, content_text, parse_ts};
use super::{
    Adapter, Cursor, EventSink, IngestEnv, Result, SessionMeta, Source, file_sources, walk_files,
};
use blirp_core::model::EventKind;
use blirp_core::store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

pub struct Amp {
    threads: Vec<PathBuf>,
}

impl Amp {
    pub fn new(env: &IngestEnv) -> Self {
        let mut threads = vec![
            env.var_path("AMP_DATA_DIR")
                .unwrap_or_else(|| env.data_home().join("amp"))
                .join("threads"),
        ];
        if let Some(a) = env.appdata() {
            threads.push(a.join("amp").join("threads"));
        }
        Self { threads }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    messages: u64,
}

/// `file:///C:/x` or `file:///home/x` -> a local path string.
fn file_uri_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    let bytes = decoded.as_bytes();
    // Windows drive: "/C:/..." -> "C:/..."
    if bytes.len() > 3 && bytes[0] == b'/' && bytes[2] == b':' && bytes[1].is_ascii_alphabetic() {
        return Some(decoded[1..].replace('/', "\\"));
    }
    Some(decoded)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            let v = (h * 16 + l) as u8;
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn thread_cwd(v: &Value) -> Option<String> {
    let trees = v.get("env")?.get("initial")?.get("trees")?.as_array()?;
    trees
        .iter()
        .find_map(|t| t.get("uri").and_then(Value::as_str))
        .and_then(file_uri_path)
}

impl Adapter for Amp {
    fn id(&self) -> &'static str {
        "amp"
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.threads.clone()
    }

    fn scan(&self, _store: &Store) -> Result<Vec<Source>> {
        let mut files = Vec::new();
        for t in &self.threads {
            files.extend(walk_files(t, 0, &|p| {
                p.extension().is_some_and(|e| e == "json")
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("T-"))
            }));
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
        let v: Value = match serde_json::from_slice(&std::fs::read(&src.path)?) {
            Ok(v) => v,
            Err(e) => {
                // Usually a write in progress; the next change retries.
                sink.warn(&format!("unreadable thread file: {e}"));
                let mut c = Cursor::from_state(&st)?;
                c.retry = true;
                return Ok(c);
            }
        };
        let asid = v
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                src.path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| src.key.clone());
        let created = v.get("created").and_then(parse_ts);
        let mut meta = SessionMeta {
            cwd: thread_cwd(&v),
            title: v.get("title").and_then(Value::as_str).map(str::to_string),
            started_at: created,
            transcript_path: Some(src.path.display().to_string()),
            ..SessionMeta::default()
        };
        let empty = Vec::new();
        let msgs = v
            .get("messages")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        if (msgs.len() as u64) < st.messages {
            // Edited/truncated thread: continue after what is left.
            st.messages = msgs.len() as u64;
        }
        let (mut tin, mut tout, mut cost, mut model) = (0i64, 0i64, 0f64, None::<String>);
        let mut retry = false;
        for (i, m) in msgs.iter().enumerate() {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("");
            if let Some(u) = m.get("usage").filter(|u| u.is_object()) {
                let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
                let mdl = u.get("model").and_then(Value::as_str).unwrap_or("");
                let usage = Usage {
                    input: n("inputTokens"),
                    output: n("outputTokens"),
                    cache_read: n("cacheReadInputTokens"),
                    cache_write: n("cacheCreationInputTokens"),
                };
                tin += usage.input + usage.cache_read + usage.cache_write;
                tout += usage.output;
                cost += pricing::estimate(mdl, usage);
                if !mdl.is_empty() {
                    model = Some(mdl.to_string());
                }
            }
            if role == "user" && meta.first_prompt.is_none() {
                let body = text_blocks(m.get("content"));
                if text::title_from_prompt(&body).is_some() {
                    meta.first_prompt = Some(body);
                }
            }
            if (i as u64) < st.messages {
                continue;
            }
            let streaming = m
                .get("state")
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|t| t == "streaming");
            if streaming && i + 1 == msgs.len() {
                retry = true;
                break;
            }
            let ts = m
                .get("meta")
                .and_then(|x| x.get("sentAt"))
                .and_then(parse_ts)
                .or_else(|| {
                    m.get("usage")
                        .and_then(|u| u.get("timestamp"))
                        .and_then(parse_ts)
                })
                .or(created);
            let mut e = Emit::line(sink, &asid, i as u64);
            message(m, role, ts, &mut e)?;
            st.messages = i as u64 + 1;
        }
        meta.tokens_in = Some(tin);
        meta.tokens_out = Some(tout);
        meta.cost_usd = Some(cost);
        meta.model = model;
        sink.session(&asid, meta);
        let mut c = Cursor::from_state(&st)?;
        c.retry = retry;
        Ok(c)
    }
}

fn text_blocks(content: Option<&Value>) -> String {
    content.map(content_text).unwrap_or_default()
}

fn message(m: &Value, role: &str, ts: Option<i64>, e: &mut Emit<'_>) -> Result<()> {
    let Some(Value::Array(blocks)) = m.get("content") else {
        let body = text_blocks(m.get("content"));
        let kind = if role == "user" {
            EventKind::User
        } else {
            EventKind::Assistant
        };
        return e.text(ts, kind, &body, None);
    };
    for b in blocks {
        let s = |k: &str| b.get(k).and_then(Value::as_str);
        match s("type").unwrap_or("") {
            "text" => {
                let kind = if role == "user" {
                    EventKind::User
                } else {
                    EventKind::Assistant
                };
                e.text(ts, kind, s("text").unwrap_or(""), None)?;
            }
            "tool_use" => e.tool_call(
                ts,
                s("name").unwrap_or("tool"),
                s("id"),
                b.get("input").unwrap_or(&Value::Null),
            )?,
            "tool_result" => {
                let id = s("toolUseID").or(s("tool_use_id"));
                let run = b.get("run");
                let status = run.and_then(|r| r.get("status")).and_then(Value::as_str);
                let out = match run.and_then(|r| r.get("result").or_else(|| r.get("error"))) {
                    Some(Value::String(t)) => t.clone(),
                    Some(v) => {
                        let t = content_text(v);
                        if t.is_empty() { v.to_string() } else { t }
                    }
                    None => text_blocks(b.get("content")),
                };
                let is_error = status == Some("error")
                    || b.get("is_error").and_then(Value::as_bool) == Some(true);
                e.tool_result(
                    ts,
                    None,
                    id,
                    &out,
                    is_error,
                    status.map(|st| json!({ "status": st })),
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
    fn file_uris() {
        assert_eq!(
            file_uri_path("file:///home/u/my%20app").as_deref(),
            Some("/home/u/my app")
        );
        assert_eq!(
            file_uri_path("file:///C:/Users/u/x").as_deref(),
            Some("C:\\Users\\u\\x")
        );
        assert_eq!(file_uri_path("https://x"), None);
    }
}
