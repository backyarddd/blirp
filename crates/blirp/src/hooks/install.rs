//! Opt-in global integration (`blirp hooks install|uninstall|status`, §9):
//! blirp hook entries and the blirp MCP server in the agents' user configs,
//! for sessions started outside blirp.
//!
//! Edits are idempotent and marker-identified (hook commands end in
//! `hook <agent> <event> --global`, the MCP server is named `blirp` and runs
//! `blirp mcp`), preserve every foreign entry, back the original file up once
//! to `<file>.blirp-backup` and replace files atomically.

use crate::memory::hook_command;
use crate::memory::launch::{CLAUDE_EVENTS, GEMINI_EVENTS};
use anyhow::{Context as _, bail};
use blirp_core::model::IntegrationState;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Agents with a global install; others report `unsupported`.
pub const SUPPORTED: &[&str] = &["claude", "codex", "gemini", "cursor", "opencode"];

const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "PreCompact",
    "SessionEnd",
];
const CURSOR_EVENTS: &[&str] = &[
    "sessionStart",
    "beforeSubmitPrompt",
    "stop",
    "sessionEnd",
    "preCompact",
];
const CODEX_TRUST: &str =
    "Codex asks you to trust new hooks: run /hooks in Codex once after installing.";

static OURS: LazyLock<Regex> = LazyLock::new(|| {
    // Infallible: a literal, tested pattern.
    #[allow(clippy::expect_used)]
    Regex::new(r#"(?i)blirp(?:\.exe)?"?\s+hook\s+\S+\s+\S+\s+--global\s*$"#).expect("valid regex")
});

/// Is `command` a hook entry written by `blirp hooks install`?
pub fn is_ours(command: &str) -> bool {
    OURS.is_match(command)
}

/// Where each agent keeps its user config. Tests point this at temp dirs.
#[derive(Debug, Clone)]
pub struct Homes {
    pub claude_dir: PathBuf,
    pub claude_json: PathBuf,
    pub codex_home: PathBuf,
    pub gemini_dir: PathBuf,
    pub cursor_dir: PathBuf,
    pub opencode_dir: PathBuf,
}

impl Homes {
    /// Defaults under `home` without env overrides.
    pub fn under(home: &Path) -> Self {
        Self {
            claude_dir: home.join(".claude"),
            claude_json: home.join(".claude.json"),
            codex_home: home.join(".codex"),
            gemini_dir: home.join(".gemini"),
            cursor_dir: home.join(".cursor"),
            opencode_dir: home.join(".config").join("opencode"),
        }
    }

    /// The real locations, honoring `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `XDG_CONFIG_HOME`.
    pub fn from_env() -> Option<Self> {
        let home = blirp_core::paths::user_home()?;
        let mut h = Self::under(&home);
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        if let Some(d) = var("CLAUDE_CONFIG_DIR") {
            h.claude_json = d.join(".claude.json");
            h.claude_dir = d;
        }
        if let Some(d) = var("CODEX_HOME") {
            h.codex_home = d;
        }
        if let Some(d) = var("XDG_CONFIG_HOME") {
            h.opencode_dir = d.join("opencode");
        }
        Some(h)
    }

    fn opencode_file(&self) -> PathBuf {
        let json = self.opencode_dir.join("opencode.json");
        let jsonc = self.opencode_dir.join("opencode.jsonc");
        if !json.exists() && jsonc.exists() {
            jsonc
        } else {
            json
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub hooks: IntegrationState,
    pub mcp: IntegrationState,
    pub detail: Option<String>,
}

impl Status {
    fn unsupported() -> Self {
        Self {
            hooks: IntegrationState::Unsupported,
            mcp: IntegrationState::Unsupported,
            detail: None,
        }
    }
}

// ---------------------------------------------------------------- files

/// Remove `//` and `/* */` comments and trailing commas outside strings.
pub fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let (mut in_str, mut esc) = (false, false);
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            match (esc, c) {
                (true, _) => esc = false,
                (false, '\\') => esc = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_str = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            (',', _) => {
                // Drop the comma when the next significant char closes a container.
                let rest: String = chars.clone().collect();
                let next = rest.trim_start().chars().next();
                if !matches!(next, Some('}') | Some(']')) {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

struct JsonFile {
    path: PathBuf,
    root: Map<String, Value>,
    original: Option<String>,
}

impl JsonFile {
    fn load(path: &Path) -> anyhow::Result<Self> {
        let original = match std::fs::read_to_string(path) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let root = match original.as_deref().map(str::trim) {
            None | Some("") => Map::new(),
            Some(t) => match serde_json::from_str::<Value>(t) {
                Ok(Value::Object(m)) => m,
                Ok(_) => bail!("{} is not a JSON object; not editing it", path.display()),
                Err(e) => {
                    if serde_json::from_str::<Value>(&strip_jsonc(t)).is_ok() {
                        bail!(
                            "{} contains comments; blirp does not rewrite commented files, add the entries by hand",
                            path.display()
                        );
                    }
                    bail!("{} is not valid JSON ({e}); not editing it", path.display())
                }
            },
        };
        Ok(Self {
            path: path.to_path_buf(),
            root,
            original,
        })
    }

    fn save(&self) -> anyhow::Result<()> {
        let mut text = serde_json::to_string_pretty(&Value::Object(self.root.clone()))?;
        text.push('\n');
        if self.original.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        write_atomic(&self.path, &text, self.original.is_some())
    }
}

/// Back the original up once, then replace `path` via a temp file + rename.
fn write_atomic(path: &Path, text: &str, existed: bool) -> anyhow::Result<()> {
    let dir = path.parent().context("config path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    if existed {
        let backup = PathBuf::from(format!("{}.blirp-backup", path.display()));
        if !backup.exists() {
            std::fs::copy(path, &backup).with_context(|| {
                format!("backing up {} to {}", path.display(), backup.display())
            })?;
        }
    }
    let tmp = PathBuf::from(format!("{}.blirp-tmp", path.display()));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

fn obj<'a>(m: &'a mut Map<String, Value>, key: &str) -> anyhow::Result<&'a mut Map<String, Value>> {
    let v = m.entry(key).or_insert_with(|| json!({}));
    v.as_object_mut()
        .with_context(|| format!("\"{key}\" is not a JSON object"))
}

// ---------------------------------------------------------------- hook entries

fn group_has_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(is_ours)
            })
        })
        || group
            .get("command")
            .and_then(|c| c.as_str())
            .is_some_and(is_ours)
}

/// Add `group` to `hooks[event]` unless an entry of ours is already there.
fn add_group(hooks: &mut Map<String, Value>, event: &str, group: Value) -> anyhow::Result<()> {
    let list = hooks.entry(event).or_insert_with(|| json!([]));
    let arr = list
        .as_array_mut()
        .with_context(|| format!("hooks.{event} is not a list"))?;
    if !arr.iter().any(group_has_ours) {
        arr.push(group);
    }
    Ok(())
}

/// Remove our entries from every event; groups and events left empty by that go too.
fn remove_ours(hooks: &mut Map<String, Value>) {
    for list in hooks.values_mut() {
        let Some(arr) = list.as_array_mut() else {
            continue;
        };
        arr.retain_mut(|group| {
            if group
                .get("command")
                .and_then(|c| c.as_str())
                .is_some_and(is_ours)
            {
                return false;
            }
            let Some(hs) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                return true;
            };
            let before = hs.len();
            hs.retain(|h| {
                !h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(is_ours)
            });
            !(hs.is_empty() && before > 0)
        });
    }
    hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
}

fn has_all(hooks: Option<&Value>, events: &[&str]) -> bool {
    let Some(h) = hooks.and_then(|h| h.as_object()) else {
        return false;
    };
    events.iter().all(|e| {
        h.get(*e)
            .and_then(|l| l.as_array())
            .is_some_and(|a| a.iter().any(group_has_ours))
    })
}

fn state(b: bool) -> IntegrationState {
    if b {
        IntegrationState::Installed
    } else {
        IntegrationState::NotInstalled
    }
}

fn is_our_server(v: Option<&Value>) -> bool {
    let Some(v) = v else { return false };
    let args_mcp = v.get("args") == Some(&json!(["mcp"]));
    let cmd_mcp = v
        .get("command")
        .and_then(|c| c.as_array())
        .is_some_and(|a| a.last() == Some(&json!("mcp")));
    args_mcp || cmd_mcp
}

// ---------------------------------------------------------------- per agent

fn claude_settings(h: &Homes) -> PathBuf {
    h.claude_dir.join("settings.json")
}

fn claude_event_names() -> Vec<&'static str> {
    CLAUDE_EVENTS.iter().map(|(e, _)| *e).collect()
}

fn read_value(path: &Path) -> Option<Value> {
    let t = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&strip_jsonc(&t)).ok()
}

fn codex_doc(h: &Homes) -> anyhow::Result<(PathBuf, Option<String>, toml_edit::DocumentMut)> {
    let path = h.codex_home.join("config.toml");
    let original = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let doc = original
        .as_deref()
        .unwrap_or("")
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("{} is not valid TOML; not editing it", path.display()))?;
    Ok((path, original, doc))
}

fn codex_has_server(doc: &toml_edit::DocumentMut) -> bool {
    doc.get("mcp_servers")
        .and_then(|t| t.get("blirp"))
        .and_then(|b| b.get("args"))
        .and_then(|a| a.as_array())
        .is_some_and(|a| a.len() == 1 && a.get(0).and_then(|v| v.as_str()) == Some("mcp"))
}

pub fn status(agent: &str, h: &Homes) -> Status {
    match agent {
        "claude" => {
            let hooks = read_value(&claude_settings(h));
            let mcp = read_value(&h.claude_json);
            Status {
                hooks: state(has_all(
                    hooks.as_ref().and_then(|v| v.get("hooks")),
                    &claude_event_names(),
                )),
                mcp: state(is_our_server(
                    mcp.as_ref().and_then(|v| v.pointer("/mcpServers/blirp")),
                )),
                detail: None,
            }
        }
        "codex" => {
            let hooks = read_value(&h.codex_home.join("hooks.json"));
            let mcp = codex_doc(h)
                .map(|(_, _, d)| codex_has_server(&d))
                .unwrap_or(false);
            Status {
                hooks: state(has_all(
                    hooks.as_ref().and_then(|v| v.get("hooks")),
                    CODEX_EVENTS,
                )),
                mcp: state(mcp),
                detail: Some(CODEX_TRUST.into()),
            }
        }
        "gemini" => {
            let v = read_value(&h.gemini_dir.join("settings.json"));
            Status {
                hooks: state(has_all(
                    v.as_ref().and_then(|v| v.get("hooks")),
                    GEMINI_EVENTS,
                )),
                mcp: state(is_our_server(
                    v.as_ref().and_then(|v| v.pointer("/mcpServers/blirp")),
                )),
                detail: None,
            }
        }
        "cursor" => {
            let hooks = read_value(&h.cursor_dir.join("hooks.json"));
            let mcp = read_value(&h.cursor_dir.join("mcp.json"));
            Status {
                hooks: state(has_all(
                    hooks.as_ref().and_then(|v| v.get("hooks")),
                    CURSOR_EVENTS,
                )),
                mcp: state(is_our_server(
                    mcp.as_ref().and_then(|v| v.pointer("/mcpServers/blirp")),
                )),
                detail: None,
            }
        }
        "opencode" => {
            let v = read_value(&h.opencode_file());
            Status {
                hooks: IntegrationState::Unsupported,
                mcp: state(is_our_server(
                    v.as_ref().and_then(|v| v.pointer("/mcp/blirp")),
                )),
                detail: Some(
                    "opencode has no shell hooks; memory is injected when launched from blirp."
                        .into(),
                ),
            }
        }
        _ => Status::unsupported(),
    }
}

fn claude_style_group(exe: &Path, agent: &str, event: &str, matcher: Option<&str>) -> Value {
    let mut g = Map::new();
    if let Some(m) = matcher {
        g.insert("matcher".into(), json!(m));
    }
    g.insert(
        "hooks".into(),
        json!([{"type": "command", "command": hook_command(exe, agent, event, true), "timeout": 10}]),
    );
    Value::Object(g)
}

/// Install (or with `remove`, uninstall) the global integration of `agent`.
fn apply(agent: &str, h: &Homes, exe: &Path, remove: bool) -> anyhow::Result<()> {
    let exe_s = exe.display().to_string();
    let server = json!({"type": "stdio", "command": exe_s, "args": ["mcp"], "env": {}});
    match agent {
        "claude" => {
            let mut f = JsonFile::load(&claude_settings(h))?;
            let hooks = obj(&mut f.root, "hooks")?;
            remove_ours(hooks);
            if !remove {
                for (ev, m) in CLAUDE_EVENTS {
                    add_group(hooks, ev, claude_style_group(exe, "claude", ev, *m))?;
                }
            }
            if hooks.is_empty() {
                f.root.remove("hooks");
            }
            f.save()?;
            mcp_json(&h.claude_json, "mcpServers", server, remove)
        }
        "codex" => {
            let mut f = JsonFile::load(&h.codex_home.join("hooks.json"))?;
            let hooks = obj(&mut f.root, "hooks")?;
            remove_ours(hooks);
            if !remove {
                for ev in CODEX_EVENTS {
                    add_group(hooks, ev, claude_style_group(exe, "codex", ev, None))?;
                }
            }
            if hooks.is_empty() {
                f.root.remove("hooks");
            }
            if f.root.is_empty() && f.original.is_none() {
                // Nothing to write and nothing was there.
            } else {
                f.save()?;
            }
            let (path, original, mut doc) = codex_doc(h)?;
            if remove {
                if codex_has_server(&doc)
                    && let Some(t) = doc
                        .get_mut("mcp_servers")
                        .and_then(|t| t.as_table_like_mut())
                {
                    t.remove("blirp");
                }
            } else {
                let servers = doc
                    .entry("mcp_servers")
                    .or_insert_with(|| {
                        let mut t = toml_edit::Table::new();
                        t.set_implicit(true);
                        toml_edit::Item::Table(t)
                    })
                    .as_table_mut()
                    .context("mcp_servers in config.toml is not a table")?;
                let mut t = toml_edit::Table::new();
                t["command"] = toml_edit::value(exe_s.clone());
                let mut args = toml_edit::Array::new();
                args.push("mcp");
                t["args"] = toml_edit::value(args);
                servers.insert("blirp", toml_edit::Item::Table(t));
            }
            let text = doc.to_string();
            if original.as_deref() != Some(text.as_str()) && !(original.is_none() && remove) {
                write_atomic(&path, &text, original.is_some())?;
            }
            Ok(())
        }
        "gemini" => {
            let mut f = JsonFile::load(&h.gemini_dir.join("settings.json"))?;
            let hooks = obj(&mut f.root, "hooks")?;
            remove_ours(hooks);
            if !remove {
                for ev in GEMINI_EVENTS {
                    add_group(
                        hooks,
                        ev,
                        json!({"matcher": "*", "hooks": [{"name": "blirp", "type": "command",
                            "command": hook_command(exe, "gemini", ev, true), "timeout": 5000}]}),
                    )?;
                }
            }
            if hooks.is_empty() {
                f.root.remove("hooks");
            }
            let servers = obj(&mut f.root, "mcpServers")?;
            if is_our_server(servers.get("blirp")) {
                servers.remove("blirp");
            }
            if !remove {
                servers.insert("blirp".into(), json!({"command": exe_s, "args": ["mcp"]}));
            }
            if servers.is_empty() {
                f.root.remove("mcpServers");
            }
            f.save()
        }
        "cursor" => {
            let mut f = JsonFile::load(&h.cursor_dir.join("hooks.json"))?;
            f.root.entry("version").or_insert(json!(1));
            let hooks = obj(&mut f.root, "hooks")?;
            remove_ours(hooks);
            if !remove {
                for ev in CURSOR_EVENTS {
                    add_group(
                        hooks,
                        ev,
                        json!({"command": hook_command(exe, "cursor", ev, true)}),
                    )?;
                }
            }
            f.save()?;
            mcp_json(
                &h.cursor_dir.join("mcp.json"),
                "mcpServers",
                json!({"command": exe_s, "args": ["mcp"]}),
                remove,
            )
        }
        "opencode" => mcp_json(
            &h.opencode_file(),
            "mcp",
            json!({"type": "local", "command": [exe_s, "mcp"], "enabled": true}),
            remove,
        ),
        other => bail!("global integration is not supported for {other}"),
    }
}

/// Set or remove `root[key].blirp` in a JSON config file.
fn mcp_json(path: &Path, key: &str, server: Value, remove: bool) -> anyhow::Result<()> {
    let mut f = JsonFile::load(path)?;
    if remove && f.original.is_none() {
        return Ok(());
    }
    let servers = obj(&mut f.root, key)?;
    if remove {
        if is_our_server(servers.get("blirp")) {
            servers.remove("blirp");
        }
        if servers.is_empty() {
            f.root.remove(key);
        }
    } else {
        servers.insert("blirp".into(), server);
    }
    f.save()
}

pub fn install(agent: &str, h: &Homes, exe: &Path) -> anyhow::Result<Status> {
    if !SUPPORTED.contains(&agent) {
        bail!("global integration is not supported for {agent}");
    }
    apply(agent, h, exe, false)?;
    Ok(status(agent, h))
}

pub fn uninstall(agent: &str, h: &Homes) -> anyhow::Result<Status> {
    if !SUPPORTED.contains(&agent) {
        bail!("global integration is not supported for {agent}");
    }
    apply(agent, h, Path::new("blirp"), true)?;
    Ok(status(agent, h))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const EXE: &str = "C:/Users/alice/.cargo/bin/blirp.exe";

    /// Sanitized `~/.claude/settings.json` shape (agent-formats.md §7).
    const CLAUDE_SETTINGS: &str = r#"{
  "hooks": {
    "SessionStart": [ { "hooks": [ { "type": "command", "command": "node /x/other-hooks/session-start.js" } ] } ],
    "PostToolUse":  [ { "hooks": [ { "type": "command", "command": "node /x/other-hooks/post-tool-use.js" } ] } ],
    "Stop":         [ { "hooks": [ { "type": "command", "command": "node /x/other-hooks/stop.js" } ] } ],
    "SessionEnd":   [ { "hooks": [ { "type": "command", "command": "node /x/other-hooks/session-end.js" } ] } ]
  },
  "statusLine": { "type": "command", "command": "node /x/other-hooks/statusline.js" }
}"#;

    /// Sanitized third-party entries (agent-formats.md §7).
    const GEMINI_SETTINGS: &str = r#"{
  "hooks": {
    "SessionStart": [ { "matcher": "*", "hooks": [
      { "name": "other-memory", "type": "command", "command": "bun /x/worker.js hook gemini-cli context", "timeout": 10000 } ] } ]
  },
  "ui": { "theme": "dark" }
}"#;

    /// Sanitized `~/.codex/config.toml` shape (agent-formats.md §7).
    const CODEX_CONFIG: &str = r#"notify = [ "C:/x/notify.exe", "turn-ended" ]

[hooks.state."someplugin@market:hooks/hooks.json:session_start:0:0"]
trusted_hash = "sha256:abc"

# my servers
[mcp_servers.other]
command = "C:/x/other.exe"
args = ["serve"]
startup_timeout_sec = 20

[projects.'C:\x']
trust_level = "trusted"
"#;

    fn homes() -> (tempfile::TempDir, Homes) {
        let d = tempfile::tempdir().unwrap();
        let h = Homes::under(d.path());
        (d, h)
    }

    fn read(p: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn marker_detection() {
        assert!(is_ours("C:/bin/blirp.exe hook claude Stop --global"));
        assert!(is_ours(
            "\"C:/Program Files/blirp/blirp.exe\" hook cursor stop --global"
        ));
        assert!(!is_ours("C:/bin/blirp.exe hook claude Stop"));
        assert!(!is_ours("node /x/other-hooks/stop.js"));
        assert!(!is_ours("bun worker.js hook gemini-cli context"));
    }

    #[test]
    fn jsonc_stripping() {
        let t = "{\n // c\n \"a\": \"http://x\", /* b */ \"b\": [1,2,],\n}";
        let v: Value = serde_json::from_str(&strip_jsonc(t)).unwrap();
        assert_eq!(v, json!({"a": "http://x", "b": [1, 2]}));
    }

    #[test]
    fn claude_install_is_idempotent_and_preserves_foreign_hooks() {
        let (_d, h) = homes();
        std::fs::create_dir_all(&h.claude_dir).unwrap();
        std::fs::write(claude_settings(&h), CLAUDE_SETTINGS).unwrap();
        std::fs::write(
            &h.claude_json,
            r#"{"numStartups": 3, "mcpServers": {"x": {"command": "y"}}}"#,
        )
        .unwrap();
        assert_eq!(status("claude", &h).hooks, IntegrationState::NotInstalled);

        let s = install("claude", &h, Path::new(EXE)).unwrap();
        assert_eq!(
            (s.hooks, s.mcp),
            (IntegrationState::Installed, IntegrationState::Installed)
        );
        let once = std::fs::read_to_string(claude_settings(&h)).unwrap();
        install("claude", &h, Path::new(EXE)).unwrap();
        assert_eq!(std::fs::read_to_string(claude_settings(&h)).unwrap(), once);

        let v = read(&claude_settings(&h));
        let start = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 2);
        assert!(
            start[0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("other-hooks")
        );
        assert_eq!(start[1]["matcher"], "startup|resume|clear|compact");
        assert_eq!(
            start[1]["hooks"][0]["command"],
            format!("{EXE} hook claude SessionStart --global")
        );
        assert_eq!(v["statusLine"]["type"], "command");
        // Key order of the user's file is kept.
        assert!(once.find("\"hooks\"").unwrap() < once.find("\"statusLine\"").unwrap());
        let cj = read(&h.claude_json);
        assert_eq!(cj["numStartups"], 3);
        assert_eq!(cj["mcpServers"]["x"]["command"], "y");
        assert_eq!(cj["mcpServers"]["blirp"]["args"], json!(["mcp"]));

        let backup = PathBuf::from(format!("{}.blirp-backup", claude_settings(&h).display()));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), CLAUDE_SETTINGS);

        let s = uninstall("claude", &h).unwrap();
        assert_eq!(
            (s.hooks, s.mcp),
            (
                IntegrationState::NotInstalled,
                IntegrationState::NotInstalled
            )
        );
        assert_eq!(
            read(&claude_settings(&h)),
            serde_json::from_str::<Value>(CLAUDE_SETTINGS).unwrap()
        );
        assert!(read(&h.claude_json)["mcpServers"].get("blirp").is_none());
        uninstall("claude", &h).unwrap();
        // The backup is taken only once.
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), CLAUDE_SETTINGS);
    }

    #[test]
    fn codex_toml_is_edited_in_place() {
        let (_d, h) = homes();
        std::fs::create_dir_all(&h.codex_home).unwrap();
        std::fs::write(h.codex_home.join("config.toml"), CODEX_CONFIG).unwrap();
        let s = install("codex", &h, Path::new(EXE)).unwrap();
        assert_eq!(
            (s.hooks, s.mcp),
            (IntegrationState::Installed, IntegrationState::Installed)
        );
        assert!(s.detail.unwrap().contains("/hooks"));
        let text = std::fs::read_to_string(h.codex_home.join("config.toml")).unwrap();
        // Foreign content, comments and formatting are untouched.
        for line in CODEX_CONFIG.lines() {
            assert!(text.contains(line), "{line} missing from {text}");
        }
        assert!(text.contains("[mcp_servers.blirp]"));
        install("codex", &h, Path::new(EXE)).unwrap();
        assert_eq!(
            std::fs::read_to_string(h.codex_home.join("config.toml")).unwrap(),
            text
        );
        let hooks = read(&h.codex_home.join("hooks.json"));
        assert_eq!(
            hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            format!("{EXE} hook codex SessionStart --global")
        );

        uninstall("codex", &h).unwrap();
        assert_eq!(
            std::fs::read_to_string(h.codex_home.join("config.toml")).unwrap(),
            CODEX_CONFIG
        );
        assert_eq!(read(&h.codex_home.join("hooks.json")), json!({}));
    }

    #[test]
    fn gemini_cursor_opencode_roundtrip() {
        let (_d, h) = homes();
        std::fs::create_dir_all(&h.gemini_dir).unwrap();
        std::fs::write(h.gemini_dir.join("settings.json"), GEMINI_SETTINGS).unwrap();
        for a in ["gemini", "cursor", "opencode"] {
            install(a, &h, Path::new(EXE)).unwrap();
            install(a, &h, Path::new(EXE)).unwrap();
            let s = status(a, &h);
            assert_eq!(s.mcp, IntegrationState::Installed, "{a}");
            if a != "opencode" {
                assert_eq!(s.hooks, IntegrationState::Installed, "{a}");
            }
        }
        let g = read(&h.gemini_dir.join("settings.json"));
        assert_eq!(g["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        assert_eq!(
            g["hooks"]["SessionStart"][0]["hooks"][0]["name"],
            "other-memory"
        );
        let c = read(&h.cursor_dir.join("hooks.json"));
        assert_eq!(c["version"], 1);
        assert_eq!(c["hooks"]["sessionStart"].as_array().unwrap().len(), 1);
        let o = read(&h.opencode_dir.join("opencode.json"));
        assert_eq!(o["mcp"]["blirp"]["command"], json!([EXE, "mcp"]));

        for a in ["gemini", "cursor", "opencode"] {
            uninstall(a, &h).unwrap();
        }
        assert_eq!(
            read(&h.gemini_dir.join("settings.json")),
            serde_json::from_str::<Value>(GEMINI_SETTINGS).unwrap()
        );
        assert_eq!(status("cursor", &h).hooks, IntegrationState::NotInstalled);
        assert_eq!(status("pi", &h), Status::unsupported());
        assert!(install("pi", &h, Path::new(EXE)).is_err());
    }

    #[test]
    fn commented_or_broken_files_are_never_rewritten() {
        let (_d, h) = homes();
        std::fs::create_dir_all(&h.claude_dir).unwrap();
        let commented = "{\n // mine\n \"hooks\": {}\n}";
        std::fs::write(claude_settings(&h), commented).unwrap();
        let err = install("claude", &h, Path::new(EXE))
            .unwrap_err()
            .to_string();
        assert!(err.contains("comments"), "{err}");
        assert_eq!(
            std::fs::read_to_string(claude_settings(&h)).unwrap(),
            commented
        );
        std::fs::write(claude_settings(&h), "{ nope").unwrap();
        assert!(install("claude", &h, Path::new(EXE)).is_err());
    }
}
