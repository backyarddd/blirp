---
name: blirp-sessions
description: Inspect agent sessions and git worktrees managed by blirp (list recent sessions and their status, read a session's summary and transcript, find which session you are in, clean up worktrees of ended sessions) and hand the user off to the blirp UI to start, resume, fork or continue sessions. Use when the user asks about running or past blirp sessions or blirp worktrees.
---

# blirp sessions

blirp runs each agent session in a terminal it supervises. Inside a blirp session your environment has `BLIRP_SESSION_ID`, `BLIRP_PROJECT_ID` and `BLIRP_MEMORY_FILE` (the memory injected at start).

## List and read (read-only)

```sh
blirp sessions --limit 20
blirp sessions --project <PROJECT_ID> --limit 50
blirp mem recent --limit 10
blirp mem show --limit 200 <SESSION_ID>
blirp worktrees list
```

- `blirp sessions` (needs the daemon): id, status, agent, title or folder, newest first. Live statuses: `starting`, `working`, `idle`, `waiting` (waiting = the agent needs the user). Ended: `completed`, `failed`, `detached` (the daemon stopped while it ran; resumable).
- `blirp mem recent` and `blirp mem show` (no daemon needed): summaries and transcripts of the current folder's project; add `--json` for machine-readable output.
- `blirp worktrees list`: git worktrees blirp created for sessions, with their session and number of uncommitted changes.

## Change (ask the user first)

```sh
blirp worktrees prune
```

Removes the worktrees of ended sessions that have no uncommitted changes (branches are kept; needs the daemon). Show the user `blirp worktrees list` first.

## Start, resume, fork, continue

There is no CLI for these. Tell the user to open the UI with `blirp open` (it opens their browser), or the desktop app, then:

- **New session**: pick project, agent and, with linked machines, where it runs.
- **Resume** on an ended session.
- **Fork** or **Continue in** another agent: a new session that starts with a handoff of this one (summary, recent turns, files touched, open threads).

Never type into or stop another session's terminal. Stopping the daemon (`blirp stop`, `blirp update`) ends all live sessions, including yours.
