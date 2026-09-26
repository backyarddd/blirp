# blirp architecture

blirp is a free, open-source, self-hostable workspace for CLI coding agents (Claude Code, Codex, opencode, pi, Gemini CLI, Cursor CLI, Amp, Aider, DeepSeek Harness `dsh`, or any other command). It runs the real, unmodified agent TUIs inside terminal panes, tracks every session, and gives every new session automatic memory of prior work in the same project. Machines can pair to one self-hosted hub so memory follows the user across Windows, macOS and Linux.

This document is the contract. Code must match it; if code needs to diverge, update this file in the same change.

## 1. Principles

1. **Ingest, don't scrape.** Agents already persist structured transcripts. blirp tails those native stores. PTY output is only used for the live terminal and activity/status heuristics.
2. **Agents stay unmodified.** blirp never proxies model traffic and never stores agent credentials. It only launches the CLI with extra flags/env, and installs hooks/MCP config.
3. **Projects are folders, not repos.** A project is any directory. Git is optional and only unlocks git features (branch display, diff panel, optional worktree per session). A folder with no `.git` (a design tool's MCP workspace, a notes directory) is a first-class project.
4. **Local first.** Everything works standalone with no hub and no network. The hub is an optional replica/aggregator.
5. **One binary.** `blirp` is the daemon, CLI, hook handler, MCP server and hub. The desktop app is a thin Tauri shell around the web UI served by the daemon. The web portal is the same SPA.
6. **Never break the agent.** Hooks must exit 0 fast (hard 2 s budget) even if the daemon is down. Memory injection failures degrade to "no memory", never to a broken session.

## 2. Repository layout

```
blirp/
  Cargo.toml                 workspace
  crates/
    blirp-core/              model types, config, paths, SQLite store + migrations, redaction
    blirp-sync/              iroh endpoint, pairing (SPAKE2), replication protocol, remote proxy
    blirp/                   binary: daemon (axum API + WS), PTY supervisor, ingest adapters,
                             memory engine (distill/brief/inject), MCP server, hooks, CLI, service install
  app/                       Tauri 2 desktop shell (src-tauri/), bundles `blirp` as a sidecar
  web/                       Svelte 5 + Vite + TypeScript SPA (desktop UI and portal); built to web/dist,
                             embedded into the `blirp` binary with rust-embed
  docs/
  .github/workflows/         CI (fmt, clippy, test on windows/macos/linux; web check/test) and release
```

## 3. Runtime layout

Data dir `BLIRP_HOME`, default `~/.blirp` on every OS (Windows: `%USERPROFILE%\.blirp`).

```
~/.blirp/
  config.toml        user config (see §12)
  blirp.db           SQLite (WAL) - all state
  runtime.json       {pid, port, token, version, started_at}; written by daemon, mode 0600
  identity.key       iroh secret key (0600)
  daemon.lock        single-instance lock (OS file lock held by the daemon)
  logs/blirpd.<date>.log   rolling logs (tracing-appender, daily, keep 7)
  worktrees/<project-id>/<name>/   optional per-session git worktrees
  launch/<session-id>/             per-launch generated files (claude settings.json, mcp.json, memory.md)
```

`runtime.json` + the token authenticate every local client (Tauri shell, hooks, MCP stdio shim, CLI). The daemon binds `127.0.0.1:<port>` (default 47770, falls back to an ephemeral port if taken; the actual port is in runtime.json).

## 4. Processes

- `blirp daemon` - long-running per-user process. Owns PTYs, ingest watchers, memory jobs, SQLite writer, local HTTP/WS API, and (optionally) the iroh sync endpoint and LAN portal (hub). Single instance enforced by a lock file.
- `blirp hook <agent> <event>` - short-lived; reads hook JSON from stdin, POSTs to daemon, prints injection output where the agent supports it. Always exits 0.
- `blirp mcp` - stdio MCP server spawned by agents. Reads the local SQLite directly in read-only mode for queries; writes (e.g. `mem_record`) go through the daemon API.
- `blirp <cli>` - `status`, `open` (opens UI/portal in browser with a login link), `mem search|brief|show`, `sessions`, `pair`, `hub`, `service install|uninstall`, `hooks install|uninstall|status`, `doctor`.
- Desktop app - on launch ensures the daemon is running (spawns sidecar `blirp daemon --detach` if not), reads runtime.json, opens a window at `http://127.0.0.1:<port>/auth?token=...` which sets an HttpOnly cookie and redirects to `/`. Closing the window does not stop the daemon or sessions.

Autostart: `blirp service install` registers per-user autostart: macOS LaunchAgent, Linux `systemd --user` unit, Windows `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` entry. Never a system service (it would not see the user's agent logins).

## 5. Data model (SQLite, `blirp-core::store`)

All ids are UUIDv7 strings unless stated. Timestamps are integer unix milliseconds. Migrations are embedded and versioned (`PRAGMA user_version`). WAL mode, `foreign_keys=ON`, `busy_timeout=5000`.

```sql
machines(id TEXT PK,            -- iroh NodeId (z32) or local uuid before identity exists
         name TEXT, os TEXT, role TEXT CHECK(role IN ('standalone','node','hub')),
         last_seen INT, revoked INT DEFAULT 0)

projects(id TEXT PK, name TEXT NOT NULL, created_at INT, updated_at INT, deleted INT DEFAULT 0)
project_paths(project_id TEXT REFERENCES projects, machine_id TEXT, path TEXT,
              git_remote TEXT NULL,          -- normalized (host/owner/repo), null when not git
              PRIMARY KEY(machine_id, path))

sessions(id TEXT PK, project_id TEXT, machine_id TEXT,
         agent TEXT,                  -- claude|codex|opencode|pi|gemini|cursor|amp|aider|dsh|shell|custom:<name>
         agent_session_id TEXT NULL,  -- the agent's own id (claude uuid, codex rollout uuid, ...)
         origin TEXT CHECK(origin IN ('blirp','external')),
         cwd TEXT, title TEXT NULL,
         status TEXT CHECK(status IN ('starting','working','idle','waiting','completed','failed','detached')),
         branch TEXT NULL, worktree TEXT NULL,
         transcript_path TEXT NULL,
         started_at INT, ended_at INT NULL, last_activity_at INT,
         exit_code INT NULL,
         summary_json TEXT NULL,      -- distill output (see §8)
         distilled_through_seq INT DEFAULT 0,
         tokens_in INT DEFAULT 0, tokens_out INT DEFAULT 0, cost_usd REAL DEFAULT 0,
         parent_session_id TEXT NULL, -- fork / continue-in lineage; parent of an ingested subagent
         UNIQUE(agent, agent_session_id))

events(session_id TEXT, seq INT, ts INT,
       kind TEXT CHECK(kind IN ('user','assistant','tool_call','tool_result','system','file_edit','summary')),
       text TEXT,                     -- redacted, human-readable
       meta_json TEXT NULL,           -- tool name, file paths, token usage, etc.
       PRIMARY KEY(session_id, seq))
events_fts USING fts5(text, content='events', content_rowid='rowid', tokenize='porter unicode61')

records(id TEXT PK, project_id TEXT,
        kind TEXT CHECK(kind IN ('decision','plan','note','open_thread','gotcha')),
        title TEXT, body TEXT, status TEXT CHECK(status IN ('active','resolved','archived')),
        pinned INT DEFAULT 0, source_session_id TEXT NULL,
        created_at INT, updated_at INT, updated_by TEXT)   -- 'user' | 'distiller' | machine id
records_fts USING fts5(title, body, content='records', ...)

briefs(project_id TEXT PK, body_md TEXT, version INT, updated_at INT, updated_by TEXT)
brief_history(project_id TEXT, version INT, body_md TEXT, updated_at INT, updated_by TEXT,
              PRIMARY KEY(project_id, version))

wiki_pages(id TEXT PK, project_id TEXT, slug TEXT, title TEXT, body_md TEXT,
           updated_at INT, updated_by TEXT, deleted INT DEFAULT 0, UNIQUE(project_id, slug))
suggestions(id TEXT PK, project_id TEXT, target TEXT CHECK(target IN ('brief','record','wiki')),
            target_id TEXT NULL, proposal_json TEXT, rationale TEXT, source_session_id TEXT NULL,
            status TEXT CHECK(status IN ('pending','accepted','rejected','dismissed')), created_at INT, decided_at INT NULL)
resources(id TEXT PK, project_id TEXT, kind TEXT CHECK(kind IN ('link','repo','pr','issue','doc','file')),
          url TEXT, title TEXT, meta_json TEXT NULL, created_at INT, deleted INT DEFAULT 0)

ingest_cursors(adapter TEXT, source TEXT, cursor_json TEXT, PRIMARY KEY(adapter, source))
settings(key TEXT PK, value_json TEXT)
devices(id TEXT PK, name TEXT, kind TEXT CHECK(kind IN ('machine','browser')),
        token_hash TEXT NULL, node_id TEXT NULL, created_at INT, last_seen INT,
        revoked INT DEFAULT 0, can_control_terminals INT DEFAULT 0)

-- replication (§10)
outbox(origin_seq INTEGER PRIMARY KEY AUTOINCREMENT, entity TEXT, op TEXT, key TEXT, payload_json TEXT, ts INT)
sync_state(peer TEXT PK, last_pushed_origin_seq INT, last_pulled_hub_seq INT)
hub_log(hub_seq INTEGER PRIMARY KEY AUTOINCREMENT, origin_machine TEXT, origin_seq INT,
        entity TEXT, op TEXT, key TEXT, payload_json TEXT, ts INT,
        UNIQUE(origin_machine, origin_seq))            -- only populated on the hub
```

All writes to replicated entities (`projects`, `project_paths`, `sessions`, `events`, `records`, `briefs`, `wiki_pages`, `resources`, `machines`) go through `Store::apply(Change)` which writes the row and appends to `outbox` in one transaction. Nothing else may write those tables.

### Project resolution

`resolve_project(machine_id, cwd)`:
1. Canonicalize cwd. Walk up to find the nearest registered `project_paths.path` that is a prefix; if found, use it.
2. Else, if cwd is inside a git work tree, use the git top-level as the project root; if another machine already has a project with the same normalized `git_remote`, attach this path to that project; else create a project named after the folder.
3. Else (no git) use cwd itself as the project root and create a project named after the folder. Exception: the user's home directory and filesystem roots are never auto-registered as project roots; sessions there go to a per-machine "Home" project.
4. Users can register folders explicitly (Projects > Add folder), rename, merge two projects (moves paths, sessions, records), or split a path off.

Worktree sessions resolve to the parent repo's project (git common dir).

Implementation notes: a subfolder of an unregistered repo registers the repo top level (step 2). The Home project is named `Home (<machine name>)`, has no `project_paths` rows (so it never captures subfolders by prefix) and its id is kept in the local `settings` key `home_project_id`. git runs as the `git` CLI with a timeout; when git is missing every folder is treated as non-git.

## 6. PTY supervisor (`blirp::pty`)

- `portable-pty` (vendored if needed for fixes). Windows: ConPTY. Ship `conpty.dll` + `OpenConsole.exe` next to the binary when available; fall back to the system ConPTY.
- Each live terminal: child process, master reader task, writer, `vt100::Parser` holding screen state (scrollback 10 000 lines), subscriber broadcast channel.
- Attach protocol (WS `/api/terminals/:id/ws`): server first sends a `snapshot` frame (formatted screen contents reproducing the current screen, including alt-screen state, cursor position and title), then streams raw output bytes. Client sends `input` (bytes), `resize {cols, rows}`. Multiple clients may attach; last resize wins.
- Framing (types `TerminalServerMessage` / `TerminalClientMessage` in `types.gen.ts`):
  - server -> client **text** frames are JSON: `{"type":"snapshot","cols","rows","data"}` (reset the terminal, then write `data`; it starts with `ESC c`, replays scrollback lines on the normal screen, redraws the screen and restores input modes, cursor and title; sent first and again whenever the client fell behind), `{"type":"resize","cols","rows"}` (another client resized), `{"type":"exit","status","exit_code"}` (process ended; the socket closes).
  - server -> client **binary** frames are raw PTY output bytes (may split UTF-8 sequences; feed them to the terminal as bytes).
  - client -> server **binary** frames are raw input bytes; **text** frames are JSON `{"type":"input","data":"..."}` or `{"type":"resize","cols":N,"rows":N}` (1-1000 each). Frames are capped at 1 MiB.
  - A live terminal that has exited is gone: attaching returns 404 `terminal_not_found`; use the session's status and events instead.
- The daemon answers terminal queries itself when no client is attached (DSR cursor position `ESC[6n`, DSR status `ESC[5n`, primary DA `ESC[c`) so ConPTY and TUIs never block at startup. Attached clients (xterm.js) answer them instead.
- Kill = terminate the whole process tree (Windows job object + ClosePseudoConsole; Unix process group SIGHUP then SIGKILL after 3 s). The Windows job has `KILL_ON_JOB_CLOSE`, so sessions also end if the daemon dies; after a normal exit the rest of the tree is reaped the same way (like a terminal hangup).
- Activity tracking: `last_output_at`; status heuristics in §7.

## 7. Sessions and agents (`blirp::agents`)

An `AgentSpec` describes each agent: binary name(s), how to detect it on PATH, launch args, resume args, env injection, hook/MCP integration, transcript adapter. Custom agents are any command line from config (no memory ingest, but still tracked with status and injection via `BLIRP_MEMORY_FILE` env and AGENTS.md if the user enables it).

Launch (`POST /api/sessions`): body `{project_id | cwd, agent, prompt?, worktree?: bool, continue_from?: session_id, machine?: id}`:
1. Resolve project + cwd. If `worktree` and project is git: `git worktree add ~/.blirp/worktrees/<project>/<name> -b blirp/<name>`; name is `adjective-animal-xxxx`.
2. Create session row (`starting`, origin `blirp`).
3. Render memory injection (§9) to `~/.blirp/launch/<id>/memory.md`.
4. Build argv/env per agent:
   - claude: `claude --session-id <new uuid> --settings <launch/settings.json> --mcp-config <launch/mcp.json>` (settings contains blirp hooks for SessionStart, UserPromptSubmit, Stop, Notification, SessionEnd, PreCompact). Store the uuid as `agent_session_id` immediately.
   - codex: `codex -c mcp_servers.blirp.command=... -c mcp_servers.blirp.args=[...]` plus the best available context mechanism for the installed version (SessionStart hook for >= 0.155.1, else developer instructions / experimental instructions file override via `-c`); verify against `codex --help` of the installed version at runtime and degrade gracefully.
   - opencode, pi, gemini, cursor, amp, aider, dsh: per §9 table.
   - Env always: `BLIRP_SESSION_ID`, `BLIRP_PROJECT_ID`, `BLIRP_HOME`, `BLIRP_MEMORY_FILE`, plus `TERM=xterm-256color`, `COLORTERM=truecolor`. Parent-agent markers (`CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT`, `CODEX_SANDBOX`) are removed so a daemon started from inside an agent does not leak them.
   - shell: `$SHELL` (unix, else `/bin/sh`); Windows `pwsh`, else `powershell`, else `%ComSpec%`.
   - Binaries resolve on PATH; on Windows only `.exe/.com/.cmd/.bat/.ps1` count (npm's extensionless shims are skipped), `.cmd/.bat` run via `cmd /d /c`, `.ps1` via PowerShell `-File`.
   - Resume (`POST /api/sessions/:id/resume`, same session row) with `agent_session_id`: claude `--resume <id>`, codex `resume <id>`, opencode `--session <id>`, gemini `--resume <id>`, cursor `--resume <id>`, amp `threads continue <id>`; pi, aider, dsh, shell and custom agents (or no known id) relaunch fresh in the session folder.
5. Spawn PTY; if `prompt` given, type it after the agent is ready (first output idle for 1.5 s, at most 60 s) followed by Enter. The prompt is sent as a bracketed paste when the application enabled bracketed paste mode.

Status:
- From hooks when available (claude: UserPromptSubmit -> working, Stop -> idle, Notification(permission/idle prompt) -> waiting, SessionEnd -> completed).
- Else heuristics: output within the last 2 s -> working; otherwise idle.
- Process exit: code 0 -> completed, else failed. A user Stop -> completed regardless of exit code. Daemon shutdown ends running sessions as `detached`.
- Heuristics never overwrite `waiting` (hook-owned) or a final status.
- On daemon restart, blirp-launched sessions whose process is gone become `detached` (ingested external sessions keep their status); UI offers Resume (agent resume flag with `agent_session_id`).

Continue in / fork: `continue_from` builds a handoff pack (§9) from the source session and passes it as the initial context of a new session with any agent; `parent_session_id` records lineage.

## 8. Ingest (`blirp::ingest`)

One adapter module per agent implementing:

```rust
trait Adapter {
    fn id(&self) -> &'static str;
    fn roots(&self) -> Vec<PathBuf>;                        // dirs to watch, per OS, honoring env overrides
    fn scan(&self, store: &Store) -> Result<Vec<Source>>;   // discover transcript sources
    fn ingest(&self, src: &Source, cursor: Option<Cursor>, sink: &mut dyn EventSink) -> Result<Cursor>;
}
```

- Watch roots with `notify` (debounced 500 ms) plus a full rescan every 5 min and at startup. Only roots that exist are watched; the rescan adds roots created later (agent installed after blirp). A watcher event re-reads just the changed known file; anything else (new file, database) rescans that adapter.
- Passes run on the blocking pool, one at a time (adapters of a pass in parallel, sources of one adapter sequentially, most recently modified first), never on the async runtime or the PTY path. Events are written in `Store::ingest_tx` transactions of at most 1000 events with `apply` semantics (row + outbox); a source's cursor is stored in the transaction of its last batch. Shutdown stops a pass between events.
- Incremental: a `Source` carries a fingerprint (size + mtime for files, `time_updated` for database rows); an equal fingerprint in the stored cursor skips the source. JSONL cursors store byte offset + line count guarded by file identity (creation time / inode) and a hash of the first 256 bytes; only complete lines are consumed (a final line without newline counts once the file has been quiet for 60 s). SQLite stores keep the last consumed `(time_created, id)`; JSON files the message count. Truncation, rotation or a changed head -> re-read from 0, dedupe on `(session_id, seq)` (so a file rewritten with *different* content at the same positions is not re-imported; agents append, so this only affects hand-edited files).
- Seqs are deterministic for the same source content and increase with arrival (the distiller consumes events past `distilled_through_seq`): line-oriented sources use `line_index * 1024 + n`; sources without stable lines (gemini, cursor `store.db`, opencode) keep a counter in the cursor and dedupe by message / tool-call / blob id.
- Handles compressed Codex rollouts (`.jsonl.zst`) and dsh logs (`.jsonl.zstd`) via `zstd` (appended frames are read incrementally; a torn last frame waits).
- Normalizes into `sessions` + `events`. `text` is human-readable and redacted (`blirp_core::redact`, applied to text, titles and meta); tool calls are `tool(args preview)` in text with full (redacted) args in `meta_json`; tool results are cut to 4 KiB of text, other text to 64 KiB; `meta_json` is capped at 16 KiB. Edit/write tool calls (and `apply_patch` payloads) add one `file_edit` event per path (`meta.path`). Compaction summaries are `summary` events; injected context (Codex `<environment_context>`, Claude local-command output) is `system`. Reasoning/thinking is not stored.
- Sessions: `project_id` from `resolve_project` on the transcript's cwd (`resolve_project_lenient` when the folder no longer exists: prefix match, else a folder project at the recorded path; no cwd -> Home project). Title: the agent's own title (Claude `ai-title`, opencode/pi/amp/cursor names, Gemini summary, dsh title), else the first user prompt (first line, 80 chars); set only while the title is empty, so a distiller or user title is never overwritten. Tokens and cost are absolute totals: the transcript's own cost when it has one (Claude `cost-state`, opencode, pi, aider), else an estimate from the dated static price table in `ingest/pricing.rs` (unknown models cost 0). `started_at` / `last_activity_at` come from event timestamps (or the store's own update time); external sessions are `working` while their source changed in the last 2 min, else `completed` with `ended_at = last_activity_at`; a 30 s sweep completes external sessions that went quiet.
- Subagents are child sessions with `parent_session_id` set: Claude `<sessionId>/subagents/agent-<id>.jsonl` (`agent_session_id = "<sessionId>:agent-<id>"`, title from its `.meta.json`), opencode `session.parent_id`, Gemini `chats/<parentId>/`, Cursor `subagents/`. Claude tool output spilled to `tool-results/*.txt` is linked by path in the result's `meta.result_file` with at most a 4 KiB preview.
- Links to blirp-launched sessions: by `agent_session_id` (claude, set at launch), else by (agent, same folder, first event within 60 s after launch, closest launch wins) on this machine; linking sets `agent_session_id`. A linked or blirp-owned row keeps its `origin`, `status`, `started_at` / `ended_at`, `worktree` and `cwd`; ingest only fills the title (if empty), tokens, cost, transcript path, branch and last activity.
- Sessions started outside blirp are ingested as origin `external`; the daemon's restart handling (`detached`) applies to blirp-launched sessions only.
- Transcripts whose cwd is inside `BLIRP_HOME` (except `worktrees/`) are blirp's own background runs (e.g. the distiller's scratch dir) and are skipped.
- Retention: history is permanent. A source file or database row that disappears never deletes blirp rows.
- Server events: `session_created`, `session_updated` (at most one per session per second; a suppressed update is sent once its second has passed) and `project_updated` for projects created by resolution.
- `blirp doctor` prints one line per adapter: root found, source count, last ingest time (settings keys `ingest.<adapter>.last_at` and `.sources`).
- Each adapter ships fixture transcripts under `crates/blirp/tests/fixtures/<agent>/` and a test asserting the normalized output (`crates/blirp/tests/ingest.rs`).

| Agent | Source (defaults; honor env overrides) | Format verified against |
|---|---|---|
| claude | `$CLAUDE_CONFIG_DIR/projects` or `~/.claude/projects/*/*.jsonl`; subagents `*/<sessionId>/subagents/*.jsonl` | real data |
| codex | `$CODEX_HOME` or `~/.codex/{sessions/YYYY/MM/DD,archived_sessions}/rollout-*.jsonl[.zst]`; conversation from `response_item`, cumulative usage from `token_usage_record` / `token_count` | real data |
| opencode | `$XDG_DATA_HOME/opencode` or `~/.local/share/opencode/opencode*.db` (Windows also `%APPDATA%\opencode`); one source per `session` row; opened `mode=ro` (not `immutable`, it is live), `SQLITE_BUSY` retried. The legacy JSON `storage/` tree is not read (opencode migrates it into the database) | real data |
| pi | `$PI_CODING_AGENT_DIR/sessions` or `~/.pi/agent/sessions/--<cwd>--/<ts>_<id>.jsonl`; every tree entry kept in file order | published format (v3) |
| gemini | `$GEMINI_CLI_HOME/.gemini` or `~/.gemini/tmp/<project>/chats/session-*.json[l]` (+ `chats/<parentId>/*.jsonl`); cwd from `tmp/<project>/.project_root` or `projects.json`; messages re-appended with the same id are deduped | gemini-cli source |
| cursor | `$CURSOR_CONFIG_DIR` or `~/.cursor/chats/<hash>/<id>/store.db` (preferred: blob tree from `meta.latestRootBlobId`, cwd from the sibling `meta.json`) and `~/.cursor/projects/<encoded-cwd>/agent-transcripts/**/*.jsonl` (no timestamps; cwd decoded from the folder name). The Cursor editor's `state.vscdb` is not ingested | transcripts: real (sparse) data; store.db: community documentation |
| amp | `$AMP_DATA_DIR` or `~/.local/share/amp/threads/T-*.json` (Windows also `%APPDATA%\amp`); a streaming last message waits | public format descriptions |
| aider | `.aider.chat.history.md` in this machine's registered project folders (periodic rescan only, not watched) | aider docs |
| dsh | `$DSH_HOME` or `~/.dsh/sessions/<cwd>/session-<id>/session.v3.jsonl.zstd` (other log versions are skipped) | real data (decoded) |

Unknown or changed formats must never crash the daemon: log once per source, skip the line, continue.

## 9. Memory (`blirp::memory`)

### Redaction (`blirp-core::redact`)
Applied to every event text and meta before storage, sync or summarization. Rule set: gitleaks-compatible regexes compiled once (AWS, GCP, Azure, GitHub, GitLab, Slack, Stripe, OpenAI, Anthropic, generic `api_key|secret|token|password` assignments with high-entropy values, private key blocks, JWTs, connection strings with credentials, `.env`-style lines). Replacement `[REDACTED:<kind>]`. Unit tests for every rule with positive and negative cases.

### Distill
Trigger: session `idle` for `memory.distill_idle_secs` (default 300) with new events past `distilled_through_seq`, or session ended. One job at a time; daily budget `memory.daily_distill_limit` (default 40 jobs).

Summarizer backends (config `memory.summarizer`, default `auto`): `claude` (`claude -p --model haiku --output-format json`), `codex` (`codex exec --json`), `ollama` (HTTP `localhost:11434`, model configurable), `none`. `auto` picks the first available in that order. The summarizer process runs in an empty scratch dir with `BLIRP_DISTILLING=1` (blirp hooks exit immediately when set) and `--strict-mcp-config`/equivalent so repo hooks and MCP servers do not run.

Input: redacted, compacted transcript (user prompts verbatim; assistant text; tool calls as one-line `tool(args preview)`; tool results truncated), capped at `memory.distill_max_chars` (default 60 000, keep head 20% + tail 80%), plus the current brief and active records of the project.

Output (strict JSON, validated with serde; one retry on invalid, then mark failed):
```json
{ "title": "short session title",
  "summary": "3-6 sentences",
  "decisions": [{"title": "...", "body": "..."}],
  "open_threads": [{"title": "...", "body": "..."}],
  "resolved_record_ids": ["..."],
  "gotchas": [{"title": "...", "body": "..."}],
  "files": ["relative/paths"],
  "brief_md": "full replacement brief, <= 1500 tokens" }
```
Apply: session title (if unset) + summary_json; new records; resolve records; brief: if `memory.brief_mode = auto` (default) write new version to `briefs` (history kept, user can revert); if `review`, create a `suggestion` instead. Records edited by the user (`updated_by = user`) are never modified by the distiller, only suggested.

### Inject
`render_injection(project, agent, session?) -> String` (markdown, hard cap `memory.inject_max_chars`, default 8000):
```
# blirp memory: <project name>
<brief>
## Open threads      (active open_thread records, pinned first, max 10)
## Recent decisions  (max 8)
## Gotchas           (max 5)
## Recent sessions   (last 3: date, agent, machine, title, summary)
Tools: search older history with the blirp MCP tools (mem_search, mem_session, mem_recent, mem_record) or `blirp mem search "<query>"`.
```
The section order is stable and content changes only when memory changes, so agent prompt caches keep hitting.

| Agent | Session-start injection | On-demand |
|---|---|---|
| claude | SessionStart hook -> `hookSpecificOutput.additionalContext` (sources startup/resume/clear/compact) | MCP |
| codex | SessionStart hook if supported by installed version, else instructions override via `-c` | MCP |
| gemini | SessionStart hook additionalContext | MCP |
| cursor | `sessionStart` hook `additional_context` | MCP |
| opencode | `instructions` entry pointing at `BLIRP_MEMORY_FILE` via `OPENCODE_CONFIG_CONTENT`/config override | MCP |
| pi | `--append-system-prompt`-equivalent / APPEND_SYSTEM.md in launch dir via `PI_CODING_AGENT_DIR` overlay if supported; else extension | CLI |
| amp | AGENTS.md is not touched; pass memory file per its CLI options if available | MCP |
| aider | `--read <memory.md>` | none |
| dsh | AGENTS-style instructions if supported; else none | MCP |

Launch-time integration (inside blirp) never edits user files. Global integration for sessions started outside blirp is opt-in (`blirp hooks install`, onboarding checkbox): it writes clearly marked entries (`"command": "blirp hook ..."`) into the agent's user config, is idempotent, and `blirp hooks uninstall` removes exactly those entries. Existing hooks (e.g. other-memory) are preserved.

### Handoff pack (continue in / fork)
Brief + the source session's summary + its last N (default 12) user/assistant turns + files touched + open threads, capped at 12 000 chars, delivered as the new session's injected context plus an initial prompt "Continue the work described in the blirp handoff above."

### MCP server (`blirp mcp`, stdio; also Streamable HTTP at `/mcp` on the daemon)
Built with the official Rust SDK `rmcp`. Tools:
- `mem_search {query, project?: "current"|"all"|id, kinds?, limit?}` -> ranked hits (events + records) with session id, date, agent, snippet.
- `mem_session {session_id, from_seq?, limit?}` -> session summary + events page.
- `mem_recent {project?, limit?}` -> recent sessions with summaries.
- `mem_brief {project?}` -> current brief + records.
- `mem_record {kind, title, body}` -> create a record in the current project (via daemon API).
Current project = resolved from `BLIRP_PROJECT_ID` env or the MCP client's cwd.

## 10. Sync (`blirp-sync`)

Roles: `standalone` (default), `hub`, `node`. Transport: iroh (QUIC, NAT traversal, relay fallback; custom relay URL configurable; local-network discovery enabled). ALPNs: `blirp/pair/1`, `blirp/sync/1`, `blirp/proxy/1`.

Pairing:
1. Hub: `blirp hub enable` (or UI) -> shows invite `blirp1-<base32 ticket>` + 8-char code `XXXX-XXXX` (+ QR of `blirp://join/<ticket>#<code>`). Code valid 10 minutes, single use.
2. Node: `blirp pair <invite> <code>` (or UI; on LAN the hub is discoverable so only the code is needed). Connects via `blirp/pair/1`, runs SPAKE2 (spake2 crate, Ed25519 group) with the code as password, then both sides exchange node ids + machine metadata authenticated by a MAC keyed from the SPAKE2 session key.
3. Hub stores the node in `devices` (kind machine) and `machines`; node stores the hub id in config. From then on `blirp/sync/1` and `blirp/proxy/1` connections are accepted only from known, non-revoked node ids.

Replication (`blirp/sync/1`):
- Push: node sends outbox entries after `last_pushed_origin_seq` in batches (<= 500 entries or 4 MiB). Hub inserts into `hub_log` (idempotent on `(origin_machine, origin_seq)`), applies to its own tables, acks the highest origin_seq.
- Pull: node requests `hub_log` after `last_pulled_hub_seq`, skips entries it originated, applies them in order.
- Conflict rule: rows are applied as last-writer-wins by hub_seq order. `events` are append-only and keyed by (session_id, seq). Local deletion of an agent transcript never deletes anything (ingest-only).
- Live: after catch-up, the stream stays open; hub pushes new entries as notifications. Offline nodes queue in outbox.
- Hub itself is also a normal machine with its own sessions.

Remote proxy (`blirp/proxy/1`): the hub (or any node) can forward an API request or a terminal WS to another machine: request frame `{method, path, headers, body}` answered by the target daemon's own axum router (in-process tower call), WS frames tunneled as a bidirectional stream. Terminal input over proxy requires the requesting device to have `can_control_terminals`.

## 11. HTTP API (daemon, axum)

Auth: local clients send `Authorization: Bearer <runtime token>` or the `blirp_session` cookie set by `/auth?token=`. LAN/portal browser devices use device cookies (§13). All JSON; validation errors return 400 `{error:{code,message}}`.

```
GET  /api/health                         {version, machine, role}
GET  /api/machines                       list; DELETE /api/machines/:id (revoke)
GET  /api/projects                       list with path(s), git flag, session counts, last activity; GET /api/projects/:id one
POST /api/projects                       {path, name?} register folder
PATCH/DELETE /api/projects/:id           rename / soft delete; POST /api/projects/:id/merge {into}
GET  /api/projects/:id/memory            brief, records, recent sessions
PUT  /api/projects/:id/brief             {body_md}; GET .../brief/history; POST .../brief/revert {version}
CRUD /api/projects/:id/records[/:rid]
CRUD /api/projects/:id/wiki[/:slug]
CRUD /api/projects/:id/resources[/:id]
GET  /api/projects/:id/suggestions       POST /api/suggestions/:id/{accept|reject|dismiss}
GET  /api/projects/:id/git               {is_git, branch, status[], ahead/behind}; 404 `not_git` when the folder is not a repo ; GET .../git/diff?path=
GET  /api/projects/:id/files?path=       directory listing (read-only) ; GET .../files/content?path= (text, <= 1 MiB)
                                         files and git take optional `root=` (one of the project's folders here);
                                         paths are relative, `..`/absolute paths and symlinks escaping the root are rejected
GET  /api/sessions?project=&status=&agent=&machine=&q=&cursor=
POST /api/sessions                       launch (§7)
GET  /api/sessions/:id                   detail incl. summary; GET .../events?after=&limit=
POST /api/sessions/:id/stop | /resume | /distill
PATCH /api/sessions/:id                  {title}
GET  /api/terminals/:id/ws               terminal attach (§6)
GET  /api/search?q=&project=&kind=       FTS over events + records
GET  /api/agents                         detected agents + versions + integration status
POST /api/hooks/:agent/:event            hook ingress (from `blirp hook`)
GET  /api/inject?session=&cwd=&agent=    rendered injection
GET  /api/settings ; PATCH /api/settings  {config: Config, values: {key: json}}; PATCH {config?: full Config
                                         (validated, written to config.toml), values?: {key: json|null}}
POST /api/sync/hub/enable ; POST /api/sync/invite ; POST /api/sync/join {invite, code} ; GET /api/sync/status
POST /api/devices/browser-invite         one-time QR login for phone/browser (hub)
GET  /api/events/ws                      server push: session status changes, new sessions, memory updates
GET  /mcp                                MCP Streamable HTTP
GET  /*                                  embedded SPA
```

Request/response DTOs are defined in `blirp-core::model` and exported to `web/src/lib/api/types.gen.ts` (`cargo test -p blirp-core export_bindings`). Endpoints owned by later phases (`/api/hooks/*`, `/api/inject`, `/api/sync/*`, `/api/devices/*`, `DELETE /api/machines/:id`, `POST /api/sessions/:id/distill`, `/mcp`, and `continue_from`/`machine` on launch) answer 501 `not_implemented` until implemented. `/api/events/ws` pushes JSON `ServerEvent` frames: `session_created`, `session_updated`, `project_updated`, `memory_updated {project_id, part}`, and `resync` when the client fell behind and must refetch.

## 12. Config (`~/.blirp/config.toml`, validated at startup; unknown keys are an error with a clear message)

```toml
[daemon]   port = 47770
[machine]  name = "<hostname>"
[agents]   default = "claude"
           [[agents.custom]] name = "..." command = "..." args = [] 
[sessions] worktree_default = false
[memory]   summarizer = "auto" | "claude" | "codex" | "ollama" | "none"
           ollama_model = "qwen2.5:7b"
           distill_idle_secs = 300
           daily_distill_limit = 40
           brief_mode = "auto" | "review"
           inject_max_chars = 8000
           distill_max_chars = 60000
[sync]     role = "standalone" | "hub" | "node"
           hub = "<node id>"
           relay = "default" | "disabled" | "<url>"
[portal]   lan = false          # hub: serve portal on LAN with HTTPS
           lan_port = 47771
```

## 13. Portal and remote browser access

- Local desktop and `blirp open`: localhost + token cookie.
- Hub with `portal.lan = true`: axum-server with rustls on `0.0.0.0:lan_port`, self-signed cert generated with `rcgen` and persisted; fingerprint shown in the UI. Browser devices log in by scanning a one-time QR (5 min, single use) shown on an already-authenticated screen, which issues a long-lived device cookie (random 256-bit token, stored hashed). Devices listed and revocable in Settings > Devices. Terminal control from a browser device requires `can_control_terminals`.
- Users with Tailscale can instead run `tailscale serve` in front of the hub port (documented).
- Security headers: CSP (self only, no inline scripts), `X-Frame-Options: DENY`, `SameSite=Strict` cookies, CSRF protection via same-site cookie + `Origin` check on mutations and WS upgrades.

## 14. Web UI (Svelte 5, TypeScript strict)

Layout mirrors the reference (Xirp-style):
- Top bar: blirp logo, **Projects**, **Sessions**, orange **+** (new session dialog: project/folder, agent, optional prompt, worktree toggle only for git projects, machine picker when synced). Right: grid view toggle, machine/connection indicator, command palette (Ctrl/Cmd+K), settings.
- Sessions view: breadcrumb `<project> / Sessions`; left sidebar grouped by project (and by repo/subfolder for multi-root projects) with session cards: title, branch or folder, status chip (Working/Idle/Waiting/Completed/Failed/Detached) with colors, "+" per group. Main pane: the live terminal (xterm.js 6 + WebGL addon, fit addon, web-links addon, unicode11). Session toolbar: agent chip with "Continue in..." menu, elapsed time, open folder, open in editor, fork, Stop / Resume. Right collapsible **Memory** panel: exactly what was injected + brief + open threads, editable.
- Grid view: all live terminals tiled; click to focus.
- Projects view: cards with path(s), git/no-git badge, last activity, session counts, machines. Project page tabs: Overview (brief, open threads, recent sessions, prompt box to start a session), Sessions, Memory (records CRUD, brief history/revert, suggestions queue), Wiki, Resources, Files, Git (only when git).
- Session detail (for ended/external sessions): transcript viewer (events), summary, resume/continue buttons.
- Search: global FTS with filters.
- Settings: agents (detected, versions, integration status, install/uninstall global hooks), memory, machines & sync (hub enable, invite/code/QR, join, devices, revoke), portal, about/updates.
- Keyboard: Ctrl/Cmd+K palette, Ctrl/Cmd+T new session, Ctrl/Cmd+G grid, Ctrl/Cmd+Left/Right switch session, Ctrl/Cmd+W close tab (does not kill; Stop kills).
- Notifications (browser Notification API) when a session becomes `waiting` or finishes while not focused.
- Themes: light and dark via CSS custom properties, follows system, toggle in settings. Accessible: keyboard reachable, visible focus, aria labels on icon buttons.

## 15. Desktop shell (Tauri 2)

- Sidecar `blirp` binary (externalBin). On start: `blirp daemon --detach` if runtime.json missing/stale; wait for `/api/health`; navigate main window to the auth URL.
- Single instance plugin; window state plugin; deep links `blirp://join/...` forwarded to the join flow; updater plugin (GitHub Releases `latest.json`, minisign pubkey in config).
- Tray icon with status and "Quit blirp (stop daemon)" vs "Close window".

## 16. Quality bar

- Rust: edition 2024, `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. No `unwrap()`/`expect()` outside tests and provably-infallible cases (comment why). Errors: `thiserror` in libraries, `anyhow` at binary edges, never silently swallowed (log with context). No `unsafe` except where a platform API demands it, with a `SAFETY:` comment.
- Web: `pnpm -C web check` (svelte-check, strict TS, no `any`), `pnpm -C web test` (vitest), `pnpm -C web build`.
- Every adapter, redaction rule, migration, the distill JSON contract, project resolution, pairing, and replication have tests. An integration test starts a daemon on a temp `BLIRP_HOME`, launches a PTY session running a shell echo, attaches over WS, and asserts snapshot + stream.
- CI matrix: windows-latest, macos-latest, ubuntu-latest.
- Logs never contain transcript text or secrets.
