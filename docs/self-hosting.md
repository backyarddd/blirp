# Self-hosting a blirp hub

A hub is an ordinary blirp daemon with the `hub` role, running on a machine that is always on. Your other machines pair with it once; after that their sessions, briefs and records replicate through it, so a session on your laptop starts with memory of work done on your desktop. Without a hub every machine works standalone.

What the hub is and is not:

- It is a replica and aggregator of **your** machines' blirp data (redacted transcripts, summaries, briefs, records, wiki pages, resources). It runs agent sessions like any other machine if you want it to.
- It is not a cloud service: there are no accounts, and nothing is sent anywhere except between your paired machines.
- Machines talk over QUIC (iroh) with NAT traversal, so the hub does not need a public IP or port forwarding. A public relay is used only when a direct path cannot be found; set `[sync] relay = "disabled"` or your own relay URL to avoid it.
- Pairing uses a one-time 8-character code (valid 10 minutes, single use) and SPAKE2, so the invite alone is not enough to join. After pairing, only known, non-revoked machines are accepted. Revoke a machine in **Settings > Machines & sync**.

## Pick a machine

Anything that stays on and runs as your user: a Mac mini, a small Linux box or VM, a desktop that never sleeps. Requirements are modest (the daemon idles at a few tens of MB of RAM); disk grows with your transcript history, typically tens to hundreds of MB per year of heavy use.

Use the **standalone `blirp` binary** on a hub (see [install.md](install.md)); the desktop app works too if the machine has a desktop session you stay logged in to.

## macOS (e.g. a Mac mini)

1. Install the CLI:
   ```sh
   tar -xzf blirp-<version>-aarch64-apple-darwin.tar.gz
   sudo install -m 0755 blirp-<version>-aarch64-apple-darwin/blirp /usr/local/bin/blirp
   ```
2. Log in to the agent CLIs you want the hub to run or summarize with (`claude`, `codex`, ...), in the same user account.
3. Start the daemon at login and now:
   ```sh
   blirp service install     # ~/Library/LaunchAgents/dev.blirp.daemon.plist
   blirp service status
   ```
   The LaunchAgent restarts the daemon after a crash (not after a deliberate stop) and logs launchd-level output to `~/.blirp/logs/launchd.log`; the daemon's own logs are `~/.blirp/logs/blirpd.*.log`.
4. Keep it running:
   - LaunchAgents run while the user is logged in. For a headless Mac enable **System Settings > Users & Groups > Automatically log in as** (not available with FileVault on; then log in once after each reboot, e.g. via Screen Sharing).
   - Prevent sleep: **System Settings > Energy > Prevent automatic sleeping when the display is off**, or `sudo pmset -a sleep 0 disksleep 0`.
   - Optional: **Start up automatically after a power failure**.

## Linux (server, VM, Raspberry Pi 4/5 on 64-bit OS)

1. Install the CLI for your architecture:
   ```sh
   tar -xzf blirp-<version>-aarch64-unknown-linux-gnu.tar.gz
   install -m 0755 blirp-<version>-aarch64-unknown-linux-gnu/blirp ~/.local/bin/blirp
   ```
2. Start the daemon with your user's systemd:
   ```sh
   blirp service install     # ~/.config/systemd/user/blirp.service, enable --now
   sudo loginctl enable-linger "$USER"
   ```
   `enable-linger` is what makes a user service start at boot and keep running without an open login session; without it the daemon stops when you log out of SSH.
3. Check it: `blirp service status`, `journalctl --user -u blirp -f`, `blirp doctor`.

`service install` bakes the absolute binary path and your current `PATH` into the unit so the daemon finds agent CLIs. Re-run it after moving the binary or installing agents elsewhere.

## Windows

`blirp service install` adds a per-user `Run` entry, so the daemon starts when that user logs in. For an always-on Windows hub, configure automatic sign-in for that account and disable sleep in Power settings.

## Enable the hub and pair machines

On the hub:

```sh
blirp hub enable
```

or **Settings > Machines & sync > Make this machine the hub** in the UI. It shows an invite (`blirp1-...`), a pairing code (`XXXX-XXXX`) and a QR code for `blirp://join/...`.

On every other machine, either:

- **UI:** Settings > Machines & sync > Join a hub, paste the invite and the code. With the desktop app, opening the `blirp://join/...` link (or scanning the QR code on a machine that has the app) opens this screen prefilled.
- **CLI:** `blirp pair <invite> <code>`. On the same LAN the hub is discoverable, so the code alone is enough.

Each code works once and expires after 10 minutes; create a new invite for the next machine. Existing history on a newly paired machine is uploaded in the background; offline machines queue changes and catch up when they reconnect.

## Browser access (phone, tablet, another computer)

### On your LAN

Set in the hub's `~/.blirp/config.toml`:

```toml
[portal]
lan = true
lan_port = 47771
```

and restart the daemon. The portal serves HTTPS on `https://<hub-address>:47771` with a self-signed certificate; compare the fingerprint shown in the hub's UI with the one your browser shows. To log a device in, open **Settings > Machines & sync > Devices** on an already logged-in screen and scan the one-time QR code (valid 5 minutes) with the device. Devices can be revoked there, and a browser device can type into terminals only if you allow it.

Allow inbound TCP on `lan_port` in the hub's firewall (e.g. `sudo ufw allow 47771/tcp`) if one is active.

### With Tailscale

If your devices are on a tailnet you can skip the LAN portal and let Tailscale provide HTTPS and access control:

```sh
tailscale serve --bg http://127.0.0.1:47770
```

The UI is then at `https://<hub-name>.<tailnet>.ts.net/` for devices in your tailnet only (use `tailscale serve`, never `tailscale funnel`, which would publish it to the internet). Log a device in with the same one-time QR flow. `tailscale serve reset` removes it again. The daemon's port is in `~/.blirp/runtime.json` if you changed it.

Tailscale is not needed for sync itself; iroh finds a path between machines on its own, and it also works across tailnet addresses.

## Backups and moving the hub

Everything is in `~/.blirp` on the hub:

- `blirp.db`: all data. Back it up while the daemon runs with `sqlite3 ~/.blirp/blirp.db ".backup '/backups/blirp.db'"`, or stop the daemon and copy the file.
- `identity.key`: the hub's identity. Paired machines trust this key; keep it with the backup (it is secret, mode 0600). Losing it means pairing every machine again.
- `config.toml`: settings.

To move the hub, stop the daemon, copy `~/.blirp` to the new machine, install blirp there and start it. Every node also holds a full copy of the replicated data, so a lost hub loses nothing that already synced.

## Updating

Replace the binary with the new release and restart: `launchctl kickstart -k gui/$(id -u)/dev.blirp.daemon` (macOS) or `systemctl --user restart blirp` (Linux). The sync protocol is versioned (`blirp/sync/1`); keep the hub and the nodes on the same release, updating the hub first.

## Security checklist

- Run the hub under a normal user account, never root or a system service.
- Use full-disk encryption on the hub; it holds the (redacted) history of every paired machine.
- Keep `portal.lan` off unless you use it, and never expose the daemon port or the portal to the internet.
- Revoke lost or retired machines and browser devices in **Settings > Machines & sync**.
