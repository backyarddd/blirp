# Changelog

All notable changes to blirp are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and blirp adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, data
formats can still change between minor versions.

The release workflow publishes the section of a version as its release notes,
so every tag needs one.

## [Unreleased]

### Added

- Paste images and files into a terminal pane, or drop files onto it: they
  are saved on the machine running the session (also through the hub) and
  their paths typed into the terminal, so Claude Code and Codex attach
  pasted screenshots. `POST /api/sessions/:id/uploads` (terminal control,
  25 MB per file); uploads are removed with the session and after 7 days.

### Fixed

- The web UI header, sign-in screen and favicon show the same "b" logo as
  the desktop app icon instead of an older mark.
- Sessions found in transcripts no longer create a project for scratch
  folders: the temp folder, the Windows folder, hidden tool folders in the
  home directory (`~/.codex`, ...) and Codex desktop chat folders
  (`~/Documents/Codex/<date>/<chat>`). They go to the machine's Home project.
  Projects created for them earlier are merged into Home once, unless you
  renamed, used or edited them.
- Transcripts of a folder that no longer exists no longer create a second
  project when the folder is spelled differently (case, separators, `\\?\`).

## [0.1.0] - 2026-09-26

First release: a desktop app and daemon that runs CLI coding agents in real
terminals, tracks every session and gives each new session memory of the work
done before it.

### Added

- Daemon (`blirp daemon`) supervising agent sessions in pseudo-terminals
  (ConPTY on Windows): sessions outlive the UI, reattach with the screen and
  10 000 lines of scrollback, and stop with their whole process tree.
- Agents: Claude Code, Codex, opencode, Gemini CLI, Cursor CLI, Amp, pi,
  Aider, DeepSeek Harness (`dsh`), a plain shell and custom commands; resume
  by id where the agent supports it.
- Projects are folders; git is optional. Git projects get branch, status and
  diff views and an optional worktree per session, removed on request
  (`blirp worktrees list|prune`).
- Session tracking with status from hooks and activity, tokens and cost, and
  ingest of the agents' own transcripts, so sessions started outside blirp
  (and their subagents) appear too.
- Automatic memory: redacted transcripts are distilled by a summarizer you
  choose (`claude`, `codex`, a local Ollama model, or none) into a project
  brief, records, wiki pages and suggestions, injected at session start
  through each agent's own mechanism. Memory is searchable in the UI, with
  `blirp mem` and through an MCP server (`blirp mcp`, `/mcp`).
- Opt-in global hooks and MCP entries for sessions started in your own
  terminal (`blirp hooks install|uninstall`), reversible and limited to
  blirp's own entries.
- Continue in / fork: start a session with any agent from a handoff pack of
  an earlier one.
- Web UI: terminal tabs and a grid of live sessions, memory, wiki,
  resources, files, git, search, settings and a command palette.
- Desktop app (Tauri) around the daemon UI, with a tray icon and `blirp://`
  links.
- Optional self-hosted hub: pair machines with an 8-character code
  (SPAKE2), replicate sessions and memory over QUIC (iroh), start cloud
  sessions on the hub and attach to them from any paired machine.
- LAN HTTPS portal on the hub for phones and other browsers, with QR login
  and per-device permissions.
- CLI: `status`, `stop`, `logs`, `open`, `sessions`, `doctor`, `service
  install|uninstall|status`, `pair`, `hub`, `devices`, `update` and
  `uninstall`.
- Installers: `install.sh` (macOS, Linux) and `install.ps1` (Windows) with
  SHA-256 and minisign verification, `blirp update` (signature-verified,
  all-or-nothing) and `blirp uninstall`; classic NSIS, MSI, DMG, deb, rpm
  and AppImage packages.

[Unreleased]: https://github.com/backyarddd/blirp/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/backyarddd/blirp/releases/tag/v0.1.0
