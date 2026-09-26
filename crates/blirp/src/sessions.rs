//! Session lifecycle (§7): launch, resume, stop, status heuristics, exit.

use crate::agents::{Agent, AgentError, LaunchContext};
use crate::api::{ApiError, ApiResult};
use crate::pty::{ExitInfo, SpawnRequest, Terminal};
use crate::state::SharedState;
use axum::http::StatusCode;
use blirp_core::model::{LaunchSession, ServerEvent, Session, SessionOrigin, SessionStatus};
use blirp_core::{git, now_ms};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 32;
/// Initial prompt is typed once output has been quiet this long (§7 step 5).
const PROMPT_SETTLE: Duration = Duration::from_millis(1500);
/// Give up waiting for quiet output and type the prompt anyway.
const PROMPT_MAX_WAIT: Duration = Duration::from_secs(60);
const MAX_PROMPT: usize = 64 * 1024;

/// Env vars of a parent agent session that must not leak into sessions
/// blirp starts (they make nested CLIs think they run inside another agent).
const ENV_REMOVE: &[&str] = &["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CODEX_SANDBOX"];

fn agent_error(e: AgentError) -> ApiError {
    match e {
        AgentError::Unknown(_) => {
            ApiError::new(StatusCode::BAD_REQUEST, "unknown_agent", e.to_string())
        }
        AgentError::NotInstalled(_) => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "agent_not_installed",
            e.to_string(),
        ),
    }
}

const ADJECTIVES: &[&str] = &[
    "amber", "brave", "calm", "clever", "cosmic", "crisp", "dusty", "eager", "fuzzy", "gentle",
    "golden", "happy", "icy", "jolly", "keen", "lucky", "mellow", "misty", "nimble", "proud",
    "quiet", "rapid", "rusty", "shiny", "silent", "snowy", "sunny", "swift", "tidy", "vivid",
    "witty", "zesty",
];
const ANIMALS: &[&str] = &[
    "badger", "beaver", "bison", "crane", "falcon", "ferret", "gecko", "heron", "ibis", "koala",
    "lemur", "lynx", "marmot", "moose", "newt", "ocelot", "otter", "panda", "puffin", "quokka",
    "raven", "salmon", "seal", "stoat", "tapir", "toucan", "walrus", "weasel", "wombat", "yak",
    "zebra", "finch",
];

/// `adjective-animal-xxxx` with 16 random bits.
pub fn worktree_name() -> ApiResult<String> {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).map_err(|e| ApiError::internal("random name", e))?;
    Ok(format!(
        "{}-{}-{:02x}{:02x}",
        ADJECTIVES[usize::from(b[0]) % ADJECTIVES.len()],
        ANIMALS[usize::from(b[1]) % ANIMALS.len()],
        b[2],
        b[3]
    ))
}

struct Prepared {
    project_id: String,
    cwd: PathBuf,
    branch: Option<String>,
    worktree: Option<String>,
}

/// §7 step 1: resolve project + cwd, optionally create a worktree.
fn prepare(state: &SharedState, req: &LaunchSession) -> ApiResult<Prepared> {
    let store = &state.store;
    let (project_id, cwd) = match (&req.project_id, &req.cwd) {
        // Explicit project: the folder must be inside one of its folders here
        // (no resolution, so a mismatch never registers a stray project).
        (Some(pid), Some(cwd)) => {
            store.live_project(pid)?;
            let cwd = dunce::canonicalize(cwd)
                .map_err(|e| ApiError::bad_request(format!("cwd is not accessible: {e}")))?;
            let inside = store
                .local_roots(pid, &state.machine.id)?
                .iter()
                .any(|root| cwd.starts_with(root));
            if !inside {
                return Err(ApiError::bad_request(
                    "cwd is not inside a folder of the given project on this machine",
                ));
            }
            (pid.clone(), cwd)
        }
        (None, Some(cwd)) => {
            let resolved =
                store.resolve_project(&state.machine.id, &state.machine.name, Path::new(cwd))?;
            if resolved.created {
                state.emit(ServerEvent::ProjectUpdated {
                    project_id: resolved.project.id.clone(),
                });
            }
            let cwd = dunce::canonicalize(cwd)
                .map_err(|e| ApiError::bad_request(format!("cwd is not accessible: {e}")))?;
            (resolved.project.id, cwd)
        }
        (Some(pid), None) => {
            store.live_project(pid)?;
            let root = store
                .local_roots(pid, &state.machine.id)?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    ApiError::bad_request("project has no folder on this machine; pass cwd")
                })?;
            (pid.clone(), root)
        }
        (None, None) => return Err(ApiError::bad_request("project_id or cwd is required")),
    };
    if !cwd.is_dir() {
        return Err(ApiError::bad_request(format!(
            "{} is not a folder",
            cwd.display()
        )));
    }

    let explicit = req.worktree;
    let want_worktree = explicit.unwrap_or(state.config().sessions.worktree_default);
    if !want_worktree {
        return Ok(Prepared {
            branch: git::current_branch(&cwd),
            project_id,
            cwd,
            worktree: None,
        });
    }
    let repo = git::repo_info(&cwd).map_err(|e| ApiError::internal("inspecting git repo", e))?;
    let Some(repo) = repo else {
        if explicit == Some(true) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "not_git",
                "worktrees need a git project",
            ));
        }
        // Config default only applies where it can.
        return Ok(Prepared {
            branch: None,
            project_id,
            cwd,
            worktree: None,
        });
    };
    let name = worktree_name()?;
    let wt = state.paths.worktrees_dir().join(&project_id).join(&name);
    std::fs::create_dir_all(wt.parent().unwrap_or(&wt))
        .map_err(|e| ApiError::internal("creating worktree dir", e))?;
    let branch = format!("blirp/{name}");
    git::worktree_add(&repo.toplevel, &wt, &branch).map_err(|e| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "worktree_failed",
            format!("git worktree add failed: {e}"),
        )
    })?;
    // Keep the same subfolder inside the new worktree.
    let sub = cwd.strip_prefix(&repo.toplevel).unwrap_or(Path::new(""));
    let session_cwd = wt.join(sub);
    Ok(Prepared {
        project_id,
        cwd: if session_cwd.is_dir() {
            session_cwd
        } else {
            wt.clone()
        },
        branch: Some(branch),
        worktree: Some(wt.display().to_string()),
    })
}

/// POST /api/sessions
pub async fn launch(state: &SharedState, req: LaunchSession) -> ApiResult<Session> {
    if let Some(m) = &req.machine
        && *m != state.machine.id
    {
        // Remote launches are forwarded by the API layer (crate::sync).
        return Err(ApiError::bad_request(
            "machine is not this machine; launch through the API to reach it",
        ));
    }
    if req.continue_from.is_some() {
        return Err(ApiError::not_implemented(
            "continue_from (handoff) is not available in this version",
        ));
    }
    if req.prompt.as_ref().is_some_and(|p| p.len() > MAX_PROMPT) {
        return Err(ApiError::bad_request("prompt exceeds 64 KiB"));
    }
    let agent = Agent::resolve(&req.agent, &state.config()).map_err(agent_error)?;
    if agent.path.is_none() {
        return Err(agent_error(AgentError::NotInstalled(agent.display_name)));
    }
    let st = state.clone();
    let req2 = req.clone();
    let prepared = crate::api::blocking(move || prepare(&st, &req2)).await?;

    let now = now_ms();
    let session = Session {
        id: blirp_core::new_id(),
        project_id: prepared.project_id,
        machine_id: state.machine.id.clone(),
        agent: agent.id.clone(),
        agent_session_id: agent
            .assigns_session_id()
            .then(|| uuid::Uuid::new_v4().to_string()),
        origin: SessionOrigin::Blirp,
        cwd: prepared.cwd.display().to_string(),
        title: None,
        status: SessionStatus::Starting,
        branch: prepared.branch,
        worktree: prepared.worktree,
        transcript_path: None,
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
    };
    let store = state.store.clone();
    let row = session.clone();
    crate::api::blocking(move || Ok(store.insert_session(&row)?)).await?;
    state.emit(ServerEvent::SessionCreated {
        session: session.clone(),
    });
    start(
        state, session, &agent, false, req.cols, req.rows, req.prompt,
    )
    .await
}

/// POST /api/sessions/:id/resume
pub async fn resume(state: &SharedState, id: &str) -> ApiResult<Session> {
    let store = state.store.clone();
    let sid = id.to_string();
    let session = crate::api::blocking(move || {
        store
            .get_session(&sid)?
            .ok_or_else(|| ApiError::not_found("session"))
    })
    .await?;
    if session.machine_id != state.machine.id {
        // Forwarded by the API layer when the machine is reachable.
        return Err(ApiError::conflict(
            "machine_unreachable",
            "this session runs on another machine that is not reachable",
        ));
    }
    if state.terminals.get(id).is_some() {
        return Err(ApiError::conflict(
            "already_running",
            "session is still running",
        ));
    }
    if !Path::new(&session.cwd).is_dir() {
        return Err(ApiError::bad_request(format!(
            "session folder {} no longer exists",
            session.cwd
        )));
    }
    let agent = Agent::resolve(&session.agent, &state.config()).map_err(agent_error)?;
    start(state, session, &agent, true, None, None, None).await
}

/// §7 steps 4-5: build argv/env, spawn the PTY, schedule the prompt.
async fn start(
    state: &SharedState,
    session: Session,
    agent: &Agent,
    resume: bool,
    cols: Option<u16>,
    rows: Option<u16>,
    prompt: Option<String>,
) -> ApiResult<Session> {
    // Resumed rows go back to `starting` before the process exists, so a
    // fast exit can never be overwritten by this update.
    let session = if resume {
        let store = state.store.clone();
        let id = session.id.clone();
        let s = crate::api::blocking(move || {
            Ok(store.modify_session(&id, |s| {
                s.status = SessionStatus::Starting;
                s.ended_at = None;
                s.exit_code = None;
                s.last_activity_at = now_ms();
            })?)
        })
        .await?;
        state.emit(ServerEvent::SessionUpdated { session: s.clone() });
        s
    } else {
        session
    };
    let launch_dir = state.paths.launch_dir(&session.id);
    let (program, args) = agent
        .command(&LaunchContext {
            agent_session_id: session.agent_session_id.as_deref(),
            resume,
            launch_dir: &launch_dir,
        })
        .map_err(agent_error)?;
    let home = state.paths.home().display().to_string();
    let env = vec![
        ("BLIRP_SESSION_ID".to_string(), session.id.clone()),
        ("BLIRP_PROJECT_ID".to_string(), session.project_id.clone()),
        ("BLIRP_HOME".to_string(), home),
        (
            "BLIRP_MEMORY_FILE".to_string(),
            launch_dir.join("memory.md").display().to_string(),
        ),
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
    ];
    let spawn = SpawnRequest {
        program,
        args,
        cwd: PathBuf::from(&session.cwd),
        env,
        env_remove: ENV_REMOVE.iter().map(|s| s.to_string()).collect(),
        cols: cols.unwrap_or(DEFAULT_COLS).clamp(10, 1000),
        rows: rows.unwrap_or(DEFAULT_ROWS).clamp(4, 1000),
    };

    let st = state.clone();
    let sid = session.id.clone();
    let spawned = tokio::task::spawn_blocking(move || {
        let on_exit_state = st.clone();
        let on_exit_id = sid.clone();
        Terminal::spawn(&sid, spawn, move |info| {
            on_exit(&on_exit_state, &on_exit_id, info)
        })
    })
    .await
    .map_err(|e| ApiError::internal("spawning session", e))?;

    let term = match spawned {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(session = %session.id, error = %format!("{e:#}"), "session failed to start");
            let store = state.store.clone();
            let id = session.id.clone();
            let failed = crate::api::blocking(move || {
                Ok(store.modify_session(&id, |s| {
                    s.status = SessionStatus::Failed;
                    s.ended_at = Some(now_ms());
                })?)
            })
            .await?;
            state.emit(ServerEvent::SessionUpdated { session: failed });
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "spawn_failed",
                format!("could not start {}: {e:#}", agent.display_name),
            ));
        }
    };
    state.terminals.insert(term.clone());
    // A process that exited before registration already ran on_exit.
    if term.has_exited() {
        state.terminals.remove(&session.id);
    }

    if let Some(prompt) = prompt.filter(|p| !p.trim().is_empty()) {
        tokio::spawn(type_prompt(term, prompt));
    }
    Ok(session)
}

/// Type `prompt` once the agent's output has settled, then press Enter.
async fn type_prompt(term: Arc<Terminal>, prompt: String) {
    let begin = Instant::now();
    loop {
        if term.has_exited() {
            return;
        }
        let settled = term
            .last_output()
            .is_some_and(|t| t.elapsed() >= PROMPT_SETTLE);
        if settled || begin.elapsed() >= PROMPT_MAX_WAIT {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut bytes = Vec::with_capacity(prompt.len() + 16);
    if term.bracketed_paste() {
        // Pasted as one block so embedded newlines do not submit early.
        bytes.extend(b"\x1b[200~");
        bytes.extend(prompt.as_bytes());
        bytes.extend(b"\x1b[201~");
    } else {
        bytes.extend(prompt.replace("\r\n", "\n").replace('\n', "\r").as_bytes());
    }
    term.write(bytes);
    // Some TUIs drop an Enter that arrives in the same read as a paste.
    tokio::time::sleep(Duration::from_millis(150)).await;
    term.write(b"\r".to_vec());
}

/// Runs on the PTY waiter thread when the session process exits.
fn on_exit(state: &SharedState, session_id: &str, info: ExitInfo) {
    state.terminals.remove(session_id);
    let now = now_ms();
    match state.store.modify_session(session_id, |s| {
        s.status = info.status;
        s.exit_code = Some(info.code);
        s.ended_at = Some(now);
        s.last_activity_at = now;
    }) {
        Ok(s) => state.emit(ServerEvent::SessionUpdated { session: s }),
        Err(e) => {
            tracing::error!(session = %session_id, error = %e, "recording session exit failed")
        }
    }
}

/// POST /api/sessions/:id/stop
pub fn stop(state: &SharedState, id: &str) -> ApiResult<()> {
    let term = state
        .terminals
        .get(id)
        .ok_or_else(|| ApiError::conflict("not_running", "session is not running"))?;
    term.kill();
    Ok(())
}

/// Apply the output heuristic (§7) to every live session and persist
/// changes. `last` remembers what was reported so unchanged sessions cost
/// no database write.
pub fn refresh_statuses(state: &SharedState, last: &mut HashMap<String, SessionStatus>) {
    let now = Instant::now();
    let terms = state.terminals.all();
    last.retain(|id, _| terms.iter().any(|t| &t.session_id == id));
    for term in terms {
        if term.has_exited() {
            continue;
        }
        let status = term.activity_status(now);
        if last.get(&term.session_id) == Some(&status) {
            continue;
        }
        let mut changed = false;
        let result = state.store.modify_session(&term.session_id, |s| {
            // Exits and hook-driven statuses (waiting) are owned elsewhere.
            if s.status.is_live() && s.status != SessionStatus::Waiting && s.status != status {
                s.status = status;
                s.last_activity_at = now_ms();
                changed = true;
            }
        });
        match result {
            Ok(s) => {
                last.insert(term.session_id.clone(), status);
                if changed {
                    state.emit(ServerEvent::SessionUpdated { session: s });
                }
            }
            Err(e) => {
                tracing::warn!(session = %term.session_id, error = %e, "updating session status failed");
            }
        }
    }
}

/// On startup: sessions that claim a live process from a previous daemon run
/// have lost it.
pub fn mark_detached(state: &SharedState) -> anyhow::Result<()> {
    for s in state.store.live_sessions_on(&state.machine.id)? {
        state.store.modify_session(&s.id, |s| {
            s.status = SessionStatus::Detached;
        })?;
        tracing::info!(session = %s.id, "marked detached after daemon restart");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn worktree_names() {
        let n = super::worktree_name().unwrap();
        let parts: Vec<&str> = n.split('-').collect();
        assert_eq!(parts.len(), 3, "{n}");
        assert_eq!(parts[2].len(), 4);
        assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()));
    }
}
