# CLI reference

`blirp` is one binary: daemon, CLI, hook handler and MCP server. Every command honors `BLIRP_HOME`. Commands that talk to the daemon read `~/.blirp/runtime.json` for its port and token and fail with "blirp daemon is not running; start it with `blirp daemon --detach`" when it is down. Errors are printed as `error: <message>` with exit code 1.

```
blirp [-h|--help] [-V|--version] [<command>]
```

Without a command, `blirp` does what [`blirp app`](#blirp-app) does.

| Command | Needs daemon | Purpose |
|---|---|---|
| [`app`](#blirp-app) (or no command) | starts it | start the daemon, open the desktop app or the browser UI |
| [`daemon`](#blirp-daemon) | - | run the daemon |
| [`start`](#blirp-start) | starts it | start the daemon (through the autostart service when installed) |
| [`status`](#blirp-status) | - | is the daemon running |
| [`open`](#blirp-open) | yes | open the UI in the browser, logged in |
| [`sessions`](#blirp-sessions) | yes | list recent sessions |
| [`stop`](#blirp-stop) | - | stop the daemon |
| [`logs`](#blirp-logs) | no | show the daemon log |
| [`doctor`](#blirp-doctor) | no | check the installation |
| [`mem`](#blirp-mem) | no | search and show project memory |
| [`hooks`](#blirp-hooks) | no | install/remove global agent hooks and MCP |
| [`agents`](#blirp-agents) | no | store or remove the Claude login token for a headless hub |
| [`pair`](#blirp-pair) | yes | pair this machine with a hub |
| [`hub`](#blirp-hub) | yes | hub role, invites, sync status |
| [`devices`](#blirp-devices) | yes | paired machines and browser devices |
| [`service`](#blirp-service) | no | autostart at login |
| [`update`](#blirp-update) | no | update blirp and the desktop app |
| [`uninstall`](#blirp-uninstall) | no | remove blirp (and with `--purge` its data) |
| [`hook`](#blirp-hook), [`mcp`](#blirp-mcp) | - | entry points spawned by agents |

## blirp app

```
blirp
blirp app
```

Starts the daemon in the background if it is not running (like `blirp start`), then opens the desktop app when it is installed (macOS `~/Applications/blirp.app` or `/Applications/blirp.app`, Linux `~/.local/share/blirp/blirp.AppImage` or `blirp-desktop` next to `blirp`, Windows `blirp-desktop.exe` next to `blirp.exe`), else the UI in your default browser, logged in. In an SSH session (`SSH_CONNECTION` / `SSH_TTY`), or on Linux without `DISPLAY` / `WAYLAND_DISPLAY`, it opens nothing: it prints the daemon URL, a login link to use through an SSH port forward, and the hub / LAN portal alternative ([portal.md](portal.md)).

## blirp daemon

```
blirp daemon [--detach] [--port <PORT>]
```

Runs the daemon in the foreground: local API and UI on `127.0.0.1`, terminals, transcript ingest, memory jobs, sync and (hub) the LAN portal. Logs go to stderr and `~/.blirp/logs/blirpd.<date>.log`. Stops on Ctrl+C / SIGTERM (or `POST /api/daemon/shutdown`); running sessions end and become Detached. Only one daemon runs per `BLIRP_HOME`; a second one exits with "another blirp daemon is already running".

- `--detach`: start the daemon in the background (new session / no console window) and return once it answers health checks (up to 20 s). If a daemon is already running, prints its pid and port and exits 0.
- `--port <PORT>`: listen on this port instead of `daemon.port`; `0` picks a free port.

## blirp start

Starts the daemon in the background unless it is running, and returns once it answers. When an autostart service is installed for this data directory (macOS LaunchAgent, Linux `systemd --user` unit; see `blirp service`) it starts the daemon through it (`launchctl kickstart`, `systemctl --user start`), so the service keeps supervising it; otherwise it runs `blirp daemon --detach`. The install scripts use it after an upgrade, and the desktop app when it finds no daemon.

## blirp status

Prints version, pid, URL, machine name and id, sync role and data directory. Exit code 1 when the daemon is not running.

## blirp open

Opens `http://127.0.0.1:<port>/#token=...` in the default browser. The UI stores the token for its origin, removes it from the address bar and shows the workspace. The fragment is never sent to the daemon or any other server.

## blirp sessions

```
blirp sessions [--project <PROJECT_ID>] [--limit <N>]
```

Recent sessions (default 20), newest first: id, status, agent, title or folder.

## blirp stop

Stops the daemon gracefully (`POST /api/daemon/shutdown`; running sessions end as Detached). If it still runs after 15 seconds, or does not answer, its process is killed. Prints "not running" and exits 0 when no daemon runs.

## blirp logs

```
blirp logs
```

Shows the daemon log (`~/.blirp/logs/blirpd.<date>.log`). Run `blirp logs --help` for the options of your version.

## blirp doctor

Checks, one line each, `[ ok ]` or `[FAIL]`: data directory writable, `config.toml` valid, database opens and passes `quick_check` (schema version shown), daemon reachable, `git` on PATH. Then `[info]` lines: LAN discovery (on or off, the running daemon's latest mDNS send failure when it logged one in the last 11 minutes, and on macOS a hint about the Local Network permission), detected agents with versions, how claude logs in (`claude auth`: a login token from the environment, a stored token or the keychain / credentials file, and whether `claude auth status` reports it logged in; asked from the running daemon, else from this shell), and per transcript adapter the store it found, the number of sources and the time of the last ingest. Exit code 1 if any check failed. Works without the daemon; run it in the same environment the daemon runs in (PATH matters for agent detection).

## blirp mem

Reads the database directly in read-only mode, so it works while the daemon is stopped (after the daemon ran once). Without `--project`, the project is the one containing the current directory; outside any project the command fails and asks for `--project <id>`.

```
blirp mem search [--project <ID> | --all] [--kind record|event] [--limit <N>] [--json] <QUERY>...
blirp mem brief  [--project <ID>] [--json]
blirp mem recent [--project <ID>] [--limit <N>] [--json]
blirp mem show   [--limit <N>] [--json] <SESSION_ID>
```

- `search`: full-text search (porter stemming) over records and transcript events. `--all` searches every project; `--kind` restricts to records or transcript events; `--limit` default 20 (max 200). Prints date, the record or session/seq, and a snippet.
- `brief`: the project brief and all active records (kind, title, pinned, body).
- `recent`: recent sessions of the project with summaries; `--limit` default 10.
- `show`: one session's header, summary (or last distill error) and its transcript events; `--limit` default 200 events (max 1000).
- `--json`: machine-readable output (`search`: `{"hits": [...]}`; `brief`: `{project, brief, records}`; `show`: `{session, events}`; `recent`: array of sessions).

## blirp hooks

```
blirp hooks install   [--agent <AGENT>]
blirp hooks uninstall [--agent <AGENT>]
blirp hooks status    [--agent <AGENT>]
```

Global integration for sessions started outside blirp. Supported agents: `claude`, `codex`, `gemini`, `cursor`, `opencode`. Without `--agent`, `install` handles every supported agent found on PATH, `uninstall` and `status` all supported agents. Prints one line per agent: `hooks <installed|not_installed|unsupported>  mcp <...>`, plus a note after install (Codex: run `/hooks` once to trust them). Exit code 1 if any agent failed (e.g. a config file with comments). Does not need the daemon. Exactly what changes: [memory.md](memory.md#global-hooks).

## blirp agents

```
blirp agents set-token claude
blirp agents clear-token claude
```

`set-token` reads a token printed by `claude setup-token` from stdin (hidden when you type or paste it into a terminal; piped input works too, e.g. `blirp agents set-token claude < token.txt`), trims surrounding whitespace, and stores it in `~/.blirp/secrets/claude_oauth_token` (`0600`, folder `0700` on macOS and Linux). A token with spaces or line breaks inside is refused. Terminals that are not a console (Git Bash / mintty on Windows) cannot hide input; there, pipe the token in. Ctrl+C at the prompt turns echo back on. The daemon passes it to claude sessions and the claude summarizer as `CLAUDE_CODE_OAUTH_TOKEN`, read each time it starts one, so no restart is needed. `clear-token` removes it. Does not need the daemon. Why and when: [agents.md](agents.md#headless-login-for-a-hub).

## blirp pair

```
blirp pair <INVITE> <CODE>
blirp pair <CODE>
blirp pair 'blirp://join/<invite>#<code>'
```

Pairs this standalone machine with a hub ([sync-and-hub.md](sync-and-hub.md#pair-machines)). With only a code, the hub is found on the local network (one hub, one open invite). Waits up to 2 minutes for the hub. On success the machine becomes a `node` and prints its sync status.

## blirp hub

```
blirp hub enable    # become the hub; prints status and a first invite
blirp hub invite    # another invite + code (10 minutes, single use)
blirp hub status    # role, machine id, hub, connected, pending changes, portal URL + fingerprint
blirp hub disable   # back to standalone; paired machines stay known
```

`enable` fails on a paired node (`paired_node`); `invite` fails unless this machine is the hub (`not_hub`). `status` works in every role.

## blirp devices

```
blirp devices list
blirp devices revoke <ID>
```

On the hub: paired machines and browser devices with id, kind, state, terminal control and name. `revoke` cuts the device off immediately (connections close, reconnects refused).

## blirp service

```
blirp service install     # autostart at login, and start now
blirp service status      # installed? daemon running?
blirp service uninstall   # remove the autostart entry; a running daemon keeps running
```

Per-user autostart, never a system service: macOS LaunchAgent `dev.blirp.daemon`, Linux `systemd --user` unit `blirp.service`, Windows `HKCU\...\Run` value `blirp`. Records the absolute path of the binary you ran it with and, on macOS/Linux, your current `PATH` and `BLIRP_HOME`; re-run after moving the binary or changing where agents are installed. Refuses to install from a temporary location (an AppImage mount or `/tmp`). Idempotent.

## blirp update

```
blirp update [--check] [--version <X.Y.Z>]
```

Updates an installation made by the install scripts to the latest published release, or to `--version` (the only way to go to an older release). It verifies the minisign signature of the release's `SHA256SUMS.txt` with the key built into blirp and the SHA-256 of every download, then stops the daemon, replaces `blirp` (Windows: plus `conpty.dll`, `x64\OpenConsole.exe`) and the desktop app if the script installed it, updates the install receipt and starts the daemon again if it was running (like `blirp start`, so through the autostart service when one manages it). Nothing is changed when a check fails. A `blirp` the scripts did not install (source build, package) is not replaced; the command says so.

- `--check`: only report. Prints one line; exit code `0` when up to date, `10` when an update is available.
- `GITHUB_TOKEN` and `BLIRP_RELEASE_BASE_URL` work as for the install scripts ([install.md](install.md#private-repository-mirrors-and-testing)).

## blirp uninstall

```
blirp uninstall [--purge] [--yes]
```

Stops the daemon, removes the autostart entry (`blirp service uninstall`) and blirp's global hooks and MCP entries for every agent (`blirp hooks uninstall`; entries of other tools stay), then removes what the install script installed: the CLI files, the desktop app with its menu entry or Start Menu shortcut and `blirp://` registration, the PATH change the script made, and the install receipt. Only known file names in the recorded locations are deleted; a `blirp` the scripts did not install is left in place. On Windows the running `blirp.exe` is deleted right after the command exits.

- `--purge`: also delete the data directory (`BLIRP_HOME`, default `~/.blirp`: memory, sessions, settings). Asks for confirmation; refuses a directory that does not look like blirp's.
- `--yes`: do not ask.

## blirp hook

```
blirp hook <AGENT> <EVENT> [--global]
```

Called by agents, not by you. Reads the agent's hook JSON from stdin, reports it to the daemon, prints what the agent expects (memory on session start), and always exits 0 within 2 seconds, even when the daemon is down. `--global` marks entries written by `blirp hooks install`.

## blirp mcp

MCP server over stdio for agents ([memory.md](memory.md#mcp-tools)). Uses `BLIRP_PROJECT_ID` or the current directory to pick the project; opens the database read-only; `mem_record` needs the daemon.

## Exit codes

`0` success; `1` error (message on stderr), `blirp status` with no daemon, `blirp doctor` with a failed check, `blirp hooks` with a failed agent, `blirp uninstall` when something could not be removed; `2` invalid arguments; `10` `blirp update --check` with an update available. `blirp hook` always exits 0.
