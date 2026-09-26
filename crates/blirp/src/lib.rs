//! blirp binary crate: daemon (HTTP/WS API), PTY supervisor, agents, CLI.
//! Exposed as a library so integration tests can run the daemon in-process.

pub mod agents;
pub mod api;
pub mod cli;
pub mod daemon;
pub mod proc_tree;
pub mod pty;
pub mod sessions;
pub mod state;
pub mod static_files;
