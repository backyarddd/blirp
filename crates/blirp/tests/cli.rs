//! The `blirp` binary's daemon lifecycle commands, end to end: `daemon
//! --detach`, `logs`, `stop`, against a temp BLIRP_HOME and a fake user home
//! (so ingest never reads the real agent stores).

// Test helpers panic on failure by design.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output};

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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_blirp"));
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
