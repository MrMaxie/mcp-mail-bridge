use std::path::PathBuf;

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

use crate::{config::Config, permissions::Permission};

#[derive(Debug, Clone)]
pub struct MailBridgeServer {
  database_path: PathBuf,
  tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AccountRequest {
  pub account_id: String,
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
  pub subject: String,
  pub body: String,
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

  fn require_permission(&self, account_id: &str, permission: Permission) -> Result<(), String> {
    let config = self.load_config()?;
    let account = config
      .find_account(account_id)
      .map_err(|error| error.to_string())?;
    if account.allows(permission) {
      Ok(())
    } else {
      Err(format!(
        "account '{account_id}' does not allow '{permission}'"
      ))
    }
  }
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

  #[tool(description = "List messages for an account. Mail backend implementation is pending.")]
  fn list_messages(&self, Parameters(request): Parameters<AccountRequest>) -> String {
    match self.require_permission(&request.account_id, Permission::Read) {
      Ok(()) => json!({
          "account_id": request.account_id,
          "messages": [],
          "status": "mail backend is not implemented yet"
      })
      .to_string(),
      Err(error) => json!({ "error": error }).to_string(),
    }
  }

  #[tool(description = "Read one message for an account. Mail backend implementation is pending.")]
  fn read_message(&self, Parameters(request): Parameters<ReadMessageRequest>) -> String {
    match self.require_permission(&request.account_id, Permission::Read) {
      Ok(()) => json!({
          "account_id": request.account_id,
          "message_id": request.message_id,
          "status": "mail backend is not implemented yet"
      })
      .to_string(),
      Err(error) => json!({ "error": error }).to_string(),
    }
  }

  #[tool(description = "Send a message from an account. Mail backend implementation is pending.")]
  fn send_message(&self, Parameters(request): Parameters<SendMessageRequest>) -> String {
    match self.require_permission(&request.account_id, Permission::Write) {
      Ok(()) => json!({
          "account_id": request.account_id,
          "to": request.to,
          "subject": request.subject,
          "body_length": request.body.len(),
          "status": "mail backend is not implemented yet"
      })
      .to_string(),
      Err(error) => json!({ "error": error }).to_string(),
    }
  }

  #[tool(
    description = "Mark a message as read for an account. Mail backend implementation is pending."
  )]
  fn mark_as_read(&self, Parameters(request): Parameters<ReadMessageRequest>) -> String {
    match self.require_permission(&request.account_id, Permission::MarkAsRead) {
      Ok(()) => json!({
          "account_id": request.account_id,
          "message_id": request.message_id,
          "status": "mail backend is not implemented yet"
      })
      .to_string(),
      Err(error) => json!({ "error": error }).to_string(),
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
