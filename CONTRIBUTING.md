# Contributing to blirp

Thanks for helping. Start with [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): it is the contract for how blirp works, and a change that diverges from it updates it in the same pull request. Setup, running from source, tests, adding an agent and the release process are in [docs/development.md](docs/development.md).

## Before you push

These are exactly what CI runs (on Windows, macOS and Linux):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm -C web check && pnpm -C web test && pnpm -C web build
```

If you change a type in `crates/blirp-core/src/model.rs`, regenerate the TypeScript bindings with `cargo test -p blirp-core export_bindings` and commit `web/src/lib/api/types.gen.ts`; CI fails when it is stale. UI changes should also pass `pnpm -C web e2e` locally.

## Rules reviews enforce

- No `unwrap`/`expect` outside tests without a comment proving it cannot fail; no `unsafe` without a `SAFETY:` comment and a platform reason; no `any` in TypeScript.
- Errors are handled or logged with context, never silently dropped. Logs never contain transcript text or secrets.
- Projects are folders; never assume `.git` exists.
- Never edit a user's agent config or project files except through the explicit, reversible `blirp hooks install` flow. Hooks always exit 0 within 2 s.
- Writes to replicated tables go through `Store::apply`. Redact before storing, syncing or summarizing.
- Tests for every adapter, redaction rule, migration and protocol change.
- User-facing behavior changes update the docs in `docs/` (and the README agent table when an integration changes).

## Commits and pull requests

- [Conventional commits](https://www.conventionalcommits.org/): `feat(daemon): ...`, `fix(web): ...`, `docs: ...`, `ci: ...`. One logical change per commit.
- Keep pull requests focused; describe what changed, why, and how you verified it (commands and results).
- No generated-by or co-author lines for tools.

## Reporting bugs

Include `blirp --version`, your OS, `blirp doctor` output and the relevant lines of `~/.blirp/logs/blirpd.<date>.log` (logs contain no transcript text). For agent integration problems, name the agent and its version. Security issues go through [SECURITY.md](SECURITY.md), not public issues.
