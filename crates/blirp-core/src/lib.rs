//! blirp core: model types, config, paths, the SQLite store with migrations,
//! git helpers and secret redaction. See `docs/ARCHITECTURE.md`.

pub mod config;
pub mod git;
pub mod model;
pub mod paths;
pub mod proc_tree;
pub mod process;
pub mod redact;
pub mod store;

use std::time::{SystemTime, UNIX_EPOCH};

/// New UUIDv7 id string (time-ordered, §5).
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Whether `id` has the shape of a blirp id: 1-128 chars of `[A-Za-z0-9_-]`.
/// UUIDs and iroh endpoint ids pass. Ids name files (`launch/<session>/`)
/// and arrive from other machines, so anything that could be a path (`..`,
/// separators, a drive prefix) is refused.
pub fn is_safe_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Current unix time in milliseconds.
pub fn now_ms() -> i64 {
    // A clock before 1970 is a broken host; clamp to 0 rather than panic.
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Fill `N` bytes from the OS CSPRNG and hex-encode them.
pub fn random_hex<const N: usize>() -> std::io::Result<String> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(hex::encode(buf))
}
