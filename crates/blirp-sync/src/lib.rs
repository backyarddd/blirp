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

/// With no common sync version: whether the peer speaking `theirs` runs the
/// older release (so it is the one to update).
pub fn sync_peer_older(theirs: &[u32]) -> bool {
    theirs.iter().max() < SYNC_VERSIONS.iter().max()
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
    /// Hub and node speak no common sync version: they replicate different
    /// fields (§10). `peer_older`: the other machine runs the older release.
    #[error(
        "this machine and the other one run blirp releases that replicate different data: \
         update blirp on {} to the same release",
        if *peer_older { "the other machine" } else { "this machine" }
    )]
    ReleaseMismatch { peer_older: bool },
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
    use blirp_core::store::Change;

    #[test]
    fn sync_refuses_peers_with_another_replicated_schema() {
        // 0.1.0 speaks [1], 0.1.1 to 0.2.0 [1, 2]: none knows the fields of
        // migration 10, so none may pull or push rows here.
        for old in [&[1][..], &[1, 2]] {
            assert_eq!(super::negotiate_from(super::SYNC_VERSIONS, old), None);
        }
        assert_eq!(super::negotiate_from(super::SYNC_VERSIONS, &[3]), Some(3));
        assert_eq!(super::negotiate_from(super::SYNC_VERSIONS, &[4]), None);
        // Which side to update: an old hub sends no versions at all.
        assert!(super::sync_peer_older(&[1, 2]));
        assert!(super::sync_peer_older(&[]));
        assert!(!super::sync_peer_older(&[4]));
    }

    /// Position of `c`'s variant. No wildcard: a new `Change` variant does
    /// not compile until it is listed here and gets a sample in
    /// [`samples`].
    fn variant(c: &Change) -> usize {
        match c {
            Change::Machine(_) => 0,
            Change::DeleteMachine { .. } => 1,
            Change::Project(_) => 2,
            Change::ProjectPath(_) => 3,
            Change::DeleteProjectPath { .. } => 4,
            Change::Session(_) => 5,
            Change::DeleteSession { .. } => 6,
            Change::Event(_) => 7,
            Change::Record(_) => 8,
            Change::DeleteRecord { .. } => 9,
            Change::Brief(_) => 10,
            Change::WikiPage(_) => 11,
            Change::Resource(_) => 12,
        }
    }
    const VARIANTS: usize = 13;

    /// One change of every variant, every field set.
    fn samples() -> Vec<Change> {
        use blirp_core::model::*;
        let s = String::new;
        vec![
            Change::Machine(Machine {
                id: s(),
                name: s(),
                os: s(),
                role: MachineRole::Node,
                last_seen: 0,
                revoked: false,
            }),
            Change::DeleteMachine { id: s() },
            Change::Project(Project {
                id: s(),
                name: s(),
                created_at: 0,
                updated_at: 0,
                deleted: false,
                chats: false,
                merged_into: None,
            }),
            Change::ProjectPath(ProjectPath {
                project_id: s(),
                machine_id: s(),
                path: s(),
                git_remote: None,
            }),
            Change::DeleteProjectPath {
                machine_id: s(),
                path: s(),
            },
            Change::Session(Session {
                id: s(),
                project_id: s(),
                machine_id: s(),
                agent: s(),
                agent_session_id: None,
                origin: SessionOrigin::Blirp,
                cwd: s(),
                title: None,
                status: SessionStatus::Idle,
                branch: None,
                worktree: None,
                transcript_path: None,
                started_at: 0,
                ended_at: None,
                last_activity_at: 0,
                exit_code: None,
                summary: None,
                distilled_through_seq: 0,
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                parent_session_id: None,
                stopped_by_user: false,
            }),
            Change::DeleteSession { id: s() },
            Change::Event(Event {
                session_id: s(),
                seq: 0,
                ts: 0,
                kind: EventKind::User,
                text: s(),
                meta: None,
            }),
            Change::Record(Record {
                id: s(),
                project_id: s(),
                kind: RecordKind::Note,
                title: s(),
                body: s(),
                status: RecordStatus::Active,
                pinned: false,
                source_session_id: None,
                created_at: 0,
                updated_at: 0,
                updated_by: s(),
            }),
            Change::DeleteRecord { id: s() },
            Change::Brief(Brief {
                id: s(),
                project_id: s(),
                body_md: s(),
                version: 0,
                updated_at: 0,
                updated_by: s(),
                machine_id: s(),
            }),
            Change::WikiPage(WikiPage {
                id: s(),
                project_id: s(),
                slug: s(),
                title: s(),
                body_md: s(),
                updated_at: 0,
                updated_by: s(),
                deleted: false,
            }),
            Change::Resource(Resource {
                id: s(),
                project_id: s(),
                kind: ResourceKind::Link,
                url: s(),
                title: s(),
                meta: None,
                created_at: 0,
                updated_at: 0,
                deleted: false,
            }),
        ]
    }

    /// Tripwire for the rule on [`super::SYNC_VERSIONS`]: what replication
    /// carries, pinned to the sync version. Per `Change` variant the fields
    /// of its payload as serialized, and per table a change writes
    /// (`Change::describe`) its columns. Adding a variant, a field (even one
    /// with a serde default) or a column of a replicated table fails this
    /// test: bump the version (speaking only the new one), then update the
    /// pinned text and the version here together.
    #[test]
    fn replicated_schema_is_pinned_to_the_sync_version() {
        const PINNED: &str = "\
machine (machines upsert): id name os role last_seen revoked
delete_machine (machines delete): id
project (projects upsert): id name created_at updated_at deleted chats merged_into
project_path (project_paths upsert): project_id machine_id path git_remote
delete_project_path (project_paths delete): machine_id path
session (sessions upsert): id project_id machine_id agent agent_session_id origin cwd title \
status branch worktree transcript_path started_at ended_at last_activity_at exit_code summary \
distilled_through_seq tokens_in tokens_out cost_usd parent_session_id stopped_by_user
delete_session (sessions delete): id
event (events insert): session_id seq ts kind text meta
record (records upsert): id project_id kind title body status pinned source_session_id \
created_at updated_at updated_by
delete_record (records delete): id
brief (briefs upsert): id project_id body_md version updated_at updated_by machine_id
wiki_page (wiki_pages upsert): id project_id slug title body_md updated_at updated_by deleted
resource (resources upsert): id project_id kind url title meta created_at updated_at deleted
table briefs: project_id body_md version updated_at updated_by history_id machine_id
table events: session_id seq ts kind text meta_json
table machines: id name os role last_seen revoked
table project_paths: project_id machine_id path git_remote
table projects: id name created_at updated_at deleted chats merged_into
table records: id project_id kind title body status pinned source_session_id created_at \
updated_at updated_by
table resources: id project_id kind url title meta_json created_at deleted updated_at
table sessions: id project_id machine_id agent agent_session_id origin cwd title status branch \
worktree transcript_path started_at ended_at last_activity_at exit_code summary_json \
distilled_through_seq tokens_in tokens_out cost_usd parent_session_id stopped_by_user
table wiki_pages: id project_id slug title body_md updated_at updated_by deleted
";
        let samples = samples();
        let covered: std::collections::BTreeSet<usize> = samples.iter().map(variant).collect();
        assert_eq!(covered, (0..VARIANTS).collect(), "one sample per variant");

        let mut schema = String::new();
        let mut tables = std::collections::BTreeSet::new();
        for c in &samples {
            let (table, op, _) = c.describe();
            tables.insert(table);
            let json = serde_json::to_value(c).unwrap();
            let fields: Vec<&str> = json["row"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            schema.push_str(&format!(
                "{} ({table} {op}): {}\n",
                json["entity"].as_str().unwrap(),
                fields.join(" ")
            ));
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blirp.db");
        drop(blirp_core::store::Store::open(&path).unwrap());
        let conn = rusqlite::Connection::open(&path).unwrap();
        for table in tables {
            let mut st = conn
                .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
                .unwrap();
            let cols: Vec<String> = st
                .query_map([table], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            schema.push_str(&format!("table {table}: {}\n", cols.join(" ")));
        }
        assert_eq!(
            (super::SYNC_VERSIONS, schema.as_str()),
            (&[3][..], PINNED),
            "the replicated schema changed: bump SYNC_VERSIONS (see its doc)"
        );
    }
}
