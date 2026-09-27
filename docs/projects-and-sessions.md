# Projects and sessions

## Projects

A project is a place for related work and its memory (brief, records, wiki, resources): usually one or more folders, but a project can also have no folder at all. Every session in a project shares its memory. Git is optional and only adds features: branch display, the Git tab (status and diffs), and per-session worktrees. A folder without `.git`, for example a notes directory, works fully: sessions, ingest, memory and sync are the same.

Sessions that are not part of an actual project are **chats**. They are listed under **Chats** in Sessions, not among projects (see [Chats](#chats)).

### Projects without a folder

Some work does not live in a folder on your computer: a design in a desktop app you drive through its MCP server, a game edited in its own editor, research that is only notes. Create such a project with **New project** on the Projects page and give it just a name (and optionally a brief); leave the folder empty. Through the API: `POST /api/projects {"name": "...", "brief": "..."}`.

- An agent still needs a working directory, so sessions of the project start in a blirp workspace: `~/.blirp/workspaces/<project id>/`, an empty folder only you can read, created with the first session. The project page shows its path, labelled **blirp workspace**. Sessions started there outside blirp (from your own terminal) belong to the project too.
- The workspace is per machine and is not synced: each machine, including the hub for cloud sessions, starts the project's sessions in its own workspace. The project itself and its memory sync like any other project.
- The agents' MCP servers come from their own configuration, so a server you set up for your user (for example `claude mcp add --scope user`, or `[mcp_servers]` in `~/.codex/config.toml`) works in the workspace like anywhere else. For servers only this project should see, put the agent's project-level file into the workspace: `.mcp.json` for Claude Code, `.gemini/settings.json` for Gemini CLI, `.cursor/mcp.json` for Cursor, `opencode.json` for opencode. Open the folder from a session's toolbar (**Open folder**), or browse it on the project's **Files** tab.
- To work in a real folder instead (say the folder an app saves the project in), tick **Start in another folder** in the New session dialog and pick it: the folder is added to the project, so later sessions there, in blirp or not, belong to it. A project that has a folder no longer uses the workspace; remove its folders again and it does.

A project with folders can have them all removed (the **x** next to a folder on the project page, or `POST /api/projects/:id/folders/remove {"path": "..."}`): the project stays with its sessions and memory, and its new sessions start in the workspace. Only this machine's folders can be removed here; each machine removes its own.

### How a session finds its project

When a session starts (from blirp, or discovered in an agent's transcripts) blirp resolves its working directory:

1. A folder inside a project's blirp workspace belongs to that project.
2. If the folder is inside a folder already registered on this machine, that project is used (nearest match wins).
3. Else, if the folder is inside a git work tree, the repository's top level becomes the project root. If another machine already has a project with the same normalized remote (`host/owner/repo`), this folder joins that project, so a repo cloned on two machines shares one memory.
4. Else, for a session you start from blirp with **Folder path**, the folder itself becomes a new project named after it: you picked it. A session found in transcripts or reported by hooks only makes a project of an actual project folder: the nearest folder from its working directory up that holds a project file (see below). Without one it is a chat.
5. Your home directory and filesystem roots are never registered as project roots. For sessions started outside blirp neither are scratch places: the temp folder, the Windows folder (`C:\Windows`, including `System32`), a hidden folder directly in your home directory (tool and agent data such as `~/.codex`, `~/.claude` or `~/.blirp`, and anything below them), your Desktop, Downloads and Documents folders themselves, and Codex desktop chat folders (`~/Documents/Codex/<YYYY-MM-DD>/<chat>`, also in a Documents folder moved to OneDrive). On macOS and Linux `/tmp` and `/var/tmp` count as temp folders too. Sessions there are chats. A folder you registered yourself (step 2) always wins, so **New project** with a folder makes a real project out of any of these places.

Project files (matched in any letter case): `package.json`, `deno.json`, `deno.jsonc`, `Cargo.toml`, `go.mod`, `pyproject.toml`, `setup.py`, `Pipfile`, `requirements.txt`, `pom.xml`, `build.gradle`, `build.gradle.kts`, `build.sbt`, `Gemfile`, `composer.json`, `mix.exs`, `Package.swift`, `pubspec.yaml`, `CMakeLists.txt`, `meson.build`, `stack.yaml`, `deps.edn`, `project.clj`, `project.godot`, `default.project.json`; files ending in `.sln`, `.csproj`, `.fsproj`, `.vbproj`, `.vcxproj`, `.cabal`, `.gemspec`, `.xcodeproj`, `.xcworkspace`, `.uproject`; other version control (`.hg`, `.svn`, `.jj`); and agent setup written for the folder (`.mcp.json`, `AGENTS.md`, `CLAUDE.md`).

Worktree sessions resolve to their main repository's project. Transcripts whose folder no longer exists are matched by path prefix (or, for Codex, by the recorded git remote); otherwise they are chats, since a folder that is gone cannot show it was a project. On Windows spellings of the same folder (case, `/` or `\`, a `\\?\` prefix, a trailing separator) match the same project.

After upgrading, projects that earlier versions created for scratch places (the temp and Windows folders, hidden tool folders in your home directory, Codex chat folders, Desktop, Downloads and Documents themselves) are merged into Chats once, shortly after the daemon first starts (on a machine paired to a hub, after it has caught up with the hub): their sessions and summarized records move to Chats and the project disappears from the list on every paired machine. Their briefs are not carried over. Projects of plain folders (no git, no project file) are never moved on their own, since blirp 0.1.0 did not record which folders you added yourself: the Projects page lists the ones that look like chats once, with a checkbox each, and moves the ones you keep ticked when you click **Move to Chats** (**Keep them as projects** hides the offer for good; `GET /api/projects/chat-candidates`, `POST /api/projects/:id/to-chats`). Neither happens to a project you renamed, merged another project into, removed a folder of, started a session in from blirp, pinned a record in, or wrote memory for yourself (a record, a brief version, a wiki page or a resource), nor to one that is a git repository with a remote, has a project file, has no sessions, has folders or sessions of another machine, or whose folder is gone or not reachable (an unmounted drive).

### Chats

Chats are the sessions that belong to no project: everything you run in your home folder, a scratch folder or a folder that is no project. The Sessions list shows them in one **Chats** group (over all paired machines), search finds them, and they are summarized like other sessions, but:

- chats share no memory: nothing is injected when one starts, and distilling a chat writes only its own summary (no brief, no records);
- **Move** in the session toolbar (the folder icon) files a session into a project, into a **New project** (created without a folder), or back into Chats. Its subagent sessions and the records it produced move with it; into Chats only its summarized records do, while records you wrote or pinned stay in the project. A session of another machine moves into that machine's Chats, once that machine has one (it creates it with its first chat). Subagents that show up later follow their parent, and a moved session stays where you put it. Moving never registers the session's folder; run **Distill now** afterwards to add what the chat decided to the project's memory.

Each machine keeps its chats in a bucket of its own (a project flagged `chats`, id `chats-<machine id>`, called `Chats (<machine name>)`); the UI never lists it among projects, and it cannot be deleted or merged. blirp 0.1.0 filed such sessions in a `Home (<machine name>)` project: after upgrading it is merged into Chats, unless you renamed it or wrote memory for it (a brief version, a record, a pinned record, a wiki page or a resource): then it stays a project and Chats starts empty. API: `POST /api/sessions/:id/move {"project_id": "<id>" | null}`.

### Managing projects

- **New project** on the Projects page: a name, a folder (any absolute path on this machine), or both, and an optional brief.
- **Rename** on the project page (pencil next to the name).
- **Remove folder** (the **x** next to one of this machine's folders): unregisters it, the project stays.
- **Delete** unregisters the project's folders and hides it. Its sessions and memory stay in the database, but a new session in one of its folders starts a new project.
- **Merge into…** moves folders, sessions, records, wiki pages and resources into another project. Use it to join the same non-git folder on two machines, since those cannot be matched by remote.

The project page has tabs: Overview (brief, open threads, recent sessions, a prompt box to start a session), Sessions, Memory, Wiki, Resources, Files (read-only browser, text files up to 1 MiB; the workspace for a project without folders) and Git (git projects only).

## Sessions

A session is one run of an agent (or shell, or custom command) in a folder. blirp-launched sessions run in a pseudo-terminal owned by the daemon: closing the window or the tab (Ctrl/Cmd+W) does not stop them, and several browsers or windows can attach to the same terminal at once (the last resize wins).

### Pasting images and files

Paste a screenshot or a copied file into a terminal pane (Ctrl+V or Ctrl+Shift+V on Windows, Ctrl+Shift+V on Linux, Cmd+V on macOS, or the context menu's Paste), or drag files from your desktop onto the pane. blirp uploads each file to the machine that runs the session (also a cloud or other paired machine) and types its path into the terminal, the way a native terminal types a dropped file's path:

- Claude Code and Codex attach an image whose path is pasted like this, as they do for a file dropped onto a native terminal. For other files, and other agents, the path is there to use in your prompt ("read this log: <path>").
- Files are saved as `~/.blirp/uploads/<session>/<time>-<name>` (name reduced to letters, digits, `.`, `-`, `_`) on that machine, readable only by your user. They are deleted when you delete the session and after 7 days, and are never synced.
- At most 25 MB per file and 500 MB per session. Folders cannot be dropped, only files. Plain text pastes work exactly as before; when the clipboard holds both text and a picture of it (copying cells or paragraphs from an office app), the text is pasted.
- On Linux (and macOS) plain Ctrl+V still goes to the program in the terminal, as in native terminals: Claude Code then reads the clipboard of the machine it runs on, which only works for sessions on this machine. On Windows plain Ctrl+V pastes, as in Windows Terminal (Claude Code's own image paste there is Alt+V, which still reaches it).
- Needs **Terminal control** on portal devices, like typing.

### The Sessions list

The Sessions sidebar lists every session this machine knows: its own, and on paired machines the other machines' sessions, which replicate. Order:

- Running sessions (Starting, Working, Idle, Waiting) come first: a process is attached, so they are the ones you can act on, even after hours of idling. Another machine's running session stays on top while that machine is connected to the hub, however long it idles. When the machine goes offline its session sorts by its last activity and its chip reads **Offline** (hover it for the last status the machine reported and when it was last seen), since nothing can correct that status until it is back. When this machine cannot tell (it is not connected to the hub itself, or the hub runs an older blirp), a running session of another machine stays on top until 30 minutes pass without an update, then reads **No update**.
- Then by most recent activity, not by start time: for sessions started outside blirp the time of the latest transcript event, for sessions blirp launched the last status change (working, idle, waiting, ended) reported by hooks or detected from terminal output. So a session started yesterday that is working now is not buried.

Sessions are grouped by project, groups ordered by their first session in that order. A group shows its first 5 sessions, plus any running one and the one that is open; **Show N more** lists the rest. The sidebar loads the first 200 sessions and **Load older sessions** fetches the next 200. The previous/next session shortcuts follow the cards as the sidebar shows them.

The filter box matches title, folder, branch and agent over all sessions (it asks the daemon, so it also finds sessions not loaded yet). On paired machines each card shows the machine it runs on, this machine's included, and a machine picker narrows the list to one machine. The project page's Sessions tab lists one project's sessions in the same order, with a status filter.

External sessions (started in your own terminal) show up while they run, marked `external`; see [External sessions](#external-sessions).

### Statuses

| Status | Meaning |
|---|---|
| Starting | Process is being spawned. |
| Working | Agent is busy: a prompt-submit hook fired, or (without hooks) output arrived in the last 2 s. For external sessions: the transcript changed in the last 2 minutes. |
| Idle | Agent finished its turn (Stop hook), or no output for 2 s. |
| Waiting | Agent needs you: permission prompt or idle notification (only agents with a Notification hook: Claude Code and Gemini CLI). |
| Completed | Process exited with code 0, or you stopped it, or the agent reported session end. External sessions become Completed when their transcript goes quiet. |
| Failed | Process exited with a non-zero code, or could not be spawned. |
| Detached | The daemon stopped (or restarted) while the session was running. The process is gone; use Resume. |

A session you stop with **Stop** (UI or `blirp stop <id>`) is recorded as Completed with `stopped_by_user` set, whatever exit code the agent returns, and its chip reads **Stopped**, so a deliberate stop is distinguishable from the agent finishing on its own. Stop kills the whole process tree (Windows job object; process group SIGHUP then SIGKILL after 3 s on macOS/Linux). Only sessions blirp launched can be stopped from blirp; external sessions belong to the terminal that started them.

### Notifications

blirp tells you when a session becomes **Waiting** (needs input), **Completed** or **Failed** and you are not looking at it. Nothing fires for the session on screen in a focused window, or for a session you stopped yourself. Choose the events, a sound (off by default) or turn it all off under **Settings > Appearance > Notifications**; the choice is saved per browser (or per desktop app).

- **Window in front, another page or session on screen:** a toast in blirp with **Open**.
- **Window in the background or minimized:** a system notification and a count in the window title and taskbar button (`(2) blirp`) until you look at those sessions; in a browser the tab icon also gets a dot.
  - **Desktop app:** a native notification (Windows notifications, macOS Notification Center, the Linux notification daemon over D-Bus), and the taskbar button flashes (the dock icon bounces on macOS). Clicking the notification does not open the session; the flashing window does.
  - **Browser** (`blirp open`, the LAN portal): the browser's own notifications. Allow them with **Enable desktop notifications** in Settings, or with the **Enable** offer blirp shows the first time one would have fired. Browsers only allow them on `https://` pages and on `localhost`/`127.0.0.1`. Clicking one opens the session.

**Send test notification** shows what happened: which service got it, or why the browser refused (never asked, blocked, unsupported). When it says sent but nothing appears, the OS is holding it back:

- **Windows:** Settings > System > Notifications: notifications on, **blirp** (desktop app) or your browser allowed, and Do not disturb / Focus off (or blirp added to its priority list). The desktop app registers itself as a notification sender for your user on start; `blirp uninstall` removes that registration.
- **macOS:** System Settings > Notifications > blirp (or your browser): Allow notifications, and Focus off.
- **Linux:** a notification daemon must run (GNOME, KDE and most desktops have one; bare window managers need e.g. `dunst` or `mako`).

### Worktrees

For git projects, **Run in a new git worktree** (or `[sessions] worktree_default = true`) runs `git worktree add ~/.blirp/worktrees/<project-id>/<adjective-animal-xxxx> -b blirp/<adjective-animal-xxxx>` and starts the session there, in the same subfolder you picked. The session shows its branch. blirp never removes worktrees or branches; merge the branch and clean up with `git worktree remove <path>` and `git branch -d blirp/<name>` when you are done. For non-git projects the option is hidden, and the config default is ignored.

### Resume

**Resume** is offered for ended sessions on this machine that either have the agent's own session id or were launched by blirp. It reuses the same session row:

- Claude Code `--resume <id>`, Codex `resume <id>`, opencode `--session <id>`, Gemini CLI `--resume <id>`, Cursor CLI `--resume <id>`, Amp `threads continue <id>`: the agent continues its conversation.
- pi, Aider, dsh, shell and custom agents (and sessions without a known id) relaunch fresh in the session's folder.

External sessions with a known id can be resumed too, which brings them into a blirp terminal. Resuming fails if the folder no longer exists or the session is still running. Sessions of another paired machine are resumed on that machine (see [sync-and-hub.md](sync-and-hub.md#remote-sessions)).

### Continue in / fork

The agent chip in the session toolbar opens **Continue in...**: start a new session with any installed agent that picks up where this one left off. **Start new session from this session** is the same with the same agent: a fresh session that starts with this session's handoff. Both work on live and ended sessions.

The new session gets a handoff pack appended to its injected memory: the source session's title, agent, date and folder, its summary, the project brief, its last 12 user/assistant turns (each up to 1 500 characters), files it touched and the open threads, capped at 12 000 characters (oldest turns are dropped first). The pack is also saved as `~/.blirp/launch/<new-session>/handoff.md`. Unless you give a prompt, the first prompt is "Continue the work described in the blirp handoff above.", or for agents without memory injection "Read the blirp handoff in <path to handoff.md> and continue the work described there." (no prompt for Shell). The new session records the source as its parent (`parent_session_id`).

The summary in the pack is brought up to date first: when the source session has done something since it was last summarized, blirp summarizes it before starting the new session, so the button reads **Summarizing session...** for a moment (at most 90 seconds, usually much less). That run counts against the daily distill budget like any other. When it cannot run (the summarizer is off or paused, today's budget is used up, it fails or takes too long), the new session starts anyway with the last summary, and the pack says so and why; its last turns are always current. A session of another machine (including a cloud session) is summarized by that machine, so its handoff uses the summary synced from there; starting a new session on another machine (a cloud session) from a session of this one summarizes it here first. While one new session from a session is being started, a second click (another tab or device) is refused with a notice.

When the agent of a running session compacts its context (Claude Code, Codex, opencode and pi record a compaction in their transcript when the context window fills up), the session page shows a note with the same **Start new session from this session** button: a fresh session starts with room in its context and the handoff instead of a lossy summary of a summary. **Dismiss** (x) hides it for that session in this browser until the agent compacts again. For the other agents blirp reads no compaction from the transcript, so they get no suggestion.

### Subagents

When an agent spawns subagents, blirp ingests them as child sessions of the parent: Claude Code `subagents/agent-*.jsonl`, opencode sessions with a parent, Gemini CLI `chats/<parentId>/`, Cursor `subagents/`. They do not clutter the session lists: the parent's card shows an expandable "N subagents" toggle that lists them. Subagents are not distilled on their own (the parent transcript already contains each subagent's task and final report), but they are searchable, and **Distill now** works on them manually.

In the API, `GET /api/sessions?parent=<id>` lists a session's children, and `include_children` controls whether subagent sessions appear in a normal listing (see [api.md](api.md#sessions)). Continue-in and fork sessions also record their source as parent but are regular top-level sessions.

### External sessions

Sessions started outside blirp (your own terminal, an IDE terminal, a script) are discovered from the agents' transcript stores: file watchers plus a full rescan every 5 minutes and at startup. With global hooks installed (`blirp hooks install`), the agent also reports the transcript path and status directly, so the session appears within about a second. External sessions:

- have origin `external`, are shown read-only (transcript, summary) with Resume when the agent supports it,
- are distilled like blirp sessions, so their work reaches the next session's memory,
- are never marked Detached by a daemon restart.

A session launched by blirp is linked to its transcript by the agent session id (Claude Code, set at launch) or by agent + folder + first event within 60 s of launch.

Aider has no central store: its `.aider.chat.history.md` is read only in folders registered as projects on this machine.

### Titles, cost and history

Titles come from the agent (Claude's AI title, opencode/pi/Amp/Cursor names, Gemini summaries, dsh titles), else the first prompt, The distiller sets a title only when the session has none. Rename a session through the API (`PATCH /api/sessions/:id {"title": ...}`). Tokens and cost are the transcript's own totals when it records cost (Claude Code, opencode, pi, Aider), else an estimate from a static, dated price table (unknown models cost 0).

History is permanent: deleting an agent's transcript file never deletes anything in blirp.
