//! Replication storage (§10): outbox batches, hub_log ingest and paging,
//! node-side apply of pulled entries, sync cursors.
//!
//! Ordering rule: rows are last-writer-wins by `hub_seq`. The hub applies a
//! pushed batch right after logging its own pending local writes, so its
//! tables always equal `hub_log` replayed in order. A node applies pulled
//! entries in `hub_seq` order but skips a remote entry for a row it wrote
//! itself later (its own entry either is still unpushed or has a larger
//! `hub_seq` than the remote one); the hub reports the node's own entries as
//! position markers so the node knows which of its writes are "later".

use super::misc::device_row;
use super::projects::path_row;
use super::sessions::session_row;
use super::{Change, Result, Store, StoreError, all, check_ids, one, write_row};
use crate::model::Session;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

/// Present while replication is off (standalone): nothing is queued.
const OUTBOX_OFF_KEY: &str = "sync.outbox_off";
/// Events rowid up to which a backfill queued them (present while one runs).
const BACKFILL_KEY: &str = "sync.backfill_events";

/// Drop outbox entries up to `upto` that are older than the session status
/// coalescing window (`modify_session` reads the time of a session's last
/// entry).
fn prune_in(tx: &Transaction<'_>, upto: i64) -> Result<()> {
    tx.execute(
        "DELETE FROM outbox WHERE origin_seq <= ?1 AND ts < ?2",
        params![upto, crate::now_ms() - super::sessions::STATUS_COALESCE_MS],
    )?;
    Ok(())
}

/// Whether replication is off, so writes queue nothing.
pub(super) fn outbox_off(c: &Connection) -> Result<bool> {
    Ok(one(
        c,
        "SELECT 1 FROM settings WHERE key = ?1",
        params![OUTBOX_OFF_KEY],
        |_| Ok(()),
    )?
    .is_some())
}

/// Queue every row this machine replicates except events (those follow in
/// batches, see [`Store::backfill_events`]): its machine row, folders and
/// sessions, all projects and shared memory, and session and record
/// tombstones. Shared rows keep their `updated_at`, so a stale copy loses
/// to a newer version the hub already has (see `write_row`).
/// Sessions come before any event is queued, so the hub knows the session
/// of every event it receives.
fn backfill_rows_in(tx: &Transaction<'_>) -> Result<usize> {
    use super::memory::{record_row, resource_row, wiki_row};
    let me = super::local_machine_in(tx)?;
    let mut changes: Vec<Change> = Vec::new();
    changes.extend(
        all(
            tx,
            "SELECT * FROM machines WHERE id = ?1",
            params![me],
            super::misc::machine_row,
        )?
        .into_iter()
        .map(Change::Machine),
    );
    changes.extend(
        all(
            tx,
            "SELECT * FROM projects ORDER BY created_at",
            [],
            super::projects::project_row,
        )?
        .into_iter()
        .map(Change::Project),
    );
    changes.extend(
        all(
            tx,
            "SELECT * FROM project_paths WHERE machine_id = ?1",
            params![me],
            path_row,
        )?
        .into_iter()
        .map(Change::ProjectPath),
    );
    changes.extend(
        all(
            tx,
            "SELECT * FROM sessions WHERE machine_id = ?1 ORDER BY started_at",
            params![me],
            session_row,
        )?
        .into_iter()
        .map(Change::Session),
    );
    changes.extend(
        all(tx, "SELECT * FROM records", [], record_row)?
            .into_iter()
            .map(Change::Record),
    );
    changes.extend(
        all(tx, "SELECT * FROM wiki_pages", [], wiki_row)?
            .into_iter()
            .map(Change::WikiPage),
    );
    changes.extend(
        all(tx, "SELECT * FROM resources", [], resource_row)?
            .into_iter()
            .map(Change::Resource),
    );
    // Every brief version, oldest first, then each project's current one
    // (its history row is already queued; this makes it current there too).
    let projects: Vec<String> = all(tx, "SELECT project_id FROM briefs", [], |r| r.get(0))?;
    for p in &projects {
        for b in super::memory::history_in(tx, p)?.into_iter().rev() {
            changes.push(Change::Brief(b));
        }
        if let Some(b) = super::memory::get_brief_in(tx, p)? {
            changes.push(Change::Brief(b));
        }
    }
    changes.extend(
        all(tx, "SELECT id FROM deleted_sessions", [], |r| r.get(0))?
            .into_iter()
            .map(|id| Change::DeleteSession { id }),
    );
    changes.extend(
        all(tx, "SELECT id FROM deleted_records", [], |r| r.get(0))?
            .into_iter()
            .map(|id| Change::DeleteRecord { id }),
    );
    for c in &changes {
        super::queue_in(tx, c)?;
    }
    Ok(changes.len())
}

/// Largest accepted replicated entry. API bodies are capped at 2 MB, so a
/// legitimate entry stays below this even with JSON escaping.
pub const MAX_ENTRY_BYTES: usize = 4 << 20;

/// Another machine's write of session `old`: it may re-point the session to
/// another project (merge), retitle it and change its status fields;
/// everything else is the owner's (what it runs and reads: resume argv,
/// folder, worktree, transcript; and what it derives from its transcript:
/// summary, distill position, tokens, cost), so it is kept from `old`. A
/// foreign copy carries whatever version of those fields it last saw.
fn foreign_session_write(old: Session, new: &Session) -> Session {
    Session {
        project_id: new.project_id.clone(),
        title: new.title.clone(),
        status: new.status,
        ended_at: new.ended_at,
        last_activity_at: new.last_activity_at,
        exit_code: new.exit_code,
        stopped_by_user: new.stopped_by_user,
        ..old
    }
}

/// §10 ownership: a machine's folders, sessions and their events are
/// written only by that machine. Another machine may only re-point an
/// existing folder or session to another project (merge) and retitle a
/// session or change its status fields ([`foreign_session_write`]: its other
/// fields are replaced by the stored ones, the returned change); folders are
/// never removed by another machine (a deleted project's folders are dropped
/// by every machine itself). `hub` is set when a node applies a pull: the
/// hub also writes the machine rows of the nodes it pairs and revokes.
/// Records, briefs, wiki pages, resources and projects are shared by design.
fn check_owner(
    c: &Connection,
    origin: &str,
    hub: Option<&str>,
    change: Change,
) -> std::result::Result<Change, String> {
    let session = |id: &str| -> std::result::Result<Option<Session>, String> {
        one(
            c,
            "SELECT * FROM sessions WHERE id = ?1",
            params![id],
            session_row,
        )
        .map_err(|e| e.to_string())
    };
    let path_project = |machine: &str,
                        path: &str|
     -> std::result::Result<Option<crate::model::ProjectPath>, String> {
        one(
            c,
            "SELECT * FROM project_paths WHERE machine_id = ?1 AND path = ?2",
            params![machine, path],
            path_row,
        )
        .map_err(|e| e.to_string())
    };
    let foreign = |owner: &str| owner != origin;
    match change {
        Change::Machine(m) if foreign(&m.id) && hub != Some(origin) => {
            Err(format!("machine row of {}", m.id))
        }
        Change::DeleteMachine { id } => Err(format!("machine delete of {id}")),
        Change::ProjectPath(p) if foreign(&p.machine_id) => {
            match path_project(&p.machine_id, &p.path)? {
                Some(old) if old.git_remote == p.git_remote => Ok(Change::ProjectPath(p)),
                _ => Err(format!("folder of machine {}", p.machine_id)),
            }
        }
        Change::DeleteProjectPath { machine_id, .. } if foreign(&machine_id) => {
            Err(format!("folder removal on machine {machine_id}"))
        }
        Change::Session(s) => match session(&s.id)? {
            Some(old) if !foreign(&old.machine_id) && !foreign(&s.machine_id) => {
                Ok(Change::Session(s))
            }
            Some(old) if old.machine_id == s.machine_id => {
                Ok(Change::Session(foreign_session_write(old, &s)))
            }
            Some(old) => Err(format!("session of machine {}", old.machine_id)),
            None if !foreign(&s.machine_id) => Ok(Change::Session(s)),
            None => Err(format!("new session for machine {}", s.machine_id)),
        },
        Change::DeleteSession { id } => match session(&id)? {
            Some(old) if foreign(&old.machine_id) => {
                Err(format!("delete of a session of machine {}", old.machine_id))
            }
            _ => Ok(Change::DeleteSession { id }),
        },
        Change::Event(e) => match session(&e.session_id)? {
            Some(s) if !foreign(&s.machine_id) => Ok(Change::Event(e)),
            Some(s) => Err(format!("event of a session of machine {}", s.machine_id)),
            None => Err("event of an unknown session".into()),
        },
        other => Ok(other),
    }
}

/// Everything a replicated entry must pass before it is logged or applied.
/// Returns the change to apply, and its payload when that differs from the
/// entry's (another machine's session write, reduced to what it may change).
fn check_entry(
    c: &Connection,
    origin: &str,
    hub: Option<&str>,
    e: &WireEntry,
) -> std::result::Result<(Change, Option<String>), String> {
    if e.payload_json.len() > MAX_ENTRY_BYTES {
        return Err(format!("payload of {} bytes", e.payload_json.len()));
    }
    let parsed = e.change().map_err(|err| err.to_string())?;
    check_ids(&parsed).map_err(|err| err.to_string())?;
    let sent = matches!(parsed, Change::Session(_)).then(|| parsed.clone());
    let change = check_owner(c, origin, hub, parsed)?;
    let payload = match sent {
        Some(sent) if sent != change => {
            Some(serde_json::to_string(&change).map_err(|err| err.to_string())?)
        }
        _ => None,
    };
    Ok((change, payload))
}

/// One outbox entry as sent from a node to the hub. `payload_json` is the
/// serialized [`Change`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireEntry {
    pub origin_seq: i64,
    pub entity: String,
    pub op: String,
    pub key: String,
    pub payload_json: String,
    pub ts: i64,
}

impl WireEntry {
    fn size(&self) -> usize {
        self.payload_json.len() + self.entity.len() + self.key.len() + self.op.len() + 32
    }

    /// Parse the payload and check it matches the declared entity/op/key.
    fn change(&self) -> Result<Change> {
        let change: Change = serde_json::from_str(&self.payload_json)?;
        let (entity, op, key) = change.describe();
        if entity != self.entity || op != self.op || key != self.key {
            return Err(StoreError::Invalid(format!(
                "entry {} declares {}/{}/{} but carries {entity}/{op}/{key}",
                self.origin_seq, self.entity, self.op, self.key
            )));
        }
        Ok(change)
    }
}

/// An entry of a pull page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PulledEntry {
    /// Written by the requesting machine itself; only its position is sent.
    Own { hub_seq: i64, origin_seq: i64 },
    Remote {
        hub_seq: i64,
        origin_machine: String,
        entry: WireEntry,
    },
}

/// A page of `hub_log` for one requester.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HubPage {
    /// Highest `origin_seq` of the requester's own entries logged at or
    /// before the page start.
    pub own_seen: i64,
    pub entries: Vec<PulledEntry>,
    /// Pull cursor after applying this page.
    pub up_to: i64,
    /// More entries follow `up_to`.
    pub more: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncCursors {
    pub last_pushed_origin_seq: i64,
    pub last_pulled_hub_seq: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestOutcome {
    /// Highest origin_seq now durably logged for the origin.
    pub acked: i64,
    /// Newly logged entries (0 for a fully duplicate batch).
    pub inserted: usize,
    /// Entries rejected as malformed (logged at warn, never retried).
    pub rejected: usize,
}

fn cursors_in(c: &rusqlite::Connection, peer: &str) -> Result<SyncCursors> {
    Ok(one(
        c,
        "SELECT last_pushed_origin_seq, last_pulled_hub_seq FROM sync_state WHERE peer = ?1",
        params![peer],
        |r| {
            Ok(SyncCursors {
                last_pushed_origin_seq: r.get(0)?,
                last_pulled_hub_seq: r.get(1)?,
            })
        },
    )?
    .unwrap_or_default())
}

fn set_cursors_in(tx: &Transaction<'_>, peer: &str, c: SyncCursors) -> Result<()> {
    tx.execute(
        "INSERT INTO sync_state(peer, last_pushed_origin_seq, last_pulled_hub_seq) VALUES (?1,?2,?3)
         ON CONFLICT(peer) DO UPDATE SET last_pushed_origin_seq=excluded.last_pushed_origin_seq,
           last_pulled_hub_seq=excluded.last_pulled_hub_seq",
        params![peer, c.last_pushed_origin_seq, c.last_pulled_hub_seq],
    )?;
    Ok(())
}

/// Run `f` inside a savepoint: its writes are rolled back if it fails, the
/// enclosing transaction continues either way.
fn savepoint<T>(tx: &Transaction<'_>, f: impl FnOnce() -> Result<T>) -> Result<Result<T>> {
    tx.execute_batch("SAVEPOINT repl_entry")?;
    match f() {
        Ok(v) => {
            tx.execute_batch("RELEASE repl_entry")?;
            Ok(Ok(v))
        }
        Err(e) => {
            tx.execute_batch("ROLLBACK TO repl_entry; RELEASE repl_entry")?;
            Ok(Err(e))
        }
    }
}

/// Move this machine's own outbox entries past its cursor into `hub_log`.
fn flush_own_in(tx: &Transaction<'_>, own: &str) -> Result<usize> {
    let mut cur = cursors_in(tx, own)?;
    let n = tx.execute(
        "INSERT OR IGNORE INTO hub_log(origin_machine, origin_seq, entity, op, key, payload_json, ts)
         SELECT ?1, origin_seq, entity, op, key,
           CASE WHEN entity = 'events' THEN '' ELSE payload_json END, ts
         FROM outbox WHERE origin_seq > ?2 ORDER BY origin_seq",
        params![own, cur.last_pushed_origin_seq],
    )?;
    let head: i64 = tx.query_row("SELECT coalesce(max(origin_seq), 0) FROM outbox", [], |r| {
        r.get(0)
    })?;
    if head > cur.last_pushed_origin_seq {
        cur.last_pushed_origin_seq = head;
        set_cursors_in(tx, own, cur)?;
    }
    // Logged: the hub's own outbox entries have done their job (the last
    // few seconds stay: session status coalescing looks at them).
    prune_in(tx, cur.last_pushed_origin_seq)?;
    Ok(n)
}

/// What one [`Store::compact_hub_log`] run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Compacted {
    /// Rows deleted.
    pub removed: usize,
    /// Rows reduced to a position marker.
    pub stripped: usize,
}

/// Compacted entities and the SQL for the owner of their row `l`: an
/// upsert by the owner rewrites every column, so it hides what came before.
/// (A foreign write of a session or folder only changes some columns and
/// needs the row to exist; it is kept when it follows the owner's last
/// upsert.)
const COMPACTED: [(&str, &str); 3] = [
    ("machines", "l.key"),
    (
        "project_paths",
        "substr(l.key, 1, instr(l.key, char(10)) - 1)",
    ),
    (
        "sessions",
        "CASE WHEN l.payload_json <> '' THEN json_extract(l.payload_json, '$.row.machine_id') END",
    ),
];

/// Highest floor up to which `hub_log` was compacted (hub).
const COMPACTED_KEY: &str = "sync.hub_log_compacted";

fn compacted_in(c: &Connection) -> Result<i64> {
    Ok(one(
        c,
        "SELECT CAST(value_json AS INTEGER) FROM settings WHERE key = ?1",
        params![COMPACTED_KEY],
        |r| r.get(0),
    )?
    .unwrap_or(0))
}

/// One batch of [`Store::compact_hub_log`]: upserts of `entity` at or below
/// `floor` after `pos` (key, hub_seq), in row order. Returns what it did
/// and where the next batch starts (None: this entity is done).
fn compact_batch_in(
    tx: &Transaction<'_>,
    entity: &str,
    owner: &str,
    floor: i64,
    pos: &(String, i64),
    batch: usize,
) -> Result<(Compacted, Option<(String, i64)>)> {
    let rows = all(
        tx,
        "SELECT hub_seq, key, origin_machine, origin_seq, payload_json = '' FROM hub_log
         WHERE op = 'upsert' AND entity = ?1 AND (key, hub_seq) > (?2, ?3) AND hub_seq <= ?4
         ORDER BY key, hub_seq LIMIT ?5",
        params![entity, pos.0, pos.1, floor, batch as i64],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, bool>(4)?,
            ))
        },
    )?;
    let mut superseded = tx.prepare_cached(&format!(
        "SELECT 1 FROM hub_log l
         WHERE l.op = 'upsert' AND l.entity = ?1 AND l.key = ?2 AND l.hub_seq > ?3
           AND l.hub_seq <= ?4 AND l.payload_json <> '' AND ({owner}) = l.origin_machine
         LIMIT 1"
    ))?;
    // A later row of the same origin with a higher origin_seq keeps that
    // origin's highest origin_seq at or below the floor in the log.
    let mut outranked = tx.prepare_cached(
        "SELECT 1 FROM hub_log WHERE origin_machine = ?1 AND hub_seq > ?2 AND hub_seq <= ?3
           AND origin_seq > ?4
         LIMIT 1",
    )?;
    // The first upsert of a row created it: a session's events are only
    // accepted once it exists, so a replay needs it before them.
    let mut first = tx.prepare_cached(
        "SELECT 1 FROM hub_log
         WHERE op = 'upsert' AND entity = ?1 AND key = ?2 AND hub_seq < ?3
         LIMIT 1",
    )?;
    let mut done = Compacted::default();
    let changed = || -> Result<()> {
        tx.execute(
            "INSERT INTO settings(key, value_json) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json
             WHERE CAST(value_json AS INTEGER) < CAST(excluded.value_json AS INTEGER)",
            params![COMPACTED_KEY, floor.to_string()],
        )?;
        Ok(())
    };
    for (hub_seq, key, origin, origin_seq, stripped) in &rows {
        if !superseded.exists(params![entity, key, hub_seq, floor])?
            || !first.exists(params![entity, key, hub_seq])?
        {
            continue;
        }
        if outranked.exists(params![origin, hub_seq, floor, origin_seq])? {
            tx.execute("DELETE FROM hub_log WHERE hub_seq = ?1", params![hub_seq])?;
            done.removed += 1;
        } else if !stripped {
            tx.execute(
                "UPDATE hub_log SET payload_json = '' WHERE hub_seq = ?1",
                params![hub_seq],
            )?;
            done.stripped += 1;
        }
    }
    if done != Compacted::default() {
        changed()?;
    }
    let next = if rows.len() < batch {
        None
    } else {
        rows.last().map(|(h, k, ..)| (k.clone(), *h))
    };
    Ok((done, next))
}

impl Store {
    /// Turn replication on (a hub or node) or off (standalone). Off: nothing
    /// is queued and the outbox is emptied; nothing unpushed is lost by that,
    /// since pairing again queues the current state, including the
    /// tombstones of deleted sessions and records, and newer versions win
    /// wherever stale copies meet them. Turning it on backfills: every
    /// replicated row except events is queued in this transaction, events
    /// that exist now follow in bounded, resumable batches
    /// ([`Store::backfill_events`]). Returns how many rows were queued.
    pub fn set_replication(&self, on: bool) -> Result<usize> {
        let queued = self.write(|tx| {
            let off = outbox_off(tx)?;
            match (on, off) {
                (true, true) => {
                    tx.execute(
                        "DELETE FROM settings WHERE key = ?1",
                        params![OUTBOX_OFF_KEY],
                    )?;
                    let n = backfill_rows_in(tx)?;
                    // Events written from now on are queued as they come.
                    let until: i64 =
                        tx.query_row("SELECT coalesce(max(rowid), 0) FROM events", [], |r| {
                            r.get(0)
                        })?;
                    tx.execute(
                        "INSERT INTO settings(key, value_json) VALUES (?1, ?2)
                         ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
                        params![BACKFILL_KEY, serde_json::to_string(&[0, until])?],
                    )?;
                    Ok(n)
                }
                (false, false) => {
                    tx.execute(
                        "INSERT INTO settings(key, value_json) VALUES (?1, 'true')",
                        params![OUTBOX_OFF_KEY],
                    )?;
                    tx.execute("DELETE FROM settings WHERE key = ?1", params![BACKFILL_KEY])?;
                    tx.execute("DELETE FROM outbox_deferred", [])?;
                    Ok(0)
                }
                _ => Ok(0),
            }
        })?;
        if !on {
            // Emptied in chunks so a large outbox never holds the writer long.
            while self.write(|tx| {
                Ok(tx.execute(
                    "DELETE FROM outbox WHERE origin_seq IN
                       (SELECT origin_seq FROM outbox ORDER BY origin_seq LIMIT 20000)",
                    [],
                )?)
            })? > 0
            {}
        }
        Ok(queued)
    }

    /// Queue up to `limit` more of this machine's events after turning
    /// replication on (see [`Store::set_replication`]): the events that
    /// existed then, in rowid order. The position is kept in the database,
    /// so it resumes after a restart. Returns how many were queued (0:
    /// nothing left, the backfill is finished).
    pub fn backfill_events(&self, limit: usize) -> Result<usize> {
        // Usually none is running: answer that without taking the writer.
        let running = self.read(|c| {
            Ok(one(
                c,
                "SELECT 1 FROM settings WHERE key = ?1",
                params![BACKFILL_KEY],
                |_| Ok(()),
            )?
            .is_some())
        })?;
        if !running {
            return Ok(0);
        }
        self.write(|tx| {
            let pos: Option<String> = one(
                tx,
                "SELECT value_json FROM settings WHERE key = ?1",
                params![BACKFILL_KEY],
                |r| r.get(0),
            )?;
            let Some(pos) = pos else {
                return Ok(0);
            };
            // `[after, until]` rowids; a bare position is a backfill started
            // before the bound was kept: it ends at the events there are now.
            let (after, until) = match serde_json::from_str::<(i64, i64)>(&pos) {
                Ok(p) => p,
                Err(_) => (
                    pos.parse::<i64>().unwrap_or(0),
                    tx.query_row("SELECT coalesce(max(rowid), 0) FROM events", [], |r| {
                        r.get(0)
                    })?,
                ),
            };
            let me = super::local_machine_in(tx)?;
            let rows = all(
                tx,
                "SELECT e.rowid AS rid, e.* FROM events e
                 JOIN sessions s ON s.id = e.session_id AND s.machine_id = ?1
                 WHERE e.rowid > ?2 AND e.rowid <= ?3 ORDER BY e.rowid LIMIT ?4",
                params![me, after, until, limit as i64],
                |r| Ok((r.get::<_, i64>("rid")?, super::sessions::event_row(r)?)),
            )?;
            let Some(last) = rows.last().map(|(rid, _)| *rid) else {
                tx.execute("DELETE FROM settings WHERE key = ?1", params![BACKFILL_KEY])?;
                return Ok(0);
            };
            for (_, e) in &rows {
                super::queue_in(tx, &Change::Event(e.clone()))?;
            }
            tx.execute(
                "UPDATE settings SET value_json = ?2 WHERE key = ?1",
                params![BACKFILL_KEY, serde_json::to_string(&[last, until])?],
            )?;
            Ok(rows.len())
        })
    }

    /// Write a replicated change received from another machine: the row
    /// only, never the outbox. Returns false for a duplicate event.
    pub fn apply_remote(&self, change: &Change) -> Result<bool> {
        self.write(|tx| Ok(write_row(tx, change)? > 0 || !matches!(change, Change::Event(_))))
    }

    /// Highest `origin_seq` in the outbox (0 when empty).
    pub fn outbox_head(&self) -> Result<i64> {
        self.read(|c| {
            Ok(
                c.query_row("SELECT coalesce(max(origin_seq), 0) FROM outbox", [], |r| {
                    r.get(0)
                })?,
            )
        })
    }

    /// Outbox entries after `after`, at most `max_entries` and about
    /// `max_bytes` (always at least one entry when any exist).
    pub fn outbox_batch(
        &self,
        after: i64,
        max_entries: usize,
        max_bytes: usize,
    ) -> Result<Vec<WireEntry>> {
        self.read(|c| {
            let mut st = c.prepare_cached(
                "SELECT origin_seq, entity, op, key, payload_json, ts FROM outbox
                 WHERE origin_seq > ?1 ORDER BY origin_seq LIMIT ?2",
            )?;
            let rows = st.query_map(params![after, max_entries as i64], |r| {
                Ok(WireEntry {
                    origin_seq: r.get(0)?,
                    entity: r.get(1)?,
                    op: r.get(2)?,
                    key: r.get(3)?,
                    payload_json: r.get(4)?,
                    ts: r.get(5)?,
                })
            })?;
            let mut out = Vec::new();
            let mut bytes = 0;
            for row in rows {
                let e = row?;
                bytes += e.size();
                if !out.is_empty() && bytes > max_bytes {
                    break;
                }
                out.push(e);
            }
            Ok(out)
        })
    }

    pub fn sync_cursors(&self, peer: &str) -> Result<SyncCursors> {
        self.read(|c| cursors_in(c, peer))
    }

    /// Record that the hub acknowledged our outbox up to `seq`.
    pub fn set_pushed_cursor(&self, peer: &str, seq: i64) -> Result<()> {
        self.write(|tx| {
            let mut c = cursors_in(tx, peer)?;
            if seq > c.last_pushed_origin_seq {
                c.last_pushed_origin_seq = seq;
                set_cursors_in(tx, peer, c)?;
            }
            Ok(())
        })
    }

    /// Outbox entries not yet acknowledged by `peer`.
    pub fn pending_outbox(&self, peer: &str) -> Result<i64> {
        self.read(|c| {
            let pushed = cursors_in(c, peer)?.last_pushed_origin_seq;
            Ok(c.query_row(
                "SELECT count(*) FROM outbox WHERE origin_seq > ?1",
                params![pushed],
                |r| r.get(0),
            )?)
        })
    }

    /// Hub: log this machine's own new writes. Returns how many were added.
    pub fn hub_flush_own(&self, own: &str) -> Result<usize> {
        self.write(|tx| flush_own_in(tx, own))
    }

    /// Highest `hub_seq` (0 when empty).
    pub fn hub_head(&self) -> Result<i64> {
        self.read(|c| {
            Ok(
                c.query_row("SELECT coalesce(max(hub_seq), 0) FROM hub_log", [], |r| {
                    r.get(0)
                })?,
            )
        })
    }

    /// Hub: log and apply a batch pushed by `origin`, in one transaction.
    /// Idempotent on `(origin, origin_seq)`; the hub's own pending writes are
    /// logged first so the log order matches the order rows were written here.
    pub fn hub_ingest(
        &self,
        own: &str,
        origin: &str,
        entries: &[WireEntry],
    ) -> Result<IngestOutcome> {
        if origin == own {
            return Err(StoreError::Invalid("hub cannot ingest its own id".into()));
        }
        self.write(|tx| {
            flush_own_in(tx, own)?;
            let mut cur = cursors_in(tx, origin)?;
            let mut inserted = 0;
            let mut rejected = 0;
            let mut acked = cur.last_pushed_origin_seq;
            for e in entries {
                if e.origin_seq <= 0 {
                    return Err(StoreError::Invalid(format!(
                        "origin_seq {} must be positive",
                        e.origin_seq
                    )));
                }
                // Already logged (or rejected) in an earlier transaction: the
                // cursor moves with the log. Its row may have been compacted
                // away, so the log's unique key no longer catches it.
                if e.origin_seq <= cur.last_pushed_origin_seq {
                    continue;
                }
                acked = acked.max(e.origin_seq);
                let (change, payload) = match check_entry(tx, origin, None, e) {
                    Ok(c) => c,
                    Err(err) => {
                        tracing::warn!(origin, origin_seq = e.origin_seq, entity = %e.entity, error = %err, "rejecting replicated entry");
                        rejected += 1;
                        continue;
                    }
                };
                // Logged as applied: other machines never see what the
                // origin was not allowed to change. Events are logged
                // without payload: pulls read them from `events`.
                let payload = match change {
                    Change::Event(_) => "",
                    _ => payload.as_deref().unwrap_or(&e.payload_json),
                };
                let n = tx.execute(
                    "INSERT OR IGNORE INTO hub_log(origin_machine, origin_seq, entity, op, key, payload_json, ts)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![origin, e.origin_seq, e.entity, e.op, e.key, payload, e.ts],
                )?;
                if n == 0 {
                    continue;
                }
                inserted += 1;
                if let Err(err) = savepoint(tx, || write_row(tx, &change))? {
                    // Logged regardless: other machines may still apply it.
                    tracing::warn!(origin, origin_seq = e.origin_seq, entity = %e.entity, error = %err, "hub could not apply replicated entry");
                }
            }
            if acked > cur.last_pushed_origin_seq {
                cur.last_pushed_origin_seq = acked;
                set_cursors_in(tx, origin, cur)?;
            }
            Ok(IngestOutcome {
                acked,
                inserted,
                rejected,
            })
        })
    }

    /// Hub: `hub_log` after `after` for `requester` (its own entries become
    /// markers), at most `max_entries` rows and about `max_bytes` of payload.
    /// Events are read from `events` (logged without payload); one the hub
    /// no longer has (its session was deleted) is left out. So is a
    /// compacted row ([`Store::compact_hub_log`]), which only its origin
    /// still gets, as a marker.
    pub fn hub_page(
        &self,
        requester: &str,
        after: i64,
        max_entries: usize,
        max_bytes: usize,
    ) -> Result<HubPage> {
        self.read(|c| {
            let own_seen: i64 = c.query_row(
                "SELECT coalesce(max(origin_seq), 0) FROM hub_log
                 WHERE origin_machine = ?1 AND hub_seq <= ?2",
                params![requester, after],
                |r| r.get(0),
            )?;
            let mut st = c.prepare_cached(
                "SELECT hub_seq, origin_machine, origin_seq, entity, op, key, payload_json, ts
                 FROM hub_log WHERE hub_seq > ?1 ORDER BY hub_seq LIMIT ?2",
            )?;
            let mut event =
                c.prepare_cached("SELECT * FROM events WHERE session_id = ?1 AND seq = ?2")?;
            let limit = max_entries as i64 + 1;
            let mut rows = st.query(params![after, limit])?;
            let mut entries = Vec::new();
            let mut scanned = 0;
            let mut bytes = 0;
            let mut up_to = after;
            let mut more = false;
            while let Some(r) = rows.next()? {
                if scanned == max_entries {
                    more = true;
                    break;
                }
                scanned += 1;
                let hub_seq: i64 = r.get(0)?;
                let origin_machine: String = r.get(1)?;
                let origin_seq: i64 = r.get(2)?;
                let entry = if origin_machine == requester {
                    bytes += 16;
                    PulledEntry::Own {
                        hub_seq,
                        origin_seq,
                    }
                } else {
                    let entity: String = r.get(3)?;
                    let key: String = r.get(5)?;
                    let mut payload_json: String = r.get(6)?;
                    if payload_json.is_empty() {
                        let found = match (entity.as_str(), key.rsplit_once('\n')) {
                            ("events", Some((session, seq))) => match seq.parse::<i64>() {
                                Ok(seq) => event
                                    .query_row(params![session, seq], super::sessions::event_row)
                                    .optional()?,
                                Err(_) => None,
                            },
                            _ => None,
                        };
                        let Some(e) = found else {
                            // Nothing to send; the cursor still moves past it.
                            up_to = hub_seq;
                            continue;
                        };
                        payload_json = serde_json::to_string(&Change::Event(e))?;
                    }
                    let entry = WireEntry {
                        origin_seq,
                        entity,
                        op: r.get(4)?,
                        key,
                        payload_json,
                        ts: r.get(7)?,
                    };
                    bytes += entry.size() + origin_machine.len();
                    PulledEntry::Remote {
                        hub_seq,
                        origin_machine,
                        entry,
                    }
                };
                if !entries.is_empty() && bytes > max_bytes {
                    more = true;
                    break;
                }
                entries.push(entry);
                up_to = hub_seq;
            }
            Ok(HubPage {
                own_seen,
                entries,
                up_to,
                more,
            })
        })
    }

    /// Hub: `machine` asks for the log after `after`, so it has applied
    /// everything up to there. Bounds compaction ([`Store::compact_hub_log`]).
    /// Returns false, recording nothing, when its cursor went back below a
    /// position it already pulled from and below what was compacted (e.g.
    /// its database lost recent writes): its pulls could then skip writes
    /// they must apply, so it has to leave and pair again.
    pub fn hub_record_pull(&self, machine: &str, after: i64) -> Result<bool> {
        self.write(|tx| {
            let recorded: Option<i64> = one(
                tx,
                "SELECT after FROM hub_pulls WHERE machine_id = ?1",
                params![machine],
                |r| r.get(0),
            )?;
            if recorded.is_some_and(|r| after < r) && after < compacted_in(tx)? {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO hub_pulls(machine_id, after) VALUES (?1, ?2)
                 ON CONFLICT(machine_id) DO UPDATE SET after = excluded.after
                 WHERE after <> excluded.after",
                params![machine, after],
            )?;
            Ok(true)
        })
    }

    /// Hub: forget `machine`'s pull position (it was revoked): pairing
    /// again starts from whatever cursor it has.
    pub fn hub_forget_pull(&self, machine: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM hub_pulls WHERE machine_id = ?1",
                params![machine],
            )?;
            Ok(())
        })
    }

    /// Hub: drop superseded rows from `hub_log` at or below the compaction
    /// floor, in write transactions of at most `batch` rows.
    ///
    /// The floor is the lowest pull position of the paired, non-revoked
    /// machines (0 for one that has not pulled yet; the head when none is
    /// paired): no active node reads below it again, so what is compacted
    /// there changes nothing for them. A node whose cursor is below it is a
    /// machine paired afresh or again: joining starts from standalone,
    /// which empties its outbox, so all of its queued writes are newer than
    /// anything it had logged and the rows compacted away cannot change what
    /// it skips or applies.
    ///
    /// Compacted: upserts of machines, folders and sessions (last writer
    /// wins by hub order) logged before a later upsert of the same row by
    /// its owner, which rewrites the whole row, except the row's first
    /// upsert (a session's events need the session to exist). Deletes, events (logged
    /// without payload) and shared rows (newest version wins by content,
    /// not by log order; brief upserts carry their history rows) are kept.
    /// Each origin's row with its highest `origin_seq` at or below the floor
    /// is stripped to a marker instead of removed, so `own_seen` in
    /// [`Store::hub_page`] stays exact.
    pub fn compact_hub_log(&self, batch: usize) -> Result<Compacted> {
        let floor = self.read(|c| {
            Ok(c.query_row(
                "SELECT coalesce(
                   (SELECT min(coalesce(p.after, 0)) FROM devices d
                    LEFT JOIN hub_pulls p ON p.machine_id = d.node_id
                    WHERE d.kind = 'machine' AND d.revoked = 0),
                   (SELECT coalesce(max(hub_seq), 0) FROM hub_log))",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })?;
        let mut done = Compacted::default();
        for (entity, owner) in COMPACTED {
            let mut pos = (String::new(), 0);
            loop {
                let (step, next) =
                    self.write(|tx| compact_batch_in(tx, entity, owner, floor, &pos, batch))?;
                done.removed += step.removed;
                done.stripped += step.stripped;
                match next {
                    Some(p) => pos = p,
                    None => break,
                }
            }
        }
        Ok(done)
    }

    /// Node: apply a pull page from `hub` and advance the pull cursor in the
    /// same transaction. Returns how many remote entries were applied.
    pub fn node_apply_pull(&self, hub: &str, page: &HubPage) -> Result<usize> {
        let applied = self.node_apply_pull_in(hub, page)?;
        if !page.more {
            self.pulled_to_head
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(applied)
    }

    /// Whether a node pull reached the hub's head since the store was
    /// opened (this machine has seen every change the hub had then).
    pub fn pulled_to_head(&self) -> bool {
        self.pulled_to_head
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn node_apply_pull_in(&self, hub: &str, page: &HubPage) -> Result<usize> {
        self.write(|tx| {
            let mut cur = cursors_in(tx, hub)?;
            if page.up_to <= cur.last_pulled_hub_seq {
                return Ok(0);
            }
            let me = super::local_machine_in(tx)?;
            let mut own_seen = page.own_seen;
            let mut applied = 0;
            let mut later_own = tx.prepare_cached(
                "SELECT 1 FROM outbox WHERE entity = ?1 AND key = ?2 AND origin_seq > ?3
                 UNION ALL SELECT 1 FROM outbox_deferred WHERE entity = ?1 AND key = ?2
                 LIMIT 1",
            )?;
            for pe in &page.entries {
                match pe {
                    PulledEntry::Own { hub_seq, origin_seq } => {
                        if *hub_seq > cur.last_pulled_hub_seq {
                            own_seen = own_seen.max(*origin_seq);
                        }
                    }
                    PulledEntry::Remote {
                        hub_seq,
                        origin_machine,
                        entry,
                    } => {
                        if *hub_seq <= cur.last_pulled_hub_seq {
                            continue;
                        }
                        // This machine's own writes only ever come back as
                        // markers; a "remote" entry claiming to be ours
                        // would pass every ownership check.
                        if origin_machine.is_empty() || *origin_machine == me {
                            tracing::warn!(origin = %origin_machine, hub_seq, entity = %entry.entity, "skipping a pulled entry that claims this machine as its origin");
                            continue;
                        }
                        // The hub checked this too; a node does not rely on it.
                        let change = match check_entry(tx, origin_machine, Some(hub), entry) {
                            Ok((c, _)) => c,
                            Err(err) => {
                                tracing::warn!(origin = %origin_machine, hub_seq, entity = %entry.entity, error = %err, "skipping rejected replicated entry");
                                continue;
                            }
                        };
                        // Events are append-only, deletes of sessions and
                        // records always win (tombstones) and shared rows
                        // keep the newest version (`write_row`); only owned
                        // rows are last-writer-wins by hub order.
                        let by_hub_order = matches!(
                            change,
                            Change::Machine(_)
                                | Change::DeleteMachine { .. }
                                | Change::ProjectPath(_)
                                | Change::DeleteProjectPath { .. }
                                | Change::Session(_)
                        );
                        if by_hub_order
                            && later_own.exists(params![entry.entity, entry.key, own_seen])?
                        {
                            continue;
                        }
                        match savepoint(tx, || write_row(tx, &change))? {
                            Ok(_) => applied += 1,
                            Err(err) => {
                                tracing::warn!(origin = %origin_machine, hub_seq, entity = %entry.entity, error = %err, "could not apply replicated entry");
                            }
                        }
                    }
                }
            }
            drop(later_own);
            cur.last_pulled_hub_seq = page.up_to;
            set_cursors_in(tx, hub, cur)?;
            // Our entries logged up to here are behind every entry still to
            // be pulled, so they can never be the "later own write" that
            // makes a pull skip a remote one: they are no longer needed.
            // Never past what the hub acknowledged: `own_seen` is the hub's
            // word, and an unpushed entry dropped here would be lost.
            prune_in(tx, own_seen.min(cur.last_pushed_origin_seq))?;
            Ok(applied)
        })
    }

    /// Replace a pre-identity machine id (a local uuid) with the iroh
    /// endpoint id: moves this machine's folders and sessions and drops the
    /// old machine row, all through the outbox.
    pub fn rebind_machine(&self, old: &str, new: &str) -> Result<()> {
        if old == new {
            return Ok(());
        }
        self.write(|tx| {
            let paths = all(
                tx,
                "SELECT * FROM project_paths WHERE machine_id = ?1",
                params![old],
                path_row,
            )?;
            for p in paths {
                super::apply_in(
                    tx,
                    &Change::DeleteProjectPath {
                        machine_id: old.to_string(),
                        path: p.path.clone(),
                    },
                )?;
                super::apply_in(
                    tx,
                    &Change::ProjectPath(crate::model::ProjectPath {
                        machine_id: new.to_string(),
                        ..p
                    }),
                )?;
            }
            let sessions = all(
                tx,
                "SELECT * FROM sessions WHERE machine_id = ?1",
                params![old],
                session_row,
            )?;
            for s in sessions {
                super::apply_in(
                    tx,
                    &Change::Session(crate::model::Session {
                        machine_id: new.to_string(),
                        ..s
                    }),
                )?;
            }
            super::apply_in(
                tx,
                &Change::DeleteMachine {
                    id: old.to_string(),
                },
            )?;
            Ok(())
        })
    }

    /// The non-revoked machine device paired with iroh endpoint `node_id`.
    pub fn machine_device(&self, node_id: &str) -> Result<Option<crate::model::Device>> {
        self.read(|c| {
            one(
                c,
                "SELECT * FROM devices WHERE kind = 'machine' AND node_id = ?1 AND revoked = 0",
                params![node_id],
                device_row,
            )
        })
    }

    /// Any device (revoked or not) paired with endpoint `node_id`.
    pub fn device_by_node_id(&self, node_id: &str) -> Result<Option<crate::model::Device>> {
        self.read(|c| {
            one(
                c,
                "SELECT * FROM devices WHERE node_id = ?1 ORDER BY revoked, created_at DESC LIMIT 1",
                params![node_id],
                device_row,
            )
        })
    }

    pub fn get_device(&self, id: &str) -> Result<Option<crate::model::Device>> {
        self.read(|c| {
            one(
                c,
                "SELECT * FROM devices WHERE id = ?1",
                params![id],
                device_row,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::temp_store;
    use super::*;
    use crate::model::{Machine, MachineRole, Project};

    fn project(id: &str, name: &str) -> Change {
        Change::Project(Project {
            id: id.into(),
            name: name.into(),
            created_at: 1,
            updated_at: 1,
            deleted: false,
            chats: false,
            merged_into: None,
        })
    }

    fn name_of(s: &Store, id: &str) -> String {
        s.get_project(id).unwrap().unwrap().name
    }

    /// Push everything pending from `node` to `hub`.
    fn push(node: &Store, node_id: &str, hub: &Store, hub_id: &str) {
        let after = node.sync_cursors(hub_id).unwrap().last_pushed_origin_seq;
        let batch = node.outbox_batch(after, 500, 4 << 20).unwrap();
        let out = hub.hub_ingest(hub_id, node_id, &batch).unwrap();
        node.set_pushed_cursor(hub_id, out.acked).unwrap();
    }

    /// Make every outbox entry older than the coalescing window, so pruning
    /// runs in these tests as it does in real use.
    fn age(s: &Store) {
        s.write(|tx| Ok(tx.execute("UPDATE outbox SET ts = 0", [])?))
            .unwrap();
    }

    fn outbox_len(s: &Store) -> usize {
        s.outbox_after(0, 1_000_000).unwrap().len()
    }

    fn pull(node: &Store, node_id: &str, hub: &Store, hub_id: &str) -> usize {
        age(node);
        age(hub);
        hub.hub_flush_own(hub_id).unwrap();
        let mut total = 0;
        loop {
            let after = node.sync_cursors(hub_id).unwrap().last_pulled_hub_seq;
            assert!(hub.hub_record_pull(node_id, after).unwrap());
            let page = hub.hub_page(node_id, after, 2, 4 << 20).unwrap();
            total += node.node_apply_pull(hub_id, &page).unwrap();
            if !page.more {
                return total;
            }
        }
    }

    #[test]
    fn push_pull_idempotent_without_echo() {
        let (_h, hub) = temp_store();
        let (_a, a) = temp_store();
        let (_b, b) = temp_store();
        a.apply(project("p1", "from a")).unwrap();
        hub.apply(project("p2", "from hub")).unwrap();
        push(&a, "A", &hub, "H");
        // Duplicate push is a no-op.
        let batch = a.outbox_batch(0, 500, 4 << 20).unwrap();
        let again = hub.hub_ingest("H", "A", &batch).unwrap();
        assert_eq!((again.inserted, again.acked), (0, 1));
        assert_eq!(name_of(&hub, "p1"), "from a");

        assert_eq!(pull(&b, "B", &hub, "H"), 2);
        assert_eq!(name_of(&b, "p1"), "from a");
        assert_eq!(name_of(&b, "p2"), "from hub");
        // Applied rows were not queued again on B or on the hub.
        assert!(b.outbox_batch(0, 500, 4 << 20).unwrap().is_empty());
        hub.hub_flush_own("H").unwrap();
        assert_eq!(hub.hub_head().unwrap(), 2);
        // A skips its own entry and gets the hub's.
        assert_eq!(pull(&a, "A", &hub, "H"), 1);
        assert_eq!(pull(&a, "A", &hub, "H"), 0);
    }

    fn project_at(id: &str, name: &str, updated_at: i64) -> Change {
        Change::Project(Project {
            id: id.into(),
            name: name.into(),
            created_at: 1,
            updated_at,
            deleted: false,
            chats: false,
            merged_into: None,
        })
    }

    // Shared rows converge on the newest version whatever order the
    // versions reach the hub and the nodes in.
    #[test]
    fn newest_version_wins_everywhere() {
        let (_h, hub) = temp_store();
        let (_a, a) = temp_store();
        let (_b, b) = temp_store();
        a.apply(project_at("p", "v0", 10)).unwrap();
        push(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");

        // B's newer v1 reaches the hub first; A's older v2 (unpushed) loses
        // on A when it pulls v1, and on the hub when it arrives.
        b.apply(project_at("p", "v1", 30)).unwrap();
        push(&b, "B", &hub, "H");
        a.apply(project_at("p", "v2", 20)).unwrap();
        pull(&a, "A", &hub, "H");
        push(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        for s in [&hub, &a, &b] {
            assert_eq!(name_of(s, "p"), "v1");
        }

        // A newer write reaching the hub last wins too.
        a.apply(project_at("p", "v3", 50)).unwrap();
        b.apply(project_at("p", "v4", 40)).unwrap();
        push(&b, "B", &hub, "H");
        push(&a, "A", &hub, "H");
        pull(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        for s in [&hub, &a, &b] {
            assert_eq!(name_of(s, "p"), "v3");
        }

        // Equal timestamps: every machine picks the same version.
        a.apply(project_at("p", "same-a", 70)).unwrap();
        b.apply(project_at("p", "same-b", 70)).unwrap();
        push(&a, "A", &hub, "H");
        push(&b, "B", &hub, "H");
        pull(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        for s in [&hub, &a, &b] {
            assert_eq!(name_of(s, "p"), "same-b");
        }

        // A local edit on a machine whose clock is behind the version it
        // replaces is stamped after it, so it is not lost.
        a.apply(project_at("p", "slow clock", 5)).unwrap();
        push(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        for s in [&hub, &a, &b] {
            assert_eq!(name_of(s, "p"), "slow clock");
        }
    }

    // A machine that leaves and pairs again queues its (stale) copy of
    // everything; newer versions written meanwhile stay newest, deleted
    // records stay deleted, and a delete it had not pushed when it left
    // still reaches the others (its tombstone is queued again).
    #[test]
    fn rejoining_with_stale_copies_keeps_newer_versions() {
        use crate::model::{Record, RecordKind, RecordStatus};
        let (_h, hub) = temp_store();
        let (_a, a) = temp_store();
        let (_b, b) = temp_store();
        for s in [&hub, &a, &b] {
            s.set_replication(true).unwrap();
        }
        let record = |id: &str, body: &str, at: i64| Record {
            id: id.into(),
            project_id: "p".into(),
            kind: RecordKind::Decision,
            title: id.into(),
            body: body.into(),
            status: RecordStatus::Active,
            pinned: false,
            source_session_id: None,
            created_at: 1,
            updated_at: at,
            updated_by: "user".into(),
        };
        a.apply(project_at("p", "shared", 10)).unwrap();
        a.put_brief("p", "old brief", "user").unwrap();
        a.create_wiki_page("p", "w", "Wiki", "old page", "user")
            .unwrap();
        for id in ["edited", "gone", "doomed"] {
            a.create_record(record(id, "old", 10)).unwrap();
        }
        push(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        // A deletes a record but leaves before pushing it.
        a.delete_record("doomed").unwrap();
        a.set_replication(false).unwrap();

        // Meanwhile B edits, deletes, renames and writes a newer brief.
        b.modify_record("edited", |r| r.body = "new".into())
            .unwrap();
        b.delete_record("gone").unwrap();
        b.apply(project_at("p", "renamed", crate::now_ms() + 10))
            .unwrap();
        b.put_brief("p", "new brief", "user").unwrap();
        b.update_wiki_page("p", "w", "Wiki", "new page", "user")
            .unwrap();
        push(&b, "B", &hub, "H");

        // A pairs again: everything it has is queued, and pulled over.
        assert!(a.set_replication(true).unwrap() > 0);
        push(&a, "A", &hub, "H");
        pull(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        for s in [&hub, &a, &b] {
            assert_eq!(s.get_record("edited").unwrap().unwrap().body, "new");
            assert!(s.get_record("gone").unwrap().is_none());
            assert!(s.get_record("doomed").unwrap().is_none());
            assert_eq!(name_of(s, "p"), "renamed");
            assert_eq!(s.get_brief("p").unwrap().unwrap().body_md, "new brief");
            assert_eq!(
                s.get_wiki_page("p", "w").unwrap().unwrap().body_md,
                "new page"
            );
        }
        // A replayed copy of a deleted record does not bring it back.
        let late = [wire(
            1000,
            &Change::Record(record("gone", "zombie", 1 << 50)),
        )];
        hub.hub_ingest("H", "A", &late).unwrap();
        pull(&b, "B", &hub, "H");
        assert!(hub.get_record("gone").unwrap().is_none());
        assert!(b.get_record("gone").unwrap().is_none());
    }

    // Two machines writing the next brief version at once: both versions
    // stay in every history (append-only, unique ids), and the current
    // brief is the one later in hub order everywhere.
    #[test]
    fn concurrent_brief_versions_both_survive() {
        let (_h, hub) = temp_store();
        let (_a, a) = temp_store();
        let (_b, b) = temp_store();
        a.apply(project("p", "shared")).unwrap();
        a.put_brief("p", "v1", "user").unwrap();
        push(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        a.put_brief("p", "from a", "user").unwrap();
        b.put_brief("p", "from b", "user").unwrap();
        push(&a, "A", &hub, "H");
        push(&b, "B", &hub, "H");
        pull(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        pull(&a, "A", &hub, "H");
        for s in [&hub, &a, &b] {
            let mut bodies: Vec<String> = s
                .brief_history("p")
                .unwrap()
                .into_iter()
                .map(|h| h.body_md)
                .collect();
            bodies.sort();
            assert_eq!(bodies, ["from a", "from b", "v1"]);
            let cur = s.get_brief("p").unwrap().unwrap();
            assert_eq!(cur.body_md, "from b");
            assert_eq!(
                s.brief_history("p")
                    .unwrap()
                    .iter()
                    .map(|h| h.version)
                    .collect::<Vec<_>>(),
                [3, 2, 1]
            );
        }
    }

    // A deleted session never comes back: not from a late write of its
    // machine, not from another machine's unpushed retitle, not by events.
    #[test]
    fn deleted_sessions_stay_deleted() {
        use crate::model::{Event, EventKind};
        let (_h, hub) = temp_store();
        let (_a, a) = temp_store();
        let (_b, b) = temp_store();
        a.apply(project("p", "shared")).unwrap();
        let s = session_of("s1", "A");
        a.apply(Change::Session(s.clone())).unwrap();
        push(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        // B retitles it (unpushed) while A deletes it.
        b.apply(Change::Session(crate::model::Session {
            title: Some("renamed".into()),
            ..s.clone()
        }))
        .unwrap();
        a.delete_session("s1").unwrap();
        push(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        assert!(b.get_session("s1").unwrap().is_none(), "delete wins on B");
        push(&b, "B", &hub, "H");
        let ev = Event {
            session_id: "s1".into(),
            seq: 1,
            ts: 1,
            kind: EventKind::User,
            text: "late".into(),
            meta: None,
        };
        // A late write on the owner is ignored and not queued.
        let head = a.outbox_head().unwrap();
        assert!(!a.apply(Change::Session(s.clone())).unwrap());
        assert!(!a.apply(Change::Event(ev.clone())).unwrap());
        assert_eq!(a.outbox_head().unwrap(), head);
        // And replicated ones are ignored everywhere.
        let late = [
            wire(1000, &Change::Session(s.clone())),
            wire(1001, &Change::Event(ev)),
        ];
        hub.hub_ingest("H", "A", &late).unwrap();
        pull(&b, "B", &hub, "H");
        for st in [&hub, &a, &b] {
            assert!(st.get_session("s1").unwrap().is_none());
            assert_eq!(st.max_event_seq("s1").unwrap(), 0);
        }
    }

    // Entries the hub logged are dropped from the outbox once a pull passed
    // them (node) or once logged (hub); hub_log keeps them for pulls.
    #[test]
    fn acknowledged_outbox_entries_are_pruned() {
        let (_h, hub) = temp_store();
        let (_a, a) = temp_store();
        for i in 0..3 {
            a.apply(project(&format!("p{i}"), "x")).unwrap();
        }
        hub.apply(project("h", "hub")).unwrap();
        push(&a, "A", &hub, "H");
        assert_eq!(outbox_len(&a), 3, "pushed, not yet pulled past");
        pull(&a, "A", &hub, "H");
        assert_eq!(outbox_len(&a), 0);
        assert_eq!(outbox_len(&hub), 0);
        assert_eq!(hub.hub_head().unwrap(), 4);
        // Unpushed writes stay.
        a.apply(project("p9", "later")).unwrap();
        pull(&a, "A", &hub, "H");
        assert_eq!(outbox_len(&a), 1);
    }

    // A standalone machine queues nothing; pairing queues what exists:
    // everything but events at once, the events that exist then in
    // resumable batches.
    #[test]
    fn standalone_queues_nothing_and_pairing_backfills() {
        use crate::model::{Event, EventKind};
        let (dir, s) = temp_store();
        s.set_setting(super::super::MACHINE_ID_KEY, &serde_json::json!("A"))
            .unwrap();
        s.apply(project("early", "x")).unwrap();
        s.set_replication(false).unwrap();
        assert_eq!(outbox_len(&s), 0, "emptied");
        s.upsert_machine(&Machine {
            id: "A".into(),
            name: "a".into(),
            os: "x".into(),
            role: MachineRole::Standalone,
            last_seen: 1,
            revoked: false,
        })
        .unwrap();
        s.apply(project("p", "shared")).unwrap();
        for (id, m) in [("mine", "A"), ("theirs", "B"), ("gone", "A")] {
            s.apply(Change::Session(crate::model::Session {
                agent_session_id: Some(id.into()),
                ..session_of(id, m)
            }))
            .unwrap();
        }
        s.delete_session("gone").unwrap();
        for (sid, seq) in [("mine", 1), ("mine", 2), ("mine", 3), ("theirs", 1)] {
            s.apply(Change::Event(Event {
                session_id: sid.into(),
                seq,
                ts: 1,
                kind: EventKind::User,
                text: "t".into(),
                meta: None,
            }))
            .unwrap();
        }
        s.put_brief("p", "one", "user").unwrap();
        s.put_brief("p", "two", "user").unwrap();
        assert_eq!(outbox_len(&s), 0, "standalone queues nothing");

        let n = s.set_replication(true).unwrap();
        let kinds = |s: &Store| -> Vec<(String, String)> {
            s.outbox_after(0, 1000)
                .unwrap()
                .into_iter()
                .map(|e| (e.entity, e.key))
                .collect()
        };
        let q = kinds(&s);
        assert_eq!(q.len(), n);
        for want in [
            ("machines", "A"),
            ("projects", "early"),
            ("projects", "p"),
            ("sessions", "mine"),
            ("sessions", "gone"),
        ] {
            assert!(
                q.iter().any(|(e, k)| (e.as_str(), k.as_str()) == want),
                "{want:?} in {q:?}"
            );
        }
        assert!(
            !q.iter().any(|(_, k)| k == "theirs"),
            "other machines' rows stay"
        );
        assert_eq!(q.iter().filter(|(e, _)| e == "briefs").count(), 3);
        assert!(!q.iter().any(|(e, _)| e == "events"), "events come later");
        // Live writes are queued again, events too (and only once).
        s.apply(project("live", "x")).unwrap();
        s.apply(Change::Event(Event {
            session_id: "mine".into(),
            seq: 4,
            ts: 1,
            kind: EventKind::User,
            text: "live".into(),
            meta: None,
        }))
        .unwrap();

        assert_eq!(s.backfill_events(2).unwrap(), 2);
        // Resumes from the stored position (e.g. after a restart).
        drop(s);
        let s = Store::open(&dir.path().join("blirp.db")).unwrap();
        assert_eq!(s.backfill_events(2).unwrap(), 1);
        assert_eq!(s.backfill_events(2).unwrap(), 0);
        let events: Vec<String> = kinds(&s)
            .into_iter()
            .filter(|(e, _)| e == "events")
            .map(|(_, k)| k)
            .collect();
        assert_eq!(events, ["mine\n4", "mine\n1", "mine\n2", "mine\n3"]);
        assert!(kinds(&s).iter().any(|(_, k)| k == "live"));
    }

    #[test]
    fn malformed_and_spoofed_entries_are_rejected() {
        let (_h, hub) = temp_store();
        let spoof = Change::Machine(Machine {
            id: "H".into(),
            name: "evil".into(),
            os: "x".into(),
            role: MachineRole::Hub,
            last_seen: 1,
            revoked: true,
        });
        let entries = vec![
            WireEntry {
                origin_seq: 1,
                entity: "machines".into(),
                op: "upsert".into(),
                key: "H".into(),
                payload_json: serde_json::to_string(&spoof).unwrap(),
                ts: 1,
            },
            WireEntry {
                origin_seq: 2,
                entity: "projects".into(),
                op: "upsert".into(),
                key: "other".into(),
                payload_json: serde_json::to_string(&project("p", "x")).unwrap(),
                ts: 1,
            },
        ];
        let out = hub.hub_ingest("H", "A", &entries).unwrap();
        assert_eq!((out.acked, out.inserted, out.rejected), (2, 0, 2));
        assert_eq!(hub.hub_head().unwrap(), 0);
        assert!(hub.get_project("p").unwrap().is_none());
    }

    fn wire(seq: i64, change: &Change) -> WireEntry {
        let (entity, op, key) = change.describe();
        WireEntry {
            origin_seq: seq,
            entity: entity.into(),
            op: op.into(),
            key,
            payload_json: serde_json::to_string(change).unwrap(),
            ts: 1,
        }
    }

    fn session_of(id: &str, machine: &str) -> crate::model::Session {
        crate::model::Session {
            id: id.into(),
            project_id: "p".into(),
            machine_id: machine.into(),
            agent: "codex".into(),
            agent_session_id: Some("r1".into()),
            origin: crate::model::SessionOrigin::External,
            cwd: "/w".into(),
            title: None,
            status: crate::model::SessionStatus::Completed,
            branch: None,
            worktree: None,
            transcript_path: None,
            started_at: 1,
            ended_at: None,
            last_activity_at: 1,
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
    fn machines_only_write_their_own_rows() {
        use crate::model::{Event, EventKind, ProjectPath};
        let (_h, hub) = temp_store();
        hub.apply(project("p", "shared")).unwrap();
        let hs = session_of("hs", "H");
        hub.apply(Change::Session(hs.clone())).unwrap();
        let path = |m: &str, p: &str| ProjectPath {
            project_id: "p".into(),
            machine_id: m.into(),
            path: p.into(),
            git_remote: None,
        };
        hub.apply(Change::ProjectPath(path("H", "/hub"))).unwrap();

        let evil = [
            // A folder on the hub's disk (then readable via the files API).
            Change::ProjectPath(path("H", "/Users/h")),
            // A new session of the hub, or moving one to another machine.
            Change::Session(session_of("planted", "H")),
            Change::Session(crate::model::Session {
                machine_id: "A".into(),
                ..hs.clone()
            }),
            Change::DeleteSession { id: "hs".into() },
            Change::Event(Event {
                session_id: "hs".into(),
                seq: 1,
                ts: 1,
                kind: EventKind::User,
                text: "x".into(),
                meta: None,
            }),
            Change::DeleteMachine { id: "H".into() },
            // A live project's folder cannot be removed by another machine.
            Change::DeleteProjectPath {
                machine_id: "H".into(),
                path: "/hub".into(),
            },
            // Ids that would escape launch/<id>/.
            Change::Session(session_of("../../x", "A")),
        ];
        let mut entries: Vec<WireEntry> = evil
            .iter()
            .enumerate()
            .map(|(i, c)| wire(i as i64 + 1, c))
            .collect();
        let mut big = wire(100, &project("q", &"x".repeat(MAX_ENTRY_BYTES)));
        big.origin_seq = entries.len() as i64 + 1;
        entries.push(big);
        let out = hub.hub_ingest("H", "A", &entries).unwrap();
        assert_eq!(
            (out.inserted, out.rejected),
            (0, entries.len()),
            "every entry is rejected"
        );
        assert_eq!(hub.get_session("hs").unwrap().unwrap(), hs);
        assert!(hub.get_session("planted").unwrap().is_none());
        assert_eq!(hub.project_paths("p").unwrap(), [path("H", "/hub")]);

        // Allowed: A's own rows, and retitling H's session; everything else
        // in A's copy of it (what H runs, reads and derives from its
        // transcript) is replaced by H's values, also in the logged entry.
        let mut own = session_of("as", "A");
        own.cwd = "/a".into();
        let foreign_edit = crate::model::Session {
            title: Some("renamed".into()),
            agent_session_id: Some("-cnotify=[\"calc\"]".into()),
            cwd: "/".into(),
            worktree: Some("/etc".into()),
            transcript_path: Some("/etc/passwd".into()),
            summary: Some(serde_json::json!({"summary": "planted"})),
            distilled_through_seq: 1 << 40,
            tokens_in: 1,
            tokens_out: 2,
            cost_usd: 1e9,
            branch: Some("evil".into()),
            ..hs.clone()
        };
        let fine = [
            Change::Session(own),
            Change::ProjectPath(path("A", "/a")),
            Change::Session(foreign_edit),
        ];
        let entries: Vec<WireEntry> = fine
            .iter()
            .enumerate()
            .map(|(i, c)| wire(100 + i as i64, c))
            .collect();
        let out = hub.hub_ingest("H", "A", &entries).unwrap();
        assert_eq!((out.inserted, out.rejected), (fine.len(), 0));
        let renamed = crate::model::Session {
            title: Some("renamed".into()),
            ..hs.clone()
        };
        assert_eq!(hub.get_session("hs").unwrap().unwrap(), renamed);
        let logged = hub.hub_page("B", 0, 100, 4 << 20).unwrap();
        let logged = serde_json::to_string(&logged).unwrap();
        assert!(!logged.contains("planted") && !logged.contains("passwd"));

        // A deleted project's folders are dropped by every machine when it
        // applies the delete; a foreign folder removal stays refused.
        let deleted = wire(
            200,
            &Change::Project(crate::model::Project {
                id: "p".into(),
                name: "shared".into(),
                created_at: 1,
                updated_at: 2,
                deleted: true,
                chats: false,
                merged_into: None,
            }),
        );
        let remove = wire(
            201,
            &Change::DeleteProjectPath {
                machine_id: "H".into(),
                path: "/hub".into(),
            },
        );
        let out = hub.hub_ingest("H", "A", &[deleted, remove]).unwrap();
        assert_eq!((out.inserted, out.rejected), (1, 1));
        assert!(hub.project_paths("p").unwrap().is_empty());

        // A node checks pulled entries itself: the hub cannot plant a
        // folder of the node, and a third machine cannot either.
        let (_b, b) = temp_store();
        let page = HubPage {
            own_seen: 0,
            entries: vec![
                PulledEntry::Remote {
                    hub_seq: 1,
                    origin_machine: "H".into(),
                    entry: wire(1, &project("p", "shared")),
                },
                PulledEntry::Remote {
                    hub_seq: 2,
                    origin_machine: "H".into(),
                    entry: wire(2, &Change::ProjectPath(path("B", "/etc"))),
                },
                PulledEntry::Remote {
                    hub_seq: 3,
                    origin_machine: "C".into(),
                    entry: wire(1, &Change::Session(session_of("cs", "B"))),
                },
            ],
            up_to: 3,
            more: false,
        };
        assert_eq!(b.node_apply_pull("H", &page).unwrap(), 1);
        assert!(b.project_paths("p").unwrap().is_empty());
        assert!(b.get_session("cs").unwrap().is_none());
    }

    // A hub labelling an entry with the node's own id would pass every
    // ownership check (a folder of the node at "/", then readable through
    // the files API; rewritten sessions): the node refuses such entries.
    #[test]
    fn pulled_entries_claiming_this_machine_are_refused() {
        use crate::model::ProjectPath;
        let (_b, b) = temp_store();
        b.set_setting(super::super::MACHINE_ID_KEY, &serde_json::json!("B"))
            .unwrap();
        let mine = session_of("bs", "B");
        b.apply(project_at("p", "shared", 10)).unwrap();
        b.apply(Change::Session(mine.clone())).unwrap();
        let root = Change::ProjectPath(ProjectPath {
            project_id: "p".into(),
            machine_id: "B".into(),
            path: "/".into(),
            git_remote: None,
        });
        let hijack = Change::Session(crate::model::Session {
            agent_session_id: Some("-cnotify=[\"calc\"]".into()),
            ..mine.clone()
        });
        let entries = [project_at("p", "renamed", 20), root, hijack];
        for origin in ["B", ""] {
            let page = HubPage {
                own_seen: 0,
                entries: entries
                    .iter()
                    .enumerate()
                    .map(|(i, c)| PulledEntry::Remote {
                        hub_seq: i as i64 + 1,
                        origin_machine: origin.into(),
                        entry: wire(i as i64 + 1, c),
                    })
                    .collect(),
                up_to: 3,
                more: false,
            };
            let hub = format!("hub{origin}");
            assert_eq!(b.node_apply_pull(&hub, &page).unwrap(), 0);
        }
        assert_eq!(name_of(&b, "p"), "shared");
        assert!(b.project_paths("p").unwrap().is_empty());
        assert_eq!(b.get_session("bs").unwrap().unwrap(), mine);
    }

    // `own_seen` comes from the hub: a pull never prunes outbox entries the
    // hub has not acknowledged, whatever it claims to have seen.
    #[test]
    fn pull_never_prunes_unacknowledged_entries() {
        let (_a, a) = temp_store();
        a.apply(project("p1", "x")).unwrap();
        a.apply(project("p2", "x")).unwrap();
        age(&a);
        a.set_pushed_cursor("H", 1).unwrap();
        let page = HubPage {
            own_seen: 100,
            entries: vec![],
            up_to: 1,
            more: false,
        };
        a.node_apply_pull("H", &page).unwrap();
        let left: Vec<i64> = a
            .outbox_after(0, 10)
            .unwrap()
            .iter()
            .map(|e| e.origin_seq)
            .collect();
        assert_eq!(left, [2]);
    }

    #[test]
    fn rebind_moves_rows_and_drops_old_machine() {
        let (dir, store) = temp_store();
        store
            .upsert_machine(&Machine {
                id: "old".into(),
                name: "box".into(),
                os: "x".into(),
                role: MachineRole::Standalone,
                last_seen: 1,
                revoked: false,
            })
            .unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir(&proj).unwrap();
        let r = store.register_project("old", &proj, None).unwrap();
        let s = crate::model::Session {
            id: "s1".into(),
            project_id: r.id.clone(),
            machine_id: "old".into(),
            agent: "shell".into(),
            agent_session_id: None,
            origin: crate::model::SessionOrigin::Blirp,
            cwd: proj.display().to_string(),
            title: None,
            status: crate::model::SessionStatus::Completed,
            branch: None,
            worktree: None,
            transcript_path: None,
            started_at: 1,
            ended_at: None,
            last_activity_at: 1,
            exit_code: None,
            summary: None,
            distilled_through_seq: 0,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            parent_session_id: None,
            stopped_by_user: false,
        };
        store.insert_session(&s).unwrap();
        store.rebind_machine("old", "new").unwrap();
        assert!(store.get_machine("old").unwrap().is_none());
        assert_eq!(store.get_session("s1").unwrap().unwrap().machine_id, "new");
        assert_eq!(store.local_roots(&r.id, "new").unwrap().len(), 1);
        assert!(store.local_roots(&r.id, "old").unwrap().is_empty());
    }

    fn machine_device(hub: &Store, node: &str, revoked: bool) {
        hub.upsert_device(&crate::model::Device {
            id: format!("d{node}"),
            name: node.into(),
            kind: crate::model::DeviceKind::Machine,
            token_hash: None,
            node_id: Some(node.into()),
            created_at: 1,
            last_seen: 1,
            revoked,
            can_control_terminals: true,
        })
        .unwrap();
    }

    fn event(session: &str, seq: i64) -> crate::model::Event {
        crate::model::Event {
            session_id: session.into(),
            seq,
            ts: seq,
            kind: crate::model::EventKind::User,
            text: format!("{session} event {seq}"),
            meta: Some(serde_json::json!({"n": seq})),
        }
    }

    fn events_of(s: &Store, session: &str) -> Vec<crate::model::Event> {
        s.events_page(session, 0, 1000).unwrap().0
    }

    /// (hub_seq, entity, payload stripped) of every logged row.
    fn log_rows(hub: &Store) -> Vec<(i64, String, bool)> {
        hub.read(|c| {
            all(
                c,
                "SELECT hub_seq, entity, payload_json = '' FROM hub_log ORDER BY hub_seq",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })
        .unwrap()
    }

    // Events are logged once (in `events`) and still pulled in full;
    // compacting superseded rows leaves every pull with the hub's state:
    // a machine replaying from the start, deleted sessions, events exactly
    // once, and each origin's `own_seen`.
    #[test]
    fn compacted_log_replays_to_the_hub_state() {
        let (_h, hub) = temp_store();
        let (_a, a) = temp_store();
        let (_b, b) = temp_store();
        machine_device(&hub, "A", false);
        machine_device(&hub, "B", false);
        a.apply(project("p", "shared")).unwrap();
        let machine = |name: &str| {
            Change::Machine(Machine {
                id: "A".into(),
                name: name.into(),
                os: "linux".into(),
                role: MachineRole::Node,
                last_seen: 1,
                revoked: false,
            })
        };
        a.apply(machine("first")).unwrap();
        let s1 = session_of("s1", "A");
        for i in 0..4 {
            a.apply(Change::Session(crate::model::Session {
                title: Some(format!("t{i}")),
                tokens_in: i,
                ..s1.clone()
            }))
            .unwrap();
        }
        for seq in 1..=3 {
            a.apply(Change::Event(event("s1", seq))).unwrap();
        }
        a.apply(Change::Session(crate::model::Session {
            agent_session_id: Some("r2".into()),
            ..session_of("s2", "A")
        }))
        .unwrap();
        a.apply(Change::Event(event("s2", 1))).unwrap();
        a.apply(machine("second")).unwrap();
        push(&a, "A", &hub, "H");
        // Event payloads are not stored twice.
        let stored: Vec<bool> = log_rows(&hub)
            .into_iter()
            .filter(|(_, e, _)| e == "events")
            .map(|(.., stripped)| stripped)
            .collect();
        assert_eq!(stored, [true; 4]);

        pull(&b, "B", &hub, "H");
        // A writes s1 once more and deletes s2; then B retitles s1 (a
        // foreign write after A's last one).
        a.apply(Change::Session(crate::model::Session {
            title: Some("owner last".into()),
            tokens_in: 9,
            ..s1.clone()
        }))
        .unwrap();
        a.delete_session("s2").unwrap();
        push(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        let seen = b.get_session("s1").unwrap().unwrap();
        b.apply(Change::Session(crate::model::Session {
            title: Some("from b".into()),
            ..seen
        }))
        .unwrap();
        push(&b, "B", &hub, "H");
        pull(&a, "A", &hub, "H");
        // A's last write hides B's retitle (B's only logged row: it stays
        // as B's marker); the hub's retitle after it stays.
        a.apply(Change::Session(crate::model::Session {
            title: Some("owner final".into()),
            ..a.get_session("s1").unwrap().unwrap()
        }))
        .unwrap();
        push(&a, "A", &hub, "H");
        hub.apply(Change::Session(crate::model::Session {
            title: Some("from hub".into()),
            ..hub.get_session("s1").unwrap().unwrap()
        }))
        .unwrap();
        pull(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        // Both nodes ask for the head once more: the floor is there.
        pull(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        let head = hub.hub_head().unwrap();
        let own_seen = |who: &str| hub.hub_page(who, head, 10, 4 << 20).unwrap().own_seen;
        let before = (own_seen("A"), own_seen("B"));
        let rows = log_rows(&hub).len();

        let done = hub.compact_hub_log(2).unwrap();
        // s1's upserts between its first and A's last one. First upserts
        // stay (s1's events need it; so does the first machine row), and
        // s2's is followed by its delete, not by a newer upsert.
        assert_eq!((done.removed, done.stripped), (4, 1), "{done:?}");
        assert_eq!(log_rows(&hub).len(), rows - done.removed);
        assert_eq!((own_seen("A"), own_seen("B")), before);
        assert_eq!(hub.compact_hub_log(2).unwrap(), Compacted::default());

        // A machine replaying from the start ends where the hub is.
        let (_c, c) = temp_store();
        let page = hub.hub_page("C", 0, 1000, 4 << 20).unwrap();
        let events = page
            .entries
            .iter()
            .filter(|e| matches!(e, PulledEntry::Remote { entry, .. } if entry.entity == "events"))
            .count();
        assert_eq!(events, 3, "s1's events once, none of deleted s2");
        assert!(
            !page.entries.iter().any(
                |e| matches!(e, PulledEntry::Remote { origin_machine, .. } if origin_machine == "B")
            ),
            "B's compacted row is only B's marker"
        );
        let marker = hub.hub_page("B", 0, 1000, 4 << 20).unwrap();
        assert!(
            marker
                .entries
                .iter()
                .any(|e| matches!(e, PulledEntry::Own { .. }))
        );
        pull(&c, "C", &hub, "H");
        let s1_hub = hub.get_session("s1").unwrap().unwrap();
        assert_eq!(s1_hub.title.as_deref(), Some("from hub"));
        assert_eq!(s1_hub.tokens_in, 9);
        for st in [&a, &b, &c] {
            assert_eq!(st.get_session("s1").unwrap().unwrap(), s1_hub);
            assert!(st.get_session("s2").unwrap().is_none());
            assert!(events_of(st, "s2").is_empty());
            assert_eq!(st.get_machine("A").unwrap().unwrap().name, "second");
        }
        assert_eq!(events_of(&c, "s1"), events_of(&hub, "s1"));
        assert_eq!(events_of(&c, "s1").len(), 3);
        assert_eq!(
            events_of(&c, "s1")[2].meta,
            Some(serde_json::json!({"n": 3}))
        );
    }

    // Compaction stops at the lowest pull position of an active node, which
    // still gets the latest state; a revoked node does not hold it back.
    #[test]
    fn compaction_floor_is_the_slowest_active_node() {
        let (_h, hub) = temp_store();
        let (_a, a) = temp_store();
        let (_b, b) = temp_store();
        machine_device(&hub, "A", false);
        machine_device(&hub, "B", false);
        hub.hub_record_pull("R", 0).unwrap();
        machine_device(&hub, "R", true);
        a.apply(project("p", "shared")).unwrap();
        let s1 = session_of("s1", "A");
        let title = |i: i64| {
            Change::Session(crate::model::Session {
                title: Some(format!("t{i}")),
                ..s1.clone()
            })
        };
        for i in 0..3 {
            a.apply(title(i)).unwrap();
        }
        push(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        pull(&b, "B", &hub, "H");
        let slow = b.sync_cursors("H").unwrap().last_pulled_hub_seq;
        for i in 3..6 {
            a.apply(title(i)).unwrap();
        }
        push(&a, "A", &hub, "H");
        pull(&a, "A", &hub, "H");
        pull(&a, "A", &hub, "H");
        let above = |hub: &Store| -> Vec<(i64, String, bool)> {
            log_rows(hub)
                .into_iter()
                .filter(|(seq, ..)| *seq > slow)
                .collect()
        };
        let untouched = above(&hub);

        // Below B's position: t0 is the first upsert and t2 the latest
        // there (both kept), t1 goes.
        let done = hub.compact_hub_log(1000).unwrap();
        assert_eq!(done.removed + done.stripped, 1, "{done:?}");
        assert_eq!(above(&hub), untouched, "nothing past B was touched");
        pull(&b, "B", &hub, "H");
        assert_eq!(
            b.get_session("s1").unwrap().unwrap().title.as_deref(),
            Some("t5")
        );

        // B asked for the head: everything up to it compacts.
        pull(&b, "B", &hub, "H");
        let done = hub.compact_hub_log(1000).unwrap();
        assert_eq!(done.removed + done.stripped, 3, "{done:?}");
        let (_c, c) = temp_store();
        pull(&c, "C", &hub, "H");
        assert_eq!(
            c.get_session("s1").unwrap().unwrap(),
            hub.get_session("s1").unwrap().unwrap()
        );

        // A paired node that has not pulled yet holds compaction at 0.
        machine_device(&hub, "N", false);
        a.apply(title(6)).unwrap();
        a.apply(title(7)).unwrap();
        push(&a, "A", &hub, "H");
        pull(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        assert_eq!(hub.compact_hub_log(1000).unwrap(), Compacted::default());
    }

    // Hub databases from before migration 9 keep their logged events; the
    // payload moves out of `hub_log` and pulls read it from `events`.
    #[test]
    fn migration_strips_logged_event_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blirp.db");
        let ev = event("s1", 1);
        let change = Change::Event(ev.clone());
        let proj = project("p", "shared");
        {
            let c = Connection::open(&path).unwrap();
            for sql in &super::super::migrations::MIGRATIONS[..8] {
                c.execute_batch(sql).unwrap();
            }
            c.pragma_update(None, "user_version", 8).unwrap();
            c.execute(
                "INSERT INTO events(session_id, seq, ts, kind, text, meta_json)
                 VALUES ('s1', 1, 1, 'user', ?1, ?2)",
                params![ev.text, r#"{"n":1}"#],
            )
            .unwrap();
            for (seq, ch) in [(1, &proj), (2, &change)] {
                let w = wire(seq, ch);
                c.execute(
                    "INSERT INTO hub_log(origin_machine, origin_seq, entity, op, key, payload_json, ts)
                     VALUES ('A', ?1, ?2, ?3, ?4, ?5, 1)",
                    params![w.origin_seq, w.entity, w.op, w.key, w.payload_json],
                )
                .unwrap();
            }
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            log_rows(&store),
            [(1, "projects".into(), false), (2, "events".into(), true)]
        );
        let page = store.hub_page("B", 0, 100, 4 << 20).unwrap();
        let changes: Vec<Change> = page
            .entries
            .iter()
            .map(|e| match e {
                PulledEntry::Remote { entry, .. } => entry.change().unwrap(),
                PulledEntry::Own { .. } => panic!("no own entries"),
            })
            .collect();
        assert_eq!(changes, [proj, change]);
    }

    // A batch pushed again after its rows were compacted away (the node
    // never saw the ack) is not logged again: the cursor dedups it.
    #[test]
    fn repushed_batch_after_compaction_is_ignored() {
        let (_h, hub) = temp_store();
        let (_a, a) = temp_store();
        machine_device(&hub, "A", false);
        a.apply(project("p", "shared")).unwrap();
        let s1 = session_of("s1", "A");
        for i in 0..4 {
            a.apply(Change::Session(crate::model::Session {
                title: Some(format!("t{i}")),
                ..s1.clone()
            }))
            .unwrap();
        }
        let batch = a.outbox_batch(0, 500, 4 << 20).unwrap();
        push(&a, "A", &hub, "H");
        pull(&a, "A", &hub, "H");
        pull(&a, "A", &hub, "H");
        let done = hub.compact_hub_log(1000).unwrap();
        assert!(done.removed > 0, "{done:?}");
        let rows = log_rows(&hub);
        let out = hub.hub_ingest("H", "A", &batch).unwrap();
        assert_eq!(
            (out.inserted, out.acked),
            (0, batch.last().unwrap().origin_seq)
        );
        assert_eq!(log_rows(&hub), rows);
        assert_eq!(
            hub.get_session("s1").unwrap().unwrap().title.as_deref(),
            Some("t3")
        );
    }

    // A node whose cursor went back behind the compacted log (its database
    // lost recent writes) is refused instead of silently diverging; a new
    // or re-paired node is not.
    #[test]
    fn pull_cursor_going_back_behind_compaction_is_refused() {
        let (_h, hub) = temp_store();
        let (_a, a) = temp_store();
        machine_device(&hub, "A", false);
        a.apply(project("p", "shared")).unwrap();
        let s1 = session_of("s1", "A");
        for i in 0..4 {
            a.apply(Change::Session(crate::model::Session {
                title: Some(format!("t{i}")),
                ..s1.clone()
            }))
            .unwrap();
        }
        push(&a, "A", &hub, "H");
        pull(&a, "A", &hub, "H");
        pull(&a, "A", &hub, "H");
        let head = hub.hub_head().unwrap();
        // Going back is harmless while nothing was compacted.
        assert!(hub.hub_record_pull("A", 1).unwrap());
        assert!(hub.hub_record_pull("A", head).unwrap());
        assert!(hub.compact_hub_log(1000).unwrap().removed > 0);
        assert!(!hub.hub_record_pull("A", 1).unwrap());
        assert!(hub.hub_record_pull("A", head).unwrap(), "forward is fine");
        assert!(hub.hub_record_pull("NEW", 0).unwrap(), "never pulled");
        // Revoked (left) and paired again: it starts from its cursor.
        hub.hub_forget_pull("A").unwrap();
        assert!(hub.hub_record_pull("A", 1).unwrap());
    }

    // Folder upserts compact by their owner (the machine in the key).
    #[test]
    fn folder_upserts_compact_by_owner() {
        let (_h, hub) = temp_store();
        let (_a, a) = temp_store();
        machine_device(&hub, "A", false);
        a.apply(project("p", "shared")).unwrap();
        a.apply(project("q", "other")).unwrap();
        let folder = |project: &str, remote: Option<&str>| {
            Change::ProjectPath(crate::model::ProjectPath {
                project_id: project.into(),
                machine_id: "A".into(),
                path: "/w".into(),
                git_remote: remote.map(Into::into),
            })
        };
        a.apply(folder("p", None)).unwrap();
        a.apply(folder("p", Some("host/o/r"))).unwrap();
        a.apply(folder("q", Some("host/o/r2"))).unwrap();
        a.apply(folder("q", Some("host/o/r3"))).unwrap();
        push(&a, "A", &hub, "H");
        pull(&a, "A", &hub, "H");
        pull(&a, "A", &hub, "H");
        // The first and the last stay; the two between go.
        let done = hub.compact_hub_log(1000).unwrap();
        assert_eq!(done.removed + done.stripped, 2, "{done:?}");
        let (_c, c) = temp_store();
        pull(&c, "C", &hub, "H");
        let roots = |s: &Store| {
            s.read(|c| {
                all(
                    c,
                    "SELECT project_id, git_remote FROM project_paths WHERE machine_id = 'A'",
                    [],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )
            })
            .unwrap()
        };
        assert_eq!(
            roots(&c),
            [("q".to_string(), Some("host/o/r3".to_string()))]
        );
        assert_eq!(roots(&c), roots(&hub));
    }
}
