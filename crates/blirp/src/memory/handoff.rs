//! Fresh summary for a handoff pack (§9 Handoff pack): a session continued in
//! a new one is distilled first when its transcript has moved on since its
//! last summary, so the pack does not carry a summary up to
//! `memory.distill_idle_secs` old (or none at all).

use crate::memory::distill::budget_used;
use crate::state::SharedState;
use blirp_core::config::Summarizer;
use blirp_core::model::{Session, SessionSummary};
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

/// How long a launch with `continue_from` waits for the source's distill.
/// Shorter than the summarizer's own timeout: a launch is interactive, and
/// the pack's last turns carry the latest work either way.
pub const REFRESH_WAIT: Duration = Duration::from_secs(90);

/// Distill `source` before its handoff pack is built, when it has new
/// prompts or replies past its last summary. Returns why the summary was not
/// refreshed (noted in the pack's header), `None` when it is current. Never
/// fails the launch.
///
/// - Only this machine's sessions: another machine's session is distilled by
///   that machine and its summary arrives by sync, so that one is used.
/// - The run is a queued job like any other: one run at a time, counted
///   against the daily budget. A job of this session that is already queued
///   or running is waited for instead of starting a second one.
/// - Nothing is started when the summarizer is off, paused after failing, or
///   the budget is used up; at most `wait` is spent waiting.
pub async fn refresh_summary(
    state: &SharedState,
    source: &Session,
    wait: Duration,
) -> Option<String> {
    let store = state.store.clone();
    let sid = source.id.clone();
    // Only a new prompt or reply makes the summary stale, as for the
    // distill loop: hook rows, injected context and compaction markers past
    // the last summary never start a run.
    let fresh = match tokio::task::spawn_blocking(move || store.has_new_content(&sid)).await {
        Ok(Ok(new)) => !new,
        Ok(Err(e)) => {
            tracing::warn!(session = %source.id, error = %e, "reading the handoff source failed");
            return Some("its transcript could not be read".into());
        }
        Err(e) => {
            tracing::error!(error = %e, "handoff source lookup task failed");
            return Some("its transcript could not be read".into());
        }
    };
    if fresh {
        return None;
    }
    if source.machine_id != state.machine.id {
        return Some(
            "it runs on another machine, which distills it; this is the summary synced from there"
                .into(),
        );
    }
    let cfg = state.config().memory;
    if cfg.summarizer == Summarizer::None {
        return Some("the summarizer is off".into());
    }
    if let Some(why) = blocked(state, cfg.daily_distill_limit).await {
        return Some(why);
    }

    let started = blirp_core::now_ms();
    // Subscribed before queueing, so the end of the job cannot be missed.
    let mut done = state.distiller.subscribe();
    state.distiller.enqueue(&source.id, true);
    let job_done = async {
        loop {
            match done.recv().await {
                Ok(id) if id == source.id => return,
                Ok(_) => {}
                Err(RecvError::Lagged(_)) if state.distiller.is_queued(&source.id) => {}
                Err(_) => return,
            }
        }
    };
    // The worker stops on shutdown; the launch must not hold it up.
    let mut shutdown = state.shutdown.clone();
    tokio::select! {
        r = tokio::time::timeout(wait, job_done) => {
            if r.is_err() {
                return Some(format!(
                    "summarizing took longer than {} s; the new summary shows on the source session when done",
                    wait.as_secs()
                ));
            }
        }
        _ = shutdown.wait_for(|stop| *stop) => return Some("blirp is shutting down".into()),
    }

    let store = state.store.clone();
    let sid = source.id.clone();
    let now = match tokio::task::spawn_blocking(move || store.get_session(&sid)).await {
        Ok(Ok(Some(s))) => s,
        Ok(Ok(None)) => return Some("the session was deleted".into()),
        Ok(Err(e)) => {
            tracing::warn!(session = %source.id, error = %e, "re-reading the handoff source failed");
            return Some("its summary could not be read".into());
        }
        Err(e) => {
            tracing::error!(error = %e, "handoff source lookup task failed");
            return Some("its summary could not be read".into());
        }
    };
    // A job already running when the launch came may stop short of the
    // newest events; it still refreshed the summary.
    if now.distilled_through_seq > source.distilled_through_seq {
        return None;
    }
    let failure = now
        .summary
        .and_then(|v| serde_json::from_value::<SessionSummary>(v).ok())
        .and_then(|s| s.error)
        .filter(|e| e.at >= started);
    if let Some(e) = failure {
        return Some(format!("summarizing failed: {}", e.message));
    }
    Some(
        blocked(state, cfg.daily_distill_limit)
            .await
            .unwrap_or_else(|| "the summarizer did not run".into()),
    )
}

/// Why no distill can run now: the summarizer is paused after failing, or
/// today's budget is used up.
async fn blocked(state: &SharedState, limit: u32) -> Option<String> {
    let now = blirp_core::now_ms();
    let pause = state.distiller.status(now);
    if pause.paused.is_some() && pause.retry_at.is_some_and(|t| t > now) {
        return Some(match pause.reason {
            Some(r) => format!("the summarizer is paused: {r}"),
            None => "the summarizer is paused".into(),
        });
    }
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || budget_used(&store)).await {
        Ok(Ok(used)) if used >= u64::from(limit) => {
            Some("today's distill budget is used up".into())
        }
        Ok(Ok(_)) => None,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "reading the distill budget failed");
            Some("the distill budget could not be read".into())
        }
        Err(e) => {
            tracing::error!(error = %e, "distill budget task failed");
            Some("the distill budget could not be read".into())
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::memory::distill::record_failure;
    use crate::memory::testutil::*;
    use crate::state::AppState;
    use blirp_core::config::Config;
    use blirp_core::model::{EventKind, Machine, MachineRole};
    use blirp_core::paths::Paths;
    use std::sync::Arc;

    /// A temp dir, the state, its shutdown switch and a session `src` of
    /// this machine with two events and no summary.
    struct Fixture {
        _dir: tempfile::TempDir,
        st: SharedState,
        src: Session,
        stop: tokio::sync::watch::Sender<bool>,
    }

    fn state(summarizer: Summarizer, limit: u32) -> Fixture {
        let (dir, store, pid) = project_store("P");
        store.insert_session(&session("src", &pid, 1000)).unwrap();
        event(&store, "src", 1, EventKind::User, "fix the parser");
        event(&store, "src", 2, EventKind::Assistant, "fixed");
        let src = store.get_session("src").unwrap().unwrap();
        let mut config = Config::default();
        config.memory.summarizer = summarizer;
        config.memory.daily_distill_limit = limit;
        let machine = Machine {
            id: "m".into(),
            name: "box".into(),
            os: "windows".into(),
            role: MachineRole::Standalone,
            last_seen: 1,
            revoked: false,
        };
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let st = Arc::new(AppState::new(
            Paths::at(dir.path()),
            Arc::new(store),
            config,
            machine,
            "t".into(),
            0,
            shutdown,
        ));
        Fixture {
            _dir: dir,
            st,
            src,
            stop,
        }
    }

    /// Plays the distill worker for one job of `sid`: waits until it is
    /// queued and awaited, applies `run` (what the worker writes) and
    /// reports the job finished.
    fn worker(st: &SharedState, sid: &str, run: impl FnOnce(&SharedState) + Send + 'static) {
        let (st, sid) = (st.clone(), sid.to_string());
        tokio::spawn(async move {
            while !st.distiller.is_queued(&sid) || st.distiller.subscribers() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            run(&st);
            st.distiller.finished(&sid);
        });
    }

    fn distilled(st: &SharedState) {
        st.store
            .modify_session("src", |s| s.distilled_through_seq = 2)
            .unwrap();
    }

    const WAIT: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn a_distilled_session_and_the_skips_start_nothing() {
        let f = state(Summarizer::Auto, 40);
        distilled(&f.st);
        let mut src = f.st.store.get_session("src").unwrap().unwrap();
        assert_eq!(refresh_summary(&f.st, &src, WAIT).await, None);
        assert!(!f.st.distiller.is_queued("src"));
        // Only a prompt or reply makes the summary stale, as for the
        // distill loop.
        event(&f.st.store, "src", 3, EventKind::System, "compacted");
        assert_eq!(refresh_summary(&f.st, &src, WAIT).await, None);
        assert!(!f.st.distiller.is_queued("src"));

        event(&f.st.store, "src", 4, EventKind::User, "and the lexer");
        src.machine_id = "other".into();
        let why = refresh_summary(&f.st, &src, WAIT).await.unwrap();
        assert!(why.contains("another machine"), "{why}");
        assert!(!f.st.distiller.is_queued("src"));

        let f = state(Summarizer::None, 40);
        assert_eq!(
            refresh_summary(&f.st, &f.src, WAIT).await.as_deref(),
            Some("the summarizer is off")
        );
        let f = state(Summarizer::Auto, 0);
        let why = refresh_summary(&f.st, &f.src, WAIT).await.unwrap();
        assert!(why.contains("budget is used up"), "{why}");
        assert!(!f.st.distiller.is_queued("src"));
    }

    #[tokio::test]
    async fn waits_for_the_distill_and_reports_its_failure() {
        let f = state(Summarizer::Auto, 40);
        worker(&f.st, "src", distilled);
        assert_eq!(refresh_summary(&f.st, &f.src, WAIT).await, None);

        let f = state(Summarizer::Auto, 40);
        worker(&f.st, "src", |st| {
            record_failure(&st.store, "src", "summarizer failed: boom").unwrap();
        });
        let why = refresh_summary(&f.st, &f.src, WAIT).await.unwrap();
        assert_eq!(why, "summarizing failed: summarizer failed: boom");
    }

    #[tokio::test]
    async fn a_queued_job_is_waited_for_not_doubled() {
        let f = state(Summarizer::Auto, 40);
        assert!(f.st.distiller.enqueue("src", false));
        worker(&f.st, "src", distilled);
        assert_eq!(refresh_summary(&f.st, &f.src, WAIT).await, None);
        // The job is over: nothing else was queued behind it.
        assert!(!f.st.distiller.is_queued("src"));
    }

    #[tokio::test]
    async fn gives_up_after_the_wait_or_on_shutdown() {
        let f = state(Summarizer::Auto, 40);
        let why = refresh_summary(&f.st, &f.src, Duration::from_millis(50))
            .await
            .unwrap();
        assert!(why.starts_with("summarizing took longer than"), "{why}");

        let f = state(Summarizer::Auto, 40);
        f.stop.send(true).unwrap();
        let why = refresh_summary(&f.st, &f.src, WAIT).await;
        assert_eq!(why.as_deref(), Some("blirp is shutting down"));
    }
}
