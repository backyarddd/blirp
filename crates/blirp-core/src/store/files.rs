//! Project file sync tables of every machine (migration 11,
//! docs/project-files.md): its working copies of roots, the base each copy
//! last agreed on with the hub per path, and the hash cache. None of these
//! tables replicate; the hub's own tables are in `file_hub`.

use super::{Result, Store, all, one};
use crate::files::EntryContent;
use crate::files::scan::Cached;
use rusqlite::{Row, params};
use std::collections::{HashMap, HashSet};
use std::path::Path;

// ---------------------------------------------------------------- local

/// A working copy of a root on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCopy {
    /// The folder (canonical, as registered).
    pub path: String,
    pub root_id: String,
    /// This machine's own folder (upload-only in v1).
    pub origin: bool,
    /// Root version up to which the hub's entries were compared.
    pub seen: i64,
    pub created_at: i64,
    pub mode: CopyMode,
    /// The hub root's incarnation it belongs to ("" until known).
    pub incarnation: String,
}

/// What a working copy does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    /// Uploads its edits live, takes the hub's changes on demand.
    OnDemand,
    /// Being downloaded: never uploads until the hub's files are written.
    Pending,
    /// Its hub copy is gone: it no longer syncs (files kept) and never
    /// becomes an origin of its own.
    Detached,
}

impl CopyMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::OnDemand => "on_demand",
            Self::Pending => "pending",
            Self::Detached => "detached",
        }
    }
}

/// What a copy last agreed on with the hub for one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub version: i64,
    pub content: Option<EntryContent>,
    /// Executable bit of that version (kept by writers that cannot see it).
    pub mode_x: bool,
    /// Known on the hub but not held here (a name this OS cannot hold, a
    /// case collision, a symlink on Windows): never read as a local delete.
    pub skipped: bool,
    /// Local content (hash, or `-` for a delete) the hub refused as a
    /// conflict and kept in a conflict copy: not sent again; an origin
    /// takes the winner on "Bring changes here".
    pub rejected: Option<String>,
}

/// `rejected` value of a refused delete.
pub const REJECTED_DELETE: &str = "-";

fn copy_row(r: &Row<'_>) -> rusqlite::Result<FileCopy> {
    Ok(FileCopy {
        path: r.get("path")?,
        root_id: r.get("root_id")?,
        origin: r.get("origin")?,
        seen: r.get("seen")?,
        created_at: r.get("created_at")?,
        mode: match r.get::<_, String>("mode")?.as_str() {
            "pending" => CopyMode::Pending,
            "detached" => CopyMode::Detached,
            _ => CopyMode::OnDemand,
        },
        incarnation: r.get("incarnation")?,
    })
}

pub(super) fn content_of(hash: Option<String>, link: Option<String>) -> Option<EntryContent> {
    match (hash, link) {
        (Some(hash), _) => Some(EntryContent::Blob { hash }),
        (None, Some(target)) => Some(EntryContent::Link { target }),
        (None, None) => None,
    }
}

pub(super) fn split_content(c: Option<&EntryContent>) -> (Option<&str>, Option<&str>) {
    match c {
        Some(EntryContent::Blob { hash }) => (Some(hash), None),
        Some(EntryContent::Link { target }) => (None, Some(target)),
        None => (None, None),
    }
}

impl Store {
    pub fn file_copies(&self) -> Result<Vec<FileCopy>> {
        self.read(|c| all(c, "SELECT * FROM file_copies ORDER BY path", [], copy_row))
    }

    pub fn file_copy(&self, path: &str) -> Result<Option<FileCopy>> {
        self.read(|c| {
            one(
                c,
                "SELECT * FROM file_copies WHERE path = ?1",
                params![path],
                copy_row,
            )
        })
    }

    /// Record a working copy (kept as is when it exists for the same root).
    /// A folder that becomes a copy of another root (or changes between
    /// origin and copy) starts over: its bases and hash cache are dropped.
    pub fn put_file_copy(&self, copy: &FileCopy) -> Result<()> {
        self.write(|tx| put_file_copy_in(tx, copy))
    }

    /// A download registers its folder: the copy (pending until the hub's
    /// files are written) and, for a folder rather than a workspace, this
    /// machine's folder of the project, in one transaction, so a copy never
    /// shows up without its row (it would count as an origin).
    pub fn register_download(&self, copy: &FileCopy, project: Option<(&str, &str)>) -> Result<()> {
        match project {
            None => self.put_file_copy(copy),
            Some((project_id, machine_id)) => self
                .add_project_folder_then(project_id, machine_id, Path::new(&copy.path), |tx| {
                    put_file_copy_in(tx, copy)
                })
                .map(|_| ()),
        }
    }

    /// The hub's files are written: the copy starts syncing.
    pub fn finish_file_copy(&self, path: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE file_copies SET mode = 'on_demand' WHERE path = ?1 AND mode = 'pending'",
                params![path],
            )?;
            Ok(())
        })
    }

    /// Record the hub root's incarnation; `fresh` drops the copy's bases
    /// (they belong to another incarnation: everything uploads again).
    pub fn set_file_copy_incarnation(
        &self,
        path: &str,
        incarnation: &str,
        fresh: bool,
    ) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE file_copies SET incarnation = ?2, seen = CASE WHEN ?3 THEN 0 ELSE seen END
                 WHERE path = ?1",
                params![path, incarnation, fresh],
            )?;
            if fresh {
                tx.execute("DELETE FROM file_base WHERE copy = ?1", params![path])?;
            }
            Ok(())
        })
    }
}

fn put_file_copy_in(tx: &rusqlite::Transaction<'_>, copy: &FileCopy) -> Result<()> {
    let old: Option<(String, bool)> = one(
        tx,
        "SELECT root_id, origin FROM file_copies WHERE path = ?1",
        params![copy.path],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if old
        .as_ref()
        .is_some_and(|(r, o)| *r == copy.root_id && *o == copy.origin)
    {
        tx.execute(
            "UPDATE file_copies SET mode = ?2,
               incarnation = CASE WHEN ?3 = '' THEN incarnation ELSE ?3 END WHERE path = ?1",
            params![copy.path, copy.mode.as_str(), copy.incarnation],
        )?;
        return Ok(());
    }
    tx.execute("DELETE FROM file_base WHERE copy = ?1", params![copy.path])?;
    tx.execute(
        "DELETE FROM file_hashes WHERE copy = ?1",
        params![copy.path],
    )?;
    tx.execute(
        "INSERT INTO file_copies(path, root_id, origin, mode, seen, created_at, incarnation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(path) DO UPDATE SET root_id = excluded.root_id, origin = excluded.origin,
           mode = excluded.mode, seen = excluded.seen, incarnation = excluded.incarnation",
        params![
            copy.path,
            copy.root_id,
            copy.origin,
            copy.mode.as_str(),
            copy.seen,
            copy.created_at,
            copy.incarnation
        ],
    )?;
    Ok(())
}

impl Store {
    /// Its hub copy is gone: the folder stops syncing, keeps its files and
    /// never turns into an origin of its own.
    pub fn detach_file_copy(&self, path: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE file_copies SET mode = 'detached' WHERE path = ?1",
                params![path],
            )?;
            tx.execute("DELETE FROM file_base WHERE copy = ?1", params![path])?;
            Ok(())
        })
    }

    pub fn set_file_copy_seen(&self, path: &str, seen: i64) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE file_copies SET seen = ?2 WHERE path = ?1",
                params![path, seen],
            )?;
            Ok(())
        })
    }

    /// Forget a working copy with its bases and hash cache (the folder on
    /// disk is left alone).
    pub fn remove_file_copy(&self, path: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute("DELETE FROM file_copies WHERE path = ?1", params![path])?;
            tx.execute("DELETE FROM file_base WHERE copy = ?1", params![path])?;
            tx.execute("DELETE FROM file_hashes WHERE copy = ?1", params![path])?;
            Ok(())
        })
    }

    pub fn file_bases(&self, copy: &str) -> Result<HashMap<String, Base>> {
        let rows = self.read(|c| {
            all(
                c,
                "SELECT path, version, hash, link, skipped, rejected, mode_x FROM file_base WHERE copy = ?1",
                params![copy],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        Base {
                            version: r.get(1)?,
                            content: content_of(r.get(2)?, r.get(3)?),
                            skipped: r.get(4)?,
                            rejected: r.get(5)?,
                            mode_x: r.get(6)?,
                        },
                    ))
                },
            )
        })?;
        Ok(rows.into_iter().collect())
    }

    /// Set (Some) or drop (None) the bases of `copy` in one transaction.
    pub fn update_file_bases(&self, copy: &str, updates: &[(String, Option<Base>)]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        self.write(|tx| {
            for (path, base) in updates {
                match base {
                    Some(b) => {
                        let (hash, link) = split_content(b.content.as_ref());
                        tx.execute(
                            "INSERT INTO file_base(copy, path, version, hash, link, skipped, rejected, mode_x)
                             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                             ON CONFLICT(copy, path) DO UPDATE SET version = excluded.version,
                               hash = excluded.hash, link = excluded.link, skipped = excluded.skipped,
                               rejected = excluded.rejected, mode_x = excluded.mode_x",
                            params![copy, path, b.version, hash, link, b.skipped, b.rejected, b.mode_x],
                        )?;
                    }
                    None => {
                        tx.execute(
                            "DELETE FROM file_base WHERE copy = ?1 AND path = ?2",
                            params![copy, path],
                        )?;
                    }
                }
            }
            Ok(())
        })
    }

    pub fn file_hash_cache(&self, copy: &str) -> Result<HashMap<String, Cached>> {
        let rows = self.read(|c| {
            all(
                c,
                "SELECT path, size, mtime_ns, file_id, hash, secret, checked_at FROM file_hashes WHERE copy = ?1",
                params![copy],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        Cached {
                            size: u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
                            mtime_ns: r.get(2)?,
                            file_id: r.get(3)?,
                            hash: r.get(4)?,
                            secret: r.get(5)?,
                            checked_at: r.get(6)?,
                        },
                    ))
                },
            )
        })?;
        Ok(rows.into_iter().collect())
    }

    /// Store new cache rows; with `keep`, drop rows of paths not in it
    /// (files that are gone).
    pub fn save_file_hash_cache(
        &self,
        copy: &str,
        updates: &[(String, Cached)],
        keep: Option<&HashSet<String>>,
    ) -> Result<()> {
        let stale: Vec<String> = match keep {
            Some(keep) => self
                .file_hash_cache(copy)?
                .into_keys()
                .filter(|p| !keep.contains(p))
                .collect(),
            None => Vec::new(),
        };
        if updates.is_empty() && stale.is_empty() {
            return Ok(());
        }
        self.write(|tx| {
            for p in &stale {
                tx.execute(
                    "DELETE FROM file_hashes WHERE copy = ?1 AND path = ?2",
                    params![copy, p],
                )?;
            }
            for (p, c) in updates {
                tx.execute(
                    "INSERT INTO file_hashes(copy, path, size, mtime_ns, file_id, hash, secret, checked_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                     ON CONFLICT(copy, path) DO UPDATE SET size = excluded.size,
                       mtime_ns = excluded.mtime_ns, file_id = excluded.file_id, hash = excluded.hash,
                       secret = excluded.secret, checked_at = excluded.checked_at",
                    params![
                        copy,
                        p,
                        i64::try_from(c.size).unwrap_or(i64::MAX),
                        c.mtime_ns,
                        c.file_id,
                        c.hash,
                        c.secret,
                        c.checked_at
                    ],
                )?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::temp_store;

    #[test]
    fn local_copies_bases_and_cache() {
        let (_d, s) = temp_store();
        let copy = FileCopy {
            path: "/p".into(),
            root_id: "0".repeat(32),
            origin: true,
            seen: 0,
            created_at: 1,
            mode: CopyMode::OnDemand,
            incarnation: String::new(),
        };
        s.put_file_copy(&copy).unwrap();
        s.set_file_copy_seen("/p", 7).unwrap();
        assert_eq!(s.file_copy("/p").unwrap().unwrap().seen, 7);
        let base = Base {
            version: 3,
            content: Some(EntryContent::Blob { hash: "h".into() }),
            mode_x: true,
            skipped: false,
            rejected: Some(REJECTED_DELETE.into()),
        };
        s.update_file_bases(
            "/p",
            &[
                ("a".into(), Some(base.clone())),
                ("b".into(), Some(base.clone())),
            ],
        )
        .unwrap();
        s.update_file_bases("/p", &[("b".into(), None)]).unwrap();
        assert_eq!(
            s.file_bases("/p").unwrap(),
            HashMap::from([("a".to_string(), base)])
        );
        let c = Cached {
            size: 1,
            mtime_ns: 2,
            file_id: 3,
            hash: "h".into(),
            secret: false,
            checked_at: 4,
        };
        s.save_file_hash_cache(
            "/p",
            &[("a".into(), c.clone()), ("b".into(), c.clone())],
            None,
        )
        .unwrap();
        s.save_file_hash_cache("/p", &[], Some(&HashSet::from(["a".to_string()])))
            .unwrap();
        assert_eq!(s.file_hash_cache("/p").unwrap().len(), 1);
        // The same folder as a copy of another root starts over.
        s.put_file_copy(&copy).unwrap();
        assert_eq!(s.file_bases("/p").unwrap().len(), 1, "unchanged: kept");
        s.put_file_copy(&FileCopy {
            root_id: "1".repeat(32),
            origin: false,
            ..copy.clone()
        })
        .unwrap();
        assert!(s.file_bases("/p").unwrap().is_empty());
        assert!(s.file_hash_cache("/p").unwrap().is_empty());
        s.detach_file_copy("/p").unwrap();
        assert_eq!(s.file_copy("/p").unwrap().unwrap().mode, CopyMode::Detached);
        s.remove_file_copy("/p").unwrap();
        assert!(s.file_copies().unwrap().is_empty());
        assert!(s.file_bases("/p").unwrap().is_empty());
    }
}
