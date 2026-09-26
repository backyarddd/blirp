//! PTY supervisor (§6): one [`Terminal`] per live session.
//!
//! Threads per terminal: a reader (PTY output -> vt100 screen + broadcast), a
//! writer (input channel -> PTY) and a waiter (child exit -> close PTY ->
//! final broadcast -> `on_exit`). The vt100 parser holds the screen state
//! (10 000 lines of scrollback) used for attach snapshots, and answers
//! terminal queries itself while no client is attached so ConPTY and TUIs
//! never block waiting for a terminal.

use anyhow::Context as _;
use blirp_core::model::SessionStatus;
use blirp_core::proc_tree::ProcessTree;
use bytes::Bytes;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

pub const SCROLLBACK: usize = 10_000;
/// Output within this window means the agent is working (§7).
pub const ACTIVE_WINDOW: Duration = Duration::from_secs(2);
const KILL_GRACE: Duration = Duration::from_secs(3);
/// Output still arriving this long after the group was killed is dropped
/// and the reader thread ends (unix).
#[cfg_attr(not(unix), allow(dead_code))]
const READER_LINGER: Duration = Duration::from_secs(1);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Poisoning only means a panic elsewhere mid-update of plain data
    // (parser state, handles); continuing with it is safe.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    /// The process's exit code; `None` when the user stopped the session
    /// (the code then only says how it was killed).
    pub code: Option<i32>,
    pub status: SessionStatus,
    /// Ended by a user Stop (§7), reported as `completed`.
    pub stopped_by_user: bool,
}

#[derive(Debug, Clone)]
pub enum TermEvent {
    Data(Bytes),
    /// `by`: the client that asked for it (see [`Terminal::resize`]).
    Resize {
        cols: u16,
        rows: u16,
        by: u64,
    },
    Exit(ExitInfo),
}

pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    pub data: String,
}

#[derive(Default)]
struct Callbacks {
    title: Option<String>,
    replies: Vec<u8>,
}

impl vt100::Callbacks for Callbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let p0 = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, p0) {
            // DSR cursor position report.
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .extend(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            // DSR status: terminal OK.
            (None, 'n', 5) => self.replies.extend(b"\x1b[0n"),
            // Primary device attributes: VT100 with advanced video.
            (None, 'c', 0) => self.replies.extend(b"\x1b[?1;2c"),
            _ => {}
        }
    }
}

struct ScreenState {
    parser: vt100::Parser<Callbacks>,
    last_output: Option<Instant>,
    exited: Option<ExitInfo>,
}

pub struct Terminal {
    pub session_id: String,
    screen: Mutex<ScreenState>,
    tx: broadcast::Sender<TermEvent>,
    input: std::sync::mpsc::Sender<Vec<u8>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    tree: ProcessTree,
    stop_requested: AtomicBool,
    detach_on_exit: AtomicBool,
    /// Set once the process tree is gone for good: a reader still blocked on
    /// output (a process outside the tree holds the PTY open) gives up.
    #[cfg_attr(not(unix), allow(dead_code))]
    abandon_reader: AtomicBool,
}

impl Terminal {
    /// Spawn `req` in a new PTY. `on_exit` runs on the waiter thread once the
    /// child has exited and its final output has been broadcast; it gets the
    /// terminal so a registry can tell it apart from a later one.
    pub fn spawn(
        session_id: &str,
        req: SpawnRequest,
        on_exit: impl FnOnce(&Arc<Terminal>, ExitInfo) + Send + 'static,
    ) -> anyhow::Result<Arc<Terminal>> {
        let size = PtySize {
            rows: req.rows,
            cols: req.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system().openpty(size).context("open pty")?;
        let mut cmd = CommandBuilder::new(&req.program);
        cmd.args(&req.args);
        cmd.cwd(&req.cwd);
        for k in &req.env_remove {
            cmd.env_remove(k);
        }
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn {}", req.program.to_string_lossy()))?;
        // Only the child may hold the slave side, or EOF never arrives on unix.
        drop(pair.slave);
        let tree = process_tree(child.as_ref());
        let killer = child.clone_killer();
        let mut reader = PtyReader::new(pair.master.as_ref())?;
        let mut writer = pair.master.take_writer().context("pty writer")?;

        let (tx, _) = broadcast::channel(1024);
        let (input, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let term = Arc::new(Terminal {
            session_id: session_id.to_string(),
            screen: Mutex::new(ScreenState {
                parser: vt100::Parser::new_with_callbacks(
                    req.rows,
                    req.cols,
                    SCROLLBACK,
                    Callbacks::default(),
                ),
                last_output: None,
                exited: None,
            }),
            tx,
            input,
            master: Mutex::new(Some(pair.master)),
            killer: Mutex::new(killer),
            tree,
            stop_requested: AtomicBool::new(false),
            detach_on_exit: AtomicBool::new(false),
            abandon_reader: AtomicBool::new(false),
        });

        let sid = session_id.to_string();
        std::thread::Builder::new()
            .name(format!("pty-write-{sid}"))
            .spawn(move || {
                // Ends when every sender (held by the Terminal) is dropped.
                while let Ok(bytes) = input_rx.recv() {
                    if let Err(e) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                        tracing::debug!(session = %sid, error = %e, "pty write failed; input closed");
                        break;
                    }
                }
            })
            .context("spawn pty writer thread")?;

        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let t = term.clone();
        std::thread::Builder::new()
            .name(format!("pty-read-{session_id}"))
            .spawn(move || {
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    match reader.read(&mut buf, &t.abandon_reader) {
                        Ok(0) => break,
                        Ok(n) => t.on_output(&buf[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            // EIO on unix / broken pipe on Windows once the PTY closes.
                            tracing::debug!(session = %t.session_id, error = %e, "pty read ended");
                            break;
                        }
                    }
                }
                // The waiter may already have given up; nothing to report then.
                let _ = done_tx.send(());
            })
            .context("spawn pty reader thread")?;

        let t = term.clone();
        std::thread::Builder::new()
            .name(format!("pty-wait-{session_id}"))
            .spawn(move || {
                let code = match child.wait() {
                    // NTSTATUS-style Windows codes wrap to negative i32 on purpose.
                    Ok(st) => st.exit_code() as i32,
                    Err(e) => {
                        tracing::warn!(session = %t.session_id, error = %e, "waiting for session process failed");
                        -1
                    }
                };
                // Closing the master ends the reader (ClosePseudoConsole on
                // Windows). Take it under the lock but close it outside: the
                // close waits for the reader to drain output, and the reader
                // needs the screen lock.
                let master = lock(&t.master).take();
                drop(master);
                if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                    tracing::debug!(session = %t.session_id, "pty output still open after exit (orphaned children)");
                }
                // Reap anything the agent left running in its tree, like a
                // terminal hangup; unix escalates to SIGKILL after the grace.
                t.tree.terminate();
                t.reap_in_background();
                let info = if t.detach_on_exit.load(Ordering::SeqCst) {
                    ExitInfo {
                        code: Some(code),
                        status: SessionStatus::Detached,
                        stopped_by_user: false,
                    }
                } else if t.stop_requested.load(Ordering::SeqCst) {
                    // The code only says how the tree was killed (on Windows
                    // TerminateJobObject's 1); the user's intent is the result.
                    ExitInfo {
                        code: None,
                        status: SessionStatus::Completed,
                        stopped_by_user: true,
                    }
                } else {
                    ExitInfo {
                        code: Some(code),
                        status: if code == 0 {
                            SessionStatus::Completed
                        } else {
                            SessionStatus::Failed
                        },
                        stopped_by_user: false,
                    }
                };
                {
                    let mut s = lock(&t.screen);
                    s.exited = Some(info);
                    // No subscribers is fine.
                    let _ = t.tx.send(TermEvent::Exit(info));
                }
                on_exit(&t, info);
            })
            .context("spawn pty waiter thread")?;

        Ok(term)
    }

    fn on_output(&self, bytes: &[u8]) {
        let mut s = lock(&self.screen);
        s.parser.process(bytes);
        s.last_output = Some(Instant::now());
        let replies = std::mem::take(&mut s.parser.callbacks_mut().replies);
        // Attached clients (xterm.js) answer queries themselves.
        if !replies.is_empty() && self.tx.receiver_count() == 0 {
            self.write(replies);
        }
        let _ = self.tx.send(TermEvent::Data(Bytes::copy_from_slice(bytes)));
    }

    /// Snapshot of the current state plus a receiver for everything after it.
    /// Taken under the screen lock, so no output is lost or duplicated.
    pub fn attach(&self) -> (Snapshot, broadcast::Receiver<TermEvent>, Option<ExitInfo>) {
        let mut s = lock(&self.screen);
        let snap = snapshot(&mut s.parser);
        (snap, self.tx.subscribe(), s.exited)
    }

    pub fn write(&self, bytes: Vec<u8>) {
        if self.input.send(bytes).is_err() {
            tracing::debug!(session = %self.session_id, "input dropped: pty writer closed");
        }
    }

    /// Resize the PTY and screen; `by` identifies the requesting client so
    /// the broadcast is not echoed back to it.
    pub fn resize(&self, cols: u16, rows: u16, by: u64) -> anyhow::Result<()> {
        if !(1..=1000).contains(&cols) || !(1..=1000).contains(&rows) {
            anyhow::bail!("terminal size must be 1-1000 columns and rows");
        }
        // Lock order is `master` then `screen`, and `screen` is never held
        // while the PTY resizes: ResizePseudoConsole can wait for the reader,
        // which needs `screen`. Holding `master` throughout keeps concurrent
        // resizes from interleaving PTY and parser sizes.
        let master = lock(&self.master);
        if lock(&self.screen).parser.screen().size() == (rows, cols) {
            return Ok(());
        }
        if let Some(m) = master.as_ref() {
            m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize pty")?;
        }
        lock(&self.screen).parser.screen_mut().set_size(rows, cols);
        let _ = self.tx.send(TermEvent::Resize { cols, rows, by });
        Ok(())
    }

    /// Stop the session: terminate the whole process tree.
    pub fn kill(self: &Arc<Self>) {
        self.stop_requested.store(true, Ordering::SeqCst);
        self.kill_tree();
    }

    /// Kill for daemon shutdown: the session will be reported as `detached`.
    pub fn kill_for_shutdown(self: &Arc<Self>) {
        self.detach_on_exit.store(true, Ordering::SeqCst);
        self.kill_tree();
    }

    fn kill_tree(self: &Arc<Self>) {
        if !self.tree.terminate()
            && let Err(e) = lock(&self.killer).kill()
        {
            tracing::warn!(session = %self.session_id, error = %e, "kill failed");
        }
        if cfg!(unix) {
            // SIGHUP first; SIGKILL the whole group if anything in it ignores
            // it, whether or not the leader has exited by then.
            let t = self.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("pty-kill-{}", self.session_id))
                .spawn(move || {
                    t.tree.escalate(KILL_GRACE);
                    if !t.has_exited()
                        && let Err(e) = lock(&t.killer).kill()
                    {
                        tracing::debug!(session = %t.session_id, error = %e, "kill after grace failed");
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(session = %self.session_id, error = %e, "cannot start kill escalation; killing now");
                self.tree.force_kill();
            }
        }
    }

    /// After the process exited: SIGKILL group members that outlived the
    /// hangup once the grace is over, then release a reader that processes
    /// outside the group still hold open (unix; the Windows job is gone).
    fn reap_in_background(self: &Arc<Self>) {
        if !cfg!(unix) {
            return;
        }
        let t = self.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("pty-reap-{}", self.session_id))
            .spawn(move || {
                t.tree.escalate(KILL_GRACE);
                std::thread::sleep(READER_LINGER);
                t.abandon_reader.store(true, Ordering::SeqCst);
            });
        if let Err(e) = spawned {
            tracing::warn!(session = %self.session_id, error = %e, "cannot start reaper; killing the group now");
            self.tree.force_kill();
            self.abandon_reader.store(true, Ordering::SeqCst);
        }
    }

    pub fn has_exited(&self) -> bool {
        lock(&self.screen).exited.is_some()
    }

    pub fn last_output(&self) -> Option<Instant> {
        lock(&self.screen).last_output
    }

    pub fn bracketed_paste(&self) -> bool {
        lock(&self.screen).parser.screen().bracketed_paste()
    }

    /// Output heuristic of §7: `starting` before any output, `working` with
    /// output in the last 2 s, else `idle`.
    pub fn activity_status(&self, now: Instant) -> SessionStatus {
        match self.last_output() {
            None => SessionStatus::Starting,
            Some(t) if now.duration_since(t) < ACTIVE_WINDOW => SessionStatus::Working,
            Some(_) => SessionStatus::Idle,
        }
    }

    /// Plain-text screen contents (tests and diagnostics).
    pub fn screen_text(&self) -> String {
        lock(&self.screen).parser.screen().contents()
    }
}

/// Bytes that make a fresh terminal look like `parser`'s: reset, scrollback
/// lines (normal screen only), the visible screen with attributes and
/// cursor, input modes and title.
fn snapshot(parser: &mut vt100::Parser<Callbacks>) -> Snapshot {
    let (rows, cols) = parser.screen().size();
    let mut out: Vec<u8> = b"\x1bc".to_vec();
    if parser.screen().alternate_screen() {
        out.extend(b"\x1b[?1049h");
    } else {
        // Replay scrollback oldest first, followed by the screen rows, as
        // plain lines: the client's own scrollback then holds the history.
        parser.screen_mut().set_scrollback(usize::MAX);
        let mut offset = parser.screen().scrollback();
        let mut lines: Vec<Vec<u8>> = Vec::with_capacity(offset + usize::from(rows));
        while offset > 0 {
            parser.screen_mut().set_scrollback(offset);
            let take = offset.min(usize::from(rows));
            lines.extend(parser.screen().rows_formatted(0, cols).take(take));
            offset -= take;
        }
        parser.screen_mut().set_scrollback(0);
        if !lines.is_empty() {
            lines.extend(parser.screen().rows_formatted(0, cols));
            for (i, line) in lines.iter().enumerate() {
                if i > 0 {
                    out.extend(b"\r\n");
                }
                out.extend(line);
                out.extend(b"\x1b[m");
            }
        }
    }
    // Redraw the visible screen exactly (clears it first) plus input modes.
    out.extend(parser.screen().state_formatted());
    if let Some(title) = &parser.callbacks().title {
        out.extend(format!("\x1b]0;{title}\x07").as_bytes());
    }
    Snapshot {
        cols,
        rows,
        data: String::from_utf8_lossy(&out).into_owned(),
    }
}

/// The session's whole process tree (§6); without one, Stop kills only the
/// main process.
fn process_tree(child: &(dyn portable_pty::Child + Send + Sync)) -> ProcessTree {
    #[cfg(windows)]
    let tree = child
        .as_raw_handle()
        .map_or_else(ProcessTree::none, ProcessTree::for_process_handle);
    #[cfg(unix)]
    let tree = child
        .process_id()
        .map_or_else(ProcessTree::none, ProcessTree::for_process_group);
    tree
}

enum Slot {
    /// A launch or resume reserved the id and is spawning its process.
    Pending,
    Live(Arc<Terminal>),
}

type Slots = Arc<Mutex<HashMap<String, Slot>>>;

/// Live terminals by session id. An id is reserved before its process is
/// spawned, so concurrent resumes can never start two agents for one
/// session, and an exiting terminal only ever removes its own entry.
#[derive(Default)]
pub struct Registry {
    terms: Slots,
}

/// A reserved id, held until [`Reservation::fill`]; dropping it unfilled
/// (the spawn failed) frees the id again.
pub struct Reservation {
    terms: Slots,
    id: String,
    filled: bool,
}

impl Reservation {
    pub fn fill(mut self, t: Arc<Terminal>) {
        lock(&self.terms).insert(self.id.clone(), Slot::Live(t));
        self.filled = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.filled {
            let mut terms = lock(&self.terms);
            if matches!(terms.get(&self.id), Some(Slot::Pending)) {
                terms.remove(&self.id);
            }
        }
    }
}

impl Registry {
    /// Reserve `session_id`; `None` while it is live or being started.
    pub fn reserve(&self, session_id: &str) -> Option<Reservation> {
        let mut terms = lock(&self.terms);
        if terms.contains_key(session_id) {
            return None;
        }
        terms.insert(session_id.to_string(), Slot::Pending);
        Some(Reservation {
            terms: self.terms.clone(),
            id: session_id.to_string(),
            filled: false,
        })
    }

    pub fn get(&self, session_id: &str) -> Option<Arc<Terminal>> {
        match lock(&self.terms).get(session_id) {
            Some(Slot::Live(t)) => Some(t.clone()),
            _ => None,
        }
    }

    /// Remove `t`, but only while the entry is still `t` and not a newer
    /// terminal of the same session.
    pub fn remove(&self, t: &Arc<Terminal>) -> bool {
        let mut terms = lock(&self.terms);
        let ours = matches!(terms.get(&t.session_id), Some(Slot::Live(x)) if Arc::ptr_eq(x, t));
        if ours {
            terms.remove(&t.session_id);
        }
        ours
    }

    pub fn all(&self) -> Vec<Arc<Terminal>> {
        lock(&self.terms)
            .values()
            .filter_map(|s| match s {
                Slot::Live(t) => Some(t.clone()),
                Slot::Pending => None,
            })
            .collect()
    }

    /// No live terminal and no launch in progress.
    pub fn is_empty(&self) -> bool {
        lock(&self.terms).is_empty()
    }
}

/// PTY output reader. On unix it owns a duplicate of the master fd and
/// polls it, so it can give up (`abandon`) when a process outside the
/// session's group keeps the PTY open forever; a plain blocking read would
/// leak the thread. Windows reads end when ClosePseudoConsole runs.
struct PtyReader {
    #[cfg(unix)]
    file: std::fs::File,
    #[cfg(not(unix))]
    inner: Box<dyn Read + Send>,
}

impl PtyReader {
    #[cfg(not(unix))]
    fn new(master: &(dyn MasterPty + Send)) -> anyhow::Result<Self> {
        Ok(Self {
            inner: master.try_clone_reader().context("pty reader")?,
        })
    }

    #[cfg(not(unix))]
    fn read(&mut self, buf: &mut [u8], _abandon: &AtomicBool) -> std::io::Result<usize> {
        self.inner.read(buf)
    }

    #[cfg(unix)]
    fn new(master: &(dyn MasterPty + Send)) -> anyhow::Result<Self> {
        let fd = master.as_raw_fd().context("pty master has no fd")?;
        Ok(Self {
            file: unix_fd::dup(fd).context("pty reader")?,
        })
    }

    #[cfg(unix)]
    fn read(&mut self, buf: &mut [u8], abandon: &AtomicBool) -> std::io::Result<usize> {
        loop {
            if unix_fd::readable(&self.file, 200)? {
                return self.file.read(buf);
            }
            if abandon.load(Ordering::SeqCst) {
                return Ok(0);
            }
        }
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
mod unix_fd {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

    /// A new close-on-exec descriptor for the same open file.
    pub fn dup(fd: RawFd) -> std::io::Result<std::fs::File> {
        // SAFETY: fcntl(F_DUPFD_CLOEXEC) only reads `fd`, which the caller's
        // live MasterPty owns; an invalid fd yields EBADF.
        let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if new < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `new` is a fresh descriptor nothing else owns.
        Ok(std::fs::File::from(unsafe { OwnedFd::from_raw_fd(new) }))
    }

    /// Whether `f` has input (or a hangup or error to report) within `timeout_ms`.
    pub fn readable(f: &std::fs::File, timeout_ms: i32) -> std::io::Result<bool> {
        let mut p = libc::pollfd {
            fd: f.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `p` is one valid pollfd for a descriptor `f` keeps open.
        let n = unsafe { libc::poll(&raw mut p, 1, timeout_ms) };
        if n > 0 {
            return Ok(true);
        }
        if n == 0 {
            return Ok(false);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reproduces_scrollback_screen_and_title() {
        let mut p = vt100::Parser::new_with_callbacks(4, 20, SCROLLBACK, Callbacks::default());
        for i in 0..10 {
            p.process(format!("line {i}\r\n").as_bytes());
        }
        p.process(b"\x1b]0;my title\x07\x1b[31mred\x1b[m");
        let snap = snapshot(&mut p);
        assert_eq!((snap.cols, snap.rows), (20, 4));

        let mut client = vt100::Parser::new_with_callbacks(4, 20, SCROLLBACK, Callbacks::default());
        client.process(snap.data.as_bytes());
        assert_eq!(client.screen().contents(), p.screen().contents());
        assert_eq!(
            client.screen().cursor_position(),
            p.screen().cursor_position()
        );
        assert_eq!(client.callbacks().title.as_deref(), Some("my title"));
        client.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(
            client.screen().scrollback(),
            p.screen_mut().tap_scrollback()
        );
        assert!(client.screen().contents().contains("line 0"));
    }

    trait TapScrollback {
        fn tap_scrollback(&mut self) -> usize;
    }
    impl TapScrollback for vt100::Screen {
        fn tap_scrollback(&mut self) -> usize {
            self.set_scrollback(usize::MAX);
            let n = self.scrollback();
            self.set_scrollback(0);
            n
        }
    }

    #[test]
    fn alternate_screen_snapshot() {
        let mut p = vt100::Parser::new_with_callbacks(4, 20, SCROLLBACK, Callbacks::default());
        p.process(b"shell$ \x1b[?1049h\x1b[2;3Htui");
        let snap = snapshot(&mut p);
        let mut client = vt100::Parser::new(4, 20, 0);
        client.process(snap.data.as_bytes());
        assert!(client.screen().alternate_screen());
        assert_eq!(client.screen().contents(), p.screen().contents());
    }

    #[test]
    fn terminal_queries_are_answered() {
        let mut p = vt100::Parser::new_with_callbacks(10, 20, 0, Callbacks::default());
        p.process(b"ab\x1b[6n\x1b[c\x1b[5n");
        assert_eq!(p.callbacks().replies, b"\x1b[1;3R\x1b[?1;2c\x1b[0n");
    }
}
