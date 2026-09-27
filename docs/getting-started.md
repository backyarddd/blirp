# Getting started

This walks through a first install, a first session and what memory looks like in the second session. It takes about five minutes.

## 1. Install

Install the desktop app or the standalone binary for your OS as described in [install.md](install.md). The desktop app is the easiest start: it bundles `blirp`, starts the daemon and opens the UI.

With only the standalone binary:

```sh
blirp daemon --detach     # start the daemon in the background
blirp open                # open the UI in your default browser, already logged in
```

Check the installation at any time:

```sh
blirp doctor
```

It reports the data directory, `config.toml`, the database, whether the daemon is reachable, whether `git` is installed, which agents it found on `PATH`, and one line per transcript adapter (store found, number of sources, last ingest). See [cli.md](cli.md#blirp-doctor).

## 2. Start a session

1. Click the orange **+** in the top bar (Ctrl+T on Windows/Linux, Cmd+T on macOS; inside a focused terminal on Windows/Linux use Ctrl+Shift+T, because Ctrl+T belongs to the shell).
2. Choose **Project** (one blirp already knows) or **Folder path** (any absolute path; it becomes a project automatically). Git is optional.
3. Pick an agent. Only agents found on the daemon's `PATH` can be selected; `Shell` always works. Custom commands from `config.toml` appear here too ([agents.md](agents.md#custom-agents)).
4. Optional: type a first prompt. blirp types it into the agent once its output settles (and waits if the agent is showing a "trust this folder" dialog, so your answer to that dialog is not overwritten).
5. For git projects you can tick **Run in a new git worktree**; the session then works on its own branch `blirp/<name>` in `~/.blirp/worktrees/`.
6. With a hub, pick the machine to run on.

In a terminal pane, copy with Ctrl+Shift+C (Cmd+C on macOS) and paste with Ctrl+Shift+V (Cmd+V on macOS; on Windows plain Ctrl+V too, as in Windows Terminal). On Linux and macOS plain Ctrl+V goes to the program in the terminal, as in native terminals. Pasting a screenshot or a copied file, or dropping files onto the pane, puts their paths into the terminal so the agent can attach them ([projects-and-sessions.md](projects-and-sessions.md#pasting-images-and-files)). The key choice follows the computer your browser or desktop app runs on, not the session's machine.

The session opens as a tab with the live terminal. The sidebar lists sessions grouped by project, each with a status chip: Working, Idle, Waiting (the agent needs you), Completed, Failed, Detached.

### A project without a folder

Not every project lives in a folder. Say you work on a design in a desktop app that your agents reach through the app's MCP server, and the design itself is stored by that app:

1. On **Projects**, click **New project**, name it (for example `Landing page redesign`), optionally write a brief ("Marketing site redesign in the design tool, file 'Landing v2'"), and leave **Folder** empty.
2. Start a session in it. It runs in a private, empty folder blirp keeps for the project on this machine (`~/.blirp/workspaces/<project id>`), shown on the project page as **blirp workspace**.
3. MCP servers you configured for your user work there as anywhere. For a server only this project should use, put the agent's project MCP file into the workspace, for Claude Code a `.mcp.json`.

Memory works as for any project, and syncs to your other machines; each machine uses its own workspace. Details: [projects-and-sessions.md](projects-and-sessions.md#projects-without-a-folder).

Sessions that are part of no project at all, such as a quick question asked in your home folder, are chats: they are listed under **Chats**, not as projects ([more](projects-and-sessions.md#chats)).

## 3. Let memory build up

Work as usual. When the session has been idle for 5 minutes (`memory.distill_idle_secs`) or ends, blirp distills it:

- the session gets a title and a 3-6 sentence summary,
- decisions, open threads and gotchas become **records** of the project,
- the **project brief** (what the project is, how to build and run it, current priorities) is updated as a new version.

The summarizer is picked automatically: your default agent for new sessions if that is Claude Code (runs Sonnet) or Codex and it is signed in, else your `claude` CLI, else a local Ollama server, else nothing. **Settings > Memory** shows which one it uses and why. Change it in **Settings > Memory** or `config.toml` ([memory.md](memory.md#summarizer-backends)). Want to see the result now? Open the session's detail page and use **Distill now**.

## 4. Start the next session

Start another session in the same folder, with the same or a different agent. It begins with a block like:

```markdown
# blirp memory: acme-api
<the project brief>
## Open threads
- **Rate limiter resets on deploy**: the counters live in process memory ...
## Recent decisions
- **Keep request validation in the route handlers**: ...
## Gotchas
- **The test database is shared across workers**: ...
## Recent sessions
- 2026-09-24 · claude · workstation · Persist rate limiter counters: ...
Tools: search older history with the blirp MCP tools (mem_search, mem_session, mem_recent, mem_record) or `blirp mem search "<query>"`.
```

How it reaches the agent depends on the agent (a startup hook, instructions, a flag; see [agents.md](agents.md)). Open **Memory** in the session toolbar to see exactly what this session received, and to edit the brief and open threads. The project page's **Memory** tab has everything: brief versions with revert, records, suggestions.

## 5. Sessions you start elsewhere

Sessions started in your own terminal or IDE terminal are picked up from the agents' transcript files within seconds and appear with origin "external". They are summarized like any other session, so their work shows up in the next session's memory. To also give *those* sessions memory at startup, install the global hooks once:

```sh
blirp hooks install          # Claude Code, Codex, Gemini CLI, Cursor (hooks + MCP); opencode (MCP)
blirp hooks status
blirp hooks uninstall        # removes exactly what install added
```

This is the only thing in blirp that edits your agents' config files. What it changes: [memory.md](memory.md#global-hooks).

## 6. Keep it running

- Closing the desktop window keeps the daemon and every session running; the tray icon reopens the window. **Quit blirp** in the tray menu stops the daemon and ends all sessions.
- `blirp service install` starts the daemon when you log in (macOS LaunchAgent, systemd user unit, Windows Run key). See [install.md](install.md#start-at-login).
- Hover the blirp logo (or run `blirp --version`) to see which version runs. When a new release is out, the Settings button gets a dot and a banner offers **Update now**, or run `blirp update`. See [install.md](install.md#updating).

## Next

- [Projects and sessions](projects-and-sessions.md): folders vs git, worktrees, resume, continue in / fork, subagents.
- [Memory](memory.md): everything about capture, summarizing and injection, and how to turn parts off.
- [Sync and hub](sync-and-hub.md): pair machines, reach the UI from a phone.
