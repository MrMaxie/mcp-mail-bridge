use std::{
  fmt,
  path::{Path, PathBuf},
  thread,
  time::{Duration, Instant},
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use inquire::{Confirm, MultiSelect, Password, Select, Text};
use reqwest::{StatusCode, blocking::Client};
use serde::{Deserialize, Serialize};

use crate::config::{AccountConfig, AuthConfig, AuthKind, Config, Provider};
use crate::mail;
use crate::permissions::Permission;

const GMAIL_DEVICE_CODE_URL: &str = "https://oauth2.googleapis.com/device/code";
const GMAIL_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GMAIL_SCOPE: &str =
  "https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/gmail.send";

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
  let secret = prompt_secret(provider, auth_kind, &email)?;
  if provider == Provider::Gmail && auth_kind == AuthKind::OAuthToken {
    mail::validate_gmail_identity_blocking(&email, &secret)?;
  }
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

#[derive(Debug, Clone, Copy)]
enum GmailOAuthMode {
  DeviceLogin,
  PasteSecret,
}

impl fmt::Display for GmailOAuthMode {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::DeviceLogin => formatter.write_str("Run Gmail device OAuth login"),
      Self::PasteSecret => formatter.write_str("Paste existing token or token bundle"),
    }
  }
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
  device_code: String,
  user_code: String,
  verification_url: String,
  expires_in: u64,
  interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenResponse {
  access_token: String,
  refresh_token: Option<String>,
  expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
  error: String,
  error_description: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OAuthTokenBundle {
  access_token: Option<String>,
  refresh_token: String,
  client_id: String,
  client_secret: String,
  token_uri: Option<String>,
  expires_at_unix: Option<i64>,
}

fn prompt_secret(provider: Provider, auth_kind: AuthKind, email: &str) -> Result<String> {
  if provider != Provider::Gmail || auth_kind != AuthKind::OAuthToken {
    return Password::new("Secret or token")
      .without_confirmation()
      .prompt()
      .map_err(Into::into);
  }

  let mode = Select::new(
    "Gmail OAuth setup",
    vec![GmailOAuthMode::DeviceLogin, GmailOAuthMode::PasteSecret],
  )
  .prompt()?;

  match mode {
    GmailOAuthMode::DeviceLogin => run_gmail_device_login(email),
    GmailOAuthMode::PasteSecret => Password::new(
      "OAuth access token or token bundle JSON; bundles without expires_at_unix refresh immediately",
    )
    .without_confirmation()
    .prompt()
    .map_err(Into::into),
  }
}

fn run_gmail_device_login(_email: &str) -> Result<String> {
  let client_id = Text::new("Google OAuth client id").prompt()?;
  let client_secret = Password::new("Google OAuth client secret")
    .without_confirmation()
    .prompt()?;
  let client = Client::builder().timeout(Duration::from_secs(15)).build()?;
  let device = client
    .post(GMAIL_DEVICE_CODE_URL)
    .form(&[("client_id", client_id.as_str()), ("scope", GMAIL_SCOPE)])
    .send()?
    .error_for_status()?
    .json::<DeviceCodeResponse>()?;

  println!("Open this URL locally: {}", device.verification_url);
  println!("Enter this code: {}", device.user_code);
  println!("Waiting for Google authorization to complete...");

  let started = Instant::now();
  let mut interval = Duration::from_secs(device.interval.unwrap_or(5).max(1));
  loop {
    if started.elapsed() >= Duration::from_secs(device.expires_in) {
      anyhow::bail!("Gmail OAuth device code expired before authorization completed");
    }

    thread::sleep(interval);
    let response = client
      .post(GMAIL_TOKEN_URL)
      .form(&[
        ("client_id", client_id.as_str()),
        ("client_secret", client_secret.as_str()),
        ("device_code", device.device_code.as_str()),
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
      ])
      .send()?;

    if response.status() == StatusCode::OK {
      let token = response.json::<DeviceTokenResponse>()?;
      let refresh_token = token.refresh_token.ok_or_else(|| {
        anyhow::anyhow!("Google did not return a refresh token; retry with offline access enabled")
      })?;
      let expires_at_unix = token
        .expires_in
        .map(|seconds| unix_timestamp() + seconds as i64);
      let bundle = OAuthTokenBundle {
        access_token: Some(token.access_token),
        refresh_token,
        client_id,
        client_secret,
        token_uri: Some(GMAIL_TOKEN_URL.to_owned()),
        expires_at_unix,
      };
      return serde_json::to_string(&bundle).map_err(Into::into);
    }

    let status = response.status();
    let error = response.json::<OAuthErrorResponse>().ok();
    match error.as_ref().map(|error| error.error.as_str()) {
      Some("authorization_pending") => {}
      Some("slow_down") => interval += Duration::from_secs(5),
      Some("access_denied") => anyhow::bail!("Gmail OAuth authorization was denied"),
      Some("expired_token") => anyhow::bail!("Gmail OAuth device code expired"),
      Some(other) => {
        let description = error
          .as_ref()
          .and_then(|error| error.error_description.as_deref())
          .unwrap_or("no additional description");
        anyhow::bail!("Gmail OAuth failed with redacted error '{other}': {description}");
      }
      None => anyhow::bail!("Gmail OAuth failed with HTTP status {status}"),
    }
  }
}

fn unix_timestamp() -> i64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs() as i64
}
