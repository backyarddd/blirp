//! HTTP/WS API (§11): router, auth, origin check, security headers, errors.

mod files;
mod integration;
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
use axum::routing::{any, get};
use axum::{Json, Router};
use blirp_core::model::{ErrorBody, ErrorDetail};
use blirp_core::store::StoreError;
use serde::Deserialize;

pub const COOKIE: &str = "blirp_session";
/// Long-lived browser device cookie on the LAN portal (§13).
pub const DEVICE_COOKIE: &str = "blirp_device";

/// Who is calling. `require_auth` attaches it to every authenticated
/// request; the sync proxy attaches it to requests relayed from another
/// machine (extensions never come from the network).
#[derive(Debug, Clone)]
pub struct Principal {
    /// May type into terminals and launch, resume or stop sessions.
    pub control: bool,
    /// May manage the hub, pairing and devices (local clients only).
    pub admin: bool,
    /// Portal browser device id.
    pub device: Option<String>,
    /// Short description for logs and proxied requests.
    pub label: String,
}

impl Principal {
    pub fn local() -> Self {
        Self {
            control: true,
            admin: true,
            device: None,
            label: "local".into(),
        }
    }

    pub fn require_control(&self) -> ApiResult<()> {
        if self.control {
            Ok(())
        } else {
            Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "control_not_allowed",
                "this device may not control terminals; allow it in Settings > Devices",
            ))
        }
    }

    pub fn require_admin(&self) -> ApiResult<()> {
        if self.admin {
            Ok(())
        } else {
            Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "admin_only",
                "only the machine's own desktop app or CLI can do this",
            ))
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Principal {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Principal>().cloned().ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "not authenticated",
            )
        })
    }
}

/// Marks requests that arrived on the LAN portal listener.
#[derive(Debug, Clone, Copy)]
pub struct PortalListener;

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

/// Which listener a router serves. Only the loopback listener mounts `/mcp`:
/// MCP clients are local agents, and the LAN portal and sync proxy have no
/// device-authenticated MCP yet.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Listener {
    Local,
    Portal,
    Proxy,
}

/// The local API router (127.0.0.1 listener).
pub fn router(state: SharedState) -> Router {
    build(state, Listener::Local)
}

/// The API for requests relayed by the sync proxy (no `/mcp`).
pub fn proxy_router(state: SharedState) -> Router {
    build(state, Listener::Proxy)
}

/// The same API for the LAN portal: device-cookie auth only, plus the
/// one-time browser login route (no `/mcp`).
pub fn portal_router(state: SharedState) -> Router {
    build(state, Listener::Portal)
}

fn build(state: SharedState, listener: Listener) -> Router {
    let mut api = Router::new()
        .merge(misc::routes())
        .merge(projects::routes())
        .merge(memory::routes())
        .merge(files::routes())
        .merge(sessions::routes())
        .merge(open::routes())
        .merge(integration::routes())
        .merge(crate::sync::routes())
        .route("/api/terminals/{id}/ws", get(terminal::attach));
    if listener == Listener::Local {
        api = api.merge(integration::mcp_routes(&state));
    }
    let api = api
        .route("/api/{*rest}", any(api_not_found))
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let app = match listener {
        Listener::Portal => Router::new()
            .merge(api)
            .route("/device-login", get(crate::portal::device_login)),
        Listener::Local | Listener::Proxy => {
            Router::new().merge(api).route("/auth", get(auth_login))
        }
    };
    let app = app
        .fallback(crate::static_files::serve)
        .layer(middleware::from_fn(check_origin))
        .layer(middleware::from_fn(security_headers))
        .with_state(state);
    if listener == Listener::Portal {
        app.layer(axum::Extension(PortalListener))
    } else {
        app
    }
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

fn unauthorized() -> Response {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "missing or invalid token",
    )
    .into_response()
}

async fn require_auth(State(state): State<SharedState>, mut req: Request, next: Next) -> Response {
    // Relayed by the sync proxy, which already authenticated the peer.
    if req.extensions().get::<Principal>().is_some() {
        return next.run(req).await;
    }
    if req.extensions().get::<PortalListener>().is_none() {
        return match request_token(req.headers()) {
            Some(t) if token_eq(t.trim(), &state.token) => {
                req.extensions_mut().insert(Principal::local());
                next.run(req).await
            }
            _ => unauthorized(),
        };
    }
    // LAN portal: browser device cookie only.
    let Some(token) = cookie_value(req.headers(), DEVICE_COOKIE).map(str::to_string) else {
        return unauthorized();
    };
    match crate::portal::authenticate(&state, token).await {
        Ok(Some(p)) => {
            req.extensions_mut().insert(p);
            next.run(req).await
        }
        Ok(None) => unauthorized(),
        Err(e) => e.into_response(),
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
