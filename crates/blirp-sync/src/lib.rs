//! blirp machine sync (§10): iroh endpoint and identity, SPAKE2 pairing,
//! outbox/hub_log replication and the remote API/terminal proxy transport.
//!
//! Wire format for every protocol: frames of a 4-byte big-endian length
//! followed by that many bytes of JSON ([`wire`]). JSON because replicated
//! payloads are already JSON (`Change`) and stay debuggable; every frame is
//! size-checked before it is read.

pub mod files;
pub mod identity;
pub mod pair;
pub mod proxy;
pub mod repl;
pub mod service;
pub mod wire;

pub use service::{Role, RuntimeStatus, StartOptions, SyncService};

/// ALPN of the pairing handshake.
pub const ALPN_PAIR: &[u8] = b"blirp/pair/1";
/// ALPN of outbox/hub_log replication.
pub const ALPN_SYNC: &[u8] = b"blirp/sync/1";
/// ALPN of the API/terminal proxy.
pub const ALPN_PROXY: &[u8] = b"blirp/proxy/1";
/// ALPN of project file sync (hub only).
pub const ALPN_FILES: &[u8] = b"blirp/files/1";

/// Versions of the in-band message formats this build speaks. Each protocol
/// opens with a hello carrying the versions of the sender; the receiver
/// picks the highest common one or answers `unsupported_version`.
pub const PROTOCOL_VERSIONS: &[u32] = &[1];

/// Versions of the replication protocol (`blirp/sync/1`). 2 adds the hub's
/// `presence` frames on the notification stream; a version 1 peer never
/// receives them.
pub const SYNC_VERSIONS: &[u32] = &[1, 2];

/// mDNS service name blirp endpoints advertise on the local network.
pub const MDNS_SERVICE: &str = "blirp";
/// mDNS user data marking an endpoint as a blirp hub.
pub const HUB_MARKER: &str = "blirp-hub";

/// Development and test switch, not user configuration (see
/// docs/development.md): `1` keeps every listener on 127.0.0.1 and turns off
/// relays, mDNS and port mapping, so test runs never touch the network or
/// raise firewall prompts. Cargo sets it for `cargo test` / `cargo run`.
pub const LOOPBACK_ONLY_ENV: &str = "BLIRP_LOOPBACK_ONLY";

/// `BLIRP_LOOPBACK_ONLY=1` is set.
pub fn loopback_only() -> bool {
    std::env::var_os(LOOPBACK_ONLY_ENV).is_some_and(|v| v == "1")
}

/// Highest protocol version both sides support.
pub fn negotiate(theirs: &[u32]) -> Option<u32> {
    negotiate_from(PROTOCOL_VERSIONS, theirs)
}

/// Highest of `ours` that `theirs` also lists.
pub fn negotiate_from(ours: &[u32], theirs: &[u32]) -> Option<u32> {
    ours.iter().rev().find(|v| theirs.contains(v)).copied()
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Wire(#[from] wire::WireError),
    #[error(transparent)]
    Pair(#[from] pair::PairError),
    #[error(transparent)]
    Identity(#[from] identity::IdentityError),
    #[error("database: {0}")]
    Store(#[from] blirp_core::store::StoreError),
    #[error("cannot start the network endpoint: {0}")]
    Bind(String),
    #[error("cannot reach {what}: {message}")]
    Connect { what: String, message: String },
    #[error("connection lost: {0}")]
    Connection(String),
    #[error("peer error {code}: {message}")]
    Remote { code: String, message: String },
    #[error("protocol violation: {0}")]
    Protocol(String),
    #[error("{0}")]
    Unavailable(String),
}

impl SyncError {
    pub(crate) fn connection(e: impl std::fmt::Display) -> Self {
        Self::Connection(e.to_string())
    }
}

pub type Result<T, E = SyncError> = std::result::Result<T, E>;

/// Run blocking store work off the async runtime.
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| SyncError::Unavailable(format!("background task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    #[test]
    fn sync_version_falls_back_for_older_peers() {
        // A 0.1.0 peer only speaks 1 (no presence frames); both new speak 2.
        assert_eq!(super::negotiate_from(super::SYNC_VERSIONS, &[1]), Some(1));
        assert_eq!(
            super::negotiate_from(super::SYNC_VERSIONS, &[1, 2]),
            Some(2)
        );
        assert_eq!(super::negotiate_from(&[1], &[1, 2]), Some(1));
        assert_eq!(super::negotiate_from(super::SYNC_VERSIONS, &[9]), None);
    }
}
