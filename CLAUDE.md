# blirp

Open-source workspace for CLI coding agents with automatic cross-session memory. Read `docs/ARCHITECTURE.md` before changing anything; it is the contract.

## Commands

- Rust: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`
- Web: `pnpm -C web install`, `pnpm -C web check`, `pnpm -C web test`, `pnpm -C web build`
- Run daemon from source: `cargo run -p blirp -- daemon` (uses `BLIRP_HOME` if set)
- Desktop: `pnpm -C app tauri dev`

## Rules

- Projects are folders; git is optional. Never assume `.git` exists.
- Never edit the user's agent config or project files except through the explicit, reversible `blirp hooks install` flow.
- Hooks always exit 0 within 2 s.
- All writes to replicated tables go through `Store::apply`.
- Redact before storing, syncing or summarizing transcript text.
- No `unwrap`/`expect` in non-test code without a justification comment. No `any` in TypeScript.
- Conventional commits, no AI attribution lines.
