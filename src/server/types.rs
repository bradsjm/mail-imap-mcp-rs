use std::collections::BTreeMap;
use std::time::Instant;

use chrono::Utc;
use rmcp::Json;
use rmcp::model::ErrorData;
use schemars::JsonSchema;
use tracing::{error, warn};

use crate::errors::{AppError, AppResult};
use crate::message_id::MessageId;
use crate::models::{
    AccountInfo, AttachmentResult, MailboxInfo, MessageDetail, MessageSummary, Meta, SetOperation,
    ThreadMethod, ToolEnvelope,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub(super) struct ListAccountsData {
    pub(super) accounts: Vec<AccountInfo>,
    pub(super) next_action: NextAction,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub(super) struct ListMailboxesData {
    pub(super) status: String,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) next_action: NextAction,
    pub(super) account_id: String,
    pub(super) mailboxes: Vec<MailboxInfo>,
    pub(super) returned: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) total: Option<usize>,
    pub(super) has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_cursor: Option<String>,
    pub(super) truncated: bool,
}

/// Output of `imap_search_messages`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub(super) struct SearchResultData {
    pub(super) status: String,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) next_action: NextAction,
    pub(super) account_id: String,
    /// Searched mailboxes in request order, with canonical names.
    pub(super) mailboxes: Vec<String>,
    /// Combined match count across all mailboxes; absent when a mailbox search failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) total: Option<usize>,
    /// Conversation discovery method when the search used `thread_of_message_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) thread_method: Option<ThreadMethod>,
    /// True when conversation discovery used headers and may miss related messages.
    pub(super) thread_possibly_incomplete: bool,
    pub(super) attempted: usize,
    pub(super) returned: usize,
    pub(super) failed: usize,
    pub(super) messages: Vec<MessageSummary>,
    pub(super) next_cursor: Option<String>,
    pub(super) has_more: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub(super) struct GetMessageData {
    pub(super) status: String,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) account_id: String,
    pub(super) message: Option<MessageDetail>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub(super) struct GetMessageRawData {
    pub(super) status: String,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) account_id: String,
    pub(super) message_id: String,
    pub(super) message_uri: String,
    pub(super) message_raw_uri: String,
    pub(super) total_size_bytes: usize,
    pub(super) returned_bytes: usize,
    pub(super) offset_bytes: usize,
    pub(super) truncated: bool,
    pub(super) raw_source_base64: Option<String>,
    pub(super) raw_source_encoding: Option<String>,
}

/// Output of `imap_get_attachment`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub(super) struct GetAttachmentData {
    /// Outcome label: `ok`, `partial`, or `failed`.
    pub(super) status: String,
    /// Problems found while reading the attachment.
    pub(super) issues: Vec<ToolIssue>,
    /// Account that owns the message.
    pub(super) account_id: String,
    /// Stable identifier of the message that was read.
    pub(super) message_id: String,
    /// Part identifier of the requested attachment.
    pub(super) part_id: String,
    /// Attachment content; absent unless the attachment was read completely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) attachment: Option<AttachmentResult>,
    /// Suggested follow-up call, such as the next byte page or bytes mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_action: Option<NextAction>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub(super) struct NextAction {
    pub(super) instruction: String,
    pub(super) tool: String,
    pub(super) arguments: serde_json::Value,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, JsonSchema)]
pub(super) struct ToolIssue {
    pub(super) code: String,
    pub(super) stage: String,
    pub(super) message: String,
    pub(super) retryable: bool,
    pub(super) uid: Option<u32>,
    pub(super) message_id: Option<String>,
}

impl ToolIssue {
    pub(super) fn from_error(stage: &str, error: &AppError) -> Self {
        let (code, retryable) = match error {
            AppError::InvalidInput(_) => ("invalid_input", false),
            AppError::NotFound(_) => ("not_found", false),
            AppError::AuthFailed(_) => ("auth_failed", false),
            AppError::Timeout(_) => ("timeout", true),
            AppError::Conflict(_) => ("conflict", false),
            AppError::Internal(_) => ("internal", true),
        };
        Self {
            code: code.to_owned(),
            stage: stage.to_owned(),
            message: error.to_string(),
            retryable,
            uid: None,
            message_id: None,
        }
    }

    pub(super) fn with_uid(mut self, uid: u32) -> Self {
        self.uid = Some(uid);
        self
    }

    /// Marks the issue as not retryable, as required when a sent write has an unknown outcome.
    pub(super) fn non_retryable(mut self) -> Self {
        self.retryable = false;
        self
    }

    /// Appends a short explanation to the issue message.
    pub(super) fn with_detail(mut self, detail: &str) -> Self {
        self.message = format!("{}; {detail}", self.message);
        self
    }

    pub(super) fn with_message_id(mut self, message_id: &str) -> Self {
        self.message_id = Some(message_id.to_owned());
        self
    }
}

/// Summaries, issues, and counts built for one or more claimed mailbox slices.
#[derive(Debug, Default)]
pub(super) struct SummaryBuildResult {
    pub(super) messages: Vec<MessageSummary>,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) attempted: usize,
    pub(super) failed: usize,
    /// Whether an IMAP command failed, so the session must not be used again.
    pub(super) command_failed: bool,
}

impl SummaryBuildResult {
    /// Appends the outcome of another slice and keeps any command failure flag.
    pub(super) fn merge(&mut self, other: Self) {
        self.messages.extend(other.messages);
        self.issues.extend(other.issues);
        self.attempted += other.attempted;
        self.failed += other.failed;
        self.command_failed |= other.command_failed;
    }

    /// Counts a whole slice as attempted and failed, with one issue explaining why.
    pub(super) fn fail_slice(&mut self, uid_count: usize, issue: ToolIssue) {
        self.issues.push(issue);
        self.attempted += uid_count;
        self.failed += uid_count;
    }
}

/// Conversation criterion that a new search adds to its ordinary filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SearchThreadQuery {
    /// Gmail conversation identifier searched with `X-GM-THRID`.
    Gmail(u64),
    /// Message-ID values searched in Message-ID, References, and In-Reply-To headers.
    Headers(Vec<String>),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub(super) struct MessageMutationResult {
    pub(super) message_id: String,
    pub(super) status: String,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) source_mailbox: String,
    pub(super) destination_mailbox: Option<String>,
    pub(super) flags: Option<Vec<String>>,
    /// Gmail labels returned by the server after a label update.
    pub(super) labels: Option<Vec<String>>,
    pub(super) new_message_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub(super) struct BulkMessageOperationData {
    pub(super) status: String,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) account_id: String,
    pub(super) action: Option<String>,
    pub(super) operation: Option<String>,
    pub(super) matched: usize,
    pub(super) attempted: usize,
    pub(super) succeeded: usize,
    /// Number of messages whose operation was applied but not fully verified or identified.
    pub(super) partial: usize,
    pub(super) failed: usize,
    pub(super) results: Vec<MessageMutationResult>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub(super) struct MailboxManagementResult {
    pub(super) status: String,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) account_id: String,
    pub(super) action: String,
    pub(super) mailbox: String,
    pub(super) destination_mailbox: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub(super) struct OperationMetadata {
    pub(super) operation_id: String,
    pub(super) kind: String,
    pub(super) state: String,
    pub(super) done: bool,
    pub(super) cancel_supported: bool,
    pub(super) created_at: String,
    pub(super) started_at: Option<String>,
    pub(super) finished_at: Option<String>,
    pub(super) progress: OperationProgress,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(untagged)]
pub(super) enum OperationResultData {
    BulkMessage(BulkMessageOperationData),
    MailboxManagement(MailboxManagementResult),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub(super) struct OperationStatusData {
    pub(super) status: String,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) operation: OperationMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) result: Option<OperationResultData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_action: Option<NextAction>,
}

#[derive(Debug, Clone)]
pub(super) enum MessageActionInput {
    Move { destination_mailbox: String },
    Copy { destination_mailbox: String },
    Delete,
}

/// Property mutation applied to explicit messages; values are decoded text.
#[derive(Debug, Clone)]
pub(super) enum PropertyUpdate {
    /// Change IMAP flags with the given mutation mode.
    Flags {
        /// Mutation mode.
        operation: SetOperation,
        /// Validated flag atoms.
        flags: Vec<String>,
    },
    /// Change Gmail labels with the given mutation mode.
    GmailLabels {
        /// Mutation mode.
        operation: SetOperation,
        /// Validated, deduplicated label text.
        labels: Vec<String>,
    },
}

impl PropertyUpdate {
    /// Returns the mutation mode shared by both property kinds.
    pub(super) fn operation(&self) -> SetOperation {
        match self {
            Self::Flags { operation, .. } | Self::GmailLabels { operation, .. } => *operation,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct MessageMutationGroup {
    pub(super) mailbox: String,
    pub(super) uidvalidity: u32,
    pub(super) entries: Vec<MessageId>,
}

#[derive(Debug, Clone)]
pub(super) enum MailboxAction {
    Create {
        mailbox: String,
    },
    Rename {
        mailbox: String,
        destination_mailbox: String,
    },
    Delete {
        mailbox: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OperationState {
    Pending,
    Running,
    CancelRequested,
    Ok,
    Partial,
    Failed,
    Canceled,
}

impl OperationState {
    pub(super) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Ok | Self::Partial | Self::Failed | Self::Canceled
        )
    }

    pub(super) fn status_label(self) -> &'static str {
        match self {
            Self::Pending => "accepted",
            Self::Running => "running",
            Self::CancelRequested => "running",
            Self::Ok => "ok",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    pub(super) fn state_label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::CancelRequested => "cancel_requested",
            Self::Ok => "ok",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub(super) struct OperationProgress {
    pub(super) total_units: usize,
    pub(super) completed_units: usize,
    pub(super) failed_units: usize,
    pub(super) remaining_units: usize,
    pub(super) current_mailbox: Option<String>,
    pub(super) phase: String,
}

impl OperationProgress {
    pub(super) fn new(total_units: usize, phase: &str) -> Self {
        Self {
            total_units,
            completed_units: 0,
            failed_units: 0,
            remaining_units: total_units,
            current_mailbox: None,
            phase: phase.to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct StoredOperation {
    pub(super) operation_id: String,
    pub(super) kind: String,
    pub(super) state: OperationState,
    pub(super) created_at: String,
    pub(super) started_at: Option<String>,
    pub(super) finished_at: Option<String>,
    pub(super) cancel_supported: bool,
    pub(super) worker_started: bool,
    pub(super) progress: OperationProgress,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) result: Option<OperationResultData>,
    pub(super) spec: StoredOperationSpec,
}

#[derive(Debug, Clone)]
pub(super) enum StoredOperationSpec {
    ApplyMessages(ApplyMessagesOperation),
    /// Flag or Gmail label update on explicit messages.
    UpdateProperties(UpdatePropertiesOperation),
    ManageMailbox(ManageMailboxOperation),
}

#[derive(Debug, Clone)]
pub(super) struct ApplyMessagesOperation {
    pub(super) account_id: String,
    pub(super) action: MessageActionInput,
    pub(super) message_ids: Vec<MessageId>,
    pub(super) groups: Vec<MessageMutationGroup>,
    pub(super) next_group_index: usize,
    pub(super) result_by_id: BTreeMap<String, MessageMutationResult>,
}

/// Stored state of a flag or Gmail label update operation.
#[derive(Debug, Clone)]
pub(super) struct UpdatePropertiesOperation {
    pub(super) account_id: String,
    /// Property change applied to every group.
    pub(super) update: PropertyUpdate,
    pub(super) message_ids: Vec<MessageId>,
    pub(super) groups: Vec<MessageMutationGroup>,
    pub(super) next_group_index: usize,
    pub(super) result_by_id: BTreeMap<String, MessageMutationResult>,
}

#[derive(Debug, Clone)]
pub(super) struct ManageMailboxOperation {
    pub(super) account_id: String,
    pub(super) action: MailboxAction,
    pub(super) completed: bool,
    pub(super) result: Option<MailboxManagementResult>,
}

#[derive(Debug, Clone)]
pub(super) enum OperationStep {
    ApplyMessagesGroup {
        account_id: String,
        action: MessageActionInput,
        group: MessageMutationGroup,
    },
    /// Applies one property update to one mailbox group.
    UpdatePropertiesGroup {
        account_id: String,
        update: PropertyUpdate,
        group: MessageMutationGroup,
    },
    ManageMailbox {
        account_id: String,
        action: MailboxAction,
    },
}

impl OperationStep {
    pub(super) fn account_id(&self) -> &str {
        match self {
            Self::ApplyMessagesGroup { account_id, .. }
            | Self::UpdatePropertiesGroup { account_id, .. }
            | Self::ManageMailbox { account_id, .. } => account_id,
        }
    }
}

#[derive(Debug)]
pub(super) enum OperationStepOutcome {
    MessageResults(Vec<MessageMutationResult>),
    MailboxResult(MailboxManagementResult),
}

pub(super) fn duration_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

pub(super) fn status_from_counts(no_issues: bool, has_data: bool) -> &'static str {
    if no_issues {
        "ok"
    } else if has_data {
        "partial"
    } else {
        "failed"
    }
}

pub(super) fn status_from_issue_and_counts(issues: &[ToolIssue], has_data: bool) -> &'static str {
    status_from_counts(issues.is_empty(), has_data)
}

fn app_error_code(error: &AppError) -> &'static str {
    match error {
        AppError::InvalidInput(_) => "invalid_input",
        AppError::NotFound(_) => "not_found",
        AppError::AuthFailed(_) => "auth_failed",
        AppError::Timeout(_) => "timeout",
        AppError::Conflict(_) => "conflict",
        AppError::Internal(_) => "internal",
    }
}

pub(super) fn log_runtime_issues(
    tool: &str,
    status: &str,
    account_id: &str,
    mailbox: Option<&str>,
    issues: &[ToolIssue],
) {
    for issue in issues {
        let is_error = status == "failed" || matches!(issue.code.as_str(), "internal" | "timeout");
        if is_error {
            error!(
                tool,
                stage = %issue.stage,
                code = %issue.code,
                retryable = issue.retryable,
                account_id,
                mailbox = ?mailbox,
                uid = ?issue.uid,
                message_id = ?issue.message_id,
                message = %issue.message,
                "runtime imap issue"
            );
        } else {
            warn!(
                tool,
                stage = %issue.stage,
                code = %issue.code,
                retryable = issue.retryable,
                account_id,
                mailbox = ?mailbox,
                uid = ?issue.uid,
                message_id = ?issue.message_id,
                message = %issue.message,
                "runtime imap issue"
            );
        }
    }
}

pub(super) fn next_action(
    instruction: &str,
    tool: &str,
    arguments: serde_json::Value,
) -> NextAction {
    NextAction {
        instruction: instruction.to_owned(),
        tool: tool.to_owned(),
        arguments,
    }
}

pub(super) fn next_action_list_accounts() -> NextAction {
    next_action(
        "List configured accounts before retrying mailbox access.",
        "imap_list_accounts",
        serde_json::json!({}),
    )
}

pub(super) fn next_action_list_mailboxes(account_id: &str) -> NextAction {
    next_action(
        "List mailboxes to choose a mailbox for message search.",
        "imap_list_mailboxes",
        serde_json::json!({
            "account_id": account_id,
        }),
    )
}

/// Suggests the next mailbox page; count mode and page size carry over from the request.
pub(super) fn next_action_continue_list_mailboxes(
    account_id: &str,
    cursor: &str,
    limit: usize,
    include_counts: bool,
) -> NextAction {
    // Arguments of the continuation call.
    let mut arguments = serde_json::json!({
        "account_id": account_id,
        "cursor": cursor,
        "limit": limit,
    });
    if include_counts {
        arguments["include_counts"] = serde_json::Value::Bool(true);
    }
    next_action(
        "Continue pagination to discover more mailboxes.",
        "imap_list_mailboxes",
        arguments,
    )
}

/// Suggests reading decoded attachment bytes from `offset_bytes`, at most `max_bytes` per page.
pub(super) fn next_action_get_attachment_bytes(
    instruction: &str,
    message_id: &str,
    part_id: &str,
    offset_bytes: usize,
    max_bytes: usize,
) -> NextAction {
    next_action(
        instruction,
        "imap_get_attachment",
        serde_json::json!({
            "message_id": message_id,
            "part_id": part_id,
            "mode": "bytes",
            "offset_bytes": offset_bytes,
            "max_bytes": max_bytes,
        }),
    )
}

/// Suggests searching one mailbox; the suggestion is the shape of a new search.
pub(super) fn next_action_search_mailbox(account_id: &str, mailbox: &str) -> NextAction {
    next_action(
        "Search for messages in the selected mailbox.",
        "imap_search_messages",
        serde_json::json!({
            "account_id": account_id,
            "mailboxes": [mailbox],
            "limit": 10,
        }),
    )
}

pub(super) fn next_action_get_operation(operation_id: &str) -> NextAction {
    next_action(
        "Poll the operation until it reaches a terminal state.",
        "imap_get_operation",
        serde_json::json!({
            "operation_id": operation_id,
        }),
    )
}

pub(super) fn next_action_get_operation_with_result(operation_id: &str) -> NextAction {
    next_action(
        "Fetch the completed operation result payload.",
        "imap_get_operation",
        serde_json::json!({
            "operation_id": operation_id,
            "include_result": true,
        }),
    )
}

pub(super) fn preferred_mailbox_name(mailboxes: &[MailboxInfo]) -> Option<String> {
    mailboxes
        .iter()
        .find(|mailbox| {
            mailbox.selectable
                && (mailbox.name.eq_ignore_ascii_case("INBOX")
                    || mailbox
                        .role
                        .is_some_and(|role| role == crate::models::MailboxRole::Inbox))
        })
        .or_else(|| mailboxes.iter().find(|mailbox| mailbox.selectable))
        .map(|mailbox| mailbox.name.clone())
}

/// Chooses the next step after a search page; continuations carry only the binding and paging arguments.
pub(super) fn next_action_for_search_result(
    status: &str,
    account_id: &str,
    mailboxes: &[String],
    limit: usize,
    cursor: Option<&str>,
    messages: &[MessageSummary],
) -> NextAction {
    if let Some(cursor) = cursor {
        return next_action(
            "Continue pagination to retrieve more messages.",
            "imap_search_messages",
            serde_json::json!({
                "account_id": account_id,
                "mailboxes": mailboxes,
                "cursor": cursor,
                "limit": limit,
            }),
        );
    }

    if status == "failed" {
        return next_action_list_mailboxes(account_id);
    }

    if let Some(first) = messages.first() {
        return next_action(
            "Open a message to inspect full content and headers.",
            "imap_get_message",
            serde_json::json!({
                "message_id": first.message_id,
            }),
        );
    }

    next_action(
        "Retry search with broader criteria.",
        "imap_search_messages",
        serde_json::json!({
            "account_id": account_id,
            "mailboxes": mailboxes,
            "limit": limit,
        }),
    )
}

pub(super) fn is_hard_precondition_error(error: &AppError) -> bool {
    matches!(error, AppError::InvalidInput(_) | AppError::Conflict(_))
}

pub(super) fn finalize_tool<T>(
    started: Instant,
    tool: &str,
    result: AppResult<(String, T)>,
) -> Result<Json<ToolEnvelope<T>>, ErrorData>
where
    T: schemars::JsonSchema,
{
    match result {
        Ok((summary, data)) => Ok(Json(ToolEnvelope {
            summary,
            data,
            meta: Meta::now(duration_ms(started)),
        })),
        Err(error) => {
            error!(
                tool,
                code = app_error_code(&error),
                message = %error,
                "hard mcp error"
            );
            Err(error.to_error_data())
        }
    }
}

pub(super) fn operation_summary(status: &str, kind: &str) -> String {
    let label = kind.strip_prefix("imap_").unwrap_or(kind);
    match status {
        "accepted" => format!("{label} accepted"),
        "running" => format!("{label} running"),
        "ok" => format!("{label} completed"),
        "partial" => format!("{label} completed with issues"),
        "failed" => format!("{label} failed"),
        "canceled" => format!("{label} canceled"),
        _ => format!("{label} {status}"),
    }
}

/// Returns the tool name that started an operation.
pub(super) fn operation_kind_label(spec: &StoredOperationSpec) -> &'static str {
    match spec {
        StoredOperationSpec::ApplyMessages(_) => "imap_apply_to_messages",
        StoredOperationSpec::UpdateProperties(spec) => match spec.update {
            PropertyUpdate::Flags { .. } => "imap_update_message_flags",
            PropertyUpdate::GmailLabels { .. } => "imap_update_message_labels",
        },
        StoredOperationSpec::ManageMailbox(_) => "imap_manage_mailbox",
    }
}

/// Returns the number of group steps that an operation executes.
pub(super) fn operation_total_units(spec: &StoredOperationSpec) -> usize {
    match spec {
        StoredOperationSpec::ApplyMessages(spec) => spec.groups.len(),
        StoredOperationSpec::UpdateProperties(spec) => spec.groups.len(),
        StoredOperationSpec::ManageMailbox(_) => 1,
    }
}

/// Claims the next unit of work and updates the progress phase text.
pub(super) fn next_operation_step(operation: &mut StoredOperation) -> Option<OperationStep> {
    if operation.state == OperationState::CancelRequested {
        return None;
    }
    match &mut operation.spec {
        StoredOperationSpec::ApplyMessages(spec) => {
            let group = spec.groups.get(spec.next_group_index)?.clone();
            operation.progress.current_mailbox = Some(group.mailbox.clone());
            operation.progress.phase = format!("processing_{}", message_action_name(&spec.action));
            Some(OperationStep::ApplyMessagesGroup {
                account_id: spec.account_id.clone(),
                action: spec.action.clone(),
                group,
            })
        }
        StoredOperationSpec::UpdateProperties(spec) => {
            let group = spec.groups.get(spec.next_group_index)?.clone();
            operation.progress.current_mailbox = Some(group.mailbox.clone());
            operation.progress.phase =
                format!("processing_{}", set_operation_name(spec.update.operation()));
            Some(OperationStep::UpdatePropertiesGroup {
                account_id: spec.account_id.clone(),
                update: spec.update.clone(),
                group,
            })
        }
        StoredOperationSpec::ManageMailbox(spec) => {
            if spec.completed {
                return None;
            }
            let (_, mailbox, _) = mailbox_action_display(&spec.action);
            operation.progress.current_mailbox = Some(mailbox.clone());
            operation.progress.phase = format!("processing_{}", mailbox_action_name(&spec.action));
            Some(OperationStep::ManageMailbox {
                account_id: spec.account_id.clone(),
                action: spec.action.clone(),
            })
        }
    }
}

pub(super) fn message_action_name(action: &MessageActionInput) -> &'static str {
    match action {
        MessageActionInput::Move { .. } => "move",
        MessageActionInput::Copy { .. } => "copy",
        MessageActionInput::Delete => "delete",
    }
}

/// Returns the lowercase wire name of a property mutation mode.
pub(super) fn set_operation_name(operation: SetOperation) -> &'static str {
    match operation {
        SetOperation::Add => "add",
        SetOperation::Remove => "remove",
        SetOperation::Replace => "replace",
    }
}

pub(super) fn destination_mailbox_for_action(action: &MessageActionInput) -> Option<&str> {
    match action {
        MessageActionInput::Move {
            destination_mailbox,
        }
        | MessageActionInput::Copy {
            destination_mailbox,
        } => Some(destination_mailbox.as_str()),
        MessageActionInput::Delete => None,
    }
}

pub(super) fn canceled_tool_issue(uid: Option<u32>, message_id: Option<String>) -> ToolIssue {
    ToolIssue {
        code: "canceled".to_owned(),
        stage: "operation_canceled".to_owned(),
        message: "operation was canceled before this item ran".to_owned(),
        retryable: false,
        uid,
        message_id,
    }
}

pub(super) fn mailbox_action_name(action: &MailboxAction) -> &'static str {
    match action {
        MailboxAction::Create { .. } => "create",
        MailboxAction::Rename { .. } => "rename",
        MailboxAction::Delete { .. } => "delete",
    }
}

pub(super) fn mailbox_action_stage(action: &MailboxAction) -> &'static str {
    match action {
        MailboxAction::Create { .. } => "create_mailbox",
        MailboxAction::Rename { .. } => "rename_mailbox",
        MailboxAction::Delete { .. } => "delete_mailbox",
    }
}

pub(super) fn mailbox_action_display(
    action: &MailboxAction,
) -> (&'static str, String, Option<String>) {
    match action {
        MailboxAction::Create { mailbox } => ("create", mailbox.clone(), None),
        MailboxAction::Rename {
            mailbox,
            destination_mailbox,
        } => ("rename", mailbox.clone(), Some(destination_mailbox.clone())),
        MailboxAction::Delete { mailbox } => ("delete", mailbox.clone(), None),
    }
}

pub(super) fn now_utc_string() -> String {
    Utc::now().to_rfc3339()
}

pub(super) fn build_message_uri(
    account_id: &str,
    mailbox: &str,
    uidvalidity: u32,
    uid: u32,
) -> String {
    format!(
        "imap://{}/mailbox/{}/message/{}/{}",
        account_id,
        urlencoding::encode(mailbox),
        uidvalidity,
        uid
    )
}

pub(super) fn build_message_raw_uri(
    account_id: &str,
    mailbox: &str,
    uidvalidity: u32,
    uid: u32,
) -> String {
    format!(
        "{}/raw",
        build_message_uri(account_id, mailbox, uidvalidity, uid)
    )
}

#[cfg(test)]
mod tests {
    use super::{
        NextAction, SearchResultData, SummaryBuildResult, ToolIssue,
        next_action_continue_list_mailboxes, next_action_for_search_result,
        next_action_get_attachment_bytes, next_action_search_mailbox, preferred_mailbox_name,
    };
    use crate::models::{MailboxInfo, MailboxRole, ThreadMethod};

    fn mailbox(name: &str, selectable: bool, role: Option<MailboxRole>) -> MailboxInfo {
        MailboxInfo {
            name: name.to_owned(),
            delimiter: Some("/".to_owned()),
            attributes: Vec::new(),
            role,
            selectable,
            provider_managed: false,
            total_messages: None,
            unread_messages: None,
        }
    }

    #[test]
    fn mailbox_continuation_keeps_count_mode_only_when_requested() {
        let plain = next_action_continue_list_mailboxes("default", "c1", 50, false);
        assert_eq!(plain.tool, "imap_list_mailboxes");
        assert_eq!(
            plain.arguments,
            serde_json::json!({"account_id": "default", "cursor": "c1", "limit": 50})
        );
        let counted = next_action_continue_list_mailboxes("default", "c1", 20, true);
        assert_eq!(
            counted.arguments,
            serde_json::json!({
                "account_id": "default",
                "cursor": "c1",
                "limit": 20,
                "include_counts": true,
            })
        );
    }

    #[test]
    fn attachment_bytes_action_names_mode_ids_and_window() {
        let action = next_action_get_attachment_bytes(
            "Read the next page.",
            "imap:default:INBOX:7:42",
            "1.2",
            16_000,
            8_000,
        );
        assert_eq!(action.tool, "imap_get_attachment");
        assert_eq!(
            action.arguments,
            serde_json::json!({
                "message_id": "imap:default:INBOX:7:42",
                "part_id": "1.2",
                "mode": "bytes",
                "offset_bytes": 16_000,
                "max_bytes": 8_000,
            })
        );
    }

    #[test]
    fn preferred_mailbox_never_selects_noselect_container() {
        let mailboxes = [
            mailbox("INBOX", false, Some(MailboxRole::Inbox)),
            mailbox("Archive", true, Some(MailboxRole::Archive)),
        ];

        assert_eq!(
            preferred_mailbox_name(&mailboxes).as_deref(),
            Some("Archive")
        );
    }

    #[test]
    fn preferred_mailbox_prefers_selectable_inbox() {
        let mailboxes = [
            mailbox("Archive", true, Some(MailboxRole::Archive)),
            mailbox("Primary", true, Some(MailboxRole::Inbox)),
        ];

        assert_eq!(
            preferred_mailbox_name(&mailboxes).as_deref(),
            Some("Primary")
        );
    }

    fn mailboxes(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn search_continuation_carries_only_binding_and_paging_arguments() {
        let action = next_action_for_search_result(
            "ok",
            "default",
            &mailboxes(&["INBOX", "Sent"]),
            25,
            Some("cursor-1"),
            &[],
        );
        assert_eq!(action.tool, "imap_search_messages");
        assert_eq!(
            action.arguments,
            serde_json::json!({
                "account_id": "default",
                "mailboxes": ["INBOX", "Sent"],
                "cursor": "cursor-1",
                "limit": 25,
            })
        );
    }

    #[test]
    fn empty_search_retry_and_mailbox_suggestions_use_the_mailboxes_array() {
        let retry =
            next_action_for_search_result("ok", "default", &mailboxes(&["INBOX"]), 10, None, &[]);
        assert_eq!(retry.arguments["mailboxes"], serde_json::json!(["INBOX"]));
        assert!(retry.arguments.get("mailbox").is_none());

        let suggestion = next_action_search_mailbox("default", "Archive");
        assert_eq!(
            suggestion.arguments["mailboxes"],
            serde_json::json!(["Archive"])
        );
        assert!(suggestion.arguments.get("mailbox").is_none());

        let failed = next_action_for_search_result(
            "failed",
            "default",
            &mailboxes(&["INBOX"]),
            10,
            None,
            &[],
        );
        assert_eq!(failed.tool, "imap_list_mailboxes");
    }

    fn search_result(total: Option<usize>, method: Option<ThreadMethod>) -> SearchResultData {
        SearchResultData {
            status: "ok".to_owned(),
            issues: Vec::new(),
            next_action: NextAction {
                instruction: String::new(),
                tool: "imap_search_messages".to_owned(),
                arguments: serde_json::json!({}),
            },
            account_id: "default".to_owned(),
            mailboxes: mailboxes(&["INBOX"]),
            total,
            thread_method: method,
            thread_possibly_incomplete: method == Some(ThreadMethod::Headers),
            attempted: 0,
            returned: 0,
            failed: 0,
            messages: Vec::new(),
            next_cursor: None,
            has_more: false,
        }
    }

    #[test]
    fn unknown_search_total_is_absent_and_known_zero_is_serialized() {
        let unknown = serde_json::to_value(search_result(None, None)).expect("serialize");
        assert!(unknown.get("total").is_none());
        assert!(unknown.get("thread_method").is_none());
        assert_eq!(unknown["thread_possibly_incomplete"], false);

        let zero = serde_json::to_value(search_result(Some(0), Some(ThreadMethod::Headers)))
            .expect("serialize");
        assert_eq!(zero["total"], 0);
        assert_eq!(zero["thread_method"], "headers");
        assert_eq!(zero["thread_possibly_incomplete"], true);

        let gmail = serde_json::to_value(search_result(Some(1), Some(ThreadMethod::Gmail)))
            .expect("serialize");
        assert_eq!(gmail["thread_method"], "gmail");
        assert_eq!(gmail["thread_possibly_incomplete"], false);
    }

    #[test]
    fn summary_results_merge_counts_and_keep_the_command_failure_flag() {
        let issue = || ToolIssue {
            code: "internal".to_owned(),
            stage: "fetch".to_owned(),
            message: "boom".to_owned(),
            retryable: true,
            uid: None,
            message_id: None,
        };
        let mut total = SummaryBuildResult::default();
        total.fail_slice(3, issue());
        total.merge(SummaryBuildResult {
            attempted: 2,
            failed: 1,
            command_failed: true,
            ..SummaryBuildResult::default()
        });
        total.merge(SummaryBuildResult {
            attempted: 4,
            ..SummaryBuildResult::default()
        });
        assert_eq!((total.attempted, total.failed), (9, 4));
        assert!(total.command_failed);
        assert_eq!(total.issues.len(), 1);
    }
}
