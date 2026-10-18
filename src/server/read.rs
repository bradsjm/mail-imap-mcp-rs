use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Instant;

use async_imap::Session;
use base64::Engine;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, Semaphore};

use super::provider::{is_gmail_capable, mailbox_info};
use crate::config::ServerConfig;
use crate::errors::{AppError, AppResult};
use crate::imap;
use crate::mailbox_codec::normalize_mailbox_name;
use crate::message_id::MessageId;
use crate::mime;
use crate::models::{
    AttachmentContent, AttachmentInfo, AttachmentResult, GetAttachmentInput, GetMessageInput,
    GetMessageRawInput, ListMailboxesInput, MailboxInfo, MessageDetail, MessageSummary,
    SearchMessagesInput, ThreadMethod,
};
use crate::pagination::{
    CursorEntry, CursorStore, MailboxCursorClaimError, MailboxCursorEntry, MailboxCursorPage,
    MailboxCursorStore, SearchCursorClaimError, SearchCursorGroup, SearchCursorPage,
};

use super::session_cache::ReadSessionLease;
use super::types::{
    GetAttachmentData, GetMessageData, GetMessageRawData, ListMailboxesData, NextAction,
    SearchResultData, SearchThreadQuery, SummaryBuildResult, ToolIssue, build_message_raw_uri,
    build_message_uri, is_hard_precondition_error, log_runtime_issues,
    next_action_continue_list_mailboxes, next_action_for_search_result,
    next_action_get_attachment_bytes, next_action_list_accounts, next_action_list_mailboxes,
    next_action_search_mailbox, preferred_mailbox_name, status_from_counts,
    status_from_issue_and_counts,
};
use super::validation::{
    ATTACHMENT_BYTES_DEFAULT, build_search_query, check_gmail_query_supported, header_value,
    parse_and_validate_message_id, validate_account_id, validate_attachment_input, validate_chars,
    validate_list_mailboxes_input, validate_search_input, validate_search_text,
};
use super::{MAX_CURSOR_UIDS_STORED, MAX_SEARCH_LIMIT, MailImapServer};

/// Maximum raw message bytes requested per message for a body preview.
const PREVIEW_FETCH_MAX_BYTES: usize = 32_768;

/// Why a search stopped before it produced a claimed page.
enum SearchStop {
    /// A tool error is returned; `reusable` says whether the session is still healthy.
    Error {
        /// Error returned to the caller.
        error: AppError,
        /// Whether the session saw no command error and may return to the cache.
        reusable: bool,
    },
    /// The search failed operationally and the response reports these issues.
    Failed {
        /// Issues describing the failure.
        issues: Vec<ToolIssue>,
        /// Whether the session saw no command error and may return to the cache.
        reusable: bool,
    },
}

/// One page claimed from the cursor store.
#[derive(Debug)]
struct ClaimedPage {
    /// Non-empty mailbox slices and snapshot settings of this page.
    page: SearchCursorPage,
    /// Cursor identifier retained only when another page remains.
    cursor_id: Option<String>,
    /// Issues collected before the page was claimed, such as conversation discovery notes.
    issues: Vec<ToolIssue>,
}

/// Conversation criterion resolved from the source message of a thread search.
#[derive(Debug)]
struct ThreadSeed {
    /// Criterion added to the search query.
    query: SearchThreadQuery,
    /// Discovery method reported to the caller and stored in the cursor.
    method: ThreadMethod,
    /// Issues found while reading the source message.
    issues: Vec<ToolIssue>,
}

/// Identity and presentation settings of one claimed mailbox slice.
struct SummarySlice {
    /// Account that owns the mailbox.
    account_id: String,
    /// Canonical mailbox name used in message identifiers.
    mailbox: String,
    /// UIDVALIDITY observed when the search ran.
    uidvalidity: u32,
    /// Claimed UIDs in stored descending order.
    uids: Vec<u32>,
    /// Preview character limit, or `None` when previews are not wanted.
    preview_max_chars: Option<usize>,
    /// Whether Gmail attributes were requested and are expected.
    gmail: bool,
    /// Resource ceilings for header and preview parsing.
    limits: mime::ProcessingLimits,
}

/// Gmail identity fields rendered as output text.
#[derive(Debug, Default, PartialEq, Eq)]
struct GmailFields {
    /// Labels exactly as observed, or `None` when the server omitted them.
    labels: Option<Vec<String>>,
    /// `X-GM-MSGID` as a decimal string.
    message_id: Option<String>,
    /// `X-GM-THRID` as a decimal string.
    thread_id: Option<String>,
}

impl GmailFields {
    /// Renders the Gmail attributes of decoded metadata without inventing values.
    fn from_metadata(metadata: &imap::MessageMetadata) -> Self {
        Self {
            labels: metadata.gmail_labels.clone(),
            message_id: metadata.gmail_message_id.map(|id| id.to_string()),
            thread_id: metadata.gmail_thread_id.map(|id| id.to_string()),
        }
    }
}

/// Flags and Gmail identity of one message with issues for omitted attributes.
#[derive(Debug)]
struct MessageState {
    /// Flags as returned by the server, or `None` when omitted.
    flags: Option<Vec<String>>,
    /// Gmail identity fields, empty on non-Gmail servers.
    gmail: GmailFields,
    /// Issues for attributes the server did not return.
    issues: Vec<ToolIssue>,
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

/// Raw message prefix read within the fetch budget.
struct MessagePrefix {
    /// Raw bytes from the start of the message, at most the fetch budget.
    raw: Vec<u8>,
    /// Message size declared by the server.
    declared_size: usize,
    /// Whether `raw` holds the whole message.
    complete: bool,
}

/// Why a message prefix could not be read.
enum PrefixFailure {
    /// The configured fetch budget is zero, so no content may be fetched.
    ZeroBudget,
    /// A FETCH command failed, so the session must not be used again.
    Fetch {
        /// Name of the failed stage, reported in the issue.
        stage: &'static str,
        /// Error returned by the command.
        error: AppError,
    },
}

impl PrefixFailure {
    /// Whether the session saw no command error and may return to the cache.
    fn session_reusable(&self) -> bool {
        matches!(self, Self::ZeroBudget)
    }

    /// Describes the failure as an issue for one message.
    fn into_issue(self, uid: u32, message_id: &str) -> ToolIssue {
        match self {
            Self::ZeroBudget => fetch_budget_issue(
                uid,
                message_id,
                "message fetch budget is zero; content was omitted",
            ),
            Self::Fetch { stage, error } => ToolIssue::from_error(stage, &error)
                .with_uid(uid)
                .with_message_id(message_id),
        }
    }
}

/// Reads the declared size and the raw prefix of one message within the fetch budget.
///
/// The mailbox must already be selected. Callers add their own issues and
/// release the session.
async fn fetch_message_prefix(
    config: &ServerConfig,
    session: &mut imap::ImapSession,
    uid: u32,
) -> Result<MessagePrefix, PrefixFailure> {
    let declared_size = imap::fetch_message_size(config, session, uid)
        .await
        .map_err(|error| PrefixFailure::Fetch {
            stage: "fetch_message_size",
            error,
        })?;
    // Bytes requested from the start of the message.
    let fetch_budget = config.message_fetch_budget_bytes;
    if fetch_budget == 0 {
        return Err(PrefixFailure::ZeroBudget);
    }
    let raw = imap::fetch_raw_message_range(config, session, uid, 0, fetch_budget)
        .await
        .map_err(|error| PrefixFailure::Fetch {
            stage: "fetch_raw_message_range",
            error,
        })?;
    Ok(MessagePrefix {
        complete: raw.len() >= declared_size,
        raw,
        declared_size,
    })
}

impl MailImapServer {
    pub(super) async fn list_mailboxes_impl(
        &self,
        input: ListMailboxesInput,
    ) -> AppResult<ListMailboxesData> {
        validate_list_mailboxes_input(&input)?;

        if let Some(cursor_id) = input.cursor.clone() {
            let data = list_mailboxes_from_cursor(&self.mailbox_cursors, &input, cursor_id).await?;
            return Ok(self.add_mailbox_counts(&input, data, None).await);
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
        // A fully drained LIST session is kept for the counts stage; a truncated one is discarded.
        let counts_session = if truncated {
            session.discard();
            None
        } else if input.include_counts {
            Some(session)
        } else {
            let _ = release_read_session(self, session, true).await;
            None
        };

        let total = (!truncated).then_some(mailboxes.len());
        let data = list_mailboxes_from_snapshot(
            &self.mailbox_cursors,
            &input,
            mailboxes.into(),
            total,
            truncated,
        )
        .await?;
        Ok(self.add_mailbox_counts(&input, data, counts_session).await)
    }

    /// Adds live counts to the selectable mailboxes of one assembled page when requested.
    ///
    /// `session` is a clean lease kept from a fully drained LIST; without one a
    /// session is checked out. A failure keeps the page and its pagination,
    /// leaves unknown counts absent, reports a partial status, and discards the
    /// errored session without sending more commands on it.
    async fn add_mailbox_counts(
        &self,
        input: &ListMailboxesInput,
        mut data: ListMailboxesData,
        session: Option<ReadSessionLease>,
    ) -> ListMailboxesData {
        if !input.include_counts || !data.mailboxes.iter().any(|mailbox| mailbox.selectable) {
            if let Some(session) = session {
                finish_read_session(self, session, true).await;
            }
            return data;
        }
        // A usable session, or the issue that explains why none could be obtained.
        let leased = match session {
            Some(session) => Ok(session),
            None => self
                .checkout_read_session(&input.account_id)
                .await
                .map_err(|error| ToolIssue::from_error("connect_authenticated", &error)),
        };
        // The first count or connection failure; `None` when every count was read.
        let failure = match leased {
            Ok(mut session) => {
                let failure =
                    enrich_mailbox_counts(&self.config, session.session(), &mut data.mailboxes)
                        .await;
                finish_read_session(self, session, failure.is_none()).await;
                failure
            }
            Err(issue) => Some(issue),
        };
        if let Some(issue) = failure {
            data.issues.push(issue);
            data.status =
                status_from_issue_and_counts(&data.issues, !data.mailboxes.is_empty()).to_owned();
            log_runtime_issues(
                "imap_list_mailboxes",
                &data.status,
                &input.account_id,
                None,
                &data.issues,
            );
        }
        data
    }

    /// Searches 1..10 mailboxes, or continues a stored cursor, and returns one grouped page.
    ///
    /// A continuation is validated before network use and claimed only after a
    /// session and its capabilities are available. A claimed page is a consumed
    /// attempt even when its fetch fails.
    pub(super) async fn search_messages_impl(
        &self,
        input: SearchMessagesInput,
    ) -> AppResult<SearchResultData> {
        validate_account_id(&input.account_id)?;
        validate_search_input(&input)?;
        // Canonical mailbox identities in request order; they key groups, cursor binding, and IDs.
        let mailboxes = input
            .mailboxes
            .iter()
            .map(|mailbox| normalize_mailbox_name(mailbox))
            .collect::<Vec<_>>();
        // Page size bounded by the existing search ceiling.
        let limit = input.limit.clamp(1, MAX_SEARCH_LIMIT);

        if let Some(cursor) = input.cursor.as_deref() {
            // A continuation is checked before any network use and is not claimed yet.
            self.cursors
                .lock()
                .await
                .validate_binding(cursor, &input.account_id, &mailboxes)
                .map_err(search_cursor_claim_error)?;
        }

        let mut session = match self.checkout_read_session(&input.account_id).await {
            Ok(session) => session,
            Err(error) => {
                let issues = vec![ToolIssue::from_error("connect_authenticated", &error)];
                return Ok(logged_failed_search(&input.account_id, mailboxes, issues));
            }
        };
        // Capabilities are read once for this request on the checked-out session.
        let gmail = match imap::capabilities(&self.config, session.session()).await {
            Ok(capabilities) => is_gmail_capable(&capabilities),
            Err(error) => {
                let issues = vec![ToolIssue::from_error("capabilities", &error)];
                session.discard();
                return Ok(logged_failed_search(&input.account_id, mailboxes, issues));
            }
        };

        // Mailbox and UIDVALIDITY currently selected on the session, if known.
        let mut selected = None;
        // First page of a new search or the next page of a continuation.
        let claimed = match input.cursor.as_deref() {
            Some(cursor) => {
                claim_search_page(&self.cursors, cursor, &input.account_id, &mailboxes, limit)
                    .await
                    .map_err(|error| SearchStop::Error {
                        error,
                        reusable: true,
                    })
            }
            None => {
                self.start_search(
                    &input,
                    &mailboxes,
                    limit,
                    gmail,
                    session.session(),
                    &mut selected,
                )
                .await
            }
        };
        let ClaimedPage {
            page,
            cursor_id,
            mut issues,
        } = match claimed {
            Ok(claimed) => claimed,
            Err(SearchStop::Error { error, reusable }) => {
                finish_read_session(self, session, reusable).await;
                return Err(error);
            }
            Err(SearchStop::Failed { issues, reusable }) => {
                finish_read_session(self, session, reusable).await;
                return Ok(logged_failed_search(&input.account_id, mailboxes, issues));
            }
        };

        // Summaries and counts for the claimed slices of this page.
        let mut fetched = match self
            .fetch_claimed_page(
                session.session(),
                &input.account_id,
                &page,
                gmail,
                &mut selected,
            )
            .await
        {
            Ok(fetched) => fetched,
            Err(error) => {
                if let Some(cursor) = cursor_id.as_deref() {
                    self.cursors.lock().await.delete(cursor);
                }
                finish_read_session(self, session, true).await;
                return Err(error);
            }
        };
        issues.append(&mut fetched.issues);
        finish_read_session(self, session, !fetched.command_failed).await;

        let status = status_from_issue_and_counts(&issues, !fetched.messages.is_empty()).to_owned();
        log_runtime_issues(
            "imap_search_messages",
            &status,
            &input.account_id,
            Some(&mailboxes.join(", ")),
            &issues,
        );
        let next_action = next_action_for_search_result(
            &status,
            &input.account_id,
            &mailboxes,
            input.limit,
            cursor_id.as_deref(),
            &fetched.messages,
        );

        Ok(SearchResultData {
            status,
            issues,
            next_action,
            account_id: input.account_id,
            mailboxes,
            total: Some(page.total),
            thread_method: page.thread_method,
            thread_possibly_incomplete: page.thread_method == Some(ThreadMethod::Headers),
            attempted: fetched.attempted,
            returned: fetched.messages.len(),
            failed: fetched.failed,
            messages: fetched.messages,
            next_cursor: cursor_id,
            has_more: page.has_more,
        })
    }

    /// Runs a new search across all requested mailboxes and claims its first page.
    ///
    /// Mailboxes are examined and searched sequentially in request order. Any
    /// failure stops the search with an unknown total, and a session that saw
    /// a command error is reported as not reusable.
    async fn start_search(
        &self,
        input: &SearchMessagesInput,
        mailboxes: &[String],
        limit: usize,
        gmail: bool,
        session: &mut imap::ImapSession,
        selected: &mut Option<(String, u32)>,
    ) -> Result<ClaimedPage, SearchStop> {
        check_gmail_query_supported(input, gmail).map_err(|error| SearchStop::Error {
            error,
            reusable: true,
        })?;
        // Hard errors keep the session unless a command already failed on it.
        let hard_error = |error: AppError| SearchStop::Error {
            reusable: is_hard_precondition_error(&error),
            error,
        };

        // Conversation criterion resolved from the source message, if requested.
        let thread = match input.thread_of_message_id.as_deref() {
            Some(seed) => {
                let seed_id = parse_and_validate_message_id(seed).map_err(hard_error)?;
                ensure_uidvalidity_matches_readonly(&self.config, session, &seed_id)
                    .await
                    .map_err(hard_error)?;
                Some(
                    resolve_thread_seed(&self.config, session, &seed_id, gmail)
                        .await
                        .map_err(hard_error)?,
                )
            }
            None => None,
        };
        // Search criteria for a new search, including the conversation criterion.
        let query = build_search_query(input, thread.as_ref().map(|seed| &seed.query)).map_err(
            |error| SearchStop::Error {
                error,
                reusable: true,
            },
        )?;
        // Issues kept in a failed response, starting with conversation discovery notes.
        let mut notes = thread
            .as_ref()
            .map(|seed| seed.issues.clone())
            .unwrap_or_default();

        // Groups stored in the cursor, one per mailbox in request order.
        let mut groups = Vec::with_capacity(mailboxes.len());
        // Combined candidate count checked against the cursor ceiling.
        let mut combined = 0usize;
        for mailbox in mailboxes {
            let uidvalidity =
                match imap::select_mailbox_readonly(&self.config, session, mailbox).await {
                    Ok(uidvalidity) => uidvalidity,
                    Err(error) => {
                        notes.push(ToolIssue::from_error("select_mailbox_readonly", &error));
                        return Err(SearchStop::Failed {
                            issues: notes,
                            reusable: false,
                        });
                    }
                };
            *selected = Some((mailbox.clone(), uidvalidity));
            let candidates = match imap::uid_search(&self.config, session, &query).await {
                Ok(uids) => uids,
                Err(error) => {
                    notes.push(
                        ToolIssue::from_error("uid_search", &error)
                            .with_detail(&format!("mailbox '{mailbox}'")),
                    );
                    return Err(SearchStop::Failed {
                        issues: notes,
                        reusable: false,
                    });
                }
            };
            combined = add_to_combined_total(combined, candidates.len()).map_err(|error| {
                SearchStop::Error {
                    error,
                    reusable: true,
                }
            })?;
            let uids = match thread.as_ref().map(|seed| &seed.query) {
                Some(SearchThreadQuery::Headers(ids)) => {
                    match verified_thread_matches(
                        &self.config,
                        self.mime_parse_semaphore.clone(),
                        session,
                        ids,
                        candidates,
                    )
                    .await
                    {
                        Ok(uids) => uids,
                        Err(error) => {
                            notes.push(
                                ToolIssue::from_error("thread_verification", &error)
                                    .with_detail(&format!("mailbox '{mailbox}'")),
                            );
                            return Err(SearchStop::Failed {
                                issues: notes,
                                reusable: false,
                            });
                        }
                    }
                }
                _ => candidates,
            };
            groups.push(SearchCursorGroup {
                mailbox: mailbox.clone(),
                uidvalidity,
                uids_desc: Arc::<[u32]>::from(uids),
            });
        }

        // Snapshot stored for later pages of this search.
        let entry = CursorEntry {
            account_id: input.account_id.clone(),
            groups: groups.into(),
            offset: 0,
            preview_max_chars: input.preview_max_chars,
            thread_method: thread.as_ref().map(|seed| seed.method),
            expires_at: Instant::now(),
        };
        // First page claimed from the stored snapshot.
        let mut claimed = store_and_claim_first_page(&self.cursors, entry, mailboxes, limit)
            .await
            .map_err(|error| SearchStop::Error {
                error,
                reusable: true,
            })?;
        claimed.issues = thread.map(|seed| seed.issues).unwrap_or_default();
        Ok(claimed)
    }

    /// Fetches the summaries of every claimed mailbox slice in group order.
    ///
    /// A changed UIDVALIDITY is a conflict. After a command error the session
    /// is not used again and the remaining slices are reported as failed.
    async fn fetch_claimed_page(
        &self,
        session: &mut imap::ImapSession,
        account_id: &str,
        page: &SearchCursorPage,
        gmail: bool,
        selected: &mut Option<(String, u32)>,
    ) -> AppResult<SummaryBuildResult> {
        // Aggregated outcome across all slices of the page.
        let mut outcome = SummaryBuildResult::default();
        for group in &page.groups {
            if outcome.command_failed {
                outcome.fail_slice(group.uids.len(), skipped_slice_issue(&group.mailbox));
                continue;
            }
            // The mailbox is reselected only when this request has not already selected it.
            let already_selected = selected.as_ref().is_some_and(|(mailbox, uidvalidity)| {
                *mailbox == group.mailbox && *uidvalidity == group.uidvalidity
            });
            if !already_selected {
                match imap::select_mailbox_readonly(&self.config, session, &group.mailbox).await {
                    Ok(uidvalidity) if uidvalidity == group.uidvalidity => {
                        *selected = Some((group.mailbox.clone(), uidvalidity));
                    }
                    Ok(_) => {
                        return Err(AppError::Conflict(
                            "mailbox snapshot changed; rerun search".to_owned(),
                        ));
                    }
                    Err(error) => {
                        *selected = None;
                        outcome.command_failed = true;
                        outcome.fail_slice(
                            group.uids.len(),
                            ToolIssue::from_error("select_mailbox_readonly", &error),
                        );
                        continue;
                    }
                }
            }
            // Identity and presentation settings of this slice.
            let slice = SummarySlice {
                account_id: account_id.to_owned(),
                mailbox: group.mailbox.clone(),
                uidvalidity: group.uidvalidity,
                uids: group.uids.clone(),
                preview_max_chars: page.preview_max_chars,
                gmail,
                limits: preview_processing_limits(&self.config),
            };
            outcome.merge(
                fetch_group_summaries(
                    &self.config,
                    self.mime_parse_semaphore.clone(),
                    session,
                    slice,
                )
                .await,
            );
        }
        Ok(outcome)
    }

    /// Returns one parsed message with flags and, on Gmail servers, Gmail labels and identities.
    ///
    /// Flags and Gmail attributes come from one State metadata fetch on the same
    /// session after a single capability check.
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

        let prefix =
            match fetch_message_prefix(&self.config, session.session(), message_id.uid).await {
                Ok(prefix) => prefix,
                Err(failure) => {
                    let reusable = failure.session_reusable();
                    issues.push(failure.into_issue(message_id.uid, &encoded_message_id));
                    // A failed fetch discards the connection instead of sending LOGOUT on it.
                    finish_read_session(self, session, reusable).await;
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
        let raw = prefix.raw;
        let raw_complete = prefix.complete;
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

        // Set when a command failed, so the session is discarded without further commands.
        let mut command_failed = false;
        // Capabilities are read once for this request, then one State metadata fetch follows.
        let state = match imap::capabilities(&self.config, session.session()).await {
            Ok(capabilities) => {
                match fetch_message_state(
                    &self.config,
                    session.session(),
                    message_id.uid,
                    &encoded_message_id,
                    is_gmail_capable(&capabilities),
                )
                .await
                {
                    Ok(state) => Some(state),
                    Err(error) => {
                        command_failed = true;
                        issues.push(
                            ToolIssue::from_error("fetch_message_metadata", &error)
                                .with_uid(message_id.uid)
                                .with_message_id(&encoded_message_id),
                        );
                        None
                    }
                }
            }
            Err(error) => {
                command_failed = true;
                issues.push(
                    ToolIssue::from_error("capabilities", &error)
                        .with_uid(message_id.uid)
                        .with_message_id(&encoded_message_id),
                );
                None
            }
        };
        let MessageState {
            flags,
            gmail,
            issues: mut state_issues,
        } = state.unwrap_or(MessageState {
            flags: None,
            gmail: GmailFields::default(),
            issues: Vec::new(),
        });
        issues.append(&mut state_issues);

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
            gmail_labels: gmail.labels,
            gmail_message_id: gmail.message_id,
            gmail_thread_id: gmail.thread_id,
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
        if command_failed {
            session.discard();
        } else {
            let _ = release_read_session(self, session, issues.is_empty()).await;
        }

        Ok(GetMessageData {
            status: status.to_owned(),
            issues,
            account_id: message_id.account_id.clone(),
            message: Some(detail),
        })
    }

    /// Reads one attachment of a message as text or base64 bytes.
    ///
    /// The raw prefix must cover the whole message; an incomplete prefix, a
    /// zero budget, or any processing limit fails the call without a payload.
    pub(super) async fn get_attachment_impl(
        &self,
        input: GetAttachmentInput,
    ) -> AppResult<GetAttachmentData> {
        let request = validate_attachment_input(&input)?;
        let message_id = parse_and_validate_message_id(&input.message_id)?;
        let encoded_message_id = message_id.encode();
        // Logs the issues of a call that returns no attachment and builds its response.
        let failed = |issues: Vec<ToolIssue>| {
            log_runtime_issues(
                "imap_get_attachment",
                "failed",
                &message_id.account_id,
                Some(&message_id.mailbox),
                &issues,
            );
            attachment_data(&message_id, &input.part_id, issues, None, None)
        };

        let mut session = match self.checkout_read_session(&message_id.account_id).await {
            Ok(session) => session,
            Err(error) => {
                return Ok(failed(vec![
                    ToolIssue::from_error("connect_authenticated", &error)
                        .with_message_id(&encoded_message_id),
                ]));
            }
        };
        if let Err(error) =
            ensure_uidvalidity_matches_readonly(&self.config, session.session(), &message_id).await
        {
            // A failed selection leaves the connection state unknown, so no LOGOUT is sent.
            finish_read_session(self, session, false).await;
            return Err(error);
        }
        let prefix =
            match fetch_message_prefix(&self.config, session.session(), message_id.uid).await {
                Ok(prefix) => prefix,
                Err(failure) => {
                    let reusable = failure.session_reusable();
                    let issue = failure.into_issue(message_id.uid, &encoded_message_id);
                    // A failed fetch discards the connection instead of sending LOGOUT on it.
                    finish_read_session(self, session, reusable).await;
                    return Ok(failed(vec![issue]));
                }
            };
        // The IMAP work is done, so the connection is free for other requests.
        let _ = release_read_session(self, session, true).await;
        if !prefix.complete {
            return Ok(failed(vec![fetch_budget_issue(
                message_id.uid,
                &encoded_message_id,
                &format!(
                    "message size {} bytes exceeds the fetch budget of {} bytes; the attachment cannot be read completely",
                    prefix.declared_size, self.config.message_fetch_budget_bytes
                ),
            )]));
        }

        let limits = preview_processing_limits(&self.config);
        let raw = prefix.raw;
        let part_id = input.part_id.clone();
        let outcome = run_bounded_mime_parse(self.mime_parse_semaphore.clone(), move || {
            mime::read_attachment(&raw, &part_id, &request, &limits)
        })
        .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error @ (AppError::InvalidInput(_) | AppError::NotFound(_))) => return Err(error),
            Err(error) => {
                return Ok(failed(vec![
                    ToolIssue::from_error("read_attachment", &error)
                        .with_uid(message_id.uid)
                        .with_message_id(&encoded_message_id),
                ]));
            }
        };
        let data = attachment_outcome_data(outcome, &request, &message_id, &input.part_id);
        log_runtime_issues(
            "imap_get_attachment",
            &data.status,
            &message_id.account_id,
            Some(&message_id.mailbox),
            &data.issues,
        );
        Ok(data)
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
        .map(|cursor| {
            next_action_continue_list_mailboxes(
                &input.account_id,
                cursor,
                input.limit,
                input.include_counts,
            )
        })
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

/// Returns a healthy session to the cache, or drops a failed one without sending more commands.
async fn finish_read_session(server: &MailImapServer, session: ReadSessionLease, healthy: bool) {
    if healthy {
        let _ = release_read_session(server, session, true).await;
    } else {
        session.discard();
    }
}

/// Builds a failed search response with an unknown total and logs its issues.
fn logged_failed_search(
    account_id: &str,
    mailboxes: Vec<String>,
    issues: Vec<ToolIssue>,
) -> SearchResultData {
    log_runtime_issues(
        "imap_search_messages",
        "failed",
        account_id,
        Some(&mailboxes.join(", ")),
        &issues,
    );
    SearchResultData {
        status: "failed".to_owned(),
        issues,
        next_action: next_action_list_mailboxes(account_id),
        account_id: account_id.to_owned(),
        mailboxes,
        total: None,
        thread_method: None,
        thread_possibly_incomplete: false,
        attempted: 0,
        returned: 0,
        failed: 0,
        messages: Vec::new(),
        next_cursor: None,
        has_more: false,
    }
}

/// Issue for a slice that was not fetched because an earlier command failed on the session.
fn skipped_slice_issue(mailbox: &str) -> ToolIssue {
    ToolIssue {
        code: "internal".to_owned(),
        stage: "session_discarded".to_owned(),
        message: format!(
            "slice of mailbox '{mailbox}' was not fetched because an earlier command failed"
        ),
        retryable: true,
        uid: None,
        message_id: None,
    }
}

/// Atomically claims the next page of an existing search cursor.
///
/// The claim revalidates the account and ordered mailbox binding under the
/// store lock. A claimed page is a consumed attempt and is never rolled back.
async fn claim_search_page(
    cursors: &Arc<Mutex<CursorStore>>,
    cursor_id: &str,
    account_id: &str,
    mailboxes: &[String],
    limit: usize,
) -> AppResult<ClaimedPage> {
    let page = cursors
        .lock()
        .await
        .claim_page(cursor_id, account_id, mailboxes, limit)
        .map_err(search_cursor_claim_error)?;
    Ok(ClaimedPage {
        cursor_id: page.has_more.then(|| cursor_id.to_owned()),
        page,
        issues: Vec::new(),
    })
}

/// Stores a new search snapshot and claims its first page under one lock.
///
/// The first page takes the same store path as every continuation.
async fn store_and_claim_first_page(
    cursors: &Arc<Mutex<CursorStore>>,
    entry: CursorEntry,
    mailboxes: &[String],
    limit: usize,
) -> AppResult<ClaimedPage> {
    let account_id = entry.account_id.clone();
    let mut store = cursors.lock().await;
    // Opaque cursor identifier of the new snapshot.
    let id = store.create(entry);
    let page = store
        .claim_page(&id, &account_id, mailboxes, limit)
        .map_err(search_cursor_claim_error)?;
    Ok(ClaimedPage {
        cursor_id: page.has_more.then_some(id),
        page,
        issues: Vec::new(),
    })
}

/// Maps a cursor claim failure to the existing input-error contract.
fn search_cursor_claim_error(error: SearchCursorClaimError) -> AppError {
    match error {
        SearchCursorClaimError::MissingOrExpired => {
            AppError::InvalidInput("cursor is invalid or expired".to_owned())
        }
        SearchCursorClaimError::AccountMismatch | SearchCursorClaimError::MailboxesMismatch => {
            AppError::InvalidInput("cursor does not match account/mailboxes".to_owned())
        }
        SearchCursorClaimError::OffsetOutOfRange => {
            AppError::InvalidInput("cursor offset is out of range".to_owned())
        }
    }
}

/// Adds one mailbox's candidate count to the running total under the cursor ceiling.
fn add_to_combined_total(running: usize, additional: usize) -> AppResult<usize> {
    let total = running.saturating_add(additional);
    if total > MAX_CURSOR_UIDS_STORED {
        return Err(AppError::InvalidInput(format!(
            "search matched {total} messages so far across the requested mailboxes; narrow filters or mailboxes to at most {MAX_CURSOR_UIDS_STORED} results"
        )));
    }
    Ok(total)
}

/// Resource ceilings for header, preview, and attachment parsing, taken from the server configuration.
fn preview_processing_limits(config: &ServerConfig) -> mime::ProcessingLimits {
    mime::ProcessingLimits {
        decode_budget_bytes: config.message_decode_budget_bytes,
        max_depth: config.mime_max_depth,
        max_parts: config.mime_max_parts,
        attachment_extract_budget_bytes: config.attachment_extract_budget_bytes,
        // Neither previews nor attachment reads use the parse-time attachment text limit.
        attachment_text_max_chars: 1,
    }
}

/// Builds the conversation criterion from the source message metadata.
///
/// Gmail requires the numeric `X-GM-THRID`. Other servers use the parsed
/// Message-ID, In-Reply-To, and References values of the source message.
fn thread_seed_from_metadata(
    message_id: &MessageId,
    metadata: Option<&imap::MessageMetadata>,
    gmail: bool,
) -> AppResult<ThreadSeed> {
    let metadata = metadata
        .ok_or_else(|| AppError::NotFound(format!("message uid {} not found", message_id.uid)))?;
    if gmail {
        let thread_id = metadata.gmail_thread_id.ok_or_else(|| {
            AppError::Internal("server did not return X-GM-THRID for the source message".to_owned())
        })?;
        return Ok(ThreadSeed {
            query: SearchThreadQuery::Gmail(thread_id),
            method: ThreadMethod::Gmail,
            issues: Vec::new(),
        });
    }
    let header_bytes = metadata.header_bytes.as_deref().ok_or_else(|| {
        AppError::Internal("server did not return headers for the source message".to_owned())
    })?;
    let references = mime::thread_reference_ids(&mime::parse_header_bytes(header_bytes)?);
    // Set when any header or ID could not be used, so discovery may be incomplete.
    let mut skipped = references.skipped_headers;
    let ids = references
        .ids
        .into_iter()
        .filter(|id| {
            let usable = validate_search_text(id).is_ok();
            skipped |= !usable;
            usable
        })
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Err(AppError::InvalidInput(
            "message has no usable Message-ID, References, or In-Reply-To; thread lookup is unavailable"
                .to_owned(),
        ));
    }
    let issues = if skipped {
        vec![ToolIssue {
            code: "partial_result".to_owned(),
            stage: "thread_discovery".to_owned(),
            message: "some Message-ID, In-Reply-To, or References values were skipped; the conversation may be incomplete".to_owned(),
            retryable: false,
            uid: Some(message_id.uid),
            message_id: Some(message_id.encode()),
        }]
    } else {
        Vec::new()
    };
    Ok(ThreadSeed {
        query: SearchThreadQuery::Headers(ids),
        method: ThreadMethod::Headers,
        issues,
    })
}

/// Fetches the source message metadata of a thread search and builds its criterion.
///
/// The caller has already verified UIDVALIDITY on the selected mailbox.
async fn resolve_thread_seed<T>(
    config: &ServerConfig,
    session: &mut Session<T>,
    message_id: &MessageId,
    gmail: bool,
) -> AppResult<ThreadSeed>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    let mut metadata = imap::fetch_message_metadata(
        config,
        session,
        &[message_id.uid],
        imap::MetadataProjection::Summary {
            preview_bytes: None,
        },
        gmail,
    )
    .await?;
    thread_seed_from_metadata(message_id, metadata.remove(&message_id.uid).as_ref(), gmail)
}

/// Keeps candidates whose parsed header IDs exactly match one of the seed IDs.
///
/// IMAP HEADER search is case-insensitive and substring-based, so this
/// verification decides membership. A candidate without headers or with
/// unparseable headers provides no evidence and is excluded.
fn verify_thread_candidates(
    seed_ids: &[String],
    candidates: Vec<u32>,
    metadata: &HashMap<u32, imap::MessageMetadata>,
) -> Vec<u32> {
    // Seed IDs for exact, case-sensitive membership tests.
    let seeds = seed_ids.iter().map(String::as_str).collect::<HashSet<_>>();
    candidates
        .into_iter()
        .filter(|uid| {
            metadata
                .get(uid)
                .and_then(|entry| entry.header_bytes.as_deref())
                .and_then(|bytes| mime::parse_header_bytes(bytes).ok())
                .is_some_and(|headers| {
                    mime::thread_reference_ids(&headers)
                        .ids
                        .iter()
                        .any(|id| seeds.contains(id.as_str()))
                })
        })
        .collect()
}

/// Fetches candidate headers in one command and verifies thread membership.
///
/// Parsing runs inside one bounded MIME batch. An empty candidate list sends no command.
async fn verified_thread_matches<T>(
    config: &ServerConfig,
    semaphore: Arc<Semaphore>,
    session: &mut Session<T>,
    seed_ids: &[String],
    candidates: Vec<u32>,
) -> AppResult<Vec<u32>>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    if candidates.is_empty() {
        return Ok(candidates);
    }
    let metadata = imap::fetch_message_metadata(
        config,
        session,
        &candidates,
        imap::MetadataProjection::Summary {
            preview_bytes: None,
        },
        false,
    )
    .await?;
    // Owned copy of the seed IDs for the blocking worker.
    let seed_ids = seed_ids.to_vec();
    run_bounded_mime_parse(semaphore, move || {
        Ok(verify_thread_candidates(&seed_ids, candidates, &metadata))
    })
    .await
}

/// Names the Gmail attributes the server omitted, or `None` when all are present.
fn gmail_missing_issue(
    metadata: &imap::MessageMetadata,
    uid: u32,
    message_id: &str,
) -> Option<ToolIssue> {
    // Names of requested Gmail attributes that the response lacked.
    let mut missing = Vec::new();
    if metadata.gmail_labels.is_none() {
        missing.push("X-GM-LABELS");
    }
    if metadata.gmail_message_id.is_none() {
        missing.push("X-GM-MSGID");
    }
    if metadata.gmail_thread_id.is_none() {
        missing.push("X-GM-THRID");
    }
    (!missing.is_empty()).then(|| ToolIssue {
        code: "partial_result".to_owned(),
        stage: "gmail_metadata".to_owned(),
        message: format!("server did not return {}", missing.join(", ")),
        retryable: false,
        uid: Some(uid),
        message_id: Some(message_id.to_owned()),
    })
}

/// Builds the preview of one message, recording issues without dropping the summary.
fn preview_from_metadata(
    metadata: &imap::MessageMetadata,
    preview_max_chars: usize,
    limits: &mime::ProcessingLimits,
    uid: u32,
    message_id: &str,
    issues: &mut Vec<ToolIssue>,
) -> Option<String> {
    if let Some(error) = &metadata.preview_issue {
        issues.push(
            ToolIssue::from_error("fetch_preview", error)
                .with_uid(uid)
                .with_message_id(message_id),
        );
        return None;
    }
    let Some(prefix) = metadata.raw_prefix.as_deref() else {
        let error =
            AppError::Internal("server did not return the requested body prefix".to_owned());
        issues.push(
            ToolIssue::from_error("fetch_preview", &error)
                .with_uid(uid)
                .with_message_id(message_id),
        );
        return None;
    };
    match mime::build_preview(prefix, preview_max_chars, limits) {
        Ok(outcome) => {
            issues.extend(
                outcome
                    .processing_limits
                    .into_iter()
                    .map(|limit| processing_limit_issue(limit, uid, message_id)),
            );
            outcome.preview
        }
        Err(error) => {
            issues.push(
                ToolIssue::from_error("build_preview", &error)
                    .with_uid(uid)
                    .with_message_id(message_id),
            );
            None
        }
    }
}

/// Assembles message summaries for one slice from decoded metadata.
///
/// Runs sequentially on a MIME worker. A message without usable headers counts
/// as failed; preview and Gmail attribute problems only add issues.
fn assemble_summaries(
    slice: SummarySlice,
    mut metadata: HashMap<u32, imap::MessageMetadata>,
) -> SummaryBuildResult {
    let mut result = SummaryBuildResult {
        attempted: slice.uids.len(),
        ..SummaryBuildResult::default()
    };
    for &uid in &slice.uids {
        let Some(entry) = metadata.remove(&uid) else {
            result.failed += 1;
            result.issues.push(ToolIssue {
                code: "internal".to_owned(),
                stage: "fetch_message_metadata".to_owned(),
                message: format!("UID {uid} missing from batch fetch response"),
                retryable: true,
                uid: Some(uid),
                message_id: None,
            });
            continue;
        };
        let Some(header_bytes) = entry.header_bytes.as_deref() else {
            result.failed += 1;
            result.issues.push(ToolIssue {
                code: "internal".to_owned(),
                stage: "fetch_message_metadata".to_owned(),
                message: format!("UID {uid} headers missing from batch fetch response"),
                retryable: true,
                uid: Some(uid),
                message_id: None,
            });
            continue;
        };
        let headers = match mime::parse_header_bytes(header_bytes) {
            Ok(headers) => headers,
            Err(error) => {
                result.failed += 1;
                result
                    .issues
                    .push(ToolIssue::from_error("parse_header_bytes", &error).with_uid(uid));
                continue;
            }
        };

        // Stable identifier of this message.
        let message_id = MessageId {
            account_id: slice.account_id.clone(),
            mailbox: slice.mailbox.clone(),
            uidvalidity: slice.uidvalidity,
            uid,
        }
        .encode();
        let preview = slice.preview_max_chars.and_then(|max_chars| {
            preview_from_metadata(
                &entry,
                max_chars,
                &slice.limits,
                uid,
                &message_id,
                &mut result.issues,
            )
        });
        if slice.gmail
            && let Some(issue) = gmail_missing_issue(&entry, uid, &message_id)
        {
            result.issues.push(issue);
        }
        let gmail = GmailFields::from_metadata(&entry);

        result.messages.push(MessageSummary {
            message_uri: build_message_uri(
                &slice.account_id,
                &slice.mailbox,
                slice.uidvalidity,
                uid,
            ),
            message_raw_uri: build_message_raw_uri(
                &slice.account_id,
                &slice.mailbox,
                slice.uidvalidity,
                uid,
            ),
            message_id,
            mailbox: slice.mailbox.clone(),
            uidvalidity: slice.uidvalidity,
            uid,
            date: header_value(&headers, "date"),
            from: header_value(&headers, "from"),
            subject: header_value(&headers, "subject"),
            flags: entry.flags,
            preview,
            gmail_labels: gmail.labels,
            gmail_message_id: gmail.message_id,
            gmail_thread_id: gmail.thread_id,
        });
    }
    result
}

/// Fetches one claimed slice with a single Summary FETCH and builds its summaries.
///
/// Previews request a bounded raw prefix in the same FETCH. A zero fetch
/// budget sends no body query and reports one budget issue. A failed command
/// marks the result so the caller discards the session.
async fn fetch_group_summaries<T>(
    config: &ServerConfig,
    semaphore: Arc<Semaphore>,
    session: &mut Session<T>,
    mut slice: SummarySlice,
) -> SummaryBuildResult
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    if slice.uids.is_empty() {
        return SummaryBuildResult::default();
    }
    // Issue reported once when previews were requested but the fetch budget is zero.
    let mut budget_issue = None;
    let preview_bytes = if slice.preview_max_chars.is_some() {
        let budget = PREVIEW_FETCH_MAX_BYTES.min(config.message_fetch_budget_bytes);
        if budget == 0 {
            slice.preview_max_chars = None;
            budget_issue = Some(ToolIssue {
                code: "limit_exceeded".to_owned(),
                stage: "fetch_budget".to_owned(),
                message: "message fetch budget is zero; previews were omitted".to_owned(),
                retryable: false,
                uid: None,
                message_id: None,
            });
            None
        } else {
            Some(budget)
        }
    } else {
        None
    };

    let metadata = match imap::fetch_message_metadata(
        config,
        session,
        &slice.uids,
        imap::MetadataProjection::Summary { preview_bytes },
        slice.gmail,
    )
    .await
    {
        Ok(metadata) => metadata,
        Err(error) => {
            let mut failed = SummaryBuildResult {
                command_failed: true,
                ..SummaryBuildResult::default()
            };
            failed.fail_slice(
                slice.uids.len(),
                ToolIssue::from_error("fetch_message_metadata", &error),
            );
            return failed;
        }
    };

    let uid_count = slice.uids.len();
    let mut result =
        match run_bounded_mime_parse(semaphore, move || Ok(assemble_summaries(slice, metadata)))
            .await
        {
            Ok(result) => result,
            Err(error) => {
                let mut failed = SummaryBuildResult::default();
                failed.fail_slice(
                    uid_count,
                    ToolIssue::from_error("assemble_summaries", &error),
                );
                return failed;
            }
        };
    result.issues.extend(budget_issue);
    result
}

/// Fetches flags and Gmail identity of one message with a single State FETCH.
///
/// An error means the command failed and the session must not be used again.
async fn fetch_message_state<T>(
    config: &ServerConfig,
    session: &mut Session<T>,
    uid: u32,
    encoded_message_id: &str,
    gmail: bool,
) -> AppResult<MessageState>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    let mut metadata = imap::fetch_message_metadata(
        config,
        session,
        &[uid],
        imap::MetadataProjection::State,
        gmail,
    )
    .await?;
    let Some(entry) = metadata.remove(&uid) else {
        return Ok(MessageState {
            flags: None,
            gmail: GmailFields::default(),
            issues: vec![ToolIssue {
                code: "internal".to_owned(),
                stage: "fetch_message_metadata".to_owned(),
                message: "server returned no metadata for the message".to_owned(),
                retryable: true,
                uid: Some(uid),
                message_id: Some(encoded_message_id.to_owned()),
            }],
        });
    };
    let issues = if gmail {
        gmail_missing_issue(&entry, uid, encoded_message_id)
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    Ok(MessageState {
        gmail: GmailFields::from_metadata(&entry),
        flags: entry.flags,
        issues,
    })
}

/// Reads live counts for the selectable mailboxes of one page, stopping at the first failure.
///
/// Mailboxes after a failure keep unknown counts. After a failure the
/// session must not be used again.
async fn enrich_mailbox_counts<T>(
    config: &ServerConfig,
    session: &mut Session<T>,
    mailboxes: &mut [MailboxInfo],
) -> Option<ToolIssue>
where
    T: AsyncRead + AsyncWrite + Unpin + Debug + Send,
{
    for mailbox in mailboxes.iter_mut().filter(|mailbox| mailbox.selectable) {
        match imap::mailbox_status(config, session, &mailbox.name).await {
            Ok(counts) => {
                mailbox.total_messages = Some(counts.total_messages);
                mailbox.unread_messages = Some(counts.unread_messages);
            }
            Err(error) => {
                return Some(ToolIssue::from_error("mailbox_status", &error).with_detail(
                    &format!(
                        "counts are unknown for mailbox '{}' and the remaining mailboxes of this page",
                        mailbox.name
                    ),
                ));
            }
        }
    }
    None
}

/// Builds an attachment response; its status follows the issues and the presence of content.
fn attachment_data(
    message_id: &MessageId,
    part_id: &str,
    issues: Vec<ToolIssue>,
    attachment: Option<AttachmentResult>,
    next_action: Option<NextAction>,
) -> GetAttachmentData {
    GetAttachmentData {
        status: status_from_issue_and_counts(&issues, attachment.is_some()).to_owned(),
        issues,
        account_id: message_id.account_id.clone(),
        message_id: message_id.encode(),
        part_id: part_id.to_owned(),
        attachment,
        next_action,
    }
}

/// Maps the MIME outcome of one attachment read to the tool response.
///
/// A limit leaves no payload. Unavailable text points to bytes mode instead
/// of switching modes, and a byte page with more data points to the next page.
fn attachment_outcome_data(
    outcome: mime::AttachmentOutcome,
    request: &mime::AttachmentRequest,
    message_id: &MessageId,
    part_id: &str,
) -> GetAttachmentData {
    // Encoded identifier used in issues and follow-up arguments.
    let encoded = message_id.encode();
    match outcome {
        mime::AttachmentOutcome::Content(read) => {
            // Follow-up call for the next byte page, when one exists.
            let mut next_action = None;
            let content = match read.payload {
                mime::AttachmentPayload::Text { text, truncated } => {
                    AttachmentContent::Text { text, truncated }
                }
                mime::AttachmentPayload::Bytes {
                    bytes,
                    offset_bytes,
                    has_more,
                } => {
                    let returned_bytes = bytes.len();
                    if let (true, mime::AttachmentRequest::Bytes { max_bytes, .. }) =
                        (has_more, request)
                    {
                        next_action = Some(next_action_get_attachment_bytes(
                            "Read the next page of attachment bytes.",
                            &encoded,
                            part_id,
                            offset_bytes + returned_bytes,
                            *max_bytes,
                        ));
                    }
                    AttachmentContent::Base64 {
                        content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                        offset_bytes,
                        returned_bytes,
                        has_more,
                    }
                }
            };
            let attachment = AttachmentResult {
                filename: read.filename,
                content_type: read.content_type,
                size_bytes: read.size_bytes,
                content,
            };
            attachment_data(
                message_id,
                part_id,
                Vec::new(),
                Some(attachment),
                next_action,
            )
        }
        mime::AttachmentOutcome::LimitExceeded(limit) => {
            let issue = processing_limit_issue(limit, message_id.uid, &encoded);
            attachment_data(message_id, part_id, vec![issue], None, None)
        }
        mime::AttachmentOutcome::TextUnavailable {
            size_bytes, reason, ..
        } => {
            let issue = ToolIssue {
                code: "text_unavailable".to_owned(),
                stage: "attachment_text".to_owned(),
                message: format!("{reason}; use mode=bytes to read the attachment"),
                retryable: false,
                uid: Some(message_id.uid),
                message_id: Some(encoded.clone()),
            };
            let next_action = next_action_get_attachment_bytes(
                "Read the attachment as base64 bytes instead.",
                &encoded,
                part_id,
                0,
                size_bytes.clamp(1, ATTACHMENT_BYTES_DEFAULT),
            );
            attachment_data(message_id, part_id, vec![issue], None, Some(next_action))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use async_imap::{Client, Session};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
    use tokio::sync::{Mutex, Semaphore};
    use tokio::task::JoinHandle;

    use super::{
        PrefixFailure, SummarySlice, add_to_combined_total, assemble_summaries,
        attachment_mode_for_raw_completeness, attachment_outcome_data, claim_search_page,
        enrich_mailbox_counts, fetch_budget_issue, fetch_group_summaries, fetch_message_state,
        list_mailboxes_from_cursor, list_mailboxes_from_snapshot, logged_failed_search,
        normalize_attachment_sizes_for_raw_completeness, preview_processing_limits,
        resolve_thread_seed, should_report_attachment_limit, status_from_issue_and_counts,
        store_and_claim_first_page, thread_seed_from_metadata, verified_thread_matches,
        verify_thread_candidates,
    };
    use crate::config::ServerConfig;
    use crate::errors::AppError;
    use crate::imap::MessageMetadata;
    use crate::message_id::MessageId;
    use crate::mime;
    use crate::models::{
        AttachmentContent, AttachmentInfo, AttachmentMode, ListMailboxesInput, MailboxInfo,
        MessageSummary, SearchMessagesInput, ThreadMethod,
    };
    use crate::pagination::{
        CursorEntry, CursorStore, MailboxCursorEntry, MailboxCursorStore, SearchCursorGroup,
    };
    use crate::server::MailImapServer;
    use crate::server::tests::schema_test_server_config;
    use crate::server::types::{SearchThreadQuery, ToolIssue, next_action_for_search_result};

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

    /// Starts a scripted IMAP peer; a `None` step closes the connection instead of replying.
    ///
    /// Returns an authenticated session and a task that yields every command the peer saw.
    async fn scripted_session(
        script: Vec<Option<String>>,
    ) -> (Session<DuplexStream>, JoinHandle<Vec<String>>) {
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
                let Some(text) = step else {
                    return commands;
                };
                writer
                    .write_all(text.replace("{tag}", &tag).as_bytes())
                    .await
                    .expect("scripted reply");
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

    /// Joins reply lines with CRLF terminators as one scripted reply.
    fn reply(lines: &[&str]) -> Option<String> {
        Some(lines.iter().map(|line| format!("{line}\r\n")).collect())
    }

    /// Formats an IMAP literal for the given text.
    fn lit(text: &str) -> String {
        format!("{{{}}}\r\n{text}", text.len())
    }

    /// FETCH data item requested for the summary header section.
    const PEEK_ITEM: &str =
        "BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC SUBJECT MESSAGE-ID REFERENCES IN-REPLY-TO)]";
    /// FETCH data item name in server responses for the summary header section.
    const HEADER_ITEM: &str =
        "BODY[HEADER.FIELDS (DATE FROM TO CC SUBJECT MESSAGE-ID REFERENCES IN-REPLY-TO)]";

    /// Server configuration whose socket timeout makes an unexpected extra command fail fast.
    fn test_config() -> ServerConfig {
        ServerConfig {
            socket_timeout_ms: 2_000,
            ..schema_test_server_config()
        }
    }

    /// Builds a FETCH response with headers and an optional bounded body prefix.
    fn fetch_line(seq: u32, uid: u32, headers: &str, prefix: Option<&str>) -> String {
        match prefix {
            Some(prefix) => format!(
                "* {seq} FETCH (UID {uid} FLAGS (\\Seen) {HEADER_ITEM} {} RFC822.SIZE 5000 BODY[]<0> {})",
                lit(headers),
                lit(prefix)
            ),
            None => format!(
                "* {seq} FETCH (UID {uid} FLAGS (\\Seen) {HEADER_ITEM} {})",
                lit(headers)
            ),
        }
    }

    /// Summary slice of INBOX with UIDVALIDITY 7.
    fn slice(uids: &[u32], preview_max_chars: Option<usize>, gmail: bool) -> SummarySlice {
        SummarySlice {
            account_id: "default".to_owned(),
            mailbox: "INBOX".to_owned(),
            uidvalidity: 7,
            uids: uids.to_vec(),
            preview_max_chars,
            gmail,
            limits: preview_processing_limits(&test_config()),
        }
    }

    /// Metadata with headers and `\Seen`, and optionally a raw body prefix.
    fn metadata_with(headers: &str, prefix: Option<&str>) -> MessageMetadata {
        MessageMetadata {
            header_bytes: Some(headers.as_bytes().to_vec()),
            flags: Some(vec!["\\Seen".to_owned()]),
            raw_prefix: prefix.map(|prefix| prefix.as_bytes().to_vec()),
            ..MessageMetadata::default()
        }
    }

    /// Assembles one UID and returns its preview and every issue.
    fn preview_of(prefix: &str, max_chars: usize) -> (Option<String>, Vec<ToolIssue>) {
        let metadata = HashMap::from([(1, metadata_with("Subject: s\r\n\r\n", Some(prefix)))]);
        let result = assemble_summaries(slice(&[1], Some(max_chars), false), metadata);
        assert_eq!(result.failed, 0, "{:?}", result.issues);
        assert_eq!(result.messages.len(), 1);
        (result.messages[0].preview.clone(), result.issues)
    }

    const TEXT_HEADER: &str = "Content-Type: text/plain; charset=utf-8\r\n\r\n";

    #[test]
    fn preview_skips_leading_whitespace_longer_than_the_limit() {
        let prefix = format!(
            "{TEXT_HEADER}{}Hello \r\n\r\n   brave\tnew   world",
            " \r\n".repeat(200)
        );
        let (preview, issues) = preview_of(&prefix, 50);
        assert_eq!(preview.as_deref(), Some("Hello brave new world"));
        assert!(issues.is_empty());
    }

    #[test]
    fn preview_truncates_multibyte_text_by_characters() {
        let prefix = format!("{TEXT_HEADER}{}", "語".repeat(120));
        let (preview, issues) = preview_of(&prefix, 50);
        assert_eq!(preview, Some("語".repeat(50)));
        assert!(issues.is_empty());
        let (short, _) = preview_of(&format!("{TEXT_HEADER}日本語 のテキスト"), 5);
        assert_eq!(short.as_deref(), Some("日本語 の"));
    }

    #[test]
    fn preview_of_html_only_message_is_plain_text() {
        let prefix = "Content-Type: text/html; charset=utf-8\r\n\r\n<html><body><p>Hello <b>bold</b> world</p><script>alert(1)</script></body></html>";
        let (preview, issues) = preview_of(prefix, 100);
        let preview = preview.expect("HTML-only message has a preview");
        assert!(preview.contains("Hello bold world"), "{preview}");
        assert!(!preview.contains('<') && !preview.contains("alert"));
        assert!(issues.is_empty());
    }

    #[test]
    fn preview_is_absent_for_empty_or_headers_only_messages() {
        for prefix in [
            "Subject: only headers\r\n\r\n",
            "Subject: only headers\r\n\r\n \r\n\t\r\n",
            "",
        ] {
            let (preview, issues) = preview_of(prefix, 80);
            assert_eq!(preview, None, "{prefix:?}");
            assert!(issues.is_empty(), "{prefix:?}: {issues:?}");
        }
    }

    #[test]
    fn preview_survives_truncated_transfer_encodings_and_cut_headers() {
        for prefix in [
            "Content-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\nSGVsbG8gd29ybGQgdGhpcyBpcyBhIHByZXZp\r\nZXc=\r\nSGVsb",
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\ncaf=C3=A9 au lait =\r\nsuite =C3",
            "Content-Type: text/plain\r\nX-Cut: half a hea",
            "Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nfirst part\r\n--b\r\nContent-Type: text/pl",
        ] {
            let metadata = HashMap::from([(1, metadata_with("Subject: s\r\n\r\n", Some(prefix)))]);
            let result = assemble_summaries(slice(&[1], Some(100), false), metadata);
            assert_eq!(result.messages.len(), 1, "{prefix:?}");
            assert_eq!(result.failed, 0, "{prefix:?}: {:?}", result.issues);
        }
        let (preview, _) = preview_of(
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\ncaf=C3=A9 au lait",
            80,
        );
        assert_eq!(preview.as_deref(), Some("café au lait"));
    }

    #[test]
    fn preview_respects_small_character_limits() {
        let prefix = format!("{TEXT_HEADER}{}", "word ".repeat(100));
        let (preview, _) = preview_of(&prefix, 50);
        let preview = preview.expect("preview");
        assert!(preview.chars().count() <= 50, "{preview}");
        assert!(preview.starts_with("word word"));
    }

    #[test]
    fn preview_depth_limit_adds_an_issue_and_keeps_the_header_summary() {
        let prefix = "Content-Type: multipart/mixed; boundary=a\r\n\r\n--a\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: multipart/mixed; boundary=c\r\n\r\n--c\r\nContent-Type: text/plain\r\n\r\ndeep text\r\n--c--\r\n--b--\r\n--a--\r\n";
        let mut slice = slice(&[1], Some(100), false);
        slice.limits.max_depth = 1;
        let metadata = HashMap::from([(1, metadata_with("Subject: kept\r\n\r\n", Some(prefix)))]);
        let result = assemble_summaries(slice, metadata);
        assert_eq!(result.failed, 0);
        assert_eq!(result.messages[0].subject.as_deref(), Some("kept"));
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.code == "limit_exceeded" && issue.stage == "mime_depth"),
            "{:?}",
            result.issues
        );
        assert_eq!(
            status_from_issue_and_counts(&result.issues, true),
            "partial"
        );
    }

    #[test]
    fn preview_parts_and_decode_limits_add_issues_and_keep_the_header_summary() {
        let many_parts = format!(
            "Content-Type: multipart/mixed; boundary=b\r\n\r\n{}--b--\r\n",
            "--b\r\nContent-Type: text/plain\r\n\r\npart\r\n".repeat(6)
        );
        let big_body = format!("{TEXT_HEADER}{}", "word ".repeat(2_000));
        for (prefix, stage, tighten) in [
            (
                many_parts,
                "mime_parts",
                (|limits: &mut crate::mime::ProcessingLimits| limits.max_parts = 2)
                    as fn(&mut crate::mime::ProcessingLimits),
            ),
            (big_body, "decode_budget", |limits| {
                limits.decode_budget_bytes = 64;
            }),
        ] {
            let mut slice = slice(&[1], Some(100), false);
            tighten(&mut slice.limits);
            let metadata =
                HashMap::from([(1, metadata_with("Subject: kept\r\n\r\n", Some(&prefix)))]);
            let result = assemble_summaries(slice, metadata);
            assert_eq!(result.failed, 0, "{stage}: {:?}", result.issues);
            assert_eq!(result.messages[0].subject.as_deref(), Some("kept"));
            assert!(
                result
                    .issues
                    .iter()
                    .any(|issue| issue.code == "limit_exceeded" && issue.stage == stage),
                "{stage}: {:?}",
                result.issues
            );
        }
    }

    #[test]
    fn oversized_or_missing_preview_prefix_keeps_header_summary_without_failure() {
        let mut oversized = metadata_with("Subject: kept\r\n\r\n", None);
        oversized.preview_issue = Some(AppError::Internal(
            "IMAP server returned more than the requested body range".to_owned(),
        ));
        let missing = metadata_with("Subject: also kept\r\n\r\n", None);
        let metadata = HashMap::from([(1, oversized), (2, missing)]);
        let result = assemble_summaries(slice(&[2, 1], Some(100), false), metadata);
        assert_eq!((result.attempted, result.failed), (2, 0));
        assert_eq!(result.messages.len(), 2);
        assert!(
            result
                .messages
                .iter()
                .all(|message| message.preview.is_none())
        );
        assert_eq!(result.issues.len(), 2);
        assert!(
            result
                .issues
                .iter()
                .all(|issue| issue.stage == "fetch_preview")
        );
        assert_eq!(result.issues[0].uid, Some(2));
        assert_eq!(result.issues[1].uid, Some(1));
    }

    #[test]
    fn headers_that_fail_to_parse_or_are_missing_count_as_failed_summaries() {
        let metadata = HashMap::from([
            (1, MessageMetadata::default()),
            (3, metadata_with("Subject: fine\r\n\r\n", None)),
        ]);
        let result = assemble_summaries(slice(&[3, 2, 1], None, false), metadata);
        assert_eq!((result.attempted, result.failed), (3, 2));
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].uid, 3);
        assert_eq!(result.issues.len(), 2);
    }

    #[test]
    fn summaries_keep_gmail_ids_exact_and_distinguish_missing_from_empty_labels() {
        let exact = MessageMetadata {
            gmail_labels: Some(vec!["\\Inbox".to_owned(), "Work".to_owned()]),
            gmail_message_id: Some((1u64 << 53) + 1),
            gmail_thread_id: Some(u64::MAX),
            ..metadata_with("Subject: a\r\n\r\n", None)
        };
        let observed_empty = MessageMetadata {
            gmail_labels: Some(Vec::new()),
            gmail_message_id: Some(2),
            gmail_thread_id: Some(3),
            ..metadata_with("Subject: b\r\n\r\n", None)
        };
        let missing = metadata_with("Subject: c\r\n\r\n", None);
        let metadata = HashMap::from([(1, exact), (2, observed_empty), (3, missing)]);
        let result = assemble_summaries(slice(&[1, 2, 3], None, true), metadata);

        let [first, second, third] = &result.messages[..] else {
            panic!("three summaries expected");
        };
        assert_eq!(first.gmail_message_id.as_deref(), Some("9007199254740993"));
        assert_eq!(
            first.gmail_thread_id.as_deref(),
            Some("18446744073709551615")
        );
        assert_eq!(first.gmail_labels.as_deref().map(<[String]>::len), Some(2));
        assert_eq!(second.gmail_labels, Some(Vec::new()));
        assert_eq!(third.gmail_labels, None);
        assert_eq!(third.gmail_message_id, None);
        // Only the message with missing attributes yields an issue; nothing is invented.
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].stage, "gmail_metadata");
        assert_eq!(result.issues[0].uid, Some(3));
        assert!(result.issues[0].message.contains("X-GM-LABELS"));
        assert_eq!(result.failed, 0);
    }

    #[test]
    fn non_gmail_summaries_leave_gmail_fields_absent_without_issues() {
        let metadata = HashMap::from([(1, metadata_with("Subject: a\r\n\r\n", None))]);
        let result = assemble_summaries(slice(&[1], None, false), metadata);
        assert!(result.issues.is_empty());
        let message = &result.messages[0];
        assert!(message.gmail_labels.is_none() && message.gmail_message_id.is_none());
        assert_eq!(message.message_id, "imap:default:INBOX:7:1");
    }

    #[tokio::test]
    async fn group_fetch_is_one_summary_command_and_ignores_unrelated_uids() {
        let first = fetch_line(
            1,
            5,
            "Subject: five\r\n\r\n",
            Some("Content-Type: text/plain\r\n\r\nbody five"),
        );
        let second = fetch_line(
            2,
            4,
            "Subject: four\r\n\r\n",
            Some("Content-Type: text/plain\r\n\r\nbody four"),
        );
        let (mut session, peer) = scripted_session(vec![reply(&[
            "* 9 EXISTS",
            "* 3 FETCH (UID 99 FLAGS (\\Deleted))",
            &first,
            &second,
            "{tag} OK done",
        ])])
        .await;
        let result = fetch_group_summaries(
            &test_config(),
            Arc::new(Semaphore::new(1)),
            &mut session,
            slice(&[5, 4], Some(100), false),
        )
        .await;
        drop(session);
        let commands = peer.await.expect("peer task");

        assert_eq!(commands.len(), 1, "{commands:?}");
        assert_eq!(
            commands[0],
            format!("A0002 UID FETCH 4:5 (UID FLAGS {PEEK_ITEM} RFC822.SIZE BODY.PEEK[]<0.32768>)")
        );
        assert!(!result.command_failed);
        assert_eq!((result.attempted, result.failed), (2, 0));
        let uids = result
            .messages
            .iter()
            .map(|message| message.uid)
            .collect::<Vec<_>>();
        assert_eq!(uids, [5, 4], "stored descending order is preserved");
        assert_eq!(result.messages[0].preview.as_deref(), Some("body five"));
        assert_eq!(result.messages[1].preview.as_deref(), Some("body four"));
        assert!(result.issues.is_empty());
    }

    #[tokio::test]
    async fn preview_prefix_is_bounded_by_a_small_fetch_budget() {
        let (mut session, peer) = scripted_session(vec![reply(&[
            &fetch_line(
                1,
                1,
                "Subject: s\r\n\r\n",
                Some("Content-Type: text/plain\r\n\r\nhi"),
            ),
            "{tag} OK done",
        ])])
        .await;
        let config = ServerConfig {
            message_fetch_budget_bytes: 100,
            ..test_config()
        };
        let result = fetch_group_summaries(
            &config,
            Arc::new(Semaphore::new(1)),
            &mut session,
            slice(&[1], Some(60), false),
        )
        .await;
        drop(session);
        let commands = peer.await.expect("peer task");
        assert!(
            commands[0].ends_with("RFC822.SIZE BODY.PEEK[]<0.100>)"),
            "{commands:?}"
        );
        assert_eq!(result.messages[0].preview.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn zero_fetch_budget_sends_no_body_query_and_reports_one_budget_issue() {
        let (mut session, peer) = scripted_session(vec![reply(&[
            &fetch_line(1, 2, "Subject: a\r\n\r\n", None),
            &fetch_line(2, 1, "Subject: b\r\n\r\n", None),
            "{tag} OK done",
        ])])
        .await;
        let config = ServerConfig {
            message_fetch_budget_bytes: 0,
            ..test_config()
        };
        let result = fetch_group_summaries(
            &config,
            Arc::new(Semaphore::new(1)),
            &mut session,
            slice(&[2, 1], Some(100), false),
        )
        .await;
        drop(session);
        let commands = peer.await.expect("peer task");

        assert_eq!(commands.len(), 1);
        assert!(!commands[0].contains("BODY.PEEK[]"), "{commands:?}");
        assert!(!commands[0].contains("RFC822.SIZE"));
        assert_eq!(result.messages.len(), 2);
        assert!(
            result
                .messages
                .iter()
                .all(|message| message.preview.is_none())
        );
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].stage, "fetch_budget");
        assert_eq!(result.failed, 0);
    }

    #[tokio::test]
    async fn oversized_server_prefix_keeps_headers_and_adds_an_issue() {
        let oversized = "x".repeat(40);
        let (mut session, peer) = scripted_session(vec![reply(&[
            &fetch_line(1, 1, "Subject: kept\r\n\r\n", Some(&oversized)),
            "{tag} OK done",
        ])])
        .await;
        let config = ServerConfig {
            message_fetch_budget_bytes: 8,
            ..test_config()
        };
        let result = fetch_group_summaries(
            &config,
            Arc::new(Semaphore::new(1)),
            &mut session,
            slice(&[1], Some(100), false),
        )
        .await;
        drop(session);
        peer.await.expect("peer task");
        assert!(!result.command_failed);
        assert_eq!(result.failed, 0);
        assert_eq!(result.messages[0].subject.as_deref(), Some("kept"));
        assert!(result.messages[0].preview.is_none());
        assert_eq!(result.issues[0].stage, "fetch_preview");
        assert_eq!(
            status_from_issue_and_counts(&result.issues, true),
            "partial"
        );
    }

    #[tokio::test]
    async fn failed_summary_fetch_marks_the_command_failure_and_sends_nothing_more() {
        let (mut session, peer) = scripted_session(vec![reply(&["{tag} NO fetch refused"])]).await;
        let result = fetch_group_summaries(
            &test_config(),
            Arc::new(Semaphore::new(1)),
            &mut session,
            slice(&[3, 2], None, false),
        )
        .await;
        drop(session);
        assert_eq!(peer.await.expect("peer task").len(), 1);
        assert!(result.command_failed);
        assert!(result.messages.is_empty());
        assert_eq!((result.attempted, result.failed), (2, 2));
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].stage, "fetch_message_metadata");
    }

    #[tokio::test(start_paused = true)]
    async fn summary_assembly_waits_for_the_shared_mime_permit() {
        let (mut session, peer) = scripted_session(vec![reply(&[
            &fetch_line(1, 1, "Subject: s\r\n\r\n", None),
            "{tag} OK done",
        ])])
        .await;
        let semaphore = Arc::new(Semaphore::new(0));
        let config = test_config();
        let waiting = tokio::time::timeout(
            Duration::from_millis(500),
            fetch_group_summaries(
                &config,
                semaphore.clone(),
                &mut session,
                slice(&[1], None, false),
            ),
        )
        .await;
        assert!(waiting.is_err(), "assembly must wait for a permit");
        assert_eq!(semaphore.available_permits(), 0);
        drop(session);
        assert_eq!(peer.await.expect("peer task").len(), 1);
    }

    #[tokio::test]
    async fn message_state_is_one_state_fetch_with_gmail_attributes() {
        let (mut session, peer) = scripted_session(vec![reply(&[
            "* 4 FETCH (UID 9 FLAGS (\\Seen) X-GM-LABELS (\\Inbox \"a b\") X-GM-MSGID 1278455344230334865 X-GM-THRID 18446744073709551615)",
            "{tag} OK done",
        ])])
        .await;
        let state = fetch_message_state(
            &test_config(),
            &mut session,
            9,
            "imap:default:INBOX:7:9",
            true,
        )
        .await
        .expect("state");
        drop(session);
        assert_eq!(
            peer.await.expect("peer task"),
            ["A0002 UID FETCH 9 (UID FLAGS X-GM-LABELS X-GM-MSGID X-GM-THRID)"]
        );
        assert_eq!(state.flags, Some(vec!["\\Seen".to_owned()]));
        assert_eq!(
            state.gmail.labels,
            Some(vec!["\\Inbox".to_owned(), "a b".to_owned()])
        );
        assert_eq!(
            state.gmail.message_id.as_deref(),
            Some("1278455344230334865")
        );
        assert_eq!(
            state.gmail.thread_id.as_deref(),
            Some("18446744073709551615")
        );
        assert!(state.issues.is_empty());
    }

    #[tokio::test]
    async fn message_state_reports_missing_gmail_attributes_and_skips_them_elsewhere() {
        let (mut session, peer) = scripted_session(vec![
            reply(&["* 1 FETCH (UID 9 FLAGS ())", "{tag} OK done"]),
            reply(&["* 1 FETCH (UID 9 FLAGS (\\Flagged))", "{tag} OK done"]),
        ])
        .await;
        let gmail = fetch_message_state(&test_config(), &mut session, 9, "m", true)
            .await
            .expect("state");
        assert_eq!(gmail.flags, Some(Vec::new()));
        assert_eq!(gmail.gmail, super::GmailFields::default());
        assert_eq!(gmail.issues.len(), 1);
        assert_eq!(gmail.issues[0].stage, "gmail_metadata");

        let plain = fetch_message_state(&test_config(), &mut session, 9, "m", false)
            .await
            .expect("state");
        drop(session);
        let commands = peer.await.expect("peer task");
        assert!(
            commands[1].ends_with("UID FETCH 9 (UID FLAGS)"),
            "{commands:?}"
        );
        assert!(plain.issues.is_empty());
        assert_eq!(plain.flags, Some(vec!["\\Flagged".to_owned()]));
    }

    #[tokio::test]
    async fn message_state_command_failure_is_an_error() {
        let (mut session, peer) = scripted_session(vec![reply(&["{tag} NO nope"])]).await;
        fetch_message_state(&test_config(), &mut session, 9, "m", false)
            .await
            .expect_err("tagged NO is a command failure");
        drop(session);
        assert_eq!(peer.await.expect("peer task").len(), 1);
    }

    /// Message identifier of the thread source used by seed tests.
    fn seed_message_id() -> MessageId {
        MessageId {
            account_id: "default".to_owned(),
            mailbox: "INBOX".to_owned(),
            uidvalidity: 7,
            uid: 42,
        }
    }

    #[test]
    fn gmail_seed_requires_the_numeric_thread_id() {
        let seed = MessageMetadata {
            gmail_thread_id: Some(1_278_455_344_230_334_865),
            ..metadata_with("Subject: s\r\n\r\n", None)
        };
        let resolved =
            thread_seed_from_metadata(&seed_message_id(), Some(&seed), true).expect("gmail seed");
        assert_eq!(
            resolved.query,
            SearchThreadQuery::Gmail(1_278_455_344_230_334_865)
        );
        assert_eq!(resolved.method, ThreadMethod::Gmail);
        assert!(resolved.issues.is_empty());

        let missing = metadata_with("Message-ID: <a@x>\r\n\r\n", None);
        let error = thread_seed_from_metadata(&seed_message_id(), Some(&missing), true)
            .expect_err("no X-GM-THRID");
        assert!(matches!(error, AppError::Internal(_)));
    }

    #[test]
    fn header_seed_collects_ids_in_header_order() {
        let seed = metadata_with(
            "Message-ID: <reply@x>\r\nIn-Reply-To: <parent@x>\r\nReferences: <root@x> <parent@x>\r\n\r\n",
            None,
        );
        let resolved =
            thread_seed_from_metadata(&seed_message_id(), Some(&seed), false).expect("header seed");
        assert_eq!(
            resolved.query,
            SearchThreadQuery::Headers(vec![
                "reply@x".to_owned(),
                "parent@x".to_owned(),
                "root@x".to_owned()
            ])
        );
        assert_eq!(resolved.method, ThreadMethod::Headers);
        assert!(resolved.issues.is_empty());
    }

    #[test]
    fn header_seed_without_usable_ids_is_invalid_input_with_the_exact_text() {
        for headers in [
            "Subject: nothing\r\n\r\n",
            "Message-ID: not an id\r\nReferences: (comment only)\r\n\r\n",
        ] {
            let seed = metadata_with(headers, None);
            let error = thread_seed_from_metadata(&seed_message_id(), Some(&seed), false)
                .expect_err("no usable id");
            assert!(
                matches!(&error, AppError::InvalidInput(message)
                    if message == "message has no usable Message-ID, References, or In-Reply-To; thread lookup is unavailable"),
                "{error}"
            );
        }
    }

    #[test]
    fn header_seed_caps_at_twenty_ids_and_reports_one_discovery_issue() {
        let references = (0..30)
            .map(|index| format!("<ref{index}@x>"))
            .collect::<Vec<_>>()
            .join(" ");
        let seed = metadata_with(&format!("References: {references}\r\n\r\n"), None);
        let resolved =
            thread_seed_from_metadata(&seed_message_id(), Some(&seed), false).expect("header seed");
        let SearchThreadQuery::Headers(ids) = &resolved.query else {
            panic!("header criterion expected");
        };
        assert_eq!(ids.len(), 20);
        assert_eq!(ids[0], "ref0@x");
        assert_eq!(resolved.issues.len(), 1);
        let issue = &resolved.issues[0];
        assert_eq!(
            (issue.code.as_str(), issue.stage.as_str()),
            ("partial_result", "thread_discovery")
        );
        assert!(!issue.retryable);
    }

    #[test]
    fn seed_that_vanished_is_not_found() {
        let error = thread_seed_from_metadata(&seed_message_id(), None, false)
            .expect_err("missing metadata");
        assert!(matches!(error, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn seed_resolution_fetches_the_source_once_with_the_shared_projection() {
        let headers = "Message-ID: <root@x>\r\n\r\n";
        let (mut session, peer) = scripted_session(vec![
            reply(&[&fetch_line(1, 42, headers, None), "{tag} OK done"]),
            reply(&[
                "* 1 FETCH (UID 42 FLAGS () X-GM-LABELS () X-GM-MSGID 1 X-GM-THRID 1278455344230334865)",
                "{tag} OK done",
            ]),
        ])
        .await;
        let header_seed =
            resolve_thread_seed(&test_config(), &mut session, &seed_message_id(), false)
                .await
                .expect("header seed");
        assert_eq!(
            header_seed.query,
            SearchThreadQuery::Headers(vec!["root@x".to_owned()])
        );
        let gmail_seed =
            resolve_thread_seed(&test_config(), &mut session, &seed_message_id(), true)
                .await
                .expect("gmail seed");
        assert_eq!(
            gmail_seed.query,
            SearchThreadQuery::Gmail(1_278_455_344_230_334_865)
        );
        drop(session);
        let commands = peer.await.expect("peer task");
        assert_eq!(commands.len(), 2);
        assert!(commands[0].ends_with(&format!("UID FETCH 42 (UID FLAGS {PEEK_ITEM})")));
        assert!(commands[1].ends_with(&format!(
            "UID FETCH 42 (UID FLAGS {PEEK_ITEM} X-GM-LABELS X-GM-MSGID X-GM-THRID)"
        )));
    }

    #[test]
    fn header_thread_verification_requires_an_exact_case_sensitive_id_match() {
        let metadata = HashMap::from([
            (
                12,
                metadata_with(
                    "Message-ID: <reply@x>\r\nIn-Reply-To: <root@x>\r\n\r\n",
                    None,
                ),
            ),
            (11, metadata_with("Message-ID: <Root@X>\r\n\r\n", None)),
            (10, metadata_with("Message-ID: <root@x>\r\n\r\n", None)),
            (9, MessageMetadata::default()),
            (8, metadata_with("References: <unclosed\r\n\r\n", None)),
            (
                6,
                metadata_with(
                    "Subject: about <root@x>\r\nMessage-ID: <rootx@x>\r\n\r\n",
                    None,
                ),
            ),
            (
                5,
                metadata_with(
                    "Message-ID: <aroot@x>\r\nReferences: <root@x.org>\r\n\r\n",
                    None,
                ),
            ),
        ]);
        let kept = verify_thread_candidates(
            &["root@x".to_owned()],
            vec![12, 11, 10, 9, 8, 7, 6, 5],
            &metadata,
        );
        assert_eq!(
            kept,
            [12, 10],
            "substring, case-only, unparseable, and missing candidates drop"
        );
    }

    #[tokio::test]
    async fn verified_thread_matches_uses_one_fetch_per_folder_and_counts_verified_ids() {
        let seed = vec!["root@x".to_owned()];
        let (mut session, peer) = scripted_session(vec![
            reply(&[
                &fetch_line(1, 30, "Message-ID: <root@x>\r\n\r\n", None),
                &fetch_line(2, 20, "Message-ID: <ROOT@x>\r\n\r\n", None),
                "{tag} OK done",
            ]),
            reply(&[
                &fetch_line(1, 7, "In-Reply-To: <root@x>\r\n\r\n", None),
                "{tag} OK done",
            ]),
        ])
        .await;
        let semaphore = Arc::new(Semaphore::new(1));
        let first = verified_thread_matches(
            &test_config(),
            semaphore.clone(),
            &mut session,
            &seed,
            vec![30, 20],
        )
        .await
        .expect("first folder");
        let second = verified_thread_matches(
            &test_config(),
            semaphore.clone(),
            &mut session,
            &seed,
            vec![7],
        )
        .await
        .expect("second folder");
        let none =
            verified_thread_matches(&test_config(), semaphore, &mut session, &seed, Vec::new())
                .await
                .expect("no candidates");
        drop(session);
        let commands = peer.await.expect("peer task");
        assert_eq!(
            commands.len(),
            2,
            "empty candidates send no command: {commands:?}"
        );
        assert_eq!((first, second, none), (vec![30], vec![7], Vec::new()));
    }

    #[test]
    fn combined_ceiling_accepts_exactly_1000_and_rejects_more() {
        assert_eq!(add_to_combined_total(0, 1_000).expect("at ceiling"), 1_000);
        assert_eq!(
            add_to_combined_total(600, 400).expect("sum at ceiling"),
            1_000
        );
        for (running, additional) in [(0, 1_001), (600, 401), (1_000, 1)] {
            let error = add_to_combined_total(running, additional).expect_err("over ceiling");
            assert!(matches!(error, AppError::InvalidInput(_)));
        }
    }

    fn entry(groups: &[(&str, u32, &[u32])], offset: usize) -> CursorEntry {
        CursorEntry {
            account_id: "default".to_owned(),
            groups: groups
                .iter()
                .map(|(mailbox, uidvalidity, uids)| SearchCursorGroup {
                    mailbox: (*mailbox).to_owned(),
                    uidvalidity: *uidvalidity,
                    uids_desc: (*uids).into(),
                })
                .collect::<Vec<_>>()
                .into(),
            offset,
            preview_max_chars: Some(120),
            thread_method: Some(ThreadMethod::Headers),
            expires_at: Instant::now(),
        }
    }

    fn names(mailboxes: &[&str]) -> Vec<String> {
        mailboxes.iter().map(|name| (*name).to_owned()).collect()
    }

    #[tokio::test]
    async fn claimed_pages_walk_groups_in_order_and_expose_stored_settings() {
        let cursors = Arc::new(Mutex::new(CursorStore::new(600, 8)));
        let mailboxes = names(&["INBOX", "Sent"]);
        let first = store_and_claim_first_page(
            &cursors,
            entry(&[("INBOX", 7, &[5, 4, 3]), ("Sent", 8, &[9, 8])], 2),
            &mailboxes,
            2,
        )
        .await
        .expect("first page");
        assert_eq!(first.page.total, 5);
        assert_eq!(first.page.groups.len(), 2);
        assert_eq!(
            (
                first.page.groups[0].mailbox.as_str(),
                &first.page.groups[0].uids[..]
            ),
            ("INBOX", &[3][..])
        );
        assert_eq!(
            (
                first.page.groups[1].mailbox.as_str(),
                &first.page.groups[1].uids[..]
            ),
            ("Sent", &[9][..])
        );
        assert_eq!(first.page.preview_max_chars, Some(120));
        assert_eq!(first.page.thread_method, Some(ThreadMethod::Headers));
        let cursor = first.cursor_id.expect("more pages remain");

        let last = claim_search_page(&cursors, &cursor, "default", &mailboxes, 10)
            .await
            .expect("last page");
        assert_eq!(last.page.groups.len(), 1);
        assert_eq!(last.page.groups[0].uids, [8]);
        assert_eq!(last.page.thread_method, Some(ThreadMethod::Headers));
        assert!(last.cursor_id.is_none() && !last.page.has_more);
        claim_search_page(&cursors, &cursor, "default", &mailboxes, 10)
            .await
            .expect_err("a terminal page deletes the cursor");
    }

    #[tokio::test]
    async fn successful_empty_search_has_known_zero_total_and_no_cursor() {
        let cursors = Arc::new(Mutex::new(CursorStore::new(600, 8)));
        let claimed = store_and_claim_first_page(
            &cursors,
            entry(&[("INBOX", 7, &[]), ("Sent", 8, &[])], 0),
            &names(&["INBOX", "Sent"]),
            10,
        )
        .await
        .expect("empty snapshot");
        assert_eq!(claimed.page.total, 0);
        assert!(claimed.page.groups.is_empty() && claimed.cursor_id.is_none());
    }

    #[test]
    fn failed_search_response_has_unknown_total_and_no_snapshot() {
        let data = logged_failed_search(
            "default",
            names(&["INBOX", "Sent"]),
            vec![ToolIssue::from_error(
                "uid_search",
                &AppError::Internal("boom".to_owned()),
            )],
        );
        assert_eq!(data.status, "failed");
        assert_eq!(data.total, None);
        assert_eq!(data.mailboxes, ["INBOX", "Sent"]);
        assert!(data.next_cursor.is_none() && !data.has_more && data.messages.is_empty());
        assert_eq!(data.next_action.tool, "imap_list_mailboxes");
    }

    #[tokio::test]
    async fn concurrent_claims_with_different_limits_are_disjoint_and_complete() {
        let cursors = Arc::new(Mutex::new(CursorStore::new(600, 8)));
        let mailboxes = names(&["INBOX", "Sent"]);
        let first = store_and_claim_first_page(
            &cursors,
            entry(&[("INBOX", 7, &[6, 5, 4, 3]), ("Sent", 8, &[9, 8, 7])], 0),
            &mailboxes,
            1,
        )
        .await
        .expect("first page");
        let cursor = first.cursor_id.clone().expect("continuation");
        let (a, b, c) = tokio::join!(
            claim_search_page(&cursors, &cursor, "default", &mailboxes, 2),
            claim_search_page(&cursors, &cursor, "default", &mailboxes, 3),
            claim_search_page(&cursors, &cursor, "default", &mailboxes, 4),
        );
        let mut seen = first
            .page
            .groups
            .iter()
            .flat_map(|group| group.uids.iter().map(|uid| (group.mailbox.clone(), *uid)))
            .collect::<Vec<_>>();
        for claimed in [a, b, c].into_iter().flatten() {
            seen.extend(
                claimed
                    .page
                    .groups
                    .iter()
                    .flat_map(|group| group.uids.iter().map(|uid| (group.mailbox.clone(), *uid))),
            );
        }
        seen.sort();
        let mut expected = [
            ("INBOX", 6),
            ("INBOX", 5),
            ("INBOX", 4),
            ("INBOX", 3),
            ("Sent", 9),
            ("Sent", 8),
            ("Sent", 7),
        ]
        .map(|(mailbox, uid)| (mailbox.to_owned(), uid))
        .to_vec();
        expected.sort();
        assert_eq!(seen, expected, "every UID is delivered exactly once");
    }

    #[tokio::test]
    async fn cursor_binding_accepts_equivalent_mailbox_spellings() {
        let cursors = Arc::new(Mutex::new(CursorStore::new(600, 8)));
        let first = store_and_claim_first_page(
            &cursors,
            entry(&[("日本語", 42, &[10, 9]), ("INBOX", 5, &[1])], 0),
            &names(&["&ZeVnLIqe-", "inbox"]),
            1,
        )
        .await
        .expect("first page");
        let cursor = first.cursor_id.expect("continuation");
        let next = claim_search_page(
            &cursors,
            &cursor,
            "default",
            &names(&["日本語", "INBOX"]),
            10,
        )
        .await
        .expect("equivalent spellings resume");
        assert_eq!(next.page.groups[0].uids, [9]);
    }

    /// Server whose configuration has no accounts, so any network attempt fails visibly.
    fn offline_server() -> MailImapServer {
        MailImapServer::new(schema_test_server_config())
    }

    fn search_input(mailboxes: &[&str], cursor: Option<&str>) -> SearchMessagesInput {
        SearchMessagesInput {
            account_id: "default".to_owned(),
            mailboxes: names(mailboxes),
            cursor: cursor.map(str::to_owned),
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
            limit: 10,
            preview_max_chars: None,
        }
    }

    #[tokio::test]
    async fn continuation_binding_errors_are_reported_before_any_network_use() {
        let server = offline_server();
        let cursor = server
            .cursors
            .lock()
            .await
            .create(entry(&[("INBOX", 7, &[3, 2, 1]), ("Sent", 8, &[9])], 0));
        for (input, expected) in [
            (
                search_input(&["INBOX", "Sent"], Some("missing")),
                "cursor is invalid or expired",
            ),
            (
                SearchMessagesInput {
                    account_id: "other".to_owned(),
                    ..search_input(&["INBOX", "Sent"], Some(&cursor))
                },
                "cursor does not match account/mailboxes",
            ),
            (
                search_input(&["Sent", "INBOX"], Some(&cursor)),
                "cursor does not match account/mailboxes",
            ),
            (
                search_input(&["INBOX"], Some(&cursor)),
                "cursor does not match account/mailboxes",
            ),
        ] {
            let error = server
                .search_messages_impl(input)
                .await
                .expect_err("binding errors are hard errors raised before network use");
            assert!(
                matches!(&error, AppError::InvalidInput(message) if message.contains(expected)),
                "{error}"
            );
        }
        // The failed attempts did not consume the cursor.
        let claimed = claim_search_page(
            &server.cursors,
            &cursor,
            "default",
            &names(&["INBOX", "Sent"]),
            10,
        )
        .await
        .expect("cursor is still intact");
        assert_eq!(claimed.page.total, 4);
    }

    #[tokio::test]
    async fn replayed_gmail_filter_on_a_continuation_is_ignored_semantically() {
        let server = offline_server();
        let cursor = server
            .cursors
            .lock()
            .await
            .create(entry(&[("INBOX", 7, &[3, 2, 1])], 0));
        let input = SearchMessagesInput {
            gmail_query: Some("has:attachment".to_owned()),
            flagged: Some(true),
            ..search_input(&["inbox"], Some(&cursor))
        };
        // No InvalidInput for the Gmail filter: the request reaches the (absent) account.
        let data = server
            .search_messages_impl(input)
            .await
            .expect("a replayed filter is not a validation error");
        assert_eq!(data.status, "failed");
        assert_eq!(data.issues[0].stage, "connect_authenticated");
        assert_eq!(data.total, None);
        assert_eq!(data.mailboxes, ["INBOX"]);

        let new_search = SearchMessagesInput {
            gmail_query: Some("has:attachment".to_owned()),
            ..search_input(&["INBOX"], None)
        };
        let data = server
            .search_messages_impl(new_search)
            .await
            .expect("failed response");
        assert_eq!(data.issues[0].stage, "connect_authenticated");
    }

    #[tokio::test]
    async fn invalid_search_input_is_rejected_before_any_cursor_or_network_work() {
        let server = offline_server();
        for input in [
            search_input(&["INBOX", "inbox"], None),
            search_input(&[], None),
            SearchMessagesInput {
                thread_of_message_id: Some("imap:other:INBOX:1:2".to_owned()),
                ..search_input(&["INBOX"], None)
            },
            SearchMessagesInput {
                keyword: Some("\\Seen".to_owned()),
                ..search_input(&["INBOX"], None)
            },
        ] {
            let error = server
                .search_messages_impl(input)
                .await
                .expect_err("invalid input");
            assert!(matches!(error, AppError::InvalidInput(_)));
        }
    }

    #[test]
    fn search_result_next_action_for_get_message_omits_account_id() {
        let next_action = next_action_for_search_result(
            "ok",
            "default",
            &names(&["INBOX"]),
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
                preview: Some("preview".to_owned()),
                gmail_labels: None,
                gmail_message_id: None,
                gmail_thread_id: None,
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
            total_messages: None,
            unread_messages: None,
        }
    }

    #[tokio::test]
    async fn mailbox_pages_use_stable_snapshot_boundaries() {
        let cursors = Arc::new(Mutex::new(MailboxCursorStore::new(600, 8)));
        let input = ListMailboxesInput {
            account_id: "default".to_owned(),
            cursor: None,
            limit: 2,
            include_counts: false,
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
                include_counts: false,
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
                include_counts: false,
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
            include_counts: false,
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

    /// Mailbox with a chosen selectable flag.
    fn selectable_mailbox(name: &str, selectable: bool) -> MailboxInfo {
        MailboxInfo {
            selectable,
            ..mailbox(name)
        }
    }

    /// Mailbox list input in count mode.
    fn counted_input(cursor: Option<&str>, limit: usize) -> ListMailboxesInput {
        ListMailboxesInput {
            account_id: "default".to_owned(),
            cursor: cursor.map(str::to_owned),
            limit,
            include_counts: true,
        }
    }

    #[tokio::test]
    async fn count_mode_continuation_action_keeps_count_mode_and_limit() {
        let cursors = Arc::new(Mutex::new(MailboxCursorStore::new(600, 8)));
        let first = list_mailboxes_from_snapshot(
            &cursors,
            &counted_input(None, 2),
            vec![mailbox("INBOX"), mailbox("Archive"), mailbox("Sent")].into(),
            Some(3),
            false,
        )
        .await
        .expect("first page");
        let action = first.next_action;
        assert_eq!(action.tool, "imap_list_mailboxes");
        assert_eq!(action.arguments["include_counts"], true);
        assert_eq!(action.arguments["limit"], 2);
        assert_eq!(first.mailboxes.len(), 2, "only the page is returned");

        let plain = list_mailboxes_from_snapshot(
            &cursors,
            &ListMailboxesInput {
                include_counts: false,
                ..counted_input(None, 2)
            },
            vec![mailbox("INBOX"), mailbox("Archive"), mailbox("Sent")].into(),
            Some(3),
            false,
        )
        .await
        .expect("plain page");
        assert!(plain.next_action.arguments.get("include_counts").is_none());
    }

    #[tokio::test]
    async fn counts_cover_only_selectable_mailboxes_of_the_current_page() {
        let cursors = Arc::new(Mutex::new(MailboxCursorStore::new(600, 8)));
        let mut page = list_mailboxes_from_snapshot(
            &cursors,
            &counted_input(None, 3),
            vec![
                selectable_mailbox("INBOX", true),
                selectable_mailbox("Container", false),
                selectable_mailbox("Archive", true),
                selectable_mailbox("Beyond the page", true),
            ]
            .into(),
            Some(4),
            false,
        )
        .await
        .expect("page");
        assert_eq!(page.mailboxes.len(), 3);
        let (mut session, peer) = scripted_session(vec![
            reply(&["* STATUS INBOX (MESSAGES 0 UNSEEN 0)", "{tag} OK done"]),
            reply(&["* STATUS Archive (MESSAGES 12 UNSEEN 3)", "{tag} OK done"]),
        ])
        .await;

        let failure =
            enrich_mailbox_counts(&test_config(), &mut session, &mut page.mailboxes).await;

        assert!(failure.is_none());
        drop(session);
        assert_eq!(
            peer.await.expect("peer task"),
            vec![
                "A0002 STATUS \"INBOX\" (MESSAGES UNSEEN)".to_owned(),
                "A0003 STATUS \"Archive\" (MESSAGES UNSEEN)".to_owned(),
            ],
            "no STATUS for the container or for mailboxes beyond the page"
        );
        let counts = |index: usize| {
            (
                page.mailboxes[index].total_messages,
                page.mailboxes[index].unread_messages,
            )
        };
        assert_eq!(counts(0), (Some(0), Some(0)), "observed zeros are known");
        assert_eq!(counts(1), (None, None));
        assert_eq!(counts(2), (Some(12), Some(3)));
    }

    #[tokio::test]
    async fn incomplete_status_leaves_counts_unknown_and_stops_the_session() {
        let mut mailboxes = vec![
            selectable_mailbox("INBOX", true),
            selectable_mailbox("Archive", true),
            selectable_mailbox("Sent", true),
        ];
        let (mut session, peer) = scripted_session(vec![
            reply(&["* STATUS INBOX (MESSAGES 4 UNSEEN 1)", "{tag} OK done"]),
            reply(&["* STATUS Archive (MESSAGES 9)", "{tag} OK done"]),
        ])
        .await;

        let issue = enrich_mailbox_counts(&test_config(), &mut session, &mut mailboxes)
            .await
            .expect("a missing UNSEEN value is an issue");

        drop(session);
        assert_eq!(peer.await.expect("peer task").len(), 2, "Sent is not asked");
        assert_eq!(issue.stage, "mailbox_status");
        assert!(issue.message.contains("Archive"));
        assert_eq!(mailboxes[0].total_messages, Some(4));
        assert_eq!(mailboxes[1].total_messages, None);
        assert_eq!(mailboxes[1].unread_messages, None);
        assert_eq!(mailboxes[2].total_messages, None);
    }

    #[tokio::test]
    async fn continuation_without_counts_is_network_free_and_with_counts_is_partial_on_failure() {
        let server = offline_server();
        let cursor = server
            .mailbox_cursors
            .lock()
            .await
            .create(MailboxCursorEntry {
                account_id: "default".to_owned(),
                mailboxes: vec![mailbox("INBOX"), mailbox("Archive"), mailbox("Sent")].into(),
                offset: 0,
                total: Some(3),
                truncated: false,
                expires_at: Instant::now(),
            });

        let plain = server
            .list_mailboxes_impl(ListMailboxesInput {
                include_counts: false,
                ..counted_input(Some(&cursor), 1)
            })
            .await
            .expect("a plain continuation needs no connection");
        assert_eq!(plain.status, "ok");
        assert!(plain.issues.is_empty());
        assert_eq!(plain.next_cursor.as_deref(), Some(cursor.as_str()));

        let counted = server
            .list_mailboxes_impl(counted_input(Some(&cursor), 1))
            .await
            .expect("a count failure keeps the page");
        assert_eq!(counted.status, "partial", "{:?}", counted.issues);
        assert_eq!(counted.issues[0].stage, "connect_authenticated");
        assert_eq!(counted.returned, 1);
        assert_eq!(counted.mailboxes[0].name, "Archive");
        assert_eq!(counted.mailboxes[0].total_messages, None);
        assert_eq!(counted.next_cursor.as_deref(), Some(cursor.as_str()));
        assert_eq!(counted.next_action.arguments["include_counts"], true);
    }

    #[tokio::test]
    async fn count_limit_is_validated_before_any_cursor_or_network_work() {
        let server = offline_server();
        let error = server
            .list_mailboxes_impl(counted_input(None, 51))
            .await
            .expect_err("51 mailboxes with counts is invalid");
        assert!(matches!(error, AppError::InvalidInput(_)));
    }

    /// Message identity used by attachment mapping tests.
    fn attachment_message_id() -> MessageId {
        seed_message_id()
    }

    /// Fully read attachment outcome with the given payload.
    fn read_outcome(
        size_bytes: usize,
        payload: mime::AttachmentPayload,
    ) -> mime::AttachmentOutcome {
        mime::AttachmentOutcome::Content(mime::AttachmentRead {
            filename: Some("a.bin".to_owned()),
            content_type: "application/octet-stream".to_owned(),
            size_bytes,
            payload,
        })
    }

    #[test]
    fn text_outcome_maps_to_text_content_without_next_action() {
        let outcome = read_outcome(
            3,
            mime::AttachmentPayload::Text {
                text: "a,b".to_owned(),
                truncated: true,
            },
        );
        let data = attachment_outcome_data(
            outcome,
            &mime::AttachmentRequest::Text { max_chars: 3 },
            &attachment_message_id(),
            "1.2",
        );
        assert_eq!(data.status, "ok");
        assert!(data.issues.is_empty());
        assert!(data.next_action.is_none());
        assert_eq!(data.account_id, "default");
        assert_eq!(data.message_id, "imap:default:INBOX:7:42");
        assert_eq!(data.part_id, "1.2");
        let attachment = data.attachment.expect("attachment");
        assert_eq!(attachment.size_bytes, 3);
        assert_eq!(attachment.filename.as_deref(), Some("a.bin"));
        assert_eq!(
            attachment.content,
            AttachmentContent::Text {
                text: "a,b".to_owned(),
                truncated: true
            }
        );
    }

    #[test]
    fn byte_page_with_more_data_points_to_the_next_offset() {
        let outcome = read_outcome(
            10,
            mime::AttachmentPayload::Bytes {
                bytes: vec![0, 1, 2, 3],
                offset_bytes: 2,
                has_more: true,
            },
        );
        let data = attachment_outcome_data(
            outcome,
            &mime::AttachmentRequest::Bytes {
                offset_bytes: 2,
                max_bytes: 4,
            },
            &attachment_message_id(),
            "1.2",
        );
        let attachment = data.attachment.expect("attachment");
        assert_eq!(
            attachment.content,
            AttachmentContent::Base64 {
                content_base64: "AAECAw==".to_owned(),
                offset_bytes: 2,
                returned_bytes: 4,
                has_more: true,
            }
        );
        let action = data.next_action.expect("next page");
        assert_eq!(action.tool, "imap_get_attachment");
        assert_eq!(
            action.arguments,
            serde_json::json!({
                "message_id": "imap:default:INBOX:7:42",
                "part_id": "1.2",
                "mode": "bytes",
                "offset_bytes": 6,
                "max_bytes": 4,
            })
        );
    }

    #[test]
    fn offset_at_the_end_returns_an_empty_final_page() {
        let outcome = read_outcome(
            4,
            mime::AttachmentPayload::Bytes {
                bytes: Vec::new(),
                offset_bytes: 4,
                has_more: false,
            },
        );
        let data = attachment_outcome_data(
            outcome,
            &mime::AttachmentRequest::Bytes {
                offset_bytes: 4,
                max_bytes: 4,
            },
            &attachment_message_id(),
            "1.2",
        );
        assert_eq!(data.status, "ok");
        assert!(data.next_action.is_none());
        assert_eq!(
            data.attachment.expect("attachment").content,
            AttachmentContent::Base64 {
                content_base64: String::new(),
                offset_bytes: 4,
                returned_bytes: 0,
                has_more: false,
            }
        );
    }

    #[test]
    fn processing_limit_fails_without_payload_or_next_action() {
        for limit in [
            mime::ProcessingLimit::DecodeBudgetBytes,
            mime::ProcessingLimit::MaxDepth,
            mime::ProcessingLimit::MaxParts,
            mime::ProcessingLimit::AttachmentExtractBudgetBytes,
        ] {
            let data = attachment_outcome_data(
                mime::AttachmentOutcome::LimitExceeded(limit),
                &mime::AttachmentRequest::Text { max_chars: 10 },
                &attachment_message_id(),
                "1.2",
            );
            assert_eq!(data.status, "failed");
            assert!(data.attachment.is_none());
            assert!(data.next_action.is_none());
            assert_eq!(data.issues.len(), 1);
            assert_eq!(data.issues[0].code, "limit_exceeded");
            assert!(!data.issues[0].retryable);
        }
    }

    #[test]
    fn unavailable_text_points_to_bytes_mode_instead_of_switching() {
        let data = attachment_outcome_data(
            mime::AttachmentOutcome::TextUnavailable {
                filename: None,
                content_type: "image/png".to_owned(),
                size_bytes: 100_000,
                reason: "text mode does not support content type image/png".to_owned(),
            },
            &mime::AttachmentRequest::Text { max_chars: 10 },
            &attachment_message_id(),
            "1.3",
        );
        assert_eq!(data.status, "failed");
        assert!(data.attachment.is_none(), "no payload in another mode");
        assert_eq!(data.issues[0].code, "text_unavailable");
        assert!(data.issues[0].message.contains("mode=bytes"));
        let action = data.next_action.expect("bytes action");
        assert_eq!(action.tool, "imap_get_attachment");
        assert_eq!(action.arguments["mode"], "bytes");
        assert_eq!(action.arguments["part_id"], "1.3");
        assert_eq!(action.arguments["offset_bytes"], 0);
        assert_eq!(action.arguments["max_bytes"], 16_000);
    }

    #[test]
    fn small_unavailable_text_attachment_fits_one_bytes_page() {
        let data = attachment_outcome_data(
            mime::AttachmentOutcome::TextUnavailable {
                filename: None,
                content_type: "image/png".to_owned(),
                size_bytes: 0,
                reason: "unsupported".to_owned(),
            },
            &mime::AttachmentRequest::Text { max_chars: 10 },
            &attachment_message_id(),
            "1.3",
        );
        let action = data.next_action.expect("bytes action");
        assert_eq!(action.arguments["max_bytes"], 1, "max_bytes is at least 1");
    }

    /// Message with a plain-text part and one attachment with the given transfer-encoded body.
    fn message_with_attachment(content_type: &str, encoding: &str, body: &str) -> Vec<u8> {
        format!(
            "From: a@example.com\r\nTo: b@example.com\r\nSubject: s\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=B\r\n\r\n\
             --B\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
             --B\r\nContent-Type: {content_type}\r\n\
             Content-Disposition: attachment; filename=\"a.bin\"\r\n\
             Content-Transfer-Encoding: {encoding}\r\n\r\n{body}\r\n--B--\r\n"
        )
        .into_bytes()
    }

    /// Resource ceilings that never trigger in the paging tests.
    fn generous_limits() -> mime::ProcessingLimits {
        mime::ProcessingLimits {
            decode_budget_bytes: 10_000_000,
            max_depth: 32,
            max_parts: 250,
            attachment_extract_budget_bytes: 10_000_000,
            attachment_text_max_chars: 1,
        }
    }

    #[test]
    fn byte_pages_follow_next_actions_and_concatenate_to_the_original_bytes() {
        use base64::Engine;

        let message_id = attachment_message_id();
        for length in [1usize, 2, 299, 300, 301, 1_000] {
            let original = (0..length)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>();
            let encoded = base64::engine::general_purpose::STANDARD.encode(&original);
            let wrapped = encoded
                .as_bytes()
                .chunks(76)
                .map(|line| String::from_utf8_lossy(line).into_owned())
                .collect::<Vec<_>>()
                .join("\r\n");
            let raw = message_with_attachment("application/octet-stream", "base64", &wrapped);
            for page in [1usize, 7, 300] {
                let mut request = mime::AttachmentRequest::Bytes {
                    offset_bytes: 0,
                    max_bytes: page,
                };
                let mut joined = Vec::new();
                let mut pages = 0usize;
                loop {
                    let outcome = mime::read_attachment(&raw, "1.2", &request, &generous_limits())
                        .expect("read");
                    let data = attachment_outcome_data(outcome, &request, &message_id, "1.2");
                    let attachment = data.attachment.expect("every page has content");
                    assert_eq!(attachment.size_bytes, length);
                    let AttachmentContent::Base64 {
                        content_base64,
                        offset_bytes,
                        returned_bytes,
                        has_more,
                    } = attachment.content
                    else {
                        panic!("bytes mode returns base64");
                    };
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(content_base64)
                        .expect("valid base64");
                    assert_eq!(bytes.len(), returned_bytes);
                    assert_eq!(offset_bytes, joined.len());
                    joined.extend(bytes);
                    pages += 1;
                    match data.next_action {
                        Some(action) => {
                            assert!(has_more);
                            assert_eq!(action.arguments["mode"], "bytes");
                            request = mime::AttachmentRequest::Bytes {
                                offset_bytes: action.arguments["offset_bytes"]
                                    .as_u64()
                                    .expect("offset")
                                    as usize,
                                max_bytes: action.arguments["max_bytes"].as_u64().expect("max")
                                    as usize,
                            };
                        }
                        None => {
                            assert!(!has_more);
                            break;
                        }
                    }
                }
                assert_eq!(joined, original, "length {length}, page {page}");
                assert_eq!(pages, length.div_ceil(page), "length {length}, page {page}");
            }
        }
    }

    #[test]
    fn csv_attachment_reads_as_text_through_the_response_mapping() {
        let raw = message_with_attachment("text/csv", "7bit", "a,b\r\n1,2");
        let request = mime::AttachmentRequest::Text { max_chars: 3 };
        let outcome =
            mime::read_attachment(&raw, "1.2", &request, &generous_limits()).expect("read");
        let data = attachment_outcome_data(outcome, &request, &attachment_message_id(), "1.2");
        let attachment = data.attachment.expect("attachment");
        assert_eq!(attachment.content_type, "text/csv");
        assert_eq!(
            attachment.content,
            AttachmentContent::Text {
                text: "a,b".to_owned(),
                truncated: true
            }
        );
    }

    #[test]
    fn prefix_failures_describe_the_stage_and_session_state() {
        let zero = PrefixFailure::ZeroBudget;
        assert!(zero.session_reusable());
        let issue = zero.into_issue(42, "imap:default:INBOX:7:42");
        assert_eq!(issue.code, "limit_exceeded");
        assert_eq!(issue.stage, "fetch_budget");
        assert_eq!(issue.uid, Some(42));

        let failed = PrefixFailure::Fetch {
            stage: "fetch_message_size",
            error: AppError::Timeout("FETCH timed out".to_owned()),
        };
        assert!(!failed.session_reusable());
        let issue = failed.into_issue(42, "imap:default:INBOX:7:42");
        assert_eq!(issue.code, "timeout");
        assert_eq!(issue.stage, "fetch_message_size");
        assert_eq!(issue.message_id.as_deref(), Some("imap:default:INBOX:7:42"));
    }

    #[tokio::test]
    async fn attachment_arguments_are_validated_before_any_network_work() {
        use crate::models::{AttachmentContentMode, GetAttachmentInput};

        let server = offline_server();
        let input = |mode, max_chars, offset_bytes, max_bytes| GetAttachmentInput {
            message_id: "imap:default:INBOX:7:42".to_owned(),
            part_id: "1.2".to_owned(),
            mode,
            max_chars,
            offset_bytes,
            max_bytes,
        };
        for bad in [
            input(AttachmentContentMode::Text, None, Some(0), None),
            input(AttachmentContentMode::Text, None, None, Some(10)),
            input(AttachmentContentMode::Bytes, Some(10), None, None),
            input(AttachmentContentMode::Bytes, None, None, Some(64_001)),
            input(AttachmentContentMode::Text, Some(0), None, None),
        ] {
            let error = server
                .get_attachment_impl(bad)
                .await
                .expect_err("invalid arguments are a hard error");
            assert!(matches!(error, AppError::InvalidInput(_)));
        }
        let unreachable = server
            .get_attachment_impl(input(AttachmentContentMode::Text, None, None, None))
            .await
            .expect("valid arguments reach the connection stage");
        assert_eq!(unreachable.status, "failed");
        assert_eq!(unreachable.issues[0].stage, "connect_authenticated");
        assert!(unreachable.attachment.is_none());
    }
}
