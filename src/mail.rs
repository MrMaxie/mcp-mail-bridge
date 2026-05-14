use chrono::TimeZone;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{
  Engine as _,
  engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use reqwest::{Client, Response, StatusCode, blocking::Client as BlockingClient};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{AccountConfig, AuthKind, Provider};

const GMAIL_BASE_URL: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const MAX_LIST_LIMIT: u32 = 50;
const DEFAULT_LIST_LIMIT: u32 = 25;
const MAX_WINDOW_DAYS: i64 = 90;
const DEFAULT_WINDOW_DAYS: i64 = 30;
const SECONDS_PER_DAY: i64 = 24 * 60 * 60;
const GMAIL_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const ACCESS_TOKEN_REFRESH_SKEW_SECONDS: i64 = 60;
const GMAIL_METADATA_FIELDS: &str =
  "id,threadId,historyId,snippet,labelIds,internalDate,payload/headers";
const SUMMARY_FETCH_CONCURRENCY: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSummary {
  pub id: String,
  pub thread_id: Option<String>,
  pub remote_version: Option<String>,
  pub subject: Option<String>,
  pub sender: Option<String>,
  pub recipients: Vec<String>,
  pub snippet: Option<String>,
  pub received_at: Option<i64>,
  pub is_read: bool,
  pub labels: Vec<String>,
  pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageList {
  pub messages: Vec<MessageSummary>,
  pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageContent {
  pub id: String,
  pub thread_id: Option<String>,
  pub subject: Option<String>,
  pub sender: Option<String>,
  pub recipients: Vec<String>,
  pub snippet: Option<String>,
  pub body: Option<String>,
  pub body_format: String,
  pub received_at: Option<i64>,
  pub is_read: bool,
  pub labels: Vec<String>,
  pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageResult {
  pub message_id: String,
  pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateMutationResult {
  pub message_id: String,
  pub is_read: bool,
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
      terms.push(format!("before:{}", Self::unix_to_gmail_before_date(end)?));
    }
    Ok(terms)
  }

  fn unix_to_gmail_date(unix: i64) -> Result<String, MailError> {
    Ok(Self::unix_to_utc_date(unix)?.format("%Y/%m/%d").to_string())
  }

  fn unix_to_gmail_before_date(unix: i64) -> Result<String, MailError> {
    Ok(
      Self::unix_to_utc_date(unix)?
        .succ_opt()
        .ok_or_else(|| MailError::InvalidRequest("invalid unix timestamp".to_owned()))?
        .format("%Y/%m/%d")
        .to_string(),
    )
  }

  fn unix_to_utc_date(unix: i64) -> Result<chrono::NaiveDate, MailError> {
    Ok(
      chrono::Utc
        .timestamp_opt(unix, 0)
        .single()
        .ok_or_else(|| MailError::InvalidRequest("invalid unix timestamp".to_owned()))?
        .date_naive(),
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

  pub fn bounded_or_default(self) -> Result<Self, MailError> {
    match (self.start_unix, self.end_unix) {
      (None, None) => {
        let end = Self::now_unix();
        let bounded = Self {
          start_unix: Some(end - DEFAULT_WINDOW_DAYS * SECONDS_PER_DAY),
          end_unix: Some(end),
        };
        bounded.validate()?;
        Ok(bounded)
      }
      (Some(_), Some(_)) => {
        self.validate()?;
        Ok(self)
      }
      _ => Err(MailError::InvalidRequest(
        "date window must include both start_unix and end_unix".to_owned(),
      )),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadStateFilter {
  Read,
  Unread,
}

impl ReadStateFilter {
  pub fn as_gmail_term(self) -> &'static str {
    match self {
      Self::Read => "-is:unread",
      Self::Unread => "is:unread",
    }
  }

  pub fn as_cache_value(self) -> &'static str {
    match self {
      Self::Read => "read",
      Self::Unread => "unread",
    }
  }
}

#[derive(Debug, Clone)]
pub struct ListMessagesRequest {
  pub query: Option<String>,
  pub label: Option<String>,
  pub window: Option<MessageDateWindow>,
  pub read_state: Option<ReadStateFilter>,
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
  pub cc: Option<String>,
  pub bcc: Option<String>,
  pub subject: String,
  pub body: String,
  pub body_format: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SetReadStateRequest {
  pub message_id: String,
  pub is_read: bool,
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
  fn set_read_state(
    &self,
    request: SetReadStateRequest,
  ) -> Pin<Box<dyn Future<Output = Result<StateMutationResult, MailError>> + Send>>;
  fn refresh_message_summaries(
    &self,
    message_ids: Vec<String>,
  ) -> Pin<Box<dyn Future<Output = Result<Vec<MessageSummary>, MailError>> + Send>>;
}

pub fn adapter_for(account: &AccountConfig) -> Result<Box<dyn MailAdapter>, MailError> {
  match account.provider {
    Provider::Gmail => Ok(Box::new(GmailAdapter::new(
      &account.auth.kind,
      &account.email,
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
  #[error("mail transport failed: {0}")]
  Transport(String),
  #[error("request failed: {0}")]
  RequestFailed(String),
}

#[derive(Clone)]
enum GmailCredential {
  AccessToken(String),
  Refreshable(OAuthTokenBundle),
}

#[derive(Clone, Deserialize)]
struct OAuthTokenBundle {
  access_token: Option<String>,
  refresh_token: String,
  client_id: String,
  client_secret: String,
  token_uri: Option<String>,
  expires_at_unix: Option<i64>,
}

impl GmailCredential {
  fn parse(secret: &str) -> Result<Self, MailError> {
    let trimmed = secret.trim();
    if trimmed.is_empty() {
      return Err(MailError::MissingSecret);
    }

    if trimmed.starts_with('{') {
      let bundle = serde_json::from_str::<OAuthTokenBundle>(trimmed).map_err(|_| {
        MailError::InvalidRequest("oauth token bundle must be valid JSON".to_owned())
      })?;
      if bundle.refresh_token.trim().is_empty()
        || bundle.client_id.trim().is_empty()
        || bundle.client_secret.trim().is_empty()
      {
        return Err(MailError::InvalidRequest(
          "oauth token bundle must include refresh_token, client_id, and client_secret".to_owned(),
        ));
      }
      bundle.validated_token_uri()?;
      return Ok(Self::Refreshable(bundle));
    }

    Ok(Self::AccessToken(trimmed.to_owned()))
  }

  async fn access_token(&self, client: &Client) -> Result<String, MailError> {
    match self {
      Self::AccessToken(token) => Ok(token.clone()),
      Self::Refreshable(bundle) => {
        if let Some(access_token) = bundle.valid_cached_access_token(MessageDateWindow::now_unix())
        {
          return Ok(access_token.to_owned());
        }

        #[derive(Deserialize)]
        struct TokenRefreshResponse {
          access_token: String,
        }

        let token_uri = bundle.validated_token_uri()?;
        let response = client
          .post(token_uri)
          .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", bundle.refresh_token.as_str()),
            ("client_id", bundle.client_id.as_str()),
            ("client_secret", bundle.client_secret.as_str()),
          ])
          .send()
          .await
          .map_err(|error| MailError::Transport(error.to_string()))?;
        let response = GmailAdapter::request_success(response)?;
        let response = response
          .json::<TokenRefreshResponse>()
          .await
          .map_err(|error| MailError::RequestFailed(error.to_string()))?;
        Ok(response.access_token)
      }
    }
  }
}

impl OAuthTokenBundle {
  fn valid_cached_access_token(&self, now_unix: i64) -> Option<&str> {
    let access_token = self.access_token.as_deref()?.trim();
    if access_token.is_empty() {
      return None;
    }

    let expires_at = self.expires_at_unix?;
    if expires_at > now_unix + ACCESS_TOKEN_REFRESH_SKEW_SECONDS {
      Some(access_token)
    } else {
      None
    }
  }

  fn validated_token_uri(&self) -> Result<&str, MailError> {
    let Some(token_uri) = self.token_uri.as_deref() else {
      return Ok(GMAIL_TOKEN_URL);
    };

    if token_uri == GMAIL_TOKEN_URL {
      return Ok(token_uri);
    }

    Err(MailError::InvalidRequest(format!(
      "oauth token bundle token_uri must be {GMAIL_TOKEN_URL}"
    )))
  }
}

pub(crate) fn validate_gmail_identity_blocking(
  expected_email: &str,
  secret: &str,
) -> Result<(), MailError> {
  let client = BlockingClient::builder()
    .timeout(std::time::Duration::from_secs(15))
    .build()
    .map_err(|error| MailError::RequestFailed(error.to_string()))?;
  let access_token = gmail_access_token_blocking(&client, secret)?;
  let response = client
    .get(format!("{GMAIL_BASE_URL}/profile"))
    .bearer_auth(access_token)
    .send()
    .map_err(|error| MailError::Transport(error.to_string()))?;
  GmailAdapter::status_success(response.status())?;
  let profile = response
    .json::<GmailProfileResponse>()
    .map_err(|error| MailError::RequestFailed(error.to_string()))?;

  if !profile.email_address.eq_ignore_ascii_case(expected_email) {
    return Err(MailError::InvalidRequest(
      "authenticated Gmail account does not match configured account email".to_owned(),
    ));
  }

  Ok(())
}

fn gmail_access_token_blocking(client: &BlockingClient, secret: &str) -> Result<String, MailError> {
  match GmailCredential::parse(secret)? {
    GmailCredential::AccessToken(token) => Ok(token),
    GmailCredential::Refreshable(bundle) => {
      if let Some(access_token) = bundle.valid_cached_access_token(MessageDateWindow::now_unix()) {
        return Ok(access_token.to_owned());
      }

      #[derive(Deserialize)]
      struct TokenRefreshResponse {
        access_token: String,
      }

      let token_uri = bundle.validated_token_uri()?;
      let response = client
        .post(token_uri)
        .form(&[
          ("grant_type", "refresh_token"),
          ("refresh_token", bundle.refresh_token.as_str()),
          ("client_id", bundle.client_id.as_str()),
          ("client_secret", bundle.client_secret.as_str()),
        ])
        .send()
        .map_err(|error| MailError::Transport(error.to_string()))?;
      GmailAdapter::status_success(response.status())?;
      let response = response
        .json::<TokenRefreshResponse>()
        .map_err(|error| MailError::RequestFailed(error.to_string()))?;
      Ok(response.access_token)
    }
  }
}

#[derive(Clone)]
pub struct GmailAdapter {
  credential: GmailCredential,
  expected_email: String,
  from_address: Option<String>,
  client: Client,
}

impl GmailAdapter {
  fn status_success(status: StatusCode) -> Result<(), MailError> {
    match status {
      StatusCode::OK | StatusCode::CREATED => Ok(()),
      StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(MailError::Authorization),
      StatusCode::NOT_FOUND => Err(MailError::NotFound),
      StatusCode::TOO_MANY_REQUESTS
      | StatusCode::INTERNAL_SERVER_ERROR
      | StatusCode::BAD_GATEWAY
      | StatusCode::SERVICE_UNAVAILABLE
      | StatusCode::GATEWAY_TIMEOUT => Err(MailError::ServiceUnavailable),
      status if status.is_client_error() => Err(MailError::InvalidRequest(format!(
        "gmail request was rejected with status {status}"
      ))),
      status => Err(MailError::RequestFailed(format!(
        "gmail request failed with status {status}"
      ))),
    }
  }

  pub fn new(auth_kind: &AuthKind, expected_email: &str, secret: &str) -> Result<Self, MailError> {
    if *auth_kind != AuthKind::OAuthToken {
      return Err(MailError::UnsupportedAuthKind(auth_kind.to_string()));
    }

    Ok(Self {
      credential: GmailCredential::parse(secret)?,
      expected_email: expected_email.to_owned(),
      from_address: Some(expected_email.to_owned()),
      client: Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| MailError::RequestFailed(error.to_string()))?,
    })
  }

  fn request_success(response: Response) -> Result<Response, MailError> {
    Self::status_success(response.status())?;
    Ok(response)
  }

  fn message_query(&self, request: &ListMessagesRequest) -> Result<String, MailError> {
    let mut terms = request
      .query
      .clone()
      .unwrap_or_default()
      .split_whitespace()
      .map(|term| term.to_owned())
      .collect::<Vec<_>>();
    let window = request
      .window
      .clone()
      .unwrap_or_default()
      .bounded_or_default()?;
    terms.extend(window.as_gmail_terms()?);
    if let Some(read_state) = request.read_state {
      terms.push(read_state.as_gmail_term().to_owned());
    }
    terms.retain(|term| !term.is_empty());
    Ok(terms.join(" "))
  }

  fn sanitize_query_atom(field: &str, value: &str) -> Result<String, MailError> {
    let sanitized = value.trim();
    if sanitized.is_empty() {
      return Err(MailError::InvalidRequest(format!(
        "{field} cannot be empty"
      )));
    }
    if sanitized.chars().any(|character| {
      character.is_control() || character.is_whitespace() || matches!(character, '"' | '\'' | '\\')
    }) {
      return Err(MailError::InvalidRequest(format!(
        "{field} contains unsupported query characters"
      )));
    }
    Ok(sanitized.to_owned())
  }

  fn sanitize_message_id(message_id: &str) -> Result<String, MailError> {
    let sanitized = message_id.trim();
    if sanitized.is_empty() {
      return Err(MailError::InvalidRequest(
        "message_id cannot be empty".to_owned(),
      ));
    }
    if !sanitized
      .chars()
      .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
      return Err(MailError::InvalidRequest(
        "message_id contains unsupported path characters".to_owned(),
      ));
    }
    Ok(sanitized.to_owned())
  }

  fn decode_message_body(data: Option<String>) -> Option<String> {
    let encoded = data?;
    URL_SAFE_NO_PAD
      .decode(encoded.as_bytes())
      .or_else(|_| URL_SAFE.decode(encoded.as_bytes()))
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

  fn recipients_from_headers(headers: &HashMap<String, String>) -> Vec<String> {
    ["to", "cc", "bcc"]
      .into_iter()
      .filter_map(|name| headers.get(name).cloned())
      .collect()
  }

  fn internal_date_to_unix(internal_date: Option<&str>) -> Option<i64> {
    internal_date
      .and_then(|value| value.parse::<i64>().ok())
      .map(|millis| millis / 1000)
  }

  fn find_body_by_mime(payload: &GmailMessagePayload, mime_type: &str) -> Option<String> {
    if payload.mime_type.as_deref() == Some(mime_type)
      && let Some(data) = payload.body.as_ref().and_then(|body| body.data.clone())
    {
      return Self::decode_message_body(Some(data));
    }

    payload
      .parts
      .as_ref()
      .into_iter()
      .flatten()
      .find_map(|part| Self::find_body_by_mime(part, mime_type))
  }

  fn find_decoded_body(payload: Option<GmailMessagePayload>) -> (Option<String>, String) {
    let Some(payload) = payload else {
      return (None, "text/plain".to_owned());
    };

    if let Some(body) = Self::find_body_by_mime(&payload, "text/plain") {
      return (Some(body), "text/plain".to_owned());
    }

    if let Some(body) = Self::find_body_by_mime(&payload, "text/html") {
      return (Some(body), "text/html".to_owned());
    }

    if let Some(data) = payload.body.and_then(|body| body.data)
      && let Some(body) = Self::decode_message_body(Some(data))
    {
      return (
        Some(body),
        payload.mime_type.unwrap_or_else(|| "text/plain".to_owned()),
      );
    }

    (None, "text/plain".to_owned())
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
      remote_version: message.history_id.clone(),
      subject: headers.get("subject").cloned(),
      sender: headers.get("from").cloned(),
      recipients: Self::recipients_from_headers(&headers),
      snippet: message.snippet.clone(),
      received_at: Self::internal_date_to_unix(message.internal_date.as_deref()),
      is_read: message
        .label_ids
        .as_ref()
        .map(|labels| !labels.iter().any(|label| label == "UNREAD"))
        .unwrap_or(true),
      labels: message.label_ids.clone().unwrap_or_default(),
      source: "gmail".to_owned(),
    }
  }

  async fn fetch_message_summary(
    client: Client,
    access_token: String,
    message_id: String,
  ) -> Result<MessageSummary, MailError> {
    let message_id = Self::sanitize_message_id(&message_id)?;
    let response = client
      .get(format!("{GMAIL_BASE_URL}/messages/{message_id}"))
      .bearer_auth(access_token)
      .query(&[
        ("format", "metadata"),
        ("metadataHeaders", "From"),
        ("metadataHeaders", "Subject"),
        ("metadataHeaders", "To"),
        ("metadataHeaders", "Cc"),
        ("fields", GMAIL_METADATA_FIELDS),
      ])
      .send()
      .await
      .map_err(|error| MailError::Transport(error.to_string()))?;
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
    let is_read = message
      .label_ids
      .as_ref()
      .map(|labels| !labels.iter().any(|label| label == "UNREAD"))
      .unwrap_or(true);
    let received_at = Self::internal_date_to_unix(message.internal_date.as_deref());
    let labels = message.label_ids.unwrap_or_default();
    let (body, body_format) = Self::find_decoded_body(payload);

    MessageContent {
      id: message.id,
      thread_id: message.thread_id,
      subject: headers.get("subject").cloned(),
      sender: headers.get("from").cloned(),
      recipients: Self::recipients_from_headers(&headers),
      snippet: message.snippet,
      body,
      body_format,
      received_at,
      is_read,
      labels,
      source: "gmail".to_owned(),
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

    if !sanitized.is_ascii() {
      return Err(MailError::InvalidRequest(format!(
        "{field} must contain only ASCII characters"
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
    if address
      .chars()
      .any(|character| character.is_whitespace() || character.is_control())
    {
      return Err(MailError::InvalidRequest(format!(
        "recipient address '{address}' is not valid"
      )));
    }

    let mut parts = address.split('@');
    let local_part = parts.next().filter(|part| !part.is_empty());
    let domain_part = parts.next().filter(|part| !part.is_empty());
    if local_part.is_none()
      || domain_part.is_none()
      || parts.next().is_some()
      || domain_part.unwrap_or_default().find('.').is_none()
    {
      return Err(MailError::InvalidRequest(format!(
        "recipient address '{address}' is not valid"
      )));
    }

    let local_part = local_part.unwrap_or_default();
    let domain_part = domain_part.unwrap_or_default();
    if local_part.chars().any(|character| {
      matches!(
        character,
        '<' | '>' | '(' | ')' | '[' | ']' | ':' | ';' | ','
      )
    }) {
      return Err(MailError::InvalidRequest(format!(
        "recipient address '{address}' is not valid"
      )));
    }

    let domain_labels = domain_part.split('.').collect::<Vec<_>>();
    if domain_labels.len() < 2
      || domain_labels.iter().any(|label| {
        label.is_empty()
          || label.starts_with('-')
          || label.ends_with('-')
          || !label
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
      })
    {
      return Err(MailError::InvalidRequest(format!(
        "recipient address '{address}' is not valid"
      )));
    }

    Ok(())
  }

  fn sanitize_recipient_list(field: &str, input: &str) -> Result<String, MailError> {
    let recipients = input
      .split(',')
      .map(|entry| entry.trim())
      .filter(|entry| !entry.is_empty())
      .collect::<Vec<_>>();
    if recipients.is_empty() {
      return Err(MailError::InvalidRequest(format!(
        "{field} address cannot be empty"
      )));
    }

    let recipients = recipients
      .into_iter()
      .map(|entry| {
        let sanitized = Self::sanitize_header_value(field, entry)?;
        Self::validate_recipient(&sanitized)?;
        Ok::<String, MailError>(sanitized)
      })
      .collect::<Result<Vec<_>, _>>()?;

    Ok(recipients.join(", "))
  }

  fn sanitize_message_body(body: &str) -> Result<String, MailError> {
    let body = body.replace("\r\n", "\n");
    if body.trim().is_empty() {
      return Err(MailError::InvalidRequest("body cannot be empty".to_owned()));
    }
    if body.contains('\x00') {
      return Err(MailError::InvalidRequest(
        "body contains invalid control characters".to_owned(),
      ));
    }
    Ok(body)
  }
}

impl MailAdapter for GmailAdapter {
  fn validate_account(
    &self,
  ) -> Pin<Box<dyn Future<Output = Result<ValidationResult, MailError>> + Send>> {
    let client = self.client.clone();
    let credential = self.credential.clone();
    let expected_email = self.expected_email.clone();

    Box::pin(async move {
      let access_token = credential.access_token(&client).await?;
      let response = client
        .get(format!("{GMAIL_BASE_URL}/profile"))
        .bearer_auth(&access_token)
        .send()
        .await
        .map_err(|error| MailError::Transport(error.to_string()))?;

      let response = Self::request_success(response)?;
      let profile = response
        .json::<GmailProfileResponse>()
        .await
        .map_err(|error| MailError::RequestFailed(error.to_string()))?;

      if !profile.email_address.eq_ignore_ascii_case(&expected_email) {
        return Err(MailError::InvalidRequest(
          "authenticated Gmail account does not match configured account email".to_owned(),
        ));
      }

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
    let credential = self.credential.clone();

    Box::pin(async move {
      let access_token = credential.access_token(&client).await?;

      let limit = enforce_request_limit(request.limit);
      let query = adapter.message_query(&request)?;
      let mut list_request = client
        .get(format!("{GMAIL_BASE_URL}/messages"))
        .bearer_auth(&access_token)
        .query(&[("maxResults", limit.to_string())]);

      if let Some(label) = request
        .label
        .as_deref()
        .filter(|value| !value.trim().is_empty())
      {
        let label = Self::sanitize_query_atom("label", label)?;
        list_request = list_request.query(&[("labelIds", label.as_str())]);
      }
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
        .map_err(|error| MailError::Transport(error.to_string()))?;
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

      let total = requested_ids.len();
      let mut pending = requested_ids
        .into_iter()
        .enumerate()
        .collect::<VecDeque<_>>();
      let mut tasks = tokio::task::JoinSet::new();
      let mut messages = vec![None; total];

      while !pending.is_empty() || !tasks.is_empty() {
        while tasks.len() < SUMMARY_FETCH_CONCURRENCY {
          let Some((index, message_id)) = pending.pop_front() else {
            break;
          };
          let client = client.clone();
          let access_token = access_token.clone();
          tasks.spawn(async move {
            Self::fetch_message_summary(client, access_token, message_id)
              .await
              .map(|summary| (index, summary))
          });
        }

        let Some(result) = tasks.join_next().await else {
          continue;
        };
        let (index, summary) =
          result.map_err(|error| MailError::RequestFailed(error.to_string()))??;
        messages[index] = Some(summary);
      }

      Ok(MessageList {
        messages: messages.into_iter().flatten().collect(),
        next_page_token: response.next_page_token,
      })
    })
  }

  fn read_message(
    &self,
    request: ReadMessageRequest,
  ) -> Pin<Box<dyn Future<Output = Result<MessageContent, MailError>> + Send>> {
    let client = self.client.clone();
    let credential = self.credential.clone();
    Box::pin(async move {
      let message_id = Self::sanitize_message_id(&request.message_id)?;

      let access_token = credential.access_token(&client).await?;
      let response = client
        .get(format!("{GMAIL_BASE_URL}/messages/{message_id}"))
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
        .map_err(|error| MailError::Transport(error.to_string()))?;
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
    let credential = self.credential.clone();
    let from_address = self.from_address.clone();
    let expected_email = self.expected_email.clone();
    Box::pin(async move {
      let to = Self::sanitize_recipient_list("to", &request.to)?;
      let cc = request
        .cc
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| Self::sanitize_recipient_list("cc", value))
        .transpose()?;
      let bcc = request
        .bcc
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| Self::sanitize_recipient_list("bcc", value))
        .transpose()?;
      let subject = Self::sanitize_header_value("subject", &request.subject)?;
      let from_address = from_address
        .as_deref()
        .map(|value| Self::sanitize_header_value("from", value))
        .transpose()?
        .unwrap_or(expected_email);
      let body = Self::sanitize_message_body(&request.body)?;
      let content_type = match request.body_format.as_deref().unwrap_or("text/plain") {
        "text/plain" | "plain" => "text/plain",
        "text/html" | "html" => "text/html",
        other => {
          return Err(MailError::InvalidRequest(format!(
            "body_format '{other}' is not supported"
          )));
        }
      };
      let mut raw_message = format!("From: {from_address}\r\nTo: {to}\r\n");
      if let Some(cc) = cc {
        raw_message.push_str(&format!("Cc: {cc}\r\n"));
      }
      if let Some(bcc) = bcc {
        raw_message.push_str(&format!("Bcc: {bcc}\r\n"));
      }
      raw_message.push_str(&format!(
        "Subject: {subject}\r\n\
MIME-Version: 1.0\r\nContent-Type: {content_type}; charset=\"UTF-8\"\r\n\
Content-Transfer-Encoding: 8bit\r\n\r\n{body}"
      ));
      let payload = SendRequestBody {
        raw: URL_SAFE_NO_PAD.encode(raw_message.as_bytes()),
      };

      let access_token = credential.access_token(&client).await?;
      let response = client
        .post(format!("{GMAIL_BASE_URL}/messages/send"))
        .bearer_auth(&access_token)
        .json(&payload)
        .send()
        .await
        .map_err(|error| MailError::Transport(error.to_string()))?;
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

  fn set_read_state(
    &self,
    request: SetReadStateRequest,
  ) -> Pin<Box<dyn Future<Output = Result<StateMutationResult, MailError>> + Send>> {
    let client = self.client.clone();
    let credential = self.credential.clone();

    Box::pin(async move {
      let message_id = Self::sanitize_message_id(&request.message_id)?;

      let payload = ModifyMessageRequest {
        add_label_ids: if request.is_read {
          Vec::new()
        } else {
          vec!["UNREAD".to_owned()]
        },
        remove_label_ids: if request.is_read {
          vec!["UNREAD".to_owned()]
        } else {
          Vec::new()
        },
      };

      let access_token = credential.access_token(&client).await?;
      let response = client
        .post(format!("{GMAIL_BASE_URL}/messages/{message_id}/modify"))
        .bearer_auth(&access_token)
        .json(&payload)
        .send()
        .await
        .map_err(|error| MailError::Transport(error.to_string()))?;

      Self::request_success(response).map(|_| StateMutationResult {
        message_id,
        is_read: request.is_read,
      })
    })
  }

  fn refresh_message_summaries(
    &self,
    message_ids: Vec<String>,
  ) -> Pin<Box<dyn Future<Output = Result<Vec<MessageSummary>, MailError>> + Send>> {
    let client = self.client.clone();
    let credential = self.credential.clone();

    Box::pin(async move {
      if message_ids.is_empty() {
        return Ok(Vec::new());
      }

      let access_token = credential.access_token(&client).await?;
      let mut pending = message_ids.into_iter().enumerate().collect::<VecDeque<_>>();
      let mut tasks = tokio::task::JoinSet::new();
      let mut summaries = vec![None; pending.len()];

      while !pending.is_empty() || !tasks.is_empty() {
        while tasks.len() < SUMMARY_FETCH_CONCURRENCY {
          let Some((index, message_id)) = pending.pop_front() else {
            break;
          };
          let client = client.clone();
          let access_token = access_token.clone();
          tasks.spawn(async move {
            Self::fetch_message_summary(client, access_token, message_id)
              .await
              .map(|summary| (index, summary))
          });
        }

        let Some(result) = tasks.join_next().await else {
          continue;
        };
        let (index, summary) =
          result.map_err(|error| MailError::RequestFailed(error.to_string()))??;
        summaries[index] = Some(summary);
      }

      Ok(summaries.into_iter().flatten().collect())
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
  history_id: Option<String>,
  snippet: Option<String>,
  label_ids: Option<Vec<String>>,
  internal_date: Option<String>,
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
#[serde(rename_all = "camelCase")]
struct ModifyMessageRequest {
  add_label_ids: Vec<String>,
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
        "before:2026/02/01".to_owned()
      ]
    );
  }

  #[test]
  fn defaults_missing_list_window_to_recent_bounded_window() {
    let window = MessageDateWindow::default()
      .bounded_or_default()
      .expect("default window should be valid");

    assert!(window.start_unix.is_some());
    assert!(window.end_unix.is_some());
    assert!(
      window.end_unix.unwrap() - window.start_unix.unwrap()
        <= DEFAULT_WINDOW_DAYS * SECONDS_PER_DAY
    );
  }

  #[test]
  fn rejects_partial_date_window() {
    assert_eq!(
      MessageDateWindow::new(Some(1), None)
        .bounded_or_default()
        .unwrap_err(),
      MailError::InvalidRequest("date window must include both start_unix and end_unix".to_owned())
    );
  }

  #[test]
  fn maps_read_state_filters_to_gmail_terms() {
    assert_eq!(ReadStateFilter::Read.as_gmail_term(), "-is:unread");
    assert_eq!(ReadStateFilter::Unread.as_gmail_term(), "is:unread");
  }

  #[test]
  fn rejects_recipient_with_control_characters() {
    assert_eq!(
      GmailAdapter::sanitize_recipient_list("to", "test\r\n@example.com"),
      Err(MailError::InvalidRequest(
        "to contains invalid line breaks".to_owned()
      ))
    );
  }

  #[test]
  fn rejects_recipient_with_whitespace_or_invalid_domain() {
    for address in ["alice@example.com extra", "bad addr@example.com", "a@b."] {
      assert!(GmailAdapter::sanitize_recipient_list("to", address).is_err());
    }
  }

  #[test]
  fn rejects_non_ascii_subject_headers() {
    assert_eq!(
      GmailAdapter::sanitize_header_value("subject", "Zażółć"),
      Err(MailError::InvalidRequest(
        "subject must contain only ASCII characters".to_owned()
      ))
    );
  }

  #[test]
  fn enforces_request_limit_minimum_one() {
    assert_eq!(enforce_request_limit(Some(0)), 1);
    assert_eq!(enforce_request_limit(Some(1000)), MAX_LIST_LIMIT);
  }

  #[test]
  fn parses_refreshable_oauth_bundle_without_exposing_secret() {
    let credential = GmailCredential::parse(
      r#"{
        "refresh_token": "refresh-token",
        "client_id": "client-id",
        "client_secret": "client-secret"
      }"#,
    )
    .expect("oauth bundle should parse");

    assert!(matches!(credential, GmailCredential::Refreshable(_)));
  }

  #[test]
  fn missing_bundle_expiry_treats_cached_access_token_as_stale() {
    let bundle = OAuthTokenBundle {
      access_token: Some("cached-access-token".to_owned()),
      refresh_token: "refresh-token".to_owned(),
      client_id: "client-id".to_owned(),
      client_secret: "client-secret".to_owned(),
      token_uri: Some(GMAIL_TOKEN_URL.to_owned()),
      expires_at_unix: None,
    };

    assert_eq!(bundle.valid_cached_access_token(1_770_000_000), None);
  }

  #[test]
  fn bundle_access_token_must_outlive_refresh_skew() {
    let mut bundle = OAuthTokenBundle {
      access_token: Some("cached-access-token".to_owned()),
      refresh_token: "refresh-token".to_owned(),
      client_id: "client-id".to_owned(),
      client_secret: "client-secret".to_owned(),
      token_uri: Some(GMAIL_TOKEN_URL.to_owned()),
      expires_at_unix: Some(1_770_000_061),
    };

    assert_eq!(
      bundle.valid_cached_access_token(1_770_000_000),
      Some("cached-access-token")
    );

    bundle.expires_at_unix = Some(1_770_000_060);
    assert_eq!(bundle.valid_cached_access_token(1_770_000_000), None);

    bundle.access_token = Some("   ".to_owned());
    bundle.expires_at_unix = Some(1_770_000_061);
    assert_eq!(bundle.valid_cached_access_token(1_770_000_000), None);
  }

  #[test]
  fn rejects_custom_oauth_token_uri() {
    assert!(matches!(
      GmailCredential::parse(
        r#"{
          "refresh_token": "refresh-token",
          "client_id": "client-id",
          "client_secret": "client-secret",
          "token_uri": "https://example.com/token"
        }"#,
      ),
      Err(MailError::InvalidRequest(message))
        if message == format!("oauth token bundle token_uri must be {GMAIL_TOKEN_URL}")
    ));
  }

  #[test]
  fn accepts_google_oauth_token_uri() {
    let credential = GmailCredential::parse(
      r#"{
        "refresh_token": "refresh-token",
        "client_id": "client-id",
        "client_secret": "client-secret",
        "token_uri": "https://oauth2.googleapis.com/token"
      }"#,
    )
    .expect("oauth bundle should parse");

    assert!(matches!(credential, GmailCredential::Refreshable(_)));
  }

  #[test]
  fn rejects_empty_send_body() {
    assert_eq!(
      GmailAdapter::sanitize_message_body("  \r\n\t  "),
      Err(MailError::InvalidRequest("body cannot be empty".to_owned()))
    );
  }

  #[test]
  fn rejects_message_ids_that_are_not_single_path_segments() {
    for message_id in ["abc/def", "abc?format=full", "abc#fragment", "../abc"] {
      assert_eq!(
        GmailAdapter::sanitize_message_id(message_id),
        Err(MailError::InvalidRequest(
          "message_id contains unsupported path characters".to_owned()
        ))
      );
    }

    assert_eq!(
      GmailAdapter::sanitize_message_id("abc_DEF-123").unwrap(),
      "abc_DEF-123"
    );
  }

  #[test]
  fn accepts_gmail_label_ids_as_query_atoms() {
    assert_eq!(
      GmailAdapter::sanitize_query_atom("label", "Label_123").unwrap(),
      "Label_123"
    );
    assert!(GmailAdapter::sanitize_query_atom("label", "bad label").is_err());
  }

  #[test]
  fn gmail_metadata_fields_include_remote_version_marker() {
    assert!(GMAIL_METADATA_FIELDS.contains("historyId"));
  }

  #[test]
  fn gmail_status_mapping_keeps_client_errors_non_cacheable() {
    assert_eq!(
      GmailAdapter::status_success(StatusCode::BAD_REQUEST),
      Err(MailError::InvalidRequest(
        "gmail request was rejected with status 400 Bad Request".to_owned()
      ))
    );
    assert_eq!(
      GmailAdapter::status_success(StatusCode::BAD_GATEWAY),
      Err(MailError::ServiceUnavailable)
    );
  }
}
