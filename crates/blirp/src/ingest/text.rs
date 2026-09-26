//! Text normalization shared by adapters: previews, truncation, timestamps,
//! tool-call rendering and file-edit detection.

use serde_json::Value;

/// Cap for tool results in `events.text`.
pub const TOOL_RESULT_MAX: usize = 4 * 1024;
/// Cap for other event text (pasted logs can be megabytes).
pub const TEXT_MAX: usize = 64 * 1024;
/// Cap for serialized `events.meta_json`.
pub const META_MAX: usize = 16 * 1024;
/// Longest title derived from a prompt, in chars.
pub const TITLE_MAX: usize = 80;

/// Largest prefix of `s` that is at most `max` bytes and ends on a char boundary.
pub fn prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `s` cut to `max` bytes with a marker saying how much was dropped.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head = prefix(s, max);
    format!("{head}\n…[truncated {} bytes]", s.len() - head.len())
}

/// Whitespace collapsed to single spaces, cut to `max` chars with an ellipsis.
pub fn one_line(s: &str, max: usize) -> String {
    let mut out = String::new();
    for w in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    if out.chars().count() > max {
        let cut: String = out.chars().take(max.saturating_sub(1)).collect();
        return format!("{}…", cut.trim_end());
    }
    out
}

/// Title candidate from a user prompt: the first non-empty line, 80 chars.
/// Prompts that are agent plumbing (XML-ish wrappers, caveats) give none.
pub fn title_from_prompt(prompt: &str) -> Option<String> {
    let t = prompt.trim_start();
    if t.is_empty()
        || t.starts_with('<')
        || t.starts_with("Caveat:")
        || (t.starts_with('#') && t.contains("AGENTS.md"))
    {
        return None;
    }
    let line = t.lines().map(str::trim).find(|l| !l.is_empty())?;
    let title = one_line(line, TITLE_MAX);
    (!title.is_empty()).then_some(title)
}

/// Unix ms from an RFC 3339 string or a numeric epoch (s or ms).
pub fn parse_ts(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => parse_ts_str(s),
        Value::Number(n) => {
            let f = n.as_f64()?;
            if !f.is_finite() || f <= 0.0 {
                return None;
            }
            // Seconds until 2286; anything larger is already milliseconds.
            Some(if f < 1e10 {
                (f * 1000.0) as i64
            } else {
                f as i64
            })
        }
        _ => None,
    }
}

pub fn parse_ts_str(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| d.timestamp_millis())
}

/// Plain text of a message `content`: a string, or the text of text-like
/// blocks (`text`, `input_text`, `output_text`) joined by newlines.
pub fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(block_text).collect();
            parts.join("\n")
        }
        Value::Object(_) => block_text(v).unwrap_or_default(),
        _ => String::new(),
    }
}

fn block_text(b: &Value) -> Option<String> {
    match b {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => {
            let ty = o.get("type").and_then(Value::as_str).unwrap_or("text");
            if matches!(ty, "text" | "input_text" | "output_text") {
                o.get("text").and_then(Value::as_str).map(str::to_string)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Arguments that best describe a call, in preference order.
const PREVIEW_KEYS: &[&str] = &[
    "command",
    "cmd",
    "file_path",
    "filePath",
    "path",
    "notebook_path",
    "target_file",
    "pattern",
    "query",
    "url",
    "description",
    "prompt",
];

/// Compact one-line preview of tool arguments.
pub fn args_preview(args: &Value) -> String {
    let parsed;
    let args = match args {
        // Some agents store arguments as a JSON string.
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(v @ Value::Object(_)) => {
                parsed = v;
                &parsed
            }
            _ => return one_line(s, 160),
        },
        a => a,
    };
    if let Value::Object(o) = args {
        for k in PREVIEW_KEYS {
            match o.get(*k) {
                Some(Value::String(s)) if !s.is_empty() => return one_line(s, 160),
                Some(Value::Array(a)) if !a.is_empty() => {
                    let parts: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
                    if !parts.is_empty() {
                        return one_line(&parts.join(" "), 160);
                    }
                }
                _ => {}
            }
        }
        if o.is_empty() {
            return String::new();
        }
    }
    one_line(&args.to_string(), 160)
}

/// `tool(args preview)`
pub fn tool_call_text(name: &str, args: &Value) -> String {
    format!("{name}({})", args_preview(args))
}

/// Tools whose path argument is a file they write.
fn is_edit_tool(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let n = n.rsplit(['.', ':', '/']).next().unwrap_or(&n);
    matches!(
        n,
        "edit"
            | "multiedit"
            | "multi_edit"
            | "write"
            | "notebookedit"
            | "edit_file"
            | "write_file"
            | "create_file"
            | "replace"
            | "search_replace"
            | "str_replace_editor"
            | "str_replace_based_edit_tool"
            | "apply_patch"
            | "patch"
            | "delete_file"
            | "edit_notebook"
    )
}

const PATH_KEYS: &[&str] = &[
    "file_path",
    "filePath",
    "path",
    "notebook_path",
    "target_file",
    "filename",
    "file",
];

/// Files an edit/write tool call changes: its path argument for known edit
/// tools, plus the file headers of any apply_patch payload in its arguments.
pub fn edited_paths(tool: &str, args: &Value) -> Vec<String> {
    let parsed;
    let args = match args {
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(v) => {
                parsed = v;
                &parsed
            }
            Err(_) => return patch_paths(s),
        },
        a => a,
    };
    let mut out = Vec::new();
    if is_edit_tool(tool)
        && let Value::Object(o) = args
    {
        for k in PATH_KEYS {
            if let Some(Value::String(p)) = o.get(*k)
                && !p.is_empty()
            {
                out.push(p.clone());
                break;
            }
        }
        // MultiEdit-style batches carry one path per edit.
        if let Some(Value::Array(edits)) = o.get("edits") {
            for e in edits {
                for k in PATH_KEYS {
                    if let Some(Value::String(p)) = e.get(*k) {
                        out.push(p.clone());
                        break;
                    }
                }
            }
        }
    }
    collect_patch_paths(args, 0, &mut out);
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

fn collect_patch_paths(v: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > 3 {
        return;
    }
    match v {
        Value::String(s) if s.contains("*** Begin Patch") => out.extend(patch_paths(s)),
        Value::Array(a) => a
            .iter()
            .for_each(|x| collect_patch_paths(x, depth + 1, out)),
        Value::Object(o) => o
            .values()
            .for_each(|x| collect_patch_paths(x, depth + 1, out)),
        _ => {}
    }
}

/// File headers of a Codex-style `*** Begin Patch` payload.
pub fn patch_paths(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|l| {
            let l = l.trim_end();
            [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .iter()
            .find_map(|p| l.strip_prefix(p))
            .map(|p| p.trim().to_string())
        })
        .filter(|p| !p.is_empty())
        .collect()
}

/// `meta` serialized within [`META_MAX`]: long strings are shortened until
/// it fits; if even that fails only the top-level keys are kept.
pub fn cap_meta(meta: Value) -> Value {
    if meta.to_string().len() <= META_MAX {
        return meta;
    }
    let mut limit = 4096usize;
    while limit >= 64 {
        let mut v = meta.clone();
        shorten_strings(&mut v, limit, 0);
        if v.to_string().len() <= META_MAX {
            return v;
        }
        limit /= 2;
    }
    let keys: Vec<Value> = match &meta {
        Value::Object(o) => o.keys().map(|k| Value::String(k.clone())).collect(),
        _ => Vec::new(),
    };
    serde_json::json!({ "truncated": true, "keys": keys })
}

fn shorten_strings(v: &mut Value, limit: usize, depth: usize) {
    match v {
        Value::String(s) if s.len() > limit => *s = truncate(s, limit),
        Value::Array(a) => {
            if depth > 6 {
                a.clear();
            } else {
                a.truncate(200);
                a.iter_mut()
                    .for_each(|x| shorten_strings(x, limit, depth + 1));
            }
        }
        Value::Object(o) => {
            if depth > 6 {
                o.clear();
            } else {
                o.values_mut()
                    .for_each(|x| shorten_strings(x, limit, depth + 1));
            }
        }
        _ => {}
    }
}

/// Stable 64-bit FNV-1a (ids and change detection; not cryptographic).
pub fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Same path, allowing for canonicalization and (on Windows/macOS) case.
pub fn same_path(a: &str, b: &str) -> bool {
    fn norm(p: &str) -> String {
        let c = dunce::canonicalize(p)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| p.to_string());
        let c = c.trim_end_matches(['/', '\\']).to_string();
        if cfg!(any(windows, target_os = "macos")) {
            c.to_lowercase()
        } else {
            c
        }
    }
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn previews_and_titles() {
        assert_eq!(
            tool_call_text("Bash", &json!({"command": "ls   -la\n/tmp", "timeout": 5})),
            "Bash(ls -la /tmp)"
        );
        assert_eq!(
            tool_call_text("shell", &json!({"command": ["git", "status"]})),
            "shell(git status)"
        );
        assert_eq!(
            tool_call_text("x", &json!("{\"path\":\"a.rs\"}")),
            "x(a.rs)"
        );
        assert_eq!(tool_call_text("x", &json!({})), "x()");
        assert_eq!(
            title_from_prompt("\n  fix the bug\nmore"),
            Some("fix the bug".into())
        );
        assert_eq!(
            title_from_prompt("<command-name>/clear</command-name>"),
            None
        );
        let long = "a".repeat(200);
        assert_eq!(title_from_prompt(&long).unwrap().chars().count(), TITLE_MAX);
        assert_eq!(truncate("héllo", 2), "h\n…[truncated 5 bytes]");
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_ts(&json!("1970-01-01T00:00:01.500Z")), Some(1500));
        assert_eq!(parse_ts(&json!(1_700_000_000)), Some(1_700_000_000_000));
        assert_eq!(
            parse_ts(&json!(1_700_000_000_123i64)),
            Some(1_700_000_000_123)
        );
        assert_eq!(parse_ts(&json!("nope")), None);
    }

    #[test]
    fn file_edits() {
        assert_eq!(
            edited_paths("Edit", &json!({"file_path": "/a/b.rs"})),
            ["/a/b.rs"]
        );
        assert_eq!(
            edited_paths("Read", &json!({"file_path": "/a/b.rs"})),
            Vec::<String>::new()
        );
        let patch =
            "*** Begin Patch\n*** Update File: src/x.rs\n@@\n*** Add File: y.md\n*** End Patch";
        assert_eq!(
            edited_paths("apply_patch", &json!(patch)),
            ["src/x.rs", "y.md"]
        );
        assert_eq!(
            edited_paths("shell", &json!({"command": ["apply_patch", patch]})),
            ["src/x.rs", "y.md"]
        );
        assert_eq!(
            edited_paths(
                "MultiEdit",
                &json!({"file_path": "a", "edits": [{"old_string": "x"}]})
            ),
            ["a"]
        );
    }

    #[test]
    fn meta_is_capped() {
        let big = json!({"args": {"content": "x".repeat(100_000)}, "tool": "Write"});
        let capped = cap_meta(big);
        assert!(capped.to_string().len() <= META_MAX);
        assert_eq!(capped["tool"], "Write");
        let wide = Value::Object((0..5000).map(|i| (format!("k{i}"), json!(i))).collect());
        assert_eq!(cap_meta(wide)["truncated"], true);
    }
}
