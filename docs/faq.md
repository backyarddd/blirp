# FAQ

**Is blirp free? Is there a paid tier or a cloud?**
Free and open source (Apache-2.0). There is no blirp cloud, account or telemetry. The only server you may run is your own hub.

**Does blirp see my API keys or proxy my model traffic?**
No. Agents run as their own CLIs with their own logins; blirp only adds flags, environment variables and generated files under `~/.blirp/launch/`. It never reads agent credentials.

**Does my code or transcript leave my machine?**
Only in two cases you control: the summarizer (`claude` or `codex` CLI) sends redacted transcript excerpts to that provider, and sync sends redacted data to your own paired machines. Use `memory.summarizer = "ollama"` or `"none"` to keep everything local. See [security.md](security.md).

**What does distilling cost?**
With the `claude` summarizer each run is one Haiku request of up to roughly 15 000 input tokens, charged to your Claude plan's usage or your API key. `codex` uses Codex's default model. By default at most 40 runs per day. Ollama is free. See [memory.md](memory.md#summarizer-backends).

**Do I need git?**
No. Any folder is a project. Git only adds branch display, the Git tab and optional per-session worktrees.

**Can I keep using agents in my own terminal?**
Yes. Those sessions are ingested and summarized automatically. Run `blirp hooks install` once if they should also *receive* memory at startup.

**Will blirp change my agent configuration?**
Not when launching sessions. Only `blirp hooks install` edits agent user configs, reversibly, with a backup; `blirp hooks uninstall` removes exactly its entries.

**How is this different from a CLAUDE.md / AGENTS.md handoff file?**
You do not write or maintain anything, it works across different agents, it covers sessions started anywhere, older history stays searchable instead of being cut, and the injected block is bounded in size. You can still keep your own instruction files; blirp does not touch them.

**What happens to running sessions when I close the window?**
They keep running in the daemon. Reopen from the tray. **Quit blirp** in the tray (or stopping the daemon) ends them; they show as Detached and can be resumed.

**Can I use blirp without the desktop app?**
Yes: `blirp daemon --detach` and `blirp open` give the same UI in your browser. That is also how a headless hub runs.

**Can two machines work on the same project?**
Yes, with a hub. Git projects are matched by remote automatically; for non-git folders merge the two projects once. Each machine's sessions feed the shared memory.

**Can I reach my sessions from my phone?**
Yes, through the hub's LAN portal, or over Tailscale. Browser devices are read-only for terminals until you enable **Terminal control** for them. See [portal.md](portal.md).

**Can I drive a session on another machine?**
Yes. With sync, pick the machine in the new-session dialog; its terminal is relayed through the hub.

**How big does the database get?**
It grows with transcript history (redacted text, tool results cut to 4 KiB). History is never pruned automatically.

**How do I delete something from memory?**
Edit, resolve or delete records and edit or revert the brief in the Memory tab. Transcripts and sessions cannot be deleted from the UI yet.

**How do I uninstall completely?**
`blirp hooks uninstall`, `blirp service uninstall`, quit blirp, remove the app or binary, then delete `~/.blirp` if you want your data gone. See [install.md](install.md#uninstalling).

**Which agent should I use for summaries?**
Whichever you have: `auto` picks `claude`, then Ollama. `codex` works too but only when you choose it (see [memory.md](memory.md#summarizer-backends) for why). Summaries are short and structured, so a small model is enough.

**An agent I use is not supported.**
Add it as a [custom agent](agents.md#custom-agents) today (tracked sessions, memory file in the environment), or add an adapter ([development.md](development.md#adding-an-agent-adapter)).
