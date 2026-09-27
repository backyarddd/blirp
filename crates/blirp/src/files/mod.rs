//! Project file sync through the hub (docs/project-files.md): the daemon's
//! file engine, working copies and their API.

pub mod api;
pub mod copy;
pub mod download;
pub mod engine;
pub mod local;

#[cfg(test)]
mod tests;

use crate::state::SharedState;
use blirp_core::model::ServerEvent;
use blirp_sync::SyncService;
use blirp_sync::files::blobs::BlobStore;
use blirp_sync::files::{FileHub, HubFiles, LocalHub, RemoteHub};
use engine::Engine;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// `settings` key: the hub's default quota in bytes (half the free disk
/// space when file sync first ran there).
pub const QUOTA_KEY: &str = "files.hub_quota";
/// Quota when the free space cannot be read.
const FALLBACK_QUOTA: u64 = 10 << 30;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Options replaced whole; a poisoned one is still consistent.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// File sync state of the daemon.
pub struct FilesState {
    /// Hashing runs one folder at a time (design §3), so a large first scan
    /// never competes with itself for the disk.
    pub hash_gate: Arc<tokio::sync::Semaphore>,
    engine: Mutex<Option<Arc<Engine>>>,
    hub: Mutex<Option<Arc<HubFiles>>>,
    /// Copies being made on this machine.
    pub downloads: download::Downloads,
}

impl Default for FilesState {
    fn default() -> Self {
        Self {
            hash_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            engine: Mutex::default(),
            hub: Mutex::default(),
            downloads: download::Downloads::default(),
        }
    }
}

/// The running engine (hub and node roles).
pub fn engine(state: &SharedState) -> Option<Arc<Engine>> {
    lock(&state.files.engine).clone()
}

fn limits(state: &SharedState) -> (u64, i64) {
    let cfg = state.config().files;
    let keep = i64::from(cfg.keep_versions_days) * 24 * 3600 * 1000;
    if cfg.hub_quota_gb > 0 {
        return (u64::from(cfg.hub_quota_gb) << 30, keep);
    }
    let saved = state
        .store
        .get_setting(QUOTA_KEY)
        .ok()
        .flatten()
        .and_then(|v| v.as_u64());
    let quota = saved.unwrap_or_else(|| {
        let q = blirp_core::files::disk::free_bytes(state.paths.home())
            .map_or(FALLBACK_QUOTA, |free| free / 2);
        if let Err(e) = state.store.set_setting(QUOTA_KEY, &serde_json::json!(q)) {
            tracing::warn!(error = %e, "saving the project file quota failed");
        }
        q
    });
    (quota, keep)
}

/// The hub's file service (created once, shared by the protocol server and
/// the hub's own engine).
pub fn hub_files(state: &SharedState) -> Arc<HubFiles> {
    let (quota, keep) = limits(state);
    let mut h = lock(&state.files.hub);
    if let Some(hub) = h.as_ref() {
        hub.set_limits(quota, keep);
        return hub.clone();
    }
    let hub = Arc::new(HubFiles::new(
        state.store.clone(),
        &state.paths.home().join("files"),
        quota,
        keep,
    ));
    *h = Some(hub.clone());
    hub
}

/// Start the engine for a freshly started sync service.
pub async fn start(state: &SharedState, svc: &Arc<SyncService>) {
    stop(state).await;
    let cfg = state.config();
    let parts = BlobStore::new(state.paths.home().join("files").join("dl"));
    let hub = if svc.is_hub() {
        FileHub::Local(Arc::new(LocalHub {
            hub: hub_files(state),
            machine_id: state.machine.id.clone(),
            machine_name: cfg.machine.name.clone(),
            parts,
        }))
    } else {
        let s = svc.clone();
        let weak = Arc::downgrade(state);
        FileHub::Remote(Arc::new(RemoteHub::new(
            Arc::new(move || {
                let s = s.clone();
                Box::pin(async move { s.connect_files().await })
            }),
            parts,
            cfg.files.upload_kbps,
            Arc::new(move |_| {
                if let Some(st) = weak.upgrade() {
                    st.emit(ServerEvent::FilesUpdated);
                }
            }),
        )))
    };
    // First run after upgrade or pairing: uploads wait a grace period.
    let st = state.clone();
    let saved = tokio::task::spawn_blocking(move || {
        if engine::grace_until(&st).is_none() {
            let until = blirp_core::now_ms() + engine::GRACE_MS;
            if let Err(e) = st
                .store
                .set_setting(engine::GRACE_KEY, &serde_json::json!(until))
            {
                tracing::warn!(error = %e, "saving the file sync grace period failed");
            }
        }
    })
    .await;
    if let Err(e) = saved {
        tracing::warn!(error = %e, "saving the file sync grace period failed");
    }
    let env = copy::Env {
        store: state.store.clone(),
        hub,
        data_dir: state.paths.home().to_path_buf(),
        scan: local::scan_config(&cfg.files, state.paths.home()),
        gate: state.files.hash_gate.clone(),
    };
    let e = Engine::start(state, env);
    *lock(&state.files.engine) = Some(e);
}

/// Stop the engine (sync stopping, role change, daemon shutdown).
pub async fn stop(state: &SharedState) {
    let e = lock(&state.files.engine).take();
    if let Some(e) = e {
        e.shutdown().await;
    }
}

/// Settings changed: new limits and toggles apply now.
pub fn config_changed(state: &SharedState) {
    let cfg = state.config();
    if lock(&state.files.hub).is_some() {
        hub_files(state);
    }
    if let Some(e) = engine(state) {
        if let FileHub::Remote(r) = &e.env.hub {
            r.rate().set(cfg.files.upload_kbps);
        }
        e.rescan();
    }
}

/// How long a session start waits for its copy to take the hub's changes.
const FAST_FORWARD: std::time::Duration = std::time::Duration::from_secs(30);

/// A session starts in `cwd`: a downloaded copy containing it takes the
/// hub's changes first (design §5, `on_demand`). Failures are logged; the
/// session starts either way.
pub async fn fast_forward(state: &SharedState, cwd: &std::path::Path) {
    let Some(e) = engine(state) else { return };
    let key = blirp_core::paths::path_key(cwd);
    let Some(t) = e.tracked().into_iter().find(|t| {
        !t.copy.origin
            && key.starts_with(blirp_core::paths::path_key(std::path::Path::new(
                &t.copy.key,
            )))
    }) else {
        return;
    };
    let run = async {
        let _work = e.work.lock().await;
        copy::apply(&e.env, &t.copy, false).await
    };
    match tokio::time::timeout(FAST_FORWARD, run).await {
        Ok(Ok(r)) if r.written + r.deleted + r.conflicts.len() > 0 => tracing::info!(
            written = r.written,
            deleted = r.deleted,
            conflicts = r.conflicts.len(),
            "session folder took the hub's changes"
        ),
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "session folder could not take the hub's changes")
        }
        Err(_) => tracing::warn!("taking the hub's changes before a session start timed out"),
    }
}
