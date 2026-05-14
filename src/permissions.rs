use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
  Search,
  Read,
  Send,
  MarkAsRead,
  MarkAsUnread,
}

impl Permission {
  pub fn variants() -> Vec<Self> {
    vec![
      Self::Search,
      Self::Read,
      Self::Send,
      Self::MarkAsRead,
      Self::MarkAsUnread,
    ]
  }

  pub fn parse(value: &str) -> Result<Self, String> {
    match value {
      "search" => Ok(Self::Search),
      "read" => Ok(Self::Read),
      "send" | "write" => Ok(Self::Send),
      "mark_as_read" => Ok(Self::MarkAsRead),
      "mark_as_unread" => Ok(Self::MarkAsUnread),
      _ => Err(format!("unknown permission '{value}'")),
    }
  }
}

impl fmt::Display for Permission {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Search => formatter.write_str("search"),
      Self::Read => formatter.write_str("read"),
      Self::Send => formatter.write_str("send"),
      Self::MarkAsRead => formatter.write_str("mark_as_read"),
      Self::MarkAsUnread => formatter.write_str("mark_as_unread"),
    }
  }
}
