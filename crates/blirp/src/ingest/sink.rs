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
        inside(home) && !inside(&home.join("worktrees"))
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
        }
        let mut plans = Vec::new();
        for (asid, p) in pending {
            if self.excluded.contains(&asid) {
                continue;
            }
            let existing = store.session_by_agent_id(agent, &asid)?;
            let mut link = None;
            let mut project = None;
            if existing.is_none() {
                if p.events.is_empty() {
                    // Never create an empty session; its facts arrive again.
                    continue;
                }
                let cwd = p.meta.cwd.clone();
                if cwd.as_deref().is_some_and(|c| self.is_excluded_cwd(c)) {
                    self.excluded.insert(asid);
                    continue;
                }
                link = self.link_candidate(&p)?;
                if link.is_none() {
                    let dir = cwd.map_or_else(|| eng.env.home.clone(), Into::into);
                    let r =
                        store.resolve_project_lenient(&eng.machine.id, &eng.machine.name, &dir)?;
                    project = Some((r.project.id, r.created));
                }
            }
            let parent = match &p.meta.parent {
                Some(pa) => store.session_by_agent_id(agent, pa)?.map(|s| s.id),
                None => None,
            };
            plans.push(Plan {
                asid,
                p,
                link,
                project,
                parent,
            });
        }

        let source_key = self.source_key.clone();
        let results = store.ingest_tx(|tx| {
            let mut out = Vec::new();
            for plan in &plans {
                let mut base = tx.session_by_agent_id(agent, &plan.asid)?;
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
                let s = self.merge(base, &plan.asid, &plan.p, project_id, plan.parent.clone());
                let changed = before.as_ref() != Some(&s);
                if changed {
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
                let project_created = created && plan.project.as_ref().is_some_and(|p| p.1);
                out.push((s, created, changed, project_created));
            }
            if let Some(c) = cursor {
                tx.set_cursor(agent, &source_key, &serde_json::to_value(c)?)?;
            }
            Ok(out)
        })?;
        for (s, created, changed, project_created) in results {
            if project_created {
                eng.notifier.project(&s.project_id);
            }
            if created {
                eng.notifier.created(s);
            } else if changed {
                eng.notifier.updated(s);
            }
        }
        Ok(())
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
