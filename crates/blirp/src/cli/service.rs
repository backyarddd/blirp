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
    let mut cmd = blirp_core::process::command(program);
    cmd.args(args);
    run_tool(cmd, program)
}

fn run_tool(mut cmd: std::process::Command, program: &str) -> anyhow::Result<(bool, String)> {
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("run {program}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text.trim().to_string()))
}

#[cfg_attr(all(unix, not(target_os = "macos")), allow(dead_code))]
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

/// `sudo loginctl enable-linger <user>`: the command an administrator runs
/// so this user's services run without a login session and start at boot.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
pub(crate) fn linger_command(user: &str) -> String {
    format!("sudo loginctl enable-linger {user}")
}

/// `loginctl show-user <uid> --property=Linger --value` -> lingering?
/// logind forgets a user without sessions or linger ("not logged in or
/// lingering"), which means off. None: logind could not be asked.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn parse_linger(ok: bool, out: &str) -> Option<bool> {
    match (ok, out.trim()) {
        (true, "yes") => Some(true),
        (true, "no") => Some(false),
        (false, o) if o.contains("not logged in or lingering") => Some(false),
        _ => None,
    }
}

/// The user's name for printed commands: `$USER`, `$LOGNAME`, else the
/// numeric uid (loginctl accepts both).
#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn user_name() -> String {
    ["USER", "LOGNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| uid().to_string())
}

#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Whether this process runs as root (effective uid 0).
#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

// ------------------------------------------------------------------ install

pub(crate) async fn install(paths: &Paths) -> anyhow::Result<ExitCode> {
    let exe = std::env::current_exe().context("locate the blirp executable")?;
    // Autostart must not point into a mount that is gone after a reboot.
    let exe = crate::memory::persistent_exe(exe)?;
    paths.ensure_dirs()?;
    let running = crate::daemon::running_daemon(paths).await;
    platform::install(paths, &exe, running.as_ref().map(|r| r.pid)).await?;
    Ok(ExitCode::SUCCESS)
}

/// What removing the autostart entry did to a running daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Removed {
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

/// Start the daemon through the autostart service, if one is installed for
/// this data dir (`blirp start`). False when there is none, or it serves
/// another data dir.
pub(crate) fn start_managed(paths: &Paths) -> anyhow::Result<bool> {
    platform::start_managed(paths.home())
}

/// Whether an installed launchd/systemd definition runs the daemon for
/// `home`: the `BLIRP_HOME` it sets (`entry`: that setting for `home`,
/// rendered as install writes it), else the default data dir. Keeps a shell
/// with another `BLIRP_HOME` (a test or second setup) from starting the
/// service's daemon instead of its own.
#[cfg_attr(windows, allow(dead_code))]
fn serves_home(definition: &str, entry: &str, home: &Path, default_home: Option<&Path>) -> bool {
    if definition.contains("BLIRP_HOME") {
        definition.contains(entry)
    } else {
        default_home == Some(home)
    }
}

/// Whether starting through this launchd/systemd definition can work: it
/// serves `home` (`serves_home`) and the binary it runs still exists. A
/// definition left behind by a moved or deleted binary would "start" and
/// then fail at once, so the caller starts the daemon directly instead.
#[cfg_attr(windows, allow(dead_code))]
fn usable(definition: &str, exe: Option<PathBuf>, entry: &str, home: &Path) -> bool {
    serves_home(definition, entry, home, default_home().as_deref())
        && exe.is_some_and(|e| e.is_file())
}

/// The binary a LaunchAgent runs: its first `ProgramArguments` string.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plist_exe(plist: &str) -> Option<PathBuf> {
    let args = plist.split_once("<key>ProgramArguments</key>")?.1;
    let first = args.split_once("<string>")?.1.split_once("</string>")?.0;
    let text = first
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        // Last, so an escaped entity is not unescaped twice.
        .replace("&amp;", "&");
    Some(PathBuf::from(text))
}

/// The binary a systemd unit runs: the first word of `ExecStart=`, quoted
/// the way `systemd_quote` writes it (a hand-edited unit may not quote it).
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn unit_exe(unit: &str) -> Option<PathBuf> {
    let line = unit.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
    let Some(quoted) = line.strip_prefix('"') else {
        return line.split_whitespace().next().map(PathBuf::from);
    };
    let mut out = String::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '"' => return Some(PathBuf::from(out.replace("%%", "%"))),
            c => out.push(c),
        }
    }
    None
}

/// Stop the autostart service's daemon, if a service is installed for this
/// data dir (`blirp stop`): stopping the unit also cancels a restart it has
/// pending. False when there is none, or its platform needs nothing (a
/// graceful stop exits 0, which launchd's KeepAlive does not restart).
pub(crate) fn stop_managed(paths: &Paths) -> anyhow::Result<bool> {
    platform::stop_managed(paths.home())
}

/// `~/.blirp`: the data dir of a service installed without `BLIRP_HOME`.
#[cfg_attr(windows, allow(dead_code))]
fn default_home() -> Option<PathBuf> {
    blirp_core::paths::user_home().map(|h| h.join(".blirp"))
}

/// The installed autostart entry and its state (`blirp doctor`).
pub(crate) fn installed() -> anyhow::Result<Option<String>> {
    platform::status()
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
        format!("gui/{}", uid())
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
            // With nobody logged in at the Mac (SSH only) there is no GUI
            // session to bootstrap into; the plist still loads at the next
            // login, so this is not a failure.
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

    pub fn start_managed(home: &Path) -> anyhow::Result<bool> {
        let entry = format!(
            "<key>BLIRP_HOME</key>\n    <string>{}</string>",
            xml_escape(&home.display().to_string())
        );
        let usable = std::fs::read_to_string(plist_path()?)
            .is_ok_and(|p| usable(&p, plist_exe(&p), &entry, home));
        if !usable || !loaded()? {
            return Ok(false);
        }
        must(
            "launchctl",
            &["kickstart", &format!("{}/{LAUNCHD_LABEL}", domain())],
        )?;
        Ok(true)
    }

    /// `KeepAlive.SuccessfulExit = false` never restarts a daemon that
    /// exited 0 (a graceful stop, or a duplicate that found one running),
    /// so the API stop sticks without unloading the LaunchAgent.
    pub fn stop_managed(_home: &Path) -> anyhow::Result<bool> {
        Ok(false)
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
pub(crate) mod platform {
    use super::*;

    fn unit_path() -> anyhow::Result<PathBuf> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .map_or_else(|| user_home().map(|h| h.join(".config")), Ok)?;
        Ok(config.join("systemd/user").join(SYSTEMD_UNIT))
    }

    /// `systemctl --user <args>`. Without this user's `XDG_RUNTIME_DIR`
    /// (none after `sudo -iu` or on a CI runner, another user's after `su`)
    /// systemctl cannot find the user manager's bus; point it at
    /// `/run/user/<uid>` when that manager runs (linger).
    fn user_systemctl(args: &[&str]) -> anyhow::Result<(bool, String)> {
        let mut cmd = blirp_core::process::command("systemctl");
        cmd.arg("--user").args(args);
        let own = PathBuf::from(format!("/run/user/{}", uid()));
        let inherited = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        if inherited.as_deref() != Some(own.as_path()) && own.join("bus").exists() {
            cmd.env("XDG_RUNTIME_DIR", own);
        }
        run_tool(cmd, "systemctl")
    }

    /// The service's daemon is running (`systemctl --user is-active`).
    pub fn unit_active() -> bool {
        user_systemctl(&["is-active", "--quiet", SYSTEMD_UNIT]).is_ok_and(|(ok, _)| ok)
    }

    fn systemctl(args: &[&str]) -> anyhow::Result<()> {
        user_systemctl(args)
            .and_then(|(ok, out)| {
                if !ok {
                    bail!("`systemctl --user {}` failed: {out}", args.join(" "));
                }
                Ok(())
            })
            .context(
                "systemd --user is not available; start `blirp daemon --detach` from your \
                 session startup instead",
            )
    }

    /// systemd is PID 1 (`sd_booted`), so user services can exist at all.
    /// False in most containers and on distributions without systemd.
    pub fn systemd_booted() -> bool {
        Path::new("/run/systemd/system").is_dir()
    }

    /// This user's systemd manager answers (it runs for a login session or
    /// because of linger). Waits up to `wait` for one that is starting.
    pub async fn user_manager_ready(wait: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + wait;
        loop {
            if user_systemctl(&["show-environment"]).is_ok_and(|(ok, _)| ok) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    /// Whether logind keeps this user's services running without a login
    /// session. None when logind cannot be asked (no loginctl, no systemd).
    pub fn linger() -> Option<bool> {
        let uid = uid().to_string();
        let (ok, out) = tool(
            "loginctl",
            &["show-user", &uid, "--property=Linger", "--value"],
        )
        .ok()?;
        parse_linger(ok, &out)
    }

    /// Try `loginctl enable-linger` without asking for a password (polkit
    /// allows it for some users, e.g. in an active local session). True
    /// when linger is on afterwards.
    pub fn enable_linger() -> bool {
        let uid = uid().to_string();
        // The outcome is read back below; a refusal is the expected case
        // for SSH users without polkit rights.
        let _ = tool("loginctl", &["--no-ask-password", "enable-linger", &uid]);
        linger() == Some(true)
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
        let active = user_systemctl(&["is-active", "--quiet", SYSTEMD_UNIT])?.0;
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
        if linger() != Some(true) {
            println!(
                "On a server, also run `{}` once so the daemon runs without an open \
                 login session and starts at boot.",
                linger_command(&user_name())
            );
        }
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

    /// The installed unit runs the daemon for `home` (see `usable`).
    fn unit_usable(home: &Path) -> anyhow::Result<bool> {
        let entry = format!(
            "Environment={}",
            systemd_quote(&format!("BLIRP_HOME={}", home.display()))
        );
        Ok(std::fs::read_to_string(unit_path()?)
            .is_ok_and(|u| usable(&u, unit_exe(&u), &entry, home)))
    }

    /// `systemctl --user stop` (SIGTERM, the daemon's graceful stop), also
    /// when the unit is not active: that cancels a pending restart.
    pub fn stop_managed(home: &Path) -> anyhow::Result<bool> {
        if !unit_usable(home)? {
            return Ok(false);
        }
        systemctl(&["stop", SYSTEMD_UNIT])?;
        Ok(true)
    }

    pub fn start_managed(home: &Path) -> anyhow::Result<bool> {
        if !unit_usable(home)? {
            return Ok(false);
        }
        let enabled =
            user_systemctl(&["is-enabled", "--quiet", SYSTEMD_UNIT]).is_ok_and(|(ok, _)| ok);
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
        let (_, enabled) = user_systemctl(&["is-enabled", SYSTEMD_UNIT])?;
        let (_, active) = user_systemctl(&["is-active", SYSTEMD_UNIT])?;
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
    pub fn start_managed(_home: &Path) -> anyhow::Result<bool> {
        Ok(false)
    }

    /// The Run entry never restarts anything.
    pub fn stop_managed(_home: &Path) -> anyhow::Result<bool> {
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
    fn service_serves_only_its_data_dir() {
        let default = Path::new("/home/me/.blirp");
        let other = Path::new("/tmp/test home");
        let plist_entry = |h: &Path| {
            format!(
                "<key>BLIRP_HOME</key>\n    <string>{}</string>",
                xml_escape(&h.display().to_string())
            )
        };
        let unit_entry = |h: &Path| {
            format!(
                "Environment={}",
                systemd_quote(&format!("BLIRP_HOME={}", h.display()))
            )
        };
        let exe = Path::new("/b");
        let logs = Path::new("/l");
        for (with_home, without, entry) in [
            (
                launch_agent_plist(exe, logs, Some(other), None),
                launch_agent_plist(exe, logs, None, None),
                &plist_entry as &dyn Fn(&Path) -> String,
            ),
            (
                systemd_unit(exe, Some(other), None),
                systemd_unit(exe, None, None),
                &unit_entry,
            ),
        ] {
            // Installed with BLIRP_HOME: only that data dir.
            assert!(serves_home(&with_home, &entry(other), other, Some(default)));
            assert!(!serves_home(
                &with_home,
                &entry(default),
                default,
                Some(default)
            ));
            // Installed without: the default data dir only.
            assert!(serves_home(
                &without,
                &entry(default),
                default,
                Some(default)
            ));
            assert!(!serves_home(&without, &entry(other), other, Some(default)));
            assert!(!serves_home(&without, &entry(default), default, None));
        }
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
    fn service_definitions_name_their_binary() {
        let odd = Path::new(r#"/Apps/R&D <x> "q" 5%/b\in/blirp"#);
        let plist = launch_agent_plist(odd, Path::new("/l"), Some(Path::new("/h")), None);
        assert_eq!(plist_exe(&plist).as_deref(), Some(odd));
        let unit = systemd_unit(odd, Some(Path::new("/h")), Some("/usr/bin"));
        assert_eq!(unit_exe(&unit).as_deref(), Some(odd));
        assert_eq!(
            unit_exe("[Service]\nExecStart=/usr/bin/blirp daemon\n").as_deref(),
            Some(Path::new("/usr/bin/blirp"))
        );
        assert_eq!(unit_exe("[Service]\nExecStart=\"/unterminated\n"), None);
        assert_eq!(plist_exe("<plist></plist>"), None);
        // A definition whose binary is gone is not used.
        let home = default_home().unwrap_or_else(|| PathBuf::from("/h"));
        let gone = systemd_unit(Path::new("/nonexistent/blirp"), None, None);
        assert!(!usable(&gone, unit_exe(&gone), "", &home));
    }

    #[test]
    fn linger_state_from_loginctl() {
        assert_eq!(parse_linger(true, "yes\n"), Some(true));
        assert_eq!(parse_linger(true, "no"), Some(false));
        assert_eq!(
            parse_linger(
                false,
                "Failed to get user: User ID 1001 is not logged in or lingering"
            ),
            Some(false)
        );
        assert_eq!(
            parse_linger(false, "System has not been booted with systemd"),
            None
        );
        assert_eq!(parse_linger(true, ""), None);
        assert_eq!(
            linger_command("deploy"),
            "sudo loginctl enable-linger deploy"
        );
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
