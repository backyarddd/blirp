//! Exclusion layers (design §2). Later layers win, so they are asked first:
//!
//! 1. always excluded, not overridable: VCS folders, blirp temp files and
//!    blirp's own data folder (enforced by the scanner, see `scan`);
//! 2. the build denylist ([`BUILD`], plus `*.exe` outside `bin/` of non-git
//!    roots and `*.zip` over 10 MB);
//! 3. `.gitignore` files (and `.git/info/exclude`);
//! 4. the secrets denylist ([`SECRETS`]) plus a content check for private
//!    keys ([`looks_like_private_key`]);
//! 5. `.blirpignore` files, which may re-include anything from 2-4 with
//!    `!pattern` (a re-included secret is reported as such).

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::LazyLock;
use ts_rs::TS;

/// Why a path is not synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// `.gitignore`, `.blirpignore` or the build denylist.
    Ignored,
    /// Secrets denylist or a private key found in the file.
    Secret,
    /// Over `files.max_file_mb`.
    TooLarge,
    /// Cannot be synced: a name that is not UTF-8 or not portable, a
    /// symlink leaving the folder, a device or pipe.
    Unsupported,
}

/// Layer 2: build output, caches and OS litter (gitignore syntax).
pub const BUILD: &[&str] = &[
    "node_modules/",
    "target/",
    "dist/",
    "build/",
    "out/",
    ".next/",
    ".nuxt/",
    ".svelte-kit/",
    ".turbo/",
    ".cache/",
    "__pycache__/",
    ".venv/",
    "venv/",
    ".tox/",
    ".gradle/",
    ".idea/",
    "*.pyc",
    ".DS_Store",
    "Thumbs.db",
    "*.o",
    "*.obj",
    "*.class",
    "*.dll",
    "*.so",
    "*.dylib",
    "*.iso",
    "*.dmg",
];

/// Layer 4: files that usually hold credentials (gitignore syntax; `!`
/// lines are the usual templates, which are not secrets).
pub const SECRETS: &[&str] = &[
    ".env",
    ".env.*",
    "*.env",
    ".envrc",
    "!.env.example",
    "!.env.sample",
    "!.env.template",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.jks",
    "*.keystore",
    "*.kdbx",
    "*.keychain*",
    "id_rsa*",
    "id_ed25519*",
    "id_ecdsa*",
    ".npmrc",
    ".pypirc",
    ".netrc",
    ".pgpass",
    "**/.kube/config",
    ".git-credentials",
    ".dockercfg",
    "**/.docker/config.json",
    "credentials.json",
    "service-account*.json",
    "*.tfstate*",
    ".aws/",
    ".ssh/",
    ".gnupg/",
];

/// `*.zip` files larger than this are build artifacts (layer 2).
pub const ZIP_LIMIT: u64 = 10 * 1024 * 1024;
/// Files at most this large are checked for private keys.
pub const KEY_CHECK_MAX: u64 = 1024 * 1024;
/// How much of a file the private key check reads.
pub const KEY_CHECK_HEAD: usize = 4096;

/// The name of per-folder blirp ignore files.
pub const BLIRPIGNORE: &str = ".blirpignore";

fn matcher(root: &Path, lines: &[&str]) -> Gitignore {
    let mut b = GitignoreBuilder::new(root);
    b.case_insensitive(true).ok();
    for l in lines {
        // The patterns are constants covered by tests; a bad one would be
        // a programming error caught there.
        let _ = b.add_line(None, l);
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

/// `-----BEGIN ... PRIVATE KEY-----`, as in `redact.rs`.
static PRIVATE_KEY: LazyLock<Option<regex::bytes::Regex>> = LazyLock::new(|| {
    regex::bytes::Regex::new(r"-----BEGIN[ A-Z0-9_-]{0,100}PRIVATE KEY(?: BLOCK)?-----").ok()
});

/// Whether the first bytes of a file contain a private key header.
pub fn looks_like_private_key(head: &[u8]) -> bool {
    let head = &head[..head.len().min(KEY_CHECK_HEAD)];
    PRIVATE_KEY.as_ref().is_some_and(|r| r.is_match(head))
}

/// The fixed layers for one root.
pub struct Rules {
    build: Gitignore,
    secrets: Gitignore,
    /// The root is a git work tree (changes the `*.exe` rule).
    git: bool,
}

/// One folder's ignore files, as the scanner descends.
#[derive(Default)]
pub struct Frame {
    pub gitignore: Option<Gitignore>,
    pub blirpignore: Option<Gitignore>,
}

impl Frame {
    /// Read `.gitignore` and `.blirpignore` of `dir` (missing files are
    /// fine; a file that cannot be read or parsed is logged and skipped,
    /// which can only make the sync include more, never write anything).
    pub fn load(dir: &Path) -> Frame {
        let read = |name: &str| {
            let file = dir.join(name);
            if !file.is_file() {
                return None;
            }
            let mut b = GitignoreBuilder::new(dir);
            b.case_insensitive(crate::files::path::case_insensitive_fs())
                .ok();
            if let Some(e) = b.add(&file) {
                tracing::warn!(file = %file.display(), error = %e, "ignore file partly unreadable");
            }
            match b.build() {
                Ok(g) if !g.is_empty() => Some(g),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(file = %file.display(), error = %e, "ignore file skipped");
                    None
                }
            }
        };
        Frame {
            gitignore: read(".gitignore"),
            blirpignore: read(BLIRPIGNORE),
        }
    }

    /// The user's global gitignore (`core.excludesFile`, else
    /// `$XDG_CONFIG_HOME/git/ignore` or `~/.config/git/ignore`) and
    /// `.git/info/exclude` of a repository root: the lowest gitignore.
    pub fn git_exclude(root: &Path) -> Frame {
        let mut b = GitignoreBuilder::new(root);
        b.case_insensitive(crate::files::path::case_insensitive_fs())
            .ok();
        let mut any = false;
        if let Some(global) = global_gitignore().filter(|f| f.is_file()) {
            let _ = b.add(&global);
            any = true;
        }
        let file = root.join(".git").join("info").join("exclude");
        if file.is_file() {
            let _ = b.add(&file);
            any = true;
        }
        if !any {
            return Frame::default();
        }
        Frame {
            gitignore: b.build().ok().filter(|g| !g.is_empty()),
            blirpignore: None,
        }
    }
}

/// The user's global gitignore file, as git finds it (read once).
fn global_gitignore() -> Option<std::path::PathBuf> {
    static FILE: LazyLock<Option<std::path::PathBuf>> = LazyLock::new(|| {
        let mut c = crate::process::command("git");
        c.args(["config", "--global", "--path", "--get", "core.excludesFile"]);
        let configured = crate::process::run(c, std::time::Duration::from_secs(5), 64 * 1024)
            .ok()
            .filter(|o| o.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from);
        configured.or_else(|| {
            let xdg = std::env::var_os("XDG_CONFIG_HOME")
                .map(std::path::PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| crate::paths::user_home().map(|h| h.join(".config")))?;
            Some(xdg.join("git").join("ignore"))
        })
    });
    FILE.clone()
}

/// Deepest frame with an opinion decides (nested ignore files override
/// their parents).
fn stack_match(
    frames: &[Frame],
    pick: impl Fn(&Frame) -> Option<&Gitignore>,
    abs: &Path,
    is_dir: bool,
) -> Match<()> {
    for f in frames.iter().rev() {
        if let Some(g) = pick(f) {
            match g.matched(abs, is_dir) {
                Match::None => {}
                Match::Ignore(_) => return Match::Ignore(()),
                Match::Whitelist(_) => return Match::Whitelist(()),
            }
        }
    }
    Match::None
}

/// Decision for one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Include,
    /// Included because `.blirpignore` re-includes it (a private key found
    /// by content is then still synced, flagged).
    Reincluded,
    /// Included only because `.blirpignore` re-includes a secret.
    IncludeSecret,
    Exclude(Reason),
}

impl Rules {
    pub fn new(root: &Path, git: bool) -> Rules {
        Rules {
            build: matcher(root, BUILD),
            secrets: matcher(root, SECRETS),
            git,
        }
    }

    fn secret(&self, abs: &Path, is_dir: bool) -> bool {
        // Also when a folder above it is a secret folder (`.ssh/`).
        self.secrets
            .matched_path_or_any_parents(abs, is_dir)
            .is_ignore()
    }

    fn build_listed(&self, wire: &str, abs: &Path, is_dir: bool, size: u64) -> bool {
        if self.build.matched(abs, is_dir).is_ignore() {
            return true;
        }
        if is_dir {
            return false;
        }
        let lower = wire.rsplit('/').next().unwrap_or(wire).to_ascii_lowercase();
        if lower.ends_with(".zip") && size > ZIP_LIMIT {
            return true;
        }
        if lower.ends_with(".exe") {
            let in_bin = wire
                .split('/')
                .rev()
                .skip(1)
                .any(|c| c.eq_ignore_ascii_case("bin"));
            return self.git || !in_bin;
        }
        false
    }

    /// Layers 2-5 for `wire` (at `abs`) with the ignore files of every
    /// folder from the root down to its parent in `frames`. `size` is 0 for
    /// folders.
    pub fn decide(
        &self,
        frames: &[Frame],
        wire: &str,
        abs: &Path,
        is_dir: bool,
        size: u64,
    ) -> Verdict {
        match stack_match(frames, |f| f.blirpignore.as_ref(), abs, is_dir) {
            Match::Ignore(()) => return Verdict::Exclude(Reason::Ignored),
            Match::Whitelist(()) => {
                return if self.secret(abs, is_dir) {
                    Verdict::IncludeSecret
                } else {
                    Verdict::Reincluded
                };
            }
            Match::None => {}
        }
        if self.secret(abs, is_dir) {
            return Verdict::Exclude(Reason::Secret);
        }
        match stack_match(frames, |f| f.gitignore.as_ref(), abs, is_dir) {
            Match::Ignore(()) => return Verdict::Exclude(Reason::Ignored),
            Match::Whitelist(()) => return Verdict::Include,
            Match::None => {}
        }
        if self.build_listed(wire, abs, is_dir, size) {
            return Verdict::Exclude(Reason::Ignored);
        }
        Verdict::Include
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(
        root: &Path,
        frames: &[Frame],
        git: bool,
        wire: &str,
        is_dir: bool,
        size: u64,
    ) -> Verdict {
        let abs = crate::files::path::to_local(root, wire);
        Rules::new(root, git).decide(frames, wire, &abs, is_dir, size)
    }

    #[test]
    fn constants_parse() {
        let root = Path::new("/r");
        for lines in [BUILD, SECRETS] {
            let mut b = GitignoreBuilder::new(root);
            for l in lines {
                assert!(b.add_line(None, l).is_ok(), "{l}");
            }
            assert!(b.build().is_ok());
        }
    }

    #[test]
    fn build_denylist() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let f: Vec<Frame> = vec![Frame::default()];
        use Verdict::*;
        for (wire, is_dir) in [
            ("node_modules", true),
            ("web/node_modules", true),
            ("target", true),
            ("a/__pycache__", true),
            ("x.pyc", false),
            ("sub/.DS_Store", false),
            ("lib.dll", false),
        ] {
            assert_eq!(
                check(r, &f, true, wire, is_dir, 1),
                Exclude(Reason::Ignored),
                "{wire}"
            );
        }
        assert_eq!(check(r, &f, true, "src", true, 0), Include);
        assert_eq!(check(r, &f, true, "src/main.rs", false, 1), Include);
        // *.exe: always in git roots; outside bin/ in plain folders.
        assert_eq!(
            check(r, &f, true, "bin/tool.exe", false, 1),
            Exclude(Reason::Ignored)
        );
        assert_eq!(check(r, &f, false, "bin/tool.exe", false, 1), Include);
        assert_eq!(check(r, &f, false, "x/bin/tool.EXE", false, 1), Include);
        assert_eq!(
            check(r, &f, false, "tool.exe", false, 1),
            Exclude(Reason::Ignored)
        );
        // *.zip only over 10 MB.
        assert_eq!(check(r, &f, true, "a.zip", false, ZIP_LIMIT), Include);
        assert_eq!(
            check(r, &f, true, "a.zip", false, ZIP_LIMIT + 1),
            Exclude(Reason::Ignored)
        );
    }

    #[test]
    fn secrets_and_templates() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let f: Vec<Frame> = vec![Frame::default()];
        use Verdict::*;
        for wire in [
            ".env",
            "api/.env.local",
            "certs/server.pem",
            "k.KEY",
            "id_rsa",
            "id_ed25519.pub",
            ".npmrc",
            "deploy/.docker/config.json",
            "credentials.json",
            "service-account-prod.json",
            "infra/terraform.tfstate.backup",
            ".aws/credentials",
            "home/.ssh/config",
        ] {
            assert_eq!(
                check(r, &f, true, wire, false, 1),
                Exclude(Reason::Secret),
                "{wire}"
            );
        }
        assert_eq!(check(r, &f, true, ".ssh", true, 0), Exclude(Reason::Secret));
        for wire in [
            ".env.example",
            ".env.sample",
            "x/.env.template",
            "config.json",
            "keys.md",
        ] {
            assert_eq!(check(r, &f, true, wire, false, 1), Include, "{wire}");
        }
    }

    #[test]
    fn gitignore_and_blirpignore_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::write(
            r.join(".gitignore"),
            "*.log\n!keep.log\n!dist/\n.env\n!.env\n",
        )
        .unwrap();
        std::fs::write(r.join(BLIRPIGNORE), "big/\n!node_modules/\n!.env.local\n").unwrap();
        std::fs::create_dir(r.join("sub")).unwrap();
        std::fs::write(r.join("sub/.gitignore"), "!nested.log\n").unwrap();
        let root_frames = vec![Frame::git_exclude(r), Frame::load(r)];
        let sub_frames = vec![
            Frame::git_exclude(r),
            Frame::load(r),
            Frame::load(&r.join("sub")),
        ];
        use Verdict::*;
        // Layer 3.
        assert_eq!(
            check(r, &root_frames, true, "a.log", false, 1),
            Exclude(Reason::Ignored)
        );
        assert_eq!(check(r, &root_frames, true, "keep.log", false, 1), Include);
        // A nested .gitignore overrides its parent.
        assert_eq!(
            check(r, &sub_frames, true, "sub/nested.log", false, 1),
            Include
        );
        assert_eq!(
            check(r, &sub_frames, true, "sub/other.log", false, 1),
            Exclude(Reason::Ignored)
        );
        // Layer 3 re-includes from layer 2.
        assert_eq!(check(r, &root_frames, true, "dist", true, 0), Include);
        // Layer 4 beats a .gitignore re-include.
        assert_eq!(
            check(r, &root_frames, true, ".env", false, 1),
            Exclude(Reason::Secret)
        );
        // Layer 5 ignores, and re-includes from 2 and 4 (a secret is flagged).
        assert_eq!(
            check(r, &root_frames, true, "big", true, 0),
            Exclude(Reason::Ignored)
        );
        assert_eq!(
            check(r, &root_frames, true, "node_modules", true, 0),
            Reincluded
        );
        assert_eq!(
            check(r, &root_frames, true, ".env.local", false, 1),
            IncludeSecret
        );
    }

    #[test]
    fn git_info_exclude_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join(".git/info")).unwrap();
        std::fs::write(r.join(".git/info/exclude"), "local-only/\n").unwrap();
        let frames = vec![Frame::git_exclude(r), Frame::load(r)];
        assert_eq!(
            check(r, &frames, true, "local-only", true, 0),
            Verdict::Exclude(Reason::Ignored)
        );
    }

    #[test]
    fn private_keys_by_content() {
        assert!(looks_like_private_key(
            b"x\n-----BEGIN OPENSSH PRIVATE KEY-----\nabc"
        ));
        assert!(looks_like_private_key(b"-----BEGIN PRIVATE KEY-----"));
        assert!(!looks_like_private_key(b"-----BEGIN PUBLIC KEY-----"));
        assert!(!looks_like_private_key(b"plain text"));
        let mut far = vec![b' '; KEY_CHECK_HEAD];
        far.extend_from_slice(b"-----BEGIN RSA PRIVATE KEY-----");
        assert!(!looks_like_private_key(&far), "only the head is read");
    }
}
