use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
  Read,
  Write,
  MarkAsRead,
}

impl Permission {
  pub fn variants() -> Vec<Self> {
    vec![Self::Read, Self::Write, Self::MarkAsRead]
  }

  pub fn parse(value: &str) -> Result<Self, String> {
    match value {
      "read" => Ok(Self::Read),
      "write" => Ok(Self::Write),
      "mark_as_read" => Ok(Self::MarkAsRead),
      _ => Err(format!("unknown permission '{value}'")),
    }
  }
}

impl fmt::Display for Permission {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Read => formatter.write_str("read"),
      Self::Write => formatter.write_str("write"),
      Self::MarkAsRead => formatter.write_str("mark_as_read"),
    }
  }
}
