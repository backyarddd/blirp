//! Embedded, versioned schema migrations tracked with `PRAGMA user_version`.
//! Append new migrations; never edit a released one.

use rusqlite::Connection;

/// Index `i` holds the migration that moves the schema from version `i` to `i + 1`.
pub(crate) const MIGRATIONS: &[&str] = &[
    V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11, V12, V13, V14, V15, V16, V17, V18, V19,
];

/// Schema of §5. Note on the FTS tables: they are external-content tables keyed
/// by the implicit rowid of `events`/`records`. blirp never runs `VACUUM`
/// (which may renumber implicit rowids); anyone who does must follow it with
/// `INSERT INTO events_fts(events_fts) VALUES('rebuild')` (same for records_fts).
const V1: &str = r#"
CREATE TABLE machines(
    id        TEXT PRIMARY KEY,
    name      TEXT NOT NULL,
    os        TEXT NOT NULL,
    role      TEXT NOT NULL CHECK(role IN ('standalone','node','hub')),
    last_seen INTEGER NOT NULL,
    revoked   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE projects(
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE project_paths(
    project_id TEXT NOT NULL REFERENCES projects(id),
    machine_id TEXT NOT NULL,
    path       TEXT NOT NULL,
    git_remote TEXT NULL,
    PRIMARY KEY(machine_id, path)
);
CREATE INDEX project_paths_project ON project_paths(project_id);
CREATE INDEX project_paths_remote ON project_paths(git_remote) WHERE git_remote IS NOT NULL;

CREATE TABLE sessions(
    id                    TEXT PRIMARY KEY,
    project_id            TEXT NOT NULL,
    machine_id            TEXT NOT NULL,
    agent                 TEXT NOT NULL,
    agent_session_id      TEXT NULL,
    origin                TEXT NOT NULL CHECK(origin IN ('blirp','external')),
    cwd                   TEXT NOT NULL,
    title                 TEXT NULL,
    status                TEXT NOT NULL CHECK(status IN ('starting','working','idle','waiting','completed','failed','detached')),
    branch                TEXT NULL,
    worktree              TEXT NULL,
    transcript_path       TEXT NULL,
    started_at            INTEGER NOT NULL,
    ended_at              INTEGER NULL,
    last_activity_at      INTEGER NOT NULL,
    exit_code             INTEGER NULL,
    summary_json          TEXT NULL,
    distilled_through_seq INTEGER NOT NULL DEFAULT 0,
    tokens_in             INTEGER NOT NULL DEFAULT 0,
    tokens_out            INTEGER NOT NULL DEFAULT 0,
    cost_usd              REAL NOT NULL DEFAULT 0,
    parent_session_id     TEXT NULL,
    UNIQUE(agent, agent_session_id)
);
CREATE INDEX sessions_project ON sessions(project_id, started_at DESC);
CREATE INDEX sessions_started ON sessions(started_at DESC, id DESC);
CREATE INDEX sessions_machine_status ON sessions(machine_id, status);

CREATE TABLE events(
    session_id TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    ts         INTEGER NOT NULL,
    kind       TEXT NOT NULL CHECK(kind IN ('user','assistant','tool_call','tool_result','system','file_edit','summary')),
    text       TEXT NOT NULL,
    meta_json  TEXT NULL,
    PRIMARY KEY(session_id, seq)
);
CREATE VIRTUAL TABLE events_fts USING fts5(
    text, content='events', content_rowid='rowid', tokenize='porter unicode61'
);
CREATE TRIGGER events_fts_ai AFTER INSERT ON events BEGIN
    INSERT INTO events_fts(rowid, text) VALUES (new.rowid, new.text);
END;
CREATE TRIGGER events_fts_ad AFTER DELETE ON events BEGIN
    INSERT INTO events_fts(events_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
END;
CREATE TRIGGER events_fts_au AFTER UPDATE ON events BEGIN
    INSERT INTO events_fts(events_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
    INSERT INTO events_fts(rowid, text) VALUES (new.rowid, new.text);
END;

CREATE TABLE records(
    id                TEXT PRIMARY KEY,
    project_id        TEXT NOT NULL,
    kind              TEXT NOT NULL CHECK(kind IN ('decision','plan','note','open_thread','gotcha')),
    title             TEXT NOT NULL,
    body              TEXT NOT NULL,
    status            TEXT NOT NULL CHECK(status IN ('active','resolved','archived')),
    pinned            INTEGER NOT NULL DEFAULT 0,
    source_session_id TEXT NULL,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    updated_by        TEXT NOT NULL
);
CREATE INDEX records_project ON records(project_id, status, updated_at DESC);
CREATE VIRTUAL TABLE records_fts USING fts5(
    title, body, content='records', content_rowid='rowid', tokenize='porter unicode61'
);
CREATE TRIGGER records_fts_ai AFTER INSERT ON records BEGIN
    INSERT INTO records_fts(rowid, title, body) VALUES (new.rowid, new.title, new.body);
END;
CREATE TRIGGER records_fts_ad AFTER DELETE ON records BEGIN
    INSERT INTO records_fts(records_fts, rowid, title, body) VALUES ('delete', old.rowid, old.title, old.body);
END;
CREATE TRIGGER records_fts_au AFTER UPDATE ON records BEGIN
    INSERT INTO records_fts(records_fts, rowid, title, body) VALUES ('delete', old.rowid, old.title, old.body);
    INSERT INTO records_fts(rowid, title, body) VALUES (new.rowid, new.title, new.body);
END;

CREATE TABLE briefs(
    project_id TEXT PRIMARY KEY,
    body_md    TEXT NOT NULL,
    version    INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    updated_by TEXT NOT NULL
);
CREATE TABLE brief_history(
    project_id TEXT NOT NULL,
    version    INTEGER NOT NULL,
    body_md    TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    updated_by TEXT NOT NULL,
    PRIMARY KEY(project_id, version)
);

CREATE TABLE wiki_pages(
    id         TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    slug       TEXT NOT NULL,
    title      TEXT NOT NULL,
    body_md    TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    updated_by TEXT NOT NULL,
    deleted    INTEGER NOT NULL DEFAULT 0,
    UNIQUE(project_id, slug)
);

CREATE TABLE suggestions(
    id                TEXT PRIMARY KEY,
    project_id        TEXT NOT NULL,
    target            TEXT NOT NULL CHECK(target IN ('brief','record','wiki')),
    target_id         TEXT NULL,
    proposal_json     TEXT NOT NULL,
    rationale         TEXT NOT NULL,
    source_session_id TEXT NULL,
    status            TEXT NOT NULL CHECK(status IN ('pending','accepted','rejected','dismissed')),
    created_at        INTEGER NOT NULL,
    decided_at        INTEGER NULL
);
CREATE INDEX suggestions_project ON suggestions(project_id, status, created_at DESC);

CREATE TABLE resources(
    id         TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK(kind IN ('link','repo','pr','issue','doc','file')),
    url        TEXT NOT NULL,
    title      TEXT NOT NULL,
    meta_json  TEXT NULL,
    created_at INTEGER NOT NULL,
    deleted    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX resources_project ON resources(project_id, created_at DESC);

CREATE TABLE ingest_cursors(
    adapter     TEXT NOT NULL,
    source      TEXT NOT NULL,
    cursor_json TEXT NOT NULL,
    PRIMARY KEY(adapter, source)
);

CREATE TABLE settings(
    key        TEXT PRIMARY KEY,
    value_json TEXT NOT NULL
);

CREATE TABLE devices(
    id                    TEXT PRIMARY KEY,
    name                  TEXT NOT NULL,
    kind                  TEXT NOT NULL CHECK(kind IN ('machine','browser')),
    token_hash            TEXT NULL,
    node_id               TEXT NULL,
    created_at            INTEGER NOT NULL,
    last_seen             INTEGER NOT NULL,
    revoked               INTEGER NOT NULL DEFAULT 0,
    can_control_terminals INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX devices_token ON devices(token_hash) WHERE token_hash IS NOT NULL;

CREATE TABLE outbox(
    origin_seq   INTEGER PRIMARY KEY AUTOINCREMENT,
    entity       TEXT NOT NULL,
    op           TEXT NOT NULL,
    key          TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    ts           INTEGER NOT NULL
);

CREATE TABLE sync_state(
    peer                   TEXT PRIMARY KEY,
    last_pushed_origin_seq INTEGER NOT NULL DEFAULT 0,
    last_pulled_hub_seq    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE hub_log(
    hub_seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    origin_machine TEXT NOT NULL,
    origin_seq     INTEGER NOT NULL,
    entity         TEXT NOT NULL,
    op             TEXT NOT NULL,
    key            TEXT NOT NULL,
    payload_json   TEXT NOT NULL,
    ts             INTEGER NOT NULL,
    UNIQUE(origin_machine, origin_seq)
);
"#;

/// Replication (§10): pulling checks the outbox for unsynced local writes to
/// the same row, and the hub filters `hub_log` by origin.
const V2: &str = r#"
CREATE INDEX outbox_entity_key ON outbox(entity, key, origin_seq);
CREATE INDEX hub_log_origin ON hub_log(origin_machine, hub_seq);
"#;

/// A user Stop is recorded as intent (§7): `completed`, no exit code.
const V3: &str = r#"
ALTER TABLE sessions ADD COLUMN stopped_by_user INTEGER NOT NULL DEFAULT 0;
"#;

/// Subagent children are listed and counted by parent (§11).
const V4: &str = r#"
CREATE INDEX sessions_parent ON sessions(parent_session_id, started_at DESC)
    WHERE parent_session_id IS NOT NULL;
"#;

/// Coalesced status-only session writes (§5): rows whose outbox entry is
/// deferred until `due`.
const V5: &str = r#"
CREATE TABLE outbox_deferred(
    entity TEXT NOT NULL,
    key    TEXT NOT NULL,
    due    INTEGER NOT NULL,
    PRIMARY KEY(entity, key)
);
"#;

/// Brief history is append-only (§5): rows are keyed by a globally unique id
/// and carry their machine, so versions written on two machines at once
/// both survive; version numbers are derived by time when read. Existing
/// rows get deterministic ids, so the same legacy row has the same id on
/// every machine. `briefs.history_id` names the current version's row.
const V6: &str = r#"
CREATE TABLE brief_history_v6(
    id         TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    body_md    TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    updated_by TEXT NOT NULL,
    machine_id TEXT NOT NULL DEFAULT ''
);
INSERT INTO brief_history_v6(id, project_id, body_md, updated_at, updated_by)
    SELECT 'legacy-' || project_id || '-' || version, project_id, body_md, updated_at, updated_by
    FROM brief_history;
DROP TABLE brief_history;
ALTER TABLE brief_history_v6 RENAME TO brief_history;
CREATE INDEX brief_history_project ON brief_history(project_id, updated_at, id);
ALTER TABLE briefs ADD COLUMN history_id TEXT NOT NULL DEFAULT '';
ALTER TABLE briefs ADD COLUMN machine_id TEXT NOT NULL DEFAULT '';
UPDATE briefs SET history_id = 'legacy-' || project_id || '-' || version;
"#;

/// Deleted sessions stay deleted (§5): a tombstone per deleted session id
/// (written wherever the delete is applied, so it replicates with it);
/// later writes of that session or its events are ignored.
const V7: &str = r#"
CREATE TABLE deleted_sessions(
    id         TEXT PRIMARY KEY,
    deleted_at INTEGER NOT NULL
);
"#;

/// Replicated memory converges on the newest version (§10): resources get the
/// `updated_at` the other shared rows already have, and hard-deleted records
/// a tombstone like sessions, so a stale copy never brings them back.
const V8: &str = r#"
ALTER TABLE resources ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;
UPDATE resources SET updated_at = created_at;
CREATE TABLE deleted_records(
    id         TEXT PRIMARY KEY,
    deleted_at INTEGER NOT NULL
);
"#;

/// Bounded hub log (§10). Events are logged without their payload: the hub
/// has every event it logged in `events` (append-only), so a pull reads it
/// from there instead of keeping a second copy. Superseded upserts are
/// compacted (`Store::compact_hub_log`), found by row through the partial
/// index. `hub_pulls` holds the pull position each node last asked for, which
/// bounds compaction.
const V9: &str = r#"
UPDATE hub_log SET payload_json = '' WHERE entity = 'events';
CREATE INDEX hub_log_row ON hub_log(entity, key, hub_seq) WHERE op = 'upsert';
CREATE TABLE hub_pulls(
    machine_id TEXT PRIMARY KEY,
    after      INTEGER NOT NULL
);
"#;

/// Chats (§5): each machine's bucket for sessions that belong to no
/// project is a project flagged `chats` (id `chats-<machine id>`), so every
/// machine can tell it apart; the daemon moves an existing Home project
/// into it at start (`Store::ensure_chats`). A merged project records its
/// target, so a workspace of a merged project follows it.
const V10: &str = r#"
ALTER TABLE projects ADD COLUMN chats INTEGER NOT NULL DEFAULT 0;
ALTER TABLE projects ADD COLUMN merged_into TEXT;
"#;

/// Project file sync through the hub (docs/project-files.md). New tables
/// only, none replicated through `hub_log`.
///
/// Hub: `file_roots` (one per origin folder; `head` is the root's sequence,
/// which never restarts: a deleted hub copy keeps its row with `deleted_at`,
/// and a root made again gets a new `incarnation`),
/// `file_entries` (current version of every path, `hash` and `link` both
/// NULL for a tombstone), `file_history` (replaced versions, kept
/// `files.keep_versions_days`), `file_blobs` (content-addressed blobs in
/// `BLIRP_HOME/files/blobs`, `stored` = compressed bytes on disk) and
/// `file_projects` (per-project Default/On/Off).
///
/// Every machine: `file_copies` (its working copies of roots: its own
/// origin folders and downloaded copies; `seen` is the root version up to
/// which it compared the hub's entries; `incarnation` the hub root it
/// belongs to; `detached` once the hub copy is gone, so the folder never
/// becomes an origin of its own; `pending` while a download writes it), `file_base` (per path the version
/// it last agreed on with the hub; `skipped` paths it cannot hold,
/// `rejected` content the hub refused as a conflict that an origin keeps
/// until "Bring changes here", `-` for a refused delete) and `file_hashes`
/// (hash cache).
///
/// `devices.can_access_files`: the portal "Files" permission, off by default.
const V11: &str = r#"
CREATE TABLE file_roots(
    root_id       TEXT PRIMARY KEY,
    machine_id    TEXT NOT NULL,
    path          TEXT NOT NULL,
    project_id    TEXT NOT NULL,
    head          INTEGER NOT NULL DEFAULT 0,
    manifest_json TEXT NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    incarnation   TEXT NOT NULL DEFAULT '',
    deleted_at    INTEGER NULL
);
CREATE TABLE file_entries(
    root_id    TEXT NOT NULL,
    path       TEXT NOT NULL,
    version    INTEGER NOT NULL,
    hash       TEXT NULL,
    link       TEXT NULL,
    size       INTEGER NOT NULL,
    mode_x     INTEGER NOT NULL,
    mtime      INTEGER NOT NULL,
    by_machine TEXT NOT NULL,
    at         INTEGER NOT NULL,
    PRIMARY KEY(root_id, path)
);
CREATE INDEX file_entries_version ON file_entries(root_id, version);
CREATE INDEX file_entries_hash ON file_entries(hash) WHERE hash IS NOT NULL;
CREATE TABLE file_history(
    root_id     TEXT NOT NULL,
    path        TEXT NOT NULL,
    version     INTEGER NOT NULL,
    hash        TEXT NULL,
    link        TEXT NULL,
    size        INTEGER NOT NULL,
    mode_x      INTEGER NOT NULL,
    mtime       INTEGER NOT NULL,
    by_machine  TEXT NOT NULL,
    at          INTEGER NOT NULL,
    replaced_at INTEGER NOT NULL,
    PRIMARY KEY(root_id, path, version)
);
CREATE INDEX file_history_replaced ON file_history(replaced_at);
CREATE INDEX file_history_hash ON file_history(hash) WHERE hash IS NOT NULL;
CREATE TABLE file_blobs(
    hash       TEXT PRIMARY KEY,
    size       INTEGER NOT NULL,
    stored     INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE file_projects(
    project_id TEXT PRIMARY KEY,
    mode       TEXT NOT NULL CHECK(mode IN ('default','on','off'))
);
CREATE TABLE file_copies(
    path       TEXT PRIMARY KEY,
    root_id    TEXT NOT NULL,
    origin     INTEGER NOT NULL,
    mode       TEXT NOT NULL CHECK(mode IN ('on_demand','keep_synced','detached','pending')),
    seen       INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    incarnation TEXT NOT NULL DEFAULT ''
);
CREATE TABLE file_base(
    copy     TEXT NOT NULL,
    path     TEXT NOT NULL,
    version  INTEGER NOT NULL,
    hash     TEXT NULL,
    link     TEXT NULL,
    mode_x   INTEGER NOT NULL DEFAULT 0,
    skipped  INTEGER NOT NULL DEFAULT 0,
    rejected TEXT NULL,
    PRIMARY KEY(copy, path)
);
CREATE TABLE file_hashes(
    copy       TEXT NOT NULL,
    path       TEXT NOT NULL,
    size       INTEGER NOT NULL,
    mtime_ns   INTEGER NOT NULL,
    file_id    INTEGER NOT NULL,
    hash       TEXT NOT NULL,
    secret     INTEGER NOT NULL,
    checked_at INTEGER NOT NULL,
    PRIMARY KEY(copy, path)
);
ALTER TABLE devices ADD COLUMN can_access_files INTEGER NOT NULL DEFAULT 0;
"#;

/// Re-send every project once (§10). Nodes still on 0.1.x pulled the
/// projects the hub wrote with the fields of migration 10 (`chats`,
/// `merged_into`), dropped those fields and moved their pull cursor past
/// them, so they never got them again. The next time replication is on
/// (`Store::set_replication`), each machine queues its current projects; the
/// hub's copy then reaches every node, and its sticky fields win the
/// conflict rule over a copy that lost them. Nothing to re-send without
/// projects. Since then the sync protocol version gates replicated schema
/// changes (§10), so an older peer never pulls rows it cannot store.
const V12: &str = r#"
INSERT INTO settings(key, value_json)
    SELECT 'sync.requeue_projects', 'true' WHERE EXISTS (SELECT 1 FROM projects)
    ON CONFLICT(key) DO NOTHING;
"#;

/// Hub: entries that change another machine's session or folder the hub
/// does not have yet (its owner has not synced it), kept until the owner's
/// row arrives (§10). Never replicated.
const V13: &str = r#"
CREATE TABLE hub_parked(
    origin_machine TEXT NOT NULL,
    origin_seq     INTEGER NOT NULL,
    entity         TEXT NOT NULL,
    key            TEXT NOT NULL,
    payload_json   TEXT NOT NULL,
    ts             INTEGER NOT NULL,
    parked_at      INTEGER NOT NULL,
    PRIMARY KEY(origin_machine, origin_seq)
);
CREATE INDEX hub_parked_row ON hub_parked(entity, key);
"#;

/// A session's title and project are edited by other machines too (§10):
/// each converges on its newest edit by its own time, so the owner's
/// full-row writes (status ticks) never revert a newer rename or move.
/// Existing rows read as never edited (0). Changes logged before carry no
/// times either, so between them the larger value would win, not the later
/// one: a hub re-sends its sessions once with time 1
/// (`sync.stamp_sessions`, `Store::hub_stamp_legacy_sessions`), so every
/// machine converges on the hub's values, which followed hub order.
const V14: &str = r#"
ALTER TABLE sessions ADD COLUMN title_updated_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN project_updated_at INTEGER NOT NULL DEFAULT 0;
INSERT INTO settings(key, value_json)
    SELECT 'sync.stamp_sessions', 'true' WHERE EXISTS (SELECT 1 FROM sessions)
    ON CONFLICT(key) DO NOTHING;
"#;

/// The identity of a working copy's folder at its last settled pass
/// (`<device>:<inode>`), so a folder deleted and made again while the
/// daemon was stopped is still told apart.
const V15: &str = r#"
ALTER TABLE file_copies ADD COLUMN identity TEXT NULL;
"#;

/// `sessions.compacted_at`: newest compaction summary of the transcript (§8),
/// for the "start a fresh session" suggestion. Set by ingest from now on;
/// sessions ingested before stay null until their next compaction (a backfill
/// would scan every event once, for sessions that are mostly over).
const V16: &str = r#"
ALTER TABLE sessions ADD COLUMN compacted_at INTEGER NULL;
"#;

/// `event_floors`: per session, events below `below_seq` were dropped
/// (`Change::TruncateEvents`, replicated) and are ignored when they arrive.
const V17: &str = r#"
CREATE TABLE event_floors(
    session_id TEXT PRIMARY KEY,
    below_seq  INTEGER NOT NULL
);
"#;

/// `sessions.context_near_full_at`: the agent's context crossed 90% of its
/// window (codex reports both), for the "start a fresh session" suggestion.
const V18: &str = r#"
ALTER TABLE sessions ADD COLUMN context_near_full_at INTEGER NULL;
"#;

/// `sessions.distilled_through_seq` is -1 before the first distill: event
/// seqs start at 0 (an ingested transcript's first line), so 0 meant both
/// "nothing" and "through the first event", and the first event was never
/// distilled. A session with no successful summary had nothing distilled.
/// Only this machine's sessions: only the owner distills a session (and
/// its copies elsewhere keep what replication gave them).
const V19: &str = r#"
UPDATE sessions SET distilled_through_seq = -1
 WHERE distilled_through_seq = 0
   AND (summary_json IS NULL OR json_extract(summary_json, '$.distilled_at') IS NULL)
   AND machine_id = (SELECT json_extract(value_json, '$') FROM settings WHERE key = 'machine_id');
"#;

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error(
        "database schema version {found} is newer than this blirp supports ({supported}); upgrade blirp"
    )]
    TooNew { found: i64, supported: i64 },
    #[error("migration to version {version} failed: {source}")]
    Failed {
        version: i64,
        #[source]
        source: rusqlite::Error,
    },
}

/// Apply every pending migration, each in its own transaction.
pub(crate) fn migrate(conn: &mut Connection) -> Result<i64, MigrationError> {
    let latest = MIGRATIONS.len() as i64;
    let read_version =
        |c: &Connection| c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0));
    let current =
        read_version(conn).map_err(|source| MigrationError::Failed { version: 0, source })?;
    if current > latest {
        return Err(MigrationError::TooNew {
            found: current,
            supported: latest,
        });
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = i as i64 + 1;
        let fail = |source| MigrationError::Failed { version, source };
        let tx = conn.transaction().map_err(fail)?;
        tx.execute_batch(sql).map_err(fail)?;
        tx.pragma_update(None, "user_version", version)
            .map_err(fail)?;
        tx.commit().map_err(fail)?;
    }
    Ok(latest)
}
