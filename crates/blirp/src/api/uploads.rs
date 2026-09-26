//! `POST /api/sessions/:id/uploads`: a file pasted or dropped into a
//! terminal, saved on the machine that runs the session (§6).

use super::{ApiError, ApiPath, ApiQuery, ApiResult, Control, blocking};
use crate::state::SharedState;
use crate::uploads::MAX_BYTES;
use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use blirp_core::model::UploadedFile;
use futures_util::StreamExt as _;
use serde::Deserialize;

pub fn routes() -> Router<SharedState> {
    Router::new().route("/api/sessions/{id}/uploads", post(upload))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadQuery {
    /// The client's file name; only a sanitized last component is used.
    name: Option<String>,
}

fn too_large() -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "file_too_large",
        format!(
            "files larger than {} MB cannot be uploaded",
            MAX_BYTES >> 20
        ),
    )
}

/// The raw body, refused past `MAX_BYTES` (by `Content-Length` before
/// reading anything, else while reading).
async fn read_body(headers: &HeaderMap, body: Body) -> ApiResult<Bytes> {
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_BYTES as u64) {
        return Err(too_large());
    }
    let mut stream = body.into_data_stream();
    let mut buf = Vec::with_capacity(declared.map_or(0, |n| n as usize));
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| ApiError::bad_request(format!("reading the upload failed: {e}")))?;
        if buf.len() + chunk.len() > MAX_BYTES {
            return Err(too_large());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

/// Same right as typing into the terminal (`Control`); a session on
/// another machine is forwarded to it through the hub, like its terminal.
async fn upload(
    State(s): State<SharedState>,
    Control(principal): Control,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let name = crate::uploads::sanitize_name(q.name.as_deref().unwrap_or_default());
    if let Some(m) = super::sessions::remote_machine(&s, &id).await? {
        let bytes = read_body(&headers, body).await?;
        let mut url = reqwest::Url::parse("http://blirp.remote/")
            .map_err(|e| ApiError::internal("building the upload path", e))?;
        url.path_segments_mut()
            .map_err(|()| ApiError::internal("building the upload path", "cannot be a base"))?
            .extend(["api", "sessions", id.as_str(), "uploads"]);
        url.query_pairs_mut().append_pair("name", &name);
        let path = format!("{}?{}", url.path(), url.query().unwrap_or_default());
        return crate::sync::forward_body(
            &s,
            &m,
            &principal,
            Method::POST,
            &path,
            Some(("application/octet-stream", bytes)),
        )
        .await;
    }
    if s.terminals.get(&id).is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "terminal_not_found",
            "the session is not running",
        ));
    }
    let bytes = read_body(&headers, body).await?;
    let paths = s.paths.clone();
    let saved = blocking(move || {
        let path = crate::uploads::save(&paths, &id, &name, &bytes, blirp_core::now_ms())
            .map_err(|e| ApiError::internal("saving the upload", format!("{e:#}")))?;
        let path = path.to_string_lossy().into_owned();
        Ok(UploadedFile {
            quoted: crate::uploads::quote_path(&path, cfg!(windows)),
            path,
            size: bytes.len() as u64,
        })
    })
    .await?;
    Ok((StatusCode::CREATED, Json(saved)).into_response())
}
