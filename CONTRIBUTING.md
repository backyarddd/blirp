# Contributing to blirp

Thanks for helping. Start with [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): it is the contract for how blirp works, and a change that diverges from it updates it in the same pull request.

## Setup

- Rust 1.89 or newer (CI pins 1.94.0), with `rustfmt` and `clippy`.
- Node.js 22 and pnpm 12.
- Desktop app only: the [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/). On Debian/Ubuntu:
  `sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev build-essential file`.
  Because `app/src-tauri` is a workspace member, `cargo clippy --workspace` and `cargo test --workspace` need these on Linux too.

```sh
pnpm -C web install && pnpm -C web build   # the UI embedded into the binary (debug builds read web/dist from disk)
cargo run -p blirp -- daemon               # daemon on 127.0.0.1:47770, data in ~/.blirp
BLIRP_HOME=/tmp/blirp-dev cargo run -p blirp -- daemon   # throwaway data dir
pnpm -C web dev                            # Vite dev server for UI work
pnpm -C app install && pnpm -C app tauri dev   # desktop shell; builds and uses target/debug/blirp
```

## Before you push

These are exactly what CI runs (on Windows, macOS and Linux):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm -C web check && pnpm -C web test && pnpm -C web build
```

If you change a type in `crates/blirp-core/src/model.rs`, regenerate the TypeScript bindings with `cargo test -p blirp-core export_bindings` and commit `web/src/lib/api/types.gen.ts`; CI fails when it is stale.

Rules that reviews enforce (see `CLAUDE.md` and ARCHITECTURE §16):

- No `unwrap`/`expect` outside tests without a comment proving it cannot fail; no `unsafe` without a `SAFETY:` comment and a platform reason; no `any` in TypeScript.
- Errors are handled or logged with context, never silently dropped. Logs never contain transcript text or secrets.
- Never edit a user's agent config or project files except through the explicit, reversible `blirp hooks install` flow. Hooks always exit 0 within 2 s.
- Writes to replicated tables go through `Store::apply`. Redact before storing, syncing or summarizing.
- Tests for every adapter, redaction rule, migration and protocol change.

## Commits and pull requests

- [Conventional commits](https://www.conventionalcommits.org/): `feat(daemon): ...`, `fix(web): ...`, `docs: ...`, `ci: ...`. One logical change per commit.
- Keep pull requests focused; describe what changed, why, and how you verified it (commands and results).
- No generated-by or co-author lines for tools.

## Repository layout

| Path | What |
|---|---|
| `crates/blirp-core` | model types, config, paths, SQLite store and migrations, redaction |
| `crates/blirp-sync` | pairing, replication, remote proxy (iroh) |
| `crates/blirp` | the `blirp` binary: daemon, PTY supervisor, ingest, memory, MCP, hooks, CLI, service install |
| `web/` | Svelte 5 SPA, built to `web/dist` and embedded into `blirp` |
| `app/` | Tauri 2 desktop shell; `app/src` is only the loading/error page |
| `scripts/` | `build-sidecar.{ps1,sh}` (stage `blirp` as the Tauri sidecar), `render-packaging.sh` |
| `packaging/` | Homebrew formula and cask, winget manifests (templates filled at release) |
| `.github/workflows` | `ci.yml` (every push/PR), `release.yml` (tags `v*`) |

## Desktop app notes

- The window shows the daemon's UI at `http://127.0.0.1:<port>`; that origin gets **no** Tauri IPC. Only the bundled loading page (`app/src`) may call the three app commands, as declared in `app/src-tauri/capabilities/main.json`. Keep it that way; if the UI ever needs a native capability, add a narrowly scoped command and review it as a security change.
- `tauri dev` and all non-release cargo builds run `target/<profile>/blirp` next to the app binary (see `app/src-tauri/build.rs`). Release builds require the sidecar staged by `scripts/build-sidecar.{ps1,sh}` at `app/src-tauri/binaries/blirp-<target-triple>[.exe]`, plus on Windows `binaries/conpty/conpty.dll` and `binaries/conpty/x64/OpenConsole.exe`.
- A local `pnpm -C app tauri build` signs updater artifacts and needs `TAURI_SIGNING_PRIVATE_KEY`; without the key add `--config '{"bundle":{"createUpdaterArtifacts":false}}'`.
- ConPTY is pinned in `scripts/build-sidecar.ps1` (NuGet `Microsoft.Windows.Console.ConPTY`, version + SHA-256). Bump both together.

## Releasing (maintainers)

1. Bump `version` in `[workspace.package]` of the root `Cargo.toml` (the app, CLI and installers all take it from there), update `Cargo.lock` (`cargo check`), commit `chore(release): vX.Y.Z`.
2. Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. `release.yml` creates a **draft** release with: standalone archives for Linux x64/arm64, macOS arm64/x64 and Windows x64 (each with a `.sha256`), desktop bundles (NSIS + MSI, DMG + app, AppImage + deb + rpm), updater signatures and `latest.json`, `SHA256SUMS.txt`, and rendered Homebrew/winget manifests.
4. Check the draft (install on at least one OS), then publish it. The in-app updater only sees published releases.
5. Copy the rendered `homebrew-formula-blirp.rb` / `homebrew-cask-blirp.rb` into the tap (`Formula/blirp.rb`, `Casks/blirp.rb`) and open a winget-pkgs PR with the three `winget-*.yaml` files (`manifests/b/blirp/blirp/X.Y.Z/`, dropping the `winget-` prefix).

### Release secrets

Set these in **Settings > Secrets and variables > Actions**:

| Secret | Required | Purpose |
|---|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | yes | Contents of the updater private key (minisign, from `pnpm -C app tauri signer generate`). Its public key is `plugins.updater.pubkey` in `app/src-tauri/tauri.conf.json`. |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | yes | Password of that key. |
| `APPLE_CERTIFICATE` | for signed macOS builds | base64 of a "Developer ID Application" `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | with the above | `.p12` password |
| `APPLE_SIGNING_IDENTITY` | with the above | e.g. `Developer ID Application: Name (TEAMID)` |
| `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` | for notarization | Apple ID, an app-specific password, team id |
| `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` | for Windows signing via Azure Artifact Signing | service principal with the "Artifact Signing Certificate Profile Signer" role |
| `AZURE_SIGNING_ENDPOINT`, `AZURE_SIGNING_ACCOUNT`, `AZURE_SIGNING_PROFILE` | with the above | e.g. `https://eus.codesigning.azure.net`, account name, certificate profile name |
| `WINDOWS_CERTIFICATE`, `WINDOWS_CERTIFICATE_PASSWORD` | alternative Windows signing | base64 `.pfx` code-signing certificate and its password (used when the Azure secrets are absent) |

Signing steps are skipped with a warning when their secrets are missing; the updater key is mandatory because shipped apps only accept updates signed with it. **Never rotate the updater key casually:** installed apps trust only the public key they shipped with, so a new key requires a release signed with the old key that ships the new public key.
