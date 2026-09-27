# Run the hub on a VPS

No always-on computer at home? Rent a small Linux server (a VPS) and run the hub there. Your PCs pair with it as with any hub: memory follows you between machines, and [cloud sessions](cloud-sessions.md) keep running on the server while your PC sleeps.

- [What you need](#what-you-need)
- [1. Create a user for blirp](#1-create-a-user-for-blirp)
- [2. Install and set up the hub](#2-install-and-set-up-the-hub)
- [3. Pair your PC](#3-pair-your-pc)
- [4. Agents for cloud sessions](#4-agents-for-cloud-sessions)
- [What `blirp hub setup` does](#what-blirp-hub-setup-does)
- [Network and firewall](#network-and-firewall)
- [Browser and phone access](#browser-and-phone-access)
- [Security](#security)
- [Backups](#backups)
- [Updates](#updates)
- [Checking the server](#checking-the-server)
- [Troubleshooting](#troubleshooting)
- [Why there is no Docker image](#why-there-is-no-docker-image)

## What you need

- **Linux, x86_64 or arm64, glibc 2.35 or newer:** Ubuntu 22.04 or 24.04, Debian 12 or 13. Arm servers work the same way (for example Hetzner's CAX line or Oracle Cloud's Ampere A1). Not Debian 11 (glibc 2.31), RHEL/Rocky/Alma 9 (glibc 2.34) or Alpine (musl): the release binary does not run there ([install.md](install.md#system-requirements)); build from source or pick a newer image.
- **systemd** as the init system (every image above has it), so the hub starts at boot.
- **Memory:** 1 GB is enough for the hub alone (the daemon idles at a few tens of MB). Cloud sessions run the agents on the server too, and a Claude Code or Codex process takes a few hundred MB each: plan 2 GB or more, or add swap.
- **Disk:** the database grows with your transcript history; a few GB leave years of room for most people.
- SSH access. No domain name, public port or TLS certificate is needed.

## 1. Create a user for blirp

The hub runs as a normal user, never as root: sessions on the hub run as that user, and every paired machine can start them. On a fresh server logged in as root:

```sh
adduser blirp                                   # or: useradd -m -s /bin/bash blirp && passwd blirp
loginctl enable-linger blirp                    # its services run without a login and start at boot
rsync --archive --chown=blirp:blirp ~/.ssh /home/blirp   # reuse your SSH key for the new user
```

Then log in as that user directly: `ssh blirp@<server>`. Log in over SSH rather than switching with `su` or `sudo -u`, which give no user session; `blirp hub setup` then cannot reach the user's systemd until linger is on (it tells you when that is the case).

Already have a non-root user with `sudo`? Use it; `blirp hub setup` turns on linger itself when polkit allows it and otherwise prints the `sudo loginctl enable-linger <user>` command to run once.

## 2. Install and set up the hub

As the blirp user:

```sh
curl -fsSL https://raw.githubusercontent.com/backyarddd/blirp/main/install.sh | sh -s -- --hub
```

This installs the CLI only (no desktop app) into `~/.local/bin`, verifies the download like every install ([install.md](install.md#how-the-scripts-verify-downloads)), and runs `blirp hub setup`, which ends with an invite:

```
On the other machine run:

  blirp pair blirp1-... ABCD-EFGH

Code ABCD-EFGH is valid for 10 minutes and works once.
```

Running the command again is safe: it upgrades blirp in place and repeats the setup, which changes nothing that is already right and prints a new invite. As root it refuses and prints the steps of [step 1](#1-create-a-user-for-blirp).

## 3. Pair your PC

On your PC (with blirp installed): run the `blirp pair ...` line from the server, or **Settings > Machines & Sync > Join a hub** and paste the invite and code. Compare the machine id the join dialog shows with `blirp hub status` on the server. The invite expires after 10 minutes; `blirp hub invite` on the server makes another. Pair each machine with its own invite. More: [sync-and-hub.md](sync-and-hub.md#pair-machines).

## 4. Agents for cloud sessions

The hub syncs memory without any agent installed. To run [cloud sessions](cloud-sessions.md) on it, install the agent CLIs on the server as the blirp user and log them in without a browser on the server:

- **Claude Code:** install it (its native installer puts `claude` into `~/.local/bin`), run `claude setup-token` on any machine with a browser, then on the server `blirp agents set-token claude` and paste the token ([agents.md](agents.md#headless-login-for-a-hub)). The token is stored in `~/.blirp/secrets/` (mode 0600, never synced) and used for claude sessions and the claude summarizer.
- **Codex:** `codex login --device-auth` prints a link and a one-time code to enter in a browser anywhere (device code login must be allowed in your ChatGPT security settings). Alternatives from the [Codex authentication docs](https://developers.openai.com/codex/auth): `printenv OPENAI_API_KEY | codex login --with-api-key`, copying `~/.codex/auth.json` from a machine where you logged in (treat it like a password), or `ssh -L 1455:localhost:1455 blirp@<server>` and `codex login`, finishing the login in your local browser.
- **Git:** sessions and **Clone on <hub>** use the server's own git credentials. Give the server its own SSH key (`ssh-keygen -t ed25519`) and add it to your Git host as a deploy key or a key of a separate account, so a leaked server key does not open all your repositories.

Then run `blirp hub setup` again: the service records your `PATH` at setup time, so it finds agents installed since (this restarts the daemon when the `PATH` changed). `blirp doctor` shows the agents the daemon sees and how claude logs in.

## What `blirp hub setup` does

`blirp hub setup [--lan]` sets up any machine as an always-on hub, also one where blirp is already installed. Each step checks first, so it is safe to repeat:

1. Refuses to run as root.
2. Linux: turns on **linger** (`loginctl enable-linger`) when polkit allows it without a password; otherwise prints `sudo loginctl enable-linger <user>` to run once. Without linger, systemd stops the user's services at logout and starts them only at the next login.
3. Installs the autostart service (`blirp service install`: on Linux the `systemd --user` unit `~/.config/systemd/user/blirp.service` with `Restart=on-failure`, enabled and started). Where systemd is not running (a container) or the user's systemd is not reachable, it says so, installs no service and starts the daemon directly; that daemon does not come back after a reboot.
4. Starts the daemon if it is not running.
5. Turns LAN discovery (mDNS) off (`sync.lan_discovery = false`): a VPS has no LAN peers to find, and on a provider's shared network it would advertise the hub to other customers' machines. Pairing then always uses the invite. On Linux it also turns keep-awake off (`sessions.keep_awake = false`): a server does not sleep. `--lan` is for a hub at home instead: LAN discovery on, keep-awake left as it is. Either can be changed back in `config.toml` or **Settings**.
6. Makes the machine the hub (`blirp hub enable`; fails on a machine that is paired with another hub) and prints its status and a new invite, then the next steps.

Why a `systemd --user` unit with linger and not a system service: the daemon must run as the user whose agent logins, SSH keys and home folder the sessions use, and `blirp start`, `stop` and `update` already manage the user unit ([ARCHITECTURE.md](ARCHITECTURE.md#18-servers-and-vps-hubs)).

## Network and firewall

No inbound port is needed. Machines connect over QUIC ([iroh](https://iroh.computer)): each side dials out, a relay helps them find each other and punch through NAT and firewalls, and traffic goes directly between them when that works, else through the relay (end-to-end encrypted either way). The sync endpoint uses a random UDP port, so there is no fixed port to open, and a stateful firewall lets the hole-punched traffic through like a home router does.

A firewall that only allows SSH is the right setup:

```sh
sudo ufw allow OpenSSH
sudo ufw enable
```

The server needs outbound UDP and HTTPS (443) for the relays and DNS discovery. `sync.relay` and its privacy trade-offs: [sync-and-hub.md](sync-and-hub.md#network-relay-and-privacy). The daemon's own API listens on `127.0.0.1` only.

## Browser and phone access

The web portal stays off on a VPS, and `blirp hub setup` does not turn it on. Know its model before you do: with `portal.lan = true` it listens on **all interfaces, including the public IP**, over HTTPS with a self-signed certificate. There is no password: a browser signs in by opening a one-time login link (5 minutes, single use) made on an already signed-in screen, then holds a device cookie with the rights you give that device ([portal.md](portal.md)). That is designed for a LAN; on the internet it would be one TLS listener away from anyone scanning the address. Do not expose it.

Safe ways to use the UI from outside:

- **SSH tunnel** (nothing to install): `ssh -L 47770:127.0.0.1:47770 blirp@<server>`, then run `blirp` on the server: over SSH it prints a login link for that forwarded port; open it in your local browser.
- **Tailscale or WireGuard** (also for phones): join the server to your tailnet, turn on the portal (`[portal] lan = true` and restart the daemon), and allow the portal port only on the tailnet interface; with the SSH-only `ufw` rules above it stays closed on the public IP:
  ```sh
  sudo ufw allow in on tailscale0 to any port 47771 proto tcp
  ```
  Check the provider's cloud firewall too, and confirm from outside the tailnet that `https://<public IP>:47771` does not answer. `tailscale serve --bg https+insecure://127.0.0.1:47771` adds a trusted certificate for tailnet devices ([portal.md](portal.md#tailscale)); never `tailscale funnel`.

Usually you do not need either: the desktop app or `blirp` on your PC shows the hub's sessions once paired.

## Security

- **What the hub holds:** the redacted history of every paired machine (transcripts, summaries, memory) in `~/.blirp/blirp.db`, plus its identity key. Treat the server like a machine with all your code conversations on it. The VPS provider can read its disks; if that is not acceptable, use a server you control.
- **A dedicated, non-root user** for blirp ([step 1](#1-create-a-user-for-blirp)). Paired machines can start sessions on the hub (turn **Terminal control** off per machine on the hub if one should only read).
- **SSH:** key-only logins (`PasswordAuthentication no`), no root login (`PermitRootLogin no`) in `/etc/ssh/sshd_config`, and automatic security updates (`sudo apt install unattended-upgrades`). Your provider's hardening guide covers the rest.
- **Revoke** retired machines and devices (`blirp devices list|revoke`), and rotate any secret that ever appeared in a session: redaction is pattern based ([security.md](security.md#redaction-limits)).
- Keep-awake does not matter on a server; `blirp hub setup` turns it off. Turned back on, a sleep lock logind refuses is logged (`cannot keep this machine awake`) at most once per 10 minutes; nothing else happens.

## Backups

```sh
blirp backup ~/blirp-backup.db
```

writes a consistent copy of the database while the daemon runs (SQLite `VACUUM INTO`, mode 0600) and refuses to overwrite an existing file. Keep `~/.blirp/identity.key` and `~/.blirp/config.toml` with it: paired machines trust the identity key, and a hub restored without it has to pair every machine again. Copy the backup off the server (it contains everything the database holds).

A nightly backup with cron (`crontab -e`; `%` must be escaped there):

```
15 3 * * * $HOME/.local/bin/blirp backup "$HOME/backups/blirp-$(date +\%F).db" && find "$HOME/backups" -name 'blirp-*.db' -mtime +14 -delete
```

Restore: `blirp stop`, copy the backup to `~/.blirp/blirp.db` (remove `blirp.db-wal` and `blirp.db-shm` if present) and the saved `identity.key` and `config.toml` next to it, then `blirp start`. Every paired PC also keeps a full copy of the replicated data, so a lost hub loses nothing that had synced ([sync-and-hub.md](sync-and-hub.md#backups-and-moving-the-hub)).

## Updates

```sh
blirp update
```

verifies and installs the latest release and restarts the daemon through its service. Running sessions on the hub end as **Detached** (resume them afterwards), so update when nothing important runs, and update the hub before your PCs.

To update automatically, add a timer yourself; blirp does not install one. For example at 04:30 every Sunday, as the blirp user:

```sh
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/blirp-update.service <<'EOF'
[Unit]
Description=Update blirp

[Service]
Type=oneshot
ExecStart=%h/.local/bin/blirp update
EOF
cat > ~/.config/systemd/user/blirp-update.timer <<'EOF'
[Unit]
Description=Update blirp weekly

[Timer]
OnCalendar=Sun 04:30
Persistent=true

[Install]
WantedBy=timers.target
EOF
systemctl --user daemon-reload
systemctl --user enable --now blirp-update.timer
```

`systemctl --user list-timers` shows the next run and `journalctl --user -u blirp-update` the result. `systemctl --user disable --now blirp-update.timer` stops it.

## Checking the server

`blirp doctor` adds lines for a server:

| Line | Meaning |
|---|---|
| `[info] role: hub` | the sync role |
| `autostart:` | the systemd unit and its state (`enabled, active`); `[warn]` on a hub without one |
| `linger:` | whether the user's services run without a login; `[warn]` on a hub where it is off, with the command to fix it |
| `relay:` | `[ ok ]` with the relay the hub is reachable through; `[warn]` while it has none (outbound UDP or HTTPS blocked?) |

`blirp service status`, `journalctl --user -u blirp -f` and `blirp logs -f` show the service and the daemon.

## Troubleshooting

- **The hub stops when I log out, or is gone after a reboot:** linger is off. `sudo loginctl enable-linger <user>`, then `blirp hub setup`.
- **`systemd --user is not available` / `Failed to connect to bus`:** you switched users with `su` or `sudo -u`. Log in over SSH as the blirp user; with linger on, `blirp` also finds the user's systemd from a `sudo -iu` shell.
- **`GLIBC_2.35 not found`:** the image is too old; see [What you need](#what-you-need).
- **Agents not found in cloud sessions:** run `blirp hub setup` again after installing them, so the service's `PATH` includes their folder.
- **Relay not connected:** check that the provider's firewall allows outbound UDP and HTTPS, then `blirp logs`.

More: [troubleshooting.md](troubleshooting.md).

## Why there is no Docker image

blirp is a per-user daemon that starts agents in terminals as that user, with that user's logins, SSH keys and home folder, and it updates itself with `blirp update`. In a container, every agent and its login would have to live in the image or a volume, `blirp update` would fight the image's own versioning, and the extra layer buys little over one static binary and a systemd unit. The daemon does run under another supervisor (`blirp daemon` in the foreground, with `~/.blirp` on a persistent volume), but that setup is not tested or supported.
