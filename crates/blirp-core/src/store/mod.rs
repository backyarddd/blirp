//! SQLite store (§5). One writer connection behind a mutex, a small pool of
//! read-only connections for readers (WAL lets them run concurrently).
//!
//! Every write to a replicated table goes through [`Store::apply`] /
//! [`Store::apply_all`], which write the row and append to `outbox` in the
//! same transaction. Typed helpers for non-replicated tables write directly.

mod engine;
mod ingest;
mod memory;
mod migrations;
mod misc;
mod projects;
mod sessions;
mod sync;

pub use engine::{
    BY_DISTILLER, BriefApply, DistillEvents, DistillOutcome, DistillPlan, MACHINE_ID_KEY,
    norm_title,
};
pub use ingest::IngestTx;
pub use memory::RecordFilter;
pub use migrations::MigrationError;
pub use projects::{NonProjectDirs, ResolvedProject};
pub use sessions::SessionFilter;
pub use sync::{Compacted, HubPage, IngestOutcome, PulledEntry, SyncCursors, WireEntry};

use crate::model::{
    Brief, Event, Machine, Project, ProjectPath, Record, Resource, Session, WikiPage,
};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Migration(#[from] MigrationError),
    #[error("invalid JSON in database: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// A replicated write. Serialized form is the `outbox` payload.
// Values are built and consumed immediately, never stored in bulk, so boxing
// the large `Session` variant would only add noise.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "entity", content = "row", rename_all = "snake_case")]
pub enum Change {
    Machine(Machine),
    /// Drops a machine row; only emitted when a pre-identity machine id is
    /// replaced by the iroh endpoint id (see [`Store::rebind_machine`]).
    DeleteMachine {
        id: String,
    },
    Project(Project),
    ProjectPath(ProjectPath),
    DeleteProjectPath {
        machine_id: String,
        path: String,
    },
    Session(Session),
    /// Deletes a session with its events (and their full-text rows) and its
    /// ingested subagent sessions; records and suggestions it produced stay
    /// with `source_session_id` cleared, and continue/fork sessions lose
    /// their parent link.
    DeleteSession {
        id: String,
    },
    /// Append-only; a duplicate `(session_id, seq)` is ignored.
    Event(Event),
    Record(Record),
    DeleteRecord {
        id: String,
    },
    /// Writes the current brief and appends its `brief_history` row (history
    /// rows are insert-only, keyed by `Brief::id`).
    Brief(Brief),
    WikiPage(WikiPage),
    Resource(Resource),
}

impl Change {
    /// (table, op, primary key)
    fn describe(&self) -> (&'static str, &'static str, String) {
        match self {
            Change::Machine(m) => ("machines", "upsert", m.id.clone()),
            Change::DeleteMachine { id } => ("machines", "delete", id.clone()),
            Change::Project(p) => ("projects", "upsert", p.id.clone()),
            Change::ProjectPath(p) => (
                "project_paths",
                "upsert",
                format!("{}\n{}", p.machine_id, p.path),
            ),
            Change::DeleteProjectPath { machine_id, path } => {
                ("project_paths", "delete", format!("{machine_id}\n{path}"))
            }
            Change::Session(s) => ("sessions", "upsert", s.id.clone()),
            Change::DeleteSession { id } => ("sessions", "delete", id.clone()),
            Change::Event(e) => ("events", "insert", format!("{}\n{}", e.session_id, e.seq)),
            Change::Record(r) => ("records", "upsert", r.id.clone()),
            Change::DeleteRecord { id } => ("records", "delete", id.clone()),
            Change::Brief(b) => ("briefs", "upsert", b.project_id.clone()),
            Change::WikiPage(w) => ("wiki_pages", "upsert", w.id.clone()),
            Change::Resource(r) => ("resources", "upsert", r.id.clone()),
        }
    }
}

/// One row of `outbox`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub origin_seq: i64,
    pub entity: String,
    pub op: String,
    pub key: String,
    pub payload: JsonValue,
    pub ts: i64,
}

const READER_POOL: usize = 4;

pub struct Store {
    path: PathBuf,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
    /// A node pull reached the hub's head since this store was opened.
    pulled_to_head: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("path", &self.path).finish()
    }
}

/// A poisoned lock only means another thread panicked mid-call; SQLite rolls
/// back its open transaction when the `Transaction` is dropped, so the
/// connection is still consistent and safe to reuse.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Store {
    /// Open (creating if needed) the database and run migrations.
    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(%mode, "SQLite did not enable WAL mode");
        }
        conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
        migrations::migrate(&mut conn)?;
        Ok(Self {
            path: path.to_path_buf(),
            writer: Mutex::new(conn),
            readers: Mutex::new(Vec::new()),
            pulled_to_head: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.read(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?))
    }

    /// `PRAGMA quick_check`: "ok" when the database is healthy.
    pub fn quick_check(&self) -> Result<String> {
        self.read(|c| Ok(c.query_row("PRAGMA quick_check", [], |r| r.get(0))?))
    }

    /// Write a consistent, compacted copy of the database to `dest` with
    /// `VACUUM INTO`, which reads one snapshot and does not block the
    /// daemon's writers (WAL). `dest` must not exist or be an empty file;
    /// the caller creates it empty to choose its permissions.
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        let dest = dest.to_str().ok_or_else(|| {
            StoreError::Invalid(format!("backup path is not UTF-8: {}", dest.display()))
        })?;
        self.read(|c| {
            c.execute("VACUUM INTO ?1", [dest])?;
            Ok(())
        })
    }

    /// Run `f` on a pooled read-only connection.
    pub(crate) fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let pooled = lock(&self.readers).pop();
        let conn = match pooled {
            Some(c) => c,
            None => {
                let c = Connection::open_with_flags(
                    &self.path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                c.busy_timeout(Duration::from_secs(5))?;
                c
            }
        };
        let out = f(&conn);
        let mut pool = lock(&self.readers);
        if pool.len() < READER_POOL {
            pool.push(conn);
        }
        out
    }

    /// Run `f` in an immediate transaction on the writer connection.
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = lock(&self.writer);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Apply one replicated change (row + outbox) atomically. Returns false
    /// when the change was a no-op (duplicate event).
    pub fn apply(&self, change: Change) -> Result<bool> {
        self.write(|tx| apply_in(tx, &change))
    }

    /// Apply several replicated changes in one transaction.
    pub fn apply_all(&self, changes: &[Change]) -> Result<()> {
        self.write(|tx| {
            for c in changes {
                apply_in(tx, c)?;
            }
            Ok(())
        })
    }

    /// Outbox entries after `after_seq`, oldest first.
    pub fn outbox_after(&self, after_seq: i64, limit: i64) -> Result<Vec<OutboxEntry>> {
        self.read(|c| {
            let mut st = c.prepare(
                "SELECT origin_seq, entity, op, key, payload_json, ts FROM outbox
                 WHERE origin_seq > ?1 ORDER BY origin_seq LIMIT ?2",
            )?;
            let rows = st.query_map(params![after_seq, limit], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get::<_, String>(4)?,
                    r.get(5)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (origin_seq, entity, op, key, payload, ts) = row?;
                out.push(OutboxEntry {
                    origin_seq,
                    entity,
                    op,
                    key,
                    payload: serde_json::from_str(&payload)?,
                    ts,
                });
            }
            Ok(out)
        })
    }
}

fn json_text(v: &Option<JsonValue>) -> Option<String> {
    v.as_ref().map(JsonValue::to_string)
}

/// Parse an optional JSON column inside a row mapper.
fn json_col(row: &rusqlite::Row<'_>, col: &str) -> rusqlite::Result<Option<JsonValue>> {
    let raw: Option<String> = row.get(col)?;
    raw.map(|s| {
        serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
    })
    .transpose()
}

/// Memory written on this machine (records, briefs, wiki pages, resources
/// from users, MCP clients or the distiller) is redacted before it is stored
/// and replicated (§9). `None` when nothing needed redacting.
fn redact_memory(change: &Change) -> Option<Change> {
    use crate::redact::{redact, redact_json};
    use std::borrow::Cow;
    let red = |s: &str| match redact(s) {
        Cow::Borrowed(_) => None,
        Cow::Owned(o) => Some(o),
    };
    let mut c = change.clone();
    let mut changed = false;
    let mut fix = |field: &mut String| {
        if let Some(r) = red(field) {
            *field = r;
            changed = true;
        }
    };
    match &mut c {
        Change::Record(r) => {
            fix(&mut r.title);
            fix(&mut r.body);
        }
        Change::Brief(b) => fix(&mut b.body_md),
        Change::WikiPage(w) => {
            fix(&mut w.title);
            fix(&mut w.body_md);
        }
        Change::Resource(r) => {
            fix(&mut r.title);
            fix(&mut r.url);
            if let Some(m) = &mut r.meta {
                let before = m.clone();
                redact_json(m);
                changed |= *m != before;
            }
        }
        _ => return None,
    }
    changed.then_some(c)
}

/// This machine's id as the daemon recorded it ("" before the first start).
pub(crate) fn local_machine_in(c: &Connection) -> Result<String> {
    let v: Option<String> = one(
        c,
        "SELECT value_json FROM settings WHERE key = ?1",
        params![MACHINE_ID_KEY],
        |r| r.get(0),
    )?;
    Ok(v.and_then(|s| serde_json::from_str::<String>(&s).ok())
        .unwrap_or_default())
}

/// Append `change` to the outbox (unless replication is off, see
/// [`Store::set_replication`]).
pub(crate) fn queue_in(tx: &Transaction<'_>, change: &Change) -> Result<()> {
    if sync::outbox_off(tx)? {
        return Ok(());
    }
    let (entity, op, key) = change.describe();
    tx.execute(
        "INSERT INTO outbox(entity, op, key, payload_json, ts) VALUES (?1,?2,?3,?4,?5)",
        params![
            entity,
            op,
            key,
            serde_json::to_string(change)?,
            crate::now_ms()
        ],
    )?;
    Ok(())
}

/// A local edit replaces the version this machine has, so it must also be
/// newer than that version where shared rows converge on the newest
/// `updated_at` (§10); a clock behind the replicated copy is moved past it.
fn stamp_after_stored(tx: &Transaction<'_>, change: &mut Cow<'_, Change>) -> Result<()> {
    let (table, id, at) = match change.as_ref() {
        Change::Project(p) => ("projects", p.id.clone(), p.updated_at),
        Change::Record(r) => ("records", r.id.clone(), r.updated_at),
        Change::WikiPage(w) => ("wiki_pages", w.id.clone(), w.updated_at),
        Change::Resource(r) => ("resources", r.id.clone(), r.updated_at),
        _ => return Ok(()),
    };
    let stored: Option<i64> = one(
        tx,
        &format!("SELECT updated_at FROM {table} WHERE id = ?1"),
        params![id],
        |r| r.get(0),
    )?;
    if let Some(stored) = stored.filter(|s| *s >= at) {
        match change.to_mut() {
            Change::Project(p) => p.updated_at = stored + 1,
            Change::Record(r) => r.updated_at = stored + 1,
            Change::WikiPage(w) => w.updated_at = stored + 1,
            Change::Resource(r) => r.updated_at = stored + 1,
            _ => {}
        }
    }
    Ok(())
}

/// Write the row(s) for `change` and append it to the outbox.
pub(crate) fn apply_in(tx: &Transaction<'_>, change: &Change) -> Result<bool> {
    let mut change = match redact_memory(change) {
        Some(redacted) => Cow::Owned(redacted),
        None => Cow::Borrowed(change),
    };
    stamp_after_stored(tx, &mut change)?;
    let change = change.as_ref();
    if let Change::Session(s) = change {
        // A session filed under a project that was removed (or merged into
        // Home by `retire_non_projects`) after the caller resolved it: fail
        // so the caller resolves again. Sessions already in a removed
        // project stay writable.
        let into_deleted: Option<i64> = one(
            tx,
            "SELECT 1 FROM projects p WHERE p.id = ?1 AND p.deleted = 1
               AND NOT EXISTS (SELECT 1 FROM sessions WHERE id = ?2 AND project_id = ?1)",
            params![s.project_id, s.id],
            |r| r.get(0),
        )?;
        if into_deleted.is_some() {
            return Err(StoreError::Conflict(format!(
                "project {} was removed",
                s.project_id
            )));
        }
    }
    let written = write_row(tx, change)?;
    if written == 0
        && matches!(
            change,
            Change::Event(_) | Change::Session(_) | Change::Record(_)
        )
    {
        // A duplicate event, or a write of a deleted session or record:
        // nothing to queue.
        return Ok(false);
    }
    if let Change::Session(s) = change {
        // The full row is queued now; a deferred status write is covered.
        tx.execute(
            "DELETE FROM outbox_deferred WHERE entity = 'sessions' AND key = ?1",
            params![s.id],
        )?;
    }
    queue_in(tx, change)?;
    Ok(true)
}

/// Every id a change carries must have the shape of a blirp id
/// ([`crate::is_safe_id`]): ids name files and arrive from other machines.
pub(crate) fn check_ids(change: &Change) -> Result<()> {
    let ids: Vec<Option<&str>> = match change {
        Change::Machine(m) => vec![Some(&m.id)],
        Change::DeleteMachine { id }
        | Change::DeleteSession { id }
        | Change::DeleteRecord { id } => vec![Some(id)],
        Change::Project(p) => vec![Some(&p.id)],
        Change::ProjectPath(p) => vec![Some(&p.project_id), Some(&p.machine_id)],
        Change::DeleteProjectPath { machine_id, .. } => vec![Some(machine_id)],
        Change::Session(s) => vec![
            Some(&s.id),
            Some(&s.project_id),
            Some(&s.machine_id),
            s.parent_session_id.as_deref(),
        ],
        Change::Event(e) => vec![Some(&e.session_id)],
        Change::Record(r) => vec![
            Some(&r.id),
            Some(&r.project_id),
            r.source_session_id.as_deref(),
        ],
        Change::Brief(b) => vec![
            Some(&b.project_id),
            Some(b.id.as_str()).filter(|i| !i.is_empty()),
        ],
        Change::WikiPage(w) => vec![Some(&w.id), Some(&w.project_id)],
        Change::Resource(r) => vec![Some(&r.id), Some(&r.project_id)],
    };
    match ids.into_iter().flatten().find(|id| !crate::is_safe_id(id)) {
        Some(bad) => Err(StoreError::Invalid(format!("invalid id {bad:?}"))),
        None => Ok(()),
    }
}

/// Write the row(s) for `change` only; returns the affected row count.
/// Replication applies received changes with this so they are never
/// queued again (no echo).
///
/// Shared rows (projects, records, wiki pages, resources, the current
/// brief) converge on the newest version wherever and in whatever order the
/// versions arrive: an upsert only replaces a row it is newer than by
/// `updated_at`, ties broken by comparing the remaining columns (a total
/// order, so every machine keeps the same one). A stale copy (a machine
/// re-pairing with what it had, a late replay) never wins (§10).
fn write_row(tx: &Transaction<'_>, change: &Change) -> Result<usize> {
    check_ids(change)?;
    Ok(match change {
        Change::Machine(m) => tx.execute(
            "INSERT INTO machines(id, name, os, role, last_seen, revoked) VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name, os=excluded.os, role=excluded.role,
               last_seen=excluded.last_seen, revoked=excluded.revoked",
            params![m.id, m.name, m.os, m.role, m.last_seen, m.revoked],
        )?,
        Change::DeleteMachine { id } => {
            tx.execute("DELETE FROM machines WHERE id=?1", params![id])?
        }
        Change::Project(p) => {
            let n = tx.execute(
                "INSERT INTO projects(id, name, created_at, updated_at, deleted, chats) VALUES (?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(id) DO UPDATE SET name=excluded.name, created_at=excluded.created_at,
                   updated_at=excluded.updated_at, deleted=excluded.deleted, chats=excluded.chats
                 WHERE (excluded.updated_at, excluded.deleted, excluded.name, excluded.created_at, excluded.chats)
                     > (projects.updated_at, projects.deleted, projects.name, projects.created_at, projects.chats)",
                params![p.id, p.name, p.created_at, p.updated_at, p.deleted, p.chats],
            )?;
            if n > 0 && p.deleted {
                // A deleted project has no folders. Every machine drops them
                // itself when it applies the delete; nobody removes another
                // machine's folder by replication (§10 ownership).
                tx.execute(
                    "DELETE FROM project_paths WHERE project_id = ?1",
                    params![p.id],
                )?;
            }
            n
        }
        Change::ProjectPath(p) => tx.execute(
            "INSERT INTO project_paths(project_id, machine_id, path, git_remote) VALUES (?1,?2,?3,?4)
             ON CONFLICT(machine_id, path) DO UPDATE SET project_id=excluded.project_id,
               git_remote=excluded.git_remote",
            params![p.project_id, p.machine_id, p.path, p.git_remote],
        )?,
        Change::DeleteProjectPath { machine_id, path } => tx.execute(
            "DELETE FROM project_paths WHERE machine_id=?1 AND path=?2",
            params![machine_id, path],
        )?,
        Change::Session(s) if tombstoned(tx, "deleted_sessions", &s.id)? => 0,
        Change::Session(s) => tx.execute(
            "INSERT INTO sessions(id, project_id, machine_id, agent, agent_session_id, origin, cwd, title,
               status, branch, worktree, transcript_path, started_at, ended_at, last_activity_at, exit_code,
               summary_json, distilled_through_seq, tokens_in, tokens_out, cost_usd, parent_session_id,
               stopped_by_user)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)
             ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id, machine_id=excluded.machine_id,
               agent=excluded.agent, agent_session_id=excluded.agent_session_id, origin=excluded.origin,
               cwd=excluded.cwd, title=excluded.title, status=excluded.status, branch=excluded.branch,
               worktree=excluded.worktree, transcript_path=excluded.transcript_path,
               started_at=excluded.started_at, ended_at=excluded.ended_at,
               last_activity_at=excluded.last_activity_at, exit_code=excluded.exit_code,
               summary_json=excluded.summary_json, distilled_through_seq=excluded.distilled_through_seq,
               tokens_in=excluded.tokens_in, tokens_out=excluded.tokens_out, cost_usd=excluded.cost_usd,
               parent_session_id=excluded.parent_session_id, stopped_by_user=excluded.stopped_by_user",
            params![
                s.id, s.project_id, s.machine_id, s.agent, s.agent_session_id, s.origin, s.cwd, s.title,
                s.status, s.branch, s.worktree, s.transcript_path, s.started_at, s.ended_at,
                s.last_activity_at, s.exit_code, json_text(&s.summary), s.distilled_through_seq,
                s.tokens_in, s.tokens_out, s.cost_usd, s.parent_session_id, s.stopped_by_user
            ],
        )?,
        Change::DeleteSession { id } => {
            // The session and its ingested subagents (§8).
            const DOOMED: &str = "SELECT id FROM sessions WHERE id = ?1
                 OR (parent_session_id = ?1 AND origin = 'external')";
            // Tombstones first: nothing may bring these rows back.
            tx.execute(
                &format!(
                    "INSERT OR IGNORE INTO deleted_sessions(id, deleted_at)
                     SELECT id, ?2 FROM ({DOOMED}) UNION SELECT ?1, ?2"
                ),
                params![id, crate::now_ms()],
            )?;
            // The FTS triggers drop the events' full-text rows.
            tx.execute(
                &format!("DELETE FROM events WHERE session_id IN ({DOOMED})"),
                params![id],
            )?;
            for table in ["records", "suggestions"] {
                tx.execute(
                    &format!(
                        "UPDATE {table} SET source_session_id = NULL WHERE source_session_id IN ({DOOMED})"
                    ),
                    params![id],
                )?;
            }
            tx.execute(
                "UPDATE sessions SET parent_session_id = NULL
                 WHERE parent_session_id = ?1 AND origin <> 'external'",
                params![id],
            )?;
            tx.execute(
                &format!(
                    "DELETE FROM outbox_deferred WHERE entity = 'sessions' AND key IN ({DOOMED})"
                ),
                params![id],
            )?;
            tx.execute(
                "DELETE FROM sessions WHERE id = ?1 OR (parent_session_id = ?1 AND origin = 'external')",
                params![id],
            )?
        }
        Change::Event(e) if tombstoned(tx, "deleted_sessions", &e.session_id)? => 0,
        Change::Event(e) => tx.execute(
            "INSERT INTO events(session_id, seq, ts, kind, text, meta_json) VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(session_id, seq) DO NOTHING",
            params![e.session_id, e.seq, e.ts, e.kind, e.text, json_text(&e.meta)],
        )?,
        Change::Record(r) if tombstoned(tx, "deleted_records", &r.id)? => 0,
        Change::Record(r) => tx.execute(
            "INSERT INTO records(id, project_id, kind, title, body, status, pinned, source_session_id,
               created_at, updated_at, updated_by) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id, kind=excluded.kind,
               title=excluded.title, body=excluded.body, status=excluded.status, pinned=excluded.pinned,
               source_session_id=excluded.source_session_id, created_at=excluded.created_at,
               updated_at=excluded.updated_at, updated_by=excluded.updated_by
             WHERE (excluded.updated_at, excluded.updated_by, excluded.status, excluded.pinned,
                    excluded.kind, excluded.title, excluded.body, excluded.project_id,
                    coalesce(excluded.source_session_id, ''), excluded.created_at)
                 > (records.updated_at, records.updated_by, records.status, records.pinned,
                    records.kind, records.title, records.body, records.project_id,
                    coalesce(records.source_session_id, ''), records.created_at)",
            params![
                r.id, r.project_id, r.kind, r.title, r.body, r.status, r.pinned, r.source_session_id,
                r.created_at, r.updated_at, r.updated_by
            ],
        )?,
        Change::DeleteRecord { id } => {
            // Tombstone first: no copy of the record may bring it back.
            tx.execute(
                "INSERT OR IGNORE INTO deleted_records(id, deleted_at) VALUES (?1, ?2)",
                params![id, crate::now_ms()],
            )?;
            tx.execute("DELETE FROM records WHERE id=?1", params![id])?
        }
        Change::Brief(b) => {
            let id = memory::insert_brief_history(tx, b)?;
            // Every version joins the history; the current brief is the
            // newest one (the order version numbers are derived in).
            tx.execute(
                "INSERT INTO briefs(project_id, body_md, version, updated_at, updated_by, history_id, machine_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(project_id) DO UPDATE SET body_md=excluded.body_md, version=excluded.version,
                   updated_at=excluded.updated_at, updated_by=excluded.updated_by,
                   history_id=excluded.history_id, machine_id=excluded.machine_id
                 WHERE (excluded.updated_at, excluded.history_id) > (briefs.updated_at, briefs.history_id)",
                params![b.project_id, b.body_md, b.version, b.updated_at, b.updated_by, id, b.machine_id],
            )?
        }
        Change::WikiPage(w) => tx.execute(
            "INSERT INTO wiki_pages(id, project_id, slug, title, body_md, updated_at, updated_by, deleted)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id, slug=excluded.slug,
               title=excluded.title, body_md=excluded.body_md, updated_at=excluded.updated_at,
               updated_by=excluded.updated_by, deleted=excluded.deleted
             WHERE (excluded.updated_at, excluded.deleted, excluded.updated_by, excluded.title,
                    excluded.body_md, excluded.slug, excluded.project_id)
                 > (wiki_pages.updated_at, wiki_pages.deleted, wiki_pages.updated_by, wiki_pages.title,
                    wiki_pages.body_md, wiki_pages.slug, wiki_pages.project_id)",
            params![w.id, w.project_id, w.slug, w.title, w.body_md, w.updated_at, w.updated_by, w.deleted],
        )?,
        Change::Resource(r) => tx.execute(
            "INSERT INTO resources(id, project_id, kind, url, title, meta_json, created_at, updated_at, deleted)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id, kind=excluded.kind,
               url=excluded.url, title=excluded.title, meta_json=excluded.meta_json,
               created_at=excluded.created_at, updated_at=excluded.updated_at, deleted=excluded.deleted
             WHERE (excluded.updated_at, excluded.deleted, excluded.url, excluded.title, excluded.kind,
                    excluded.project_id, coalesce(excluded.meta_json, ''), excluded.created_at)
                 > (resources.updated_at, resources.deleted, resources.url, resources.title,
                    resources.kind, resources.project_id, coalesce(resources.meta_json, ''),
                    resources.created_at)",
            params![
                r.id, r.project_id, r.kind, r.url, r.title, json_text(&r.meta), r.created_at,
                r.updated_at, r.deleted
            ],
        )?,
    })
}

/// Whether `id` has a tombstone in `table` (`deleted_sessions`, migration 7;
/// `deleted_records`, migration 8).
fn tombstoned(c: &Connection, table: &str, id: &str) -> Result<bool> {
    Ok(one(
        c,
        &format!("SELECT 1 FROM {table} WHERE id = ?1"),
        params![id],
        |_| Ok(()),
    )?
    .is_some())
}

/// `SELECT` helper returning at most one mapped row.
fn one<T>(
    c: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
    f: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Option<T>> {
    Ok(c.query_row(sql, p, f).optional()?)
}

/// `SELECT` helper collecting all mapped rows.
fn all<T>(
    c: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
    f: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut st = c.prepare_cached(sql)?;
    let rows = st.query_map(p, f)?;
    Ok(rows.collect::<rusqlite::Result<Vec<T>>>()?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::*;

    pub(crate) fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("blirp.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn backup_is_a_complete_snapshot_from_a_read_only_store() {
        let (dir, store) = temp_store();
        let mut values = std::collections::BTreeMap::new();
        values.insert("backup.probe".to_string(), Some(serde_json::json!(42)));
        store.set_settings(&values).unwrap();
        // The CLI backs up through a read-only handle next to the daemon.
        let ro = Store::open_read_only(store.path()).unwrap();
        let dest = dir.path().join("copy.db");
        std::fs::File::create(&dest).unwrap();
        ro.backup_to(&dest).unwrap();
        let copy = Store::open_read_only(&dest).unwrap();
        assert_eq!(
            copy.schema_version().unwrap(),
            store.schema_version().unwrap()
        );
        assert_eq!(copy.quick_check().unwrap(), "ok");
        assert_eq!(
            copy.get_setting("backup.probe").unwrap(),
            Some(serde_json::json!(42))
        );
        // Never overwrites an existing backup.
        assert!(ro.backup_to(&dest).is_err());
    }

    #[test]
    fn migrations_create_every_table_and_are_idempotent() {
        let (dir, store) = temp_store();
        assert_eq!(
            store.schema_version().unwrap(),
            migrations::MIGRATIONS.len() as i64
        );
        let names: Vec<String> = store
            .read(|c| {
                all(
                    c,
                    "SELECT name FROM sqlite_master WHERE type='table'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        for t in [
            "machines",
            "projects",
            "project_paths",
            "sessions",
            "events",
            "events_fts",
            "records",
            "records_fts",
            "briefs",
            "brief_history",
            "wiki_pages",
            "suggestions",
            "resources",
            "ingest_cursors",
            "settings",
            "devices",
            "outbox",
            "sync_state",
            "hub_log",
        ] {
            assert!(names.iter().any(|n| n == t), "missing table {t}");
        }
        drop(store);
        let again = Store::open(&dir.path().join("blirp.db")).unwrap();
        assert_eq!(
            again.schema_version().unwrap(),
            migrations::MIGRATIONS.len() as i64
        );
        let mode: String = again
            .read(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    // Migration 6 keeps every brief version under a deterministic id and
    // points the current brief at its row.
    #[test]
    fn brief_history_migrates_to_unique_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blirp.db");
        let c = Connection::open(&path).unwrap();
        for sql in &migrations::MIGRATIONS[..5] {
            c.execute_batch(sql).unwrap();
        }
        c.pragma_update(None, "user_version", 5).unwrap();
        c.execute_batch(
            "INSERT INTO brief_history VALUES ('p', 1, 'one', 10, 'user'), ('p', 2, 'two', 20, 'distiller');
             INSERT INTO briefs VALUES ('p', 'two', 2, 20, 'distiller');",
        )
        .unwrap();
        drop(c);
        let store = Store::open(&path).unwrap();
        let hist = store.brief_history("p").unwrap();
        assert_eq!(
            hist.iter()
                .map(|h| (h.id.as_str(), h.version, h.body_md.as_str()))
                .collect::<Vec<_>>(),
            [("legacy-p-2", 2, "two"), ("legacy-p-1", 1, "one")]
        );
        let cur = store.get_brief("p").unwrap().unwrap();
        assert_eq!((cur.id.as_str(), cur.version), ("legacy-p-2", 2));
        let next = store.put_brief("p", "three", "user").unwrap();
        assert_eq!(next.version, 3);
        assert!(crate::is_safe_id(&next.id));
        assert_eq!(
            store
                .revert_brief_to("p", "legacy-p-1", "user")
                .unwrap()
                .body_md,
            "one"
        );
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blirp.db");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 999)
            .unwrap();
        let err = Store::open(&path).unwrap_err();
        assert!(err.to_string().contains("newer"), "{err}");
    }

    #[test]
    fn apply_writes_row_and_outbox_atomically() {
        let (_d, store) = temp_store();
        let p = Project {
            id: crate::new_id(),
            name: "p".into(),
            created_at: 1,
            updated_at: 1,
            deleted: false,
            chats: false,
        };
        assert!(store.apply(Change::Project(p.clone())).unwrap());
        let ob = store.outbox_after(0, 10).unwrap();
        assert_eq!(ob.len(), 1);
        assert_eq!(
            (ob[0].entity.as_str(), ob[0].op.as_str()),
            ("projects", "upsert")
        );
        let back: Change = serde_json::from_value(ob[0].payload.clone()).unwrap();
        assert_eq!(back, Change::Project(p.clone()));

        // A failing change rolls back the whole batch, outbox included.
        let bad_path = ProjectPath {
            project_id: "no-such-project".into(),
            machine_id: "m".into(),
            path: "/x".into(),
            git_remote: None,
        };
        let renamed = Project {
            name: "q".into(),
            ..p.clone()
        };
        assert!(
            store
                .apply_all(&[Change::Project(renamed), Change::ProjectPath(bad_path)])
                .is_err()
        );
        assert_eq!(store.outbox_after(0, 10).unwrap().len(), 1);
        assert_eq!(store.get_project(&p.id).unwrap().unwrap().name, "p");

        // Duplicate events are ignored and not re-queued.
        let ev = Event {
            session_id: "s".into(),
            seq: 1,
            ts: 1,
            kind: EventKind::User,
            text: "hello".into(),
            meta: None,
        };
        assert!(store.apply(Change::Event(ev.clone())).unwrap());
        assert!(!store.apply(Change::Event(ev)).unwrap());
        assert_eq!(store.outbox_after(0, 10).unwrap().len(), 2);
    }

    #[test]
    fn memory_is_redacted_before_it_is_stored_and_queued() {
        let (_d, store) = temp_store();
        let key = "AKIAIOSFODNN7EXAMPLE";
        let r = store
            .create_record(Record {
                id: crate::new_id(),
                project_id: "p".into(),
                kind: RecordKind::Note,
                title: format!("key {key}"),
                body: format!("use {key}"),
                status: RecordStatus::Active,
                pinned: false,
                source_session_id: None,
                created_at: 1,
                updated_at: 1,
                updated_by: "user".into(),
            })
            .unwrap();
        store
            .put_brief("p", &format!("brief {key}"), "user")
            .unwrap();
        store
            .create_wiki_page("p", "w", "wiki", &format!("page {key}"), "user")
            .unwrap();
        let stored = store.get_record(&r.id).unwrap().unwrap();
        assert!(stored.body.contains("[REDACTED:") && !stored.title.contains(key));
        assert!(!store.get_brief("p").unwrap().unwrap().body_md.contains(key));
        assert!(
            !store
                .get_wiki_page("p", "w")
                .unwrap()
                .unwrap()
                .body_md
                .contains(key)
        );
        let outbox = serde_json::to_string(&store.outbox_after(0, 100).unwrap()).unwrap();
        assert!(!outbox.contains(key), "{outbox}");
    }

    #[test]
    fn brief_change_writes_history() {
        let (_d, store) = temp_store();
        store.put_brief("p", "one", "user").unwrap();
        let b = store.put_brief("p", "two", "user").unwrap();
        assert_eq!(b.version, 2);
        let hist = store.brief_history("p").unwrap();
        assert_eq!(hist.iter().map(|h| h.version).collect::<Vec<_>>(), [2, 1]);
        let r = store.revert_brief("p", 1, "user").unwrap();
        assert_eq!((r.version, r.body_md.as_str()), (3, "one"));
        assert!(matches!(
            store.revert_brief("p", 9, "user"),
            Err(StoreError::NotFound(_))
        ));
        assert_eq!(store.outbox_after(0, 10).unwrap().len(), 3);
    }
}
