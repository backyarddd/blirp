//! Memory engine (§9): distill, injection rendering, handoff packs and
//! per-launch agent integration.

pub mod distill;
pub mod handoff;
pub mod launch;
pub mod render;
#[cfg(test)]
pub(crate) mod testutil;

use std::path::{Path, PathBuf};

/// Lets hooks tell the ingest subsystem where an agent's transcript lives, so
/// it can be ingested promptly. The ingest module installs its implementation
/// with `AppState::set_ingest_trigger`; the default does nothing.
pub trait IngestTrigger: Send + Sync {
    /// `session_id` is the blirp session the transcript belongs to.
    fn transcript_hint(&self, agent: &str, session_id: &str, transcript_path: &Path);
}

/// Default trigger used until an ingest implementation is installed.
pub struct NoopIngest;

impl IngestTrigger for NoopIngest {
    fn transcript_hint(&self, _agent: &str, _session_id: &str, _transcript_path: &Path) {}
}

/// Absolute path of the running `blirp` executable, used in hook commands and
/// MCP server definitions.
pub fn blirp_exe() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| dunce::canonicalize(&p).ok().or(Some(p)))
        .unwrap_or_else(|| PathBuf::from("blirp"))
}

/// `exe` (the running binary), or what to write instead into configuration
/// that outlives this process: agent hooks and MCP entries, autostart. An
/// AppImage runs its binary from a temporary mount (`/tmp/.mount_*`) that
/// disappears when the app exits, so there the installed CLI is used: the
/// one the install receipt names, else `blirp` on PATH outside the mount.
pub fn persistent_exe(exe: PathBuf) -> anyhow::Result<PathBuf> {
    let (appimage, roots) = temp_roots();
    let roots: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
    persistent_exe_from(exe, appimage, &roots, || installed_candidates(&roots))
}

/// The binary a background daemon runs from. Started from an AppImage (the
/// desktop app's sidecar), the installed CLI when its receipt names this
/// version: a daemon outlives the app, and everything it writes or runs from
/// its own path (per-session hooks and MCP entries, `blirp update`) must
/// outlive the mount too. Only the same version: the daemon serves the UI and
/// owns the database schema. Else `exe` as before.
pub fn daemon_exe(exe: PathBuf) -> PathBuf {
    let (appimage, roots) = temp_roots();
    let roots: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
    daemon_exe_from(
        exe,
        appimage,
        &roots,
        crate::update::install::installed_cli_version,
    )
}

/// Whether this runs from an AppImage, and the folders that are gone once
/// the app exits (`/tmp`, the AppImage mount).
fn temp_roots() -> (bool, Vec<PathBuf>) {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    // APPDIR is a generic name elsewhere; AppImages are Linux only.
    let appimage =
        cfg!(target_os = "linux") && (var("APPIMAGE").is_some() || var("APPDIR").is_some());
    let mut roots = vec![PathBuf::from("/tmp")];
    roots.extend(var("APPDIR").map(PathBuf::from));
    (appimage, roots)
}

/// The installed CLI: the install receipt's, then `blirp` on PATH.
fn installed_candidates(roots: &[&Path]) -> impl Iterator<Item = PathBuf> {
    // An AppImage puts its own bin dir first on PATH: skip it, so a real
    // install later on PATH is found.
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|d| !under(d, roots))
        .collect();
    crate::update::install::installed_cli()
        .into_iter()
        .chain(blirp_core::process::which_in("blirp", dirs))
}

/// `receipt_cli`: the install receipt's CLI and version. PATH is not
/// searched: a `blirp` there has no known version.
fn daemon_exe_from(
    exe: PathBuf,
    appimage: bool,
    temp_roots: &[&Path],
    receipt_cli: impl FnOnce() -> Option<(PathBuf, String)>,
) -> PathBuf {
    // Only the AppImage case: a binary someone runs from /tmp themselves
    // keeps running as itself.
    if !appimage {
        return exe;
    }
    let current = crate::update::parse_version(crate::update::CURRENT).ok();
    let cli = receipt_cli()
        .filter(|(_, v)| current.is_some() && crate::update::parse_version(v).ok() == current)
        .map(|(cli, _)| cli);
    persistent_exe_from(exe.clone(), true, temp_roots, || cli).unwrap_or(exe)
}

/// `p` is inside one of `roots` (as given or canonical).
fn under(p: &Path, roots: &[&Path]) -> bool {
    roots
        .iter()
        .any(|r| p.starts_with(r) || dunce::canonicalize(r).is_ok_and(|r| p.starts_with(r)))
}

/// `temp_roots`: folders that are gone once the app exits (`/tmp`, the
/// AppImage mount).
fn persistent_exe_from<I: IntoIterator<Item = PathBuf>>(
    exe: PathBuf,
    appimage: bool,
    temp_roots: &[&Path],
    candidates: impl FnOnce() -> I,
) -> anyhow::Result<PathBuf> {
    let temporary = |p: &Path| under(p, temp_roots);
    if !appimage && !temporary(&exe) {
        return Ok(exe);
    }
    candidates()
        .into_iter()
        .map(|p| dunce::canonicalize(&p).unwrap_or(p))
        .find(|p| p.is_file() && !temporary(p))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "this blirp runs from a temporary location ({}) that is gone once the app \
                 exits, and no installed blirp CLI was found; install the CLI (see \
                 docs/install.md) and try again",
                exe.display()
            )
        })
}

/// Path with `/` separators (valid on Windows too) for shell command strings.
pub fn slash_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// Shell command running `blirp hook <agent> <event>` (plus `--global` for
/// entries installed into user configs).
pub fn hook_command(exe: &Path, agent: &str, event: &str, global: bool) -> String {
    let exe = slash_path(exe);
    let exe = if exe.contains(' ') {
        format!("\"{exe}\"")
    } else {
        exe
    };
    let mut cmd = format!("{exe} hook {agent} {event}");
    if global {
        cmd.push_str(" --global");
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_exe_leaves_the_appimage_mount() {
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join(".mount_blirpX");
        let installed = dir.path().join("bin/blirp");
        for f in [mount.join("usr/bin/blirp"), installed.clone()] {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, "").unwrap();
        }
        let inside = mount.join("usr/bin/blirp");

        // A normal install is used as is, without looking further.
        // Only the mount counts as temporary here: tempdir() itself is under
        // /tmp on Linux.
        let roots = [mount.as_path()];
        let plain = persistent_exe_from(installed.clone(), false, &roots, || -> Vec<PathBuf> {
            panic!("no lookup needed")
        });
        assert_eq!(plain.unwrap(), installed);

        // In an AppImage: the first candidate outside the mount that exists.
        let found = persistent_exe_from(inside.clone(), true, &roots, || {
            vec![
                dir.path().join("missing/blirp"),
                inside.clone(),
                installed.clone(),
            ]
        });
        assert_eq!(
            found.unwrap(),
            dunce::canonicalize(&installed).unwrap(),
            "receipt/PATH copy, never the mount"
        );

        let none = persistent_exe_from(inside.clone(), true, &roots, || vec![inside.clone()]);
        let err = none.unwrap_err().to_string();
        assert!(err.contains("temporary location"), "{err}");
        assert!(under(
            Path::new("/tmp/.mount_x/usr/bin"),
            &[Path::new("/tmp")]
        ));
        assert!(!under(Path::new("/usr/bin"), &[Path::new("/tmp")]));

        // The daemon: the receipt's CLI from an AppImage when it is this
        // version, else the running binary.
        let same = crate::update::CURRENT.to_string();
        let daemon = daemon_exe_from(inside.clone(), true, &roots, || {
            Some((installed.clone(), format!("v{same}")))
        });
        assert_eq!(daemon, dunce::canonicalize(&installed).unwrap());
        let daemon = daemon_exe_from(inside.clone(), true, &roots, || {
            Some((installed.clone(), "0.0.1".to_string()))
        });
        assert_eq!(daemon, inside, "another version: run from the mount");
        let daemon = daemon_exe_from(inside.clone(), true, &roots, || {
            Some((installed.clone(), "garbage".to_string()))
        });
        assert_eq!(daemon, inside, "unreadable version: run from the mount");
        let daemon = daemon_exe_from(inside.clone(), true, &roots, || None);
        assert_eq!(daemon, inside, "no installed CLI: run from the mount");
        let own = dir.path().join("blirp");
        let daemon = daemon_exe_from(own.clone(), false, &roots, || {
            panic!("no lookup outside an AppImage")
        });
        assert_eq!(daemon, own);
    }
}
