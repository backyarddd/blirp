# Configuration

## config.toml

`~/.blirp/config.toml` (`$BLIRP_HOME/config.toml`). The daemon writes a file with all defaults on first start. Every section and key is optional; missing keys take their default. Unknown keys and invalid values are errors: the daemon refuses to start and names the key (`blirp doctor` shows the same message).

When the UI saves settings (**Settings**, `PATCH /api/settings`) it validates the whole config and edits the file in place: only changed values are rewritten, and your comments and layout are kept. Hand edits take effect when the daemon restarts; the daemon does not watch the file. Changes saved through the UI take effect immediately for `[agents]`, `[sessions]`, `[memory]`, `[portal]`, `sync.allow_hub_control` and `sync.lan_discovery`; `daemon.port`, `machine.name` and `sync.relay` need a daemon restart. `sync.role` and `sync.hub` are only changed by the hub, pairing and leave buttons (and their CLI and API equivalents), never by a settings save.

```toml
[daemon]
port = 47770

[machine]
name = "workstation"

[agents]
default = "claude"

[[agents.custom]]
name = "my-agent"
command = "my-agent"
args = ["--fast"]

[sessions]
worktree_default = false
# keep_awake = true            # default: true on the hub, false elsewhere

[memory]
summarizer = "auto"
ollama_model = "qwen2.5:7b"
distill_idle_secs = 300
daily_distill_limit = 40
brief_mode = "auto"
inject_max_chars = 8000
distill_max_chars = 60000

[sync]
role = "standalone"
# hub = "<hub endpoint id>"
relay = "default"
allow_hub_control = false
lan_discovery = true
project_files = true

[portal]
lan = false
lan_port = 47771

[update]
check = true

[files]
max_file_mb = 50
max_root_gb = 2
hub_quota_gb = 0
upload_kbps = 0
keep_versions_days = 30
```

### `[daemon]`

| Key | Type | Default | Description |
|---|---|---|---|
| `port` | integer 0-65535 | `47770` | Port of the local API/UI on `127.0.0.1`. If it is taken, the daemon does not start and names the program holding it when it can (see [troubleshooting](troubleshooting.md#port-in-use)). `0` picks a free port at every start; clients find it in `runtime.json`. `blirp daemon --port` overrides it. |

### `[machine]`

| Key | Type | Default | Description |
|---|---|---|---|
| `name` | string, not empty | the OS hostname | Display name of this machine in the UI, in "Recent sessions" lines of injected memory, and for paired machines. Also editable in **Settings > Machines & Sync**. |

### `[agents]`

| Key | Type | Default | Description |
|---|---|---|---|
| `default` | string | `"claude"` | Agent preselected in the new-session dialog: a built-in id (`claude`, `codex`, `opencode`, `pi`, `gemini`, `cursor`, `amp`, `aider`, `dsh`, `shell`) or `custom:<name>` of a defined custom agent. |
| `custom` | array of tables | `[]` | Custom agents, see below. |

`[[agents.custom]]` entries ([agents.md](agents.md#custom-agents)):

| Key | Type | Default | Description |
|---|---|---|---|
| `name` | string, 1-40 chars of `A-Z a-z 0-9 _ -`, unique | required | The agent id becomes `custom:<name>`. |
| `command` | string, not empty | required | Executable name looked up on `PATH`, or a path containing `/` or `\`. Not a shell command line: arguments go in `args`. |
| `args` | array of strings | `[]` | Arguments passed on every launch. |

### `[sessions]`

| Key | Type | Default | Description |
|---|---|---|---|
| `worktree_default` | bool | `false` | Pre-tick **Run in a new git worktree** for git projects. Ignored for non-git folders. |
| `keep_awake` | bool | unset: `true` when `sync.role = "hub"`, else `false` | While any session runs on this machine, prevent idle sleep (macOS `caffeinate -i -w <daemon pid>`, Windows `SetThreadExecutionState`, Linux `systemd-inhibit --what=sleep` when installed); released when the last session ends. Takes effect within a second, also when saved from Settings. See [cloud-sessions.md](cloud-sessions.md#keep-awake). |

### `[memory]`

| Key | Type | Default | Description |
|---|---|---|---|
| `summarizer` | `"auto"` \| `"claude"` \| `"codex"` \| `"ollama"` \| `"none"` | `"auto"` | Backend that distills sessions. `auto` = the summarizer of `agents.default` (`claude` or `codex`) when it is installed and logged in, else `claude`, else a reachable Ollama. `claude` runs Sonnet. `none` disables distilling. See [memory.md](memory.md#summarizer-backends). |
| `ollama_model` | string | `"qwen2.5:7b"` | Ollama model; must be non-empty when `summarizer = "ollama"`. The Ollama URL comes from `OLLAMA_HOST` (default `http://127.0.0.1:11434`). |
| `distill_idle_secs` | integer > 0 | `300` | A session idle this long with new events is distilled. |
| `daily_distill_limit` | integer | `40` | Maximum distill runs per UTC day (automatic and manual). `0` stops all distilling. |
| `brief_mode` | `"auto"` \| `"review"` | `"auto"` | `auto` applies brief updates as new versions; `review` turns them into suggestions you accept or reject. |
| `inject_max_chars` | integer >= 500 | `8000` | Hard cap on the memory injected into a new session. |
| `distill_max_chars` | integer >= 1000 | `60000` | Cap on the transcript text sent to the summarizer per run (head 20 %, tail 80 %). |

### `[sync]`

Normally managed by `blirp hub enable|disable`, `blirp pair` and **Settings > Machines & Sync**. Edit by hand only to change `relay` or `lan_discovery`.

| Key | Type | Default | Description |
|---|---|---|---|
| `role` | `"standalone"` \| `"hub"` \| `"node"` | `"standalone"` | This machine's sync role. |
| `hub` | string | unset | Endpoint id of the paired hub; required when `role = "node"`. |
| `relay` | `"default"` \| `"disabled"` \| `http(s)://` URL | `"default"` | How machines find and reach each other: n0 public relays and DNS discovery, direct/LAN only, or only your own iroh relay. See [sync-and-hub.md](sync-and-hub.md#network-relay-and-privacy). |
| `allow_hub_control` | bool | `false` | Node only: let the hub and other paired machines launch, stop, resume and delete sessions, type into terminals and change memory on this machine. Off: they can only read it. Takes effect immediately. |
| `lan_discovery` | bool | `true` | Find and announce machines on the local network with mDNS (hub and node). Off: `blirp pair <code>` without an invite does not work, and peers connect through the relay or the addresses they already know. On macOS it needs the Local Network permission ([troubleshooting.md](troubleshooting.md#error-sending-mdns-no-route-to-host-in-the-log-macos)). Also in **Settings > Machines & Sync**; takes effect immediately (a running sync endpoint restarts; if it cannot, the setting is still saved and the save answers `sync_failed`). |
| `project_files` | bool | `true` | Upload this machine's project folders to the hub (hub and node roles). `false` is a hard opt-out: this machine never uploads, whatever a project's **Files on hub** setting says (Off there stops uploads from every machine). Also in **Settings > Machines & Sync**. See [project-files.md](project-files.md). |

### `[portal]`

| Key | Type | Default | Description |
|---|---|---|---|
| `lan` | bool | `false` | On a hub, serve the UI over HTTPS on all interfaces. Ignored unless `role = "hub"`. |
| `lan_port` | integer 1-65535 | `47771` | Port of the LAN portal. The UI accepts 1024-65535. |

### `[update]`

| Key | Type | Default | Description |
|---|---|---|---|
| `check` | bool | `true` | Let the daemon ask GitHub once a day (hourly after a failure; the UI asks the daemon on load and every few hours) whether a newer release exists, and **Check now** in Settings > About ask right away (at most once a minute). The UI then shows a notice with **Update now** (script installs) or a link to the release page. `false`: no requests, no notice and no Update now; `blirp update` still works ([install.md](install.md#updating)). |

### `[files]`

Project file sync through the hub ([project-files.md](project-files.md)). Changes apply at once.

| Key | Type | Default | Description |
|---|---|---|---|
| `max_file_mb` | integer 1-1024 | `50` | Larger files are skipped and listed. On the hub, also the largest upload it accepts. |
| `max_root_gb` | integer 1-1024 | `2` | A folder with more synced data (or more than 100 000 files) is paused as too large. |
| `hub_quota_gb` | integer | `0` | Hub: disk space for file contents; `0` = half of the free space when file sync first ran on the hub. Old versions are dropped first, then new uploads are refused (`hub_quota`). |
| `upload_kbps` | integer | `0` | Upload limit of this machine in kilobits per second; `0` = unlimited. |
| `keep_versions_days` | integer > 0 | `30` | Hub: how long replaced versions are kept. |

## Environment variables

| Variable | Used by | Effect |
|---|---|---|
| `BLIRP_HOME` | every `blirp` command, the desktop app | Data directory. Default `~/.blirp` (`%USERPROFILE%\.blirp` on Windows). `blirp daemon --detach` and `blirp service install` pass it on to the daemon. On Windows set it as a user environment variable so the login entry sees it. |
| `BLIRP_LOG` | daemon | Log filter in `tracing` `EnvFilter` syntax, default `info` (e.g. `BLIRP_LOG=debug`, `BLIRP_LOG=info,blirp::ingest=debug`). |
| `OLLAMA_HOST` | Ollama summarizer | Ollama base URL (`host:port` or URL), default `http://127.0.0.1:11434`. |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `GEMINI_CLI_HOME`, `CURSOR_CONFIG_DIR`, `AMP_DATA_DIR`, `PI_CODING_AGENT_DIR`, `DSH_HOME`, `APPDATA` | ingest, global hooks | Where agents keep transcripts and config; blirp follows the same overrides as the agents. |
| `VISUAL`, `EDITOR` | **Open in editor** | Editor used for a session folder when it is a graphical one (terminal editors such as vim, nano or `emacs -nw` are skipped: the daemon has no terminal to show them in), else the first of `code`, `cursor`, `codium`, `zed`, `subl` on PATH, else the OS file manager. |
| `SHELL` | Shell sessions (macOS/Linux) | The shell to start. |
| `GITHUB_TOKEN` | install scripts, `blirp update`, daemon update check | Bearer token for the GitHub API; downloads then go through the API (private repository, rate limits). |
| `BLIRP_RELEASE_BASE_URL` | install scripts, `blirp update`, daemon update check | Releases API to use instead of `https://api.github.com/repos/backyarddd/blirp/releases` (mirror, local test server). |
| `BLIRP_INSTALL_DIR`, `BLIRP_VERSION`, `BLIRP_NO_APP`, `BLIRP_SERVICE`, `BLIRP_MODIFY_PATH`, `BLIRP_NO_MODIFY_PATH` | install scripts | See [install.md](install.md#options). |
| `BLIRP_E2E_CHANNEL`, `BLIRP_E2E_KEEP` | web e2e tests | See [development.md](development.md#end-to-end-tests). |

Set by blirp for the processes it starts (do not set them yourself): `BLIRP_SESSION_ID`, `BLIRP_PROJECT_ID`, `BLIRP_MEMORY_FILE` (agent sessions and their MCP server), `BLIRP_DISTILLING=1` (summarizer runs; blirp hooks exit immediately when it is set).

## Files and locations

### Data directory

Same layout on every OS under `BLIRP_HOME` (default `~/.blirp`; Windows `%USERPROFILE%\.blirp`):

| Path | Contents |
|---|---|
| `config.toml` | configuration (above) |
| `blirp.db` (+ `-wal`, `-shm`) | SQLite database: all projects, sessions, redacted transcripts, memory, devices, sync state |
| `runtime.json` | pid, port, token, version and start time of the running daemon (mode 0600 on macOS/Linux) |
| `daemon.lock` | single-instance lock held by the daemon |
| `identity.key` | this machine's Ed25519 identity for sync (mode 0600); created on first start |
| `tls/cert.pem`, `tls/key.pem` | LAN portal certificate and key (hub only; key 0600) |
| `logs/blirpd.<date>.log` | daemon log, rotated daily, 7 kept |
| `logs/desktop.<date>.log`, `logs/desktop-daemon-start.log` | desktop app log; stderr of a daemon the app started |
| `logs/launchd.log` | macOS LaunchAgent stdout/stderr |
| `launch/<session-id>/` | per-launch files: `memory.md`, `handoff.md`, Claude `settings.json` and `mcp.json`, Gemini/Amp settings copies |
| `worktrees/<project-id>/<name>/` | per-session git worktrees |
| `distill/run-*/` | summarizer scratch folders, removed after each run |
| `files/blobs/`, `files/tmp/`, `files/dl/` | project file sync: contents stored on the hub (zstd), partial uploads, partial downloads |

Installing, upgrading or removing blirp never touches this directory.

### Autostart (`blirp service install`)

| OS | File / entry |
|---|---|
| macOS | `~/Library/LaunchAgents/dev.blirp.daemon.plist` (runs `blirp daemon`; restarts after a crash) |
| Linux | `~/.config/systemd/user/blirp.service` (`Restart=on-failure`, enabled with `--now`) |
| Windows | registry value `blirp` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`: `conhost.exe --headless "<blirp.exe>" daemon --detach` |

### Installed files

Install scripts (`install.sh`, `install.ps1`; removed again by `blirp uninstall`):

| OS | Location |
|---|---|
| macOS | CLI `~/.local/bin/blirp` (or `BLIRP_INSTALL_DIR`), app `~/Applications/blirp.app`, receipt `~/.local/share/blirp/install.json` |
| Linux | CLI `~/.local/bin/blirp`, app `~/.local/share/blirp/blirp.AppImage`, menu entry `~/.local/share/applications/blirp.desktop` (plus `blirp-desktop-handler.desktop`, the app's `blirp://` handler), receipt `~/.local/share/blirp/install.json` (`XDG_DATA_HOME` moves `~/.local/share`) |
| Windows | `%LOCALAPPDATA%\Programs\blirp\` (or `BLIRP_INSTALL_DIR`): `blirp.exe`, `conpty.dll`, `x64\OpenConsole.exe`, `blirp-desktop.exe` (+ its `blirp-desktop.exe.WebView2` data folder), `install.json`; Start Menu `blirp.lnk`; user `Path` entry; `HKCU\Software\Classes\blirp` (`blirp://` links) |

With `--modify-path`, `install.sh` appends one line marked `# added by the blirp installer` to your shell startup file.

Classic installers:

| OS | Location |
|---|---|
| Windows (setup.exe) | `%LOCALAPPDATA%\blirp\` with `blirp.exe` (CLI), `conpty.dll`, `x64\OpenConsole.exe` |
| Windows (.msi) | per machine, under Program Files |
| macOS | `/Applications/blirp.app`; CLI at `blirp.app/Contents/MacOS/blirp` |
| Linux | `.deb`/`.rpm` install `/usr/bin/blirp`; the AppImage is self-contained |

### Files outside the data directory that blirp may change

Only `blirp hooks install|uninstall` (agent user configs, listed in [memory.md](memory.md#global-hooks), with `.blirp-backup` copies), `blirp service install|uninstall` and the install scripts, `blirp update` and `blirp uninstall` (above). Sessions with **Run in a new git worktree** add a worktree and a `blirp/<name>` branch to your repository's git metadata.
