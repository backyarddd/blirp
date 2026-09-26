//! git via the `git` CLI (optional dependency: every caller handles git being
//! absent). All calls have timeouts and never prompt.

use crate::model::{GitStatus, GitStatusEntry};
use crate::process::{self, RunError};
use std::path::{Path, PathBuf};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);
/// Output cap for regular commands; diffs pass their own cap.
const MAX_OUTPUT: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git is not installed or not on PATH")]
    NotInstalled,
    #[error(transparent)]
    Run(RunError),
    #[error("git {args} failed: {stderr}")]
    Failed { args: String, stderr: String },
}

/// Config forced onto every call. A cloned repo's own config is untrusted and
/// must never get to run a command during our read-only queries. Passed as
/// `GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n` (command scope, overrides repo
/// config; keys need no `=` escaping unlike `-c`).
const SAFE_CONFIG: &[(&str, &str)] = &[
    ("core.fsmonitor", "false"),
    ("diff.external", ""),
    // Inline submodule diffs and status summaries spawn git inside submodules,
    // whose config we have not neutralized.
    ("diff.submodule", "short"),
    ("status.submoduleSummary", "false"),
];

fn git(dir: &Path, args: &[&str], cap: usize) -> Result<Vec<u8>, GitError> {
    git_with(dir, args, cap, &[])
}

/// `git` with `extra` config layered on top of [`SAFE_CONFIG`].
fn git_with(
    dir: &Path,
    args: &[&str],
    cap: usize,
    extra: &[(String, String)],
) -> Result<Vec<u8>, GitError> {
    let mut cmd = process::command("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        // Read-only queries must never take index.lock away from the user's own git.
        .env("GIT_OPTIONAL_LOCKS", "0")
        // Paths we pass come from API clients; pathspec magic such as `:(top)x`
        // or `:/` would reach outside the project folder.
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("LC_ALL", "C");
    let config = SAFE_CONFIG
        .iter()
        .copied()
        .chain(extra.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let mut count = 0usize;
    for (i, (k, v)) in config.enumerate() {
        cmd.env(format!("GIT_CONFIG_KEY_{i}"), k)
            .env(format!("GIT_CONFIG_VALUE_{i}"), v);
        count = i + 1;
    }
    cmd.env("GIT_CONFIG_COUNT", count.to_string());
    let out = process::run(cmd, TIMEOUT, cap).map_err(|e| match e {
        RunError::NotFound(_) => GitError::NotInstalled,
        other => GitError::Run(other),
    })?;
    if out.success() {
        Ok(out.stdout)
    } else {
        Err(GitError::Failed {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// Overrides disabling every clean/smudge filter driver the repo's own config
/// (local, worktree or included from them) defines or redefines: `status` and
/// `diff` pipe work tree files through them. Drivers only defined in the
/// user's global/system config (git-lfs) are the user's own and keep working.
fn repo_filter_overrides(dir: &Path) -> Result<Vec<(String, String)>, GitError> {
    let out = match git(
        dir,
        &["config", "-z", "--show-scope", "--get-regexp", r"^filter\."],
        MAX_OUTPUT,
    ) {
        Ok(o) => o,
        // Exit status 1: no filter configured anywhere.
        Err(GitError::Failed { .. }) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&out);
    // `-z --show-scope` emits `<scope>\0<key>\n<value>\0` per entry.
    let mut fields = text.split('\0');
    let mut names: Vec<&str> = Vec::new();
    while let (Some(scope), Some(entry)) = (fields.next(), fields.next()) {
        if matches!(scope, "system" | "global") {
            continue;
        }
        let key = entry.split('\n').next().unwrap_or("");
        if let Some((name, _)) = key.strip_prefix("filter.").and_then(|k| k.rsplit_once('.'))
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    Ok(names
        .into_iter()
        .flat_map(|n| {
            [
                (format!("filter.{n}.clean"), String::new()),
                (format!("filter.{n}.smudge"), String::new()),
                (format!("filter.{n}.process"), String::new()),
                // An empty driver is a no-op, which `required` would turn into a hard error.
                (format!("filter.{n}.required"), "false".to_string()),
            ]
        })
        .collect())
}

fn git_line(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    Ok(String::from_utf8_lossy(&git(dir, args, MAX_OUTPUT)?)
        .trim()
        .to_string())
}

pub fn is_installed() -> bool {
    let mut cmd = process::command("git");
    cmd.arg("--version");
    matches!(process::run(cmd, TIMEOUT, 4096), Ok(o) if o.success())
}

/// Nearest ancestor of `path` (inclusive) containing a `.git` entry. Pure
/// filesystem check, used where spawning git per item would be too slow.
pub fn find_git_root_fs(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| p.join(".git").exists())
        .map(Path::to_path_buf)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// Work tree top level containing the queried path (a linked worktree's own root).
    pub toplevel: PathBuf,
    /// Root of the main work tree (differs from `toplevel` for linked worktrees).
    pub main_root: PathBuf,
    /// Normalized remote (`host/owner/repo`) of `origin`, else of the first remote.
    pub remote: Option<String>,
}

/// Inspect the repo containing `dir`. `Ok(None)` when `dir` is not in a git
/// work tree or git is not installed.
pub fn repo_info(dir: &Path) -> Result<Option<RepoInfo>, GitError> {
    let out = match git(
        dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ],
        MAX_OUTPUT,
    ) {
        Ok(o) => String::from_utf8_lossy(&o).into_owned(),
        Err(GitError::NotInstalled | GitError::Failed { .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut lines = out.lines();
    let (Some(top), Some(common)) = (lines.next(), lines.next()) else {
        return Ok(None);
    };
    let toplevel = canonical(Path::new(top.trim()));
    let common = canonical(Path::new(common.trim()));
    // `<main>/.git` is the common dir of every linked worktree; anything else
    // (bare repo, separate git dir) has no main work tree we can point at.
    let main_root = match (common.file_name(), common.parent()) {
        (Some(n), Some(parent)) if n == ".git" => parent.to_path_buf(),
        _ => toplevel.clone(),
    };
    let remote = remote_url(&toplevel).and_then(|u| normalize_remote(&u));
    Ok(Some(RepoInfo {
        toplevel,
        main_root,
        remote,
    }))
}

fn canonical(p: &Path) -> PathBuf {
    dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn remote_url(dir: &Path) -> Option<String> {
    if let Ok(url) = git_line(dir, &["remote", "get-url", "origin"]) {
        return Some(url).filter(|u| !u.is_empty());
    }
    let first = git_line(dir, &["remote"]).ok()?;
    let name = first.lines().next()?.trim().to_string();
    git_line(dir, &["remote", "get-url", &name])
        .ok()
        .filter(|u| !u.is_empty())
}

/// Normalize a git remote URL to lowercase `host/owner/repo`, so https, ssh
/// and scp-style URLs of the same repo compare equal. Local paths and
/// `file://` remotes have no network identity and return `None`.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?;
        // Ports differ between ssh and https for the same repo; drop them.
        let host = match host.rsplit_once(':') {
            Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
            _ => host,
        };
        (host, path)
    } else {
        // scp-like `[user@]host:path`; a Windows drive (`C:\x`) or a plain path is local.
        let (left, path) = url.split_once(':')?;
        if left.len() <= 1 || left.contains(['/', '\\']) || path.starts_with('\\') {
            return None;
        }
        (left.rsplit('@').next()?, path)
    };
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{host}/{path}").to_lowercase())
}

/// Current branch name, `None` when detached or not a repo.
pub fn current_branch(dir: &Path) -> Option<String> {
    git_line(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .filter(|b| !b.is_empty())
}

pub fn status(root: &Path) -> Result<GitStatus, GitError> {
    let filters = repo_filter_overrides(root)?;
    let out = git_with(
        root,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
            // Checking a submodule's work tree runs git with the submodule's own config.
            "--ignore-submodules=dirty",
        ],
        MAX_OUTPUT,
        &filters,
    )?;
    Ok(parse_status(&out, root))
}

/// Parse `git status --porcelain=v2 --branch -z`.
pub fn parse_status(out: &[u8], root: &Path) -> GitStatus {
    let text = String::from_utf8_lossy(out);
    let mut st = GitStatus {
        is_git: true,
        root: root.to_string_lossy().into_owned(),
        branch: None,
        head: None,
        upstream: None,
        ahead: 0,
        behind: 0,
        entries: Vec::new(),
    };
    let mut fields = text.split('\0').filter(|s| !s.is_empty());
    while let Some(rec) = fields.next() {
        if let Some(h) = rec.strip_prefix("# ") {
            let (key, val) = h.split_once(' ').unwrap_or((h, ""));
            match key {
                "branch.oid" if val != "(initial)" => st.head = Some(val.into()),
                "branch.head" if val != "(detached)" => st.branch = Some(val.into()),
                "branch.upstream" => st.upstream = Some(val.into()),
                "branch.ab" => {
                    for part in val.split_whitespace() {
                        if let Some(n) = part.strip_prefix('+') {
                            st.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            st.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let entry = |xy: &str, path: &str, orig: Option<&str>, conflicted| {
            let mut c = xy.chars();
            GitStatusEntry {
                path: path.to_string(),
                orig_path: orig.map(str::to_string),
                index: c.next().unwrap_or('.').to_string(),
                worktree: c.next().unwrap_or('.').to_string(),
                conflicted,
            }
        };
        match rec.as_bytes().first() {
            // 1 XY sub mH mI mW hH hI path
            Some(b'1') => {
                let p: Vec<&str> = rec.splitn(9, ' ').collect();
                if let (Some(xy), Some(path)) = (p.get(1), p.get(8)) {
                    st.entries.push(entry(xy, path, None, false));
                }
            }
            // 2 XY sub mH mI mW hH hI Xscore path \0 origPath
            Some(b'2') => {
                let p: Vec<&str> = rec.splitn(10, ' ').collect();
                let orig = fields.next();
                if let (Some(xy), Some(path)) = (p.get(1), p.get(9)) {
                    st.entries.push(entry(xy, path, orig, false));
                }
            }
            // u XY sub m1 m2 m3 mW h1 h2 h3 path
            Some(b'u') => {
                let p: Vec<&str> = rec.splitn(11, ' ').collect();
                if let (Some(xy), Some(path)) = (p.get(1), p.get(10)) {
                    st.entries.push(entry(xy, path, None, true));
                }
            }
            Some(b'?') => {
                if let Some(path) = rec.get(2..) {
                    st.entries.push(entry("??", path, None, false));
                }
            }
            _ => {}
        }
    }
    st
}

/// Unified diff of the work tree against HEAD (or the index before the first
/// commit), optionally limited to one relative path. Returns (diff, truncated).
pub fn diff(root: &Path, rel_path: Option<&str>, cap: usize) -> Result<(String, bool), GitError> {
    let filters = repo_filter_overrides(root)?;
    let run = |base: &[&str]| {
        let mut args: Vec<&str> = base.to_vec();
        // External diff drivers and textconv are commands named by repo config.
        args.extend([
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=dirty",
        ]);
        if let Some(p) = rel_path {
            args.extend(["--", p]);
        }
        git_with(root, &args, cap + 1, &filters)
    };
    let out = match run(&["diff", "HEAD"]) {
        Ok(o) => o,
        // No commits yet: HEAD does not resolve.
        Err(GitError::Failed { .. }) => run(&["diff", "--cached"])?,
        Err(e) => return Err(e),
    };
    let truncated = out.len() > cap;
    let slice = &out[..out.len().min(cap)];
    Ok((String::from_utf8_lossy(slice).into_owned(), truncated))
}

/// `git worktree add <path> -b <branch>` in the repo at `repo`.
pub fn worktree_add(repo: &Path, path: &Path, branch: &str) -> Result<(), GitError> {
    let path_s = path.to_string_lossy();
    git(
        repo,
        &["worktree", "add", &path_s, "-b", branch],
        MAX_OUTPUT,
    )
    .map(|_| ())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// `git` with deterministic identity for test repos.
    pub(crate) fn run_git(dir: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(st.status.success(), "git {args:?}: {st:?}");
    }

    #[test]
    fn normalize_remote_variants() {
        let want = Some("github.com/owner/repo".to_string());
        for url in [
            "https://github.com/Owner/Repo.git",
            "https://user:pw@github.com/owner/repo",
            "https://github.com:443/owner/repo/",
            "ssh://git@github.com/owner/repo.git",
            "ssh://git@github.com:22/owner/repo.git",
            "git@github.com:owner/repo.git",
            "github.com:owner/repo",
            "git://github.com/owner/repo.git",
        ] {
            assert_eq!(normalize_remote(url), want, "{url}");
        }
        assert_eq!(
            normalize_remote("https://gitlab.com/group/sub/proj.git").as_deref(),
            Some("gitlab.com/group/sub/proj")
        );
        for local in [
            "/srv/git/repo.git",
            "../repo",
            "C:\\repos\\x.git",
            "C:/repos/x.git",
            "file:///srv/repo.git",
            "",
        ] {
            assert_eq!(normalize_remote(local), None, "{local}");
        }
    }

    #[test]
    fn parse_status_v2() {
        let out = b"# branch.oid 0123abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaa bbb src/a b.rs\0\
2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\0old.rs\0\
u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
? untracked.txt\0";
        let st = parse_status(out, Path::new("/r"));
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.head.as_deref(), Some("0123abc"));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        assert_eq!(st.entries.len(), 4);
        assert_eq!(st.entries[0].path, "src/a b.rs");
        assert_eq!(st.entries[0].worktree, "M");
        assert_eq!(st.entries[1].orig_path.as_deref(), Some("old.rs"));
        assert!(st.entries[2].conflicted);
        assert_eq!(st.entries[3].index, "?");
        let detached = parse_status(
            b"# branch.oid (initial)\0# branch.head (detached)\0",
            Path::new("/r"),
        );
        assert_eq!((detached.head, detached.branch), (None, None));
    }

    #[test]
    fn repo_info_and_worktree() {
        if !is_installed() {
            eprintln!("git missing; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init"]);
        run_git(
            &repo,
            &["remote", "add", "origin", "git@github.com:o/r.git"],
        );
        std::fs::write(repo.join("f.txt"), "one\n").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);
        std::fs::create_dir(repo.join("sub")).unwrap();

        let info = repo_info(&repo.join("sub")).unwrap().unwrap();
        let canon = dunce::canonicalize(&repo).unwrap();
        assert_eq!(info.toplevel, canon);
        assert_eq!(info.main_root, canon);
        assert_eq!(info.remote.as_deref(), Some("github.com/o/r"));
        assert_eq!(current_branch(&repo).as_deref(), Some("main"));

        let wt = dir.path().join("wt");
        worktree_add(&repo, &wt, "blirp/test").unwrap();
        let wi = repo_info(&wt).unwrap().unwrap();
        assert_eq!(wi.toplevel, dunce::canonicalize(&wt).unwrap());
        assert_eq!(wi.main_root, canon);

        std::fs::write(repo.join("f.txt"), "two\n").unwrap();
        let st = status(&repo).unwrap();
        assert_eq!(st.entries.len(), 1);
        let (d, truncated) = diff(&repo, Some("f.txt"), 1 << 20).unwrap();
        assert!(d.contains("+two") && !truncated, "{d}");

        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(repo_info(&plain).unwrap(), None);
    }

    #[test]
    fn pathspec_magic_stays_inside_root() {
        if !is_installed() {
            eprintln!("git missing; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        run_git(&repo, &["init"]);
        std::fs::write(repo.join("outside.txt"), "one\n").unwrap();
        std::fs::write(repo.join("sub/inside.txt"), "one\n").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);
        std::fs::write(repo.join("outside.txt"), "leak\n").unwrap();
        std::fs::write(repo.join("sub/inside.txt"), "two\n").unwrap();

        let sub = repo.join("sub");
        let (d, _) = diff(&sub, Some("inside.txt"), 1 << 20).unwrap();
        assert!(d.contains("+two"), "{d}");
        for magic in [":(top)outside.txt", ":/outside.txt", ":/", ":(glob)**", "*"] {
            let (d, _) = diff(&sub, Some(magic), 1 << 20).unwrap();
            assert!(!d.contains("leak"), "{magic}: {d}");
        }
    }

    /// A cloned repo's config names commands; none of them may run when we
    /// only read the repo.
    #[test]
    fn untrusted_repo_config_never_runs_commands() {
        if !is_installed() {
            eprintln!("git missing; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init"]);
        std::fs::write(repo.join(".gitattributes"), "f.txt filter=evil diff=evil\n").unwrap();
        std::fs::write(repo.join("f.txt"), "one\n").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", "init"]);
        // Each command drops a marker next to the repo (hooks run in the work tree).
        for (key, cmd) in [
            ("core.fsmonitor", "touch ../fsmonitor.ran; true"),
            ("filter.evil.clean", "touch ../clean.ran; cat"),
            ("filter.evil.smudge", "touch ../smudge.ran; cat"),
            ("filter.evil.required", "true"),
            ("diff.evil.textconv", "touch ../textconv.ran; cat"),
            ("diff.evil.command", "touch ../extdiff.ran; true"),
            ("diff.external", "touch ../external.ran; true"),
        ] {
            run_git(&repo, &["config", key, cmd]);
        }
        std::fs::write(repo.join("f.txt"), "two\n").unwrap();
        let markers = ["fsmonitor.ran", "clean.ran", "textconv.ran"].map(|m| dir.path().join(m));

        // Control: stock git runs them, so the assertions below mean something.
        for args in [
            &["status"][..],
            &["diff", "HEAD"],
            &["diff", "HEAD", "--no-ext-diff"],
        ] {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
        }
        for m in &markers {
            assert!(m.exists(), "control: {} not created", m.display());
        }
        let all = [
            "fsmonitor.ran",
            "clean.ran",
            "smudge.ran",
            "textconv.ran",
            "extdiff.ran",
            "external.ran",
        ];
        for m in all {
            let _ = std::fs::remove_file(dir.path().join(m));
        }

        let st = status(&repo).unwrap();
        assert_eq!(st.entries.len(), 1, "{st:?}");
        let (d, _) = diff(&repo, None, 1 << 20).unwrap();
        assert!(d.contains("+two"), "{d}");
        diff(&repo, Some("f.txt"), 1 << 20).unwrap();
        repo_info(&repo).unwrap().unwrap();
        current_branch(&repo);
        for m in all {
            assert!(!dir.path().join(m).exists(), "{m} ran");
        }
    }
}
