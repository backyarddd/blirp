//! The hub's project file tables (migration 10, docs/project-files.md):
//! roots, the current entry of every path, replaced versions, blob
//! bookkeeping and per-project modes. A commit is one transaction: each
//! change is accepted only when the path's current version equals the
//! writer's base version (compare-and-set against the single sequencer);
//! otherwise the second writer's content is kept as a conflict copy. Not
//! replicated through `hub_log`.

use super::files::{content_of, split_content};
use super::{Result, Store, all, one};
use crate::files::path as wpath;
use crate::files::scan::link_inside;
use crate::files::{
    ChangeOp, ChangeResult, EntryContent, FileChange, FilesMode, GitManifest, IndexEntry, RootInfo,
    conflict_path, conflict_prefix, is_hash, is_root_id,
};
use rusqlite::{Connection, Row, Transaction, params};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Live conflict copies kept per path; older ones move to history.
pub const MAX_CONFLICT_COPIES: usize = 10;

/// Why a whole commit was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommitRefused {
    #[error("the hub has no such root")]
    UnknownRoot,
    #[error("{0}")]
    BadRoot(String),
    /// The origin's folder has not replicated to the hub yet.
    #[error("the hub does not know this folder yet")]
    Pending,
    #[error("file sync is off for this project")]
    Off,
}

impl CommitRefused {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownRoot => "unknown_root",
            Self::BadRoot(_) => "bad_root",
            Self::Pending => "root_pending",
            Self::Off => "files_off",
        }
    }
}

/// A commit batch as the hub received it.
#[derive(Debug, Clone)]
pub struct CommitInput<'a> {
    pub root_id: &'a str,
    /// Writer, as authenticated by the connection (never from the message).
    pub machine_id: &'a str,
    /// Its name, for conflict copy names.
    pub machine_name: &'a str,
    /// Origin only: its folder, registering the root on first commit.
    pub claim_path: Option<&'a str>,
    pub manifest: Option<&'a GitManifest>,
    pub changes: &'a [FileChange],
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    pub results: Vec<ChangeResult>,
    pub head: i64,
}

fn entry_row(r: &Row<'_>) -> rusqlite::Result<IndexEntry> {
    Ok(IndexEntry {
        path: r.get("path")?,
        version: r.get("version")?,
        content: content_of(r.get("hash")?, r.get("link")?),
        size: r.get("size")?,
        mode_x: r.get("mode_x")?,
        mtime: r.get("mtime")?,
        by_machine: r.get("by_machine")?,
        at: r.get("at")?,
    })
}

fn root_info_row(r: &Row<'_>) -> rusqlite::Result<RootInfo> {
    let manifest: Option<String> = r.get("manifest_json")?;
    Ok(RootInfo {
        root_id: r.get("root_id")?,
        project_id: r.get("project_id")?,
        machine_id: r.get("machine_id")?,
        machine_name: r.get("machine_name")?,
        origin_revoked: r.get("revoked")?,
        path: r.get("path")?,
        head: r.get("head")?,
        files: r.get("files")?,
        bytes: r.get("bytes")?,
        conflicts: r.get("conflicts")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        manifest: manifest.and_then(|m| serde_json::from_str(&m).ok()),
    })
}

const ROOTS_SQL: &str = "SELECT r.root_id, coalesce(pp.project_id, r.project_id) AS project_id,
       r.machine_id, coalesce(m.name, '') AS machine_name, coalesce(m.revoked, 0) AS revoked,
       r.path, r.head, r.created_at, r.updated_at, r.manifest_json,
       (SELECT count(*) FROM file_entries e WHERE e.root_id = r.root_id
          AND (e.hash IS NOT NULL OR e.link IS NOT NULL)) AS files,
       (SELECT coalesce(sum(e.size), 0) FROM file_entries e WHERE e.root_id = r.root_id
          AND e.hash IS NOT NULL) AS bytes,
       (SELECT count(*) FROM file_entries e WHERE e.root_id = r.root_id
          AND (e.hash IS NOT NULL OR e.link IS NOT NULL) AND e.path GLOB '*.conflict-*') AS conflicts
     FROM file_roots r
     LEFT JOIN project_paths pp ON pp.machine_id = r.machine_id AND pp.path = r.path
     LEFT JOIN machines m ON m.id = r.machine_id";

fn get_entry(c: &Connection, root: &str, path: &str) -> Result<Option<IndexEntry>> {
    one(
        c,
        "SELECT * FROM file_entries WHERE root_id = ?1 AND path = ?2",
        params![root, path],
        entry_row,
    )
}

fn mode_in(c: &Connection, project: &str) -> Result<FilesMode> {
    let m: Option<String> = one(
        c,
        "SELECT mode FROM file_projects WHERE project_id = ?1",
        params![project],
        |r| r.get(0),
    )?;
    Ok(m.and_then(|m| FilesMode::parse(&m)).unwrap_or_default())
}

/// Keep the replaced version of `cur` in history.
fn to_history(tx: &Transaction<'_>, root: &str, cur: &IndexEntry, now: i64) -> Result<()> {
    if cur.content.is_none() {
        return Ok(());
    }
    let (hash, link) = split_content(cur.content.as_ref());
    tx.execute(
        "INSERT OR IGNORE INTO file_history(root_id, path, version, hash, link, size, mode_x, mtime,
           by_machine, at, replaced_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            root, cur.path, cur.version, hash, link, cur.size, cur.mode_x, cur.mtime, cur.by_machine,
            cur.at, now
        ],
    )?;
    Ok(())
}

fn put_entry(tx: &Transaction<'_>, root: &str, e: &IndexEntry) -> Result<()> {
    let (hash, link) = split_content(e.content.as_ref());
    tx.execute(
        "INSERT INTO file_entries(root_id, path, version, hash, link, size, mode_x, mtime, by_machine, at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(root_id, path) DO UPDATE SET version = excluded.version, hash = excluded.hash,
           link = excluded.link, size = excluded.size, mode_x = excluded.mode_x,
           mtime = excluded.mtime, by_machine = excluded.by_machine, at = excluded.at",
        params![root, e.path, e.version, hash, link, e.size, e.mode_x, e.mtime, e.by_machine, e.at],
    )?;
    Ok(())
}

struct Committer<'a, 'b> {
    tx: &'a Transaction<'b>,
    root: &'a str,
    input: &'a CommitInput<'a>,
    head: i64,
    /// The root's origin machine: only it may forget a path.
    origin: &'a str,
}

impl Committer<'_, '_> {
    fn next(&mut self) -> i64 {
        self.head += 1;
        self.head
    }

    fn write(&mut self, cur: Option<&IndexEntry>, mut e: IndexEntry) -> Result<i64> {
        if let Some(cur) = cur {
            to_history(self.tx, self.root, cur, self.input.now)?;
        }
        e.version = self.next();
        put_entry(self.tx, self.root, &e)?;
        Ok(e.version)
    }

    fn entry(
        &self,
        path: &str,
        content: Option<EntryContent>,
        size: i64,
        mode_x: bool,
        mtime: i64,
    ) -> IndexEntry {
        IndexEntry {
            path: path.to_string(),
            version: 0,
            content,
            size,
            mode_x,
            mtime,
            by_machine: self.input.machine_id.to_string(),
            at: self.input.now,
        }
    }

    /// Keep a losing write next to `path`; older copies beyond the cap move
    /// to history.
    fn conflict_copy(&mut self, path: &str, loser: IndexEntry) -> Result<Option<(String, i64)>> {
        let mut name = None;
        for n in 1..=1000 {
            let p = conflict_path(path, self.input.machine_name, self.input.now, n);
            if wpath::check(&p).is_err() {
                return Ok(None);
            }
            if get_entry(self.tx, self.root, &p)?.is_none_or(|e| e.content.is_none()) {
                name = Some(p);
                break;
            }
        }
        let Some(name) = name else { return Ok(None) };
        let cur = get_entry(self.tx, self.root, &name)?;
        let version = self.write(
            cur.as_ref(),
            IndexEntry {
                path: name.clone(),
                ..loser
            },
        )?;
        let (prefix, ext) = conflict_prefix(path);
        let mut copies: Vec<IndexEntry> = all(
            self.tx,
            "SELECT * FROM file_entries WHERE root_id = ?1 AND substr(path, 1, ?3) = ?2
               AND (hash IS NOT NULL OR link IS NOT NULL) ORDER BY version",
            params![
                self.root,
                prefix,
                i64::try_from(prefix.len()).unwrap_or(i64::MAX)
            ],
            entry_row,
        )?
        .into_iter()
        .filter(|e| e.path.ends_with(&ext) && !e.path[prefix.len()..].contains('/'))
        .collect();
        while copies.len() > MAX_CONFLICT_COPIES {
            let old = copies.remove(0);
            let tomb = self.entry(&old.path, None, 0, false, self.input.now);
            self.write(Some(&old), tomb)?;
        }
        Ok(Some((name, version)))
    }

    fn apply(&mut self, ch: &FileChange, blobs: &HashSet<String>) -> Result<ChangeResult> {
        let reject = |code: &str, message: &str| ChangeResult::Rejected {
            code: code.to_string(),
            message: message.to_string(),
        };
        if let Err(e) = wpath::check(&ch.path) {
            return Ok(reject("invalid_path", &e.to_string()));
        }
        let new = match &ch.op {
            ChangeOp::Put {
                hash,
                size,
                mode_x,
                mtime,
            } => {
                if !is_hash(hash) || *size < 0 {
                    return Ok(reject("invalid_blob", "not a content hash"));
                }
                if !blobs.contains(hash) {
                    return Ok(reject("missing_blob", "upload the blob first"));
                }
                Some(self.entry(
                    &ch.path,
                    Some(EntryContent::Blob { hash: hash.clone() }),
                    *size,
                    *mode_x,
                    *mtime,
                ))
            }
            ChangeOp::Link { target } => {
                if link_inside(&ch.path, Path::new(target)).is_none()
                    || target.len() > wpath::MAX_PATH
                {
                    return Ok(reject("invalid_link", "the link leads outside the folder"));
                }
                Some(self.entry(
                    &ch.path,
                    Some(EntryContent::Link {
                        target: target.clone(),
                    }),
                    0,
                    false,
                    self.input.now,
                ))
            }
            ChangeOp::Delete | ChangeOp::Forget => None,
        };
        let cur = get_entry(self.tx, self.root, &ch.path)?;
        let cur_version = cur.as_ref().map_or(0, |e| e.version);
        let live = cur.as_ref().filter(|e| e.content.is_some());
        match new {
            Some(new) => {
                if let Some(l) = live
                    && l.content == new.content
                    && l.mode_x == new.mode_x
                {
                    // Two writers made the same change.
                    return Ok(ChangeResult::Ok { version: l.version });
                }
                // Modify wins over delete: a tombstone never blocks a write.
                if live.is_none() || ch.base_version == cur_version {
                    let version = self.write(live, new)?;
                    return Ok(ChangeResult::Ok { version });
                }
                let current = live.cloned();
                let copy = self.conflict_copy(&ch.path, new)?;
                Ok(ChangeResult::Conflict {
                    current,
                    copy_version: copy.as_ref().map(|c| c.1),
                    copy_path: copy.map(|c| c.0),
                })
            }
            None => {
                // A copy may exclude what its origin syncs (other caps,
                // other ignore files): only the origin drops a path for all.
                if ch.op == ChangeOp::Forget && self.input.machine_id != self.origin {
                    return Ok(reject(
                        "forget_origin_only",
                        "only the origin folder can stop syncing a file",
                    ));
                }
                let Some(l) = live else {
                    return Ok(ChangeResult::Ok {
                        version: cur_version,
                    });
                };
                if ch.base_version != cur_version {
                    return Ok(ChangeResult::Conflict {
                        current: Some(l.clone()),
                        copy_path: None,
                        copy_version: None,
                    });
                }
                if ch.op == ChangeOp::Forget {
                    to_history(self.tx, self.root, l, self.input.now)?;
                    self.tx.execute(
                        "DELETE FROM file_entries WHERE root_id = ?1 AND path = ?2",
                        params![self.root, ch.path],
                    )?;
                    let version = self.next();
                    return Ok(ChangeResult::Ok { version });
                }
                let tomb = self.entry(&ch.path, None, 0, false, self.input.now);
                let version = self.write(Some(l), tomb)?;
                Ok(ChangeResult::Ok { version })
            }
        }
    }
}

impl Store {
    /// Every root on the hub with its totals.
    pub fn hub_file_roots(&self) -> Result<Vec<RootInfo>> {
        self.read(|c| {
            all(
                c,
                &format!("{ROOTS_SQL} ORDER BY r.updated_at DESC"),
                [],
                root_info_row,
            )
        })
    }

    pub fn hub_file_root(&self, root_id: &str) -> Result<Option<RootInfo>> {
        self.read(|c| {
            one(
                c,
                &format!("{ROOTS_SQL} WHERE r.root_id = ?1"),
                params![root_id],
                root_info_row,
            )
        })
    }

    pub fn hub_file_modes(&self) -> Result<HashMap<String, FilesMode>> {
        let rows: Vec<(String, String)> = self.read(|c| {
            all(c, "SELECT project_id, mode FROM file_projects", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|(p, m)| FilesMode::parse(&m).map(|m| (p, m)))
            .collect())
    }

    pub fn hub_file_mode(&self, project: &str) -> Result<FilesMode> {
        self.read(|c| mode_in(c, project))
    }

    pub fn hub_set_file_mode(&self, project: &str, mode: FilesMode) -> Result<()> {
        self.write(|tx| {
            if mode == FilesMode::Default {
                tx.execute(
                    "DELETE FROM file_projects WHERE project_id = ?1",
                    params![project],
                )?;
            } else {
                tx.execute(
                    "INSERT INTO file_projects(project_id, mode) VALUES (?1, ?2)
                     ON CONFLICT(project_id) DO UPDATE SET mode = excluded.mode",
                    params![project, mode.as_str()],
                )?;
            }
            Ok(())
        })
    }

    /// Entries of `root_id` with a version after `after`, oldest first, and
    /// the root's head. None when the root does not exist.
    pub fn hub_file_index(
        &self,
        root_id: &str,
        after: i64,
        limit: usize,
    ) -> Result<Option<(Vec<IndexEntry>, i64)>> {
        self.read(|c| {
            let Some(head) = one(
                c,
                "SELECT head FROM file_roots WHERE root_id = ?1",
                params![root_id],
                |r| r.get::<_, i64>(0),
            )?
            else {
                return Ok(None);
            };
            let entries = all(
                c,
                "SELECT * FROM file_entries WHERE root_id = ?1 AND version > ?2 ORDER BY version LIMIT ?3",
                params![root_id, after, i64::try_from(limit).unwrap_or(i64::MAX)],
                entry_row,
            )?;
            Ok(Some((entries, head)))
        })
    }

    /// Apply one commit batch (one transaction). Each change gets its own
    /// result; the batch as a whole is refused for an unknown root, a claim
    /// that does not match, or a project whose file sync is off.
    pub fn hub_file_commit(
        &self,
        input: &CommitInput<'_>,
    ) -> Result<std::result::Result<CommitOutcome, CommitRefused>> {
        if !is_root_id(input.root_id) {
            return Ok(Err(CommitRefused::BadRoot("invalid root id".into())));
        }
        self.write(|tx| {
            let root: Option<(String, String, String, i64)> = one(
                tx,
                "SELECT machine_id, path, project_id, head FROM file_roots WHERE root_id = ?1",
                params![input.root_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            let (origin_machine, origin_path, stored_project, head) = match (root, input.claim_path) {
                (Some(r), None) => r,
                (Some(r), Some(p)) => {
                    if r.0 != input.machine_id || r.1 != p {
                        return Ok(Err(CommitRefused::BadRoot(
                            "the root belongs to another folder".into(),
                        )));
                    }
                    r
                }
                (None, None) => return Ok(Err(CommitRefused::UnknownRoot)),
                (None, Some(p)) => {
                    if crate::files::root_id(input.machine_id, p) != input.root_id {
                        return Ok(Err(CommitRefused::BadRoot(
                            "the root id does not match the folder".into(),
                        )));
                    }
                    let project: Option<String> = one(
                        tx,
                        "SELECT pp.project_id FROM project_paths pp JOIN projects p ON p.id = pp.project_id
                         WHERE pp.machine_id = ?1 AND pp.path = ?2 AND p.deleted = 0",
                        params![input.machine_id, p],
                        |r| r.get(0),
                    )?;
                    let Some(project) = project else {
                        return Ok(Err(CommitRefused::Pending));
                    };
                    tx.execute(
                        "INSERT INTO file_roots(root_id, machine_id, path, project_id, head, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5)",
                        params![input.root_id, input.machine_id, p, project, input.now],
                    )?;
                    (input.machine_id.to_string(), p.to_string(), project, 0)
                }
            };
            // The folder may have moved to another project (merge).
            let current: Option<String> = one(
                tx,
                "SELECT project_id FROM project_paths WHERE machine_id = ?1 AND path = ?2",
                params![origin_machine, origin_path],
                |r| r.get(0),
            )?;
            let project = current.unwrap_or(stored_project);
            if mode_in(tx, &project)? == FilesMode::Off {
                return Ok(Err(CommitRefused::Off));
            }
            let manifest = input
                .manifest
                .filter(|m| m.is_valid() && origin_machine == input.machine_id)
                .map(serde_json::to_string)
                .transpose()?;
            let hashes: Vec<&str> = input
                .changes
                .iter()
                .filter_map(|c| match &c.op {
                    ChangeOp::Put { hash, .. } => Some(hash.as_str()),
                    _ => None,
                })
                .collect();
            let mut blobs = HashSet::new();
            for h in hashes {
                if one(tx, "SELECT 1 FROM file_blobs WHERE hash = ?1", params![h], |r| {
                    r.get::<_, i64>(0)
                })?
                .is_some()
                {
                    blobs.insert(h.to_string());
                }
            }
            let mut c = Committer {
                tx,
                root: input.root_id,
                input,
                head,
                origin: &origin_machine,
            };
            let mut results = Vec::with_capacity(input.changes.len());
            for ch in input.changes {
                results.push(c.apply(ch, &blobs)?);
            }
            let head = c.head;
            tx.execute(
                "UPDATE file_roots SET head = ?2, project_id = ?3, updated_at = ?4,
                   manifest_json = coalesce(?5, manifest_json) WHERE root_id = ?1",
                params![input.root_id, head, project, input.now, manifest],
            )?;
            Ok(Ok(CommitOutcome { results, head }))
        })
    }

    /// Content hashes in a root's history (replaced versions still kept).
    pub fn hub_history_hashes(&self, root_id: &str) -> Result<Vec<String>> {
        self.read(|c| {
            all(
                c,
                "SELECT DISTINCT hash FROM file_history WHERE root_id = ?1 AND hash IS NOT NULL",
                params![root_id],
                |r| r.get(0),
            )
        })
    }

    /// Drop a root with its entries and history (blobs go with the next GC).
    pub fn hub_delete_file_root(&self, root_id: &str) -> Result<bool> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM file_entries WHERE root_id = ?1",
                params![root_id],
            )?;
            tx.execute(
                "DELETE FROM file_history WHERE root_id = ?1",
                params![root_id],
            )?;
            Ok(tx.execute(
                "DELETE FROM file_roots WHERE root_id = ?1",
                params![root_id],
            )? > 0)
        })
    }

    /// Which of `hashes` the hub has.
    pub fn hub_blobs_known(&self, hashes: &[String]) -> Result<HashSet<String>> {
        self.read(|c| {
            let mut out = HashSet::new();
            for h in hashes {
                if one(
                    c,
                    "SELECT 1 FROM file_blobs WHERE hash = ?1",
                    params![h],
                    |r| r.get::<_, i64>(0),
                )?
                .is_some()
                {
                    out.insert(h.clone());
                }
            }
            Ok(out)
        })
    }

    /// Raw size of a stored blob.
    pub fn hub_blob_size(&self, hash: &str) -> Result<Option<i64>> {
        self.read(|c| {
            one(
                c,
                "SELECT size FROM file_blobs WHERE hash = ?1",
                params![hash],
                |r| r.get(0),
            )
        })
    }

    pub fn hub_blob_added(&self, hash: &str, size: i64, stored: i64, now: i64) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO file_blobs(hash, size, stored, created_at) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(hash) DO UPDATE SET created_at = excluded.created_at",
                params![hash, size, stored, now],
            )?;
            Ok(())
        })
    }

    /// Bytes the blobs take on disk.
    pub fn hub_blob_usage(&self) -> Result<i64> {
        self.read(|c| {
            Ok(
                c.query_row("SELECT coalesce(sum(stored), 0) FROM file_blobs", [], |r| {
                    r.get(0)
                })?,
            )
        })
    }

    /// Retention: history older than `history_before`, tombstones older
    /// than `tombstones_before`. Returns (history rows, tombstones) removed.
    pub fn hub_prune_files(
        &self,
        history_before: i64,
        tombstones_before: i64,
    ) -> Result<(usize, usize)> {
        self.write(|tx| {
            let h = tx.execute(
                "DELETE FROM file_history WHERE replaced_at < ?1",
                params![history_before],
            )?;
            let t = tx.execute(
                "DELETE FROM file_entries WHERE hash IS NULL AND link IS NULL AND at < ?1",
                params![tombstones_before],
            )?;
            Ok((h, t))
        })
    }

    /// Quota: drop the `n` oldest history rows.
    pub fn hub_prune_oldest_history(&self, n: usize) -> Result<usize> {
        self.write(|tx| {
            Ok(tx.execute(
                "DELETE FROM file_history WHERE rowid IN
                   (SELECT rowid FROM file_history ORDER BY replaced_at LIMIT ?1)",
                params![i64::try_from(n).unwrap_or(i64::MAX)],
            )?)
        })
    }

    /// Blobs no entry or history row uses, added before `before` (younger
    /// ones may belong to a commit still on its way).
    pub fn hub_unreferenced_blobs(&self, before: i64) -> Result<Vec<String>> {
        self.read(|c| {
            all(
                c,
                "SELECT hash FROM file_blobs b WHERE created_at < ?1
                   AND NOT EXISTS (SELECT 1 FROM file_entries e WHERE e.hash = b.hash)
                   AND NOT EXISTS (SELECT 1 FROM file_history h WHERE h.hash = b.hash)",
                params![before],
                |r| r.get(0),
            )
        })
    }

    /// Drop the rows of those `hashes` that are still unreferenced and
    /// older than `before`, in one write transaction (so a commit that
    /// starts using one meanwhile keeps it). Returns the dropped ones;
    /// their files may go.
    pub fn hub_forget_blobs(&self, hashes: &[String], before: i64) -> Result<Vec<String>> {
        self.write(|tx| {
            let mut out = Vec::new();
            for h in hashes {
                let n = tx.execute(
                    "DELETE FROM file_blobs WHERE hash = ?1 AND created_at < ?2
                       AND NOT EXISTS (SELECT 1 FROM file_entries e WHERE e.hash = ?1)
                       AND NOT EXISTS (SELECT 1 FROM file_history h WHERE h.hash = ?1)",
                    params![h, before],
                )?;
                if n > 0 {
                    out.push(h.clone());
                }
            }
            Ok(out)
        })
    }

    /// Blobs a writer was just told the hub has: keep them past the next
    /// collection's grace period.
    pub fn hub_touch_blobs(&self, hashes: &[String], now: i64) -> Result<()> {
        self.write(|tx| {
            for h in hashes {
                tx.execute(
                    "UPDATE file_blobs SET created_at = ?2 WHERE hash = ?1 AND created_at < ?2",
                    params![h, now],
                )?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::hash_bytes;
    use crate::store::tests::temp_store;

    fn put(path: &str, base: i64, content: &str) -> FileChange {
        FileChange {
            path: path.into(),
            base_version: base,
            op: ChangeOp::Put {
                hash: hash_bytes(content.as_bytes()),
                size: content.len() as i64,
                mode_x: false,
                mtime: 1,
            },
        }
    }

    fn del(path: &str, base: i64) -> FileChange {
        FileChange {
            path: path.into(),
            base_version: base,
            op: ChangeOp::Delete,
        }
    }

    struct Hub {
        _dir: tempfile::TempDir,
        store: Store,
        root: String,
        folder: String,
    }

    fn hub() -> Hub {
        let (dir, store) = temp_store();
        let folder = dir.path().join("proj");
        std::fs::create_dir_all(&folder).unwrap();
        let p = store
            .register_project("origin", &folder, Some("p"))
            .unwrap();
        let folder = store.project_paths(&p.id).unwrap()[0].path.clone();
        for c in ["a", "b", "c", "d"] {
            store
                .hub_blob_added(&hash_bytes(c.as_bytes()), 1, 1, 0)
                .unwrap();
        }
        Hub {
            root: crate::files::root_id("origin", &folder),
            _dir: dir,
            store,
            folder,
        }
    }

    impl Hub {
        fn commit(
            &self,
            machine: &str,
            claim: bool,
            changes: &[FileChange],
        ) -> std::result::Result<CommitOutcome, CommitRefused> {
            self.store
                .hub_file_commit(&CommitInput {
                    root_id: &self.root,
                    machine_id: machine,
                    machine_name: machine,
                    claim_path: claim.then_some(self.folder.as_str()),
                    manifest: None,
                    changes,
                    now: 1_767_323_045_000,
                })
                .unwrap()
        }
    }

    #[test]
    fn roots_register_only_from_their_origin() {
        let h = hub();
        assert_eq!(
            h.commit("origin", false, &[put("x", 0, "a")]).unwrap_err(),
            CommitRefused::UnknownRoot
        );
        assert!(matches!(
            h.commit("other", true, &[put("x", 0, "a")]).unwrap_err(),
            CommitRefused::BadRoot(_)
        ));
        let out = h.commit("origin", true, &[put("x", 0, "a")]).unwrap();
        assert_eq!(out.results, [ChangeResult::Ok { version: 1 }]);
        // Copies commit without a claim; a claim for another folder is refused.
        assert!(h.commit("copy", false, &[put("y", 0, "b")]).is_ok());
        assert!(matches!(
            h.commit("copy", true, &[]).unwrap_err(),
            CommitRefused::BadRoot(_)
        ));
        let info = h.store.hub_file_root(&h.root).unwrap().unwrap();
        assert_eq!(
            (info.files, info.head, info.machine_id.as_str()),
            (2, 2, "origin")
        );
    }

    #[test]
    fn compare_and_set_with_conflict_copies() {
        let h = hub();
        h.commit("origin", true, &[put("s/f.txt", 0, "a")]).unwrap();
        // Same base from two writers: the second loses and is kept aside.
        assert_eq!(
            h.commit("m1", false, &[put("s/f.txt", 1, "b")])
                .unwrap()
                .results,
            [ChangeResult::Ok { version: 2 }]
        );
        let out = h.commit("m2", false, &[put("s/f.txt", 1, "c")]).unwrap();
        let ChangeResult::Conflict {
            current,
            copy_path,
            copy_version,
        } = &out.results[0]
        else {
            panic!("{out:?}")
        };
        assert_eq!(current.as_ref().unwrap().version, 2);
        assert_eq!(
            copy_path.as_deref(),
            Some("s/f.conflict-m2-20260102-030405.txt")
        );
        assert_eq!(*copy_version, Some(3));
        // The same content again is not a conflict.
        assert_eq!(
            h.commit("m2", false, &[put("s/f.txt", 1, "b")])
                .unwrap()
                .results,
            [ChangeResult::Ok { version: 2 }]
        );
        // The replaced version is in history.
        let n: i64 = h
            .store
            .read(|c| Ok(c.query_row("SELECT count(*) FROM file_history", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            h.store.hub_file_root(&h.root).unwrap().unwrap().conflicts,
            1
        );
    }

    #[test]
    fn modify_beats_delete() {
        let h = hub();
        h.commit("origin", true, &[put("f", 0, "a")]).unwrap();
        // Delete with a stale base: refused, the file stays.
        h.commit("m1", false, &[put("f", 1, "b")]).unwrap();
        let out = h.commit("m2", false, &[del("f", 1)]).unwrap();
        assert!(
            matches!(&out.results[0], ChangeResult::Conflict { current: Some(e), copy_path: None, .. } if e.version == 2)
        );
        // A delete with the current base leaves a tombstone ...
        assert_eq!(
            h.commit("m1", false, &[del("f", 2)]).unwrap().results,
            [ChangeResult::Ok { version: 3 }]
        );
        let (entries, head) = h.store.hub_file_index(&h.root, 2, 10).unwrap().unwrap();
        assert_eq!((entries[0].content.clone(), head), (None, 3));
        // ... which any write overrides, whatever its base.
        assert_eq!(
            h.commit("m2", false, &[put("f", 1, "c")]).unwrap().results,
            [ChangeResult::Ok { version: 4 }]
        );
        // Deleting what is already gone is fine.
        h.commit("m1", false, &[del("f", 4)]).unwrap();
        assert!(matches!(
            h.commit("m2", false, &[del("f", 4)]).unwrap().results[0],
            ChangeResult::Ok { .. }
        ));
    }

    #[test]
    fn malicious_changes_are_rejected() {
        let h = hub();
        h.commit("origin", true, &[]).unwrap();
        let bad = [
            put("../x", 0, "a"),
            put(".git/hooks/post-checkout", 0, "a"),
            put("a/GIT~1/config", 0, "a"),
            put("/abs", 0, "a"),
            put("ok.txt", 0, "not uploaded"),
            FileChange {
                path: "l".into(),
                base_version: 0,
                op: ChangeOp::Link {
                    target: "../../etc/passwd".into(),
                },
            },
        ];
        let out = h.commit("m", false, &bad).unwrap();
        let codes: Vec<&str> = out
            .results
            .iter()
            .map(|r| match r {
                ChangeResult::Rejected { code, .. } => code.as_str(),
                _ => "accepted",
            })
            .collect();
        assert_eq!(
            codes,
            [
                "invalid_path",
                "invalid_path",
                "invalid_path",
                "invalid_path",
                "missing_blob",
                "invalid_link"
            ]
        );
        assert_eq!(out.head, 0);
    }

    #[test]
    fn at_most_ten_conflict_copies_stay() {
        let h = hub();
        h.commit("origin", true, &[put("f", 0, "a")]).unwrap();
        for i in 0..12 {
            let content = if i % 2 == 0 { "b" } else { "c" };
            let out = h
                .commit(&format!("m{i}"), false, &[put("f", 0, content)])
                .unwrap();
            assert!(
                matches!(out.results[0], ChangeResult::Conflict { .. }),
                "{out:?}"
            );
        }
        assert_eq!(
            h.store.hub_file_root(&h.root).unwrap().unwrap().conflicts,
            10
        );
    }

    #[test]
    fn off_projects_refuse_uploads_and_forget_leaves_no_tombstone() {
        let h = hub();
        h.commit("origin", true, &[put("f", 0, "a"), put("g", 0, "b")])
            .unwrap();
        let project = h.store.hub_file_root(&h.root).unwrap().unwrap().project_id;
        h.store.hub_set_file_mode(&project, FilesMode::Off).unwrap();
        assert_eq!(
            h.commit("origin", false, &[]).unwrap_err(),
            CommitRefused::Off
        );
        h.store
            .hub_set_file_mode(&project, FilesMode::Default)
            .unwrap();
        assert!(h.store.hub_file_modes().unwrap().is_empty());
        let forget = FileChange {
            path: "f".into(),
            base_version: 1,
            op: ChangeOp::Forget,
        };
        // A copy's own exclusions never drop a file for everyone.
        let out = h
            .commit("copy", false, std::slice::from_ref(&forget))
            .unwrap();
        assert!(
            matches!(&out.results[0], ChangeResult::Rejected { code, .. } if code == "forget_origin_only")
        );
        h.commit("origin", false, &[forget]).unwrap();
        let (entries, _) = h.store.hub_file_index(&h.root, 0, 10).unwrap().unwrap();
        assert_eq!(
            entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            ["g"]
        );
    }

    #[test]
    fn retention_and_gc() {
        let h = hub();
        h.commit("origin", true, &[put("f", 0, "a")]).unwrap();
        h.commit("origin", false, &[put("f", 1, "b"), put("g", 0, "c")])
            .unwrap();
        h.commit("origin", false, &[del("g", 3)]).unwrap();
        // "a" is only in history, "d" nowhere.
        let unused = h.store.hub_unreferenced_blobs(1).unwrap();
        assert_eq!(unused, [hash_bytes(b"d")]);
        let now = 1_767_323_045_000;
        assert_eq!(h.store.hub_prune_files(now + 1, now + 1).unwrap(), (2, 1));
        let mut unused = h.store.hub_unreferenced_blobs(1).unwrap();
        unused.sort();
        let mut want = vec![hash_bytes(b"a"), hash_bytes(b"c"), hash_bytes(b"d")];
        want.sort();
        assert_eq!(unused, want);
        assert_eq!(h.store.hub_forget_blobs(&unused, 1).unwrap().len(), 3);
        assert_eq!(h.store.hub_blob_usage().unwrap(), 1);
        assert!(h.store.hub_delete_file_root(&h.root).unwrap());
        assert!(h.store.hub_file_root(&h.root).unwrap().is_none());
    }
}
