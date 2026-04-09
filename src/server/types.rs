use std::collections::BTreeMap;
use std::time::Instant;

use chrono::Utc;
use rmcp::Json;
use rmcp::model::ErrorData;
use tracing::{error, warn};

use crate::errors::{AppError, AppResult};
use crate::message_id::MessageId;
use crate::models::{MailboxInfo, MessageSummary, Meta, ToolEnvelope};

#[derive(Debug, serde::Serialize)]
pub(super) struct SearchResultData {
    pub(super) status: String,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) next_action: NextAction,
    pub(super) account_id: String,
    pub(super) mailbox: String,
    pub(super) total: usize,
    pub(super) attempted: usize,
    pub(super) returned: usize,
    pub(super) failed: usize,
    pub(super) messages: Vec<MessageSummary>,
    pub(super) next_cursor: Option<String>,
    pub(super) has_more: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct NextAction {
    pub(super) instruction: String,
    pub(super) tool: String,
    pub(super) arguments: serde_json::Value,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
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

    pub(super) fn with_message_id(mut self, message_id: &str) -> Self {
        self.message_id = Some(message_id.to_owned());
        self
    }
}

#[derive(Debug)]
pub(super) struct SummaryBuildResult {
    pub(super) messages: Vec<MessageSummary>,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) attempted: usize,
    pub(super) failed: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct MessageMutationResult {
    pub(super) message_id: String,
    pub(super) status: String,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) source_mailbox: String,
    pub(super) destination_mailbox: Option<String>,
    pub(super) flags: Option<Vec<String>>,
    pub(super) new_message_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct MailboxManagementResult {
    pub(super) status: String,
    pub(super) issues: Vec<ToolIssue>,
    pub(super) account_id: String,
    pub(super) action: String,
    pub(super) mailbox: String,
    pub(super) destination_mailbox: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) enum MessageActionInput {
    Move { destination_mailbox: String },
    Copy { destination_mailbox: String },
    Delete,
}

#[derive(Debug, Clone)]
pub(super) enum FlagOperation {
    Add,
    Remove,
    Replace,
}

#[derive(Debug, Clone)]
pub(super) struct FlagUpdateRequest {
    pub(super) operation: FlagOperation,
    pub(super) flags: Vec<String>,
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

#[derive(Debug, Clone, serde::Serialize)]
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
    pub(super) result: Option<serde_json::Value>,
    pub(super) spec: StoredOperationSpec,
}

#[derive(Debug, Clone)]
pub(super) enum StoredOperationSpec {
    ApplyMessages(ApplyMessagesOperation),
    UpdateFlags(UpdateFlagsOperation),
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

#[derive(Debug, Clone)]
pub(super) struct UpdateFlagsOperation {
    pub(super) account_id: String,
    pub(super) request: FlagUpdateRequest,
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
    UpdateFlagsGroup {
        account_id: String,
        request: FlagUpdateRequest,
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
            | Self::UpdateFlagsGroup { account_id, .. }
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

pub(super) fn next_action_search_mailbox(account_id: &str, mailbox: &str) -> NextAction {
    next_action(
        "Search for messages in the selected mailbox.",
        "imap_search_messages",
        serde_json::json!({
            "account_id": account_id,
            "mailbox": mailbox,
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

pub(super) fn preferred_mailbox_name(mailboxes: &[MailboxInfo]) -> Option<String> {
    mailboxes
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case("INBOX"))
        .map(|m| m.name.clone())
        .or_else(|| mailboxes.first().map(|m| m.name.clone()))
}

pub(super) fn next_action_for_search_result(
    status: &str,
    account_id: &str,
    mailbox: &str,
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
                "mailbox": mailbox,
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
            "mailbox": mailbox,
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

pub(super) fn serialization_error(error: serde_json::Error) -> AppError {
    AppError::Internal(format!("serialization failure: {error}"))
}

pub(super) fn operation_summary(noun: &str, data: &serde_json::Value) -> String {
    let status = data["status"].as_str().unwrap_or("running");
    let kind = data["operation"]["kind"].as_str().unwrap_or(noun);
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

pub(super) fn operation_kind_label(spec: &StoredOperationSpec) -> &'static str {
    match spec {
        StoredOperationSpec::ApplyMessages(_) => "imap_apply_to_messages",
        StoredOperationSpec::UpdateFlags(_) => "imap_update_message_flags",
        StoredOperationSpec::ManageMailbox(_) => "imap_manage_mailbox",
    }
}

pub(super) fn operation_total_units(spec: &StoredOperationSpec) -> usize {
    match spec {
        StoredOperationSpec::ApplyMessages(spec) => spec.groups.len(),
        StoredOperationSpec::UpdateFlags(spec) => spec.groups.len(),
        StoredOperationSpec::ManageMailbox(_) => 1,
    }
}

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
        StoredOperationSpec::UpdateFlags(spec) => {
            let group = spec.groups.get(spec.next_group_index)?.clone();
            operation.progress.current_mailbox = Some(group.mailbox.clone());
            operation.progress.phase = format!(
                "processing_{}",
                flag_operation_name(&spec.request.operation)
            );
            Some(OperationStep::UpdateFlagsGroup {
                account_id: spec.account_id.clone(),
                request: spec.request.clone(),
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

pub(super) fn flag_operation_name(operation: &FlagOperation) -> &'static str {
    match operation {
        FlagOperation::Add => "add",
        FlagOperation::Remove => "remove",
        FlagOperation::Replace => "replace",
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
