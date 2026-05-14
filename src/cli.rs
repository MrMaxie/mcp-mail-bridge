use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use inquire::{Confirm, MultiSelect, Password, Select, Text};

use crate::config::{AccountConfig, AuthConfig, AuthKind, Config, Provider};
use crate::permissions::Permission;

#[derive(Debug, Parser)]
#[command(version, about = "MCP stdio bridge for configured mail accounts")]
pub struct Cli {
  #[arg(long, global = true, value_name = "PATH")]
  pub database: Option<PathBuf>,

  #[command(subcommand)]
  pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
  Serve,
  #[command(subcommand)]
  Config(ConfigCommand),
  Tui,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
  List,
  Add,
  Remove { id: String },
  Edit { id: String },
  Check,
}

pub fn run_config_command(database_path: PathBuf, command: ConfigCommand) -> Result<()> {
  match command {
    ConfigCommand::List => list_accounts(&database_path),
    ConfigCommand::Add => add_account(&database_path),
    ConfigCommand::Remove { id } => remove_account(&database_path, &id),
    ConfigCommand::Edit { id } => edit_account(&database_path, &id),
    ConfigCommand::Check => check_database(&database_path),
  }
}

pub fn list_accounts(database_path: &Path) -> Result<()> {
  let config = Config::load_or_default(database_path)?;
  if config.accounts.is_empty() {
    println!("No accounts configured.");
    return Ok(());
  }

  for account in config.accounts {
    println!(
      "{} <{}> provider={} permissions={}",
      account.id,
      account.email,
      account.provider,
      account.permission_list()
    );
  }

  Ok(())
}

pub fn check_database(database_path: &Path) -> Result<()> {
  let config = Config::load_or_default(database_path)?;
  config.validate()?;
  println!("Database is valid: {}", database_path.display());
  Ok(())
}

pub fn add_account(database_path: &Path) -> Result<()> {
  let mut config = Config::load_or_default(database_path)?;
  let account = prompt_account(None)?;
  config.upsert_account(account)?;
  config.save(database_path)?;
  println!("Account saved.");
  Ok(())
}

pub fn edit_account(database_path: &Path, id: &str) -> Result<()> {
  let mut config = Config::load_or_default(database_path)?;
  let existing = config.find_account(id)?.clone();
  let account = prompt_account(Some(existing))?;
  config.upsert_account(account)?;
  config.save(database_path)?;
  println!("Account updated.");
  Ok(())
}

pub fn remove_account(database_path: &Path, id: &str) -> Result<()> {
  let mut config = Config::load_or_default(database_path)?;
  let should_remove = Confirm::new(&format!("Remove account '{id}'?"))
    .with_default(false)
    .prompt()?;
  if !should_remove {
    println!("No changes made.");
    return Ok(());
  }

  config.remove_account(id)?;
  config.save(database_path)?;
  println!("Account removed.");
  Ok(())
}

pub fn prompt_account(existing: Option<AccountConfig>) -> Result<AccountConfig> {
  let existing_id = existing
    .as_ref()
    .map(|account| account.id.as_str())
    .unwrap_or("");
  let existing_email = existing
    .as_ref()
    .map(|account| account.email.as_str())
    .unwrap_or("");
  let existing_username = existing
    .as_ref()
    .and_then(|account| account.auth.username.as_deref())
    .unwrap_or(existing_email);

  let id = Text::new("Account id (local alias, e.g. work or gmail-main)")
    .with_initial_value(existing_id)
    .prompt()?;
  let email = Text::new("Email")
    .with_initial_value(existing_email)
    .prompt()?;
  let provider = Select::new("Provider", Provider::variants()).prompt()?;
  let auth_kind = Select::new("Auth kind", AuthKind::variants()).prompt()?;
  let username = Text::new("Username")
    .with_initial_value(existing_username)
    .prompt()?;
  let secret = Password::new("Secret or token")
    .without_confirmation()
    .prompt()?;
  let permissions = MultiSelect::new("Permissions", Permission::variants()).prompt()?;

  Ok(AccountConfig {
    id,
    email,
    provider,
    permissions,
    auth: AuthConfig {
      kind: auth_kind,
      username: Some(username),
      secret,
    },
  })
}
