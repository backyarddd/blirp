//! Sleep prevention while sessions run (`sessions.keep_awake`, §6).
//!
//! The status tick reports how many terminals are live; while that count is
//! above zero (and the setting is on) one OS assertion is held, released as
//! soon as the last session ends:
//! - macOS: `caffeinate -i -w <daemon pid>` (it also ends if the daemon dies),
//! - Windows: `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)`
//!   on a dedicated thread (the state belongs to the thread that set it),
//! - Linux: `systemd-inhibit --what=sleep` around a loop that ends with the
//!   daemon, when `systemd-inhibit` exists.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// An acquired assertion; dropping it releases it.
pub trait Assertion: Send {
    /// Still in force (a helper process may have been killed).
    fn alive(&mut self) -> bool;
}

/// Takes the platform's sleep-prevention assertion.
pub trait Inhibit: Send + Sync {
    fn acquire(&self) -> anyhow::Result<Box<dyn Assertion>>;
}

/// After a failed acquire (or an assertion that died), wait this long
/// before trying again, so a missing tool is not respawned every tick.
const RETRY_AFTER: Duration = Duration::from_secs(60);

#[derive(Default)]
struct State {
    held: Option<Box<dyn Assertion>>,
    retry_at: Option<Instant>,
}

pub struct KeepAwake {
    inhibit: Box<dyn Inhibit>,
    state: Mutex<State>,
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self::with(Box::new(Platform))
    }
}

impl KeepAwake {
    pub fn with(inhibit: Box<dyn Inhibit>) -> Self {
        Self {
            inhibit,
            state: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Plain data replaced whole; a poisoned lock is still consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hold the assertion while `live > 0` and `enabled`, release it
    /// otherwise. Called on every status tick; cheap when nothing changes.
    pub fn update(&self, live: usize, enabled: bool, now: Instant) {
        let mut st = self.lock();
        if !(enabled && live > 0) {
            if st.held.take().is_some() {
                tracing::info!("no live sessions; machine may sleep again");
            }
            st.retry_at = None;
            return;
        }
        if let Some(a) = st.held.as_mut() {
            if a.alive() {
                return;
            }
            tracing::warn!("sleep prevention ended unexpectedly; retrying in a minute");
            st.held = None;
            st.retry_at = Some(now + RETRY_AFTER);
            return;
        }
        if st.retry_at.is_some_and(|t| now < t) {
            return;
        }
        match self.inhibit.acquire() {
            Ok(a) => {
                tracing::info!(live, "keeping this machine awake while sessions run");
                st.held = Some(a);
                st.retry_at = None;
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "cannot keep this machine awake");
                st.retry_at = Some(now + RETRY_AFTER);
            }
        }
    }

    /// Whether the assertion is held right now.
    pub fn held(&self) -> bool {
        self.lock().held.is_some()
    }
}

struct Platform;

#[cfg(unix)]
mod helper {
    use super::Assertion;
    use blirp_core::proc_tree::ProcessTree;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    /// A helper process that holds the assertion for as long as it runs,
    /// in its own process group so release ends all of it.
    pub struct Helper {
        child: Child,
        tree: ProcessTree,
    }

    impl Helper {
        pub fn spawn(mut cmd: Command) -> anyhow::Result<Self> {
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0);
            let child = cmd.spawn()?;
            let tree = ProcessTree::for_process_group(child.id());
            Ok(Self { child, tree })
        }
    }

    impl Assertion for Helper {
        fn alive(&mut self) -> bool {
            matches!(self.child.try_wait(), Ok(None))
        }
    }

    impl Drop for Helper {
        fn drop(&mut self) {
            if !self.tree.terminate()
                && let Err(e) = self.child.kill()
            {
                tracing::debug!(error = %e, "stopping the sleep helper");
            }
            self.tree.escalate(Duration::from_secs(1));
            // Reap it; the exit status says nothing useful.
            let _ = self.child.wait();
        }
    }
}

#[cfg(target_os = "macos")]
impl Inhibit for Platform {
    fn acquire(&self) -> anyhow::Result<Box<dyn Assertion>> {
        use anyhow::Context as _;
        let mut cmd = blirp_core::process::command("/usr/bin/caffeinate");
        // -i: no idle system sleep; -w: ends with the daemon, whatever happens to it.
        cmd.args(["-i", "-w", &std::process::id().to_string()]);
        Ok(Box::new(
            helper::Helper::spawn(cmd).context("start caffeinate")?,
        ))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl Inhibit for Platform {
    fn acquire(&self) -> anyhow::Result<Box<dyn Assertion>> {
        use anyhow::Context as _;
        let inhibit = blirp_core::process::which("systemd-inhibit")
            .context("systemd-inhibit is not installed")?;
        let mut cmd = blirp_core::process::command(inhibit);
        // The held command ends with the daemon, so a crash cannot leave the
        // lock behind (`sleep infinity` would).
        let watch = format!(
            "while kill -0 {} 2>/dev/null; do sleep 10; done",
            std::process::id()
        );
        cmd.args([
            "--what=sleep",
            "--who=blirp",
            "--why=blirp sessions are running",
            "--mode=block",
            "sh",
            "-c",
            &watch,
        ]);
        Ok(Box::new(
            helper::Helper::spawn(cmd).context("start systemd-inhibit")?,
        ))
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
impl Inhibit for Platform {
    fn acquire(&self) -> anyhow::Result<Box<dyn Assertion>> {
        use windows_sys::Win32::System::Power::{
            ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
        };
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (started, ok) = std::sync::mpsc::channel::<bool>();
        let thread = std::thread::Builder::new()
            .name("keep-awake".into())
            .spawn(move || {
                // SAFETY: plain Win32 call with valid flags; it only changes
                // this thread's execution state.
                let prev = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
                let _ = started.send(prev != 0);
                // Returns when the Assertion (the sender) is dropped.
                let _ = released.recv();
                // SAFETY: as above; clears this thread's requirement.
                unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
            })?;
        if !ok.recv().unwrap_or(false) {
            drop(release);
            let _ = thread.join();
            anyhow::bail!("SetThreadExecutionState failed");
        }
        Ok(Box::new(WinAssertion {
            release: Some(release),
            thread: Some(thread),
        }))
    }
}

#[cfg(windows)]
struct WinAssertion {
    release: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl Assertion for WinAssertion {
    fn alive(&mut self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

#[cfg(windows)]
impl Drop for WinAssertion {
    fn drop(&mut self) {
        self.release.take();
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            tracing::warn!("keep-awake thread panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct Counts {
        acquired: AtomicUsize,
        released: AtomicUsize,
        fail: AtomicBool,
        dead: Arc<AtomicBool>,
    }

    struct Fake(Arc<Counts>);
    struct FakeAssertion(Arc<Counts>);

    impl Inhibit for Fake {
        fn acquire(&self) -> anyhow::Result<Box<dyn Assertion>> {
            if self.0.fail.load(Ordering::SeqCst) {
                anyhow::bail!("no tool");
            }
            self.0.acquired.fetch_add(1, Ordering::SeqCst);
            self.0.dead.store(false, Ordering::SeqCst);
            Ok(Box::new(FakeAssertion(self.0.clone())))
        }
    }

    impl Assertion for FakeAssertion {
        fn alive(&mut self) -> bool {
            !self.0.dead.load(Ordering::SeqCst)
        }
    }

    impl Drop for FakeAssertion {
        fn drop(&mut self) {
            self.0.released.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn setup() -> (KeepAwake, Arc<Counts>) {
        let c = Arc::new(Counts::default());
        (KeepAwake::with(Box::new(Fake(c.clone()))), c)
    }

    fn counts(c: &Counts) -> (usize, usize) {
        (
            c.acquired.load(Ordering::SeqCst),
            c.released.load(Ordering::SeqCst),
        )
    }

    #[test]
    fn held_exactly_while_sessions_are_live() {
        let (k, c) = setup();
        let t = Instant::now();
        k.update(0, true, t);
        assert!(!k.held());
        assert_eq!(counts(&c), (0, 0));
        // One assertion however many sessions and ticks.
        for live in [1, 2, 3, 2, 1] {
            k.update(live, true, t);
            assert!(k.held());
        }
        assert_eq!(counts(&c), (1, 0));
        k.update(0, true, t);
        assert!(!k.held());
        assert_eq!(counts(&c), (1, 1));
        k.update(0, true, t);
        assert_eq!(counts(&c), (1, 1));
        // A new session takes it again.
        k.update(1, true, t);
        assert_eq!(counts(&c), (2, 1));
    }

    #[test]
    fn disabling_releases_and_prevents_it() {
        let (k, c) = setup();
        let t = Instant::now();
        k.update(2, false, t);
        assert!(!k.held());
        k.update(2, true, t);
        assert!(k.held());
        k.update(2, false, t);
        assert!(!k.held());
        assert_eq!(counts(&c), (1, 1));
    }

    #[test]
    fn failures_are_retried_after_a_pause_not_every_tick() {
        let (k, c) = setup();
        let t = Instant::now();
        c.fail.store(true, Ordering::SeqCst);
        k.update(1, true, t);
        assert!(!k.held());
        c.fail.store(false, Ordering::SeqCst);
        k.update(1, true, t + Duration::from_secs(1));
        assert!(!k.held(), "retried too early");
        k.update(1, true, t + RETRY_AFTER);
        assert!(k.held());
        // An assertion that died is replaced after the same pause.
        c.dead.store(true, Ordering::SeqCst);
        k.update(1, true, t + RETRY_AFTER);
        assert!(!k.held());
        assert_eq!(counts(&c), (1, 1));
        k.update(1, true, t + RETRY_AFTER * 2);
        assert!(k.held());
        assert_eq!(counts(&c), (2, 1));
        // Sessions ending resets the pause.
        c.fail.store(true, Ordering::SeqCst);
        k.update(0, true, t + RETRY_AFTER * 2);
        c.fail.store(false, Ordering::SeqCst);
        k.update(1, true, t + RETRY_AFTER * 2);
        assert!(k.held());
    }

    // The real assertion can be taken and released. Not on Linux: CI
    // containers have no logind for systemd-inhibit to talk to.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn platform_assertion_round_trip() {
        let mut a = Platform.acquire().unwrap();
        assert!(a.alive());
        drop(a);
    }
}
