//! `leveler completions <shell>` — emit a shell completion script.
//!
//! The script is generated from the same clap definition the binary parses
//! with, so it cannot drift from the real subcommands and flags. Output goes to
//! stdout alone: a shell sources this file, and any banner or log line would be
//! a syntax error there.

use std::io;

use clap::CommandFactory;
use clap_complete::Shell;

use crate::cli::Cli;

pub fn cmd_completions(shell: Shell) -> anyhow::Result<std::process::ExitCode> {
    let mut command = Cli::command();
    let bin_name = command.get_name().to_string();
    clap_complete::generate(shell, &mut command, bin_name, &mut io::stdout());
    Ok(std::process::ExitCode::SUCCESS)
}
