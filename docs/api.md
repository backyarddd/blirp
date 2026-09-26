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

The local listener accepts no cookies: a cookie for `127.0.0.1` is sent to every server on that host, whatever its port. The web UI is opened as `http://127.0.0.1:<port>/#token=<token>` (`blirp open`, the desktop app); the fragment never reaches a server. The UI keeps the token in its origin's `localStorage`, removes it from the address bar and sends it as a bearer token. Browsers cannot set headers on a WebSocket, so the UI first calls `POST /api/ws-ticket` with `{path}` and opens `<path>?ticket=<ticket>`: a ticket is valid once, for 30 seconds, for that path only. `GET /auth?token=` from older versions redirects to `/#token=` and sets no cookie; an old `blirp_session` cookie is expired on any request that still sends it.

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

`control` is required to launch, stop or resume sessions, distill a session, and send terminal input (403 `control_not_allowed`). `admin` is required for hub, pairing and device management, hook ingress, global hooks install/uninstall, `POST /api/sessions/:id/open` and `POST /api/daemon/shutdown` (403 `admin_only`). Other endpoints need only authentication.

CSRF protection: a mutating request or WebSocket upgrade that carries an `Origin` header must have the same host as its `Host` header, else 403 `forbidden_origin`. Clients without `Origin` (CLI, curl) are not affected.

## Errors

```json
{ "error": { "code": "invalid_request", "message": "project_id or cwd is required" } }
```

| Status | Codes |
|---|---|
| 400 | `invalid_request` (validation, malformed JSON or query, unknown fields), `unknown_agent`, `not_git`, pairing errors (`wrong_code`, `expired`, `too_many_attempts`, `unknown_invite`, `invite_required`, ...), `invalid_invite` |
| 401 | `unauthorized`, `invalid_invite` (device login) |
| 403 | `forbidden_origin`, `control_not_allowed`, `admin_only` |
| 404 | `not_found`, `terminal_not_found`, `not_git` (git endpoints on a plain folder) |
| 405 | `method_not_allowed` |
| 409 | `conflict`, `session_live`, `not_running`, `already_running`, `nothing_to_distill`, `remote_session`, `machine_unreachable`, `machine_offline`, `paired_node`, `not_hub`, `already_synced`, `portal_disabled` |
| 422 | `agent_not_installed`, `worktree_failed`, `spawn_failed`, global hooks install failures |
| 429 | `rate_limited` (device login) |
| 500 | `internal` (details only in the daemon log) |

## Endpoints

### Health, machines, search, settings, agents

| Method and path | Description |
|---|---|
| `GET /api/health` | `{version, machine: Machine, role, capabilities, keep_awake}`; `keep_awake`: this machine holds a sleep assertion because sessions run (`sessions.keep_awake`) |
| `GET /api/update` | `UpdateStatus {current, latest, available, notes_url, enabled}`: whether a newer published release exists. `latest` is null when `[update] check` is off (`enabled: false`) or GitHub could not be reached. The daemon asks GitHub at most once a day (hourly after a failure). |
| `GET /api/machines` | `Machine[]`: `{id, name, os, role, last_seen, revoked}` of this and paired machines |
| `DELETE /api/machines/:id` | admin. On the hub: revoke that machine (204). On a node, with the hub's id: leave the hub. |
| `GET /api/machines/:id/health`, `GET /api/machines/:id/agents` | that machine's `/api/health` and `/api/agents`: answered here for this machine's id, else relayed through the hub (`machine_unreachable` when it is offline) |
| `GET /api/machines/:id/dirs?path=&hidden=` | control. `MachineDirs {machine_id, home, path, parent, entries: [{name, path, is_git}], truncated}`: the folders (never files) in `path` (absolute, default the home folder) on that machine, relayed like above. Only inside that machine's user home (403 `path_outside_home`; symlinked folders are not listed); hidden folders only with `hidden=true`; at most 2 000 entries. |
| `POST /api/machines/:id/clone` | control. `{url?, project_id?, parent?, name?}`: `git clone` on that machine into `<parent or ~/blirp>/<name or repo name>` (parent must be inside its home; 409 `already_exists`). `project_id` is resolved on the machine receiving the request to the remote URL of that project's git folder there (400 `no_git_remote`). URLs must be `https`, `http`, `ssh`, `git` or `user@host:path`; credentials in them are removed before the URL is forwarded or used. The clone runs in the background with the target user's git credentials, never prompting (`GIT_TERMINAL_PROMPT=0`, ssh `BatchMode`); 202 `CloneJob {id, machine_id, url, dest, state (running\|done\|failed), progress, error, started_at, finished_at}` |
| `GET /api/machines/:id/clone/:job` | the `CloneJob`, relayed like above; poll until `state` is not `running` |
| `GET /api/search?q=&project=&kind=&limit=` | full-text search; `kind` = `record` or `event`; `limit` default 50. `{hits: SearchHit[]}` with `kind, project_id, session_id, seq, record_id, title, agent, snippet, ts, score` |
| `GET /api/settings` | `{config: Config, values: {key: json}}`: `config.toml` plus local settings values |
| `PATCH /api/settings` | `{config?: Config, values?: {key: json \| null}}`. A full `Config` is validated (400 with the message) and written to `config.toml`; `null` deletes a value. Returns the new `SettingsView`. |
| `GET /api/agents` | `AgentInfo[]`: `{id, display_name, builtin, installed, path, version, can_resume, integration: {global_hooks, mcp, inject, detail}, auth}`; `auth` is `{logged_in, method}` for Claude Code (from `claude auth status`, run by the daemon, no model call) and null otherwise; `global_hooks`/`mcp` are `installed`, `not_installed` or `unsupported`, `inject` is `hook`, `instructions`, `flag` or `none`. Cached 60 s. |
| `POST /api/agents/:id/hooks/install`, `.../uninstall` | admin. Global integration for that agent; returns its `AgentInfo`; 422 when the agent is unsupported or a config file cannot be edited. |
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
| `GET /api/sessions?project=&status=&agent=&machine=&q=&parent=&include_children=&cursor=&limit=` | `{items: Session[], next_cursor}`, newest first, `limit` default 50. `parent=<id>` returns the sessions whose `parent_session_id` is `<id>`; `include_children=true` includes subagent sessions (external sessions with a parent) in other listings, `false` leaves them out. Pass `next_cursor` back as `cursor` for the next page. |
| `POST /api/sessions` | control. Launch, body `LaunchSession`: `{agent, project_id?, cwd?, prompt?, worktree?, continue_from?, machine?, cols?, rows?}`. One of `project_id`/`cwd` is required unless `continue_from` is given. `machine` other than this one forwards the launch to that machine. 201 `Session`. |
| `GET /api/sessions/:id` | `Session` |
| `PATCH /api/sessions/:id` | `{title}` (up to 300 characters; `null` clears) |
| `GET /api/sessions/:id/events?after=&limit=` | `{items: Event[], next_after}`; `Event {session_id, seq, ts, kind, text, meta}` with kind `user`, `assistant`, `tool_call`, `tool_result`, `system`, `file_edit`, `summary`; `limit` default 200 |
| `POST /api/sessions/:id/stop` | control. 202; the final status arrives on the event stream. 409 `not_running` if blirp has no live process for it. Forwarded when the session runs on another machine. |
| `POST /api/sessions/:id/resume` | control. Relaunch in the same row; returns `Session`. 409 `already_running`. |
| `POST /api/sessions/:id/distill` | control. 202 queued; 409 `nothing_to_distill` (no events) or `remote_session` (another machine's session). Counts against the daily budget. |
| `POST /api/sessions/:id/open` | admin. `{target: "folder" \| "editor"}` opens the session folder in the file manager or editor on this machine; 204 |

`Session`: `{id, project_id, machine_id, agent, agent_session_id, origin (blirp|external), cwd, title, status, stopped_by_user, branch, worktree, transcript_path, started_at, ended_at, last_activity_at, exit_code, summary, distilled_through_seq, tokens_in, tokens_out, cost_usd, parent_session_id}`. `status` is `starting`, `working`, `idle`, `waiting`, `completed`, `failed` or `detached`. `summary` is the distill result `{title, summary, decisions[], open_threads[], gotchas[], resolved_record_ids[], files[], backend, distilled_at, through_seq, error}` or `null`.

### Memory injection and hooks

| Method and path | Description |
|---|---|
| `GET /api/inject?session=&cwd=&agent=` | `{markdown}`: the session's launch `memory.md` if it exists, else a fresh render for the session's project or for the project containing `cwd` |
| `POST /api/hooks/:agent/:event` | admin. Hook ingress used by `blirp hook`: `{blirp_session_id?, cwd?, global, payload}` → `{session_id, additional_context}` |

### Sync, devices, portal

| Method and path | Description |
|---|---|
| `GET /api/sync/status` | `SyncStatus {role, machine_id, hub, connected, last_sync_at, pending_outbox, portal_url, portal_cert_fingerprint}` |
| `POST /api/sync/hub/enable` | admin. Become the hub (starts the portal if `portal.lan`); `SyncStatus`. 409 `paired_node`. |
| `POST /api/sync/hub/disable` | admin. Back to standalone; `SyncStatus`. 409 `not_hub`. |
| `POST /api/sync/invite` | admin, hub. `{invite, code, uri, expires_at}` |
| `POST /api/sync/join/preview` | admin. `{invite}` (invite, join link or `""`) -> `JoinPreview {hub_id}`: the hub's machine id read from the invite, nothing contacted; `null` for a LAN join. The hub's name is only exchanged after the code is verified. |
| `POST /api/sync/join` | admin. `{invite, code, allow_hub_control?}`; `invite` may be a `blirp://join/...` link (its code is used when `code` is empty) or `""` to find the hub on the LAN. `allow_hub_control` sets `sync.allow_hub_control` with the new role (left out: unchanged). `SyncStatus`. 409 `already_synced`. |
| `GET /api/devices` | `Device[]`: `{id, name, kind (machine\|browser), node_id, created_at, last_seen, revoked, can_control_terminals}` |
| `PATCH /api/devices/:id` | admin. `{can_control_terminals}`; closes the device's connections so they reopen with the new rights |
| `DELETE /api/devices/:id` | admin. Revoke; 204 |
| `POST /api/devices/browser-invite` | `{url, expires_at}`: one-time portal login link (5 minutes). 409 `portal_disabled` when the portal is not running. |
| `GET /device-login?invite=` | portal listener only: redeem a login link, set the device cookie, redirect to `/` |
| `POST /api/ws-ticket` | local listener only. `{path}` (a WebSocket path under `/api/`) -> `{ticket}`: single use, 30 s, that path only |
| `GET /auth?token=` | local listener: old login links; redirects to `/#token=<token>`, sets no cookie |

## WebSockets

### Terminal: `GET /api/terminals/:id/ws`

Attach to a live terminal (`:id` is the session id). Browsers authenticate the upgrade with `?ticket=` (see [Listeners and authentication](#listeners-and-authentication)). 404 `terminal_not_found` when the session has no live process; use the session's events instead. Sessions on another machine are relayed through the hub. Several clients may attach.

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
