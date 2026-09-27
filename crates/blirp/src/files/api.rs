//! File sync API (docs/project-files.md): per-project settings, status,
//! preview and actions under `/api/projects/:id/files-sync`, the machine
//! wide switches under `/api/files`, and copies under
//! `/api/machines/:id/files/download` (forwarded to another machine like
//! clone).

use super::copy::{self, Act, Copy, CopyError};
use super::engine::{self, Engine};
use crate::api::{
    ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, Control, FilesAccess, Principal, blocking,
};
use crate::state::SharedState;
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use blirp_core::files::{FilesMode, RootInfo};
use blirp_core::model::{
    AppliedFiles, ApplyFiles, CopyState, DownloadFiles, FilesIncoming, FilesOverview, FilesPreview,
    FilesRoot, HeldAction, IncomingAction, IncomingFile, LocalFiles, MachineRole, PauseFiles,
    ProjectFiles, ResolveHeld, SetFilesMode,
};
use blirp_core::store::CopyMode;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/files/status", get(overview))
        .route("/api/files/pause", post(pause))
        .route("/api/files/start-now", post(start_now))
        .route("/api/projects/{id}/files-sync", get(project).put(set_mode))
        .route("/api/projects/{id}/files-sync/preview", get(preview))
        .route("/api/projects/{id}/files-sync/incoming", get(incoming))
        .route("/api/projects/{id}/files-sync/apply", post(apply))
        .route("/api/projects/{id}/files-sync/held", post(resolve_held))
        .route(
            "/api/projects/{id}/files-sync/roots/{root}",
            delete(delete_root),
        )
        .route("/api/machines/{id}/files/download", post(download))
        .route("/api/machines/{id}/files/download/{job}", get(download_job))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RootQuery {
    /// One of the project's folders on this machine (absolute); default the first.
    #[serde(default)]
    root: Option<String>,
}

fn files_error(e: CopyError) -> ApiError {
    match e.code() {
        "hub_unreachable" => {
            ApiError::new(StatusCode::BAD_GATEWAY, "hub_unreachable", e.to_string())
        }
        "hub_outdated" => ApiError::conflict("hub_outdated", e.to_string()),
        "files_on" => ApiError::conflict("files_on", e.to_string()),
        "unknown_root" => ApiError::not_found("root"),
        "local_error" => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "files_failed",
            e.to_string(),
        ),
        _ => ApiError::new(StatusCode::BAD_GATEWAY, "hub_error", e.to_string()),
    }
}

fn need_engine(s: &SharedState) -> ApiResult<Arc<Engine>> {
    super::engine(s).ok_or_else(|| {
        ApiError::conflict(
            "files_unavailable",
            "project file sync needs a hub: make this machine the hub or pair it with one",
        )
    })
}

/// The project's folder on this machine: `root` when given (it must be
/// one), else the first.
pub(crate) fn local_root(s: &SharedState, id: &str, root: Option<&str>) -> ApiResult<PathBuf> {
    // A project without folders has its workspace here (created on demand);
    // the engine keys it by its canonical path. A picked `root` may name it
    // either way.
    let canon = |p: PathBuf| dunce::canonicalize(&p).unwrap_or(p);
    match root {
        Some(r) => crate::api::files::project_root(s, id, Some(r))
            .or_else(|e| {
                let ws = canon(crate::api::files::project_root(s, id, None)?);
                if ws.as_os_str() == r { Ok(ws) } else { Err(e) }
            })
            .map(canon),
        None => crate::api::files::project_root(s, id, None).map(canon),
    }
}

fn paused(s: &SharedState) -> bool {
    s.store
        .get_setting(engine::PAUSED_KEY)
        .ok()
        .flatten()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// `GET /api/files/status`.
async fn overview(
    State(s): State<SharedState>,
    _files: FilesAccess,
) -> ApiResult<Json<FilesOverview>> {
    let cfg = s.config();
    let engine = super::engine(&s);
    let st = s.clone();
    let (paused, grace) = blocking(move || Ok((paused(&st), engine::grace_until(&st)))).await?;
    let hub_name = match cfg.sync.role {
        MachineRole::Hub => Some(cfg.machine.name.clone()),
        MachineRole::Node => {
            let (store, hub) = (s.store.clone(), cfg.sync.hub.clone());
            blocking(move || {
                Ok(hub
                    .and_then(|h| store.get_machine(&h).ok().flatten())
                    .map(|m| m.name))
            })
            .await?
        }
        MachineRole::Standalone => None,
    };
    let mut folders = 0;
    let mut bytes = 0;
    if let Some(e) = &engine {
        for t in e
            .tracked()
            .iter()
            .filter(|t| t.effective && t.never.is_none())
        {
            folders += 1;
            bytes += e.status_of(&t.copy.key).map_or(0, |l| l.bytes);
        }
    }
    Ok(Json(FilesOverview {
        available: engine.is_some(),
        enabled: cfg.sync.project_files,
        paused,
        grace_until: grace.filter(|g| *g > blirp_core::now_ms()),
        hub_name,
        folders,
        bytes,
        hub_error: engine.and_then(|e| e.hub_error()),
    }))
}

/// `POST /api/files/pause`: "Pause file sync" on this machine.
async fn pause(
    State(s): State<SharedState>,
    Control(_): Control,
    _files: FilesAccess,
    ApiJson(body): ApiJson<PauseFiles>,
) -> ApiResult<Json<FilesOverview>> {
    let st = s.clone();
    blocking(move || {
        Ok(st
            .store
            .set_setting(engine::PAUSED_KEY, &serde_json::json!(body.paused))?)
    })
    .await?;
    tracing::info!(paused = body.paused, "file sync pause switched");
    if let Some(e) = super::engine(&s) {
        e.rescan();
    }
    overview(State(s), FilesAccess(crate::api::Principal::local())).await
}

/// `POST /api/files/start-now`: end the first-run grace period.
async fn start_now(
    State(s): State<SharedState>,
    Control(_): Control,
    _files: FilesAccess,
) -> ApiResult<Json<FilesOverview>> {
    let st = s.clone();
    blocking(move || {
        Ok(st
            .store
            .set_setting(engine::GRACE_KEY, &serde_json::json!(blirp_core::now_ms()))?)
    })
    .await?;
    if let Some(e) = super::engine(&s) {
        e.rescan();
    }
    overview(State(s), FilesAccess(crate::api::Principal::local())).await
}

fn default_local(key: &str, origin: bool) -> LocalFiles {
    LocalFiles {
        path: key.to_string(),
        origin,
        state: CopyState::Waiting,
        message: None,
        last_upload_at: None,
        files: 0,
        bytes: 0,
        pending: 0,
        excluded: Vec::new(),
        reincluded_secrets: Vec::new(),
        held_deletes: 0,
    }
}

async fn project_files(s: &SharedState, id: &str) -> ApiResult<ProjectFiles> {
    let st = s.clone();
    let pid = id.to_string();
    let (project, is_paused) =
        blocking(move || Ok((st.store.live_project(&pid)?, paused(&st)))).await?;
    let cfg = s.config();
    let global = cfg.sync.project_files;
    let Some(e) = super::engine(s) else {
        return Ok(ProjectFiles {
            available: false,
            mode: FilesMode::Default,
            global,
            effective: false,
            paused: is_paused,
            hub_error: None,
            roots: Vec::new(),
        });
    };
    let hub_roots = match e.refresh_roots().await {
        Ok(r) => r,
        Err(_) => e.cached_roots(),
    };
    let mode = e.modes().get(&project.id).copied().unwrap_or_default();
    let tracked: Vec<_> = e
        .tracked()
        .into_iter()
        .filter(|t| t.project_id == project.id)
        .collect();
    let mut roots: Vec<FilesRoot> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for r in hub_roots.into_iter().filter(|r| r.project_id == project.id) {
        by_id.insert(r.root_id.clone(), roots.len());
        roots.push(FilesRoot {
            root_id: r.root_id.clone(),
            machine_id: r.machine_id.clone(),
            machine_name: r.machine_name.clone(),
            path: r.path.clone(),
            origin_revoked: r.origin_revoked,
            hub: Some(r),
            local: None,
        });
    }
    for t in tracked {
        let local = e
            .status_of(&t.copy.key)
            .unwrap_or_else(|| default_local(&t.copy.key, t.copy.origin));
        match by_id.get(&t.copy.root_id) {
            Some(&i) => roots[i].local = Some(local),
            None => roots.push(FilesRoot {
                root_id: t.copy.root_id.clone(),
                machine_id: s.machine.id.clone(),
                machine_name: cfg.machine.name.clone(),
                path: t.copy.key.clone(),
                origin_revoked: false,
                hub: None,
                local: Some(local),
            }),
        }
    }
    Ok(ProjectFiles {
        available: true,
        mode,
        global,
        effective: mode.effective(global),
        paused: is_paused,
        hub_error: e.hub_error(),
        roots,
    })
}

/// `GET /api/projects/:id/files-sync`.
async fn project(
    State(s): State<SharedState>,
    _files: FilesAccess,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<ProjectFiles>> {
    project_files(&s, &id).await.map(Json)
}

/// `PUT /api/projects/:id/files-sync {mode}`: Default / On / Off, stored on
/// the hub for every machine.
async fn set_mode(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    Control(_): Control,
    _files: FilesAccess,
    ApiJson(body): ApiJson<SetFilesMode>,
) -> ApiResult<Json<ProjectFiles>> {
    let e = need_engine(&s)?;
    let (store, pid) = (s.store.clone(), id.clone());
    blocking(move || Ok(store.live_project(&pid).map(|_| ())?)).await?;
    e.env
        .hub
        .set_mode(&id, body.mode)
        .await
        .map_err(|err| files_error(err.into()))?;
    tracing::info!(mode = body.mode.as_str(), "project file sync mode changed");
    e.rescan();
    project_files(&s, &id).await.map(Json)
}

/// `GET /api/projects/:id/files-sync/preview?root=`: a dry run of what
/// uploading this folder would send. Nothing leaves the machine; the hashes
/// it computes stay in the local cache for the real upload.
async fn preview(
    State(s): State<SharedState>,
    _files: FilesAccess,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<RootQuery>,
) -> ApiResult<Json<FilesPreview>> {
    let _gate = s
        .files
        .hash_gate
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| ApiError::internal("waiting for the file scanner", e))?;
    let st = s.clone();
    blocking(move || {
        let root = local_root(&st, &id, q.root.as_deref())?;
        let project = st.store.live_project(&id)?;
        let never = super::local::never_synced(&st.store, st.paths.home(), &project, &root);
        let cfg = super::local::scan_config(&st.config().files, st.paths.home());
        let key = root.display().to_string();
        let ls = super::local::scan_copy(&st.store, &key, &root, &cfg).map_err(|e| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "root_missing",
                format!("project folder is not accessible: {e}"),
            )
        })?;
        Ok(Json(super::local::preview(&ls, &root, never)))
    })
    .await
}

/// This machine's working copy for `root` (a folder of the project here).
fn local_copy(e: &Engine, s: &SharedState, id: &str, root: Option<&str>) -> ApiResult<Copy> {
    tracked_copy(e, s, id, root).map(|t| t.copy)
}

fn tracked_copy(
    e: &Engine,
    s: &SharedState,
    id: &str,
    root: Option<&str>,
) -> ApiResult<super::engine::Tracked> {
    let folder = local_root(s, id, root)?;
    let key = folder.display().to_string();
    e.tracked()
        .into_iter()
        .find(|t| t.copy.key == key)
        .ok_or_else(|| ApiError::conflict("not_synced", "this folder does not sync with the hub"))
}

fn names(s: &SharedState) -> HashMap<String, String> {
    s.store
        .list_machines()
        .map(|ms| ms.into_iter().map(|m| (m.id, m.name)).collect())
        .unwrap_or_default()
}

/// `GET /api/projects/:id/files-sync/incoming?root=`: hub versions this
/// folder has not taken ("Hub has N newer files").
async fn incoming(
    State(s): State<SharedState>,
    _files: FilesAccess,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<RootQuery>,
) -> ApiResult<Json<FilesIncoming>> {
    let e = need_engine(&s)?;
    let st = s.clone();
    let (e2, id2, r2) = (e.clone(), id.clone(), q.root.clone());
    let (copy, names) =
        blocking(move || Ok((local_copy(&e2, &st, &id2, r2.as_deref())?, names(&st)))).await?;
    let (list, head) = copy::incoming(&e.env, &copy, false)
        .await
        .map_err(files_error)?;
    let mut files = Vec::new();
    for i in &list {
        let action = match &i.act {
            Act::Ack | Act::DropBase | Act::Rebase => continue,
            Act::Write(blirp_core::files::write::Expect::Absent) => IncomingAction::New,
            Act::Write(_) => IncomingAction::Update,
            Act::Delete(_) => IncomingAction::Delete,
            Act::ConflictCopy => IncomingAction::Conflict,
            Act::Skip(_) => IncomingAction::Skip,
        };
        files.push(IncomingFile {
            path: i.entry.path.clone(),
            action,
            by_machine_name: names.get(&i.entry.by_machine).cloned().unwrap_or_default(),
            at: i.entry.at,
        });
    }
    if files.is_empty() {
        // Everything newer is this copy's own: nothing to show until the head moves.
        let (store, key) = (s.store.clone(), copy.key.clone());
        blocking(move || Ok(store.set_file_copy_seen(&key, head)?)).await?;
    }
    Ok(Json(FilesIncoming {
        root: copy.key,
        files,
    }))
}

/// `POST /api/projects/:id/files-sync/apply {root}`: "Update from hub" on
/// a copy, "Bring changes here" on the origin folder. Local changes are
/// uploaded first; files unchanged since they last synced are replaced,
/// the hub's version of anything changed here is written next to it as a
/// conflict copy.
async fn apply(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    Control(_): Control,
    _files: FilesAccess,
    ApiJson(body): ApiJson<ApplyFiles>,
) -> ApiResult<Json<AppliedFiles>> {
    let e = need_engine(&s)?;
    let st = s.clone();
    let (e2, id2, root) = (e.clone(), id.clone(), body.root.clone());
    let (tracked, pending) = blocking(move || {
        let t = tracked_copy(&e2, &st, &id2, Some(&root))?;
        let pending = st
            .store
            .file_copy(&t.copy.key)?
            .is_some_and(|c| c.mode == CopyMode::Pending);
        Ok((t, pending))
    })
    .await?;
    let copy = tracked.copy.clone();
    let report = if pending {
        // An unfinished download: write the hub's files, then it syncs.
        let r = copy::apply(&e.env, &copy, true)
            .await
            .map_err(files_error)?;
        if r.failed.is_empty() {
            let (store, key) = (s.store.clone(), copy.key.clone());
            blocking(move || Ok(store.finish_file_copy(&key)?)).await?;
            e.rescan();
        }
        r
    } else {
        // Local changes go up first, but only where uploads may run now
        // (the machine's switch, the project's mode, Pause, the grace
        // period): taking the hub's changes never uploads behind them.
        if tracked.effective
            && tracked.never.is_none()
            && e.gate().is_ok()
            && let Err(err) = copy::upload(&e.env, &copy).await
        {
            // Still take the hub's changes; the upload retries on its own.
            tracing::warn!(error = %err, "uploading before taking the hub's changes failed");
        }
        copy::apply(&e.env, &copy, false)
            .await
            .map_err(files_error)?
    };
    tracing::info!(
        written = report.written,
        deleted = report.deleted,
        conflicts = report.conflicts.len(),
        "took the hub's project file changes"
    );
    // Local versions kept next to conflict copies upload now.
    e.touch(&copy.key);
    Ok(Json(AppliedFiles {
        written: i64::try_from(report.written).unwrap_or(0),
        deleted: i64::try_from(report.deleted).unwrap_or(0),
        conflicts: report.conflicts,
        skipped: report
            .skipped
            .into_iter()
            .map(|(p, why)| format!("{p}: {why}"))
            .collect(),
        failed: report
            .failed
            .into_iter()
            .map(|(p, why)| format!("{p}: {why}"))
            .collect(),
    }))
}

/// `POST /api/projects/:id/files-sync/held {root, action}`: after the
/// mass-delete guard held a folder's upload, "Delete on hub too" (upload
/// with the deletes) or "Restore from hub" (write the missing files back,
/// only while the folder exists). Leaving it alone keeps it paused.
async fn resolve_held(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    Control(_): Control,
    _files: FilesAccess,
    ApiJson(body): ApiJson<ResolveHeld>,
) -> ApiResult<Json<AppliedFiles>> {
    let e = need_engine(&s)?;
    let st = s.clone();
    let (e2, id2, root) = (e.clone(), id.clone(), body.root.clone());
    let copy = blocking(move || local_copy(&e2, &st, &id2, Some(&root))).await?;
    let out = {
        match body.action {
            HeldAction::Delete => {
                // Only what the folder showed as held; anything that
                // disappeared since waits for its own confirmation.
                let shown: std::collections::HashSet<String> =
                    e.held_deletes(&copy.key).into_iter().collect();
                if shown.is_empty() {
                    return Err(ApiError::conflict(
                        "nothing_held",
                        "no deletes are waiting for a confirmation",
                    ));
                }
                let r = copy::upload_with(&e.env, &copy, copy::Deletes::Confirm(shown))
                    .await
                    .map_err(files_error)?;
                tracing::info!(files = r.sent, "confirmed deleting files on the hub");
                AppliedFiles {
                    written: 0,
                    deleted: i64::try_from(r.sent).unwrap_or(0),
                    conflicts: Vec::new(),
                    skipped: Vec::new(),
                    failed: Vec::new(),
                }
            }
            HeldAction::Restore => {
                let r = copy::restore_missing(&e.env, &copy)
                    .await
                    .map_err(files_error)?;
                tracing::info!(files = r.written, "restored files from the hub");
                AppliedFiles {
                    written: i64::try_from(r.written).unwrap_or(0),
                    deleted: 0,
                    conflicts: Vec::new(),
                    skipped: Vec::new(),
                    failed: r
                        .failed
                        .into_iter()
                        .map(|(p, why)| format!("{p}: {why}"))
                        .collect(),
                }
            }
        }
    };
    e.touch(&copy.key);
    Ok(Json(out))
}

/// `DELETE /api/projects/:id/files-sync/roots/:root`: "Delete hub copy"
/// (only while the project's file sync is off, or its origin is revoked).
/// Copies elsewhere stay on disk and stop syncing.
async fn delete_root(
    State(s): State<SharedState>,
    ApiPath((id, root)): ApiPath<(String, String)>,
    Control(_): Control,
    _files: FilesAccess,
) -> ApiResult<StatusCode> {
    let e = need_engine(&s)?;
    let known = e
        .refresh_roots()
        .await
        .map_err(files_error)?
        .into_iter()
        .any(|r| r.root_id == root && r.project_id == id);
    if !known {
        return Err(ApiError::not_found("root"));
    }
    e.env
        .hub
        .delete_root(&root)
        .await
        .map_err(|err| files_error(err.into()))?;
    // This machine's copy of it (origin or not) forgets its sync state.
    let (store, r) = (s.store.clone(), root.clone());
    blocking(move || {
        for c in store.file_copies()?.into_iter().filter(|c| c.root_id == r) {
            if c.origin {
                store.remove_file_copy(&c.path)?;
            } else {
                store.detach_file_copy(&c.path)?;
            }
        }
        Ok(())
    })
    .await?;
    tracing::info!("hub copy of a project folder deleted");
    e.rescan();
    Ok(StatusCode::NO_CONTENT)
}

fn relay_path(id: &str, rest: &str) -> ApiResult<String> {
    if !blirp_core::is_safe_id(id) {
        return Err(ApiError::bad_request("invalid machine id"));
    }
    Ok(format!("/api/machines/{id}/files/download{rest}"))
}

/// Where a copy of `root` goes on this machine.
fn destination(s: &SharedState, root: &RootInfo, body: &DownloadFiles) -> ApiResult<PathBuf> {
    let home = blirp_core::paths::user_home().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_home",
            "this machine's home folder is unknown",
        )
    })?;
    if super::download::is_workspace(&root.path)
        && s.store.project_paths(&root.project_id)?.is_empty()
    {
        let dest = s
            .paths
            .workspace_dir(&root.project_id)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        return if empty_or_missing(&dest) {
            Ok(dest)
        } else {
            Err(ApiError::conflict(
                "already_exists",
                format!("{} already exists", dest.display()),
            ))
        };
    }
    let project = s.store.live_project(&root.project_id)?;
    let name = match body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        Some(n) => n.to_string(),
        None => super::download::folder_name(&root.path, &project.name),
    };
    match crate::clone::destination(&home, body.parent.as_deref(), &name) {
        Ok(d) => Ok(d),
        Err(crate::clone::CloneError::Exists(d)) if empty_or_missing(Path::new(&d)) => {
            Ok(PathBuf::from(d))
        }
        Err(crate::clone::CloneError::Exists(d)) => Err(ApiError::conflict(
            "already_exists",
            format!("{d} already exists and is not empty"),
        )),
        Err(crate::clone::CloneError::Invalid(m)) => Err(ApiError::bad_request(m)),
    }
}

pub(crate) fn empty_or_missing(p: &Path) -> bool {
    match std::fs::symlink_metadata(p) {
        Err(_) => true,
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {
            std::fs::read_dir(p).is_ok_and(|mut d| d.next().is_none())
        }
        Ok(_) => false,
    }
}

/// `POST /api/machines/:id/files/download`: make a copy of a root on that
/// machine ("Download from hub", "Use hub copy"). 202 with the job.
async fn download(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    Control(principal): Control,
    _files: FilesAccess,
    ApiJson(body): ApiJson<DownloadFiles>,
) -> ApiResult<Response> {
    if id != s.machine.id {
        let json =
            serde_json::to_vec(&body).map_err(|e| ApiError::internal("encoding download", e))?;
        let path = relay_path(&id, "")?;
        return crate::sync::forward(&s, &id, &principal, Method::POST, &path, Some(json)).await;
    }
    let e = need_engine(&s)?;
    let root = e
        .refresh_roots()
        .await
        .map_err(files_error)?
        .into_iter()
        .find(|r| r.root_id == body.root_id)
        .ok_or_else(|| ApiError::not_found("root"))?;
    if root.machine_id == s.machine.id {
        return Err(ApiError::conflict(
            "origin_here",
            "this machine has the original folder",
        ));
    }
    let st = s.clone();
    let (r2, b2) = (root.clone(), body.clone());
    let dest = blocking(move || {
        if let Some(c) = st
            .store
            .file_copies()?
            .into_iter()
            .find(|c| c.root_id == r2.root_id)
        {
            return Err(ApiError::conflict(
                "already_copied",
                format!("this machine has a copy already: {}", c.path),
            ));
        }
        destination(&st, &r2, &b2)
    })
    .await?;
    let job = s
        .files
        .downloads
        .start(&s, e, root, dest)
        .map_err(|err| match err {
            super::download::DownloadError::Invalid(m) => ApiError::bad_request(m),
            super::download::DownloadError::Conflict(m) => ApiError::conflict("already_running", m),
            super::download::DownloadError::Unavailable(m) => {
                ApiError::internal("starting a download", m)
            }
        })?;
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

async fn download_job(
    State(s): State<SharedState>,
    _files: FilesAccess,
    ApiPath((id, job)): ApiPath<(String, String)>,
    principal: Principal,
) -> ApiResult<Response> {
    if id != s.machine.id {
        let path = relay_path(&id, &format!("/{job}"))?;
        if !blirp_core::is_safe_id(&job) {
            return Err(ApiError::bad_request("invalid job id"));
        }
        return crate::sync::forward(&s, &id, &principal, Method::GET, &path, None).await;
    }
    s.files
        .downloads
        .get(&job)
        .map(|j| Json(j).into_response())
        .ok_or_else(|| ApiError::not_found("download job"))
}
