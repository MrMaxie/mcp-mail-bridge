use chrono::TimeZone;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, Response, StatusCode};
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

  pub fn as_gmail_terms(&self) -> Result<Vec<String>, MailError> {
    let mut terms = Vec::new();
    if let Some(start) = self.start_unix {
      terms.push(format!("after:{}", Self::unix_to_gmail_date(start)?));
    }
    if let Some(end) = self.end_unix {
      terms.push(format!("before:{}", Self::unix_to_gmail_date(end)?));
    }
    Ok(terms)
  }

  fn unix_to_gmail_date(unix: i64) -> Result<String, MailError> {
    Ok(
      chrono::Utc
        .timestamp_opt(unix, 0)
        .single()
        .ok_or_else(|| MailError::InvalidRequest("invalid unix timestamp".to_owned()))?
        .format("%Y/%m/%d")
        .to_string(),
    )
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

pub trait MailAdapter: Send + Sync {
  fn validate_account(
    &self,
  ) -> Pin<Box<dyn Future<Output = Result<ValidationResult, MailError>> + Send>>;
  fn list_messages(
    &self,
    request: ListMessagesRequest,
  ) -> Pin<Box<dyn Future<Output = Result<MessageList, MailError>> + Send>>;
  fn read_message(
    &self,
    request: ReadMessageRequest,
  ) -> Pin<Box<dyn Future<Output = Result<MessageContent, MailError>> + Send>>;
  fn send_message(
    &self,
    request: SendMessageRequest,
  ) -> Pin<Box<dyn Future<Output = Result<SendMessageResult, MailError>> + Send>>;
  fn mark_as_read(
    &self,
    request: ReadMessageRequest,
  ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send>>;
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

#[derive(Clone)]
pub struct GmailAdapter {
  access_token: String,
  from_address: Option<String>,
  client: Client,
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
      client: Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| MailError::RequestFailed(error.to_string()))?,
    })
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
      terms.extend(window.as_gmail_terms()?);
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

  fn headers_to_text(headers: Option<Vec<GmailHeader>>) -> HashMap<String, String> {
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

  fn message_summary_from_metadata_response(message: &GmailMessageResponse) -> MessageSummary {
    let headers = Self::headers_to_text(
      message
        .payload
        .as_ref()
        .and_then(|payload| payload.headers.clone()),
    );
    MessageSummary {
      id: message.id.clone(),
      thread_id: message.thread_id.clone(),
      subject: headers.get("subject").cloned(),
      sender: headers.get("from").cloned(),
      snippet: message.snippet.clone(),
      is_read: message
        .label_ids
        .as_ref()
        .map(|labels| !labels.iter().any(|label| label == "UNREAD"))
        .unwrap_or(true),
    }
  }

  async fn fetch_message_summary(
    client: Client,
    access_token: String,
    message_id: String,
  ) -> Result<MessageSummary, MailError> {
    let response = client
      .get(format!("{GMAIL_BASE_URL}/messages/{message_id}"))
      .bearer_auth(access_token)
      .query(&[
        ("format", "metadata"),
        ("metadataHeaders", "From"),
        ("metadataHeaders", "Subject"),
        ("metadataHeaders", "To"),
        ("fields", "id,threadId,snippet,labelIds,payload/headers"),
      ])
      .send()
      .await
      .map_err(|error| MailError::RequestFailed(error.to_string()))?;
    let response = Self::request_success(response)?;
    let response = response
      .json::<GmailMessageResponse>()
      .await
      .map_err(|error| MailError::RequestFailed(error.to_string()))?;

    Ok(Self::message_summary_from_metadata_response(&response))
  }

  fn message_content_from_response(message: GmailMessageResponse) -> MessageContent {
    let payload = message.payload.clone();
    let headers =
      Self::headers_to_text(payload.as_ref().and_then(|payload| payload.headers.clone()));

    MessageContent {
      id: message.id,
      thread_id: message.thread_id,
      subject: headers.get("subject").cloned(),
      sender: headers.get("from").cloned(),
      recipients: headers
        .into_iter()
        .filter_map(|(name, value)| match name.as_str() {
          "to" | "cc" => Some(value),
          _ => None,
        })
        .collect(),
      snippet: message.snippet,
      body: Self::find_text_plain_body(payload),
      is_read: message
        .label_ids
        .into_iter()
        .flatten()
        .find(|label| label == "UNREAD")
        .is_none(),
    }
  }

  fn sanitize_header_value(field: &str, value: &str) -> Result<String, MailError> {
    let sanitized = value.trim();
    if sanitized.is_empty() {
      return Err(MailError::InvalidRequest(format!(
        "{field} cannot be empty"
      )));
    }

    if sanitized.contains('\r') || sanitized.contains('\n') {
      return Err(MailError::InvalidRequest(format!(
        "{field} contains invalid line breaks"
      )));
    }

    if sanitized.contains('\x00') || sanitized.chars().any(|c| c.is_control()) {
      return Err(MailError::InvalidRequest(format!(
        "{field} contains invalid control characters"
      )));
    }

    Ok(sanitized.to_owned())
  }

  fn validate_recipient(address: &str) -> Result<(), MailError> {
    let mut parts = address.split('@');
    let local_part = parts.next().filter(|part| !part.is_empty());
    let domain_part = parts.next().filter(|part| !part.is_empty());
    if local_part.is_none()
      || domain_part.is_none()
      || parts.next().is_some()
      || domain_part.unwrap_or_default().find('.').is_none()
    {
      return Err(MailError::InvalidRequest(format!(
        "to address '{address}' is not valid"
      )));
    }

    Ok(())
  }

  fn sanitize_recipient_list(input: &str) -> Result<String, MailError> {
    let recipients = input
      .split(',')
      .map(|entry| entry.trim())
      .filter(|entry| !entry.is_empty())
      .collect::<Vec<_>>();
    if recipients.is_empty() {
      return Err(MailError::InvalidRequest(
        "to address cannot be empty".to_owned(),
      ));
    }

    let recipients = recipients
      .into_iter()
      .map(|entry| {
        let sanitized = Self::sanitize_header_value("to", entry)?;
        Self::validate_recipient(&sanitized)?;
        Ok::<String, MailError>(sanitized)
      })
      .collect::<Result<Vec<_>, _>>()?;

    Ok(recipients.join(", "))
  }

  fn sanitize_message_body(body: &str) -> String {
    body.replace("\r\n", "\n")
  }
}

impl MailAdapter for GmailAdapter {
  fn validate_account(
    &self,
  ) -> Pin<Box<dyn Future<Output = Result<ValidationResult, MailError>> + Send>> {
    let client = self.client.clone();
    let access_token = self.access_token.clone();

    Box::pin(async move {
      let response = client
        .get(format!("{GMAIL_BASE_URL}/profile"))
        .bearer_auth(&access_token)
        .send()
        .await
        .map_err(|error| MailError::RequestFailed(error.to_string()))?;

      let response = Self::request_success(response)?;
      let profile = response
        .json::<GmailProfileResponse>()
        .await
        .map_err(|error| MailError::RequestFailed(error.to_string()))?;

      Ok(ValidationResult {
        provider_account: profile.email_address,
      })
    })
  }

  fn list_messages(
    &self,
    request: ListMessagesRequest,
  ) -> Pin<Box<dyn Future<Output = Result<MessageList, MailError>> + Send>> {
    let adapter = self.clone();
    let client = self.client.clone();
    let access_token = self.access_token.clone();

    Box::pin(async move {
      let window = request.window.clone().unwrap_or_default();
      window.validate()?;

      let limit = enforce_request_limit(request.limit);
      let query = adapter.message_query(&request)?;
      let mut list_request = client
        .get(format!("{GMAIL_BASE_URL}/messages"))
        .bearer_auth(&access_token)
        .query(&[("maxResults", limit.to_string())]);

      if let Some(page_token) = request
        .page_token
        .as_ref()
        .filter(|token| !token.is_empty())
      {
        list_request = list_request.query(&[("pageToken", page_token)]);
      }
      if !query.trim().is_empty() {
        list_request = list_request.query(&[("q", query.as_str())]);
      }

      let response = list_request
        .send()
        .await
        .map_err(|error| MailError::RequestFailed(error.to_string()))?;
      let response = Self::request_success(response)?;
      let response = response
        .json::<GmailMessageListResponse>()
        .await
        .map_err(|error| MailError::RequestFailed(error.to_string()))?;

      let requested_ids = response
        .messages
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>();

      if requested_ids.is_empty() {
        return Ok(MessageList {
          messages: Vec::new(),
          next_page_token: response.next_page_token,
        });
      }

      let mut messages = Vec::with_capacity(requested_ids.len());
      for message_id in requested_ids {
        let summary =
          Self::fetch_message_summary(client.clone(), access_token.clone(), message_id.clone())
            .await?;
        messages.push(summary);
      }

      Ok(MessageList {
        messages,
        next_page_token: response.next_page_token,
      })
    })
  }

  fn read_message(
    &self,
    request: ReadMessageRequest,
  ) -> Pin<Box<dyn Future<Output = Result<MessageContent, MailError>> + Send>> {
    let client = self.client.clone();
    let access_token = self.access_token.clone();
    Box::pin(async move {
      if request.message_id.trim().is_empty() {
        return Err(MailError::InvalidRequest(
          "message_id cannot be empty".to_owned(),
        ));
      }

      let response = client
        .get(format!("{GMAIL_BASE_URL}/messages/{}", request.message_id))
        .bearer_auth(&access_token)
        .query(&[
          ("format", "full".to_owned()),
          ("metadataHeaders", "From".to_owned()),
          ("metadataHeaders", "To".to_owned()),
          ("metadataHeaders", "Cc".to_owned()),
          ("metadataHeaders", "Subject".to_owned()),
        ])
        .send()
        .await
        .map_err(|error| MailError::RequestFailed(error.to_string()))?;
      let response = Self::request_success(response)?;
      let response = response
        .json::<GmailMessageResponse>()
        .await
        .map_err(|error| MailError::RequestFailed(error.to_string()))?;

      Ok(Self::message_content_from_response(response))
    })
  }

  fn send_message(
    &self,
    request: SendMessageRequest,
  ) -> Pin<Box<dyn Future<Output = Result<SendMessageResult, MailError>> + Send>> {
    let client = self.client.clone();
    let access_token = self.access_token.clone();
    let from_address = self.from_address.clone();
    Box::pin(async move {
      let to = Self::sanitize_recipient_list(&request.to)?;
      let subject = Self::sanitize_header_value("subject", &request.subject)?;
      let from_address = from_address
        .as_deref()
        .map(|value| Self::sanitize_header_value("from", value))
        .transpose()?
        .unwrap_or_else(|| "me".to_owned());
      let body = Self::sanitize_message_body(&request.body);
      let raw_message = format!(
        "From: {from_address}\r\nTo: {to}\r\nSubject: {subject}\r\n\
MIME-Version: 1.0\r\nContent-Type: text/plain; charset=\"UTF-8\"\r\n\
Content-Transfer-Encoding: 8bit\r\n\r\n{body}"
      );
      let payload = SendRequestBody {
        raw: URL_SAFE_NO_PAD.encode(raw_message.as_bytes()),
      };

      let response = client
        .post(format!("{GMAIL_BASE_URL}/messages/send"))
        .bearer_auth(&access_token)
        .json(&payload)
        .send()
        .await
        .map_err(|error| MailError::RequestFailed(error.to_string()))?;
      let response = Self::request_success(response)?;
      let response = response
        .json::<GmailSendMessageResponse>()
        .await
        .map_err(|error| MailError::RequestFailed(error.to_string()))?;

      Ok(SendMessageResult {
        message_id: response.id,
        thread_id: response.thread_id,
      })
    })
  }

  fn mark_as_read(
    &self,
    request: ReadMessageRequest,
  ) -> Pin<Box<dyn Future<Output = Result<(), MailError>> + Send>> {
    let client = self.client.clone();
    let access_token = self.access_token.clone();

    Box::pin(async move {
      if request.message_id.trim().is_empty() {
        return Err(MailError::InvalidRequest(
          "message_id cannot be empty".to_owned(),
        ));
      }

      let payload = ModifyMessageRequest {
        remove_label_ids: vec!["UNREAD".to_owned()],
      };

      let response = client
        .post(format!(
          "{GMAIL_BASE_URL}/messages/{}/modify",
          request.message_id
        ))
        .bearer_auth(&access_token)
        .json(&payload)
        .send()
        .await
        .map_err(|error| MailError::RequestFailed(error.to_string()))?;

      Self::request_success(response).map(|_| ())
    })
  }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailProfileResponse {
  email_address: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailMessageListResponse {
  messages: Option<Vec<GmailListMessage>>,
  next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailListMessage {
  id: String,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct GmailMessageResponse {
  id: String,
  thread_id: Option<String>,
  snippet: Option<String>,
  label_ids: Option<Vec<String>>,
  payload: Option<GmailMessagePayload>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
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
  fn formats_gmail_date_window_terms_with_ymd_dates() {
    use chrono::TimeZone;

    let start = chrono::Utc
      .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
      .single()
      .expect("start date should be valid")
      .timestamp();
    let end = chrono::Utc
      .with_ymd_and_hms(2026, 1, 31, 0, 0, 0)
      .single()
      .expect("end date should be valid")
      .timestamp();
    let request = MessageDateWindow::new(Some(start), Some(end));
    assert_eq!(
      request.as_gmail_terms().expect("window should be valid"),
      vec![
        "after:2026/01/01".to_owned(),
        "before:2026/01/31".to_owned()
      ]
    );
  }

  #[test]
  fn rejects_recipient_with_control_characters() {
    assert_eq!(
      GmailAdapter::sanitize_recipient_list("test\r\n@example.com"),
      Err(MailError::InvalidRequest(
        "to contains invalid line breaks".to_owned()
      ))
    );
  }

  #[test]
  fn enforces_request_limit_minimum_one() {
    assert_eq!(enforce_request_limit(Some(0)), 1);
    assert_eq!(enforce_request_limit(Some(1000)), MAX_LIST_LIMIT);
  }
}
