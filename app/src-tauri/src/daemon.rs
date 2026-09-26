//! Find, start and stop the `blirp` daemon (§4, §15).

use anyhow::{Context as _, bail};
use blirp_core::paths::{Paths, RuntimeInfo};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
/// `blirp daemon --detach` itself waits up to 20 s for health.
const DETACH_TIMEOUT: Duration = Duration::from_secs(40);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

fn http() -> anyhow::Result<reqwest::Client> {
    // Loopback only: never route through a configured HTTP proxy.
    Ok(reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .no_proxy()
        .build()?)
}

/// runtime.json + a successful authenticated `/api/health`.
pub async fn probe(paths: &Paths) -> Option<RuntimeInfo> {
    let info = match RuntimeInfo::read(paths) {
        Ok(i) => i?,
        Err(e) => {
            tracing::warn!(error = %e, "unreadable runtime.json");
            return None;
        }
    };
    let ok = http()
        .ok()?
        .get(format!("{}/api/health", info.base_url()))
        .bearer_auth(&info.token)
        .send()
        .await
        .is_ok_and(|r| r.status().is_success());
    ok.then_some(info)
}

/// The `blirp` binary: the bundled sidecar next to this executable. In a
/// development build that is the workspace's `target/debug/blirp` (the dev
/// command builds it first).
pub fn sidecar() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("locate the blirp desktop executable")?;
    let dir = exe
        .parent()
        .context("the blirp desktop executable has no parent directory")?;
    let bin = dir.join(format!("blirp{}", std::env::consts::EXE_SUFFIX));
    if !bin.is_file() {
        bail!(
            "the blirp binary is missing: expected {}. Reinstall blirp{}.",
            bin.display(),
            if cfg!(debug_assertions) {
                " or run `cargo build -p blirp`"
            } else {
                ""
            }
        );
    }
    Ok(bin)
}

/// Make sure a healthy daemon is running and return how to reach it.
pub async fn ensure(paths: &Paths) -> anyhow::Result<RuntimeInfo> {
    if let Some(info) = probe(paths).await {
        return Ok(info);
    }
    let bin = sidecar()?;
    tracing::info!(bin = %bin.display(), "starting the blirp daemon");
    // stderr goes to a file, not a pipe: on Windows the daemon that --detach
    // spawns inherits every inheritable handle, so a pipe would never reach
    // EOF while the daemon runs.
    let err_path = paths.logs_dir().join("desktop-daemon-start.log");
    let err_file = std::fs::File::create(&err_path)
        .with_context(|| format!("create {}", err_path.display()))?;
    // No console window flash for the short-lived --detach process.
    let mut cmd = tokio::process::Command::from(blirp_core::process::command(&bin));
    cmd.args(["daemon", "--detach"])
        .env(blirp_core::paths::HOME_ENV, paths.home())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(err_file)
        .kill_on_drop(true);
    #[cfg(unix)]
    if let Some(path) = login_shell_path().await {
        cmd.env("PATH", path);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("run {}", bin.display()))?;
    let status = tokio::time::timeout(DETACH_TIMEOUT, child.wait())
        .await
        .context("the daemon did not start in time")?
        .with_context(|| format!("wait for {}", bin.display()))?;
    if !status.success() {
        let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
        let detail = stderr.trim();
        bail!(
            "the daemon failed to start ({status}){}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }
    // --detach returned after health succeeded; confirm with our own probe.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(info) = probe(paths).await {
            return Ok(info);
        }
        if Instant::now() > deadline {
            bail!("the daemon started but does not answer /api/health");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Ask the daemon to shut down (ends live sessions) and wait until it is gone.
/// Returns false when no daemon was running.
pub async fn stop(paths: &Paths) -> anyhow::Result<bool> {
    let Some(info) = probe(paths).await else {
        return Ok(false);
    };
    let resp = http()?
        .post(format!("{}/api/daemon/shutdown", info.base_url()))
        .bearer_auth(&info.token)
        .send()
        .await
        .context("request daemon shutdown")?;
    if !resp.status().is_success() {
        bail!("the daemon refused to shut down: {}", resp.status());
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    while probe(paths).await.is_some() {
        if Instant::now() > deadline {
            bail!(
                "the daemon is still running after {}s",
                STOP_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(true)
}

/// Apps started from Finder / a desktop launcher get a minimal PATH that hides
/// the user's agent CLIs (npm globals, Homebrew, ~/.local/bin). Ask the login
/// shell for the real one; fall back to the inherited PATH.
#[cfg(unix)]
async fn login_shell_path() -> Option<String> {
    const MARK: &str = "__BLIRP_PATH__=";
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "/bin/zsh".into()
            } else {
                "/bin/sh".into()
            }
        });
    let mut cmd = tokio::process::Command::from(blirp_core::process::command(&shell));
    cmd.args(["-ilc", &format!("printf '\\n{MARK}%s\\n' \"$PATH\"")])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(5), cmd.output()).await {
        Ok(Ok(out)) => {
            let text = String::from_utf8_lossy(&out.stdout);
            let path = text
                .lines()
                .find_map(|l| l.strip_prefix(MARK))
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string);
            if path.is_none() {
                tracing::warn!(%shell, "login shell did not report PATH");
            }
            path
        }
        Ok(Err(e)) => {
            tracing::warn!(%shell, error = %e, "cannot run login shell for PATH");
            None
        }
        Err(_) => {
            tracing::warn!(%shell, "login shell timed out reporting PATH");
            None
        }
    }
}
