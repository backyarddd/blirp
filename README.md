# blirp

blirp is a free, open-source workspace for CLI coding agents (Claude Code, Codex, opencode, Gemini CLI, Cursor CLI, Amp, Aider, pi, DeepSeek Harness and any other command) with automatic cross-session memory. It runs the real, unmodified agent TUIs in terminal panes, keeps track of every session, and gives each new session a short, stable brief of what happened before in the same project. Machines can pair with one self-hosted hub so that memory follows you across Windows, macOS and Linux.

It runs on Windows, macOS and Linux as a desktop app, and as a single `blirp` binary for servers and headless hubs. Licensed under Apache-2.0.

> Status: pre-1.0 (0.x). Data formats may still change between minor versions; `blirp doctor` checks an installation.

## What you get

- **One window for all your agents.** Projects are plain folders (git optional). Start any installed agent in a project, run several side by side, tile them in a grid, stop, resume or fork them. Each session shows its status (working, idle, waiting for you, completed, failed) and branch or folder.
- **The real TUIs.** Sessions run in a real pseudo-terminal (ConPTY on Windows, bundled with the app) rendered with xterm.js. blirp never proxies model traffic and never stores agent credentials; your agents keep their own logins and configs.
- **Automatic memory.** blirp reads the transcripts that agents already write to disk, distills finished work into a per-project brief plus decisions, open threads and gotchas, and injects that into the next session at startup. Older history stays searchable through the UI, the CLI (`blirp mem search`) and an MCP server the agents can call.
- **Your data, your machines.** Everything lives in `~/.blirp` (SQLite). Nothing leaves your machine unless you pair it with a hub you run yourself.
- **Optional hub.** Run blirp on an always-on machine (a Mac mini, a home server) and pair your laptops and desktops to it. Sessions and memory replicate through the hub, and you can open the UI from a phone on your LAN or over Tailscale.

The UI is a web app served by the local daemon: the desktop app is a thin native window around it, and `blirp open` shows the same UI in your browser.

## Install

Pick one; details, uninstall steps and troubleshooting are in [docs/install.md](docs/install.md).

| | Desktop app | Standalone CLI / daemon |
|---|---|---|
| Windows 10 1809+ (x64) | `blirp_<version>_x64-setup.exe` (per user) or `.msi` from [Releases](https://github.com/blirp/blirp/releases) | `blirp-<version>-x86_64-pc-windows-msvc.zip` |
| macOS 11+ (Apple silicon, Intel) | `blirp_<version>_aarch64.dmg` / `_x64.dmg` | `blirp-<version>-<arch>-apple-darwin.tar.gz` |
| Linux x64 / arm64 (glibc 2.35+) | `.AppImage`, `.deb` or `.rpm` | `blirp-<version>-<arch>-unknown-linux-gnu.tar.gz` |

Every release lists SHA-256 checksums (`SHA256SUMS.txt`, plus a `.sha256` next to each archive). Homebrew formula/cask and winget manifests are generated with each release; see [docs/install.md](docs/install.md) for their status.

The desktop app updates itself: on launch it checks GitHub Releases for a newer version signed with the project's update key and asks before installing.

## Quick start

1. Install and start the desktop app. It starts the blirp daemon in the background and opens the UI.
   Headless or CLI-only: run `blirp daemon --detach`, then `blirp open`.
2. **Projects > Add folder**, or just start a session in any folder with the orange **+** (Ctrl/Cmd+T).
3. Pick an agent (only agents found on your `PATH` are offered; `shell` always works) and optionally type a first prompt.
4. Work as usual. When a session goes idle or ends, blirp distills it. The next session in that project starts with the brief, open threads, recent decisions and a summary of the last few sessions already in its context.

Closing the window keeps the daemon and all sessions running; the tray icon reopens it. **Quit blirp** in the tray menu stops the daemon and ends every session (it asks first).

Useful commands:

```sh
blirp status              # is the daemon running, where, which version
blirp open                # open the UI in your browser, logged in
blirp sessions            # recent sessions
blirp doctor              # check data dir, config, database, git and detected agents
blirp service install     # start the daemon at login (per-user; see below)
```

## How memory works

1. **Ingest.** Agents already persist structured transcripts (Claude Code JSONL, Codex rollouts, opencode's SQLite store, ...). blirp tails those files incrementally and normalizes them into sessions and events. Terminal output is only used for the live view and status, never scraped for content. Sessions you start outside blirp are picked up too.
2. **Redact.** Every event is passed through secret redaction (cloud keys, tokens, private keys, JWTs, connection strings, `.env` lines, ...) before it is stored, synced or summarized.
3. **Distill.** When a session has been idle for a while or ends, a small summarizer run (your installed `claude` or `codex` CLI with a cheap model, or a local Ollama model; configurable, can be off) turns the new part of the transcript into a title, summary, decisions, open threads, gotchas and an updated project brief. There is a daily budget, and you can switch the brief to review mode so changes arrive as suggestions instead.
4. **Inject.** A new session gets a compact markdown block (hard size cap, stable order so agent prompt caches keep hitting) through the agent's own mechanism: a SessionStart hook for Claude Code, Codex, Gemini CLI and Cursor, an instructions entry for opencode, `--read` for Aider. Everything is visible and editable in the Memory panel.
5. **Recall on demand.** Agents with MCP support get `mem_search`, `mem_session`, `mem_recent`, `mem_brief` and `mem_record` tools for older history.

blirp never edits your agent config or project files for sessions it launches (it passes flags and environment). For sessions you start outside blirp there is an explicit, reversible opt-in: `blirp hooks install` / `blirp hooks uninstall`.

## Supported agents

| Agent | Launch, resume, status | Transcript ingest | Memory injection at start | MCP recall |
|---|---|---|---|---|
| Claude Code (`claude`) | yes, hook-accurate status | yes | SessionStart hook | yes |
| Codex CLI (`codex`) | yes | yes (incl. `.jsonl.zst`) | SessionStart hook on 0.155.1+, else instructions override | yes |
| opencode | yes | yes | instructions entry | yes |
| Gemini CLI (`gemini`) | yes | yes | SessionStart hook | yes |
| Cursor CLI (`cursor-agent`) | yes | yes | sessionStart hook | yes |
| Amp (`amp`) | yes | yes | memory file when the CLI supports it | yes |
| pi | yes (fresh relaunch on resume) | yes | system-prompt append where supported | CLI only |
| Aider (`aider`) | yes (fresh relaunch on resume) | `.aider.chat.history.md` | `--read` | no |
| DeepSeek Harness (`dsh`) | yes (fresh relaunch on resume) | best effort | when supported | yes |
| Any other command | yes, via `[[agents.custom]]` in config | no | `BLIRP_MEMORY_FILE` env | no |

Agent CLIs change their formats and flags often. blirp checks installed versions at runtime and degrades to "no memory" rather than breaking a session; unknown transcript lines are skipped and logged. If an integration misbehaves with your version, please open an issue with the output of `blirp doctor`.

Not supported: IDE-embedded agents (e.g. Copilot Chat in VS Code), cloud-only agents without a local CLI, and proxying or metering model traffic.

## Privacy and security

- Local first: all state is in `~/.blirp` on your machine. No telemetry, no accounts, no blirp servers.
- The daemon listens on `127.0.0.1` only. Local clients authenticate with a random token from `~/.blirp/runtime.json` (owner-only on Unix); the browser UI gets an HttpOnly, SameSite=Strict cookie. Mutations and WebSockets check `Origin`, and responses carry a strict CSP.
- Transcript text is redacted before storage, sync and summarization; logs never contain transcript text or secrets.
- Summaries are produced by tools you already use (your `claude`/`codex` CLI) or a local model, and can be turned off (`memory.summarizer = "none"`).
- Sync is end-to-end between your own devices over QUIC (iroh) with SPAKE2 pairing codes; a relay is used only for NAT traversal and can be disabled or self-hosted.
- The desktop window loads only the local UI; it exposes no native APIs to it, and other links open in your browser. Updates are verified against the project's minisign key before installing.

Report vulnerabilities privately; see [SECURITY.md](SECURITY.md).

## Self-hosting a hub

Run `blirp` on a machine that is always on, enable the hub role, and pair your other machines with a short code. Step by step, including autostart on macOS and Linux, `loginctl enable-linger` for headless Linux, LAN portal access and Tailscale: [docs/self-hosting.md](docs/self-hosting.md).

## Autostart

`blirp service install` registers a per-user autostart, never a system service (a system service would not see your agent logins):

- macOS: `~/Library/LaunchAgents/dev.blirp.daemon.plist` (restarted after a crash, not after a deliberate quit).
- Linux: `~/.config/systemd/user/blirp.service`, enabled with `systemctl --user enable --now`.
- Windows: an `HKCU\...\CurrentVersion\Run` entry that starts `blirp daemon --detach` without a window.

`blirp service status` shows what is installed; `blirp service uninstall` removes exactly that entry.

## Building from source

Requirements: Rust 1.89+ (CI uses 1.94), Node.js 22 and pnpm 12. For the desktop app also the [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/) (WebView2 on Windows, Xcode command line tools on macOS, `libwebkit2gtk-4.1-dev` and friends on Linux).

```sh
pnpm -C web install && pnpm -C web build     # the UI, embedded into the binary
cargo build --release -p blirp               # target/release/blirp
cargo run -p blirp -- daemon                 # run from source (honors BLIRP_HOME)
```

Desktop app:

```sh
pnpm -C app install
pnpm -C app tauri dev                        # uses target/debug/blirp as the daemon
scripts/build-sidecar.sh                     # or scripts/build-sidecar.ps1 on Windows
pnpm -C app tauri build                      # installers in target/release/bundle/
```

`tauri build` signs updater artifacts and therefore needs `TAURI_SIGNING_PRIVATE_KEY`; for a local unsigned build pass `--config '{"bundle":{"createUpdaterArtifacts":false}}'`.

Checks that CI runs: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `pnpm -C web check`, `pnpm -C web test`. See [CONTRIBUTING.md](CONTRIBUTING.md) and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
