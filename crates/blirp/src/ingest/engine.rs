//! Ingest passes: scan adapters, ingest changed sources, keep per-adapter
//! bookkeeping. Everything here is blocking; the service runs it on the
//! blocking pool.

use super::sink::{ACTIVE_MS, Emit, Notifier, StoreSink};
use super::{Adapter, Cursor, IngestEnv, Result, Source};
use blirp_core::model::{Machine, MachineRole, SessionStatus};
use blirp_core::store::Store;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

pub(crate) use blirp_core::paths::path_key;

/// What a pass should look at.
#[derive(Debug, Clone, Default)]
pub struct Work {
    /// Adapters to scan completely.
    pub full: HashSet<usize>,
    /// Changed paths per adapter (from the watcher).
    pub paths: HashMap<usize, HashSet<PathBuf>>,
}

impl Work {
    pub fn all(n: usize) -> Work {
        Work {
            full: (0..n).collect(),
            paths: HashMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.full.is_empty() && self.paths.is_empty()
    }

    pub fn merge(&mut self, o: Work) {
        self.full.extend(o.full);
        for (k, v) in o.paths {
            self.paths.entry(k).or_default().extend(v);
        }
        for k in &self.full {
            self.paths.remove(k);
        }
    }
}

/// Result of one adapter pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PassStats {
    pub sources: usize,
    pub ingested: usize,
    pub failed: usize,
}

pub struct Engine {
    pub(crate) store: Arc<Store>,
    pub(crate) machine: Machine,
    pub(crate) env: IngestEnv,
    adapters: Vec<Box<dyn Adapter>>,
    pub(crate) notifier: Notifier,
    stop: AtomicBool,
    warned: Mutex<HashSet<String>>,
    /// Sources seen by each adapter's last full scan.
    known: Mutex<HashMap<usize, Vec<Source>>>,
    /// `retire_non_projects` ran (or had run before).
    retired: AtomicBool,
    /// `remove_headless` ran (or had run before).
    headless_removed: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Guarded values are plain collections, valid after a panic elsewhere.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Engine {
    pub fn new(store: Arc<Store>, machine: Machine, env: IngestEnv, emit: Emit) -> Engine {
        let adapters = super::adapters(&env);
        Self::with_adapters(store, machine, env, adapters, emit)
    }

    pub fn with_adapters(
        store: Arc<Store>,
        machine: Machine,
        env: IngestEnv,
        adapters: Vec<Box<dyn Adapter>>,
        emit: Emit,
    ) -> Engine {
        Engine {
            store,
            machine,
            env,
            adapters,
            notifier: Notifier::new(emit),
            stop: AtomicBool::new(false),
            warned: Mutex::new(HashSet::new()),
            known: Mutex::new(HashMap::new()),
            retired: AtomicBool::new(false),
            headless_removed: AtomicBool::new(false),
        }
    }

    pub fn adapters(&self) -> &[Box<dyn Adapter>] {
        &self.adapters
    }

    pub fn notifier(&self) -> &Notifier {
        &self.notifier
    }

    /// Ask running passes to stop at the next source or event.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub(crate) fn warn_once(&self, adapter: &str, source: &str, what: &str) {
        if lock(&self.warned).insert(format!("{adapter}\n{source}\n{what}")) {
            tracing::warn!(adapter, source, "{what}");
        }
    }

    /// Adapter index owning `path` (longest matching root). Paths reported
    /// by hooks may differ from the roots in case or form on Windows.
    pub fn adapter_for(&self, path: &Path) -> Option<usize> {
        let key = path_key(path);
        self.adapters
            .iter()
            .enumerate()
            .flat_map(|(i, a)| a.roots().into_iter().map(move |r| (i, r)))
            .filter(|(_, r)| key.starts_with(path_key(r)))
            .max_by_key(|(_, r)| r.as_os_str().len())
            .map(|(i, _)| i)
    }

    /// Run `work`; adapters run in parallel, sources within one sequentially.
    pub fn run(&self, work: &Work) -> HashMap<&'static str, PassStats> {
        self.remove_headless();
        self.repair_codex_subagents();
        self.retire_non_projects();
        let mut ids: Vec<usize> = work.full.iter().chain(work.paths.keys()).copied().collect();
        ids.sort_unstable();
        ids.dedup();
        std::thread::scope(|scope| {
            let handles: Vec<_> = ids
                .into_iter()
                .filter_map(|i| self.adapters.get(i).map(|a| (i, a)))
                .map(|(i, a)| {
                    let changed = if work.full.contains(&i) {
                        None
                    } else {
                        work.paths.get(&i)
                    };
                    (a.id(), scope.spawn(move || self.run_adapter(i, changed)))
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|(id, h)| match h.join() {
                    Ok(stats) => Some((id, stats)),
                    Err(_) => {
                        tracing::error!(adapter = id, "ingest pass panicked");
                        None
                    }
                })
                .collect()
        })
    }

    /// Sources to look at: the known ones matching `changed`, or a full scan
    /// when there is no change list or a change is not a known source.
    fn sources_for(&self, ix: usize, changed: Option<&HashSet<PathBuf>>) -> Result<Vec<Source>> {
        let adapter = &self.adapters[ix];
        if let Some(changed) = changed
            && let Some(known) = lock(&self.known).get(&ix)
        {
            // File sources are keyed by path (item None); database sources
            // always rescan because one file holds many of them. A changed
            // path maps to the known source's own spelling, so a hook's
            // differently cased path never becomes a second source.
            let known_file = |c: &PathBuf| {
                let key = path_key(c);
                known
                    .iter()
                    .find(|s| s.item.is_none() && path_key(&s.path) == key)
                    .map(|s| s.path.clone())
            };
            let paths: Option<Vec<PathBuf>> = changed.iter().map(known_file).collect();
            if let Some(paths) = paths {
                return Ok(paths.iter().filter_map(|p| Source::file(p)).collect());
            }
        }
        let mut sources = adapter.scan(&self.store)?;
        sources.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms).then_with(|| a.key.cmp(&b.key)));
        lock(&self.known).insert(ix, sources.clone());
        Ok(sources)
    }

    pub fn run_adapter(&self, ix: usize, changed: Option<&HashSet<PathBuf>>) -> PassStats {
        let adapter = &self.adapters[ix];
        let id = adapter.id();
        let mut stats = PassStats::default();
        let sources = match self.sources_for(ix, changed) {
            Ok(s) => s,
            Err(e) => {
                self.warn_once(id, "(scan)", &format!("scan failed: {e:#}"));
                stats.failed += 1;
                return stats;
            }
        };
        stats.sources = sources.len();
        for src in &sources {
            if self.stopping() {
                break;
            }
            match self.ingest_source(adapter.as_ref(), src) {
                Ok(true) => stats.ingested += 1,
                Ok(false) => {}
                Err(e) => {
                    stats.failed += 1;
                    if !self.stopping() {
                        self.warn_once(id, &src.key, &format!("ingest failed: {e:#}"));
                    }
                }
            }
        }
        if changed.is_none() {
            let now = blirp_core::now_ms();
            let values = [
                (format!("ingest.{id}.last_at"), Some(json!(now))),
                (format!("ingest.{id}.sources"), Some(json!(stats.sources))),
            ]
            .into_iter()
            .collect();
            if let Err(e) = self.store.set_settings(&values) {
                tracing::warn!(adapter = id, error = %e, "recording ingest status failed");
            }
        } else if stats.ingested > 0
            && let Err(e) = self.store.set_setting(
                &format!("ingest.{id}.last_at"),
                &json!(blirp_core::now_ms()),
            )
        {
            tracing::warn!(adapter = id, error = %e, "recording ingest status failed");
        }
        stats
    }

    /// Ingest one source if it changed since its cursor. True when read.
    pub fn ingest_source(&self, adapter: &dyn Adapter, src: &Source) -> Result<bool> {
        let id = adapter.id();
        let cursor: Option<Cursor> = self
            .store
            .get_cursor(id, &src.key)?
            .and_then(|v| serde_json::from_value(v).ok());
        if cursor
            .as_ref()
            .is_some_and(|c| !c.fp.is_empty() && c.fp == src.fingerprint)
        {
            return Ok(false);
        }
        // None: first read.
        let was_headless = cursor.as_ref().map(Cursor::headless);
        // A read that starts the source over next time resets its state.
        let asid_of = |c: &Cursor| {
            c.state
                .get("asid")
                .and_then(|a| a.as_str())
                .map(str::to_string)
        };
        let prev_asid = cursor.as_ref().and_then(asid_of);
        let mut sink = StoreSink::new(self, id, &src.key, src.mtime_ms);
        let mut next = adapter.ingest(src, cursor, &mut sink)?;
        next.fp = if next.retry {
            String::new()
        } else {
            src.fingerprint.clone()
        };
        sink.finish(&next)?;
        if was_headless != Some(false) && !next.headless() {
            match id {
                "claude" => self.reread_skipped_subagents(&src.path),
                "codex" => {
                    if let Some(asid) = asid_of(&next).or(prev_asid) {
                        self.reread_skipped_codex_subagents(&asid);
                    }
                }
                _ => {}
            }
        }
        Ok(true)
    }

    /// [`Engine::reread_skipped_subagents`] for codex: subagent rollouts
    /// skipped with their parent `parent` (a scripted run then, or not read
    /// yet) are read again from the start now that it is a session.
    fn reread_skipped_codex_subagents(&self, parent: &str) {
        let subs = match self.store.cursors_with_parent("codex", parent) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "listing a codex session's subagents failed");
                return;
            }
        };
        for key in subs {
            if !super::known_headless(&self.store, "codex", &key) {
                continue;
            }
            if let Err(e) = self.store.delete_cursor("codex", &key) {
                tracing::warn!(error = %e, "resetting a subagent rollout's cursor failed");
            }
        }
    }

    /// A claude session that looked scripted (or was not read yet) when its
    /// subagents were read is a session now: subagent transcripts skipped
    /// because of it are read again from the start on the next full pass
    /// (one written only during the first prompt never changes again).
    fn reread_skipped_subagents(&self, session_transcript: &Path) {
        for sub in super::claude::subagent_transcripts(session_transcript) {
            let key = sub.to_string_lossy();
            if !super::known_headless(&self.store, "claude", &key) {
                continue;
            }
            if let Err(e) = self.store.delete_cursor("claude", &key) {
                tracing::warn!(error = %e, "resetting a subagent transcript's cursor failed");
            }
        }
    }

    /// One-time repair: before ingest waited for a transcript's cwd, a long
    /// transcript could be filed under the home folder (Home project, no
    /// launch link) by a mid-read flush. Their cursors are reset once so
    /// the next pass reads them from the start and the sink re-files them.
    pub fn repair_home_filed(&self) {
        const KEY: &str = "ingest.repair.home_filed";
        match self.store.get_setting(KEY) {
            Ok(None) => {}
            Ok(Some(_)) => return,
            Err(e) => {
                tracing::warn!(error = %e, "reading ingest repair state failed");
                return;
            }
        }
        // Exactly the cwd the sink stored for rows without one.
        let home = dunce::canonicalize(&self.env.home)
            .unwrap_or_else(|_| self.env.home.clone())
            .display()
            .to_string();
        let res = self
            .store
            .reset_cursors_filed_under(&self.machine.id, &home)
            .and_then(|n| {
                self.store.set_setting(KEY, &json!(blirp_core::now_ms()))?;
                Ok(n)
            });
        match res {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                sources = n,
                "re-reading transcripts filed under the home folder"
            ),
            Err(e) => tracing::warn!(error = %e, "ingest repair failed"),
        }
    }

    /// This machine's role, or `None` (logged) when it cannot be read.
    fn role(&self) -> Option<MachineRole> {
        match self.store.get_machine(&self.machine.id) {
            Ok(m) => Some(m.map_or(self.machine.role, |m| m.role)),
            Err(e) => {
                tracing::warn!(error = %e, "reading this machine's role failed");
                None
            }
        }
    }

    /// One-time cleanup after upgrading to the rule that scripted runs are
    /// no sessions (§8): ingested claude and codex sessions whose whole
    /// transcript is a scripted run are deleted like a user delete, with the
    /// distiller records made from them and the projects ingest created only
    /// for them (`Store::remove_headless_sessions`). A row whose transcript
    /// is gone or unreadable is kept. Runs at the start of an ingest pass,
    /// and on a node only once a pull reached the hub's head, as
    /// [`Engine::retire_non_projects`].
    pub fn remove_headless(&self) {
        const KEY: &str = "ingest.cleanup.headless";
        if self.headless_removed.load(Ordering::Relaxed) {
            return;
        }
        let Some(role) = self.role() else { return };
        if role == MachineRole::Node && !self.store.pulled_to_head() {
            return;
        }
        match self.store.get_setting(KEY) {
            Ok(None) => {}
            Ok(Some(_)) => {
                self.headless_removed.store(true, Ordering::Relaxed);
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "reading headless cleanup state failed");
                return;
            }
        }
        let candidates = match self.store.headless_candidates(&self.machine.id) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "listing sessions for the headless cleanup failed");
                return;
            }
        };
        let codex = super::codex::Codex::new(&self.env);
        let mut ids = Vec::new();
        for s in candidates {
            if self.stopping() {
                return;
            }
            let Some(path) = &s.transcript_path else {
                continue;
            };
            // A codex subagent is judged by its parent's rollout: one of a
            // scripted run was stored without a parent.
            let scripted = if s.agent == "codex" {
                codex.scripted(Path::new(path))
            } else {
                super::transcript_is_headless(&s.agent, Path::new(path))
            };
            match scripted {
                Ok(true) => ids.push(s.id),
                Ok(false) => {}
                Err(e) => {
                    tracing::debug!(session = %s.id, error = %e, "transcript not readable; kept")
                }
            }
        }
        let res = self
            .store
            .remove_headless_sessions(&self.machine.id, &ids)
            .and_then(|out| {
                self.store.set_setting(KEY, &json!(blirp_core::now_ms()))?;
                Ok(out)
            });
        match res {
            Ok(out) => {
                self.headless_removed.store(true, Ordering::Relaxed);
                tracing::info!(
                    sessions = out.sessions.len(),
                    records = out.records,
                    records_kept = out.records_kept,
                    projects = out.projects.len(),
                    "removed ingested scripted runs (claude -p, Agent SDK, codex exec)"
                );
                for id in out.sessions {
                    self.notifier.deleted(id);
                }
                for p in &out.projects {
                    self.notifier.project(p);
                }
            }
            Err(e) => tracing::warn!(error = %e, "removing ingested scripted runs failed"),
        }
    }

    /// One-time repair of Codex subagent sessions ingested before blirp
    /// knew them ([`super::codex::repair_subagents`]).
    /// Runs at the start of an ingest pass, and on a node only once a pull
    /// reached the hub's head (it writes replicated changes of rows the hub
    /// may still be sending), as [`Engine::remove_headless`].
    pub fn repair_codex_subagents(&self) {
        const KEY: &str = "ingest.repair.codex_subagents";
        let Some(role) = self.role() else { return };
        if role == MachineRole::Node && !self.store.pulled_to_head() {
            return;
        }
        match self.store.get_setting(KEY) {
            Ok(None) => {}
            Ok(Some(_)) => return,
            Err(e) => {
                tracing::warn!(error = %e, "reading ingest repair state failed");
                return;
            }
        }
        let home = self
            .env
            .var_path("CODEX_HOME")
            .unwrap_or_else(|| self.env.home.join(".codex"));
        let res =
            super::codex::repair_subagents(&self.store, &self.machine.id, &home).and_then(|n| {
                self.store.set_setting(KEY, &json!(blirp_core::now_ms()))?;
                Ok(n)
            });
        match res {
            Ok(n) => {
                tracing::info!(
                    subagents = n.subagents,
                    relinked = n.relinked,
                    forks_truncated = n.forks_truncated,
                    events_dropped = n.events_dropped,
                    forks_unconfirmed = n.forks_unconfirmed,
                    titles_fixed = n.titles_fixed,
                    records_removed = n.records_removed,
                    "repaired codex subagent sessions"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "codex subagent repair failed; retried on the next pass")
            }
        }
    }

    /// One-time cleanup after upgrading to the Chats rules (§5): projects
    /// earlier ingest created for folders that are no actual project
    /// (temp, tool, system or Codex chat folders, folders without git or a
    /// project marker) are merged into Chats when nothing shows the user
    /// made or used them as a project (`Store::retire_non_projects`).
    /// Runs at the start of an ingest pass, so never concurrently with
    /// one; a node waits for a pull to reach the hub's head first, so a
    /// change another machine made to such a project is seen (and makes it
    /// ineligible) before it is removed.
    pub fn retire_non_projects(&self) {
        // Renamed in 0.1.1 when chats left the projects: the pass runs again
        // with the new rules where an earlier build ran it.
        const KEY: &str = "projects.cleanup.chats";
        if self.retired.load(Ordering::Relaxed) {
            return;
        }
        let Some(role) = self.role() else { return };
        if role == MachineRole::Node && !self.store.pulled_to_head() {
            return;
        }
        match self.store.get_setting(KEY) {
            Ok(None) => {}
            Ok(Some(_)) => {
                self.retired.store(true, Ordering::Relaxed);
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "reading project cleanup state failed");
                return;
            }
        }
        let res = self
            .store
            .retire_non_projects(&self.machine.id, &self.machine.name, &self.env.non_projects)
            .and_then(|retired| {
                self.store.set_setting(KEY, &json!(blirp_core::now_ms()))?;
                Ok(retired)
            });
        if res.is_ok() {
            self.retired.store(true, Ordering::Relaxed);
        }
        match res {
            Ok(retired) if retired.is_empty() => {}
            Ok(retired) => {
                tracing::info!(
                    projects = retired.len(),
                    "moved the sessions of projects that are no actual project to Chats"
                );
                for p in &retired {
                    self.notifier.project(&p.id);
                }
                if let Ok(Some(home)) = self.store.home_project_id() {
                    self.notifier.project(&home);
                }
            }
            Err(e) => tracing::warn!(error = %e, "project cleanup failed"),
        }
    }

    /// External sessions still `working` without activity for 2 minutes
    /// become `completed` (no further transcript writes will say so).
    pub fn sweep_stale(&self) {
        let now = blirp_core::now_ms();
        let stale = match self
            .store
            .stale_external_sessions(&self.machine.id, now - ACTIVE_MS)
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "listing stale sessions failed");
                return;
            }
        };
        for s in stale {
            let res = self.store.modify_session(&s.id, |s| {
                if s.status == SessionStatus::Working && now - s.last_activity_at >= ACTIVE_MS {
                    s.status = SessionStatus::Completed;
                    s.ended_at = Some(s.last_activity_at);
                }
            });
            match res {
                Ok(s) => self.notifier.updated(s),
                Err(e) => {
                    tracing::warn!(session = %s.id, error = %e, "completing stale session failed")
                }
            }
        }
    }
}

/// `blirp doctor` line data for one adapter.
#[derive(Debug, Clone)]
pub struct AdapterStatus {
    pub id: &'static str,
    /// False for adapters that only scan project folders (aider).
    pub has_roots: bool,
    pub roots_found: Vec<PathBuf>,
    pub sources: std::result::Result<usize, String>,
    pub last_ingest_at: Option<i64>,
}

/// Roots, source counts and last ingest time per adapter (read-only).
pub fn status(store: &Store, env: &IngestEnv) -> Vec<AdapterStatus> {
    super::adapters(env)
        .iter()
        .map(|a| AdapterStatus {
            id: a.id(),
            has_roots: !a.roots().is_empty(),
            roots_found: a.roots().into_iter().filter(|r| r.exists()).collect(),
            sources: a.scan(store).map(|s| s.len()).map_err(|e| format!("{e:#}")),
            last_ingest_at: store
                .get_setting(&format!("ingest.{}.last_at", a.id()))
                .ok()
                .flatten()
                .and_then(|v| v.as_i64()),
        })
        .collect()
}
