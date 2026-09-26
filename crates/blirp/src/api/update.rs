//! `GET /api/update`: whether a newer blirp release exists (Settings > About).

use crate::state::SharedState;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use blirp_core::model::UpdateStatus;

pub fn routes() -> Router<SharedState> {
    Router::new().route("/api/update", get(status))
}

/// Asks GitHub at most once a day (`update::latest_cached`), and only while
/// `[update] check` is on. Updating itself is `blirp update` in a terminal.
async fn status(State(s): State<SharedState>) -> Json<UpdateStatus> {
    let current = crate::update::CURRENT.to_string();
    let enabled = s.config().update.check;
    let latest = if enabled {
        crate::update::latest_cached().await
    } else {
        None
    };
    let available = latest
        .as_ref()
        .is_some_and(|l| crate::update::parse_version(&current).is_ok_and(|c| l.version > c));
    Json(UpdateStatus {
        current,
        latest: latest.as_ref().map(|l| l.version.to_string()),
        available,
        notes_url: latest.map(|l| l.notes_url.clone()),
        enabled,
    })
}
