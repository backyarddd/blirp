//! Subcommands implemented by later releases (memory CLI, hooks, MCP,
//! sync). Kept together so each can be replaced in one place.

use clap::Subcommand;
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum LaterCommand {
    /// Search and show project memory.
    Mem(Args),
    /// Hook entry point used by agents.
    Hook(Args),
    /// MCP server over stdio.
    Mcp(Args),
    /// Pair this machine with a hub.
    Pair(Args),
    /// Hub management.
    Hub(Args),
    /// Install or remove global agent hooks.
    Hooks(Args),
}

#[derive(clap::Args)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    rest: Vec<String>,
}

pub fn run(cmd: &LaterCommand) -> ExitCode {
    let name = match cmd {
        LaterCommand::Mem(_) => "mem",
        LaterCommand::Hook(_) => "hook",
        LaterCommand::Mcp(_) => "mcp",
        LaterCommand::Pair(_) => "pair",
        LaterCommand::Hub(_) => "hub",
        LaterCommand::Hooks(_) => "hooks",
    };
    eprintln!("blirp {name} is available in a later release");
    ExitCode::from(2)
}
