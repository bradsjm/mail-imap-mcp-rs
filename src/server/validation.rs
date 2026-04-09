use std::collections::HashSet;

use chrono::{Duration as ChronoDuration, NaiveDate, Utc};

use crate::config::ServerConfig;
use crate::errors::{AppError, AppResult};
use crate::message_id::MessageId;
use crate::models::{
    ApplyToMessagesInput, ManageMailboxInput, SearchMessagesInput, UpdateMessageFlagsInput,
};

use super::types::{FlagOperation, FlagUpdateRequest, MailboxAction, MessageActionInput};
use super::{MAX_BULK_MESSAGE_IDS, VALID_SYSTEM_FLAGS};

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

pub(super) fn build_flag_update_request(
    input: &UpdateMessageFlagsInput,
) -> AppResult<FlagUpdateRequest> {
    let operation = match input.operation.as_str() {
        "add" => FlagOperation::Add,
        "remove" => FlagOperation::Remove,
        "replace" => FlagOperation::Replace,
        _ => {
            return Err(AppError::InvalidInput(format!(
                "operation must be one of add, remove, replace; got '{}'",
                input.operation
            )));
        }
    };
    Ok(FlagUpdateRequest {
        operation,
        flags: input.flags.clone(),
    })
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

pub(super) fn validate_flag_update_request(request: &FlagUpdateRequest) -> AppResult<()> {
    if request.flags.is_empty() {
        return Err(AppError::InvalidInput(
            "flags must contain at least one entry".to_owned(),
        ));
    }
    validate_flags(&request.flags, "flags")
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

pub(super) fn validate_search_input(input: &SearchMessagesInput) -> AppResult<()> {
    validate_mailbox(&input.mailbox)?;
    validate_chars(input.limit, 1, 50, "limit")?;

    if input.cursor.is_some() {
        return Ok(());
    }

    if let Some(last_days) = input.last_days
        && !(1..=365).contains(&last_days)
    {
        return Err(AppError::InvalidInput(
            "last_days must be in range 1..365".to_owned(),
        ));
    }
    if let Some(snippet_max_chars) = input.snippet_max_chars {
        validate_chars(snippet_max_chars, 50, 500, "snippet_max_chars")?;
    }

    for text in [&input.query, &input.from, &input.to, &input.subject]
        .into_iter()
        .flatten()
    {
        validate_search_text(text)?;
    }

    if input.last_days.is_some() && (input.start_date.is_some() || input.end_date.is_some()) {
        return Err(AppError::InvalidInput(
            "last_days cannot be combined with start_date/end_date".to_owned(),
        ));
    }

    if let (Some(start), Some(end)) = (&input.start_date, &input.end_date) {
        let start_date = parse_ymd(start)?;
        let end_date = parse_ymd(end)?;
        if start_date > end_date {
            return Err(AppError::InvalidInput(
                "start_date must be <= end_date".to_owned(),
            ));
        }
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

pub(super) fn build_search_query(input: &SearchMessagesInput) -> AppResult<String> {
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

    if parts.is_empty() {
        Ok("ALL".to_owned())
    } else {
        Ok(parts.join(" "))
    }
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
        FlagOperation, FlagUpdateRequest, build_flag_update_request, build_mailbox_action,
        build_message_action, dedupe_and_parse_message_ids, escape_imap_quoted,
        parse_bulk_message_ids, validate_flag, validate_flag_update_request, validate_mailbox,
        validate_search_input, validate_search_text,
    };
    use crate::models::{
        ApplyToMessagesInput, ManageMailboxInput, SearchMessagesInput, UpdateMessageFlagsInput,
    };

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

    #[test]
    fn validate_flag_update_request_lists_valid_standard_flags() {
        let err = validate_flag_update_request(&FlagUpdateRequest {
            operation: FlagOperation::Add,
            flags: vec!["\\Read".to_owned()],
        })
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
    fn validate_flag_update_request_rejects_empty_flags() {
        let err = validate_flag_update_request(&FlagUpdateRequest {
            operation: FlagOperation::Add,
            flags: Vec::new(),
        })
        .expect_err("must reject empty flags");
        assert!(
            err.to_string()
                .contains("flags must contain at least one entry")
        );
    }

    #[test]
    fn build_flag_update_request_rejects_unknown_operation() {
        let input = UpdateMessageFlagsInput {
            message_ids: vec!["imap:default:INBOX:42:7".to_owned()],
            operation: "merge".to_owned(),
            flags: vec!["\\Seen".to_owned()],
        };

        let err = build_flag_update_request(&input).expect_err("must reject unknown operation");
        assert!(
            err.to_string()
                .contains("operation must be one of add, remove, replace")
        );
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

    #[test]
    fn validate_search_input_allows_replayed_criteria_when_cursor_present() {
        let input = SearchMessagesInput {
            account_id: "default".to_owned(),
            mailbox: "Donations".to_owned(),
            cursor: Some("cursor-id".to_owned()),
            query: Some(".*".to_owned()),
            from: Some(".*".to_owned()),
            to: Some(".*".to_owned()),
            subject: Some(".*".to_owned()),
            unread_only: Some(false),
            last_days: Some(365),
            start_date: Some("2025-01-01".to_owned()),
            end_date: Some("2025-12-31".to_owned()),
            limit: 50,
            snippet_max_chars: Some(200),
        };

        validate_search_input(&input).expect("cursor mode should ignore replayed criteria");
    }

    #[test]
    fn validate_search_input_still_rejects_conflicting_dates_without_cursor() {
        let input = SearchMessagesInput {
            account_id: "default".to_owned(),
            mailbox: "Donations".to_owned(),
            cursor: None,
            query: None,
            from: None,
            to: None,
            subject: None,
            unread_only: None,
            last_days: Some(30),
            start_date: Some("2025-01-01".to_owned()),
            end_date: Some("2025-12-31".to_owned()),
            limit: 50,
            snippet_max_chars: None,
        };

        let err = validate_search_input(&input).expect_err("must reject conflicting date filters");
        assert!(
            err.to_string()
                .contains("last_days cannot be combined with start_date/end_date")
        );
    }

    #[test]
    fn validate_search_input_accepts_snippet_size_without_boolean_toggle() {
        let input = SearchMessagesInput {
            account_id: "default".to_owned(),
            mailbox: "Donations".to_owned(),
            cursor: None,
            query: None,
            from: None,
            to: None,
            subject: None,
            unread_only: None,
            last_days: None,
            start_date: None,
            end_date: None,
            limit: 50,
            snippet_max_chars: Some(200),
        };

        validate_search_input(&input).expect("snippet_max_chars alone should enable snippets");
    }
}
