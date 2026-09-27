//! Agent hooks (§4, §7, §9): the short-lived `blirp hook <agent> <event>`
//! client and the daemon side of `POST /api/hooks/:agent/:event`.

pub mod install;

use crate::api::{ApiError, ApiResult};
use crate::memory::distill::DISTILLING_ENV;
use crate::memory::launch::{MEMORY_FILE, inject_mode};
use crate::memory::render::render_injection;
use crate::state::SharedState;
use blirp_core::config::MemoryConfig;
use blirp_core::model::{
    BUILTIN_AGENTS, InjectMode, ServerEvent, Session, SessionOrigin, SessionStatus,
};
use blirp_core::paths::{Paths, RuntimeInfo};
use blirp_core::store::{NonProjectDirs, Store};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Hard budget for the whole hook process (§1.6: exit 0 within 2 s).
pub const HOOK_BUDGET: Duration = Duration::from_millis(1800);
/// Cap on the daemon round trip.
const HTTP_BUDGET: Duration = Duration::from_millis(1500);
/// Left for the offline fallback and printing after the round trip.
const FALLBACK_RESERVE: Duration = Duration::from_millis(300);
const STDIN_WAIT: Duration = Duration::from_millis(400);

/// Body of `POST /api/hooks/:agent/:event`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookIngress {
    /// `BLIRP_SESSION_ID` of the hook process (set for blirp-launched sessions).
    #[serde(default)]
    pub blirp_session_id: Option<String>,
    /// Working directory of the hook process.
    #[serde(default)]
    pub cwd: Option<String>,
    /// The entry comes from the opt-in global install, not a per-launch config.
    #[serde(default)]
    pub global: bool,
    /// The agent's hook JSON (stdin), passed through.
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HookReply {
    pub session_id: Option<String>,
    /// Context to inject (SessionStart only).
    pub additional_context: Option<String>,
}

/// Agent-neutral hook events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    PromptSubmit,
    Stop,
    Notification,
    SessionEnd,
    PreCompact,
    Other,
}

impl HookEvent {
    /// Claude/Codex (`UserPromptSubmit`), Gemini (`BeforeAgent`, `AfterAgent`,
    /// `PreCompress`) and Cursor (`sessionStart`, `beforeSubmitPrompt`, `stop`) names.
    pub fn parse(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "sessionstart" => Self::SessionStart,
            "userpromptsubmit" | "beforeagent" | "beforesubmitprompt" => Self::PromptSubmit,
            "stop" | "afteragent" => Self::Stop,
            "notification" => Self::Notification,
            "sessionend" => Self::SessionEnd,
            "precompact" | "precompress" => Self::PreCompact,
            _ => Self::Other,
        }
    }
}

fn valid_agent(agent: &str) -> bool {
    BUILTIN_AGENTS.contains(&agent)
        || agent
            .strip_prefix("custom:")
            .is_some_and(|n| !n.is_empty() && n.len() <= 40)
}

fn str_field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
        .filter(|s| !s.trim().is_empty())
}

/// The agent's own session id in a hook payload.
fn agent_session_id(p: &Value) -> Option<String> {
    str_field(p, &["session_id", "conversation_id", "sessionId"]).map(str::to_string)
}

fn payload_cwd(p: &Value) -> Option<String> {
    str_field(p, &["cwd"]).map(str::to_string).or_else(|| {
        p.get("workspace_roots")
            .and_then(|r| r.get(0))
            .and_then(|r| r.as_str())
            .map(str::to_string)
    })
}

/// Memory for a SessionStart: the launch file (which may carry a handoff pack)
/// on first start of a blirp session, else a fresh render. `None` when
/// injection is off for the agent (§12) and there is no launch file.
fn session_start_context(
    store: &Store,
    paths: &Paths,
    session: &Session,
    source: Option<&str>,
    memory: &MemoryConfig,
) -> ApiResult<Option<String>> {
    if session.origin == SessionOrigin::Blirp
        && matches!(source, None | Some("startup"))
        && let Ok(dir) = paths.launch_dir(&session.id)
        && let Ok(text) = std::fs::read_to_string(dir.join(MEMORY_FILE))
    {
        return Ok(Some(text));
    }
    if !memory.inject_enabled(&session.agent) {
        return Ok(None);
    }
    Ok(Some(render_injection(
        store,
        &session.project_id,
        Some(&session.id),
        memory.inject_max_chars as usize,
    )?))
}

/// Daemon side of a hook (blocking; runs on the blocking pool).
pub fn handle(
    state: &SharedState,
    agent: &str,
    event_name: &str,
    req: HookIngress,
) -> ApiResult<HookReply> {
    if !valid_agent(agent) {
        return Err(ApiError::bad_request(format!("unknown agent {agent:?}")));
    }
    let store = &state.store;
    let event = HookEvent::parse(event_name);
    let p = &req.payload;
    let asid = agent_session_id(p);
    let now = blirp_core::now_ms();

    let mut found = match &req.blirp_session_id {
        Some(id) => store.get_session(id)?,
        None => None,
    };
    if found.is_none()
        && let Some(asid) = &asid
    {
        found = store.session_by_agent_id(agent, asid)?;
    }
    // A replicated session of another machine (same agent id, e.g. a synced
    // agent config dir): its origin machine owns status, memory and distill.
    if found
        .as_ref()
        .is_some_and(|s| s.machine_id != state.machine.id)
    {
        return Ok(HookReply::default());
    }
    let mut created = false;
    let session = match found {
        Some(s) => s,
        None => {
            let Some(asid) = asid.clone() else {
                return Ok(HookReply::default());
            };
            let Some(cwd) = payload_cwd(p).or(req.cwd.clone()) else {
                return Ok(HookReply::default());
            };
            let resolved =
                // An external session: the auto rules apply as for ingest (§5).
                store.resolve_project_with(
                    &state.machine.id,
                    &state.machine.name,
                    Path::new(&cwd),
                    &NonProjectDirs::from_process().with_workspaces(&state.paths.workspaces_dir()),
                )?;
            if resolved.created {
                state.emit(ServerEvent::ProjectUpdated {
                    project_id: resolved.project.id.clone(),
                });
            }
            let s = Session {
                id: blirp_core::new_id(),
                project_id: resolved.project.id,
                machine_id: state.machine.id.clone(),
                agent: agent.to_string(),
                agent_session_id: Some(asid),
                origin: SessionOrigin::External,
                cwd: dunce::canonicalize(&cwd)
                    .map(|c| c.display().to_string())
                    .unwrap_or(cwd),
                title: None,
                status: SessionStatus::Idle,
                branch: blirp_core::git::current_branch(Path::new(&resolved.root)),
                worktree: None,
                transcript_path: str_field(p, &["transcript_path"]).map(str::to_string),
                started_at: now,
                ended_at: None,
                last_activity_at: now,
                exit_code: None,
                summary: None,
                distilled_through_seq: 0,
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                parent_session_id: None,
                stopped_by_user: false,
            };
            // Ingest may have created it since the lookup: use that row.
            let (s, inserted) = store.insert_session_unless_known(&s)?;
            if s.machine_id != state.machine.id {
                return Ok(HookReply::default());
            }
            created = inserted;
            s
        }
    };

    let launched = session.origin == SessionOrigin::Blirp;
    let mode = inject_mode(agent);
    // A global entry firing inside a blirp launch that already has per-launch
    // hooks would double every update and injection.
    if req.global && launched && mode == InjectMode::Hook {
        return Ok(HookReply {
            session_id: Some(session.id),
            additional_context: None,
        });
    }
    let live = state.terminals.get(&session.id).is_some();
    let other_owner = asid.as_ref().and_then(|a| {
        store
            .session_by_agent_id(agent, a)
            .ok()
            .flatten()
            .filter(|o| o.id != session.id)
    });
    let transcript = str_field(p, &["transcript_path"]).map(str::to_string);
    let reason = str_field(p, &["reason"]);
    let updated = store.modify_session(&session.id, |s| {
        if s.agent_session_id.is_none() && other_owner.is_none() {
            s.agent_session_id = asid.clone();
        }
        if transcript.is_some() && s.transcript_path != transcript {
            s.transcript_path = transcript.clone();
        }
        let status = match event {
            HookEvent::PromptSubmit => Some(SessionStatus::Working),
            HookEvent::Stop => Some(SessionStatus::Idle),
            HookEvent::Notification => Some(SessionStatus::Waiting),
            HookEvent::SessionStart
                if !s.status.is_live() || s.status == SessionStatus::Starting =>
            {
                // Launched sessions end through their process; a resumed external one is live again.
                (!launched || live).then_some(SessionStatus::Idle)
            }
            // `/clear` ends and restarts within the same process; a PTY exit owns the final status.
            HookEvent::SessionEnd if reason != Some("clear") && !live => {
                Some(SessionStatus::Completed)
            }
            _ => None,
        };
        if let Some(st) = status {
            if st == SessionStatus::Completed {
                if s.status.is_live() {
                    s.status = st;
                    s.ended_at = Some(now);
                }
            } else if s.status.is_live() || !launched {
                if !s.status.is_live() {
                    s.ended_at = None;
                }
                s.status = st;
            }
        }
        s.last_activity_at = now;
    })?;
    state.emit(if created {
        ServerEvent::SessionCreated {
            session: updated.clone(),
        }
    } else {
        ServerEvent::SessionUpdated {
            session: updated.clone(),
        }
    });

    if let Some(t) = &updated.transcript_path {
        state
            .ingest_trigger()
            .transcript_hint(agent, &updated.id, Path::new(t));
    }
    if event == HookEvent::SessionEnd && reason != Some("clear") {
        state.distiller.enqueue_ended(&updated.id);
    }
    let additional_context = if event == HookEvent::SessionStart
        && !(req.global && launched && mode == InjectMode::Instructions)
    {
        session_start_context(
            store,
            &state.paths,
            &updated,
            str_field(p, &["source"]),
            &state.config().memory,
        )?
    } else {
        None
    };
    Ok(HookReply {
        session_id: Some(updated.id),
        additional_context,
    })
}

// ---------------------------------------------------------------- client

/// What the agent expects on stdout for `event`, given the context to inject.
pub fn format_output(agent: &str, event_name: &str, context: Option<&str>) -> Option<String> {
    let event = HookEvent::parse(event_name);
    let ctx = context.filter(|c| !c.trim().is_empty());
    match (agent, event, ctx) {
        ("cursor", HookEvent::SessionStart, Some(c)) => {
            Some(serde_json::json!({ "additional_context": c }).to_string())
        }
        ("cursor", HookEvent::PromptSubmit, _) => Some(r#"{"continue":true}"#.into()),
        ("cursor", _, _) => Some("{}".into()),
        (_, HookEvent::SessionStart, Some(c)) => Some(
            serde_json::json!({
                "hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": c}
            })
            .to_string(),
        ),
        // Gemini parses stdout as JSON for every event.
        ("gemini", _, _) => Some("{}".into()),
        _ => None,
    }
}

/// Inputs of one hook invocation, injectable for tests.
pub struct HookEnv<'a> {
    /// When the hook process started: the watchdog's clock.
    pub started: Instant,
    pub agent: &'a str,
    pub event: &'a str,
    pub global: bool,
    pub stdin: &'a str,
    pub cwd: Option<PathBuf>,
    pub var: &'a dyn Fn(&str) -> Option<String>,
}

/// SessionStart memory without the daemon: the launch file of a blirp session,
/// else a read-only render for the folder.
fn offline_context(env: &HookEnv<'_>, payload: &Value) -> Option<String> {
    let paths = match (env.var)(blirp_core::paths::HOME_ENV).filter(|v| !v.is_empty()) {
        Some(h) => Paths::at(h),
        None => Paths::resolve().ok()?,
    };
    if let Some(sid) = (env.var)("BLIRP_SESSION_ID").filter(|v| !v.is_empty())
        && let Ok(dir) = paths.launch_dir(&sid)
        && let Ok(text) = std::fs::read_to_string(dir.join(MEMORY_FILE))
    {
        return Some(text);
    }
    let cwd = payload_cwd(payload)
        .map(PathBuf::from)
        .or_else(|| env.cwd.clone())?;
    let store = Store::open_read_only(&paths.db_file()).ok()?;
    let project = match (env.var)("BLIRP_PROJECT_ID").filter(|v| !v.is_empty()) {
        Some(p) => p,
        None => {
            let machine = store.machine_id().ok()??;
            store
                .find_project_for_path(&machine, &cwd, Some(&paths.workspaces_dir()))
                .ok()??
                .id
        }
    };
    // Read-only: never create config.toml from a hook.
    let memory = std::fs::read_to_string(paths.config_file())
        .ok()
        .and_then(|t| blirp_core::config::Config::parse(&t, &paths.config_file()).ok())
        .unwrap_or_default()
        .memory;
    if !memory.inject_enabled(env.agent) {
        return None;
    }
    render_injection(&store, &project, None, memory.inject_max_chars as usize).ok()
}

async fn post(
    info: &RuntimeInfo,
    agent: &str,
    event: &str,
    body: &HookIngress,
    timeout: Duration,
) -> Result<HookReply, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(format!("{}/api/hooks/{agent}/{event}", info.base_url()))
        .bearer_auth(&info.token)
        .json(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("daemon answered {}", resp.status()));
    }
    resp.json().await.map_err(|e| e.to_string())
}

/// Run one hook: forward to the daemon, fall back offline for SessionStart,
/// and return what to print. Never fails; bounded by `HOOK_BUDGET`.
pub fn run(env: &HookEnv<'_>) -> Option<String> {
    // reqwest panics building a client without a rustls provider (the sync
    // stack enables `rustls-no-provider`); a hook must never panic.
    crate::install_crypto_provider();
    if (env.var)(DISTILLING_ENV).as_deref() == Some("1") {
        return None;
    }
    let payload: Value = serde_json::from_str(env.stdin).unwrap_or(Value::Null);
    let body = HookIngress {
        blirp_session_id: (env.var)("BLIRP_SESSION_ID").filter(|v| !v.is_empty()),
        cwd: env.cwd.as_ref().map(|c| c.display().to_string()),
        global: env.global,
        payload: payload.clone(),
    };
    let paths = match (env.var)(blirp_core::paths::HOME_ENV).filter(|v| !v.is_empty()) {
        Some(h) => Some(Paths::at(h)),
        None => Paths::resolve().ok(),
    };
    let info = paths.and_then(|p| RuntimeInfo::read(&p).ok().flatten());
    // The watchdog counts from process start (stdin reading included); the
    // round trip ends early enough for the fallback to still print.
    let left = HOOK_BUDGET
        .saturating_sub(env.started.elapsed())
        .saturating_sub(FALLBACK_RESERVE)
        .min(HTTP_BUDGET);
    let remote = info.filter(|_| !left.is_zero()).and_then(|info| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        rt.block_on(post(&info, env.agent, env.event, &body, left))
            .ok()
    });
    let context = match remote {
        Some(r) => r.additional_context,
        None if HookEvent::parse(env.event) == HookEvent::SessionStart => {
            offline_context(env, &payload)
        }
        None => None,
    };
    format_output(env.agent, env.event, context.as_deref())
}

/// Read stdin for at most `STDIN_WAIT` (agents close it right away, but a
/// hook must never hang on an open pipe).
fn read_stdin() -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        // A broken or non-UTF-8 stdin just means no payload.
        let _ = std::io::stdin()
            .take(8 * 1024 * 1024)
            .read_to_string(&mut s);
        let _ = tx.send(s);
    });
    rx.recv_timeout(STDIN_WAIT).unwrap_or_default()
}

/// `blirp hook <agent> <event> [--global]` entry point. Always exits 0.
pub fn main(agent: &str, event: &str, global: bool) {
    let started = Instant::now();
    if std::env::var(DISTILLING_ENV).as_deref() == Ok("1") {
        return;
    }
    // Watchdog: whatever happens, the process is gone before the 2 s budget.
    std::thread::spawn(|| {
        std::thread::sleep(HOOK_BUDGET);
        std::process::exit(0);
    });
    let stdin = read_stdin();
    let var = |k: &str| std::env::var(k).ok();
    let out = run(&HookEnv {
        started,
        agent,
        event,
        global,
        stdin: &stdin,
        cwd: std::env::current_dir().ok(),
        var: &var,
    });
    if let Some(out) = out {
        println!("{out}");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn output_shapes_per_agent() {
        let claude = format_output("claude", "SessionStart", Some("MEM")).unwrap();
        let v: Value = serde_json::from_str(&claude).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "MEM");
        assert_eq!(format_output("claude", "Stop", None), None);
        assert_eq!(format_output("claude", "SessionStart", Some("  ")), None);
        let codex: Value =
            serde_json::from_str(&format_output("codex", "SessionStart", Some("M")).unwrap())
                .unwrap();
        assert_eq!(codex["hookSpecificOutput"]["additionalContext"], "M");
        assert_eq!(
            format_output("gemini", "AfterAgent", None).as_deref(),
            Some("{}")
        );
        let cursor: Value =
            serde_json::from_str(&format_output("cursor", "sessionStart", Some("M")).unwrap())
                .unwrap();
        assert_eq!(cursor["additional_context"], "M");
        assert_eq!(
            format_output("cursor", "beforeSubmitPrompt", None).as_deref(),
            Some(r#"{"continue":true}"#)
        );
    }

    #[test]
    fn event_names() {
        assert_eq!(HookEvent::parse("BeforeAgent"), HookEvent::PromptSubmit);
        assert_eq!(HookEvent::parse("sessionEnd"), HookEvent::SessionEnd);
        assert_eq!(HookEvent::parse("PreCompress"), HookEvent::PreCompact);
        assert_eq!(HookEvent::parse("PostToolUse"), HookEvent::Other);
    }

    fn env_fn(vars: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        move |k| vars.iter().find(|(a, _)| *a == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn distilling_short_circuits() {
        let var = env_fn(vec![(DISTILLING_ENV, "1".into())]);
        let begin = Instant::now();
        let out = run(&HookEnv {
            started: Instant::now(),
            agent: "claude",
            event: "SessionStart",
            global: false,
            stdin: "{}",
            cwd: None,
            var: &var,
        });
        assert_eq!(out, None);
        assert!(begin.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn daemon_down_falls_back_to_launch_file() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::at(home.path());
        std::fs::create_dir_all(paths.launch_dir("S1").unwrap()).unwrap();
        std::fs::write(
            paths.launch_dir("S1").unwrap().join(MEMORY_FILE),
            "# blirp memory: X",
        )
        .unwrap();
        let var = env_fn(vec![
            ("BLIRP_HOME", home.path().display().to_string()),
            ("BLIRP_SESSION_ID", "S1".into()),
        ]);
        let out = run(&HookEnv {
            started: Instant::now(),
            agent: "claude",
            event: "SessionStart",
            global: false,
            stdin: "{}",
            cwd: None,
            var: &var,
        })
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"],
            "# blirp memory: X"
        );
        // Non-start events print nothing when the daemon is down.
        assert_eq!(
            run(&HookEnv {
                started: Instant::now(),
                agent: "claude",
                event: "Stop",
                global: false,
                stdin: "{}",
                cwd: None,
                var: &var
            }),
            None
        );
    }

    #[test]
    fn daemon_down_renders_from_db_read_only() {
        let (d, store, pid) = crate::memory::testutil::project_store("Proj");
        store.put_brief(&pid, "offline brief", "user").unwrap();
        drop(store);
        let folder = dunce::canonicalize(d.path().join("Proj")).unwrap();
        let var = env_fn(vec![("BLIRP_HOME", d.path().display().to_string())]);
        let stdin =
            serde_json::json!({"session_id": "x", "cwd": folder.join("sub").display().to_string()})
                .to_string();
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        let out = run(&HookEnv {
            started: Instant::now(),
            agent: "claude",
            event: "SessionStart",
            global: true,
            stdin: &stdin,
            cwd: None,
            var: &var,
        })
        .unwrap();
        assert!(out.contains("offline brief"), "{out}");
    }

    #[test]
    fn unresponsive_daemon_respects_budget() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::at(home.path());
        // Accepts connections and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for c in listener.incoming() {
                held.push(c);
            }
        });
        RuntimeInfo {
            pid: 1,
            port,
            token: "t".into(),
            version: "0".into(),
            started_at: 0,
        }
        .write(&paths)
        .unwrap();
        std::fs::create_dir_all(paths.launch_dir("S").unwrap()).unwrap();
        std::fs::write(paths.launch_dir("S").unwrap().join(MEMORY_FILE), "M").unwrap();
        let var = env_fn(vec![
            ("BLIRP_HOME", home.path().display().to_string()),
            ("BLIRP_SESSION_ID", "S".into()),
        ]);
        // The process already spent the stdin wait before `run`.
        let started = Instant::now() - STDIN_WAIT;
        let out = run(&HookEnv {
            started,
            agent: "claude",
            event: "SessionStart",
            global: false,
            stdin: "{}",
            cwd: None,
            var: &var,
        });
        let took = started.elapsed();
        assert!(took < HOOK_BUDGET - Duration::from_millis(100), "{took:?}");
        assert!(out.unwrap().contains("\"M\""));
    }
}
