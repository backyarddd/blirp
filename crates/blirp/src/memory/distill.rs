//! Distill (§9): turn a session transcript into a title, summary, records and
//! an updated brief with a summarizer backend, then apply it transactionally.

use crate::agents::wrap_for_platform;
use crate::memory::render::clip;
use crate::state::SharedState;
use blirp_core::config::{BriefMode, MemoryConfig, Summarizer};
use blirp_core::model::{
    DistillPause, DistillStatus, Event, EventKind, MemoryPart, Record, RecordKind, RecordStatus,
    ServerEvent, Session, SessionOrigin, SessionSummary, SummaryItem,
};
use blirp_core::process;
use blirp_core::store::{
    BriefApply, DistillEvents, DistillOutcome, DistillPlan, RecordFilter, Store, StoreError,
    norm_title,
};
use serde::Deserialize;
use std::collections::HashSet;
use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// Summarizer process/HTTP timeout.
pub const DISTILL_TIMEOUT: Duration = Duration::from_secs(180);
/// How often the scheduler looks for idle/ended sessions.
const SCHEDULE_TICK: Duration = Duration::from_secs(30);
/// Sessions inactive for longer than this are never auto-distilled (keeps
/// freshly ingested old history from eating the daily budget).
const AUTO_WINDOW_MS: i64 = 7 * 24 * 3600 * 1000;
const BUDGET_KEY: &str = "memory.distill_budget";
const MAX_ITEMS: usize = 20;
const MAX_BRIEF_CHARS: usize = 12_000;
/// Env var set for summarizer processes; blirp hooks exit immediately when set.
pub const DISTILLING_ENV: &str = "BLIRP_DISTILLING";
const DEFAULT_OLLAMA: &str = "http://127.0.0.1:11434";

#[derive(Debug, thiserror::Error)]
pub enum DistillError {
    #[error("session has no events to distill")]
    NothingToDistill,
    #[error("no summarizer available: {0}")]
    NoBackend(String),
    #[error("summarizer failed: {0}")]
    Backend(String),
    #[error("summarizer output invalid after retry: {0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

// ---------------------------------------------------------------- compaction

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn event_line(e: &Event) -> Option<String> {
    let text = e.text.trim();
    if text.is_empty() {
        return None;
    }
    Some(match e.kind {
        EventKind::User => format!("USER: {text}"),
        EventKind::Assistant => format!("ASSISTANT: {text}"),
        EventKind::ToolCall => format!("TOOL: {}", clip(&one_line(text), 300)),
        EventKind::ToolResult => format!("RESULT: {}", clip(&one_line(text), 400)),
        EventKind::FileEdit => format!("EDIT: {}", clip(&one_line(text), 300)),
        EventKind::System => format!("SYSTEM: {}", clip(&one_line(text), 200)),
        EventKind::Summary => format!("SUMMARY: {}", clip(text, 3000)),
    })
}

/// §9 input compaction: prompts verbatim, assistant text, one-line tool calls,
/// truncated results; over `max_chars` keeps the first 20% and last 80%.
pub fn compact_transcript(events: &[Event], max_chars: usize) -> String {
    let full = events
        .iter()
        .filter_map(event_line)
        .collect::<Vec<_>>()
        .join("\n");
    cut(full, max_chars)
}

/// [`compact_transcript`] of events selected by `Store::distill_events`:
/// the events left out between head and tail are marked in the text.
pub fn compact_selected(ev: &DistillEvents, max_chars: usize) -> String {
    let mut lines: Vec<String> = ev.head.iter().filter_map(event_line).collect();
    if ev.omitted > 0 {
        lines.push(format!("[... {} events omitted ...]", ev.omitted));
    }
    lines.extend(ev.tail.iter().filter_map(event_line));
    cut(lines.join("\n"), max_chars)
}

/// Keep the first 20% and last 80% of `full` when it is over `max_chars`.
fn cut(full: String, max_chars: usize) -> String {
    let total = full.chars().count();
    if total <= max_chars {
        return full;
    }
    let marker_room = 60;
    let budget = max_chars.saturating_sub(marker_room);
    let head_n = budget / 5;
    let tail_n = budget - head_n;
    let head: String = full.chars().take(head_n).collect();
    let tail: String = full.chars().skip(total - tail_n).collect();
    let omitted = total - head_n - tail_n;
    format!("{head}\n[... {omitted} characters omitted ...]\n{tail}")
}

// ---------------------------------------------------------------- contract

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub title: String,
    #[serde(default)]
    pub body: String,
}

/// §9 distill output contract.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistillOutput {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub decisions: Vec<Item>,
    #[serde(default)]
    pub open_threads: Vec<Item>,
    #[serde(default)]
    pub resolved_record_ids: Vec<String>,
    #[serde(default)]
    pub gotchas: Vec<Item>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub brief_md: String,
}

/// The JSON object inside a model reply (tolerates code fences and prose around it).
fn extract_json(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
}

/// Parse and validate a reply. Unknown record ids are dropped; structural
/// problems are errors (the caller retries once).
pub fn parse_output(text: &str, known_ids: &HashSet<String>) -> Result<DistillOutput, String> {
    let json = extract_json(text).ok_or("reply contains no JSON object")?;
    let mut out: DistillOutput = serde_json::from_str(json)
        .map_err(|e| format!("reply is not valid JSON for the contract: {e}"))?;
    out.title = one_line(&out.title);
    if out.title.is_empty() {
        return Err("title is empty".into());
    }
    if out.title.chars().count() > 200 {
        return Err("title is longer than 200 characters".into());
    }
    out.summary = out.summary.trim().to_string();
    if out.summary.is_empty() {
        return Err("summary is empty".into());
    }
    out.summary = clip(&out.summary, 3000);
    for (name, list) in [
        ("decisions", &mut out.decisions),
        ("open_threads", &mut out.open_threads),
        ("gotchas", &mut out.gotchas),
    ] {
        for it in list.iter_mut() {
            it.title = one_line(&it.title);
            it.body = it.body.trim().to_string();
            if it.title.is_empty() {
                return Err(format!("an item in {name} has an empty title"));
            }
            it.title = clip(&it.title, 200);
            it.body = clip(&it.body, 2000);
        }
        list.truncate(MAX_ITEMS);
    }
    out.resolved_record_ids.retain(|id| known_ids.contains(id));
    out.files = out
        .files
        .iter()
        .map(|f| f.trim().trim_start_matches("./").to_string())
        .filter(|f| !f.is_empty() && f.chars().count() <= 300)
        .take(100)
        .collect();
    out.brief_md = out.brief_md.trim().to_string();
    if out.brief_md.chars().count() > MAX_BRIEF_CHARS {
        return Err(format!(
            "brief_md is longer than {MAX_BRIEF_CHARS} characters"
        ));
    }
    // The model may echo a secret the transcript redaction missed or that
    // it reconstructed; its output is stored, injected and replicated.
    let red = |s: &mut String| {
        if let std::borrow::Cow::Owned(r) = blirp_core::redact::redact(s) {
            *s = r;
        }
    };
    red(&mut out.title);
    red(&mut out.summary);
    red(&mut out.brief_md);
    out.files.iter_mut().for_each(red);
    for it in out
        .decisions
        .iter_mut()
        .chain(&mut out.open_threads)
        .chain(&mut out.gotchas)
    {
        red(&mut it.title);
        red(&mut it.body);
    }
    Ok(out)
}

const INSTRUCTIONS: &str = r#"You are the memory distiller of blirp, a workspace for coding agents. Read the coding-agent session transcript below and extract durable project memory for future sessions.

Reply with ONLY one JSON object (no prose, no code fences) with exactly these keys:
{"title": string, "summary": string, "decisions": [{"title": string, "body": string}], "open_threads": [{"title": string, "body": string}], "resolved_record_ids": [string], "gotchas": [{"title": string, "body": string}], "files": [string], "brief_md": string}

Rules:
- title: at most 8 words naming what the session worked on.
- summary: 3-6 sentences: the goal, what was done, the outcome and what is left.
- decisions: choices future work must respect (architecture, libraries, conventions), reason in body. [] if none.
- open_threads: unfinished work, known bugs and follow-ups at the end of the session. Do not repeat EXISTING RECORDS.
- resolved_record_ids: ids from EXISTING RECORDS that this session clearly completed or made obsolete. Only ids from that list.
- gotchas: non-obvious pitfalls discovered (environment quirks, surprising behavior) that would cost a future session time.
- files: repository-relative paths the session created or changed.
- brief_md: the full updated project brief in markdown (what the project is, how to build and run it, current state and priorities), at most 1500 tokens. Start from CURRENT BRIEF and change only what this session changed; return it unchanged if nothing changed, or "" if you do not know enough.
- Never include secrets, tokens or credentials. Write in English. Keep item bodies to 1-3 sentences."#;

pub fn build_prompt(
    brief: Option<&str>,
    records: &[Record],
    previous: Option<&SessionSummary>,
    transcript: &str,
) -> String {
    let mut p = String::with_capacity(transcript.len() + 4096);
    p.push_str(INSTRUCTIONS);
    if let Some(prev) = previous {
        p.push_str(
            "\n\nPREVIOUS SUMMARY OF THIS SESSION (covers the transcript before the part below):\n",
        );
        if let Some(t) = &prev.title {
            p.push_str(&format!("Title: {}\n", one_line(t)));
        }
        if let Some(s) = &prev.summary {
            p.push_str(&format!("Summary: {}\n", one_line(s)));
        }
        for (name, list) in [
            ("Decision", &prev.decisions),
            ("Open thread", &prev.open_threads),
            ("Gotcha", &prev.gotchas),
        ] {
            for i in list {
                p.push_str(&format!(
                    "{name}: {}: {}\n",
                    one_line(&i.title),
                    clip(&one_line(&i.body), 300)
                ));
            }
        }
        p.push_str(
            "The TRANSCRIPT below continues the session after that point. Return the title and summary \
             of the whole session. In decisions, open_threads and gotchas list only items that are new \
             or changed in this part (same title for a changed item).",
        );
    }
    p.push_str("\n\nCURRENT BRIEF:\n");
    p.push_str(
        brief
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .unwrap_or("(none)"),
    );
    p.push_str("\n\nEXISTING RECORDS (id | kind | title: body):\n");
    if records.is_empty() {
        p.push_str("(none)\n");
    }
    for r in records {
        p.push_str(&format!(
            "{} | {} | {}: {}\n",
            r.id,
            r.kind,
            one_line(&r.title),
            clip(&one_line(&r.body), 300)
        ));
    }
    p.push_str("\nTRANSCRIPT (compacted; [...] marks omitted text):\n");
    p.push_str(transcript);
    p.push_str("\n\nReply with the JSON object now.");
    p
}

// ---------------------------------------------------------------- backends

/// A summarizer: prompt in, model reply text out.
pub trait Summarize: Send + Sync {
    fn name(&self) -> &'static str;
    fn complete(&self, prompt: String) -> impl Future<Output = Result<String, String>> + Send;
}

/// `scratch` is the root for per-run working dirs (`BLIRP_HOME/distill`).
#[derive(Debug, Clone)]
pub enum Backend {
    Claude { exe: PathBuf, scratch: PathBuf },
    Codex { exe: PathBuf, scratch: PathBuf },
    Ollama { base: String, model: String },
}

fn ollama_base() -> String {
    std::env::var("OLLAMA_HOST")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(|h| {
            let h = h.trim().trim_end_matches('/').to_string();
            if h.starts_with("http://") || h.starts_with("https://") {
                h
            } else {
                format!("http://{h}")
            }
        })
        .unwrap_or_else(|| DEFAULT_OLLAMA.into())
}

async fn ollama_up(base: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
    else {
        return false;
    };
    client
        .get(format!("{base}/api/tags"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// `auto`: claude, else ollama. Codex is never picked automatically: its
/// built-in tools cannot all be switched off (only shell, exec and the
/// optional tools are, see [`codex_args`]), so it runs only when chosen.
fn pick_auto(claude: Option<Backend>, ollama: Option<Backend>) -> Result<Backend, String> {
    claude.or(ollama).ok_or_else(|| {
        "neither claude nor ollama is available (codex runs only when chosen explicitly)".into()
    })
}

/// `codex exec` for a summarizer run: no hooks, MCP servers, shell or exec
/// tools, apps, plugins, browser, computer use, subagents or image tools
/// (feature names verified against codex 0.153), read-only sandbox, no
/// persisted session; the reply goes to `last`.
fn codex_args(last: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "exec",
        "--json",
        "--ephemeral",
        "--skip-git-repo-check",
        "--sandbox",
        "read-only",
        "-c",
        "mcp_servers={}",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    for feature in [
        "hooks",
        "shell_tool",
        "unified_exec",
        "view_image",
        "apps",
        "plugins",
        "browser_use",
        "computer_use",
        "multi_agent",
        "image_generation",
    ] {
        args.push("--disable".into());
        args.push(feature.into());
    }
    args.push("--output-last-message".into());
    args.push(last.as_os_str().to_owned());
    args
}

/// Resolve `memory.summarizer` to a runnable backend (see [`pick_auto`]).
/// `Ok(None)` for `none`. CLI summarizers run in a fresh dir under `scratch`.
pub async fn select_backend(cfg: &MemoryConfig, scratch: &Path) -> Result<Option<Backend>, String> {
    // PATH scans touch the filesystem: off the async runtime.
    let (claude_exe, codex_exe) =
        tokio::task::spawn_blocking(|| (process::which("claude"), process::which("codex")))
            .await
            .map_err(|e| format!("looking up summarizers: {e}"))?;
    let claude = || {
        claude_exe.clone().map(|exe| Backend::Claude {
            exe,
            scratch: scratch.to_path_buf(),
        })
    };
    let codex = || {
        codex_exe.clone().map(|exe| Backend::Codex {
            exe,
            scratch: scratch.to_path_buf(),
        })
    };
    let base = ollama_base();
    let ollama = Backend::Ollama {
        base: base.clone(),
        model: cfg.ollama_model.clone(),
    };
    match cfg.summarizer {
        Summarizer::None => Ok(None),
        Summarizer::Claude => claude()
            .map(Some)
            .ok_or_else(|| "claude is not on PATH".into()),
        Summarizer::Codex => codex()
            .map(Some)
            .ok_or_else(|| "codex is not on PATH".into()),
        Summarizer::Ollama => {
            if ollama_up(&base).await {
                Ok(Some(ollama))
            } else {
                Err(format!("ollama is not reachable at {base}"))
            }
        }
        Summarizer::Auto => {
            let claude = claude();
            let ollama = if claude.is_none() && ollama_up(&base).await {
                Some(ollama)
            } else {
                None
            };
            pick_auto(claude, ollama).map(Some)
        }
    }
}

/// Output of a summarizer child process.
struct ProcOut {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run a summarizer process in `dir` with the prompt on stdin, killing its
/// whole process tree on timeout.
async fn run_process(
    program: &Path,
    args: Vec<OsString>,
    stdin: String,
    dir: &Path,
    timeout: Duration,
) -> Result<ProcOut, String> {
    // Shim parsing reads the file: off the async runtime.
    let program_path = program.to_path_buf();
    let wrapped = tokio::task::spawn_blocking(move || wrap_for_platform(&program_path, args))
        .await
        .map_err(|e| format!("preparing {}: {e}", program.display()))?
        .map_err(|e| e.to_string())?;
    let mut cmd = tokio::process::Command::new(&wrapped.program);
    cmd.args(&wrapped.args)
        .envs(wrapped.env)
        .current_dir(dir)
        .env(DISTILLING_ENV, "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for k in crate::sessions::ENV_REMOVE.iter().chain(&[
        "BLIRP_SESSION_ID",
        "BLIRP_PROJECT_ID",
        "BLIRP_MEMORY_FILE",
    ]) {
        cmd.env_remove(k);
    }
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", program.display()))?;
    #[cfg(windows)]
    let tree = child
        .raw_handle()
        .map(blirp_core::proc_tree::ProcessTree::for_process_handle);
    #[cfg(unix)]
    let tree = child
        .id()
        .map(blirp_core::proc_tree::ProcessTree::for_process_group);

    let mut sin = child.stdin.take();
    let mut sout = child.stdout.take();
    let mut serr = child.stderr.take();
    let io = async {
        let write = async {
            if let Some(mut s) = sin.take() {
                // A child that exits without reading stdin closes the pipe; that is its business.
                let _ = s.write_all(stdin.as_bytes()).await;
                let _ = s.shutdown().await;
            }
        };
        let read = |p: Option<_>| async move {
            let mut buf = Vec::new();
            if let Some(mut p) = p {
                let p: &mut (dyn tokio::io::AsyncRead + Unpin + Send) = &mut p;
                let _ = p.take(16 * 1024 * 1024).read_to_end(&mut buf).await;
            }
            buf
        };
        let out: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>> =
            sout.take().map(|s| Box::new(s) as _);
        let err: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>> =
            serr.take().map(|s| Box::new(s) as _);
        let ((), o, e) = tokio::join!(write, read(out), read(err));
        let status = child.wait().await;
        (status, o, e)
    };
    match tokio::time::timeout(timeout, io).await {
        Ok((status, o, e)) => {
            let status = status.map_err(|e| format!("waiting for summarizer: {e}"))?;
            Ok(ProcOut {
                code: status.code(),
                stdout: String::from_utf8_lossy(&o).into_owned(),
                stderr: String::from_utf8_lossy(&e).into_owned(),
            })
        }
        Err(_) => {
            if let Some(t) = &tree {
                t.force_kill();
            }
            // kill_on_drop reaps the direct child when `io` is dropped.
            Err(format!("timed out after {}s", timeout.as_secs()))
        }
    }
}

fn tail(s: &str) -> String {
    clip(&one_line(s), 300)
}

/// Fresh working dir for one summarizer run, inside `root`
/// (`BLIRP_HOME/distill`). Ingest skips transcripts whose cwd is inside
/// BLIRP_HOME outside `worktrees/` (§8), so a summarizer that does persist a
/// session is never ingested and distilled in turn.
fn scratch_dir(root: &Path) -> Result<tempfile::TempDir, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("scratch dir {}: {e}", root.display()))?;
    tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(root)
        .map_err(|e| format!("scratch dir {}: {e}", root.display()))
}

impl Backend {
    async fn run(&self, prompt: String, timeout: Duration) -> Result<String, String> {
        match self {
            Backend::Claude { exe: path, scratch } => {
                let dir = scratch_dir(scratch)?;
                let args: Vec<OsString> = [
                    "-p",
                    "--model",
                    "haiku",
                    "--output-format",
                    "json",
                    "--safe-mode",
                    "--strict-mcp-config",
                    "--no-session-persistence",
                    "--tools",
                    "",
                ]
                .iter()
                .map(OsString::from)
                .collect();
                let out = run_process(path, args, prompt, dir.path(), timeout).await?;
                parse_claude_json(&out.stdout).map_err(|e| {
                    format!("{e} (exit {:?}; stderr: {})", out.code, tail(&out.stderr))
                })
            }
            Backend::Codex { exe: path, scratch } => {
                let dir = scratch_dir(scratch)?;
                let last = dir.path().join("last-message.txt");
                let out = run_process(path, codex_args(&last), prompt, dir.path(), timeout).await?;
                match std::fs::read_to_string(&last) {
                    Ok(t) if !t.trim().is_empty() => Ok(t),
                    _ => parse_codex_jsonl(&out.stdout).map_err(|e| {
                        format!("{e} (exit {:?}; stderr: {})", out.code, tail(&out.stderr))
                    }),
                }
            }
            Backend::Ollama { base, model } => {
                let client = reqwest::Client::builder()
                    .timeout(timeout)
                    .build()
                    .map_err(|e| e.to_string())?;
                let body = serde_json::json!({
                    "model": model,
                    "stream": false,
                    "format": "json",
                    "messages": [{"role": "user", "content": prompt}],
                    "options": {"num_ctx": 32768},
                });
                let resp = client
                    .post(format!("{base}/api/chat"))
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| format!("ollama request failed: {e}"))?;
                let status = resp.status();
                let v: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| format!("ollama reply is not JSON: {e}"))?;
                if !status.is_success() {
                    return Err(format!(
                        "ollama returned {status}: {}",
                        tail(&v.to_string())
                    ));
                }
                v.pointer("/message/content")
                    .and_then(|c| c.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| "ollama reply has no message content".into())
            }
        }
    }
}

impl Summarize for Backend {
    fn name(&self) -> &'static str {
        match self {
            Backend::Claude { .. } => "claude",
            Backend::Codex { .. } => "codex",
            Backend::Ollama { .. } => "ollama",
        }
    }

    fn complete(&self, prompt: String) -> impl Future<Output = Result<String, String>> + Send {
        self.run(prompt, DISTILL_TIMEOUT)
    }
}

/// `claude -p --output-format json` prints one result object.
pub fn parse_claude_json(stdout: &str) -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("claude output is not JSON: {e}"))?;
    let result = v.get("result").and_then(|r| r.as_str()).unwrap_or_default();
    if v.get("is_error").and_then(|b| b.as_bool()) == Some(true) {
        return Err(format!("claude reported an error: {}", tail(result)));
    }
    if result.trim().is_empty() {
        return Err("claude returned an empty result".into());
    }
    Ok(result.to_string())
}

/// Last agent message from `codex exec --json` JSONL events.
pub fn parse_codex_jsonl(stdout: &str) -> Result<String, String> {
    let mut last = None;
    let mut error = None;
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("item.completed") => {
                let item = v.get("item");
                if item.and_then(|i| i.get("type")).and_then(|t| t.as_str())
                    == Some("agent_message")
                    && let Some(t) = item.and_then(|i| i.get("text")).and_then(|t| t.as_str())
                {
                    last = Some(t.to_string());
                }
            }
            Some("error") | Some("turn.failed") => {
                error = v
                    .get("message")
                    .or_else(|| v.pointer("/error/message"))
                    .and_then(|m| m.as_str())
                    .map(str::to_string);
            }
            _ => {}
        }
    }
    last.ok_or_else(|| match error {
        Some(e) => format!("codex failed: {}", tail(&e)),
        None => "codex produced no agent message".into(),
    })
}

// ---------------------------------------------------------------- run + apply

fn merge_items(prev: &[SummaryItem], new: Vec<SummaryItem>) -> Vec<SummaryItem> {
    let mut out: Vec<SummaryItem> = prev
        .iter()
        .filter(|p| {
            !new.iter()
                .any(|n| norm_title(&n.title) == norm_title(&p.title))
        })
        .cloned()
        .collect();
    out.extend(new);
    out
}

fn union(a: &[String], b: &[String]) -> Vec<String> {
    let mut out = a.to_vec();
    out.extend(b.iter().filter(|x| !a.contains(x)).cloned());
    out
}

fn items(v: &[Item]) -> Vec<SummaryItem> {
    v.iter()
        .map(|i| SummaryItem {
            title: i.title.clone(),
            body: i.body.clone(),
        })
        .collect()
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
) -> Result<T, DistillError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| DistillError::Backend(format!("background task failed: {e}")))?
        .map_err(DistillError::from)
}

/// Distill one session with `backend` and apply the result (§9).
pub async fn run_distill<S: Summarize>(
    store: Arc<Store>,
    session_id: &str,
    backend: &S,
    cfg: &MemoryConfig,
) -> Result<DistillOutcome, DistillError> {
    let (st, sid) = (store.clone(), session_id.to_string());
    let max = i64::from(cfg.distill_max_chars);
    let (session, events, continued, brief, records) = blocking(move || {
        let session = st
            .get_session(&sid)?
            .ok_or(StoreError::NotFound("session"))?;
        // Only what the last summary does not cover yet; with nothing new
        // (a manual re-run) the whole session again.
        let (head, tail) = (max / 5, max - max / 5);
        let mut events = st.distill_events(&sid, session.distilled_through_seq, head, tail)?;
        let continued = session.distilled_through_seq > 0 && !events.head.is_empty();
        if events.head.is_empty() {
            events = st.distill_events(&sid, 0, head, tail)?;
        }
        let brief = st.get_brief(&session.project_id)?;
        let records = st.list_records(
            &session.project_id,
            &RecordFilter {
                status: Some(RecordStatus::Active),
                kind: None,
            },
        )?;
        Ok((session, events, continued, brief, records))
    })
    .await?;
    let through = events
        .tail
        .last()
        .or(events.head.last())
        .map(|e| e.seq)
        .ok_or(DistillError::NothingToDistill)?;
    let previous: Option<SessionSummary> = session
        .summary
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .filter(|p: &SessionSummary| continued && p.summary.is_some());
    let transcript = compact_selected(&events, cfg.distill_max_chars as usize);
    let transcript = blirp_core::redact::redact(&transcript).into_owned();
    let prompt = build_prompt(
        brief.as_ref().map(|b| b.body_md.as_str()),
        &records,
        previous.as_ref(),
        &transcript,
    );
    let known: HashSet<String> = records.iter().map(|r| r.id.clone()).collect();

    let first = backend
        .complete(prompt.clone())
        .await
        .map_err(DistillError::Backend)?;
    let out = match parse_output(&first, &known) {
        Ok(o) => o,
        Err(problem) => {
            tracing::info!(session = %session_id, %problem, "distill reply invalid; retrying once");
            let retry = format!(
                "{prompt}\n\nYour previous reply was rejected: {problem}. Reply again with ONLY the JSON object."
            );
            let second = backend
                .complete(retry)
                .await
                .map_err(DistillError::Backend)?;
            parse_output(&second, &known).map_err(DistillError::Invalid)?
        }
    };

    // A continued summary keeps the earlier items; a changed one replaces
    // its namesake.
    let prev = previous.unwrap_or_default();
    let summary = SessionSummary {
        title: Some(out.title.clone()),
        summary: Some(out.summary.clone()),
        decisions: merge_items(&prev.decisions, items(&out.decisions)),
        open_threads: merge_items(&prev.open_threads, items(&out.open_threads)),
        gotchas: merge_items(&prev.gotchas, items(&out.gotchas)),
        resolved_record_ids: union(&prev.resolved_record_ids, &out.resolved_record_ids),
        files: union(&prev.files, &out.files),
        backend: Some(backend.name().to_string()),
        distilled_at: Some(blirp_core::now_ms()),
        through_seq: through,
        error: None,
    };
    let mut new_records = Vec::new();
    for (kind, list) in [
        (RecordKind::Decision, &out.decisions),
        (RecordKind::OpenThread, &out.open_threads),
        (RecordKind::Gotcha, &out.gotchas),
    ] {
        new_records.extend(list.iter().map(|i| (kind, i.title.clone(), i.body.clone())));
    }
    let plan = DistillPlan {
        session_id: session.id.clone(),
        through_seq: through,
        summary: serde_json::to_value(&summary)
            .map_err(|e| DistillError::Backend(e.to_string()))?,
        title: Some(out.title),
        new_records,
        resolve_record_ids: out.resolved_record_ids,
        brief_md: Some(out.brief_md).filter(|b| !b.is_empty()),
        brief_apply: match cfg.brief_mode {
            BriefMode::Auto => BriefApply::Write,
            BriefMode::Review => BriefApply::Suggest,
        },
        brief_base: brief.as_ref().map(|b| b.id.clone()),
    };
    blocking(move || store.apply_distill(&plan)).await
}

/// Record a failed attempt in `summary_json.error` (keeping any earlier summary).
pub fn record_failure(
    store: &Store,
    session_id: &str,
    message: &str,
) -> Result<blirp_core::model::Session, StoreError> {
    let through = store.max_event_seq(session_id)?;
    store.modify_session(session_id, |s| {
        let mut sum: SessionSummary = s
            .summary
            .as_ref()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        sum.error = Some(blirp_core::model::DistillFailure {
            message: clip(message, 500),
            at: blirp_core::now_ms(),
            through_seq: through,
        });
        if let Ok(v) = serde_json::to_value(&sum) {
            s.summary = Some(v);
        }
    })
}

// ---------------------------------------------------------------- breaker

/// First pause after the summarizer itself failed, doubled on every failed
/// retry up to [`PAUSE_MAX_MS`].
const PAUSE_MIN_MS: i64 = 5 * 60 * 1000;
const PAUSE_MAX_MS: i64 = 6 * 3600 * 1000;

/// A failure of the summarizer itself (credentials, installation, limits),
/// which would fail every session alike, as opposed to one session's
/// content. `None`: a per-session failure.
pub fn classify(e: &DistillError) -> Option<DistillPause> {
    let msg = match e {
        DistillError::NoBackend(_) => return Some(DistillPause::Unavailable),
        DistillError::Backend(m) => m.to_lowercase(),
        _ => return None,
    };
    let any = |words: &[&str]| words.iter().any(|w| msg.contains(w));
    if any(&[
        "not logged in",
        "please log in",
        "/login",
        "log in",
        "unauthorized",
        "authentication",
        "invalid api key",
        "api key",
        "credit balance",
        "status 401",
        "status 403",
    ]) {
        Some(DistillPause::Auth)
    } else if any(&[
        "rate limit",
        "rate_limit",
        "usage limit",
        "too many requests",
        "status 429",
        "quota",
        "overloaded",
    ]) {
        Some(DistillPause::RateLimited)
    } else if any(&[
        "cannot start",
        "not on path",
        "not reachable",
        "connection refused",
    ]) {
        Some(DistillPause::Unavailable)
    } else {
        None
    }
}

/// Circuit breaker for the summarizer: while open, automatic jobs are left
/// alone (no budget, no per-session failure); the first job after
/// `open_until` is the probe.
#[derive(Debug, Default)]
struct Breaker {
    failures: u32,
    open_until: i64,
    pause: Option<(DistillPause, String)>,
}

impl Breaker {
    fn is_open(&self, now: i64) -> bool {
        self.pause.is_some() && now < self.open_until
    }

    /// Record a summarizer failure; returns when it will be retried.
    fn trip(&mut self, kind: DistillPause, message: &str, now: i64) -> i64 {
        self.failures = self.failures.saturating_add(1);
        let shift = self.failures.saturating_sub(1).min(16);
        let pause = PAUSE_MIN_MS.saturating_mul(1 << shift).min(PAUSE_MAX_MS);
        self.open_until = now + pause;
        self.pause = Some((kind, clip(message, 300)));
        self.open_until
    }

    /// The summarizer worked; true when it had been paused.
    fn reset(&mut self) -> bool {
        let was = self.pause.is_some();
        *self = Breaker::default();
        was
    }
}

// ---------------------------------------------------------------- queue

struct Job {
    session_id: String,
    manual: bool,
    /// Unix ms before which the job does not start.
    not_before: i64,
}

/// A session that just ended is distilled after this delay, so the
/// transcript's last lines are ingested first and the SessionEnd hook and
/// the process exit (both enqueue) make one job, not two.
pub const ENDED_DELAY_MS: i64 = 10_000;

/// Single-job distill queue with a daily budget (§9).
pub struct Distiller {
    tx: mpsc::UnboundedSender<Job>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<Job>>>,
    queued: Mutex<HashSet<String>>,
    breaker: Mutex<Breaker>,
    /// Day on which "budget used up" was last logged (once per day).
    budget_logged: Mutex<Option<String>>,
}

impl Default for Distiller {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            queued: Mutex::new(HashSet::new()),
            breaker: Mutex::new(Breaker::default()),
            budget_logged: Mutex::new(None),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Plain state; a panic elsewhere cannot leave it inconsistent.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn today() -> String {
    crate::memory::render::date(blirp_core::now_ms())
}

/// Jobs run today (UTC).
pub fn budget_used(store: &Store) -> Result<u64, StoreError> {
    let day = today();
    Ok(store
        .get_setting(BUDGET_KEY)?
        .filter(|v| v.get("day").and_then(|d| d.as_str()) == Some(day.as_str()))
        .and_then(|v| v.get("count").and_then(|c| c.as_u64()))
        .unwrap_or(0))
}

/// Give back a unit taken for a job the summarizer never really ran
/// (it failed as a whole, see [`classify`]).
fn refund_budget(store: &Store) -> Result<(), StoreError> {
    let used = budget_used(store)?;
    if used > 0 {
        store.set_setting(
            BUDGET_KEY,
            &serde_json::json!({"day": today(), "count": used - 1}),
        )?;
    }
    Ok(())
}

/// Consume one unit of today's budget; false when it is exhausted.
fn take_budget(store: &Store, limit: u32) -> Result<bool, StoreError> {
    let day = today();
    let used = budget_used(store)?;
    if used >= u64::from(limit) {
        return Ok(false);
    }
    store.set_setting(
        BUDGET_KEY,
        &serde_json::json!({"day": day, "count": used + 1}),
    )?;
    Ok(true)
}

impl Distiller {
    /// Pause state for the settings view (budget filled in by the caller).
    pub fn status(&self, now: i64) -> DistillStatus {
        let b = lock(&self.breaker);
        match &b.pause {
            Some((kind, reason)) => DistillStatus {
                paused: Some(*kind),
                reason: Some(reason.clone()),
                retry_at: Some(b.open_until.max(now)),
                ..DistillStatus::default()
            },
            None => DistillStatus::default(),
        }
    }

    /// Queue a session; false when it is already queued or running.
    pub fn enqueue(&self, session_id: &str, manual: bool) -> bool {
        self.push(session_id, manual, 0)
    }

    /// Queue a session that just ended (SessionEnd hook, process exit),
    /// starting after [`ENDED_DELAY_MS`].
    pub fn enqueue_ended(&self, session_id: &str) -> bool {
        self.push(session_id, false, blirp_core::now_ms() + ENDED_DELAY_MS)
    }

    fn push(&self, session_id: &str, manual: bool, not_before: i64) -> bool {
        let mut q = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
        if !q.insert(session_id.to_string()) {
            return false;
        }
        self.tx
            .send(Job {
                session_id: session_id.to_string(),
                manual,
                not_before,
            })
            .is_ok()
    }

    /// Start the worker and the idle/ended scheduler. Call once per daemon.
    pub fn start(state: SharedState) {
        let rx = state
            .distiller
            .rx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(mut rx) = rx else {
            tracing::error!("distill worker already started");
            return;
        };
        let worker_state = state.clone();
        let mut shutdown = state.shutdown.clone();
        tokio::spawn(async move {
            loop {
                let job = tokio::select! {
                    j = rx.recv() => match j { Some(j) => j, None => break },
                    _ = shutdown.changed() => break,
                };
                let wait = job.not_before - blirp_core::now_ms();
                if wait > 0 {
                    let wait = Duration::from_millis(wait.unsigned_abs());
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = shutdown.changed() => break,
                    }
                }
                process(&worker_state, &job).await;
                worker_state
                    .distiller
                    .queued
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&job.session_id);
            }
        });
        let mut shutdown = state.shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SCHEDULE_TICK);
            loop {
                tokio::select! {
                    _ = tick.tick() => schedule(&state).await,
                    _ = shutdown.changed() => break,
                }
            }
        });
    }
}

async fn schedule(state: &SharedState) {
    let cfg = state.config().memory;
    if cfg.summarizer == Summarizer::None {
        return;
    }
    let store = state.store.clone();
    let machine = state.machine.id.clone();
    let now = blirp_core::now_ms();
    let idle_before = now - i64::from(cfg.distill_idle_secs) * 1000;
    let found = tokio::task::spawn_blocking(move || {
        store.distill_candidates(&machine, idle_before, now - AUTO_WINDOW_MS, 20)
    })
    .await;
    match found {
        Ok(Ok(list)) => {
            for s in list {
                state.distiller.enqueue(&s.id, false);
            }
        }
        Ok(Err(e)) => tracing::warn!(error = %e, "finding sessions to distill failed"),
        Err(e) => tracing::error!(error = %e, "distill scheduler task failed"),
    }
}

/// Why a queued job must not run (checked before it takes budget). Sessions
/// of other machines are distilled on their origin machine and arrive by
/// replication. Ingested subagent children (external with a parent) are
/// covered by their parent: its transcript holds the subagent's task (tool
/// call) and final report (tool result); they are distilled only on request.
pub fn skip_reason(s: &Session, local_machine: &str, manual: bool) -> Option<&'static str> {
    if s.machine_id != local_machine {
        return Some("session belongs to another machine");
    }
    if !manual && s.origin == SessionOrigin::External && s.parent_session_id.is_some() {
        return Some("subagent session (covered by its parent)");
    }
    None
}

async fn process(state: &SharedState, job: &Job) {
    let cfg = state.config().memory;
    if cfg.summarizer == Summarizer::None && !job.manual {
        return;
    }
    let store = state.store.clone();
    let sid = job.session_id.clone();
    let lookup = move || -> Result<_, StoreError> {
        let s = store.get_session(&sid)?;
        let max = store.max_event_seq(&sid)?;
        Ok(s.map(|s| (s, max)))
    };
    match tokio::task::spawn_blocking(lookup).await {
        Ok(Ok(Some((s, max_seq)))) => {
            if let Some(why) = skip_reason(&s, &state.machine.id, job.manual) {
                tracing::debug!(session = %job.session_id, why, "not distilling");
                return;
            }
            // Nothing past the last summary (e.g. queued twice): no budget spent.
            if !job.manual && max_seq <= s.distilled_through_seq {
                tracing::debug!(session = %job.session_id, "nothing new to distill");
                return;
            }
        }
        Ok(Ok(None)) => return,
        Ok(Err(e)) => {
            tracing::warn!(session = %job.session_id, error = %e, "loading session to distill failed");
            return;
        }
        Err(e) => {
            tracing::error!(error = %e, "distill session lookup task failed");
            return;
        }
    }
    // A failing summarizer pauses automatic jobs; they stay eligible and run
    // once it works again. A manual distill always tries (and can close it).
    if !job.manual && lock(&state.distiller.breaker).is_open(blirp_core::now_ms()) {
        tracing::debug!(session = %job.session_id, "summarizer paused; not distilling");
        return;
    }
    let store = state.store.clone();
    let limit = cfg.daily_distill_limit;
    match tokio::task::spawn_blocking(move || take_budget(&store, limit)).await {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            let day = today();
            let mut logged = lock(&state.distiller.budget_logged);
            if logged.as_deref() != Some(day.as_str()) {
                tracing::info!(
                    limit,
                    "daily distill budget used up; distilling resumes tomorrow (UTC)"
                );
                *logged = Some(day);
            } else {
                tracing::debug!(session = %job.session_id, "daily distill budget used up; skipping");
            }
            return;
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "reading distill budget failed");
            return;
        }
        Err(e) => {
            tracing::error!(error = %e, "distill budget task failed");
            return;
        }
    }
    let result = match select_backend(&cfg, &state.paths.distill_dir()).await {
        Ok(Some(b)) => run_distill(state.store.clone(), &job.session_id, &b, &cfg).await,
        Ok(None) => Err(DistillError::NoBackend(
            "memory.summarizer = \"none\"".into(),
        )),
        Err(e) => Err(DistillError::NoBackend(e)),
    };
    let now = blirp_core::now_ms();
    match &result {
        Err(e) => {
            if let Some(kind) = classify(e) {
                let retry_at = lock(&state.distiller.breaker).trip(kind, &e.to_string(), now);
                let store = state.store.clone();
                if let Ok(Err(e)) = tokio::task::spawn_blocking(move || refund_budget(&store)).await
                {
                    tracing::warn!(error = %e, "returning distill budget failed");
                }
                // Logged once per failed attempt (at most every 5 min .. 6 h).
                tracing::warn!(
                    reason = ?kind,
                    error = %e,
                    retry_in_secs = (retry_at - now) / 1000,
                    "summarizer unavailable; automatic distilling paused"
                );
                return;
            }
        }
        Ok(_) => {
            if lock(&state.distiller.breaker).reset() {
                tracing::info!("summarizer works again; automatic distilling resumed");
            }
        }
    }
    match result {
        Ok(outcome) => {
            tracing::info!(
                session = %job.session_id,
                created = outcome.records_created,
                updated = outcome.records_updated,
                resolved = outcome.records_resolved,
                suggestions = outcome.suggestions_created,
                brief = outcome.brief_updated,
                "session distilled"
            );
            if let Some(s) = outcome.session {
                let pid = s.project_id.clone();
                state.emit(ServerEvent::SessionUpdated { session: s });
                state.emit(ServerEvent::MemoryUpdated {
                    project_id: pid.clone(),
                    part: MemoryPart::Records,
                });
                if outcome.brief_updated {
                    state.emit(ServerEvent::MemoryUpdated {
                        project_id: pid.clone(),
                        part: MemoryPart::Brief,
                    });
                }
                if outcome.suggestions_created > 0 {
                    state.emit(ServerEvent::MemoryUpdated {
                        project_id: pid,
                        part: MemoryPart::Suggestions,
                    });
                }
            }
        }
        Err(DistillError::NothingToDistill) => {}
        Err(e) => {
            tracing::warn!(session = %job.session_id, error = %e, "distill failed");
            let store = state.store.clone();
            let sid = job.session_id.clone();
            let msg = e.to_string();
            match tokio::task::spawn_blocking(move || record_failure(&store, &sid, &msg)).await {
                Ok(Ok(s)) => state.emit(ServerEvent::SessionUpdated { session: s }),
                Ok(Err(e)) => tracing::warn!(error = %e, "recording distill failure failed"),
                Err(e) => tracing::error!(error = %e, "distill failure task failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::memory::testutil::*;
    use blirp_core::model::{SuggestionStatus, SuggestionTarget};
    use std::collections::VecDeque;

    struct Fake(
        Mutex<VecDeque<Result<String, String>>>,
        Mutex<usize>,
        Mutex<Vec<String>>,
    );

    impl Fake {
        fn new(replies: Vec<Result<String, String>>) -> Self {
            Self(
                Mutex::new(replies.into()),
                Mutex::new(0),
                Mutex::new(Vec::new()),
            )
        }
        fn prompts(&self) -> Vec<String> {
            self.2.lock().unwrap().clone()
        }
        fn calls(&self) -> usize {
            *self.1.lock().unwrap()
        }
    }

    impl Summarize for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn complete(&self, prompt: String) -> impl Future<Output = Result<String, String>> + Send {
            *self.1.lock().unwrap() += 1;
            self.2.lock().unwrap().push(prompt);
            let r = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err("no more replies".into()));
            async move { r }
        }
    }

    fn ev(seq: i64, kind: EventKind, text: &str) -> Event {
        Event {
            session_id: "s".into(),
            seq,
            ts: seq,
            kind,
            text: text.into(),
            meta: None,
        }
    }

    #[test]
    fn compaction_formats_and_keeps_head_and_tail() {
        let events = vec![
            ev(1, EventKind::User, "please fix\nthe bug"),
            ev(
                2,
                EventKind::ToolCall,
                &format!("Bash(ls {})", "a".repeat(1000)),
            ),
            ev(3, EventKind::ToolResult, &"r".repeat(2000)),
            ev(4, EventKind::Assistant, "done"),
        ];
        let small = compact_transcript(&events, 100_000);
        assert!(small.starts_with("USER: please fix\nthe bug\nTOOL: Bash(ls"));
        assert!(
            small
                .lines()
                .any(|l| l.starts_with("RESULT: ") && l.chars().count() <= 408)
        );
        assert!(small.ends_with("ASSISTANT: done"));

        let long: Vec<Event> = (0..2000)
            .map(|i| ev(i, EventKind::User, &format!("message number {i:05}")))
            .collect();
        let out = compact_transcript(&long, 10_000);
        assert!(out.chars().count() <= 10_000);
        assert!(out.starts_with("USER: message number 00000"));
        assert!(out.ends_with("USER: message number 01999"));
        assert!(out.contains("characters omitted"));
        let marker = out.find("[...").unwrap();
        // Head is ~20% of the budget, tail ~80%.
        assert!(marker < 2_100 && marker > 1_800, "{marker}");
    }

    #[test]
    fn codex_is_opt_in_and_runs_without_tools() {
        let claude = Backend::Claude {
            exe: "claude".into(),
            scratch: "s".into(),
        };
        let ollama = Backend::Ollama {
            base: "b".into(),
            model: "m".into(),
        };
        assert_eq!(pick_auto(Some(claude), None).unwrap().name(), "claude");
        assert_eq!(pick_auto(None, Some(ollama)).unwrap().name(), "ollama");
        assert!(pick_auto(None, None).is_err());
        let args: Vec<String> = codex_args(Path::new("last.txt"))
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        for f in ["hooks", "shell_tool", "unified_exec", "view_image"] {
            assert!(
                args.windows(2).any(|w| w[0] == "--disable" && w[1] == f),
                "{f}: {args:?}"
            );
        }
        assert_eq!(args.last().map(String::as_str), Some("last.txt"));
    }

    #[test]
    fn output_validation() {
        let known: HashSet<String> = ["r1".to_string()].into();
        let ok = r#"Here you go: ```json
{"title":" Fix  the parser ","summary":"Did it.","decisions":[{"title":"Use nom","body":"fast"}],
 "open_threads":[],"resolved_record_ids":["r1","ghost"],"gotchas":[],"files":["./src/a.rs"],"brief_md":"B"}
```"#;
        let o = parse_output(ok, &known).unwrap();
        assert_eq!(o.title, "Fix the parser");
        assert_eq!(o.resolved_record_ids, ["r1"]);
        assert_eq!(o.files, ["src/a.rs"]);
        for bad in [
            "no json at all",
            r#"{"title":"","summary":"x"}"#,
            r#"{"title":"t","summary":""}"#,
            r#"{"title":"t","summary":"s","extra":1}"#,
            r#"{"title":"t","summary":"s","decisions":[{"title":""}]}"#,
            r#"{"title":"t","summary":"s","decisions":"nope"}"#,
            "{not json}",
        ] {
            assert!(parse_output(bad, &known).is_err(), "{bad}");
        }
        let long_brief = format!(
            r#"{{"title":"t","summary":"s","brief_md":"{}"}}"#,
            "b".repeat(13_000)
        );
        assert!(parse_output(&long_brief, &known).is_err());
        // A secret echoed by the model never reaches the stored summary.
        let key = "AKIAIOSFODNN7EXAMPLE";
        let leaky = format!(
            r#"{{"title":"t {key}","summary":"used {key}","gotchas":[{{"title":"g","body":"{key}"}}],"brief_md":"b {key}"}}"#
        );
        let o = parse_output(&leaky, &known).unwrap();
        let all = format!(
            "{} {} {} {}",
            o.title, o.summary, o.gotchas[0].body, o.brief_md
        );
        assert!(!all.contains(key) && all.contains("[REDACTED:"), "{all}");
    }

    #[test]
    fn backend_output_parsers() {
        assert_eq!(
            parse_claude_json(r#"{"type":"result","is_error":false,"result":"{\"a\":1}"}"#)
                .unwrap(),
            r#"{"a":1}"#
        );
        assert!(parse_claude_json(r#"{"is_error":true,"result":"Not logged in"}"#).is_err());
        assert!(parse_claude_json("garbage").is_err());
        let jsonl = "{\"type\":\"thread.started\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"first\"}}\nnoise\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"{}\"}}\n";
        assert_eq!(parse_codex_jsonl(jsonl).unwrap(), "{}");
        let failed = "{\"type\":\"turn.failed\",\"error\":{\"message\":\"quota\"}}";
        assert!(parse_codex_jsonl(failed).unwrap_err().contains("quota"));
    }

    fn cfg(mode: BriefMode) -> MemoryConfig {
        MemoryConfig {
            brief_mode: mode,
            ..MemoryConfig::default()
        }
    }

    fn seed() -> (tempfile::TempDir, Arc<Store>, String) {
        let (d, store, pid) = project_store("P");
        store.insert_session(&session("s", &pid, 1000)).unwrap();
        event(&store, "s", 1, EventKind::User, "add a cache");
        event(&store, "s", 2, EventKind::Assistant, "added an LRU cache");
        store.put_brief(&pid, "old brief", "user").unwrap();
        (d, Arc::new(store), pid)
    }

    fn reply(resolved: &[&str]) -> String {
        serde_json::json!({
            "title": "Add LRU cache",
            "summary": "Added a cache.",
            "decisions": [{"title": "Use lru crate", "body": "small"}],
            "open_threads": [{"title": "Tune cache size", "body": ""}],
            "resolved_record_ids": resolved,
            "gotchas": [],
            "files": ["src/cache.rs"],
            "brief_md": "new brief"
        })
        .to_string()
    }

    #[tokio::test]
    async fn auto_mode_applies_everything_and_protects_user_records() {
        let (_d, store, pid) = seed();
        let mine = record(
            &store,
            &pid,
            RecordKind::OpenThread,
            "Mine",
            "user wrote",
            5,
            false,
        );
        store
            .modify_record(&mine.id, |r| r.updated_by = "user".into())
            .unwrap();
        let bot = record(
            &store,
            &pid,
            RecordKind::OpenThread,
            "Add cache",
            "",
            5,
            false,
        );
        let fake = Fake::new(vec![Ok(reply(&[&mine.id, &bot.id]))]);
        let out = run_distill(store.clone(), "s", &fake, &cfg(BriefMode::Auto))
            .await
            .unwrap();
        assert_eq!(
            (
                out.records_created,
                out.records_resolved,
                out.suggestions_created
            ),
            (2, 1, 1)
        );
        assert!(out.brief_updated);

        let s = store.get_session("s").unwrap().unwrap();
        assert_eq!(s.title.as_deref(), Some("Add LRU cache"));
        assert_eq!(s.distilled_through_seq, 2);
        let sum: SessionSummary = serde_json::from_value(s.summary.unwrap()).unwrap();
        assert_eq!(sum.backend.as_deref(), Some("fake"));
        assert_eq!(sum.files, ["src/cache.rs"]);

        // User-edited record untouched; a suggestion to resolve it instead.
        let m = store.get_record(&mine.id).unwrap().unwrap();
        assert_eq!(
            (m.status, m.updated_by.as_str(), m.body.as_str()),
            (RecordStatus::Active, "user", "user wrote")
        );
        assert_eq!(
            store.get_record(&bot.id).unwrap().unwrap().status,
            RecordStatus::Resolved
        );
        let sugg = store
            .list_suggestions(&pid, Some(SuggestionStatus::Pending))
            .unwrap();
        assert_eq!(sugg.len(), 1);
        assert_eq!(
            (sugg[0].target, sugg[0].target_id.as_deref()),
            (SuggestionTarget::Record, Some(mine.id.as_str()))
        );

        // Brief versioned by the distiller; the user can revert.
        let hist = store.brief_history(&pid).unwrap();
        assert_eq!(hist[0].body_md, "new brief");
        assert_eq!(hist[0].updated_by, "distiller");
        store.revert_brief(&pid, 1, "user").unwrap();
        assert_eq!(store.get_brief(&pid).unwrap().unwrap().body_md, "old brief");

        // Re-distilling does not duplicate records.
        let again = Fake::new(vec![Ok(reply(&[]))]);
        let out = run_distill(store.clone(), "s", &again, &cfg(BriefMode::Auto))
            .await
            .unwrap();
        assert_eq!(out.records_created, 0);
    }

    // A re-distill sends only the events after the last summary (with that
    // summary as context), and refreshes the session's own records instead
    // of adding near-duplicates.
    #[tokio::test]
    async fn redistill_continues_and_updates_its_own_records() {
        let (_d, store, pid) = seed();
        let first = Fake::new(vec![Ok(reply(&[]))]);
        run_distill(store.clone(), "s", &first, &cfg(BriefMode::Auto))
            .await
            .unwrap();
        event(&store, "s", 3, EventKind::User, "make it thread safe");
        event(
            &store,
            "s",
            4,
            EventKind::Assistant,
            "wrapped it in a mutex",
        );
        let second = serde_json::json!({
            "title": "Add a thread-safe LRU cache",
            "summary": "Added a cache and made it thread safe.",
            "decisions": [{"title": "use LRU crate!", "body": "small and fast"}],
            "open_threads": [],
            "resolved_record_ids": [],
            "gotchas": [{"title": "Mutex poisoning", "body": "recover the guard"}],
            "files": ["src/lock.rs"],
            "brief_md": ""
        })
        .to_string();
        let again = Fake::new(vec![Ok(second)]);
        let out = run_distill(store.clone(), "s", &again, &cfg(BriefMode::Auto))
            .await
            .unwrap();
        let prompt = &again.prompts()[0];
        assert!(
            prompt.contains("PREVIOUS SUMMARY OF THIS SESSION"),
            "{prompt}"
        );
        assert!(prompt.contains("Summary: Added a cache."), "{prompt}");
        assert!(prompt.contains("make it thread safe"), "{prompt}");
        assert!(
            !prompt.contains("add a cache"),
            "old events were sent again"
        );
        assert_eq!((out.records_created, out.records_updated), (1, 1));
        let decisions = store
            .list_records(
                &pid,
                &RecordFilter {
                    status: None,
                    kind: Some(RecordKind::Decision),
                },
            )
            .unwrap();
        assert_eq!(decisions.len(), 1, "{decisions:?}");
        assert_eq!(decisions[0].body, "small and fast");

        let s = store.get_session("s").unwrap().unwrap();
        assert_eq!(s.distilled_through_seq, 4);
        let sum: SessionSummary = serde_json::from_value(s.summary.unwrap()).unwrap();
        assert_eq!(sum.decisions.len(), 1);
        assert_eq!(sum.decisions[0].body, "small and fast");
        assert_eq!(sum.open_threads.len(), 1, "earlier items are kept");
        assert_eq!(sum.gotchas.len(), 1);
        assert_eq!(sum.files, ["src/cache.rs", "src/lock.rs"]);
    }

    /// Edits the brief (like a user in the UI) while the summarizer runs.
    struct EditsBrief(Arc<Store>, String);

    impl Summarize for EditsBrief {
        fn name(&self) -> &'static str {
            "edits-brief"
        }
        fn complete(&self, _prompt: String) -> impl Future<Output = Result<String, String>> + Send {
            self.0.put_brief(&self.1, "user edit", "user").unwrap();
            async { Ok(reply(&[])) }
        }
    }

    #[tokio::test]
    async fn brief_edited_during_a_run_is_kept_and_the_update_suggested() {
        let (_d, store, pid) = seed();
        let backend = EditsBrief(store.clone(), pid.clone());
        let out = run_distill(store.clone(), "s", &backend, &cfg(BriefMode::Auto))
            .await
            .unwrap();
        assert!(!out.brief_updated);
        assert_eq!(store.get_brief(&pid).unwrap().unwrap().body_md, "user edit");
        let sugg = store.list_suggestions(&pid, None).unwrap();
        assert_eq!(sugg.len(), 1);
        assert_eq!(sugg[0].target, SuggestionTarget::Brief);
        assert_eq!(sugg[0].proposal["body_md"], "new brief");
    }

    #[tokio::test]
    async fn ended_sessions_are_queued_once_after_a_delay() {
        let d = Distiller::default();
        let before = blirp_core::now_ms();
        assert!(d.enqueue_ended("s"));
        // The process exit right after the SessionEnd hook joins that job.
        assert!(!d.enqueue_ended("s"));
        assert!(!d.enqueue("s", false));
        let mut rx = d.rx.lock().unwrap().take().unwrap();
        let job = rx.try_recv().unwrap();
        assert!(job.not_before >= before + ENDED_DELAY_MS);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn review_mode_suggests_the_brief() {
        let (_d, store, pid) = seed();
        let fake = Fake::new(vec![Ok(reply(&[]))]);
        let out = run_distill(store.clone(), "s", &fake, &cfg(BriefMode::Review))
            .await
            .unwrap();
        assert!(!out.brief_updated);
        assert_eq!(store.get_brief(&pid).unwrap().unwrap().body_md, "old brief");
        let sugg = store.list_suggestions(&pid, None).unwrap();
        assert_eq!(sugg.len(), 1);
        assert_eq!(sugg[0].target, SuggestionTarget::Brief);
        assert_eq!(sugg[0].proposal["body_md"], "new brief");
    }

    #[tokio::test]
    async fn invalid_reply_is_retried_once_then_fails() {
        let (_d, store, _) = seed();
        let fake = Fake::new(vec![
            Ok("I think the session was about caching.".into()),
            Ok(reply(&[])),
        ]);
        run_distill(store.clone(), "s", &fake, &cfg(BriefMode::Auto))
            .await
            .unwrap();
        assert_eq!(fake.calls(), 2);

        let (_d2, store2, _) = seed();
        let garbage = Fake::new(vec![
            Ok("garbage".into()),
            Ok("{\"still\": \"bad\"}".into()),
        ]);
        let err = run_distill(store2.clone(), "s", &garbage, &cfg(BriefMode::Auto))
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::Invalid(_)), "{err}");
        assert_eq!(garbage.calls(), 2);
        let s = record_failure(&store2, "s", &err.to_string()).unwrap();
        let sum: SessionSummary = serde_json::from_value(s.summary.unwrap()).unwrap();
        let e = sum.error.unwrap();
        assert_eq!(e.through_seq, 2);
        assert!(e.message.contains("invalid"));
        // A failed session is not a candidate again until new events arrive.
        assert!(
            store2
                .distill_candidates("m", i64::MAX, 0, 10)
                .unwrap()
                .is_empty()
        );
        event(&store2, "s", 3, EventKind::User, "more");
        assert_eq!(
            store2
                .distill_candidates("m", i64::MAX, 0, 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn scheduling_ignores_subagents_and_remote_sessions() {
        let (_d, store, pid) = project_store("S");
        store
            .upsert_machine(&blirp_core::model::Machine {
                id: "other".into(),
                name: "laptop".into(),
                os: "linux".into(),
                role: blirp_core::model::MachineRole::Node,
                last_seen: 1,
                revoked: false,
            })
            .unwrap();
        let mut sessions = vec![session("top", &pid, 10)];
        // A continue-in / fork session: blirp-launched with lineage, distilled.
        let mut fork = session("fork", &pid, 10);
        fork.parent_session_id = Some("top".into());
        sessions.push(fork);
        // An ingested subagent child.
        let mut sub = session("sub", &pid, 10);
        sub.origin = SessionOrigin::External;
        sub.parent_session_id = Some("top".into());
        sessions.push(sub);
        // Replicated from another machine.
        let mut remote = session("remote", &pid, 10);
        remote.machine_id = "other".into();
        remote.origin = SessionOrigin::External;
        sessions.push(remote);
        for s in &sessions {
            store.insert_session(s).unwrap();
            event(&store, &s.id, 1, EventKind::User, "hello");
        }

        let mut due: Vec<String> = store
            .distill_candidates("m", i64::MAX, 0, 10)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        due.sort();
        assert_eq!(due, ["fork", "top"]);

        let by_id = |id: &str| sessions.iter().find(|s| s.id == id).unwrap();
        assert_eq!(skip_reason(by_id("top"), "m", false), None);
        assert_eq!(skip_reason(by_id("fork"), "m", false), None);
        // Queued subagents (SessionEnd hook, process exit) are skipped unless
        // the user asks; remote sessions are never distilled here.
        assert!(skip_reason(by_id("sub"), "m", false).is_some());
        assert_eq!(skip_reason(by_id("sub"), "m", true), None);
        assert!(skip_reason(by_id("remote"), "m", false).is_some());
        assert!(skip_reason(by_id("remote"), "m", true).is_some());
    }

    #[tokio::test]
    async fn backend_error_is_not_retried() {
        let (_d, store, _) = seed();
        let fake = Fake::new(vec![Err("boom".into()), Ok(reply(&[]))]);
        let err = run_distill(store, "s", &fake, &cfg(BriefMode::Auto))
            .await
            .unwrap_err();
        assert!(matches!(err, DistillError::Backend(_)));
        assert_eq!(fake.calls(), 1);
    }

    #[test]
    fn summarizer_failures_are_classified() {
        let backend = |m: &str| DistillError::Backend(m.into());
        assert_eq!(
            classify(&backend(
                "(exit Some(1); stderr: Not logged in · Please run /login)"
            )),
            Some(DistillPause::Auth)
        );
        assert_eq!(
            classify(&backend("HTTP status 429: rate limit exceeded")),
            Some(DistillPause::RateLimited)
        );
        assert_eq!(
            classify(&backend("cannot start claude: program not found")),
            Some(DistillPause::Unavailable)
        );
        assert_eq!(
            classify(&DistillError::NoBackend("none available".into())),
            Some(DistillPause::Unavailable)
        );
        // Per-session problems do not pause anything.
        assert_eq!(classify(&backend("timed out after 180s")), None);
        assert_eq!(classify(&DistillError::Invalid("bad json".into())), None);
    }

    #[test]
    fn breaker_backs_off_exponentially_and_resets() {
        let mut b = Breaker::default();
        assert!(!b.is_open(0));
        assert_eq!(b.trip(DistillPause::Auth, "not logged in", 0), PAUSE_MIN_MS);
        assert!(b.is_open(PAUSE_MIN_MS - 1));
        assert!(
            !b.is_open(PAUSE_MIN_MS),
            "the probe runs once the pause is over"
        );
        assert_eq!(b.trip(DistillPause::Auth, "x", 0), 2 * PAUSE_MIN_MS);
        assert_eq!(b.trip(DistillPause::Auth, "x", 0), 4 * PAUSE_MIN_MS);
        for _ in 0..30 {
            b.trip(DistillPause::Auth, "x", 0);
        }
        assert_eq!(b.open_until, PAUSE_MAX_MS);
        assert!(b.reset());
        assert!(!b.is_open(1));
        assert!(!b.reset());
    }

    #[test]
    fn refunded_budget_is_given_back() {
        let (_d, store, _) = project_store("R");
        assert!(take_budget(&store, 1).unwrap());
        assert!(!take_budget(&store, 1).unwrap());
        refund_budget(&store).unwrap();
        assert_eq!(budget_used(&store).unwrap(), 0);
        refund_budget(&store).unwrap();
        assert_eq!(budget_used(&store).unwrap(), 0);
    }

    #[test]
    fn budget_is_daily() {
        let (_d, store, _) = project_store("B");
        assert!(take_budget(&store, 2).unwrap());
        assert!(take_budget(&store, 2).unwrap());
        assert!(!take_budget(&store, 2).unwrap());
        store
            .set_setting(
                BUDGET_KEY,
                &serde_json::json!({"day": "1999-01-01", "count": 99}),
            )
            .unwrap();
        assert!(take_budget(&store, 2).unwrap());
    }

    /// End to end through a real child process: a fake `claude` script that
    /// prints a canned `--output-format json` envelope.
    #[tokio::test]
    async fn fake_claude_process_end_to_end() {
        let (d, store, pid) = seed();
        let canned = serde_json::json!({"type": "result", "is_error": false, "result": reply(&[])});
        let json_path = d.path().join("canned.json");
        std::fs::write(&json_path, canned.to_string()).unwrap();
        let script = fake_script(d.path(), "fake-claude", &json_path, false);
        let backend = Backend::Claude {
            exe: script,
            scratch: d.path().join("distill"),
        };
        run_distill(store.clone(), "s", &backend, &cfg(BriefMode::Auto))
            .await
            .unwrap();
        assert_eq!(store.get_brief(&pid).unwrap().unwrap().body_md, "new brief");
    }

    #[tokio::test]
    async fn summarizer_timeout_kills_the_process() {
        let d = tempfile::tempdir().unwrap();
        let script = fake_script(d.path(), "slow", &d.path().join("x"), true);
        let begin = std::time::Instant::now();
        let err = Backend::Claude {
            exe: script,
            scratch: d.path().join("distill"),
        }
        .run("prompt".into(), Duration::from_secs(1))
        .await
        .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(begin.elapsed() < Duration::from_secs(8));
    }

    /// Platform script that prints `json` (or sleeps) and ignores its args.
    fn fake_script(dir: &Path, name: &str, json: &Path, slow: bool) -> PathBuf {
        if cfg!(windows) {
            let p = dir.join(format!("{name}.cmd"));
            let body = if slow {
                "@echo off\r\nping -n 30 127.0.0.1 >nul\r\n".to_string()
            } else {
                format!("@echo off\r\nmore >nul\r\ntype \"{}\"\r\n", json.display())
            };
            std::fs::write(&p, body).unwrap();
            p
        } else {
            let p = dir.join(name);
            let body = if slow {
                "#!/bin/sh\nsleep 30\n".to_string()
            } else {
                format!("#!/bin/sh\ncat >/dev/null\ncat '{}'\n", json.display())
            };
            std::fs::write(&p, body).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            p
        }
    }
}
