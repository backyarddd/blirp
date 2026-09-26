//! `blirp service install|uninstall|status` (§4): per-user autostart of the
//! daemon. macOS LaunchAgent, Linux `systemd --user` unit, Windows HKCU Run
//! key. Never a system service: the daemon must see the user's agent logins.
//!
//! Install is idempotent and uninstall removes exactly what install created.
//! The file/command renderers are pure so their output is unit tested on
//! every platform.

use anyhow::{Context as _, bail};
use blirp_core::paths::Paths;
use clap::Subcommand;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum ServiceCommand {
    /// Start the daemon automatically when you log in (and now).
    Install,
    /// Remove the autostart entry. A running daemon keeps running.
    Uninstall,
    /// Show whether autostart is installed and the daemon is running.
    Status,
}

pub const LAUNCHD_LABEL: &str = "dev.blirp.daemon";
pub const SYSTEMD_UNIT: &str = "blirp.service";
pub const RUN_VALUE: &str = "blirp";

pub async fn run(cmd: &ServiceCommand, paths: &Paths) -> anyhow::Result<ExitCode> {
    match cmd {
        ServiceCommand::Install => install(paths).await,
        ServiceCommand::Uninstall => uninstall(),
        ServiceCommand::Status => status(paths).await,
    }
}

/// The binary autostart will run: this one, unless it lives somewhere that
/// will not exist after a reboot.
fn service_exe() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("locate the blirp executable")?;
    if std::env::var_os("APPIMAGE").is_some() || exe.starts_with("/tmp") {
        bail!(
            "this blirp runs from a temporary location ({}); install the standalone \
             blirp binary (see docs/install.md) and run `blirp service install` with it",
            exe.display()
        );
    }
    Ok(exe)
}

/// `BLIRP_HOME` baked into the unit when set explicitly, so the service uses
/// the same data dir as the shell that installed it.
fn explicit_home(paths: &Paths) -> Option<PathBuf> {
    std::env::var_os(blirp_core::paths::HOME_ENV)
        .filter(|v| !v.is_empty())
        .map(|_| paths.home().to_path_buf())
}

/// launchd and systemd start services with a minimal PATH, which would hide
/// the user's agent CLIs (npm globals, Homebrew, ~/.local/bin). Capture the
/// installing shell's PATH; re-run `blirp service install` after changing it.
#[cfg_attr(windows, allow(dead_code))]
fn captured_path() -> Option<String> {
    std::env::var("PATH").ok().filter(|p| !p.is_empty())
}

// ---------------------------------------------------------------- renderers

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// macOS LaunchAgent. `KeepAlive.SuccessfulExit = false` restarts the daemon
/// after a crash but not after a deliberate stop (tray Quit exits 0).
pub fn launch_agent_plist(
    exe: &Path,
    log_dir: &Path,
    home: Option<&Path>,
    path_env: Option<&str>,
) -> String {
    let mut env = String::new();
    for (k, v) in [
        ("BLIRP_HOME", home.map(|h| h.display().to_string())),
        ("PATH", path_env.map(str::to_string)),
    ] {
        if let Some(v) = v {
            // Writing to a String cannot fail.
            let _ = write!(
                env,
                "\n    <key>{k}</key>\n    <string>{}</string>",
                xml_escape(&v)
            );
        }
    }
    let env = if env.is_empty() {
        String::new()
    } else {
        format!("\n  <key>EnvironmentVariables</key>\n  <dict>{env}\n  </dict>")
    };
    let log = xml_escape(&log_dir.join("launchd.log").display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>daemon</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Interactive</string>{env}
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        exe = xml_escape(&exe.display().to_string()),
    )
}

/// Quote a value for a systemd unit: `%` starts a specifier, `\` and `"`
/// are escapes inside double quotes.
fn systemd_quote(s: &str) -> String {
    let inner = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{inner}\"")
}

/// Linux `systemd --user` unit. `Restart=on-failure`: a deliberate stop
/// (exit 0) stays stopped, a crash restarts.
pub fn systemd_unit(exe: &Path, home: Option<&Path>, path_env: Option<&str>) -> String {
    let mut env = String::new();
    for (k, v) in [
        ("BLIRP_HOME", home.map(|h| h.display().to_string())),
        ("PATH", path_env.map(str::to_string)),
    ] {
        if let Some(v) = v {
            // Writing to a String cannot fail.
            let _ = writeln!(env, "Environment={}", systemd_quote(&format!("{k}={v}")));
        }
    }
    format!(
        "[Unit]
Description=blirp daemon (workspace and memory for CLI coding agents)
Documentation=https://github.com/backyarddd/blirp

[Service]
Type=simple
ExecStart={exe} daemon
{env}Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
",
        exe = systemd_quote(&exe.display().to_string()),
    )
}

/// Windows Run-key command. `conhost --headless` hosts the console binary
/// without a window, so nothing flashes at login; `--detach` returns once
/// the daemon is healthy.
pub fn windows_run_command(system_root: &Path, exe: &Path) -> String {
    format!(
        "\"{}\\System32\\conhost.exe\" --headless \"{}\" daemon --detach",
        system_root.display(),
        exe.display()
    )
}

// ------------------------------------------------------------------ helpers

/// Write `contents` unless the file already has them. Returns whether it changed.
#[cfg_attr(windows, allow(dead_code))]
fn write_if_changed(path: &Path, contents: &str) -> anyhow::Result<bool> {
    if std::fs::read_to_string(path).is_ok_and(|c| c == contents) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

#[cfg_attr(windows, allow(dead_code))]
fn remove_if_exists(path: &Path) -> anyhow::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

/// Run a helper tool; returns (success, combined output).
fn tool(program: &str, args: &[&str]) -> anyhow::Result<(bool, String)> {
    let out = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("run {program}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text.trim().to_string()))
}

fn must(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let (ok, out) = tool(program, args)?;
    if !ok {
        bail!("`{program} {}` failed: {out}", args.join(" "));
    }
    Ok(())
}

#[cfg_attr(windows, allow(dead_code))]
fn user_home() -> anyhow::Result<PathBuf> {
    blirp_core::paths::user_home().context("cannot determine the home directory")
}

// ------------------------------------------------------------------ install

async fn install(paths: &Paths) -> anyhow::Result<ExitCode> {
    let exe = service_exe()?;
    paths.ensure_dirs()?;
    let running = crate::daemon::running_daemon(paths).await;
    platform::install(paths, &exe, running.as_ref().map(|r| r.pid)).await?;
    Ok(ExitCode::SUCCESS)
}

/// What removing the autostart entry did to a running daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Removed {
    NotInstalled,
    /// The entry is gone; a running daemon was left alone.
    DaemonKept,
    /// Unloading the service stopped its daemon (macOS `launchctl bootout`).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    DaemonStopped,
}

fn uninstall_message(r: Removed) -> &'static str {
    match r {
        Removed::NotInstalled => "Autostart was not installed.",
        Removed::DaemonKept => "Autostart removed. A running daemon keeps running.",
        Removed::DaemonStopped => {
            "Autostart removed. Unloading it stopped the daemon it had started \
             (live sessions ended as detached); run `blirp daemon --detach` to start one."
        }
    }
}

pub(crate) fn uninstall() -> anyhow::Result<ExitCode> {
    println!("{}", uninstall_message(platform::uninstall()?));
    Ok(ExitCode::SUCCESS)
}

/// Start the daemon through the autostart service, if one is installed
/// (`blirp update` restarting a stopped daemon). False when there is none.
pub(crate) fn start_managed() -> anyhow::Result<bool> {
    platform::start_managed()
}

async fn status(paths: &Paths) -> anyhow::Result<ExitCode> {
    let installed = platform::status()?;
    match &installed {
        Some(detail) => println!("autostart: installed\n  {detail}"),
        None => println!("autostart: not installed (run `blirp service install`)"),
    }
    match crate::daemon::running_daemon(paths).await {
        Some(info) => println!("daemon:    running (pid {}, {})", info.pid, info.base_url()),
        None => println!("daemon:    not running"),
    }
    Ok(if installed.is_some() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn plist_path() -> anyhow::Result<PathBuf> {
        Ok(user_home()?
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist")))
    }

    fn domain() -> String {
        #[allow(unsafe_code)]
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        format!("gui/{uid}")
    }

    fn loaded() -> anyhow::Result<bool> {
        Ok(tool(
            "launchctl",
            &["print", &format!("{}/{LAUNCHD_LABEL}", domain())],
        )?
        .0)
    }

    pub async fn install(paths: &Paths, exe: &Path, running: Option<u32>) -> anyhow::Result<()> {
        let plist = plist_path()?;
        let contents = launch_agent_plist(
            exe,
            &paths.logs_dir(),
            explicit_home(paths).as_deref(),
            captured_path().as_deref(),
        );
        let changed = write_if_changed(&plist, &contents)?;
        let plist_s = plist.display().to_string();
        match (loaded()?, changed, running) {
            (true, false, _) => println!("Autostart already installed ({plist_s})."),
            (true, true, _) => {
                // Reload so launchd picks up the new definition; this restarts
                // the service's daemon (live sessions end as detached).
                must(
                    "launchctl",
                    &["bootout", &format!("{}/{LAUNCHD_LABEL}", domain())],
                )?;
                must("launchctl", &["bootstrap", &domain(), &plist_s])?;
                println!("Autostart updated and reloaded ({plist_s}).");
            }
            (false, _, Some(pid)) => println!(
                "Autostart installed ({plist_s}). A daemon is already running (pid {pid}); \
                 the service takes over at your next login."
            ),
            // Over SSH there is no GUI session to bootstrap into; the plist
            // still loads at the next login, so this is not a failure.
            (false, _, None) => match tool("launchctl", &["bootstrap", &domain(), &plist_s])? {
                (true, _) => println!("Autostart installed and started ({plist_s})."),
                (false, out) => println!(
                    "Autostart installed ({plist_s}); it starts at your next login. \
                     launchctl could not start it now ({out}); run `blirp daemon --detach` \
                     to start the daemon meanwhile."
                ),
            },
        }
        Ok(())
    }

    pub fn uninstall() -> anyhow::Result<Removed> {
        let plist = plist_path()?;
        // bootout also stops the service's running daemon.
        let stopped = if loaded()? {
            must(
                "launchctl",
                &["bootout", &format!("{}/{LAUNCHD_LABEL}", domain())],
            )?;
            true
        } else {
            false
        };
        Ok(match (remove_if_exists(&plist)?, stopped) {
            (_, true) => Removed::DaemonStopped,
            (true, false) => Removed::DaemonKept,
            (false, false) => Removed::NotInstalled,
        })
    }

    pub fn start_managed() -> anyhow::Result<bool> {
        if !loaded()? {
            return Ok(false);
        }
        must(
            "launchctl",
            &["kickstart", &format!("{}/{LAUNCHD_LABEL}", domain())],
        )?;
        Ok(true)
    }

    pub fn status() -> anyhow::Result<Option<String>> {
        let plist = plist_path()?;
        if !plist.exists() {
            return Ok(None);
        }
        let state = if loaded()? {
            "loaded"
        } else {
            "not loaded until next login"
        };
        Ok(Some(format!("{} ({state})", plist.display())))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::*;

    fn unit_path() -> anyhow::Result<PathBuf> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .map_or_else(|| user_home().map(|h| h.join(".config")), Ok)?;
        Ok(config.join("systemd/user").join(SYSTEMD_UNIT))
    }

    fn systemctl(args: &[&str]) -> anyhow::Result<()> {
        let mut all = vec!["--user"];
        all.extend_from_slice(args);
        must("systemctl", &all).context(
            "systemd --user is not available; start `blirp daemon --detach` from your \
             session startup instead",
        )
    }

    pub async fn install(paths: &Paths, exe: &Path, running: Option<u32>) -> anyhow::Result<()> {
        let unit = unit_path()?;
        let contents = systemd_unit(
            exe,
            explicit_home(paths).as_deref(),
            captured_path().as_deref(),
        );
        let changed = write_if_changed(&unit, &contents)?;
        systemctl(&["daemon-reload"])?;
        let active = tool(
            "systemctl",
            &["--user", "is-active", "--quiet", SYSTEMD_UNIT],
        )?
        .0;
        let unit_s = unit.display();
        if active {
            systemctl(&["enable", SYSTEMD_UNIT])?;
            if changed {
                systemctl(&["restart", SYSTEMD_UNIT])?;
                println!("Autostart updated and restarted ({unit_s}).");
            } else {
                println!("Autostart already installed ({unit_s}).");
            }
        } else if let Some(pid) = running {
            systemctl(&["enable", SYSTEMD_UNIT])?;
            println!(
                "Autostart installed ({unit_s}). A daemon is already running (pid {pid}); \
                 the service takes over at your next login."
            );
        } else {
            systemctl(&["enable", "--now", SYSTEMD_UNIT])?;
            println!("Autostart installed and started ({unit_s}).");
        }
        println!(
            "On a headless machine, also run `loginctl enable-linger $USER` so the \
             daemon runs without an open login session."
        );
        Ok(())
    }

    pub fn uninstall() -> anyhow::Result<Removed> {
        let unit = unit_path()?;
        if !unit.exists() {
            return Ok(Removed::NotInstalled);
        }
        // `disable` alone keeps a running daemon alive, matching Windows/macOS
        // where removing autostart does not stop the daemon either.
        systemctl(&["disable", SYSTEMD_UNIT])?;
        remove_if_exists(&unit)?;
        systemctl(&["daemon-reload"])?;
        Ok(Removed::DaemonKept)
    }

    pub fn start_managed() -> anyhow::Result<bool> {
        if !unit_path()?.exists() {
            return Ok(false);
        }
        let enabled = tool(
            "systemctl",
            &["--user", "is-enabled", "--quiet", SYSTEMD_UNIT],
        )
        .is_ok_and(|(ok, _)| ok);
        if !enabled {
            return Ok(false);
        }
        systemctl(&["start", SYSTEMD_UNIT])?;
        Ok(true)
    }

    pub fn status() -> anyhow::Result<Option<String>> {
        let unit = unit_path()?;
        if !unit.exists() {
            return Ok(None);
        }
        let (_, enabled) = tool("systemctl", &["--user", "is-enabled", SYSTEMD_UNIT])?;
        let (_, active) = tool("systemctl", &["--user", "is-active", SYSTEMD_UNIT])?;
        Ok(Some(format!("{} ({enabled}, {active})", unit.display())))
    }
}

#[cfg(windows)]
mod platform {
    use super::*;

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

    fn system_root() -> PathBuf {
        std::env::var_os("SystemRoot")
            .filter(|v| !v.is_empty())
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from)
    }

    fn reg() -> String {
        system_root()
            .join("System32")
            .join("reg.exe")
            .display()
            .to_string()
    }

    /// Current Run value, if any.
    fn current() -> anyhow::Result<Option<String>> {
        let (ok, out) = tool(&reg(), &["query", RUN_KEY, "/v", RUN_VALUE])?;
        if !ok {
            // reg.exe exits 1 when the value does not exist.
            return Ok(None);
        }
        Ok(out
            .lines()
            .find_map(|l| l.trim().strip_prefix(RUN_VALUE))
            .and_then(|rest| rest.trim_start().strip_prefix("REG_SZ"))
            .map(|v| v.trim().to_string()))
    }

    pub async fn install(paths: &Paths, exe: &Path, running: Option<u32>) -> anyhow::Result<()> {
        if explicit_home(paths).is_some() {
            println!(
                "note: BLIRP_HOME is set in this shell; autostart uses it only if it is \
                 also set as a user environment variable."
            );
        }
        let cmd = windows_run_command(&system_root(), exe);
        if current()?.as_deref() == Some(cmd.as_str()) {
            println!("Autostart already installed ({RUN_KEY}\\{RUN_VALUE}).");
        } else {
            must(
                &reg(),
                &[
                    "add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &cmd, "/f",
                ],
            )?;
            println!("Autostart installed ({RUN_KEY}\\{RUN_VALUE}).");
        }
        if running.is_none() {
            let info = crate::daemon::detach(paths, None).await?;
            println!("Daemon started (pid {}, port {}).", info.pid, info.port);
        }
        Ok(())
    }

    pub fn uninstall() -> anyhow::Result<Removed> {
        if current()?.is_none() {
            return Ok(Removed::NotInstalled);
        }
        must(&reg(), &["delete", RUN_KEY, "/v", RUN_VALUE, "/f"])?;
        Ok(Removed::DaemonKept)
    }

    /// The Run entry only acts at login; the caller starts the daemon.
    pub fn start_managed() -> anyhow::Result<bool> {
        Ok(false)
    }

    pub fn status() -> anyhow::Result<Option<String>> {
        Ok(current()?.map(|cmd| format!("{RUN_KEY}\\{RUN_VALUE} = {cmd}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninstall_messages_match_what_happened() {
        assert!(uninstall_message(Removed::DaemonKept).contains("keeps running"));
        let stopped = uninstall_message(Removed::DaemonStopped);
        assert!(stopped.contains("stopped") && !stopped.contains("keeps running"));
        assert!(uninstall_message(Removed::NotInstalled).contains("not installed"));
    }

    #[test]
    fn plist_contents() {
        let p = launch_agent_plist(
            Path::new("/opt/homebrew/bin/blirp"),
            Path::new("/Users/a b/.blirp/logs"),
            None,
            Some("/opt/homebrew/bin:/usr/bin:/bin"),
        );
        assert!(p.starts_with("<?xml version=\"1.0\""));
        assert!(p.contains("<string>dev.blirp.daemon</string>"));
        assert!(p.contains(
            "<array>\n    <string>/opt/homebrew/bin/blirp</string>\n    <string>daemon</string>\n  </array>"
        ));
        assert!(p.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(p.contains("<key>SuccessfulExit</key>\n    <false/>"));
        assert!(
            p.contains("<key>PATH</key>\n    <string>/opt/homebrew/bin:/usr/bin:/bin</string>")
        );
        assert!(!p.contains("BLIRP_HOME"));
        let log = Path::new("/Users/a b/.blirp/logs").join("launchd.log");
        assert!(p.contains(&format!("<string>{}</string>", log.display())));
        // Exactly one of each top-level structure.
        assert_eq!(p.matches("<dict>").count(), 3);
        assert_eq!(p.matches("</dict>").count(), 3);
    }

    #[test]
    fn plist_escapes_and_home() {
        let p = launch_agent_plist(
            Path::new("/Apps/R&D <x>/blirp"),
            Path::new("/l"),
            Some(Path::new("/data/blirp")),
            None,
        );
        assert!(p.contains("<string>/Apps/R&amp;D &lt;x&gt;/blirp</string>"));
        assert!(p.contains("<key>BLIRP_HOME</key>\n    <string>/data/blirp</string>"));
        assert!(!p.contains("<key>PATH</key>"));
        // No environment at all: no EnvironmentVariables dict.
        let bare = launch_agent_plist(Path::new("/b"), Path::new("/l"), None, None);
        assert!(!bare.contains("EnvironmentVariables"));
    }

    #[test]
    fn systemd_unit_contents() {
        let u = systemd_unit(
            Path::new("/home/me/.local/bin/blirp"),
            Some(Path::new("/srv/blirp 100%")),
            Some("/usr/local/bin:/usr/bin"),
        );
        assert_eq!(
            u,
            "[Unit]
Description=blirp daemon (workspace and memory for CLI coding agents)
Documentation=https://github.com/backyarddd/blirp

[Service]
Type=simple
ExecStart=\"/home/me/.local/bin/blirp\" daemon
Environment=\"BLIRP_HOME=/srv/blirp 100%%\"
Environment=\"PATH=/usr/local/bin:/usr/bin\"
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"
        );
        let bare = systemd_unit(Path::new("/b"), None, None);
        assert!(!bare.contains("Environment="));
        assert!(bare.contains("ExecStart=\"/b\" daemon\nRestart=on-failure"));
    }

    #[test]
    fn systemd_quoting() {
        assert_eq!(systemd_quote(r#"a"b\c%d"#), r#""a\"b\\c%%d""#);
    }

    #[test]
    fn windows_command() {
        assert_eq!(
            windows_run_command(
                Path::new(r"C:\Windows"),
                Path::new(r"C:\Users\me\AppData\Local\blirp\blirp.exe")
            ),
            r#""C:\Windows\System32\conhost.exe" --headless "C:\Users\me\AppData\Local\blirp\blirp.exe" daemon --detach"#
        );
    }

    #[test]
    fn write_if_changed_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sub/unit");
        assert!(write_if_changed(&f, "a").unwrap());
        assert!(!write_if_changed(&f, "a").unwrap());
        assert!(write_if_changed(&f, "b").unwrap());
        assert!(remove_if_exists(&f).unwrap());
        assert!(!remove_if_exists(&f).unwrap());
    }
}
