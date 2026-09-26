//! Memory engine (§9): distill, injection rendering, handoff packs and
//! per-launch agent integration.

pub mod distill;
pub mod launch;
pub mod render;
#[cfg(test)]
pub(crate) mod testutil;

use std::path::{Path, PathBuf};

/// Lets hooks tell the ingest subsystem where an agent's transcript lives, so
/// it can be ingested promptly. The ingest module installs its implementation
/// with `AppState::set_ingest_trigger`; the default does nothing.
pub trait IngestTrigger: Send + Sync {
    /// `session_id` is the blirp session the transcript belongs to.
    fn transcript_hint(&self, agent: &str, session_id: &str, transcript_path: &Path);
}

/// Default trigger used until an ingest implementation is installed.
pub struct NoopIngest;

impl IngestTrigger for NoopIngest {
    fn transcript_hint(&self, _agent: &str, _session_id: &str, _transcript_path: &Path) {}
}

/// Absolute path of the running `blirp` executable, used in hook commands and
/// MCP server definitions.
pub fn blirp_exe() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| dunce::canonicalize(&p).ok().or(Some(p)))
        .unwrap_or_else(|| PathBuf::from("blirp"))
}

/// Path with `/` separators (valid on Windows too) for shell command strings.
pub fn slash_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// Shell command running `blirp hook <agent> <event>` (plus `--global` for
/// entries installed into user configs).
pub fn hook_command(exe: &Path, agent: &str, event: &str, global: bool) -> String {
    let exe = slash_path(exe);
    let exe = if exe.contains(' ') {
        format!("\"{exe}\"")
    } else {
        exe
    };
    let mut cmd = format!("{exe} hook {agent} {event}");
    if global {
        cmd.push_str(" --global");
    }
    cmd
}
