//! Types shared by the hub store, the `blirp/files/1` protocol and the API.

use super::GitManifest;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Per-project file sync setting, stored on the hub (`file_projects`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum FilesMode {
    /// Follow each origin machine's `[sync] project_files`.
    #[default]
    Default,
    On,
    /// Uploads stop; the hub copy stays readable ("paused").
    Off,
}

impl FilesMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// Whether uploads run for a root of a project in this mode on a
    /// machine whose `[sync] project_files` is `global`. The machine's
    /// switch is a hard opt-out: no project mode makes it upload.
    pub fn effective(self, global: bool) -> bool {
        global && self != Self::Off
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_switch_wins() {
        for mode in [FilesMode::Default, FilesMode::On, FilesMode::Off] {
            assert!(
                !mode.effective(false),
                "{mode:?} uploads from an opted-out machine"
            );
        }
        assert!(FilesMode::Default.effective(true));
        assert!(FilesMode::On.effective(true));
        assert!(!FilesMode::Off.effective(true));
    }
}

/// What a path of a root holds on the hub.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryContent {
    Blob { hash: String },
    Link { target: String },
}

/// One path of a root on the hub (`file_entries`). `content: None` is a
/// tombstone: the path was deleted at `version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub path: String,
    pub version: i64,
    pub content: Option<EntryContent>,
    pub size: i64,
    pub mode_x: bool,
    /// File mtime on the writer (unix ms).
    pub mtime: i64,
    /// Machine that wrote this version.
    pub by_machine: String,
    /// When the hub accepted it (unix ms).
    pub at: i64,
}

/// One change of a commit: accepted only when the path's current version
/// equals `base_version` (0: the writer has never seen the path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub base_version: i64,
    pub op: ChangeOp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ChangeOp {
    Put {
        hash: String,
        size: i64,
        mode_x: bool,
        mtime: i64,
    },
    Link {
        target: String,
    },
    /// The writer deleted the file (a tombstone is kept).
    Delete,
    /// The file is excluded now on the writer: drop it without a tombstone,
    /// so other copies keep theirs.
    Forget,
}

/// Outcome of one change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ChangeResult {
    Ok {
        version: i64,
    },
    /// Another writer got there first. `current` is the winning entry
    /// (None when the path is gone); a losing `put` or `link` was stored at
    /// `copy_path` as version `copy_version`.
    Conflict {
        current: Option<IndexEntry>,
        copy_path: Option<String>,
        copy_version: Option<i64>,
    },
    /// Never applied (invalid path, missing blob); not retried as is.
    Rejected {
        code: String,
        message: String,
    },
}

/// A root on the hub with its totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RootInfo {
    pub root_id: String,
    pub project_id: String,
    /// Origin machine and folder.
    pub machine_id: String,
    pub machine_name: String,
    /// The origin machine was revoked: nothing uploads to this root anymore.
    pub origin_revoked: bool,
    pub path: String,
    /// Newest version (the root's sequence).
    pub head: i64,
    pub files: i64,
    pub bytes: i64,
    /// Live conflict copies (`*.conflict-*`).
    pub conflicts: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub manifest: Option<GitManifest>,
}
