# Development

Read [ARCHITECTURE.md](ARCHITECTURE.md) first: it is the design contract, and a change that diverges from it updates it in the same pull request. Contribution rules are in [CONTRIBUTING.md](../CONTRIBUTING.md).

## Repository layout

| Path | What |
|---|---|
| `crates/blirp-core` | model types (the API contract), config, paths, SQLite store and migrations, redaction, git helpers |
| `crates/blirp-sync` | iroh endpoint, pairing (SPAKE2), replication protocol, remote proxy |
| `crates/blirp` | the `blirp` binary: `cli/`, `daemon.rs`, `api/` (axum routes), `pty.rs`, `sessions.rs`, `agents.rs`, `ingest/` (one adapter per agent), `memory/` (distill, render, launch integration), `hooks/` (hook client, daemon side, global install), `mcp/`, `sync/`, `portal/` |
| `crates/blirp/tests` | integration tests: `daemon.rs` (real daemon + PTY + WebSocket), `ingest.rs` (fixtures per agent), `memory.rs`, `sync.rs`; `fixtures/<agent>/` synthetic transcripts |
| `web/` | Svelte 5 + Vite + TypeScript SPA (desktop UI and portal), built to `web/dist` and embedded into `blirp`; `web/e2e/` Playwright suite |
| `app/` | Tauri 2 desktop shell (`src-tauri/`); `app/src` is only the loading/error page |
| `scripts/` | `build-sidecar.{sh,ps1}` (stage `blirp` and ConPTY as the Tauri sidecar), `render-packaging.sh` |
| `packaging/` | Homebrew formula and cask, winget manifest templates |
| `.github/workflows/` | `ci.yml` (every push and PR), `release.yml` (tags `v*`) |
| `docs/` | user and developer docs; `ARCHITECTURE.md`; `agent-formats.md` (survey of agents' on-disk transcript formats) |

## Requirements

- Rust 1.89 or newer (`rust-version` in `Cargo.toml`; CI pins 1.94.0) with `rustfmt` and `clippy`.
- Node.js 22 and pnpm 12.
- `git` (tests create repositories).
- Desktop app and, on Linux, any `cargo clippy/test --workspace` (the Tauri crate is a workspace member): the [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/). Debian/Ubuntu:
  `sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev build-essential file`.

## Build and test

```sh
pnpm -C web install && pnpm -C web build    # UI into web/dist (debug builds read it from disk at runtime)
cargo build -p blirp                        # target/debug/blirp
cargo build --release -p blirp              # target/release/blirp, UI embedded
```

Without `web/dist` the binary still builds and serves a page explaining how to build the UI.

The checks CI runs on Windows, macOS and Linux:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm -C web check        # svelte-check + tsc, strict, warnings fail
pnpm -C web test         # vitest
pnpm -C web build
```

`crates/blirp-core/src/model.rs` generates `web/src/lib/api/types.gen.ts`. After changing a type there, run `cargo test -p blirp-core export_bindings` and commit the regenerated file; CI fails when it is stale.

## Running from source

Use a throwaway data directory so development never touches your real `~/.blirp`:

```sh
BLIRP_HOME=/tmp/blirp-dev cargo run -p blirp -- daemon          # foreground, logs to stderr
BLIRP_HOME=/tmp/blirp-dev cargo run -p blirp -- open            # log the browser in
```

PowerShell: `$env:BLIRP_HOME = "$env:TEMP\blirp-dev"; cargo run -p blirp -- daemon`.

The daemon still ingests your real agent transcripts (it reads `~/.claude`, `~/.codex`, ...). Point the agents' own overrides (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`, ...) at empty folders for the daemon process if you want an empty instance. Use `--port 0` to run next to an installed daemon.

### UI development with hot reload

```sh
cargo run -p blirp -- daemon --port 47770     # the Vite proxy expects 47770
pnpm -C web dev                               # http://localhost:5173
```

Log the Vite origin in once: open `http://localhost:5173/auth?token=<token from runtime.json>`. Vite proxies `/api`, `/auth` and `/mcp` (with WebSockets) to the daemon and rewrites `Origin` so the daemon's same-origin check passes.

### Desktop app

```sh
pnpm -C app install
pnpm -C app tauri dev     # builds and uses target/debug/blirp as the daemon
```

Debug builds of the app run `target/<profile>/blirp`; release builds need the sidecar staged by `scripts/build-sidecar.sh` (or `.ps1`) at `app/src-tauri/binaries/blirp-<target-triple>[.exe]`, plus on Windows `binaries/conpty/conpty.dll` and `binaries/conpty/x64/OpenConsole.exe` (NuGet `Microsoft.Windows.Console.ConPTY`, version and SHA-256 pinned in `build-sidecar.ps1`; bump both together).

```sh
scripts/build-sidecar.sh
pnpm -C app tauri build --config '{"bundle":{"createUpdaterArtifacts":false}}'   # unsigned local build
```

A plain `tauri build` signs updater artifacts and needs `TAURI_SIGNING_PRIVATE_KEY`. The daemon's origin gets no Tauri IPC; only the bundled loading page may call the app's commands (`app/src-tauri/capabilities/main.json`). Treat any new native capability as a security change.

## End-to-end tests

```sh
pnpm -C web e2e
```

Builds the SPA and `cargo build -p blirp`, starts the real daemon on a temporary `BLIRP_HOME` with a temporary git repository, and drives the UI with Playwright in the installed Microsoft Edge (no browser download). `BLIRP_E2E_CHANNEL=chrome` (or another Playwright channel) picks another browser; `BLIRP_E2E_KEEP=1` keeps the temp directory and `daemon.log` for inspection. Not part of `pnpm -C web test` or CI.

## Adding an agent adapter

A new agent touches these places. Look at an existing agent with the same shape (JSONL file per session: `pi`, `claude`; SQLite store: `opencode`; JSON document: `amp`) and copy its structure.

1. **Survey the format.** Find where the agent stores sessions on each OS and which environment variable moves it. Record the sanitized shape in [agent-formats.md](agent-formats.md) (no real prompts, paths or secrets).
2. **Register the id.** Add it to `BUILTIN_AGENTS` in `crates/blirp-core/src/model.rs` and regenerate the TypeScript bindings. Add a display name in `AGENT_NAMES` in `web/src/lib/status.ts`.
3. **Launch spec.** Add a `Builtin` to `BUILTINS` in `crates/blirp/src/agents.rs`: display name, binary names in preference order, and `Resume::Args(&[...])` if the agent can resume by id (the id is appended), else `Resume::None`. Add cases to the argv tests there.
4. **Ingest adapter.** Create `crates/blirp/src/ingest/<agent>.rs` implementing `Adapter` (`id`, `roots`, `scan`, `ingest`) and add it to `adapters()` in `ingest/mod.rs`; add its environment override to `ENV_VARS` in `ingest/mod.rs` if it has one. Reuse the helpers: `jsonl.rs` (incremental line reading with file identity and head hash), `text.rs` (tool call previews, truncation), `emit.rs` (building events), `pricing.rs` (price table for cost estimates). Rules: never panic on unknown lines (log once per source and skip), redaction happens in the sink, sequence numbers must be deterministic and increasing (`line_index * 1024 + n` for line-oriented sources), subagents set `parent_session_id`.
5. **Fixtures and tests.** Add synthetic transcripts to `crates/blirp/tests/fixtures/<agent>/` (a base file and an `append` file to test incremental reads) and a test in `crates/blirp/tests/ingest.rs` asserting the normalized sessions and events.
6. **Memory at launch.** In `crates/blirp/src/memory/launch.rs`, set `inject_mode` and add a branch to `prepare` that returns extra argv/env and writes any generated files into the launch directory. Never edit the user's own config; when the agent reads a settings file, copy and extend it. Add a unit test.
7. **Hooks (if the agent has shell hooks).** Map its event names in `crates/blirp/src/hooks/mod.rs` (normalization and the output format it expects on stdout) and, for global install, add it to `SUPPORTED` and `apply`/`status` in `hooks/install.rs` with tests that prove foreign entries are preserved and uninstall is exact.
8. **Docs.** Update the tables in `docs/ARCHITECTURE.md` (§8 ingest, §9 injection and global integration), [agents.md](agents.md) and the README agent table, and mark the integration "verified" only after checking it against a real install.

`blirp doctor` picks up the adapter automatically (root found, sources, last ingest).

## Release process

1. Bump `version` in `[workspace.package]` of the root `Cargo.toml` (app, CLI and installers take it from there), run `cargo check` to update `Cargo.lock`, commit `chore(release): vX.Y.Z`.
2. Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. `release.yml` creates a **draft** GitHub release with: standalone archives for Linux x64/arm64, macOS arm64/x64 and Windows x64 (each with a `.sha256`), desktop bundles (NSIS + MSI, DMG + app, AppImage + deb + rpm), updater signatures and `latest.json`, `SHA256SUMS.txt`, and rendered Homebrew/winget manifests.
4. Check the draft (install on at least one OS), then publish it. The in-app updater only sees published releases.
5. Optional: copy `homebrew-formula-blirp.rb` / `homebrew-cask-blirp.rb` into a tap (`Formula/blirp.rb`, `Casks/blirp.rb`) and open a `microsoft/winget-pkgs` PR with the three `winget-*.yaml` files (`manifests/b/blirp/blirp/X.Y.Z/`, without the `winget-` prefix).

### Release secrets

Repository **Settings > Secrets and variables > Actions**:

| Secret | Required | Purpose |
|---|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | yes | updater private key (minisign; `pnpm -C app tauri signer generate`). Its public key is `plugins.updater.pubkey` in `app/src-tauri/tauri.conf.json`. |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | yes | its password |
| `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY` | for signed macOS builds | base64 "Developer ID Application" `.p12`, its password, e.g. `Developer ID Application: Name (TEAMID)` |
| `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` | for notarization | Apple ID, app-specific password, team id |
| `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_SIGNING_ENDPOINT`, `AZURE_SIGNING_ACCOUNT`, `AZURE_SIGNING_PROFILE` | for Windows signing via Azure Artifact Signing | service principal with the "Artifact Signing Certificate Profile Signer" role; endpoint (e.g. `https://eus.codesigning.azure.net`), account and certificate profile |
| `WINDOWS_CERTIFICATE`, `WINDOWS_CERTIFICATE_PASSWORD` | alternative Windows signing | base64 `.pfx` and password, used when the Azure secrets are absent |

Signing steps are skipped with a warning when their secrets are missing. The updater key is mandatory: shipped apps accept only updates signed with the key whose public half they contain. Never rotate it casually; a new key needs a release signed with the old key that ships the new public key.
