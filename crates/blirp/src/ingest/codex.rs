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
    /// A fork (`forked_from_id`, e.g. a Codex Desktop subagent) begins with
    /// a copy of its parent's rollout: the parent's `session_meta` second,
    /// then the parent's lines (messages, compactions; no token usage).
    /// Lines with an `ordinal` below this (`subagent_history_start_ordinal`)
    /// are that copy.
    #[serde(default)]
    fork_start: Option<i64>,
    /// A fork without that field: when it started (see [`FORK_COPY_MS`]).
    #[serde(default)]
    fork_at: Option<i64>,
    /// A subagent's parent (`parent_thread_id`) and title, kept for later
    /// reads: the parent may be ingested after the subagent.
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    title: Option<String>,
    /// The latest model call used at least [`NEAR_FULL_PERCENT`] of the
    /// context window: the next crossing is a new signal.
    #[serde(default)]
    near_full: bool,
}

/// Share of the context window (`token_count.info.model_context_window`)
/// the latest call's input (`last_token_usage.input_tokens`) must reach for
/// the "start a fresh session" suggestion.
const NEAR_FULL_PERCENT: i64 = 90;

/// `info` of an `event_msg` `token_count`: whether the latest call used at
/// least [`NEAR_FULL_PERCENT`] of the window; `None` without both numbers.
fn near_full(info: &Value) -> Option<bool> {
    let used = info.pointer("/last_token_usage/input_tokens")?.as_i64()?;
    let window = info
        .get("model_context_window")?
        .as_i64()
        .filter(|w| *w > 0)?;
    Some(used.saturating_mul(100) >= window.saturating_mul(NEAR_FULL_PERCENT))
}

/// A fork without `subagent_history_start_ordinal`: lines stamped within
/// this of its start are the parent's copy, which is written at once
/// (observed within 1 ms, codex 0.153), before any turn of its own.
const FORK_COPY_MS: i64 = 1_000;

impl State {
    /// A line of the parent's history copied into a fork.
    fn copied(&self, ordinal: i64, ts: Option<i64>) -> bool {
        match (self.fork_start, self.fork_at) {
            (Some(start), _) => ordinal < start,
            (None, Some(at)) => ts.is_none_or(|t| t - at < FORK_COPY_MS),
            (None, None) => false,
        }
    }

    /// Fork and subagent facts of the session's own (first) `session_meta`.
    fn own_meta(&mut self, p: &Value, ts: Option<i64>) {
        let s = |k: &str| p.get(k).and_then(Value::as_str);
        if s("forked_from_id").is_some() {
            let start = p.get("subagent_history_start_ordinal");
            self.fork_start = start
                .and_then(Value::as_i64)
                .or_else(|| start.and_then(Value::as_str).and_then(|v| v.parse().ok()));
            if self.fork_start.is_none() {
                self.fork_at = ts;
            }
        }
        if s("thread_source") == Some("subagent") {
            self.parent = s("parent_thread_id")
                .or_else(|| {
                    p.pointer("/source/subagent/thread_spawn/parent_thread_id")
                        .and_then(Value::as_str)
                })
                .or_else(|| s("forked_from_id"))
                .map(str::to_string);
            let task = s("agent_path")
                .and_then(|a| a.rsplit('/').next())
                .filter(|a| !a.is_empty());
            self.title = s("agent_nickname").map(|n| match task {
                Some(t) => format!("subagent ({n}): {t}"),
                None => format!("subagent ({n})"),
            });
        }
    }
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

/// A Codex subagent rollout, as [`repair_subagents`] needs it.
#[derive(Debug, PartialEq)]
struct Subagent {
    id: String,
    parent: Option<String>,
    title: Option<String>,
    /// A fork: lines of the parent's history it starts with, when the file
    /// confirms them (every one of them has `ordinal` = its line index, the
    /// second is the parent's `session_meta`). Their events have seqs below
    /// `copied * 1024` (§8 seqs are `line_index * 1024 + n`).
    copied: Option<u64>,
    /// A fork whose copy could not be confirmed (left alone).
    unconfirmed: bool,
}

/// Read the head of a rollout: `None` unless it is a subagent's.
fn inspect_subagent(path: &Path) -> Result<Option<Subagent>> {
    let compressed = path.extension().is_some_and(|e| e == "zst");
    let mut lines = Lines::open(path, &FilePos::default(), compressed)?;
    let mut st = State::default();
    let mut found: Option<Subagent> = None;
    let mut ok = true;
    let mut stop = false;
    lines.for_each(|ix, raw| {
        if stop {
            return Ok(());
        }
        let v: Value = serde_json::from_slice(raw).unwrap_or(Value::Null);
        let p = v.get("payload").unwrap_or(&Value::Null);
        if ix == 0 {
            let id = p
                .get("id")
                .or_else(|| p.get("session_id"))
                .and_then(Value::as_str);
            if v.get("type").and_then(Value::as_str) != Some("session_meta")
                || p.get("thread_source").and_then(Value::as_str) != Some("subagent")
                || id.is_none()
            {
                stop = true;
                return Ok(());
            }
            st.own_meta(p, None);
            found = Some(Subagent {
                id: id.unwrap_or_default().to_string(),
                parent: st.parent.clone(),
                title: st.title.clone(),
                copied: None,
                unconfirmed: false,
            });
            stop = st.fork_start.is_none_or(|n| n <= 1);
            return Ok(());
        }
        let start = st.fork_start.unwrap_or(0);
        let ordinal = v.get("ordinal").and_then(Value::as_i64);
        ok &= ordinal == i64::try_from(ix).ok();
        if ix == 1 {
            let own = found.as_ref().map(|f| f.id.as_str());
            let meta_id = p.get("id").and_then(Value::as_str);
            ok &= v.get("type").and_then(Value::as_str) == Some("session_meta")
                && meta_id.is_some()
                && meta_id != own;
        }
        stop = i64::try_from(ix + 1).unwrap_or(i64::MAX) >= start;
        Ok(())
    })?;
    if let Some(f) = found.as_mut()
        && let Some(start) = st.fork_start
    {
        // A file shorter than its copy is not confirmed either.
        let complete = stop && lines.pos().line >= u64::try_from(start).unwrap_or(u64::MAX);
        if ok && complete {
            f.copied = u64::try_from(start).ok();
        } else {
            f.unconfirmed = true;
        }
    }
    Ok(found)
}

/// Counts of [`repair_subagents`].
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SubagentRepair {
    /// Subagent rollouts of sessions on this machine.
    pub subagents: usize,
    /// Of those, cursors reset (re-read to link the parent).
    pub relinked: usize,
    /// Forks whose copied history was dropped, and the events dropped.
    pub forks_truncated: usize,
    pub events_dropped: i64,
    /// Forks left alone: the file did not confirm the copy.
    pub forks_unconfirmed: usize,
    /// Fork titles taken from the parent's copied first prompt, replaced.
    pub titles_fixed: usize,
    /// Summaries of truncated forks, made from the copy too, cleared.
    pub summaries_cleared: usize,
    /// Distiller records of forks removed (nothing else relied on them).
    pub records_removed: usize,
}

/// One-time cleanup of Codex subagent sessions ingested before blirp knew
/// them (§8): their cursors are reset so the next pass links them to their
/// parent (`parent_session_id`, replicated with the row); a fork's copy of
/// its parent's history is dropped everywhere with a replicated
/// [`blirp_core::store::Change::TruncateEvents`] (only for this machine's
/// sessions, and only when the file confirms the copy line by line); a
/// fork's title taken from the parent's copied first prompt becomes its
/// subagent title; and records the distiller made from a fork alone are
/// removed (see [`Store::distiller_records_only_of`]).
///
/// A rollout that cannot be read is skipped (logged), so one bad file never
/// holds up the rest. None: `stop` said to stop before the end (the repair
/// runs again next time; every step is idempotent).
pub fn repair_subagents(
    store: &Store,
    machine_id: &str,
    codex_home: &Path,
    stop: &dyn Fn() -> bool,
) -> Result<Option<SubagentRepair>> {
    use blirp_core::store::Change;
    let mut out = SubagentRepair::default();
    let mut forks = Vec::new();
    let codex = Codex {
        home: codex_home.to_path_buf(),
    };
    for src in codex.scan(store)? {
        if stop() {
            return Ok(None);
        }
        let sub = match inspect_subagent(&src.path) {
            Ok(Some(sub)) => sub,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(path = %src.path.display(), error = %e, "codex subagent repair: rollout not readable; skipped");
                continue;
            }
        };
        let Some(s) = store.session_by_agent_id("codex", &sub.id)? else {
            continue;
        };
        if s.machine_id != machine_id {
            continue;
        }
        out.subagents += 1;
        if s.parent_session_id.is_none() && store.delete_cursor("codex", &src.key)? {
            out.relinked += 1;
        }
        if sub.unconfirmed {
            out.forks_unconfirmed += 1;
            tracing::warn!(path = %src.path.display(), "codex fork: copied history not confirmed; left as is");
        }
        let Some(copied) = sub.copied else { continue };
        forks.push(s.id.clone());
        let below = i64::try_from(copied)
            .unwrap_or(i64::MAX)
            .saturating_mul(1024);
        let n = store.count_events_below(&s.id, below)?;
        if n > 0 {
            store.apply(Change::TruncateEvents {
                session_id: s.id.clone(),
                below_seq: below,
            })?;
            out.forks_truncated += 1;
            out.events_dropped += n;
            // Its summary was made from the parent's copied turns too: gone,
            // so the fork is summarized again from its own events.
            if s.summary.is_some() || s.distilled_through_seq > 0 {
                store.modify_session(&s.id, |x| {
                    x.summary = None;
                    x.distilled_through_seq = 0;
                })?;
                out.summaries_cleared += 1;
            }
        }
        let parent = match &sub.parent {
            Some(p) => store.session_by_agent_id("codex", p)?,
            None => None,
        };
        if let (Some(title), Some(parent)) = (&sub.title, parent)
            && s.title.is_some()
            && s.title == parent.title
        {
            store.modify_session(&s.id, |x| x.title = Some(title.clone()))?;
            out.titles_fixed += 1;
        }
    }
    for id in store.distiller_records_only_of(&forks)? {
        store.delete_record(&id)?;
        out.records_removed += 1;
    }
    Ok(Some(out))
}

impl Codex {
    /// The rollout of session `id` (`rollout-<date>-<id>.jsonl[.zst]`).
    pub(crate) fn rollout_of(&self, id: &str) -> Option<PathBuf> {
        let names = [format!("-{id}.jsonl"), format!("-{id}.jsonl.zst")];
        self.roots().iter().find_map(|r| {
            walk_files(r, 4, &|p: &Path| {
                is_rollout(p)
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| names.iter().any(|s| n.ends_with(s.as_str())))
            })
            .into_iter()
            .next()
        })
    }

    /// Whether the rollout of session `parent` is a scripted run
    /// (`codex exec`, §8). A parent not on disk (yet), or one that cannot
    /// be read, is not.
    pub(crate) fn parent_is_scripted(&self, parent: &str) -> bool {
        self.rollout_of(parent)
            .is_some_and(|p| matches!(super::transcript_is_headless("codex", &p), Ok(true)))
    }

    /// Whether the rollout at `path` is a scripted run, or a subagent of
    /// one (judged by its parent's rollout, as ingest does).
    pub(crate) fn scripted(&self, path: &Path) -> Result<bool> {
        match inspect_subagent(path)?.and_then(|s| s.parent) {
            Some(parent) => Ok(self.parent_is_scripted(&parent)),
            None => super::transcript_is_headless("codex", path),
        }
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
        // A subagent of a scripted run goes with it; judged again at every
        // read, since the parent may have turned into a session since.
        if let Some(parent) = &st.parent {
            launch.set_parent_scripted(self.parent_is_scripted(parent));
        }
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
        meta.parent.clone_from(&st.parent);
        meta.title.clone_from(&st.title);
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
            let ts = v.get("timestamp").and_then(parse_ts);
            let id = p
                .get("id")
                .or_else(|| p.get("session_id"))
                .and_then(Value::as_str);
            let before = launch.is_headless();
            if ty == "session_meta" && st.asid.is_none() {
                // The session's own (first) `session_meta`.
                st.asid = id.map(str::to_string);
                st.own_meta(p, ts);
                meta.parent.clone_from(&st.parent);
                meta.title.clone_from(&st.title);
                if let Some(parent) = &st.parent {
                    launch.set_parent_scripted(self.parent_is_scripted(parent));
                }
            } else {
                // The parent's history copied into a fork is the parent's:
                // its events are in the parent's session already, and its
                // runs and turns are not the fork's (§8 scripted runs).
                let ordinal = v
                    .get("ordinal")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(|| i64::try_from(ix).unwrap_or(i64::MAX));
                if st.asid.is_some() && st.copied(ordinal, ts) {
                    return Ok(());
                }
            }
            // The parent's `session_meta` heads that copy; it is not a run of
            // this session either.
            let parents_meta = ty == "session_meta" && id.is_some() && id != st.asid.as_deref();
            if !parents_meta {
                launch_line(&v, &mut launch);
            }
            launch.report(
                before,
                sink,
                &st.asid.clone().unwrap_or_else(|| fallback.clone()),
            );
            if let Some(t) = ts {
                meta.started_at = Some(meta.started_at.map_or(t, |s| s.min(t)));
            }
            match ty {
                "session_meta" => {
                    if parents_meta {
                        // A fork (older ones say so only here).
                        if st.fork_start.is_none() && st.fork_at.is_none() {
                            st.fork_at = ts;
                        }
                        return Ok(());
                    }
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
                        && let Some(info) = p.get("info")
                    {
                        if let Some(t) = info.get("total_token_usage").and_then(tokens_of) {
                            st.usage = Some(t);
                        }
                        if let Some(full) = near_full(info) {
                            if full && !st.near_full {
                                meta.context_near_full_at = meta.context_near_full_at.max(ts);
                            }
                            st.near_full = full;
                        }
                    }
                    return Ok(());
                }
                "compacted" => {
                    // Codex replaced its history with a compacted one (its
                    // context filled up). `message` is its summary; empty
                    // when the model compacted remotely.
                    let msg = p
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|m| !m.is_empty())
                        .unwrap_or("conversation compacted");
                    let asid = st.asid.clone().unwrap_or_else(|| fallback.clone());
                    return Emit::line(sink, &asid, ix).text(ts, EventKind::Summary, msg, None);
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
