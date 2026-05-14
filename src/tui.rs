use std::{
  path::PathBuf,
  sync::mpsc::{self, TryRecvError},
  thread,
  time::Duration,
};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
  DefaultTerminal, Frame,
  layout::{Constraint, Direction, Layout},
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{Block, Borders, List, ListItem, Paragraph},
};

use crate::{
  config::{AccountConfig, AuthConfig, AuthKind, Config, Provider},
  mail,
  permissions::Permission,
};

const FIELD_COUNT: usize = 7;
const VALIDATION_TICK: Duration = Duration::from_millis(100);
const VALIDATION_SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

struct TerminalGuard;

impl TerminalGuard {
  fn enter() -> (Self, DefaultTerminal) {
    let terminal = ratatui::init();
    (Self, terminal)
  }
}

impl Drop for TerminalGuard {
  fn drop(&mut self) {
    ratatui::restore();
  }
}

struct App {
  database_path: PathBuf,
  config: Config,
  selected: usize,
  message: String,
  mode: Mode,
}

enum Mode {
  List,
  Form(AccountForm),
  Validating(ValidationState),
}

#[derive(Clone)]
struct AccountForm {
  original_id: Option<String>,
  id: String,
  email: String,
  provider_index: usize,
  auth_kind_index: usize,
  username: String,
  secret: String,
  permissions: Vec<bool>,
  permission_focus: usize,
  focus: usize,
  message: String,
}

struct ValidationState {
  form: AccountForm,
  original_id: Option<String>,
  account: AccountConfig,
  receiver: mpsc::Receiver<Result<(), mail::MailError>>,
  spinner_index: usize,
}

impl App {
  fn load(database_path: PathBuf) -> Result<Self> {
    let config = Config::load_or_default(&database_path)?;
    Ok(Self {
      database_path,
      config,
      selected: 0,
      message: "q quit | a add | e edit | d delete | r reload".to_owned(),
      mode: Mode::List,
    })
  }

  fn reload(&mut self) -> Result<()> {
    self.config = Config::load_or_default(&self.database_path)?;
    self.selected = self
      .selected
      .min(self.config.accounts.len().saturating_sub(1));
    self.message = "Database reloaded.".to_owned();
    Ok(())
  }

  fn selected_account(&self) -> Option<&AccountConfig> {
    self.config.accounts.get(self.selected)
  }

  fn select_next(&mut self) {
    if self.config.accounts.is_empty() {
      return;
    }
    self.selected = (self.selected + 1).min(self.config.accounts.len() - 1);
  }

  fn select_previous(&mut self) {
    self.selected = self.selected.saturating_sub(1);
  }

  fn save(&self) -> Result<()> {
    self.config.save(&self.database_path)
  }

  fn open_add_form(&mut self) {
    self.mode = Mode::Form(AccountForm::new(None));
  }

  fn open_edit_form(&mut self) {
    if let Some(account) = self.selected_account().cloned() {
      self.mode = Mode::Form(AccountForm::new(Some(account)));
    }
  }

  fn begin_save_form(&mut self, form: AccountForm) -> Result<()> {
    let original_id = form.original_id.clone();
    let account = form.clone().into_account();

    if requires_gmail_identity_validation(&account) {
      self.start_gmail_identity_validation(form, original_id, account);
      return Ok(());
    }

    self.finish_save_form(original_id, account)
  }

  fn start_gmail_identity_validation(
    &mut self,
    form: AccountForm,
    original_id: Option<String>,
    account: AccountConfig,
  ) {
    let expected_email = account.email.clone();
    let secret = account.auth.secret.clone();
    let (sender, receiver) = mpsc::channel();

    thread::spawn(move || {
      let result = mail::validate_gmail_identity_blocking(&expected_email, &secret);
      let _ = sender.send(result);
    });

    self.mode = Mode::Validating(ValidationState {
      form,
      original_id,
      account,
      receiver,
      spinner_index: 0,
    });
  }

  fn finish_save_form(
    &mut self,
    original_id: Option<String>,
    account: AccountConfig,
  ) -> Result<()> {
    let id = account.id.clone();

    if let Some(original_id) = original_id
      && original_id != id
    {
      self.config.remove_account(&original_id)?;
    }

    self.config.upsert_account(account)?;
    self.save()?;
    self.selected = self
      .config
      .accounts
      .iter()
      .position(|account| account.id == id)
      .unwrap_or(0);
    self.message = format!("Saved account '{id}'.");
    self.mode = Mode::List;
    Ok(())
  }

  fn poll_validation(&mut self) {
    let outcome = match &mut self.mode {
      Mode::Validating(state) => {
        state.advance_spinner();
        match state.receiver.try_recv() {
          Ok(result) => Some(result),
          Err(TryRecvError::Empty) => None,
          Err(TryRecvError::Disconnected) => Some(Err(mail::MailError::RequestFailed(
            "gmail identity validation was interrupted".to_owned(),
          ))),
        }
      }
      Mode::List | Mode::Form(_) => None,
    };

    let Some(outcome) = outcome else {
      return;
    };

    let mode = std::mem::replace(&mut self.mode, Mode::List);
    let Mode::Validating(state) = mode else {
      return;
    };

    match outcome {
      Ok(()) => {
        let retry_form = state.form;
        if let Err(error) = self.finish_save_form(state.original_id, state.account) {
          self.mode = Mode::Form(retry_form);
          if let Mode::Form(form) = &mut self.mode {
            form.message = format!("Error: {error}");
          }
        }
      }
      Err(error) => {
        self.mode = Mode::Form(state.form);
        if let Mode::Form(form) = &mut self.mode {
          form.message = gmail_validation_failure_message(&error);
        }
      }
    }
  }

  fn cancel_validation(&mut self) {
    let mode = std::mem::replace(&mut self.mode, Mode::List);
    if let Mode::Validating(state) = mode {
      self.mode = Mode::Form(state.form);
      if let Mode::Form(form) = &mut self.mode {
        form.message =
          "Validation canceled. Press Enter to try again or Esc to discard changes.".to_owned();
      }
    }
  }
}

impl AccountForm {
  fn new(existing: Option<AccountConfig>) -> Self {
    let providers = Provider::variants();
    let auth_kinds = AuthKind::variants();
    let permission_variants = Permission::variants();

    if let Some(account) = existing {
      let provider_index = providers
        .iter()
        .position(|provider| *provider == account.provider)
        .unwrap_or(0);
      let auth_kind_index = auth_kinds
        .iter()
        .position(|kind| *kind == account.auth.kind)
        .unwrap_or(0);
      let permissions = permission_variants
        .iter()
        .map(|permission| account.permissions.contains(permission))
        .collect();

      return Self {
        original_id: Some(account.id.clone()),
        id: account.id,
        email: account.email,
        provider_index,
        auth_kind_index,
        username: account.auth.username.unwrap_or_default(),
        secret: account.auth.secret,
        permissions,
        permission_focus: 0,
        focus: 0,
        message: "Enter saves | Esc cancels | Tab moves | Space toggles permissions".to_owned(),
      };
    }

    Self {
      original_id: None,
      id: String::new(),
      email: String::new(),
      provider_index: 0,
      auth_kind_index: 0,
      username: String::new(),
      secret: String::new(),
      permissions: vec![false; permission_variants.len()],
      permission_focus: 0,
      focus: 0,
      message: "Enter saves | Esc cancels | Tab moves | Space toggles permissions".to_owned(),
    }
  }

  fn into_account(self) -> AccountConfig {
    let provider = Provider::variants()[self.provider_index];
    let auth_kind = AuthKind::variants()[self.auth_kind_index];
    let permissions = Permission::variants()
      .into_iter()
      .zip(self.permissions)
      .filter_map(|(permission, selected)| selected.then_some(permission))
      .collect();

    AccountConfig {
      id: self.id,
      email: self.email,
      provider,
      permissions,
      auth: AuthConfig {
        kind: auth_kind,
        username: Some(self.username),
        secret: self.secret,
      },
    }
  }

  fn handle_key(&mut self, key: KeyEvent) -> FormAction {
    match key.code {
      KeyCode::Esc => return FormAction::Cancel,
      KeyCode::Enter => return FormAction::Save,
      KeyCode::Tab | KeyCode::Down => self.focus_next(),
      KeyCode::BackTab | KeyCode::Up => self.focus_previous(),
      KeyCode::Left => self.adjust_choice(false),
      KeyCode::Right => self.adjust_choice(true),
      KeyCode::Char(' ') if self.focus == 6 => self.toggle_focused_permission(),
      KeyCode::Char(character) => self.push_character(character, key.modifiers),
      KeyCode::Backspace => self.backspace(),
      _ => {}
    }

    FormAction::Continue
  }

  fn focus_next(&mut self) {
    self.focus = (self.focus + 1) % FIELD_COUNT;
  }

  fn focus_previous(&mut self) {
    self.focus = if self.focus == 0 {
      FIELD_COUNT - 1
    } else {
      self.focus - 1
    };
  }

  fn adjust_choice(&mut self, forward: bool) {
    match self.focus {
      2 => {
        self.provider_index =
          adjusted_index(self.provider_index, Provider::variants().len(), forward);
      }
      3 => {
        self.auth_kind_index =
          adjusted_index(self.auth_kind_index, AuthKind::variants().len(), forward);
      }
      6 => self.move_permission_focus(forward),
      _ => {}
    }
  }

  fn move_permission_focus(&mut self, forward: bool) {
    if self.permissions.is_empty() {
      return;
    }

    self.permission_focus = adjusted_index(self.permission_focus, self.permissions.len(), forward);
  }

  fn toggle_focused_permission(&mut self) {
    if self.permissions.is_empty() {
      return;
    }

    self.permissions[self.permission_focus] = !self.permissions[self.permission_focus];
  }

  fn push_character(&mut self, character: char, modifiers: KeyModifiers) {
    if modifiers.contains(KeyModifiers::CONTROL) {
      return;
    }

    match self.focus {
      0 => self.id.push(character),
      1 => self.email.push(character),
      4 => self.username.push(character),
      5 => self.secret.push(character),
      _ => {}
    }
  }

  fn backspace(&mut self) {
    match self.focus {
      0 => {
        self.id.pop();
      }
      1 => {
        self.email.pop();
      }
      4 => {
        self.username.pop();
      }
      5 => {
        self.secret.pop();
      }
      _ => {}
    }
  }
}

enum FormAction {
  Continue,
  Save,
  Cancel,
}

impl ValidationState {
  fn advance_spinner(&mut self) {
    self.spinner_index = (self.spinner_index + 1) % VALIDATION_SPINNER.len();
  }

  fn message(&self) -> String {
    format!(
      "{} Validating Gmail identity with Google. Press Esc to cancel.",
      VALIDATION_SPINNER[self.spinner_index]
    )
  }
}

pub fn run(database_path: PathBuf) -> Result<()> {
  let mut app = App::load(database_path)?;
  let (_guard, mut terminal) = TerminalGuard::enter();

  loop {
    app.poll_validation();
    terminal.draw(|frame| render(frame, &app))?;

    let event = if matches!(app.mode, Mode::Validating(_)) {
      if !event::poll(VALIDATION_TICK)? {
        continue;
      }
      event::read()?
    } else {
      event::read()?
    };

    if let Event::Key(key) = event {
      if key.kind != KeyEventKind::Press {
        continue;
      }
      if is_quit_key(key) {
        break;
      }

      match &mut app.mode {
        Mode::List => {
          if handle_list_key(&mut app, key)? {
            break;
          }
        }
        Mode::Form(form) => match form.handle_key(key) {
          FormAction::Continue => {}
          FormAction::Cancel => {
            app.message = "No changes made.".to_owned();
            app.mode = Mode::List;
          }
          FormAction::Save => {
            let form = std::mem::replace(&mut app.mode, Mode::List);
            if let Mode::Form(form) = form {
              let retry_form = form.clone();
              if let Err(error) = app.begin_save_form(form) {
                app.mode = Mode::Form(retry_form);
                if let Mode::Form(form) = &mut app.mode {
                  form.message = format!("Error: {error}");
                }
              }
            }
          }
        },
        Mode::Validating(_) => {
          if key.code == KeyCode::Esc {
            app.cancel_validation();
          }
        }
      }
    }
  }

  Ok(())
}

fn render(frame: &mut Frame, app: &App) {
  let areas = Layout::default()
    .direction(Direction::Vertical)
    .constraints([Constraint::Min(3), Constraint::Length(3)])
    .split(frame.area());

  match &app.mode {
    Mode::List => render_account_list(frame, areas[0], app),
    Mode::Form(form) => render_form(frame, areas[0], form),
    Mode::Validating(state) => render_form(frame, areas[0], &state.form),
  }

  let help_text = match &app.mode {
    Mode::List => app.message.clone(),
    Mode::Form(form) => form.message.clone(),
    Mode::Validating(state) => state.message(),
  };
  let help = Paragraph::new(Line::from(help_text)).block(Block::default().borders(Borders::ALL));

  frame.render_widget(help, areas[1]);
}

fn render_account_list(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
  let items = if app.config.accounts.is_empty() {
    vec![ListItem::new("No accounts configured.")]
  } else {
    app
      .config
      .accounts
      .iter()
      .enumerate()
      .map(|(index, account)| {
        let prefix = if index == app.selected { "> " } else { "  " };
        ListItem::new(format!(
          "{prefix}{} <{}> provider={} permissions={}",
          account.id,
          account.email,
          account.provider,
          account.permission_list()
        ))
      })
      .collect()
  };

  let list = List::new(items)
    .block(
      Block::default()
        .title("McpMailBridge Accounts")
        .borders(Borders::ALL),
    )
    .highlight_style(Style::default().add_modifier(Modifier::BOLD));

  frame.render_widget(list, area);
}

fn render_form(frame: &mut Frame, area: ratatui::layout::Rect, form: &AccountForm) {
  let providers = Provider::variants();
  let auth_kinds = AuthKind::variants();
  let permissions = Permission::variants();
  let secret = "*".repeat(form.secret.chars().count());
  let provider = providers[form.provider_index].to_string();
  let auth_kind = auth_kinds[form.auth_kind_index].to_string();
  let permission_labels = permission_labels(&permissions, &form.permissions, form.permission_focus);

  let lines = vec![
    form_line(
      0,
      form.focus,
      "Account id",
      &form.id,
      "Local alias used in commands and MCP requests, not your email.",
    ),
    form_line(1, form.focus, "Email", &form.email, ""),
    form_line(
      2,
      form.focus,
      "Provider",
      &provider,
      "Left/right changes value.",
    ),
    form_line(
      3,
      form.focus,
      "Auth kind",
      &auth_kind,
      "Left/right changes value.",
    ),
    form_line(4, form.focus, "Username", &form.username, ""),
    form_line(5, form.focus, "Secret or token", &secret, ""),
    form_line(
      6,
      form.focus,
      "Permissions",
      &permission_labels,
      "Left/right chooses, Space toggles.",
    ),
  ];

  let title = if form.original_id.is_some() {
    "Edit Account"
  } else {
    "Add Account"
  };
  let paragraph = Paragraph::new(lines).block(Block::default().title(title).borders(Borders::ALL));
  frame.render_widget(paragraph, area);
}

fn form_line<'a>(
  index: usize,
  focus: usize,
  label: &'a str,
  value: &'a str,
  hint: &'a str,
) -> Line<'a> {
  let marker = if index == focus { "> " } else { "  " };
  let label_style = if index == focus {
    Style::default()
      .fg(Color::Yellow)
      .add_modifier(Modifier::BOLD)
  } else {
    Style::default()
  };

  let mut spans = vec![
    Span::raw(marker),
    Span::styled(format!("{label}: "), label_style),
    Span::raw(value.to_owned()),
  ];
  if !hint.is_empty() {
    spans.push(Span::styled(
      format!("  {hint}"),
      Style::default().fg(Color::DarkGray),
    ));
  }

  Line::from(spans)
}

fn permission_labels(permissions: &[Permission], selected: &[bool], focus: usize) -> String {
  permissions
    .iter()
    .zip(selected)
    .enumerate()
    .map(|(index, (permission, selected))| {
      let marker = if *selected { "x" } else { " " };
      let focus_marker = if index == focus { ">" } else { " " };
      format!("{focus_marker}[{marker}] {permission}")
    })
    .collect::<Vec<_>>()
    .join(" ")
}

fn adjusted_index(index: usize, len: usize, forward: bool) -> usize {
  if len == 0 {
    return 0;
  }

  if forward {
    (index + 1) % len
  } else if index == 0 {
    len - 1
  } else {
    index - 1
  }
}

fn handle_list_key(app: &mut App, key: KeyEvent) -> Result<bool> {
  match key.code {
    KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
    KeyCode::Down | KeyCode::Char('j') => app.select_next(),
    KeyCode::Up | KeyCode::Char('k') => app.select_previous(),
    KeyCode::Char('r') => handle_result(app, |app| app.reload()),
    KeyCode::Char('a') => app.open_add_form(),
    KeyCode::Char('e') => app.open_edit_form(),
    KeyCode::Char('d') => {
      if let Some(account) = app.selected_account().cloned() {
        app.config.remove_account(&account.id)?;
        app.selected = app
          .selected
          .min(app.config.accounts.len().saturating_sub(1));
        handle_result(app, |app| app.save());
        app.message = format!("Removed account '{}'.", account.id);
      }
    }
    _ => {}
  }

  Ok(false)
}

fn is_quit_key(key: KeyEvent) -> bool {
  matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn handle_result(app: &mut App, action: impl FnOnce(&mut App) -> Result<()>) {
  if let Err(error) = action(app) {
    app.message = format!("Error: {error}");
  }
}

fn requires_gmail_identity_validation(account: &AccountConfig) -> bool {
  account.provider == Provider::Gmail && account.auth.kind == AuthKind::OAuthToken
}

fn gmail_validation_failure_message(error: &mail::MailError) -> String {
  match error {
    mail::MailError::Authorization => {
      "Gmail identity validation failed. Google rejected the OAuth token; refresh or re-issue the OAuth token bundle, then try again.".to_owned()
    }
    mail::MailError::MissingSecret => {
      "Gmail identity validation failed. Paste a Gmail OAuth token or token bundle, then try again.".to_owned()
    }
    mail::MailError::InvalidRequest(message) => {
      format!("Gmail identity validation failed. {message}")
    }
    mail::MailError::Transport(_) => {
      "Gmail identity validation failed. Google could not be reached; check your network connection, then try again.".to_owned()
    }
    mail::MailError::ServiceUnavailable => {
      "Gmail identity validation failed. Gmail is unavailable right now; wait a moment, then try again.".to_owned()
    }
    mail::MailError::NotFound => {
      "Gmail identity validation failed. Google could not find the Gmail profile; re-issue the OAuth token bundle for this account, then try again.".to_owned()
    }
    mail::MailError::RequestFailed(_) => {
      "Gmail identity validation failed. The Gmail response could not be read; try again, then re-issue the OAuth token bundle if it keeps failing.".to_owned()
    }
    mail::MailError::UnsupportedProvider(_) | mail::MailError::UnsupportedAuthKind(_) => {
      "Gmail identity validation failed. Review the provider and auth kind, then try again."
        .to_owned()
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn account(provider: Provider, auth_kind: AuthKind) -> AccountConfig {
    AccountConfig {
      id: "gmail".to_owned(),
      email: "user@example.com".to_owned(),
      provider,
      permissions: vec![Permission::Read],
      auth: AuthConfig {
        kind: auth_kind,
        username: None,
        secret: "local-secret".to_owned(),
      },
    }
  }

  #[test]
  fn gmail_oauth_accounts_require_identity_validation() {
    assert!(requires_gmail_identity_validation(&account(
      Provider::Gmail,
      AuthKind::OAuthToken
    )));
    assert!(!requires_gmail_identity_validation(&account(
      Provider::Gmail,
      AuthKind::Password
    )));
    assert!(!requires_gmail_identity_validation(&account(
      Provider::ImapSmtp,
      AuthKind::OAuthToken
    )));
  }

  #[test]
  fn gmail_validation_message_hides_transport_details() {
    let message = gmail_validation_failure_message(&mail::MailError::Transport(
      "failed with refresh-token and client-secret".to_owned(),
    ));

    assert!(message.contains("check your network connection"));
    assert!(!message.contains("refresh-token"));
    assert!(!message.contains("client-secret"));
  }

  #[test]
  fn gmail_validation_message_hides_request_details() {
    let message = gmail_validation_failure_message(&mail::MailError::RequestFailed(
      "bad response with access-token".to_owned(),
    ));

    assert!(message.contains("try again"));
    assert!(!message.contains("access-token"));
  }

  #[test]
  fn gmail_validation_message_gives_auth_next_action() {
    let message = gmail_validation_failure_message(&mail::MailError::Authorization);

    assert!(message.contains("refresh or re-issue the OAuth token bundle"));
  }

  #[test]
  fn gmail_validation_message_gives_missing_secret_next_action() {
    let message = gmail_validation_failure_message(&mail::MailError::MissingSecret);

    assert!(message.contains("Paste a Gmail OAuth token or token bundle"));
  }

  #[test]
  fn gmail_validation_message_gives_service_next_action() {
    let message = gmail_validation_failure_message(&mail::MailError::ServiceUnavailable);

    assert!(message.contains("wait a moment"));
  }
}
