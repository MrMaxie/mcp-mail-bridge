use std::{
  collections::HashSet,
  env, fmt, fs,
  path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
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

const CURRENT_SCHEMA_VERSION: i64 = 2;

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

    let desired_account_ids: HashSet<String> =
      self.accounts.iter().map(|account| account.id.clone()).collect();
    let mut statement = transaction.prepare("select id from accounts")?;
    let existing_account_ids = statement.query_map([], |row| row.get::<_, String>(0))?;
    for existing_account_id in existing_account_ids {
      let existing_account_id = existing_account_id?;
      if !desired_account_ids.contains(&existing_account_id) {
        transaction.execute("delete from accounts where id = ?1", params![existing_account_id])?;
      }
    }

    for account in &self.accounts {
      transaction.execute(
        "insert into accounts (
                   id, email, provider, auth_kind, auth_username, auth_secret
                 ) values (?1, ?2, ?3, ?4, ?5, ?6)
                 on conflict(id) do update set
                   email = excluded.email,
                   provider = excluded.provider,
                   auth_kind = excluded.auth_kind,
                   auth_username = excluded.auth_username,
                   auth_secret = excluded.auth_secret",
        params![
          account.id,
          account.email,
          account.provider.to_string(),
          account.auth.kind.to_string(),
          account.auth.username,
          account.auth.secret,
        ],
      )?;

      transaction.execute(
        "delete from account_permissions where account_id = ?1",
        params![account.id],
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
  let version = detect_schema_version(connection)?;
  match version {
    0 => initialize_fresh_schema(connection)?,
    1 => {
      migrate_schema_to_v2(connection)?;
    }
    CURRENT_SCHEMA_VERSION => {}
    other => {
      anyhow::bail!("unsupported database schema version '{other}'")
    }
  }
  Ok(())
}

fn detect_schema_version(connection: &Connection) -> Result<i64> {
  if !table_exists(connection, "schema_migrations")? {
    if table_exists(connection, "accounts")? {
      return Ok(1);
    }
    return Ok(0);
  }

  let version = connection
    .query_row("select version from schema_migrations", [], |row| {
      row.get::<_, i64>(0)
    })
    .optional()?
    .unwrap_or(0);

  Ok(version)
}

fn initialize_fresh_schema(connection: &Connection) -> Result<()> {
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
         );

         create table if not exists message_metadata_cache (
           account_id text not null,
           cache_key text not null,
           message_id text not null,
           subject text,
           sender text,
           recipients text,
           snippet text,
           received_at integer,
           is_read integer not null default 0,
           remote_version text,
           cached_at integer not null,
           primary key (account_id, cache_key, message_id),
           foreign key (account_id) references accounts(id) on delete cascade
         );

         create table if not exists message_body_cache (
           account_id text not null,
           cache_key text,
           message_id text not null,
           mime_type text,
           body text,
           fetched_at integer,
           primary key (account_id, message_id),
           foreign key (account_id) references accounts(id) on delete cascade
         );

         create table if not exists cache_windows (
           account_id text not null,
           cache_key text not null,
           query text,
           from_timestamp integer,
           to_timestamp integer,
           read_state text,
           cursor text,
           refreshed_at integer not null,
           primary key (account_id, cache_key),
           foreign key (account_id) references accounts(id) on delete cascade
         );

         create table if not exists message_remote_state (
           account_id text not null,
           message_id text not null,
           is_read integer not null,
           remote_state integer,
           marker text,
           updated_at integer not null,
           primary key (account_id, message_id),
           foreign key (account_id) references accounts(id) on delete cascade
         );

         create table if not exists sync_markers (
           account_id text not null,
           marker_key text not null,
           marker_value text not null,
           updated_at integer not null,
           primary key (account_id, marker_key),
           foreign key (account_id) references accounts(id) on delete cascade
         );

         create table if not exists schema_migrations (
           version integer not null primary key
         );

         insert into schema_migrations (version) values (2);",
  )?;
  Ok(())
}

fn migrate_schema_to_v2(connection: &Connection) -> Result<()> {
  connection.execute_batch(
    "create table if not exists message_metadata_cache (
           account_id text not null,
           cache_key text not null,
           message_id text not null,
           subject text,
           sender text,
           recipients text,
           snippet text,
           received_at integer,
           is_read integer not null default 0,
           remote_version text,
           cached_at integer not null,
           primary key (account_id, cache_key, message_id),
           foreign key (account_id) references accounts(id) on delete cascade
         );

         create table if not exists message_body_cache (
           account_id text not null,
           cache_key text,
           message_id text not null,
           mime_type text,
           body text,
           fetched_at integer,
           primary key (account_id, message_id),
           foreign key (account_id) references accounts(id) on delete cascade
         );

         create table if not exists cache_windows (
           account_id text not null,
           cache_key text not null,
           query text,
           from_timestamp integer,
           to_timestamp integer,
           read_state text,
           cursor text,
           refreshed_at integer not null,
           primary key (account_id, cache_key),
           foreign key (account_id) references accounts(id) on delete cascade
         );

         create table if not exists message_remote_state (
           account_id text not null,
           message_id text not null,
           is_read integer not null,
           remote_state integer,
           marker text,
           updated_at integer not null,
           primary key (account_id, message_id),
           foreign key (account_id) references accounts(id) on delete cascade
         );

         create table if not exists sync_markers (
           account_id text not null,
           marker_key text not null,
           marker_value text not null,
           updated_at integer not null,
           primary key (account_id, marker_key),
           foreign key (account_id) references accounts(id) on delete cascade
         );
         create table if not exists schema_migrations (
           version integer not null primary key
         );

         delete from schema_migrations;
         insert into schema_migrations (version) values (2);",
  )?;
  Ok(())
}

fn table_exists(connection: &Connection, table_name: &str) -> Result<bool> {
  let exists: Option<String> = connection
    .query_row(
      "select name from sqlite_master where type='table' and name = ?1",
      params![table_name],
      |row| row.get(0),
    )
    .optional()?;
  Ok(exists.is_some())
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

  #[test]
  fn loads_fresh_schema_version_2() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mmb.db");

    let config = Config {
      accounts: vec![account("work")],
    };
    config.save(&path).unwrap();

    let connection = Connection::open(&path).unwrap();
    let version: i64 = connection
      .query_row("select version from schema_migrations", [], |row| {
        row.get(0)
      })
      .unwrap();

    assert_eq!(version, CURRENT_SCHEMA_VERSION);
  }

  #[test]
  fn upgrades_legacy_schema_to_current_version() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mmb_legacy.db");

    let connection = Connection::open(&path).unwrap();
    connection
      .execute_batch(
        "create table accounts (
           id text primary key,
           email text not null,
           provider text not null,
           auth_kind text not null,
           auth_username text,
           auth_secret text not null
         );

         create table account_permissions (
           account_id text not null,
           permission text not null,
           primary key (account_id, permission),
           foreign key (account_id) references accounts(id) on delete cascade
         );",
      )
      .unwrap();

    connection
      .execute(
        "insert into accounts (id, email, provider, auth_kind, auth_username, auth_secret)
         values (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
          "work",
          "work@example.com",
          "imap_smtp",
          "password",
          "work",
          "secret"
        ],
      )
      .unwrap();
    connection
      .execute(
        "insert into account_permissions (account_id, permission) values (?1, ?2)",
        params!["work", "read"],
      )
      .unwrap();
    drop(connection);

    let config = Config::load_or_default(&path).unwrap();
    assert_eq!(config.accounts[0].id, "work");

    let connection = Connection::open(&path).unwrap();
    let version: i64 = connection
      .query_row("select version from schema_migrations", [], |row| {
        row.get(0)
      })
      .unwrap();
    assert_eq!(version, CURRENT_SCHEMA_VERSION);

    let cache_count: i64 = connection
      .query_row(
        "select count(name) from sqlite_master where type='table' and name = 'message_metadata_cache'",
        [],
        |row| row.get(0),
      )
      .unwrap();
    assert_eq!(cache_count, 1);
  }

  #[test]
  fn preserves_other_accounts_cache_data_during_save() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mmb_cache.db");

    let config = Config {
      accounts: vec![account("work"), account("personal")],
    };
    config.save(&path).unwrap();

    let connection = Connection::open(&path).unwrap();
    connection
      .execute(
        "insert into message_metadata_cache (
            account_id, cache_key, message_id, cached_at
         ) values (?1, ?2, ?3, ?4)",
        params!["work", "q-work", "work-message", 100],
      )
      .unwrap();
    connection
      .execute(
        "insert into message_metadata_cache (
            account_id, cache_key, message_id, cached_at
         ) values (?1, ?2, ?3, ?4)",
        params!["personal", "q-personal", "personal-message", 100],
      )
      .unwrap();
    drop(connection);

    let mut updated = config.clone();
    updated.accounts[0].email = "work-updated@example.com".to_string();
    updated.save(&path).unwrap();

    let connection = Connection::open(&path).unwrap();
    let work_cache_count: i64 = connection
      .query_row(
        "select count(*) from message_metadata_cache where account_id = ?1",
        ["work"],
        |row| row.get(0),
      )
      .unwrap();
    let personal_cache_count: i64 = connection
      .query_row(
        "select count(*) from message_metadata_cache where account_id = ?1",
        ["personal"],
        |row| row.get(0),
      )
      .unwrap();

    assert_eq!(work_cache_count, 1);
    assert_eq!(personal_cache_count, 1);
  }
}
