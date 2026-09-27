//! The file engine: keeps this machine's folders uploaded to the hub.
//!
//! - Which folders: this machine's project folders (origins) that may sync
//!   and whose project's file sync is on, plus copies downloaded from the
//!   hub (they upload their edits live).
//! - When: after a change settles (watcher, 2 s debounce), at start and
//!   every 10 minutes (a full rescan is the source of truth; it also runs
//!   after watcher errors), and on request. Folders are worked on one at a
//!   time.
//! - First run: uploads wait for the grace period (`files.grace_until`)
//!   while folders are scanned, so the banner can show what will go.

use super::copy::{self, Copy, CopyError, Env};
use crate::state::{AppState, SharedState};
use blirp_core::files::path::TMP_PREFIX;
use blirp_core::files::scan::ScanState;
use blirp_core::files::{FilesMode, RootInfo};
use blirp_core::model::{CopyState, LocalFiles, ServerEvent};
use blirp_core::store::{CopyMode, FileCopy};
use notify_debouncer_full::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Changes settle this long before a folder uploads.
pub const DEBOUNCE: Duration = Duration::from_secs(2);
/// Full rescan period.
pub const RESCAN: Duration = Duration::from_secs(600);
/// First hub collection after start, then how often.
const GC_FIRST: Duration = Duration::from_secs(300);
const GC_EVERY: Duration = Duration::from_secs(24 * 3600);
/// `settings` key: uploads start after this time (unix ms).
pub const GRACE_KEY: &str = "files.grace_until";
/// First-run grace period.
pub const GRACE_MS: i64 = 10 * 60 * 1000;
/// `settings` key: "Pause file sync" on this machine.
pub const PAUSED_KEY: &str = "files.paused";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Maps replaced entry by entry; a poisoned one is still consistent.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

enum Kick {
    Paths(Vec<PathBuf>),
    /// Reconcile the folder list and scan everything.
    Rescan,
    /// Scan this folder now.
    Copy(String),
}

/// A folder the engine tracks.
#[derive(Debug, Clone)]
pub struct Tracked {
    pub copy: Copy,
    pub project_id: String,
    pub never: Option<String>,
    pub effective: bool,
}

pub struct Engine {
    weak: Weak<AppState>,
    pub env: Env,
    status: Mutex<HashMap<String, LocalFiles>>,
    tracked: Mutex<Vec<Tracked>>,
    hub_error: Mutex<Option<String>>,
    modes: Mutex<HashMap<String, FilesMode>>,
    roots: Mutex<Vec<RootInfo>>,
    kick: mpsc::UnboundedSender<Kick>,
    stop: watch::Sender<bool>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Paths each folder's last pass held by the mass-delete guard: what
    /// "Delete on hub too" confirms (nothing more).
    held: Mutex<HashMap<String, Vec<String>>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").finish_non_exhaustive()
    }
}

impl Engine {
    pub fn start(state: &SharedState, env: Env) -> Arc<Engine> {
        let (kick, rx) = mpsc::unbounded_channel();
        let (stop, stop_rx) = watch::channel(false);
        let engine = Arc::new(Engine {
            weak: Arc::downgrade(state),
            env,
            status: Mutex::default(),
            tracked: Mutex::default(),
            hub_error: Mutex::default(),
            modes: Mutex::default(),
            roots: Mutex::default(),
            kick,
            stop,
            task: Mutex::default(),
            held: Mutex::default(),
        });
        let task = tokio::spawn(run(engine.clone(), rx, stop_rx));
        *lock(&engine.task) = Some(task);
        engine
    }

    pub async fn shutdown(&self) {
        let _ = self.stop.send(true);
        let task = lock(&self.task).take();
        if let Some(t) = task
            && tokio::time::timeout(Duration::from_secs(5), t)
                .await
                .is_err()
        {
            tracing::warn!("file engine did not stop in time");
        }
    }

    /// Reconcile and scan everything soon.
    pub fn rescan(&self) {
        let _ = self.kick.send(Kick::Rescan);
    }

    /// Scan one folder soon.
    pub fn touch(&self, key: &str) {
        let _ = self.kick.send(Kick::Copy(key.to_string()));
    }

    pub fn status_of(&self, key: &str) -> Option<LocalFiles> {
        lock(&self.status).get(key).cloned()
    }

    pub fn tracked(&self) -> Vec<Tracked> {
        lock(&self.tracked).clone()
    }

    pub fn hub_error(&self) -> Option<String> {
        lock(&self.hub_error).clone()
    }

    pub fn modes(&self) -> HashMap<String, FilesMode> {
        lock(&self.modes).clone()
    }

    pub fn cached_roots(&self) -> Vec<RootInfo> {
        lock(&self.roots).clone()
    }

    fn emit(&self) {
        if let Some(st) = self.weak.upgrade() {
            st.emit(ServerEvent::FilesUpdated);
        }
    }

    fn set_status(&self, key: &str, f: impl FnOnce(&mut LocalFiles)) {
        let mut map = lock(&self.status);
        let e = map.entry(key.to_string()).or_insert_with(|| LocalFiles {
            path: key.to_string(),
            origin: true,
            state: CopyState::Waiting,
            message: None,
            last_upload_at: None,
            files: 0,
            bytes: 0,
            pending: 0,
            excluded: Vec::new(),
            reincluded_secrets: Vec::new(),
            held_deletes: 0,
        });
        f(e);
    }

    /// Ask the hub for its roots and modes (they drive what uploads).
    pub async fn refresh_roots(&self) -> Result<Vec<RootInfo>, CopyError> {
        match self.env.hub.roots().await {
            Ok((roots, modes)) => {
                *lock(&self.hub_error) = None;
                *lock(&self.modes) = modes;
                *lock(&self.roots) = roots.clone();
                Ok(roots)
            }
            Err(e) => {
                let e = CopyError::from(e);
                *lock(&self.hub_error) = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// The paths the last pass over `key` held (mass-delete guard).
    pub fn held_deletes(&self, key: &str) -> Vec<String> {
        lock(&self.held).get(key).cloned().unwrap_or_default()
    }

    /// Recompute the tracked folders.
    async fn reconcile(&self) {
        let Some(st) = self.weak.upgrade() else {
            return;
        };
        // Only a hub that answered can say a root is gone.
        let roots = match self.refresh_roots().await {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(error = %e, "cannot read project file roots from the hub");
                None
            }
        };
        let modes = self.modes();
        let global = st.config().sync.project_files;
        let s = st.clone();
        let tracked = tokio::task::spawn_blocking(move || {
            tracked_folders(&s, &modes, global, roots.as_deref())
        })
        .await;
        match tracked {
            Ok(Ok(t)) => *lock(&self.tracked) = t,
            Ok(Err(e)) => tracing::warn!(error = %e, "listing folders for file sync failed"),
            Err(e) => tracing::warn!(error = %e, "listing folders for file sync failed"),
        }
    }

    /// Whether uploads may run now (grace period over, not paused).
    pub(crate) fn gate(&self) -> Result<(), CopyState> {
        let Some(st) = self.weak.upgrade() else {
            return Err(CopyState::Waiting);
        };
        let paused = st
            .store
            .get_setting(PAUSED_KEY)
            .ok()
            .flatten()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if paused {
            return Err(CopyState::Paused);
        }
        if grace_until(&st).is_some_and(|g| g > blirp_core::now_ms()) {
            return Err(CopyState::Waiting);
        }
        Ok(())
    }

    /// One pass over one folder.
    async fn sync_one(&self, t: &Tracked) {
        let key = t.copy.key.clone();
        let origin = t.copy.origin;
        self.set_status(&key, |s| s.origin = origin);
        if let Some(why) = &t.never {
            self.set_status(&key, |s| {
                s.state = CopyState::NeverSynced;
                s.message = Some(why.clone());
            });
            return;
        }
        if !t.effective {
            self.set_status(&key, |s| {
                s.state = CopyState::Off;
                s.message = None;
            });
            return;
        }
        match self.gate() {
            Err(CopyState::Waiting) => {
                // Scan only: the banner shows what will go.
                let env = self.env.clone();
                let permit = env.gate.clone().acquire_owned().await;
                if permit.is_err() {
                    return;
                }
                let cfg = super::copy::scan_config(&env).await;
                let (store, k) = (env.store.clone(), key.clone());
                let scanned = tokio::task::spawn_blocking(move || {
                    let root = PathBuf::from(&k);
                    super::local::scan_copy(&store, &k, &root, &cfg)
                })
                .await;
                drop(permit);
                if let Ok(Ok(ls)) = scanned {
                    self.set_status(&key, |s| {
                        s.state = CopyState::Waiting;
                        s.message = Some("uploads start after the first-run grace period".into());
                        s.files = i64::try_from(ls.hashed.files.len()).unwrap_or(0);
                        s.bytes =
                            i64::try_from(ls.hashed.files.iter().map(|f| f.size).sum::<u64>())
                                .unwrap_or(0);
                        s.excluded = super::local::excluded_groups(&ls);
                    });
                }
                return;
            }
            Err(state) => {
                self.set_status(&key, |s| {
                    s.state = state;
                    s.message = None;
                });
                return;
            }
            Ok(()) => {}
        }
        self.set_status(&key, |s| s.state = CopyState::Uploading);
        self.emit();
        let result = copy::upload(&self.env, &t.copy).await;
        match result {
            Ok(r) => {
                let now = blirp_core::now_ms();
                let held = r.held_deletes.len();
                if held > 0 {
                    lock(&self.held).insert(key.clone(), r.held_deletes.clone());
                } else {
                    lock(&self.held).remove(&key);
                }
                if held > 0 {
                    tracing::warn!(files = held, "upload held: many files disappeared at once");
                }
                self.set_status(&key, |s| {
                    s.held_deletes = i64::try_from(held).unwrap_or(i64::MAX);
                    s.state = match r.state {
                        _ if held > 0 => CopyState::HeldDeletes,
                        Some(ScanState::TooLarge) => CopyState::TooLarge,
                        Some(ScanState::Busy) => CopyState::Busy,
                        _ => CopyState::Idle,
                    };
                    s.message = match r.state {
                        _ if held > 0 => Some(format!(
                            "{held} files disappeared: confirm the delete or restore them from the hub"
                        )),
                        Some(ScanState::TooLarge) => Some(
                            "too large to sync: add a .blirpignore to leave big folders out".into(),
                        ),
                        Some(ScanState::Busy) => {
                            Some("waiting for a git operation to finish".into())
                        }
                        _ => None,
                    };
                    s.files = i64::try_from(r.files).unwrap_or(0);
                    s.bytes = i64::try_from(r.bytes).unwrap_or(0);
                    s.pending = i64::try_from(r.pending).unwrap_or(0);
                    s.excluded = r.excluded.clone();
                    s.reincluded_secrets = r.reincluded_secrets.clone();
                    if r.sent > 0 {
                        s.last_upload_at = Some(now);
                    }
                });
                if r.sent > 0 || r.conflicts > 0 {
                    tracing::info!(
                        files = r.sent,
                        conflicts = r.conflicts,
                        "uploaded project file changes"
                    );
                }
                if r.state == Some(ScanState::Busy) {
                    self.retry_later(&key, Duration::from_secs(5));
                }
            }
            Err(e) => self.failed(t, e).await,
        }
        self.emit();
    }

    fn retry_later(&self, key: &str, after: Duration) {
        let tx = self.kick.clone();
        let key = key.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let _ = tx.send(Kick::Copy(key));
        });
    }

    async fn failed(&self, t: &Tracked, e: CopyError) {
        let key = &t.copy.key;
        let (state, message) = match e.code() {
            "files_off" => (CopyState::Off, None),
            // The hub has not received this folder's registration yet
            // (replication runs every half second): try again shortly.
            "root_pending" => {
                self.retry_later(key, Duration::from_secs(5));
                (
                    CopyState::Waiting,
                    Some("waiting for the hub to learn this folder".into()),
                )
            }
            "unknown_root" if !t.copy.origin => {
                // The hub copy was deleted: this copy detaches (its files stay).
                let (store, k) = (self.env.store.clone(), key.clone());
                // Kept as detached: it must never turn into an origin of its own.
                match tokio::task::spawn_blocking(move || store.detach_file_copy(&k)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!(error = %e, "detaching a copy failed"),
                    Err(e) => tracing::warn!(error = %e, "detaching a copy failed"),
                }
                tracing::info!("a downloaded copy's hub copy is gone; it no longer syncs");
                (
                    CopyState::Off,
                    Some("the hub copy was deleted; this folder no longer syncs".into()),
                )
            }
            "root_replaced" => {
                // Reconcile: the copy detaches, the origin starts over.
                let _ = self.kick.send(Kick::Rescan);
                (
                    CopyState::Waiting,
                    Some("the hub copy was made again; checking this folder".into()),
                )
            }
            "hub_quota" => (
                CopyState::Error,
                Some("the hub's storage for project files is full".into()),
            ),
            "hub_outdated" => {
                *lock(&self.hub_error) = Some(e.to_string());
                (CopyState::Error, Some(e.to_string()))
            }
            _ => {
                tracing::warn!(code = e.code(), error = %e, "project file upload failed");
                (CopyState::Error, Some(e.to_string()))
            }
        };
        self.set_status(key, |s| {
            s.state = state;
            s.message = message;
        });
    }

    /// The copy a watcher event under `path` wakes, if any: only paths
    /// below the copy's root are filtered (a project that itself lives in
    /// a folder named `build` or `out` still syncs).
    fn copy_for(&self, path: &Path) -> Option<String> {
        let tracked = lock(&self.tracked);
        event_copy(tracked.iter().map(|t| t.copy.key.as_str()), path)
    }
}

/// Uploads start after this time, if still ahead.
pub fn grace_until(state: &AppState) -> Option<i64> {
    state
        .store
        .get_setting(GRACE_KEY)
        .ok()
        .flatten()
        .and_then(|v| v.as_i64())
}

/// Check this machine's copies against the hub's roots: a root gone from
/// the hub or made again under a new incarnation voids a copy's bases. An
/// origin then starts over (every file uploads again, a deleted hub copy is
/// made anew); any other copy detaches. Copies that know no incarnation yet
/// adopt the current one.
pub(crate) fn check_roots(
    store: &blirp_core::store::Store,
    roots: &[RootInfo],
) -> Result<(), blirp_core::store::StoreError> {
    for c in store.file_copies()? {
        if c.mode != CopyMode::OnDemand {
            continue;
        }
        let root = roots.iter().find(|r| r.root_id == c.root_id);
        match (c.origin, root) {
            (_, Some(r)) if c.incarnation.is_empty() => {
                store.set_file_copy_incarnation(&c.path, &r.incarnation, false)?;
            }
            (_, Some(r)) if r.incarnation == c.incarnation => {}
            (true, Some(r)) => {
                tracing::info!("the hub copy of a folder was made again; uploading it all");
                store.set_file_copy_incarnation(&c.path, &r.incarnation, true)?;
            }
            (true, None) if !c.incarnation.is_empty() => {
                tracing::info!("the hub copy of a folder was deleted; uploading it anew");
                store.set_file_copy_incarnation(&c.path, "", true)?;
            }
            (true, None) => {}
            (false, _) => {
                tracing::info!("a downloaded copy's hub copy is gone; it no longer syncs");
                store.detach_file_copy(&c.path)?;
            }
        }
    }
    Ok(())
}

/// This machine's folders the engine works on. With the hub's roots at
/// hand (`roots`), each copy is checked against them first: a root gone
/// from the hub or made again under a new incarnation means the copy's
/// bases are void. An origin then starts over (every file uploads again, a
/// deleted hub copy is made anew); any other copy detaches.
fn tracked_folders(
    st: &SharedState,
    modes: &HashMap<String, FilesMode>,
    global: bool,
    roots: Option<&[RootInfo]>,
) -> Result<Vec<Tracked>, blirp_core::store::StoreError> {
    let store = &st.store;
    if let Some(roots) = roots {
        check_roots(store, roots)?;
    }
    let rows: HashMap<String, FileCopy> = store
        .file_copies()?
        .into_iter()
        .map(|c| (c.path.clone(), c))
        .collect();
    let mut out = Vec::new();
    let mut live = HashSet::new();
    for s in store.list_project_summaries(&st.machine.id)? {
        if s.is_home || s.project.chats {
            continue;
        }
        // A project without folders works in this machine's workspace (§5):
        // it syncs like a folder once it exists.
        let workspace = (s.paths.is_empty())
            .then(|| st.paths.workspace_dir(&s.project.id).ok())
            .flatten()
            .filter(|w| w.is_dir())
            .map(|w| dunce::canonicalize(&w).unwrap_or(w).display().to_string());
        let folders: Vec<String> = s
            .paths
            .iter()
            .filter(|p| p.local)
            .map(|p| p.path.clone())
            .chain(workspace)
            .collect();
        for path in folders {
            let p = &path;
            live.insert(p.clone());
            let row = rows.get(p);
            let origin = row.is_none_or(|r| r.origin);
            let root_id = row.map_or_else(
                || blirp_core::files::root_id(&st.machine.id, p),
                |r| r.root_id.clone(),
            );
            let never = match row.map(|r| r.mode) {
                Some(CopyMode::Detached) => {
                    Some("the hub copy was deleted; this folder no longer syncs".to_string())
                }
                Some(CopyMode::Pending) => {
                    Some("the download did not finish: Update from hub completes it".to_string())
                }
                _ => super::local::never_synced(store, st.paths.home(), &s.project, Path::new(p)),
            };
            let effective = modes
                .get(&s.project.id)
                .copied()
                .unwrap_or_default()
                .effective(global);
            if origin && effective && never.is_none() && row.is_none() {
                store.put_file_copy(&FileCopy {
                    path: p.clone(),
                    root_id: root_id.clone(),
                    origin: true,
                    seen: 0,
                    created_at: blirp_core::now_ms(),
                    mode: CopyMode::OnDemand,
                    incarnation: String::new(),
                })?;
            }
            out.push(Tracked {
                copy: Copy {
                    key: p.clone(),
                    root_id,
                    origin,
                    incarnation: row.map(|r| r.incarnation.clone()).unwrap_or_default(),
                },
                project_id: s.project.id.clone(),
                never,
                effective,
            });
        }
    }
    // Folders no longer registered (project deleted): an origin's state
    // goes; a downloaded copy stays detached, so it never turns into an
    // origin if its folder is registered again.
    for (path, row) in rows.iter().filter(|(p, _)| !live.contains(*p)) {
        if row.origin {
            store.remove_file_copy(path)?;
        } else if row.mode != CopyMode::Detached && row.mode != CopyMode::Pending {
            store.detach_file_copy(path)?;
        }
    }
    Ok(out)
}

type Watcher = Debouncer<RecommendedWatcher, RecommendedCache>;

fn start_watcher(tx: mpsc::UnboundedSender<Kick>) -> Option<Watcher> {
    let res = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| match res {
        Ok(events) => {
            let paths: Vec<PathBuf> = events.into_iter().flat_map(|e| e.event.paths).collect();
            if !paths.is_empty() {
                let _ = tx.send(Kick::Paths(paths));
            }
        }
        Err(errors) => {
            for e in &errors {
                tracing::warn!(error = %e, "project folder watcher error; rescanning");
            }
            // Missed events (inotify limits, FSEvents drops): rescan.
            let _ = tx.send(Kick::Rescan);
        }
    });
    match res {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!(error = %e, "cannot watch project folders; relying on rescans");
            None
        }
    }
}

/// Of the copies at `roots`, the innermost holding `path`, unless the
/// event is one that never syncs (judged below that copy's root only).
fn event_copy<'a>(roots: impl Iterator<Item = &'a str>, path: &Path) -> Option<String> {
    let key = blirp_core::paths::path_key(path);
    let (root, rel) = roots
        .filter_map(|r| {
            let rel = key
                .strip_prefix(blirp_core::paths::path_key(Path::new(r)))
                .ok()?
                .to_path_buf();
            Some((r, rel))
        })
        .max_by_key(|(r, _)| r.len())?;
    relevant(&rel).then(|| root.to_string())
}

/// Paths blirp writes itself or that never sync do not wake the engine.
/// `p` is relative to the copy's root.
fn relevant(p: &Path) -> bool {
    // Build output and caches (the denylist's folders) change all the time
    // during builds and never sync: their events wake nothing.
    let build_dirs = blirp_core::files::rules::BUILD
        .iter()
        .filter_map(|b| b.strip_suffix('/'));
    let build: Vec<&str> = build_dirs.collect();
    !p.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        blirp_core::files::path::is_vcs_component(&s)
            || s.starts_with(TMP_PREFIX)
            || build.iter().any(|b| s.eq_ignore_ascii_case(b))
    })
}

async fn run(
    engine: Arc<Engine>,
    mut rx: mpsc::UnboundedReceiver<Kick>,
    mut stop: watch::Receiver<bool>,
) {
    let mut watcher = start_watcher(engine.kick.clone());
    let mut watched: HashSet<PathBuf> = HashSet::new();
    let mut rescan = tokio::time::interval(RESCAN);
    rescan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let hub_gc = matches!(engine.env.hub, blirp_sync::files::FileHub::Local(_));
    let mut gc = tokio::time::interval_at(tokio::time::Instant::now() + GC_FIRST, GC_EVERY);
    let mut notes = match &engine.env.hub {
        blirp_sync::files::FileHub::Local(l) => Some(l.hub.subscribe()),
        blirp_sync::files::FileHub::Remote(_) => None,
    };
    // Grace ends: scan (and now upload) everything.
    let grace_tick = {
        let e = engine.clone();
        tokio::spawn(async move {
            loop {
                let wait = e
                    .weak
                    .upgrade()
                    .and_then(|st| grace_until(&st))
                    .map(|g| g - blirp_core::now_ms())
                    .filter(|w| *w > 0);
                let Some(w) = wait else {
                    e.rescan();
                    return;
                };
                tokio::time::sleep(Duration::from_millis(u64::try_from(w).unwrap_or(0) + 50)).await;
            }
        })
    };
    let mut pending: BTreeSet<String> = BTreeSet::new();
    let mut full = false;
    // Reconcile only: new folders are scanned, known ones are not.
    let mut refresh = false;
    // New or changed projects (a folder registered, a workspace in use):
    // reconcile soon rather than at the next rescan.
    let mut project_events = engine.weak.upgrade().map(|st| st.events.subscribe());
    loop {
        if !full && !refresh && pending.is_empty() {
            tokio::select! {
                _ = stop.changed() => break,
                _ = rescan.tick() => full = true,
                _ = gc.tick(), if hub_gc => {
                    if let blirp_sync::files::FileHub::Local(l) = &engine.env.hub {
                        let h = l.hub.clone();
                        match tokio::task::spawn_blocking(move || h.gc(blirp_core::now_ms())).await {
                            Ok(Ok(s)) if s.blobs + s.history + s.tombstones > 0 => tracing::info!(
                                blobs = s.blobs, history = s.history, tombstones = s.tombstones,
                                "collected old project file versions"
                            ),
                            Ok(Ok(_)) => {}
                            Ok(Err(e)) => tracing::warn!(error = %e, "collecting project files failed"),
                            Err(e) => tracing::warn!(error = %e, "collecting project files failed"),
                        }
                    }
                }
                ev = async { match project_events.as_mut() { Some(r) => r.recv().await, None => std::future::pending().await } } => {
                match ev {
                    Ok(ServerEvent::ProjectUpdated { .. } | ServerEvent::SessionCreated { .. })
                    | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => refresh = true,
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => project_events = None,
                }
            }
            n = async { match notes.as_mut() { Some(r) => r.recv().await.ok(), None => std::future::pending().await } } => {
                    if n.is_some() {
                        engine.emit();
                    }
                }
                k = rx.recv() => match k {
                    None => break,
                    Some(Kick::Rescan) => full = true,
                    Some(Kick::Copy(key)) => { pending.insert(key); }
                    Some(Kick::Paths(paths)) => {
                        for p in &paths {
                            if let Some(k) = engine.copy_for(p) {
                                pending.insert(k);
                            }
                        }
                    }
                },
            }
        }
        if full || refresh {
            let before: HashSet<String> =
                engine.tracked().into_iter().map(|t| t.copy.key).collect();
            let scan_all = full;
            full = false;
            refresh = false;
            engine.reconcile().await;
            let tracked = engine.tracked();
            // Watch what may upload; drop the rest.
            if let Some(w) = watcher.as_mut() {
                let want: HashSet<PathBuf> = tracked
                    .iter()
                    .filter(|t| t.effective && t.never.is_none())
                    .map(|t| t.copy.root())
                    .collect();
                for p in watched.difference(&want) {
                    let _ = w.unwatch(p);
                }
                watched.retain(|p| want.contains(p));
                for p in want {
                    if watched.contains(&p) {
                        continue;
                    }
                    match w.watch(&p, RecursiveMode::Recursive) {
                        Ok(()) => {
                            watched.insert(p);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "cannot watch a project folder; relying on rescans")
                        }
                    }
                }
            }
            pending.extend(
                tracked
                    .into_iter()
                    .map(|t| t.copy.key)
                    .filter(|k| scan_all || !before.contains(k)),
            );
            engine.emit();
        }
        while let Some(key) = pending.pop_first() {
            let Some(t) = engine.tracked().into_iter().find(|t| t.copy.key == key) else {
                continue;
            };
            tokio::select! {
                _ = engine.sync_one(&t) => {}
                _ = stop.changed() => {
                    grace_tick.abort();
                    return;
                }
            }
            // New events while working join the queue.
            while let Ok(k) = rx.try_recv() {
                match k {
                    Kick::Rescan => full = true,
                    Kick::Copy(key) => {
                        pending.insert(key);
                    }
                    Kick::Paths(paths) => {
                        for p in &paths {
                            if let Some(k) = engine.copy_for(p) {
                                pending.insert(k);
                            }
                        }
                    }
                }
            }
            if full {
                break;
            }
        }
    }
    grace_tick.abort();
    drop(watcher);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_filtered_below_the_copy_root_only() {
        let base = std::env::temp_dir();
        let root = base.join("out").join("proj");
        let key = root.display().to_string();
        let roots = || std::iter::once(key.as_str());
        // A project inside a folder named like build output still syncs.
        assert_eq!(
            event_copy(roots(), &root.join("src/a.rs")),
            Some(key.clone())
        );
        assert_eq!(event_copy(roots(), &root.join("out/x.o")), None);
        assert_eq!(event_copy(roots(), &root.join(".git/index")), None);
        assert_eq!(event_copy(roots(), &base.join("elsewhere/a.rs")), None);
    }
}
