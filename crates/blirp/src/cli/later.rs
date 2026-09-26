//! Subcommands implemented by later releases (autostart). Kept together so
//! each can be replaced in one place.

use clap::Subcommand;
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum LaterCommand {
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
        LaterCommand::Service(_) => "service",
    };
    eprintln!("blirp {name} is available in a later release");
    ExitCode::from(2)
}
