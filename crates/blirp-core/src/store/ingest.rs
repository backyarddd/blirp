//! Store access for transcript ingest (§8): session lookup and linking,
//! event batches and ingest cursors written in one transaction. Replicated
//! rows still go through [`apply_in`], so every write lands in the outbox.

use super::sessions::session_row;
use super::{Change, Result, Store, all, apply_in, one};
use crate::model::Session;
use rusqlite::{Connection, Transaction, params};
use serde_json::Value as JsonValue;

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
            distilled_through_seq: 0,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            parent_session_id: None,
            stopped_by_user: false,
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
