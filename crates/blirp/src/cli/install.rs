//! `blirp update`, `blirp uninstall` and `blirp` / `blirp app` (open the
//! desktop app or the browser UI). Installation layout: `crate::update::install`.

use crate::update::{self, install};
use anyhow::{Context as _, bail};
use blirp_core::paths::Paths;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// `blirp update --check` when a newer release exists.
const UPDATE_AVAILABLE: u8 = 10;

pub async fn update(
    paths: &Paths,
    check: bool,
    version: Option<String>,
) -> anyhow::Result<ExitCode> {
    let current = update::parse_version(update::CURRENT)?;
    let wanted = version.as_deref().map(update::parse_version).transpose()?;
    let client = update::http()?;
    let release = update::fetch_release(&client, wanted.as_ref()).await?;
    let target = release.version()?;
    if check {
        if target > current {
            println!("blirp {target} is available (you have {current}); run `blirp update`");
            return Ok(ExitCode::from(UPDATE_AVAILABLE));
        }
        println!("blirp {current} is up to date");
        return Ok(ExitCode::SUCCESS);
    }
    if target == current {
        println!("blirp {current} is already installed");
        return Ok(ExitCode::SUCCESS);
    }
    if target < current && wanted.is_none() {
        println!(
            "blirp {current} is newer than the latest release ({target}); \
             use `blirp update --version {target}` to downgrade"
        );
        return Ok(ExitCode::SUCCESS);
    }
    let Some(mut inst) = install::installed()? else {
        bail!(
            "{} was not installed by the blirp install script, so it is not updated in place. \
             Update it the way you installed it (git pull + cargo build, your package manager), \
             or install with the script (see docs/install.md).",
            crate::memory::blirp_exe().display()
        );
    };
    let names = update::this_platform(&target)?;
    let app = inst
        .receipt
        .app()
        .filter(|a| a.exists())
        .map(Path::to_path_buf);

    // Staged next to the destination so the final moves are renames.
    let stage = tempfile::Builder::new()
        .prefix(".blirp-update-")
        .tempdir_in(&inst.dir)
        .with_context(|| format!("create a staging folder in {}", inst.dir.display()))?;
    let sums_file = stage.path().join(update::SUMS);
    let sig_file = stage.path().join(update::SUMS_SIG);
    update::download(&client, release.asset(update::SUMS)?, &sums_file).await?;
    update::download(&client, release.asset(update::SUMS_SIG)?, &sig_file).await?;
    let sums_bytes = std::fs::read(&sums_file)?;
    let sig = std::fs::read_to_string(&sig_file)?;
    update::verify_signature(&sums_bytes, &sig, update::release_key()).with_context(|| {
        format!(
            "{} of {} is not signed by the blirp release key; not updating",
            update::SUMS,
            release.tag_name
        )
    })?;
    let sums = update::parse_sums(&String::from_utf8_lossy(&sums_bytes));

    println!("Downloading blirp {target}...");
    let cli_archive = stage.path().join(&names.cli);
    let hash = update::download(&client, release.asset(&names.cli)?, &cli_archive).await?;
    update::check_sha256(&sums, &names.cli, &hash)?;
    let unpacked = stage.path().join("cli");
    install::extract(&cli_archive, &unpacked)?;
    let cli_root = unpacked.join(&names.cli_dir);
    for f in install::CLI_FILES {
        if !cli_root.join(f).is_file() {
            bail!("{} does not contain {}/{f}", names.cli, names.cli_dir);
        }
    }

    let app_stage = match &app {
        Some(dst) => {
            let parent = dst
                .parent()
                .with_context(|| format!("{} has no parent folder", dst.display()))?;
            let dir = tempfile::Builder::new()
                .prefix(".blirp-update-")
                .tempdir_in(parent)
                .with_context(|| format!("create a staging folder in {}", parent.display()))?;
            let file = dir.path().join(&names.app);
            let hash = update::download(&client, release.asset(&names.app)?, &file).await?;
            update::check_sha256(&sums, &names.app, &hash)?;
            let staged = stage_app(&file, dir.path(), &names.app)?;
            Some((dir, staged, dst.clone()))
        }
        None => None,
    };

    // Everything is verified; only now touch the running installation.
    let was_running = crate::daemon::running_daemon(paths).await.is_some();
    if was_running {
        super::lifecycle::stop(paths).await?;
    }
    for f in install::CLI_FILES {
        install::replace_path(&cli_root.join(f), &inst.dir.join(f))?;
    }
    if let Some((_dir, staged, dst)) = &app_stage {
        install::replace_path(staged, dst)?;
    }
    inst.receipt.version = target.to_string();
    inst.save()?;
    println!("Updated blirp {current} -> {target}");
    if was_running {
        restart_daemon(paths, &inst.dir.join(install::CLI_FILES[0])).await?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Unpack a downloaded desktop app asset in `dir`; returns the new app
/// (bundle, AppImage or desktop exe) ready to be moved into place.
fn stage_app(file: &Path, dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let staged = if name.ends_with(".AppImage") {
        file.to_path_buf()
    } else {
        let out = dir.join("app");
        install::extract(file, &out)?;
        if name.ends_with(".app.tar.gz") {
            out.join("blirp.app")
        } else {
            // Windows portable zip: blirp_<ver>_x64-portable/blirp-desktop.exe
            out.join(name.trim_end_matches(".zip"))
                .join(install::DESKTOP_EXE)
        }
    };
    if staged.symlink_metadata().is_err() {
        bail!("{name} does not contain the desktop app");
    }
    #[cfg(unix)]
    if staged.is_file() {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(staged)
}

/// Start the daemon again after an update: through the autostart service
/// when it manages one, else `<new blirp> daemon --detach`.
async fn restart_daemon(paths: &Paths, cli: &Path) -> anyhow::Result<()> {
    if super::service::start_managed()? {
        println!("Daemon restarted by the autostart service.");
        return Ok(());
    }
    let status = tokio::process::Command::new(cli)
        .args(["daemon", "--detach"])
        .env(blirp_core::paths::HOME_ENV, paths.home())
        .stdin(std::process::Stdio::null())
        .status()
        .await
        .with_context(|| format!("run {} daemon --detach", cli.display()))?;
    if !status.success() {
        bail!("the updated daemon did not start ({status}); see `blirp logs`");
    }
    Ok(())
}

// ---------------------------------------------------------------- uninstall

fn confirm(question: &str) -> anyhow::Result<bool> {
    use std::io::{BufRead as _, IsTerminal as _, Write as _};
    if !std::io::stdin().is_terminal() {
        bail!("{question} Pass --yes to confirm without a prompt.");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

pub async fn uninstall(paths: &Paths, purge: bool, yes: bool) -> anyhow::Result<ExitCode> {
    let inst = install::installed()?;
    let data = paths.home().to_path_buf();
    if purge {
        if !install::purgeable(&data, blirp_core::paths::user_home().as_deref()) {
            bail!(
                "refusing to delete {}: it does not look like a blirp data folder",
                data.display()
            );
        }
        if !yes
            && !confirm(&format!(
                "Delete {} with all blirp memory, sessions and settings?",
                data.display()
            ))?
        {
            println!("Nothing was changed.");
            return Ok(ExitCode::FAILURE);
        }
    }

    let mut ok = true;
    super::lifecycle::stop(paths).await?;
    super::service::uninstall()?;
    println!("Removing global agent hooks:");
    ok &= super::mem::run_hooks(super::mem::HooksCommand::Uninstall { agent: None })?
        == ExitCode::SUCCESS;
    match &inst {
        Some(inst) => ok &= remove_install(inst),
        None => println!(
            "{} was not installed by the install script (a source build or a package); \
             it was left in place.",
            crate::memory::blirp_exe().display()
        ),
    }
    if purge {
        std::fs::remove_dir_all(&data).with_context(|| format!("delete {}", data.display()))?;
        println!("Deleted {}", data.display());
    } else {
        println!(
            "Your memory and settings stay in {} (`blirp uninstall --purge` deletes them).",
            data.display()
        );
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Delete what the install script installed. Returns false when something
/// could not be removed (reported).
fn remove_install(inst: &install::Installed) -> bool {
    let mut ok = true;
    #[cfg(windows)]
    if !windows::stop_desktop_app(&inst.dir) {
        ok = false;
    }
    let launcher = install::app_launcher();
    let (plan, skipped) = install::removal_plan(inst, launcher.as_deref());
    let exe = inst.dir.join(install::CLI_FILES[0]);
    for r in &plan {
        // Windows cannot delete the running blirp.exe, and WebView2 helper
        // processes of the closed app hold its data folder for a moment:
        // both go after this command exits (delete_after_exit).
        if cfg!(windows)
            && (*r == install::Removal::File(exe.clone()) || matches!(r, install::Removal::Tree(_)))
        {
            continue;
        }
        match install::remove(r) {
            Ok(true) => {
                if let install::Removal::File(p) | install::Removal::Tree(p) = r {
                    println!("removed {}", p.display());
                }
            }
            Ok(false) => {}
            Err(e) => {
                ok = false;
                eprintln!("error: {e:#}");
            }
        }
    }
    ok &= remove_url_handler(inst);
    for s in skipped {
        println!("left in place: {s}");
    }
    let entry = inst.receipt.path_entry.trim();
    if !entry.is_empty() {
        #[cfg(windows)]
        let changed = windows::remove_user_path(entry);
        #[cfg(not(windows))]
        let changed = install::remove_path_lines(Path::new(entry));
        match changed {
            Ok(true) => println!("removed the PATH entry ({entry})"),
            Ok(false) => {}
            Err(e) => {
                ok = false;
                eprintln!("error: PATH entry in {entry}: {e:#}");
            }
        }
    }
    #[cfg(windows)]
    if let Err(e) = windows::delete_after_exit(&exe, &inst.dir) {
        ok = false;
        eprintln!("error: {e:#}; delete {} yourself", exe.display());
    } else {
        println!("removed {} (as soon as this command exits)", exe.display());
    }
    ok
}

/// The desktop app registers `blirp://` for itself when it runs (Linux and
/// Windows, see app/src-tauri/src/lib.rs); drop that registration when it
/// points at the app being removed.
fn remove_url_handler(inst: &install::Installed) -> bool {
    let Some(app) = inst.receipt.app() else {
        return true;
    };
    #[cfg(windows)]
    {
        let _ = app;
        match windows::remove_url_handler(&inst.dir) {
            Ok(true) => println!("removed the blirp:// link handler"),
            Ok(false) => {}
            Err(e) => {
                eprintln!("error: blirp:// link handler: {e:#}");
                return false;
            }
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(data) = install::data_home() {
        let handler = data.join("applications/blirp-desktop-handler.desktop");
        let ours = std::fs::read_to_string(&handler)
            .is_ok_and(|t| t.contains(app.to_string_lossy().as_ref()));
        if ours {
            match install::remove(&install::Removal::File(handler.clone())) {
                Ok(_) => println!("removed {}", handler.display()),
                Err(e) => {
                    eprintln!("error: {e:#}");
                    return false;
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    let _ = app;
    true
}

#[cfg(windows)]
mod windows {
    use anyhow::{Context as _, bail};
    use std::os::windows::process::CommandExt as _;
    use std::path::Path;

    // Not DETACHED_PROCESS: Windows PowerShell exits at once without a
    // console. CREATE_NO_WINDOW gives it a hidden console of its own.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    /// Windows PowerShell running `script`. Paths reach scripts through
    /// environment variables, never quoted into the script text.
    fn powershell(script: &str) -> std::process::Command {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        let mut c = std::process::Command::new(
            Path::new(&root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
        );
        c.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
        ])
        // Any error fails the run; lookups that may find nothing say
        // `-ErrorAction SilentlyContinue` themselves.
        .arg(format!("$ErrorActionPreference = 'Stop'\n{script}\nexit 0"))
        .stdin(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
        c
    }

    /// Run a script and return its output.
    fn run(script: &str, env: &[(&str, &Path)]) -> anyhow::Result<String> {
        let mut c = powershell(script);
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.output().context("run powershell")?;
        if !out.status.success() {
            bail!(
                "powershell failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Close the desktop app of this installation (it holds its exe open).
    pub fn stop_desktop_app(dir: &Path) -> bool {
        let script = r#"
$d = $env:BLIRP_DIR.TrimEnd('\') + '\'
Get-Process -Name blirp-desktop -ErrorAction SilentlyContinue |
  Where-Object { $_.Path -and $_.Path.StartsWith($d, [StringComparison]::OrdinalIgnoreCase) } |
  Stop-Process -Force
"#;
        run(script, &[("BLIRP_DIR", dir)])
            .inspect_err(|e| eprintln!("error: closing the desktop app: {e:#}"))
            .is_ok()
    }

    /// Delete `HKCU\Software\Classes\blirp` when it opens an exe in `dir`.
    pub fn remove_url_handler(dir: &Path) -> anyhow::Result<bool> {
        let script = r#"
$k = 'HKCU:\Software\Classes\blirp'
$c = (Get-ItemProperty -LiteralPath "$k\shell\open\command" -ErrorAction SilentlyContinue).'(default)'
$d = $env:BLIRP_DIR.TrimEnd('\') + '\'
if ($c -and $c.IndexOf($d, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
  Remove-Item -LiteralPath $k -Recurse -Force
  'removed'
}
"#;
        Ok(run(script, &[("BLIRP_DIR", dir)])? == "removed")
    }

    /// Remove `dir` from the user Path (registry, keeping REG_EXPAND_SZ) and
    /// tell running programs about it.
    pub fn remove_user_path(dir: &str) -> anyhow::Result<bool> {
        let script = r#"
$dir = $env:BLIRP_DIR.TrimEnd('\')
$k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
$p = [string]$k.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
$new = @($p -split ';' | Where-Object { $_ -and ($_.TrimEnd('\') -ne $dir) }) -join ';'
if ($new -ne $p) {
  $k.SetValue('Path', $new, 'ExpandString')
  # Setting a user variable through .NET broadcasts WM_SETTINGCHANGE.
  [Environment]::SetEnvironmentVariable('BLIRP_PATH_CHANGED', '1', 'User')
  [Environment]::SetEnvironmentVariable('BLIRP_PATH_CHANGED', $null, 'User')
  'changed'
}
"#;
        Ok(run(script, &[("BLIRP_DIR", Path::new(dir))])? == "changed")
    }

    /// A running executable cannot delete itself: a hidden PowerShell waits
    /// for this process to exit, deletes `exe` and the desktop app's WebView2
    /// data folder (its helper processes may hold it a little longer), then
    /// the install folder and `x64` if they are empty.
    pub fn delete_after_exit(exe: &Path, dir: &Path) -> anyhow::Result<()> {
        let script = r#"
try { Wait-Process -Id ([int]$env:BLIRP_PID) -Timeout 60 -ErrorAction Stop } catch {}
$webview = Join-Path $env:BLIRP_DIR 'blirp-desktop.exe.WebView2'
foreach ($p in @($env:BLIRP_EXE, $webview)) {
  for ($i = 0; $i -lt 40 -and (Test-Path -LiteralPath $p); $i++) {
    Remove-Item -LiteralPath $p -Recurse -Force -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $p) { Start-Sleep -Milliseconds 250 }
  }
}
Get-ChildItem -LiteralPath $env:BLIRP_DIR -Filter '*.old' -Recurse -ErrorAction SilentlyContinue |
  Where-Object { $_.Name -match '^(blirp\.exe|blirp-desktop\.exe|conpty\.dll|OpenConsole\.exe)\.' } |
  Remove-Item -Force -ErrorAction SilentlyContinue
foreach ($d in @((Join-Path $env:BLIRP_DIR 'x64'), $env:BLIRP_DIR)) {
  if ((Test-Path -LiteralPath $d) -and -not (Get-ChildItem -LiteralPath $d -Force)) {
    Remove-Item -LiteralPath $d -Force -ErrorAction SilentlyContinue
  }
}
"#;
        let spawn = |flags: u32| {
            powershell(script)
                .env("BLIRP_EXE", exe)
                .env("BLIRP_DIR", dir)
                .env("BLIRP_PID", std::process::id().to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .creation_flags(flags)
                .spawn()
        };
        // Breaking away also survives a job object that kills the terminal's
        // process tree when it closes; a job that forbids it refuses the
        // flag, then run without.
        spawn(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
            .or_else(|_| spawn(CREATE_NO_WINDOW))
            .context("start the cleanup process")?;
        Ok(())
    }
}

// ------------------------------------------------------------------- launch

/// No screen to show a window on: an SSH session, or Linux without X11/Wayland.
fn headless() -> bool {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    set("SSH_CONNECTION")
        || set("SSH_TTY")
        || (cfg!(all(unix, not(target_os = "macos"))) && !set("DISPLAY") && !set("WAYLAND_DISPLAY"))
}

/// `blirp` / `blirp app`: make sure the daemon runs, then open the desktop
/// app if it is installed, else the browser UI. Headless: print how to reach it.
pub async fn launch(paths: &Paths) -> anyhow::Result<ExitCode> {
    let info = crate::daemon::detach(paths, None).await?;
    let base = info.base_url();
    let login = format!("{base}/auth?token={}", info.token);
    if headless() {
        println!("blirp daemon running at {base} (no display here, so nothing was opened)");
        println!(
            "Use it from another device: forward the port (ssh -L {p}:127.0.0.1:{p} <this machine>) \
             and open {login}\nor make this machine a hub and enable the LAN portal \
             (`blirp hub enable`, see docs/portal.md). `blirp --help` lists the CLI.",
            p = info.port
        );
        return Ok(ExitCode::SUCCESS);
    }
    let exe = crate::memory::blirp_exe();
    let dir = exe.parent().unwrap_or(Path::new("."));
    match install::app_candidates(dir)
        .into_iter()
        .find(|p| p.exists())
    {
        Some(app) => {
            open_app(&app, paths).with_context(|| format!("start {}", app.display()))?;
            println!("Opened the blirp desktop app ({})", app.display());
        }
        None => {
            super::open_browser(&login).context("launch browser")?;
            println!("Opened {base} in your browser");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn open_app(app: &Path, paths: &Paths) -> anyhow::Result<()> {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg("-a").arg(app);
        // Launch Services does not pass this shell's environment on; a
        // custom data dir must reach the app or it would start a second
        // daemon for ~/.blirp.
        if std::env::var_os(blirp_core::paths::HOME_ENV).is_some_and(|v| !v.is_empty()) {
            c.arg("--env").arg(format!(
                "{}={}",
                blirp_core::paths::HOME_ENV,
                paths.home().display()
            ));
        }
        c
    } else {
        std::process::Command::new(app)
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::os::unix::process::CommandExt as _;
        #[allow(unsafe_code)]
        // SAFETY: setsid is async-signal-safe and touches no Rust state in
        // the forked child; it keeps the app alive when this terminal closes.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    if cfg!(target_os = "macos") {
        let status = cmd.status()?;
        if !status.success() {
            bail!("open -a failed ({status})");
        }
    } else {
        cmd.spawn()?;
    }
    Ok(())
}
