//! Configuration module for IMAP accounts and server settings
//!
//! All configuration is loaded from environment variables following the pattern
//! `MAIL_IMAP_<SEGMENT>_<KEY>`. Account segments are discovered by scanning for
//! `MAIL_IMAP_*_HOST` variables.

use std::collections::BTreeMap;
use std::env;
use std::env::VarError;

use regex::Regex;
use secrecy::SecretString;

use crate::errors::{AppError, AppResult};

/// IMAP account configuration
///
/// Holds connection details and credentials for a single IMAP account.
/// Passwords are stored using `SecretString` to prevent accidental logging.
#[derive(Debug, Clone)]
pub struct AccountConfig {
    /// Account identifier (lowercase, used as default `account_id` parameter)
    pub account_id: String,
    /// IMAP server hostname
    pub host: String,
    /// IMAP server port (typically 993 for TLS)
    pub port: u16,
    /// Whether to use TLS (currently enforced to `true`)
    pub secure: bool,
    /// Username for authentication
    pub user: String,
    /// Password stored in a type that prevents accidental logging
    pub pass: SecretString,
}

/// Server-wide configuration
///
/// Wraps all account configs and global server settings. Cloned into MCP tool
/// handlers via `Arc` for thread-safe shared access.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// All configured accounts, keyed by `account_id`
    pub accounts: BTreeMap<String, AccountConfig>,
    /// Whether write operations (copy, move, delete, flag updates) are enabled
    pub write_enabled: bool,
    /// TCP connection timeout in milliseconds
    pub connect_timeout_ms: u64,
    /// IMAP greeting/TLS handshake timeout in milliseconds
    pub greeting_timeout_ms: u64,
    /// Socket I/O timeout in milliseconds
    pub socket_timeout_ms: u64,
    /// Time-to-live for search cursors in seconds
    pub cursor_ttl_seconds: u64,
    /// Maximum number of cursors to retain (LRU eviction when exceeded)
    pub cursor_max_entries: usize,
}

impl ServerConfig {
    /// Load all configuration from environment variables
    ///
    /// Discovers accounts by scanning for `MAIL_IMAP_*_HOST` patterns.
    /// If no accounts are explicitly defined, a `default` account is required
    /// via `MAIL_IMAP_DEFAULT_HOST`, `MAIL_IMAP_DEFAULT_USER`, and
    /// `MAIL_IMAP_DEFAULT_PASS`.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` if required environment variables are missing
    /// or malformed.
    ///
    /// # Example Environment
    ///
    /// ```text
    /// MAIL_IMAP_DEFAULT_HOST=imap.gmail.com
    /// MAIL_IMAP_DEFAULT_USER=user@gmail.com
    /// MAIL_IMAP_DEFAULT_PASS=app-password
    /// MAIL_IMAP_WORK_HOST=outlook.office365.com
    /// MAIL_IMAP_WORK_USER=user@company.com
    /// MAIL_IMAP_WORK_PASS=work-pass
    /// MAIL_IMAP_WRITE_ENABLED=false
    /// ```
    pub fn load_from_env() -> AppResult<Self> {
        let account_pattern = Regex::new(r"^MAIL_IMAP_([A-Z0-9_]+)_HOST$")
            .map_err(|e| AppError::Internal(format!("invalid account regex: {e}")))?;

        let mut account_segments: Vec<String> = env::vars()
            .filter_map(|(k, _)| {
                account_pattern
                    .captures(&k)
                    .and_then(|c| c.get(1).map(|m| m.as_str().to_owned()))
            })
            .collect();

        if account_segments.is_empty() {
            account_segments.push("DEFAULT".to_owned());
        }

        account_segments.sort();
        account_segments.dedup();

        let mut accounts = BTreeMap::new();
        for seg in account_segments {
            let account = load_account(&seg)?;
            accounts.insert(account.account_id.clone(), account);
        }

        Ok(Self {
            accounts,
            write_enabled: parse_bool_env("MAIL_IMAP_WRITE_ENABLED", false)?,
            connect_timeout_ms: parse_u64_env("MAIL_IMAP_CONNECT_TIMEOUT_MS", 30_000)?,
            greeting_timeout_ms: parse_u64_env("MAIL_IMAP_GREETING_TIMEOUT_MS", 15_000)?,
            socket_timeout_ms: parse_u64_env("MAIL_IMAP_SOCKET_TIMEOUT_MS", 300_000)?,
            cursor_ttl_seconds: parse_u64_env("MAIL_IMAP_CURSOR_TTL_SECONDS", 600)?,
            cursor_max_entries: parse_usize_env("MAIL_IMAP_CURSOR_MAX_ENTRIES", 512)?,
        })
    }

    /// Get account configuration by ID
    ///
    /// # Errors
    ///
    /// Returns `NotFound` if the account ID is not configured.
    pub fn get_account(&self, account_id: &str) -> AppResult<&AccountConfig> {
        self.accounts
            .get(account_id)
            .ok_or_else(|| AppError::NotFound(format!("account '{account_id}' is not configured")))
    }
}

/// Load a single account configuration from environment
///
/// Reads `MAIL_IMAP_<SEGMENT>_HOST`, `_USER`, `_PASS`, `_PORT`, and `_SECURE`.
/// Normalizes the segment name to lowercase for `account_id` (except `DEFAULT`
/// becomes `default`).
fn load_account(segment: &str) -> AppResult<AccountConfig> {
    let prefix = format!("MAIL_IMAP_{}_", sanitize_segment(segment));
    let host = required_env(&format!("{prefix}HOST"))?;
    let user = required_env(&format!("{prefix}USER"))?;
    let pass = required_env(&format!("{prefix}PASS"))?;

    Ok(AccountConfig {
        account_id: if segment == "DEFAULT" {
            "default".to_owned()
        } else {
            segment.to_ascii_lowercase()
        },
        host,
        port: parse_u16_env(&format!("{prefix}PORT"), 993)?,
        secure: parse_bool_env(&format!("{prefix}SECURE"), true)?,
        user,
        pass: SecretString::new(pass.into()),
    })
}

/// Read a required environment variable, returning error if missing or empty
fn required_env(key: &str) -> AppResult<String> {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(AppError::InvalidInput(format!(
            "missing required environment variable {key}"
        ))),
    }
}

/// Sanitize an account segment to uppercase alphanumeric/underscore
///
/// Non-alphanumeric characters are replaced with underscores, and leading/
/// trailing underscores are trimmed.
fn sanitize_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for ch in seg.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
        } else {
            out.push('_');
        }
    }
    out.trim_matches('_').to_owned()
}

/// Parse a boolean environment variable with flexible values
///
/// Accepts: `1`, `true`, `yes`, `y`, `on` (truthy) or `0`, `false`, `no`,
/// `n`, `off` (falsy). Case-insensitive. Returns `default` if unset.
///
/// # Errors
///
/// Returns `InvalidInput` if the variable is set to an unrecognized value.
fn parse_bool_env(key: &str, default: bool) -> AppResult<bool> {
    match env::var(key) {
        Ok(v) => parse_bool_value(&v).ok_or_else(|| {
            AppError::InvalidInput(format!("invalid boolean environment variable {key}: '{v}'"))
        }),
        Err(VarError::NotPresent) => Ok(default),
        Err(VarError::NotUnicode(_)) => Err(AppError::InvalidInput(format!(
            "environment variable {key} contains non-unicode data"
        ))),
    }
}

fn parse_bool_value(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// Parse a `u16` environment variable with default fallback
///
/// Returns `default` if unset.
///
/// # Errors
///
/// Returns `InvalidInput` if the variable is set but not a valid `u16`.
fn parse_u16_env(key: &str, default: u16) -> AppResult<u16> {
    match env::var(key) {
        Ok(v) => v.parse::<u16>().map_err(|_| {
            AppError::InvalidInput(format!("invalid u16 environment variable {key}: '{v}'"))
        }),
        Err(VarError::NotPresent) => Ok(default),
        Err(VarError::NotUnicode(_)) => Err(AppError::InvalidInput(format!(
            "environment variable {key} contains non-unicode data"
        ))),
    }
}

/// Parse a `u64` environment variable with default fallback
///
/// Returns `default` if unset.
///
/// # Errors
///
/// Returns `InvalidInput` if the variable is set but not a valid `u64`.
fn parse_u64_env(key: &str, default: u64) -> AppResult<u64> {
    match env::var(key) {
        Ok(v) => v.parse::<u64>().map_err(|_| {
            AppError::InvalidInput(format!("invalid u64 environment variable {key}: '{v}'"))
        }),
        Err(VarError::NotPresent) => Ok(default),
        Err(VarError::NotUnicode(_)) => Err(AppError::InvalidInput(format!(
            "environment variable {key} contains non-unicode data"
        ))),
    }
}

/// Parse a `usize` environment variable with default fallback
///
/// Returns `default` if unset.
///
/// # Errors
///
/// Returns `InvalidInput` if the variable is set but not a valid `usize`.
fn parse_usize_env(key: &str, default: usize) -> AppResult<usize> {
    match env::var(key) {
        Ok(v) => v.parse::<usize>().map_err(|_| {
            AppError::InvalidInput(format!("invalid usize environment variable {key}: '{v}'"))
        }),
        Err(VarError::NotPresent) => Ok(default),
        Err(VarError::NotUnicode(_)) => Err(AppError::InvalidInput(format!(
            "environment variable {key} contains non-unicode data"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_bool_value;

    #[test]
    fn parse_bool_value_accepts_common_truthy_and_falsy_values() {
        for truthy in ["1", "true", "TRUE", " yes ", "Y", "on"] {
            assert_eq!(parse_bool_value(truthy), Some(true));
        }

        for falsy in ["0", "false", "FALSE", " no ", "N", "off"] {
            assert_eq!(parse_bool_value(falsy), Some(false));
        }
    }

    #[test]
    fn parse_bool_value_rejects_unrecognized_values() {
        for invalid in ["", "2", "maybe", "enabled", "disabled"] {
            assert_eq!(parse_bool_value(invalid), None);
        }
    }
}
