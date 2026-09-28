//! Brief, records, wiki, resources and suggestions of a project.

use super::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, Control, blocking};
use crate::state::SharedState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use blirp_core::model::{
    Brief, CreateRecord, CreateResource, CreateWikiPage, MemoryPart, PatchRecord, PatchResource,
    PutBrief, PutWikiPage, Record, RecordKind, RecordStatus, RenameWikiPage, Resource, RevertBrief,
    ServerEvent, Suggestion, SuggestionStatus, WikiPage,
};
use blirp_core::store::{RecordFilter, Store};
use serde::Deserialize;
use std::sync::Arc;

const BY_USER: &str = "user";
const MAX_TEXT: usize = 256 * 1024;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/projects/{id}/brief", axum::routing::put(put_brief))
        .route("/api/projects/{id}/brief/history", get(brief_history))
        .route("/api/projects/{id}/brief/revert", post(revert_brief))
        .route(
            "/api/projects/{id}/records",
            get(list_records).post(create_record),
        )
        .route(
            "/api/projects/{id}/records/{rid}",
            get(get_record).patch(patch_record).delete(delete_record),
        )
        .route("/api/projects/{id}/wiki", get(list_wiki).post(create_wiki))
        .route(
            "/api/projects/{id}/wiki/{slug}",
            get(get_wiki).put(put_wiki).delete(delete_wiki),
        )
        .route("/api/projects/{id}/wiki/{slug}/restore", post(restore_wiki))
        .route("/api/projects/{id}/wiki/{slug}/rename", post(rename_wiki))
        .route(
            "/api/projects/{id}/resources",
            get(list_resources).post(create_resource),
        )
        .route(
            "/api/projects/{id}/resources/{rid}",
            get(get_resource)
                .patch(patch_resource)
                .delete(delete_resource),
        )
        .route("/api/projects/{id}/suggestions", get(list_suggestions))
        .route("/api/suggestions/{id}/{action}", post(decide_suggestion))
}

pub(super) fn check_len(field: &str, v: &str) -> ApiResult<()> {
    if v.len() > MAX_TEXT {
        return Err(ApiError::bad_request(format!(
            "{field} exceeds {} KiB",
            MAX_TEXT / 1024
        )));
    }
    Ok(())
}

/// Run `f` after checking the project exists, then emit a memory update.
async fn project_op<T: Send + 'static>(
    s: &SharedState,
    project_id: String,
    part: Option<MemoryPart>,
    f: impl FnOnce(&Store, &str) -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    let store: Arc<Store> = s.store.clone();
    let pid = project_id.clone();
    let out = blocking(move || {
        store.live_project(&pid)?;
        f(&store, &pid)
    })
    .await?;
    if let Some(part) = part {
        s.emit(ServerEvent::MemoryUpdated { project_id, part });
    }
    Ok(out)
}

// ---------------------------------------------------------------- brief

async fn put_brief(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
    ApiJson(b): ApiJson<PutBrief>,
) -> ApiResult<Json<Brief>> {
    check_len("body_md", &b.body_md)?;
    project_op(&s, id, Some(MemoryPart::Brief), move |st, pid| {
        Ok(st.put_brief(pid, &b.body_md, BY_USER)?)
    })
    .await
    .map(Json)
}

async fn brief_history(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<Vec<Brief>>> {
    project_op(&s, id, None, |st, pid| Ok(st.brief_history(pid)?))
        .await
        .map(Json)
}

async fn revert_brief(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
    ApiJson(b): ApiJson<RevertBrief>,
) -> ApiResult<Json<Brief>> {
    project_op(&s, id, Some(MemoryPart::Brief), move |st, pid| {
        match (&b.id, b.version) {
            (Some(id), _) => Ok(st.revert_brief_to(pid, id, BY_USER)?),
            (None, Some(v)) => Ok(st.revert_brief(pid, v, BY_USER)?),
            (None, None) => Err(ApiError::bad_request("version or id is required")),
        }
    })
    .await
    .map(Json)
}

// ---------------------------------------------------------------- records

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordQuery {
    status: Option<RecordStatus>,
    kind: Option<RecordKind>,
}

async fn list_records(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<RecordQuery>,
) -> ApiResult<Json<Vec<Record>>> {
    project_op(&s, id, None, move |st, pid| {
        Ok(st.list_records(
            pid,
            &RecordFilter {
                status: q.status,
                kind: q.kind,
            },
        )?)
    })
    .await
    .map(Json)
}

/// A record that belongs to the project in the path.
fn owned_record(st: &Store, pid: &str, rid: &str) -> ApiResult<Record> {
    st.get_record(rid)?
        .filter(|r| r.project_id == pid)
        .ok_or_else(|| ApiError::not_found("record"))
}

async fn create_record(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
    ApiJson(b): ApiJson<CreateRecord>,
) -> ApiResult<(StatusCode, Json<Record>)> {
    check_len("title", &b.title)?;
    check_len("body", &b.body)?;
    let r = project_op(&s, id, Some(MemoryPart::Records), move |st, pid| {
        let now = blirp_core::now_ms();
        Ok(st.create_record(Record {
            id: blirp_core::new_id(),
            project_id: pid.to_string(),
            kind: b.kind,
            title: b.title.trim().to_string(),
            body: b.body,
            status: b.status.unwrap_or(RecordStatus::Active),
            pinned: b.pinned.unwrap_or(false),
            source_session_id: None,
            created_at: now,
            updated_at: now,
            updated_by: BY_USER.into(),
        })?)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(r)))
}

async fn get_record(
    State(s): State<SharedState>,
    ApiPath((id, rid)): ApiPath<(String, String)>,
) -> ApiResult<Json<Record>> {
    project_op(&s, id, None, move |st, pid| owned_record(st, pid, &rid))
        .await
        .map(Json)
}

async fn patch_record(
    State(s): State<SharedState>,
    _: Control,
    ApiPath((id, rid)): ApiPath<(String, String)>,
    ApiJson(p): ApiJson<PatchRecord>,
) -> ApiResult<Json<Record>> {
    for (f, v) in [("title", &p.title), ("body", &p.body)] {
        if let Some(v) = v {
            check_len(f, v)?;
        }
    }
    let target = p.project_id.clone().filter(|to| *to != id);
    let moved_to = target.clone();
    let rec = project_op(&s, id, Some(MemoryPart::Records), move |st, pid| {
        owned_record(st, pid, &rid)?;
        if let Some(to) = &target
            && st.live_project(to)?.chats
        {
            return Err(ApiError::bad_request(
                "Chats has no project memory; move the record to a project",
            ));
        }
        Ok(st.modify_record(&rid, |r| {
            if let Some(to) = target {
                r.project_id = to;
            }
            if let Some(k) = p.kind {
                r.kind = k;
            }
            if let Some(t) = p.title {
                r.title = t.trim().to_string();
            }
            if let Some(b) = p.body {
                r.body = b;
            }
            if let Some(st) = p.status {
                r.status = st;
            }
            if let Some(pin) = p.pinned {
                r.pinned = pin;
            }
            r.updated_by = BY_USER.into();
        })?)
    })
    .await?;
    if let Some(project_id) = moved_to {
        s.emit(ServerEvent::MemoryUpdated {
            project_id,
            part: MemoryPart::Records,
        });
    }
    Ok(Json(rec))
}

async fn delete_record(
    State(s): State<SharedState>,
    _: Control,
    ApiPath((id, rid)): ApiPath<(String, String)>,
) -> ApiResult<StatusCode> {
    project_op(&s, id, Some(MemoryPart::Records), move |st, pid| {
        owned_record(st, pid, &rid)?;
        Ok(st.delete_record(&rid)?)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- wiki

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WikiQuery {
    /// Deleted pages (restorable) instead of the live ones.
    deleted: Option<bool>,
}

async fn list_wiki(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<WikiQuery>,
) -> ApiResult<Json<Vec<WikiPage>>> {
    project_op(&s, id, None, move |st, pid| {
        Ok(if q.deleted.unwrap_or(false) {
            st.list_deleted_wiki(pid)?
        } else {
            st.list_wiki(pid)?
        })
    })
    .await
    .map(Json)
}

async fn restore_wiki(
    State(s): State<SharedState>,
    _: Control,
    ApiPath((id, slug)): ApiPath<(String, String)>,
) -> ApiResult<Json<WikiPage>> {
    project_op(&s, id, Some(MemoryPart::Wiki), move |st, pid| {
        Ok(st.restore_wiki_page(pid, &slug, BY_USER)?)
    })
    .await
    .map(Json)
}

/// A new slug for a page. Nothing refers to pages by slug except links a
/// user or agent wrote into text, which are left as they are; the web UI
/// follows the page to its new address.
async fn rename_wiki(
    State(s): State<SharedState>,
    _: Control,
    ApiPath((id, slug)): ApiPath<(String, String)>,
    ApiJson(b): ApiJson<RenameWikiPage>,
) -> ApiResult<Json<WikiPage>> {
    project_op(&s, id, Some(MemoryPart::Wiki), move |st, pid| {
        Ok(st.rename_wiki_page(pid, &slug, b.slug.trim(), BY_USER)?)
    })
    .await
    .map(Json)
}

async fn create_wiki(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
    ApiJson(b): ApiJson<CreateWikiPage>,
) -> ApiResult<(StatusCode, Json<WikiPage>)> {
    check_len("body_md", &b.body_md)?;
    let w = project_op(&s, id, Some(MemoryPart::Wiki), move |st, pid| {
        Ok(st.create_wiki_page(pid, &b.slug, b.title.trim(), &b.body_md, BY_USER)?)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(w)))
}

async fn get_wiki(
    State(s): State<SharedState>,
    ApiPath((id, slug)): ApiPath<(String, String)>,
) -> ApiResult<Json<WikiPage>> {
    project_op(&s, id, None, move |st, pid| {
        st.get_wiki_page(pid, &slug)?
            .ok_or_else(|| ApiError::not_found("wiki page"))
    })
    .await
    .map(Json)
}

async fn put_wiki(
    State(s): State<SharedState>,
    _: Control,
    ApiPath((id, slug)): ApiPath<(String, String)>,
    ApiJson(b): ApiJson<PutWikiPage>,
) -> ApiResult<Json<WikiPage>> {
    check_len("body_md", &b.body_md)?;
    project_op(&s, id, Some(MemoryPart::Wiki), move |st, pid| {
        Ok(st.update_wiki_page(pid, &slug, b.title.trim(), &b.body_md, BY_USER)?)
    })
    .await
    .map(Json)
}

async fn delete_wiki(
    State(s): State<SharedState>,
    _: Control,
    ApiPath((id, slug)): ApiPath<(String, String)>,
) -> ApiResult<StatusCode> {
    project_op(&s, id, Some(MemoryPart::Wiki), move |st, pid| {
        Ok(st.delete_wiki_page(pid, &slug, BY_USER)?)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- resources

fn validate_url(url: &str) -> ApiResult<()> {
    let url = url.trim();
    if url.is_empty() || url.len() > 4096 || url.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "url must be 1-4096 characters without control characters",
        ));
    }
    Ok(())
}

fn owned_resource(st: &Store, pid: &str, rid: &str) -> ApiResult<Resource> {
    st.get_resource(rid)?
        .filter(|r| r.project_id == pid)
        .ok_or_else(|| ApiError::not_found("resource"))
}

async fn list_resources(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<Vec<Resource>>> {
    project_op(&s, id, None, |st, pid| Ok(st.list_resources(pid)?))
        .await
        .map(Json)
}

async fn create_resource(
    State(s): State<SharedState>,
    _: Control,
    ApiPath(id): ApiPath<String>,
    ApiJson(b): ApiJson<CreateResource>,
) -> ApiResult<(StatusCode, Json<Resource>)> {
    validate_url(&b.url)?;
    let r = project_op(&s, id, Some(MemoryPart::Resources), move |st, pid| {
        let now = blirp_core::now_ms();
        Ok(st.create_resource(Resource {
            id: blirp_core::new_id(),
            project_id: pid.to_string(),
            kind: b.kind,
            url: b.url.trim().to_string(),
            title: b.title.trim().to_string(),
            meta: b.meta,
            created_at: now,
            updated_at: now,
            deleted: false,
        })?)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(r)))
}

async fn get_resource(
    State(s): State<SharedState>,
    ApiPath((id, rid)): ApiPath<(String, String)>,
) -> ApiResult<Json<Resource>> {
    project_op(&s, id, None, move |st, pid| owned_resource(st, pid, &rid))
        .await
        .map(Json)
}

async fn patch_resource(
    State(s): State<SharedState>,
    _: Control,
    ApiPath((id, rid)): ApiPath<(String, String)>,
    ApiJson(p): ApiJson<PatchResource>,
) -> ApiResult<Json<Resource>> {
    if let Some(u) = &p.url {
        validate_url(u)?;
    }
    project_op(&s, id, Some(MemoryPart::Resources), move |st, pid| {
        owned_resource(st, pid, &rid)?;
        Ok(st.modify_resource(&rid, |r| {
            if let Some(k) = p.kind {
                r.kind = k;
            }
            if let Some(u) = p.url {
                r.url = u.trim().to_string();
            }
            if let Some(t) = p.title {
                r.title = t.trim().to_string();
            }
            if let Some(m) = p.meta {
                r.meta = Some(m);
            }
        })?)
    })
    .await
    .map(Json)
}

async fn delete_resource(
    State(s): State<SharedState>,
    _: Control,
    ApiPath((id, rid)): ApiPath<(String, String)>,
) -> ApiResult<StatusCode> {
    project_op(&s, id, Some(MemoryPart::Resources), move |st, pid| {
        owned_resource(st, pid, &rid)?;
        st.modify_resource(&rid, |r| r.deleted = true)?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- suggestions

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuggestionQuery {
    status: Option<SuggestionStatus>,
}

async fn list_suggestions(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<SuggestionQuery>,
) -> ApiResult<Json<Vec<Suggestion>>> {
    project_op(&s, id, None, move |st, pid| {
        Ok(st.list_suggestions(pid, q.status)?)
    })
    .await
    .map(Json)
}

async fn decide_suggestion(
    State(s): State<SharedState>,
    _: Control,
    ApiPath((id, action)): ApiPath<(String, String)>,
) -> ApiResult<Json<Suggestion>> {
    let decision = match action.as_str() {
        "accept" => SuggestionStatus::Accepted,
        "reject" => SuggestionStatus::Rejected,
        "dismiss" => SuggestionStatus::Dismissed,
        _ => return Err(ApiError::not_found("suggestion action")),
    };
    let store = s.store.clone();
    let sug = blocking(move || Ok(store.decide_suggestion(&id, decision, BY_USER)?)).await?;
    s.emit(ServerEvent::MemoryUpdated {
        project_id: sug.project_id.clone(),
        part: MemoryPart::Suggestions,
    });
    if decision == SuggestionStatus::Accepted {
        let part = match sug.target {
            blirp_core::model::SuggestionTarget::Brief => MemoryPart::Brief,
            blirp_core::model::SuggestionTarget::Record => MemoryPart::Records,
            blirp_core::model::SuggestionTarget::Wiki => MemoryPart::Wiki,
        };
        s.emit(ServerEvent::MemoryUpdated {
            project_id: sug.project_id.clone(),
            part,
        });
    }
    Ok(Json(sug))
}
