//! Synthetic stores for memory tests (events inserted directly, no adapters).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use blirp_core::model::{
    Event, EventKind, Machine, MachineRole, Record, RecordKind, RecordStatus, Session,
    SessionOrigin, SessionStatus,
};
use blirp_core::store::{Change, Store};

pub fn project_store(name: &str) -> (tempfile::TempDir, Store, String) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("blirp.db")).unwrap();
    store
        .upsert_machine(&Machine {
            id: "m".into(),
            name: "box".into(),
            os: "windows".into(),
            role: MachineRole::Standalone,
            last_seen: 1,
            revoked: false,
        })
        .unwrap();
    store
        .set_setting(blirp_core::store::MACHINE_ID_KEY, &serde_json::json!("m"))
        .unwrap();
    let folder = dir.path().join(name);
    std::fs::create_dir_all(&folder).unwrap();
    let p = store.register_project("m", &folder, Some(name)).unwrap();
    (dir, store, p.id)
}

pub fn session(id: &str, project: &str, started_at: i64) -> Session {
    Session {
        id: id.into(),
        project_id: project.into(),
        machine_id: "m".into(),
        agent: "shell".into(),
        agent_session_id: None,
        origin: SessionOrigin::Blirp,
        cwd: "/tmp/x".into(),
        title: None,
        status: SessionStatus::Completed,
        branch: None,
        worktree: None,
        transcript_path: None,
        started_at,
        ended_at: None,
        last_activity_at: started_at,
        exit_code: None,
        summary: None,
        distilled_through_seq: 0,
        tokens_in: 0,
        tokens_out: 0,
        cost_usd: 0.0,
        parent_session_id: None,
        stopped_by_user: false,
        title_updated_at: 0,
        project_updated_at: 0,
        compacted_at: None,
    }
}

pub fn event(store: &Store, sid: &str, seq: i64, kind: EventKind, text: &str) {
    store
        .insert_event(Event {
            session_id: sid.into(),
            seq,
            ts: seq,
            kind,
            text: text.into(),
            meta: None,
        })
        .unwrap();
}

#[allow(clippy::too_many_arguments)]
pub fn record(
    store: &Store,
    pid: &str,
    kind: RecordKind,
    title: &str,
    body: &str,
    at: i64,
    pinned: bool,
) -> Record {
    let r = Record {
        // Ids are `[A-Za-z0-9_-]` (§5); titles may carry anything else.
        id: format!(
            "r-{}",
            title.replace(|c: char| !c.is_ascii_alphanumeric(), "_")
        ),
        project_id: pid.into(),
        kind,
        title: title.into(),
        body: body.into(),
        status: RecordStatus::Active,
        pinned,
        source_session_id: None,
        created_at: at,
        updated_at: at,
        updated_by: "distiller".into(),
    };
    store.apply(Change::Record(r.clone())).unwrap();
    r
}
