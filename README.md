# blirp

blirp is a free, open-source desktop app and daemon for running CLI coding agents (Claude Code, Codex, opencode, pi, Gemini CLI, Cursor CLI, Amp, Aider, DeepSeek Harness `dsh`, or any other command) in real terminal tabs. It tracks every session, including the ones you start outside blirp, and gives each new session in a project automatic memory of the work done before it. No handoff documents, no copy-pasting.

A project is just a folder. Git is optional: a notes or design folder without `.git` works exactly like a repository. Machines can optionally pair with one self-hosted hub (for example an always-on Mac mini or Linux box), so memory follows you across Windows, macOS and Linux, and a phone can reach the UI through the hub's web portal.

Licensed under Apache-2.0. Status: pre-1.0 (0.x); data formats can still change between minor versions.

## Why

Coding agents forget everything between sessions. Every new session re-reads the code, re-discovers the same pitfalls and re-litigates decisions, unless you write a handoff note by hand. The agents already save full transcripts on disk. blirp reads them, keeps a short project brief plus decisions, open threads and gotchas up to date, and hands that to the next session at startup through each agent's own mechanism.

## Features

- **Real terminals.** Agents run unmodified in a pseudo-terminal (ConPTY on Windows) rendered with xterm.js. Tabs, a grid of all live sessions, notifications when a session needs input.
- **Every session tracked.** Status (starting, working, idle, waiting, completed, failed, detached), branch or folder, tokens and cost, transcript, summary. Sessions started outside blirp are picked up from the agents' transcript stores as well, including subagents.
- **Automatic memory.** Transcripts are redacted, summarized by a summarizer you choose (your own `claude` or `codex` CLI, a local Ollama model, or off), and turned into a per-project brief and records. New sessions get that memory at startup; older history stays searchable from the UI, `blirp mem` and an MCP server the agents can call.
- **Continue in / fork.** Start a new session, with any agent, from a handoff pack of an earlier one.
- **Optional git worktree per session** for git projects.
- **Optional self-hosted hub.** Pair machines with an 8-character code; sessions and memory replicate end to end over QUIC. Start sessions on another machine and attach to their terminals remotely.
- **Web portal.** A hub can serve the same UI over HTTPS on your LAN (or behind `tailscale serve`) for phones and other browsers, with per-device permissions.
- **No accounts, no telemetry, no blirp servers.** blirp never proxies model traffic and never stores agent credentials.

## Supported agents

What blirp does for each agent. "Launch" is a session started from blirp; "global hooks" is the opt-in `blirp hooks install` for sessions started outside blirp. "Verified" means checked against the installed CLI version shown; "docs" means implemented from the agent's official documentation only.

| Agent (binary) | Transcript ingest source | Memory at launch | MCP at launch | Global hooks / MCP | Resume | Verified |
|---|---|---|---|---|---|---|
| Claude Code (`claude`) | `~/.claude/projects/**/*.jsonl` incl. subagents | SessionStart hook from a per-launch `--settings` file | `--mcp-config` | hooks in `~/.claude/settings.json`, MCP in `~/.claude.json` | `--resume <id>` | 2.1.283 |
| Codex (`codex`) | `~/.codex/sessions/**/rollout-*.jsonl[.zst]`, `archived_sessions/` | `-c developer_instructions=...` (your own value kept first) | `-c mcp_servers.blirp.*` | `~/.codex/hooks.json` (trust once with `/hooks`), MCP in `config.toml` | `codex resume <id>` | 0.153.2 |
| opencode (`opencode`) | `~/.local/share/opencode/opencode*.db` | `instructions` entry via `OPENCODE_CONFIG_CONTENT` | same variable | MCP only, in `~/.config/opencode/opencode.json` | `--session <id>` | 1.18.25 |
| Gemini CLI (`gemini`) | `~/.gemini/tmp/*/chats/` | SessionStart hook via `GEMINI_CLI_SYSTEM_DEFAULTS_PATH` | same file | hooks and MCP in `~/.gemini/settings.json` | `--resume <id>` | docs |
| Cursor CLI (`cursor-agent` or `agent`) | `~/.cursor/chats/**/store.db`, `~/.cursor/projects/*/agent-transcripts/` | none at launch; `sessionStart` hook once global hooks are installed | only when installed globally | hooks in `~/.cursor/hooks.json`, MCP in `~/.cursor/mcp.json` | `--resume <id>` | docs |
| Amp (`amp`) | `~/.local/share/amp/threads/T-*.json` | `amp.systemPrompt` in a per-launch copy of your settings (`--settings-file`) | `amp.mcpServers` in that copy | not supported | `threads continue <id>` | docs |
| pi (`pi`) | `~/.pi/agent/sessions/**/*.jsonl` | `--append-system-prompt <memory file>` | none (use `blirp mem`) | not supported | relaunches fresh | docs |
| Aider (`aider`) | `.aider.chat.history.md` in registered project folders | `--read <memory file>` | none | not supported | relaunches fresh | docs |
| DeepSeek Harness (`dsh`) | `~/.dsh/sessions/**/session.v3.jsonl.zstd` | none (no mechanism found); `BLIRP_MEMORY_FILE` env only | none | not supported | relaunches fresh | ingest: real data |
| Shell | none | `BLIRP_MEMORY_FILE` env only | none | n/a | relaunches fresh | n/a |
| Any command (`[[agents.custom]]`) | none | `BLIRP_MEMORY_FILE` env only | none | n/a | relaunches fresh | n/a |

Every agent gets the environment variables `BLIRP_SESSION_ID`, `BLIRP_PROJECT_ID`, `BLIRP_HOME` and `BLIRP_MEMORY_FILE`. Agent directories honor their usual overrides (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`, `GEMINI_CLI_HOME`, `CURSOR_CONFIG_DIR`, `AMP_DATA_DIR`, `PI_CODING_AGENT_DIR`, `DSH_HOME`). Launching from blirp never edits your agent config: it passes flags, environment and generated files under `~/.blirp/launch/<session>/`. Details per agent: [docs/agents.md](docs/agents.md).

Not supported: IDE-embedded agents (Copilot Chat in VS Code, the Cursor editor's own chat), cloud-only agents without a local CLI, proxying or metering model traffic.

## Install

Downloads are on [GitHub Releases](https://github.com/backyarddd/blirp/releases). Full instructions, checksums, upgrades and uninstalling: [docs/install.md](docs/install.md).

| | Desktop app | Standalone CLI / daemon |
|---|---|---|
| Windows 10 1809+ (x64) | `blirp_<version>_x64-setup.exe` (per user) or `.msi` | `blirp-<version>-x86_64-pc-windows-msvc.zip` |
| macOS 11+ (Apple silicon, Intel) | `blirp_<version>_aarch64.dmg` / `_x64.dmg` | `blirp-<version>-<arch>-apple-darwin.tar.gz` |
| Linux x64 / arm64 (glibc 2.35+) | `.AppImage`, `.deb` or `.rpm` | `blirp-<version>-<arch>-unknown-linux-gnu.tar.gz` |

The desktop app bundles the `blirp` binary, starts it as a background daemon, and updates itself (signed updates, it asks first). The standalone binary is the same daemon, CLI, hook handler and MCP server in one file, for servers, hubs or browser-only use.

## Quick start (5 minutes)

1. Install and open the desktop app. It starts the daemon and shows the UI.
   CLI only: `blirp daemon --detach`, then `blirp open` (opens the UI in your browser, logged in).
2. Click the orange **+** (Ctrl+T, Cmd+T on macOS). Choose **Folder path**, enter any absolute folder path, pick an agent. Only agents found on your `PATH` can be selected; `shell` always works. Optionally type a first prompt.
3. Work as usual in the terminal tab. When the session has been idle for 5 minutes, or ends, blirp summarizes it.
4. Start a second session in the same folder. It begins with a `# blirp memory: <project>` block: the project brief, open threads, recent decisions, gotchas and the last sessions. Open the **Memory** panel in the session toolbar to see exactly what was injected, and edit the brief or threads there.
5. Optional: `blirp hooks install` gives sessions you start in your own terminal the same memory (Claude Code, Codex, Gemini CLI, Cursor; MCP only for opencode). It edits those agents' user config files, reversibly; see [docs/memory.md](docs/memory.md#global-hooks).

Closing the window keeps the daemon and all sessions running; the tray icon reopens it. **Quit blirp** in the tray stops the daemon and ends every session (it asks first).

```sh
blirp status            # is the daemon running, where, which version
blirp sessions          # recent sessions
blirp mem search "why did we drop sqlx"   # search memory of the current folder's project
blirp doctor            # data dir, config, database, git, agents, ingest per agent
blirp service install   # start the daemon at login (per user)
```

## How memory works

```
 agent CLIs write their own transcripts          blirp-launched sessions also report
 (~/.claude, ~/.codex, opencode.db, ...)         status through hooks where supported
               |                                              |
               v                                              v
   [1 ingest] tail files/DBs incrementally ----> sessions + events in ~/.blirp/blirp.db
               |                                              ^
   [2 redact]  secrets -> [REDACTED:<kind>] before anything is stored
               |
   [3 distill] session idle 5 min or ended -> summarizer (claude / codex / ollama / none)
               |   -> session title + summary
               |   -> records: decisions, open threads, gotchas (resolves old threads)
               |   -> new brief version (or a suggestion, in review mode)
               v
   [4 inject]  next session in the project starts with:
               # blirp memory: <project>
               brief | open threads | recent decisions | gotchas | recent sessions
               (hard size cap, stable order, so agent prompt caches keep hitting)
               |
   [5 recall]  on demand: MCP tools mem_search / mem_session / mem_recent / mem_brief /
               mem_record, `blirp mem ...`, full-text search in the UI
```

Terminal output is never scraped for memory; it is only used for the live view and the idle/working heuristic. Only this machine's top-level sessions are distilled (subagents are covered by their parent's summary; replicated sessions are distilled on the machine that ran them). Distillation has a daily budget (40 runs by default). Everything is visible and editable in the UI. Full description: [docs/memory.md](docs/memory.md).

## Self-hosted hub

Run blirp on an always-on machine, enable the hub role (`blirp hub enable` or **Settings > Machines & Sync**), and pair each other machine with `blirp pair <invite> <code>` (or just `blirp pair <code>` on the same LAN). After that projects, sessions, transcripts, briefs, records, wiki pages and resources replicate through the hub, you can start sessions on any paired machine, and the hub can serve the UI to phones over HTTPS on your LAN. Connections are QUIC (iroh) with NAT traversal, so the hub needs no port forwarding; the public relay fallback can be disabled or replaced. Guide: [docs/sync-and-hub.md](docs/sync-and-hub.md).

## Privacy

- All state is in `~/.blirp` (`%USERPROFILE%\.blirp` on Windows) on your machines: one SQLite database plus config, keys and logs. No accounts, no telemetry, no blirp servers.
- Transcript text is redacted (cloud keys, tokens, private keys, JWTs, connection strings, `.env` lines, ...) before it is stored, synced or summarized. Logs never contain transcript text.
- What leaves the machine, and only if you use it: redacted transcript excerpts go to the summarizer you picked (your `claude` or `codex` CLI sends them to that provider under your own login; Ollama stays local; `none` sends nothing). Sync traffic goes only to your paired machines, end-to-end encrypted, with public iroh relays and DNS discovery used for connection setup unless you set `sync.relay = "disabled"` or your own relay. The desktop app checks GitHub Releases for updates.
- The daemon listens on `127.0.0.1` only. The LAN portal is off by default and exists only on a hub.

Details and threat model: [docs/security.md](docs/security.md). Report vulnerabilities privately: [SECURITY.md](SECURITY.md).

## Documentation

- [Getting started](docs/getting-started.md), [Install](docs/install.md)
- [Projects and sessions](docs/projects-and-sessions.md), [Memory](docs/memory.md), [Agents](docs/agents.md)
- [Sync and hub](docs/sync-and-hub.md), [Web portal](docs/portal.md)
- [Configuration](docs/configuration.md), [CLI](docs/cli.md), [HTTP API](docs/api.md)
- [Security](docs/security.md), [Troubleshooting](docs/troubleshooting.md), [FAQ](docs/faq.md)
- [Development](docs/development.md), [Architecture](docs/ARCHITECTURE.md) (the design contract)

Index: [docs/README.md](docs/README.md).

## Building from source

Requirements: Rust 1.89+ (CI pins 1.94.0), Node.js 22, pnpm 12. For the desktop app also the [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/).

```sh
pnpm -C web install && pnpm -C web build     # the UI, embedded into the binary
cargo build --release -p blirp               # target/release/blirp
```

Running from source, tests, the desktop app and the release process: [docs/development.md](docs/development.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
