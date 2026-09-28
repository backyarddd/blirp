//! Store access for transcript ingest (§8): session lookup and linking,
//! event batches and ingest cursors written in one transaction. Replicated
//! rows still go through [`apply_in`], so every write lands in the outbox.

use super::sessions::session_row;
use super::{Change, Result, Store, all, apply_in, one};
use crate::model::{Session, SessionOrigin};
use rusqlite::{Connection, Transaction, params};
use serde_json::Value as JsonValue;
use std::collections::HashSet;

/// What [`Store::remove_headless_sessions`] removed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeadlessCleanup {
    /// Deleted sessions, ingested subagents included.
    pub sessions: Vec<String>,
    pub records: usize,
    /// Distiller records of those sessions kept because a remaining session
    /// was distilled after they existed (it may have reached them too).
    pub records_kept: usize,
    /// Projects deleted because nothing else was in them.
    pub projects: Vec<String>,
}

/// A record as the distiller wrote it: unpinned and never edited by a user.
// `'distiller'` is [`super::BY_DISTILLER`].
const DISTILLER_ONLY: &str = "r.pinned = 0 AND r.updated_by = 'distiller'";

/// Records the distiller wrote from one of `sessions` ([`DISTILLER_ONLY`])
/// that no other session relied on: none in the record's project, outside
/// `sessions`, was distilled after the record was created. The distiller
/// adds no second record with the title of an active one, so such a run had
/// the record in its prompt and may have left out a duplicate of it. The one
/// rule for removing what removed sessions produced (the scripted-run
/// cleanup and the codex subagent repair, §8).
pub(crate) fn distiller_records_only_of_in(
    c: &Connection,
    sessions: &[String],
) -> Result<Vec<String>> {
    all(
        c,
        &format!(
            "SELECT r.id FROM records r
             WHERE r.source_session_id IN (SELECT value FROM json_each(?1))
               AND {DISTILLER_ONLY}
               AND NOT EXISTS (
                 SELECT 1 FROM sessions s
                 WHERE s.project_id = r.project_id
                   AND s.id NOT IN (SELECT value FROM json_each(?1))
                   AND json_extract(s.summary_json, '$.distilled_at') > r.created_at)
             ORDER BY r.id"
        ),
        params![serde_json::to_string(sessions)?],
        |r| r.get(0),
    )
}

fn by_agent_id(c: &Connection, agent: &str, agent_session_id: &str) -> Result<Option<Session>> {
    one(
        c,
        "SELECT * FROM sessions WHERE agent = ?1 AND agent_session_id = ?2",
        params![agent, agent_session_id],
        session_row,
    )
}

fn unlinked(
    c: &Connection,
    machine_id: &str,
    agent: &str,
    from: i64,
    to: i64,
) -> Result<Vec<Session>> {
    all(
        c,
        "SELECT * FROM sessions WHERE machine_id = ?1 AND agent = ?2 AND origin = 'blirp'
           AND agent_session_id IS NULL AND started_at BETWEEN ?3 AND ?4
         ORDER BY started_at DESC",
        params![machine_id, agent, from, to],
        session_row,
    )
}

/// A write transaction handed to [`Store::ingest_tx`].
pub struct IngestTx<'a> {
    tx: &'a Transaction<'a>,
}

impl IngestTx<'_> {
    /// Apply a replicated change (row + outbox). False for a duplicate event.
    pub fn apply(&self, change: &Change) -> Result<bool> {
        apply_in(self.tx, change)
    }

    /// [`Self::apply`] for a session re-filed under another project.
    pub fn apply_move(&self, change: &Change) -> Result<bool> {
        super::apply_move_in(self.tx, change)
    }

    pub fn session_by_agent_id(
        &self,
        agent: &str,
        agent_session_id: &str,
    ) -> Result<Option<Session>> {
        by_agent_id(self.tx, agent, agent_session_id)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        one(
            self.tx,
            "SELECT * FROM sessions WHERE id = ?1",
            params![id],
            session_row,
        )
    }

    /// blirp-launched sessions of `agent` on this machine that have no agent
    /// session id yet and started within `[from, to]`, closest to `to` first.
    pub fn unlinked_blirp_sessions(
        &self,
        machine_id: &str,
        agent: &str,
        from: i64,
        to: i64,
    ) -> Result<Vec<Session>> {
        unlinked(self.tx, machine_id, agent, from, to)
    }

    pub fn set_cursor(&self, adapter: &str, source: &str, cursor: &JsonValue) -> Result<()> {
        self.tx.execute(
            "INSERT INTO ingest_cursors(adapter, source, cursor_json) VALUES (?1, ?2, ?3)
             ON CONFLICT(adapter, source) DO UPDATE SET cursor_json = excluded.cursor_json",
            params![adapter, source, cursor.to_string()],
        )?;
        Ok(())
    }
}

impl Store {
    /// Run `f` in one immediate write transaction.
    pub fn ingest_tx<T>(&self, f: impl FnOnce(&IngestTx<'_>) -> Result<T>) -> Result<T> {
        self.write(|tx| f(&IngestTx { tx }))
    }

    /// Read-only [`IngestTx::unlinked_blirp_sessions`].
    pub fn unlinked_blirp_sessions(
        &self,
        machine_id: &str,
        agent: &str,
        from: i64,
        to: i64,
    ) -> Result<Vec<Session>> {
        self.read(|c| unlinked(c, machine_id, agent, from, to))
    }

    /// Forget the cursors of ingested sessions on `machine_id` filed under
    /// `cwd` (the folder ingest used when a transcript's cwd was unknown), so
    /// their transcripts are read again from the start and re-filed. Events
    /// dedupe, so a re-read adds nothing twice. Returns how many were reset.
    pub fn reset_cursors_filed_under(&self, machine_id: &str, cwd: &str) -> Result<usize> {
        self.write(|tx| {
            Ok(tx.execute(
                "DELETE FROM ingest_cursors WHERE EXISTS (
                   SELECT 1 FROM sessions s WHERE s.machine_id = ?1 AND s.origin = 'external'
                     AND s.cwd = ?2 AND s.agent = ingest_cursors.adapter
                     AND s.transcript_path = ingest_cursors.source)",
                params![machine_id, cwd],
            )?)
        })
    }

    /// Ingested claude and codex sessions of `machine_id` with a transcript:
    /// the rows that can be scripted runs (§8). Subagents are left out: they
    /// look scripted (the parent's entrypoint, one prompt) and go with a
    /// removed parent, stay with a kept one.
    pub fn headless_candidates(&self, machine_id: &str) -> Result<Vec<Session>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM sessions WHERE machine_id = ?1 AND origin = 'external'
                   AND agent IN ('claude','codex') AND transcript_path IS NOT NULL
                   AND parent_session_id IS NULL",
                params![machine_id],
                session_row,
            )
        })
    }

    /// Events of `session_id` below `below_seq` (what a
    /// [`Change::TruncateEvents`] would drop).
    pub fn count_events_below(&self, session_id: &str, below_seq: i64) -> Result<i64> {
        self.read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM events WHERE session_id = ?1 AND seq < ?2",
                params![session_id, below_seq],
                |r| r.get(0),
            )?)
        })
    }

    /// Records only the distiller wrote from one of `sessions` that no other
    /// session relied on ([`distiller_records_only_of_in`]).
    pub fn distiller_records_only_of(&self, sessions: &[String]) -> Result<Vec<String>> {
        self.read(|c| distiller_records_only_of_in(c, sessions))
    }

    /// Remove sessions of scripted runs (§8) that ingest stored before it
    /// skipped them, the way a user delete does (tombstones and outbox, so
    /// every machine drops them): of `ids`, the rows still external sessions
    /// of `machine_id`, with their ingested subagents; the records the
    /// distiller made from them (unpinned, never edited by the user); and
    /// projects ingest created only for them (untouched, §5, and every
    /// session going). Their ingest cursors are dropped, so a transcript
    /// that grows later is judged from its start again. Idempotent.
    pub fn remove_headless_sessions(
        &self,
        machine_id: &str,
        ids: &[String],
    ) -> Result<HeadlessCleanup> {
        self.write(|tx| {
            let get = |id: &str| {
                one(
                    tx,
                    "SELECT * FROM sessions WHERE id = ?1",
                    params![id],
                    session_row,
                )
            };
            let mut doomed: Vec<Session> = Vec::new();
            for id in ids {
                if let Some(s) = get(id)?
                    && s.machine_id == machine_id
                    && s.origin == SessionOrigin::External
                {
                    doomed.push(s);
                }
            }
            // Deleting a session deletes its ingested subagents too.
            let mut children = Vec::new();
            for s in &doomed {
                children.extend(all(
                    tx,
                    "SELECT * FROM sessions WHERE parent_session_id = ?1 AND origin = 'external'",
                    params![s.id],
                    session_row,
                )?);
            }
            doomed.extend(children);
            let mut seen = HashSet::new();
            doomed.retain(|s| seen.insert(s.id.clone()));
            let gone: HashSet<&str> = doomed.iter().map(|s| s.id.as_str()).collect();

            let mut out = HeadlessCleanup::default();
            let mut projects = Vec::new();
            for (p, _) in super::projects::untouched_projects(tx, machine_id)? {
                let members: Vec<String> = all(
                    tx,
                    "SELECT id FROM sessions WHERE project_id = ?1",
                    params![p.id],
                    |r| r.get(0),
                )?;
                if members.iter().all(|id| gone.contains(id.as_str())) {
                    projects.push(p);
                }
            }
            // Records a remaining session may have relied on are kept (their
            // source is cleared with the session).
            let gone_ids: Vec<String> = doomed.iter().map(|s| s.id.clone()).collect();
            let deletable = distiller_records_only_of_in(tx, &gone_ids)?;
            let all_records: i64 = tx.query_row(
                &format!(
                    "SELECT count(*) FROM records r WHERE r.source_session_id IN
                       (SELECT value FROM json_each(?1)) AND {DISTILLER_ONLY}"
                ),
                params![serde_json::to_string(&gone_ids)?],
                |r| r.get(0),
            )?;
            out.records_kept = usize::try_from(all_records)
                .unwrap_or(0)
                .saturating_sub(deletable.len());
            for id in deletable {
                apply_in(tx, &Change::DeleteRecord { id })?;
                out.records += 1;
            }
            for s in &doomed {
                if get(&s.id)?.is_some() {
                    apply_in(tx, &Change::DeleteSession { id: s.id.clone() })?;
                }
                if let Some(t) = &s.transcript_path {
                    tx.execute(
                        "DELETE FROM ingest_cursors WHERE adapter = ?1 AND source = ?2",
                        params![s.agent, t],
                    )?;
                }
                out.sessions.push(s.id.clone());
            }
            let now = crate::now_ms();
            for mut p in projects {
                p.deleted = true;
                p.updated_at = now;
                out.projects.push(p.id.clone());
                apply_in(tx, &Change::Project(p))?;
            }
            Ok(out)
        })
    }

    /// External sessions on this machine still marked `working` whose last
    /// activity is before `before`.
    pub fn stale_external_sessions(&self, machine_id: &str, before: i64) -> Result<Vec<Session>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM sessions WHERE machine_id = ?1 AND origin = 'external'
                   AND status = 'working' AND last_activity_at < ?2",
                params![machine_id, before],
                session_row,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::temp_store;
    use super::*;
    use crate::model::{Event, EventKind, SessionOrigin, SessionStatus};

    fn session(id: &str, origin: SessionOrigin, asid: Option<&str>, started_at: i64) -> Session {
        Session {
            id: id.into(),
            project_id: "p".into(),
            machine_id: "m".into(),
            agent: "codex".into(),
            agent_session_id: asid.map(str::to_string),
            origin,
            cwd: "/x".into(),
            title: None,
            status: SessionStatus::Working,
            branch: None,
            worktree: None,
            transcript_path: None,
            started_at,
            ended_at: None,
            last_activity_at: started_at,
            exit_code: None,
            summary: None,
            distilled_through_seq: crate::model::NOT_DISTILLED,
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
    }

    #[test]
    fn ingest_tx_writes_rows_outbox_and_cursor_atomically() {
        let (_d, store) = temp_store();
        store
            .insert_session(&session("b", SessionOrigin::Blirp, None, 1000))
            .unwrap();
        store
            .insert_session(&session("old", SessionOrigin::Blirp, None, 10))
            .unwrap();
        let found = store
            .ingest_tx(|tx| {
                let c = tx.unlinked_blirp_sessions("m", "codex", 900, 1100)?;
                let mut s = c[0].clone();
                s.agent_session_id = Some("rollout-1".into());
                tx.apply(&Change::Session(s))?;
                tx.apply(&Change::Event(Event {
                    session_id: "b".into(),
                    seq: 1,
                    ts: 1001,
                    kind: EventKind::User,
                    text: "hi".into(),
                    meta: None,
                }))?;
                tx.set_cursor("codex", "/r.jsonl", &serde_json::json!({"offset": 5}))?;
                Ok(c.len())
            })
            .unwrap();
        assert_eq!(found, 1);
        assert_eq!(
            store
                .session_by_agent_id("codex", "rollout-1")
                .unwrap()
                .unwrap()
                .id,
            "b"
        );
        assert_eq!(store.outbox_after(0, 10).unwrap().len(), 4);
        assert!(store.get_cursor("codex", "/r.jsonl").unwrap().is_some());

        // A failed transaction keeps nothing, cursor included.
        let err = store.ingest_tx(|tx| {
            tx.set_cursor("codex", "/other", &serde_json::json!({}))?;
            Err::<(), _>(super::super::StoreError::Invalid("boom".into()))
        });
        assert!(err.is_err());
        assert!(store.get_cursor("codex", "/other").unwrap().is_none());

        let mut ext = session("e", SessionOrigin::External, Some("x"), 50);
        ext.last_activity_at = 50;
        store.insert_session(&ext).unwrap();
        let stale = store.stale_external_sessions("m", 100).unwrap();
        assert_eq!(
            stale.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["e"]
        );
    }
}
