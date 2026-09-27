//! Project file sync through the hub (docs/design/project-file-sync.md,
//! docs/project-files.md): the pieces every machine shares. Path rules for
//! paths that arrive from the network ([`path`]), the exclusion layers and
//! the scanner of a working copy ([`rules`], [`scan`]), safe writes into a
//! working copy ([`write`]) and disk facts ([`disk`]).
//!
//! A root is one project folder on its origin machine; every working copy
//! of it (the origin folder, the hub copy for cloud sessions, copies on
//! other machines) writes to the same root on the hub. Paths inside a root
//! travel as `/`-separated relative strings ("wire paths").

pub mod disk;
pub mod path;
pub mod rules;
pub mod scan;
mod types;
pub mod write;

pub use types::{
    ChangeOp, ChangeResult, EntryContent, FileChange, FilesMode, IndexEntry, RootInfo,
};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Largest file content hash accepted from anywhere: 32-byte BLAKE3, hex.
pub const HASH_LEN: usize = 64;

/// Id of the root for `path` on `machine_id`: the first 32 hex chars of
/// BLAKE3(`<machine_id>\n<path>`). Deterministic, so no replicated row is
/// needed to agree on it.
pub fn root_id(machine_id: &str, path: &str) -> String {
    let h = blake3::hash(format!("{machine_id}\n{path}").as_bytes());
    h.to_hex()[..32].to_string()
}

/// A root id as [`root_id`] makes them.
pub fn is_root_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A content hash as the file engine makes them (lowercase BLAKE3 hex).
pub fn is_hash(s: &str) -> bool {
    s.len() == HASH_LEN && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Hex BLAKE3 of `bytes`.
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Git facts of a root, read with plain git commands on its origin and
/// sent with every commit batch. A copy elsewhere clones `remote` and
/// checks out `head_sha` (else `branch`) before the hub's files are written
/// on top.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GitManifest {
    /// Remote URL without credentials.
    pub remote: Option<String>,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub upstream_sha: Option<String>,
}

impl GitManifest {
    /// Refuse values that could be read as git options or carry anything
    /// but what their names say (they come from another machine).
    pub fn is_valid(&self) -> bool {
        let sha = |s: &Option<String>| {
            s.as_deref().is_none_or(|s| {
                (4..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
            })
        };
        let branch_ok = self.branch.as_deref().is_none_or(|b| {
            (1..=250).contains(&b.len())
                && !b.starts_with('-')
                && !b.contains("..")
                && b.chars()
                    .all(|c| !c.is_control() && !c.is_whitespace() && !"~^:?*[\\".contains(c))
        });
        let remote_ok = self.remote.as_deref().is_none_or(|r| {
            (1..=2000).contains(&r.len())
                && !r.starts_with('-')
                && r.chars().all(|c| !c.is_control() && !c.is_whitespace())
        });
        sha(&self.head_sha) && sha(&self.upstream_sha) && branch_ok && remote_ok
    }
}

/// `name.conflict-<machine>-<YYYYMMDD-HHMMSS>.ext` next to `path`: the name
/// a losing concurrent edit is kept under. `machine` is reduced to
/// `[A-Za-z0-9-]`; `n > 1` adds `-n` when that name is taken.
pub fn conflict_path(path: &str, machine: &str, at_ms: i64, n: u32) -> String {
    let (dir, name) = match path.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, path),
    };
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut who: String = machine
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    who.truncate(40);
    if who.is_empty() {
        who.push_str("machine");
    }
    let stamp = format_stamp(at_ms);
    let suffix = if n > 1 {
        format!("-{n}")
    } else {
        String::new()
    };
    let file = format!("{stem}.conflict-{who}-{stamp}{suffix}{ext}");
    match dir {
        Some(d) => format!("{d}/{file}"),
        None => file,
    }
}

/// Whether a wire path is a conflict copy made by [`conflict_path`].
pub fn is_conflict_copy(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.contains(".conflict-")
}

/// The conflict copies of `path` among `names` (same folder, same stem and
/// extension).
pub fn conflict_prefix(path: &str) -> (String, String) {
    let (dir, name) = match path.rsplit_once('/') {
        Some((d, n)) => (format!("{d}/"), n),
        None => (String::new(), path),
    };
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (format!("{dir}{stem}.conflict-"), ext.to_string())
}

/// UTC `YYYYMMDD-HHMMSS` of unix milliseconds.
fn format_stamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to (year, month, day) in the proleptic Gregorian
/// calendar (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_ids_are_stable_and_distinct() {
        let a = root_id("m1", "/home/u/p");
        assert_eq!(a, root_id("m1", "/home/u/p"));
        assert_ne!(a, root_id("m2", "/home/u/p"));
        assert!(is_root_id(&a));
        assert!(!is_root_id("../x"));
        assert!(is_hash(&hash_bytes(b"x")));
        assert!(!is_hash("ABC"));
    }

    #[test]
    fn conflict_names() {
        // 2026-01-02 03:04:05 UTC
        let t = 1_767_323_045_000;
        assert_eq!(
            conflict_path("src/main.rs", "Lap Top!", t, 1),
            "src/main.conflict-Lap-Top--20260102-030405.rs"
        );
        assert_eq!(
            conflict_path("Makefile", "hub", t, 2),
            "Makefile.conflict-hub-20260102-030405-2"
        );
        assert_eq!(
            conflict_path("a.tar.gz", "m", t, 1),
            "a.tar.conflict-m-20260102-030405.gz"
        );
        assert!(is_conflict_copy("x/a.conflict-m-20260102-030405.gz"));
        assert!(!is_conflict_copy("x/a.gz"));
        let (pre, ext) = conflict_prefix("x/a.gz");
        assert_eq!((pre.as_str(), ext.as_str()), ("x/a.conflict-", ".gz"));
        assert_eq!(format_stamp(0), "19700101-000000");
    }

    #[test]
    fn manifests_from_the_network_are_checked() {
        let ok = GitManifest {
            remote: Some("https://example.com/o/r.git".into()),
            branch: Some("feature/x".into()),
            head_sha: Some("0123abcd".into()),
            upstream_sha: None,
        };
        assert!(ok.is_valid());
        for bad in [
            GitManifest {
                branch: Some("--upload-pack=x".into()),
                ..ok.clone()
            },
            GitManifest {
                head_sha: Some("zz".into()),
                ..ok.clone()
            },
            GitManifest {
                remote: Some("-c x".into()),
                ..ok.clone()
            },
            GitManifest {
                branch: Some("a..b".into()),
                ..ok.clone()
            },
        ] {
            assert!(!bad.is_valid(), "{bad:?}");
        }
    }
}
