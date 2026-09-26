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
            let _hidden = HiddenInput::start().context("turning off terminal echo")?;
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

/// Terminal echo of stdin is off while this lives.
struct HiddenInput {
    #[cfg(unix)]
    saved: libc::termios,
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
            let mut quiet = saved;
            quiet.c_lflag &= !libc::ECHO;
            // SAFETY: `quiet` is a valid termios obtained from tcgetattr.
            if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &quiet) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { saved })
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::{
                ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
            };
            // SAFETY: no pointer arguments; returns this process's stdin handle.
            let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            let mut saved = 0u32;
            // SAFETY: `saved` is a valid out pointer; an invalid handle is an error.
            if unsafe { GetConsoleMode(handle, &mut saved) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `handle` is the console input handle checked above.
            if unsafe { SetConsoleMode(handle, saved & !ENABLE_ECHO_INPUT) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { handle, saved })
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
        }
        #[cfg(windows)]
        {
            // SAFETY: restores the mode read from the same handle in `start`.
            if unsafe {
                windows_sys::Win32::System::Console::SetConsoleMode(self.handle, self.saved)
            } == 0
            {
                eprintln!(
                    "warning: could not turn console echo back on ({})",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}
