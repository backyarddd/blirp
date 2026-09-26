//! opencode: SQLite `opencode*.db` under `$XDG_DATA_HOME/opencode` or
//! `~/.local/share/opencode` (Windows also `%APPDATA%\opencode`). Verified
//! against real data (schema `session` -> `message` -> `part`).
//!
//! Each opencode session is one [`Source`] (`<db>#<session id>`),
//! fingerprinted by `session.time_updated`. The database is live: it is
//! opened read-only (`mode=ro`, not `immutable`) and `SQLITE_BUSY` is
//! retried. Messages are consumed in `(time_created, id)` order; an
//! assistant message still streaming stops the read until it completes.
//! Child sessions (`parent_id`, opencode's subagents) link to their parent.
//! The pre-SQLite JSON `storage/` tree is not read (opencode migrates it
//! into the database on upgrade).

use super::emit::Emit;
use super::text::{self, parse_ts};
use super::{Adapter, Cursor, EventSink, IngestEnv, Result, SessionMeta, Source};
use blirp_core::model::EventKind;
use blirp_core::store::Store;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Opencode {
    dirs: Vec<PathBuf>,
}

impl Opencode {
    pub fn new(env: &IngestEnv) -> Self {
        let mut dirs = vec![env.data_home().join("opencode")];
        let local = env.home.join(".local").join("share").join("opencode");
        if !dirs.contains(&local) {
            dirs.push(local);
        }
        if let Some(a) = env.appdata() {
            dirs.push(a.join("opencode"));
        }
        Self { dirs }
    }

    fn databases(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for d in &self.dirs {
            let Ok(rd) = std::fs::read_dir(d) else {
                continue;
            };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("opencode") && name.ends_with(".db") {
                    out.push(e.path());
                }
            }
        }
        out
    }
}

/// Stop waiting after this long on a locked database.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const BUSY_RETRIES: u32 = 5;
/// A message that never completed (crash) is ingested once the session has
/// been quiet this long.
const ABANDONED_MS: i64 = 10 * 60_000;

pub(crate) fn open_ro(path: &Path) -> Result<Connection> {
    let raw = path
        .to_string_lossy()
        .replace('\\', "/")
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    // `file:///C:/x` on Windows, `file:///x` on unix.
    let uri = format!(
        "file://{}{raw}?mode=ro",
        if raw.starts_with('/') { "" } else { "/" }
    );
    let c = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    c.busy_timeout(BUSY_TIMEOUT)?;
    Ok(c)
}

/// Run `f`, retrying while the database reports busy/locked.
pub(crate) fn retry_busy<T>(mut f: impl FnMut() -> rusqlite::Result<T>) -> Result<T> {
    let mut attempt = 0;
    loop {
        match f() {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && attempt < BUSY_RETRIES =>
            {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(100 * u64::from(attempt)));
            }
            r => return Ok(r?),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Last consumed message (time_created, id).
    last_time: i64,
    last_id: String,
    next_seq: i64,
}

impl Adapter for Opencode {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.dirs.clone()
    }

    fn scan(&self, _store: &Store) -> Result<Vec<Source>> {
        let mut out = Vec::new();
        for db in self.databases() {
            let conn = open_ro(&db)?;
            let rows: Vec<(String, i64)> = retry_busy(|| {
                let mut st = conn.prepare("SELECT id, time_updated FROM session")?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect()
            })?;
            let key = db.to_string_lossy().into_owned();
            out.extend(rows.into_iter().map(|(id, updated)| Source {
                key: format!("{key}#{id}"),
                path: db.clone(),
                fingerprint: updated.to_string(),
                mtime_ms: updated,
                item: Some(id),
            }));
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
        let Some(sid) = src.item.as_deref() else {
            return Cursor::from_state(&st);
        };
        let db = open_ro(&src.path)?;
        // One read transaction: session, messages and parts from the same
        // snapshot, so a message is never seen without parts that were
        // committed together with it.
        let conn = retry_busy(|| db.unchecked_transaction())?;
        let row = retry_busy(|| {
            conn.query_row(
                "SELECT parent_id, directory, title, time_created, time_updated, cost,
                   tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, model
                 FROM session WHERE id = ?1",
                params![sid],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, f64>(5)?,
                        [r.get::<_, i64>(6)?, r.get::<_, i64>(7)?, r.get::<_, i64>(8)?, r.get::<_, i64>(9)?, r.get::<_, i64>(10)?],
                        r.get::<_, Option<String>>(11)?,
                    ))
                },
            )
            .optional()
        })?;
        let Some((parent, dir, title, created, updated, cost, tok, model)) = row else {
            // Deleted in opencode; blirp keeps what it has.
            return Cursor::from_state(&st);
        };
        let placeholder = title.starts_with("New session") || title.starts_with("Child session");
        let mut meta = SessionMeta {
            cwd: Some(dir),
            title: (!placeholder && !title.trim().is_empty()).then_some(title),
            started_at: Some(created),
            last_activity: Some(updated),
            tokens_in: Some(tok[0] + tok[3] + tok[4]),
            tokens_out: Some(tok[1] + tok[2]),
            cost_usd: Some(cost),
            parent,
            model: model.and_then(|m| {
                serde_json::from_str::<Value>(&m)
                    .ok()
                    .and_then(|v| v.get("modelID").and_then(Value::as_str).map(str::to_string))
                    .or(Some(m))
            }),
            transcript_path: Some(format!("{}#{sid}", src.path.display())),
            ..SessionMeta::default()
        };
        super::report_cwd(sink, sid, &meta, &mut false);
        let messages: Vec<(String, i64, String)> = retry_busy(|| {
            let mut q = conn.prepare(
                "SELECT id, time_created, data FROM message
                 WHERE session_id = ?1 AND (time_created > ?2 OR (time_created = ?2 AND id > ?3))
                 ORDER BY time_created, id",
            )?;
            let rows = q.query_map(params![sid, st.last_time, st.last_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            rows.collect()
        })?;
        let now = blirp_core::now_ms();
        let mut retry = false;
        let count = messages.len();
        for (i, (mid, mtime, data)) in messages.into_iter().enumerate() {
            let m: Value = match serde_json::from_str(&data) {
                Ok(v) => v,
                Err(e) => {
                    sink.warn(&format!("skipping unparseable message: {e}"));
                    st.last_time = mtime;
                    st.last_id = mid;
                    continue;
                }
            };
            let role = if m.get("summary").and_then(Value::as_bool) == Some(true) {
                "summary"
            } else {
                m.get("role").and_then(Value::as_str).unwrap_or("")
            };
            let complete = !matches!(role, "assistant" | "summary")
                || m.get("time")
                    .and_then(|t| t.get("completed"))
                    .is_some_and(|c| !c.is_null())
                || m.get("finish").is_some_and(|f| !f.is_null())
                || i + 1 < count
                || now - updated > ABANDONED_MS;
            if !complete {
                retry = true;
                break;
            }
            let parts: Vec<String> = retry_busy(|| {
                let mut q =
                    conn.prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY id")?;
                let rows = q.query_map(params![mid], |r| r.get(0))?;
                rows.collect()
            })?;
            // opencode writes a message before its parts: an empty one is
            // still being written, unless it was left that way long ago.
            if parts.is_empty() && now - mtime <= ABANDONED_MS {
                retry = true;
                break;
            }
            let ts = m
                .get("time")
                .and_then(|t| t.get("created"))
                .and_then(parse_ts)
                .or(Some(mtime));
            let mut e = Emit::counter(sink, sid, st.next_seq);
            for p in &parts {
                let Ok(part) = serde_json::from_str::<Value>(p) else {
                    e.sink().warn("skipping unparseable part");
                    continue;
                };
                self::part(&part, role, ts, &mut e, &mut meta)?;
            }
            st.next_seq = e.next_seq();
            st.last_time = mtime;
            st.last_id = mid;
        }
        sink.session(sid, meta);
        let mut c = Cursor::from_state(&st)?;
        c.retry = retry;
        Ok(c)
    }
}

fn part(
    p: &Value,
    role: &str,
    ts: Option<i64>,
    e: &mut Emit<'_>,
    meta: &mut SessionMeta,
) -> Result<()> {
    let s = |k: &str| p.get(k).and_then(Value::as_str);
    match s("type").unwrap_or("") {
        "text" => {
            if p.get("synthetic").and_then(Value::as_bool) == Some(true)
                || p.get("ignored").and_then(Value::as_bool) == Some(true)
            {
                return Ok(());
            }
            let body = s("text").unwrap_or("");
            if role == "summary" {
                e.text(ts, EventKind::Summary, body, None)
            } else if role == "user" {
                if meta.first_prompt.is_none() && text::title_from_prompt(body).is_some() {
                    meta.first_prompt = Some(body.to_string());
                }
                e.text(ts, EventKind::User, body, None)
            } else {
                e.text(ts, EventKind::Assistant, body, None)
            }
        }
        "file" => {
            let name = s("filename").or(s("url")).unwrap_or("file");
            e.text(ts, EventKind::User, &format!("[file: {name}]"), None)
        }
        "tool" => {
            let tool = s("tool").unwrap_or("tool");
            let call_id = s("callID");
            let state = p.get("state").unwrap_or(&Value::Null);
            let ts = state
                .get("time")
                .and_then(|t| t.get("start"))
                .and_then(parse_ts)
                .or(ts);
            e.tool_call(
                ts,
                tool,
                call_id,
                state.get("input").unwrap_or(&Value::Null),
            )?;
            match state.get("status").and_then(Value::as_str) {
                Some("completed") => {
                    let out = state.get("output").and_then(Value::as_str).unwrap_or("");
                    e.tool_result(
                        ts,
                        Some(tool),
                        call_id,
                        out,
                        false,
                        state.get("title").map(|t| json!({ "title": t })),
                    )
                }
                Some("error") => {
                    let err = state.get("error").and_then(Value::as_str).unwrap_or("");
                    e.tool_result(ts, Some(tool), call_id, err, true, None)
                }
                _ => Ok(()),
            }
        }
        // `patch` parts are snapshots of files the step's tool calls
        // changed; those calls already produced the file_edit events.
        "compaction" => e.text(ts, EventKind::System, "conversation compacted", None),
        _ => Ok(()),
    }
}
