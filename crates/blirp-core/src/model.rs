//! API and storage DTOs. These shapes are the HTTP/WS contract consumed by the
//! web UI; TypeScript mirrors live in `web/src/lib/api/types.gen.ts`
//! (regenerate with `cargo test -p blirp-core export_bindings`).
//!
//! Conventions: ids are UUIDv7 strings, timestamps are unix milliseconds
//! (`number` in TS), enums serialize as lowercase/snake_case strings, optional
//! values are always present as `null`.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use ts_rs::TS;

use crate::config::{Config, Summarizer};

/// Built-in agent ids (§7). Custom agents are `custom:<name>`.
pub const BUILTIN_AGENTS: &[&str] = &[
    "claude", "codex", "opencode", "pi", "gemini", "cursor", "amp", "aider", "dsh", "shell",
];

macro_rules! str_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident = $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
        pub enum $name {
            $(#[serde(rename = $s)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.pad(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($s => Ok($name::$variant),)+
                    _ => Err(format!("invalid {} {:?}", stringify!($name), s)),
                }
            }
        }

        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(self.as_str().into())
            }
        }

        impl rusqlite::types::FromSql for $name {
            fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                v.as_str()?.parse().map_err(|e: String| rusqlite::types::FromSqlError::Other(e.into()))
            }
        }
    };
}

str_enum!(MachineRole {
    Standalone = "standalone",
    Node = "node",
    Hub = "hub",
});

// `str_enum!` cannot carry `#[default]` on a variant.
#[allow(clippy::derivable_impls)]
impl Default for MachineRole {
    fn default() -> Self {
        Self::Standalone
    }
}

str_enum!(SessionStatus {
    Starting = "starting",
    Working = "working",
    Idle = "idle",
    Waiting = "waiting",
    Completed = "completed",
    Failed = "failed",
    Detached = "detached",
});

impl SessionStatus {
    /// True while a process may still be attached to the session.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Working | Self::Idle | Self::Waiting
        )
    }
}

str_enum!(SessionOrigin {
    Blirp = "blirp",
    External = "external",
});

str_enum!(EventKind {
    User = "user",
    Assistant = "assistant",
    ToolCall = "tool_call",
    ToolResult = "tool_result",
    System = "system",
    FileEdit = "file_edit",
    Summary = "summary",
});

str_enum!(RecordKind {
    Decision = "decision",
    Plan = "plan",
    Note = "note",
    OpenThread = "open_thread",
    Gotcha = "gotcha",
});

str_enum!(RecordStatus {
    Active = "active",
    Resolved = "resolved",
    Archived = "archived",
});

str_enum!(SuggestionTarget {
    Brief = "brief",
    Record = "record",
    Wiki = "wiki",
});

str_enum!(SuggestionStatus {
    Pending = "pending",
    Accepted = "accepted",
    Rejected = "rejected",
    Dismissed = "dismissed",
});

str_enum!(ResourceKind {
    Link = "link",
    Repo = "repo",
    Pr = "pr",
    Issue = "issue",
    Doc = "doc",
    File = "file",
});

str_enum!(DeviceKind {
    Machine = "machine",
    Browser = "browser",
});

str_enum!(FileKind {
    File = "file",
    Dir = "dir",
    Symlink = "symlink",
    Other = "other",
});

str_enum!(SearchHitKind {
    Event = "event",
    Record = "record",
});

str_enum!(IntegrationState {
    Installed = "installed",
    NotInstalled = "not_installed",
    Unsupported = "unsupported",
});

// `hook`: a SessionStart-style hook returns the memory as additional context;
// `instructions`: the memory is added to the agent's instructions via a config
// override; `flag`: a command-line flag passes the memory file (aider `--read`);
// `none`: only the `BLIRP_MEMORY_FILE` env var is set.
str_enum!(InjectMode {
    Hook = "hook",
    Instructions = "instructions",
    Flag = "flag",
    None = "none",
});

// ---------------------------------------------------------------- rows

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Machine {
    pub id: String,
    pub name: String,
    pub os: String,
    pub role: MachineRole,
    pub last_seen: i64,
    pub revoked: bool,
}

/// `GET /api/machines`: the replicated row plus runtime presence. `online`:
/// the machine has a live sync connection to the hub (as the hub reports
/// it), always true for this machine, null when unknown (standalone, not
/// connected to the hub, or a hub that does not report presence). With
/// presence known, `last_seen` is its connect, disconnect or latest minute
/// online (never replicated).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct MachineInfo {
    #[serde(flatten)]
    pub machine: Machine,
    pub online: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted: bool,
    /// A machine's Chats bucket (§5, id `chats-<machine id>`): sessions that
    /// belong to no project. Not listed among projects; its memory is never
    /// injected. Absent in rows from blirp 0.1.0.
    #[serde(default)]
    pub chats: bool,
    /// Set on a deleted project that was merged into another one.
    #[serde(default)]
    pub merged_into: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ProjectPath {
    pub project_id: String,
    pub machine_id: String,
    pub path: String,
    /// Normalized `host/owner/repo`, null when the folder is not a git repo or has no remote.
    pub git_remote: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Session {
    pub id: String,
    pub project_id: String,
    pub machine_id: String,
    /// `claude|codex|opencode|pi|gemini|cursor|amp|aider|dsh|shell|custom:<name>`
    pub agent: String,
    /// The agent's own session id (claude uuid, codex rollout uuid, ...).
    pub agent_session_id: Option<String>,
    pub origin: SessionOrigin,
    pub cwd: String,
    pub title: Option<String>,
    pub status: SessionStatus,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub transcript_path: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub last_activity_at: i64,
    pub exit_code: Option<i32>,
    /// Distill output (§9), null until the session is distilled.
    pub summary: Option<JsonValue>,
    pub distilled_through_seq: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost_usd: f64,
    pub parent_session_id: Option<String>,
    /// Ended by a user Stop: `status` is `completed` and `exit_code` null.
    // Default: rows replicated from older versions do not carry it.
    #[serde(default)]
    pub stopped_by_user: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Event {
    pub session_id: String,
    pub seq: i64,
    pub ts: i64,
    pub kind: EventKind,
    /// Redacted, human-readable text.
    pub text: String,
    pub meta: Option<JsonValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Record {
    pub id: String,
    pub project_id: String,
    pub kind: RecordKind,
    pub title: String,
    pub body: String,
    pub status: RecordStatus,
    pub pinned: bool,
    pub source_session_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// `user`, `distiller` or a machine id.
    pub updated_by: String,
}

/// A project brief; also used for entries of the brief history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Brief {
    /// Id of this version's `brief_history` row: a UUIDv7, unique across
    /// machines, so versions written concurrently on two machines never
    /// overwrite each other. Empty in changes from older blirp versions.
    #[serde(default)]
    pub id: String,
    pub project_id: String,
    pub body_md: String,
    /// Position in the project's history (1 = oldest, by time), derived
    /// when read; for display and revert only.
    pub version: i64,
    pub updated_at: i64,
    pub updated_by: String,
    /// Machine that wrote this version (empty when unknown).
    #[serde(default)]
    pub machine_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct WikiPage {
    pub id: String,
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub body_md: String,
    pub updated_at: i64,
    pub updated_by: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Suggestion {
    pub id: String,
    pub project_id: String,
    pub target: SuggestionTarget,
    /// Record id or wiki slug being changed; null for the brief or for new items.
    pub target_id: Option<String>,
    /// `BriefProposal`, `RecordProposal` or `WikiProposal` depending on `target`.
    pub proposal: JsonValue,
    pub rationale: String,
    pub source_session_id: Option<String>,
    pub status: SuggestionStatus,
    pub created_at: i64,
    pub decided_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct BriefProposal {
    pub body_md: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RecordProposal {
    pub kind: RecordKind,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub status: Option<RecordStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct WikiProposal {
    pub slug: String,
    pub title: String,
    pub body_md: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Resource {
    pub id: String,
    pub project_id: String,
    pub kind: ResourceKind,
    pub url: String,
    pub title: String,
    pub meta: Option<JsonValue>,
    pub created_at: i64,
    /// Last change; replicated copies converge on the newest (§10). Changes
    /// queued by versions without it (before migration 8) read as 0.
    #[serde(default)]
    pub updated_at: i64,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    /// SHA-256 of the device cookie token; never serialized.
    #[serde(skip)]
    #[ts(skip)]
    pub token_hash: Option<String>,
    pub node_id: Option<String>,
    pub created_at: i64,
    pub last_seen: i64,
    pub revoked: bool,
    pub can_control_terminals: bool,
}

// ---------------------------------------------------------------- API

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ErrorDetail {
    /// Stable machine-readable code, e.g. `not_found`, `invalid_request`, `not_git`.
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct Health {
    pub version: String,
    pub machine: Machine,
    pub role: MachineRole,
    /// What the calling client may do here.
    // Default: older daemons do not send it.
    #[serde(default)]
    pub capabilities: Capabilities,
    /// This machine is holding a sleep-prevention assertion because sessions
    /// are live (`sessions.keep_awake`).
    #[serde(default)]
    pub keep_awake: bool,
}

/// `GET /api/update`: whether a newer release than the daemon exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct UpdateStatus {
    /// This daemon's version.
    pub current: String,
    /// Newest published release; null when checks are off or failed.
    pub latest: Option<String>,
    pub available: bool,
    /// Release notes of `latest`.
    pub notes_url: Option<String>,
    /// `[update] check` in config.toml.
    pub enabled: bool,
    /// `blirp update` can replace this daemon's binary: the install script
    /// installed it (its receipt matches), or it is the sidecar of the
    /// desktop app the script installed. False for installers, package
    /// managers and source builds, which update the way they were installed.
    pub self_update: bool,
    /// When GitHub was last asked (unix ms); null before the first check.
    pub checked_at: Option<i64>,
    /// Why that check failed (offline, rate limited, ...); null when it worked.
    pub error: Option<String>,
    /// The last `blirp update` that tried to install a release on this
    /// machine (from the Update now button or a terminal).
    pub last_update: Option<UpdateOutcome>,
}

/// One run of `blirp update` (not `--check`), a line of `logs/update.log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct UpdateOutcome {
    /// The version that ran the update.
    pub from: String,
    /// The release it aimed for; null when it failed before finding one.
    pub to: Option<String>,
    /// The files were replaced: `to` is installed now.
    pub installed: bool,
    /// Everything worked, including "nothing to install".
    pub ok: bool,
    /// Why it failed: before `installed`, `from` is still installed;
    /// after it, the daemon did not start again.
    pub error: Option<String>,
    /// Unix ms.
    pub finished_at: i64,
}

/// Rights of the calling client (§11), so a UI can hide what the daemon
/// would refuse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Capabilities {
    /// Configuration, sync, devices, agent integration, invites, open,
    /// shutdown: local clients (runtime token) only.
    pub admin: bool,
    /// Launch/resume/stop sessions, type into terminals, change memory:
    /// local clients, browser devices allowed to control terminals, and
    /// requests relayed from a machine allowed to.
    pub control_terminals: bool,
    /// The client authenticated with this machine's runtime token.
    pub local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ProjectPathInfo {
    pub machine_id: String,
    pub path: String,
    pub git_remote: Option<String>,
    /// Folder is inside a git work tree (only known for this machine's paths).
    pub is_git: bool,
    /// Path belongs to the machine serving this response.
    pub local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ProjectSummary {
    #[serde(flatten)]
    #[ts(flatten)]
    pub project: Project,
    pub paths: Vec<ProjectPathInfo>,
    pub is_git: bool,
    /// This machine's Chats bucket (sessions that belong to no project).
    pub is_home: bool,
    /// Project without folders: this machine's blirp workspace
    /// (`BLIRP_HOME/workspaces/<id>`), where its sessions start. Set when the
    /// project has no folder on any machine; the folder may not exist yet.
    pub workspace: Option<String>,
    pub session_count: i64,
    pub live_session_count: i64,
    pub last_activity_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CreateProject {
    /// Folder to register (absolute, on this machine). Without it the
    /// project has no folder and `name` is required.
    #[serde(default)]
    #[ts(optional)]
    pub path: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub name: Option<String>,
    /// Initial project brief (markdown).
    #[serde(default)]
    #[ts(optional)]
    pub brief: Option<String>,
}

/// `POST /api/projects/:id/folders/remove`: unregister one of this
/// machine's folders. The project stays, also without any folder.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RemoveProjectFolder {
    pub path: String,
}

/// `POST /api/sessions/:id/move`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MoveSession {
    /// Target project; null moves the session to Chats.
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PatchProject {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MergeProject {
    /// Project that receives this project's paths, sessions and memory.
    pub into: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ProjectMemory {
    pub brief: Option<Brief>,
    /// Active records, pinned first then most recently updated.
    pub records: Vec<Record>,
    pub recent_sessions: Vec<Session>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PutBrief {
    pub body_md: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RevertBrief {
    /// History entry to restore, by its `version` number...
    #[ts(optional)]
    pub version: Option<i64>,
    /// ...or by its id (preferred: numbers can shift when versions written
    /// earlier on another machine arrive).
    #[ts(optional)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CreateRecord {
    pub kind: RecordKind,
    pub title: String,
    pub body: String,
    #[serde(default)]
    #[ts(optional)]
    pub status: Option<RecordStatus>,
    #[serde(default)]
    #[ts(optional)]
    pub pinned: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PatchRecord {
    #[serde(default)]
    #[ts(optional)]
    pub kind: Option<RecordKind>,
    #[serde(default)]
    #[ts(optional)]
    pub title: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub body: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub status: Option<RecordStatus>,
    #[serde(default)]
    #[ts(optional)]
    pub pinned: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CreateWikiPage {
    /// `[a-z0-9-]`, 1-100 chars, unique per project.
    pub slug: String,
    pub title: String,
    pub body_md: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PutWikiPage {
    pub title: String,
    pub body_md: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CreateResource {
    pub kind: ResourceKind,
    pub url: String,
    pub title: String,
    #[serde(default)]
    #[ts(optional)]
    pub meta: Option<JsonValue>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PatchResource {
    #[serde(default)]
    #[ts(optional)]
    pub kind: Option<ResourceKind>,
    #[serde(default)]
    #[ts(optional)]
    pub url: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub title: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub meta: Option<JsonValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct GitStatusEntry {
    /// Path relative to the repo root, `/`-separated.
    pub path: String,
    /// Source path of a rename or copy.
    pub orig_path: Option<String>,
    /// Porcelain v2 index (staged) status letter, `.` when unchanged, `?` untracked.
    pub index: String,
    /// Porcelain v2 work tree status letter, `.` when unchanged, `?` untracked.
    pub worktree: String,
    pub conflicted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct GitStatus {
    pub is_git: bool,
    pub root: String,
    /// Branch name, null when HEAD is detached.
    pub branch: Option<String>,
    /// HEAD commit id, null before the first commit.
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: i64,
    pub behind: i64,
    pub entries: Vec<GitStatusEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct GitDiff {
    pub path: Option<String>,
    /// Unified diff against HEAD (staged + unstaged).
    pub diff: String,
    /// Diff exceeded 1 MiB and was cut.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct FileEntry {
    pub name: String,
    /// Path relative to the listing root, `/`-separated.
    pub path: String,
    pub kind: FileKind,
    pub size: i64,
    pub modified_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct DirListing {
    /// Absolute project root the listing is relative to.
    pub root: String,
    pub path: String,
    /// Directories first, then files, each sorted by name.
    pub entries: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct FileContent {
    pub root: String,
    pub path: String,
    pub size: i64,
    /// UTF-8 text; binary files and files over 1 MiB are rejected.
    pub content: String,
}

/// `GET /api/machines/:id/dirs`: folders on a machine, for picking where a
/// session runs. Only directories are listed, never file names or contents,
/// and only inside that machine's user home.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct MachineDirs {
    pub machine_id: String,
    /// Absolute home folder the listing is confined to.
    pub home: String,
    /// Absolute path of the listed folder.
    pub path: String,
    /// Absolute parent folder, null at the home folder.
    pub parent: Option<String>,
    pub entries: Vec<MachineDir>,
    /// More folders exist than were returned.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct MachineDir {
    pub name: String,
    /// Absolute path on that machine.
    pub path: String,
    /// Contains a `.git` entry.
    pub is_git: bool,
}

/// `POST /api/machines/:id/clone`: clone a git repository on a machine.
/// Give `url`, or `project_id` to use the remote of that project's git folder
/// on the machine receiving the request (credentials in it are removed).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CloneRepo {
    #[serde(default)]
    #[ts(optional)]
    pub url: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub project_id: Option<String>,
    /// Absolute folder inside the target's home to clone into; default `~/blirp`.
    #[serde(default)]
    #[ts(optional)]
    pub parent: Option<String>,
    /// Folder name; default the repository name.
    #[serde(default)]
    #[ts(optional)]
    pub name: Option<String>,
}

str_enum!(CloneState {
    Running = "running",
    Done = "done",
    Failed = "failed",
});

/// A clone started by `POST /api/machines/:id/clone`; poll
/// `GET /api/machines/:id/clone/:job` until it is no longer `running`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct CloneJob {
    pub id: String,
    pub machine_id: String,
    /// The URL cloned, without credentials.
    pub url: String,
    /// Absolute destination folder on that machine.
    pub dest: String,
    pub state: CloneState,
    /// Last progress line from git.
    pub progress: Option<String>,
    /// git's error output when it failed.
    pub error: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// `GET /api/sessions/:id`: the session plus how many subagent sessions
/// ingest recorded under it (origin `external` with `parent_session_id`
/// set to it, §8); list them with `GET /api/sessions?parent=<id>`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub session: Session,
    pub children_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct SessionsPage {
    pub items: Vec<Session>,
    /// Pass as `cursor` to fetch the next page; null on the last page.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct LaunchSession {
    /// Launch in this project's folder on this machine (a project without
    /// folders: its blirp workspace). One of `project_id`/`cwd` is required.
    #[serde(default)]
    #[ts(optional)]
    pub project_id: Option<String>,
    /// Launch in this folder; the project is resolved (and created) from it.
    /// With `project_id` it must be inside one of the project's folders here,
    /// or `add_folder` is set.
    #[serde(default)]
    #[ts(optional)]
    pub cwd: Option<String>,
    /// With `project_id` and a `cwd` outside its folders: register `cwd` (its
    /// git top level inside a repository) as a folder of the project first.
    #[serde(default)]
    #[ts(optional)]
    pub add_folder: Option<bool>,
    /// Agent id, e.g. `claude` or `custom:<name>`.
    pub agent: String,
    /// Typed into the agent once its output settles, followed by Enter.
    #[serde(default)]
    #[ts(optional)]
    pub prompt: Option<String>,
    /// Create a git worktree for the session (git projects only).
    #[serde(default)]
    #[ts(optional)]
    pub worktree: Option<bool>,
    /// Start with a handoff pack of this session (continue in / fork, §9).
    /// Defaults the folder to the source session's folder.
    #[serde(default)]
    #[ts(optional)]
    pub continue_from: Option<String>,
    /// Target machine id; defaults to this machine.
    #[serde(default)]
    #[ts(optional)]
    pub machine: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub cols: Option<u16>,
    #[serde(default)]
    #[ts(optional)]
    pub rows: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PatchSession {
    /// New title; null clears it.
    pub title: Option<String>,
}

/// `POST /api/sessions/:id/worktree/remove`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RemoveWorktree {
    /// Also discard uncommitted changes and untracked files.
    #[serde(default)]
    #[ts(optional)]
    pub force: Option<bool>,
}

/// Where `POST /api/sessions/:id/open` shows the session folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum OpenTarget {
    /// The OS file manager.
    Folder,
    /// `$VISUAL`, `$EDITOR` or `code` when on PATH, else the OS default handler.
    Editor,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct OpenSession {
    pub target: OpenTarget,
}

/// `POST /api/sessions/:id/uploads`: a file pasted or dropped into the
/// session's terminal, saved on the machine that runs the session.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct UploadedFile {
    /// Absolute path on that machine.
    pub path: String,
    /// `path` as a terminal drop would type it on that machine: in double
    /// quotes on Windows, with backslash escapes elsewhere, unchanged when
    /// it needs neither.
    pub quoted: String,
    /// Bytes stored.
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct EventsPage {
    pub items: Vec<Event>,
    /// Pass as `after` to fetch the next page; null on the last page.
    pub next_after: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct SearchHit {
    pub kind: SearchHitKind,
    pub project_id: String,
    pub session_id: Option<String>,
    pub seq: Option<i64>,
    pub record_id: Option<String>,
    /// Record title or session title.
    pub title: Option<String>,
    pub agent: Option<String>,
    /// Plain text excerpt. Matched terms are wrapped in U+0002 (start) and
    /// U+0003 (end); render them as highlights and escape everything else.
    pub snippet: String,
    pub ts: i64,
    /// bm25 rank; lower is better.
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct AgentInfo {
    /// Launch id: built-in id or `custom:<name>`.
    pub id: String,
    pub display_name: String,
    pub builtin: bool,
    pub installed: bool,
    /// Resolved executable.
    pub path: Option<String>,
    /// First line of `--version` output.
    pub version: Option<String>,
    /// Resume by the agent's own session id is supported.
    pub can_resume: bool,
    pub integration: AgentIntegration,
    /// Whether the agent is logged in for this machine's daemon, where that
    /// can be checked without a model call (claude: `claude auth status`);
    /// null when unknown.
    // Default: older daemons do not send it.
    #[serde(default)]
    pub auth: Option<AgentAuth>,
    /// Headless login token (claude only, §7); null for other agents. The
    /// token itself is never sent.
    // Default: older daemons do not send it.
    #[serde(default)]
    pub token: Option<AgentToken>,
}

/// Whether claude sessions and the summarizer on this machine log in with a
/// long-lived OAuth token (`claude setup-token`) instead of the keychain or
/// `~/.claude/.credentials.json`, which a daemon started by launchd on a
/// locked Mac or over SSH cannot use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AgentToken {
    /// A token is stored in `BLIRP_HOME/secrets/claude_oauth_token`
    /// (`blirp agents set-token claude`).
    pub stored: bool,
    /// The daemon's own environment already sets `CLAUDE_CODE_OAUTH_TOKEN`;
    /// sessions inherit it and a stored token is not used.
    pub env: bool,
}

/// `PUT /api/agents/claude/token` (admin).
#[derive(Clone, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SetAgentToken {
    pub token: String,
}

impl std::fmt::Debug for SetAgentToken {
    // The token must never reach a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetAgentToken")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Login state of an agent CLI as seen by the daemon's own process (on
/// macOS a daemon started over SSH cannot read the login keychain, so it
/// may differ from a terminal on the same machine).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AgentAuth {
    pub logged_in: bool,
    /// How it is logged in, as the agent reports it (e.g. `claude.ai`, `api_key`).
    pub method: Option<String>,
}

/// How blirp memory reaches an agent (§9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct AgentIntegration {
    /// Opt-in global hooks in the agent's user config (sessions started outside blirp).
    pub global_hooks: IntegrationState,
    /// blirp MCP server registered in the agent's user config.
    pub mcp: IntegrationState,
    /// Session-start injection used when the session is launched from blirp.
    pub inject: InjectMode,
    /// Extra information, e.g. a manual step the agent requires.
    pub detail: Option<String>,
}

/// `GET /api/inject`: the rendered memory injection.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct Injection {
    pub markdown: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SummaryItem {
    pub title: String,
    pub body: String,
}

/// Last failed distill attempt of a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct DistillFailure {
    pub message: String,
    pub at: i64,
    /// Highest event seq the failed attempt covered; retried only after newer events.
    pub through_seq: i64,
}

/// Shape of `Session.summary` (the `summary_json` column, §9 distill output).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(default)]
pub struct SessionSummary {
    pub title: Option<String>,
    /// 3-6 sentences; null until a distill succeeded.
    pub summary: Option<String>,
    pub decisions: Vec<SummaryItem>,
    pub open_threads: Vec<SummaryItem>,
    pub gotchas: Vec<SummaryItem>,
    pub resolved_record_ids: Vec<String>,
    /// Files touched, relative to the session folder.
    pub files: Vec<String>,
    /// Summarizer backend that produced it (`claude`, `codex`, `ollama`).
    pub backend: Option<String>,
    pub distilled_at: Option<i64>,
    pub through_seq: i64,
    /// Set when the last distill attempt failed.
    pub error: Option<DistillFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct SettingsView {
    pub config: Config,
    /// UI preferences and other free-form values stored in the database.
    pub values: BTreeMap<String, JsonValue>,
    /// Automatic distilling (§9): paused summarizer, today's budget.
    pub distill: DistillStatus,
}

/// Why automatic distilling is paused: the summarizer itself fails, not a
/// session (§9). Retried with exponential backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum DistillPause {
    /// Not logged in, invalid or missing credentials.
    Auth,
    /// No summarizer installed or reachable.
    Unavailable,
    /// Rate or usage limit reached.
    RateLimited,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DistillStatus {
    /// Set while automatic distilling is paused.
    pub paused: Option<DistillPause>,
    /// The summarizer's last error while paused.
    pub reason: Option<String>,
    /// When the summarizer is tried again (unix ms).
    pub retry_at: Option<i64>,
    /// Distill jobs run today (UTC) and the daily limit.
    pub budget_used: u32,
    pub budget_limit: u32,
}

/// What `memory.summarizer = "auto"` resolves to on this machine right now
/// (`GET /api/settings/summarizer`, §9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SummarizerPick {
    /// `claude`, `codex` or `ollama`; null when none is available.
    pub backend: Option<Summarizer>,
    /// Model it runs (`sonnet`, the Ollama model); null for codex, which
    /// uses its built-in default.
    pub model: Option<String>,
    /// `agents.default`, the agent preselected for new sessions.
    pub default_agent: String,
    /// Why the default agent's own summarizer is not used; null when it is.
    pub fallback: Option<SummarizerFallback>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SummarizerFallback {
    /// The default agent has no summarizer backend (only claude and codex do).
    NoBackend,
    /// Its CLI is not on PATH.
    NotInstalled,
    /// Its CLI reports it is not logged in.
    NotLoggedIn,
    /// Its CLI is older than the summarizer supports (codex before 0.153),
    /// or its version is unknown.
    Outdated,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    /// The edited config, validated and written to `config.toml`.
    /// `daemon.port` takes effect after a daemon restart. `sync.role` and
    /// `sync.hub` are never taken from it: hub enable/disable, join and
    /// leave own them.
    #[serde(default)]
    #[ts(optional)]
    pub config: Option<Config>,
    /// The config `config` was edited from (as read from `GET /api/settings`).
    /// With it only the values the client changed are applied, so a stale
    /// copy never reverts what changed since; without it `config` replaces
    /// every other value.
    #[serde(default)]
    #[ts(optional)]
    pub base: Option<Config>,
    /// Keys to set; a null value deletes the key.
    #[serde(default)]
    #[ts(optional)]
    pub values: Option<BTreeMap<String, Option<JsonValue>>>,
}

/// `GET /api/sync/status` (§10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SyncStatus {
    pub role: MachineRole,
    /// This machine's id (its iroh endpoint id).
    pub machine_id: String,
    /// Hub endpoint id: the paired hub on a node, this machine on a hub,
    /// null when standalone.
    pub hub: Option<String>,
    /// Node: the sync session with the hub is up. Hub: the endpoint is running.
    pub connected: bool,
    /// Last completed exchange with the hub (node) or with any node (hub).
    pub last_sync_at: Option<i64>,
    /// Local writes not yet acknowledged by the hub (0 on a hub or standalone).
    pub pending_outbox: i64,
    /// HTTPS LAN portal (hub with `portal.lan`), null when not serving.
    pub portal_url: Option<String>,
    /// SHA-256 of the portal's self-signed certificate, `AA:BB:...`.
    pub portal_cert_fingerprint: Option<String>,
    /// Relay server the sync endpoint is connected to (its home relay), so
    /// machines behind other networks can reach it. Null when relays are
    /// off, no endpoint runs, or no relay has answered yet.
    pub relay_url: Option<String>,
}

/// `POST /api/sync/invite`: show `code` and `invite` (or a QR of `uri`) to
/// the machine that joins.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct SyncInvite {
    /// `blirp1-<base32 ticket>`
    pub invite: String,
    /// `XXXX-XXXX`, single use, 5 attempts.
    pub code: String,
    /// `blirp://join/<invite>#<code>`
    pub uri: String,
    pub expires_at: i64,
}

/// `POST /api/sync/join`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct JoinHub {
    /// Invite from the hub; an empty string finds the hub on the local network.
    pub invite: String,
    pub code: String,
    /// Sets `sync.allow_hub_control` together with the pairing; left out,
    /// the configured value stays (off by default).
    #[serde(default)]
    #[ts(optional)]
    pub allow_hub_control: Option<bool>,
}

/// `POST /api/sync/leave`: this machine is standalone again.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct LeftHub {
    pub status: SyncStatus,
    /// Set when the hub could not be told (it still lists this machine as
    /// paired) or changes made here had not reached it.
    pub warning: Option<String>,
}

/// `POST /api/sync/join/preview`: what an invite says about its hub,
/// without contacting it.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct JoinPreviewRequest {
    /// Invite or `blirp://join/...` link; empty for a LAN join by code.
    pub invite: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct JoinPreview {
    /// The hub's machine id (its endpoint id, verified by the connection
    /// when pairing); null when the hub is found on the LAN by code alone.
    pub hub_id: Option<String>,
}

/// `POST /api/ws-ticket` (loopback listener): browsers cannot send
/// `Authorization` on a WebSocket, so they trade the bearer token for a
/// single-use ticket bound to one path and pass it as `?ticket=`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct WsTicketRequest {
    /// WebSocket path, e.g. `/api/events/ws`.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct WsTicket {
    /// Valid for 30 s, once, for the requested path only.
    pub ticket: String,
}

/// `POST /api/devices/browser-invite`: one-time login link for a browser (5 min).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct BrowserInvite {
    pub url: String,
    pub expires_at: i64,
}

// ---------------------------------------------------------------- project files

str_enum!(
    /// State of the last scan of a folder: `busy` while a git operation
    /// holds the index, `too_large` over the caps (paused until a
    /// `.blirpignore` shrinks it).
    FilesScanState {
        Ok = "ok",
        Busy = "busy",
        TooLarge = "too_large",
    }
);

/// Excluded paths of one reason: how many, and the first 50 (folders end
/// with `/`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ExcludedGroup {
    pub reason: crate::files::rules::Reason,
    pub count: i64,
    pub paths: Vec<String>,
}

/// `GET /api/projects/:id/files-sync/preview?root=`: what uploading this
/// folder would send, computed locally (a dry run: nothing leaves the
/// machine).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct FilesPreview {
    pub root: String,
    pub state: FilesScanState,
    /// Why this folder is never synced (Home, scratch, removable drive...),
    /// null when it can be.
    pub never_synced: Option<String>,
    pub files: i64,
    pub bytes: i64,
    pub excluded: Vec<ExcludedGroup>,
    /// Secrets that `.blirpignore` re-includes: they are uploaded.
    pub reincluded_secrets: Vec<String>,
}

/// `PATCH /api/devices/:id`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PatchDevice {
    pub can_control_terminals: bool,
}

str_enum!(MemoryPart {
    Brief = "brief",
    Records = "records",
    Wiki = "wiki",
    Resources = "resources",
    Suggestions = "suggestions",
});

/// Frames pushed on `/api/events/ws`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    SessionCreated {
        session: Session,
    },
    SessionUpdated {
        session: Session,
    },
    /// The session (and its subagent sessions) was deleted.
    SessionDeleted {
        session_id: String,
    },
    ProjectUpdated {
        project_id: String,
    },
    MemoryUpdated {
        project_id: String,
        part: MemoryPart,
    },
    /// Sync role, connection or portal state changed.
    SyncUpdated {
        status: SyncStatus,
    },
    /// Events were dropped because the client fell behind; refetch state.
    Resync,
}

/// Text frames sent by the server on `/api/terminals/:id/ws` (§6). Raw
/// terminal output is sent as binary frames.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalServerMessage {
    /// Reset the terminal and write `data`; reproduces scrollback, screen,
    /// modes, cursor and title. Sent first, and again after the client lagged.
    Snapshot { cols: u16, rows: u16, data: String },
    /// Sent right after the first snapshot when this client may not control
    /// the terminal: its input and resize frames are ignored.
    Readonly,
    /// Another client resized the terminal (last resize wins); never sent
    /// to the client that asked for the resize.
    Resize { cols: u16, rows: u16 },
    /// The process exited; the socket closes (code 1000) after this frame.
    Exit {
        status: SessionStatus,
        exit_code: Option<i32>,
    },
}

/// Text frames accepted from clients on `/api/terminals/:id/ws`. Binary frames
/// are written to the terminal as raw input bytes.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalClientMessage {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts_rs::Config as TsConfig;

    macro_rules! decls {
        ($cfg:expr; $($t:ty),+ $(,)?) => {{
            let mut out = Vec::new();
            $(
                let docs = <$t as TS>::docs().map(|d| format!("{}\n", d.trim_end())).unwrap_or_default();
                out.push(format!("{docs}export {}", <$t as TS>::decl($cfg)));
            )+
            out
        }};
    }

    /// Writes `web/src/lib/api/types.gen.ts`. Fails (after rewriting) if the
    /// committed file was stale, so CI catches forgotten regenerations.
    #[test]
    fn export_bindings() {
        use crate::config::*;
        let cfg = TsConfig::new().with_large_int("number");
        let decls = decls!(&cfg;
            JsonValue,
            MachineRole, SessionStatus, SessionOrigin, EventKind, RecordKind, RecordStatus,
            SuggestionTarget, SuggestionStatus, ResourceKind, DeviceKind, FileKind, SearchHitKind,
            MemoryPart, IntegrationState, InjectMode, CloneState,
            Machine, MachineInfo, Project, ProjectPath, Session, Event, Record, Brief, WikiPage, Suggestion,
            BriefProposal, RecordProposal, WikiProposal, Resource, Device,
            ErrorBody, ErrorDetail, Health, UpdateStatus, UpdateOutcome, ProjectPathInfo, ProjectSummary, CreateProject,
            PatchProject, MergeProject, RemoveProjectFolder, MoveSession, ProjectMemory, PutBrief, RevertBrief, CreateRecord,
            PatchRecord, CreateWikiPage, PutWikiPage, CreateResource, PatchResource,
            GitStatusEntry, GitStatus, GitDiff, FileEntry, DirListing, FileContent, SessionsPage, SessionDetail,
            LaunchSession, PatchSession, RemoveWorktree, OpenTarget, OpenSession, UploadedFile, EventsPage, SearchHit, SearchResults, AgentInfo,
            AgentIntegration, AgentAuth, AgentToken, SetAgentToken, MachineDirs, MachineDir, CloneRepo, CloneJob, Injection, SummaryItem, DistillFailure, SessionSummary,
            SettingsView, SettingsPatch, Capabilities, DistillStatus, DistillPause, SummarizerPick, SummarizerFallback, SyncStatus, SyncInvite, JoinHub, JoinPreviewRequest, JoinPreview, LeftHub, WsTicketRequest, WsTicket, BrowserInvite, PatchDevice,
            ServerEvent, TerminalServerMessage, TerminalClientMessage,
            Config, DaemonConfig, MachineConfig, AgentsConfig, CustomAgent, SessionsConfig,
            FilesScanState, ExcludedGroup, FilesPreview, crate::files::rules::Reason,
            Summarizer, BriefMode, MemoryConfig, SyncConfig, PortalConfig, UpdateConfig, FilesConfig,
        );
        let body = format!(
            "// Generated by `cargo test -p blirp-core export_bindings` from crates/blirp-core/src/model.rs.\n// Do not edit by hand.\n\n{}\n",
            decls.join("\n\n")
        );
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../web/src/lib/api/types.gen.ts");
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current.replace("\r\n", "\n") != body {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &body).unwrap();
            assert!(
                std::env::var_os("CI").is_none(),
                "types.gen.ts was stale and has been regenerated; commit it"
            );
        }
    }

    #[test]
    fn enums_roundtrip_strings() {
        for s in SessionStatus::ALL {
            assert_eq!(s.as_str().parse::<SessionStatus>().unwrap(), *s);
            assert_eq!(
                serde_json::to_value(s).unwrap(),
                JsonValue::String(s.as_str().into())
            );
        }
        assert_eq!(RecordKind::OpenThread.as_str(), "open_thread");
        assert!("bogus".parse::<RecordKind>().is_err());
    }

    #[test]
    fn server_event_is_tagged() {
        let v = serde_json::to_value(ServerEvent::MemoryUpdated {
            project_id: "p".into(),
            part: MemoryPart::Brief,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"type":"memory_updated","project_id":"p","part":"brief"})
        );
    }
}
