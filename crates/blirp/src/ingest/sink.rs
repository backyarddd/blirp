//! [`EventSink`] backed by the store: redaction, size caps, session upsert
//! and linking, batched `ingest_tx` transactions, rate-limited notifications.

use super::engine::Engine;
use super::text::{self, TEXT_MAX, TOOL_RESULT_MAX};
use super::{Cursor, EventSink, NormEvent, Result, SessionMeta};
use blirp_core::model::{Event, EventKind, ServerEvent, Session, SessionOrigin, SessionStatus};
use blirp_core::redact;
use blirp_core::store::Change;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Events per transaction.
pub const BATCH: usize = 1000;
/// An external session whose source changed this recently is `working`.
pub const ACTIVE_MS: i64 = 120_000;
/// A transcript's first event must come this soon after a blirp launch to link.
pub const LINK_WINDOW_MS: i64 = 60_000;
/// Text is cut to this before redaction so huge blobs stay cheap.
const PRE_REDACT_MAX: usize = 256 * 1024;

/// A normalized event waiting for its session row: (seq, ts, kind, text, meta).
type PendingEvent = (i64, Option<i64>, EventKind, String, Option<Value>);

#[derive(Default)]
struct Pending {
    meta: SessionMeta,
    events: Vec<PendingEvent>,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
}

pub struct StoreSink<'e> {
    eng: &'e Engine,
    agent: &'static str,
    source_key: String,
    mtime_ms: i64,
    pending: Vec<(String, Pending)>,
    buffered: usize,
    /// Sessions whose cwd is inside BLIRP_HOME (blirp's own runs).
    excluded: HashSet<String>,
}

impl<'e> StoreSink<'e> {
    pub fn new(eng: &'e Engine, agent: &'static str, source_key: &str, mtime_ms: i64) -> Self {
        Self {
            eng,
            agent,
            source_key: source_key.to_string(),
            mtime_ms,
            pending: Vec::new(),
            buffered: 0,
            excluded: HashSet::new(),
        }
    }

    fn pending_mut(&mut self, asid: &str) -> &mut Pending {
        let ix = match self.pending.iter().position(|(a, _)| a == asid) {
            Some(ix) => ix,
            None => {
                self.pending.push((asid.to_string(), Pending::default()));
                self.pending.len() - 1
            }
        };
        &mut self.pending[ix].1
    }

    /// Flush everything and store `cursor` in the same transaction.
    pub fn finish(mut self, cursor: &Cursor) -> Result<()> {
        self.flush(Some(cursor))
    }

    fn is_excluded_cwd(&self, cwd: &str) -> bool {
        let home = &self.eng.env.blirp_home;
        let p = Path::new(cwd);
        let inside = |root: &Path| {
            p.starts_with(root)
                || dunce::canonicalize(root).is_ok_and(|r| p.starts_with(&r))
                || (cfg!(windows)
                    && cwd
                        .to_lowercase()
                        .starts_with(&root.to_string_lossy().to_lowercase()))
        };
        // Session worktrees and project workspaces hold the user's work.
        inside(home) && !inside(&home.join("worktrees")) && !inside(&home.join("workspaces"))
    }

    fn flush(&mut self, cursor: Option<&Cursor>) -> Result<()> {
        let eng = self.eng;
        let store = &eng.store;
        let agent = self.agent;
        let pending = std::mem::take(&mut self.pending);
        self.buffered = 0;

        // Outside the transaction: project resolution may run git and
        // creates projects in its own transaction.
        struct Plan {
            asid: String,
            p: Pending,
            link: Option<String>,
            project: Option<(String, bool)>,
            parent: Option<String>,
            /// (cwd, project id, project created, project when planned) for
            /// a row filed before its cwd was known.
            refile: Option<(String, String, bool, String)>,
        }
        let mut plans = Vec::new();
        let mut drops = Vec::new();
        let mut scripted = Vec::new();
        let mut waiting = Vec::new();
        for (asid, p) in pending {
            if self.excluded.contains(&asid) {
                continue;
            }
            let existing = store.session_by_agent_id(agent, &asid)?;
            let parent = match &p.meta.parent {
                Some(pa) => store.session_by_agent_id(agent, pa)?,
                None => None,
            };
            let foreign =
                |s: &Option<Session>| s.as_ref().is_some_and(|s| s.machine_id != eng.machine.id);
            if foreign(&existing) || foreign(&parent) {
                // The transcript belongs to a replicated session of another
                // machine (e.g. a synced agent config dir). Its origin ingests
                // it; writing here would fight that machine's rows.
                eng.warn_once(
                    agent,
                    &self.source_key,
                    "transcript belongs to another machine's session; skipped",
                );
                self.excluded.insert(asid);
                continue;
            }
            if p.meta.headless == Some(true)
                && let Some(s) = &existing
                && s.origin == SessionOrigin::External
                && store.min_event_seq(&s.id)?.is_none()
            {
                // A scripted run (§8) whose row a hook created before its
                // transcript was read (a row with stored events is left to
                // the one-time cleanup, which reads the whole transcript).
                // The project the hook resolved goes too when it was made
                // for this row alone.
                scripted.push(s.id.clone());
                self.excluded.insert(asid);
                continue;
            }
            let mut link = None;
            let mut project = None;
            let mut refile = None;
            if let Some(s) = &existing
                && let Some(cwd) = self.misfiled(s, &p)?
            {
                if self.is_excluded_cwd(&cwd) {
                    // One of blirp's own runs, filed before its cwd was known.
                    drops.push(s.id.clone());
                    self.excluded.insert(asid);
                    continue;
                }
                // Only a row still in Chats (where the home folder filed it)
                // gets a project: one the user moved keeps theirs.
                let in_chats = store.get_project(&s.project_id)?.is_some_and(|p| p.chats)
                    || store.home_project_id()?.as_deref() == Some(s.project_id.as_str());
                refile = Some(if in_chats {
                    let r = store.resolve_project_lenient(
                        &eng.machine.id,
                        &eng.machine.name,
                        Path::new(&cwd),
                        p.meta.git_remote.as_deref(),
                        &eng.env.non_projects,
                    )?;
                    (cwd, r.project.id, r.created, s.project_id.clone())
                } else {
                    (cwd, s.project_id.clone(), false, s.project_id.clone())
                });
            }
            if existing.is_none() {
                if p.events.is_empty() {
                    // Never create an empty session. Mid-read its facts (an
                    // early cwd report) wait for its events; at the end of
                    // the read they arrive again next time.
                    if cursor.is_none() {
                        waiting.push((asid, p));
                    }
                    continue;
                }
                if cursor.is_none() && p.meta.cwd.is_none() {
                    // The cwd decides the project, the launch link and the
                    // BLIRP_HOME exclusion: a mid-read flush waits for it
                    // (adapters report it as soon as they read it) or for
                    // the end of the read.
                    waiting.push((asid, p));
                    continue;
                }
                let cwd = p.meta.cwd.clone();
                if cwd.as_deref().is_some_and(|c| self.is_excluded_cwd(c)) {
                    self.excluded.insert(asid);
                    continue;
                }
                link = self.link_candidate(&p)?;
                if p.meta.headless == Some(true) && link.is_none() && parent.is_none() {
                    // A scripted run (`claude -p`, Agent SDK, `codex exec`,
                    // §8): no session, no project. A blirp launch keeps it,
                    // and a subagent of a stored session is that session's
                    // (it inherits the entrypoint and has a single prompt).
                    self.excluded.insert(asid);
                    continue;
                }
                // A subagent works for its parent: same project, also after
                // the parent was moved (or its project merged). A parent in
                // a removed project leaves the folder to decide.
                let parent_project = match (&link, &parent) {
                    (None, Some(par)) => store.current_project(&par.project_id)?,
                    _ => None,
                };
                if let Some(pp) = parent_project {
                    project = Some((pp.id, false));
                } else if link.is_none() {
                    let dir = cwd.map_or_else(|| eng.env.home.clone(), Into::into);
                    let r = store.resolve_project_lenient(
                        &eng.machine.id,
                        &eng.machine.name,
                        &dir,
                        p.meta.git_remote.as_deref(),
                        &eng.env.non_projects,
                    )?;
                    project = Some((r.project.id, r.created));
                }
            }
            let parent = parent.map(|s| s.id);
            plans.push(Plan {
                asid,
                p,
                link,
                project,
                parent,
                refile,
            });
        }
        self.pending = waiting;

        if !scripted.is_empty() {
            let gone = store.remove_headless_sessions(&eng.machine.id, &scripted)?;
            for id in gone.sessions {
                eng.notifier.deleted(id);
            }
            for p in &gone.projects {
                eng.notifier.project(p);
            }
        }
        let source_key = self.source_key.clone();
        let results = store.ingest_tx(|tx| {
            for id in &drops {
                tx.apply(&Change::DeleteSession { id: id.clone() })?;
            }
            let mut out = Vec::new();
            for plan in &plans {
                let mut base = tx.session_by_agent_id(agent, &plan.asid)?;
                if base
                    .as_ref()
                    .is_some_and(|s| s.machine_id != eng.machine.id)
                {
                    // Replicated in since planning; see the check above.
                    continue;
                }
                if base.is_none()
                    && let Some(id) = &plan.link
                {
                    base = tx.get_session(id)?.filter(|s| s.agent_session_id.is_none());
                }
                let project_id = match (&base, &plan.project) {
                    (Some(_), _) => None,
                    (None, Some((pid, _))) => Some(pid.as_str()),
                    // The link target was taken meanwhile: abort without
                    // storing the cursor so the next pass re-plans.
                    (None, None) => {
                        return Err(blirp_core::store::StoreError::Conflict(
                            "link target changed during ingest".into(),
                        ));
                    }
                };
                let created = base.is_none();
                let before = base.clone();
                let mut s = self.merge(base, &plan.asid, &plan.p, project_id, plan.parent.clone());
                // A re-file moves the row only while it is still where the
                // plan saw it; a move made since planning wins.
                let mut moving = false;
                if let Some((cwd, pid, _, planned)) = &plan.refile {
                    s.cwd.clone_from(cwd);
                    if before.as_ref().is_some_and(|b| &b.project_id == planned) {
                        s.project_id.clone_from(pid);
                        moving = true;
                    }
                }
                let changed = before.as_ref() != Some(&s);
                if changed && moving {
                    tx.apply_move(&Change::Session(s.clone()))?;
                } else if changed {
                    tx.apply(&Change::Session(s.clone()))?;
                }
                for (seq, ts, kind, text, meta) in &plan.p.events {
                    tx.apply(&Change::Event(Event {
                        session_id: s.id.clone(),
                        seq: *seq,
                        ts: ts.or(plan.p.last_ts).unwrap_or(self.mtime_ms),
                        kind: *kind,
                        text: text.clone(),
                        meta: meta.clone(),
                    }))?;
                }
                // A project resolution created for this row (a re-file that
                // did not happen leaves its new project unused).
                let project_created = match (&plan.project, &plan.refile) {
                    (Some((pid, true)), _) if created => Some(pid.clone()),
                    (_, Some((_, pid, true, _))) if moving => Some(pid.clone()),
                    _ => None,
                };
                out.push((s, created, changed, project_created));
            }
            if let Some(c) = cursor {
                tx.set_cursor(agent, &source_key, &serde_json::to_value(c)?)?;
            }
            Ok(out)
        })?;
        for id in drops {
            eng.notifier.deleted(id);
        }
        for (s, created, changed, project_created) in results {
            if let Some(pid) = project_created {
                eng.notifier.project(&pid);
            }
            if created {
                eng.notifier.created(s);
            } else if changed {
                eng.notifier.updated(s);
            }
        }
        Ok(())
    }

    /// The transcript's cwd when `s` is an ingested row filed under the home
    /// folder because its cwd was unknown when the row was created, and this
    /// read covers the start of the transcript (so `meta.cwd` is where the
    /// session began, not a later `cd`). Linked and blirp-owned rows keep
    /// their cwd (§8).
    fn misfiled(&self, s: &Session, p: &Pending) -> Result<Option<String>> {
        let Some(cwd) = p.meta.cwd.as_deref() else {
            return Ok(None);
        };
        let home = self.eng.env.home.to_string_lossy();
        if s.origin != SessionOrigin::External
            || !text::same_path(&s.cwd, &home)
            || text::same_path(cwd, &home)
        {
            return Ok(None);
        }
        let first_read = p.events.iter().map(|e| e.0).min();
        let first_stored = self.eng.store.min_event_seq(&s.id)?;
        let from_start = match (first_read, first_stored) {
            (Some(r), Some(s)) => r <= s,
            (Some(_), None) => true,
            (None, _) => false,
        };
        Ok(from_start.then(|| {
            dunce::canonicalize(cwd)
                .map(|c| c.display().to_string())
                .unwrap_or_else(|_| cwd.to_string())
        }))
    }

    /// §7: a blirp-launched session of this agent in the same folder whose
    /// launch preceded the transcript's first event by at most 60 s.
    fn link_candidate(&self, p: &Pending) -> Result<Option<String>> {
        let (Some(cwd), Some(first)) = (&p.meta.cwd, p.meta.started_at.or(p.first_ts)) else {
            return Ok(None);
        };
        let cands = self.eng.store.unlinked_blirp_sessions(
            &self.eng.machine.id,
            self.agent,
            first - LINK_WINDOW_MS,
            first,
        )?;
        Ok(cands
            .into_iter()
            .find(|s| text::same_path(&s.cwd, cwd))
            .map(|s| s.id))
    }

    fn merge(
        &self,
        base: Option<Session>,
        asid: &str,
        p: &Pending,
        project_id: Option<&str>,
        parent: Option<String>,
    ) -> Session {
        let m = &p.meta;
        let now = blirp_core::now_ms();
        let last_ts = p.last_ts.or(p.first_ts).max(m.last_activity);
        let title = m
            .title
            .as_deref()
            .map(|t| text::one_line(t, text::TITLE_MAX))
            .filter(|t| !t.is_empty())
            .or_else(|| m.first_prompt.as_deref().and_then(text::title_from_prompt))
            .map(|t| redact::redact(&t).into_owned());
        let mut s = base.unwrap_or_else(|| {
            let started = m.started_at.or(p.first_ts).unwrap_or(self.mtime_ms);
            let cwd = m
                .cwd
                .clone()
                .unwrap_or_else(|| self.eng.env.home.display().to_string());
            Session {
                id: blirp_core::new_id(),
                project_id: project_id.unwrap_or_default().to_string(),
                machine_id: self.eng.machine.id.clone(),
                agent: self.agent.to_string(),
                agent_session_id: Some(asid.to_string()),
                origin: SessionOrigin::External,
                cwd: dunce::canonicalize(&cwd)
                    .map(|c| c.display().to_string())
                    .unwrap_or(cwd),
                title: None,
                status: SessionStatus::Completed,
                branch: None,
                worktree: None,
                transcript_path: None,
                started_at: started,
                ended_at: None,
                last_activity_at: started,
                exit_code: None,
                summary: None,
                distilled_through_seq: 0,
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                parent_session_id: None,
                stopped_by_user: false,
                title_updated_at: 0,
                project_updated_at: 0,
                compacted_at: None,
                context_near_full_at: None,
            }
        });
        if s.agent_session_id.is_none() {
            s.agent_session_id = Some(asid.to_string());
        }
        if s.title.is_none() {
            s.title = title;
        }
        if s.branch.is_none() {
            s.branch.clone_from(&m.branch);
        }
        if s.parent_session_id.is_none() {
            s.parent_session_id = parent;
        }
        if let Some(t) = &m.transcript_path {
            s.transcript_path = Some(t.clone());
        }
        if let Some(v) = m.tokens_in {
            s.tokens_in = v;
        }
        if let Some(v) = m.tokens_out {
            s.tokens_out = v;
        }
        if let Some(v) = m.cost_usd.filter(|c| c.is_finite()) {
            s.cost_usd = v;
        }
        // Stamped like the events themselves (see the flush).
        let compacted = p
            .events
            .iter()
            .filter(|(_, _, kind, _, meta)| is_compaction(*kind, meta.as_ref()))
            .map(|(_, ts, ..)| ts.or(p.last_ts).unwrap_or(self.mtime_ms))
            .max();
        s.compacted_at = s.compacted_at.max(compacted);
        s.context_near_full_at = s.context_near_full_at.max(m.context_near_full_at);
        if let Some(t) = last_ts {
            s.last_activity_at = s.last_activity_at.max(t);
        }
        if s.origin == SessionOrigin::External {
            if let Some(st) = m.started_at.or(p.first_ts) {
                s.started_at = s.started_at.min(st);
            }
            // blirp-owned rows keep their PTY-driven status and timestamps.
            let active =
                now - self.mtime_ms < ACTIVE_MS && self.mtime_ms - s.last_activity_at < ACTIVE_MS;
            if active {
                s.status = SessionStatus::Working;
                s.ended_at = None;
            } else {
                s.status = SessionStatus::Completed;
                s.ended_at = Some(s.last_activity_at);
            }
        }
        s
    }
}

/// The agent compacted its context: a compaction summary (claude
/// `isCompactSummary`, opencode summary messages, pi `compaction`), not a pi
/// branch summary.
fn is_compaction(kind: EventKind, meta: Option<&Value>) -> bool {
    kind == EventKind::Summary
        && !meta
            .and_then(|m| m.get("branch_summary"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn normalize(kind: EventKind, text: &str, meta: Option<Value>) -> (String, Option<Value>) {
    let cap = if matches!(kind, EventKind::ToolResult | EventKind::ToolCall) {
        TOOL_RESULT_MAX
    } else {
        TEXT_MAX
    };
    let red = redact::redact(text::prefix(text, PRE_REDACT_MAX));
    let mut out = text::truncate(&red, cap);
    if text.len() > PRE_REDACT_MAX && out.len() == red.len() {
        out.push_str(&format!(
            "\n…[truncated {} bytes]",
            text.len() - PRE_REDACT_MAX
        ));
    }
    let meta = meta.map(|mut m| {
        redact::redact_json(&mut m);
        text::cap_meta(m)
    });
    (out, meta)
}

impl EventSink for StoreSink<'_> {
    fn session(&mut self, asid: &str, meta: SessionMeta) {
        self.pending_mut(asid).meta.merge(meta);
    }

    fn event(&mut self, asid: &str, ev: NormEvent) -> Result<()> {
        if self.eng.stopping() {
            anyhow::bail!("ingest stopped");
        }
        if self.excluded.contains(asid) {
            return Ok(());
        }
        let (text, meta) = normalize(ev.kind, &ev.text, ev.meta);
        let p = self.pending_mut(asid);
        if let Some(ts) = ev.ts {
            p.first_ts = Some(p.first_ts.map_or(ts, |f| f.min(ts)));
            p.last_ts = Some(p.last_ts.map_or(ts, |l| l.max(ts)));
        }
        p.events.push((ev.seq, ev.ts, ev.kind, text, meta));
        self.buffered += 1;
        if self.buffered >= BATCH {
            self.flush(None)?;
        }
        Ok(())
    }

    fn warn(&mut self, what: &str) {
        self.eng.warn_once(self.agent, &self.source_key, what);
    }
}

// ---------------------------------------------------------------- notifications

pub type Emit = Arc<dyn Fn(ServerEvent) + Send + Sync>;

/// Minimum gap between two updates of the same session.
const MIN_GAP: Duration = Duration::from_secs(1);

#[derive(Default)]
struct NotifyState {
    last: HashMap<String, Instant>,
    pending: HashMap<String, Session>,
}

/// `/api/events/ws` notifications, at most one per session per second; a
/// suppressed update is sent by [`Notifier::flush`] once its gap has passed.
pub struct Notifier {
    emit: Emit,
    st: Mutex<NotifyState>,
}

impl Notifier {
    pub fn new(emit: Emit) -> Self {
        Self {
            emit,
            st: Mutex::new(NotifyState::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, NotifyState> {
        // Only plain maps inside; a panic mid-update leaves them usable.
        self.st.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn created(&self, s: Session) {
        {
            let mut st = self.state();
            st.pending.remove(&s.id);
            st.last.insert(s.id.clone(), Instant::now());
        }
        (self.emit)(ServerEvent::SessionCreated { session: s });
    }

    pub fn updated(&self, s: Session) {
        {
            let mut st = self.state();
            if st.last.get(&s.id).is_some_and(|t| t.elapsed() < MIN_GAP) {
                st.pending.insert(s.id.clone(), s);
                return;
            }
            st.last.insert(s.id.clone(), Instant::now());
        }
        (self.emit)(ServerEvent::SessionUpdated { session: s });
    }

    pub fn deleted(&self, session_id: String) {
        {
            let mut st = self.state();
            st.pending.remove(&session_id);
            st.last.remove(&session_id);
        }
        (self.emit)(ServerEvent::SessionDeleted { session_id });
    }

    pub fn project(&self, project_id: &str) {
        (self.emit)(ServerEvent::ProjectUpdated {
            project_id: project_id.to_string(),
        });
    }

    /// Send suppressed updates whose gap has passed; forget old entries.
    pub fn flush(&self) {
        let due: Vec<Session> = {
            let mut st = self.state();
            let ids: Vec<String> = st
                .pending
                .keys()
                .filter(|id| st.last.get(*id).is_none_or(|t| t.elapsed() >= MIN_GAP))
                .cloned()
                .collect();
            let now = Instant::now();
            let due: Vec<Session> = ids.iter().filter_map(|id| st.pending.remove(id)).collect();
            for s in &due {
                st.last.insert(s.id.clone(), now);
            }
            let NotifyState { last, pending } = &mut *st;
            last.retain(|id, t| pending.contains_key(id) || t.elapsed() < Duration::from_secs(60));
            due
        };
        for s in due {
            (self.emit)(ServerEvent::SessionUpdated { session: s });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compaction_summaries_count_but_pi_branch_summaries_do_not() {
        assert!(is_compaction(EventKind::Summary, None));
        assert!(is_compaction(EventKind::Summary, Some(&json!({"x": 1}))));
        let branch = json!({"branch_summary": true});
        assert!(!is_compaction(EventKind::Summary, Some(&branch)));
        assert!(!is_compaction(EventKind::System, None));
    }
}
