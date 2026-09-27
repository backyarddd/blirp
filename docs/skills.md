# Agent skills

blirp ships [Agent Skills](https://agentskills.io/specification): small `SKILL.md` instruction files that coding agents load when a task matches. They teach an agent to operate blirp itself with the `blirp` CLI: check its health, link machines, update it, recall memory and inspect sessions, and to ask you before anything that changes your setup.

```sh
blirp skills install     # for every supported agent found on PATH
blirp skills list        # what is installed where
blirp skills refresh     # update unedited blirp skills to this version
blirp skills uninstall   # remove them again
```

Installing is opt-in and separate from `blirp hooks install`. `blirp doctor` shows the state, `blirp uninstall` removes them. The skill texts are built into the `blirp` binary; the sources are in [`skills/`](../skills) in the repository.

## The skills

| Skill | The agent uses it to |
|---|---|
| `blirp-status` | check the daemon, `blirp doctor`, logs, hooks and skills; fix common problems |
| `blirp-link-machines` | make a hub, create invites, pair machines, list and revoke devices, disable the hub |
| `blirp-update` | check for updates and update, after warning that running sessions end |
| `blirp-memory` | search the project's memory and past sessions (MCP tools, else `blirp mem`), record decisions |
| `blirp-sessions` | list sessions and worktrees, read transcripts, point you to the UI to start, resume or fork |

Every skill tells the agent to confirm with you before an action that changes or removes something: `blirp hub enable`, `blirp hub invite`, `blirp pair` (only when you asked to link machines), `blirp update`, `blirp stop`, `blirp devices revoke`, `blirp hub disable`, `blirp worktrees prune`, `blirp hooks install|uninstall`, `blirp service install|uninstall`, `blirp skills install --force`. Some actions stay yours alone: storing a Claude login token, leaving a hub (**Settings > Machines & Sync > Leave hub**), allowing the hub to control a machine. A test parses every `blirp` command in the skills with the CLI's own argument parser, so the skills cannot name a command or flag that does not exist.

## Where they go

Two folders cover the agents that support `SKILL.md` skills:

| Folder | Loaded by |
|---|---|
| `~/.claude/skills/<name>/SKILL.md` (`$CLAUDE_CONFIG_DIR/skills`) | Claude Code ([docs](https://code.claude.com/docs/en/skills)); Amp ([docs](https://ampcode.com/news/agent-skills)), opencode and Cursor read it too |
| `~/.agents/skills/<name>/SKILL.md` | Codex ([docs](https://developers.openai.com/codex/skills)), Gemini CLI ([docs](https://geminicli.com/docs/cli/skills/)), opencode ([docs](https://opencode.ai/docs/skills/)), Cursor ([docs](https://cursor.com/docs/skills)), pi ([docs](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/docs/skills.md)) |

`--agent claude` writes the first (`$CLAUDE_CONFIG_DIR/skills` when set), `--agent amp` always `~/.claude/skills` (where Amp looks), `--agent codex`, `gemini`, `opencode`, `cursor` or `pi` the second. Without `--agent`, `install` picks the folders of the supported agents found on `PATH`; `list` and `uninstall` handle both. With `--project <dir>` (an existing folder) the folders are `<dir>/.claude/skills` and `<dir>/.agents/skills` instead, for a repository you want to commit them to.

opencode and Cursor read both folders; with both installed they see each skill twice, with identical text.

Aider and DeepSeek Harness have no documented skill folder; they can still run `blirp` commands like any shell command, and the memory blirp injects at launch names `blirp mem search`.

## Guarantees

- Each skill folder blirp writes holds `SKILL.md` and `.blirp-skill`, the SHA-256 of the `SKILL.md` it wrote. That file is how blirp recognizes its own skills.
- `install` is idempotent. It writes missing skills and refreshes skills an older blirp version wrote (`outdated`), replacing the file atomically. A `SKILL.md` identical to this version's counts as blirp's even if an interrupted install left its marker stale (the marker is rewritten). `refresh` does only the refreshing, in your home folders.
- A skill you edited (`modified`) or a skill of the same name blirp did not write (`not_ours`) is not overwritten by default. `install` reports it as `skipped` and exits 1; `install --force` replaces it after copying the old file to `SKILL.md.blirp-backup` (`.1`, `.2`, ... when that exists with other content), so no edit is lost.
- `uninstall` removes only folders with a `.blirp-skill` whose hash still matches, including skills an older version installed and this one no longer ships. Edited skills stay (`kept_modified`), as do symlinked folders and all other skills and files; a folder is removed only when nothing else is left in it.
- `blirp uninstall` runs `blirp skills uninstall` for your home folders. Skills installed with `--project` stay in that project.
- After a successful `blirp update`, the new binary refreshes the `outdated` skills in your home folders and prints each one it refreshed; edited and foreign skills are left alone, and a failed refresh does not fail the update. Skills installed with `--project` are not refreshed (run `blirp skills install --project <dir>` there), and skills the new version adds need `blirp skills install`.

## No admin tools over MCP

The blirp MCP server stays limited to memory. Status, sync and update information is not added as MCP tools: every agent that uses the skills can run the read-only `blirp status`, `blirp doctor` and `blirp hub status` commands directly, and every change stays a CLI command the agent runs only after you agree.
