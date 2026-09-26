//! Whole-process-tree control for child processes: PTY sessions (§6),
//! summarizer runs and short-lived helpers ([`crate::process::run`]).
//!
//! Windows: the child is assigned to a job object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; killing terminates the job, and the
//! job also dies with the daemon. Processes the child spawns before it is
//! assigned (a few microseconds after creation) escape the job; ConPTY's
//! close still sends them `CTRL_CLOSE_EVENT`.
//!
//! Unix: the child leads its own process group (portable-pty starts PTY
//! children with `setsid`; plain children are spawned with
//! `process_group(0)`), so its pid is the group id; killing sends SIGHUP to
//! the group, then SIGKILL.

pub struct ProcessTree {
    #[cfg(windows)]
    job: Option<win::Job>,
    #[cfg(unix)]
    pgid: Option<i32>,
}

impl ProcessTree {
    /// No tree handle: [`ProcessTree::terminate`] reports false and the
    /// caller kills the main process itself.
    pub fn none() -> Self {
        Self {
            #[cfg(windows)]
            job: None,
            #[cfg(unix)]
            pgid: None,
        }
    }

    /// Tree of a child process. Windows: assigns the process to a new
    /// kill-on-close job. Unix: the child must lead its own process group.
    #[cfg(windows)]
    pub fn for_process_handle(handle: std::os::windows::io::RawHandle) -> Self {
        Self::job_for(handle, true)
    }

    /// Like [`ProcessTree::for_process_handle`], but dropping it leaves the
    /// tree alone (Windows): background helpers a short-lived tool starts on
    /// purpose (git's fsmonitor daemon) outlive it, and only an explicit
    /// terminate kills them.
    #[cfg(windows)]
    pub fn for_process_handle_detached(handle: std::os::windows::io::RawHandle) -> Self {
        Self::job_for(handle, false)
    }

    #[cfg(windows)]
    fn job_for(handle: std::os::windows::io::RawHandle, kill_on_close: bool) -> Self {
        let job = win::Job::for_process(handle, kill_on_close)
            .map_err(|e| tracing::warn!(error = %e, "cannot assign child process to a job object"))
            .ok();
        Self { job }
    }

    #[cfg(unix)]
    pub fn for_process_group(pid: u32) -> Self {
        Self {
            pgid: i32::try_from(pid).ok(),
        }
    }

    /// Ask the tree to exit (unix SIGHUP) or terminate it (Windows). Returns
    /// false when no tree handle exists and the caller must fall back to
    /// killing the main process.
    pub fn terminate(&self) -> bool {
        #[cfg(windows)]
        {
            match &self.job {
                Some(job) => match job.terminate(1) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(error = %e, "TerminateJobObject failed");
                        false
                    }
                },
                None => false,
            }
        }
        #[cfg(unix)]
        {
            self.signal(libc::SIGHUP)
        }
    }

    /// Hard kill of whatever is left (unix SIGKILL; Windows: same as terminate).
    pub fn force_kill(&self) {
        #[cfg(windows)]
        {
            self.terminate();
        }
        #[cfg(unix)]
        {
            self.signal(libc::SIGKILL);
        }
    }

    /// After [`ProcessTree::terminate`]: unix waits up to `grace` for every
    /// member of the process group to exit, then SIGKILLs the group. This
    /// does not depend on the group leader: members that ignore SIGHUP die
    /// even when the leader already exited. Windows: nothing to do,
    /// terminating the job is final.
    pub fn escalate(&self, grace: std::time::Duration) {
        #[cfg(unix)]
        {
            let deadline = std::time::Instant::now() + grace;
            while self.alive() {
                if std::time::Instant::now() >= deadline {
                    self.signal(libc::SIGKILL);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        #[cfg(windows)]
        let _ = grace;
    }

    /// Whether any process of the group still exists (zombies included).
    #[cfg(unix)]
    pub fn alive(&self) -> bool {
        let Some(pgid) = self.pgid else { return false };
        #[allow(unsafe_code)]
        // SAFETY: signal 0 only checks existence and permission.
        let rc = unsafe { libc::killpg(pgid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[cfg(unix)]
    fn signal(&self, sig: i32) -> bool {
        let Some(pgid) = self.pgid else { return false };
        #[allow(unsafe_code)]
        // SAFETY: killpg has no memory-safety preconditions; an invalid or
        // already-reaped group only yields ESRCH.
        let rc = unsafe { libc::killpg(pgid, sig) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                tracing::warn!(error = %err, pgid, sig, "killpg failed");
            }
        }
        true
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod win {
    use std::io;
    use std::os::windows::io::RawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    pub struct Job(HANDLE);

    // SAFETY: a job object HANDLE is a process-wide kernel handle; the Win32
    // job APIs may be called on it from any thread.
    unsafe impl Send for Job {}
    // SAFETY: see `Send`; the handle is never mutated after creation.
    unsafe impl Sync for Job {}

    impl Job {
        pub fn for_process(process: RawHandle, kill_on_close: bool) -> io::Result<Job> {
            // SAFETY: null attributes and name are documented as valid.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Job(handle);
            if kill_on_close {
                job.kill_on_close()?;
            }
            // SAFETY: `process` is a live process handle owned by the
            // caller's child, valid for the duration of this call.
            if unsafe { AssignProcessToJobObject(job.0, process as HANDLE) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        fn kill_on_close(&self) -> io::Result<()> {
            // SAFETY: an all-zero JOBOBJECT_EXTENDED_LIMIT_INFORMATION is a
            // valid "no limits" value (plain integers and structs of integers).
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let size = u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .map_err(io::Error::other)?;
            // SAFETY: `info` is a live, correctly sized struct of the class passed.
            let ok = unsafe {
                SetInformationJobObject(
                    self.0,
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    size,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn terminate(&self, code: u32) -> io::Result<()> {
            // SAFETY: self.0 is a valid job handle until drop.
            if unsafe { TerminateJobObject(self.0, code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: we own the handle and close it exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::ProcessTree;
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    fn group(script: &str) -> (std::process::Child, ProcessTree) {
        let child = std::process::Command::new("sh")
            .args(["-c", script])
            .process_group(0)
            .spawn()
            .unwrap();
        let tree = ProcessTree::for_process_group(child.id());
        (child, tree)
    }

    fn wait_dead(tree: &ProcessTree) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while tree.alive() {
            assert!(Instant::now() < deadline, "process group survived SIGKILL");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn escalation_kills_a_group_that_ignores_sighup() {
        let (mut child, tree) = group("trap '' HUP; sleep 60");
        std::thread::sleep(Duration::from_millis(200));
        tree.terminate();
        tree.escalate(Duration::from_millis(300));
        child.wait().unwrap();
        wait_dead(&tree);
    }

    #[test]
    fn escalation_does_not_depend_on_the_leader() {
        // The leader exits at once; its background child ignores SIGHUP and
        // keeps the group alive.
        let (mut child, tree) = group("trap '' HUP; sleep 60 & exit 0");
        child.wait().unwrap();
        assert!(tree.alive());
        tree.terminate();
        tree.escalate(Duration::from_millis(300));
        wait_dead(&tree);
    }
}
