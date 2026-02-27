//! IMAP transport and session operations
//!
//! Provides timeout-bounded wrappers around `async-imap` operations. All network
//! calls are enforced to use TLS, and timeouts are derived from server config.

use std::sync::Arc;
use std::time::Duration;

use async_imap::types::Fetch;
use async_imap::{Client, Session};
use futures::TryStreamExt;
use rustls::ClientConfig;
use rustls::RootCertStore;
use rustls_pki_types::ServerName;
use secrecy::ExposeSecret;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use crate::config::{AccountConfig, ServerConfig};
use crate::errors::{AppError, AppResult};

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

/// Send NOOP to test connection liveness
///
/// Typically used after connection to verify the server is responsive.
pub async fn noop(server: &ServerConfig, session: &mut ImapSession) -> AppResult<()> {
    timeout(socket_timeout(server), session.noop())
        .await
        .map_err(|_| AppError::Timeout("NOOP timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("NOOP failed: {e}"))))
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

/// List all visible mailboxes/folders
///
/// Returns up to the server's full mailbox list. Caller should truncate if
/// necessary (e.g., to 200 items).
pub async fn list_all_mailboxes(
    server: &ServerConfig,
    session: &mut ImapSession,
) -> AppResult<Vec<async_imap::types::Name>> {
    let stream = timeout(socket_timeout(server), session.list(None, Some("*")))
        .await
        .map_err(|_| AppError::Timeout("LIST timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("LIST failed: {e}"))))?;

    timeout(socket_timeout(server), stream.try_collect::<Vec<_>>())
        .await
        .map_err(|_| AppError::Timeout("LIST stream timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("LIST stream failed: {e}"))))
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
    let selected = timeout(socket_timeout(server), session.examine(mailbox))
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
    let selected = timeout(socket_timeout(server), session.select(mailbox))
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

    fetches
        .into_iter()
        .next()
        .ok_or_else(|| AppError::NotFound(format!("message uid {uid} not found")))
}

/// Fetch full RFC822 message source
///
/// Returns raw bytes of the entire message.
pub async fn fetch_raw_message(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
) -> AppResult<Vec<u8>> {
    let fetch = fetch_one(server, session, uid, "UID RFC822").await?;
    let body = fetch
        .body()
        .ok_or_else(|| AppError::Internal("message has no RFC822 body".to_owned()))?;
    Ok(body.to_vec())
}

/// Fetch curated headers and flags
///
/// Returns standard headers (Date, From, To, CC, Subject) and message flags.
/// Uses `BODY.PEEK` to avoid marking the message as read.
pub async fn fetch_headers_and_flags(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
) -> AppResult<(Vec<u8>, Vec<String>)> {
    let fetch = fetch_one(
        server,
        session,
        uid,
        "UID FLAGS BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC SUBJECT)]",
    )
    .await?;
    let header_bytes = fetch
        .header()
        .or_else(|| fetch.body())
        .ok_or_else(|| AppError::Internal("message headers not available".to_owned()))?
        .to_vec();
    Ok((header_bytes, flags_to_strings(&fetch)))
}

/// Fetch message flags only
///
/// Returns IMAP flags (e.g., `\Seen`, `\Flagged`, `\Draft`) as strings.
pub async fn fetch_flags(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
) -> AppResult<Vec<String>> {
    let fetch = fetch_one(server, session, uid, "UID FLAGS").await?;
    Ok(flags_to_strings(&fetch))
}

/// Convert fetch flags to string representation
///
/// Helper to serialize flag types to `Debug` string format.
pub fn flags_to_strings(fetch: &Fetch) -> Vec<String> {
    fetch.flags().map(|flag| format!("{flag:?}")).collect()
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

/// Store flags on a message
///
/// Runs `UID STORE` with a flag query string. Use `+FLAGS.SILENT` to add
/// flags or `-FLAGS.SILENT` to remove flags.
pub async fn uid_store(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
    query: &str,
) -> AppResult<()> {
    let stream = timeout(
        socket_timeout(server),
        session.uid_store(uid.to_string(), query),
    )
    .await
    .map_err(|_| AppError::Timeout("UID STORE timed out".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("uid store failed: {e}"))))?;
    let _: Vec<Fetch> = timeout(socket_timeout(server), stream.try_collect())
        .await
        .map_err(|_| AppError::Timeout("UID STORE stream timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("uid store stream failed: {e}"))))?;
    Ok(())
}

/// Copy message to another mailbox
///
/// Runs `UID COPY` to duplicate the message. Returns the new UID on success
/// (currently not captured due to protocol limitations).
pub async fn uid_copy(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
    mailbox: &str,
) -> AppResult<()> {
    timeout(
        socket_timeout(server),
        session.uid_copy(uid.to_string(), mailbox),
    )
    .await
    .map_err(|_| AppError::Timeout("UID COPY timed out".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("UID COPY failed: {e}"))))
}

/// Move message to another mailbox
///
/// Runs `UID MOVE` if server supports it (RFC 6851). More efficient than
/// copy+delete as it's atomic.
pub async fn uid_move(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
    mailbox: &str,
) -> AppResult<()> {
    timeout(
        socket_timeout(server),
        session.uid_mv(uid.to_string(), mailbox),
    )
    .await
    .map_err(|_| AppError::Timeout("UID MOVE timed out".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("UID MOVE failed: {e}"))))
}

/// Permanently delete a message
///
/// Runs `UID EXPUNGE` to immediately remove the message marked as `\Deleted`.
pub async fn uid_expunge(
    server: &ServerConfig,
    session: &mut ImapSession,
    uid: u32,
) -> AppResult<()> {
    let stream = timeout(socket_timeout(server), session.uid_expunge(uid.to_string()))
        .await
        .map_err(|_| AppError::Timeout("UID EXPUNGE timed out".to_owned()))
        .and_then(|r| r.map_err(|e| AppError::Internal(format!("UID EXPUNGE failed: {e}"))))?;
    let _: Vec<u32> = timeout(socket_timeout(server), stream.try_collect())
        .await
        .map_err(|_| AppError::Timeout("UID EXPUNGE stream timed out".to_owned()))
        .and_then(|r| {
            r.map_err(|e| AppError::Internal(format!("UID EXPUNGE stream failed: {e}")))
        })?;
    Ok(())
}

/// Append raw RFC822 message to mailbox
///
/// Used for cross-account copy operations. Does not return the new UID
/// directly (would require `UIDPLUS` capability).
pub async fn append(
    server: &ServerConfig,
    session: &mut ImapSession,
    mailbox: &str,
    content: &[u8],
) -> AppResult<()> {
    timeout(
        socket_timeout(server),
        session.append(mailbox, None, None, content),
    )
    .await
    .map_err(|_| AppError::Timeout("APPEND timed out".to_owned()))
    .and_then(|r| r.map_err(|e| AppError::Internal(format!("APPEND failed: {e}"))))
}
