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
    /// Local API port on 127.0.0.1. The daemon refuses to start when it is
    /// taken (another program could otherwise answer at the well-known
    /// address); `0` explicitly picks a free port each start.
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
    /// Keep this machine from sleeping while any session it runs is live.
    /// Unset: on for the hub role (cloud sessions), off otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub keep_awake: Option<bool>,
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
    /// Node: let requests relayed by the hub control this machine (launch,
    /// resume, stop, terminal input and other changes). Off by default:
    /// the hub and other paired machines can only read.
    pub allow_hub_control: bool,
    /// Find and announce machines on the local network over mDNS (hub, node
    /// and `blirp pair <code>` without an invite). Off: pairing needs the
    /// invite, and peers connect through relays or the addresses they know.
    pub lan_discovery: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            role: MachineRole::Standalone,
            hub: None,
            relay: "default".into(),
            allow_hub_control: false,
            lan_discovery: true,
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
    /// Whether live sessions keep this machine awake (`sessions.keep_awake`,
    /// default on for a hub).
    pub fn keep_awake(&self) -> bool {
        self.sessions
            .keep_awake
            .unwrap_or(self.sync.role == MachineRole::Hub)
    }

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

    /// Validate and write atomically. An existing file is edited in place:
    /// only keys whose value changed are rewritten, so the user's comments
    /// and layout survive UI saves.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        let body = toml::to_string_pretty(self)
            .map_err(|e| ConfigError::Invalid(format!("cannot serialize: {e}")))?;
        let io = |action| {
            move |source| ConfigError::Io {
                action,
                path: path.to_path_buf(),
                source,
            }
        };
        let existing = match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io("read")(e)),
        };
        // A file that no longer parses (edited by hand meanwhile) is replaced.
        let text = existing
            .and_then(|old| edit_in_place(&old, &body))
            .unwrap_or_else(|| {
                format!(
                    "# blirp configuration. Reference: docs/ARCHITECTURE.md section 12.\n\n{body}"
                )
            });
        let tmp = path.with_extension("toml.tmp");
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

/// `old` (the file on disk) changed to hold the values of `fresh` (the
/// serialized config): equal values keep their text, comments and position;
/// changed values are replaced keeping their surrounding comments; keys
/// `fresh` no longer has (unset options) are removed. None when `old` is not
/// valid TOML.
fn edit_in_place(old: &str, fresh: &str) -> Option<String> {
    let mut doc: toml_edit::DocumentMut = old.parse().ok()?;
    let fresh: toml_edit::DocumentMut = fresh.parse().ok()?;
    merge_table(doc.as_table_mut(), fresh.as_table());
    Some(doc.to_string())
}

fn merge_table(old: &mut dyn toml_edit::TableLike, new: &dyn toml_edit::TableLike) {
    use toml_edit::Item;
    let gone: Vec<String> = old
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| !new.contains_key(k))
        .collect();
    for k in &gone {
        old.remove(k);
    }
    for (key, n) in new.iter() {
        let replace = match old.get_mut(key) {
            None => true,
            Some(Item::Value(ov)) if n.is_value() => {
                if let Some(nv) = n.as_value()
                    && !same_value(ov, nv)
                {
                    let decor = ov.decor().clone();
                    *ov = nv.clone();
                    *ov.decor_mut() = decor;
                }
                false
            }
            Some(o) if o.is_table_like() && n.is_table_like() => {
                if let (Some(ot), Some(nt)) = (o.as_table_like_mut(), n.as_table_like()) {
                    merge_table(ot, nt);
                }
                false
            }
            // Arrays of tables, or a value that became a table.
            Some(o) => !same_item(o, n),
        };
        if replace {
            let mut item = n.clone();
            detach(&mut item);
            old.insert(key, item);
        }
    }
}

/// Tables copied from another document drop their position there, so they
/// are written after the table they are inserted into, a blank line apart.
fn detach(item: &mut toml_edit::Item) {
    use toml_edit::Item;
    match item {
        Item::Table(t) => {
            t.set_position(None);
            t.decor_mut().set_prefix("\n");
            for (_, v) in t.iter_mut() {
                detach(v);
            }
        }
        Item::ArrayOfTables(a) => {
            for t in a.iter_mut() {
                t.set_position(None);
                for (_, v) in t.iter_mut() {
                    detach(v);
                }
            }
        }
        Item::None | Item::Value(_) => {}
    }
}

fn same_item(a: &toml_edit::Item, b: &toml_edit::Item) -> bool {
    use toml_edit::Item;
    match (a, b) {
        (Item::Value(x), Item::Value(y)) => same_value(x, y),
        (Item::ArrayOfTables(x), Item::ArrayOfTables(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| same_table(p, q))
        }
        _ => match (a.as_table_like(), b.as_table_like()) {
            (Some(x), Some(y)) => same_table(x, y),
            _ => false,
        },
    }
}

fn same_table(a: &dyn toml_edit::TableLike, b: &dyn toml_edit::TableLike) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|(k, v)| b.get(k).is_some_and(|w| same_item(v, w)))
}

/// Equal as data, whatever the formatting (quotes, spacing, number style).
fn same_value(a: &toml_edit::Value, b: &toml_edit::Value) -> bool {
    use toml_edit::Value;
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.value() == y.value(),
        (Value::Integer(x), Value::Integer(y)) => x.value() == y.value(),
        (Value::Float(x), Value::Float(y)) => x.value() == y.value(),
        (Value::Boolean(x), Value::Boolean(y)) => x.value() == y.value(),
        (Value::Datetime(x), Value::Datetime(y)) => x.value() == y.value(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| same_value(p, q))
        }
        (Value::InlineTable(x), Value::InlineTable(y)) => same_table(x, y),
        _ => false,
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
keep_awake = false
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
lan_discovery = false
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
        assert!(!c.sync.lan_discovery);
        assert!(Config::default().sync.lan_discovery);
        assert!(c.portal.lan);
        assert!(!c.update.check);
        assert!(Config::default().update.check);
        assert!(!c.keep_awake());
    }

    #[test]
    fn keep_awake_defaults_to_the_hub_role() {
        assert!(!Config::default().keep_awake());
        let hub = parse("[sync]\nrole = \"hub\"").unwrap();
        assert!(hub.keep_awake());
        let off = parse("[sync]\nrole = \"hub\"\n[sessions]\nkeep_awake = false").unwrap();
        assert!(!off.keep_awake());
        let on = parse("[sessions]\nkeep_awake = true").unwrap();
        assert!(on.keep_awake());
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
    fn save_keeps_comments_and_layout_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = r#"# my blirp setup
[machine]
name = 'box' # short name

# memory tuning
[memory]
summarizer = "ollama"   # local only
ollama_model = "llama3"
inject_disabled_agents = [ "codex" ] # noisy

[sessions]
keep_awake = true # laptop on a dock

[sync]
relay = "disabled"
"#;
        std::fs::write(&path, original).unwrap();
        let mut c = Config::load_or_init(&path).unwrap();

        // Saving unchanged values leaves every comment and spelling alone.
        c.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        for kept in [
            "# my blirp setup",
            "name = 'box' # short name",
            "# memory tuning",
            "summarizer = \"ollama\"   # local only",
            "inject_disabled_agents = [ \"codex\" ] # noisy",
            "keep_awake = true # laptop on a dock",
        ] {
            assert!(text.contains(kept), "{kept:?} lost:\n{text}");
        }
        assert_eq!(Config::load_or_init(&path).unwrap(), c);

        // A changed value keeps its comment; an unset option disappears;
        // new keys and sections are added.
        c.machine.name = "desk".into();
        c.memory.inject_disabled_agents = vec!["codex".into(), "gemini".into()];
        c.sessions.keep_awake = None;
        c.sync.role = MachineRole::Node;
        c.sync.hub = Some("abc".into());
        c.agents.custom.push(CustomAgent {
            name: "mine".into(),
            command: "my-agent".into(),
            args: vec!["--fast".into()],
        });
        c.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("name = \"desk\" # short name"), "{text}");
        assert!(text.contains("# noisy") && text.contains("# memory tuning"));
        assert!(
            !text.contains("keep_awake") && !text.contains("# laptop"),
            "{text}"
        );
        assert_eq!(Config::load_or_init(&path).unwrap(), c);

        // A file that does not parse is replaced by a fresh one.
        std::fs::write(&path, "[machine\nname = ").unwrap();
        c.save(&path).unwrap();
        assert_eq!(Config::load_or_init(&path).unwrap(), c);
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
