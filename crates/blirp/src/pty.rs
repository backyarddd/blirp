//! PTY supervisor (§6): one [`Terminal`] per live session.
//!
//! Threads per terminal: a reader (PTY output -> vt100 screen + broadcast), a
//! writer (input channel -> PTY) and a waiter (child exit -> close PTY ->
//! final broadcast -> `on_exit`). The vt100 parser holds the screen state
//! (10 000 lines of scrollback) used for attach snapshots, and answers
//! terminal queries itself while no client is attached so ConPTY and TUIs
//! never block waiting for a terminal.

use crate::proc_tree::ProcessTree;
use anyhow::Context as _;
use blirp_core::model::SessionStatus;
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
    pub code: i32,
    pub status: SessionStatus,
}

#[derive(Debug, Clone)]
pub enum TermEvent {
    Data(Bytes),
    Resize { cols: u16, rows: u16 },
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
}

impl Terminal {
    /// Spawn `req` in a new PTY. `on_exit` runs on the waiter thread once the
    /// child has exited and its final output has been broadcast.
    pub fn spawn(
        session_id: &str,
        req: SpawnRequest,
        on_exit: impl FnOnce(ExitInfo) + Send + 'static,
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
        let tree = ProcessTree::attach(child.as_ref());
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader().context("pty reader")?;
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
                    match reader.read(&mut buf) {
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
                // Closing the master ends the reader (ClosePseudoConsole on Windows).
                drop(lock(&t.master).take());
                if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                    tracing::debug!(session = %t.session_id, "pty output still open after exit (orphaned children)");
                }
                // Reap anything the agent left running in its tree, like a terminal hangup.
                t.tree.terminate();
                let status = if t.detach_on_exit.load(Ordering::SeqCst) {
                    SessionStatus::Detached
                } else if code == 0 || t.stop_requested.load(Ordering::SeqCst) {
                    SessionStatus::Completed
                } else {
                    SessionStatus::Failed
                };
                let info = ExitInfo { code, status };
                {
                    let mut s = lock(&t.screen);
                    s.exited = Some(info);
                    // No subscribers is fine.
                    let _ = t.tx.send(TermEvent::Exit(info));
                }
                on_exit(info);
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

    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        if !(1..=1000).contains(&cols) || !(1..=1000).contains(&rows) {
            anyhow::bail!("terminal size must be 1-1000 columns and rows");
        }
        let mut s = lock(&self.screen);
        if s.parser.screen().size() == (rows, cols) {
            return Ok(());
        }
        if let Some(m) = lock(&self.master).as_ref() {
            m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resize pty")?;
        }
        s.parser.screen_mut().set_size(rows, cols);
        let _ = self.tx.send(TermEvent::Resize { cols, rows });
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
            // SIGHUP first; escalate if the tree ignores it.
            let t = self.clone();
            std::thread::spawn(move || {
                std::thread::sleep(KILL_GRACE);
                if !t.has_exited() {
                    t.tree.force_kill();
                    if let Err(e) = lock(&t.killer).kill() {
                        tracing::debug!(session = %t.session_id, error = %e, "kill after grace failed");
                    }
                }
            });
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

/// Live terminals by session id.
#[derive(Default)]
pub struct Registry {
    terms: Mutex<HashMap<String, Arc<Terminal>>>,
}

impl Registry {
    pub fn insert(&self, t: Arc<Terminal>) {
        lock(&self.terms).insert(t.session_id.clone(), t);
    }

    pub fn get(&self, session_id: &str) -> Option<Arc<Terminal>> {
        lock(&self.terms).get(session_id).cloned()
    }

    pub fn remove(&self, session_id: &str) -> Option<Arc<Terminal>> {
        lock(&self.terms).remove(session_id)
    }

    pub fn all(&self) -> Vec<Arc<Terminal>> {
        lock(&self.terms).values().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        lock(&self.terms).is_empty()
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
