//! Per-launch agent integration (§7 step 3-4, §9 table). Writes
//! `~/.blirp/launch/<session>/` files and returns extra argv/env. Never edits
//! the user's own agent config.

use crate::memory::{hook_command, slash_path};
use blirp_core::model::{InjectMode, Session};
use serde_json::{Map, Value, json};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const MEMORY_FILE: &str = "memory.md";
pub const HANDOFF_FILE: &str = "handoff.md";
const CODEX_MAX_INSTRUCTIONS: usize = 24_000;

/// Claude events wired at launch; SessionStart covers every source.
pub const CLAUDE_EVENTS: &[(&str, Option<&str>)] = &[
    ("SessionStart", Some("startup|resume|clear|compact")),
    ("UserPromptSubmit", None),
    ("Stop", None),
    ("Notification", None),
    ("SessionEnd", None),
    ("PreCompact", None),
];

/// Gemini CLI events (docs: geminicli.com/docs/hooks).
pub const GEMINI_EVENTS: &[&str] = &[
    "SessionStart",
    "BeforeAgent",
    "AfterAgent",
    "Notification",
    "SessionEnd",
    "PreCompress",
];

/// Session-start injection used for `agent` when launched from blirp.
pub fn inject_mode(agent: &str) -> InjectMode {
    match agent {
        "claude" | "gemini" => InjectMode::Hook,
        "codex" | "opencode" | "amp" => InjectMode::Instructions,
        "aider" | "pi" => InjectMode::Flag,
        _ => InjectMode::None,
    }
}

#[derive(Debug)]
pub struct LaunchIntegration {
    /// Placed before the agent's own launch/resume args (codex `-c` overrides
    /// must precede a `resume` subcommand).
    pub args_before: Vec<OsString>,
    pub args_after: Vec<OsString>,
    pub env: Vec<(String, String)>,
    pub memory_file: PathBuf,
    pub inject: InjectMode,
}

impl Default for LaunchIntegration {
    fn default() -> Self {
        Self {
            args_before: Vec::new(),
            args_after: Vec::new(),
            env: Vec::new(),
            memory_file: PathBuf::new(),
            inject: InjectMode::None,
        }
    }
}

/// Everything `prepare` needs besides the rendered memory.
pub struct LaunchInput<'a> {
    pub launch_dir: &'a Path,
    pub blirp_home: &'a Path,
    pub exe: &'a Path,
    pub session: &'a Session,
    /// Rendered injection (plus handoff pack, if any).
    pub memory: &'a str,
    pub handoff: Option<&'a str>,
    /// Environment the agent would otherwise see (for merging existing overrides).
    pub env_lookup: &'a dyn Fn(&str) -> Option<String>,
    pub user_home: Option<&'a Path>,
}

fn write(path: &Path, body: &str) -> anyhow::Result<()> {
    std::fs::write(path, body).map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))
}

fn write_json(path: &Path, v: &Value) -> anyhow::Result<()> {
    write(path, &serde_json::to_string_pretty(v)?)
}

fn mcp_env(inp: &LaunchInput<'_>) -> Map<String, Value> {
    let mut env = Map::new();
    env.insert(
        "BLIRP_HOME".into(),
        json!(inp.blirp_home.display().to_string()),
    );
    env.insert("BLIRP_PROJECT_ID".into(), json!(inp.session.project_id));
    env.insert("BLIRP_SESSION_ID".into(), json!(inp.session.id));
    env
}

fn toml_str(s: &str) -> String {
    toml_edit::Value::from(s).to_string().trim().to_string()
}

/// Append `add` to a JSON array at `obj[key]`, creating it when missing.
fn push_array(obj: &mut Map<String, Value>, key: &str, add: Value) {
    match obj.get_mut(key) {
        Some(Value::Array(a)) => a.push(add),
        _ => {
            obj.insert(key.into(), Value::Array(vec![add]));
        }
    }
}

fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Read a JSON (or JSONC) object file; `Ok(None)` when absent.
fn read_json_object(path: &Path) -> anyhow::Result<Option<Map<String, Value>>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let v: Value = serde_json::from_str(&crate::hooks::install::strip_jsonc(&text))
                .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", path.display()))?;
            match v {
                Value::Object(m) => Ok(Some(m)),
                _ => anyhow::bail!("{} is not a JSON object", path.display()),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    }
}

/// Write the launch files for `agent` and return the extra argv/env.
pub fn prepare(agent: &str, inp: &LaunchInput<'_>) -> anyhow::Result<LaunchIntegration> {
    std::fs::create_dir_all(inp.launch_dir)?;
    let memory_file = inp.launch_dir.join(MEMORY_FILE);
    write(&memory_file, inp.memory)?;
    if let Some(h) = inp.handoff {
        write(&inp.launch_dir.join(HANDOFF_FILE), h)?;
    }
    let mem = slash_path(&memory_file);
    let exe = inp.exe.display().to_string();
    let mut out = LaunchIntegration {
        memory_file: memory_file.clone(),
        inject: inject_mode(agent),
        ..Default::default()
    };
    match agent {
        "claude" => {
            let mut hooks = Map::new();
            for (event, matcher) in CLAUDE_EVENTS {
                let mut group = Map::new();
                if let Some(m) = matcher {
                    group.insert("matcher".into(), json!(m));
                }
                group.insert(
                    "hooks".into(),
                    json!([{"type": "command", "command": hook_command(inp.exe, "claude", event, false), "timeout": 10}]),
                );
                hooks.insert((*event).into(), json!([group]));
            }
            let settings = inp.launch_dir.join("settings.json");
            write_json(&settings, &json!({ "hooks": hooks }))?;
            let mcp = inp.launch_dir.join("mcp.json");
            write_json(
                &mcp,
                &json!({"mcpServers": {"blirp": {"type": "stdio", "command": exe, "args": ["mcp"], "env": mcp_env(inp)}}}),
            )?;
            out.args_after = vec![
                "--settings".into(),
                settings.into_os_string(),
                "--mcp-config".into(),
                mcp.into_os_string(),
            ];
        }
        "codex" => {
            // Codex 0.153 has SessionStart hooks, but non-managed hooks need a
            // persisted trust decision, so launch-time context goes through
            // `developer_instructions` (verified with `codex debug prompt-input`).
            let codex_home = (inp.env_lookup)("CODEX_HOME")
                .map(PathBuf::from)
                .or_else(|| inp.user_home.map(|h| h.join(".codex")));
            let existing = codex_home
                .and_then(|h| std::fs::read_to_string(h.join("config.toml")).ok())
                .and_then(|t| t.parse::<toml::Table>().ok())
                .and_then(|t| {
                    t.get("developer_instructions")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                });
            let dev = match existing {
                Some(e) if !e.trim().is_empty() => format!("{e}\n\n{}", inp.memory),
                _ => inp.memory.to_string(),
            };
            // Stays well inside the 32 767-char Windows command line.
            let dev = crate::memory::render::clip(&dev, CODEX_MAX_INSTRUCTIONS);
            let env: Vec<String> = mcp_env(inp)
                .iter()
                .map(|(k, v)| format!("{k} = {}", toml_str(v.as_str().unwrap_or_default())))
                .collect();
            for kv in [
                format!("developer_instructions={}", toml_str(&dev)),
                format!("mcp_servers.blirp.command={}", toml_str(&exe)),
                "mcp_servers.blirp.args=[\"mcp\"]".to_string(),
                format!("mcp_servers.blirp.env={{ {} }}", env.join(", ")),
            ] {
                out.args_before.push("-c".into());
                out.args_before.push(kv.into());
            }
        }
        "opencode" => {
            let mut cfg = (inp.env_lookup)("OPENCODE_CONFIG_CONTENT")
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .map(object)
                .unwrap_or_default();
            push_array(&mut cfg, "instructions", json!(mem));
            let mcp = cfg.entry("mcp").or_insert_with(|| json!({}));
            if let Value::Object(m) = mcp {
                m.insert(
                    "blirp".into(),
                    json!({"type": "local", "command": [exe, "mcp"], "environment": mcp_env(inp), "enabled": true}),
                );
            }
            out.env.push((
                "OPENCODE_CONFIG_CONTENT".into(),
                Value::Object(cfg).to_string(),
            ));
        }
        "aider" => out.args_after = vec!["--read".into(), memory_file.into_os_string()],
        "pi" => {
            out.args_after = vec![
                "--append-system-prompt".into(),
                memory_file.into_os_string(),
            ]
        }
        "gemini" => {
            // System defaults are the lowest settings layer, so the user's own
            // settings still win; an existing defaults file is carried over.
            let default_path = if cfg!(windows) {
                PathBuf::from(r"C:\ProgramData\gemini-cli\system-defaults.json")
            } else if cfg!(target_os = "macos") {
                PathBuf::from("/Library/Application Support/GeminiCli/system-defaults.json")
            } else {
                PathBuf::from("/etc/gemini-cli/system-defaults.json")
            };
            let base_path = (inp.env_lookup)("GEMINI_CLI_SYSTEM_DEFAULTS_PATH")
                .map(PathBuf::from)
                .unwrap_or(default_path);
            let mut cfg = read_json_object(&base_path)
                .ok()
                .flatten()
                .unwrap_or_default();
            let hooks = cfg.entry("hooks").or_insert_with(|| json!({}));
            if let Value::Object(h) = hooks {
                for ev in GEMINI_EVENTS {
                    push_array(
                        h,
                        ev,
                        json!({"matcher": "*", "hooks": [{"name": "blirp", "type": "command",
                            "command": hook_command(inp.exe, "gemini", ev, false), "timeout": 5000}]}),
                    );
                }
            }
            let servers = cfg.entry("mcpServers").or_insert_with(|| json!({}));
            if let Value::Object(s) = servers {
                s.insert(
                    "blirp".into(),
                    json!({"command": exe, "args": ["mcp"], "env": mcp_env(inp)}),
                );
            }
            let path = inp.launch_dir.join("gemini-system-defaults.json");
            write_json(&path, &Value::Object(cfg))?;
            out.env.push((
                "GEMINI_CLI_SYSTEM_DEFAULTS_PATH".into(),
                path.display().to_string(),
            ));
        }
        "amp" => {
            // `--settings-file` replaces the user settings file, so start from a copy of it.
            let user = inp.user_home.map(|h| h.join(".config").join("amp"));
            let base = user
                .as_ref()
                .map(|d| {
                    read_json_object(&d.join("settings.json")).and_then(|v| match v {
                        Some(v) => Ok(Some(v)),
                        None => read_json_object(&d.join("settings.jsonc")),
                    })
                })
                .transpose()
                .map(Option::flatten);
            match base {
                Ok(base) => {
                    let mut cfg = base.unwrap_or_default();
                    let prompt = match cfg.get("amp.systemPrompt").and_then(|v| v.as_str()) {
                        Some(p) if !p.trim().is_empty() => format!("{p}\n\n{}", inp.memory),
                        _ => inp.memory.to_string(),
                    };
                    cfg.insert("amp.systemPrompt".into(), json!(prompt));
                    let servers = cfg.entry("amp.mcpServers").or_insert_with(|| json!({}));
                    if let Value::Object(s) = servers {
                        s.insert(
                            "blirp".into(),
                            json!({"command": exe, "args": ["mcp"], "env": mcp_env(inp)}),
                        );
                    }
                    let path = inp.launch_dir.join("amp-settings.json");
                    write_json(&path, &Value::Object(cfg))?;
                    out.args_before = vec!["--settings-file".into(), path.into_os_string()];
                }
                Err(e) => {
                    tracing::warn!(error = %e, "amp settings unreadable; launching without memory injection");
                    out.inject = InjectMode::None;
                }
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Initial prompt for a continued session, given how memory reaches the agent.
pub fn continue_prompt(inject: InjectMode, handoff_file: &Path) -> String {
    match inject {
        InjectMode::None => format!(
            "Read the blirp handoff in {} and continue the work described there.",
            slash_path(handoff_file)
        ),
        _ => "Continue the work described in the blirp handoff above.".into(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::memory::testutil::session;

    fn run(
        agent: &str,
        env: &[(&str, &str)],
        home: &Path,
    ) -> (tempfile::TempDir, LaunchIntegration) {
        let dir = tempfile::tempdir().unwrap();
        let s = session("sess", "proj", 1);
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let lookup = move |k: &str| env.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        let out = prepare(
            agent,
            &LaunchInput {
                launch_dir: dir.path(),
                blirp_home: Path::new("/bh"),
                exe: Path::new("/opt/blirp"),
                session: &s,
                memory: "# blirp memory: P\nline \"two\"",
                handoff: None,
                env_lookup: &lookup,
                user_home: Some(home),
            },
        )
        .unwrap();
        (dir, out)
    }

    fn strs(v: &[OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn claude_gets_settings_hooks_and_mcp() {
        let home = tempfile::tempdir().unwrap();
        let (dir, out) = run("claude", &[], home.path());
        let args = strs(&out.args_after);
        assert_eq!(args[0], "--settings");
        assert_eq!(args[2], "--mcp-config");
        let s: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        for (ev, _) in CLAUDE_EVENTS {
            let cmd = s["hooks"][ev][0]["hooks"][0]["command"].as_str().unwrap();
            assert_eq!(cmd, format!("/opt/blirp hook claude {ev}"));
        }
        assert_eq!(
            s["hooks"]["SessionStart"][0]["matcher"],
            "startup|resume|clear|compact"
        );
        let m: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("mcp.json")).unwrap())
                .unwrap();
        assert_eq!(m["mcpServers"]["blirp"]["args"], json!(["mcp"]));
        assert_eq!(m["mcpServers"]["blirp"]["env"]["BLIRP_SESSION_ID"], "sess");
        assert!(dir.path().join(MEMORY_FILE).is_file());
    }

    #[test]
    fn codex_overrides_parse_as_toml_and_keep_user_instructions() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(
            home.path().join(".codex/config.toml"),
            "developer_instructions = \"be terse\"\n",
        )
        .unwrap();
        let (_d, out) = run("codex", &[], home.path());
        let args = strs(&out.args_before);
        assert_eq!(args.iter().filter(|a| *a == "-c").count(), 4);
        for kv in args.iter().filter(|a| *a != "-c") {
            let t: toml::Table = kv.parse().unwrap_or_else(|e| panic!("{kv}: {e}"));
            if let Some(d) = t.get("developer_instructions") {
                let d = d.as_str().unwrap();
                assert!(d.starts_with("be terse\n\n# blirp memory: P"), "{d}");
                assert!(d.ends_with("line \"two\""));
            }
        }
        assert!(args.iter().any(|a| a == "mcp_servers.blirp.args=[\"mcp\"]"));
    }

    #[test]
    fn opencode_merges_existing_inline_config() {
        let home = tempfile::tempdir().unwrap();
        let (_d, out) = run(
            "opencode",
            &[(
                "OPENCODE_CONFIG_CONTENT",
                r#"{"instructions":["mine.md"],"model":"x"}"#,
            )],
            home.path(),
        );
        let (k, v) = &out.env[0];
        assert_eq!(k, "OPENCODE_CONFIG_CONTENT");
        let v: Value = serde_json::from_str(v).unwrap();
        assert_eq!(v["model"], "x");
        assert_eq!(v["instructions"][0], "mine.md");
        assert!(
            v["instructions"][1]
                .as_str()
                .unwrap()
                .ends_with("/memory.md")
        );
        assert_eq!(v["mcp"]["blirp"]["command"], json!(["/opt/blirp", "mcp"]));
    }

    #[test]
    fn flag_agents_and_env_only_agents() {
        let home = tempfile::tempdir().unwrap();
        let (_d, aider) = run("aider", &[], home.path());
        assert_eq!(strs(&aider.args_after)[0], "--read");
        let (_d, pi) = run("pi", &[], home.path());
        assert_eq!(strs(&pi.args_after)[0], "--append-system-prompt");
        for a in ["cursor", "dsh", "shell", "custom:x"] {
            let (_d, o) = run(a, &[], home.path());
            assert!(o.args_before.is_empty() && o.args_after.is_empty() && o.env.is_empty());
            assert_eq!(o.inject, InjectMode::None);
        }
    }

    #[test]
    fn amp_copies_user_settings() {
        let home = tempfile::tempdir().unwrap();
        let d = home.path().join(".config/amp");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("settings.json"),
            "{\n // comment\n \"amp.theme\": \"dark\",\n}",
        )
        .unwrap();
        let (dir, out) = run("amp", &[], home.path());
        assert_eq!(strs(&out.args_before)[0], "--settings-file");
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("amp-settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["amp.theme"], "dark");
        assert!(
            v["amp.systemPrompt"]
                .as_str()
                .unwrap()
                .starts_with("# blirp memory")
        );
        assert_eq!(v["amp.mcpServers"]["blirp"]["args"], json!(["mcp"]));
    }

    #[test]
    fn gemini_defaults_overlay() {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().join("defaults.json");
        std::fs::write(&base, r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"x"}]}]},"ui":{"theme":"a"}}"#).unwrap();
        let (dir, out) = run(
            "gemini",
            &[("GEMINI_CLI_SYSTEM_DEFAULTS_PATH", base.to_str().unwrap())],
            home.path(),
        );
        assert_eq!(out.env[0].0, "GEMINI_CLI_SYSTEM_DEFAULTS_PATH");
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("gemini-system-defaults.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["ui"]["theme"], "a");
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        assert_eq!(v["mcpServers"]["blirp"]["command"], "/opt/blirp");
    }
}
