//! Headless Claude Code login (§7): a long-lived OAuth token from
//! `claude setup-token`, stored in `BLIRP_HOME/secrets/claude_oauth_token`
//! and passed as `CLAUDE_CODE_OAUTH_TOKEN` to the claude processes the
//! daemon starts (sessions, the summarizer, the login probe). A daemon
//! started by launchd on a locked Mac, or over SSH, cannot read the login
//! keychain: claude then hangs before its prompt or reports "not logged in".
//!
//! The token never enters the database (so it is never replicated), is
//! never logged and is never returned by the API.

use crate::model::AgentToken;
use crate::paths::{Paths, PathsError, create_private_dir};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::Path;

/// The variable Claude Code reads a `claude setup-token` token from.
pub const ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// Far above any real token; only stops pasting a whole file.
const MAX_LEN: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("the token is empty")]
    Empty,
    #[error("the token is too long (at most {MAX_LEN} characters)")]
    TooLong,
    #[error("the token must be a single word: no spaces, line breaks or control characters")]
    Malformed,
    #[error(transparent)]
    Paths(#[from] PathsError),
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> TokenError {
    let path = path.to_path_buf();
    move |source| TokenError::Io {
        action,
        path,
        source,
    }
}

/// The token without surrounding whitespace (a pasted line ends in a
/// newline). Its format is Anthropic's business; only what cannot be one
/// token (or be passed in an environment variable) is refused.
pub fn validate(raw: &str) -> Result<&str, TokenError> {
    let token = raw.trim();
    if token.is_empty() {
        return Err(TokenError::Empty);
    }
    if token.len() > MAX_LEN {
        return Err(TokenError::TooLong);
    }
    if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(TokenError::Malformed);
    }
    Ok(token)
}

/// Store the token, replacing any earlier one. The secrets dir is 0700 and
/// the file 0600 on unix (written to a fresh temp file, then renamed, so it
/// is never readable by others, not even briefly); on Windows both inherit
/// the user profile ACL.
pub fn store(paths: &Paths, raw: &str) -> Result<(), TokenError> {
    let token = validate(raw)?;
    create_private_dir(&paths.secrets_dir())?;
    let file = paths.claude_token_file();
    let tmp = file.with_extension("tmp");
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io("remove", &tmp)(e)),
    }
    let mut opts = std::fs::OpenOptions::new();
    // create_new: never write through something already at the temp path.
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let written = opts
        .open(&tmp)
        .map_err(io("create", &tmp))
        .and_then(|mut f| {
            f.write_all(token.as_bytes()).map_err(io("write", &tmp))?;
            f.sync_all().map_err(io("sync", &tmp))
        });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &file).map_err(io("rename", &file)))
    {
        // Best effort: the temp file holds the token; the error is what matters.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// The stored token, `None` when there is none.
pub fn read(paths: &Paths) -> Result<Option<String>, TokenError> {
    let file = paths.claude_token_file();
    match std::fs::read_to_string(&file) {
        Ok(text) if text.trim().is_empty() => Ok(None),
        Ok(text) => validate(&text).map(|t| Some(t.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io("read", &file)(e)),
    }
}

/// Remove the stored token. `false` when there was none.
pub fn clear(paths: &Paths) -> Result<bool, TokenError> {
    let file = paths.claude_token_file();
    match std::fs::remove_file(&file) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io("remove", &file)(e)),
    }
}

/// The value of [`ENV`] in this process, when set and non-empty.
fn process_env() -> Option<OsString> {
    std::env::var_os(ENV).filter(|v| !v.is_empty())
}

/// What claude processes started by this process log in with.
pub fn status(paths: &Paths) -> AgentToken {
    AgentToken {
        // A file that holds no usable token is not used, so it does not count.
        stored: matches!(read(paths), Ok(Some(_))),
        env: process_env().is_some(),
    }
}

/// The env var to add to a claude process started now: the stored token,
/// read at every spawn so `set-token` takes effect without a restart.
/// `None` when this process's environment already sets [`ENV`] (children
/// inherit it; an explicit setting wins) or no token is stored. A token
/// that cannot be read is logged (never its content) and left out.
pub fn launch_env(paths: &Paths) -> Option<(String, String)> {
    launch_env_with(paths, process_env())
}

fn launch_env_with(paths: &Paths, inherited: Option<OsString>) -> Option<(String, String)> {
    if inherited.is_some() {
        return None;
    }
    match read(paths) {
        Ok(token) => token.map(|t| (ENV.to_string(), t)),
        Err(e) => {
            tracing::warn!(error = %e, "stored claude login token unusable; starting claude without it");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_read_clear() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("home"));
        assert_eq!(read(&paths).unwrap(), None);
        assert!(!status(&paths).stored);
        assert!(!clear(&paths).unwrap());

        store(&paths, "  sk-ant-oat01-abc_DEF-123\r\n").unwrap();
        assert_eq!(
            read(&paths).unwrap().as_deref(),
            Some("sk-ant-oat01-abc_DEF-123")
        );
        assert_eq!(
            std::fs::read_to_string(paths.claude_token_file()).unwrap(),
            "sk-ant-oat01-abc_DEF-123"
        );
        assert!(status(&paths).stored);
        // Replacing leaves no temp file behind.
        store(&paths, "second").unwrap();
        assert_eq!(read(&paths).unwrap().as_deref(), Some("second"));
        assert_eq!(
            std::fs::read_dir(paths.secrets_dir()).unwrap().count(),
            1,
            "only the token file"
        );

        assert!(clear(&paths).unwrap());
        assert_eq!(read(&paths).unwrap(), None);
        assert!(!clear(&paths).unwrap());
    }

    #[test]
    fn only_what_cannot_be_a_token_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        for bad in [
            "",
            "   \n",
            "two words",
            "a\nb",
            "a\u{0}b",
            &"x".repeat(MAX_LEN + 1),
        ] {
            assert!(store(&paths, bad).is_err(), "{bad:?}");
        }
        assert!(!paths.claude_token_file().exists());
        assert_eq!(validate(" tok\n").unwrap(), "tok");
        // Formats are not second-guessed.
        assert!(validate("anything-goes.here/+=").is_ok());
        // A hand-edited file that is not a token is not passed on.
        std::fs::create_dir_all(paths.secrets_dir()).unwrap();
        std::fs::write(paths.claude_token_file(), "a b").unwrap();
        assert!(read(&paths).is_err());
        assert_eq!(launch_env_with(&paths, None), None);
        assert!(!status(&paths).stored);
    }

    #[test]
    fn env_is_injected_unless_already_set() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        assert_eq!(launch_env_with(&paths, None), None);
        store(&paths, "tok").unwrap();
        assert_eq!(
            launch_env_with(&paths, None),
            Some((ENV.to_string(), "tok".to_string()))
        );
        // The daemon's own setting is inherited and wins.
        assert_eq!(launch_env_with(&paths, Some("mine".into())), None);
        // Read at every call: a new token is used at the next spawn.
        store(&paths, "newer").unwrap();
        assert_eq!(
            launch_env_with(&paths, None).map(|(_, v)| v).as_deref(),
            Some("newer")
        );
        clear(&paths).unwrap();
        assert_eq!(launch_env_with(&paths, None), None);
    }

    #[test]
    fn debug_output_hides_the_token() {
        let body: crate::model::SetAgentToken =
            serde_json::from_str(r#"{"token":"sk-secret-value"}"#).unwrap();
        assert!(!format!("{body:?}").contains("sk-secret-value"));
    }

    #[cfg(unix)]
    #[test]
    fn owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        // A loose secrets dir and a loose leftover temp file are not reused as they are.
        std::fs::create_dir(paths.secrets_dir()).unwrap();
        std::fs::set_permissions(paths.secrets_dir(), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let tmp = paths.claude_token_file().with_extension("tmp");
        std::fs::write(&tmp, "old").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        store(&paths, "tok").unwrap();
        assert_eq!(mode(&paths.secrets_dir()), 0o700);
        assert_eq!(mode(&paths.claude_token_file()), 0o600);
        assert!(!tmp.exists());
    }
}
