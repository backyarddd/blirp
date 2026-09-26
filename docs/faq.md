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
Yes: install with `--no-app` and run `blirp` (or `blirp daemon --detach` and `blirp open`) for the same UI in your browser. That is also how a headless hub runs.

**Why is blirp not code signed?**
Signing costs a yearly fee on both platforms (an Apple Developer ID, a Windows code-signing certificate or Artifact Signing subscription) for a free project, so blirp ships unsigned and is installed from the command line instead. Operating systems only ask about unsigned programs that carry a "downloaded from the internet" mark, which browsers add and `curl` / `irm` do not, so `install.sh` and `install.ps1` install without prompts. What protects you instead: downloads come over HTTPS from GitHub Releases, every file is checked against the release's `SHA256SUMS.txt`, and `blirp update` additionally verifies that file's minisign signature with the release key built into blirp (`packaging/minisign.pub`), so a tampered or swapped download is refused. You can check the same yourself ([install.md](install.md#manual-download)). If you download with a browser, see [troubleshooting.md](troubleshooting.md#blirp-is-damaged-or-windows-protected-your-pc).

**How do I update?**
`blirp update` (`--check` only reports). **Settings > About** tells you when a new release is out. See [install.md](install.md#updating).

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
`blirp uninstall --purge`: stops the daemon, removes autostart, blirp's agent hooks, the app and the CLI, and deletes `~/.blirp` (it asks first). Without `--purge` your data stays. See [install.md](install.md#uninstalling).

**Which agent should I use for summaries?**
Whichever you have: `auto` picks `claude`, then `codex`, then Ollama. Summaries are short and structured, so a small model is enough.

**An agent I use is not supported.**
Add it as a [custom agent](agents.md#custom-agents) today (tracked sessions, memory file in the environment), or add an adapter ([development.md](development.md#adding-an-agent-adapter)).
