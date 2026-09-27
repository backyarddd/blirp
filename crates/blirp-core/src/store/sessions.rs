//! Sessions, events and full-text search.

use super::{Change, Result, Store, StoreError, all, apply_in, json_col, one, write_row};
use crate::model::{Event, SearchHit, SearchHitKind, Session, SessionStatus, SessionsPage};
use rusqlite::{Row, params, params_from_iter, types::Value};

pub(super) fn session_row(r: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get("id")?,
        project_id: r.get("project_id")?,
        machine_id: r.get("machine_id")?,
        agent: r.get("agent")?,
        agent_session_id: r.get("agent_session_id")?,
        origin: r.get("origin")?,
        cwd: r.get("cwd")?,
        title: r.get("title")?,
        status: r.get("status")?,
        branch: r.get("branch")?,
        worktree: r.get("worktree")?,
        transcript_path: r.get("transcript_path")?,
        started_at: r.get("started_at")?,
        ended_at: r.get("ended_at")?,
        last_activity_at: r.get("last_activity_at")?,
        exit_code: r.get("exit_code")?,
        summary: json_col(r, "summary_json")?,
        distilled_through_seq: r.get("distilled_through_seq")?,
        tokens_in: r.get("tokens_in")?,
        tokens_out: r.get("tokens_out")?,
        cost_usd: r.get("cost_usd")?,
        parent_session_id: r.get("parent_session_id")?,
        stopped_by_user: r.get("stopped_by_user")?,
    })
}

pub(super) fn event_row(r: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        session_id: r.get("session_id")?,
        seq: r.get("seq")?,
        ts: r.get("ts")?,
        kind: r.get("kind")?,
        text: r.get("text")?,
        meta: json_col(r, "meta_json")?,
    })
}

/// At most one status-only outbox entry per session per window (ms).
pub const STATUS_COALESCE_MS: i64 = 5_000;

/// Only a live status (and the activity time with it) changed.
fn status_only(before: &Session, after: &Session) -> bool {
    if !before.status.is_live() || !after.status.is_live() {
        return false;
    }
    let mut probe = after.clone();
    probe.status = before.status;
    probe.last_activity_at = before.last_activity_at;
    probe == *before
}

#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    pub project_id: Option<String>,
    pub status: Option<SessionStatus>,
    pub agent: Option<String>,
    pub machine_id: Option<String>,
    /// Substring match on title, cwd and agent.
    pub q: Option<String>,
    /// Only subagent children of this session (origin `external` with
    /// `parent_session_id` set to it).
    pub parent: Option<String>,
    /// Leave out every subagent child session.
    pub hide_children: bool,
    /// Opaque cursor from a previous page.
    pub cursor: Option<String>,
    pub limit: i64,
    /// This machine: its live sessions always sort first. Another machine's
    /// only while it is in `online`; never while it is in `offline` (a
    /// replica keeps the last status it heard, which an offline machine
    /// cannot correct); with its presence unknown, only while their activity
    /// is younger than [`REMOTE_LIVE_MS`]. `None` treats every live status
    /// as current.
    pub local_machine: Option<String>,
    /// Machines connected to the hub right now.
    pub online: Vec<String>,
    /// Machines known not to be connected to the hub.
    pub offline: Vec<String>,
}

/// With a machine's presence unknown, how long its live session stays
/// pinned (and reads as live in the UI) without a replicated update.
pub const REMOTE_LIVE_MS: i64 = 30 * 60_000;

/// SQL condition for "is an ingested subagent session" (§8): continue/fork
/// sessions also carry a parent but are the user's own (origin `blirp`).
const IS_CHILD: &str = "(origin = 'external' AND parent_session_id IS NOT NULL)";

/// 1 when the session sorts as live (see [`SessionFilter::local_machine`]):
/// `?1` the local machine id (or NULL), `?2` the activity cutoff for
/// machines of unknown presence, `?3`/`?4` JSON arrays of online/offline ids.
const LIVE: &str = "(status IN ('starting','working','idle','waiting')
    AND (?1 IS NULL OR machine_id = ?1
         OR machine_id IN (SELECT value FROM json_each(?3))
         OR (machine_id NOT IN (SELECT value FROM json_each(?4)) AND last_activity_at >= ?2)))";

/// `live:last_activity_at:id` from [`Store::list_sessions`].
fn parse_cursor(c: &str) -> Option<(i64, i64, String)> {
    let (live, rest) = c.split_once(':')?;
    let (ts, id) = rest.split_once(':')?;
    let live = match live {
        "0" => 0,
        "1" => 1,
        _ => return None,
    };
    Some((live, ts.parse().ok()?, id.to_string()))
}

fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('%');
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// Turn user input into a safe FTS5 query: every term is quoted (so FTS
/// syntax characters are literal), terms are ANDed, the last one is a prefix.
pub fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        return None;
    }
    Some(format!("{}*", terms.join(" ")))
}

impl Store {
    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        self.read(|c| {
            one(
                c,
                "SELECT * FROM sessions WHERE id = ?1",
                params![id],
                session_row,
            )
        })
    }

    pub fn insert_session(&self, s: &Session) -> Result<()> {
        self.apply(Change::Session(s.clone())).map(|_| ())
    }

    /// Insert `s` unless a session with its `(agent, agent_session_id)`
    /// already exists (ingest may create it between a caller's lookup and
    /// this insert), in one write transaction. Returns the stored session
    /// and whether `s` was inserted.
    pub fn insert_session_unless_known(&self, s: &Session) -> Result<(Session, bool)> {
        self.write(|tx| {
            if let Some(asid) = &s.agent_session_id
                && let Some(existing) = one(
                    tx,
                    "SELECT * FROM sessions WHERE agent = ?1 AND agent_session_id = ?2",
                    params![s.agent, asid],
                    session_row,
                )?
            {
                return Ok((existing, false));
            }
            apply_in(tx, &Change::Session(s.clone()))?;
            Ok((s.clone(), true))
        })
    }

    /// Read-modify-write a session in one transaction.
    ///
    /// Live status flips (working/idle/waiting and the activity time that
    /// goes with them) are coalesced for replication: when the session's
    /// last outbox entry is younger than [`STATUS_COALESCE_MS`], the row is
    /// written but its outbox entry is deferred until that window has passed
    /// ([`Store::flush_deferred`] then queues the current row). Any other
    /// change, including every move to a final status, is queued at once and
    /// covers a deferred one, so the final state is never lost.
    pub fn modify_session(&self, id: &str, f: impl FnOnce(&mut Session)) -> Result<Session> {
        self.write(|tx| {
            let mut s = one(
                tx,
                "SELECT * FROM sessions WHERE id = ?1",
                params![id],
                session_row,
            )?
            .ok_or(StoreError::NotFound("session"))?;
            let before = s.clone();
            f(&mut s);
            if s == before {
                return Ok(s);
            }
            let change = Change::Session(s.clone());
            if status_only(&before, &s) {
                let last: Option<i64> = tx.query_row(
                    "SELECT max(ts) FROM outbox WHERE entity = 'sessions' AND key = ?1",
                    params![id],
                    |r| r.get(0),
                )?;
                if let Some(last) = last.filter(|t| crate::now_ms() - t < STATUS_COALESCE_MS) {
                    write_row(tx, &change)?;
                    tx.execute(
                        "INSERT INTO outbox_deferred(entity, key, due) VALUES ('sessions', ?1, ?2)
                         ON CONFLICT(entity, key) DO NOTHING",
                        params![id, last + STATUS_COALESCE_MS],
                    )?;
                    return Ok(s);
                }
            }
            apply_in(tx, &change)?;
            Ok(s)
        })
    }

    /// This machine's sessions that have a worktree.
    pub fn sessions_with_worktree(&self, machine_id: &str) -> Result<Vec<Session>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM sessions WHERE machine_id = ?1 AND worktree IS NOT NULL
                 ORDER BY started_at DESC, id DESC",
                params![machine_id],
                session_row,
            )
        })
    }

    /// Delete a session that is not running (see [`Change::DeleteSession`]).
    /// Returns the deleted session.
    pub fn delete_session(&self, id: &str) -> Result<Session> {
        self.write(|tx| {
            let s = one(
                tx,
                "SELECT * FROM sessions WHERE id = ?1",
                params![id],
                session_row,
            )?
            .ok_or(StoreError::NotFound("session"))?;
            if s.status.is_live() {
                return Err(StoreError::Conflict(
                    "the session is running; stop it first".into(),
                ));
            }
            apply_in(tx, &Change::DeleteSession { id: id.to_string() })?;
            Ok(s)
        })
    }

    /// Queue the current row of every coalesced session write due by `now`
    /// (the daemon's status tick calls this). Returns how many were queued.
    pub fn flush_deferred(&self, now: i64) -> Result<usize> {
        self.write(|tx| {
            let due: Vec<String> = all(
                tx,
                "SELECT key FROM outbox_deferred WHERE entity = 'sessions' AND due <= ?1",
                params![now],
                |r| r.get(0),
            )?;
            let mut queued = 0;
            for id in &due {
                match one(
                    tx,
                    "SELECT * FROM sessions WHERE id = ?1",
                    params![id],
                    session_row,
                )? {
                    Some(s) => {
                        apply_in(tx, &Change::Session(s))?;
                        queued += 1;
                    }
                    None => {
                        tx.execute(
                            "DELETE FROM outbox_deferred WHERE entity = 'sessions' AND key = ?1",
                            params![id],
                        )?;
                    }
                }
            }
            Ok(queued)
        })
    }

    /// Sessions on `machine_id` whose status says a process is attached.
    pub fn live_sessions_on(&self, machine_id: &str) -> Result<Vec<Session>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM sessions WHERE machine_id = ?1
                   AND status IN ('starting','working','idle','waiting')",
                params![machine_id],
                session_row,
            )
        })
    }

    /// Newest sessions of a project, without ingested subagent children
    /// (their parent's session covers them, §8).
    pub fn recent_sessions(&self, project_id: &str, limit: i64) -> Result<Vec<Session>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM sessions WHERE project_id = ?1
                   AND NOT (origin = 'external' AND parent_session_id IS NOT NULL)
                 ORDER BY started_at DESC, id DESC LIMIT ?2",
                params![project_id, limit],
                session_row,
            )
        })
    }

    /// Live sessions first (a process is attached, so the user can act on
    /// them even after hours of idling), then by most recent activity,
    /// keyset-paginated on `(live, last_activity_at, id)`.
    ///
    /// The sort keys change while a client pages (activity, status, a remote
    /// session going stale), so a session can be skipped (it moved ahead of
    /// a cursor already handed out) or returned twice (it moved behind it).
    /// Clients dedupe by id and learn about moved sessions through
    /// `session_updated`.
    pub fn list_sessions(&self, f: &SessionFilter) -> Result<SessionsPage> {
        let mut args: Vec<Value> = vec![
            f.local_machine.clone().map_or(Value::Null, Value::from),
            (crate::now_ms() - REMOTE_LIVE_MS).into(),
            serde_json::to_string(&f.online)
                .map_err(|e| StoreError::Invalid(e.to_string()))?
                .into(),
            serde_json::to_string(&f.offline)
                .map_err(|e| StoreError::Invalid(e.to_string()))?
                .into(),
        ];
        let mut bind = |v: Value| {
            args.push(v);
            format!("?{}", args.len())
        };
        let mut sql = format!("SELECT *, {LIVE} AS live FROM sessions WHERE 1=1");
        if let Some(p) = &f.project_id {
            sql += &format!(" AND project_id = {}", bind(p.clone().into()));
        }
        if let Some(s) = f.status {
            sql += &format!(" AND status = {}", bind(s.as_str().to_string().into()));
        }
        if let Some(a) = &f.agent {
            sql += &format!(" AND agent = {}", bind(a.clone().into()));
        }
        if let Some(m) = &f.machine_id {
            sql += &format!(" AND machine_id = {}", bind(m.clone().into()));
        }
        if let Some(p) = &f.parent {
            sql += &format!(
                " AND {IS_CHILD} AND parent_session_id = {}",
                bind(p.clone().into())
            );
        } else if f.hide_children {
            sql += &format!(" AND NOT {IS_CHILD}");
        }
        if let Some(q) = f.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            let pat = bind(like_escape(q).into());
            sql += &format!(
                " AND (title LIKE {pat} ESCAPE '\\' OR cwd LIKE {pat} ESCAPE '\\'
                   OR branch LIKE {pat} ESCAPE '\\' OR agent LIKE {pat} ESCAPE '\\')"
            );
        }
        if let Some(cursor) = &f.cursor {
            let (live, ts, id) =
                parse_cursor(cursor).ok_or_else(|| StoreError::Invalid("invalid cursor".into()))?;
            let (live, ts, id) = (bind(live.into()), bind(ts.into()), bind(id.into()));
            sql += &format!(
                " AND ({LIVE} < {live} OR ({LIVE} = {live} AND (last_activity_at < {ts}
                   OR (last_activity_at = {ts} AND id < {id}))))"
            );
        }
        let limit = f.limit.clamp(1, 500);
        // Note: no index; scans matching sessions. Add an index on
        // (live, last_activity_at) if lists get slow.
        sql += &format!(
            " ORDER BY live DESC, last_activity_at DESC, id DESC LIMIT {}",
            limit + 1
        );
        let mut rows = self.read(|c| {
            all(c, &sql, params_from_iter(args), |r| {
                Ok((session_row(r)?, r.get::<_, i64>("live")?))
            })
        })?;
        let next_cursor = if rows.len() as i64 > limit {
            rows.truncate(limit as usize);
            rows.last()
                .map(|(s, live)| format!("{live}:{}:{}", s.last_activity_at, s.id))
        } else {
            None
        };
        Ok(SessionsPage {
            items: rows.into_iter().map(|(s, _)| s).collect(),
            next_cursor,
        })
    }

    /// Subagent sessions recorded under `id` (see [`SessionFilter::parent`]).
    pub fn children_count(&self, id: &str) -> Result<i64> {
        self.read(|c| {
            Ok(c.query_row(
                &format!(
                    "SELECT count(*) FROM sessions WHERE {IS_CHILD} AND parent_session_id = ?1"
                ),
                params![id],
                |r| r.get(0),
            )?)
        })
    }

    pub fn insert_event(&self, e: Event) -> Result<bool> {
        self.apply(Change::Event(e))
    }

    /// Events with `seq > after`, ascending. Returns (items, next_after).
    pub fn events_page(
        &self,
        session_id: &str,
        after: i64,
        limit: i64,
    ) -> Result<(Vec<Event>, Option<i64>)> {
        let limit = limit.clamp(1, 1000);
        let mut items = self.read(|c| {
            all(
                c,
                "SELECT * FROM events WHERE session_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
                params![session_id, after, limit + 1],
                event_row,
            )
        })?;
        let next = if items.len() as i64 > limit {
            items.truncate(limit as usize);
            items.last().map(|e| e.seq)
        } else {
            None
        };
        Ok((items, next))
    }

    /// FTS5 search over events and records, ranked by bm25 (lower is better).
    pub fn search(
        &self,
        query: &str,
        project_id: Option<&str>,
        kind: Option<SearchHitKind>,
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        let Some(q) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let limit = limit.clamp(1, 200);
        let mut hits = self.read(|c| {
            let mut hits = Vec::new();
            if kind != Some(SearchHitKind::Record) {
                hits.extend(all(
                    c,
                    "SELECT e.session_id, e.seq, e.ts, s.project_id, s.title, s.agent,
                       snippet(events_fts, 0, char(2), char(3), '…', 16) AS snip,
                       bm25(events_fts) AS score
                     FROM events_fts
                     JOIN events e ON e.rowid = events_fts.rowid
                     JOIN sessions s ON s.id = e.session_id
                     WHERE events_fts MATCH ?1 AND (?2 IS NULL OR s.project_id = ?2)
                     ORDER BY score LIMIT ?3",
                    params![q, project_id, limit],
                    |r| {
                        Ok(SearchHit {
                            kind: SearchHitKind::Event,
                            session_id: Some(r.get(0)?),
                            seq: Some(r.get(1)?),
                            ts: r.get(2)?,
                            project_id: r.get(3)?,
                            title: r.get(4)?,
                            agent: Some(r.get(5)?),
                            record_id: None,
                            snippet: r.get(6)?,
                            score: r.get(7)?,
                        })
                    },
                )?);
            }
            if kind != Some(SearchHitKind::Event) {
                hits.extend(all(
                    c,
                    "SELECT r.id, r.project_id, r.title, r.updated_at, r.source_session_id,
                       snippet(records_fts, -1, char(2), char(3), '…', 16) AS snip,
                       bm25(records_fts, 2.0, 1.0) AS score
                     FROM records_fts
                     JOIN records r ON r.rowid = records_fts.rowid
                     WHERE records_fts MATCH ?1 AND (?2 IS NULL OR r.project_id = ?2)
                     ORDER BY score LIMIT ?3",
                    params![q, project_id, limit],
                    |r| {
                        Ok(SearchHit {
                            kind: SearchHitKind::Record,
                            record_id: Some(r.get(0)?),
                            project_id: r.get(1)?,
                            title: Some(r.get(2)?),
                            ts: r.get(3)?,
                            session_id: r.get(4)?,
                            seq: None,
                            agent: None,
                            snippet: r.get(5)?,
                            score: r.get(6)?,
                        })
                    },
                )?);
            }
            Ok(hits)
        })?;
        hits.sort_by(|a, b| a.score.total_cmp(&b.score));
        hits.truncate(limit as usize);
        Ok(hits)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::tests::temp_store;
    use super::*;
    use crate::model::{EventKind, Record, RecordKind, RecordStatus, SessionOrigin};

    #[test]
    fn insert_unless_known_never_violates_the_agent_id() {
        let (_d, store) = temp_store();
        let first = Session {
            agent_session_id: Some("a1".into()),
            ..session("s1", "p", 1)
        };
        store.insert_session(&first).unwrap();
        // What a hook did when ingest created the row after its lookup.
        let racing = Session {
            agent_session_id: Some("a1".into()),
            ..session("s2", "p", 2)
        };
        assert!(store.insert_session(&racing).is_err());
        let (got, inserted) = store.insert_session_unless_known(&racing).unwrap();
        assert_eq!((got.id.as_str(), inserted), ("s1", false));
        let (got, inserted) = store
            .insert_session_unless_known(&session("s3", "p", 3))
            .unwrap();
        assert_eq!((got.id.as_str(), inserted), ("s3", true));
    }

    pub(crate) fn session(id: &str, project: &str, started_at: i64) -> Session {
        Session {
            id: id.into(),
            project_id: project.into(),
            machine_id: "m".into(),
            agent: "shell".into(),
            agent_session_id: None,
            origin: SessionOrigin::Blirp,
            cwd: "/tmp/x".into(),
            title: Some(format!("title {id}")),
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
    fn list_sessions_paginates_and_filters() {
        let (_d, store) = temp_store();
        for i in 0..5 {
            store
                .insert_session(&session(&format!("s{i}"), "p", 100 + i))
                .unwrap();
        }
        store.insert_session(&session("other", "q", 50)).unwrap();
        let mut f = SessionFilter {
            project_id: Some("p".into()),
            limit: 2,
            ..Default::default()
        };
        let p1 = store.list_sessions(&f).unwrap();
        assert_eq!(
            p1.items.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s4", "s3"]
        );
        f.cursor = p1.next_cursor;
        let p2 = store.list_sessions(&f).unwrap();
        assert_eq!(
            p2.items.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["s2", "s1"]
        );
        f.cursor = p2.next_cursor;
        let p3 = store.list_sessions(&f).unwrap();
        assert_eq!(p3.items.len(), 1);
        assert!(p3.next_cursor.is_none());

        let q = store
            .list_sessions(&SessionFilter {
                q: Some("title s3".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(q.items.len(), 1);
        let pct = store
            .list_sessions(&SessionFilter {
                q: Some("%".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert!(pct.items.is_empty());
        assert!(
            store
                .list_sessions(&SessionFilter {
                    cursor: Some("junk".into()),
                    limit: 10,
                    ..Default::default()
                })
                .is_err()
        );

        let m = store
            .modify_session("s1", |s| s.status = SessionStatus::Completed)
            .unwrap();
        assert_eq!(m.status, SessionStatus::Completed);
        assert_eq!(store.live_sessions_on("m").unwrap().len(), 5);
    }

    #[test]
    fn list_sessions_pins_live_then_orders_by_activity_across_pages() {
        let (_d, store) = temp_store();
        // (id, machine, started_at, last_activity_at, status)
        let rows = [
            ("old-live", "pc", 10, 20, SessionStatus::Idle),
            ("new-ended", "mac", 900, 950, SessionStatus::Completed),
            ("long-running", "pc", 100, 1000, SessionStatus::Completed),
            ("tie-a", "mac", 300, 500, SessionStatus::Completed),
            ("tie-b", "pc", 400, 500, SessionStatus::Failed),
            ("working", "mac", 50, 60, SessionStatus::Working),
            ("oldest", "pc", 1, 2, SessionStatus::Detached),
        ];
        for (id, machine, started, active, status) in rows {
            let mut s = session(id, "p", started);
            s.machine_id = machine.into();
            s.last_activity_at = active;
            s.status = status;
            store.insert_session(&s).unwrap();
        }
        let expected = [
            "working",
            "old-live",
            "long-running",
            "new-ended",
            "tie-b",
            "tie-a",
            "oldest",
        ];
        let all_pages = |f: SessionFilter| -> Vec<String> {
            let mut f = f;
            let mut out = Vec::new();
            loop {
                let page = store.list_sessions(&f).unwrap();
                out.extend(page.items.into_iter().map(|s| s.id));
                match page.next_cursor {
                    Some(c) => f.cursor = Some(c),
                    None => return out,
                }
            }
        };
        for limit in [1, 2, 3, 50] {
            assert_eq!(
                all_pages(SessionFilter {
                    limit,
                    ..Default::default()
                }),
                expected,
                "limit {limit}"
            );
        }
        // Every machine's sessions are listed; `machine` narrows to one.
        assert_eq!(
            all_pages(SessionFilter {
                machine_id: Some("pc".into()),
                limit: 2,
                ..Default::default()
            }),
            ["old-live", "long-running", "tie-b", "oldest"]
        );
        // Seen from "pc": its own idle session stays pinned however old, the
        // other machine's "working" session without an update for longer
        // than REMOTE_LIVE_MS sorts by its activity (its machine may be gone),
        // and a fresh one of that machine is pinned.
        let mut fresh = session("mac-fresh", "p", 5);
        fresh.machine_id = "mac".into();
        fresh.last_activity_at = crate::now_ms() - REMOTE_LIVE_MS + 60_000;
        store.insert_session(&fresh).unwrap();
        for limit in [1, 3, 50] {
            assert_eq!(
                all_pages(SessionFilter {
                    local_machine: Some("pc".into()),
                    limit,
                    ..Default::default()
                }),
                [
                    "mac-fresh",
                    "old-live",
                    "long-running",
                    "new-ended",
                    "tie-b",
                    "tie-a",
                    "working",
                    "oldest",
                ],
                "limit {limit}"
            );
        }
        // Known presence wins over the activity rule: "mac" offline unpins
        // even its fresh session; "mac" online pins its old "working" one.
        let order = |online: &[&str], offline: &[&str]| {
            all_pages(SessionFilter {
                local_machine: Some("pc".into()),
                online: online.iter().map(|s| s.to_string()).collect(),
                offline: offline.iter().map(|s| s.to_string()).collect(),
                limit: 2,
                ..Default::default()
            })
        };
        assert_eq!(
            order(&[], &["mac"]),
            [
                "old-live",
                "mac-fresh",
                "long-running",
                "new-ended",
                "tie-b",
                "tie-a",
                "working",
                "oldest",
            ]
        );
        assert_eq!(
            order(&["mac"], &[]),
            [
                "mac-fresh",
                "working",
                "old-live",
                "long-running",
                "new-ended",
                "tie-b",
                "tie-a",
                "oldest",
            ]
        );
        // Search also matches the branch.
        store
            .modify_session("tie-a", |s| s.branch = Some("feature/zebra".into()))
            .unwrap();
        assert_eq!(
            all_pages(SessionFilter {
                q: Some("ZEBRA".into()),
                limit: 10,
                ..Default::default()
            }),
            ["tie-a"]
        );
        // Cursors of the old `started_at:id` form are rejected, not misread.
        for bad in ["100:s1", "2:1:x", "1:x:y", ""] {
            assert!(
                store
                    .list_sessions(&SessionFilter {
                        cursor: Some(bad.into()),
                        limit: 10,
                        ..Default::default()
                    })
                    .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn subagent_children_are_filtered_and_counted() {
        let (_d, store) = temp_store();
        store.insert_session(&session("top", "p", 100)).unwrap();
        let mut fork = session("fork", "p", 101);
        fork.parent_session_id = Some("top".into());
        store.insert_session(&fork).unwrap();
        for (i, id) in ["sub1", "sub2"].iter().enumerate() {
            let mut sub = session(id, "p", 102 + i as i64);
            sub.origin = SessionOrigin::External;
            sub.parent_session_id = Some("top".into());
            store.insert_session(&sub).unwrap();
        }
        let ids = |f: SessionFilter| -> Vec<String> {
            store
                .list_sessions(&SessionFilter { limit: 50, ..f })
                .unwrap()
                .items
                .into_iter()
                .map(|s| s.id)
                .collect()
        };
        assert_eq!(
            ids(SessionFilter::default()),
            ["sub2", "sub1", "fork", "top"]
        );
        // Forks keep showing: only ingested subagents are children.
        assert_eq!(
            ids(SessionFilter {
                hide_children: true,
                ..Default::default()
            }),
            ["fork", "top"]
        );
        assert_eq!(
            ids(SessionFilter {
                parent: Some("top".into()),
                hide_children: true,
                ..Default::default()
            }),
            ["sub2", "sub1"]
        );
        assert_eq!(store.children_count("top").unwrap(), 2);
        assert_eq!(store.children_count("sub1").unwrap(), 0);
    }

    #[test]
    fn status_flips_are_coalesced_without_losing_the_final_state() {
        let (_d, store) = temp_store();
        let queued = |store: &Store| -> Vec<Session> {
            store
                .outbox_after(0, 1000)
                .unwrap()
                .into_iter()
                .filter(|e| e.entity == "sessions")
                .map(|e| serde_json::from_value(e.payload["row"].clone()).unwrap())
                .collect()
        };
        store.insert_session(&session("s", "p", 100)).unwrap();
        assert_eq!(queued(&store).len(), 1);

        // Flips right after the insert: the row changes, the outbox does not.
        for st in [
            SessionStatus::Idle,
            SessionStatus::Working,
            SessionStatus::Idle,
        ] {
            let s = store
                .modify_session("s", |s| {
                    s.status = st;
                    s.last_activity_at += 1;
                })
                .unwrap();
            assert_eq!(s.status, st);
        }
        assert_eq!(queued(&store).len(), 1);
        assert_eq!(
            store.get_session("s").unwrap().unwrap().status,
            SessionStatus::Idle
        );
        // Not due yet; once due, the current row is queued exactly once.
        assert_eq!(store.flush_deferred(crate::now_ms()).unwrap(), 0);
        let later = crate::now_ms() + STATUS_COALESCE_MS;
        assert_eq!(store.flush_deferred(later).unwrap(), 1);
        assert_eq!(store.flush_deferred(later).unwrap(), 0);
        let q = queued(&store);
        assert_eq!(q.len(), 2);
        assert_eq!(q[1].status, SessionStatus::Idle);

        // A final status is queued at once and covers a deferred flip.
        store
            .modify_session("s", |s| s.status = SessionStatus::Working)
            .unwrap();
        assert_eq!(queued(&store).len(), 2);
        store
            .modify_session("s", |s| s.status = SessionStatus::Completed)
            .unwrap();
        let q = queued(&store);
        assert_eq!(q.len(), 3);
        assert_eq!(q[2].status, SessionStatus::Completed);
        assert_eq!(store.flush_deferred(i64::MAX).unwrap(), 0);

        // Changes other than the status are never deferred.
        store
            .modify_session("s", |s| s.title = Some("renamed".into()))
            .unwrap();
        assert_eq!(queued(&store).len(), 4);
    }

    #[test]
    fn deleting_a_session_takes_its_events_and_subagents() {
        let (_d, store) = temp_store();
        let mut top = session("top", "p", 100);
        top.status = SessionStatus::Completed;
        store.insert_session(&top).unwrap();
        let mut sub = session("sub", "p", 101);
        sub.origin = SessionOrigin::External;
        sub.parent_session_id = Some("top".into());
        store.insert_session(&sub).unwrap();
        let mut fork = session("fork", "p", 102);
        fork.parent_session_id = Some("top".into());
        store.insert_session(&fork).unwrap();
        for (sid, seq) in [("top", 1), ("sub", 1), ("fork", 1)] {
            store
                .insert_event(Event {
                    session_id: sid.into(),
                    seq,
                    ts: 1,
                    kind: EventKind::User,
                    text: format!("zebra crossing {sid}"),
                    meta: None,
                })
                .unwrap();
        }
        let rec = Record {
            id: "r1".into(),
            project_id: "p".into(),
            kind: RecordKind::Decision,
            title: "keep".into(),
            body: "".into(),
            status: RecordStatus::Active,
            pinned: false,
            source_session_id: Some("top".into()),
            created_at: 1,
            updated_at: 1,
            updated_by: "distiller".into(),
        };
        store.apply(Change::Record(rec)).unwrap();

        // A running session cannot be deleted.
        assert!(matches!(
            store.delete_session("fork"),
            Err(StoreError::Conflict(_))
        ));
        store.delete_session("top").unwrap();
        assert!(store.get_session("top").unwrap().is_none());
        assert!(store.get_session("sub").unwrap().is_none());
        let fork = store.get_session("fork").unwrap().unwrap();
        assert_eq!(fork.parent_session_id, None);
        let hits = store.search("zebra", None, None, 10).unwrap();
        let sessions: Vec<_> = hits
            .iter()
            .filter_map(|h| h.session_id.as_deref())
            .collect();
        assert_eq!(
            sessions,
            ["fork"],
            "full-text rows of deleted events remain"
        );
        let r = store.get_record("r1").unwrap().unwrap();
        assert_eq!(r.source_session_id, None);
        let last = store.outbox_after(0, 100).unwrap().pop().unwrap();
        assert_eq!(
            (last.entity.as_str(), last.op.as_str(), last.key.as_str()),
            ("sessions", "delete", "top")
        );
        assert!(matches!(
            store.delete_session("top"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn events_page_and_search() {
        let (_d, store) = temp_store();
        store.insert_session(&session("s", "p", 1)).unwrap();
        for (seq, text) in [
            (1, "refactor the parser module"),
            (2, "run the tests"),
            (3, "parsing works now"),
        ] {
            store
                .insert_event(Event {
                    session_id: "s".into(),
                    seq,
                    ts: seq,
                    kind: EventKind::Assistant,
                    text: text.into(),
                    meta: None,
                })
                .unwrap();
        }
        let (items, next) = store.events_page("s", 0, 2).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(next, Some(2));
        let (rest, next) = store.events_page("s", 2, 2).unwrap();
        assert_eq!((rest.len(), next), (1, None));

        let now = crate::now_ms();
        store
            .create_record(Record {
                id: "r1".into(),
                project_id: "p".into(),
                kind: RecordKind::Gotcha,
                title: "Parser quirk".into(),
                body: "The parser needs a trailing newline".into(),
                status: RecordStatus::Active,
                pinned: false,
                source_session_id: None,
                created_at: now,
                updated_at: now,
                updated_by: "user".into(),
            })
            .unwrap();

        // Porter stemming: "parse" matches parser/parsing.
        let hits = store.search("parse", None, None, 10).unwrap();
        assert!(
            hits.iter().any(|h| h.kind == SearchHitKind::Record),
            "{hits:?}"
        );
        assert!(
            hits.iter()
                .filter(|h| h.kind == SearchHitKind::Event)
                .count()
                >= 2,
            "{hits:?}"
        );
        assert!(hits.iter().any(|h| h.snippet.contains('\u{2}')));
        let only_records = store
            .search("parser", Some("p"), Some(SearchHitKind::Record), 10)
            .unwrap();
        assert!(only_records.iter().all(|h| h.kind == SearchHitKind::Record));
        assert!(
            store
                .search("parser", Some("nope"), None, 10)
                .unwrap()
                .is_empty()
        );
        // FTS syntax in user input is treated literally, never an error.
        assert!(
            store
                .search("\"unbalanced AND OR ( *", None, None, 10)
                .is_ok()
        );
        assert!(store.search("   ", None, None, 10).unwrap().is_empty());

        // Updating a record re-indexes it.
        store
            .modify_record("r1", |r| r.title = "Lexer quirk".into())
            .unwrap();
        assert!(
            store
                .search("lexer", None, None, 10)
                .unwrap()
                .iter()
                .any(|h| h.record_id.as_deref() == Some("r1"))
        );
    }
}
