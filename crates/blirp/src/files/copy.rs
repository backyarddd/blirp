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
use blirp_core::files::write::{Expect, RETRY_BUDGET, RetryBudget, Target, WriteError};
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
    /// Serializes passes that write bases (uploads, the write phase of an
    /// apply): taken inside [`upload`], [`apply`] and [`restore_missing`],
    /// never by their callers.
    // ponytail: one lock for all copies; per-copy locks if folders queue.
    pub work: Arc<tokio::sync::Mutex<()>>,
    /// Called between an upload's scan and its blob uploads (tests change
    /// files there); None in the daemon.
    pub after_scan: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Retry time for locked files, shared by the writes of one pass
    /// (reset when a pass takes `work`).
    pub retry: Arc<RetryBudget>,
}

/// One working copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    /// `file_copies.path`: the folder as registered.
    pub key: String,
    pub root_id: String,
    /// This machine's own folder: upload-only, it registers the root.
    pub origin: bool,
    /// The hub root incarnation its bases belong to ("" until known).
    pub incarnation: String,
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

/// This machine's scan settings, with files larger than the hub accepts
/// (as last heard; never connects) left out as too large.
pub fn scan_config(env: &Env) -> ScanConfig {
    let mut cfg = env.scan.clone();
    let hub = env.hub.max_file();
    if hub > 0 {
        cfg.max_file_bytes = cfg.max_file_bytes.min(hub);
    }
    cfg
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

/// Mass-delete guard: a pass deleting at least this many files ...
pub const MASS_DELETE_MIN: usize = 10;
/// ... and more than this share (percent) of the root's synced files, or
/// every synced file of a folder that had more than one, commits nothing.
pub const MASS_DELETE_PERCENT: usize = 30;

/// Whether deleting `deletes` of `synced` files needs a confirmation.
pub fn mass_delete(deletes: usize, synced: usize) -> bool {
    (synced > 1 && deletes >= synced)
        || (deletes >= MASS_DELETE_MIN
            && deletes.saturating_mul(100) > synced.saturating_mul(MASS_DELETE_PERCENT))
}

/// How an upload treats many deletions at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deletes {
    /// Hold them (the default).
    Guard,
    /// Hold any delete, however few: the folder was just missing, and what
    /// came back may be an empty folder made in its place.
    HoldAll,
    /// The user confirmed deleting these paths (what the folder showed);
    /// other deletes wait for the next pass.
    Confirm(HashSet<String>),
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

/// An upload plan: changes to send, paths this copy leaves out without
/// telling the hub (its own exclusions), and paths held after a refused
/// change.
#[derive(Debug, Default)]
struct UploadPlan {
    changes: Vec<Planned>,
    left_out: Vec<String>,
    held: Vec<String>,
}

/// Whether `path` exists under exactly this name, reached through real
/// folders. On case-insensitive filesystems a file or folder renamed only
/// in case still answers to its old name; that old name is gone (a
/// delete), not excluded, so every component is compared exactly. A folder
/// along the way replaced by a symlink makes the path gone too (the scan
/// never follows it).
fn exists_exact(root: &Path, path: &str) -> std::io::Result<bool> {
    let parts: Vec<&str> = path.split('/').collect();
    let mut dir = root.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        let next = dir.join(part);
        let meta = match std::fs::symlink_metadata(&next) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if i + 1 < parts.len() && !meta.is_dir() {
            return Ok(false);
        }
        if case_insensitive_fs() {
            let mut found = false;
            for e in std::fs::read_dir(&dir)? {
                if e?.file_name().to_str() == Some(*part) {
                    found = true;
                    break;
                }
            }
            if !found {
                return Ok(false);
            }
        }
        dir = next;
    }
    Ok(true)
}

/// Changes of `files` (what the scan found) against `bases`. Paths that
/// could not be read this time, or lie in a folder that could not be
/// listed, are left alone. A vanished path the scan left out (`excluded`:
/// paths, folders ending in `/`) is excluded now: the origin stops syncing
/// it for everyone, a copy only leaves it out here. One that exists but the
/// scan did not see at all appeared after it: the next pass takes it. Only
/// a path confirmed gone is deleted.
fn plan_upload(
    root: &Path,
    files: &[Hashed],
    bases: &HashMap<String, Base>,
    unreadable: &HashSet<String>,
    unreadable_dirs: &[String],
    excluded: &[String],
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
            // A skipped path found under its exact name (no longer left out
            // here, or made by hand) uploads as new (base 0): this copy never
            // held the hub's version, so the hub keeps it and the local one
            // becomes a conflict copy, never a silent overwrite.
            if b.content.as_ref() == Some(&content) && b.mode_x == mode_x {
                continue;
            }
            // The hub refused this path's last change: it waits for "Bring
            // changes here" (or Update from hub) instead of making a new
            // conflict copy with every save.
            if b.rejected.is_some() {
                out.held.push(f.path.clone());
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
                base_version: base.filter(|b| !b.skipped).map_or(0, |b| b.version),
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
        .filter(|(_, b)| b.rejected.is_none())
        .collect();
    gone.sort_by(|a, b| a.0.cmp(b.0));
    for (path, b) in gone {
        let left_out = excluded
            .iter()
            .any(|e| e == path || (e.ends_with('/') && path.starts_with(e.as_str())));
        let op = match exists_exact(root, path) {
            Ok(true) if !left_out => continue,
            Ok(true) if origin => ChangeOp::Forget,
            Ok(true) => {
                out.left_out.push(path.clone());
                continue;
            }
            Ok(false) => ChangeOp::Delete,
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

/// Scan, hash and upload `copy`'s changes (an origin registers its root).
pub async fn upload(env: &Env, copy: &Copy) -> Result<UploadReport, CopyError> {
    upload_with(env, copy, Deletes::Guard).await
}

/// [`upload`], with the mass-delete guard on or some deletes confirmed.
pub async fn upload_with(
    env: &Env,
    copy: &Copy,
    deletes: Deletes,
) -> Result<UploadReport, CopyError> {
    let _work = env.work.lock().await;
    env.retry.reset(RETRY_BUDGET);
    let root = copy.root();
    let cfg = scan_config(env);
    let ls: LocalScan = {
        let _permit = env
            .gate
            .acquire()
            .await
            .map_err(|e| local(format!("file scanner stopped: {e}")))?;
        let (store, key, r) = (env.store.clone(), copy.key.clone(), root.clone());
        blocking(move || local::scan_copy(&store, &key, &r, &cfg).map_err(local)).await?
    };
    // Temp files a crash left behind; nothing else would remove them.
    for t in &ls.scan.stale_temp {
        if let Err(e) = std::fs::remove_file(t) {
            tracing::debug!(error = %e, "removing a stale temp file failed");
        }
    }
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
    if let Some(hook) = &env.after_scan {
        hook();
    }
    let (store, key) = (env.store.clone(), copy.key.clone());
    let bases = blocking(move || store.file_bases(&key).map_err(local)).await?;
    let unreadable: HashSet<String> = ls.hashed.unreadable.iter().cloned().collect();
    let dirs = ls.scan.unreadable.clone();
    let excluded: Vec<String> = ls
        .scan
        .excluded
        .iter()
        .map(|e| {
            if e.dir {
                format!("{}/", e.path)
            } else {
                e.path.clone()
            }
        })
        .chain(ls.hashed.secrets.iter().cloned())
        .collect();
    let (r, files, origin) = (root.clone(), ls.hashed.files.clone(), copy.origin);
    let b2 = bases.clone();
    let planned = blocking(move || {
        Ok(plan_upload(
            &r,
            &files,
            &b2,
            &unreadable,
            &dirs,
            &excluded,
            origin,
        ))
    })
    .await?;
    report.pending += planned.held.len();
    if !planned.left_out.is_empty() {
        // Marked skipped, not dropped: without a base the next apply would
        // see the hub's file as new here and make a conflict copy of it
        // with every pass. Should it sync again, it uploads as new.
        let marks: Vec<(String, Option<Base>)> = planned
            .left_out
            .into_iter()
            .filter_map(|p| {
                let b = bases.get(&p)?.clone();
                Some((p, Some(Base { skipped: true, ..b })))
            })
            .collect();
        let (store, key) = (env.store.clone(), copy.key.clone());
        blocking(move || store.update_file_bases(&key, &marks).map_err(local)).await?;
    }
    let mut plan = planned.changes;
    if plan.is_empty() {
        return Ok(report);
    }
    // An emptied or swapped folder, not an edit: commit nothing until the
    // user says what happened. A confirmation covers what it listed only.
    if let Deletes::Confirm(ok) = &deletes {
        plan.retain(|p| p.change.op != ChangeOp::Delete || ok.contains(&p.change.path));
    }
    let doomed: Vec<String> = plan
        .iter()
        .filter(|p| p.change.op == ChangeOp::Delete)
        .map(|p| p.change.path.clone())
        .collect();
    let synced = bases
        .values()
        .filter(|b| b.content.is_some() && !b.skipped)
        .count();
    let hold = match deletes {
        Deletes::Guard => mass_delete(doomed.len(), synced),
        Deletes::HoldAll => !doomed.is_empty(),
        Deletes::Confirm(_) => false,
    };
    if hold {
        tracing::warn!(
            files = doomed.len(),
            synced,
            "many files disappeared from a synced folder; holding the upload"
        );
        report.held_deletes = doomed;
        return Ok(report);
    }

    let manifest = if copy.origin && ls.git {
        let r = root.clone();
        Some(blocking(move || Ok(local::git_manifest(&r))).await?)
    } else {
        None
    };
    let claim = copy.origin.then(|| copy.key.clone());
    // Batch by batch: its blobs, then its commit, so no blob waits long
    // between upload and the commit that uses it.
    // Learned from the first commit when the copy had none yet, and named
    // in every later batch of this pass.
    let mut incarnation = copy.incarnation.clone();
    for (i, batch) in plan.chunks(MAX_CHANGES).enumerate() {
        let failed = upload_blobs(env, &root, batch).await?;
        let batch: Vec<&Planned> = batch
            .iter()
            .filter(
                |p| !matches!(&p.change.op, ChangeOp::Put { hash, .. } if failed.contains(hash)),
            )
            .collect();
        report.pending += failed.len();
        if batch.is_empty() {
            continue;
        }
        let changes: Vec<FileChange> = batch.iter().map(|p| p.change.clone()).collect();
        let expected = (!incarnation.is_empty()).then_some(incarnation.as_str());
        let out = match env
            .hub
            .commit(
                &copy.root_id,
                claim.as_deref(),
                expected,
                if i == 0 { manifest.as_ref() } else { None },
                changes,
            )
            .await
        {
            Err(SyncError::Remote { code, .. }) if code == "root_replaced" => {
                return Err(replaced(env, copy).await);
            }
            r => r?,
        };
        report.head = Some(out.head);
        if incarnation.is_empty() {
            incarnation.clone_from(&out.incarnation);
            let (store, key, inc) = (env.store.clone(), copy.key.clone(), out.incarnation.clone());
            blocking(move || {
                store
                    .set_file_copy_incarnation(&key, &inc, false)
                    .map_err(local)
            })
            .await?;
        }
        let mut updates: Vec<(String, Option<Base>)> = Vec::new();
        for (p, result) in batch.into_iter().zip(out.results) {
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

/// Upload the blobs of `batch` the hub lacks (a few at once). Returns the
/// hashes whose file changed since it was hashed: those changes wait for
/// the next pass.
async fn upload_blobs(
    env: &Env,
    root: &Path,
    batch: &[Planned],
) -> Result<HashSet<String>, CopyError> {
    let mut sources: HashMap<String, (PathBuf, u64)> = HashMap::new();
    for p in batch {
        if let (ChangeOp::Put { hash, .. }, Some(l)) = (&p.change.op, &p.local) {
            sources
                .entry(hash.clone())
                .or_insert_with(|| (wpath::to_local(root, &l.path), l.size));
        }
    }
    if sources.is_empty() {
        return Ok(HashSet::new());
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
    let mut failed = HashSet::new();
    for (hash, r) in results {
        match r {
            Ok(()) => {}
            // This file only (it changed while uploading, or is larger than
            // the hub accepts now): it waits, the rest of the folder goes on.
            Err(SyncError::Remote { code, .. })
                if code == "file_changed" || code == "hash_mismatch" || code == "too_large" =>
            {
                if code == "too_large"
                    && let Some((path, _)) = sources.get(&hash)
                {
                    tracing::warn!(path = %path.display(), "the hub refused a file as too large");
                }
                failed.insert(hash);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(failed)
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
        let retry = env.retry.clone();
        let written = blocking(move || {
            let t = Target {
                root: &r,
                data_dir: Some(&data),
                retry: Some(&*retry),
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
    write_entry_from(env, root, e, at, expect, None).await
}

/// [`write_entry_at`], from a blob downloaded already (`prefetched`, kept
/// for other paths with the same content) or downloading it now.
async fn write_entry_from(
    env: &Env,
    root: &Path,
    e: &IndexEntry,
    at: &str,
    expect: &Expect,
    prefetched: Option<&Path>,
) -> Result<(), CopyError> {
    let data = env.data_dir.clone();
    match &e.content {
        Some(EntryContent::Blob { hash }) => {
            let owned = prefetched.is_none();
            let part = match prefetched {
                Some(p) => p.to_path_buf(),
                None => env.hub.download(hash).await?,
            };
            let (r, at, expect, hash, mode_x, mtime, p) = (
                root.to_path_buf(),
                at.to_string(),
                expect.clone(),
                hash.clone(),
                e.mode_x,
                e.mtime,
                part.clone(),
            );
            let retry = env.retry.clone();
            let res = blocking(move || {
                let t = Target {
                    root: &r,
                    data_dir: Some(&data),
                    retry: Some(&*retry),
                };
                let mut f = std::fs::File::open(&p).map_err(local)?;
                t.write_verified(&at, &expect, &mut f, &hash, mode_x, Some(mtime))
                    .map_err(|e| match e {
                        WriteError::Changed(p) => CopyError::Local(format!("{p} changed locally")),
                        other => local(other),
                    })
            })
            .await;
            if owned {
                let _ = tokio::fs::remove_file(&part).await;
            }
            res
        }
        Some(EntryContent::Link { target }) => {
            let (r, at, expect, target) = (
                root.to_path_buf(),
                at.to_string(),
                expect.clone(),
                target.clone(),
            );
            let retry = env.retry.clone();
            blocking(move || {
                let t = Target {
                    root: &r,
                    data_dir: Some(&data),
                    retry: Some(&*retry),
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

/// Whether something exists at `path` under a name that differs from it
/// only in case (a case-insensitive lookup finds another file). Only asked
/// on case-insensitive systems.
fn other_case_on_disk(root: &Path, path: &str) -> bool {
    if std::fs::symlink_metadata(wpath::to_local(root, path)).is_err() {
        return false;
    }
    let mut dir = root.to_path_buf();
    for part in path.split('/') {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return false;
        };
        let names: Vec<String> = rd
            .filter_map(|d| d.ok())
            .map(|d| d.file_name().to_string_lossy().into_owned())
            .collect();
        if !names.iter().any(|n| n == part) {
            let key = case_key(part);
            return names.iter().any(|n| case_key(n) == key);
        }
        dir.push(part);
    }
    false
}

/// Decide one path. `fresh`: a copy being created, where the hub wins over
/// whatever the clone checked out. `live` holds the content hashes the hub
/// has now: content the hub once refused may only be replaced while it is
/// still there (in its conflict copy).
fn decide(
    e: &IndexEntry,
    base: Option<&Base>,
    now: &Local,
    fresh: bool,
    live: &HashSet<&str>,
) -> Option<Act> {
    if base.is_some_and(|b| b.version >= e.version) {
        return None;
    }
    let matches_base = |c: &EntryContent| base.is_some_and(|b| b.content.as_ref() == Some(c));
    let preserved = |c: &EntryContent| {
        base.is_some_and(|b| b.rejected.as_deref() == Some(content_hash(c)))
            && live.contains(content_hash(c))
    };
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

/// Decisions for every path of `index` against `bases` and the disk.
fn plan_incoming(
    root: &Path,
    entries: &[IndexEntry],
    bases: &HashMap<String, Base>,
    fresh: bool,
) -> Vec<Incoming> {
    let live: HashSet<&str> = entries
        .iter()
        .filter_map(|e| e.content.as_ref().map(content_hash))
        .collect();
    // Case-only collisions on case-insensitive systems: the name this copy
    // holds (a synced base, or the file on disk) keeps it; of new ones, the
    // first is written. The others are skipped here and stay on the hub.
    let ci = case_insensitive_fs();
    let mut taken: HashMap<String, &str> = HashMap::new();
    if ci {
        for e in entries {
            let held = bases
                .get(&e.path)
                .is_some_and(|b| !b.skipped && b.content.is_some());
            if e.content.is_some() && held {
                taken.entry(case_key(&e.path)).or_insert(e.path.as_str());
            }
        }
    }
    let mut out = Vec::new();
    let mut on_hub: HashSet<&str> = HashSet::new();
    for e in entries {
        on_hub.insert(e.path.as_str());
        if wpath::check(&e.path).is_err() {
            continue;
        }
        let mut base = bases.get(&e.path);
        if base.is_some_and(|b| b.skipped) && e.content.is_some() {
            // Skipped, yet here under this exact name: left out on this
            // machine (its own exclusions). Never written over, and the
            // base keeps the version it had: should the file sync again,
            // its upload is compared against that one, so a newer hub
            // version makes a conflict copy instead of being replaced.
            if exists_exact(root, &e.path).unwrap_or(true) {
                continue;
            }
        }
        // A case collision is looked at again: the name it collided with
        // may be gone.
        let skipped = ci && base.is_some_and(|b| b.skipped && b.version >= e.version);
        if skipped {
            base = None;
        } else if base.is_some_and(|b| b.version >= e.version) {
            continue;
        }
        let collides = ci
            && e.content.is_some()
            && (taken.get(&case_key(&e.path)).is_some_and(|p| *p != e.path)
                || other_case_on_disk(root, &e.path));
        if ci && e.content.is_some() && !collides {
            taken.insert(case_key(&e.path), e.path.as_str());
        }
        let act = if collides {
            Some(Act::Skip(
                "another file has the same name in a different case".into(),
            ))
        } else {
            decide(e, base, &local_state(root, &e.path), fresh, &live)
        };
        // Still skipped: nothing new to do or report.
        if skipped && matches!(act, Some(Act::Skip(_))) {
            continue;
        }
        if let Some(act) = act {
            out.push(Incoming {
                entry: e.clone(),
                act,
            });
        }
    }
    // Bases of paths the hub no longer lists (forgotten, or tombstones past
    // retention). A file still as it was synced keeps its base, so it is
    // not uploaded again as new; otherwise only the base goes (a changed
    // file uploads as the change it is).
    for (p, b) in bases {
        if on_hub.contains(p.as_str()) {
            continue;
        }
        let unchanged =
            matches!(local_state(root, p), Local::Has(c) if b.content.as_ref() == Some(&c));
        if !unchanged {
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
    out
}

/// The hub's index of `copy`'s root, checked against the incarnation its
/// bases belong to.
async fn root_index(env: &Env, copy: &Copy) -> Result<blirp_sync::files::RootIndex, CopyError> {
    let index = match env.hub.index(&copy.root_id, 0).await {
        Err(SyncError::Remote { code, .. }) if code == "root_replaced" => {
            return Err(replaced(env, copy).await);
        }
        r => r?,
    };
    if !copy.incarnation.is_empty() && index.incarnation != copy.incarnation {
        return Err(replaced(env, copy).await);
    }
    Ok(index)
}

/// The hub copy was deleted and made again: every base of `copy` belongs
/// to the old one. An origin starts over (everything uploads again, with
/// no incarnation until the hub names the new one); any other copy
/// detaches, keeping its files.
async fn replaced(env: &Env, copy: &Copy) -> CopyError {
    let (store, key, origin) = (env.store.clone(), copy.key.clone(), copy.origin);
    let r = blocking(move || {
        if origin {
            store.set_file_copy_incarnation(&key, "", true)
        } else {
            store.detach_file_copy(&key)
        }
        .map_err(local)
    })
    .await;
    match r {
        Ok(()) => CopyError::Hub {
            code: "root_replaced".into(),
            message: if origin {
                "the hub copy was made again; uploading everything".into()
            } else {
                "the hub copy was made again; this copy no longer syncs".into()
            },
        },
        Err(e) => e,
    }
}

/// The hub's changes this copy has not taken, and the root's head.
pub async fn incoming(
    env: &Env,
    copy: &Copy,
    fresh: bool,
) -> Result<(Vec<Incoming>, i64), CopyError> {
    let index = root_index(env, copy).await?;
    let (store, key, root) = (env.store.clone(), copy.key.clone(), copy.root());
    blocking(move || {
        let bases = store.file_bases(&key).map_err(local)?;
        Ok((
            plan_incoming(&root, &index.entries, &bases, fresh),
            index.head,
        ))
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
    let _work = env.work.lock().await;
    env.retry.reset(RETRY_BUDGET);
    let root = copy.root();
    if !std::fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir()) {
        return Err(local("the folder is gone; nothing was restored"));
    }
    let index = root_index(env, copy).await?;
    let (store, key) = (env.store.clone(), copy.key.clone());
    let bases = blocking(move || store.file_bases(&key).map_err(local)).await?;
    let mut report = ApplyReport {
        head: index.head,
        ..Default::default()
    };
    for e in index.entries {
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

/// Progress of an apply: blobs downloaded so far, of how many.
pub type Progress<'a> = &'a (dyn Fn(usize, usize) + Send + Sync);

/// Download the blobs `todo` will write, a few at once and each once.
/// Parts that fail are left out (their paths download again, and report
/// the error, when written).
async fn prefetch(
    env: &Env,
    todo: &[Incoming],
    progress: Option<Progress<'_>>,
) -> HashMap<String, PathBuf> {
    let mut hashes: Vec<String> = todo
        .iter()
        .filter(|i| matches!(i.act, Act::Write(_) | Act::ConflictCopy))
        .filter_map(|i| match &i.entry.content {
            Some(EntryContent::Blob { hash }) => Some(hash.clone()),
            _ => None,
        })
        .collect();
    hashes.sort();
    hashes.dedup();
    let total = hashes.len();
    let done = std::sync::atomic::AtomicUsize::new(0);
    futures_util::stream::iter(hashes)
        .map(|h| {
            let hub = env.hub.clone();
            let done = &done;
            async move {
                let r = hub.download(&h).await;
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if let Some(p) = progress {
                    p(n, total);
                }
                match r {
                    Ok(part) => Some((h, part)),
                    Err(e) => {
                        tracing::debug!(error = %e, "prefetching a file from the hub failed");
                        None
                    }
                }
            }
        })
        .buffer_unordered(blirp_sync::files::proto::STREAMS)
        .filter_map(|x| async move { x })
        .collect()
        .await
}

/// Take the hub's changes into `copy`: fast-forward what is unchanged
/// since its base, keep local changes and write the hub's versions next to
/// them. `fresh` for a copy being created (the hub wins over the clone, and
/// the clone's files the hub does not have are recorded as they are, never
/// uploaded as edits). Blobs download first, outside the work lock; the
/// writes then run under it against a fresh look at the folder.
pub async fn apply(env: &Env, copy: &Copy, fresh: bool) -> Result<ApplyReport, CopyError> {
    apply_with(env, copy, fresh, None).await
}

/// [`apply`] reporting download progress.
pub async fn apply_with(
    env: &Env,
    copy: &Copy,
    fresh: bool,
    progress: Option<Progress<'_>>,
) -> Result<ApplyReport, CopyError> {
    let (first, _) = incoming(env, copy, fresh).await?;
    let parts = prefetch(env, &first, progress).await;
    let result = apply_locked(env, copy, fresh, &parts).await;
    for p in parts.values() {
        let _ = tokio::fs::remove_file(p).await;
    }
    result
}

async fn apply_locked(
    env: &Env,
    copy: &Copy,
    fresh: bool,
    parts: &HashMap<String, PathBuf>,
) -> Result<ApplyReport, CopyError> {
    let _work = env.work.lock().await;
    env.retry.reset(RETRY_BUDGET);
    let index = root_index(env, copy).await?;
    let head = index.head;
    let (store, key, root) = (env.store.clone(), copy.key.clone(), copy.root());
    let entries = index.entries;
    let (mut todo, entries) = blocking(move || {
        let bases = store.file_bases(&key).map_err(local)?;
        let todo = plan_incoming(&root, &entries, &bases, fresh);
        Ok((todo, entries))
    })
    .await?;
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
    let hub_names: HashMap<String, Option<EntryContent>> = entries
        .iter()
        .filter(|e| e.content.is_some())
        .map(|e| (case_key(&e.path), e.content.clone()))
        .collect();
    let part_of = |e: &IndexEntry| match &e.content {
        Some(EntryContent::Blob { hash }) => parts.get(hash).map(PathBuf::as_path),
        _ => None,
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
                // Nothing is written, not even a conflict copy for a name
                // that differs only in case: such a copy would upload as a
                // new file, and copies made on several machines collide
                // again (by case) with each other. The file stays on the
                // hub, in its index and history.
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
                let retry = env.retry.clone();
                let res = blocking(move || {
                    Target {
                        root: &r,
                        data_dir: Some(&data),
                        retry: Some(&*retry),
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
            Act::Write(expect) => {
                match write_entry_from(env, &root, e, &e.path, &expect, part_of(e)).await {
                    Ok(()) => {
                        report.written += 1;
                        updates.push((e.path.clone(), Some(base_of(e))));
                    }
                    Err(err) => report.failed.push((e.path.clone(), err.to_string())),
                }
            }
            Act::ConflictCopy => {
                let name = names
                    .get(&e.by_machine)
                    .cloned()
                    .unwrap_or_else(|| "hub".into());
                match conflict_copy(env, &root, e, &name, part_of(e), &hub_names).await {
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
    if fresh {
        // The clone's files the hub does not know: recorded as they are
        // (version 0), so they are not uploaded as edits of this copy.
        let on_hub: HashSet<String> = entries.iter().map(|e| e.path.clone()).collect();
        let (store, key, r, cfg) = (
            env.store.clone(),
            copy.key.clone(),
            root.clone(),
            env.scan.clone(),
        );
        let written: HashSet<String> = updates.iter().map(|(p, _)| p.clone()).collect();
        let recorded = blocking(move || {
            let ls = local::scan_copy(&store, &key, &r, &cfg).map_err(local)?;
            Ok(ls
                .hashed
                .files
                .into_iter()
                .filter(|f| !on_hub.contains(&f.path) && !written.contains(&f.path))
                .map(|f| {
                    let content = Some(to_content(&f.content));
                    (
                        f.path,
                        Some(Base {
                            version: 0,
                            content,
                            mode_x: f.mode_x,
                            skipped: false,
                            rejected: None,
                        }),
                    )
                })
                .collect::<Vec<_>>())
        })
        .await?;
        updates.extend(recorded);
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
    prefetched: Option<&Path>,
    on_hub: &HashMap<String, Option<EntryContent>>,
) -> Result<String, CopyError> {
    for n in 1..=100 {
        let name = conflict_path(&e.path, machine, e.at, n);
        // The same copy made by another machine (names are deterministic)
        // is already on the hub: it arrives like any file, never twice.
        // Another file under a name equal but for case is avoided too, so
        // copies never collide on case-insensitive systems.
        match on_hub.get(&case_key(&name)) {
            Some(c) if *c == e.content => return Ok(name),
            Some(_) => continue,
            None => {}
        }
        let (r, nm, data) = (root.to_path_buf(), name.clone(), env.data_dir.clone());
        let retry = env.retry.clone();
        let free = blocking(move || {
            Target {
                root: &r,
                data_dir: Some(&data),
                retry: Some(&*retry),
            }
            .is(&nm, &Expect::Absent)
            .map_err(local)
        })
        .await?;
        if free {
            write_entry_from(env, root, e, &name, &Expect::Absent, prefetched).await?;
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
        let live: HashSet<&str> = HashSet::from(["mine"]);
        let e = entry("f", 5, Some("new"));
        let b = base(3, "old");
        // Up to date.
        assert_eq!(
            decide(
                &entry("f", 3, Some("x")),
                Some(&b),
                &has("old"),
                false,
                &live
            ),
            None
        );
        // Unchanged since base: fast-forward over exactly that.
        assert_eq!(
            decide(&e, Some(&b), &has("old"), false, &live),
            Some(Act::Write(Expect::Blob("old".into())))
        );
        // Changed here: conflict copy, never an overwrite.
        assert_eq!(
            decide(&e, Some(&b), &has("mine"), false, &live),
            Some(Act::ConflictCopy)
        );
        // Already there.
        assert_eq!(
            decide(&e, Some(&b), &has("new"), false, &live),
            Some(Act::Ack)
        );
        // Deleted here, changed there: modify wins.
        assert_eq!(
            decide(&e, Some(&b), &Local::Missing, false, &live),
            Some(Act::Write(Expect::Absent))
        );
        // New file.
        assert_eq!(
            decide(&e, None, &Local::Missing, false, &live),
            Some(Act::Write(Expect::Absent))
        );
        // Content the hub refused may be replaced while the hub still
        // has it (its conflict copy) ...
        let rej = Base {
            rejected: Some("mine".into()),
            ..b.clone()
        };
        assert_eq!(
            decide(&e, Some(&rej), &has("mine"), false, &live),
            Some(Act::Write(Expect::Blob("mine".into())))
        );
        // ... and not once it is gone (copy cap, deleted, forgotten).
        assert_eq!(
            decide(&e, Some(&rej), &has("mine"), false, &HashSet::new()),
            Some(Act::ConflictCopy)
        );
        // Tombstones: delete only what is unchanged; keep edits.
        let t = entry("f", 5, None);
        assert_eq!(
            decide(&t, Some(&b), &has("old"), false, &live),
            Some(Act::Delete(Expect::Blob("old".into())))
        );
        assert_eq!(
            decide(&t, Some(&b), &has("mine"), false, &live),
            Some(Act::Rebase)
        );
        assert_eq!(
            decide(&t, Some(&b), &Local::Missing, false, &live),
            Some(Act::DropBase)
        );
        assert_eq!(decide(&t, None, &has("x"), false, &live), Some(Act::Rebase));
        // A fresh copy takes the hub's state over the clone's.
        assert_eq!(
            decide(&t, None, &has("x"), true, &live),
            Some(Act::Delete(Expect::Blob("x".into())))
        );
        assert_eq!(
            decide(&e, None, &has("x"), true, &live),
            Some(Act::Write(Expect::Blob("x".into())))
        );
        assert_eq!(
            decide(&e, None, &Local::Other, false, &live),
            Some(Act::Skip("a folder is in the way".into()))
        );
    }

    #[test]
    fn mass_delete_threshold() {
        // Emptying a folder of more than one file.
        assert!(mass_delete(40, 40));
        assert!(mass_delete(10, 10));
        assert!(mass_delete(5, 5));
        assert!(mass_delete(2, 2));
        assert!(!mass_delete(1, 1));
        // At least 10 and more than 30%.
        assert!(!mass_delete(9, 20));
        assert!(mass_delete(10, 20));
        assert!(!mass_delete(300, 1000));
        assert!(mass_delete(301, 1000));
        assert!(!mass_delete(0, 0));
        assert!(!mass_delete(3, 40));
    }

    proptest::proptest! {
        // Deletes the guard lets through are never a large share of a
        // larger folder, and small edits always go through.
        #[test]
        fn mass_delete_guard_bounds(deletes in 0usize..5000, synced in 0usize..20000) {
            let deletes = deletes.min(synced);
            if !mass_delete(deletes, synced) {
                // What passes is a small share, and never a whole folder.
                proptest::prop_assert!(deletes < 10 || deletes * 100 <= synced * 30);
                proptest::prop_assert!(synced <= 1 || deletes < synced);
            }
            // A few deletes from a larger folder always go through.
            if deletes < 10 && deletes < synced {
                proptest::prop_assert!(!mass_delete(deletes, synced));
            }
        }
    }

    #[test]
    fn skipped_paths_upload_as_new_and_case_renames_are_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Skipped here (a collision, a folder in the way): the hub's version
        // was never held, so the file found there now goes up as new.
        let bases = HashMap::from([(
            "held.txt".to_string(),
            Base {
                skipped: true,
                ..base(9, "hub")
            },
        )]);
        let files = [Hashed {
            path: "held.txt".into(),
            size: 1,
            mtime_ns: 2_000_000,
            mode_x: false,
            content: Content::Blob("mine".into()),
        }];
        let plan = plan_upload(root, &files, &bases, &HashSet::new(), &[], &[], false);
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].change.base_version, 0);
        // A folder renamed only in case: its old name is gone.
        std::fs::create_dir_all(root.join("Sub")).unwrap();
        std::fs::write(root.join("Sub/f.txt"), "x").unwrap();
        assert!(exists_exact(root, "Sub/f.txt").unwrap());
        if case_insensitive_fs() {
            assert!(!exists_exact(root, "sub/f.txt").unwrap());
        }
        // A folder replaced by a symlink: what was below it is gone.
        #[cfg(unix)]
        {
            std::fs::create_dir_all(root.join("elsewhere")).unwrap();
            std::fs::write(root.join("elsewhere/g.txt"), "x").unwrap();
            std::os::unix::fs::symlink(root.join("elsewhere"), root.join("linked")).unwrap();
            assert!(!exists_exact(root, "linked/g.txt").unwrap());
            assert!(exists_exact(root, "linked").unwrap());
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
        let excluded = ["excluded.log".to_string()];
        let plan = plan_upload(root, &files, &bases, &unreadable, &dirs, &excluded, true);
        assert!(plan.left_out.is_empty());
        // A path whose last change the hub refused waits.
        assert_eq!(plan.held, ["refused"]);
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
        // A copy's own exclusions are only left out here; the hub keeps
        // the file.
        let plan = plan_upload(root, &files, &bases, &unreadable, &dirs, &excluded, false);
        assert_eq!(plan.left_out, ["excluded.log"]);
        assert!(!plan.changes.iter().any(|p| p.change.path == "refused"));
        // On disk but not seen by the scan, and not left out: made after
        // the scan. Neither forgotten nor deleted; the next pass takes it.
        let plan = plan_upload(root, &files, &bases, &unreadable, &dirs, &[], true);
        assert!(!plan.changes.iter().any(|p| p.change.path == "excluded.log"));
        assert!(plan.left_out.is_empty());
        // A folder the scan could not list at all deletes nothing.
        let plan = plan_upload(
            root,
            &[],
            &bases,
            &HashSet::new(),
            &[String::new()],
            &[],
            true,
        );
        assert!(plan.changes.is_empty(), "{:?}", plan.changes);
    }
}
