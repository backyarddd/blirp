# Cloud sessions

A cloud session runs on your hub (for example an always-on Mac mini or Linux box) while you use it from another machine. You start it from your main PC, close the laptop lid or let the PC sleep, and the next day you open blirp again: the session is still running on the hub, the tab you had open comes back, and scrolling up shows what happened. Nothing is compacted or restarted; the agent process never noticed you left.

It is the [remote session](sync-and-hub.md#remote-sessions) feature with the hub as the target, plus what makes that pleasant for long-running work: keep-awake on the hub, a folder browser and `git clone` on the hub, a login check for the agent there, and full scrollback when you reattach.

- [Set up the hub (macOS example)](#set-up-the-hub-macos-example)
- [Pair your PC](#pair-your-pc)
- [Start a cloud session](#start-a-cloud-session)
- [Leave and come back](#leave-and-come-back)
- [Keep-awake](#keep-awake)
- [Limits](#limits)
- [Troubleshooting](#troubleshooting)

## Set up the hub (macOS example)

Do this at the Mac itself (or over Screen Sharing), logged in as the user the agents should run as.

1. Install blirp ([install.md](install.md)) and the agent CLIs you want to use there.
2. Log in to each agent in Terminal on the Mac, for example run `claude` and sign in. Claude Code keeps its login in the macOS login keychain.
3. Run blirp as a service so it starts at login and survives logging out of SSH and reboots:
   ```sh
   blirp service install     # ~/Library/LaunchAgents/dev.blirp.daemon.plist
   blirp service status
   ```
   Run this in Terminal on the Mac, or over SSH while you are logged in at the Mac: `service install` loads the daemon into your login session either way. Keep the Mac logged in (turn on automatic login so it comes back after a restart). A daemon started directly in an SSH shell (`blirp daemon --detach`) runs outside that session and cannot read the login keychain, so `claude` would not be logged in there; blirp shows this in the new-session dialog.
   A LaunchAgent on a Mac whose screen is locked (or after a reboot before anyone logs in) cannot read the keychain either. To make Claude independent of it, give the hub a login token: `claude setup-token` on any machine with a browser, then `blirp agents set-token claude` on the Mac ([agents.md](agents.md#headless-login-for-a-hub)). This works over SSH too.
4. Keep the Mac up:
   - **System Settings > Users & Groups > Automatically log in as** your user, so the LaunchAgent starts after a reboot (not available with FileVault; then log in once after each reboot, for example over Screen Sharing).
   - **System Settings > Energy > Prevent automatic sleeping when the display is off** (or `sudo pmset -a sleep 0`). blirp also holds a sleep assertion while sessions run ([keep-awake](#keep-awake)); the Energy setting covers the time in between.
   - **System Settings > Energy > Start up automatically after a power failure**.
   - The display may sleep and the screen may lock; sessions keep running.
5. Make it the hub: `blirp hub enable` (or **Settings > Machines & Sync > Enable hub**). It prints an invite and a pairing code.

Linux and Windows hubs work the same way; see [sync-and-hub.md](sync-and-hub.md#set-up-a-hub) for `loginctl enable-linger` (Linux) and power settings (Windows).

## Pair your PC

On the PC: `blirp pair <invite> <code>`, or **Settings > Machines & Sync > Join a hub**. The hub accepts terminal control from paired machines by default (**Devices > Terminal control** on the hub turns it off per machine). Your PC does not need to allow the hub to control it for cloud sessions.

After pairing, the hub's sessions replicate to the PC: they show up in the sidebar with a cloud badge, and their terminals attach through the hub.

## Start a cloud session

Press **+** (Ctrl+T). With a hub paired, the dialog starts with **Run on**: **This machine** (the default), **Cloud (<hub name>)**, and any other paired machine. Picking the hub changes the rest of the dialog to the hub's view:

- **Agent** lists the agents installed on the hub, with their versions. If Claude Code is installed there but not logged in for the hub's blirp daemon, the dialog says so and how to fix it ([below](#claude-not-logged-in-on-the-hub)).
- **Project**: a project that already has a folder on the hub starts there. A git project that exists only on your PC offers **Clone on <hub>**: the hub runs `git clone` with the remote URL of your local repository into `~/blirp/<repo name>` (or a folder you pick), with progress and git's error output in the dialog. The hub uses its own git credentials (SSH keys, credential helper); nothing is copied from the PC, and credentials inside the URL (`https://user:token@...`) are removed before it is sent. Once cloned, the session joins the same project (matched by the git remote).
- **Folder on <hub>**: type a path or **Browse…** folders on the hub. The browser shows folder names only (never files or their contents), only inside the hub user's home, hidden folders on request, and git repositories marked. **Recent on <hub>** offers folders earlier sessions ran in there.

Non-git folders cannot be cloned; pick an existing folder on the hub. Anything the agent talks to has to be on the hub too: for example an MCP server that drives a desktop app needs that app running on the hub machine, not on your PC.

Start the session: its terminal opens on the PC, streamed from the hub. Session cards, the session toolbar and grid tiles show a machine badge (a cloud for the hub); sessions on this machine have none.

## Leave and come back

- Closing the tab or window, quitting the app, sleeping or shutting down the PC, or even stopping the PC's daemon does not touch the session: the process runs on the hub.
- The session is live in the sidebar on every paired machine (session rows replicate; status changes arrive within a few seconds).
- Opening blirp again shows the session you had open last (remembered per browser/app in local storage), and its terminal reattaches.
- On reattach the hub sends the whole terminal state: the visible screen plus the scrollback it keeps (10 000 lines, at most 16 MiB of it per attach). Scrolling up shows the history as it was. Claude Code draws on the normal screen, so its conversation is in that scrollback; when it clears and redraws its history (`ESC [3J`, e.g. after a resize), the kept scrollback is cleared too, as in any terminal, so it is not shown twice.
- Full-screen TUIs that use the alternate screen (vim, htop, some agents in full-screen mode) have no terminal scrollback: they redraw their own screen and keep their own history.
- If the PC went to sleep while a terminal was attached, the hub notices when the connection times out. Terminal queries (cursor position, device attributes) that the sleeping client cannot answer are answered by the hub after 2 s, so a TUI starting meanwhile does not hang.

## Keep-awake

While any session runs on a machine with `sessions.keep_awake = true`, blirp keeps that machine from idle-sleeping, and lets it sleep again as soon as the last session ends:

- macOS: `caffeinate -i -w <daemon pid>` (visible in `pmset -g assertions` as "caffeinate command-line tool"; it also ends if the daemon dies),
- Windows: `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)`,
- Linux: `systemd-inhibit --what=sleep`, when available.

It is on by default for the hub and off for other machines ([configuration](configuration.md#sessions)). The top bar shows **Keeping <machine> awake** for this machine and, on a paired PC, for the hub. It does not stop a sleep you ask for (Apple menu > Sleep, closing a MacBook lid) and does not keep the display on.

## Limits

- The hub must be on and online. If it is unreachable the session cannot be attached (`machine_unreachable`), but it keeps running there.
- Restarting the hub's daemon (or the hub) ends its running sessions; they become **Detached**. **Resume** continues the agent's conversation (`claude --resume` and the equivalents), in a new process.
- Terminal history lives in the hub daemon's RAM, not on disk: 10 000 lines per session, gone when the session ends. The agent's transcript is ingested as usual, so search and project memory keep it.
- Sessions run as the hub's user with the hub's files, credentials and tools. The dialog never transfers secrets or files from the PC; clone only passes a URL.
- The folder browser is limited to the home folder of the hub user.

## Troubleshooting

### Claude not logged in on the hub

The dialog shows "Claude Code is not logged in on <hub>" when `claude auth status` (run by the hub's daemon, no model call) says so. The daemon cannot use the login keychain when it was started over SSH, or when the Mac is locked. The reliable fix is a login token: run `claude setup-token` on any machine with a browser, then on the Mac (SSH is fine) `blirp agents set-token claude` and paste the token ([agents.md](agents.md#headless-login-for-a-hub)); the next session uses it, no restart needed. Without a token, log in on the Mac (`claude` in Terminal) and make sure the daemon is the LaunchAgent started at login (`blirp service status`), not one started over SSH.

### Claude Code does not show its prompt

`claude` started inside a launchd job that was loaded over SSH, while the Mac's screen is locked, can stop before drawing its prompt (with and without blirp); it cannot reach the login keychain there. Store a [login token](agents.md#headless-login-for-a-hub) (`claude setup-token`, then `blirp agents set-token claude`) so claude does not need the keychain, and check a first cloud session before relying on it.

### The Mac still sleeps

Check `pmset -g assertions` while a session runs: a "caffeinate command-line tool" assertion on behalf of the blirp daemon should be listed. Without it, see the daemon log (`blirp logs`) for `cannot keep this machine awake`, and whether `[sessions] keep_awake = false` is set. Scheduled sleep and a sleep you trigger still apply.

### `error sending mDNS: No route to host` in the log (macOS)

macOS has not granted blirp Local Network access (System Settings > Privacy & Security > Local Network). Sync and cloud sessions still work over the relay; only `blirp pair <code>` without an invite needs local discovery. Allow the access, or set `[sync] lan_discovery = false`; see [troubleshooting.md](troubleshooting.md#error-sending-mdns-no-route-to-host-in-the-log-macos).
