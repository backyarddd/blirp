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
//! `process_group(0)`), so its pid is the group id. The group alone is not
//! the tree: a shell puts background jobs (`nohup sleep &`, `(...) &` with
//! job control) into process groups of their own. The tree is therefore the
//! group, plus every process of the child's session (a PTY child's pid is
//! its session id, and reparenting to init keeps the session), plus every
//! descendant still linked by parent pid. Terminating sends SIGHUP and
//! SIGTERM to all of them, escalation SIGKILL to whatever is left.

#[cfg(unix)]
use std::sync::{Mutex, PoisonError};

pub struct ProcessTree {
    #[cfg(windows)]
    job: Option<win::Job>,
    #[cfg(unix)]
    pgid: Option<i32>,
    /// (pid, session id) of the members found when terminating, so they are
    /// still found after the leader died and parent links are gone.
    #[cfg(unix)]
    seen: Mutex<Vec<(i32, i32)>>,
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
            #[cfg(unix)]
            seen: Mutex::new(Vec::new()),
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
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Ask the tree to exit (unix SIGHUP + SIGTERM) or terminate it
    /// (Windows). Returns false when no tree handle exists and the caller
    /// must fall back to killing the main process.
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
            if self.pgid.is_none() {
                return false;
            }
            let members = self.members();
            {
                let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
                for &pid in &members {
                    if let Some(sid) = unix::sid(pid)
                        && !seen.iter().any(|(p, _)| *p == pid)
                    {
                        seen.push((pid, sid));
                    }
                }
            }
            for sig in [libc::SIGHUP, libc::SIGTERM] {
                self.signal(sig);
                for &pid in &members {
                    unix::kill(pid, sig);
                }
            }
            true
        }
    }

    /// Hard kill of whatever is left (unix SIGKILL to the whole tree;
    /// Windows: same as terminate).
    pub fn force_kill(&self) {
        #[cfg(windows)]
        {
            self.terminate();
        }
        #[cfg(unix)]
        {
            self.signal(libc::SIGKILL);
            for pid in self.members() {
                unix::kill(pid, libc::SIGKILL);
            }
        }
    }

    /// After [`ProcessTree::terminate`]: unix waits up to `grace` for every
    /// process of the tree to exit, then SIGKILLs what is left. This does not
    /// depend on the leader: processes that ignore SIGHUP/SIGTERM, or moved
    /// to process groups of their own, die even when the leader already
    /// exited. Windows: nothing to do, terminating the job is final.
    pub fn escalate(&self, grace: std::time::Duration) {
        #[cfg(unix)]
        {
            let deadline = std::time::Instant::now() + grace;
            while self.alive() {
                if std::time::Instant::now() >= deadline {
                    self.force_kill();
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
        #[cfg(windows)]
        let _ = grace;
    }

    /// Whether any process of the tree still runs (a group of zombies only
    /// counts until they are reaped).
    #[cfg(unix)]
    pub fn alive(&self) -> bool {
        let Some(pgid) = self.pgid else { return false };
        #[allow(unsafe_code)]
        // SAFETY: signal 0 only checks existence and permission.
        let rc = unsafe { libc::killpg(pgid, 0) };
        let group = rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        group || !self.members().is_empty()
    }

    /// Live (non-zombie) processes of the tree other than the group: the
    /// leader's session, descendants by parent pid, and members seen at
    /// terminate time that are still the same process (same session id).
    #[cfg(unix)]
    fn members(&self) -> Vec<i32> {
        let Some(leader) = self.pgid else {
            return Vec::new();
        };
        let procs = unix::processes();
        let mut set: Vec<i32> = procs
            .iter()
            .filter(|p| p.pid == leader || unix::sid(p.pid) == Some(leader))
            .map(|p| p.pid)
            .collect();
        if !set.contains(&leader) {
            set.push(leader);
        }
        // Descendants still linked to a member (fixpoint over parent pids).
        loop {
            let before = set.len();
            for p in &procs {
                if set.contains(&p.ppid) && !set.contains(&p.pid) {
                    set.push(p.pid);
                }
            }
            if set.len() == before {
                break;
            }
        }
        let seen = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for (pid, sid) in seen {
            if !set.contains(&pid)
                && procs.iter().any(|p| p.pid == pid)
                && unix::sid(pid) == Some(sid)
            {
                set.push(pid);
            }
        }
        let own = i32::try_from(std::process::id()).unwrap_or(0);
        set.retain(|&p| p > 1 && p != own && procs.iter().any(|x| x.pid == p));
        set
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

#[cfg(unix)]
#[allow(unsafe_code)]
mod unix {
    use std::time::Duration;

    pub struct Proc {
        pub pid: i32,
        pub ppid: i32,
    }

    /// Every live (non-zombie) process: `ps` is the portable listing on
    /// Linux and macOS. An unreadable listing yields none.
    pub fn processes() -> Vec<Proc> {
        let mut cmd = crate::process::command("ps");
        cmd.args(["-A", "-o", "pid=,ppid=,stat="]);
        let out = match crate::process::run(cmd, Duration::from_secs(5), 8 << 20) {
            Ok(o) if o.success() => o.stdout,
            Ok(o) => {
                tracing::warn!(status = ?o.status, "ps failed; process tree limited to its group");
                return Vec::new();
            }
            Err(e) => {
                tracing::warn!(error = %e, "ps failed; process tree limited to its group");
                return Vec::new();
            }
        };
        parse(&String::from_utf8_lossy(&out))
    }

    pub fn parse(text: &str) -> Vec<Proc> {
        text.lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let pid = f.next()?.parse().ok()?;
                let ppid = f.next()?.parse().ok()?;
                let stat = f.next().unwrap_or("");
                (!stat.starts_with('Z')).then_some(Proc { pid, ppid })
            })
            .collect()
    }

    pub fn sid(pid: i32) -> Option<i32> {
        // SAFETY: getsid has no memory-safety preconditions.
        let s = unsafe { libc::getsid(pid) };
        (s > 0).then_some(s)
    }

    pub fn kill(pid: i32, sig: i32) {
        // SAFETY: kill has no memory-safety preconditions; a gone process
        // only yields ESRCH.
        let rc = unsafe { libc::kill(pid, sig) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                tracing::debug!(error = %err, pid, sig, "kill failed");
            }
        }
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
        let child = crate::process::command("sh")
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
    fn ps_listing_skips_zombies() {
        let procs = super::unix::parse("  12     1 Ss\n  13    12 Z+\n garbage\n  14    12 R+\n");
        let pids: Vec<i32> = procs.iter().map(|p| p.pid).collect();
        assert_eq!(pids, [12, 14]);
    }

    /// A PTY-like child: its own session, like portable-pty's `setsid`.
    fn session(script: &str) -> (std::process::Child, ProcessTree) {
        let mut cmd = crate::process::command("sh");
        cmd.args(["-c", script]);
        #[allow(unsafe_code)]
        // SAFETY: setsid is async-signal-safe and touches no Rust state.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        let tree = ProcessTree::for_process_group(child.id());
        (child, tree)
    }

    // Background jobs in process groups of their own (job control on) that
    // ignore SIGHUP and SIGTERM still die with the tree, also after the
    // leader exited and they were reparented.
    #[test]
    fn stop_kills_jobs_outside_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!(
            "set -m; (trap '' HUP TERM; exec sleep 60) & echo $! > '{}'; nohup sleep 60 >/dev/null 2>&1 & wait",
            pidfile.display()
        );
        let (mut child, tree) = session(&script);
        let deadline = Instant::now() + Duration::from_secs(5);
        let job: i32 = loop {
            if let Some(p) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|t| t.trim().parse().ok())
            {
                break p;
            }
            assert!(Instant::now() < deadline, "background job did not start");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(tree.members().contains(&job));
        tree.terminate();
        tree.escalate(Duration::from_millis(500));
        child.wait().unwrap();
        wait_dead(&tree);
        assert!(!super::unix::processes().iter().any(|p| p.pid == job));
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
