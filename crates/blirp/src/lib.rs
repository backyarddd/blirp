//! blirp binary crate: daemon (HTTP/WS API), PTY supervisor, agents, CLI.
//! Exposed as a library so integration tests can run the daemon in-process.

pub mod agents;
pub mod api;
pub mod cli;
pub mod daemon;
pub mod hooks;
pub mod ingest;
pub mod mcp;
pub mod memory;
pub mod portal;
pub mod pty;
pub mod sessions;
pub mod state;
pub mod static_files;
pub mod sync;
pub mod update;

/// Install ring as the process-wide rustls provider. iroh turns on
/// reqwest's rustls backend without a provider, so every reqwest client
/// needs this first. Idempotent.
pub fn install_crypto_provider() {
    // Err only means a provider is already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
