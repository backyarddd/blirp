//! Agent specs (§7): binaries, launch/resume argv, detection.

use blirp_core::config::Config;
use blirp_core::model::{AgentAuth, AgentInfo, AgentIntegration, InjectMode, IntegrationState};
use blirp_core::paths::Paths;
use blirp_core::{claude_token, process};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How an agent resumes one of its own sessions by id.
#[derive(Debug, Clone, Copy)]
enum Resume {
    /// No id-based resume; relaunch fresh.
    None,
    /// Arguments placed before the id, e.g. `["--resume"]`.
    Args(&'static [&'static str]),
}

#[derive(Debug)]
struct Builtin {
    id: &'static str,
    display_name: &'static str,
    /// Executable names in preference order.
    binaries: &'static [&'static str],
    resume: Resume,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        id: "claude",
        display_name: "Claude Code",
        binaries: &["claude"],
        resume: Resume::Args(&["--resume"]),
    },
    Builtin {
        id: "codex",
        display_name: "Codex",
        binaries: &["codex"],
        resume: Resume::Args(&["resume"]),
    },
    Builtin {
        id: "opencode",
        display_name: "opencode",
        binaries: &["opencode"],
        resume: Resume::Args(&["--session"]),
    },
    Builtin {
        id: "pi",
        display_name: "pi",
        binaries: &["pi"],
        resume: Resume::None,
    },
    Builtin {
        id: "gemini",
        display_name: "Gemini CLI",
        binaries: &["gemini"],
        resume: Resume::Args(&["--resume"]),
    },
    Builtin {
        id: "cursor",
        display_name: "Cursor CLI",
        binaries: &["cursor-agent", "agent"],
        resume: Resume::Args(&["--resume"]),
    },
    Builtin {
        id: "amp",
        display_name: "Amp",
        binaries: &["amp"],
        resume: Resume::Args(&["threads", "continue"]),
    },
    Builtin {
        id: "aider",
        display_name: "Aider",
        binaries: &["aider"],
        resume: Resume::None,
    },
    Builtin {
        id: "dsh",
        display_name: "DeepSeek Harness",
        binaries: &["dsh"],
        resume: Resume::None,
    },
];

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("unknown agent {0:?}")]
    Unknown(String),
    #[error("{0} is not installed (not found on PATH)")]
    NotInstalled(String),
    #[error("cannot pass this to a Windows batch file: {0}")]
    InvalidArgument(String),
}

/// A resolved, launchable agent.
#[derive(Debug, Clone)]
pub struct Agent {
    pub id: String,
    pub display_name: String,
    pub builtin: bool,
    /// Resolved executable, `None` when not installed.
    pub path: Option<PathBuf>,
    extra_args: Vec<String>,
    resume: Resume,
}

pub struct LaunchContext<'a> {
    /// The agent's own session id to create (claude) or resume.
    pub agent_session_id: Option<&'a str>,
    pub resume: bool,
    /// Memory integration args (§9) placed before the launch/resume args.
    pub args_before: &'a [OsString],
    /// Memory integration args placed after them.
    pub args_after: &'a [OsString],
}

/// Default interactive shell: `$SHELL` (unix), else pwsh / Windows PowerShell / cmd.
fn default_shell() -> Option<PathBuf> {
    if cfg!(windows) {
        ["pwsh", "powershell"]
            .iter()
            .find_map(|n| process::which(n))
            .or_else(|| std::env::var_os("ComSpec").map(PathBuf::from))
            .or_else(|| process::which("cmd"))
    } else {
        std::env::var_os("SHELL")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .or_else(|| Some(PathBuf::from("/bin/sh")))
    }
}

impl Agent {
    /// Resolve `id` (`claude`, `shell`, `custom:<name>`, ...) against PATH and config.
    pub fn resolve(id: &str, config: &Config) -> Result<Agent, AgentError> {
        if id == "shell" {
            return Ok(Agent {
                id: id.into(),
                display_name: "Shell".into(),
                builtin: true,
                path: default_shell(),
                extra_args: Vec::new(),
                resume: Resume::None,
            });
        }
        if let Some(name) = id.strip_prefix("custom:") {
            let c = config
                .agents
                .custom
                .iter()
                .find(|c| c.name == name)
                .ok_or_else(|| AgentError::Unknown(id.into()))?;
            return Ok(Agent {
                id: id.into(),
                display_name: c.name.clone(),
                builtin: false,
                path: process::which(&c.command),
                extra_args: c.args.clone(),
                resume: Resume::None,
            });
        }
        let b = BUILTINS
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| AgentError::Unknown(id.into()))?;
        Ok(Agent {
            id: id.into(),
            display_name: b.display_name.into(),
            builtin: true,
            path: b.binaries.iter().find_map(|n| process::which(n)),
            extra_args: Vec::new(),
            resume: b.resume,
        })
    }

    /// Every built-in agent plus configured custom agents.
    pub fn all(config: &Config) -> Vec<Agent> {
        let mut ids: Vec<String> = BUILTINS.iter().map(|b| b.id.to_string()).collect();
        ids.push("shell".into());
        ids.extend(
            config
                .agents
                .custom
                .iter()
                .map(|c| format!("custom:{}", c.name)),
        );
        ids.iter()
            .filter_map(|id| Agent::resolve(id, config).ok())
            .collect()
    }

    pub fn can_resume(&self) -> bool {
        matches!(self.resume, Resume::Args(_))
    }

    /// Whether blirp assigns the agent's session id at launch (claude only).
    pub fn assigns_session_id(&self) -> bool {
        self.id == "claude"
    }

    /// Program, argv and extra env for a launch or resume.
    pub fn command(&self, ctx: &LaunchContext<'_>) -> Result<PlatformCommand, AgentError> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| AgentError::NotInstalled(self.display_name.clone()))?;
        let mut args: Vec<OsString> = self.extra_args.iter().map(OsString::from).collect();
        args.extend(ctx.args_before.iter().cloned());
        if self.id == "shell" && cfg!(windows) && is_powershell(path) {
            args.push("-NoLogo".into());
        }
        let resuming = ctx.resume && self.can_resume();
        // The id comes from transcripts and hooks; it lands in argv, so an
        // id that could be read as an option (or anything else odd) is refused.
        if let Some(id) = ctx.agent_session_id
            && (resuming || self.assigns_session_id())
            && !valid_agent_session_id(id)
        {
            return Err(AgentError::InvalidArgument(format!(
                "agent session id {id:?}"
            )));
        }
        match (resuming, self.resume, ctx.agent_session_id) {
            (true, Resume::Args(prefix), Some(id)) => {
                args.extend(prefix.iter().map(OsString::from));
                args.push(id.into());
            }
            _ if self.assigns_session_id() => {
                if let Some(id) = ctx.agent_session_id {
                    args.extend(["--session-id".into(), id.into()]);
                }
            }
            _ => {}
        }
        args.extend(ctx.args_after.iter().cloned());
        wrap_for_platform(path, args)
    }
}

/// Agents' own session ids: uuids, `ses_...`, `T-...`, `<uuid>:agent-<id>`.
/// Never empty, never starting with `-`, only `[A-Za-z0-9_.:-]`.
fn valid_agent_session_id(id: &str) -> bool {
    (1..=200).contains(&id.len())
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}

fn is_powershell(p: &Path) -> bool {
    p.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("pwsh") || s.eq_ignore_ascii_case("powershell"))
}

/// The node program and script behind an npm `cmd-shim` (`codex.cmd`,
/// `gemini.cmd`, ...), when the shim runs node and both files exist. Current
/// shims end in `"%_prog%" "%dp0%\node_modules\...\x.js" %*` with
/// `SET "_prog=node"`; older ones run `"%~dp0\node.exe" "%~dp0\...\x.js" %*`
/// or `node "%~dp0\...\x.js" %*`. Shims of other targets (a native `.exe`,
/// a `sh` script) are not unwrapped.
fn npm_shim_target(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.len() > 16 * 1024 {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    if !lower.contains(r#"set "_prog=node""#) && !lower.contains(r#""%~dp0\node.exe""#) {
        return None;
    }
    // The quoted script that is directly followed by `%*`.
    let rel = [r#""%dp0%\"#, r#""%~dp0\"#].iter().find_map(|mark| {
        text.match_indices(mark).find_map(|(i, m)| {
            let start = i + m.len();
            let len = text[start..].find('"')?;
            text[start + len + 1..]
                .trim_start()
                .starts_with("%*")
                .then(|| &text[start..start + len])
        })
    })?;
    let dir = path.parent()?;
    let script = dir.join(rel);
    let node = Some(dir.join("node.exe"))
        .filter(|p| p.is_file())
        .or_else(|| process::which("node"))?;
    script.is_file().then_some((node, script))
}

/// A program with its arguments, plus env it needs, ready to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub env: Vec<(String, String)>,
}

impl PlatformCommand {
    fn plain(program: OsString, args: Vec<OsString>) -> Self {
        Self {
            program,
            args,
            env: Vec::new(),
        }
    }

    /// A `std::process::Command` for it (no console window on Windows).
    pub fn std_command(&self) -> std::process::Command {
        let mut cmd = process::command(&self.program);
        cmd.args(&self.args).envs(self.env.iter().cloned());
        cmd
    }
}

/// Env var carrying a batch file's escaped command line (see
/// [`wrap_for_platform`]).
pub const CMD_LINE_ENV: &str = "BLIRP_CMD_LINE";

/// Append `arg` to a `cmd.exe` command line so cmd passes it on as one
/// literal argument: anything but plain word characters is quoted (inside
/// quotes `& | < > ( ) ^` are literal to cmd), embedded quotes are doubled
/// with the backslashes before them doubled (how MSVC-style parsers of the
/// forwarded `%*` read it), and a trailing backslash is doubled so it cannot
/// escape the closing quote. Line breaks and NUL cannot be passed at all.
fn cmd_quote(arg: &str, out: &mut String) -> Result<(), AgentError> {
    if arg.contains(['\r', '\n', '\0']) {
        return Err(AgentError::InvalidArgument(
            "arguments must not contain line breaks".into(),
        ));
    }
    const PLAIN: &str = r"#$*+-./:?@\_";
    let quote = arg.is_empty()
        || arg.ends_with('\\')
        || arg.chars().any(|c| {
            c.is_control() || (c.is_ascii() && !c.is_ascii_alphanumeric() && !PLAIN.contains(c))
        });
    if !quote {
        out.push_str(arg);
        return Ok(());
    }
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
        } else {
            if c == '"' {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push('"');
            }
            backslashes = 0;
        }
        out.push(c);
    }
    out.extend(std::iter::repeat_n('\\', backslashes));
    out.push('"');
    Ok(())
}

/// The line `cmd.exe` runs for batch file `script`: the quoted path, then
/// the quoted arguments.
fn cmd_line(script: &Path, args: &[OsString]) -> Result<String, AgentError> {
    let path = script
        .to_str()
        .filter(|p| !p.contains('"'))
        .ok_or_else(|| AgentError::InvalidArgument(script.display().to_string()))?;
    let mut line = format!("\"{path}\"");
    for a in args {
        let a = a
            .to_str()
            .ok_or_else(|| AgentError::InvalidArgument("argument is not valid Unicode".into()))?;
        line.push(' ');
        cmd_quote(a, &mut line)?;
    }
    Ok(line)
}

/// On Windows, npm `.cmd` shims run their node script directly (so
/// arguments never pass through `cmd.exe`), other `.cmd`/`.bat` files run
/// through `cmd.exe` and `.ps1` shims through PowerShell; everything else is
/// executed directly.
///
/// Batch files: the escaped command line (see [`cmd_quote`]) travels in the
/// [`CMD_LINE_ENV`] env var and cmd runs `/d /v:off /c %BLIRP_CMD_LINE%`.
/// cmd expands the variable once and then parses the result like a typed
/// line: quotes protect the metacharacters, and a `%` in the value is not
/// expanded again. The environment is needed because the PTY spawner quotes
/// every argument MSVC-style (there is no raw command line), and `\"` means
/// nothing to cmd; `%BLIRP_CMD_LINE%` itself contains nothing to quote.
pub fn wrap_for_platform(path: &Path, args: Vec<OsString>) -> Result<PlatformCommand, AgentError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if cfg!(windows)
        && ext.as_deref() == Some("cmd")
        && let Some((node, script)) = npm_shim_target(path)
    {
        let mut a: Vec<OsString> = vec![script.into_os_string()];
        a.extend(args);
        return Ok(PlatformCommand::plain(node.into_os_string(), a));
    }
    Ok(match ext.as_deref() {
        Some("cmd" | "bat") if cfg!(windows) => {
            let cmd = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
            let line = cmd_line(path, &args)?;
            PlatformCommand {
                program: cmd,
                args: ["/d", "/v:off", "/c", &format!("%{CMD_LINE_ENV}%")]
                    .iter()
                    .map(OsString::from)
                    .collect(),
                env: vec![(CMD_LINE_ENV.to_string(), line)],
            }
        }
        Some("ps1") if cfg!(windows) => {
            let ps = ["pwsh", "powershell"]
                .iter()
                .find_map(|n| process::which(n))
                .map_or_else(|| OsString::from("powershell.exe"), PathBuf::into_os_string);
            let mut a: Vec<OsString> = [
                "-NoLogo",
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]
            .iter()
            .map(OsString::from)
            .collect();
            a.push(path.into());
            a.extend(args);
            PlatformCommand::plain(ps, a)
        }
        _ => PlatformCommand::plain(path.as_os_str().to_owned(), args),
    })
}

/// First line of `<agent> --version`, `None` on failure or timeout.
pub fn probe_version(path: &Path) -> Option<String> {
    let cmd = wrap_for_platform(path, vec!["--version".into()]).ok()?;
    let out = process::run(cmd.std_command(), Duration::from_secs(5), 64 * 1024).ok()?;
    if !out.success() {
        return None;
    }
    let text = String::from_utf8_lossy(if out.stdout.is_empty() {
        &out.stderr
    } else {
        &out.stdout
    })
    .into_owned();
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(120).collect())
}

/// `claude auth status` (JSON by default, local only: no model call, no
/// tokens). It runs in the daemon's own context, which is what matters: on
/// macOS a daemon started over SSH cannot read the login keychain and
/// reports "not logged in" even when a terminal on the Mac is. It gets the
/// stored login token like sessions do (`token_env`), so it reports what
/// they will see. `None` when the CLI has no such command (older versions)
/// or its output is not JSON.
pub fn probe_claude_auth(path: &Path, token_env: Option<(String, String)>) -> Option<AgentAuth> {
    let cmd = wrap_for_platform(path, vec!["auth".into(), "status".into()]).ok()?;
    let mut command = cmd.std_command();
    command.envs(token_env);
    match process::run(command, Duration::from_secs(10), 64 * 1024) {
        // Exits 1 when logged out; the JSON says which.
        Ok(out) => parse_claude_auth(&out.stdout),
        Err(e) => {
            tracing::debug!(error = %e, "claude auth status failed");
            None
        }
    }
}

fn parse_claude_auth(stdout: &[u8]) -> Option<AgentAuth> {
    let v: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let logged_in = v.get("loggedIn")?.as_bool()?;
    let method = v
        .get("authMethod")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty() && *m != "none")
        .map(str::to_string);
    Some(AgentAuth { logged_in, method })
}

/// Memory integration status of `agent` against the real user configs (read-only).
pub fn integration(agent: &str) -> AgentIntegration {
    let status = match crate::hooks::install::Homes::from_env() {
        Some(h) => crate::hooks::install::status(agent, &h),
        None => crate::hooks::install::Status {
            hooks: IntegrationState::Unsupported,
            mcp: IntegrationState::Unsupported,
            detail: Some("home directory unknown".into()),
        },
    };
    let mut inject = crate::memory::launch::inject_mode(agent);
    // Cursor has no launch-time mechanism; its global sessionStart hook injects instead.
    if inject == InjectMode::None && status.hooks == IntegrationState::Installed {
        inject = InjectMode::Hook;
    }
    AgentIntegration {
        global_hooks: status.hooks,
        mcp: status.mcp,
        inject,
        detail: status.detail,
    }
}

/// Headless login token state; claude is the only agent that has one.
fn token_status(agent: &str, paths: &Paths) -> Option<blirp_core::model::AgentToken> {
    (agent == "claude").then(|| claude_token::status(paths))
}

/// Detect every agent with its version, probing in parallel. `paths`
/// locates claude's stored login token.
pub fn detect_all(config: &Config, paths: &Paths) -> Vec<AgentInfo> {
    let agents = Agent::all(config);
    std::thread::scope(|s| {
        let handles: Vec<_> = agents
            .iter()
            .map(|a| {
                s.spawn(move || {
                    // Shells do not answer `--version` uniformly; their presence is enough.
                    let version = match &a.path {
                        Some(p) if a.id != "shell" => probe_version(p),
                        _ => None,
                    };
                    let auth = match &a.path {
                        Some(p) if a.id == "claude" => {
                            probe_claude_auth(p, claude_token::launch_env(paths))
                        }
                        _ => None,
                    };
                    AgentInfo {
                        id: a.id.clone(),
                        display_name: a.display_name.clone(),
                        builtin: a.builtin,
                        installed: a.path.is_some(),
                        path: a.path.as_ref().map(|p| p.display().to_string()),
                        version,
                        can_resume: a.can_resume(),
                        integration: integration(&a.id),
                        auth,
                        token: token_status(&a.id, paths),
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .zip(&agents)
            .map(|(h, a)| {
                h.join().unwrap_or_else(|_| AgentInfo {
                    id: a.id.clone(),
                    display_name: a.display_name.clone(),
                    builtin: a.builtin,
                    installed: a.path.is_some(),
                    path: None,
                    version: None,
                    can_resume: a.can_resume(),
                    integration: integration(&a.id),
                    auth: None,
                    token: token_status(&a.id, paths),
                })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Output of `claude auth status` 2.1.x (logged in; over SSH on macOS).
    #[test]
    fn claude_auth_status_is_parsed() {
        let yes = br#"{"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty", "email": "x@y"}"#;
        assert_eq!(
            parse_claude_auth(yes),
            Some(AgentAuth {
                logged_in: true,
                method: Some("claude.ai".into())
            })
        );
        let no = br#"{"loggedIn": false, "authMethod": "none", "apiProvider": "firstParty"}"#;
        assert_eq!(
            parse_claude_auth(no),
            Some(AgentAuth {
                logged_in: false,
                method: None
            })
        );
        assert_eq!(parse_claude_auth(b"error: unknown command 'auth'"), None);
        assert_eq!(parse_claude_auth(br#"{"other": 1}"#), None);
    }

    fn agent(id: &str, path: &str) -> Agent {
        let mut a = Agent::resolve(id, &Config::default()).unwrap();
        a.path = Some(PathBuf::from(path));
        a
    }

    fn argv(a: &Agent, resume: bool, sid: Option<&str>) -> Vec<String> {
        let PlatformCommand { args, .. } = a
            .command(&LaunchContext {
                agent_session_id: sid,
                resume,
                args_before: &[],
                args_after: &[],
            })
            .unwrap();
        args.into_iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn launch_and_resume_args() {
        let claude = agent("claude", "/bin/claude");
        assert_eq!(argv(&claude, false, Some("u1")), ["--session-id", "u1"]);
        assert_eq!(argv(&claude, true, Some("u1")), ["--resume", "u1"]);
        let codex = agent("codex", "/bin/codex");
        assert!(argv(&codex, false, None).is_empty());
        assert_eq!(argv(&codex, true, Some("r1")), ["resume", "r1"]);
        // An id that would be read as an option (or carries anything but
        // id characters) never reaches argv.
        for bad in ["-cnotify=[\"calc\"]", "--help", "a b", "", "a;b"] {
            for a in [&codex, &claude] {
                let r = a.command(&LaunchContext {
                    agent_session_id: Some(bad),
                    resume: true,
                    args_before: &[],
                    args_after: &[],
                });
                assert!(matches!(r, Err(AgentError::InvalidArgument(_))), "{bad:?}");
            }
        }
        // Resume without a known id relaunches fresh.
        assert!(argv(&codex, true, None).is_empty());
        let aider = agent("aider", "/bin/aider");
        assert!(argv(&aider, true, Some("x")).is_empty());
        let amp = agent("amp", "/bin/amp");
        assert_eq!(
            argv(&amp, true, Some("T-1")),
            ["threads", "continue", "T-1"]
        );
    }

    #[test]
    fn integration_args_surround_launch_args() {
        let codex = agent("codex", "/bin/codex");
        let PlatformCommand { args, .. } = codex
            .command(&LaunchContext {
                agent_session_id: Some("r1"),
                resume: true,
                args_before: &["-c".into(), "a=1".into()],
                args_after: &["--x".into()],
            })
            .unwrap();
        let args: Vec<String> = args
            .into_iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["-c", "a=1", "resume", "r1", "--x"]);
    }

    #[cfg(windows)]
    #[test]
    fn npm_shims_run_their_script_directly() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join(r"node_modules\@openai\codex\bin");
        std::fs::create_dir_all(&script).unwrap();
        std::fs::write(script.join("codex.js"), "").unwrap();
        std::fs::write(dir.path().join("node.exe"), "").unwrap();
        let shim = dir.path().join("codex.cmd");
        // Verbatim cmd-shim output.
        std::fs::write(
            &shim,
            r#"@ECHO off
GOTO start
:find_dp0
SET dp0=%~dp0
EXIT /b
:start
SETLOCAL
CALL :find_dp0

IF EXIST "%dp0%\node.exe" (
  SET "_prog=%dp0%\node.exe"
) ELSE (
  SET "_prog=node"
  SET PATHEXT=%PATHEXT:;.JS;=;%
)

endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & "%_prog%"  "%dp0%\node_modules\@openai\codex\bin\codex.js" %*
"#,
        )
        .unwrap();
        let c = wrap_for_platform(&shim, vec!["a b\"c".into()]).unwrap();
        assert_eq!(c.program, dir.path().join("node.exe").into_os_string());
        assert_eq!(c.args[0], script.join("codex.js").into_os_string());
        assert_eq!(c.args[1], "a b\"c");
        assert!(c.env.is_empty());

        // A shim of a native binary is not a node script.
        let exe_shim = dir.path().join("tool.cmd");
        std::fs::write(
            &exe_shim,
            "@ECHO off\r\n\"%~dp0\\node_modules\\tool\\bin\\tool.exe\"   %*\r\n",
        )
        .unwrap();
        assert_eq!(npm_shim_target(&exe_shim), None);
    }

    #[test]
    fn cmd_quoting() {
        let q = |a: &str| {
            let mut s = String::new();
            cmd_quote(a, &mut s).map(|()| s)
        };
        assert_eq!(q("plain-arg_1.txt").unwrap(), "plain-arg_1.txt");
        assert_eq!(q("").unwrap(), r#""""#);
        assert_eq!(q("a b").unwrap(), r#""a b""#);
        assert_eq!(q("x&y").unwrap(), r#""x&y""#);
        assert_eq!(q("50%").unwrap(), r#""50%""#);
        assert_eq!(q("^|<>()").unwrap(), r#""^|<>()""#);
        assert_eq!(q(r#"say "hi""#).unwrap(), r#""say ""hi""""#);
        assert_eq!(q(r#"a\"b"#).unwrap(), r#""a\\""b""#);
        assert_eq!(q(r"C:\dir\").unwrap(), r#""C:\dir\\""#);
        assert!(q("two\nlines").is_err());
    }

    // Batch files get their arguments literally: a path with spaces and
    // parentheses works, and `&`, `%`, `^`, `|`, `<`, `>`, quotes and `!`
    // are never interpreted by cmd.
    #[cfg(windows)]
    #[test]
    fn batch_files_receive_arguments_literally() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("my tools (x86)");
        std::fs::create_dir_all(&bin).unwrap();
        let shim = bin.join("echo args.cmd");
        // `%*` is the raw argument text; `echo(` prints it, and cmd does not
        // interpret metacharacters that are inside quotes.
        std::fs::write(&shim, "@echo off\r\necho(%*\r\n").unwrap();
        let args: Vec<OsString> = [
            "plain",
            "a&b",
            "& echo INJECTED",
            "50%",
            "%PATH%",
            "x|y<z>w^v",
            r#"say "hi""#,
            "bang!",
            "",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let c = wrap_for_platform(&shim, args).unwrap();
        let out = process::run(c.std_command(), Duration::from_secs(20), 64 * 1024).unwrap();
        assert!(out.success(), "{out:?}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            text.trim_end(),
            r#"plain "a&b" "& echo INJECTED" "50%" "%PATH%" "x|y<z>w^v" "say ""hi""" "bang!" """#
        );
        assert!(!text.lines().any(|l| l.trim() == "INJECTED"));
    }

    #[test]
    fn unknown_and_custom_agents() {
        assert!(matches!(
            Agent::resolve("vim", &Config::default()),
            Err(AgentError::Unknown(_))
        ));
        let mut cfg = Config::default();
        cfg.agents.custom.push(blirp_core::config::CustomAgent {
            name: "mine".into(),
            command: "definitely-not-on-path-xyz".into(),
            args: vec!["--fast".into()],
        });
        let a = Agent::resolve("custom:mine", &cfg).unwrap();
        assert!(a.path.is_none());
        let err = a
            .command(&LaunchContext {
                agent_session_id: None,
                resume: false,
                args_before: &[],
                args_after: &[],
            })
            .unwrap_err();
        assert!(matches!(err, AgentError::NotInstalled(_)));
        assert!(Agent::all(&cfg).iter().any(|a| a.id == "custom:mine"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_shims_are_wrapped() {
        let c = wrap_for_platform(Path::new(r"C:\npm\codex.cmd"), vec!["resume".into()]).unwrap();
        assert!(
            c.program
                .to_string_lossy()
                .to_lowercase()
                .ends_with("cmd.exe")
        );
        assert_eq!(c.args, ["/d", "/v:off", "/c", "%BLIRP_CMD_LINE%"]);
        assert_eq!(
            c.env,
            [(
                CMD_LINE_ENV.to_string(),
                r#""C:\npm\codex.cmd" resume"#.to_string()
            )]
        );
        let c = wrap_for_platform(Path::new(r"C:\npm\x.ps1"), vec![]).unwrap();
        assert!(c.args.iter().any(|a| a == "-File"));
        let c = wrap_for_platform(Path::new(r"C:\bin\claude.exe"), vec![]).unwrap();
        assert_eq!(c.program, r"C:\bin\claude.exe");
        assert!(wrap_for_platform(Path::new(r"C:\npm\x.cmd"), vec!["a\nb".into()]).is_err());
    }
}
