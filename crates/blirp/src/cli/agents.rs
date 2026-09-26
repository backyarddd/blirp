//! `blirp agents set-token|clear-token claude` (§7): the login token claude
//! sessions and the summarizer started by the daemon use, for a daemon that
//! cannot reach the login keychain (a LaunchAgent on a locked Mac, a start
//! over SSH). Works without a running daemon: it reads the file at spawn.

use anyhow::Context as _;
use blirp_core::claude_token;
use blirp_core::paths::Paths;
use clap::Subcommand;
use std::io::{BufRead as _, IsTerminal as _, Read as _, Write as _};
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum AgentsCommand {
    /// Store a login token from `claude setup-token`, read from stdin
    /// (hidden when typed or pasted; piping it in works too).
    SetToken {
        #[arg(value_parser = ["claude"])]
        agent: String,
    },
    /// Remove the stored login token.
    ClearToken {
        #[arg(value_parser = ["claude"])]
        agent: String,
    },
}

/// Piped input beyond this is not a token.
const MAX_INPUT: u64 = 64 * 1024;

pub fn run(paths: &Paths, cmd: AgentsCommand) -> anyhow::Result<ExitCode> {
    match cmd {
        AgentsCommand::SetToken { agent: _ } => {
            let input = read_token()?;
            claude_token::store(paths, &input).context("the token was not stored")?;
            println!(
                "Stored the claude login token in {}.",
                paths.claude_token_file().display()
            );
            println!(
                "New claude sessions and the summarizer use it (no restart needed); running sessions keep their login."
            );
        }
        AgentsCommand::ClearToken { agent: _ } => {
            if claude_token::clear(paths)? {
                println!("Removed the claude login token.");
            } else {
                println!("No claude login token was stored.");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// One hidden line from a terminal, else all of stdin.
fn read_token() -> anyhow::Result<String> {
    let stdin = std::io::stdin();
    let mut input = String::new();
    if stdin.is_terminal() {
        {
            // Echo goes off before the prompt, so nothing pasted early is shown.
            let _hidden = HiddenInput::start().map_err(|_| {
                // Git Bash / mintty: a terminal, but not a console whose
                // echo can be turned off.
                anyhow::anyhow!(
                    "cannot hide what you type in this terminal; pipe the token in instead, \
                     e.g. `blirp agents set-token claude < token.txt`"
                )
            })?;
            eprint!(
                "Paste the token printed by `claude setup-token` (input is hidden), then press Enter: "
            );
            std::io::stderr().flush()?;
            stdin.lock().read_line(&mut input)?;
        }
        eprintln!();
    } else {
        stdin.lock().take(MAX_INPUT).read_to_string(&mut input)?;
    }
    Ok(input)
}

/// Terminal state to restore when the process is interrupted at the prompt
/// (Ctrl+C runs no destructors). Set once: one prompt per process.
#[cfg(unix)]
static SAVED: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();
#[cfg(windows)]
static SAVED: std::sync::OnceLock<(usize, u32)> = std::sync::OnceLock::new();

#[cfg(unix)]
const SIGNALS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGTERM];

/// Restores the terminal, then dies of the same signal as it would have.
#[cfg(unix)]
#[allow(unsafe_code)]
extern "C" fn restore_and_reraise(sig: libc::c_int) {
    // SAFETY: tcsetattr, signal and raise are async-signal-safe; SAVED was
    // set before this handler was installed and is never written again.
    unsafe {
        if let Some(t) = SAVED.get() {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t);
        }
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Restores the console mode; FALSE lets the default handler end the process.
#[cfg(windows)]
#[allow(unsafe_code)]
unsafe extern "system" fn restore_on_ctrl(_event: u32) -> windows_sys::core::BOOL {
    if let Some((handle, mode)) = SAVED.get() {
        // SAFETY: the console input handle and mode saved in `start`.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleMode(*handle as _, *mode);
        }
    }
    0
}

/// Terminal echo of stdin is off while this lives, and is turned back on
/// when the process is interrupted (Ctrl+C, SIGTERM) in the meantime.
struct HiddenInput {
    #[cfg(unix)]
    saved: libc::termios,
    #[cfg(unix)]
    previous: [libc::sighandler_t; 2],
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(windows)]
    saved: u32,
}

impl HiddenInput {
    #[allow(unsafe_code)]
    fn start() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            let mut t = std::mem::MaybeUninit::<libc::termios>::uninit();
            // SAFETY: `t` is a valid out pointer for one termios; fd 0 is stdin.
            if unsafe { libc::tcgetattr(libc::STDIN_FILENO, t.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: tcgetattr succeeded, so it initialized `t`.
            let saved = unsafe { t.assume_init() };
            // Only fails when already set, and there is one prompt per process.
            let _ = SAVED.set(saved);
            let handler = restore_and_reraise as extern "C" fn(libc::c_int) as libc::sighandler_t;
            // SAFETY: installs a handler that only calls async-signal-safe functions.
            let previous = SIGNALS.map(|sig| unsafe { libc::signal(sig, handler) });
            let mut quiet = saved;
            quiet.c_lflag &= !libc::ECHO;
            // SAFETY: `quiet` is a valid termios obtained from tcgetattr.
            if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &quiet) } != 0 {
                let err = std::io::Error::last_os_error();
                // Dropping it puts the previous handlers back.
                drop(Self { saved, previous });
                return Err(err);
            }
            Ok(Self { saved, previous })
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::{
                ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
                SetConsoleCtrlHandler, SetConsoleMode,
            };
            // SAFETY: no pointer arguments; returns this process's stdin handle.
            let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            let mut saved = 0u32;
            // SAFETY: `saved` is a valid out pointer; an invalid handle is an error.
            if unsafe { GetConsoleMode(handle, &mut saved) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Only fails when already set, and there is one prompt per process.
            let _ = SAVED.set((handle as usize, saved));
            // SAFETY: registers a handler that only restores the console mode.
            if unsafe { SetConsoleCtrlHandler(Some(restore_on_ctrl), 1) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let hidden = Self { handle, saved };
            // SAFETY: `handle` is the console input handle checked above.
            if unsafe { SetConsoleMode(handle, saved & !ENABLE_ECHO_INPUT) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(hidden)
        }
    }
}

impl Drop for HiddenInput {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: restores the termios read from the same fd in `start`.
            if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved) } != 0 {
                eprintln!(
                    "warning: could not turn terminal echo back on ({}); run `stty echo`",
                    std::io::Error::last_os_error()
                );
            }
            for (sig, previous) in SIGNALS.into_iter().zip(self.previous) {
                // SAFETY: puts back the handler `start` replaced.
                unsafe { libc::signal(sig, previous) };
            }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, SetConsoleMode};
            // SAFETY: restores the mode read from the same handle in `start`.
            if unsafe { SetConsoleMode(self.handle, self.saved) } == 0 {
                eprintln!(
                    "warning: could not turn console echo back on ({})",
                    std::io::Error::last_os_error()
                );
            }
            // SAFETY: unregisters the handler `start` registered.
            unsafe { SetConsoleCtrlHandler(Some(restore_on_ctrl), 0) };
        }
    }
}
