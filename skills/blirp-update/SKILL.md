---
name: blirp-update
description: Check for and install blirp updates (the CLI, daemon and desktop app installed by the blirp install script), including updating a hub and its paired machines in the right order. Use when the user asks to update or upgrade blirp, which version is installed, or whether an update is available.
---

# Update blirp

## Check (read-only, safe)

```sh
blirp update --check
```

Prints one line. Exit code `0` = up to date, `10` = an update is available (the line names the version and how to update). It only reads the latest published release.

## Update (ask the user first)

```sh
blirp sessions --limit 50
blirp update
```

`blirp update` downloads the release, verifies its signature and checksums, then **stops the daemon**, replaces `blirp` and the desktop app, and starts the daemon again. Stopping the daemon ends every running session (they become Detached; the user can resume them). So, before updating:

1. Run `blirp sessions --limit 50` and tell the user which sessions are live (status `starting`, `working`, `idle` or `waiting`).
2. If `BLIRP_SESSION_ID` is set in your environment, you run inside a blirp session and the update ends it too: tell the user, and suggest they run `blirp update` in a terminal outside blirp instead.
3. Get an explicit yes. Never update unprompted or on a schedule.

Other forms:

```sh
blirp update --version <X.Y.Z>
```

Installs that exact version (the only way to downgrade). Use only when the user names the version.

## Reading the result

- `... is already installed`: nothing to do.
- `... was not installed by the blirp install script`: a source build, package or classic installer. Nothing was changed; the user updates it the way they installed it.
- `not signed by the blirp release key` or `checksum mismatch`: nothing was changed. Do not retry with a manually downloaded file; tell the user.
- `403` or rate limit: GitHub's anonymous API limit; the user can set `GITHUB_TOKEN` and retry.

## After updating

```sh
blirp status
blirp skills list
```

- `blirp status` shows the new version. If the daemon is not running, `blirp start`, then `blirp logs -n 100` if it fails.
- If `blirp skills list` shows `outdated` skills, run `blirp skills install` to refresh them (skills the user edited are left alone).

## Several machines

Hub and paired machines should run the same version (skill `blirp-link-machines`). Update the hub first, then each node. On each machine the user (or an agent on that machine) runs the same steps.
