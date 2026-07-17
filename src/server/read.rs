use base64::Engine;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, Semaphore};

use super::provider::{is_gmail_capable, mailbox_info};
use crate::errors::{AppError, AppResult};
use crate::imap;
use crate::mailbox_codec::normalize_mailbox_name;
use crate::message_id::MessageId;
use crate::mime;
use crate::models::{
    AttachmentInfo, GetMessageInput, GetMessageRawInput, ListMailboxesInput, MailboxInfo,
    MessageDetail, MessageSummary, SearchMessagesInput,
};
use crate::pagination::{
    CursorEntry, CursorStore, MailboxCursorClaimError, MailboxCursorEntry, MailboxCursorPage,
    MailboxCursorStore,
};

use super::session_cache::ReadSessionLease;
use super::types::{
    GetMessageData, GetMessageRawData, ListMailboxesData, SearchResultData, SummaryBuildResult,
    ToolIssue, build_message_raw_uri, build_message_uri, is_hard_precondition_error,
    log_runtime_issues, next_action_continue_list_mailboxes, next_action_for_search_result,
    next_action_list_accounts, next_action_list_mailboxes, next_action_search_mailbox,
    preferred_mailbox_name, status_from_counts, status_from_issue_and_counts,
};
use super::validation::{
    build_search_query, header_value, parse_and_validate_message_id, validate_account_id,
    validate_chars, validate_list_mailboxes_input, validate_mailbox, validate_search_input,
};
use super::{MAX_CURSOR_UIDS_STORED, MAX_SEARCH_LIMIT, MailImapServer};

struct SearchSnapshot {
    uids_desc: Arc<[u32]>,
    offset: usize,
    snippet_max_chars: Option<usize>,
    cursor_id_from_request: Option<String>,
}

struct SummaryBuildOptions<'a> {
    account_id: &'a str,
    mailbox: &'a str,
    uidvalidity: u32,
    snippet_max_chars: Option<usize>,
}

fn normalize_attachment_sizes_for_raw_completeness(
    attachments: &mut [AttachmentInfo],
    raw_complete: bool,
) {
    if !raw_complete {
        for attachment in attachments {
            attachment.size_bytes = None;
        }
    }
}
fn attachment_mode_for_raw_completeness(
    requested: crate::models::AttachmentMode,
    raw_complete: bool,
) -> crate::models::AttachmentMode {
    if !raw_complete && requested == crate::models::AttachmentMode::ExtractText {
        crate::models::AttachmentMode::Metadata
    } else {
        requested
    }
}

fn should_report_attachment_limit(
    attachments_truncated: bool,
    processing_limits: &[mime::ProcessingLimit],
) -> bool {
    attachments_truncated
        && !processing_limits.iter().any(|limit| {
            matches!(
                limit,
                mime::ProcessingLimit::MaxDepth | mime::ProcessingLimit::MaxParts
            )
        })
}

fn processing_limit_issue(limit: mime::ProcessingLimit, uid: u32, message_id: &str) -> ToolIssue {
    let (stage, message) = match limit {
        mime::ProcessingLimit::DecodeBudgetBytes => (
            "decode_budget",
            "message decode budget was reached; returned content is partial",
        ),
        mime::ProcessingLimit::MaxDepth => (
            "mime_depth",
            "MIME nesting depth limit was reached; returned content is partial",
        ),
        mime::ProcessingLimit::MaxParts => (
            "mime_parts",
            "MIME part limit was reached; returned content is partial",
        ),
        mime::ProcessingLimit::AttachmentExtractBudgetBytes => (
            "attachment_extract_budget",
            "attachment extraction budget was reached; returned attachment text is partial",
        ),
    };
    ToolIssue {
        code: "limit_exceeded".to_owned(),
        stage: stage.to_owned(),
        message: message.to_owned(),
        retryable: false,
        uid: Some(uid),
        message_id: Some(message_id.to_owned()),
    }
}

async fn run_bounded_mime_parse<T>(
    semaphore: Arc<Semaphore>,
    parse: impl FnOnce() -> AppResult<T> + Send + 'static,
) -> AppResult<T>
where
    T: Send + 'static,
{
    let permit = semaphore
        .acquire_owned()
        .await
        .map_err(|_| AppError::Internal("MIME parse executor is shutting down".to_owned()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        parse()
    })
    .await
    .map_err(|error| AppError::Internal(format!("MIME parse worker failed: {error}")))?
}

fn fetch_budget_issue(uid: u32, message_id: &str, message: &str) -> ToolIssue {
    ToolIssue {
        code: "limit_exceeded".to_owned(),
        stage: "fetch_budget".to_owned(),
        message: message.to_owned(),
        retryable: false,
        uid: Some(uid),
        message_id: Some(message_id.to_owned()),
    }
}

impl MailImapServer {
    pub(super) async fn list_mailboxes_impl(
        &self,
        input: ListMailboxesInput,
    ) -> AppResult<ListMailboxesData> {
        validate_list_mailboxes_input(&input)?;

        if let Some(cursor_id) = input.cursor.clone() {
            return list_mailboxes_from_cursor(&self.mailbox_cursors, &input, cursor_id).await;
        }

        let mut issues = Vec::new();
        let mut session = match self.checkout_read_session(&input.account_id).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(ToolIssue::from_error("connect_authenticated", &error));
                log_runtime_issues(
                    "imap_list_mailboxes",
                    "failed",
                    &input.account_id,
                    None,
                    &issues,
                );
                return Ok(failed_list_mailboxes_data(&input.account_id, issues));
            }
        };

        let capabilities = match imap::capabilities(&self.config, session.session()).await {
            Ok(capabilities) => capabilities,
            Err(error) => {
                issues.push(ToolIssue::from_error("capabilities", &error));
                let _ = release_read_session(self, session, false).await;
                log_runtime_issues(
                    "imap_list_mailboxes",
                    "failed",
                    &input.account_id,
                    None,
                    &issues,
                );
                return Ok(failed_list_mailboxes_data(&input.account_id, issues));
            }
        };
        let gmail_capable = is_gmail_capable(&capabilities);

        let observation = match imap::list_all_mailboxes(&self.config, session.session()).await {
            Ok(observation) => observation,
            Err(error) => {
                issues.push(ToolIssue::from_error("list_mailboxes", &error));
                let _ = release_read_session(self, session, false).await;
                log_runtime_issues(
                    "imap_list_mailboxes",
                    "failed",
                    &input.account_id,
                    None,
                    &issues,
                );
                return Ok(failed_list_mailboxes_data(&input.account_id, issues));
            }
        };

        let truncated = observation.truncated;
        let mailboxes = observation
            .items
            .into_iter()
            .map(|item| mailbox_info(&item, gmail_capable))
            .collect::<Vec<_>>();
        if truncated {
            session.discard();
        } else {
            let _ = release_read_session(self, session, true).await;
        }

        let total = (!truncated).then_some(mailboxes.len());
        list_mailboxes_from_snapshot(
            &self.mailbox_cursors,
            &input,
            mailboxes.into(),
            total,
            truncated,
        )
        .await
    }

    pub(super) async fn search_messages_impl(
        &self,
        input: SearchMessagesInput,
    ) -> AppResult<SearchResultData> {
        validate_search_input(&input)?;
        validate_account_id(&input.account_id)?;
        validate_mailbox(&input.mailbox)?;

        let mut session = match self.checkout_read_session(&input.account_id).await {
            Ok(session) => session,
            Err(error) => {
                let issues = vec![ToolIssue::from_error("connect_authenticated", &error)];
                log_runtime_issues(
                    "imap_search_messages",
                    "failed",
                    &input.account_id,
                    Some(&input.mailbox),
                    &issues,
                );
                return Ok(SearchResultData {
                    status: "failed".to_owned(),
                    issues,
                    next_action: next_action_list_mailboxes(&input.account_id),
                    account_id: input.account_id,
                    mailbox: input.mailbox,
                    total: 0,
                    attempted: 0,
                    returned: 0,
                    failed: 0,
                    messages: Vec::new(),
                    next_cursor: None,
                    has_more: false,
                });
            }
        };

        let uidvalidity =
            match imap::select_mailbox_readonly(&self.config, session.session(), &input.mailbox)
                .await
            {
                Ok(uidvalidity) => uidvalidity,
                Err(error) => {
                    let issues = vec![ToolIssue::from_error("select_mailbox_readonly", &error)];
                    let _ = release_read_session(self, session, false).await;
                    log_runtime_issues(
                        "imap_search_messages",
                        "failed",
                        &input.account_id,
                        Some(&input.mailbox),
                        &issues,
                    );
                    return Ok(SearchResultData {
                        status: "failed".to_owned(),
                        issues,
                        next_action: next_action_list_mailboxes(&input.account_id),
                        account_id: input.account_id,
                        mailbox: input.mailbox,
                        total: 0,
                        attempted: 0,
                        returned: 0,
                        failed: 0,
                        messages: Vec::new(),
                        next_cursor: None,
                        has_more: false,
                    });
                }
            };

        let snapshot = if let Some(cursor) = input.cursor.clone() {
            match resume_cursor_search(&self.cursors, &input, uidvalidity, cursor).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    let _ = release_read_session(self, session, true).await;
                    return Err(error);
                }
            }
        } else {
            match start_new_search(&self.config, session.session(), &input).await {
                Ok(snapshot) => snapshot,
                Err(error) if is_hard_precondition_error(&error) => {
                    let _ = release_read_session(self, session, false).await;
                    return Err(error);
                }
                Err(error) => {
                    let issues = vec![ToolIssue::from_error("uid_search", &error)];
                    let _ = release_read_session(self, session, false).await;
                    log_runtime_issues(
                        "imap_search_messages",
                        "failed",
                        &input.account_id,
                        Some(&input.mailbox),
                        &issues,
                    );
                    return Ok(SearchResultData {
                        status: "failed".to_owned(),
                        issues,
                        next_action: next_action_list_mailboxes(&input.account_id),
                        account_id: input.account_id,
                        mailbox: input.mailbox,
                        total: 0,
                        attempted: 0,
                        returned: 0,
                        failed: 0,
                        messages: Vec::new(),
                        next_cursor: None,
                        has_more: false,
                    });
                }
            }
        };

        let SearchSnapshot {
            uids_desc,
            offset,
            snippet_max_chars,
            cursor_id_from_request,
        } = snapshot;

        let total = uids_desc.len();
        if offset > total {
            let _ = release_read_session(self, session, true).await;
            return Err(AppError::InvalidInput(
                "cursor offset is out of range".to_owned(),
            ));
        }

        let limit = input.limit.clamp(1, MAX_SEARCH_LIMIT);
        let page_uids = uids_desc
            .iter()
            .skip(offset)
            .take(limit)
            .copied()
            .collect::<Vec<_>>();

        let SummaryBuildResult {
            messages,
            issues,
            attempted,
            failed,
        } = build_message_summaries(
            &self.config,
            session.session(),
            &page_uids,
            SummaryBuildOptions {
                account_id: &input.account_id,
                mailbox: &input.mailbox,
                uidvalidity,
                snippet_max_chars,
            },
        )
        .await;

        let next_offset = offset + page_uids.len();
        let has_more = next_offset < total;
        let next_cursor = if has_more {
            let mut store = self.cursors.lock().await;
            if let Some(existing) = cursor_id_from_request {
                store.update_offset(&existing, next_offset);
                Some(existing)
            } else {
                let id = store.create(CursorEntry {
                    account_id: input.account_id.clone(),
                    mailbox: input.mailbox.clone(),
                    uidvalidity,
                    uids_desc,
                    offset: next_offset,
                    snippet_max_chars,
                    expires_at: Instant::now(),
                });
                Some(id)
            }
        } else {
            if let Some(existing) = cursor_id_from_request {
                let mut store = self.cursors.lock().await;
                store.delete(&existing);
            }
            None
        };

        let status = status_from_issue_and_counts(&issues, !messages.is_empty()).to_owned();
        log_runtime_issues(
            "imap_search_messages",
            &status,
            &input.account_id,
            Some(&input.mailbox),
            &issues,
        );
        let next_action = next_action_for_search_result(
            &status,
            &input.account_id,
            &input.mailbox,
            input.limit,
            next_cursor.as_deref(),
            &messages,
        );
        let reusable = issues.is_empty() || failed < attempted;
        let _ = release_read_session(self, session, reusable).await;

        Ok(SearchResultData {
            status,
            issues,
            next_action,
            account_id: input.account_id,
            mailbox: input.mailbox,
            total,
            attempted,
            returned: messages.len(),
            failed,
            messages,
            next_cursor: next_cursor.clone(),
            has_more: next_cursor.is_some(),
        })
    }

    pub(super) async fn get_message_impl(
        &self,
        input: GetMessageInput,
    ) -> AppResult<GetMessageData> {
        validate_chars(input.body_max_chars, 1, 16_000, "body_max_chars")?;
        let attachment_text_max_chars = input.attachment_text_max_chars.unwrap_or(10_000);
        if input.attachment_text_max_chars.is_some()
            && input.attachment_mode != crate::models::AttachmentMode::ExtractText
        {
            return Err(AppError::InvalidInput(
                "attachment_text_max_chars requires attachment_mode=extract_text".to_owned(),
            ));
        }
        validate_chars(
            attachment_text_max_chars,
            1,
            64_000,
            "attachment_text_max_chars",
        )?;

        let message_id = parse_and_validate_message_id(&input.message_id)?;
        let encoded_message_id = message_id.encode();
        let mut issues = Vec::new();

        let mut session = match self.checkout_read_session(&message_id.account_id).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("connect_authenticated", &error)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_get_message",
                    "failed",
                    &message_id.account_id,
                    Some(&message_id.mailbox),
                    &issues,
                );
                return Ok(GetMessageData {
                    status: "failed".to_owned(),
                    issues,
                    account_id: message_id.account_id.clone(),
                    message: None,
                });
            }
        };
        if let Err(error) =
            ensure_uidvalidity_matches_readonly(&self.config, session.session(), &message_id).await
        {
            let _ = release_read_session(self, session, false).await;
            return Err(error);
        }

        let declared_size =
            match imap::fetch_message_size(&self.config, session.session(), message_id.uid).await {
                Ok(size) => size,
                Err(error) => {
                    issues.push(
                        ToolIssue::from_error("fetch_message_size", &error)
                            .with_uid(message_id.uid)
                            .with_message_id(&encoded_message_id),
                    );
                    let _ = release_read_session(self, session, false).await;
                    log_runtime_issues(
                        "imap_get_message",
                        "failed",
                        &message_id.account_id,
                        Some(&message_id.mailbox),
                        &issues,
                    );
                    return Ok(GetMessageData {
                        status: "failed".to_owned(),
                        issues,
                        account_id: message_id.account_id.clone(),
                        message: None,
                    });
                }
            };

        let fetch_budget = self.config.message_fetch_budget_bytes;
        if fetch_budget == 0 {
            issues.push(fetch_budget_issue(
                message_id.uid,
                &encoded_message_id,
                "message fetch budget is zero; content was omitted",
            ));
            let _ = release_read_session(self, session, true).await;
            log_runtime_issues(
                "imap_get_message",
                "failed",
                &message_id.account_id,
                Some(&message_id.mailbox),
                &issues,
            );
            return Ok(GetMessageData {
                status: "failed".to_owned(),
                issues,
                account_id: message_id.account_id.clone(),
                message: None,
            });
        }

        let raw = match imap::fetch_raw_message_range(
            &self.config,
            session.session(),
            message_id.uid,
            0,
            fetch_budget,
        )
        .await
        {
            Ok(raw) => raw,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("fetch_raw_message_range", &error)
                        .with_uid(message_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                let _ = release_read_session(self, session, false).await;
                log_runtime_issues(
                    "imap_get_message",
                    "failed",
                    &message_id.account_id,
                    Some(&message_id.mailbox),
                    &issues,
                );
                return Ok(GetMessageData {
                    status: "failed".to_owned(),
                    issues,
                    account_id: message_id.account_id.clone(),
                    message: None,
                });
            }
        };
        let raw_complete = raw.len() >= declared_size;
        if !raw_complete {
            issues.push(fetch_budget_issue(
                message_id.uid,
                &encoded_message_id,
                "raw message exceeded the fetch budget; returned content is partial",
            ));
        }

        let limits = mime::ProcessingLimits {
            decode_budget_bytes: self.config.message_decode_budget_bytes,
            max_depth: self.config.mime_max_depth,
            max_parts: self.config.mime_max_parts,
            attachment_extract_budget_bytes: self.config.attachment_extract_budget_bytes,
            attachment_text_max_chars,
        };
        let body_max_chars = input.body_max_chars;
        let body_mode = input.body_mode;
        let attachment_mode =
            attachment_mode_for_raw_completeness(input.attachment_mode, raw_complete);
        let parsed = run_bounded_mime_parse(self.mime_parse_semaphore.clone(), move || {
            mime::parse_message(&raw, body_max_chars, body_mode, attachment_mode, &limits)
        })
        .await;

        let mut parsed = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("parse_message", &error)
                        .with_uid(message_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                let _ = release_read_session(self, session, true).await;
                log_runtime_issues(
                    "imap_get_message",
                    "failed",
                    &message_id.account_id,
                    Some(&message_id.mailbox),
                    &issues,
                );
                return Ok(GetMessageData {
                    status: "failed".to_owned(),
                    issues,
                    account_id: message_id.account_id.clone(),
                    message: None,
                });
            }
        };
        normalize_attachment_sizes_for_raw_completeness(&mut parsed.attachments, raw_complete);
        for limit in parsed.processing_limits.iter().copied() {
            issues.push(processing_limit_issue(
                limit,
                message_id.uid,
                &encoded_message_id,
            ));
        }

        if should_report_attachment_limit(parsed.attachments_truncated, &parsed.processing_limits) {
            issues.push(ToolIssue {
                code: "limit_exceeded".to_owned(),
                stage: "attachment_limit".to_owned(),
                message: format!(
                    "message has more than {} attachments; only the first {} are returned",
                    mime::MAX_ATTACHMENTS,
                    mime::MAX_ATTACHMENTS
                ),
                retryable: false,
                uid: Some(message_id.uid),
                message_id: Some(encoded_message_id.clone()),
            });
        }

        let headers = if input.include_headers || input.include_all_headers {
            Some(mime::curated_headers(
                &parsed.headers_all,
                input.include_all_headers,
            ))
        } else {
            None
        };

        let flags = match imap::fetch_flags(&self.config, session.session(), message_id.uid).await {
            Ok(flags) => Some(flags),
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("fetch_flags", &error)
                        .with_uid(message_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                None
            }
        };

        let detail = MessageDetail {
            message_id: encoded_message_id.clone(),
            message_uri: build_message_uri(
                &message_id.account_id,
                &message_id.mailbox,
                message_id.uidvalidity,
                message_id.uid,
            ),
            message_raw_uri: build_message_raw_uri(
                &message_id.account_id,
                &message_id.mailbox,
                message_id.uidvalidity,
                message_id.uid,
            ),
            mailbox: message_id.mailbox.clone(),
            uidvalidity: message_id.uidvalidity,
            uid: message_id.uid,
            date: parsed.date,
            from: parsed.from,
            to: parsed.to,
            cc: parsed.cc,
            subject: parsed.subject,
            flags,
            headers,
            body_text: parsed.body_text,
            body_html: parsed.body_html_sanitized,
            attachments: Some(parsed.attachments),
        };

        let status = status_from_issue_and_counts(&issues, true);
        log_runtime_issues(
            "imap_get_message",
            status,
            &message_id.account_id,
            Some(&message_id.mailbox),
            &issues,
        );
        let reusable = issues.is_empty();
        let _ = release_read_session(self, session, reusable).await;

        Ok(GetMessageData {
            status: status.to_owned(),
            issues,
            account_id: message_id.account_id.clone(),
            message: Some(detail),
        })
    }

    pub(super) async fn get_message_raw_impl(
        &self,
        input: GetMessageRawInput,
    ) -> AppResult<GetMessageRawData> {
        validate_chars(input.max_bytes, 1, 64_000, "max_bytes")?;

        let message_id = parse_and_validate_message_id(&input.message_id)?;
        let encoded_message_id = message_id.encode();
        let mut issues = Vec::new();

        let mut session = match self.checkout_read_session(&message_id.account_id).await {
            Ok(session) => session,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("connect_authenticated", &error)
                        .with_message_id(&encoded_message_id),
                );
                log_runtime_issues(
                    "imap_get_message_raw",
                    "failed",
                    &message_id.account_id,
                    Some(&message_id.mailbox),
                    &issues,
                );
                return Ok(GetMessageRawData {
                    status: "failed".to_owned(),
                    issues,
                    account_id: message_id.account_id.clone(),
                    message_id: encoded_message_id.clone(),
                    message_uri: build_message_uri(
                        &message_id.account_id,
                        &message_id.mailbox,
                        message_id.uidvalidity,
                        message_id.uid,
                    ),
                    message_raw_uri: build_message_raw_uri(
                        &message_id.account_id,
                        &message_id.mailbox,
                        message_id.uidvalidity,
                        message_id.uid,
                    ),
                    total_size_bytes: 0,
                    returned_bytes: 0,
                    offset_bytes: input.offset_bytes,
                    truncated: false,
                    raw_source_base64: None,
                    raw_source_encoding: None,
                });
            }
        };
        if let Err(error) =
            ensure_uidvalidity_matches_readonly(&self.config, session.session(), &message_id).await
        {
            let _ = release_read_session(self, session, false).await;
            return Err(error);
        }

        let total_size_bytes =
            match imap::fetch_message_size(&self.config, session.session(), message_id.uid).await {
                Ok(size) => size,
                Err(error) => {
                    issues.push(
                        ToolIssue::from_error("fetch_message_size", &error)
                            .with_uid(message_id.uid)
                            .with_message_id(&encoded_message_id),
                    );
                    let _ = release_read_session(self, session, false).await;
                    log_runtime_issues(
                        "imap_get_message_raw",
                        "failed",
                        &message_id.account_id,
                        Some(&message_id.mailbox),
                        &issues,
                    );
                    return Ok(GetMessageRawData {
                        status: "failed".to_owned(),
                        issues,
                        account_id: message_id.account_id.clone(),
                        message_id: encoded_message_id.clone(),
                        message_uri: build_message_uri(
                            &message_id.account_id,
                            &message_id.mailbox,
                            message_id.uidvalidity,
                            message_id.uid,
                        ),
                        message_raw_uri: build_message_raw_uri(
                            &message_id.account_id,
                            &message_id.mailbox,
                            message_id.uidvalidity,
                            message_id.uid,
                        ),
                        total_size_bytes: 0,
                        returned_bytes: 0,
                        offset_bytes: input.offset_bytes,
                        truncated: false,
                        raw_source_base64: None,
                        raw_source_encoding: None,
                    });
                }
            };
        if input.offset_bytes > total_size_bytes {
            let _ = release_read_session(self, session, true).await;
            return Err(AppError::InvalidInput(
                "offset_bytes must not exceed total message size".to_owned(),
            ));
        }

        let raw = match imap::fetch_raw_message_range(
            &self.config,
            session.session(),
            message_id.uid,
            input.offset_bytes,
            input.max_bytes,
        )
        .await
        {
            Ok(raw) => raw,
            Err(error) => {
                issues.push(
                    ToolIssue::from_error("fetch_raw_message_range", &error)
                        .with_uid(message_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                let _ = release_read_session(self, session, false).await;
                log_runtime_issues(
                    "imap_get_message_raw",
                    "failed",
                    &message_id.account_id,
                    Some(&message_id.mailbox),
                    &issues,
                );
                return Ok(GetMessageRawData {
                    status: "failed".to_owned(),
                    issues,
                    account_id: message_id.account_id.clone(),
                    message_id: encoded_message_id.clone(),
                    message_uri: build_message_uri(
                        &message_id.account_id,
                        &message_id.mailbox,
                        message_id.uidvalidity,
                        message_id.uid,
                    ),
                    message_raw_uri: build_message_raw_uri(
                        &message_id.account_id,
                        &message_id.mailbox,
                        message_id.uidvalidity,
                        message_id.uid,
                    ),
                    total_size_bytes,
                    returned_bytes: 0,
                    offset_bytes: input.offset_bytes,
                    truncated: false,
                    raw_source_base64: None,
                    raw_source_encoding: None,
                });
            }
        };
        let total_size_bytes = total_size_bytes.max(input.offset_bytes.saturating_add(raw.len()));
        let truncated = input.offset_bytes.saturating_add(raw.len()) < total_size_bytes;

        log_runtime_issues(
            "imap_get_message_raw",
            "ok",
            &message_id.account_id,
            Some(&message_id.mailbox),
            &issues,
        );
        let _ = release_read_session(self, session, issues.is_empty()).await;

        Ok(GetMessageRawData {
            status: "ok".to_owned(),
            issues,
            account_id: message_id.account_id.clone(),
            message_id: encoded_message_id,
            message_uri: build_message_uri(
                &message_id.account_id,
                &message_id.mailbox,
                message_id.uidvalidity,
                message_id.uid,
            ),
            message_raw_uri: build_message_raw_uri(
                &message_id.account_id,
                &message_id.mailbox,
                message_id.uidvalidity,
                message_id.uid,
            ),
            total_size_bytes,
            returned_bytes: raw.len(),
            offset_bytes: input.offset_bytes,
            truncated,
            raw_source_base64: Some(base64::engine::general_purpose::STANDARD.encode(raw)),
            raw_source_encoding: Some("base64".to_owned()),
        })
    }
}

fn failed_list_mailboxes_data(account_id: &str, issues: Vec<ToolIssue>) -> ListMailboxesData {
    ListMailboxesData {
        status: "failed".to_owned(),
        issues,
        next_action: next_action_list_accounts(),
        account_id: account_id.to_owned(),
        mailboxes: Vec::new(),
        returned: 0,
        total: None,
        has_more: false,
        next_cursor: None,
        truncated: false,
    }
}

async fn list_mailboxes_from_cursor(
    cursors: &Arc<Mutex<MailboxCursorStore>>,
    input: &ListMailboxesInput,
    cursor_id: String,
) -> AppResult<ListMailboxesData> {
    let claimed = {
        let mut store = cursors.lock().await;
        store
            .claim_page(&cursor_id, &input.account_id, input.limit)
            .map_err(|error| {
                let message = match error {
                    MailboxCursorClaimError::MissingOrExpired => "cursor is invalid or expired",
                    MailboxCursorClaimError::AccountMismatch => "cursor does not match account",
                    MailboxCursorClaimError::OffsetOutOfRange => "cursor offset is out of range",
                };
                AppError::InvalidInput(message.to_owned())
            })?
    };

    let next_cursor = claimed.has_more.then_some(cursor_id);
    Ok(list_mailboxes_page_data(input, claimed, next_cursor))
}

async fn list_mailboxes_from_snapshot(
    cursors: &Arc<Mutex<MailboxCursorStore>>,
    input: &ListMailboxesInput,
    mailboxes: Arc<[MailboxInfo]>,
    total: Option<usize>,
    truncated: bool,
) -> AppResult<ListMailboxesData> {
    let end = input.limit.min(mailboxes.len());
    let page = mailboxes[..end].to_vec();
    let has_more = end < mailboxes.len();
    let next_cursor = if has_more {
        let mut store = cursors.lock().await;
        Some(store.create(MailboxCursorEntry {
            account_id: input.account_id.clone(),
            mailboxes,
            offset: end,
            total,
            truncated,
            expires_at: Instant::now(),
        }))
    } else {
        None
    };

    Ok(list_mailboxes_page_data(
        input,
        MailboxCursorPage {
            mailboxes: page,
            total,
            truncated,
            has_more,
        },
        next_cursor,
    ))
}

fn list_mailboxes_page_data(
    input: &ListMailboxesInput,
    page: MailboxCursorPage,
    next_cursor: Option<String>,
) -> ListMailboxesData {
    let next_action = next_cursor
        .as_deref()
        .map(|cursor| next_action_continue_list_mailboxes(&input.account_id, cursor, input.limit))
        .or_else(|| {
            preferred_mailbox_name(&page.mailboxes)
                .map(|mailbox| next_action_search_mailbox(&input.account_id, &mailbox))
        })
        .unwrap_or_else(next_action_list_accounts);

    ListMailboxesData {
        status: status_from_counts(true, !page.mailboxes.is_empty()).to_owned(),
        issues: Vec::new(),
        next_action,
        account_id: input.account_id.clone(),
        returned: page.mailboxes.len(),
        total: page.total,
        has_more: page.has_more,
        next_cursor,
        truncated: page.truncated,
        mailboxes: page.mailboxes,
    }
}

async fn release_read_session(
    server: &MailImapServer,
    session: ReadSessionLease,
    reusable: bool,
) -> AppResult<()> {
    if let Some(error) = session
        .finish(&server.config, &server.read_sessions, reusable)
        .await
    {
        return Err(error);
    }
    Ok(())
}

async fn ensure_uidvalidity_matches_readonly(
    config: &crate::config::ServerConfig,
    session: &mut imap::ImapSession,
    message_id: &MessageId,
) -> AppResult<()> {
    let current_uidvalidity =
        imap::select_mailbox_readonly(config, session, &message_id.mailbox).await?;
    if current_uidvalidity != message_id.uidvalidity {
        return Err(AppError::Conflict(
            "message uidvalidity no longer matches mailbox".to_owned(),
        ));
    }
    Ok(())
}

async fn resume_cursor_search(
    cursors: &Arc<Mutex<CursorStore>>,
    input: &SearchMessagesInput,
    uidvalidity: u32,
    cursor_id: String,
) -> AppResult<SearchSnapshot> {
    let mut store = cursors.lock().await;
    let entry = store
        .get(&cursor_id)
        .ok_or_else(|| AppError::InvalidInput("cursor is invalid or expired".to_owned()))?;
    if entry.account_id != input.account_id
        || normalize_mailbox_name(&entry.mailbox) != normalize_mailbox_name(&input.mailbox)
    {
        return Err(AppError::InvalidInput(
            "cursor does not match account/mailbox".to_owned(),
        ));
    }
    if entry.uidvalidity != uidvalidity {
        store.delete(&cursor_id);
        return Err(AppError::Conflict(
            "mailbox snapshot changed; rerun search".to_owned(),
        ));
    }
    Ok(SearchSnapshot {
        uids_desc: entry.uids_desc,
        offset: entry.offset,
        snippet_max_chars: entry.snippet_max_chars,
        cursor_id_from_request: Some(cursor_id),
    })
}

async fn start_new_search(
    config: &crate::config::ServerConfig,
    session: &mut imap::ImapSession,
    input: &SearchMessagesInput,
) -> AppResult<SearchSnapshot> {
    let query = build_search_query(input)?;
    let searched_uids = imap::uid_search(config, session, &query).await?;
    if searched_uids.len() > MAX_CURSOR_UIDS_STORED {
        return Err(AppError::InvalidInput(format!(
            "search matched {} messages; narrow filters to at most {} results",
            searched_uids.len(),
            MAX_CURSOR_UIDS_STORED
        )));
    }

    Ok(SearchSnapshot {
        uids_desc: Arc::<[u32]>::from(searched_uids),
        offset: 0,
        snippet_max_chars: input.snippet_max_chars.map(|value| value.clamp(50, 500)),
        cursor_id_from_request: None,
    })
}

async fn build_message_summaries(
    config: &crate::config::ServerConfig,
    session: &mut imap::ImapSession,
    uids: &[u32],
    options: SummaryBuildOptions<'_>,
) -> SummaryBuildResult {
    let mut messages = Vec::with_capacity(uids.len());
    let mut issues = Vec::new();
    let mut failed = 0usize;
    if uids.is_empty() {
        return SummaryBuildResult {
            messages,
            issues,
            attempted: 0,
            failed: 0,
        };
    }
    let uid_set = build_uid_set(uids);

    let fetched = match imap::fetch_headers_and_flags_by_uid_set(config, session, &uid_set).await {
        Ok(fetched) => fetched,
        Err(error) => {
            failed = uids.len();
            issues.extend(uids.iter().map(|uid| {
                ToolIssue::from_error("fetch_headers_and_flags", &error).with_uid(*uid)
            }));
            return SummaryBuildResult {
                messages,
                issues,
                attempted: uids.len(),
                failed,
            };
        }
    };

    for uid in uids {
        let Some(fetched_message) = fetched.get(uid) else {
            failed += 1;
            issues.push(ToolIssue {
                code: "internal".to_owned(),
                stage: "fetch_headers_and_flags".to_owned(),
                message: format!("UID {uid} missing from batch fetch response"),
                retryable: true,
                uid: Some(*uid),
                message_id: None,
            });
            continue;
        };

        let headers = match mime::parse_header_bytes(&fetched_message.header_bytes) {
            Ok(headers) => headers,
            Err(error) => {
                failed += 1;
                issues.push(ToolIssue::from_error("parse_header_bytes", &error).with_uid(*uid));
                continue;
            }
        };

        let snippet = options.snippet_max_chars.and_then(|max_chars| {
            header_value(&headers, "subject").map(|s| mime::truncate_chars(s, max_chars))
        });

        let message_id = MessageId {
            account_id: options.account_id.to_owned(),
            mailbox: options.mailbox.to_owned(),
            uidvalidity: options.uidvalidity,
            uid: *uid,
        }
        .encode();

        messages.push(MessageSummary {
            message_id,
            message_uri: build_message_uri(
                options.account_id,
                options.mailbox,
                options.uidvalidity,
                *uid,
            ),
            message_raw_uri: build_message_raw_uri(
                options.account_id,
                options.mailbox,
                options.uidvalidity,
                *uid,
            ),
            mailbox: options.mailbox.to_owned(),
            uidvalidity: options.uidvalidity,
            uid: *uid,
            date: header_value(&headers, "date"),
            from: header_value(&headers, "from"),
            subject: header_value(&headers, "subject"),
            flags: Some(fetched_message.flags.clone()),
            snippet,
        });
    }

    SummaryBuildResult {
        messages,
        issues,
        attempted: uids.len(),
        failed,
    }
}

fn build_uid_set(uids: &[u32]) -> String {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    let mut ranges = Vec::new();
    let mut start = sorted[0];
    let mut end = sorted[0];
    for uid in sorted.into_iter().skip(1) {
        if uid == end + 1 {
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

fn format_uid_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}:{end}")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        attachment_mode_for_raw_completeness, fetch_budget_issue, list_mailboxes_from_cursor,
        list_mailboxes_from_snapshot, normalize_attachment_sizes_for_raw_completeness,
        resume_cursor_search, should_report_attachment_limit, status_from_issue_and_counts,
    };
    use crate::models::{
        AttachmentInfo, AttachmentMode, ListMailboxesInput, MailboxInfo, MessageSummary,
        SearchMessagesInput,
    };
    use crate::pagination::{CursorEntry, CursorStore, MailboxCursorEntry, MailboxCursorStore};
    use crate::server::types::next_action_for_search_result;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::Mutex;

    #[test]
    fn incomplete_raw_prefix_yields_fetch_budget_issue_and_partial_status() {
        let issues = vec![fetch_budget_issue(
            7,
            "message-id",
            "raw message exceeded the fetch budget; returned content is partial",
        )];

        assert_eq!(issues[0].stage, "fetch_budget");
        assert_eq!(status_from_issue_and_counts(&issues, true), "partial");
    }

    #[test]
    fn incomplete_raw_prefix_downgrades_attachment_extraction() {
        assert_eq!(
            attachment_mode_for_raw_completeness(AttachmentMode::ExtractText, false),
            AttachmentMode::Metadata
        );
    }

    #[test]
    fn complete_raw_message_retains_attachment_extraction() {
        assert_eq!(
            attachment_mode_for_raw_completeness(AttachmentMode::ExtractText, true),
            AttachmentMode::ExtractText
        );
    }

    #[test]
    fn incomplete_raw_message_hides_attachment_sizes() {
        let attachment = || AttachmentInfo {
            filename: Some("document.pdf".to_owned()),
            content_type: "application/pdf".to_owned(),
            size_bytes: Some(42),
            part_id: "2".to_owned(),
            extracted_text: None,
        };

        let mut partial = vec![attachment()];
        normalize_attachment_sizes_for_raw_completeness(&mut partial, false);
        assert_eq!(partial[0].size_bytes, None);

        let mut complete = vec![attachment()];
        normalize_attachment_sizes_for_raw_completeness(&mut complete, true);
        assert_eq!(complete[0].size_bytes, Some(42));
    }

    #[test]
    fn structural_mime_overflow_does_not_report_attachment_count_limit() {
        assert!(!should_report_attachment_limit(
            true,
            &[crate::mime::ProcessingLimit::MaxDepth]
        ));
        assert!(!should_report_attachment_limit(
            true,
            &[crate::mime::ProcessingLimit::MaxParts]
        ));
        assert!(should_report_attachment_limit(true, &[]));
    }

    #[tokio::test]
    async fn resume_cursor_accepts_legacy_encoded_mailbox_with_decoded_input() {
        let cursors = Arc::new(Mutex::new(CursorStore::new(600, 8)));
        let cursor_id = {
            let mut store = cursors.lock().await;
            store.create(CursorEntry {
                account_id: "default".to_owned(),
                mailbox: "&ZeVnLIqe-".to_owned(),
                uidvalidity: 42,
                uids_desc: vec![10, 9].into(),
                offset: 1,
                snippet_max_chars: Some(200),
                expires_at: Instant::now(),
            })
        };

        let input = SearchMessagesInput {
            account_id: "default".to_owned(),
            mailbox: "日本語".to_owned(),
            cursor: Some(cursor_id.clone()),
            query: None,
            from: None,
            to: None,
            subject: None,
            unread_only: None,
            last_days: None,
            start_date: None,
            end_date: None,
            limit: 10,
            snippet_max_chars: None,
        };

        let snapshot = resume_cursor_search(&cursors, &input, 42, cursor_id)
            .await
            .expect("legacy encoded cursor should resume");
        assert_eq!(snapshot.offset, 1);
        assert_eq!(&*snapshot.uids_desc, &[10, 9]);
    }

    #[tokio::test]
    async fn resume_cursor_accepts_decoded_mailbox_with_legacy_encoded_input() {
        let cursors = Arc::new(Mutex::new(CursorStore::new(600, 8)));
        let cursor_id = {
            let mut store = cursors.lock().await;
            store.create(CursorEntry {
                account_id: "default".to_owned(),
                mailbox: "日本語".to_owned(),
                uidvalidity: 42,
                uids_desc: vec![10, 9].into(),
                offset: 1,
                snippet_max_chars: Some(120),
                expires_at: Instant::now(),
            })
        };

        let input = SearchMessagesInput {
            account_id: "default".to_owned(),
            mailbox: "&ZeVnLIqe-".to_owned(),
            cursor: Some(cursor_id.clone()),
            query: None,
            from: None,
            to: None,
            subject: None,
            unread_only: None,
            last_days: None,
            start_date: None,
            end_date: None,
            limit: 10,
            snippet_max_chars: None,
        };

        let snapshot = resume_cursor_search(&cursors, &input, 42, cursor_id)
            .await
            .expect("decoded cursor should resume from legacy encoded request");
        assert_eq!(snapshot.snippet_max_chars, Some(120));
    }

    #[test]
    fn search_result_next_action_for_get_message_omits_account_id() {
        let next_action = next_action_for_search_result(
            "ok",
            "default",
            "INBOX",
            10,
            None,
            &[MessageSummary {
                message_id: "imap:default:INBOX:42:7".to_owned(),
                message_uri: "imap://default/messages/1".to_owned(),
                message_raw_uri: "imap://default/messages/1/raw".to_owned(),
                mailbox: "INBOX".to_owned(),
                uidvalidity: 42,
                uid: 7,
                date: Some("2026-01-01T00:00:00Z".to_owned()),
                from: Some("sender@example.com".to_owned()),
                subject: Some("subject".to_owned()),
                flags: Some(vec!["\\Seen".to_owned()]),
                snippet: Some("snippet".to_owned()),
            }],
        );

        assert_eq!(next_action.tool, "imap_get_message");
        assert_eq!(
            next_action.arguments["message_id"],
            "imap:default:INBOX:42:7"
        );
        assert!(next_action.arguments.get("account_id").is_none());
    }

    fn mailbox(name: &str) -> MailboxInfo {
        MailboxInfo {
            name: name.to_owned(),
            delimiter: Some("/".to_owned()),
            attributes: Vec::new(),
            role: None,
            selectable: true,
            provider_managed: false,
        }
    }

    #[tokio::test]
    async fn mailbox_pages_use_stable_snapshot_boundaries() {
        let cursors = Arc::new(Mutex::new(MailboxCursorStore::new(600, 8)));
        let input = ListMailboxesInput {
            account_id: "default".to_owned(),
            cursor: None,
            limit: 2,
        };
        let first = list_mailboxes_from_snapshot(
            &cursors,
            &input,
            vec![mailbox("INBOX"), mailbox("Archive"), mailbox("Sent")].into(),
            Some(3),
            false,
        )
        .await
        .expect("first page");
        assert_eq!(first.returned, 2);
        assert_eq!(first.total, Some(3));
        assert!(first.has_more);
        let cursor = first.next_cursor.expect("first page has continuation");

        let second = list_mailboxes_from_cursor(
            &cursors,
            &ListMailboxesInput {
                account_id: "default".to_owned(),
                cursor: Some(cursor.clone()),
                limit: 2,
            },
            cursor,
        )
        .await
        .expect("second page");
        assert_eq!(
            second
                .mailboxes
                .iter()
                .map(|mailbox| mailbox.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Sent"]
        );
        assert!(!second.has_more);
        assert!(second.next_cursor.is_none());
    }

    #[tokio::test]
    async fn mailbox_cursor_rejects_account_mismatch() {
        let cursors = Arc::new(Mutex::new(MailboxCursorStore::new(600, 8)));
        let cursor = {
            let mut store = cursors.lock().await;
            store.create(MailboxCursorEntry {
                account_id: "default".to_owned(),
                mailboxes: vec![mailbox("INBOX")].into(),
                offset: 0,
                total: Some(1),
                truncated: false,
                expires_at: Instant::now(),
            })
        };

        let error = list_mailboxes_from_cursor(
            &cursors,
            &ListMailboxesInput {
                account_id: "other".to_owned(),
                cursor: Some(cursor.clone()),
                limit: 1,
            },
            cursor,
        )
        .await
        .expect_err("cursor must be bound to its account");
        assert!(error.to_string().contains("cursor does not match account"));
    }

    #[tokio::test]
    async fn truncated_mailbox_snapshot_omits_total() {
        let cursors = Arc::new(Mutex::new(MailboxCursorStore::new(600, 8)));
        let input = ListMailboxesInput {
            account_id: "default".to_owned(),
            cursor: None,
            limit: 2_000,
        };
        let result = list_mailboxes_from_snapshot(
            &cursors,
            &input,
            (0..2_000)
                .map(|index| mailbox(&format!("Mailbox-{index}")))
                .collect::<Vec<_>>()
                .into(),
            None,
            true,
        )
        .await
        .expect("bounded snapshot");
        assert_eq!(result.returned, 2_000);
        assert_eq!(result.total, None);
        assert!(result.truncated);
        assert!(!result.has_more);
        assert!(result.next_cursor.is_none());
    }
}
