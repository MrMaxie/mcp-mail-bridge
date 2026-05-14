use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{
  StatusCode,
  blocking::{Client, Response},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{AccountConfig, AuthKind, Provider};

const GMAIL_BASE_URL: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const MAX_LIST_LIMIT: u32 = 50;
const DEFAULT_LIST_LIMIT: u32 = 25;
const MAX_WINDOW_DAYS: i64 = 90;
const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Serialize)]
pub struct MessageSummary {
  pub id: String,
  pub thread_id: Option<String>,
  pub subject: Option<String>,
  pub sender: Option<String>,
  pub snippet: Option<String>,
  pub is_read: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageList {
  pub messages: Vec<MessageSummary>,
  pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageContent {
  pub id: String,
  pub thread_id: Option<String>,
  pub subject: Option<String>,
  pub sender: Option<String>,
  pub recipients: Vec<String>,
  pub snippet: Option<String>,
  pub body: Option<String>,
  pub is_read: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendMessageResult {
  pub message_id: String,
  pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationResult {
  pub provider_account: String,
}

#[derive(Debug, Clone, Default)]
pub struct MessageDateWindow {
  pub start_unix: Option<i64>,
  pub end_unix: Option<i64>,
}

impl MessageDateWindow {
  pub fn new(start_unix: Option<i64>, end_unix: Option<i64>) -> Self {
    Self {
      start_unix,
      end_unix,
    }
  }

  fn now_unix() -> i64 {
    let since_epoch = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap_or_default();

    since_epoch.as_secs() as i64
  }

  pub fn as_gmail_terms(&self) -> Vec<String> {
    let mut terms = Vec::new();
    if let Some(start) = self.start_unix {
      terms.push(format!("after:{start}"));
    }
    if let Some(end) = self.end_unix {
      terms.push(format!("before:{end}"));
    }
    terms
  }

  pub fn validate(&self) -> Result<(), MailError> {
    if let Some(start) = self.start_unix
      && (start < 0 || start > Self::now_unix())
    {
      return Err(MailError::InvalidRequest(
        "start date must be between Unix epoch and now".to_owned(),
      ));
    }

    if let Some(end) = self.end_unix
      && (end < 0 || end > Self::now_unix())
    {
      return Err(MailError::InvalidRequest(
        "end date must be between Unix epoch and now".to_owned(),
      ));
    }

    if let (Some(start), Some(end)) = (self.start_unix, self.end_unix) {
      if end < start {
        return Err(MailError::InvalidRequest(
          "end date must not be older than start date".to_owned(),
        ));
      }

      if end - start > MAX_WINDOW_DAYS * SECONDS_PER_DAY {
        return Err(MailError::InvalidRequest(
          "date window must not exceed ninety days".to_owned(),
        ));
      }
    }

    Ok(())
  }
}

#[derive(Debug, Clone)]
pub struct ListMessagesRequest {
  pub query: Option<String>,
  pub window: Option<MessageDateWindow>,
  pub limit: Option<u32>,
  pub page_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReadMessageRequest {
  pub message_id: String,
}

#[derive(Debug, Clone)]
pub struct SendMessageRequest {
  pub to: String,
  pub subject: String,
  pub body: String,
}

pub trait MailAdapter {
  fn validate_account(&self) -> Result<ValidationResult, MailError>;
  fn list_messages(&self, request: &ListMessagesRequest) -> Result<MessageList, MailError>;
  fn read_message(&self, request: &ReadMessageRequest) -> Result<MessageContent, MailError>;
  fn send_message(&self, request: &SendMessageRequest) -> Result<SendMessageResult, MailError>;
  fn mark_as_read(&self, request: &ReadMessageRequest) -> Result<(), MailError>;
}

pub fn adapter_for(account: &AccountConfig) -> Result<Box<dyn MailAdapter>, MailError> {
  match account.provider {
    Provider::Gmail => Ok(Box::new(GmailAdapter::new(
      &account.auth.kind,
      account.auth.username.as_deref(),
      &account.auth.secret,
    )?)),
    Provider::ImapSmtp | Provider::Microsoft365 => {
      Err(MailError::UnsupportedProvider(account.provider.to_string()))
    }
  }
}

pub fn enforce_request_limit(limit: Option<u32>) -> u32 {
  limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT)
}

#[derive(Debug, Error, PartialEq)]
pub enum MailError {
  #[error("provider '{0}' is not implemented yet")]
  UnsupportedProvider(String),
  #[error("auth kind '{0}' is not supported for this provider")]
  UnsupportedAuthKind(String),
  #[error("account secret is missing")]
  MissingSecret,
  #[error("invalid request: {0}")]
  InvalidRequest(String),
  #[error("authentication failed; refresh credentials and retry")]
  Authorization,
  #[error("resource was not found")]
  NotFound,
  #[error("mail service is unavailable")]
  ServiceUnavailable,
  #[error("request failed: {0}")]
  RequestFailed(String),
}

#[derive(Default, Clone)]
pub struct GmailAdapter {
  access_token: String,
  from_address: Option<String>,
  client: Option<Client>,
}

impl GmailAdapter {
  pub fn new(
    auth_kind: &AuthKind,
    from_address: Option<&str>,
    access_token: &str,
  ) -> Result<Self, MailError> {
    if *auth_kind != AuthKind::OAuthToken {
      return Err(MailError::UnsupportedAuthKind(auth_kind.to_string()));
    }

    if access_token.trim().is_empty() {
      return Err(MailError::MissingSecret);
    }

    Ok(Self {
      access_token: access_token.to_owned(),
      from_address: from_address.map(ToOwned::to_owned),
      client: Some(
        Client::builder()
          .timeout(std::time::Duration::from_secs(15))
          .build()
          .map_err(|error| MailError::RequestFailed(error.to_string()))?,
      ),
    })
  }

  fn client(&self) -> &Client {
    self
      .client
      .as_ref()
      .expect("gmail adapter requires a configured client")
  }

  fn request_success(response: Response) -> Result<Response, MailError> {
    match response.status() {
      StatusCode::OK | StatusCode::CREATED => Ok(response),
      StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(MailError::Authorization),
      StatusCode::NOT_FOUND => Err(MailError::NotFound),
      StatusCode::SERVICE_UNAVAILABLE => Err(MailError::ServiceUnavailable),
      status => Err(MailError::RequestFailed(format!(
        "gmail request failed with status {status}"
      ))),
    }
  }

  fn message_query(&self, request: &ListMessagesRequest) -> Result<String, MailError> {
    let mut terms = request
      .query
      .clone()
      .unwrap_or_default()
      .split_whitespace()
      .map(|term| term.to_owned())
      .collect::<Vec<_>>();
    if let Some(window) = &request.window {
      window.validate()?;
      terms.extend(window.as_gmail_terms());
    }
    terms.retain(|term| !term.is_empty());
    Ok(terms.join(" "))
  }

  fn decode_message_body(data: Option<String>) -> Option<String> {
    let encoded = data?;
    URL_SAFE_NO_PAD
      .decode(encoded.as_bytes())
      .ok()
      .and_then(|bytes| {
        String::from_utf8(bytes)
          .ok()
          .map(|body| body.replace("\r\n", "\n"))
      })
  }

  fn headers_to_text(
    headers: Option<Vec<GmailHeader>>,
  ) -> std::collections::HashMap<String, String> {
    headers
      .unwrap_or_default()
      .into_iter()
      .map(|header| (header.name.to_lowercase(), header.value))
      .collect()
  }

  fn find_text_plain_body(payload: Option<GmailMessagePayload>) -> Option<String> {
    let payload = payload?;
    if let Some(data) = payload.body.and_then(|body| body.data) {
      return Self::decode_message_body(Some(data));
    }

    payload
      .parts
      .into_iter()
      .flatten()
      .filter(|part| part.mime_type.as_deref() == Some("text/plain"))
      .find_map(|part| {
        part
          .body
          .and_then(|body| Self::decode_message_body(body.data))
      })
  }
}

impl MailAdapter for GmailAdapter {
  fn validate_account(&self) -> Result<ValidationResult, MailError> {
    let response = self
      .client()
      .get(format!("{GMAIL_BASE_URL}/profile"))
      .bearer_auth(&self.access_token)
      .send()
      .map_err(|error| MailError::RequestFailed(error.to_string()))?;

    let response = Self::request_success(response)?;
    let profile = response
      .json::<GmailProfileResponse>()
      .map_err(|error| MailError::RequestFailed(error.to_string()))?;

    Ok(ValidationResult {
      provider_account: profile.email_address,
    })
  }

  fn list_messages(&self, request: &ListMessagesRequest) -> Result<MessageList, MailError> {
    let window = request.window.as_ref().cloned().unwrap_or_default();
    window.validate()?;

    let limit = enforce_request_limit(request.limit);
    let query = self.message_query(request)?;
    let mut request_builder = self
      .client()
      .get(format!("{GMAIL_BASE_URL}/messages"))
      .bearer_auth(&self.access_token)
      .query(&[
        ("maxResults", limit.to_string()),
        ("pageToken", request.page_token.clone().unwrap_or_default()),
      ]);

    if !query.trim().is_empty() {
      request_builder = request_builder.query(&[("q", query)]);
    }

    let response = request_builder
      .send()
      .map_err(|error| MailError::RequestFailed(error.to_string()))?;
    let response = Self::request_success(response)?;
    let response = response
      .json::<GmailMessageListResponse>()
      .map_err(|error| MailError::RequestFailed(error.to_string()))?;

    let messages = response
      .messages
      .unwrap_or_default()
      .into_iter()
      .map(|entry| {
        let message = self.read_message(&ReadMessageRequest {
          message_id: entry.id,
        })?;
        Ok(message_to_summary(message))
      })
      .collect::<Result<Vec<_>, MailError>>()?;

    Ok(MessageList {
      messages,
      next_page_token: response.next_page_token,
    })
  }

  fn read_message(&self, request: &ReadMessageRequest) -> Result<MessageContent, MailError> {
    if request.message_id.trim().is_empty() {
      return Err(MailError::InvalidRequest(
        "message_id cannot be empty".to_owned(),
      ));
    }

    let response = self
      .client()
      .get(format!("{GMAIL_BASE_URL}/messages/{}", request.message_id))
      .bearer_auth(&self.access_token)
      .query(&[
        ("format", "full".to_owned()),
        ("metadataHeaders", "From".to_owned()),
        ("metadataHeaders", "To".to_owned()),
        ("metadataHeaders", "Cc".to_owned()),
        ("metadataHeaders", "Subject".to_owned()),
      ])
      .send()
      .map_err(|error| MailError::RequestFailed(error.to_string()))?;
    let response = Self::request_success(response)?;
    let response = response
      .json::<GmailMessageResponse>()
      .map_err(|error| MailError::RequestFailed(error.to_string()))?;

    let payload = response.payload.clone();
    let headers =
      Self::headers_to_text(payload.as_ref().and_then(|payload| payload.headers.clone()));

    let is_read = response
      .label_ids
      .unwrap_or_default()
      .into_iter()
      .find(|label| label == "UNREAD")
      .is_none();

    Ok(MessageContent {
      id: response.id,
      thread_id: response.thread_id,
      subject: headers.get("subject").cloned(),
      sender: headers.get("from").cloned(),
      recipients: headers
        .into_iter()
        .filter_map(|(name, value)| match name.as_str() {
          "to" | "cc" => Some(value),
          _ => None,
        })
        .collect(),
      snippet: response.snippet,
      body: Self::find_text_plain_body(payload),
      is_read,
    })
  }

  fn send_message(&self, request: &SendMessageRequest) -> Result<SendMessageResult, MailError> {
    if request.to.trim().is_empty() {
      return Err(MailError::InvalidRequest(
        "to address cannot be empty".to_owned(),
      ));
    }
    if request.subject.trim().is_empty() {
      return Err(MailError::InvalidRequest(
        "subject cannot be empty".to_owned(),
      ));
    }

    let from_address = self.from_address.clone().unwrap_or_else(|| "me".to_owned());
    let raw_message = format!(
      "From: {from_address}\r\nTo: {}\r\nSubject: {}\r\n\r\n{}",
      request.to, request.subject, request.body
    );
    let payload = SendRequestBody {
      raw: URL_SAFE_NO_PAD.encode(raw_message.as_bytes()),
    };

    let response = self
      .client()
      .post(format!("{GMAIL_BASE_URL}/messages/send"))
      .bearer_auth(&self.access_token)
      .json(&payload)
      .send()
      .map_err(|error| MailError::RequestFailed(error.to_string()))?;
    let response = Self::request_success(response)?;
    let response = response
      .json::<GmailSendMessageResponse>()
      .map_err(|error| MailError::RequestFailed(error.to_string()))?;

    Ok(SendMessageResult {
      message_id: response.id,
      thread_id: response.thread_id,
    })
  }

  fn mark_as_read(&self, request: &ReadMessageRequest) -> Result<(), MailError> {
    if request.message_id.trim().is_empty() {
      return Err(MailError::InvalidRequest(
        "message_id cannot be empty".to_owned(),
      ));
    }

    let payload = ModifyMessageRequest {
      remove_label_ids: vec!["UNREAD".to_owned()],
    };

    let response = self
      .client()
      .post(format!(
        "{GMAIL_BASE_URL}/messages/{}/modify",
        request.message_id
      ))
      .bearer_auth(&self.access_token)
      .json(&payload)
      .send()
      .map_err(|error| MailError::RequestFailed(error.to_string()))?;

    Self::request_success(response).map(|_| ())
  }
}

#[derive(Deserialize)]
struct GmailProfileResponse {
  email_address: String,
}

#[derive(Deserialize)]
struct GmailMessageListResponse {
  messages: Option<Vec<GmailListMessage>>,
  next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct GmailListMessage {
  id: String,
}

#[derive(Deserialize, Clone)]
struct GmailMessageResponse {
  id: String,
  thread_id: Option<String>,
  snippet: Option<String>,
  label_ids: Option<Vec<String>>,
  payload: Option<GmailMessagePayload>,
}

#[derive(Deserialize, Clone)]
struct GmailMessagePayload {
  headers: Option<Vec<GmailHeader>>,
  body: Option<GmailBody>,
  parts: Option<Vec<GmailMessagePayload>>,
  mime_type: Option<String>,
}

#[derive(Deserialize, Clone)]
struct GmailHeader {
  name: String,
  value: String,
}

#[derive(Deserialize, Clone)]
struct GmailBody {
  data: Option<String>,
}

#[derive(Serialize)]
struct SendRequestBody {
  raw: String,
}

#[derive(Deserialize)]
struct GmailSendMessageResponse {
  id: String,
  thread_id: Option<String>,
}

#[derive(Serialize)]
struct ModifyMessageRequest {
  remove_label_ids: Vec<String>,
}

fn message_to_summary(message: MessageContent) -> MessageSummary {
  MessageSummary {
    id: message.id,
    thread_id: message.thread_id,
    subject: message.subject,
    sender: message.sender,
    snippet: message.snippet,
    is_read: message.is_read,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rejects_gmail_list_window_longer_than_three_months() {
    let now = MessageDateWindow::now_unix();
    let request = MessageDateWindow {
      start_unix: Some(now - (MAX_WINDOW_DAYS + 2) * SECONDS_PER_DAY),
      end_unix: Some(now),
    };
    assert_eq!(
      request.validate(),
      Err(MailError::InvalidRequest(
        "date window must not exceed ninety days".to_owned()
      ))
    );
  }

  #[test]
  fn enforces_request_limit_minimum_one() {
    assert_eq!(enforce_request_limit(Some(0)), 1);
    assert_eq!(enforce_request_limit(Some(1000)), MAX_LIST_LIMIT);
  }
}
