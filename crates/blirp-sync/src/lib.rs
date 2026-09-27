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

/// Versions of the replication protocol (`blirp/sync/1`). 2 added the hub's
/// `presence` frames on the notification stream. The version also names the
/// replicated schema (the rows and fields that changes carry): a peer that
/// does not know a field drops it from what it pulls and still moves its
/// cursor past the row, so it never gets it again (0.1.x nodes lost
/// `projects.chats` and `merged_into` that way). So a release that adds or
/// changes a replicated field bumps the version and speaks only the new one,
/// and older peers are refused until they update (§10). 3 = the schema of
/// migration 10 (`projects.chats`, `projects.merged_into`).
pub const SYNC_VERSIONS: &[u32] = &[3];

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
    fn sync_refuses_peers_with_another_replicated_schema() {
        // 0.1.0 speaks [1], 0.1.1 to 0.2.0 [1, 2]: none knows the fields of
        // migration 10, so none may pull or push rows here.
        for old in [&[1][..], &[1, 2]] {
            assert_eq!(super::negotiate_from(super::SYNC_VERSIONS, old), None);
        }
        assert_eq!(super::negotiate_from(super::SYNC_VERSIONS, &[3]), Some(3));
        assert_eq!(super::negotiate_from(super::SYNC_VERSIONS, &[4]), None);
    }

    /// Tripwire for the rule on [`super::SYNC_VERSIONS`]: the columns of
    /// every replicated table, pinned to the sync version. A migration that
    /// changes them fails this test: bump the version (speaking only the new
    /// one), then update both here together.
    #[test]
    fn replicated_schema_is_pinned_to_the_sync_version() {
        const TABLES: &[&str] = &[
            "machines",
            "projects",
            "project_paths",
            "sessions",
            "events",
            "records",
            "briefs",
            "brief_history",
            "wiki_pages",
            "resources",
            "deleted_sessions",
            "deleted_records",
        ];
        const PINNED: &str = "\
            machines: id name os role last_seen revoked\n\
            projects: id name created_at updated_at deleted chats merged_into\n\
            project_paths: project_id machine_id path git_remote\n\
            sessions: id project_id machine_id agent agent_session_id origin cwd title status \
            branch worktree transcript_path started_at ended_at last_activity_at exit_code \
            summary_json distilled_through_seq tokens_in tokens_out cost_usd parent_session_id \
            stopped_by_user\n\
            events: session_id seq ts kind text meta_json\n\
            records: id project_id kind title body status pinned source_session_id created_at \
            updated_at updated_by\n\
            briefs: project_id body_md version updated_at updated_by history_id machine_id\n\
            brief_history: id project_id body_md updated_at updated_by machine_id\n\
            wiki_pages: id project_id slug title body_md updated_at updated_by deleted\n\
            resources: id project_id kind url title meta_json created_at deleted updated_at\n\
            deleted_sessions: id deleted_at\n\
            deleted_records: id deleted_at\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blirp.db");
        drop(blirp_core::store::Store::open(&path).unwrap());
        let conn = rusqlite::Connection::open(&path).unwrap();
        let mut schema = String::new();
        for table in TABLES {
            let mut st = conn
                .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
                .unwrap();
            let cols: Vec<String> = st
                .query_map([table], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            schema.push_str(&format!("{table}: {}\n", cols.join(" ")));
        }
        assert_eq!(
            (super::SYNC_VERSIONS, schema.as_str()),
            (&[3][..], PINNED),
            "the replicated schema changed: bump SYNC_VERSIONS (see its doc)"
        );
    }
}
