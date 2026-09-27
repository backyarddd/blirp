//! Read-only project files and git views. Every path stays inside a
//! registered root of the project on this machine.

use super::{ApiError, ApiPath, ApiQuery, ApiResult, FilesAccess, blocking};
use crate::state::SharedState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use blirp_core::git::{self, GitError};
use blirp_core::model::{DirListing, FileContent, FileEntry, FileKind, GitDiff, GitStatus};
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/projects/{id}/files", get(list_dir))
        .route("/api/projects/{id}/files/content", get(read_file))
        .route("/api/projects/{id}/git", get(git_status))
        .route("/api/projects/{id}/git/diff", get(git_diff))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathQuery {
    /// Relative path inside the root; empty for the root itself.
    #[serde(default)]
    path: Option<String>,
    /// Absolute project root to use when the project has several folders here.
    #[serde(default)]
    root: Option<String>,
}

/// Validate a client-supplied relative path: only normal components, no
/// `..`, no absolute or drive-prefixed paths.
pub fn check_relative(rel: &str) -> ApiResult<PathBuf> {
    if rel.contains('\0') {
        return Err(ApiError::bad_request("path contains a NUL byte"));
    }
    let mut out = PathBuf::new();
    for c in std::path::Path::new(rel).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "path_outside_root",
                    "path must be relative to the project root and must not contain '..'",
                ));
            }
        }
    }
    Ok(out)
}

/// Resolve `rel` under `root` following symlinks, rejecting anything whose
/// real location is outside the real root, or inside blirp's `data_dir`:
/// that holds the runtime token and the machine identity, and a project
/// folder may contain it (a home folder registered as a project, read by
/// another machine through the hub).
pub fn resolve_inside(root: &Path, rel: &str, data_dir: &Path) -> ApiResult<PathBuf> {
    let rel = check_relative(rel)?;
    let root = dunce::canonicalize(root).map_err(|e| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "root_missing",
            format!("project folder is not accessible: {e}"),
        )
    })?;
    let target = dunce::canonicalize(root.join(&rel)).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ApiError::not_found("path"),
        _ => ApiError::bad_request(format!("cannot access path: {e}")),
    })?;
    if !target.starts_with(&root) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "path_outside_root",
            "path resolves outside the project folder",
        ));
    }
    let data_dir = dunce::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    // Session worktrees (checkouts of the user's repos) and the workspaces
    // of projects without folders live there too.
    if target.starts_with(&data_dir)
        && !target.starts_with(data_dir.join("worktrees"))
        && !target.starts_with(data_dir.join("workspaces"))
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "path_in_data_dir",
            "blirp's own data folder is not served",
        ));
    }
    Ok(target)
}

/// The project root on this machine: `root` if given (must be registered), else the first one.
/// A project without folders: its blirp workspace here (created on demand, empty).
fn project_root(s: &SharedState, id: &str, root: Option<&str>) -> ApiResult<PathBuf> {
    let project = s.store.live_project(id)?;
    let mut roots = s.store.local_roots(id, &s.machine.id)?;
    if roots.is_empty() && !project.chats && s.store.project_paths(id)?.is_empty() {
        roots.push(
            s.paths
                .ensure_workspace(id)
                .map_err(|e| ApiError::internal("creating the project workspace", e))?,
        );
    }
    match root {
        Some(r) => roots
            .into_iter()
            .find(|p| p.as_os_str() == r)
            .ok_or_else(|| {
                ApiError::bad_request("root is not a folder of this project on this machine")
            }),
        None => roots.into_iter().next().ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "no_local_folder",
                "project has no folder on this machine",
            )
        }),
    }
}

fn rel_string(p: &std::path::Path) -> String {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn mtime_ms(m: &std::fs::Metadata) -> Option<i64> {
    m.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
}

async fn list_dir(
    State(s): State<SharedState>,
    _files: FilesAccess,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<PathQuery>,
) -> ApiResult<Json<DirListing>> {
    let st = s.clone();
    blocking(move || {
        let root = project_root(&st, &id, q.root.as_deref())?;
        let rel = q.path.unwrap_or_default();
        let dir = resolve_inside(&root, &rel, st.paths.home())?;
        if !dir.is_dir() {
            return Err(ApiError::bad_request("path is not a directory"));
        }
        let rel_dir = check_relative(&rel)?;
        let read = std::fs::read_dir(&dir)
            .map_err(|e| ApiError::bad_request(format!("cannot list directory: {e}")))?;
        let mut entries = Vec::new();
        for entry in read {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::debug!(error = %e, "skipping unreadable directory entry");
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata().ok();
            let kind = match entry.file_type() {
                Ok(t) if t.is_symlink() => FileKind::Symlink,
                Ok(t) if t.is_dir() => FileKind::Dir,
                Ok(t) if t.is_file() => FileKind::File,
                _ => FileKind::Other,
            };
            entries.push(FileEntry {
                path: rel_string(&rel_dir.join(&name)),
                name,
                kind,
                size: meta
                    .as_ref()
                    .map_or(0, |m| i64::try_from(m.len()).unwrap_or(i64::MAX)),
                modified_at: meta.as_ref().and_then(mtime_ms),
            });
        }
        entries.sort_by(|a, b| {
            (a.kind != FileKind::Dir)
                .cmp(&(b.kind != FileKind::Dir))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(DirListing {
            root: root.display().to_string(),
            path: rel_string(&rel_dir),
            entries,
        })
    })
    .await
    .map(Json)
}

async fn read_file(
    State(s): State<SharedState>,
    _files: FilesAccess,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<PathQuery>,
) -> ApiResult<Json<FileContent>> {
    let st = s.clone();
    blocking(move || {
        let root = project_root(&st, &id, q.root.as_deref())?;
        let rel = q
            .path
            .filter(|p| !p.is_empty())
            .ok_or_else(|| ApiError::bad_request("path is required"))?;
        let file = resolve_inside(&root, &rel, st.paths.home())?;
        read_text(&file).map(|(content, size)| FileContent {
            root: root.display().to_string(),
            path: rel_string(&check_relative(&rel).unwrap_or_default()),
            size,
            content,
        })
    })
    .await
    .map(Json)
}

/// Read a UTF-8 text file of at most [`MAX_FILE_BYTES`].
pub fn read_text(file: &std::path::Path) -> ApiResult<(String, i64)> {
    use std::io::Read;
    let meta = std::fs::metadata(file)
        .map_err(|e| ApiError::bad_request(format!("cannot read file: {e}")))?;
    if !meta.is_file() {
        return Err(ApiError::bad_request("path is not a file"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_too_large",
            "file is larger than 1 MiB",
        ));
    }
    let mut buf = Vec::new();
    std::fs::File::open(file)
        .and_then(|f| f.take(MAX_FILE_BYTES + 1).read_to_end(&mut buf))
        .map_err(|e| ApiError::bad_request(format!("cannot read file: {e}")))?;
    if buf.len() as u64 > MAX_FILE_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_too_large",
            "file is larger than 1 MiB",
        ));
    }
    let binary = || {
        ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "binary_file",
            "file is not UTF-8 text",
        )
    };
    if buf.contains(&0) {
        return Err(binary());
    }
    let size = i64::try_from(buf.len()).unwrap_or(i64::MAX);
    String::from_utf8(buf)
        .map(|s| (s, size))
        .map_err(|_| binary())
}

fn git_err(e: GitError) -> ApiError {
    match e {
        GitError::NotInstalled => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "git_not_installed",
            "git is not installed or not on PATH",
        ),
        other => ApiError::internal("git", other),
    }
}

fn git_root(st: &SharedState, id: &str, root: Option<&str>) -> ApiResult<PathBuf> {
    let root = project_root(st, id, root)?;
    if git::find_git_root_fs(&root).is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_git",
            "project folder is not a git repository",
        ));
    }
    Ok(root)
}

async fn git_status(
    State(s): State<SharedState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<PathQuery>,
) -> ApiResult<Json<GitStatus>> {
    let st = s.clone();
    blocking(move || {
        let root = git_root(&st, &id, q.root.as_deref())?;
        git::status(&root).map_err(git_err)
    })
    .await
    .map(Json)
}

async fn git_diff(
    State(s): State<SharedState>,
    _files: FilesAccess,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<PathQuery>,
) -> ApiResult<Json<GitDiff>> {
    let st = s.clone();
    blocking(move || {
        let root = git_root(&st, &id, q.root.as_deref())?;
        // Deleted files still have diffs, so only validate the path shape.
        let rel = match q.path.as_deref().filter(|p| !p.is_empty()) {
            Some(p) => Some(rel_string(&check_relative(p)?)),
            None => None,
        };
        let (diff, truncated) =
            git::diff(&root, rel.as_deref(), MAX_FILE_BYTES as usize).map_err(git_err)?;
        Ok(GitDiff {
            path: rel,
            diff,
            truncated,
        })
    })
    .await
    .map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.txt"), "hi").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "no").unwrap();
        let data = dir.path().join("data");

        assert!(resolve_inside(&root, "src/a.txt", &data).is_ok());
        assert!(resolve_inside(&root, "./src/../src/a.txt", &data).is_err());
        for bad in ["../secret.txt", "src/../../secret.txt", "..", "/etc/passwd"] {
            let e = resolve_inside(&root, bad, &data).unwrap_err();
            assert_eq!(e.code, "path_outside_root", "{bad}");
        }
        #[cfg(windows)]
        for bad in [r"..\secret.txt", r"C:\Windows\win.ini", r"\\server\share\x"] {
            let e = resolve_inside(&root, bad, &data).unwrap_err();
            assert_eq!(e.code, "path_outside_root", "{bad}");
        }
        assert_eq!(
            resolve_inside(&root, "missing.txt", &data)
                .unwrap_err()
                .code,
            "not_found"
        );
        assert!(check_relative("a\0b").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(dir.path().join("secret.txt"), "no").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret.txt"), root.join("link")).unwrap();
        let data = dir.path().join("data");
        assert_eq!(
            resolve_inside(&root, "link", &data).unwrap_err().code,
            "path_outside_root"
        );
    }

    // A project folder that contains the data dir (a home folder) never
    // serves it: the runtime token and the machine key live there.
    #[test]
    fn data_dir_is_never_served() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join(".blirp");
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("runtime.json"), "{}").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hi").unwrap();
        let root = dir.path();
        assert!(resolve_inside(root, "notes.txt", &data).is_ok());
        for bad in [".blirp", ".blirp/runtime.json"] {
            let e = resolve_inside(root, bad, &data).unwrap_err();
            assert_eq!(e.code, "path_in_data_dir", "{bad}");
        }
        // Also when the data dir is the project folder itself.
        let e = resolve_inside(&data, "", &data).unwrap_err();
        assert_eq!(e.code, "path_in_data_dir");
        // Session worktrees are checkouts of the user's repos: served.
        std::fs::create_dir_all(data.join("worktrees/p/w")).unwrap();
        std::fs::write(data.join("worktrees/p/w/a.txt"), "x").unwrap();
        assert!(resolve_inside(root, ".blirp/worktrees/p/w/a.txt", &data).is_ok());
        // So are the workspaces of projects without folders.
        std::fs::create_dir_all(data.join("workspaces/p")).unwrap();
        std::fs::write(data.join("workspaces/p/.mcp.json"), "{}").unwrap();
        assert!(resolve_inside(&data.join("workspaces/p"), ".mcp.json", &data).is_ok());
    }

    #[test]
    fn text_only_and_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("t.txt");
        std::fs::write(&text, "héllo").unwrap();
        assert_eq!(read_text(&text).unwrap().0, "héllo");
        let bin = dir.path().join("b.bin");
        std::fs::write(&bin, [0u8, 1, 2]).unwrap();
        assert_eq!(read_text(&bin).unwrap_err().code, "binary_file");
        let big = dir.path().join("big.txt");
        std::fs::write(&big, vec![b'a'; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert_eq!(read_text(&big).unwrap_err().code, "file_too_large");
        assert!(read_text(dir.path()).is_err());
    }
}
