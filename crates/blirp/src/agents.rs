//! Agent specs (§7): binaries, launch/resume argv, detection.

use blirp_core::config::Config;
use blirp_core::model::{AgentInfo, AgentIntegration, InjectMode, IntegrationState};
use blirp_core::process;
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

    /// Program and argv for a launch or resume.
    pub fn command(
        &self,
        ctx: &LaunchContext<'_>,
    ) -> Result<(OsString, Vec<OsString>), AgentError> {
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
        Ok(wrap_for_platform(path, args))
    }
}

fn is_powershell(p: &Path) -> bool {
    p.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("pwsh") || s.eq_ignore_ascii_case("powershell"))
}

/// The node program and script behind an npm-generated `.cmd` shim
/// (`"%_prog%" "%dp0%\node_modules\...\bin\x.js" %*`), when both exist.
fn npm_shim_target(path: &Path) -> Option<(PathBuf, PathBuf)> {
    const MARK: &str = "\"%dp0%\\";
    let text = std::fs::read_to_string(path).ok()?;
    if text.len() > 16 * 1024 {
        return None;
    }
    let start = text.find(MARK)? + MARK.len();
    let len = text[start..].find('"')?;
    let rel = &text[start..start + len];
    if !text[start + len + 1..].trim_start().starts_with("%*") {
        return None;
    }
    let dir = path.parent()?;
    let script = dir.join(rel);
    let node = Some(dir.join("node.exe"))
        .filter(|p| p.is_file())
        .or_else(|| process::which("node"))?;
    script.is_file().then_some((node, script))
}

/// On Windows, npm `.cmd` shims run their node script directly (so arguments
/// never pass through `cmd.exe` quoting), other `.cmd`/`.bat` files through
/// `cmd /d /c` and `.ps1` shims through PowerShell; everything else is
/// executed directly.
pub fn wrap_for_platform(path: &Path, args: Vec<OsString>) -> (OsString, Vec<OsString>) {
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
        return (node.into_os_string(), a);
    }
    match ext.as_deref() {
        Some("cmd" | "bat") if cfg!(windows) => {
            let cmd = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
            let mut a: Vec<OsString> = vec!["/d".into(), "/c".into(), path.into()];
            a.extend(args);
            (cmd, a)
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
            (ps, a)
        }
        _ => (path.as_os_str().to_owned(), args),
    }
}

/// First line of `<agent> --version`, `None` on failure or timeout.
pub fn probe_version(path: &Path) -> Option<String> {
    let (program, args) = wrap_for_platform(path, vec!["--version".into()]);
    let mut cmd = process::command(program);
    cmd.args(args);
    let out = process::run(cmd, Duration::from_secs(5), 64 * 1024).ok()?;
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

/// Detect every agent with its version, probing in parallel.
pub fn detect_all(config: &Config) -> Vec<AgentInfo> {
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
                    AgentInfo {
                        id: a.id.clone(),
                        display_name: a.display_name.clone(),
                        builtin: a.builtin,
                        installed: a.path.is_some(),
                        path: a.path.as_ref().map(|p| p.display().to_string()),
                        version,
                        can_resume: a.can_resume(),
                        integration: integration(&a.id),
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
                })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, path: &str) -> Agent {
        let mut a = Agent::resolve(id, &Config::default()).unwrap();
        a.path = Some(PathBuf::from(path));
        a
    }

    fn argv(a: &Agent, resume: bool, sid: Option<&str>) -> Vec<String> {
        let (_, args) = a
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
        let (_, args) = codex
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
        std::fs::write(
            &shim,
            r#"@ECHO off
endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & "%_prog%"  "%dp0%\node_modules\@openai\codex\bin\codex.js" %*
"#,
        )
        .unwrap();
        let (prog, args) = wrap_for_platform(&shim, vec!["a b\"c".into()]);
        assert_eq!(prog, dir.path().join("node.exe").into_os_string());
        assert_eq!(args[0], script.join("codex.js").into_os_string());
        assert_eq!(args[1], "a b\"c");
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
        let (prog, args) = wrap_for_platform(Path::new(r"C:\npm\codex.cmd"), vec!["resume".into()]);
        assert!(prog.to_string_lossy().to_lowercase().ends_with("cmd.exe"));
        assert_eq!(args, ["/d", "/c", r"C:\npm\codex.cmd", "resume"]);
        let (_, args) = wrap_for_platform(Path::new(r"C:\npm\x.ps1"), vec![]);
        assert!(args.iter().any(|a| a == "-File"));
        let (prog, _) = wrap_for_platform(Path::new(r"C:\bin\claude.exe"), vec![]);
        assert_eq!(prog, r"C:\bin\claude.exe");
    }
}
