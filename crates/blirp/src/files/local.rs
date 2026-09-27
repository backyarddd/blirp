//! This machine's side of a working copy: which folders may sync at all,
//! scanning and hashing one with its cache, the preview (dry run) and the
//! git manifest. Blocking; callers run it on the blocking pool.

use blirp_core::config::FilesConfig;
use blirp_core::files::GitManifest;
use blirp_core::files::disk::{self, DriveKind};
use blirp_core::files::rules::Reason;
use blirp_core::files::scan::{self, HashPass, Scan, ScanConfig, ScanState};
use blirp_core::model::{ExcludedGroup, FilesPreview, FilesScanState, Project};
use blirp_core::paths::path_key;
use blirp_core::process;
use blirp_core::store::{NonProjectDirs, Store};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

/// Files per root before the whole root is paused.
pub const MAX_ROOT_FILES: usize = 100_000;
/// Paths listed per exclusion reason.
pub const LISTED: usize = 50;

pub fn scan_config(cfg: &FilesConfig, data_dir: &Path) -> ScanConfig {
    ScanConfig {
        max_file_bytes: u64::from(cfg.max_file_mb) << 20,
        max_root_bytes: u64::from(cfg.max_root_gb) << 30,
        max_files: MAX_ROOT_FILES,
        data_dir: Some(data_dir.to_path_buf()),
    }
}

/// Whether `root` is the top of a git work tree (`.git` folder or file).
pub fn is_git(root: &Path) -> bool {
    std::fs::symlink_metadata(root.join(".git")).is_ok()
}

/// Buckets that hold sessions of no project: a machine's Chats project
/// (§5), or the pre-Chats Home project it replaces.
pub fn is_bucket(store: &Store, project: &Project) -> bool {
    project.chats
        || store
            .home_project_id()
            .ok()
            .flatten()
            .is_some_and(|h| h == project.id)
}

/// Why `root` of `project` is never synced, or None when it may be.
/// Buckets (Home), scratch projects (made by resolution in a scratch folder
/// and never touched by the user), anything inside blirp's data folder but
/// folderless-project workspaces, and removable or network drives.
pub fn never_synced(
    store: &Store,
    data_dir: &Path,
    project: &Project,
    root: &Path,
) -> Option<String> {
    if is_bucket(store, project) {
        return Some("sessions without a project folder are never synced".into());
    }
    let data = path_key(&dunce::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf()));
    let key = path_key(root);
    if key.starts_with(&data) && !key.starts_with(data.join("workspaces")) {
        return Some("folders inside blirp's data folder are never synced".into());
    }
    if key.starts_with(data.join("workspaces")) {
        // A project's workspace: synced like any folder.
        return None;
    }
    if project.updated_at == project.created_at && NonProjectDirs::from_process().contains(root) {
        return Some("scratch folders (temp, tool data) are never synced".into());
    }
    match disk::drive_kind(root) {
        DriveKind::Removable => Some("folders on removable drives are not synced".into()),
        DriveKind::Network => Some("folders on network drives are not synced".into()),
        DriveKind::Local => None,
    }
}

/// A scanned and hashed working copy.
pub struct LocalScan {
    pub scan: Scan,
    pub hashed: HashPass,
    pub git: bool,
}

/// Scan `root` (cache rows under `copy`) and hash what it found. Nothing is
/// hashed while the scan is busy or too large.
pub fn scan_copy(
    store: &Store,
    copy: &str,
    root: &Path,
    cfg: &ScanConfig,
) -> std::io::Result<LocalScan> {
    let git = is_git(root);
    let scan = scan::scan(root, git, cfg)?;
    if scan.state != ScanState::Ok {
        return Ok(LocalScan {
            scan,
            hashed: HashPass::default(),
            git,
        });
    }
    let cache = store.file_hash_cache(copy).map_err(std::io::Error::other)?;
    let hashed = scan::hash_all(root, &scan.found, &cache, blirp_core::now_ms());
    let keep: HashSet<String> = scan.found.iter().map(|f| f.path.clone()).collect();
    store
        .save_file_hash_cache(copy, &hashed.cache_updates, Some(&keep))
        .map_err(std::io::Error::other)?;
    Ok(LocalScan { scan, hashed, git })
}

pub fn scan_state(s: ScanState) -> FilesScanState {
    match s {
        ScanState::Ok => FilesScanState::Ok,
        ScanState::Busy => FilesScanState::Busy,
        ScanState::TooLarge => FilesScanState::TooLarge,
    }
}

/// Excluded paths grouped by reason (secrets found by content included).
pub fn excluded_groups(ls: &LocalScan) -> Vec<ExcludedGroup> {
    let mut groups: BTreeMap<Reason, (i64, Vec<String>)> = BTreeMap::new();
    let mut add = |reason: Reason, path: String| {
        let g = groups.entry(reason).or_default();
        g.0 += 1;
        if g.1.len() < LISTED {
            g.1.push(path);
        }
    };
    for e in &ls.scan.excluded {
        add(
            e.reason,
            if e.dir {
                format!("{}/", e.path)
            } else {
                e.path.clone()
            },
        );
    }
    for p in &ls.hashed.secrets {
        add(Reason::Secret, p.clone());
    }
    groups
        .into_iter()
        .map(|(reason, (count, paths))| ExcludedGroup {
            reason,
            count,
            paths,
        })
        .collect()
}

pub fn preview(ls: &LocalScan, root: &Path, never: Option<String>) -> FilesPreview {
    let files = &ls.hashed.files;
    let mut reincluded: Vec<String> = ls
        .scan
        .reincluded_secrets
        .iter()
        .chain(&ls.hashed.reincluded_secrets)
        .cloned()
        .collect();
    reincluded.sort();
    reincluded.dedup();
    FilesPreview {
        root: root.display().to_string(),
        state: scan_state(ls.scan.state),
        never_synced: never,
        files: i64::try_from(files.len()).unwrap_or(i64::MAX),
        bytes: i64::try_from(files.iter().map(|f| f.size).sum::<u64>()).unwrap_or(i64::MAX),
        excluded: excluded_groups(ls),
        reincluded_secrets: reincluded,
    }
}

fn git_line(root: &Path, args: &[&str]) -> Option<String> {
    let mut c = process::command("git");
    c.arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    let out = process::run(c, Duration::from_secs(10), 64 * 1024).ok()?;
    if !out.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// The git manifest of a work tree (remote without credentials).
pub fn git_manifest(root: &Path) -> GitManifest {
    let remote =
        blirp_core::git::remote_url(root).and_then(|u| crate::clone::sanitize_url(&u).ok());
    let m = GitManifest {
        remote,
        branch: git_line(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]),
        head_sha: git_line(root, &["rev-parse", "--verify", "--quiet", "HEAD"]),
        upstream_sha: git_line(root, &["rev-parse", "--verify", "--quiet", "@{u}"]),
    };
    if m.is_valid() {
        m
    } else {
        GitManifest::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_groups_and_lists() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        let r = &dir.path().join("proj");
        std::fs::create_dir(r).unwrap();
        std::fs::write(r.join("a.txt"), "hello").unwrap();
        std::fs::write(r.join(".env"), "SECRET=1").unwrap();
        std::fs::write(r.join("k.txt"), "-----BEGIN RSA PRIVATE KEY-----").unwrap();
        std::fs::create_dir(r.join("node_modules")).unwrap();
        for i in 0..60 {
            std::fs::write(r.join(format!("x{i}.pyc")), "").unwrap();
        }
        let cfg = scan_config(&FilesConfig::default(), &dir.path().join("data"));
        let ls = scan_copy(&store, "c", r, &cfg).unwrap();
        let p = preview(&ls, r, None);
        assert_eq!((p.files, p.bytes, p.state), (1, 5, FilesScanState::Ok));
        let ignored = p
            .excluded
            .iter()
            .find(|g| g.reason == Reason::Ignored)
            .unwrap();
        assert_eq!(ignored.count, 61);
        assert_eq!(ignored.paths.len(), LISTED);
        assert!(ignored.paths.contains(&"node_modules/".to_string()));
        let secret = p
            .excluded
            .iter()
            .find(|g| g.reason == Reason::Secret)
            .unwrap();
        assert_eq!(secret.paths, [".env", "k.txt"]);
        // The cache holds the hashed files.
        assert_eq!(store.file_hash_cache("c").unwrap().len(), 2);
    }
}
