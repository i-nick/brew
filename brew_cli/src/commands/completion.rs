use clap::{CommandFactory, Parser};
use clap_complete::generate;
use std::io;

#[derive(Parser)]
#[command(name = "b")]
#[command(about = "brew - A fast package manager for Apple Silicon Macs")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    command: crate::cli::Commands,
}

pub fn execute(shell: clap_complete::shells::Shell) -> Result<(), brew_core::Error> {
    let mut cmd = crate::cli::Cli::command();
    generate(shell, &mut cmd, "b", &mut io::stdout());
    Ok(())
}
