# Memory

blirp gives every new session in a project a short, stable summary of the work done before it, and keeps the full history searchable. This page covers every stage and how to turn each part off.

- [Capture](#capture)
- [Redaction](#redaction)
- [Distill](#distill) and [summarizer backends](#summarizer-backends) (cost and quota)
- [What memory contains](#what-memory-contains): brief, records, wiki, resources, suggestions
- [Injection](#injection)
- [Recall: MCP tools](#mcp-tools) and [`blirp mem`](#blirp-mem)
- [Global hooks](#global-hooks) for sessions started outside blirp
- [Turning things off](#turning-things-off)

## Capture

blirp reads the transcripts that agents already write to disk (Claude Code JSONL, Codex rollouts, opencode's SQLite database, ...; the list per agent is in [agents.md](agents.md)). It tails them incrementally: file watchers, a full rescan every 5 minutes and at daemon start, and immediate reads when a hook reports a transcript path. Each transcript becomes a session with events: user prompts, assistant replies, tool calls (one line plus redacted arguments), tool results (cut to 4 KiB), file edits (one event per path), compaction summaries and injected system context. Reasoning/thinking blocks are not stored.

Terminal output is never used as memory. It only drives the live view and the Working/Idle heuristic.

Shell sessions and custom agents have no transcript, so they are tracked (status, folder, duration) but add nothing to memory.

History is permanent: deleting or rotating an agent's transcript never deletes blirp's copy.

## Redaction

Every event's text and metadata, and every title, is redacted before it is written to the database, which means before it can be synced or summarized. Matches are replaced with `[REDACTED:<kind>]`. Rules (gitleaks-style, each with tests):

AWS access and secret keys, GCP API keys, Azure client secrets, storage keys and SAS signatures, GitHub and GitLab tokens, Slack tokens and webhooks, Stripe keys, OpenAI and Anthropic keys, generic `sk-` keys, npm, PyPI, Hugging Face, Google OAuth, Shopify, DigitalOcean, Twilio and SendGrid tokens, private key blocks, JWTs, connection strings with credentials (the password part), `.env`-style `KEY=value` lines, and values of keys named like `password`, `secret`, `token` or `api_key`, plus high-entropy values assigned to secret-looking names.

Redaction is pattern based. A secret in an unusual format can survive; see [security.md](security.md#redaction-limits).

## Distill

Distilling turns the new part of a session's transcript into memory.

**When.** A session is distilled when it has been quiet for `memory.distill_idle_secs` (default 300 s) with a new prompt or reply since its last distill (tool output alone does not count), or shortly after it ends (process exit or the agent's session-end hook; the run waits 10 s so the last transcript lines are in, and both triggers make one run). A scheduler checks every 30 s. Only sessions active in the last 7 days are distilled automatically, so importing old history does not use up the budget.

**Which sessions.** Only sessions that ran on this machine: a session replicated from another machine is distilled there and its results arrive by sync. Subagent sessions are skipped (their task and final report are already in the parent's transcript); **Distill now** still works on them.

**Budget.** At most `memory.daily_distill_limit` runs (default 40) per UTC day, counting manual runs. One run at a time.

**Input.** The redacted transcript since the last distill (with the session's previous summary as context, so a long session is summarized piece by piece and nothing is sent twice), compacted (prompts verbatim, assistant text, tool calls and results as short one-liners), capped at `memory.distill_max_chars` (default 60 000 characters: the first 20 % and last 80 % are kept around an omission marker), plus the current brief and the active records with their ids.

**Output.** Strict JSON, validated (one retry with the validation error on bad output):

- `title` and a 3-6 sentence `summary` of the session,
- `decisions`, `open_threads`, `gotchas` (up to 20 each) that become records,
- `resolved_record_ids`: existing open threads (or other records) this session settled,
- `files` touched,
- `brief_md`: a full replacement brief (up to 12 000 characters), or empty for no change.

**Apply.** In one transaction: the session gets its summary (and a title if it has none); new records are added unless an active record of the same kind and title exists (titles compare ignoring case, spacing and punctuation), and a record this session already produced is updated rather than added again; resolved records are marked resolved; the brief gets a new version (`brief_mode = "auto"`, the default) or becomes a suggestion (`"review"`, and always when you edited the brief while the run was in progress, so your edit is never overwritten). Records you created or edited yourself are never changed by the distiller; if it wants to resolve one, it files a suggestion instead.

A failed run keeps the earlier summary, stores the error on the session (shown on the session page and by `blirp mem show`), and is retried only when new events arrive or you click **Distill now**. Backend failures (not installed, not logged in, timeout after 180 s) are not retried in a loop.

### Summarizer backends

`memory.summarizer` in `config.toml`, or **Settings > Memory**:

| Value | What runs | Where your data goes | Cost / quota |
|---|---|---|---|
| `auto` (default) | the summarizer of your default agent for new sessions (`agents.default`: `claude` or `codex`), else `claude`, else Ollama answering at `$OLLAMA_HOST` or `http://127.0.0.1:11434` (see below) | depends on the pick | depends on the pick |
| `claude` | `claude -p --model sonnet --output-format json --safe-mode --strict-mcp-config --no-session-persistence --tools ""` | Anthropic, under your Claude Code login | Uses your Claude plan's usage limits or your API key's billing. One run sends up to about 60 000 characters (roughly 15 000 tokens) plus the brief and records to Sonnet, which uses noticeably more of your plan (or API spend) per run than Haiku did before 0.1.1. |
| `codex` | `codex exec --json --ephemeral --skip-git-repo-check --ignore-user-config --sandbox read-only -c web_search="disabled"` plus `--disable` for `hooks`, `shell_tool`, `unified_exec`, `view_image`, `apps`, `plugins`, `browser_use`, `computer_use`, `multi_agent`, `image_generation`, `sleep_tool`, `goals`, `tool_suggest`, `skill_search`, `code_mode_host` (needs codex 0.153 or newer) | OpenAI, under your Codex login | Your `~/.codex/config.toml` is not loaded (no MCP servers, plugins or hooks), so Codex's built-in default model and provider are used; counts against your ChatGPT plan or API billing. |
| `ollama` | `POST /api/chat` with model `memory.ollama_model` (default `qwen2.5:7b`) | stays on the machine running Ollama | free; quality depends on the model (7B+ recommended) |
| `none` | nothing | nowhere | none |

Every CLI run happens in an empty scratch folder `~/.blirp/distill/run-*` (deleted afterwards) with hooks, plugins, MCP servers and session persistence disabled, and with `BLIRP_DISTILLING=1`, so the summarizer run is never ingested and never triggers blirp hooks. It is killed with its whole process tree after 180 s. `claude` runs with no tools at all. `codex` has no switch that removes every built-in tool: blirp turns off its shell and exec tools and the optional tools (feature names checked against codex 0.153) and keeps its read-only sandbox, but a transcript crafted to steer the model could still use a remaining built-in tool (for example to look at files). That is why `auto` picks `codex` only when it is your default agent (you already run it with every tool on in these projects); otherwise choose it only if you accept that. The summarizer's reply is redacted before it is stored.

How `auto` picks, every run:

1. Your default agent's summarizer, when that agent is Claude Code or Codex, it is on PATH, and its own status command (`claude auth status`, `codex login status`; local, no model call) does not say it is logged out. Codex must also be 0.153 or newer: older versions refuse the switches that turn its tools off.
2. Else `claude` (on PATH and not logged out). Other default agents (opencode, custom agents, ...) have no summarizer and start here.
3. Else Ollama, if it answers.
4. Else a CLI that is installed but logged out, the default agent's first: its run fails with its own sign-in error, which pauses automatic distilling with that reason.

**Settings > Memory** shows what `auto` uses right now and why, for example "Automatic uses Claude Code (Sonnet), your default agent" or "Automatic uses Ollama (qwen2.5:7b): your default agent, Codex, is not signed in". If `codex login status` says it is not logged in although Codex works (for example with a key only in the environment), choose `codex` explicitly.

To cap spend: lower `memory.daily_distill_limit`, raise `memory.distill_idle_secs` (fewer partial distills of long sessions), lower `memory.distill_max_chars`, or use `ollama`.

## What memory contains

Everything below is per project, visible on the project's **Memory** tab and in the session's **Memory** panel, and editable. Chats (sessions that belong to no project, see [projects-and-sessions.md](projects-and-sessions.md#chats)) have no project memory: distilling a chat writes only its session summary, and nothing is injected into one. So memory from unrelated chats never reaches a project, and a project's memory never reaches a chat.

- **Brief.** A markdown description of the project: what it is, how to build and run it, current priorities. Written by the distiller and by you. Every change is a new version; **Versions** lists them and **Revert** restores one (as a new version).
- **Records.** Short items of kind `decision`, `open_thread`, `gotcha`, `plan` or `note`, each with title, body, status (`active`, `resolved`, `archived`) and a pinned flag. The distiller creates decisions, open threads and gotchas and resolves threads; you can create any kind and edit, pin, resolve, reopen or delete them (archiving is available through the API). Agents can add records through the `mem_record` MCP tool.
- **Suggestions.** Changes the distiller proposes instead of applying: brief updates in review mode, and resolutions of records you edited. **Accept**, **Reject** or **Dismiss** them on the Memory tab. Suggestions are local to the machine that produced them.
- **Wiki.** Markdown pages you write per project (the Wiki tab). Not injected and not written by the distiller.
- **Resources.** Links, repos, PRs, issues, docs and files you attach to the project (the Resources tab). Not injected.
- **Session summaries.** Title, summary, decisions, open threads, gotchas and files per session, on the session's page.

## Injection

At launch blirp renders the project's memory as markdown and hands it to the agent. The render is:

```markdown
# blirp memory: <project name>
<brief, at most half the budget; "_No project brief yet._" when empty>
## Pinned              pinned active records of any kind
## Active plans        active plan records
## Open threads        active open threads (max 10)
## Recent decisions    (max 8)
## Gotchas             (max 5)
## Recent sessions     last 3 titled or summarized sessions: date · agent · machine · title: summary
Tools: search older history with the blirp MCP tools (mem_search, mem_session, mem_recent, mem_record) or `blirp mem search "<query>"`.
```

- Hard cap `memory.inject_max_chars` (default 8 000 characters). Sections are filled in the order above until the budget is used; the Tools line is always kept; empty sections are left out.
- Records are ordered pinned first, then most recently updated. Each item is one line of at most 400 characters. Dates are UTC.
- Nothing in the text depends on the current time, so it only changes when memory changes, and agents' prompt caches keep hitting.
- For continue in / fork, a [handoff pack](projects-and-sessions.md#continue-in--fork) is appended.
- Chats get nothing (only a handoff pack you asked for with continue in / fork).

The rendered text is written to `~/.blirp/launch/<session-id>/memory.md` and `BLIRP_MEMORY_FILE` points at it. The session's **Memory** panel shows exactly that file under **Injected at start** (`GET /api/inject?session=<id>`), followed by the session's distilled summary and the project's brief, open threads and pinned records, all editable in place.

How it reaches each agent:

| Agent | Mechanism at blirp launch |
|---|---|
| Claude Code | SessionStart hook (`startup`, `resume`, `clear`, `compact`) in a per-launch `--settings` file returns the memory as `additionalContext`. Your own hooks keep running. |
| Codex | `-c developer_instructions=<your developer_instructions from config.toml>\n\n<memory>` (capped at 24 000 characters). |
| opencode | `OPENCODE_CONFIG_CONTENT` adds `memory.md` to `instructions` (merged with any existing value of that variable). |
| Gemini CLI | SessionStart hook in a per-launch system-defaults file (`GEMINI_CLI_SYSTEM_DEFAULTS_PATH`); your own settings still take precedence. |
| Amp | `--settings-file` pointing at a copy of your Amp settings with the memory appended to `amp.systemPrompt`. If your settings file cannot be read, Amp starts without memory. |
| pi | `--append-system-prompt <memory.md>` |
| Aider | `--read <memory.md>` |
| Cursor CLI | Nothing at launch (no per-launch config override exists). With global hooks installed, the `sessionStart` hook injects it. |
| dsh, Shell, custom | Only `BLIRP_MEMORY_FILE`. Tell the agent to read it, or use a handoff, whose first prompt names the file. |

Any failure while preparing injection is logged and the agent starts without memory; a session is never blocked by memory.

Hooks keep working when the daemon is down: a SessionStart hook then falls back to the session's `memory.md`, or renders from the database opened read-only.

## MCP tools

blirp is an MCP server (`blirp mcp` over stdio, and Streamable HTTP at `http://127.0.0.1:<port>/mcp` with the runtime token). It is registered automatically for blirp launches of Claude Code, Codex, opencode, Gemini CLI and Amp, and globally by `blirp hooks install` for Claude Code, Codex, Gemini CLI, Cursor and opencode.

| Tool | Arguments | Returns |
|---|---|---|
| `mem_search` | `query`, `project?` (`"current"` default, `"all"`, or an id), `kinds?` (`["record"]`, `["event"]`), `limit?` (default 10, max 50) | ranked hits from records and transcripts with session id, seq, date, agent and a snippet (matches in bold) |
| `mem_session` | `session_id`, `from_seq?`, `limit?` (default 50, max 200) | session header, summary, files and a page of events, with the next `from_seq` |
| `mem_recent` | `project?`, `limit?` (default 10, max 50) | recent sessions with agent, date, status, title and summary |
| `mem_brief` | `project?` | the brief and every active record, grouped by kind |
| `mem_record` | `kind` (`decision`, `open_thread`, `gotcha`, `plan`, `note`), `title`, `body` | creates a record in the current project (needs the daemon running) |

The current project is `BLIRP_PROJECT_ID` (set for blirp launches), else the registered project containing the server's working directory (nothing is registered by a lookup), else, over HTTP, the `?project=<id>` query parameter of the `/mcp` URL. The stdio server opens the database read-only; writes go through the daemon API.

## `blirp mem`

Memory commands read the database directly (read-only), so they work while the daemon is stopped (it must have run once). Without `--project` they use the project of the current folder.

```sh
blirp mem search "rate limiter"              # records and transcripts of this project
blirp mem search --all --kind record "auth"  # every project, records only
blirp mem brief                              # brief + active records
blirp mem recent --limit 5                   # last sessions with summaries
blirp mem show <session-id> --limit 50       # summary + transcript events
```

Every subcommand takes `--json`. Full reference: [cli.md](cli.md#blirp-mem).

## Global hooks

Sessions blirp launches get memory and status without touching any of your files. For sessions you start **outside** blirp (your own terminal, an IDE terminal), `blirp hooks install` adds blirp hook entries and the blirp MCP server to the agents' user config. This is the only place blirp edits files it does not own. It is opt-in and reversible.

```sh
blirp hooks install [--agent claude|codex|gemini|cursor|opencode]
blirp hooks status  [--agent ...]
blirp hooks uninstall [--agent ...]
```

Without `--agent`, `install` handles every supported agent found on `PATH`; `status` and `uninstall` handle all supported agents. The same is available per agent in **Settings > Agents**, which shows the state of global hooks, the MCP server and memory at launch for each agent; **Install** lists the files it will change before it changes them, **Uninstall** removes the entries. Only the machine's own desktop app or `blirp open` session can do this, not portal devices.

Files changed (`~` is your home directory; environment overrides in brackets):

| Agent | Hooks | MCP server |
|---|---|---|
| Claude Code | `~/.claude/settings.json` [`$CLAUDE_CONFIG_DIR/settings.json`]: `hooks.SessionStart` (matcher `startup\|resume\|clear\|compact`), `UserPromptSubmit`, `Stop`, `Notification`, `SessionEnd`, `PreCompact` | `~/.claude.json` [`$CLAUDE_CONFIG_DIR/.claude.json`]: `mcpServers.blirp` |
| Codex | `~/.codex/hooks.json` [`$CODEX_HOME`]: `SessionStart`, `UserPromptSubmit`, `Stop`, `PreCompact`, `SessionEnd`. Codex asks you to trust new hooks: run `/hooks` in Codex once. | `~/.codex/config.toml`: `[mcp_servers.blirp]` (edited in place; comments and layout kept) |
| Gemini CLI | `~/.gemini/settings.json`: `hooks.SessionStart`, `BeforeAgent`, `AfterAgent`, `Notification`, `SessionEnd`, `PreCompress` | same file: `mcpServers.blirp` |
| Cursor CLI | `~/.cursor/hooks.json` (`version: 1`): `sessionStart`, `beforeSubmitPrompt`, `stop`, `sessionEnd`, `preCompact` | `~/.cursor/mcp.json`: `mcpServers.blirp` |
| opencode | not supported (opencode has no shell hooks) | `~/.config/opencode/opencode.json` [`$XDG_CONFIG_HOME/opencode/`, or `opencode.jsonc` if that is the only one]: `mcp.blirp` |

Each hook entry runs `<absolute path to blirp> hook <agent> <event> --global` with a short timeout; the MCP entry runs `<blirp> mcp`. Guarantees:

- Every other entry in those files (other hooks, other MCP servers, unrelated settings) is preserved; JSON key order is kept.
- The first time a file is changed, the original is copied to `<file>.blirp-backup` (once; later installs do not overwrite it).
- Files are replaced atomically (temp file + rename).
- Files that contain comments (JSONC) or invalid syntax are not rewritten; the command reports it and you can add the entries by hand. (`config.toml` is edited with a comment-preserving TOML editor.)
- Installing twice changes nothing. `uninstall` removes exactly the entries whose command matches `blirp hook <agent> <event> --global` and an MCP server named `blirp` running `blirp mcp`, and nothing else.
- The absolute path of the `blirp` binary is written into the entries. After moving or reinstalling blirp to another path, run `blirp hooks install` again.

What the hooks do: report status (working, idle, waiting, completed), link the session to its transcript for immediate ingest, queue a distill when the session ends, and on session start print the project's memory for the agent to add to its context (Claude Code, Codex, Gemini CLI: `hookSpecificOutput.additionalContext`; Cursor: `additional_context`). Hooks always exit 0 within 2 seconds, also when the daemon is down or slow. Inside a blirp launch the global entries do not inject a second time.

## Turning things off

| To... | Do this |
|---|---|
| Never send transcripts to a model | `memory.summarizer = "none"`. Ingest, search, MCP and injection of what you write yourself (brief, records, recent session titles) keep working. |
| Keep summarizing local | `memory.summarizer = "ollama"`. |
| Review brief changes before they apply | `memory.brief_mode = "review"`. |
| Spend less | lower `memory.daily_distill_limit`, raise `memory.distill_idle_secs`. |
| Inject less | lower `memory.inject_max_chars` (minimum 500). |
| Stop memory for sessions started outside blirp | `blirp hooks uninstall` (or never install). |
| Launch an agent from blirp without injection | there is no switch for built-in agents; define a [custom agent](agents.md#custom-agents) running the same binary (custom agents only get `BLIRP_MEMORY_FILE`). |
| Remove a bad memory | edit, resolve or delete the record; edit the brief or revert it to an earlier version. |

Ingest of transcripts on this machine cannot be switched off; it only reads local files and sends nothing anywhere by itself.
