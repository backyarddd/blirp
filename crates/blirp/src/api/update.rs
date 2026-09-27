//! `/api/update*`: whether a newer blirp release exists, a manual check, and
//! the Settings > About "Update now" button.

use super::{Admin, ApiError, ApiResult};
use crate::state::SharedState;
use crate::update::{self, Checked};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use blirp_core::model::{UpdateOutcome, UpdateStatus};
use std::path::PathBuf;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/update", get(status))
        .route("/api/update/check", post(check))
}

/// Loopback listener only, like `/api/daemon/shutdown`: it restarts this
/// daemon. Neither portal devices nor relayed requests can update a machine.
pub fn local_routes() -> Router<SharedState> {
    Router::new().route("/api/update/apply", post(apply))
}

/// What the receipt and the update log say (file reads, off the runtime).
struct Local {
    /// The script-installed CLI when `blirp update` can replace this binary.
    cli: Option<PathBuf>,
    last_update: Option<UpdateOutcome>,
}

async fn local(s: &SharedState) -> Local {
    let cli = s.updates.overrides().cli;
    let paths = s.paths.clone();
    let read = tokio::task::spawn_blocking(move || Local {
        cli: cli.or_else(installed_cli),
        last_update: update::last_outcome(&paths),
    });
    read.await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "read the install receipt and update log");
        Local {
            cli: None,
            last_update: None,
        }
    })
}

/// The installed CLI, when `blirp update` would replace this binary (it
/// refuses anything the install script did not install). A daemon started
/// by the desktop app runs its sidecar; the CLI next to the receipt updates
/// both.
fn installed_cli() -> Option<PathBuf> {
    match update::install::self_updating() {
        Ok(true) => update::install::installed_cli(),
        Ok(false) => None,
        Err(e) => {
            tracing::warn!(error = format!("{e:#}"), "read the install receipt");
            None
        }
    }
}

fn to_status(checked: Option<&Checked>, enabled: bool, local: Local) -> UpdateStatus {
    let current = update::CURRENT.to_string();
    let latest = checked.and_then(|c| c.latest.as_ref().ok());
    let available =
        latest.is_some_and(|l| update::parse_version(&current).is_ok_and(|c| l.version > c));
    UpdateStatus {
        current,
        latest: latest.map(|l| l.version.to_string()),
        available,
        notes_url: latest.map(|l| l.notes_url.clone()),
        enabled,
        self_update: local.cli.is_some(),
        checked_at: checked.map(|c| c.at_ms),
        error: checked.and_then(|c| c.latest.as_ref().err().cloned()),
        last_update: local.last_update,
    }
}

/// Asks GitHub at most once a day (hourly after a failure), and only while
/// `[update] check` is on.
async fn status(State(s): State<SharedState>) -> Json<UpdateStatus> {
    let enabled = s.config().update.check;
    let checked = if enabled {
        Some(s.updates.latest(false).await)
    } else {
        None
    };
    Json(to_status(checked.as_ref(), enabled, local(&s).await))
}

fn checks_off() -> ApiError {
    ApiError::conflict(
        "update_checks_off",
        "update checks are off ([update] check = false in config.toml)",
    )
}

/// "Check now": asks GitHub unless it was asked less than a minute ago
/// (then that answer), so repeated clicks cannot use up the API rate limit.
async fn check(State(s): State<SharedState>, _: Admin) -> ApiResult<Json<UpdateStatus>> {
    if !s.config().update.check {
        return Err(checks_off());
    }
    let checked = s.updates.latest(true).await;
    Ok(Json(to_status(Some(&checked), true, local(&s).await)))
}

/// An updater started this long ago without an outcome no longer blocks a
/// new attempt (it died without recording one).
const APPLY_TIMEOUT_MS: i64 = 15 * 60 * 1000;

/// "Update now": runs `blirp update --version <latest>` from the installed
/// CLI as a detached process, which stops this daemon, replaces blirp and
/// starts it again (all or nothing; see `cli::install::update`). 202 right
/// away; the outcome lands in `logs/update.log` (`last_update`).
async fn apply(State(s): State<SharedState>, _: Admin) -> ApiResult<StatusCode> {
    if !s.config().update.check {
        return Err(checks_off());
    }
    let checked = s.updates.latest(false).await;
    let local = local(&s).await;
    let Some(cli) = local.cli.clone() else {
        return Err(ApiError::conflict(
            "not_self_update",
            "this blirp was not installed by the install script; update it the way you installed it",
        ));
    };
    let last_finished = local.last_update.as_ref().map(|o| o.finished_at);
    let status = to_status(Some(&checked), true, local);
    let target = match &checked.latest {
        Ok(l) if status.available => l.version.clone(),
        Ok(_) => {
            return Err(ApiError::conflict(
                "no_update",
                format!("blirp {} is up to date", update::CURRENT),
            ));
        }
        Err(e) => {
            return Err(ApiError::conflict(
                "no_update",
                format!("could not check for updates: {e}"),
            ));
        }
    };
    let now = blirp_core::now_ms();
    {
        let mut started = s
            .updates
            .started_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(at) = *started
            && now - at < APPLY_TIMEOUT_MS
            && last_finished.is_none_or(|f| f < at)
        {
            return Err(ApiError::conflict(
                "update_in_progress",
                "an update is already running",
            ));
        }
        *started = Some(now);
    }
    let args = update::updater_args(&target);
    let spawn: update::Spawner = s
        .updates
        .overrides()
        .spawn
        .unwrap_or_else(|| std::sync::Arc::new(update::start_updater));
    let home = s.paths.home().to_path_buf();
    let started = tokio::task::spawn_blocking(move || spawn(&cli, &args, &home))
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)));
    match started {
        Ok(()) => {
            tracing::info!(
                from = update::CURRENT,
                to = %target,
                "update started from the UI"
            );
            Ok(StatusCode::ACCEPTED)
        }
        Err(e) => {
            *s.updates
                .started_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            Err(ApiError::internal("starting the updater", e))
        }
    }
}
