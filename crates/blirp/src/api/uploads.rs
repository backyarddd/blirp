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

/// How much more of a refused upload is read and thrown away before the
/// 413 goes out. A client busy sending does not read the answer yet, and a
/// server that closes the connection with the body unread makes the TCP
/// stack reset it, so the client sees ECONNRESET instead of the 413. Past
/// this bound (or [`DRAIN_TIME`]) the connection is closed regardless.
const DRAIN_MAX: u64 = 4 * MAX_BYTES as u64;
const DRAIN_TIME: std::time::Duration = std::time::Duration::from_secs(30);

/// Read and discard the rest of a refused body, within the bounds above.
async fn drain(stream: &mut axum::body::BodyDataStream, mut read: u64) {
    let _ = tokio::time::timeout(DRAIN_TIME, async {
        while let Some(Ok(chunk)) = stream.next().await {
            read += chunk.len() as u64;
            if read > MAX_BYTES as u64 + DRAIN_MAX {
                break;
            }
        }
    })
    .await;
}

fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
}

/// Throw away the body of a request refused before reading it, so the
/// client receives the answer (see [`DRAIN_MAX`]). A body the drain would
/// give up on anyway is not read at all.
async fn discard(headers: &HeaderMap, body: Body) {
    if declared_length(headers).is_none_or(|n| n <= MAX_BYTES as u64 + DRAIN_MAX) {
        drain(&mut body.into_data_stream(), 0).await;
    }
}

/// The raw body, refused past `MAX_BYTES` (by `Content-Length` before
/// keeping anything, else while reading).
async fn read_body(headers: &HeaderMap, body: Body) -> ApiResult<Bytes> {
    let declared = declared_length(headers);
    if declared.is_some_and(|n| n > MAX_BYTES as u64) {
        discard(headers, body).await;
        return Err(too_large());
    }
    let mut stream = body.into_data_stream();
    // A declared length is only a hint: never reserve more than 1 MiB up front.
    let mut buf = Vec::with_capacity(declared.map_or(0, |n| n.min(1 << 20) as usize));
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| ApiError::bad_request(format!("reading the upload failed: {e}")))?;
        if buf.len() + chunk.len() > MAX_BYTES {
            drain(&mut stream, (buf.len() + chunk.len()) as u64).await;
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
    let remote = match super::sessions::remote_machine(&s, &id).await {
        Ok(m) => m,
        Err(e) => {
            discard(&headers, body).await;
            return Err(e);
        }
    };
    if let Some(m) = remote {
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
    let not_running = || {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "terminal_not_found",
            "the session is not running",
        )
    };
    if s.terminals.get(&id).is_none() {
        discard(&headers, body).await;
        return Err(not_running());
    }
    let bytes = read_body(&headers, body).await?;
    // Reading a large body takes a while; the session may have ended since.
    if s.terminals.get(&id).is_none() {
        return Err(not_running());
    }
    let paths = s.paths.clone();
    let saved = blocking(move || {
        let path = crate::uploads::save(&paths, &id, &name, &bytes, blirp_core::now_ms()).map_err(
            |e| match e.downcast_ref::<crate::uploads::QuotaExceeded>() {
                Some(q) => ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "upload_quota_exceeded",
                    q.to_string(),
                ),
                None => ApiError::internal("saving the upload", format!("{e:#}")),
            },
        )?;
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
