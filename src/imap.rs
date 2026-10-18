//! IMAP transport and session operations
//!
//! Provides timeout-bounded wrappers around `async-imap` operations. All network
//! calls are enforced to use TLS, and timeouts are derived from server config.

use std::sync::Arc;
use std::time::Duration;

use async_imap::types::{Fetch, Name};
use async_imap::{Client, Session};
use futures::{StreamExt, TryStreamExt};
use rustls::ClientConfig;
use rustls::RootCertStore;
use rustls_pki_types::ServerName;
use secrecy::ExposeSecret;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use crate::config::{AccountConfig, ServerConfig};
use crate::errors::{AppError, AppResult};
use crate::mailbox_codec::encode_mailbox_name_for_command;
use crate::mailbox_codec::normalize_mailbox_name;

use crate::models::MailboxInfo;
use crate::server::provider;

mod commands;

pub use commands::{
    CommandFailure, CopyMoveOutcome, CopyUidMapping, MessageMetadata, MetadataProjection,
    fetch_message_metadata, mailbox_status, store_message_labels, uid_copy_sequence,
    uid_expunge_sequence, uid_move_sequence, uid_store_sequence,
};

/// Maximum LIST observations: 2,000 retained mailboxes plus one truncation sentinel.
pub const MAILBOX_LIST_OBSERVATION_LIMIT: usize = 2_001;
const MAX_MAILBOXES_RETAINED: usize = MAILBOX_LIST_OBSERVATION_LIMIT - 1;

#[derive(Debug)]
pub struct MailboxListObservation {
    pub items: Vec<Name>,
    pub truncated: bool,
}

fn bounded_mailbox_observation<T>(mut items: Vec<T>) -> (Vec<T>, bool) {
    let truncated = items.len() >= MAILBOX_LIST_OBSERVATION_LIMIT;
    items.truncate(MAX_MAILBOXES_RETAINED);
    (items, truncated)
}

/// Type alias for authenticated IMAP session over TLS
///
/// Wraps the TLS stream type to simplify signatures throughout the codebase.
pub type ImapSession = Session<tokio_rustls::client::TlsStream<TcpStream>>;

/// Get socket timeout duration from server config
///
/// Helper to avoid repeatedly accessing the config field.
fn socket_timeout(server: &ServerConfig) -> Duration {
    Duration::from_millis(server.socket_timeout_ms)
}

/// Connect to IMAP server and authenticate
///
/// Performs full connection sequence with timeouts:
/// 1. TCP connect
/// 2. TLS handshake with system root certificates
/// 3. Read IMAP greeting
/// 4. LOGIN authentication
///
/// # Security
///
/// Rejects insecure connections (`secure: false`) to prevent password exposure.
///
/// # Timeouts
///
/// - TCP connect: `connect_timeout_ms`
/// - TLS handshake: `greeting_timeout_ms`
/// - Greeting read: `greeting_timeout_ms`
/// - LOGIN: `greeting_timeout_ms`
///
/// # Errors
///
/// - `InvalidInput` if `secure` is false or hostname is invalid for TLS SNI
/// - `Timeout` if any connection phase times out
/// - `AuthFailed` if authentication fails
/// - `Internal` for TCP, TLS, or greeting failures
pub async fn connect_authenticated(
    server: &ServerConfig,
    account: &AccountConfig,
) -> AppResult<ImapSession> {
    if !account.secure {
        return Err(AppError::InvalidInput(
            "insecure IMAP is not supported; set MAIL_IMAP_<ACCOUNT>_SECURE=true".to_owned(),
        ));
    }

    let connect_duration = Duration::from_millis(server.connect_timeout_ms);
    let greeting_duration = Duration::from_millis(server.greeting_timeout_ms);

    let tcp = timeout(
        connect_duration,
        TcpStream::connect((account.host.as_str(), account.port)),
    )
    .await
    .map_err(|_| AppError::Timeout("tcp connect timeout".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("tcp connect failed: {e}"))))?;

    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots.add_parsable_certificates(server.trusted_ca_certs.iter().cloned());
    let tls_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(tls_config));

    let server_name = ServerName::try_from(account.host.clone())
        .map_err(|_| AppError::InvalidInput("invalid IMAP host for TLS SNI".to_owned()))?;
    let tls_stream = timeout(greeting_duration, connector.connect(server_name, tcp))
        .await
        .map_err(|_| AppError::Timeout("TLS handshake timeout".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("TLS handshake failed: {e}"))))?;

    let mut client = Client::new(tls_stream);
    let greeting = timeout(greeting_duration, client.read_response())
        .await
        .map_err(|_| AppError::Timeout("IMAP greeting timeout".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("IMAP greeting failed: {e}"))))?;

    if greeting.is_none() {
        return Err(AppError::Internal(
            "IMAP server closed connection before greeting".to_owned(),
        ));
    }

    let pass = account.pass.expose_secret();
    let session = timeout(greeting_duration, client.login(account.user.as_str(), pass))
        .await
        .map_err(|_| AppError::Timeout("IMAP login timeout".to_owned()))
        .and_then(|r| {
            r.map_err(|(e, _)| {
                let msg = e.to_string();
                if msg.to_ascii_lowercase().contains("auth") || msg.contains("LOGIN") {
                    AppError::AuthFailed(msg)
                } else {
                    AppError::Internal(msg)
                }
            })
        })?;

    Ok(session)
}

/// Verify that an authenticated session is still usable.
pub async fn noop_session(server: &ServerConfig, session: &mut ImapSession) -> AppResult<()> {
    timeout(socket_timeout(server), session.noop())
        .await
        .map_err(|_| AppError::Timeout("NOOP timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("NOOP failed: {e}"))))?;
    Ok(())
}

/// Best-effort logout for a session that is no longer needed.
pub async fn logout_session_best_effort(
    server: &ServerConfig,
    mut session: ImapSession,
) -> AppResult<()> {
    timeout(socket_timeout(server), session.logout())
        .await
        .map_err(|_| AppError::Timeout("LOGOUT timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("logout failed: {e}"))))?;
    Ok(())
}

/// Query server capabilities
///
/// Returns the IMAP capabilities supported by the server. Used to detect
/// support for features like `MOVE`.
pub async fn capabilities(
    server: &ServerConfig,
    session: &mut ImapSession,
) -> AppResult<async_imap::types::Capabilities> {
    timeout(socket_timeout(server), session.capabilities())
        .await
        .map_err(|_| AppError::Timeout("CAPABILITY timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("CAPABILITY failed: {e}"))))
}

/// List visible mailboxes with a bounded observation count.
///
/// The 2,001st response is a truncation sentinel; callers retain at most the
/// first 2,000 snapshot entries and must not claim an exact total when it is
/// present.
pub async fn list_all_mailboxes(
    server: &ServerConfig,
    session: &mut ImapSession,
) -> AppResult<MailboxListObservation> {
    let stream = timeout(socket_timeout(server), session.list(None, Some("*")))
        .await
        .map_err(|_| AppError::Timeout("LIST timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("LIST failed: {e}"))))?;

    let observations = timeout(
        socket_timeout(server),
        stream
            .take(MAILBOX_LIST_OBSERVATION_LIMIT)
            .try_collect::<Vec<_>>(),
    )
    .await
    .map_err(|_| AppError::Timeout("LIST stream timed out".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("LIST stream failed: {e}"))))?;
    let (items, truncated) = bounded_mailbox_observation(observations);
    Ok(MailboxListObservation { items, truncated })
}

/// Look up metadata for one mailbox without relying on approximate name matching.
///
/// The command name is encoded with the mailbox codec and the returned name is
/// normalized before exact comparison, so modified UTF-7 and display names are
/// treated as the same mailbox.
pub async fn mailbox_metadata(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
    gmail_capable: bool,
) -> AppResult<Option<MailboxInfo>> {
    let encoded_mailbox = encode_mailbox_name_for_command(mailbox);
    let quoted_mailbox_pattern = format!("\"{}\"", encoded_mailbox.replace('"', "\\\""));
    let stream = timeout(
        socket_timeout(server),
        session.list(None, Some(quoted_mailbox_pattern.as_str())),
    )
    .await
    .map_err(|_| AppError::Timeout(format!("LIST timed out for mailbox '{mailbox}'")))
    .and_then(|r| {
        r.map_err(|e| AppError::Internal(format!("LIST failed for mailbox '{mailbox}': {e}")))
    })?;
    let names = timeout(socket_timeout(server), stream.try_collect::<Vec<_>>())
        .await
        .map_err(|_| AppError::Timeout(format!("LIST stream timed out for mailbox '{mailbox}'")))
        .and_then(|r| {
            r.map_err(|e| {
                AppError::Internal(format!("LIST stream failed for mailbox '{mailbox}': {e}"))
            })
        })?;
    let expected_name = normalize_mailbox_name(mailbox);

    Ok(names
        .iter()
        .find(|name| {
            normalize_mailbox_name(name.name()).eq_ignore_ascii_case(expected_name.as_str())
        })
        .map(|name| provider::mailbox_info(name, gmail_capable)))
}

async fn reject_provider_managed_mailbox(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
    operation: &str,
) -> AppResult<()> {
    let gmail_capable = provider::is_gmail_capable(&capabilities(server, session).await?);
    if let Some(metadata) = mailbox_metadata(server, session, mailbox, gmail_capable).await?
        && metadata.provider_managed
    {
        return Err(AppError::InvalidInput(format!(
            "cannot {operation} provider-managed mailbox '{mailbox}'"
        )));
    }
    Ok(())
}

/// Create a mailbox if it does not already exist.
pub async fn create_mailbox_if_missing(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
) -> AppResult<()> {
    let encoded_mailbox = encode_mailbox_name_for_command(mailbox);
    let result = timeout(socket_timeout(server), session.create(&encoded_mailbox))
        .await
        .map_err(|_| AppError::Timeout(format!("CREATE timed out for mailbox '{mailbox}'")))?;
    match result {
        Ok(_) => Ok(()),
        Err(error) => {
            let lowered = error.to_string().to_ascii_lowercase();
            if lowered.contains("exists") || lowered.contains("already") {
                Ok(())
            } else {
                Err(AppError::Internal(format!(
                    "CREATE failed for mailbox '{mailbox}': {error}"
                )))
            }
        }
    }
}

/// Rename or move a mailbox.
pub async fn rename_mailbox(
    server: &ServerConfig,
    session: &mut ImapSession,
    from_mailbox: &str,
    to_mailbox: &str,
) -> AppResult<()> {
    reject_provider_managed_mailbox(server, session, from_mailbox, "rename").await?;
    create_parent_mailboxes(server, session, to_mailbox).await?;

    let from_encoded = encode_mailbox_name_for_command(from_mailbox);
    let to_encoded = encode_mailbox_name_for_command(to_mailbox);
    timeout(
        socket_timeout(server),
        session.rename(&from_encoded, &to_encoded),
    )
    .await
    .map_err(|_| AppError::Timeout(format!("RENAME timed out for mailbox '{from_mailbox}'")))
    .and_then(|r| {
        r.map_err(|e| {
            AppError::Internal(format!(
                "RENAME failed for mailbox '{from_mailbox}' to '{to_mailbox}': {e}"
            ))
        })
    })
}

/// Delete a mailbox.
pub async fn delete_mailbox(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
) -> AppResult<()> {
    reject_provider_managed_mailbox(server, session, mailbox, "delete").await?;

    let encoded_mailbox = encode_mailbox_name_for_command(mailbox);
    timeout(socket_timeout(server), session.delete(&encoded_mailbox))
        .await
        .map_err(|_| AppError::Timeout(format!("DELETE timed out for mailbox '{mailbox}'")))
        .and_then(|r| {
            r.map_err(|e| AppError::Internal(format!("DELETE failed for mailbox '{mailbox}': {e}")))
        })
}

/// Create missing parent mailboxes for a hierarchical mailbox path.
pub async fn create_parent_mailboxes(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
) -> AppResult<()> {
    for parent in mailbox_parent_paths(server, session, mailbox).await? {
        create_mailbox_if_missing(server, session, &parent).await?;
    }
    Ok(())
}

/// Create a mailbox path, creating any missing parent mailboxes first.
pub async fn create_mailbox_path(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
) -> AppResult<()> {
    create_parent_mailboxes(server, session, mailbox).await?;
    create_mailbox_if_missing(server, session, mailbox).await
}

async fn mailbox_parent_paths(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
) -> AppResult<Vec<String>> {
    let Some(delimiter) = hierarchy_delimiter(server, session).await? else {
        return Ok(Vec::new());
    };
    Ok(build_mailbox_parent_paths(delimiter, mailbox))
}

/// Discover the server's mailbox hierarchy delimiter, if any.
pub async fn hierarchy_delimiter(
    server: &ServerConfig,
    session: &mut ImapSession,
) -> AppResult<Option<char>> {
    let stream = timeout(socket_timeout(server), session.list(None, Some("\"\"")))
        .await
        .map_err(|_| AppError::Timeout("LIST delimiter probe timed out".to_owned()))
        .and_then(|r| {
            r.map_err(|e| AppError::Internal(format!("LIST delimiter probe failed: {e}")))
        })?;
    let names = timeout(socket_timeout(server), stream.try_collect::<Vec<_>>())
        .await
        .map_err(|_| AppError::Timeout("LIST delimiter probe stream timed out".to_owned()))
        .and_then(|r| {
            r.map_err(|e| AppError::Internal(format!("LIST delimiter probe stream failed: {e}")))
        })?;

    Ok(names
        .into_iter()
        .find_map(|name| name.delimiter().and_then(|value| value.chars().next())))
}

fn build_mailbox_parent_paths(delimiter: char, mailbox: &str) -> Vec<String> {
    if !mailbox.contains(delimiter) {
        return Vec::new();
    }

    let parts: Vec<&str> = mailbox.split(delimiter).collect();
    let mut parents = Vec::new();
    for idx in 1..parts.len() {
        parents.push(parts[..idx].join(&delimiter.to_string()));
    }
    parents
}

async fn reject_nonselect_mailbox(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
) -> AppResult<()> {
    if let Some(metadata) = mailbox_metadata(server, session, mailbox, false).await?
        && !metadata.selectable
    {
        return Err(AppError::InvalidInput(format!(
            "mailbox '{mailbox}' is not selectable"
        )));
    }
    Ok(())
}

/// Select mailbox in read-only mode
///
/// Uses `EXAMINE` command to fetch mailbox state without marking messages
/// as read. Returns the `UIDVALIDITY` for message ID stability.
pub async fn select_mailbox_readonly(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
) -> AppResult<u32> {
    reject_nonselect_mailbox(server, session, mailbox).await?;

    let encoded_mailbox = encode_mailbox_name_for_command(mailbox);
    let selected = timeout(socket_timeout(server), session.examine(&encoded_mailbox))
        .await
        .map_err(|_| AppError::Timeout(format!("EXAMINE timed out for mailbox '{mailbox}'")))
        .and_then(|r| {
            r.map_err(|e| AppError::NotFound(format!("cannot examine mailbox '{mailbox}': {e}")))
        })?;
    selected
        .uid_validity
        .ok_or_else(|| AppError::Internal("mailbox missing UIDVALIDITY".to_owned()))
}

/// Select mailbox in read-write mode
///
/// Uses `SELECT` command to enable write operations. Returns the `UIDVALIDITY`
/// for message ID stability.
pub async fn select_mailbox_readwrite(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
) -> AppResult<u32> {
    reject_nonselect_mailbox(server, session, mailbox).await?;

    let encoded_mailbox = encode_mailbox_name_for_command(mailbox);
    let selected = timeout(socket_timeout(server), session.select(&encoded_mailbox))
        .await
        .map_err(|_| AppError::Timeout(format!("SELECT timed out for mailbox '{mailbox}'")))
        .and_then(|r| {
            r.map_err(|e| AppError::NotFound(format!("cannot select mailbox '{mailbox}': {e}")))
        })?;
    selected
        .uid_validity
        .ok_or_else(|| AppError::Internal("mailbox missing UIDVALIDITY".to_owned()))
}

/// Fetch a single message with custom query
///
/// Runs a `UID FETCH` for a specific UID and returns the first result.
/// Used internally by other fetch functions.
///
/// # Errors
///
/// - `NotFound` if UID does not exist in mailbox
/// - `Timeout` or `Internal` for network/protocol errors
pub async fn fetch_one(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
    query: &str,
) -> AppResult<Fetch> {
    let stream = timeout(
        socket_timeout(server),
        session.uid_fetch(uid.to_string(), query),
    )
    .await
    .map_err(|_| AppError::Timeout("UID FETCH timed out".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("uid fetch failed: {e}"))))?;
    let fetches: Vec<Fetch> = timeout(socket_timeout(server), stream.try_collect())
        .await
        .map_err(|_| AppError::Timeout("UID FETCH stream timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("uid fetch stream failed: {e}"))))?;

    if let Some(fetch) = fetches.into_iter().next() {
        return Ok(fetch);
    }

    let existence_query = format!("UID {uid}");
    match uid_search(server, session, existence_query.as_str()).await {
        Ok(matches) if matches.contains(&uid) => Err(AppError::Internal(format!(
            "UID FETCH returned no data for existing uid {uid}; possible server FETCH incompatibility (query: {query})"
        ))),
        Ok(_) => Err(AppError::NotFound(format!("message uid {uid} not found"))),
        Err(error) => Err(AppError::Internal(format!(
            "UID FETCH returned no data for uid {uid}; failed to verify existence: {error}"
        ))),
    }
}

/// Search for messages matching query
///
/// Runs `UID SEARCH` and returns matching UIDs in descending order (newest
/// first). Callers typically limit the result set via pagination.
pub async fn uid_search(
    server: &ServerConfig,
    session: &mut ImapSession,
    query: &str,
) -> AppResult<Vec<u32>> {
    let set = timeout(socket_timeout(server), session.uid_search(query))
        .await
        .map_err(|_| AppError::Timeout("UID SEARCH timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("uid search failed: {e}"))))?;
    let mut uids: Vec<u32> = set.into_iter().collect();
    uids.sort_unstable_by(|a, b| b.cmp(a));
    Ok(uids)
}

/// Fetch the total RFC822 size for a message UID.
pub async fn fetch_message_size(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
) -> AppResult<usize> {
    let fetch = fetch_one(server, session, uid, "(UID RFC822.SIZE)").await?;
    let size = fetch
        .size
        .ok_or_else(|| AppError::Internal("message size not available".to_owned()))?;
    Ok(size as usize)
}

/// Fetch a bounded raw RFC822 range without setting `\\Seen`.
pub async fn fetch_raw_message_range(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
    offset: usize,
    max_bytes: usize,
) -> AppResult<Vec<u8>> {
    let requested = raw_body_range_length(max_bytes)?;
    let query = raw_body_range_query(offset, requested);
    let fetch = fetch_one(server, session, uid, &query).await?;
    let body = fetch
        .body()
        .ok_or_else(|| AppError::Internal("message has no raw message body".to_owned()))?;
    validate_raw_body_range_length(body.len(), requested)?;
    Ok(body.to_vec())
}

fn raw_body_range_length(max_bytes: usize) -> AppResult<usize> {
    if max_bytes == 0 {
        return Err(AppError::InvalidInput(
            "body range length must be at least 1".to_owned(),
        ));
    }
    Ok(max_bytes.min(u32::MAX as usize))
}

fn raw_body_range_query(offset: usize, requested: usize) -> String {
    format!("BODY.PEEK[]<{offset}.{requested}>")
}

fn validate_raw_body_range_length(actual: usize, requested: usize) -> AppResult<()> {
    if actual > requested {
        return Err(AppError::Internal(
            "IMAP server returned more than the requested body range".to_owned(),
        ));
    }
    Ok(())
}

/// Builds an IMAP UID set from UIDs in any order.
///
/// UIDs are sorted and deduplicated, and consecutive UIDs are compressed into
/// `first:last` ranges joined by commas. Empty input returns an empty string.
pub fn build_uid_set(uids: impl IntoIterator<Item = u32>) -> String {
    // Sorted, unique UIDs.
    let mut sorted: Vec<u32> = uids.into_iter().collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut iter = sorted.into_iter();
    let Some(first) = iter.next() else {
        return String::new();
    };
    // Formatted ranges completed so far.
    let mut ranges: Vec<String> = Vec::new();
    // First and last UID of the range being built.
    let (mut start, mut end) = (first, first);
    for uid in iter {
        if end.checked_add(1) == Some(uid) {
            end = uid;
            continue;
        }
        ranges.push(format_uid_range(start, end));
        start = uid;
        end = uid;
    }
    ranges.push(format_uid_range(start, end));
    ranges.join(",")
}

/// Formats one inclusive UID range as `n` or `start:end`.
fn format_uid_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}:{end}")
    }
}

/// Stores flags on a single message (test helper).
#[cfg(test)]
pub async fn uid_store(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
    query: &str,
) -> AppResult<()> {
    uid_store_sequence(server, session, &[uid], query)
        .await
        .map_err(AppError::from)
}

/// Permanently deletes a single message already marked `\Deleted` (test helper).
#[cfg(test)]
pub async fn uid_expunge(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
) -> AppResult<()> {
    uid_expunge_sequence(server, session, &[uid]).await
}

/// Append raw RFC822 message to mailbox
///
/// Used for cross-account copy operations. Does not return the new UID
/// directly (would require `UIDPLUS` capability).
#[cfg_attr(not(test), allow(dead_code))]
pub async fn append(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
    content: &[u8],
) -> AppResult<()> {
    let encoded_mailbox = encode_mailbox_name_for_command(mailbox);
    timeout(
        socket_timeout(server),
        session.append(&encoded_mailbox, None, None, content),
    )
    .await
    .map_err(|_| AppError::Timeout("APPEND timed out".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("APPEND failed: {e}"))))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use async_imap::Client;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme};
    use secrecy::{ExposeSecret, SecretString};
    use tokio::net::TcpStream;
    use tokio::time::sleep;
    use tokio::time::timeout;
    use tokio_rustls::TlsConnector;

    use crate::mailbox_codec::encode_mailbox_name_for_command;

    use super::{
        CopyMoveOutcome, MetadataProjection, append, bounded_mailbox_observation,
        build_mailbox_parent_paths, build_uid_set, fetch_message_metadata, list_all_mailboxes,
        mailbox_status, raw_body_range_length, raw_body_range_query, rename_mailbox,
        select_mailbox_readonly, select_mailbox_readwrite, socket_timeout, uid_copy_sequence,
        uid_expunge, uid_move_sequence, uid_search, uid_store, validate_raw_body_range_length,
    };
    use crate::config::{AccountConfig, ServerConfig};

    /// Holds connection details for a GreenMail test server instance.
    #[derive(Debug, Clone)]
    struct GreenmailEndpoints {
        host: String,
        smtp_port: u16,
        imap_port: u16,
        user: String,
        pass: String,
    }

    /// Parses a test port from the environment, falling back to a default.
    fn parse_test_port(key: &str, default: u16) -> u16 {
        std::env::var(key)
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(default)
    }

    /// Returns GreenmailEndpoints using environment variables or defaults.
    fn greenmail_endpoints() -> GreenmailEndpoints {
        GreenmailEndpoints {
            host: std::env::var("GREENMAIL_HOST").unwrap_or_else(|_| "localhost".to_owned()),
            smtp_port: parse_test_port("GREENMAIL_SMTP_PORT", 3025),
            imap_port: parse_test_port("GREENMAIL_IMAP_PORT", 3143),
            user: std::env::var("GREENMAIL_USER").unwrap_or_else(|_| "test@localhost".to_owned()),
            pass: std::env::var("GREENMAIL_PASS").unwrap_or_else(|_| "test".to_owned()),
        }
    }

    #[test]
    fn mailbox_parent_paths_builds_hierarchy_for_slash_paths() {
        assert_eq!(
            build_mailbox_parent_paths('/', "Archive/Projects/2026"),
            vec!["Archive".to_owned(), "Archive/Projects".to_owned()]
        );
    }

    #[test]
    fn mailbox_parent_paths_uses_dot_when_no_slash_exists() {
        assert_eq!(
            build_mailbox_parent_paths('.', "Archive.Projects.2026"),
            vec!["Archive".to_owned(), "Archive.Projects".to_owned()]
        );
    }

    #[test]
    fn raw_body_range_query_is_always_bounded() {
        assert_eq!(raw_body_range_query(0, 1), "BODY.PEEK[]<0.1>");
        assert_eq!(raw_body_range_query(4, 12), "BODY.PEEK[]<4.12>");
        assert_eq!(
            raw_body_range_query(usize::MAX, u32::MAX as usize),
            format!("BODY.PEEK[]<{}.{}>", usize::MAX, u32::MAX)
        );
    }

    #[test]
    fn raw_body_range_rejects_zero_length() {
        assert!(matches!(
            raw_body_range_length(0),
            Err(crate::errors::AppError::InvalidInput(message))
                if message == "body range length must be at least 1"
        ));
        assert_eq!(
            raw_body_range_length(usize::MAX).expect("positive range should be valid"),
            u32::MAX as usize
        );
    }

    #[test]
    fn raw_body_range_rejects_overlong_literal() {
        assert!(validate_raw_body_range_length(12, 12).is_ok());
        assert!(matches!(
            validate_raw_body_range_length(13, 12),
            Err(crate::errors::AppError::Internal(message))
                if message == "IMAP server returned more than the requested body range"
        ));
    }

    #[test]
    fn uid_set_sorts_deduplicates_and_compresses_ranges() {
        assert_eq!(build_uid_set(Vec::<u32>::new()), "");
        assert_eq!(build_uid_set([7]), "7");
        assert_eq!(build_uid_set([9, 4, 5, 4, 1, 2, 3]), "1:5,9");
        assert_eq!(build_uid_set([10, 12, 14]), "10,12,14");
        assert_eq!(build_uid_set([2, 1]), "1:2");
        assert_eq!(
            build_uid_set([u32::MAX - 1, u32::MAX]),
            "4294967294:4294967295"
        );
    }

    /// Asserts that a destination mapping, when the server reported one, names the real message.
    ///
    /// A server that reports no mapping passes; a reported mapping must be valid and match.
    fn assert_native_mapping(
        outcome: &CopyMoveOutcome,
        source_uid: u32,
        destination_uidvalidity: u32,
        destination_uids: &[u32],
    ) {
        assert!(
            outcome.mapping_issue.is_none(),
            "server mapping must be valid when present: {:?}",
            outcome.mapping_issue
        );
        let Some(mapping) = &outcome.mapping else {
            return;
        };
        assert_eq!(mapping.uidvalidity, destination_uidvalidity);
        let destination_uid = mapping
            .source_to_destination
            .get(&source_uid)
            .expect("mapping must name the source UID");
        assert!(
            destination_uids.contains(destination_uid),
            "mapped destination UID {destination_uid} must exist in {destination_uids:?}"
        );
    }

    /// Constructs a ServerConfig for GreenMail integration tests.
    fn greenmail_test_config(endpoints: &GreenmailEndpoints) -> ServerConfig {
        let account = AccountConfig {
            account_id: "default".to_owned(),
            host: endpoints.host.clone(),
            port: endpoints.imap_port,
            secure: true,
            user: endpoints.user.clone(),
            pass: SecretString::new(endpoints.pass.clone().into()),
        };

        let mut accounts = BTreeMap::new();
        accounts.insert(account.account_id.clone(), account);

        ServerConfig {
            accounts,
            trusted_ca_certs: Vec::new(),
            write_enabled: true,
            connect_timeout_ms: 5_000,
            greeting_timeout_ms: 5_000,
            socket_timeout_ms: 15_000,
            cursor_ttl_seconds: 600,
            cursor_max_entries: 128,
            read_session_cache_ttl_seconds: 120,
            read_session_cache_max_per_account: 4,
            operation_max_entries: 256,
            message_fetch_budget_bytes: 8_388_608,
            message_decode_budget_bytes: 16_777_216,
            mime_max_depth: 32,
            mime_max_parts: 250,
            attachment_extract_budget_bytes: 10_485_760,
        }
    }

    /// Disables certificate verification for test TLS connections.
    #[derive(Debug)]
    struct NoCertificateVerification;

    impl ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, RustlsError> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, RustlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, RustlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ED25519,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
            ]
        }
    }

    /// Connects and authenticates to the GreenMail IMAP server for integration tests.
    async fn connect_authenticated_greenmail(
        config: &ServerConfig,
    ) -> Result<super::ImapSession, String> {
        let account = config
            .get_account("default")
            .map_err(|e| format!("missing default account: {e}"))?;
        let connect_duration = Duration::from_millis(config.connect_timeout_ms);
        let greeting_duration = Duration::from_millis(config.greeting_timeout_ms);

        let tcp = timeout(
            connect_duration,
            TcpStream::connect((account.host.as_str(), account.port)),
        )
        .await
        .map_err(|_| "tcp connect timeout".to_owned())
        .and_then(|r| r.map_err(|e| format!("tcp connect failed: {e}")))?;

        let tls_config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(tls_config));

        let server_name = ServerName::try_from(account.host.clone())
            .map_err(|_| "invalid IMAP host for TLS SNI".to_owned())?;
        let tls_stream = timeout(greeting_duration, connector.connect(server_name, tcp))
            .await
            .map_err(|_| "TLS handshake timeout".to_owned())
            .and_then(|r| r.map_err(|e| format!("TLS handshake failed: {e}")))?;

        let mut client = Client::new(tls_stream);
        let greeting = timeout(greeting_duration, client.read_response())
            .await
            .map_err(|_| "IMAP greeting timeout".to_owned())
            .and_then(|r| r.map_err(|e| format!("IMAP greeting failed: {e}")))?;
        if greeting.is_none() {
            return Err("IMAP server closed connection before greeting".to_owned());
        }

        let login = timeout(
            greeting_duration,
            client.login(account.user.as_str(), account.pass.expose_secret()),
        )
        .await
        .map_err(|_| "IMAP login timeout".to_owned())?;

        login.map_err(|(e, _)| format!("IMAP login failed: {e}"))
    }

    /// Attempts to connect to the GreenMail IMAP port to verify server availability.
    async fn probe_greenmail_imap(endpoints: &GreenmailEndpoints) -> Result<(), String> {
        timeout(
            Duration::from_secs(1),
            TcpStream::connect((endpoints.host.as_str(), endpoints.imap_port)),
        )
        .await
        .map_err(|_| "tcp connect timeout".to_owned())
        .and_then(|r| r.map_err(|e| format!("tcp connect failed: {e}")))?;

        Ok(())
    }

    /// Waits until the GreenMail IMAP server is reachable and login succeeds.
    ///
    /// Retries for up to 60 seconds, returning an error if the server does not become ready.
    async fn wait_until_login_works(
        config: &ServerConfig,
        endpoints: &GreenmailEndpoints,
    ) -> Result<(), String> {
        let mut last_probe_err = String::new();
        let mut last_login_err = String::new();

        for _ in 0..60 {
            match probe_greenmail_imap(endpoints).await {
                Ok(()) => match connect_authenticated_greenmail(config).await {
                    Ok(_) => return Ok(()),
                    Err(e) => last_login_err = e,
                },
                Err(e) => last_probe_err = e,
            }

            sleep(Duration::from_secs(1)).await;
        }

        Err(format!(
            "greenmail unreachable at {}:{} (SMTP={}) after 60s; probe_error='{}'; login_error='{}'",
            endpoints.host,
            endpoints.imap_port,
            endpoints.smtp_port,
            if last_probe_err.is_empty() {
                "none"
            } else {
                last_probe_err.as_str()
            },
            if last_login_err.is_empty() {
                "none"
            } else {
                last_login_err.as_str()
            }
        ))
    }

    /// Creates a mailbox if it does not already exist.
    ///
    /// Ignores errors if the mailbox already exists.
    async fn create_mailbox_if_missing(
        config: &ServerConfig,
        session: &mut super::ImapSession,
        mailbox: &str,
    ) -> Result<(), String> {
        let encoded_mailbox = encode_mailbox_name_for_command(mailbox);
        let result = timeout(socket_timeout(config), session.create(&encoded_mailbox))
            .await
            .map_err(|_| format!("CREATE timed out for mailbox '{mailbox}'"))?;
        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                if msg.contains("exists") || msg.contains("already") {
                    Ok(())
                } else {
                    Err(format!("CREATE failed for mailbox '{mailbox}': {e}"))
                }
            }
        }
    }
    #[test]
    fn mailbox_observation_retains_limit_and_marks_sentinel() {
        let observations: Vec<usize> = (0..2_001).collect();

        let (items, truncated) = bounded_mailbox_observation(observations);

        assert_eq!(items.len(), 2_000);
        assert_eq!(items.first(), Some(&0));
        assert_eq!(items.last(), Some(&1_999));
        assert!(truncated);
    }

    /// Connects to a GreenMail IMAP server and verifies basic read operations.
    ///
    /// This test checks that the server is reachable, login works, and
    /// that expected mailboxes and seeded messages are present and accessible.
    /// It also verifies fetching a known message by subject.
    #[tokio::test]
    #[ignore = "requires running GreenMail IMAP server"]
    async fn greenmail_imap_smoke_test() {
        let endpoints = greenmail_endpoints();
        let config = greenmail_test_config(&endpoints);
        wait_until_login_works(&config, &endpoints)
            .await
            .expect("greenmail did not become ready");

        let mut session = connect_authenticated_greenmail(&config)
            .await
            .expect("imap login should work");

        let mailboxes = list_all_mailboxes(&config, &mut session)
            .await
            .expect("LIST should succeed");
        let mailbox_names: Vec<String> = mailboxes
            .items
            .into_iter()
            .map(|mailbox| mailbox.name().to_owned())
            .collect();

        for required_mailbox in ["INBOX", "Archive", "Spam", "Newsletters"] {
            assert!(
                mailbox_names
                    .iter()
                    .any(|name| name.contains(required_mailbox)),
                "expected seeded mailbox {required_mailbox:?}, got {mailbox_names:?}"
            );
        }

        assert!(
            mailbox_names.len() >= 5,
            "expected at least five seeded mailboxes, got {mailbox_names:?}"
        );

        let uidvalidity = select_mailbox_readonly(&config, &mut session, "INBOX")
            .await
            .expect("INBOX should be selectable");
        assert!(uidvalidity > 0);

        let inbox_uids = uid_search(&config, &mut session, "ALL")
            .await
            .expect("UID SEARCH ALL should succeed");
        assert!(
            inbox_uids.len() >= 4,
            "expected seeded INBOX corpus with at least 4 messages, got {}",
            inbox_uids.len()
        );

        // STATUS must report the live total and unread counts of the seeded INBOX.
        let counts = mailbox_status(&config, &mut session, "INBOX")
            .await
            .expect("STATUS should succeed for INBOX");
        assert_eq!(counts.total_messages as usize, inbox_uids.len());
        assert!(counts.unread_messages >= 1 && counts.unread_messages <= counts.total_messages);

        // The Summary projection returns headers and flags keyed by UID. A bounded body prefix
        // is not requested because GreenMail answers `BODY[]<0>{N}` without the RFC 3501 space
        // before the literal; the strict-peer unit tests cover the prefix projection.
        let summaries = fetch_message_metadata(
            &config,
            &mut session,
            &inbox_uids,
            MetadataProjection::Summary {
                preview_bytes: None,
            },
            false,
        )
        .await
        .expect("Summary projection should succeed");
        assert_eq!(summaries.len(), inbox_uids.len());
        assert!(summaries.values().all(|metadata| {
            metadata.header_bytes.is_some()
                && metadata.flags.is_some()
                && metadata.gmail_labels.is_none()
        }));

        let unseen_uids = uid_search(&config, &mut session, "UNSEEN")
            .await
            .expect("UID SEARCH UNSEEN should succeed");
        assert!(
            !unseen_uids.is_empty(),
            "expected at least one unread seeded INBOX message"
        );

        let roadmap_uids = uid_search(&config, &mut session, "SUBJECT \"Roadmap Review\"")
            .await
            .expect("UID SEARCH SUBJECT should find seeded roadmap message");
        assert_eq!(
            roadmap_uids.len(),
            1,
            "expected exactly one seeded Roadmap Review message"
        );

        let archive_mailbox = mailbox_names
            .iter()
            .find(|name| name.contains("Archive") && name.contains("2025"))
            .cloned()
            .expect("expected archive mailbox in seeded dataset");

        select_mailbox_readonly(&config, &mut session, &archive_mailbox)
            .await
            .expect("archive mailbox should be selectable");
        let archive_uids = uid_search(&config, &mut session, "ALL")
            .await
            .expect("UID SEARCH in archive should succeed");
        assert!(
            archive_uids.len() >= 2,
            "expected at least two seeded archive messages, got {}",
            archive_uids.len()
        );
    }

    /// Exercises IMAP write-path operations against GreenMail.
    ///
    /// This test covers appending a message, flag updates, mailbox creation,
    /// copying, moving, deleting, and expunging messages. It ensures that
    /// all write operations succeed and that mailbox/message state is correct
    /// after each operation.
    #[tokio::test]
    #[ignore = "requires running GreenMail IMAP server"]
    async fn greenmail_imap_write_paths_test() {
        let endpoints = greenmail_endpoints();
        let config = greenmail_test_config(&endpoints);
        wait_until_login_works(&config, &endpoints)
            .await
            .expect("greenmail did not become ready");

        let mut session = connect_authenticated_greenmail(&config)
            .await
            .expect("imap login should work");

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be monotonic enough for test")
            .as_nanos();
        let subject = format!("Greenmail write path {nonce}");
        let destination_mailbox = format!("INBOX.greenmail_{nonce}");
        let rename_source = format!("INBOX.greenmail_rename_source_{nonce}");
        let rename_parent = format!("INBOX.greenmail_rename_parent_{nonce}");
        let rename_destination = format!("{rename_parent}.child");
        let before_rename = list_all_mailboxes(&config, &mut session)
            .await
            .expect("LIST before nested rename should succeed");
        assert!(
            before_rename
                .items
                .iter()
                .all(|mailbox| mailbox.name() != rename_parent
                    && mailbox.name() != rename_destination),
            "nested rename destination and parent should initially be absent"
        );
        create_mailbox_if_missing(&config, &mut session, &rename_source)
            .await
            .expect("CREATE rename source should succeed");
        rename_mailbox(&config, &mut session, &rename_source, &rename_destination)
            .await
            .expect("nested RENAME should create missing parents and succeed");
        select_mailbox_readonly(&config, &mut session, &rename_destination)
            .await
            .expect("nested rename destination should exist and be selectable");

        let message = format!(
            "From: sender@example.com\r\nTo: user@example.com\r\nSubject: {subject}\r\n\r\nWrite-path body\r\n"
        );
        append(&config, &mut session, "INBOX", message.as_bytes())
            .await
            .expect("APPEND should succeed");

        select_mailbox_readwrite(&config, &mut session, "INBOX")
            .await
            .expect("INBOX should be selectable read-write");
        let source_uids = uid_search(&config, &mut session, &format!("SUBJECT \"{subject}\""))
            .await
            .expect("UID SEARCH by SUBJECT should succeed");
        assert_eq!(source_uids.len(), 1, "expected exactly one source message");
        let source_uid = source_uids[0];

        uid_store(&config, &mut session, source_uid, "+FLAGS.SILENT (\\Seen)")
            .await
            .expect("UID STORE should set \\Seen flag");
        let mut state = fetch_message_metadata(
            &config,
            &mut session,
            &[source_uid],
            MetadataProjection::State,
            false,
        )
        .await
        .expect("State metadata fetch should succeed after UID STORE");
        let flags = state
            .remove(&source_uid)
            .and_then(|metadata| metadata.flags)
            .expect("State metadata should include flags");
        assert!(
            flags.iter().any(|f| f == "\\Seen"),
            "expected \\Seen in flags after UID STORE"
        );

        create_mailbox_if_missing(&config, &mut session, &destination_mailbox)
            .await
            .expect("CREATE destination mailbox should succeed");

        let copy_outcome =
            uid_copy_sequence(&config, &mut session, &[source_uid], &destination_mailbox).await;
        assert!(copy_outcome.completion.is_ok(), "UID COPY should succeed");

        let copy_uidvalidity = select_mailbox_readonly(&config, &mut session, &destination_mailbox)
            .await
            .expect("destination mailbox should be selectable");
        let copied_uids = uid_search(&config, &mut session, &format!("SUBJECT \"{subject}\""))
            .await
            .expect("UID SEARCH in destination should succeed");
        assert_eq!(
            copied_uids.len(),
            1,
            "expected one copied message in destination mailbox"
        );
        assert_native_mapping(&copy_outcome, source_uid, copy_uidvalidity, &copied_uids);

        select_mailbox_readwrite(&config, &mut session, "INBOX")
            .await
            .expect("INBOX should be selectable read-write for move");
        let move_outcome =
            uid_move_sequence(&config, &mut session, &[source_uid], &destination_mailbox).await;
        assert!(move_outcome.completion.is_ok(), "UID MOVE should succeed");

        select_mailbox_readonly(&config, &mut session, "INBOX")
            .await
            .expect("INBOX should be selectable readonly after move");
        let remaining_in_source =
            uid_search(&config, &mut session, &format!("SUBJECT \"{subject}\""))
                .await
                .expect("UID SEARCH in source after move should succeed");
        assert!(
            remaining_in_source.is_empty(),
            "expected no source messages after UID MOVE"
        );

        let move_uidvalidity =
            select_mailbox_readwrite(&config, &mut session, &destination_mailbox)
                .await
                .expect("destination mailbox should be selectable read-write");
        let moved_and_copied = uid_search(&config, &mut session, &format!("SUBJECT \"{subject}\""))
            .await
            .expect("UID SEARCH in destination after move should succeed");
        assert_eq!(
            moved_and_copied.len(),
            2,
            "expected two messages in destination after copy+move"
        );
        assert_native_mapping(
            &move_outcome,
            source_uid,
            move_uidvalidity,
            &moved_and_copied,
        );

        let uid_to_delete = moved_and_copied[0];
        uid_store(
            &config,
            &mut session,
            uid_to_delete,
            "+FLAGS.SILENT (\\Deleted)",
        )
        .await
        .expect("UID STORE should set \\Deleted flag");
        uid_expunge(&config, &mut session, uid_to_delete)
            .await
            .expect("UID EXPUNGE should remove deleted message");

        let after_delete = uid_search(&config, &mut session, &format!("SUBJECT \"{subject}\""))
            .await
            .expect("UID SEARCH after delete should succeed");
        assert_eq!(
            after_delete.len(),
            1,
            "expected one message remaining after delete"
        );
    }
}
