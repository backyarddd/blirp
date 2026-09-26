//! HTTP/WS API (§11): router, auth, origin check, security headers, errors.

mod files;
mod memory;
mod misc;
mod open;
mod projects;
mod sessions;
mod terminal;

use crate::state::SharedState;
use axum::extract::{FromRequest, FromRequestParts, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use blirp_core::model::{ErrorBody, ErrorDetail};
use blirp_core::store::StoreError;
use serde::Deserialize;

pub const COOKIE: &str = "blirp_session";

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }
    pub fn not_found(what: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("{what} not found"),
        )
    }
    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }
    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, "not_implemented", message)
    }
    /// Logs the real cause; clients get a generic message.
    pub fn internal(context: &str, err: impl std::fmt::Display) -> Self {
        tracing::error!(error = %err, "{context}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("{context} failed; see the daemon log"),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code.to_string(),
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound(what) => Self::not_found(what),
            StoreError::Conflict(m) => Self::conflict("conflict", m),
            StoreError::Invalid(m) => Self::bad_request(m),
            other => Self::internal("database operation", other),
        }
    }
}

/// Run blocking work (SQLite, git, filesystem) off the async runtime.
pub async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal("background task", e))?
}

/// `Json` extractor whose rejections use the API error shape.
pub struct ApiJson<T>(pub T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Self(v)),
            Err(r) => Err(ApiError::new(r.status(), "invalid_request", r.body_text())),
        }
    }
}

/// `Query` extractor whose rejections use the API error shape.
pub struct ApiQuery<T>(pub T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> FromRequestParts<S> for ApiQuery<T> {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(v)) => Ok(Self(v)),
            Err(r) => Err(ApiError::bad_request(r.body_text())),
        }
    }
}

pub fn router(state: SharedState) -> Router {
    let api = Router::new()
        .merge(misc::routes())
        .merge(projects::routes())
        .merge(memory::routes())
        .merge(files::routes())
        .merge(sessions::routes())
        .merge(open::routes())
        .route("/api/terminals/{id}/ws", get(terminal::attach))
        .merge(later_phase_routes())
        .route("/api/{*rest}", any(api_not_found))
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .merge(api)
        .route("/auth", get(auth_login))
        .fallback(crate::static_files::serve)
        .layer(middleware::from_fn(check_origin))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// Endpoints owned by later phases (hooks, injection, sync, devices, MCP).
fn later_phase_routes() -> Router<SharedState> {
    async fn later() -> ApiError {
        ApiError::not_implemented("this endpoint is not available in this version of blirp")
    }
    Router::new()
        .route("/api/hooks/{agent}/{event}", post(later))
        .route("/api/inject", get(later))
        .route("/api/sync/{*rest}", any(later))
        .route("/api/devices/{*rest}", any(later))
        .route("/api/sessions/{id}/distill", post(later))
        .route("/api/machines/{id}", axum::routing::delete(later))
        .route("/mcp", any(later))
}

async fn api_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such endpoint")
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "method not allowed for this endpoint",
    )
}

/// Constant-time comparison so the token cannot be recovered by timing.
fn token_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn request_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| cookie_value(headers, COOKIE))
}

async fn require_auth(State(state): State<SharedState>, req: Request, next: Next) -> Response {
    match request_token(req.headers()) {
        Some(t) if token_eq(t.trim(), &state.token) => next.run(req).await,
        _ => ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid token",
        )
        .into_response(),
    }
}

#[derive(Deserialize)]
struct AuthQuery {
    token: String,
}

/// `/auth?token=` exchanges the runtime token for an HttpOnly session cookie.
async fn auth_login(
    State(state): State<SharedState>,
    q: Result<Query<AuthQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    match q {
        Ok(Query(q)) if token_eq(&q.token, &state.token) => {
            let cookie = format!(
                "{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/",
                state.token
            );
            let mut resp = Redirect::to("/").into_response();
            match HeaderValue::from_str(&cookie) {
                Ok(v) => {
                    resp.headers_mut().insert(header::SET_COOKIE, v);
                    resp
                }
                Err(e) => ApiError::internal("building session cookie", e).into_response(),
            }
        }
        _ => ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid login link; run `blirp open` for a fresh one",
        )
        .into_response(),
    }
}

fn is_ws_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// CSRF defense (§13): a browser-sent `Origin` on a mutation or WebSocket
/// upgrade must match the host it was sent to. Non-browser clients (CLI,
/// hooks) send no `Origin` and authenticate with the bearer token.
async fn check_origin(req: Request, next: Next) -> Response {
    let mutating = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if (mutating || is_ws_upgrade(req.headers()))
        && let Some(origin) = req.headers().get(header::ORIGIN)
    {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok());
        let same = match (origin.to_str().ok(), host) {
            (Some(o), Some(h)) => {
                o.strip_prefix("http://")
                    .or_else(|| o.strip_prefix("https://"))
                    == Some(h)
            }
            _ => false,
        };
        if !same {
            return ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden_origin",
                "cross-origin request rejected",
            )
            .into_response();
        }
    }
    next.run(req).await
}

/// Scripts are self-only (no inline, no eval). Inline styles are allowed because
/// xterm.js and Svelte `style:` directives set element styles. Images allow
/// `data:` for generated QR codes and icons.
const CSP_BASE: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
img-src 'self' data:; font-src 'self'; object-src 'none'; base-uri 'self'; \
form-action 'self'; frame-ancestors 'none'";

/// `connect-src` names the same-origin WebSocket URLs explicitly: older
/// browsers do not match `ws:`/`wss:` against `'self'`.
fn csp(host: Option<&str>) -> String {
    let host = host.filter(|h| {
        !h.is_empty()
            && h.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
    });
    match host {
        Some(h) => format!("{CSP_BASE}; connect-src 'self' ws://{h} wss://{h}"),
        None => format!("{CSP_BASE}; connect-src 'self'"),
    }
}

async fn security_headers(req: Request, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    let policy = csp(req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok()));
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    match HeaderValue::from_str(&policy) {
        Ok(v) => {
            h.insert(header::CONTENT_SECURITY_POLICY, v);
        }
        // Unreachable: `csp` only emits ASCII from a filtered host.
        Err(e) => tracing::error!(error = %e, "invalid CSP header"),
    }
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if is_api {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison() {
        assert!(token_eq("abc", "abc"));
        assert!(!token_eq("abc", "abd"));
        assert!(!token_eq("abc", "abcd"));
    }

    #[test]
    fn csp_allows_same_origin_websockets_only() {
        let p = csp(Some("127.0.0.1:47770"));
        assert!(p.contains("script-src 'self';"));
        assert!(p.ends_with("connect-src 'self' ws://127.0.0.1:47770 wss://127.0.0.1:47770"));
        assert!(csp(Some("[::1]:47770")).contains("ws://[::1]:47770"));
        // A hostile Host header must not be able to add directives.
        for bad in ["evil; script-src *", "a b", ""] {
            assert!(csp(Some(bad)).ends_with("connect-src 'self'"), "{bad}");
        }
        assert!(csp(None).ends_with("connect-src 'self'"));
    }

    #[test]
    fn token_from_bearer_or_cookie() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; blirp_session=tok; b=2"),
        );
        assert_eq!(request_token(&h), Some("tok"));
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer bear"),
        );
        assert_eq!(request_token(&h), Some("bear"));
    }
}
