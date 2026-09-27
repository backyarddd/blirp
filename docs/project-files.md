# Project files on the hub

With a hub, blirp keeps a copy of your project folders on it, including uncommitted changes. Cloud sessions can then work on exactly what you have on your PC, and another machine can open a project it has no folder of yet. Design: [design/project-file-sync.md](design/project-file-sync.md).

- [What syncs](#what-syncs)
- [What stays on your machine](#what-stays-on-your-machine)
- [Turning it on and off](#turning-it-on-and-off)
- [Copies on other machines and on the hub](#copies-on-other-machines-and-on-the-hub)
- [Conflicts](#conflicts)
- [Limits](#limits)
- [Security and privacy](#security-and-privacy)
- [Troubleshooting](#troubleshooting)

## What syncs

Each project folder on a machine (its **origin**) uploads its working tree to the hub: tracked and untracked files that are not ignored, plus a small git manifest (remote, branch, HEAD, upstream). It is only on machines paired with a hub, or on the hub itself; standalone machines upload nothing.

- The same git repo cloned on two machines is two folders on the hub, never merged file by file: git merges those.
- `.git` itself is never synced (copying it while git writes corrupts it, and hooks would run on the receiver). Unpushed commits travel through your git remote; a copy made elsewhere shows them as local changes.
- A project without folders syncs its blirp workspace on each machine like a folder; a copy downloaded elsewhere goes to that machine's own workspace of the project (and the project stays without folders).
- Chats (sessions that belong to no project), scratch folders blirp made on its own (temp folders, tool data), anything else inside blirp's data folder and folders on removable or network drives are never synced; the project's **Files on hub** tab says why.
- Changes upload about 2 seconds after they settle, and every folder is rescanned at start and every 10 minutes. Identical content is stored once.
- The origin folder is **upload-only**: blirp never writes into it unless you click **Bring changes here**.

## What stays on your machine

Checked in this order, a later layer winning over an earlier one:

1. Always: `.git`, `.hg`, `.svn`, `.jj`, blirp's temp files, blirp's data folder.
2. Build output and caches: `node_modules/`, `target/`, `dist/`, `build/`, `out/`, `.next/`, `.nuxt/`, `.svelte-kit/`, `.turbo/`, `.cache/`, `__pycache__/`, `.venv/`, `venv/`, `.tox/`, `.gradle/`, `.idea/`, `*.pyc`, `.DS_Store`, `Thumbs.db`, `*.o`, `*.obj`, `*.class`, `*.dll`, `*.so`, `*.dylib`, `*.iso`, `*.dmg`, `*.exe` (except under `bin/` of folders that are not git repos), `*.zip` over 10 MB.
3. Your `.gitignore` files, `.git/info/exclude` and your global gitignore (`core.excludesFile`); a `!pattern` there brings back build output.
4. Secrets: `.env`, `.env.*` and `*.env` (not `.env.example`, `.env.sample`, `.env.template`), `.envrc`, `.pgpass`, `.kube/config`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.jks`, `*.keystore`, `*.kdbx`, `*.keychain*`, `id_rsa*`, `id_ed25519*`, `id_ecdsa*`, `.npmrc`, `.pypirc`, `.netrc`, `.git-credentials`, `.dockercfg`, `.docker/config.json`, `credentials.json`, `service-account*.json`, `*.tfstate*`, `.aws/`, `.ssh/`, `.gnupg/`, and any file under 1 MiB whose first 4 KiB contain a `-----BEGIN ... PRIVATE KEY-----` header. Secrets are left out, never redacted.
5. `.blirpignore` files (gitignore syntax, in any folder): leave more out, or bring anything from 2-4 back with `!pattern`. A secret brought back this way is shown in red.

**Preview** on the Files on hub tab is a dry run on your machine: how many files and bytes would upload and what is left out by reason (ignored, secrets, too large, cannot sync) with the first 50 paths of each.

## Turning it on and off

- **This machine**: `[sync] project_files` (default on), or **Settings > Machines & Sync > Upload project folders to the hub**. Off is a hard opt-out: this machine never uploads, whatever a project's setting says. **Pause file sync** there stops all uploads from this machine, and the automatic update of copies when a session starts, until you resume. While paused or off, **Bring changes here** and **Update from hub** still take the hub's changes but upload nothing first.
- **Per project** (on the project's **Files on hub** tab): **Default** and **On** upload from every machine that has file sync on, **Off** stops uploads everywhere and keeps the hub copy readable. The choice is stored on the hub and applies to every machine.
- **Many files disappear at once** (every synced file of a folder that had more than one, or at least 10 files and more than 30% of them in one pass, e.g. an emptied or swapped folder): nothing uploads from that folder and its tab says "N files disappeared". **Restore from hub** writes those files back (only while the folder exists), **Delete on hub too** confirms the delete of exactly the files listed (anything that disappears later asks again); until you choose, the folder stays paused.
- **First run**: after upgrading or pairing, uploads wait 10 minutes. A banner says how many folders and how much will go to which hub, with **Review exclusions**, **Upload now** and **Turn off**.
- **Delete hub copy** (only while the project is Off, or its origin machine was revoked) removes the files and their history from the hub. Folders on your machines stay as they are; copies elsewhere stop syncing.

## Copies on other machines and on the hub

- **New session on a machine without the project's folder**: the dialog offers **Download from hub** (files, size, which machine, when updated; with several folders the most recently updated one). The copy goes to `~/blirp/<folder name>` (or a folder you pick that is empty or missing; folderless projects go to their workspace folder). For a git folder whose remote can be cloned there, blirp clones it, checks out the origin's HEAD (if it is on the remote; else its branch) and writes the hub's files on top, so uncommitted work comes along. Otherwise you get plain files without git history.
- **Cloud sessions**: running on the hub, the dialog offers **Use hub copy (includes uncommitted changes)**, the same download on the hub. Edits the session makes sync back to the hub live.
- A copy is that machine's folder of the same project. It uploads its own edits live and takes the hub's changes when a session starts in it and on **Update from hub**. A download that could not write every file does not sync until **Update from hub** completes it.
- If a hub copy is deleted and its origin uploads it again, copies of the old one on other machines stop syncing (their files stay); download a new copy.
- On the origin, the tab shows **Hub has N newer files**; **Bring changes here** lists them and replaces only files you did not change since they last synced.

## Conflicts

Every change carries the version it was based on, and the hub accepts it only if nobody changed the file meanwhile. The second edit to arrive is kept as `name.conflict-<machine>-<YYYYMMDD-HHMMSS>.ext` next to the file (at most 10 per file; older ones stay in the history), and the machine that lost the race gets the winner at the original name and its own version in the conflict copy. Nothing is overwritten silently:

- An edit beats a delete: the file comes back.
- Bringing the hub's version into a file you changed writes the hub's version as a conflict copy next to yours; yours uploads.
- Names Windows cannot hold (`CON`, `aux.c`, trailing dots, `:<>|?*`), symlinks on Windows and names that differ only in case on Windows or macOS are skipped on that machine (the name it already holds stays; the other one stays on the hub) and never deleted on the hub because of it.

The tab shows a **conflict copies** badge per folder.

## Limits

`[files]` in config.toml ([configuration.md](configuration.md#files)):

- Files over `max_file_mb` (50) are skipped and listed; the hub's own `max_file_mb` applies too.
- A folder over `max_root_gb` (2) or 100 000 files is paused as too large: add a `.blirpignore`.
- `hub_quota_gb` (0 = half of the hub's free disk when file sync first ran there): old versions are dropped first; then new uploads are refused (`hub_quota`) while existing copies stay readable.
- `upload_kbps` (0 = unlimited) limits uploads; at most 4 transfers run at once. Interrupted transfers resume.
- Replaced versions are kept `keep_versions_days` (30) days, deletions 90 days. A copy that was offline for longer than that and missed a delete keeps its file.

## Security and privacy

- Only paired, non-revoked machines read or write hub copies; a revoked machine is refused at once. Leaving a hub keeps your local files.
- Files are encrypted in transit (iroh) but stored in plain form on the hub, so cloud sessions can use them: keep full-disk encryption on the hub. Secrets are left out by default.
- Every path from the network is checked: nothing is written outside the folder, into a VCS folder (`.git/hooks` would run code), through a symlinked folder, or over a file that changed since blirp last saw it (writes go to a temp file that is checked again before it is moved into place).
- Browser devices on the LAN portal need the **Files** permission (off by default; **Settings > Machines & Sync > Devices**) for project files, diffs and file sync. A device with terminal control can run a shell on the hub anyway.
- Logs name paths and counts, never file contents.

## Troubleshooting

- **"update the hub to sync project files"**: the hub runs an older blirp. Everything else keeps syncing; update the hub.
- **Folder missing**: the project's folder is not on this machine any more (deleted, moved, or on a drive that is not mounted). Nothing uploads from it and the hub copy stays as it was; when the folder is back, it syncs again within 10 minutes. To stop tracking it, remove the folder from the project.
- **Waiting for git**: a git operation (`index.lock`, rebase) is in progress; scanning resumes when it ends.
- **Too large**: see [Limits](#limits).
- **The hub's storage for project files is full**: raise `hub_quota_gb` on the hub, or delete hub copies you no longer need.
