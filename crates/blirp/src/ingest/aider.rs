//! Aider: `.aider.chat.history.md` in each registered project folder of
//! this machine. Implemented from aider's documented history format; aider
//! is not installed on the reference machine.
//!
//! Each `# aider chat started at <local time>` heading starts a session.
//! `#### ` lines are the user's prompt, `> ` lines are aider's own output
//! (edits applied, commits, token/cost reports), everything else is the
//! model's reply. A block is stored once the next block starts, so the read
//! position rewinds to the start of an unfinished block. These files live
//! in project folders, which are not watched: they are picked up by the
//! periodic rescan.

use super::emit::Emit;
use super::jsonl::{FilePos, Lines, STALE_TAIL_MS};
use super::text::{self, fnv64};
use super::{Adapter, Cursor, EventSink, IngestEnv, Result, SessionMeta, Source, file_sources};
use blirp_core::model::EventKind;
use blirp_core::store::Store;
use chrono::TimeZone as _;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const FILE: &str = ".aider.chat.history.md";
const HEADER: &str = "# aider chat started at ";

pub struct Aider;

impl Aider {
    pub fn new(_env: &IngestEnv) -> Self {
        Aider
    }
}

/// Settings key under which the daemon stores this machine's id.
const MACHINE_ID_KEY: &str = "machine_id";

/// One stored block: its first line and the events it produces.
type Group = (u64, Vec<(EventKind, String)>);

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(flatten)]
    pos: FilePos,
    asid: Option<String>,
    started_at: Option<i64>,
    first_prompt: Option<String>,
    tokens_in: i64,
    tokens_out: i64,
    cost: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Block {
    User,
    Output,
    Reply,
}

struct Open {
    kind: Block,
    line: u64,
    offset: u64,
    text: String,
}

/// Local wall-clock `YYYY-MM-DD HH:MM:SS` -> unix ms.
fn local_ts(s: &str) -> Option<i64> {
    let naive = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|d| d.timestamp_millis())
}

/// `2.3k` -> 2300, `1.2M` -> 1200000, `150` -> 150.
fn count(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches(',');
    let (num, mult) = match s.chars().last()? {
        'k' | 'K' => (&s[..s.len() - 1], 1e3),
        'm' | 'M' => (&s[..s.len() - 1], 1e6),
        _ => (s, 1.0),
    };
    num.parse::<f64>().ok().map(|n| (n * mult).round() as i64)
}

/// `Tokens: 2.3k sent, 150 received. Cost: $0.01 message, $0.05 session.`
fn parse_tokens(line: &str, st: &mut State) {
    let Some(rest) = line.strip_prefix("Tokens: ") else {
        return;
    };
    let words: Vec<&str> = rest.split_whitespace().collect();
    for w in words.windows(2) {
        match w[1].trim_end_matches([',', '.']) {
            "sent" => st.tokens_in += count(w[0]).unwrap_or(0),
            "received" => st.tokens_out += count(w[0]).unwrap_or(0),
            "session" => {
                if let Ok(c) = w[0].trim_start_matches('$').parse::<f64>() {
                    st.cost = Some(c);
                }
            }
            _ => {}
        }
    }
}

impl Adapter for Aider {
    fn id(&self) -> &'static str {
        "aider"
    }

    fn roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    fn scan(&self, store: &Store) -> Result<Vec<Source>> {
        let Some(machine_id) = store
            .get_setting(MACHINE_ID_KEY)?
            .and_then(|v| v.as_str().map(str::to_string))
        else {
            return Ok(Vec::new());
        };
        let paths = store.local_project_paths(&machine_id)?;
        Ok(file_sources(paths.into_iter().map(|p| p.join(FILE))))
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
        let cwd = src.path.parent().map(|p| p.display().to_string());
        let path_id = fnv64(src.key.as_bytes());
        let mut open: Option<Open> = None;
        // Blocks of the current session not yet handed to the sink.
        let mut buf: Vec<Group> = Vec::new();

        let emit_session = |sink: &mut dyn EventSink,
                            st: &State,
                            buf: &mut Vec<Group>,
                            ts: Option<i64>|
         -> Result<()> {
            let Some(asid) = st.asid.clone() else {
                buf.clear();
                return Ok(());
            };
            for (line, events) in buf.drain(..) {
                let mut e = Emit::line(sink, &asid, line);
                for (kind, body) in events {
                    if kind == EventKind::FileEdit {
                        e.file_edit(ts, &body, Some("aider"), None)?;
                    } else {
                        e.text(ts, kind, &body, None)?;
                    }
                }
            }
            sink.session(
                &asid,
                SessionMeta {
                    cwd: cwd.clone(),
                    first_prompt: st.first_prompt.clone(),
                    started_at: st.started_at,
                    tokens_in: Some(st.tokens_in),
                    tokens_out: Some(st.tokens_out),
                    cost_usd: st.cost,
                    transcript_path: Some(src.path.display().to_string()),
                    ..SessionMeta::default()
                },
            );
            Ok(())
        };

        let close = |open: &mut Option<Open>, st: &mut State, buf: &mut Vec<Group>| {
            let Some(o) = open.take() else { return };
            let body = o.text.trim_end().to_string();
            if body.trim().is_empty() {
                return;
            }
            match o.kind {
                Block::User => {
                    if st.first_prompt.is_none() && text::title_from_prompt(&body).is_some() {
                        st.first_prompt = Some(body.clone());
                    }
                    buf.push((o.line, vec![(EventKind::User, body)]));
                }
                Block::Reply => buf.push((o.line, vec![(EventKind::Assistant, body)])),
                Block::Output => {
                    let mut events = Vec::new();
                    for l in body.lines() {
                        parse_tokens(l.trim(), st);
                        if let Some(p) = l.trim().strip_prefix("Applied edit to ") {
                            events.push((EventKind::FileEdit, p.trim().to_string()));
                        }
                    }
                    events.insert(0, (EventKind::System, body));
                    buf.push((o.line, events));
                }
            }
        };

        // Inside a ``` fence of a reply, `>`/`####` lines are code (e.g.
        // `>>>>>>> REPLACE` markers), not aider output.
        let mut in_fence = false;
        lines.for_each_at(|ix, offset, raw| {
            let line = String::from_utf8_lossy(raw);
            if let Some(when) = line.strip_prefix(HEADER).filter(|_| !in_fence) {
                close(&mut open, &mut st, &mut buf);
                emit_session(sink, &st, &mut buf, st.started_at)?;
                st = State {
                    asid: Some(format!(
                        "{path_id:016x}:{}",
                        when.trim().replace([' ', ':', '-'], "")
                    )),
                    started_at: local_ts(when),
                    ..State::default()
                };
                return Ok(());
            }
            let (kind, content) = if in_fence {
                (Block::Reply, &*line)
            } else if let Some(u) = line.strip_prefix("####") {
                (Block::User, u.strip_prefix(' ').unwrap_or(u))
            } else if let Some(o) = line.strip_prefix('>') {
                (Block::Output, o.strip_prefix(' ').unwrap_or(o))
            } else {
                (Block::Reply, &*line)
            };
            let same = open.as_ref().is_some_and(|o| o.kind == kind);
            if !same {
                close(&mut open, &mut st, &mut buf);
                open = Some(Open {
                    kind,
                    line: ix,
                    offset,
                    text: String::new(),
                });
            }
            if kind == Block::Reply && line.trim_start().starts_with("```") {
                in_fence = !in_fence;
            }
            if let Some(o) = open.as_mut() {
                o.text.push_str(content.trim_end());
                o.text.push('\n');
            }
            Ok(())
        })?;
        let quiet = blirp_core::now_ms() - lines.mtime_ms >= STALE_TAIL_MS;
        let mut pos = lines.pos().clone();
        let mut retry = lines.partial;
        match open.take() {
            Some(o) if !quiet => {
                // Store it once the next block shows it is complete.
                pos = lines.rewound(o.line, o.offset);
                retry = true;
            }
            o => {
                open = o;
                close(&mut open, &mut st, &mut buf);
            }
        }
        emit_session(sink, &st, &mut buf, None)?;
        st.pos = pos;
        let mut c = Cursor::from_state(&st)?;
        c.retry = retry;
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_lines() {
        let mut st = State::default();
        parse_tokens(
            "Tokens: 2.3k sent, 150 received. Cost: $0.01 message, $0.05 session.",
            &mut st,
        );
        assert_eq!(
            (st.tokens_in, st.tokens_out, st.cost),
            (2300, 150, Some(0.05))
        );
        assert_eq!(count("1.2M"), Some(1_200_000));
        assert!(local_ts("2024-05-01 10:00:00").is_some());
    }
}
