//! Project file sync end to end: a hub and two nodes as in-process daemons
//! on temp homes, iroh on loopback only (`BLIRP_LOOPBACK_ONLY`, set by
//! `.cargo/config.toml`). Copies land in `~/blirp`, so every test runs in a
//! child process whose home folder is a temp dir: nothing here touches the
//! real home folder.

// Test helpers panic on failure by design.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use blirp::daemon::{Daemon, DaemonOptions};
use blirp_core::model::{
    AppliedFiles, DownloadJob, FilesIncoming, FilesOverview, FilesPreview, ProjectFiles,
    ProjectSummary, SyncInvite, SyncStatus,
};
use blirp_core::paths::Paths;
use reqwest::Method;
use serde_json::{Value, json};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CHILD: &str = "BLIRP_FILES_TEST_CHILD";

/// Run `test` again in a child process with a temp home folder; true when
/// this is that child (run the body), false in the parent (done).
fn in_temp_home(test: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let home = tempfile::tempdir().unwrap();
    let mut c = blirp_core::process::command(std::env::current_exe().unwrap());
    c.args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path());
    let out = c.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{test} failed:\n{text}");
    assert!(text.contains("1 passed"), "{test} did not run:\n{text}");
    false
}

struct Node {
    daemon: Daemon,
    http: reqwest::Client,
    base: String,
}

impl Node {
    async fn start(home: &Path, name: &str) -> Node {
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!("[machine]\nname = \"{name}\"\n[sync]\nrelay = \"disabled\"\n"),
        )
        .unwrap();
        let daemon = Daemon::start(DaemonOptions {
            paths: Paths::at(home),
            port: Some(0),
            ingest: None,
        })
        .await
        .unwrap();
        Node {
            base: format!("http://127.0.0.1:{}", daemon.port),
            http: reqwest::Client::new(),
            daemon,
        }
    }

    fn id(&self) -> String {
        self.daemon.state.machine.id.clone()
    }

    async fn req(&self, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        let mut r = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(self.daemon.token())
            .timeout(Duration::from_secs(90));
        if let Some(b) = body {
            r = r.json(&b);
        }
        r.send().await.unwrap()
    }

    async fn ok<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> T {
        let r = self.req(method.clone(), path, body).await;
        let status = r.status();
        let text = r.text().await.unwrap();
        assert!(status.is_success(), "{method} {path}: {status} {text}");
        serde_json::from_str(&text).unwrap()
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        self.ok(Method::GET, path, None).await
    }

    async fn project(&self, dir: &Path) -> String {
        let p: ProjectSummary = self
            .ok(Method::POST, "/api/projects", Some(json!({"path": dir})))
            .await;
        p.project.id
    }

    async fn files(&self, project: &str) -> ProjectFiles {
        self.get(&format!("/api/projects/{project}/files-sync"))
            .await
    }

    async fn start_now(&self) {
        let _: FilesOverview = self.ok(Method::POST, "/api/files/start-now", None).await;
    }

    async fn pause(&self, paused: bool) {
        let _: FilesOverview = self
            .ok(
                Method::POST,
                "/api/files/pause",
                Some(json!({ "paused": paused })),
            )
            .await;
    }

    /// Make a copy of `root` here and wait for it; returns its folder.
    async fn download(&self, root: &str, name: &str) -> PathBuf {
        let job: DownloadJob = self
            .ok(
                Method::POST,
                &format!("/api/machines/{}/files/download", self.id()),
                Some(json!({ "root_id": root, "name": name })),
            )
            .await;
        let mut done = job;
        let deadline = Instant::now() + Duration::from_secs(60);
        while done.state.as_str() == "running" {
            assert!(Instant::now() < deadline, "download did not finish");
            tokio::time::sleep(Duration::from_millis(100)).await;
            done = self
                .get(&format!(
                    "/api/machines/{}/files/download/{}",
                    self.id(),
                    done.id
                ))
                .await;
        }
        assert_eq!(done.state.as_str(), "done", "{:?}", done.error);
        PathBuf::from(done.dest)
    }
}

async fn eventually<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(60);
    while !f().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn pair(hub: &Node, node: &Node) {
    let _: SyncStatus = hub.ok(Method::POST, "/api/sync/hub/enable", None).await;
    let inv: SyncInvite = hub.ok(Method::POST, "/api/sync/invite", None).await;
    let _: SyncStatus = node
        .ok(
            Method::POST,
            "/api/sync/join",
            Some(json!({"invite": inv.invite, "code": inv.code})),
        )
        .await;
    eventually("node connected", || async {
        let s: SyncStatus = node.get("/api/sync/status").await;
        s.connected
    })
    .await;
}

/// The hub's view of the project's only root: (root id, files there).
async fn hub_files(hub: &Node, project: &str) -> Option<(String, i64)> {
    // The project may not have replicated to the hub yet.
    let r = hub
        .req(
            Method::GET,
            &format!("/api/projects/{project}/files-sync"),
            None,
        )
        .await;
    if !r.status().is_success() {
        return None;
    }
    let f: ProjectFiles = r.json().await.unwrap();
    f.roots
        .into_iter()
        .find_map(|r| r.hub.map(|h| (r.root_id, h.files)))
}

async fn read_eventually(path: &Path, want: &str) {
    eventually(&format!("{} = {want:?}", path.display()), || async {
        std::fs::read_to_string(path).is_ok_and(|t| t == want)
    })
    .await;
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

struct Rig {
    _tmp: tempfile::TempDir,
    hub: Node,
    b: Node,
    c: Node,
    project: String,
    origin: PathBuf,
    root: String,
}

/// Hub A, nodes B and C; B's folder uploaded.
async fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let hub = Node::start(&tmp.path().join("a"), "hub-a").await;
    let b = Node::start(&tmp.path().join("b"), "node-b").await;
    let c = Node::start(&tmp.path().join("c"), "node-c").await;
    pair(&hub, &b).await;
    pair(&hub, &c).await;
    let origin = tmp.path().join("work").join("proj");
    write(&origin.join("a.txt"), "one");
    write(&origin.join("sub/b.txt"), "two");
    write(&origin.join(".env"), "API_TOKEN=do-not-sync");
    write(&origin.join("node_modules/x/index.js"), "ignored");
    let project = b.project(&origin).await;
    let origin = dunce::canonicalize(&origin).unwrap();

    // The preview is local and names what stays behind.
    let p: FilesPreview = b
        .get(&format!("/api/projects/{project}/files-sync/preview"))
        .await;
    assert_eq!(p.files, 2, "{p:?}");
    let reasons: Vec<(String, Vec<String>)> = p
        .excluded
        .iter()
        .map(|g| (format!("{:?}", g.reason), g.paths.clone()))
        .collect();
    assert!(
        reasons.contains(&("Secret".into(), vec![".env".into()])),
        "{reasons:?}"
    );
    assert!(
        reasons.contains(&("Ignored".into(), vec!["node_modules/".into()])),
        "{reasons:?}"
    );

    // First run: nothing uploads before the grace period ends.
    let o: FilesOverview = b.get("/api/files/status").await;
    assert!(o.available && o.grace_until.is_some(), "{o:?}");
    for n in [&hub, &b, &c] {
        n.start_now().await;
    }
    eventually("the origin uploaded", || async {
        hub_files(&hub, &project).await.is_some_and(|(_, n)| n == 2)
    })
    .await;
    let root = hub_files(&hub, &project).await.unwrap().0;
    Rig {
        _tmp: tmp,
        hub,
        b,
        c,
        project,
        origin,
        root,
    }
}

impl Rig {
    async fn shutdown(self) {
        for n in [self.c, self.b, self.hub] {
            n.daemon.shutdown().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_copy_edit_on_the_hub_copy_and_bring_back() {
    if !in_temp_home("upload_copy_edit_on_the_hub_copy_and_bring_back") {
        return;
    }
    let r = rig().await;

    // A copy on C: the hub's files, never the secret or ignored ones.
    let on_c = r.c.download(&r.root, "proj-c").await;
    assert_eq!(std::fs::read_to_string(on_c.join("a.txt")).unwrap(), "one");
    assert_eq!(
        std::fs::read_to_string(on_c.join("sub/b.txt")).unwrap(),
        "two"
    );
    assert!(!on_c.join(".env").exists() && !on_c.join("node_modules").exists());
    // It is C's folder of the same project now.
    let f = r.c.files(&r.project).await;
    let mine = f.roots.iter().find_map(|x| x.local.clone()).unwrap();
    assert!(!mine.origin);

    // The hub copy for a cloud session, edited there.
    let on_hub = r.hub.download(&r.root, "proj-hub").await;
    write(&on_hub.join("a.txt"), "edited on the hub");
    eventually("the hub copy's edit reached the root", || async {
        let (_, _) = hub_files(&r.hub, &r.project).await.unwrap();
        let inc: FilesIncoming =
            r.b.get(&format!(
                "/api/projects/{}/files-sync/incoming?root={}",
                r.project,
                urlencode(&r.origin)
            ))
            .await;
        inc.files
            .iter()
            .any(|f| f.path == "a.txt" && f.action.as_str() == "update")
    })
    .await;
    // The origin is upload-only: nothing changed there until asked.
    assert_eq!(
        std::fs::read_to_string(r.origin.join("a.txt")).unwrap(),
        "one"
    );
    let applied: AppliedFiles =
        r.b.ok(
            Method::POST,
            &format!("/api/projects/{}/files-sync/apply", r.project),
            Some(json!({ "root": r.origin })),
        )
        .await;
    assert_eq!(
        (applied.written, applied.conflicts.len()),
        (1, 0),
        "{applied:?}"
    );
    assert_eq!(
        std::fs::read_to_string(r.origin.join("a.txt")).unwrap(),
        "edited on the hub"
    );
    // C takes it with Update from hub.
    let _: AppliedFiles =
        r.c.ok(
            Method::POST,
            &format!("/api/projects/{}/files-sync/apply", r.project),
            Some(json!({ "root": on_c })),
        )
        .await;
    assert_eq!(
        std::fs::read_to_string(on_c.join("a.txt")).unwrap(),
        "edited on the hub"
    );
    r.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_edits_keep_both_versions_and_modify_beats_delete() {
    if !in_temp_home("concurrent_edits_keep_both_versions_and_modify_beats_delete") {
        return;
    }
    let r = rig().await;
    let on_c = r.c.download(&r.root, "proj-c").await;

    // C edits while paused; B's edit of the same file lands first.
    r.c.pause(true).await;
    write(&on_c.join("a.txt"), "C's edit");
    std::fs::remove_file(r.origin.join("sub/b.txt")).unwrap();
    write(&on_c.join("sub/b.txt"), "C changed it");
    write(&r.origin.join("a.txt"), "B's edit");
    eventually("B's edit and delete reached the hub", || async {
        hub_files(&r.hub, &r.project)
            .await
            .is_some_and(|(_, n)| n == 1)
    })
    .await;
    r.c.pause(false).await;
    // C lost the race on a.txt: its version is kept as a conflict copy and
    // it takes B's; its change to the deleted file wins (modify beats delete).
    read_eventually(&on_c.join("a.txt"), "B's edit").await;
    let copy = std::fs::read_dir(&on_c)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("a.conflict-node-c-") && n.ends_with(".txt"))
        .expect("conflict copy on C");
    assert_eq!(
        std::fs::read_to_string(on_c.join(&copy)).unwrap(),
        "C's edit"
    );
    eventually(
        "the hub has both versions and the restored file",
        || async {
            let f = r.hub.files(&r.project).await;
            f.roots.iter().any(|x| {
                x.hub
                    .as_ref()
                    .is_some_and(|h| h.files == 3 && h.conflicts == 1)
            })
        },
    )
    .await;
    // B brings everything here; nothing was lost.
    let applied: AppliedFiles =
        r.b.ok(
            Method::POST,
            &format!("/api/projects/{}/files-sync/apply", r.project),
            Some(json!({ "root": r.origin })),
        )
        .await;
    assert!(applied.failed.is_empty(), "{applied:?}");
    assert_eq!(
        std::fs::read_to_string(r.origin.join("sub/b.txt")).unwrap(),
        "C changed it"
    );
    assert_eq!(
        std::fs::read_to_string(r.origin.join(&copy)).unwrap(),
        "C's edit"
    );
    assert_eq!(
        std::fs::read_to_string(r.origin.join("a.txt")).unwrap(),
        "B's edit"
    );
    r.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bring_changes_here_uploads_nothing_while_paused() {
    if !in_temp_home("bring_changes_here_uploads_nothing_while_paused") {
        return;
    }
    let r = rig().await;
    r.b.pause(true).await;
    write(&r.origin.join("new.txt"), "made while paused");
    // Taking the hub's changes uploads first only where uploads may run:
    // the upload inside the request would have finished before it returns.
    let applied: AppliedFiles =
        r.b.ok(
            Method::POST,
            &format!("/api/projects/{}/files-sync/apply", r.project),
            Some(json!({ "root": r.origin })),
        )
        .await;
    assert!(applied.failed.is_empty(), "{applied:?}");
    assert_eq!(
        hub_files(&r.hub, &r.project).await.map(|(_, n)| n),
        Some(2),
        "uploaded while paused"
    );
    r.b.pause(false).await;
    eventually("the new file uploaded after Resume", || async {
        hub_files(&r.hub, &r.project)
            .await
            .is_some_and(|(_, n)| n == 3)
    })
    .await;
    r.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_machines_are_refused_and_off_pauses_uploads() {
    if !in_temp_home("revoked_machines_are_refused_and_off_pauses_uploads") {
        return;
    }
    let r = rig().await;
    let on_c = r.c.download(&r.root, "proj-c").await;

    // Off: uploads stop (the hub copy stays readable), and only then can
    // the hub copy be deleted.
    let r_del =
        r.b.req(
            Method::DELETE,
            &format!("/api/projects/{}/files-sync/roots/{}", r.project, r.root),
            None,
        )
        .await;
    assert_eq!(r_del.status(), 409);
    let f: ProjectFiles =
        r.b.ok(
            Method::PUT,
            &format!("/api/projects/{}/files-sync", r.project),
            Some(json!({"mode": "off"})),
        )
        .await;
    assert!(!f.effective);
    write(&r.origin.join("new.txt"), "not uploaded while off");
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(hub_files(&r.hub, &r.project).await.unwrap().1, 2);
    let _: ProjectFiles =
        r.b.ok(
            Method::PUT,
            &format!("/api/projects/{}/files-sync", r.project),
            Some(json!({"mode": "on"})),
        )
        .await;
    eventually("uploads resume when turned on", || async {
        hub_files(&r.hub, &r.project)
            .await
            .is_some_and(|(_, n)| n == 3)
    })
    .await;

    // The machine's own switch is a hard opt-out, whatever the project says.
    let settings: Value = r.b.get("/api/settings").await;
    let mut cfg = settings["config"].clone();
    cfg["sync"]["project_files"] = json!(false);
    let _: Value =
        r.b.ok(
            Method::PATCH,
            "/api/settings",
            Some(json!({"config": cfg, "base": settings["config"]})),
        )
        .await;
    let f = r.b.files(&r.project).await;
    assert!(!f.effective && !f.global, "{f:?}");
    write(
        &r.origin.join("private.txt"),
        "never uploaded from an opted-out machine",
    );
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(hub_files(&r.hub, &r.project).await.unwrap().1, 3);

    // A revoked machine's edits never reach the hub.
    let _ = r
        .hub
        .req(Method::DELETE, &format!("/api/machines/{}", r.c.id()), None)
        .await;
    write(&on_c.join("a.txt"), "from a revoked machine");
    tokio::time::sleep(Duration::from_secs(5)).await;
    let inc: FilesIncoming =
        r.b.get(&format!(
            "/api/projects/{}/files-sync/incoming?root={}",
            r.project,
            urlencode(&r.origin)
        ))
        .await;
    assert!(inc.files.is_empty(), "{inc:?}");
    r.shutdown().await;
}

fn urlencode(p: &Path) -> String {
    p.display()
        .to_string()
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folderless_workspaces_sync_and_land_in_the_other_workspace() {
    if !in_temp_home("folderless_workspaces_sync_and_land_in_the_other_workspace") {
        return;
    }
    let r = rig().await;
    // A project without folders works in B's blirp workspace.
    let p: ProjectSummary =
        r.b.ok(
            Method::POST,
            "/api/projects",
            Some(json!({"name": "notes"})),
        )
        .await;
    let pid = p.project.id.clone();
    let ws_b = PathBuf::from(p.workspace.clone().expect("workspace"));
    write(&ws_b.join("todo.md"), "from the workspace");
    // Creating it on demand is what the files API does too.
    let _: FilesPreview =
        r.b.get(&format!("/api/projects/{pid}/files-sync/preview"))
            .await;
    eventually("the workspace uploaded", || async {
        hub_files(&r.hub, &pid).await.is_some_and(|(_, n)| n == 1)
    })
    .await;
    let root = hub_files(&r.hub, &pid).await.unwrap().0;
    // C's copy goes to C's own workspace and adds no folder to the project.
    let on_c = r.c.download(&root, "ignored").await;
    let ws_c = r.c.daemon.state.paths.workspace_dir(&pid).unwrap();
    assert_eq!(
        dunce::canonicalize(&on_c).unwrap(),
        dunce::canonicalize(&ws_c).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(ws_c.join("todo.md")).unwrap(),
        "from the workspace"
    );
    let p: ProjectSummary = r.c.get(&format!("/api/projects/{pid}")).await;
    assert!(p.paths.is_empty(), "{:?}", p.paths);

    // Chats buckets never sync.
    let roots = r.hub.daemon.state.store.hub_file_roots().unwrap();
    assert!(
        roots.iter().all(|x| !x.project_id.starts_with("chats-")),
        "{roots:?}"
    );
    r.shutdown().await;
}

/// This machine's state of the project's only folder.
async fn local_state(n: &Node, project: &str) -> Option<String> {
    let f = n.files(project).await;
    f.roots
        .into_iter()
        .find_map(|r| r.local)
        .map(|l| l.state.as_str().to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_files_upload_and_missing_folders_wait_for_their_return() {
    if !in_temp_home("empty_files_upload_and_missing_folders_wait_for_their_return") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let hub = Node::start(&tmp.path().join("a"), "hub-a").await;
    let b = Node::start(&tmp.path().join("b"), "node-b").await;
    pair(&hub, &b).await;
    let origin = tmp.path().join("work").join("proj");
    write(&origin.join("a.txt"), "one");
    write(&origin.join("empty.txt"), "");
    let project = b.project(&origin).await;
    let origin = dunce::canonicalize(&origin).unwrap();
    // A folder gone before its first sync (a start with a drive unplugged).
    let gone = tmp.path().join("work").join("gone");
    write(&gone.join("x.txt"), "x");
    let gone_project = b.project(&gone).await;
    let gone_key = dunce::canonicalize(&gone).unwrap().display().to_string();
    std::fs::remove_dir_all(&gone).unwrap();
    for n in [&hub, &b] {
        n.start_now().await;
    }

    // The empty file does not hold up its folder.
    eventually("the folder with an empty file uploaded", || async {
        hub_files(&hub, &project).await.is_some_and(|(_, n)| n == 2)
    })
    .await;
    // The folder that is gone reports it, and no copy row was made.
    eventually("the gone folder shows missing", || async {
        local_state(&b, &gone_project).await.as_deref() == Some("missing")
    })
    .await;
    assert!(b.daemon.state.store.file_copy(&gone_key).unwrap().is_none());

    // Deleted while synced: missing, and taking the hub's changes is
    // refused rather than making the folder again.
    let head = || {
        let roots = hub.daemon.state.store.hub_file_roots().unwrap();
        roots.iter().find(|r| r.project_id == project).unwrap().head
    };
    let head_before = head();
    std::fs::remove_dir_all(&origin).unwrap();
    eventually("the deleted folder shows missing", || async {
        local_state(&b, &project).await.as_deref() == Some("missing")
    })
    .await;
    let apply_path = format!("/api/projects/{project}/files-sync/apply");
    let apply = |root: PathBuf| b.req(Method::POST, &apply_path, Some(json!({ "root": root })));
    let r = apply(origin.clone()).await;
    assert_eq!(r.status(), reqwest::StatusCode::CONFLICT);
    assert!(r.text().await.unwrap().contains("root_missing"));

    // Back as it was (a drive mounted again): the request sees it at once,
    // and the folder syncs again without uploading anything anew.
    write(&origin.join("a.txt"), "one");
    write(&origin.join("empty.txt"), "");
    let r = apply(origin.clone()).await;
    let status = r.status();
    assert!(status.is_success(), "{status} {}", r.text().await.unwrap());
    eventually("the folder is back in sync", || async {
        local_state(&b, &project).await.as_deref() == Some("idle")
    })
    .await;
    assert_eq!(
        head(),
        head_before,
        "the returned folder's files were committed again"
    );
    for n in [b, hub] {
        n.daemon.shutdown().await.unwrap();
    }
}
