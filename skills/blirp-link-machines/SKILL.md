---
name: blirp-link-machines
description: Link computers with blirp sync (make one machine the hub, create an invite, pair another machine, list or revoke paired machines and browser devices, stop being a hub). Use when the user wants to connect, pair, sync or unlink machines running blirp, set up a hub (including one on a server), or check sync state.
---

# Link machines with blirp

One machine is the **hub**; the others pair with it and become **nodes**. Every machine keeps a full copy of the synced projects, sessions and memory. All commands need the daemon (`blirp start` if `blirp status` says it is not running).

## Look first (read-only)

```sh
blirp hub status
blirp devices list
```

- `blirp hub status`: `role` (`standalone`, `hub`, `node`), `machine`, `hub`, `connected`, `pending` changes, and on a hub with the portal on its `portal` URL and `cert` fingerprint.
- `blirp devices list` (hub only): paired machines and browser devices with id, kind, state, terminal control and name.

## Pair a new machine (ask the user first)

Run `blirp hub enable`, `blirp hub invite` or `blirp pair` only when the user asked in this conversation to link machines, and confirm each one with them before running it: they change this machine's sync role, create credentials that grant access to all synced data, or join this machine to a hub.

1. On the machine that will be the hub (only a `standalone` machine can become one; a paired node fails with `paired_node`):

   ```sh
   blirp hub enable
   ```

   It prints the status and a first invite. For each further machine:

   ```sh
   blirp hub invite
   ```

   Output: a line `blirp pair <INVITE> <CODE>`. The code is valid 10 minutes, works once and dies after 5 wrong attempts or a hub restart.
2. On the other machine (must be `standalone`):

   ```sh
   blirp pair <INVITE> <CODE>
   blirp pair <CODE>
   ```

   The short form finds the hub on the same local network (mDNS; gives up after 5 s with `no_hub_found`) and fails with `invite_required` when the hub has several open invites or LAN discovery is off; then use the full invite. Pairing waits up to 2 minutes and prints the new sync status (`role node`).
3. Verify on both: `blirp hub status` shows `connected yes`.

The invite plus code lets a machine join the user's hub and receive all synced data. Give them only to the user, never write them into files, commits, issues or chat outside this session. If the two machines are not both reachable by you, print the pair line from the invite output and let the user run it on the other machine.

A hub on a server (VPS) or always-on computer: follow the setup guide in the blirp docs (`docs/sync-and-hub.md` in the blirp repository, and a VPS guide if the installed version ships one). Suggest the steps; the user runs anything that needs their server login.

## Remove and unlink (ask the user first, every time)

```sh
blirp devices revoke <ID>
blirp hub disable
```

- `blirp devices revoke <ID>` (on the hub, id from `blirp devices list`): cuts that machine or browser off at once; reconnects are refused. Data it already synced stays. Irreversible for that device: it must pair again.
- `blirp hub disable`: sync and the portal stop, the role goes back to `standalone`; paired machines stay known, so `blirp hub enable` later resumes without re-pairing.
- Leaving the hub on a node has no CLI command: the user clicks **Settings > Machines & Sync > Leave hub** in the blirp UI (`blirp open`).
- Whether the hub may start and control terminals on a node (`sync.allow_hub_control`) is the user's decision in **Settings > Machines & Sync**; never turn it on yourself.

## Troubleshooting

- `connected no`: check both daemons run (`blirp status`), and the log on each (`blirp logs -n 100`). Firewalls, VPNs and blocked mDNS are the usual causes; see skill `blirp-status`.
- Hub and nodes should run the same blirp version; update the hub first (skill `blirp-update`).
