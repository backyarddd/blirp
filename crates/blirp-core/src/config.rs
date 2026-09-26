//! `config.toml` (§12): serde with unknown keys rejected, defaults, validation.

use crate::model::{BUILTIN_AGENTS, MachineRole};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use ts_rs::TS;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot {action} config file {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config file {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// User configuration. Every section and key is optional; missing values take defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub machine: MachineConfig,
    pub agents: AgentsConfig,
    pub sessions: SessionsConfig,
    pub memory: MemoryConfig,
    pub sync: SyncConfig,
    pub portal: PortalConfig,
    pub update: UpdateConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Preferred local API port; falls back to an ephemeral port when taken.
    pub port: u16,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self { port: 47770 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(default, deny_unknown_fields)]
pub struct MachineConfig {
    pub name: String,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            name: gethostname::gethostname().to_string_lossy().into_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(default, deny_unknown_fields)]
pub struct AgentsConfig {
    /// Agent preselected in the new-session dialog: a built-in id or `custom:<name>`.
    pub default: String,
    pub custom: Vec<CustomAgent>,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            default: "claude".into(),
            custom: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CustomAgent {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SessionsConfig {
    pub worktree_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "lowercase")]
pub enum Summarizer {
    #[default]
    Auto,
    Claude,
    Codex,
    Ollama,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "lowercase")]
pub enum BriefMode {
    #[default]
    Auto,
    Review,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    pub summarizer: Summarizer,
    pub ollama_model: String,
    pub distill_idle_secs: u32,
    pub daily_distill_limit: u32,
    pub brief_mode: BriefMode,
    pub inject_max_chars: u32,
    pub distill_max_chars: u32,
    /// Inject project memory when agents start (§9). Off: sessions start
    /// without memory; MCP tools and the CLI still reach it on demand.
    pub inject: bool,
    /// Agents (`claude`, `custom:<name>`, ...) that never get memory
    /// injected, even when `inject` is on.
    pub inject_disabled_agents: Vec<String>,
}

impl MemoryConfig {
    /// Whether sessions of `agent` get memory injected at start.
    pub fn inject_enabled(&self, agent: &str) -> bool {
        self.inject && !self.inject_disabled_agents.iter().any(|a| a == agent)
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            summarizer: Summarizer::Auto,
            ollama_model: "qwen2.5:7b".into(),
            distill_idle_secs: 300,
            daily_distill_limit: 40,
            brief_mode: BriefMode::Auto,
            inject_max_chars: 8000,
            distill_max_chars: 60_000,
            inject: true,
            inject_disabled_agents: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    pub role: MachineRole,
    /// Hub node id; required when `role = "node"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub hub: Option<String>,
    /// `"default"`, `"disabled"` or a relay URL.
    pub relay: String,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            role: MachineRole::Standalone,
            hub: None,
            relay: "default".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(default, deny_unknown_fields)]
pub struct PortalConfig {
    pub lan: bool,
    pub lan_port: u16,
}

impl Default for PortalConfig {
    fn default() -> Self {
        Self {
            lan: false,
            lan_port: 47771,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    /// Ask GitHub once a day whether a newer release exists (`GET /api/update`).
    pub check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self { check: true }
    }
}

impl Config {
    /// Load `path`, writing a default file first if it does not exist.
    pub fn load_or_init(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Self::default();
                cfg.save(path)?;
                Ok(cfg)
            }
            Err(source) => Err(ConfigError::Io {
                action: "read",
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate and write atomically. Rewrites the whole file (comments are not kept).
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        let body = toml::to_string_pretty(self)
            .map_err(|e| ConfigError::Invalid(format!("cannot serialize: {e}")))?;
        let text =
            format!("# blirp configuration. Reference: docs/ARCHITECTURE.md section 12.\n\n{body}");
        let tmp = path.with_extension("toml.tmp");
        let io = |action| {
            move |source| ConfigError::Io {
                action,
                path: path.to_path_buf(),
                source,
            }
        };
        std::fs::write(&tmp, text).map_err(io("write"))?;
        std::fs::rename(&tmp, path).map_err(io("replace"))
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |m: String| Err(ConfigError::Invalid(m));
        if self.machine.name.trim().is_empty() {
            return bad("machine.name must not be empty".into());
        }
        let mut seen = std::collections::HashSet::new();
        for a in &self.agents.custom {
            let valid = !a.name.is_empty()
                && a.name.len() <= 40
                && a.name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !valid {
                return bad(format!(
                    "agents.custom name {:?} must be 1-40 chars of [A-Za-z0-9_-]",
                    a.name
                ));
            }
            if !seen.insert(a.name.as_str()) {
                return bad(format!("agents.custom name {:?} is defined twice", a.name));
            }
            if a.command.trim().is_empty() {
                return bad(format!("agents.custom {:?} has an empty command", a.name));
            }
        }
        let d = self.agents.default.as_str();
        let default_ok = BUILTIN_AGENTS.contains(&d)
            || d.strip_prefix("custom:").is_some_and(|n| seen.contains(n));
        if !default_ok {
            return bad(format!(
                "agents.default {d:?} is not a built-in agent ({}) or a defined custom:<name>",
                BUILTIN_AGENTS.join(", ")
            ));
        }
        let m = &self.memory;
        if m.distill_idle_secs == 0 {
            return bad("memory.distill_idle_secs must be > 0".into());
        }
        for a in &m.inject_disabled_agents {
            let known = BUILTIN_AGENTS.contains(&a.as_str())
                || a.strip_prefix("custom:").is_some_and(|n| seen.contains(n));
            if !known {
                return bad(format!(
                    "memory.inject_disabled_agents entry {a:?} is not a built-in agent or a defined custom:<name>"
                ));
            }
        }
        if m.inject_max_chars < 500 {
            return bad("memory.inject_max_chars must be >= 500".into());
        }
        if m.distill_max_chars < 1000 {
            return bad("memory.distill_max_chars must be >= 1000".into());
        }
        if m.summarizer == Summarizer::Ollama && m.ollama_model.trim().is_empty() {
            return bad("memory.ollama_model is required when summarizer = \"ollama\"".into());
        }
        let relay = self.sync.relay.as_str();
        if !(relay == "default"
            || relay == "disabled"
            || relay.starts_with("https://")
            || relay.starts_with("http://"))
        {
            return bad(format!(
                "sync.relay {relay:?} must be \"default\", \"disabled\" or an http(s) URL"
            ));
        }
        if self.sync.role == MachineRole::Node
            && self.sync.hub.as_deref().is_none_or(|h| h.trim().is_empty())
        {
            return bad("sync.hub is required when sync.role = \"node\"".into());
        }
        if self.portal.lan_port == 0 {
            return bad("portal.lan_port must be > 0".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Config, ConfigError> {
        Config::parse(s, Path::new("config.toml"))
    }

    #[test]
    fn empty_file_is_defaults() {
        let c = parse("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.daemon.port, 47770);
        assert_eq!(c.memory.distill_max_chars, 60_000);
        assert_eq!(c.agents.default, "claude");
    }

    #[test]
    fn full_file_parses() {
        let c = parse(
            r#"
[daemon]
port = 5000
[machine]
name = "box"
[agents]
default = "custom:mine"
[[agents.custom]]
name = "mine"
command = "my-agent"
args = ["--fast"]
[sessions]
worktree_default = true
[memory]
summarizer = "ollama"
ollama_model = "llama3"
distill_idle_secs = 60
daily_distill_limit = 5
brief_mode = "review"
inject_max_chars = 4000
distill_max_chars = 20000
inject = true
inject_disabled_agents = ["codex", "custom:mine"]
[sync]
role = "node"
hub = "abc"
relay = "https://relay.example"
[portal]
lan = true
lan_port = 9000
[update]
check = false
"#,
        )
        .unwrap();
        assert_eq!(c.daemon.port, 5000);
        assert_eq!(c.agents.custom[0].args, ["--fast"]);
        assert_eq!(c.memory.summarizer, Summarizer::Ollama);
        assert_eq!(c.memory.brief_mode, BriefMode::Review);
        assert!(c.memory.inject_enabled("claude"));
        assert!(!c.memory.inject_enabled("codex"));
        assert!(!c.memory.inject_enabled("custom:mine"));
        let off = parse("[memory]\ninject = false").unwrap();
        assert!(!off.memory.inject_enabled("claude"));
        assert_eq!(c.sync.role, MachineRole::Node);
        assert!(c.portal.lan);
        assert!(!c.update.check);
        assert!(Config::default().update.check);
    }

    #[test]
    fn unknown_keys_are_rejected_with_the_key_name() {
        let err = parse("[daemon]\nprot = 1\n").unwrap_err().to_string();
        assert!(err.contains("prot"), "{err}");
        let err = parse("[nope]\n").unwrap_err().to_string();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn validation_errors() {
        for (toml, needle) in [
            ("[agents]\ndefault = \"vim\"", "agents.default"),
            ("[agents]\ndefault = \"custom:x\"", "agents.default"),
            (
                "[[agents.custom]]\nname = \"a b\"\ncommand = \"x\"",
                "agents.custom",
            ),
            (
                "[[agents.custom]]\nname = \"a\"\ncommand = \"x\"\n[[agents.custom]]\nname = \"a\"\ncommand = \"y\"",
                "twice",
            ),
            ("[sync]\nrole = \"node\"", "sync.hub"),
            ("[sync]\nrelay = \"ftp://x\"", "sync.relay"),
            ("[memory]\ndistill_idle_secs = 0", "distill_idle_secs"),
            ("[memory]\nsummarizer = \"gpt\"", "summarizer"),
            (
                "[memory]\ninject_disabled_agents = [\"vim\"]",
                "inject_disabled_agents",
            ),
            ("[daemon]\nport = 70000", "port"),
        ] {
            let err = parse(toml).unwrap_err().to_string();
            assert!(err.contains(needle), "{toml:?} -> {err}");
        }
    }

    #[test]
    fn first_run_writes_default_file_that_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let c = Config::load_or_init(&path).unwrap();
        assert!(path.exists());
        assert_eq!(Config::load_or_init(&path).unwrap(), c);
    }
}
