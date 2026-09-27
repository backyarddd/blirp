//! Always-on hubs on servers (docs/vps.md): `blirp hub setup`, `blirp
//! backup`, and the server lines of `blirp doctor`.

use super::Client;
use anyhow::Context as _;
use blirp_core::config::Config;
use blirp_core::model::{MachineRole, SyncStatus};
use blirp_core::paths::Paths;
use reqwest::Method;
use serde_json::{Value, json};
use std::path::Path;
use std::process::ExitCode;

const GUIDE: &str = "https://github.com/backyarddd/blirp/blob/main/docs/vps.md";

/// Why the hub refuses to run as root, and what to do instead.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) const ROOT_REFUSAL: &str = "\
refusing to set up the hub as root: sessions on the hub run as its user, \
and root would give every paired machine root on this server. Create a \
user for blirp and run this as that user, logged in over SSH as it:

    adduser blirp                       # or: useradd -m -s /bin/bash blirp
    loginctl enable-linger blirp        # keep its services running
    ssh blirp@<this server>             # after copying your SSH key

Details: docs/vps.md";

/// `blirp hub setup`: make this machine an always-on hub. Every step checks
/// first and is safe to repeat: autostart (on Linux with systemd linger),
/// a running daemon, LAN discovery, the hub role, then a fresh invite.
pub async fn hub_setup(paths: &Paths, lan: bool) -> anyhow::Result<ExitCode> {
    #[cfg(unix)]
    if super::service::is_root() {
        anyhow::bail!("{ROOT_REFUSAL}");
    }
    autostart(paths).await;
    super::lifecycle::start(paths).await?;
    let client = Client::connect(paths).await?;
    if set_lan_discovery(&client, lan).await? {
        println!(
            "LAN discovery (mDNS) turned {}.",
            if lan {
                "on"
            } else {
                "off: a server has no LAN peers to find (`--lan` keeps it on)"
            }
        );
    }
    println!();
    super::sync::hub(&client, super::sync::HubAction::Enable).await?;
    println!();
    print!("{}", next_steps());
    Ok(ExitCode::SUCCESS)
}

/// Install the autostart service where it can work, and say plainly what
/// happens where it cannot (containers, no user manager). Never fails the
/// setup: the daemon still starts directly afterwards.
async fn autostart(paths: &Paths) {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use super::service::platform;
        if !platform::systemd_booted() {
            println!(
                "systemd is not running here (a container?): the daemon starts now but not at \
                 boot. Start `blirp daemon` from your container's entrypoint or init system."
            );
            return;
        }
        let user = super::service::user_name();
        match platform::linger() {
            Some(true) => println!("Linger is on: the daemon runs without a login session."),
            _ if platform::enable_linger() => {
                println!("Linger enabled: the daemon runs without a login session.");
            }
            _ => println!(
                "warning: linger is off, so the daemon stops when you log out and does not \
                 start at boot. Run this once as an administrator, then `blirp hub setup` \
                 again:\n\n    {}\n",
                super::service::linger_command(&user)
            ),
        }
        if !platform::user_manager_ready(std::time::Duration::from_secs(10)).await {
            println!(
                "warning: your systemd user manager is not reachable (logged in with `su` or \
                 `sudo -u` without linger?), so no autostart service was installed. Log in \
                 over SSH as {user}, or enable linger, and run `blirp hub setup` again."
            );
            return;
        }
    }
    if let Err(e) = super::service::install(paths).await {
        println!("warning: autostart was not installed ({e:#}); starting the daemon directly.");
    }
}

/// Set `sync.lan_discovery` through the daemon (which applies it live).
/// True when it changed.
async fn set_lan_discovery(client: &Client, on: bool) -> anyhow::Result<bool> {
    let view: Value = client.get("/api/settings").await?;
    let base = view
        .get("config")
        .cloned()
        .context("GET /api/settings returned no config")?;
    if base["sync"]["lan_discovery"] == json!(on) {
        return Ok(false);
    }
    let mut config = base.clone();
    config["sync"]["lan_discovery"] = json!(on);
    client
        .send(
            Method::PATCH,
            "/api/settings",
            Some(json!({"config": config, "base": base})),
        )
        .await?;
    Ok(true)
}

fn next_steps() -> String {
    format!(
        "\
Next steps
  1. On your PC (blirp installed there too), run the `blirp pair` line above,
     or use Settings > Machines & Sync > Join a hub. `blirp hub invite` here
     makes a new invite when this one expires.
  2. For cloud sessions, install the agent CLIs on this server and log them in
     without a browser here:
       claude: `claude setup-token` on any machine with a browser, then
               `blirp agents set-token claude` here
       codex:  `codex login --device-auth`
     then run `blirp hub setup` again so the service sees them on its PATH.
  3. Check everything with `blirp doctor`.
Guide: {GUIDE}
"
    )
}

/// `blirp backup <FILE>`: a consistent copy of the database while the
/// daemon runs (SQLite `VACUUM INTO`), readable only by this user.
pub fn backup(paths: &Paths, dest: &Path) -> anyhow::Result<ExitCode> {
    let store = blirp_core::store::Store::open_read_only(&paths.db_file())
        .with_context(|| format!("open {}", paths.db_file().display()))?;
    create_private(dest)?;
    if let Err(e) = store.backup_to(dest) {
        // Only the empty file this command created; never a user's file.
        let _ = std::fs::remove_file(dest);
        return Err(e).with_context(|| format!("back up to {}", dest.display()));
    }
    let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    println!(
        "Backed up {} to {} ({:.1} MB).",
        paths.db_file().display(),
        dest.display(),
        size as f64 / 1_000_000.0
    );
    println!(
        "Keep {} and {} with it: paired machines trust this machine's identity key.",
        paths.home().join("identity.key").display(),
        paths.config_file().display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Create `path` empty with mode 0600 (unix); an existing file is an error,
/// so a backup never overwrites anything.
fn create_private(path: &Path) -> anyhow::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)
        .map(drop)
        .with_context(|| format!("create {} (it must not exist yet)", path.display()))
}

/// Doctor lines for a machine that serves others: role, autostart, linger
/// (Linux) and the relay. `[warn]` marks what breaks an always-on hub but
/// is fine on a desktop; it does not fail the check.
pub async fn doctor_lines(config: &Config, client: Option<&Client>) -> Vec<String> {
    let role = config.sync.role;
    let hub = role == MachineRole::Hub;
    let mut lines = vec![format!("[info] role: {role}")];

    let service = super::service::installed();
    lines.push(match &service {
        Ok(Some(detail)) => format!("[info] autostart: {detail}"),
        Ok(None) if hub => "[warn] autostart: not installed; the hub does not start at boot \
                            (`blirp hub setup`)"
            .into(),
        Ok(None) => "[info] autostart: not installed".into(),
        Err(e) => format!("[info] autostart: unknown ({e:#})"),
    });

    #[cfg(all(unix, not(target_os = "macos")))]
    lines.push(match super::service::platform::linger() {
        Some(true) => "[info] linger: on (services run without a login session)".into(),
        Some(false) if hub => format!(
            "[warn] linger: off; the hub stops when you log out and does not start at boot \
             (`{}`)",
            super::service::linger_command(&super::service::user_name())
        ),
        Some(false) => "[info] linger: off".into(),
        None => "[info] linger: unknown (no loginctl)".into(),
    });

    if role != MachineRole::Standalone {
        let status = match client {
            Some(c) => c.get::<SyncStatus>("/api/sync/status").await.ok(),
            None => None,
        };
        lines.push(relay_line(
            &config.sync.relay,
            blirp_sync::loopback_only(),
            status.as_ref(),
        ));
    }
    lines
}

/// Whether machines on other networks can reach this one through a relay.
fn relay_line(setting: &str, loopback: bool, status: Option<&SyncStatus>) -> String {
    if loopback {
        return "[info] relay: off (BLIRP_LOOPBACK_ONLY=1)".into();
    }
    if setting == "disabled" {
        return "[info] relay: disabled (sync.relay); only direct connections".into();
    }
    match status {
        None => "[info] relay: unknown (daemon not running)".into(),
        Some(s) => match &s.relay_url {
            Some(url) => format!("[ ok ] relay: connected ({url})"),
            None if !s.connected => "[warn] relay: sync is not running".into(),
            None => "[warn] relay: not connected yet; machines on other networks may not reach \
                     this one. Check outbound UDP and HTTPS (443), then `blirp logs`"
                .into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(connected: bool, relay: Option<&str>) -> SyncStatus {
        SyncStatus {
            role: MachineRole::Hub,
            machine_id: "m".into(),
            hub: Some("m".into()),
            connected,
            last_sync_at: None,
            pending_outbox: 0,
            portal_url: None,
            portal_cert_fingerprint: None,
            relay_url: relay.map(str::to_string),
        }
    }

    #[test]
    fn relay_lines() {
        let up = status(true, Some("https://relay.example./"));
        assert!(relay_line("default", true, Some(&up)).contains("BLIRP_LOOPBACK_ONLY"));
        assert!(relay_line("disabled", false, Some(&up)).starts_with("[info] relay: disabled"));
        assert_eq!(
            relay_line("default", false, Some(&up)),
            "[ ok ] relay: connected (https://relay.example./)"
        );
        assert!(relay_line("default", false, None).contains("daemon not running"));
        assert!(relay_line("default", false, Some(&status(true, None))).starts_with("[warn]"));
        assert!(relay_line("default", false, Some(&status(false, None))).contains("not running"));
    }

    #[test]
    fn next_steps_cover_pairing_and_headless_logins() {
        let s = next_steps();
        for needle in [
            "blirp pair",
            "blirp hub invite",
            "claude setup-token",
            "blirp agents set-token claude",
            "codex login --device-auth",
            "blirp hub setup",
            "docs/vps.md",
        ] {
            assert!(s.contains(needle), "missing {needle}");
        }
        assert!(ROOT_REFUSAL.contains("loginctl enable-linger blirp"));
    }

    #[test]
    fn backup_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("b.db");
        create_private(&f).unwrap();
        assert!(create_private(&f).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&f).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
