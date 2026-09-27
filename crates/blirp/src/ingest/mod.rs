//! Transcript ingest (§8): one adapter per agent tails the agent's native
//! session store and normalizes it into `sessions` + `events`.
//!
//! Flow: [`service::IngestService`] watches every adapter root (debounced),
//! rescans every 5 minutes and at startup, and runs [`engine::Engine`] passes
//! on the blocking pool. A pass asks an adapter for its [`Source`]s, skips
//! the ones whose fingerprint matches the stored [`Cursor`], and feeds the
//! rest through [`Adapter::ingest`] into a [`sink::StoreSink`], which redacts,
//! caps, links and batches everything into `Store::ingest_tx` transactions.

pub mod aider;
pub mod amp;
pub mod claude;
pub mod codex;
pub mod cursor;
pub mod dsh;
mod emit;
pub mod engine;
pub mod gemini;
mod jsonl;
pub mod opencode;
pub mod pi;
pub mod pricing;
pub mod service;
mod sink;
mod text;

use blirp_core::model::EventKind;
use blirp_core::store::{NonProjectDirs, Store};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub use engine::Engine;
pub use service::IngestService;

pub type Result<T> = anyhow::Result<T>;

/// Where adapters look for agent stores. Built from the process environment
/// in the daemon; tests point it at a temp home.
#[derive(Debug, Clone)]
pub struct IngestEnv {
    /// The user's home directory.
    pub home: PathBuf,
    /// `BLIRP_HOME`. Transcripts whose cwd is inside it (except
    /// `worktrees/` and `workspaces/`) are blirp's own background runs and
    /// are not ingested.
    pub blirp_home: PathBuf,
    /// Environment overrides adapters honor (`CLAUDE_CONFIG_DIR`, ...).
    pub vars: HashMap<String, OsString>,
    /// Folders ingested sessions never create a project for (§5).
    pub non_projects: NonProjectDirs,
}

/// Env vars adapters consult; anything else is ignored.
const ENV_VARS: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "PI_CODING_AGENT_DIR",
    "GEMINI_CLI_HOME",
    "XDG_DATA_HOME",
    "APPDATA",
    "AMP_DATA_DIR",
    "CURSOR_CONFIG_DIR",
    "DSH_HOME",
];

impl IngestEnv {
    /// From the running process. `None` when the home directory is unknown.
    pub fn from_process(blirp_home: &Path) -> Option<Self> {
        let home = blirp_core::paths::user_home()?;
        let vars = ENV_VARS
            .iter()
            .filter_map(|k| {
                std::env::var_os(k)
                    .filter(|v| !v.is_empty())
                    .map(|v| (k.to_string(), v))
            })
            .collect();
        Some(Self {
            home,
            blirp_home: blirp_home.to_path_buf(),
            vars,
            non_projects: NonProjectDirs::from_process()
                .with_workspaces(&blirp_home.join("workspaces")),
        })
    }

    /// A self-contained environment rooted at `home` (tests): no temp or
    /// system folders, since tests work in the temp folder.
    pub fn at_home(home: &Path, blirp_home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            blirp_home: blirp_home.to_path_buf(),
            vars: HashMap::new(),
            non_projects: NonProjectDirs::auto(Some(home.to_path_buf()), Vec::new())
                .with_workspaces(&blirp_home.join("workspaces")),
        }
    }

    pub(crate) fn var_path(&self, key: &str) -> Option<PathBuf> {
        self.vars.get(key).map(PathBuf::from)
    }

    /// `$XDG_DATA_HOME` or `~/.local/share` (also used on Windows by the
    /// Node-based agents).
    pub(crate) fn data_home(&self) -> PathBuf {
        self.var_path("XDG_DATA_HOME")
            .unwrap_or_else(|| self.home.join(".local").join("share"))
    }

    /// Windows `%APPDATA%` (roaming), when running on Windows.
    pub(crate) fn appdata(&self) -> Option<PathBuf> {
        if !cfg!(windows) {
            return None;
        }
        Some(
            self.var_path("APPDATA")
                .unwrap_or_else(|| self.home.join("AppData").join("Roaming")),
        )
    }
}

/// Every built-in adapter, in a stable order.
pub fn adapters(env: &IngestEnv) -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(claude::Claude::new(env)),
        Box::new(codex::Codex::new(env)),
        Box::new(opencode::Opencode::new(env)),
        Box::new(pi::Pi::new(env)),
        Box::new(gemini::Gemini::new(env)),
        Box::new(cursor::Cursor::new(env)),
        Box::new(amp::Amp::new(env)),
        Box::new(aider::Aider::new(env)),
        Box::new(dsh::Dsh::new(env)),
    ]
}

/// One transcript source: a file, or one session inside a database.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    /// Cursor key, unique per adapter (usually the path).
    pub key: String,
    /// File whose changes concern this source (the database for SQLite).
    pub path: PathBuf,
    /// Cheap change detector; equal to the stored cursor's -> nothing to do.
    pub fingerprint: String,
    /// Last modification, unix ms (file mtime or the store's update time).
    pub mtime_ms: i64,
    /// Adapter-specific item inside `path` (e.g. an opencode session id).
    /// `Some` marks a database-backed source: watcher events for it trigger
    /// a rescan instead of a plain file re-read.
    pub item: Option<String>,
}

impl Source {
    /// A plain file source fingerprinted by size and mtime.
    pub fn file(path: &Path) -> Option<Source> {
        let md = std::fs::metadata(path).ok()?;
        if !md.is_file() {
            return None;
        }
        let mtime_ns = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        Some(Source {
            key: path.to_string_lossy().into_owned(),
            path: path.to_path_buf(),
            fingerprint: format!("{}:{mtime_ns}", md.len()),
            mtime_ms: i64::try_from(mtime_ns / 1_000_000).unwrap_or(i64::MAX),
            item: None,
        })
    }
}

/// Stored per `(adapter, source.key)` in `ingest_cursors`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cursor {
    /// Fingerprint of the source when this cursor was written.
    #[serde(default)]
    pub fp: String,
    /// Adapter-specific position and running totals.
    #[serde(default)]
    pub state: JsonValue,
    /// Set by an adapter that stopped before the end on purpose (partial
    /// line, message still streaming): the source is retried on the next
    /// pass even if its fingerprint does not change.
    #[serde(skip)]
    pub retry: bool,
}

impl Cursor {
    /// Typed adapter state; unreadable state (format change) restarts from
    /// scratch, which is safe because events dedupe on `(session_id, seq)`.
    pub fn state_of<T: DeserializeOwned + Default>(c: Option<&Cursor>) -> T {
        c.and_then(|c| serde_json::from_value(c.state.clone()).ok())
            .unwrap_or_default()
    }

    pub fn from_state<T: Serialize>(state: &T) -> Result<Cursor> {
        Ok(Cursor {
            fp: String::new(),
            state: serde_json::to_value(state)?,
            retry: false,
        })
    }
}

/// Session-level facts an adapter learned. `None` means "unknown", never
/// "clear"; token and cost values are absolute running totals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionMeta {
    pub cwd: Option<String>,
    /// Title the agent itself assigned.
    pub title: Option<String>,
    /// First user prompt; the title fallback.
    pub first_prompt: Option<String>,
    pub branch: Option<String>,
    /// The repository's remote URL as the transcript records it (codex).
    pub git_remote: Option<String>,
    pub model: Option<String>,
    pub started_at: Option<i64>,
    /// Latest activity the store itself records (e.g. a database row's
    /// update time), beyond event timestamps.
    pub last_activity: Option<i64>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub cost_usd: Option<f64>,
    /// `agent_session_id` of the parent session (subagents).
    pub parent: Option<String>,
    pub transcript_path: Option<String>,
}

impl SessionMeta {
    fn merge(&mut self, o: SessionMeta) {
        fn take<T>(dst: &mut Option<T>, src: Option<T>) {
            if src.is_some() {
                *dst = src;
            }
        }
        take(&mut self.cwd, o.cwd);
        take(&mut self.title, o.title);
        if self.first_prompt.is_none() {
            self.first_prompt = o.first_prompt;
        }
        take(&mut self.branch, o.branch);
        take(&mut self.git_remote, o.git_remote);
        take(&mut self.model, o.model);
        self.started_at = match (self.started_at, o.started_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.last_activity = self.last_activity.max(o.last_activity);
        take(&mut self.tokens_in, o.tokens_in);
        take(&mut self.tokens_out, o.tokens_out);
        take(&mut self.cost_usd, o.cost_usd);
        take(&mut self.parent, o.parent);
        take(&mut self.transcript_path, o.transcript_path);
    }
}

/// Hand `meta` to the sink once its cwd is known (once per read). The sink
/// does not create a session row before it knows the cwd (it decides the
/// project, the launch link and the BLIRP_HOME exclusion), so a long read
/// reports it early instead of buffering every event until the end.
pub(crate) fn report_cwd(
    sink: &mut dyn EventSink,
    asid: &str,
    meta: &SessionMeta,
    reported: &mut bool,
) {
    if !*reported && meta.cwd.is_some() {
        *reported = true;
        sink.session(asid, meta.clone());
    }
}

/// A normalized event before redaction and storage.
#[derive(Debug, Clone, PartialEq)]
pub struct NormEvent {
    /// Deterministic for the same source content, increasing with arrival.
    pub seq: i64,
    pub ts: Option<i64>,
    pub kind: EventKind,
    pub text: String,
    pub meta: Option<JsonValue>,
}

/// Receives what an adapter reads. Redaction, size caps, session linking
/// and batching happen behind it.
pub trait EventSink {
    /// Record facts about the session `agent_session_id`.
    fn session(&mut self, agent_session_id: &str, meta: SessionMeta);
    /// Append one event. Errors abort the source (retried next pass).
    fn event(&mut self, agent_session_id: &str, ev: NormEvent) -> Result<()>;
    /// A skipped line or unknown shape; logged once per source. Must not
    /// contain transcript text.
    fn warn(&mut self, what: &str);
}

/// §8 adapter contract.
pub trait Adapter: Send + Sync {
    fn id(&self) -> &'static str;
    /// Directories to watch (only existing ones are watched).
    fn roots(&self) -> Vec<PathBuf>;
    /// Discover transcript sources.
    fn scan(&self, store: &Store) -> Result<Vec<Source>>;
    /// Read everything after `cursor` into `sink`; return the new cursor.
    fn ingest(
        &self,
        src: &Source,
        cursor: Option<Cursor>,
        sink: &mut dyn EventSink,
    ) -> Result<Cursor>;
}

/// Files under `root` (recursive, symlinks not followed) accepted by `keep`.
pub(crate) fn walk_files(
    root: &Path,
    max_depth: usize,
    keep: &dyn Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let p = entry.path();
            if ft.is_dir() {
                if depth < max_depth {
                    stack.push((p, depth + 1));
                }
            } else if ft.is_file() && keep(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// File sources for `paths`, skipping files that vanished meanwhile.
pub(crate) fn file_sources(paths: impl IntoIterator<Item = PathBuf>) -> Vec<Source> {
    paths.into_iter().filter_map(|p| Source::file(&p)).collect()
}

/// Best-effort inverse of the "path with separators turned into dashes"
/// directory names used by Claude Code (`C--Users-x-proj`), Cursor
/// (`C-Users-x-proj`, `Users-x-proj`) and similar. The encoding is lossy, so
/// candidates are checked against the filesystem; `None` when no existing
/// folder matches.
pub(crate) fn decode_dashed_dir(name: &str) -> Option<PathBuf> {
    let name = name.trim_matches('-');
    let bytes = name.as_bytes();
    let (root, rest) = if bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b'-'
        && cfg!(windows)
    {
        let rest = name[2..].trim_start_matches('-');
        (PathBuf::from(format!("{}:\\", &name[..1])), rest)
    } else {
        (PathBuf::from("/"), name)
    };
    let tokens: Vec<&str> = rest.split('-').collect();
    if tokens.is_empty() || tokens.len() > 64 {
        return None;
    }
    fn dfs(dir: &Path, tokens: &[&str], budget: &mut u32) -> Option<PathBuf> {
        if tokens.is_empty() {
            return Some(dir.to_path_buf());
        }
        for take in 1..=tokens.len().min(6) {
            let parts = &tokens[..take];
            // Separators a dash may stand for inside one path component.
            let seps: &[&str] = if take == 1 {
                &[""]
            } else {
                &["-", ".", " ", "_"]
            };
            for sep in seps {
                if *budget == 0 {
                    return None;
                }
                *budget -= 1;
                let comp = parts.join(sep);
                if comp.is_empty() {
                    continue;
                }
                let cand = dir.join(&comp);
                if cand.is_dir()
                    && let Some(found) = dfs(&cand, &tokens[take..], budget)
                {
                    return Some(found);
                }
            }
        }
        None
    }
    let mut budget = 2000;
    dfs(&root, &tokens, &mut budget)
}
