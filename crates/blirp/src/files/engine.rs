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
use blirp_core::store::FileCopy;
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
    /// Serializes work on copies (uploads, updates, downloads' overlays):
    /// bases must not be written by two passes at once.
    // ponytail: one lock for all copies; per-copy locks if folders queue.
    pub work: tokio::sync::Mutex<()>,
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
            work: tokio::sync::Mutex::new(()),
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

    /// Recompute the tracked folders.
    async fn reconcile(&self) {
        let Some(st) = self.weak.upgrade() else {
            return;
        };
        if let Err(e) = self.refresh_roots().await {
            tracing::warn!(error = %e, "cannot read project file roots from the hub");
        }
        let modes = self.modes();
        let global = st.config().sync.project_files;
        let s = st.clone();
        let tracked =
            tokio::task::spawn_blocking(move || tracked_folders(&s, &modes, global)).await;
        match tracked {
            Ok(Ok(t)) => *lock(&self.tracked) = t,
            Ok(Err(e)) => tracing::warn!(error = %e, "listing folders for file sync failed"),
            Err(e) => tracing::warn!(error = %e, "listing folders for file sync failed"),
        }
    }

    /// Whether uploads may run now (grace period over, not paused).
    fn gate(&self) -> Result<(), CopyState> {
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
                let (store, k, cfg) = (env.store.clone(), key.clone(), env.scan.clone());
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
        let result = {
            let _work = self.work.lock().await;
            copy::upload(&self.env, &t.copy).await
        };
        match result {
            Ok(r) => {
                let now = blirp_core::now_ms();
                self.set_status(&key, |s| {
                    s.state = match r.state {
                        Some(ScanState::TooLarge) => CopyState::TooLarge,
                        Some(ScanState::Busy) => CopyState::Busy,
                        _ => CopyState::Idle,
                    };
                    s.message = match r.state {
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

    fn copy_for(&self, path: &Path) -> Option<String> {
        let key = blirp_core::paths::path_key(path);
        lock(&self.tracked)
            .iter()
            .filter(|t| key.starts_with(blirp_core::paths::path_key(Path::new(&t.copy.key))))
            .max_by_key(|t| t.copy.key.len())
            .map(|t| t.copy.key.clone())
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

/// This machine's folders the engine works on.
fn tracked_folders(
    st: &SharedState,
    modes: &HashMap<String, FilesMode>,
    global: bool,
) -> Result<Vec<Tracked>, blirp_core::store::StoreError> {
    let store = &st.store;
    let rows: HashMap<String, FileCopy> = store
        .file_copies()?
        .into_iter()
        .map(|c| (c.path.clone(), c))
        .collect();
    let mut out = Vec::new();
    let mut live = HashSet::new();
    for s in store.list_project_summaries(&st.machine.id)? {
        if s.is_home {
            continue;
        }
        for p in s.paths.iter().filter(|p| p.local) {
            live.insert(p.path.clone());
            let row = rows.get(&p.path);
            let origin = row.is_none_or(|r| r.origin);
            let root_id = row.map_or_else(
                || blirp_core::files::root_id(&st.machine.id, &p.path),
                |r| r.root_id.clone(),
            );
            let never = if row.is_some_and(|r| r.detached) {
                Some("the hub copy was deleted; this folder no longer syncs".to_string())
            } else {
                super::local::never_synced(store, st.paths.home(), &s.project, Path::new(&p.path))
            };
            let effective = modes
                .get(&s.project.id)
                .copied()
                .unwrap_or_default()
                .effective(global);
            if origin && effective && never.is_none() && row.is_none() {
                store.put_file_copy(&FileCopy {
                    path: p.path.clone(),
                    root_id: root_id.clone(),
                    origin: true,
                    seen: 0,
                    created_at: blirp_core::now_ms(),
                    detached: false,
                })?;
            }
            out.push(Tracked {
                copy: Copy {
                    key: p.path.clone(),
                    root_id,
                    origin,
                },
                project_id: s.project.id.clone(),
                never,
                effective,
            });
        }
    }
    // Copies whose folder was unregistered (project deleted) detach.
    for path in rows.keys().filter(|p| !live.contains(*p)) {
        store.remove_file_copy(path)?;
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

/// Paths blirp writes itself or that never sync do not wake the engine.
fn relevant(p: &Path) -> bool {
    !p.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        blirp_core::files::path::is_vcs_component(&s) || s.starts_with(TMP_PREFIX)
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
    loop {
        if !full && pending.is_empty() {
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
                        for p in paths.iter().filter(|p| relevant(p)) {
                            if let Some(k) = engine.copy_for(p) {
                                pending.insert(k);
                            }
                        }
                    }
                },
            }
        }
        if full {
            full = false;
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
            pending.extend(tracked.into_iter().map(|t| t.copy.key));
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
                        for p in paths.iter().filter(|p| relevant(p)) {
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
