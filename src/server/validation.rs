use std::collections::HashSet;

use chrono::{Duration as ChronoDuration, NaiveDate, Utc};

use crate::config::ServerConfig;
use crate::errors::{AppError, AppResult};
use crate::mailbox_codec::normalize_mailbox_name;
use crate::message_id::MessageId;
use crate::mime;
use crate::models::{
    ApplyToMessagesInput, AttachmentContentMode, GetAttachmentInput, ListMailboxesInput,
    ManageMailboxInput, SearchMessagesInput, SetOperation, UpdateMessageFlagsInput,
    UpdateMessageLabelsInput,
};

use super::types::{MailboxAction, MessageActionInput, PropertyUpdate, SearchThreadQuery};
use super::{MAX_BULK_MESSAGE_IDS, VALID_SYSTEM_FLAGS};

/// Maximum number of mailboxes accepted by one search.
const MAX_SEARCH_MAILBOXES: usize = 10;
/// Maximum characters of one custom keyword filter.
const MAX_KEYWORD_CHARS: usize = 64;

/// Maximum number of flags or labels accepted by one property update.
const MAX_PROPERTY_VALUES: usize = 32;
/// Maximum number of Unicode characters in one Gmail label.
const MAX_LABEL_CHARS: usize = 256;
/// Maximum mailboxes per page when live counts are requested.
const MAX_COUNTED_MAILBOXES: usize = 50;
/// Default characters returned in attachment text mode.
const ATTACHMENT_TEXT_DEFAULT_CHARS: usize = 10_000;
/// Default decoded bytes returned per attachment page in bytes mode.
pub(super) const ATTACHMENT_BYTES_DEFAULT: usize = 16_000;
/// Largest page of attachment text characters or decoded bytes.
const ATTACHMENT_MAX_PAGE: usize = 64_000;

pub(super) fn parse_and_validate_message_id(message_id: &str) -> AppResult<MessageId> {
    let message_id = MessageId::parse(message_id)?;
    validate_mailbox(&message_id.mailbox)?;
    Ok(message_id)
}

pub(super) fn build_message_action(input: &ApplyToMessagesInput) -> AppResult<MessageActionInput> {
    match input.action.as_str() {
        "move" => Ok(MessageActionInput::Move {
            destination_mailbox: required_mailbox_field(
                input.destination_mailbox.as_ref(),
                "destination_mailbox",
                "move",
            )?,
        }),
        "copy" => Ok(MessageActionInput::Copy {
            destination_mailbox: required_mailbox_field(
                input.destination_mailbox.as_ref(),
                "destination_mailbox",
                "copy",
            )?,
        }),
        "delete" => {
            reject_field_for_action(
                input.destination_mailbox.as_ref(),
                "destination_mailbox",
                "delete",
            )?;
            Ok(MessageActionInput::Delete)
        }
        _ => Err(AppError::InvalidInput(format!(
            "action must be one of move, copy, delete; got '{}'",
            input.action
        ))),
    }
}

/// Validates the flags of a flag update and builds the typed property update.
pub(super) fn build_flag_update(input: &UpdateMessageFlagsInput) -> AppResult<PropertyUpdate> {
    if input.flags.is_empty() {
        return Err(AppError::InvalidInput(
            "flags must contain at least one entry".to_owned(),
        ));
    }
    if input.flags.len() > MAX_PROPERTY_VALUES {
        return Err(AppError::InvalidInput(format!(
            "flags must contain at most {MAX_PROPERTY_VALUES} entries"
        )));
    }
    validate_flags(&input.flags, "flags")?;
    Ok(PropertyUpdate::Flags {
        operation: input.operation,
        flags: input.flags.clone(),
    })
}

/// Validates every Gmail label, removes duplicates in order, and builds the typed update.
///
/// Add and remove need at least one label; replace may be empty and then clears all labels.
pub(super) fn build_label_update(input: &UpdateMessageLabelsInput) -> AppResult<PropertyUpdate> {
    if input.labels.len() > MAX_PROPERTY_VALUES {
        return Err(AppError::InvalidInput(format!(
            "labels must contain at most {MAX_PROPERTY_VALUES} entries"
        )));
    }
    // Labels seen so far, used to drop duplicates while keeping the first spelling and order.
    let mut seen = HashSet::new();
    // Validated labels in input order without duplicates.
    let mut labels = Vec::with_capacity(input.labels.len());
    for (index, label) in input.labels.iter().enumerate() {
        validate_label(label, index)?;
        if seen.insert(label.as_str()) {
            labels.push(label.clone());
        }
    }
    if labels.is_empty() && input.operation != SetOperation::Replace {
        return Err(AppError::InvalidInput(
            "labels must contain at least one entry for add and remove; only replace may be empty"
                .to_owned(),
        ));
    }
    Ok(PropertyUpdate::GmailLabels {
        operation: input.operation,
        labels,
    })
}

/// Checks that one label is 1..256 characters without ASCII control characters.
///
/// The error names the label position and never echoes the label text.
fn validate_label(label: &str, index: usize) -> AppResult<()> {
    let chars = label.chars().count();
    if chars == 0 || chars > MAX_LABEL_CHARS {
        return Err(AppError::InvalidInput(format!(
            "labels[{index}] must be 1..{MAX_LABEL_CHARS} characters"
        )));
    }
    validate_no_controls(label, &format!("labels[{index}]"))
}

pub(super) fn build_mailbox_action(input: &ManageMailboxInput) -> AppResult<MailboxAction> {
    match input.action.as_str() {
        "create" => {
            reject_field_for_action(
                input.destination_mailbox.as_ref(),
                "destination_mailbox",
                "create",
            )?;
            Ok(MailboxAction::Create {
                mailbox: input.mailbox.clone(),
            })
        }
        "rename" => Ok(MailboxAction::Rename {
            mailbox: input.mailbox.clone(),
            destination_mailbox: required_mailbox_field(
                input.destination_mailbox.as_ref(),
                "destination_mailbox",
                "rename",
            )?,
        }),
        "delete" => {
            reject_field_for_action(
                input.destination_mailbox.as_ref(),
                "destination_mailbox",
                "delete",
            )?;
            Ok(MailboxAction::Delete {
                mailbox: input.mailbox.clone(),
            })
        }
        _ => Err(AppError::InvalidInput(format!(
            "action must be one of create, rename, delete; got '{}'",
            input.action
        ))),
    }
}

fn required_mailbox_field(value: Option<&String>, field: &str, action: &str) -> AppResult<String> {
    match value {
        Some(value) => Ok(value.clone()),
        None => Err(AppError::InvalidInput(format!(
            "{field} is required for action={action}"
        ))),
    }
}

fn reject_field_for_action<T>(value: Option<&T>, field: &str, action: &str) -> AppResult<()> {
    if value.is_some() {
        return Err(AppError::InvalidInput(format!(
            "{field} is not allowed for action={action}"
        )));
    }
    Ok(())
}

pub(super) fn validate_message_action(action: &MessageActionInput) -> AppResult<()> {
    match action {
        MessageActionInput::Move {
            destination_mailbox,
        }
        | MessageActionInput::Copy {
            destination_mailbox,
        } => validate_mailbox(destination_mailbox),
        MessageActionInput::Delete => Ok(()),
    }
}

pub(super) fn parse_bulk_message_ids(
    message_ids: &[String],
) -> AppResult<(String, Vec<MessageId>)> {
    if message_ids.is_empty() {
        return Err(AppError::InvalidInput(
            "message_ids must contain at least one entry".to_owned(),
        ));
    }
    if message_ids.len() > MAX_BULK_MESSAGE_IDS {
        return Err(AppError::InvalidInput(format!(
            "message_ids must contain at most {MAX_BULK_MESSAGE_IDS} entries"
        )));
    }
    dedupe_and_parse_message_ids(message_ids)
}

pub(super) fn dedupe_and_parse_message_ids(
    message_ids: &[String],
) -> AppResult<(String, Vec<MessageId>)> {
    let mut seen = HashSet::new();
    let mut resolved = Vec::new();
    let mut account_id = None;

    for message_id in message_ids {
        let parsed = parse_and_validate_message_id(message_id)?;
        match &account_id {
            Some(existing) if existing != &parsed.account_id => {
                return Err(AppError::InvalidInput(
                    "message_ids must all belong to the same account".to_owned(),
                ));
            }
            Some(_) => {}
            None => account_id = Some(parsed.account_id.clone()),
        }
        let encoded = parsed.encode();
        if seen.insert(encoded) {
            resolved.push(parsed);
        }
    }

    let account_id = account_id.ok_or_else(|| {
        AppError::InvalidInput("message_ids must contain at least one entry".to_owned())
    })?;
    Ok((account_id, resolved))
}

pub(super) fn validate_account_id(account_id: &str) -> AppResult<()> {
    if account_id.is_empty() || account_id.len() > 64 {
        return Err(AppError::InvalidInput(
            "account_id must be 1..64 characters".to_owned(),
        ));
    }
    if !account_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(AppError::InvalidInput(
            "account_id must match [A-Za-z0-9_-]+".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn validate_operation_id(operation_id: &str) -> AppResult<()> {
    if operation_id.is_empty() || operation_id.len() > 64 {
        return Err(AppError::InvalidInput(
            "operation_id must be 1..64 chars".to_owned(),
        ));
    }
    validate_no_controls(operation_id, "operation_id")
}

pub(super) fn validate_mailbox(mailbox: &str) -> AppResult<()> {
    if mailbox.is_empty() || mailbox.len() > 256 {
        return Err(AppError::InvalidInput(
            "mailbox must be 1..256 characters".to_owned(),
        ));
    }
    validate_no_controls(mailbox, "mailbox")
}

fn validate_no_controls(value: &str, field: &str) -> AppResult<()> {
    if value.chars().any(|ch| ch.is_ascii_control()) {
        return Err(AppError::InvalidInput(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

pub(super) fn validate_chars(value: usize, min: usize, max: usize, field: &str) -> AppResult<()> {
    if value < min || value > max {
        return Err(AppError::InvalidInput(format!(
            "{field} must be in range {min}..{max}"
        )));
    }
    Ok(())
}

/// Validates the list page size: 1..200, or 1..50 when live counts are requested.
pub(super) fn validate_list_mailboxes_input(input: &ListMailboxesInput) -> AppResult<()> {
    validate_account_id(&input.account_id)?;
    if input.include_counts && input.limit > MAX_COUNTED_MAILBOXES {
        return Err(AppError::InvalidInput(format!(
            "limit must be at most {MAX_COUNTED_MAILBOXES} when include_counts is true"
        )));
    }
    validate_chars(input.limit, 1, 200, "limit")
}

/// Validates attachment arguments and builds the typed MIME request.
///
/// Text mode accepts only `max_chars`; bytes mode accepts only `offset_bytes`
/// and `max_bytes`. The part-ID grammar is checked where the part is located.
pub(super) fn validate_attachment_input(
    input: &GetAttachmentInput,
) -> AppResult<mime::AttachmentRequest> {
    // The part-ID grammar is checked here so a malformed ID never causes IMAP traffic.
    mime::parse_part_id(&input.part_id)?;
    match input.mode {
        AttachmentContentMode::Text => {
            if input.offset_bytes.is_some() || input.max_bytes.is_some() {
                return Err(AppError::InvalidInput(
                    "offset_bytes and max_bytes require mode=bytes".to_owned(),
                ));
            }
            // Characters returned, defaulting to the documented size.
            let max_chars = input.max_chars.unwrap_or(ATTACHMENT_TEXT_DEFAULT_CHARS);
            validate_chars(max_chars, 1, ATTACHMENT_MAX_PAGE, "max_chars")?;
            Ok(mime::AttachmentRequest::Text { max_chars })
        }
        AttachmentContentMode::Bytes => {
            if input.max_chars.is_some() {
                return Err(AppError::InvalidInput(
                    "max_chars requires mode=text".to_owned(),
                ));
            }
            // Page size in decoded bytes, defaulting to the documented size.
            let max_bytes = input.max_bytes.unwrap_or(ATTACHMENT_BYTES_DEFAULT);
            validate_chars(max_bytes, 1, ATTACHMENT_MAX_PAGE, "max_bytes")?;
            Ok(mime::AttachmentRequest::Bytes {
                offset_bytes: input.offset_bytes.unwrap_or(0),
                max_bytes,
            })
        }
    }
}

/// Validates every search field before any network use.
///
/// Replayed filters on a continuation are ignored semantically, but their
/// syntax is still checked so control characters and injection-shaped text
/// never pass through.
pub(super) fn validate_search_input(input: &SearchMessagesInput) -> AppResult<()> {
    validate_search_mailboxes(&input.mailboxes)?;
    validate_chars(input.limit, 1, 100, "limit")?;

    if let Some(last_days) = input.last_days
        && !(1..=365).contains(&last_days)
    {
        return Err(AppError::InvalidInput(
            "last_days must be in range 1..365".to_owned(),
        ));
    }
    if let Some(preview_max_chars) = input.preview_max_chars {
        validate_chars(preview_max_chars, 50, 500, "preview_max_chars")?;
    }

    for text in [
        &input.query,
        &input.from,
        &input.to,
        &input.subject,
        &input.message_id_header,
        &input.gmail_query,
    ]
    .into_iter()
    .flatten()
    {
        validate_search_text(text)?;
    }
    if let Some(keyword) = &input.keyword {
        validate_keyword(keyword)?;
    }
    if let Some(seed) = &input.thread_of_message_id {
        // Parsed seed identifier; its account is only enforced for new searches.
        let seed = parse_and_validate_message_id(seed)?;
        if input.cursor.is_none() && seed.account_id != input.account_id {
            return Err(AppError::InvalidInput(
                "thread_of_message_id must belong to the searched account".to_owned(),
            ));
        }
    }

    let start_date = input.start_date.as_deref().map(parse_ymd).transpose()?;
    let end_date = input.end_date.as_deref().map(parse_ymd).transpose()?;

    if input.cursor.is_some() {
        return Ok(());
    }

    if input.last_days.is_some() && (input.start_date.is_some() || input.end_date.is_some()) {
        return Err(AppError::InvalidInput(
            "last_days cannot be combined with start_date/end_date".to_owned(),
        ));
    }

    if let (Some(start_date), Some(end_date)) = (start_date, end_date)
        && start_date > end_date
    {
        return Err(AppError::InvalidInput(
            "start_date must be <= end_date".to_owned(),
        ));
    }

    Ok(())
}

/// Checks that 1..10 requested mailboxes are valid and distinct after normalization.
///
/// `INBOX` and `inbox` collide because normalization canonicalizes the reserved name.
fn validate_search_mailboxes(mailboxes: &[String]) -> AppResult<()> {
    if mailboxes.is_empty() || mailboxes.len() > MAX_SEARCH_MAILBOXES {
        return Err(AppError::InvalidInput(format!(
            "mailboxes must contain 1..{MAX_SEARCH_MAILBOXES} entries"
        )));
    }
    // Canonical identities seen so far, used to reject duplicates.
    let mut seen = HashSet::new();
    for mailbox in mailboxes {
        validate_mailbox(mailbox)?;
        if !seen.insert(normalize_mailbox_name(mailbox)) {
            return Err(AppError::InvalidInput(
                "mailboxes must not contain duplicates; INBOX is case-insensitive".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Checks that a keyword filter is a 1..64 character custom IMAP atom.
///
/// Backslash system flags are rejected; use `flagged` or `answered` for those.
fn validate_keyword(keyword: &str) -> AppResult<()> {
    if keyword.is_empty() || keyword.len() > MAX_KEYWORD_CHARS {
        return Err(AppError::InvalidInput(format!(
            "keyword must be 1..{MAX_KEYWORD_CHARS} characters"
        )));
    }
    if keyword.starts_with('\\') {
        return Err(AppError::InvalidInput(
            "keyword must be a custom keyword, not a backslash system flag".to_owned(),
        ));
    }
    let is_atom_char = |ch: char| {
        ch.is_ascii()
            && !ch.is_ascii_control()
            && !matches!(
                ch,
                ' ' | '(' | ')' | '{' | '}' | '%' | '*' | '"' | '\\' | ']'
            )
    };
    if !keyword.chars().all(is_atom_char) {
        return Err(AppError::InvalidInput(
            "keyword must be an IMAP atom without spaces, quotes, brackets, or wildcards"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Rejects `gmail_query` on a new search when the server is not Gmail-capable.
///
/// A continuation ignores its replayed filters, so it is never rejected here.
pub(super) fn check_gmail_query_supported(
    input: &SearchMessagesInput,
    gmail_capable: bool,
) -> AppResult<()> {
    if input.cursor.is_none() && input.gmail_query.is_some() && !gmail_capable {
        return Err(AppError::InvalidInput(
            "gmail_query requires a server that advertises X-GM-EXT-1".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn validate_search_text(input: &str) -> AppResult<()> {
    if input.is_empty() || input.len() > 256 {
        return Err(AppError::InvalidInput(
            "search text fields must be 1..256 chars".to_owned(),
        ));
    }
    validate_no_controls(input, "search text")
}

/// Builds the `UID SEARCH` criteria for a new search.
///
/// Every filter and the optional conversation criterion are ANDed. Text
/// values are quoted and escaped; no raw IMAP text is accepted.
pub(super) fn build_search_query(
    input: &SearchMessagesInput,
    thread: Option<&SearchThreadQuery>,
) -> AppResult<String> {
    let mut parts = Vec::new();
    if let Some(value) = &input.query {
        parts.push(format!("TEXT \"{}\"", escape_imap_quoted(value)?));
    }
    if let Some(value) = &input.from {
        parts.push(format!("FROM \"{}\"", escape_imap_quoted(value)?));
    }
    if let Some(value) = &input.to {
        parts.push(format!("TO \"{}\"", escape_imap_quoted(value)?));
    }
    if let Some(value) = &input.subject {
        parts.push(format!("SUBJECT \"{}\"", escape_imap_quoted(value)?));
    }
    if input.unread_only.unwrap_or(false) {
        parts.push("UNSEEN".to_owned());
    }
    if let Some(flagged) = input.flagged {
        parts.push(if flagged { "FLAGGED" } else { "UNFLAGGED" }.to_owned());
    }
    if let Some(answered) = input.answered {
        parts.push(if answered { "ANSWERED" } else { "UNANSWERED" }.to_owned());
    }
    if let Some(keyword) = &input.keyword {
        validate_keyword(keyword)?;
        parts.push(format!("KEYWORD {keyword}"));
    }
    if let Some(value) = &input.message_id_header {
        parts.push(format!(
            "HEADER Message-ID \"{}\"",
            escape_imap_quoted(value)?
        ));
    }
    if let Some(value) = &input.gmail_query {
        parts.push(format!("X-GM-RAW \"{}\"", escape_imap_quoted(value)?));
    }
    if let Some(days) = input.last_days {
        let since = Utc::now().date_naive() - ChronoDuration::days(i64::from(days));
        parts.push(format!("SINCE {}", imap_date(since)));
    }
    if let Some(start) = &input.start_date {
        parts.push(format!("SINCE {}", imap_date(parse_ymd(start)?)));
    }
    if let Some(end) = &input.end_date {
        let end_exclusive = parse_ymd(end)? + ChronoDuration::days(1);
        parts.push(format!("BEFORE {}", imap_date(end_exclusive)));
    }
    match thread {
        Some(SearchThreadQuery::Gmail(thread_id)) => {
            parts.push(format!("X-GM-THRID {thread_id}"));
        }
        Some(SearchThreadQuery::Headers(ids)) => parts.push(thread_header_criteria(ids)?),
        None => {}
    }

    if parts.is_empty() {
        Ok("ALL".to_owned())
    } else {
        Ok(parts.join(" "))
    }
}

/// Builds one parenthesized nested `OR` over Message-ID, References, and In-Reply-To.
///
/// Each parsed ID is wrapped in angle brackets and escaped as a quoted string.
/// HEADER search is substring-based and case-insensitive, so the result is
/// only candidate discovery; callers must verify the parsed IDs afterwards.
fn thread_header_criteria(ids: &[String]) -> AppResult<String> {
    // Individual HEADER criteria, three per ID, joined by OR below.
    let mut criteria = Vec::with_capacity(ids.len() * 3);
    for id in ids {
        let escaped = escape_imap_quoted(id)?;
        for header in ["Message-ID", "References", "In-Reply-To"] {
            criteria.push(format!("HEADER {header} \"<{escaped}>\""));
        }
    }
    // Right-nested expression; IMAP OR takes exactly two search keys.
    let mut expression = criteria.pop().ok_or_else(|| {
        AppError::InvalidInput("thread lookup needs at least one Message-ID".to_owned())
    })?;
    while let Some(criterion) = criteria.pop() {
        expression = format!("OR {criterion} {expression}");
    }
    Ok(format!("({expression})"))
}

pub(super) fn escape_imap_quoted(input: &str) -> AppResult<String> {
    validate_search_text(input)?;
    Ok(input.replace('\\', "\\\\").replace('"', "\\\""))
}

fn validate_flags(flags: &[String], field: &str) -> AppResult<()> {
    for flag in flags {
        validate_flag(flag).map_err(|_| invalid_flag_error(field, flag))?;
    }
    Ok(())
}

pub(super) fn validate_flag(flag: &str) -> AppResult<()> {
    if flag.is_empty() || flag.len() > 64 {
        return Err(AppError::InvalidInput("invalid flag".to_owned()));
    }

    let atom = match flag.strip_prefix('\\') {
        Some(rest) => {
            if rest.is_empty() || !VALID_SYSTEM_FLAGS.contains(&flag) {
                return Err(AppError::InvalidInput("invalid flag".to_owned()));
            }
            rest
        }
        None => flag,
    };

    if atom.chars().any(|ch| {
        ch.is_ascii_control()
            || ch.is_ascii_whitespace()
            || matches!(ch, '"' | '(' | ')' | '{' | '}' | '\\')
    }) {
        return Err(AppError::InvalidInput("invalid flag".to_owned()));
    }

    Ok(())
}

fn invalid_flag_error(field: &str, flag: &str) -> AppError {
    AppError::InvalidInput(format!(
        "{field} contains invalid flag '{flag}'. Valid standard flags are {}. Custom keywords may also be allowed by the server if they are valid IMAP atoms.",
        VALID_SYSTEM_FLAGS.join(", ")
    ))
}

fn imap_date(date: NaiveDate) -> String {
    date.format("%-d-%b-%Y").to_string()
}

fn parse_ymd(input: &str) -> AppResult<NaiveDate> {
    NaiveDate::parse_from_str(input, "%Y-%m-%d")
        .map_err(|_| AppError::InvalidInput(format!("invalid date '{input}', expected YYYY-MM-DD")))
}

pub(super) fn header_value(headers: &[(String, String)], key: &str) -> Option<String> {
    headers
        .iter()
        .find_map(|(header, value)| header.eq_ignore_ascii_case(key).then(|| value.clone()))
}

pub(super) fn require_write_enabled(config: &ServerConfig) -> AppResult<()> {
    if !config.write_enabled {
        return Err(AppError::InvalidInput(
            "write tools are disabled; set MAIL_IMAP_WRITE_ENABLED=true".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        build_flag_update, build_label_update, build_mailbox_action, build_message_action,
        build_search_query, check_gmail_query_supported, dedupe_and_parse_message_ids,
        escape_imap_quoted, parse_bulk_message_ids, validate_attachment_input, validate_flag,
        validate_list_mailboxes_input, validate_mailbox, validate_search_input,
        validate_search_text,
    };
    use crate::errors::AppError;
    use crate::mime;
    use crate::models::{
        ApplyToMessagesInput, AttachmentContentMode, GetAttachmentInput, ListMailboxesInput,
        ManageMailboxInput, SearchMessagesInput, SetOperation, UpdateMessageFlagsInput,
        UpdateMessageLabelsInput,
    };
    use crate::server::types::{PropertyUpdate, SearchThreadQuery};

    #[test]
    fn control_character_validation_rejects_invalid_inputs() {
        let cases = [
            (
                "search text",
                validate_search_text("hello\nworld").map(|_| ()),
            ),
            ("mailbox", validate_mailbox("INBOX\r").map(|_| ())),
            ("imap quoted", escape_imap_quoted("a\nb").map(|_| ())),
        ];

        for (label, result) in cases {
            let err = result.expect_err(label);
            assert!(err.to_string().contains("control characters"));
        }
    }

    #[test]
    fn validate_flag_allows_common_flags() {
        validate_flag("\\Seen").expect("system flag must be valid");
        validate_flag("Important").expect("keyword flag must be valid");
        validate_flag("$MailFlagBit0").expect("keyword flag must be valid");
    }

    #[test]
    fn validate_flag_rejects_injection_like_value() {
        let err = validate_flag("\\Seen) UID FETCH 1:* (BODY[]").expect_err("must fail");
        assert!(err.to_string().contains("invalid flag"));
    }

    fn flag_input(operation: SetOperation, flags: &[&str]) -> UpdateMessageFlagsInput {
        UpdateMessageFlagsInput {
            message_ids: vec!["imap:default:INBOX:42:7".to_owned()],
            operation,
            flags: flags.iter().map(|flag| (*flag).to_owned()).collect(),
        }
    }

    fn label_input(operation: SetOperation, labels: &[&str]) -> UpdateMessageLabelsInput {
        UpdateMessageLabelsInput {
            message_ids: vec!["imap:default:INBOX:42:7".to_owned()],
            operation,
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        }
    }

    #[test]
    fn build_flag_update_lists_valid_standard_flags() {
        let err = build_flag_update(&flag_input(SetOperation::Add, &["\\Read"]))
            .expect_err("must reject unknown system flag");
        let message = err.to_string();
        assert!(message.contains("Valid standard flags are"));
        assert!(message.contains("\\Seen"));
        assert!(message.contains("\\Answered"));
        assert!(message.contains("\\Flagged"));
        assert!(message.contains("\\Deleted"));
        assert!(message.contains("\\Draft"));
    }

    #[test]
    fn build_flag_update_rejects_empty_and_excessive_flags() {
        let err = build_flag_update(&flag_input(SetOperation::Add, &[]))
            .expect_err("must reject empty flags");
        assert!(
            err.to_string()
                .contains("flags must contain at least one entry")
        );
        let many = vec!["Keyword"; 33];
        let err = build_flag_update(&flag_input(SetOperation::Replace, &many))
            .expect_err("must reject more than 32 flags");
        assert!(err.to_string().contains("at most 32"));
    }

    #[test]
    fn build_flag_update_keeps_operation_and_flags() {
        let update = build_flag_update(&flag_input(SetOperation::Remove, &["\\Seen", "Work"]))
            .expect("valid flags must build");
        let PropertyUpdate::Flags { operation, flags } = update else {
            panic!("flag input must build a flag update");
        };
        assert_eq!(operation, SetOperation::Remove);
        assert_eq!(flags, ["\\Seen", "Work"]);
    }

    #[test]
    fn build_label_update_rejects_controls_and_length_without_echoing_label() {
        let long = "x".repeat(257);
        for (labels, expected) in [
            (
                vec!["ok", "bad\nlabel"],
                "labels[1] must not contain control characters",
            ),
            (
                vec!["tab\there"],
                "labels[0] must not contain control characters",
            ),
            (
                vec!["\u{7f}"],
                "labels[0] must not contain control characters",
            ),
            (vec![""], "labels[0] must be 1..256 characters"),
            (vec![long.as_str()], "labels[0] must be 1..256 characters"),
        ] {
            let err = build_label_update(&label_input(SetOperation::Add, &labels))
                .expect_err("invalid label must be rejected");
            let message = err.to_string();
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("xxxx"), "message must not echo the label");
        }
    }

    #[test]
    fn build_label_update_counts_characters_not_bytes() {
        let max = "é".repeat(256);
        build_label_update(&label_input(SetOperation::Add, &[max.as_str()]))
            .expect("256 multibyte characters must be accepted");
        let over = "é".repeat(257);
        build_label_update(&label_input(SetOperation::Add, &[over.as_str()]))
            .expect_err("257 multibyte characters must be rejected");
    }

    #[test]
    fn build_label_update_rejects_more_than_32_labels() {
        let names = (0..33)
            .map(|index| format!("label{index}"))
            .collect::<Vec<_>>();
        let labels = names.iter().map(String::as_str).collect::<Vec<_>>();
        let err = build_label_update(&label_input(SetOperation::Replace, &labels))
            .expect_err("33 labels must be rejected");
        assert!(err.to_string().contains("at most 32"));
        build_label_update(&label_input(SetOperation::Replace, &labels[..32]))
            .expect("32 labels must be accepted");
    }

    #[test]
    fn build_label_update_deduplicates_preserving_order() {
        let update = build_label_update(&label_input(
            SetOperation::Add,
            &["Work", "a b", "Work", "Étiquette", "a b"],
        ))
        .expect("valid labels must build");
        let PropertyUpdate::GmailLabels { operation, labels } = update else {
            panic!("label input must build a label update");
        };
        assert_eq!(operation, SetOperation::Add);
        assert_eq!(labels, ["Work", "a b", "Étiquette"]);
    }

    #[test]
    fn build_label_update_allows_empty_only_for_replace() {
        let update = build_label_update(&label_input(SetOperation::Replace, &[]))
            .expect("empty replace clears all labels");
        assert!(matches!(
            update,
            PropertyUpdate::GmailLabels { operation: SetOperation::Replace, ref labels }
                if labels.is_empty()
        ));
        for operation in [SetOperation::Add, SetOperation::Remove] {
            let err = build_label_update(&label_input(operation, &[]))
                .expect_err("empty add and remove must be rejected");
            assert!(err.to_string().contains("at least one entry"));
        }
    }

    #[test]
    fn dedupe_and_parse_message_ids_removes_duplicates() {
        let message_ids = vec![
            "imap:default:INBOX:42:7".to_owned(),
            "imap:default:INBOX:42:7".to_owned(),
            "imap:default:INBOX:42:8".to_owned(),
        ];

        let (account_id, parsed) =
            dedupe_and_parse_message_ids(&message_ids).expect("message ids should parse");
        assert_eq!(account_id, "default");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].uid, 7);
        assert_eq!(parsed[1].uid, 8);
    }

    #[test]
    fn parse_bulk_message_ids_rejects_empty_input() {
        let err = parse_bulk_message_ids(&[]).expect_err("must reject empty message_ids");
        assert!(
            err.to_string()
                .contains("message_ids must contain at least one entry")
        );
    }

    #[test]
    fn build_message_action_requires_destination_for_move_and_copy() {
        for action in ["move", "copy"] {
            let input = ApplyToMessagesInput {
                message_ids: vec!["imap:default:INBOX:42:7".to_owned()],
                action: action.to_owned(),
                destination_mailbox: None,
            };

            let err = build_message_action(&input).expect_err("action must require destination");
            assert!(err.to_string().contains(&format!(
                "destination_mailbox is required for action={action}"
            )));
        }
    }

    #[test]
    fn build_mailbox_action_rejects_destination_for_delete() {
        let input = ManageMailboxInput {
            account_id: "default".to_owned(),
            action: "delete".to_owned(),
            mailbox: "Archive".to_owned(),
            destination_mailbox: Some("Archive/Elsewhere".to_owned()),
        };

        let err = build_mailbox_action(&input).expect_err("delete must reject destination_mailbox");
        assert!(
            err.to_string()
                .contains("destination_mailbox is not allowed for action=delete")
        );
    }

    fn cursor_search_input() -> SearchMessagesInput {
        SearchMessagesInput {
            account_id: "default".to_owned(),
            mailboxes: vec!["Donations".to_owned()],
            cursor: Some("cursor-id".to_owned()),
            query: None,
            from: None,
            to: None,
            subject: None,
            unread_only: None,
            flagged: None,
            answered: None,
            keyword: None,
            message_id_header: None,
            gmail_query: None,
            thread_of_message_id: None,
            last_days: None,
            start_date: None,
            end_date: None,
            limit: 100,
            preview_max_chars: None,
        }
    }

    fn new_search_input() -> SearchMessagesInput {
        SearchMessagesInput {
            cursor: None,
            ..cursor_search_input()
        }
    }

    #[test]
    fn validate_search_input_allows_conflicting_and_reversed_replay_dates() {
        let input = SearchMessagesInput {
            last_days: Some(365),
            start_date: Some("2025-12-31".to_owned()),
            end_date: Some("2025-01-01".to_owned()),
            ..cursor_search_input()
        };

        validate_search_input(&input)
            .expect("cursor mode should ignore relationships between valid replay criteria");
    }

    #[test]
    fn validate_search_input_rejects_invalid_replay_text_fields() {
        for field in [
            "query",
            "from",
            "to",
            "subject",
            "message_id_header",
            "gmail_query",
        ] {
            for value in ["x".repeat(257), "hello\nworld".to_owned()] {
                let expected = if value.len() > 256 {
                    "search text fields must be 1..256 chars"
                } else {
                    "search text must not contain control characters"
                };
                let mut input = cursor_search_input();
                match field {
                    "query" => input.query = Some(value),
                    "from" => input.from = Some(value),
                    "to" => input.to = Some(value),
                    "subject" => input.subject = Some(value),
                    "message_id_header" => input.message_id_header = Some(value),
                    "gmail_query" => input.gmail_query = Some(value),
                    _ => unreachable!(),
                }

                let err = validate_search_input(&input)
                    .expect_err("invalid replay text must be rejected");
                assert!(err.to_string().contains(expected), "{field}: {err}");
            }
        }
    }

    #[test]
    fn validate_search_input_rejects_invalid_replay_bounds() {
        for last_days in [0, 366] {
            let input = SearchMessagesInput {
                last_days: Some(last_days),
                ..cursor_search_input()
            };
            let err = validate_search_input(&input).expect_err("last_days must be rejected");
            assert!(
                err.to_string()
                    .contains("last_days must be in range 1..365")
            );
        }

        for preview_max_chars in [49, 501] {
            let input = SearchMessagesInput {
                preview_max_chars: Some(preview_max_chars),
                ..cursor_search_input()
            };
            let err =
                validate_search_input(&input).expect_err("preview_max_chars must be rejected");
            assert!(
                err.to_string()
                    .contains("preview_max_chars must be in range 50..500")
            );
        }
    }

    #[test]
    fn validate_search_input_rejects_individually_invalid_replay_dates() {
        for (start_date, end_date) in [
            (Some("not-a-date".to_owned()), None),
            (None, Some("2025-02-30".to_owned())),
        ] {
            let input = SearchMessagesInput {
                start_date,
                end_date,
                ..cursor_search_input()
            };
            let err =
                validate_search_input(&input).expect_err("invalid replay date must be rejected");
            assert!(err.to_string().contains("expected YYYY-MM-DD"));
        }
    }

    #[test]
    fn validate_search_input_still_rejects_conflicting_dates_without_cursor() {
        let input = SearchMessagesInput {
            last_days: Some(30),
            start_date: Some("2025-01-01".to_owned()),
            end_date: Some("2025-12-31".to_owned()),
            ..new_search_input()
        };

        let err = validate_search_input(&input).expect_err("must reject conflicting date filters");
        assert!(
            err.to_string()
                .contains("last_days cannot be combined with start_date/end_date")
        );
    }

    #[test]
    fn validate_search_input_still_rejects_reversed_dates_without_cursor() {
        let input = SearchMessagesInput {
            start_date: Some("2025-12-31".to_owned()),
            end_date: Some("2025-01-01".to_owned()),
            ..new_search_input()
        };

        let err = validate_search_input(&input).expect_err("must reject reversed date range");
        assert!(err.to_string().contains("start_date must be <= end_date"));
    }

    #[test]
    fn validate_search_input_accepts_preview_size_without_boolean_toggle() {
        let input = SearchMessagesInput {
            preview_max_chars: Some(200),
            ..new_search_input()
        };

        validate_search_input(&input).expect("preview_max_chars alone should enable previews");
    }

    #[test]
    fn validate_search_input_enforces_mailbox_count_and_duplicates() {
        let names = |count: usize| (0..count).map(|index| format!("Box{index}")).collect();
        for mailboxes in [Vec::new(), names(11)] {
            let input = SearchMessagesInput {
                mailboxes,
                ..new_search_input()
            };
            let err = validate_search_input(&input).expect_err("count must be 1..10");
            assert!(err.to_string().contains("mailboxes must contain 1..10"));
        }
        validate_search_input(&SearchMessagesInput {
            mailboxes: names(10),
            ..new_search_input()
        })
        .expect("ten mailboxes are accepted");

        for pair in [
            ["INBOX", "inbox"],
            ["Inbox", "INBOX"],
            ["日本語", "&ZeVnLIqe-"],
            ["Sent", "Sent"],
        ] {
            let input = SearchMessagesInput {
                mailboxes: pair.iter().map(|name| (*name).to_owned()).collect(),
                ..new_search_input()
            };
            let err = validate_search_input(&input).expect_err("duplicates must be rejected");
            assert!(matches!(err, AppError::InvalidInput(_)));
            assert!(err.to_string().contains("duplicates"), "{pair:?}");
        }
        validate_search_input(&SearchMessagesInput {
            mailboxes: vec![
                "INBOX".to_owned(),
                "INBOX.Sub".to_owned(),
                "Inbox2".to_owned(),
            ],
            ..new_search_input()
        })
        .expect("distinct names are accepted");
    }

    #[test]
    fn validate_search_input_rejects_invalid_mailbox_entries() {
        for name in ["", "bad\nname"] {
            let input = SearchMessagesInput {
                mailboxes: vec!["INBOX".to_owned(), name.to_owned()],
                ..new_search_input()
            };
            validate_search_input(&input).expect_err("invalid mailbox entry");
        }
        let long = SearchMessagesInput {
            mailboxes: vec!["x".repeat(257)],
            ..new_search_input()
        };
        validate_search_input(&long).expect_err("overlong mailbox entry");
    }

    #[test]
    fn keyword_validation_accepts_custom_atoms_and_rejects_system_flags_and_injection() {
        for keyword in ["Work", "$Forwarded", "a.b-c_d", &"k".repeat(64)] {
            validate_search_input(&SearchMessagesInput {
                keyword: Some(keyword.to_owned()),
                ..new_search_input()
            })
            .unwrap_or_else(|error| panic!("{keyword} must be accepted: {error}"));
        }
        for keyword in [
            "",
            "\\Seen",
            "\\Flagged",
            "has space",
            "a)b",
            "a(b",
            "q\"uote",
            "wild*",
            "per%cent",
            "br]acket",
            "{lit}",
            "ctl\u{7}",
            "nl\n",
            "ünïcode",
            &"k".repeat(65),
        ] {
            let err = validate_search_input(&SearchMessagesInput {
                keyword: Some(keyword.to_owned()),
                cursor: Some("replayed-cursor".to_owned()),
                ..new_search_input()
            })
            .expect_err("invalid keyword must be rejected even on a continuation");
            assert!(matches!(err, AppError::InvalidInput(_)), "{keyword:?}");
        }
    }

    #[test]
    fn thread_seed_must_be_valid_and_same_account_on_new_searches_only() {
        let foreign = Some("imap:other:INBOX:42:7".to_owned());
        let err = validate_search_input(&SearchMessagesInput {
            thread_of_message_id: foreign.clone(),
            ..new_search_input()
        })
        .expect_err("cross-account seed must be rejected before the network");
        assert!(err.to_string().contains("searched account"));

        validate_search_input(&SearchMessagesInput {
            thread_of_message_id: foreign,
            ..cursor_search_input()
        })
        .expect("a replayed seed is ignored on continuations");

        for cursor in [None, Some("cursor-id".to_owned())] {
            validate_search_input(&SearchMessagesInput {
                thread_of_message_id: Some("not a message id".to_owned()),
                cursor,
                ..new_search_input()
            })
            .expect_err("malformed seed must be rejected");
        }
        validate_search_input(&SearchMessagesInput {
            thread_of_message_id: Some("imap:default:INBOX:42:7".to_owned()),
            ..new_search_input()
        })
        .expect("same-account seed is accepted");
    }

    #[test]
    fn gmail_query_is_rejected_on_new_non_gmail_searches_only() {
        let new = SearchMessagesInput {
            gmail_query: Some("has:attachment".to_owned()),
            ..new_search_input()
        };
        let err = check_gmail_query_supported(&new, false).expect_err("non-Gmail new search");
        assert!(matches!(err, AppError::InvalidInput(_)));
        check_gmail_query_supported(&new, true).expect("Gmail server accepts gmail_query");

        let replay = SearchMessagesInput {
            gmail_query: Some("has:attachment".to_owned()),
            ..cursor_search_input()
        };
        check_gmail_query_supported(&replay, false)
            .expect("continuation ignores the replayed filter");
        check_gmail_query_supported(&new_search_input(), false).expect("no gmail_query is fine");
    }

    #[test]
    fn build_search_query_generates_positive_and_negative_flag_keys() {
        let query = |flagged, answered| {
            build_search_query(
                &SearchMessagesInput {
                    flagged,
                    answered,
                    ..new_search_input()
                },
                None,
            )
            .expect("query builds")
        };
        assert_eq!(query(Some(true), None), "FLAGGED");
        assert_eq!(query(Some(false), None), "UNFLAGGED");
        assert_eq!(query(None, Some(true)), "ANSWERED");
        assert_eq!(query(None, Some(false)), "UNANSWERED");
        assert_eq!(
            query(Some(false), Some(true)),
            "UNFLAGGED ANSWERED",
            "flag filters are ANDed"
        );
        assert_eq!(query(None, None), "ALL");
    }

    #[test]
    fn build_search_query_generates_keyword_and_escaped_text_criteria() {
        let input = SearchMessagesInput {
            keyword: Some("Work".to_owned()),
            message_id_header: Some("<a\"b\\c@example.com>".to_owned()),
            gmail_query: Some("subject:\"q\" \\o/".to_owned()),
            unread_only: Some(true),
            ..new_search_input()
        };
        assert_eq!(
            build_search_query(&input, None).expect("query builds"),
            "UNSEEN KEYWORD Work HEADER Message-ID \"<a\\\"b\\\\c@example.com>\" \
             X-GM-RAW \"subject:\\\"q\\\" \\\\o/\""
        );
    }

    #[test]
    fn build_search_query_rejects_injection_shaped_keywords() {
        let input = SearchMessagesInput {
            keyword: Some("a) UID FETCH 1:* (BODY[]".to_owned()),
            ..new_search_input()
        };
        build_search_query(&input, None).expect_err("keyword injection must be rejected");
    }

    #[test]
    fn gmail_thread_query_uses_the_numeric_thread_id_and_ands_with_filters() {
        let input = SearchMessagesInput {
            flagged: Some(true),
            subject: Some("plan".to_owned()),
            ..new_search_input()
        };
        let thread = SearchThreadQuery::Gmail(1_278_455_344_230_334_865);
        assert_eq!(
            build_search_query(&input, Some(&thread)).expect("query builds"),
            "SUBJECT \"plan\" FLAGGED X-GM-THRID 1278455344230334865"
        );
        assert_eq!(
            build_search_query(&new_search_input(), Some(&thread)).expect("query builds"),
            "X-GM-THRID 1278455344230334865"
        );
    }

    #[test]
    fn header_thread_query_nests_or_with_angle_delimited_escaped_ids() {
        let thread = SearchThreadQuery::Headers(vec!["a@x".to_owned()]);
        assert_eq!(
            build_search_query(&new_search_input(), Some(&thread)).expect("query builds"),
            "(OR HEADER Message-ID \"<a@x>\" OR HEADER References \"<a@x>\" \
             HEADER In-Reply-To \"<a@x>\")"
        );

        let thread = SearchThreadQuery::Headers(vec!["q\"b\\s@x".to_owned(), "b@x".to_owned()]);
        let input = SearchMessagesInput {
            unread_only: Some(true),
            ..new_search_input()
        };
        let query = build_search_query(&input, Some(&thread)).expect("query builds");
        assert!(query.starts_with("UNSEEN (OR HEADER Message-ID \"<q\\\"b\\\\s@x>\" OR "));
        assert!(query.ends_with("HEADER In-Reply-To \"<b@x>\")"));
        assert_eq!(
            query.matches("OR ").count(),
            5,
            "six criteria need five ORs"
        );
        assert_eq!(query.matches("HEADER ").count(), 6);
    }

    #[test]
    fn header_thread_query_rejects_empty_and_control_character_ids() {
        let empty = SearchThreadQuery::Headers(Vec::new());
        build_search_query(&new_search_input(), Some(&empty)).expect_err("no ids");
        let control = SearchThreadQuery::Headers(vec!["a\r\nb@x".to_owned()]);
        build_search_query(&new_search_input(), Some(&control)).expect_err("control characters");
    }

    /// Builds a mailbox list input for page-bound tests.
    fn list_input(limit: usize, include_counts: bool) -> ListMailboxesInput {
        ListMailboxesInput {
            account_id: "default".to_owned(),
            cursor: None,
            limit,
            include_counts,
        }
    }

    #[test]
    fn validate_list_mailboxes_input_enforces_page_bounds() {
        for limit in [0, 201] {
            assert!(
                validate_list_mailboxes_input(&list_input(limit, false)).is_err(),
                "limit {limit} must be rejected"
            );
        }
        let input = ListMailboxesInput {
            cursor: Some("cursor-id".to_owned()),
            ..list_input(200, false)
        };
        validate_list_mailboxes_input(&input).expect("maximum valid limit must be accepted");
    }

    #[test]
    fn count_mode_limits_pages_to_fifty_mailboxes() {
        validate_list_mailboxes_input(&list_input(50, true)).expect("50 is valid in count mode");
        validate_list_mailboxes_input(&list_input(1, true)).expect("1 is valid in count mode");
        let error = validate_list_mailboxes_input(&list_input(51, true))
            .expect_err("51 must be rejected in count mode");
        assert!(matches!(error, AppError::InvalidInput(_)));
        assert!(error.to_string().contains("include_counts"));
        validate_list_mailboxes_input(&list_input(51, false))
            .expect("51 stays valid without counts");
        validate_list_mailboxes_input(&list_input(200, false))
            .expect("200 stays valid without counts");
        validate_list_mailboxes_input(&list_input(0, true)).expect_err("0 is never valid");
    }

    /// Builds attachment input with every optional field unset.
    fn attachment_input(mode: AttachmentContentMode) -> GetAttachmentInput {
        GetAttachmentInput {
            message_id: "imap:default:INBOX:7:42".to_owned(),
            part_id: "1.2".to_owned(),
            mode,
            max_chars: None,
            offset_bytes: None,
            max_bytes: None,
        }
    }

    #[test]
    fn attachment_text_mode_defaults_and_bounds() {
        assert_eq!(
            validate_attachment_input(&attachment_input(AttachmentContentMode::Text))
                .expect("defaults"),
            mime::AttachmentRequest::Text { max_chars: 10_000 }
        );
        for max_chars in [1, 64_000] {
            let input = GetAttachmentInput {
                max_chars: Some(max_chars),
                ..attachment_input(AttachmentContentMode::Text)
            };
            assert_eq!(
                validate_attachment_input(&input).expect("bound is valid"),
                mime::AttachmentRequest::Text { max_chars }
            );
        }
        for max_chars in [0, 64_001] {
            let input = GetAttachmentInput {
                max_chars: Some(max_chars),
                ..attachment_input(AttachmentContentMode::Text)
            };
            validate_attachment_input(&input).expect_err("out of range");
        }
    }

    #[test]
    fn attachment_bytes_mode_defaults_and_bounds() {
        assert_eq!(
            validate_attachment_input(&attachment_input(AttachmentContentMode::Bytes))
                .expect("defaults"),
            mime::AttachmentRequest::Bytes {
                offset_bytes: 0,
                max_bytes: 16_000
            }
        );
        let input = GetAttachmentInput {
            offset_bytes: Some(32_000),
            max_bytes: Some(64_000),
            ..attachment_input(AttachmentContentMode::Bytes)
        };
        assert_eq!(
            validate_attachment_input(&input).expect("valid page"),
            mime::AttachmentRequest::Bytes {
                offset_bytes: 32_000,
                max_bytes: 64_000
            }
        );
        for max_bytes in [0, 64_001] {
            let input = GetAttachmentInput {
                max_bytes: Some(max_bytes),
                ..attachment_input(AttachmentContentMode::Bytes)
            };
            validate_attachment_input(&input).expect_err("out of range");
        }
    }

    #[test]
    fn attachment_mode_inapplicable_fields_are_rejected() {
        let text_with_bytes_field = [
            GetAttachmentInput {
                offset_bytes: Some(0),
                ..attachment_input(AttachmentContentMode::Text)
            },
            GetAttachmentInput {
                max_bytes: Some(10),
                ..attachment_input(AttachmentContentMode::Text)
            },
        ];
        for input in text_with_bytes_field {
            let error = validate_attachment_input(&input).expect_err("bytes field in text mode");
            assert!(matches!(error, AppError::InvalidInput(_)));
        }
        let bytes_with_text_field = GetAttachmentInput {
            max_chars: Some(10),
            ..attachment_input(AttachmentContentMode::Bytes)
        };
        let error = validate_attachment_input(&bytes_with_text_field)
            .expect_err("text field in bytes mode");
        assert!(matches!(error, AppError::InvalidInput(_)));
    }

    #[test]
    fn attachment_part_id_length_is_bounded() {
        let too_long = GetAttachmentInput {
            part_id: format!("1{}", ".1".repeat(32)),
            ..attachment_input(AttachmentContentMode::Text)
        };
        validate_attachment_input(&too_long).expect_err("65 characters");
        let longest = GetAttachmentInput {
            part_id: format!("1{}", ".1".repeat(31)),
            ..attachment_input(AttachmentContentMode::Text)
        };
        validate_attachment_input(&longest).expect("63 characters");
        let empty = GetAttachmentInput {
            part_id: String::new(),
            ..attachment_input(AttachmentContentMode::Text)
        };
        validate_attachment_input(&empty).expect_err("empty");
    }

    /// A malformed part ID is rejected before any IMAP traffic happens.
    #[test]
    fn attachment_part_id_grammar_is_validated_before_network_use() {
        for part_id in [
            "1.0",
            "a",
            "1..2",
            "1.",
            ".1",
            "0",
            "2",
            "1.-1",
            "1.99999999999999999999",
        ] {
            let input = GetAttachmentInput {
                part_id: part_id.to_owned(),
                ..attachment_input(AttachmentContentMode::Text)
            };
            validate_attachment_input(&input).expect_err(part_id);
        }
        let nested = GetAttachmentInput {
            part_id: "1.2.3".to_owned(),
            ..attachment_input(AttachmentContentMode::Text)
        };
        validate_attachment_input(&nested).expect("nested part id");
    }
}
