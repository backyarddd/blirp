# Project file sync through the hub: design

Status: approved for v1 (phases 0 and 1). Scope: upload each project folder's working tree to the hub, run cloud sessions on the hub's copy, and open copies on other machines on demand.

## 0. Baseline

- Sync is a star: nodes push `outbox` entries, the hub orders them in `hub_log` (`crates/blirp-core/src/store/sync.rs`, `crates/blirp-sync/src/repl.rs`). One ALPN per protocol on one iroh endpoint: `blirp/pair/1`, `blirp/sync/1`, `blirp/proxy/1` (`crates/blirp-sync/src/lib.rs`, `service.rs::handle_incoming`). Frames: 4-byte length + JSON, capped at 8 MiB (`wire.rs`).
- A project folder on one machine is a `ProjectPath {project_id, machine_id, path, git_remote}`; only its owner machine writes it (`check_owner`).
- Available already: `notify-debouncer-full` (ingest watcher), `zstd`, `walkdir`, `blake3` (transitive via iroh). Not in tree: `ignore`.
- iroh-blobs 0.103 and iroh-docs 0.101 are compatible with iroh 1.2; v1 uses neither (section 9).
- Reuse: `clone.rs` (`sanitize_url`, clone jobs), `api/files.rs` (`check_relative`, `resolve_inside`).

## 1. What syncs

Unit: the **root** = one `ProjectPath` on its origin machine. `root_id = hex(blake3("<machine_id>\n<path>"))[..32]`, deterministic, no new replicated row. The hub keeps one logical tree per root; every working copy of that tree (origin folder, hub copy for cloud sessions, copies on other nodes) writes to the same root (Dropbox Nucleus model: one server-side journal per namespace).

The same git repo cloned on two machines is two roots of one project; never merged file by file (git merges those).

Never synced: Home/Chats buckets and scratch projects, anything inside `BLIRP_HOME` except folderless-project workspaces, roots on removable or network drives (warning in v1).

Git: sync the working tree (tracked + untracked, not ignored) plus a git manifest; never `.git` (concurrent pack/index/lock writes corrupt copies; syncing `.git/hooks` would allow code execution on the receiver). Manifest per root: `{remote, branch, head_sha, upstream_sha}`, read with plain git commands, uploaded with every commit batch. Unpushed commits travel via the remote (phase 2: `git bundle create <upstream>..HEAD`). Scanning pauses while `.git/index.lock` or `.git/rebase-*` exists.

Folderless projects (`BLIRP_HOME/workspaces/<project-id>/`) are ordinary roots; downloaded into the receiving machine's own `workspaces/<project-id>/`.

## 2. Exclusions

Engine: the `ignore` crate (ripgrep's): `WalkBuilder::require_git(false)`, `.add_custom_ignore_filename(".blirpignore")` (higher priority), `.hidden(false)`, never follow symlinks. Layers, later wins, `.blirpignore` may re-include with `!`:

1. Always excluded, not overridable: `.git`, `.hg`, `.svn`, `.jj`, `.blirp-tmp-*`, anything resolving into `BLIRP_HOME`.
2. Build denylist: `node_modules/`, `target/`, `dist/`, `build/`, `out/`, `.next/`, `.nuxt/`, `.svelte-kit/`, `.turbo/`, `.cache/`, `__pycache__/`, `.venv/`, `venv/`, `.tox/`, `.gradle/`, `.idea/`, `*.pyc`, `.DS_Store`, `Thumbs.db`, `*.o`, `*.obj`, `*.class`, `*.dll`, `*.so`, `*.dylib`, `*.exe` (outside `bin/` of non-git roots), `*.iso`, `*.dmg`, `*.zip` over 10 MB.
3. `.gitignore` rules.
4. Secrets denylist (default, visible in the UI): `.env`, `.env.*` (except `.env.example`, `.env.sample`, `.env.template`), `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.jks`, `*.keystore`, `*.kdbx`, `*.keychain*`, `id_rsa*`, `id_ed25519*`, `id_ecdsa*`, `.npmrc`, `.pypirc`, `.netrc`, `.git-credentials`, `.dockercfg`, `.docker/config.json`, `credentials.json`, `service-account*.json`, `*.tfstate*`, `.aws/`, `.ssh/`, `.gnupg/`. Content check: files under 1 MiB whose first 4 KiB match `-----BEGIN .*PRIVATE KEY-----` (reuse `redact.rs` pattern) are excluded and reported. Secrets are excluded, never redacted.
5. `.blirpignore`; `!pattern` re-includes from layers 2-4; re-included secrets are marked red in the UI.

Caps (`[files]` config): `max_file_mb = 50` (skip + list); `max_root_gb = 2` and 100 000 files per root (whole root paused: "too large, add a .blirpignore"); `hub_quota_gb = 0` meaning up to 50% of the hub's free disk at enable time (new uploads refused with `hub_quota`, existing copies stay readable).

UI lists excluded files by reason (ignored / secret / too large) with counts and the first 50 paths. Dry run: `GET /api/projects/:id/files-sync/preview`.

## 3. Change detection and transfer

- Watcher: `notify-debouncer-full`, 2 s debounce (as `ingest/service.rs`); upload after 2 s quiet.
- Rescan at startup, every 10 min, and after watcher errors (inotify limits, FSEvents drops). Rescan is the source of truth.
- Local hash cache `(size, mtime_ns, file_id/inode) -> blake3`; rehash on change or when mtime is within 2 s of the previous scan (racy-git rule).
- BLAKE3 on `spawn_blocking` behind a semaphore of 1.
- Whole-file content addressing in v1 (dedupe across versions, roots, projects; renames free). No sub-file chunking in v1.
- Blob storage: zstd level 3 on the wire and on the hub at `BLIRP_HOME/files/blobs/ab/cd/<hash>.zst`; hub decompresses and verifies the hash, fsyncs, then renames into place.
- Limits: `[files] upload_kbps = 0` (unlimited, token bucket), at most 4 concurrent blob streams per node, "Pause file sync" switch. Battery/metered deferral is phase 2.

## 4. Versions and conflicts

Hub-sequenced versions with compare-and-set against a base version (single sequencer, so no vector clocks).

- Hub: `file_entries(root_id, path) -> {version, hash|tombstone, size, mode_x, mtime, by_machine}`, `version` = the root's monotonic `root_seq`.
- Each working copy keeps a synced base: `file_base(copy, path) -> {base_version, base_hash}` (local / synced / remote, as Nucleus).
- Commit `{path, base_version, new_hash|delete}` is accepted only if the current version equals `base_version`; otherwise conflict.
- Conflict: the second commit to arrive loses; the hub stores its content at `name.conflict-<machine-name>-<YYYYMMDD-HHMMSS>.ext` (machine name sanitized to `[A-Za-z0-9-]`); the loser's copy downloads the winner and writes its own content to the conflict path locally. Never silently overwrite. Conflict copies sync as normal files, max 10 per path; older ones stay in history.
- Delete vs modify: modify wins, file restored. Tombstones kept 90 days.
- Renames: delete + add in v1 (no bytes move; same hash). Pairing by hash is phase 2.
- Case-insensitive targets (Windows, default macOS): case-only collisions are written as conflict copies. Windows-invalid names (`CON`, `aux.c`, trailing dot/space, `:<>|?*`) skipped and listed.
- Metadata: executable bit and mtime only. Symlinks stored as links with target text; recreated on Unix, skipped with a notice on Windows; links leaving the root never followed.

## 5. Copies on other machines

- On demand (default): a node downloads a root only when you open or start a session in a project with no folder on that machine. The new-session dialog offers "Download from hub (N files, X MB, from <machine>, updated <time>)" next to Clone on hub; with several roots, the most recently updated is preselected.
- Location: default `~/blirp/<project-name>`; the user can pick another empty or missing folder. Folderless projects go to `BLIRP_HOME/workspaces/<project-id>/`.
- Git roots: if the manifest has a remote and git is installed, clone (existing `clone.rs` job), checkout `head_sha` (else `branch`, and say unpushed commits appear as local changes), then write the hub's files on top and apply tombstones. Without a usable remote: plain files, "no git history".
- The copy is registered as that machine's `ProjectPath` with an explicit `project_id` (joins non-git projects across machines). Local table `file_copies(path, root_id, mode)`.
- Modes: `on_demand` (v1 default): uploads its edits live; fast-forwards from the hub at session start and on "Update from hub". `keep_synced` (phase 2): live both ways.
- Writing: temp file `.blirp-tmp-<rand>` in the target directory, fsync, re-check the local file still matches its base (size, mtime, hash), then rename; if it changed, write a conflict copy. Every path passes `check_relative`, has no VCS or `.blirp-tmp` component, and no symlinked parent; otherwise refused and logged.
- The origin folder is upload-only in v1. Remote changes show as "Hub has N newer files · Bring changes here"; the action lists files, fast-forwards files unchanged since their base, writes conflict copies for the rest. Automatic writes into origin folders need a per-folder `keep_synced` opt-in (phase 2).

## 6. Toggle

- Global `[sync] project_files = true` (new `SyncConfig` field). Effective only when role is `hub` or `node`.
- Per project Default / On / Off, stored on the hub in `file_projects(project_id, mode)`, not replicated through `hub_log`; nodes read it in `Welcome`/`Roots`. Project settings "Files on hub": toggle, per-root status (last upload, files, size, pending, excluded by reason), Preview, Pause, Delete hub copy.
- Off: uploads stop; hub copy kept read-only ("paused"). "Delete hub copy" is separate and explicit: entries and history dropped, GC removes blobs, copies elsewhere stay on disk but detach.
- First run after upgrade: banner "Uploading N projects (X GB) to <hub>. Review exclusions / Turn off"; initial upload starts after a 10-minute grace period; preview available immediately.

## 7. Security and privacy

- Paired, non-revoked machines (`authorized()` in `service.rs`) may read roots and commit. Origin folders change only through their own opt-in.
- No at-rest or end-to-end encryption in v1: cloud sessions on the hub copy need plaintext; the hub is the user's machine (security.md already requires full-disk encryption); transport is encrypted by iroh. Optional per-project E2E ("storage-only") is phase 4.
- Portal devices: new device permission "Files", off by default; without it the files API returns 403 to portal devices. Devices with terminal control can already run a shell on the hub (documented).
- Retention: replaced versions kept `keep_versions_days = 30` in `file_history`; tombstones 90 days; daily mark-and-sweep GC of unreferenced blobs, skipping blobs younger than 1 h; quota enforcement prunes history before refusing uploads.
- Revoked machines: refused; their roots stay, shown "from a revoked machine" with Delete. Leaving a hub keeps local files.
- Logs: paths and counts only, never contents.

## 8. Cloud sessions on the hub copy

The hub runs the same file engine in-process (no network). "Use hub copy (includes uncommitted changes)" becomes the default in the Run-on-hub dialog when a root exists; Clone on hub remains. The hub creates its copy on demand at `~/blirp/<project-name>` on the hub via the section 5 path (clone + overlay), registers it as the hub's `ProjectPath`; session edits commit back to the root live (normal writer copy with compare-and-set). The origin sees "Hub has N newer files". Concurrent edits produce conflict copies on both sides; no edit is lost.

## 9. Protocol

New ALPN `blirp/files/1` accepted by the hub, own connection, same `authorized()` check. Control messages: existing length-prefixed JSON frames. Blob bytes are raw after a header frame. Streams: one control stream (request/response), one unidirectional `Notify{root_id, seq}` from the hub, one bidirectional stream per blob transfer.

| Message | Answer | Purpose |
|---|---|---|
| `Hello{versions, machine}` | `Welcome{version, quota, project_modes}` | Handshake |
| `Roots` | `RootList` | Roots and state |
| `Index{root_id, after_seq}` | `IndexPage{entries, head}` | Metadata, max 5000 entries per page |
| `Have{hashes}` | `Missing{hashes}` | Dedupe |
| `PutBlob{hash, len, zlen, offset}` + raw bytes | `BlobAck` | Upload |
| `GetBlob{hash, offset}` | header + raw bytes | Download |
| `Commit{root_id, manifest?, changes[<=1000]}` | `CommitResult{per path: Ok{version} \| Conflict{current_version, copy_path}}` | Apply |
| `DeleteRoot{root_id}` | - | Remove hub copy |

Commits reference only blobs the hub has (`missing_blob` otherwise); each batch is one SQLite transaction (backpressure). Resume: partial uploads at `files/tmp/<hash>.part`, length reported in `BlobAck`/`Missing`, sender continues from offset; downloads resume with `offset`.

The file index lives in hub-only tables (`file_roots`, `file_entries`, `file_history`, `file_projects`) with a per-root sequence; not in `hub_log` (per-save churn would bloat it and reach every node).

Why not iroh-docs (per-key LWW by timestamp: silent overwrites; replicates whole namespaces) or iroh-blobs (0.x churn, large dependency surface, hash-knowledge access needs an extra authorization layer; replaces only ~200 lines here). Revisit iroh-blobs for large files in phase 3.

## 10. Phases

- Phase 0: scanner and exclusions (`ignore`, `blake3` direct deps), secrets content check, hash cache, preview API. Nothing leaves the machine.
- Phase 1 (v1): `blirp/files/1`, hub store, quota, GC; origin to hub upload (watcher + rescan); compare-and-set commits and conflict copies; hub copy for cloud sessions (clone + overlay); on-demand download to other nodes; Update from hub; Bring changes here; global and per-project toggle; settings UI; portal Files permission; docs (`docs/project-files.md`, sync-and-hub, security, configuration, cloud-sessions).
- Phase 2: `keep_synced` bidirectional (incl. opted-in origins), rename pairing, battery/metered detection, `git bundle` for unpushed commits, version history and restore UI.
- Phase 3: chunked large files with ranged transfer, higher caps.
- Phase 4: optional per-project E2E encryption.

Tests: model-based property test (proptest, dev-only) with N writers and random interleavings/crashes; invariants: every locally written byte string survives at its path, in a conflict copy or in history; copies converge when quiet; nothing written under a VCS dir or outside the root; modify beats delete. Loopback iroh integration tests (hub + two nodes): upload → copy elsewhere → edit on hub copy → bring back; interrupted transfer resume; hash mismatch rejection; revoked machine refused; quota; malicious commits (`../x`, `.git/hooks/post-checkout`, `CON`, symlinked parent, case collisions). Unit tests for each exclusion layer and the racy-mtime rule. Web e2e: toggle, preview, conflict badge.

Migration: new tables only. A node whose hub rejects `blirp/files/1` shows "update the hub to sync project files" and continues normal sync. Default-on grace period and banner apply on upgrade.

Risks: secrets (denylist, content check, preview, grace period); hub disk (quota, GC); folder corruption (upload-only origins, atomic re-checked writes, `.git` never written); huge repos (caps, hash cache, pause); missed watcher events (rescan); OS path rules (name checks, conflict copies); unpushed commits (dialog note; phase 2 bundles).
