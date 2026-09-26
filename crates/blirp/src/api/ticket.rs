//! Single-use WebSocket tickets (§11). Browsers cannot send
//! `Authorization` on a WebSocket upgrade, and the loopback listener takes
//! no cookies (a cookie for 127.0.0.1 reaches every local server, whatever
//! its port). The SPA trades its bearer token for a ticket bound to one
//! path, valid once within 30 s, and opens `<path>?ticket=<ticket>`.

use super::{Admin, ApiError, ApiJson, ApiResult};
use crate::state::SharedState;
use axum::Json;
use axum::extract::State;
use blirp_core::model::{WsTicket, WsTicketRequest};
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

pub const TICKET_TTL: Duration = Duration::from_secs(30);

/// Open tickets: ticket -> (path, expiry). In memory only.
#[derive(Default)]
pub struct WsTickets {
    inner: Mutex<HashMap<String, (String, Instant)>>,
}

impl WsTickets {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (String, Instant)>> {
        // Plain data; a panic elsewhere cannot leave it half-updated.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A 128-bit random ticket for `path`, valid until `now + TICKET_TTL`.
    pub fn issue(&self, path: &str, now: Instant) -> std::io::Result<String> {
        let ticket = blirp_core::random_hex::<16>()?;
        let mut map = self.lock();
        map.retain(|_, (_, exp)| *exp > now);
        map.insert(ticket.clone(), (path.to_string(), now + TICKET_TTL));
        Ok(ticket)
    }

    /// Consume `ticket`. True only for an unexpired ticket issued for
    /// exactly `path`; a ticket presented once is gone whatever the outcome.
    pub fn redeem(&self, ticket: &str, path: &str, now: Instant) -> bool {
        let mut map = self.lock();
        map.retain(|_, (_, exp)| *exp > now);
        map.remove(ticket).is_some_and(|(p, _)| p == path)
    }
}

/// The `ticket` query parameter of a request URI.
pub fn from_query(query: Option<&str>) -> Option<&str> {
    query?
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == "ticket")
        .map(|(_, v)| v)
}

/// The WebSocket routes of the API: `/api/events/ws`, `/api/terminals/:id/ws`.
fn is_ws_path(path: &str) -> bool {
    path.len() <= 512
        && path.starts_with("/api/")
        && path.ends_with("/ws")
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_.%".contains(&b))
}

/// `POST /api/ws-ticket` (loopback listener, local clients only).
pub async fn issue(
    State(s): State<SharedState>,
    _: Admin,
    ApiJson(body): ApiJson<WsTicketRequest>,
) -> ApiResult<Json<WsTicket>> {
    if !is_ws_path(&body.path) {
        return Err(ApiError::bad_request(
            "path must be a WebSocket path under /api/",
        ));
    }
    let ticket = s
        .ws_tickets
        .issue(&body.path, Instant::now())
        .map_err(|e| ApiError::internal("generating a ticket", e))?;
    Ok(Json(WsTicket { ticket }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const EVENTS: &str = "/api/events/ws";

    #[test]
    fn single_use() {
        let t = WsTickets::default();
        let now = Instant::now();
        let a = t.issue(EVENTS, now).unwrap();
        assert_eq!(a.len(), 32);
        assert!(t.redeem(&a, EVENTS, now));
        assert!(!t.redeem(&a, EVENTS, now), "reused");
        assert!(!t.redeem("nope", EVENTS, now));
    }

    #[test]
    fn expires() {
        let t = WsTickets::default();
        let now = Instant::now();
        let a = t.issue(EVENTS, now).unwrap();
        let b = t.issue(EVENTS, now).unwrap();
        assert!(t.redeem(&a, EVENTS, now + TICKET_TTL - Duration::from_millis(1)));
        assert!(!t.redeem(&b, EVENTS, now + TICKET_TTL));
        assert!(t.lock().is_empty(), "expired tickets are dropped");
    }

    #[test]
    fn bound_to_its_path() {
        let t = WsTickets::default();
        let now = Instant::now();
        let a = t.issue("/api/terminals/s1/ws", now).unwrap();
        assert!(!t.redeem(&a, "/api/terminals/s2/ws", now));
        // A mismatch burns it too.
        assert!(!t.redeem(&a, "/api/terminals/s1/ws", now));
    }

    #[test]
    fn query_and_paths() {
        assert_eq!(from_query(Some("a=1&ticket=abc")), Some("abc"));
        assert_eq!(from_query(Some("tickets=abc")), None);
        assert_eq!(from_query(None), None);
        assert!(is_ws_path(EVENTS));
        assert!(is_ws_path("/api/terminals/0190-ab_c/ws"));
        for bad in [
            "/ws",
            "/api/health",
            "/api/x/ws?y=1",
            "/api/a b/ws",
            "//x/ws",
        ] {
            assert!(!is_ws_path(bad), "{bad}");
        }
    }
}
