//! Storage helpers for the memory engine (§9): read-only access for short-lived
//! processes (hooks, MCP stdio, `blirp mem`), distill candidates and the
//! transactional application of a distill result.

use super::memory::{get_brief_in, put_brief_in, record_row};
use super::projects::{live_local_paths, live_project_in, longest_prefix};
use super::sessions::{event_row, session_row};
use super::{Change, Result, Store, StoreError, all, apply_in, one};
use crate::model::{
    BriefProposal, Event, Project, Record, RecordKind, RecordProposal, RecordStatus, Session,
    SuggestionStatus, SuggestionTarget,
};
use rusqlite::{Connection, OpenFlags, Transaction, params};
use serde_json::Value as JsonValue;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// `settings` key holding this machine's id (written by the daemon).
pub const MACHINE_ID_KEY: &str = "machine_id";
/// `updated_by` of rows written by the distiller.
pub const BY_DISTILLER: &str = "distiller";

/// How a distill result changes the project brief.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BriefApply {
    /// Write a new brief version (history kept).
    Write,
    /// Queue a `suggestion` for the user instead.
    Suggest,
}

/// A validated distill result ready to be written in one transaction.
#[derive(Debug, Clone)]
pub struct DistillPlan {
    pub session_id: String,
    /// Highest event seq the summary covers.
    pub through_seq: i64,
    /// New `summary_json` value.
    pub summary: JsonValue,
    /// Applied only when the session has no title yet.
    pub title: Option<String>,
    /// New records (kind, title, body); duplicates of active records are skipped.
    pub new_records: Vec<(RecordKind, String, String)>,
    pub resolve_record_ids: Vec<String>,
    pub brief_md: Option<String>,
    pub brief_apply: BriefApply,
}

#[derive(Debug, Clone, Default)]
pub struct DistillOutcome {
    pub records_created: usize,
    pub records_resolved: usize,
    pub suggestions_created: usize,
    pub brief_updated: bool,
    pub session: Option<Session>,
}

fn insert_suggestion_in(
    tx: &Transaction<'_>,
    project_id: &str,
    target: SuggestionTarget,
    target_id: Option<&str>,
    proposal: &JsonValue,
    rationale: &str,
    source_session_id: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO suggestions(id, project_id, target, target_id, proposal_json, rationale,
           source_session_id, status, created_at, decided_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,NULL)",
        params![
            crate::new_id(),
            project_id,
            target,
            target_id,
            proposal.to_string(),
            rationale,
            source_session_id,
            SuggestionStatus::Pending,
            crate::now_ms()
        ],
    )?;
    Ok(())
}

impl Store {
    /// Open an existing database without write access or migrations. Used by
    /// short-lived processes that must never block or change the daemon's DB.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let open = || -> Result<Connection> {
            let c = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            c.busy_timeout(Duration::from_secs(2))?;
            Ok(c)
        };
        let conn = open()?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let supported = super::migrations::MIGRATIONS.len() as i64;
        if version == 0 {
            return Err(StoreError::Invalid(
                "database is not initialized; start the blirp daemon once".into(),
            ));
        }
        if version > supported {
            return Err(super::MigrationError::TooNew {
                found: version,
                supported,
            }
            .into());
        }
        Ok(Self {
            path: path.to_path_buf(),
            writer: Mutex::new(open()?),
            readers: Mutex::new(Vec::new()),
        })
    }

    /// This machine's id as recorded by the daemon.
    pub fn machine_id(&self) -> Result<Option<String>> {
        Ok(self
            .get_setting(MACHINE_ID_KEY)?
            .and_then(|v| v.as_str().map(str::to_string)))
    }

    /// Project whose registered folder on `machine_id` contains `path`, without
    /// registering anything (read-only counterpart of `resolve_project`).
    pub fn find_project_for_path(&self, machine_id: &str, path: &Path) -> Result<Option<Project>> {
        let path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.read(|c| {
            let paths = live_local_paths(c, machine_id)?;
            match longest_prefix(&paths, &path) {
                Some(pp) => Ok(Some(live_project_in(c, &pp.project_id)?)),
                None => Ok(None),
            }
        })
    }

    /// Every event of a session, ascending.
    pub fn session_events(&self, session_id: &str) -> Result<Vec<Event>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM events WHERE session_id = ?1 ORDER BY seq",
                params![session_id],
                event_row,
            )
        })
    }

    /// The last `limit` events of `kinds`, ascending.
    pub fn last_events(&self, session_id: &str, kinds: &[&str], limit: i64) -> Result<Vec<Event>> {
        let kinds = serde_json::to_string(kinds)?;
        let mut v = self.read(|c| {
            all(
                c,
                "SELECT * FROM events WHERE session_id = ?1
                   AND kind IN (SELECT value FROM json_each(?2))
                 ORDER BY seq DESC LIMIT ?3",
                params![session_id, kinds, limit],
                event_row,
            )
        })?;
        v.reverse();
        Ok(v)
    }

    pub fn max_event_seq(&self, session_id: &str) -> Result<i64> {
        self.read(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )?)
        })
    }

    /// Session by the agent's own id.
    pub fn session_by_agent_id(
        &self,
        agent: &str,
        agent_session_id: &str,
    ) -> Result<Option<Session>> {
        self.read(|c| {
            one(
                c,
                "SELECT * FROM sessions WHERE agent = ?1 AND agent_session_id = ?2",
                params![agent, agent_session_id],
                session_row,
            )
        })
    }

    /// Sessions on `machine_id` due for distillation (§9 trigger): idle since
    /// `idle_before` or ended, active after `active_after`, with events past
    /// `distilled_through_seq`, and not already failed at the same point.
    /// Ingested subagent children (external with a parent) are left out: the
    /// parent's transcript already carries their task and result. Sessions of
    /// other machines are distilled on their origin machine and replicated.
    /// Most recently active first.
    pub fn distill_candidates(
        &self,
        machine_id: &str,
        idle_before: i64,
        active_after: i64,
        limit: i64,
    ) -> Result<Vec<Session>> {
        self.read(|c| {
            all(
                c,
                "SELECT s.* FROM sessions s
                 WHERE s.machine_id = ?1 AND s.last_activity_at >= ?3
                   AND NOT (s.origin = 'external' AND s.parent_session_id IS NOT NULL)
                   AND ((s.status = 'idle' AND s.last_activity_at <= ?2)
                        OR s.status IN ('completed','failed','detached'))
                   AND EXISTS (SELECT 1 FROM events e WHERE e.session_id = s.id
                               AND e.seq > s.distilled_through_seq
                               AND e.seq > COALESCE(json_extract(s.summary_json, '$.error.through_seq'), 0))
                 ORDER BY s.last_activity_at DESC LIMIT ?4",
                params![machine_id, idle_before, active_after, limit],
                session_row,
            )
        })
    }

    /// Apply a distill result atomically (§9 Apply). Records last edited by the
    /// user are never modified; resolving one creates a suggestion instead.
    pub fn apply_distill(&self, plan: &DistillPlan) -> Result<DistillOutcome> {
        self.write(|tx| {
            let mut out = DistillOutcome::default();
            let mut s = one(
                tx,
                "SELECT * FROM sessions WHERE id = ?1",
                params![plan.session_id],
                session_row,
            )?
            .ok_or(StoreError::NotFound("session"))?;
            let pid = s.project_id.clone();
            if s.title.as_deref().is_none_or(|t| t.trim().is_empty()) {
                s.title = plan.title.clone();
            }
            s.summary = Some(plan.summary.clone());
            s.distilled_through_seq = s.distilled_through_seq.max(plan.through_seq);
            apply_in(tx, &Change::Session(s.clone()))?;

            let active: Vec<Record> = all(
                tx,
                "SELECT * FROM records WHERE project_id = ?1 AND status = 'active'",
                params![pid],
                record_row,
            )?;
            let now = crate::now_ms();
            let mut seen: Vec<(RecordKind, String)> = active
                .iter()
                .map(|r| (r.kind, r.title.trim().to_lowercase()))
                .collect();
            for (kind, title, body) in &plan.new_records {
                let key = (*kind, title.trim().to_lowercase());
                if key.1.is_empty() || seen.contains(&key) {
                    continue;
                }
                seen.push(key);
                apply_in(
                    tx,
                    &Change::Record(Record {
                        id: crate::new_id(),
                        project_id: pid.clone(),
                        kind: *kind,
                        title: title.trim().to_string(),
                        body: body.clone(),
                        status: RecordStatus::Active,
                        pinned: false,
                        source_session_id: Some(s.id.clone()),
                        created_at: now,
                        updated_at: now,
                        updated_by: BY_DISTILLER.into(),
                    }),
                )?;
                out.records_created += 1;
            }

            for id in &plan.resolve_record_ids {
                let Some(mut r) = active.iter().find(|r| &r.id == id).cloned() else {
                    continue;
                };
                if r.updated_by == "user" {
                    let proposal = serde_json::to_value(RecordProposal {
                        kind: r.kind,
                        title: r.title.clone(),
                        body: r.body.clone(),
                        status: Some(RecordStatus::Resolved),
                    })?;
                    insert_suggestion_in(
                        tx,
                        &pid,
                        SuggestionTarget::Record,
                        Some(&r.id),
                        &proposal,
                        "The distiller thinks this was resolved in a later session.",
                        &s.id,
                    )?;
                    out.suggestions_created += 1;
                } else {
                    r.status = RecordStatus::Resolved;
                    r.updated_at = now;
                    r.updated_by = BY_DISTILLER.into();
                    apply_in(tx, &Change::Record(r))?;
                    out.records_resolved += 1;
                }
            }

            if let Some(body) = plan
                .brief_md
                .as_deref()
                .map(str::trim)
                .filter(|b| !b.is_empty())
            {
                let current = get_brief_in(tx, &pid)?;
                if current.as_ref().is_none_or(|b| b.body_md.trim() != body) {
                    match plan.brief_apply {
                        BriefApply::Write => {
                            put_brief_in(tx, &pid, body, BY_DISTILLER)?;
                            out.brief_updated = true;
                        }
                        BriefApply::Suggest => {
                            let proposal = serde_json::to_value(BriefProposal {
                                body_md: body.to_string(),
                            })?;
                            insert_suggestion_in(
                                tx,
                                &pid,
                                SuggestionTarget::Brief,
                                None,
                                &proposal,
                                "Brief update proposed by the distiller.",
                                &s.id,
                            )?;
                            out.suggestions_created += 1;
                        }
                    }
                }
            }
            out.session = Some(s);
            Ok(out)
        })
    }
}
