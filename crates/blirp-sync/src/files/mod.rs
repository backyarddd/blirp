//! Project file sync through the hub (docs/project-files.md): blob storage,
//! the hub's file service and the `blirp/files/1` protocol.

pub mod blobs;
pub mod client;
pub mod hub;
pub mod proto;
pub mod server;

pub use client::{Committed, FileHub, LocalHub, RemoteHub, Welcome};
pub use hub::{GcStats, HubError, HubFiles, RootChanged};

#[cfg(test)]
mod tests;
