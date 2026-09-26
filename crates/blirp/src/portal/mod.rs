//! LAN portal (§13): on a hub with `portal.lan = true`, the same router is
//! served over HTTPS on `0.0.0.0:<portal.lan_port>` with a self-signed
//! certificate kept in `~/.blirp/tls/` (its SHA-256 fingerprint is shown in
//! the UI so users can verify the browser warning). Browsers authenticate
//! with a device cookie: a random 256-bit token (stored as SHA-256), issued
//! by redeeming a one-time invite link (5 minutes, single use, redemption
//! rate limited per client address). Devices start without terminal control.

use crate::api::{ApiError, ApiResult, DEVICE_COOKIE, Principal};
use crate::state::SharedState;
use anyhow::Context as _;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use blirp_core::model::{BrowserInvite, Device, DeviceKind, MachineRole};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::task::JoinHandle;

const INVITE_TTL_MS: i64 = 5 * 60 * 1000;
const RATE_WINDOW_MS: i64 = 60 * 1000;
const RATE_MAX: u32 = 10;
/// Browsers cap cookie lifetimes at 400 days.
const COOKIE_MAX_AGE: u64 = 400 * 24 * 3600;
const TOUCH_EVERY_MS: i64 = 60 * 1000;

struct Running {
    handle: axum_server::Handle<SocketAddr>,
    port: u16,
    url: String,
    fingerprint: String,
    task: JoinHandle<()>,
}

#[derive(Default)]
pub struct PortalState {
    running: Mutex<Option<Running>>,
    /// SHA-256(invite token) -> expiry.
    invites: Mutex<HashMap<String, i64>>,
    /// Redemption attempts per client address in the current window.
    attempts: Mutex<HashMap<IpAddr, (i64, u32)>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Plain maps; poisoning cannot leave them inconsistent.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// `AA:BB:...` SHA-256 of a DER certificate.
fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The address other devices on the LAN reach this machine at. Connecting
/// a UDP socket sends nothing; it only selects the outbound interface.
fn lan_ip() -> IpAddr {
    std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|s| {
            s.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
            s.local_addr()
        })
        .map(|a| a.ip())
        .ok()
        .filter(|ip| !ip.is_unspecified())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// Load `tls/cert.pem` + `tls/key.pem`, generating them on first use, when
/// they are unreadable or do not belong together, and when the LAN address
/// moved out of the certificate's names (a new fingerprint: browsers ask to
/// trust it again).
fn load_or_create_cert(
    dir: &Path,
    ip: IpAddr,
) -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    if cert_path.exists() && key_path.exists() {
        match read_cert(&cert_path, &key_path) {
            Ok((cert, _)) if !names_ip(&cert, ip) => {
                tracing::info!(%ip, "LAN address changed; new portal certificate");
            }
            Ok((cert, key)) => match tls_config(cert.clone(), key.clone_key()) {
                Ok(_) => return Ok((cert, key)),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "portal certificate unusable; making a new one")
                }
            },
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "portal certificate unreadable; making a new one")
            }
        }
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    if !ip.is_loopback() {
        names.push(ip.to_string());
    }
    let ck = rcgen::generate_simple_self_signed(names).context("generate certificate")?;
    // Complete files renamed into place; a crash between the two renames
    // leaves a pair that does not match, which the next start replaces.
    let key_tmp = dir.join("key.pem.tmp");
    let cert_tmp = dir.join("cert.pem.tmp");
    blirp_core::paths::write_private(&key_tmp, ck.signing_key.serialize_pem().as_bytes())?;
    std::fs::write(&cert_tmp, ck.cert.pem())
        .with_context(|| format!("write {}", cert_tmp.display()))?;
    for (from, to) in [(&key_tmp, &key_path), (&cert_tmp, &cert_path)] {
        std::fs::rename(from, to).with_context(|| format!("replace {}", to.display()))?;
    }
    read_cert(&cert_path, &key_path)
}

/// Server TLS settings for the portal; fails when key and certificate do
/// not belong together.
fn tls_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<rustls::ServerConfig> {
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("TLS protocol versions")?
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)
    .context("TLS certificate")?;
    // HTTP/1.1 only: terminal WebSockets need the HTTP/1.1 upgrade.
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tls)
}

fn read_cert(
    cert_path: &Path,
    key_path: &Path,
) -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let cert = CertificateDer::from_pem_file(cert_path)
        .with_context(|| format!("read {}", cert_path.display()))?;
    let key = PrivateKeyDer::from_pem_file(key_path)
        .with_context(|| format!("read {}", key_path.display()))?;
    Ok((cert, key))
}

/// The certificate is valid for `ip` (loopback is always among its names).
fn names_ip(cert: &CertificateDer<'_>, ip: IpAddr) -> bool {
    ip.is_loopback()
        || rustls::server::ParsedCertificate::try_from(cert).is_ok_and(|c| {
            rustls::client::verify_server_name(
                &c,
                &rustls::pki_types::ServerName::IpAddress(ip.into()),
            )
            .is_ok()
        })
}

/// Start, stop or restart the portal to match the config (hub +
/// `portal.lan`, on `portal.lan_port`). Callers hold the sync transition
/// lock so two changes never race.
pub async fn sync_with_config(state: &SharedState) -> anyhow::Result<()> {
    let cfg = state.config();
    let want = (cfg.sync.role == MachineRole::Hub && cfg.portal.lan).then_some(cfg.portal.lan_port);
    let running = lock(&state.sync.portal.running).as_ref().map(|r| r.port);
    match (want, running) {
        (Some(_), None) => start(state).await,
        (None, Some(_)) => {
            stop(state).await;
            Ok(())
        }
        (Some(port), Some(old)) if port != old => {
            stop(state).await;
            start(state).await
        }
        _ => Ok(()),
    }
}

async fn start(state: &SharedState) -> anyhow::Result<()> {
    let port = state.config().portal.lan_port;
    let (ip, bind_ip) = if blirp_sync::loopback_only() {
        (IpAddr::V4(Ipv4Addr::LOCALHOST), Ipv4Addr::LOCALHOST)
    } else {
        (lan_ip(), Ipv4Addr::UNSPECIFIED)
    };
    let dir = state.paths.home().join("tls");
    let (cert, key) = tokio::task::spawn_blocking(move || load_or_create_cert(&dir, ip)).await??;
    let fp = fingerprint(&cert);
    let tls = tls_config(cert, key)?;
    let listener = crate::bind_exclusive(SocketAddr::from((bind_ip, port)))
        .with_context(|| format!("bind {bind_ip}:{port} for the LAN portal"))?;
    let server = axum_server::from_tcp_rustls(
        listener,
        axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls)),
    )?;
    let handle = axum_server::Handle::new();
    let app = crate::api::portal_router(state.clone())
        .into_make_service_with_connect_info::<SocketAddr>();
    let server = server.handle(handle.clone());
    let task = tokio::spawn(async move {
        if let Err(e) = server.serve(app).await {
            tracing::error!(error = %e, "LAN portal stopped");
        }
    });
    let url = format!("https://{ip}:{port}");
    tracing::info!(%url, fingerprint = %fp, "LAN portal listening");
    *lock(&state.sync.portal.running) = Some(Running {
        handle,
        port,
        url,
        fingerprint: fp,
        task,
    });
    crate::sync::emit_status(state);
    Ok(())
}

pub async fn stop(state: &SharedState) {
    let running = lock(&state.sync.portal.running).take();
    if let Some(r) = running {
        r.handle.graceful_shutdown(Some(Duration::from_secs(2)));
        if let Err(e) = r.task.await {
            tracing::error!(error = %e, "LAN portal task failed");
        }
        lock(&state.sync.portal.invites).clear();
        tracing::info!("LAN portal stopped");
        crate::sync::emit_status(state);
    }
}

/// (url, certificate fingerprint) while serving.
pub fn info(state: &SharedState) -> Option<(String, String)> {
    lock(&state.sync.portal.running)
        .as_ref()
        .map(|r| (r.url.clone(), r.fingerprint.clone()))
}

/// A one-time login link for another browser (5 minutes, single use).
pub fn create_invite(state: &SharedState) -> ApiResult<BrowserInvite> {
    let (url, _) = info(state).ok_or_else(|| {
        ApiError::conflict(
            "portal_disabled",
            "the LAN portal is off; enable the hub and set portal.lan = true",
        )
    })?;
    let token =
        blirp_core::random_hex::<16>().map_err(|e| ApiError::internal("random token", e))?;
    let now = blirp_core::now_ms();
    let expires_at = now + INVITE_TTL_MS;
    let mut invites = lock(&state.sync.portal.invites);
    invites.retain(|_, exp| *exp > now);
    invites.insert(sha256_hex(&token), expires_at);
    Ok(BrowserInvite {
        url: format!("{url}/device-login?invite={token}"),
        expires_at,
    })
}

/// Resolve a device cookie to a principal (None when unknown or revoked).
pub async fn authenticate(state: &SharedState, token: String) -> ApiResult<Option<Principal>> {
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let store = state.store.clone();
    crate::api::blocking(move || {
        let Some(d) = store.device_by_token_hash(&sha256_hex(&token))? else {
            return Ok(None);
        };
        if d.kind != DeviceKind::Browser {
            return Ok(None);
        }
        let now = blirp_core::now_ms();
        if now - d.last_seen > TOUCH_EVERY_MS {
            store.touch_device(&d.id, now)?;
        }
        Ok(Some(Principal {
            control: d.can_control_terminals,
            admin: false,
            label: format!("browser {}", d.name),
            device: Some(d.id),
        }))
    })
    .await
}

/// Short device name from the User-Agent, e.g. "Safari on iPhone".
fn device_name(headers: &HeaderMap) -> String {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let os = [
        ("iPhone", "iPhone"),
        ("iPad", "iPad"),
        ("Android", "Android"),
        ("Macintosh", "Mac"),
        ("Windows", "Windows"),
        ("CrOS", "ChromeOS"),
        ("Linux", "Linux"),
    ]
    .iter()
    .find(|(k, _)| ua.contains(k))
    .map_or("unknown device", |(_, v)| v);
    let browser = [
        ("Firefox/", "Firefox"),
        ("Edg/", "Edge"),
        ("Chrome/", "Chrome"),
        ("Safari/", "Safari"),
    ]
    .iter()
    .find(|(k, _)| ua.contains(k))
    .map_or("Browser", |(_, v)| v);
    format!("{browser} on {os}")
}

#[derive(Deserialize)]
pub struct LoginQuery {
    invite: String,
}

fn login_error(status: StatusCode, message: &str) -> Response {
    ApiError::new(status, "invalid_invite", message).into_response()
}

/// `GET /device-login?invite=` (portal only): redeem a one-time invite,
/// set the device cookie and go to the app.
pub async fn device_login(
    State(state): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    q: Result<Query<LoginQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let now = blirp_core::now_ms();
    {
        let mut attempts = lock(&state.sync.portal.attempts);
        attempts.retain(|_, (start, _)| now - *start < RATE_WINDOW_MS);
        let entry = attempts.entry(peer.ip()).or_insert((now, 0));
        entry.1 += 1;
        if entry.1 > RATE_MAX {
            return ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "too many login attempts; wait a minute",
            )
            .into_response();
        }
    }
    let Ok(Query(q)) = q else {
        return login_error(StatusCode::BAD_REQUEST, "missing invite");
    };
    // Single use: removed whether or not it is still valid.
    let valid = lock(&state.sync.portal.invites)
        .remove(&sha256_hex(&q.invite))
        .is_some_and(|exp| exp > now);
    if !valid {
        return login_error(
            StatusCode::UNAUTHORIZED,
            "this login link is invalid, expired or already used; create a new one",
        );
    }
    let token = match blirp_core::random_hex::<32>() {
        Ok(t) => t,
        Err(e) => return ApiError::internal("random token", e).into_response(),
    };
    let device = Device {
        id: blirp_core::new_id(),
        name: device_name(&headers),
        kind: DeviceKind::Browser,
        token_hash: Some(sha256_hex(&token)),
        node_id: None,
        created_at: now,
        last_seen: now,
        revoked: false,
        can_control_terminals: false,
    };
    let store = state.store.clone();
    let name = device.name.clone();
    if let Err(e) = crate::api::blocking(move || Ok(store.upsert_device(&device)?)).await {
        return e.into_response();
    }
    tracing::info!(device = %name, client = %peer.ip(), "browser device signed in on the LAN portal");
    let cookie = format!(
        "{DEVICE_COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={COOKIE_MAX_AGE}"
    );
    let mut resp = Redirect::to("/").into_response();
    match HeaderValue::from_str(&cookie) {
        Ok(v) => {
            resp.headers_mut().insert(header::SET_COOKIE, v);
            resp
        }
        Err(e) => ApiError::internal("building device cookie", e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_fingerprints() {
        let mut h = HeaderMap::new();
        h.insert(
            header::USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605 Version/17.0 Mobile Safari/604.1",
            ),
        );
        assert_eq!(device_name(&h), "Safari on iPhone");
        assert_eq!(device_name(&HeaderMap::new()), "Browser on unknown device");
        let fp = fingerprint(b"x");
        assert_eq!(fp.len(), 32 * 3 - 1);
        assert!(fp.starts_with("2D:71:16"));
    }

    #[test]
    fn certificate_follows_the_lan_address() {
        let dir = tempfile::tempdir().unwrap();
        let a: IpAddr = "192.0.2.20".parse().unwrap();
        let b: IpAddr = "198.51.100.7".parse().unwrap();
        let (first, _) = load_or_create_cert(dir.path(), a).unwrap();
        assert!(names_ip(&first, a) && !names_ip(&first, b));
        let (same, _) = load_or_create_cert(dir.path(), a).unwrap();
        assert_eq!(
            fingerprint(&same),
            fingerprint(&first),
            "kept while the address stays"
        );
        // Loopback only (no network): the existing certificate still serves.
        let (kept, _) = load_or_create_cert(dir.path(), IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        assert_eq!(fingerprint(&kept), fingerprint(&first));
        let (moved, _) = load_or_create_cert(dir.path(), b).unwrap();
        assert!(names_ip(&moved, b));
        assert_ne!(fingerprint(&moved), fingerprint(&first));

        // A key that does not belong to the certificate (a crash between the
        // two renames) is replaced too, not a permanent start failure.
        let other = rcgen::generate_simple_self_signed(vec!["x".to_string()]).unwrap();
        std::fs::write(
            dir.path().join("key.pem"),
            other.signing_key.serialize_pem(),
        )
        .unwrap();
        let (fixed, key) = load_or_create_cert(dir.path(), b).unwrap();
        assert_ne!(fingerprint(&fixed), fingerprint(&moved));
        tls_config(fixed, key).unwrap();
        assert!(!dir.path().join("key.pem.tmp").exists());
    }
}
