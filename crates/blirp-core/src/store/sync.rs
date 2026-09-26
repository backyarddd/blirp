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
use rusqlite::{Connection, Transaction, params};
use serde::{Deserialize, Serialize};

/// Largest accepted replicated entry. API bodies are capped at 2 MB, so a
/// legitimate entry stays below this even with JSON escaping.
pub const MAX_ENTRY_BYTES: usize = 4 << 20;

/// Session fields only the session's own machine may set: they decide what
/// that machine runs and reads (resume argv, folder, worktree, transcript).
fn same_owned_fields(a: &Session, b: &Session) -> bool {
    a.machine_id == b.machine_id
        && a.agent == b.agent
        && a.agent_session_id == b.agent_session_id
        && a.origin == b.origin
        && a.cwd == b.cwd
        && a.worktree == b.worktree
        && a.transcript_path == b.transcript_path
        && a.parent_session_id == b.parent_session_id
}

/// §10 ownership: a machine's folders, sessions and their events are
/// written only by that machine. Another machine may only re-point an
/// existing folder or session to another project (merge), retitle a
/// session or change its status fields, and unregister folders of a deleted
/// project. `hub` is set when a node applies a pull: the hub also writes
/// the machine rows of the nodes it pairs and revokes. Records, briefs,
/// wiki pages, resources and projects are shared by design.
fn check_owner(
    c: &Connection,
    origin: &str,
    hub: Option<&str>,
    change: &Change,
) -> std::result::Result<(), String> {
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
                Some(old) if old.git_remote == p.git_remote => Ok(()),
                _ => Err(format!("folder of machine {}", p.machine_id)),
            }
        }
        Change::DeleteProjectPath { machine_id, path } if foreign(machine_id) => {
            let Some(old) = path_project(machine_id, path)? else {
                return Ok(());
            };
            let deleted: bool = one(
                c,
                "SELECT deleted FROM projects WHERE id = ?1",
                params![old.project_id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?
            .unwrap_or(true);
            if deleted {
                Ok(())
            } else {
                Err(format!("folder removal on machine {machine_id}"))
            }
        }
        Change::Session(s) => match session(&s.id)? {
            Some(old) if !foreign(&old.machine_id) && !foreign(&s.machine_id) => Ok(()),
            Some(old) if same_owned_fields(&old, s) => Ok(()),
            Some(old) => Err(format!("session of machine {}", old.machine_id)),
            None if !foreign(&s.machine_id) => Ok(()),
            None => Err(format!("new session for machine {}", s.machine_id)),
        },
        Change::DeleteSession { id } => match session(id)? {
            Some(old) if foreign(&old.machine_id) => {
                Err(format!("delete of a session of machine {}", old.machine_id))
            }
            _ => Ok(()),
        },
        Change::Event(e) => match session(&e.session_id)? {
            Some(s) if !foreign(&s.machine_id) => Ok(()),
            Some(s) => Err(format!("event of a session of machine {}", s.machine_id)),
            None => Err("event of an unknown session".into()),
        },
        _ => Ok(()),
    }
}

/// Everything a replicated entry must pass before it is logged or applied.
fn check_entry(
    c: &Connection,
    origin: &str,
    hub: Option<&str>,
    e: &WireEntry,
) -> std::result::Result<Change, String> {
    if e.payload_json.len() > MAX_ENTRY_BYTES {
        return Err(format!("payload of {} bytes", e.payload_json.len()));
    }
    let change = e.change().map_err(|err| err.to_string())?;
    check_ids(&change).map_err(|err| err.to_string())?;
    check_owner(c, origin, hub, &change)?;
    Ok(change)
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
         SELECT ?1, origin_seq, entity, op, key, payload_json, ts FROM outbox
         WHERE origin_seq > ?2 ORDER BY origin_seq",
        params![own, cur.last_pushed_origin_seq],
    )?;
    let head: i64 = tx.query_row("SELECT coalesce(max(origin_seq), 0) FROM outbox", [], |r| {
        r.get(0)
    })?;
    if head > cur.last_pushed_origin_seq {
        cur.last_pushed_origin_seq = head;
        set_cursors_in(tx, own, cur)?;
    }
    Ok(n)
}

impl Store {
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
                acked = acked.max(e.origin_seq);
                let change = match check_entry(tx, origin, None, e) {
                    Ok(c) => c,
                    Err(err) => {
                        tracing::warn!(origin, origin_seq = e.origin_seq, entity = %e.entity, error = %err, "rejecting replicated entry");
                        rejected += 1;
                        continue;
                    }
                };
                let n = tx.execute(
                    "INSERT OR IGNORE INTO hub_log(origin_machine, origin_seq, entity, op, key, payload_json, ts)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![origin, e.origin_seq, e.entity, e.op, e.key, e.payload_json, e.ts],
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
            let limit = max_entries as i64 + 1;
            let mut rows = st.query(params![after, limit])?;
            let mut entries = Vec::new();
            let mut bytes = 0;
            let mut up_to = after;
            let mut more = false;
            while let Some(r) = rows.next()? {
                if entries.len() == max_entries {
                    more = true;
                    break;
                }
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
                    let entry = WireEntry {
                        origin_seq,
                        entity: r.get(3)?,
                        op: r.get(4)?,
                        key: r.get(5)?,
                        payload_json: r.get(6)?,
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

    /// Node: apply a pull page from `hub` and advance the pull cursor in the
    /// same transaction. Returns how many remote entries were applied.
    pub fn node_apply_pull(&self, hub: &str, page: &HubPage) -> Result<usize> {
        self.write(|tx| {
            let mut cur = cursors_in(tx, hub)?;
            if page.up_to <= cur.last_pulled_hub_seq {
                return Ok(0);
            }
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
                        // The hub checked this too; a node does not rely on it.
                        let change = match check_entry(tx, origin_machine, Some(hub), entry) {
                            Ok(c) => c,
                            Err(err) => {
                                tracing::warn!(origin = %origin_machine, hub_seq, entity = %entry.entity, error = %err, "skipping rejected replicated entry");
                                continue;
                            }
                        };
                        // Events are append-only; everything else is LWW.
                        if entry.op != "insert"
                            && later_own.exists(params![entry.entity, entry.key, own_seen])?
                        {
                            // Our later brief stays current, but the other
                            // machine's version still joins the history.
                            if let Change::Brief(b) = &change {
                                super::memory::insert_brief_history(tx, b)?;
                            }
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

    fn pull(node: &Store, node_id: &str, hub: &Store, hub_id: &str) -> usize {
        hub.hub_flush_own(hub_id).unwrap();
        let mut total = 0;
        loop {
            let after = node.sync_cursors(hub_id).unwrap().last_pulled_hub_seq;
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

    #[test]
    fn last_writer_by_hub_order_converges() {
        let (_h, hub) = temp_store();
        let (_a, a) = temp_store();
        let (_b, b) = temp_store();
        a.apply(project("p", "v0")).unwrap();
        push(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");

        // B writes v1 and pushes; A writes v2 (unpushed) then pulls v1:
        // A keeps v2 because its write will be logged after v1.
        b.apply(project("p", "v1")).unwrap();
        push(&b, "B", &hub, "H");
        a.apply(project("p", "v2")).unwrap();
        pull(&a, "A", &hub, "H");
        assert_eq!(name_of(&a, "p"), "v2");
        push(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        for s in [&hub, &a, &b] {
            assert_eq!(name_of(s, "p"), "v2");
        }

        // A writes v3 and pushes before B's v4 reaches the hub, then pulls:
        // v4 is later in hub order and wins everywhere.
        a.apply(project("p", "v3")).unwrap();
        push(&a, "A", &hub, "H");
        b.apply(project("p", "v4")).unwrap();
        push(&b, "B", &hub, "H");
        pull(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        for s in [&hub, &a, &b] {
            assert_eq!(name_of(s, "p"), "v4");
        }

        // B writes v5, pushes it, but a remote v6 lands before B pulls:
        // B's own entry sits before v6, so v6 wins.
        b.apply(project("p", "v5")).unwrap();
        push(&b, "B", &hub, "H");
        a.apply(project("p", "v6")).unwrap();
        push(&a, "A", &hub, "H");
        pull(&b, "B", &hub, "H");
        pull(&a, "A", &hub, "H");
        for s in [&hub, &a, &b] {
            assert_eq!(name_of(s, "p"), "v6");
        }
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
            // A session of the hub with an option-shaped resume id.
            Change::Session(session_of("planted", "H")),
            Change::Session(crate::model::Session {
                agent_session_id: Some("-cnotify=[\"calc\"]".into()),
                ..hs.clone()
            }),
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

        // Allowed: A's own rows, retitling and re-pointing H's session (merge),
        // and dropping H's folder once the project is deleted.
        let mut own = session_of("as", "A");
        own.cwd = "/a".into();
        let fine = [
            Change::Session(own),
            Change::ProjectPath(path("A", "/a")),
            Change::Session(crate::model::Session {
                title: Some("renamed".into()),
                ..hs.clone()
            }),
            Change::Project(crate::model::Project {
                id: "p".into(),
                name: "shared".into(),
                created_at: 1,
                updated_at: 2,
                deleted: true,
            }),
            Change::DeleteProjectPath {
                machine_id: "H".into(),
                path: "/hub".into(),
            },
        ];
        let entries: Vec<WireEntry> = fine
            .iter()
            .enumerate()
            .map(|(i, c)| wire(100 + i as i64, c))
            .collect();
        let out = hub.hub_ingest("H", "A", &entries).unwrap();
        assert_eq!((out.inserted, out.rejected), (fine.len(), 0));
        assert_eq!(
            hub.get_session("hs").unwrap().unwrap().title.as_deref(),
            Some("renamed")
        );
        assert_eq!(hub.project_paths("p").unwrap(), [path("A", "/a")]);

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
}
