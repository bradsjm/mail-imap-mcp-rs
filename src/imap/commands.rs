//! Parsed IMAP command adapter.
//!
//! The convenience APIs of `async-imap` discard response attributes and
//! completion codes that this server needs: `COPYUID`, Gmail extension
//! attributes, STATUS values, and the tagged completion of write commands.
//! This module runs commands through `Session::run_command` and inspects the
//! typed responses from `async_imap::imap_proto` under one timeout per command.

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;

use async_imap::Session;
use async_imap::imap_proto::{
    AttributeValue, MailboxDatum, MessageSection, Response, ResponseCode, SectionPath, Status,
    StatusAttribute, UidSetMember,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::timeout;

use super::{build_uid_set, raw_body_range_length, socket_timeout, validate_raw_body_range_length};
use crate::config::ServerConfig;
use crate::errors::{AppError, AppResult};
use crate::mailbox_codec::{
    decode_mailbox_name_for_display, encode_mailbox_name_for_command, normalize_mailbox_name,
};
use crate::models::SetOperation;

/// Header fields requested by the summary projection.
const SUMMARY_HEADER_FIELDS: &str = "DATE FROM TO CC SUBJECT MESSAGE-ID REFERENCES IN-REPLY-TO";

/// Maximum number of characters of server text retained in a rejection message.
const MAX_SERVER_TEXT_CHARS: usize = 200;

/// Outcome classification for a command that was sent to the server.
///
/// The distinction decides whether a write may be reported as failed or must
/// be reported as an unknown outcome that callers must not retry.
#[derive(Debug)]
pub enum CommandFailure {
    /// The server definitively refused the command with a matching tagged NO or BAD.
    Rejected(AppError),
    /// The command was sent but its outcome is unknown or the response was invalid.
    Uncertain(AppError),
}

impl From<CommandFailure> for AppError {
    /// Discards the certainty classification and keeps the error.
    fn from(failure: CommandFailure) -> Self {
        match failure {
            CommandFailure::Rejected(error) | CommandFailure::Uncertain(error) => error,
        }
    }
}

/// Decoded per-message data from a FETCH or label STORE response.
///
/// A `None` field means the server did not return that attribute. It does not
/// mean the attribute is empty: an observed empty label list is `Some(vec![])`.
#[derive(Debug, Default)]
pub struct MessageMetadata {
    /// Raw header section bytes, when the server returned them.
    pub header_bytes: Option<Vec<u8>>,
    /// Message flags exactly as returned by the server.
    pub flags: Option<Vec<String>>,
    /// Total RFC822 size in bytes.
    pub size_bytes: Option<usize>,
    /// Gmail labels decoded from modified UTF-7.
    pub gmail_labels: Option<Vec<String>>,
    /// Gmail message identifier (`X-GM-MSGID`).
    pub gmail_message_id: Option<u64>,
    /// Gmail conversation identifier (`X-GM-THRID`).
    pub gmail_thread_id: Option<u64>,
    /// Raw message prefix from the bounded preview body request.
    pub raw_prefix: Option<Vec<u8>>,
    /// Problem with the returned preview prefix; it never invalidates other fields.
    pub preview_issue: Option<AppError>,
}

/// Attribute set requested by [`fetch_message_metadata`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataProjection {
    /// Headers and flags, plus a bounded raw prefix and size when previews are requested.
    Summary {
        /// Maximum number of leading message bytes to request, if any.
        preview_bytes: Option<usize>,
    },
    /// Flags only.
    State,
}

/// Destination identities reported by a `COPYUID` response code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyUidMapping {
    /// UIDVALIDITY of the destination mailbox.
    pub uidvalidity: u32,
    /// Destination UID for each source UID.
    pub source_to_destination: HashMap<u32, u32>,
}

/// Result of a `UID COPY` or `UID MOVE` command.
///
/// Completion and identity discovery are independent: a missing or malformed
/// mapping never changes the outcome of a completed command.
#[derive(Debug)]
pub struct CopyMoveOutcome {
    /// Validated destination identities, when the server reported them.
    pub mapping: Option<CopyUidMapping>,
    /// Tagged completion of the command.
    pub completion: Result<(), CommandFailure>,
    /// Description of a malformed or conflicting `COPYUID` mapping.
    pub mapping_issue: Option<AppError>,
}

/// Result of a Gmail label `UID STORE` command.
#[derive(Debug)]
pub struct LabelStoreOutcome {
    /// Tagged completion of the command.
    pub completion: Result<(), CommandFailure>,
    /// Metadata from the STORE responses; empty unless the command completed with OK.
    pub metadata: HashMap<u32, MessageMetadata>,
}

/// Message counts of one mailbox from `STATUS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxCounts {
    /// Total number of messages.
    pub total_messages: u32,
    /// Number of messages without `\Seen`.
    pub unread_messages: u32,
}

/// Returns the command verb (`UID FETCH`, `STATUS`) without any arguments.
fn command_verb(command: &str) -> &str {
    let mut words = command.split(' ');
    let first = words.next().unwrap_or_default();
    if first.eq_ignore_ascii_case("UID")
        && let Some(second) = words.next()
    {
        return &command[..first.len() + 1 + second.len()];
    }
    first
}

/// Strips control characters and bounds untrusted server text for error messages.
fn bounded_server_text(text: Option<&str>) -> String {
    text.unwrap_or_default()
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_SERVER_TEXT_CHARS)
        .collect()
}

/// Sends one command and inspects every response until its tagged completion.
///
/// The callback also receives the raw bytes of each response, which lets
/// callers check wire details that the response parser normalizes. The whole
/// exchange, including all response reads, is bounded by one socket timeout.
/// After this function returns an error the session must not be used again.
async fn run_raw_command<T, F>(
    server: &ServerConfig,
    session: &mut Session<T>,
    command: &str,
    mut on_response: F,
) -> Result<(), CommandFailure>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
    F: for<'a> FnMut(&Response<'a>, &[u8]) -> AppResult<()> + Send,
{
    // Verb used in every error message so that arguments and data never leak.
    let verb = command_verb(command).to_owned();
    if command
        .bytes()
        .any(|byte| byte == b'\r' || byte == b'\n' || byte == 0)
    {
        return Err(CommandFailure::Rejected(AppError::InvalidInput(format!(
            "{verb} command contains line breaks or NUL"
        ))));
    }

    let exchange = async {
        // Tag that identifies the completion of this command.
        let tag = session.run_command(command).await.map_err(|error| {
            CommandFailure::Uncertain(AppError::Internal(format!(
                "{verb} could not be sent: {error}"
            )))
        })?;
        // First callback validation error; later responses are drained without callbacks.
        let mut callback_error: Option<AppError> = None;
        loop {
            let response = match session.read_response().await {
                Ok(Some(response)) => response,
                Ok(None) => {
                    return Err(CommandFailure::Uncertain(AppError::Internal(format!(
                        "{verb}: connection closed before completion"
                    ))));
                }
                Err(error) => {
                    return Err(CommandFailure::Uncertain(AppError::Internal(format!(
                        "{verb}: response read failed: {error}"
                    ))));
                }
            };
            let parsed = response.parsed();
            if let Response::Done { tag: done_tag, .. } = parsed
                && *done_tag != tag
            {
                return Err(CommandFailure::Uncertain(AppError::Internal(format!(
                    "{verb}: unrelated tagged completion"
                ))));
            }
            if callback_error.is_none()
                && let Err(error) = on_response(parsed, response.borrow_owner())
            {
                callback_error = Some(error);
            }
            match parsed {
                Response::Done {
                    status,
                    information,
                    ..
                } => {
                    return match status {
                        Status::Ok => match callback_error {
                            None => Ok(()),
                            Some(error) => Err(CommandFailure::Uncertain(error)),
                        },
                        Status::No | Status::Bad => {
                            let name = if matches!(status, Status::No) {
                                "NO"
                            } else {
                                "BAD"
                            };
                            Err(CommandFailure::Rejected(AppError::Internal(format!(
                                "{verb} rejected by server ({name}): {}",
                                bounded_server_text(information.as_deref())
                            ))))
                        }
                        Status::PreAuth | Status::Bye => Err(CommandFailure::Uncertain(
                            AppError::Internal(format!("{verb}: unexpected completion status")),
                        )),
                    };
                }
                Response::Data {
                    status: Status::Bye,
                    ..
                } => {
                    return Err(CommandFailure::Uncertain(AppError::Internal(format!(
                        "{verb}: server closed the connection (BYE)"
                    ))));
                }
                Response::Continue { .. } => {
                    return Err(CommandFailure::Uncertain(AppError::Internal(format!(
                        "{verb}: unexpected continuation request"
                    ))));
                }
                _ => {}
            }
        }
    };

    match timeout(socket_timeout(server), exchange).await {
        Ok(result) => result,
        Err(_) => Err(CommandFailure::Uncertain(AppError::Timeout(format!(
            "{verb} timed out"
        )))),
    }
}

/// Sends one command and passes every parsed response to the callback.
///
/// Success requires the matching tagged OK. A tagged NO or BAD is
/// [`CommandFailure::Rejected`]; every other failure after sending is
/// [`CommandFailure::Uncertain`]. A callback validation error is retained, the
/// remaining responses are drained, and the error is then returned as
/// uncertain. The command is never retried.
async fn run_parsed_command<T, F>(
    server: &ServerConfig,
    session: &mut Session<T>,
    command: &str,
    mut on_response: F,
) -> Result<(), CommandFailure>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
    F: for<'a> FnMut(&Response<'a>) -> AppResult<()> + Send,
{
    run_raw_command(server, session, command, |response, _raw| {
        on_response(response)
    })
    .await
}

/// Quotes a normalized, modified UTF-7 encoded mailbox name for a command.
fn quote_mailbox(mailbox: &str) -> String {
    let encoded = encode_mailbox_name_for_command(&normalize_mailbox_name(mailbox));
    quote_string(&encoded)
}

/// Wraps text in a quoted string and escapes backslashes and double quotes.
fn quote_string(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        if character == '\\' || character == '"' {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

/// Encodes one Gmail label for `X-GM-LABELS`.
///
/// System labels such as `\Inbox` are atoms. Every other label is encoded as
/// modified UTF-7 in a quoted string. The strict encoder is used because
/// labels are decoded domain text, so a literal `&` must always be escaped.
pub(crate) fn encode_gmail_label(label: &str) -> String {
    let is_system_label = label.strip_prefix('\\').is_some_and(|name| {
        !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_alphabetic())
    });
    if is_system_label {
        return label.to_owned();
    }
    quote_string(&utf7_imap::encode_utf7_imap(label.to_owned()))
}

/// Builds a `UID <verb> <set> <rest>` command from a non-empty UID slice.
fn uid_command(verb: &str, uids: &[u32], rest: &str) -> Result<String, CommandFailure> {
    if uids.is_empty() {
        return Err(CommandFailure::Rejected(AppError::InvalidInput(format!(
            "UID {verb} requires at least one UID"
        ))));
    }
    let suffix = if rest.is_empty() {
        String::new()
    } else {
        format!(" {rest}")
    };
    Ok(format!(
        "UID {verb} {}{suffix}",
        build_uid_set(uids.iter().copied())
    ))
}

/// Merges the attributes of one FETCH response into the per-UID metadata map.
///
/// Results are keyed by the returned `UID` attribute, never by sequence
/// number. Responses without a UID or with a UID outside `requested` are ignored.
fn collect_fetch_metadata(
    response: &Response<'_>,
    requested: &HashSet<u32>,
    preview_limit: Option<usize>,
    results: &mut HashMap<u32, MessageMetadata>,
) {
    let Response::Fetch(_, attributes) = response else {
        return;
    };
    let Some(uid) = attributes.iter().find_map(|attribute| match attribute {
        AttributeValue::Uid(uid) => Some(*uid),
        _ => None,
    }) else {
        return;
    };
    if !requested.contains(&uid) {
        return;
    }
    // Entry that accumulates attributes for this UID across responses.
    let entry = results.entry(uid).or_default();
    for attribute in attributes {
        match attribute {
            AttributeValue::Flags(flags) => {
                entry.flags = Some(flags.iter().map(|flag| flag.to_string()).collect());
            }
            AttributeValue::Rfc822Size(size) => entry.size_bytes = Some(*size as usize),
            AttributeValue::GmailLabels(labels) => {
                entry.gmail_labels = Some(
                    labels
                        .iter()
                        .map(|label| decode_mailbox_name_for_display(&unescape_quoted_label(label)))
                        .collect(),
                );
            }
            AttributeValue::GmailMsgId(id) => entry.gmail_message_id = Some(*id),
            AttributeValue::GmailThrId(id) => entry.gmail_thread_id = Some(*id),
            AttributeValue::BodySection {
                section: Some(SectionPath::Full(MessageSection::Header)),
                data: Some(data),
                ..
            } => entry.header_bytes = Some(data.to_vec()),
            AttributeValue::BodySection {
                section: None,
                index,
                data: Some(data),
            } => {
                if let Some(limit) = preview_limit {
                    match (
                        index.unwrap_or(0),
                        validate_raw_body_range_length(data.len(), limit),
                    ) {
                        (0, Ok(())) => entry.raw_prefix = Some(data.to_vec()),
                        (0, Err(error)) => entry.preview_issue = Some(error),
                        _ => {
                            entry.preview_issue = Some(AppError::Internal(
                                "IMAP server returned a body range with a non-zero origin"
                                    .to_owned(),
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Removes the quoted-string escapes (`\\` and `\"`) that the response parser leaves in a label.
///
/// The parser cannot tell a quoted string from an atom, so only the two escape pairs of
/// RFC 3501 are removed. A system label atom such as `\Inbox` contains neither pair and
/// stays unchanged.
fn unescape_quoted_label(label: &str) -> String {
    // Label text without escape backslashes.
    let mut unescaped = String::with_capacity(label.len());
    // Characters of the label still to inspect.
    let mut characters = label.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\'
            && let Some(escaped) = characters.next_if(|next| matches!(next, '\\' | '"'))
        {
            unescaped.push(escaped);
        } else {
            unescaped.push(character);
        }
    }
    unescaped
}

/// Builds the FETCH data item list and the preview byte limit for a projection.
fn metadata_fetch_query(
    projection: MetadataProjection,
    gmail: bool,
) -> AppResult<(String, Option<usize>)> {
    // Data items inside the parentheses, in request order.
    let mut items = String::from("UID FLAGS");
    // Validated preview byte limit, when previews are requested.
    let mut preview_limit = None;
    if let MetadataProjection::Summary { preview_bytes } = projection {
        items.push_str(&format!(
            " BODY.PEEK[HEADER.FIELDS ({SUMMARY_HEADER_FIELDS})]"
        ));
        if let Some(bytes) = preview_bytes {
            let requested = raw_body_range_length(bytes)?;
            items.push_str(&format!(" RFC822.SIZE BODY.PEEK[]<0.{requested}>"));
            preview_limit = Some(requested);
        }
    }
    if gmail {
        items.push_str(" X-GM-LABELS X-GM-MSGID X-GM-THRID");
    }
    Ok((format!("({items})"), preview_limit))
}

/// Fetches one metadata projection for a set of UIDs in a single command.
///
/// An empty UID list returns an empty map without sending a command.
/// Attributes the server omitted stay `None`. Any command failure returns an
/// error and the session must be discarded.
pub async fn fetch_message_metadata<T>(
    server: &ServerConfig,
    session: &mut Session<T>,
    uids: &[u32],
    projection: MetadataProjection,
    gmail: bool,
) -> AppResult<HashMap<u32, MessageMetadata>>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    if uids.is_empty() {
        return Ok(HashMap::new());
    }
    let (query, preview_limit) = metadata_fetch_query(projection, gmail)?;
    let command = format!("UID FETCH {} {query}", build_uid_set(uids.iter().copied()));
    // UIDs that the caller asked for; other FETCH responses are ignored.
    let requested: HashSet<u32> = uids.iter().copied().collect();
    // Metadata accumulated by the response callback.
    let mut results = HashMap::new();
    run_parsed_command(server, session, &command, |response| {
        collect_fetch_metadata(response, &requested, preview_limit, &mut results);
        Ok(())
    })
    .await?;
    Ok(results)
}

/// Expands UID set members in their stated order, including descending ranges.
///
/// Fails when the expansion would exceed `max_len` entries.
fn expand_uid_members(members: &[UidSetMember], max_len: usize) -> AppResult<Vec<u32>> {
    // Expanded UIDs in stated order.
    let mut expanded = Vec::new();
    for member in members {
        let (start, end) = match member {
            UidSetMember::Uid(uid) => (*uid, *uid),
            UidSetMember::UidRange(range) => (*range.start(), *range.end()),
        };
        let span = u64::from(start.abs_diff(end)) + 1;
        if expanded.len() as u64 + span > max_len as u64 {
            return Err(AppError::Internal(
                "COPYUID names more UIDs than were requested".to_owned(),
            ));
        }
        if start <= end {
            expanded.extend(start..=end);
        } else {
            expanded.extend((end..=start).rev());
        }
    }
    Ok(expanded)
}

/// Validates one `COPYUID` code against the requested source UIDs.
fn build_copy_mapping(
    uidvalidity: u32,
    sources: &[UidSetMember],
    destinations: &[UidSetMember],
    requested: &HashSet<u32>,
) -> AppResult<CopyUidMapping> {
    let malformed = |reason: &str| AppError::Internal(format!("malformed COPYUID: {reason}"));
    if uidvalidity == 0 {
        return Err(malformed("zero UIDVALIDITY"));
    }
    let source_uids = expand_uid_members(sources, requested.len())?;
    let destination_uids = expand_uid_members(destinations, requested.len())?;
    if source_uids.len() != destination_uids.len() {
        return Err(malformed("source and destination counts differ"));
    }
    // Mapping built from the validated pairs.
    let mut source_to_destination = HashMap::with_capacity(source_uids.len());
    // Destination UIDs already assigned.
    let mut seen_destinations = HashSet::with_capacity(destination_uids.len());
    for (source, destination) in source_uids.into_iter().zip(destination_uids) {
        if source == 0 || destination == 0 {
            return Err(malformed("zero UID"));
        }
        if !requested.contains(&source) {
            return Err(malformed("source UID was not requested"));
        }
        if !seen_destinations.insert(destination) {
            return Err(malformed("duplicate destination UID"));
        }
        if source_to_destination.insert(source, destination).is_some() {
            return Err(malformed("duplicate source UID"));
        }
    }
    Ok(CopyUidMapping {
        uidvalidity,
        source_to_destination,
    })
}

/// Reports whether the raw `COPYUID` response text contains a descending range.
///
/// The response parser normalizes `5:4` to `4:5`, which would silently pair
/// source and destination UIDs in the wrong order, so the raw text is checked.
fn copyuid_has_descending_range(raw: &[u8]) -> bool {
    let text = String::from_utf8_lossy(raw);
    let Some(start) = text.to_ascii_uppercase().find("COPYUID ") else {
        return false;
    };
    let rest = &text[start..];
    let code = rest.split_once(']').map_or(rest, |(code, _)| code);
    code.split_whitespace().skip(2).take(2).any(|set| {
        set.split(',').any(|member| {
            member.split_once(':').is_some_and(|(first, second)| {
                matches!(
                    (first.parse::<u32>(), second.parse::<u32>()),
                    (Ok(first), Ok(second)) if first > second
                )
            })
        })
    })
}

/// Collects and validates `COPYUID` codes across the responses of one command.
struct CopyUidCollector {
    /// Source UIDs that the command requested.
    requested: HashSet<u32>,
    /// Validated mapping so far.
    mapping: Option<CopyUidMapping>,
    /// First mapping problem; once set, no mapping is retained.
    issue: Option<AppError>,
}

impl CopyUidCollector {
    /// Inspects one response and records any `COPYUID` code it carries.
    fn observe(&mut self, response: &Response<'_>, raw: &[u8]) {
        let code = match response {
            Response::Data {
                status: Status::Ok,
                code: Some(code),
                ..
            }
            | Response::Done {
                status: Status::Ok,
                code: Some(code),
                ..
            } => code,
            _ => return,
        };
        let ResponseCode::CopyUid(uidvalidity, sources, destinations) = code else {
            return;
        };
        if self.issue.is_some() {
            return;
        }
        if copyuid_has_descending_range(raw) {
            self.fail(AppError::Internal(
                "COPYUID uses a descending range whose order the parser discards".to_owned(),
            ));
            return;
        }
        match build_copy_mapping(*uidvalidity, sources, destinations, &self.requested) {
            Ok(candidate) => match &self.mapping {
                Some(existing) if *existing != candidate => {
                    self.fail(AppError::Internal(
                        "conflicting COPYUID responses".to_owned(),
                    ));
                }
                Some(_) => {}
                None => self.mapping = Some(candidate),
            },
            Err(error) => self.fail(error),
        }
    }

    /// Records the first mapping problem and clears any retained mapping.
    fn fail(&mut self, error: AppError) {
        self.mapping = None;
        self.issue = Some(error);
    }
}

/// Runs `UID COPY` or `UID MOVE` and captures the destination identities.
async fn run_copy_or_move<T>(
    server: &ServerConfig,
    session: &mut Session<T>,
    verb: &str,
    uids: &[u32],
    mailbox: &str,
) -> CopyMoveOutcome
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    let command = match uid_command(verb, uids, &quote_mailbox(mailbox)) {
        Ok(command) => command,
        Err(failure) => {
            return CopyMoveOutcome {
                mapping: None,
                completion: Err(failure),
                mapping_issue: None,
            };
        }
    };
    // Collector that validates every COPYUID code seen during the command.
    let mut collector = CopyUidCollector {
        requested: uids.iter().copied().collect(),
        mapping: None,
        issue: None,
    };
    let completion = run_raw_command(server, session, &command, |response, raw| {
        collector.observe(response, raw);
        Ok(())
    })
    .await;
    CopyMoveOutcome {
        mapping: collector.mapping,
        completion,
        mapping_issue: collector.issue,
    }
}

/// Copies UIDs to another mailbox and returns the reported destination UIDs.
pub async fn uid_copy_sequence<T>(
    server: &ServerConfig,
    session: &mut Session<T>,
    uids: &[u32],
    mailbox: &str,
) -> CopyMoveOutcome
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    run_copy_or_move(server, session, "COPY", uids, mailbox).await
}

/// Moves UIDs to another mailbox and returns the reported destination UIDs.
pub async fn uid_move_sequence<T>(
    server: &ServerConfig,
    session: &mut Session<T>,
    uids: &[u32],
    mailbox: &str,
) -> CopyMoveOutcome
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    run_copy_or_move(server, session, "MOVE", uids, mailbox).await
}

/// Permanently removes UIDs that are already marked `\Deleted`.
///
/// A tagged NO or BAD is an error; the checked completion is read under one timeout.
pub async fn uid_expunge_sequence<T>(
    server: &ServerConfig,
    session: &mut Session<T>,
    uids: &[u32],
) -> AppResult<()>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    let command = uid_command("EXPUNGE", uids, "").map_err(AppError::from)?;
    run_parsed_command(server, session, &command, |_| Ok(())).await?;
    Ok(())
}

/// Stores flags or other properties on UIDs and requires a tagged OK.
pub async fn uid_store_sequence<T>(
    server: &ServerConfig,
    session: &mut Session<T>,
    uids: &[u32],
    query: &str,
) -> Result<(), CommandFailure>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    let command = uid_command("STORE", uids, query)?;
    run_parsed_command(server, session, &command, |_| Ok(())).await
}

/// Changes Gmail labels with a non-silent `UID STORE` and decodes the returned labels.
///
/// No FETCH follows the STORE: removing the selected folder's label can
/// invalidate the original UID. Returned metadata is accepted only after the
/// tagged OK.
pub async fn store_message_labels<T>(
    server: &ServerConfig,
    session: &mut Session<T>,
    uids: &[u32],
    operation: SetOperation,
    labels: &[String],
) -> LabelStoreOutcome
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    // Space-separated wire form of every label.
    let list = labels
        .iter()
        .map(|label| encode_gmail_label(label))
        .collect::<Vec<_>>()
        .join(" ");
    let query = match operation {
        SetOperation::Add => format!("+X-GM-LABELS ({list})"),
        SetOperation::Remove => format!("-X-GM-LABELS ({list})"),
        SetOperation::Replace => format!("X-GM-LABELS ({list})"),
    };
    let command = match uid_command("STORE", uids, &query) {
        Ok(command) => command,
        Err(failure) => {
            return LabelStoreOutcome {
                completion: Err(failure),
                metadata: HashMap::new(),
            };
        }
    };
    // UIDs that the caller asked for; other FETCH responses are ignored.
    let requested: HashSet<u32> = uids.iter().copied().collect();
    // Metadata accumulated by the response callback.
    let mut metadata = HashMap::new();
    let completion = run_parsed_command(server, session, &command, |response| {
        collect_fetch_metadata(response, &requested, None, &mut metadata);
        Ok(())
    })
    .await;
    if completion.is_err() {
        metadata.clear();
    }
    LabelStoreOutcome {
        completion,
        metadata,
    }
}

/// Reads the message and unseen counts of one mailbox with `STATUS`.
///
/// The response must name the requested mailbox and carry both values.
/// Observed zero counts are valid; anything missing is an error.
pub async fn mailbox_status<T>(
    server: &ServerConfig,
    session: &mut Session<T>,
    mailbox: &str,
) -> AppResult<MailboxCounts>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    let command = format!("STATUS {} (MESSAGES UNSEEN)", quote_mailbox(mailbox));
    // Identity that responses must match.
    let expected = normalize_mailbox_name(mailbox);
    // Whether a STATUS response named a different mailbox.
    let mut saw_other_mailbox = false;
    // Total message count from the matching response.
    let mut total_messages = None;
    // Unseen message count from the matching response.
    let mut unread_messages = None;
    run_parsed_command(server, session, &command, |response| {
        if let Response::MailboxData(MailboxDatum::Status { mailbox, status }) = response {
            let actual = normalize_mailbox_name(mailbox);
            let same = actual == expected
                || (actual.eq_ignore_ascii_case("INBOX") && expected.eq_ignore_ascii_case("INBOX"));
            if !same {
                saw_other_mailbox = true;
                return Ok(());
            }
            for attribute in status {
                match attribute {
                    StatusAttribute::Messages(count) => total_messages = Some(*count),
                    StatusAttribute::Unseen(count) => unread_messages = Some(*count),
                    _ => {}
                }
            }
        }
        Ok(())
    })
    .await?;
    match (total_messages, unread_messages) {
        (Some(total_messages), Some(unread_messages)) => Ok(MailboxCounts {
            total_messages,
            unread_messages,
        }),
        (None, None) if saw_other_mailbox => Err(AppError::Internal(
            "STATUS response named a different mailbox".to_owned(),
        )),
        (None, None) => Err(AppError::Internal(
            "STATUS returned no response for the mailbox".to_owned(),
        )),
        (None, Some(_)) => Err(AppError::Internal(
            "STATUS response is missing MESSAGES".to_owned(),
        )),
        (Some(_), None) => Err(AppError::Internal(
            "STATUS response is missing UNSEEN".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    /// Quoted-string escapes are removed from labels while system label atoms stay intact.
    #[test]
    fn label_escapes_are_removed_without_touching_system_atoms() {
        assert_eq!(unescape_quoted_label("q\\\"uote"), "q\"uote");
        assert_eq!(unescape_quoted_label("a\\\\b"), "a\\b");
        assert_eq!(unescape_quoted_label("\\Inbox"), "\\Inbox");
        assert_eq!(unescape_quoted_label("plain label"), "plain label");
        assert_eq!(unescape_quoted_label("trailing\\"), "trailing\\");
    }

    use std::collections::{BTreeMap, HashMap};
    use std::time::Duration;

    use async_imap::imap_proto::UidSetMember;
    use async_imap::{Client, Session};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
    use tokio::task::JoinHandle;

    use super::{
        CommandFailure, MailboxCounts, MetadataProjection, encode_gmail_label, expand_uid_members,
        fetch_message_metadata, mailbox_status, store_message_labels, uid_copy_sequence,
        uid_expunge_sequence, uid_move_sequence, uid_store_sequence, unescape_quoted_label,
    };
    use crate::config::ServerConfig;
    use crate::errors::AppError;
    use crate::models::SetOperation;

    /// One scripted server action for one received command.
    enum Step {
        /// Writes the text, substituting `{tag}` with the command tag.
        Reply(String),
        /// Writes the text, then keeps the connection open without replying.
        Partial(String),
        /// Writes the text, then closes the connection.
        ReplyThenClose(String),
        /// Closes the connection without replying.
        Close,
    }

    /// Builds a reply from lines; each line gets a CRLF terminator.
    fn reply(lines: &[&str]) -> Step {
        Step::Reply(lines.iter().map(|line| format!("{line}\r\n")).collect())
    }

    /// Formats an IMAP literal for the given text.
    fn lit(text: &str) -> String {
        format!("{{{}}}\r\n{text}", text.len())
    }

    /// Server configuration with a given socket timeout.
    fn test_server(socket_timeout_ms: u64) -> ServerConfig {
        ServerConfig {
            accounts: BTreeMap::new(),
            trusted_ca_certs: Vec::new(),
            write_enabled: true,
            connect_timeout_ms: 1_000,
            greeting_timeout_ms: 1_000,
            socket_timeout_ms,
            cursor_ttl_seconds: 600,
            cursor_max_entries: 8,
            read_session_cache_ttl_seconds: 120,
            read_session_cache_max_per_account: 4,
            operation_max_entries: 8,
            message_fetch_budget_bytes: 1_048_576,
            message_decode_budget_bytes: 1_048_576,
            mime_max_depth: 8,
            mime_max_parts: 32,
            attachment_extract_budget_bytes: 1_048_576,
        }
    }

    /// Starts a scripted peer and returns an authenticated session plus the commands it saw.
    ///
    /// The peer answers LOGIN, then handles one command per step. After the
    /// script it records any further commands until the client disconnects.
    async fn connect(script: Vec<Step>) -> (Session<DuplexStream>, JoinHandle<Vec<String>>) {
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let peer = tokio::spawn(async move {
            let (read_half, mut writer) = tokio::io::split(server_io);
            let mut reader = BufReader::new(read_half);
            writer.write_all(b"* OK ready\r\n").await.expect("greeting");
            let mut commands = Vec::new();
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("login line");
            let tag = line.split(' ').next().unwrap_or_default().to_owned();
            writer
                .write_all(format!("{tag} OK logged in\r\n").as_bytes())
                .await
                .expect("login reply");
            for step in script {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return commands;
                }
                commands.push(line.trim_end().to_owned());
                let tag = line.split(' ').next().unwrap_or_default().to_owned();
                match step {
                    Step::Reply(text) => writer
                        .write_all(text.replace("{tag}", &tag).as_bytes())
                        .await
                        .expect("scripted reply"),
                    Step::Partial(text) => {
                        writer
                            .write_all(text.replace("{tag}", &tag).as_bytes())
                            .await
                            .expect("partial reply");
                        std::future::pending::<()>().await;
                    }
                    Step::ReplyThenClose(text) => {
                        writer
                            .write_all(text.replace("{tag}", &tag).as_bytes())
                            .await
                            .expect("final reply");
                        return commands;
                    }
                    Step::Close => return commands,
                }
            }
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return commands,
                    Ok(_) => commands.push(line.trim_end().to_owned()),
                }
            }
        });
        let mut client = Client::new(client_io);
        client.read_response().await.expect("greeting read");
        let session = client
            .login("user", "pass")
            .await
            .map_err(|(error, _)| error)
            .expect("login");
        (session, peer)
    }

    /// Runs `UID STORE` against a script and returns its result and recorded commands.
    async fn store_with(script: Vec<Step>) -> (Result<(), CommandFailure>, Vec<String>) {
        let (mut session, peer) = connect(script).await;
        let result = uid_store_sequence(
            &test_server(1_000),
            &mut session,
            &[1],
            "+FLAGS.SILENT (\\Seen)",
        )
        .await;
        drop(session);
        (result, peer.await.expect("peer task"))
    }

    #[tokio::test]
    async fn tagged_no_and_bad_are_rejected() {
        for status in ["NO", "BAD"] {
            let completion = format!("{{tag}} {status} refused");
            let (result, commands) = store_with(vec![reply(&[completion.as_str()])]).await;
            assert!(
                matches!(result, Err(CommandFailure::Rejected(_))),
                "{status}"
            );
            assert_eq!(commands, vec!["A0002 UID STORE 1 +FLAGS.SILENT (\\Seen)"]);
        }
    }

    #[tokio::test]
    async fn rejection_message_names_verb_not_arguments() {
        let (result, _) = store_with(vec![reply(&["{tag} NO refused"])]).await;
        let Err(CommandFailure::Rejected(error)) = result else {
            panic!("expected rejection");
        };
        let message = error.to_string();
        assert!(message.contains("UID STORE"));
        assert!(!message.contains("FLAGS.SILENT"));
    }

    #[tokio::test]
    async fn matching_tagged_ok_succeeds() {
        let (result, _) = store_with(vec![reply(&["* 1 EXISTS", "{tag} OK done"])]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn incomplete_or_unrelated_streams_are_uncertain() {
        let scripts = vec![
            vec![Step::Close],
            vec![Step::ReplyThenClose("* 1 EXISTS\r\n".to_owned())],
            vec![reply(&["* BYE shutting down"])],
            vec![reply(&["A9999 OK done"])],
            vec![reply(&["+ go ahead"])],
        ];
        for (index, script) in scripts.into_iter().enumerate() {
            let server = test_server(1_000);
            let (mut session, peer) = connect(script).await;
            let result = uid_store_sequence(&server, &mut session, &[1], "FLAGS ()").await;
            assert!(
                matches!(result, Err(CommandFailure::Uncertain(_))),
                "script {index}"
            );
            drop(session);
            peer.await.expect("peer task");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn full_command_timeout_is_uncertain() {
        let server = test_server(1_000);
        let (mut session, peer) = connect(vec![Step::Partial("* 1 EXISTS\r\n".to_owned())]).await;
        let (result, ()) = tokio::join!(
            uid_store_sequence(&server, &mut session, &[1], "FLAGS ()"),
            async {
                tokio::task::yield_now().await;
                tokio::time::advance(Duration::from_millis(1_001)).await;
            }
        );
        assert!(matches!(
            result,
            Err(CommandFailure::Uncertain(AppError::Timeout(message))) if message == "UID STORE timed out"
        ));
        peer.abort();
    }

    #[tokio::test]
    async fn command_with_line_break_is_rejected_before_sending() {
        let (mut session, peer) = connect(Vec::new()).await;
        let result = uid_store_sequence(
            &test_server(1_000),
            &mut session,
            &[1],
            "FLAGS ()\r\nA1 LOGOUT",
        )
        .await;
        assert!(matches!(result, Err(CommandFailure::Rejected(_))));
        drop(session);
        assert!(peer.await.expect("peer task").is_empty());
    }

    /// Runs `UID COPY` for UIDs 4, 5, and 9 against the replies.
    async fn copy_with(lines: &[&str]) -> super::CopyMoveOutcome {
        let (mut session, peer) = connect(vec![reply(lines)]).await;
        let outcome =
            uid_copy_sequence(&test_server(1_000), &mut session, &[9, 4, 5], "Dest").await;
        drop(session);
        let commands = peer.await.expect("peer task");
        assert_eq!(commands, vec!["A0002 UID COPY 4:5,9 \"Dest\""]);
        outcome
    }

    #[tokio::test]
    async fn copyuid_maps_source_to_destination() {
        let outcome = copy_with(&["{tag} OK [COPYUID 777 4:5,9 41:42,99] Copy completed"]).await;
        assert!(outcome.completion.is_ok());
        assert!(outcome.mapping_issue.is_none());
        let mapping = outcome.mapping.expect("mapping");
        assert_eq!(mapping.uidvalidity, 777);
        assert_eq!(
            mapping.source_to_destination,
            HashMap::from([(4, 41), (5, 42), (9, 99)])
        );
    }

    #[tokio::test]
    async fn copyuid_supports_sparse_sets_and_untagged_codes() {
        let outcome = copy_with(&[
            "* OK [COPYUID 12 4,9 100,300] ",
            "* 3 EXPUNGE",
            "{tag} OK done",
        ])
        .await;
        assert!(outcome.completion.is_ok());
        let mapping = outcome.mapping.expect("mapping");
        assert_eq!(mapping.uidvalidity, 12);
        assert_eq!(
            mapping.source_to_destination,
            HashMap::from([(4, 100), (9, 300)])
        );
    }

    #[tokio::test]
    async fn copyuid_from_untagged_ok_survives_tagged_no() {
        let outcome = copy_with(&["* OK [COPYUID 777 4 41] ", "{tag} NO later phase failed"]).await;
        assert!(matches!(
            outcome.completion,
            Err(CommandFailure::Rejected(_))
        ));
        assert_eq!(
            outcome.mapping.expect("mapping").source_to_destination,
            HashMap::from([(4, 41)])
        );
    }

    #[tokio::test]
    async fn missing_copyuid_leaves_mapping_and_issue_empty() {
        let outcome = copy_with(&["{tag} OK done"]).await;
        assert!(outcome.completion.is_ok());
        assert!(outcome.mapping.is_none());
        assert!(outcome.mapping_issue.is_none());
    }

    #[tokio::test]
    async fn malformed_copyuid_never_changes_completion() {
        let cases = [
            "{tag} OK [COPYUID 777 4:5 41] done",
            "{tag} OK [COPYUID 777 4,4 41,42] done",
            "{tag} OK [COPYUID 777 4,5 41,41] done",
            "{tag} OK [COPYUID 777 4,6 41,42] done",
            "{tag} OK [COPYUID 777 4 0] done",
            "{tag} OK [COPYUID 0 4 41] done",
            "{tag} OK [COPYUID 777 1:300 1001:1300] done",
            "{tag} OK [COPYUID 777 5:4 51:52] done",
            "{tag} OK [COPYUID 777 4:5 52:51] done",
        ];
        for case in cases {
            let outcome = copy_with(&[case]).await;
            assert!(outcome.completion.is_ok(), "{case}");
            assert!(outcome.mapping.is_none(), "{case}");
            assert!(outcome.mapping_issue.is_some(), "{case}");
        }
    }

    #[tokio::test]
    async fn conflicting_second_copyuid_clears_mapping() {
        let outcome = copy_with(&[
            "* OK [COPYUID 777 4 41] ",
            "{tag} OK [COPYUID 777 4 42] done",
        ])
        .await;
        assert!(outcome.completion.is_ok());
        assert!(outcome.mapping.is_none());
        assert!(outcome.mapping_issue.is_some());
    }

    #[tokio::test]
    async fn repeated_identical_copyuid_is_accepted() {
        let outcome = copy_with(&[
            "* OK [COPYUID 777 4 41] ",
            "{tag} OK [COPYUID 777 4 41] done",
        ])
        .await;
        assert!(outcome.mapping_issue.is_none());
        assert!(outcome.mapping.is_some());
    }

    #[tokio::test]
    async fn move_uses_move_verb_and_captures_copyuid() {
        let (mut session, peer) = connect(vec![reply(&[
            "* OK [COPYUID 5 7 70] ",
            "* 1 EXPUNGE",
            "{tag} OK moved",
        ])])
        .await;
        let outcome =
            uid_move_sequence(&test_server(1_000), &mut session, &[7], "Archive/\"Q\"").await;
        drop(session);
        assert_eq!(
            peer.await.expect("peer task"),
            vec!["A0002 UID MOVE 7 \"Archive/\\\"Q\\\"\""]
        );
        assert!(outcome.completion.is_ok());
        assert_eq!(
            outcome.mapping.expect("mapping").source_to_destination[&7],
            70
        );
    }

    #[tokio::test]
    async fn copy_with_unicode_mailbox_is_encoded_and_quoted() {
        let (mut session, peer) = connect(vec![reply(&["{tag} OK done"])]).await;
        let outcome = uid_copy_sequence(&test_server(1_000), &mut session, &[1], "日本語").await;
        drop(session);
        assert!(outcome.completion.is_ok());
        assert_eq!(
            peer.await.expect("peer task"),
            vec!["A0002 UID COPY 1 \"&ZeVnLIqe-\""]
        );
    }

    #[tokio::test]
    async fn copy_without_uids_sends_nothing() {
        let (mut session, peer) = connect(Vec::new()).await;
        let outcome = uid_copy_sequence(&test_server(1_000), &mut session, &[], "Dest").await;
        assert!(matches!(
            outcome.completion,
            Err(CommandFailure::Rejected(_))
        ));
        drop(session);
        assert!(peer.await.expect("peer task").is_empty());
    }

    #[test]
    fn uid_members_expand_in_stated_order() {
        let members = [
            UidSetMember::UidRange(std::ops::RangeInclusive::new(5, 4)),
            UidSetMember::Uid(9),
            UidSetMember::UidRange(1..=2),
        ];
        assert_eq!(
            expand_uid_members(&members, 5).expect("expand"),
            vec![5, 4, 9, 1, 2]
        );
        assert!(expand_uid_members(&members, 4).is_err());
        assert!(expand_uid_members(&[UidSetMember::UidRange(1..=u32::MAX)], 250).is_err());
    }

    /// Header block used by metadata tests.
    const HEADERS: &str = "Subject: Hello\r\n\r\n";
    /// FETCH data item requested for the summary header section.
    const PEEK_ITEM: &str =
        "BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC SUBJECT MESSAGE-ID REFERENCES IN-REPLY-TO)]";
    /// FETCH data item name in server responses for the summary header section.
    const HEADER_ITEM: &str =
        "BODY[HEADER.FIELDS (DATE FROM TO CC SUBJECT MESSAGE-ID REFERENCES IN-REPLY-TO)]";

    /// Fetches summary metadata for the UIDs against the replies and returns the commands.
    async fn fetch_with(
        uids: &[u32],
        projection: MetadataProjection,
        gmail: bool,
        lines: &[&str],
    ) -> (
        crate::errors::AppResult<HashMap<u32, super::MessageMetadata>>,
        Vec<String>,
    ) {
        let script = if uids.is_empty() {
            Vec::new()
        } else {
            vec![reply(lines)]
        };
        let (mut session, peer) = connect(script).await;
        let result =
            fetch_message_metadata(&test_server(1_000), &mut session, uids, projection, gmail)
                .await;
        drop(session);
        (result, peer.await.expect("peer task"))
    }

    #[tokio::test]
    async fn metadata_ignores_unrelated_uids_and_sequence_numbers() {
        let header_line = format!(
            "* 7 FETCH (UID 2 FLAGS (\\Seen) {HEADER_ITEM} {})",
            lit(HEADERS)
        );
        let (result, commands) = fetch_with(
            &[2],
            MetadataProjection::Summary {
                preview_bytes: None,
            },
            false,
            &[
                "* 9 EXISTS",
                "* 2 FETCH (FLAGS (\\Flagged))",
                "* 3 FETCH (UID 99 FLAGS (\\Deleted))",
                &header_line,
                "{tag} OK done",
            ],
        )
        .await;
        assert_eq!(
            commands,
            vec![format!("A0002 UID FETCH 2 (UID FLAGS {PEEK_ITEM})")]
        );
        let metadata = result.expect("metadata");
        assert_eq!(metadata.len(), 1);
        let entry = &metadata[&2];
        assert_eq!(entry.flags.as_deref(), Some(&["\\Seen".to_owned()][..]));
        assert_eq!(entry.header_bytes.as_deref(), Some(HEADERS.as_bytes()));
        assert!(entry.raw_prefix.is_none());
        assert!(entry.gmail_labels.is_none());
    }

    #[tokio::test]
    async fn metadata_merges_split_responses_and_keeps_exact_gmail_ids() {
        let (result, commands) = fetch_with(
            &[3, 4, 5],
            MetadataProjection::State,
            true,
            &[
                "* 1 FETCH (UID 3 FLAGS (\\Seen))",
                "* 1 FETCH (UID 3 X-GM-LABELS (\\Inbox \"Muy Importante\" \"&AOk-t&AOk-\") X-GM-MSGID 1278455344230334865 X-GM-THRID 1278455344230334866)",
                "* 2 FETCH (UID 4 FLAGS () X-GM-LABELS ())",
                "* 3 FETCH (UID 5 FLAGS (\\Seen))",
                "{tag} OK done",
            ],
        )
        .await;
        assert_eq!(
            commands,
            vec!["A0002 UID FETCH 3:5 (UID FLAGS X-GM-LABELS X-GM-MSGID X-GM-THRID)"]
        );
        let metadata = result.expect("metadata");
        let merged = &metadata[&3];
        assert_eq!(merged.flags.as_deref(), Some(&["\\Seen".to_owned()][..]));
        assert_eq!(
            merged.gmail_labels.as_deref(),
            Some(
                &[
                    "\\Inbox".to_owned(),
                    "Muy Importante".to_owned(),
                    "été".to_owned()
                ][..]
            )
        );
        assert_eq!(merged.gmail_message_id, Some(1_278_455_344_230_334_865));
        assert_eq!(merged.gmail_thread_id, Some(1_278_455_344_230_334_866));
        assert_eq!(metadata[&4].gmail_labels, Some(Vec::new()));
        assert_eq!(metadata[&4].flags, Some(Vec::new()));
        assert!(metadata[&5].gmail_labels.is_none());
        assert!(metadata[&5].gmail_message_id.is_none());
    }

    #[tokio::test]
    async fn metadata_preview_prefix_is_bounded_and_oversize_keeps_headers() {
        let ok_line = format!(
            "* 1 FETCH (UID 1 FLAGS () {HEADER_ITEM} {} RFC822.SIZE 4000 BODY[]<0> {})",
            lit(HEADERS),
            lit("12345678")
        );
        let big_line = format!(
            "* 2 FETCH (UID 2 FLAGS () {HEADER_ITEM} {} RFC822.SIZE 4000 BODY[]<0> {})",
            lit(HEADERS),
            lit("123456789")
        );
        let (result, commands) = fetch_with(
            &[1, 2],
            MetadataProjection::Summary {
                preview_bytes: Some(8),
            },
            false,
            &[&ok_line, &big_line, "{tag} OK done"],
        )
        .await;
        assert_eq!(
            commands,
            vec![format!(
                "A0002 UID FETCH 1:2 (UID FLAGS {PEEK_ITEM} RFC822.SIZE BODY.PEEK[]<0.8>)"
            )]
        );
        let metadata = result.expect("metadata");
        assert_eq!(metadata[&1].raw_prefix.as_deref(), Some(&b"12345678"[..]));
        assert_eq!(metadata[&1].size_bytes, Some(4000));
        assert!(metadata[&1].preview_issue.is_none());
        assert!(metadata[&2].raw_prefix.is_none());
        assert!(metadata[&2].preview_issue.is_some());
        assert_eq!(
            metadata[&2].header_bytes.as_deref(),
            Some(HEADERS.as_bytes())
        );
    }

    #[tokio::test]
    async fn metadata_with_no_uids_sends_nothing() {
        let (result, commands) = fetch_with(&[], MetadataProjection::State, false, &[]).await;
        assert!(result.expect("empty").is_empty());
        assert!(commands.is_empty());
    }

    #[tokio::test]
    async fn metadata_zero_preview_budget_is_invalid_input() {
        let (result, commands) = fetch_with(
            &[1],
            MetadataProjection::Summary {
                preview_bytes: Some(0),
            },
            false,
            &[],
        )
        .await;
        assert!(matches!(result, Err(AppError::InvalidInput(_))));
        assert!(commands.is_empty());
    }

    #[tokio::test]
    async fn metadata_command_failure_is_an_error() {
        let (result, _) =
            fetch_with(&[1], MetadataProjection::State, false, &["{tag} NO nope"]).await;
        assert!(result.is_err());
    }

    /// Runs a label STORE against the replies and returns the outcome and commands.
    async fn labels_with(
        operation: SetOperation,
        labels: &[&str],
        lines: &[&str],
    ) -> (super::LabelStoreOutcome, Vec<String>) {
        let (mut session, peer) = connect(vec![reply(lines)]).await;
        let labels: Vec<String> = labels.iter().map(|label| (*label).to_owned()).collect();
        let outcome = store_message_labels(
            &test_server(1_000),
            &mut session,
            &[7, 8],
            operation,
            &labels,
        )
        .await;
        drop(session);
        (outcome, peer.await.expect("peer task"))
    }

    #[tokio::test]
    async fn label_store_commands_are_exact_and_unsilent() {
        let fetch = "* 1 FETCH (UID 7 X-GM-LABELS (\\Inbox Work))";
        let cases = [
            (
                SetOperation::Add,
                vec!["\\Inbox", "Work"],
                "A0002 UID STORE 7:8 +X-GM-LABELS (\\Inbox \"Work\")",
            ),
            (
                SetOperation::Remove,
                vec!["a b", "q\"r"],
                "A0002 UID STORE 7:8 -X-GM-LABELS (\"a b\" \"q\\\"r\")",
            ),
            (
                SetOperation::Replace,
                vec!["été"],
                "A0002 UID STORE 7:8 X-GM-LABELS (\"&AOk-t&AOk-\")",
            ),
            (
                SetOperation::Replace,
                vec![],
                "A0002 UID STORE 7:8 X-GM-LABELS ()",
            ),
        ];
        for (operation, labels, expected) in cases {
            let (outcome, commands) =
                labels_with(operation, &labels, &[fetch, "{tag} OK done"]).await;
            assert!(outcome.completion.is_ok());
            assert_eq!(commands, vec![expected.to_owned()]);
            assert_eq!(
                outcome.metadata[&7].gmail_labels.as_deref(),
                Some(&["\\Inbox".to_owned(), "Work".to_owned()][..])
            );
        }
    }

    #[tokio::test]
    async fn label_store_metadata_is_dropped_without_tagged_ok() {
        for completion in ["{tag} NO refused", "{tag} BAD broken"] {
            let (outcome, commands) = labels_with(
                SetOperation::Add,
                &["Work"],
                &["* 1 FETCH (UID 7 X-GM-LABELS (Work))", completion],
            )
            .await;
            assert!(matches!(
                outcome.completion,
                Err(CommandFailure::Rejected(_))
            ));
            assert!(outcome.metadata.is_empty());
            assert_eq!(commands.len(), 1);
        }
    }

    #[tokio::test]
    async fn label_store_without_uids_sends_nothing() {
        let (mut session, peer) = connect(Vec::new()).await;
        let outcome = store_message_labels(
            &test_server(1_000),
            &mut session,
            &[],
            SetOperation::Add,
            &["Work".to_owned()],
        )
        .await;
        assert!(outcome.completion.is_err());
        drop(session);
        assert!(peer.await.expect("peer task").is_empty());
    }

    #[test]
    fn gmail_labels_encode_for_the_wire() {
        assert_eq!(encode_gmail_label("Work"), "\"Work\"");
        assert_eq!(encode_gmail_label("a b"), "\"a b\"");
        assert_eq!(encode_gmail_label("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(encode_gmail_label("a\\b"), "\"a\\\\b\"");
        assert_eq!(encode_gmail_label("été"), "\"&AOk-t&AOk-\"");
        assert_eq!(encode_gmail_label("R&D"), "\"R&-D\"");
        assert_eq!(encode_gmail_label("\\Inbox"), "\\Inbox");
        assert_eq!(encode_gmail_label("\\Important"), "\\Important");
        assert_eq!(encode_gmail_label("\\Inbox2"), "\"\\\\Inbox2\"");
        assert_eq!(encode_gmail_label("\\"), "\"\\\\\"");
        assert_eq!(encode_gmail_label("\\In box"), "\"\\\\In box\"");
        assert_eq!(encode_gmail_label("\r\n"), "\"&AA0ACg-\"");
    }

    /// Runs STATUS for `INBOX` against the replies.
    async fn status_with(lines: &[&str]) -> crate::errors::AppResult<MailboxCounts> {
        let (mut session, peer) = connect(vec![reply(lines)]).await;
        let result = mailbox_status(&test_server(1_000), &mut session, "INBOX").await;
        drop(session);
        assert_eq!(
            peer.await.expect("peer task"),
            vec!["A0002 STATUS \"INBOX\" (MESSAGES UNSEEN)"]
        );
        result
    }

    #[tokio::test]
    async fn status_reports_observed_zero_counts() {
        let counts = status_with(&["* STATUS INBOX (MESSAGES 0 UNSEEN 0)", "{tag} OK done"])
            .await
            .expect("counts");
        assert_eq!(
            counts,
            MailboxCounts {
                total_messages: 0,
                unread_messages: 0
            }
        );
        let counts = status_with(&["* STATUS \"inbox\" (UNSEEN 3 MESSAGES 12)", "{tag} OK done"])
            .await
            .expect("counts");
        assert_eq!(
            counts,
            MailboxCounts {
                total_messages: 12,
                unread_messages: 3
            }
        );
    }

    #[tokio::test]
    async fn status_requires_matching_mailbox_and_both_values() {
        for lines in [
            vec!["* STATUS INBOX (MESSAGES 4)", "{tag} OK done"],
            vec!["* STATUS INBOX (UNSEEN 4)", "{tag} OK done"],
            vec!["* STATUS Other (MESSAGES 4 UNSEEN 1)", "{tag} OK done"],
            vec!["{tag} OK done"],
        ] {
            let result = status_with(&lines).await;
            assert!(matches!(result, Err(AppError::Internal(_))), "{lines:?}");
        }
    }

    #[tokio::test]
    async fn status_rejection_is_an_error() {
        assert!(status_with(&["{tag} NO no such mailbox"]).await.is_err());
    }

    #[tokio::test]
    async fn status_quotes_unicode_mailbox_names() {
        let (mut session, peer) = connect(vec![reply(&[
            "* STATUS &ZeVnLIqe- (MESSAGES 1 UNSEEN 1)",
            "{tag} OK done",
        ])])
        .await;
        let counts = mailbox_status(&test_server(1_000), &mut session, "日本語")
            .await
            .expect("counts");
        drop(session);
        assert_eq!(counts.total_messages, 1);
        assert_eq!(
            peer.await.expect("peer task"),
            vec!["A0002 STATUS \"&ZeVnLIqe-\" (MESSAGES UNSEEN)"]
        );
    }

    #[tokio::test]
    async fn expunge_uses_uid_set_and_checks_completion() {
        let server = test_server(1_000);
        let (mut session, peer) = connect(vec![
            reply(&["* 3 EXPUNGE", "{tag} OK done"]),
            reply(&["{tag} NO refused"]),
            reply(&["{tag} BAD broken"]),
        ])
        .await;
        assert!(
            uid_expunge_sequence(&server, &mut session, &[9, 3, 4])
                .await
                .is_ok()
        );
        assert!(
            uid_expunge_sequence(&server, &mut session, &[3])
                .await
                .is_err()
        );
        assert!(
            uid_expunge_sequence(&server, &mut session, &[3])
                .await
                .is_err()
        );
        drop(session);
        assert_eq!(
            peer.await.expect("peer task"),
            vec![
                "A0002 UID EXPUNGE 3:4,9",
                "A0003 UID EXPUNGE 3",
                "A0004 UID EXPUNGE 3"
            ]
        );
    }

    #[tokio::test]
    async fn expunge_without_uids_is_an_error_and_sends_nothing() {
        let (mut session, peer) = connect(Vec::new()).await;
        assert!(
            uid_expunge_sequence(&test_server(1_000), &mut session, &[])
                .await
                .is_err()
        );
        drop(session);
        assert!(peer.await.expect("peer task").is_empty());
    }
}
