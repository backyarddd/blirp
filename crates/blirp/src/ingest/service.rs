//! Background ingest: filesystem watcher (debounced 500 ms), startup scan,
//! 5-minute rescan (which also picks up roots created after start), stale
//! status sweep and notification flushing. Passes run on the blocking pool,
//! one at a time; changes arriving meanwhile are merged into the next pass.
//! Hooks hand over transcript paths through [`IngestService::trigger`]; the
//! hinted files join the next pass after [`HINT_DELAY`].

use super::engine::{Engine, Work};
use crate::memory::IngestTrigger;
use notify_debouncer_full::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

const DEBOUNCE: Duration = Duration::from_millis(500);
const RESCAN: Duration = Duration::from_secs(300);
const TICK: Duration = Duration::from_secs(1);
/// Stale-status sweep period, in ticks.
const SWEEP_TICKS: u32 = 30;
const STOP_GRACE: Duration = Duration::from_secs(5);
/// Hook hints arriving within this window share one pass (a turn fires
/// several hooks, and the agent may still be appending to the transcript).
pub const HINT_DELAY: Duration = Duration::from_millis(250);

type Watcher = Debouncer<RecommendedWatcher, RecommendedCache>;

pub struct IngestService {
    engine: Arc<Engine>,
    stop_tx: watch::Sender<bool>,
    hint_tx: mpsc::UnboundedSender<PathBuf>,
    task: JoinHandle<()>,
}

/// [`IngestTrigger`] feeding hook-reported transcript paths to the service.
struct HintTrigger(mpsc::UnboundedSender<PathBuf>);

impl IngestTrigger for HintTrigger {
    fn transcript_hint(&self, _agent: &str, _session_id: &str, transcript_path: &Path) {
        // Never blocks (hooks answer within 1.5 s); the receiver is gone only
        // after shutdown, when there is nothing left to ingest into.
        let _ = self.0.send(transcript_path.to_path_buf());
    }
}

impl IngestService {
    /// Start watching and run the startup scan. Must be called inside a
    /// tokio runtime.
    pub fn start(engine: Arc<Engine>) -> IngestService {
        let (stop_tx, stop_rx) = watch::channel(false);
        let (hint_tx, hint_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(engine.clone(), stop_rx, hint_rx));
        IngestService {
            engine,
            stop_tx,
            hint_tx,
            task,
        }
    }

    /// Trigger to install with `AppState::set_ingest_trigger`.
    pub fn trigger(&self) -> Arc<dyn IngestTrigger> {
        Arc::new(HintTrigger(self.hint_tx.clone()))
    }

    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// Stop watching and wait (bounded) for a running pass to notice.
    pub async fn shutdown(self) {
        self.engine.request_stop();
        let _ = self.stop_tx.send(true);
        match tokio::time::timeout(STOP_GRACE, self.task).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = %e, "ingest task failed"),
            Err(_) => tracing::warn!("ingest pass did not stop in time"),
        }
    }
}

fn start_watcher(tx: mpsc::UnboundedSender<Vec<PathBuf>>) -> Option<Watcher> {
    let res = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| match res {
        Ok(events) => {
            let paths: Vec<PathBuf> = events.into_iter().flat_map(|e| e.event.paths).collect();
            if !paths.is_empty() {
                // The receiver only goes away at shutdown.
                let _ = tx.send(paths);
            }
        }
        Err(errors) => {
            for e in errors {
                tracing::warn!(error = %e, "transcript watcher error");
            }
        }
    });
    match res {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!(error = %e, "cannot start transcript watcher; relying on periodic rescans");
            None
        }
    }
}

/// Watch roots that exist and are not watched yet.
fn refresh_watches(engine: &Engine, watcher: &mut Option<Watcher>, watched: &mut HashSet<PathBuf>) {
    let Some(w) = watcher else { return };
    for root in engine.adapters().iter().flat_map(|a| a.roots()) {
        if watched.contains(&root) || !root.is_dir() {
            continue;
        }
        match w.watch(&root, RecursiveMode::Recursive) {
            Ok(()) => {
                tracing::debug!(root = %root.display(), "watching transcripts");
                watched.insert(root);
            }
            Err(e) => {
                tracing::warn!(root = %root.display(), error = %e, "cannot watch transcript root")
            }
        }
    }
    // A removed root is dropped so it is re-watched if it comes back.
    watched.retain(|r| {
        let keep = r.is_dir();
        if !keep {
            let _ = w.unwatch(r);
        }
        keep
    });
}

/// Changed paths grouped by owning adapter; paths outside every root are dropped.
fn work_for(engine: &Engine, paths: impl IntoIterator<Item = PathBuf>) -> Work {
    let mut w = Work::default();
    for p in paths {
        if let Some(ix) = engine.adapter_for(&p) {
            w.paths.entry(ix).or_default().insert(p);
        }
    }
    w
}

async fn run(
    engine: Arc<Engine>,
    mut stop_rx: watch::Receiver<bool>,
    mut hint_rx: mpsc::UnboundedReceiver<PathBuf>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
    let e = engine.clone();
    let mut w = start_watcher(tx);
    let (mut watcher, mut watched) = match tokio::task::spawn_blocking(move || {
        e.repair_home_filed();
        let mut set = HashSet::new();
        refresh_watches(&e, &mut w, &mut set);
        (w, set)
    })
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "setting up transcript watches failed");
            (None, HashSet::new())
        }
    };

    let n = engine.adapters().len();
    let mut pending = Work::all(n);
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<()>();
    let mut running = false;
    let mut rescan = tokio::time::interval_at(tokio::time::Instant::now() + RESCAN, RESCAN);
    let mut tick = tokio::time::interval(TICK);
    let mut ticks = 0u32;
    let mut hinted: Vec<PathBuf> = Vec::new();
    // Set by the first hint of a burst; later ones join it, so a steady
    // stream of hooks cannot postpone ingest.
    let mut hint_due: Option<tokio::time::Instant> = None;

    loop {
        if !running && !pending.is_empty() {
            running = true;
            let work = std::mem::take(&mut pending);
            let e = engine.clone();
            let done = done_tx.clone();
            tokio::task::spawn_blocking(move || {
                let started = std::time::Instant::now();
                let stats = e.run(&work);
                let ingested: usize = stats.values().map(|s| s.ingested).sum();
                let ms = started.elapsed().as_millis() as u64;
                // Full scans (start, 5 min rescan) are news; passes for
                // changed files run about once a second while an agent works.
                if ingested > 0 && !work.full.is_empty() {
                    tracing::info!(sources = ingested, ms, "ingested transcripts");
                } else if ingested > 0 {
                    tracing::debug!(sources = ingested, ms, "ingested transcripts");
                }
                let _ = done.send(());
            });
        }
        tokio::select! {
            _ = stop_rx.changed() => break,
            Some(paths) = rx.recv() => pending.merge(work_for(&engine, paths)),
            Some(path) = hint_rx.recv() => {
                hinted.push(path);
                hint_due.get_or_insert_with(|| tokio::time::Instant::now() + HINT_DELAY);
            }
            () = tokio::time::sleep_until(hint_due.unwrap_or_else(tokio::time::Instant::now)),
                if hint_due.is_some() =>
            {
                hint_due = None;
                pending.merge(work_for(&engine, std::mem::take(&mut hinted)));
            }
            Some(()) = done_rx.recv() => running = false,
            _ = rescan.tick() => {
                let e = engine.clone();
                let mut w = watcher.take();
                let mut set = std::mem::take(&mut watched);
                match tokio::task::spawn_blocking(move || {
                    refresh_watches(&e, &mut w, &mut set);
                    (w, set)
                }).await {
                    Ok((w, set)) => {
                        watcher = w;
                        watched = set;
                    }
                    Err(e) => tracing::error!(error = %e, "refreshing transcript watches failed"),
                }
                pending.merge(Work::all(n));
            }
            _ = tick.tick() => {
                engine.notifier().flush();
                ticks += 1;
                if ticks.is_multiple_of(SWEEP_TICKS) {
                    let e = engine.clone();
                    tokio::task::spawn_blocking(move || e.sweep_stale());
                }
            }
        }
    }
    drop(watcher);
    if running {
        let _ = done_rx.recv().await;
    }
}
