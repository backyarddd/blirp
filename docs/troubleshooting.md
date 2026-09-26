# Troubleshooting

Start with:

```sh
blirp doctor      # data dir, config, database, daemon, git, agents, ingest per agent
blirp status      # daemon pid, URL, version, role
```

Run them in the same environment the daemon runs in: agent detection and hooks depend on `PATH` and `BLIRP_HOME`.

## Installing and updating

### `blirp: command not found` after installing

The CLI folder is not on your `PATH` yet.

- macOS/Linux: `install.sh` prints the line to add unless `~/.local/bin` (or `BLIRP_INSTALL_DIR`) is already on `PATH`. Add it to your shell startup file (`echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc`, or `~/.bashrc` / `~/.bash_profile`), or re-run the installer with `--modify-path`, then open a new terminal. Meanwhile run `~/.local/bin/blirp` directly.
- Windows: `install.ps1` adds `%LOCALAPPDATA%\Programs\blirp` to your user `Path`. Terminals that were already open keep the old `Path`; open a new one (in some setups sign out and in, so Explorer picks it up). Check with `[Environment]::GetEnvironmentVariable('Path', 'User')`.

### Antivirus or Defender flags blirp

blirp is not code signed, and new unsigned programs that download files and start processes are sometimes flagged by heuristics (false positives). Verify the file first: its SHA-256 must match the release's `SHA256SUMS.txt` (the install scripts and `blirp update` already did this, and `SHA256SUMS.txt.sig` proves the sums come from the blirp release key, see [install.md](install.md#manual-download)). Then restore the file from quarantine or add an exclusion for the install folder, and please report the false positive to the vendor (Microsoft: [submit a file](https://www.microsoft.com/en-us/wdsi/filesubmission)) and in an issue. Windows Defender may also block `blirp.exe` while an update replaces it; run `blirp update` again.

### "blirp is damaged" or "Windows protected your PC"

These appear only for files downloaded with a browser: macOS quarantines them and Windows marks them as coming from the internet, and then refuses or warns about unsigned programs. Files fetched by the install scripts (`curl`, `irm`) carry no such mark, so use the scripts, or:

- macOS, after checking the checksum: `xattr -dr com.apple.quarantine ~/Applications/blirp.app` (or wherever you put the app or the `blirp` binary). "Damaged" here does not mean corrupted; it is Gatekeeper's message for a quarantined unsigned app.
- Windows: **More info > Run anyway**, or remove the mark first: `Unblock-File .\blirp_<version>_x64-setup.exe`.

### `blirp update` fails

- **"was not installed by the blirp install script"**: that `blirp` came from a source build, a package or a classic installer (no install receipt next to it). Update it the same way you installed it, or install with the script.
- **"is not signed by the blirp release key"** or **"checksum mismatch"**: the download does not match what the release was signed with; nothing was changed. Try again; if it persists, report it (and do not install that file by hand).
- **"no published release found"** or **403 rate limit**: GitHub's anonymous API limit is 60 requests per hour per IP; set `GITHUB_TOKEN`. For a private repository `GITHUB_TOKEN` is required.
- After an update (or re-running the install script) the daemon is started again automatically, through the autostart service when one is installed. If it is not (`blirp status`), start it with `blirp start` and look at `blirp logs`.

## Logs

| File | What |
|---|---|
| `~/.blirp/logs/blirpd.<date>.log` | daemon (rotated daily, 7 kept); also printed to stderr by `blirp daemon` in the foreground |
| `~/.blirp/logs/desktop.<date>.log` | desktop app |
| `~/.blirp/logs/desktop-daemon-start.log` | stderr of the daemon the desktop app tried to start |
| `~/.blirp/logs/launchd.log` | macOS LaunchAgent output |
| `journalctl --user -u blirp` | Linux systemd user unit |

`blirp logs` prints the daemon log from the terminal. For more detail start the daemon with `BLIRP_LOG=debug` (or e.g. `BLIRP_LOG=info,blirp::ingest=debug`). Logs never contain transcript text, so they are safe to attach to an issue after a quick look.

Warnings and errors that can repeat for as long as a problem lasts (mDNS and relay retries, the hub connection, the status tick, keep-awake, the ingest and distill schedulers) are logged the first time and then at most once every 10 minutes, with `(N identical lines suppressed in the last 10 min)` appended. A different message, and every info line, is always logged.

## The daemon does not start

- **"blirp could not start its daemon"** in the desktop app: the page shows the error, the log folder, **Retry** and **Open log folder**. Look at `desktop-daemon-start.log` and the latest `blirpd.*.log`.
- **Invalid config.** `invalid config file ...: unknown field ...` or `invalid config: memory.distill_idle_secs must be > 0`: fix the named key in `~/.blirp/config.toml` ([configuration.md](configuration.md)), or move the file away to get defaults.
- **"another blirp daemon is already running for ~/.blirp (pid N, port P)".** Only one daemon per data directory. Use the running one (`blirp status`), or stop it (tray **Quit blirp**; `kill <pid>` / Task Manager; `systemctl --user stop blirp`; `launchctl bootout gui/$(id -u)/dev.blirp.daemon`). The lock is an OS file lock that disappears with the process; there is no stale lock file to delete.
- **Different versions, same data directory.** A daemon started by an older standalone binary keeps running while the app expects a newer one. Stop it and let the app (or the new binary) start it.
- **`daemon did not become healthy within 20s`** after `blirp daemon --detach`: see the log. Run `blirp daemon` in the foreground to see the error directly.
- **`cannot determine the user's home directory; set BLIRP_HOME`**: set `BLIRP_HOME` explicitly (services and containers without `HOME`).

## Port in use

If port 47770 is taken, the daemon does not start: `port 47770 on 127.0.0.1 is already in use by <program> (pid N)` (the program is named when the OS shows it; other users' processes usually are not). blirp does not move to another port on its own, because whatever holds the port would then answer at the address the desktop app, `blirp open` and your bookmarks use. Stop that program, or set another port in `config.toml`:

```toml
[daemon]
port = 47790   # or 0: a free port at every start
```

`blirp status`, the desktop app and the CLI find the actual port through `runtime.json`. `blirp daemon --port <n>` overrides the setting for one run. To see who holds the port yourself: `netstat -ano | findstr :47770` (Windows), `lsof -nP -iTCP:47770 -sTCP:LISTEN` (macOS/Linux) or `ss -ltnp 'sport = :47770'` (Linux). The LAN portal port (47771) has no fallback: if it is taken, enabling the hub fails with `bind 0.0.0.0:47771 for the LAN portal`; pick another `portal.lan_port`.

## "unauthorized" / blank UI in the browser after a restart

The runtime token changes at every daemon start, so a browser tab opened with `blirp open` loses its login when the daemon restarts. Run `blirp open` again. The desktop app logs in again by itself once the restarted daemon answers: when its window gets focus or is opened again, or when you press **Try again** on its "Sign in required" screen. Bookmarks of `http://127.0.0.1:<port>/` keep working after that: the UI keeps the token in the browser's storage for that address. If your browser blocks site storage for `127.0.0.1`, the login lasts only for the open tab.

## Windows: ConPTY and terminals

- The desktop app and the standalone zip ship `conpty.dll` and `x64\OpenConsole.exe` next to `blirp.exe`; blirp loads them from there, which fixes rendering and resize bugs of the older console built into Windows. If you copied only `blirp.exe` somewhere, copy those two as well (keep the `x64` folder), or blirp falls back to the system ConPTY.
- Windows 10 1809 or newer is required.
- An agent that seems stuck at startup usually waits for a terminal answer; blirp answers cursor-position and device-attribute queries while no client is attached. If a TUI still hangs, attach the terminal (open the session) and report the agent and version.
- `npm` agents are started as `node <script>` from their `.cmd` shim; if `node` is not on the daemon's `PATH`, launching fails with `spawn_failed`.
- Sessions end when the daemon ends (Windows job object), so an update or a daemon crash turns running sessions into Detached. Use **Resume**.

## An agent is missing or "not installed"

The daemon, not your shell, looks the agent up on its own `PATH`.

- **macOS/Linux desktop app** started from the Dock or a launcher: the app asks your login shell (`$SHELL -ilc`) for `PATH` before starting the daemon, so the `PATH` from `~/.zprofile`/`~/.zshrc`/`~/.bash_profile` counts. If the daemon was already running (started by the service or an earlier `blirp daemon --detach`), it keeps the `PATH` it started with: quit blirp and start it again.
- **Autostart (`blirp service install`)** records the `PATH` of the shell you ran it in. After installing an agent somewhere new, run `blirp service install` again from a shell where `which <agent>` works, then restart the daemon.
- **Windows:** only `.exe`, `.com`, `.cmd`, `.bat` and `.ps1` files count; npm's extensionless shims are ignored. A newly installed agent is found once the daemon's environment has it: log out and in, or quit and restart blirp.
- Cursor CLI is looked up as `cursor-agent`, then `agent`.
- The agent list is cached for 60 seconds.

`blirp doctor` prints the agents it finds from your current shell.

## Claude is not logged in for the daemon

Symptoms: the new-session dialog says Claude Code is not logged in on a machine, claude sessions there hang before the prompt or report "not logged in", or distilling pauses with an auth error, while `claude` in your own terminal works. The daemon cannot read the login keychain: it was started over SSH, or a LaunchAgent runs it on a Mac that is locked or has nobody logged in.

1. Run `claude setup-token` on any machine with a browser and copy the token.
2. On the affected machine run `blirp agents set-token claude` and paste it (over SSH is fine).
3. `blirp doctor` should now print `claude auth: stored login token ..., logged in (oauth_token) as seen by the daemon`. No restart is needed; sessions already running keep their old login.

If `blirp doctor` says `CLAUDE_CODE_OAUTH_TOKEN from the environment`, the daemon's own environment sets the variable and that value wins over the stored token; fix or remove it where the daemon is started. `blirp agents clear-token claude` removes the stored token. Details: [agents.md](agents.md#headless-login-for-a-hub).

## Hooks are not firing / status stays Idle

- `blirp hooks status` shows per agent whether hooks and MCP are installed.
- **Codex** runs new hooks only after you trust them: run `/hooks` in Codex once after `blirp hooks install`.
- **Config file with comments** (JSONC): install refuses to rewrite it and says so. Add the entries by hand or remove the comments, then install again.
- **Moved or reinstalled blirp**: hook entries contain the absolute path of the `blirp` binary. Run `blirp hooks install` again.
- **Daemon down**: hooks still exit 0 quickly and SessionStart still injects memory from the database, but status and ingest updates are lost until the daemon runs.
- Agents without hooks (opencode, pi, Amp, Aider, dsh, custom, shell) get status from terminal output only: Working while output arrives, Idle 2 s after it stops, never Waiting.
- Sessions launched from blirp do not need global hooks at all.

## Memory is not injected

1. Open the session's **Memory** panel: "Injected at start" is exactly what the session got (`~/.blirp/launch/<session-id>/memory.md`). If it only says `_No project brief yet._` with no sections, the project has no memory yet (no distilled sessions, no records).
2. Check the agent's mechanism in [agents.md](agents.md): Cursor CLI gets memory only with global hooks; dsh, shell and custom agents only get `BLIRP_MEMORY_FILE`; Amp gets none if its settings file cannot be read (see the log).
3. Wrong project: the session's folder resolved to another project (for example `Home (<machine>)` for sessions in your home directory). See [projects-and-sessions.md](projects-and-sessions.md#how-a-folder-becomes-a-project).
4. Sessions started outside blirp get memory only through global hooks (`blirp hooks install`).
5. Search the log for `launching without it` warnings (memory render or integration failed).

## Memory is not updating (no summaries)

- `summarizer = "none"`, or `auto` found nothing: install/log in to `claude` or `codex`, or run Ollama. **Settings > Memory** shows the setting.
- The summarizer is not logged in or out of quota: the session page (and `blirp mem show <id>`) shows `last distill failed: ...`. Log in to the CLI in the account the daemon runs as. For `claude` on a daemon without keychain access (started over SSH, or a locked Mac), see [Claude is not logged in for the daemon](#claude-is-not-logged-in-for-the-daemon).
- Daily budget used up (log: `daily distill budget used up`): raise `memory.daily_distill_limit` or wait for the next UTC day.
- The session is still running and has not been idle for `distill_idle_secs`.
- Only sessions active in the last 7 days are distilled automatically; use **Distill now** for older ones.
- Subagents and sessions of other machines are not distilled here by design.
- **Distill now** answering `nothing_to_distill`: no transcript events were ingested. Check `blirp doctor`'s ingest lines for that agent (store found? sources?).

## Sessions from outside blirp do not appear

- `blirp doctor` ingest line for the agent: `no store found` means the transcript directory does not exist where blirp looks. If you moved it with an environment variable (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, ...), the daemon must have the same variable.
- Aider is only found in folders registered as projects.
- The daemon rescans every 5 minutes; hooks make it immediate.
- Transcripts inside `~/.blirp` (summarizer runs) are skipped on purpose.

## Sync does not connect

- `blirp hub status` on both sides: role, hub id, connected, pending changes.
- **Pairing fails** with `wrong_code`, `expired`, `too_many_attempts` or `unknown_invite`: create a new invite (`blirp hub invite`). Invites expire after 10 minutes, after 5 attempts, after one success, and when the hub restarts.
- **`blirp pair <code>` cannot find the hub**: LAN discovery needs both machines on the same network with mDNS allowed (`sync.lan_discovery` on both, and on macOS the Local Network permission) and exactly one hub with one open invite. Use `blirp pair <invite> <code>` instead.
- **`sync.relay = "disabled"`** only works when the machines can reach each other directly (same LAN, Tailscale, open UDP). Across NATs use `default` or your own relay.
- Firewalls must allow UDP for the daemon (QUIC). Corporate networks that block UDP need the relay (`default`), which also carries traffic when no direct path exists.
- A revoked machine is refused; pair it again with a new invite.
- If `~/.blirp/identity.key` was deleted or replaced, the machine has a new identity: the hub no longer knows it. Leave and pair again.
- Keep hub and nodes on the same blirp version (`unsupported_version` in the log otherwise).

## `error sending mDNS: No route to host` in the log (macOS)

Hubs and nodes announce and find each other on the local network with mDNS (`sync.lan_discovery`, on by default). macOS allows that, and direct connections to other machines on the LAN, only for programs that have the Local Network permission; without it every send fails with "No route to host". The daemon logs the first failure, then one line every 10 minutes with the number of repeats, and `blirp doctor` reports it on its `LAN discovery` line while the running daemon keeps logging it (a failure line in the last 11 minutes).

- **Allow it:** System Settings > Privacy & Security > Local Network, turn blirp on (a daemon started from a terminal is listed as that terminal app), then restart the daemon. macOS asks the first time a program uses the local network; if blirp is not listed, start the daemon from a session at the Mac (`blirp`, or `blirp service install` in Terminal) instead of over SSH so the question can appear.
- **Or turn discovery off:** `[sync] lan_discovery = false` in `config.toml`, or uncheck **Find machines on the local network** in **Settings > Machines & Sync**. Sync and cloud sessions keep working; traffic between machines takes the relay, and `blirp pair` needs the invite. Keep `sync.relay` on `default` or your own relay then: without the permission a Mac cannot connect directly to machines on its LAN.

## Portal certificate warnings

The LAN portal uses a self-signed certificate, so every browser warns once per device. Compare the SHA-256 fingerprint in the browser's certificate details with **Settings > Portal** (or `blirp hub status`); if they match, proceed. When the hub's LAN IP changes, the portal gets a new certificate the next time it starts (restart the daemon to trigger it); its fingerprint is new, so every browser warns once more. To avoid warnings, use `tailscale serve` ([portal.md](portal.md#tailscale)).

The portal does not start although it is enabled: it runs only on a hub, and a changed `portal.lan` setting applies after a daemon restart. `POST /api/devices/browser-invite` answers `portal_disabled` while it is not running.

## Linux desktop

- No tray icon on GNOME: install the "AppIndicator and KStatusNotifierItem Support" extension. Without it, closing the window still keeps the daemon running; launch blirp again to get the window back.
- The desktop AppImage runs `blirp` from a temporary mount that is gone once the app exits. Autostart (`blirp service install`) and the global agent integration (Settings > Agents, `blirp hooks install`) therefore record the installed CLI (from the install script, or `blirp` on `PATH`) and refuse with "runs from a temporary location" when there is none: install the CLI (see [install.md](install.md)) and try again.

## Still stuck

Open an issue with `blirp --version`, your OS, `blirp doctor` output and the relevant log lines.
