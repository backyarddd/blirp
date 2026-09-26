# Agents

blirp runs agents as their own, unmodified CLIs. It never proxies model traffic and never reads or stores their credentials; each agent keeps its own login and config. For every agent this page lists what blirp does at launch, where it reads transcripts, how memory gets in and what has been verified.

"Verified" means checked against the installed CLI (version shown) on the reference machine. "Docs" means implemented from the agent's official documentation or published format only; if it misbehaves with your version, please open an issue with `blirp doctor` output.

## Common behavior

- **Detection.** An agent is available when its binary is on the daemon's `PATH`. On Windows only `.exe`, `.com`, `.cmd`, `.bat` and `.ps1` count; npm `.cmd` shims (`codex.cmd`, `gemini.cmd`, ...) are run as `node <script>` so arguments never pass through `cmd.exe` quoting, other `.cmd`/`.bat` run through `cmd /d /c`, `.ps1` through PowerShell `-File`. Versions come from `<binary> --version`. **Settings > Agents** and `blirp doctor` show what was found; the list is cached for 60 seconds.
- **Environment.** Every session gets `BLIRP_SESSION_ID`, `BLIRP_PROJECT_ID`, `BLIRP_HOME`, `BLIRP_MEMORY_FILE` (path of the rendered memory), `TERM=xterm-256color` and `COLORTERM=truecolor`. Variables that would make an agent think it runs inside another agent (`CLAUDECODE`, `CLAUDE_CODE_*` session variables, `CLAUDE_PID`, `CLAUDE_EFFORT`, `CODEX_SANDBOX`) are removed.
- **Launch files.** Everything blirp generates for a launch lives in `~/.blirp/launch/<session-id>/` (`memory.md`, `handoff.md`, and per-agent settings). Your agent config files are never edited at launch.
- **First prompt.** Typed into the agent once its output has been quiet for 1.5 s (at most 60 s), as a bracketed paste when the agent enabled that, followed by Enter. While a folder-trust dialog is on screen, blirp waits (up to 10 minutes) for you to answer it.
- **Failures degrade.** If blirp cannot prepare the memory integration, the agent starts without memory.

Where the agents' own data directories are moved with environment variables, blirp follows: `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `GEMINI_CLI_HOME`, `CURSOR_CONFIG_DIR`, `AMP_DATA_DIR`, `PI_CODING_AGENT_DIR`, `DSH_HOME` (the daemon must see them, so set them where the daemon starts).

## Claude Code

- Binary `claude`. Verified with 2.1.283.
- Launch: `claude --session-id <new uuid> --settings ~/.blirp/launch/<id>/settings.json --mcp-config ~/.blirp/launch/<id>/mcp.json`. The settings file contains only blirp hooks for `SessionStart` (matcher `startup|resume|clear|compact`), `UserPromptSubmit`, `Stop`, `Notification`, `SessionEnd`, `PreCompact`; Claude merges them with your own hooks, so both run.
- Memory: the SessionStart hook returns it as `additionalContext`. MCP: `blirp` server from `mcp.json`.
- Status: from hooks (prompt submitted = Working, Stop = Idle, permission or idle notification = Waiting, session end = Completed).
- Resume: `--resume <session id>`.
- Ingest: `~/.claude/projects/*/*.jsonl` (verified on real data), including subagents in `<session>/subagents/agent-*.jsonl` (title from the `.meta.json` next to it) and tool output spilled to `tool-results/*.txt` (linked, 4 KiB preview). Cost from Claude's own `cost-state` lines.
- Global hooks: `~/.claude/settings.json` and `~/.claude.json` ([details](memory.md#global-hooks)).

## Codex

- Binary `codex`. Verified with 0.153.2 (`codex debug prompt-input`).
- Launch: `codex -c developer_instructions=<...> -c mcp_servers.blirp.command=<blirp> -c mcp_servers.blirp.args=["mcp"] -c mcp_servers.blirp.env={...}`, placed before a `resume` subcommand.
- Memory: appended to your own `developer_instructions` from `config.toml` (yours stay first), capped at 24 000 characters. Codex has SessionStart hooks, but hooks it does not manage only run after you trust their exact definition, so launches use `developer_instructions` instead.
- Status: output heuristics. With global hooks installed and trusted, from hooks.
- Resume: `codex resume <id>`.
- Ingest: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` and `archived_sessions/`, also zstd-compressed `.jsonl.zst` (verified on real data). Conversation from `response_item` lines, token totals from usage records, cost estimated from the price table.
- Global hooks: `~/.codex/hooks.json` plus `[mcp_servers.blirp]` in `~/.codex/config.toml`. Run `/hooks` in Codex once to trust them.

## opencode

- Binary `opencode`. Verified with 1.18.25 (`opencode debug config`).
- Launch: `OPENCODE_CONFIG_CONTENT` with `instructions += ["<memory.md>"]` and an `mcp.blirp` local server, merged into any value of that variable you already set (arrays concatenate with your other config layers).
- Status: output heuristics (opencode has no shell hooks).
- Resume: `--session <id>`.
- Ingest: `~/.local/share/opencode/opencode*.db` (`$XDG_DATA_HOME/opencode`; on Windows also `%APPDATA%\opencode`), one session per row, opened read-only while opencode uses it (verified on real data). The legacy JSON `storage/` tree is not read. Child sessions become subagents. Cost from opencode.
- Global: MCP only, `mcp.blirp` in `~/.config/opencode/opencode.json`.

## Gemini CLI

- Binary `gemini`. Docs only (not installed on the reference machine).
- Launch: `GEMINI_CLI_SYSTEM_DEFAULTS_PATH=~/.blirp/launch/<id>/gemini-system-defaults.json`, a copy of your existing system defaults (from that variable or the OS default location) plus blirp hooks (`SessionStart`, `BeforeAgent`, `AfterAgent`, `Notification`, `SessionEnd`, `PreCompress`) and `mcpServers.blirp`. System defaults are the lowest settings layer, so your user settings still win.
- Memory: SessionStart hook `additionalContext`. Status: hooks.
- Resume: `--resume <id>`.
- Ingest: `~/.gemini/tmp/<project>/chats/session-*.json[l]` (`$GEMINI_CLI_HOME/.gemini`), subagents in `chats/<parentId>/`, folder from `.project_root` or `projects.json`. Implemented from the gemini-cli source.
- Global hooks: `~/.gemini/settings.json` (hooks and `mcpServers.blirp`).

## Cursor CLI

- Binary `cursor-agent`, else `agent`. Docs only.
- Launch: no per-launch config override exists, so nothing is added; `BLIRP_MEMORY_FILE` is set.
- Memory and MCP: only with global integration installed (`sessionStart` hook returns `additional_context`; MCP from `~/.cursor/mcp.json`).
- Status: global hooks if installed, else output heuristics.
- Resume: `--resume <id>`.
- Ingest: `~/.cursor/chats/<hash>/<id>/store.db` (preferred; folder from the sibling `meta.json`, format from community documentation) and `~/.cursor/projects/<encoded folder>/agent-transcripts/**/*.jsonl` (verified on sparse real data; no timestamps). `$CURSOR_CONFIG_DIR` overrides `~/.cursor`. The Cursor editor's `state.vscdb` (IDE chat) is not ingested.
- Global hooks: `~/.cursor/hooks.json`, `~/.cursor/mcp.json`.

## Amp

- Binary `amp`. Docs only.
- Launch: `--settings-file ~/.blirp/launch/<id>/amp-settings.json`, a copy of your `~/.config/amp/settings.json` (or `settings.jsonc`) with the memory appended to `amp.systemPrompt` and `amp.mcpServers.blirp` added. If your settings cannot be read, Amp starts without memory rather than without your settings.
- Status: output heuristics. Resume: `amp threads continue <id>`.
- Ingest: `~/.local/share/amp/threads/T-*.json` (`$AMP_DATA_DIR`; on Windows also `%APPDATA%\amp`), from public format descriptions. A message still streaming is picked up once complete.
- Global integration: not supported.

## pi

- Binary `pi`. Docs only.
- Launch: `--append-system-prompt <memory.md>`. No MCP; the agent can run `blirp mem ...`.
- Status: output heuristics. Resume: relaunches fresh in the folder (no id-based resume).
- Ingest: `~/.pi/agent/sessions/--<folder>--/<ts>_<id>.jsonl` (`$PI_CODING_AGENT_DIR/sessions`), published format v3; every tree entry kept in file order. Cost from pi.

## Aider

- Binary `aider`. Docs only.
- Launch: `--read <memory.md>` (Aider treats it as a read-only file in the chat). No MCP.
- Status: output heuristics. Resume: relaunches fresh.
- Ingest: `.aider.chat.history.md` in the folders registered as projects on this machine (periodic rescan, not watched). Aider sessions in unregistered folders are not found; add the folder under Projects. Cost from Aider's history.

## DeepSeek Harness (dsh)

- Binary `dsh`.
- Launch: no documented way to add context, so only `BLIRP_MEMORY_FILE` is set. No MCP.
- Status: output heuristics. Resume: relaunches fresh.
- Ingest: `~/.dsh/sessions/<folder>/session-<id>/session.v3.jsonl.zstd` (`$DSH_HOME`), verified on real (decoded) data; other log versions are skipped.

## Shell

Your `$SHELL` (macOS/Linux, else `/bin/sh`); on Windows `pwsh`, else Windows PowerShell, else `%ComSpec%`. Tracked with status and duration; no transcript, so nothing is added to memory. Resume starts a new shell in the folder.

## Custom agents

Any command can be a session type. Add it to `~/.blirp/config.toml`:

```toml
[[agents.custom]]
name = "claude-plain"          # 1-40 chars of A-Z a-z 0-9 _ -; unique
command = "claude"             # executable on PATH, or a path containing / or \
args = ["--model", "opus"]     # optional, passed on every launch

[[agents.custom]]
name = "my-repl"
command = "C:/tools/repl.exe"
```

The agent id is `custom:<name>` (usable as `[agents] default = "custom:claude-plain"`). Custom agents appear in the new-session dialog and can be selected when the command resolves. They get status tracking (output heuristics), the standard environment including `BLIRP_MEMORY_FILE`, and handoffs whose first prompt tells the agent to read the handoff file. They have no transcript ingest, no injection beyond the environment variable, no MCP registration, and Resume relaunches fresh. A custom agent running a built-in agent's binary (like `claude-plain` above) is the way to launch that agent from blirp without memory injection; its transcript is still ingested by the built-in adapter and shows up as a separate external session of that agent.

Config changes made in **Settings** apply immediately; hand edits to `config.toml` apply after restarting the daemon ([configuration.md](configuration.md)).

## Adding support for a new agent

See [development.md](development.md#adding-an-agent-adapter).
