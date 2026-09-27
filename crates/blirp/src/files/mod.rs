//! Project file sync through the hub (docs/project-files.md): the daemon's
//! file engine and its API.

pub mod api;
pub mod local;

/// File sync state of the daemon.
pub struct FilesState {
    /// Hashing runs one folder at a time (design §3), so a large first scan
    /// never competes with itself for the disk.
    pub hash_gate: tokio::sync::Semaphore,
}

impl Default for FilesState {
    fn default() -> Self {
        Self {
            hash_gate: tokio::sync::Semaphore::new(1),
        }
    }
}
