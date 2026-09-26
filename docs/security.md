# Security

This page describes what blirp protects, against whom, and where its limits are. To report a vulnerability, see [SECURITY.md](../SECURITY.md).

## Threat model

blirp runs as your user account and starts coding agents that can already read and change your files and run commands. Anything that can act as your user on the machine (malware, another process of yours) can also read `~/.blirp` and use the daemon; that is out of scope. blirp aims to make sure that nothing *else* gets that power, and that data leaves your machines only where you chose.

| Actor | Trust | What protects you |
|---|---|---|
| Your user account on the machine | trusted | - |
| Other local user accounts and local programs you did not start | untrusted | data dir `0700` and `runtime.json`/`identity.key` `0600` (macOS/Linux); Windows profile ACL; daemon bound to `127.0.0.1` and token-authenticated; no cookie on the loopback listener (cookies ignore ports, so any other server on `127.0.0.1` a page of yours talks to would receive it); the daemon refuses to start when its port is taken instead of moving, so a squatter cannot keep the well-known address |
| Web pages open in your browser | untrusted | bearer token kept in the UI origin's storage (per port; handed over in a URL fragment, which is never sent to a server), single-use path-bound WebSocket tickets, same-origin check on mutations and WebSocket upgrades, CSP, `X-Frame-Options: DENY`; the loopback listener answers only requests addressed to `127.0.0.1`, `localhost` or `[::1]` with its port, so a DNS-rebinding page is refused |
| Devices on your LAN | untrusted | nothing listens on the LAN unless you run a hub with `portal.lan = true`; then TLS, one-time login links, hashed device tokens, per-device permissions, revocation |
| Paired machines | trusted with your data | they receive all synced (redacted) history and shared memory (records, briefs, wiki, resources); they write only their own folders, sessions and transcripts (checked by the hub and again by every node; a node also refuses entries relayed under its own name); they control a node only when it set `sync.allow_hub_control` (an unchecked box when joining, a toggle in Settings), and the hub unless you turn it off per machine |
| Portal browser devices | partly trusted | read all synced data, edit memory and settings; terminals only with **Terminal control** ([portal.md](portal.md#what-a-browser-device-may-do)) |
| Relay and discovery servers (sync) | untrusted | end-to-end encryption and key authentication; they see endpoint ids and IP addresses only; can be disabled |
| Summarizer provider | as trusted as your agent's provider | receives redacted transcript excerpts only if you use `claude`/`codex` summarizers (`codex` only when chosen explicitly); `ollama` or `none` keep everything local |
| Agents and their transcripts | as trusted as the agents | blirp never gives an agent more rights than it has; memory is injected as context, see [prompt injection](#memory-is-context-not-instructions) |

## What is stored where

| Data | Location | Protection |
|---|---|---|
| Projects, sessions, redacted transcripts, summaries, memory, devices, sync log | `~/.blirp/blirp.db` (SQLite, not encrypted) | data dir permissions; use full-disk encryption |
| Runtime token (full local API access) | `~/.blirp/runtime.json` | `0600`; regenerated every daemon start |
| Machine identity (sync) | `~/.blirp/identity.key` | `0600`; secret |
| Portal TLS key | `~/.blirp/tls/key.pem` | `0600` |
| Browser device tokens, portal login links | database / daemon memory | stored as SHA-256 hashes only |
| Launch files (memory, handoff, generated agent settings) | `~/.blirp/launch/<session>/` | contain rendered (redacted) memory, no credentials |
| Logs | `~/.blirp/logs/` | never contain transcript text or secrets |
| Files pasted or dropped into a terminal | `~/.blirp/uploads/<session>/` on the machine running the session | `0600` in `0700` folders (Windows: profile ACL); not redacted; not in the database, so never synced, and never ingested; deleted with the session and after 7 days; at most 25 MB each; saved only inside that folder under a sanitized name (the client's path is never used) |
| Claude login token (only if you store one: `blirp agents set-token claude`) | `~/.blirp/secrets/claude_oauth_token` | `0600` in a `0700` folder (Windows: profile ACL); not in the database, so never synced; never logged or returned by the API; passed only to claude processes as `CLAUDE_CODE_OAUTH_TOKEN` ([agents.md](agents.md#headless-login-for-a-hub)) |
| Other agent credentials | the agents' own config | never read or stored by blirp |

Nothing is stored outside `~/.blirp` except what you ask for: autostart entries (`blirp service install`), agent config entries (`blirp hooks install`, with `.blirp-backup` copies), and git worktrees and `blirp/*` branches for worktree sessions.

## Redaction

All transcript text, titles and event metadata pass through the redactor before they are written to the database, and so do memory written on the machine (records, briefs, wiki pages and resources from the UI, API, MCP `mem_record` or accepted suggestions) and the summarizer's reply, so nothing unredacted is ever synced, summarized, searched or injected. Matches become `[REDACTED:<kind>]`. The rule set follows gitleaks (cloud provider keys, GitHub/GitLab/Slack/Stripe/OpenAI/Anthropic/npm/PyPI/Hugging Face and other tokens, private key blocks, JWTs, credentials in connection strings, `.env` lines, secret-named keys, high-entropy values assigned to secret-looking names); the full list is in [memory.md](memory.md#redaction). Every rule has positive and negative tests.

### Redaction limits

Redaction is pattern based. It cannot recognize a secret with no recognizable shape (a plain password pasted into a prompt without a `password=` style key, a proprietary token format), and it does not remove personal data or source code. Transcripts are still your code and conversations; protect `~/.blirp` and your hub accordingly. If a secret slipped through, it is in the database of every paired machine; rotate the secret.

Terminal output (the live screen) is not stored in the database and not redacted; it is only kept in the daemon's memory while the session runs.

## Authentication

- **Local clients** (desktop app, CLI, hooks, `blirp open`) use the 256-bit random runtime token from `runtime.json` as `Authorization: Bearer`. Comparison is constant time. The desktop app and `blirp open` hand it to the web UI in the URL fragment (`/#token=`); the UI keeps it in `localStorage` for its own origin (scheme, host and port: other local servers cannot read it) and removes it from the address bar. WebSockets use single-use tickets (30 s, bound to one path) minted with the token. The loopback listener accepts no cookie and never takes the token from a query string.
- **Joining a hub** always goes through a confirmation that shows the hub's machine id from the invite (compare it with the hub's Settings > Machines & Sync), lists what the hub receives and may do, and has an unchecked box for terminal control. `blirp://join` links, which any web page can open, only prefill this form.
- **Portal browser devices** use a device cookie (`HttpOnly; Secure; SameSite=Strict`, 400 days) obtained by redeeming a one-time link (5 minutes, single use, 128-bit, stored hashed; 10 attempts per minute per client IP). The portal does not accept the runtime token.
- **Machines** authenticate with their Ed25519 keys (QUIC TLS). Pairing uses an 8-character one-time code (40 bits, 10 minutes, single use, 5 attempts) in a SPAKE2 exchange with key confirmation bound to both machine keys and the invite, so neither a stolen invite nor a man in the middle can pair. After pairing only known, non-revoked machine keys are accepted. A machine that leaves the hub asks it to revoke that machine key; the hub revokes only the key of the connection the request arrived on. If the hub could not be reached, the node says so and the machine must be revoked on the hub by hand.

## Authorization

Requests run with `control` and `admin` rights ([api.md](api.md#listeners-and-authentication)). Only this machine's own local clients have `admin`: pairing, hub and device management, global hooks install, claude's login token, opening folders or editors on the machine, and stopping the daemon. `control` (launch/stop/resume sessions, distill, terminal input) is granted to local clients, to browser devices with **Terminal control**, to paired machines on the hub unless turned off there, and to the hub and other machines on a node only when the node set `sync.allow_hub_control = true`. Replicated rows are checked for ownership: a machine's folders, sessions and transcript events are written only by that machine (another machine may retitle a session, change its status or move it to another project, nothing else), no machine removes another's folders, and ids are checked before anything is stored or used as a file name. Shared memory keeps the newest version, so a machine re-pairing with an old copy cannot roll back or resurrect it. Shared memory (records, briefs, wiki pages, resources, project names) can be edited by every paired machine by design; it is injected into agents, so a compromised machine can plant [prompt injection](#memory-is-context-not-instructions) there. The MCP endpoint and daemon shutdown exist only on the loopback listener.

### Limits

- The token in `localStorage` is readable by script running in the UI's origin; the CSP allows only the UI's own scripts, and the UI renders memory and transcripts as sanitized text.
- The portal's device cookie is a cookie: any other HTTPS server on the hub's address (another port on the same host) that a signed-in browser visits receives it. Run nothing else on the hub's LAN address that you do not trust, and revoke devices you no longer use.
- The hub's name cannot be shown before pairing: pairing reveals machine names only after the code is proven. Verify the machine id instead.

## Network exposure

| Listener / connection | When | Details |
|---|---|---|
| TCP `127.0.0.1:47770` (or the port in `runtime.json`) | always | local API, UI, `/mcp`; loopback only |
| TCP `0.0.0.0:47771` HTTPS | hub with `portal.lan = true` | LAN portal, device cookies only, no `/mcp`, no shutdown |
| UDP (QUIC, iroh) and mDNS | hub or node | sync, pairing, remote terminals; standalone machines open no endpoint |
| Outbound to n0 relays and DNS discovery | hub or node with `sync.relay = "default"` | connection setup and fallback relaying of encrypted traffic |
| Outbound via your `claude`/`codex` CLI, or to Ollama | when distilling | redacted transcript excerpts |
| Outbound to GitHub Releases | daemon (at most daily, unless `update.check = false`), `blirp update`, install scripts | release check; `blirp update` installs only a release whose `SHA256SUMS.txt` has a valid minisign signature, the install scripts check it when they can ([install.md](install.md#how-the-scripts-verify-downloads)) |

Never expose the daemon port or the portal to the internet. For remote access use Tailscale ([portal.md](portal.md#tailscale)).

## Global hooks

`blirp hooks install` is the only operation that edits files blirp does not own. It is explicit, idempotent, marker-based, backs up the original once, writes atomically, refuses to rewrite files with comments or invalid syntax, and `uninstall` removes exactly its own entries ([memory.md](memory.md#global-hooks)). Hook processes always exit 0 within 2 seconds and send only the agent's hook payload to the local daemon.

## Summarizer runs

The `claude` summarizer runs with every tool, hooks, plugins, MCP servers and session persistence disabled (`--tools ""`, `--safe-mode`, `--no-session-persistence`), in an empty scratch folder, with a 180 s timeout that kills the process tree: a transcript cannot make it run commands or read files. `codex` has no single switch for "no tools": blirp does not load your Codex `config.toml` (so none of your MCP servers, plugins or hooks), turns off web search, its shell and exec tools, apps, browser, computer use, subagents and image tools, and keeps its read-only sandbox; with codex 0.153.2 the model is then offered no tool at all. A newer Codex can add a built-in tool that these switches do not cover, which a crafted transcript could steer it to, so `auto` never picks `codex`; it runs only when you set `summarizer = "codex"`. Summarizer replies are redacted before they are stored.

## Memory is context, not instructions

Injected memory is derived from earlier transcripts, which can contain text from untrusted sources (web pages, issue text, tool output). The distiller turns that into short summaries and records, and new sessions receive them as context. Treat memory like any other context an agent reads: review the brief and records (everything is visible and editable in the Memory panel), use `brief_mode = "review"` if you want to approve brief changes, and keep your agents' own permission prompts on.

## Desktop app

The window loads only its bundled loading page and the local daemon UI. The daemon origin gets no Tauri IPC capabilities; only the bundled page may call its three commands (startup state, retry, open logs). Navigation to other origins and `window.open` go to your default browser. The app has no updater of its own: `blirp update` replaces it, after verifying the release signature with the key built into `blirp`.

## Hardening checklist

- Full-disk encryption on every machine, especially the hub.
- Keep the portal off unless you use it; give **Terminal control** only to devices that need it; revoke unused devices and machines.
- Use `summarizer = "ollama"` or `"none"` if transcripts must not reach a model provider, and `sync.relay = "disabled"` (or your own relay) if you do not want public relay/discovery servers involved.
- Keep blirp updated; only the latest release gets security fixes.
