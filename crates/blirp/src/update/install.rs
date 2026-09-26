//! What the install scripts put on disk, and how `blirp update` and
//! `blirp uninstall` change it.
//!
//! `install.sh` / `install.ps1` write a receipt (`install.json`: version,
//! install dir, desktop app path, PATH change). Only a binary whose folder
//! matches its receipt counts as script-installed; everything else (a
//! source build, a package manager) is never replaced or deleted.
//!
//! Layout:
//! - CLI: `<install dir>/blirp` (default `~/.local/bin`); Windows
//!   `<install dir>\blirp.exe`, `conpty.dll`, `x64\OpenConsole.exe` (default
//!   `%LOCALAPPDATA%\Programs\blirp`).
//! - Desktop app: macOS `~/Applications/blirp.app`; Linux
//!   `~/.local/share/blirp/blirp.AppImage` + `~/.local/share/applications/blirp.desktop`;
//!   Windows `<install dir>\blirp-desktop.exe` + Start Menu `blirp.lnk`.
//! - Receipt: Windows `<install dir>\install.json`; macOS/Linux
//!   `${XDG_DATA_HOME:-~/.local/share}/blirp/install.json`.

use anyhow::{Context as _, bail};
use std::path::{Path, PathBuf};

pub const RECEIPT: &str = "install.json";
/// Lines the install script adds to a shell startup file end with this.
pub const PATH_MARKER: &str = "# added by the blirp installer";

/// Files of the CLI archive, relative to its top folder and to the install dir.
pub const CLI_FILES: &[&str] = if cfg!(windows) {
    &["blirp.exe", "conpty.dll", "x64/OpenConsole.exe"]
} else {
    &["blirp"]
};
/// The desktop executable in the Windows portable zip and install dir.
pub const DESKTOP_EXE: &str = "blirp-desktop.exe";
/// File name of the installed desktop app; a receipt naming anything else
/// deletes nothing.
const APP_NAME: &str = if cfg!(target_os = "macos") {
    "blirp.app"
} else if cfg!(windows) {
    DESKTOP_EXE
} else {
    "blirp.AppImage"
};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Receipt {
    pub version: String,
    pub install_dir: PathBuf,
    /// Desktop app path, "" when installed without it.
    #[serde(default)]
    pub app: String,
    /// macOS/Linux: shell startup file the PATH line was added to;
    /// Windows: the folder added to the user Path. "" when PATH was not changed.
    #[serde(default)]
    pub path_entry: String,
}

impl Receipt {
    pub fn app(&self) -> Option<&Path> {
        (!self.app.is_empty()).then(|| Path::new(&self.app))
    }
}

/// `${XDG_DATA_HOME:-~/.local/share}` (macOS too, like the install script).
pub fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| blirp_core::paths::user_home().map(|h| h.join(".local/share")))
}

pub fn receipt_path(exe_dir: &Path) -> Option<PathBuf> {
    if cfg!(windows) {
        Some(exe_dir.join(RECEIPT))
    } else {
        data_home().map(|d| d.join("blirp").join(RECEIPT))
    }
}

/// This binary, installed by the install script.
#[derive(Debug)]
pub struct Installed {
    pub receipt: Receipt,
    pub receipt_path: PathBuf,
    /// Canonical install dir (the running binary's folder).
    pub dir: PathBuf,
}

impl Installed {
    pub fn save(&self) -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(&self.receipt)?;
        std::fs::write(&self.receipt_path, text + "\n")
            .with_context(|| format!("write {}", self.receipt_path.display()))
    }
}

/// The receipt of the running binary, when the install script put it here.
pub fn installed() -> anyhow::Result<Option<Installed>> {
    let exe = crate::memory::blirp_exe();
    let Some(dir) = exe.parent() else {
        return Ok(None);
    };
    let Some(receipt_path) = receipt_path(dir) else {
        return Ok(None);
    };
    let text = match std::fs::read_to_string(&receipt_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", receipt_path.display())),
    };
    let receipt: Receipt = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .with_context(|| format!("invalid install receipt {}", receipt_path.display()))?;
    let same = dunce::canonicalize(&receipt.install_dir).is_ok_and(|d| d == dir);
    Ok(same.then(|| Installed {
        receipt,
        receipt_path,
        dir: dir.to_path_buf(),
    }))
}

/// Default desktop app locations, for launching (`blirp` without arguments).
pub fn app_candidates(exe_dir: &Path) -> Vec<PathBuf> {
    let home = blirp_core::paths::user_home();
    if cfg!(target_os = "macos") {
        let mut v: Vec<PathBuf> = home
            .iter()
            .map(|h| h.join("Applications/blirp.app"))
            .collect();
        v.push(PathBuf::from("/Applications/blirp.app"));
        v
    } else if cfg!(windows) {
        // Script install and the NSIS/MSI installers put it next to blirp.exe.
        vec![exe_dir.join(DESKTOP_EXE)]
    } else {
        let mut v: Vec<PathBuf> = data_home()
            .iter()
            .map(|d| d.join("blirp/blirp.AppImage"))
            .collect();
        // .deb / .rpm: /usr/bin/blirp-desktop next to /usr/bin/blirp.
        v.push(exe_dir.join("blirp-desktop"));
        v
    }
}

/// Desktop entry (Linux) or Start Menu shortcut (Windows) the script creates
/// with the app.
pub fn app_launcher() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(|a| PathBuf::from(a).join(r"Microsoft\Windows\Start Menu\Programs\blirp.lnk"))
    } else if cfg!(target_os = "macos") {
        None
    } else {
        data_home().map(|d| d.join("applications/blirp.desktop"))
    }
}

// ------------------------------------------------------------------ replace

fn remove_any(p: &Path) -> std::io::Result<()> {
    if p.symlink_metadata()?.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

fn file_name(p: &Path) -> anyhow::Result<String> {
    Ok(p.file_name()
        .with_context(|| format!("{} has no file name", p.display()))?
        .to_string_lossy()
        .into_owned())
}

/// Where `dst` goes while it is replaced: `<name>.old`, or a unique name
/// when an earlier `.old` is still in use (a Windows executable that runs).
fn aside_path(dst: &Path) -> anyhow::Result<PathBuf> {
    let name = file_name(dst)?;
    let old = dst.with_file_name(format!("{name}.old"));
    if old.symlink_metadata().is_err() || remove_any(&old).is_ok() {
        return Ok(old);
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    Ok(dst.with_file_name(format!("{name}.{ms}.old")))
}

/// Move `new` (same filesystem) to `dst`. An existing `dst` is renamed aside
/// first: Windows cannot overwrite or delete a running executable but can
/// rename it, and a directory (`blirp.app`) cannot be renamed over. The
/// aside copy is deleted when possible, else by [`cleanup_old`] on a later
/// start. On failure the old file is put back.
pub fn replace_path(new: &Path, dst: &Path) -> anyhow::Result<()> {
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let aside = match dst.symlink_metadata() {
        Ok(_) => {
            let a = aside_path(dst)?;
            std::fs::rename(dst, &a).with_context(|| format!("move {} aside", dst.display()))?;
            Some(a)
        }
        Err(_) => None,
    };
    if let Err(e) = std::fs::rename(new, dst) {
        if let Some(a) = &aside
            && let Err(back) = std::fs::rename(a, dst)
        {
            tracing::error!(error = %back, path = %dst.display(), "restore after failed replace");
        }
        return Err(e).with_context(|| format!("move {} to {}", new.display(), dst.display()));
    }
    if let Some(a) = aside {
        // Fails for a running Windows executable; cleanup_old gets it later.
        let _ = remove_any(&a);
    }
    Ok(())
}

/// Delete `<known name>[.<n>].old` leftovers of earlier updates in `dir`
/// and `dir/x64`. Best effort: a file still in use stays for next time.
pub fn cleanup_old(dir: &Path) {
    const KNOWN: &[&str] = &[
        "blirp.exe.",
        "blirp-desktop.exe.",
        "conpty.dll.",
        "OpenConsole.exe.",
    ];
    for d in [dir.to_path_buf(), dir.join("x64")] {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".old") && KNOWN.iter().any(|k| name.starts_with(k)) {
                let _ = remove_any(&e.path());
            }
        }
    }
}

/// Unpack a `.tar.gz` or `.zip` with the system `tar` (bsdtar on Windows 10
/// 1803+ and macOS reads zip too).
pub fn extract(archive: &Path, into: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(into).with_context(|| format!("create {}", into.display()))?;
    let tar = if cfg!(windows) {
        // Not a `tar` from PATH: Git's GNU tar cannot read zip files.
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        PathBuf::from(root).join(r"System32\tar.exe")
    } else {
        PathBuf::from("tar")
    };
    let out = std::process::Command::new(&tar)
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("run {}", tar.display()))?;
    if !out.status.success() {
        bail!(
            "extracting {} failed: {}",
            archive.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- uninstall

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    File(PathBuf),
    /// Only ever `blirp.app` or the Windows app's WebView2 data folder.
    Tree(PathBuf),
    /// Removed only when empty.
    DirIfEmpty(PathBuf),
}

/// What `blirp uninstall` deletes for a receipt. Only known file names are
/// ever listed; an app path with an unexpected name is reported in the
/// second value and left alone.
pub fn removal_plan(inst: &Installed, launcher: Option<&Path>) -> (Vec<Removal>, Vec<String>) {
    let mut plan = Vec::new();
    let mut skipped = Vec::new();
    for f in CLI_FILES {
        plan.push(Removal::File(inst.dir.join(f)));
    }
    if let Some(app) = inst.receipt.app() {
        let name = app.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if app.is_absolute() && name == APP_NAME {
            plan.push(if name == "blirp.app" {
                Removal::Tree(app.to_path_buf())
            } else {
                Removal::File(app.to_path_buf())
            });
            if name == DESKTOP_EXE {
                // WebView2's default data folder next to the exe (cache, storage).
                plan.push(Removal::Tree(
                    app.with_file_name(format!("{DESKTOP_EXE}.WebView2")),
                ));
            }
            if let Some(l) = launcher
                && matches!(
                    l.file_name().and_then(|n| n.to_str()),
                    Some("blirp.desktop" | "blirp.lnk")
                )
            {
                plan.push(Removal::File(l.to_path_buf()));
            }
        } else {
            skipped.push(format!(
                "desktop app {} (unexpected location; remove it yourself)",
                app.display()
            ));
        }
    }
    plan.push(Removal::File(inst.receipt_path.clone()));
    if cfg!(windows) {
        plan.push(Removal::DirIfEmpty(inst.dir.join("x64")));
        plan.push(Removal::DirIfEmpty(inst.dir.clone()));
    } else if let Some(parent) = inst.receipt_path.parent() {
        // ~/.local/share/blirp (receipt and AppImage). The CLI folder is
        // usually shared (~/.local/bin) and stays.
        plan.push(Removal::DirIfEmpty(parent.to_path_buf()));
    }
    (plan, skipped)
}

/// Carry out one removal; missing files are fine. Returns whether something
/// was deleted.
pub fn remove(r: &Removal) -> anyhow::Result<bool> {
    let (res, p) = match r {
        Removal::File(p) => (std::fs::remove_file(p), p),
        Removal::Tree(p) => (std::fs::remove_dir_all(p), p),
        Removal::DirIfEmpty(p) => {
            let empty = std::fs::read_dir(p).is_ok_and(|mut d| d.next().is_none());
            if !empty {
                return Ok(false);
            }
            (std::fs::remove_dir(p), p)
        }
    };
    match res {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("remove {}", p.display())),
    }
}

/// Remove the installer's PATH line(s) from a shell startup file; a
/// `blirp.fish` left empty is deleted. Returns whether the file changed.
pub fn remove_path_lines(rc: &Path) -> anyhow::Result<bool> {
    let text = match std::fs::read_to_string(rc) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("read {}", rc.display())),
    };
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_end().ends_with(PATH_MARKER))
        .collect();
    if kept.len() == text.lines().count() {
        return Ok(false);
    }
    let own_file = rc.file_name().is_some_and(|n| n == "blirp.fish");
    if own_file && kept.iter().all(|l| l.trim().is_empty()) {
        std::fs::remove_file(rc).with_context(|| format!("remove {}", rc.display()))?;
        return Ok(true);
    }
    let mut out = kept.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(rc, out).with_context(|| format!("write {}", rc.display()))?;
    Ok(true)
}

/// Whether `dir` looks like a blirp data dir that `--purge` may delete: not
/// the home folder or a filesystem root, and named `.blirp` or holding
/// blirp's own files.
pub fn purgeable(dir: &Path, user_home: Option<&Path>) -> bool {
    if dir.parent().is_none() || !dir.is_absolute() || Some(dir) == user_home {
        return false;
    }
    dir.file_name().is_some_and(|n| n == ".blirp")
        || dir.join("blirp.db").is_file()
        || dir.join("config.toml").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(dir: &Path, app: &str) -> Installed {
        Installed {
            receipt: Receipt {
                version: "0.1.0".into(),
                install_dir: dir.to_path_buf(),
                app: app.into(),
                path_entry: String::new(),
            },
            receipt_path: dir.join("share/blirp").join(RECEIPT),
            dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn receipt_roundtrip_and_empty_app() {
        let r: Receipt = serde_json::from_str(
            r#"{"version":"0.1.0","install_dir":"/x/bin","app":"","path_entry":""}"#,
        )
        .unwrap();
        assert_eq!(r.app(), None);
        let r: Receipt =
            serde_json::from_str(r#"{"version":"0.1.0","install_dir":"C:\\a b\\blirp"}"#).unwrap();
        assert_eq!(r.install_dir, PathBuf::from(r"C:\a b\blirp"));
        assert_eq!(r.path_entry, "");
    }

    #[test]
    fn uninstall_only_touches_known_names() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let app_ok = if cfg!(target_os = "macos") {
            dir.join("Applications/blirp.app")
        } else if cfg!(windows) {
            dir.join(DESKTOP_EXE)
        } else {
            dir.join("share/blirp/blirp.AppImage")
        };
        let launcher = dir.join("applications/blirp.desktop");
        let (plan, skipped) =
            removal_plan(&inst(dir, &app_ok.display().to_string()), Some(&launcher));
        assert!(skipped.is_empty());
        for r in &plan {
            let p = match r {
                Removal::File(p) | Removal::Tree(p) | Removal::DirIfEmpty(p) => p,
            };
            assert!(p.starts_with(dir), "{} outside the install", p.display());
            if let Removal::Tree(p) = r {
                assert!(p.ends_with("blirp.app") || p.ends_with("blirp-desktop.exe.WebView2"));
            }
        }
        for f in CLI_FILES {
            assert!(plan.contains(&Removal::File(dir.join(f))));
        }
        assert!(plan.contains(&Removal::File(launcher)));

        // A receipt pointing the app at the home folder, a root or a
        // relative path deletes nothing there.
        for bad in [
            dir.display().to_string(),
            "/".to_string(),
            "blirp.app".to_string(),
            dir.join("Applications").display().to_string(),
            dir.join("blirp.app.bak").display().to_string(),
        ] {
            let (plan, skipped) = removal_plan(&inst(dir, &bad), Some(&dir.join("x.desktop")));
            assert_eq!(skipped.len(), 1, "{bad}");
            assert!(plan.iter().all(|r| !matches!(r, Removal::Tree(_))), "{bad}");
            assert!(!plan.contains(&Removal::File(PathBuf::from(&bad))), "{bad}");
            assert!(!plan.contains(&Removal::File(dir.join("x.desktop"))));
        }
    }

    #[test]
    fn remove_handles_missing_and_non_empty() {
        let root = tempfile::tempdir().unwrap();
        let d = root.path().join("d");
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("keep"), "x").unwrap();
        assert!(!remove(&Removal::DirIfEmpty(d.clone())).unwrap());
        assert!(d.join("keep").exists());
        assert!(remove(&Removal::File(d.join("keep"))).unwrap());
        assert!(!remove(&Removal::File(d.join("keep"))).unwrap());
        assert!(remove(&Removal::DirIfEmpty(d.clone())).unwrap());
        assert!(!d.exists());
    }

    #[test]
    fn purge_guard() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        assert!(purgeable(&home.join(".blirp"), Some(home)));
        assert!(!purgeable(home, Some(home)));
        assert!(!purgeable(Path::new("/"), Some(home)));
        let custom = home.join("data");
        std::fs::create_dir(&custom).unwrap();
        assert!(!purgeable(&custom, Some(home)));
        std::fs::write(custom.join("blirp.db"), "").unwrap();
        assert!(purgeable(&custom, Some(home)));
    }

    #[test]
    fn path_lines_are_removed_exactly() {
        let root = tempfile::tempdir().unwrap();
        let rc = root.path().join(".zshrc");
        let line = format!("export PATH=\"/h/.local/bin:$PATH\" {PATH_MARKER}");
        std::fs::write(&rc, format!("alias a=b\n\n{line}\nexport X=1\n")).unwrap();
        assert!(remove_path_lines(&rc).unwrap());
        assert_eq!(
            std::fs::read_to_string(&rc).unwrap(),
            "alias a=b\n\nexport X=1\n"
        );
        assert!(!remove_path_lines(&rc).unwrap());
        let fish = root.path().join("blirp.fish");
        std::fs::write(&fish, format!("fish_add_path -g /h/bin {PATH_MARKER}\n")).unwrap();
        assert!(remove_path_lines(&fish).unwrap());
        assert!(!fish.exists());
    }

    #[test]
    fn replace_and_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let dst = dir.join("blirp.exe");
        std::fs::write(&dst, "old").unwrap();
        let new = dir.join("staged");
        std::fs::write(&new, "new").unwrap();
        replace_path(&new, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "new");
        assert!(!new.exists());
        // A directory is replaced as a whole.
        let app = dir.join("blirp.app");
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        let staged = dir.join("stage/blirp.app");
        std::fs::create_dir_all(staged.join("New")).unwrap();
        replace_path(&staged, &app).unwrap();
        assert!(app.join("New").is_dir() && !app.join("Contents").exists());
        // Leftovers of known names go, other files stay.
        std::fs::write(dir.join("blirp.exe.old"), "").unwrap();
        std::fs::create_dir(dir.join("x64")).unwrap();
        std::fs::write(dir.join("x64/OpenConsole.exe.123.old"), "").unwrap();
        std::fs::write(dir.join("notes.old"), "").unwrap();
        cleanup_old(dir);
        assert!(!dir.join("blirp.exe.old").exists());
        assert!(!dir.join("x64/OpenConsole.exe.123.old").exists());
        assert!(dir.join("notes.old").exists());
    }

    /// The Windows case this module exists for: replacing an executable
    /// while it runs.
    #[cfg(windows)]
    #[test]
    fn replace_running_executable() {
        let root = tempfile::tempdir().unwrap();
        let exe = root.path().join("blirp.exe");
        let system = std::env::var_os("SystemRoot").unwrap();
        std::fs::copy(Path::new(&system).join(r"System32\PING.EXE"), &exe).unwrap();
        let mut child = std::process::Command::new(&exe)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // Running: neither delete nor overwrite works.
        assert!(std::fs::remove_file(&exe).is_err());
        let new = root.path().join("new.exe");
        std::fs::write(&new, "new").unwrap();
        replace_path(&new, &exe).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        let old = root.path().join("blirp.exe.old");
        assert!(old.exists(), "the running copy stays aside");
        // A second update while the first old copy still runs.
        let newer = root.path().join("newer.exe");
        std::fs::write(&newer, "newer").unwrap();
        replace_path(&newer, &exe).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "newer");
        child.kill().unwrap();
        child.wait().unwrap();
        cleanup_old(root.path());
        let left: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["blirp.exe".to_string()]);
    }

    #[test]
    fn extracts_archives() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("src/blirp-0.2.0-x");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("blirp"), "bin").unwrap();
        let archive = root.path().join("a.tar.gz");
        let status = std::process::Command::new(if cfg!(windows) {
            r"C:\Windows\System32\tar.exe"
        } else {
            "tar"
        })
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(root.path().join("src"))
        .arg("blirp-0.2.0-x")
        .status()
        .unwrap();
        assert!(status.success());
        let out = root.path().join("out");
        extract(&archive, &out).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("blirp-0.2.0-x/blirp")).unwrap(),
            "bin"
        );
        assert!(extract(&root.path().join("missing.tar.gz"), &out).is_err());
    }
}
