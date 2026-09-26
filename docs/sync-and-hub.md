# Sync and the self-hosted hub

Without a hub every machine works standalone. With a hub, your machines replicate their blirp data through it, so a session on your laptop starts with memory of work done on your desktop, and you can start and watch sessions on any paired machine.

- [What a hub is](#what-a-hub-is)
- [Set up a hub](#set-up-a-hub): [macOS / Mac mini](#macos-eg-a-mac-mini), [Linux](#linux), [Windows](#windows)
- [Pair machines](#pair-machines)
- [What syncs](#what-syncs) and [conflicts](#conflicts)
- [Network, relay and privacy](#network-relay-and-privacy), [Tailscale](#tailscale)
- [Remote sessions](#remote-sessions)
- [Revoking, leaving, disabling](#revoking-leaving-disabling)
- [Backups, moving and updating the hub](#backups-and-moving-the-hub)

## What a hub is

A hub is an ordinary blirp daemon with the `hub` role on a machine that is always on. It is:

- a replica and aggregator of **your** machines' blirp data (redacted transcripts, summaries, briefs, records, wiki pages, resources),
- a relay for remote terminals and remote session launches between your machines,
- optionally the host of the [web portal](portal.md) for phones and other browsers,
- a normal machine too: it can run agent sessions itself.

It is not a cloud service: no accounts, and data goes only between your paired machines. Roles are `standalone` (default), `hub` and `node` (a machine paired with a hub). There is one hub per set of machines.

## Set up a hub

Any machine that stays on and runs as your user works: a Mac mini, a small Linux box or VM, a Raspberry Pi 4/5 on a 64-bit OS, a desktop that never sleeps. The daemon idles at a few tens of MB of RAM; the database grows with your transcript history. Use the standalone `blirp` binary ([install.md](install.md)); the desktop app works too on a machine where you stay logged in.

Run the hub under your normal user account, never as root or a system service: summaries use your agent logins, and sessions started on the hub run as that user.

### macOS (e.g. a Mac mini)

1. Install the CLI:
   ```sh
   tar -xzf blirp-<version>-aarch64-apple-darwin.tar.gz
   sudo install -m 0755 blirp-<version>-aarch64-apple-darwin/blirp /usr/local/bin/blirp
   ```
2. Log in to the agent CLIs you want the hub to run or summarize with (`claude`, `codex`, ...) in the same user account.
3. Start the daemon now and at every login:
   ```sh
   blirp service install     # ~/Library/LaunchAgents/dev.blirp.daemon.plist
   blirp service status
   ```
   The LaunchAgent restarts the daemon after a crash (not after a deliberate stop). launchd output goes to `~/.blirp/logs/launchd.log`, the daemon's own log to `~/.blirp/logs/blirpd.<date>.log`.
4. Keep it running:
   - LaunchAgents run while the user is logged in. For a headless Mac enable **System Settings > Users & Groups > Automatically log in as** (unavailable with FileVault; then log in once after each reboot, e.g. over Screen Sharing).
   - Prevent sleep: **System Settings > Energy > Prevent automatic sleeping when the display is off**, or `sudo pmset -a sleep 0 disksleep 0`. While sessions run, the hub also holds a sleep assertion itself (`sessions.keep_awake`, on by default for the hub; see [cloud-sessions.md](cloud-sessions.md#keep-awake)).
   - Optional: **Start up automatically after a power failure**.
5. `blirp hub enable` (below).

### Linux

1. Install the CLI for your architecture:
   ```sh
   tar -xzf blirp-<version>-aarch64-unknown-linux-gnu.tar.gz
   install -m 0755 blirp-<version>-aarch64-unknown-linux-gnu/blirp ~/.local/bin/blirp
   ```
2. Start it with your user's systemd and keep it running without a login session:
   ```sh
   blirp service install              # ~/.config/systemd/user/blirp.service, enable --now
   sudo loginctl enable-linger "$USER"
   ```
   Without `enable-linger` the daemon stops when you log out of SSH and does not start at boot.
3. Check: `blirp service status`, `journalctl --user -u blirp -f`, `blirp doctor`.
4. `blirp hub enable`.

`service install` writes the absolute binary path, your current `PATH` (so the daemon finds agent CLIs) and `BLIRP_HOME` if set into the unit. Re-run it after moving the binary or installing agents elsewhere.

### Windows

`blirp service install` adds a per-user `Run` entry (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `blirp`), so the daemon starts when that user logs in. For an always-on Windows hub, configure automatic sign-in for that account and disable sleep in Power settings. Then `blirp hub enable`.

## Pair machines

On the hub:

```sh
blirp hub enable        # become the hub and print an invite
blirp hub invite        # another invite later
```

or **Settings > Machines & Sync > Make this the hub > Enable hub**, then **Create invite**. An invite consists of:

- the invite `blirp1-...` (the hub's address and an invite id),
- the pairing code `XXXX-XXXX` (8 characters, no 0/O/1/I),
- a QR code / link `blirp://join/<invite>#<code>`.

It is valid for 10 minutes, works once, and is invalidated after 5 wrong attempts or a hub restart. Create a new invite for each machine.

On each other machine (standalone role), either:

- **CLI:** `blirp pair <invite> <code>`. On the same LAN, `blirp pair <code>` alone finds the hub through local network discovery (mDNS), as long as exactly one hub is advertising and it has one open invite. `blirp pair 'blirp://join/...#CODE'` also works.
- **UI:** **Settings > Machines & Sync > Join a hub**, paste the invite and code (on the same LAN the invite can stay empty), **Pair with hub**. A confirmation shows the hub's machine id from the invite: compare it with the **Machine id** on the hub's own Settings > Machines & Sync. It lists what the hub will receive and may do, and has an unchecked **Allow this hub to start and control terminals on this machine** box (`sync.allow_hub_control`). With the desktop app installed, opening a `blirp://join/...` link (or scanning the hub's QR code with a device that has the app) opens this screen prefilled; since any web page can open such a link, it never pairs without this confirmation.

Pairing is a password-authenticated key exchange (SPAKE2) over a QUIC connection bound to both machines' keys, with key confirmation. The invite alone is useless without the code, a wrong code cannot be brute-forced within 5 attempts, and a man in the middle is detected. After pairing, the hub accepts sync only from known, non-revoked machine keys.

The joining machine switches to the `node` role (`[sync] role = "node"`, `hub = "<hub id>"` in its config) and starts uploading its existing history in the background. Offline machines queue changes locally and catch up when they reconnect (retry with backoff up to 60 s).

`blirp hub status` (any role) shows role, machine id, hub, whether it is connected, pending changes and, on a hub with the portal on, the portal URL and certificate fingerprint. The same is in **Settings > Machines & Sync**, with **Last sync** and **Pending changes**.

## What syncs

Replicated between all paired machines (every node ends up with a full copy):

- machines (name, OS, role, last seen, revoked),
- projects and their folders per machine (with the git remote, if any),
- sessions (metadata, status, summaries, tokens, cost) and all transcript events,
- records, briefs (with version history), wiki pages, resources.

Local to each machine, never synced: `config.toml`, the runtime token, `identity.key`, the TLS certificate, suggestions, local settings and ingest cursors, launch files, worktrees, terminal screen contents, and the device list (kept on the hub).

Everything replicated has been redacted before it was stored. Each machine distills and ingests only its own sessions; summaries of a session run on the laptop are produced on the laptop and arrive on the other machines by sync.

Projects are matched across machines by git remote: the same repo cloned on two machines is one project automatically. Non-git folders are separate projects per machine until you merge them ([projects-and-sessions.md](projects-and-sessions.md#managing-projects)).

## Conflicts

The hub keeps an ordered log of every change from every machine. Shared memory and projects (records, briefs, wiki pages, resources, project names) keep the newest edit by time on every machine, whichever order the edits arrive in: a machine that was offline or left the hub and pairs again cannot overwrite newer edits with its older copies, and a deleted record, page, resource or project stays deleted. An edit is always stamped after the version it replaces, so a machine whose clock is behind does not lose its edits. Sessions and folders belong to their machine; for those, a later change in the hub's order replaces an earlier one. Transcript events are append-only and keyed by session and position, so they never conflict. Briefs keep every version in their history, so a replaced brief can be restored with **Revert**.

## Network, relay and privacy

Machines talk over QUIC using [iroh](https://iroh.computer): each machine is identified by the Ed25519 key in `~/.blirp/identity.key`, connections are end-to-end encrypted and authenticated with those keys, and NAT traversal means the hub needs no public IP or port forwarding. Standalone machines open no network endpoint at all.

`[sync] relay` controls how peers find each other:

| Value | Behavior |
|---|---|
| `"default"` | n0's public relay servers are used for connection setup and as a fallback when no direct path exists, and addresses are published to and looked up from n0's DNS discovery, so peers can be found by id alone. Relays forward encrypted packets and cannot read them, but they see which endpoint ids connect and their IP addresses. |
| `"disabled"` | No relays and no DNS publishing: only direct addresses and local network discovery. Works on one LAN, or across networks where machines can reach each other directly (e.g. over Tailscale). |
| `"https://relay.example"` | Only your own [iroh relay](https://github.com/n0-computer/iroh). |

Local network discovery (mDNS, service `blirp`) is always on for hubs and nodes; hubs advertise themselves so `blirp pair <code>` can find them. Relay changes apply after restarting the daemon.

### Tailscale

Sync does not need Tailscale; iroh finds a path on its own and also works across tailnet addresses (useful with `relay = "disabled"`). For browser access from outside your LAN, `tailscale serve` in front of the hub is the recommended option; see [portal.md](portal.md#tailscale).

## Remote sessions

With sync, the new-session dialog has a **Run on** choice: this machine, **Cloud (<hub>)** and the other paired machines. A session started for another machine is created by that machine's daemon (its agents, its folders), and its terminal is attached through the hub, so you can drive a session on the hub from your laptop, and vice versa. The dialog lists that machine's agents (and whether Claude Code is logged in there), browses its folders and can `git clone` a project onto it. Stop and Resume of a session that runs elsewhere are forwarded to its machine. Running your work on the hub while your PC sleeps: [cloud-sessions.md](cloud-sessions.md).

- Every node keeps one connection to the hub, and the hub relays requests byte for byte to the target machine, so nodes behind NAT are reachable.
- A node accepts control from elsewhere only when it opted in with `sync.allow_hub_control = true` (default off; the box in the join dialog, **Allow the hub to control this machine** in Settings > Machines & Sync, or `config.toml`; leaving the hub resets it; changing it closes relayed terminals and streams so they reopen with the new rights): until then the hub and other machines can view its sessions, but their launches, stops, resumes, deletes, terminal input and memory edits on it are refused (`control_not_allowed`). The hub itself accepts control from paired machines unless you turn **Terminal control** off for a machine under **Devices** on the hub.
- Each machine owns its folders, sessions and transcripts: other machines can retitle a session, change its status, or move it and its folder to another project (merge), but never create, delete or change anything else of another machine's sessions (folder, agent id, transcript, summary, tokens), folders or transcript events. Deleting a project unregisters its folders on every machine, each machine dropping its own. Deleting a session of another machine is forwarded to that machine. A node also refuses anything its hub relays under the node's own name.
- Only the request itself is forwarded, never local credentials; relayed requests have no admin rights (they cannot pair, revoke or change devices on the target).
- If the target machine is offline, you get `machine_offline` / `machine_unreachable`.

## Revoking, leaving, disabling

- **Revoke a machine** (on the hub): **Settings > Machines & Sync > Machines > Revoke**, or `blirp devices revoke <device-id>` (ids from `blirp devices list`). Its live connections close at once and reconnects are refused. The machine row is marked revoked on every machine. Data it already synced stays.
- **Revoke a browser device** (on the hub): **Devices > Revoke**, or `blirp devices revoke <id>`; it is signed out immediately.
- **Leave the hub** (on a node): **Settings > Machines & Sync > Leave hub** (API: `DELETE /api/machines/<hub id>` on the node). The node returns to `standalone` and keeps its local copy of the data. Pairing again sends everything it has, including what it had not sent yet and what it deleted; newer edits made elsewhere meanwhile stay newest.
- **Stop being a hub:** **Disable hub** in the same place, or `blirp hub disable`. Sync and the portal stop and the role returns to `standalone`; paired machines stay known, so `blirp hub enable` later resumes without re-pairing. A node cannot become a hub while paired (`paired_node`); leave first.

## Backups and moving the hub

Everything is in `~/.blirp` on the hub:

- `blirp.db`: all data. Back it up while the daemon runs with `sqlite3 ~/.blirp/blirp.db ".backup '/backups/blirp.db'"`, or stop the daemon and copy the file (with `blirp.db-wal` if present).
- `identity.key`: the hub's identity (secret, mode 0600). Paired machines trust this key; losing it means pairing every machine again.
- `config.toml`.

To move the hub, stop the daemon, copy `~/.blirp` to the new machine, install blirp there and start it. Every node also holds a full copy of the replicated data, so a lost hub loses nothing that had already synced.

### Updating

Replace the binary and restart the daemon: `launchctl kickstart -k gui/$(id -u)/dev.blirp.daemon` (macOS) or `systemctl --user restart blirp` (Linux); desktop apps update themselves. The sync protocol is versioned (`blirp/sync/1`); keep hub and nodes on the same release and update the hub first.

## Security checklist

- Run the hub as a normal user; use full-disk encryption on it (it holds the redacted history of every paired machine).
- Keep the portal off unless you use it, and never expose the daemon port or the portal to the internet (`tailscale serve`, never `tailscale funnel`).
- Revoke lost or retired machines and browser devices.
- Consider turning off **Terminal control** for machines that should only read.

More: [security.md](security.md).
