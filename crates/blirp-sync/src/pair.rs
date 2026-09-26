//! Pairing (§10, `blirp/pair/1`).
//!
//! The hub shows an invite (`blirp1-<base32 ticket>`: its endpoint address
//! plus a random invite id) and an 8-character code (40 bits from an
//! alphabet without 0/O/1/I), valid 10 minutes, single use, at most 5
//! attempts. Over the QUIC connection (already authenticated to both
//! endpoint ids by TLS) the node and hub run symmetric SPAKE2 on Ed25519
//! with the code as password; neither side learns anything about the code
//! from a failed run, so an attacker gets one online guess per attempt.
//! Both sides then prove knowledge of the session key with HMAC-SHA256
//! confirmation tags over a transcript that binds the invite id, both
//! endpoint ids and both SPAKE2 messages (a relay or man in the middle
//! changes the ids and fails confirmation). Machine metadata is exchanged
//! only after confirmation and is MACed with the same key. Tags are checked
//! in constant time.
//!
//! Messages (JSON frames, see [`crate::wire`]):
//! node `hello {versions, invite_id?, spake}` -> hub `challenge {version, spake}`
//! -> node `confirm {mac}` -> hub `confirm {mac}` -> node `meta {name, os, mac}`
//! -> hub `meta {name, os, mac}` -> hub `welcome` (node stored). Either side
//! may answer `error {code, message}` instead and close.

use crate::wire::{MAX_CONTROL_FRAME, WireError, read_frame, write_frame};
use hmac::{KeyInit, Mac};
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Mutex, PoisonError};
use tokio::io::{AsyncRead, AsyncWrite};

type HmacSha256 = hmac::Hmac<Sha256>;

/// 32 symbols, no 0/O/1/I; `byte & 31` indexes it uniformly.
pub const CODE_ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
pub const CODE_LEN: usize = 8;
pub const CODE_TTL_MS: i64 = 10 * 60 * 1000;
pub const MAX_ATTEMPTS: u32 = 5;
const INVITE_PREFIX: &str = "blirp1-";
const JOIN_URI_PREFIX: &str = "blirp://join/";
const SPAKE_IDENTITY: &[u8] = b"blirp/pair/1";
const MAX_NAME: usize = 100;
const MAX_OS: usize = 32;
/// Expired invites are remembered this long so late joiners hear "expired".
const EXPIRED_GRACE_MS: i64 = 60 * 60 * 1000;

#[derive(Debug, thiserror::Error)]
pub enum PairError {
    #[error("the code must be {CODE_LEN} characters from {}", std::str::from_utf8(CODE_ALPHABET).unwrap_or(""))]
    InvalidCode,
    #[error("invalid invite: {0}")]
    InvalidInvite(String),
    #[error("the hub does not know this invite; create a new one")]
    UnknownInvite,
    #[error("the hub has several open invites; pass the invite too")]
    InviteRequired,
    #[error("the invite expired; create a new one on the hub")]
    Expired,
    #[error("too many wrong codes; the invite was invalidated, create a new one")]
    TooManyAttempts,
    #[error("wrong code")]
    WrongCode,
    #[error("the other side does not speak a compatible pairing protocol")]
    UnsupportedVersion,
    #[error("no hub found on the local network; pass the invite")]
    NoHubFound,
    #[error("several hubs found on the local network; pass the invite")]
    MultipleHubs,
    #[error("hub refused pairing: {0}")]
    Refused(String),
    #[error("pairing protocol error: {0}")]
    Protocol(String),
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error("cannot generate random values: {0}")]
    Random(String),
}

impl PairError {
    /// Stable code sent to the peer.
    fn code(&self) -> &'static str {
        match self {
            Self::UnknownInvite => "unknown_invite",
            Self::InviteRequired => "invite_required",
            Self::Expired => "expired",
            Self::TooManyAttempts => "too_many_attempts",
            Self::WrongCode => "wrong_code",
            Self::UnsupportedVersion => "unsupported_version",
            Self::Refused(_) => "refused",
            _ => "protocol",
        }
    }

    fn from_remote(code: &str, message: String) -> Self {
        match code {
            "unknown_invite" => Self::UnknownInvite,
            "invite_required" => Self::InviteRequired,
            "expired" => Self::Expired,
            "too_many_attempts" => Self::TooManyAttempts,
            "wrong_code" => Self::WrongCode,
            "unsupported_version" => Self::UnsupportedVersion,
            "refused" => Self::Refused(message),
            _ => Self::Protocol(message),
        }
    }
}

fn random<const N: usize>() -> Result<[u8; N], PairError> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| PairError::Random(e.to_string()))?;
    Ok(b)
}

/// A fresh normalized code (8 symbols, no separator).
pub fn generate_code() -> Result<String, PairError> {
    Ok(random::<CODE_LEN>()?
        .iter()
        .map(|b| char::from(CODE_ALPHABET[usize::from(b & 31)]))
        .collect())
}

/// `ABCDEFGH` -> `ABCD-EFGH`.
pub fn format_code(code: &str) -> String {
    let (a, b) = code.split_at(code.len().min(CODE_LEN / 2));
    format!("{a}-{b}")
}

/// Accept `abcd-efgh`, `ABCD EFGH`, ... and return the 8 bare symbols.
pub fn normalize_code(input: &str) -> Result<String, PairError> {
    let code: String = input
        .chars()
        .filter(|c| !matches!(c, '-' | ' ' | '\t'))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let ok = code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b));
    if ok {
        Ok(code)
    } else {
        Err(PairError::InvalidCode)
    }
}

/// What an invite string carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ticket {
    pub addr: EndpointAddr,
    /// 128-bit random id (hex) naming the invite on the hub.
    pub invite_id: String,
}

impl Ticket {
    pub fn encode(&self) -> Result<String, PairError> {
        let json = serde_json::to_vec(self).map_err(|e| PairError::Protocol(e.to_string()))?;
        Ok(format!(
            "{INVITE_PREFIX}{}",
            data_encoding::BASE32_NOPAD
                .encode(&json)
                .to_ascii_lowercase()
        ))
    }

    pub fn decode(s: &str) -> Result<Self, PairError> {
        let body = s
            .trim()
            .strip_prefix(INVITE_PREFIX)
            .ok_or_else(|| PairError::InvalidInvite(format!("must start with {INVITE_PREFIX}")))?;
        if body.len() > 8192 {
            return Err(PairError::InvalidInvite("too long".into()));
        }
        let json = data_encoding::BASE32_NOPAD
            .decode(body.to_ascii_uppercase().as_bytes())
            .map_err(|_| PairError::InvalidInvite("not base32".into()))?;
        let t: Ticket =
            serde_json::from_slice(&json).map_err(|e| PairError::InvalidInvite(e.to_string()))?;
        if t.invite_id.len() != 32 || !t.invite_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(PairError::InvalidInvite("bad invite id".into()));
        }
        Ok(t)
    }
}

/// `blirp://join/<invite>#<code>` (the QR payload).
pub fn join_uri(invite: &str, code: &str) -> String {
    format!("{JOIN_URI_PREFIX}{invite}#{}", format_code(code))
}

/// Split a join URI into (invite, code).
pub fn parse_join_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.trim().strip_prefix(JOIN_URI_PREFIX)?;
    let (invite, code) = rest.split_once('#')?;
    Some((invite.to_string(), code.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineMeta {
    pub name: String,
    pub os: String,
}

impl MachineMeta {
    fn validate(&self) -> Result<(), PairError> {
        let ok = !self.name.trim().is_empty()
            && self.name.chars().count() <= MAX_NAME
            && !self.os.is_empty()
            && self.os.len() <= MAX_OS
            && !self.name.chars().any(char::is_control);
        if ok {
            Ok(())
        } else {
            Err(PairError::Protocol("invalid machine metadata".into()))
        }
    }
}

// ---------------------------------------------------------------- invites

struct Pending {
    code: String,
    expires_at: i64,
    attempts: u32,
}

/// Open invites of a hub (in memory: a daemon restart invalidates them).
#[derive(Default)]
pub struct InviteBook {
    inner: Mutex<HashMap<String, Pending>>,
}

impl std::fmt::Debug for InviteBook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InviteBook").finish_non_exhaustive()
    }
}

/// A newly created invite.
#[derive(Debug, Clone)]
pub struct NewInvite {
    pub invite_id: String,
    pub code: String,
    pub expires_at: i64,
}

impl InviteBook {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        // Plain data; a panic elsewhere cannot leave it half-updated.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn create(&self, now: i64) -> Result<NewInvite, PairError> {
        let invite_id = data_encoding::HEXLOWER.encode(&random::<16>()?);
        let code = generate_code()?;
        let expires_at = now + CODE_TTL_MS;
        let mut map = self.lock();
        map.retain(|_, p| p.expires_at + EXPIRED_GRACE_MS > now);
        map.insert(
            invite_id.clone(),
            Pending {
                code: code.clone(),
                expires_at,
                attempts: 0,
            },
        );
        Ok(NewInvite {
            invite_id,
            code,
            expires_at,
        })
    }

    /// Start an attempt; counts toward the limit before any secret is used,
    /// so parallel attempts cannot exceed it. `None` picks the only live
    /// invite (LAN join with just the code).
    fn begin(&self, id: Option<&str>, now: i64) -> Result<(String, String), PairError> {
        let mut map = self.lock();
        let id = match id {
            Some(id) => id.to_string(),
            None => {
                let mut live = map.iter().filter(|(_, p)| p.expires_at > now);
                match (live.next(), live.next()) {
                    (Some((id, _)), None) => id.clone(),
                    (None, _) => return Err(PairError::UnknownInvite),
                    (Some(_), Some(_)) => return Err(PairError::InviteRequired),
                }
            }
        };
        let p = map.get_mut(&id).ok_or(PairError::UnknownInvite)?;
        if p.expires_at <= now {
            return Err(PairError::Expired);
        }
        if p.attempts >= MAX_ATTEMPTS {
            map.remove(&id);
            return Err(PairError::TooManyAttempts);
        }
        p.attempts += 1;
        Ok((id, p.code.clone()))
    }

    fn consume(&self, id: &str) {
        self.lock().remove(id);
    }

    fn failed(&self, id: &str) {
        let mut map = self.lock();
        if map.get(id).is_some_and(|p| p.attempts >= MAX_ATTEMPTS) {
            map.remove(id);
        }
    }

    /// Live (unexpired, unused) invites.
    pub fn live(&self, now: i64) -> usize {
        self.lock().values().filter(|p| p.expires_at > now).count()
    }
}

// ---------------------------------------------------------------- handshake

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Msg {
    Hello {
        versions: Vec<u32>,
        invite_id: Option<String>,
        spake: String,
    },
    Challenge {
        version: u32,
        spake: String,
    },
    Confirm {
        mac: String,
    },
    Meta {
        name: String,
        os: String,
        mac: String,
    },
    Welcome,
    Error {
        code: String,
        message: String,
    },
}

/// Endpoint ids of both sides as authenticated by the QUIC handshake.
#[derive(Debug, Clone, Copy)]
pub struct PairIds {
    pub node: [u8; 32],
    pub hub: [u8; 32],
}

struct Keyed {
    key: Vec<u8>,
    transcript: [u8; 32],
}

impl Keyed {
    fn new(key: Vec<u8>, invite_id: &str, ids: PairIds, msg_node: &[u8], msg_hub: &[u8]) -> Self {
        let mut h = Sha256::new();
        for part in [
            SPAKE_IDENTITY,
            invite_id.as_bytes(),
            &ids.node,
            &ids.hub,
            msg_node,
            msg_hub,
        ] {
            h.update((part.len() as u64).to_be_bytes());
            h.update(part);
        }
        Self {
            key,
            transcript: h.finalize().into(),
        }
    }

    fn mac(&self, label: &str, data: &[&[u8]]) -> HmacSha256 {
        // HMAC accepts keys of any length; this cannot fail.
        let mut m = <HmacSha256 as KeyInit>::new_from_slice(&self.key)
            .unwrap_or_else(|_| unreachable!("HMAC takes any key length"));
        m.update(label.as_bytes());
        m.update(&[0]);
        m.update(&self.transcript);
        for d in data {
            m.update(&(d.len() as u64).to_be_bytes());
            m.update(d);
        }
        m
    }

    fn tag(&self, label: &str, data: &[&[u8]]) -> String {
        data_encoding::HEXLOWER.encode(&self.mac(label, data).finalize().into_bytes())
    }

    /// Constant-time tag check.
    fn verify(&self, label: &str, data: &[&[u8]], tag_hex: &str) -> bool {
        match data_encoding::HEXLOWER_PERMISSIVE.decode(tag_hex.as_bytes()) {
            Ok(tag) => self.mac(label, data).verify_slice(&tag).is_ok(),
            Err(_) => false,
        }
    }
}

fn unhex(s: &str) -> Result<Vec<u8>, PairError> {
    data_encoding::HEXLOWER_PERMISSIVE
        .decode(s.as_bytes())
        .map_err(|_| PairError::Protocol("bad hex".into()))
}

fn meta_parts(m: &MachineMeta) -> [&[u8]; 2] {
    [m.name.as_bytes(), m.os.as_bytes()]
}

async fn recv<R: AsyncRead + Unpin>(r: &mut R) -> Result<Msg, PairError> {
    match read_frame(r, MAX_CONTROL_FRAME).await? {
        Msg::Error { code, message } => Err(PairError::from_remote(&code, message)),
        m => Ok(m),
    }
}

fn unexpected(m: &Msg) -> PairError {
    PairError::Protocol(format!("unexpected message {m:?}"))
}

/// Node side. Returns the hub's metadata once the hub stored this node.
pub async fn node_pair<R, W>(
    r: &mut R,
    w: &mut W,
    ids: PairIds,
    invite_id: Option<&str>,
    code: &str,
    me: &MachineMeta,
) -> Result<MachineMeta, PairError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let code = normalize_code(code)?;
    let (spake, msg_node) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(code.as_bytes()),
        &Identity::new(SPAKE_IDENTITY),
    );
    write_frame(
        w,
        &Msg::Hello {
            versions: crate::PROTOCOL_VERSIONS.to_vec(),
            invite_id: invite_id.map(str::to_string),
            spake: data_encoding::HEXLOWER.encode(&msg_node),
        },
    )
    .await?;
    let (msg_hub, invite_id) = match recv(r).await? {
        Msg::Challenge { version, spake } if crate::PROTOCOL_VERSIONS.contains(&version) => {
            (unhex(&spake)?, invite_id.unwrap_or_default().to_string())
        }
        Msg::Challenge { .. } => return Err(PairError::UnsupportedVersion),
        m => return Err(unexpected(&m)),
    };
    let key = spake
        .finish(&msg_hub)
        .map_err(|e| PairError::Protocol(format!("SPAKE2: {e:?}")))?;
    // Code-only joins do not know the invite id; the hub binds "" too.
    let k = Keyed::new(key, &invite_id, ids, &msg_node, &msg_hub);
    write_frame(
        w,
        &Msg::Confirm {
            mac: k.tag("confirm node", &[]),
        },
    )
    .await?;
    match recv(r).await? {
        Msg::Confirm { mac } if k.verify("confirm hub", &[], &mac) => {}
        // Only a hub that knows the code can produce this tag.
        Msg::Confirm { .. } => return Err(PairError::WrongCode),
        m => return Err(unexpected(&m)),
    }
    write_frame(
        w,
        &Msg::Meta {
            name: me.name.clone(),
            os: me.os.clone(),
            mac: k.tag("meta node", &meta_parts(me)),
        },
    )
    .await?;
    let hub_meta = match recv(r).await? {
        Msg::Meta { name, os, mac } => {
            let m = MachineMeta { name, os };
            if !k.verify("meta hub", &meta_parts(&m), &mac) {
                return Err(PairError::Protocol(
                    "hub metadata failed authentication".into(),
                ));
            }
            m.validate()?;
            m
        }
        m => return Err(unexpected(&m)),
    };
    match recv(r).await? {
        Msg::Welcome => Ok(hub_meta),
        m => Err(unexpected(&m)),
    }
}

/// Hub side. `register` stores the node (called only after the node proved
/// the code and its metadata authenticated); `Welcome` follows its success.
pub async fn hub_pair<R, W, F, Fut>(
    r: &mut R,
    w: &mut W,
    ids: PairIds,
    book: &InviteBook,
    me: &MachineMeta,
    now: i64,
    register: F,
) -> Result<MachineMeta, PairError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: FnOnce(MachineMeta) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let result = hub_pair_inner(r, w, ids, book, me, now, register).await;
    if let Err(e) = &result
        && !matches!(e, PairError::Wire(_))
    {
        // Best effort: tell the node why; it may already be gone.
        let _ = write_frame(
            w,
            &Msg::Error {
                code: e.code().to_string(),
                message: e.to_string(),
            },
        )
        .await;
    }
    result
}

async fn hub_pair_inner<R, W, F, Fut>(
    r: &mut R,
    w: &mut W,
    ids: PairIds,
    book: &InviteBook,
    me: &MachineMeta,
    now: i64,
    register: F,
) -> Result<MachineMeta, PairError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: FnOnce(MachineMeta) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let (versions, invite_id, msg_node) = match recv(r).await? {
        Msg::Hello {
            versions,
            invite_id,
            spake,
        } => (versions, invite_id, unhex(&spake)?),
        m => return Err(unexpected(&m)),
    };
    let version = crate::negotiate(&versions).ok_or(PairError::UnsupportedVersion)?;
    let bound_id = invite_id.clone().unwrap_or_default();
    let (id, code) = book.begin(invite_id.as_deref(), now)?;
    let (spake, msg_hub) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(code.as_bytes()),
        &Identity::new(SPAKE_IDENTITY),
    );
    write_frame(
        w,
        &Msg::Challenge {
            version,
            spake: data_encoding::HEXLOWER.encode(&msg_hub),
        },
    )
    .await?;
    let key = spake
        .finish(&msg_node)
        .map_err(|e| PairError::Protocol(format!("SPAKE2: {e:?}")))?;
    let k = Keyed::new(key, &bound_id, ids, &msg_node, &msg_hub);
    match recv(r).await? {
        Msg::Confirm { mac } if k.verify("confirm node", &[], &mac) => book.consume(&id),
        Msg::Confirm { .. } => {
            book.failed(&id);
            return Err(PairError::WrongCode);
        }
        m => return Err(unexpected(&m)),
    }
    write_frame(
        w,
        &Msg::Confirm {
            mac: k.tag("confirm hub", &[]),
        },
    )
    .await?;
    let node_meta = match recv(r).await? {
        Msg::Meta { name, os, mac } => {
            let m = MachineMeta { name, os };
            if !k.verify("meta node", &meta_parts(&m), &mac) {
                return Err(PairError::Protocol(
                    "node metadata failed authentication".into(),
                ));
            }
            m.validate()?;
            m
        }
        m => return Err(unexpected(&m)),
    };
    write_frame(
        w,
        &Msg::Meta {
            name: me.name.clone(),
            os: me.os.clone(),
            mac: k.tag("meta hub", &meta_parts(me)),
        },
    )
    .await?;
    register(node_meta.clone())
        .await
        .map_err(PairError::Refused)?;
    write_frame(w, &Msg::Welcome).await?;
    Ok(node_meta)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use iroh::SecretKey;

    fn ids() -> PairIds {
        PairIds {
            node: *SecretKey::from_bytes(&[1; 32]).public().as_bytes(),
            hub: *SecretKey::from_bytes(&[2; 32]).public().as_bytes(),
        }
    }

    fn meta(name: &str) -> MachineMeta {
        MachineMeta {
            name: name.into(),
            os: "linux".into(),
        }
    }

    async fn run(
        book: &InviteBook,
        invite: Option<&str>,
        code: &str,
        now: i64,
        node_ids: PairIds,
    ) -> (
        Result<MachineMeta, PairError>,
        Result<MachineMeta, PairError>,
    ) {
        let (node_io, hub_io) = tokio::io::duplex(1 << 16);
        let (mut nr, mut nw) = tokio::io::split(node_io);
        let (mut hr, mut hw) = tokio::io::split(hub_io);
        let registered = std::sync::atomic::AtomicBool::new(false);
        let (laptop, mini) = (meta("laptop"), meta("mini"));
        let node = node_pair(&mut nr, &mut nw, node_ids, invite, code, &laptop);
        let hub = hub_pair(&mut hr, &mut hw, ids(), book, &mini, now, |m| {
            assert_eq!(m.name, "laptop");
            registered.store(true, std::sync::atomic::Ordering::SeqCst);
            async { Ok(()) }
        });
        let (n, h) = tokio::join!(node, hub);
        assert_eq!(
            registered.load(std::sync::atomic::Ordering::SeqCst),
            h.is_ok()
        );
        (n, h)
    }

    #[test]
    fn codes() {
        for _ in 0..50 {
            let c = generate_code().unwrap();
            assert_eq!(normalize_code(&format_code(&c).to_lowercase()).unwrap(), c);
            assert!(!c.contains(['0', 'O', '1', 'I']));
        }
        assert!(normalize_code("ABCD-EFG0").is_err());
        assert!(normalize_code("ABCD").is_err());
        assert_eq!(format_code("ABCDEFGH"), "ABCD-EFGH");
    }

    #[test]
    fn ticket_roundtrip() {
        let t = Ticket {
            addr: EndpointAddr::new(SecretKey::from_bytes(&[3; 32]).public())
                .with_ip_addr("127.0.0.1:4242".parse().unwrap()),
            invite_id: "ab".repeat(16),
        };
        let s = t.encode().unwrap();
        assert!(s.starts_with("blirp1-"));
        assert_eq!(Ticket::decode(&s).unwrap(), t);
        assert!(Ticket::decode("blirp1-!!!").is_err());
        assert!(Ticket::decode("nope").is_err());
        let uri = join_uri(&s, "ABCDEFGH");
        assert_eq!(parse_join_uri(&uri).unwrap(), (s, "ABCD-EFGH".into()));
    }

    #[tokio::test]
    async fn success_is_single_use() {
        let book = InviteBook::default();
        let inv = book.create(0).unwrap();
        let (n, h) = run(&book, Some(&inv.invite_id), &inv.code, 1, ids()).await;
        assert_eq!(n.unwrap().name, "mini");
        assert_eq!(h.unwrap().name, "laptop");
        let (n, _) = run(&book, Some(&inv.invite_id), &inv.code, 2, ids()).await;
        assert!(matches!(n, Err(PairError::UnknownInvite)), "{n:?}");
    }

    #[tokio::test]
    async fn code_only_uses_the_single_live_invite() {
        let book = InviteBook::default();
        let inv = book.create(0).unwrap();
        let (n, _) = run(&book, None, &format_code(&inv.code), 1, ids()).await;
        assert!(n.is_ok(), "{n:?}");
        book.create(0).unwrap();
        book.create(0).unwrap();
        let (n, _) = run(&book, None, "ABCD-EFGH", 1, ids()).await;
        assert!(matches!(n, Err(PairError::InviteRequired)), "{n:?}");
    }

    #[tokio::test]
    async fn wrong_code_then_attempt_limit() {
        let book = InviteBook::default();
        let inv = book.create(0).unwrap();
        let wrong = if inv.code == "AAAAAAAA" {
            "BBBBBBBB"
        } else {
            "AAAAAAAA"
        };
        for _ in 0..MAX_ATTEMPTS {
            let (n, h) = run(&book, Some(&inv.invite_id), wrong, 1, ids()).await;
            assert!(matches!(n, Err(PairError::WrongCode)), "{n:?}");
            assert!(matches!(h, Err(PairError::WrongCode)), "{h:?}");
        }
        // Invalidated: even the right code no longer works.
        let (n, _) = run(&book, Some(&inv.invite_id), &inv.code, 1, ids()).await;
        assert!(
            matches!(
                n,
                Err(PairError::UnknownInvite | PairError::TooManyAttempts)
            ),
            "{n:?}"
        );
    }

    #[tokio::test]
    async fn expired_invite_is_refused() {
        let book = InviteBook::default();
        let inv = book.create(0).unwrap();
        let (n, h) = run(
            &book,
            Some(&inv.invite_id),
            &inv.code,
            CODE_TTL_MS + 1,
            ids(),
        )
        .await;
        assert!(matches!(n, Err(PairError::Expired)), "{n:?}");
        assert!(matches!(h, Err(PairError::Expired)), "{h:?}");
    }

    #[tokio::test]
    async fn mismatched_endpoint_ids_fail_confirmation() {
        // A relay in the middle sees different ids on each leg.
        let book = InviteBook::default();
        let inv = book.create(0).unwrap();
        let mut other = ids();
        other.hub = *SecretKey::from_bytes(&[9; 32]).public().as_bytes();
        let (n, h) = run(&book, Some(&inv.invite_id), &inv.code, 1, other).await;
        assert!(n.is_err() && h.is_err(), "{n:?} {h:?}");
    }
}
