//! Subcommands implemented by later releases (sync, autostart). Kept together so each can be replaced in one place.

use clap::Subcommand;
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum LaterCommand {
    /// Pair this machine with a hub.
    Pair(Args),
    /// Hub management.
    Hub(Args),
    /// Install or remove autostart.
    Service(Args),
}

#[derive(clap::Args)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    rest: Vec<String>,
}

pub fn run(cmd: &LaterCommand) -> ExitCode {
    let name = match cmd {
        LaterCommand::Pair(_) => "pair",
        LaterCommand::Hub(_) => "hub",
        LaterCommand::Service(_) => "service",
    };
    eprintln!("blirp {name} is available in a later release");
    ExitCode::from(2)
}
