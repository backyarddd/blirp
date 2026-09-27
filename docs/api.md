# HTTP API

The daemon serves a JSON API, two WebSocket streams, an MCP endpoint and the UI. The UI, the CLI, hooks and the desktop app all use this API; it is also usable from scripts. Request and response types are defined in `crates/blirp-core/src/model.rs` and mirrored in TypeScript in [`web/src/lib/api/types.gen.ts`](../web/src/lib/api/types.gen.ts), which is the precise schema reference. This page summarizes them. The API is pre-1.0 and may change between minor versions.

Conventions: JSON bodies; ids are UUIDv7 strings (machine ids are hex endpoint ids); timestamps are Unix milliseconds; enums are lowercase or snake_case strings; optional fields are present as `null`. Unknown query parameters and body fields are rejected.

## Listeners and authentication

| Listener | Address | Accepts |
|---|---|---|
| Local | `http://127.0.0.1:<port>` (default 47770; actual port in `~/.blirp/runtime.json`) | `Authorization: Bearer <token>` with the `token` from `runtime.json`; WebSocket upgrades may instead carry `?ticket=` from `POST /api/ws-ticket`. No cookies. |
| LAN portal (hub with `portal.lan`) | `https://<LAN IP>:<lan_port>` | only the `blirp_device` cookie set by `GET /device-login?invite=<token>` ([portal.md](portal.md)) |
| Sync proxy | requests relayed from paired machines over the hub | authenticated by the machine's key; no credentials are forwarded |

The runtime token is 256 random bits, compared in constant time, and regenerated at every daemon start.

The local listener accepts no cookies: a cookie for `127.0.0.1` is sent to every server on that host, whatever its port. The web UI is opened as `http://127.0.0.1:<port>/#token=<token>` (`blirp open`, the desktop app); the fragment never reaches a server. The UI keeps the token in its origin's `localStorage`, removes it from the address bar and sends it as a bearer token. Browsers cannot set headers on a WebSocket, so the UI first calls `POST /api/ws-ticket` with `{path}` and opens `<path>?ticket=<ticket>`: a ticket is valid once, for 30 seconds, for that path only.

```sh
TOKEN=$(jq -r .token ~/.blirp/runtime.json); PORT=$(jq -r .port ~/.blirp/runtime.json)
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:$PORT/api/health
```

Every request runs as a principal:

| Principal | `control` | `admin` |
|---|---|---|
| local client (token or WebSocket ticket) | yes | yes |
| portal browser device | its **Terminal control** setting | no |
| request relayed from another machine | that machine's terminal control (as set on the hub) | no |

`control` is required for every change to sessions, projects and memory (launch, stop, resume, rename or delete a session, remove its worktree, distill it; register, rename, merge or delete a project; write the brief, records, wiki pages and resources, decide suggestions), to list or clone folders on a machine, and to send terminal input or upload files into a terminal (403 `control_not_allowed`). `admin` is required for `PATCH /api/settings`, hub, pairing and device management, browser login links, hook ingress, global hooks install/uninstall, claude's login token, `POST /api/sessions/:id/open`, `POST /api/daemon/shutdown`, `POST /api/update/check` and `POST /api/update/apply` (403 `admin_only`). Other endpoints need only authentication.

CSRF protection: a mutating request or WebSocket upgrade that carries an `Origin` header must have the same host as its `Host` header, else 403 `forbidden_origin`. Clients without `Origin` (CLI, curl) are not affected.

## Errors

```json
{ "error": { "code": "invalid_request", "message": "project_id or cwd is required" } }
```

| Status | Codes |
|---|---|
| 400 | `invalid_request` (validation, malformed JSON or query, unknown fields), `unknown_agent`, `not_git`, pairing errors (`wrong_code`, `invite_expired`, `too_many_attempts`, `unknown_invite`, `invite_required`, `no_hub_found`, `multiple_hubs`, ...), `invalid_invite` |
| 401 | `unauthorized`, `invalid_invite` (device login) |
| 403 | `forbidden_origin`, `control_not_allowed`, `admin_only` |
| 404 | `not_found`, `terminal_not_found`, `not_git` (git endpoints on a plain folder) |
| 405 | `method_not_allowed` |
| 413 | `file_too_large` (terminal uploads over 25 MiB), `upload_quota_exceeded` (a session's uploads over 500 MiB) |
| 409 | `conflict`, `session_live`, `not_running`, `already_running`, `nothing_to_distill`, `remote_session`, `machine_unreachable`, `machine_offline`, `paired_node`, `not_hub`, `already_synced`, `portal_disabled`, `portal_failed`, `not_node` |
| 422 | `agent_not_installed`, `worktree_failed`, `spawn_failed`, `open_failed`, global hooks install failures |
| 502 | `sync_failed` (settings were saved, but the sync endpoint could not restart) |
| 429 | `rate_limited` (device login) |
| 500 | `internal` (details only in the daemon log) |

## Endpoints

### Health, machines, search, settings, agents

| Method and path | Description |
|---|---|
| `GET /api/health` | `{version, machine: Machine, role, capabilities, keep_awake}`; `keep_awake`: this machine holds a sleep assertion because sessions run (`sessions.keep_awake`) |
| `GET /api/update` | `UpdateStatus {current, latest, available, notes_url, enabled, self_update, checked_at, error, last_update}`: whether a newer published release exists. `latest` is null when `[update] check` is off (`enabled: false`) or the check failed (`error` says why: offline, GitHub's rate limit, ...). The daemon asks GitHub at most once a day (hourly after a failure); `checked_at` (unix ms) is when it last did. `self_update`: `blirp update` can replace this binary (a script install). `last_update`: the last run of `blirp update` (not `--check`) on this machine, `{from, to, installed, ok, error, finished_at}` from `logs/update.log`, or null: `to` is null when it failed before finding the release, `installed` says whether the files were replaced (then an error means the daemon did not start again; before that, `from` is still installed). |
| `POST /api/update/check` | admin. Asks GitHub now unless it was asked in the last minute (then that answer); `UpdateStatus`. 409 `update_checks_off` when `[update] check` is off. |
| `POST /api/update/apply` | admin, local listener only (404 on the portal and proxy: no machine updates another). Starts `blirp update --version <latest>` from the installed CLI as a detached process (Linux under a systemd service: in a transient unit via `systemd-run`, whose failure is a 500) and answers 202; that process stops the daemon, replaces blirp and the desktop app and starts the daemon again (with a new runtime token), or leaves the old version running when anything fails. 409 `update_checks_off`, `not_self_update` (not a script install), `no_update` (up to date, or the check failed) or `update_in_progress`. Poll `GET /api/update` for `last_update` and `/api/health` for the restart. |
| `GET /api/machines` | `Machine[]`: `{id, name, os, role, last_seen, revoked}` of this and paired machines |
| `DELETE /api/machines/:id` | admin, hub only: revoke that machine (204); 409 `not_hub` elsewhere |
| `GET /api/machines/:id/health`, `GET /api/machines/:id/agents` | that machine's `/api/health` and `/api/agents`: answered here for this machine's id, else relayed through the hub (`machine_unreachable` when it is offline) |
| `GET /api/machines/:id/dirs?path=&hidden=` | control. `MachineDirs {machine_id, home, path, parent, entries: [{name, path, is_git}], truncated}`: the folders (never files) in `path` (absolute, default the home folder) on that machine, relayed like above. Only inside that machine's user home (403 `path_outside_home`; symlinked folders are not listed); hidden folders only with `hidden=true`; at most 2 000 entries. |
| `POST /api/machines/:id/clone` | control. `{url?, project_id?, parent?, name?}`: `git clone` on that machine into `<parent or ~/blirp>/<name or repo name>` (parent must be inside its home; 409 `already_exists`). `project_id` is resolved on the machine receiving the request to the remote URL of that project's git folder there (400 `no_git_remote`). URLs must be `https`, `http`, `ssh`, `git` or `user@host:path`; credentials in them are removed before the URL is forwarded or used. The clone runs in the background with the target user's git credentials, never prompting (`GIT_TERMINAL_PROMPT=0`, ssh `BatchMode`); 202 `CloneJob {id, machine_id, url, dest, state (running\|done\|failed), progress, error, started_at, finished_at}` |
| `GET /api/machines/:id/clone/:job` | the `CloneJob`, relayed like above; poll until `state` is not `running` |
| `GET /api/search?q=&project=&kind=&limit=` | full-text search; `kind` = `record` or `event`; `limit` default 50. `{hits: SearchHit[]}` with `kind, project_id, session_id, seq, record_id, title, agent, snippet, ts, score` |
| `GET /api/settings` | `{config: Config, values: {key: json}}`: `config.toml` plus local settings values |
| `GET /api/settings/summarizer` | what `memory.summarizer = "auto"` resolves to on this machine now: `{backend: "claude"\|"codex"\|"ollama"\|null, model, default_agent, fallback}`; `model` is `sonnet`, the Ollama model, or null for codex; `fallback` says why the default agent's own summarizer is not used (`no_backend`, `not_installed`, `not_logged_in`, `outdated`) and is null when it is. Runs local version and login probes (no model call); cached 30 s. |
| `PATCH /api/settings` | admin. `{config?: Config, base?: Config, values?: {key: json \| null}}`. `base` is the config the client edited (as read from `GET /api/settings`): only the values that differ from it are applied to the current config, so a copy read before another change never reverts it. Without `base`, `config` replaces every value. `sync.role` and `sync.hub` are never written here (hub enable/disable, join and leave own them). The result is validated (400 with the message) and written to `config.toml`; `null` deletes a value. Returns the new `SettingsView`. |
| `GET /api/agents` | `AgentInfo[]`: `{id, display_name, builtin, installed, path, version, can_resume, integration: {global_hooks, mcp, inject, detail}, auth, token}`; `auth` is `{logged_in, method}` for Claude Code (from `claude auth status`, run by the daemon with the stored login token, no model call; `method` is `oauth_token` when it logs in with one) and null otherwise; `token` is `{stored, env}` for Claude Code (a [login token](agents.md#headless-login-for-a-hub) is stored; the daemon's environment sets `CLAUDE_CODE_OAUTH_TOKEN`, which then wins) and null otherwise, never the token itself; `global_hooks`/`mcp` are `installed`, `not_installed` or `unsupported`, `inject` is `hook`, `instructions`, `flag` or `none`. Cached 60 s, and detected again when the stored token changed. |
| `POST /api/agents/:id/hooks/install`, `.../uninstall` | admin. Global integration for that agent; returns its `AgentInfo`; 422 when the agent is unsupported or a config file cannot be edited. |
| `PUT /api/agents/claude/token` | admin. `{token}`: store a `claude setup-token` token for claude sessions and the summarizer on this machine (surrounding whitespace is trimmed; 400 when it is empty, longer than 4096 characters or has spaces or control characters inside). Returns claude's `AgentInfo`, never the token. 422 `unsupported` for other agents. Relayed requests are never admin, so another machine cannot set it; run `blirp agents set-token claude` there. |
| `DELETE /api/agents/claude/token` | admin. Remove the stored token; returns claude's `AgentInfo`. |
| `POST /api/daemon/shutdown` | admin, local listener only (404 on the portal and proxy). Graceful stop; 202. |

### Projects

| Method and path | Description |
|---|---|
| `GET /api/projects` | `ProjectSummary[]`: `{project, paths[{machine_id, path, git_remote, is_git, local}], is_git, is_home, session_count, live_session_count, last_activity_at}` |
| `GET /api/projects/:id` | one `ProjectSummary` |
| `POST /api/projects` | `{path, name?}`: register a folder on this machine; 201 `ProjectSummary` |
| `PATCH /api/projects/:id` | `{name}`: rename |
| `DELETE /api/projects/:id` | unregister its folders and hide it; 204 |
| `POST /api/projects/:id/merge` | `{into}`: move everything into another project; returns the target |
| `GET /api/projects/:id/memory` | `{brief, records (active), recent_sessions}` |
| `GET /api/projects/:id/files?path=&root=` | directory listing `{root, path, entries[{name, path, kind, size, modified_at}]}` |
| `GET /api/projects/:id/files/content?path=&root=` | `{root, path, size, content}` of a UTF-8 text file up to 1 MiB |
| `GET /api/projects/:id/git?root=` | `{is_git, root, branch, head, upstream, ahead, behind, entries[{path, orig_path, index, worktree, conflicted}]}`; 404 `not_git` for plain folders |
| `GET /api/projects/:id/git/diff?path=&root=` | `{path, diff, truncated}` (working tree diff, capped at 1 MiB) |

Files and git are read-only and take paths relative to the project folder on this machine (`root` picks one when the project has several). Absolute paths, `..` and symlinks leading outside the folder are rejected, and so is anything inside blirp's own data folder (`BLIRP_HOME`, e.g. when the project folder is your home folder; session worktrees under `worktrees/` excepted): 403 `path_in_data_dir`.

### Memory

| Method and path | Description |
|---|---|
| `PUT /api/projects/:id/brief` | `{body_md}`: new brief version; returns `Brief {project_id, body_md, version, updated_at, updated_by}` |
| `GET /api/projects/:id/brief/history` | `Brief[]`, all versions |
| `POST /api/projects/:id/brief/revert` | `{id}` (a history entry's id, preferred) or `{version}` (its number, 1 = oldest; numbers can shift when older versions from another machine arrive): that version's text as a new version |
| `GET /api/projects/:id/records?status=&kind=` | `Record[]`: `{id, project_id, kind, title, body, status, pinned, source_session_id, created_at, updated_at, updated_by}` |
| `POST /api/projects/:id/records` | `{kind, title, body, status?, pinned?}`; 201 |
| `GET/PATCH/DELETE /api/projects/:id/records/:rid` | read; patch any of `kind, title, body, status, pinned`; delete (204). User edits set `updated_by = "user"`, which the distiller never overwrites. |
| `GET /api/projects/:id/wiki` | `WikiPage[]`: `{id, project_id, slug, title, body_md, updated_at, updated_by, deleted}` |
| `POST /api/projects/:id/wiki` | `{slug, title, body_md}`; 201 |
| `GET/PUT/DELETE /api/projects/:id/wiki/:slug` | read; replace `{title, body_md}`; delete (204) |
| `GET /api/projects/:id/resources` | `Resource[]`: `{id, project_id, kind, url, title, meta, created_at, updated_at, deleted}`; kind `link`, `repo`, `pr`, `issue`, `doc`, `file` |
| `POST /api/projects/:id/resources` | `{kind, url, title, meta?}`; 201 |
| `GET/PATCH/DELETE /api/projects/:id/resources/:rid` | read; patch; delete (204) |
| `GET /api/projects/:id/suggestions?status=` | `Suggestion[]`: `{id, project_id, target (brief\|record\|wiki), target_id, proposal, rationale, source_session_id, status, created_at, decided_at}` |
| `POST /api/suggestions/:id/accept`, `/reject`, `/dismiss` | decide; returns the `Suggestion` |

Text fields are limited to 256 KiB.

### Sessions

| Method and path | Description |
|---|---|
| `GET /api/sessions?project=&status=&agent=&machine=&q=&parent=&include_children=&cursor=&limit=` | `{items: Session[], next_cursor}`: live sessions (starting, working, idle, waiting) first, then by `last_activity_at`, newest first; `limit` default 50 (at most 500). Every machine's sessions are listed (paired machines' replicate here); `machine=<id>` narrows to one. `q` matches title, folder, branch and agent. `parent=<id>` returns the sessions whose `parent_session_id` is `<id>`; `include_children=true` includes subagent sessions (external sessions with a parent) in other listings, `false` leaves them out. Pass `next_cursor` back as `cursor` for the next page. |
| `POST /api/sessions` | control. Launch, body `LaunchSession`: `{agent, project_id?, cwd?, prompt?, worktree?, continue_from?, machine?, cols?, rows?}`. One of `project_id`/`cwd` is required unless `continue_from` is given. `machine` other than this one forwards the launch to that machine. 201 `Session`. |
| `GET /api/sessions/:id` | `SessionDetail`: the `Session` plus `children_count` (its subagent sessions) |
| `PATCH /api/sessions/:id` | `{title}` (up to 300 characters; `null` clears) |
| `GET /api/sessions/:id/events?after=&limit=` | `{items: Event[], next_after}`; `Event {session_id, seq, ts, kind, text, meta}` with kind `user`, `assistant`, `tool_call`, `tool_result`, `system`, `file_edit`, `summary`; `limit` default 200 |
| `POST /api/sessions/:id/stop` | control. 202; the final status arrives on the event stream. 409 `not_running` if blirp has no live process for it. Forwarded when the session runs on another machine. |
| `POST /api/sessions/:id/resume` | control. Relaunch in the same row; returns `Session`. 409 `already_running`. |
| `DELETE /api/sessions/:id` | control. 204. Only when it is not running (409 `session_live`). Deletes the session, its events, its ingested subagent sessions and its launch files; records and suggestions it produced stay, with `source_session_id` cleared. Replicated to paired machines; forwarded when the session belongs to another machine. |
| `POST /api/sessions/:id/worktree/remove` | control. `{force?}`: remove the ended session's git worktree (`git worktree remove` from the main work tree; the `blirp/<name>` branch is kept) and clear its `worktree`; returns `Session`. 409 `session_live`, `no_worktree`, `worktree_dirty` (uncommitted changes or untracked files; `force: true` discards them), `not_git` or `worktree_remove_failed`; 400 for another machine's session. Only folders under `BLIRP_HOME/worktrees` are touched. From the CLI, `blirp worktrees list` lists them and `blirp worktrees prune` removes those of ended sessions without changes. |
| `POST /api/sessions/:id/uploads?name=` | control, like typing. The raw file bytes as the body (any content type), `name` the original file name. Saves a file pasted or dropped into the session's running terminal on the machine that runs it (forwarded there through the hub for another machine's session) as `~/.blirp/uploads/<session>/<unix ms>-<name>`, with `name` reduced to a safe single file name (letters, digits, `.`, `-`, `_`; extension kept). 201 `UploadedFile {path, quoted, size}`: `path` is absolute on that machine, `quoted` is `path` as a terminal drop types it there (double quotes on Windows, backslash escapes elsewhere, unchanged when not needed). 413 `file_too_large` over 25 MiB (checked on `Content-Length` before reading, and while reading), 413 `upload_quota_exceeded` when the session's upload folder would hold more than 500 MiB; 404 `terminal_not_found` when the session is not running here. Files are deleted with the session and after 7 days. |
| `POST /api/sessions/:id/distill` | control. 202 queued; 409 `nothing_to_distill` (no events) or `remote_session` (another machine's session). Counts against the daily budget. |
| `POST /api/sessions/:id/open` | admin. `{target: "folder" \| "editor"}` opens the session folder in the file manager or a graphical editor on this machine (`$VISUAL`, `$EDITOR`, then `code`, `cursor`, `codium`, `zed`, `subl` on PATH; terminal editors such as vim or nano are skipped; none found: the file manager); 204. 422 `open_failed` when the program cannot start or fails at once (e.g. no desktop session). |

`Session`: `{id, project_id, machine_id, agent, agent_session_id, origin (blirp|external), cwd, title, status, stopped_by_user, branch, worktree, transcript_path, started_at, ended_at, last_activity_at, exit_code, summary, distilled_through_seq, tokens_in, tokens_out, cost_usd, parent_session_id}`. `status` is `starting`, `working`, `idle`, `waiting`, `completed`, `failed` or `detached`. `summary` is the distill result `{title, summary, decisions[], open_threads[], gotchas[], resolved_record_ids[], files[], backend, distilled_at, through_seq, error}` or `null`.

### Memory injection and hooks

| Method and path | Description |
|---|---|
| `GET /api/inject?session=&cwd=&agent=` | `{markdown}`: the session's launch `memory.md` if it exists, else a fresh render for the session's project or for the project containing `cwd` |
| `POST /api/hooks/:agent/:event` | admin. Hook ingress used by `blirp hook`: `{blirp_session_id?, cwd?, global, payload}` → `{session_id, additional_context}` |

### Sync, devices, portal

| Method and path | Description |
|---|---|
| `GET /api/sync/status` | `SyncStatus {role, machine_id, hub, connected, last_sync_at, pending_outbox, portal_url, portal_cert_fingerprint, relay_url}` (`relay_url`: the relay the sync endpoint is reachable through, null when relays are off or none answered yet) |
| `POST /api/sync/hub/enable` | admin. Become the hub (starts the portal if `portal.lan`); `SyncStatus`. 409 `paired_node`. |
| `POST /api/sync/hub/disable` | admin. Back to standalone; `SyncStatus`. 409 `not_hub`. |
| `POST /api/sync/invite` | admin, hub. `{invite, code, uri, expires_at}` |
| `POST /api/sync/join/preview` | admin. `{invite}` (invite, join link or `""`) -> `JoinPreview {hub_id}`: the hub's machine id read from the invite, nothing contacted; `null` for a LAN join. The hub's name is only exchanged after the code is verified. |
| `POST /api/sync/join` | admin. `{invite, code, allow_hub_control?}`; `invite` may be a `blirp://join/...` link (its code is used when `code` is empty) or `""` to find the hub on the LAN. `allow_hub_control` sets `sync.allow_hub_control` with the new role (left out: unchanged). `SyncStatus`. 409 `already_synced`. |
| `POST /api/sync/leave` | admin, node. Pushes the changes still queued (for at most 5 s), asks the hub to revoke this machine, then returns to `standalone` and resets `sync.allow_hub_control`. `LeftHub {status: SyncStatus, warning}`: `warning` is set when the hub could not be reached (it still lists this machine as paired: revoke it there) or changes made here had not reached it. Leaving never fails because the hub is down. 409 `not_node`. |
| `GET /api/devices` | `Device[]`: `{id, name, kind (machine\|browser), node_id, created_at, last_seen, revoked, can_control_terminals}` |
| `PATCH /api/devices/:id` | admin. `{can_control_terminals}`; closes the device's connections so they reopen with the new rights |
| `DELETE /api/devices/:id` | admin. Revoke; 204 |
| `POST /api/devices/browser-invite` | admin. `{url, expires_at}`: one-time portal login link (5 minutes). 409 `portal_disabled` when the portal is not running. |
| `GET /device-login?invite=` | portal listener only: redeem a login link, set the device cookie, redirect to `/` |
| `POST /api/ws-ticket` | local listener only. `{path}` (a WebSocket path under `/api/`) -> `{ticket}`: single use, 30 s, that path only |

## WebSockets

### Terminal: `GET /api/terminals/:id/ws`

Attach to a live terminal (`:id` is the session id). Browsers authenticate the upgrade with `?ticket=` (see [Listeners and authentication](#listeners-and-authentication)). A session of this machine whose process has ended gets one `exit` frame (its stored status and exit code) and a normal close; 404 `terminal_not_found` when the session is unknown or still starting without a terminal. Use the session's events for an ended session's history. Sessions on another machine are relayed through the hub. Several clients may attach.

Server to client:

- **Text** frames, JSON:
  - `{"type":"snapshot","cols":N,"rows":N,"data":"..."}`: reset your terminal and write `data`. It starts with `ESC c`, replays the scrollback (up to 10 000 lines, the newest 16 MiB of it), redraws the screen and restores modes, cursor and title. Sent first, and again whenever the client fell behind. Relayed snapshots can be large; accept text frames up to 64 MiB.
  - `{"type":"resize","cols":N,"rows":N}`: another client resized the terminal (last resize wins).
  - `{"type":"exit","status":"completed","exit_code":0}`: the process ended; the socket closes.
- **Binary** frames: raw PTY output bytes. A frame can end in the middle of a UTF-8 sequence; feed bytes to the terminal, do not decode per frame.

Client to server:

- **Binary** frames: raw input bytes.
- **Text** frames, JSON: `{"type":"input","data":"..."}` or `{"type":"resize","cols":N,"rows":N}` (1-1000 each).
- Frames are limited to 1 MiB. Without `control`, input and resize frames are dropped.

### Events: `GET /api/events/ws`

Server push only; each text frame is one JSON event:

| `type` | Fields | When |
|---|---|---|
| `session_created` | `session` | a session was launched or discovered |
| `session_updated` | `session` | status, title, summary, tokens... changed (at most once per second per session) |
| `project_updated` | `project_id` | a project was created, renamed, merged or removed |
| `memory_updated` | `project_id`, `part` (`brief`, `records`, `wiki`, `resources`, `suggestions`) | memory changed |
| `sync_updated` | `status: SyncStatus` | role, connection or portal changed |
| `resync` | - | events were dropped because the client fell behind; refetch state |

## MCP: `/mcp`

MCP over Streamable HTTP, on the local listener only, with the same bearer token, and only for `Host: localhost`, `127.0.0.1` or `::1`. The project for tools that default to "current" is taken from `?project=<id>` on the URL. Tools are described in [memory.md](memory.md#mcp-tools). Agents launched by blirp use the stdio server (`blirp mcp`) instead, which needs no token.

## UI

Every other `GET` path serves the embedded web UI (client-side routes fall back to `index.html`).
