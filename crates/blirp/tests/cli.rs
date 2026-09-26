//! The `blirp` binary's daemon lifecycle commands, end to end: `daemon
//! --detach`, `logs`, `stop`, against a temp BLIRP_HOME and a fake user home
//! (so ingest never reads the real agent stores).

// Test helpers panic on failure by design.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Output;

const INGEST_VARS: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "PI_CODING_AGENT_DIR",
    "GEMINI_CLI_HOME",
    "XDG_DATA_HOME",
    "AMP_DATA_DIR",
    "CURSOR_CONFIG_DIR",
    "DSH_HOME",
];

fn blirp(home: &Path, user: &Path, args: &[&str]) -> Output {
    let mut cmd = blirp_core::process::command(env!("CARGO_BIN_EXE_blirp"));
    cmd.args(args)
        .env("BLIRP_HOME", home)
        .env("HOME", user)
        .env("USERPROFILE", user)
        .env("APPDATA", user.join("AppData"));
    for v in INGEST_VARS {
        cmd.env_remove(v);
    }
    cmd.output().unwrap()
}

/// Stops the detached daemon even when a test fails half way, so no daemon
/// outlives the test.
struct StopOnDrop<'a>(&'a Path, &'a Path);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        let _ = blirp(self.0, self.1, &["stop"]);
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn detach_logs_and_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("blirp");
    let user = tmp.path().join("user");
    std::fs::create_dir_all(&user).unwrap();

    let o = blirp(&home, &user, &["daemon", "--detach", "--port", "0"]);
    let _stop = StopOnDrop(&home, &user);
    assert!(o.status.success(), "{}", text(&o));
    assert!(home.join("runtime.json").exists());

    let o = blirp(&home, &user, &["logs", "-n", "50"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("listening"), "{}", text(&o));

    let o = blirp(&home, &user, &["stop"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("stopped"), "{}", text(&o));
    assert!(!home.join("runtime.json").exists());
    // The lock is free again: a new daemon could start.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .open(home.join("daemon.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    drop(lock);

    let o = blirp(&home, &user, &["stop"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("not running"), "{}", text(&o));
}

// The detached daemon must not inherit any handle of its caller: a caller
// reading a pipe it handed down (or `blirp daemon --detach | ...`) would
// otherwise wait until the daemon stops.
#[cfg(windows)]
#[test]
fn detached_daemon_holds_none_of_the_callers_handles() {
    use std::io::Read as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("blirp");
    let user = tmp.path().join("user");
    std::fs::create_dir_all(&user).unwrap();

    let (mut reader, writer) = std::io::pipe().unwrap();
    #[allow(unsafe_code)]
    // SAFETY: `writer` is an open pipe handle owned by this test; only its
    // inherit flag changes, so `blirp daemon --detach` receives a copy.
    let ok = unsafe {
        SetHandleInformation(
            writer.as_raw_handle(),
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        )
    };
    assert_ne!(ok, 0);
    let o = blirp(&home, &user, &["daemon", "--detach", "--port", "0"]);
    let _stop = StopOnDrop(&home, &user);
    assert!(o.status.success(), "{}", text(&o));
    drop(writer);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok(),
        "the detached daemon keeps the caller's pipe open"
    );
    let o = blirp(&home, &user, &["stop"]);
    assert!(o.status.success(), "{}", text(&o));
}

fn git(dir: &Path, args: &[&str]) {
    let o = blirp_core::process::command("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}: {o:?}");
}

#[tokio::test]
async fn worktrees_list_and_prune() {
    use blirp_core::model::{Session, SessionStatus};
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("blirp");
    let user = tmp.path().join("user");
    let repo = tmp.path().join("repo");
    for d in [&user, &repo] {
        std::fs::create_dir_all(d).unwrap();
    }
    git(&repo, &["init"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "init"]);

    let o = blirp(&home, &user, &["daemon", "--detach", "--port", "0"]);
    let _stop = StopOnDrop(&home, &user);
    assert!(o.status.success(), "{}", text(&o));
    let info = blirp_core::paths::RuntimeInfo::read(&blirp_core::paths::Paths::at(&home))
        .unwrap()
        .unwrap();
    blirp::install_crypto_provider();
    let http = reqwest::Client::new();
    let api = |method: reqwest::Method, path: &str| {
        http.request(method, format!("{}{path}", info.base_url()))
            .bearer_auth(&info.token)
    };
    let s: Session = api(reqwest::Method::POST, "/api/sessions")
        .json(&serde_json::json!({"cwd": repo, "agent": "shell", "worktree": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let wt = std::path::PathBuf::from(s.worktree.clone().unwrap());
    let r = api(
        reqwest::Method::POST,
        &format!("/api/sessions/{}/stop", s.id),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 202);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let cur: Session = api(reqwest::Method::GET, &format!("/api/sessions/{}", s.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if cur.status == SessionStatus::Completed {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "session never ended");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let o = blirp(&home, &user, &["worktrees", "list"]);
    assert!(o.status.success(), "{}", text(&o));
    let listed = text(&o);
    assert!(
        listed.contains(&s.id) && listed.contains("clean"),
        "{listed}"
    );

    // The terminal leaves the registry right after the exit is recorded.
    let mut pruned = String::new();
    for _ in 0..20 {
        let o = blirp(&home, &user, &["worktrees", "prune"]);
        assert!(o.status.success(), "{}", text(&o));
        pruned = text(&o);
        if pruned.contains("1 removed") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(pruned.contains("1 removed"), "{pruned}");
    assert!(!wt.exists());
    let o = blirp(&home, &user, &["worktrees", "list"]);
    assert!(text(&o).contains("no blirp worktrees"), "{}", text(&o));

    let o = blirp(&home, &user, &["stop"]);
    assert!(o.status.success(), "{}", text(&o));
}
