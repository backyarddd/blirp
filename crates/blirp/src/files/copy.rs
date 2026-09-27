//! One working copy against the hub: upload its changes (scan, blobs,
//! compare-and-set commits, conflict handling) and apply the hub's changes
//! to it (Update from hub, Bring changes here, the overlay of a new copy).
//!
//! Per path a copy keeps a base: the version and content it last agreed on
//! with the hub. A local file that differs from its base is uploaded with
//! that base version; the hub accepts it only if nobody wrote the path
//! meanwhile. A remote version is written locally only over a file that
//! still equals its base; otherwise the remote content lands next to it as
//! a conflict copy. Nothing is ever overwritten that was not synced first.

use super::local::{self, LocalScan};
use blirp_core::files::path::{self as wpath, case_insensitive_fs, case_key};
use blirp_core::files::scan::{Content, Hashed, ScanConfig, ScanState, hash_file};
use blirp_core::files::write::{Expect, Target, WriteError};
use blirp_core::files::{
    ChangeOp, ChangeResult, EntryContent, FileChange, IndexEntry, conflict_path,
};
use blirp_core::model::ExcludedGroup;
use blirp_core::store::{Base, REJECTED_DELETE, Store};
use blirp_sync::SyncError;
use blirp_sync::files::FileHub;
use blirp_sync::files::proto::MAX_CHANGES;
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, thiserror::Error)]
pub enum CopyError {
    /// The hub refused or could not be reached (`code` as the hub sent it,
    /// or `hub_unreachable`).
    #[error("{message}")]
    Hub { code: String, message: String },
    #[error("{0}")]
    Local(String),
}

impl CopyError {
    pub fn code(&self) -> &str {
        match self {
            Self::Hub { code, .. } => code,
            Self::Local(_) => "local_error",
        }
    }
}

impl From<SyncError> for CopyError {
    fn from(e: SyncError) -> Self {
        match e {
            SyncError::Remote { code, message } => Self::Hub { code, message },
            other => Self::Hub {
                code: "hub_unreachable".into(),
                message: other.to_string(),
            },
        }
    }
}

fn local(e: impl std::fmt::Display) -> CopyError {
    CopyError::Local(e.to_string())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, CopyError> + Send + 'static,
) -> Result<T, CopyError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| CopyError::Local(format!("background task failed: {e}")))?
}

/// What the engine needs to work on copies.
#[derive(Clone)]
pub struct Env {
    pub store: Arc<Store>,
    pub hub: FileHub,
    pub data_dir: PathBuf,
    pub scan: ScanConfig,
    /// Hashing runs one folder at a time.
    pub gate: Arc<tokio::sync::Semaphore>,
}

/// One working copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    /// `file_copies.path`: the folder as registered.
    pub key: String,
    pub root_id: String,
    /// This machine's own folder: upload-only, it registers the root.
    pub origin: bool,
}

impl Copy {
    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.key)
    }
}

fn to_content(c: &Content) -> EntryContent {
    match c {
        Content::Blob(hash) => EntryContent::Blob { hash: hash.clone() },
        Content::Link(target) => EntryContent::Link {
            target: target.clone(),
        },
    }
}

fn content_hash(c: &EntryContent) -> &str {
    match c {
        EntryContent::Blob { hash } => hash,
        EntryContent::Link { target } => target,
    }
}

fn expect_of(c: Option<&EntryContent>) -> Expect {
    match c {
        None => Expect::Absent,
        Some(EntryContent::Blob { hash }) => Expect::Blob(hash.clone()),
        Some(EntryContent::Link { target }) => Expect::Link(target.clone()),
    }
}

fn base_of(e: &IndexEntry) -> Base {
    Base {
        version: e.version,
        content: e.content.clone(),
        mode_x: e.mode_x,
        skipped: false,
        rejected: None,
    }
}

/// Result of one upload pass.
#[derive(Debug, Clone, Default)]
pub struct UploadReport {
    pub state: Option<ScanState>,
    pub files: usize,
    pub bytes: u64,
    pub excluded: Vec<ExcludedGroup>,
    pub reincluded_secrets: Vec<String>,
    /// Changes the hub accepted.
    pub sent: usize,
    /// Changes that lost a race (kept as conflict copies).
    pub conflicts: usize,
    /// Changes still to send (failed uploads, refused changes).
    pub pending: usize,
    /// The root's head after the last commit.
    pub head: Option<i64>,
    /// Files that disappeared in numbers too large to be an edit: nothing
    /// was committed; the user confirms the delete or restores them.
    pub held_deletes: Vec<String>,
}

/// Mass-delete guard: a pass that would delete more than this many of a
/// root's synced files ...
pub const MASS_DELETE_MIN: usize = 50;
/// ... and more than this share of them (percent) commits nothing.
pub const MASS_DELETE_PERCENT: usize = 30;

/// Whether deleting `deletes` of `synced` files needs a confirmation.
pub fn mass_delete(deletes: usize, synced: usize) -> bool {
    deletes > MASS_DELETE_MIN.max(synced.saturating_mul(MASS_DELETE_PERCENT) / 100)
}

/// How an upload treats many deletions at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deletes {
    /// Hold them (the default).
    Guard,
    /// The user confirmed: delete on the hub too.
    Confirm,
}

/// A change to send, with where its content is.
#[derive(Debug, Clone)]
struct Planned {
    change: FileChange,
    local: Option<Hashed>,
}

/// Changes of `files` (what the scan found) against `bases`. Paths that
/// could not be read this time are left alone; a vanished path that still
/// exists on disk is excluded now and is forgotten rather than deleted.
/// Whether `path` lies in (or is) one of the folders the scan could not
/// read completely.
fn under_unreadable(path: &str, dirs: &[String]) -> bool {
    dirs.iter().any(|d| {
        d.is_empty()
            || path == d
            || path
                .strip_prefix(d.as_str())
                .is_some_and(|r| r.starts_with('/'))
    })
}

/// An upload plan: changes to send, and bases to drop without telling the
/// hub (a copy's own exclusions).
#[derive(Debug, Default)]
struct UploadPlan {
    changes: Vec<Planned>,
    drop_bases: Vec<String>,
}

/// Changes of `files` (what the scan found) against `bases`. Paths that
/// could not be read this time, or lie in a folder that could not be
/// listed, are left alone. A vanished path that still exists on disk is
/// excluded now: the origin stops syncing it for everyone, a copy only
/// forgets it here. Only a path confirmed gone is deleted.
fn plan_upload(
    root: &Path,
    files: &[Hashed],
    bases: &HashMap<String, Base>,
    unreadable: &HashSet<String>,
    unreadable_dirs: &[String],
    origin: bool,
) -> UploadPlan {
    let mut out = UploadPlan::default();
    let mut seen = HashSet::new();
    for f in files {
        seen.insert(f.path.as_str());
        let content = to_content(&f.content);
        let base = bases.get(&f.path);
        // Windows cannot see the executable bit: keep the synced one.
        let mode_x = if cfg!(unix) {
            f.mode_x
        } else {
            base.is_some_and(|b| b.mode_x)
        };
        if let Some(b) = base {
            if b.skipped || (b.content.as_ref() == Some(&content) && b.mode_x == mode_x) {
                continue;
            }
            if b.rejected.as_deref() == Some(content_hash(&content)) {
                continue;
            }
        }
        let op = match &f.content {
            Content::Blob(hash) => ChangeOp::Put {
                hash: hash.clone(),
                size: i64::try_from(f.size).unwrap_or(i64::MAX),
                mode_x,
                mtime: f.mtime_ns / 1_000_000,
            },
            Content::Link(target) => ChangeOp::Link {
                target: target.clone(),
            },
        };
        out.changes.push(Planned {
            change: FileChange {
                path: f.path.clone(),
                base_version: base.map_or(0, |b| b.version),
                op,
            },
            local: Some(Hashed {
                mode_x,
                ..f.clone()
            }),
        });
    }
    let mut gone: Vec<(&String, &Base)> = bases
        .iter()
        .filter(|(p, b)| !seen.contains(p.as_str()) && !b.skipped && b.content.is_some())
        .filter(|(p, _)| !unreadable.contains(*p) && !under_unreadable(p, unreadable_dirs))
        .filter(|(_, b)| b.rejected.as_deref() != Some(REJECTED_DELETE))
        .collect();
    gone.sort_by(|a, b| a.0.cmp(b.0));
    for (path, b) in gone {
        let op = match std::fs::symlink_metadata(wpath::to_local(root, path)) {
            Ok(_) if origin => ChangeOp::Forget,
            Ok(_) => {
                out.drop_bases.push(path.clone());
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ChangeOp::Delete,
            // Cannot tell (permissions, I/O): never read as a delete.
            Err(_) => continue,
        };
        out.changes.push(Planned {
            change: FileChange {
                path: path.clone(),
                base_version: b.version,
                op,
            },
            local: None,
        });
    }
    out
}

/// Machine names for conflict copy names (the replicated machines table).
fn machine_names(store: &Store) -> HashMap<String, String> {
    store
        .list_machines()
        .map(|ms| ms.into_iter().map(|m| (m.id, m.name)).collect())
        .unwrap_or_default()
}

/// Scan, hash and upload `copy`'s changes. `claim` is the origin's folder
/// (registers the root on the hub).
pub async fn upload(env: &Env, copy: &Copy) -> Result<UploadReport, CopyError> {
    upload_with(env, copy, Deletes::Guard).await
}

/// [`upload`], with the mass-delete guard on or confirmed.
pub async fn upload_with(
    env: &Env,
    copy: &Copy,
    deletes: Deletes,
) -> Result<UploadReport, CopyError> {
    let root = copy.root();
    let ls: LocalScan = {
        let _permit = env
            .gate
            .acquire()
            .await
            .map_err(|e| local(format!("file scanner stopped: {e}")))?;
        let (store, key, r, cfg) = (
            env.store.clone(),
            copy.key.clone(),
            root.clone(),
            env.scan.clone(),
        );
        blocking(move || local::scan_copy(&store, &key, &r, &cfg).map_err(local)).await?
    };
    let mut report = UploadReport {
        state: Some(ls.scan.state),
        files: ls.hashed.files.len(),
        bytes: ls.hashed.files.iter().map(|f| f.size).sum(),
        excluded: local::excluded_groups(&ls),
        reincluded_secrets: ls
            .scan
            .reincluded_secrets
            .iter()
            .chain(&ls.hashed.reincluded_secrets)
            .cloned()
            .collect(),
        ..Default::default()
    };
    if ls.scan.state != ScanState::Ok {
        return Ok(report);
    }
    let (store, key) = (env.store.clone(), copy.key.clone());
    let bases = blocking(move || store.file_bases(&key).map_err(local)).await?;
    let unreadable: HashSet<String> = ls.hashed.unreadable.iter().cloned().collect();
    let dirs = ls.scan.unreadable.clone();
    let (r, files, origin) = (root.clone(), ls.hashed.files.clone(), copy.origin);
    let b2 = bases.clone();
    let planned =
        blocking(move || Ok(plan_upload(&r, &files, &b2, &unreadable, &dirs, origin))).await?;
    if !planned.drop_bases.is_empty() {
        let drops: Vec<(String, Option<Base>)> =
            planned.drop_bases.into_iter().map(|p| (p, None)).collect();
        let (store, key) = (env.store.clone(), copy.key.clone());
        blocking(move || store.update_file_bases(&key, &drops).map_err(local)).await?;
    }
    let mut plan = planned.changes;
    if plan.is_empty() {
        return Ok(report);
    }
    // An emptied or swapped folder, not an edit: commit nothing until the
    // user says what happened.
    let doomed: Vec<String> = plan
        .iter()
        .filter(|p| p.change.op == ChangeOp::Delete)
        .map(|p| p.change.path.clone())
        .collect();
    let synced = bases
        .values()
        .filter(|b| b.content.is_some() && !b.skipped)
        .count();
    if deletes == Deletes::Guard && mass_delete(doomed.len(), synced) {
        tracing::warn!(
            files = doomed.len(),
            synced,
            "many files disappeared from a synced folder; holding the upload"
        );
        report.held_deletes = doomed;
        return Ok(report);
    }

    // Blobs first: a commit only names content the hub has.
    let mut sources: HashMap<String, (PathBuf, u64)> = HashMap::new();
    for p in &plan {
        if let (ChangeOp::Put { hash, .. }, Some(l)) = (&p.change.op, &p.local) {
            sources
                .entry(hash.clone())
                .or_insert_with(|| (wpath::to_local(&root, &l.path), l.size));
        }
    }
    let hashes: Vec<String> = sources.keys().cloned().collect();
    let missing = env.hub.missing(&hashes).await?;
    let results: Vec<(String, Result<(), SyncError>)> = futures_util::stream::iter(missing)
        .map(|m| {
            let hub = env.hub.clone();
            let src = sources.get(&m.hash).cloned();
            async move {
                let r = match src {
                    Some((path, len)) => hub.upload(&m.hash, &path, len).await,
                    None => Ok(()),
                };
                (m.hash, r)
            }
        })
        .buffer_unordered(blirp_sync::files::proto::STREAMS)
        .collect()
        .await;
    let mut failed: HashSet<String> = HashSet::new();
    for (hash, r) in results {
        match r {
            Ok(()) => {}
            Err(SyncError::Remote { code, .. })
                if code == "file_changed" || code == "hash_mismatch" =>
            {
                // Changed since it was hashed: the next pass sends it.
                failed.insert(hash);
            }
            Err(e) => return Err(e.into()),
        }
    }
    let before = plan.len();
    plan.retain(|p| !matches!(&p.change.op, ChangeOp::Put { hash, .. } if failed.contains(hash)));
    report.pending = before - plan.len();

    let manifest = if copy.origin && ls.git {
        let r = root.clone();
        Some(blocking(move || Ok(local::git_manifest(&r))).await?)
    } else {
        None
    };
    let claim = copy.origin.then(|| copy.key.clone());
    for (i, batch) in plan.chunks(MAX_CHANGES).enumerate() {
        let changes: Vec<FileChange> = batch.iter().map(|p| p.change.clone()).collect();
        let out = env
            .hub
            .commit(
                &copy.root_id,
                claim.as_deref(),
                if i == 0 { manifest.as_ref() } else { None },
                changes,
            )
            .await?;
        report.head = Some(out.head);
        let mut updates: Vec<(String, Option<Base>)> = Vec::new();
        for (p, result) in batch.iter().zip(out.results) {
            match result {
                ChangeResult::Ok { version } => {
                    report.sent += 1;
                    let base = p.local.as_ref().map(|l| Base {
                        version,
                        content: Some(to_content(&l.content)),
                        mode_x: l.mode_x,
                        skipped: false,
                        rejected: None,
                    });
                    updates.push((p.change.path.clone(), base));
                }
                ChangeResult::Conflict {
                    current,
                    copy_path,
                    copy_version,
                } => {
                    report.conflicts += 1;
                    let fix = resolve_conflict(
                        env,
                        copy,
                        p,
                        current.as_ref(),
                        copy_path.zip(copy_version),
                        bases.get(&p.change.path),
                    )
                    .await;
                    updates.extend(fix);
                }
                ChangeResult::Rejected { code, .. } => {
                    report.pending += 1;
                    tracing::warn!(path = %p.change.path, %code, "the hub refused a file change");
                }
            }
        }
        let (store, key) = (env.store.clone(), copy.key.clone());
        blocking(move || store.update_file_bases(&key, &updates).map_err(local)).await?;
    }
    Ok(report)
}

/// A change of ours lost a race. The hub kept our content at `saved`
/// (`(copy path, version)`) when it was a write. An origin folder is never
/// written by the engine in v1: it remembers what the hub refused and
/// shows the winner as incoming ("Bring changes here"). Any other copy
/// writes its own content to the conflict copy and takes the winner.
async fn resolve_conflict(
    env: &Env,
    copy: &Copy,
    p: &Planned,
    current: Option<&IndexEntry>,
    saved: Option<(String, i64)>,
    base: Option<&Base>,
) -> Vec<(String, Option<Base>)> {
    let path = p.change.path.clone();
    let mine = p.local.as_ref().map(|l| to_content(&l.content));
    let rejected = match &mine {
        Some(c) => content_hash(c).to_string(),
        None => REJECTED_DELETE.to_string(),
    };
    // Remember what the hub refused, so it is not sent again: only when
    // the hub kept it (a conflict copy of a write; a refused delete keeps
    // nothing to lose). Otherwise the next pass simply tries again.
    let remember = |base: Option<&Base>| {
        vec![(
            path.clone(),
            Some(Base {
                version: base.map_or(0, |b| b.version),
                content: base.and_then(|b| b.content.clone()),
                mode_x: base.is_some_and(|b| b.mode_x),
                skipped: false,
                rejected: Some(rejected.clone()),
            }),
        )]
    };
    if mine.is_some() && saved.is_none() {
        return Vec::new();
    }
    if copy.origin {
        return remember(base);
    }
    let root = copy.root();
    let mut out = Vec::new();
    // Our content, where the hub kept it.
    if let (Some((copy_path, version)), Some(Content::Blob(hash))) =
        (&saved, p.local.as_ref().map(|l| &l.content))
    {
        let (r, src, dst, h, data) = (
            root.clone(),
            path.clone(),
            copy_path.clone(),
            hash.clone(),
            env.data_dir.clone(),
        );
        let written = blocking(move || {
            let t = Target {
                root: &r,
                data_dir: Some(&data),
            };
            let mut f = std::fs::File::open(wpath::to_local(&r, &src)).map_err(local)?;
            t.write_verified(&dst, &Expect::Absent, &mut f, &h, false, None)
                .map_err(local)
        })
        .await;
        match written {
            Ok(()) => out.push((
                copy_path.clone(),
                Some(Base {
                    version: *version,
                    content: Some(EntryContent::Blob { hash: hash.clone() }),
                    mode_x: false,
                    skipped: false,
                    rejected: None,
                }),
            )),
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "keeping a conflict copy failed; retrying later");
                return remember(base);
            }
        }
    }
    // A losing link stays on the hub at its copy path; the next update brings it.
    // The winner at the path, over our (now saved) content.
    let expect = mine.as_ref().map_or(Expect::Absent, |c| expect_of(Some(c)));
    match current {
        Some(winner) => match write_entry_at(env, &root, winner, &winner.path, &expect).await {
            Ok(()) => out.push((path.clone(), Some(base_of(winner)))),
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "taking the winning version failed; retrying later");
                out.extend(remember(base));
            }
        },
        None => out.push((path.clone(), None)),
    }
    out
}

/// Write the hub's content of `e` at `at` if the local file is `expect`.
async fn write_entry_at(
    env: &Env,
    root: &Path,
    e: &IndexEntry,
    at: &str,
    expect: &Expect,
) -> Result<(), CopyError> {
    let data = env.data_dir.clone();
    match &e.content {
        Some(EntryContent::Blob { hash }) => {
            let part = env.hub.download(hash).await?;
            let (r, at, expect, hash, mode_x, mtime, p) = (
                root.to_path_buf(),
                at.to_string(),
                expect.clone(),
                hash.clone(),
                e.mode_x,
                e.mtime,
                part.clone(),
            );
            let res = blocking(move || {
                let t = Target {
                    root: &r,
                    data_dir: Some(&data),
                };
                let mut f = std::fs::File::open(&p).map_err(local)?;
                t.write_verified(&at, &expect, &mut f, &hash, mode_x, Some(mtime))
                    .map_err(|e| match e {
                        WriteError::Changed(p) => CopyError::Local(format!("{p} changed locally")),
                        other => local(other),
                    })
            })
            .await;
            let _ = tokio::fs::remove_file(&part).await;
            res
        }
        Some(EntryContent::Link { target }) => {
            let (r, at, expect, target) = (
                root.to_path_buf(),
                at.to_string(),
                expect.clone(),
                target.clone(),
            );
            blocking(move || {
                let t = Target {
                    root: &r,
                    data_dir: Some(&data),
                };
                t.link(&at, &expect, &target).map_err(local)
            })
            .await
        }
        None => Err(local("nothing to write")),
    }
}

/// What applying the hub's version does to one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// The local file already is the hub's version.
    Ack,
    /// Only the base goes (the hub dropped a path this copy no longer has).
    DropBase,
    /// Write the hub's version over a file that still equals its base (or
    /// where there is none).
    Write(Expect),
    /// Remove a file that still equals its base.
    Delete(Expect),
    /// The hub deleted a path this copy changed: keep the file and base it
    /// on the tombstone, so it uploads (modify beats delete).
    Rebase,
    /// The local file changed since its base: the hub's version is written
    /// next to it as a conflict copy, and the local one uploads over it.
    ConflictCopy,
    /// Cannot be held here (invalid name, symlink on Windows, a folder in
    /// the way, a case-only collision): recorded, never read as a delete.
    Skip(String),
}

/// Local state of a path, compared by content.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Local {
    Missing,
    Has(EntryContent),
    /// A folder or special file.
    Other,
}

fn local_state(root: &Path, path: &str) -> Local {
    let p = wpath::to_local(root, path);
    let Ok(m) = std::fs::symlink_metadata(&p) else {
        return Local::Missing;
    };
    if m.file_type().is_symlink() {
        return std::fs::read_link(&p)
            .ok()
            .and_then(|t| t.to_str().map(|s| s.replace('\\', "/")))
            .map_or(Local::Other, |target| {
                Local::Has(EntryContent::Link { target })
            });
    }
    if m.is_file() {
        return hash_file(&p).map_or(Local::Other, |(hash, _, _)| {
            Local::Has(EntryContent::Blob { hash })
        });
    }
    Local::Other
}

/// Decide one path. `fresh`: a copy being created, where the hub wins over
/// whatever the clone checked out.
fn decide(e: &IndexEntry, base: Option<&Base>, now: &Local, fresh: bool) -> Option<Act> {
    if base.is_some_and(|b| b.version >= e.version) {
        return None;
    }
    let matches_base = |c: &EntryContent| base.is_some_and(|b| b.content.as_ref() == Some(c));
    // Content the hub refused and kept in a conflict copy may be replaced.
    let preserved =
        |c: &EntryContent| base.is_some_and(|b| b.rejected.as_deref() == Some(content_hash(c)));
    match &e.content {
        None => match now {
            Local::Missing | Local::Other => base.map(|_| Act::DropBase),
            Local::Has(c) if matches_base(c) || fresh => Some(Act::Delete(expect_of(Some(c)))),
            // Changed here: kept, and it uploads over the tombstone (also
            // content an origin held back after losing a race).
            Local::Has(_) => Some(Act::Rebase),
        },
        Some(content) => {
            if !wpath::valid_here(&e.path) {
                return Some(Act::Skip("the name is not valid on this system".into()));
            }
            if matches!(content, EntryContent::Link { .. }) && cfg!(windows) {
                return Some(Act::Skip("symlinks are not recreated on Windows".into()));
            }
            match now {
                Local::Has(c) if c == content => Some(Act::Ack),
                // Deleted here but changed there: modify wins.
                Local::Missing => Some(Act::Write(Expect::Absent)),
                Local::Has(c) if matches_base(c) || preserved(c) || fresh => {
                    Some(Act::Write(expect_of(Some(c))))
                }
                Local::Has(_) => Some(Act::ConflictCopy),
                Local::Other => Some(Act::Skip("a folder is in the way".into())),
            }
        }
    }
}

/// One path the hub has newer than this copy.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub entry: IndexEntry,
    pub act: Act,
}

/// The hub's changes this copy has not taken, and the root's head.
pub async fn incoming(
    env: &Env,
    copy: &Copy,
    fresh: bool,
) -> Result<(Vec<Incoming>, i64), CopyError> {
    let (entries, head) = env.hub.index(&copy.root_id, 0).await?;
    let (store, key, root) = (env.store.clone(), copy.key.clone(), copy.root());
    blocking(move || {
        let bases = store.file_bases(&key).map_err(local)?;
        let mut out = Vec::new();
        // Case-only collisions on case-insensitive systems: the first name
        // is written, the others as conflict copies.
        let mut taken: HashSet<String> = HashSet::new();
        let mut on_hub: HashSet<&str> = HashSet::new();
        for e in &entries {
            on_hub.insert(e.path.as_str());
            if wpath::check(&e.path).is_err() {
                continue;
            }
            let base = bases.get(&e.path);
            if base.is_some_and(|b| b.version >= e.version) {
                if e.content.is_some() {
                    taken.insert(case_key(&e.path));
                }
                continue;
            }
            let collides =
                e.content.is_some() && case_insensitive_fs() && !taken.insert(case_key(&e.path));
            let act = if collides {
                Some(Act::Skip(
                    "another file has the same name in a different case".into(),
                ))
            } else {
                let now = local_state(&root, &e.path);
                decide(e, base, &now, fresh)
            };
            if let Some(act) = act {
                out.push(Incoming {
                    entry: e.clone(),
                    act,
                });
            }
        }
        // Bases of paths the hub no longer lists (forgotten, or tombstones
        // past retention): only the base goes; local files stay.
        for p in bases.keys() {
            if !on_hub.contains(p.as_str()) {
                out.push(Incoming {
                    entry: IndexEntry {
                        path: p.clone(),
                        version: 0,
                        content: None,
                        size: 0,
                        mode_x: false,
                        mtime: 0,
                        by_machine: String::new(),
                        at: 0,
                    },
                    act: Act::DropBase,
                });
            }
        }
        Ok((out, head))
    })
    .await
}

/// Result of [`apply`].
#[derive(Debug, Clone, Default)]
pub struct ApplyReport {
    pub written: usize,
    pub deleted: usize,
    /// Local conflict copies made (hub versions next to local changes).
    pub conflicts: Vec<String>,
    /// Paths not held here, with why.
    pub skipped: Vec<(String, String)>,
    /// Paths that failed (retried next time), with why.
    pub failed: Vec<(String, String)>,
    pub head: i64,
}

/// "Restore from hub" after the mass-delete guard held: write back the
/// hub's version of every file that is missing here but unchanged on the
/// hub since this copy last synced it. Only while the folder itself exists.
pub async fn restore_missing(env: &Env, copy: &Copy) -> Result<ApplyReport, CopyError> {
    let root = copy.root();
    if !std::fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir()) {
        return Err(local("the folder is gone; nothing was restored"));
    }
    let (entries, head) = env.hub.index(&copy.root_id, 0).await?;
    let (store, key) = (env.store.clone(), copy.key.clone());
    let bases = blocking(move || store.file_bases(&key).map_err(local)).await?;
    let mut report = ApplyReport {
        head,
        ..Default::default()
    };
    for e in entries {
        let Some(b) = bases.get(&e.path) else {
            continue;
        };
        if e.content.is_none() || b.skipped || b.version != e.version || b.content != e.content {
            continue;
        }
        let (r, p) = (root.clone(), e.path.clone());
        if blocking(move || Ok(local_state(&r, &p))).await? != Local::Missing {
            continue;
        }
        match write_entry_at(env, &root, &e, &e.path, &Expect::Absent).await {
            Ok(()) => report.written += 1,
            Err(err) => report.failed.push((e.path.clone(), err.to_string())),
        }
    }
    Ok(report)
}

/// Take the hub's changes into `copy`: fast-forward what is unchanged
/// since its base, keep local changes and write the hub's versions next to
/// them. `fresh` for a copy being created (the hub wins over the clone).
pub async fn apply(env: &Env, copy: &Copy, fresh: bool) -> Result<ApplyReport, CopyError> {
    let (mut todo, head) = incoming(env, copy, fresh).await?;
    let names = {
        let store = env.store.clone();
        blocking(move || Ok(machine_names(&store))).await?
    };
    // Deletes first, so a file replaced by a folder of the same name fits.
    todo.sort_by_key(|i| {
        (
            !matches!(i.act, Act::Delete(_) | Act::DropBase),
            i.entry.path.clone(),
        )
    });
    let root = copy.root();
    let mut report = ApplyReport {
        head,
        ..Default::default()
    };
    let mut updates: Vec<(String, Option<Base>)> = Vec::new();
    for i in todo {
        let e = &i.entry;
        match i.act {
            Act::Ack => updates.push((e.path.clone(), Some(base_of(e)))),
            Act::DropBase => updates.push((e.path.clone(), None)),
            Act::Rebase => updates.push((
                e.path.clone(),
                Some(Base {
                    version: e.version,
                    content: None,
                    mode_x: false,
                    skipped: false,
                    rejected: None,
                }),
            )),
            Act::Skip(why) => {
                let name = names
                    .get(&e.by_machine)
                    .cloned()
                    .unwrap_or_else(|| "hub".into());
                if why.contains("different case")
                    && let Ok(p) = conflict_copy(env, &root, e, &name).await
                {
                    report.conflicts.push(p);
                }
                report.skipped.push((e.path.clone(), why));
                updates.push((
                    e.path.clone(),
                    Some(Base {
                        skipped: true,
                        ..base_of(e)
                    }),
                ));
            }
            Act::Delete(expect) => {
                let (r, p, data) = (root.clone(), e.path.clone(), env.data_dir.clone());
                let res = blocking(move || {
                    Target {
                        root: &r,
                        data_dir: Some(&data),
                    }
                    .remove(&p, &expect)
                    .map_err(local)
                })
                .await;
                match res {
                    Ok(()) => {
                        report.deleted += 1;
                        updates.push((e.path.clone(), None));
                    }
                    Err(err) => report.failed.push((e.path.clone(), err.to_string())),
                }
            }
            Act::Write(expect) => match write_entry_at(env, &root, e, &e.path, &expect).await {
                Ok(()) => {
                    report.written += 1;
                    updates.push((e.path.clone(), Some(base_of(e))));
                }
                Err(err) => report.failed.push((e.path.clone(), err.to_string())),
            },
            Act::ConflictCopy => {
                let name = names
                    .get(&e.by_machine)
                    .cloned()
                    .unwrap_or_else(|| "hub".into());
                match conflict_copy(env, &root, e, &name).await {
                    Ok(p) => {
                        report.conflicts.push(p);
                        // The local version uploads over the hub's (which
                        // stays in the conflict copy and in history).
                        updates.push((
                            e.path.clone(),
                            Some(Base {
                                content: e.content.clone(),
                                ..base_of(e)
                            }),
                        ));
                    }
                    Err(err) => report.failed.push((e.path.clone(), err.to_string())),
                }
            }
        }
    }
    let (store, key, all_ok) = (
        env.store.clone(),
        copy.key.clone(),
        report.failed.is_empty(),
    );
    blocking(move || {
        store.update_file_bases(&key, &updates).map_err(local)?;
        if all_ok {
            store.set_file_copy_seen(&key, head).map_err(local)?;
        }
        Ok(())
    })
    .await?;
    Ok(report)
}

/// Write the hub's version of `e` to a new local conflict copy name.
async fn conflict_copy(
    env: &Env,
    root: &Path,
    e: &IndexEntry,
    machine: &str,
) -> Result<String, CopyError> {
    for n in 1..=100 {
        let name = conflict_path(&e.path, machine, e.at, n);
        let (r, nm, data) = (root.to_path_buf(), name.clone(), env.data_dir.clone());
        let free = blocking(move || {
            Target {
                root: &r,
                data_dir: Some(&data),
            }
            .is(&nm, &Expect::Absent)
            .map_err(local)
        })
        .await?;
        if free {
            write_entry_at(env, root, e, &name, &Expect::Absent).await?;
            return Ok(name);
        }
    }
    Err(local("no free conflict copy name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, version: i64, content: Option<&str>) -> IndexEntry {
        IndexEntry {
            path: path.into(),
            version,
            content: content.map(|h| EntryContent::Blob { hash: h.into() }),
            size: 1,
            mode_x: false,
            mtime: 0,
            by_machine: "m".into(),
            at: 0,
        }
    }

    fn base(version: i64, content: &str) -> Base {
        Base {
            version,
            content: Some(EntryContent::Blob {
                hash: content.into(),
            }),
            mode_x: false,
            skipped: false,
            rejected: None,
        }
    }

    fn has(h: &str) -> Local {
        Local::Has(EntryContent::Blob { hash: h.into() })
    }

    #[test]
    fn apply_decisions() {
        let e = entry("f", 5, Some("new"));
        let b = base(3, "old");
        // Up to date.
        assert_eq!(
            decide(&entry("f", 3, Some("x")), Some(&b), &has("old"), false),
            None
        );
        // Unchanged since base: fast-forward over exactly that.
        assert_eq!(
            decide(&e, Some(&b), &has("old"), false),
            Some(Act::Write(Expect::Blob("old".into())))
        );
        // Changed here: conflict copy, never an overwrite.
        assert_eq!(
            decide(&e, Some(&b), &has("mine"), false),
            Some(Act::ConflictCopy)
        );
        // Already there.
        assert_eq!(decide(&e, Some(&b), &has("new"), false), Some(Act::Ack));
        // Deleted here, changed there: modify wins.
        assert_eq!(
            decide(&e, Some(&b), &Local::Missing, false),
            Some(Act::Write(Expect::Absent))
        );
        // New file.
        assert_eq!(
            decide(&e, None, &Local::Missing, false),
            Some(Act::Write(Expect::Absent))
        );
        // An origin's content the hub kept as a conflict copy may be replaced.
        let rej = Base {
            rejected: Some("mine".into()),
            ..b.clone()
        };
        assert_eq!(
            decide(&e, Some(&rej), &has("mine"), false),
            Some(Act::Write(Expect::Blob("mine".into())))
        );
        // Tombstones: delete only what is unchanged; keep edits.
        let t = entry("f", 5, None);
        assert_eq!(
            decide(&t, Some(&b), &has("old"), false),
            Some(Act::Delete(Expect::Blob("old".into())))
        );
        assert_eq!(decide(&t, Some(&b), &has("mine"), false), Some(Act::Rebase));
        assert_eq!(
            decide(&t, Some(&b), &Local::Missing, false),
            Some(Act::DropBase)
        );
        assert_eq!(decide(&t, None, &has("x"), false), Some(Act::Rebase));
        // A fresh copy takes the hub's state over the clone's.
        assert_eq!(
            decide(&t, None, &has("x"), true),
            Some(Act::Delete(Expect::Blob("x".into())))
        );
        assert_eq!(
            decide(&e, None, &has("x"), true),
            Some(Act::Write(Expect::Blob("x".into())))
        );
        assert_eq!(
            decide(&e, None, &Local::Other, false),
            Some(Act::Skip("a folder is in the way".into()))
        );
    }

    #[test]
    fn mass_delete_threshold() {
        // At least 50, and more than 30% of the synced files.
        assert!(!mass_delete(50, 10));
        assert!(mass_delete(51, 10));
        assert!(!mass_delete(300, 1000));
        assert!(mass_delete(301, 1000));
        assert!(!mass_delete(0, 0));
    }

    proptest::proptest! {
        // Deletes the guard lets through are never a large share of a
        // larger folder, and small edits always go through.
        #[test]
        fn mass_delete_guard_bounds(deletes in 0usize..5000, synced in 0usize..20000) {
            if !mass_delete(deletes, synced) {
                proptest::prop_assert!(deletes <= 50 || deletes * 100 <= synced * 30);
            }
            if deletes <= 50 {
                proptest::prop_assert!(!mass_delete(deletes, synced));
            }
        }
    }

    #[test]
    fn upload_plan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("excluded.log"), "x").unwrap();
        let file = |p: &str, h: &str| Hashed {
            path: p.into(),
            size: 1,
            mtime_ns: 2_000_000,
            mode_x: false,
            content: Content::Blob(h.into()),
        };
        let bases = HashMap::from([
            ("same".to_string(), base(1, "a")),
            ("changed".to_string(), base(2, "a")),
            ("deleted".to_string(), base(3, "a")),
            ("excluded.log".to_string(), base(4, "a")),
            ("unreadable".to_string(), base(5, "a")),
            ("locked/x".to_string(), base(7, "a")),
            (
                "refused".to_string(),
                Base {
                    rejected: Some("b".into()),
                    ..base(6, "a")
                },
            ),
        ]);
        let files = [
            file("same", "a"),
            file("changed", "b"),
            file("new", "c"),
            file("refused", "b"),
        ];
        let unreadable = HashSet::from(["unreadable".to_string()]);
        let dirs = vec!["locked".to_string()];
        let plan = plan_upload(root, &files, &bases, &unreadable, &dirs, true);
        assert!(plan.drop_bases.is_empty());
        let got: Vec<(&str, i64, &str)> = plan
            .changes
            .iter()
            .map(|p| {
                let op = match &p.change.op {
                    ChangeOp::Put { .. } => "put",
                    ChangeOp::Link { .. } => "link",
                    ChangeOp::Delete => "delete",
                    ChangeOp::Forget => "forget",
                };
                (p.change.path.as_str(), p.change.base_version, op)
            })
            .collect();
        assert_eq!(
            got,
            [
                ("changed", 2, "put"),
                ("new", 0, "put"),
                ("deleted", 3, "delete"),
                ("excluded.log", 4, "forget")
            ]
        );
        // A copy's own exclusions only drop its base; the hub keeps the file.
        let plan = plan_upload(root, &files, &bases, &unreadable, &dirs, false);
        assert_eq!(plan.drop_bases, ["excluded.log"]);
        assert!(!plan.changes.iter().any(|p| p.change.path == "refused"));
        // A folder the scan could not list at all deletes nothing.
        let plan = plan_upload(root, &[], &bases, &HashSet::new(), &[String::new()], true);
        assert!(plan.changes.is_empty(), "{:?}", plan.changes);
    }
}
