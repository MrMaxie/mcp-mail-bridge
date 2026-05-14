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

use crate::{config::Config, mail, permissions::Permission};

#[derive(Debug, Clone)]
pub struct MailBridgeServer {
  database_path: PathBuf,
  tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListMessagesRequest {
  pub account_id: String,
  pub query: Option<String>,
  pub start_unix: Option<i64>,
  pub end_unix: Option<i64>,
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

  fn run_with_adapter<T, F>(&self, account_id: &str, permission: Permission, action: F) -> String
  where
    F: FnOnce(&dyn mail::MailAdapter) -> Result<T, mail::MailError>,
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

    if let Err(error) = adapter.validate_account() {
      return json!({ "error": error.to_string() }).to_string();
    }

    match action(adapter.as_ref()) {
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
  fn list_messages(&self, Parameters(request): Parameters<ListMessagesRequest>) -> String {
    if let Err(error) =
      mail::MessageDateWindow::new(request.start_unix, request.end_unix).validate()
    {
      return json!({ "error": error.to_string() }).to_string();
    }

    let window = mail::MessageDateWindow::new(request.start_unix, request.end_unix);

    self.run_with_adapter(&request.account_id, Permission::Read, move |adapter| {
      adapter.list_messages(&mail::ListMessagesRequest {
        query: request.query,
        window: Some(window),
        limit: request.limit,
        page_token: request.page_token,
      })
    })
  }

  #[tool(description = "Read one message for an account.")]
  fn read_message(&self, Parameters(request): Parameters<ReadMessageRequest>) -> String {
    let message_id = request.message_id;
    self.run_with_adapter(&request.account_id, Permission::Read, move |adapter| {
      adapter.read_message(&mail::ReadMessageRequest { message_id })
    })
  }

  #[tool(description = "Send a message from an account.")]
  fn send_message(&self, Parameters(request): Parameters<SendMessageRequest>) -> String {
    let message = mail::SendMessageRequest {
      to: request.to,
      subject: request.subject,
      body: request.body,
    };
    self.run_with_adapter(&request.account_id, Permission::Write, move |adapter| {
      adapter.send_message(&message)
    })
  }

  #[tool(description = "Mark a message as read for an account.")]
  fn mark_as_read(&self, Parameters(request): Parameters<ReadMessageRequest>) -> String {
    let message_id = request.message_id;
    self.run_with_adapter(
      &request.account_id,
      Permission::MarkAsRead,
      move |adapter| {
        adapter.mark_as_read(&mail::ReadMessageRequest { message_id })?;
        Ok(json!({ "status": "message marked as read" }))
      },
    )
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
