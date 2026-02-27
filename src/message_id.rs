//! Stable, opaque message identifier parsing and encoding
//!
//! Provides a message ID format that encodes account, mailbox,
//! UIDVALIDITY, and UID into a stable string. Mailbox names may
//! contain colons, which are preserved during parsing.

use serde::{Deserialize, Serialize};

use crate::errors::{AppError, AppResult};

/// Stable message identifier
///
/// Encodes all necessary information to locate a message in an IMAP
/// account. This format is opaque but parse-able for validation.
///
/// # Format
///
/// `imap:{account_id}:{mailbox}:{uidvalidity}:{uid}`
///
/// The `mailbox` segment may contain colons internally (e.g.,
/// `Projects:2026:Q1`). All trailing segments after `account_id`
/// are joined with colons.
///
/// # Example
///
/// ```
/// imap:default:INBOX:12345:42
/// imap:acct:Projects:2026:Q1:999:7
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageId {
    /// Account identifier
    pub account_id: String,
    /// Mailbox name (may contain colons)
    pub mailbox: String,
    /// IMAP UIDVALIDITY (mailbox snapshot identifier)
    pub uidvalidity: u32,
    /// Message UID within mailbox
    pub uid: u32,
}

impl MessageId {
    /// Parse message ID from string
    ///
    /// Validates format and extracts components. Returns error if:
    /// - Does not start with `imap:`
    /// - Has fewer than 5 segments
    /// - UID or UIDVALIDITY are not valid integers
    /// - Mailbox segment is empty
    ///
    /// # Example
    ///
    /// ```
    /// let id = MessageId::parse("imap:default:INBOX:123:42")?;
    /// assert_eq!(id.account_id, "default");
    /// assert_eq!(id.mailbox, "INBOX");
    /// assert_eq!(id.uidvalidity, 123);
    /// assert_eq!(id.uid, 42);
    /// ```
    pub fn parse(raw: &str) -> AppResult<Self> {
        let mut parts: Vec<&str> = raw.split(':').collect();
        if parts.len() < 5 {
            return Err(AppError::invalid(
                "message_id must have at least 5 segments",
            ));
        }
        if parts[0] != "imap" {
            return Err(AppError::invalid("message_id must start with 'imap'"));
        }

        let uid = parts
            .pop()
            .ok_or_else(|| AppError::invalid("missing uid"))?
            .parse::<u32>()
            .map_err(|_| AppError::invalid("invalid uid in message_id"))?;

        let uidvalidity = parts
            .pop()
            .ok_or_else(|| AppError::invalid("missing uidvalidity"))?
            .parse::<u32>()
            .map_err(|_| AppError::invalid("invalid uidvalidity in message_id"))?;

        let account_id = parts
            .get(1)
            .ok_or_else(|| AppError::invalid("missing account_id"))?
            .to_string();
        let mailbox = parts[2..].join(":");
        if mailbox.is_empty() {
            return Err(AppError::invalid("message_id mailbox cannot be empty"));
        }

        Ok(Self {
            account_id,
            mailbox,
            uidvalidity,
            uid,
        })
    }

    /// Encode message ID to string
    ///
    /// Produces the canonical string format.
    ///
    /// # Example
    ///
    /// ```
    /// let id = MessageId {
    ///     account_id: "default".to_owned(),
    ///     mailbox: "INBOX".to_owned(),
    ///     uidvalidity: 123,
    ///     uid: 42,
    /// };
    /// assert_eq!(id.encode(), "imap:default:INBOX:123:42");
    /// ```
    pub fn encode(&self) -> String {
        format!(
            "imap:{}:{}:{}:{}",
            self.account_id, self.mailbox, self.uidvalidity, self.uid
        )
    }
}

#[cfg(test)]
mod tests {
    use super::MessageId;

    #[test]
    fn parses_and_encodes_standard_message_id() {
        let id = MessageId::parse("imap:default:INBOX:123:42").expect("parse succeeds");
        assert_eq!(id.account_id, "default");
        assert_eq!(id.mailbox, "INBOX");
        assert_eq!(id.uidvalidity, 123);
        assert_eq!(id.uid, 42);
        assert_eq!(id.encode(), "imap:default:INBOX:123:42");
    }

    #[test]
    fn parses_mailbox_with_colons() {
        let id = MessageId::parse("imap:acct:Projects:2026:Q1:999:7").expect("parse succeeds");
        assert_eq!(id.account_id, "acct");
        assert_eq!(id.mailbox, "Projects:2026:Q1");
        assert_eq!(id.uidvalidity, 999);
        assert_eq!(id.uid, 7);
    }

    #[test]
    fn rejects_invalid_prefix() {
        let err = MessageId::parse("smtp:default:INBOX:123:1").expect_err("must fail");
        assert!(err.to_string().contains("must start with 'imap'"));
    }
}
