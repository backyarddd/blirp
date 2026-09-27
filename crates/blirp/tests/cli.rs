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

    // A second daemon for the same data dir exits 0 (so a supervisor does
    // not keep restarting it), and `start` leaves the running one alone.
    let o = blirp(&home, &user, &["daemon", "--port", "0"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("already running"), "{}", text(&o));
    let o = blirp(&home, &user, &["start"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("already running"), "{}", text(&o));

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

/// `blirp agents set-token claude` with the token piped in (no terminal):
/// stored trimmed, never echoed; refused when empty; `clear-token` removes it.
#[test]
fn set_and_clear_claude_token() {
    use std::io::Write as _;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("blirp");
    let user = tmp.path().join("user");
    std::fs::create_dir_all(&user).unwrap();
    let file = home.join("secrets").join("claude_oauth_token");
    let set = |input: &str| {
        let mut cmd = blirp_core::process::command(env!("CARGO_BIN_EXE_blirp"));
        cmd.args(["agents", "set-token", "claude"])
            .env("BLIRP_HOME", &home)
            .env("HOME", &user)
            .env("USERPROFILE", &user)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };

    let o = set("  \n");
    assert!(!o.status.success(), "{}", text(&o));
    assert!(!file.exists());

    let o = set("tok-cli-test-123\n");
    assert!(o.status.success(), "{}", text(&o));
    assert!(!text(&o).contains("tok-cli-test-123"), "{}", text(&o));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "tok-cli-test-123");

    let o = blirp(&home, &user, &["agents", "set-token", "codex"]);
    assert!(!o.status.success(), "only claude has a token");

    let o = blirp(&home, &user, &["agents", "clear-token", "claude"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(!file.exists());
}

/// A daemon started from inside a claude session blirp launched inherits
/// that session's copy of the stored login token. It must not treat the
/// copy as the user's own setting, nor hand it to other programs.
#[tokio::test]
async fn daemon_started_inside_claude_drops_the_inherited_token() {
    use blirp_core::model::AgentInfo;
    const SECRET: &str = "tok-inherited-copy-test";
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("blirp");
    let user = tmp.path().join("user");
    let work = tmp.path().join("work");
    for d in [&user, &work] {
        std::fs::create_dir_all(d).unwrap();
    }
    let paths = blirp_core::paths::Paths::at(&home);
    blirp_core::claude_token::store(&paths, SECRET).unwrap();

    let mut cmd = blirp_core::process::command(env!("CARGO_BIN_EXE_blirp"));
    cmd.args(["daemon", "--detach", "--port", "0"])
        .env("BLIRP_HOME", &home)
        .env("HOME", &user)
        .env("USERPROFILE", &user)
        .env("APPDATA", user.join("AppData"))
        .env(blirp_core::claude_token::ENV, SECRET);
    for v in INGEST_VARS {
        cmd.env_remove(v);
    }
    let o = cmd.output().unwrap();
    let _stop = StopOnDrop(&home, &user);
    assert!(o.status.success(), "{}", text(&o));
    let info = blirp_core::paths::RuntimeInfo::read(&paths)
        .unwrap()
        .unwrap();
    blirp::install_crypto_provider();
    let http = reqwest::Client::new();
    let api = |method: reqwest::Method, path: &str| {
        http.request(method, format!("{}{path}", info.base_url()))
            .bearer_auth(&info.token)
    };

    // Not reported as the environment's token: the stored one is in use.
    let agents: Vec<AgentInfo> = api(reqwest::Method::GET, "/api/agents")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = agents
        .into_iter()
        .find(|a| a.id == "claude")
        .and_then(|a| a.token)
        .unwrap();
    assert!(token.stored && !token.env, "{token:?}");

    // A shell session does not get it.
    let out = work.join("seen.txt");
    let prompt = if cfg!(windows) {
        format!(
            "Set-Content -LiteralPath '{}' -Value ('[' + $env:{} + ']')",
            out.display(),
            blirp_core::claude_token::ENV
        )
    } else {
        format!(
            "printf '[%s]' \"${}\" > '{}'",
            blirp_core::claude_token::ENV,
            out.display()
        )
    };
    let r = api(reqwest::Method::POST, "/api/sessions")
        .json(&serde_json::json!({"cwd": work, "agent": "shell", "prompt": prompt}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{:?}", r.text().await);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let seen = loop {
        if let Ok(s) = std::fs::read_to_string(&out)
            && s.contains(']')
        {
            break s;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the shell never ran the prompt"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    assert_eq!(seen.trim(), "[]");

    let o = blirp(&home, &user, &["stop"]);
    assert!(o.status.success(), "{}", text(&o));
}

/// `blirp update` against the releases API at `base`, never a real
/// release: the test binary has no install receipt, so it refuses to
/// replace itself. Returns the output and the last recorded outcome.
async fn update_run(
    home: &Path,
    user: &Path,
    base: &str,
    args: &[&str],
) -> (Output, blirp_core::model::UpdateOutcome) {
    let (home, user, base) = (home.to_path_buf(), user.to_path_buf(), base.to_string());
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let o = tokio::task::spawn_blocking(move || {
        let mut cmd = blirp_core::process::command(env!("CARGO_BIN_EXE_blirp"));
        cmd.arg("update")
            .args(&args)
            .env("BLIRP_HOME", &home)
            .env("HOME", &user)
            .env("USERPROFILE", &user)
            .env("APPDATA", user.join("AppData"))
            .env("BLIRP_RELEASE_BASE_URL", &base)
            .env_remove("GITHUB_TOKEN");
        for v in INGEST_VARS {
            cmd.env_remove(v);
        }
        let o = cmd.output().unwrap();
        (o, home)
    })
    .await
    .unwrap();
    let outcome = blirp::update::last_outcome(&blirp_core::paths::Paths::at(&o.1))
        .unwrap_or_else(|| panic!("no outcome recorded: {}", text(&o.0)));
    (o.0, outcome)
}

// Every run that may install records how it ended, also when it stops
// early (the UI's Update now waits for that record).
#[tokio::test]
async fn update_records_every_run() {
    let current = env!("CARGO_PKG_VERSION");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("blirp");
    let user = tmp.path().join("user");
    std::fs::create_dir_all(&user).unwrap();

    // Offline: fails before a release is known.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let offline = format!("http://{}/releases", closed.local_addr().unwrap());
    drop(closed);
    let (o, out) = update_run(&home, &user, &offline, &[]).await;
    assert!(!o.status.success(), "{}", text(&o));
    assert!(!out.ok && !out.installed && out.to.is_none(), "{out:?}");
    assert!(out.error.is_some_and(|e| e.contains("/releases/latest")));
    assert_eq!(out.from, current);

    let app = axum::Router::new()
        .route(
            "/releases/latest",
            axum::routing::get(|| async {
                let tag = concat!("v", env!("CARGO_PKG_VERSION"));
                axum::Json(serde_json::json!({ "tag_name": tag, "html_url": "", "assets": [] }))
            }),
        )
        .route(
            "/releases/tags/{tag}",
            axum::routing::get(
                |axum::extract::Path(tag): axum::extract::Path<String>| async move {
                    axum::Json(serde_json::json!({ "tag_name": tag, "html_url": "", "assets": [] }))
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/releases", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // Nothing to install: ok, nothing replaced.
    let (o, out) = update_run(&home, &user, &base, &[]).await;
    assert!(o.status.success(), "{}", text(&o));
    assert!(out.ok && !out.installed, "{out:?}");
    assert_eq!(out.to.as_deref(), Some(current));

    // Not a script install: refused before anything is downloaded.
    let (o, out) = update_run(&home, &user, &base, &["--version", "0.0.1"]).await;
    assert!(!o.status.success(), "{}", text(&o));
    assert!(!out.ok && !out.installed, "{out:?}");
    assert_eq!(out.to.as_deref(), Some("0.0.1"));
    assert!(
        out.error
            .is_some_and(|e| e.contains("not installed by the blirp install script"))
    );
}
