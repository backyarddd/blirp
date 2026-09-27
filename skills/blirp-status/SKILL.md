---
name: blirp-status
description: Check and troubleshoot the local blirp installation (daemon running, version, data folder, detected agents, Claude login, transcript ingest, global hooks, installed skills, daemon log). Use when the user asks whether blirp works or why blirp sessions, memory, hooks or sync misbehave.
---

# blirp status and troubleshooting

blirp is a local daemon plus the `blirp` CLI. It runs coding agents in terminals, ingests their transcripts and builds project memory. The commands in "Check" only read.

## Check

```sh
blirp status
blirp doctor
blirp logs -n 100
blirp hooks status
blirp skills list
```

- `blirp status`: version, pid, local URL, machine name and id, sync role (`standalone`, `hub`, `node`), data folder. Exit code 1 means the daemon is not running.
- `blirp doctor`: one line per check, `[ ok ]` or `[FAIL]`, then `[info]` lines (detected agents with versions, how claude logs in, transcript ingest per agent, skills). Exit code 1 if a check failed. The daemon's PATH decides which agents it finds, so a result can differ from your shell.
- `blirp logs -n 100`: the last 100 lines of the daemon log (`-n` sets the count). It never contains transcript text. Do not use `--follow`: it never exits.
- `blirp hooks status`: per agent, whether the global hooks and the blirp MCP server are in its user config (`installed`, `not_installed`, `unsupported`).
- `blirp skills list`: which blirp skills are installed where, and whether they are `outdated` or `modified`.

## Fix

| Finding | Action |
|---|---|
| daemon not running | `blirp start` (safe; returns once the daemon answers) |
| `[FAIL] config` | show the user the key named in the message (`~/.blirp/config.toml`); edit only with their consent |
| `[FAIL] database` | stop and tell the user; never delete, move or copy `blirp.db` yourself |
| agent missing | the daemon's PATH lacks it; tell the user. After they fix PATH the daemon needs a restart (`blirp stop`, then `blirp start`; ends running sessions, so only with their consent). On macOS and Linux with autostart, `blirp service install` (with their consent) first records the new PATH |
| claude not logged in for the daemon | the user runs `claude setup-token`, then `blirp agents set-token claude` and pastes the token; never handle, print or store the token yourself |
| hooks `not_installed` for an agent the user runs outside blirp | offer `blirp hooks install --agent <AGENT>`; it edits that agent's user config (reversible) |
| skills `outdated` | `blirp skills refresh` (rewrites only unedited blirp skills) |

## Rules

- Ask before `blirp stop`, `blirp service install`, `blirp service uninstall`, `blirp hooks install` and `blirp hooks uninstall`: they change the user's machine setup. `blirp stop` ends every running session, possibly including the one you run in (`BLIRP_SESSION_ID` is set inside blirp sessions).
- Never pass `--force` to `blirp skills install` without the user's consent: it replaces skills they edited.
- Never print `~/.blirp/runtime.json`, `~/.blirp/identity.key` or anything under `~/.blirp/secrets/`.
- Linking machines: skill `blirp-link-machines`. Updating: skill `blirp-update`.
