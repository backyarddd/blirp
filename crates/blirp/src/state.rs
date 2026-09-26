//! Shared daemon state.

use crate::pty::Registry;
use blirp_core::config::Config;
use blirp_core::model::{AgentInfo, Machine, ServerEvent};
use blirp_core::paths::Paths;
use blirp_core::store::Store;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Instant;
use tokio::sync::{broadcast, watch};

pub type SharedState = Arc<AppState>;

pub struct AppState {
    pub paths: Paths,
    pub store: Arc<Store>,
    config: RwLock<Config>,
    pub machine: Machine,
    pub token: String,
    pub port: u16,
    pub events: broadcast::Sender<ServerEvent>,
    pub terminals: Registry,
    /// Flips to true when the daemon begins shutting down; long-lived
    /// handlers (WebSockets) exit on it so graceful shutdown can finish.
    pub shutdown: watch::Receiver<bool>,
    pub agents_cache: Mutex<Option<(Instant, Vec<AgentInfo>)>>,
}

impl AppState {
    pub fn new(
        paths: Paths,
        store: Arc<Store>,
        config: Config,
        machine: Machine,
        token: String,
        port: u16,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            paths,
            store,
            config: RwLock::new(config),
            machine,
            token,
            port,
            events,
            terminals: Registry::default(),
            shutdown,
            agents_cache: Mutex::new(None),
        }
    }

    pub fn config(&self) -> Config {
        // A poisoned lock still holds a fully written Config (writes replace it whole).
        self.config
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set_config(&self, c: Config) {
        *self.config.write().unwrap_or_else(PoisonError::into_inner) = c;
    }

    /// Broadcast to `/api/events/ws` subscribers (none connected is fine).
    pub fn emit(&self, e: ServerEvent) {
        let _ = self.events.send(e);
    }
}
