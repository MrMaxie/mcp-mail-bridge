use std::{
  collections::HashSet,
  env, fmt, fs,
  path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::Serialize;
use thiserror::Error;

use crate::permissions::Permission;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Config {
  pub accounts: Vec<AccountConfig>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountConfig {
  pub id: String,
  pub email: String,
  pub provider: Provider,
  pub permissions: Vec<Permission>,
  pub auth: AuthConfig,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthConfig {
  pub kind: AuthKind,
  pub username: Option<String>,
  pub secret: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Provider {
  ImapSmtp,
  Microsoft365,
  Gmail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum AuthKind {
  Password,
  OAuthToken,
  Sso,
}

#[derive(Debug, Error)]
pub enum ConfigError {
  #[error("account id cannot be empty")]
  EmptyAccountId,
  #[error("account email cannot be empty for '{0}'")]
  EmptyEmail(String),
  #[error("account secret cannot be empty for '{0}'")]
  EmptySecret(String),
  #[error("duplicate account id '{0}'")]
  DuplicateAccountId(String),
  #[error("account '{0}' was not found")]
  AccountNotFound(String),
  #[error("account '{0}' has no permissions")]
  MissingPermissions(String),
  #[error("unknown provider '{0}'")]
  UnknownProvider(String),
  #[error("unknown auth kind '{0}'")]
  UnknownAuthKind(String),
  #[error("unknown permission '{0}'")]
  UnknownPermission(String),
}

impl Config {
  pub fn load_or_default(path: &Path) -> Result<Self> {
    let connection = open_database(path)?;
    initialize_database(&connection)?;

    let mut statement = connection
      .prepare(
        "select id, email, provider, auth_kind, auth_username, auth_secret
                 from accounts
                 order by id",
      )
      .with_context(|| format!("failed to query database '{}'", path.display()))?;
    let rows = statement.query_map([], |row| {
      Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, String>(3)?,
        row.get::<_, Option<String>>(4)?,
        row.get::<_, String>(5)?,
      ))
    })?;

    let mut accounts = Vec::new();
    for row in rows {
      let (id, email, provider, auth_kind, username, secret) = row?;
      let permissions = load_permissions(&connection, &id)?;
      accounts.push(AccountConfig {
        id,
        email,
        provider: Provider::parse(&provider)?,
        permissions,
        auth: AuthConfig {
          kind: AuthKind::parse(&auth_kind)?,
          username,
          secret,
        },
      });
    }

    let config = Self { accounts };
    config.validate()?;
    Ok(config)
  }

  pub fn save(&self, path: &Path) -> Result<()> {
    self.validate()?;

    let mut connection = open_database(path)?;
    initialize_database(&connection)?;
    let transaction = connection.transaction()?;

    transaction.execute("delete from account_permissions", [])?;
    transaction.execute("delete from accounts", [])?;

    for account in &self.accounts {
      transaction.execute(
        "insert into accounts (
                   id, email, provider, auth_kind, auth_username, auth_secret
                 ) values (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
          account.id,
          account.email,
          account.provider.to_string(),
          account.auth.kind.to_string(),
          account.auth.username,
          account.auth.secret,
        ],
      )?;

      for permission in &account.permissions {
        transaction.execute(
          "insert into account_permissions (account_id, permission)
                     values (?1, ?2)",
          params![account.id, permission.to_string()],
        )?;
      }
    }

    transaction.commit()?;
    Ok(())
  }

  pub fn validate(&self) -> Result<(), ConfigError> {
    let mut ids = HashSet::new();
    for account in &self.accounts {
      let id = account.id.trim();
      if id.is_empty() {
        return Err(ConfigError::EmptyAccountId);
      }
      if !ids.insert(id.to_owned()) {
        return Err(ConfigError::DuplicateAccountId(id.to_owned()));
      }
      if account.email.trim().is_empty() {
        return Err(ConfigError::EmptyEmail(account.id.clone()));
      }
      if account.auth.secret.trim().is_empty() {
        return Err(ConfigError::EmptySecret(account.id.clone()));
      }
      if account.permissions.is_empty() {
        return Err(ConfigError::MissingPermissions(account.id.clone()));
      }
    }
    Ok(())
  }

  pub fn find_account(&self, id: &str) -> Result<&AccountConfig, ConfigError> {
    self
      .accounts
      .iter()
      .find(|account| account.id == id)
      .ok_or_else(|| ConfigError::AccountNotFound(id.to_owned()))
  }

  pub fn upsert_account(&mut self, account: AccountConfig) -> Result<(), ConfigError> {
    if let Some(existing) = self
      .accounts
      .iter_mut()
      .find(|candidate| candidate.id == account.id)
    {
      *existing = account;
    } else {
      self.accounts.push(account);
    }
    self.validate()
  }

  pub fn remove_account(&mut self, id: &str) -> Result<(), ConfigError> {
    let before = self.accounts.len();
    self.accounts.retain(|account| account.id != id);
    if self.accounts.len() == before {
      return Err(ConfigError::AccountNotFound(id.to_owned()));
    }
    Ok(())
  }
}

impl AccountConfig {
  pub fn allows(&self, permission: Permission) -> bool {
    self.permissions.contains(&permission)
  }

  pub fn permission_list(&self) -> String {
    self
      .permissions
      .iter()
      .map(ToString::to_string)
      .collect::<Vec<_>>()
      .join(",")
  }
}

impl Provider {
  pub fn variants() -> Vec<Self> {
    vec![Self::ImapSmtp, Self::Microsoft365, Self::Gmail]
  }

  fn parse(value: &str) -> Result<Self, ConfigError> {
    match value {
      "imap_smtp" => Ok(Self::ImapSmtp),
      "microsoft365" => Ok(Self::Microsoft365),
      "gmail" => Ok(Self::Gmail),
      _ => Err(ConfigError::UnknownProvider(value.to_owned())),
    }
  }
}

impl fmt::Display for Provider {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::ImapSmtp => formatter.write_str("imap_smtp"),
      Self::Microsoft365 => formatter.write_str("microsoft365"),
      Self::Gmail => formatter.write_str("gmail"),
    }
  }
}

impl AuthKind {
  pub fn variants() -> Vec<Self> {
    vec![Self::Password, Self::OAuthToken, Self::Sso]
  }

  fn parse(value: &str) -> Result<Self, ConfigError> {
    match value {
      "password" => Ok(Self::Password),
      "oauth_token" => Ok(Self::OAuthToken),
      "sso" => Ok(Self::Sso),
      _ => Err(ConfigError::UnknownAuthKind(value.to_owned())),
    }
  }
}

impl fmt::Display for AuthKind {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Password => formatter.write_str("password"),
      Self::OAuthToken => formatter.write_str("oauth_token"),
      Self::Sso => formatter.write_str("sso"),
    }
  }
}

pub fn resolve_database_path(database: Option<PathBuf>) -> Result<PathBuf> {
  if let Some(path) = database {
    return Ok(path);
  }

  let executable = env::current_exe().context("failed to resolve current executable path")?;
  let directory = executable
    .parent()
    .context("failed to resolve executable directory")?;
  Ok(directory.join("mmb.db"))
}

fn open_database(path: &Path) -> Result<Connection> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("failed to create database directory '{}'", parent.display()))?;
  }

  let connection = Connection::open(path)
    .with_context(|| format!("failed to open database '{}'", path.display()))?;
  connection.pragma_update(None, "foreign_keys", true)?;
  Ok(connection)
}

fn initialize_database(connection: &Connection) -> Result<()> {
  connection.execute_batch(
    "create table if not exists accounts (
           id text primary key,
           email text not null,
           provider text not null,
           auth_kind text not null,
           auth_username text,
           auth_secret text not null
         );

         create table if not exists account_permissions (
           account_id text not null,
           permission text not null,
           primary key (account_id, permission),
           foreign key (account_id) references accounts(id) on delete cascade
         );",
  )?;
  Ok(())
}

fn load_permissions(connection: &Connection, account_id: &str) -> Result<Vec<Permission>> {
  let mut statement = connection.prepare(
    "select permission
         from account_permissions
         where account_id = ?1
         order by permission",
  )?;
  let rows = statement.query_map(params![account_id], |row| row.get::<_, String>(0))?;

  let mut permissions = Vec::new();
  for row in rows {
    let permission = row?;
    permissions.push(Permission::parse(&permission).map_err(ConfigError::UnknownPermission)?);
  }

  Ok(permissions)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn account(id: &str) -> AccountConfig {
    AccountConfig {
      id: id.to_owned(),
      email: format!("{id}@example.com"),
      provider: Provider::ImapSmtp,
      permissions: vec![Permission::Read],
      auth: AuthConfig {
        kind: AuthKind::Password,
        username: Some(id.to_owned()),
        secret: "secret".to_owned(),
      },
    }
  }

  #[test]
  fn rejects_duplicate_account_ids() {
    let config = Config {
      accounts: vec![account("work"), account("work")],
    };

    let error = config.validate().unwrap_err().to_string();

    assert_eq!(error, "duplicate account id 'work'");
  }

  #[test]
  fn saves_and_loads_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mmb.db");
    let config = Config {
      accounts: vec![account("work")],
    };

    config.save(&path).unwrap();
    let loaded = Config::load_or_default(&path).unwrap();

    assert_eq!(loaded.accounts[0].id, "work");
  }
}
