//! blirp MCP server (§9): memory tools for agents over stdio (`blirp mcp`) and
//! Streamable HTTP (`/mcp` on the daemon), built with `rmcp`.
//!
//! stdio reads SQLite read-only and sends writes (`mem_record`) through the
//! daemon API; the HTTP server lives in the daemon and uses the store directly.

use crate::memory::render::{clip, date, parse_summary};
use crate::state::SharedState;
use blirp_core::model::{
    CreateRecord, MemoryPart, Record, RecordKind, RecordStatus, SearchHitKind, ServerEvent,
};
use blirp_core::paths::{Paths, RuntimeInfo};
use blirp_core::store::{RecordFilter, Store, StoreError};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Extensions, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const INSTRUCTIONS: &str = "blirp keeps memory of every coding-agent session in this project: \
a project brief, records (decisions, open threads, gotchas, notes, plans) and full searchable \
transcripts. The session-start context already contains the brief and the most recent items; use \
these tools to look further back: mem_search to find past discussions, mem_session to read one \
session, mem_recent for the latest sessions, mem_brief for the brief and all active records, \
mem_record to save something future sessions must know.";

#[derive(Clone)]
enum Writer {
    /// Inside the daemon: write through the store and notify the UI.
    Direct(SharedState),
    /// stdio process: write through the daemon API.
    Daemon(Paths),
}

#[derive(Clone)]
pub struct BlirpMcp {
    store: Arc<Store>,
    /// Current project (from `BLIRP_PROJECT_ID` or the client's folder).
    project: Option<String>,
    writer: Writer,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Words to search for (full-text, stemmed; all words must match, the last one as a prefix).
    pub query: String,
    /// "current" (default), "all", or a project id.
    #[serde(default)]
    pub project: Option<String>,
    /// Restrict to "record" (brief-level memory) or "event" (transcript messages). Default: both.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
    /// Maximum hits, 1-50 (default 10).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionArgs {
    /// Session id (from mem_search or mem_recent).
    pub session_id: String,
    /// Return events after this sequence number (default 0 = from the start).
    #[serde(default)]
    pub from_seq: Option<i64>,
    /// Maximum events, 1-200 (default 50).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProjectArgs {
    /// "current" (default) or a project id.
    #[serde(default)]
    pub project: Option<String>,
    /// Maximum sessions, 1-50 (default 10). Ignored by mem_brief.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BriefArgs {
    /// "current" (default) or a project id.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecordArgs {
    /// One of "decision", "plan", "note", "open_thread", "gotcha".
    pub kind: String,
    /// Short title (one line).
    pub title: String,
    /// Details: what, why, and anything a future session needs to act on it.
    pub body: String,
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// FTS snippets mark matches with U+0002/U+0003; show them as bold.
fn snippet(s: &str) -> String {
    one_line(&s.replace(['\u{2}', '\u{3}'], "**"))
}

fn record_block(r: &Record) -> String {
    let mut s = format!(
        "- [{}] {} (id {}, updated {}{})",
        r.kind,
        one_line(&r.title),
        r.id,
        date(r.updated_at),
        if r.pinned { ", pinned" } else { "" }
    );
    if !r.body.trim().is_empty() {
        s.push_str("\n  ");
        s.push_str(&clip(&one_line(&r.body), 1500));
    }
    s
}

impl BlirpMcp {
    fn new(store: Arc<Store>, project: Option<String>, writer: Writer) -> Self {
        Self {
            store,
            project,
            writer,
            tool_router: Self::tool_router(),
        }
    }

    /// Server for the daemon's `/mcp` endpoint.
    pub fn for_daemon(state: SharedState) -> Self {
        Self::new(state.store.clone(), None, Writer::Direct(state))
    }

    /// Server for `blirp mcp` (stdio): read-only store, project from env or cwd.
    pub fn for_stdio(
        paths: Paths,
        project_env: Option<String>,
        cwd: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let store = Arc::new(Store::open_read_only(&paths.db_file())?);
        let project = match project_env.filter(|p| !p.is_empty()) {
            Some(p) => Some(p),
            None => match (store.machine_id()?, cwd) {
                (Some(m), Some(cwd)) => store.find_project_for_path(&m, cwd)?.map(|p| p.id),
                _ => None,
            },
        };
        Ok(Self::new(store, project, Writer::Daemon(paths)))
    }

    /// `project` query parameter of an HTTP request (Streamable HTTP clients).
    fn http_project(ext: &Extensions) -> Option<String> {
        let parts = ext.get::<axum::http::request::Parts>()?;
        parts.uri.query()?.split('&').find_map(|kv| {
            kv.strip_prefix("project=")
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        })
    }

    fn current(&self, ext: &Extensions) -> Option<String> {
        self.project.clone().or_else(|| Self::http_project(ext))
    }

    /// Resolve a `project` argument: None/"current" -> current, "all" -> None.
    fn scope(
        &self,
        arg: Option<&str>,
        ext: &Extensions,
        allow_all: bool,
    ) -> Result<Option<String>, String> {
        match arg.map(str::trim) {
            None | Some("") | Some("current") => match self.current(ext) {
                Some(p) => Ok(Some(p)),
                None if allow_all => Ok(None),
                None => Err(
                    "no current project (the folder is not a blirp project); pass a project id"
                        .into(),
                ),
            },
            Some("all") if allow_all => Ok(None),
            Some("all") => {
                Err("this tool needs one project; pass \"current\" or a project id".into())
            }
            Some(id) => Ok(Some(id.to_string())),
        }
    }

    async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, String> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || f(&store))
            .await
            .map_err(|e| format!("internal error: {e}"))?
            .map_err(|e| e.to_string())
    }
}

#[tool_router(router = tool_router)]
impl BlirpMcp {
    #[tool(
        name = "mem_search",
        description = "Search blirp memory of past coding sessions: saved records (decisions, open threads, gotchas, notes, plans) and transcript messages of every earlier session. Use it before re-investigating something that may have been solved, decided or discussed before. Returns ranked hits with session id, date, agent and a snippet; open one with mem_session."
    )]
    async fn mem_search(
        &self,
        Parameters(a): Parameters<SearchArgs>,
        ext: Extensions,
    ) -> Result<String, String> {
        let project = self.scope(a.project.as_deref(), &ext, true)?;
        let kind = match a.kinds.as_deref() {
            None | Some([]) => None,
            Some(k) if k.len() > 1 => None,
            Some([k]) => Some(match k.trim_end_matches('s') {
                "record" => SearchHitKind::Record,
                "event" => SearchHitKind::Event,
                other => {
                    return Err(format!(
                        "unknown kind {other:?}; use \"record\" or \"event\""
                    ));
                }
            }),
            Some(_) => None,
        };
        let limit = i64::from(a.limit.unwrap_or(10).clamp(1, 50));
        let query = a.query.clone();
        let p2 = project.clone();
        let hits = self
            .read(move |s| s.search(&query, p2.as_deref(), kind, limit))
            .await?;
        if hits.is_empty() {
            return Ok(format!("No results for {:?}.", a.query));
        }
        let mut out = format!("{} results for {:?}:\n", hits.len(), a.query);
        for (i, h) in hits.iter().enumerate() {
            match h.kind {
                SearchHitKind::Record => out.push_str(&format!(
                    "{}. record {} \"{}\" ({}){}\n   {}\n",
                    i + 1,
                    h.record_id.as_deref().unwrap_or("?"),
                    one_line(h.title.as_deref().unwrap_or("")),
                    date(h.ts),
                    h.session_id
                        .as_deref()
                        .map(|s| format!(" from session {s}"))
                        .unwrap_or_default(),
                    snippet(&h.snippet)
                )),
                SearchHitKind::Event => out.push_str(&format!(
                    "{}. session {} seq {} · {} · \"{}\" ({})\n   {}\n",
                    i + 1,
                    h.session_id.as_deref().unwrap_or("?"),
                    h.seq.unwrap_or(0),
                    h.agent.as_deref().unwrap_or("?"),
                    one_line(h.title.as_deref().unwrap_or("untitled")),
                    date(h.ts),
                    snippet(&h.snippet)
                )),
            }
        }
        Ok(out)
    }

    #[tool(
        name = "mem_session",
        description = "Read one past session: its distilled summary and a page of its transcript events (user prompts, assistant replies, tool calls). Page with from_seq using the next_seq value in the reply."
    )]
    async fn mem_session(&self, Parameters(a): Parameters<SessionArgs>) -> Result<String, String> {
        let limit = i64::from(a.limit.unwrap_or(50).clamp(1, 200));
        let from = a.from_seq.unwrap_or(0).max(0);
        let sid = a.session_id.clone();
        let (session, (events, next)) = self
            .read(move |s| {
                let session = s
                    .get_session(&sid)?
                    .ok_or(StoreError::NotFound("session"))?;
                Ok((session, s.events_page(&sid, from, limit)?))
            })
            .await?;
        let mut out = format!(
            "Session {} · {} · {} · status {} · folder {}\nTitle: {}\n",
            session.id,
            session.agent,
            date(session.started_at),
            session.status,
            session.cwd,
            session.title.as_deref().unwrap_or("(untitled)")
        );
        if let Some(sum) = parse_summary(&session) {
            if let Some(text) = &sum.summary {
                out.push_str(&format!("Summary: {text}\n"));
            }
            if !sum.files.is_empty() {
                out.push_str(&format!("Files: {}\n", sum.files.join(", ")));
            }
        }
        out.push_str(&format!("\nEvents after seq {from}:\n"));
        if events.is_empty() {
            out.push_str("(none)\n");
        }
        for e in &events {
            out.push_str(&format!(
                "[{} {}] {}\n",
                e.seq,
                e.kind,
                clip(e.text.trim(), 4000)
            ));
        }
        if let Some(n) = next {
            out.push_str(&format!("\nMore events: call again with from_seq={n}.\n"));
        }
        Ok(out)
    }

    #[tool(
        name = "mem_recent",
        description = "List the most recent sessions of a project (newest first) with agent, date, status, title and distilled summary."
    )]
    async fn mem_recent(
        &self,
        Parameters(a): Parameters<ProjectArgs>,
        ext: Extensions,
    ) -> Result<String, String> {
        let project = self
            .scope(a.project.as_deref(), &ext, false)?
            .unwrap_or_default();
        let limit = i64::from(a.limit.unwrap_or(10).clamp(1, 50));
        let sessions = self
            .read(move |s| s.recent_sessions(&project, limit))
            .await?;
        if sessions.is_empty() {
            return Ok("No sessions yet in this project.".into());
        }
        let mut out = String::new();
        for s in &sessions {
            out.push_str(&format!(
                "- {} · {} · {} · {} · \"{}\"\n",
                s.id,
                date(s.started_at),
                s.agent,
                s.status,
                one_line(s.title.as_deref().unwrap_or("untitled"))
            ));
            if let Some(text) = parse_summary(s).and_then(|x| x.summary) {
                out.push_str(&format!("  {}\n", clip(&one_line(&text), 800)));
            }
        }
        Ok(out)
    }

    #[tool(
        name = "mem_brief",
        description = "Get the project brief (what the project is, how to build and run it, current priorities) and every active record: decisions, open threads, gotchas, notes and plans."
    )]
    async fn mem_brief(
        &self,
        Parameters(a): Parameters<BriefArgs>,
        ext: Extensions,
    ) -> Result<String, String> {
        let project = self
            .scope(a.project.as_deref(), &ext, false)?
            .unwrap_or_default();
        let (p, brief, records) = self
            .read(move |s| {
                let p = s.live_project(&project)?;
                let brief = s.get_brief(&project)?;
                let records = s.list_records(
                    &project,
                    &RecordFilter {
                        status: Some(RecordStatus::Active),
                        kind: None,
                    },
                )?;
                Ok((p, brief, records))
            })
            .await?;
        let mut out = format!("# {} (project {})\n\n", p.name, p.id);
        match brief {
            Some(b) => out.push_str(&format!(
                "{}\n(brief v{}, {})\n",
                b.body_md.trim(),
                b.version,
                date(b.updated_at)
            )),
            None => out.push_str("No brief yet.\n"),
        }
        for kind in RecordKind::ALL {
            let items: Vec<String> = records
                .iter()
                .filter(|r| r.kind == *kind)
                .map(record_block)
                .collect();
            if !items.is_empty() {
                out.push_str(&format!("\n## {kind}\n{}\n", items.join("\n")));
            }
        }
        Ok(out)
    }

    #[tool(
        name = "mem_record",
        description = "Save a record to the current project's memory so future sessions see it: a decision (with its reason), an open_thread (unfinished work), a gotcha (non-obvious pitfall), a plan or a note. Keep the title short and put details in body."
    )]
    async fn mem_record(
        &self,
        Parameters(a): Parameters<RecordArgs>,
        ext: Extensions,
    ) -> Result<String, String> {
        let kind: RecordKind = a.kind.parse().map_err(|_| {
            format!(
                "invalid kind {:?}; use decision, plan, note, open_thread or gotcha",
                a.kind
            )
        })?;
        let title = one_line(&a.title);
        if title.is_empty() {
            return Err("title must not be empty".into());
        }
        if title.chars().count() > 300 || a.body.len() > 64 * 1024 {
            return Err("title must be at most 300 characters and body at most 64 KiB".into());
        }
        let project = self.scope(None, &ext, false)?.unwrap_or_default();
        let body = CreateRecord {
            kind,
            title,
            body: a.body,
            status: None,
            pinned: None,
        };
        let rec = match &self.writer {
            Writer::Direct(state) => {
                let st = state.clone();
                let pid = project.clone();
                let r = tokio::task::spawn_blocking(move || {
                    st.store.live_project(&pid)?;
                    let now = blirp_core::now_ms();
                    st.store.create_record(Record {
                        id: blirp_core::new_id(),
                        project_id: pid,
                        kind: body.kind,
                        title: body.title,
                        body: body.body,
                        status: RecordStatus::Active,
                        pinned: false,
                        source_session_id: None,
                        created_at: now,
                        updated_at: now,
                        updated_by: "user".into(),
                    })
                })
                .await
                .map_err(|e| format!("internal error: {e}"))?
                .map_err(|e| e.to_string())?;
                state.emit(ServerEvent::MemoryUpdated {
                    project_id: project,
                    part: MemoryPart::Records,
                });
                r
            }
            Writer::Daemon(paths) => {
                let info = RuntimeInfo::read(paths).ok().flatten().ok_or(
                    "the blirp daemon is not running; start it with `blirp daemon --detach`",
                )?;
                let resp = reqwest::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .map_err(|e| e.to_string())?
                    .post(format!(
                        "{}/api/projects/{project}/records",
                        info.base_url()
                    ))
                    .bearer_auth(&info.token)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| format!("cannot reach the blirp daemon: {e}"))?;
                if !resp.status().is_success() {
                    return Err(format!(
                        "the blirp daemon rejected the record: {} {}",
                        resp.status(),
                        resp.text().await.unwrap_or_default()
                    ));
                }
                resp.json::<Record>().await.map_err(|e| e.to_string())?
            }
        };
        Ok(format!(
            "Saved {} \"{}\" (id {}).",
            rec.kind, rec.title, rec.id
        ))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BlirpMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("blirp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// `blirp mcp`: serve over stdio until the client disconnects.
pub async fn serve_stdio(paths: Paths) -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    let server = BlirpMcp::for_stdio(
        paths,
        std::env::var("BLIRP_PROJECT_ID").ok(),
        std::env::current_dir().ok().as_deref(),
    )?;
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// The daemon's `/mcp` Streamable HTTP service.
pub fn http_service(
    state: SharedState,
    cancel: tokio_util::sync::CancellationToken,
) -> rmcp::transport::streamable_http_server::StreamableHttpService<
    BlirpMcp,
    rmcp::transport::streamable_http_server::session::local::LocalSessionManager,
> {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    StreamableHttpService::new(
        move || Ok(BlirpMcp::for_daemon(state.clone())),
        Arc::default(),
        StreamableHttpServerConfig::default().with_cancellation_token(cancel),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::memory::testutil::*;
    use blirp_core::model::EventKind;
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;

    async fn call(
        client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        tool: &str,
        args: serde_json::Value,
    ) -> (bool, String) {
        let res = client
            .call_tool(
                CallToolRequestParams::new(tool.to_string())
                    .with_arguments(args.as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        let text = serde_json::to_value(&res).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let is_error = serde_json::to_value(&res).unwrap()["isError"]
            .as_bool()
            .unwrap_or(false);
        (is_error, text)
    }

    #[tokio::test]
    async fn tools_over_an_in_process_transport() {
        let (d, store, pid) = project_store("Demo");
        store.put_brief(&pid, "Demo brief", "user").unwrap();
        record(
            &store,
            &pid,
            RecordKind::Gotcha,
            "Parser quirk",
            "needs a trailing newline",
            5,
            false,
        );
        let mut s = session("s1", &pid, 1_758_758_400_000);
        s.title = Some("Parser work".into());
        s.summary = Some(serde_json::json!({"summary": "Refactored the parser."}));
        store.insert_session(&s).unwrap();
        event(
            &store,
            "s1",
            1,
            EventKind::User,
            "refactor the parser module",
        );
        event(&store, "s1", 2, EventKind::Assistant, "parser refactored");
        drop(store);

        let paths = Paths::at(d.path());
        let server = BlirpMcp::for_stdio(paths, None, Some(&d.path().join("Demo"))).unwrap();
        assert_eq!(server.project.as_deref(), Some(pid.as_str()));
        let (st, ct) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let running = server.serve(st).await.unwrap();
            let _ = running.waiting().await;
        });
        let client = ().serve(ct).await.unwrap();
        let tools = client.list_tools(None).await.unwrap();
        let mut names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "mem_brief",
                "mem_recent",
                "mem_record",
                "mem_search",
                "mem_session"
            ]
        );
        assert!(
            tools
                .tools
                .iter()
                .all(|t| t.description.as_ref().is_some_and(|d| d.len() > 40))
        );

        let (err, text) = call(
            &client,
            "mem_search",
            serde_json::json!({"query": "parser"}),
        )
        .await;
        assert!(!err, "{text}");
        assert!(
            text.contains("record r-Parser quirk") && text.contains("session s1 seq"),
            "{text}"
        );
        assert!(text.contains("**"), "{text}");
        let (_, text) = call(
            &client,
            "mem_search",
            serde_json::json!({"query": "parser", "kinds": ["record"]}),
        )
        .await;
        assert!(!text.contains("session s1 seq"), "{text}");

        let (_, text) = call(
            &client,
            "mem_session",
            serde_json::json!({"session_id": "s1", "limit": 1}),
        )
        .await;
        assert!(text.contains("Summary: Refactored the parser."), "{text}");
        assert!(text.contains("[1 user] refactor the parser module"));
        assert!(text.contains("from_seq=1"), "{text}");

        let (_, text) = call(&client, "mem_recent", serde_json::json!({})).await;
        assert!(
            text.contains("\"Parser work\"") && text.contains("Refactored the parser."),
            "{text}"
        );

        let (_, text) = call(&client, "mem_brief", serde_json::json!({})).await;
        assert!(
            text.contains("Demo brief") && text.contains("## gotcha"),
            "{text}"
        );

        // Writes need the daemon; without one the tool reports it clearly.
        let (err, text) = call(
            &client,
            "mem_record",
            serde_json::json!({"kind": "note", "title": "t", "body": "b"}),
        )
        .await;
        assert!(err && text.contains("daemon is not running"), "{text}");
        let (err, text) = call(
            &client,
            "mem_record",
            serde_json::json!({"kind": "bogus", "title": "t", "body": "b"}),
        )
        .await;
        assert!(err && text.contains("invalid kind"), "{text}");
        client.cancel().await.unwrap();
    }
}
