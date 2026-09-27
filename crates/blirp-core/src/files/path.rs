//! Wire paths: `/`-separated paths relative to a root. Every path that
//! arrives from the network passes [`check`] before anything is stored or
//! written; writers apply [`valid_here`] for the local OS on top.

use std::path::{Component, Path, PathBuf};

/// Longest wire path accepted.
pub const MAX_PATH: usize = 4096;
/// Prefix of the temp files writers create next to their target.
pub const TMP_PREFIX: &str = ".blirp-tmp-";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path is empty or too long")]
    Length,
    #[error("path must be relative, without '.', '..' or empty parts")]
    Shape,
    #[error("path contains a control character or backslash")]
    Chars,
    #[error("path is inside a version control folder")]
    Vcs,
    #[error("path names a blirp temp file")]
    Temp,
}

/// Version control folders: never synced, never written (`.git/hooks`
/// would run code on the receiver).
const VCS: &[&str] = &[".git", ".hg", ".svn", ".jj"];

/// What Windows and macOS compare: trailing dots and spaces are dropped by
/// Windows, case is folded by both.
fn fold(component: &str) -> String {
    component.trim_end_matches(['.', ' ']).to_ascii_lowercase()
}

/// Whether one path component names a version control folder on any OS,
/// including the spellings Windows maps onto `.git` (`.GIT.`, `.git ` and
/// the 8.3 short name `GIT~1`) and NTFS streams (`.git::$INDEX_ALLOCATION`).
pub fn is_vcs_component(component: &str) -> bool {
    let base = component.split(':').next().unwrap_or(component);
    let f = fold(base);
    if VCS.contains(&f.as_str()) {
        return true;
    }
    // 8.3 short names: `GIT~1`, `HG~1`, `SVN~1`, `JJ~1`, or the hashed
    // form Windows uses once those are taken (`GI1A2B~1`).
    let Some((prefix, n)) = f.split_once('~') else {
        return false;
    };
    if prefix.is_empty() || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    VCS.iter().map(|v| &v[1..]).any(|v| {
        v.starts_with(prefix)
            || (prefix.len() == 6
                && v.starts_with(&prefix[..2])
                && prefix[2..].bytes().all(|b| b.is_ascii_hexdigit()))
    })
}

/// Validate a wire path from anywhere: 1..=4096 bytes, `/`-separated
/// normal components (no empty, `.` or `..`), no control characters or
/// backslashes, nothing inside a VCS folder and no blirp temp file.
pub fn check(p: &str) -> Result<(), PathError> {
    if p.is_empty() || p.len() > MAX_PATH {
        return Err(PathError::Length);
    }
    for c in p.split('/') {
        if c.is_empty() || c == "." || c == ".." {
            return Err(PathError::Shape);
        }
        if c.chars().any(|ch| ch.is_control() || ch == '\\') {
            return Err(PathError::Chars);
        }
        if is_vcs_component(c) {
            return Err(PathError::Vcs);
        }
        if c.starts_with(TMP_PREFIX) {
            return Err(PathError::Temp);
        }
    }
    Ok(())
}

/// Names Windows cannot create: reserved device names (with any
/// extension), trailing dot or space, `<>:"|?*`.
pub fn windows_invalid(component: &str) -> bool {
    if component.ends_with(['.', ' ']) {
        return true;
    }
    if component
        .chars()
        .any(|c| "<>:\"|?*".contains(c) || (c as u32) < 32)
    {
        return true;
    }
    let stem = component.split('.').next().unwrap_or(component).trim_end();
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0')
}

/// Whether this OS can hold `p` (a checked wire path) under its name.
pub fn valid_here(p: &str) -> bool {
    if cfg!(windows) {
        !p.split('/').any(windows_invalid)
    } else {
        true
    }
}

/// Wire form of a relative filesystem path (components joined with `/`);
/// `None` when a component is not UTF-8 or not a normal name.
pub fn to_wire(rel: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?.to_string()),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Filesystem path of a checked wire path under `root`.
pub fn to_local(root: &Path, wire: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for c in wire.split('/') {
        out.push(c);
    }
    out
}

/// Case-folded key of a wire path, for finding names that collide on
/// case-insensitive filesystems (Windows, default macOS).
pub fn case_key(p: &str) -> String {
    p.to_lowercase()
}

/// Whether this OS's default filesystems compare names case-insensitively.
pub fn case_insensitive_fs() -> bool {
    cfg!(any(windows, target_os = "macos"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_paths_are_checked() {
        for ok in [
            "a",
            "src/main.rs",
            "a b/c",
            "MYDOCU~1/x",
            "a~b",
            "dir/.gitignore",
            "x/.github/w.yml",
            "aux.c",
        ] {
            assert_eq!(check(ok), Ok(()), "{ok}");
        }
        for (bad, e) in [
            ("", PathError::Length),
            ("/etc/passwd", PathError::Shape),
            ("../x", PathError::Shape),
            ("a/../../x", PathError::Shape),
            ("a//b", PathError::Shape),
            ("./a", PathError::Shape),
            ("a/", PathError::Shape),
            ("a\\..\\b", PathError::Chars),
            ("a\0b", PathError::Chars),
            (".git/hooks/post-checkout", PathError::Vcs),
            ("sub/.git/config", PathError::Vcs),
            (".GIT/config", PathError::Vcs),
            (".git./config", PathError::Vcs),
            (".git /config", PathError::Vcs),
            ("GIT~1/config", PathError::Vcs),
            ("HG~1/hgrc", PathError::Vcs),
            ("svn~2/x", PathError::Vcs),
            ("JJ~1/repo", PathError::Vcs),
            ("GI7F3A~1/config", PathError::Vcs),
            (".git::$INDEX_ALLOCATION/config", PathError::Vcs),
            (".hg/hgrc", PathError::Vcs),
            (".svn/x", PathError::Vcs),
            (".jj/repo", PathError::Vcs),
            ("a/.blirp-tmp-123", PathError::Temp),
        ] {
            assert_eq!(check(bad), Err(e), "{bad:?}");
        }
        assert_eq!(check(&"a".repeat(MAX_PATH + 1)), Err(PathError::Length));
    }

    #[test]
    fn windows_names() {
        for bad in [
            "CON", "con.txt", "aux.c", "COM1", "lpt9.log", "a.", "a ", "a:b", "a?", "x|y",
        ] {
            assert!(windows_invalid(bad), "{bad}");
        }
        for ok in ["console", "COM0", "com10", "auxiliary.c", ".env", "a.b"] {
            assert!(!windows_invalid(ok), "{ok}");
        }
    }

    #[test]
    fn wire_and_local_forms() {
        let rel = Path::new("src").join("lib.rs");
        assert_eq!(to_wire(&rel).as_deref(), Some("src/lib.rs"));
        assert_eq!(to_wire(Path::new("../x")), None);
        let root = Path::new("root");
        assert_eq!(to_local(root, "a/b"), root.join("a").join("b"));
    }
}
