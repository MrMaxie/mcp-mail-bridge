mod cli;
mod config;
mod mail;
mod mcp;
mod permissions;
mod tui;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
  init_tracing();

  let cli = Cli::parse();
  let database_path = config::resolve_database_path(cli.database)?;

  match cli.command.unwrap_or(Command::Serve) {
    Command::Serve => mcp::serve(database_path).await,
    Command::Config(command) => cli::run_config_command(database_path, command),
    Command::Tui => tui::run(database_path),
  }
}

fn init_tracing() {
  let _ = tracing_subscriber::fmt()
    .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
    .with_writer(std::io::stderr)
    .try_init();
}
