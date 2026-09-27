# blirp documentation

## Using blirp

| Guide | What it covers |
|---|---|
| [Getting started](getting-started.md) | first install, first session, what memory looks like, sessions started elsewhere |
| [Install](install.md) | downloads per OS, checksums, autostart, upgrading, uninstalling |
| [Projects and sessions](projects-and-sessions.md) | folders vs git, worktrees, statuses, stop and resume, continue in / fork, subagents, external sessions |
| [Memory](memory.md) | capture, redaction, distilling and summarizer costs, brief/records/wiki/suggestions, injection per agent, MCP tools, `blirp mem`, global hooks, turning things off |
| [Agents](agents.md) | what blirp does for each supported agent, custom agents |
| [Agent skills](skills.md) | `blirp skills install`: teach your agents to check, link, update and search blirp |
| [Cloud sessions](cloud-sessions.md) | run sessions on your hub (an always-on machine) from your PC, keep them through sleep and restarts, keep-awake, clone and pick folders on the hub |
| [Hub on a VPS](vps.md) | no always-on machine at home: run the hub on a rented Linux server with one command, pairing, headless agent logins, firewall, backups, updates |
| [Sync and hub](sync-and-hub.md) | self-hosting a hub (macOS, Linux, Windows), pairing, what syncs, conflicts, relays and privacy, remote sessions, revocation, backups |
| [Web portal](portal.md) | LAN HTTPS portal, phone login via QR, certificate fingerprint, browser device permissions, Tailscale |

## Reference

| Page | What it covers |
|---|---|
| [Configuration](configuration.md) | every `config.toml` key, environment variables, file locations per OS |
| [CLI](cli.md) | every `blirp` command and flag |
| [HTTP API](api.md) | routes, auth, request/response shapes, terminal and event WebSockets, MCP endpoint |
| [Security](security.md) | threat model, stored data, redaction, auth, network exposure |
| [Troubleshooting](troubleshooting.md) | daemon, ports, ConPTY, agent detection, hooks, memory, sync, portal, logs |
| [FAQ](faq.md) | short answers |

## Developing blirp

| Page | What it covers |
|---|---|
| [Development](development.md) | repo layout, build and test, running daemon and UI from source, e2e, adding an agent adapter, release process |
| [Architecture](ARCHITECTURE.md) | the design contract: data model, protocols, per-agent integration details |
| [Agent transcript formats](agent-formats.md) | survey of the agents' on-disk transcript formats that the ingest adapters are based on |
| [Contributing](../CONTRIBUTING.md), [Security policy](../SECURITY.md) | rules for changes, reporting vulnerabilities |
