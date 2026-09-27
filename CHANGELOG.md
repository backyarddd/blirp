# Changelog

All notable changes to blirp are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and blirp adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, data
formats can still change between minor versions.

The release workflow publishes the section of a version as its release notes,
so every tag needs one.

## [Unreleased]

### Added

- Continue in / Start new session from this session summarizes the source
  session first when it has moved on since its last summary, so the handoff
  carries a current summary instead of one up to 5 minutes old (or none).
  The launch waits at most 90 s and the button reads "Summarizing
  session..." meanwhile; when the summary cannot be refreshed (summarizer
  off or paused, daily budget used up, failure, timeout, or a session of
  another machine, which that machine summarizes) the session starts anyway
  and the handoff says why.
- When the agent of a running session compacts its context (Claude Code,
  Codex, opencode, pi), the session page suggests starting a new session from it.
  Dismissible per session until the next compaction. Sessions carry the time
  of their latest compaction (`compacted_at`; database migration 12).
- Codex Desktop subagents are listed under their parent session, and a
  forked subagent no longer repeats its parent's conversation (the copy of
  the parent's history at the start of its rollout is skipped). A one-time
  repair after upgrading links the subagents already in blirp to their
  parents, removes the repeated conversation from forks on every synced
  machine, fixes their titles, and removes memory records only a fork's
  own summary produced.

### Changed

- The sync protocol version now also versions what is replicated: hub and
  nodes that replicate different fields refuse each other until both run the
  same release, instead of silently dropping the fields one side does not
  know. Update the hub and every node from 0.2.0 together.

### Fixed

- Nodes that still ran 0.1.x while the hub already ran 0.2.0 showed the
  hub's Chats as a normal project ("Chats (<hub>)") and lost which project a
  merged project went into. Every machine re-sends its projects once after
  updating, which repairs them.
- Project file sync: an empty file no longer keeps its whole folder from
  uploading, and copies made elsewhere get empty files too.
- Project file sync: a project folder that is no longer on disk (deleted,
  moved, an unplugged drive or a share that went away) shows "Folder
  missing" instead of an error on every rescan. Nothing is watched,
  uploaded or recorded for it, the log names it once, and it syncs again
  when it is back. Bring changes here and Update from hub refuse it.
- Project file sync: a folder that comes back emptied (for example a
  project's workspace made again after it was deleted) never deletes
  files on the hub on its own, even a single one: it waits for Restore
  from hub or Delete on hub too. A workspace takes its files back from
  the hub by itself. This also holds for a folder emptied and made again
  while blirp was not running.
- Project file sync: a project deleted, or a folder removed, on another
  machine stops syncing on this one at once instead of after the next
  10 minute rescan.
- Project file sync on Windows: files that real-time protection was still
  scanning no longer fail to update with "Access is denied". A crash in the
  middle of such an update puts the old file back on the next scan instead
  of deleting it on the hub.
- Project file sync: a project without folders whose workspace was first
  opened from the Files tab uploads right away instead of after up to 10
  minutes.
- Project file sync: upload errors are logged when they start or change,
  not on every pass, and name the folder.
- The hub's storage quota no longer loses track of uploads that stop
  midway or run twice at once, and partial downloads left by stopped
  transfers are removed after a day.
- A long session started outside blirp is no longer distilled at every
  short pause (up to the whole daily budget): a session marked completed
  after two quiet minutes now waits `memory.distill_idle_secs` like an idle
  one, and a session is distilled again only when a new prompt or reply
  arrived since its last distill.
- Claude Code's "You've hit your session limit" (and weekly limit) is
  recognized as a usage limit: automatic distilling pauses and the budget
  unit is given back instead of the session failing.
- Scripted agent runs (`claude -p`, the Claude Agent SDK, `codex exec`) no
  longer become sessions: bots and automation that call an agent in a loop
  created a session (and sometimes a project) per run, used up the distill
  budget and filled project memory. A scripted run resumed interactively
  still becomes a session, and sessions blirp launched are always kept.
  A run with a second prompt (an app you chat in over the Agent SDK) is a
  session too. On first start, sessions of such runs stored before are
  removed like a user delete, with the records the distiller made from them
  (pinned or edited ones stay) and projects that held nothing else.
- Background-task notifications and other lines Claude Code writes itself
  are no longer shown as your prompts and no longer make a session due for
  distilling again.
- A distill reply with keys outside the output contract is accepted (the
  extra keys are ignored) instead of failing and costing a retry.

## [0.2.0] - 2026-09-27

Project files sync through the hub, projects without a folder, Chats for
sessions outside projects, a one-command VPS hub, agent skills, in-app
updates and working notifications.

### Added

- Project files on the hub ([docs/project-files.md](docs/project-files.md)):
  paired machines upload their project folders to the hub, uncommitted
  changes included, leaving out secrets, build output and ignored files
  (`.gitignore`, `.blirpignore`). Cloud sessions can start from a hub copy
  and other machines can download a copy (clone plus the uploaded files);
  edits sync back, concurrent edits are kept as conflict copies, and your own
  folders change only when you click Bring changes here. Per-project
  Default/On/Off, Preview, Pause, first-run grace period and banner,
  `[sync] project_files` and a `[files]` config section. Portal browser
  devices need the new Files permission for project files.
- Paste images and files into a terminal pane, or drop files onto it: they
  are saved on the machine running the session (also through the hub) and
  their paths typed into the terminal, so Claude Code and Codex attach
  pasted screenshots. `POST /api/sessions/:id/uploads` (terminal control,
  25 MB per file); uploads are removed with the session and after 7 days.
- Agent Skills for operating blirp: `blirp skills install|uninstall|list|refresh`
  puts `SKILL.md` skills (status and troubleshooting, linking machines,
  updating, memory, sessions) into `~/.claude/skills` and
  `~/.agents/skills` (Claude Code, Codex, Gemini CLI, opencode, Cursor,
  Amp, pi), or a project with `--project`. Skills you edited are never
  overwritten without `--force` nor removed; `blirp update` refreshes
  unedited ones, `blirp doctor` shows their state and `blirp uninstall`
  removes them.
- The version is easy to find: hover the blirp logo in the top bar, or look
  under the Settings sections.
- Update notice: when a newer release is out, the Settings button gets a dot
  and a banner offers **Update now** (script installs) or **How to update**
  (installers and packages), with the release notes. Dismissing it hides it
  until the next release. Settings > About has **Check now** and shows the
  last update's result.
- **Update now** runs `blirp update` in the background: the daemon restarts
  with the new version and the page reloads (a browser tab signs in again
  with `blirp open`). If anything fails, the old version keeps running and
  About shows why. The desktop app then offers **Restart app** to relaunch
  its own new version, a stale browser page **Reload**. New routes `POST /api/update/check` and
  `POST /api/update/apply` (admin; apply only on the machine's own
  listener).
- `blirp update` prints the version before and after and records each
  attempt in `logs/update.log`.
- Projects without a folder: **New project** takes just a name (and an
  optional brief). Their sessions start in a private per-machine blirp
  workspace (`~/.blirp/workspaces/<project id>`), where an agent's own
  project MCP file (such as `.mcp.json`) can go; memory syncs as for any
  project. A folder picked with **Start in another folder** joins the
  project. `POST /api/projects {name, brief?}`, `LaunchSession.add_folder`.
- Remove a folder from a project (the **x** next to it, or
  `POST /api/projects/:id/folders/remove`); the project stays, also with no
  folder left.
- Chats: sessions that are part of no project are listed under **Chats** in
  Sessions instead of as a project, are searchable and summarized, and share
  no memory. Sessions found in transcripts join a project only in an actual
  project folder (git, or a project file such as `package.json`,
  `Cargo.toml` or `.mcp.json`); a folder you pick in blirp still becomes one.
  Move a session into a project, a new project or back to Chats from its
  toolbar (`POST /api/sessions/:id/move`). Projects of plain folders that
  look like chats are offered once on the Projects page to move to Chats;
  nothing moves without your confirmation.
- Run the hub on a VPS: `install.sh --hub` installs the CLI and runs the new
  `blirp hub setup`, which installs the autostart service with systemd
  linger (so the hub survives logout and reboots), turns LAN discovery and
  (on Linux) keep-awake off, enables the hub and prints an invite and the next steps. It refuses to run
  as root, and without systemd (containers) it says so and starts the daemon
  directly. Guide: `docs/vps.md`.
- `blirp backup <file>` writes a consistent copy of the database while the
  daemon runs.
- `blirp doctor` shows the sync role, the autostart service, systemd linger
  (Linux) and whether a hub or node is reachable through a relay;
  `GET /api/sync/status` reports the relay (`relay_url`).

### Changed

- LAN portal browser devices no longer read project files by default: the
  project file browser, file contents, git diffs and project file sync need
  the new **Files** permission (Settings > Machines & Sync > Devices; off for
  existing and new devices), answering 403 `files_not_allowed` without it.
- `blirp service install` on Linux prints the `sudo loginctl enable-linger`
  hint only when linger is off, naming the user, and finds the user's
  systemd without `XDG_RUNTIME_DIR` (after `sudo -iu`) when linger is on.

- `memory.summarizer = "auto"` now uses your default agent for new sessions
  (`agents.default`) when that is Claude Code or Codex and it is installed
  and signed in; otherwise it falls back to `claude`, then Ollama, as
  before. Settings > Memory shows what Automatic uses right now and why
  (`GET /api/settings/summarizer`).
- The `claude` summarizer runs Sonnet instead of Haiku, for better summaries
  and briefs. Each run uses more of your Claude plan or API spend; the daily
  run limit (`memory.daily_distill_limit`, default 40) still caps it.

### Fixed

- The web UI header, sign-in screen and favicon show the same "b" logo as
  the desktop app icon instead of an older mark.
- Sessions found in transcripts no longer create a project for scratch
  folders: the temp folder, the Windows folder, hidden tool folders in the
  home directory (`~/.codex`, ...) and Codex desktop chat folders
  (`~/Documents/Codex/<date>/<chat>`). They are chats now, as are sessions
  in other folders that are no project. Projects created for them earlier are
  merged into Chats once, unless you renamed, used or edited them. The
  machine's Home project is merged into its Chats, unless you renamed it or
  wrote memory for it; then it stays a project.
- The Sessions list is ordered by most recent activity, with running
  sessions on top, instead of by start time, so a long session that is
  working now is not buried under newer finished ones. Another machine's
  running session stays on top only while that machine is online; while
  it is offline its chip reads "Offline". The hub now reports which
  machines are connected (`online` in `GET /api/machines`, also shown in
  Settings > Machines & Sync). `GET /api/sessions`
  (and `blirp sessions`) use the same order; cursors from 0.1.0 are
  rejected with 400.
- The Sessions sidebar no longer stops silently at the newest 200 sessions:
  **Load older sessions** pages through all of them, and the filter box
  searches every session through the daemon, not only the loaded ones.
- One busy project no longer pushes every other project out of the Sessions
  sidebar: each project group shows its 5 most recent sessions (plus any
  running or open one) with **Show N more**.
- On paired machines every session in the sidebar is labeled with the
  machine it runs on, this machine's included, and a machine picker filters
  the list, so a mixed list no longer reads as if only the other machine's
  sessions were shown.
- Transcripts of a folder that no longer exists no longer create a second
  project when the folder is spelled differently (case, separators, `\\?\`).
- Notifications work in the desktop app: it shows native notifications
  (Windows, macOS, Linux) and flashes its taskbar button, instead of relying
  on the webview's Web Notification API, which never reached the OS. On
  Windows the app registers itself as a notification sender, without which
  Windows dropped its notifications. In the browser, permission is asked
  from an explicit **Enable desktop notifications** button (and a one-time
  offer), with the reason shown when it is blocked. They are on by default,
  never fire for the session on screen or one you stopped, and come with
  in-app signals that need no permission: a toast, `(n) blirp` in the window
  title (and taskbar button) and, in a browser, a dot on the tab icon. Settings > Appearance > Notifications picks the
  events (needs input, finished, failed), an optional sound, and has
  **Send test notification**.
- A terminal pane opened just as its session ended shows that it exited
  instead of flashing "Reconnecting" and retrying: attaching to an ended
  session now sends its exit instead of a 404.

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

[Unreleased]: https://github.com/backyarddd/blirp/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/backyarddd/blirp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/backyarddd/blirp/releases/tag/v0.1.0
