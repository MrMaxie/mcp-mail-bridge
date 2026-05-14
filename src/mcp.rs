use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use anyhow::Result;
use rmcp::{
  ServerHandler, ServiceExt,
  handler::server::{router::tool::ToolRouter, wrapper::Parameters},
  model::{ServerCapabilities, ServerInfo},
  schemars, tool, tool_handler, tool_router,
  transport::stdio,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
  config::{self, AccountConfig, CachedMessageSummary, Config},
  mail,
  permissions::Permission,
};

#[derive(Debug, Clone)]
pub struct MailBridgeServer {
  database_path: PathBuf,
  tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListMessagesRequest {
  pub account_id: String,
  pub query: Option<String>,
  pub label: Option<String>,
  pub start_unix: Option<i64>,
  pub end_unix: Option<i64>,
  pub read_state: Option<String>,
  pub limit: Option<u32>,
  pub page_token: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadMessageRequest {
  pub account_id: String,
  pub message_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendMessageRequest {
  pub account_id: String,
  pub to: String,
  pub cc: Option<String>,
  pub bcc: Option<String>,
  pub subject: String,
  pub body: String,
  pub body_format: Option<String>,
}

#[derive(Debug, Serialize)]
struct AccountSummary {
  id: String,
  email: String,
  provider: String,
  permissions: Vec<String>,
}

impl MailBridgeServer {
  pub fn new(database_path: PathBuf) -> Self {
    Self {
      database_path,
      tool_router: Self::tool_router(),
    }
  }

  fn load_config(&self) -> Result<Config, String> {
    Config::load_or_default(&self.database_path).map_err(|error| error.to_string())
  }

  fn account_for_request(
    &self,
    account_id: &str,
    permission: Permission,
  ) -> Result<AccountConfig, String> {
    let config = self.load_config()?;
    let account = config
      .find_account(account_id)
      .map_err(|error| error.to_string())?
      .clone();

    if !account.allows(permission) {
      return Err(format!(
        "account '{account_id}' does not allow '{permission}'"
      ));
    }

    Ok(account)
  }

  async fn adapter_for_account(
    account: &AccountConfig,
  ) -> Result<Box<dyn mail::MailAdapter>, String> {
    let adapter = mail::adapter_for(account).map_err(|error| error.to_string())?;
    adapter
      .validate_account()
      .await
      .map_err(|error| error.to_string())?;
    Ok(adapter)
  }

  async fn adapter_for_request(
    &self,
    account_id: &str,
    permission: Permission,
  ) -> Result<Box<dyn mail::MailAdapter>, String> {
    let account = self.account_for_request(account_id, permission)?;
    Self::adapter_for_account(&account).await
  }

  fn serialize_response<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|error| {
      json!({
        "error": format!("failed to serialize response: {error}")
      })
      .to_string()
    })
  }

  fn parse_read_state(read_state: Option<&str>) -> Result<Option<mail::ReadStateFilter>, String> {
    read_state
      .map(|value| match value {
        "read" => Ok(mail::ReadStateFilter::Read),
        "unread" => Ok(mail::ReadStateFilter::Unread),
        other => Err(format!("read_state '{other}' is not supported")),
      })
      .transpose()
  }

  fn cache_key(
    account_id: &str,
    query: Option<&str>,
    label: Option<&str>,
    window: &mail::MessageDateWindow,
    read_state: Option<mail::ReadStateFilter>,
    limit: Option<u32>,
    page_token: Option<&str>,
  ) -> String {
    json!({
      "account_id": account_id,
      "query": query.unwrap_or_default(),
      "label": label.unwrap_or_default(),
      "start_unix": window.start_unix,
      "end_unix": window.end_unix,
      "read_state": read_state.map(|state| state.as_cache_value()),
      "limit": mail::enforce_request_limit(limit),
      "page_token": page_token.unwrap_or_default(),
    })
    .to_string()
  }

  fn mail_summary_to_cache(summary: &mail::MessageSummary) -> CachedMessageSummary {
    CachedMessageSummary {
      id: summary.id.clone(),
      thread_id: summary.thread_id.clone(),
      remote_version: summary.remote_version.clone(),
      subject: summary.subject.clone(),
      sender: summary.sender.clone(),
      recipients: summary.recipients.clone(),
      snippet: summary.snippet.clone(),
      received_at: summary.received_at,
      is_read: summary.is_read,
      labels: summary.labels.clone(),
    }
  }

  fn cached_summary_to_mail(summary: CachedMessageSummary) -> mail::MessageSummary {
    mail::MessageSummary {
      id: summary.id,
      thread_id: summary.thread_id,
      remote_version: summary.remote_version,
      subject: summary.subject,
      sender: summary.sender,
      recipients: summary.recipients,
      snippet: summary.snippet,
      received_at: summary.received_at,
      is_read: summary.is_read,
      labels: summary.labels,
      source: "gmail-cache".to_owned(),
    }
  }

  fn refresh_cached_message_content(
    mut content: mail::MessageContent,
    summary: &mail::MessageSummary,
  ) -> mail::MessageContent {
    content.thread_id = summary.thread_id.clone();
    content.subject = summary.subject.clone();
    content.sender = summary.sender.clone();
    content.recipients = summary.recipients.clone();
    content.snippet = summary.snippet.clone();
    content.received_at = summary.received_at;
    content.is_read = summary.is_read;
    content.labels = summary.labels.clone();
    content.source = summary.source.clone();
    content
  }

  async fn run_with_adapter<T, F>(
    &self,
    account_id: &str,
    permission: Permission,
    action: F,
  ) -> String
  where
    F: for<'a> FnOnce(
      &'a dyn mail::MailAdapter,
    ) -> Pin<Box<dyn Future<Output = Result<T, mail::MailError>> + Send + 'a>>,
    T: Serialize,
  {
    let config = match self.load_config() {
      Ok(config) => config,
      Err(error) => return json!({ "error": error }).to_string(),
    };

    let account = match config.find_account(account_id) {
      Ok(account) => account,
      Err(error) => return json!({ "error": error.to_string() }).to_string(),
    };

    if !account.allows(permission) {
      return json!({
        "error": format!("account '{account_id}' does not allow '{permission}'")
      })
      .to_string();
    }

    let adapter = match mail::adapter_for(account) {
      Ok(adapter) => adapter,
      Err(error) => return json!({ "error": error.to_string() }).to_string(),
    };

    if let Err(error) = adapter.validate_account().await {
      return json!({ "error": error.to_string() }).to_string();
    }

    match action(adapter.as_ref()).await {
      Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|error| {
        json!({
          "error": format!("failed to serialize response: {error}")
        })
        .to_string()
      }),
      Err(error) => json!({ "error": error.to_string() }).to_string(),
    }
  }
}

fn filter_by_read_state(
  messages: Vec<mail::MessageSummary>,
  read_state: Option<mail::ReadStateFilter>,
) -> Vec<mail::MessageSummary> {
  match read_state {
    Some(mail::ReadStateFilter::Read) => messages
      .into_iter()
      .filter(|message| message.is_read)
      .collect(),
    Some(mail::ReadStateFilter::Unread) => messages
      .into_iter()
      .filter(|message| !message.is_read)
      .collect(),
    None => messages,
  }
}

fn filter_by_window(
  messages: Vec<mail::MessageSummary>,
  window: &mail::MessageDateWindow,
) -> Vec<mail::MessageSummary> {
  messages
    .into_iter()
    .filter(|message| {
      let Some(received_at) = message.received_at else {
        return false;
      };
      window
        .start_unix
        .map(|start| received_at >= start)
        .unwrap_or(true)
        && window
          .end_unix
          .map(|end| received_at <= end)
          .unwrap_or(true)
    })
    .collect()
}

#[tool_router]
impl MailBridgeServer {
  #[tool(description = "List configured mail accounts without exposing secrets")]
  fn list_accounts(&self) -> String {
    let result = self.load_config().and_then(|config| {
      let accounts = config
        .accounts
        .into_iter()
        .map(|account| AccountSummary {
          id: account.id,
          email: account.email,
          provider: account.provider.to_string(),
          permissions: account
            .permissions
            .into_iter()
            .map(|permission| permission.to_string())
            .collect(),
        })
        .collect::<Vec<_>>();
      serde_json::to_string_pretty(&accounts).map_err(|error| error.to_string())
    });

    result.unwrap_or_else(|error| json!({ "error": error }).to_string())
  }

  #[tool(description = "List messages for an account.")]
  async fn list_messages(&self, Parameters(request): Parameters<ListMessagesRequest>) -> String {
    let window = match mail::MessageDateWindow::new(request.start_unix, request.end_unix)
      .bounded_or_default()
    {
      Ok(window) => window,
      Err(error) => return json!({ "error": error.to_string() }).to_string(),
    };
    let read_state = match Self::parse_read_state(request.read_state.as_deref()) {
      Ok(read_state) => read_state,
      Err(error) => return json!({ "error": error }).to_string(),
    };
    let cache_key = Self::cache_key(
      &request.account_id,
      request.query.as_deref(),
      request.label.as_deref(),
      &window,
      read_state,
      request.limit,
      request.page_token.as_deref(),
    );

    let adapter = match self
      .adapter_for_request(&request.account_id, Permission::Search)
      .await
    {
      Ok(adapter) => adapter,
      Err(error) => return json!({ "error": error }).to_string(),
    };

    match adapter
      .list_messages(mail::ListMessagesRequest {
        query: request.query.clone(),
        label: request.label.clone(),
        window: Some(window.clone()),
        read_state,
        limit: request.limit,
        page_token: request.page_token.clone(),
      })
      .await
    {
      Ok(mut message_list) => {
        message_list.messages = filter_by_read_state(message_list.messages, read_state);
        message_list.messages = filter_by_window(message_list.messages, &window);
        let summaries = message_list
          .messages
          .iter()
          .map(Self::mail_summary_to_cache)
          .collect::<Vec<_>>();
        if let Err(error) = config::save_cached_message_summaries(
          &self.database_path,
          &request.account_id,
          &cache_key,
          config::CacheWindow {
            query: request.query.as_deref(),
            label: request.label.as_deref(),
            from_timestamp: window.start_unix,
            to_timestamp: window.end_unix,
            read_state: read_state.map(|state| state.as_cache_value()),
            cursor: message_list.next_page_token.as_deref(),
          },
          &summaries,
        ) {
          return json!({ "error": error.to_string() }).to_string();
        }
        Self::serialize_response(&message_list)
      }
      Err(error) => match config::load_cached_message_summaries(
        &self.database_path,
        &request.account_id,
        &cache_key,
      ) {
        Ok(Some(cached)) => Self::serialize_response(&mail::MessageList {
          messages: cached
            .messages
            .into_iter()
            .map(Self::cached_summary_to_mail)
            .collect(),
          next_page_token: cached.next_page_token,
        }),
        Ok(None) | Err(_) => json!({ "error": error.to_string() }).to_string(),
      },
    }
  }

  #[tool(description = "Read one message for an account.")]
  async fn read_message(&self, Parameters(request): Parameters<ReadMessageRequest>) -> String {
    let account = match self.account_for_request(&request.account_id, Permission::Read) {
      Ok(account) => account,
      Err(error) => return json!({ "error": error }).to_string(),
    };
    let adapter = match Self::adapter_for_account(&account).await {
      Ok(adapter) => adapter,
      Err(error) => return json!({ "error": error }).to_string(),
    };

    match config::load_cached_message_body(
      &self.database_path,
      &request.account_id,
      &request.message_id,
    ) {
      Ok(Some(body_json)) => {
        if let Ok(mut message) = serde_json::from_str::<mail::MessageContent>(&body_json) {
          match adapter
            .refresh_message_summaries(vec![request.message_id.clone()])
            .await
          {
            Ok(mut summaries) => {
              if let Some(summary) = summaries.pop() {
                message = Self::refresh_cached_message_content(message, &summary);
              }
            }
            Err(mail::MailError::NotFound) => {
              return json!({ "error": mail::MailError::NotFound.to_string() }).to_string();
            }
            Err(_) => return Self::serialize_response(&message),
          }

          let body_json = match serde_json::to_string(&message) {
            Ok(body_json) => body_json,
            Err(error) => return json!({ "error": error.to_string() }).to_string(),
          };
          if let Err(error) = config::save_cached_message_body(
            &self.database_path,
            &request.account_id,
            None,
            &message.id,
            &body_json,
          ) {
            return json!({ "error": error.to_string() }).to_string();
          }
          return Self::serialize_response(&message);
        }
      }
      Ok(None) => {}
      Err(error) => return json!({ "error": error.to_string() }).to_string(),
    }

    match adapter
      .read_message(mail::ReadMessageRequest {
        message_id: request.message_id.clone(),
      })
      .await
    {
      Ok(message) => {
        let body_json = match serde_json::to_string(&message) {
          Ok(body_json) => body_json,
          Err(error) => return json!({ "error": error.to_string() }).to_string(),
        };
        if let Err(error) = config::save_cached_message_body(
          &self.database_path,
          &request.account_id,
          None,
          &message.id,
          &body_json,
        ) {
          return json!({ "error": error.to_string() }).to_string();
        }
        Self::serialize_response(&message)
      }
      Err(error) => json!({ "error": error.to_string() }).to_string(),
    }
  }

  #[tool(description = "Send a message from an account.")]
  async fn send_message(&self, Parameters(request): Parameters<SendMessageRequest>) -> String {
    let message = mail::SendMessageRequest {
      to: request.to,
      cc: request.cc,
      bcc: request.bcc,
      subject: request.subject,
      body: request.body,
      body_format: request.body_format,
    };
    self
      .run_with_adapter(&request.account_id, Permission::Send, move |adapter| {
        Box::pin(async move { adapter.send_message(message).await })
      })
      .await
  }

  #[tool(description = "Mark a message as read for an account.")]
  async fn mark_as_read(&self, Parameters(request): Parameters<ReadMessageRequest>) -> String {
    self
      .set_read_state(request, true, Permission::MarkAsRead)
      .await
  }

  #[tool(description = "Mark a message as unread for an account.")]
  async fn mark_as_unread(&self, Parameters(request): Parameters<ReadMessageRequest>) -> String {
    self
      .set_read_state(request, false, Permission::MarkAsUnread)
      .await
  }
}

impl MailBridgeServer {
  async fn set_read_state(
    &self,
    request: ReadMessageRequest,
    is_read: bool,
    permission: Permission,
  ) -> String {
    let adapter = match self
      .adapter_for_request(&request.account_id, permission)
      .await
    {
      Ok(adapter) => adapter,
      Err(error) => return json!({ "error": error }).to_string(),
    };

    match adapter
      .set_read_state(mail::SetReadStateRequest {
        message_id: request.message_id.clone(),
        is_read,
      })
      .await
    {
      Ok(result) => {
        if let Err(error) = config::update_cached_message_state(
          &self.database_path,
          &request.account_id,
          &request.message_id,
          is_read,
        ) {
          return json!({ "error": error.to_string() }).to_string();
        }
        Self::serialize_response(&result)
      }
      Err(error) => json!({ "error": error.to_string() }).to_string(),
    }
  }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MailBridgeServer {
  fn get_info(&self) -> ServerInfo {
    ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
      .with_instructions("Mail bridge MCP server. Tools enforce account permissions from mmb.db.")
  }
}

pub async fn serve(database_path: PathBuf) -> Result<()> {
  let service = MailBridgeServer::new(database_path).serve(stdio()).await?;
  service.waiting().await?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_supported_read_state_filters() {
    assert_eq!(
      MailBridgeServer::parse_read_state(Some("read")).unwrap(),
      Some(mail::ReadStateFilter::Read)
    );
    assert_eq!(
      MailBridgeServer::parse_read_state(Some("unread")).unwrap(),
      Some(mail::ReadStateFilter::Unread)
    );
    assert!(MailBridgeServer::parse_read_state(Some("archived")).is_err());
  }

  #[test]
  fn cache_key_includes_bounded_request_shape() {
    let window = mail::MessageDateWindow::new(Some(10), Some(20));
    let key = MailBridgeServer::cache_key(
      "work",
      Some("from:example.com"),
      Some("INBOX"),
      &window,
      Some(mail::ReadStateFilter::Unread),
      Some(1000),
      Some("cursor"),
    );

    assert!(key.contains("\"account_id\":\"work\""));
    assert!(key.contains("\"label\":\"INBOX\""));
    assert!(key.contains("\"read_state\":\"unread\""));
    assert!(key.contains("\"limit\":50"));
    assert!(key.contains("\"page_token\":\"cursor\""));
  }

  #[test]
  fn filters_message_summaries_to_exact_requested_window() {
    let messages = vec![
      mail::MessageSummary {
        id: "before".to_owned(),
        thread_id: None,
        remote_version: None,
        subject: None,
        sender: None,
        recipients: Vec::new(),
        snippet: None,
        received_at: Some(9),
        is_read: true,
        labels: Vec::new(),
        source: "gmail".to_owned(),
      },
      mail::MessageSummary {
        id: "inside".to_owned(),
        thread_id: None,
        remote_version: None,
        subject: None,
        sender: None,
        recipients: Vec::new(),
        snippet: None,
        received_at: Some(10),
        is_read: true,
        labels: Vec::new(),
        source: "gmail".to_owned(),
      },
      mail::MessageSummary {
        id: "missing-date".to_owned(),
        thread_id: None,
        remote_version: None,
        subject: None,
        sender: None,
        recipients: Vec::new(),
        snippet: None,
        received_at: None,
        is_read: true,
        labels: Vec::new(),
        source: "gmail".to_owned(),
      },
    ];

    let filtered = filter_by_window(messages, &mail::MessageDateWindow::new(Some(10), Some(20)));

    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].id, "inside");
  }

  #[test]
  fn refreshes_cached_message_content_metadata_without_dropping_body() {
    let content = mail::MessageContent {
      id: "message-1".to_owned(),
      thread_id: Some("old-thread".to_owned()),
      subject: Some("Old subject".to_owned()),
      sender: Some("old@example.com".to_owned()),
      recipients: vec!["reader@example.com".to_owned()],
      snippet: Some("Old snippet".to_owned()),
      body: Some("Cached body".to_owned()),
      body_format: "text/plain".to_owned(),
      received_at: Some(10),
      is_read: false,
      labels: vec!["UNREAD".to_owned()],
      source: "gmail".to_owned(),
    };
    let summary = mail::MessageSummary {
      id: "message-1".to_owned(),
      thread_id: Some("new-thread".to_owned()),
      remote_version: Some("history-2".to_owned()),
      subject: Some("New subject".to_owned()),
      sender: Some("new@example.com".to_owned()),
      recipients: vec!["reader@example.com".to_owned(), "cc@example.com".to_owned()],
      snippet: Some("New snippet".to_owned()),
      received_at: Some(20),
      is_read: true,
      labels: vec!["INBOX".to_owned()],
      source: "gmail".to_owned(),
    };

    let refreshed = MailBridgeServer::refresh_cached_message_content(content, &summary);

    assert_eq!(refreshed.body.as_deref(), Some("Cached body"));
    assert_eq!(refreshed.subject.as_deref(), Some("New subject"));
    assert!(refreshed.is_read);
    assert_eq!(refreshed.labels, vec!["INBOX"]);
  }
}
