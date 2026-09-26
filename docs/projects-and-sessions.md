# Projects and sessions

## Projects are folders

A project is one or more folders. Memory (brief, records, wiki, resources) belongs to the project, so every session started anywhere inside its folders shares it. Git is optional and only adds features: branch display, the Git tab (status and diffs), and per-session worktrees. A folder without `.git`, for example a design tool's MCP working folder or a notes directory, works fully: sessions, ingest, memory and sync are the same.

### How a folder becomes a project

When a session starts (from blirp, or discovered in an agent's transcripts) blirp resolves its working directory:

1. If the folder is inside a folder already registered on this machine, that project is used (nearest match wins).
2. Else, if the folder is inside a git work tree, the repository's top level becomes the project root. If another machine already has a project with the same normalized remote (`host/owner/repo`), this folder joins that project, so a repo cloned on two machines shares one memory.
3. Else the folder itself becomes a new project named after it.
4. Your home directory and filesystem roots are never registered as project roots. Sessions there go to a per-machine project named `Home (<machine name>)`.

Worktree sessions resolve to their main repository's project. Transcripts whose folder no longer exists are matched by path prefix, or get a project at the recorded path.

### Managing projects

- **Add folder** on the Projects page registers a folder explicitly (any absolute path on this machine).
- **Rename** on the project page (pencil next to the name).
- **Remove** unregisters the project's folders and hides it. Its sessions and memory stay in the database, but a new session in that folder creates a new, empty project.
- **Merge** (API only for now: `POST /api/projects/:id/merge {"into": "<id>"}`) moves folders, sessions, records, wiki pages and resources into another project. Use it to join the same non-git folder on two machines, since those cannot be matched by remote.

The project page has tabs: Overview (brief, open threads, recent sessions, a prompt box to start a session), Sessions, Memory, Wiki, Resources, Files (read-only browser, text files up to 1 MiB) and Git (git projects only).

## Sessions

A session is one run of an agent (or shell, or custom command) in a folder. blirp-launched sessions run in a pseudo-terminal owned by the daemon: closing the window or the tab (Ctrl/Cmd+W) does not stop them, and several browsers or windows can attach to the same terminal at once (the last resize wins).

### Pasting images and files

Paste a screenshot or a copied file into a terminal pane (Ctrl+Shift+V on Windows and Linux, Cmd+V on macOS, or the context menu's Paste), or drag files from your desktop onto the pane. blirp uploads each file to the machine that runs the session (also a cloud or other paired machine) and types its path into the terminal, the way a native terminal types a dropped file's path:

- Claude Code and Codex attach an image whose path is pasted like this, as they do for a file dropped onto a native terminal. For other files, and other agents, the path is there to use in your prompt ("read this log: <path>").
- Files are saved as `~/.blirp/uploads/<session>/<time>-<name>` (name reduced to letters, digits, `.`, `-`, `_`) on that machine, readable only by your user. They are deleted when you delete the session and after 7 days, and are never synced.
- At most 25 MB per file. Plain text pastes work exactly as before; when the clipboard holds both text and a picture of it (copying cells or paragraphs from an office app), the text is pasted.
- Plain Ctrl+V (Windows and Linux) still goes to the program in the terminal: Claude Code then reads the clipboard of the machine it runs on, which only works for sessions on this machine.
- Needs **Terminal control** on portal devices, like typing.

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

Browser notifications (Settings > Appearance) fire when a session becomes Waiting, Completed or Failed while blirp is in the background.

### Worktrees

For git projects, **Run in a new git worktree** (or `[sessions] worktree_default = true`) runs `git worktree add ~/.blirp/worktrees/<project-id>/<adjective-animal-xxxx> -b blirp/<adjective-animal-xxxx>` and starts the session there, in the same subfolder you picked. The session shows its branch. blirp never removes worktrees or branches; merge the branch and clean up with `git worktree remove <path>` and `git branch -d blirp/<name>` when you are done. For non-git projects the option is hidden, and the config default is ignored.

### Resume

**Resume** is offered for ended sessions on this machine that either have the agent's own session id or were launched by blirp. It reuses the same session row:

- Claude Code `--resume <id>`, Codex `resume <id>`, opencode `--session <id>`, Gemini CLI `--resume <id>`, Cursor CLI `--resume <id>`, Amp `threads continue <id>`: the agent continues its conversation.
- pi, Aider, dsh, shell and custom agents (and sessions without a known id) relaunch fresh in the session's folder.

External sessions with a known id can be resumed too, which brings them into a blirp terminal. Resuming fails if the folder no longer exists or the session is still running. Sessions of another paired machine are resumed on that machine (see [sync-and-hub.md](sync-and-hub.md#remote-sessions)).

### Continue in / fork

The agent chip in the session toolbar opens **Continue in...**: start a new session with any installed agent that picks up where this one left off. **Fork** (the branch icon) is the same with the same agent. Both work on live and ended sessions.

The new session gets a handoff pack appended to its injected memory: the source session's title, agent, date and folder, its summary, the project brief, its last 12 user/assistant turns (each up to 1 500 characters), files it touched and the open threads, capped at 12 000 characters (oldest turns are dropped first). The pack is also saved as `~/.blirp/launch/<new-session>/handoff.md`. Unless you give a prompt, the first prompt is "Continue the work described in the blirp handoff above.", or for agents without memory injection "Read the blirp handoff in <path to handoff.md> and continue the work described there." (no prompt for Shell). The new session records the source as its parent (`parent_session_id`).

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
