# Coding-agent on-disk transcript formats (Windows, `C:\Users\alice`)

Investigation date: 2026-09-25. Read-only survey of every installed agent's local
session store, for designing a Rust ingester. All example lines below are
**sanitized**: every real string value is replaced by a type placeholder
(`<uuid>`, `<text>`, `<abs path>`, `<iso8601>`, `<int>`, `<bool>`, `<...>`), keys
and enum-like values (`type`/`role`/`subtype`) are kept verbatim, nesting is
preserved. No real prompts, code, file paths, tokens, or secrets are included.

---

## 1. Claude Code — `~/.claude/projects/`

### Directory / file naming
- One directory per **cwd**, named by taking the absolute Windows cwd and
  replacing every `:` `\` with `-` (dots kept), e.g. cwd `C:\src\my.app`
  → `C--src-my.app`. Long paths with subfolders and hyphens/spaces are
  represented as-is with dashes as separators.
- Inside a project dir: one `<sessionId>.jsonl` file per session (sessionId is
  a UUID), plus a same-named directory `<sessionId>/` holding:
  - `<sessionId>/subagents/agent-<agentId>.jsonl` + matching
    `agent-<agentId>.meta.json` — one pair per Task/Agent-tool invocation.
  - `<sessionId>/tool-results/<random>.txt` — large tool outputs spilled to
    disk and referenced back into the transcript via a
    `compact_file_reference` attachment (see below) instead of being inlined.
- A `memory/` directory sits at the **project** level (sibling to the
  session `.jsonl` files), holding project-scoped memory markdown files
  written by the agent (e.g. long-lived notes), not per-session.
- `~/.claude/history.jsonl` (top-level, not per-project): one JSON object per
  submitted prompt/slash-command across all sessions:
  `{"display":"<text>","pastedContents":{},"timestamp":<int-ms>,"project":"<abs path>","sessionId":"<uuid>"}`.
- `~/.claude/file-history/<sessionId>/` holds edit/write backups keyed by a
  short numeric id: `<n>.<sha256>.key` and `<n>.json`.

### JSONL line shape (common envelope)
Nearly every line carries: `type`, `uuid`, `timestamp` (ISO8601),
`sessionId`, `cwd` (abs path), `gitBranch`, `version`, `userType`,
`entrypoint`, and (for conversation lines) `parentUuid` + `isSidechain`.

### Distinct top-level `type` values observed (across ~10 sessions)
`user`, `assistant`, `attachment`, `system`, `file-history-snapshot`,
`file-history-delta`, `cost-state`, `bridge-session`, `mode`,
`permission-mode`, `atis-latch`, `ai-title`, `last-prompt`,
`queue-operation`.

#### `user` / `assistant` (the actual conversation)
```json
{
  "parentUuid": "<uuid|null>",
  "isSidechain": "<bool>",
  "promptId": "<uuid>",
  "type": "user",
  "message": { "role": "user", "content": "<text | array-of-blocks>" },
  "uuid": "<uuid>", "timestamp": "<iso8601>",
  "cwd": "<abs path>", "sessionId": "<uuid>", "gitBranch": "<text>",
  "version": "<text>", "userType": "external", "entrypoint": "cli"
}
```
```json
{
  "parentUuid": "<uuid>",
  "isSidechain": "<bool>",
  "message": {
    "model": "<model-id>", "id": "<text>", "type": "message", "role": "assistant",
    "content": [ /* content blocks, see below */ ],
    "stop_reason": "<text>", "stop_sequence": null,
    "usage": {
      "input_tokens": "<int>", "cache_creation_input_tokens": "<int>",
      "cache_read_input_tokens": "<int>", "output_tokens": "<int>",
      "output_tokens_details": { "thinking_tokens": "<int>" },
      "server_tool_use": { "web_search_requests": "<int>", "web_fetch_requests": "<int>" },
      "service_tier": "<text>",
      "cache_creation": { "ephemeral_1h_input_tokens": "<int>", "ephemeral_5m_input_tokens": "<int>" },
      "iterations": [ { "input_tokens": "<int>", "output_tokens": "<int>", "type": "<text>" } ]
    }
  },
  "requestId": "<text>", "type": "assistant", "uuid": "<uuid>",
  "timestamp": "<iso8601>", "effort": "<text>", "session_id": "<uuid>",
  "cwd": "<abs path>", "sessionId": "<uuid>", "gitBranch": "<text>"
}
```

Content block `type` values seen inside `message.content[]`: `text`,
`thinking` (`{type,thinking,signature}`), `tool_use`
(`{type,id,name,input,caller:{type}}`), `image`
(`{type,source:{type,media_type,data}}`) on user messages, and on user
messages replying to a tool: `tool_result`
(`{type:"tool_result",tool_use_id,content,is_error}` — `content` is either a
plain string or an array of typed blocks, e.g. `tool_reference` items for
deferred-tool listings).

#### `attachment` — sideband/system events threaded into the transcript
Wraps an inner `attachment.type`; **24 distinct inner types** observed:
`agent_listing_delta`, `auto_mode`, `bash_output_audience_note`,
`batching_reminder_sent`, `command_permissions`, `compact_file_reference`,
`credential_org`, `date`, `date_change`, `deferred_tools_delta`,
`deferred_tools_record`, `edited_text_file`, `environment`, `goal_status`,
`hook_additional_context`, `hook_success`, `instructions`,
`mcp_instructions_delta`, `model`, `prompt_snapshot`, `queued_command`,
`remote_session_change`, `session_context`, `silent_turn_reminder`,
`skill_listing`, `total_tokens_reminder`.
Notable ones:
- `hook_success`: `{type:"hook_success",hookName,toolUseID,hookEvent,content,stdout,stderr,exitCode,command,durationMs}`, plus a `rendered:[{content}]` sibling on the envelope.
- `compact_file_reference`: `{type:"compact_file_reference",filename:"<abs path under tool-results>",displayPath:"<relative path>"}` — the mechanism that spills a large tool result to `tool-results/*.txt` and leaves a pointer.
- `edited_text_file`, `environment` (`{addedNames,addedLines,removedNames,...}`) — env/context deltas.

#### `system` — turn-level lifecycle markers
`{type:"system", subtype, ...}`. Observed `subtype` values:
`compact_boundary` (marks a context-compaction point — this is Claude Code's
analogue of a "summary" record type), `stop_hook_summary`, `turn_duration`,
`away_summary`, `local_command`. `stop_hook_summary` example shape:
```json
{
  "type": "system", "subtype": "stop_hook_summary",
  "hookCount": "<int>",
  "hookInfos": [ { "command": "<text>", "durationMs": "<int>" } ],
  "hookErrors": [], "preventedContinuation": "<bool>",
  "toolUseID": "<uuid>", "sessionId": "<uuid>"
}
```

#### `file-history-snapshot` / `file-history-delta`
Track backups of files the agent edits, keyed by `messageId`; `snapshot`
holds `trackedFileBackups` (map, empty in samples); `delta` adds
`trackingPath` and a `backup:{backupFileName,version,backupTime,realParentDir}`.

#### `cost-state`
Per-session running totals: `totalCostUSD`, `totalAPIDuration*`,
`totalLinesAdded/Removed`, and `modelUsage: { "<model-id>": {inputTokens,
outputTokens, thinkingTokens, cacheReadInputTokens, cacheCreationInputTokens,
webSearchRequests, costUSD} }` — this is where per-model token/cost usage
lives at the session level (as opposed to per-turn usage on the `assistant`
line itself).

#### `mode` / `permission-mode` / `atis-latch` / `ai-title` / `last-prompt` / `bridge-session` / `queue-operation`
Small single-purpose lines: `{type,mode|permissionMode|atis|aiTitle|lastPrompt,sessionId}`;
`bridge-session` carries `bridgeSessionId, lastSequenceNum, ownerAccountUuid,
ownerOrganizationUuid` (remote/mobile sync bookkeeping); `queue-operation`
carries a queued follow-up prompt (`{type,operation,content,timestamp,sessionId}`).

### Sidechains / subagents
Subagent (Task-tool) invocations are **not** inlined as `isSidechain:true`
lines in the parent `<sessionId>.jsonl`. Instead each spawned agent gets its
own file pair under `<sessionId>/subagents/`:
- `agent-<agentId>.meta.json`: `{agentType,description,toolUseId,spawnDepth,requestShape,requestNonInteractive,model}`.
- `agent-<agentId>.jsonl`: a full transcript in the **same line schema** as
  the parent (user/assistant/attachment/...), but every line has
  `"isSidechain": true` and an extra `"agentId":"<agentId>"` field, and its
  first `user` line's `parentUuid` is `null`.

---

## 2. Codex — `~/.codex/`

### Path layout — **no `.zst` compression on this install**
`~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<YYYY-MM-DDTHH-mm-ss>-<uuid7>.jsonl`
— plain JSONL, one file per session/turn burst, date-sharded by day.
Also: `~/.codex/session_index.jsonl` (top-level index:
`{"id":"<uuid>","thread_name":"<text>","updated_at":"<iso8601>"}` per
session) and `~/.codex/history.jsonl` (`{"session_id":"<uuid>","ts":<int>,"text":"<text>"}`,
much sparser than Claude's — appears to log only a subset of turns/commands).

### Record envelope
Every line: `{"timestamp":"<iso8601>","ordinal":<int>,"type":"<line-type>","payload":{...}}`.
Distinct `(type, payload.type)` combinations observed:

| type | payload.type | purpose |
|---|---|---|
| `session_meta` | — | session/thread identity, cwd, cli_version, model_provider, base_instructions |
| `turn_context` | — | per-turn cwd, workspace_roots, sandbox/approval policy, model, collaboration_mode |
| `world_state` | — | environment/skills/personality/multi-agent-mode snapshot |
| `token_usage_record` | — | per-response + per-turn + per-thread token usage |
| `inter_agent_communication_metadata` | — | multi-agent turn-trigger flag |
| `response_item` | `message` | chat message (role, content[] of `{type:"output_text"/"input_text",text}`) |
| `response_item` | `reasoning` | `{id, summary:[], encrypted_content}` — hidden reasoning, no plaintext |
| `response_item` | `function_call` | `{name, arguments, call_id}` — classic tool call |
| `response_item` | `function_call_output` | `{call_id, output}` |
| `response_item` | `custom_tool_call` | `{id,status,call_id,name,input}` — newer tool-call shape |
| `response_item` | `custom_tool_call_output` | `{id,call_id,output:[{type,text}]}` |
| `response_item` | `web_search_call` | `{status, action:{type,query,queries[]}}` |
| `response_item` | `agent_message` | multi-agent inter-thread message (`author`,`recipient`,`content[]`) |
| `event_msg` | `task_started` / `task_complete` / `turn_aborted` | turn lifecycle, timings |
| `event_msg` | `item_completed` | mirrors a completed response item with timing |
| `event_msg` | `token_count` | `{info:{total_token_usage,last_token_usage,model_context_window}, rate_limits:{...}}` |

Example `session_meta`:
```json
{
  "timestamp": "<iso8601>", "ordinal": 0, "type": "session_meta",
  "payload": {
    "session_id": "<uuid>", "id": "<uuid>", "timestamp": "<iso8601>",
    "cwd": "<abs path>", "originator": "<text>", "cli_version": "<text>",
    "source": "<text>", "thread_source": "<text>", "model_provider": "<text>",
    "base_instructions": { "text": "<text>", "provenance": { "type": "<text>", "model": "<text>" } },
    "history_mode": "<text>", "context_window": { "window_id": "<uuid>" }
  }
}
```
Example `function_call` / `function_call_output` pair:
```json
{"timestamp":"<iso8601>","ordinal":<int>,"type":"response_item",
 "payload":{"type":"function_call","name":"<text>","arguments":"<json-text>","call_id":"<text>"}}
{"timestamp":"<iso8601>","ordinal":<int>,"type":"response_item",
 "payload":{"type":"function_call_output","call_id":"<text>","output":"<text>"}}
```
`cwd` lives in `session_meta.payload.cwd` and again per-turn in
`turn_context.payload.cwd`; ids live in `session_meta.payload.{session_id,id}`
and are echoed as `turn_id`/`root_turn_id`/`thread_id` on later records.
Multi-agent/subagent turns show up as extra `agent_message` response items
and `inter_agent_communication_metadata` records rather than a separate file
(this differs from Claude Code, which externalizes subagents to their own
file).

---

## 3. opencode — SQLite at `~/.local/share/opencode/opencode.db`

Single `better-sqlite3` database (it can grow to hundreds of MB), no JSON-tree
storage. Read with `sqlite3` in `mode=ro`.

Tables: `account`, `account_state`, `control_account`, `credential`,
`data_migration`, `event`, `event_sequence`, `message`, `migration`, `part`,
`permission`, `project`, `project_directory`, `session`, `session_context_epoch`,
`session_input`, `session_message`, `session_share`, `todo`, `workspace`.

Key ones for an ingester:
- **`project`**: `id, worktree(abs path), vcs, name, icon_url*, time_created,
  time_updated, time_initialized, sandboxes, commands`.
- **`session`**: `id, project_id, workspace_id, parent_id, slug, directory,
  path, title, version, share_url, summary_additions/deletions/files,
  summary_diffs, metadata, cost, tokens_input/output/reasoning/cache_read/
  cache_write, revert, permission, agent, model, time_created, time_updated,
  time_compacting, time_archived`. `parent_id` links subagent/child sessions
  to a parent session (opencode's sidechain mechanism).
- **`message`**: `id, session_id, time_created, time_updated, data(JSON)`.
  `data` shape by role:
  - `user`: `{role,time:{created},agent,model:{providerID,modelID},summary:{diffs:[]}}`
  - `assistant`: `{parentID,role,mode,agent,path:{cwd,root},cost,
    tokens:{total,input,output,reasoning,cache:{write,read}},modelID,
    providerID,time:{created,completed},finish}`
- **`part`**: `id, message_id, session_id, time_created, time_updated, data(JSON)`.
  `data.type` values seen: `text` (`{type,text}`), `reasoning`
  (`{type,text,time:{start,end},metadata:{openrouter:{reasoning_details[]}}}`),
  `tool` (`{type,tool,callID,state:{status,input,output,metadata,title,
  time:{start,end}},metadata}`), `file` (`{type,mime,filename,url}`),
  `step-start` (`{type}`), `step-finish` (`{reason,type,tokens,cost}`).
- **`event`**: `id, aggregate_id, seq, type, data(JSON)` — an event log;
  `type` values: `message.part.updated.1`, `message.updated.1`,
  `session.created.1`, `session.updated.1`, `message.removed.1`.
- **`todo`**: per-session TODO list (`content,status,priority,position`).

Note: `~/.opencode` (a *different* dir, project-local) only holds an npm
install for an MCP helper — the real data dir is
`~/.local/share/opencode` (also checked `%APPDATA%\opencode` and
`%LOCALAPPDATA%\opencode` — neither exists on this machine).

---

## 4. DeepSeek Harness (`dsh`) — `~/.dsh/`

### Layout
```
~/.dsh/
  sessions/<sanitized-cwd>/session-<uuid>/session.v3.jsonl.zstd   (compressed event log)
  storages/session_projcache/sessions/session-<uuid>.json          (uncompressed cache/snapshot)
  storages/workspace.json
  profiles/                                                        (own npm-installed runtime deps, not data)
  settings.yaml, .credentials.yaml, .anonymous-user-id
```
- cwd is encoded the same family of way as Claude Code but with a **double
  dash separator and leading/trailing `--`**, e.g. cwd `C:\Users\alice` →
  `--C-Users-alice--`.
- `session.v3.jsonl.zstd` — verified real Zstandard frames (magic bytes
  `28 B5 2F FD`); no `zstd`/`7z` binary or Python `zstandard` module is
  available on this machine, so the raw per-event JSONL could not be
  decompressed for this survey without installing tooling (out of scope for
  a read-only pass). The v3 filename suggests a versioned line-oriented
  event schema paralleling the uncompressed cache below.
- `storages/session_projcache/sessions/session-<uuid>.json` is an
  **uncompressed, fully structured snapshot** of the same session state
  (event-sourced key/value rows) and is enough on its own to model the
  session:
```json
{
  "version": 7,
  "record": {
    "identity": { "formatVersion": 3, "createdAt": <int-ms>, "cwd": "<abs path>", "isSeeded": "<bool>", "inheritedEventCount": <int> },
    "rows": {
      "<rowName>": { "ver": <int>, "seq": <int>, "val": "<...>" }
    }
  }
}
```
  Row names observed: `title`, `titleInput`, `llmRetry`, `sandboxMode`,
  `goal`, `tokenUsage`, `contextPressure`, `contextBreakdown`,
  `turnBoundary`, `sessionStats`, `turnOutline`, `agentPreset`,
  `subagentCatalog`, `subagentTiming`, `subagent`, `permissions`,
  `modelSelection`, `sessionListMetadata`, `imageLimits`, `todos`, `plan`,
  `subagentModelSelectionPolicy`, `inbox`.
  Notable row `val` shapes:
  - `tokenUsage.val`: `{totals:{uncachedInputTokens,outputTokens,cacheReadTokens,cacheWriteTokens}, last:{turn,step,buckets:{...same 4 fields}}}`.
  - `turnOutline.val`: `{turns:[{turn,seq,prompt,response}], draft}` — this is the closest thing to a linear transcript in the cache; full per-message detail presumably lives in the `.zstd` event log.
  - `sessionStats.val`: `{turns,steps,llmMs,toolMs,ttftMs,ttftSteps,decodeMs,decodeTokens,lastTurn,openStep,pendingCalls}`.
  - `permissions.val`: `{preset,sandbox,approval,seeded}`.
  - `modelSelection.val`: `{lastUsed:{provider,model,reasoningEffort},pending}`.
  - `subagent.val` / `subagentCatalog.val`: mostly empty on this machine (`{}` / `{inheritedEventCount}`) — subagent nesting appears to be tracked via `subagentCatalog`/`subagentTiming` rows and an `inheritedEventCount` pointer into the parent's event log rather than a separate file per subagent.

---

## 5. Gemini CLI — `~/.gemini/`

**No chat/session transcript store exists on this machine.** Contents are
config-only:
```
~/.gemini/
  GEMINI.md                    (project-memory markdown)
  settings.json                (hooks + tool config, see below)
  config/
    mcp_config.json
    projects/<uuid>.json        ({id,name,projectResources:{resources:[{gitFolder:{folderUri,defaultBranch,allowWrite}}]}})
    sidecars/                   (empty)
  other-ide*/                 (a separate Google IDE product's data, not Gemini-CLI sessions)
```
No `tmp/<hash>/chats` directory (the layout the task expected) was found
anywhere under `~/.gemini`; this install has apparently never produced a
persisted Gemini CLI conversation, or Gemini CLI stores active sessions
somewhere this survey didn't find (checked `%APPDATA%` and `%LOCALAPPDATA%`
equivalents implicitly via the home-dir walk — nothing named for Gemini CLI
session storage turned up outside `~/.gemini`).

`~/.gemini/settings.json` does show the **other-memory** plugin's hook
wiring (bonus finding, sanitized — see §7/§8 below).

---

## 6. Cursor — `~/.cursor/` + `%APPDATA%\Cursor\`

Two independent stores for two different Cursor products on this machine:

### 6a. Cursor CLI / background agent — `~/.cursor/projects/<project>/agent-transcripts/<uuid>/<uuid>.jsonl`
Directory naming under `~/.cursor/projects/`: sanitized cwd (`C:\...` →
`C-Users-...`, single-dash, no double-dash unlike Claude/dsh) for real
project folders, or an opaque literal like `empty-window` / a millisecond
timestamp id for scratch/no-folder sessions. Only a thin transcript was
present to sample; its one non-empty line:
```json
{"type":"turn_ended","status":"<text>","error":"<text>"}
```
(a fuller install would be expected to also emit turn/message/tool-call
records under the same `type` discriminator — not observed here).

### 6b. Cursor editor (VS Code fork) — `%APPDATA%\Cursor\User\globalStorage\state.vscdb` (and one per workspace under `User\workspaceStorage\<id>\state.vscdb`)
SQLite, tables: `ItemTable` (generic key/value settings), `cursorDiskKV`
(key/value blobs), `composerHeaders` (structured header table).
- **`composerHeaders`** columns: `composerId, workspaceId, createdAt,
  lastUpdatedAt, isArchived, isSubagent, recency, checkpointAt,
  subagentTypeName, value(JSON text)`. `isSubagent`/`subagentTypeName` show
  Cursor tracks subagent composers as rows in the *same* table, flagged
  rather than filed separately. `value` JSON:
  `{type,composerId,createdAt,unifiedMode,forceMode,hasUnreadMessages,
  totalLinesAdded/Removed,isDraft,isWorktree,isSpec,isProject,
  isBestOfNSubcomposer,isEphemeral,numSubComposers,referencedPlans[],
  trackedGitRepos[],workspaceIdentifier:{id},hasBlockingPendingActions,hasPendingPlan}`.
- **`cursorDiskKV`** keys: `composerData:<composerId>` (one per chat
  "composer"/session — the JSON blob mirrors `composerHeaders.value` plus
  `richText, text, fullConversationHeadersOnly[], conversationMap{},
  context:{...many selection arrays + mentions{...}}, generatingBubbleIds[],
  capabilities:[{type:<int>,data:{bubbleDataMap:"<json-text>"}}],
  modelConfig:{modelName,maxMode,selectedModels:[{modelId,parameters:[{id,value}]}]},
  subComposerIds[], subagentComposerIds[], todos[], totalLinesAdded/Removed,
  ...}`; `checkpointId:<composerId>:<uuid>` (git checkpoint refs);
  `composerVirtualRowHeights:_recentIds`. On this machine `conversationMap`
  and `fullConversationHeadersOnly` were empty for every sampled composer
  (draft/near-empty chats), so the actual per-message bubble schema
  (normally reached via `conversationMap[bubbleId]` or a separate
  `bubbleId:<composerId>:<bubbleId>` key) could not be captured — the
  `capabilities[].data.bubbleDataMap` field name suggests bubbles may also be
  indexed through a per-composer map rather than one key per bubble.
- **`~/.cursor/ai-tracking/ai-code-tracking.db`** (separate DB): tables
  `ai_code_hashes(hash,source,fileExtension,fileName,requestId,
  conversationId,timestamp,model,createdAt)`, `scored_commits(commitHash,
  branchName,scoredAt,linesAdded/Deleted,tabLines*,composerLines*,
  humanLines*,blankLines*,commitMessage,commitDate,v1/v2AiPercentage)`,
  `tracking_state(key,value)`, `conversation_summaries(conversationId,title,
  tldr,overview,summaryBullets,model,mode,updatedAt)`,
  `tracked_file_content(gitPath,content,conversationId,model,fileExtension,
  createdAt)`, `ai_deleted_files(gitPath,composerId,conversationId,model,
  deletedAt)` — this is Cursor's own AI-attribution/analytics DB, keyed off
  the same `conversationId`/`composerId`.

---

## 7. Other agent dirs found in `~/`

| dir | contents | verdict |
|---|---|---|
| `~/.agent` | `rules/*.md` only | config/rules dir, no session store |
| `~/.agents` | `.skill-lock.json`, `skills/*` | shared skill-cache dir, no session store |
| `~/.copilot` | `config.json` (first-launch marker), `logs/process-*.log` | no transcript store found; only a process log |
| `~/.otherbot` | `settings.json` (account scopes, MCP/tool permission flags), `local-exec-daemon*.json/.log` (a local execution daemon's own connection/credential files) | conversation data is not stored locally here — this looks like a thin local relay for a cloud-hosted agent; no session/transcript files found |
| `~/.other` | `defaults.json` (workflow flags: model_profile, branching_strategy, hooks.context_warnings, etc.) | config only, no session store |
| `~/.other-memory` | see below | plugin data dir, not a primary agent |

### `~/.other-memory` (other-memory plugin)
```
~/.other-memory/
  other-memory.db (+ -wal/-shm)   sqlite: observations(+fts), session_summaries(+fts),
                                  user_prompts(+fts), sdk_sessions, pending_messages,
                                  schema_versions   — derived memory, not a raw transcript
  chroma/                        local vector-store data for semantic recall
  corpora/, logs/, observer-sessions/
  settings.json                  CLAUDE_MEM_* env config (models, ports, feature flags)
  supervisor.json                {"processes":{"worker":{pid,type,startedAt}, "mcp-server":{...}, "chroma-mcp":{...}}}
  transcript-watch.json          declarative parser schema (see below) for tailing *other* agents' native transcripts
```
`transcript-watch.json` is directly relevant to this task: it is other-memory's
own hand-written schema for **tailing Codex's JSONL** (matches everything
found in §2 independently — `session_meta`/`turn_context` matched by `type`,
`user_message`/`agent_message`/tool-call variants matched by `payload.type`,
mapped to actions `session_context`, `session_init`, `assistant_message`,
`tool_use`, `tool_result`, `session-end`). Only a `codex` schema is present
in this file — Claude Code itself is ingested directly via hooks rather than
by tailing its own JSONL.

**How it installs hooks (`~/.claude/settings.json` shape):** On *this*
machine, `~/.claude/settings.json`'s hooks are currently wired to a different
plugin (`other-hooks`, not `other-memory`) — see §8 for the exact shape.
other-memory's own hook-installation mechanism is still visible and live in
`~/.gemini/settings.json` (it hooks multiple agent hosts, not just Claude
Code), with this shape (commands paraphrased):
```json
{
  "hooks": {
    "SessionStart": [ { "matcher": "*", "hooks": [
      { "name": "other-memory", "type": "command",
        "command": "<bun runtime> <other-memory worker script> hook gemini-cli context",
        "timeout": 10000 } ] } ],
    "BeforeAgent": [ { "matcher": "*", "hooks": [
      { "name": "other-memory", "type": "command",
        "command": "<bun runtime> <other-memory worker script> hook gemini-cli user-message",
        "timeout": 10000 } ] } ],
    "AfterAgent": [ { "matcher": "*", "hooks": [
      { "name": "other-memory", "type": "command",
        "command": "<bun runtime> <other-memory worker script> hook gemini-cli observation",
        "timeout": 10000 } ] } ],
    "BeforeTool": [ { "matcher": "*", "hooks": [ { "...": "same pattern, event name 'BeforeTool'" } ] } ]
  }
}
```
Pattern: one hook entry per lifecycle event (`SessionStart`, `BeforeAgent`,
`AfterAgent`, `BeforeTool`, and likely `AfterTool`/`SessionEnd`), each a
`matcher:"*"` array wrapping a single `command`-type hook that shells out to
other-memory's worker service with `hook <host-agent-name> <event-name>`. The
marketplace path the command points to (`<vendor>/plugin`) no longer
exists under `~/.claude/plugins/marketplaces` on this machine, so the
Gemini-CLI wiring is currently stale/orphaned even though the file entry
survives.

---

## 8. `~/.claude/settings.json` hooks section (current, live)

```json
{
  "hooks": {
    "SessionStart": [ { "hooks": [ { "type": "command", "command": "<node> <other-hooks session-start script>" } ] } ],
    "PostToolUse":  [ { "hooks": [ { "type": "command", "command": "<node> <other-hooks post-tool-use script>" } ] } ],
    "Stop":         [ { "hooks": [ { "type": "command", "command": "<node> <other-hooks stop script>" } ] } ],
    "SessionEnd":   [ { "hooks": [ { "type": "command", "command": "<node> <other-hooks session-end script>" } ] } ]
  },
  "statusLine": { "type": "command", "command": "<node> <other-hooks statusline script>" }
}
```
Shape: top-level `hooks` keyed by lifecycle event name; each event is an
array of `{ "matcher"?: "<glob>", "hooks": [ { "type": "command", "command": "<text>", "timeout"?: <int> } ] }`.
No `matcher` key is present on this machine's entries (they fire
unconditionally); `other-memory`'s equivalent entries (§7) additionally use
`"matcher": "*"` explicitly. Currently active hook owner is
**other-hooks**, not other-memory, for Claude Code specifically.

## 9. `~/.codex/config.toml` — hooks/mcp/notify-related keys (values sanitized)

```toml
notify = [ "<abs path to notify program>", "turn-ended" ]

[hooks.state]
[hooks.state."someplugin@market:hooks/hooks.json:session_start:0:0"]
trusted_hash = "sha256:<hex>"
[hooks.state."someplugin@market:hooks/hooks.json:user_prompt_submit:0:0"]
trusted_hash = "sha256:<hex>"
[hooks.state."someplugin@market:hooks/hooks.json:subagent_start:0:0"]
trusted_hash = "sha256:<hex>"

[mcp_servers.<name>]
command = "<abs path to executable>"
args = ["<arg>", "..."]
env_vars = ["<ENV_VAR_NAME>"]
startup_timeout_sec = <int>
[mcp_servers.<name>.env]
<ENV_KEY> = "<value>"

[marketplaces.<name>]
source_type = "local" | "git"
source = "<abs path or git url>"

[plugins."<plugin>@<marketplace>"]
enabled = <bool>

[agents]
enabled = <bool>
default_subagent_model = "<model-id>"
default_subagent_reasoning_effort = "<level>"
max_concurrent_threads_per_session = <int>

[projects.'<abs path>']
trust_level = "trusted"
```
Notes: `hooks.state` keys are of the form
`"<plugin>@<marketplace>:<hook-file>:<hook-event-name>:<index>:<index>"` and
store only a `trusted_hash` (a signature the CLI checks before running the
hook script — the actual hook script content lives inside the plugin, not in
`config.toml`). `notify` is a single `[command, event-name]` pair invoked on
`turn-ended`. `mcp_servers.*` blocks are keyed by server name and hold
`command`/`args`/`env_vars`/`env`/`startup_timeout_sec`.

---

## Summary of what a Rust ingester needs to handle

1. **Claude Code**: tail `~/.claude/projects/<sanitized-cwd>/*.jsonl` (plain
   JSONL, no compression); subagents are separate files under
   `<sessionId>/subagents/`; large tool output is off-loaded to
   `<sessionId>/tool-results/*.txt` and must be re-joined via
   `compact_file_reference`; usage/cost lives both per-turn (`assistant.message.usage`)
   and per-session (`cost-state.modelUsage`).
2. **Codex**: tail `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (plain
   JSONL, **not** zstd on current versions); discriminate on
   `(type, payload.type)`; both legacy (`function_call`) and new
   (`custom_tool_call`) tool-call shapes must be supported.
3. **opencode**: read `~/.local/share/opencode/opencode.db` via SQLite;
   `session`→`message`→`part` is the row hierarchy; `session.parent_id` is
   the subagent link.
4. **dsh**: real event log is zstd-compressed (`session.v3.jsonl.zstd`) and
   needs a zstd decoder; a usable uncompressed snapshot/cache already exists
   at `storages/session_projcache/sessions/*.json` if full fidelity isn't
   required.
5. **Gemini CLI**: no transcript store found on this machine to ingest.
6. **Cursor**: two sources — `~/.cursor/projects/*/agent-transcripts/*/*.jsonl`
   for the CLI/background agent, and `state.vscdb` (`cursorDiskKV`/
   `composerHeaders`) under `%APPDATA%\Cursor\User\{global,workspace}Storage`
   for the editor's chat; bubble-level message content wasn't observed
   populated on this machine and needs verification on a richer install.
7. **Others** (`.agent`, `.agents`, `.copilot`, `.otherbot`, `.other`): no
   session/transcript stores to ingest — config/skill dirs only.
